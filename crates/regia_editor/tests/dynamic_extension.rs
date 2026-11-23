// SPDX-License-Identifier: MPL-2.0
//! Spec 57 § 8 tests 11 and 12, end to end: a real dynamic extension behind the ABI stamp.
//!
//! The test builds `regia-ext-host` with `--features hot-dylib` and `-C prefer-dynamic`, asks it for
//! the ABI hash and engine fingerprint, builds `samples/ext_fixture` as a dylib twice with different
//! markers, then lets the host refuse flipped and truncated copies and reload the two builds.
//! Nesting cargo inside `cargo test` needs its own target directory (see `tests/hot_reload.rs`).
//!
//! Linux and macOS (ADR-2136: the macOS branch only differs in the library suffix and the loader's
//! search-path variable; the lead verifies it on the Mac). Windows needs a port of the host's
//! search path first.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use regia_framework::extension::stamp::{BUILD_TARGET, STAMP_FIELDS};

fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop();
    dir.pop();
    dir
}

/// The dynamic loader's search-path variable.
const LIBRARY_PATH_VAR: &str = if cfg!(target_os = "macos") {
    "DYLD_LIBRARY_PATH"
} else {
    "LD_LIBRARY_PATH"
};

/// `libext_fixture.so` or `libext_fixture.dylib`.
fn fixture_file() -> String {
    format!("libext_fixture{}", std::env::consts::DLL_SUFFIX)
}

fn toolchain_lib_dir() -> PathBuf {
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc sysroot");
    let sysroot = String::from_utf8_lossy(&sysroot.stdout).trim().to_string();
    PathBuf::from(sysroot)
        .join("lib/rustlib")
        .join(BUILD_TARGET)
        .join("lib")
}

fn cargo(workspace: &Path, target: &Path) -> Command {
    let mut command = Command::new("cargo");
    command
        .current_dir(workspace)
        .env("RUSTFLAGS", "-C prefer-dynamic")
        .env("CARGO_TARGET_DIR", target);
    command
}

