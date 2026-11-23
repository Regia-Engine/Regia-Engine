// SPDX-License-Identifier: MPL-2.0
//! The logic graph's agent dogfood script (spec 102 § 13 P4 scope, "the agent dogfood script in CI";
//! rule 8): a designer's whole session with a logic graph, driven only through the agent protocol's
//! real dispatcher, so the merge gate runs it as `cargo test -p regia_editor --test
//! logic_graph_dogfood`.
//!
//! The script: `editor.asset_kinds` lists the kind → `editor.create_asset` writes a new graph (what
//! Content ▸ New ▸ Logic graph writes) → `logic.compile` accepts it → `editor.open_asset` opens it in
//! the Logic Graph panel → `editor.logic_edit` wires two rules (a refused wire changes nothing, an
//! extra edit is undone by `editor.undo`) → `editor.logic_validate` → `editor.logic_save` → the
//! runtime runs the saved file: `logic.emit`, `play.wait_until` on `LogicGraphStatus`,
//! `logic.status` → `logic.debug` breakpoint, `logic.control step` / `resume`, a disabled rule →
//! an edit saved and hot-reloaded with `logic.reload` keeps the locals → `editor.open_asset`
//! reopens the file unchanged. Fixed steps only; no wall-clock bars.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, Response, RpcError};
use regia_agent::server::Inbound;
use regia_ai::logic::{LogicGraph, LogicGraphDef, LogicInbox};
use regia_ai::{Blackboard, BtTaskRegistry};
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use serde_json::{Value, json};

const GRAPH: &str = "assets/logic/gate.logic.ron";

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Client {
    /// The editor's world on `project` with the agent plugin, the editor's methods and the AI runtime.
    fn new(project: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.add_plugins(regia_ai::AiPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        app.insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.to_path_buf()),
        });
        // `logic.compile { path }` and `logic.reload { path }` read under the asset server's root.
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: project.to_path_buf(),
            platform: regia_asset_pipeline::host_platform().to_string(),
            content_dir: None,
        })
        .expect("asset server");
        app.insert_resource(server);
        let level = regia_editor::level_io::new_level(app.world_mut());
        app.insert_resource(level);
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        permissions.config.heavy_rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
        }
    }

    /// Sends one request and runs frames until it is answered: one frame for an immediate method,
    /// up to `steps` fixed steps (each followed by a frame) for a deferred one (`play.wait_until`).
    fn send(&mut self, method: &str, params: Value, steps: usize) -> Response {
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
        for _ in 0..=steps {
            if let Ok(response) = answer.try_recv() {
                return response;
            }
            self.app.world_mut().run_schedule(FixedUpdate);
            self.app.update();
        }
        panic!("{method} did not answer within {steps} fixed steps");
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        match self.send(method, params, 0) {
            Response {
                error: Some(error), ..
            } => Err(error),
            Response {
                result: Some(value), ..
            } => Ok(value),
            _ => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(value) => value,
            Err(err) => panic!("{method} {params}: {err:?}"),
        }
    }

    /// `play.wait_until` on a `LogicGraphStatus` field, driven by fixed steps.
    fn wait_until(&mut self, entity: u64, path: &str, value: u64) -> Value {
        let response = self.send(
            "play.wait_until",
            json!({
                "condition": { "entity": entity, "component": "LogicGraphStatus", "path": path, "op": "gte", "value": value },
                "timeout_ms": 60_000
            }),
            16,
        );
        response
            .result
            .unwrap_or_else(|| panic!("play.wait_until {path}: {:?}", response.error))
    }

    fn step(&mut self) {
        self.app.world_mut().run_schedule(FixedUpdate);
    }
}

