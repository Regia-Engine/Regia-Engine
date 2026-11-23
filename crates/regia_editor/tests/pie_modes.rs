// SPDX-License-Identifier: MPL-2.0
//! Play-in-editor modes and robustness (`docs/specs/19-editor.md` § 1, § 4, § 7; ADR-7205):
//! Simulate, keep simulation changes, split-screen players, the `EditorOnly` save filter and a
//! game panic's backtrace. Every test drives the same `apply_request` / `frame` path the headless
//! and windowed editor loops use; play worlds step on a fixed frame duration (no wall clock).

use std::time::Duration;

use regia::physics::{Collider, ColliderShape, RigidBody};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::GameExports;
use regia_editor::command::{EditorCommand, SpawnEntities, UndoStack, undo_world};
use regia_editor::level_io;
use regia_editor::pie::{
    CurrentGame, PieRequest, PieSession, PieStatus, PlayKind, PlayOptions, apply_request, stop_after_panic,
    sync_status,
};
use regia_editor::selection::{SelectMode, Selection};
use regia_framework::player::{Player, PlayerKind, PlayerManager, PlayerPlugin};
use regia_scene::components::EntityGuid;
use regia_scene::prelude::{Name, Transform};

/// The edit world the editor binary builds (scene + editor resources + the `editor.*` surface),
/// with the physics body types registered so authored bodies are level content.
fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>()
        .init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<PieStatus>();
    app.register_type::<RigidBody>();
    regia_editor::mesh_source::register_level_types(app.world_mut());
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

/// This binary's one GPU device ([`regia_render::RenderDevice::shared_headless`], the device the
/// editor lib's own tests share, ADR-9602), cloned per test. Each GPU test opening and dropping
/// its own instance and device on parallel test threads live-locked the NVIDIA driver (RTX 5090,
/// ADR-1438): three tests parked in driver futex waits for hours (ADR-8411).
fn gpu_device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

/// An entity created through the editor's own spawn path (guid-tagged, level member), not on the
/// undo stack.
fn create(app: &mut App, name: &str, at: Vec3) -> (Entity, Guid) {
    let guid = Guid::new_random();
    let transform = Transform::from_translation(at);
    let mut command = SpawnEntities::new_entity(app.world(), guid, name, transform).expect("spawn");
    command.apply(app.world_mut());
    let world = app.world_mut();
    let entity = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
        .expect("spawned");
    (entity, guid)
}

/// A dynamic ball 5 m up.
fn create_ball(app: &mut App) -> (Entity, Guid) {
    let (ball, guid) = create(app, "Ball", Vec3::new(0.0, 5.0, 0.0));
    app.world_mut().entity_mut(ball).insert((
        RigidBody::default(),
        Collider {
            shape: ColliderShape::Sphere { radius: 0.5 },
            ..Collider::default()
        },
    ));
    (ball, guid)
}

fn play(app: &mut App, options: PlayOptions, game: Option<CurrentGame>) -> PieSession {
    let mut slot = None;
    let line = apply_request(app.world_mut(), &mut slot, PieRequest::PlayWith(options), game);
    let mut session = slot.unwrap_or_else(|| panic!("play did not start: {line}"));
    // Fixed frame duration: the physics step count follows the frame count, not the wall clock.
    session
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    session
}

fn entity_by_guid(world: &mut World, guid: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
}

fn height(world: &mut World, guid: Guid) -> f32 {
    let entity = entity_by_guid(world, guid).expect("entity");
    world.get::<Transform>(entity).expect("transform").translation.y
}

fn local_players(world: &mut World) -> usize {
    world
        .query::<&Player>()
        .iter(world)
        .filter(|p| p.kind == PlayerKind::Local)
        .count()
}

fn no_world_hook(_: &mut World) {}

/// A game that installs the player plugin, as a real game does.
fn register_players(app: &mut App) {
    app.add_plugins(PlayerPlugin);
}

/// A game that installs the player plugin and physics, as the shipped templates do: with a game
/// crate open, play steps bodies only through the game's own physics (ADR-7251).
fn register_players_and_physics(app: &mut App) {
    app.add_plugins((PlayerPlugin, regia::physics::PhysicsPlugin));
}

/// A game whose `on_load` joins its first local player itself.
fn join_one_player(world: &mut World) {
    let _ = PlayerManager::spawn_player_static(world, PlayerKind::Local);
}

fn game(register: fn(&mut App), on_load: fn(&mut World)) -> CurrentGame {
    CurrentGame {
        exports: GameExports {
            register,
            on_unload: no_world_hook,
            on_load,
        },
        generation: 1,
        package: "pie_modes_game".to_owned(),
    }
}

#[test]
fn simulate_steps_a_falling_body_with_no_player_entity() {
    let mut app = edit_app();
    let (_, ball) = create_ball(&mut app);
    // The view was left possessed by an earlier play session.
    app.insert_resource(regia_editor::views::ViewportLayout {
        possessed: true,
        ..regia_editor::views::ViewportLayout::default()
    });
    let options = PlayOptions {
        kind: PlayKind::Simulate,
        ..PlayOptions::default()
    };
    let mut session = play(
        &mut app,
        options,
        Some(game(register_players_and_physics, join_one_player)),
    );
    let start = height(session.app.world_mut(), ball);
    for _ in 0..30 {
        assert!(session.frame().is_none(), "no panic");
    }
    let end = height(session.app.world_mut(), ball);
    assert!(end < start - 0.5, "the body falls in Simulate: {start} -> {end}");
    assert_eq!(
        local_players(session.app.world_mut()),
        0,
        "Simulate spawns no local player, even when the game's on_load asks for one"
    );
    assert!(
        !session
            .app
            .world()
            .resource::<regia_editor::views::ViewportLayout>()
            .possessed,
        "Simulate never possesses a game camera"
    );
    sync_status(app.world_mut(), Some(&session));
    assert_eq!(app.world().resource::<PieStatus>().kind, Some(PlayKind::Simulate));
    // The edit world never moved.
    assert!((height(app.world_mut(), ball) - 5.0).abs() < 1e-6);

    // Control: the same game in Play joins its player.
    let mut app = edit_app();
    let _ = create_ball(&mut app);
    let mut session = play(
        &mut app,
        PlayOptions::default(),
        Some(game(register_players_and_physics, join_one_player)),
    );
    assert_eq!(
        local_players(session.app.world_mut()),
        1,
        "Play lets the game join its player"
    );
}

#[test]
fn keep_simulation_changes_copies_a_moved_body_and_undo_restores_it() {
    let mut app = edit_app();
    let (_, ball) = create_ball(&mut app);
    let (_, still) = create(&mut app, "Still", Vec3::new(3.0, 0.0, 0.0));
    let mut session = play(&mut app, PlayOptions::default(), None);
    for _ in 0..30 {
        assert!(session.frame().is_none());
    }
    let fallen = height(session.app.world_mut(), ball);
    assert!(fallen < 4.5, "the ball fell in play: {fallen}");

    // Select the ball (and the unchanged entity) in the play world, then keep.
    let play_world = session.app.world_mut();
    let play_ball = entity_by_guid(play_world, ball).expect("play ball");
    let play_still = entity_by_guid(play_world, still).expect("play still");
    let mut selection = Selection::default();
    selection.select(play_ball, SelectMode::Replace);
    selection.select(play_still, SelectMode::Add);
    play_world.insert_resource(selection);
    let report =
        regia_editor::pie_keep::keep_simulation_changes(app.world_mut(), session.app.world_mut(), None)
            .expect("keep");
    assert_eq!(
        report.entities,
        vec![ball],
        "only the moved entity changed: {report:?}"
    );
    assert!(report.undo_step);
    assert!(
        (height(app.world_mut(), ball) - fallen).abs() < 1e-5,
        "the edit ball has the play pose"
    );

    // One undo step restores it; there is no second one from the keep.
    assert!(undo_world(app.world_mut()), "the keep is undoable");
    assert!(
        (height(app.world_mut(), ball) - 5.0).abs() < 1e-6,
        "undo restores the authored pose"
    );
    assert!(!undo_world(app.world_mut()), "the keep was one undo step");

    // Nothing selected is a clean refusal.
    session.app.world_mut().insert_resource(Selection::default());
    let err = regia_editor::pie_keep::keep_simulation_changes(app.world_mut(), session.app.world_mut(), None)
        .expect_err("nothing selected");
    assert_eq!(err.code, "keep_nothing_selected");
}

#[test]
fn split_screen_two_makes_two_viewports_with_two_players() {
    let mut app = edit_app();
    let options = PlayOptions {
        players: 2,
        ..PlayOptions::default()
    };
    let mut session = play(&mut app, options, None);
    assert!(session.frame().is_none());
    assert_eq!(
        local_players(session.app.world_mut()),
        2,
        "two local players joined"
    );
    sync_status(app.world_mut(), Some(&session));
    let status = app.world().resource::<PieStatus>().clone();
    assert_eq!(status.player_views.len(), 2, "{:?}", status.player_views);
    let rects: Vec<[f32; 4]> = status.player_views.iter().map(|v| v.rect).collect();
    assert_eq!(
        rects,
        vec![[0.0, 0.0, 0.5, 1.0], [0.5, 0.0, 0.5, 1.0]],
        "side by side"
    );
    // The viewport draws two panes over a 200 x 100 view.
    let view = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
    let panes = regia_editor::pie::split_screen_panes(session.app.world(), view);
    assert_eq!(
        panes,
        vec![
            (
                0,
                egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(100.0, 100.0))
            ),
            (
                1,
                egui::Rect::from_min_size(egui::pos2(110.0, 20.0), egui::vec2(100.0, 100.0))
            ),
        ]
    );

    // A game that joins its own first player gets one more, not two more.
    let mut app = edit_app();
    let mut session = play(&mut app, options, Some(game(register_players, join_one_player)));
    assert!(session.frame().is_none());
    assert_eq!(local_players(session.app.world_mut()), 2);
}

#[test]
fn a_saved_level_holds_no_editor_only_entity() {
    let mut app = edit_app();
    let _ = create(&mut app, "Rock", Vec3::ZERO);
    let (helper, _) = create(&mut app, "HelperGizmo", Vec3::ZERO);
    app.world_mut()
        .entity_mut(helper)
        .insert(regia_editor::editor_only::EditorOnly);
    let (child, _) = create(&mut app, "HelperChildArrow", Vec3::ZERO);
    app.world_mut().entity_mut(child).insert(ChildOf(helper));
    let (gltf, _) = create(&mut app, "GltfScaffold", Vec3::ZERO);
    app.world_mut()
        .entity_mut(gltf)
        .insert(regia_editor::gltf_hierarchy::GltfEditorOnly);
    app.update();

    let dir = regia_test_utils::unique_temp_dir("pie-modes-editor-only");
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("room.level.ron");
    let open = app.world().resource::<level_io::OpenLevel>().clone();
    level_io::save_level_file(app.world_mut(), &open, &path).expect("save");
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(text.contains("Rock"), "{text}");
    for gone in ["HelperGizmo", "HelperChildArrow", "GltfScaffold"] {
        assert!(
            !text.contains(gone),
            "{gone} is editor-only and must not be saved:\n{text}"
        );
    }
    // The edit world keeps them.
    let world = app.world_mut();
    let names: Vec<String> = world
        .query::<&Name>()
        .iter(world)
        .map(|n| n.as_str().to_owned())
        .collect();
    assert!(names.contains(&"HelperGizmo".to_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Panics on its second run: the first (settling) play frame passes, the next one fails.
#[inline(never)]
fn panic_on_the_second_frame(mut runs: Local<u32>) {
    *runs += 1;
    if *runs == 2 {
        panic!("the game failed on frame two");
    }
}

fn register_panicking_system(app: &mut App) {
    app.add_systems(Update, panic_on_the_second_frame);
}

#[test]
fn a_panicking_game_logs_a_backtrace_frame() {
    // The editor's log ring (the Output Log and `logs.read`); the editor binary installs it too.
    let _log = regia_core::log::init(regia_core::log::LogConfig {
        panic_hook: false,
        ..Default::default()
    });
    let mut app = edit_app();
    let mut session = play(
        &mut app,
        PlayOptions::default(),
        Some(game(register_panicking_system, no_world_hook)),
    );
    let message = session.frame().expect("the second frame panics");
    assert_eq!(message, "the game failed on frame two");
    stop_after_panic(app.world_mut(), session);
    sync_status(app.world_mut(), None);
    let status = app.world().resource::<PieStatus>().clone();
    assert_eq!(status.panic.as_deref(), Some("the game failed on frame two"));
    let trace = status.panic_backtrace.expect("a backtrace in the status");
    assert!(
        trace.contains("panic_on_the_second_frame"),
        "the game frame is in it:\n{trace}"
    );

    let (records, _) = regia_core::log::ring::recent(&regia_core::log::ring::LogQuery {
        contains: Some("the game failed on frame two".to_owned()),
        ..Default::default()
    });
    assert!(
        records
            .iter()
            .any(|r| r.message.contains("backtrace:") && r.message.contains("panic_on_the_second_frame")),
        "the log carries the panic with a backtrace frame: {records:?}"
    );
}

/// Every file under `dir`, relative, sorted.
fn files_under(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(dir) {
                out.push(rel.display().to_string());
            }
        }
    }
    out.sort();
    out
}

#[test]
fn cancelling_an_import_leaves_no_partial_asset() {
    let root = regia_test_utils::unique_temp_dir("pie-modes-import-cancel");
    let sources_dir = root.join("incoming");
    std::fs::create_dir_all(&sources_dir).expect("incoming");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let sources: Vec<std::path::PathBuf> = ["first.png", "second.png", "third.png"]
        .iter()
        .map(|name| {
            let path = sources_dir.join(name);
            std::fs::write(&path, b"not decoded before the cook").expect("source");
            path
        })
        .collect();
    // The cancel arrives after the first file was copied (the second check sees it).
    let checks = std::cell::Cell::new(0u32);
    let cancelled = || {
        checks.set(checks.get() + 1);
        checks.get() >= 2
    };
    let report = regia_editor::content::import_files_cancellable(&root, "props", &sources, None, &cancelled)
        .expect("import ran");
    assert!(report.cancelled, "{report:?}");
    assert!(
        report.imported.is_empty(),
        "nothing is reported imported: {report:?}"
    );
    assert!(checks.get() >= 2, "the cancel was checked between files");
    assert_eq!(
        files_under(&root.join("assets")),
        Vec::<String>::new(),
        "no partial asset (source or .meta) is left in assets/"
    );
    assert_eq!(report.summary(), "import cancelled (nothing was added)");

    // Without a cancel the same import copies the files (control).
    let report =
        regia_editor::content::import_files_cancellable(&root, "props", &sources[..1], None, &|| false)
            .expect("import ran");
    assert!(!report.cancelled);
    assert_eq!(report.imported, vec!["assets/props/first.png".to_owned()]);
    let _ = std::fs::remove_dir_all(&root);
}

/// Calls an editor method by name through the registered extension handler, as dispatch does.
fn call(
    world: &mut World,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params)
}

