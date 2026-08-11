//! The two pickers parked over the upper corners of the plot.
//!
//! A port of `ViewOverlay.tsx` and `AnalyzerOverlay.tsx`. They describe the
//! picture rather than the sound, so they sit *on* the display rather than in
//! the header — what is being shown at the top left, how it is being analysed at
//! the top right. Deliberately the same shape, one at each corner, and both
//! frosted, because a picker over a live spectrum is otherwise unreadable.

use std::sync::Arc;

use nih_plug_egui::egui::Ui;

use crate::gui::gpu::FxRenderer;
use crate::gui::state::{AnalyzerMode, ChannelView, UiState};
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

/// What the plot is showing: which slice of the stereo image, and how tall.
pub fn view(ui: &mut Ui, state: &mut UiState, fx: &Arc<FxRenderer>) {
    ui.spacing_mut().item_spacing.x = 5.0;

    let views: Vec<&str> = ChannelView::ALL.iter().map(|v| v.label()).collect();
    let selected = ChannelView::ALL
        .iter()
        .position(|v| *v == state.channel_view)
        .unwrap_or(0);
    if let Some(i) = menu::dropdown(
        ui,
        ui.id().with("view"),
        "View",
        &views,
        selected,
        Align::Start,
        fx,
    ) {
        state.channel_view = ChannelView::ALL[i];
    }

    let labels: Vec<String> = DB_RANGES.iter().map(|r| format!("± {r:.0} dB")).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let selected = DB_RANGES
        .iter()
        .position(|r| (*r - state.db_range).abs() < 0.001)
        .unwrap_or(2);
    if let Some(i) = menu::dropdown(
        ui,
        ui.id().with("range"),
        "Range",
        &refs,
        selected,
        Align::Start,
        fx,
    ) {
        state.db_range = DB_RANGES[i];
    }
}

/// How the spectrum is being measured.
pub fn analyzer(ui: &mut Ui, state: &mut UiState, fx: &Arc<FxRenderer>) {
    ui.spacing_mut().item_spacing.x = 5.0;

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
