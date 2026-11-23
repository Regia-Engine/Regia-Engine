// SPDX-License-Identifier: MIT-0
//! Headless physics lab: `physics_lab` prints the report of one run (see the library docs).

use std::process::ExitCode;

fn main() -> ExitCode {
    match physics_lab::run(&physics_lab::LabConfig::default()) {
        Ok(report) => {
            println!("{report}");
            println!("traversal course: {:?}", physics_lab::course::run_course());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("physics_lab: {e}");
            ExitCode::FAILURE
        }
    }
}
