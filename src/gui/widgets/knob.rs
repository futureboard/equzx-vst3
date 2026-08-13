//! The dial every continuous control uses.
//!
//! Ported from `Knob.tsx`, including the parts that are not obvious: 270° of
//! travel starting at seven o'clock, a vertical drag rather than a rotary one,
//! shift for fine, double-click to the default, and a value arc that fills out
//! from the centre for a bipolar parameter and from the left for everything
//! else.

use nih_plug_egui::egui::{
    epaint::PathStroke, vec2, Align2, Color32, Pos2, Response, Sense, Shape, Stroke, Ui,
    Vec2,
};

use crate::gui::theme::{self, white, NEON, SURFACE_HUB};

/// Degrees of travel, and where the sweep starts.
const ARC: f32 = 270.0;
const START: f32 = -135.0;

/// How much of the range a pixel of drag is worth, coarse and fine.
/// A full coarse sweep is roughly 140 px; Shift keeps precise trimming.
const SPEED: f32 = 0.007;
const SPEED_FINE: f32 = 0.0012;
const SCROLL_SPEED: f32 = 0.045;
const SCROLL_SPEED_FINE: f32 = 0.009;

/// Turn egui's device-dependent wheel points into useful notches. Preserve
/// magnitude so a fast wheel spin is not collapsed into a single tiny step.
fn scroll_steps(delta: f32) -> f32 {
    if delta.abs() <= 0.1 {
        0.0
    } else {
        delta.signum() * (delta.abs() / 14.0).clamp(0.35, 5.0)
    }
}

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
            size: 48.0,
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
                    theme::medium(theme::SMALL),
                    Color32::PLACEHOLDER,
                )
                .size()
                .x
                .max(
                    ui.painter()
                        .layout_no_wrap(
                            self.label.to_uppercase(),
                            theme::caption(),
                            Color32::PLACEHOLDER,
                        )
                        .size()
                        .x,
                );
            vec2(self.size + 6.0 + caption_width, self.size)
        } else {
            // The old panel gave every dial a 64-point column: dial, value,
            // label, on a 4-point rhythm.
            vec2(self.size.max(64.0), self.size + 33.0)
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
            // `Response::drag_delta()` is cumulative for the whole gesture.
            // Adding that to the current parameter every frame caused the dial
            // to accelerate, jump, and fight host parameter feedback. Pointer
            // delta is frame-local and starts responding on the first movement,
            // without waiting for egui's drag threshold.
            let primary_held = ui.input(|i| i.pointer.primary_down());
            let dy = ui.input(|i| i.pointer.delta().y);
            if primary_held && response.is_pointer_button_down_on() && dy != 0.0 {
                let fine = ui.input(|i| i.modifiers.shift_only());
                let speed = if fine { SPEED_FINE } else { SPEED };
                let next = (self.to_norm(self.value) - dy * speed).clamp(0.0, 1.0);
                changed = Some(self.denorm(next));
            }
            if response.hovered() {
                let (wheel, fine) = ui.input(|i| {
                    // egui maps Shift+wheel onto X, so accept either axis.
                    let raw = i.raw_scroll_delta;
                    let wheel = if raw.y.abs() >= raw.x.abs() { raw.y } else { raw.x };
                    (wheel, i.modifiers.shift_only())
                });
                let steps = scroll_steps(wheel);
                if steps != 0.0 {
                    let current = changed.unwrap_or(self.value);
                    let speed = if fine {
                        SCROLL_SPEED_FINE
                    } else {
                        SCROLL_SPEED
                    };
                    let next = (self.to_norm(current) + steps * speed).clamp(0.0, 1.0);
                    changed = Some(self.denorm(next));
                }
            }
            if response.double_clicked() {
                changed = self.default;
            }
        }

        let norm = self.to_norm(changed.unwrap_or(self.value));
        // Disabled controls stay readable while their muted accent still
        // communicates that they cannot currently be edited.
        let alpha = if self.disabled { 0.46 } else { 1.0 };
        paint_dial(ui, dial_rect, norm, &self, alpha, &response);

        let text = (self.format)(changed.unwrap_or(self.value));
        paint_caption(ui, rect, dial_rect, &self, &text, alpha);

        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_steps_keep_direction_and_fast_spin_magnitude() {
        assert_eq!(scroll_steps(0.0), 0.0);
        assert!(scroll_steps(14.0) > 0.9);
        assert!(scroll_steps(-14.0) < -0.9);
        assert!(scroll_steps(56.0) > scroll_steps(14.0));
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

    // A small glass hub with its own shadow and reflected catch keeps the dial
    // distinct from every panel depth, including inside dark DAW hosts.
    painter.circle_filled(
        center + vec2(0.0, size * 0.035),
        hub * 1.16,
        dim(Color32::from_black_alpha(150), alpha),
    );
    painter.circle(
        center,
        hub * 1.12,
        dim(Color32::from_rgb(0x2b, 0x2b, 0x33), alpha),
        Stroke::new(1.0, dim(white(42), alpha)),
    );

    // The control scale needs to read without requiring hover.
    painter.add(arc(center, radius, 0.0, 1.0, width, dim(white(54), alpha)));

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
        Stroke::new(1.0, dim(white(58), alpha)),
    );
    painter.circle_filled(
        center + vec2(-hub * 0.28, -hub * 0.32),
        (size * 0.026).max(1.1),
        dim(white(72), alpha),
    );
    if response.hovered() && !knob.disabled {
        painter.circle_stroke(center, hub * 1.12, Stroke::new(1.0, dim(knob.color, 0.55)));
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
        // The two lines pull into each other — the `leading-tight` +
        // `-space-y-1` stack of the original — so the pair reads as one
        // block centred on the dial.
        let x = dial.max.x + 6.0;
        painter.text(
            Pos2::new(x, rect.center().y + 1.5),
            Align2::LEFT_BOTTOM,
            label,
            theme::caption(),
            dim(white(89), alpha),
        );
        painter.text(
            Pos2::new(x, rect.center().y - 1.5),
            Align2::LEFT_TOP,
            text,
            theme::medium(theme::SMALL),
            dim(white(230), alpha),
        );
    } else {
        painter.text(
            Pos2::new(rect.center().x, dial.max.y + 3.0),
            Align2::CENTER_TOP,
            text,
            theme::medium(theme::TINY),
            dim(white(217), alpha),
        );
        painter.text(
            Pos2::new(rect.center().x, dial.max.y + 19.0),
            Align2::CENTER_TOP,
            label,
            theme::caption(),
            dim(white(89), alpha),
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
