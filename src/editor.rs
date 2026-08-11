//! The webview editor: assets, IPC, and the frame loop.
//!
//! The UI is a Vite build embedded in the binary and served by a loopback HTTP
//! listener (see [`crate::assets`]), so the plugin has no external files and
//! nothing to install.
//!
//! Per frame the loop does three things: drain whatever the UI sent, push a
//! fresh [`StateMessage`] if the parameters moved *from anywhere else*, and — on
//! a 30 Hz timer — push analyser curves and band meters.
//!
//! The "from anywhere else" is the subtle part. Every UI action sets parameters
//! immediately, so echoing the resulting state back would hand the UI its own
//! drag a frame late and make the whole session churn React state sixty times a
//! second. Instead a frame that processed a parameter-setting action updates the
//! cached snapshot *without* sending it: the UI already knows.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nih_plug::prelude::*;
use nih_plug_webview::{HTMLSource, WebViewEditor};

use crate::analyzer::{Analyzer, Taps};
use crate::assets::AssetServer;
use crate::meters::Meters;
use crate::params::{BandParams, EquzxParams, TransientState, MAX_BANDS};
use crate::protocol::{
    parse_channel, parse_dyn_mode, parse_kind, parse_slope, state_message, Action, BandPatch,
    FrameMessage, StateMessage,
};

/// Wide enough for the band editor's full control row — type, frequency,
/// gain, Q, slope, all five channels, and the on/solo/delete group.
pub const DEFAULT_WIDTH: u32 = 1260;
pub const DEFAULT_HEIGHT: u32 = 760;
/// Analyser frames per second. The curve is smoothed across frames anyway, so
/// past this the extra traffic buys nothing the eye can use.
const FRAME_HZ: u64 = 30;

/// Everything the frame loop mutates. The loop closure is `Fn`, not `FnMut`, so
/// this lives behind a lock — one only ever contended by the GUI thread itself.
struct EditorState {
    analyzer: Analyzer,
    /// Last state we told the UI about, as JSON.
    cache: Option<StateMessage>,
    last_frame: Instant,
    level: Vec<f32>,
    delta: Vec<f32>,
}

pub struct EditorContext {
    pub params: Arc<EquzxParams>,
    pub transient: Arc<TransientState>,
    pub taps: Arc<Taps>,
    pub meters: Arc<Meters>,
    /// Published by the audio thread on `initialize`.
    pub sample_rate: Arc<AtomicF32>,
}

/// Where the editor loads its UI from.
///
/// Normally the bundle baked into the binary. Setting `EQUZX_UI_URL` points it
/// at something else instead — a Vite dev server, typically, which turns the
/// usual edit/rebuild/reload cycle into a hot reload.
fn ui_source(server: Option<&AssetServer>) -> HTMLSource {
    let url = match std::env::var("EQUZX_UI_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => match server {
            Some(server) => server.index_url(),
            // Without a listener there is nothing to show. Say so on the page
            // rather than opening a window that is blank for no stated reason.
            None => "data:text/html,<body style='background:%230b0b0d;color:%23fff;                     font:14px system-ui'>EQUZX could not open its UI server.</body>"
                .to_owned(),
        },
    };
    // The editor outlives any borrow we could give it and this is read once per
    // editor, so leaking the string is the honest way to say 'static.
    HTMLSource::URL(Box::leak(url.into_boxed_str()))
}

