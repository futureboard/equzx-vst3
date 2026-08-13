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

use crate::dsp::biquad::{band_cascade, Coeffs, MAX_SECTIONS};
use crate::gui::state::BandView;

/// Display limits. The analyser clamps its own top end to Nyquist; the axis does
/// not move, so a 44.1 kHz session simply has nothing drawn in its last few
/// pixels rather than a display that changes width with the sample rate.
pub const F_MIN: f32 = 20.0;
pub const F_MAX: f32 = 22_000.0;

/// Segments across the axis before the display measures itself — the display
/// re-grids to one evaluation per physical pixel column on its first frame
/// (see [`ResponseGrid::set_resolution`]). One more point than this is
/// evaluated.
pub const CURVE_POINTS: usize = 960;

/// The frequencies a curve is evaluated at, with the per-frequency terms of
/// `|H(e^jw)|` already worked out.
///
/// The trig tables and the evaluation are f64 on purpose, and so are the
/// coefficients they are evaluated against — see [`Coeffs`]. At the low end of
/// the axis `cos ω` sits within 1e-5 of 1.0, and the magnitude formula
/// cancels terms of size 2 down to that residue — in f32 that leaves a few
/// percent of noise per section, and a steep cut cascades four sections, so
/// the drawn curve wobbled by whole pixels. In f64 the cancellation keeps
/// twelve clean digits and the curve is exact to far below a pixel.
pub struct ResponseGrid {
    freqs: Vec<f32>,
    cos_w: Vec<f64>,
    sin_w: Vec<f64>,
    cos_2w: Vec<f64>,
    sin_2w: Vec<f64>,
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
            let w = 2.0 * std::f64::consts::PI * self.freqs[i] as f64 / sample_rate as f64;
            let (s, c) = w.sin_cos();
            let (s2, c2) = (2.0 * w).sin_cos();
            self.sin_w[i] = s;
            self.cos_w[i] = c;
            self.sin_2w[i] = s2;
            self.cos_2w[i] = c2;
        }
    }

    /// Re-grid to `segments` across the axis — how the display keeps one
    /// evaluation per physical pixel column whatever the window size and DPI.
    ///
    /// A no-op at the resolution it already has; a rebuild otherwise, which
    /// only happens while the window is actually resizing.
    pub fn set_resolution(&mut self, segments: usize) {
        let segments = segments.max(16);
        let n = segments + 1;
        if self.freqs.len() == n {
            return;
        }
        self.freqs = (0..n)
            .map(|i| F_MIN * (F_MAX / F_MIN).powf(i as f32 / segments as f32))
            .collect();
        self.cos_w = vec![0.0; n];
        self.sin_w = vec![0.0; n];
        self.cos_2w = vec![0.0; n];
        self.sin_2w = vec![0.0; n];
        let rate = self.sample_rate;
        self.sample_rate = 0.0;
        self.set_sample_rate(rate);
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
    pub fn magnitude(&self, c: &Coeffs, i: usize) -> f64 {
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
        let mut mag = 1.0f64;
        for section in sections {
            mag *= self.magnitude(section, i);
        }
        (20.0 * mag.max(1e-9).log10()) as f32
    }

    /// The whole cascade, written into `out`.
    pub fn curve(&self, sections: &[Coeffs], out: &mut [f32]) {
        for (i, slot) in out.iter_mut().enumerate().take(self.len()) {
            *slot = self.db_at(sections, i);
        }
    }
}

/// Expand one band into the sections that realise it.
///
/// Not a second implementation: it calls the same [`band_cascade`] the audio
/// path does, which is the only way the drawn curve and the heard curve stay
/// the same curve.
pub fn band_sections(band: &BandView, sample_rate: f32, out: &mut Vec<Coeffs>) {
    out.clear();
    let f = band.freq.clamp(10.0, sample_rate / 2.0 - 1.0);
    let mut sections = [Coeffs::identity(); MAX_SECTIONS];
    let n = band_cascade(
        band.kind,
        f,
        sample_rate,
        band.slope.order(),
        band.q,
        band.gain,
        &mut sections,
    );
    out.extend_from_slice(&sections[..n]);
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
    use crate::dsp::biquad::FLAT_Q;
    use crate::params::{BandChannel, BandKind, DynMode, Slope};

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
            res_mode: crate::params::BandResMode::Adaptive,
            res_range: 36.0,
            res_sens: 0.0,
            res_width: 1.0,
            res_attack: 5.0,
            res_release: 40.0,
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
            let direct = c.magnitude(grid.freq(i), SR) as f64;
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
        for slope in [Slope::S6, Slope::S12, Slope::S24, Slope::S36, Slope::S48] {
            let mut band = bell(100.0, 0.0, FLAT_Q);
            band.kind = BandKind::LowCut;
            band.slope = slope;
            band_sections(&band, SR, &mut sections);
            assert_eq!(sections.len(), slope.order().div_ceil(2));

            let at_corner = grid.db_at(&sections, point_for(100.0));
            assert!(
                (at_corner + 3.0).abs() < 0.35,
                "{:?} read {at_corner} dB at the corner",
                slope
            );
            // And two octaves below, roughly the slope it advertises.
            // One octave is still inside the knee for the shallow orders.
            let below = grid.db_at(&sections, point_for(25.0));
            let expected = -2.0 * slope.db_per_oct() as f32;
            assert!(
                (below - expected).abs() < expected.abs() * 0.15,
                "{:?} read {below} dB two octaves down, expected near {expected}",
                slope
            );
        }
    }

    #[test]
    fn a_cut_with_a_high_q_lifts_its_own_corner() {
        let grid = ResponseGrid::new(SR);
        let mut sections = Vec::new();
        let mut band = bell(1000.0, 0.0, 4.0);
        band.kind = BandKind::HighCut;
        band.slope = Slope::S24;
        band_sections(&band, SR, &mut sections);
        let at_corner = grid.db_at(&sections, point_for(1000.0));
        assert!(
            (at_corner - 20.0 * 4.0f32.log10()).abs() < 0.2,
            "read {at_corner} dB at the corner, wanted {}",
            20.0 * 4.0f32.log10()
        );
    }

    #[test]
    fn a_shelf_takes_its_slope_from_the_same_control() {
        let grid = ResponseGrid::new(SR);
        let mut sections = Vec::new();
        let mut previous = 0usize;
        for slope in [Slope::S12, Slope::S24, Slope::S36, Slope::S48] {
            let mut band = bell(1000.0, 12.0, FLAT_Q);
            band.kind = BandKind::LowShelf;
            band.slope = slope;
            band_sections(&band, SR, &mut sections);
            assert_eq!(sections.len(), slope.order() / 2);
            assert!(sections.len() > previous);
            previous = sections.len();

            let corner = grid.db_at(&sections, point_for(1000.0));
            assert!((corner - 6.0).abs() < 0.1, "{slope:?} read {corner} dB");
            let plateau = grid.db_at(&sections, point_for(30.0));
            assert!((plateau - 12.0).abs() < 0.2, "{slope:?} settled at {plateau} dB");
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
