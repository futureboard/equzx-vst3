//! The message contract between the plugin and the web UI.
//!
//! Two directions, deliberately asymmetric:
//!
//! * **UI → plugin** is a stream of [`Action`]s. The UI never guesses at
//!   normalized values; it sends plain ones in its own units (Hz, dB, ms) and
//!   the editor converts.
//! * **plugin → UI** is a [`StateMessage`] whenever the parameters change from
//!   anywhere (host automation, a preset recall, the DAW's own undo), plus a
//!   [`FrameMessage`] of analyser and meter data on a timer.
//!
//! Field names are camelCase throughout because the other end of this pipe is
//! TypeScript, and `dsp/bands.ts` already named these things.

use serde::{Deserialize, Serialize};

use crate::params::{BandChannel, BandKind, DynMode, EquzxParams, Slope, MAX_BANDS};

/// A partial band edit. Every field is optional: dragging a handle sends the one
/// value that moved, while adding a band or recalling a preset sends all of them.
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct BandPatch {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub channel: Option<String>,
    pub freq: Option<f32>,
    pub gain: Option<f32>,
    pub q: Option<f32>,
    pub slope: Option<u32>,
    pub enabled: Option<bool>,
    pub dynamic: Option<bool>,
    pub dyn_mode: Option<String>,
    pub dyn_range: Option<f32>,
    pub threshold: Option<f32>,
    pub attack: Option<f32>,
    pub release: Option<f32>,
}

/// One entry of a whole-state recall — an A/B swap, a preset, or an undo.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BandSlot {
    pub slot: usize,
    #[serde(flatten)]
    pub patch: BandPatch,
}

#[derive(Deserialize, Debug)]
// `rename_all` covers the variant tags; the fields inside them need their own
// rule, or `outputGain` would have to arrive as `output_gain`.
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    /// Sent once when the UI has mounted; answered with the full state.
    Init,
    Resize {
        width: u32,
        height: u32,
    },
    /// Claim a slot and give it a starting shape.
    AddBand {
        slot: usize,
        #[serde(default)]
        band: BandPatch,
    },
    /// Edit a live band. Only the fields present move. Nested rather than
    /// flattened because a band's own `type` would otherwise collide with the
    /// `type` that tags the action.
    SetBand {
        slot: usize,
        #[serde(default)]
        band: BandPatch,
    },
    RemoveBand {
        slot: usize,
    },
    /// `None` clears solo.
    Solo {
        slot: Option<usize>,
    },
    Bypass {
        value: bool,
    },
    OutputGain {
        value: f32,
    },
    /// Replace every band at once. Slots not listed are freed.
    LoadState {
        bands: Vec<BandSlot>,
        output_gain: f32,
    },
    /// Opaque view state the UI wants saved with the session.
    UiState {
        value: String,
    },
}

/// A band as the UI reads it. Mirrors the `Band` interface in `dsp/bands.ts`,
/// minus the fields the UI derives for itself.
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BandState {
    /// Slot index, which is also the band's `id` on the UI side.
    pub id: usize,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub channel: &'static str,
    pub freq: f32,
    pub gain: f32,
    pub q: f32,
    pub slope: u32,
    pub enabled: bool,
    pub dynamic: bool,
    pub dyn_mode: &'static str,
    pub dyn_range: f32,
    pub threshold: f32,
    pub attack: f32,
    pub release: f32,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StateMessage {
    #[serde(rename = "type")]
    pub msg: &'static str,
    /// Only the slots currently in use, low slot first.
    pub bands: Vec<BandState>,
    pub output_gain: f32,
    pub bypass: bool,
    pub sample_rate: f32,
    pub max_bands: usize,
    /// Whatever the UI last asked to persist, verbatim.
    pub ui: String,
}

/// Analyser curves and meters. Sent on a timer, never diffed — it is all
/// time-varying by nature.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FrameMessage {
    #[serde(rename = "type")]
    pub msg: &'static str,
    /// Base64 of one byte per log-spaced point, pre-EQ.
    pub pre: String,
    /// The same, post-EQ.
    pub post: String,
    /// Per-slot band level in dBFS, indexed by slot.
    pub level: Vec<f32>,
    /// Per-slot dynamic gain offset in dB, indexed by slot.
    pub delta: Vec<f32>,
}

/// Read the parameters into the shape the UI expects.
pub fn state_message(params: &EquzxParams, ui: String, sample_rate: f32) -> StateMessage {
    let mut bands = Vec::with_capacity(MAX_BANDS);
    for (slot, p) in params.bands.iter().enumerate() {
        if !p.active.value() {
            continue;
        }
        bands.push(BandState {
            id: slot,
            kind: p.kind.value().as_wire(),
            channel: p.channel.value().as_wire(),
            freq: p.freq.value(),
            gain: p.gain.value(),
            q: p.q.value(),
            slope: p.slope.value().db_per_oct(),
            enabled: p.enabled.value(),
            dynamic: p.dynamic.value(),
            dyn_mode: p.dyn_mode.value().as_wire(),
            dyn_range: p.dyn_range.value(),
            threshold: p.threshold.value(),
            attack: p.attack.value(),
            release: p.release.value(),
        });
    }

    StateMessage {
        msg: "state",
        bands,
        output_gain: params.output_gain.value(),
        bypass: params.bypass.value(),
        sample_rate,
        max_bands: MAX_BANDS,
        ui,
    }
}

