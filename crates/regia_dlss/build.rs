// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from dlss_wgpu (https://github.com/bevyengine/dlss_wgpu, commit 4046a407cc8d),
// src/initialization.rs, src/feature_info.rs, src/sdk.rs, src/super_resolution.rs, src/nvsdk_ngx.rs
// and build.rs: the NGX extension registration, the feature-discovery block, the NGX init call,
// the evaluate parameter block and the bindgen setup. Copyright (c) the dlss_wgpu contributors.
// Licensed MIT OR Apache-2.0 (see docs/ported-sources.toml); used here under MIT:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions: The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software. THE SOFTWARE IS PROVIDED "AS IS",
// WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS
// OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF
// CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
// OTHER DEALINGS IN THE SOFTWARE.

//! Links the NGX SDK when the `sdk` feature is on and `DLSS_SDK` names an SDK checkout
//! (ADR-2002); otherwise the crate builds as the stub. Never fails the build for a missing SDK:
//! it prints a warning and the stub reports `not_linked` at run time.
//!
//! With the SDK: bindgen reads the NGX headers (the Vulkan helpers), cc compiles a C file
//! wrapping their `static inline` helpers, and the static `nvsdk_ngx` library is linked. The
//! DLSS runtime itself (`libnvidia-ngx-dlss.so.*`, `nvngx_dlss.dll`) is never linked: NGX
//! loads it at run time from the directories the engine passes.
//!
//! `REGIA_DLSS_CHECK_STUB=1` (with `sdk`, no SDK needed) swaps bindgen for the check stub in
//! `check_stub/ngx_bindings.rs` and turns `regia_dlss_linked` on without linking anything, so
//! `cargo check` / `cargo clippy` compile `src/ngx.rs` (the gate's feature matrix, ADR-2322). A build
//! that links fails on the missing NGX symbols: the stub is for checking only.

use std::env;
#[cfg(feature = "sdk")]
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rustc-check-cfg=cfg(regia_dlss_linked)");
    println!("cargo::rerun-if-env-changed=DLSS_SDK");
    println!("cargo::rerun-if-env-changed=VULKAN_SDK");
    println!("cargo::rerun-if-env-changed=REGIA_DLSS_CHECK_STUB");
    let version = link().unwrap_or_else(|why| {
        if env::var_os("CARGO_FEATURE_SDK").is_some() {
            println!("cargo::warning=regia_dlss: building the stub ({why})");
        }
        "none".to_owned()
    });
    println!("cargo::rustc-env=REGIA_DLSS_SDK_VERSION={version}");
}

/// The SDK version string from the redistributable's file name.
///
/// Linux: the suffix of `libnvidia-ngx-dlss.so.<version>` (a dotted version such as `310.9.1`).
/// Windows: `nvngx_dlss.dll` has no version in its name, so the file version in its version
/// resource (`310.9.16` for SDK v310.9.16, ADR-7695), or the word `windows` if that cannot be read.
/// `None` when neither name is in the directory; the caller then records `unknown`.
#[cfg(feature = "sdk")]
fn sdk_version(rel_dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(rel_dir).ok()?;
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find_map(|name| {
            name.strip_prefix("libnvidia-ngx-dlss.so.")
                .map(str::to_owned)
                .or_else(|| {
                    name.eq_ignore_ascii_case("nvngx_dlss.dll").then(|| {
                        std::fs::read(rel_dir.join(&name))
                            .ok()
                            .and_then(|bytes| pe_file_version(&bytes))
                            .unwrap_or_else(|| "windows".to_owned())
                    })
                })
        })
}

/// The file version of a Windows PE image: the first `VS_FIXEDFILEINFO` (signature `0xFEEF04BD`,
/// structure version 1.0, then `dwFileVersionMS` and `dwFileVersionLS`) as `a.b.c`, or `a.b.c.d`
/// when the fourth part is not zero. Microsoft documents the structure in the Win32 version
/// information reference (`VS_FIXEDFILEINFO`).
#[cfg(feature = "sdk")]
fn pe_file_version(bytes: &[u8]) -> Option<String> {
    let mut needle = [0u8; 8];
    needle[..4].copy_from_slice(&0xFEEF_04BD_u32.to_le_bytes());
    needle[4..].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
    let at = bytes.windows(needle.len()).position(|window| window == needle)?;
    let word = |offset: usize| {
        bytes
            .get(at + offset..at + offset + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    let (ms, ls) = (word(8)?, word(12)?);
    let parts = [ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff];
    Some(if parts[3] == 0 {
        format!("{}.{}.{}", parts[0], parts[1], parts[2])
    } else {
        format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3])
    })
}

#[cfg(not(feature = "sdk"))]
fn link() -> Result<String, String> {
    Err("the sdk feature is off".into())
}

