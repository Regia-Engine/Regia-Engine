// SPDX-License-Identifier: MPL-2.0
//! CT-03: source geometry and material findings on the validation CLI and the cook/package gate.
use std::path::{Path, PathBuf};
use std::process::Command;

use regia_asset::registry::{AssetRegistry, RegistryEntry};
use regia_asset_pipeline::source_lint_cache::{
    PublishRequest, ScanRequest, open_cook_registry, run_publish_gate, scan,
};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

fn regia() -> Command {
    Command::new(env!("CARGO_BIN_EXE_regia"))
}

fn write_bytes(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

fn duplicate_gltf(project: &Path) -> PathBuf {
    let props = project.join("assets/props");
    std::fs::create_dir_all(&props).expect("props");
    let mut bin = Vec::new();
    for value in [0.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        bin.extend(value.to_le_bytes());
    }
    write_bytes(&props.join("dup.bin"), &bin);
    let doc = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0}],
        "buffers": [{"uri": "dup.bin", "byteLength": bin.len()}],
        "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": bin.len()}],
        "accessors": [{
            "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
            "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]
        }],
        "meshes": [
            {"primitives": [{"attributes": {"POSITION": 0}, "mode": 4}]},
            {"primitives": [{"attributes": {"POSITION": 0}, "mode": 4}]}
        ]
    });
    let path = props.join("dup.gltf");
    write_bytes(&path, &serde_json::to_vec(&doc).expect("json"));
    path
}

fn material_gltf(project: &Path) -> PathBuf {
    let props = project.join("assets/props");
    std::fs::create_dir_all(&props).expect("props");
    let mut bin = Vec::new();
    for value in [0.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        bin.extend(value.to_le_bytes());
    }
    write_bytes(&props.join("mat.bin"), &bin);
    let doc = json!({
        "asset": {"version": "2.0"},
        "materials": [{"name": "plastic", "pbrMetallicRoughness": {}}],
        "buffers": [{"uri": "mat.bin", "byteLength": bin.len()}],
        "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": bin.len()}],
        "accessors": [{
            "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
            "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]
        }],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "mode": 4, "material": 0}]}]
    });
    let path = props.join("mat.gltf");
    write_bytes(&path, &serde_json::to_vec(&doc).expect("json"));
    path
}

