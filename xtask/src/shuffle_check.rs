// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask shuffle-check`: run the headless smoke scene under N seeded system orders and report
//! any run whose observable state differs, plus the ambiguity report of the engine's schedules
//! (`book/agent-guide/schedule-shuffle.md`).
//!
//! The scene and the comparison live in `regia_test_utils::shuffle`; xtask may not depend on that
//! crate outside dev-dependencies (`check-layers`), so this runs its integration test.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

/// Arguments of `cargo xtask shuffle-check`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// How many seeds to run (seed 1 is the reference, run twice to prove the scene repeatable).
    #[arg(long, default_value_t = 8)]
    pub seeds: u64,
    /// Frames each run steps.
    #[arg(long, default_value_t = 90)]
    pub frames: u32,
    /// Only print the ambiguity report; skip the seed runs.
    #[arg(long)]
    pub ambiguities: bool,
    /// Also write the ambiguity report (markdown) to this file.
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// The `cargo test` arguments and environment this command runs.
pub fn test_command(args: &Args) -> (Vec<String>, Vec<(String, String)>) {
    let mut cargo: Vec<String> = ["test", "-p", "regia_test_utils", "--test", "shuffle_check"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    cargo.extend(["--".into(), "--nocapture".into(), "--test-threads=1".into()]);
    if args.ambiguities {
        cargo.push("the_engine_schedules_report_their_ambiguities".into());
    }
    let mut env = vec![
        ("REGIA_SHUFFLE_SEEDS".to_string(), args.seeds.to_string()),
        ("REGIA_SHUFFLE_FRAMES".to_string(), args.frames.to_string()),
    ];
    if let Some(report) = &args.report {
        // The test runs with its package directory as cwd: a relative path is made absolute
        // against the caller's directory, or the report lands inside `crates/regia_test_utils/`.
        let report = std::path::absolute(report).unwrap_or_else(|_| report.clone());
        env.push(("REGIA_SHUFFLE_REPORT".into(), report.display().to_string()));
    }
    (cargo, env)
}

/// Runs the shuffle test; the exit code is the test's.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let (cargo_args, env) = test_command(args);
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args(&cargo_args)
        .envs(env)
        .status()
        .context("running cargo test for shuffle-check")?;
    Ok(if status.success() {
        println!("shuffle-check: ok");
        ExitCode::SUCCESS
    } else {
        eprintln!("shuffle-check: FAILED (the report above lists the diverging seeds and components)");
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_sizes_the_run_through_the_environment() {
        let args = Args {
            seeds: 20,
            frames: 120,
            ambiguities: true,
            report: Some(std::env::temp_dir().join("r.md")),
        };
        let (cargo, env) = test_command(&args);
        assert_eq!(
            cargo[..5],
            ["test", "-p", "regia_test_utils", "--test", "shuffle_check"]
        );
        assert_eq!(
            cargo.last().map(String::as_str),
            Some("the_engine_schedules_report_their_ambiguities")
        );
        assert!(env.contains(&("REGIA_SHUFFLE_SEEDS".into(), "20".into())));
        assert!(env.contains(&("REGIA_SHUFFLE_FRAMES".into(), "120".into())));
        // An absolute report path is passed through unchanged.
        let report = std::env::temp_dir().join("r.md").display().to_string();
        assert!(env.contains(&("REGIA_SHUFFLE_REPORT".into(), report)));
    }

    #[test]
    fn a_relative_report_path_is_resolved_against_the_callers_directory() {
        let args = Args {
            seeds: 1,
            frames: 1,
            ambiguities: true,
            report: Some("target/amb.md".into()),
        };
        let (_, env) = test_command(&args);
        let expected = std::env::current_dir()
            .expect("cwd")
            .join("target")
            .join("amb.md");
        assert!(
            env.contains(&("REGIA_SHUFFLE_REPORT".into(), expected.display().to_string())),
            "{env:?}"
        );
    }
}
