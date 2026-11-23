// SPDX-License-Identifier: MPL-2.0
//! `regia bake-captures`: a level's local reflection capture bake (`.fcap`, ADR-2860,
//! `book/agents/reflection-captures.md`).
//!
//! `--check FILE` reads a `.fcap` offline and prints its summary as JSON, or its error code
//! (`fcap_bad_magic`, `fcap_version`, `fcap_truncated`, `fcap_crc`, `fcap_layout`, `fcap_value`,
//! `fcap_io`) with exit status 1.
//!
//! Without `--check` it drives the running app with the level open (as `regia render` does): it
//! queues a bake of every placed capture (`render.bake_local_capture`), waits until none is
//! pending, and writes them to `assets/<--file>` named in the open level
//! (`render.save_local_captures`); save the level afterwards (`level.save`). Captures are baked
//! by the renderer that draws the level, so the bake sees exactly what frames see.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_agent::client::Client;
use regia_asset::local_capture_bake::FcapFile;
use serde_json::{Value, json};

/// `regia bake-captures` arguments.
#[derive(Args, Debug, Clone)]
pub struct BakeCapturesArgs {
    /// Project directory of the running app (default current directory).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Asset path of the bake to write, ending in `.fcap` (default `maps/captures.fcap`).
    #[arg(long, default_value = "maps/captures.fcap")]
    pub file: String,
    /// Do not name the file in the open level's `look.captures`.
    #[arg(long)]
    pub no_level: bool,
    /// Give up waiting for the bakes after this many seconds.
    #[arg(long, default_value_t = 600)]
    pub timeout_s: u64,
    /// Read this `.fcap` offline and print its summary (no app needed).
    #[arg(long, value_name = "FCAP")]
    pub check: Option<PathBuf>,
}

/// The `--check` summary of a read file.
#[must_use]
pub fn summary(file: &FcapFile, bytes: u64) -> Value {
    json!({
        "ok": true,
        "format_version": regia_asset::local_capture_bake::FCAP_FORMAT_VERSION,
        "width": file.width,
        "mips": file.captures.first().map_or(0, |c| c.mips.len()),
        "bytes": bytes,
        "captures": file.captures.iter().map(|c| json!({
            "id": c.id,
            "center": c.center,
            "half_extent": c.half_extent,
            "weight": c.weight,
            "fade": c.fade,
            "coverage": c.coverage,
        })).collect::<Vec<_>>(),
    })
}

/// The `--check` result of `path`: the summary, or `{ ok: false, code, error }`.
#[must_use]
pub fn check(path: &std::path::Path) -> (bool, Value) {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match FcapFile::read(path) {
        Ok(file) => (true, summary(&file, bytes)),
        Err(e) => (
            false,
            json!({ "ok": false, "code": e.code(), "error": e.to_string(), "file": path.display().to_string() }),
        ),
    }
}

/// Runs the command.
///
/// # Errors
/// `--check` of a file that does not read (its code is printed), no running app, a refused call,
/// or the bakes did not finish within `--timeout-s`.
pub fn run(args: &BakeCapturesArgs) -> Result<()> {
    if let Some(path) = &args.check {
        let (ok, out) = check(path);
        println!("{}", serde_json::to_string_pretty(&out).context("serialize")?);
        if !ok {
            bail!(
                "{} does not read: {}",
                path.display(),
                out["code"].as_str().unwrap_or("fcap_io")
            );
        }
        return Ok(());
    }
    if !args.file.ends_with(".fcap") {
        bail!("--file {} must end in .fcap", args.file);
    }
    let project_dir = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    let mut client =
        Client::connect_project(&project_dir).context("cannot connect to a running Regia Engine app")?;
    let queued = client
        .call("render.bake_local_capture", json!({}))
        .context("render.bake_local_capture failed")?;
    eprintln!("queued {} capture bakes", queued["queued"].as_u64().unwrap_or(0));
    let deadline = Instant::now() + Duration::from_secs(args.timeout_s.max(1));
    loop {
        let list = client
            .call("render.list_local_captures", json!({}))
            .context("render.list_local_captures failed")?;
        let pending = list["captures"]
            .as_array()
            .map_or(0, |c| c.iter().filter(|c| c["pending"] == true).count());
        if pending == 0 {
            break;
        }
        if Instant::now() > deadline {
            bail!("{pending} capture bakes still pending after {} s", args.timeout_s);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let saved = client
        .call(
            "render.save_local_captures",
            json!({ "file": args.file, "name_in_level": !args.no_level }),
        )
        .context("render.save_local_captures failed")?;
    println!("{}", serde_json::to_string_pretty(&saved).context("serialize")?);
    if saved["named_in_level"] == true {
        eprintln!("named in the open level: save it (level.save) to keep the bake");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::local_capture_bake::{FcapCapture, fcap_mip_bytes};

    #[test]
    fn check_reads_a_bake_and_names_a_damaged_one() {
        let dir = regia_test_utils::unique_temp_dir("bake-captures-check");
        let file = FcapFile {
            width: 8,
            captures: vec![FcapCapture {
                id: 2,
                center: [0.0, 1.0, 0.0],
                half_extent: [1.0; 3],
                weight: 1.0,
                fade: 0.25,
                coverage: 0.5,
                mips: (0..2).map(|m| vec![0; fcap_mip_bytes(8, m)]).collect(),
            }],
        };
        let good = dir.join("good.fcap");
        file.write(&good).expect("write");
        let (ok, out) = check(&good);
        assert!(ok, "{out}");
        assert_eq!(out["captures"][0]["id"], 2);
        assert_eq!(out["mips"], 2);
        let mut bytes = std::fs::read(&good).expect("read");
        bytes.truncate(bytes.len() - 1);
        let bad = dir.join("bad.fcap");
        std::fs::write(&bad, &bytes).expect("write");
        let (ok, out) = check(&bad);
        assert!(!ok);
        assert_eq!(out["code"], "fcap_truncated");
        let (ok, out) = check(&dir.join("none.fcap"));
        assert!(!ok);
        assert_eq!(out["code"], "fcap_io");
        std::fs::remove_dir_all(&dir).ok();
    }
}
