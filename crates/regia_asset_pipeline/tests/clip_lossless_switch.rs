// SPDX-License-Identifier: MPL-2.0
//! The per-asset lossless clip switch (ADR-7819, `docs/specs/17-animation.md` § 1): a source whose
//! `.meta` settings say `lossless_clips: true` cooks every clip it produces as a `Raw` `cooked-clip`
//! v3 body, bit-exact to the clip its importer decoded; an unflagged source still compresses; an
//! older (v1) `.meta` migrates with the switch off; flipping the switch recooks the clip; a folder
//! rule in `config/import_rules.ron` can set it.

use std::path::{Path, PathBuf};

use regia_animation::AnimationClip;
use regia_asset::{AssetManifest, cooked_cache_dir};
use regia_asset_pipeline::bvh_import::{BvhImportSettings, FIXTURE_BVH, convert_bvh};
use regia_asset_pipeline::gltf_anim::{GltfAnimSettings, import_gltf_animation};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};
use serde_json::json;

const BVH_ID: &str = "0b5c1d2e-3f40-4a51-8b62-7c83d94e0f15";
const GLB_ID: &str = "1c6d2e3f-4051-4b62-8c73-8d94ea5f1026";

fn project(label: &str) -> (PathBuf, PathBuf) {
    let root = regia_test_utils::unique_temp_dir(label);
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    (root, assets)
}

fn write_meta(source: &Path, version: u32, id: &str, importer: &str, settings: Option<&str>) {
    let settings = settings.map_or_else(String::new, |s| format!("    settings: {s:?},\n"));
    let text = format!(
        "(\n    format_version: {version},\n    id: \"{id}\",\n    importer: \"{importer}\",\n{settings})\n"
    );
    std::fs::write(regia_asset::meta_path_for(source), text).expect("meta");
}

/// The cooked payload of `sub` (`assets/<file>#clip/<name>`) after a clean cook of `root`.
fn cook_and_read(root: &Path, sub: &str) -> Vec<u8> {
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    read_cooked(root, sub)
}

