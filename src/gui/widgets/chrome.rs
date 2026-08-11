//! The surfaces everything else sits on.
//!
//! A direct port of what the old stylesheet called `.glass`, `.glass-pill`,
//! `.neon-on` and friends. The frosted plates go through [`crate::gui::gpu`];
//! everything smaller is a painted fill, which is what the CSS did too — only
//! the large panels ever carried a `backdrop-filter`.

use std::sync::Arc;

use nih_plug_egui::egui::{
    epaint::StrokeKind, vec2, Align2, Color32, FontId, Pos2, Rect, Response, Sense, Stroke, Ui,
    Vec2,
};

use crate::gui::gpu::{FxRenderer, Glass};
use crate::gui::theme::{self, fade, white, NEON};

/// Four painter slots held open for a frosted plate whose size is not known
/// until its contents have been laid out.
///
/// The order matters and is the whole reason this exists: the blur callback has
/// to be *earlier in the draw list* than the panel's contents, or it would
/// capture the panel on top of itself. Reserving the slots first and filling
/// them afterwards keeps the ordering while letting the rectangle come from the
/// layout.
pub struct GlassSlot {
    fill: nih_plug_egui::egui::layers::ShapeIdx,
    blur: nih_plug_egui::egui::layers::ShapeIdx,
    border: nih_plug_egui::egui::layers::ShapeIdx,
    highlight: nih_plug_egui::egui::layers::ShapeIdx,
}

/// Hold the plate's place in the draw list.
pub fn reserve_glass(ui: &Ui) -> GlassSlot {
    let painter = ui.painter();
    GlassSlot {
        fill: painter.add(nih_plug_egui::egui::Shape::Noop),
        blur: painter.add(nih_plug_egui::egui::Shape::Noop),
        border: painter.add(nih_plug_egui::egui::Shape::Noop),
        highlight: painter.add(nih_plug_egui::egui::Shape::Noop),
    }
}

/// Fill a reserved plate now that its rectangle is known.
pub fn fill_glass(
    ui: &Ui,
    slot: GlassSlot,
    fx: &Arc<FxRenderer>,
    rect: Rect,
    radius: f32,
    sheen: Option<Pos2>,
) {
    let painter = ui.painter();
    let corner = theme::corner(radius.round().clamp(0.0, 255.0) as u8);

    // Opaque, and close to what the blur averages out to, so a frame that falls
    // back to it looks deliberate rather than broken.
    painter.set(
        slot.fill,
        nih_plug_egui::egui::epaint::RectShape::filled(
            rect,
            corner,
            Color32::from_rgb(0x15, 0x15, 0x19),
        ),
    );
    painter.set(
        slot.blur,
        fx.glass(
            rect,
            Glass {
                tint: Color32::from_rgba_unmultiplied(0x16, 0x16, 0x1a, 140),
                corner_radius: radius,
                rim: 0.05,
                noise: 0.014,
                sheen: sheen.map(|p| Pos2::new(p.x - rect.min.x, p.y - rect.min.y)),
                sheen_amount: if sheen.is_some() { 0.07 } else { 0.0 },
                levels: 3,
            },
        ),
    );
    painter.set(
        slot.border,
        nih_plug_egui::egui::epaint::RectShape::stroke(
            rect,
            corner,
            Stroke::new(1.0, white(26)),
            StrokeKind::Inside,
        ),
    );
    // A glass edge catches a brighter line than the border it sits in.
    painter.set(
        slot.highlight,
        nih_plug_egui::egui::Shape::line_segment(
            [
                Pos2::new(rect.min.x + radius * 0.6, rect.min.y + 1.0),
                Pos2::new(rect.max.x - radius * 0.6, rect.min.y + 1.0),
            ],
            Stroke::new(1.0, white(22)),
        ),
    );
}

/// A frosted plate at a rectangle already known.
///
/// Emitted *before* the panel's contents, so the callback captures the
/// framebuffer while it still holds only what is behind the panel.
pub fn glass_panel(ui: &Ui, fx: &Arc<FxRenderer>, rect: Rect, radius: f32, sheen: Option<Pos2>) {
    let slot = reserve_glass(ui);
    fill_glass(ui, slot, fx, rect, radius, sheen);
}

/// How a small control is filled.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fill {
    /// The resting `.glass-pill`.
    Quiet,
    /// `.glass-pill-on` — pressed, or holding an open menu.
    Lit,
    /// `.neon-on` — armed. Tinted, ringed, and lettered in the pale end of the hue.
    Armed,
    /// `.neon-solid` — the one filled control in a group, lettered in black.
    Solid(Color32),
    /// No fill at all; used where the pill is only there to be hit.
    None,
}

impl Fill {
    /// The colour text and glyphs on this fill should be drawn in.
    pub fn foreground(self, hovered: bool) -> Color32 {
        match self {
            Fill::Solid(_) => Color32::from_rgba_unmultiplied(0, 0, 0, 220),
            Fill::Armed => theme::MOCHI,
            Fill::Lit => white(235),
            Fill::Quiet | Fill::None => {
                if hovered {
                    white(220)
                } else {
                    white(150)
                }
            }
        }
    }
}

