// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `lightmap_device`.

use std::path::PathBuf;
use std::process::ExitCode;

use lightmap_device::LightmapDevice;
use regia::prelude::*;

fn main() -> ExitCode {
    let exit = regia::run_with(LightmapDevice, |app| {
        app.insert_resource(regia::agent::AgentSettings {
            project_dir: agent_project_dir(),
        });
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}

/// The agent's project directory in a dev build: the sample's own folder, where
/// `regia agent --project samples/lightmap_device` looks for the endpoint.
#[cfg(debug_assertions)]
fn agent_project_dir() -> Option<PathBuf> {
    // Runtime path (ADR-10370). Release keeps the working directory below.
    sample_project_dir()
}

/// Project directory of this sample, resolved at runtime (ADR-10370).
///
/// Debug builds only. `cargo run` exports `CARGO_MANIFEST_DIR`. A binary under
/// `target/<profile>` walks up to `samples/lightmap_device`.
#[cfg(debug_assertions)]
fn sample_project_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return Some(dir.into());
    }
    let root = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .parent()?
        .to_path_buf();
    let project = root.join("samples").join("lightmap_device");
    project.is_dir().then_some(project)
}

/// The agent's project directory in a release build (the tier profiles' shipping packages, ADR-5382): the working
/// directory, so the binary carries no build-machine path (the package's ship check refuses one).
#[cfg(not(debug_assertions))]
fn agent_project_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}
