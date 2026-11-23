// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask web-player`: the web player's static files and their size report (ADR-2541).
//!
//! Builds `regia_web` for `wasm32-unknown-unknown` in the `web-release` profile, runs `wasm-bindgen`
//! (which must match the crate's version exactly), then `wasm-opt` when it is on the `PATH`, and reports each
//! file raw, gzip and brotli against the budget of `docs/specs/42-product-viewer.md` § 8.5.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The target: at most this much on the wire, brotli (`docs/specs/42-product-viewer.md` § 8.5).
pub const TARGET_BROTLI: u64 = 2_000_000;
/// The hard cap: above this the command fails.
pub const CAP_BROTLI: u64 = 3_000_000;

/// Arguments of `cargo xtask web-player`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Where the files go (default `target/web-player`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Skip `wasm-opt` even when it is installed.
    #[arg(long)]
    pub no_opt: bool,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

/// One file of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeRow {
    /// The file name.
    pub name: String,
    /// Bytes on disk.
    pub raw: u64,
    /// Bytes after `gzip -9`, when the tool ran.
    pub gzip: Option<u64>,
    /// Bytes after `brotli -q 11`, when the tool ran.
    pub brotli: Option<u64>,
}

/// The version of the `wasm-bindgen` crate in a `Cargo.lock`.
pub fn locked_version(lock: &str) -> Option<String> {
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line == "name = \"wasm-bindgen\"" {
            let version = lines.next()?;
            return version
                .strip_prefix("version = \"")?
                .strip_suffix('"')
                .map(str::to_string);
        }
    }
    None
}

/// The version `wasm-bindgen --version` prints.
pub fn tool_version(output: &str) -> Option<String> {
    output.split_whitespace().nth(1).map(str::to_string)
}

/// The total brotli size of `rows`, and whether every row has one.
pub fn total_brotli(rows: &[SizeRow]) -> Option<u64> {
    rows.iter().map(|r| r.brotli).sum()
}

/// What the report concludes.
pub fn verdict(total: Option<u64>) -> &'static str {
    match total {
        None => "no brotli tool: gzip only, install brotli for the budget check",
        Some(t) if t > CAP_BROTLI => "OVER THE CAP",
        Some(t) if t > TARGET_BROTLI => "over the target, under the cap",
        Some(_) => "within the target",
    }
}

/// The report as text.
pub fn render(rows: &[SizeRow]) -> String {
    let mut out = format!("{:<28}{:>12}{:>12}{:>12}\n", "file", "raw", "gzip", "brotli");
    let cell = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |n| n.to_string());
    for r in rows {
        out.push_str(&format!(
            "{:<28}{:>12}{:>12}{:>12}\n",
            r.name,
            r.raw,
            cell(r.gzip),
            cell(r.brotli)
        ));
    }
    let total = total_brotli(rows);
    out.push_str(&format!(
        "total brotli {} (target {TARGET_BROTLI}, cap {CAP_BROTLI}): {}\n",
        cell(total),
        verdict(total)
    ));
    out
}

/// The report as JSON.
pub fn render_json(rows: &[SizeRow]) -> String {
    let opt = |v: Option<u64>| v.map_or_else(|| "null".to_string(), |n| n.to_string());
    let items: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{{\"name\":\"{}\",\"raw\":{},\"gzip\":{},\"brotli\":{}}}",
                r.name,
                r.raw,
                opt(r.gzip),
                opt(r.brotli)
            )
        })
        .collect();
    format!(
        "{{\"files\":[{}],\"total_brotli\":{}}}",
        items.join(","),
        opt(total_brotli(rows))
    )
}

