//! Linear-segment ADSR envelope generator.
//!
//! Shared primitive used by every voice-based instrument in the
//! ecosystem — built-in sine synth, stardust-sfz, future Stardust
//! plugins. Per-sample tick is allocation-free and branch-light.
//!
//! # Lifecycle
//!
//! ```text
//!  Idle ──trigger()──▶ Attack ──▶ Decay ──▶ Sustain ──release()──▶ Released ──▶ Idle
//! ```
//!
//! `trigger()` from any non-Idle state re-attacks WITHOUT zeroing the
//! current level — that's deliberate so re-triggered voices don't click
//! back to silence before ramping up. `reset()` is the hard-zero
//! variant used for panic / all-notes-off.

use std::fmt;

/// ADSR shape. Times are in seconds, sustain is the linear level
/// `0.0..=1.0` held while the note is down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdsrConfig {
    /// Time from 0 → 1.0 at the start of the note. Should be at least
    /// a few milliseconds to avoid click on the first sample.
    pub attack_secs: f32,
    /// Time from 1.0 → `sustain_level` after attack completes.
    pub decay_secs: f32,
    /// Held level while the note is down. 0.0..=1.0.
    pub sustain_level: f32,
    /// Time from `sustain_level` → 0.0 after note-off.
    pub release_secs: f32,
}

impl Default for AdsrConfig {
    /// Sensible defaults for a generic instrument voice — short attack
    /// (avoids clicks but stays responsive), modest decay, high
    /// sustain, ~200ms release. Override per-instrument as needed.
    fn default() -> Self {
        Self {
            attack_secs: 0.005,
            decay_secs: 0.050,
            sustain_level: 0.7,
            release_secs: 0.200,
        }
    }
}

/// Which segment of the ADSR curve the envelope is currently in.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum EnvState {
    /// Producing silence. `tick` returns 0.0 and is cheap.
    Idle,
    /// Ramping up from current level to 1.0.
    Attack,
    /// Ramping down from 1.0 to `sustain_level`.
    Decay,
    /// Held at `sustain_level` until released.
    Sustain,
    /// Ramping down from current level to 0.0.
    Released,
}

impl fmt::Debug for EnvState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EnvState::Idle => "Idle",
            EnvState::Attack => "Attack",
            EnvState::Decay => "Decay",
            EnvState::Sustain => "Sustain",
            EnvState::Released => "Released",
        })
    }
}

/// One-shot ADSR envelope. RT-safe: zero allocs, no branching beyond
/// the segment match.
#[derive(Debug, Clone, Copy)]
pub struct Envelope {
    state: EnvState,
    level: f32,
    sustain_level: f32,
    attack_per_sample: f32,
    decay_per_sample: f32,
    /// Per-sample release decrement, computed at release-time so the
    /// release lasts the configured number of seconds regardless of
    /// what level it started from.
    release_decrement: f32,
    /// Total release time in seconds, kept so we can recompute the
    /// decrement when release starts (rather than baking in a value
    /// that assumes release begins from sustain_level).
    release_secs: f32,
    sample_rate: f32,
}

impl Envelope {
    /// Build an envelope for the given ADSR shape and sample rate.
    pub fn new(config: AdsrConfig, sample_rate: f32) -> Self {
        debug_assert!(sample_rate > 0.0);
        let attack = config.attack_secs.max(0.0001);
        let decay = config.decay_secs.max(0.0001);
        let release = config.release_secs.max(0.0001);
        Self {
            state: EnvState::Idle,
            level: 0.0,
            sustain_level: config.sustain_level.clamp(0.0, 1.0),
            attack_per_sample: 1.0 / (sample_rate * attack),
            decay_per_sample: (1.0 - config.sustain_level.clamp(0.0, 1.0)) / (sample_rate * decay),
            release_decrement: 0.0,
            release_secs: release,
            sample_rate,
        }
    }

    /// Start (or re-start) the attack phase. Doesn't zero the current
    /// level so re-triggered voices avoid an audible drop to silence.
    pub fn trigger(&mut self) {
        self.state = EnvState::Attack;
    }

    /// Begin the release segment from wherever the envelope currently
    /// is. No-op if already idle or released.
    pub fn release(&mut self) {
        if self.state == EnvState::Idle || self.state == EnvState::Released {
            return;
        }
        // Compute decrement so the release takes `release_secs` from
        // the current level (not from sustain_level).
        self.release_decrement = (self.level.max(0.0001)) / (self.sample_rate * self.release_secs);
        self.state = EnvState::Released;
    }

