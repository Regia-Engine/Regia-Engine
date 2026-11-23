// SPDX-License-Identifier: MPL-2.0
//! `in.entities` points carry the entity's mesh bounds (spec 63 § 3, ADR-3027): the local bounds of
//! its `MeshRenderer` mesh, the unit box for an entity without a mesh.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource, ProcOutput,
};
use regia_procgen::{Edge, GeneratedMesh, NodeEntry, ProcGraph, SettingValue};
use regia_render::{CpuMaterial, MeshRenderer};
use regia_scene::components::{EntityGuid, Tags};
use regia_scene::{GlobalTransform, Transform};

fn n(ty: &str, s: &[(&str, &str)]) -> NodeEntry {
    let mut e = NodeEntry::new(ty);
    for (k, v) in s {
        e = e.with(k, SettingValue::parse(v).expect("setting"));
    }
    e
}

fn graph() -> String {
    ProcGraph {
        nodes: [
            (1, n("in.entities", &[("tags", "[\"rock\"]")])),
            (2, n("out.points", &[("name", "\"rocks\"")])),
        ]
        .into(),
        edges: vec![Edge::new(1, "out", 2, "in")],
        ..ProcGraph::default()
    }
    .to_ron()
}

#[test]
fn entity_points_take_their_mesh_bounds() {
    let mut app = App::new();
    let files: BTreeMap<String, String> = [("g.pgraph.ron".to_string(), graph())].into();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    // A triangle spanning (-1, 0, -2) .. (3, 2, 1) in its own space.
    let mesh = regia_procgen::draw::cpu_mesh(&GeneratedMesh {
        positions: vec![[-1.0, 0.0, -2.0], [3.0, 2.0, 0.0], [0.0, 1.0, 1.0]],
        normals: vec![[0.0, 1.0, 0.0]; 3],
        uvs: vec![[0.0, 0.0]; 3],
        indices: vec![0, 1, 2],
        ..GeneratedMesh::default()
    });
    let renderer = MeshRenderer::new(Arc::new(mesh), Arc::new(CpuMaterial::default()));
    let tags = || Tags::from_iter_str(["rock"]);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(1.0, 0.0, 1.0)),
        GlobalTransform::default(),
        EntityGuid(Guid(1)),
        tags(),
        renderer,
    ));
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(-1.0, 0.0, -1.0)),
        GlobalTransform::default(),
        EntityGuid(Guid(2)),
        tags(),
    ));
    app.update();
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(20.0, 20.0, 20.0),
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
    let rocks = &out.0.points["rocks"];
    assert_eq!(rocks.len(), 2, "both tagged entities");
    // Guid order: the meshed entity first.
    assert_eq!(rocks.bounds_min[0], [-1.0, 0.0, -2.0]);
    assert_eq!(rocks.bounds_max[0], [3.0, 2.0, 1.0]);
    assert_eq!(rocks.bounds_min[1], [-0.5; 3], "no mesh: the unit box");
    assert_eq!(rocks.bounds_max[1], [0.5; 3]);
}
