// SPDX-License-Identifier: MIT-0
//! Desktop entry for the M7 UI gallery sample.

use std::process::ExitCode;

use regia::prelude::*;
use ui_gallery::gallery::UiGallery;

fn main() -> ExitCode {
    let exit = regia::run_with(UiGallery, |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine - ui_gallery".into(),
                ..Default::default()
            },
        });
        app.insert_resource(regia::agent::AgentSettings {
            project_dir: Some(env!("CARGO_MANIFEST_DIR").into()),
        });
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
