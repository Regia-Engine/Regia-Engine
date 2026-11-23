// SPDX-License-Identifier: MPL-2.0
//! Custom handles and snap targets (Tincal engine request F6): snap points of a posed box
//! (face centers, edge midpoints, bounds centers) and of a connector child, picking within a
//! pixel radius, the egui widget driven by synthetic pointer events, and the agent methods.

use std::sync::Arc;

use regia_core::math::{Quat, Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_editor::handles::{
    DEFAULT_PICK_RADIUS, DragPhase, Handle, SnapConnector, SnapKind, SnapKinds, SnapSet, handles,
    snap_targets,
};
use regia_editor::views::ViewCamera;
use regia_render::{CpuMesh, LabMaterial, MeshRenderer};
use regia_scene::GlobalTransform;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

/// A box of half-size (0.5, 1, 0.25) turned 90° about Y and moved to x = 3, with a connector
/// child on its +X face (local), world transforms set by hand (no schedule runs here).
fn scene() -> (World, Entity, Entity) {
    let mut world = World::new();
    let (vertices, indices) = regia_render::box_mesh([0.5, 1.0, 0.25]);
    let model = Transform {
        translation: Vec3::new(3.0, 0.0, 0.0),
        rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        ..Transform::IDENTITY
    };
    let root = world
        .spawn((
            model,
            GlobalTransform(model.compute_affine()),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
        ))
        .id();
    let connector = world
        .spawn((
            Transform::IDENTITY,
            GlobalTransform(model.compute_affine()),
            SnapConnector {
                center: Vec3::new(0.5, 0.0, 0.0),
                normal: Vec3::X,
            },
            ChildOf(root),
        ))
        .id();
    (world, root, connector)
}

fn camera() -> ViewCamera {
    ViewCamera {
        eye: Vec3::new(1.0, 2.0, 12.0),
        target: Vec3::new(1.0, 0.0, 0.0),
        fov_y: 0.8,
        near: 0.1,
        far: 100.0,
        ortho: false,
    }
}

const SIZE: Vec2 = Vec2::new(800.0, 600.0);

#[test]
fn snap_targets_find_faces_edges_bounds_and_connectors() {
    let (world, root, connector) = scene();
    let all = snap_targets(&world, root, SnapKinds::ALL);
    let count = |kind| all.points.iter().filter(|p| p.kind == kind).count();
    assert_eq!(count(SnapKind::FaceCenter), 6);
    assert_eq!(count(SnapKind::EdgeMidpoint), 12);
    assert_eq!(
        count(SnapKind::BoundsCenter),
        1,
        "one mesh: no extra subtree center"
    );
    assert_eq!(count(SnapKind::Connector), 1);
    assert!(!all.truncated);

    // The box is turned 90° about Y: its local +X face (center (0.5, 0, 0)) points to world -Z.
    let face = all
        .points
        .iter()
        .find(|p| p.kind == SnapKind::FaceCenter && p.normal.is_some_and(|n| n.dot(Vec3::NEG_Z) > 0.99))
        .expect("the turned +X face");
    assert!(
        (face.position - Vec3::new(3.0, 0.0, -0.5)).length() < 1e-4,
        "{:?}",
        face.position
    );
    let bounds = all
        .points
        .iter()
        .find(|p| p.kind == SnapKind::BoundsCenter)
        .expect("bounds");
    assert!((bounds.position - Vec3::new(3.0, 0.0, 0.0)).length() < 1e-4);
    let contact = all
        .points
        .iter()
        .find(|p| p.kind == SnapKind::Connector)
        .expect("connector");
    assert_eq!(contact.entity, connector);
    assert!(
        (contact.position - face.position).length() < 1e-4,
        "the connector sits on that face"
    );
    assert!(contact.normal.expect("normal").dot(Vec3::NEG_Z) > 0.99);

    // Kinds filter.
    let only_edges = snap_targets(&world, root, SnapKinds::EDGE_MIDPOINTS);
    assert_eq!(only_edges.points.len(), 12);
    assert!(snap_targets(&world, root, SnapKinds::NONE).points.is_empty());
    // A leaf entity without a mesh has nothing but its own connector.
    assert_eq!(snap_targets(&world, connector, SnapKinds::ALL).points.len(), 1);
}

#[test]
fn nearest_snap_picks_within_the_pixel_radius() {
    let (world, root, _) = scene();
    let set = snap_targets(&world, root, SnapKinds::FACE_CENTERS);
    let view = camera();
    // The +Y face center is (3, 1, 0).
    let target = Vec3::new(3.0, 1.0, 0.0);
    let pixel = view.project(target, SIZE).expect("visible");
    let hit = set
        .nearest(&view, SIZE, pixel + Vec2::new(3.0, 2.0), DEFAULT_PICK_RADIUS)
        .expect("near");
    assert!((hit.position - target).length() < 1e-4, "{hit:?}");
    assert_eq!(hit.kind, SnapKind::FaceCenter);
    assert!(
        set.nearest(&view, SIZE, Vec2::new(5.0, 5.0), DEFAULT_PICK_RADIUS)
            .is_none()
    );
}

fn raw(events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(SIZE.x, SIZE.y),
        )),
        events,
        ..egui::RawInput::default()
    }
}

