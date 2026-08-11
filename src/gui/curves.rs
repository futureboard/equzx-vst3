//! Frequency-response curves for the display.
//!
//! Ported from `dsp/biquad.ts` and `dsp/bands.ts`, which the web UI needed
//! because it could not see the Rust filters. This build can — the coefficients
//! come straight from [`crate::dsp::biquad`] — but the *evaluation* still wants
//! its own home, because drawing has a constraint the DSP does not: the same few
//! hundred frequencies are asked about sixty times a second.
//!
//! Hence [`ResponseGrid`]. `Coeffs::magnitude` costs four transcendentals per
//! call, and a full display is up to twenty-four bands of up to eight sections
//! across four hundred and eighty points — around ninety thousand evaluations a
//! frame. Precomputing the four sinusoids per grid point turns each of those
//! into a dozen multiply-adds, which is the difference between a frame budget
//! spent and a frame budget noticed.

use crate::dsp::biquad::{butterworth_qs, Coeffs, MAX_SECTIONS};
use crate::gui::state::BandView;
use crate::params::BandKind;

/// Display limits. The analyser clamps its own top end to Nyquist; the axis does
/// not move, so a 44.1 kHz session simply has nothing drawn in its last few
/// pixels rather than a display that changes width with the sample rate.
pub const F_MIN: f32 = 20.0;
pub const F_MAX: f32 = 22_000.0;

/// Segments across the axis. One more point than this is evaluated.
///
/// Fine enough that a segment is shorter than a pixel and a half at the
/// default window: a steep bell drawn with the wide translucent glow strokes
/// shows every kink, and 480 was visibly polygonal at the apex.
pub const CURVE_POINTS: usize = 960;

/// The frequencies a curve is evaluated at, with the per-frequency terms of
/// `|H(e^jw)|` already worked out.
pub struct ResponseGrid {
    freqs: Vec<f32>,
    cos_w: Vec<f32>,
    sin_w: Vec<f32>,
    cos_2w: Vec<f32>,
    sin_2w: Vec<f32>,
    sample_rate: f32,
}

