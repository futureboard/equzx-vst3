//! The band editor along the bottom.
//!
//! A port of `BandStrip.tsx`: the chips that select a band on the left, and
//! everything about the selected one on the right — filter shape and its four
//! knobs on the first row, the dynamics section on the second.

use std::sync::Arc;

use nih_plug_egui::egui::{
    pos2, vec2, Align, Align2, Color32, FontId, Layout, Rect, Sense, Stroke, Ui, UiBuilder,
};

use crate::gui::edit::{self, Frame, SLOPES};
use crate::gui::gpu::FxRenderer;
use crate::gui::state::{BandView, PANEL_MIN};
use crate::gui::theme::{self, band_color, fade, white, MOCHI, NEON};
use crate::gui::widgets::chrome::{self, Fill};
use crate::gui::widgets::glyph;
use crate::gui::widgets::Knob;
use crate::params::{BandChannel, BandKind, DynMode, MAX_BANDS};

const KINDS: [BandKind; 7] = [
    BandKind::LowCut,
    BandKind::LowShelf,
    BandKind::Bell,
    BandKind::Notch,
    BandKind::BandPass,
    BandKind::HighShelf,
    BandKind::HighCut,
];

const KIND_LABELS: [&str; 7] = [
    "Low Cut",
    "Low Shelf",
    "Bell",
    "Notch",
    "Band Pass",
    "High Shelf",
    "High Cut",
];

const CHANNELS: [(BandChannel, &str, &str); 5] = [
    (BandChannel::Stereo, "ST", "Stereo — both channels"),
    (BandChannel::Left, "L", "Left — the left signal only"),
    (BandChannel::Right, "R", "Right — the right signal only"),
    (BandChannel::Mid, "M", "Mid — the centre of the image"),
    (BandChannel::Side, "S", "Side — the difference between the channels"),
];

/// Bottom of the band-level meter, in dBFS.
const METER_MIN: f32 = -70.0;

fn fmt_band_freq(f: f32) -> String {
    if f >= 1000.0 {
        format!("{:.*} kHz", if f >= 10_000.0 { 1 } else { 2 }, f / 1000.0)
    } else {
        format!("{:.*} Hz", if f < 100.0 { 1 } else { 0 }, f)
    }
}

/// The drag bar between the plot and the panel. Drag up to grow the panel.
pub fn resizer(ui: &mut Ui, width: f32, height: &mut f32, max: f32) {
    let (rect, response) = ui.allocate_exact_size(vec2(width, 12.0), Sense::click_and_drag());
    if response.dragged() {
        *height = (*height - response.drag_delta().y).clamp(PANEL_MIN, max.max(PANEL_MIN));
    }
    if response.double_clicked() {
        *height = crate::gui::state::PANEL_DEFAULT.min(max.max(PANEL_MIN));
    }
    if response.hovered() || response.dragged() {
        ui.ctx()
            .set_cursor_icon(nih_plug_egui::egui::CursorIcon::ResizeVertical);
    }

    let active = response.dragged();
    ui.painter().line_segment(
        [pos2(rect.min.x, rect.min.y), pos2(rect.max.x, rect.min.y)],
        Stroke::new(1.0, if active { fade(NEON, 0.5) } else { white(22) }),
    );
    let grip = Rect::from_center_size(rect.center(), vec2(40.0, 2.0));
    ui.painter().rect_filled(
        grip,
        theme::corner(1),
        if active {
            NEON
        } else if response.hovered() {
            white(100)
        } else {
            white(38)
        },
    );
    response.on_hover_text("Drag to resize the band panel · double-click to reset");
}

