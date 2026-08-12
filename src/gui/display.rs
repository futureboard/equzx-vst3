//! The plot: grid, analyser, curves and handles.
//!
//! A port of `EQDisplay.tsx`, which was a canvas for the painted layers and an
//! SVG for the interactive ones. egui has no such split — everything here is
//! painted, and the handles are hit rectangles registered after the plot
//! background so they win the pointer.
//!
//! What the canvas got from `shadowBlur` around the composite curve, this gets
//! from a real screen-space bloom: a paint callback emitted once the curves are
//! down but before the handles and labels, so the neon glows and the text stays
//! sharp. See [`crate::gui::gpu`].

use std::collections::HashMap;

use nih_plug_egui::egui::{
    epaint::{PathShape, PathStroke},
    pos2, vec2, Align2, Color32, FontId, Mesh, PointerButton, Pos2, Rect, Sense, Shape, Stroke, Ui,
};

use crate::analyzer::{CEIL_DB, FLOOR_DB};
use crate::dsp::biquad::Coeffs;
use crate::dsp::resonance::{band_freq, RES_BANDS};
use crate::gui::curves::{self, ResponseGrid, CURVE_POINTS, F_MAX, F_MIN};
use crate::gui::edit::{self, Frame};
use crate::gui::gpu::Bloom;
use crate::gui::spectrum;
use crate::gui::state::{BandView, UiState};
use crate::gui::theme::{self, band_color, fade, white, MOCHI, NEON, SURFACE_DEEP};

/// Room around the plot for the two axes. Matches `PAD` in the original.
pub const PAD_TOP: f32 = 14.0;
pub const PAD_RIGHT: f32 = 14.0;
pub const PAD_BOTTOM: f32 = 26.0;
pub const PAD_LEFT: f32 = 40.0;

const FREQ_TICKS: [f32; 22] = [
    20.0, 30.0, 40.0, 50.0, 60.0, 80.0, 100.0, 200.0, 300.0, 400.0, 500.0, 600.0, 800.0, 1000.0,
    2000.0, 3000.0, 4000.0, 5000.0, 6000.0, 8000.0, 10000.0, 20000.0,
];
const LABELLED: [f32; 10] = [
    20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0,
];

/// The Q range a handle's scroll wheel and drag can reach.
const Q_MIN: f32 = 0.025;
const Q_MAX: f32 = 40.0;

pub fn fmt_freq(f: f32) -> String {
    if f >= 10_000.0 {
        format!("{:.0}k", f / 1000.0)
    } else if f >= 1000.0 {
        if (f % 1000.0).abs() < 0.5 {
            format!("{:.0}k", f / 1000.0)
        } else {
            format!("{:.1}k", f / 1000.0)
        }
    } else {
        format!("{:.0}", f)
    }
}

/// Everything the plot keeps between frames.
pub struct Display {
    grid: ResponseGrid,
    /// The static bands' responses, by slot. Dynamic bands are absent: their
    /// gain moves every frame, so they are recomputed rather than cached.
    per_band: HashMap<usize, Vec<f32>>,
    static_total: Vec<f32>,
    dynamic: Vec<usize>,
    /// What the cache above was built from.
    cached_for: Option<CacheKey>,

    sections: Vec<Coeffs>,
    total: Vec<f32>,
    band_curve: Vec<f32>,
    pre: spectrum::Scratch,
    post: spectrum::Scratch,

    /// The band a press created and is still dragging, if any.
    draft: Option<usize>,
    /// The band being auditioned with a right-drag, and what was soloed before.
    audition: Option<(usize, Option<usize>)>,
    hovered: Option<usize>,
}

#[derive(PartialEq)]
struct CacheKey {
    bands: Vec<BandView>,
    sample_rate: f32,
    bypassed: bool,
    view: crate::gui::state::ChannelView,
}

impl Display {
    pub fn new(sample_rate: f32) -> Self {
        let n = CURVE_POINTS + 1;
        Self {
            grid: ResponseGrid::new(sample_rate),
            per_band: HashMap::new(),
            static_total: vec![0.0; n],
            dynamic: Vec::new(),
            cached_for: None,
            sections: Vec::new(),
            total: vec![0.0; n],
            band_curve: vec![0.0; n],
            pre: spectrum::Scratch::new(),
            post: spectrum::Scratch::new(),
            draft: None,
            audition: None,
            hovered: None,
        }
    }

    /// The band under the pointer, if any — what the wheel and the band panel
    /// treat as focused when nothing is selected.
    pub fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    fn rebuild(&mut self, bands: &[BandView], frame: &Frame, ui_state: &UiState, bypassed: bool) {
        let key = CacheKey {
            bands: bands.to_vec(),
            sample_rate: frame.sample_rate,
            bypassed,
            view: ui_state.channel_view,
        };
        if self.cached_for.as_ref() == Some(&key) {
            return;
        }

        self.grid.set_sample_rate(frame.sample_rate);
        let n = self.grid.len();
        self.per_band.clear();
        self.static_total[..n].fill(0.0);
        self.dynamic.clear();

        for band in bands {
            if !band.in_view(ui_state.channel_view) {
                continue;
            }
            // A dynamic band's curve depends on this frame's gain reduction, so
            // it is left out of the cache entirely rather than cached and fixed.
            if band.dynamic && band.enabled && band.kind.uses_gain() && !bypassed {
                self.dynamic.push(band.slot);
                continue;
            }
            curves::band_sections(band, frame.sample_rate, &mut self.sections);
            let mut curve = vec![0.0f32; n];
            self.grid.curve(&self.sections, &mut curve);
            if band.enabled && !bypassed {
                for (total, db) in self.static_total.iter_mut().zip(curve.iter()) {
                    *total += db;
                }
            }
            self.per_band.insert(band.slot, curve);
        }

        self.cached_for = Some(key);
    }

