//! Writing to the parameters, and what a frame of the UI is handed to read.
//!
//! This is what the `Action` enum used to be. The webview could only ask for a
//! change by describing it in JSON; here the UI holds a [`ParamSetter`] and
//! makes the change itself. What survives from that design is the discipline
//! around it: every write is wrapped in the begin/end pair a host needs to see
//! in order to record a gesture, and a whole-state recall still resets a slot
//! before writing into it so a preset that omits a field gets the default rather
//! than whatever the last band left behind.

use std::sync::Arc;

use nih_plug::prelude::*;

use crate::gui::gpu::FxRenderer;
use crate::gui::state::{BandSnapshot, ChannelView, Snapshot};
use crate::params::{
    BandChannel, BandKind, BandParams, DynMode, EquzxParams, ResonanceParams, Slope, TransientState,
    MAX_BANDS,
};

/// Everything one frame of the UI can see and do.
pub struct Frame<'a> {
    pub setter: &'a ParamSetter<'a>,
    pub params: &'a EquzxParams,
    pub transient: &'a TransientState,
    /// Per-slot band level in dBFS, indexed by slot.
    pub level: &'a [f32],
    /// Per-slot dynamic gain offset in dB, indexed by slot.
    pub delta: &'a [f32],
    /// dB of cut per resonance band. Positive is a cut.
    pub resonance: &'a [f32],
    /// The deepest cut anywhere in the bank.
    pub resonance_peak: f32,
    pub spectrum_pre: &'a [f32],
    pub spectrum_post: &'a [f32],
    pub sample_rate: f32,
    pub fx: &'a Arc<FxRenderer>,
}

impl Frame<'_> {
    pub fn band(&self, slot: usize) -> Option<&BandParams> {
        self.params.bands.get(slot)
    }
}

/// Cut slopes in the order the wheel and the button row step through them.
pub const SLOPES: [Slope; 6] = [
    Slope::S12,
    Slope::S24,
    Slope::S36,
    Slope::S48,
    Slope::S72,
    Slope::S96,
];

// --- primitives --------------------------------------------------------------

pub fn set_float(setter: &ParamSetter, param: &FloatParam, value: f32) {
    if !value.is_finite() {
        return;
    }
    setter.begin_set_parameter(param);
    setter.set_parameter(param, value);
    setter.end_set_parameter(param);
}

pub fn set_bool(setter: &ParamSetter, param: &BoolParam, value: bool) {
    setter.begin_set_parameter(param);
    setter.set_parameter(param, value);
    setter.end_set_parameter(param);
}

pub fn set_enum<T: Enum + PartialEq>(setter: &ParamSetter, param: &EnumParam<T>, value: T) {
    setter.begin_set_parameter(param);
    setter.set_parameter(param, value);
    setter.end_set_parameter(param);
}

// --- one band ----------------------------------------------------------------

macro_rules! band_setter {
    ($name:ident, $field:ident, $kind:ident, $ty:ty) => {
        pub fn $name(frame: &Frame, slot: usize, value: $ty) {
            if let Some(band) = frame.band(slot) {
                $kind(frame.setter, &band.$field, value);
            }
        }
    };
}

band_setter!(set_freq, freq, set_float, f32);
band_setter!(set_gain, gain, set_float, f32);
band_setter!(set_q, q, set_float, f32);
band_setter!(set_dyn_range, dyn_range, set_float, f32);
band_setter!(set_threshold, threshold, set_float, f32);
band_setter!(set_attack, attack, set_float, f32);
band_setter!(set_release, release, set_float, f32);
band_setter!(set_enabled, enabled, set_bool, bool);
band_setter!(set_dynamic, dynamic, set_bool, bool);
band_setter!(set_kind, kind, set_enum, BandKind);
band_setter!(set_channel, channel, set_enum, BandChannel);
band_setter!(set_slope, slope, set_enum, Slope);
band_setter!(set_dyn_mode, dyn_mode, set_enum, DynMode);

/// Per-band resonance travels as a percentage and is held as a ratio — the one
/// conversion on this side of the UI.
pub fn set_band_resonance(frame: &Frame, slot: usize, percent: f32) {
    if let Some(band) = frame.band(slot) {
        set_float(frame.setter, &band.resonance, percent / 100.0);
    }
}

/// Move a cut band's slope by one step of the list.
pub fn step_slope(frame: &Frame, slot: usize, direction: i32) {
    let Some(band) = frame.band(slot) else {
        return;
    };
    let current = SLOPES
        .iter()
        .position(|s| *s == band.slope.value())
        .unwrap_or(1) as i32;
    let next = (current + direction).clamp(0, SLOPES.len() as i32 - 1) as usize;
    set_enum(frame.setter, &band.slope, SLOPES[next]);
}

/// Claim the lowest free slot and give it a starting shape.
///
/// A band created while looking at one channel belongs to that channel —
/// otherwise it would appear in a view that is not showing it.
pub fn add_band(frame: &Frame, freq: f32, gain: f32, view: ChannelView) -> Option<usize> {
    let slot = (0..MAX_BANDS).find(|slot| !frame.params.bands[*slot].active.value())?;
    let band = frame.band(slot)?;
    reset_band(frame.setter, band);
    write_band(
        frame.setter,
        band,
        &BandSnapshot {
            freq,
            gain,
            channel: view.new_band_channel(),
            ..BandSnapshot::default()
        },
    );
    set_bool(frame.setter, &band.active, true);
    Some(slot)
}

