// SPDX-License-Identifier: MPL-2.0
//! Tests of the incremental level autosave (ADR-7748): counted, never timed.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use regia_app::prelude::App;
use regia_scene::prelude::{Name, Transform};

use super::*;
use crate::command::EditorCommand as _;
use crate::doc_autosave::{self, TickInput};
use crate::level_io::OpenLevel;

const FLUSH: Duration = Duration::from_secs(120);
const ENTITIES: u64 = 20_000;

fn temp_project(label: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(&format!("autosave-incremental-{label}"));
    std::fs::create_dir_all(dir.join("assets")).expect("dir");
    dir
}

/// An edit app with a `count`-entity level open and dirty (one spawn on the undo stack).
fn level_app(count: u64) -> (App, OpenLevel, Vec<Entity>) {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    let open = crate::level_io::new_level(world);
    world.insert_resource(open.clone());
    let mut undo = crate::command::UndoStack::new();
    let mut entities = Vec::new();
    for i in 0..count {
        let mut spawn = crate::command::SpawnEntities::new_entity(
            world,
            regia_core::id::Guid::from_u128(0x1000_0000 + u128::from(i)),
            &format!("Rock{i}"),
            Transform::from_translation(regia_core::math::Vec3::new(i as f32, 0.0, 0.0)),
        )
        .expect("spawn");
        spawn.apply(world);
        if i == 0 {
            undo.push(Box::new(spawn));
        }
    }
    world.insert_resource(undo);
    let mut query = world.query_filtered::<Entity, With<Name>>();
    entities.extend(query.iter(world));
    entities.sort();
    (app, open, entities)
}

fn tick(world: &mut World, root: &std::path::Path, at: u64) -> doc_autosave::TickReport {
    doc_autosave::tick(
        world,
        TickInput {
            project_root: root,
            now: Instant::now() + Duration::from_secs(600 * at),
            force: true,
        },
    )
}

/// The full snapshot's file (the old path: every entity reflected, `autosave::write_autosave`).
fn full_autosave_bytes(world: &mut World, open: &OpenLevel) -> Vec<u8> {
    let other = temp_project("full");
    let path = crate::autosave::write_autosave(world, open, &other).expect("full autosave");
    let bytes = std::fs::read(&path).expect("read full");
    let _ = std::fs::remove_dir_all(&other);
    bytes
}

/// Acceptance (brief w11 item 1): on a 20 000-entity level, after one edit the autosave tick
/// reflects one entity on the main thread (the rest come from the cache), and the autosave file,
/// and the level restored from it, are byte-identical to a full snapshot's.
#[test]
fn one_edit_on_a_20k_level_reflects_one_entity_and_matches_a_full_snapshot() {
    let root = temp_project("one-edit");
    let (mut app, open, entities) = level_app(ENTITIES);
    let first = tick(app.world_mut(), &root, 1);
    assert!(first.queued && first.level.is_some(), "{first:?}");
    let stats = last_stats(app.world()).expect("a snapshot ran");
    assert_eq!(
        (stats.full, stats.reflected, stats.reused),
        (true, ENTITIES, 0),
        "the first tick reflects every entity"
    );
    doc_autosave::flush(app.world_mut(), FLUSH);
    // Frames between the ticks: the scene's systems (transform propagation) run and change no
    // saved component.
    app.update();
    app.update();
    let world = app.world_mut();

    // One edit, the way the Inspector or the gizmo makes it.
    let edited = entities[ENTITIES as usize / 2];
    world
        .get_mut::<Transform>(edited)
        .expect("transform")
        .translation
        .y = 42.5;

    let second = tick(world, &root, 2);
    assert!(second.queued, "{second:?}");
    let stats = last_stats(world).expect("a snapshot ran");
    assert_eq!(
        (stats.full, stats.reflected, stats.reused),
        (false, 1, ENTITIES - 1),
        "after one edit only the edited entity is reflected on the main thread"
    );
    doc_autosave::flush(world, FLUSH);
    let path = second.level.expect("level autosave");
    let incremental = std::fs::read(&path).expect("read autosave");
    let full = full_autosave_bytes(world, &open);
    assert!(
        incremental == full,
        "the incremental autosave is byte-identical to a full snapshot ({} vs {} bytes)",
        incremental.len(),
        full.len()
    );
    assert!(
        String::from_utf8_lossy(&incremental).contains("42.5"),
        "and carries the edit"
    );

    // The agent surface reports the same counts (`editor.autosave_state`'s `level_snapshot`).
    let (_, handler) = crate::autosave_agent::methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.autosave_state")
        .expect("editor.autosave_state");
    let state = handler(world, &serde_json::json!({})).expect("autosave_state");
    assert_eq!(
        state["level_snapshot"],
        serde_json::json!({ "reflected": 1, "reused": ENTITIES - 1, "full": false })
    );

    // Restoring either gives the same level.
    let restored_text = |bytes: &[u8]| {
        let level =
            regia_scene::level::Level::from_ron_str(open.id, std::str::from_utf8(bytes).expect("utf8"))
                .expect("parse autosave");
        level.to_ron_string().expect("encode")
    };
    assert_eq!(restored_text(&incremental), restored_text(&full));
    let _ = std::fs::remove_dir_all(&root);
}

