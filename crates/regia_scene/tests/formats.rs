// SPDX-License-Identifier: MPL-2.0
//! End-to-end checks of `specs/14` § M6 acceptance: round trips for levels with prefabs,
//! overrides and nested prefabs; the 10,000-entity load budget; reparenting that preserves the
//! world transform — exercised through the published format files and the [`ScenePlugin`].

use std::time::Instant;

use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::world::CommandQueue;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::Name;
use regia_scene::cooked::CookedScene;
use regia_scene::level::{LevelId, LevelLibrary, LevelManager, LevelState, LoadMode};
use regia_scene::prefab::{PrefabLibrary, PrefabLink, PrefabTemplate};
use regia_scene::prelude::*;
use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};

/// Manifest dir, so the published example files load from a `cargo test` run in any cwd.
fn docs_formats_path(file: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/formats")
        .join(file)
}

/// The published examples parse, re-save byte-identically and survive postcard.
#[test]
fn published_example_files_parse_and_round_trip() {
    for file in ["level.example.ron", "prefab.example.ron"] {
        let text = std::fs::read_to_string(docs_formats_path(file))
            .unwrap_or_else(|err| panic!("read docs/formats/{file}: {err}"));
        let parsed = SceneFile::from_ron_str(&text).unwrap_or_else(|err| panic!("{file}: {err}"));
        assert_eq!(
            parsed.format_version,
            regia_scene::format::SCENE_FORMAT_VERSION,
            "{file}"
        );
        let rewritten = parsed.to_ron_string().expect("serialize");
        let reparsed = SceneFile::from_ron_str(&rewritten).expect("reparse");
        assert_eq!(reparsed, parsed, "{file}: reparse must be equal");
        assert_eq!(
            reparsed.to_ron_string().expect("re-serialize"),
            rewritten,
            "{file}: canonical form is stable"
        );

        let bytes = parsed.to_postcard().expect("postcard");
        let back = SceneFile::from_postcard(&bytes).expect("unpostcard");
        assert_eq!(back, parsed, "{file}: postcard round trip");
    }
}

/// Test component standing in for a game component with a numeric field to override. Its type
/// path in generated file text is `formats::LampIntensity` (this test crate).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct LampIntensity {
    /// Candela.
    pub intensity: f32,
}

const LAMP_INTENSITY_PATH: &str = "formats::LampIntensity";

/// A world with the scene types and the test component registered (what `ScenePlugin` plus a
/// game plugin would set up).
fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
        registry.register::<regia_scene::components::Tags>();
        registry.register::<smallvec::SmallVec<[Name; 4]>>();
        registry.register::<LampIntensity>();
    }
    world.init_resource::<PrefabLibrary>();
    world
}

/// The inner prefab text: one root plus a light entity (local id `light_id`).
fn lamp_prefab_text(root: Guid, light: Guid) -> String {
    format!(
        r#"
(
    format_version: 1,
    entities: [
        (guid: "{root}", name: "Lamp", components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0)),
        }}),
        (guid: "{light}", name: "Light", parent: Some("{root}"), components: {{
            "{LAMP_INTENSITY_PATH}": (intensity: 100.0),
        }}),
    ],
)
"#
    )
}

/// The entity carrying `local`'s [`PrefabLink`].
fn find_by_local(world: &mut World, local: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &PrefabLink)>()
        .iter(world)
        .find(|(_, link)| link.guid() == local)
        .map(|(entity, _)| entity)
}

/// The entity with prefab local id `local` inside the instance whose root (or an ancestor) is named
/// `instance`. Two instances of one prefab share every local id, and query order follows archetype
/// creation, so [`find_by_local`] alone picks whichever instance the query yields first.
fn find_by_local_in(world: &mut World, local: Guid, instance: &str) -> Option<Entity> {
    let candidates: Vec<Entity> = world
        .query::<(Entity, &PrefabLink)>()
        .iter(world)
        .filter(|(_, link)| link.guid() == local)
        .map(|(entity, _)| entity)
        .collect();
    candidates.into_iter().find(|&entity| {
        let mut cursor = Some(entity);
        // Bounded walk up the parents: a test hierarchy is a few levels deep.
        for _ in 0..64 {
            let Some(at) = cursor else { return false };
            if world
                .get::<Name>(at)
                .is_some_and(|name| name.as_str() == instance)
            {
                return true;
            }
            cursor = world.get::<ChildOf>(at).map(|link| link.parent());
        }
        false
    })
}

/// Bakes `inner_file` under `inner_asset` into `world`'s library.
fn bake(world: &mut World, asset: Guid, file: &SceneFile) {
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let template = PrefabTemplate::from_scene_file(asset, file, &registry.read()).expect("bake");
    world.resource_mut::<PrefabLibrary>().insert(template);
}

