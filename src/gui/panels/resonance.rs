//! The header control for the adaptive resonance suppressor.
//!
//! A port of `ResonancePanel.tsx`. The pill is the switch and the meter at once
//! — click to arm the stage, and while it is armed the number beside it is the
//! deepest cut the bank is making right now, which is the one thing worth seeing
//! without opening anything. Everything that shapes *what* it cuts lives behind
//! the chevron.

use std::sync::Arc;

use nih_plug_egui::egui::{vec2, Align2, Color32, FontId, Id, Rect, Sense, Ui};

use crate::gui::edit::{self, Frame};
use crate::gui::gpu::FxRenderer;
use crate::gui::theme::{self, white};
use crate::gui::widgets::chrome::{self, Fill, PILL_HEIGHT};
use crate::gui::widgets::menu::{self, Align};
use crate::gui::widgets::glyph;
use crate::gui::widgets::Knob;

/// The switch and its chevron, as one control.
///
/// Laid out in a sub-`Ui` of its own because the header puts this inside a
/// right-to-left group, and a pair drawn straight into that would come out in
/// the order they were written — chevron first, then the switch it belongs to.
pub fn show(ui: &mut Ui, frame: &Frame, fx: &Arc<FxRenderer>) {
    let enabled = frame.params.resonance.enabled.value();
    let grow = crate::gui::anim::state(
        ui.ctx(),
        nih_plug_egui::egui::Id::new("res-grow"),
        enabled,
        0.18,
    );
    let width = SWITCH_WIDTH + READOUT_WIDTH * grow + CHEVRON_WIDTH;
    ui.allocate_ui_with_layout(
        vec2(width, PILL_HEIGHT),
        nih_plug_egui::egui::Layout::left_to_right(nih_plug_egui::egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            pair(ui, frame, fx);
        },
    );
}

/// Width of the switch, of the reduction read-out it grows by while armed, and
/// of the chevron that completes the pill.
const SWITCH_WIDTH: f32 = 58.0;
const READOUT_WIDTH: f32 = 26.0;
const CHEVRON_WIDTH: f32 = 22.0;

