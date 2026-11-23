// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `flight_lab`: a window on the lab with the agent protocol on.
//!
//! Flags: `--no-script` leaves the plane parked (brakes set) for an agent to fly; without it
//! [`FlightScript`] takes off, climbs and levels off.

use flight_lab::{FlightLabPlugin, FlightScript};
use regia::prelude::*;

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
            project_dir: Some(env!("CARGO_MANIFEST_DIR").into()),
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
