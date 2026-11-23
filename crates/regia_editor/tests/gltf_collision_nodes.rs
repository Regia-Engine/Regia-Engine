// SPDX-License-Identifier: MPL-2.0
//! `collision_*` nodes are collision hulls, not render meshes (ADR-1622, DO27 request DO-6): the
//! editor's glTF loaders leave them undrawn, as the cook always has. A generated GLB with a
//! visible `Hull` triangle and a two-triangle `collision_hull_0` child is loaded through the
//! merged-mesh path and the hierarchy spawn; the collision triangles must not reach a renderer
//! unless `show_collision` asks for them, in the loader, the spawn and the pane renderer.

use std::path::PathBuf;
use std::sync::Arc;

use regia_editor::gltf_hierarchy::{GltfCollisionNode, GltfSpawnOptions, spawn_gltf_hierarchy};
use regia_editor::mesh_source::{
    GltfLoadOptions, load_gltf_model, load_gltf_model_filtered, load_gltf_model_with_options,
};
use regia_render::{CpuMaterial, MeshRenderer};
use serde_json::json;

fn f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// `Hull` (one triangle) ─ `collision_hull_0` (two triangles), plus an empty `collision_marker`.
fn fixture_glb() -> Vec<u8> {
    let hull: [f32; 9] = [-0.5, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 1.0, 0.0];
    let hull_collision: [f32; 18] = [
        -1.0, -1.0, 0.0, 1.0, -1.0, 0.0, 1.0, 1.0, 0.0, -1.0, -1.0, 0.0, 1.0, 1.0, 0.0, -1.0, 1.0, 0.0,
    ];
    let mut bin = Vec::new();
    bin.extend(f32s(&hull));
    bin.extend(f32s(&hull_collision));
    let json = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0, 2] }],
        "nodes": [
            { "name": "Hull", "mesh": 0, "children": [1] },
            { "name": "collision_hull_0", "mesh": 1 },
            { "name": "collision_marker" }
        ],
        "meshes": [
            { "primitives": [{ "attributes": { "POSITION": 0 } }] },
            { "primitives": [{ "attributes": { "POSITION": 1 } }] }
        ],
        "buffers": [{ "byteLength": bin.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 36, "byteLength": 72 }
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [-0.5, 0.0, 0.0], "max": [0.5, 1.0, 0.0] },
            { "bufferView": 1, "componentType": 5126, "count": 6, "type": "VEC3", "min": [-1.0, -1.0, 0.0], "max": [1.0, 1.0, 0.0] }
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

fn write_fixture() -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir("gltf-collision-nodes");
    let path = dir.join("hull.glb");
    let tmp = dir.join("hull.glb.tmp");
    std::fs::write(&tmp, fixture_glb()).expect("write fixture");
    std::fs::rename(&tmp, &path).expect("rename fixture");
    path
}

fn new_world() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app
}

#[test]
fn the_merged_mesh_loader_leaves_collision_hulls_out() {
    let path = write_fixture();
    let material = Arc::new(CpuMaterial::default());
    let model = load_gltf_model(&path, &material).expect("load");
    assert_eq!(model.triangles(), 1, "only Hull's triangle is drawn");
    assert_eq!(model.vertices(), 3);
    // A filter that keeps every node still cannot bring a collision hull back.
    let all = load_gltf_model_filtered(&path, &material, &|_| true).expect("load");
    assert_eq!(all.triangles(), 1);
    // The filter still applies to the other nodes: without Hull, nothing is left to draw.
    assert!(load_gltf_model_filtered(&path, &material, &|name| name != "Hull").is_err());
}