fn on_path(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn compressed(tool: &str, args: &[&str], path: &Path) -> Option<u64> {
    let output = Command::new(tool).args(args).arg(path).output().ok()?;
    output.status.success().then_some(output.stdout.len() as u64)
}

fn run_checked(command: &mut Command) -> Result<()> {
    let status = command.status().with_context(|| format!("running {command:?}"))?;
    if !status.success() {
        bail!("{command:?} failed: {status}");
    }
    Ok(())
}

/// Runs the command.
pub fn run(root: &Path, args: &Args) -> Result<()> {
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).context("reading Cargo.lock")?;
    let locked = locked_version(&lock).context("Cargo.lock has no wasm-bindgen")?;
    let tool = Command::new("wasm-bindgen")
        .arg("--version")
        .output()
        .with_context(|| {
            format!(
                "wasm-bindgen is not installed: cargo install wasm-bindgen-cli --version {locked} --locked"
            )
        })?;
    let tool = tool_version(&String::from_utf8_lossy(&tool.stdout)).unwrap_or_default();
    if tool != locked {
        bail!(
            "wasm-bindgen {tool} does not match the crate's {locked}: cargo install wasm-bindgen-cli --version {locked} --locked"
        );
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    run_checked(Command::new(cargo).current_dir(root).args([
        "build",
        "--profile",
        "web-release",
        "--target",
        "wasm32-unknown-unknown",
        "-p",
        "regia_web",
    ]))?;
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let wasm = target.join("wasm32-unknown-unknown/web-release/regia_web.wasm");
    let out = args.out.clone().unwrap_or_else(|| target.join("web-player"));
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    run_checked(
        Command::new("wasm-bindgen")
            .args(["--target", "web", "--no-typescript", "--out-dir"])
            .arg(&out)
            .arg(&wasm),
    )?;
    let module = out.join("regia_web_bg.wasm");
    if !args.no_opt && on_path("wasm-opt") {
        // wgpu's WebGL path and the browser's WebGPU glue use these post-MVP features.
        run_checked(
            Command::new("wasm-opt")
                .args([
                    "-Oz",
                    "--enable-bulk-memory",
                    "--enable-nontrapping-float-to-int",
                    "--enable-sign-ext",
                    "--enable-mutable-globals",
                    "--enable-reference-types",
                    "--enable-multivalue",
                    "-o",
                ])
                .arg(&module)
                .arg(&module),
        )?;
    } else if !args.no_opt {
        eprintln!("wasm-opt not found: sizes are without it");
    }

    let mut rows = Vec::new();
    for name in ["regia_web_bg.wasm", "regia_web.js"] {
        let path = out.join(name);
        let raw = std::fs::metadata(&path)
            .with_context(|| path.display().to_string())?
            .len();
        rows.push(SizeRow {
            name: name.to_string(),
            raw,
            gzip: compressed("gzip", &["-9c"], &path),
            brotli: compressed("brotli", &["-c", "-q", "11"], &path),
        });
    }
    if args.json {
        println!("{}", render_json(&rows));
    } else {
        print!("{}", render(&rows));
        println!("files in {}", out.display());
    }
    if total_brotli(&rows).is_some_and(|t| t > CAP_BROTLI) {
        bail!("the web player is over the {CAP_BROTLI} byte cap");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_locked_version() {
        let lock = "[[package]]\nname = \"wasm-bindgen-shared\"\nversion = \"9\"\n\n[[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.129\"\nsource = \"x\"\n";
        assert_eq!(locked_version(lock).as_deref(), Some("0.2.129"));
        assert_eq!(locked_version("[[package]]\nname = \"other\"\n"), None);
    }

    #[test]
    fn reads_the_tool_version() {
        assert_eq!(tool_version("wasm-bindgen 0.2.129\n").as_deref(), Some("0.2.129"));
        assert_eq!(tool_version(""), None);
    }

    fn row(brotli: Option<u64>) -> SizeRow {
        SizeRow {
            name: "a".to_string(),
            raw: 10,
            gzip: Some(5),
            brotli,
        }
    }

    #[test]
    fn the_total_needs_every_file() {
        assert_eq!(total_brotli(&[row(Some(3)), row(Some(4))]), Some(7));
        assert_eq!(total_brotli(&[row(Some(3)), row(None)]), None);
    }

    #[test]
    fn the_verdict_follows_the_budget() {
        assert_eq!(verdict(Some(TARGET_BROTLI)), "within the target");
        assert_eq!(verdict(Some(TARGET_BROTLI + 1)), "over the target, under the cap");
        assert_eq!(verdict(Some(CAP_BROTLI + 1)), "OVER THE CAP");
        assert!(verdict(None).starts_with("no brotli"));
    }

    #[test]
    fn the_reports_carry_every_file() {
        let text = render(&[row(Some(3))]);
        assert!(text.contains("total brotli 3"), "{text}");
        assert_eq!(
            render_json(&[row(None)]),
            r#"{"files":[{"name":"a","raw":10,"gzip":5,"brotli":null}],"total_brotli":null}"#
        );
    }
}
