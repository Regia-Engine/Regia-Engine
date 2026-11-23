// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask stream-bench`: the teleport bench of `docs/specs/78-world-streaming.md` § 18
//! (acceptance B): a 2 km teleport in async timing on the selected card, reporting the time to fill
//! the view and the frames over 33 ms. The measurement is the ignored test `teleport_bench` of
//! `samples/stream_world`, in a release build, one at a time on an exclusive card.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result};

/// Arguments of `cargo xtask stream-bench`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The view size.
    #[arg(long, default_value = "2560x1440")]
    pub size: String,
    /// Also write the JSON line to this file.
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// Turn on the VRAM planner bound and the render residency wait (ADR-2317, off by default).
    #[arg(long)]
    pub bounds: bool,
    /// Run the long soak instead of the teleport bench: `regia_world`'s `tests/soak.rs` in release,
    /// headless, no card (spec 78 § 18). Counters only.
    #[arg(long)]
    pub soak: bool,
    /// The soak's seed (default `0x5eed0001`); the run's JSON line names it.
    #[arg(long, requires = "soak")]
    pub seed: Option<u64>,
}

/// The `cargo` arguments and the environment of the run.
pub fn command(args: &Args) -> (Vec<String>, Vec<(String, String)>) {
    if args.soak {
        let cargo = [
            "test",
            "--release",
            "-p",
            "regia_world",
            "--test",
            "soak",
            "a_long_seeded_soak",
            "--",
            "--nocapture",
            "--test-threads=1",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let mut env = Vec::new();
        if let Some(seed) = args.seed {
            env.push(("STREAM_SOAK_SEED".to_string(), seed.to_string()));
        }
        if let Some(json) = &args.json {
            env.push(("STREAM_SOAK_JSON".into(), json.display().to_string()));
        }
        return (cargo, env);
    }
    let cargo = [
        "test",
        "--release",
        "-p",
        "stream_world",
        "--test",
        "acceptance",
        "teleport_bench",
        "--",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    let mut env = vec![("STREAM_BENCH_SIZE".to_string(), args.size.clone())];
    if let Some(json) = &args.json {
        env.push(("STREAM_BENCH_JSON".into(), json.display().to_string()));
    }
    if args.bounds {
        env.push(("STREAM_BENCH_BOUNDS".into(), "1".into()));
    }
    (cargo, env)
}

/// Runs the bench.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let cargo_bin = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let (cargo_args, env) = command(args);
    let status = Command::new(cargo_bin)
        .current_dir(root)
        .args(&cargo_args)
        .envs(env)
        .status()
        .context("running the stream_world teleport bench")?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_runs_the_ignored_test_in_release_with_the_size() {
        let args = Args {
            size: "1280x720".into(),
            json: Some(PathBuf::from("out.json")),
            bounds: true,
            soak: false,
            seed: None,
        };
        let (cargo, env) = command(&args);
        assert!(cargo.contains(&"--release".to_string()));
        assert!(cargo.contains(&"--ignored".to_string()));
        assert!(env.contains(&("STREAM_BENCH_SIZE".to_string(), "1280x720".to_string())));
        assert!(
            env.iter()
                .any(|(k, v)| k == "STREAM_BENCH_JSON" && v == "out.json")
        );
        assert!(env.contains(&("STREAM_BENCH_BOUNDS".to_string(), "1".to_string())));
        let plain = command(&Args {
            bounds: false,
            ..args
        });
        assert!(!plain.1.iter().any(|(k, _)| k == "STREAM_BENCH_BOUNDS"));
    }

    #[test]
    fn the_soak_flag_runs_the_headless_soak_test_with_its_seed() {
        let args = Args {
            size: "2560x1440".into(),
            json: Some(PathBuf::from("soak.json")),
            bounds: false,
            soak: true,
            seed: Some(7),
        };
        let (cargo, env) = command(&args);
        assert!(cargo.contains(&"regia_world".to_string()));
        assert!(cargo.contains(&"soak".to_string()));
        assert!(!cargo.contains(&"--ignored".to_string()));
        assert!(env.contains(&("STREAM_SOAK_SEED".to_string(), "7".to_string())));
        assert!(env.contains(&("STREAM_SOAK_JSON".to_string(), "soak.json".to_string())));
        assert!(!env.iter().any(|(k, _)| k == "STREAM_BENCH_SIZE"));
    }
}
