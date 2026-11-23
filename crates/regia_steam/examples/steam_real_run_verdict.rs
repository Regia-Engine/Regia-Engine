// SPDX-License-Identifier: MPL-2.0
//! Judges the two evidence logs of the owner's two-account Steam run (spec 24 § 4, ADR-4896).
//! No SDK and no Steam client needed: copy the guest's log next to the host's and run
//!
//! ```console
//! cargo run -p regia_steam --example steam_real_run_verdict -- host.log guest.log
//! ```
//!
//! The same command judges the template-match leg's logs (`net-host.log net-guest.log`, ADR-5375):
//! the leg is told apart by the logs' first line.
//!
//! Prints one line, `PASS`, `NOT RUN (<code>)`, `INCOMPLETE (<code>): missing ...` or
//! `FAIL (<code>): ...`, and exits 0 only for `PASS` (1 for a failure or an incomplete run, 2 for
//! not run or a usage error). A missing file is `NOT RUN`, never a pass.

use std::path::PathBuf;
use std::process::ExitCode;

use regia_steam::real_run::RealRunVerdict;
use regia_steam::real_run_net::verify_leg_files;

fn main() -> ExitCode {
    // `args_os`: `std::env::args` panics on a log path that is not valid Unicode (a Latin-1
    // file name on Linux), before the usage line could even print.
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [host, guest] = args.as_slice() else {
        eprintln!("usage: steam_real_run_verdict <host.log> <guest.log>");
        return ExitCode::from(2);
    };
    match verify_leg_files(host, guest) {
        Ok(verdict) => {
            println!("{verdict}");
            match verdict {
                RealRunVerdict::Pass => ExitCode::SUCCESS,
                RealRunVerdict::NotRun { .. } => ExitCode::from(2),
                RealRunVerdict::Incomplete { .. } | RealRunVerdict::Fail { .. } => ExitCode::from(1),
            }
        }
        Err(err) => {
            eprintln!("could not read the logs: {err}");
            ExitCode::from(2)
        }
    }
}