fn read_cooked(root: &Path, sub: &str) -> Vec<u8> {
    let cache = cooked_cache_dir(root, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    let id = manifest
        .id_for_path(sub)
        .unwrap_or_else(|| panic!("{sub} is cooked"));
    let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked clip");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("cooked file");
    payload.to_vec()
}

/// Bit-exact: the decoded clip serializes to the same bytes as the source clip (every `f32` by its
/// bit pattern, every key, every name).
fn assert_bit_exact(payload: &[u8], source: &AnimationClip) {
    assert_eq!(AnimationClip::cooked_payload_version(payload), 3);
    assert!(
        AnimationClip::compressed_cooked_payload(payload).is_none(),
        "a lossless clip has a Raw body"
    );
    assert_eq!(
        payload,
        source.to_cooked_payload().expect("payload").as_slice(),
        "the cooked bytes are the source clip's lossless payload"
    );
    let decoded = AnimationClip::from_cooked_payload(payload).expect("decodes");
    assert_eq!(
        postcard::to_allocvec(&decoded).expect("postcard"),
        postcard::to_allocvec(source).expect("postcard"),
        "the decoded clip is bit-exact to its decoded source"
    );
}

fn assert_compressed(payload: &[u8], source: &AnimationClip) {
    assert_eq!(AnimationClip::cooked_payload_version(payload), 3);
    assert!(
        AnimationClip::compressed_cooked_payload(payload).is_some(),
        "an unflagged clip still compresses"
    );
    let decoded = AnimationClip::from_cooked_payload(payload).expect("decodes");
    assert_ne!(
        postcard::to_allocvec(&decoded).expect("postcard"),
        postcard::to_allocvec(source).expect("postcard"),
        "the compressed clip is not bit-exact (else this test could not tell the paths apart)"
    );
}

fn bvh_source_clip() -> AnimationClip {
    convert_bvh(FIXTURE_BVH.as_bytes(), "walk", &BvhImportSettings::default())
        .expect("import")
        .clip
}

#[test]
fn a_lossless_bvh_clip_cooks_bit_exact_to_its_decoded_source() {
    let (root, assets) = project("clip-lossless-bvh");
    let source = assets.join("walk.bvh");
    std::fs::write(&source, FIXTURE_BVH).expect("bvh");
    write_meta(&source, 2, BVH_ID, "bvh", Some("(lossless_clips: true)"));
    let payload = cook_and_read(&root, "assets/walk.bvh#clip/walk");
    assert_bit_exact(&payload, &bvh_source_clip());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unflagged_bvh_clip_still_compresses() {
    let (root, assets) = project("clip-lossy-bvh");
    let source = assets.join("walk.bvh");
    std::fs::write(&source, FIXTURE_BVH).expect("bvh");
    write_meta(&source, 2, BVH_ID, "bvh", Some("(lossless_clips: false)"));
    let payload = cook_and_read(&root, "assets/walk.bvh#clip/walk");
    assert_compressed(&payload, &bvh_source_clip());
    let _ = std::fs::remove_dir_all(&root);
}

/// A v1 `.meta` that named the key (the v1 BVH importer refused it as an unknown field) migrates
/// with the switch off: the clip compresses, and the cook's rewrite of the sidecar is v2 without it.
#[test]
fn an_old_bvh_meta_migrates_with_the_switch_off() {
    let (root, assets) = project("clip-v1-bvh");
    let source = assets.join("walk.bvh");
    std::fs::write(&source, FIXTURE_BVH).expect("bvh");
    write_meta(
        &source,
        1,
        BVH_ID,
        "bvh",
        Some("(lossless_clips: true, end_sites: false)"),
    );
    let payload = cook_and_read(&root, "assets/walk.bvh#clip/walk");
    assert_compressed(&payload, &bvh_source_clip());
    let meta = regia_asset::MetaSidecar::from_ron(
        &std::fs::read(regia_asset::meta_path_for(&source)).expect("meta"),
    )
    .expect("parses");
    assert_eq!(meta.format_version, 2);
    assert!(
        !meta
            .settings
            .as_deref()
            .unwrap_or_default()
            .contains("lossless_clips"),
        "{:?}",
        meta.settings
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn le_f32(values: impl IntoIterator<Item = f32>) -> Vec<u8> {
    values.into_iter().flat_map(f32::to_le_bytes).collect()
}

fn pack_glb(json: &serde_json::Value, bin: &[u8]) -> Vec<u8> {
    let mut json = json.to_string().into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut glb = b"glTF".to_vec();
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(bin);
    glb
}

/// A three-joint walking rig (`Root`, `FootL`, `FootR`) with one 31-key `walk` animation: smooth
/// sine curves, which the compressing cook reduces and quantizes.
fn rig_glb() -> Vec<u8> {
    let keys = 31usize;
    let times: Vec<f32> = (0..keys).map(|k| k as f32 / 30.0).collect();
    let root: Vec<f32> = times.iter().flat_map(|&t| [0.0, 0.0, 1.5 * t]).collect();
    let foot = |side: f32| -> Vec<f32> {
        times
            .iter()
            .flat_map(|&t| {
                let s = side * (std::f32::consts::TAU * t).sin();
                [-0.1 * side, 0.05 * s.max(0.0), 0.3 * s]
            })
            .collect()
    };
    let mut bin = le_f32([0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    for i in 0u32..3 {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    bin.extend(le_f32(times.iter().copied()));
    bin.extend(le_f32(root));
    bin.extend(le_f32(foot(1.0)));
    bin.extend(le_f32(foot(-1.0)));
    let t_len = keys * 4;
    let v_len = keys * 12;
    let doc = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0, 3]}],
        "nodes": [
            {"name": "Root", "children": [1, 2]},
            {"name": "FootL", "translation": [-0.1, 0.0, 0.0]},
            {"name": "FootR", "translation": [0.1, 0.0, 0.0]},
            {"name": "Tri", "mesh": 0}
        ],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
        "skins": [{"joints": [0, 1, 2]}],
        "animations": [{
            "name": "walk",
            "samplers": [
                {"input": 2, "output": 3},
                {"input": 2, "output": 4},
                {"input": 2, "output": 5}
            ],
            "channels": [
                {"sampler": 0, "target": {"node": 0, "path": "translation"}},
                {"sampler": 1, "target": {"node": 1, "path": "translation"}},
                {"sampler": 2, "target": {"node": 2, "path": "translation"}}
            ]
        }],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
             "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]},
            {"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"},
            {"bufferView": 2, "componentType": 5126, "count": keys, "type": "SCALAR",
             "min": [0.0], "max": [times[keys - 1]]},
            {"bufferView": 3, "componentType": 5126, "count": keys, "type": "VEC3"},
            {"bufferView": 4, "componentType": 5126, "count": keys, "type": "VEC3"},
            {"bufferView": 5, "componentType": 5126, "count": keys, "type": "VEC3"}
        ],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 36},
            {"buffer": 0, "byteOffset": 36, "byteLength": 12},
            {"buffer": 0, "byteOffset": 48, "byteLength": t_len},
            {"buffer": 0, "byteOffset": 48 + t_len, "byteLength": v_len},
            {"buffer": 0, "byteOffset": 48 + t_len + v_len, "byteLength": v_len},
            {"buffer": 0, "byteOffset": 48 + t_len + 2 * v_len, "byteLength": v_len}
        ],
        "buffers": [{"byteLength": bin.len()}]
    });
    pack_glb(&doc, &bin)
}

