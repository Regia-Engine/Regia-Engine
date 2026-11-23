// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask repro-check`: the record/replay determinism stress run (ADR-537,
//! `book/agents/determinism.md`). Every scene is recorded under N seeds and replayed from the
//! recording; any checkpoint whose state hash differs is reported with the components that
//! changed.
//!
//! The harness and the engine scenes live in `regia_test_utils::repro`, the bot match in
//! `samples/arena/tests/repro_check.rs`; xtask may not depend on either outside
//! dev-dependencies (`check-layers`), so this runs their integration tests.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

/// Arguments of `cargo xtask repro-check`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Seeds per scene.
    #[arg(long, default_value_t = 20)]
    pub seeds: u64,
    /// Frames (one fixed step each) per run.
    #[arg(long, default_value_t = 240)]
    pub steps: u32,
    /// Also write the markdown reports next to this path (`<path>.engine.md`, `<path>.arena.md`).
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// The two `cargo test` invocations (package, test file) this command runs.
pub const TESTS: [(&str, &str); 2] = [("regia_test_utils", "repro_check"), ("arena", "repro_check")];

/// The `cargo test` arguments of one test file and the environment every run gets.
pub fn test_command(args: &Args, package: &str, test: &str) -> (Vec<String>, Vec<(String, String)>) {
    let mut cargo: Vec<String> = ["test", "-p", package, "--test", test]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    cargo.extend(["--".into(), "--nocapture".into(), "--test-threads=1".into()]);
    let mut env = vec![
        ("REGIA_REPRO_SEEDS".to_string(), args.seeds.to_string()),
        ("REGIA_REPRO_STEPS".to_string(), args.steps.to_string()),
    ];
    if let Some(report) = &args.report {
        env.push(("REGIA_REPRO_REPORT".into(), report.display().to_string()));
    }
    (cargo, env)
}

/// Runs both test files (the second even when the first fails); the exit code fails when either
/// does.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let cargo_bin = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut failed = Vec::new();
    for (package, test) in TESTS {
        let (cargo_args, env) = test_command(args, package, test);
        let status = Command::new(&cargo_bin)
            .current_dir(root)
            .args(&cargo_args)
            .envs(env)
            .status()
            .with_context(|| format!("running cargo test -p {package} --test {test}"))?;
        if !status.success() {
            failed.push(package);
        }
    }
    Ok(if failed.is_empty() {
        println!("repro-check: ok");
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "repro-check: FAILED in {} (the report above lists each diverging scene, seed, step and component)",
            failed.join(", ")
        );
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_sizes_the_run_through_the_environment() {
        let args = Args {
            seeds: 5,
            steps: 60,
            report: Some("/tmp/repro".into()),
        };
        let (cargo, env) = test_command(&args, "arena", "repro_check");
        assert_eq!(cargo[..5], ["test", "-p", "arena", "--test", "repro_check"]);
        assert!(env.contains(&("REGIA_REPRO_SEEDS".into(), "5".into())));
        assert!(env.contains(&("REGIA_REPRO_STEPS".into(), "60".into())));
        assert!(env.contains(&("REGIA_REPRO_REPORT".into(), "/tmp/repro".into())));
        assert_eq!(TESTS.len(), 2, "the engine scenes and the bot match");
    }
}
