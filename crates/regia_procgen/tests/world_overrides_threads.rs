// SPDX-License-Identifier: MPL-2.0
//! The world host's side of ADR-2724: `spawn.prefab` overrides reach the instances a generator
//! spawns, `ProcGenSettings::threads` picks the schedule without changing the output, and a bake of
//! a graph with overrides is refused with its own code instead of writing a file that cannot load.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::reflect::AppTypeRegistry;
use regia_procgen::world::{
    ProcBounds, ProcGenMode, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
    bake, bake_path, run_now,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::EntityGuid;
use regia_scene::format::SceneFile;
use regia_scene::prefab::{PrefabLibrary, PrefabTemplate, resolve_pending_prefabs};
use regia_scene::{GlobalTransform, Transform};

/// A game component the prefab carries; the graph drives `intensity` per instance.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Glow {
    /// The driven field.
    pub intensity: f32,
}

const PREFAB: Guid = Guid(0xfeed);
const ROOT: Guid = Guid(0xbeef);

fn graph_text(overrides: bool) -> String {
    let n = |ty: &str, s: &[(&str, String)]| {
        let mut e = NodeEntry::new(ty);
        for (k, v) in s {
            e = e.with(k, SettingValue::parse(v).expect("setting"));
        }
        e
    };
    let mut spawn = vec![("prefab", format!("\"guid:{:032x}\"", PREFAB.as_u128()))];
    if overrides {
        spawn.push((
            "overrides",
            "[(attr: \"position:x\", path: \"Glow.intensity\")]".to_string(),
        ));
    }
    ProcGraph {
        nodes: [
            (
                1,
                n(
                    "gen.grid",
                    &[
                        ("count", "(3.0, 1.0, 1.0)".to_string()),
                        ("step_m", "(2.0, 1.0, 1.0)".to_string()),
                        ("centered", "false".to_string()),
                    ],
                ),
            ),
            (2, n("spawn.prefab", &spawn)),
        ]
        .into(),
        edges: vec![Edge::new(1, "out", 2, "in")],
        ..ProcGraph::default()
    }
    .to_ron()
}

fn app(overrides: bool, settings: ProcGenSettings) -> (App, Entity) {
    let mut app = App::new();
    let files: BTreeMap<String, String> = [("g.pgraph.ron".to_string(), graph_text(overrides))].into();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..settings
    });
    app.add_plugins(ProcGenPlugin);
    app.register_type::<Glow>();
    app.init_resource::<PrefabLibrary>();
    app.add_systems(PreUpdate, resolve_pending_prefabs);
    let file = SceneFile::from_ron_str(&format!(
        r#"(format_version: 1, entities: [
            (guid: "{ROOT}", name: "Lamp", components: {{
                "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),
                "{}": (intensity: -1.0),
            }}),
        ])"#,
        std::any::type_name::<Glow>()
    ))
    .expect("prefab");
    let registry = app.world().resource::<AppTypeRegistry>().0.clone();
    let template = PrefabTemplate::from_scene_file(PREFAB, &file, &registry.read()).expect("bake");
    app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(40.0, 50.0, 40.0),
                },
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(Guid(0xabc)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    (app, e)
}

fn glows(app: &mut App) -> Vec<f32> {
    let world = app.world_mut();
    let mut values: Vec<f32> = world.query::<&Glow>().iter(world).map(|g| g.intensity).collect();
    values.sort_by(f32::total_cmp);
    values
}

#[test]
fn an_attribute_drives_a_field_on_every_instance_a_generator_spawns() {
    let (mut app, e) = app(true, ProcGenSettings::default());
    assert!(run_now(app.world_mut(), e).is_some());
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(glows(&mut app), vec![0.0, 2.0, 4.0], "position:x of each point");
    let world = app.world_mut();
    let left = world
        .query::<&regia_procgen::prefab_overrides::ProcPrefabOverrides>()
        .iter(world)
        .count();
    assert_eq!(left, 0, "applied overrides are removed");
}

