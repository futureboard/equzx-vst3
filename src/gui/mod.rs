//! The editor.
//!
//! EQUZX used to draw its UI in a webview: a Vite bundle baked into the binary,
//! served over a loopback listener, talking to the plugin across a JSON bridge.
//! This is the same interface built directly in egui, drawn by the same OpenGL
//! context baseview hands the editor — so the parameters are read where they
//! live rather than diffed and serialised sixty times a second, and the glass
//! and the glow are real shader passes rather than a browser's approximation of
//! them. See [`gpu`] for those.
//!
//! Layout is the one it always was: the plot runs edge to edge and everything
//! else floats over it, frosted, so the picture is never cut into by its own
//! chrome.

pub mod curves;
pub mod display;
pub mod edit;
pub mod gpu;
pub mod panels;
pub mod presets;
pub mod spectrum;
pub mod state;
pub mod theme;
pub mod widgets;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use nih_plug::prelude::*;
use nih_plug_egui::egui::{pos2, vec2, Align2, Color32, Context, Key, Mesh, Pos2, Rect, Shape, Ui};
use nih_plug_egui::{create_egui_editor, resizable_window::ResizableWindow};

use crate::analyzer::{Analyzer, Taps};
use crate::dsp::resonance::RES_BANDS;
use crate::gui::display::Display;
use crate::gui::edit::Frame;
use crate::gui::gpu::FxRenderer;
use crate::gui::panels::{header::HeaderState, Floating};
use crate::gui::state::{read_bands, UiState, PANEL_MIN};
use crate::gui::theme::{fade, MOCHI, NEON, PLOT_BOTTOM, PLOT_TOP};
use crate::meters::Meters;
use crate::params::{EquzxParams, TransientState, MAX_BANDS};

/// Opening size, and the floor a resize is clamped to.
pub const DEFAULT_WIDTH: u32 = 1400;
pub const DEFAULT_HEIGHT: u32 = 900;
pub const MIN_WIDTH: f32 = 640.0;
pub const MIN_HEIGHT: f32 = 420.0;

/// Inset of the floating panels from the window edge.
const INSET: f32 = 12.0;
/// Clearance between a floating panel and the plot underneath it.
const CLEARANCE: f32 = 20.0;

pub struct EditorContext {
    pub params: Arc<EquzxParams>,
    pub transient: Arc<TransientState>,
    pub taps: Arc<Taps>,
    pub meters: Arc<Meters>,
    /// Published by the audio thread on `initialize`.
    pub sample_rate: Arc<AtomicF32>,
}

/// Everything the editor keeps between frames.
///
/// The analyser is held apart from the rest so a frame can read its curves while
/// still mutating everything else — it is the one piece the UI borrows from and
/// writes around at the same time.
struct App {
    analyzer: Analyzer,
    readings: Readings,
    view: View,
}

/// This frame's meter values, filled before the frame is built and read from
/// while it runs.
struct Readings {
    level: Vec<f32>,
    delta: Vec<f32>,
    resonance: Vec<f32>,
    resonance_peak: f32,
}

struct View {
    fx: Arc<FxRenderer>,
    display: Display,
    header: HeaderState,
    ui: UiState,
    /// What was last written back to the session, so a frame that changed
    /// nothing does not touch the parameter.
    persisted: String,
    restored: bool,
    selected: Option<usize>,

    /// Heights measured last frame, which is what keeps the plot clear of two
    /// panels whose size depends on their own contents.
    header_height: f32,
    bottom_height: f32,
}

impl App {
    fn new(sample_rate: f32) -> Self {
        Self {
            analyzer: Analyzer::new(sample_rate),
            readings: Readings {
                level: vec![-100.0; MAX_BANDS],
                delta: vec![0.0; MAX_BANDS],
                resonance: vec![0.0; RES_BANDS],
                resonance_peak: 0.0,
            },
            view: View::new(sample_rate),
        }
    }
}

impl View {
    fn new(sample_rate: f32) -> Self {
        Self {
            fx: Arc::new(FxRenderer::new()),
            display: Display::new(sample_rate),
            header: HeaderState::default(),
            ui: UiState::default(),
            persisted: String::new(),
            restored: false,
            selected: None,
            header_height: 42.0,
            bottom_height: 260.0,
        }
    }
}

