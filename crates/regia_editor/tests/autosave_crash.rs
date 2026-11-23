// SPDX-License-Identifier: MPL-2.0
//! Crash recovery end to end (`docs/specs/19-editor.md` § 7, ADR-9770): a child editor process
//! opens a fixture project, edits the level and a material graph, waits for one autosave tick
//! (interval from the preference, time stepped, no sleep) and aborts with
//! `std::process::abort`. The relaunch offers both, and Restore gives documents byte-identical
//! to the child's in-memory state before the crash. Also the agent surface of autosave and
//! recovery through the dispatcher.
//!
//! The child is this test binary run again on `autosave_crash_child` with the project in
//! `REGIA_AUTOSAVE_CRASH_PROJECT` (the `pie_fixture` pattern without a separate binary).

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::command::{EditorCommand as _, SpawnEntities, UndoStack};
use regia_editor::doc_autosave::{self, RecoveryOffers, TickInput};
use regia_editor::level_io::{self, OpenLevel};
use regia_editor::matgraph_editor::MatGraphEditor;
use serde_json::{Value, json};

const CHILD_ENV: &str = "REGIA_AUTOSAVE_CRASH_PROJECT";
const FLUSH: Duration = Duration::from_secs(120);

/// A project with one level and one material graph.
fn fixture_project(tag: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets/maps")).unwrap();
    std::fs::create_dir_all(dir.join("assets/materials")).unwrap();
    std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"crash\"\n").unwrap();
    std::fs::write(
        dir.join("assets/maps/main.level.ron"),
        "( format_version: 1, entities: [] )",
    )
    .unwrap();
    let graph = regia_matgraph::MaterialGraph::new("rock");
    std::fs::write(
        dir.join("assets/materials/rock.matgraph.ron"),
        graph.to_ron().unwrap(),
    )
    .unwrap();
    // The files are older than anything the child writes (no reliance on clock granularity).
    for file in ["assets/maps/main.level.ron", "assets/materials/rock.matgraph.ron"] {
        std::fs::File::options()
            .write(true)
            .open(dir.join(file))
            .and_then(|f| f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000)))
            .unwrap();
    }
    dir
}

fn editor_world(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<regia_editor::selection::Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
    regia_editor::agent_ext::install(app.world_mut());
    doc_autosave::install(app.world_mut());
    app
}

/// The level's text as a save writes it.
fn level_text(world: &mut World) -> String {
    let open = world.resource::<OpenLevel>().clone();
    regia_scene::level::snapshot_level(world, open.id)
        .unwrap()
        .to_ron_string()
        .unwrap()
}

/// The material graph tabs' autosave bytes (the registry's snapshot).
fn matgraph_bytes(world: &World) -> Vec<u8> {
    let snapshots: Vec<_> = doc_autosave::take_snapshots(world)
        .into_iter()
        .filter(|s| s.key.kind == "matgraph")
        .collect();
    assert_eq!(snapshots.len(), 1, "one dirty material graph");
    snapshots[0].bytes.clone()
}

/// The child: edit, one autosave tick, abort. Does nothing unless run as the child.
#[test]
fn autosave_crash_child() {
    let Some(root) = std::env::var_os(CHILD_ENV).map(PathBuf::from) else {
        return;
    };
    let mut app = editor_world(&root);
    let world = app.world_mut();
    // Open the fixture level and edit it (one undoable spawn).
    let open = level_io::open_level_file(world, &root.join("assets/maps/main.level.ron")).unwrap();
    world.insert_resource(open);
    let mut spawn = SpawnEntities::new_entity(
        world,
        regia_core::id::Guid::new_random(),
        "CrashRock",
        regia_scene::prelude::Transform::default(),
    )
    .unwrap();
    spawn.apply(world);
    world.resource_mut::<UndoStack>().push(Box::new(spawn));
    // Open the material graph and edit it.
    let mut graphs = MatGraphEditor::default();
    graphs
        .open(&root.join("assets/materials/rock.matgraph.ron"))
        .unwrap();
    graphs.doc.graph.name = "rock (edited before the crash)".into();
    graphs.doc.layout.insert("output".into(), [12.0, 34.0]);
    world.insert_resource(graphs);

    // The interval comes from the preference (1 minute), as the shell reads it.
    let mut prefs = regia_editor::editor_prefs::EditorPreferences::default();
    prefs
        .set(
            "autosave.interval_minutes",
            &regia_editor::editor_prefs::PrefValue::Int(1),
        )
        .unwrap();
    world.resource_mut::<doc_autosave::AutosaveState>().interval =
        regia_editor::editor_prefs::autosave_interval(&prefs);
    // One tick: the first dirty frame starts the clock, the frame a minute later writes.
    let t0 = Instant::now();
    let tick = |world: &mut World, now| {
        doc_autosave::tick(
            world,
            TickInput {
                project_root: &root,
                now,
                force: false,
            },
        )
    };
    assert!(!tick(world, t0).queued);
    assert!(tick(world, t0 + Duration::from_secs(61)).queued);
    assert_eq!(doc_autosave::flush(world, FLUSH), 1, "the tick's write finished");

    // The in-memory state at the crash, for the parent to compare.
    let expected = root.join("expected");
    std::fs::create_dir_all(&expected).unwrap();
    std::fs::write(expected.join("level.ron"), level_text(world)).unwrap();
    std::fs::write(expected.join("matgraph.bytes"), matgraph_bytes(world)).unwrap();
    std::process::abort();
}

