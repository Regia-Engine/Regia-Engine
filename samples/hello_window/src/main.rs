// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `hello_window`.
//!
//! Flags for automated runs: `--exit-after-frames N`, `--simulate-suspend-every N` (exercise surface
//! teardown and recreation the way Android suspend does).

use std::process::ExitCode;

use hello_window::{ExitAfterFrames, HelloWindow};
use regia::prelude::*;

fn flag(args: &[String], name: &str) -> Result<Option<u64>, String> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => match args.get(i + 1).map(|v| v.parse::<u64>()) {
            Some(Ok(n)) => Ok(Some(n)),
            _ => Err(format!("{name} needs a number")),
        },
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (exit_after, suspend_every) = match (
        flag(&args, "--exit-after-frames"),
        flag(&args, "--simulate-suspend-every"),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("hello_window: {e}");
            return ExitCode::from(2);
        }
    };
    let exit = regia::run_with(HelloWindow, move |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - hello_window".into(),
                ..Default::default()
            },
        });
        app.insert_resource(RunnerSettings {
            simulate_suspend_every: suspend_every,
        });
        // Advertise the agent endpoint in this sample's project folder
        // (`regia agent call --project samples/hello_window ...`).
        app.insert_resource(regia::agent::AgentSettings {
            project_dir: sample_project_dir(),
        });
        if let Some(n) = exit_after {
            app.insert_resource(ExitAfterFrames(n));
        }
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}

/// This sample's project directory, without embedding the build machine's path at compile time
/// (`env!("CARGO_MANIFEST_DIR")` ships that path in every binary; ADR-10354).
///
/// `cargo run` exports `CARGO_MANIFEST_DIR` at runtime. A bare binary (CI, `--exit-after-frames`
/// smoke runs) finds the workspace layout around the executable (`target/<profile>/hello_window`
/// sits two levels under the workspace root). A packaged game finds neither and returns `None`,
/// so the agent endpoint falls back to its own resolution.
fn sample_project_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return Some(dir.into());
    }
    let root = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .parent()?
        .to_path_buf();
    let project = root.join("samples").join("hello_window");
    project.is_dir().then_some(project)
}
