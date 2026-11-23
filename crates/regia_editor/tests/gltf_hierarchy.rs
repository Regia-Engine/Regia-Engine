// SPDX-License-Identifier: MPL-2.0
//! `spawn_gltf_hierarchy` (Tincal engine request F1): a generated GLB with a three-level node
//! chain, a rigid mesh, an empty `socket_*` node and a two-joint skin is spawned as a named
//! entity hierarchy; names, parents, local transforms, the skin binding, the name ↔ entity
//! round trip, the axis convention and a real render through the editor pane are checked.

use std::path::{Path, PathBuf};

use regia_animation::SkinnedMesh;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_editor::gltf_hierarchy::{GltfEditorOnly, GltfSpawnOptions, spawn_gltf_hierarchy};
use regia_editor::render::{PaneRequest, render_pane};
use regia_editor::viewport::EditorCamera;
use regia_editor::views::ViewKind;
use regia_render::{MeshRenderer, SkinJoints};
use regia_scene::prelude::{GlobalTransform, Name, Transform};
use serde_json::json;

fn f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The fixture as GLB bytes.
///
/// ```text
/// Base ─ Turret (y 1, yaw 90°) ─ Barrel (z 1, red triangle) ─ socket_muzzle (z 0.5)
///      └ Rig (x 3) ─ Hip (y 0.5) ─ Hand (y 1)         Skinny (identity, blue triangle, skin 0)
/// ```
fn fixture_glb() -> Vec<u8> {
    fixture_glb_with_skinny_at(Vec3::ZERO)
}

/// The fixture with the skinned node `Skinny` translated by `offset`; its inverse binds are
/// `joint⁻¹ · skinny_world`, as exporters write them, so the bind pose lands at `offset + v`.
fn fixture_glb_with_skinny_at(offset: Vec3) -> Vec<u8> {
    let barrel: [f32; 9] = [-0.4, -0.4, 0.0, 0.4, -0.4, 0.0, 0.0, 0.4, 0.0];
    // Bind-pose positions in scene space around the Hip (3, 0.5, 0) and Hand (3, 1.5, 0).
    let skinned: [f32; 9] = [2.6, 0.5, 0.0, 3.4, 0.5, 0.0, 3.0, 1.3, 0.0];
    let joints: Vec<u8> = vec![0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0];
    let weights = f32s(&[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
    let skinny_world = Mat4::from_translation(offset);
    let hip_ibm = Mat4::from_translation(Vec3::new(-3.0, -0.5, 0.0)) * skinny_world;
    let hand_ibm = Mat4::from_translation(Vec3::new(-3.0, -1.5, 0.0)) * skinny_world;
    let mut bin = Vec::new();
    bin.extend(f32s(&barrel));
    bin.extend(f32s(&skinned));
    bin.extend(&joints);
    bin.extend(&weights);
    bin.extend(f32s(&hip_ibm.to_cols_array()));
    bin.extend(f32s(&hand_ibm.to_cols_array()));
    let half = std::f32::consts::FRAC_PI_4;
    let json = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0, 4, 8] }],
        "nodes": [
            { "name": "Base", "children": [1] },
            { "name": "Turret", "translation": [0.0, 1.0, 0.0], "rotation": [0.0, half.sin(), 0.0, half.cos()], "children": [2] },
            { "name": "Barrel", "translation": [0.0, 0.0, 1.0], "mesh": 0, "children": [3] },
            { "name": "socket_muzzle", "translation": [0.0, 0.0, 0.5] },
            { "name": "Rig", "translation": [3.0, 0.0, 0.0], "children": [5] },
            { "name": "Hip", "translation": [0.0, 0.5, 0.0], "children": [6] },
            { "name": "Hand", "translation": [0.0, 1.0, 0.0] },
            { "name": "Unused" },
            { "name": "Skinny", "translation": offset.to_array(), "mesh": 1, "skin": 0 }
        ],
        "meshes": [
            { "primitives": [{ "attributes": { "POSITION": 0 }, "material": 0 }] },
            { "primitives": [{ "attributes": { "POSITION": 1, "JOINTS_0": 2, "WEIGHTS_0": 3 }, "material": 1 }] }
        ],
        "materials": [
            { "name": "RedMetal", "doubleSided": true, "pbrMetallicRoughness": { "baseColorFactor": [0.9, 0.05, 0.05, 1.0], "metallicFactor": 0.0 } },
            { "name": "BlueCloth", "doubleSided": true, "pbrMetallicRoughness": { "baseColorFactor": [0.1, 0.3, 0.95, 1.0], "metallicFactor": 0.0 } }
        ],
        "skins": [{ "joints": [5, 6], "inverseBindMatrices": 4 }],
        "buffers": [{ "byteLength": bin.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 36, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 72, "byteLength": 12 },
            { "buffer": 0, "byteOffset": 84, "byteLength": 48 },
            { "buffer": 0, "byteOffset": 132, "byteLength": 128 }
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [-0.4, -0.4, 0.0], "max": [0.4, 0.4, 0.0] },
            { "bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3", "min": [2.6, 0.5, 0.0], "max": [3.4, 1.3, 0.0] },
            { "bufferView": 2, "componentType": 5121, "count": 3, "type": "VEC4" },
            { "bufferView": 3, "componentType": 5126, "count": 3, "type": "VEC4" },
            { "bufferView": 4, "componentType": 5126, "count": 2, "type": "MAT4" }
        ]
    });
    let mut json_bytes = serde_json::to_vec(&json).expect("fixture json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut glb = Vec::with_capacity(total);
    glb.extend(b"glTF");
    glb.extend(2u32.to_le_bytes());
    glb.extend((total as u32).to_le_bytes());
    glb.extend((json_bytes.len() as u32).to_le_bytes());
    glb.extend(b"JSON");
    glb.extend(&json_bytes);
    glb.extend((bin.len() as u32).to_le_bytes());
    glb.extend(b"BIN\0");
    glb.extend(&bin);
    glb
}