/// Structural edits are seen too: a component added or removed (another archetype), an entity
/// spawned or despawned, a rename, a reparent and the level's settings all reach the incremental
/// snapshot, which stays byte-identical to a full one after each.
#[test]
fn structural_edits_reach_the_incremental_snapshot() {
    let root = temp_project("structural");
    let (mut app, open, entities) = level_app(64);
    let world = app.world_mut();
    let _ = tick(world, &root, 1);
    doc_autosave::flush(world, FLUSH);

    let check = |world: &mut World, at: u64, what: &str, most: u64| {
        let report = tick(world, &root, at);
        doc_autosave::flush(world, FLUSH);
        let stats = last_stats(world).expect("snapshot");
        assert!(
            stats.reflected <= most,
            "{what}: {} reflected, at most {most} expected",
            stats.reflected
        );
        let incremental = std::fs::read(report.level.expect("level")).expect("read");
        let full = full_autosave_bytes(world, &open);
        assert!(
            incremental == full,
            "{what}: the incremental snapshot differs from a full one"
        );
    };

    // A component added (a new archetype).
    world
        .entity_mut(entities[2])
        .insert(regia_scene::components::Visibility::Hidden);
    check(world, 2, "component added", 1);
    // ... and removed again.
    world
        .entity_mut(entities[2])
        .remove::<regia_scene::components::Visibility>();
    check(world, 3, "component removed", 1);
    // A rename (structural field, read fresh) reflects nothing.
    *world.get_mut::<Name>(entities[3]).expect("name") = Name::new("Renamed");
    check(world, 4, "rename", 0);
    // A reparent: the link is read fresh, but both the child (`ChildOf`) and the new parent
    // (`Children`) move to another archetype, so both are reflected again.
    world.entity_mut(entities[4]).insert(ChildOf(entities[5]));
    check(world, 5, "reparent", 2);
    // A despawn and a spawn.
    world.despawn(entities[6]);
    let mut spawn = crate::command::SpawnEntities::new_entity(
        world,
        regia_core::id::Guid::from_u128(0xFEED),
        "New",
        Transform::default(),
    )
    .expect("spawn");
    spawn.apply(world);
    check(world, 6, "despawn and spawn", 1);
    // Level settings are a resource, read fresh every tick.
    if let Some(mut settings) = world.get_resource_mut::<regia_scene::level::LevelSettings>() {
        settings.game_rules = Some("Changed".to_string());
    }
    check(world, 7, "settings", 0);
    let _ = std::fs::remove_dir_all(&root);
}

/// The cache starts over when the open level changes or when the type registry gains a type,
/// and is pruned of despawned entities.
#[test]
fn the_cache_starts_over_for_another_level_or_a_new_type() {
    let (mut app, open, entities) = level_app(16);
    let world = app.world_mut();
    let mut cache = LevelSnapshotCache::default();
    let full = |world: &mut World, cache: &mut LevelSnapshotCache, level| {
        let _ = snapshot_level_through(world, level, cache).expect("snapshot");
        cache.last_stats()
    };
    assert!(full(world, &mut cache, open.id).full);
    let again = full(world, &mut cache, open.id);
    assert_eq!((again.full, again.reflected, again.reused), (false, 0, 16));
    world.despawn(entities[0]);
    let _ = full(world, &mut cache, open.id);
    assert_eq!(cache.len(), 15, "a despawned entity's entry is pruned");

    // Another type registered: a full snapshot.
    #[derive(regia_ecs::reflect::Reflect, Default)]
    struct LateType;
    world.resource::<AppTypeRegistry>().write().register::<LateType>();
    assert!(full(world, &mut cache, open.id).full, "a new type starts over");
    // Another level: a full snapshot.
    let other = crate::level_io::new_level(world);
    assert!(
        full(world, &mut cache, other.id).full,
        "another level starts over"
    );
}

/// Measurement (ADR-7748): the main-thread cost of an autosave tick on a 20 000-entity level
/// after one edit, the full snapshot (ADR-9770's path) against the incremental one, interleaved.
/// `cargo test -p regia_editor --lib autosave_incremental_cost_20k -- --ignored --nocapture`
#[test]
#[ignore = "measurement; prints numbers"]
fn autosave_incremental_cost_20k_entities() {
    let root = temp_project("cost");
    let (mut app, open, entities) = level_app(ENTITIES);
    let world = app.world_mut();
    let _ = tick(world, &root, 1);
    doc_autosave::flush(world, FLUSH);
    let mut full = Vec::new();
    let mut incremental = Vec::new();
    for round in 0..7u64 {
        let edited = entities[round as usize * 97];
        world
            .get_mut::<Transform>(edited)
            .expect("transform")
            .translation
            .z += 1.0;
        // Full: the snapshot ADR-9770's tick took on the main thread.
        let t = Instant::now();
        let level = crate::seq_tracks::with_preview_lifted(world, |world| {
            crate::level_io::snapshot_saved_level(world, open.id)
        })
        .expect("full snapshot");
        full.push(t.elapsed().as_micros());
        drop(level);
        // Incremental: the whole main-thread tick.
        let report = tick(world, &root, round + 2);
        incremental.push(u128::from(report.main_thread_us));
        doc_autosave::flush(world, FLUSH);
    }
    full.sort_unstable();
    incremental.sort_unstable();
    println!(
        "autosave tick, {ENTITIES} entities, one edit, main thread: full snapshot median {} us (min {}), incremental tick median {} us (min {})",
        full[3], full[0], incremental[3], incremental[0]
    );
    let _ = std::fs::remove_dir_all(&root);
}