pub fn create(ctx: EditorContext) -> WebViewEditor {
    let (width, height) = persisted_size(&ctx.params);

    let server = match AssetServer::start() {
        Ok(server) => Some(server),
        Err(err) => {
            nih_log!("EQUZX: could not start the UI server: {}", err);
            None
        }
    };
    let source = ui_source(server.as_ref());

    let state = Mutex::new(EditorState {
        analyzer: Analyzer::new(ctx.sample_rate.load(Ordering::Relaxed)),
        cache: None,
        last_frame: Instant::now(),
        level: vec![-100.0; MAX_BANDS],
        delta: vec![0.0; MAX_BANDS],
    });

    WebViewEditor::new(source, (width, height))
        .with_background_color((11, 11, 13, 255))
        .with_developer_mode(cfg!(debug_assertions))
        .with_event_loop(move |wv, setter, window| {
            // Held for the life of the editor: dropping the server would close
            // the listener the page is still loading its assets from.
            let _server = &server;
            let mut st = match state.lock() {
                Ok(st) => st,
                // A panic in a previous frame poisoned the lock; the data behind
                // it is still perfectly usable, and dropping the editor here
                // would be a worse outcome than carrying on.
                Err(poisoned) => poisoned.into_inner(),
            };
            // Reborrow through the guard so the fields can be borrowed apart —
            // `DerefMut` alone hands out one borrow of the whole struct.
            let st = &mut *st;

            let sr = ctx.sample_rate.load(Ordering::Relaxed);
            st.analyzer.set_sample_rate(sr);

            let mut ui_originated = false;
            let mut send_state = false;

            while let Ok(value) = wv.next_event() {
                let action: Action = match serde_json::from_value(value) {
                    Ok(action) => action,
                    // A message we don't understand is a UI bug, not a reason to
                    // take the editor down with a panic.
                    Err(err) => {
                        nih_debug_assert_failure!("unparseable UI message: {}", err);
                        continue;
                    }
                };

                match action {
                    Action::Init => send_state = true,
                    Action::Resize { width, height } => {
                        wv.resize(window, width.max(640), height.max(420));
                    }
                    Action::AddBand { slot, band } => {
                        if let Some(p) = ctx.params.bands.get(slot) {
                            reset_band(&setter, p);
                            apply_patch(&setter, p, &band);
                            set_bool(&setter, &p.active, true);
                            ui_originated = true;
                        }
                    }
                    Action::SetBand { slot, band } => {
                        if let Some(p) = ctx.params.bands.get(slot) {
                            apply_patch(&setter, p, &band);
                            ui_originated = true;
                        }
                    }
                    Action::RemoveBand { slot } => {
                        if let Some(p) = ctx.params.bands.get(slot) {
                            set_bool(&setter, &p.active, false);
                            ui_originated = true;
                        }
                        if ctx.transient.solo() == Some(slot) {
                            ctx.transient.set_solo(None);
                        }
                    }
                    Action::Solo { slot } => {
                        ctx.transient.set_solo(slot.filter(|s| *s < MAX_BANDS));
                    }
                    Action::Bypass { value } => {
                        set_bool(&setter, &ctx.params.bypass, value);
                        ui_originated = true;
                    }
                    Action::OutputGain { value } => {
                        set_float(&setter, &ctx.params.output_gain, value);
                        ui_originated = true;
                    }
                    Action::LoadState { bands, output_gain } => {
                        // Everything not named in the recall goes away, so an A/B
                        // swap can't leave a stray band from the other slot behind.
                        let mut wanted = [false; MAX_BANDS];
                        for entry in &bands {
                            if entry.slot < MAX_BANDS {
                                wanted[entry.slot] = true;
                            }
                        }
                        for (slot, p) in ctx.params.bands.iter().enumerate() {
                            if !wanted[slot] && p.active.value() {
                                set_bool(&setter, &p.active, false);
                            }
                        }
                        for entry in &bands {
                            if let Some(p) = ctx.params.bands.get(entry.slot) {
                                reset_band(&setter, p);
                                apply_patch(&setter, p, &entry.patch);
                                set_bool(&setter, &p.active, true);
                            }
                        }
                        set_float(&setter, &ctx.params.output_gain, output_gain);
                        ctx.transient.set_solo(None);
                        ctx.transient.flush.store(true, Ordering::Relaxed);
                        ui_originated = true;
                    }
                    Action::UiState { value } => {
                        if let Ok(mut ui) = ctx.params.ui_state.write() {
                            *ui = value;
                        }
                        // View state is part of the snapshot the UI is compared
                        // against, and this one came from the UI — echoing it
                        // back would churn the whole session for nothing.
                        ui_originated = true;
                    }
                }
            }

            // --- parameter sync ---------------------------------------------
            let ui = ctx
                .params
                .ui_state
                .read()
                .map(|s| s.clone())
                .unwrap_or_default();
            let current = state_message(&ctx.params, ui, sr);
            if st.cache.as_ref() != Some(&current) {
                let changed_elsewhere = !ui_originated;
                st.cache = Some(current.clone());
                if send_state || changed_elsewhere {
                    if let Ok(value) = serde_json::to_value(&current) {
                        wv.send_json(value);
                    }
                }
            } else if send_state {
                if let Ok(value) = serde_json::to_value(&current) {
                    wv.send_json(value);
                }
            }

            // --- analyser + meters ------------------------------------------
            if st.last_frame.elapsed() >= Duration::from_millis(1000 / FRAME_HZ) {
                st.last_frame = Instant::now();
                let (pre, post) = st.analyzer.analyze(&ctx.taps);
                ctx.meters.read_into(&mut st.level, &mut st.delta);
                let frame = FrameMessage {
                    msg: "frame",
                    pre,
                    post,
                    level: st.level.iter().map(|v| round1(*v)).collect(),
                    delta: st.delta.iter().map(|v| round1(*v)).collect(),
                };
                if let Ok(value) = serde_json::to_value(&frame) {
                    wv.send_json(value);
                }
            }
        })
}

/// One decimal is well under what the meters can show, and it keeps the JSON
/// for 48 numbers down to a couple of hundred bytes.
fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