fn write_fixture(name: &str) -> PathBuf {
    write_fixture_bytes(name, &fixture_glb())
}

fn write_fixture_bytes(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-gltf-hierarchy-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

fn new_world() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut().insert_resource(EditorCamera::default());
    app
}

fn name_of(world: &World, entity: Entity) -> String {
    world
        .get::<Name>(entity)
        .map(|n| n.as_str().to_string())
        .unwrap_or_default()
}

fn spawn(world: &mut World, path: &Path, opts: &GltfSpawnOptions) -> regia_editor::gltf_hierarchy::GltfSpawn {
    spawn_gltf_hierarchy(world, path, opts).expect("spawn")
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn names_parents_transforms_and_lookups_match_the_file() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = write_fixture("hierarchy.glb");
    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn(world, &path, &GltfSpawnOptions::default());

    let entity = |name: &str| spawned.entity(name).unwrap_or_else(|| panic!("node {name}"));
    assert_eq!(name_of(world, spawned.root), "hierarchy");

    // Parents.
    let parent = |name: &str| world.get::<ChildOf>(entity(name)).map(|c| c.parent());
    assert_eq!(parent("Base"), Some(spawned.root));
    assert_eq!(parent("Turret"), Some(entity("Base")));
    assert_eq!(parent("Barrel"), Some(entity("Turret")));
    assert_eq!(parent("socket_muzzle"), Some(entity("Barrel")));
    assert_eq!(parent("Hip"), Some(entity("Rig")));
    assert_eq!(parent("Hand"), Some(entity("Hip")));
    assert_eq!(
        spawned.node("Barrel").and_then(|n| n.parent),
        Some(entity("Turret"))
    );

    // Local transforms are the node's own, not baked.
    let turret = world
        .get::<Transform>(entity("Turret"))
        .expect("turret transform");
    assert!((turret.translation - Vec3::Y).length() < 1e-5);
    let expected = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    assert!(turret.rotation.angle_between(expected) < 1e-4);
    let barrel = world
        .get::<Transform>(entity("Barrel"))
        .expect("barrel transform");
    assert!((barrel.translation - Vec3::Z).length() < 1e-5);

    // Propagated world pose: yaw 90° turns the barrel's local +Z onto world +X.
    let barrel_world = world
        .get::<GlobalTransform>(entity("Barrel"))
        .expect("global")
        .translation();
    assert!(
        (barrel_world - Vec3::new(1.0, 1.0, 0.0)).length() < 1e-4,
        "{barrel_world:?}"
    );
    let muzzle_world = world
        .get::<GlobalTransform>(entity("socket_muzzle"))
        .expect("global")
        .translation();
    assert!(
        (muzzle_world - Vec3::new(1.5, 1.0, 0.0)).length() < 1e-4,
        "{muzzle_world:?}"
    );

    // The socket and the plain joints are empty entities.
    let muzzle = spawned.node("socket_muzzle").expect("socket");
    assert!(muzzle.is_socket && muzzle.triangles == 0);
    assert!(world.get::<MeshRenderer>(entity("socket_muzzle")).is_none());
    assert!(world.get::<MeshRenderer>(entity("Base")).is_none());

    // Meshes and materials.
    let barrel_node = spawned.node("Barrel").expect("barrel node");
    assert_eq!(barrel_node.triangles, 1);
    assert_eq!(barrel_node.materials, vec!["RedMetal".to_string()]);
    assert!(world.get::<MeshRenderer>(entity("Barrel")).is_some());

    // Round trip: name → entity → node → name.
    for node in &spawned.nodes {
        assert_eq!(spawned.name_of(node.entity), Some(node.name.as_str()));
        assert_eq!(
            spawned.entity(&node.name),
            spawned.name_map().get(&node.name).copied()
        );
    }
    // `Unused` is not in the scene, so it is not spawned.
    assert_eq!(spawned.nodes.len(), 8);
    assert_eq!(spawned.report.nodes, 8);
    assert_eq!(spawned.report.sockets, 1);
    assert_eq!(spawned.report.triangles, 2);
    assert_eq!(spawned.report.skeletons, 1);
    assert_eq!(spawned.report.joints, 2);
    assert!(spawned.report.duplicate_names.is_empty(), "{:?}", spawned.report);
    assert!(
        spawned.report.warnings.is_empty(),
        "{:?}",
        spawned.report.warnings
    );
}