/// A level file: a plain entity and two prefab instances — one with a field override and a
/// whole-component override, one untouched.
fn two_lamps_level_text(prefab_asset: Guid, light_id: Guid) -> String {
    let level_root = Guid::new_random();
    format!(
        r#"
(
    format_version: 1,
    entities: [
        (guid: "{level_root}", name: "Level Origin", components: {{
            "regia_scene::transform::Transform": (),
        }}),
        (guid: "{}", name: "Lamp One", parent: Some("{level_root}"), prefab_instance: (
            prefab: "asset:{prefab_asset}",
            overrides: [
                (target: "{light_id}", component: "{LAMP_INTENSITY_PATH}", path: ".intensity", value: 800.0),
                (target: "{light_id}", component: "regia_scene::components::Tags", path: ".", value: (["recycled"])),
            ],
        )),
        (guid: "{}", name: "Lamp Two", parent: Some("{level_root}"), prefab_instance: (
            prefab: "asset:{prefab_asset}",
        )),
    ],
)
"#,
        Guid::new_random(),
        Guid::new_random(),
    )
}

/// specs/14 M6: "round-trip tests for levels with prefabs/overrides" — spawn a level with two
/// prefab instances (one overridden), snapshot it, respawn the snapshot in a fresh world and
/// snapshot again: identical files, identical content.
#[test]
fn level_with_prefab_instance_and_overrides_round_trips() {
    let mut world = test_world();
    let prefab_asset = Guid::new_random();
    let prefab_root = Guid::new_random();
    let light_id = Guid::new_random();
    let prefab_file =
        SceneFile::from_ron_str(&lamp_prefab_text(prefab_root, light_id)).expect("parse prefab");
    bake(&mut world, prefab_asset, &prefab_file);

    let level = SceneFile::from_ron_str(&two_lamps_level_text(prefab_asset, light_id)).expect("parse level");
    let spawned = spawn_scene(&mut world, &level, &SpawnOptions::default()).expect("spawn");
    assert_eq!(spawned.entities, 7, "3 scene entities + 2×(prefab root + light)");
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);

    let light_one = find_by_local_in(&mut world, light_id, "Lamp One").expect("light one");
    assert_eq!(
        world
            .get::<LampIntensity>(light_one)
            .expect("intensity")
            .intensity,
        800.0,
        "field override landed"
    );
    let lights_at_baked = world
        .query::<(Entity, &LampIntensity)>()
        .iter(&world)
        .filter(|(_, light)| light.intensity == 100.0)
        .count();
    assert_eq!(lights_at_baked, 1, "the untouched instance keeps the baked value");
    let light_one_tags = world
        .get::<regia_scene::components::Tags>(light_one)
        .expect("tags");
    assert!(
        light_one_tags.contains("recycled"),
        "whole-component override landed"
    );

    // Snapshot → respawn → snapshot: byte-identical files, identical content.
    let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
    let text = snapshot.to_ron_string().expect("write");
    assert_eq!(
        snapshot.entities.len(),
        3,
        "prefab-internal entities are re-instantiated from the template, not written"
    );

    let mut world2 = test_world();
    bake(&mut world2, prefab_asset, &prefab_file);
    spawn_scene(&mut world2, &snapshot, &SpawnOptions::default()).expect("respawn");
    let snapshot2 = snapshot_scene(&mut world2, &SnapshotFilter::default()).expect("snapshot 2");
    assert_eq!(
        snapshot2.to_ron_string().expect("write 2"),
        text,
        "round trip is stable"
    );
    // ...and the override really survived into world 2 (not just the file text).
    let light_one_2 = find_by_local_in(&mut world2, light_id, "Lamp One").expect("light one in world 2");
    assert_eq!(
        world2
            .get::<LampIntensity>(light_one_2)
            .expect("intensity")
            .intensity,
        800.0
    );
}