fn gltf_source_clip(glb: &[u8]) -> AnimationClip {
    let import = import_gltf_animation(glb, &GltfAnimSettings::default()).expect("import");
    import.skins[0]
        .clips
        .iter()
        .find(|c| c.name == "walk")
        .cloned()
        .expect("walk clip")
}

#[test]
fn a_lossless_gltf_clip_cooks_bit_exact_and_an_unflagged_one_compresses() {
    let glb = rig_glb();
    let source_clip = gltf_source_clip(&glb);

    let (root, assets) = project("clip-lossless-gltf");
    let source = assets.join("rig.glb");
    std::fs::write(&source, &glb).expect("glb");
    write_meta(&source, 2, GLB_ID, "gltf", Some("(lossless_clips: true)"));
    let payload = cook_and_read(&root, "assets/rig.glb#clip/walk");
    assert_bit_exact(&payload, &source_clip);
    let _ = std::fs::remove_dir_all(&root);

    let (root, assets) = project("clip-lossy-gltf");
    let source = assets.join("rig.glb");
    std::fs::write(&source, &glb).expect("glb");
    let payload = cook_and_read(&root, "assets/rig.glb#clip/walk");
    assert_compressed(&payload, &source_clip);
    let _ = std::fs::remove_dir_all(&root);
}

/// A v1 glTF `.meta` that named the key (the v1 glTF importer ignored it with an unknown-key
/// warning) keeps compressing after the migration.
#[test]
fn an_old_gltf_meta_migrates_with_the_switch_off() {
    let glb = rig_glb();
    let (root, assets) = project("clip-v1-gltf");
    let source = assets.join("rig.glb");
    std::fs::write(&source, &glb).expect("glb");
    write_meta(&source, 1, GLB_ID, "gltf", Some("(lossless_clips: true)"));
    let payload = cook_and_read(&root, "assets/rig.glb#clip/walk");
    assert_compressed(&payload, &gltf_source_clip(&glb));
    let _ = std::fs::remove_dir_all(&root);
}

/// Flipping the switch on a cooked project recooks the clip on the next incremental cook (the
/// cook's cache key covers it), and flipping it back compresses again.
#[test]
fn flipping_the_switch_recooks_the_clip() {
    let glb = rig_glb();
    let source_clip = gltf_source_clip(&glb);
    let (root, assets) = project("clip-flip-gltf");
    let source = assets.join("rig.glb");
    std::fs::write(&source, &glb).expect("glb");
    write_meta(&source, 2, GLB_ID, "gltf", None);
    assert_compressed(&cook_and_read(&root, "assets/rig.glb#clip/walk"), &source_clip);

    let mut meta = regia_asset::MetaSidecar::from_ron(
        &std::fs::read(regia_asset::meta_path_for(&source)).expect("meta"),
    )
    .expect("parses");
    meta.settings = Some("(lossless_clips: true)".into());
    std::fs::write(
        regia_asset::meta_path_for(&source),
        meta.to_ron_string().expect("ron"),
    )
    .expect("meta");
    assert_bit_exact(&cook_and_read(&root, "assets/rig.glb#clip/walk"), &source_clip);

    meta.settings = None;
    std::fs::write(
        regia_asset::meta_path_for(&source),
        meta.to_ron_string().expect("ron"),
    )
    .expect("meta");
    assert_compressed(&cook_and_read(&root, "assets/rig.glb#clip/walk"), &source_clip);
    let _ = std::fs::remove_dir_all(&root);
}

/// A folder rule sets the switch for every clip source under it (ADR-3321 rules, ADR-7819 key), and
/// `import_rules::check_project` knows the field for the clip importers.
#[test]
fn an_import_rule_can_keep_a_folder_lossless() {
    let (root, assets) = project("clip-rule-bvh");
    let source = assets.join("walk.bvh");
    std::fs::write(&source, FIXTURE_BVH).expect("bvh");
    std::fs::create_dir_all(root.join("config")).expect("config");
    std::fs::write(
        root.join("config/import_rules.ron"),
        "(format_version: 1, rules: [(glob: \"assets/**\", importer: \"bvh\", settings: \"(lossless_clips: true)\")])",
    )
    .expect("rules");
    let payload = cook_and_read(&root, "assets/walk.bvh#clip/walk");
    assert_bit_exact(&payload, &bvh_source_clip());
    let check = regia_asset_pipeline::import_rules::check_project(&root, None);
    assert!(check.config_error.is_none(), "{:?}", check.config_error);
    assert!(
        check.findings.is_empty(),
        "lossless_clips is a known field: {:?}",
        check.findings
    );
    for importer in ["gltf", "fbx", "bvh"] {
        let known = regia_asset_pipeline::import_rules::known_fields(importer).expect("rule-aware");
        assert!(known.contains(&"lossless_clips"), "{importer}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
