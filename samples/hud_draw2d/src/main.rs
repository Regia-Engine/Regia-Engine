// SPDX-License-Identifier: MIT-0
//! Desktop entry point of `hud_draw2d`: the sample HUD over a cleared window.
//!
//! `--exit-after-frames N` stops after N frames (automated runs).

use std::process::ExitCode;

use hud_draw2d::HudSamplePlugin;
use regia_app::prelude::*;

/// Exits after this many frames when set.
#[derive(Resource)]
struct ExitAfter(u64);

fn exit_after(mut frames: Local<u64>, limit: Res<ExitAfter>, mut exit: MessageWriter<AppExit>) {
    *frames += 1;
    if *frames >= limit.0 {
        exit.write(AppExit::Success);
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let exit_after_frames = match args.iter().position(|a| a == "--exit-after-frames") {
        None => None,
        Some(i) => match args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
            Some(n) => Some(n),
            None => {
                eprintln!("hud_draw2d: --exit-after-frames needs a number");
                return ExitCode::from(2);
            }
        },
    };
    let exit = regia::run_with(HudSamplePlugin, move |app| {
        if let Some(n) = exit_after_frames {
            app.insert_resource(ExitAfter(n)).add_systems(Update, exit_after);
        }
    });
    if matches!(exit, AppExit::Success) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
