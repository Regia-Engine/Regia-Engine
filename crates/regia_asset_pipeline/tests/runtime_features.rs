// SPDX-License-Identifier: MPL-2.0
//! ADR-7972: the optional importers seen through the project cook, in every feature set.
//!
//! The default build runs these with every importer built in; `cargo test -p regia_asset_pipeline
//! --no-default-features --test runtime_features` runs the player-build side, where a disabled
//! importer must be the source's cook error (`importer_disabled`), never a panic and never a
//! silent skip, while the glTF next to it still cooks.

use regia_asset_pipeline::audio_transcode::{AUDIO_TRANSCODE_DISABLED_CODE, transcode_to_vorbis};
use regia_asset_pipeline::fbx_import::TRIANGLE_FBX;
use regia_asset_pipeline::generated_mesh_export::generated_mesh_glb;
use regia_asset_pipeline::{CookOptions, IMPORTER_DISABLED_CODE, cook_project};

/// A one-triangle GLB.
fn triangle_glb() -> Vec<u8> {
    let triangle = regia_procgen::GeneratedMesh {
        material: String::new(),
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 3],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
        indices: vec![0, 1, 2],
        colors: vec![],
        collision: false,
    };
    generated_mesh_glb("triangle", &triangle).expect("the triangle exports")
}

#[test]
fn a_disabled_importer_is_the_sources_cook_error_and_the_gltf_still_cooks() {
    let root = regia_test_utils::unique_temp_dir("runtime-features-cook");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    std::fs::write(assets.join("triangle.glb"), triangle_glb()).expect("glb");
    std::fs::write(assets.join("triangle.fbx"), TRIANGLE_FBX).expect("fbx");
    std::fs::write(
        assets.join("square.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="red"/></svg>"#,
    )
    .expect("svg");
    std::fs::write(assets.join("empty.usda"), "#usda 1.0\n").expect("usda");

    let report = cook_project(CookOptions::for_host(root.clone())).expect("the cook runs");
    let error_of = |name: &str| {
        report
            .errors
            .iter()
            .find(|(path, _)| path.ends_with(name))
            .map(|(_, e)| e.clone())
    };
    assert!(
        error_of("triangle.glb").is_none(),
        "the glTF cooks in every build: {:?}",
        report.errors
    );
    assert!(report.cooked >= 1, "{report:?}");
    for (name, feature, on) in [
        ("triangle.fbx", "fbx_import", cfg!(feature = "fbx_import")),
        ("square.svg", "svg_import", cfg!(feature = "svg_import")),
        ("empty.usda", "usd_import", cfg!(feature = "usd_import")),
    ] {
        let error = error_of(name);
        if on {
            // Built in: whatever the importer says about this input, it is never "disabled".
            if let Some(e) = &error {
                assert!(!e.contains(IMPORTER_DISABLED_CODE), "{name}: {e}");
            }
        } else {
            let e = error.unwrap_or_else(|| {
                panic!(
                    "{name}: a disabled importer must be reported, not skipped: {:?}",
                    report.errors
                )
            });
            assert!(e.contains(IMPORTER_DISABLED_CODE), "{name}: {e}");
            assert!(e.contains(feature), "{name}: {e}");
        }
    }
    if cfg!(feature = "fbx_import") {
        assert!(error_of("triangle.fbx").is_none(), "{:?}", report.errors);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_vorbis_transcode_answers_its_disabled_code_only_without_audio_transcode() {
    let result = transcode_to_vorbis(b"not audio", "sfx/bad.wav", 0.4);
    let message = result.expect_err("these bytes never decode, and nothing encodes without the feature");
    if cfg!(feature = "audio_transcode") {
        assert!(!message.starts_with(AUDIO_TRANSCODE_DISABLED_CODE), "{message}");
    } else {
        assert!(message.starts_with(AUDIO_TRANSCODE_DISABLED_CODE), "{message}");
        assert!(message.contains("audio_transcode"), "{message}");
    }
}