pub fn remove_band(frame: &Frame, slot: usize) {
    if let Some(band) = frame.band(slot) {
        set_bool(frame.setter, &band.active, false);
    }
    if frame.transient.solo() == Some(slot) {
        frame.transient.set_solo(None);
    }
}

/// Put a slot back to its defaults before a recall writes into it.
pub fn reset_band(setter: &ParamSetter, band: &BandParams) {
    set_bool(setter, &band.enabled, band.enabled.default_plain_value());
    set_enum(setter, &band.kind, band.kind.default_plain_value());
    set_enum(setter, &band.channel, band.channel.default_plain_value());
    set_enum(setter, &band.slope, band.slope.default_plain_value());
    set_enum(setter, &band.dyn_mode, band.dyn_mode.default_plain_value());
    set_float(setter, &band.freq, band.freq.default_plain_value());
    set_float(setter, &band.gain, band.gain.default_plain_value());
    set_float(setter, &band.q, band.q.default_plain_value());
    set_float(setter, &band.dyn_range, band.dyn_range.default_plain_value());
    set_float(setter, &band.threshold, band.threshold.default_plain_value());
    set_float(setter, &band.attack, band.attack.default_plain_value());
    set_float(setter, &band.release, band.release.default_plain_value());
    set_float(setter, &band.resonance, band.resonance.default_plain_value());
    set_bool(setter, &band.dynamic, band.dynamic.default_plain_value());
}

fn write_band(setter: &ParamSetter, band: &BandParams, snapshot: &BandSnapshot) {
    set_enum(setter, &band.kind, snapshot.kind);
    set_enum(setter, &band.channel, snapshot.channel);
    set_enum(setter, &band.slope, snapshot.slope);
    set_enum(setter, &band.dyn_mode, snapshot.dyn_mode);
    set_float(setter, &band.freq, snapshot.freq);
    set_float(setter, &band.gain, snapshot.gain);
    set_float(setter, &band.q, snapshot.q);
    set_float(setter, &band.dyn_range, snapshot.dyn_range);
    set_float(setter, &band.threshold, snapshot.threshold);
    set_float(setter, &band.attack, snapshot.attack);
    set_float(setter, &band.release, snapshot.release);
    set_float(setter, &band.resonance, snapshot.resonance / 100.0);
    set_bool(setter, &band.enabled, snapshot.enabled);
    set_bool(setter, &band.dynamic, snapshot.dynamic);
}

// --- the whole state ---------------------------------------------------------

/// Apply the resonance stage from a snapshot. Percentages in, ratios out.
pub fn write_resonance(setter: &ParamSetter, p: &ResonanceParams, snapshot: &crate::gui::state::ResonanceSnapshot) {
    set_bool(setter, &p.enabled, snapshot.enabled);
    set_float(setter, &p.depth, snapshot.depth / 100.0);
    set_float(setter, &p.sharpness, snapshot.sharpness / 100.0);
    set_float(setter, &p.mix, snapshot.mix / 100.0);
    set_float(setter, &p.threshold, snapshot.threshold);
    set_float(setter, &p.attack, snapshot.attack);
    set_float(setter, &p.release, snapshot.release);
    set_float(setter, &p.low, snapshot.low);
    set_float(setter, &p.high, snapshot.high);
}

/// Replace everything at once — an A/B swap, a preset, a reset.
///
/// Slots the snapshot does not name are freed, so a swap cannot leave a stray
/// band from the other slot behind. The DSP is flushed as well: a recall moves
/// every band at once, and without it a chorus of filters rings out on settings
/// that no longer exist.
pub fn apply_snapshot(frame: &Frame, snapshot: &Snapshot) {
    let snapshot = snapshot.sanitized();
    let used = snapshot.bands.len().min(MAX_BANDS);

    for (slot, band) in frame.params.bands.iter().enumerate() {
        if slot >= used && band.active.value() {
            set_bool(frame.setter, &band.active, false);
        }
    }
    for (slot, wanted) in snapshot.bands.iter().take(used).enumerate() {
        let band = &frame.params.bands[slot];
        reset_band(frame.setter, band);
        write_band(frame.setter, band, wanted);
        set_bool(frame.setter, &band.active, true);
    }

    set_float(frame.setter, &frame.params.output_gain, snapshot.output_gain);
    write_resonance(frame.setter, &frame.params.resonance, &snapshot.resonance);

    frame.transient.set_solo(None);
    frame
        .transient
        .flush
        .store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Reset to flat: no bands, unity output, the suppressor back to its defaults.
pub fn reset_all(frame: &Frame) {
    apply_snapshot(frame, &Snapshot::default());
    set_bool(frame.setter, &frame.params.bypass, false);
}

/// Everything the parameters currently say, for an A/B park or a preset save.
pub fn capture(params: &EquzxParams) -> Snapshot {
    Snapshot::capture(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slope_list_matches_the_parameter() {
        for slope in SLOPES {
            assert_eq!(Slope::from_db_per_oct(slope.db_per_oct()), Some(slope));
        }
        // Steps run from shallowest to steepest, which is what makes the wheel
        // direction mean something.
        let steps: Vec<u32> = SLOPES.iter().map(|s| s.db_per_oct()).collect();
        assert_eq!(steps, vec![12, 24, 36, 48, 72, 96]);
    }

    #[test]
    fn a_new_band_belongs_to_the_channel_it_was_drawn_in() {
        assert_eq!(ChannelView::All.new_band_channel(), BandChannel::Stereo);
        assert_eq!(ChannelView::Side.new_band_channel(), BandChannel::Side);
        assert_eq!(ChannelView::Left.new_band_channel(), BandChannel::Left);
    }
}
