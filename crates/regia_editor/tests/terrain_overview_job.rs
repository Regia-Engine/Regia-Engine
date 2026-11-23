// SPDX-License-Identifier: MPL-2.0
//! Terrain ▸ Overview over the agent protocol (ADR-8596, CT-23): `editor.terrain_overview_start`
//! through the real dispatcher, `play.wait_until` on the job's agent events and `play.expect` on
//! the result.
//!
//! What each test proves:
//!
//! - the editor job, `terrain.bake_overview { register: true }` and the CLI's service
//!   (`regia_agent::publish_project_overview`, what `regia terrain source-overview` runs) publish
//!   the same pair and report the same mapping; a repeat is a cache hit that writes nothing;
//! - an entity marker lands on the pixel the reported mapping puts its world position on;
//! - `editor.cancel_job` before publication leaves no PNG, descriptor or temporary file, and an
//!   earlier pair byte for byte;
//! - the Terrain panel's fields build the same request.
//!
//! The worker is synchronised by a channel gate and by joining it
//! (`terrain_overview_job::wait_worker`), never by sleeps or wall-clock budgets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, Response, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use regia_editor::terrain_overview_job as tov;
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
    root: PathBuf,
}

impl Client {
    fn new(name: &str) -> Self {
        let root = regia_test_utils::unique_temp_dir(name);
        std::fs::create_dir_all(root.join("assets/maps")).expect("assets");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        app.insert_resource(
            regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            })
            .expect("asset server"),
        );
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<regia_agent::AgentEventLog>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        app.add_systems(Last, regia_editor::editor_jobs::tick_system);
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(root.clone()),
            });
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
            root,
        }
    }

    fn world(&mut self) -> &mut World {
        self.app.world_mut()
    }

    fn send(&mut self, method: &str, params: Value) -> crossbeam_channel::Receiver<Response> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        answer
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let answer = self.send(method, params);
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

    /// `play.wait_until` on the job event `<job> <state>`: the request is sent, the worker is joined,
    /// then a bounded number of frames ends the job and answers the wait.
    fn wait_for(&mut self, job: &str, state: &str) {
        let answer = self.send(
            "play.wait_until",
            json!({
                "condition": { "events_contain": { "kind": tov::EVENT_KIND, "contains": format!("{job} {state}") } },
                "timeout_ms": 60_000
            }),
        );
        tov::wait_worker(self.world());
        for _ in 0..8 {
            self.app.update();
            if let Ok(response) = answer.try_recv() {
                assert!(
                    response.error.is_none(),
                    "wait for {job} {state}: {:?}",
                    response.error
                );
                return;
            }
        }
        panic!("{job} never reached {state} within 8 frames after its worker returned");
    }

    /// `play.expect` on the job event: the assertion holds.
    fn expect_event(&mut self, job: &str, state: &str) {
        let report = self.call(
            "play.expect",
            json!({ "assertions": [{ "name": format!("{job} {state}"),
                "condition": { "events_contain": { "kind": tov::EVENT_KIND, "contains": format!("{job} {state}") } } }] }),
        );
        assert_eq!(report["pass"], true, "{report}");
    }
}

/// A schema-2 source of 12 x 6 samples, 2 m apart, first sample at (-11, -5): the cells span X
/// -12..12 and Z -6..6 (2:1).
fn write_source(root: &Path) {
    let counts: Vec<u16> = (0..72u16).map(|i| 1000 + 7 * (i % 12) + 3 * (i / 12)).collect();
    let doc = json!({ "schema": 2,
        "grid": { "cols": 12, "rows": 6, "cell_m": 2.0, "sample0_x_m": -11.0, "sample0_z_m": -5.0 },
        "units": { "kind": "counts", "meters_per_count": 0.125, "zero_counts": 1000 },
        "counts": counts });
    regia_agent::bake_export::write_atomic(&root.join("assets/src.json"), doc.to_string().as_bytes())
        .expect("write");
}

fn request() -> Value {
    json!({
        "path": "assets/src.json",
        "output": "assets/maps/ov.png",
        "overview": { "size": [64, 64], "fit": "preserve_aspect", "blur_passes": 0 },
        "markers": [
            { "label": "origin", "xz": [0.0, 0.0] },
            { "entity": { "name": "Spawn" } },
        ],
    })
}

/// Every regular file under `root` with its bytes.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                // Regular files only: the agent's socket under the project state folder is not
                // readable as a file.
                out.insert(path.clone(), std::fs::read(&path).expect("read"));
            }
        }
    }
    out
}

fn spawn_marker(world: &mut World) {
    world.spawn((
        Name::new("Spawn"),
        Transform::from_translation(regia_core::math::Vec3::new(4.0, 7.0, -2.0)),
    ));
}

