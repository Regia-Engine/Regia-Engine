// SPDX-License-Identifier: MPL-2.0
//! Source-file safety and GLB metadata-only repair (C-05).
use regia_asset_pipeline::gltf_material_lint::{MaterialLintSettings, lint_material_file};
use serde_json::{Value, json};

fn glb(g: &Value, tail: &[u8]) -> Vec<u8> {
    let mut json = serde_json::to_vec(g).expect("encode");
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut bytes = b"glTF".to_vec();
    bytes.extend(2_u32.to_le_bytes());
    bytes.extend(((20 + json.len() + tail.len()) as u32).to_le_bytes());
    bytes.extend((json.len() as u32).to_le_bytes());
    bytes.extend(b"JSON");
    bytes.extend(json);
    bytes.extend(tail);
    bytes
}

fn source() -> Value {
    json!({"asset":{"version":"2.0"},"materials":[{"emissiveFactor":[3.0,1.5,0.0]}],
        "accessors":[{"opaque":"preserve"}],"buffers":[{"byteLength":4}],"extras":{"custom":true}})
}

#[test]
fn glb_fix_preserves_binary_and_unknown_chunks_exactly() {
    let root = regia_test_utils::unique_temp_dir("material-glb");
    std::fs::create_dir_all(&root).expect("dir");
    let path = root.join("source.glb");
    let tail = [
        4, 0, 0, 0, b'B', b'I', b'N', 0, 19, 81, 0, 255, 4, 0, 0, 0, b'T', b'E', b'S', b'T', 11, 22, 33, 44,
    ];
    let before = glb(&source(), &tail);
    std::fs::write(&path, &before).expect("write");
    let settings = MaterialLintSettings::default();
    let check = lint_material_file(&path, &settings, false).expect("check");
    assert!(!check.findings.is_empty());
    assert_eq!(std::fs::read(&path).expect("read"), before);
    assert!(lint_material_file(&path, &settings, true).expect("fix").changed);
    let bytes = std::fs::read(&path).expect("read");
    let total = u32::from_le_bytes(bytes[8..12].try_into().expect("length")) as usize;
    let n = u32::from_le_bytes(bytes[12..16].try_into().expect("chunk")) as usize;
    assert_eq!(total, bytes.len());
    assert_eq!(&bytes[20 + n..], &tail);
    let fixed: Value = serde_json::from_slice(&bytes[20..20 + n]).expect("json");
    assert_eq!(fixed["accessors"], source()["accessors"]);
    assert_eq!(fixed["extras"], source()["extras"]);
    assert_eq!(fixed["materials"][0]["emissiveFactor"], json!([1.0, 0.5, 0.0]));
    assert!(!lint_material_file(&path, &settings, true).expect("again").changed);
    assert_eq!(std::fs::read(&path).expect("read"), bytes);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn malformed_glb_never_replaces_the_source() {
    let root = regia_test_utils::unique_temp_dir("material-bad-glb");
    std::fs::create_dir_all(&root).expect("dir");
    let path = root.join("source.glb");
    let valid = glb(&source(), &[]);
    let mut wrong_version = valid.clone();
    wrong_version[4] = 1;
    let mut bad_length = valid.clone();
    bad_length[8] = 0;
    let mut bad_chunk = valid.clone();
    bad_chunk[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut wrong_type = valid.clone();
    wrong_type[16] = b'X';
    let duplicate_json = glb(
        &source(),
        &[4, 0, 0, 0, b'J', b'S', b'O', b'N', b'{', b'}', b' ', b' '],
    );
    let truncated_tail = glb(&source(), &[4, 0, 0, 0, b'B', b'I', b'N', 0]);
    for bytes in [
        vec![],
        b"glTF".to_vec(),
        wrong_version,
        bad_length,
        bad_chunk,
        wrong_type,
        duplicate_json,
        truncated_tail,
    ] {
        std::fs::write(&path, &bytes).expect("write");
        let e = lint_material_file(&path, &MaterialLintSettings::default(), true).expect_err("bad file");
        assert_eq!(e.code, "gltf_material_invalid");
        assert_eq!(std::fs::read(&path).expect("read"), bytes);
    }
    assert_eq!(
        std::fs::read_dir(&root).expect("dir").count(),
        1,
        "no staging debris"
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn clean_json_keeps_exact_whitespace_and_external_files_need_not_exist() {
    let root = regia_test_utils::unique_temp_dir("material-clean-file");
    std::fs::create_dir_all(&root).expect("dir");
    let path = root.join("source.gltf");
    let bytes=br#"{ "asset":{"version":"2.0"}, "buffers":[{"uri":"missing.bin","byteLength":4}], "materials":[{"pbrMetallicRoughness":{"metallicFactor":0.0,"roughnessFactor":1.0}}] }"#;
    std::fs::write(&path, bytes).expect("write");
    assert!(
        !lint_material_file(&path, &MaterialLintSettings::default(), true)
            .expect("clean")
            .changed
    );
    assert_eq!(std::fs::read(&path).expect("read"), bytes);
    assert_eq!(std::fs::read_dir(&root).expect("dir").count(), 1);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn unreadable_and_unsupported_sources_return_errors() {
    let root = regia_test_utils::unique_temp_dir("material-file-errors");
    std::fs::create_dir_all(&root).expect("dir");
    let s = MaterialLintSettings::default();
    assert_eq!(
        lint_material_file(&root.join("missing.gltf"), &s, false)
            .expect_err("missing")
            .code,
        "gltf_material_io"
    );
    let path = root.join("source.txt");
    std::fs::write(&path, b"sentinel").expect("write");
    assert_eq!(
        lint_material_file(&path, &s, true).expect_err("suffix").code,
        "gltf_material_invalid"
    );
    assert_eq!(std::fs::read(&path).expect("read"), b"sentinel");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn oversized_source_is_refused_without_rewriting() {
    let root = regia_test_utils::unique_temp_dir("material-file-size");
    std::fs::create_dir_all(&root).expect("dir");
    let path = root.join("large.gltf");
    let f = std::fs::File::create(&path).expect("file");
    f.set_len(64 * 1024 * 1024 + 1).expect("sparse size");
    drop(f);
    let e = lint_material_file(&path, &MaterialLintSettings::default(), true).expect_err("bounded");
    // ADR-8226: a resource refusal has the common limit code; retain the exact
    // unchanged-source assertion below.
    assert_eq!(e.code, "content_lint_limit");
    assert_eq!(
        std::fs::metadata(&path).expect("metadata").len(),
        64 * 1024 * 1024 + 1
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn fix_retains_permissions_and_refuses_symlink_sources() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = regia_test_utils::unique_temp_dir("material-file-permissions");
    std::fs::create_dir_all(&root).expect("dir");
    let path = root.join("source.gltf");
    let bytes = serde_json::to_vec(&source()).expect("json");
    std::fs::write(&path, &bytes).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("permissions");
    let link = root.join("link.gltf");
    symlink(&path, &link).expect("link");
    assert_eq!(
        lint_material_file(&link, &MaterialLintSettings::default(), true)
            .expect_err("symlink")
            .code,
        "gltf_material_io"
    );
    assert_eq!(std::fs::read(&path).expect("read"), bytes);
    lint_material_file(&path, &MaterialLintSettings::default(), true).expect("fix");
    assert_eq!(
        std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o640
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}
