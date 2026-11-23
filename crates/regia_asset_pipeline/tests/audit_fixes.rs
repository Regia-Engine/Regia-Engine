// SPDX-License-Identifier: MPL-2.0
//! Regression tests for the asset-pipeline audit (sprint 2026-10-21): nested node transforms
//! under `Forward::Convert`, normals of non-indexed primitives, sub-asset id collisions, external
//! animation buffers, the cache key of `.meta` settings.

use std::path::Path;

use regia_asset::{AssetId, MetaSidecar};
use regia_asset_pipeline::{
    CookOptions, Forward, TextureCookQuality, cook_project, import_gltf, import_gltf_glb_bytes,
};
use regia_core::math::{Mat4, Quat, Vec3};
use regia_test_utils::unique_temp_dir;
use serde_json::json;

const TRI: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];

fn le_f32(values: impl IntoIterator<Item = f32>) -> Vec<u8> {
    values.into_iter().flat_map(f32::to_le_bytes).collect()
}

/// Position bytes (36) then three u32 indices (12).
fn tri_bin(positions: &[[f32; 3]; 3]) -> Vec<u8> {
    let mut bin = le_f32(positions.iter().flatten().copied());
    for i in 0u32..3 {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    bin
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

/// Accessors 0 (positions) and 1 (indices) over a `tri_bin` buffer.
fn tri_accessors() -> serde_json::Value {
    json!([
        {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
         "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]},
        {"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}
    ])
}

fn tri_views() -> serde_json::Value {
    json!([
        {"buffer": 0, "byteOffset": 0, "byteLength": 36},
        {"buffer": 0, "byteOffset": 36, "byteLength": 12}
    ])
}

fn assert_same_points(got: &[[f32; 3]], want: &[Vec3], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: vertex count");
    for w in want {
        assert!(
            got.iter().any(|g| Vec3::from_array(*g).abs_diff_eq(*w, 1e-4)),
            "{what}: {w:?} not in {got:?}"
        );
    }
}

fn nested_glb(parent: serde_json::Value, child: serde_json::Value) -> Vec<u8> {
    let bin = tri_bin(&TRI);
    let mut parent = parent;
    parent["children"] = json!([1]);
    parent["name"] = json!("Empty");
    let mut child = child;
    child["mesh"] = json!(0);
    child["name"] = json!("Leaf");
    pack_glb(
        &json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [parent, child],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
            "accessors": tri_accessors(),
            "bufferViews": tri_views(),
            "buffers": [{"byteLength": bin.len()}]
        }),
        &bin,
    )
}

#[test]
fn nested_nodes_are_converted_once_and_compose_parent_first() {
    let yaw = Mat4::from_rotation_y(std::f32::consts::PI);
    // cos and sin of a 45 degree half-angle: a 90 degree yaw quaternion.
    let h = std::f64::consts::FRAC_1_SQRT_2;
    let cases = [
        // A mesh under an identity empty: the yaw must not cancel.
        (json!({}), json!({"translation": [0.0, 0.0, 2.0]})),
        // A rotated, translated parent over a translated child: parent * child, not child * parent.
        (
            json!({"translation": [1.0, 0.0, 0.0], "rotation": [0.0, h, 0.0, h]}),
            json!({"translation": [0.0, 0.0, 2.0]}),
        ),
    ];
    for (parent, child) in cases {
        let local = |n: &serde_json::Value| {
            let t = n.get("translation").map_or(Vec3::ZERO, |t| {
                Vec3::new(
                    t[0].as_f64().unwrap_or(0.0) as f32,
                    0.0,
                    t[2].as_f64().unwrap_or(0.0) as f32,
                )
            });
            let r = n.get("rotation").map_or(Quat::IDENTITY, |_| {
                Quat::from_rotation_y(std::f32::consts::FRAC_PI_2)
            });
            Mat4::from_rotation_translation(r, t)
        };
        let world = local(&parent) * local(&child);
        let glb = nested_glb(parent, child);
        for (forward, axis) in [(Forward::Keep, Mat4::IDENTITY), (Forward::Convert, yaw)] {
            let mesh = import_gltf(&glb, forward).expect("import");
            let want: Vec<Vec3> = TRI
                .iter()
                .map(|p| (axis * world).transform_point3(Vec3::from_array(*p)))
                .collect();
            assert_same_points(&mesh.positions, &want, &format!("{forward:?}"));
        }
    }
}

#[test]
fn non_indexed_primitive_without_normals_gets_face_normals() {
    // Facing +X: (0,0,0) -> (0,1,0) -> (0,0,1). No indices accessor, no NORMAL.
    let positions = [[0.0f32, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let bin = le_f32(positions.iter().flatten().copied());
    let glb = pack_glb(
        &json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
            "accessors": [{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
                           "min": [0.0, 0.0, 0.0], "max": [0.0, 1.0, 1.0]}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": 36}],
            "buffers": [{"byteLength": bin.len()}]
        }),
        &bin,
    );
    let mesh = import_gltf(&glb, Forward::Keep).expect("import");
    assert_eq!(mesh.normals.len(), 3);
    for n in &mesh.normals {
        assert!(
            (n[0] - 1.0).abs() < 1e-3 && n[1].abs() < 1e-3 && n[2].abs() < 1e-3,
            "{n:?}"
        );
    }
}

#[test]
fn duplicate_node_and_material_names_get_distinct_sub_assets() {
    let bin = tri_bin(&TRI);
    let prim = |material: usize| json!({"primitives": [{"attributes": {"POSITION": 0}, "indices": 1, "material": material}]});
    let glb = pack_glb(
        &json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0, 1, 2, 3]}],
            "nodes": [
                {"name": "Cube", "mesh": 0},
                {"name": "Cube", "mesh": 1, "translation": [3.0, 0.0, 0.0]},
                {"mesh": 0},
                {"mesh": 1}
            ],
            "meshes": [prim(0), prim(1)],
            "materials": [{"name": "M"}, {"name": "M"}],
            "accessors": tri_accessors(),
            "bufferViews": tri_views(),
            "buffers": [{"byteLength": bin.len()}]
        }),
        &bin,
    );
    let imported = import_gltf_glb_bytes(
        &glb,
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
    )
    .expect("import");
    let mut meshes: Vec<_> = imported
        .sub_assets
        .iter()
        .filter(|s| s.path.starts_with("mesh/"))
        .collect();
    assert_eq!(meshes.len(), 4, "one sub-asset per node primitive");
    meshes.sort_by_key(|s| s.id);
    meshes.dedup_by_key(|s| s.id);
    assert_eq!(meshes.len(), 4, "mesh ids are distinct");
    assert!(
        imported
            .sub_assets
            .iter()
            .any(|s| s.path == "mesh/Cube/primitive_0"),
        "the first keeps the plain path"
    );
    let mut materials: Vec<_> = imported
        .sub_assets
        .iter()
        .filter(|s| s.path.starts_with("material/"))
        .collect();
    assert_eq!(materials.len(), 2);
    materials.sort_by_key(|s| s.id);
    materials.dedup_by_key(|s| s.id);
    assert_eq!(materials.len(), 2, "material ids are distinct");
}