fn build_host(workspace: &Path, target: &Path) {
    let status = cargo(workspace, target)
        .args(["build", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .args([
            "-p",
            "regia_editor",
            "--bin",
            "regia-ext-host",
            "--features",
            "hot-dylib",
        ])
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "regia-ext-host failed to build: {status}");
}

fn host(target: &Path) -> Command {
    let mut command = Command::new(target.join("debug/regia-ext-host"));
    let profile = target.join("debug");
    let path = format!(
        "{}:{}:{}",
        toolchain_lib_dir().display(),
        profile.display(),
        profile.join("deps").display()
    );
    command.env(LIBRARY_PATH_VAR, path).env("RUST_BACKTRACE", "1");
    command
}

/// Builds the fixture with `marker` and copies the library to `out`.
fn build_fixture(
    workspace: &Path,
    target: &Path,
    abi_hash: &str,
    fingerprint: &str,
    marker: &str,
    out: &Path,
) {
    let status = cargo(workspace, target)
        .args(["rustc", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .args([
            "-p",
            "ext_fixture",
            "--lib",
            "--crate-type",
            "dylib",
            "--features",
            "dynamic-extension",
        ])
        .env("REGIA_ABI_HASH", abi_hash)
        .env("REGIA_ENGINE_FINGERPRINT", fingerprint)
        .env("EXT_FIXTURE_MARKER", marker)
        .args(["--", "-C", "prefer-dynamic"])
        .status()
        .expect("spawn cargo rustc");
    assert!(status.success(), "ext_fixture failed to build: {status}");
    std::fs::copy(target.join("debug").join(fixture_file()), out).expect("copy the library");
}

fn observations(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("obs "))
        .filter_map(|line| line.split_once(' '))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

#[test]
fn stamp_refusal_load_and_reload() {
    let workspace = workspace_root();
    let target = workspace.join("target/hot-reload-m3");
    build_host(&workspace, &target);

    let env = host(&target)
        .args(["env", "--target-dir"])
        .arg(&target)
        .output()
        .expect("spawn regia-ext-host env");
    assert!(env.status.success(), "{}", String::from_utf8_lossy(&env.stderr));
    let env = String::from_utf8_lossy(&env.stdout).trim().to_owned();
    let (abi_hash, fingerprint) = env.split_once(' ').expect("two numbers");

    let project = regia_test_utils::unique_temp_dir("dynamic-extension");
    let dir = project.join("extensions/ext_fixture");
    let bin = dir.join("bin").join(BUILD_TARGET);
    std::fs::create_dir_all(&bin).expect("bin dir");
    std::fs::copy(
        workspace.join("samples/ext_fixture/regia-plugin.toml"),
        dir.join("regia-plugin.toml"),
    )
    .expect("manifest");
    std::fs::write(
        project.join("Regia.toml"),
        "[extensions]\nenabled = [\"ext_fixture\"]\n\n[extensions.grants]\next_fixture = [\"agent_method\"]\n",
    )
    .expect("Regia.toml");
    let first = project.join(format!("first{}", std::env::consts::DLL_SUFFIX));
    let second = project.join(format!("second{}", std::env::consts::DLL_SUFFIX));
    build_fixture(&workspace, &target, abi_hash, fingerprint, "one", &first);
    build_fixture(&workspace, &target, abi_hash, fingerprint, "two", &second);
    std::fs::copy(&first, bin.join(fixture_file())).expect("install");

    let output = host(&target)
        .arg("run")
        .arg("--project")
        .arg(&project)
        .arg("--target-dir")
        .arg(&target)
        .arg("--first")
        .arg(&first)
        .arg("--second")
        .arg(&second)
        .output()
        .expect("spawn regia-ext-host run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "host failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let obs = observations(&stdout);
    let value = |key: &str| obs.get(key).map_or("<missing>", String::as_str);

    // The record is read from the exported symbol, so other copies of the bytes (a Rust dylib may
    // hold one in its crate metadata) are decoys: damaging them changes nothing (ADR-2844).
    assert!(
        value("magic_records").parse::<u32>().is_ok_and(|n| n >= 1),
        "{stdout}"
    );
    assert_eq!(value("decoys_ignored"), "true", "{stdout}");

    // Test 11: every field names itself, is refused from the file before any copy, and no entry ran.
    assert_eq!(value("file_check"), "true", "{stdout}\n{stderr}");
    for (field, _, _) in STAMP_FIELDS.iter().skip(1) {
        assert_eq!(
            value(&format!("flip_{field}")),
            format!("abi_mismatch:{field}"),
            "{stdout}"
        );
        assert_eq!(
            value(&format!("flip_{field}_copied")),
            "false",
            "`{field}` is refused before any copy: {stdout}"
        );
    }
    // Every magic gone, or a truncated file: no record, refused before any copy.
    assert_eq!(value("no_magic"), "not_an_extension", "{stdout}");
    assert_eq!(value("no_magic_copied"), "false", "{stdout}");
    assert_eq!(value("truncated"), "not_an_extension", "{stdout}");
    assert_eq!(value("truncated_copied"), "false", "{stdout}");
    // The exported record's magic damaged: refused from the file, before any copy.
    assert_eq!(value("flip_magic"), "not_an_extension", "{stdout}");
    assert_eq!(value("flip_magic_copied"), "false", "{stdout}");
    assert_eq!(value("entry_calls_after_refusals"), "0", "{stdout}");

    // A valid copy loads.
    assert_eq!(value("start_state"), "active", "{stdout}\n{stderr}");
    assert_eq!(value("start_kind"), "\"dynamic\"", "{stdout}");
    assert_eq!(value("start_codes"), "", "{stdout}");
    assert_eq!(value("start_marker"), "one", "{stdout}");
    assert_eq!(value("start_ticks"), "one=1 two=0", "{stdout}");

    // Test 12: generations count, registrations swap, state crosses.
    assert_eq!(value("reload1_generation"), "1", "{stdout}\n{stderr}");
    assert_eq!(value("reload1_removed"), "1");
    assert_eq!(value("reload1_added"), "1");
    assert_eq!(value("reload1_marker"), "two");
    assert_eq!(value("reload1_count"), "1");
    assert_eq!(
        value("reload1_ticks"),
        "one=0 two=1",
        "the old copy's system left and the new one's joined: {stdout}"
    );
    assert_eq!(value("reload2_generation"), "2");
    assert_eq!(value("reload2_count"), "2");
    assert_eq!(value("reload2_ticks"), "one=0 two=1", "{stdout}");

    // A failing build restores the old registrations.
    assert_eq!(value("failed_error"), "reload_failed", "{stdout}");
    assert_eq!(value("failed_marker"), "two");
    assert_eq!(value("failed_ticks"), "one=0 two=1", "{stdout}");
    assert_eq!(value("recovered_generation"), "3");

    // A refused library changes nothing.
    assert_eq!(value("refused_error"), "abi_mismatch", "{stdout}");
    assert_eq!(value("refused_marker"), "two");
    assert_eq!(value("refused_ticks"), "one=0 two=1", "{stdout}");
    assert_eq!(value("refused_count"), value("recovered_count"));
    assert_eq!(value("schema_has_reload"), "true");
    let _ = std::fs::remove_dir_all(&project);
}
