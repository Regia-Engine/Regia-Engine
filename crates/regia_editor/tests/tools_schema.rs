// SPDX-License-Identifier: MPL-2.0
//! `agent.schema` for the editor tools, and `quit.request` in the edit world (ADR-1582), through
//! the real dispatcher from an in-process inbox. No clocks: frames run until the answer arrives.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::tools::host::ViewItem;
use serde_json::{Value, json};

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Agent {
    /// The edit world as `main.rs` builds it, without the window or the renderer.
    fn new() -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.add_plugins(regia_agent::AgentPlugin);
        app.world_mut().spawn(regia_platform::PrimaryWindow);
        regia_editor::quit::install(&mut app);
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

    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        // The request is taken in the next frame's PreUpdate; a handful of frames is plenty.
        let response = loop {
            self.app.update();
            if let Ok(response) = answer.try_recv() {
                break response;
            }
        };
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(Value::Null),
        }
    }

    fn method_schema(&mut self, method: &str) -> Value {
        let table = self
            .call("agent.schema", json!({ "method": method }))
            .expect("schema");
        table["methods"][0].clone()
    }
}

/// True when any JSON Schema `type` in `schema` is the non-standard word `any`.
fn has_any_type(schema: &Value) -> bool {
    match schema {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| (key == "type" && value == "any") || has_any_type(value)),
        Value::Array(items) => items.iter().any(has_any_type),
        _ => false,
    }
}

const TOOL_METHODS: [&str; 11] = [
    "editor.tool_list",
    "editor.tool_status",
    "editor.tool_methods",
    "editor.tool_launch",
    "editor.tool_stop",
    "editor.tool_click",
    "editor.tool_panel",
    "editor.tool_progress",
    "editor.tool_reply",
    "editor.tool_set_view",
    "editor.tool_call",
];

#[test]
fn the_tool_methods_are_in_agent_schema_with_no_any() {
    let mut agent = Agent::new();
    let table = agent.call("agent.schema", json!({})).expect("whole table");
    let names: Vec<&str> = table["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    for method in TOOL_METHODS.iter().chain(&["editor.tool_poll"]) {
        assert!(names.contains(method), "{method} is in the table");
        let entry = agent.method_schema(method);
        assert_eq!(entry["name"], *method);
        assert!(
            !has_any_type(&entry["params"]),
            "{method} has no `any` type: {}",
            entry["params"]
        );
    }
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "no method is listed twice");
    assert!(
        agent
            .call("agent.schema", json!({ "method": "editor.tool_nope" }))
            .is_err()
    );
}

#[test]
fn the_view_item_schema_matches_the_parser() {
    let mut agent = Agent::new();
    let entry = agent.method_schema("editor.tool_set_view");
    let items = &entry["params"]["properties"]["items"];
    assert_eq!(items["type"], "array");
    assert_eq!(items["maxItems"], 256);
    assert_eq!(entry["params"]["required"], json!(["items"]));
    let alternatives = items["items"]["oneOf"]
        .as_array()
        .expect("one alternative per kind");
    let kinds: Vec<&str> = alternatives
        .iter()
        .filter_map(|alt| alt["properties"]["kind"]["const"].as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "heading",
            "label",
            "value",
            "progress",
            "button",
            "link",
            "separator"
        ]
    );

    // The documented example of each kind (book/agents/editor-tools.md) parses, round-trips, and
    // sets exactly the fields its alternative lists.
    let examples = [
        json!({ "kind": "heading", "text": "Review queue" }),
        json!({ "kind": "label", "text": "3 scenes waiting" }),
        json!({ "kind": "value", "label": "Daily cap", "value": "40" }),
        json!({ "kind": "progress", "label": "Upload", "fraction": 0.25 }),
        json!({ "kind": "button", "id": "approve", "label": "Approve" }),
        json!({ "kind": "link", "label": "Pick page", "url": "http://127.0.0.1:8765/" }),
        json!({ "kind": "separator" }),
    ];
    for (example, alternative) in examples.iter().zip(alternatives) {
        let parsed = ViewItem::parse(example).expect("the documented item parses");
        assert_eq!(&parsed.to_json(), example, "round trip");
        let properties = alternative["properties"].as_object().expect("properties");
        for key in example.as_object().expect("object").keys() {
            assert!(
                properties.contains_key(key),
                "{example}: `{key}` is in the schema"
            );
        }
        for required in alternative["required"].as_array().expect("required") {
            let key = required.as_str().expect("name");
            assert!(
                example.get(key).is_some(),
                "{example}: required `{key}` is present"
            );
        }
        // Dropping any one required field makes the parser refuse the item (kind aside).
        for required in alternative["required"].as_array().expect("required") {
            let key = required.as_str().expect("name");
            if key == "kind" {
                continue;
            }
            let mut broken = example.clone();
            broken.as_object_mut().expect("object").remove(key);
            assert!(ViewItem::parse(&broken).is_err(), "{example} without `{key}`");
        }
    }
}

#[test]
fn call_ids_are_integers_in_the_schema() {
    let mut agent = Agent::new();
    let reply = agent.method_schema("editor.tool_reply");
    assert_eq!(reply["params"]["properties"]["call"]["type"], "integer");
    assert_eq!(reply["params"]["required"], json!(["call"]));
    for method in ["editor.tool_call", "editor.tool_poll"] {
        let entry = agent.method_schema(method);
        assert_eq!(
            entry["params"]["properties"]["timeout_ms"]["type"], "integer",
            "{method}"
        );
    }
}

#[test]
fn tool_side_methods_called_by_an_agent_answer_tool_caller_required() {
    let mut agent = Agent::new();
    // The default permission profile reads; these are read-class methods, so the answer is the
    // tool-side check, not a permission refusal.
    for (method, params) in [
        ("editor.tool_set_view", json!({ "items": [] })),
        ("editor.tool_reply", json!({ "call": 1, "result": {} })),
        ("editor.tool_progress", json!({ "task": "t" })),
        ("editor.tool_poll", json!({})),
    ] {
        let error = agent.call(method, params).expect_err(method);
        let code = error
            .data
            .as_ref()
            .and_then(|data| data["code"].as_str())
            .unwrap_or_default();
        assert_eq!(code, "tool_caller_required", "{method}");
    }
}

/// `quit.request` in the edit world: answered `will_exit`, the close message is delivered to a
/// registered queue (an unregistered one is the ERROR the owner's run logged), and the editor's
/// quit flag is set for the event loop to act on.
#[test]
fn quit_request_reaches_the_editor_event_loop() {
    let mut agent = Agent::new();
    assert!(
        !agent
            .app
            .world()
            .resource::<regia_editor::quit::EditorQuit>()
            .is_requested()
    );
    let out = agent.call("quit.request", json!({})).expect("quit");
    assert_eq!(out["will_exit"], true);
    assert!(
        out["window"].is_u64(),
        "the primary window was asked to close: {out}"
    );
    assert!(
        agent
            .app
            .world()
            .contains_resource::<Messages<regia_platform::WindowCloseRequested>>(),
        "the close message is registered"
    );
    agent.app.update();
    let mut quit = agent
        .app
        .world_mut()
        .resource_mut::<regia_editor::quit::EditorQuit>();
    assert!(quit.take(), "the quit flag is set for main.rs");
}
