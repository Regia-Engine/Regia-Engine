// SPDX-License-Identifier: MPL-2.0
//! The State Trees, Smart Objects and Cameras panels over the agent protocol (rule 8, ADR-2030): every
//! call goes through the real dispatcher, a real play session feeds the live views through the real
//! `pump` hooks, and a `.camera.ron` edit travels the real file watcher.
//!
//! The unit tests in the panel modules cover the behavior; this file proves the wiring. Nothing here
//! needs a window or a GPU, and no golden is involved.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_ai::blackboard::Blackboard;
use regia_ai::smart_object::{SmartObject, SmartObjectPlugin, SmartObjectSlot, claim_slot};
use regia_ai::state_tree::{StateTree, StateTreeDef};
use regia_ai::{AiPlugin, BtTaskRegistry};
use regia_app::prelude::*;
use regia_asset::{ExternalReloadSource, SourceAssetChanged, SourceKind};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_editor::pie::{PieRequests, PieSession, PieStatus};
use regia_editor::selection::Selection;
use regia_editor::{asset_watch, camera_panel, pie, shell::Panel, smart_object_panel, state_tree_panel};
use regia_framework::camera::CameraPlugin;
use regia_framework::camera::driver::{CameraAssets, CameraDriver};
use regia_framework::camera::load::{CameraAssetsPlugin, CameraFiles};
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

const CAMERA: &str = r#"(format_version: 1, name: "player",
    params: { "len": Float(4.0) },
    variables: [(name: "aiming", default: Bool(false))],
    rigs: [
        (name: "walk", root: [Boom(length: Param("len")), Fov(degrees: Literal(60.0))]),
        (name: "aim", root: [Fov(degrees: Literal(40.0))]),
    ],
    transitions: [(from: Any, to: Any, blend: Smooth(0.25))],
    director: Rules(default: "walk", rules: [(when: VarBool("aiming", true), rig: "aim")]))"#;

const TREE: &str = r#"(format_version: 2, root: "Root", states: [
    (name: "Root", children: ["Idle", "Alert"]),
    (name: "Idle", transitions: [(trigger: Event("heard"), target: State("Alert"))]),
    (name: "Alert"),
])"#;

#[test]
fn debug_overlay_selected_dispatch_contract() {
    let dir = project("debug-overlay-selected");
    let mut client = Client::new(&dir);
    let entity = client.app.world_mut().spawn(Transform::default()).id();
    let answer = client.call(
        "editor.debug_overlay",
        json!({ "entity": entity.to_bits(), "on": true }),
    );
    assert_eq!(answer["on"], true);
    assert_eq!(answer["entity"], entity.to_bits());
    assert_eq!(client.app.world().resource::<Selection>().primary(), Some(entity));
    let answer = client.call("editor.debug_overlay", json!({ "on": false }));
    assert_eq!(answer["on"], false);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn visual_log_panel_dispatch_contract() {
    let dir = project("visual-log-panel");
    let mut client = Client::new(&dir);
    let answer = client.call("editor.visual_log", json!({ "entities": [], "frame": 15 }));
    assert_eq!(answer["frame"], 15);
    assert_eq!(answer["entries"], json!([]));
    std::fs::remove_dir_all(dir).expect("cleanup");
}

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Client {
    /// The editor's edit world with the agent plugin, and the panels' resources as `main` sets them.
    fn new(project: &Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.add_plugins(CameraAssetsPlugin);
        app.init_resource::<regia_editor::command::UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>()
            .init_resource::<PieRequests>()
            .init_resource::<PieStatus>()
            .init_resource::<state_tree_panel::StateTreePanel>()
            .init_resource::<smart_object_panel::SmartObjectPanel>()
            .init_resource::<camera_panel::CameraPanel>()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(project.to_path_buf()),
            });
        let open = regia_editor::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
        }
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        self.app.update();
        let response = answer
            .try_recv()
            .expect("the dispatcher answers within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(value) => value,
            Err(err) => panic!("{method} {params}: {err:?}"),
        }
    }

    fn reason(&mut self, method: &str, params: Value) -> String {
        let err = self.try_call(method, params).expect_err("refused");
        err.data
            .as_ref()
            .and_then(|data| data["reason"].as_str())
            .unwrap_or_default()
            .to_owned()
    }
}

fn project(tag: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets")).expect("assets");
    dir
}

fn write(dir: &Path, rel: &str, text: &str) -> PathBuf {
    let file = dir.join("assets").join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
    regia_test_utils::write_atomic(&file, text.as_bytes()).expect("write");
    file
}

