use std::fs;
use std::path::Path;

/// `include_dir!` needs `editor/dist` to exist at compile time, but a fresh clone has
/// never run the Vite build. Drop a placeholder page in so the plugin still compiles
/// and boots — it tells you to build the UI instead of failing the crate.
fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("editor/dist");
    let index = dist.join("index.html");

    if !index.exists() {
        fs::create_dir_all(&dist).expect("could not create editor/dist");
        fs::write(
            &index,
            r#"<!doctype html><meta charset="utf-8">
<body style="background:#0b0b0d;color:#fff;font:14px system-ui;display:grid;place-items:center;height:100vh;margin:0">
<div style="text-align:center">
  <div style="font-size:20px;font-weight:600">EQUZX — UI not built</div>
  <div style="opacity:.6;margin-top:8px">Run <code>bun install &amp;&amp; bun run build</code> in <code>editor/</code>, then rebuild the plugin.</div>
</div>"#,
        )
        .expect("could not write placeholder index.html");
    }

    println!("cargo:rerun-if-changed=editor/dist");
}
