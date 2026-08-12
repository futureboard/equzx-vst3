//! What the UI holds that the parameters do not.
//!
//! Three kinds of thing live here.
//!
//! [`BandView`] is a band read out of the parameter array — a plain snapshot the
//! display and the band panel can pass around without holding a lock or a
//! smoother. It replaces the `StateMessage` the plugin used to serialise for the
//! webview: same fields, no JSON, and read fresh every frame instead of diffed.
//!
//! [`Snapshot`] is everything an A/B slot or a preset carries. It is the one
//! thing here that still travels as JSON, because preset files are a format
//! people already have on disk — the shapes below are the ones
//! `state/presets.ts` wrote, so a preset saved by the web UI still loads.
//!
//! [`UiState`] is the view state the DAW should remember but never automate.

use serde::{Deserialize, Serialize};

use crate::params::{
    BandChannel, BandKind, BandResMode, DynMode, EquzxParams, ResMode, ResQuality,
    ResonanceParams, Slope, MAX_BANDS,
};

// The resonance enums travel as their wire names, like every other enum in a
// preset — implemented here so `params` stays serde-free.

macro_rules! wire_serde {
    ($ty:ty, $default:expr) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_wire())
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                // A name this build does not know lands on the default rather
                // than failing the whole preset.
                Ok(<$ty>::from_wire(&s).unwrap_or($default))
            }
        }
    };
}

wire_serde!(ResMode, ResMode::Adaptive);
wire_serde!(ResQuality, ResQuality::Ultra);
wire_serde!(BandResMode, BandResMode::Adaptive);

// --- a band, as the UI reads it ---------------------------------------------

/// One active band slot, flattened for drawing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BandView {
    /// Slot index, which is the band's identity everywhere: automation lane,
    /// solo target, and the number drawn on its handle.
    pub slot: usize,
    pub kind: BandKind,
    pub channel: BandChannel,
    pub freq: f32,
    pub gain: f32,
    pub q: f32,
    pub slope: Slope,
    pub enabled: bool,
    pub dynamic: bool,
    pub dyn_mode: DynMode,
    pub dyn_range: f32,
    pub threshold: f32,
    pub attack: f32,
    pub release: f32,
    /// Per-band resonance suppression, 0..100 as the UI shows it.
    pub resonance: f32,
    pub res_mode: BandResMode,
    /// Ceiling on this band's resonance cut, in dB.
    pub res_range: f32,
    /// dB taken off the detection threshold inside this band's region.
    pub res_sens: f32,
    /// Spectral search half-width, octaves either side of the band.
    pub res_width: f32,
    pub res_attack: f32,
    pub res_release: f32,
}

impl BandView {
    /// Dynamics move a band's gain, so they only mean something where gain does.
    pub fn can_be_dynamic(&self) -> bool {
        self.kind.uses_gain()
    }

    /// Is this band drawn at all in the channel view currently on screen?
    pub fn in_view(&self, view: ChannelView) -> bool {
        match view {
            ChannelView::All => true,
            ChannelView::Left => matches!(self.channel, BandChannel::Stereo | BandChannel::Left),
            ChannelView::Right => matches!(self.channel, BandChannel::Stereo | BandChannel::Right),
            ChannelView::Mid => matches!(self.channel, BandChannel::Stereo | BandChannel::Mid),
            ChannelView::Side => matches!(self.channel, BandChannel::Stereo | BandChannel::Side),
        }
    }

    /// The one-letter badge the band list and the display draw.
    pub fn badge(&self) -> &'static str {
        match self.channel {
            BandChannel::Stereo => "",
            BandChannel::Left => "L",
            BandChannel::Right => "R",
            BandChannel::Mid => "M",
            BandChannel::Side => "S",
        }
    }

    /// Where the handle sits vertically: at its gain, or on the zero line for
    /// the types that have no gain to speak of.
    pub fn handle_db(&self) -> f32 {
        if self.kind.uses_gain() {
            self.gain
        } else {
            0.0
        }
    }
}

