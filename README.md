# EQUZX

A 24-band dynamic mid/side parametric EQ, built as a VST3 and CLAP plugin.
By Futureboard Digital Technologies.

The DSP is Rust ([nih-plug]); the interface is the React app in `editor/`,
running in an embedded webview.

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

The UI has to be built first — it is compiled into the plugin binary.

```sh
cd editor && bun install && bun run build && cd ..
cargo xtask bundle equzx --release
```

That writes `target/bundled/EQUZX.vst3` and `target/bundled/EQUZX.clap`. Copy
them where your host looks for plugins; on Windows that is usually
`C:\Program Files\Common Files\VST3` and `C:\Program Files\Common Files\CLAP`.

Both formats are *directory bundles* on both platforms, and the `.vst3` /
`.clap` suffix on the outer directory is how a host finds them — a bundle that
has been renamed or flattened into a bare `.dll` is invisible. On Windows the
binary inside is itself named `EQUZX.vst3`
(`EQUZX.vst3\Contents_64-win\EQUZX.vst3`); on macOS it is
`EQUZX.vst3/Contents/MacOS/EQUZX`. CI asserts both layouts on every run.

### Installers

```sh
# Windows — needs Inno Setup 6
iscc /DVersion=2026.8.11 installer\windows\equzx.iss

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
pull request: editor, `cargo test`, bundles (universal on macOS), then both
installers, uploaded as artifacts. Pushing a `v*` tag additionally publishes the
installers to a GitHub release.

`build.rs` drops a placeholder page into `editor/dist` when the Vite build has
not been run, so the crate still compiles from a fresh clone — you just get a
window telling you to build the UI.

### Working on the UI

`cargo run --bin equzx-standalone` opens the editor wrapped around the system
audio device, with no DAW in the loop. Point it at a dev server for hot reload:

```sh
cd editor && bun run dev          # in one terminal
EQUZX_UI_URL=http://localhost:5173/ cargo run --bin equzx-standalone
```

`EQUZX_UI_URL` overrides the embedded bundle for that run only.

The same React app also runs as a plain web page (`bun run dev` on its own).
There it drives a Web Audio graph instead of the plugin — drop an audio file on
the window and the EQ processes it. Which engine is in play is decided once, in
`App.tsx`, and nothing below that knows the difference.

### Tests

```sh
cargo test          # DSP, analyser, protocol, asset server, plugin integration
cd editor && bun run build   # type-checks the UI as part of the build
```

## How it fits together

```
       ┌── editor/ (React) ──────────────┐        ┌── src/ (Rust) ─────────────┐
       │  App.tsx                        │        │  lib.rs      plugin entry  │
       │   ├── AudioEngine   (web page)  │        │  params.rs   24 band slots │
       │   └── PluginBridge  (plugin) ───┼── IPC ─┼─ editor.rs   frame loop    │
       │  EQDisplay, BandStrip, Header   │        │  dsp/        the EQ        │
       └─────────────────────────────────┘        │  analyzer.rs FFT + curves  │
                       ▲                          │  assets.rs   serves the UI │
                       └────── HTTP (loopback) ───┴────────────────────────────┘
```

A few decisions worth knowing about:

**Band slots are parameters.** A host needs a parameter list that never changes
shape, so the plugin exposes a fixed array of 24 band slots and "adding a band"
claims a free one. A band's slot index is its identity on both sides of the
bridge, which is what keeps an automation lane pointed at the band the user drew.

**The chain moves between two domains.** Left/right and mid/side are two views
of the same signal, and a left-only filter is not expressible as a pair of
independent mid and side filters — so no single domain serves every band. The
two views are related by an exactly invertible transform costing four operations
a sample, so the chain simply carries the signal in whichever domain the next
band needs and converts in place when it crosses between them. A stereo band
needs neither in particular: one filter on both buses commutes with the
transform, so it runs wherever the chain already is.

**The analyser reduces before it sends.** A 8192-point FFT is 4096 bins; the
plugin reduces that to 512 log-spaced points, quantises each to a byte and
base64s the result, so a frame of both curves is about 1.4 kB rather than tens
of kilobytes of JSON.

**The UI never hears its own edits.** Every UI action sets parameters
immediately, so the frame loop skips the state push on any frame that processed
one — otherwise a drag would come back a frame late and churn the whole session
sixty times a second.

**The UI is served over loopback HTTP.** The natural choice is wry's custom
protocol; on current WebView2 the interception never fires and the page never
loads. See `src/assets.rs`.

## Known limitations

* The standalone wrapper's WASAPI backend needs a period size matching what the
  device hands it, e.g. `--period-size 1056`; otherwise nih-plug's cpal backend
  panics before any audio flows. This is the dev harness, not the plugin.
* `webview/` vendors [nih-plug-webview] with two fixes. See its README.
* Presets are stored in the webview's local storage, so they are per-machine.
  The plugin's own state (bands, gains, view settings) is saved with the DAW
  session as normal.
* The macOS build has never been run — it is written and wired into CI, but
  every check in this repo so far has happened on Windows.

[nih-plug]: https://github.com/robbert-vdh/nih-plug
[nih-plug-webview]: https://github.com/httnn/nih-plug-webview
