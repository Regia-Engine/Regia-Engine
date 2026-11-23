// SPDX-License-Identifier: MPL-2.0
//! Blank template: `regia new` creates a project the editor can open and play
//! (`docs/specs/19` § 6, `docs/specs/23` § 8).

use std::path::PathBuf;

use regia_app::prelude::*;
use regia_editor::agent_ext;
use regia_editor::command::UndoStack;
use regia_editor::level_io;
use regia_editor::pie::{PieRequest, PieSession};
use regia_editor::project;
use regia_scene::level::LEVEL_SETTINGS_PATH;
use regia_scene::prelude::Name;
use regia_scene::spawn::SnapshotFilter;
use serde_json::json;

fn temp_parent(label: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-blank-{label}-{n}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("parent");
    project::set_recent_file_for_tests(dir.join("recents.txt"));
    dir
}

fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<regia_editor::pie::PieStatus>()
        .init_resource::<regia_editor::prefab_edit::PrefabEditMode>();
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    agent_ext::install(app.world_mut());
    app.update();
    app
}

fn names(app: &mut App) -> Vec<String> {
    let world = app.world_mut();
    let mut found: Vec<String> = world
        .query::<&Name>()
        .iter(world)
        .map(|name| name.as_str().to_string())
        .collect();
    found.sort();
    found
}

fn snapshot_text(app: &mut App) -> String {
    let filter = SnapshotFilter {
        level: None,
        resources: vec![LEVEL_SETTINGS_PATH.to_string()],
    };
    let file = regia_scene::prelude::snapshot_scene(app.world_mut(), &filter).expect("snapshot");
    file.to_ron_string().expect("ron")
}

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

#[test]
fn blank_project_opens_plays_and_gains_a_script() {
    let parent = temp_parent("open");
    let mut app = edit_app();
    let created = agent_ext::test_support::new_project(
        app.world_mut(),
        &json!({ "name": "My Game", "path": parent.display().to_string(), "template": "blank" }),
    )
    .expect("new project");
    let root = PathBuf::from(created["root"].as_str().expect("root"));
    assert!(root.join("Regia.toml").is_file(), "{}", root.display());
    assert!(created["warning"].is_null(), "startup level opened: {created}");

    let level_names = names(&mut app);
    assert!(
        level_names.iter().any(|name| name == "Ground") && level_names.iter().any(|name| name == "Marker"),
        "startup level: {level_names:?}"
    );

    let before = snapshot_text(&mut app);
    agent_ext::test_support::play(app.world_mut(), &json!({})).expect("play");
    let mut session = None;
    drain(&mut app, &mut session);
    let status = agent_ext::test_support::play_state(app.world_mut(), &json!({})).expect("state");
    // No wall-clock bound here: `start_elapsed_ms` measures real time, which parallel test load
    // (cargo test --workspace) inflates arbitrarily — the one observed flake. The PIE-start
    // budget is gated on reference hardware through the budget benches (docs/06 § 4); here we
    // assert play actually reached a settled frame (see level_build.rs).
    let frames = status["frames"].as_u64().unwrap_or(0);
    eprintln!(
        "blank PIE start_elapsed_ms={}",
        status["start_elapsed_ms"].as_u64().unwrap_or(0)
    );
    assert!(frames >= 1, "PIE has not run a settled frame: {status}");
    assert_eq!(status["mode"], "playing");
    let play_names = session.as_mut().map(|session| {
        let world = session.app.world_mut();
        world
            .query::<&Name>()
            .iter(world)
            .map(|name| name.as_str().to_string())
            .collect::<Vec<_>>()
    });
    let play_names = play_names.unwrap_or_default();
    assert!(
        play_names.iter().any(|name| name == "Ground"),
        "play copy: {play_names:?}"
    );
    agent_ext::test_support::stop(app.world_mut(), &json!({})).expect("stop");
    drain(&mut app, &mut session);
    assert!(session.is_none(), "stop drops the play world");
    assert_eq!(snapshot_text(&mut app), before, "stop restores the edit world");

    let script =
        agent_ext::test_support::create_script(app.world_mut(), &json!({ "name": "Door" })).expect("script");
    let path = script["path"].as_str().expect("path");
    assert!(
        PathBuf::from(path).ends_with("game/src/scripts/door.rs"),
        "{path}"
    );
    let lib = std::fs::read_to_string(root.join("game/src/lib.rs")).expect("lib");
    assert!(lib.contains("mod door;"), "{lib}");
    assert!(lib.contains("REGIA:SCRIPTS"), "marker stays");

    let recents = agent_ext::test_support::recent_projects(app.world_mut()).expect("recents");
    let listed = recents["projects"].as_array().expect("projects");
    assert!(
        listed
            .iter()
            .any(|value| value.as_str() == Some(root.to_str().unwrap_or(""))),
        "recent list: {recents}"
    );

    let refused = agent_ext::test_support::new_project(
        app.world_mut(),
        &json!({ "name": "Other", "template": "side-scroller", "path": parent }),
    );
    let message = refused.expect_err("an unknown template must be refused").message;
    assert!(message.contains("unknown template"), "{message}");

    let _ = std::fs::remove_dir_all(&parent);
}