fn reason(result: Result<serde_json::Value, regia_agent::protocol::RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(error) => error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

/// Applies the queued play requests like the editor loop.
fn drain(app: &mut App, session: &mut Option<PieSession>) {
    let requests = std::mem::take(
        &mut app
            .world_mut()
            .resource_mut::<regia_editor::pie::PieRequests>()
            .queue,
    );
    for request in requests {
        let _ = apply_request(app.world_mut(), session, request, None);
    }
    sync_status(app.world_mut(), session.as_ref());
}

#[test]
fn the_play_modes_surface_answers_by_name() {
    use serde_json::json;
    let mut app = edit_app();
    let (_, ball) = create_ball(&mut app);
    let world = app.world_mut();
    assert_eq!(
        reason(call(world, "editor.play", json!({ "mode": "fly" }))),
        "play_mode_unknown"
    );
    assert_eq!(
        reason(call(world, "editor.play", json!({ "players": 9 }))),
        "play_players_out_of_range"
    );
    assert_eq!(
        reason(call(world, "editor.play", json!({ "players": 0 }))),
        "play_players_out_of_range"
    );
    assert_eq!(
        reason(call(world, "editor.play", json!({ "window": true }))),
        "play_window_unavailable"
    );
    assert_eq!(
        reason(call(world, "editor.keep_simulation_changes", json!({}))),
        "not_playing",
        "no session yet"
    );
    assert_eq!(
        reason(call(world, "editor.cancel_job", json!({ "id": "import:99" }))),
        "job_not_found"
    );
    let idle = call(world, "editor.cancel_job", json!({ "id": "build" })).expect("build");
    assert_eq!(idle["requested"], false);

    let queued = call(world, "editor.play", json!({ "mode": "simulate" })).expect("play");
    assert_eq!(queued["mode"], "simulate");
    let mut session = None;
    drain(&mut app, &mut session);
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["kind"], "simulate", "{state}");
    assert_eq!(state["players"], 0, "{state}");

    // Keep by guid: the ball moved a little in the settled frame plus a few more.
    let running = session.as_mut().expect("session");
    running
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    for _ in 0..10 {
        assert!(running.frame().is_none());
    }
    let queued = call(
        app.world_mut(),
        "editor.keep_simulation_changes",
        json!({ "guids": [ball.to_string()] }),
    )
    .expect("keep");
    assert_eq!(queued["queued"], true);
    drain(&mut app, &mut session);
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["kept"]["entities"], json!([ball.to_string()]), "{state}");
    assert_eq!(state["kept"]["undo_step"], true, "{state}");
    assert!(
        height(app.world_mut(), ball) < 5.0,
        "the edit ball took the play pose"
    );
    assert!(
        reason(call(
            app.world_mut(),
            "editor.keep_simulation_changes",
            json!({ "guids": ["x"] })
        ))
        .is_empty(),
        "a bad guid is plain invalid_params"
    );

    // An async import answers a job id the editor can cancel; whichever way it ends, no partial
    // asset is left (cancelled: nothing; done: the whole file).
    let root = regia_test_utils::unique_temp_dir("pie-modes-async-import");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"Import\"\nid = \"com.example.import\"\n",
    )
    .expect("manifest");
    let source = root.join("rock.png");
    std::fs::write(&source, b"bytes").expect("source");
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
    let answer = call(
        app.world_mut(),
        "editor.import",
        json!({ "sources": [source.display().to_string()], "async": true }),
    )
    .expect("import job");
    let job = answer["job"].as_str().expect("job id").to_owned();
    assert!(job.starts_with("import:"), "{answer}");
    let cancel = call(app.world_mut(), "editor.cancel_job", json!({ "id": job })).expect("cancel");
    assert_eq!(cancel["job"], json!(job));
    let state = loop {
        let state = app
            .world()
            .resource::<regia_editor::editor_jobs::EditorJobs>()
            .get(&job)
            .expect("the editor registered its job")
            .state();
        if state.finished() {
            break state.name().to_owned();
        }
        std::thread::yield_now();
    };
    // Which way it ends is a race with the worker (a one-file import may finish before the cancel
    // lands; the deterministic rollback proof is `cancelling_an_import_leaves_no_partial_asset`).
    // What the protocol must hold either way: the job's state, its result's `cancelled` and the
    // files on disk agree (a cancel after the worker's last check ends it `done`, files kept).
    let files = files_under(&root.join("assets"));
    let mut no_provider = |_: &mut World, method: &str, _: &serde_json::Value| {
        Err(regia_agent::protocol::RpcError::internal(format!(
            "unexpected call to {method}"
        )))
    };
    let status = regia_agent::jobs::job_status(app.world_mut(), &json!({ "job": job }), &mut no_provider)
        .expect("job status");
    assert_eq!(status["state"], json!(state), "{status}");
    match state.as_str() {
        "cancelled" => {
            assert!(files.is_empty(), "a cancelled import leaves nothing: {files:?}");
            assert_eq!(status["result"]["cancelled"], true, "{status}");
            assert_eq!(status["result"]["imported"], json!([]), "{status}");
        }
        "done" => {
            assert!(files.iter().any(|f| f.ends_with("rock.png")), "{files:?}");
            assert_eq!(status["result"]["cancelled"], false, "{status}");
            assert_eq!(
                status["result"]["imported"],
                json!(["assets/rock.png"]),
                "{status}"
            );
        }
        other => panic!("unexpected end state {other}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

// ---- fixrev7-editor-pie (review sprint-rev-claude-26-k, findings 1 and 4; ADR-7251) ----

/// Frames the template-style game's `Update` system ran.
static TEMPLATE_GAME_FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn count_template_game_frames(_: Res<Time>) {
    TEMPLATE_GAME_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// A game `register` shaped like `templates/third_person/game/src/lib.rs` `Game::build`: it adds
/// `ScenePlugin` and `PhysicsPlugin` itself (both are already in, or about to be in, the play
/// world), the player plugin, and an `Update` system that proves the register was not abandoned.
fn register_like_third_person(app: &mut App) {
    app.add_plugins((
        regia_scene::ScenePlugin,
        regia::physics::PhysicsPlugin,
        PlayerPlugin,
    ));
    app.add_systems(Update, count_template_game_frames);
}

/// Review finding 1: a game whose `register` adds `ScenePlugin` and `PhysicsPlugin` (the shipped
/// templates' shape), played over a level that holds a rigid body with a collider, registers in
/// both kinds: its own system runs every frame, the play world has exactly one physics plugin and
/// the level's ball falls. Simulate still removes the player the game's `on_load` joins.
#[test]
fn a_template_game_that_adds_scene_and_physics_registers_and_steps_its_level() {
    use std::sync::atomic::Ordering;
    for kind in [PlayKind::Play, PlayKind::Simulate] {
        let mut app = edit_app();
        let (_, ball) = create_ball(&mut app);
        let options = PlayOptions {
            kind,
            ..PlayOptions::default()
        };
        let mut session = play(
            &mut app,
            options,
            Some(game(register_like_third_person, join_one_player)),
        );
        assert!(
            session
                .app
                .has_plugin(std::any::type_name::<regia::physics::PhysicsPlugin>()),
            "{kind:?}: the game's PhysicsPlugin is in the play world"
        );
        let before = TEMPLATE_GAME_FRAMES.load(Ordering::Relaxed);
        let start = height(session.app.world_mut(), ball);
        for _ in 0..30 {
            assert!(session.frame().is_none(), "{kind:?}: no panic");
        }
        let ran = TEMPLATE_GAME_FRAMES.load(Ordering::Relaxed) - before;
        // Tests in this binary run in parallel, but only this test counts on the static.
        assert!(
            ran >= 30,
            "{kind:?}: the game's Update system runs every play frame (ran {ran} of 30): its register was not abandoned"
        );
        let end = height(session.app.world_mut(), ball);
        assert!(
            end < start - 0.5,
            "{kind:?}: the level's body falls under the game's physics: {start} -> {end}"
        );
        let expected_players = usize::from(kind == PlayKind::Play);
        assert_eq!(
            local_players(session.app.world_mut()),
            expected_players,
            "{kind:?}: the game's on_load player (removed again in Simulate)"
        );
    }
}

/// ADR-7251 (the ADR-9180 contract): with a game crate open, play has physics only when the game
/// adds it, as the shipped game does; PIE adds none behind it, in Play or Simulate. Without a game
/// crate the play world still simulates the level's bodies.
#[test]
fn play_steps_bodies_only_through_the_games_physics_when_a_game_is_open() {
    for kind in [PlayKind::Play, PlayKind::Simulate] {
        let options = PlayOptions {
            kind,
            ..PlayOptions::default()
        };
        // A game without physics: no physics world, the ball stays where it was authored.
        let mut app = edit_app();
        let (_, ball) = create_ball(&mut app);
        let mut session = play(&mut app, options, Some(game(register_players, no_world_hook)));
        for _ in 0..30 {
            assert!(session.frame().is_none(), "{kind:?}: no panic");
        }
        assert!(
            !session
                .app
                .has_plugin(std::any::type_name::<regia::physics::PhysicsPlugin>()),
            "{kind:?}: PIE adds no PhysicsPlugin behind a game that has none"
        );
        assert!(
            !session
                .app
                .world()
                .contains_resource::<regia::physics::PhysicsWorld>(),
            "{kind:?}: no physics world behind a game that has none"
        );
        assert!(
            (height(session.app.world_mut(), ball) - 5.0).abs() < 1e-6,
            "{kind:?}: nothing simulates the ball"
        );

        // No game crate: play adds physics itself so the level's bodies simulate.
        let mut app = edit_app();
        let (_, ball) = create_ball(&mut app);
        let mut session = play(&mut app, options, None);
        for _ in 0..30 {
            assert!(session.frame().is_none(), "{kind:?}: no panic");
        }
        assert!(
            height(session.app.world_mut(), ball) < 4.5,
            "{kind:?}: without a game crate the level's body falls"
        );
    }
}

/// A local player the game spawned outside the player manager whose controller is the player
/// entity itself, and one with no controller wired: Simulate removes both, two entities in all.
fn spawn_stray_players(world: &mut World) {
    let own = world.spawn_empty().id();
    world.entity_mut(own).insert(Player {
        index: 7,
        kind: PlayerKind::Local,
        controller: own,
        device: None,
    });
    world.spawn(Player {
        index: 8,
        kind: PlayerKind::Local,
        controller: Entity::PLACEHOLDER,
        device: None,
    });
}

/// Review finding 4: Simulate's removal of the game's local players subtracts the entities it
/// actually removed from the session's count, not two per player.
#[test]
fn simulate_counts_only_the_entities_it_removed() {
    let options = PlayOptions {
        kind: PlayKind::Simulate,
        ..PlayOptions::default()
    };
    // Control: the same game spawning nothing in on_load.
    let mut app = edit_app();
    let _ = create_ball(&mut app);
    let _ = create(&mut app, "Rock", Vec3::new(2.0, 0.0, 0.0));
    let control = play(&mut app, options, Some(game(register_players, no_world_hook)));
    let expected = control.entities;
    assert!(expected >= 2, "the level's two entities are counted: {expected}");

    let mut app = edit_app();
    let _ = create_ball(&mut app);
    let _ = create(&mut app, "Rock", Vec3::new(2.0, 0.0, 0.0));
    let mut session = play(
        &mut app,
        options,
        Some(game(register_players, spawn_stray_players)),
    );
    assert_eq!(
        local_players(session.app.world_mut()),
        0,
        "both strays were removed"
    );
    assert_eq!(
        session.entities, expected,
        "the two stray players were two entities, not four"
    );
}

/// A component type only the game's `register` registers in the play world.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct GameCrate {
    weight: u32,
}

fn register_game_crate(app: &mut App) {
    app.register_type::<GameCrate>();
}

/// ADR-7251: the game's plugins go in before the level spawns, so a component of the game's own
/// type authored in the level is live in play (not an unregistered blob), and the level's bodies
/// are in the game's physics world from the first frame.
#[test]
fn a_game_type_authored_in_the_level_is_live_in_play() {
    let mut app = edit_app();
    // The edit world knows the type (the real editor keeps it from the level file).
    app.register_type::<GameCrate>();
    let (entity, guid) = create(&mut app, "Crate", Vec3::ZERO);
    app.world_mut().entity_mut(entity).insert(GameCrate { weight: 7 });
    let mut session = play(
        &mut app,
        PlayOptions::default(),
        Some(game(register_game_crate, no_world_hook)),
    );
    let world = session.app.world_mut();
    let crate_entity = entity_by_guid(world, guid).expect("the crate is in play");
    assert_eq!(
        world.get::<GameCrate>(crate_entity),
        Some(&GameCrate { weight: 7 }),
        "the game's component is live in play"
    );
    assert!(
        world
            .get::<regia_scene::spawn::UnregisteredComponents>(crate_entity)
            .is_none(),
        "nothing of the crate stayed unregistered"
    );

    // The level's ball is in the game's physics world once fixed steps ran (the sync runs there).
    let mut app = edit_app();
    let (_, ball) = create_ball(&mut app);
    let mut session = play(
        &mut app,
        PlayOptions::default(),
        Some(game(register_like_third_person, no_world_hook)),
    );
    for _ in 0..5 {
        assert!(session.frame().is_none(), "no panic");
    }
    let world = session.app.world_mut();
    let ball = entity_by_guid(world, ball).expect("ball");
    assert!(
        world.get::<regia::physics::RigidBodyHandle>(ball).is_some(),
        "the level's body has a rapier body"
    );
    assert_eq!(
        world.resource::<regia::physics::PhysicsWorld>().colliders.len(),
        1,
        "the level's collider is in the game's physics world"
    );
}

// ---- post05w3-editor-play-window: the separate play window (specs/19 § 4, ADR-7272) ----
//
// Window events are driven through the extracted handler (`PlayWindow::on_event` / `sync`) with a
// recording platform, never a real window: the editor binary's winit loop only translates winit
// events into `PlayWindowEvent`s and implements `PlayWindowPlatform` over a real window and surface.

use regia_editor::editor_prefs::EditorPreferences;
use regia_editor::play_window::{
    PlayWindow, PlayWindowEvent, PlayWindowGeometry, PlayWindowPlatform, PlayWindowSupport, Routed,
};

/// The editor window's id in these tests (the play window gets ids from 100).
const EDITOR_WINDOW: u64 = 1;

/// A platform that records what the handler asked of it.
#[derive(Default)]
struct RecordingPlatform {
    opened: Vec<PlayWindowGeometry>,
    closed: u32,
    configures: Vec<(u32, u32)>,
    recreations: u32,
    captures: Vec<bool>,
    fail_open: bool,
}

impl PlayWindowPlatform for RecordingPlatform {
    fn open(&mut self, geometry: PlayWindowGeometry) -> Result<u64, String> {
        if self.fail_open {
            return Err("the window system refused the window".to_owned());
        }
        self.opened.push(geometry);
        Ok(99 + self.opened.len() as u64)
    }

    fn configure_surface(&mut self, width: u32, height: u32) {
        self.configures.push((width, height));
    }

    fn recreate_surface(&mut self) -> Result<(), String> {
        self.recreations += 1;
        Ok(())
    }

    fn set_capture(&mut self, captured: bool) {
        self.captures.push(captured);
    }

    fn close(&mut self) {
        self.closed += 1;
    }
}

fn key(physical: &str, pressed: bool) -> PlayWindowEvent {
    PlayWindowEvent::Key {
        physical: physical.to_owned(),
        logical: physical.to_owned(),
        pressed,
        repeat: false,
        shift: false,
        ctrl: false,
        alt: false,
    }
}

/// Keyboard messages written to the play world's message queue so far.
fn play_keys(session: &Option<PieSession>) -> Vec<String> {
    let world = session.as_ref().expect("session").app.world();
    world
        .get_resource::<Messages<regia_platform::KeyboardInput>>()
        .map(|messages| {
            messages
                .iter_current_update_messages()
                .map(|m| m.physical_key.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// An edit world with a window system (`PlayWindowSupport`), playing in a separate window.
fn play_in_window(platform: &mut RecordingPlatform) -> (App, Option<PieSession>, PlayWindow, Guid) {
    use serde_json::json;
    let mut app = edit_app();
    let (_, ball) = create_ball(&mut app);
    app.world_mut().insert_resource(PlayWindowSupport);
    let answer = call(app.world_mut(), "editor.play", json!({ "window": true })).expect("play");
    assert_eq!(answer["window"], "play_window", "{answer}");
    let mut session = None;
    drain(&mut app, &mut session);
    assert!(session.is_some(), "play started");
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), platform);
    (app, session, window, ball)
}

#[test]
fn editor_play_with_window_true_starts_play_in_the_play_window() {
    use serde_json::json;
    let mut platform = RecordingPlatform::default();
    let (mut app, mut session, mut window, _) = play_in_window(&mut platform);
    assert_eq!(platform.opened.len(), 1, "one play window opened");
    assert_eq!(window.id(), Some(100));
    assert_eq!(
        platform.opened[0],
        PlayWindowGeometry::default(),
        "no preferences: the default size, the window system places it"
    );
    // A second sync with the window open opens nothing more.
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert_eq!(platform.opened.len(), 1);
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["window"], true, "{state}");
    assert_eq!(state["play_window"]["open"], true, "{state}");
    assert_eq!(
        state["play_window"]["title"],
        regia_editor::play_window::PLAY_WINDOW_TITLE,
        "{state}"
    );
    assert_eq!(state["play_window"]["size"], json!([1280, 720]), "{state}");
    // The play world has a primary window entity sized like the play window (a game reads it).
    let world = session.as_mut().expect("session").app.world_mut();
    let sizes: Vec<(u32, u32)> = world
        .query_filtered::<&regia_platform::Window, With<regia_platform::PrimaryWindow>>()
        .iter(world)
        .map(|w| w.physical_size)
        .collect();
    assert_eq!(sizes, vec![(1280, 720)]);

    // Play in the viewport (window false) opens no window.
    let mut platform = RecordingPlatform::default();
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    let answer = call(app.world_mut(), "editor.play", json!({})).expect("play");
    assert_eq!(answer["window"], "viewport", "{answer}");
    let mut session = None;
    drain(&mut app, &mut session);
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert!(platform.opened.is_empty());
    assert_eq!(window.id(), None);

    // A window system that refuses the window: play goes on in the viewport, the status says why.
    let mut platform = RecordingPlatform {
        fail_open: true,
        ..RecordingPlatform::default()
    };
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    call(app.world_mut(), "editor.play", json!({ "window": true })).expect("play");
    let mut session = None;
    drain(&mut app, &mut session);
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert!(session.is_some(), "play keeps running in the viewport");
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["play_window"]["open"], false, "{state}");
    assert!(
        state["play_window"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("refused")),
        "{state}"
    );
}

#[test]
fn play_window_keys_reach_the_play_world_and_editor_keys_do_not() {
    let mut platform = RecordingPlatform::default();
    let (mut app, mut session, mut window, _) = play_in_window(&mut platform);
    let play_id = window.id().expect("open");

    // Tagged with the play window: the play world gets it, the editor does not.
    let routed = window.on_event(
        play_id,
        key("KeyW", true),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Play);
    assert_eq!(play_keys(&session), vec!["KeyW".to_owned()]);

    // Tagged with the editor window: the editor handles it, the play world gets nothing.
    let routed = window.on_event(
        EDITOR_WINDOW,
        key("KeyS", true),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Editor);
    assert_eq!(
        play_keys(&session),
        vec!["KeyW".to_owned()],
        "an editor key never reaches the play world"
    );

    // Mouse buttons and the cursor follow the same split.
    let routed = window.on_event(
        play_id,
        PlayWindowEvent::CursorMoved { x: 10.0, y: 20.0 },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Play);
    let routed = window.on_event(
        EDITOR_WINDOW,
        PlayWindowEvent::CursorMoved { x: 1.0, y: 2.0 },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Editor);
    let world = session.as_ref().expect("session").app.world();
    let cursors: Vec<(f64, f64)> = world
        .resource::<Messages<regia_platform::CursorMoved>>()
        .iter_current_update_messages()
        .map(|m| m.position)
        .collect();
    assert_eq!(cursors, vec![(10.0, 20.0)]);

    // A game reading `Keyboard` through the input plugin sees the key held after a frame.
    let running = session.as_mut().expect("session");
    running
        .app
        .add_plugins(regia_input::InputPlugin::new(regia_input::InputConfig::new()));
    window.on_event(
        play_id,
        key("KeyD", true),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    let running = session.as_mut().expect("session");
    assert!(running.frame().is_none());
    assert!(
        running
            .app
            .world()
            .resource::<regia_input::Keyboard>()
            .pressed("KeyD"),
        "the game's keyboard state has the play window's key"
    );
}

#[test]
fn shift_f1_releases_the_mouse_the_play_window_captured() {
    let mut platform = RecordingPlatform::default();
    let (mut app, mut session, mut window, _) = play_in_window(&mut platform);
    let play_id = window.id().expect("open");
    let click = PlayWindowEvent::MouseButton {
        button: regia_platform::MouseButton::Left,
        pressed: true,
    };
    window.on_event(
        play_id,
        click.clone(),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert!(window.captured(), "a click in the play window captures the mouse");
    assert_eq!(platform.captures, vec![true]);

    let release = PlayWindowEvent::Key {
        physical: "F1".to_owned(),
        logical: "F1".to_owned(),
        pressed: true,
        repeat: false,
        shift: true,
        ctrl: false,
        alt: false,
    };
    let before = play_keys(&session).len();
    let routed = window.on_event(play_id, release, app.world_mut(), &mut session, &mut platform);
    assert_eq!(routed, Routed::Play);
    assert!(!window.captured(), "Shift+F1 releases it");
    assert_eq!(platform.captures, vec![true, false]);
    assert_eq!(
        play_keys(&session).len(),
        before,
        "the release shortcut is the editor's, not the game's"
    );

    // Capture again; losing focus releases it too, and a plain F1 goes to the game.
    window.on_event(play_id, click, app.world_mut(), &mut session, &mut platform);
    window.on_event(
        play_id,
        PlayWindowEvent::Focused(false),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert!(!window.captured());
    assert_eq!(platform.captures, vec![true, false, true, false]);
    window.on_event(
        play_id,
        key("F1", true),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert!(play_keys(&session).contains(&"F1".to_owned()));
}

/// The level as the editor would save it.
fn saved_level(app: &mut App) -> String {
    let id = app.world().resource::<level_io::OpenLevel>().id;
    let level = level_io::snapshot_saved_level(app.world_mut(), id).expect("snapshot");
    level.to_ron_string().expect("ron")
}

#[test]
fn closing_the_play_window_ends_play_and_leaves_the_edit_world_unchanged() {
    let mut platform = RecordingPlatform::default();
    let (mut app, mut session, mut window, ball) = play_in_window(&mut platform);
    let before = saved_level(&mut app);
    let undo_before = app.world().resource::<UndoStack>().undo_depth();
    let play_id = window.id().expect("open");
    // The ball falls in play, so the play world differs from the edit world.
    let running = session.as_mut().expect("session");
    running
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    for _ in 0..20 {
        assert!(running.frame().is_none());
    }
    assert!(height(running.app.world_mut(), ball) < 5.0);

    let routed = window.on_event(
        play_id,
        PlayWindowEvent::CloseRequested,
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Closed);
    assert!(session.is_none(), "closing the play window ends play");
    assert_eq!(platform.closed, 1);
    assert_eq!(window.id(), None);
    let status = app.world().resource::<PieStatus>().clone();
    assert!(status.mode.is_none(), "stopped: {status:?}");
    assert_eq!(saved_level(&mut app), before, "the edit world is unchanged");
    assert_eq!(app.world().resource::<UndoStack>().undo_depth(), undo_before);
    assert!((height(app.world_mut(), ball) - 5.0).abs() < 1e-6);

    // Events for the closed window go nowhere; stopping play another way closes the window too.
    let routed = window.on_event(
        play_id,
        key("KeyW", true),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Ignored);
    let mut platform = RecordingPlatform::default();
    let (mut app, mut session, mut window, _) = play_in_window(&mut platform);
    let _ = apply_request(app.world_mut(), &mut session, PieRequest::Stop, None);
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert_eq!(
        platform.closed, 1,
        "Stop (toolbar, Esc, agent) closes the play window"
    );
    assert_eq!(window.id(), None);
}

#[test]
fn a_resize_or_a_lost_surface_recreates_only_the_surface() {
    let mut platform = RecordingPlatform::default();
    let (mut app, mut session, mut window, _) = play_in_window(&mut platform);
    let play_id = window.id().expect("open");
    let entities = session.as_ref().expect("session").entities;

    let routed = window.on_event(
        play_id,
        PlayWindowEvent::Resized {
            width: 800,
            height: 600,
        },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Play);
    assert_eq!(
        platform.configures,
        vec![(800, 600)],
        "the surface is reconfigured"
    );
    // Minimized (0 x 0): nothing to configure.
    window.on_event(
        play_id,
        PlayWindowEvent::Resized { width: 0, height: 0 },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(platform.configures, vec![(800, 600)]);
    window.on_event(
        play_id,
        PlayWindowEvent::SurfaceLost,
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(platform.recreations, 1, "a lost surface is recreated");

    let counters = window.counters();
    assert_eq!(counters.surface_configures, 1);
    assert_eq!(counters.surface_recreations, 1);
    assert_eq!(counters.windows_opened, 1, "the window itself is not recreated");
    assert_eq!(platform.opened.len(), 1);
    assert_eq!(platform.closed, 0);
    assert_eq!(window.id(), Some(play_id));
    let running = session
        .as_ref()
        .expect("play survives a resize and a surface loss");
    assert_eq!(running.entities, entities);
    // The game's window size follows.
    let world = session.as_mut().expect("session").app.world_mut();
    let sizes: Vec<(u32, u32)> = world
        .query_filtered::<&regia_platform::Window, With<regia_platform::PrimaryWindow>>()
        .iter(world)
        .map(|w| w.physical_size)
        .collect();
    assert_eq!(sizes, vec![(800, 600)]);
}

#[test]
fn the_play_window_size_and_position_persist_in_editor_preferences() {
    let mut platform = RecordingPlatform::default();
    let (mut app, mut session, mut window, _) = play_in_window(&mut platform);
    app.world_mut().init_resource::<EditorPreferences>();
    let play_id = window.id().expect("open");
    window.on_event(
        play_id,
        PlayWindowEvent::Resized {
            width: 1024,
            height: 576,
        },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    window.on_event(
        play_id,
        PlayWindowEvent::Moved { x: 140, y: -20 },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    window.on_event(
        play_id,
        PlayWindowEvent::CloseRequested,
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    let prefs = app.world().resource::<EditorPreferences>().clone();
    let expected = PlayWindowGeometry {
        width: 1024,
        height: 576,
        position: Some([140, -20]),
    };
    assert_eq!(PlayWindowGeometry::from_prefs(&prefs), expected);

    // Round trip through the preferences file text.
    let (read, diagnostics) = EditorPreferences::from_text(&prefs.to_text()).expect("parse");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(PlayWindowGeometry::from_prefs(&read), expected);

    // The next play window opens where the last one was.
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    app.world_mut().insert_resource(read);
    call(
        app.world_mut(),
        "editor.play",
        serde_json::json!({ "window": true }),
    )
    .expect("play");
    let mut session = None;
    drain(&mut app, &mut session);
    let mut platform = RecordingPlatform::default();
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert_eq!(platform.opened, vec![expected]);

    // Out-of-range stored values fall back to the defaults (the preference check refuses them).
    let mut prefs = EditorPreferences::default();
    assert!(
        prefs
            .set(
                "play.window_width",
                &regia_editor::editor_prefs::PrefValue::Int(10)
            )
            .is_err()
    );
    assert_eq!(
        PlayWindowGeometry::from_prefs(&prefs),
        PlayWindowGeometry::default()
    );
}

#[test]
fn without_a_window_system_play_window_answers_a_named_reason() {
    use serde_json::json;
    let mut app = edit_app();
    // No `PlayWindowSupport`: the headless editor and tests.
    let error = call(app.world_mut(), "editor.play", json!({ "window": true })).expect_err("refused");
    let data = error.data.clone().unwrap_or_default();
    assert_eq!(data["reason"], "play_window_unavailable", "{data}");
    assert!(
        error.message.contains("no window system"),
        "the message names the cause: {}",
        error.message
    );
    // Nothing was queued: play did not start in the viewport instead.
    assert!(
        app.world()
            .resource::<regia_editor::pie::PieRequests>()
            .queue
            .is_empty()
    );
}

// ---- post05w4-play-window-sizes: the chosen play resolution, one render, the main.rs glue (ADR-7283) ----

use regia_editor::play_window::{FrameDriver, Letterbox, PlayFrame, PlayResolution, SurfaceAcquire};

/// A recording platform whose surface recreation can be made to fail.
#[derive(Default)]
struct FlakySurface {
    inner: RecordingPlatform,
    fail_recreate: bool,
}

impl PlayWindowPlatform for FlakySurface {
    fn open(&mut self, geometry: PlayWindowGeometry) -> Result<u64, String> {
        self.inner.open(geometry)
    }
    fn configure_surface(&mut self, width: u32, height: u32) {
        self.inner.configure_surface(width, height);
    }
    fn recreate_surface(&mut self) -> Result<(), String> {
        if self.fail_recreate {
            return Err("the compositor has no surface for the window yet".to_owned());
        }
        self.inner.recreate_surface()
    }
    fn set_capture(&mut self, captured: bool) {
        self.inner.set_capture(captured);
    }
    fn close(&mut self) {
        self.inner.close();
    }
}

/// A game with the UI plugin (code-built UI) and the platform messages its pointer reads.
fn register_ui(app: &mut App) {
    app.add_plugins(regia_ui::UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
}

/// Applies the queued play requests like the editor loop, with a game crate open.
fn drain_with(app: &mut App, session: &mut Option<PieSession>, game: &CurrentGame) {
    let requests = std::mem::take(
        &mut app
            .world_mut()
            .resource_mut::<regia_editor::pie::PieRequests>()
            .queue,
    );
    for request in requests {
        let _ = apply_request(app.world_mut(), session, request, Some(game.clone()));
    }
    sync_status(app.world_mut(), session.as_ref());
}

/// UI clicks the play world's UI sent to `target` (drains the queue).
fn ui_clicks(session: &mut Option<PieSession>, target: Entity) -> usize {
    let world = session.as_mut().expect("session").app.world_mut();
    world
        .resource_mut::<Messages<regia_ui::UiEvent>>()
        .drain()
        .filter(|e| e.target == target && e.kind == regia_ui::UiEventKind::Click)
        .count()
}

fn primary_window_sizes(session: &mut Option<PieSession>) -> Vec<(u32, u32)> {
    let world = session.as_mut().expect("session").app.world_mut();
    world
        .query_filtered::<&regia_platform::Window, With<regia_platform::PrimaryWindow>>()
        .iter(world)
        .map(|w| w.physical_size)
        .collect()
}

#[test]
fn a_phone_preset_renders_at_its_size_and_a_mapped_click_hits_the_ui_rect() {
    use serde_json::json;
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    let answer = call(
        app.world_mut(),
        "editor.play",
        json!({ "window": true, "preset": "phone_9_16_portrait" }),
    )
    .expect("play");
    assert_eq!(answer["window"], "play_window", "{answer}");
    assert_eq!(answer["resolution"]["preset"], "phone_9_16_portrait", "{answer}");
    assert_eq!(answer["resolution"]["size"], json!([1080, 1920]), "{answer}");
    let game = game(register_ui, no_world_hook);
    let mut session = None;
    drain_with(&mut app, &mut session, &game);
    assert!(session.is_some(), "play started");
    let mut platform = RecordingPlatform::default();
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    let play_id = window.id().expect("open");
    // The OS window keeps its own (default) size; the play world renders at the preset's.
    assert_eq!(platform.opened, vec![PlayWindowGeometry::default()]);
    assert_eq!(window.render_size(), [1080, 1920]);
    // 1080 x 1920 fitted into 1280 x 720: scale 0.375, 405 x 720, pillarboxed in the middle.
    let picture = window.picture();
    assert_eq!(
        picture,
        Letterbox {
            x: 437,
            y: 0,
            width: 405,
            height: 720
        }
    );
    // The play camera's target: the frame the window renders is the preset's size, drawn into the
    // letterbox rect.
    match window.begin_frame(
        SurfaceAcquire::Ready,
        app.world_mut(),
        &mut session,
        &mut platform,
    ) {
        PlayFrame::Render { size, picture: at } => {
            assert_eq!(size, [1080, 1920]);
            assert_eq!(at, picture);
        }
        PlayFrame::Skip => panic!("a ready surface renders"),
    }
    // The game sees a 1080 x 1920 primary window (its UI lays out in that space).
    assert_eq!(primary_window_sizes(&mut session), vec![(1080, 1920)]);
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    let pw = &state["play_window"];
    assert_eq!(pw["size"], json!([1280, 720]), "{state}");
    assert_eq!(pw["resolution"]["preset"], "phone_9_16_portrait", "{state}");
    assert_eq!(pw["resolution"]["size"], json!([1080, 1920]), "{state}");
    assert_eq!(pw["picture"], json!([437, 0, 405, 720]), "{state}");

    // A button near the bottom of the phone screen, in the play world's pixels.
    let button = session
        .as_mut()
        .expect("session")
        .app
        .world_mut()
        .spawn((
            regia_ui::Interactable::default(),
            regia_ui::node::ComputedNode {
                rect: [440.0, 1700.0, 200.0, 120.0],
                clip_rect: [440.0, 1700.0, 200.0, 120.0],
                scale: 1.0,
                ..Default::default()
            },
        ))
        .id();
    let press = |pressed| PlayWindowEvent::MouseButton {
        button: regia_platform::MouseButton::Left,
        pressed,
    };

    // A click in the letterbox (left of the picture) reaches nothing: no cursor, no button, no
    // capture.
    let routed = window.on_event(
        play_id,
        PlayWindowEvent::CursorMoved { x: 100.0, y: 360.0 },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Letterbox);
    let routed = window.on_event(play_id, press(true), app.world_mut(), &mut session, &mut platform);
    assert_eq!(routed, Routed::Letterbox);
    assert!(!window.captured(), "a letterbox click does not capture the mouse");
    {
        let world = session.as_ref().expect("session").app.world();
        assert_eq!(
            world
                .resource::<Messages<regia_platform::CursorMoved>>()
                .iter_current_update_messages()
                .count(),
            0
        );
        assert_eq!(
            world
                .resource::<Messages<regia_platform::MouseButtonInput>>()
                .iter_current_update_messages()
                .count(),
            0
        );
    }
    window.on_event(
        play_id,
        press(false),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    let running = session.as_mut().expect("session");
    assert!(running.frame().is_none());
    assert!(running.frame().is_none());
    assert_eq!(ui_clicks(&mut session, button), 0);

    // The button's middle (540, 1760) in play pixels is (437 + 540 * 0.375, 1760 * 0.375) =
    // (639.5, 660) in the window: the mapped click lands on it.
    let routed = window.on_event(
        play_id,
        PlayWindowEvent::CursorMoved { x: 639.5, y: 660.0 },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(routed, Routed::Play);
    {
        let world = session.as_ref().expect("session").app.world();
        let positions: Vec<(f64, f64)> = world
            .resource::<Messages<regia_platform::CursorMoved>>()
            .iter_current_update_messages()
            .map(|m| m.position)
            .collect();
        assert_eq!(positions.len(), 1);
        let (x, y) = positions[0];
        assert!(
            (x - 540.0).abs() < 1.5 && (y - 1760.0).abs() < 1.5,
            "mapped to {x},{y}"
        );
    }
    let routed = window.on_event(play_id, press(true), app.world_mut(), &mut session, &mut platform);
    assert_eq!(routed, Routed::Play);
    assert!(session.as_mut().expect("session").frame().is_none());
    window.on_event(
        play_id,
        press(false),
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert!(session.as_mut().expect("session").frame().is_none());
    assert!(session.as_mut().expect("session").frame().is_none());
    assert_eq!(
        ui_clicks(&mut session, button),
        1,
        "the mapped click clicks the button"
    );

    // A resize of the OS window moves the letterbox; the game's window keeps the preset's size.
    window.on_event(
        play_id,
        PlayWindowEvent::Resized {
            width: 1000,
            height: 1000,
        },
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    assert_eq!(window.render_size(), [1080, 1920]);
    assert_eq!(
        window.picture(),
        Letterbox {
            x: 218,
            y: 0,
            width: 563,
            height: 1000
        }
    );
    assert_eq!(primary_window_sizes(&mut session), vec![(1080, 1920)]);
}

#[test]
fn the_play_resolution_is_a_preference_and_editor_play_validates_it() {
    use regia_editor::editor_prefs::PrefValue;
    use serde_json::json;
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    app.world_mut().init_resource::<EditorPreferences>();
    // The toolbar's size menu stores the choice; the next play window uses it.
    regia_editor::play_window::set_play_resolution(
        app.world_mut(),
        PlayResolution::preset("tablet_3_4_portrait").expect("preset"),
    );
    let prefs = app.world().resource::<EditorPreferences>().clone();
    assert_eq!(prefs.text("play.resolution"), "tablet_3_4_portrait");
    let (read, diagnostics) = EditorPreferences::from_text(&prefs.to_text()).expect("parse");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(
        PlayResolution::from_prefs(&read),
        PlayResolution::Fixed {
            width: 1536,
            height: 2048
        }
    );
    call(app.world_mut(), "editor.play", json!({ "window": true })).expect("play");
    let mut session = None;
    drain(&mut app, &mut session);
    let mut platform = RecordingPlatform::default();
    let mut window = PlayWindow::default();
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert_eq!(window.render_size(), [1536, 2048]);
    // A custom size through the preference keys.
    let mut prefs = EditorPreferences::default();
    prefs
        .set("play.resolution", &PrefValue::Text("custom".into()))
        .expect("custom");
    prefs
        .set("play.resolution_width", &PrefValue::Int(800))
        .expect("w");
    prefs
        .set("play.resolution_height", &PrefValue::Int(600))
        .expect("h");
    assert_eq!(
        PlayResolution::from_prefs(&prefs),
        PlayResolution::Fixed {
            width: 800,
            height: 600
        }
    );
    assert!(
        prefs
            .set("play.resolution", &PrefValue::Text("8k_phone".into()))
            .is_err()
    );

    // editor.play: `size` wins for the session; a bad size or preset is a named error.
    let answer = call(
        app.world_mut(),
        "editor.play",
        json!({ "window": true, "size": [640, 360] }),
    )
    .expect("custom size");
    assert_eq!(answer["resolution"]["preset"], "custom", "{answer}");
    assert_eq!(answer["resolution"]["size"], json!([640, 360]), "{answer}");
    let answer = call(
        app.world_mut(),
        "editor.play",
        json!({ "window": true, "preset": "window" }),
    )
    .expect("window-sized");
    assert_eq!(answer["resolution"]["preset"], "window", "{answer}");
    assert_eq!(answer["resolution"]["size"], serde_json::Value::Null, "{answer}");
    for bad in [
        json!({ "window": true, "preset": "8k_phone" }),
        json!({ "window": true, "size": [0, 100] }),
        json!({ "window": true, "size": [100] }),
        json!({ "window": true, "size": [100, 99999] }),
        json!({ "window": true, "size": [100, 100], "preset": "hd_720" }),
    ] {
        assert_eq!(
            reason(call(app.world_mut(), "editor.play", bad.clone())),
            "play_resolution_invalid",
            "{bad}"
        );
    }
    assert_eq!(
        reason(call(
            app.world_mut(),
            "editor.play",
            json!({ "preset": "hd_720" })
        )),
        "play_resolution_needs_window"
    );
}

/// One editor frame through the real shell, headless egui, with the play world in the panels.
fn shell_frame(
    shell: &mut regia_editor::shell::EditorShell,
    ctx: &egui::Context,
    app: &mut App,
    session: &mut Option<PieSession>,
    frame: u32,
) {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(640.0, 400.0),
        )),
        time: Some(f64::from(frame) / 60.0),
        predicted_dt: 1.0 / 60.0,
        ..Default::default()
    };
    let world = app.world_mut();
    let mut play = session.as_mut().map(|s| s.app.world_mut());
    let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, play.as_deref_mut()));
    output.textures_delta.clear();
}

#[test]
fn with_the_play_window_open_play_renders_once_per_editor_frame() {
    use serde_json::json;
    let device = gpu_device();
    let root = regia_test_utils::unique_temp_dir("pie-play-renders");
    std::fs::create_dir_all(root.join("assets/levels")).expect("root");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"Play Renders\"\nid = \"com.example.renders\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    let run = |window_open: bool| -> u64 {
        let mut app = edit_app();
        app.world_mut().insert_resource(PlayWindowSupport);
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(root.clone()),
            });
        create_ball(&mut app);
        let params = if window_open {
            json!({ "window": true, "size": [320, 180] })
        } else {
            json!({})
        };
        call(app.world_mut(), "editor.play", params).expect("play");
        let mut session = None;
        drain(&mut app, &mut session);
        let mut platform = RecordingPlatform::default();
        let mut window = PlayWindow::default();
        let mut shell = regia_editor::shell::EditorShell::new(Some(device.clone()), Some(root.clone()));
        let ctx = egui::Context::default();
        // Settle the dock and the first viewport frame, then count.
        for frame in 0..2 {
            window.sync(app.world_mut(), session.as_mut(), &mut platform);
            shell_frame(&mut shell, &ctx, &mut app, &mut session, frame);
        }
        let before = regia_editor::render::play_world_renders(session.as_ref().expect("session").app.world());
        const FRAMES: u32 = 3;
        for frame in 0..FRAMES {
            assert!(session.as_mut().expect("session").frame().is_none());
            window.sync(app.world_mut(), session.as_mut(), &mut platform);
            shell_frame(&mut shell, &ctx, &mut app, &mut session, 2 + frame);
            // The windowed editor's play-window present (main.rs), through the same handler.
            if let PlayFrame::Render { size, .. } = window.begin_frame(
                SurfaceAcquire::Ready,
                app.world_mut(),
                &mut session,
                &mut platform,
            ) {
                let world = session.as_mut().expect("session").app.world_mut();
                assert!(regia_editor::render::render_play_window_gpu(&device, world, size, true).is_some());
                window.presented(app.world_mut());
            }
        }
        assert_eq!(window.id().is_some(), window_open);
        let renders =
            regia_editor::render::play_world_renders(session.as_ref().expect("session").app.world()) - before;
        assert_eq!(
            renders % u64::from(FRAMES),
            0,
            "{renders} renders over {FRAMES} frames"
        );
        renders / u64::from(FRAMES)
    };
    assert_eq!(
        run(false),
        1,
        "play in the viewport: the viewport renders the play world once"
    );
    assert_eq!(
        run(true),
        1,
        "play in the window: the window renders it and the viewport shows a placeholder"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_lost_play_window_surface_is_recreated_and_play_keeps_going() {
    let mut platform = FlakySurface::default();
    let (mut app, mut session, mut window, ball) = {
        let mut recording = RecordingPlatform::default();
        let (app, session, window, ball) = play_in_window(&mut recording);
        // Hand the open window over to the flaky platform (same ids, same counters).
        platform.inner = recording;
        (app, session, window, ball)
    };
    session
        .as_mut()
        .expect("session")
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    let frame = |window: &mut PlayWindow,
                 app: &mut App,
                 session: &mut Option<PieSession>,
                 platform: &mut FlakySurface,
                 acquire: SurfaceAcquire| {
        assert!(session.as_mut().expect("play survives").frame().is_none());
        let drawn = window.begin_frame(acquire, app.world_mut(), session, platform);
        if matches!(drawn, PlayFrame::Render { .. }) {
            window.presented(app.world_mut());
        }
        drawn
    };
    assert!(matches!(
        frame(
            &mut window,
            &mut app,
            &mut session,
            &mut platform,
            SurfaceAcquire::Ready
        ),
        PlayFrame::Render { .. }
    ));
    // Lost: the frame is skipped and only the surface is made again.
    assert_eq!(
        frame(
            &mut window,
            &mut app,
            &mut session,
            &mut platform,
            SurfaceAcquire::Lost
        ),
        PlayFrame::Skip
    );
    assert_eq!(platform.inner.recreations, 1);
    // A recreation that fails (the compositor is not ready): play goes on, the status says why,
    // and the next lost frame tries again.
    platform.fail_recreate = true;
    assert_eq!(
        frame(
            &mut window,
            &mut app,
            &mut session,
            &mut platform,
            SurfaceAcquire::Lost
        ),
        PlayFrame::Skip
    );
    let state = call(app.world_mut(), "editor.play_state", serde_json::json!({})).expect("state");
    assert!(
        state["play_window"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("no surface")),
        "{state}"
    );
    platform.fail_recreate = false;
    assert_eq!(
        frame(
            &mut window,
            &mut app,
            &mut session,
            &mut platform,
            SurfaceAcquire::Lost
        ),
        PlayFrame::Skip
    );
    assert_eq!(platform.inner.recreations, 2);
    // Occluded / timed out: skipped, nothing recreated.
    assert_eq!(
        frame(
            &mut window,
            &mut app,
            &mut session,
            &mut platform,
            SurfaceAcquire::Unavailable
        ),
        PlayFrame::Skip
    );
    assert!(matches!(
        frame(
            &mut window,
            &mut app,
            &mut session,
            &mut platform,
            SurfaceAcquire::Ready
        ),
        PlayFrame::Render { .. }
    ));
    let counters = window.counters();
    assert_eq!(counters.surface_recreations, 2);
    assert_eq!(counters.frames_presented, 2);
    assert_eq!(counters.frames_skipped, 4);
    assert_eq!(counters.windows_opened, 1, "the window itself was never remade");
    assert_eq!(platform.inner.opened.len(), 1);
    assert_eq!(platform.inner.closed, 0);
    let state = call(app.world_mut(), "editor.play_state", serde_json::json!({})).expect("state");
    assert_eq!(state["play_window"]["error"], serde_json::Value::Null, "{state}");
    assert_eq!(state["play_window"]["counters"]["frames_presented"], 2, "{state}");
    // Play kept stepping through the losses: the ball fell for all seven frames.
    let running = session.as_mut().expect("session");
    assert_eq!(running.frames, 7);
    assert!(height(running.app.world_mut(), ball) < 5.0);
}

#[test]
fn the_play_window_drives_the_frame_when_the_editor_windows_redraws_stop() {
    let start = std::time::Instant::now();
    let at = |ms: u64| start + Duration::from_millis(ms);
    let mut driver = FrameDriver::default();
    // No play window: its redraws never drive anything.
    assert!(!driver.play_redraw_runs_frame(at(0), false));
    // Before the editor ran any frame the play window waits for it.
    driver.editor_frame_ran(at(0));
    let mut editor_frames = 1u32;
    let mut driven = 0u32;
    // 60 Hz: both windows redraw for 200 ms, then the compositor stops the editor's (hidden
    // behind the play window) and only the play window keeps redrawing, for 600 ms.
    for tick in 1..=48u64 {
        let now = at(tick * 50 / 3);
        let editor_redraws = tick <= 12;
        if editor_redraws {
            driver.editor_frame_ran(now);
            editor_frames += 1;
        }
        if driver.play_redraw_runs_frame(now, true) {
            // main.rs runs the editor frame from the play window's redraw.
            driver.editor_frame_ran(now);
            driven += 1;
        }
    }
    assert_eq!(editor_frames, 13);
    // While the editor redraws the play window never runs a second frame; after it stops, the
    // play window runs one every EDITOR_STALL (100 ms) of the 600 ms: 5 or 6 frames, never 0.
    assert!((5..=6).contains(&driven), "the play window drove {driven} frames");
    assert_eq!(
        regia_editor::play_window::EDITOR_STALL,
        Duration::from_millis(100)
    );
}

#[test]
fn choosing_a_resolution_while_the_play_window_is_open_resizes_the_games_window() {
    let mut platform = RecordingPlatform::default();
    let (mut app, mut session, mut window, _) = play_in_window(&mut platform);
    app.world_mut().init_resource::<EditorPreferences>();
    let play_id = window.id().expect("open");
    assert_eq!(window.render_size(), [1280, 720], "window-sized at first");
    // The toolbar's size menu while playing: the open window follows on the next sync.
    regia_editor::play_window::set_play_resolution(
        app.world_mut(),
        PlayResolution::preset("phone_9_19_5_portrait").expect("preset"),
    );
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert_eq!(window.id(), Some(play_id), "the same window");
    assert_eq!(window.render_size(), [1080, 2340]);
    assert_eq!(primary_window_sizes(&mut session), vec![(1080, 2340)]);
    let world = session.as_ref().expect("session").app.world();
    let resized: Vec<(u32, u32)> = world
        .resource::<Messages<regia_platform::WindowResized>>()
        .iter_current_update_messages()
        .map(|m| (m.width, m.height))
        .collect();
    assert_eq!(resized, vec![(1080, 2340)], "the game hears about it");
    assert_eq!(platform.opened.len(), 1);
    // A session that chose its own size keeps it when the preference changes.
    let (mut app, mut session, mut window) = {
        use serde_json::json;
        let mut app = edit_app();
        app.world_mut().insert_resource(PlayWindowSupport);
        app.world_mut().init_resource::<EditorPreferences>();
        call(
            app.world_mut(),
            "editor.play",
            json!({ "window": true, "size": [640, 480] }),
        )
        .expect("play");
        let mut session = None;
        drain(&mut app, &mut session);
        let mut window = PlayWindow::default();
        window.sync(app.world_mut(), session.as_mut(), &mut platform);
        (app, session, window)
    };
    regia_editor::play_window::set_play_resolution(app.world_mut(), PlayResolution::Window);
    regia_editor::play_window::set_play_resolution(
        app.world_mut(),
        PlayResolution::preset("hd_720").expect("preset"),
    );
    window.sync(app.world_mut(), session.as_mut(), &mut platform);
    assert_eq!(window.render_size(), [640, 480]);
    assert_eq!(primary_window_sizes(&mut session), vec![(640, 480)]);
}

// ---- zai-play-window-split: split-screen players in the play window (specs/19 § 4, ADR-7395) ----

/// PIE's stand-in cameras: `(player, viewport rect, order)` of every `PiePlayerCamera` in `world`.
fn stand_in_cameras(world: &mut World) -> Vec<(u8, [f32; 4], i32)> {
    world
        .query::<(&regia_editor::pie::PiePlayerCamera, &regia_render::Camera)>()
        .iter(world)
        .map(|(marker, camera)| (marker.player, camera.viewport, camera.order))
        .collect()
}

/// Every window camera's viewport in `world`, in draw order (order, then entity).
fn window_camera_viewports(world: &mut World) -> Vec<[f32; 4]> {
    let mut cameras: Vec<(i32, [f32; 4])> = world
        .query::<&regia_render::Camera>()
        .iter(world)
        .filter(|camera| matches!(camera.target, regia_render::RenderTarget::Window))
        .map(|camera| (camera.order, camera.viewport))
        .collect();
    cameras.sort_by_key(|(order, _)| *order);
    cameras.into_iter().map(|(_, rect)| rect).collect()
}

/// Plays `players` split-screen in the play window and returns the open window's id.
fn play_split_in_window(
    app: &mut App,
    session: &mut Option<PieSession>,
    window: &mut PlayWindow,
    platform: &mut RecordingPlatform,
    players: u8,
) -> u64 {
    use serde_json::json;
    app.world_mut().insert_resource(PlayWindowSupport);
    let answer = call(
        app.world_mut(),
        "editor.play",
        json!({ "window": true, "players": players }),
    )
    .expect("play");
    assert_eq!(answer["players"], players, "{answer}");
    assert_eq!(answer["window"], "play_window", "{answer}");
    drain(app, session);
    assert!(session.is_some(), "play started");
    window.sync(app.world_mut(), session.as_mut(), platform);
    window.id().expect("the play window opened")
}

#[test]
fn two_split_screen_players_get_one_pane_each_in_the_play_window() {
    use serde_json::json;
    let mut platform = RecordingPlatform::default();
    let mut app = edit_app();
    let mut session = None;
    let mut window = PlayWindow::default();
    play_split_in_window(&mut app, &mut session, &mut window, &mut platform, 2);

    // The window's frame is the whole window (no letterbox at the default resolution) ...
    assert_eq!(window.render_size(), [1280, 720]);
    match window.begin_frame(
        SurfaceAcquire::Ready,
        app.world_mut(),
        &mut session,
        &mut platform,
    ) {
        PlayFrame::Render { size, picture: at } => {
            assert_eq!(size, [1280, 720]);
            assert_eq!(at, Letterbox::fit([1280, 720], [1280, 720]));
        }
        PlayFrame::Skip => panic!("a ready surface renders"),
    }

    // ... and the play world holds one stand-in camera per joined player, laid out in the
    // player manager's split rects (the game has none; PIE stands in).
    let world = session.as_mut().expect("session").app.world_mut();
    let mut stand_ins = stand_in_cameras(world);
    stand_ins.sort_by_key(|(player, _, _)| *player);
    assert_eq!(
        stand_ins,
        vec![(0, [0.0, 0.0, 0.5, 1.0], 0), (1, [0.5, 0.0, 0.5, 1.0], 1),],
        "one camera per player at that player's rect, in player order"
    );
    assert_eq!(local_players(world), 2);
    assert_eq!(window_camera_viewports(world).len(), 2);

    // The agent surface reports the split: the session's players and viewports, and the
    // window's panes in window pixels (the picture fills the 1280 x 720 window).
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["players"], 2, "{state}");
    assert_eq!(state["viewports"].as_array().map(Vec::len), Some(2), "{state}");
    assert_eq!(state["play_window"]["players"], 2, "{state}");
    assert_eq!(
        state["play_window"]["panes"],
        json!([
            { "player": 0, "rect": [0, 0, 640, 720] },
            { "player": 1, "rect": [640, 0, 640, 720] },
        ]),
        "{state}"
    );

    // A game that spawns its own window camera owns presentation: PIE adds no stand-ins
    // over it (the game composites its own cameras, as in the shipped game).
    fn register_camera(app: &mut App) {
        app.add_plugins(PlayerPlugin);
    }
    fn spawn_camera(world: &mut World) {
        world.spawn((
            Name::new("game camera"),
            Transform::from_translation(Vec3::new(0.0, 3.0, 6.0)),
            regia_render::Camera::default(),
        ));
    }
    let game = game(register_camera, spawn_camera);
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    call(
        app.world_mut(),
        "editor.play",
        json!({ "window": true, "players": 2 }),
    )
    .expect("play");
    let mut session = None;
    drain_with(&mut app, &mut session, &game);
    let world = session.as_mut().expect("session").app.world_mut();
    assert!(
        stand_in_cameras(world).is_empty(),
        "no stand-ins over a game's own cameras"
    );
    assert_eq!(
        window_camera_viewports(world),
        vec![[0.0, 0.0, 1.0, 1.0]],
        "the game's camera covers the window"
    );
}

#[test]
fn four_split_screen_players_get_one_quarter_each() {
    use serde_json::json;
    let mut platform = RecordingPlatform::default();
    let mut app = edit_app();
    let mut session = None;
    let mut window = PlayWindow::default();
    let _ = play_split_in_window(&mut app, &mut session, &mut window, &mut platform, 4);

    let world = session.as_mut().expect("session").app.world_mut();
    let mut stand_ins = stand_in_cameras(world);
    stand_ins.sort_by_key(|(player, _, _)| *player);
    assert_eq!(
        stand_ins,
        vec![
            (0, [0.0, 0.0, 0.5, 0.5], 0),
            (1, [0.5, 0.0, 0.5, 0.5], 1),
            (2, [0.0, 0.5, 0.5, 0.5], 2),
            (3, [0.5, 0.5, 0.5, 0.5], 3),
        ],
        "four players sit in quarters, in player order"
    );
    assert_eq!(local_players(world), 4);

    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(state["players"], 4, "{state}");
    assert_eq!(state["play_window"]["players"], 4, "{state}");
    assert_eq!(
        state["play_window"]["panes"],
        json!([
            { "player": 0, "rect": [0, 0, 640, 360] },
            { "player": 1, "rect": [640, 0, 640, 360] },
            { "player": 2, "rect": [0, 360, 640, 360] },
            { "player": 3, "rect": [640, 360, 640, 360] },
        ]),
        "{state}"
    );
}

/// The play window's GPU frame (ADR-7395): a possessed pane draws every window view into its
/// viewport rect of the one target — two players as halves, four as quarters, the game's single
/// camera over the whole picture, and nothing (the editor camera) when the game has none.
#[test]
fn the_play_window_frame_composites_the_split_screen_views() {
    let device = gpu_device();
    let mut platform = RecordingPlatform::default();
    let mut app = edit_app();
    let mut session = None;
    let mut window = PlayWindow::default();
    play_split_in_window(&mut app, &mut session, &mut window, &mut platform, 2);
    window.begin_frame(
        SurfaceAcquire::Ready,
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    let world = session.as_mut().expect("session").app.world_mut();
    let frame = regia_editor::render::render_play_window_gpu(&device, world, [1280, 720], true)
        .expect("the play window frame");
    assert_eq!(frame.size(), [1280, 720]);
    assert_eq!(
        frame.views,
        vec![[0.0, 0.0, 0.5, 1.0], [0.5, 0.0, 0.5, 1.0]],
        "two players composite side by side"
    );
    // The frame drew the play world once (each view is a render into the same target).
    assert!(regia_editor::render::play_world_renders(world) >= 1);

    // Four players: quarters.
    let mut platform = RecordingPlatform::default();
    let mut app = edit_app();
    let mut session = None;
    let mut window = PlayWindow::default();
    play_split_in_window(&mut app, &mut session, &mut window, &mut platform, 4);
    window.begin_frame(
        SurfaceAcquire::Ready,
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    let world = session.as_mut().expect("session").app.world_mut();
    let frame = regia_editor::render::render_play_window_gpu(&device, world, [1280, 720], true)
        .expect("the play window frame");
    assert_eq!(
        frame.views,
        vec![
            [0.0, 0.0, 0.5, 0.5],
            [0.5, 0.0, 0.5, 0.5],
            [0.0, 0.5, 0.5, 0.5],
            [0.5, 0.5, 0.5, 0.5],
        ],
        "four players composite in quarters"
    );

    // One player and no game camera: the pane is the editor camera's, not possessed.
    let mut platform = RecordingPlatform::default();
    let mut app = edit_app();
    let mut session = None;
    let mut window = PlayWindow::default();
    play_split_in_window(&mut app, &mut session, &mut window, &mut platform, 1);
    window.begin_frame(
        SurfaceAcquire::Ready,
        app.world_mut(),
        &mut session,
        &mut platform,
    );
    let world = session.as_mut().expect("session").app.world_mut();
    let frame = regia_editor::render::render_play_window_gpu(&device, world, [1280, 720], true)
        .expect("the play window frame");
    assert!(
        frame.views.is_empty(),
        "an editor-camera pane composited no window views"
    );
}

/// The pixels of the composite (lavapipe): each player's pane shows that player's own view. The
/// stand-in cameras look the same way from spawn points 4 m apart, so an unlit red box on player
/// 1's view axis lands in the left pane and a green one on player 2's in the right.
#[test]
fn split_screen_panes_show_each_players_own_view() {
    use regia_render::{CpuMesh, LabMaterial, MeshRenderer};
    let device = gpu_device();
    let mut platform = RecordingPlatform::default();
    let mut app = edit_app();
    let mut session = None;
    let mut window = PlayWindow::default();
    play_split_in_window(&mut app, &mut session, &mut window, &mut platform, 2);
    window.begin_frame(
        SurfaceAcquire::Ready,
        app.world_mut(),
        &mut session,
        &mut platform,
    );

    // Unlit markers 2.5 m ahead of each stand-in camera (the cameras sit at
    // (4 * player, 2.5, 5) looking -Z, so player 0's marker is at (0, 2.5, 2.5)).
    let world = session.as_mut().expect("session").app.world_mut();
    world.insert_resource(device.clone());
    let marker = |world: &mut World, at: Vec3, color: [f32; 4]| {
        let (vertices, indices) = regia_render::box_mesh([1.0, 1.0, 1.0]);
        world.spawn((
            Name::new("marker"),
            Transform::from_translation(at),
            MeshRenderer::new(
                std::sync::Arc::new(CpuMesh { vertices, indices }),
                std::sync::Arc::new(LabMaterial {
                    base_color: color,
                    unlit: true,
                    ..LabMaterial::default()
                }),
            ),
        ));
    };
    marker(world, Vec3::new(0.0, 2.5, 2.5), [0.9, 0.05, 0.05, 1.0]);
    marker(world, Vec3::new(4.0, 2.5, 2.5), [0.05, 0.9, 0.05, 1.0]);

    let request = regia_editor::render::PaneRequest {
        pane: 0,
        kind: regia_editor::views::ViewKind::Perspective,
        camera: regia_editor::viewport::EditorCamera::default(),
        width: 480,
        height: 320,
        ids: false,
        warm_frames: 2,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    let frame = regia_editor::render::render_pane(&device, world, &request).expect("the pane renders");
    let pixel = |x: u32, y: u32| {
        let at = frame.image.pixels[(y * 480 + x) as usize];
        [at.r(), at.g(), at.b()]
    };
    let left = pixel(120, 160);
    let right = pixel(360, 160);
    assert!(
        left[0] > 110 && left[0] > left[1] + 50 && left[0] > left[2] + 50,
        "player 1's pane shows the red marker: {left:?}"
    );
    assert!(
        right[1] > 110 && right[1] > right[0] + 50 && right[1] > right[2] + 50,
        "player 2's pane shows the green marker: {right:?}"
    );
    assert_ne!(left, right, "the panes show two different views");
}

// ---- w8-play-window-standins: stand-ins follow the game's cameras (ADR-7466) ----

/// A window camera a game spawns from a system (`Startup` or `Update`), as the arena sample does.
fn spawn_game_window_camera(commands: &mut Commands, name: &'static str) {
    commands.spawn((
        Name::new(name),
        Transform::from_translation(Vec3::new(0.0, 3.0, 6.0)),
        regia_render::Camera::default(),
    ));
}

/// A game that spawns its window camera in `Startup` (the arena sample's `camera::spawn_camera`).
fn register_startup_camera(app: &mut App) {
    fn startup_camera(mut commands: Commands) {
        spawn_game_window_camera(&mut commands, "startup camera");
    }
    app.add_plugins(PlayerPlugin);
    app.add_systems(Startup, startup_camera);
}

/// A game that spawns its window camera on its third `Update` (a camera that waits for content).
fn register_late_camera(app: &mut App) {
    fn late_camera(mut runs: Local<u32>, mut commands: Commands) {
        *runs += 1;
        if *runs == 3 {
            spawn_game_window_camera(&mut commands, "late camera");
        }
    }
    app.add_plugins(PlayerPlugin);
    app.add_systems(Update, late_camera);
}

/// Review finding 1: a camera the game spawns in `Startup` (after PIE's join, in the first
/// frame) owns presentation: zero stand-ins and exactly the game's own window view. A camera a
/// game spawns later replaces the stand-ins; a camera-less game keeps one stand-in per player.
#[test]
fn a_game_camera_spawned_in_startup_or_later_gets_no_stand_ins() {
    let options = PlayOptions {
        players: 2,
        ..PlayOptions::default()
    };
    // Startup: no stand-in ever reaches a frame.
    let mut app = edit_app();
    let mut session = play(
        &mut app,
        options,
        Some(game(register_startup_camera, no_world_hook)),
    );
    for frame in 0..3 {
        let world = session.app.world_mut();
        assert_eq!(local_players(world), 2);
        assert!(
            stand_in_cameras(world).is_empty(),
            "frame {frame}: no stand-ins next to a Startup camera: {:?}",
            stand_in_cameras(world)
        );
        assert_eq!(
            window_camera_viewports(world),
            vec![[0.0, 0.0, 1.0, 1.0]],
            "frame {frame}: exactly the game's own window view"
        );
        assert!(session.frame().is_none());
    }

    // A camera the game spawns on its third frame: the stand-ins go once it is there.
    let mut app = edit_app();
    let mut session = play(&mut app, options, Some(game(register_late_camera, no_world_hook)));
    assert_eq!(
        stand_in_cameras(session.app.world_mut()).len(),
        2,
        "no game camera yet: one stand-in per player"
    );
    for _ in 0..3 {
        assert!(session.frame().is_none());
    }
    let world = session.app.world_mut();
    assert!(
        stand_in_cameras(world).is_empty(),
        "the game's late camera replaced the stand-ins: {:?}",
        stand_in_cameras(world)
    );
    assert_eq!(window_camera_viewports(world), vec![[0.0, 0.0, 1.0, 1.0]]);

    // Control: a camera-less game gets one stand-in per player, and keeps them.
    let mut app = edit_app();
    let three = PlayOptions {
        players: 3,
        ..PlayOptions::default()
    };
    let mut session = play(&mut app, three, Some(game(register_players, no_world_hook)));
    for _ in 0..2 {
        assert!(session.frame().is_none());
    }
    let world = session.app.world_mut();
    let mut stand_ins = stand_in_cameras(world);
    stand_ins.sort_by_key(|(player, _, _)| *player);
    assert_eq!(
        stand_ins,
        vec![
            (0, [0.0, 0.0, 1.0, 0.5], 0),
            (1, [0.0, 0.5, 0.5, 0.5], 1),
            (2, [0.5, 0.5, 0.5, 0.5], 2),
        ],
        "a camera-less game: one stand-in per player at its rect"
    );
}

/// The transform a game gives the controller of the player it joins itself.
fn placed_controller_transform() -> Transform {
    Transform {
        translation: Vec3::new(-7.25, 1.5, 3.125),
        rotation: regia_core::math::Quat::from_rotation_y(0.7),
        scale: Vec3::new(1.0, 2.0, 0.5),
    }
}

/// A game whose `on_load` joins its first local player and places that player's controller.
fn join_one_placed_player(world: &mut World) {
    if let Some((_, controller)) = PlayerManager::spawn_player_static(world, PlayerKind::Local) {
        world.entity_mut(controller).insert(placed_controller_transform());
    }
}

/// A transform's bits, for a bit-for-bit comparison.
fn transform_bits(transform: &Transform) -> Vec<u32> {
    let t = transform.translation.to_array();
    let r = transform.rotation.to_array();
    let s = transform.scale.to_array();
    t.iter()
        .chain(r.iter())
        .chain(s.iter())
        .map(|v| v.to_bits())
        .collect()
}

/// Review finding 2: PIE stands apart only the controllers it joined itself; a controller the
/// game joined keeps its transform bit for bit, even with stand-ins in the play world.
#[test]
fn a_game_joined_controller_keeps_its_transform_bit_for_bit() {
    let mut app = edit_app();
    let options = PlayOptions {
        players: 2,
        ..PlayOptions::default()
    };
    let mut session = play(
        &mut app,
        options,
        Some(game(register_players, join_one_placed_player)),
    );
    assert!(session.frame().is_none());
    let world = session.app.world_mut();
    assert_eq!(
        stand_in_cameras(world).len(),
        2,
        "a camera-less game gets stand-ins"
    );
    let controllers: Vec<(u8, Entity)> = world
        .query::<&Player>()
        .iter(world)
        .map(|player| (player.index, player.controller))
        .collect();
    let game_joined = controllers
        .iter()
        .find(|(index, _)| *index == 0)
        .map(|(_, controller)| *controller)
        .expect("the game's player");
    let pie_joined = controllers
        .iter()
        .find(|(index, _)| *index == 1)
        .map(|(_, controller)| *controller)
        .expect("PIE's player");
    let kept = world.get::<Transform>(game_joined).expect("controller transform");
    assert_eq!(
        transform_bits(kept),
        transform_bits(&placed_controller_transform()),
        "the game-joined controller keeps its transform: {kept:?}"
    );
    let stood = world.get::<Transform>(pie_joined).expect("controller transform");
    assert_eq!(
        stood.translation,
        Vec3::new(4.0, 0.0, 0.0),
        "PIE stands its own player 4 m along +X"
    );
}

/// Review finding 3: the stand-in cameras count in the session's entities (and `PieStatus`).
#[test]
fn stand_in_cameras_count_in_the_play_entities() {
    let mut app = edit_app();
    let one = play(&mut app, PlayOptions::default(), None);
    let control = one.entities;
    let mut app = edit_app();
    let options = PlayOptions {
        players: 2,
        ..PlayOptions::default()
    };
    let session = play(&mut app, options, None);
    assert_eq!(
        session.entities,
        control + 2 * 2 + 2,
        "two players and their controllers, plus one stand-in camera each"
    );
    sync_status(app.world_mut(), Some(&session));
    assert_eq!(app.world().resource::<PieStatus>().entities, control + 6);
}

/// Review finding 5: on an odd picture size the reported panes are the rects the composite
/// draws (`regia_render::pixel_viewport`'s floor-and-clamp), not rounded products.
#[test]
fn play_window_panes_equal_the_drawn_rects_on_an_odd_picture() {
    use serde_json::json;
    let mut app = edit_app();
    let halves = [[0.0, 0.0, 0.5, 1.0], [0.5, 0.0, 0.5, 1.0]];
    app.world_mut().insert_resource(PieStatus {
        player_views: halves
            .iter()
            .enumerate()
            .map(|(player, rect)| regia_editor::pie::PlayerView {
                player: u8::try_from(player).expect("two players"),
                rect: *rect,
            })
            .collect(),
        ..PieStatus::default()
    });
    app.world_mut()
        .insert_resource(regia_editor::play_window::PlayWindowStatus {
            open: true,
            size: [1279, 719],
            render_size: [1279, 719],
            picture: [0, 0, 1279, 719],
            ..Default::default()
        });
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    let drawn: Vec<serde_json::Value> = halves
        .iter()
        .enumerate()
        .map(|(player, rect)| json!({ "player": player, "rect": regia_render::pixel_viewport(*rect, 1279, 719) }))
        .collect();
    assert_eq!(state["play_window"]["panes"], json!(drawn), "{state}");
    assert_eq!(
        state["play_window"]["panes"],
        json!([
            { "player": 0, "rect": [0, 0, 640, 719] },
            { "player": 1, "rect": [639, 0, 640, 719] },
        ]),
        "{state}"
    );

    // A letterboxed picture: the panes sit inside it, offset by its origin.
    app.world_mut()
        .insert_resource(regia_editor::play_window::PlayWindowStatus {
            open: true,
            size: [1400, 719],
            render_size: [1279, 719],
            picture: [60, 0, 1279, 719],
            ..Default::default()
        });
    let state = call(app.world_mut(), "editor.play_state", json!({})).expect("state");
    assert_eq!(
        state["play_window"]["panes"],
        json!([
            { "player": 0, "rect": [60, 0, 640, 719] },
            { "player": 1, "rect": [699, 0, 640, 719] },
        ]),
        "{state}"
    );
}

/// Stand-ins follow the players (ADR-7466): a player who leaves loses its camera and the
/// survivor's camera takes the new full-window rect; a player the game joins at runtime gets one.
#[test]
fn stand_ins_follow_players_who_leave_and_join_at_runtime() {
    let mut app = edit_app();
    let options = PlayOptions {
        players: 2,
        ..PlayOptions::default()
    };
    let mut session = play(&mut app, options, Some(game(register_players, no_world_hook)));
    assert_eq!(stand_in_cameras(session.app.world_mut()).len(), 2);

    assert!(PlayerManager::remove_player_static(session.app.world_mut(), 1));
    for _ in 0..2 {
        assert!(session.frame().is_none());
    }
    assert_eq!(
        stand_in_cameras(session.app.world_mut()),
        vec![(0, [0.0, 0.0, 1.0, 1.0], 0)],
        "the survivor keeps one stand-in over the whole window"
    );

    let joined = PlayerManager::spawn_player_static(session.app.world_mut(), PlayerKind::Local);
    assert!(joined.is_some(), "a runtime join");
    for _ in 0..2 {
        assert!(session.frame().is_none());
    }
    let mut stand_ins = stand_in_cameras(session.app.world_mut());
    stand_ins.sort_by_key(|(player, _, _)| *player);
    assert_eq!(
        stand_ins.len(),
        2,
        "the runtime-joined player got a stand-in: {stand_ins:?}"
    );
    assert_eq!(stand_ins[0], (0, [0.0, 0.0, 0.5, 1.0], 0));
    assert_eq!(stand_ins[1].1, [0.5, 0.0, 0.5, 1.0]);
    assert_eq!(window_camera_viewports(session.app.world_mut()).len(), 2);
}

// ---- w9-editor-review-fixes: the play entity count follows the stand-ins (rev-claude-58 play 3, 4) ----

/// The count `editor.play_state` reports, as the editor loop mirrors it.
fn status_entities(app: &mut App, session: &PieSession) -> usize {
    sync_status(app.world_mut(), Some(session));
    app.world().resource::<PieStatus>().entities
}

/// rev-claude-58 play finding 3: the session's entities (and `PieStatus.entities`) count the
/// stand-in cameras that are there now, not only the ones the first frame spawned: a stand-in that
/// goes with its player, one a runtime join adds, and every one a late game camera replaces.
#[test]
fn the_play_entity_count_follows_the_stand_ins_after_start() {
    let options = PlayOptions {
        players: 2,
        ..PlayOptions::default()
    };
    let mut app = edit_app();
    let mut session = play(&mut app, options, Some(game(register_players, no_world_hook)));
    let at_start = session.entities;
    assert_eq!(stand_in_cameras(session.app.world_mut()).len(), 2);
    assert_eq!(status_entities(&mut app, &session), at_start);

    // Player 2 leaves: its stand-in goes (the player and controller are the game's to count).
    assert!(PlayerManager::remove_player_static(session.app.world_mut(), 1));
    for _ in 0..2 {
        assert!(session.frame().is_none());
    }
    assert_eq!(stand_in_cameras(session.app.world_mut()).len(), 1);
    assert_eq!(
        session.entities,
        at_start - 1,
        "the departed player's stand-in is gone"
    );
    assert_eq!(status_entities(&mut app, &session), at_start - 1);

    // A runtime join gets a stand-in, which counts again; a paused step counts the same way.
    session.mode = regia_editor::pie::PlayMode::Paused;
    assert!(PlayerManager::spawn_player_static(session.app.world_mut(), PlayerKind::Local).is_some());
    assert!(session.step(2).is_none());
    assert_eq!(stand_in_cameras(session.app.world_mut()).len(), 2);
    assert_eq!(session.entities, at_start, "the runtime join's stand-in counts");
    assert_eq!(status_entities(&mut app, &session), at_start);

    // A camera the game spawns later replaces both stand-ins: neither counts any more.
    let mut app = edit_app();
    let mut session = play(&mut app, options, Some(game(register_late_camera, no_world_hook)));
    let at_start = session.entities;
    assert_eq!(stand_in_cameras(session.app.world_mut()).len(), 2);
    for _ in 0..3 {
        assert!(session.frame().is_none());
    }
    assert!(stand_in_cameras(session.app.world_mut()).is_empty());
    assert_eq!(
        session.entities,
        at_start - 2,
        "the late camera replaced both stand-ins"
    );
    assert_eq!(status_entities(&mut app, &session), at_start - 2);
}

/// rev-claude-58 play finding 4, documented on book/agents/pie-modes.md: a player the game joins
/// at runtime gets a stand-in welded to its controller, but PIE never moves that controller (the
/// game joined it, ADR-7466), so it keeps the transform the game left it at.
#[test]
fn a_runtime_joined_controller_gets_a_stand_in_but_keeps_its_transform() {
    let mut app = edit_app();
    let options = PlayOptions {
        players: 2,
        ..PlayOptions::default()
    };
    let mut session = play(&mut app, options, Some(game(register_players, no_world_hook)));
    let (_, controller) = PlayerManager::spawn_player_static(session.app.world_mut(), PlayerKind::Local)
        .expect("a runtime join");
    for _ in 0..2 {
        assert!(session.frame().is_none());
    }
    assert_eq!(stand_in_cameras(session.app.world_mut()).len(), 3);
    let world = session.app.world();
    let kept = world.get::<Transform>(controller).expect("controller transform");
    assert_eq!(
        transform_bits(kept),
        transform_bits(&Transform::default()),
        "PIE does not stand a runtime-joined controller apart: {kept:?}"
    );
}

/// Smoke regression: the actual possessed editor render path, including each player count.
#[test]
fn smoke_split_screen_2_players_renders_without_validation_error() {
    smoke_split_screen_render(2, false);
}

#[test]
fn smoke_split_screen_3_players_renders_without_validation_error() {
    smoke_split_screen_render(3, false);
}

#[test]
fn smoke_split_screen_4_players_renders_without_validation_error() {
    smoke_split_screen_render(4, false);
}

#[test]
fn smoke_split_screen_mixed_hdr_ldr_layers_match_both_targets() {
    smoke_split_screen_render(3, true);
}

struct SmokeLdrLayer;

impl regia_render::TranslucentLayer for SmokeLdrLayer {
    fn simulate(
        &self,
        _depth: &wgpu::TextureView,
        _camera: &regia_render::LayerCamera,
        _encoder: &mut wgpu::CommandEncoder,
    ) {
    }

    fn draw(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}
}

/// Keep the HDR effect pass active even in an empty test level, as the play world's VFX
/// layer does during its initial warm frames. The renderer still records the real passes.
#[derive(Default)]
struct SmokeHdrLayer {
    depths: std::sync::Mutex<Vec<[u32; 2]>>,
}

impl regia_render::TranslucentLayer for SmokeHdrLayer {
    fn simulate(
        &self,
        depth: &wgpu::TextureView,
        camera: &regia_render::LayerCamera,
        _encoder: &mut wgpu::CommandEncoder,
    ) {
        assert!(camera.hdr, "fixture must exercise the HDR effects pass");
        self.depths
            .lock()
            .expect("depth observations")
            .push([depth.texture().width(), depth.texture().height()]);
    }

    fn draw(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

    fn participates_in_hdr(&self) -> bool {
        true
    }
}

fn smoke_split_screen_render(players: u8, mixed_layers: bool) {
    let device = gpu_device();
    let mut app = edit_app();
    let mut session = play(
        &mut app,
        PlayOptions {
            players,
            ..PlayOptions::default()
        },
        None,
    );
    session.app.update();
    let world = session.app.world_mut();
    let layer = std::sync::Arc::new(SmokeHdrLayer::default());
    world
        .get_resource_or_insert_with(regia_render::RuntimeRenderer::default)
        .0
        .set_translucent_layers(vec![layer.clone()]);
    if mixed_layers {
        world
            .resource_mut::<regia_render::RuntimeRenderer>()
            .0
            .insert_translucent_layer(1, std::sync::Arc::new(SmokeLdrLayer));
    }
    // The second frame changes both dimensions and gives the three-player layout unequal
    // pixel extents. Cached depth and scratch targets must follow each view after resizing.
    for size in [[1280, 720], [1279, 719]] {
        layer.depths.lock().expect("depth observations").clear();
        let frame = regia_editor::render::render_play_window_gpu(&device, world, size, true)
            .expect("split-screen frame");
        assert_eq!(frame.size(), size);
        assert_eq!(frame.views.len(), usize::from(players));
        let expected: Vec<_> = frame
            .views
            .iter()
            .map(|viewport| {
                let rect = regia_render::pixel_viewport(*viewport, size[0], size[1]);
                [rect[2], rect[3]]
            })
            .collect();
        assert_eq!(*layer.depths.lock().expect("depth observations"), expected);
    }
}

/// The names of every node under `nodes`, depth first.
fn capture_node_names(nodes: &[regia::telemetry::profile_capture::CaptureNode], out: &mut Vec<String>) {
    for node in nodes {
        out.push(node.name.clone());
        capture_node_names(&node.children, out);
    }
}

/// v06-editor-usability-b item 2 (ADR-8594, the w11 "profiler play attribution" gap): the editor's
/// pictures of the play world are play time. Playing in the viewport and in the play window, the
/// Profiler's play source holds the `pie_render` subtree with time in it and the edit source holds
/// none of it. Before the fix the play world's extraction and draw ran under the editor's frame
/// span only, so the edit source charged them to the editor and the play source never showed them.
///
/// The capture needs its own `tracing` registry, and a scoped registry cannot share a process
/// with a global one: `tracing_subscriber`'s registry ends an exited span through the thread's
/// *current* dispatcher, so a span of one registry entered while the other is the default is
/// closed on the wrong one ("tried to drop a ref to Id(..)"). Another test of this binary installs
/// the global log subscriber (`a_panicking_game_logs_a_backtrace_frame`), so the body runs in a
/// child process of this binary that runs this test only (lane v06-editor-b-reds).
#[test]
fn the_play_worlds_pictures_count_under_the_profilers_play_source() {
    const TEST: &str = "the_play_worlds_pictures_count_under_the_profilers_play_source";
    const CHILD: &str = "REGIA_TEST_PIE_PLAY_ATTRIBUTION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .expect("child test");
        let (stdout, stderr) = (
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            output.status.success(),
            "the child test failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("test result: ok. 1 passed"),
            "the child ran this test once:\n{stdout}\n{stderr}"
        );
        return;
    }
    assert!(
        tracing::dispatcher::get_default(|d| d.is::<tracing::subscriber::NoSubscriber>()),
        "the child process holds no global subscriber beside the capture's"
    );
    use serde_json::json;
    use tracing_subscriber::layer::SubscriberExt;
    let device = gpu_device();
    let root = regia_test_utils::unique_temp_dir("pie-play-attribution");
    std::fs::create_dir_all(root.join("assets/levels")).expect("root");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"Play Attribution\"\nid = \"com.example.attribution\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    let mut app = edit_app();
    app.world_mut().insert_resource(PlayWindowSupport);
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
    create_ball(&mut app);
    call(app.world_mut(), "editor.play", json!({})).expect("play");
    let mut session = None;
    drain(&mut app, &mut session);
    let mut shell = regia_editor::shell::EditorShell::new(Some(device.clone()), Some(root.clone()));
    let ctx = egui::Context::default();
    // Settle the dock and the first viewport frame outside the capture.
    for frame in 0..2 {
        shell_frame(&mut shell, &ctx, &mut app, &mut session, frame);
    }
    let profiler = regia_core::profiling::FrameProfiler::new();
    profiler.set_flame_enabled(true);
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
    let before = regia_editor::render::play_world_renders(session.as_ref().expect("session").app.world());
    tracing::dispatcher::with_default(&dispatch, || {
        for frame in 0..3 {
            let editor_frame = tracing::info_span!("editor_frame").entered();
            assert!(session.as_mut().expect("session").frame().is_none());
            shell_frame(&mut shell, &ctx, &mut app, &mut session, 2 + frame);
            // The play window's present (main.rs) draws the play world too.
            let world = session.as_mut().expect("session").app.world_mut();
            assert!(regia_editor::render::render_play_window_gpu(&device, world, [160, 90], true).is_some());
            drop(editor_frame);
            profiler.end_frame();
        }
    });
    let renders =
        regia_editor::render::play_world_renders(session.as_ref().expect("session").app.world()) - before;
    assert!(
        renders >= 6,
        "the viewport and the play window drew the play world each frame: {renders}"
    );
    app.world_mut().insert_resource(regia::Profiler(profiler));
    let whole = regia_editor::profiler_panel::current_capture(app.world()).expect("a capture");
    let play = regia_editor::profiler_model::split_source(&whole, true);
    let edit = regia_editor::profiler_model::split_source(&whole, false);
    let render = play
        .cpu
        .iter()
        .find(|node| node.name == "pie_render")
        .unwrap_or_else(|| panic!("the play source holds the play world's pictures: {:#?}", play.cpu));
    assert!(render.inclusive_ms > 0.0, "{render:?}");
    assert!(
        play.frame_ms + 0.01 >= render.inclusive_ms,
        "play time includes the pictures: {} < {}",
        play.frame_ms,
        render.inclusive_ms
    );
    let mut edit_names = Vec::new();
    capture_node_names(&edit.cpu, &mut edit_names);
    assert!(
        !edit_names.iter().any(|name| name == "pie_render"),
        "the edit source no longer charges the play world's pictures to the editor: {edit_names:?}"
    );
    assert!(
        edit_names.iter().any(|name| name == "editor_frame"),
        "{edit_names:?}"
    );
}
