//! The top bar: identity, A/B, presets, output, bypass.
//!
//! A port of `Header.tsx`. The view and range pickers that used to be described
//! here live over the plot instead — see [`super::overlays`] — so what is left
//! is everything that acts on the sound rather than on the picture.

use std::sync::Arc;

use nih_plug_egui::egui::{
    pos2, vec2, Align, Align2, Color32, FontId, Id, Layout, Rect, Sense, Ui,
};

use crate::gui::edit::{self, Frame};
use crate::gui::gpu::FxRenderer;
use crate::gui::presets;
use crate::gui::state::{AbSlot, Snapshot, UiState};
use crate::gui::theme::{self, white};
use crate::gui::widgets::chrome::{self, Fill, PILL_HEIGHT};
use crate::gui::widgets::glyph;
use crate::gui::widgets::menu::{self, Align as MenuAlign};
use crate::gui::widgets::Knob;

/// What the header needs to keep between frames: the preset list it last read,
/// and the name being typed into it.
#[derive(Default)]
pub struct HeaderState {
    pub presets: Vec<String>,
    pub current: String,
    pub draft: String,
    /// A short-lived message shown in place of the preset name.
    pub status: Option<(String, f64)>,
    pub loaded: bool,
}

impl HeaderState {
    fn flash(&mut self, ui: &Ui, message: &str) {
        self.status = Some((message.to_owned(), ui.input(|i| i.time) + 2.0));
    }

    fn refresh(&mut self) {
        self.presets = presets::list();
    }
}

pub fn show(
    ui: &mut Ui,
    frame: &Frame,
    fx: &Arc<FxRenderer>,
    state: &mut HeaderState,
    ui_state: &mut UiState,
) {
    if !state.loaded {
        state.loaded = true;
        state.refresh();
    }
    if state
        .status
        .as_ref()
        .is_some_and(|(_, until)| ui.input(|i| i.time) > *until)
    {
        state.status = None;
    }

    ui.spacing_mut().item_spacing.x = 8.0;

    // --- identity -------------------------------------------------------
    let (mark, _) = ui.allocate_exact_size(vec2(64.0, PILL_HEIGHT), Sense::hover());
    ui.painter().text(
        pos2(mark.min.x + 4.0, mark.center().y),
        Align2::LEFT_CENTER,
        menu::spaced("EQUZX"),
        FontId::proportional(theme::BODY),
        white(225),
    );
    chrome::divider(ui, 18.0);

    // --- A/B ------------------------------------------------------------
    if let Some(picked) = chrome::segmented(ui, &["A", "B"], ui_state.slot as usize, theme::NEON, 24.0)
    {
        let wanted = if picked == 0 { AbSlot::A } else { AbSlot::B };
        if wanted != ui_state.slot {
            swap_slots(frame, ui_state);
        }
    }
    let copy = chrome::pill(
        ui,
        &format!("{} → {}", ui_state.slot.label(), ui_state.slot.other().label()),
        Fill::Quiet,
    );
    if copy.clicked() {
        ui_state.parked = edit::capture(frame.params);
    }
    copy.on_hover_text(format!(
        "Copy slot {} into {}",
        ui_state.slot.label(),
        ui_state.slot.other().label()
    ));

    preset_menu(ui, frame, fx, state, ui_state);

    // --- everything that acts on the output, pushed to the right --------
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;

        // Laid out right to left, so this reads bottom-up against the original.
        let more = Id::new("more-menu");
        let anchor = menu::trigger(ui, more, 28.0, |ui, rect, fg| {
            for dx in [-4.0f32, 0.0, 4.0] {
                ui.painter()
                    .circle_filled(pos2(rect.center().x + dx, rect.center().y), 1.3, fg);
            }
        });
        menu::popup(ui, more, anchor, MenuAlign::End, 200.0, fx, |ui, close| {
            if menu::item(ui, "Reset to flat", false, true).clicked() {
                edit::reset_all(frame);
                *close = true;
            }
            menu::divider(ui);
            for line in [
                "Click display — add band",
                "Scroll handle — Q / slope",
                "Right-drag handle — solo",
                "B — bypass · X — swap A/B",
                "Del — remove · Esc — deselect",
            ] {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        nih_plug_egui::egui::RichText::new(line)
                            .font(FontId::proportional(theme::TINY))
                            .color(white(90)),
                    );
                });
            }
        });

        let bypassed = frame.params.bypass.value();
        let bypass = bypass_button(ui, bypassed);
        if bypass.clicked() {
            edit::set_bool(frame.setter, &frame.params.bypass, !bypassed);
        }
        bypass.on_hover_text("Bypass the whole EQ (B)");

        super::resonance::show(ui, frame, fx);

        output_gain(ui, frame);
    });
}