pub fn create(ctx: EditorContext) -> Option<Box<dyn Editor>> {
    let state = ctx.params.editor_state.clone();
    let app = App::new(ctx.sample_rate.load(Ordering::Relaxed));

    create_egui_editor(
        state.clone(),
        app,
        |egui_ctx, _| theme::apply(egui_ctx),
        move |egui_ctx, setter, app| {
            let sample_rate = ctx.sample_rate.load(Ordering::Relaxed);
            app.analyzer.set_sample_rate(sample_rate);
            app.analyzer.analyze(&ctx.taps);

            // Split the borrow: the frame below reads the analyser's curves in
            // place while everything else is being written.
            let App {
                analyzer,
                readings,
                view,
            } = app;

            ctx.meters.read_into(&mut readings.level, &mut readings.delta);
            readings.resonance_peak = ctx.meters.read_resonance(&mut readings.resonance);

            // The view state belongs to the user once they have touched it, so
            // it is restored once and then owned here.
            if !view.restored {
                view.restored = true;
                if let Ok(raw) = ctx.params.ui_state.read() {
                    view.ui = UiState::load(&raw);
                    view.persisted = raw.clone();
                }
            }

            let (pre, post) = analyzer.curves();
            let fx = view.fx.clone();
            let frame = Frame {
                setter,
                params: &ctx.params,
                transient: &ctx.transient,
                level: &readings.level,
                delta: &readings.delta,
                resonance: &readings.resonance,
                resonance_peak: readings.resonance_peak,
                spectrum_pre: pre,
                spectrum_post: post,
                sample_rate,
                fx: &fx,
            };

            let bands = read_bands(&ctx.params);
            keyboard(egui_ctx, &frame, view, &bands);

            ResizableWindow::new("equzx-window")
                .min_size(vec2(MIN_WIDTH, MIN_HEIGHT))
                .show(egui_ctx, &state, |ui| {
                    layout(ui, &frame, view, &bands);
                });

            // Push the view state back for the session to hold, but only when
            // it moved: this runs every frame, and the parameter is a lock.
            let text = view.ui.save();
            if text != view.persisted {
                if let Ok(mut slot) = ctx.params.ui_state.write() {
                    *slot = text.clone();
                }
                view.persisted = text;
            }
        },
    )
}

fn layout(ui: &mut Ui, frame: &Frame, app: &mut View, bands: &[state::BandView]) {
    let full = ui.max_rect();
    backdrop(ui, full);

    // The plot fills what the two floating panels leave.
    let plot_rect = Rect::from_min_max(
        pos2(full.min.x, full.min.y + INSET + app.header_height + CLEARANCE),
        pos2(
            full.max.x,
            full.max.y - INSET - app.bottom_height - CLEARANCE,
        ),
    );
    if plot_rect.height() > 60.0 {
        app.display
            .show(ui, plot_rect, frame, bands, &app.ui, &mut app.selected);
        hint(ui, plot_rect, bands.is_empty());
    }

    // --- the two overlays, over the plot's upper corners ------------------
    let fx = frame.fx.clone();
    Floating::new("view-overlay", plot_rect.min + vec2(display::PAD_LEFT, INSET))
        .padding(vec2(5.0, 5.0))
        .radius(18.0)
        .show(ui.ctx(), &fx, |ui| {
            panels::overlays::view(ui, &mut app.ui, &fx)
        });
    Floating::new(
        "analyzer-overlay",
        pos2(plot_rect.max.x - INSET, plot_rect.min.y + INSET),
    )
    .pivot(Align2::RIGHT_TOP)
    .padding(vec2(5.0, 5.0))
    .radius(18.0)
    .show(ui.ctx(), &fx, |ui| {
        panels::overlays::analyzer(ui, &mut app.ui, &fx)
    });

    // --- header -----------------------------------------------------------
    let header = Floating::new("header", full.min + vec2(INSET, INSET))
        .width(full.width() - INSET * 2.0)
        .padding(vec2(8.0, 7.0))
        .radius(theme::R_PANEL as f32)
        .sheen(true)
        .show(ui.ctx(), &fx, |ui| {
            panels::header::show(ui, frame, &fx, &mut app.header, &mut app.ui)
        });
    app.header_height = header.height();

    // --- the bottom slab: resizer over band panel -------------------------
    let panel_max = (full.height() - 380.0).max(PANEL_MIN);
    let panel_height = app.ui.panel_height.clamp(PANEL_MIN, panel_max);
    let width = full.width() - INSET * 2.0;
    let bottom = Floating::new("bottom", pos2(full.min.x + INSET, full.max.y - INSET))
        .pivot(Align2::LEFT_BOTTOM)
        .width(width)
        .padding(vec2(0.0, 0.0))
        .radius(theme::R_PANEL as f32)
        .vertical()
        .show(ui.ctx(), &fx, |ui| {
            let mut height = panel_height;
            panels::band_strip::resizer(ui, width, &mut height, panel_max);
            app.ui.panel_height = height;
            panels::band_strip::show(
                ui,
                frame,
                &fx,
                bands,
                &mut app.selected,
                height,
                width,
            );
        });
    app.bottom_height = bottom.height();
}