/// The panel itself.
pub fn show(
    ui: &mut Ui,
    frame: &Frame,
    _fx: &Arc<FxRenderer>,
    bands: &[BandView],
    selected: &mut Option<usize>,
    height: f32,
    width: f32,
) {
    let rect = Rect::from_min_size(ui.cursor().min, vec2(width, height));
    let mut inner = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(10.0, 8.0)))
            .layout(Layout::left_to_right(Align::Min)),
    );
    inner.spacing_mut().item_spacing = vec2(10.0, 6.0);

    chips(&mut inner, bands, selected, height - 16.0);

    let divider = Rect::from_min_size(
        pos2(rect.min.x + 208.0, rect.min.y + 8.0),
        vec2(1.0, height - 16.0),
    );
    ui.painter().rect_filled(divider, 0, white(28));

    let editor_rect = Rect::from_min_max(
        pos2(divider.max.x + 10.0, rect.min.y + 8.0),
        pos2(rect.max.x - 10.0, rect.max.y - 8.0),
    );
    let mut editor = ui.new_child(
        UiBuilder::new()
            .max_rect(editor_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    editor.spacing_mut().item_spacing = vec2(8.0, 8.0);

    let index = selected.and_then(|slot| bands.iter().position(|b| b.slot == slot));
    match index {
        None => {
            editor.add_space(editor_rect.height() / 2.0 - 10.0);
            editor.label(
                nih_plug_egui::egui::RichText::new(
                    "Click anywhere on the display to create a band.",
                )
                .font(FontId::proportional(theme::SMALL))
                .color(white(80)),
            );
        }
        Some(index) => {
            let band = bands[index];
            let color = band_color(index);
            filter_row(&mut editor, frame, &band, color, selected);
            dynamics_row(&mut editor, frame, &band, color);
        }
    }

    ui.allocate_rect(rect, Sense::hover());
}

/// The band chips, and the count under them.
fn chips(ui: &mut Ui, bands: &[BandView], selected: &mut Option<usize>, height: f32) {
    let rect = Rect::from_min_size(ui.cursor().min, vec2(188.0, height));
    let mut column = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
    );
    column.spacing_mut().item_spacing = vec2(5.0, 5.0);

    // Wrapped in the column rather than in a child with its own rectangle, so
    // the count below lands directly under the last row of chips instead of at
    // the bottom of whatever space the panel happens to have.
    column.horizontal_wrapped(|grid| {
        grid.spacing_mut().item_spacing = vec2(5.0, 5.0);
        if bands.is_empty() {
            grid.label(
                nih_plug_egui::egui::RichText::new("No bands yet")
                    .font(FontId::proportional(theme::TINY))
                    .color(white(64)),
            );
        }
        for (index, band) in bands.iter().enumerate() {
            chip(grid, band, index, selected);
        }
    });

    let full = bands.len() >= MAX_BANDS;
    column.add_space(2.0);
    column.label(
        nih_plug_egui::egui::RichText::new(format!(
            "{} / {} bands{}",
            bands.len(),
            MAX_BANDS,
            if full { " — limit reached" } else { "" }
        ))
        .font(FontId::proportional(theme::MICRO))
        .color(if full { NEON } else { white(64) }),
    );

    ui.allocate_rect(rect, Sense::hover());
}

/// One band chip: its number, its colour, and the two marks that say it is
/// dynamic or that it acts on one side of the image only.
fn chip(ui: &mut Ui, band: &BandView, index: usize, selected: &mut Option<usize>) {
    let color = band_color(index);
    let active = *selected == Some(band.slot);
    let (rect, response) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());

    let corner = theme::corner(theme::R_CHIP);
    if active {
        ui.painter()
            .rect_filled(rect.expand(1.5), theme::corner(8), fade(color, 0.25));
        ui.painter().rect_filled(rect, corner, color);
    } else {
        ui.painter().rect_filled(
            rect,
            corner,
            if band.enabled {
                white(11)
            } else {
                Color32::TRANSPARENT
            },
        );
        ui.painter().rect_stroke(
            rect,
            corner,
            Stroke::new(1.0, white(if response.hovered() { 64 } else { 26 })),
            nih_plug_egui::egui::epaint::StrokeKind::Inside,
        );
    }

    let fg = if active {
        Color32::from_rgba_unmultiplied(0, 0, 0, 225)
    } else if band.enabled {
        white(160)
    } else {
        white(80)
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        format!("{}", index + 1),
        FontId::proportional(theme::SMALL),
        fg,
    );
    if band.dynamic && band.can_be_dynamic() {
        ui.painter().circle_filled(
            pos2(rect.max.x - 3.0, rect.min.y + 3.0),
            2.0,
            if active { Color32::BLACK } else { color },
        );
    }
    if !band.badge().is_empty() {
        ui.painter().text(
            pos2(rect.max.x - 2.0, rect.max.y - 1.0),
            Align2::RIGHT_BOTTOM,
            band.badge(),
            FontId::proportional(7.0),
            if active { Color32::BLACK } else { color },
        );
    }

    if response.clicked() {
        *selected = Some(band.slot);
    }
    response.on_hover_text(format!(
        "{} · {}{}",
        KIND_LABELS[KINDS.iter().position(|k| *k == band.kind).unwrap_or(2)],
        fmt_band_freq(band.freq),
        if band.badge().is_empty() {
            String::new()
        } else {
            format!(" · {}", band.channel.as_wire())
        }
    ));
}

