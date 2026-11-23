// SPDX-License-Identifier: MPL-2.0
//! A `Baked` generator that comes from a placed level loads the bake beside *that* level (spec 62 § 8,
//! ADR-7262): `regia procgen bake` writes `<sub>.procgen/<source guid>.pgout` when it bakes the
//! sublevel, while the game runs with the bake directory of the level it opened. An inline placement
//! derives the member's guid from the placement (spec 37 § 6), a streamed one keeps the source guid
//! and tags the member with `LevelMember`; either way the loader finds the sublevel's directory in
//! `ProcLevelBakeDirs` and the source guid. Headless app with the scene and generator plugins, fixed
//! frame counts, bakes on disk under a unique temp dir.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_procgen::exec::{Collision, Instance, InstanceSet, RunOutput};
use regia_procgen::pgout::{PgoutHeader, encode};
use regia_procgen::world::{
    ProcBakeOrigin, ProcBakeSource, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator,
    ProcGraphSource, ProcLevelBakeDirs, ProcOutput, level_bake_dir,
};
use regia_scene::components::EntityGuid;
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::placed::{PlacedLevel, PlacedMode, derive_guid};
use regia_scene::spawn::{SnapshotFilter, snapshot_scene};
use regia_scene::{GlobalTransform, Transform};

const MAIN: LevelId = LevelId(Guid(0x3a1));
const SUB: LevelId = LevelId(Guid(0x5b2));
const PLACEMENT: Guid = Guid(0x91ac);
const SUB_GEN: Guid = Guid(0x6e6);

fn bake(count: usize) -> Vec<u8> {
    let instances = (0..count)
        .map(|k| Instance {
            translation: [k as f32, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            color: [1.0; 4],
            custom: [0.0; 4],
        })
        .collect();
    let output = RunOutput {
        instance_sets: vec![InstanceSet {
            mesh: "rock.glb".into(),
            material: None,
            cast_shadows: true,
            collision: Collision::None,
            draw_distance_m: 100.0,
            fade_m: 0.0,
            cull_below_px: 2.0,
            instances,
            layer: None,
        }],
        ..RunOutput::default()
    };
    let header = PgoutHeader {
        graph_hash: [1; 32],
        param_hash: [2; 32],
        seed: 3,
        input_hash: [4; 32],
        output_hash: output.hash(),
        quality: 5,
    };
    encode(&header, &output)
}

/// The records of a scratch world holding `spawn`'s entities.
fn records(spawn: impl FnOnce(&mut World)) -> regia_scene::format::SceneFile {
    let mut scratch = App::new();
    scratch.add_plugins(regia_scene::ScenePlugin);
    scratch.add_plugins(ProcGenPlugin);
    spawn(scratch.world_mut());
    snapshot_scene(scratch.world_mut(), &SnapshotFilter::default()).expect("snapshot")
}

/// `<root>/levels/main.level.ron` places `<root>/levels/sub.level.ron` (`mode`), whose one `Baked`
/// generator has a 3-instance bake beside the sublevel. With `register`, the sublevel's bake
/// directory is in `ProcLevelBakeDirs`.
fn app(label: &str, mode: PlacedMode, register: bool) -> (App, PathBuf) {
    let root = regia_test_utils::unique_temp_dir(label);
    let levels = root.join("levels");
    let sub_bakes = level_bake_dir(&levels.join("sub.level.ron"));
    std::fs::create_dir_all(&sub_bakes).expect("sub bake dir");
    std::fs::write(sub_bakes.join(format!("{SUB_GEN}.pgout")), bake(3)).expect("sub bake");

    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::new()),
        bake_dir: Some(level_bake_dir(&levels.join("main.level.ron"))),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    if register {
        app.world_mut()
            .resource_mut::<ProcLevelBakeDirs>()
            .0
            .insert(SUB, sub_bakes.clone());
    }
    let sub = records(|w| {
        w.spawn((
            ProcGenerator::new("g.pgraph.ron"),
            EntityGuid(SUB_GEN),
            Transform::default(),
            GlobalTransform::default(),
        ));
    });
    let main = records(|w| {
        w.spawn((
            PlacedLevel {
                mode,
                ..PlacedLevel::inline(SUB)
            },
            EntityGuid(PLACEMENT),
            Transform::default(),
            GlobalTransform::default(),
        ));
    });
    {
        let mut library = app.world_mut().resource_mut::<LevelLibrary>();
        library.insert(Level::from_scene_file(SUB, sub));
        library.insert(Level::from_scene_file(MAIN, main));
    }
    open_level(app.world_mut(), MAIN, LoadMode::Replace).expect("open");
    for _ in 0..5 {
        app.update();
    }
    (app, sub_bakes)
}