#[test]
fn skin_binds_to_the_joint_node_entities_and_stays_in_the_gltf_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = write_fixture("skin.glb");
    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn(world, &path, &GltfSpawnOptions::default());
    let skinny = spawned.entity("Skinny").expect("skinned node");
    let hip = spawned.entity("Hip").expect("hip");
    let hand = spawned.entity("Hand").expect("hand");

    let binding = world.get::<SkinnedMesh>(skinny).expect("skeleton binding");
    assert_eq!(binding.joints.len(), 2);
    assert!(binding.joints.contains(&hip) && binding.joints.contains(&hand));
    for (id, joint) in binding.joints.iter().enumerate() {
        assert_eq!(
            binding.skeleton.joint_names[id],
            name_of(world, *joint),
            "joint id {id} is the node it is named after"
        );
    }
    assert_eq!(spawned.skins.len(), 1);
    assert_eq!(spawned.node("Skinny").and_then(|n| n.skin), Some(0));

    // Skinned vertices at bind pose land on the raw (unconverted) glTF positions: the same
    // frame the rigid path uses, so no 180° yaw between the two.
    let renderer = world.get::<MeshRenderer>(skinny).expect("skinned mesh");
    let palette = &world.get::<SkinJoints>(skinny).expect("palette").palette;
    let expected: [[f32; 3]; 3] = [[2.6, 0.5, 0.0], [3.4, 0.5, 0.0], [3.0, 1.3, 0.0]];
    for (vertex, expected) in renderer.mesh.vertices.iter().zip(expected) {
        let joint = vertex.joints[0] as usize;
        let skinned = palette[joint].transform_point(vertex.position);
        let error = Vec3::from(skinned) - Vec3::from(expected);
        assert!(
            error.length() < 1e-4,
            "bind pose moved {skinned:?} vs {expected:?}"
        );
    }
    // Weights survive the import (the lab path zeroes them).
    assert!(renderer.mesh.vertices.iter().all(|v| v.weights[0] > 0.99));
    assert_ne!(
        renderer.mesh.vertices[2].joints[0],
        renderer.mesh.vertices[0].joints[0]
    );
}

/// The renderer multiplies a skinned mesh's palette by the entity's model matrix, so a skinned
/// node away from the origin must not have its own transform inside the palette as well.
#[test]
fn a_skinned_node_away_from_the_origin_binds_in_the_right_place() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let offset = Vec3::new(0.0, 0.0, 4.0);
    let path = write_fixture_bytes("skin_offset.glb", &fixture_glb_with_skinny_at(offset));
    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn(world, &path, &GltfSpawnOptions::default());
    app.update();
    let world = app.world_mut();
    let skinny = spawned.entity("Skinny").expect("skinned node");
    let model = Mat4::from(world.get::<GlobalTransform>(skinny).expect("global").0);
    assert!(
        (model.w_axis.truncate() - offset).length() < 1e-5,
        "model sits at the node"
    );
    let renderer = world.get::<MeshRenderer>(skinny).expect("skinned mesh");
    let palette = &world.get::<SkinJoints>(skinny).expect("palette").palette;
    let local: [[f32; 3]; 3] = [[2.6, 0.5, 0.0], [3.4, 0.5, 0.0], [3.0, 1.3, 0.0]];
    for (vertex, local) in renderer.mesh.vertices.iter().zip(local) {
        let joint = vertex.joints[0] as usize;
        let skinned = Vec3::from(palette[joint].transform_point(vertex.position));
        let on_screen = model.transform_point3(skinned);
        let expected = Vec3::from(local) + offset;
        assert!(
            (on_screen - expected).length() < 1e-4,
            "bind pose drawn at {on_screen:?}, expected {expected:?}"
        );
    }
}

