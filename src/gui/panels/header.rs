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
use crate::gui::state::{AbSlot, ChannelView, Snapshot, UiState};
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

    ui.spacing_mut().item_spacing.x = 10.0;

    // --- identity -------------------------------------------------------
    //
    // The wordmark of `assets/logo.svg`, at the sixteen pixels of cap height
    // the old bar gave the image: four letters set solid, and the thin-stroke
    // X the logotype signs off with, drawn because no cut this UI ships is
    // that light.
    let (mark, _) = ui.allocate_exact_size(vec2(84.0, PILL_HEIGHT), Sense::hover());
    let word = ui.painter().text(
        pos2(mark.min.x + 4.0, mark.center().y),
        Align2::LEFT_CENTER,
        "EQUZ",
        theme::semibold(22.0),
        Color32::WHITE,
    );
    let (x0, cy, xw, xh) = (word.max.x + 2.5, mark.center().y, 13.0, 15.6);
    for (a, b) in [
        (pos2(x0, cy - xh / 2.0), pos2(x0 + xw, cy + xh / 2.0)),
        (pos2(x0 + xw, cy - xh / 2.0), pos2(x0, cy + xh / 2.0)),
    ] {
        ui.painter()
            .line_segment([a, b], nih_plug_egui::egui::Stroke::new(1.1, Color32::WHITE));
    }
    chrome::divider(ui, 20.0);

    // --- A/B ------------------------------------------------------------
    if let Some(picked) = chrome::segmented(
        ui,
        &["A", "B"],
        ui_state.slot as usize,
        theme::NEON,
        28.0,
        PILL_HEIGHT,
        theme::semibold(theme::SMALL),
    ) {
        let wanted = if picked == 0 { AbSlot::A } else { AbSlot::B };
        if wanted != ui_state.slot {
            swap_slots(frame, ui_state);
        }
    }
    if copy_button(ui, ui_state.slot).clicked() {
        ui_state.parked = edit::capture(frame.params);
    }

    preset_menu(ui, frame, fx, state, ui_state);

    // --- everything that acts on the picture or the output, pushed right --
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 10.0;

        // Laid out right to left, so this reads bottom-up against the original.
        let more = Id::new("more-menu");
        let anchor = menu::trigger(ui, more, 32.0, |ui, rect, fg| {
            for dx in [-4.0f32, 0.0, 4.0] {
                ui.painter()
                    .circle_filled(pos2(rect.center().x + dx, rect.center().y), 1.3, fg);
            }
        });
        menu::popup(ui, more, anchor, MenuAlign::End, 196.0, fx, |ui, close| {
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

        chrome::divider(ui, 20.0);

        // --- what the plot shows: the view and range pickers --------------
        // These describe the picture rather than the sound, but the original
        // kept them here at the bar's right end, ahead of the output group.
        let ranges: Vec<String> = super::overlays::DB_RANGES
            .iter()
            .map(|r| format!("± {r:.0} dB"))
            .collect();
        let range_refs: Vec<&str> = ranges.iter().map(String::as_str).collect();
        let range_current = super::overlays::DB_RANGES
            .iter()
            .position(|r| (*r - ui_state.db_range).abs() < 0.001)
            .unwrap_or(2);
        if let Some(i) = menu::dropdown(
            ui,
            Id::new("range-menu"),
            "Range",
            &range_refs,
            range_current,
            MenuAlign::End,
            fx,
        ) {
            ui_state.db_range = super::overlays::DB_RANGES[i];
        }

        let views: Vec<&str> = ChannelView::ALL.iter().map(|v| v.label()).collect();
        let view_current = ChannelView::ALL
            .iter()
            .position(|v| *v == ui_state.channel_view)
            .unwrap_or(0);
        if let Some(i) = menu::dropdown(
            ui,
            Id::new("view-menu"),
            "View",
            &views,
            view_current,
            MenuAlign::End,
            fx,
        ) {
            ui_state.channel_view = ChannelView::ALL[i];
        }
    });
}

