//! The dial every continuous control uses.
//!
//! Ported from `Knob.tsx`, including the parts that are not obvious: 270° of
//! travel starting at seven o'clock, a vertical drag rather than a rotary one,
//! shift for fine, double-click to the default, and a value arc that fills out
//! from the centre for a bipolar parameter and from the left for everything
//! else.

use nih_plug_egui::egui::{
    epaint::PathStroke, vec2, Align2, Color32, FontId, Pos2, Response, Sense, Shape, Stroke, Ui,
    Vec2,
};

use crate::gui::theme::{self, white, NEON, SURFACE_HUB};

/// Degrees of travel, and where the sweep starts.
const ARC: f32 = 270.0;
const START: f32 = -135.0;

/// How much of the range a pixel of drag is worth, coarse and fine.
const SPEED: f32 = 0.004;
const SPEED_FINE: f32 = 0.0008;

pub struct Knob<'a> {
    pub label: &'a str,
    pub value: f32,
    pub min: f32,
    pub max: f32,
    /// A log taper suits frequency, Q and times; linear suits gain.
    pub log: bool,
    /// Where a double-click puts it. `None` disables that.
    pub default: Option<f32>,
    pub color: Color32,
    pub disabled: bool,
    /// Diameter of the dial. Everything inside scales with it.
    pub size: f32,
    /// `false` is the band-panel form — dial over value over label. `true` is
    /// the compact one for a header pill: dial beside a two-line caption.
    pub inline: bool,
    pub format: &'a dyn Fn(f32) -> String,
}

impl<'a> Knob<'a> {
    pub fn new(label: &'a str, value: f32, min: f32, max: f32, format: &'a dyn Fn(f32) -> String) -> Self {
        Self {
            label,
            value,
            min,
            max,
            log: false,
            default: None,
            color: NEON,
            disabled: false,
            size: 40.0,
            inline: false,
            format,
        }
    }

    pub fn log(mut self, log: bool) -> Self {
        self.log = log;
        self
    }

    pub fn default_value(mut self, default: f32) -> Self {
        self.default = Some(default);
        self
    }

