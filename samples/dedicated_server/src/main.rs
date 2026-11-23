// SPDX-License-Identifier: MIT-0
//! Desktop and dedicated-server entry point of `dedicated_server`: one `main` for both builds.

use std::process::ExitCode;

use dedicated_server::DedicatedServerGame;
use regia::prelude::*;

fn main() -> ExitCode {
    match regia::run(DedicatedServerGame) {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
