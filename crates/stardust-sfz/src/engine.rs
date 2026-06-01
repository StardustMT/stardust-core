//! Polyphonic sample-playback engine.
//!
//! Audio-thread design:
//!
//! - Pre-allocated voice pool sized at construction (POLYPHONY).
//! - Per-voice state is plain data — no heap allocation per note.
//! - Voice steal strategy: prefer Idle, then Releasing (closest to
//!   silence), else oldest active.
//! - Pitch shift via linear interpolation between two consecutive
//!   source frames — cheap and audibly clean for non-extreme intervals.
//! - **ADSR via the shared [`stardust_dsp::Envelope`] primitive**, so
//!   the SFZ engine and the built-in sine synth use the same
//!   envelope code path.
//! - Looping (`loop_continuous`) wraps the read position between
//!   `loop_start` and `loop_end` while the voice is in
//!   Attack/Decay/Sustain. Releasing voices play through past
//!   `loop_end` so the tail rings out naturally.
//! - **Sustain pedal** (CC 64): note-offs are deferred while held; on
//!   pedal-up every deferred note releases.
//! - **Pitch bend** is applied as a global pitch offset (default range
//!   ±2 semitones) to the per-voice playback rate.
//!
//! Channel routing: the engine renders interleaved stereo. Mono source
//! samples get duplicated to both channels with optional pan applied.

use crate::instrument::Instrument;
use crate::sfz::LoopMode;
use stardust_dsp::{AdsrConfig, EnvState, Envelope};
use std::sync::Arc;

/// Maximum polyphony. 32 is plenty for a piano-like instrument and
/// keeps memory predictable.
pub const POLYPHONY: usize = 32;

/// Pitch-bend range in semitones. SFZ supports per-region bend range
/// via `bend_up`/`bend_down`; we default to ±2 (general MIDI spec) and
/// leave per-region overrides as a future enhancement.
const PITCH_BEND_SEMITONES: f32 = 2.0;

/// One playing note.
#[derive(Clone, Copy)]
struct Voice {
    /// Index into the instrument's `regions`. `usize::MAX` = inactive.
    region_index: usize,
    /// MIDI key — used for matching note-offs.
    key: u8,
    /// Channel — matched alongside key for note-offs.
    channel: u8,
    /// Fractional read position in the source sample's frames.
    position: f64,
    /// Base per-source-frame increment (sr_ratio * pitch from key
    /// vs keycenter + tune/transpose). Pitch bend multiplies this.
    base_increment: f64,
    /// Linear amplitude after velocity + volume_db. Held constant.
    gain: f32,
    /// Pan scalar pair (left_gain, right_gain) — equal-power.
    pan_lr: (f32, f32),
    /// Monotonic allocation counter for stealing the oldest active voice.
    age: u64,
    /// Envelope state.
    env: Envelope,
    /// True after note-off when the engine is waiting for the sustain
    /// pedal to lift before actually releasing.
    sustaining: bool,
}

impl Voice {
    fn inactive() -> Self {
        Self {
            region_index: usize::MAX,
            key: 0,
            channel: 0,
            position: 0.0,
            base_increment: 1.0,
            gain: 0.0,
            pan_lr: (1.0, 1.0),
            age: 0,
            env: Envelope::new(AdsrConfig::default(), 48_000.0),
            sustaining: false,
        }
    }

    fn is_active(&self) -> bool {
        self.region_index != usize::MAX && self.env.is_active()
    }
}

/// The sample-playback engine.
pub struct Engine {
    instrument: Arc<Instrument>,
    voices: [Voice; POLYPHONY],
    sample_rate: f32,
    age_counter: u64,
    /// Current pitch-bend value, normalised to -1.0..=1.0.
    pitch_bend: f32,
    /// True while CC 64 (sustain) is held above the on threshold.
    sustain_pedal: bool,
}

impl Engine {
    /// Construct an engine for an instrument + output sample rate.
    pub fn new(instrument: Arc<Instrument>, sample_rate: f32) -> Self {
        Self {
            instrument,
            voices: [Voice::inactive(); POLYPHONY],
            sample_rate,
            age_counter: 0,
            pitch_bend: 0.0,
            sustain_pedal: false,
        }
    }

    /// Replace the loaded instrument and reset all voices. Call from
    /// the main thread between blocks, not from the audio callback.
    pub fn replace_instrument(&mut self, instrument: Arc<Instrument>) {
        self.all_notes_off();
        self.instrument = instrument;
    }

