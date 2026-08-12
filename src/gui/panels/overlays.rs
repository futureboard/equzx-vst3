//! The analyser pickers parked over the top-right of the plot.
//!
//! A port of `AnalyzerOverlay.tsx`. They describe the spectrum, so they sit
//! next to it rather than in the header, frosted because a picker over a live
//! spectrum is otherwise unreadable. The view and range pickers are the
//! header's — see [`super::header`] — as they were in the original bar.

use std::sync::Arc;

use nih_plug_egui::egui::Ui;

use crate::gui::edit::{self, Frame};
use crate::gui::gpu::FxRenderer;
use crate::gui::state::{AnalyzerMode, BandView, UiState};
use crate::gui::theme::MOCHI;
use crate::gui::widgets::chrome::{self, Fill};
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
pub fn analyzer(
    ui: &mut Ui,
    state: &mut UiState,
    fx: &Arc<FxRenderer>,
    frame: &Frame,
    bands: &[BandView],
    selected: &mut Option<usize>,
) {
    ui.spacing_mut().item_spacing.x = 4.0;

    let inverted = frame.params.phase_invert.value();
    let phase = chrome::pill_action(
        ui,
        "Phase",
        if inverted { Fill::Armed } else { Fill::Quiet },
    );
    if phase.clicked() {
        edit::set_bool(frame.setter, &frame.params.phase_invert, !inverted);
    }
    phase.on_hover_text("Invert output polarity by 180 degrees");

    if let Some(band) = selected.and_then(|slot| bands.iter().find(|band| band.slot == slot)) {
        let on = chrome::pill_action(
            ui,
            if band.enabled { "On" } else { "Off" },
            if band.enabled { Fill::Lit } else { Fill::Quiet },
        );
        if on.clicked() {
            edit::set_enabled(frame, band.slot, !band.enabled);
        }
        on.on_hover_text("Enable or bypass this band");

        let soloed = frame.transient.solo() == Some(band.slot);
        let solo = chrome::pill_action(
            ui,
            "Solo",
            if soloed {
                Fill::Solid(MOCHI)
            } else {
                Fill::Quiet
            },
        );
        if solo.clicked() {
            frame
                .transient
                .set_solo(if soloed { None } else { Some(band.slot) });
        }
        solo.on_hover_text("Solo this band — or right-drag its handle on the display");

        let del = chrome::pill_action(ui, "Del", Fill::Quiet);
        if del.clicked() {
            edit::remove_band(frame, band.slot);
            *selected = None;
        }
        del.on_hover_text("Delete this band");

    }

    // One deliberate break separates selected-band actions from analyser
    // configuration without making either group feel boxed in.
    ui.add_space(8.0);
    chrome::divider(ui, 18.0);
    ui.add_space(4.0);

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
