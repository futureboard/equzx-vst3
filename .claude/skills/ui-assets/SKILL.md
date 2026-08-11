---
name: ui-assets
description: How EQUZX embeds fonts and vector art in the egui editor, and how to verify UI changes visually. Use when adding or changing fonts, icons, the logo, or any visual asset in src/gui.
---

# EQUZX UI assets

## Fonts

Embedded at compile time from `assets/fonts/` — nothing is loaded from the host
machine. The typeface is Mona Sans (SIL OFL 1.1; keep `OFL.txt` next to the
files). Three static cuts are wired in `src/gui/theme.rs::fonts()`:

- **Regular** leads `FontFamily::Proportional` — all body text.
- **Medium** is the `"mona-medium"` family — `theme::medium(size)`, and
  `theme::caption()` for the 9px uppercase captions.
- **SemiBold** is `"mona-semibold"` — `theme::semibold(size)`, wordmark only.

egui's bundled fonts stay behind each cut in its family list. Never remove
them: the captions are tracked with U+2009 thin spaces (`menu::spaced`) and any
glyph the lead font lacks falls through instead of drawing an empty box. The
test `every_character_the_ui_draws_has_a_glyph` enforces this — run it after
any font change.

To add a weight: drop the TTF in `assets/fonts/`, add it to the array in
`theme.rs::fonts()`, give it a family there, add a helper beside `medium()`.

## Vector art

No SVG renderer ships in the plugin. The convention (see
`src/gui/widgets/glyph.rs`) is to mirror each SVG path as egui shapes in the
same design-space box, flattening quadratics by hand:

- Band-type glyphs: 20×17 box, from the old web UI's SVGs.
- The brand is `assets/logo.svg` — the official EQUZX logotype from the old
  web editor (four solid letters, thin-stroke X). The header renders it as
  Mona Sans SemiBold "EQUZ" plus a drawn thin X at 16px cap height; keep that
  drawing in step with the SVG. `assets/logo-lockup.svg` is a secondary
  bell-mark lockup, mirrored by `glyph::logo` in the same 24×16 box.

The layout ground truth was the original web editor (an `editor.old/` folder,
since removed): its Tailwind classes fixed the sizes the egui port now
carries — header 54px with h-8 (32px) pills and 10px gaps, chips 28px/r6,
filter buttons 32px/r12, dials 48px in 64pt columns, slope 28×23, channel
27px, panel default 232px, panel min 176px, 12px window insets. Treat those
numbers as the spec when layout drifts.

## Seeing what it looks like

- `cargo test --lib` — layout, tessellation and glyph coverage, headless.
- `cargo test --lib render_the_editor -- --ignored` — writes
  `target/equzx-preview.png`, software-rasterised (GPU callbacks skipped, so
  glass panels show their flat fallback).
- Real thing: `cargo build --release --bin equzx-standalone`, run it, and
  screenshot with PowerShell `Graphics.CopyFromScreen` — no DAW needed, same
  baseview + egui_glow path as the plugin.
- `EQUZX_DISABLE_FX=1` disables every custom GL pass (glass, bloom) for A/B
  against the flat fallback.
- `EQUZX_TUNE=1` opens a slider window scaling the glass and spectrum
  quantities live (`src/gui/tune.rs`); shipped defaults are all 1.0.

## Shipping

`cargo xtask bundle equzx --release` writes `target/bundled/EQUZX.vst3`; the
installed copy lives at `C:\Program Files\Common Files\VST3\EQUZX.vst3`
(bundle directory — copy the whole tree, not just the DLL).
