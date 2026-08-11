//! Per-band meter values, published by the audio thread and read by the editor.
//!
//! These are display-only: a dropped or slightly stale update costs a frame of
//! meter movement and nothing else, so plain relaxed atomics are the right
//! trade — no locks anywhere near `process`.

use std::sync::atomic::Ordering;

use nih_plug::prelude::AtomicF32;

use crate::dsp::engine::BandMeter;
use crate::params::MAX_BANDS;

pub struct Meters {
    /// Band level in dBFS, measured on the sidechain-filtered input.
    level: Vec<AtomicF32>,
    /// Gain offset the dynamics section is applying, in dB.
    delta: Vec<AtomicF32>,
}

impl Default for Meters {
    fn default() -> Self {
        Self {
            level: (0..MAX_BANDS).map(|_| AtomicF32::new(-100.0)).collect(),
            delta: (0..MAX_BANDS).map(|_| AtomicF32::new(0.0)).collect(),
        }
    }
}

impl Meters {
    pub fn publish(&self, slot: usize, meter: BandMeter) {
        self.level[slot].store(meter.level_db, Ordering::Relaxed);
        self.delta[slot].store(meter.delta_db, Ordering::Relaxed);
    }

    pub fn read_into(&self, level: &mut [f32], delta: &mut [f32]) {
        for slot in 0..MAX_BANDS.min(level.len()).min(delta.len()) {
            level[slot] = self.level[slot].load(Ordering::Relaxed);
            delta[slot] = self.delta[slot].load(Ordering::Relaxed);
        }
    }

    pub fn clear(&self) {
        for slot in 0..MAX_BANDS {
            self.level[slot].store(-100.0, Ordering::Relaxed);
            self.delta[slot].store(0.0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_values_come_back_out() {
        let meters = Meters::default();
        meters.publish(
            3,
            BandMeter {
                level_db: -12.5,
                delta_db: -4.0,
            },
        );

        let mut level = vec![0.0; MAX_BANDS];
        let mut delta = vec![0.0; MAX_BANDS];
        meters.read_into(&mut level, &mut delta);

        assert_eq!(level[3], -12.5);
        assert_eq!(delta[3], -4.0);
        // Untouched slots stay at the resting values.
        assert_eq!(level[0], -100.0);
        assert_eq!(delta[0], 0.0);
    }

    #[test]
    fn clearing_parks_every_slot() {
        let meters = Meters::default();
        for slot in 0..MAX_BANDS {
            meters.publish(
                slot,
                BandMeter {
                    level_db: 0.0,
                    delta_db: 6.0,
                },
            );
        }
        meters.clear();

        let mut level = vec![0.0; MAX_BANDS];
        let mut delta = vec![0.0; MAX_BANDS];
        meters.read_into(&mut level, &mut delta);
        assert!(level.iter().all(|v| *v == -100.0));
        assert!(delta.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn a_short_read_buffer_is_not_an_overrun() {
        let meters = Meters::default();
        meters.publish(
            0,
            BandMeter {
                level_db: -3.0,
                delta_db: 1.0,
            },
        );
        let mut level = vec![0.0; 2];
        let mut delta = vec![0.0; 2];
        meters.read_into(&mut level, &mut delta);
        assert_eq!(level[0], -3.0);
    }
}
