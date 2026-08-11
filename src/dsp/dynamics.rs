//! Band dynamics — the envelope that moves a band's gain with the signal.
//!
//! Ported from `editor/src/dsp/dynamics.ts` so the UI's prediction of where a
//! dynamic band sits matches what the audio thread actually does.

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

/// One control-rate step.
///
/// `over` is how far the measured band level sits past the threshold in the
/// engaging direction; it maps across a [`DYN_KNEE_DB`] soft knee to 0..1, then
/// is smoothed by a one-pole using attack going up and release coming down.
/// Returns the new envelope and the gain offset in dB.
pub fn dynamic_step(s: DynSettings, level_db: f32, env: f32, dt: f32) -> (f32, f32) {
    let over = match s.mode {
        DynMode::Above => level_db - s.threshold_db,
        DynMode::Below => s.threshold_db - level_db,
    };
    let target = (over / DYN_KNEE_DB).clamp(0.0, 1.0);

    let tau_ms = if target > env {
        s.attack_ms
    } else {
        s.release_ms
    };
    let tau = (tau_ms / 1000.0).max(0.001);
    let next = env + (target - env) * (1.0 - (-dt / tau).exp());

    (next, next * s.range_db)
}

/// Mean square of a block, as dBFS. The floor keeps silence from reaching -inf.
pub fn ms_to_db(mean_square: f32) -> f32 {
    10.0 * mean_square.max(1e-12).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
