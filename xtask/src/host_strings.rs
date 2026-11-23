// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask host-strings`: the privacy string gate over a built executable (ADR-10354).
//!
//! Runs the same check `regia package` applies to every staged executable
//! (`regia_cli::package_hygiene::check_executable_strings`) on demand, so a release binary can be
//! gated in CI or by hand without packaging first: a private build-machine name or directory root
//! (a private hostname, `/home/…`, `/media/…`, a LAN address, a tailnet name) in the binary's
//! printable strings refuses the check. The default target is the release build of the
//! `hello_window` sample, built on demand; `--binary` repeats for other executables. Output names
//! rule IDs, scrubbed paths and offsets, never the matched text: the error must not echo what it
//! rejects.
//!
//! The sample is built with plain cargo, so the check exercises the workspace's config-file
//! remaps. On an account whose name is itself private, a plain build keeps
//! `/builder/<account>/.cargo/…` dependency paths and the check reports them; there, restate the
//! root remaps in `RUSTFLAGS` with `--remap-path-prefix=$HOME=/builder` last (rustc applies the
//! last matching prefix), or package with `regia package`, which remaps the whole home directory.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;

/// Arguments of `cargo xtask host-strings`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Executable to scan (repeatable; default the release `hello_window` sample, built if absent).
    #[arg(long = "binary")]
    pub binaries: Vec<PathBuf>,
    /// Optional TOML allow-list (`[[allow]]` with `path`, `text`, `reason`) for reviewed exceptions.
    #[arg(long)]
    pub allow: Option<PathBuf>,
}

/// The release sample the check runs over when `--binary` is not given, and the package that
/// builds it.
const DEFAULT_PACKAGE: &str = "hello_window";
const DEFAULT_BINARY: &str = "target/release/hello_window";

/// Run `cargo xtask host-strings` from the workspace root: fails when any scanned executable is
/// rejected (exit 1), after printing every rejected binary.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let binaries: Vec<PathBuf> = if args.binaries.is_empty() {
        let binary = root.join(DEFAULT_BINARY);
        if !binary.is_file() {
            let status = Command::new("cargo")
                .current_dir(root)
                .args(["build", "--locked", "--profile", "release", "-p"])
                .arg(DEFAULT_PACKAGE)
                .status()
                .with_context(|| format!("building {DEFAULT_PACKAGE} (release)"))?;
            if !status.success() {
                bail!("cargo build -p {DEFAULT_PACKAGE} (release) failed");
            }
        }
        vec![binary]
    } else {
        args.binaries.clone()
    };
    let mut failed = false;
    for binary in &binaries {
        match regia_cli::package_hygiene::check_executable_strings(binary, args.allow.as_deref()) {
            Ok(report) => {
                println!(
                    "host-strings: {} clean ({} bytes scanned, {} reviewed exception{})",
                    binary.display(),
                    report.bytes_scanned,
                    report.allowed_findings,
                    if report.allowed_findings == 1 { "" } else { "s" },
                );
            }
            Err(error) => {
                failed = true;
                println!(
                    "host-strings: {} rejected ({} at {})",
                    binary.display(),
                    error.code,
                    error.location,
                );
                if let Some(report) = &error.report {
                    for finding in report.findings.iter().take(8) {
                        println!(
                            "  {} rule {} source {} offset {}",
                            finding.path, finding.rule, finding.source, finding.offset
                        );
                    }
                    if report.findings.len() > 8 {
                        println!("  … and {} more", report.findings.len() - 8);
                    }
                }
            }
        }
    }
    if failed {
        bail!("private host strings found; rebuild with the ADR-10354 path remaps");
    }
    Ok(ExitCode::SUCCESS)
}