/// specs/14 M6: "round-trip tests for levels with ... nested prefabs". A prefab containing a
/// prefab instance; the level instantiates the outer one and overrides the inner light again —
/// outer wins (specs/14 A2: "overrides compose outer-wins"). The round trip re-bakes both
/// templates, so the composed value survives.
#[test]
fn nested_prefab_level_round_trips_outer_wins() {
    let mut world = test_world();
    let inner_asset = Guid::new_random();
    let inner_root = Guid::new_random();
    let inner_light = Guid::new_random();
    let inner_file =
        SceneFile::from_ron_str(&lamp_prefab_text(inner_root, inner_light)).expect("parse inner");
    bake(&mut world, inner_asset, &inner_file);

    let outer_asset = Guid::new_random();
    let outer_root = Guid::new_random();
    let outer_text = |inner_asset: Guid| {
        format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{outer_root}", name: "Room", prefab_instance: (
            prefab: "asset:{inner_asset}",
            overrides: [
                (target: "{inner_light}", component: "{LAMP_INTENSITY_PATH}", path: ".intensity", value: 42.0),
            ],
        )),
    ],
)
"#
        )
    };
    let outer_file = SceneFile::from_ron_str(&outer_text(inner_asset)).expect("parse outer");
    bake(&mut world, outer_asset, &outer_file);

    // The level instantiates the OUTER prefab and overrides the inner light once more.
    let level = SceneFile::from_ron_str(&format!(
        r#"
(
    format_version: 1,
    entities: [
        (guid: "{}", name: "Yard", prefab_instance: (
            prefab: "asset:{outer_asset}",
            overrides: [
                (target: "{inner_light}", component: "{LAMP_INTENSITY_PATH}", path: ".intensity", value: 7.0),
            ],
        )),
    ],
)
"#,
        Guid::new_random(),
    ))
    .expect("parse level");
    let spawned = spawn_scene(&mut world, &level, &SpawnOptions::default()).expect("spawn");
    assert_eq!(
        spawned.entities, 4,
        "Yard + outer Room + inner lamp root + inner light"
    );
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);

    let light = find_by_local(&mut world, inner_light).expect("inner light");
    assert_eq!(
        world.get::<LampIntensity>(light).expect("intensity").intensity,
        7.0,
        "the outermost override wins"
    );

    // Round trip: snapshot (the whole instance collapses into its authored record), respawn in a
    // fresh world with both templates re-baked, same composed value again.
    let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
    assert_eq!(
        snapshot.entities.len(),
        1,
        "only the level's instance root is written"
    );
    let mut world2 = test_world();
    let inner_asset2 = Guid::new_random();
    bake(&mut world2, inner_asset2, &inner_file);
    bake(
        &mut world2,
        outer_asset,
        &SceneFile::from_ron_str(&outer_text(inner_asset2)).expect("parse outer 2"),
    );
    spawn_scene(&mut world2, &snapshot, &SpawnOptions::default()).expect("respawn");
    let light2 = find_by_local(&mut world2, inner_light).expect("inner light");
    assert_eq!(
        world2.get::<LampIntensity>(light2).expect("intensity").intensity,
        7.0,
        "outer-most override survives the round trip"
    );
}

/// specs/14 A2: "load speed target: 10,000 entities < 50 ms on desktop" — measured on the
/// cooked (postcard, string-tabled) runtime path. The text path is authoring-only; its time is
/// printed for information.
#[test]
fn ten_thousand_entity_level_loads_within_budget() {
    const COUNT: u32 = 10_000;
    let mut world = test_world();
    let mut file = SceneFile::new();
    for index in 0..COUNT {
        let guid = Guid::from_u128(u128::from(index) + 1);
        let mut components = std::collections::BTreeMap::new();
        components.insert(
            "regia_scene::transform::Transform".to_owned(),
            ron::value::RawValue::from_ron(&format!(
                "(translation: ({:.1}, 0.0, {:.1}))",
                (index % 100) as f32,
                (index / 100) as f32
            ))
            .expect("ron")
            .to_owned(),
        );
        components.insert(
            LAMP_INTENSITY_PATH.to_owned(),
            ron::value::RawValue::from_ron("(intensity: 50.0)")
                .expect("ron")
                .to_owned(),
        );
        file.entities.push(regia_scene::format::SceneEntity {
            guid,
            name: Some(format!("Prop {index}")),
            parent: (index % 20 != 0).then(|| Guid::from_u128(u128::from(index - index % 20) + 1)),
            components,
            prefab_instance: None,
        });
    }
    file.validate().expect("generated level is valid");

    // Authoring path: text parse + spawn (what the editor and the agent's `scene.load` do).
    let text = file.to_ron_string().expect("write");
    let start = Instant::now();
    let parsed = SceneFile::from_ron_str(&text).expect("parse text");
    let text_spawned = spawn_scene(&mut world, &parsed, &SpawnOptions::default()).expect("spawn text");
    let text_ms = start.elapsed().as_secs_f64() * 1000.0;

    // Runtime path: cook once, then measure parse + spawn of the cooked bytes.
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let (cooked, warnings) = CookedScene::cook(&parsed, &registry.read()).expect("cook");
    assert!(warnings.is_empty(), "{warnings:?}");
    let bytes = cooked.to_bytes().expect("bytes");
    // Best of three cooked loads (a fresh world each): one deschedule in a parallel debug run used
    // to exceed the budget with a single timing (audit-v035-flaky L3).
    let mut cooked_ms = f64::MAX;
    let mut cooked_spawned = None;
    for _ in 0..3 {
        let mut world2 = test_world();
        let start = Instant::now();
        let cooked_back = CookedScene::from_bytes(&bytes).expect("parse cooked");
        let spawned = cooked_back
            .spawn(&mut world2, &SpawnOptions::default())
            .expect("spawn cooked");
        cooked_ms = cooked_ms.min(start.elapsed().as_secs_f64() * 1000.0);
        cooked_spawned = Some(spawned);
    }
    let cooked_spawned = cooked_spawned.expect("three runs");

    assert_eq!(text_spawned.entities, COUNT as usize);
    assert_eq!(cooked_spawned.entities, COUNT as usize);
    assert!(
        cooked_spawned.warnings.is_empty(),
        "{:?}",
        cooked_spawned.warnings
    );
    // The budget is a desktop (release) number: enforced strictly in release builds, with a
    // debug sanity margin (≈5×) so a pathological regression still fails in CI's debug runs.
    let budget_ms = if cfg!(debug_assertions) { 250.0 } else { 50.0 };
    // The text path's budget, same release/debug split: a 10k-entity map must open in the
    // editor within a second (the whole-document `ron` parse took 50–70 s release, 70–80 s debug).
    let text_budget_ms = if cfg!(debug_assertions) { 5000.0 } else { 1000.0 };
    println!(
        "10k entities: text parse+spawn {text_ms:.1} ms (budget {text_budget_ms:.0} ms); \
         cooked parse+spawn {cooked_ms:.1} ms (budget {budget_ms:.0} ms)"
    );
    assert!(
        cooked_ms < budget_ms,
        "cooked load of {COUNT} entities took {cooked_ms:.1} ms; budget is {budget_ms:.0} ms (specs/14 A2)"
    );
    assert!(
        text_ms < text_budget_ms,
        "text load of {COUNT} entities took {text_ms:.1} ms; budget is {text_budget_ms:.0} ms"
    );
}

