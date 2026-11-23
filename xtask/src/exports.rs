// SPDX-License-Identifier: MPL-2.0
//! `check-exports`: dynamic-symbol budget for `regia_dylib` (ADR-015, `docs/06` § 5).
//!
//! Warn at 50,000, fail at 60,000. Linux counts `nm -D --defined-only`. Windows counts
//! `dumpbin /exports` ("number of functions"), which is the PE export table the 65,535 limit
//! applies to.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Print a warning once the dynamic-symbol count reaches this.
pub const WARN_AT: u64 = 50_000;
/// Fail once the count reaches this. The Windows DLL format itself stops at 65,535.
pub const FAIL_AT: u64 = 60_000;

/// What CI should do with a measured count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Under the warning line.
    Ok,
    /// At or above [`WARN_AT`] and below [`FAIL_AT`].
    Warn,
    /// At or above [`FAIL_AT`].
    Fail,
}

/// Classify `count`.
pub fn verdict(count: u64) -> Verdict {
    if count >= FAIL_AT {
        Verdict::Fail
    } else if count >= WARN_AT {
        Verdict::Warn
    } else {
        Verdict::Ok
    }
}

/// Pull the `regia_dylib` artifact path out of `cargo build --message-format=json` stdout.
pub fn dylib_from_cargo_json(text: &str) -> Option<PathBuf> {
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let name = value.pointer("/target/name").and_then(|n| n.as_str());
        if name != Some("regia_dylib") {
            continue;
        }
        let filenames = value.get("filenames").and_then(|f| f.as_array())?;
        for filename in filenames {
            let Some(path) = filename.as_str() else { continue };
            if path.ends_with(".so") || path.ends_with(".dll") || path.ends_with(".dylib") {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

/// Count defined dynamic symbols in `nm -D --defined-only` output.
pub fn count_nm_defined(stdout: &str) -> u64 {
    stdout.lines().filter(|line| !line.trim().is_empty()).count() as u64
}

/// Read the "N number of functions" line from `dumpbin /exports`.
pub fn count_dumpbin_exports(stdout: &str) -> Result<u64> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_suffix("number of functions") {
            let n: u64 = rest
                .trim()
                .parse()
                .with_context(|| format!("dumpbin function count in {trimmed:?}"))?;
            return Ok(n);
        }
    }
    bail!("dumpbin output has no 'number of functions' line")
}

/// Build `regia_dylib` and return the dynamic library path.
pub fn build_dylib(root: &Path) -> Result<PathBuf> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-p", "regia_dylib", "--message-format=json"])
        .current_dir(root)
        .output()
        .context("starting cargo build -p regia_dylib")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("cargo build -p regia_dylib failed\n{stderr}");
    }
    dylib_from_cargo_json(&stdout).context("cargo did not report a regia_dylib artifact")
}

/// Count dynamic symbols in `lib`.
pub fn count_library(lib: &Path) -> Result<u64> {
    let ext = lib.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("dll") {
        count_dll(lib)
    } else {
        count_elf_or_mach(lib)
    }
}

fn count_elf_or_mach(lib: &Path) -> Result<u64> {
    let output = Command::new("nm")
        .args(["-D", "--defined-only"])
        .arg(lib)
        .output()
        .context("running nm -D --defined-only (binutils)")?;
    if !output.status.success() {
        bail!("nm failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(count_nm_defined(&String::from_utf8_lossy(&output.stdout)))
}

fn count_dll(lib: &Path) -> Result<u64> {
    let dumpbin = find_dumpbin()?;
    let output = Command::new(&dumpbin)
        .arg("/exports")
        .arg(lib)
        .output()
        .with_context(|| format!("running {} /exports", dumpbin.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        bail!(
            "dumpbin failed: {}\n{stdout}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    count_dumpbin_exports(&stdout)
}

fn find_dumpbin() -> Result<PathBuf> {
    if let Ok(path) = which_on_path("dumpbin") {
        return Ok(path);
    }
    let vswhere = PathBuf::from(r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe");
    if vswhere.is_file() {
        let output = Command::new(&vswhere)
            .args([
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-find",
                r"VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe",
            ])
            .output()
            .context("running vswhere")?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = text.lines().find(|line| !line.is_empty()) {
                return Ok(PathBuf::from(line.trim()));
            }
        }
    }
    bail!("dumpbin.exe not on PATH and vswhere did not find it")
}

fn which_on_path(name: &str) -> Result<PathBuf> {
    let output = if cfg!(windows) {
        Command::new("where").arg(name).output()
    } else {
        Command::new("which").arg(name).output()
    };
    let output = output.with_context(|| format!("searching PATH for {name}"))?;
    if !output.status.success() {
        bail!("{name} not on PATH");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .find(|l| !l.is_empty())
        .context("empty search result")?;
    Ok(PathBuf::from(line.trim()))
}

/// Build (unless `lib` is set), count, print, and return the process exit code.
pub fn run(root: &Path, lib: Option<&Path>) -> Result<std::process::ExitCode> {
    let owned;
    let lib = if let Some(lib) = lib {
        lib
    } else {
        owned = build_dylib(root)?;
        &owned
    };
    let count = count_library(lib)?;
    let verdict = verdict(count);
    println!("regia_dylib dynamic symbols: {count} ({})", lib.display());
    match verdict {
        Verdict::Ok => println!("check-exports: ok (warn at {WARN_AT}, fail at {FAIL_AT})"),
        Verdict::Warn => {
            println!(
                "::warning::regia_dylib has {count} dynamic symbols (warn at {WARN_AT}, fail at {FAIL_AT})"
            );
            eprintln!("warning: regia_dylib has {count} dynamic symbols, warn line is {WARN_AT}");
        }
        Verdict::Fail => {
            eprintln!("error: regia_dylib has {count} dynamic symbols, fail line is {FAIL_AT}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_boundaries() {
        assert_eq!(verdict(0), Verdict::Ok);
        assert_eq!(verdict(WARN_AT - 1), Verdict::Ok);
        assert_eq!(verdict(WARN_AT), Verdict::Warn);
        assert_eq!(verdict(FAIL_AT - 1), Verdict::Warn);
        assert_eq!(verdict(FAIL_AT), Verdict::Fail);
        assert_eq!(verdict(65_535), Verdict::Fail);
    }

    #[test]
    fn nm_count_skips_blank_lines() {
        let text = "0000000000001110 T foo\n\n0000000000002220 T bar\n";
        assert_eq!(count_nm_defined(text), 2);
    }

    #[test]
    fn dumpbin_reads_function_count() {
        let text = "\
           34479 number of functions\n\
           34479 number of names\n";
        assert_eq!(count_dumpbin_exports(text).expect("function count"), 34_479);
    }

    #[test]
    fn cargo_json_picks_the_dylib() {
        let text = r#"{"reason":"compiler-artifact","target":{"name":"regia_dylib"},"filenames":["/t/deps/libregia_dylib.rlib","/t/debug/libregia_dylib.so"]}"#;
        let path = dylib_from_cargo_json(text).expect("dylib path");
        assert!(path.ends_with("libregia_dylib.so"));
    }

    #[test]
    fn dumpbin_without_the_line_errors() {
        assert!(count_dumpbin_exports("no exports here").is_err());
    }
}