    /// Draw and drive the plot.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        frame: &Frame,
        bands: &[BandView],
        ui_state: &UiState,
        selected: &mut Option<usize>,
    ) {
        let bypassed = frame.params.bypass.value();

        let plot = Rect::from_min_max(
            pos2(rect.min.x + PAD_LEFT, rect.min.y + PAD_TOP),
            pos2(rect.max.x - PAD_RIGHT, rect.max.y - PAD_BOTTOM),
        );
        if plot.width() < 20.0 || plot.height() < 20.0 {
            return;
        }

        // Sample the curves in screen space: one evaluation per physical
        // pixel column, whatever the window size and DPI, so a stroke segment
        // is never longer than a pixel and the curves stay smooth.
        let ppp = ui.ctx().pixels_per_point();
        let segments = ((plot.width() * ppp).round() as usize).clamp(256, 4096);
        self.grid.set_resolution(segments);
        if self.static_total.len() != self.grid.len() {
            let n = self.grid.len();
            self.static_total = vec![0.0; n];
            self.total = vec![0.0; n];
            self.band_curve = vec![0.0; n];
            // The cached band curves were evaluated on the old grid.
            self.cached_for = None;
        }

        self.rebuild(bands, frame, ui_state, bypassed);
        let axes = Axes {
            plot,
            db_range: ui_state.db_range,
        };

        // The plot's own background, registered first so every handle below
        // wins the pointer over it.
        let background = ui.interact(plot, ui.id().with("plot"), Sense::click_and_drag());

        let painter = ui.painter().with_clip_rect(rect);
        draw_grid(&painter, &axes);

        {
            let clipped = ui.painter().with_clip_rect(plot);
            if ui_state.analyzer_mode.draws_pre() {
                draw_spectrum(
                    &clipped,
                    &mut self.pre,
                    frame.spectrum_pre,
                    &axes,
                    ui_state.spectrum_smoothing,
                    SpectrumLayer::Pre,
                );
            }
            if ui_state.analyzer_mode.draws_post() {
                draw_spectrum(
                    &clipped,
                    &mut self.post,
                    frame.spectrum_post,
                    &axes,
                    ui_state.spectrum_smoothing,
                    SpectrumLayer::Post,
                );
            }

            // Resonance layers sit between the spectrum and the EQ response —
            // detection highlights, then live attenuation — so whatever the
            // suppressors are doing, the EQ curve stays on top of it.
            if !bypassed {
                if frame.resonance.iter().any(|c| *c > 0.05) {
                    draw_resonance(&clipped, &axes, frame.resonance);
                }
                draw_res_targets(
                    &clipped,
                    &axes,
                    frame.res_targets,
                    ui.ctx().pointer_hover_pos().filter(|p| plot.contains(*p)),
                );
            }

            let focus = self.hovered.or(*selected);
            self.draw_curves(&clipped, &axes, bands, frame, bypassed, focus);
        }

        // A restrained bloom over the curves, before the handles and labels go
        // down so the glow picks up the signal and not the typography. The
        // detailed edge light lives in the strokes now; this only lifts it.
        if !bypassed {
            let tune = crate::gui::tune::get();
            ui.painter().add(frame.fx.bloom(
                plot,
                Bloom {
                    tint: Color32::from_rgb(0xff, 0xa8, 0xd0),
                    intensity: 0.45 * tune.spectrum_glow,
                    threshold: 0.52,
                    levels: 3,
                },
            ));
        }

        self.interact(ui, &axes, &background, frame, bands, ui_state, selected);
        self.draw_handles(ui, &axes, bands, frame, ui_state, *selected);
        self.draw_readout(ui, &axes, bands, frame, ui_state, *selected);
    }

    fn draw_curves(
        &mut self,
        painter: &nih_plug_egui::egui::Painter,
        axes: &Axes,
        bands: &[BandView],
        frame: &Frame,
        bypassed: bool,
        focus: Option<usize>,
    ) {
        let n = self.grid.len();
        self.total[..n].copy_from_slice(&self.static_total[..n]);

        for (index, band) in bands.iter().enumerate() {
            let Some(curve) = self.per_band.get(&band.slot) else {
                continue;
            };
            if !band.enabled || bypassed {
                continue;
            }
            stroke_band(
                painter,
                &self.grid,
                curve,
                axes,
                band_color(index),
                focus == Some(band.slot),
            );
        }

        // Dynamic bands: rebuilt at the gain the reduction has them at right now.
        for slot in &self.dynamic {
            let Some((index, band)) = bands.iter().enumerate().find(|(_, b)| b.slot == *slot) else {
                continue;
            };
            let moved = BandView {
                gain: band.gain + frame.delta.get(*slot).copied().unwrap_or(0.0),
                ..*band
            };
            curves::band_sections(&moved, frame.sample_rate, &mut self.sections);
            self.grid.curve(&self.sections, &mut self.band_curve);
            for i in 0..n {
                self.total[i] += self.band_curve[i];
            }
            stroke_band(
                painter,
                &self.grid,
                &self.band_curve,
                axes,
                band_color(index),
                focus == Some(*slot),
            );
        }

        // The composite curve, and the field under it.
        let points: Vec<Pos2> = (0..n)
            .map(|i| pos2(axes.x(self.grid.freq(i)), axes.y(self.total[i])))
            .collect();
        painter.add(vertical_gradient_area(
            &points,
            axes.y(0.0),
            axes.plot,
            NEON,
            0.22,
            0.02,
        ));
        // Three strokes deep: a wide whisper, a pink inner glow, and the
        // crisp pale core — analytic light rather than screen-wide bloom.
        if !bypassed {
            let glow = crate::gui::tune::get().spectrum_glow;
            for (width, color) in [
                (7.0, fade(NEON, 0.08 * glow)),
                (3.2, fade(NEON, 0.26 * glow)),
            ] {
                painter.add(Shape::Path(PathShape {
                    points: points.clone(),
                    closed: false,
                    fill: Color32::TRANSPARENT,
                    stroke: PathStroke::new(width, color),
                }));
            }
        }
        painter.add(Shape::Path(PathShape {
            points,
            closed: false,
            fill: Color32::TRANSPARENT,
            stroke: PathStroke::new(
                1.8,
                if bypassed {
                    white(64)
                } else {
                    MOCHI
                },
            ),
        }));

        // Where each dynamic band currently sits, and how far it has travelled.
        for slot in &self.dynamic {
            let Some((index, band)) = bands.iter().enumerate().find(|(_, b)| b.slot == *slot) else {
                continue;
            };
            let delta = frame.delta.get(*slot).copied().unwrap_or(0.0);
            let color = band_color(index);
            let x = axes.x(band.freq);
            painter.line_segment(
                [pos2(x, axes.y(band.gain)), pos2(x, axes.y(band.gain + delta))],
                Stroke::new(1.0, fade(color, 0.5)),
            );
            painter.circle_filled(pos2(x, axes.y(band.gain + delta)), 3.0, color);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn interact(
        &mut self,
        ui: &mut Ui,
        axes: &Axes,
        background: &nih_plug_egui::egui::Response,
        frame: &Frame,
        bands: &[BandView],
        ui_state: &UiState,
        selected: &mut Option<usize>,
    ) {
        let can_add = bands.len() < crate::params::MAX_BANDS;

        // --- press on empty display creates a band and drags it into place ---
        //
        // Created on press rather than release, so it is on screen and following
        // the pointer for the whole gesture. Waiting for the click to finish
        // meant drawing blind and only seeing the result once the mouse was up.
        if background.drag_started_by(PointerButton::Primary) || background.clicked() {
            if let Some(pos) = background.interact_pointer_pos() {
                if can_add && self.draft.is_none() {
                    let slot = edit::add_band(
                        frame,
                        axes.freq_at(pos.x),
                        axes.db_at(pos.y),
                        ui_state.channel_view,
                    );
                    if let Some(slot) = slot {
                        *selected = Some(slot);
                        self.draft = Some(slot);
                    }
                }
            }
        }
        if background.dragged_by(PointerButton::Primary) {
            if let (Some(slot), Some(pos)) = (self.draft, background.interact_pointer_pos()) {
                // The band was created under the pointer, so it tracks it
                // outright rather than by delta — no drift over a long sweep.
                edit::set_freq(frame, slot, axes.freq_at(pos.x));
                edit::set_gain(frame, slot, axes.db_at(pos.y));
            }
        }
        if background.drag_stopped() || !background.dragged() {
            self.draft = None;
        }

        // --- handles -------------------------------------------------------
        let fine = ui.input(|i| i.modifiers.shift_only());
        let alt = ui.input(|i| i.modifiers.alt);
        let mut hovered = None;

        for band in bands {
            if !band.in_view(ui_state.channel_view) {
                continue;
            }
            let centre = pos2(axes.x(band.freq), axes.y(band.handle_db()));

            // The Q grips, which only the selected band shows.
            if *selected == Some(band.slot) && band.kind.uses_q() {
                let bandwidth = curves::q_to_octaves(band.q);
                for side in [-1.0f32, 1.0] {
                    let qx = axes.x(band.freq * 2f32.powf(side * bandwidth / 2.0));
                    if !qx.is_finite() {
                        continue;
                    }
                    let grip = Rect::from_center_size(pos2(qx, centre.y), vec2(12.0, 12.0));
                    let response = ui.interact(
                        grip,
                        ui.id().with(("q", band.slot, side as i32)),
                        Sense::drag(),
                    );
                    if response.dragged() {
                        let edge = qx + response.drag_delta().x;
                        let octaves = (axes.freq_at(edge) / band.freq).log2().abs() * 2.0;
                        edit::set_q(
                            frame,
                            band.slot,
                            curves::octaves_to_q(octaves).clamp(Q_MIN, Q_MAX),
                        );
                    }
                }
            }

            let hit = Rect::from_center_size(centre, vec2(28.0, 28.0));
            let response = ui.interact(
                hit,
                ui.id().with(("handle", band.slot)),
                Sense::click_and_drag(),
            );
            if response.hovered() {
                hovered = Some(band.slot);
            }

            if response.clicked() {
                *selected = Some(band.slot);
            }
            if response.double_clicked() {
                edit::remove_band(frame, band.slot);
                if *selected == Some(band.slot) {
                    *selected = None;
                }
                continue;
            }

            // Right-drag: solo the band for as long as the button is held,
            // sweeping its frequency as you move. Whatever was soloed before
            // comes back on release, so this stays an audition, not a toggle.
            if response.drag_started_by(PointerButton::Secondary) {
                self.audition = Some((band.slot, frame.transient.solo()));
                frame.transient.set_solo(Some(band.slot));
                *selected = Some(band.slot);
            }
            if response.dragged_by(PointerButton::Secondary) {
                let scale = if fine { 0.25 } else { 1.0 };
                let x = axes.x(band.freq) + response.drag_delta().x * scale;
                edit::set_freq(frame, band.slot, axes.freq_at(x));
            }

            if response.dragged_by(PointerButton::Primary) {
                let scale = if fine { 0.25 } else { 1.0 };
                let delta = response.drag_delta() * scale;
                edit::set_freq(frame, band.slot, axes.freq_at(axes.x(band.freq) + delta.x));
                // Alt pins the gain, so a band can be swept without its level moving.
                if band.kind.uses_gain() && !alt {
                    edit::set_gain(frame, band.slot, axes.db_at(axes.y(band.gain) + delta.y));
                }
            }
        }

        if let Some((slot, previous)) = self.audition {
            let still_held = ui.input(|i| i.pointer.button_down(PointerButton::Secondary));
            if !still_held {
                frame.transient.set_solo(previous);
                self.audition = None;
                let _ = slot;
            }
        }
        self.hovered = hovered;

        // --- wheel: Q for the resonant types, slope for the cuts ------------
        let focus = hovered.or(*selected);
        if let Some(slot) = focus {
            if background.hovered() || hovered.is_some() {
                let scroll = ui.input(|i| i.raw_scroll_delta.y);
                if scroll.abs() > 0.1 {
                    if let Some(band) = bands.iter().find(|b| b.slot == slot) {
                        // Scrolling up should open the filter out, which is the
                        // opposite sign to the wheel's own.
                        let up = scroll > 0.0;
                        if band.kind.is_cut() {
                            edit::step_slope(frame, slot, if up { -1 } else { 1 });
                        } else if band.kind.uses_q() {
                            let factor = if fine { 1.02 } else { 1.1 };
                            let q = if up {
                                band.q * factor
                            } else {
                                band.q / factor
                            };
                            edit::set_q(frame, slot, q.clamp(Q_MIN, Q_MAX));
                        }
                    }
                }
            }
        }
    }

    fn draw_handles(
        &self,
        ui: &Ui,
        axes: &Axes,
        bands: &[BandView],
        frame: &Frame,
        ui_state: &UiState,
        selected: Option<usize>,
    ) {
        let painter = ui.painter().with_clip_rect(axes.plot.expand(20.0));
        let solo = frame.transient.solo();

        for (index, band) in bands.iter().enumerate() {
            if !band.in_view(ui_state.channel_view) {
                continue;
            }
            let color = band_color(index);
            let centre = pos2(axes.x(band.freq), axes.y(band.handle_db()));
            let active = selected == Some(band.slot);
            let dimmed = solo.is_some_and(|s| s != band.slot);
            let opacity = if !band.enabled {
                0.3
            } else if dimmed {
                0.25
            } else {
                1.0
            };

            if active {
                painter.add(dashed_line(
                    pos2(centre.x, axes.plot.min.y),
                    pos2(centre.x, axes.plot.max.y),
                    Stroke::new(1.0, fade(color, 0.35 * opacity)),
                    3.0,
                    4.0,
                ));
            }

            // Travel limit of a dynamic band.
            if band.dynamic && band.kind.uses_gain() {
                let limit = axes.y(
                    (band.gain + band.dyn_range).clamp(-ui_state.db_range, ui_state.db_range),
                );
                painter.add(dashed_line(
                    pos2(centre.x - 16.0, limit),
                    pos2(centre.x + 16.0, limit),
                    Stroke::new(1.0, fade(color, 0.55 * opacity)),
                    2.0,
                    2.0,
                ));
            }

            if active && band.kind.uses_q() {
                let bandwidth = curves::q_to_octaves(band.q);
                for side in [-1.0f32, 1.0] {
                    let qx = axes.x(band.freq * 2f32.powf(side * bandwidth / 2.0));
                    if !qx.is_finite() {
                        continue;
                    }
                    let grip = Rect::from_center_size(pos2(qx, centre.y), vec2(8.0, 8.0));
                    painter.rect(
                        grip,
                        theme::corner(2),
                        SURFACE_DEEP,
                        Stroke::new(1.5, fade(color, opacity)),
                        nih_plug_egui::egui::epaint::StrokeKind::Middle,
                    );
                }
            }

            if active {
                painter.circle_stroke(centre, 11.0, Stroke::new(1.0, fade(color, 0.4 * opacity)));
            }
            let radius = if active || self.hovered == Some(band.slot) {
                7.0
            } else {
                5.5
            };
            painter.circle(
                centre,
                radius,
                if band.enabled {
                    fade(color, opacity)
                } else {
                    Color32::from_rgb(0x20, 0x25, 0x2c)
                },
                Stroke::new(1.5, SURFACE_DEEP),
            );
            // The catch-light a lacquered head takes, up and to the left.
            if band.enabled {
                painter.circle_filled(
                    pos2(centre.x - radius * 0.32, centre.y - radius * 0.35),
                    radius * 0.26,
                    fade(Color32::WHITE, 0.30 * opacity),
                );
            }
            if band.dynamic && band.kind.uses_gain() {
                painter.circle_stroke(centre, 10.0, Stroke::new(1.0, fade(color, 0.7 * opacity)));
            }

            painter.text(
                pos2(centre.x, centre.y),
                Align2::CENTER_CENTER,
                format!("{}", index + 1),
                FontId::proportional(8.0),
                Color32::from_rgba_unmultiplied(0, 0, 0, (180.0 * opacity) as u8),
            );
            if !band.badge().is_empty() {
                painter.text(
                    pos2(centre.x, centre.y - 12.0),
                    Align2::CENTER_BOTTOM,
                    band.badge(),
                    FontId::proportional(9.0),
                    fade(color, opacity),
                );
            }
        }
    }

    /// The floating read-out beside whichever band has the pointer.
    #[allow(clippy::too_many_arguments)]
    fn draw_readout(
        &self,
        ui: &Ui,
        axes: &Axes,
        bands: &[BandView],
        frame: &Frame,
        ui_state: &UiState,
        selected: Option<usize>,
    ) {
        let Some(slot) = self.hovered.or(selected) else {
            return;
        };
        let Some((index, band)) = bands
            .iter()
            .enumerate()
            .find(|(_, b)| b.slot == slot && b.in_view(ui_state.channel_view))
        else {
            return;
        };

        let mut lines: Vec<(String, Color32)> = Vec::with_capacity(4);
        let mut headline = format!("{} Hz", fmt_freq(band.freq));
        if !band.badge().is_empty() {
            headline.push_str(&format!("  {}", band.channel.as_wire().to_uppercase()));
        }
        lines.push((headline, white(240)));
        if band.kind.uses_gain() {
            lines.push((
                format!("{}{:.1} dB", if band.gain >= 0.0 { "+" } else { "" }, band.gain),
                white(190),
            ));
        } else {
            lines.push((format!("{} dB/oct", band.slope.db_per_oct()), white(190)));
        }
        if band.kind.uses_q() {
            lines.push((format!("Q {:.2}", band.q), white(190)));
        }
        if band.dynamic && band.kind.uses_gain() {
            lines.push((
                format!(
                    "dyn {}{:.1} dB",
                    if band.dyn_range >= 0.0 { "+" } else { "" },
                    band.dyn_range
                ),
                MOCHI,
            ));
        }

        let font = FontId::proportional(theme::TINY);
        let width = lines
            .iter()
            .map(|(text, _)| {
                ui.painter()
                    .layout_no_wrap(text.clone(), font.clone(), Color32::PLACEHOLDER)
                    .size()
                    .x
            })
            .fold(0.0f32, f32::max)
            + 16.0;
        let height = lines.len() as f32 * 12.0 + 10.0;

        let anchor = pos2(axes.x(band.freq) + 14.0, axes.y(band.handle_db()) - 40.0);
        let rect = Rect::from_min_size(
            pos2(
                anchor.x.min(axes.plot.max.x - width),
                anchor.y.max(axes.plot.min.y - PAD_TOP + 4.0),
            ),
            vec2(width, height),
        );

        // The same glass family as the panels, in its smallest cut — the
        // text has to stay sharp over whatever the spectrum is doing.
        crate::gui::widgets::chrome::glass_panel(
            ui,
            frame.fx,
            rect,
            crate::gui::gpu::Glass::tooltip(),
        );
        let painter = ui.painter();
        // The band's own colour, as a thin accent along the plate's left edge.
        painter.rect_filled(
            Rect::from_min_max(
                pos2(rect.min.x + 1.0, rect.min.y + 5.0),
                pos2(rect.min.x + 3.0, rect.max.y - 5.0),
            ),
            theme::corner(1),
            fade(band_color(index), 0.85),
        );
        for (i, (text, color)) in lines.iter().enumerate() {
            painter.text(
                pos2(rect.min.x + 8.0, rect.min.y + 5.0 + i as f32 * 12.0),
                Align2::LEFT_TOP,
                text,
                font.clone(),
                *color,
            );
        }
    }
}

/// The two scales the plot is drawn on.
pub struct Axes {
    pub plot: Rect,
    pub db_range: f32,
}

impl Axes {
    pub fn x(&self, freq: f32) -> f32 {
        let t = (freq.max(1e-3) / F_MIN).ln() / (F_MAX / F_MIN).ln();
        self.plot.min.x + t * self.plot.width()
    }

    pub fn freq_at(&self, x: f32) -> f32 {
        let t = (x - self.plot.min.x) / self.plot.width();
        (F_MIN * (F_MAX / F_MIN).powf(t)).clamp(F_MIN, F_MAX)
    }

    pub fn y(&self, db: f32) -> f32 {
        let t = (db + self.db_range) / (2.0 * self.db_range);
        self.plot.max.y - t * self.plot.height()
    }

    pub fn db_at(&self, y: f32) -> f32 {
        let t = (self.plot.max.y - y) / self.plot.height();
        (t * 2.0 * self.db_range - self.db_range).clamp(-self.db_range, self.db_range)
    }
}

fn draw_grid(painter: &nih_plug_egui::egui::Painter, axes: &Axes) {
    let font = FontId::proportional(theme::TINY);
    for f in FREQ_TICKS {
        let x = axes.x(f).round() + 0.5;
        if x < axes.plot.min.x - 0.5 || x > axes.plot.max.x + 0.5 {
            continue;
        }
        let labelled = LABELLED.contains(&f);
        painter.line_segment(
            [pos2(x, axes.plot.min.y), pos2(x, axes.plot.max.y)],
            Stroke::new(1.0, white(if labelled { 19 } else { 9 })),
        );
        if labelled {
            painter.text(
                pos2(x, axes.plot.max.y + 4.0),
                Align2::CENTER_TOP,
                fmt_freq(f),
                font.clone(),
                white(115),
            );
        }
    }

    let step = if axes.db_range <= 12.0 { 3.0 } else { 6.0 };
    let mut db = -axes.db_range;
    while db <= axes.db_range + 0.001 {
        let y = axes.y(db).round() + 0.5;
        painter.line_segment(
            [pos2(axes.plot.min.x, y), pos2(axes.plot.max.x, y)],
            Stroke::new(1.0, white(if db.abs() < 0.001 { 41 } else { 13 })),
        );
        painter.text(
            pos2(axes.plot.min.x - 8.0, y),
            Align2::RIGHT_CENTER,
            if db > 0.0 {
                format!("+{db:.0}")
            } else {
                format!("{db:.0}")
            },
            font.clone(),
            white(115),
        );
        db += step;
    }
}

/// Which of the two analyser traces is being drawn. They are deliberately not
/// the same picture: the processed signal is the subject — gradient fill,
/// bright detailed edge — and the input is its reference, a thin neutral line
/// that never competes with it.
#[derive(Clone, Copy, PartialEq)]
enum SpectrumLayer {
    Pre,
    Post,
}

/// The EQUZX spectrum ramp, brightest at the curve and falling to a dark plum
/// floor. Direction from the design constants; interpolated smoothly.
const SPECTRUM_PEAK: Color32 = Color32::from_rgb(0xFF, 0xD5, 0xE9);
const SPECTRUM_HIGH: Color32 = Color32::from_rgb(0xFF, 0x8B, 0xC2);
const SPECTRUM_ROSE: Color32 = Color32::from_rgb(0xFF, 0x4F, 0x9B);
const SPECTRUM_MID: Color32 = Color32::from_rgb(0xC5, 0x2F, 0x75);
const SPECTRUM_LOW: Color32 = Color32::from_rgb(0x6F, 0x24, 0x4D);
const SPECTRUM_FLOOR: Color32 = Color32::from_rgb(0x26, 0x13, 0x1E);

/// The colour the spectrum's edge takes at a given normalised energy: quiet
/// material sits in deep plum, the working range in the brand rose, and only
/// genuine peaks approach the pale near-white. No rainbow — one hue, lit.
fn energy_color(e: f32) -> Color32 {
    let e = e.clamp(0.0, 1.0);
    if e < 0.45 {
        theme::mix(SPECTRUM_LOW, SPECTRUM_ROSE, e / 0.45)
    } else if e < 0.80 {
        theme::mix(SPECTRUM_ROSE, SPECTRUM_HIGH, (e - 0.45) / 0.35)
    } else {
        theme::mix(SPECTRUM_HIGH, SPECTRUM_PEAK, (e - 0.80) / 0.20)
    }
}

/// One analyser trace: the field under it, the trace itself, and — for the
/// pre-EQ layer — the peak-hold line above it.
fn draw_spectrum(
    painter: &nih_plug_egui::egui::Painter,
    scratch: &mut spectrum::Scratch,
    points: &[f32],
    axes: &Axes,
    smoothing: f32,
    layer: SpectrumLayer,
) {
    let hold_peaks = layer == SpectrumLayer::Pre;
    // One column per physical pixel — the trace is sampled in screen space,
    // so it stays exactly as fine as the display it is drawn on.
    let ppp = painter.ctx().pixels_per_point();
    let columns = (((axes.plot.width() * ppp).round()).max(2.0) as usize).min(4096);
    let left = axes.plot.min.x;
    let step = axes.plot.width() / (columns - 1).max(1) as f32;
    let freq_at = move |column: f32| {
        let t = (column / (columns - 1).max(1) as f32).clamp(0.0, 1.0);
        F_MIN * (F_MAX / F_MIN).powf(t)
    };

    let curve = spectrum::resample(
        scratch, points, columns, freq_at, F_MIN, F_MAX, FLOOR_DB, smoothing, hold_peaks,
    );

    // The analyser's own dB window, which is not the EQ axis: a spectrum is
    // drawn against absolute level, an EQ curve against gain.
    let span = CEIL_DB - FLOOR_DB;
    let to_y = |db: f32| {
        axes.plot.max.y - ((db - FLOOR_DB) / span).clamp(0.0, 1.15) * axes.plot.height()
    };

    let trace: Vec<Pos2> = curve
        .iter()
        .enumerate()
        .map(|(i, db)| pos2(left + i as f32 * step, to_y(*db)))
        .collect();
    if trace.len() < 2 {
        return;
    }

    match layer {
        SpectrumLayer::Pre => {
            // The reference: a whisper of fill and a thin neutral line, held
            // well under the processed signal.
            painter.add(area_mesh(
                &trace,
                axes.plot.max.y,
                Color32::from_rgba_unmultiplied(168, 165, 180, 12),
            ));
            painter.add(Shape::Path(PathShape {
                points: trace,
                closed: false,
                fill: Color32::TRANSPARENT,
                stroke: PathStroke::new(1.2, Color32::from_rgba_unmultiplied(186, 182, 198, 110)),
            }));
        }
        SpectrumLayer::Post => {
            let energy: Vec<f32> = curve
                .iter()
                .map(|db| ((db - FLOOR_DB) / span).clamp(0.0, 1.0))
                .collect();
            painter.add(spectrum_fill(&trace, &energy, axes.plot));

            // The edge, three strokes deep: a wide whisper of glow, a soft
            // pink line, and the crisp pale core that stays readable over
            // the fill. No giant bloom.
            let tune = crate::gui::tune::get();
            let glow = tune.spectrum_glow;
            for (width, color) in [
                (6.0, fade(SPECTRUM_ROSE, 0.10 * glow)),
                (2.6, fade(SPECTRUM_ROSE, 0.30 * glow)),
                (1.3, fade(SPECTRUM_PEAK, 0.88)),
            ] {
                painter.add(Shape::Path(PathShape {
                    points: trace.clone(),
                    closed: false,
                    fill: Color32::TRANSPARENT,
                    stroke: PathStroke::new(width, color),
                }));
            }
        }
    }

    if hold_peaks {
        let peaks: Vec<Pos2> = scratch
            .peaks()
            .iter()
            .take(columns)
            .enumerate()
            .map(|(i, db)| pos2(left + i as f32 * step, to_y(*db)))
            .collect();
        if peaks.len() >= 2 {
            painter.add(Shape::Path(PathShape {
                points: peaks,
                closed: false,
                fill: Color32::TRANSPARENT,
                stroke: PathStroke::new(1.0, Color32::from_rgba_unmultiplied(214, 214, 220, 40)),
            }));
        }
    }
}

/// The processed spectrum's field: a gradient hung from the curve itself.
///
/// Two components combine per vertex. The curve-local one is the material —
/// pale at the edge, saturated rose just under it, falling through magenta and
/// plum to almost nothing — and rides the trace, so a peak carries its own
/// light down with it. The global one leans the whole field slightly darker
/// toward the graph floor. The grid stays visible through all of it.
fn spectrum_fill(trace: &[Pos2], energy: &[f32], plot: Rect) -> Shape {
    let tune = crate::gui::tune::get();
    let fill = tune.spectrum_fill;
    let depth = tune.spectrum_depth;

    // Distance below the curve each row sits, and the alpha it carries.
    let offsets = [0.0, 10.0 * depth, 26.0 * depth, 64.0 * depth];
    let alphas = [0.72 * fill, 0.46 * fill, 0.24 * fill, 0.10 * fill, 0.035 * fill];

    let mut mesh = Mesh::default();
    if trace.len() < 2 || plot.height() <= 0.0 {
        return Shape::Mesh(mesh.into());
    }

    let depth_dim = |y: f32| {
        let t = ((y - plot.min.y) / plot.height()).clamp(0.0, 1.0);
        1.0 - 0.18 * t
    };

    for (i, p) in trace.iter().enumerate() {
        let e = energy.get(i).copied().unwrap_or(0.0);
        let edge = energy_color(e);
        // Row colours: the edge's own light first, blending down the ramp.
        let colors = [
            edge,
            theme::mix(SPECTRUM_ROSE, edge, 0.35),
            SPECTRUM_MID,
            SPECTRUM_LOW,
            SPECTRUM_FLOOR,
        ];
        for row in 0..5 {
            let y = if row < 4 {
                (p.y + offsets[row]).min(plot.max.y)
            } else {
                plot.max.y
            };
            mesh.colored_vertex(
                pos2(p.x, y),
                fade(colors[row], alphas[row] * depth_dim(y)),
            );
        }
    }

    // Two triangles per band per column pair, five rows to a column.
    let cols = trace.len() as u32;
    for i in 0..cols - 1 {
        for row in 0..4u32 {
            let a = i * 5 + row;
            let b = (i + 1) * 5 + row;
            mesh.add_triangle(a, a + 1, b);
            mesh.add_triangle(a + 1, b, b + 1);
        }
    }
    Shape::Mesh(mesh.into())
}

/// The resonance stage's live reduction, hanging off the zero line.
///
/// Drawn on the plot's own dB scale rather than a meter of its own, so six dB of
/// suppression is six dB down — directly comparable with the band curves it sits
/// under, and readable as "this is the shape being subtracted".
fn draw_resonance(painter: &nih_plug_egui::egui::Painter, axes: &Axes, reduction: &[f32]) {
    let zero = axes.y(0.0);
    let points: Vec<Pos2> = (0..RES_BANDS.min(reduction.len()))
        .filter_map(|i| {
            let freq = band_freq(i);
            if !(F_MIN..=F_MAX).contains(&freq) {
                return None;
            }
            Some(pos2(axes.x(freq), axes.y(-reduction[i])))
        })
        .collect();
    if points.len() < 2 {
        return;
    }

    painter.add(area_mesh(&points, zero, fade(NEON, 0.22)));
    painter.add(Shape::Path(PathShape {
        points,
        closed: false,
        fill: Color32::TRANSPARENT,
        stroke: PathStroke::new(1.4, fade(NEON, 0.85)),
    }));
}

/// The spectral engine's targets, drawn small on purpose.
///
/// A candidate the detector is still weighing is a pale tick along the top of
/// the plot; a target being cut hangs a thin indicator from the zero line down
/// to its depth on the plot's own dB scale. Emphasis goes only to the hovered
/// target and the few deepest cuts — the EQ nodes stay the loudest thing on
/// the graph, and two dozen filters must never read as two dozen handles.
fn draw_res_targets(
    painter: &nih_plug_egui::egui::Painter,
    axes: &Axes,
    targets: &[crate::dsp::spectral::TargetView],
    hover: Option<Pos2>,
) {
    let zero = axes.y(0.0);

    // The third-deepest active cut marks the emphasis line.
    let mut top = [0.0f32; 3];
    for t in targets.iter().filter(|t| t.is_some() && t.is_active()) {
        if t.cut_db > top[0] {
            top = [t.cut_db, top[0], top[1]];
        } else if t.cut_db > top[1] {
            top = [top[0], t.cut_db, top[1]];
        } else if t.cut_db > top[2] {
            top[2] = t.cut_db;
        }
    }
    let emphasis_above = top[2];

    // Nearest target to the pointer, by screen distance in x.
    let hovered = hover.and_then(|p| {
        targets
            .iter()
            .filter(|t| t.is_some() && (F_MIN..=F_MAX).contains(&t.freq))
            .map(|t| (t, (axes.x(t.freq) - p.x).abs()))
            .filter(|(_, d)| *d < 8.0)
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(t, _)| *t)
    });

    for t in targets.iter() {
        if !t.is_some() || !(F_MIN..=F_MAX).contains(&t.freq) {
            continue;
        }
        let x = axes.x(t.freq);
        let is_hovered = hovered.is_some_and(|h| (h.freq - t.freq).abs() < f32::EPSILON);

        if t.is_active() {
            let strong = is_hovered || t.cut_db >= emphasis_above;
            let alpha = if strong { 0.9 } else { 0.45 };
            let tip = axes.y(-t.cut_db);
            painter.line_segment(
                [pos2(x, zero), pos2(x, tip)],
                Stroke::new(if strong { 1.6 } else { 1.1 }, fade(NEON, alpha)),
            );
            // A small downward arrowhead at the depth the filter has reached.
            painter.add(Shape::convex_polygon(
                vec![
                    pos2(x - 2.5, tip - 3.0),
                    pos2(x + 2.5, tip - 3.0),
                    pos2(x, tip + 1.5),
                ],
                fade(NEON, alpha),
                Stroke::NONE,
            ));
        } else if t.confidence > 0.15 {
            // Detected, not yet trusted enough to cut: a quiet mark, brighter
            // as confidence builds.
            painter.circle_filled(
                pos2(x, axes.plot.min.y + 8.0),
                1.5,
                fade(NEON, 0.12 + 0.3 * t.confidence.min(1.0)),
            );
        }

        if is_hovered {
            let label = if t.is_active() {
                format!(
                    "{} · −{:.1} dB · Q {:.0}",
                    fmt_freq(t.freq),
                    t.cut_db,
                    t.q
                )
            } else {
                format!(
                    "{} · {:.0}% sure",
                    fmt_freq(t.freq),
                    t.confidence.min(1.0) * 100.0
                )
            };
            // Kept inside the plot whichever side of it the target sits.
            let at_right = x > axes.plot.center().x;
            painter.text(
                pos2(
                    x + if at_right { -6.0 } else { 6.0 },
                    axes.plot.min.y + 18.0,
                ),
                if at_right {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::LEFT_CENTER
                },
                label,
                theme::caption(),
                white(200),
            );
        }
    }
}

