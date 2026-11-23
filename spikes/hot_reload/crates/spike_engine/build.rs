//! Computes `ABI_HASH` (specs/20 § 2): rustc version + engine version + profile + target + features.
//! A game library built against a different engine build refuses to load.

use std::process::Command;

fn fnv1a(bytes: &[u8], mut h: u64) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = Command::new(rustc)
        .arg("-vV")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut features: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CARGO_FEATURE_"))
        .map(|(k, _)| k)
        .collect();
    features.sort();
    let mut h = 0xcbf2_9ce4_8422_2325;
    for part in [
        version,
        std::env::var("CARGO_PKG_VERSION").unwrap_or_default(),
        std::env::var("PROFILE").unwrap_or_default(),
        std::env::var("TARGET").unwrap_or_default(),
        std::env::var("OPT_LEVEL").unwrap_or_default(),
        features.join(","),
    ] {
        h = fnv1a(part.as_bytes(), h);
    }
    println!("cargo:rustc-env=SPIKE_ABI_HASH={h}");
    println!("cargo:rerun-if-changed=build.rs");
}
