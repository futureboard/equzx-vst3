use super::*;
use crate::params::BandKind;

const SR: f32 = 48_000.0;
/// Every sample rate the plugin is likely to meet, plus the extremes.
const RATES: [f32; 6] = [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0];
/// The pole counts the slope control can ask a cut for.
const CUT_POLES: [usize; 5] = [1, 2, 4, 6, 8];
/// And a shelf.
const SHELF_POLES: [usize; 4] = [2, 4, 6, 8];

fn db(mag: f32) -> f32 {
    20.0 * mag.max(1e-9).log10()
}

/// `|H(e^jw)|` of one section, in f64. Same arithmetic as
/// [`Coeffs::magnitude`], but that one rounds its answer to f32 and leaves
/// hundredths of a dB per section — more than the tolerances here can spare.
fn magnitude_f64(c: &Coeffs, f: f32, sr: f32) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f as f64 / sr as f64;
    let (sw, cw) = w.sin_cos();
    let (s2w, c2w) = (2.0 * w).sin_cos();
    let num = (c.b0 + c.b1 * cw + c.b2 * c2w).hypot(c.b1 * sw + c.b2 * s2w);
    let den = (1.0 + c.a1 * cw + c.a2 * c2w).hypot(c.a1 * sw + c.a2 * s2w);
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}

/// Design a cascade and return its magnitude response at `f`, in dB.
fn response_db(shape: Shape, freq: f32, sr: f32, poles: usize, q: f32, gain: f32, f: f32) -> f32 {
    let mut sections = [Coeffs::identity(); MAX_SECTIONS];
    let n = design(shape, freq, sr, poles, q, gain, &mut sections);
    assert!(n > 0, "{shape:?} at {freq} Hz / {sr} / {poles}p / Q {q} produced nothing");
    let magnitude = sections[..n]
        .iter()
        .fold(1.0f64, |acc, c| acc * magnitude_f64(c, f, sr));
    (20.0 * magnitude.max(1e-12).log10()) as f32
}

// --- the property the whole design exists for --------------------------------