fn generators(app: &mut App) -> Vec<(Entity, Guid)> {
    let world = app.world_mut();
    world
        .query::<(Entity, &EntityGuid, &ProcGenerator)>()
        .iter(world)
        .map(|(e, g, _)| (e, g.0))
        .collect()
}

fn check_loaded(app: &mut App, sub_bakes: &Path, expect_guid: Guid) {
    let gens = generators(app);
    assert_eq!(gens.len(), 1, "the sublevel's generator spawned: {gens:?}");
    let (generator, guid) = gens[0];
    assert_eq!(guid, expect_guid);
    let world = app.world();
    let output = world
        .get::<ProcOutput>(generator)
        .expect("the sublevel's bake loaded");
    assert_eq!(output.0.instance_count(), 3);
    assert_eq!(
        world.get::<ProcGenState>(generator).map(|s| s.evaluations),
        Some(0),
        "loaded, not run"
    );
    let origin = world
        .get::<ProcBakeOrigin>(generator)
        .expect("the load names where the bake came from");
    assert_eq!(origin.level, Some(SUB));
    assert_eq!(origin.source, ProcBakeSource::Disk);
    assert_eq!(origin.path, sub_bakes.join(format!("{SUB_GEN}.pgout")));
}

#[test]
fn an_inline_placed_generator_loads_the_sublevels_bake() {
    let (mut app, sub_bakes) = app("placed-bakes-inline", PlacedMode::Inline, true);
    check_loaded(&mut app, &sub_bakes, derive_guid(PLACEMENT, SUB_GEN));
}

#[test]
fn a_streamed_placed_generator_loads_the_sublevels_bake() {
    let (mut app, sub_bakes) = app("placed-bakes-streamed", PlacedMode::Streamed, true);
    check_loaded(&mut app, &sub_bakes, SUB_GEN);
}

/// Guard: a level whose directory nobody registered is not guessed; the generator stays without
/// output (`procgen_bake_missing`), as before.
#[test]
fn an_unregistered_sublevel_is_not_guessed() {
    let (mut app, _) = app("placed-bakes-unregistered", PlacedMode::Inline, false);
    let gens = generators(&mut app);
    assert_eq!(gens.len(), 1);
    assert!(app.world().get::<ProcOutput>(gens[0].0).is_none());
    assert!(app.world().get::<ProcBakeOrigin>(gens[0].0).is_none());
}

/// The host level's own bake of a placed member (the editor baked the main level with the
/// placement filled, under the derived guid) still wins over the sublevel's.
#[test]
fn the_host_levels_bake_of_a_member_wins() {
    let root = regia_test_utils::unique_temp_dir("placed-bakes-host-wins");
    let main_bakes = level_bake_dir(&root.join("levels/main.level.ron"));
    std::fs::create_dir_all(&main_bakes).expect("main bake dir");
    let derived = derive_guid(PLACEMENT, SUB_GEN);
    let host_file = main_bakes.join(format!("{derived}.pgout"));
    std::fs::write(&host_file, bake(5)).expect("host bake");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::new()),
        bake_dir: Some(main_bakes.clone()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    let generator = app
        .world_mut()
        .spawn((ProcGenerator::new("g.pgraph.ron"), EntityGuid(derived)))
        .id();
    for _ in 0..3 {
        app.update();
    }
    let output = app.world().get::<ProcOutput>(generator).expect("loaded");
    assert_eq!(output.0.instance_count(), 5);
    let origin = app.world().get::<ProcBakeOrigin>(generator).expect("origin");
    assert_eq!(origin.level, None, "the primary bake directory");
    assert_eq!(origin.path, host_file);
}

