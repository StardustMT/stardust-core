//! Three-band stereo EQ.
//!
//! Two biquad shelves (low + high) plus a peaking-EQ mid band, applied
//! independently per channel. Gains are in dB; 0 dB on every band is a
//! bit-exact passthrough modulo IEEE-754 noise.
//!
//! ## RT-safety
//!
//! [`Eq::new`] pre-builds the filter state. [`Eq::set_gains`] and
//! [`Eq::process`] are allocation-free. Coefficient recomputation only
//! happens when gains change, not per sample.
//!
//! ## Filter shape
//!
//! Cookbook biquads (Robert Bristow-Johnson). Crossovers are constants
//! for v1 — the catalog UI doesn't expose them and the EQ exists to
//! cover the seed-data patches that chain `instrument → eq → mix → out`.
//! Future revisions can promote them to runtime config; the per-band
//! [`Band`] struct already takes its frequency at construction so this
//! is a wiring change, not a DSP change.

use std::f32::consts::TAU;

/// Low shelf corner, mid peak center, high shelf corner.
const LOW_HZ: f32 = 250.0;
const MID_HZ: f32 = 1_000.0;
const HIGH_HZ: f32 = 4_000.0;

/// Q for the mid peaking band. Shelves use a slope parameter instead.
const MID_Q: f32 = 0.71;

/// Shelf slope (1.0 = maximally steep, no resonance peak).
const SHELF_SLOPE: f32 = 1.0;

/// Tuning of the three bands. Mirrors the catalog's `{ low, mid, high }`
/// config shape — gains are decibels in the same `-12..=+12` range the
/// UI panel exposes (though nothing here clamps; the plan builder is
/// responsible for sanity-checking config).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqGains {
    /// Low shelf gain, dB.
    pub low_db: f32,
    /// Mid peak gain, dB.
    pub mid_db: f32,
    /// High shelf gain, dB.
    pub high_db: f32,
}

impl Default for EqGains {
    fn default() -> Self {
        Self {
            low_db: 0.0,
            mid_db: 0.0,
            high_db: 0.0,
        }
    }
}

/// Three-band stereo EQ. One pair of biquad states per band (left + right).
#[derive(Clone, Debug)]
pub struct Eq {
    sample_rate: f32,
    gains: EqGains,
    low_l: Biquad,
    low_r: Biquad,
    mid_l: Biquad,
    mid_r: Biquad,
    high_l: Biquad,
    high_r: Biquad,
}

impl Eq {
    /// Create an EQ with flat (0 dB) gains.
    pub fn new(sample_rate: f32) -> Self {
        Self::with_gains(sample_rate, EqGains::default())
    }

    /// Create an EQ with the given band gains.
    pub fn with_gains(sample_rate: f32, gains: EqGains) -> Self {
        debug_assert!(sample_rate > 0.0);
        let (low, mid, high) = coeffs(sample_rate, gains);
        Self {
            sample_rate,
            gains,
            low_l: Biquad::with_coeffs(low),
            low_r: Biquad::with_coeffs(low),
            mid_l: Biquad::with_coeffs(mid),
            mid_r: Biquad::with_coeffs(mid),
            high_l: Biquad::with_coeffs(high),
            high_r: Biquad::with_coeffs(high),
        }
    }

    /// Replace the band gains. Recomputes coefficients but preserves the
    /// running filter state (no audible click on small gain changes).
    pub fn set_gains(&mut self, gains: EqGains) {
        if self.gains == gains {
            return;
        }
        self.gains = gains;
        let (low, mid, high) = coeffs(self.sample_rate, gains);
        self.low_l.coeffs = low;
        self.low_r.coeffs = low;
        self.mid_l.coeffs = mid;
        self.mid_r.coeffs = mid;
        self.high_l.coeffs = high;
        self.high_r.coeffs = high;
    }

    /// Process a stereo block in place. `left` and `right` must have the
    /// same length. Allocation-free.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        debug_assert_eq!(left.len(), right.len());
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            *l = self.high_l.tick(self.mid_l.tick(self.low_l.tick(*l)));
            *r = self.high_r.tick(self.mid_r.tick(self.low_r.tick(*r)));
        }
    }

    /// Current band gains. Mostly useful for tests.
    pub fn gains(&self) -> EqGains {
        self.gains
    }
}

fn coeffs(sample_rate: f32, gains: EqGains) -> (BiquadCoeffs, BiquadCoeffs, BiquadCoeffs) {
    (
        BiquadCoeffs::low_shelf(sample_rate, LOW_HZ, SHELF_SLOPE, gains.low_db),
        BiquadCoeffs::peaking(sample_rate, MID_HZ, MID_Q, gains.mid_db),
        BiquadCoeffs::high_shelf(sample_rate, HIGH_HZ, SHELF_SLOPE, gains.high_db),
    )
}

