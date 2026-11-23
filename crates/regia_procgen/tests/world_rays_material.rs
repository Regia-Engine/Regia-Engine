// SPDX-License-Identifier: MPL-2.0
//! `in.rays` hits carry the hit collider's physics material name (`phys_material`, spec 63 § 3,
//! ADR-3027): the `PhysicsMaterialRef` next to the collider, empty for a collider without one.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, PhysicsMaterialRef, RigidBody};
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource, ProcOutput,
};
use regia_procgen::{AttrColumn, Edge, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::EntityGuid;
use regia_scene::{GlobalTransform, Transform};

fn n(ty: &str, s: &[(&str, &str)]) -> NodeEntry {
    let mut e = NodeEntry::new(ty);
    for (k, v) in s {
        e = e.with(k, SettingValue::parse(v).expect("setting"));
    }
    e
}

fn rays_graph() -> String {
    ProcGraph {
        nodes: [
            (1, n("in.self", &[])),
            (
                2,
                n("in.rays", &[("spacing_m", "4.0"), ("from_height_m", "50.0")]),
            ),
            (3, n("out.points", &[("name", "\"hits\"")])),
        ]
        .into(),
        edges: vec![Edge::new(1, "out", 2, "in"), Edge::new(2, "out", 3, "in")],
        ..ProcGraph::default()
    }
    .to_ron()
}

fn plate(app: &mut App, x: f32, guid: u128, material: Option<&str>) {
    let mut e = app.world_mut().spawn((
        Transform::from_translation(Vec3::new(x, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(4.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
        EntityGuid(Guid(guid)),
    ));
    if let Some(m) = material {
        e.insert(PhysicsMaterialRef(m.to_string()));
    }
}

#[test]
fn ray_hits_name_the_physics_material_of_the_collider_they_hit() {
    let mut app = App::new();
    let files: BTreeMap<String, String> = [("rays.pgraph.ron".to_string(), rays_graph())].into();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin, ProcGenPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
    // Two plates side by side: x in [-8, 0] is ice, x in [0, 8] has no material.
    plate(&mut app, -4.0, 1, Some("ice"));
    plate(&mut app, 4.0, 2, None);
    for _ in 0..10 {
        app.update();
    }
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(8.0, 20.0, 8.0),
                },
                ..ProcGenerator::new("rays.pgraph.ron")
            },
            EntityGuid(Guid(0x2912)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    regia_procgen::world::run_now(app.world_mut(), e);
    let out = app.world().get::<ProcOutput>(e).expect("output");
    let hits = &out.0.points["hits"];
    assert_eq!(hits.len(), 16);
    let Some(AttrColumn::Name(materials)) = hits.attrs.get("phys_material") else {
        panic!(
            "no phys_material column: {:?}",
            hits.attrs.keys().collect::<Vec<_>>()
        );
    };
    for (i, m) in materials.iter().enumerate() {
        let want = if hits.position[i][0] < 0.0 { "ice" } else { "" };
        assert_eq!(m, want, "hit {i} at {:?}", hits.position[i]);
    }
}