fn stroke_band(
    painter: &nih_plug_egui::egui::Painter,
    grid: &ResponseGrid,
    curve: &[f32],
    axes: &Axes,
    color: Color32,
    focused: bool,
) {
    let points: Vec<Pos2> = (0..grid.len())
        .map(|i| pos2(axes.x(grid.freq(i)), axes.y(curve[i])))
        .collect();
    if points.len() < 2 {
        return;
    }
    if focused {
        painter.add(area_mesh(&points, axes.y(0.0), fade(color, 0.12)));
        // The chosen band carries a little more light than its neighbours.
        painter.add(Shape::Path(PathShape {
            points: points.clone(),
            closed: false,
            fill: Color32::TRANSPARENT,
            stroke: PathStroke::new(4.2, fade(color, 0.14)),
        }));
    }
    painter.add(Shape::Path(PathShape {
        points,
        closed: false,
        fill: Color32::TRANSPARENT,
        stroke: PathStroke::new(
            // A hair over a pixel: sub-pixel strokes leave the feather doing
            // all the work and read as broken; this stays crisp but solid.
            if focused { 1.5 } else { 1.2 },
            fade(color, if focused { 0.85 } else { 0.28 }),
        ),
    }));
}

/// Fill between a traced curve and a baseline.
///
/// Built as a triangle strip rather than a closed path because epaint fills a
/// path by fanning from its first vertex, which is right for a convex shape and
/// wrong for the graph of a function — a spectrum would come out shot through
/// with wedges.
fn area_mesh(points: &[Pos2], baseline: f32, color: Color32) -> Shape {
    let mut mesh = Mesh::default();
    if color.a() == 0 || points.len() < 2 {
        return Shape::Mesh(mesh.into());
    }
    for p in points {
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(*p, color);
        mesh.colored_vertex(pos2(p.x, baseline), color);
        if base >= 2 {
            mesh.add_triangle(base - 2, base - 1, base);
            mesh.add_triangle(base - 1, base, base + 1);
        }
    }
    Shape::Mesh(mesh.into())
}