/// The field the plot sits on: a vertical gradient, and the ambient light along
/// the top that the header picks up. Glass over a flat field reads as plastic.
fn backdrop(ui: &Ui, rect: Rect) {
    let painter = ui.painter();
    let mut mesh = Mesh::default();
    for (t, color) in [(0.0f32, PLOT_TOP), (1.0, PLOT_BOTTOM)] {
        let y = rect.min.y + rect.height() * t;
        mesh.colored_vertex(pos2(rect.min.x, y), color);
        mesh.colored_vertex(pos2(rect.max.x, y), color);
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    painter.add(Shape::Mesh(mesh.into()));

    painter.add(glow(
        pos2(rect.min.x + rect.width() * 0.18, rect.min.y),
        rect.width() * 0.62,
        fade(NEON, 0.18),
    ));
    painter.add(glow(
        pos2(rect.min.x + rect.width() * 0.88, rect.min.y),
        rect.width() * 0.5,
        fade(MOCHI, 0.10),
    ));
}

/// A soft radial wash, as a fan of triangles fading to nothing at the rim.
fn glow(centre: Pos2, radius: f32, color: Color32) -> Shape {
    const STEPS: usize = 28;
    let mut mesh = Mesh::default();
    mesh.colored_vertex(centre, color);
    for i in 0..=STEPS {
        // A half disc: these hang off the top edge, so the lower half would be
        // clipped away anyway.
        let a = std::f32::consts::PI * i as f32 / STEPS as f32;
        mesh.colored_vertex(
            pos2(centre.x + radius * a.cos(), centre.y + radius * 0.75 * a.sin()),
            Color32::TRANSPARENT,
        );
    }
    for i in 1..=STEPS as u32 {
        mesh.add_triangle(0, i, i + 1);
    }
    Shape::Mesh(mesh.into())
}

/// The one-line reminder under an empty display.
fn hint(ui: &Ui, rect: Rect, empty: bool) {
    if !empty {
        return;
    }
    ui.painter().text(
        pos2(rect.center().x, rect.min.y + rect.height() * 0.62),
        Align2::CENTER_CENTER,
        "Click the display to add a band · scroll a handle for Q · \
         right-drag a handle to solo · X swaps A/B",
        nih_plug_egui::egui::FontId::proportional(theme::SMALL),
        theme::white(64),
    );
}

/// The shortcuts the web build listened for, minus the ones that only meant
/// something to a page with its own transport.
fn keyboard(ctx: &Context, frame: &Frame, app: &mut View, bands: &[state::BandView]) {
    // Never while a preset name is being typed.
    if ctx.wants_keyboard_input() {
        return;
    }
    ctx.input(|i| {
        if i.key_pressed(Key::B) {
            edit::set_bool(
                frame.setter,
                &frame.params.bypass,
                !frame.params.bypass.value(),
            );
        }
        if i.key_pressed(Key::Escape) {
            app.selected = None;
        }
        if i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace) {
            if let Some(slot) = app.selected {
                edit::remove_band(frame, slot);
                app.selected = None;
            }
        }
        if i.key_pressed(Key::X) {
            let live = edit::capture(frame.params);
            let parked = app.ui.parked.clone();
            edit::apply_snapshot(frame, &parked);
            app.ui.parked = live;
            app.ui.slot = app.ui.slot.other();
        }
        // Step through the bands, so a band can be picked without the pointer.
        if i.key_pressed(Key::Tab) && !bands.is_empty() {
            let current = app
                .selected
                .and_then(|slot| bands.iter().position(|b| b.slot == slot));
            let next = match current {
                Some(index) => (index + 1) % bands.len(),
                None => 0,
            };
            app.selected = Some(bands[next].slot);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{BandChannel, BandKind, DynMode, Slope};
    use nih_plug_egui::egui::{Id, RawInput};

    #[test]
    fn the_default_window_is_within_its_own_limits() {
        assert!(DEFAULT_WIDTH as f32 >= MIN_WIDTH);
        assert!(DEFAULT_HEIGHT as f32 >= MIN_HEIGHT);
    }

    #[test]
    fn an_editor_state_persists_the_size_it_was_built_with() {
        let state = nih_plug_egui::EguiState::from_size(DEFAULT_WIDTH, DEFAULT_HEIGHT);
        assert_eq!(state.size(), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
        assert!(!state.is_open());
    }

    /// Stands in for the host during a headless pass.
    ///
    /// Writes go nowhere: nih-plug keeps its parameter setters crate-private, so
    /// there is no way for a test to observe one. That is fine here — nothing in
    /// a headless pass generates input, so nothing writes.
    struct HeadlessHost;

    impl GuiContext for HeadlessHost {
        fn plugin_api(&self) -> PluginApi {
            PluginApi::Clap
        }
        fn request_resize(&self) -> bool {
            true
        }
        unsafe fn raw_begin_set_parameter(&self, _param: ParamPtr) {}
        unsafe fn raw_set_parameter_normalized(&self, _param: ParamPtr, _normalized: f32) {}
        unsafe fn raw_end_set_parameter(&self, _param: ParamPtr) {}
        fn get_state(&self) -> PluginState {
            // Never reached: the UI reads parameters directly and keeps its own
            // snapshots, so it has no use for the host's serialized state.
            unimplemented!("the editor does not read host state")
        }
        fn set_state(&self, _state: PluginState) {}
    }

    fn band(slot: usize, kind: BandKind, freq: f32) -> state::BandView {
        state::BandView {
            slot,
            kind,
            channel: BandChannel::Stereo,
            freq,
            gain: 3.0,
            q: 1.4,
            slope: Slope::S48,
            enabled: true,
            dynamic: false,
            dyn_mode: DynMode::Above,
            dyn_range: -6.0,
            threshold: -24.0,
            attack: 20.0,
            release: 200.0,
            resonance: 40.0,
        }
    }

    /// One of everything that draws differently: a cut (slope row live, no gain
    /// and no Q), a dynamic bell (meter, travel marker, per-frame curve), and a
    /// disabled side-only shelf (badge, dimmed, hidden in most channel views).
    fn a_bit_of_everything() -> Vec<state::BandView> {
        let mut cut = band(0, BandKind::LowCut, 80.0);
        cut.gain = 0.0;

        let mut dynamic = band(1, BandKind::Bell, 1000.0);
        dynamic.dynamic = true;
        dynamic.dyn_mode = DynMode::Below;

        let mut side = band(2, BandKind::HighShelf, 8000.0);
        side.channel = BandChannel::Side;
        side.enabled = false;
        side.gain = -4.5;

        vec![cut, dynamic, side]
    }

    /// A whole frame of the UI, laid out and tessellated, with no GPU involved.
    ///
    /// This is the closest thing to opening the editor that a test can do. It
    /// will not say the pixels are right, but it does exercise every layout
    /// path, every widget id and the tessellator — which is what catches a
    /// panic, an id collision, or a rectangle that came out `NaN`.
    fn run_frame(
        ctx: &Context,
        params: &Arc<EquzxParams>,
        transient: &Arc<TransientState>,
        view: &mut View,
        bands: &[state::BandView],
    ) -> usize {
        let host = HeadlessHost;
        let setter = ParamSetter::new(&host);
        let level = vec![-18.0f32; MAX_BANDS];
        let delta = vec![-2.5f32; MAX_BANDS];
        let resonance = vec![3.0f32; RES_BANDS];
        let spectrum = vec![-60.0f32; crate::analyzer::LOG_POINTS];
        let fx = view.fx.clone();
        let frame = Frame {
            setter: &setter,
            params,
            transient,
            level: &level,
            delta: &delta,
            resonance: &resonance,
            resonance_peak: 4.2,
            spectrum_pre: &spectrum,
            spectrum_post: &spectrum,
            sample_rate: 48_000.0,
            fx: &fx,
        };

        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                vec2(DEFAULT_WIDTH as f32, DEFAULT_HEIGHT as f32),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            nih_plug_egui::egui::CentralPanel::default().show(ctx, |ui| {
                layout(ui, &frame, view, bands);
            });
        });
        // Tessellating is where a malformed shape actually blows up, so a frame
        // is not "laid out" until this has run.
        ctx.tessellate(output.shapes, 1.0).len()
    }

    fn fixture() -> (Context, Arc<EquzxParams>, Arc<TransientState>, View) {
        let ctx = Context::default();
        theme::apply(&ctx);
        (
            ctx,
            Arc::new(EquzxParams::default()),
            Arc::new(TransientState::default()),
            View::new(48_000.0),
        )
    }

    #[test]
    fn the_whole_editor_lays_out_and_tessellates() {
        let (ctx, params, transient, mut view) = fixture();

        // An empty EQ, twice: egui needs a second pass before sized areas
        // settle, and a first-frame-only test would miss anything that only
        // goes wrong once a rectangle is known.
        for _ in 0..2 {
            assert!(run_frame(&ctx, &params, &transient, &mut view, &[]) > 0);
        }

        let bands = a_bit_of_everything();
        for selected in [None, Some(0), Some(1), Some(2)] {
            view.selected = selected;
            assert!(
                run_frame(&ctx, &params, &transient, &mut view, &bands) > 0,
                "nothing drawn with {selected:?} selected"
            );
        }
    }

    /// A soloed band dims the others and lights the Solo pill, which is a
    /// separate path through both the plot and the panel.
    #[test]
    fn soloing_and_bypassing_still_draw() {
        let (ctx, params, transient, mut view) = fixture();
        let bands = a_bit_of_everything();
        view.selected = Some(1);

        transient.set_solo(Some(1));
        assert!(run_frame(&ctx, &params, &transient, &mut view, &bands) > 0);
        transient.set_solo(None);
        assert!(run_frame(&ctx, &params, &transient, &mut view, &bands) > 0);
    }

    /// Every popover, opened. These are separate layers with their own ids, and
    /// a collision between two of them would only ever show up here.
    #[test]
    fn every_popover_opens_without_colliding() {
        let (ctx, params, transient, mut view) = fixture();
        let bands = a_bit_of_everything();
        run_frame(&ctx, &params, &transient, &mut view, &bands);

        for id in [
            Id::new("preset-menu"),
            Id::new("more-menu"),
            Id::new("resonance-menu"),
        ] {
            ctx.data_mut(|d| d.insert_temp(id, true));
        }
        for _ in 0..2 {
            assert!(run_frame(&ctx, &params, &transient, &mut view, &bands) > 0);
        }
    }

    /// Every channel view, dB range and analyser mode, since each one
    /// re-derives the band cache or the axis.
    #[test]
    fn every_view_setting_draws() {
        let (ctx, params, transient, mut view) = fixture();
        let bands = a_bit_of_everything();

        for channel in state::ChannelView::ALL {
            for range in panels::overlays::DB_RANGES {
                for mode in state::AnalyzerMode::ALL {
                    view.ui.channel_view = channel;
                    view.ui.db_range = range;
                    view.ui.analyzer_mode = mode;
                    assert!(
                        run_frame(&ctx, &params, &transient, &mut view, &bands) > 0,
                        "nothing drawn for {channel:?} / {range} dB / {mode:?}"
                    );
                }
            }
        }
    }

    /// A window squeezed past the point where the plot fits at all. The panels
    /// have to keep laying out and the plot has to bow out rather than allocate
    /// a negative rectangle.
    #[test]
    fn a_window_too_small_for_the_plot_still_draws() {
        let (ctx, params, transient, mut view) = fixture();
        let bands = a_bit_of_everything();
        view.selected = Some(0);
        view.ui.panel_height = 4000.0;
        assert!(run_frame(&ctx, &params, &transient, &mut view, &bands) > 0);
    }
}