/// Paint one pill-shaped control's background into `rect`.
pub fn pill_bg(ui: &Ui, rect: Rect, radius: f32, fill: Fill, hovered: bool) {
    let painter = ui.painter();
    let corner = theme::corner(radius.round().clamp(0.0, 255.0) as u8);
    match fill {
        Fill::None => {}
        Fill::Quiet => {
            painter.rect_filled(rect, corner, white(if hovered { 20 } else { 12 }));
            painter.rect_stroke(rect, corner, Stroke::new(1.0, white(20)), StrokeKind::Inside);
        }
        Fill::Lit => {
            painter.rect_filled(rect, corner, white(36));
            painter.rect_stroke(rect, corner, Stroke::new(1.0, white(34)), StrokeKind::Inside);
        }
        Fill::Armed => {
            painter.rect_filled(rect, corner, fade(NEON, 0.18));
            painter.rect_stroke(
                rect,
                corner,
                Stroke::new(1.0, fade(NEON, 0.5)),
                StrokeKind::Inside,
            );
        }
        Fill::Solid(color) => {
            // The glow the CSS got from a box-shadow, as two fading rings.
            for (grow, alpha) in [(3.0, 0.10), (1.5, 0.16)] {
                painter.rect_filled(
                    rect.expand(grow),
                    theme::corner((radius + grow).round().clamp(0.0, 255.0) as u8),
                    fade(color, alpha),
                );
            }
            painter.rect_filled(rect, corner, color);
        }
    }
}

/// A pill with a text label. Returns the response so callers can act on clicks
/// and hang a tooltip off it.
pub fn pill(ui: &mut Ui, text: &str, fill: Fill) -> Response {
    pill_sized(ui, text, fill, None)
}

/// The same, at a width the caller picks rather than one measured from the text.
pub fn pill_sized(ui: &mut Ui, text: &str, fill: Fill, width: Option<f32>) -> Response {
    let font = FontId::proportional(theme::SMALL);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        font.clone(),
        Color32::PLACEHOLDER,
    );
    let width = width.unwrap_or(galley.size().x + 22.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, PILL_HEIGHT), Sense::click());

    pill_bg(ui, rect, PILL_HEIGHT / 2.0, fill, response.hovered());
    ui.painter().galley(
        Pos2::new(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        fill.foreground(response.hovered()),
    );
    response
}

/// Every pill in the UI is this tall, which is what keeps the header on one
/// rhythm however its controls are built.
pub const PILL_HEIGHT: f32 = 26.0;

/// A row of mutually exclusive options in one rounded group.
///
/// Returns the index the user picked, if they picked one.
pub fn segmented(
    ui: &mut Ui,
    labels: &[&str],
    selected: usize,
    accent: Color32,
    item_width: f32,
) -> Option<usize> {
    let total = vec2(item_width * labels.len() as f32 + 4.0, PILL_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(total, Sense::hover());
    let corner = theme::corner((PILL_HEIGHT / 2.0) as u8);
    ui.painter().rect_filled(rect, corner, white(12));
    ui.painter()
        .rect_stroke(rect, corner, Stroke::new(1.0, white(18)), StrokeKind::Inside);

    let mut picked = None;
    for (i, label) in labels.iter().enumerate() {
        let cell = Rect::from_min_size(
            Pos2::new(rect.min.x + 2.0 + item_width * i as f32, rect.min.y + 2.0),
            vec2(item_width, PILL_HEIGHT - 4.0),
        );
        let response = ui.interact(cell, ui.id().with(("seg", i, label)), Sense::click());
        let on = i == selected;
        if on {
            ui.painter().rect_filled(
                cell,
                theme::corner(((PILL_HEIGHT - 4.0) / 2.0) as u8),
                accent,
            );
        } else if response.hovered() {
            ui.painter()
                .rect_filled(cell, theme::corner(((PILL_HEIGHT - 4.0) / 2.0) as u8), white(20));
        }
        ui.painter().text(
            cell.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(theme::SMALL),
            if on {
                Color32::from_rgba_unmultiplied(0, 0, 0, 225)
            } else if response.hovered() {
                white(215)
            } else {
                white(120)
            },
        );
        if response.clicked() {
            picked = Some(i);
        }
    }
    picked
}

/// The 9px uppercase caption that sits above or beside a control.
pub fn caption(ui: &mut Ui, text: &str) {
    let spaced: String = text
        .to_uppercase()
        .chars()
        .flat_map(|c| [c, '\u{2009}'])
        .collect();
    ui.label(
        nih_plug_egui::egui::RichText::new(spaced.trim_end())
            .font(FontId::proportional(theme::MICRO))
            .color(white(95)),
    );
}

/// A hairline separator between groups in the header.
pub fn divider(ui: &mut Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, height), Sense::hover());
    ui.painter().rect_filled(
        Rect::from_center_size(rect.center(), vec2(1.0, height)),
        0,
        white(30),
    );
}

/// A horizontal bar meter with a threshold marker.
pub fn meter(ui: &Ui, rect: Rect, filled: f32, marker: Option<f32>, color: Color32) {
    let painter = ui.painter();
    let corner = theme::corner((rect.height() / 2.0) as u8);
    painter.rect_filled(rect, corner, Color32::from_black_alpha(150));
    let width = rect.width() * filled.clamp(0.0, 1.0);
    if width > 0.5 {
        painter.rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(width, rect.height())),
            corner,
            fade(color, 0.6),
        );
    }
    if let Some(at) = marker {
        let x = rect.min.x + rect.width() * at.clamp(0.0, 1.0);
        painter.line_segment(
            [Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)],
            Stroke::new(1.0, white(200)),
        );
    }
}