// ------------------------------------------------------------------------------------------------
// ADR-7287: anchored (streamed-cell) generators and prefab-added members of a placed level
// ------------------------------------------------------------------------------------------------

mod common;

const TILES_GEN: Guid = Guid(0x7c7);
const PREFAB_ASSET: Guid = Guid(0xa55e7);
const PREFAB_ROOT: Guid = Guid(0xa0);
const PREFAB_INST: Guid = Guid(0x1a57);
const ADDED_GEN: Guid = Guid(0xadd);

/// `in.self → sample.region → points.jitter → spawn.mesh`: enough points that every 64 m cell
/// holds some.
fn scatter_text() -> String {
    use common::{graph, node};
    graph(
        vec![
            (1, node("in.self", &[])),
            (
                2,
                node("sample.region", &[("spacing_m", "5.0"), ("jitter", "0.6")]),
            ),
            (
                3,
                node(
                    "points.jitter",
                    &[("scale", "[(1.0, 1.0, 1.0), (2.0, 1.0, 2.0)]")],
                ),
            ),
            (4, node("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")])),
        ],
        &[(1, "out", 2, "region"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    )
    .to_ron()
}

fn graphs() -> BTreeMap<String, String> {
    BTreeMap::from([("s.pgraph.ron".to_string(), scatter_text())])
}

/// A `Baked` generator that streams its cells around tagged anchors (spec 66 § 4.3).
fn tiled_generator() -> ProcGenerator {
    use regia_core::math::Vec3;
    use regia_procgen::tiles::{AnchorSettings, AnchorSource, TileSettings};
    use regia_procgen::world::ProcBounds;
    ProcGenerator {
        bounds: ProcBounds::Box {
            half_extents: Vec3::new(128.0, 10.0, 128.0),
        },
        seed: Some(11),
        tiles: Some(TileSettings {
            cell_m: 64.0,
            runtime: Some(AnchorSettings {
                anchors: AnchorSource::Tagged,
                base_reach_m: 80.0,
                ..AnchorSettings::default()
            }),
            ..TileSettings::default()
        }),
        ..ProcGenerator::new("s.pgraph.ron")
    }
}

/// Bakes the tiled generator under `guid` into `dir`, as `regia procgen bake --level sub` does.
fn bake_cells(dir: &Path, guid: Guid) -> PathBuf {
    let mut scratch = App::new();
    scratch.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(graphs()),
        bake_dir: Some(dir.to_path_buf()),
        ..ProcGenSettings::default()
    });
    scratch.add_plugins(ProcGenPlugin);
    let e = scratch
        .world_mut()
        .spawn((tiled_generator(), EntityGuid(guid), Transform::default()))
        .id();
    regia_procgen::world::bake(scratch.world_mut(), e).unwrap_or_else(|(c, m)| panic!("bake: {c}: {m}"))
}

/// `main` places `sub` (`mode`); `sub` holds the tiled generator `TILES_GEN`, whose cell bake is
/// beside `sub`. An anchor at the origin pulls cells. With `host_bake`, the main level's directory
/// also has a cell bake under the member's guid.
fn tiles_app(label: &str, mode: PlacedMode, host_bake: bool) -> (App, PathBuf, PathBuf) {
    let root = regia_test_utils::unique_temp_dir(label);
    let levels = root.join("levels");
    let sub_bakes = level_bake_dir(&levels.join("sub.level.ron"));
    let main_bakes = level_bake_dir(&levels.join("main.level.ron"));
    let sub_file = bake_cells(&sub_bakes, TILES_GEN);
    let member = match mode {
        PlacedMode::Inline => derive_guid(PLACEMENT, TILES_GEN),
        PlacedMode::Streamed => TILES_GEN,
    };
    let host_file = host_bake.then(|| bake_cells(&main_bakes, member));

    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(graphs()),
        bake_dir: Some(main_bakes),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.world_mut()
        .resource_mut::<ProcLevelBakeDirs>()
        .0
        .insert(SUB, sub_bakes);
    let sub = records(|w| {
        w.spawn((
            tiled_generator(),
            EntityGuid(TILES_GEN),
            Transform::default(),
            GlobalTransform::default(),
        ));
    });
    let main = records(|w| {
        w.spawn((
            PlacedLevel {
                mode,
                ..PlacedLevel::inline(SUB)
            },
            EntityGuid(PLACEMENT),
            Transform::default(),
            GlobalTransform::default(),
        ));
    });
    {
        let mut library = app.world_mut().resource_mut::<LevelLibrary>();
        library.insert(Level::from_scene_file(SUB, sub));
        library.insert(Level::from_scene_file(MAIN, main));
    }
    open_level(app.world_mut(), MAIN, LoadMode::Replace).expect("open");
    app.world_mut().spawn((
        regia_procgen::tiles::ProcGenAnchor::default(),
        Transform::default(),
        GlobalTransform::default(),
    ));
    for _ in 0..8 {
        app.update();
    }
    (app, sub_file, host_file.unwrap_or_default())
}

