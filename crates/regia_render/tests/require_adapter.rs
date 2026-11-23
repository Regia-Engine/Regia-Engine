// SPDX-License-Identifier: MPL-2.0
//! `REGIA_REQUIRE_ADAPTER` (ADR-7613): a GPU test that skips when no device opens
//! (`let Ok(device) = RenderDevice::headless() else { return }`) counts as passed. On a card run that
//! lost its card, every later test then passed unseen (one the build server binary after about 18 device
//! creations). With the flag set, the device creation panics, so the skip fails the test.
//!
//! The variables are process-wide, so the parent re-runs this binary's ignored child with a strict
//! selection that names no adapter (the card-loss case: `GPU selection failed`), once without and
//! once with the flag, and reads the child's exit status.

use std::process::{Command, Output};

/// Runs the ignored child with a strict selection of a GPU that does not exist, plus `extra`.
fn child(extra: &[(&str, &str)]) -> Output {
    let exe = std::env::current_exe().expect("test binary");
    let mut command = Command::new(exe);
    command.args([
        "--ignored",
        "--exact",
        "child_skips_without_a_device",
        "--test-threads=1",
    ]);
    for suffix in ["INDEX", "NAME", "PCI", "UUID", "STRICT"] {
        command
            .env_remove(format!("REGIA_GPU_{suffix}"))
            .env_remove(format!("FERRITE_GPU_{suffix}"));
    }
    command
        .env_remove("REGIA_REQUIRE_ADAPTER")
        .env_remove("FERRITE_REQUIRE_ADAPTER")
        .env("REGIA_GPU_NAME", "no such gpu anywhere")
        .env("REGIA_GPU_STRICT", "1");
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().expect("run child")
}

fn text(out: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Without the flag the child's skip passes (the old, silent behavior); with it the same child
/// fails and names the flag.
#[test]
fn a_skipped_device_fails_the_test_when_the_run_requires_an_adapter() {
    let quiet = child(&[]);
    assert!(
        quiet.status.success(),
        "without REGIA_REQUIRE_ADAPTER the skip passes:\n{}",
        text(&quiet)
    );
    for value in ["0", ""] {
        let off = child(&[("REGIA_REQUIRE_ADAPTER", value)]);
        assert!(
            off.status.success(),
            "REGIA_REQUIRE_ADAPTER={value:?} leaves the guard off:\n{}",
            text(&off)
        );
    }
    let loud = child(&[("REGIA_REQUIRE_ADAPTER", "1")]);
    let log = text(&loud);
    assert!(
        !loud.status.success(),
        "with REGIA_REQUIRE_ADAPTER=1 a skipped device must fail the test:\n{log}"
    );
    assert!(
        log.contains("REGIA_REQUIRE_ADAPTER is set") && log.contains("no such gpu"),
        "the failure names the flag and the refused selection:\n{log}"
    );
}

/// The skip pattern GPU tests use; run only by the parent above with the variables set.
#[test]
#[ignore = "run by a_skipped_device_fails_the_test_when_the_run_requires_an_adapter"]
fn child_skips_without_a_device() {
    if regia_core::brand::env_var_os("REGIA_GPU_STRICT").is_none() {
        return;
    }
    let Ok(device) = regia_render::RenderDevice::headless() else {
        eprintln!("skipping: no GPU device");
        return;
    };
    panic!(
        "a strict selection of a GPU that does not exist came up on {:?}",
        device.adapter.get_info().name
    );
}
