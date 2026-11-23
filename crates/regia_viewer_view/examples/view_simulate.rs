// SPDX-License-Identifier: MPL-2.0
//! Run a `viewer.view_simulate` script from stdin and print the answer (the same function the agent
//! method calls): `echo '{"events": [{"wheel": 3}, {"settle": true}]}' | cargo run -p regia_viewer_view
//! --example view_simulate`.

use std::io::Read as _;

fn main() -> std::process::ExitCode {
    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("stdin: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let script: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("not JSON: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match regia_viewer_view::simulate(&script) {
        Ok(out) => {
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
