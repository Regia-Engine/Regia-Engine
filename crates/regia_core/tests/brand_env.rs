// SPDX-License-Identifier: MPL-2.0
//! `brand::env_var` against a real process environment (ADR-1388): a child test process is started
//! with only the legacy spelling of a variable set and reads it through the current name. Before
//! the rename the two prefixes are the same and these tests pass trivially; after it they prove
//! the fallback.

use std::process::Command;

use regia_core::brand::{ENV_PREFIX, LEGACY_ENV_PREFIX, env_var, env_var_os};

/// Set in the child process, so the test body knows which side it runs on.
const CHILD: &str = "BRAND_ENV_TEST_CHILD";

fn name(prefix: &str, suffix: &str) -> String {
    format!("{prefix}BRAND_TEST_{suffix}")
}

/// Runs the test `test` again in a child process with `vars` set (after removing the current and
/// legacy spellings of `suffix`).
fn run_child(test: &str, suffix: &str, vars: &[(String, &str)]) {
    let exe = std::env::current_exe().expect("test binary");
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", test, "--test-threads=1"])
        .env_remove(name(ENV_PREFIX, suffix))
        .env_remove(name(LEGACY_ENV_PREFIX, suffix))
        .env(CHILD, "1");
    for (key, value) in vars {
        cmd.env(key, value);
    }
    let status = cmd.status().expect("spawn the child test");
    assert!(status.success(), "child test {test} failed");
}

#[test]
fn a_legacy_variable_is_read_through_the_current_name() {
    if regia_core::brand::env_var_os(CHILD).is_some() {
        let current = name(ENV_PREFIX, "LEGACY_ONLY");
        assert_eq!(env_var(&current).as_deref(), Ok("42"));
        assert_eq!(env_var_os(&current), Some("42".into()));
        return;
    }
    run_child(
        "a_legacy_variable_is_read_through_the_current_name",
        "LEGACY_ONLY",
        &[(name(LEGACY_ENV_PREFIX, "LEGACY_ONLY"), "42")],
    );
}

#[test]
fn the_current_name_wins_over_the_legacy_one() {
    if regia_core::brand::env_var_os(CHILD).is_some() {
        let current = name(ENV_PREFIX, "BOTH");
        assert_eq!(env_var(&current).as_deref(), Ok("7"));
        assert_eq!(env_var_os(&current), Some("7".into()));
        return;
    }
    // Before the rename both spellings are one variable; the later value (the current one) wins.
    run_child(
        "the_current_name_wins_over_the_legacy_one",
        "BOTH",
        &[
            (name(LEGACY_ENV_PREFIX, "BOTH"), "42"),
            (name(ENV_PREFIX, "BOTH"), "7"),
        ],
    );
}