/// The anchored generator streamed cells from `file` (no error, cells loaded, nothing evaluated)
/// and names where they came from.
fn check_cells(app: &mut App, expect_guid: Guid, file: &Path, level: Option<LevelId>) {
    use regia_procgen::tiles::ProcGenTiles;
    let gens = generators(app);
    assert_eq!(gens.len(), 1, "the sublevel's generator spawned: {gens:?}");
    let (generator, guid) = gens[0];
    assert_eq!(guid, expect_guid);
    let world = app.world();
    let tiles = world.resource::<ProcGenTiles>();
    assert_eq!(
        tiles.errors.get(&generator),
        None,
        "the generator's cell bake was found"
    );
    let loaded = tiles
        .generators
        .get(&generator)
        .map_or(0, |g| g.counters.cells_loaded);
    assert!(loaded > 0, "cells streamed from the bake: {loaded}");
    let state = world.get::<ProcGenState>(generator);
    assert_eq!(state.map(|s| s.evaluations), Some(0), "loaded, not run");
    assert!(!state.is_some_and(|s| s.stale), "the bake matches the graph");
    let origin = world
        .get::<ProcBakeOrigin>(generator)
        .expect("the cell bake names where it came from");
    assert_eq!(origin.level, level);
    assert_eq!(origin.source, ProcBakeSource::Disk);
    assert_eq!(origin.path, file);
}

/// Acceptance (ADR-7287 (1)): an inline placement's anchored generator streams its cells from the
/// sublevel's bake under its source guid.
#[test]
fn an_inline_placed_anchored_generator_streams_the_sublevels_cells() {
    let (mut app, sub_file, _) = tiles_app("placed-bakes-tiles-inline", PlacedMode::Inline, false);
    check_cells(&mut app, derive_guid(PLACEMENT, TILES_GEN), &sub_file, Some(SUB));
}

/// Acceptance (ADR-7287 (1)): a streamed placement (a sublevel loaded additively) does the same.
#[test]
fn a_streamed_placed_anchored_generator_streams_the_sublevels_cells() {
    let (mut app, sub_file, _) = tiles_app("placed-bakes-tiles-streamed", PlacedMode::Streamed, false);
    check_cells(&mut app, TILES_GEN, &sub_file, Some(SUB));
}

/// The rule (ADR-7287): the host level's own cell bake of the member wins over the sublevel's.
#[test]
fn the_host_levels_cell_bake_of_a_member_wins() {
    let (mut app, _, host_file) = tiles_app("placed-bakes-tiles-host", PlacedMode::Inline, true);
    check_cells(&mut app, derive_guid(PLACEMENT, TILES_GEN), &host_file, None);
}