/// `A → B`, with the arrow drawn rather than typed — the bundled fonts have no
/// arrow, and a missing glyph renders as an empty box.
fn copy_button(ui: &mut Ui, slot: AbSlot) -> nih_plug_egui::egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(52.0, PILL_HEIGHT), Sense::click());
    let fill = Fill::Quiet;
    let hover = crate::gui::anim::state(ui.ctx(), response.id, response.hovered(), 0.16);
    chrome::pill_bg(ui, rect, PILL_HEIGHT / 2.0, fill, hover);

    let fg = fill.foreground(hover);
    let font = FontId::proportional(theme::SMALL);
    ui.painter().text(
        pos2(rect.min.x + 12.0, rect.center().y),
        Align2::CENTER_CENTER,
        slot.label(),
        font.clone(),
        fg,
    );
    ui.painter().add(glyph::arrow_right(
        Rect::from_center_size(rect.center(), vec2(14.0, 8.0)),
        white(120),
        1.4,
    ));
    ui.painter().text(
        pos2(rect.max.x - 12.0, rect.center().y),
        Align2::CENTER_CENTER,
        slot.other().label(),
        font,
        fg,
    );

    response.on_hover_text(format!(
        "Copy slot {} into {}",
        slot.label(),
        slot.other().label()
    ))
}

fn output_gain(ui: &mut Ui, frame: &Frame) {
    let value = frame.params.output_gain.value();
    let format = |v: f32| format!("{}{:.1} dB", if v > 0.0 { "+" } else { "" }, v);

    // Sized to the widest value the knob can show, so the pill hugs its
    // contents without breathing while the value is dragged.
    let caption_w = menu::text_width(ui, &format(-24.0), &theme::medium(theme::SMALL))
        .max(menu::text_width(ui, "OUT", &theme::caption()));
    let width = 6.0 + 24.0 + 6.0 + caption_w + 12.0;

    // Allocated rather than read off the cursor: this sits inside the header's
    // right-to-left group, where the cursor's left edge is negative infinity
    // because the layout has not yet decided how far left the run reaches.
    let (rect, response) = ui.allocate_exact_size(vec2(width, PILL_HEIGHT), Sense::hover());

    // A pill drawn round the dial, rather than the dial drawn inside a button:
    // the plate has to be down before the knob paints over it.
    chrome::pill_bg(ui, rect, PILL_HEIGHT / 2.0, Fill::Quiet, 0.0);

    // The knob hugs whatever the value currently measures; centring the block
    // keeps the pill balanced while the reserved width holds still.
    let shown = menu::text_width(ui, &format(value), &theme::medium(theme::SMALL))
        .max(menu::text_width(ui, "OUT", &theme::caption()));
    let content = 24.0 + 6.0 + shown;
    let slack = ((rect.width() - content) / 2.0).max(2.0);
    let mut inner = ui.new_child(
        nih_plug_egui::egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(rect.min.x + slack, rect.min.y + 2.0),
                pos2(rect.max.x - 2.0, rect.max.y - 2.0),
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    if let Some(v) = Knob::new("Out", value, -24.0, 12.0, &format)
        .default_value(0.0)
        .size(24.0)
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
    let hover = crate::gui::anim::state(ui.ctx(), response.id, response.hovered(), 0.16);
    chrome::pill_bg(ui, rect, PILL_HEIGHT / 2.0, fill, hover);
    let fg = fill.foreground(hover);
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
    let caption_w = menu::text_width(ui, &caption, &theme::caption());
    let value_w = menu::text_width(ui, &shown, &theme::medium(theme::SMALL)).min(120.0);
    let anchor = menu::trigger(ui, id, caption_w + value_w + 49.0, |ui, rect, _| {
        let painter = ui.painter();
        painter.text(
            pos2(rect.min.x + 12.0, rect.center().y),
            Align2::LEFT_CENTER,
            &caption,
            theme::caption(),
            white(89),
        );
        painter.text(
            pos2(rect.min.x + 12.0 + caption_w + 8.0, rect.center().y),
            Align2::LEFT_CENTER,
            &shown,
            theme::medium(theme::SMALL),
            white(230),
        );
        painter.add(glyph::chevron(
            Rect::from_center_size(pos2(rect.max.x - 12.0, rect.center().y), vec2(9.0, 9.0)),
            crate::gui::anim::state(ui.ctx(), id.with("chev"), menu::is_open(ui, id), 0.15),
            white(89),
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
                    pos2(row.rect.max.x - 18.0, row.rect.center().y),
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
            let save = chrome::pill_compact(
                ui,
                menu::spaced("Save").trim_end(),
                Fill::Solid(theme::NEON),
                None,
                28.0,
            );
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