#[test]
fn without_overrides_the_instances_keep_the_prefab_values() {
    let (mut app, e) = app(false, ProcGenSettings::default());
    assert!(run_now(app.world_mut(), e).is_some());
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(glows(&mut app), vec![-1.0, -1.0, -1.0]);
}

#[test]
fn the_threads_setting_picks_the_schedule_and_keeps_the_output() {
    let mut hashes = Vec::new();
    for threads in [None, Some(0), Some(1), Some(2), Some(8)] {
        let (mut app, e) = app(
            true,
            ProcGenSettings {
                threads,
                ..ProcGenSettings::default()
            },
        );
        assert!(run_now(app.world_mut(), e).is_some());
        let st = app.world().get::<ProcGenState>(e).expect("state");
        hashes.push(st.output_hash.expect("hash"));
        if let Some(n) = threads {
            let report = st.report.as_ref().expect("report");
            assert_eq!(report.threads, n.max(1), "the run used the asked schedule");
        }
    }
    assert!(hashes.windows(2).all(|w| w[0] == w[1]), "{hashes:?}");
}

/// Three hand-placed instances of the same prefab whose scene records override `Glow.intensity`
/// with `values` (the scene file's own override record, `docs/formats/scene-files.md`).
fn hand_placed_glows(values: &[f32]) -> Vec<f32> {
    let (mut app, generator) = app(false, ProcGenSettings::default());
    app.world_mut().despawn(generator);
    let glow = std::any::type_name::<Glow>();
    let entities: String = values
        .iter()
        .enumerate()
        .map(|(k, v)| {
            format!(
                r#"(guid: "{}", name: "Lamp {k}", components: {{ "regia_scene::transform::Transform": (translation: ({k}.0, 0.0, 0.0)) }},
                 prefab_instance: (prefab: "asset:{PREFAB}",
                    overrides: [(target: "{ROOT}", component: "{glow}", path: ".intensity", value: {v:?})])),"#,
                Guid(0x5000 + k as u128)
            )
        })
        .collect();
    let version = regia_scene::format::SCENE_FORMAT_VERSION;
    let file = SceneFile::from_ron_str(&format!("(format_version: {version}, entities: [{entities}])"))
        .expect("level");
    regia_scene::spawn::spawn_scene(
        app.world_mut(),
        &file,
        &regia_scene::spawn::SpawnOptions::default(),
    )
    .expect("spawn");
    for _ in 0..3 {
        app.update();
    }
    glows(&mut app)
}

