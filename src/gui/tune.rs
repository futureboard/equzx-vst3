//! Live material tuning, behind `EQUZX_TUNE`.
//!
//! With the variable set, a small window of sliders scales the glass and
//! spectrum quantities while the editor runs, so the material can be judged
//! on the real renderer instead of by recompiling constants. With it unset —
//! the shipped state — every multiplier is 1.0 and this module costs one
//! atomic-free read of a `OnceLock`.

use std::sync::{Mutex, OnceLock};

use nih_plug_egui::egui::{Context, Slider, Window};

#[derive(Clone, Copy)]
pub struct Tune {
    /// Scales every glass reflection layer together.
    pub glass_reflection: f32,
    /// Scales only the directional edge rim.
    pub glass_edge: f32,
    /// Scales the body tint's opacity.
    pub glass_tint: f32,
    /// Scales the spectrum fill's alpha ramp.
    pub spectrum_fill: f32,
    /// Scales the spectrum and EQ curve glow strokes.
    pub spectrum_glow: f32,
    /// Scales how far the curve-local gradient reaches down.
    pub spectrum_depth: f32,
}

impl Default for Tune {
    fn default() -> Self {
        Self {
            glass_reflection: 1.0,
            glass_edge: 1.0,
            glass_tint: 1.0,
            spectrum_fill: 1.0,
            spectrum_glow: 1.0,
            spectrum_depth: 1.0,
        }
    }
}

fn state() -> Option<&'static Mutex<Tune>> {
    static STATE: OnceLock<Option<Mutex<Tune>>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            std::env::var_os("EQUZX_TUNE").map(|_| Mutex::new(Tune::default()))
        })
        .as_ref()
}

/// This frame's multipliers — the defaults unless tuning is switched on.
pub fn get() -> Tune {
    state()
        .and_then(|m| m.lock().ok().map(|t| *t))
        .unwrap_or_default()
}

/// The slider window, drawn only when `EQUZX_TUNE` is set.
pub fn window(ctx: &Context) {
    let Some(mutex) = state() else {
        return;
    };
    let Ok(mut tune) = mutex.lock() else {
        return;
    };
    Window::new("Material tuning")
        .default_width(260.0)
        .show(ctx, |ui| {
            ui.add(Slider::new(&mut tune.glass_reflection, 0.0..=2.0).text("Glass reflection"));
            ui.add(Slider::new(&mut tune.glass_edge, 0.0..=2.0).text("Glass edge"));
            ui.add(Slider::new(&mut tune.glass_tint, 0.0..=1.0).text("Glass tint"));
            ui.add(Slider::new(&mut tune.spectrum_fill, 0.0..=1.0).text("Spectrum fill"));
            ui.add(Slider::new(&mut tune.spectrum_glow, 0.0..=2.0).text("Spectrum glow"));
            ui.add(Slider::new(&mut tune.spectrum_depth, 0.0..=2.0).text("Gradient depth"));
            if ui.button("Reset").clicked() {
                *tune = Tune::default();
            }
        });
}
