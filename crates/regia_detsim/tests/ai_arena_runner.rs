// SPDX-License-Identifier: MPL-2.0
//! End-to-end tests of the headless runner's JSON line and exit-code surface.

use std::process::{Command, Output};

fn runner(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ai_arena"))
        .args(args)
        .output()
        .expect("arena executable")
}

fn text(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("UTF-8 report")
}

fn fingerprint(output: &Output) -> &str {
    text(output)
        .split("\"fingerprint\":\"")
        .nth(1)
        .expect("fingerprint field")
        .split('"')
        .next()
        .expect("hex value")
}

#[test]
fn seeded_process_runs_are_identical_and_exercise_combat() {
    let args = ["--seed", "100", "--ticks", "1200"];
    let a = runner(&args);
    let b = runner(&args);
    assert!(a.status.success(), "{a:?}");
    assert!(b.status.success(), "{b:?}");
    assert_eq!(a.stdout, b.stdout);
    assert!(text(&a).starts_with("regia-det-sim {"));
    assert_eq!(text(&a).lines().count(), 1);
    assert!(text(&a).contains("\"ticks\":1200"));
    assert!(!text(&a).contains("\"attacks\":0"));
    assert_eq!(fingerprint(&a).len(), 16);
    assert!(text(&a).contains(&format!(
        "\"target\":\"{}\"",
        regia_detsim::golden::current_target()
    )));
    assert!(fingerprint(&a).bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn soak_and_profile_agree_with_an_untimed_run() {
    let plain = runner(&["--seed", "44", "--ticks", "50"]);
    let soak = runner(&["--seed", "44", "--ticks", "50", "--soak", "3"]);
    let profile = runner(&["--seed", "44", "--profile", "40", "--warmup", "10"]);
    for output in [&plain, &soak, &profile] {
        assert!(output.status.success(), "{output:?}");
        assert_eq!(fingerprint(output), fingerprint(&plain));
    }
    assert!(text(&soak).contains("\"pairs_completed\":3,\"steps_compared\":150,\"divergence\":null"));
    assert!(text(&profile).contains("\"warmup_ticks\":10,\"samples\":40,\"p50_ns\":"));
}

#[test]
fn invalid_input_is_a_machine_error_without_a_panic() {
    let cases: &[(&[&str], &str)] = &[
        (&["--seed"], "invalid_arguments"),
        (&["--unknown", "1"], "invalid_arguments"),
        (&["--seed", "1", "--seed", "2"], "invalid_arguments"),
        (&["--rate-hz", "0"], "invalid_arguments"),
        (&["--rate-hz", "NaN"], "invalid_arguments"),
        (&["--units-per-team", "0"], "invalid_arguments"),
        (&["--ticks", "-1"], "invalid_arguments"),
        (&["--soak", "0"], "empty_soak"),
        (&["--profile", "0"], "empty_profile"),
        (&["--profile", "1", "--soak", "1"], "invalid_arguments"),
        (&["--warmup", "1"], "invalid_arguments"),
        (&["--profile", "1", "--ticks", "1"], "invalid_arguments"),
        (&["--profile", "4294967295"], "tick_overflow"),
        (&["--ticks", "1000001"], "work_limit"),
        (&["--ticks", "1000000", "--units-per-team", "128"], "work_limit"),
        (&["--soak", "1001"], "work_limit"),
    ];
    for (args, code) in cases {
        let output = runner(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert_eq!(
            text(&output),
            format!("regia-det-sim {{\"status\":\"error\",\"code\":\"{code}\"}}\n")
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn help_and_zero_tick_bootstrap_are_available() {
    let help = runner(&["--help"]);
    assert!(help.status.success());
    assert!(text(&help).contains("--soak PAIRS"));
    let zero = runner(&["--ticks", "0", "--soak", "2"]);
    assert!(zero.status.success());
    assert!(text(&zero).contains("\"ticks\":0"));
    assert!(text(&zero).contains("\"steps_compared\":0"));
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_rejected_without_panicking() {
    use std::os::unix::ffi::OsStringExt;

    let output = Command::new(env!("CARGO_BIN_EXE_ai_arena"))
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .expect("arena executable");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        text(&output),
        "regia-det-sim {\"status\":\"error\",\"code\":\"invalid_arguments\"}\n"
    );
    assert!(output.stderr.is_empty());
}
