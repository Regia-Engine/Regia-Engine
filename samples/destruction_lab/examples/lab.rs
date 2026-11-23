// SPDX-License-Identifier: MIT-0
//! Runs the lab: `cargo run -p destruction_lab --example lab` opens a window on client 0, with the
//! default script. `-- --headless [--ticks N]` runs without a window and prints the report.

#[path = "../kit/mod.rs"]
mod kit;

use std::process::ExitCode;

use destruction_lab::agent::install_lab_methods;
use destruction_lab::view::LabView;
use destruction_lab::{default_script, lab_app, lab_report};
use regia::prelude::*;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");
    let ticks = match args.iter().position(|a| a == "--ticks") {
        None => 120,
        Some(i) => match args.get(i + 1).map(|v| v.parse::<u64>()) {
            Some(Ok(n)) => n,
            _ => {
                eprintln!("destruction_lab: --ticks needs a number");
                return ExitCode::from(2);
            }
        },
    };
    let kit = match kit::fracture_kit() {
        Ok(kit) => kit,
        Err(e) => {
            eprintln!("destruction_lab: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut app = match lab_app(&kit, default_script()) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("destruction_lab: {}: {e}", e.code());
            return ExitCode::FAILURE;
        }
    };
    if headless {
        app.finish_plugins();
        for _ in 0..ticks {
            app.update();
        }
        let Some(report) = lab_report(app.world()) else {
            return ExitCode::FAILURE;
        };
        println!("ticks {}", report.ticks);
        println!("clients agree: {}", report.clients_agree);
        println!(
            "broken bonds {} ({:#018x}), debris {}, chunk poses {:#018x}",
            report.clients[0].playback.broken_bond_count,
            report.clients[0].playback.broken_bond_digest,
            report.clients[0].playback.debris_count,
            report.clients[0].playback.chunk_pose_digest,
        );
        println!(
            "server: broken bonds {} ({:#018x}), debris {}, chunk poses {:#018x}",
            report.server.playback.broken_bond_count,
            report.server.playback.broken_bond_digest,
            report.server.playback.debris_count,
            report.server.playback.chunk_pose_digest,
        );
        for instance in &report.clients[0].playback.instances {
            println!(
                "{}: {} attached, {} detached, {} flying, {} bonds broken",
                instance.asset,
                instance.attached,
                instance.detached,
                instance.debris,
                instance.broken_bonds.len()
            );
        }
        return if report.clients_agree {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    install_lab_methods(app.world_mut());
    app.add_plugins(DefaultPlugins).add_plugins(LabView { kit });
    app.insert_resource(regia::agent::AgentSettings {
        project_dir: Some(env!("CARGO_MANIFEST_DIR").into()),
    });
    match app.run() {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
