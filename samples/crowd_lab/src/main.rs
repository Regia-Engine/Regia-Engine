// SPDX-License-Identifier: MIT-0
//! Headless crowd lab: `crowd_lab [--agents N] [--steps N] [--chunk N] [--in-place | --travelling-hips] [--trace <path>]`
//! (see the library docs).
//!
//! `crowd_lab --cesium` instead retargets the Khronos CesiumMan clips from `anim_lab`'s cooked
//! cache onto the lab rigs and prints the reports (desktop only; not run by CI).

use std::fmt::Write as _;
use std::process::ExitCode;

fn arg(args: &[String], name: &str, default: usize) -> Result<usize, String> {
    match args.iter().position(|a| a == name) {
        None => Ok(default),
        Some(i) => args
            .get(i + 1)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("{name} needs a number")),
    }
}

/// `target/anim_cache/project` beside this checkout, resolved at runtime (ADR-10370).
///
/// `env!("CARGO_MANIFEST_DIR")` would embed the build machine. `cargo run` exports
/// the variable. A binary under `target/<profile>` walks up to the workspace root.
#[cfg(not(target_os = "android"))]
fn anim_cache_project() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") {
        return std::path::PathBuf::from(dir).join("../../target/anim_cache/project");
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(root) = exe
            .parent()
            .and_then(|dir| dir.parent())
            .and_then(|dir| dir.parent())
    {
        return root.join("target/anim_cache/project");
    }
    std::path::PathBuf::from("target/anim_cache/project")
}

/// `--cesium`: print the real-content retarget reports.
fn cesium() -> ExitCode {
    #[cfg(target_os = "android")]
    {
        eprintln!("crowd_lab --cesium reads the anim_lab cache on the desktop host");
        ExitCode::SUCCESS
    }
    #[cfg(not(target_os = "android"))]
    {
        let project = anim_cache_project();
        match crowd_lab::cesium_report(&project) {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("crowd_lab: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--cesium") {
        return cesium();
    }
    let defaults = crowd_lab::LabConfig::default();
    let config = match (|| -> Result<crowd_lab::LabConfig, String> {
        Ok(crowd_lab::LabConfig {
            agents: arg(&args, "--agents", defaults.agents)?,
            steps: arg(&args, "--steps", defaults.steps)?,
            chunk: arg(&args, "--chunk", defaults.chunk)?,
            shape: if args.iter().any(|a| a == "--in-place") {
                crowd_lab::ClipShape::InPlace
            } else if args.iter().any(|a| a == "--travelling-hips") {
                crowd_lab::ClipShape::TravellingHips
            } else {
                crowd_lab::ClipShape::RootMotion
            },
        })
    })() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("crowd_lab: {e}");
            return ExitCode::FAILURE;
        }
    };
    let trace_path = match args.iter().position(|a| a == "--trace") {
        None => None,
        Some(i) => match args.get(i + 1) {
            Some(path) => Some(std::path::PathBuf::from(path)),
            None => {
                eprintln!("crowd_lab: --trace needs a path");
                return ExitCode::FAILURE;
            }
        },
    };
    let mut lines = String::new();
    let mut trace = |section: &str, step: u32, digest: u64| {
        if trace_path.is_some() {
            let _ = writeln!(lines, "{section} {step} {digest:016x}");
        }
    };
    match crowd_lab::run_traced(&config, &mut trace) {
        Ok(summary) => {
            for line in summary.lines() {
                println!("{line}");
            }
            if let Some(path) = trace_path
                && let Err(e) = std::fs::write(&path, lines)
            {
                eprintln!("crowd_lab: writing {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("crowd_lab: {e}");
            ExitCode::FAILURE
        }
    }
}
