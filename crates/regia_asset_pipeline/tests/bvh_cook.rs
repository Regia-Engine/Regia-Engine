// SPDX-License-Identifier: MPL-2.0
//! BVH cook end to end (ADR-2532): a `.bvh` in a project cooks to a skeleton plus a clip sub-asset,
//! and a malformed file is a cook error that names the documented code.

use std::path::{Path, PathBuf};

use regia_asset::{AssetManifest, cooked_cache_dir};
use regia_asset_pipeline::bvh_import::FIXTURE_BVH;
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};

fn write_bvh(root: &Path, text: &str) -> PathBuf {
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let path = assets.join("walk.bvh");
    std::fs::write(&path, text).expect("bvh");
    path
}

#[test]
fn a_bvh_cooks_to_a_skeleton_and_a_clip() {
    let root = regia_test_utils::unique_temp_dir("bvh-cook");
    write_bvh(&root, FIXTURE_BVH);
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = cooked_cache_dir(&root, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    assert!(manifest.id_for_path("assets/walk.bvh").is_some());
    assert!(manifest.id_for_path("assets/walk.bvh#clip/walk").is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_malformed_bvh_is_a_cook_error_with_its_code() {
    let root = regia_test_utils::unique_temp_dir("bvh-cook-bad");
    write_bvh(&root, "HIERARCHY\nROOT A\n{\nOFFSET 0 0\n}\n");
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    let text = format!("{:?}", report.errors);
    assert!(text.contains("BVH line 4"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The cook compresses clips (spec 17 § 1, ADR-7227): the cooked clip is a `cooked-clip` v3 payload
/// with a compressed body, and it decodes within the spec tolerances of the clip the importer built.
#[test]
fn the_cooked_clip_is_compressed_within_the_spec_tolerances() {
    use regia_animation::AnimationClip;
    use regia_animation::compress::{CompressionSettings, measure_compression_error};
    use regia_asset_pipeline::bvh_import::{BvhImportSettings, BvhLimits, import_bvh, parse_bvh};

    let root = regia_test_utils::unique_temp_dir("bvh-cook-compressed");
    write_bvh(&root, FIXTURE_BVH);
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = cooked_cache_dir(&root, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    let id = manifest
        .id_for_path("assets/walk.bvh#clip/walk")
        .expect("clip id");
    let bytes = std::fs::read(cache.join(regia_asset::cooked_file_name(id))).expect("cooked clip");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("cooked file");
    assert_eq!(AnimationClip::cooked_payload_version(payload), 3);
    let compressed =
        AnimationClip::compressed_cooked_payload(payload).expect("the cook writes a compressed body");

    let file = parse_bvh(FIXTURE_BVH.as_bytes(), &BvhLimits::default()).expect("parse");
    let import = import_bvh(&file, "walk", &BvhImportSettings::default()).expect("import");
    let settings = CompressionSettings::default();
    let error = measure_compression_error(&import.clip, &compressed, Some(&import.skeleton));
    assert!(error.within(&settings, 5e-5), "{error:?}");
    let _ = std::fs::remove_dir_all(&root);
}
