// SPDX-License-Identifier: MPL-2.0
//! `spawn_gltf_hierarchy` imports glTF morph targets (Tincal engine items B1/C2): a generated GLB
//! with one two-primitive mesh (two materials, so two part entities) and one morph target, named
//! through `extras.targetNames`, with a default weight. Each part gets its own sparse targets over
//! its own vertices, the file's default weights and the target names.

use std::path::PathBuf;

use regia_animation::{MorphTargetNames, MorphWeights};
use regia_editor::gltf_hierarchy::{GltfSpawnOptions, spawn_gltf_hierarchy};
use regia_render::{MeshMorphTargets, MeshRenderer};
use serde_json::json;

fn f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn glb(json: &serde_json::Value, mut bin: Vec<u8>) -> Vec<u8> {
    let mut json_bytes = serde_json::to_vec(json).expect("json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend(b"glTF");
    out.extend(2u32.to_le_bytes());
    out.extend((total as u32).to_le_bytes());
    out.extend((json_bytes.len() as u32).to_le_bytes());
    out.extend(b"JSON");
    out.extend(&json_bytes);
    out.extend((bin.len() as u32).to_le_bytes());
    out.extend(b"BIN\0");
    out.extend(&bin);
    out
}

/// Two triangles (materials 0 and 1) in one mesh; target "lift" moves the first triangle's
/// apex by (0, 0, 0.5) and the second triangle's first vertex by (0, 0.25, 0).
fn fixture() -> PathBuf {
    let tri_a: [f32; 9] = [-1.0, 0.0, 0.0, 0.0, 0.0, 0.0, -0.5, 1.0, 0.0];
    let tri_b: [f32; 9] = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.5, 1.0, 0.0];
    let lift_a: [f32; 9] = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5];
    let lift_b: [f32; 9] = [0.0, 0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let mut bin = Vec::new();
    for part in [tri_a, tri_b, lift_a, lift_b] {
        bin.extend(f32s(&part));
    }
    let views: Vec<_> = (0..4)
        .map(|i| json!({ "buffer": 0, "byteOffset": i * 36, "byteLength": 36 }))
        .collect();
    let accessor = |view: usize, min: [f32; 3], max: [f32; 3]| json!({ "bufferView": view, "componentType": 5126, "count": 3, "type": "VEC3", "min": min, "max": max });
    let json = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "name": "Face", "mesh": 0 }],
        "meshes": [{
            "primitives": [
                { "attributes": { "POSITION": 0 }, "material": 0, "targets": [{ "POSITION": 2 }] },
                { "attributes": { "POSITION": 1 }, "material": 1, "targets": [{ "POSITION": 3 }] }
            ],
            "weights": [0.25],
            "extras": { "targetNames": ["lift"] }
        }],
        "materials": [
            { "name": "Skin", "pbrMetallicRoughness": { "baseColorFactor": [0.9, 0.7, 0.6, 1.0] } },
            { "name": "Lips", "pbrMetallicRoughness": { "baseColorFactor": [0.7, 0.2, 0.2, 1.0] } }
        ],
        "buffers": [{ "byteLength": bin.len() }],
        "bufferViews": views,
        "accessors": [
            accessor(0, [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            accessor(1, [0.0, 0.0, 0.0], [1.0, 1.0, 0.0]),
            accessor(2, [0.0, 0.0, 0.0], [0.0, 0.0, 0.5]),
            accessor(3, [0.0, 0.0, 0.0], [0.0, 0.25, 0.0])
        ]
    });
    let dir = std::env::temp_dir().join(format!("regia-gltf-morph-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("face.glb");
    std::fs::write(&path, glb(&json, bin)).expect("write");
    path
}

#[test]
fn each_part_gets_its_sparse_targets_default_weights_and_names() {
    let path = fixture();
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    let spawned = spawn_gltf_hierarchy(world, &path, &GltfSpawnOptions::default()).expect("spawn");
    assert!(
        spawned.report.warnings.is_empty(),
        "{:?}",
        spawned.report.warnings
    );
    let mut query = world.query::<(&MeshRenderer, &MeshMorphTargets, &MorphWeights, &MorphTargetNames)>();
    let parts: Vec<_> = query.iter(world).collect();
    assert_eq!(parts.len(), 2, "one morphed entity per material part");
    let mut moved = Vec::new();
    for (renderer, targets, weights, names) in parts {
        assert_eq!(targets.targets.len(), 1);
        assert_eq!(targets.targets.names(), ["lift".to_string()]);
        assert_eq!(names.0.as_ref(), ["lift".to_string()]);
        assert_eq!(weights.0, vec![0.25], "the file's default weight");
        assert_eq!(
            targets.targets.vertex_count() as usize,
            renderer.mesh.vertices.len()
        );
        assert_eq!(
            targets.targets.moved_vertex_count(),
            1,
            "sparse: one vertex per part"
        );
        let mut vertices = renderer.mesh.vertices.clone();
        targets.targets.apply(&[1.0], &mut vertices);
        for (after, before) in vertices.iter().zip(&renderer.mesh.vertices) {
            let d: Vec<f32> = (0..3).map(|k| after.position[k] - before.position[k]).collect();
            if d.iter().any(|v| *v != 0.0) {
                moved.push(d);
            }
        }
    }
    moved.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    assert_eq!(moved, vec![vec![0.0, 0.0, 0.5], vec![0.0, 0.25, 0.0]]);
}
