// SPDX-License-Identifier: MIT-0
//! The agent surface of the runtime UI (ADR-273 Lane 1, `book/agents/ui.md`): the gallery is
//! queried, clicked, focused and typed into through real protocol requests, headless and under a
//! fixed step. Requests go through the dispatcher in process (no socket, no clock), so every
//! assertion is a counted number of frames.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crossbeam_channel::{Receiver, bounded, unbounded};
use regia::agent::protocol::{PERMISSION_DENIED, Request, Response};
use regia::agent::server::Inbound;
use regia::agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, UiAgentHost};
use regia::prelude::*;
use regia_ui::agent_bridge::{UiAgentBridge, host_call};
use regia_ui::style::Align;
use regia_ui::{NodeName, ScaleMode};
use serde_json::{Value, json};
use ui_gallery::gallery::UiGallery;
use ui_gallery::shots::{headless_app, resize};

/// Every `UiEvent` the app emitted, collected after each frame.
#[derive(Resource, Default)]
struct Seen(Vec<UiEvent>);

fn collect(mut reader: MessageReader<UiEvent>, mut seen: ResMut<Seen>) {
    seen.0.extend(reader.read().cloned());
}

/// A headless gallery with the agent dispatcher (no server) and a "Settings" button on top.
fn app() -> (App, Entity) {
    let mut app = headless_app(UiGallery);
    app.add_plugins(AgentPlugin);
    app.insert_resource(UiAgentHost::new(host_call));
    app.init_resource::<Seen>();
    app.add_systems(Update, collect);
    app.update();
    let root = {
        let world = app.world_mut();
        world
            .query_filtered::<Entity, With<UiRoot>>()
            .iter(world)
            .next()
            .expect("the gallery root")
    };
    // The gallery has no Settings button of its own: add one, pinned to the top-left corner.
    let settings = {
        let world = app.world_mut();
        let settings = world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    position: PositionType::Absolute,
                    inset: Edges {
                        left: Dimension::Points(0.0),
                        top: Dimension::Points(0.0),
                        right: Dimension::Auto,
                        bottom: Dimension::Auto,
                    },
                    size: [Dimension::Points(140.0), Dimension::Points(40.0)],
                    justify_content: JustifyContent::Center,
                    align_items: Align::Center,
                    ..Default::default()
                }),
                Interactable::default(),
                Focusable::default(),
                Button,
                Classes(vec!["button".into()]),
                Themed::default(),
                NodeName("settings".into()),
            ))
            .id();
        let label = world.spawn((UiNode, Text::new("Settings"))).id();
        world.entity_mut(settings).add_child(label);
        world.entity_mut(root).add_child(settings);
        settings
    };
    resize(&mut app, 1280, 800, Some(ScaleMode::ConstantPixelSize));
    (app, settings)
}

/// Queues one request for the dispatcher and returns where its answer will arrive.
fn send(app: &mut App, method: &str, params: Value) -> Receiver<Response> {
    let (reply, answers) = bounded(1);
    let (inbox, requests) = unbounded();
    inbox
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply,
        })
        .expect("queued");
    app.insert_resource(AgentInbox(requests));
    answers
}

/// Runs at most `frames` frames until `answers` has a response.
fn await_answer(app: &mut App, answers: &Receiver<Response>, frames: usize) -> Option<Response> {
    for _ in 0..frames {
        app.update();
        if let Ok(response) = answers.try_recv() {
            return Some(response);
        }
    }
    None
}

/// Sends one request and runs up to `frames` frames until it is answered.
fn request(app: &mut App, method: &str, params: Value, frames: usize) -> Option<Response> {
    let answers = send(app, method, params);
    await_answer(app, &answers, frames)
}

fn call(app: &mut App, method: &str, params: Value) -> Value {
    let response = request(app, method, params, 2).expect("answered");
    match response.result {
        Some(result) => result,
        None => panic!("{method} failed: {:?}", response.error),
    }
}