    /// Hard reset to Idle/0.0. Use for panic, all-notes-off, voice steal.
    pub fn reset(&mut self) {
        self.state = EnvState::Idle;
        self.level = 0.0;
    }

    /// Whether the envelope is still producing audio.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.state != EnvState::Idle
    }

    /// Current segment.
    #[inline]
    pub fn state(&self) -> EnvState {
        self.state
    }

    /// Advance the envelope by one sample and return the current level.
    #[inline]
    pub fn tick(&mut self) -> f32 {
        match self.state {
            EnvState::Idle => return 0.0,
            EnvState::Attack => {
                self.level += self.attack_per_sample;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.state = EnvState::Decay;
                }
            }
            EnvState::Decay => {
                self.level -= self.decay_per_sample;
                if self.level <= self.sustain_level {
                    self.level = self.sustain_level;
                    self.state = EnvState::Sustain;
                }
            }
            EnvState::Sustain => {}
            EnvState::Released => {
                self.level -= self.release_decrement;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.state = EnvState::Idle;
                    return 0.0;
                }
            }
        }
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_active_samples(env: &mut Envelope, max: usize) -> usize {
        for i in 0..max {
            let v = env.tick();
            if v == 0.0 && !env.is_active() {
                return i;
            }
        }
        max
    }

    #[test]
    fn idle_envelope_returns_zero() {
        let mut e = Envelope::new(AdsrConfig::default(), 48_000.0);
        assert_eq!(e.tick(), 0.0);
        assert!(!e.is_active());
    }

    #[test]
    fn attack_ramps_up_from_zero() {
        let mut e = Envelope::new(
            AdsrConfig {
                attack_secs: 0.01,
                decay_secs: 0.0001,
                sustain_level: 1.0,
                release_secs: 0.01,
            },
            48_000.0,
        );
        e.trigger();
        let first = e.tick();
        assert!(
            first > 0.0 && first < 0.5,
            "first sample tiny but non-zero: {first}"
        );
        // After 10ms of 48kHz: ~480 samples, should be at sustain (1.0).
        for _ in 0..500 {
            e.tick();
        }
        assert!((e.level - 1.0).abs() < 0.05, "reached top: {}", e.level);
    }

    #[test]
    fn release_brings_envelope_to_idle() {
        let mut e = Envelope::new(
            AdsrConfig {
                attack_secs: 0.001,
                decay_secs: 0.001,
                sustain_level: 0.8,
                release_secs: 0.020,
            },
            48_000.0,
        );
        e.trigger();
        // Push past attack + decay.
        for _ in 0..200 {
            e.tick();
        }
        assert_eq!(e.state(), EnvState::Sustain);
        e.release();
        let active = count_active_samples(&mut e, 48_000);
        // 20ms release at 48kHz ≈ 960 samples. Allow generous slack.
        assert!(
            active > 500 && active < 2_000,
            "release took {active} samples (expected ~960)"
        );
        assert!(!e.is_active());
    }

    #[test]
    fn reset_silences_immediately() {
        let mut e = Envelope::new(AdsrConfig::default(), 48_000.0);
        e.trigger();
        for _ in 0..100 {
            e.tick();
        }
        e.reset();
        assert!(!e.is_active());
        assert_eq!(e.tick(), 0.0);
    }

    #[test]
    fn trigger_during_release_re_attacks_without_drop() {
        let mut e = Envelope::new(
            AdsrConfig {
                attack_secs: 0.01,
                decay_secs: 0.01,
                sustain_level: 0.7,
                release_secs: 0.1,
            },
            48_000.0,
        );
        e.trigger();
        for _ in 0..600 {
            e.tick();
        }
        e.release();
        // Tick a bit into the release.
        for _ in 0..100 {
            e.tick();
        }
        let mid_release = e.level;
        assert!(mid_release > 0.0 && mid_release < 1.0);
        e.trigger();
        // Tick once — should be HIGHER than mid_release (or equal +
        // attack increment), not lower.
        let after = e.tick();
        assert!(
            after >= mid_release,
            "re-trigger dropped to {after} from {mid_release}"
        );
    }
}
