// SPDX-License-Identifier: MIT-0
//! `mm_locomotion [--frames N] [--trace] [--agent]`: run the motion-matching course headless for N frames
//! (default 660, two laps) and print a summary. With `--agent` the agent server runs too (the
//! endpoint is written under `.regia/agent/` of the current directory) and the app keeps
//! stepping at 30 frames per second until it is stopped. `--trace` prints the course leg, the
//! playing clip and the latest search every 15 frames.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let frames = match args.iter().position(|a| a == "--frames") {
        None => 660,
        Some(i) => match args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
            Some(n) => n,
            None => {
                eprintln!("mm_locomotion: --frames needs a number");
                return ExitCode::FAILURE;
            }
        },
    };
    let agent = args.iter().any(|a| a == "--agent");
    let mut app = mm_locomotion::headless_app();
    if agent {
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: std::env::current_dir().ok(),
        })
        .add_plugins(regia_agent::AgentPlugin);
        app.finish_plugins();
        loop {
            app.update();
            std::thread::sleep(std::time::Duration::from_secs_f32(mm_locomotion::FRAME));
        }
    }
    let trace = args.iter().any(|a| a == "--trace");
    for frame in 0..frames {
        app.update();
        if trace && frame % 15 == 0 {
            println!("{}", mm_locomotion::trace_line(app.world_mut()));
        }
    }
    for line in mm_locomotion::summary(app.world_mut()) {
        println!("{line}");
    }
    ExitCode::SUCCESS
}