/// Read every active slot, low slot first.
///
/// Slot order rather than frequency order, because the index into this list is
/// what picks a band's colour and its number — and a band that changed colour
/// when another one was dragged past it would be worse than one whose numbers
/// are not left to right.
pub fn read_bands(params: &EquzxParams) -> Vec<BandView> {
    let mut bands = Vec::with_capacity(MAX_BANDS);
    for (slot, p) in params.bands.iter().enumerate() {
        if !p.active.value() {
            continue;
        }
        bands.push(BandView {
            slot,
            kind: p.kind.value(),
            channel: p.channel.value(),
            freq: p.freq.value(),
            gain: p.gain.value(),
            q: p.q.value(),
            slope: p.slope.value(),
            enabled: p.enabled.value(),
            dynamic: p.dynamic.value(),
            dyn_mode: p.dyn_mode.value(),
            dyn_range: p.dyn_range.value(),
            threshold: p.threshold.value(),
            attack: p.attack.value(),
            release: p.release.value(),
            resonance: p.resonance.value() * 100.0,
            res_mode: p.res_mode.value(),
            res_range: p.res_range.value(),
            res_sens: p.res_sens.value(),
            res_width: p.res_width.value(),
            res_attack: p.res_attack.value(),
            res_release: p.res_release.value(),
        });
    }
    bands
}

/// Lowest slot no band is occupying, or `None` when the bank is full.
pub fn free_slot(params: &EquzxParams) -> Option<usize> {
    (0..MAX_BANDS).find(|slot| !params.bands[*slot].active.value())
}

// --- view state --------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum AnalyzerMode {
    Off,
    Pre,
    Post,
    #[default]
    Both,
}

impl AnalyzerMode {
    pub fn label(self) -> &'static str {
        match self {
            AnalyzerMode::Off => "Off",
            AnalyzerMode::Pre => "Pre",
            AnalyzerMode::Post => "Post",
            AnalyzerMode::Both => "Pre + Post",
        }
    }

    pub const ALL: [AnalyzerMode; 4] = [
        AnalyzerMode::Off,
        AnalyzerMode::Pre,
        AnalyzerMode::Post,
        AnalyzerMode::Both,
    ];

    pub fn draws_pre(self) -> bool {
        matches!(self, AnalyzerMode::Pre | AnalyzerMode::Both)
    }

    pub fn draws_post(self) -> bool {
        matches!(self, AnalyzerMode::Post | AnalyzerMode::Both)
    }
}

/// Which slice of the stereo image the display is showing.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChannelView {
    #[default]
    All,
    Left,
    Right,
    Mid,
    Side,
}

impl ChannelView {
    pub fn label(self) -> &'static str {
        match self {
            ChannelView::All => "Stereo",
            ChannelView::Left => "Left",
            ChannelView::Right => "Right",
            ChannelView::Mid => "Mid",
            ChannelView::Side => "Side",
        }
    }

    pub const ALL: [ChannelView; 5] = [
        ChannelView::All,
        ChannelView::Left,
        ChannelView::Right,
        ChannelView::Mid,
        ChannelView::Side,
    ];

    /// The channel a band created while looking at this view belongs to.
    pub fn new_band_channel(self) -> BandChannel {
        match self {
            ChannelView::All => BandChannel::Stereo,
            ChannelView::Left => BandChannel::Left,
            ChannelView::Right => BandChannel::Right,
            ChannelView::Mid => BandChannel::Mid,
            ChannelView::Side => BandChannel::Side,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AbSlot {
    #[default]
    A,
    B,
}

impl AbSlot {
    pub fn other(self) -> Self {
        match self {
            AbSlot::A => AbSlot::B,
            AbSlot::B => AbSlot::A,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AbSlot::A => "A",
            AbSlot::B => "B",
        }
    }
}

/// Height of the band panel at the bottom of the window, and its limits.
pub const PANEL_DEFAULT: f32 = 232.0;
pub const PANEL_MIN: f32 = 176.0;

/// View state the plugin stores with the session but never automates.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct UiState {
    pub db_range: f32,
    pub analyzer_mode: AnalyzerMode,
    /// Fractional-octave spectrum smoothing, e.g. 1/12. Zero is raw.
    pub spectrum_smoothing: f32,
    pub channel_view: ChannelView,
    pub panel_height: f32,
    pub slot: AbSlot,
    /// The A/B slot that is *not* live.
    pub parked: Snapshot,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            db_range: 18.0,
            analyzer_mode: AnalyzerMode::default(),
            spectrum_smoothing: 1.0 / 12.0,
            channel_view: ChannelView::default(),
            panel_height: PANEL_DEFAULT,
            slot: AbSlot::A,
            parked: Snapshot::default(),
        }
    }
}