// -----------------------------------------------------------------------------
// Biquad (direct-form II transposed). Coefficients via the audio cookbook.
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct BiquadCoeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl BiquadCoeffs {
    fn peaking(sample_rate: f32, freq: f32, q: f32, gain_db: f32) -> Self {
        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = TAU * freq / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    fn low_shelf(sample_rate: f32, freq: f32, slope: f32, gain_db: f32) -> Self {
        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = TAU * freq / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / 2.0 * ((a + 1.0 / a) * (1.0 / slope - 1.0) + 2.0).sqrt();
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    fn high_shelf(sample_rate: f32, freq: f32, slope: f32, gain_db: f32) -> Self {
        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = TAU * freq / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / 2.0 * ((a + 1.0 / a) * (1.0 / slope - 1.0) + 2.0).sqrt();
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Biquad {
    coeffs: BiquadCoeffs,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn with_coeffs(coeffs: BiquadCoeffs) -> Self {
        Self {
            coeffs,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let y = self.coeffs.b0 * x + self.z1;
        self.z1 = self.coeffs.b1 * x - self.coeffs.a1 * y + self.z2;
        self.z2 = self.coeffs.b2 * x - self.coeffs.a2 * y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0 dB on every band shouldn't materially change the signal.
    #[test]
    fn flat_eq_is_near_passthrough() {
        let mut eq = Eq::new(48_000.0);
        let mut l: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut r = l.clone();
        let input = l.clone();
        eq.process(&mut l, &mut r);
        // Both channels should track the input within a small numerical tolerance.
        for (i, (out, inp)) in l.iter().zip(input.iter()).enumerate() {
            assert!(
                (out - inp).abs() < 1e-3,
                "sample {i} drifted: out={out} input={inp}"
            );
        }
        for (out, inp) in r.iter().zip(input.iter()) {
            assert!((out - inp).abs() < 1e-3);
        }
    }

    /// Boosting the low shelf should make low-frequency content louder.
    #[test]
    fn low_boost_amplifies_low_freq() {
        let sr = 48_000.0;
        let mut flat = Eq::new(sr);
        let mut boost = Eq::with_gains(
            sr,
            EqGains {
                low_db: 12.0,
                mid_db: 0.0,
                high_db: 0.0,
            },
        );
        // 80 Hz sine — well below the 250 Hz shelf corner.
        let signal: Vec<f32> = (0..4800)
            .map(|i| (TAU * 80.0 * i as f32 / sr).sin())
            .collect();
        let mut l_flat = signal.clone();
        let mut r_flat = signal.clone();
        let mut l_boost = signal.clone();
        let mut r_boost = signal.clone();
        flat.process(&mut l_flat, &mut r_flat);
        boost.process(&mut l_boost, &mut r_boost);
        // Compare RMS after a settling window.
        let rms = |buf: &[f32]| -> f32 {
            let tail = &buf[buf.len() / 2..];
            (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt()
        };
        let flat_rms = rms(&l_flat);
        let boost_rms = rms(&l_boost);
        assert!(
            boost_rms > flat_rms * 2.0,
            "expected low boost to roughly double RMS; flat={flat_rms} boost={boost_rms}"
        );
    }

    /// Cutting the high shelf should attenuate high-frequency content.
    #[test]
    fn high_cut_attenuates_high_freq() {
        let sr = 48_000.0;
        let mut flat = Eq::new(sr);
        let mut cut = Eq::with_gains(
            sr,
            EqGains {
                low_db: 0.0,
                mid_db: 0.0,
                high_db: -12.0,
            },
        );
        // 10 kHz sine — well above the 4 kHz shelf corner.
        let signal: Vec<f32> = (0..4800)
            .map(|i| (TAU * 10_000.0 * i as f32 / sr).sin())
            .collect();
        let mut l_flat = signal.clone();
        let mut r_flat = signal.clone();
        let mut l_cut = signal.clone();
        let mut r_cut = signal.clone();
        flat.process(&mut l_flat, &mut r_flat);
        cut.process(&mut l_cut, &mut r_cut);
        let rms = |buf: &[f32]| -> f32 {
            let tail = &buf[buf.len() / 2..];
            (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt()
        };
        let flat_rms = rms(&l_flat);
        let cut_rms = rms(&l_cut);
        assert!(
            cut_rms < flat_rms * 0.6,
            "expected high cut to attenuate; flat={flat_rms} cut={cut_rms}"
        );
    }

    /// set_gains shouldn't reset filter state (no click on small changes).
    #[test]
    fn set_gains_preserves_state() {
        let mut eq = Eq::new(48_000.0);
        let mut l = vec![1.0f32; 64];
        let mut r = vec![1.0f32; 64];
        eq.process(&mut l, &mut r);
        let z_before = eq.low_l.z1;
        eq.set_gains(EqGains {
            low_db: 3.0,
            mid_db: 0.0,
            high_db: 0.0,
        });
        // z-state is preserved verbatim; coefficients update but the filter
        // delay line carries over.
        assert_eq!(eq.low_l.z1, z_before);
    }

    /// Setting the same gains is a no-op (cheap fast path).
    #[test]
    fn set_gains_idempotent() {
        let mut eq = Eq::with_gains(
            48_000.0,
            EqGains {
                low_db: 3.0,
                mid_db: -2.0,
                high_db: 1.0,
            },
        );
        let coeffs_before = eq.low_l.coeffs.b0;
        eq.set_gains(EqGains {
            low_db: 3.0,
            mid_db: -2.0,
            high_db: 1.0,
        });
        assert_eq!(eq.low_l.coeffs.b0, coeffs_before);
    }
}