#[test]
fn options_drop_sockets_skip_skins_and_tag_editor_only() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = write_fixture("options.glb");
    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn(
        world,
        &path,
        &GltfSpawnOptions {
            keep_sockets: false,
            skins: false,
            editor_only: true,
            root_name: Some("Tank".into()),
            ..GltfSpawnOptions::default()
        },
    );
    assert_eq!(name_of(world, spawned.root), "Tank");
    assert!(spawned.entity("socket_muzzle").is_none());
    assert_eq!(spawned.nodes.len(), 7);
    let skinny = spawned.entity("Skinny").expect("node");
    assert!(world.get::<SkinnedMesh>(skinny).is_none());
    assert!(world.get::<SkinJoints>(skinny).is_none());
    assert!(world.get::<GltfEditorOnly>(skinny).is_some());
    assert!(world.get::<GltfEditorOnly>(spawned.root).is_some());
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = new_world();
    let dir = std::env::temp_dir().join(format!("regia-gltf-hierarchy-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let bad = dir.join("bad.glb");
    std::fs::write(&bad, b"glTF\x02\0\0\0 not really").expect("write");
    assert!(spawn_gltf_hierarchy(app.world_mut(), &bad, &GltfSpawnOptions::default()).is_err());
    assert!(
        spawn_gltf_hierarchy(
            app.world_mut(),
            &dir.join("missing.glb"),
            &GltfSpawnOptions::default()
        )
        .is_err()
    );
}

#[test]
fn the_spawned_hierarchy_renders_rigid_and_skinned_meshes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let path = write_fixture("render.glb");
    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn(world, &path, &GltfSpawnOptions::default());
    // Game view: no proxy cubes for the empty nodes, which would cover the triangles.
    world.insert_resource(regia_editor::views::ViewportLayout {
        game_view: true,
        ..Default::default()
    });
    let request = PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera {
            target: Vec3::new(2.0, 1.0, 0.0),
            distance: 8.0,
            yaw: 0.7,
            pitch: 0.1,
            ..EditorCamera::default()
        },
        width: 640,
        height: 400,
        ids: false,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    let frame = render_pane(&device, world, &request).expect("render");
    let dir = regia_core::brand::env_var("REGIA_VIEWPORT_SHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-viewport-shots"));
    let _ = std::fs::create_dir_all(&dir);
    let bytes: Vec<u8> = frame
        .image
        .pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
        .collect();
    if let Some(buffer) = image::RgbaImage::from_raw(640, 400, bytes) {
        let _ = buffer.save(dir.join("gltf_hierarchy.png"));
    }
    eprintln!("meshes extracted: {}", frame.info.meshes);
    assert!(
        frame.info.meshes >= 2,
        "both meshes extracted: {}",
        frame.info.meshes
    );

    // Against the same frame without the model (sky, grid-free): only the model's pixels differ.
    for node in &spawned.nodes {
        if world.get_entity(node.entity).is_ok() {
            world.despawn(node.entity);
        }
    }
    world.despawn(spawned.root);
    let empty = render_pane(&device, world, &request).expect("empty render");
    let model_pixels = |test: &dyn Fn(egui::Color32) -> bool| {
        frame
            .image
            .pixels
            .iter()
            .zip(&empty.image.pixels)
            .filter(|(with, without)| with != without && test(**with))
            .count()
    };
    let red = model_pixels(&|p| {
        u32::from(p.r()) > u32::from(p.g()) + 25 && u32::from(p.r()) > u32::from(p.b()) + 25
    });
    let blue = model_pixels(&|p| {
        u32::from(p.b()) > u32::from(p.r()) + 25 && u32::from(p.b()) > u32::from(p.g()) + 5
    });
    assert!(red > 60, "the rigid barrel draws red ({red} px)");
    assert!(blue > 60, "the skinned triangle draws blue ({blue} px)");
}