#[test]
fn the_hierarchy_spawn_keeps_collision_nodes_but_does_not_draw_them() {
    let path = write_fixture();
    let mut app = new_world();
    let world = app.world_mut();
    let spawned = spawn_gltf_hierarchy(world, &path, &GltfSpawnOptions::default()).expect("spawn");
    let report = &spawned.report;
    assert_eq!(report.nodes, 3, "every node keeps its entity");
    assert_eq!(report.mesh_entities, 1, "only Hull draws");
    assert_eq!(report.triangles, 1, "collision triangles are not counted");
    assert_eq!(report.collision_nodes, 2);
    assert_eq!(report.hidden_collision_meshes, 1);

    let collision = spawned.node("collision_hull_0").expect("collision node");
    assert!(collision.is_collision);
    assert_eq!(collision.triangles, 0);
    assert!(collision.materials.is_empty());
    assert!(world.get::<GltfCollisionNode>(collision.entity).is_some());
    assert!(world.get::<MeshRenderer>(collision.entity).is_none(), "not drawn");
    let parent = spawned.entity("Hull").expect("Hull");
    assert_eq!(collision.parent, Some(parent), "still parented under its body");
    assert!(world.get::<MeshRenderer>(parent).is_some());
    assert!(world.get::<GltfCollisionNode>(parent).is_none());

    let marker = spawned.node("collision_marker").expect("marker");
    assert!(marker.is_collision);
    assert!(world.get::<GltfCollisionNode>(marker.entity).is_some());

    let mut renderers = world.query::<&MeshRenderer>();
    assert_eq!(renderers.iter(world).count(), 1);
}

#[test]
fn show_collision_draws_the_hulls_for_debugging() {
    let path = write_fixture();
    let mut app = new_world();
    let world = app.world_mut();
    let opts = GltfSpawnOptions {
        show_collision: true,
        ..GltfSpawnOptions::default()
    };
    let spawned = spawn_gltf_hierarchy(world, &path, &opts).expect("spawn");
    assert_eq!(spawned.report.mesh_entities, 2);
    assert_eq!(spawned.report.triangles, 3);
    assert_eq!(spawned.report.collision_nodes, 2);
    assert_eq!(spawned.report.hidden_collision_meshes, 0);
    let collision = spawned.node("collision_hull_0").expect("collision node");
    assert_eq!(collision.triangles, 2);
    assert!(world.get::<MeshRenderer>(collision.entity).is_some());
    assert!(
        world.get::<GltfCollisionNode>(collision.entity).is_some(),
        "the marker stays, so tools can still tell hulls apart"
    );
}

#[test]
fn the_default_options_drop_collision_hulls_like_before() {
    let path = write_fixture();
    let material = Arc::new(CpuMaterial::default());
    assert!(!GltfLoadOptions::default().keep_collision_nodes);
    let plain = load_gltf_model(&path, &material).expect("load");
    let optioned =
        load_gltf_model_with_options(&path, &material, &|_| true, GltfLoadOptions::default()).expect("load");
    assert!(plain.collision_parts.is_empty());
    assert!(optioned.collision_parts.is_empty());
    assert_eq!(optioned.triangles(), plain.triangles());
    assert_eq!(optioned.vertices(), plain.vertices());
    assert_eq!(optioned.aabb_min, plain.aabb_min);
    assert_eq!(optioned.aabb_max, plain.aabb_max);
}

#[test]
fn keep_collision_nodes_marks_the_layer_and_leaves_the_drawn_parts_alone() {
    let path = write_fixture();
    let material = Arc::new(CpuMaterial::default());
    let plain = load_gltf_model(&path, &material).expect("load");
    let options = GltfLoadOptions {
        keep_collision_nodes: true,
    };
    let kept = load_gltf_model_with_options(&path, &material, &|_| true, options).expect("load");
    // The drawn side is identical: a renderer that draws `parts` sees no change.
    assert_eq!(kept.parts.len(), plain.parts.len());
    assert_eq!(kept.triangles(), 1);
    assert_eq!(kept.vertices(), 3);
    assert_eq!(kept.aabb_min, plain.aabb_min);
    assert_eq!(kept.aabb_max, plain.aabb_max);
    // The collision layer holds the two hull triangles, as its own part.
    assert_eq!(kept.collision_parts.len(), 1);
    let layer = &kept.collision_parts[0];
    assert_eq!(layer.mesh.indices.len() / 3, 2);
    assert_eq!(layer.mesh.vertices.len(), 6);
    assert_eq!(layer.material_name, "collision");
    // The filter does not apply to the layer; it still applies to the drawn side.
    let hidden = load_gltf_model_with_options(&path, &material, &|name| name != "Hull", options);
    assert!(hidden.is_err(), "nothing drawn is still an error");
}

