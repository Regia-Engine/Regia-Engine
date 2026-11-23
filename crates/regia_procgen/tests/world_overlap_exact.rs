// SPDX-License-Identifier: MPL-2.0
//! `in.overlap` tests each point's oriented box against the collider's shape, not only its bounds
//! (spec 63 § 3, ADR-3027): a sphere collider takes the point beside its face and refuses the point
//! beside its bounding box's corner; a turned box collider follows its turn.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_physics::{Collider, ColliderShape, RigidBody};
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource, ProcOutput,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::EntityGuid;
use regia_scene::{GlobalTransform, Transform};

fn n(ty: &str, s: &[(&str, &str)]) -> NodeEntry {
    let mut e = NodeEntry::new(ty);
    for (k, v) in s {
        e = e.with(k, SettingValue::parse(v).expect("setting"));
    }
    e
}

/// Unit-box points (half 0.5) at `points`; `inside` and `outside` exported.
fn graph(points: &str) -> String {
    ProcGraph {
        nodes: [
            (1, n("gen.points", &[("points", points)])),
            (2, n("in.overlap", &[])),
            (3, n("out.points", &[("name", "\"inside\"")])),
            (4, n("out.points", &[("name", "\"outside\"")])),
        ]
        .into(),
        edges: vec![
            Edge::new(1, "out", 2, "in"),
            Edge::new(2, "inside", 3, "in"),
            Edge::new(2, "outside", 4, "in"),
        ],
        ..ProcGraph::default()
    }
    .to_ron()
}

fn run(points: &str, shape: ColliderShape, rotation: Quat) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
    let mut app = App::new();
    let files: BTreeMap<String, String> = [("g.pgraph.ron".to_string(), graph(points))].into();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, ProcGenPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
    app.world_mut().spawn((
        Transform {
            translation: Vec3::ZERO,
            rotation,
            ..Transform::default()
        },
        RigidBody::static_body(),
        Collider {
            shape,
            ..Collider::default()
        },
        EntityGuid(Guid(5)),
    ));
    for _ in 0..10 {
        app.update();
    }
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(10.0, 10.0, 10.0),
                },
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(Guid(0x2912)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    regia_procgen::world::run_now(app.world_mut(), e);
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let get = |k: &str| {
        out.0
            .points
            .get(k)
            .map(|p| p.position.to_vec())
            .unwrap_or_default()
    };
    (get("inside"), get("outside"))
}

#[test]
fn a_sphere_collider_takes_points_by_its_surface_not_its_bounds() {
    // Radius 1 at the origin. (1.4, 0, 0): its box is 0.9 from the center. The diagonal point's box
    // corner is 1.6 from the center, though its box overlaps the sphere's bounding box.
    let k = 1.6 / 3f32.sqrt() + 0.5;
    let points = format!("[((1.4, 0.0, 0.0)), (({k:?}, {k:?}, {k:?}))]");
    let (inside, outside) = run(&points, ColliderShape::Sphere { radius: 1.0 }, Quat::IDENTITY);
    assert_eq!(inside, vec![[1.4, 0.0, 0.0]]);
    assert_eq!(outside, vec![[k, k, k]]);
}

#[test]
fn a_turned_box_collider_follows_its_turn() {
    // A 2 x 2 x 2 box turned 45 degrees about y reaches 1.414 along x and z, 1 along the diagonal.
    // (1.8, 0, 0): its box starts at x 1.3 < 1.414. (1.55, 0, 1.55): its corner (1.05, _, 1.05) is
    // 1.485 from the axis along the diagonal, past the face at 1.0, yet inside the turned box's AABB.
    let (inside, outside) = run(
        "[((1.8, 0.0, 0.0)), ((1.55, 0.0, 1.55))]",
        ColliderShape::Box {
            half_extents: Vec3::ONE,
        },
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_4),
    );
    assert_eq!(inside, vec![[1.8, 0.0, 0.0]]);
    assert_eq!(outside, vec![[1.55, 0.0, 1.55]]);
}
