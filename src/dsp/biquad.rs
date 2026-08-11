//! Biquad coefficients and a transposed-direct-form-II section.
//!
//! The formulas are the RBJ cookbook set used by the Web Audio API, matched
//! term for term with `editor/src/dsp/biquad.ts`. That is deliberate: the UI
//! draws its curve from the TypeScript version while the audio runs through
//! this one, so any drift between them would show up as a curve that lies.

use std::f32::consts::PI;

/// A normalized biquad (a0 divided out).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Default for Coeffs {
    fn default() -> Self {
        Self::identity()
    }
}

impl Coeffs {
    pub const fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    fn norm(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        // a0 is only ever zero if the caller handed us a degenerate frequency; an
        // identity section is a far better failure than NaN spraying into the mix.
        if a0.abs() < f32::EPSILON || !a0.is_finite() {
            return Self::identity();
        }
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    pub fn peaking(freq: f32, q: f32, gain_db: f32, sr: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / sr;
        let cw = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        Self::norm(
            1.0 + alpha * a,
            -2.0 * cw,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cw,
            1.0 - alpha / a,
        )
    }

    pub fn low_shelf(freq: f32, gain_db: f32, sr: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / sr;
        let cw = w0.cos();
        // S = 1, matching Web Audio's fixed shelf slope.
        let alpha = w0.sin() / 2.0 * std::f32::consts::SQRT_2;
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        Self::norm(
            a * (a + 1.0 - (a - 1.0) * cw + two_sqrt_a_alpha),
            2.0 * a * (a - 1.0 - (a + 1.0) * cw),
            a * (a + 1.0 - (a - 1.0) * cw - two_sqrt_a_alpha),
            a + 1.0 + (a - 1.0) * cw + two_sqrt_a_alpha,
            -2.0 * (a - 1.0 + (a + 1.0) * cw),
            a + 1.0 + (a - 1.0) * cw - two_sqrt_a_alpha,
        )
    }

    pub fn high_shelf(freq: f32, gain_db: f32, sr: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / sr;
        let cw = w0.cos();
        let alpha = w0.sin() / 2.0 * std::f32::consts::SQRT_2;
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        Self::norm(
            a * (a + 1.0 + (a - 1.0) * cw + two_sqrt_a_alpha),
            -2.0 * a * (a - 1.0 + (a + 1.0) * cw),
            a * (a + 1.0 + (a - 1.0) * cw - two_sqrt_a_alpha),
            a + 1.0 - (a - 1.0) * cw + two_sqrt_a_alpha,
            2.0 * (a - 1.0 - (a + 1.0) * cw),
            a + 1.0 - (a - 1.0) * cw - two_sqrt_a_alpha,
        )
    }

    pub fn notch(freq: f32, q: f32, sr: f32) -> Self {
        let w0 = 2.0 * PI * freq / sr;
        let cw = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        Self::norm(1.0, -2.0 * cw, 1.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha)
    }

    pub fn bandpass(freq: f32, q: f32, sr: f32) -> Self {
        let w0 = 2.0 * PI * freq / sr;
        let cw = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        Self::norm(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cw, 1.0 - alpha)
    }

    pub fn lowpass(freq: f32, q: f32, sr: f32) -> Self {
        let w0 = 2.0 * PI * freq / sr;
        let cw = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        let one_minus_cw = 1.0 - cw;
        Self::norm(
            one_minus_cw / 2.0,
            one_minus_cw,
            one_minus_cw / 2.0,
            1.0 + alpha,
            -2.0 * cw,
            1.0 - alpha,
        )
    }

    pub fn highpass(freq: f32, q: f32, sr: f32) -> Self {
        let w0 = 2.0 * PI * freq / sr;
        let cw = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        let one_plus_cw = 1.0 + cw;
        Self::norm(
            one_plus_cw / 2.0,
            -one_plus_cw,
            one_plus_cw / 2.0,
            1.0 + alpha,
            -2.0 * cw,
            1.0 - alpha,
        )
    }

    /// |H(e^jw)| at `f`.
    ///
    /// Never called per sample: tests use it, and the resonance bank samples it
    /// once at construction to learn how much its filters overlap.
    pub fn magnitude(&self, f: f32, sr: f32) -> f32 {
        let w = 2.0 * PI * f / sr;
        let (sw, cw) = w.sin_cos();
        let (s2w, c2w) = (2.0 * w).sin_cos();

        let num_re = self.b0 + self.b1 * cw + self.b2 * c2w;
        let num_im = -(self.b1 * sw + self.b2 * s2w);
        let den_re = 1.0 + self.a1 * cw + self.a2 * c2w;
        let den_im = -(self.a1 * sw + self.a2 * s2w);

        let den = den_re.hypot(den_im);
        if den == 0.0 {
            0.0
        } else {
            num_re.hypot(num_im) / den
        }
    }
}

/// One second-order section, transposed direct form II.
///
/// TDF2 rather than DF1 because it holds only two state words and stays well
/// behaved when coefficients are swapped underneath it, which happens on every
/// control block as the smoothers move.
#[derive(Clone, Copy, Default, Debug)]
pub struct Biquad {
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub const fn new() -> Self {
        Self { z1: 0.0, z2: 0.0 }
    }