#[test]
fn autosave_crash_relaunch_offers_and_restores_byte_identical() {
    let root = fixture_project("autosave-crash");
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "autosave_crash_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &root)
        .status()
        .unwrap();
    assert!(!status.success(), "the child aborts: {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(status.signal(), Some(6), "SIGABRT: {status:?}");
    }
    let expected_level = std::fs::read_to_string(root.join("expected/level.ron"))
        .expect("the child wrote its state before aborting");
    let expected_graph = std::fs::read(root.join("expected/matgraph.bytes")).unwrap();

    // The relaunch: the shell offers both.
    let shell = regia_editor::shell::EditorShell::new(None, Some(root.clone()));
    assert_eq!(shell.pending_recovery().len(), 1, "the level autosave is offered");
    let offers = RecoveryOffers::scan(&root);
    assert_eq!(offers.levels.len(), 1, "{offers:?}");
    assert_eq!(offers.docs.len(), 1, "{offers:?}");
    let rows = offers.rows();
    assert!(rows.iter().any(|r| r.kind == "level" && r.size > 0));
    assert!(rows.iter().any(|r| r.kind == "matgraph"
        && r.original.as_deref() == Some(root.join("assets/materials/rock.matgraph.ron").as_path())));

    // Restore all.
    let mut app = editor_world(&root);
    let world = app.world_mut();
    world.insert_resource(offers);
    let outcome = doc_autosave::restore(world, &[]);
    assert_eq!(outcome.done.len(), 2, "{outcome:?}");
    assert!(outcome.failed.is_empty(), "{outcome:?}");
    assert!(
        world.resource::<UndoStack>().is_dirty(),
        "the restored level is dirty"
    );
    let graphs = world.resource::<MatGraphEditor>();
    assert!(graphs.dirty(), "the restored graph is dirty");
    assert_eq!(
        graphs.path.as_deref(),
        Some(root.join("assets/materials/rock.matgraph.ron").as_path()),
        "the graph opens at its original path"
    );
    assert_eq!(
        world.resource::<OpenLevel>().path.as_deref(),
        Some(root.join("assets/maps/main.level.ron").as_path())
    );
    // Byte-identical to the pre-crash in-memory state.
    assert_eq!(level_text(world), expected_level);
    assert_eq!(matgraph_bytes(world), expected_graph);
    let _ = std::fs::remove_dir_all(&root);
}

/// Calls a registered method; returns `data.code` (or the message) on failure.
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("code").or_else(|| d.get("reason")))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned)
    })
}

/// The agent surface through the dispatcher: state, autosave now, list, restore, discard and
/// cancel_task, with their error codes.
#[test]
fn autosave_agent_methods_through_the_dispatcher() {
    let root = fixture_project("autosave-agent");
    let mut app = editor_world(&root);
    let world = app.world_mut();
    let mut graphs = MatGraphEditor::default();
    graphs
        .open(&root.join("assets/materials/rock.matgraph.ron"))
        .unwrap();
    graphs.doc.graph.name = "agent edit".into();
    world.insert_resource(graphs);

    let state = call(world, "editor.autosave_state", json!({})).unwrap();
    assert!(
        state["kinds"].as_array().unwrap().iter().any(|k| k == "matgraph"),
        "{state}"
    );
    assert_eq!(state["dirty"].as_array().unwrap().len(), 1, "{state}");

    let now = call(world, "editor.autosave_now", json!({})).unwrap();
    assert_eq!(now["docs"].as_array().unwrap().len(), 1, "{now}");
    assert!(now["errors"].as_array().unwrap().is_empty(), "{now}");

    // A fresh session over the same project lists, restores and discards.
    let mut next = editor_world(&root);
    let world = next.world_mut();
    let list = call(world, "editor.recovery_list", json!({})).unwrap();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{list}");
    let id = items[0]["id"].as_str().unwrap().to_string();
    assert!(items[0]["size"].as_u64().unwrap() > 0);
    assert_eq!(
        call(world, "editor.recovery_restore", json!({ "ids": [7] })),
        Err("invalid_ids".to_string())
    );
    let restored = call(world, "editor.recovery_restore", json!({ "ids": [id] })).unwrap();
    assert_eq!(restored["restored"].as_array().unwrap().len(), 1, "{restored}");
    assert_eq!(world.resource::<MatGraphEditor>().doc.graph.name, "agent edit");
    let discarded = call(
        world,
        "editor.recovery_discard",
        json!({ "ids": ["doc:missing"] }),
    )
    .unwrap();
    assert_eq!(discarded["failed"].as_array().unwrap().len(), 1, "{discarded}");

    assert_eq!(
        call(world, "editor.cancel_task", json!({ "id": "game_build" })),
        Err("no_such_task".to_string())
    );
    assert_eq!(
        call(world, "editor.cancel_task", json!({})),
        Err("invalid_task_id".to_string())
    );
    let token = regia_editor::tasks::CancelToken::new();
    regia_editor::tasks::sync_game_build(world, Some((token.clone(), 0.5)));
    let cancelled = call(world, "editor.cancel_task", json!({ "id": "game_build" })).unwrap();
    assert_eq!(cancelled["cancelled"], json!(true));
    assert!(token.is_cancelled());

    // No project: a clear code.
    world.insert_resource(regia_editor::agent_ext::EditorProject { root: None });
    assert_eq!(
        call(world, "editor.recovery_list", json!({})),
        Err("no_project".to_string())
    );
    let _ = std::fs::remove_dir_all(&root);
}
