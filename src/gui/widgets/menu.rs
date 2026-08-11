//! Popovers, and the rows that go in them.
//!
//! A port of `components/ui/Menu.tsx`. egui has popups of its own, but not ones
//! that can be right-aligned to their trigger or given a frosted backdrop, and
//! both of those are load-bearing here — the analyser controls hang off the
//! right edge of the plot, and a popover over a live spectrum is unreadable
//! without something between it and the curve.

use std::sync::Arc;

use nih_plug_egui::egui::{
    epaint::StrokeKind, vec2, Align2, Area, Color32, FontId, Id, Key, Order, Pos2, Rect, Response,
    Sense, Shape, Stroke, Ui,
};

use crate::gui::gpu::FxRenderer;
use crate::gui::theme::{self, fade, white, NEON};
use crate::gui::widgets::chrome::{self, Fill, PILL_HEIGHT};
use crate::gui::widgets::glyph;

pub fn is_open(ui: &Ui, id: Id) -> bool {
    ui.ctx().data(|d| d.get_temp::<bool>(id).unwrap_or(false))
}

pub fn set_open(ui: &Ui, id: Id, open: bool) {
    ui.ctx().data_mut(|d| d.insert_temp(id, open));
}

pub fn toggle(ui: &Ui, id: Id) {
    let open = is_open(ui, id);
    set_open(ui, id, !open);
}

/// Which edge of the trigger the panel lines up with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    End,
}

/// Show a popover under `anchor`, if `id` is open.
///
/// Opens and closes with an eased fade and a short slide; while it is on its
/// way out the rows still draw but no longer take input. Closes on Escape, on
/// a click anywhere outside it, or when the contents set the flag they are
/// handed — which is how a menu row that did something gets the panel out of
/// the way afterwards.
pub fn popup(
    ui: &Ui,
    id: Id,
    anchor: Rect,
    align: Align,
    width: f32,
    fx: &Arc<FxRenderer>,
    contents: impl FnOnce(&mut Ui, &mut bool),
) {
    let open = is_open(ui, id);
    let reveal = crate::gui::anim::state(ui.ctx(), id.with("reveal"), open, 0.16);
    if !open && reveal <= 0.01 {
        return;
    }

    let x = match align {
        Align::Start => anchor.min.x,
        Align::End => anchor.max.x - width,
    };
    let mut close = false;

    let area = Area::new(id.with("panel"))
        .order(Order::Foreground)
        .fixed_pos(Pos2::new(x, anchor.max.y + 8.0 - 6.0 * (1.0 - reveal)))
        .constrain(true)
        .show(ui.ctx(), |ui| {
            ui.set_opacity(reveal);
            if !open {
                ui.disable();
            }
            ui.set_max_width(width);
            let slot = chrome::reserve_glass(ui);
            let inner = ui
                .vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add_space(4.0);
                    contents(ui, &mut close);
                    ui.add_space(4.0);
                })
                .response
                .rect;
            let rect = Rect::from_min_size(inner.min, vec2(width, inner.height()))
                .expand2(vec2(0.0, 0.0));
            let mut style = crate::gui::gpu::Glass::panel(0.7);
            style.corner_radius = theme::R_MENU as f32;
            style.opacity = reveal;
            chrome::fill_glass(ui, slot, fx, rect, style, None);
            rect
        });

    let panel = area.inner;
    let clicked_outside = ui.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| !panel.contains(p) && !anchor.contains(p))
    });
    if close || clicked_outside || ui.input(|i| i.key_pressed(Key::Escape)) {
        set_open(ui, id, false);
    }
}

/// One selectable row — `px-2.5 py-1.5 text-[11px]` in the old sheet. The
/// highlight sits six points in from both panel edges; asymmetric slack here
/// is immediately visible on the selected row's pill.
pub fn item(ui: &mut Ui, text: &str, selected: bool, danger: bool) -> Response {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, 28.0), Sense::click());

    let fg = if danger {
        if response.hovered() {
            Color32::from_rgb(0xff, 0x9a, 0x9a)
        } else {
            white(150)
        }
    } else if selected {
        theme::MOCHI
    } else if response.hovered() {
        white(240)
    } else {
        white(180)
    };

    if response.hovered() || selected {
        let bg = if danger {
            fade(Color32::from_rgb(0xff, 0x4d, 0x4d), 0.15)
        } else if selected {
            fade(NEON, 0.18)
        } else {
            white(24)
        };
        ui.painter()
            .rect_filled(rect.shrink2(vec2(6.0, 0.0)), theme::corner(12), bg);
    }

    ui.painter().text(
        Pos2::new(rect.min.x + 16.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(theme::SMALL),
        fg,
    );
    if selected {
        // The tick the old menu drew on the selected row.
        let c = Pos2::new(rect.max.x - 18.0, rect.center().y);
        ui.painter().add(Shape::Path(
            nih_plug_egui::egui::epaint::PathShape {
                points: vec![
                    Pos2::new(c.x - 4.0, c.y),
                    Pos2::new(c.x - 1.0, c.y + 3.0),
                    Pos2::new(c.x + 4.5, c.y - 3.0),
                ],
                closed: false,
                fill: Color32::TRANSPARENT,
                stroke: nih_plug_egui::egui::epaint::PathStroke::new(1.6, fg),
            },
        ));
    }
    response
}