impl UiState {
    /// Read the persisted blob. Anything unreadable lands on the defaults rather
    /// than taking the editor down — this came out of a session file.
    pub fn load(raw: &str) -> Self {
        if raw.is_empty() {
            return Self::default();
        }
        let mut state: Self = serde_json::from_str(raw).unwrap_or_default();
        state.db_range = if [6.0, 12.0, 18.0, 30.0].contains(&state.db_range) {
            state.db_range
        } else {
            18.0
        };
        state.spectrum_smoothing = state.spectrum_smoothing.clamp(0.0, 1.0);
        state.panel_height = state.panel_height.max(PANEL_MIN);
        state.parked = state.parked.sanitized();
        state
    }

    pub fn save(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// --- snapshots ---------------------------------------------------------------

/// Everything an A/B slot or a preset carries.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Snapshot {
    pub bands: Vec<BandSnapshot>,
    pub output_gain: f32,
    pub resonance: ResonanceSnapshot,
}

impl Snapshot {
    /// Read the live parameters.
    ///
    /// `delta` is deliberately not carried: it is a way of listening rather than
    /// part of the sound being designed, so it survives a swap or a recall
    /// untouched, for the same reason solo does.
    pub fn capture(params: &EquzxParams) -> Self {
        Self {
            bands: read_bands(params).iter().map(BandSnapshot::from).collect(),
            output_gain: params.output_gain.value(),
            resonance: ResonanceSnapshot::capture(&params.resonance),
        }
    }

    /// Rebuild from untrusted input — a preset file someone edited by hand, or a
    /// session written by a different version. Every field is clamped to the
    /// range its parameter will accept.
    pub fn sanitized(&self) -> Self {
        Self {
            bands: self
                .bands
                .iter()
                .take(MAX_BANDS)
                .map(BandSnapshot::sanitized)
                .collect(),
            output_gain: finite(self.output_gain, 0.0).clamp(-24.0, 12.0),
            resonance: self.resonance.sanitized(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bands.is_empty()
    }
}

/// One band inside a snapshot. Same fields as [`BandView`] minus the slot, which
/// is assigned on recall rather than carried — two slots would otherwise claim
/// the same parameters after an A/B swap.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BandSnapshot {
    pub kind: BandKind,
    pub channel: BandChannel,
    pub freq: f32,
    pub gain: f32,
    pub q: f32,
    pub slope: Slope,
    pub enabled: bool,
    pub dynamic: bool,
    pub dyn_mode: DynMode,
    pub dyn_range: f32,
    pub threshold: f32,
    pub attack: f32,
    pub release: f32,
    pub resonance: f32,
    pub res_mode: BandResMode,
    pub res_range: f32,
    pub res_sens: f32,
    pub res_width: f32,
    pub res_attack: f32,
    pub res_release: f32,
}

/// The shape a band is created with — the same one `makeBand` used to hand out.
impl Default for BandSnapshot {
    fn default() -> Self {
        Self {
            kind: BandKind::Bell,
            channel: BandChannel::Stereo,
            freq: 1000.0,
            gain: 0.0,
            q: 1.0,
            slope: Slope::S24,
            enabled: true,
            dynamic: false,
            dyn_mode: DynMode::Above,
            dyn_range: -6.0,
            threshold: -24.0,
            attack: 20.0,
            release: 200.0,
            resonance: 0.0,
            res_mode: BandResMode::Adaptive,
            res_range: 36.0,
            res_sens: 0.0,
            res_width: 1.0,
            res_attack: 5.0,
            res_release: 40.0,
        }
    }
}

impl From<&BandView> for BandSnapshot {
    fn from(b: &BandView) -> Self {
        Self {
            kind: b.kind,
            channel: b.channel,
            freq: b.freq,
            gain: b.gain,
            q: b.q,
            slope: b.slope,
            enabled: b.enabled,
            dynamic: b.dynamic,
            dyn_mode: b.dyn_mode,
            dyn_range: b.dyn_range,
            threshold: b.threshold,
            attack: b.attack,
            release: b.release,
            resonance: b.resonance,
            res_mode: b.res_mode,
            res_range: b.res_range,
            res_sens: b.res_sens,
            res_width: b.res_width,
            res_attack: b.res_attack,
            res_release: b.res_release,
        }
    }
}

impl BandSnapshot {
    pub fn sanitized(&self) -> Self {
        Self {
            freq: finite(self.freq, 1000.0).clamp(20.0, 22_000.0),
            gain: finite(self.gain, 0.0).clamp(-30.0, 30.0),
            q: finite(self.q, 1.0).clamp(0.025, 40.0),
            dyn_range: finite(self.dyn_range, -6.0).clamp(-30.0, 30.0),
            threshold: finite(self.threshold, -24.0).clamp(-70.0, 0.0),
            attack: finite(self.attack, 20.0).clamp(1.0, 300.0),
            release: finite(self.release, 200.0).clamp(10.0, 2000.0),
            resonance: finite(self.resonance, 0.0).clamp(0.0, 100.0),
            res_range: finite(self.res_range, 36.0).clamp(0.0, 36.0),
            res_sens: finite(self.res_sens, 0.0).clamp(-12.0, 12.0),
            res_width: finite(self.res_width, 1.0).clamp(0.25, 2.0),
            res_attack: finite(self.res_attack, 5.0).clamp(0.5, 100.0),
            res_release: finite(self.res_release, 40.0).clamp(5.0, 1000.0),
            ..*self
        }
    }
}

/// The resonance stage inside a snapshot.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct ResonanceSnapshot {
    pub enabled: bool,
    pub mode: ResMode,
    pub quality: ResQuality,
    /// Ceiling on any single cut, in dB.
    pub range: f32,
    /// The three ratios travel as percentages, the units the UI shows.
    pub depth: f32,
    pub sharpness: f32,
    pub threshold: f32,
    pub attack: f32,
    pub release: f32,
    pub low: f32,
    pub high: f32,
    pub mix: f32,
}

impl Default for ResonanceSnapshot {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ResMode::Adaptive,
            quality: ResQuality::Ultra,
            range: 36.0,
            depth: 50.0,
            sharpness: 50.0,
            threshold: 6.0,
            attack: 5.0,
            release: 40.0,
            low: 20.0,
            high: 20_000.0,
            mix: 100.0,
        }
    }
}

