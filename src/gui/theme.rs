//! The palette and the base egui style.
//!
//! Ported from `theme.ts` and the `@theme` block the old Tailwind build owned.
//! Everything the UI draws comes from here, so there is one place to change a
//! colour and one place that decides how large "small text" is.

use std::sync::Arc;

use nih_plug_egui::egui::{
    epaint, style::Selection, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId,
    Stroke, Style, TextStyle, Visuals,
};

/// Hot signal pink. Anything actively shaping the sound is drawn in this.
pub const NEON: Color32 = Color32::from_rgb(0xff, 0x4d, 0x9d);
/// The soft pastel end of the same hue — highlights, solo, secondary text.
pub const MOCHI: Color32 = Color32::from_rgb(0xff, 0xd3, 0xe4);

/// Neutral surfaces. Hue-free on purpose, so the pink is the only colour.
pub const SURFACE_DEEP: Color32 = Color32::from_rgb(0x10, 0x10, 0x14);
pub const SURFACE_HUB: Color32 = Color32::from_rgb(0x25, 0x25, 0x2c);
pub const SURFACE_ROOT: Color32 = Color32::from_rgb(0x09, 0x09, 0x0c);
/// Top and bottom of the field the plot sits on.
pub const PLOT_TOP: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0b);
pub const PLOT_BOTTOM: Color32 = Color32::from_rgb(0x12, 0x12, 0x14);

/// Per-band accents. One hue, alternating pale and vivid so that neighbouring
/// bands stay tellable apart without breaking the monochrome scheme.
pub const BAND_COLORS: [Color32; 12] = [
    Color32::from_rgb(0xff, 0xe1, 0xee),
    Color32::from_rgb(0xff, 0x4d, 0x9d),
    Color32::from_rgb(0xff, 0xb3, 0xd1),
    Color32::from_rgb(0xff, 0x2e, 0x8b),
    Color32::from_rgb(0xff, 0xd0, 0xe2),
    Color32::from_rgb(0xff, 0x6f, 0xb0),
    Color32::from_rgb(0xff, 0x90, 0xc0),
    Color32::from_rgb(0xe8, 0x1f, 0x77),
    Color32::from_rgb(0xff, 0xc4, 0xda),
    Color32::from_rgb(0xff, 0x5a, 0xa3),
    Color32::from_rgb(0xff, 0xa0, 0xc8),
    Color32::from_rgb(0xff, 0x7a, 0xb5),
];

pub fn band_color(index: usize) -> Color32 {
    BAND_COLORS[index % BAND_COLORS.len()]
}

/// White at a given opacity — the `text-white/45` of the old stylesheet.
pub const fn white(alpha: u8) -> Color32 {
    Color32::from_rgba_premultiplied(alpha, alpha, alpha, alpha)
}

/// A colour at a fraction of its opacity, premultiplied the way egui wants.
pub fn fade(color: Color32, alpha: f32) -> Color32 {
    let a = alpha.clamp(0.0, 1.0);
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), (a * 255.0).round() as u8)
}

/// Linear blend between two colours, in sRGB space — which is where the CSS
/// this was ported from did its blending too.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
}

/// The same, alpha included — what a hover fade interpolates through. Both
/// ends are premultiplied, so channel-wise interpolation stays premultiplied.
pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        lerp(a.a(), b.a()),
    )
}

// --- type scale -------------------------------------------------------------
//
// The old UI was built on a 9/10/11px scale. egui's default text is much
// larger, so every size the port uses is named here rather than sprinkled
// through the layout code.

/// 9px — the uppercase micro-captions above a control.
pub const MICRO: f32 = 9.0;
/// 10px — meter readouts and axis labels.
pub const TINY: f32 = 10.0;
/// 11px — the default for buttons and menu rows.
pub const SMALL: f32 = 11.0;
/// 12px — the one step up, used for emphasis inside a tooltip.
pub const BODY: f32 = 12.0;

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// The medium cut — captions and control labels, which sit uppercase at 9px
/// and need the extra weight to hold their colour against the glass.
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mona-medium".into()))
}

/// The semibold cut — the wordmark and nothing else, so far.
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mona-semibold".into()))
}

