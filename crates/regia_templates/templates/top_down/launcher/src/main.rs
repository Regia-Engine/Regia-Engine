// SPDX-License-Identifier: MIT-0
//! Desktop entry point of {{name}}.
//!
//! `--exit-after-frames N` renders N frames, then exits with the game's exit code (smoke tests and
//! CI; "Checking that it starts" in the book's Running and iterating page).

// A shipped Windows game opens no console window (the GUI subsystem); debug builds keep the console
// for logs (ADR-2306, `regia package` checks the shipped exe).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::process::ExitCode;

use regia::prelude::*;

fn main() -> ExitCode {
    // `args_os`: a non-UTF-8 argument is skipped, never a panic.
    let exit_after = exit_after_frames(std::env::args_os().skip(1).filter_map(|a| a.into_string().ok()));
    let exit = regia::run_with({{crate_name}}::Game, move |app| {
        if let Some(frames) = exit_after {
            app.add_systems(Last, move |mut seen: Local<u64>, mut exit: MessageWriter<AppExit>| {
                *seen += 1;
                if *seen >= frames {
                    exit.write(AppExit::Success);
                }
            });
        }
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}

/// The value of `--exit-after-frames N` (or `--exit-after-frames=N`), if given.
fn exit_after_frames(mut args: impl Iterator<Item = String>) -> Option<u64> {
    while let Some(arg) = args.next() {
        if arg == "--exit-after-frames" {
            return args.next()?.parse().ok();
        }
        if let Some(value) = arg.strip_prefix("--exit-after-frames=") {
            return value.parse().ok();
        }
    }
    None
}
