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

pub mod anim;
pub mod curves;
pub mod display;
pub mod edit;
pub mod gpu;
pub mod panels;
pub mod presets;
#[cfg(test)]
pub mod preview;
pub mod spectrum;
pub mod state;
pub mod theme;
pub mod tune;
pub mod widgets;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use nih_plug::prelude::*;
use nih_plug_egui::egui::{
    pos2, vec2, Align2, Color32, Context, Key, Mesh, Pos2, Rect, Shape, Ui, Vec2,
};
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
            header_height: 54.0,
            bottom_height: 244.0,
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
            tune::window(egui_ctx);

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

    // --- the analyser pickers, over the plot's top-right corner -----------
    // Held at rest opacity until pointed at, so they don't compete with the
    // spectrum they describe.
    let fx = frame.fx.clone();
    Floating::new(
        "analyzer-overlay",
        pos2(plot_rect.max.x - INSET, plot_rect.min.y + INSET),
    )
    .pivot(Align2::RIGHT_TOP)
    .padding(vec2(4.0, 4.0))
    .radius(20.0)
    .dim(0.55)
    .intro(0.07)
    .glass(gpu::Glass::panel(0.7))
    .show(ui.ctx(), &fx, |ui| {
        panels::overlays::analyzer(ui, &mut app.ui, &fx)
    });

    // --- header -----------------------------------------------------------
    // The most forward pane of glass in the stack, so the most reflective.
    let header = Floating::new("header", full.min + vec2(INSET, INSET))
        .width(full.width() - INSET * 2.0)
        .padding(vec2(10.0, 11.0))
        .radius(theme::R_PANEL as f32)
        .sheen(true)
        .intro(0.0)
        .glass(gpu::Glass::panel(1.0))
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
        .intro(0.14)
        .glass(gpu::Glass::panel(0.8))
        .show(ui.ctx(), &fx, |ui| {
            // The resizer sits flush on the strip, one slab — no gap.
            ui.spacing_mut().item_spacing.y = 0.0;
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
        vec2(rect.width() * 0.42, rect.height() * 0.30),
        fade(NEON, 0.20),
    ));
    painter.add(glow(
        pos2(rect.min.x + rect.width() * 0.88, rect.min.y),
        vec2(rect.width() * 0.30, rect.height() * 0.26),
        fade(MOCHI, 0.11),
    ));
}

