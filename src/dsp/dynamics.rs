//! Band dynamics — measuring a band's level, and the envelope that moves its gain.
//!
//! Two pieces. [`LevelDetector`] answers "how loud is this band's slice of the
//! signal right now"; [`dynamic_step`] turns that answer into an engagement and
//! a gain offset. The offset is published through [`crate::meters`], which is
//! what lets the display draw a dynamic band where it actually sits rather than
//! predicting it — the UI reads the audio thread's own answer.

use crate::params::{DynMode, DYN_KNEE_DB};

/// A band's dynamics settings, as they stand for one control block.
#[derive(Clone, Copy, Debug)]
pub struct DynSettings {
    pub threshold_db: f32,
    pub mode: DynMode,
    /// Signed dB the band travels at full engagement.
    pub range_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

/// Engagement across the soft knee, 0..1, from how far the level sits past the
/// threshold in the engaging direction.
///
/// Smoothstep rather than a straight ramp. A ramp is continuous but its slope
/// jumps at both corners, so the band goes from perfectly still to travelling at
/// full rate the instant the level touches the threshold — the grabbiness a soft
/// knee exists to remove. Both agree at half a knee in, where engagement is 0.5.
pub fn knee(over_db: f32) -> f32 {
    let t = (over_db / DYN_KNEE_DB).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// One-pole move from `env` toward `target` over `tau_ms`, `dt` seconds on.
///
/// Exact for a constant target, which is what it always has within a control
/// block, so stepping once a block costs nothing in accuracy against stepping
/// once a sample.
pub fn step_toward(env: f32, target: f32, tau_ms: f32, dt: f32) -> f32 {
    let tau = (tau_ms / 1000.0).max(0.001);
    let next = env + (target - env) * (1.0 - (-dt / tau).exp());
    // A tail this far down is orders below anything a gain could show, and
    // letting it trail off into denormals costs more than the value is worth.
    if next.abs() < 1e-6 {
        0.0
    } else {
        next
    }
}

/// One control-rate step.
///
/// Returns the new envelope and the gain offset in dB.
pub fn dynamic_step(s: DynSettings, level_db: f32, env: f32, dt: f32) -> (f32, f32) {
    let over = match s.mode {
        DynMode::Above => level_db - s.threshold_db,
        DynMode::Below => s.threshold_db - level_db,
    };
    let target = knee(over);

    let tau_ms = if target > env {
        s.attack_ms
    } else {
        s.release_ms
    };
    let next = step_toward(env, target, tau_ms, dt);

    (next, next * s.range_db)
}

/// Mean square as dBFS. The floor keeps silence from reaching -inf.
pub fn ms_to_db(mean_square: f32) -> f32 {
    10.0 * mean_square.max(1e-12).log10()
}

/// Integration window for a band's level detector, in ms.
///
/// Tied to the band's own frequency, because a level only means anything
/// measured over a cycle or so of the thing being measured. One control block —
/// 0.67 ms at 48 kHz — is a thirtieth of a cycle at 50 Hz, so a steady tone
/// reads as a level swinging by tens of dB on nothing but the phase the block
/// happened to start on. A fast band then chases that swing and the gain ends up
/// modulating at the signal's own rate, which is distortion, not dynamics.
///
/// Bounded at both ends: short enough up top that transients still register,
/// long enough down the bottom that the reading isn't a guess.
pub fn detector_window_ms(freq: f32) -> f32 {
    (2000.0 / freq.max(1.0)).clamp(1.0, 40.0)
}

/// A one-pole mean-square integrator, sized by [`detector_window_ms`].
#[derive(Clone, Copy, Debug)]
pub struct LevelDetector {
    ms: f32,
    coeff: f32,
    freq: f32,
    sr: f32,
}

impl Default for LevelDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl LevelDetector {
    pub const fn new() -> Self {
        Self {
            ms: 0.0,
            coeff: 1.0,
            freq: 0.0,
            sr: 0.0,
        }
    }

    /// Retune the window. Cheap to call every block: the transcendental only
    /// runs when the band actually moved.
    pub fn set_window(&mut self, freq: f32, sr: f32) {
        if freq == self.freq && sr == self.sr {
            return;
        }
        self.freq = freq;
        self.sr = sr;
        let tau_samples = (detector_window_ms(freq) / 1000.0 * sr).max(1.0);
        self.coeff = (1.0 - (-1.0 / tau_samples).exp()).clamp(0.0, 1.0);
    }

    /// Feed one sample of the band-filtered signal.
    #[inline(always)]
    pub fn push(&mut self, x: f32) {
        self.push_ms(x * x);
    }

    /// Feed one sample's worth of power directly — for a band listening to more
    /// than one channel, where the two have to be averaged before integrating.
    #[inline(always)]
    pub fn push_ms(&mut self, power: f32) {
        self.ms += (power - self.ms) * self.coeff;
    }

    /// The level now, in dBFS.
    pub fn level_db(&mut self) -> f32 {
        // Decaying toward zero walks through the denormal range, where every
        // one of these multiply-adds can cost hundreds of cycles.
        if self.ms < 1e-20 {
            self.ms = 0.0;
        }
        ms_to_db(self.ms)
    }

    pub fn reset(&mut self) {
        self.ms = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn settings(
        threshold_db: f32,
        mode: DynMode,
        range_db: f32,
        attack_ms: f32,
        release_ms: f32,
    ) -> DynSettings {
        DynSettings {
            threshold_db,
            mode,
            range_db,
            attack_ms,
            release_ms,
        }
    }

    #[test]
    fn silence_leaves_the_band_alone() {
        let (env, delta) = dynamic_step(
            settings(-24.0, DynMode::Above, -6.0, 20.0, 200.0),
            -90.0,
            0.0,
            0.01,
        );
        assert_eq!(env, 0.0);
        assert_eq!(delta, 0.0);
    }

    #[test]
    fn a_loud_band_walks_toward_full_range() {
        let mut env = 0.0;
        let mut delta = 0.0;
        // Well past the knee, so the target is 1.0 the whole way.
        for _ in 0..200 {
            let step = dynamic_step(
                settings(-24.0, DynMode::Above, -6.0, 20.0, 200.0),
                -6.0,
                env,
                0.001,
            );
            env = step.0;
            delta = step.1;
        }
        assert!(env > 0.99, "env was {env}");
        assert!((delta + 6.0).abs() < 0.1, "delta was {delta}");
    }

    #[test]
    fn below_mode_engages_on_quiet_signal() {
        let (env, delta) = dynamic_step(
            settings(-24.0, DynMode::Below, 4.0, 1.0, 200.0),
            -40.0,
            0.0,
            1.0,
        );
        assert!(env > 0.9);
        assert!(delta > 3.0);
    }

    #[test]
    fn the_knee_gives_partial_engagement() {
        // Half a knee past the threshold should target 0.5, not 1.0.
        let (env, _) = dynamic_step(
            settings(-24.0, DynMode::Above, -6.0, 0.1, 0.1),
            -24.0 + DYN_KNEE_DB / 2.0,
            0.0,
            1.0,
        );
        assert!((env - 0.5).abs() < 0.01, "env was {env}");
    }

    #[test]
    fn the_knee_eases_in_and_out_rather_than_cornering() {
        // Right at the threshold and right at the far edge the curve has to be
        // flat, or the band lurches into and out of engagement.
        let step = 0.05;
        let slope_at = |over: f32| (knee(over + step) - knee(over - step)) / (2.0 * step);
        let mid = slope_at(DYN_KNEE_DB / 2.0);
        assert!(slope_at(0.0) < mid * 0.35, "the low corner is still sharp");
        assert!(
            slope_at(DYN_KNEE_DB) < mid * 0.35,
            "the high corner is still sharp"
        );
        // And it still spans the full range monotonically.
        assert_eq!(knee(-1.0), 0.0);
        assert_eq!(knee(DYN_KNEE_DB * 2.0), 1.0);
        assert!((knee(DYN_KNEE_DB / 2.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn release_is_slower_than_attack() {
        let (fast, _) = dynamic_step(
            settings(-60.0, DynMode::Above, -6.0, 10.0, 500.0),
            0.0,
            0.0,
            0.005,
        );
        let (slow, _) = dynamic_step(
            settings(-60.0, DynMode::Above, -6.0, 10.0, 500.0),
            -90.0,
            1.0,
            0.005,
        );
        assert!(fast > 0.3, "attack moved only to {fast}");
        assert!(slow > 0.98, "release moved all the way to {slow}");
    }

    /// The detector's whole job: a steady tone has to read as a steady level,
    /// at the bottom of the range as much as the top.
    #[test]
    fn a_steady_tone_reads_a_steady_level_at_any_frequency() {
        const SR: f32 = 48_000.0;
        for freq in [40.0f32, 100.0, 1000.0, 10_000.0] {
            let mut det = LevelDetector::new();
            det.set_window(freq, SR);

            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            // Half a second in, then measure over the next half.
            for i in 0..(SR as usize) {
                det.push((2.0 * PI * freq * i as f32 / SR).sin());
                if i > SR as usize / 2 {
                    let db = det.level_db();
                    lo = lo.min(db);
                    hi = hi.max(db);
                }
            }
            // A full-scale sine is -3.01 dB RMS.
            assert!(
                (hi + 3.01).abs() < 0.5 && (lo + 3.01).abs() < 0.5,
                "{freq} Hz read {lo}..{hi} dB"
            );
            assert!(hi - lo < 0.6, "{freq} Hz wobbled over {} dB", hi - lo);
        }
    }

    #[test]
    fn the_detector_window_grows_toward_the_bottom_of_the_range() {
        assert!(detector_window_ms(20_000.0) < detector_window_ms(1000.0));
        assert!(detector_window_ms(1000.0) < detector_window_ms(50.0));
        // And stays inside bounds however absurd the frequency.
        assert_eq!(detector_window_ms(1.0), 40.0);
        assert_eq!(detector_window_ms(1e9), 1.0);
    }

    #[test]
    fn an_envelope_decaying_to_nothing_lands_on_zero() {
        let mut env = 1.0;
        for _ in 0..500 {
            env = step_toward(env, 0.0, 10.0, 0.001);
        }
        assert_eq!(env, 0.0, "the envelope trailed off into denormals");
    }
}