/// specs/14 M6: "reparenting preserves world transform when requested" — through the plugin's
/// schedules (propagation runs in `PostUpdate/TransformPropagate`).
#[test]
fn reparenting_through_the_app_preserves_world_transform() {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let a = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)))
        .id();
    let b = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(0.0, 50.0, 0.0)))
        .id();
    let child = app.world_mut().spawn(Transform::from_translation(Vec3::ONE)).id();
    app.update();
    let before = app
        .world()
        .get::<regia_scene::GlobalTransform>(child)
        .expect("global")
        .translation();
    assert_eq!(before, Vec3::ONE, "child is a root before reparenting");

    {
        let mut queue = CommandQueue::default();
        let mut commands = Commands::new(&mut queue, app.world());
        commands.entity(child).set_parent_keep_world_transform(a);
        queue.apply(app.world_mut());
    }
    app.update();
    let after = app
        .world()
        .get::<regia_scene::GlobalTransform>(child)
        .expect("global")
        .translation();
    assert!(
        (after - before).length() < 1e-3,
        "world transform must be preserved: {after:?} vs {before:?}"
    );
    assert_eq!(app.world().get::<ChildOf>(child).expect("parented").parent(), a);
    let _ = b;
}

/// `LevelManager` load/unload through the plugin's systems: open a level whose data arrives
/// late, unload it again (specs/14 A4 basics).
#[test]
fn level_lifecycle_through_the_app() {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let level_id = LevelId(Guid::new_random());
    {
        let world = app.world_mut();
        regia_scene::level::open_level(world, level_id, LoadMode::Replace).expect("open");
        assert_eq!(
            world.resource::<LevelManager>().state(level_id),
            Some(LevelState::Loading)
        );
    }
    let root = Guid::new_random();
    let child = Guid::new_random();
    let level = regia_scene::level::Level::from_ron_str(
        level_id,
        &format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{root}", name: "Origin", components: {{
            "regia_scene::transform::Transform": (),
        }}),
        (guid: "{child}", name: "Prop", parent: Some("{root}"), components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0)),
        }}),
    ],
)
"#
        ),
    )
    .expect("parse");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    app.update();

    {
        let world = app.world_mut();
        assert_eq!(world.resource::<LevelManager>().current(), Some(level_id));
        assert_eq!(
            world.resource::<LevelManager>().state(level_id),
            Some(LevelState::Active)
        );
        assert!(
            world
                .query::<(Entity, &EntityGuid)>()
                .iter(world)
                .any(|(_, g)| g.0 == root),
            "level spawned"
        );
    }

    regia_scene::level::unload_level(app.world_mut(), level_id).expect("unload");
    app.update();
    let world = app.world_mut();
    assert_eq!(world.resource::<LevelManager>().current(), None);
    assert!(
        !world
            .query::<(Entity, &EntityGuid)>()
            .iter(world)
            .any(|(_, g)| g.0 == root || g.0 == child),
        "level despawned"
    );
}
