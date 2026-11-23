// SPDX-License-Identifier: MPL-2.0
//! `ui.a11y_tree`, `ui.a11y_announce` and `ui.a11y_act`: the accessibility tree a screen reader sees,
//! speech through the screen-reader hook, and the requests a reader makes of the UI (book
//! `agents/ui-a11y.md`, ADR-865, ADR-1621).
//!
//! The tree is built fresh from the widget tree on every call, whether or not a screen reader is
//! listening, so an agent can check names, roles and states the same way on every platform.

use regia_ecs::prelude::*;
use regia_ui::a11y::{
    A11yAction, A11yNode, A11yReject, A11yRequest, A11yTree, Politeness, ScreenReader, apply_request,
    build_a11y_tree,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

/// Longest announcement accepted (characters).
const MAX_ANNOUNCEMENT: usize = 1024;
/// Backend events returned at most.
const MAX_EVENTS: usize = 64;
/// Longest `set_text` value accepted (characters).
const MAX_TEXT_VALUE: usize = 4096;

fn invalid(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn node_json(node: &A11yNode) -> Value {
    json!({
        "id": node.id,
        "parent": node.parent,
        "children": node.children,
        "role": node.role().name(),
        "name": node.name,
        "description": node.description,
        "hint": node.hint,
        "value": node.value,
        "states": node.states.names(),
        "bounds": node.bounds,
        "live": node.live.name(),
        "summary": node.spoken_summary(),
    })
}

/// The ids of `id` and its descendants in `tree`, depth first.
fn subtree(tree: &A11yTree, id: u64) -> Vec<u64> {
    let mut out = vec![id];
    let mut i = 0;
    while i < out.len() {
        if let Some(node) = tree.get(out[i]) {
            out.extend(node.children.iter().copied());
        }
        i += 1;
    }
    out
}

/// `ui.a11y_tree { node? }`.
pub(crate) fn a11y_tree(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let node = match params
        .get("node")
        .or_else(|| params.get("entity"))
        .filter(|v| !v.is_null())
    {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|bits| Entity::try_from_bits(*bits).is_some())
                .ok_or_else(|| {
                    invalid(
                        "bad_entity",
                        "`node` must be a node id from ui.query or ui.a11y_tree",
                        "omit it for the whole tree",
                    )
                })?,
        ),
    };
    if let Some(id) = node
        && Entity::try_from_bits(id).is_none_or(|e| world.get_entity(e).is_err())
    {
        return Err(RpcError::new(
            NOT_FOUND,
            "unknown_entity",
            "the entity does not exist",
            "take node ids from ui.query or ui.a11y_tree",
        ));
    }
    let tree = build_a11y_tree(world);
    let nodes: Vec<Value> = match node {
        None => tree.nodes.iter().map(node_json).collect(),
        Some(id) => {
            if tree.get(id).is_none() {
                return Err(invalid(
                    "not_in_tree",
                    "the node is not in the accessibility tree (hidden, decorative, or a flattened container)",
                    "ask for the whole tree, or for an ancestor",
                ));
            }
            subtree(&tree, id)
                .into_iter()
                .filter_map(|n| tree.get(n).map(node_json))
                .collect()
        }
    };
    let ui_entities = world.query::<&regia_ui::ComputedNode>().iter(world).count();
    let (backend, live, builds, announcements, events, requests, sync_cost) =
        match world.get_resource::<ScreenReader>() {
            Some(reader) => {
                let events = reader.backend().recent_events();
                let skip = events.len().saturating_sub(MAX_EVENTS);
                let cost = reader.sync_cost();
                (
                    reader.backend().name(),
                    reader.is_live(),
                    reader.builds,
                    reader
                        .recent_announcements()
                        .map(|(text, p)| json!({ "text": text, "politeness": p.name() }))
                        .collect::<Vec<_>>(),
                    events.into_iter().skip(skip).collect::<Vec<_>>(),
                    reader
                        .recent_requests()
                        .map(|(request, result)| request_json(request, *result))
                        .collect::<Vec<_>>(),
                    json!({
                        "samples": cost.samples(),
                        "total": cost.total(),
                        "mean_ms": cost.mean_ms(),
                        "p99_ms": cost.percentile_ms(99.0),
                        "max_ms": cost.max_ms(),
                        "stages_mean_ms": regia_ui::a11y::SyncStage::ALL
                            .iter()
                            .map(|s| (s.name().to_owned(), json!(cost.stage_mean_ms(*s))))
                            .collect::<serde_json::Map<String, Value>>(),
                        "ui_entities": ui_entities,
                    }),
                )
            }
            None => ("none", false, 0, Vec::new(), Vec::new(), Vec::new(), Value::Null),
        };
    Ok(json!({
        "backend": backend,
        "live": live,
        "builds": builds,
        "focus": tree.focus,
        "top_layer": tree.top_layer,
        "count": nodes.len(),
        "nodes": nodes,
        "announcements": announcements,
        "backend_events": events,
        "requests": requests,
        "sync_cost": sync_cost,
    }))
}