fn filter_row(
    ui: &mut Ui,
    frame: &Frame,
    band: &BandView,
    color: Color32,
    selected: &mut Option<usize>,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;

        // --- filter shape ------------------------------------------------
        for (kind, label) in KINDS.iter().zip(KIND_LABELS) {
            let on = band.kind == *kind;
            let (rect, response) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
            chrome::pill_bg(
                ui,
                rect,
                10.0,
                if on { Fill::Lit } else { Fill::Quiet },
                response.hovered(),
            );
            ui.painter().add(glyph::shape(
                *kind,
                rect.shrink(7.0),
                if on { color } else { white(115) },
                1.6,
            ));
            if response.clicked() {
                edit::set_kind(frame, band.slot, *kind);
            }
            response.on_hover_text(label);
        }

        chrome::divider(ui, 30.0);

        // --- the four knobs ----------------------------------------------
        let gain_fmt = |v: f32| format!("{}{:.1} dB", if v >= 0.0 { "+" } else { "" }, v);
        let q_fmt = |v: f32| format!("{v:.2}");
        let res_fmt = |v: f32| {
            if v > 0.0 {
                format!("{v:.0}%")
            } else {
                "off".to_owned()
            }
        };

        if let Some(v) = Knob::new("Freq", band.freq, 20.0, 22_000.0, &fmt_band_freq)
            .log(true)
            .color(color)
            .show(ui)
        {
            edit::set_freq(frame, band.slot, v);
        }
        if let Some(v) = Knob::new("Gain", band.gain, -30.0, 30.0, &gain_fmt)
            .default_value(0.0)
            .color(color)
            .disabled(!band.kind.uses_gain())
            .show(ui)
        {
            edit::set_gain(frame, band.slot, v);
        }
        if let Some(v) = Knob::new("Q", band.q, 0.025, 40.0, &q_fmt)
            .log(true)
            .default_value(1.0)
            .color(color)
            .disabled(!uses_q(band.kind))
            .show(ui)
        {
            edit::set_q(frame, band.slot, v);
        }
        if let Some(v) = Knob::new("Res", band.resonance, 0.0, 100.0, &res_fmt)
            .default_value(0.0)
            .color(color)
            .show(ui)
        {
            edit::set_band_resonance(frame, band.slot, v);
        }

        // --- slope --------------------------------------------------------
        let is_cut = band.kind.is_cut();
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            chrome::caption(ui, "Slope");
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for slope in SLOPES {
                    let on = band.slope == slope;
                    let response = chrome::pill_sized(
                        ui,
                        &format!("{}", slope.db_per_oct()),
                        if !is_cut {
                            Fill::None
                        } else if on {
                            Fill::Solid(color)
                        } else {
                            Fill::Quiet
                        },
                        Some(26.0),
                    );
                    if response.clicked() && is_cut {
                        edit::set_slope(frame, band.slot, slope);
                    }
                }
            });
            ui.label(
                nih_plug_egui::egui::RichText::new("dB / oct")
                    .font(FontId::proportional(theme::MICRO))
                    .color(white(if is_cut { 64 } else { 26 })),
            );
        });

        // --- channel ------------------------------------------------------
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            chrome::caption(ui, "Channel");
            let labels: Vec<&str> = CHANNELS.iter().map(|(_, short, _)| *short).collect();
            let current = CHANNELS
                .iter()
                .position(|(c, _, _)| *c == band.channel)
                .unwrap_or(0);
            if let Some(i) = chrome::segmented(ui, &labels, current, color, 26.0) {
                edit::set_channel(frame, band.slot, CHANNELS[i].0);
            }
            ui.label(
                nih_plug_egui::egui::RichText::new("L/R · mid/side")
                    .font(FontId::proportional(theme::MICRO))
                    .color(white(64)),
            );
        });

        // --- on / solo / delete, pinned right ------------------------------
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            if chrome::pill(ui, "Del", Fill::Quiet).clicked() {
                edit::remove_band(frame, band.slot);
                *selected = None;
            }
            let soloed = frame.transient.solo() == Some(band.slot);
            let solo = chrome::pill(
                ui,
                "Solo",
                if soloed { Fill::Solid(MOCHI) } else { Fill::Quiet },
            );
            if solo.clicked() {
                frame
                    .transient
                    .set_solo(if soloed { None } else { Some(band.slot) });
            }
            solo.on_hover_text("Solo this band — or right-drag its handle on the display");
            if chrome::pill(
                ui,
                if band.enabled { "On" } else { "Off" },
                if band.enabled { Fill::Lit } else { Fill::Quiet },
            )
            .clicked()
            {
                edit::set_enabled(frame, band.slot, !band.enabled);
            }
        });
    });
}