fn clean_registry(project: &Path) {
    let entry = RegistryEntry {
        id: regia_asset::AssetId(regia_core::id::Guid::from_u128(1)),
        path: "assets/levels/main.scene".to_owned(),
        type_path: "regia_scene::Scene".to_owned(),
        dependencies: Vec::new(),
        tags: std::collections::BTreeMap::new(),
        size: 1,
    };
    let registry = AssetRegistry::from_entries([entry]);
    let cache = regia_asset::cooked_cache_dir(project, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    registry
        .write_to(&regia_asset::registry::cache_registry_path(&cache))
        .expect("registry");
}

fn json_stdout(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON object ({error}): {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn source_geometry_fails_strict_cli_and_package_before_publication() {
    let root = unique_temp_dir("content-lint-geometry");
    let gltf = duplicate_gltf(&root);
    clean_registry(&root);
    let validate = regia()
        .args([
            "assets",
            "validate",
            "--strict",
            "--json",
            "--platform",
            "linux",
            "--project",
        ])
        .arg(&root)
        .output()
        .expect("validate");
    assert!(
        !validate.status.success(),
        "strict validate must fail: {}",
        String::from_utf8_lossy(&validate.stderr)
    );
    let report = json_stdout(&validate);
    assert_eq!(report["exit_failed"], true);
    let ids = report["source_lint"]["rule_ids"].as_array().expect("rule ids");
    assert!(
        ids.iter().any(|id| id == "gltf_duplicate_mesh"),
        "rule ids: {ids:?}"
    );

    let geometry = regia()
        .args(["assets", "lint-geometry", "--json", "--strict", "--project"])
        .arg(&root)
        .arg(&gltf)
        .output()
        .expect("lint-geometry");
    assert!(
        !geometry.status.success(),
        "{}",
        String::from_utf8_lossy(&geometry.stderr)
    );
    let geometry_report = json_stdout(&geometry);
    assert!(
        geometry_report["rule_ids"]
            .as_array()
            .expect("ids")
            .iter()
            .any(|id| id == "gltf_duplicate_mesh")
    );

    let pak = root.join("build/paks/linux/base.fpak");
    let mut published = false;
    let blocked = run_publish_gate(
        &PublishRequest {
            project: root.clone(),
            target: "default".to_owned(),
            package: true,
        },
        || {
            published = true;
            if let Some(parent) = pak.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(&pak, b"pak").map_err(|error| error.to_string())?;
            Ok::<(), String>(())
        },
    );
    assert!(blocked.is_err(), "package gate must stop on the geometry finding");
    assert!(!published, "publish callback ran");
    assert!(!pak.exists(), "base.fpak was written");
    let message = blocked.expect_err("blocked");
    assert!(!message.to_ascii_lowercase().contains("skipped"), "{message}");
}

#[test]
fn material_policy_opt_in_matches_cli_and_cook_rule_ids() {
    let root = unique_temp_dir("content-lint-material");
    let gltf = material_gltf(&root);
    let cli = regia()
        .args(["assets", "lint-materials", "--profile", "compatibility", "--json"])
        .arg(&gltf)
        .output()
        .expect("lint-materials");
    let cli_report = json_stdout(&cli);
    let mut cli_ids: Vec<String> = cli_report["files"][0]["report"]["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter_map(|finding| finding["code"].as_str().map(str::to_owned))
        .collect();
    cli_ids.sort();
    cli_ids.dedup();
    assert!(cli_ids.iter().any(|id| id == "factor_missing"), "{cli_ids:?}");

    let cooked = scan(&ScanRequest {
        project: root.clone(),
        target: "default".to_owned(),
        paths: vec![gltf],
        changed: None,
        strict: false,
        fix: false,
        cancel_file: None,
        report_path: None,
        material_profile: None,
        skip_staging: false,
    })
    .expect("scan");
    let mut cook_ids: Vec<String> = cooked
        .rule_ids
        .into_iter()
        .filter(|id| !id.starts_with("gltf_"))
        .collect();
    cook_ids.sort();
    assert_eq!(cli_ids, cook_ids);
}

#[test]
fn malformed_required_registry_is_not_a_skipped_success() {
    let root = unique_temp_dir("content-lint-registry");
    let garbage = root.join("garbage.registry.bin");
    write_bytes(&garbage, b"this is not a registry");
    let malformed = open_cook_registry(&garbage, false).expect_err("malformed");
    assert!(!malformed.to_ascii_lowercase().contains("skipped"), "{malformed}");
    let missing = root.join("missing.registry.bin");
    let required = open_cook_registry(&missing, true).expect_err("required");
    assert!(!required.to_ascii_lowercase().contains("skipped"), "{required}");
    assert!(
        open_cook_registry(&missing, false)
            .expect("optional missing")
            .is_none()
    );

    write_bytes(&root.join("config/source_lint.ron"), b"not a policy");
    clean_registry(&root);
    let validate = regia()
        .args([
            "assets",
            "validate",
            "--strict",
            "--json",
            "--platform",
            "linux",
            "--project",
        ])
        .arg(&root)
        .output()
        .expect("validate");
    assert!(!validate.status.success(), "invalid policy must not succeed");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&validate.stdout),
        String::from_utf8_lossy(&validate.stderr)
    );
    assert!(!combined.to_ascii_lowercase().contains("skipped"), "{combined}");
}

#[test]
fn threshold_change_invalidates_evidence_without_another_source_parse() {
    let root = unique_temp_dir("content-lint-cache");
    let gltf = duplicate_gltf(&root);
    let run = || {
        regia()
            .args(["assets", "lint-geometry", "--json", "--project"])
            .arg(&root)
            .arg(&gltf)
            .output()
            .expect("lint-geometry")
    };
    let first = json_stdout(&run());
    assert!(first["counters"]["source_parses"].as_u64().unwrap_or(0) >= 1);
    assert!(first["counters"]["decoded_bytes"].as_u64().unwrap_or(0) > 0);
    let first_key = first["evidence_key"].as_str().expect("key").to_owned();

    let second = json_stdout(&run());
    assert_eq!(second["counters"]["source_parses"], 0);
    assert_eq!(second["counters"]["decoded_bytes"], 0);
    assert!(second["counters"]["cache_hits"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(second["evidence_key"], first_key);

    write_bytes(
        &root.join("config/source_lint.ron"),
        br#"(
            format_version: 1,
            defaults: (geometry: Some((quantization: Some(0.01)))),
        )"#,
    );
    let third = json_stdout(&run());
    assert_ne!(third["evidence_key"].as_str(), Some(first_key.as_str()));
    assert_eq!(third["counters"]["source_parses"], 0);
    assert_eq!(third["counters"]["decoded_bytes"], 0);
}

#[test]
fn interrupted_scan_returns_one_json_error() {
    let root = unique_temp_dir("content-lint-cancel");
    let gltf = duplicate_gltf(&root);
    let flag = root.join("cancel.flag");
    write_bytes(&flag, b"1");
    let output = regia()
        .args(["assets", "lint-geometry", "--json", "--project"])
        .arg(&root)
        .arg("--cancel-file")
        .arg(&flag)
        .arg(&gltf)
        .output()
        .expect("lint-geometry");
    assert!(!output.status.success());
    let report = json_stdout(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(report["error"]["code"], "content_lint_cancelled");
    assert!(report["report"].is_null());
}
