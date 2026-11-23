// SPDX-License-Identifier: MPL-2.0
//! Generates XeSS declarations into OUT_DIR; no SDK content is stored in the repository.
use std::env;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(regia_xess_linked)");
    for name in ["XESS_SDK", "VULKAN_SDK"] {
        println!("cargo::rerun-if-env-changed={name}");
    }
    if let Err(error) = generate()
        && env::var_os("CARGO_FEATURE_SDK").is_some()
    {
        println!("cargo::warning=regia_xess stub: {error}");
    }
}

#[cfg(not(feature = "sdk"))]
fn generate() -> Result<(), String> {
    Ok(())
}

#[cfg(feature = "sdk")]
fn generate() -> Result<(), String> {
    use std::path::PathBuf;
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Err("XeSS SDK 3.0.2 provides a Windows runtime only".into());
    }
    let Some(sdk) = env::var_os("XESS_SDK").map(PathBuf::from) else {
        return Err("XESS_SDK is not set".into());
    };
    if !sdk.is_absolute() {
        return Err("XESS_SDK must be absolute".into());
    }
    let header = sdk.join("inc/xess/xess_vk.h");
    if !header.is_file() {
        return Err("XESS_SDK has no inc/xess/xess_vk.h".into());
    }
    println!("cargo::rerun-if-changed={}", header.display());
    println!(
        "cargo::rerun-if-changed={}",
        sdk.join("inc/xess/xess.h").display()
    );
    let mut builder = bindgen::Builder::default()
        .header(header.to_string_lossy())
        // Intel's public C ABI header uses C++ bool in several declarations.
        .clang_args(["-x", "c++", "-std=c++17"])
        .allowlist_function("xess(GetVersion|GetProperties|GetOptimalInputResolution|DestroyContext|SetVelocityScale|VKGetRequiredInstanceExtensions|VKGetRequiredDeviceExtensions|VKGetRequiredDeviceFeatures|VKCreateContext|VKInit|VKExecute)")
        .allowlist_type("xess_.*")
        .allowlist_var("XESS_.*")
        .blocklist_item("Vk.*")
        .blocklist_item("PFN_vk.*")
        .prepend_enum_name(false)
        .generate_comments(false)
        .derive_default(true)
        .layout_tests(true)
        .dynamic_library_name("Api")
        .dynamic_link_require_all(true);
    if let Some(vulkan) = env::var_os("VULKAN_SDK") {
        let root = PathBuf::from(vulkan);
        let include = if root.join("Include").is_dir() {
            root.join("Include")
        } else {
            root.join("include")
        };
        builder = builder.clang_arg(format!("-I{}", include.display()));
    }
    // An explicitly configured, present SDK must not silently turn a broken binding into a stub.
    let bindings = builder
        .generate()
        .unwrap_or_else(|error| panic!("XeSS bindgen failed: {error}"));
    let output = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| panic!("XeSS bindings require Cargo's absolute OUT_DIR"));
    bindings
        .write_to_file(output.join("xess.rs"))
        .unwrap_or_else(|error| panic!("XeSS bindings write failed: {error}"));
    println!("cargo::rustc-cfg=regia_xess_linked");
    Ok(())
}
