// SPDX-License-Identifier: MIT-0
//! Headless nav world lab: `nav_world_lab [--trace <path>]` prints the report of one run (see the
//! library docs). `--trace` also writes every intermediate digest, one `section step digest` line
//! each, so two hosts' runs can be diffed for the first diverging step.

use std::fmt::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let trace_path = match args.iter().position(|a| a == "--trace") {
        None => None,
        Some(i) => match args.get(i + 1) {
            Some(path) => Some(std::path::PathBuf::from(path)),
            None => {
                eprintln!("nav_world_lab: --trace needs a path");
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
    let report = match nav_world_lab::run_traced(&nav_world_lab::LabConfig::default(), &mut trace) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("nav_world_lab: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{report}");
    let traffic = match nav_world_lab::traffic::run_traffic_traced(3600, &mut trace) {
        Ok(traffic) => traffic,
        Err(e) => {
            eprintln!("nav_world_lab: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{traffic}");
    if let Some(path) = trace_path
        && let Err(e) = std::fs::write(&path, lines)
    {
        eprintln!("nav_world_lab: writing {}: {e}", path.display());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