/// `main` places `sub` inline; `sub` holds an instance of a one-entity prefab whose
/// `added_children` carry the `Baked` generator `ADDED_GEN`, with a 3-instance bake beside `sub`
/// under that guid.
fn prefab_added_app(label: &str) -> (App, PathBuf) {
    use regia_scene::format::{PrefabInstanceData, SceneAssetRef, SceneEntity};
    use regia_scene::prefab::{PrefabLibrary, PrefabTemplate};
    let root = regia_test_utils::unique_temp_dir(label);
    let levels = root.join("levels");
    let sub_bakes = level_bake_dir(&levels.join("sub.level.ron"));
    std::fs::create_dir_all(&sub_bakes).expect("sub bake dir");
    let sub_file = sub_bakes.join(format!("{ADDED_GEN}.pgout"));
    std::fs::write(&sub_file, bake(3)).expect("sub bake");

    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::new()),
        bake_dir: Some(level_bake_dir(&levels.join("main.level.ron"))),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.world_mut()
        .resource_mut::<ProcLevelBakeDirs>()
        .0
        .insert(SUB, sub_bakes);
    let prefab = records(|w| {
        w.spawn((EntityGuid(PREFAB_ROOT), Transform::default()));
    });
    let template = {
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .0
            .clone();
        let registry = registry.read();
        PrefabTemplate::from_scene_file(PREFAB_ASSET, &prefab, &registry).expect("template")
    };
    app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
    let mut added = records(|w| {
        w.spawn((
            ProcGenerator::new("g.pgraph.ron"),
            EntityGuid(ADDED_GEN),
            Transform::default(),
        ));
    })
    .entities;
    assert_eq!(added.len(), 1);
    let mut sub = regia_scene::format::SceneFile::new();
    sub.entities.push(SceneEntity {
        guid: PREFAB_INST,
        prefab_instance: Some(PrefabInstanceData {
            added_children: std::mem::take(&mut added),
            ..PrefabInstanceData::plain(SceneAssetRef(PREFAB_ASSET))
        }),
        ..SceneEntity::default()
    });
    let main = records(|w| {
        w.spawn((
            PlacedLevel::inline(SUB),
            EntityGuid(PLACEMENT),
            Transform::default(),
            GlobalTransform::default(),
        ));
    });
    {
        let mut library = app.world_mut().resource_mut::<LevelLibrary>();
        library.insert(Level::from_scene_file(SUB, sub));
        library.insert(Level::from_scene_file(MAIN, main));
    }
    open_level(app.world_mut(), MAIN, LoadMode::Replace).expect("open");
    for _ in 0..5 {
        app.update();
    }
    (app, sub_file)
}

/// Acceptance (ADR-7287 (2)): a generator a prefab's `added_children` brought into the sublevel,
/// placed inline (guid `derive_guid(placement, ADDED_GEN)`), loads the sublevel's bake written
/// under `ADDED_GEN`.
#[test]
fn a_prefab_added_generator_placed_inline_loads_the_sublevels_bake() {
    let (mut app, sub_file) = prefab_added_app("placed-bakes-prefab-added");
    let gens = generators(&mut app);
    assert_eq!(gens.len(), 1, "the added generator spawned: {gens:?}");
    let (generator, guid) = gens[0];
    assert_eq!(guid, derive_guid(PLACEMENT, ADDED_GEN));
    let world = app.world();
    let output = world
        .get::<ProcOutput>(generator)
        .expect("the sublevel's bake of the added generator loaded");
    assert_eq!(output.0.instance_count(), 3);
    let origin = world.get::<ProcBakeOrigin>(generator).expect("origin");
    assert_eq!(origin.level, Some(SUB));
    assert_eq!(origin.path, sub_file);
}

/// Found while building the fixtures above (ADR-7287 (5)): an anchored generator's
/// `ProcGenerator` survives `snapshot_scene` (the editor's level save). `AnchorSettings::
/// quality_reach` is a `[f32; 4]`, which has no reflected clone, so the snapshot dropped the
/// whole component with a warning and the saved level held the entity without its generator.
#[test]
fn an_anchored_generator_survives_a_scene_snapshot() {
    let scene = records(|w| {
        w.spawn((tiled_generator(), EntityGuid(TILES_GEN), Transform::default()));
    });
    let record = scene
        .entities
        .iter()
        .find(|r| r.guid == TILES_GEN)
        .expect("the record");
    let types: Vec<&String> = record.components.keys().collect();
    assert!(
        record
            .components
            .contains_key("regia_procgen::world::ProcGenerator"),
        "the generator is saved: {types:?}"
    );
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_plugins(ProcGenPlugin);
    regia_scene::spawn::spawn_scene(app.world_mut(), &scene, &Default::default()).expect("spawn");
    let world = app.world_mut();
    let loaded: Vec<ProcGenerator> = world.query::<&ProcGenerator>().iter(world).cloned().collect();
    assert_eq!(
        loaded,
        vec![tiled_generator()],
        "the anchored generator round-trips"
    );
}

