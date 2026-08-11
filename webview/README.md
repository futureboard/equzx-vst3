# `nih_plug_webview` (vendored)

A verbatim copy of [nih-plug-webview](https://github.com/httnn/nih-plug-webview)
by Max Huttunen, ISC licensed — see `LICENSE`.

It is vendored rather than pulled from git for one reason: upstream depends on
`baseview` at whatever its git HEAD happens to be. baseview's master has since
rewritten its windowing API (`Window::open_parented` became `Window::create`)
and moved to `raw-window-handle` 0.6, so the upstream crate no longer builds.
Cargo refuses to `[patch]` a git source with the same git source, so the only
way to pin the dependency is to own the manifest.

`src/script.js` is unmodified. `src/lib.rs` carries two additions, both marked
in place:

* `pump_foreign_messages`, called at the top of the frame callback on Windows.
  WebView2 does its asynchronous work through window messages sent to hidden
  windows it owns, and baseview's blocking event loop uses an HWND-filtered
  `GetMessageW`, which never returns them. Without this the webview is created,
  reports the right URL, and then sits on a blank document forever — under the
  standalone wrapper the editor simply never appears.
* A re-export of `wry::Error`/`wry::Result`, so a custom protocol handler can
  name them in its own signature.

The file has also been run through `cargo fmt` along with the rest of the
workspace. If upstream picks up the baseview pin and the message pump, this
directory can be deleted and the dependency pointed back at git.
