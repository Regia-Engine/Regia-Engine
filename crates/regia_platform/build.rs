// SPDX-License-Identifier: MPL-2.0
//! Sets the `has_window` cfg: the `window` feature, or Android, which always has the window half
//! (spec 76 § 2.1, ADR-1886). One name instead of a two-clause `cfg` on every gated item.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_window)");
    println!("cargo::rerun-if-changed=build.rs");
    let android = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android");
    if android || std::env::var_os("CARGO_FEATURE_WINDOW").is_some() {
        println!("cargo::rustc-cfg=has_window");
    }
}
