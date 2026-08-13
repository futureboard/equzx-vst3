//! Per-band meter values, published by the audio thread and read by the editor.
//!
//! These are display-only: a dropped or slightly stale update costs a frame of
//! meter movement and nothing else, so plain relaxed atomics are the right
//! trade — no locks anywhere near `process`.

use std::sync::atomic::Ordering;

use nih_plug::prelude::AtomicF32;

use crate::dsp::engine::BandMeter;
use crate::dsp::resonance::RES_BANDS;
use crate::dsp::spectral::{TargetView, MAX_TARGETS};
use crate::params::MAX_BANDS;

/// Stereo sample peaks accumulated between editor frames, as linear absolute
/// amplitudes where `1.0` is 0 dBFS.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IoPeaks {
    pub input: [f32; 2],
    pub output: [f32; 2],
}

pub struct Meters {
    /// Band level in dBFS, measured on the sidechain-filtered input.
    level: Vec<AtomicF32>,
    /// Gain offset the dynamics section is applying, in dB.
    delta: Vec<AtomicF32>,
    /// dB of cut per resonance band. Positive is a cut.
    resonance: Vec<AtomicF32>,
    /// The deepest of those, so the UI can show activity without the curve.
    resonance_peak: AtomicF32,
    /// The spectral pool's targets: freq, cut, q, confidence per slot, with
    /// freq 0 marking an empty one. Display-only, same relaxed discipline.
    targets: Vec<[AtomicF32; 4]>,
    /// Per-channel sample peaks. The audio thread accumulates maxima for every
    /// host callback and the editor consumes them once per frame.
    input_peak: [AtomicF32; 2],
    output_peak: [AtomicF32; 2],
}

impl Default for Meters {
    fn default() -> Self {
        Self {
            level: (0..MAX_BANDS).map(|_| AtomicF32::new(-100.0)).collect(),
            delta: (0..MAX_BANDS).map(|_| AtomicF32::new(0.0)).collect(),
            resonance: (0..RES_BANDS).map(|_| AtomicF32::new(0.0)).collect(),
            resonance_peak: AtomicF32::new(0.0),
            targets: (0..MAX_TARGETS)
                .map(|_| {
                    [
                        AtomicF32::new(0.0),
                        AtomicF32::new(0.0),
                        AtomicF32::new(0.0),
                        AtomicF32::new(0.0),
                    ]
                })
                .collect(),
            input_peak: [AtomicF32::new(0.0), AtomicF32::new(0.0)],
            output_peak: [AtomicF32::new(0.0), AtomicF32::new(0.0)],
        }
    }
}

impl Meters {
    pub fn publish(&self, slot: usize, meter: BandMeter) {
        self.level[slot].store(meter.level_db, Ordering::Relaxed);
        self.delta[slot].store(meter.delta_db, Ordering::Relaxed);
    }

    pub fn publish_resonance(&self, curve: &[f32], peak: f32) {
        for (slot, value) in curve.iter().enumerate().take(RES_BANDS) {
            self.resonance[slot].store(*value, Ordering::Relaxed);
        }
        self.resonance_peak.store(peak, Ordering::Relaxed);
    }

    pub fn read_into(&self, level: &mut [f32], delta: &mut [f32]) {
        for slot in 0..MAX_BANDS.min(level.len()).min(delta.len()) {
            level[slot] = self.level[slot].load(Ordering::Relaxed);
            delta[slot] = self.delta[slot].load(Ordering::Relaxed);
        }
    }

    /// Reads the reduction curve and returns its peak.
    pub fn read_resonance(&self, out: &mut [f32]) -> f32 {
        for slot in 0..RES_BANDS.min(out.len()) {
            out[slot] = self.resonance[slot].load(Ordering::Relaxed);
        }
        self.resonance_peak.load(Ordering::Relaxed)
    }

    pub fn publish_targets(&self, views: &[TargetView]) {
        for (slot, view) in self.targets.iter().zip(views.iter()) {
            slot[0].store(view.freq, Ordering::Relaxed);
            slot[1].store(view.cut_db, Ordering::Relaxed);
            slot[2].store(view.q, Ordering::Relaxed);
            slot[3].store(view.confidence, Ordering::Relaxed);
        }
    }

    pub fn read_targets(&self, out: &mut [TargetView]) {
        for (view, slot) in out.iter_mut().zip(self.targets.iter()) {
            *view = TargetView {
                freq: slot[0].load(Ordering::Relaxed),
                cut_db: slot[1].load(Ordering::Relaxed),
                q: slot[2].load(Ordering::Relaxed),
                confidence: slot[3].load(Ordering::Relaxed),
            };
        }
    }

    /// Accumulate one host callback's per-channel sample peaks.
    ///
    /// `fetch_max` keeps a short transient alive until the editor consumes it,
    /// even when several audio callbacks run between two GUI frames.
    pub fn publish_io(&self, peaks: IoPeaks) {
        for channel in 0..2 {
            let input = finite_peak(peaks.input[channel]);
            let output = finite_peak(peaks.output[channel]);
            self.input_peak[channel].fetch_max(input, Ordering::Relaxed);
            self.output_peak[channel].fetch_max(output, Ordering::Relaxed);
        }
    }

    /// Consume the peaks accumulated since the previous editor frame.
    pub fn take_io(&self) -> IoPeaks {
        IoPeaks {
            input: std::array::from_fn(|channel| {
                self.input_peak[channel].swap(0.0, Ordering::Relaxed)
            }),
            output: std::array::from_fn(|channel| {
                self.output_peak[channel].swap(0.0, Ordering::Relaxed)
            }),
        }
    }

    pub fn clear(&self) {
        for slot in 0..MAX_BANDS {
            self.level[slot].store(-100.0, Ordering::Relaxed);
            self.delta[slot].store(0.0, Ordering::Relaxed);
        }
        for slot in 0..RES_BANDS {
            self.resonance[slot].store(0.0, Ordering::Relaxed);
        }
        self.resonance_peak.store(0.0, Ordering::Relaxed);
        for slot in self.targets.iter() {
            for v in slot.iter() {
                v.store(0.0, Ordering::Relaxed);
            }
        }
        for peak in self.input_peak.iter().chain(self.output_peak.iter()) {
            peak.store(0.0, Ordering::Relaxed);
        }
    }
}

#[inline]
fn finite_peak(value: f32) -> f32 {
    if value.is_finite() {
        value.abs()
    } else {
        0.0
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

    #[test]
    fn io_peaks_accumulate_until_they_are_consumed() {
        let meters = Meters::default();
        meters.publish_io(IoPeaks {
            input: [0.75, 0.25],
            output: [0.5, 0.125],
        });
        meters.publish_io(IoPeaks {
            input: [0.5, 1.0],
            output: [0.25, 0.75],
        });

        assert_eq!(
            meters.take_io(),
            IoPeaks {
                input: [0.75, 1.0],
                output: [0.5, 0.75],
            }
        );
        assert_eq!(meters.take_io(), IoPeaks::default());
    }

    #[test]
    fn io_peaks_reject_non_finite_values_and_clear_to_silence() {
        let meters = Meters::default();
        meters.publish_io(IoPeaks {
            input: [f32::NAN, f32::INFINITY],
            output: [f32::NEG_INFINITY, -0.8],
        });
        assert_eq!(meters.take_io().output[1], 0.8);

        meters.publish_io(IoPeaks {
            input: [1.0, 0.5],
            output: [0.25, 0.125],
        });
        meters.clear();
        assert_eq!(meters.take_io(), IoPeaks::default());
    }
}
