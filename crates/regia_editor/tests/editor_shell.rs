// SPDX-License-Identifier: MPL-2.0
//! M10 integration: the editor shell's pieces work end to end against one edit world —
//! commands through the agent surface, undo/redo, level save/open, and the "random commands
//! then full undo returns a byte-identical scene" acceptance property (`specs/19` § 8 M10).

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::agent_ext;
use regia_editor::command::{
    AddComponent, DespawnEntities, EditorCommand, RemoveComponent, Reparent, SpawnEntities, UndoStack,
};
use regia_editor::level_io;
use regia_editor::selection::{SelectMode, Selection};
use regia_scene::prelude::{EntityGuid, LevelId, Name, Transform};
use serde_json::json;

/// The edit world the binary builds: scene + editor resources + the `editor.*` surface
/// (without the agent server, which the dispatch unit tests cover).
fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    agent_ext::install(app.world_mut());
    app.update();
    app
}

/// A reflected component the inspector edits.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Marker {
    value: f32,
}

/// Creates an entity through the editor's own spawn path (like File → Create entity).
fn create_entity(app: &mut App, name: &str) -> Entity {
    let mut command = SpawnEntities::new_entity(app.world(), Guid::new_random(), name, Transform::default())
        .expect("build spawn");
    command.apply(app.world_mut());
    app.world_mut()
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    find_by_name(app.world_mut(), name).expect("spawned")
}

fn find_by_name(world: &mut World, name: &str) -> Option<Entity> {
    for entity_ref in world.iter_entities() {
        let entity = entity_ref.id();
        if let Some(found) = world.get::<Name>(entity)
            && found.as_str() == name
        {
            return Some(entity);
        }
    }
    None
}