/// Read the window size the UI last persisted, so a reopened editor comes back
/// the size the user left it.
fn persisted_size(params: &EquzxParams) -> (u32, u32) {
    let Ok(ui) = params.ui_state.read() else {
        return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&ui) else {
        return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
    };
    let read = |key: &str, fallback: u32, min: u32| {
        value
            .get(key)
            .and_then(|v| v.as_u64())
            .map(|v| (v as u32).max(min))
            .unwrap_or(fallback)
    };
    (
        read("width", DEFAULT_WIDTH, 640),
        read("height", DEFAULT_HEIGHT, 420),
    )
}

// --- parameter helpers ------------------------------------------------------

fn set_float(setter: &ParamSetter, param: &FloatParam, value: f32) {
    if !value.is_finite() {
        return;
    }
    setter.begin_set_parameter(param);
    setter.set_parameter(param, value);
    setter.end_set_parameter(param);
}

fn set_bool(setter: &ParamSetter, param: &BoolParam, value: bool) {
    setter.begin_set_parameter(param);
    setter.set_parameter(param, value);
    setter.end_set_parameter(param);
}

fn set_enum<T: Enum + PartialEq>(setter: &ParamSetter, param: &EnumParam<T>, value: T) {
    setter.begin_set_parameter(param);
    setter.set_parameter(param, value);
    setter.end_set_parameter(param);
}

/// Put a slot back to its defaults before a recall writes into it, so a preset
/// that omits a field gets the default rather than whatever the last band left.
fn reset_band(setter: &ParamSetter, p: &BandParams) {
    setter.begin_set_parameter(&p.enabled);
    setter.set_parameter(&p.enabled, p.enabled.default_plain_value());
    setter.end_set_parameter(&p.enabled);

    set_enum(setter, &p.kind, p.kind.default_plain_value());
    set_enum(setter, &p.channel, p.channel.default_plain_value());
    set_enum(setter, &p.slope, p.slope.default_plain_value());
    set_enum(setter, &p.dyn_mode, p.dyn_mode.default_plain_value());
    set_float(setter, &p.freq, p.freq.default_plain_value());
    set_float(setter, &p.gain, p.gain.default_plain_value());
    set_float(setter, &p.q, p.q.default_plain_value());
    set_float(setter, &p.dyn_range, p.dyn_range.default_plain_value());
    set_float(setter, &p.threshold, p.threshold.default_plain_value());
    set_float(setter, &p.attack, p.attack.default_plain_value());
    set_float(setter, &p.release, p.release.default_plain_value());
    set_bool(setter, &p.dynamic, p.dynamic.default_plain_value());
}

/// Apply the fields a patch actually carries. Values outside a parameter's range
/// are clamped by nih-plug; values it can't interpret at all are dropped here.
fn apply_patch(setter: &ParamSetter, p: &BandParams, patch: &BandPatch) {
    if let Some(kind) = patch.kind.as_deref().and_then(parse_kind) {
        set_enum(setter, &p.kind, kind);
    }
    if let Some(channel) = patch.channel.as_deref().and_then(parse_channel) {
        set_enum(setter, &p.channel, channel);
    }
    if let Some(mode) = patch.dyn_mode.as_deref().and_then(parse_dyn_mode) {
        set_enum(setter, &p.dyn_mode, mode);
    }
    if let Some(slope) = patch.slope.and_then(parse_slope) {
        set_enum(setter, &p.slope, slope);
    }
    if let Some(v) = patch.freq {
        set_float(setter, &p.freq, v);
    }
    if let Some(v) = patch.gain {
        set_float(setter, &p.gain, v);
    }
    if let Some(v) = patch.q {
        set_float(setter, &p.q, v);
    }
    if let Some(v) = patch.dyn_range {
        set_float(setter, &p.dyn_range, v);
    }
    if let Some(v) = patch.threshold {
        set_float(setter, &p.threshold, v);
    }
    if let Some(v) = patch.attack {
        set_float(setter, &p.attack, v);
    }
    if let Some(v) = patch.release {
        set_float(setter, &p.release, v);
    }
    if let Some(v) = patch.enabled {
        set_bool(setter, &p.enabled, v);
    }
    if let Some(v) = patch.dynamic {
        set_bool(setter, &p.dynamic, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_ui_state_falls_back_to_the_default_size() {
        let params = EquzxParams::default();
        assert_eq!(persisted_size(&params), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn a_persisted_size_is_read_back_and_floored() {
        let params = EquzxParams::default();
        *params.ui_state.write().unwrap() = r#"{"width":1600,"height":900}"#.into();
        assert_eq!(persisted_size(&params), (1600, 900));

        // A size no usable UI could fit in is clamped rather than honoured.
        *params.ui_state.write().unwrap() = r#"{"width":10,"height":10}"#.into();
        assert_eq!(persisted_size(&params), (640, 420));
    }

    #[test]
    fn garbage_ui_state_does_not_take_the_editor_down() {
        let params = EquzxParams::default();
        *params.ui_state.write().unwrap() = "not json at all".into();
        assert_eq!(persisted_size(&params), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }
}
