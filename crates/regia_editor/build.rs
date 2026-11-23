// SPDX-License-Identifier: MPL-2.0
//! Bakes the hot-reload ABI hash (rustc version + package version + target + profile).
//!
//! The editor passes the same decimal string to the game build as `REGIA_ABI_HASH`.
//! Specs/20 § 2.
//!
//! On a Windows target it also embeds the editor's icon (`assets/icon/regia-editor.ico`, drawn by
//! `cargo xtask editor-icons`) and version info into `regia-editor.exe` through `winresource`, so
//! Explorer, the taskbar and Alt-Tab show it (ADR-20000). With no resource compiler (`rc.exe` from
//! the Windows SDK, or MinGW `windres` when cross-compiling) the build warns and goes on without.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let version = std::process::Command::new(&rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|| "unknown-rustc".to_string());
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let text = format!("{version}|regia|{pkg}|{target}|{profile}");
    println!("cargo:rustc-env=REGIA_ABI_HASH={}", fnv1a64(text.as_bytes()));
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && let Err(err) = embed_windows_resources()
    {
        println!("cargo:warning=regia-editor.exe gets no icon or version info ({err})");
    }
}

/// The icon and the version info of `regia-editor.exe` (the other binaries of the package get none).
fn embed_windows_resources() -> Result<(), String> {
    let dir =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").ok_or("no CARGO_MANIFEST_DIR")?);
    let ico = dir.join("assets/icon/regia-editor.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    let ico = ico.to_str().ok_or("the icon path is not UTF-8")?;
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico)
        .set("ProductName", "Regia Editor")
        .set("FileDescription", "Regia Editor")
        .set("OriginalFilename", "regia-editor.exe")
        .set("InternalName", "regia-editor");
    res.compile_for("regia-editor")
        .map_err(|err| format!("resource compiler: {err}"))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