fn output_gain(ui: &mut Ui, frame: &Frame) {
    let value = frame.params.output_gain.value();
    let format = |v: f32| format!("{}{:.1} dB", if v > 0.0 { "+" } else { "" }, v);

    // Allocated rather than read off the cursor: this sits inside the header's
    // right-to-left group, where the cursor's left edge is negative infinity
    // because the layout has not yet decided how far left the run reaches.
    let (rect, response) = ui.allocate_exact_size(vec2(104.0, PILL_HEIGHT), Sense::hover());

    // A pill drawn round the dial, rather than the dial drawn inside a button:
    // the plate has to be down before the knob paints over it.
    chrome::pill_bg(ui, rect, PILL_HEIGHT / 2.0, Fill::Quiet, false);

    let mut inner = ui.new_child(
        nih_plug_egui::egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(7.0, 2.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    if let Some(v) = Knob::new("Out", value, -24.0, 12.0, &format)
        .default_value(0.0)
        .size(22.0)
        .inline(true)
        .show(&mut inner)
    {
        edit::set_float(frame.setter, &frame.params.output_gain, v);
    }
    response.on_hover_text("Output gain — drag the knob, double-click to reset");
}

fn bypass_button(ui: &mut Ui, bypassed: bool) -> nih_plug_egui::egui::Response {
    let width = 78.0;
    let (rect, response) = ui.allocate_exact_size(vec2(width, PILL_HEIGHT), Sense::click());
    let fill = if bypassed { Fill::Armed } else { Fill::Quiet };
    chrome::pill_bg(ui, rect, PILL_HEIGHT / 2.0, fill, response.hovered());
    let fg = fill.foreground(response.hovered());
    ui.painter().add(glyph::power(
        Rect::from_center_size(pos2(rect.min.x + 16.0, rect.center().y), vec2(12.0, 12.0)),
        fg,
        1.6,
    ));
    ui.painter().text(
        pos2(rect.min.x + 27.0, rect.center().y),
        Align2::LEFT_CENTER,
        "Bypass",
        FontId::proportional(theme::SMALL),
        fg,
    );
    response
}

/// Park the live settings, take up whatever was parked, and remember which slot
/// is now live.
fn swap_slots(frame: &Frame, ui_state: &mut UiState) {
    let live = edit::capture(frame.params);
    let parked = ui_state.parked.clone();
    edit::apply_snapshot(frame, &parked);
    ui_state.parked = live;
    ui_state.slot = ui_state.slot.other();
}

fn preset_menu(
    ui: &mut Ui,
    frame: &Frame,
    fx: &Arc<FxRenderer>,
    state: &mut HeaderState,
    _ui_state: &mut UiState,
) {
    let id = Id::new("preset-menu");
    let shown = state
        .status
        .as_ref()
        .map(|(text, _)| text.clone())
        .unwrap_or_else(|| {
            if state.current.is_empty() {
                "Init".to_owned()
            } else {
                state.current.clone()
            }
        });

    let caption = menu::spaced("Preset");
    let caption_w = menu::text_width(ui, &caption, &FontId::proportional(theme::MICRO));
    let value_w = menu::text_width(ui, &shown, &FontId::proportional(theme::SMALL)).min(120.0);
    let anchor = menu::trigger(ui, id, caption_w + value_w + 36.0, |ui, rect, fg| {
        let painter = ui.painter();
        painter.text(
            pos2(rect.min.x + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            &caption,
            FontId::proportional(theme::MICRO),
            white(95),
        );
        painter.text(
            pos2(rect.min.x + 10.0 + caption_w + 6.0, rect.center().y),
            Align2::LEFT_CENTER,
            &shown,
            FontId::proportional(theme::SMALL),
            fg,
        );
        painter.add(glyph::chevron(
            Rect::from_center_size(pos2(rect.max.x - 11.0, rect.center().y), vec2(10.0, 10.0)),
            menu::is_open(ui, id),
            white(110),
        ));
    });

    menu::popup(ui, id, anchor, MenuAlign::Start, 250.0, fx, |ui, close| {
        menu::label(
            ui,
            if state.presets.is_empty() {
                "No presets yet"
            } else {
                "Saved"
            },
        );

        let mut to_delete: Option<String> = None;
        for name in state.presets.clone() {
            ui.horizontal(|ui| {
                let selected = name == state.current;
                let row = menu::item(ui, &name, selected, false);
                // The delete cross, over the right end of the row it belongs to.
                let cross = Rect::from_center_size(
                    pos2(row.rect.max.x - 12.0, row.rect.center().y),
                    vec2(16.0, 16.0),
                );
                let hit = ui.interact(cross, ui.id().with(("del", &name)), Sense::click());
                if row.hovered() || hit.hovered() {
                    let c = cross.center();
                    let color = if hit.hovered() {
                        Color32::from_rgb(0xff, 0x9a, 0x9a)
                    } else {
                        white(110)
                    };
                    for (a, b) in [((-3.5, -3.5), (3.5, 3.5)), ((3.5, -3.5), (-3.5, 3.5))] {
                        ui.painter().line_segment(
                            [pos2(c.x + a.0, c.y + a.1), pos2(c.x + b.0, c.y + b.1)],
                            nih_plug_egui::egui::Stroke::new(1.5, color),
                        );
                    }
                }
                if hit.clicked() {
                    to_delete = Some(name.clone());
                } else if row.clicked() {
                    match presets::load(&name) {
                        Some(snapshot) => {
                            edit::apply_snapshot(frame, &snapshot);
                            state.current = name.clone();
                            *close = true;
                        }
                        None => state.flash(ui, "Not found"),
                    }
                }
            });
        }
        if let Some(name) = to_delete {
            presets::delete(&name);
            if state.current == name {
                state.current.clear();
            }
            state.refresh();
            state.flash(ui, "Deleted");
        }

        menu::divider(ui);
        menu::label(ui, "Save current");
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            let field = menu::text_field(
                ui,
                &mut state.draft,
                if state.current.is_empty() {
                    "Preset name"
                } else {
                    &state.current
                },
                156.0,
            );
            let submitted =
                field.lost_focus() && ui.input(|i| i.key_pressed(nih_plug_egui::egui::Key::Enter));
            let save = chrome::pill(ui, "Save", Fill::Solid(theme::NEON));
            if save.clicked() || submitted {
                let name = if state.draft.trim().is_empty() {
                    state.current.clone()
                } else {
                    state.draft.trim().to_owned()
                };
                if name.is_empty() {
                    state.flash(ui, "Name it first");
                } else if presets::save(&name, &Snapshot::capture(frame.params)) {
                    state.current = name;
                    state.draft.clear();
                    state.refresh();
                    state.flash(ui, "Saved");
                } else {
                    state.flash(ui, "Could not save");
                }
            }
        });

        menu::divider(ui);
        // The folder is the exchange format now: saving a preset is exporting
        // it, and importing one is dropping a file in.
        if menu::item(ui, "Open preset folder…", false, false).clicked() {
            presets::reveal();
            *close = true;
        }
        if menu::item(ui, "Reload from disk", false, false).clicked() {
            state.refresh();
            state.flash(ui, "Reloaded");
        }
    });
}
