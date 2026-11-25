// SPDX-License-Identifier: MPL-2.0
//! Release build of the smallest template (`quick`) carries no private host string (ADR-10370).
//!
//! Ignored: it compiles the engine in release. On a build host:
//! `cargo test -p regia_cli --test template_host_strings -- --ignored`

use std::path::PathBuf;
use std::process::Command;

use regia_cli::new_project::{NewProjectRequest, create};
use regia_cli::package_hygiene::check_executable_strings;

#[test]
#[ignore = "release-builds the quick template against this checkout"]
fn quick_template_release_binary_has_no_host_strings() {
    let parent = regia_test_utils::unique_temp_dir("quick-host-strings");
    let created = create(&NewProjectRequest {
        name: "Remap Check".into(),
        template: "quick".into(),
        parent: parent.clone(),
    })
    .expect("regia new");
    let engine = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let seeded = regia_cli::distribution::seed_lockfile(&created.root, &engine).expect("seed lock");
    assert!(
        seeded || created.root.join("Cargo.lock").is_file(),
        "the engine lockfile is seeded"
    );
    // A release engine build does not belong in the temp directory. Prefer the caller's
    // target dir (the lane tree); otherwise the checkout's.
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| engine.join("target"));
    let status = Command::new("cargo")
        .current_dir(&created.root)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_INCREMENTAL", "0")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .args(["build", "--offline", "--release"])
        .status()
        .expect("spawn cargo");
    assert!(status.success(), "cargo build --release of the quick template");
    let mut binary = target.join("release").join(&created.crate_name);
    if cfg!(windows) {
        binary.set_extension("exe");
    }
    check_executable_strings(&binary, None).expect("no private host strings");
    let _ = std::fs::remove_dir_all(&parent);
}