    /// Number of voices currently producing sound.
    pub fn active_voice_count(&self) -> usize {
        self.voices.iter().filter(|v| v.is_active()).count()
    }

    /// True when no voice is producing audio — host can sleep the
    /// processor in that case.
    pub fn is_idle(&self) -> bool {
        self.active_voice_count() == 0
    }

    /// Start a note. Picks the best-matching region (last-match-wins
    /// per SFZ convention); silently drops the event if no region
    /// claims this (key, velocity) pair.
    pub fn note_on(&mut self, channel: u8, key: u8, velocity: u8) {
        let Some((region_index, region)) = self
            .instrument
            .regions
            .iter()
            .enumerate()
            .rev()
            .find(|(_, r)| r.region.matches(key, velocity))
        else {
            return;
        };

        let sample = &self.instrument.samples[region.sample_index];
        let sr_ratio = sample.sample_rate as f64 / self.sample_rate as f64;
        let pitch_semis = (key as f32 - region.region.pitch_keycenter as f32)
            + region.region.pitch_offset_semitones();
        let pitch_mult = 2.0f64.powf(pitch_semis as f64 / 12.0);
        let base_increment = sr_ratio * pitch_mult;

        let vel_gain = velocity as f32 / 127.0;
        let db_gain = 10.0f32.powf(region.region.volume_db / 20.0);
        let gain = vel_gain * db_gain;

        let pan_lr = pan_to_lr(region.region.pan);

        let env = Envelope::new(
            AdsrConfig {
                attack_secs: region.region.ampeg_attack_secs.max(0.0005),
                decay_secs: region.region.ampeg_decay_secs.max(0.0001),
                sustain_level: region.region.ampeg_sustain,
                release_secs: region.region.ampeg_release_secs,
            },
            self.sample_rate,
        );

        let idx = self.pick_voice();
        self.age_counter = self.age_counter.wrapping_add(1);
        let mut v = Voice {
            region_index,
            key,
            channel,
            position: 0.0,
            base_increment,
            gain,
            pan_lr,
            age: self.age_counter,
            env,
            sustaining: false,
        };
        v.env.trigger();
        self.voices[idx] = v;
    }

    /// Release any voice currently playing the given (channel, key).
    /// If the sustain pedal is held, mark the voices as deferred-release
    /// instead — they'll release when the pedal lifts.
    pub fn note_off(&mut self, channel: u8, key: u8) {
        for v in &mut self.voices {
            if !v.is_active() || v.key != key || v.channel != channel {
                continue;
            }
            if self.sustain_pedal {
                v.sustaining = true;
            } else {
                v.env.release();
            }
        }
    }

    /// MIDI CC. We honour:
    /// - **CC 64 (sustain pedal)**: >=64 = on, <64 = off; releasing
    ///   sweeps any sustained voices.
    /// - **CC 123 (all notes off)**: triggers `all_notes_off`.
    pub fn control_change(&mut self, _channel: u8, cc: u8, value: u8) {
        match cc {
            64 => {
                let down = value >= 64;
                let was_down = self.sustain_pedal;
                self.sustain_pedal = down;
                if was_down && !down {
                    // Pedal lifted — release any held voices.
                    for v in &mut self.voices {
                        if v.sustaining {
                            v.env.release();
                            v.sustaining = false;
                        }
                    }
                }
            }
            123 => self.all_notes_off(),
            _ => {}
        }
    }

    /// MIDI pitch bend. `value` is the raw 14-bit centred bend value:
    /// 0 = full down, 8192 = no bend, 16383 = full up. Stored as a
    /// normalised -1.0..=1.0 multiplier applied to voice increments.
    pub fn pitch_bend(&mut self, _channel: u8, value: i16) {
        // CLAP delivers pitch bend as a normalised f64; the integration
        // layer converts. For MIDI-derived ints, treat as 14-bit centred
        // around 8192 (the host passes value = raw - 8192 already).
        let normalised = (value as f32 / 8192.0).clamp(-1.0, 1.0);
        self.pitch_bend = normalised;
    }

    /// Same as `pitch_bend` but takes a pre-normalised value.
    pub fn pitch_bend_normalised(&mut self, _channel: u8, value: f32) {
        self.pitch_bend = value.clamp(-1.0, 1.0);
    }

