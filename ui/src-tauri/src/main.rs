//! Window process for the WinXtend UI.
//!
//! Three lines on purpose: everything worth testing lives in the library half of the
//! crate, which `cargo test` can build without a webview.

// No console window behind the UI in a release build. Left attached in debug builds
// so panics and `tracing` output from the engine crates are visible while working.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    winxtend_ui_lib::run();
}
