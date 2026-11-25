// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `flight_lab`: a window on the lab with the agent protocol on.
//!
//! Flags: `--no-script` leaves the plane parked (brakes set) for an agent to fly; without it
//! [`FlightScript`] takes off, climbs and levels off.

use flight_lab::{FlightLabPlugin, FlightScript};
use regia::prelude::*;

/// Project directory of this sample, resolved at runtime (ADR-10370).
///
/// `cargo run` exports `CARGO_MANIFEST_DIR`. A binary under `target/<profile>`
/// walks up to `samples/flight_lab`. A packaged game finds neither and returns
/// `None`. The compile-time `env!("CARGO_MANIFEST_DIR")` is not remapped by rustc.
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
    let project = root.join("samples").join("flight_lab");
    project.is_dir().then_some(project)
}

fn main() -> std::process::ExitCode {
    let scripted = !std::env::args().any(|arg| arg == "--no-script");
    let exit = regia::run_with(FlightLabPlugin, move |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - flight_lab".into(),
                ..Default::default()
            },
        });
        app.insert_resource(regia::agent::AgentSettings {
            project_dir: sample_project_dir(),
        });
        if scripted {
            app.insert_resource(FlightScript);
        }
    });
    match exit {
        AppExit::Success => std::process::ExitCode::SUCCESS,
        AppExit::Error(code) => std::process::ExitCode::from(code.get()),
    }
}