fn request_json(request: &A11yRequest, result: Result<(), A11yReject>) -> Value {
    json!({
        "node": request.node,
        "action": request.action.name(),
        "accepted": result.is_ok(),
        "reason": result.err().map(A11yReject::reason),
    })
}

/// `ui.a11y_act { node, action, value? }`: what a screen reader's request does.
pub(crate) fn a11y_act(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let node = match params
        .get("node")
        .or_else(|| params.get("entity"))
        .filter(|v| !v.is_null())
    {
        None => {
            return Err(invalid(
                "a11y_node_missing",
                "`node` (the node id from ui.a11y_tree) is required",
                "ui.a11y_act { node: 123, action: \"press\" }",
            ));
        }
        Some(v) => v
            .as_u64()
            .filter(|bits| Entity::try_from_bits(*bits).is_some())
            .ok_or_else(|| {
                invalid(
                    "bad_entity",
                    "`node` must be a node id from ui.a11y_tree",
                    "take it from the `id` of a tree node",
                )
            })?,
    };
    let name = match params.get("action").filter(|v| !v.is_null()) {
        None => {
            return Err(invalid(
                "a11y_action_missing",
                "`action` is required",
                format!("one of: {}", A11yAction::NAMES.join(", ")),
            ));
        }
        Some(v) => v.as_str().ok_or_else(|| {
            invalid(
                "bad_action",
                "`action` must be a string",
                format!("one of: {}", A11yAction::NAMES.join(", ")),
            )
        })?,
    };
    if !A11yAction::NAMES.contains(&name) {
        return Err(invalid(
            "bad_action",
            format!("unknown action `{name}`"),
            format!("one of: {}", A11yAction::NAMES.join(", ")),
        ));
    }
    let value = match params.get("value").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_str().ok_or_else(|| {
            invalid(
                "bad_value",
                "`value` must be a string",
                "only set_text takes a value",
            )
        })?),
    };
    if name == "set_text" && value.is_none() {
        return Err(invalid(
            "a11y_value_missing",
            "`set_text` needs `value` (the new text)",
            "ui.a11y_act { node, action: \"set_text\", value: \"Ada\" }",
        ));
    }
    if value.is_some_and(|v| v.chars().count() > MAX_TEXT_VALUE) {
        return Err(invalid(
            "text_too_long",
            format!("`value` is longer than {MAX_TEXT_VALUE} characters"),
            "set a shorter text",
        ));
    }
    let Some(action) = A11yAction::parse(name, value) else {
        return Err(invalid(
            "bad_action",
            "the action did not parse",
            "see the book page",
        ));
    };
    if world.get_resource::<ScreenReader>().is_none() {
        return Err(RpcError::new(
            UNSUPPORTED,
            "ui_not_installed",
            "this app has no screen-reader hook",
            "the app needs regia_ui::UiPlugin",
        ));
    }
    let request = A11yRequest { node, action };
    let result = apply_request(world, &request);
    if let Some(mut reader) = world.get_resource_mut::<ScreenReader>() {
        reader.note_request(request.clone(), result);
    }
    if let Err(reject) = result {
        let (code, message, hint) = match reject {
            A11yReject::UnknownNode => (
                NOT_FOUND,
                "the node is not in the accessibility tree (gone, hidden, decorative, or a flattened container)",
                "take node ids from ui.a11y_tree",
            ),
            A11yReject::Disabled => (
                INVALID_PARAMS,
                "the node is disabled, so it does not act",
                "enable it first, or act on another node",
            ),
            A11yReject::Inert => (
                INVALID_PARAMS,
                "the node is under an open dialog and cannot be reached",
                "act inside the dialog, or close it (ui.a11y_tree lists `inert` states)",
            ),
            A11yReject::Unsupported => (
                INVALID_PARAMS,
                "the node cannot do that action",
                "press buttons, check boxes and combo boxes; raise or lower sliders; set_text on editable text fields",
            ),
            A11yReject::NotDismissable => (
                INVALID_PARAMS,
                "the dialog does not close on cancel",
                "use the dialog's own buttons",
            ),
            A11yReject::NoScrollView => (
                NOT_FOUND,
                "the node is not inside a scroll view",
                "find scroll_view nodes in ui.a11y_tree",
            ),
        };
        return Err(RpcError::new(code, reject.reason(), message, hint));
    }
    let tree = build_a11y_tree(world);
    Ok(json!({
        "node": node,
        "action": name,
        "accepted": true,
        "summary": tree.get(node).map(A11yNode::spoken_summary),
        "note": "widgets react in the next frame; read ui.a11y_tree (or wait_until) for the result",
    }))
}