fn read_meta(source: &Path) -> MetaSidecar {
    let mut path = source.as_os_str().to_owned();
    path.push(".meta");
    let bytes = std::fs::read(Path::new(&path)).expect("meta");
    MetaSidecar::from_ron(&bytes).expect("meta ron")
}

#[test]
fn external_buffer_animation_cooks_with_one_asset_per_skin_and_clip() {
    let root = unique_temp_dir("audit-anim");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    // buffer: positions 0..36, indices 36..48, times 48..56, translations 56..80.
    let mut bin = tri_bin(&TRI);
    bin.extend(le_f32([0.0, 1.0]));
    bin.extend(le_f32([0.0, 0.0, 0.0, 1.0, 0.0, 0.0]));
    std::fs::write(assets.join("rig.bin"), &bin).expect("bin");
    let doc = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0, 1, 2]}],
        "nodes": [{"name": "Tri", "mesh": 0}, {"name": "Hips"}, {"name": "Hips"}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
        "skins": [{"joints": [1]}, {"joints": [2]}],
        "animations": [{
            "name": "walk",
            "samplers": [{"input": 2, "output": 3}],
            "channels": [
                {"sampler": 0, "target": {"node": 1, "path": "translation"}},
                {"sampler": 0, "target": {"node": 2, "path": "translation"}}
            ]
        }],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
             "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]},
            {"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"},
            {"bufferView": 2, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0]},
            {"bufferView": 3, "componentType": 5126, "count": 2, "type": "VEC3"}
        ],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 36},
            {"buffer": 0, "byteOffset": 36, "byteLength": 12},
            {"buffer": 0, "byteOffset": 48, "byteLength": 8},
            {"buffer": 0, "byteOffset": 56, "byteLength": 24}
        ],
        "buffers": [{"uri": "rig.bin", "byteLength": bin.len()}]
    });
    let source = assets.join("rig.gltf");
    std::fs::write(&source, doc.to_string()).expect("gltf");

    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let paths: Vec<String> = read_meta(&source)
        .sub_assets
        .into_iter()
        .map(|e| e.path)
        .collect();
    for want in ["skeleton/Hips", "skeleton/Hips~1", "clip/walk", "clip/walk~1"] {
        assert!(paths.iter().any(|p| p == want), "{want} missing from {paths:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn changing_meta_settings_recooks_the_gltf() {
    let root = unique_temp_dir("audit-settings");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    let bin = tri_bin(&TRI);
    let glb = pack_glb(
        &json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"name": "Tri", "mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
            "accessors": tri_accessors(),
            "bufferViews": tri_views(),
            "buffers": [{"byteLength": bin.len()}]
        }),
        &bin,
    );
    let source = assets.join("tri.glb");
    std::fs::write(&source, glb).expect("glb");

    let first = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert_eq!((first.cooked, first.skipped), (1, 0), "{first:?}");
    let again = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert_eq!(
        (again.cooked, again.skipped),
        (0, 1),
        "unchanged source is a cache hit"
    );

    let mut meta = read_meta(&source);
    meta.settings = Some("(generate_lods: (count: 3))".to_string());
    let mut meta_path = source.as_os_str().to_owned();
    meta_path.push(".meta");
    std::fs::write(Path::new(&meta_path), meta.to_ron_string().expect("meta ron")).expect("write meta");
    let changed = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert_eq!(
        (changed.cooked, changed.skipped),
        (1, 0),
        "new settings must recook: {changed:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
