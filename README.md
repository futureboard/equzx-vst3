# EQUZX

A 24-band dynamic mid/side parametric EQ, built as a VST3 and CLAP plugin.
By Futureboard Digital Technologies.

All Rust ([nih-plug]). The interface is [egui] on baseview's OpenGL context —
one process, one language, and the frosted panels and the glow on the curve are
real shader passes rather than a browser approximating them.

## What it does

* 24 band slots, each a low cut, low shelf, bell, notch, band pass, high shelf
  or high cut.
* Every band can act on the full stereo image, on left or right alone, or on
  mid or side alone.
* Every gain-bearing band can be **dynamic**: its gain moves by up to a set
  range as the level in its own region crosses a threshold, with attack,
  release and a soft knee.
* Cut slopes from 12 to 96 dB/oct, as Butterworth cascades.
* Pre and post spectrum analyser, per-band level and gain-reduction meters,
  A/B slots, presets, solo, and an output trim.

## Building

```sh
cargo xtask bundle equzx --release
```

That writes `target/bundled/EQUZX.vst3` and `target/bundled/EQUZX.clap`. Copy
them where your host looks for plugins; on Windows that is usually
`C:\Program Files\Common Files\VST3` and `C:\Program Files\Common Files\CLAP`.

Both formats are *directory bundles* on both platforms, and the `.vst3` /
`.clap` suffix on the outer directory is how a host finds them — a bundle that
has been renamed or flattened into a bare `.dll` is invisible. On Windows the
binary inside is itself named `EQUZX.vst3`
(`EQUZX.vst3\Contents\x86_64-win\EQUZX.vst3`); on macOS it is
`EQUZX.vst3/Contents/MacOS/EQUZX`. CI asserts both layouts on every run.

On Linux the build additionally needs ALSA, JACK and X11 development headers,
which the standalone wrapper's audio and windowing backends link against:

```sh
sudo apt install libasound2-dev libjack-jackd2-dev libx11-xcb-dev libgl1-mesa-dev
```

### Installers

```sh
# Windows — needs Inno Setup 6
iscc /DVersion=2026.8.13 installer\windows\equzx.iss

# macOS
installer/macos/build-pkg.sh
```

Both land in `target/installer/`. The Windows installer offers VST3 and CLAP as
separate components and installs into the shared plug-in folders; the macOS
package installs into `/Library/Audio/Plug-Ins/`. Signing and notarisation on
macOS are opt-in through `CODESIGN_IDENTITY`, `PRODUCTSIGN_IDENTITY` and
`NOTARY_PROFILE`, so an unsigned local build needs no identity.

### Versioning

`version.json` at the repo root is the source of truth: CI names its artifacts
from it and both installers stamp themselves with it. The scheme is CalVer,
`YYYY.M.D`, starting at **2026.8.11** — which is also valid semver, so hosts can
still parse it. Cargo needs its own literal in `Cargo.toml`; a test in
`src/version.rs` fails the build if the two drift apart.

### CI

`.github/workflows/build.yml` builds on Windows and macOS for every push and
pull request: `cargo test`, bundles (universal on macOS), then both installers,
uploaded as artifacts. Pushing a `v*` tag additionally publishes the installers
to a GitHub release.

### Working on the UI

```sh
cargo run --bin equzx-standalone
```

Opens the editor wrapped around the system audio device, with no DAW in the
loop. There is no separate UI build step and nothing to serve — the editor is
part of the crate, so `cargo run` after an edit is the whole cycle.

### Tests

```sh
cargo test          # DSP, analyser, curves, presets, plugin integration
```

## How it fits together

```
   ┌── src/gui/ ───────────────────────┐   ┌── src/ ────────────────────────┐
   │  mod.rs        layout, shortcuts  │   │  lib.rs       plugin entry     │
   │  display.rs    plot + handles ────┼───┼─ params.rs    24 band slots    │
   │  panels/       header, bands      │   │  dsp/         the EQ           │
   │  widgets/      knob, glass, menus │   │  analyzer.rs  FFT + curves     │
   │  gpu.rs        blur + bloom ──────┼─┐ │  meters.rs    what the audio   │
   │  curves.rs     response grid      │ │ │               thread published │
   └───────────────────────────────────┘ │ └────────────────────────────────┘
                                         └── OpenGL, via baseview + egui
```

A few decisions worth knowing about:

**Band slots are parameters.** A host needs a parameter list that never changes
shape, so the plugin exposes a fixed array of 24 band slots and "adding a band"
claims a free one. A band's slot index is its identity everywhere, which is what
keeps an automation lane pointed at the band the user drew.

**The chain moves between two domains.** Left/right and mid/side are two views
of the same signal, and a left-only filter is not expressible as a pair of
independent mid and side filters — so no single domain serves every band. The
two views are related by an exactly invertible transform costing four operations
a sample, so the chain simply carries the signal in whichever domain the next
band needs and converts in place when it crosses between them. A stereo band
needs neither in particular: one filter on both buses commutes with the
transform, so it runs wherever the chain already is.

**The analyser still reduces.** An 8192-point FFT is 4096 bins, most of them
crowded into the top octave of a logarithmic display. The analyser reduces to
512 log-spaced points — interpolating where a point falls inside a bin, keeping
the peak where dozens of bins fall inside a point — and the UI reads that array
in place. There is no serialisation left in the path.

**The curve on screen is the curve being applied.** The display evaluates the
same [`Coeffs`](src/dsp/biquad.rs) the audio thread runs, over a precomputed
frequency grid (`gui/curves.rs`) that caches the four sinusoids per grid point.
Without that cache a full display is around ninety thousand transcendental
evaluations a frame; with it, the same work is a few hundred microseconds.

**Glass and glow are shader passes.** `gui/gpu.rs` hands egui a paint callback,
which the glow renderer invokes mid-frame with the live GL context — so whatever
has been drawn up to that point is sitting in the default framebuffer. A frosted
panel emits its callback *before* painting itself and gets exactly the backdrop a
sheet of glass would; the plot emits one after its curves and gets a real
screen-space bloom. Both use a dual-Kawase blur: successive halvings with a
five-tap filter down and a tent-weighted eight-tap back up, so a radius of tens
of pixels only ever convolves over a sixty-fourth of the region. Every effect
sits on top of a plain painted fallback, so a driver that refuses the shaders
gives flat panels rather than holes.

## Known limitations

* **HiDPI on Windows depends on the host.** The whole layout is in egui points,
  so it scales correctly wherever the DPI is known — but the editor only learns
  it from the host, through VST3's `IPlugViewContentScaleSupport`. A host that
  never calls it leaves `nih_plug_egui` on its default factor of 1.0 and the
  interface renders 1:1, which on a 4K display is small. The window can be
  dragged larger, which gives more room rather than larger text.
* The standalone wrapper's WASAPI backend needs a period size matching what the
  device hands it, e.g. `--period-size 1056`; otherwise nih-plug's cpal backend
  panics before any audio flows. This is the dev harness, not the plugin.
* Presets are files in `<config>/EQUZX/presets`, so they are per-machine. The
  plugin's own state (bands, gains, view settings, window size) is saved with the
  DAW session as normal.
* The macOS build has never been run — it is written and wired into CI, but
  every check in this repo so far has happened on Windows and Linux.

[nih-plug]: https://github.com/robbert-vdh/nih-plug
[egui]: https://github.com/emilk/egui
