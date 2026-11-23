// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask editor-monkey`: the editor monkey test (ADR-3290, `crates/regia_editor/tests/monkey.rs`)
//! at a chosen size. The test target is `harness = false` and takes its own arguments; this runs
//! it in a release build (debug egui frames are slow), so the nightly soak is one command:
//!
//! ```text
//! cargo xtask editor-monkey --seeds 50 --steps 5000          # nightly (docs/ci/nightly-jobs.toml)
//! cargo xtask editor-monkey --seed 0x5eed0001 --steps 120    # replay one seed
//! cargo xtask editor-monkey --seed 7 --steps 5 --plant panic@2   # prove the driver catches a fault
//! ```
//!
//! Without `--seeds` or `--seed` the test's gate set runs (its fixed seeds at 300 steps, the
//! regression seeds and the planted-fault checks): the same run the merge gate's `cargo test`
//! makes, in release. The exit code is the test's: 0 when every seed passed (with `--plant`, when
//! every seed failed with the planted fault), 1 on a failure, 2 on a bad argument.

use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};
use clap::Args as ClapArgs;

/// Arguments of `cargo xtask editor-monkey`.
#[derive(ClapArgs, Debug, Clone, Default)]
pub struct Args {
    /// Run this many seeds, counted from `--first-seed`.
    #[arg(long, conflicts_with = "seed")]
    pub seeds: Option<u64>,
    /// The first seed of `--seeds` (decimal or `0x` hex).
    #[arg(long, requires = "seeds")]
    pub first_seed: Option<String>,
    /// Run one seed (decimal or `0x` hex).
    #[arg(long)]
    pub seed: Option<String>,
    /// Steps per seed (the test's default is its gate size, 300).
    #[arg(long)]
    pub steps: Option<u32>,
    /// Plant a fault at a step (`panic@STEP` or `nan@STEP`): passes only when every seed fails with it.
    #[arg(long)]
    pub plant: Option<String>,
    /// Worker threads (the test's default is up to 4).
    #[arg(long)]
    pub threads: Option<usize>,
    /// Build in the dev profile instead of release.
    #[arg(long)]
    pub debug: bool,
}

/// The `cargo` arguments and the environment of the run.
#[must_use]
pub fn test_command(args: &Args) -> (Vec<String>, Vec<(String, String)>) {
    let mut cargo: Vec<String> = vec!["test".into(), "-p".into(), "regia_editor".into()];
    if !args.debug {
        cargo.push("--release".into());
    }
    cargo.extend(["--test".into(), "monkey".into(), "--".into()]);
    if let Some(seeds) = args.seeds {
        cargo.extend(["--seeds".into(), seeds.to_string()]);
        if let Some(first) = &args.first_seed {
            cargo.extend(["--first-seed".into(), first.clone()]);
        }
    }
    if let Some(seed) = &args.seed {
        cargo.extend(["--seed".into(), seed.clone()]);
    }
    if let Some(steps) = args.steps {
        cargo.extend(["--steps".into(), steps.to_string()]);
    }
    if let Some(plant) = &args.plant {
        cargo.extend(["--plant".into(), plant.clone()]);
    }
    let env = args
        .threads
        .map(|threads| vec![("REGIA_MONKEY_THREADS".to_string(), threads.to_string())])
        .unwrap_or_default();
    (cargo, env)
}

/// Runs the monkey test; the exit code is the test's.
///
/// # Errors
///
/// `cargo` could not be started.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let cargo_bin = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let (cargo_args, env) = test_command(args);
    println!("editor-monkey: cargo {}", cargo_args.join(" "));
    let status = Command::new(&cargo_bin)
        .current_dir(root)
        .args(&cargo_args)
        .envs(env)
        .status()
        .context("starting cargo test -p regia_editor --test monkey")?;
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        None => ExitCode::FAILURE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nightly_size_is_a_release_run_of_the_monkey_target() {
        let (cargo, env) = test_command(&Args {
            seeds: Some(50),
            steps: Some(5000),
            ..Args::default()
        });
        assert_eq!(
            cargo.join(" "),
            "test -p regia_editor --release --test monkey -- --seeds 50 --steps 5000"
        );
        assert!(env.is_empty());
    }

    #[test]
    fn a_replay_and_a_planted_fault_pass_their_flags_through() {
        let (cargo, env) = test_command(&Args {
            seed: Some("0x7".into()),
            steps: Some(5),
            plant: Some("panic@2".into()),
            threads: Some(1),
            debug: true,
            ..Args::default()
        });
        assert_eq!(
            cargo.join(" "),
            "test -p regia_editor --test monkey -- --seed 0x7 --steps 5 --plant panic@2"
        );
        assert_eq!(env, vec![("REGIA_MONKEY_THREADS".to_string(), "1".to_string())]);
        let (gate, _) = test_command(&Args::default());
        assert_eq!(gate.join(" "), "test -p regia_editor --release --test monkey --");
    }
}
