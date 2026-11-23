// SPDX-License-Identifier: MPL-2.0
//! Q-06 through the existing registered agent handlers; no window or GPU is needed.

use image::ImageEncoder;
use regia_ecs::prelude::*;
use regia_editor::agent_ext::EditorProject;
use serde_json::{Value, json};

fn call(world: &mut World, method: &str, params: Value) -> Value {
    let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
    let (_, handler) = extension.method(method).expect("registered method");
    handler(world, &params).expect("agent call")
}

#[test]
fn agent_reimport_exposes_sidecar_identity_and_cooked_lod_group() {
    let root = regia_test_utils::unique_temp_dir("q06-agent-cook");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let mut bin = Vec::new();
    for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        bin.extend_from_slice(&value.to_le_bytes());
    }
    regia_test_utils::write_atomic(&assets.join("mesh.bin"), &bin).expect("buffer");
    let json = json!({
        "asset": {"version": "2.0"}, "scene": 0, "scenes": [{"nodes": [0, 1]}],
        "nodes": [{"name": "Prop_LOD0", "mesh": 0}, {"name": "Prop_LOD1", "mesh": 0}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
        "accessors": [{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0,0,0], "max": [1,1,0]}],
        "bufferViews": [{"buffer": 0, "byteLength": 36}],
        "buffers": [{"byteLength": 36, "uri": "mesh.bin"}],
        "images": [{"uri": "paint.png"}]
    });
    regia_test_utils::write_atomic(
        &assets.join("prop.gltf"),
        &serde_json::to_vec(&json).expect("json"),
    )
    .expect("gltf");
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&[10, 20, 30, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("png");
    regia_test_utils::write_atomic(&assets.join("paint.png"), &png).expect("image");
    regia_test_utils::write_atomic(
        &root.join("lod_classes.toml"),
        b"version = 1\n[classes]\nprop = [1.6, 0.4]\n",
    )
    .expect("classes");
    let mut world = World::new();
    let mut extension = regia_agent::extension::AgentExtension::new();
    for (schema, handler) in regia_editor::agent_ext::all_methods() {
        extension.register(schema, handler);
    }
    world.insert_resource(extension);
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    let before = call(
        &mut world,
        "editor.asset_provenance",
        json!({"asset": "assets/paint.png"}),
    );
    assert_eq!(before["meta"], false);
    let imported = call(&mut world, "editor.reimport", json!({"path": "assets/prop.gltf"}));
    assert_eq!(imported["errors"], json!([]), "{imported}");
    let after = call(
        &mut world,
        "editor.asset_provenance",
        json!({"asset": "assets/paint.png"}),
    );
    assert_eq!(after["meta"], true);
    assert_eq!(after["importer"], "png");
    assert!(after["id"].as_str().is_some_and(|id| !id.is_empty()));
    let imported = call(&mut world, "editor.reimport", json!({"path": "assets/prop.gltf"}));
    assert_eq!(imported["errors"], json!([]), "{imported}");
    assert_eq!(
        call(
            &mut world,
            "editor.asset_provenance",
            json!({"asset": "assets/paint.png"})
        )["id"],
        after["id"]
    );
    let meta =
        regia_asset::MetaSidecar::from_ron(&std::fs::read(assets.join("prop.gltf.meta")).expect("meta"))
            .expect("meta ron");
    let id = regia_asset::AssetId::sub_asset(meta.id, "lod_group/Prop");
    let cache = regia_asset::cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
    let manifest = regia_asset::AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.id == id && entry.path == "assets/prop.gltf#lod_group/Prop")
    );
    let _ = std::fs::remove_dir_all(root);
}