#[test]
fn the_whole_curve_is_the_same_shape_at_every_sample_rate() {
    // The reason for matching rather than bilinear-transforming: no warping.
    //
    // Only the top ten dB — passband and knee. The poles are mapped exactly and
    // are rate-invariant outright; the zeros are matched at Nyquist, which is
    // not in the same place, so further down a skirt the rates part company.
    for shape in [Shape::LowPass, Shape::HighPass, Shape::LowShelf, Shape::Bell] {
        for &poles in &[2usize, 8] {
            for &q in &[FLAT_Q, 3.0] {
                for step in 0..24 {
                    let f = 30.0 * (10_000.0f32 / 30.0).powf(step as f32 / 23.0);
                    let reference = response_db(shape, 1_000.0, 44_100.0, poles, q, 9.0, f);
                    if reference < -10.0 {
                        continue;
                    }
                    for &sr in &RATES[1..] {
                        let got = response_db(shape, 1_000.0, sr, poles, q, 9.0, f);
                        assert!(
                            (got - reference).abs() < 0.05,
                            "{shape:?} {poles}p Q {q} at {f} Hz read {got} dB on {sr} \
                             against {reference} dB on 44100"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn a_cut_is_three_db_down_at_the_corner_it_was_asked_for() {
    // The low-pass numerator is solved for this, so it holds everywhere. The
    // high-pass is pinned at DC and Nyquist instead — see the drift test below.
    for &poles in &CUT_POLES {
        for &sr in &RATES {
            for &fraction in &[1.0f32 / 512.0, 1.0 / 64.0, 1.0 / 16.0, 0.4, 0.499] {
                let freq = sr * fraction;
                let low = response_db(Shape::LowPass, freq, sr, poles, FLAT_Q, 0.0, freq);
                assert!(
                    (low + 3.0103).abs() < 0.01,
                    "LowPass {poles}p at {freq} Hz / {sr} read {low} dB"
                );

                // A high-pass corner within a sixteenth of the rate is clear
                // enough for every order; a lone real pole wants far more room.
                let headroom = if poles == 1 { 1.0 / 100.0 } else { 1.0 / 16.0 };
                if fraction <= headroom {
                    let high = response_db(Shape::HighPass, freq, sr, poles, FLAT_Q, 0.0, freq);
                    assert!(
                        (high + 3.0103).abs() < 0.03,
                        "HighPass {poles}p at {freq} Hz / {sr} read {high} dB"
                    );
                }
            }
        }
    }
}

#[test]
fn a_high_pass_corner_softens_once_it_is_up_against_nyquist() {
    // Written down rather than tested around: zeros pinned at DC and scale set
    // at Nyquist leaves less and less room for a -3 dB point as the corner
    // climbs. The steeper the cascade the further it holds; a lone real pole
    // gives up first and ends at unity.
    let sr = 48_000.0f32;

    let mut previous = response_db(Shape::HighPass, 50.0, sr, 1, FLAT_Q, 0.0, 50.0);
    assert!((previous + 3.0103).abs() < 0.001, "read {previous} dB at 50 Hz");
    for &freq in &[1_000.0f32, 5_000.0, 12_000.0, 23_000.0] {
        let got = response_db(Shape::HighPass, freq, sr, 1, FLAT_Q, 0.0, freq);
        assert!(got > previous, "{freq} Hz read {got} dB, {previous} before it");
        previous = got;
    }
    assert!(previous > -0.2, "a one-pole corner on Nyquist read {previous} dB");

    // At any given corner, a steeper cascade is closer to -3 dB than a
    // shallower one, and none of them ever reads *below* it.
    let mut previous = f32::INFINITY;
    for &poles in &CUT_POLES {
        let got = response_db(Shape::HighPass, 16_000.0, sr, poles, FLAT_Q, 0.0, 16_000.0);
        assert!((-3.02..0.0).contains(&got), "{poles}p read {got} dB");
        assert!(got < previous, "{poles}p read {got} dB, {previous} before it");
        previous = got;
    }
}

#[test]
fn a_cut_reads_its_own_q_at_the_corner() {
    // Q is a resonance control, not a per-section quality: the whole cascade
    // lands on the requested value, not on it once per biquad.
    for shape in [Shape::LowPass, Shape::HighPass] {
        for &poles in &[2usize, 4, 6, 8] {
            for &q in &[0.3f32, FLAT_Q, 1.0, 4.0, 16.0] {
                let at_corner = response_db(shape, 1_000.0, SR, poles, q, 0.0, 1_000.0);
                assert!(
                    (at_corner - db(q)).abs() < 0.06,
                    "{shape:?} {poles}p Q {q} read {at_corner} dB, wanted {}",
                    db(q)
                );
            }
        }
    }
}

#[test]
fn flat_q_is_the_butterworth_one() {
    // The claim FLAT_Q makes: at that value and no other, a cut is -3 dB at its
    // corner and has no bump anywhere.
    for &poles in &[2usize, 4, 8] {
        let mut peak = f32::NEG_INFINITY;
        for step in 0..400 {
            let f = 20.0 * (22_000.0f32 / 20.0).powf(step as f32 / 399.0);
            peak = peak.max(response_db(Shape::LowPass, 1_000.0, SR, poles, FLAT_Q, 0.0, f));
        }
        assert!(peak < 0.05, "{poles}p Butterworth low-pass peaked at {peak} dB");
    }
    // And above it, it does bump.
    let peak_q2 = (0..400)
        .map(|step| {
            let f = 20.0 * (22_000.0f32 / 20.0).powf(step as f32 / 399.0);
            response_db(Shape::LowPass, 1_000.0, SR, 4, 2.0, 0.0, f)
        })
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(peak_q2 > 3.0, "a Q of 2 should resonate, peaked at {peak_q2} dB");
}

#[test]
fn a_cut_falls_off_at_the_slope_it_advertises() {
    for (poles, db_per_oct) in [(1usize, 6.0f32), (2, 12.0), (4, 24.0), (6, 36.0), (8, 48.0)] {
        // Two octaves clear of the corner, where the asymptote has taken over.
        let low = response_db(Shape::HighPass, 1_000.0, SR, poles, FLAT_Q, 0.0, 250.0);
        let high = response_db(Shape::LowPass, 1_000.0, SR, poles, FLAT_Q, 0.0, 4_000.0);
        for (side, got) in [("high-pass", low), ("low-pass", high)] {
            assert!(
                (got + 2.0 * db_per_oct).abs() < db_per_oct * 0.12,
                "{poles}p {side} read {got} dB two octaves out, expected near {}",
                -2.0 * db_per_oct
            );
        }
    }
}

// --- shelves -----------------------------------------------------------------

#[test]
fn a_shelf_reaches_its_gain_and_crosses_half_way_at_its_corner() {
    for shape in [Shape::LowShelf, Shape::HighShelf] {
        for &poles in &SHELF_POLES {
            for &gain in &[-30.0f32, -12.0, -3.0, 3.0, 12.0, 30.0] {
                // The root find lands on half gain to well inside a thousandth
                // of a dB; this is only as loose as it is because the finished
                // coefficients are narrowed to f32 before being measured.
                let corner = response_db(shape, 1_000.0, SR, poles, FLAT_Q, gain, 1_000.0);
                assert!(
                    (corner - gain * 0.5).abs() < 0.002,
                    "{shape:?} {poles}p {gain} dB read {corner} dB at its corner"
                );

                let (shelf_side, flat_side) = match shape {
                    Shape::LowShelf => (25.0f32, 20_000.0f32),
                    _ => (20_000.0, 25.0),
                };
                let plateau = response_db(shape, 1_000.0, SR, poles, FLAT_Q, gain, shelf_side);
                assert!(
                    (plateau - gain).abs() < 0.2,
                    "{shape:?} {poles}p {gain} dB settled at {plateau} dB"
                );
                let flat = response_db(shape, 1_000.0, SR, poles, FLAT_Q, gain, flat_side);
                assert!(
                    flat.abs() < 0.2,
                    "{shape:?} {poles}p {gain} dB left its far side at {flat} dB"
                );
            }
        }
    }
}

#[test]
fn a_steeper_shelf_turns_faster() {
    // Half an octave below a +12 dB low shelf, a steeper one is further along.
    let mut previous = f32::NEG_INFINITY;
    for &poles in &SHELF_POLES {
        let got = response_db(Shape::LowShelf, 1_000.0, SR, poles, FLAT_Q, 12.0, 707.0);
        assert!(got > previous, "{poles}p read {got} dB, {previous} before it");
        previous = got;
    }
}

#[test]
fn a_shelf_of_no_gain_is_a_wire() {
    for shape in [Shape::LowShelf, Shape::HighShelf] {
        for &poles in &SHELF_POLES {
            for &f in &[30.0f32, 1_000.0, 18_000.0] {
                let got = response_db(shape, 1_000.0, SR, poles, FLAT_Q, 0.0, f);
                assert!(got.abs() < 1e-4, "{shape:?} {poles}p read {got} dB at {f} Hz");
            }
        }
    }
}

// --- the single-section responses --------------------------------------------

#[test]
fn a_bell_hits_its_gain_at_the_centre_and_nothing_far_away() {
    for &gain in &[-30.0f32, -12.0, -1.0, 1.0, 12.0, 30.0] {
        for &q in &[0.2f32, 1.0, 8.0] {
            let centre = response_db(Shape::Bell, 1_000.0, SR, 2, q, gain, 1_000.0);
            assert!(
                (centre - gain).abs() < 0.02,
                "Q {q} {gain} dB bell read {centre} dB at its centre"
            );
        }
        // Six octaves away a Q of 4 has nothing left to say.
        let far = response_db(Shape::Bell, 1_000.0, SR, 2, 4.0, gain, 20.0);
        assert!(far.abs() < 0.3, "bell read {far} dB six octaves down");
    }
}

#[test]
fn a_bell_cut_undoes_a_bell_boost_of_the_same_size() {
    // The reciprocal construction: cascade the two and the result is a wire.
    let mut boost = [Coeffs::identity(); MAX_SECTIONS];
    let mut cut = [Coeffs::identity(); MAX_SECTIONS];
    let nb = design(Shape::Bell, 900.0, SR, 2, 3.0, 9.0, &mut boost);
    let nc = design(Shape::Bell, 900.0, SR, 2, 3.0, -9.0, &mut cut);
    for step in 0..200 {
        let f = 20.0 * (22_000.0f32 / 20.0).powf(step as f32 / 199.0);
        let combined = boost[..nb].iter().chain(cut[..nc].iter());
        let total = db(combined.fold(1.0f32, |acc, c| acc * c.magnitude(f, SR)));
        assert!(total.abs() < 0.01, "{total} dB left at {f} Hz");
    }
}

#[test]
fn a_notch_is_a_null_and_a_bandpass_is_unity() {
    for &q in &[0.5f32, 2.0, 20.0] {
        let null = response_db(Shape::Notch, 1_000.0, SR, 2, q, 0.0, 1_000.0);
        assert!(null < -80.0, "notch read {null} dB at its centre");
        let peak = response_db(Shape::BandPass, 1_000.0, SR, 2, q, 0.0, 1_000.0);
        assert!(peak.abs() < 0.02, "band-pass read {peak} dB at its centre");
    }
}

// --- robustness --------------------------------------------------------------

#[test]
fn every_design_across_the_whole_parameter_space_is_stable_and_finite() {
    // Every rate, walked densely enough to catch the ill-conditioned corners:
    // 20 Hz at 192 kHz is 6e-4 normalised, where the solves are residues of
    // terms a million times their own size.
    let mut designed = 0;
    for &sr in &RATES {
        let top = (22_000.0f32).min(sr / 2.0 - 1.0);
        for step in 0..=24 {
            let freq = 20.0 * (top / 20.0).powf(step as f32 / 24.0);
            for qstep in 0..=16 {
                let q = 0.025 * (40.0f32 / 0.025).powf(qstep as f32 / 16.0);
                for (shape, poles) in [
                    (Shape::LowPass, &CUT_POLES[..]),
                    (Shape::HighPass, &CUT_POLES[..]),
                    (Shape::Bell, &[2][..]),
                    (Shape::Notch, &[2][..]),
                    (Shape::BandPass, &[2][..]),
                    (Shape::LowShelf, &SHELF_POLES[..]),
                    (Shape::HighShelf, &SHELF_POLES[..]),
                ] {
                    for &p in poles {
                        for &gain in &[-30.0f32, 0.0, 30.0] {
                            let mut sections = [Coeffs::identity(); MAX_SECTIONS];
                            let n = design(shape, freq, sr, p, q, gain, &mut sections);
                            assert!(
                                n > 0,
                                "{shape:?} {p}p gave up at {freq} Hz / {sr} / Q {q} / {gain} dB"
                            );
                            for c in &sections[..n] {
                                assert!(c.finite() && c.stable(), "{shape:?} {c:?}");
                            }
                            designed += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(designed > 20_000, "only walked {designed} designs");
}

#[test]
fn a_degenerate_request_designs_nothing_rather_than_something_wrong() {
    let mut sections = [Coeffs::identity(); MAX_SECTIONS];
    for (freq, sr) in [(0.0f32, SR), (SR / 2.0, SR), (SR, SR), (-100.0, SR), (1_000.0, 0.0)] {
        assert_eq!(
            design(Shape::LowPass, freq, sr, 4, FLAT_Q, 0.0, &mut sections),
            0,
            "{freq} Hz at {sr} designed something"
        );
    }
    assert_eq!(
        design(Shape::Bell, f32::NAN, SR, 2, 1.0, 0.0, &mut sections),
        0
    );
    assert_eq!(
        design(Shape::Bell, 1_000.0, SR, 2, 1.0, f32::INFINITY, &mut sections),
        0
    );
}

// --- the shapes a band maps to -----------------------------------------------

#[test]
fn a_low_cut_is_a_high_pass() {
    assert_eq!(Shape::from(BandKind::LowCut), Shape::HighPass);
    assert_eq!(Shape::from(BandKind::HighCut), Shape::LowPass);
    assert_eq!(Shape::from(BandKind::LowShelf), Shape::LowShelf);
}

#[test]
fn a_shape_rounds_a_pole_count_it_cannot_build() {
    // Shelves have no one-pole form; the single-section types ignore it entirely.
    assert_eq!(Shape::LowShelf.poles(1), 2);
    assert_eq!(Shape::HighShelf.poles(8), 8);
    assert_eq!(Shape::Bell.poles(8), 2);
    assert_eq!(Shape::LowPass.poles(1), 1);
    assert_eq!(Shape::LowPass.poles(8), 8);
}

#[test]
fn a_one_pole_cut_has_no_q_to_speak_of() {
    assert!(!Shape::LowPass.uses_q(1));
    assert!(Shape::LowPass.uses_q(2));
    assert!(!Shape::LowShelf.uses_q(4));
    assert!(Shape::Bell.uses_q(2));

    // And proves it: the response does not move with Q.
    let a = response_db(Shape::LowPass, 1_000.0, SR, 1, 0.1, 0.0, 1_000.0);
    let b = response_db(Shape::LowPass, 1_000.0, SR, 1, 30.0, 0.0, 1_000.0);
    assert!((a - b).abs() < 1e-5, "{a} vs {b}");
}

#[test]
fn a_cascade_never_needs_more_sections_than_there_is_room_for() {
    let mut sections = [Coeffs::identity(); MAX_SECTIONS];
    assert_eq!(
        design(Shape::LowPass, 1_000.0, SR, 8, FLAT_Q, 0.0, &mut sections),
        MAX_SECTIONS
    );
    // An odd order spends one section on its real pole and pairs up the rest.
    assert_eq!(
        design(Shape::LowPass, 1_000.0, SR, 1, FLAT_Q, 0.0, &mut sections),
        1
    );
}

// --- the plumbing filters and the section itself -----------------------------

#[test]
fn the_cookbook_helpers_still_do_what_the_detectors_expect() {
    let peaking = Coeffs::peaking(1_000.0, 1.0, 6.0, SR);
    assert!((db(peaking.magnitude(1_000.0, SR)) - 6.0).abs() < 0.05);
    assert!(db(peaking.magnitude(30.0, SR)).abs() < 0.2);

    let band = Coeffs::bandpass(1_000.0, 1.0, SR);
    assert!(db(band.magnitude(1_000.0, SR)).abs() < 0.05);
    assert!(db(band.magnitude(100.0, SR)) < -15.0);

    for c in [Coeffs::lowpass(1_000.0, 1.0, SR), Coeffs::highpass(1_000.0, 1.0, SR)] {
        assert!(c.stable() && c.finite());
    }
}

#[test]
fn identity_section_passes_signal_through() {
    let mut bq = Biquad::new();
    let c = Coeffs::identity();
    for x in [0.3f32, -0.7, 0.0, 1.0] {
        assert!((bq.process(x, &c) - x).abs() < 1e-6);
    }
}

#[test]
fn the_stability_test_agrees_with_the_pole_radii() {
    // Jury's test, checked against actually solving for the roots. The cases
    // are chosen clear of the unit circle either way, since a test that
    // disagreed only on the boundary would be testing float rounding.
    for &(a1, a2, want) in &[
        (0.0f64, 0.0, true),
        (-1.9, 0.95, true),
        (0.5, 0.1, true),
        (-1.0, 0.5, true),
        (-1.99908, 0.99908, true),
        (0.5, -0.9, false),
        (-2.1, 1.1, false),
        (0.0, 1.2, false),
        (-3.0, 0.5, false),
        (0.0, -1.5, false),
    ] {
        let c = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1, a2 };
        let discriminant = a1 * a1 - 4.0 * a2;
        let radius = if discriminant >= 0.0 {
            let root = discriminant.sqrt();
            (0.5 * (-a1 + root)).abs().max((0.5 * (-a1 - root)).abs())
        } else {
            a2.abs().sqrt()
        };
        assert_eq!(radius < 1.0, want, "a1 {a1} a2 {a2} has radius {radius}");
        assert_eq!(c.stable(), want, "a1 {a1} a2 {a2} radius {radius}");
    }
}

// --- the low end -------------------------------------------------------------
//
// Two bugs lived down here: a bell changed shape as it crossed 76 Hz, and cuts
// and shelves stopped tracking their own frequency below about 100 Hz.

/// Count how often the response at `probe` reverses direction as the corner is
/// swept, ignoring steps below [`SMOOTH_FLOOR`].
///
/// A corner moving one way moves the whole curve one way; a reversal means the
/// design has stopped resolving the frequency it was handed. The original bug
/// reversed by thousandths of a dB; what is left is f64 rounding on a zero.
const SMOOTH_FLOOR: f32 = 1.0e-6;

fn direction_reversals(shape: Shape, poles: usize, gain: f32, sr: f32, probe: f32) -> usize {
    let mut reversals = 0;
    let (mut previous, mut last_delta) = (f32::NAN, 0.0f32);
    for step in 0..=400 {
        let corner = 20.0 * (200.0f32 / 20.0).powf(step as f32 / 400.0);
        let got = response_db(shape, corner, sr, poles, FLAT_Q, gain, probe);
        if previous.is_finite() {
            let delta = got - previous;
            if delta.abs() > SMOOTH_FLOOR {
                if delta * last_delta < 0.0 {
                    reversals += 1;
                }
                last_delta = delta;
            }
        }
        previous = got;
    }
    reversals
}

#[test]
fn a_corner_sweeping_the_bottom_two_octaves_moves_the_curve_smoothly() {
    // Cuts and shelves only: a bell or band-pass carries its peak *past* the
    // probe and legitimately reverses once. Those are covered by
    // `a_peaked_band_keeps_its_shape_all_the_way_down`.
    for &sr in &[48_000.0f32, 192_000.0] {
        for (shape, poles, gain) in [
            (Shape::HighPass, 1usize, 0.0f32),
            (Shape::HighPass, 2, 0.0),
            (Shape::HighPass, 8, 0.0),
            (Shape::LowPass, 2, 0.0),
            (Shape::LowPass, 8, 0.0),
            (Shape::LowShelf, 2, 12.0),
            (Shape::LowShelf, 8, -12.0),
            (Shape::HighShelf, 4, 12.0),
            (Shape::HighShelf, 8, -30.0),
        ] {
            let reversals = direction_reversals(shape, poles, gain, sr, 100.0);
            assert_eq!(
                reversals, 0,
                "{shape:?} {poles}p {gain} dB at {sr} reversed direction {reversals} times \
                 while its corner swept 20..200 Hz"
            );
        }
    }
}

#[test]
fn a_peaked_band_keeps_its_shape_all_the_way_down() {
    // Far from Nyquist a bell's skirts depend on Q and gain alone, so its shape
    // relative to its own centre must not depend on where the centre is. It used
    // to: at 76 Hz — one hundredth of a radian — the design swapped itself for
    // the RBJ bell and stepped by a dB.
    for (shape, gains) in [
        (Shape::Bell, &[-12.0f32, -3.0, 3.0, 12.0][..]),
        (Shape::BandPass, &[0.0][..]),
        (Shape::Notch, &[0.0][..]),
    ] {
        for &q in &[0.5f32, 2.0, 8.0] {
            for &gain in gains {
                let reference = response_db(shape, 400.0, 48_000.0, 2, q, gain, 800.0);
                for step in 0..=60 {
                    let centre = 20.0 * (400.0f32 / 20.0).powf(step as f32 / 60.0);
                    let up = response_db(shape, centre, 48_000.0, 2, q, gain, centre * 2.0);
                    assert!(
                        (up - reference).abs() < 0.02,
                        "{shape:?} Q {q} {gain} dB at {centre} Hz read {up} dB an octave up, \
                         against {reference} dB for the same band at 400 Hz"
                    );
                }
            }
        }
    }
}

#[test]
fn the_dc_residue_survives_being_stored() {
    // `1 + a1 + a2` is what a section does at low frequency, and a residue of
    // order `w0^2`. At 20 Hz on 192 kHz it is 2.4e-7 — one f32 ulp, no
    // significant digits; eleven in f64.
    let mut sections = [Coeffs::identity(); MAX_SECTIONS];
    let n = design(Shape::LowShelf, 20.0, 192_000.0, 4, FLAT_Q, 12.0, &mut sections);
    assert!(n > 0);
    for c in &sections[..n] {
        let residue = 1.0 + c.a1 + c.a2;
        assert!(residue > 0.0, "residue {residue} went non-positive");
        let ulp = c.a1.abs() * f64::EPSILON;
        assert!(
            residue / ulp > 1.0e6,
            "residue {residue:e} is only {} ulps of a1",
            residue / ulp
        );
    }
}