/// ADR-7750 (the ADR-7724 (h) fold): a graph whose `spawn.prefab` uses overrides bakes, the
/// `.pgout` round-trips with the overrides, and the instances a load of the bake spawns carry them,
/// with the same component values as hand-placed instances with the same overrides.
#[test]
fn a_bake_of_a_graph_with_overrides_loads_with_the_overrides() {
    let dir = regia_test_utils::unique_temp_dir("procgen-bake-overrides");
    let settings = || ProcGenSettings {
        bake_dir: Some(dir.clone()),
        ..ProcGenSettings::default()
    };
    let (mut with, e) = app(true, settings());
    with.world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .mode = ProcGenMode::Baked;
    let path = bake(with.world_mut(), e).expect("a graph with overrides bakes");
    assert_eq!(path, bake_path(&dir, Guid(0xabc)));
    let bytes = std::fs::read(&path).expect("read the bake");
    let (header, output) = regia_procgen::pgout::decode(&bytes).expect("the bake decodes");
    assert_eq!(
        regia_procgen::pgout::encode(&header, &output),
        bytes,
        "the bake round-trips byte for byte"
    );
    assert_eq!(output.prefabs.len(), 3);
    assert!(
        output.prefabs.iter().all(|p| p.overrides.len() == 1),
        "every record keeps its override: {:?}",
        output.prefabs
    );

    // A fresh world loads the file (no run) and spawns the instances from it.
    let (mut baked, e) = app(true, settings());
    baked
        .world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .mode = ProcGenMode::Baked;
    regia_procgen::world::load_bake(baked.world_mut(), e).expect("the bake loads");
    for _ in 0..3 {
        baked.update();
    }
    assert!(
        baked
            .world()
            .get::<regia_procgen::world::ProcBakeOrigin>(e)
            .is_some(),
        "the output came from the file"
    );
    let from_bake = glows(&mut baked);
    assert_eq!(from_bake, vec![0.0, 2.0, 4.0], "position:x of each point");
    assert_eq!(
        from_bake,
        hand_placed_glows(&[0.0, 2.0, 4.0]),
        "baked instances match hand-placed instances with the same overrides"
    );

    let (mut without, e) = app(false, settings());
    without
        .world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .mode = ProcGenMode::Baked;
    let path = bake(without.world_mut(), e).expect("a graph without overrides bakes");
    let (_, plain) = regia_procgen::pgout::decode(&std::fs::read(&path).expect("read")).expect("decode");
    assert!(plain.prefabs.iter().all(|p| p.overrides.is_empty()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The instance roots' guids, in guid order, of every prefab instance the generator spawned.
fn instance_guids(app: &mut App) -> Vec<Guid> {
    let world = app.world_mut();
    let mut guids: Vec<Guid> = world
        .query_filtered::<&EntityGuid, With<regia_procgen::world::ProcPrefabRef>>()
        .iter(world)
        .map(|g| g.0)
        .collect();
    guids.sort();
    guids
}

#[test]
fn spawned_prefab_instances_get_guids_derived_from_their_generator() {
    // ADR-4806 refuses to instantiate a prefab under a root without an `EntityGuid`; the generator
    // names each instance from its own guid and the record index (ADR-4991), never randomly.
    let mut runs = Vec::new();
    for _ in 0..2 {
        let (mut app, e) = app(false, ProcGenSettings::default());
        assert!(run_now(app.world_mut(), e).is_some());
        for _ in 0..3 {
            app.update();
        }
        runs.push(instance_guids(&mut app));
    }
    let mut expected: Vec<Guid> = (0..3)
        .map(|i| regia_procgen::world::prefab_instance_guid(Guid(0xabc), i))
        .collect();
    expected.sort();
    assert_eq!(runs[0], expected, "one derived guid per instance");
    assert_eq!(
        runs[0], runs[1],
        "the same generator names its instances the same way"
    );
    let (mut app, e) = app(false, ProcGenSettings::default());
    assert!(run_now(app.world_mut(), e).is_some());
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(
        glows(&mut app).len(),
        3,
        "every instance resolved under its derived root guid"
    );
    assert!(expected.iter().all(|g| !g.is_nil()));
    assert!(expected.windows(2).all(|w| w[0] != w[1]), "distinct per instance");
}

#[test]
fn tile_cells_of_one_generator_get_distinct_guid_scopes() {
    use regia_procgen::tiles::{CellCoord, TileKey, cell_scope};
    let g = Guid(0xabc);
    let keys = [
        TileKey {
            tier: 0,
            cell: CellCoord::new(0, 0),
        },
        TileKey {
            tier: 0,
            cell: CellCoord::new(1, 0),
        },
        TileKey {
            tier: 0,
            cell: CellCoord::new(0, 1),
        },
        TileKey {
            tier: 1,
            cell: CellCoord::new(0, 0),
        },
        TileKey {
            tier: 0,
            cell: CellCoord::new(-1, 0),
        },
    ];
    let scopes: Vec<Guid> = keys.iter().map(|k| cell_scope(g, *k)).collect();
    for (i, a) in scopes.iter().enumerate() {
        assert_ne!(*a, g, "a cell scope is not the generator's own guid");
        assert_eq!(*a, cell_scope(g, keys[i]), "stable");
        for b in &scopes[i + 1..] {
            assert_ne!(a, b, "cells of one generator never share a scope");
        }
    }
    assert_ne!(
        cell_scope(g, keys[0]),
        cell_scope(Guid(0xabd), keys[0]),
        "generators differ"
    );
}
