//! Standalone EQUZX — the plugin wrapped around the system audio device.
//!
//! Useful for working on the UI without a DAW in the loop:
//! `cargo run --bin equzx-standalone` opens the editor, and `--help` lists the
//! backend, device and buffer options nih-plug's standalone wrapper accepts.

use nih_plug::prelude::*;

use equzx::Equzx;

fn main() {
    nih_export_standalone::<Equzx>();
}
