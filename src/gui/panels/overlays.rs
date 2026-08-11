//! The analyser pickers parked over the top-right of the plot.
//!
//! A port of `AnalyzerOverlay.tsx`. They describe the spectrum, so they sit
//! next to it rather than in the header, frosted because a picker over a live
//! spectrum is otherwise unreadable. The view and range pickers are the
//! header's — see [`super::header`] — as they were in the original bar.

use std::sync::Arc;

use nih_plug_egui::egui::Ui;

use crate::gui::gpu::FxRenderer;
use crate::gui::state::{AnalyzerMode, UiState};
use crate::gui::widgets::menu::{self, Align};

/// The dB ranges the plot axis can be set to.
pub const DB_RANGES: [f32; 4] = [6.0, 12.0, 18.0, 30.0];

/// The fractional-octave smoothing widths the analyser offers.
pub const SMOOTHING: [(f32, &str); 5] = [
    (0.0, "Raw"),
    (1.0 / 24.0, "1/24 oct"),
    (1.0 / 12.0, "1/12 oct"),
    (1.0 / 6.0, "1/6 oct"),
    (1.0 / 3.0, "1/3 oct"),
];

/// How the spectrum is being measured.
pub fn analyzer(ui: &mut Ui, state: &mut UiState, fx: &Arc<FxRenderer>) {
    ui.spacing_mut().item_spacing.x = 6.0;

    let modes: Vec<&str> = AnalyzerMode::ALL.iter().map(|m| m.label()).collect();
    let selected = AnalyzerMode::ALL
        .iter()
        .position(|m| *m == state.analyzer_mode)
        .unwrap_or(3);
    if let Some(i) = menu::dropdown(
        ui,
        ui.id().with("analyzer"),
        "Analyzer",
        &modes,
        selected,
        Align::End,
        fx,
    ) {
        state.analyzer_mode = AnalyzerMode::ALL[i];
    }

    let labels: Vec<&str> = SMOOTHING.iter().map(|(_, l)| *l).collect();
    let selected = SMOOTHING
        .iter()
        .position(|(v, _)| (*v - state.spectrum_smoothing).abs() < 1e-4)
        .unwrap_or(2);
    if let Some(i) = menu::dropdown(
        ui,
        ui.id().with("smooth"),
        "Smooth",
        &labels,
        selected,
        Align::End,
        fx,
    ) {
        state.spectrum_smoothing = SMOOTHING[i].0;
    }
}