fn pair(ui: &mut Ui, frame: &Frame, fx: &Arc<FxRenderer>) {
    let res = &frame.params.resonance;
    let enabled = res.enabled.value();

    // --- the switch, and the read-out it doubles as ---------------------
    let readout = if enabled {
        if frame.resonance_peak > 0.05 {
            format!("−{:.1}", frame.resonance_peak)
        } else {
            "0.0".to_owned()
        }
    } else {
        String::new()
    };
    let grow = crate::gui::anim::state(
        ui.ctx(),
        nih_plug_egui::egui::Id::new("res-grow"),
        enabled,
        0.18,
    );
    let width = SWITCH_WIDTH + READOUT_WIDTH * grow;
    let (switch_rect, switch) = ui.allocate_exact_size(vec2(width, PILL_HEIGHT), Sense::click());

    let fill = if enabled { Fill::Armed } else { Fill::Quiet };
    // One plate under the whole control — the chevron draws no fill of its
    // own, so there is no seam where the two halves meet.
    let hover = crate::gui::anim::state(ui.ctx(), switch.id, switch.hovered(), 0.16);
    chrome::pill_bg(
        ui,
        switch_rect.with_max_x(switch_rect.max.x + CHEVRON_WIDTH),
        PILL_HEIGHT / 2.0,
        fill,
        hover,
    );
    let fg = fill.foreground(hover);
    // Icon and label as one block, centred in the switch's own width — the
    // readout and chevron grow to the right of it.
    let label_w = menu::text_width(ui, "Res", &FontId::proportional(theme::SMALL));
    let start = switch_rect.min.x + (SWITCH_WIDTH - (13.0 + 4.0 + label_w)) / 2.0;
    ui.painter().add(glyph::resonance(
        Rect::from_center_size(
            nih_plug_egui::egui::pos2(start + 6.5, switch_rect.center().y),
            vec2(13.0, 13.0),
        ),
        fg,
        1.5,
    ));
    ui.painter().text(
        nih_plug_egui::egui::pos2(start + 17.0, switch_rect.center().y),
        Align2::LEFT_CENTER,
        "Res",
        FontId::proportional(theme::SMALL),
        fg,
    );
    if grow > 0.2 {
        ui.painter().text(
            nih_plug_egui::egui::pos2(switch_rect.max.x - 6.0, switch_rect.center().y),
            Align2::RIGHT_CENTER,
            &readout,
            FontId::proportional(theme::SMALL),
            theme::fade(fg, 0.8 * grow),
        );
    }
    if switch.clicked() {
        edit::set_bool(frame.setter, &res.enabled, !enabled);
    }
    switch.on_hover_text(
        "Adaptive resonance suppression — cuts whatever stands out from the spectrum around it",
    );

    // --- the chevron that opens the rest --------------------------------
    let id = Id::new("resonance-menu");
    let anchor = menu::trigger_with(ui, id, CHEVRON_WIDTH, Fill::None, |ui, rect, _| {
        ui.painter().add(glyph::chevron(
            rect,
            crate::gui::anim::state(ui.ctx(), id.with("chev"), menu::is_open(ui, id), 0.15),
            white(110),
        ));
    });
    // The chevron shares the switch's plate, so the pair reads as one control.
    let combined = switch_rect.union(anchor);

    menu::popup(ui, id, combined, Align::End, 300.0, fx, |ui, _| {
        menu::label(ui, "Adaptive resonance");

        let knob = |ui: &mut Ui,
                    label: &str,
                    value: f32,
                    min: f32,
                    max: f32,
                    default: f32,
                    log: bool,
                    format: &dyn Fn(f32) -> String| {
            Knob::new(label, value, min, max, format)
                .log(log)
                .default_value(default)
                .disabled(!enabled)
                .size(34.0)
                .show(ui)
        };

        let percent = |v: f32| format!("{v:.0}%");
        let millis_fine = |v: f32| format!("{v:.1}m");
        let millis = |v: f32| format!("{v:.0}m");
        let plain = |v: f32| format!("{v:.1}");

        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.spacing_mut().item_spacing.x = 4.0;
            if let Some(v) = knob(ui, "Depth", res.depth.value() * 100.0, 0.0, 100.0, 50.0, false, &percent) {
                edit::set_float(frame.setter, &res.depth, v / 100.0);
            }
            if let Some(v) = knob(ui, "Sharp", res.sharpness.value() * 100.0, 0.0, 100.0, 50.0, false, &percent) {
                edit::set_float(frame.setter, &res.sharpness, v / 100.0);
            }
            if let Some(v) = knob(ui, "Thresh", res.threshold.value(), -12.0, 24.0, 6.0, false, &plain) {
                edit::set_float(frame.setter, &res.threshold, v);
            }
            if let Some(v) = knob(ui, "Mix", res.mix.value() * 100.0, 0.0, 100.0, 100.0, false, &percent) {
                edit::set_float(frame.setter, &res.mix, v / 100.0);
            }
        });
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.spacing_mut().item_spacing.x = 4.0;
            if let Some(v) = knob(ui, "Attack", res.attack.value(), 0.5, 100.0, 5.0, true, &millis_fine) {
                edit::set_float(frame.setter, &res.attack, v);
            }
            if let Some(v) = knob(ui, "Rel", res.release.value(), 5.0, 1000.0, 40.0, true, &millis) {
                edit::set_float(frame.setter, &res.release, v);
            }
            if let Some(v) = knob(ui, "Low", res.low.value(), 20.0, 2000.0, 20.0, true, &fmt_hz) {
                edit::set_float(frame.setter, &res.low, v);
            }
            if let Some(v) = knob(ui, "High", res.high.value(), 500.0, 20_000.0, 20_000.0, true, &fmt_hz) {
                edit::set_float(frame.setter, &res.high, v);
            }
        });

        ui.add_space(4.0);
        let delta = res.delta.value();
        let width = ui.available_width() - 8.0;
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let response = chrome::pill_sized(
                ui,
                "Listen to what's removed",
                if delta { Fill::Armed } else { Fill::Quiet },
                Some(width),
            );
            if response.clicked() && enabled {
                edit::set_bool(frame.setter, &res.delta, !delta);
            }
        });

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.add_space(6.0);
            ui.label(
                nih_plug_egui::egui::RichText::new(
                    "Cuts only what stands proud of the spectrum around it, so a sloped mix \
                     passes through and a ringing peak does not. Zero latency.",
                )
                .font(FontId::proportional(theme::TINY))
                .color(white(90)),
            );
        });
    });
}

fn fmt_hz(v: f32) -> String {
    if v >= 1000.0 {
        if v >= 10_000.0 {
            format!("{:.0}k", v / 1000.0)
        } else {
            format!("{:.1}k", v / 1000.0)
        }
    } else {
        format!("{v:.0}")
    }
}

/// Kept so the module's colour use stays in one place if the accent moves.
#[allow(dead_code)]
const ACCENT: Color32 = theme::NEON;
