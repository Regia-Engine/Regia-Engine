// SPDX-License-Identifier: MPL-2.0
//! The Logic Graph panel over the agent protocol (spec 102 § 13 P2, rule 8): an agent builds a graph
//! with `editor.logic_edit`, saves it with `editor.logic_save`, reopens and validates it, and the
//! phase-1 runtime runs the saved file, driven by `logic.emit` and read back by `logic.status`. Every
//! call goes through the real dispatcher (registration, parameter schema, permissions), fed from an
//! in-process inbox so each one is answered within one frame. Fixed steps only.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_ai::logic::{LogicGraph, LogicGraphDef, LogicInbox};
use regia_ai::{Blackboard, BtTaskRegistry};
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
    /// The editor's world with the agent plugin, the editor's extension methods and the AI runtime.
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

fn reason(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[test]
fn an_agent_builds_saves_reopens_and_runs_a_logic_graph() {
    let project = regia_test_utils::unique_temp_dir("logic_graph_editor_e2e");
    let mut client = Client::new(&project);
    let err = client
        .try_call("editor.logic_state", json!({}))
        .expect_err("nothing open");
    assert_eq!(reason(&err), "no_graph_open");

    let opened = client.call(
        "editor.logic_open",
        json!({ "create": true, "path": "logic/counter.logic.ron" }),
    );
    assert_eq!(opened["rules"], 0);
    let built = client.call(
        "editor.logic_edit",
        json!({ "label": "build", "ops": [
            { "op": "set_local", "name": "n", "default": { "Int": 0 } },
            { "op": "add_event", "rule": "count", "event": { "EntityEvent": "go" }, "id": 1, "at": [0, 0] },
            { "op": "add_condition", "condition": { "Key": "stop" }, "id": 2, "at": [240, 0] },
            { "op": "set_condition_mode", "node": 2, "mode": "not" },
            { "op": "add_action", "task": "logic::SetBlackboard", "id": 3, "at": [480, 0], "set": {
                "key": { "Literal": { "String": "local/n" } },
                "op": { "Literal": { "String": "increment" } }
            } },
            { "op": "connect", "from": 1, "to": 2 },
            { "op": "connect", "from": 2, "to": 3 },
        ] }),
    );
    assert_eq!(built["valid"], true, "{built}");
    assert_eq!(built["created"], json!([1, 2, 3]));
    // A wire against the pin types is refused with its reason and changes nothing.
    let err = client
        .try_call(
            "editor.logic_edit",
            json!({ "ops": [{ "op": "connect", "from": 3, "to": 1 }] }),
        )
        .expect_err("pin type");
    assert_eq!(reason(&err), "pin_type");
    let state = client.call("editor.logic_state", json!({ "ron": true, "palette": true }));
    assert_eq!(state["rules"][0]["name"], "count");
    assert_eq!(state["rules"][0]["conditions"], json!([2]));
    assert_eq!(state["rules"][0]["actions"], json!([3]));
    assert_eq!(state["dirty"], true);
    assert!(
        state["palette"]["tasks"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t == "logic::EmitEvent"))
    );

    let saved = client.call("editor.logic_save", json!({}));
    assert_eq!(saved["saves"], 1);
    let path = project.join("logic/counter.logic.ron");
    assert!(path.exists() && project.join("logic/counter.logic.layout.ron").exists());

    // Reopen from disk: same text, not dirty; validation by path agrees.
    client.call("editor.logic_open", json!({ "path": "logic/counter.logic.ron" }));
    let again = client.call("editor.logic_state", json!({ "ron": true }));
    assert_eq!(again["ron"], state["ron"]);
    assert_eq!(again["dirty"], false);
    assert_eq!(again["nodes"].as_array().map(Vec::len), Some(3));
    let validated = client.call(
        "editor.logic_validate",
        json!({ "path": "logic/counter.logic.ron" }),
    );
    assert_eq!(
        (validated["valid"].clone(), validated["rules"].clone()),
        (json!(true), json!(1))
    );

    // The phase-1 runtime runs the saved file; the agent drives and reads it.
    let text = std::fs::read_to_string(&path).expect("saved file");
    let def = LogicGraphDef::from_ron(&text).expect("parses");
    let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
    let e = client
        .app
        .world_mut()
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id();
    let bits = e.to_bits();
    for _ in 0..2 {
        let queued = client.call("logic.emit", json!({ "entity": bits, "event": "go" }));
        assert_eq!(queued["queued"], 1);
        client.app.world_mut().run_schedule(FixedUpdate);
    }
    let status = client.call("logic.status", json!({ "entity": bits }));
    assert_eq!(status["rules"][0]["fires"], 2, "{status}");
    assert_eq!(status["locals"]["n"], 2, "{status}");
    let _ = std::fs::remove_dir_all(&project);
}
