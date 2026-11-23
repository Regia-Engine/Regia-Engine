// SPDX-License-Identifier: MPL-2.0
//! Frozen Stage A bit table (spec 121 § 13.2, § 14 rule 2, plan row W-05, ADR-8432).
//!
//! `tests/fixtures/stage_a_bits.ron` holds one hash per table: W-01's mesh triples, W-02's query
//! table, W-03's on-foot table and W-04's navmesh ids. This test recomputes the first three,
//! checks them against the file, and folds every word (plus `QUERY_BITS_VERSION`) into one hash
//! pinned below. It then builds the crate with and without `--features stage_b` (the feature is
//! empty today) and requires the same hash. A Stage B change that moves a word fails here and
//! must bump `QUERY_BITS_VERSION` in that same commit. Editing only the ron fails, because the
//! composite constant lives in this file.
//!
//! The child builds use a sibling of `CARGO_TARGET_DIR` so they do not take this test's target lock.
//! `REGIA_STAGE_A_CHILD=1` prints `STAGE_A_BITS_HASH` and returns.

#![allow(clippy::unwrap_used)]

#[path = "support/stage_a_tables.rs"]
mod tables;

use std::path::{Path, PathBuf};
use std::process::Command;

use regia_world_geom::QUERY_BITS_VERSION;
use serde::Deserialize;

/// FNV-1a 64 of `QUERY_BITS_VERSION` then the live words, recorded from the pinned rows.
const STAGE_A_COMPOSITE: u64 = 0x20bc_b349_c721_e180;
/// W-04's Switchbacks navmesh bake id (`det_tunnel_nav.rs`). A literal: this crate does not bake it.
const NAV_BAKE_ID: u64 = 0xbae1_74da_539e_bf71;
/// W-04's Switchbacks navmesh wire length.
const NAV_LEN: u64 = 65_181;
/// W-04's Switchbacks navmesh wire FNV-1a 64.
const NAV_FNV: u64 = 0xb9ab_f6c8_fdd9_5dc5;

#[derive(Debug, Deserialize)]
struct StageAFile {
    query_bits_version: u32,
    mesh: Vec<(String, String, (u64, u64, u64))>,
    stage_a: Vec<(String, String, u64)>,
    onfoot: Vec<(String, u64)>,
    nav_bake_id: u64,
    nav_len: u64,
    nav_fnv: u64,
}

fn fold(version: u32, words: &[u64]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    mix(&version.to_le_bytes());
    for w in words {
        mix(&w.to_le_bytes());
    }
    h
}

fn words_of(live: &tables::Live, nav: (u64, u64, u64)) -> Vec<u64> {
    let mut words = Vec::new();
    for row in &live.mesh {
        words.push(row.mesh);
        words.push(row.holes);
        words.push(row.queries);
    }
    for row in &live.stage_a {
        words.push(row.hash);
    }
    for row in &live.onfoot {
        words.push(row.hash);
    }
    words.push(nav.0);
    words.push(nav.1);
    words.push(nav.2);
    words
}

fn check_live(file: &StageAFile, live: &tables::Live) {
    assert_eq!(file.query_bits_version, QUERY_BITS_VERSION);
    assert_eq!(file.mesh.len(), live.mesh.len());
    let mut moved = Vec::new();
    for (pinned, row) in file.mesh.iter().zip(&live.mesh) {
        if pinned.0 != row.map || pinned.1 != row.terrain {
            moved.push(format!(
                "mesh order {}/{} vs {}/{}",
                row.map, row.terrain, pinned.0, pinned.1
            ));
        }
        for (part, got, want) in [
            ("mesh", row.mesh, pinned.2.0),
            ("holes", row.holes, pinned.2.1),
            ("queries", row.queries, pinned.2.2),
        ] {
            println!(
                "STAGE_A_MESH {}/{} {part} {got:#018x} pinned {want:#018x}",
                row.map, row.terrain
            );
            if got != want {
                moved.push(format!(
                    "{}/{} {part}: {got:#018x}, ron {want:#018x}",
                    row.map, row.terrain
                ));
            }
        }
    }
    assert_eq!(file.stage_a.len(), live.stage_a.len());
    for (pinned, row) in file.stage_a.iter().zip(&live.stage_a) {
        println!(
            "STAGE_A_QUERY {}/{} {:#018x} pinned {:#018x}",
            row.map, row.terrain, row.hash, pinned.2
        );
        if pinned.0 != row.map || pinned.1 != row.terrain || pinned.2 != row.hash {
            moved.push(format!(
                "stage_a {}/{}: {:#018x}, ron {}/{} {:#018x}",
                row.map, row.terrain, row.hash, pinned.0, pinned.1, pinned.2
            ));
        }
    }
    assert_eq!(file.onfoot.len(), live.onfoot.len());
    for (pinned, row) in file.onfoot.iter().zip(&live.onfoot) {
        println!(
            "STAGE_A_ONFOOT {} {:#018x} pinned {:#018x} met {:?}",
            row.name, row.hash, pinned.1, row.met
        );
        if !row.met.iter().all(|n| *n > 0) {
            moved.push(format!(
                "{}: the walk must meet a wall, a deck and a raised floor: {:?}",
                row.name, row.met
            ));
        }
        if pinned.0 != row.name || pinned.1 != row.hash {
            moved.push(format!(
                "onfoot {}: {:#018x}, ron {} {:#018x}",
                row.name, row.hash, pinned.0, pinned.1
            ));
        }
    }
    assert!(moved.is_empty(), "Stage A bits moved:\n{}", moved.join("\n"));
}

