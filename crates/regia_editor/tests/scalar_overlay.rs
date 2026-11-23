// SPDX-License-Identifier: MPL-2.0
//! Per-vertex scalar overlay through the editor viewport and the agent protocol (Tincal engine
//! request F10): `editor.set_scalar_overlay` colours one mesh entity, a rigid cube here, while a
//! skinned cube beside it is untouched; a mismatched values count draws nothing and reports it;
//! `clear` restores the pixels.

use std::sync::Arc;

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_editor::render::{PaneRequest, render_pane};
use regia_editor::viewport::EditorCamera;
use regia_editor::views::ViewKind;
use regia_render::{CpuMesh, JointMatrix, LabMaterial, MeshRenderer, SkinJoints};
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

/// A rigid cube left of centre and the same cube, skinned, right of it.
fn world_with_cubes() -> (regia_app::App, Entity, Entity, usize) {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut().insert_resource(EditorCamera::default());
    let world = app.world_mut();
    let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    let count = vertices.len();
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let material = Arc::new(LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        ..LabMaterial::default()
    });
    let rigid = world
        .spawn((
            Transform::from_translation(Vec3::new(-1.2, 0.5, 0.0)),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ))
        .id();
    let mut skinned_vertices = mesh.vertices.clone();
    for v in &mut skinned_vertices {
        v.joints = [0; 4];
        v.weights = [1.0, 0.0, 0.0, 0.0];
    }
    let skinned_mesh = Arc::new(CpuMesh {
        vertices: skinned_vertices,
        indices: mesh.indices.clone(),
    });
    let skinned = world
        .spawn((
            Transform::IDENTITY,
            MeshRenderer::new(skinned_mesh, material),
            SkinJoints {
                palette: vec![JointMatrix::translation(1.2, 0.5, 0.0)],
            },
        ))
        .id();
    (app, rigid, skinned, count)
}

fn request() -> PaneRequest {
    PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera {
            target: Vec3::new(0.0, 0.5, 0.0),
            distance: 6.0,
            yaw: 0.4,
            pitch: 0.35,
            ..EditorCamera::default()
        },
        width: 480,
        height: 320,
        ids: false,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    }
}

#[test]
fn a_negative_paint_index_leaves_every_vertex_at_zero() {
    let (mut app, rigid, _, vertices) = world_with_cubes();
    let world = app.world_mut();
    let out = call(
        world,
        &json!({ "entity": rigid.to_bits(), "paint": { "vertices": [-1, 1.5, 9999], "value": 1.0 } }),
    )
    .expect("negative paint");
    assert_eq!(out["overlay"]["fit"], "exact");
    let overlay = world.get::<regia_render::ScalarOverlay>(rigid).expect("overlay");
    assert_eq!(overlay.values.len(), vertices);
    assert!(
        overlay.values.iter().all(|value| *value == 0.0),
        "out-of-range paint indices are ignored: {:?}",
        overlay.values.as_ref()
    );
}

fn call(world: &mut World, params: &Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let methods = regia_editor::scalar_overlay_agent::methods();
    let (schema, handler) = &methods[0];
    assert_eq!(schema.name, "editor.set_scalar_overlay");
    handler(world, params)
}

/// Pixels that differ between two frames, left and right of the centre line.
fn changed(a: &egui::ColorImage, b: &egui::ColorImage) -> (usize, usize) {
    let width = a.width();
    let (mut left, mut right) = (0, 0);
    for (i, (p, q)) in a.pixels.iter().zip(&b.pixels).enumerate() {
        if p != q {
            if i % width < width / 2 {
                left += 1;
            } else {
                right += 1;
            }
        }
    }
    (left, right)
}