// ------------------------------------------------------------------------------------------------
// ADR-7331: generators inside a prefab template, placed through a level
// ------------------------------------------------------------------------------------------------

const TPL_ASSET: Guid = Guid(0x7e3a);
const TPL_ROOT: Guid = Guid(0x7e30);
const TPL_GEN: Guid = Guid(0x7e31);
const TPL_INST: Guid = Guid(0x7e1a);
const NEST_ASSET: Guid = Guid(0x7e3b);
const NEST_ROOT: Guid = Guid(0x7e40);

/// The prefab `TPL_ASSET`: a root with a child holding a `Baked` generator (prefab internals).
fn template_with_generator(app: &App) -> regia_scene::prefab::PrefabTemplate {
    use regia_scene::prefab::PrefabTemplate;
    let prefab = records(|w| {
        let root = w.spawn((EntityGuid(TPL_ROOT), Transform::default())).id();
        w.spawn((
            ProcGenerator::new("g.pgraph.ron"),
            EntityGuid(TPL_GEN),
            Transform::default(),
            ChildOf(root),
        ));
    });
    let registry = app
        .world()
        .resource::<regia_ecs::reflect::AppTypeRegistry>()
        .0
        .clone();
    let registry = registry.read();
    PrefabTemplate::from_scene_file(TPL_ASSET, &prefab, &registry).expect("template")
}

/// The prefab `NEST_ASSET`: one entity `NEST_ROOT` that instantiates `TPL_ASSET` (a nested prefab).
fn nesting_template(app: &App) -> regia_scene::prefab::PrefabTemplate {
    use regia_scene::format::{PrefabInstanceData, SceneAssetRef, SceneEntity};
    use regia_scene::prefab::PrefabTemplate;
    let mut scene = regia_scene::format::SceneFile::new();
    scene.entities.push(SceneEntity {
        guid: NEST_ROOT,
        prefab_instance: Some(PrefabInstanceData::plain(SceneAssetRef(TPL_ASSET))),
        ..SceneEntity::default()
    });
    let registry = app
        .world()
        .resource::<regia_ecs::reflect::AppTypeRegistry>()
        .0
        .clone();
    let registry = registry.read();
    PrefabTemplate::from_scene_file(NEST_ASSET, &scene, &registry).expect("nesting template")
}

/// Registers the template prefab (and, `nested`, the prefab nesting it) in `app`'s library.
fn install_templates(app: &mut App, nested: bool) {
    use regia_scene::prefab::PrefabLibrary;
    let template = template_with_generator(app);
    app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
    if nested {
        let outer = nesting_template(app);
        app.world_mut().resource_mut::<PrefabLibrary>().insert(outer);
    }
}

/// The sublevel: one instance `TPL_INST` of the prefab (`nested`: of the prefab nesting it).
fn sub_with_template_instance(nested: bool) -> regia_scene::format::SceneFile {
    use regia_scene::format::{PrefabInstanceData, SceneAssetRef, SceneEntity};
    let asset = if nested { NEST_ASSET } else { TPL_ASSET };
    let mut sub = regia_scene::format::SceneFile::new();
    sub.entities.push(SceneEntity {
        guid: TPL_INST,
        prefab_instance: Some(PrefabInstanceData::plain(SceneAssetRef(asset))),
        ..SceneEntity::default()
    });
    sub
}

/// The guid `regia procgen bake --level sub` names the template generator's bake with: the
/// sublevel opened on its own, the generator's `EntityGuid`.
fn template_generator_guid_in_sub(nested: bool) -> Guid {
    let mut scratch = App::new();
    scratch.add_plugins(regia_scene::ScenePlugin);
    scratch.add_plugins(ProcGenPlugin);
    install_templates(&mut scratch, nested);
    regia_scene::spawn::spawn_scene(
        scratch.world_mut(),
        &sub_with_template_instance(nested),
        &Default::default(),
    )
    .expect("spawn sub");
    let gens = generators(&mut scratch);
    assert_eq!(
        gens.len(),
        1,
        "the template's generator spawned in the sublevel: {gens:?}"
    );
    gens[0].1
}