#[test]
fn the_panels_are_tabs_of_the_default_layout() {
    assert_eq!(Panel::StateTrees.title(), "State Trees");
    assert_eq!(Panel::SmartObjects.title(), "Smart Objects");
    assert_eq!(Panel::Cameras.title(), "Cameras");
    assert_eq!(Panel::SelectedDebug.title(), "Selected Object Debug");
    assert_eq!(Panel::VisualLog.title(), "Visual Log");
    let dock = regia_editor::editor_layout::default_dock();
    for panel in [Panel::SelectedDebug, Panel::VisualLog] {
        assert_eq!(Panel::from_id(panel.id()), Some(panel));
        assert!(dock.iter_all_tabs().any(|(_, tab)| *tab == panel));
    }
}

#[test]
fn the_camera_page_works_over_the_dispatcher_from_load_to_save() {
    let dir = project("gameplay-panels-camera");
    write(&dir, "camera/player.camera.ron", CAMERA);
    let mut client = Client::new(&dir);
    assert_eq!(
        client.reason("editor.camera_panel_load", json!({ "path": "../x.camera.ron" })),
        "camera_bad_path"
    );
    assert_eq!(
        client.reason("editor.camera_panel_load", json!({ "path": "none.camera.ron" })),
        "camera_file_not_found"
    );
    let scan = client.call("editor.camera_panel_load", json!({}));
    assert_eq!(scan["read"], 1);
    let state = client.call("editor.camera_panel_get", json!({}));
    assert_eq!(state["source"], "edit");
    assert_eq!(state["selected_asset"], "player");
    assert_eq!(state["page"]["params"][0]["name"], "len");
    let graph = client.call("editor.camera_rig_graph", json!({ "asset": "player" }));
    assert_eq!(graph["rigs"][0]["nodes"][0]["kind"], "boom");
    assert_eq!(graph["edges"][0]["from"], "any");
    let edit = client.call(
        "editor.camera_panel_edit",
        json!({ "param": "len", "value": 8.0 }),
    );
    assert_eq!(edit["status"], "stale");
    assert_eq!(client.call("editor.camera_panel_apply", json!({}))["revision"], 2);
    let saved = client.call("editor.camera_panel_save", json!({}));
    assert_eq!(saved["path"], "camera/player.camera.ron");
    let text = std::fs::read_to_string(dir.join("assets/camera/player.camera.ron")).expect("file");
    assert!(text.contains("8.0"), "{text}");
    // The registry the page uses is the one `camera.*` answers from.
    let assets = client.call("camera.assets", json!({}));
    assert_eq!(assets["assets"][0]["name"], "player");
    assert_eq!(assets["files"][0]["path"], "camera/player.camera.ron");
    let select = client.call("editor.camera_panel_select", json!({ "asset": "player" }));
    assert_eq!(select["selected_asset"], "player");
    assert_eq!(
        client.reason("editor.camera_panel_driver", json!({ "op": "set_var" })),
        "camera_no_driver"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_state_tree_and_smart_object_panels_read_the_edit_world_over_the_dispatcher() {
    let dir = project("gameplay-panels-edit");
    let mut client = Client::new(&dir);
    assert_eq!(client.call("editor.state_tree_panel_get", json!({}))["total"], 0);
    let world = client.app.world_mut();
    let object = world
        .spawn((Name::new("Chair"), Transform::default(), SmartObject::default()))
        .id();
    world.spawn((
        Name::new("Seat"),
        Transform::default(),
        SmartObjectSlot::new("sit"),
        ChildOf(object),
    ));
    let state = client.call("editor.smart_object_panel_get", json!({}));
    assert_eq!(state["objects"][0]["name"], "Chair");
    assert_eq!(state["objects"][0]["slots"][0]["state"], "free");
    assert_eq!(
        client.reason("editor.smart_object_panel_control", json!({ "op": "release" })),
        "entity_not_found"
    );
    assert_eq!(
        client.reason("editor.state_tree_panel_control", json!({ "op": "pause" })),
        "no_state_tree"
    );
    let set = client.call("editor.smart_object_panel_set", json!({ "show_gizmo": true }));
    assert_eq!(set["show_gizmo"], true);
    assert!(!smart_object_panel::smart_object_gizmo_segments(client.app.world()).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// One editor-loop pass: the play status, the panels' hooks, then the edit world's frame.
fn editor_frame(client: &mut Client, session: &mut PieSession) {
    pie::sync_status(client.app.world_mut(), Some(&*session));
    state_tree_panel::pump(client.app.world_mut(), Some(&mut *session));
    smart_object_panel::pump(client.app.world_mut(), Some(&mut *session));
    camera_panel::pump(client.app.world_mut(), Some(&mut *session));
    client.app.update();
}

/// A real play session running a game's trees, slots and camera.
struct Game {
    session: PieSession,
    tree: Entity,
    seat: Entity,
    guard: Entity,
    camera: Entity,
}

fn start_game(client: &mut Client, dir: &Path) -> Game {
    write(dir, "camera/player.camera.ron", CAMERA);
    let mut session = pie::start_play(client.app.world_mut(), None).expect("play starts");
    let app = &mut session.app;
    app.add_plugins((AiPlugin, SmartObjectPlugin, CameraPlugin));
    let world = app.world_mut();
    world
        .resource_mut::<CameraAssets>()
        .load_ron(
            CAMERA,
            Some("camera/player.camera.ron".to_owned()),
            &regia_framework::camera::director::CameraHooks::default(),
        )
        .expect("parses");
    let def = StateTreeDef::from_ron(TREE).expect("tree parses");
    let tree = StateTree::compile(&def, world.resource::<BtTaskRegistry>()).expect("compiles");
    let tree = world.spawn((Name::new("Bot"), tree, Blackboard::default())).id();
    let chair = world
        .spawn((Name::new("Chair"), Transform::default(), SmartObject::default()))
        .id();
    let seat = world
        .spawn((
            Name::new("Seat"),
            Transform::default(),
            SmartObjectSlot::new("sit"),
            ChildOf(chair),
        ))
        .id();
    let guard = world.spawn((Name::new("Guard"), Transform::default())).id();
    claim_slot(world, seat, guard).expect("claim");
    let camera = world
        .spawn((
            Name::new("Cam"),
            Transform::default(),
            CameraDriver::new("player"),
        ))
        .id();
    session.app.update();
    Game {
        session,
        tree,
        seat,
        guard,
        camera,
    }
}

#[test]
fn a_real_play_session_feeds_all_three_panels_and_takes_their_calls() {
    let dir = project("gameplay-panels-play");
    let mut client = Client::new(&dir);
    let mut game = start_game(&mut client, &dir);
    editor_frame(&mut client, &mut game.session);

    // The edit world has none of this: what comes back is the play world's.
    let trees = client.call("editor.state_tree_panel_get", json!({}));
    assert_eq!(trees["source"], "play");
    assert_eq!(trees["total"], 1);
    assert_eq!(trees["selected"], game.tree.to_bits());
    assert_eq!(trees["trees"][0]["name"], "Bot");
    let slots = client.call("editor.smart_object_panel_get", json!({}));
    assert_eq!(slots["source"], "play");
    assert_eq!(slots["counts"]["claimed"], 1);
    let seat = &slots["objects"][0]["slots"][0];
    assert_eq!(seat["holder"], game.guard.to_bits());
    assert_eq!(seat["holder_name"], "Guard");
    let cameras = client.call("editor.camera_panel_get", json!({}));
    assert_eq!(cameras["source"], "play");
    assert_eq!(cameras["play_framework"], true);
    assert_eq!(cameras["drivers"][0]["entity"], game.camera.to_bits());
    assert_eq!(cameras["live"]["primary"], "walk");

    // Each panel's call queues, runs on the play world in the pump, and reports back.
    let queued = client.call(
        "editor.state_tree_panel_control",
        json!({ "op": "send_event", "event": "heard" }),
    );
    assert_eq!(queued["queued"], true);
    let queued = client.call(
        "editor.smart_object_panel_control",
        json!({ "op": "release", "entity": game.seat.to_bits() }),
    );
    assert_eq!(queued["queued"], true);
    let queued = client.call(
        "editor.camera_panel_driver",
        json!({ "op": "set_var", "name": "aiming", "value": true }),
    );
    assert_eq!(queued["queued"], true);
    editor_frame(&mut client, &mut game.session);
    game.session.app.update();
    editor_frame(&mut client, &mut game.session);

    let trees = client.call("editor.state_tree_panel_get", json!({}));
    assert_eq!(trees["last_call"]["ok"], true);
    assert_eq!(trees["last_call"]["source"], "play");
    let slots = client.call("editor.smart_object_panel_get", json!({}));
    assert_eq!(slots["last_call"]["answer"]["released"], true);
    assert_eq!(slots["objects"][0]["slots"][0]["state"], "free");
    assert!(
        game.session
            .app
            .world()
            .get::<SmartObjectSlot>(game.seat)
            .expect("slot")
            .claimed_by
            .is_none()
    );
    let cameras = client.call("editor.camera_panel_get", json!({}));
    assert_eq!(cameras["last_call"]["ok"], true);
    assert_eq!(cameras["live"]["primary"], "aim");

    // Play ends: the panels look at the edit world again and the queue runs there.
    pie::stop_play(game.session);
    pie::sync_status(client.app.world_mut(), None);
    state_tree_panel::pump(client.app.world_mut(), None);
    smart_object_panel::pump(client.app.world_mut(), None);
    camera_panel::pump(client.app.world_mut(), None);
    let trees = client.call("editor.state_tree_panel_get", json!({}));
    assert_eq!(trees["source"], "edit");
    assert_eq!(trees["total"], 0);
    assert_eq!(
        trees["selected"],
        Value::Null,
        "a play entity id means nothing here"
    );
    let cameras = client.call("editor.camera_panel_get", json!({}));
    assert_eq!(cameras["source"], "edit");
    assert_eq!(cameras["drivers"], json!([]));
    assert_eq!(cameras["play_assets"], json!([]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_play_session_without_the_frameworks_shows_empty_panels() {
    let dir = project("gameplay-panels-bare");
    let mut client = Client::new(&dir);
    let mut session = pie::start_play(client.app.world_mut(), None).expect("play starts");
    editor_frame(&mut client, &mut session);
    let trees = client.call("editor.state_tree_panel_get", json!({}));
    assert_eq!(
        (trees["source"].clone(), trees["total"].clone()),
        (json!("play"), json!(0))
    );
    let cameras = client.call("editor.camera_panel_get", json!({}));
    assert_eq!(cameras["play_framework"], false);
    assert_eq!(
        client.call("editor.smart_object_panel_get", json!({}))["slots_total"],
        0
    );
    pie::stop_play(session);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pumps the watcher until `done`. The watcher answers an OS event on its own thread, so this waits
/// on it; the deadline only ends a run that would otherwise hang and does not measure anything.
fn pump_until(app: &mut App, what: &str, mut done: impl FnMut(&mut App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(120) {
        asset_watch::pump_asset_watch(app.world_mut(), None);
        app.update();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("{what}: the watcher delivered nothing");
}

fn watching_app(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ExternalReloadSource);
    app.add_plugins(regia_asset::AssetPlugin {
        settings: regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: regia_asset::current_platform(),
            content_dir: None,
        },
    });
    app.add_plugins(CameraAssetsPlugin);
    app.init_resource::<regia_editor::command::UndoStack>()
        .init_resource::<Selection>();
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.update();
    app
}

/// The whole path: a save on disk, the real watcher, the message, the framework's reload.
#[test]
fn saving_a_camera_file_reloads_it_through_the_real_watcher() {
    let dir = project("gameplay-panels-watch");
    let file = write(&dir, "camera/player.camera.ron", CAMERA);
    let mut app = watching_app(&dir);
    regia_framework::camera::load::load_camera_file(app.world_mut(), "camera/player.camera.ron")
        .expect("loads");
    assert_eq!(app.world().resource::<CameraAssets>().revision("player"), 1);
    asset_watch::start_asset_watch(app.world_mut(), &dir).expect("watch");

    write(
        &dir,
        "camera/player.camera.ron",
        &CAMERA.replace("Float(4.0)", "Float(7.5)"),
    );
    pump_until(&mut app, "good file", |app| {
        app.world().resource::<CameraAssets>().revision("player") >= 2
    });
    let active = app.world().resource::<CameraAssets>().active("player").cloned();
    assert_eq!(
        active.expect("active").params["len"].as_f32(),
        Some(7.5),
        "the running version is the new file"
    );

    // A broken save keeps the last good version and says why.
    write(&dir, "camera/player.camera.ron", "(format_version: 1, oops");
    pump_until(&mut app, "bad file", |app| {
        app.world()
            .resource::<CameraFiles>()
            .get("camera/player.camera.ron")
            .is_some_and(|f| f.error.is_some())
    });
    let assets = app.world().resource::<CameraAssets>();
    assert_eq!(
        assets.active("player").expect("active").params["len"].as_f32(),
        Some(7.5)
    );
    assert!(file.is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The data files whose owners listen for a change really get one from the watcher (ADR-2030): they
/// used to be classified as unknown files, which the watcher does not forward.
#[test]
fn gameplay_data_files_reach_their_listeners_through_the_real_watcher() {
    let dir = project("gameplay-panels-data");
    for rel in [
        "props/chair.smartobject.ron",
        "anim/hit.chooser.ron",
        "tables/falloff.curves.ron",
    ] {
        write(&dir, rel, "(format_version: 1)");
        assert_eq!(SourceKind::of(Path::new(rel)), SourceKind::GameplayData, "{rel}");
    }
    let mut app = watching_app(&dir);
    asset_watch::start_asset_watch(app.world_mut(), &dir).expect("watch");
    let wanted = Path::new("assets/props/chair.smartobject.ron");
    write(
        &dir,
        "props/chair.smartobject.ron",
        "(format_version: 1, slots: [])",
    );
    let mut cursor = MessageCursor::<SourceAssetChanged>::default();
    pump_until(&mut app, "smart object definition", |app| {
        let messages = app.world().resource::<Messages<SourceAssetChanged>>();
        cursor.read(messages).any(|change| change.path.ends_with(wanted))
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn selected_debug_and_visual_log_follow_a_real_play_world_through_dispatch() {
    use regia_core::math::{Vec2, Vec3};
    use regia_editor::{debug_overlay_selected, visual_log_panel};
    use regia_framework::visual_log::VisualLog;
    let dir = project("selected-debug-play");
    let mut client = Client::new(&dir);
    let mut session = pie::start_play(client.app.world_mut(), None).expect("play");
    let first = session
        .app
        .world_mut()
        .spawn((Transform::default(), regia_ai::perception::Perception::default()))
        .id();
    let second = session
        .app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::X))
        .id();
    pie::sync_status(client.app.world_mut(), Some(&session));
    let answer = client.call(
        "editor.debug_overlay",
        json!({ "entity": first.to_bits(), "on": true }),
    );
    assert_eq!(answer["queued"], true);
    debug_overlay_selected::pump(client.app.world_mut(), Some(&mut session));
    assert_eq!(session.app.world().resource::<Selection>().primary(), Some(first));
    let answer = client.call("editor.debug_overlay", json!({}));
    assert_eq!(answer["source"], "play");
    assert_eq!(answer["entity"], first.to_bits());
    assert!(answer["shape_count"].as_u64().expect("count") > 1);
    // A viewport/Outliner selection replaces the target without another debug-panel choice.
    session
        .app
        .world_mut()
        .resource_mut::<Selection>()
        .select(second, regia_editor::selection::SelectMode::Replace);
    debug_overlay_selected::pump(client.app.world_mut(), Some(&mut session));
    let answer = client.call("editor.debug_overlay", json!({}));
    assert_eq!(answer["entity"], second.to_bits());
    assert_eq!(
        answer["shape_count"], 1,
        "only the selected object's text, no first object's cone"
    );
    let mut log = VisualLog::new(16, 2);
    for frame in 10..=25 {
        log.begin_frame(frame);
        if frame <= 20 {
            regia_framework::debug_log!(first, line, log, Vec3::ZERO, Vec3::X);
        }
    }
    session.app.world_mut().insert_resource(log);
    client.call(
        "editor.visual_log",
        json!({ "entities": [first.to_bits()], "frame": 15 }),
    );
    visual_log_panel::pump(client.app.world_mut(), Some(&mut session));
    let answer = client.call("editor.visual_log", json!({}));
    assert_eq!(answer["source"], "play");
    assert_eq!(answer["entries"].as_array().expect("entries").len(), 1);
    assert_eq!(
        answer["entries"][0]["shape"],
        json!({ "kind": "line", "from": [0.0, 0.0, 0.0], "to": [1.0, 0.0, 0.0] })
    );
    let view = regia_editor::views::ViewCamera::of(
        regia_editor::views::ViewKind::Front,
        &regia_editor::viewport::EditorCamera::default(),
    );
    assert_eq!(
        visual_log_panel::overlays(session.app.world(), &view, Vec2::new(640.0, 480.0)).len(),
        1
    );
    client.call(
        "editor.visual_log",
        json!({ "entities": [second.to_bits()], "frame": 15 }),
    );
    visual_log_panel::pump(client.app.world_mut(), Some(&mut session));
    assert!(visual_log_panel::overlays(session.app.world(), &view, Vec2::new(640.0, 480.0)).is_empty());
    let error = client
        .try_call("editor.visual_log", json!({ "frame": -1 }))
        .expect_err("bad frame");
    assert_eq!(error.code, regia_agent::protocol::INVALID_PARAMS);
    std::fs::remove_dir_all(dir).expect("cleanup");
}
