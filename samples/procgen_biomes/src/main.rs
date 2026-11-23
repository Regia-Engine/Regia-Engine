// SPDX-License-Identifier: MIT-0
//! Runs `procgen_biomes` headless through the agent protocol and writes review shots.
//!
//! ```text
//! cargo run -p procgen_biomes -- --out target/procgen_biomes
//! cargo run -p procgen_biomes -- --window   # a window with the agent server (`play.hitch_report`)
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use procgen_common::{call, drive, flag_value, generator, run_windowed, shot};
use serde_json::json;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let out =
        flag_value(&args, "--out").map_or_else(|| PathBuf::from("target/procgen_biomes"), PathBuf::from);
    let project = std::env::temp_dir().join(format!("procgen_biomes-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&project) {
        eprintln!("procgen_biomes: {e}");
        return ExitCode::FAILURE;
    }
    if args.iter().any(|a| a == "--window") {
        let exit_after = flag_value(&args, "--exit-after-frames").and_then(|n| n.parse().ok());
        run_windowed(
            procgen_common::procgen_settings(procgen_biomes::assets_dir(), None, false),
            &project,
            exit_after,
            |app| {
                let e = procgen_biomes::build(app.world_mut());
                regia_procgen::world::request_run(app.world_mut(), e);
            },
        );
        return ExitCode::SUCCESS;
    }
    let Some(app) = procgen_biomes::app(&project) else {
        return ExitCode::FAILURE;
    };
    drive(app, &project.clone(), move |client| {
        let e = generator(client, procgen_biomes::GENERATOR);
        let run = call(client, "procgen.run", json!({ "entity": e, "wait": true }));
        println!("run: {} instances", run["counts"]["instances"]);
        call(client, "play.step", json!({ "frames": 2 }));
        shot(client, Some(&out.join("overview.png")), 1280, 720);
    });
    let _ = std::fs::remove_dir_all(&project);
    ExitCode::SUCCESS
}