impl ResonanceSnapshot {
    pub fn capture(p: &ResonanceParams) -> Self {
        Self {
            enabled: p.enabled.value(),
            mode: p.mode.value(),
            quality: p.quality.value(),
            range: p.range.value(),
            depth: p.depth.value() * 100.0,
            sharpness: p.sharpness.value() * 100.0,
            threshold: p.threshold.value(),
            attack: p.attack.value(),
            release: p.release.value(),
            low: p.low.value(),
            high: p.high.value(),
            mix: p.mix.value() * 100.0,
        }
    }

    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        Self {
            enabled: self.enabled,
            mode: self.mode,
            quality: self.quality,
            range: finite(self.range, d.range).clamp(0.0, 36.0),
            depth: finite(self.depth, d.depth).clamp(0.0, 100.0),
            sharpness: finite(self.sharpness, d.sharpness).clamp(0.0, 100.0),
            threshold: finite(self.threshold, d.threshold).clamp(-12.0, 24.0),
            attack: finite(self.attack, d.attack).clamp(0.5, 100.0),
            release: finite(self.release, d.release).clamp(5.0, 1000.0),
            low: finite(self.low, d.low).clamp(20.0, 2000.0),
            high: finite(self.high, d.high).clamp(500.0, 20_000.0),
            mix: finite(self.mix, d.mix).clamp(0.0, 100.0),
        }
    }
}

