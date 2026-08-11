//! The one easing every transition shares.
//!
//! egui's animation clock moves values linearly; everything here wraps it in
//! a smoothstep so panels, hovers and popovers all arrive and leave with the
//! same ease-in-out character. Repaints while something is mid-flight are
//! egui's own — `animate_bool_with_time` requests them itself.

use nih_plug_egui::egui::{Context, Id};

/// Smoothstep: slow in, slow out.
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// An eased 0..1 for a boolean state, keyed by `id`.
///
/// The first sighting of an id returns its target immediately, so nothing
/// animates on the editor's first frame just for existing.
pub fn state(ctx: &Context, id: Id, on: bool, seconds: f32) -> f32 {
    ease(ctx.animate_bool_with_time(id, on, seconds))
}
