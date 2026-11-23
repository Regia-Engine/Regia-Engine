// SPDX-License-Identifier: MIT-0
//! `lookdev` command line: render the shot list, or compare two `shots.json` files.

use std::process::ExitCode;

use lookdev::LookdevError;
use lookdev::report::{self, Command};

fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("lookdev=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        println!("{}", report::help_text());
        return ExitCode::from(1);
    }
    match run(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("lookdev: {err}");
            ExitCode::from(1)
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, LookdevError> {
    match report::parse_args(args)? {
        Command::Help => {
            println!("{}", report::help_text());
            Ok(ExitCode::SUCCESS)
        }
        Command::Shots { dir, config } => {
            let device = regia_render::RenderDevice::headless()?;
            let report = report::render_shot_list(&device, &config, &dir, |shot| {
                println!(
                    "shot {} {}x{} lum {:.1} contrast {:.1} stability {:.3} gpu {:.2} ms",
                    shot.id,
                    shot.width,
                    shot.height,
                    shot.mean_luminance,
                    shot.contrast,
                    shot.temporal_stability,
                    shot.gpu_ms.total
                );
            })?;
            println!(
                "wrote {} ({} shots, gpu_timing={})",
                dir.join("shots.json").display(),
                report.shots.len(),
                report.gpu_timing
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Compare { old, new } => {
            let old_report = report::read_report(&old)?;
            let new_report = report::read_report(&new)?;
            println!("{}", report::compare_reports(&old_report, &new_report));
            Ok(ExitCode::SUCCESS)
        }
        Command::Hull { path } => {
            let device = regia_render::RenderDevice::headless()?;
            report::render_hull(&device, &path)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::HullCost { path } => {
            let device = regia_render::RenderDevice::headless()?;
            report::render_hull_cost(&device, &path)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