fn reason(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The two rules the script wires: `count` (on `go`, unless `stop` is set, `local/n` += 1) and
/// `blocked` (on `go`, when `stop` is set, `local/blocked` += 1).
fn build_ops() -> Value {
    json!([
        { "op": "set_local", "name": "n", "default": { "Int": 0 } },
        { "op": "set_local", "name": "blocked", "default": { "Int": 0 } },
        { "op": "add_event", "rule": "count", "event": { "EntityEvent": "go" }, "id": 1, "at": [0, 0] },
        { "op": "add_condition", "condition": { "Key": "stop" }, "id": 2, "at": [240, 0] },
        { "op": "set_condition_mode", "node": 2, "mode": "not" },
        { "op": "add_action", "task": "logic::SetBlackboard", "id": 3, "at": [480, 0], "set": {
            "key": { "Literal": { "String": "local/n" } },
            "op": { "Literal": { "String": "increment" } }
        } },
        { "op": "connect", "from": 1, "to": 2 },
        { "op": "connect", "from": 2, "to": 3 },
        { "op": "add_event", "rule": "blocked", "event": { "EntityEvent": "go" }, "id": 4, "at": [0, 160] },
        { "op": "add_condition", "condition": { "Key": "stop" }, "id": 5, "at": [240, 160] },
        { "op": "add_action", "task": "logic::SetBlackboard", "id": 6, "at": [480, 160], "set": {
            "key": { "Literal": { "String": "local/blocked" } },
            "op": { "Literal": { "String": "increment" } }
        } },
        { "op": "connect", "from": 4, "to": 5 },
        { "op": "connect", "from": 5, "to": 6 },
    ])
}

#[test]
fn a_designer_session_with_a_logic_graph_runs_end_to_end_over_the_agent_protocol() {
    let project = regia_test_utils::unique_temp_dir("logic_graph_dogfood");
    std::fs::create_dir_all(project.join("assets")).expect("assets");
    std::fs::write(project.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
    let mut client = Client::new(&project);

    // 1. The kind is listed for both Content ▸ New and open.
    let kinds = client.call("editor.asset_kinds", json!({}));
    let has = |table: &str| {
        kinds[table]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["kind"] == "logic_graph"))
    };
    assert!(has("create") && has("open"), "{kinds}");

    // 2. Create the graph as Content ▸ New ▸ Logic graph does; the runtime compiler accepts it.
    let created = client.call(
        "editor.create_asset",
        json!({ "kind": "logic_graph", "path": GRAPH }),
    );
    assert_eq!(created["path"], GRAPH);
    let compiled = client.call("logic.compile", json!({ "path": GRAPH }));
    assert_eq!(
        (compiled["ok"].clone(), compiled["rules"].clone()),
        (json!(true), json!([]))
    );

    // 3. Open it in the Logic Graph panel, as a double click does.
    let opened = client.call("editor.open_asset", json!({ "path": GRAPH }));
    assert_eq!(opened["kind"], "logic_graph");
    assert_eq!(opened["panel"], "Logic Graph");
    assert_eq!(opened["queued"], false);
    assert_eq!(opened["reply"]["rules"], 0);
    let state = client.call("editor.logic_state", json!({ "palette": true }));
    assert_eq!(state["dirty"], false);
    assert!(
        state["palette"]["tasks"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t == "logic::SetBlackboard")),
        "{}",
        state["palette"]
    );

    // 4. Wire two rules in one undoable edit.
    let built = client.call(
        "editor.logic_edit",
        json!({ "label": "wire the gate", "ops": build_ops() }),
    );
    assert_eq!(built["valid"], true, "{built}");
    assert_eq!(built["created"], json!([1, 2, 3, 4, 5, 6]));
    // A wire against the pin types is refused and changes nothing.
    let before = client.call("editor.logic_state", json!({ "ron": true }));
    let err = client
        .try_call(
            "editor.logic_edit",
            json!({ "ops": [{ "op": "connect", "from": 3, "to": 1 }] }),
        )
        .expect_err("pin type");
    assert_eq!(reason(&err), "pin_type");
    // An extra rule, then editor.undo takes it back.
    client.call(
        "editor.logic_edit",
        json!({ "label": "extra", "ops": [
            { "op": "add_event", "rule": "extra", "event": "Start", "id": 7, "at": [0, 320] }
        ] }),
    );
    assert_eq!(
        client.call("editor.logic_state", json!({}))["rules"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );
    let undone = client.call("editor.undo", json!({}));
    assert_eq!(undone["undone"], true, "{undone}");
    let after = client.call("editor.logic_state", json!({ "ron": true }));
    assert_eq!(after["ron"], before["ron"], "undo restores the wired graph");
    assert_eq!(after["rules"][0]["name"], "count");
    assert_eq!(after["rules"][0]["conditions"], json!([2]));
    assert_eq!(after["rules"][1]["name"], "blocked");

    // 5. Validate and save.
    let validated = client.call("editor.logic_validate", json!({}));
    assert_eq!(
        (validated["valid"].clone(), validated["rules"].clone()),
        (json!(true), json!(2))
    );
    let saved = client.call("editor.logic_save", json!({}));
    assert_eq!(saved["saves"], 1, "{saved}");
    assert!(project.join("assets/logic/gate.logic.layout.ron").is_file());
    let compiled = client.call("logic.compile", json!({ "path": GRAPH }));
    assert_eq!(
        (compiled["ok"].clone(), compiled["rules"].clone()),
        (json!(true), json!(["count", "blocked"]))
    );

    // 6. The runtime runs the saved file; the agent drives it and reads it back.
    let text = std::fs::read_to_string(project.join(GRAPH)).expect("saved file");
    let def = LogicGraphDef::from_ron(&text).expect("parses");
    let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
    let entity = client
        .app
        .world_mut()
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id()
        .to_bits();
    let queued = client.call("logic.emit", json!({ "entity": entity, "event": "go" }));
    assert_eq!(queued["queued"], 1);
    let waited = client.wait_until(entity, "rules[0].fires", 1);
    assert_eq!(waited["satisfied"], true, "{waited}");
    let status = client.call("logic.status", json!({ "entity": entity }));
    assert_eq!(status["locals"]["n"], 1, "{status}");
    assert_eq!(status["locals"]["blocked"], 0, "{status}");

    // 7. Debugging (dev builds): a rule breakpoint pauses at the end of the firing tick, step runs
    // exactly one tick, a disabled rule never matches, resume continues.
    if cfg!(debug_assertions) {
        let set = client.call(
            "logic.debug",
            json!({ "entity": entity, "pause_on_rule": ["count"] }),
        );
        assert_eq!(set["pause_on_rule"], json!(["count"]));
        client.call("logic.emit", json!({ "entity": entity, "event": "go" }));
        client.step();
        let hit = client.call("logic.status", json!({ "entity": entity }));
        assert_eq!(hit["paused"], true, "{hit}");
        assert_eq!(hit["locals"]["n"], 2, "{hit}");
        let ticks = hit["ticks"].as_u64().expect("ticks");
        for _ in 0..3 {
            client.step();
        }
        let held = client.call("logic.status", json!({ "entity": entity }));
        assert_eq!(held["ticks"].as_u64(), Some(ticks), "paused at the breakpoint");
        let stepped = client.call("logic.control", json!({ "entity": entity, "op": "step" }));
        assert_eq!(stepped["paused"], true);
        client.step();
        client.step();
        let after = client.call("logic.status", json!({ "entity": entity }));
        assert_eq!(after["ticks"].as_u64(), Some(ticks + 1), "one step is one tick");
        client.call(
            "logic.debug",
            json!({ "entity": entity, "pause_on_rule": [], "disabled": ["count"] }),
        );
        let resumed = client.call("logic.control", json!({ "entity": entity, "op": "resume" }));
        assert_eq!(resumed["status"], "running");
        client.call("logic.emit", json!({ "entity": entity, "event": "go" }));
        client.step();
        let disabled = client.call("logic.status", json!({ "entity": entity }));
        assert_eq!(
            disabled["locals"]["n"], 2,
            "a disabled rule never matches: {disabled}"
        );
        client.call(
            "logic.debug",
            json!({ "entity": entity, "pause_on_rule": [], "disabled": [] }),
        );
    }
    let n_before_reload = client.call("logic.status", json!({ "entity": entity }))["locals"]["n"].clone();

    // 8. Edit the open graph (the `count` rule's priority), save, hot-reload the running graph from
    // the saved file: its locals carry over by name.
    let edited = client.call(
        "editor.logic_edit",
        json!({ "label": "priority", "ops": [
            { "op": "set_event", "node": 1, "priority": 3 }
        ] }),
    );
    assert_eq!(edited["valid"], true, "{edited}");
    let saved = client.call("editor.logic_save", json!({}));
    assert_eq!(saved["saves"], 2);
    let reloaded = client.call("logic.reload", json!({ "entity": entity, "path": GRAPH }));
    assert_eq!(reloaded["outcome"], "kept", "{reloaded}");
    client.step();
    let kept = client.call("logic.status", json!({ "entity": entity }));
    assert_eq!(
        kept["locals"]["n"], n_before_reload,
        "locals survive the reload: {kept}"
    );
    client.call("logic.emit", json!({ "entity": entity, "event": "go" }));
    let fires = kept["rules"][0]["fires"].as_u64().expect("fires");
    let waited = client.wait_until(entity, "rules[0].fires", fires + 1);
    assert_eq!(
        waited["satisfied"], true,
        "the reloaded graph still runs: {waited}"
    );

    // 9. Reopen from disk: the saved graph, not dirty.
    let reopened = client.call("editor.open_asset", json!({ "path": GRAPH }));
    assert_eq!(reopened["reply"]["rules"], 2);
    let state = client.call("editor.logic_state", json!({}));
    assert_eq!(state["dirty"], false);
    assert_eq!(state["nodes"].as_array().map(Vec::len), Some(6));
    let _ = std::fs::remove_dir_all(&project);
}