#[cfg(feature = "sdk")]
fn link() -> Result<String, String> {
    let target_os = env::var("CARGO_CFG_TARGET_OS").map_err(|e| e.to_string())?;
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").map_err(|e| e.to_string())?;
    if target_os != "linux" && target_os != "windows" {
        return Err(format!("no NGX for {target_os}"));
    }
    if env::var_os("REGIA_DLSS_CHECK_STUB").is_some_and(|v| v == "1") {
        return check_stub();
    }
    let sdk = env::var("DLSS_SDK").map_err(|_| "DLSS_SDK is not set".to_owned())?;
    let sdk = PathBuf::from(sdk);
    if !sdk.is_absolute() {
        return Err("DLSS_SDK must be an absolute path".into());
    }
    let platform = if target_os == "windows" {
        format!("Windows_{target_arch}")
    } else {
        format!("Linux_{target_arch}")
    };
    let mut lib_dir = sdk.join("lib").join(&platform);
    let rel_dir = lib_dir.join("rel");
    if target_os == "windows" && target_arch == "x86_64" {
        lib_dir.push("x64");
    }
    if !lib_dir.is_dir() {
        return Err(format!("{} has no NGX libraries for {platform}", sdk.display()));
    }
    let include = sdk.join("include");
    let vulkan_include = match env::var("VULKAN_SDK") {
        Ok(vulkan) => PathBuf::from(vulkan).join(if target_os == "windows" {
            "Include"
        } else {
            "include"
        }),
        Err(_) => PathBuf::from("/usr/include"),
    };
    if !vulkan_include.join("vulkan").join("vulkan.h").is_file() {
        return Err(format!(
            "no vulkan/vulkan.h under {} (set VULKAN_SDK)",
            vulkan_include.display()
        ));
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| e.to_string())?);
    // The NGX headers the binding needs, written at build time (the repository holds no C sources;
    // the generated wrapper C file includes this header by path).
    let header = out_dir.join("regia_ngx.h");
    std::fs::write(
        &header,
        "#include <vulkan/vulkan.h>\n#include <nvsdk_ngx_helpers.h>\n#include <nvsdk_ngx_helpers_vk.h>\n",
    )
    .map_err(|e| format!("writing {}: {e}", header.display()))?;
    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .wrap_static_fns(true)
        .wrap_static_fns_path(out_dir.join("ngx_static_fns"))
        .clang_arg(format!("-I{}", include.display()))
        .clang_arg(format!("-I{}", vulkan_include.display()))
        .allowlist_item(".*NGX.*")
        .blocklist_item("Vk.*")
        .blocklist_item("PFN_vk.*")
        .blocklist_item(".*Cuda.*")
        .blocklist_item(".*CUDA.*")
        .blocklist_item(".*D3[Dd].*")
        .generate()
        .map_err(|e| format!("bindgen: {e}"))?;
    bindings
        .write_to_file(out_dir.join("ngx_bindings.rs"))
        .map_err(|e| format!("bindings: {e}"))?;
    cc::Build::new()
        .file(out_dir.join("ngx_static_fns.c"))
        .include(&include)
        .include(&vulkan_include)
        .warnings(false)
        .try_compile("regia_ngx_static_fns")
        .map_err(|e| format!("cc: {e}"))?;
    println!("cargo::rustc-link-search=native={}", lib_dir.display());
    if target_os == "windows" {
        println!("cargo::rustc-link-lib=static=nvsdk_ngx_d");
    } else {
        println!("cargo::rustc-link-lib=static=nvsdk_ngx");
        println!("cargo::rustc-link-lib=dylib=stdc++");
        println!("cargo::rustc-link-lib=dylib=dl");
    }
    println!("cargo::rustc-cfg=regia_dlss_linked");
    Ok(sdk_version(&rel_dir).unwrap_or_else(|| "unknown".to_owned()))
}

/// The check stub (ADR-2322): the hand-written declarations of the NGX items `src/ngx.rs` uses, in
/// place of bindgen's output. Links nothing, so only `cargo check` / `cargo clippy` succeed with it.
#[cfg(feature = "sdk")]
fn check_stub() -> Result<String, String> {
    let manifest = env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?;
    let stub = Path::new(&manifest).join("check_stub").join("ngx_bindings.rs");
    println!("cargo::rerun-if-changed={}", stub.display());
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| e.to_string())?);
    std::fs::copy(&stub, out_dir.join("ngx_bindings.rs"))
        .map_err(|e| format!("copying {}: {e}", stub.display()))?;
    println!(
        "cargo::warning=regia_dlss: REGIA_DLSS_CHECK_STUB=1: the NGX check stub (check and clippy only; a linked build fails)"
    );
    println!("cargo::rustc-cfg=regia_dlss_linked");
    Ok("check-stub".to_owned())
}
