// SPDX-License-Identifier: MPL-2.0
//! The committed CI registries pass their own checks through the real binary (`cargo xtask flaky
//! --check`, `cargo xtask nightly --check`), so a renamed test, target or xtask sub-command that a
//! registry names fails the gate (ADR-1035).

use std::process::{Command, Output, Stdio};

fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run xtask")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn the_flaky_registry_names_only_real_tests() {
    let out = xtask(&["flaky", "--check"]);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn the_nightly_registry_names_only_real_commands_and_tests() {
    let out = xtask(&["nightly", "--check"]);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn the_gate_args_come_one_per_line() {
    let out = xtask(&["flaky", "--gate-args", "lvp"]);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    // Pairs of `--solo` and a runner key, ready for `mapfile -t` in the gate script.
    let flags: Vec<&str> = lines.iter().step_by(2).copied().collect();
    let keys: Vec<&str> = lines.iter().skip(1).step_by(2).copied().collect();
    assert!(!flags.is_empty() && flags.len() == keys.len(), "{stdout}");
    assert!(flags.iter().all(|f| *f == "--solo"), "{stdout}");
    assert!(keys.iter().all(|k| k.split(':').count() == 3), "{stdout}");
}