    /// Force every voice silent immediately — host calls this on
    /// panic, patch change, or stop_processing.
    pub fn all_notes_off(&mut self) {
        for v in &mut self.voices {
            v.env.reset();
            v.region_index = usize::MAX;
            v.sustaining = false;
        }
        self.sustain_pedal = false;
    }

    /// Render `out` (interleaved stereo, length must be `frames * 2`).
    /// ADDS into the buffer — caller zeroes first if mixing multiple
    /// engines into the same output.
    pub fn render_into_stereo(&mut self, out: &mut [f32]) {
        let bend_mult = 2.0f64.powf((self.pitch_bend * PITCH_BEND_SEMITONES) as f64 / 12.0);
        let frames = out.len() / 2;
        for f in 0..frames {
            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for v in &mut self.voices {
                if v.region_index == usize::MAX {
                    continue;
                }
                if !v.env.is_active() {
                    v.region_index = usize::MAX;
                    continue;
                }
                let region = &self.instrument.regions[v.region_index];
                let sample = &self.instrument.samples[region.sample_index];

                // Read with linear interpolation.
                let total_frames = sample.frames();
                let pos_floor = v.position.floor() as usize;
                let pos_next = pos_floor + 1;
                if pos_next >= total_frames {
                    // Past sample end. Loop or stop.
                    if region.region.loop_mode == LoopMode::LoopContinuous
                        && v.env.state() != EnvState::Released
                    {
                        let (lstart, lend) = loop_bounds(region, total_frames);
                        v.position = lstart + (v.position - lend as f64).max(0.0);
                    } else {
                        v.region_index = usize::MAX;
                        v.env.reset();
                        continue;
                    }
                }
                let pos_floor = v.position.floor() as usize;
                let frac = (v.position - pos_floor as f64) as f32;
                let (l0, r0) = sample.frame(pos_floor);
                let (l1, r1) = sample.frame(pos_floor + 1);
                let l = l0 + (l1 - l0) * frac;
                let r = r0 + (r1 - r0) * frac;
                let env = v.env.tick();
                let amp = v.gain * env;
                left += l * amp * v.pan_lr.0;
                right += r * amp * v.pan_lr.1;

                v.position += v.base_increment * bend_mult;

                // Loop wrap during sustained playback.
                if region.region.loop_mode == LoopMode::LoopContinuous
                    && v.env.state() != EnvState::Released
                {
                    let (lstart, lend) = loop_bounds(region, total_frames);
                    if v.position >= lend as f64 {
                        v.position = lstart + (v.position - lend as f64);
                    }
                }
            }
            out[f * 2] += left;
            out[f * 2 + 1] += right;
        }
    }

    fn pick_voice(&self) -> usize {
        if let Some((i, _)) = self.voices.iter().enumerate().find(|(_, v)| !v.is_active()) {
            return i;
        }
        if let Some((i, _)) = self
            .voices
            .iter()
            .enumerate()
            .find(|(_, v)| v.env.state() == EnvState::Released)
        {
            return i;
        }
        self.voices
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| v.age)
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
}

/// Resolve loop start/end for a region, defaulting `loop_end == 0` to
/// the end of the sample.
fn loop_bounds(region: &crate::instrument::InstrumentRegion, total_frames: usize) -> (f64, u64) {
    let start = region.region.loop_start as f64;
    let end = if region.region.loop_end == 0 {
        total_frames as u64
    } else {
        region.region.loop_end.min(total_frames as u64)
    };
    (start.min(end.saturating_sub(1) as f64), end.max(1))
}