/// The same, with the alpha coming from the vertex's height in the plot — the
/// vertical gradient the composite curve's field used to get from canvas.
fn vertical_gradient_area(
    points: &[Pos2],
    baseline: f32,
    plot: Rect,
    color: Color32,
    edge_alpha: f32,
    middle_alpha: f32,
) -> Shape {
    let mut mesh = Mesh::default();
    if points.len() < 2 || plot.height() <= 0.0 {
        return Shape::Mesh(mesh.into());
    }
    // Strongest at the top and bottom of the plot, almost gone across the
    // middle, so the field reads as a glow off the curve rather than a wash.
    let at = |y: f32| {
        let t = ((y - plot.min.y) / plot.height()).clamp(0.0, 1.0);
        let away = (t - 0.5).abs() * 2.0;
        fade(color, middle_alpha + (edge_alpha - middle_alpha) * away)
    };

    for p in points {
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(*p, at(p.y));
        mesh.colored_vertex(pos2(p.x, baseline), at(baseline));
        if base >= 2 {
            mesh.add_triangle(base - 2, base - 1, base);
            mesh.add_triangle(base - 1, base, base + 1);
        }
    }
    Shape::Mesh(mesh.into())
}

/// A dashed segment, which egui has no primitive for at this granularity.
fn dashed_line(from: Pos2, to: Pos2, stroke: Stroke, dash: f32, gap: f32) -> Shape {
    Shape::dashed_line(&[from, to], stroke, dash, gap).into()
}

