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

use nih_plug_egui::egui::{
    vec2, Align, Align2, Area, Context, Id, Layout, Order, Pos2, Rect, Sense, Ui, UiBuilder, Vec2,
};

use crate::gui::gpu::FxRenderer;
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

    /// Show the plate. Returns the rectangle it ended up occupying, which is
    /// what the layout above uses to keep the plot clear of it.
    pub fn show(
        self,
        ctx: &Context,
        fx: &Arc<FxRenderer>,
        contents: impl FnOnce(&mut Ui),
    ) -> Rect {
        Area::new(self.id)
            .order(Order::Foreground)
            .fixed_pos(self.pos)
            .pivot(self.pivot)
            .constrain(false)
            .show(ctx, |ui| {
                // The plate is reserved before the contents so the blur callback
                // is earlier in the draw list than everything it sits under.
                let slot = chrome::reserve_glass(ui);
                let origin = ui.cursor().min;

                // A generous ceiling rather than the screen: this is a floating
                // panel, so `available_width` would be whatever is left of the
                // window from its own corner.
                let room = self.width.unwrap_or(4096.0) - self.padding.x * 2.0;
                let layout = if self.horizontal {
                    Layout::left_to_right(Align::Center)
                } else {
                    Layout::top_down(Align::Min)
                };
                let mut child = ui.new_child(
                    UiBuilder::new()
                        .max_rect(Rect::from_min_size(
                            origin + self.padding,
                            vec2(room.max(1.0), 4096.0),
                        ))
                        .layout(layout),
                );
                contents(&mut child);
                let inner = child.min_rect();

                let mut rect = Rect::from_min_max(origin, inner.max + self.padding);
                if let Some(width) = self.width {
                    rect = Rect::from_min_size(origin, vec2(width, rect.height()));
                }

                let response = ui.allocate_rect(rect, Sense::hover());
                let sheen = if self.sheen {
                    response
                        .hover_pos()
                        .map(|p| Pos2::new(p.x - rect.min.x, p.y - rect.min.y))
                } else {
                    None
                };
                chrome::fill_glass(ui, slot, fx, rect, self.radius, sheen);
                rect
            })
            .inner
    }
}
