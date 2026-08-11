//! Turning the analyser's log-spaced curve into pixel columns.
//!
//! Ported from `dsp/spectrum.ts`. The analyser hands over
//! [`crate::analyzer::LOG_POINTS`] points; a wide display has more columns than
//! that, so the value between two points has to be interpolated or the low end
//! draws a staircase. Above the analyser's resolution the opposite is true and
//! the reduction has already happened, in `log_reduce`.

/// Per-column scratch, so the draw loop allocates nothing per frame.
#[derive(Default)]
pub struct Scratch {
    raw: Vec<f32>,
    tmp: Vec<f32>,
    smooth: Vec<f32>,
    /// Peak-hold trace, in dB. Only the pre-EQ layer keeps one.
    peaks: Vec<f32>,
}

impl Scratch {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure(&mut self, columns: usize) {
        if self.raw.len() == columns {
            return;
        }
        self.raw = vec![0.0; columns];
        self.tmp = vec![0.0; columns];
        self.smooth = vec![0.0; columns];
        self.peaks = vec![f32::NEG_INFINITY; columns];
    }

    pub fn peaks(&self) -> &[f32] {
        &self.peaks
    }
}

fn clamp_index(i: isize, n: usize) -> usize {
    if i < 0 {
        0
    } else if i as usize >= n {
        n - 1
    } else {
        i as usize
    }
}

/// Catmull-Rom sample of the curve at a fractional index, clamped to the two
/// points it sits between so the spline cannot ring past them.
///
/// This is what removes the low-end staircase: near 20 Hz a whole run of pixel
/// columns maps inside a single analyser point, and reading that point per
/// column draws a step where the data implies a slope.
pub fn sample(points: &[f32], pos: f32, floor_db: f32) -> f32 {
    if points.is_empty() {
        return floor_db;
    }
    let n = points.len();
    let i = pos.floor() as isize;
    let t = pos - i as f32;
    let at = |k: isize| {
        let v = points[clamp_index(k, n)];
        if v.is_finite() {
            v.max(floor_db)
        } else {
            floor_db
        }
    };
    let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));

    let v = 0.5
        * (2.0 * p1
            + (-p0 + p2) * t
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t);

    v.clamp(p1.min(p2), p1.max(p2))
}

/// O(n) box blur via a running sum, with clamped edges.
fn box_blur(src: &[f32], dst: &mut [f32], radius: usize) {
    let n = src.len();
    if radius == 0 || n == 0 {
        dst[..n].copy_from_slice(src);
        return;
    }
    let window = (2 * radius + 1) as f32;
    let r = radius as isize;
    let mut sum = 0.0;
    for i in -r..=r {
        sum += src[clamp_index(i, n)];
    }
    for i in 0..n {
        dst[i] = sum / window;
        sum -= src[clamp_index(i as isize - r, n)];
        sum += src[clamp_index(i as isize + r + 1, n)];
    }
}

/// Pink-noise tilt so a full mix reads roughly flat across the display.
pub const TILT_DB_PER_OCT: f32 = 4.5;

