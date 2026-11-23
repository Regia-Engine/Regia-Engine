// SPDX-License-Identifier: MPL-2.0
//! The CLI steps of the book's "Package a build" tutorial as a doc test (ADR-9810, AGENTS.md
//! rule 8): the test reads `book/tutorials/package-a-build.md` at run time, takes the `regia ...`
//! and `cd ...` lines of its `sh` fences (`regia_test_utils::tutorial`) and runs them, in page
//! order, through the real `regia` binary in a scratch folder. The page's `regia agent call`
//! steps run against a headless editor in `regia_editor`'s `book_tutorials` test.
//!
//! Each command is bounded by `HANG_BOUND` (a bound on a hang, not on speed).

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use regia_test_utils::process::output_within;
use regia_test_utils::tutorial::{TutorialStep, read_page, tutorial_steps};
use regia_test_utils::unique_temp_dir;

/// The longest wait for one command that stopped making progress.
const HANG_BOUND: Duration = Duration::from_secs(1800);

#[test]
fn the_package_tutorial_cli_steps_run() {
    let page = "package-a-build.md";
    let text = read_page(page).unwrap_or_else(|e| panic!("{e}"));
    let steps = tutorial_steps(&text).unwrap_or_else(|e| panic!("{page}: {e}"));
    let ws = unique_temp_dir("book-tutorial-package-cli");
    let mut cwd: PathBuf = ws.clone();
    let mut ran = Vec::new();
    for step in &steps {
        match step {
            TutorialStep::Cd { line, dir } => {
                cwd = cwd.join(dir);
                assert!(cwd.is_dir(), "{page}:{line}: cd {dir}: no such folder");
            }
            TutorialStep::Cli { line, args } => {
                let out = output_within(
                    Command::new(env!("CARGO_BIN_EXE_regia"))
                        .args(args)
                        .current_dir(&cwd)
                        .env("HOME", ws.join("home"))
                        .env("XDG_CONFIG_HOME", ws.join("home/config"))
                        .env("XDG_DATA_HOME", ws.join("home/data"))
                        .env_remove("STEAM_USERNAME")
                        .env_remove("STEAM_PASSWORD")
                        .env_remove("STEAM_TOTP"),
                    HANG_BOUND,
                )
                .expect("run regia");
                assert!(
                    out.status.success(),
                    "{page}:{line}: regia {} failed ({}):\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    out.status,
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                ran.push((args.clone(), String::from_utf8_lossy(&out.stdout).into_owned()));
            }
            // The agent steps run in regia_editor's book_tutorials test.
            TutorialStep::Agent(_) => {}
        }
    }
    let firsts: Vec<&str> = ran
        .iter()
        .filter_map(|(a, _)| a.first().map(String::as_str))
        .collect();
    assert_eq!(firsts, ["new", "package"], "the page's CLI steps: {firsts:?}");
    let (args, dry_run) = &ran[1];
    assert!(
        args.iter().any(|a| a == "--dry-run"),
        "the checked package step is a dry run: {args:?}"
    );
    assert!(!dry_run.trim().is_empty(), "the dry run prints its size report");
    assert!(
        !cwd.join("dist").exists(),
        "a dry run packages nothing: {}",
        cwd.join("dist").display()
    );
    let _ = std::fs::remove_dir_all(&ws);
}
