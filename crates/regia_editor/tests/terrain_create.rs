// SPDX-License-Identifier: MPL-2.0
//! Terrain ▸ New terrain over the agent protocol (ADR-7727, rule 8): `editor.terrain_create`
//! through the real dispatcher (registration, parameter schema, permissions), then
//! `terrain.level_terrain` reports the terrain the level now names, `editor.cancel_job` reaches an
//! async create, and the documented refusals keep their codes. The unit tests in
//! `regia_editor::terrain_create` call the handlers directly.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use serde_json::{Value, json};

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Client {
    /// The editor's edit world with the agent plugin, a project at `project` and a saved level
    /// `assets/maps/arena.level.ron` open.
    fn new(project: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(project.to_path_buf()),
            });
        app.world_mut()
            .insert_resource(regia_editor::level_io::OpenLevel {
                id: regia_scene::level::LevelId::default(),
                path: Some(project.join("assets/maps/arena.level.ron")),
            });
        app.world_mut()
            .insert_resource(regia_scene::level::LevelSettings::default());
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
}

/// The error's data code.
fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[test]
fn terrain_create_reaches_the_level_and_terrain_level_terrain_reports_it() {
    let project = regia_test_utils::unique_temp_dir("terrain_create_agent_e2e");
    std::fs::create_dir_all(project.join("assets/maps")).expect("assets");
    let mut client = Client::new(&project);
    let created = client.call(
        "editor.terrain_create",
        json!({ "name": "hills", "size": 129, "cell_size": 1.0, "height_range": [-10.0, 90.0] }),
    );
    assert_eq!(created["state"], "done", "{created}");
    assert_eq!(
        created["layers"], "assets/maps/hills/hills.terrain.layers.ron",
        "{created}"
    );
    assert_eq!(created["loaded"], true, "{created}");
    let entity = created["entity"].as_u64().expect("entity");

    let report = client.call(
        "terrain.level_terrain",
        json!({ "level": "maps/arena.level.ron" }),
    );
    assert_eq!(
        report["terrain"]["layers"], "maps/hills/hills.terrain.layers.ron",
        "{report}"
    );
    assert_eq!(report["terrain"]["world_width_m"], 128.0, "{report}");
    assert_eq!(
        report["files"]["holes"]["path"], "maps/hills/hills.holes.ron",
        "{report}"
    );
    assert_eq!(report["loaded"][0]["entity"], entity, "{report}");

    // The documented refusals keep their codes through the dispatcher.
    let exists = client
        .try_call("editor.terrain_create", json!({ "name": "hills", "size": 129 }))
        .expect_err("exists");
    assert_eq!(code(&exists), "terrain_create_exists");
    let outside = client
        .try_call(
            "editor.terrain_create",
            json!({ "name": "x", "size": 129, "folder": "../outside" }),
        )
        .expect_err("outside");
    assert_eq!(code(&outside), "terrain_create_bad_path");
    let size = client
        .try_call("editor.terrain_create", json!({ "name": "x", "size": 130 }))
        .expect_err("size");
    assert_eq!(code(&size), "terrain_create_bad_size");

    // Undo removes the level's reference; the files stay.
    client.call("editor.undo", json!({}));
    let report = client.call("terrain.level_terrain", json!({}));
    assert_eq!(report["terrain"], Value::Null, "{report}");
    assert!(project.join("assets/maps/hills/heightfield.png").is_file());
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn an_async_create_is_a_job_the_editor_ticks_to_its_end() {
    let project = regia_test_utils::unique_temp_dir("terrain_create_agent_async");
    std::fs::create_dir_all(project.join("assets/maps")).expect("assets");
    let mut client = Client::new(&project);
    let started = client.call(
        "editor.terrain_create",
        json!({ "name": "dunes", "size": 129, "async": true }),
    );
    let job = started["job"].as_str().expect("job").to_owned();
    assert!(job.starts_with("terrain_create:"), "{started}");
    // A second create while one runs is refused, not queued.
    if client
        .app
        .world()
        .contains_resource::<regia_editor::terrain_create::TerrainCreateJob>()
    {
        let busy = client
            .try_call("editor.terrain_create", json!({ "name": "other", "size": 129 }))
            .expect_err("busy");
        assert_eq!(code(&busy), "terrain_create_busy");
    }
    // The editor's per-frame tick applies the end once the worker returns.
    let mut state = Value::Null;
    for _ in 0..100_000 {
        regia_editor::editor_jobs::tick(client.app.world_mut());
        state = client.call("agent.job_status", json!({ "id": job }));
        if state["state"] != "running" {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(state["state"], "done", "{state}");
    let report = client.call("terrain.level_terrain", json!({}));
    assert_eq!(
        report["terrain"]["layers"], "maps/dunes/dunes.terrain.layers.ron",
        "{report}"
    );
    let _ = std::fs::remove_dir_all(&project);
}
