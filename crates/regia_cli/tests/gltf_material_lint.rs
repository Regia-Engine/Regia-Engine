// SPDX-License-Identifier: MPL-2.0
//! C-05 CLI subprocess acceptance: exit status, JSON, recursion and repair.
use serde_json::{Value, json};
use std::process::Command;

#[test]
fn cli_checks_then_fixes_and_preserves_external_resources() {
    let root = regia_test_utils::unique_temp_dir("cli-material-lint");
    std::fs::create_dir_all(root.join("_preview")).expect("dir");
    let file = root.join("part.gltf");
    let source = json!({"asset":{"version":"2.0"},"buffers":[{"uri":"part.bin","byteLength":4}],
        "images":[{"uri":"color.png"}],"materials":[{"pbrMetallicRoughness":{
            "metallicFactor":0.0,"roughnessFactor":0.3,"metallicRoughnessTexture":{"index":0}}}]});
    std::fs::write(&file, serde_json::to_vec(&source).expect("json")).expect("source");
    std::fs::write(root.join("part.bin"), b"data").expect("buffer");
    std::fs::write(root.join("color.png"), b"image sentinel").expect("image");
    std::fs::write(root.join("_preview/broken.gltf"), b"invalid").expect("preview");
    let run = |fix: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_regia"));
        c.args(["assets", "lint-materials"]).arg(&root).args([
            "--profile",
            "compatibility",
            "--json",
            "--skip-staging",
        ]);
        if fix {
            c.arg("--fix");
        }
        c.output().expect("run")
    };
    let before = run(false);
    assert!(!before.status.success());
    let report: Value = serde_json::from_slice(&before.stdout).expect("JSON before");
    assert_eq!(report["files"].as_array().expect("files").len(), 1);
    assert_eq!(report["files"][0]["report"]["changed"], false);
    let fixed = run(true);
    assert!(
        fixed.status.success(),
        "{}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    let report: Value = serde_json::from_slice(&fixed.stdout).expect("JSON fix");
    assert_eq!(report["profile"], "compatibility");
    assert_eq!(report["files"][0]["report"]["changed"], true);
    assert_eq!(
        report["files"][0]["report"]["findings"][0]["effect"],
        "changes_appearance"
    );
    let fixed_doc: Value = serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("fixed json");
    assert_eq!(
        fixed_doc["materials"][0]["pbrMetallicRoughness"]["metallicFactor"],
        1.0
    );
    assert_eq!(
        fixed_doc["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"],
        1.0
    );
    assert!(run(false).status.success());
    assert_eq!(std::fs::read(root.join("part.bin")).expect("read"), b"data");
    assert_eq!(
        std::fs::read(root.join("color.png")).expect("read"),
        b"image sentinel"
    );
    assert_eq!(
        std::fs::read(root.join("_preview/broken.gltf")).expect("read"),
        b"invalid"
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn cli_reports_malformed_file_and_uses_configurable_name_policy() {
    let root = regia_test_utils::unique_temp_dir("cli-material-settings");
    std::fs::create_dir_all(&root).expect("dir");
    let file = root.join("part.gltf");
    std::fs::write(&file,br#"{"asset":{"version":"2.0"},"materials":[{"name":"Coated panel","pbrMetallicRoughness":{"metallicFactor":1.0,"roughnessFactor":0.3}}]}"#).expect("source");
    let settings = root.join("policy.json");
    std::fs::write(
        &settings,
        br#"{"name_rules":[{"contains":"coated","metallic":0.0}]}"#,
    )
    .expect("settings");
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["assets", "lint-materials"])
        .arg(&file)
        .arg("--settings")
        .arg(&settings)
        .args(["--fix", "--json"])
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let g: Value = serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("json");
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["metallicFactor"], 0.0);
    std::fs::write(&file, b"bad json").expect("invalid");
    let out = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["assets", "lint-materials"])
        .arg(&file)
        .args(["--fix", "--json"])
        .output()
        .expect("run");
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).expect("json error");
    assert_eq!(report["files"][0]["error"]["code"], "gltf_material_invalid");
    assert_eq!(std::fs::read(&file).expect("unchanged"), b"bad json");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn cli_default_profile_preserves_valid_textured_factors() {
    let root = regia_test_utils::unique_temp_dir("cli-material-preserve");
    std::fs::create_dir_all(&root).expect("dir");
    let file = root.join("part.gltf");
    let source = json!({"asset":{"version":"2.0"},"materials":[{"pbrMetallicRoughness":{
        "metallicRoughnessTexture":{"index":0},"metallicFactor":0.35,"roughnessFactor":0.7}}]});
    let bytes = serde_json::to_vec(&source).expect("json");
    std::fs::write(&file, &bytes).expect("source");
    let run = |extra: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_regia"));
        c.args(["assets", "lint-materials"]).arg(&file).args(["--json"]);
        c.args(extra);
        c.output().expect("run")
    };
    let checked = run(&[]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let report: Value = serde_json::from_slice(&checked.stdout).expect("JSON");
    assert_eq!(report["ok"], true);
    assert_eq!(report["profile"], "standard");
    assert!(
        report["policy"]
            .as_str()
            .expect("policy")
            .contains("preserves glTF")
    );
    assert_eq!(report["files"][0]["report"]["changed"], false);
    assert_eq!(report["files"][0]["report"]["findings"], json!([]));
    let fixed = run(&["--fix"]);
    assert!(
        fixed.status.success(),
        "{}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    let report: Value = serde_json::from_slice(&fixed.stdout).expect("JSON fix");
    assert_eq!(report["profile"], "standard");
    assert_eq!(report["files"][0]["report"]["changed"], false);
    assert_eq!(std::fs::read(&file).expect("bytes"), bytes);

    let settings = root.join("policy.json");
    std::fs::write(&settings, br#"{"profile":"standard"}"#).expect("settings");
    let overridden = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["assets", "lint-materials"])
        .arg(&file)
        .args(["--settings"])
        .arg(&settings)
        .args(["--profile", "compatibility", "--fix", "--json"])
        .output()
        .expect("override");
    assert!(
        overridden.status.success(),
        "{}",
        String::from_utf8_lossy(&overridden.stderr)
    );
    let report: Value = serde_json::from_slice(&overridden.stdout).expect("JSON override");
    assert_eq!(report["profile"], "compatibility");
    assert!(
        report["policy"]
            .as_str()
            .expect("policy")
            .contains("appearance-changing")
    );
    let fixed_doc: Value = serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("fixed json");
    assert_eq!(
        fixed_doc["materials"][0]["pbrMetallicRoughness"]["metallicFactor"],
        1.0
    );
    assert_eq!(
        fixed_doc["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"],
        1.0
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}
