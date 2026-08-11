//! The floating panels, and the frame they share.
//!
//! Every one of these is an [`Area`] over the plot rather than a strip carved
//! out of it, which is what the layout was in the web build too: the display
//! runs edge to edge and the controls sit on top of it, frosted, so the picture
//! is never cut into by its own chrome.

pub mod band_strip;
pub mod header;
pub mod overlays;
pub mod resonance;

use std::sync::Arc;

use nih_plug_egui::egui::{self, vec2, Align2, Area, Context, Id, Margin, Order, Pos2, Rect, Ui, Vec2};

use crate::gui::gpu::{FxRenderer, Glass};
use crate::gui::widgets::chrome;

/// How a floating panel is laid out.
pub struct Floating {
    pub id: Id,
    /// Where the panel is pinned, interpreted through `pivot`.
    pub pos: Pos2,
    pub pivot: Align2,
    /// A fixed width, or `None` to size to the contents.
    pub width: Option<f32>,
    pub padding: Vec2,
    pub radius: f32,
    /// Lay the contents out across the panel rather than down it.
    pub horizontal: bool,
    /// Follow the pointer with a specular highlight while it is over the plate.
    pub sheen: bool,
    /// Rest at this opacity until pointed at — the analyser overlay's way of
    /// not competing with the curve it sits over.
    pub dim: Option<f32>,
    /// Seconds after the editor opens before this panel fades and settles in.
    pub intro: Option<f32>,
    /// The material the plate is cut from.
    pub glass: Glass,
}

impl Floating {
    pub fn new(id: impl std::hash::Hash, pos: Pos2) -> Self {
        Self {
            id: Id::new(id),
            pos,
            pivot: Align2::LEFT_TOP,
            width: None,
            padding: vec2(10.0, 8.0),
            radius: 22.0,
            horizontal: true,
            sheen: false,
            dim: None,
            intro: None,
            glass: Glass::panel(0.8),
        }
    }

    pub fn pivot(mut self, pivot: Align2) -> Self {
        self.pivot = pivot;
        self
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    pub fn padding(mut self, padding: Vec2) -> Self {
        self.padding = padding;
        self
    }

    pub fn radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    pub fn vertical(mut self) -> Self {
        self.horizontal = false;
        self
    }

    pub fn sheen(mut self, sheen: bool) -> Self {
        self.sheen = sheen;
        self
    }

    pub fn dim(mut self, resting: f32) -> Self {
        self.dim = Some(resting);
        self
    }

    pub fn glass(mut self, glass: Glass) -> Self {
        self.glass = glass;
        self
    }

    pub fn intro(mut self, delay: f32) -> Self {
        self.intro = Some(delay);
        self
    }

    /// Show the plate. Returns the rectangle it ended up occupying, which is
    /// what the layout above uses to keep the plot clear of it.
    pub fn show(
        self,
        ctx: &Context,
        fx: &Arc<FxRenderer>,
        contents: impl FnOnce(&mut Ui),
    ) -> Rect {
        // Eased fades, multiplied together: the resting dim until pointed at,
        // and the once-per-open intro that settles the panel up into place.
        let mut opacity = 1.0f32;
        let mut settle = 0.0f32;
        if let Some(resting) = self.dim {
            // Judged against last frame's rectangle — this frame's is not
            // known yet, and one frame of lag on a hover fade is invisible.
            let hovered = ctx
                .memory(|m| m.area_rect(self.id))
                .zip(ctx.pointer_hover_pos())
                .is_some_and(|(rect, pointer)| rect.contains(pointer));
            let t = crate::gui::anim::state(ctx, self.id.with("dim"), hovered, 0.2);
            opacity *= resting + (1.0 - resting) * t;
        }
        // Skipped headless: the harness renders a handful of passes from t=0,
        // and a preview of a UI mid-fade answers nothing.
        if let (Some(delay), false) = (self.intro, cfg!(test)) {
            let now = ctx.input(|i| i.time);
            let t0 = ctx.data_mut(|d| {
                *d.get_temp_mut_or_insert_with(Id::new("equzx-intro-t0"), || now)
            });
            let e = crate::gui::anim::ease(((now - t0) as f32 - delay) / 0.62);
            opacity *= e;
            settle = 12.0 * (1.0 - e);
        }

        Area::new(self.id)
            .order(Order::Foreground)
            .fixed_pos(self.pos + egui::vec2(0.0, settle))
            .pivot(self.pivot)
            .constrain(false)
            .show(ctx, |ui| {
                if opacity < 1.0 {
                    ui.set_opacity(opacity);
                }

                // The plate is reserved before the contents so the blur callback
                // is earlier in the draw list than everything it sits under.
                let slot = chrome::reserve_glass(ui);

                // An `Area` hands its contents an unbounded rectangle, and two
                // things here need a real edge to measure against: a
                // right-to-left group works back from the right edge, and
                // `INFINITY - INFINITY` is `NaN`, which propagates into the
                // stored area rect and takes egui's own assertions down with it.
                let bound = self
                    .width
                    .unwrap_or_else(|| ui.ctx().screen_rect().width());
                ui.set_max_width(bound);

                // The panel is sized by what goes in it, so the contents go in
                // egui's own auto-sizing containers and the frame reports what
                // they came to. Handing a child `Ui` a tall `max_rect` to grow
                // into instead is what broke this the first time round: a row
                // centred on the cross axis reports a `min_rect` as tall as the
                // space it was offered, so every panel measured the ceiling it
                // had been given rather than its own height.
                let frame = egui::Frame::NONE
                    .inner_margin(Margin::symmetric(
                        self.padding.x as i8,
                        self.padding.y as i8,
                    ))
                    .show(ui, |ui| {
                        // A fixed-width panel spans its width whatever is in it,
                        // which is what lets the header push a group to the far
                        // end of the bar.
                        if self.width.is_some() {
                            ui.set_min_width(bound - self.padding.x * 2.0);
                        }
                        if self.horizontal {
                            ui.horizontal(|ui| contents(ui));
                        } else {
                            ui.vertical(|ui| contents(ui));
                        }
                    });

                let rect = frame.response.rect;
                let sheen = if self.sheen {
                    frame
                        .response
                        .hover_pos()
                        .map(|p| Pos2::new(p.x - rect.min.x, p.y - rect.min.y))
                } else {
                    None
                };
                let mut style = self.glass;
                style.corner_radius = self.radius;
                style.opacity = opacity;
                chrome::fill_glass(ui, slot, fx, rect, style, sheen);
                rect
            })
            .inner
    }
}