    #[inline(always)]
    pub fn process(&mut self, x: f32, c: &Coeffs) -> f32 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// Highest cut slope is 96 dB/oct — order 16, which is eight second-order sections.
pub const MAX_SECTIONS: usize = 8;

/// Butterworth Q values for the sections of an even-order cascade.
/// order 2 -> [0.7071], order 4 -> [0.5412, 1.3066], and so on.
pub fn butterworth_qs(order: usize, out: &mut [f32; MAX_SECTIONS]) -> usize {
    let n = (order / 2).min(MAX_SECTIONS);
    for (k, slot) in out.iter_mut().enumerate().take(n) {
        *slot = 1.0 / (2.0 * (((2 * k + 1) as f32 * PI) / (2.0 * order as f32)).cos());
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn db(mag: f32) -> f32 {
        20.0 * mag.max(1e-7).log10()
    }

    #[test]
    fn peaking_hits_its_gain_at_center() {
        let c = Coeffs::peaking(1000.0, 1.0, 6.0, SR);
        assert!((db(c.magnitude(1000.0, SR)) - 6.0).abs() < 0.05);
        // Far from center a bell is transparent.
        assert!(db(c.magnitude(30.0, SR)).abs() < 0.2);
    }

    #[test]
    fn shelves_reach_their_gain_in_the_shelf_region() {
        let low = Coeffs::low_shelf(500.0, -8.0, SR);
        assert!((db(low.magnitude(20.0, SR)) + 8.0).abs() < 0.3);
        let high = Coeffs::high_shelf(2000.0, 5.0, SR);
        assert!((db(high.magnitude(18_000.0, SR)) - 5.0).abs() < 0.3);
    }

    #[test]
    fn butterworth_cascade_is_minus_three_db_at_cutoff() {
        for &(slope, order) in &[(12usize, 2usize), (24, 4), (48, 8), (96, 16)] {
            let mut qs = [0.0f32; MAX_SECTIONS];
            let n = butterworth_qs(order, &mut qs);
            assert_eq!(n, order / 2);

            let mut mag = 1.0;
            for &q in &qs[..n] {
                mag *= Coeffs::highpass(1000.0, q, SR).magnitude(1000.0, SR);
            }
            assert!(
                (db(mag) + 3.0).abs() < 0.25,
                "{slope} dB/oct highpass was {} dB at cutoff",
                db(mag)
            );
        }
    }

    #[test]
    fn a_cut_cascade_falls_off_at_its_rated_slope() {
        // An octave below cutoff a 24 dB/oct highpass should be ~24 dB down.
        let mut qs = [0.0f32; MAX_SECTIONS];
        let n = butterworth_qs(4, &mut qs);
        let mut mag = 1.0;
        for &q in &qs[..n] {
            mag *= Coeffs::highpass(1000.0, q, SR).magnitude(500.0, SR);
        }
        assert!((db(mag) + 24.0).abs() < 1.0, "got {} dB", db(mag));
    }

    #[test]
    fn identity_section_passes_signal_through() {
        let mut bq = Biquad::new();
        let c = Coeffs::identity();
        for x in [0.3f32, -0.7, 0.0, 1.0] {
            assert!((bq.process(x, &c) - x).abs() < 1e-6);
        }
    }
}