#[test]
fn a_zero_rotation_node_stays_finite() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let json = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "name": "Broken", "rotation": [0.0, 0.0, 0.0, 0.0] }],
    });
    let mut json_bytes = serde_json::to_vec(&json).expect("json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len();
    let mut glb = Vec::with_capacity(total);
    glb.extend(b"glTF");
    glb.extend(2u32.to_le_bytes());
    glb.extend((total as u32).to_le_bytes());
    glb.extend((json_bytes.len() as u32).to_le_bytes());
    glb.extend(b"JSON");
    glb.extend(&json_bytes);
    let path = write_fixture_bytes("zero_rotation.glb", &glb);
    let mut app = new_world();
    let spawned = spawn(app.world_mut(), &path, &GltfSpawnOptions::default());
    let entity = spawned.entity("Broken").expect("node spawned");
    let transform = app.world().get::<Transform>(entity).expect("transform");
    assert!(transform.rotation.is_finite());
    assert_eq!(transform.rotation, Quat::IDENTITY);
    assert!(
        spawned.report.warnings.iter().any(|w| w.contains("Broken")),
        "{:?}",
        spawned.report.warnings
    );
}

/// Packs a glTF JSON and its binary chunk into GLB bytes.
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

/// A skin of three chained joints: node 1 `hip`, node 2 `hip_3` and node 3 `hip`. The importer
/// keeps `hip` and `hip_3` and renames node 3 to `hip_3_1`, so the name `{raw}_{node}` of node 3
/// (`hip_3`) belongs to node 2. Every vertex is bound to skin slot 2 (node 3).
#[test]
fn joint_named_like_a_deduped_sibling_still_binds() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let positions: [f32; 9] = [-0.4, 0.0, 0.0, 0.4, 0.0, 0.0, 0.0, 0.8, 0.0];
    let joints: Vec<u8> = vec![2, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0];
    let weights = f32s(&[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
    let mut bin = f32s(&positions);
    bin.extend(&joints);
    bin.extend(&weights);
    let json = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0, 4] }],
        "nodes": [
            { "name": "Rig", "children": [1] },
            { "name": "hip", "translation": [0.0, 0.5, 0.0], "children": [2] },
            { "name": "hip_3", "translation": [0.0, 0.5, 0.0], "children": [3] },
            { "name": "hip", "translation": [0.0, 0.5, 0.0] },
            { "name": "Skinny", "mesh": 0, "skin": 0 }
        ],
        "meshes": [
            { "primitives": [{ "attributes": { "POSITION": 0, "JOINTS_0": 1, "WEIGHTS_0": 2 } }] }
        ],
        "skins": [{ "joints": [1, 2, 3] }],
        "buffers": [{ "byteLength": bin.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 36, "byteLength": 12 },
            { "buffer": 0, "byteOffset": 48, "byteLength": 48 }
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [-0.4, 0.0, 0.0], "max": [0.4, 0.8, 0.0] },
            { "bufferView": 1, "componentType": 5121, "count": 3, "type": "VEC4" },
            { "bufferView": 2, "componentType": 5126, "count": 3, "type": "VEC4" }
        ]
    });
    let path = write_fixture_bytes("deduped_joint_names.glb", &glb(&json, bin));
    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn(world, &path, &GltfSpawnOptions::default());
    let node_entity = |index: usize| {
        spawned
            .nodes
            .iter()
            .find(|n| n.gltf_index == index)
            .map(|n| n.entity)
            .expect("joint node spawned")
    };
    let skinny = spawned.entity("Skinny").expect("skinned node");
    let binding = world
        .get::<SkinnedMesh>(skinny)
        .unwrap_or_else(|| panic!("the skin did not bind: {:?}", spawned.report.warnings));
    assert_eq!(
        binding.skeleton.joint_names,
        ["hip", "hip_3", "hip_3_1"],
        "the importer's names"
    );
    assert_eq!(
        binding.joints,
        [node_entity(1), node_entity(2), node_entity(3)],
        "each skeleton joint is the node it was imported from"
    );
    let renderer = world.get::<MeshRenderer>(skinny).expect("skinned mesh");
    assert!(
        renderer.mesh.vertices.iter().all(|v| v.joints[0] == 2),
        "skin slot 2 (node 3) is skeleton joint 2"
    );
}