#[test]
fn the_egui_widget_hovers_and_drags_a_point() {
    let ctx = egui::Context::default();
    let view = camera();
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SIZE.x, SIZE.y));
    let start = Vec3::new(0.0, 0.0, 0.0);
    let goal = Vec3::new(1.5, 1.0, 0.0);
    let at = |p: Vec3| {
        let px = view.project(p, SIZE).expect("visible");
        egui::pos2(px.x, px.y)
    };
    let list = [Handle::Point { pos: start }];
    let mut out = Vec::new();
    let mut frame = |events: Vec<egui::Event>| {
        let mut result = None;
        let mut output = ctx.run_ui(raw(events), |ui| {
            result = handles(
                ui,
                &view,
                rect,
                egui::Id::new("test-handles"),
                &list,
                &SnapSet::empty(),
            );
        });
        output.textures_delta.clear();
        out.push(result);
    };
    let button = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    frame(vec![egui::Event::PointerMoved(at(start))]);
    frame(vec![button(at(start), true)]);
    frame(vec![egui::Event::PointerMoved(at(Vec3::new(0.7, 0.5, 0.0)))]);
    frame(vec![egui::Event::PointerMoved(at(goal))]);
    frame(vec![button(at(goal), false)]);
    let phases: Vec<DragPhase> = out.iter().flatten().map(|d| d.phase).collect();
    assert_eq!(phases.first(), Some(&DragPhase::Hover), "{phases:?}");
    assert!(
        phases.contains(&DragPhase::Begin) && phases.contains(&DragPhase::Drag),
        "{phases:?}"
    );
    assert_eq!(phases.last(), Some(&DragPhase::End), "{phases:?}");
    let end = out.last().copied().flatten().expect("end event");
    // The drag plane faces the camera, so compare where the handle lands on screen.
    let landed = view.project(start + end.total, SIZE).expect("visible");
    let wanted = view.project(goal, SIZE).expect("visible");
    assert!((landed - wanted).length() < 1.0, "{landed:?} vs {wanted:?}");
}

#[test]
fn the_agent_methods_report_snaps_and_replay_drags() {
    let (mut world, root, _) = scene();
    let methods = regia_editor::agent_ext::all_methods();
    let call = |world: &mut World, name: &str, params: Value| -> Result<Value, _> {
        let (_, handler) = methods
            .iter()
            .find(|(schema, _)| schema.name == name)
            .unwrap_or_else(|| panic!("{name} is registered"));
        handler(world, &params)
    };
    let out = call(
        &mut world,
        "editor.snap_targets",
        json!({ "entity": root.to_bits(), "kinds": ["face_center", "connector"] }),
    )
    .expect("snap_targets");
    assert_eq!(out["count"], 7, "{out}");
    assert_eq!(out["truncated"], false);
    let cut = call(
        &mut world,
        "editor.snap_targets",
        json!({ "entity": root.to_bits(), "max_points": 3 }),
    )
    .expect("cut");
    assert_eq!(cut["count"], 3);
    assert_eq!(cut["truncated"], true);
    assert!(
        call(
            &mut world,
            "editor.snap_targets",
            json!({ "entity": root.to_bits(), "kinds": ["corner"] })
        )
        .is_err()
    );
    assert!(
        call(
            &mut world,
            "editor.snap_targets",
            json!({ "entity": 12_345_678_u64 })
        )
        .is_err()
    );

    // Replay a drag of a point handle through the default editor camera.
    let layout = world
        .get_resource::<regia_editor::views::ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    let (_, camera) = layout.pane_camera(&world, 0);
    let view = ViewCamera::of(regia_editor::views::ViewKind::Perspective, &camera);
    let size = Vec2::new(640.0, 480.0);
    let from = view.project(camera.target, size).expect("target visible");
    let to = from + Vec2::new(40.0, -20.0);
    let drag = call(
        &mut world,
        "editor.handle_drag",
        json!({
            "handles": [{ "type": "point", "pos": camera.target.to_array() }],
            "from": [from.x, from.y], "to": [to.x, to.y],
        }),
    )
    .expect("handle_drag");
    assert_eq!(drag["grabbed"], true, "{drag}");
    assert_eq!(drag["result"]["phase"], "end");
    let total = drag["result"]["total"].as_array().expect("total");
    assert!(
        total[0].as_f64().expect("x") > 0.0 && total[1].as_f64().expect("y") > 0.0,
        "{drag}"
    );
    let missed = call(
        &mut world,
        "editor.handle_drag",
        json!({ "handles": [{ "type": "point", "pos": [100.0, 0.0, 0.0] }], "from": [5, 5], "to": [9, 9] }),
    )
    .expect("miss");
    assert_eq!(missed["grabbed"], false);
    assert!(
        call(
            &mut world,
            "editor.handle_drag",
            json!({ "handles": [{ "type": "knob" }], "from": [1, 1], "to": [2, 2] })
        )
        .is_err()
    );
}
