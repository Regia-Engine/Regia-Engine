// SPDX-License-Identifier: MIT-0
//! Desktop entry for the M7 UI gallery sample.

use std::process::ExitCode;

use regia::prelude::*;
use ui_gallery::gallery::UiGallery;

/// Project directory of this sample, resolved at runtime (ADR-10370).
///
/// `cargo run` exports `CARGO_MANIFEST_DIR`. A binary under `target/<profile>`
/// walks up to `samples/ui_gallery`. A packaged game finds neither and returns
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
    let project = root.join("samples").join("ui_gallery");
    project.is_dir().then_some(project)
}

fn main() -> ExitCode {
    let exit = regia::run_with(UiGallery, |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - ui_gallery".into(),
                ..Default::default()
            },
        });
        app.insert_resource(regia::agent::AgentSettings {
            project_dir: sample_project_dir(),
        });
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
