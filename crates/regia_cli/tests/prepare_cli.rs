// SPDX-License-Identifier: MPL-2.0
//! Integration tests for `regia prepare` argument parsing and early exits.

use std::process::{Command, Output};

use regia_test_utils::unique_temp_dir;

fn run_prepare(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("prepare")
        .args(args)
        .env_remove("REGIA_PIPELINE_CACHE_DIR")
        .output()
        .expect("regia prepare starts")
}

#[test]
fn prepare_rejects_invalid_profile_thread_and_timeout_values_before_device_setup() {
    for profile in ["", "not-a-profile"] {
        let output = run_prepare(&["--profile", profile]);
        assert!(
            !output.status.success(),
            "profile {profile:?} must be rejected by argument parsing"
        );
        assert!(
            output.stdout.is_empty(),
            "invalid profile must not warm pipelines"
        );
    }
    for threads in ["", "many", "0", "18446744073709551616"] {
        let output = run_prepare(&["--threads", threads]);
        assert!(
            !output.status.success(),
            "threads {threads:?} must be rejected by argument parsing"
        );
        assert!(
            output.stdout.is_empty(),
            "invalid worker count must not warm pipelines"
        );
    }
    for timeout in ["", "later", "0", "18446744073709551616"] {
        let output = run_prepare(&["--timeout-secs", timeout]);
        assert!(
            !output.status.success(),
            "timeout {timeout:?} must be rejected by argument parsing"
        );
        assert!(
            output.stdout.is_empty(),
            "invalid timeout must not warm pipelines"
        );
    }
}

#[test]
fn prepare_accepts_documented_profiles_and_positive_numeric_values_without_opening_a_device() {
    let cache = unique_temp_dir("prepare-check-cache");
    for profile in [
        "MobileLow",
        "MobileHigh",
        "DesktopLow",
        "DesktopHigh",
        "DesktopUltra",
        "Deck",
    ] {
        let status = run_prepare(&[
            "--cache-dir",
            cache.to_str().expect("UTF-8 temp path"),
            "--profile",
            profile,
            "--threads",
            "1",
            "--timeout-secs",
            "1",
            "--status",
        ]);
        assert_eq!(
            status.status.code(),
            Some(0),
            "profile {profile} parsed in device-free status mode"
        );
    }
}

#[test]
fn prepare_missing_cache_argument_fails_before_warmup() {
    let output = run_prepare(&["--profile", "DesktopHigh"]);
    assert!(!output.status.success());
    let event: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("missing cache argument produces a failed JSON event");
    assert_eq!(event["state"], "failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--cache-dir"),
        "missing cache argument is clear: {stderr}"
    );
}
