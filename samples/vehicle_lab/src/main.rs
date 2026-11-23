// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `vehicle_lab`: a window on the lab with the agent protocol on.
//!
//! Flags: `--no-script` leaves the vehicles parked for an agent to drive; without it [`LabScript`]
//! runs a launch, a weave and a stop in a loop.

use regia::prelude::*;
use vehicle_lab::{LabScript, VehicleLabPlugin};

fn main() -> std::process::ExitCode {
    let scripted = !std::env::args().any(|arg| arg == "--no-script");
    let exit = regia::run_with(VehicleLabPlugin, move |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - vehicle_lab".into(),
                ..Default::default()
            },
        });
        app.insert_resource(regia::agent::AgentSettings {
            project_dir: Some(env!("CARGO_MANIFEST_DIR").into()),
        });
        if scripted {
            app.insert_resource(LabScript);
        }
    });
    match exit {
        AppExit::Success => std::process::ExitCode::SUCCESS,
        AppExit::Error(code) => std::process::ExitCode::from(code.get()),
    }
}