#[test]
fn the_agent_colors_one_entity_and_clear_restores_the_pixels() {
    let device = device();
    let (mut app, rigid, skinned, vertices) = world_with_cubes();
    let world = app.world_mut();
    let base = render_pane(&device, world, &request()).expect("base");

    // Height heatmap on the rigid cube: values from the object-space Y axis, range -0.5..0.5.
    let out = call(
        world,
        &json!({ "entity": rigid.to_bits(), "axis": "y", "range": [-0.5, 0.5], "ramp": "viridis" }),
    )
    .expect("set overlay");
    assert_eq!(out["overlay"]["fit"], "exact");
    assert_eq!(out["overlay"]["vertices"], vertices);
    assert_eq!(out["overlay"]["ramp"], "viridis");
    assert_eq!(out["overlay"]["min"], -0.5);
    assert_eq!(out["overlay"]["max"], 0.5);
    let shown = render_pane(&device, world, &request()).expect("overlay");
    let (left, right) = changed(&base.image, &shown.image);
    eprintln!("overlay on the rigid cube changed {left} px left, {right} px right");
    assert!(left > 300, "the rigid cube is coloured ({left} px)");
    assert_eq!(right, 0, "the skinned cube beside it is untouched");

    // The same on the skinned cube: it follows its palette, to the right of centre.
    call(world, &json!({ "entity": skinned.to_bits(), "constant": 1.0 })).expect("skinned overlay");
    let both = render_pane(&device, world, &request()).expect("both");
    let (_, right) = changed(&shown.image, &both.image);
    assert!(right > 300, "the skinned cube is coloured ({right} px)");

    // Ramp, range and opacity edit the existing overlay without resending values.
    let out = call(
        world,
        &json!({ "entity": rigid.to_bits(), "ramp": "diverging", "opacity": 0.5, "range": [0.0, 1.0] }),
    )
    .expect("edit overlay");
    assert_eq!(out["overlay"]["ramp"], "diverging");
    assert_eq!(out["overlay"]["opacity"], 0.5);

    // Painting vertices on top of the current values.
    let out = call(
        world,
        &json!({ "entity": rigid.to_bits(), "paint": { "vertices": [0, 1, 9999], "value": 1.0 } }),
    )
    .expect("paint");
    assert_eq!(
        out["overlay"]["fit"], "exact",
        "an out-of-range vertex is ignored"
    );

    // A values count that does not match the vertices is reported, never fatal, and draws nothing.
    call(world, &json!({ "entity": skinned.to_bits(), "clear": true })).expect("clear skinned");
    call(
        world,
        &json!({ "entity": rigid.to_bits(), "values": [0.0, 1.0, 0.5], "opacity": 1.0 }),
    )
    .expect("short values");
    let out = call(world, &json!({ "entity": rigid.to_bits(), "ramp": "viridis" })).expect("keep");
    assert_eq!(out["overlay"]["fit"]["mismatch"], "short");
    let none = render_pane(&device, world, &request()).expect("mismatch");
    assert_eq!(
        changed(&base.image, &none.image),
        (0, 0),
        "a mismatch draws nothing"
    );

    // Removal restores every pixel.
    let out = call(world, &json!({ "entity": rigid.to_bits(), "clear": true })).expect("clear");
    assert_eq!(out["removed"], true);
    let restored = render_pane(&device, world, &request()).expect("restored");
    assert_eq!(
        restored.image.pixels, base.image.pixels,
        "removal restores the pixels"
    );
}

#[test]
fn bad_calls_are_rejected_with_a_message() {
    let (mut app, rigid, _skinned, _) = world_with_cubes();
    let world = app.world_mut();
    for params in [
        json!({}),
        json!({ "entity": rigid.to_bits() }),
        json!({ "entity": rigid.to_bits(), "constant": 1.0, "axis": "x" }),
        json!({ "entity": rigid.to_bits(), "axis": "w" }),
        json!({ "entity": rigid.to_bits(), "constant": 1.0, "ramp": "nope" }),
        json!({ "entity": rigid.to_bits(), "constant": 1.0, "ramp": [{ "at": 0.0, "color": [0, 0, 0] }] }),
        json!({ "entity": rigid.to_bits(), "constant": 1.0, "range": [1.0] }),
        json!({ "entity": 12345678 }),
    ] {
        let err = call(world, &params).expect_err(&params.to_string());
        eprintln!("{params} -> {err:?}");
    }
    assert!(world.get::<regia_render::ScalarOverlay>(rigid).is_none());
    // Custom ramp stops are accepted and echoed.
    let out = call(
        world,
        &json!({ "entity": rigid.to_bits(), "constant": 0.5,
            "ramp": [{ "at": 0.0, "color": [0, 0, 1] }, { "at": 1.0, "color": [1, 0, 0] }] }),
    )
    .expect("custom ramp");
    assert_eq!(out["overlay"]["ramp"][1]["color"], json!([1.0, 0.0, 0.0]));
}
