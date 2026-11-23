// SPDX-License-Identifier: MPL-2.0
//! `regia export usda` through the real binary (ADR-4604).

use std::process::{Command, Output};

use serde_json::json;

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .output()
        .expect("run regia")
}

fn pack_glb(json_text: &[u8], bin: &[u8]) -> Vec<u8> {
    let mut json_bytes = json_text.to_vec();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let mut bin_bytes = bin.to_vec();
    while !bin_bytes.is_empty() && !bin_bytes.len().is_multiple_of(4) {
        bin_bytes.push(0);
    }
    let mut total = 12 + 8 + json_bytes.len();
    if !bin_bytes.is_empty() {
        total += 8 + bin_bytes.len();
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&u32::try_from(total).expect("glb fits").to_le_bytes());
    out.extend_from_slice(&u32::try_from(json_bytes.len()).expect("json fits").to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_bytes);
    if !bin_bytes.is_empty() {
        out.extend_from_slice(&u32::try_from(bin_bytes.len()).expect("bin fits").to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin_bytes);
    }
    out
}

/// One triangle, indices 0, 2, 1, positions (0,0,0) (1,0,0) (0,1,0).
fn triangle_glb() -> Vec<u8> {
    let mut bin = Vec::new();
    for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        bin.extend_from_slice(&value.to_le_bytes());
    }
    let index_at = bin.len();
    for index in [0u16, 2, 1] {
        bin.extend_from_slice(&index.to_le_bytes());
    }
    let json = serde_json::to_vec(&json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Tri", "mesh": 0}],
        "meshes": [{"primitives": [{
            "attributes": {"POSITION": 0},
            "indices": 1,
            "mode": 4
        }]}],
        "accessors": [
            {
                "bufferView": 0,
                "componentType": 5126,
                "count": 3,
                "type": "VEC3",
                "min": [0.0, 0.0, 0.0],
                "max": [1.0, 1.0, 0.0]
            },
            {"bufferView": 1, "componentType": 5123, "count": 3, "type": "SCALAR"}
        ],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": index_at},
            {"buffer": 0, "byteOffset": index_at, "byteLength": 6}
        ],
        "buffers": [{"byteLength": bin.len()}]
    }))
    .expect("json");
    pack_glb(&json, &bin)
}

fn empty_glb() -> Vec<u8> {
    let json = serde_json::to_vec(&json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Empty"}]
    }))
    .expect("json");
    pack_glb(&json, &[])
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn export_usda_writes_the_triangle_and_lists_in_help() {
    let dir = regia_test_utils::unique_temp_dir("usda-cli");
    let glb = dir.join("tri.glb");
    let out = dir.join("tri.usda");
    std::fs::write(&glb, triangle_glb()).expect("glb");
    let ran = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["export", "usda", "--glb"])
        .arg(&glb)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run");
    assert!(ran.status.success(), "{}", stderr(&ran));
    let text = std::fs::read_to_string(&out).expect("usda");
    assert!(
        text.contains("point3f[] points = [(0, 0, 0), (1, 0, 0), (0, 1, 0)]"),
        "{text}"
    );
    assert!(text.contains("int[] faceVertexCounts = [3]"), "{text}");
    assert!(text.contains("int[] faceVertexIndices = [0, 2, 1]"), "{text}");
    assert!(!text.contains("normal3f[] normals"), "this glb has no normals");
    assert!(
        !text.contains("format_version"),
        "an exchange file has no format version"
    );

    let help = regia(&["export", "--help"]);
    assert!(help.status.success(), "{}", stderr(&help));
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("usda"), "{help_text}");

    let usda_help = regia(&["export", "usda", "--help"]);
    assert!(usda_help.status.success(), "{}", stderr(&usda_help));
    assert!(String::from_utf8_lossy(&usda_help.stdout).contains("--glb"));

    let glb_help = regia(&["export", "glb", "--help"]);
    assert!(glb_help.status.success(), "{}", stderr(&glb_help));
    assert!(String::from_utf8_lossy(&glb_help.stdout).contains("--level"));
}

#[test]
fn export_usda_refusals_come_from_the_binary() {
    let dir = regia_test_utils::unique_temp_dir("usda-cli-err");
    let out = dir.join("out.usda");
    let missing = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["export", "usda", "--glb"])
        .arg(dir.join("missing.glb"))
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run");
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("usda_glb_missing"),
        "{}",
        stderr(&missing)
    );
    assert!(!out.exists());

    let bad = dir.join("bad.glb");
    std::fs::write(&bad, b"hello").expect("bad");
    let invalid = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["export", "usda", "--glb"])
        .arg(&bad)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run");
    assert!(
        stderr(&invalid).contains("usda_glb_invalid"),
        "{}",
        stderr(&invalid)
    );
    assert!(!out.exists());

    let glb = dir.join("empty.glb");
    std::fs::write(&glb, empty_glb()).expect("empty");
    std::fs::write(&out, b"keep").expect("seed");
    let empty = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["export", "usda", "--glb"])
        .arg(&glb)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run");
    assert!(stderr(&empty).contains("usda_empty"), "{}", stderr(&empty));
    assert_eq!(std::fs::read(&out).expect("kept"), b"keep");

    let blocked = dir.join("not-dir");
    std::fs::write(&blocked, b"x").expect("blocker");
    std::fs::write(dir.join("ok.glb"), triangle_glb()).expect("ok");
    let write = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["export", "usda", "--glb"])
        .arg(dir.join("ok.glb"))
        .arg("--out")
        .arg(blocked.join("stage.usda"))
        .output()
        .expect("run");
    assert!(stderr(&write).contains("usda_write"), "{}", stderr(&write));
    assert!(std::fs::read_dir(&dir).expect("dir").all(|entry| {
        !entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .contains(".tmp")
    }));
}

#[test]
fn top_level_help_still_parses() {
    let help = regia(&["-h"]);
    assert!(help.status.success(), "{}", stderr(&help));
    assert!(String::from_utf8_lossy(&help.stdout).contains("export"));
}

/// `export glb` still reaches its own command after `usda` is attached.
#[test]
fn export_glb_still_runs_for_a_missing_level() {
    let dir = regia_test_utils::unique_temp_dir("usda-glb-still");
    let level = dir.join("missing-level.level.ron");
    let out = dir.join("out.glb");
    let ran = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["export", "glb", "--level"])
        .arg(&level)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run");
    assert_eq!(ran.status.code(), Some(1), "{}", stderr(&ran));
    let err = stderr(&ran);
    assert!(err.contains("glb_read_failed"), "{err}");
    assert!(err.contains("missing-level.level.ron"), "{err}");
    assert!(!err.contains("usda_"), "{err}");
}