#[test]
fn the_collision_layer_agent_call_reports_the_hulls() {
    let path = write_fixture();
    let mut app = new_world();
    let world = app.world_mut();
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.collision_layer")
        .expect("registered");
    let value = handler(world, &json!({ "path": path.to_string_lossy() })).expect("call");
    assert_eq!(value["has_collision"], true);
    assert_eq!(value["collision_triangles"], 2);
    assert_eq!(value["collision_vertices"], 6);
    assert_eq!(value["render_triangles"], 1);
    assert_eq!(value["collision_bounds"]["min"], json!([-1.0, -1.0, 0.0]));
    assert_eq!(value["collision_bounds"]["max"], json!([1.0, 1.0, 0.0]));
    let missing = handler(world, &json!({ "path": "no/such.glb" }));
    assert!(missing.is_err());
}

/// Pixels of a pane's id buffer that name `entity`.
fn id_pixels_of(frame: &regia_editor::render::PaneFrame, entity: regia_ecs::prelude::Entity) -> usize {
    let ids = frame.ids.as_ref().expect("the pane rendered its id buffer");
    (0..ids.height)
        .flat_map(|y| (0..ids.width).map(move |x| (x, y)))
        .filter(|&(x, y)| ids.at(x, y).is_some_and(|(hit, _)| hit == entity))
        .count()
}

/// The pane renderer (lavapipe) draws a `GltfCollisionNode` only when the overlay is on: by
/// default the hull's node is spawned and marked but reaches neither the shaded frame's mesh
/// list nor the id buffer; with `show_collision` it does, and the drawn body is unchanged.
#[test]
fn the_pane_renderer_skips_marked_collision_nodes_unless_the_overlay_is_on() {
    use regia_editor::render::{PaneRequest, render_pane};
    use regia_editor::viewport::EditorCamera;
    use regia_editor::views::ViewKind;

    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let path = write_fixture();
    // Looking down -Z at the XY plane the fixture lies in: the 2 m hull quad fills the middle
    // of the frame, the 1 m body triangle sits on its upper half.
    let request = PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera {
            target: regia_core::math::Vec3::ZERO,
            distance: 4.0,
            yaw: 0.0,
            pitch: 0.0,
            ..EditorCamera::default()
        },
        width: 160,
        height: 120,
        ids: true,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    let mut drawn = Vec::new();
    for show_collision in [false, true] {
        let mut app = new_world();
        app.world_mut().insert_resource(request.camera.clone());
        app.world_mut().insert_resource(device.clone());
        let world = app.world_mut();
        let opts = GltfSpawnOptions {
            show_collision,
            ..GltfSpawnOptions::default()
        };
        let spawned = spawn_gltf_hierarchy(world, &path, &opts).expect("spawn");
        let hull = spawned.node("collision_hull_0").expect("collision node").entity;
        let body = spawned.entity("Hull").expect("Hull");
        assert!(
            world.get::<GltfCollisionNode>(hull).is_some(),
            "marked either way"
        );
        let frame = render_pane(&device, world, &request).expect("pane");
        drawn.push((
            frame.info.meshes,
            id_pixels_of(&frame, body),
            id_pixels_of(&frame, hull),
        ));
    }
    let (off, on) = (drawn[0], drawn[1]);
    assert_eq!(off.0, 1, "overlay off: only the body is drawn");
    assert!(off.1 > 0, "the body is in view");
    assert_eq!(off.2, 0, "overlay off: no pixel of the hull");
    assert_eq!(on.0, 2, "overlay on: the hull is drawn too");
    assert!(
        on.2 > off.1,
        "overlay on: the 2 m hull covers more than the body did: {on:?}"
    );
    // The body sits in the hull's plane, so where they overlap the depth test may give the
    // pixel to either: the body never gains pixels from the overlay.
    assert!(on.1 <= off.1, "{off:?} {on:?}");
}