/// `ui.a11y_announce { text, priority? }`.
pub(crate) fn a11y_announce(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = match params.get("text").filter(|v| !v.is_null()) {
        None => {
            return Err(invalid(
                "a11y_text_missing",
                "`text` (what to say) is required",
                "ui.a11y_announce { text: \"Checkpoint reached\" }",
            ));
        }
        Some(v) => v.as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| {
            invalid(
                "bad_text",
                "`text` must be a non-empty string",
                "pass what to say",
            )
        })?,
    };
    if text.chars().count() > MAX_ANNOUNCEMENT {
        return Err(invalid(
            "text_too_long",
            format!("`text` is longer than {MAX_ANNOUNCEMENT} characters"),
            "announce one short message at a time",
        ));
    }
    let politeness = match params.get("priority").filter(|v| !v.is_null()) {
        None => Politeness::Polite,
        Some(v) => match v.as_str().and_then(Politeness::parse) {
            Some(p) if p != Politeness::Off => p,
            _ => {
                return Err(invalid(
                    "bad_priority",
                    "`priority` must be \"polite\" or \"assertive\"",
                    "polite is the default",
                ));
            }
        },
    };
    let Some(mut reader) = world.get_resource_mut::<ScreenReader>() else {
        return Err(RpcError::new(
            UNSUPPORTED,
            "ui_not_installed",
            "this app has no screen-reader hook",
            "the app needs regia_ui::UiPlugin",
        ));
    };
    reader.announce(text, politeness);
    Ok(json!({
        "queued": true,
        "backend": reader.backend().name(),
        "live": reader.is_live(),
        "priority": politeness.name(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::a11y::Accessible;
    use regia_ui::{Button, Text, UiNode, UiRoot};

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_owned()
    }

    /// ADR-5379: the per-frame price of a connected screen reader is readable where it runs.
    #[test]
    fn the_tree_reports_what_live_syncs_cost() {
        let mut world = World::new();
        let out = a11y_tree(&mut world, &json!({})).expect("tree");
        assert_eq!(out["sync_cost"], Value::Null, "no ScreenReader, no cost");
        let root = world.spawn((UiRoot::default(), UiNode)).id();
        world.spawn((UiNode, Button, ChildOf(root)));
        let mut reader = ScreenReader::with_backend(Box::new(regia_ui::a11y::NullScreenReader));
        reader.set_exporting(true);
        world.insert_resource(reader);
        for _ in 0..3 {
            regia_ui::a11y::sync_screen_reader(&mut world);
        }
        let cost = &a11y_tree(&mut world, &json!({})).expect("tree")["sync_cost"];
        assert_eq!(
            (cost["samples"].clone(), cost["total"].clone()),
            (json!(3), json!(3))
        );
        for key in ["mean_ms", "p99_ms", "max_ms"] {
            assert!(cost[key].as_f64().is_some_and(|v| v >= 0.0), "{key}: {cost}");
        }
        let stages = cost["stages_mean_ms"].as_object().expect("stages");
        let mut names: Vec<&str> = stages.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["backend", "build", "diff"]);
        assert!(cost["ui_entities"].as_u64().is_some(), "{cost}");
    }

    #[test]
    fn reports_the_tree_and_a_subtree() {
        let mut world = World::new();
        let root = world.spawn((UiRoot::default(), UiNode)).id();
        let button = world.spawn((UiNode, Button, ChildOf(root))).id();
        world.spawn((UiNode, Text::new("Play"), ChildOf(button)));
        let star = world
            .spawn((UiNode, Text::new("*"), Accessible::decorative(), ChildOf(root)))
            .id();
        let out = a11y_tree(&mut world, &json!({})).expect("tree");
        assert_eq!(out["count"], json!(2));
        assert_eq!(out["nodes"][1]["role"], json!("button"));
        assert_eq!(out["nodes"][1]["name"], json!("Play"));
        assert_eq!(out["nodes"][1]["summary"], json!("Play, button"));
        assert_eq!(out["backend"], json!("none"));
        let sub = a11y_tree(&mut world, &json!({ "node": button.to_bits() })).expect("subtree");
        assert_eq!(sub["count"], json!(1));
        let err = a11y_tree(&mut world, &json!({ "node": star.to_bits() })).expect_err("hidden");
        assert_eq!(code(&err), "not_in_tree");
        crate::contract::assert_listed("ui.a11y_tree", "not_in_tree");
        let err = a11y_tree(&mut world, &json!({ "node": "x" })).expect_err("bad");
        assert_eq!(code(&err), "bad_entity");
        assert!(world.despawn(star));
        let err = a11y_tree(&mut world, &json!({ "node": star.to_bits() })).expect_err("gone");
        assert_eq!(code(&err), "unknown_entity");
    }

    #[test]
    fn announce_queues_through_the_hook_and_validates() {
        let mut world = World::new();
        let err = a11y_announce(&mut world, &json!({ "text": "hi" })).expect_err("no hook");
        assert_eq!(code(&err), "ui_not_installed");
        world.insert_resource(ScreenReader::with_backend(Box::new(
            regia_ui::a11y::NullScreenReader,
        )));
        let out = a11y_announce(
            &mut world,
            &json!({ "text": "Checkpoint", "priority": "assertive" }),
        )
        .expect("queued");
        assert_eq!(out["queued"], json!(true));
        assert_eq!(out["priority"], json!("assertive"));
        for (params, expected) in [
            (json!({}), "a11y_text_missing"),
            (json!({ "text": "  " }), "bad_text"),
            (json!({ "text": 3 }), "bad_text"),
            (json!({ "text": "x", "priority": "off" }), "bad_priority"),
            (json!({ "text": "x", "priority": 1 }), "bad_priority"),
            (
                json!({ "text": "x".repeat(MAX_ANNOUNCEMENT + 1) }),
                "text_too_long",
            ),
        ] {
            let err = a11y_announce(&mut world, &params).expect_err("invalid");
            assert_eq!(code(&err), expected, "{params}");
        }
    }
    #[test]
    fn act_validates_params_and_reports_why_a_request_is_refused() {
        let mut world = World::new();
        let root = world.spawn((UiRoot::default(), UiNode)).id();
        let button = world.spawn((UiNode, Button, ChildOf(root))).id();
        world.spawn((UiNode, Text::new("Play"), ChildOf(button)));
        let id = button.to_bits();
        let err = a11y_act(&mut world, &json!({ "node": id, "action": "press" })).expect_err("no hook");
        assert_eq!(code(&err), "ui_not_installed");
        world.insert_resource(ScreenReader::with_backend(Box::new(
            regia_ui::a11y::NullScreenReader,
        )));
        for (params, expected) in [
            (json!({ "action": "press" }), "a11y_node_missing"),
            (json!({ "node": "x", "action": "press" }), "bad_entity"),
            (json!({ "node": id }), "a11y_action_missing"),
            (json!({ "node": id, "action": 3 }), "bad_action"),
            (json!({ "node": id, "action": "tickle" }), "bad_action"),
            (json!({ "node": id, "action": "set_text" }), "a11y_value_missing"),
            (
                json!({ "node": id, "action": "set_text", "value": 3 }),
                "bad_value",
            ),
            (
                json!({ "node": id, "action": "set_text", "value": "x".repeat(MAX_TEXT_VALUE + 1) }),
                "text_too_long",
            ),
            (json!({ "node": id, "action": "raise" }), "unsupported_action"),
            (json!({ "node": id, "action": "page_down" }), "no_scroll_view"),
            (json!({ "node": id, "action": "close" }), "unsupported_action"),
            (
                json!({ "node": 4_294_968_295_u64, "action": "press" }),
                "unknown_node",
            ),
        ] {
            let err = a11y_act(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err), expected, "{params}");
        }
        world.entity_mut(button).insert(regia_ui::Disabled);
        let err = a11y_act(&mut world, &json!({ "node": id, "action": "press" })).expect_err("disabled");
        assert_eq!(code(&err), "node_disabled");
        // Every refusal was remembered for ui.a11y_tree.
        let tree = a11y_tree(&mut world, &json!({})).expect("tree");
        let requests = tree["requests"].as_array().expect("requests");
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|r| r["accepted"] == json!(false)));
        assert_eq!(
            requests.last().map(|r| r["reason"].clone()),
            Some(json!("node_disabled"))
        );
    }

    #[test]
    fn act_scrolls_a_view_and_returns_the_spoken_summary() {
        let mut world = World::new();
        world.insert_resource(ScreenReader::with_backend(Box::new(
            regia_ui::a11y::NullScreenReader,
        )));
        let root = world.spawn((UiRoot::default(), UiNode)).id();
        let view = world
            .spawn((
                UiNode,
                regia_ui::ScrollView::vertical(),
                Accessible::default()
                    .with_label("Log")
                    .with_hint("Swipe with three fingers"),
                regia_ui::ComputedNode {
                    rect: [0.0, 0.0, 100.0, 100.0],
                    content_size: [100.0, 1000.0],
                    ..regia_ui::ComputedNode::default()
                },
                ChildOf(root),
            ))
            .id();
        let out = a11y_act(
            &mut world,
            &json!({ "node": view.to_bits(), "action": "page_down" }),
        )
        .expect("paged");
        assert_eq!(out["accepted"], json!(true));
        assert_eq!(
            out["summary"],
            json!("Log, scroll area, Swipe with three fingers")
        );
        assert_eq!(
            world.get::<regia_ui::ScrollOffset>(view).map(|o| o.0),
            Some([0.0, 90.0])
        );
        let tree = a11y_tree(&mut world, &json!({})).expect("tree");
        assert_eq!(tree["nodes"][1]["hint"], json!("Swipe with three fingers"));
        assert_eq!(tree["requests"][0]["action"], json!("page_down"));
        assert_eq!(tree["requests"][0]["accepted"], json!(true));
    }
}
