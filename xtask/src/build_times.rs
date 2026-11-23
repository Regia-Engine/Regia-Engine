// SPDX-License-Identifier: MPL-2.0
//! `build-times`: measures the build budgets of `docs/03-workspace-and-dependencies.md` § 6.
//!
//! Builds into its own target directory so the developer's cache is untouched. Writes
//! `target/build-times/report.md` and `report.json`, plus cargo's `--timings` HTML pages.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// One measured build.
struct Measurement {
    name: &'static str,
    seconds: f64,
    budget_seconds: Option<f64>,
}

/// The line the incremental measurement appends to `crates/regia/src/lib.rs`.
const PROBE: &str = "\n#[doc(hidden)]\npub const BUILD_TIMES_PROBE: u32 = 1;\n";

/// `text` without a probe a killed earlier run left at its end (Ctrl-C skips the `Restore` drop),
/// so the probe is never appended twice and the restore writes the real original.
fn without_probe(text: &str) -> &str {
    text.strip_suffix(PROBE).unwrap_or(text)
}

/// Restores a source file when dropped, so a measurement that fails or panics leaves the tree
/// clean. A signal (Ctrl-C) skips it; the next run removes the leftover probe (`without_probe`).
struct Restore {
    path: PathBuf,
    original: String,
}

impl Drop for Restore {
    fn drop(&mut self) {
        if let Err(err) = fs::write(&self.path, &self.original) {
            eprintln!("failed to restore {}: {err}", self.path.display());
        }
    }
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn timed_build(target_dir: &Path, args: &[&str]) -> Result<Duration> {
    let start = Instant::now();
    let status = Command::new(cargo())
        .arg("build")
        .args(args)
        .env("CARGO_TARGET_DIR", target_dir)
        .status()
        .with_context(|| format!("running cargo build {args:?}"))?;
    if !status.success() {
        bail!("cargo build {args:?} failed");
    }
    Ok(start.elapsed())
}

/// Runs the measurements. `clean` wipes the dedicated target directory first.
pub fn run(root: &Path, clean: bool) -> Result<()> {
    let out_dir = root.join("target/build-times");
    let target_dir = out_dir.join("target");
    if clean && target_dir.exists() {
        fs::remove_dir_all(&target_dir).context("removing build-times target directory")?;
    }
    fs::create_dir_all(&out_dir)?;
    let mut results = Vec::new();

    let editor = timed_build(&target_dir, &["-p", "regia_editor", "--timings"])?;
    results.push(Measurement {
        name: if clean {
            "Clean build of the editor (dev)"
        } else {
            "Editor build (dev, warm cache)"
        },
        seconds: editor.as_secs_f64(),
        budget_seconds: clean.then_some(360.0),
    });
    let rest = timed_build(&target_dir, &["--workspace", "--all-targets", "--timings"])?;
    results.push(Measurement {
        name: "Rest of workspace, all targets (dev, after editor)",
        seconds: rest.as_secs_f64(),
        budget_seconds: None,
    });

    // One-line change in the crate at the top of the runtime graph: the closest stand-in for a game
    // crate until samples exist (M3).
    // Warm up first: `-p regia` resolves features differently from `--workspace`, and that rebuild of
    // dependencies must not be counted as the incremental change.
    timed_build(&target_dir, &["-p", "regia"])?;
    let lib = root.join("crates/regia/src/lib.rs");
    let original = fs::read_to_string(&lib).with_context(|| format!("reading {}", lib.display()))?;
    let original = without_probe(&original).to_owned();
    {
        let _restore = Restore {
            path: lib.clone(),
            original: original.clone(),
        };
        fs::write(&lib, format!("{original}{PROBE}"))?;
        let incremental = timed_build(&target_dir, &["-p", "regia"])?;
        results.push(Measurement {
            name: "Incremental rebuild after a one-line change (crate `regia`)",
            seconds: incremental.as_secs_f64(),
            budget_seconds: Some(5.0),
        });
    }
    // Rebuild the restored file so the next run starts from the committed source.
    timed_build(&target_dir, &["-p", "regia"])?;

    let mut md = String::from("| Build | Seconds | Budget | Result |\n|---|---|---|---|\n");
    let mut json = Vec::new();
    for m in &results {
        let (budget, verdict) = match m.budget_seconds {
            Some(b) => (format!("≤ {b:.0}"), if m.seconds <= b { "pass" } else { "over" }),
            None => ("—".into(), "report"),
        };
        md.push_str(&format!(
            "| {} | {:.1} | {budget} | {verdict} |\n",
            m.name, m.seconds
        ));
        json.push(serde_json::json!({
            "name": m.name,
            "seconds": (m.seconds * 10.0).round() / 10.0,
            "budget_seconds": m.budget_seconds,
        }));
    }
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    md.push_str(&format!(
        "\nHost: {} {}, {threads} hardware threads.\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    fs::write(out_dir.join("report.md"), &md)?;
    fs::write(
        out_dir.join("report.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "threads": threads, "builds": json }))?,
    )?;
    print!("{md}");
    println!("Timings pages: {}", target_dir.join("cargo-timings").display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_left_by_a_killed_run_is_removed_before_the_next_one() {
        let original = "//! The facade.\npub mod prelude;\n";
        let left_over = format!("{original}{PROBE}");
        assert_eq!(without_probe(&left_over), original);
        // A clean file is unchanged, so the probe is appended exactly once.
        assert_eq!(without_probe(original), original);
        assert_eq!(format!("{}{PROBE}", without_probe(&left_over)), left_over);
    }
}
