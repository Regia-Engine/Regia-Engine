// SPDX-License-Identifier: MIT-0
//! Flies `procgen_open_world` headless through the agent protocol and writes a shot per waypoint.
//!
//! ```text
//! cargo run -p procgen_open_world -- --out target/procgen_open_world
//! cargo run -p procgen_open_world -- --window   # a window with the agent server (`play.hitch_report`)
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use procgen_common::{call, drive, flag_value, generator, run_windowed, shot};
use procgen_open_world::{GENERATOR, SPEED_M_S, route_length};
use serde_json::json;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let out =
        flag_value(&args, "--out").map_or_else(|| PathBuf::from("target/procgen_open_world"), PathBuf::from);
    let project = std::env::temp_dir().join(format!("procgen_open_world-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&project) {
        eprintln!("procgen_open_world: {e}");
        return ExitCode::FAILURE;
    }
    if args.iter().any(|a| a == "--window") {
        let exit_after = flag_value(&args, "--exit-after-frames").and_then(|n| n.parse().ok());
        run_windowed(
            procgen_common::procgen_settings(procgen_open_world::assets_dir(), None, false),
            &project,
            exit_after,
            |app| {
                procgen_open_world::build(app.world_mut());
                app.add_systems(regia_app::prelude::Update, procgen_open_world::fly_camera);
            },
        );
        return ExitCode::SUCCESS;
    }
    let Some(app) = procgen_open_world::app(&project) else {
        return ExitCode::FAILURE;
    };
    drive(app, &project.clone(), move |client| {
        let e = generator(client, GENERATOR);
        let seconds = route_length() / SPEED_M_S;
        let stops = 6;
        for k in 0..=stops {
            let frames = (seconds / stops as f32 * 60.0) as u32;
            if k > 0 {
                call(client, "play.step", json!({ "frames": frames }));
            }
            call(
                client,
                "procgen.tiles_wait",
                json!({ "entity": e, "timeout_ms": 60_000 }),
            );
            shot(client, Some(&out.join(format!("waypoint_{k}.png"))), 1280, 720);
        }
    });
    let _ = std::fs::remove_dir_all(&project);
    ExitCode::SUCCESS
}