impl ResponseGrid {
    pub fn new(sample_rate: f32) -> Self {
        let n = CURVE_POINTS + 1;
        let mut grid = Self {
            freqs: (0..n)
                .map(|i| F_MIN * (F_MAX / F_MIN).powf(i as f32 / CURVE_POINTS as f32))
                .collect(),
            cos_w: vec![0.0; n],
            sin_w: vec![0.0; n],
            cos_2w: vec![0.0; n],
            sin_2w: vec![0.0; n],
            sample_rate: 0.0,
        };
        grid.set_sample_rate(sample_rate);
        grid
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (sample_rate - self.sample_rate).abs() < f32::EPSILON || sample_rate <= 0.0 {
            return;
        }
        self.sample_rate = sample_rate;
        for i in 0..self.freqs.len() {
            let w = 2.0 * std::f32::consts::PI * self.freqs[i] / sample_rate;
            let (s, c) = w.sin_cos();
            let (s2, c2) = (2.0 * w).sin_cos();
            self.sin_w[i] = s;
            self.cos_w[i] = c;
            self.sin_2w[i] = s2;
            self.cos_2w[i] = c2;
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn len(&self) -> usize {
        self.freqs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.freqs.is_empty()
    }

    pub fn freq(&self, i: usize) -> f32 {
        self.freqs[i]
    }

    pub fn freqs(&self) -> &[f32] {
        &self.freqs
    }

    /// `|H(e^jw)|` of one section at grid point `i`.
    pub fn magnitude(&self, c: &Coeffs, i: usize) -> f32 {
        let (cw, sw) = (self.cos_w[i], self.sin_w[i]);
        let (c2w, s2w) = (self.cos_2w[i], self.sin_2w[i]);

        let num_re = c.b0 + c.b1 * cw + c.b2 * c2w;
        let num_im = -(c.b1 * sw + c.b2 * s2w);
        let den_re = 1.0 + c.a1 * cw + c.a2 * c2w;
        let den_im = -(c.a1 * sw + c.a2 * s2w);

        let den = den_re.hypot(den_im);
        if den == 0.0 {
            0.0
        } else {
            num_re.hypot(num_im) / den
        }
    }

    /// Combined response of a cascade at grid point `i`, in dB.
    pub fn db_at(&self, sections: &[Coeffs], i: usize) -> f32 {
        let mut mag = 1.0;
        for section in sections {
            mag *= self.magnitude(section, i);
        }
        20.0 * mag.max(1e-7).log10()
    }

    /// The whole cascade, written into `out`.
    pub fn curve(&self, sections: &[Coeffs], out: &mut [f32]) {
        for (i, slot) in out.iter_mut().enumerate().take(self.len()) {
            *slot = self.db_at(sections, i);
        }
    }
}

/// Expand one band into the sections that realise it. Cut bands become a
/// Butterworth cascade; everything else is a single section.
///
/// The mirror of `bandSections` in `dsp/bands.ts`, and of what
/// [`crate::dsp::engine`] builds for the audio path — which is the point: a
/// curve the display draws and a curve the user hears have to be the same one.
pub fn band_sections(band: &BandView, sample_rate: f32, out: &mut Vec<Coeffs>) {
    out.clear();
    let f = band.freq.clamp(10.0, sample_rate / 2.0 - 1.0);
    match band.kind {
        BandKind::Bell => out.push(Coeffs::peaking(f, band.q, band.gain, sample_rate)),
        BandKind::LowShelf => out.push(Coeffs::low_shelf(f, band.gain, sample_rate)),
        BandKind::HighShelf => out.push(Coeffs::high_shelf(f, band.gain, sample_rate)),
        BandKind::Notch => out.push(Coeffs::notch(f, band.q, sample_rate)),
        BandKind::BandPass => out.push(Coeffs::bandpass(f, band.q, sample_rate)),
        BandKind::LowCut | BandKind::HighCut => {
            let mut qs = [0.0f32; MAX_SECTIONS];
            let n = butterworth_qs(band.slope.order(), &mut qs);
            for &q in qs.iter().take(n) {
                out.push(match band.kind {
                    BandKind::LowCut => Coeffs::highpass(f, q, sample_rate),
                    _ => Coeffs::lowpass(f, q, sample_rate),
                });
            }
        }
    }
}

/// Q to bandwidth in octaves, and back — the RBJ relation the Q handles are
/// dragged along.
pub fn q_to_octaves(q: f32) -> f32 {
    (2.0 / std::f32::consts::LN_2) * (1.0 / (2.0 * q)).asinh()
}

pub fn octaves_to_q(bandwidth: f32) -> f32 {
    1.0 / (2.0 * ((std::f32::consts::LN_2 / 2.0) * bandwidth.max(0.02)).sinh())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{BandChannel, DynMode, Slope};

    const SR: f32 = 48_000.0;

    fn bell(freq: f32, gain: f32, q: f32) -> BandView {
        BandView {
            slot: 0,
            kind: BandKind::Bell,
            channel: BandChannel::Stereo,
            freq,
            gain,
            q,
            slope: Slope::S24,
            enabled: true,
            dynamic: false,
            dyn_mode: DynMode::Above,
            dyn_range: -6.0,
            threshold: -24.0,
            attack: 20.0,
            release: 200.0,
            resonance: 0.0,
        }
    }

    /// Index of the grid point nearest a frequency.
    fn point_for(freq: f32) -> usize {
        let t = (freq / F_MIN).ln() / (F_MAX / F_MIN).ln();
        (t * CURVE_POINTS as f32).round() as usize
    }

    #[test]
    fn the_grid_agrees_with_evaluating_a_section_directly() {
        let grid = ResponseGrid::new(SR);
        let c = Coeffs::peaking(1000.0, 1.0, 6.0, SR);
        for i in (0..grid.len()).step_by(37) {
            let direct = c.magnitude(grid.freq(i), SR);
            let cached = grid.magnitude(&c, i);
            assert!(
                (direct - cached).abs() < 1e-4,
                "point {i} at {} Hz: {direct} vs {cached}",
                grid.freq(i)
            );
        }
    }

    #[test]
    fn a_bell_peaks_at_its_own_frequency_and_gain() {
        let grid = ResponseGrid::new(SR);
        let mut sections = Vec::new();
        band_sections(&bell(1000.0, 6.0, 2.0), SR, &mut sections);

        let peak = grid.db_at(&sections, point_for(1000.0));
        assert!((peak - 6.0).abs() < 0.1, "peak was {peak} dB");
        // Three octaves down a Q of 2 has nothing left to say.
        assert!(grid.db_at(&sections, point_for(125.0)).abs() < 0.5);
    }

    #[test]
    fn a_cut_is_three_db_down_at_its_corner_whatever_the_slope() {
        let grid = ResponseGrid::new(SR);
        let mut sections = Vec::new();
        for slope in [Slope::S12, Slope::S24, Slope::S48, Slope::S96] {
            let mut band = bell(100.0, 0.0, 1.0);
            band.kind = BandKind::LowCut;
            band.slope = slope;
            band_sections(&band, SR, &mut sections);
            assert_eq!(sections.len(), slope.order() / 2);

            let at_corner = grid.db_at(&sections, point_for(100.0));
            assert!(
                (at_corner + 3.0).abs() < 0.35,
                "{:?} read {at_corner} dB at the corner",
                slope
            );
            // And an octave below, roughly the slope it advertises.
            let below = grid.db_at(&sections, point_for(50.0));
            let expected = -(slope.db_per_oct() as f32);
            assert!(
                (below - expected).abs() < expected.abs() * 0.2,
                "{:?} read {below} dB an octave down, expected near {expected}",
                slope
            );
        }
    }

    #[test]
    fn bandwidth_round_trips_through_q() {
        for q in [0.1f32, 0.5, 1.0, 4.0, 20.0] {
            let back = octaves_to_q(q_to_octaves(q));
            assert!((back - q).abs() < q * 1e-3, "{q} came back as {back}");
        }
    }

    #[test]
    fn changing_the_sample_rate_rebuilds_the_terms() {
        let mut grid = ResponseGrid::new(44_100.0);
        let before = grid.cos_w[10];
        grid.set_sample_rate(96_000.0);
        assert_ne!(before, grid.cos_w[10]);
        assert_eq!(grid.sample_rate(), 96_000.0);
        // Frequencies are a property of the axis, not the rate.
        assert_eq!(grid.freq(0), F_MIN);
    }
}