#[test]
fn create_select_edit_undo_redo_save_and_reopen() {
    let mut app = edit_app();
    app.register_type::<Marker>();

    // Create two entities through the editor surface.
    let rock = create_entity(&mut app, "Rock");
    let _ = create_entity(&mut app, "Tree");

    // Select through the agent surface.
    let selection = agent_ext::test_support::select(
        app.world_mut(),
        &json!({
            "entities": [rock.to_bits()],
            "mode": "set",
        }),
    )
    .expect("select");
    assert_eq!(selection["primary"], rock.to_bits());

    // Gizmo edit through the agent surface, then undo/redo.
    let transform = agent_ext::test_support::set_transform(
        app.world_mut(),
        &json!({
            "entity": rock.to_bits(),
            "translation": [2.0, 1.0, -3.0],
        }),
    )
    .expect("set transform");
    assert!(
        transform["undoable"].as_u64().unwrap_or(0) >= 1,
        "the edit is on the undo stack: {transform}"
    );
    let moved = app.world().get::<Transform>(rock).expect("transform").translation;
    assert_eq!(moved, Vec3::new(2.0, 1.0, -3.0));
    assert!(agent_ext::test_support::undo(app.world_mut()), "undo");
    assert_eq!(
        app.world().get::<Transform>(rock).expect("t").translation,
        Vec3::ZERO,
        "undo restored the transform"
    );
    assert!(agent_ext::test_support::redo(app.world_mut()), "redo");
    assert_eq!(
        app.world().get::<Transform>(rock).expect("t").translation,
        Vec3::new(2.0, 1.0, -3.0)
    );

    // Save the level, reopen it in a fresh world: the entity and its edited transform survive.
    let dir = std::env::temp_dir().join(format!("regia-editor-shell-{}-{}", line!(), std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("edit.level.ron");
    agent_ext::test_support::save_level(
        app.world_mut(),
        &json!({
            "path": path.display().to_string(),
        }),
    )
    .expect("save level");

    let mut reopened = edit_app();
    let open = level_io::open_level_file(reopened.world_mut(), &path).expect("reopen");
    reopened.world_mut().insert_resource(open);
    let restored = find_by_name(reopened.world_mut(), "Rock").expect("rock reopened");
    assert_eq!(
        reopened
            .world()
            .get::<Transform>(restored)
            .expect("t")
            .translation,
        Vec3::new(2.0, 1.0, -3.0),
        "the gizmo edit survived the save/reopen round trip"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn random_command_sequence_full_undo_returns_byte_identical_scene() {
    let mut app = edit_app();
    app.register_type::<Marker>();

    // Seed content: a few entities, one parented.
    let rock = create_entity(&mut app, "Rock");
    let pebble = create_entity(&mut app, "Pebble");
    {
        let mut reparent = Reparent::capture(app.world(), pebble, Some(rock));
        reparent.apply(app.world_mut());
        app.world_mut()
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(reparent));
    }
    // The baseline is "the saved state": fresh stack from here (like a save point).
    app.world_mut().insert_resource(UndoStack::new());
    let baseline = snapshot_text(&mut app);

    // A deterministic "random" command sequence: property edits, gizmo moves, adds, removes,
    // reparents and spawns, interleaved with undos/redis.
    let mut lcg = 0x2545F491u64;
    let mut next = move || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        lcg >> 33
    };
    for i in 0..40 {
        let roll = next() % 7;
        let world = app.world_mut();
        match roll {
            0 | 1 => {
                // Property scrub (merges into one entry like a slider drag).
                for scrub in [0.1f32, 0.2, 0.35] {
                    if let Ok(mut command) = agent_ext::test_support::capture_set_property(
                        world,
                        rock,
                        "Transform",
                        "translation.x",
                        Box::new(scrub),
                    ) {
                        command.apply(world);
                        world
                            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                            .push(Box::new(command));
                    }
                }
            }
            2 => {
                let mut command = agent_ext::test_support::capture_set_transform(
                    world,
                    rock,
                    Transform {
                        translation: Vec3::new(next() as f32 * 0.01, 2.0, 0.0),
                        ..Default::default()
                    },
                );
                command.apply(world);
                world
                    .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                    .push(Box::new(command));
            }
            3 => {
                let mut command = AddComponent::capture(
                    world,
                    pebble,
                    "Marker",
                    Box::new(Marker {
                        value: next() as f32 * 0.001,
                    }),
                )
                .expect("capture add");
                command.apply(world);
                world
                    .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                    .push(Box::new(command));
            }
            4 => {
                let mut command = Reparent::capture(world, pebble, None);
                command.apply(world);
                world
                    .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                    .push(Box::new(command));
            }
            5 => {
                if let Ok(mut command) = SpawnEntities::new_entity(
                    world,
                    Guid::new_random(),
                    &format!("Extra{i}"),
                    Transform::default(),
                ) {
                    command.apply(world);
                    world
                        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                        .push(Box::new(command));
                }
            }
            _ => {
                // Undo/redo pairs in flight — they must not break the deep-undo invariant.
                agent_ext::test_support::undo(world);
                agent_ext::test_support::redo(world);
            }
        }
    }

    // Full undo back to the baseline.
    let depth = app.world().resource::<UndoStack>().undo_depth();
    assert!(depth > 0, "the sequence produced undo entries");
    for _ in 0..depth + 2 {
        agent_ext::test_support::undo(app.world_mut());
    }
    let after = snapshot_text(&mut app);
    assert!(
        after == baseline,
        "full undo must return the byte-identical scene:\n--- baseline\n{baseline}\n--- after\n{after}"
    );

    // And the whole thing redoes back to the final state.
    let redo_depth = app.world().resource::<UndoStack>().redo_depth();
    for _ in 0..redo_depth {
        agent_ext::test_support::redo(app.world_mut());
    }
    assert!(redo_depth > 0);
}

/// `specs/19` § 8 M10, widened (ADR-5615 audit): the random sequence also despawns entities (one with a
/// child and a reflected component) and removes components, and a full undo still returns the
/// byte-identical scene. Commands find their targets by name, so they follow entities that an undo
/// re-spawned.
#[test]
fn random_sequence_with_despawns_and_removals_full_undo_is_byte_identical() {
    fn push(world: &mut World, command: Box<dyn EditorCommand>) {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(command);
    }
    let mut app = edit_app();
    app.register_type::<Marker>();
    let rock = create_entity(&mut app, "Rock");
    let pebble = create_entity(&mut app, "Pebble");
    create_entity(&mut app, "Leaf");
    let leaf = find_by_name(app.world_mut(), "Leaf").expect("leaf");
    {
        let world = app.world_mut();
        let mut reparent = Reparent::capture(world, pebble, Some(rock));
        reparent.apply(world);
        let mut leaf_under = Reparent::capture(world, leaf, Some(pebble));
        leaf_under.apply(world);
        let mut marker = AddComponent::capture(world, pebble, "Marker", Box::new(Marker { value: 0.5 }))
            .expect("capture add");
        marker.apply(world);
    }
    app.world_mut().insert_resource(UndoStack::new());
    let baseline = snapshot_text(&mut app);

    let mut lcg = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        lcg >> 33
    };
    let mut kinds = [0u32; 8];
    for i in 0..60 {
        let roll = (next() % 8) as usize;
        let world = app.world_mut();
        let rock = find_by_name(world, "Rock");
        let pebble = find_by_name(world, "Pebble");
        let applied = match roll {
            0 => rock.is_some_and(|rock| {
                let mut command = agent_ext::test_support::capture_set_transform(
                    world,
                    rock,
                    Transform {
                        translation: Vec3::new(next() as f32 * 0.01, 1.0, 0.0),
                        ..Default::default()
                    },
                );
                command.apply(world);
                push(world, Box::new(command));
                true
            }),
            1 => pebble
                .filter(|p| world.get::<Marker>(*p).is_none())
                .is_some_and(|pebble| {
                    let value = next() as f32 * 0.001;
                    match AddComponent::capture(world, pebble, "Marker", Box::new(Marker { value })) {
                        Ok(mut command) => {
                            command.apply(world);
                            push(world, Box::new(command));
                            true
                        }
                        Err(_) => false,
                    }
                }),
            2 => pebble
                .filter(|p| world.get::<Marker>(*p).is_some())
                .is_some_and(|pebble| match RemoveComponent::capture(world, pebble, "Marker") {
                    Ok(mut command) => {
                        command.apply(world);
                        push(world, Box::new(command));
                        true
                    }
                    Err(_) => false,
                }),
            3 => match SpawnEntities::new_entity(
                world,
                Guid::new_random(),
                &format!("Extra{i}"),
                Transform::default(),
            ) {
                Ok(mut command) => {
                    command.apply(world);
                    push(world, Box::new(command));
                    true
                }
                Err(_) => false,
            },
            4 => (0..i)
                .rev()
                .find_map(|j| find_by_name(world, &format!("Extra{j}")))
                .is_some_and(|extra| {
                    let mut command = DespawnEntities {
                        entities: vec![extra.to_bits()],
                        snapshot: None,
                    };
                    command.apply(world);
                    push(world, Box::new(command));
                    true
                }),
            5 => pebble.is_some_and(|pebble| {
                // Pebble carries Leaf as a child and maybe the Marker: the whole subtree goes.
                let mut command = DespawnEntities {
                    entities: vec![pebble.to_bits()],
                    snapshot: None,
                };
                command.apply(world);
                push(world, Box::new(command));
                true
            }),
            6 => match (rock, pebble) {
                (Some(rock), Some(pebble)) => {
                    let parent = if next() % 2 == 0 { Some(rock) } else { None };
                    let mut command = Reparent::capture(world, pebble, parent);
                    command.apply(world);
                    push(world, Box::new(command));
                    true
                }
                _ => false,
            },
            _ => {
                agent_ext::test_support::undo(world);
                agent_ext::test_support::redo(world);
                true
            }
        };
        if applied {
            kinds[roll] += 1;
        }
    }
    // A fixed tail makes sure every kind ran whatever the rolls were: a removal (on Rock, which is never
    // despawned), a spawn then despawn, and the Pebble subtree despawn if the loop left it alive.
    {
        let world = app.world_mut();
        let rock = find_by_name(world, "Rock").expect("rock is never despawned");
        let mut add =
            AddComponent::capture(world, rock, "Marker", Box::new(Marker { value: 2.0 })).expect("add");
        add.apply(world);
        push(world, Box::new(add));
        let mut remove = RemoveComponent::capture(world, rock, "Marker").expect("remove");
        remove.apply(world);
        push(world, Box::new(remove));
        kinds[2] += 1;
        let mut spawn =
            SpawnEntities::new_entity(world, Guid::new_random(), "ExtraTail", Transform::default())
                .expect("spawn");
        spawn.apply(world);
        push(world, Box::new(spawn));
        let tail = find_by_name(world, "ExtraTail").expect("tail spawned");
        let mut despawn = DespawnEntities {
            entities: vec![tail.to_bits()],
            snapshot: None,
        };
        despawn.apply(world);
        push(world, Box::new(despawn));
        kinds[4] += 1;
        if let Some(pebble) = find_by_name(world, "Pebble") {
            let mut despawn = DespawnEntities {
                entities: vec![pebble.to_bits()],
                snapshot: None,
            };
            despawn.apply(world);
            push(world, Box::new(despawn));
            kinds[5] += 1;
        }
    }
    assert!(
        kinds[2] > 0 && kinds[4] > 0 && kinds[5] > 0,
        "removals and despawns ran: {kinds:?}"
    );

    let depth = app.world().resource::<UndoStack>().undo_depth();
    for _ in 0..depth + 2 {
        agent_ext::test_support::undo(app.world_mut());
    }
    let after = snapshot_text(&mut app);
    assert!(
        after == baseline,
        "full undo must return the byte-identical scene:\n--- baseline\n{baseline}\n--- after\n{after}"
    );
}

/// The canonical RON of the whole world (level snapshot shape).
fn snapshot_text(app: &mut App) -> String {
    let filter = regia_scene::spawn::SnapshotFilter {
        level: None,
        resources: vec!["regia_scene::level::LevelSettings".to_string()],
    };
    let file = regia_scene::prelude::snapshot_scene(app.world_mut(), &filter).expect("snapshot");
    file.to_ron_string().expect("ron")
}

/// The undo/redo of despawn/respawn keeps guids stable (outliner identity).
#[test]
fn despawn_undo_keeps_guid_identity() {
    let mut app = edit_app();
    let rock = create_entity(&mut app, "Rock");
    let guid = app.world().get::<EntityGuid>(rock).expect("spawned with guid").0;

    let mut command = DespawnEntities {
        entities: vec![rock.to_bits()],
        snapshot: None,
    };
    command.apply(app.world_mut());
    app.world_mut()
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    assert!(app.world().get_entity(rock).is_err());
    agent_ext::test_support::undo(app.world_mut());
    let restored = find_by_name(app.world_mut(), "Rock").expect("restored");
    assert_eq!(
        app.world().get::<EntityGuid>(restored).expect("guid").0,
        guid,
        "the same stable guid came back"
    );
}

/// Multi-select through the agent surface drives the same selection the panels use.
#[test]
fn agent_selection_matches_panel_selection() {
    let mut app = edit_app();
    let a = create_entity(&mut app, "A");
    let b = create_entity(&mut app, "B");

    agent_ext::test_support::select(
        app.world_mut(),
        &json!({
            "entities": [a.to_bits()],
            "mode": "add",
        }),
    )
    .expect("add a");
    agent_ext::test_support::select(
        app.world_mut(),
        &json!({
            "entities": [b.to_bits()],
            "mode": "add",
        }),
    )
    .expect("add b");

    let selection = app.world().resource::<Selection>().clone();
    assert_eq!(selection.entities(), &[a, b]);
    assert_eq!(selection.primary(), Some(b), "primary is the last added");
    assert_eq!(
        selection.len(),
        2,
        "the panels and the agent share one selection resource"
    );
    let _ = SelectMode::Replace;
    let _ = LevelId(Guid::new_random());
}

/// A steady editor frame does not walk the level's entities for identity and bookkeeping, yet an
/// entity an agent spawns gets its guid on the next frame and a bookkeeping entity is hidden
/// (ADR-7326).
#[test]
fn a_steady_frame_visits_no_entity_and_new_entities_still_get_identity() {
    use regia_editor::visibility::{EditorHidden, identity_walk_visits};
    let mut app = edit_app();
    let entities: Vec<_> = (0..20_000)
        .map(|i| {
            (
                Name::new(format!("Rock {i}")),
                EntityGuid::new_random(),
                Transform::from_translation(Vec3::new(i as f32, 0.0, 0.0)),
            )
        })
        .collect();
    app.world_mut().spawn_batch(entities);
    let ctx = egui::Context::default();
    let mut shell = regia_editor::shell::EditorShell::new(None, None);
    let mut frame = |app: &mut App| {
        let world = app.world_mut();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            shell.frame_effects(ui.ctx(), world);
        });
        output.textures_delta.clear();
    };
    // The first frames settle: names are copied between the two `Name` components.
    frame(&mut app);
    frame(&mut app);
    let before = identity_walk_visits();
    frame(&mut app);
    assert_eq!(
        identity_walk_visits() - before,
        0,
        "a steady frame of a 20 000-entity level visits no entity"
    );

    // What `scene.spawn` inserts for a named entity with a pose: the ECS `Name` and a
    // `Transform`, no guid. A protocol bookkeeping entity carries nothing.
    let spawned = app
        .world_mut()
        .spawn((regia_ecs::ecs::name::Name::new("Spawned"), Transform::default()))
        .id();
    let bookkeeping = app.world_mut().spawn_empty().id();
    frame(&mut app);
    assert!(
        app.world().get::<EntityGuid>(spawned).is_some(),
        "the spawned entity has scene identity after one frame"
    );
    assert_eq!(
        app.world().get::<Name>(spawned).map(|n| n.as_str().to_string()),
        Some("Spawned".to_string()),
        "and the scene name the snapshot writes"
    );
    assert!(
        app.world().get::<EditorHidden>(bookkeeping).is_some(),
        "bookkeeping is hidden"
    );
    assert!(app.world().get::<EditorHidden>(spawned).is_none());

    frame(&mut app);
    let before = identity_walk_visits();
    frame(&mut app);
    assert_eq!(identity_walk_visits() - before, 0, "steady again");
}
