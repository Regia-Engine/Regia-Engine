// SPDX-License-Identifier: MIT-0
//! Desktop window of the product viewer, for development: `product_viewer [--packages DIR] [--open SLUG]
//! [--fetch URL]`.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(target_os = "android"))]
fn main() -> std::process::ExitCode {
    product_viewer::run_desktop()
}

#[cfg(target_os = "android")]
fn main() {}