/// `main` places `sub` (`mode`); `sub` holds an instance of a prefab whose template holds the
/// `Baked` generator; its 3-instance bake is beside `sub` under the guid the sublevel gives it.
fn template_app(label: &str, mode: PlacedMode, nested: bool) -> (App, PathBuf, Guid) {
    let source = template_generator_guid_in_sub(nested);
    let root = regia_test_utils::unique_temp_dir(label);
    let levels = root.join("levels");
    let sub_bakes = level_bake_dir(&levels.join("sub.level.ron"));
    std::fs::create_dir_all(&sub_bakes).expect("sub bake dir");
    let sub_file = sub_bakes.join(format!("{source}.pgout"));
    std::fs::write(&sub_file, bake(3)).expect("sub bake");

    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::new()),
        bake_dir: Some(level_bake_dir(&levels.join("main.level.ron"))),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.world_mut()
        .resource_mut::<ProcLevelBakeDirs>()
        .0
        .insert(SUB, sub_bakes);
    install_templates(&mut app, nested);
    let main = records(|w| {
        w.spawn((
            PlacedLevel {
                mode,
                ..PlacedLevel::inline(SUB)
            },
            EntityGuid(PLACEMENT),
            Transform::default(),
            GlobalTransform::default(),
        ));
    });
    {
        let mut library = app.world_mut().resource_mut::<LevelLibrary>();
        library.insert(Level::from_scene_file(SUB, sub_with_template_instance(nested)));
        library.insert(Level::from_scene_file(MAIN, main));
    }
    open_level(app.world_mut(), MAIN, LoadMode::Replace).expect("open");
    for _ in 0..5 {
        app.update();
    }
    (app, sub_file, source)
}

fn check_template_loaded(app: &mut App, sub_file: &Path) {
    let gens = generators(app);
    assert_eq!(gens.len(), 1, "the template generator spawned: {gens:?}");
    let generator = gens[0].0;
    let world = app.world();
    let output = world
        .get::<ProcOutput>(generator)
        .expect("the sublevel's bake of the template generator loaded");
    assert_eq!(output.0.instance_count(), 3);
    assert_eq!(
        world.get::<ProcGenState>(generator).map(|s| s.evaluations),
        Some(0),
        "loaded, not run"
    );
    let origin = world.get::<ProcBakeOrigin>(generator).expect("origin");
    assert_eq!(origin.level, Some(SUB));
    assert_eq!(origin.source, ProcBakeSource::Disk);
    assert_eq!(origin.path, sub_file);
}

/// Acceptance (ADR-7331 (1)): a generator inside a prefab template (a prefab-internal entity, not an
/// added child) of an inline-placed sublevel maps back through the prefab instance's guid to the
/// guid the sublevel gives it, and loads the sublevel's bake without evaluating.
#[test]
fn a_prefab_template_generator_placed_inline_loads_the_sublevels_bake() {
    let (mut app, sub_file, source) = template_app("placed-bakes-template-inline", PlacedMode::Inline, false);
    let member = generators(&mut app)[0].1;
    assert_ne!(member, source, "the placement derives the member's guid");
    check_template_loaded(&mut app, &sub_file);
}

/// Guard: a streamed placement keeps the sublevel's guids, so its template generator finds the
/// same file.
#[test]
fn a_prefab_template_generator_placed_streamed_loads_the_sublevels_bake() {
    let (mut app, sub_file, source) =
        template_app("placed-bakes-template-streamed", PlacedMode::Streamed, false);
    assert_eq!(generators(&mut app)[0].1, source);
    check_template_loaded(&mut app, &sub_file);
}

/// ADR-7331 (1), nested: the generator sits in a prefab nested inside the sublevel's prefab
/// template; the walk maps it back through the inner instance root, then the outer one.
#[test]
fn a_nested_prefab_template_generator_placed_inline_loads_the_sublevels_bake() {
    let (mut app, sub_file, source) = template_app("placed-bakes-template-nested", PlacedMode::Inline, true);
    assert_ne!(generators(&mut app)[0].1, source);
    check_template_loaded(&mut app, &sub_file);
}