/// A soft radial wash hanging off the top edge.
///
/// Concentric rings rather than a single fan out to the rim: a fan interpolates
/// the alpha linearly, which spreads a faint tint across the whole radius and
/// reads as a wash over the display rather than as light falling on the bar. The
/// falloff below is cubic, so it is gone well before the rim — which is what the
/// `transparent 60%` stop of the gradient this replaces was doing.
fn glow(centre: Pos2, radius: Vec2, color: Color32) -> Shape {
    const STEPS: usize = 32;
    const RINGS: usize = 5;

    let mut mesh = Mesh::default();
    mesh.colored_vertex(centre, color);
    for ring in 1..=RINGS {
        let t = ring as f32 / RINGS as f32;
        let alpha = (1.0 - t).powi(3);
        for step in 0..=STEPS {
            // A half disc: these sit on the top edge, so the lower half would be
            // outside the window anyway.
            let a = std::f32::consts::PI * step as f32 / STEPS as f32;
            mesh.colored_vertex(
                pos2(
                    centre.x + radius.x * t * a.cos(),
                    centre.y + radius.y * t * a.sin(),
                ),
                theme::fade(color, (color.a() as f32 / 255.0) * alpha),
            );
        }
    }

    let ring_start = |ring: usize| 1 + (ring - 1) as u32 * (STEPS as u32 + 1);
    for step in 0..STEPS as u32 {
        // The innermost ring fans from the centre vertex.
        mesh.add_triangle(0, ring_start(1) + step, ring_start(1) + step + 1);
    }
    for ring in 1..RINGS {
        let (inner, outer) = (ring_start(ring), ring_start(ring + 1));
        for step in 0..STEPS as u32 {
            mesh.add_triangle(inner + step, outer + step, inner + step + 1);
            mesh.add_triangle(inner + step + 1, outer + step, outer + step + 1);
        }
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
    pub(super) struct HeadlessHost;

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
    pub(super) fn a_bit_of_everything() -> Vec<state::BandView> {
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
    /// Deliberately `begin_pass`/`end_pass` rather than `Context::run`, and
    /// through `ResizableWindow`, because that is exactly what `egui-baseview`
    /// does. `run` resolves egui's sizing passes internally by repeating the
    /// closure; the integration does not, and a test that used it would report a
    /// settled layout the plugin never actually gets.
    fn run_frame(
        harness: &Harness,
        view: &mut View,
        bands: &[state::BandView],
    ) -> Vec<nih_plug_egui::egui::ClippedPrimitive> {
        let host = HeadlessHost;
        let setter = ParamSetter::new(&host);
        let level = vec![-18.0f32; MAX_BANDS];
        let delta = vec![-2.5f32; MAX_BANDS];
        let resonance = vec![3.0f32; RES_BANDS];
        let spectrum = vec![-60.0f32; crate::analyzer::LOG_POINTS];
        let fx = view.fx.clone();
        let frame = Frame {
            setter: &setter,
            params: &harness.params,
            transient: &harness.transient,
            level: &level,
            delta: &delta,
            resonance: &resonance,
            resonance_peak: 4.2,
            spectrum_pre: &spectrum,
            spectrum_post: &spectrum,
            sample_rate: 48_000.0,
            fx: &fx,
        };

        harness.ctx.begin_pass(RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, harness.size)),
            ..Default::default()
        });
        ResizableWindow::new("equzx-window")
            .min_size(vec2(MIN_WIDTH, MIN_HEIGHT))
            .show(&harness.ctx, &harness.editor_state, |ui| {
                layout(ui, &frame, view, bands);
            });
        let output = harness.ctx.end_pass();
        // Tessellating is where a malformed shape actually blows up, so a frame
        // is not "laid out" until this has run.
        harness.ctx.tessellate(output.shapes, 1.0)
    }

    /// Vertices produced anywhere inside `rect`.
    fn vertices_within(primitives: &[nih_plug_egui::egui::ClippedPrimitive], rect: Rect) -> usize {
        primitives
            .iter()
            .filter_map(|p| match &p.primitive {
                nih_plug_egui::egui::epaint::Primitive::Mesh(mesh) => Some(mesh),
                _ => None,
            })
            .flat_map(|mesh| mesh.vertices.iter())
            .filter(|v| rect.contains(v.pos))
            .count()
    }

    struct Harness {
        ctx: Context,
        params: Arc<EquzxParams>,
        transient: Arc<TransientState>,
        editor_state: Arc<nih_plug_egui::EguiState>,
        size: nih_plug_egui::egui::Vec2,
    }

    fn fixture() -> (Harness, View) {
        fixture_sized(DEFAULT_WIDTH as f32, DEFAULT_HEIGHT as f32)
    }

    fn fixture_sized(width: f32, height: f32) -> (Harness, View) {
        let ctx = Context::default();
        theme::apply(&ctx);
        (
            Harness {
                ctx,
                params: Arc::new(EquzxParams::default()),
                transient: Arc::new(TransientState::default()),
                editor_state: nih_plug_egui::EguiState::from_size(
                    width as u32,
                    height as u32,
                ),
                size: vec2(width, height),
            },
            View::new(48_000.0),
        )
    }

    /// The panels every frame is expected to put on screen.
    const PANELS: [&str; 3] = ["header", "bottom", "analyzer-overlay"];

    #[test]
    fn the_whole_editor_lays_out_and_tessellates() {
        let (harness, mut view) = fixture();

        // An empty EQ, twice: an `Area` spends its first pass measuring itself
        // and paints nothing, so a first-frame-only test would miss anything
        // that only goes wrong once a rectangle is known.
        for _ in 0..2 {
            assert!(!run_frame(&harness, &mut view, &[]).is_empty());
        }

        let bands = a_bit_of_everything();
        for selected in [None, Some(0), Some(1), Some(2)] {
            view.selected = selected;
            assert!(
                !run_frame(&harness, &mut view, &bands).is_empty(),
                "nothing drawn with {selected:?} selected"
            );
        }
    }

    /// The one that would have caught shipping an editor with no panels on it.
    ///
    /// Every floating panel has to settle at a sane rectangle, stay there, and
    /// actually put geometry inside it. All three failed at once when the panels
    /// were measured against a stand-in rectangle they were told to grow into:
    /// the header reported itself four thousand pixels tall, the overlays walked
    /// off the bottom of the window a screen at a time, and every panel drew
    /// nothing — while the plot underneath carried on looking perfectly fine.
    #[test]
    fn the_floating_panels_settle_and_paint() {
        let (harness, mut view) = fixture();
        let bands = a_bit_of_everything();
        view.selected = Some(1);

        let screen = Rect::from_min_size(Pos2::ZERO, harness.size);
        let mut previous: Option<Vec<Rect>> = None;

        // Pass 0 is egui's sizing pass, which paints nothing by design. From
        // pass 1 the layout has to be settled and stay settled.
        for pass in 0..5 {
            let primitives = run_frame(&harness, &mut view, &bands);
            let rects: Vec<Rect> = PANELS
                .iter()
                .map(|name| {
                    harness
                        .ctx
                        .memory(|m| m.area_rect(nih_plug_egui::egui::Id::new(*name)))
                        .unwrap_or_else(|| panic!("pass {pass}: no area for {name}"))
                })
                .collect();

            for (name, rect) in PANELS.iter().zip(&rects) {
                assert!(
                    !rect.any_nan(),
                    "pass {pass}: {name} is not a number: {rect:?}"
                );
                assert!(
                    screen.contains_rect(*rect),
                    "pass {pass}: {name} left the window: {rect:?} outside {screen:?}"
                );
                assert!(
                    rect.height() > 8.0 && rect.height() < harness.size.y * 0.5,
                    "pass {pass}: {name} is {} tall",
                    rect.height()
                );
            }

            if pass >= 1 {
                for (name, rect) in PANELS.iter().zip(&rects) {
                    assert!(
                        vertices_within(&primitives, *rect) > 8,
                        "pass {pass}: {name} laid out at {rect:?} but drew nothing"
                    );
                }
            }
            // Pass 1 is allowed to shift by the couple of points the header
            // gives back once it has been measured for the first time. After
            // that nothing may move on its own.
            if pass >= 2 {
                assert_eq!(
                    previous.as_ref(),
                    Some(&rects),
                    "pass {pass}: the panels moved without anything changing"
                );
            }
            previous = Some(rects);
        }
    }

    /// The plot has to end up between the two panels rather than under them,
    /// which is the other half of the same measurement.
    #[test]
    fn the_plot_keeps_clear_of_the_panels() {
        let (harness, mut view) = fixture();
        let bands = a_bit_of_everything();
        for _ in 0..3 {
            run_frame(&harness, &mut view, &bands);
        }

        let header = harness
            .ctx
            .memory(|m| m.area_rect(nih_plug_egui::egui::Id::new("header")))
            .unwrap();
        let bottom = harness
            .ctx
            .memory(|m| m.area_rect(nih_plug_egui::egui::Id::new("bottom")))
            .unwrap();

        let plot_top = INSET + view.header_height + CLEARANCE;
        let plot_bottom = harness.size.y - INSET - view.bottom_height - CLEARANCE;
        assert!(
            plot_top >= header.max.y,
            "the plot starts at {plot_top}, under a header ending at {}",
            header.max.y
        );
        assert!(
            plot_bottom <= bottom.min.y,
            "the plot ends at {plot_bottom}, under a panel starting at {}",
            bottom.min.y
        );
        assert!(
            plot_bottom - plot_top > 200.0,
            "the plot was squeezed to {} points",
            plot_bottom - plot_top
        );
    }

    /// A soloed band dims the others and lights the Solo pill, which is a
    /// separate path through both the plot and the panel.
    #[test]
    fn soloing_and_bypassing_still_draw() {
        let (harness, mut view) = fixture();
        let bands = a_bit_of_everything();
        view.selected = Some(1);

        harness.transient.set_solo(Some(1));
        assert!(!run_frame(&harness, &mut view, &bands).is_empty());
        harness.transient.set_solo(None);
        assert!(!run_frame(&harness, &mut view, &bands).is_empty());
    }

    /// Every popover, opened. These are separate layers with their own ids, and
    /// a collision between two of them would only ever show up here.
    #[test]
    fn every_popover_opens_without_colliding() {
        let (harness, mut view) = fixture();
        let bands = a_bit_of_everything();
        run_frame(&harness, &mut view, &bands);

        for id in [
            Id::new("preset-menu"),
            Id::new("more-menu"),
            Id::new("resonance-menu"),
        ] {
            harness.ctx.data_mut(|d| d.insert_temp(id, true));
        }
        for _ in 0..3 {
            assert!(!run_frame(&harness, &mut view, &bands).is_empty());
        }
        // A popover is only worth opening if it draws.
        let panel = harness
            .ctx
            .memory(|m| m.area_rect(Id::new("preset-menu").with("panel")));
        assert!(panel.is_some_and(|r| r.height() > 40.0), "{panel:?}");
    }

    /// Every channel view, dB range and analyser mode, since each one
    /// re-derives the band cache or the axis.
    #[test]
    fn every_view_setting_draws() {
        let (harness, mut view) = fixture();
        let bands = a_bit_of_everything();

        for channel in state::ChannelView::ALL {
            for range in panels::overlays::DB_RANGES {
                for mode in state::AnalyzerMode::ALL {
                    view.ui.channel_view = channel;
                    view.ui.db_range = range;
                    view.ui.analyzer_mode = mode;
                    assert!(
                        !run_frame(&harness, &mut view, &bands).is_empty(),
                        "nothing drawn for {channel:?} / {range} dB / {mode:?}"
                    );
                }
            }
        }
    }

    /// Every character the UI puts on screen has to exist in the bundled fonts.
    ///
    /// A character the font cannot find renders as an empty box, and nothing
    /// else catches it: it compiles, it lays out, it tessellates, and it is only
    /// wrong to look at. This shipped once — `A → B` in the header, with the
    /// arrow as a hollow rectangle — so the arrow is a drawn shape now and this
    /// keeps the next one from getting that far.
    #[test]
    fn every_character_the_ui_draws_has_a_glyph() {
        // Scanned out of the source rather than listed, so a new string with a
        // new character in it is covered without anyone remembering to add it.
        const SOURCES: [&str; 8] = [
            include_str!("mod.rs"),
            include_str!("display.rs"),
            include_str!("panels/header.rs"),
            include_str!("panels/band_strip.rs"),
            include_str!("panels/overlays.rs"),
            include_str!("panels/resonance.rs"),
            include_str!("widgets/chrome.rs"),
            include_str!("widgets/menu.rs"),
        ];

        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.begin_pass(RawInput::default());

        let font = nih_plug_egui::egui::FontId::proportional(theme::SMALL);
        let mut missing: Vec<char> = Vec::new();
        for source in SOURCES {
            for c in string_literal_chars(source) {
                if !c.is_ascii()
                    && !missing.contains(&c)
                    && !ctx.fonts(|f| f.has_glyph(&font, c))
                {
                    missing.push(c);
                }
            }
        }
        let _ = ctx.end_pass();

        assert!(
            missing.is_empty(),
            "the bundled fonts have no glyph for {missing:?} — these draw as empty boxes"
        );
    }

    /// Characters inside double-quoted literals, skipping escapes. Crude on
    /// purpose: it only has to be good enough to find the text the UI draws,
    /// and over-reporting a character from a comment would cost nothing anyway.
    fn string_literal_chars(source: &str) -> Vec<char> {
        let mut out = Vec::new();
        let mut inside = false;
        let mut escaped = false;
        for c in source.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' if inside => escaped = true,
                '"' => inside = !inside,
                _ if inside => out.push(c),
                _ => {}
            }
        }
        out
    }

    /// Render a frame to a PNG and look at it.
    ///
    /// Not run by default — it writes a file and takes a second — but it is the
    /// only thing in the suite that answers "what does it look like", so it
    /// lives with the tests rather than in someone's shell history.
    /// `cargo test --lib render_the_editor -- --ignored --nocapture`
    #[test]
    #[ignore = "writes a PNG; run explicitly"]
    fn render_the_editor() {
        let width = std::env::var("EQUZX_PREVIEW_W")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_WIDTH as f32);
        let height = std::env::var("EQUZX_PREVIEW_H")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_HEIGHT as f32);

        let (harness, mut view) = fixture_sized(width, height);
        let bands = a_bit_of_everything();
        view.selected = Some(1);

        // A few passes, because the first is egui measuring its areas.
        let mut atlas = None;
        let mut primitives = Vec::new();
        for _ in 0..4 {
            let host = HeadlessHost;
            let setter = ParamSetter::new(&host);
            let level = vec![-22.0f32; MAX_BANDS];
            let delta = vec![-3.5f32; MAX_BANDS];
            let mut resonance = vec![0.0f32; RES_BANDS];
            for (i, slot) in resonance.iter_mut().enumerate() {
                *slot = (i as f32 * 0.35).sin().abs() * 5.0;
            }
            // A spectrum with some shape to it, so the analyser has something
            // to draw other than a flat line on the floor.
            let spectrum: Vec<f32> = (0..crate::analyzer::LOG_POINTS)
                .map(|i| {
                    let t = i as f32 / crate::analyzer::LOG_POINTS as f32;
                    -34.0 - 46.0 * t + 9.0 * (t * 26.0).sin()
                })
                .collect();
            let fx = view.fx.clone();
            let frame = Frame {
                setter: &setter,
                params: &harness.params,
                transient: &harness.transient,
                level: &level,
                delta: &delta,
                resonance: &resonance,
                resonance_peak: 5.0,
                spectrum_pre: &spectrum,
                spectrum_post: &spectrum,
                sample_rate: 48_000.0,
                fx: &fx,
            };

            harness.ctx.begin_pass(RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, harness.size)),
                ..Default::default()
            });
            ResizableWindow::new("equzx-window")
                .min_size(vec2(MIN_WIDTH, MIN_HEIGHT))
                .show(&harness.ctx, &harness.editor_state, |ui| {
                    layout(ui, &frame, &mut view, &bands);
                });
            let output = harness.ctx.end_pass();
            if let Some(found) = preview::Atlas::from_delta(&output.textures_delta) {
                atlas = Some(found);
            }
            primitives = harness.ctx.tessellate(output.shapes, 1.0);
        }

        let atlas = atlas.expect("egui never uploaded its font atlas");
        let mut canvas =
            preview::Canvas::new(width as usize, height as usize, theme::SURFACE_ROOT);
        let skipped = canvas.draw(&primitives, &atlas);

        let path = std::env::var("EQUZX_PREVIEW")
            .unwrap_or_else(|_| "target/equzx-preview.png".to_owned());
        std::fs::write(&path, canvas.to_png()).expect("could not write the preview");
        println!(
            "wrote {path} ({width}x{height}), {} primitives, {skipped} GPU callbacks skipped",
            primitives.len()
        );
        for name in PANELS {
            println!(
                "  {name:<18} {:?}",
                harness.ctx.memory(|m| m.area_rect(Id::new(name)))
            );
        }
    }

    /// The window sizes a host is likely to hand the editor, including the
    /// floor and something much larger than the default.
    #[test]
    fn the_panels_hold_together_at_any_window_size() {
        for (width, height) in [
            (MIN_WIDTH, MIN_HEIGHT),
            (1400.0, 900.0),
            (1920.0, 1250.0),
            (2560.0, 1440.0),
        ] {
            let (harness, mut view) = fixture_sized(width, height);
            let bands = a_bit_of_everything();
            view.selected = Some(0);
            for _ in 0..3 {
                run_frame(&harness, &mut view, &bands);
            }
            let screen = Rect::from_min_size(Pos2::ZERO, harness.size);
            for name in PANELS {
                let rect = harness
                    .ctx
                    .memory(|m| m.area_rect(Id::new(name)))
                    .unwrap_or_else(|| panic!("{width}x{height}: no area for {name}"));
                assert!(
                    !rect.any_nan() && screen.contains_rect(rect),
                    "{width}x{height}: {name} at {rect:?}"
                );
            }
        }
    }

    /// A window squeezed past the point where the plot fits at all. The panels
    /// have to keep laying out and the plot has to bow out rather than allocate
    /// a negative rectangle.
    #[test]
    fn a_window_too_small_for_the_plot_still_draws() {
        let (harness, mut view) = fixture();
        let bands = a_bit_of_everything();
        view.selected = Some(0);
        view.ui.panel_height = 4000.0;
        assert!(!run_frame(&harness, &mut view, &bands).is_empty());
    }
}

