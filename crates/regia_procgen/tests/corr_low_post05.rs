// SPDX-License-Identifier: MPL-2.0
//! Corr-low procgen finding without a fail-first test on the WIP branch (lane post05-procgen-runtime,
//! ADR-7214): two generators without an `EntityGuid` gave their prefab instances the same guids
//! (both scoped by `Guid::NIL`).

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource, ProcPrefabRef, request_run,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::EntityGuid;
use regia_scene::{GlobalTransform, Transform};

fn graph() -> String {
    let n = |ty: &str, s: &[(&str, &str)]| {
        let mut e = NodeEntry::new(ty);
        for (k, v) in s {
            e = e.with(k, SettingValue::parse(v).expect("setting"));
        }
        e
    };
    ProcGraph {
        nodes: [
            (1, n("gen.grid", &[("count", "(3.0, 1.0, 1.0)")])),
            (2, n("spawn.prefab", &[("prefab", "\"props/crate.prefab.ron\"")])),
        ]
        .into_iter()
        .collect(),
        edges: vec![Edge::new(1, "out", 2, "in")],
        ..ProcGraph::default()
    }
    .to_ron()
}

#[test]
fn guidless_generators_give_their_prefab_instances_distinct_guids() {
    let mut app = App::new();
    let mut files = BTreeMap::new();
    files.insert("crates.pgraph.ron".to_string(), graph());
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.insert_resource(AppTypeRegistry::default());
    let gens: Vec<Entity> = (0..2)
        .map(|_| {
            app.world_mut()
                .spawn((
                    ProcGenerator {
                        bounds: ProcBounds::Box {
                            half_extents: Vec3::new(20.0, 20.0, 20.0),
                        },
                        ..ProcGenerator::new("crates.pgraph.ron")
                    },
                    Transform::default(),
                    GlobalTransform::default(),
                ))
                .id()
        })
        .collect();
    for e in &gens {
        request_run(app.world_mut(), *e);
    }
    for _ in 0..4 {
        app.update();
    }
    let world = app.world_mut();
    let guids: Vec<EntityGuid> = world
        .query_filtered::<&EntityGuid, With<ProcPrefabRef>>()
        .iter(world)
        .copied()
        .collect();
    assert_eq!(guids.len(), 6, "three crates per generator");
    let distinct: BTreeSet<u128> = guids.iter().map(|g| g.0.0).collect();
    assert_eq!(
        distinct.len(),
        6,
        "every prefab instance has its own guid: {guids:?}"
    );
}

/// The asset path a bake is cooked under (ADR-7214): project-relative, `/`-separated, under
/// `assets/`, whatever form the bake directory was set in; nothing outside the project.
#[test]
fn bake_asset_paths_are_project_relative_under_assets() {
    use regia_procgen::world::bake_asset_path;
    use std::path::Path;
    let root = Path::new("/p");
    let want = Some("assets/a.procgen/g.pgout");
    assert_eq!(
        bake_asset_path(root, Path::new("/p/assets/a.procgen/g.pgout")).as_deref(),
        want
    );
    assert_eq!(
        bake_asset_path(root, Path::new("assets/a.procgen/g.pgout")).as_deref(),
        want
    );
    assert_eq!(
        bake_asset_path(root, Path::new("./a.procgen/g.pgout")).as_deref(),
        want
    );
    assert_eq!(bake_asset_path(root, Path::new("../x/g.pgout")), None);
    assert_eq!(bake_asset_path(root, Path::new("/elsewhere/g.pgout")), None);
}
