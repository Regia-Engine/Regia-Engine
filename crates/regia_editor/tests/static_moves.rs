// SPDX-License-Identifier: MPL-2.0
//! Spec 14 A1: the editor warns when a `Static` entity moves at runtime — and only in the
//! play world (the edit world moves statics legitimately while building a scene).

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::agent_ext;
use regia_editor::command::{EditorCommand, SpawnEntities, UndoStack};
use regia_editor::level_io;
use regia_editor::pie::{PieRequest, PieSession};
use regia_editor::selection::Selection;
use regia_editor::viewport::EditorCamera;
use regia_scene::components::Static;
use regia_scene::prelude::{Name, Transform};
use serde_json::json;

/// The edit world the editor binary builds (same shape as the PIE integration tests).
fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<regia_editor::pie::PieStatus>();
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    agent_ext::install(app.world_mut());
    app.update();
    app
}

/// Creates an entity through the editor's own spawn path (undoable, guid-tagged, level-member).
fn create_entity(app: &mut App, name: &str, x: f32) -> Entity {
    let transform = Transform {
        translation: Vec3::new(x, 0.5, 0.0),
        ..Transform::default()
    };
    let mut command =
        SpawnEntities::new_entity(app.world(), Guid::new_random(), name, transform).expect("spawn");
    command.apply(app.world_mut());
    app.world_mut()
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    let ids: Vec<Entity> = app
        .world_mut()
        .iter_entities()
        .map(|entity_ref| entity_ref.id())
        .collect();
    for entity in ids {
        if let Some(found) = app.world().get::<Name>(entity)
            && found.as_str() == name
        {
            return entity;
        }
    }
    panic!("{name} not spawned");
}

/// Drains the queued PIE requests like the editor loop does, then mirrors the status.
fn drain(app: &mut App, session: &mut Option<PieSession>) {
    let requests: Vec<PieRequest> = std::mem::take(
        &mut app
            .world_mut()
            .resource_mut::<regia_editor::pie::PieRequests>()
            .queue,
    );
    for request in requests {
        let _ = regia_editor::pie::apply_request(app.world_mut(), session, request, None);
    }
    regia_editor::pie::sync_status(app.world_mut(), session.as_ref());
}

/// The play-world entity carrying `name`, when one exists.
fn play_entity(session: &mut PieSession, name: &str) -> Option<Entity> {
    let world = session.app.world_mut();
    let mut query = world.query::<(Entity, &Name)>();
    query
        .iter(world)
        .find(|(_, found)| found.as_str() == name)
        .map(|(entity, _)| entity)
}

#[test]
fn spec14_a1_editor_warns_when_a_static_entity_moves_at_runtime() {
    let mut app = edit_app();
    let statue = create_entity(&mut app, "Statue", 0.0);
    app.world_mut().entity_mut(statue).insert(Static);
    let _mover = create_entity(&mut app, "Mover", 2.0);

    // The guard is play-world-only: the edit world never holds its state.
    assert!(
        app.world()
            .get_resource::<regia_editor::static_moves::StaticMoveWarnings>()
            .is_none(),
        "the edit world does not run the Static-move guard"
    );

    // Move the static in the EDIT world: legitimate level building, no warning exists to fire.
    app.world_mut()
        .entity_mut(statue)
        .get_mut::<Transform>()
        .expect("statue transform")
        .translation
        .x += 10.0;

    // Start play: the snapshot carries the marker.
    agent_ext::test_support::play(app.world_mut(), &json!({})).expect("play request");
    let mut session = None;
    drain(&mut app, &mut session);
    let play = session.as_mut().expect("session started");
    let play_statue = play_entity(play, "Statue").expect("statue copied into the play world");
    let play_mover = play_entity(play, "Mover").expect("mover copied into the play world");
    assert!(
        play.app.world().get::<Static>(play_statue).is_some(),
        "the snapshot carries the Static marker"
    );

    let warnings = |session: &PieSession| {
        session
            .app
            .world()
            .resource::<regia_editor::static_moves::StaticMoveWarnings>()
            .count
    };

    // The settled start frame and a held frame: spawning and holding are not moves.
    assert_eq!(warnings(play), 0, "spawning a static is not a move");
    play.frame();
    assert_eq!(warnings(play), 0, "a held pose is quiet");

    // Move the static's transform: one warning after the next frame's propagation.
    play.app
        .world_mut()
        .entity_mut(play_statue)
        .get_mut::<Transform>()
        .expect("play transform")
        .translation
        .x += 1.0;
    play.frame();
    assert_eq!(warnings(play), 1, "one move, one warning");

    // A held pose at the new position stays quiet; the plain mover never warns.
    play.frame();
    assert_eq!(warnings(play), 1, "holding the new pose is quiet");
    play.app
        .world_mut()
        .entity_mut(play_mover)
        .get_mut::<Transform>()
        .expect("mover transform")
        .translation
        .x += 1.0;
    play.frame();
    assert_eq!(warnings(play), 1, "an entity without Static never warns");

    // Every further move of the static is another event.
    play.app
        .world_mut()
        .entity_mut(play_statue)
        .get_mut::<Transform>()
        .expect("play transform")
        .translation
        .x += 1.0;
    play.frame();
    assert_eq!(warnings(play), 2);

    // The recent list names the offender.
    let recent = &play
        .app
        .world()
        .resource::<regia_editor::static_moves::StaticMoveWarnings>()
        .recent;
    assert_eq!(recent.last().map(|event| event.entity.as_str()), Some("Statue"));

    // The agent surface sees the count through the edit-world mirror (the `play` borrow of
    // `session` ended with its last use above).
    drain(&mut app, &mut session);
    let answer = agent_ext::test_support::static_move_warnings(app.world_mut(), &json!({}))
        .expect("static_move_warnings");
    assert_eq!(answer["count"], 2, "the mirrored counter: {answer}");
    let status = agent_ext::test_support::play_state(app.world_mut(), &json!({})).expect("state");
    assert_eq!(status["mode"], "playing");
}