trait KindExt {
    fn uses_q(self) -> bool;
}

impl KindExt for crate::params::BandKind {
    /// Shelves are fixed at S = 1 in the engine, so exposing a Q for them would
    /// be a control that changes nothing.
    fn uses_q(self) -> bool {
        use crate::params::BandKind::*;
        matches!(self, Bell | Notch | BandPass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axes() -> Axes {
        Axes {
            plot: Rect::from_min_size(pos2(40.0, 14.0), vec2(900.0, 400.0)),
            db_range: 18.0,
        }
    }

    #[test]
    fn the_frequency_axis_round_trips() {
        let a = axes();
        for f in [20.0f32, 100.0, 1000.0, 8000.0, 22_000.0] {
            let back = a.freq_at(a.x(f));
            assert!((back - f).abs() < f * 1e-3, "{f} came back as {back}");
        }
    }

    #[test]
    fn the_axis_ends_land_on_the_plot_edges() {
        let a = axes();
        assert!((a.x(F_MIN) - a.plot.min.x).abs() < 0.01);
        assert!((a.x(F_MAX) - a.plot.max.x).abs() < 0.01);
        // And zero dB is the middle of a symmetric range.
        assert!((a.y(0.0) - a.plot.center().y).abs() < 0.01);
    }

    #[test]
    fn the_decibel_axis_round_trips_and_clamps() {
        let a = axes();
        for db in [-18.0f32, -6.0, 0.0, 7.5, 18.0] {
            let back = a.db_at(a.y(db));
            assert!((back - db).abs() < 0.01, "{db} came back as {back}");
        }
        // Dragging off the top of the plot pins to the range rather than
        // running past it.
        assert_eq!(a.db_at(a.plot.min.y - 500.0), 18.0);
        assert_eq!(a.db_at(a.plot.max.y + 500.0), -18.0);
    }

    #[test]
    fn frequencies_are_formatted_the_way_the_axis_labels_them() {
        assert_eq!(fmt_freq(20.0), "20");
        assert_eq!(fmt_freq(999.0), "999");
        assert_eq!(fmt_freq(1000.0), "1k");
        assert_eq!(fmt_freq(1500.0), "1.5k");
        assert_eq!(fmt_freq(10_000.0), "10k");
        assert_eq!(fmt_freq(22_000.0), "22k");
    }

    #[test]
    fn an_area_mesh_covers_every_column_it_is_given() {
        let points: Vec<Pos2> = (0..8).map(|i| pos2(i as f32 * 10.0, 20.0)).collect();
        let Shape::Mesh(mesh) = area_mesh(&points, 100.0, Color32::RED) else {
            panic!("not a mesh");
        };
        assert_eq!(mesh.vertices.len(), 16);
        // Two triangles per gap between columns.
        assert_eq!(mesh.indices.len(), (points.len() - 1) * 6);
    }

    #[test]
    fn a_degenerate_area_is_empty_rather_than_malformed() {
        let Shape::Mesh(mesh) = area_mesh(&[pos2(0.0, 0.0)], 10.0, Color32::RED) else {
            panic!("not a mesh");
        };
        assert!(mesh.indices.is_empty());
    }

    #[test]
    fn only_the_resonant_types_offer_a_q() {
        use crate::params::BandKind::*;
        for kind in [Bell, Notch, BandPass] {
            assert!(kind.uses_q(), "{kind:?}");
        }
        for kind in [LowCut, HighCut, LowShelf, HighShelf] {
            assert!(!kind.uses_q(), "{kind:?}");
        }
    }
}
