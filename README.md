# EQUZX

A 24-band dynamic mid/side parametric EQ, built as a VST3 and CLAP plugin.
By Futureboard Digital Technologies.

The DSP is Rust ([nih-plug]); the interface is the React app in `editor/`,
running in an embedded webview.

## What it does

* 24 band slots, each a low cut, low shelf, bell, notch, band pass, high shelf
  or high cut.
* Every band can act on the full stereo image, on mid only, or on side only.
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

**The audio always runs in mid/side.** Encoding to M/S, running each band on the
buses it belongs to and decoding back costs nothing — a stereo band on both M
and S is the same work as running it on L and R, and the result is identical —
but it means the topology never changes, so switching a band's channel is not a
graph rebuild and cannot click.

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

[nih-plug]: https://github.com/robbert-vdh/nih-plug
[nih-plug-webview]: https://github.com/httnn/nih-plug-webview
