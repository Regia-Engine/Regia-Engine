// SPDX-License-Identifier: MPL-2.0
//! Incremental bakes through the real `regia` binary, without a GPU (ADR-1094).
//!
//! The baked-light fixture (`tests/fixtures/bake_e2e`, its `README.md`) bakes with
//! `--from-scene … --bake --cpu --incremental <state>` three times:
//!
//! 1. no state: every cell is traced, the state is written;
//! 2. nothing changed: nothing is traced, the volume is the same file byte for byte;
//! 3. the static Marker moved 1 m: the sealed room's brick, beyond the influence margin, is reused, the summary
//!    says the result is approximate, and the room's centre still reads no light.
//!
//! The test copies the fixture to a temporary directory; nothing is written into the tree.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const LEVEL: &str = "assets/levels/courtyard.level.ron";
const VOLUME: &str = "assets/maps/courtyard.fgiv";
const STATE: &str = "bake/state/courtyard.fgis";
/// Quick on the CPU in a debug build; a 2 m margin keeps the Marker out of the room's brick.
const BAKE_ARGS: [&str; 8] = [
    "--rays",
    "64",
    "--samples",
    "2",
    "--bounces",
    "2",
    "--influence",
    "2",
];

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bake_e2e")
}

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the copy");
    for entry in std::fs::read_dir(from).expect("read the fixture") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy a fixture file");
        }
    }
}

fn regia(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run regia")
}

/// The last JSON line on stdout.
fn json_line(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| {
            panic!(
                "no JSON line; stdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"))
}

/// `--from-scene … --bake --cpu --incremental`; returns the bake summary after checking the
/// command succeeded.
fn bake(dir: &Path) -> Value {
    let mut args = vec![
        "bake-gi",
        "--from-scene",
        LEVEL,
        "--bake",
        "--cpu",
        "--incremental",
        STATE,
    ];
    args.extend(BAKE_ARGS);
    let out = regia(dir, &args);
    let line = json_line(&out);
    assert!(
        out.status.success(),
        "{line}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(line["ok"], true, "{line}");
    line["bake"].clone()
}

fn count(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| panic!("not a count: {v}"))
}

#[test]
fn a_rebake_traces_only_what_an_edit_reaches() {
    let dir = regia_test_utils::unique_temp_dir("regia_bake_incremental_e2e");
    let _cleanup = Cleanup(dir.clone());
    copy_tree(&fixture(), &dir);

    // 1. No state yet: every cell is traced.
    let first = bake(&dir);
    let inc = &first["incremental"];
    assert_eq!(inc["full"], "no_state", "{inc}");
    let probes = count(&first["report"]["probes"]);
    assert_eq!(count(&inc["probes_baked"]), probes, "{inc}");
    assert_eq!(first["state"]["written"], true, "{first}");
    assert!(dir.join(STATE).is_file());
    let volume = std::fs::read(dir.join(VOLUME)).expect("the volume");

    // 2. Nothing changed: nothing is traced and the volume is the same file.
    let second = bake(&dir);
    let inc = &second["incremental"];
    assert!(inc["full"].is_null(), "{inc}");
    assert_eq!(count(&inc["probes_baked"]), 0, "{inc}");
    assert_eq!(count(&inc["probes_reused"]), probes, "{inc}");
    assert_eq!(inc["approximate"], false, "{inc}");
    assert_eq!(std::fs::read(dir.join(VOLUME)).expect("the volume"), volume);

    // 3. Move the Marker 1 m along +z: the sealed room's brick is beyond the margin and is reused.
    // Not along +x: the Marker is the scene's east end, so that grows the export's coarse
    // whole-scene brick, whose spacing sets the layout's max distance (twice the largest
    // spacing), and a different max distance rebakes every probe (`full: settings`).
    let level = dir.join(LEVEL);
    let original = std::fs::read_to_string(&level).expect("level");
    let moved = original.replace("(24.0, 0.0, 0.0)", "(24.0, 0.0, 1.0)");
    assert_ne!(
        moved, original,
        "the Marker's translation is in the fixture level"
    );
    regia_test_utils::write_atomic(&level, moved.as_bytes()).expect("move the marker");
    let third = bake(&dir);
    let inc = &third["incremental"];
    assert!(inc["full"].is_null(), "{inc}");
    assert!(count(&inc["cells_reused"]) >= 1, "the room's brick: {inc}");
    assert!(
        count(&inc["probes_baked"]) > 0,
        "the Marker's surroundings: {inc}"
    );
    assert_eq!(inc["scene_changed"], true, "{inc}");
    assert_eq!(inc["approximate"], true, "{inc}");
    let reasons: Vec<&str> = inc["baked"]
        .as_array()
        .expect("baked cells")
        .iter()
        .filter_map(|c| c["reason"].as_str())
        .collect();
    assert!(reasons.iter().all(|r| ["layout", "scene"].contains(r)), "{inc}");

    // The sealed room still reads no light.
    let sampled = regia(&dir, &["bake-gi", "--sample", VOLUME, "--at", "0,0,0"]);
    let s = json_line(&sampled);
    assert!(sampled.status.success(), "{s}");
    assert_eq!(s["samples"][0]["coverage"], 1.0, "{s}");
    assert_eq!(
        s["samples"][0]["irradiance"],
        serde_json::json!([0.0, 0.0, 0.0]),
        "{s}"
    );

    // A bad margin is a usage error, before any bake.
    let bad = regia(
        &dir,
        &[
            "bake-gi",
            "furnace",
            "--cpu",
            "--incremental",
            STATE,
            "--influence",
            "nan",
        ],
    );
    assert!(!bad.status.success());
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(stderr.contains("influence"), "{stderr}");
}