    pub fn color(mut self, color: Color32) -> Self {
        self.color = color;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn inline(mut self, inline: bool) -> Self {
        self.inline = inline;
        self
    }

    fn to_norm(&self, v: f32) -> f32 {
        let v = v.clamp(self.min, self.max);
        if self.log && self.min > 0.0 {
            (v / self.min).ln() / (self.max / self.min).ln()
        } else {
            (v - self.min) / (self.max - self.min)
        }
    }

    /// The value a normalised position stands for.
    fn denorm(&self, n: f32) -> f32 {
        let n = n.clamp(0.0, 1.0);
        if self.log && self.min > 0.0 {
            self.min * (self.max / self.min).powf(n)
        } else {
            self.min + n * (self.max - self.min)
        }
    }

    /// Draw it. Returns the new value when the user moved it.
    pub fn show(self, ui: &mut Ui) -> Option<f32> {
        let text = (self.format)(self.value);
        let dial = Vec2::splat(self.size);
        let desired = if self.inline {
            let caption_width = ui
                .painter()
                .layout_no_wrap(
                    text.clone(),
                    FontId::proportional(theme::SMALL),
                    Color32::PLACEHOLDER,
                )
                .size()
                .x
                .max(
                    ui.painter()
                        .layout_no_wrap(
                            self.label.to_uppercase(),
                            FontId::proportional(theme::MICRO),
                            Color32::PLACEHOLDER,
                        )
                        .size()
                        .x,
                );
            vec2(self.size + 6.0 + caption_width, self.size)
        } else {
            vec2(self.size.max(58.0), self.size + 24.0)
        };

        let sense = if self.disabled {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        };
        let (rect, response) = ui.allocate_exact_size(desired, sense);

        let dial_rect = if self.inline {
            nih_plug_egui::egui::Rect::from_min_size(
                Pos2::new(rect.min.x, rect.center().y - self.size / 2.0),
                dial,
            )
        } else {
            nih_plug_egui::egui::Rect::from_min_size(
                Pos2::new(rect.center().x - self.size / 2.0, rect.min.y),
                dial,
            )
        };

        let mut changed = None;
        if !self.disabled {
            // Deltas are accumulated rather than measured from the press: the
            // caller writes the result back to the parameter and this reads it
            // again next frame, so there is nothing for the drift to build up in.
            let dy = response.drag_delta().y;
            if dy != 0.0 {
                let fine = ui.input(|i| i.modifiers.shift_only());
                let speed = if fine { SPEED_FINE } else { SPEED };
                let next = (self.to_norm(self.value) - dy * speed).clamp(0.0, 1.0);
                changed = Some(self.denorm(next));
            }
            if response.double_clicked() {
                changed = self.default;
            }
        }

        let norm = self.to_norm(changed.unwrap_or(self.value));
        let alpha = if self.disabled { 0.3 } else { 1.0 };
        paint_dial(ui, dial_rect, norm, &self, alpha, &response);

        let text = (self.format)(changed.unwrap_or(self.value));
        paint_caption(ui, rect, dial_rect, &self, &text, alpha);

        changed
    }
}

fn paint_dial(
    ui: &Ui,
    rect: nih_plug_egui::egui::Rect,
    norm: f32,
    knob: &Knob<'_>,
    alpha: f32,
    response: &Response,
) {
    let painter = ui.painter();
    let center = rect.center();
    let size = rect.width();
    let radius = size * 0.354;
    let hub = size * 0.25;
    let pointer = size * 0.208;
    let width = (size * 0.0625).max(2.0);

    // Track.
    painter.add(arc(center, radius, 0.0, 1.0, width, dim(white(26), alpha)));

    // Bipolar parameters fill outward from centre; everything else from the left.
    let bipolar = knob.min < 0.0 && knob.max > 0.0 && !knob.log;
    let origin = if bipolar { knob.to_norm(0.0) } else { 0.0 };
    if (norm - origin).abs() > 1e-4 {
        painter.add(arc(
            center,
            radius,
            origin.min(norm),
            origin.max(norm),
            width,
            dim(knob.color, alpha * 0.95),
        ));
    }

    painter.circle(
        center,
        hub,
        dim(SURFACE_HUB, alpha),
        Stroke::new(1.0, dim(white(20), alpha)),
    );
    if response.hovered() && !knob.disabled {
        painter.circle_stroke(center, hub, Stroke::new(1.0, dim(knob.color, 0.35)));
    }

    let angle = (START + norm * ARC).to_radians();
    painter.line_segment(
        [
            center,
            Pos2::new(
                center.x + pointer * angle.sin(),
                center.y - pointer * angle.cos(),
            ),
        ],
        Stroke::new((width * 0.67).max(1.5), dim(knob.color, alpha)),
    );
}

fn paint_caption(
    ui: &Ui,
    rect: nih_plug_egui::egui::Rect,
    dial: nih_plug_egui::egui::Rect,
    knob: &Knob<'_>,
    text: &str,
    alpha: f32,
) {
    let painter = ui.painter();
    let label = knob.label.to_uppercase();
    if knob.inline {
        let x = dial.max.x + 6.0;
        painter.text(
            Pos2::new(x, rect.center().y - 1.0),
            Align2::LEFT_BOTTOM,
            label,
            FontId::proportional(theme::MICRO),
            dim(white(95), alpha),
        );
        painter.text(
            Pos2::new(x, rect.center().y + 1.0),
            Align2::LEFT_TOP,
            text,
            FontId::proportional(theme::SMALL),
            dim(white(230), alpha),
        );
    } else {
        painter.text(
            Pos2::new(rect.center().x, dial.max.y + 2.0),
            Align2::CENTER_TOP,
            text,
            FontId::proportional(theme::TINY),
            dim(white(215), alpha),
        );
        painter.text(
            Pos2::new(rect.center().x, dial.max.y + 13.0),
            Align2::CENTER_TOP,
            label,
            FontId::proportional(theme::MICRO),
            dim(white(95), alpha),
        );
    }
}

fn dim(color: Color32, alpha: f32) -> Color32 {
    theme::fade(color, (color.a() as f32 / 255.0) * alpha)
}

/// A stroked arc with round caps, which egui has no primitive for.
fn arc(
    center: Pos2,
    radius: f32,
    from: f32,
    to: f32,
    width: f32,
    color: Color32,
) -> Shape {
    let steps = (((to - from) * ARC / 4.0).abs().ceil() as usize).clamp(2, 96);
    let at = |t: f32| {
        let a = (START + t * ARC).to_radians();
        Pos2::new(center.x + radius * a.sin(), center.y - radius * a.cos())
    };
    let points: Vec<Pos2> = (0..=steps)
        .map(|i| at(from + (to - from) * i as f32 / steps as f32))
        .collect();

    // Round caps, drawn as discs at both ends. Cheaper than it looks: the arc
    // itself is already a path shape, so this is two more circles in the batch.
    let mut shapes = vec![Shape::Path(nih_plug_egui::egui::epaint::PathShape {
        points: points.clone(),
        closed: false,
        fill: Color32::TRANSPARENT,
        stroke: PathStroke::new(width, color),
    })];
    for p in [points[0], points[points.len() - 1]] {
        shapes.push(Shape::circle_filled(p, width / 2.0, color));
    }
    Shape::Vec(shapes)
}