/// The uppercase micro-caption every control wears.
pub fn caption() -> FontId {
    medium(MICRO)
}

/// The embedded typeface: Mona Sans (SIL OFL 1.1), three static cuts. egui's
/// bundled fonts stay behind each one in its family, so a glyph Mona Sans does
/// not carry — the thin space the captions are tracked with, an odd symbol —
/// falls through to a font that has it instead of drawing an empty box.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    for (name, bytes) in [
        (
            "MonaSans-Regular",
            &include_bytes!("../../assets/fonts/MonaSans-Regular.ttf")[..],
        ),
        (
            "MonaSans-Medium",
            &include_bytes!("../../assets/fonts/MonaSans-Medium.ttf")[..],
        ),
        (
            "MonaSans-SemiBold",
            &include_bytes!("../../assets/fonts/MonaSans-SemiBold.ttf")[..],
        ),
    ] {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }

    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, "MonaSans-Regular".to_owned());

    for (family, lead) in [
        ("mona-medium", "MonaSans-Medium"),
        ("mona-semibold", "MonaSans-SemiBold"),
    ] {
        let mut list = vec![lead.to_owned()];
        list.extend(fallback.iter().cloned());
        fonts.families.insert(FontFamily::Name(family.into()), list);
    }
    fonts
}

/// Corner radii, matching the `rounded-*` classes the port came from.
pub const R_PILL: u8 = 255;
pub const R_PANEL: u8 = 22;
pub const R_MENU: u8 = 16;
pub const R_CHIP: u8 = 6;

pub fn corner(radius: u8) -> CornerRadius {
    CornerRadius::same(radius)
}

/// Install the base style. Called once, when the editor is built.
pub fn apply(ctx: &nih_plug_egui::egui::Context) {
    ctx.set_fonts(fonts());

    // The OpenGL surface is not multisampled; egui's physical-pixel feather is
    // therefore the AA path for all native geometry. Keep it explicit so host
    // or persisted context options cannot leave curves and rounded controls
    // with hard raster edges.
    ctx.tessellation_options_mut(|options| {
        options.feathering = true;
        options.feathering_size_in_pixels = 1.0;
        options.round_line_segments_to_pixels = true;
        options.round_rects_to_pixels = true;
        options.round_text_to_pixels = true;
    });

    let text_styles = [
        (TextStyle::Small, font(MICRO)),
        (TextStyle::Body, font(SMALL)),
        (TextStyle::Button, font(SMALL)),
        (TextStyle::Monospace, mono(TINY)),
        (TextStyle::Heading, font(BODY)),
    ]
    .into();

    let mut visuals = Visuals::dark();
    visuals.panel_fill = SURFACE_ROOT;
    visuals.window_fill = Color32::from_rgb(0x1c, 0x1c, 0x22);
    visuals.extreme_bg_color = Color32::from_rgb(0x05, 0x05, 0x06);
    visuals.override_text_color = Some(white(230));
    visuals.window_corner_radius = corner(R_MENU);
    visuals.menu_corner_radius = corner(R_MENU);
    visuals.window_stroke = Stroke::new(1.0, white(24));
    visuals.selection = Selection {
        bg_fill: fade(NEON, 0.18),
        stroke: Stroke::new(1.0, MOCHI),
    };
    visuals.window_shadow = epaint::Shadow {
        offset: [0, 10],
        blur: 30,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    visuals.popup_shadow = visuals.window_shadow;

    // Widgets are drawn by hand throughout, so egui's own chrome only ever
    // shows up on the bits it owns — scroll bars and text edits.
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.corner_radius = corner(10);
        w.bg_stroke = Stroke::NONE;
    }
    visuals.widgets.inactive.weak_bg_fill = white(10);
    visuals.widgets.hovered.weak_bg_fill = white(20);
    visuals.widgets.active.weak_bg_fill = white(28);

    let mut style = Style {
        text_styles,
        visuals,
        ..Style::default()
    };
    style.spacing.item_spacing = nih_plug_egui::egui::vec2(6.0, 6.0);
    style.spacing.button_padding = nih_plug_egui::egui::vec2(8.0, 4.0);
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.floating = true;
    style.interaction.tooltip_delay = 0.35;

    ctx.set_style(style);
}