fn frames(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

fn clicks_on(app: &App, target: Entity) -> usize {
    app.world()
        .resource::<Seen>()
        .0
        .iter()
        .filter(|e| e.target == target && e.kind == UiEventKind::Click)
        .count()
}

#[test]
fn query_by_text_finds_the_settings_button() {
    let (mut app, settings) = app();
    let found = call(&mut app, "ui.query", json!({ "text": "Settings" }));
    assert_eq!(found["count"], 1, "{found}");
    let node = &found["nodes"][0];
    assert_eq!(node["id"], settings.to_bits());
    assert_eq!(node["type"], "Button");
    assert_eq!(node["name"], "settings");
    assert_eq!(node["interactable"], true);
    let rect = node["rect"].as_array().expect("rect");
    assert!(
        rect[2].as_f64().unwrap() > 0.0 && rect[3].as_f64().unwrap() > 0.0,
        "laid out: {node}"
    );

    let by_name = call(
        &mut app,
        "ui.query",
        json!({ "name": "settings", "type": "button" }),
    );
    assert_eq!(by_name["count"], 1);
    let none = call(&mut app, "ui.query", json!({ "text": "No such label" }));
    assert_eq!(none["count"], 0);
    let state = call(&mut app, "ui.state", json!({}));
    assert_eq!(state["input_mode"], "pointer");
    assert_eq!(state["scale"]["window_size"], json!([1280.0, 800.0]));
    assert!(
        state["locale"]["available"]
            .as_array()
            .is_some_and(|l| !l.is_empty()),
        "{state}"
    );
}

#[test]
fn click_fires_one_click_event() {
    let (mut app, settings) = app();
    app.world_mut().resource_mut::<Seen>().0.clear();
    let queued = call(&mut app, "ui.click", json!({ "text": "Settings" }));
    assert_eq!(queued["node"], settings.to_bits());
    assert_eq!(queued["occluded"], false, "{queued}");
    assert_eq!(queued["queued_frames"], 2);
    frames(&mut app, 4);
    assert_eq!(clicks_on(&app, settings), 1, "exactly one Click");
    assert_eq!(app.world().resource::<UiAgentBridge>().pending_frames(), 0);

    // A double click is two clicks.
    app.world_mut().resource_mut::<Seen>().0.clear();
    call(
        &mut app,
        "ui.click",
        json!({ "node": settings.to_bits(), "count": 2 }),
    );
    frames(&mut app, 6);
    assert_eq!(clicks_on(&app, settings), 2);
}

#[test]
fn type_into_a_text_input_updates_its_content() {
    let (mut app, _) = app();
    // The theme sheet page carries its own (hidden) inputs: the widgets page shows exactly one.
    let input = call(&mut app, "ui.query", json!({ "type": "TextInput" }));
    let shown: Vec<&Value> = input["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter(|node| node["visible"] == true)
        .collect();
    assert_eq!(shown.len(), 1, "{input}");
    let id = shown[0]["id"].as_u64().expect("id");
    call(&mut app, "ui.type", json!({ "node": id, "text": "Ada" }));
    frames(&mut app, 3);
    let after = call(&mut app, "ui.query", json!({ "node": id }));
    assert_eq!(after["nodes"][0]["value"], "Ada", "{after}");
    assert!(
        after["nodes"][0]["states"]
            .as_array()
            .unwrap()
            .contains(&json!("focused"))
    );

    // Keys go to the focused input: Backspace removes the last character.
    call(&mut app, "ui.type", json!({ "keys": ["Backspace"] }));
    frames(&mut app, 3);
    let after = call(&mut app, "ui.query", json!({ "node": id }));
    assert_eq!(after["nodes"][0]["value"], "Ad", "{after}");

    call(&mut app, "ui.set_value", json!({ "node": id, "value": "Grace" }));
    frames(&mut app, 3);
    let after = call(&mut app, "ui.query", json!({ "node": id }));
    assert_eq!(after["nodes"][0]["value"], "Grace", "{after}");
}

#[test]
fn wait_until_focused_succeeds_after_ui_focus() {
    let (mut app, settings) = app();
    // Not focused yet: the wait stays pending.
    let condition = json!({ "predicate": "ui.node_state", "name": "settings", "state": "Focused" });
    let waiting = send(
        &mut app,
        "play.wait_until",
        json!({ "condition": condition, "timeout_ms": 120_000 }),
    );
    assert!(
        await_answer(&mut app, &waiting, 3).is_none(),
        "nothing has focus yet"
    );

    call(&mut app, "ui.focus", json!({ "name": "settings" }));
    let satisfied = await_answer(&mut app, &waiting, 4).expect("answered once focused");
    assert_eq!(satisfied.result.expect("result")["satisfied"], true);
    assert_eq!(
        app.world().resource::<regia_ui::FocusState>().focused,
        Some(settings)
    );

    let exists = request(
        &mut app,
        "play.wait_until",
        json!({ "condition": { "predicate": "ui.node_exists", "text": "Settings" } }),
        2,
    )
    .expect("exists");
    assert_eq!(exists.result.expect("result")["satisfied"], true);
}

#[test]
fn a_read_only_session_cannot_click() {
    let (mut app, settings) = app();
    app.insert_resource(AgentPermissions {
        config: AgentConfig {
            read_only: true,
            ..AgentConfig::default()
        },
        source: None,
    });
    for (method, params) in [
        ("ui.click", json!({ "name": "settings" })),
        ("ui.focus", json!({ "name": "settings" })),
        ("ui.type", json!({ "name": "settings", "text": "x" })),
        ("ui.set_value", json!({ "name": "settings", "value": 1 })),
    ] {
        let denied = request(&mut app, method, params, 2).expect("answered");
        let error = denied
            .error
            .unwrap_or_else(|| panic!("{method} should be denied"));
        assert_eq!(error.code, PERMISSION_DENIED, "{method}");
        let code = error.data.as_ref().and_then(|d| d["code"].as_str());
        assert_eq!(code, Some("F0012"), "{method}");
    }
    assert_eq!(
        app.world().resource::<UiAgentBridge>().pending_frames(),
        0,
        "nothing queued"
    );
    app.world_mut().resource_mut::<Seen>().0.clear();
    frames(&mut app, 4);
    assert_eq!(clicks_on(&app, settings), 0);
    // Reading stays allowed.
    let read = call(&mut app, "ui.query", json!({ "text": "Settings" }));
    assert_eq!(read["count"], 1);
}

#[test]
fn bad_targets_answer_with_documented_reasons() {
    let (mut app, _) = app();
    let reason = |response: Option<Response>| -> String {
        let error = response.expect("answered").error.expect("an error");
        error
            .data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(
        reason(request(&mut app, "ui.click", json!({ "text": "Nope" }), 2)),
        "ui_node_not_found"
    );
    assert_eq!(
        reason(request(&mut app, "ui.click", json!({}), 2)),
        "ui_node_required"
    );
    assert_eq!(
        reason(request(&mut app, "ui.query", json!({ "state": "glowing" }), 2)),
        "ui_unknown_state"
    );
    assert_eq!(
        reason(request(
            &mut app,
            "ui.set_value",
            json!({ "name": "settings", "value": 1 }),
            2
        )),
        "ui_node_not_settable"
    );
    assert_eq!(
        reason(request(&mut app, "ui.type", json!({ "text": "x" }), 2)),
        "ui_no_focus"
    );
}