/// Resample one analyser curve onto `columns` pixel columns, apply the tilt, and
/// smooth it by a fraction of an octave.
///
/// `freq_at` maps a fractional column index to the frequency at that point on
/// the axis — the display owns the axis, not this module.
///
/// Columns are log-spaced by construction, so a fixed-width kernel in pixels
/// *is* constant-Q in frequency; two box passes approximate a Gaussian. The
/// smoothing happens in dB, before anything is mapped to pixels, which is what
/// keeps it level-correct.
///
/// Returns the smoothed dB per column.
#[allow(clippy::too_many_arguments)]
pub fn resample<'a>(
    scratch: &'a mut Scratch,
    points: &[f32],
    columns: usize,
    freq_at: impl Fn(f32) -> f32,
    f_min: f32,
    f_max: f32,
    floor_db: f32,
    octave_fraction: f32,
    hold_peaks: bool,
) -> &'a [f32] {
    let columns = columns.max(1);
    scratch.ensure(columns);
    let Scratch {
        raw,
        tmp,
        smooth,
        peaks,
    } = scratch;

    let n = points.len();
    if n < 2 {
        raw.fill(floor_db);
        return &raw[..columns];
    }

    let span = (f_max / f_min).ln();
    for (i, slot) in raw.iter_mut().enumerate().take(columns) {
        // The geometric mean of the column's two edges: on a log axis that is
        // its centre.
        let f_mid = (freq_at(i as f32) * freq_at(i as f32 + 1.0)).sqrt();
        let pos = (((f_mid / f_min).ln() / span) * (n - 1) as f32).clamp(0.0, (n - 1) as f32);
        let db = sample(points, pos, floor_db);

        // Ramp the tilt in over the first few dB above the noise floor. Tilting
        // silence itself would lift the top end by some twenty dB and draw a
        // rising diagonal across an empty display instead of a flat floor.
        let fade = ((db - floor_db) / 6.0).clamp(0.0, 1.0);
        *slot = db + fade * TILT_DB_PER_OCT * (f_mid / 1000.0).log2();
    }

    let px_per_octave = columns as f32 / (f_max / f_min).log2();
    let radius = if octave_fraction > 0.0 {
        ((px_per_octave * octave_fraction) / 2.0).round().max(1.0) as usize
    } else {
        0
    };

    if radius > 0 {
        box_blur(&raw[..columns], &mut tmp[..columns], radius);
        box_blur(&tmp[..columns], &mut smooth[..columns], radius);
    } else {
        smooth[..columns].copy_from_slice(&raw[..columns]);
    }

    if hold_peaks {
        // A peak is held at its dB and decays downward — the mirror of the old
        // canvas version, which held a minimum in pixels because smaller y meant
        // louder there.
        for (peak, &value) in peaks.iter_mut().zip(smooth.iter()).take(columns) {
            let decayed = *peak - 0.35;
            *peak = if value > decayed { value } else { decayed };
        }
    }

    &smooth[..columns]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_between_two_points_stays_between_them() {
        let points = [-60.0, -20.0, -50.0, -10.0];
        for k in 0..=10 {
            let t = k as f32 / 10.0;
            let v = sample(&points, 1.0 + t, -110.0);
            assert!(
                (-50.0 - 1e-4..=-20.0 + 1e-4).contains(&v),
                "t={t} sampled {v}, outside its two neighbours"
            );
        }
    }

    #[test]
    fn sampling_on_a_point_returns_it() {
        let points = [-60.0, -20.0, -50.0, -10.0];
        for (i, expected) in points.iter().enumerate() {
            let v = sample(&points, i as f32, -110.0);
            assert!((v - expected).abs() < 1e-4, "point {i} sampled {v}");
        }
    }

    #[test]
    fn a_non_finite_point_reads_as_the_floor() {
        let points = [f32::NAN, f32::NEG_INFINITY, -30.0];
        assert_eq!(sample(&points, 0.0, -110.0), -110.0);
    }

    #[test]
    fn an_empty_curve_is_not_an_overrun() {
        assert_eq!(sample(&[], 3.0, -110.0), -110.0);
    }

    #[test]
    fn a_box_blur_preserves_a_constant() {
        let src = vec![-42.0f32; 64];
        let mut dst = vec![0.0f32; 64];
        box_blur(&src, &mut dst, 5);
        assert!(dst.iter().all(|v| (v + 42.0).abs() < 1e-3));
    }

    #[test]
    fn a_box_blur_of_zero_radius_is_a_copy() {
        let src: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let mut dst = vec![0.0f32; 16];
        box_blur(&src, &mut dst, 0);
        assert_eq!(dst, src);
    }

    #[test]
    fn smoothing_flattens_a_lone_spike_without_moving_the_floor() {
        let mut points = vec![-100.0f32; 128];
        points[64] = -10.0;
        let mut scratch = Scratch::new();

        let f_min = 20.0f32;
        let f_max = 20_000.0f32;
        let columns = 200;
        let freq_at = |x: f32| f_min * (f_max / f_min).powf(x / columns as f32);

        let raw = resample(
            &mut scratch, &points, columns, freq_at, f_min, f_max, -110.0, 0.0, false,
        )
        .to_vec();
        let smoothed = resample(
            &mut scratch, &points, columns, freq_at, f_min, f_max, -110.0, 1.0 / 3.0, false,
        )
        .to_vec();

        let peak_raw = raw.iter().cloned().fold(f32::MIN, f32::max);
        let peak_smooth = smoothed.iter().cloned().fold(f32::MIN, f32::max);
        assert!(
            peak_smooth < peak_raw - 10.0,
            "raw peaked at {peak_raw}, smoothed at {peak_smooth}"
        );
        assert_eq!(raw.len(), columns);
    }

    #[test]
    fn peaks_hold_then_decay() {
        let mut scratch = Scratch::new();
        let loud = vec![-10.0f32; 32];
        let quiet = vec![-90.0f32; 32];
        let columns = 40;
        let freq_at = |x: f32| 20.0f32 * 1000f32.powf(x / columns as f32);

        resample(
            &mut scratch, &loud, columns, freq_at, 20.0, 20_000.0, -110.0, 0.0, true,
        );
        let held = scratch.peaks()[10];

        for _ in 0..3 {
            resample(
                &mut scratch, &quiet, columns, freq_at, 20.0, 20_000.0, -110.0, 0.0, true,
            );
        }
        let after = scratch.peaks()[10];
        assert!(after < held, "peak did not decay: {held} then {after}");
        assert!(
            after > -80.0,
            "peak fell straight to the signal instead of decaying: {after}"
        );
    }
}