#[test]
fn the_job_the_agent_and_the_cli_publish_one_registered_pair() {
    let mut client = Client::new("tov_publish");
    write_source(&client.root);
    spawn_marker(client.world());
    let started = client.call("editor.terrain_overview_start", request());
    let job = started["job"].as_str().expect("job").to_owned();
    assert!(job.starts_with("terrain_overview:"), "{job}");
    client.wait_for(&job, "done");
    client.expect_event(&job, "done");
    let status = client.call("editor.terrain_overview_status", json!({ "job": job }));
    assert_eq!(status["state"], "done", "{status}");
    let result = &status["result"];
    assert_eq!(result["registered"], true);
    assert_eq!(result["cached"], false);
    assert_eq!(result["descriptor"], "assets/maps/ov.overview.json");
    assert_eq!(result["fit"], "preserve_aspect");
    // 24 m x 12 m into 64 x 64: 64 x 32 centered, 0.375 m per pixel.
    assert_eq!(result["mapping"]["content_px"], json!([0, 16, 64, 32]));
    assert_eq!(result["mapping"]["world_min"], json!([-12.0, -6.0]));
    assert_eq!(result["mapping"]["world_max"], json!([12.0, 6.0]));
    assert_eq!(result["markers"][0]["pixel"], json!([32, 32]));
    // The entity at X 4, Z -2: u = 16 / 0.375, v = 16 + 8 / 0.375.
    assert_eq!(result["markers"][1]["label"], "Spawn");
    assert_eq!(result["markers"][1]["world_xz"], json!([4.0, -2.0]));
    assert_eq!(result["markers"][1]["pixel"], json!([42, 37]));
    let job_status = client.call("agent.job_status", json!({ "id": job }));
    assert_eq!(job_status["state"], "done", "{job_status}");

    let png = client.root.join("assets/maps/ov.png");
    let descriptor = client.root.join("assets/maps/ov.overview.json");
    let published = tree(&client.root.join("assets/maps"));
    assert_eq!(published.len(), 2, "{:?}", published.keys());
    let text: Value = serde_json::from_slice(&std::fs::read(&descriptor).expect("descriptor")).expect("json");
    assert_eq!(text["mapping"], result["mapping"]);
    assert_eq!(text["source"]["path"], "assets/src.json");

    // The agent's synchronous call with the same request: a cache hit, nothing rewritten.
    let mut agent = request();
    agent["register"] = json!(true);
    let reply = client.call("terrain.bake_overview", agent.clone());
    assert_eq!(reply["cached"], true, "{reply}");
    assert_eq!(reply["mapping"], result["mapping"]);
    assert_eq!(reply["markers"], result["markers"]);
    assert_eq!(tree(&client.root.join("assets/maps")), published);

    // The CLI's service with the entity marker given as coordinates: the same pair.
    let mut cli = request();
    cli["markers"][1] = json!({ "label": "Spawn", "xz": [4.0, -2.0] });
    let markers = regia_agent::overview_markers(None, &cli).expect("markers");
    let out = regia_agent::publish_project_overview(&client.root, Default::default(), &cli, markers, |_| {
        std::ops::ControlFlow::Continue(())
    })
    .expect("cli");
    assert_eq!(out["cached"], true, "{out}");

    // A changed setting bakes again; the unregistered legacy call writes only its PNG.
    agent["overview"]["relief"] = json!(2.0);
    assert_eq!(client.call("terrain.bake_overview", agent)["cached"], false);
    let legacy = client.call(
        "terrain.bake_overview",
        json!({ "path": "assets/src.json", "output": "assets/legacy.png" }),
    );
    assert_eq!(legacy["registered"], false);
    assert_eq!(legacy["fit"], "stretch");
    assert!(client.root.join("assets/legacy.png").exists());
    assert!(!client.root.join("assets/legacy.overview.json").exists());
    let _ = png;
}

#[test]
fn cancel_before_publication_writes_nothing() {
    let mut client = Client::new("tov_cancel");
    write_source(&client.root);
    spawn_marker(client.world());
    // An earlier pair from other settings must survive the cancelled bake byte for byte.
    let mut earlier = request();
    earlier["overview"]["relief"] = json!(3.0);
    earlier["register"] = json!(true);
    client.call("terrain.bake_overview", earlier);
    let files = tree(&client.root.join("assets"));

    let (release, gate) = std::sync::mpsc::channel::<()>();
    client
        .world()
        .insert_resource(tov::TerrainOverviewStartGate(std::sync::Mutex::new(Some(gate))));
    let started = client.call("editor.terrain_overview_start", request());
    let job = started["job"].as_str().expect("job").to_owned();
    let status = client.call("editor.terrain_overview_status", json!({ "job": job }));
    assert_eq!(status["state"], "running", "{status}");
    let busy = client
        .try_call("editor.terrain_overview_start", request())
        .expect_err("one job at a time");
    assert_eq!(
        busy.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str),
        Some("terrain_overview_busy")
    );
    client.call("editor.cancel_job", json!({ "id": job }));
    release.send(()).expect("release");
    client.wait_for(&job, "cancelled");
    client.expect_event(&job, "cancelled");
    let status = client.call("editor.terrain_overview_status", json!({ "job": job }));
    assert_eq!(status["state"], "cancelled", "{status}");
    assert_eq!(status["published"], false);
    // The project's own state (the agent's audit log) grows with every call; the assets do not.
    assert_eq!(
        tree(&client.root.join("assets")),
        files,
        "no PNG, descriptor or temporary file changed"
    );
}

#[test]
fn the_panel_builds_the_agent_request() {
    let source = regia_editor::terrain_source_import::TerrainSourcePanel {
        path: "assets/src.json".into(),
        mapping: "assets/maps/heightfield.json".into(),
        png: String::new(),
        ..Default::default()
    };
    let panel = tov::TerrainOverviewPanel {
        output: "assets/maps/ov.png".into(),
        preserve_aspect: true,
        size: [256, 128],
        message: String::new(),
    };
    assert_eq!(
        panel.params(&source),
        json!({
            "path": "assets/src.json",
            "output": "assets/maps/ov.png",
            "mapping": "assets/maps/heightfield.json",
            "overview": { "size": [256, 128], "fit": "preserve_aspect" },
        })
    );
    let stretch = tov::TerrainOverviewPanel {
        preserve_aspect: false,
        ..panel
    };
    assert_eq!(stretch.params(&source)["overview"]["fit"], "stretch");
}