fn dynamics_row(ui: &mut Ui, frame: &Frame, band: &BandView, color: Color32) {
    let allowed = band.can_be_dynamic();
    let on = band.dynamic && allowed;

    let outer = Rect::from_min_size(
        ui.cursor().min,
        vec2(ui.available_width(), 54.0),
    );
    ui.painter().rect(
        outer,
        theme::corner(14),
        white(10),
        Stroke::new(1.0, white(16)),
        nih_plug_egui::egui::epaint::StrokeKind::Inside,
    );

    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(outer.shrink2(vec2(10.0, 12.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 8.0;

    let toggle = chrome::pill(
        &mut row,
        "Dyn",
        if on {
            Fill::Solid(color)
        } else if allowed {
            Fill::Quiet
        } else {
            Fill::None
        },
    );
    if toggle.clicked() && allowed {
        edit::set_dynamic(frame, band.slot, !band.dynamic);
    }
    toggle.on_hover_text(if allowed {
        "Dynamic mode"
    } else {
        "Dynamics need a band with gain (bell or shelf)"
    });

    if !on {
        // Everything past the switch is inert until the section is armed; the
        // original dimmed it and dropped its pointer events, which is this.
        row.painter().text(
            pos2(row.cursor().min.x + 8.0, outer.center().y),
            Align2::LEFT_CENTER,
            if allowed {
                "Off — the band holds the gain it is drawn at"
            } else {
                "Only a bell or a shelf has a gain to move"
            },
            FontId::proportional(theme::TINY),
            white(60),
        );
        ui.allocate_rect(outer, Sense::hover());
        return;
    }

    let modes = ["above", "below"];
    let current = if band.dyn_mode == DynMode::Above { 0 } else { 1 };
    if let Some(i) = chrome::segmented(&mut row, &modes, current, white(38), 40.0) {
        edit::set_dyn_mode(
            frame,
            band.slot,
            if i == 0 { DynMode::Above } else { DynMode::Below },
        );
    }

    let db = |v: f32| format!("{}{:.1} dB", if v >= 0.0 { "+" } else { "" }, v);
    let thresh = |v: f32| format!("{v:.1} dB");
    let ms = |v: f32| format!("{v:.0} ms");

    if let Some(v) = Knob::new("Range", band.dyn_range, -30.0, 30.0, &db)
        .default_value(-6.0)
        .color(color)
        .size(34.0)
        .show(&mut row)
    {
        edit::set_dyn_range(frame, band.slot, v);
    }
    if let Some(v) = Knob::new("Thresh", band.threshold, -70.0, 0.0, &thresh)
        .default_value(-24.0)
        .color(color)
        .size(34.0)
        .show(&mut row)
    {
        edit::set_threshold(frame, band.slot, v);
    }
    if let Some(v) = Knob::new("Attack", band.attack, 1.0, 300.0, &ms)
        .log(true)
        .default_value(20.0)
        .color(color)
        .size(34.0)
        .show(&mut row)
    {
        edit::set_attack(frame, band.slot, v);
    }
    if let Some(v) = Knob::new("Release", band.release, 10.0, 2000.0, &ms)
        .log(true)
        .default_value(200.0)
        .color(color)
        .size(34.0)
        .show(&mut row)
    {
        edit::set_release(frame, band.slot, v);
    }

    dyn_meter(&mut row, frame, band, color);
    ui.allocate_rect(outer, Sense::hover());
}

/// Live band level against the threshold, plus the gain the band is applying.
fn dyn_meter(ui: &mut Ui, frame: &Frame, band: &BandView, color: Color32) {
    let width = ui.available_width().clamp(130.0, 260.0);
    let rect = Rect::from_min_size(
        pos2(ui.cursor().min.x, ui.max_rect().center().y - 16.0),
        vec2(width, 32.0),
    );

    let level = frame.level.get(band.slot).copied().unwrap_or(METER_MIN);
    let delta = frame.delta.get(band.slot).copied().unwrap_or(0.0);
    let to_fraction = |db: f32| ((db - METER_MIN) / -METER_MIN).clamp(0.0, 1.0);

    let painter = ui.painter();
    painter.text(
        pos2(rect.min.x, rect.min.y),
        Align2::LEFT_TOP,
        crate::gui::widgets::menu::spaced("Band level"),
        FontId::proportional(theme::MICRO),
        white(95),
    );
    painter.text(
        pos2(rect.max.x, rect.min.y),
        Align2::RIGHT_TOP,
        format!("{}{:.1} dB", if delta >= 0.0 { "+" } else { "" }, delta),
        FontId::proportional(theme::MICRO),
        white(180),
    );

    chrome::meter(
        ui,
        Rect::from_min_size(pos2(rect.min.x, rect.min.y + 13.0), vec2(width, 6.0)),
        to_fraction(level),
        Some(to_fraction(band.threshold)),
        color,
    );

    ui.painter().text(
        pos2(rect.min.x, rect.max.y - 8.0),
        Align2::LEFT_CENTER,
        if band.dyn_mode == DynMode::Above {
            "engages above threshold"
        } else {
            "engages below threshold"
        },
        FontId::proportional(theme::MICRO),
        white(64),
    );
    ui.allocate_rect(rect, Sense::hover());
}

/// Shelves are fixed at S = 1 in the engine, so exposing a Q for them would be a
/// control that changes nothing.
fn uses_q(kind: BandKind) -> bool {
    matches!(kind, BandKind::Bell | BandKind::Notch | BandKind::BandPass)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_filter_kind_has_a_button_and_a_name() {
        assert_eq!(KINDS.len(), KIND_LABELS.len());
        // The seven the parameter can hold, in the order the row draws them.
        for kind in [
            BandKind::LowCut,
            BandKind::LowShelf,
            BandKind::Bell,
            BandKind::Notch,
            BandKind::BandPass,
            BandKind::HighShelf,
            BandKind::HighCut,
        ] {
            assert!(KINDS.contains(&kind), "{kind:?} has no button");
        }
    }

    #[test]
    fn every_channel_has_a_badge() {
        assert_eq!(CHANNELS.len(), 5);
        for (channel, short, _) in CHANNELS {
            assert!(!short.is_empty(), "{channel:?}");
        }
    }

    #[test]
    fn band_frequencies_read_the_way_the_panel_shows_them() {
        assert_eq!(fmt_band_freq(48.0), "48.0 Hz");
        assert_eq!(fmt_band_freq(250.0), "250 Hz");
        assert_eq!(fmt_band_freq(1000.0), "1.00 kHz");
        assert_eq!(fmt_band_freq(12_000.0), "12.0 kHz");
    }
}