#[expect(
    clippy::disallowed_methods,
    reason = "test harness: the child build's target dir follows CARGO_TARGET_DIR, read on the host outside any sim tick (ADR-8367)"
)]
fn child_target(suffix: &str) -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    let mut name = base.file_name().unwrap_or_default().to_os_string();
    name.push("-");
    name.push(suffix);
    parent.join(name)
}

fn parse_hash(text: &str) -> Option<u64> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("STAGE_A_BITS_HASH ")
            && let Ok(v) = u64::from_str_radix(rest.trim().trim_start_matches("0x"), 16)
        {
            return Some(v);
        }
    }
    None
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(80);
    lines[start..].join("\n")
}

fn run_child(stage_b: bool) -> u64 {
    let suffix = if stage_b {
        "stage-a-bits-on"
    } else {
        "stage-a-bits-off"
    };
    let target = child_target(suffix);
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().unwrap().parent().unwrap().to_path_buf();
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(&workspace)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_BUILD_JOBS", "2")
        .env("REGIA_STAGE_A_CHILD", "1")
        .args(["test", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .args(["-p", "regia_world_geom", "--test", "stage_a_bits"]);
    if stage_b {
        cmd.args(["--features", "stage_b"]);
    }
    cmd.args([
        "--",
        "--exact",
        "stage_a_bits_are_the_same_with_and_without_stage_b",
        "--nocapture",
    ]);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("child cargo ({suffix}): {e}"));
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed = parse_hash(&text);
    let _ = std::fs::remove_dir_all(&target);
    assert!(
        out.status.success(),
        "stage_b={stage_b} child failed\n{}",
        tail(&text)
    );
    parsed.unwrap_or_else(|| panic!("no STAGE_A_BITS_HASH (stage_b={stage_b})\n{}", tail(&text)))
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: REGIA_STAGE_A_CHILD marks the child build's run, read on the host outside any sim tick (ADR-8367)"
)]
fn stage_a_bits_are_the_same_with_and_without_stage_b() {
    let file: StageAFile = ron::from_str(include_str!("fixtures/stage_a_bits.ron"))
        .unwrap_or_else(|e| panic!("stage_a_bits.ron: {e}"));
    assert_eq!(file.nav_bake_id, NAV_BAKE_ID, "ron nav bake id");
    assert_eq!(file.nav_len, NAV_LEN, "ron nav wire length");
    assert_eq!(file.nav_fnv, NAV_FNV, "ron nav fnv");
    let live = tables::live();
    check_live(&file, &live);
    let hash = fold(
        QUERY_BITS_VERSION,
        &words_of(&live, (file.nav_bake_id, file.nav_len, file.nav_fnv)),
    );
    println!("STAGE_A_BITS_HASH {hash:#x}");
    assert_eq!(
        hash, STAGE_A_COMPOSITE,
        "composite of the live words moved; the ron and this constant pin the same table"
    );
    if std::env::var_os("REGIA_STAGE_A_CHILD").is_some() {
        return;
    }
    let off = run_child(false);
    let on = run_child(true);
    assert_eq!(off, hash, "build without stage_b");
    assert_eq!(on, hash, "build with stage_b");
}
