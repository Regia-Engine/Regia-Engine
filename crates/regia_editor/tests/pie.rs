// SPDX-License-Identifier: MPL-2.0
//! M11 integration: play in editor end to end — agent `editor.play`/`pause`/`stop` requests,
//! the play-world copy, and the "stop restores the edit world byte-identically" acceptance
//! (`docs/specs/19-editor.md` § 8 M11, § 4).

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::agent_ext;
use regia_editor::command::{EditorCommand, SpawnEntities, UndoStack};
use regia_editor::level_io;
use regia_editor::pie::{PieRequest, PieSession, PlayMode};
use regia_editor::selection::Selection;
use regia_editor::viewport::EditorCamera;
use regia_scene::level::LEVEL_SETTINGS_PATH;
use regia_scene::prelude::{Name, Transform};
use regia_scene::spawn::SnapshotFilter;
use serde_json::json;

/// The edit world the editor binary builds (scene + editor resources + the `editor.*` surface).
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

/// The canonical snapshot text of the whole edit world (the same shape `editor.save_level` and
/// PIE use).
fn snapshot_text(app: &mut App) -> String {
    let filter = SnapshotFilter {
        level: None,
        resources: vec![LEVEL_SETTINGS_PATH.to_string()],
    };
    let file = regia_scene::prelude::snapshot_scene(app.world_mut(), &filter).expect("snapshot");
    file.to_ron_string().expect("ron")
}

/// Drains the queued PIE requests like the editor loop does, stepping the session it owns.
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

/// The names in the session's play world.
fn play_names(session: &mut PieSession) -> Vec<String> {
    let world = session.app.world_mut();
    world
        .query::<&Name>()
        .iter(world)
        .map(|n| n.as_str().to_string())
        .collect()
}

#[test]
fn agent_play_pause_stop_cycle_restores_the_edit_world_byte_identically() {
    let mut app = edit_app();
    let _ground = create_entity(&mut app, "Ground", 0.0);
    let rock = create_entity(&mut app, "Rock", 2.0);

    // Select the rock: play must clear the selection (edit ids are meaningless in the play
    // world), and stop must not bring play-world residue back.
    agent_ext::test_support::select(
        app.world_mut(),
        &json!({ "entities": [rock.to_bits()], "mode": "set" }),
    )
    .expect("select");

    let before = snapshot_text(&mut app);

    // The agent surface: editor.play queues, the loop applies, play_state reports.
    let queued = agent_ext::test_support::play(app.world_mut(), &json!({})).expect("play request");
    assert_eq!(queued["queued"], true);
    let mut session = None;
    drain(&mut app, &mut session);
    let status = agent_ext::test_support::play_state(app.world_mut(), &json!({})).expect("state");
    assert_eq!(status["mode"], "playing");
    assert_eq!(status["entities"], 2, "the play world holds the copy: {status}");
    // No wall-clock bound (see blank_project.rs / level_build.rs): assert play reached a
    // settled frame instead of measuring real time under parallel test load.
    assert!(
        status["frames"].as_u64().unwrap_or(0) >= 1,
        "PIE has not run a settled frame: {status}"
    );
    assert!(
        app.world().resource::<Selection>().is_empty(),
        "play cleared the edit selection"
    );

    // The play world really contains the copied entities, by name.
    let names = session.as_mut().map(play_names).unwrap_or_default();
    assert!(
        names.contains(&"Rock".to_string()) && names.contains(&"Ground".to_string()),
        "{names:?}"
    );

    // Simulate: the copy advances; the edit world does not.
    {
        let session = session.as_mut().expect("session");
        session.frame();
        session.frame();
    }
    assert_eq!(
        session.as_ref().expect("session").frames,
        3,
        "one settled frame plus two simulated"
    );

    // Pause / step through the agent surface.
    agent_ext::test_support::pause(app.world_mut(), &json!({})).expect("pause request");
    drain(&mut app, &mut session);
    assert_eq!(session.as_ref().expect("session").mode, PlayMode::Paused);
    agent_ext::test_support::step(app.world_mut(), &json!({ "frames": 2 })).expect("step request");
    drain(&mut app, &mut session);
    assert_eq!(session.as_ref().expect("session").frames, 5, "two stepped frames");

    // Edits during play target the play world and are discarded on stop.
    session
        .as_mut()
        .expect("session")
        .app
        .world_mut()
        .spawn((Name::new("PlayOnly"), Transform::default()));

    // Stop through the agent surface.
    agent_ext::test_support::stop(app.world_mut(), &json!({})).expect("stop request");
    drain(&mut app, &mut session);
    assert!(session.is_none(), "the session ended");
    let status = agent_ext::test_support::play_state(app.world_mut(), &json!({})).expect("state");
    assert_eq!(status["mode"], "stopped");

    let after = snapshot_text(&mut app);
    assert!(
        before == after,
        "stop restored the edit world byte-identically:\n--- before\n{before}\n--- after\n{after}"
    );
    // The play-only entity never leaked into the edit world.
    assert!(
        !after.contains("PlayOnly"),
        "play-world edits are discarded: {after}"
    );
}

#[test]
fn play_picks_up_placed_content_without_restart() {
    // The place → play loop: an entity placed after the level was last played shows up in the
    // next play world (import → place → play without restarting, `specs/19` § 8 M11).
    let mut app = edit_app();
    let _rock = create_entity(&mut app, "Rock", 1.0);

    // A "placed asset": the content-browser drop path spawns through the same command.
    let _placed = create_entity(&mut app, "PlacedMesh", -1.0);

    let queued = agent_ext::test_support::play(app.world_mut(), &json!({})).expect("play request");
    assert_eq!(queued["queued"], true);
    let mut session = None;
    drain(&mut app, &mut session);
    let names = session.as_mut().map(play_names).unwrap_or_default();
    assert!(
        names.contains(&"PlacedMesh".to_string()),
        "the newly placed asset is in the play copy: {names:?}"
    );
    if let Some(session) = session.take() {
        regia_editor::pie::stop_play(session);
    }
}