/// Parsed, validated form of a [`BandPatch`] field. Anything the UI sends that
/// isn't a known variant is dropped rather than defaulted, so a typo can't
/// silently turn a low cut into a bell.
pub fn parse_kind(s: &str) -> Option<BandKind> {
    BandKind::from_wire(s)
}

pub fn parse_channel(s: &str) -> Option<BandChannel> {
    BandChannel::from_wire(s)
}

pub fn parse_dyn_mode(s: &str) -> Option<DynMode> {
    DynMode::from_wire(s)
}

pub fn parse_slope(v: u32) -> Option<Slope> {
    Slope::from_db_per_oct(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_parses_as_a_single_field_patch() {
        let action: Action =
            serde_json::from_str(r#"{"type":"setBand","slot":3,"band":{"freq":250.5}}"#).unwrap();
        match action {
            Action::SetBand { slot, band } => {
                assert_eq!(slot, 3);
                assert_eq!(band.freq, Some(250.5));
                assert_eq!(band.gain, None);
                assert_eq!(band.kind, None);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_bands_own_type_survives_alongside_the_action_tag() {
        let action: Action = serde_json::from_str(
            r#"{"type":"setBand","slot":0,"band":{"type":"notch","dynMode":"below"}}"#,
        )
        .unwrap();
        match action {
            Action::SetBand { band, .. } => {
                assert_eq!(band.kind.as_deref(), Some("notch"));
                assert_eq!(band.dyn_mode.as_deref(), Some("below"));
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn adding_a_band_carries_a_nested_shape() {
        let action: Action = serde_json::from_str(
            r#"{"type":"addBand","slot":7,"band":{"type":"lowcut","slope":48,"freq":80}}"#,
        )
        .unwrap();
        match action {
            Action::AddBand { slot, band } => {
                assert_eq!(slot, 7);
                assert_eq!(band.kind.as_deref(), Some("lowcut"));
                assert_eq!(band.slope, Some(48));
                assert_eq!(band.freq, Some(80.0));
                assert_eq!(band.q, None);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn clearing_solo_is_a_null_slot() {
        let action: Action = serde_json::from_str(r#"{"type":"solo","slot":null}"#).unwrap();
        match action {
            Action::Solo { slot } => assert_eq!(slot, None),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_whole_state_recall_round_trips() {
        let action: Action = serde_json::from_str(
            r#"{"type":"loadState","outputGain":-3,"bands":[
                 {"slot":0,"type":"bell","freq":1000,"gain":4},
                 {"slot":5,"type":"highshelf","freq":8000,"gain":-2}]}"#,
        )
        .unwrap();
        match action {
            Action::LoadState { bands, output_gain } => {
                assert_eq!(output_gain, -3.0);
                assert_eq!(bands.len(), 2);
                assert_eq!(bands[1].slot, 5);
                assert_eq!(bands[1].patch.kind.as_deref(), Some("highshelf"));
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn unknown_actions_are_rejected_rather_than_guessed_at() {
        assert!(serde_json::from_str::<Action>(r#"{"type":"launchMissiles"}"#).is_err());
    }

    #[test]
    fn enum_names_match_the_typescript_side() {
        for (name, kind) in [
            ("lowcut", BandKind::LowCut),
            ("lowshelf", BandKind::LowShelf),
            ("bell", BandKind::Bell),
            ("notch", BandKind::Notch),
            ("bandpass", BandKind::BandPass),
            ("highshelf", BandKind::HighShelf),
            ("highcut", BandKind::HighCut),
        ] {
            assert_eq!(parse_kind(name), Some(kind));
            assert_eq!(kind.as_wire(), name);
        }
        assert_eq!(parse_kind("sideways"), None);
        assert_eq!(parse_channel("mid"), Some(BandChannel::Mid));
        assert_eq!(parse_dyn_mode("above"), Some(DynMode::Above));
        assert_eq!(parse_slope(96), Some(Slope::S96));
        assert_eq!(parse_slope(13), None);
    }

    #[test]
    fn a_default_state_message_serializes_with_no_bands() {
        let params = EquzxParams::default();
        let msg = state_message(&params, "{}".into(), 48_000.0);
        assert!(msg.bands.is_empty());
        assert_eq!(msg.max_bands, MAX_BANDS);

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"state""#));
        assert!(json.contains(r#""outputGain":0"#));
        assert!(json.contains(r#""maxBands":24"#));
    }
}
