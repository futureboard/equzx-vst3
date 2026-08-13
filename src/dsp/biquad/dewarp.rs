//! Passband calibration for the matched low-pass cascade.
//!
//! The numerator solve fixes the corner gain whatever analog corner the poles
//! came from, which leaves that corner free to be chosen for passband shape
//! instead. Below a few kHz `compensated_analog_omega` is already the answer;
//! near Nyquist it is not, and using it anyway costs up to thirty dB on a
//! resonant cut.
//!
//! Solving for it properly is a nonlinear fit over a hundred-odd points — a
//! millisecond, and impossible on the audio thread. But it depends only on the
//! *normalised* corner, the pole count and the Q, never on the sample rate, so
//! the surface is solved offline and carried as data. Stored as the ratio to the
//! compensated corner, which is smooth and exactly 1 over the lower half of the
//! domain.
//!
//! Interpolation lands within the optimiser's own residual error except for
//! near-Nyquist cuts at a Q in the tens, where it is under a dB further out. It
//! also makes the lookup pure, so the editor and audio thread cannot disagree.

include!("dewarp_table.rs");

/// Corner ratio for `poles` poles at normalised corner `dw` and quality `q`.
///
/// Multiply `compensated_analog_omega(dw)` by this. Exactly 1 below [`W_LO`] —
/// not an approximation, the converged answer, so the table spends no
/// resolution restating it.
pub(super) fn ratio(poles: usize, dw: f64, q: f64) -> f64 {
    if dw <= W_LO {
        return 1.0;
    }
    let plane = &RATIO[((poles / 2).clamp(1, 4)) - 1];

    // Both axes are geometric: a grid step is an interval, not a fixed span.
    let x = (dw.min(W_HI).ln() - W_LO.ln()) / (W_HI.ln() - W_LO.ln()) * (NW - 1) as f64;
    let y = (q.clamp(Q_LO, Q_HI).ln() - Q_LO.ln()) / (Q_HI.ln() - Q_LO.ln()) * (NQ - 1) as f64;

    let i = (x.floor() as usize).min(NW - 2);
    let j = (y.floor() as usize).min(NQ - 2);
    let tx = (x - i as f64).clamp(0.0, 1.0);
    let ty = (y - j as f64).clamp(0.0, 1.0);

    let (r0, r1) = (&plane[i], &plane[i + 1]);
    let lower = r0[j] as f64 + (r0[j + 1] as f64 - r0[j] as f64) * ty;
    let upper = r1[j] as f64 + (r1[j + 1] as f64 - r1[j] as f64) * ty;
    lower + (upper - lower) * tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_low_end_needs_no_correction() {
        for &poles in &[2usize, 4, 6, 8] {
            for &q in &[0.025f64, 0.707, 5.0, 40.0] {
                assert_eq!(ratio(poles, 1.0e-4, q), 1.0);
                assert_eq!(ratio(poles, W_LO, q), 1.0);
            }
        }
    }

    #[test]
    fn the_correction_grows_toward_nyquist() {
        // A 20 kHz corner at 48 kHz needs a markedly larger analog corner than
        // a 1 kHz one, which needs none at all.
        let low = ratio(4, 2.0 * std::f64::consts::PI * 1_000.0 / 48_000.0, 0.707);
        let high = ratio(4, 2.0 * std::f64::consts::PI * 20_000.0 / 48_000.0, 0.707);
        assert!((low - 1.0).abs() < 0.01, "low was {low}");
        assert!(high > 1.2, "high was {high}");
    }

    #[test]
    fn every_entry_is_a_sane_positive_ratio() {
        for plane in RATIO.iter() {
            for row in plane.iter() {
                for &value in row.iter() {
                    assert!(value.is_finite() && value > 0.1 && value < 10.0, "{value}");
                }
            }
        }
    }

    #[test]
    fn arguments_outside_the_grid_are_clamped_rather_than_wrapped() {
        // Q below and above the table, and a corner sitting on Nyquist.
        for &q in &[1.0e-6f64, 1.0e9] {
            let value = ratio(8, W_HI, q);
            assert!(value.is_finite() && value > 0.0, "{value}");
        }
        assert!(ratio(8, 100.0, 1.0).is_finite());
    }
}