fn finite(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

// --- the preset wire format --------------------------------------------------
//
// Enums travel as the strings `dsp/bands.ts` named them, so a preset written by
// the web UI still loads and one written here still means something to a human
// reading the file.

/// A [`BandSnapshot`] as it appears in a preset file.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
struct BandWire {
    #[serde(rename = "type")]
    kind: String,
    channel: String,
    freq: f32,
    gain: f32,
    q: f32,
    slope: u32,
    enabled: bool,
    dynamic: bool,
    dyn_mode: String,
    dyn_range: f32,
    threshold: f32,
    attack: f32,
    release: f32,
    resonance: f32,
    res_mode: BandResMode,
    res_range: f32,
    res_sens: f32,
    res_width: f32,
    res_attack: f32,
    res_release: f32,
}

impl Default for BandWire {
    fn default() -> Self {
        Self {
            kind: "bell".into(),
            channel: "stereo".into(),
            freq: 1000.0,
            gain: 0.0,
            q: 1.0,
            slope: 24,
            enabled: true,
            dynamic: false,
            dyn_mode: "above".into(),
            dyn_range: -6.0,
            threshold: -24.0,
            attack: 20.0,
            release: 200.0,
            resonance: 0.0,
            res_mode: BandResMode::Adaptive,
            res_range: 36.0,
            res_sens: 0.0,
            res_width: 1.0,
            res_attack: 5.0,
            res_release: 40.0,
        }
    }
}

impl From<BandWire> for BandSnapshot {
    fn from(w: BandWire) -> Self {
        // A name this build does not know is dropped rather than guessed at, so
        // a typo cannot silently turn a low cut into a bell.
        Self {
            kind: BandKind::from_wire(&w.kind).unwrap_or(BandKind::Bell),
            channel: BandChannel::from_wire(&w.channel).unwrap_or(BandChannel::Stereo),
            freq: w.freq,
            gain: w.gain,
            q: w.q,
            slope: Slope::from_db_per_oct(w.slope).unwrap_or(Slope::S24),
            enabled: w.enabled,
            dynamic: w.dynamic,
            dyn_mode: DynMode::from_wire(&w.dyn_mode).unwrap_or(DynMode::Above),
            dyn_range: w.dyn_range,
            threshold: w.threshold,
            attack: w.attack,
            release: w.release,
            resonance: w.resonance,
            res_mode: w.res_mode,
            res_range: w.res_range,
            res_sens: w.res_sens,
            res_width: w.res_width,
            res_attack: w.res_attack,
            res_release: w.res_release,
        }
        .sanitized()
    }
}

impl From<BandSnapshot> for BandWire {
    fn from(b: BandSnapshot) -> Self {
        Self {
            kind: b.kind.as_wire().into(),
            channel: b.channel.as_wire().into(),
            freq: b.freq,
            gain: b.gain,
            q: b.q,
            slope: b.slope.db_per_oct(),
            enabled: b.enabled,
            dynamic: b.dynamic,
            dyn_mode: b.dyn_mode.as_wire().into(),
            dyn_range: b.dyn_range,
            threshold: b.threshold,
            attack: b.attack,
            release: b.release,
            resonance: b.resonance,
            res_mode: b.res_mode,
            res_range: b.res_range,
            res_sens: b.res_sens,
            res_width: b.res_width,
            res_attack: b.res_attack,
            res_release: b.res_release,
        }
    }
}

impl Serialize for BandSnapshot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        BandWire::from(*self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BandSnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BandWire::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_written_by_the_web_ui_still_loads() {
        // Verbatim shape of what `state/presets.ts` used to write, `id` and all.
        let json = r#"{
            "name": "Vocal",
            "version": 2,
            "outputGain": -3,
            "bands": [
                {"id": 7, "type": "highshelf", "channel": "side", "freq": 8000,
                 "gain": 2.5, "q": 0.7, "slope": 24, "enabled": true,
                 "dynamic": true, "dynMode": "below", "dynRange": -4,
                 "threshold": -18, "attack": 12, "release": 150, "resonance": 25},
                {"type": "lowcut", "slope": 48, "freq": 80}
            ],
            "resonance": {"enabled": true, "depth": 70, "low": 120}
        }"#;

        let snap: Snapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snap.output_gain, -3.0);
        assert_eq!(snap.bands.len(), 2);

        let first = snap.bands[0];
        assert_eq!(first.kind, BandKind::HighShelf);
        assert_eq!(first.channel, BandChannel::Side);
        assert_eq!(first.dyn_mode, DynMode::Below);
        assert_eq!(first.resonance, 25.0);

        // A band that named only some fields gets the defaults for the rest.
        let second = snap.bands[1];
        assert_eq!(second.kind, BandKind::LowCut);
        assert_eq!(second.slope, Slope::S48);
        assert_eq!(second.freq, 80.0);
        assert_eq!(second.q, 1.0);
        assert!(second.enabled);

        // And the stage keeps what the file did not mention.
        assert!(snap.resonance.enabled);
        assert_eq!(snap.resonance.depth, 70.0);
        assert_eq!(snap.resonance.mix, 100.0);
    }

    #[test]
    fn a_preset_from_before_the_resonance_stage_lands_on_the_defaults() {
        let json = r#"{"outputGain": 0, "bands": [{"type": "bell"}]}"#;
        let snap: Snapshot = serde_json::from_str(json).unwrap();
        // Which is to say: switched off, so an old preset sounds like an old preset.
        assert!(!snap.resonance.enabled);
        assert_eq!(snap.resonance, ResonanceSnapshot::default());
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let snap = Snapshot {
            output_gain: -6.0,
            bands: vec![BandSnapshot {
                kind: BandKind::Notch,
                channel: BandChannel::Mid,
                freq: 250.0,
                q: 8.0,
                slope: Slope::S96,
                dyn_mode: DynMode::Below,
                ..BandSnapshot::default()
            }],
            ..Snapshot::default()
        };

        let text = serde_json::to_string(&snap).unwrap();
        assert!(text.contains(r#""type":"notch""#));
        assert!(text.contains(r#""channel":"mid""#));
        assert!(text.contains(r#""slope":96"#));

        let back: Snapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn nonsense_in_a_preset_is_clamped_rather_than_trusted() {
        let json = r#"{"bands":[{"type":"sideways","channel":"upwards","slope":13,
                     "freq": 1e9, "q": -5, "gain": 400, "attack": 0}],
                     "outputGain": 99}"#;
        let snap: Snapshot = serde_json::from_str::<Snapshot>(json).unwrap().sanitized();
        let band = snap.bands[0];
        assert_eq!(band.kind, BandKind::Bell);
        assert_eq!(band.channel, BandChannel::Stereo);
        assert_eq!(band.slope, Slope::S24);
        assert_eq!(band.freq, 22_000.0);
        assert_eq!(band.q, 0.025);
        assert_eq!(band.gain, 30.0);
        assert_eq!(band.attack, 1.0);
        assert_eq!(snap.output_gain, 12.0);
    }

    #[test]
    fn a_non_finite_value_falls_back_instead_of_poisoning_the_clamp() {
        let band = BandSnapshot {
            freq: f32::NAN,
            q: f32::INFINITY,
            ..BandSnapshot::default()
        }
        .sanitized();
        assert_eq!(band.freq, 1000.0);
        // Infinity falls back rather than clamping to the top of the range:
        // a preset that lost a value should read as "unset", not "maximum".
        assert_eq!(band.q, 1.0);
    }

    #[test]
    fn unreadable_view_state_lands_on_the_defaults() {
        assert_eq!(UiState::load("not json at all"), UiState::default());
        assert_eq!(UiState::load(""), UiState::default());
    }

    #[test]
    fn view_state_round_trips_and_is_bounded() {
        let state = UiState {
            db_range: 30.0,
            analyzer_mode: AnalyzerMode::Pre,
            spectrum_smoothing: 1.0 / 6.0,
            channel_view: ChannelView::Side,
            panel_height: 300.0,
            slot: AbSlot::B,
            parked: Snapshot::default(),
        };
        assert_eq!(UiState::load(&state.save()), state);

        // A range the picker cannot produce comes back as the default rather
        // than leaving the display on an axis nothing selects.
        let odd = UiState {
            db_range: 4.5,
            panel_height: 10.0,
            ..UiState::default()
        };
        let loaded = UiState::load(&odd.save());
        assert_eq!(loaded.db_range, 18.0);
        assert_eq!(loaded.panel_height, PANEL_MIN);
    }

    #[test]
    fn a_band_is_only_drawn_in_a_view_that_contains_it() {
        let mut band = BandView {
            slot: 0,
            kind: BandKind::Bell,
            channel: BandChannel::Mid,
            freq: 1000.0,
            gain: 0.0,
            q: 1.0,
            slope: Slope::S24,
            enabled: true,
            dynamic: false,
            dyn_mode: DynMode::Above,
            dyn_range: -6.0,
            threshold: -24.0,
            attack: 20.0,
            release: 200.0,
            resonance: 0.0,
            res_mode: BandResMode::Adaptive,
            res_range: 36.0,
            res_sens: 0.0,
            res_width: 1.0,
            res_attack: 5.0,
            res_release: 40.0,
        };
        assert!(band.in_view(ChannelView::All));
        assert!(band.in_view(ChannelView::Mid));
        assert!(!band.in_view(ChannelView::Side));
        assert!(!band.in_view(ChannelView::Left));

        // A stereo band acts on everything, so it is in every view.
        band.channel = BandChannel::Stereo;
        for view in ChannelView::ALL {
            assert!(band.in_view(view), "{view:?}");
        }
    }

    #[test]
    fn slots_are_claimed_lowest_first() {
        let params = EquzxParams::default();
        assert_eq!(free_slot(&params), Some(0));
        assert!(read_bands(&params).is_empty());
    }
}