/// The uppercase caption that heads a group of rows.
pub fn label(ui: &mut Ui, text: &str) {
    ui.add_space(3.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        chrome::caption(ui, text);
    });
    ui.add_space(1.0);
}

pub fn divider(ui: &mut Ui) {
    ui.add_space(3.0);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 1.0), Sense::hover());
    ui.painter().rect_filled(
        Rect::from_min_max(
            Pos2::new(rect.min.x + 6.0, rect.min.y),
            Pos2::new(rect.max.x - 6.0, rect.max.y),
        ),
        0,
        white(18),
    );
    ui.add_space(3.0);
}

/// A trigger pill that opens a popover, with an optional chevron.
///
/// Returns the trigger's rectangle so the caller can anchor the panel to it.
pub fn trigger(ui: &mut Ui, id: Id, width: f32, draw: impl FnOnce(&Ui, Rect, Color32)) -> Rect {
    trigger_with(ui, id, width, Fill::Quiet, draw)
}

/// The same, with the resting fill chosen by the caller — `Fill::None` lets a
/// trigger sit flush on a plate another control already painted, instead of
/// stamping its own rounded pill over it.
pub fn trigger_with(
    ui: &mut Ui,
    id: Id,
    width: f32,
    resting: Fill,
    draw: impl FnOnce(&Ui, Rect, Color32),
) -> Rect {
    let open = is_open(ui, id);
    let (rect, response) = ui.allocate_exact_size(vec2(width, PILL_HEIGHT), Sense::click());
    let fill = if open { Fill::Lit } else { resting };
    let hover = crate::gui::anim::state(ui.ctx(), response.id, response.hovered(), 0.16);
    chrome::pill_bg(ui, rect, PILL_HEIGHT / 2.0, fill, hover);
    draw(ui, rect, fill.foreground(hover));
    if response.clicked() {
        set_open(ui, id, !open);
    }
    rect
}

/// The compact `LABEL value ⌄` picker that replaced a row of segmented buttons.
///
/// Returns the index the user chose, if they chose one.
pub fn dropdown(
    ui: &mut Ui,
    id: Id,
    caption_text: &str,
    options: &[&str],
    selected: usize,
    align: Align,
    fx: &Arc<FxRenderer>,
) -> Option<usize> {
    let current = options.get(selected).copied().unwrap_or("—");
    let font_caption = theme::caption();
    // `font-medium text-white/90` on the value in the original.
    let font_value = theme::medium(theme::SMALL);

    let caption_w = text_width(ui, &spaced(caption_text), &font_caption);
    let value_w = options
        .iter()
        .map(|o| text_width(ui, o, &font_value))
        .fold(0.0f32, f32::max);
    // px-3 either side, gap-2 between label, value and chevron.
    let width = caption_w + value_w + 12.0 + 8.0 + 8.0 + 9.0 + 12.0;

    let anchor = trigger(ui, id, width, |ui, rect, _| {
        let painter = ui.painter();
        let mut x = rect.min.x + 12.0;
        painter.text(
            Pos2::new(x, rect.center().y),
            Align2::LEFT_CENTER,
            spaced(caption_text),
            font_caption.clone(),
            white(89),
        );
        x += caption_w + 8.0;
        painter.text(
            Pos2::new(x, rect.center().y),
            Align2::LEFT_CENTER,
            current,
            font_value.clone(),
            white(230),
        );
        painter.add(glyph::chevron(
            Rect::from_center_size(
                Pos2::new(rect.max.x - 12.0, rect.center().y),
                vec2(9.0, 9.0),
            ),
            crate::gui::anim::state(ui.ctx(), id.with("chev"), is_open(ui, id), 0.15),
            white(89),
        ));
    });

    let mut picked = None;
    popup(
        ui,
        id,
        anchor,
        align,
        (value_w + 60.0).max(132.0),
        fx,
        |ui, close| {
            for (i, option) in options.iter().enumerate() {
                if item(ui, option, i == selected, false).clicked() {
                    picked = Some(i);
                    *close = true;
                }
            }
        },
    );
    picked
}

/// Letter-spacing, which egui has no notion of, faked with thin spaces — the
/// captions in this UI are all uppercase and tracked out, and without it they
/// read as shouting rather than as labels.
pub fn spaced(text: &str) -> String {
    let mut out: String = text
        .to_uppercase()
        .chars()
        .flat_map(|c| [c, '\u{2009}'])
        .collect();
    out.pop();
    out
}

pub fn text_width(ui: &Ui, text: &str, font: &FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER)
        .size()
        .x
}

/// A single-line text field, styled to match the pills around it.
pub fn text_field(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 28.0), Sense::hover());
    let corner = theme::corner(14);
    ui.painter()
        .rect_filled(rect, corner, Color32::from_black_alpha(160));

    let response = ui.put(
        rect.shrink2(vec2(8.0, 2.0)),
        nih_plug_egui::egui::TextEdit::singleline(text)
            .hint_text(hint)
            .frame(false)
            .font(FontId::proportional(theme::SMALL))
            .desired_width(width - 16.0),
    );
    ui.painter().rect_stroke(
        rect,
        corner,
        Stroke::new(
            1.0,
            if response.has_focus() {
                fade(NEON, 0.6)
            } else {
                white(24)
            },
        ),
        StrokeKind::Inside,
    );
    response
}
