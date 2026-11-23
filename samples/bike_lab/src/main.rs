// SPDX-License-Identifier: MIT-0
//! Headless bike lab. Prints one line and exits. No window.

use std::process::ExitCode;

fn main() -> ExitCode {
    let launch = match bike_lab::launch_from_args(std::env::args().skip(1)) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("bike_lab: {error}");
            return ExitCode::from(2);
        }
    };
    match launch {
        bike_lab::Launch::Help => {
            println!("{}", bike_lab::USAGE);
            ExitCode::SUCCESS
        }
        bike_lab::Launch::Run(inputs) => match bike_lab::run(&inputs) {
            Ok(report) => {
                println!("{report}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("bike_lab: [{}] {error}", error.code());
                ExitCode::from(2)
            }
        },
    }
}
