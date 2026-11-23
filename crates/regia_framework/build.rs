// SPDX-License-Identifier: MPL-2.0
//! Bakes the toolchain half of the extension ABI stamp (`specs/57` § 2.8, ADR-1885): a BLAKE3 hash
//! of `rustc -vV`, the target triple and a hash of the build profile (profile, opt-level, panic
//! strategy, debug assertions). `extension::stamp` includes the generated file, so a dynamic
//! extension records the values of the engine build it linked and the host compares them with its own.

use std::fmt::Write as _;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let verbose = std::process::Command::new(&rustc)
        .arg("-vV")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| out.stdout)
        .unwrap_or_else(|| b"unknown-rustc".to_vec());
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    let target = var("TARGET");
    let profile = format!(
        "profile={}|opt={}|panic={}|debug_assertions={}",
        var("PROFILE"),
        var("OPT_LEVEL"),
        var("CARGO_CFG_PANIC"),
        std::env::var("CARGO_CFG_DEBUG_ASSERTIONS").is_ok()
    );
    let mut out = String::new();
    let _ = writeln!(
        out,
        "/// BLAKE3 of `rustc -vV` of the compiler that built this engine.\npub const BUILD_RUSTC_HASH: [u8; 32] = {:?};",
        blake3::hash(&verbose).as_bytes()
    );
    let _ = writeln!(
        out,
        "/// Target triple this engine was built for.\npub const BUILD_TARGET: &str = {target:?};"
    );
    let _ = writeln!(
        out,
        "/// BLAKE3 of the build profile (profile, opt-level, panic strategy, debug assertions).\npub const BUILD_PROFILE_HASH: [u8; 32] = {:?};",
        blake3::hash(profile.as_bytes()).as_bytes()
    );
    let dir = std::env::var("OUT_DIR").unwrap_or_else(|_| ".".to_owned());
    let path = std::path::Path::new(&dir).join("extension_stamp.rs");
    if let Err(error) = std::fs::write(&path, out) {
        println!("cargo:warning=could not write {}: {error}", path.display());
    }
}