/// SFZ pan: -100 = full L, +100 = full R, 0 = centred. Equal-power.
fn pan_to_lr(pan: f32) -> (f32, f32) {
    let p = (pan / 100.0).clamp(-1.0, 1.0);
    let angle = (p + 1.0) * std::f32::consts::FRAC_PI_4; // 0..π/2
    (angle.cos(), angle.sin())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::InstrumentRegion;
    use crate::sample::Sample;
    use crate::sfz::Region;
    use std::path::PathBuf;

    fn ramp_sample(rate: u32, frames: usize) -> Sample {
        let data: Vec<f32> = (0..frames).map(|i| i as f32 / frames as f32).collect();
        Sample {
            data,
            channels: 1,
            sample_rate: rate,
        }
    }

    fn instrument_with_one_region() -> Arc<Instrument> {
        let sample = ramp_sample(48_000, 4_800);
        let region = InstrumentRegion {
            region: Region {
                sample: PathBuf::from("ramp"),
                lokey: 60,
                hikey: 60,
                pitch_keycenter: 60,
                ..Region::default()
            },
            sample_index: 0,
        };
        Arc::new(Instrument {
            regions: vec![region],
            samples: vec![sample],
        })
    }

    #[test]
    fn note_on_off_lifecycle() {
        let mut e = Engine::new(instrument_with_one_region(), 48_000.0);
        assert_eq!(e.active_voice_count(), 0);
        e.note_on(0, 60, 100);
        assert_eq!(e.active_voice_count(), 1);
        e.note_off(0, 60);
        // Render past the release tail.
        let mut buf = vec![0.0f32; 48_000];
        e.render_into_stereo(&mut buf);
        assert_eq!(e.active_voice_count(), 0);
    }

    #[test]
    fn sustain_pedal_defers_note_off() {
        let mut e = Engine::new(instrument_with_one_region(), 48_000.0);
        e.note_on(0, 60, 100);
        e.control_change(0, 64, 127); // pedal down
        e.note_off(0, 60);
        // With pedal held the voice should still be active.
        let mut tiny = vec![0.0f32; 200 * 2];
        e.render_into_stereo(&mut tiny);
        assert_eq!(e.active_voice_count(), 1);
        // Release the pedal — voice should now release and die out.
        e.control_change(0, 64, 0);
        let mut buf = vec![0.0f32; 48_000 * 2];
        e.render_into_stereo(&mut buf);
        assert_eq!(e.active_voice_count(), 0);
    }

    #[test]
    fn all_notes_off_via_cc_123() {
        let mut e = Engine::new(instrument_with_one_region(), 48_000.0);
        e.note_on(0, 60, 100);
        e.control_change(0, 123, 0);
        assert_eq!(e.active_voice_count(), 0);
    }

    #[test]
    fn pitch_bend_changes_playback_rate() {
        let inst = instrument_with_one_region();
        let mut a = Engine::new(inst.clone(), 48_000.0);
        let mut b = Engine::new(inst.clone(), 48_000.0);
        a.note_on(0, 60, 127);
        b.note_on(0, 60, 127);
        b.pitch_bend_normalised(0, 1.0); // +2 semitones at default range
        let mut buf_a = vec![0.0f32; 200 * 2];
        let mut buf_b = vec![0.0f32; 200 * 2];
        a.render_into_stereo(&mut buf_a);
        b.render_into_stereo(&mut buf_b);
        let a_last = buf_a[buf_a.len() - 2];
        let b_last = buf_b[buf_b.len() - 2];
        assert!(
            b_last > a_last * 1.1,
            "pitch-bent voice should have advanced further: a={a_last} b={b_last}"
        );
    }

    #[test]
    fn note_outside_region_range_is_silent() {
        let mut e = Engine::new(instrument_with_one_region(), 48_000.0);
        e.note_on(0, 72, 100);
        assert_eq!(e.active_voice_count(), 0);
    }

    #[test]
    fn loop_continuous_keeps_voice_alive_past_sample_end() {
        let mut inst = (*instrument_with_one_region()).clone();
        inst.regions[0].region.loop_mode = LoopMode::LoopContinuous;
        inst.regions[0].region.loop_start = 0;
        inst.regions[0].region.loop_end = 0; // means "use sample length"
        let mut e = Engine::new(Arc::new(inst), 48_000.0);
        e.note_on(0, 60, 100);
        // Sample is 4800 frames; render 20_000 frames worth — without
        // looping the voice would die long before then.
        let mut buf = vec![0.0f32; 20_000 * 2];
        e.render_into_stereo(&mut buf);
        assert!(e.active_voice_count() > 0);
    }

    #[test]
    fn pan_full_left_silences_right() {
        let mut inst = (*instrument_with_one_region()).clone();
        inst.regions[0].region.pan = -100.0;
        let mut e = Engine::new(Arc::new(inst), 48_000.0);
        e.note_on(0, 60, 127);
        let mut buf = vec![0.0f32; 500 * 2];
        e.render_into_stereo(&mut buf);
        let max_left = buf
            .iter()
            .step_by(2)
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        let max_right = buf
            .iter()
            .skip(1)
            .step_by(2)
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        assert!(max_left > 0.05, "left channel should be audible");
        assert!(
            max_right < max_left * 0.05,
            "right should be near-silent (got {max_right})"
        );
    }
}
