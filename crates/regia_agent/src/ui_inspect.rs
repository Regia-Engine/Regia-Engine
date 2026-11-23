// SPDX-License-Identifier: MPL-2.0
//! Runtime UI inspection, event history, transient edits, picking and highlighting (ADR-2703,
//! ADR-7811, `agents/ui-inspector.md`). The editor's UI Inspector uses the same operations.
//!
//! `regia_ui::inspect` owns style attribution, bounded event recording and validated live edits.
//! This adapter resolves node parameters and returns the documented protocol errors.

use regia_ecs::prelude::*;
use regia_ui::debug_overlay::UiDebugOverlay;
use regia_ui::extract::{UiDamage, UiDamageSet};
use regia_ui::inspect as ui_inspect;
use regia_ui::node::{ComputedNode, NodeName};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// An error with a stable snake_case `data.reason`.
fn error(code: i64, reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let regia_code = if code == NOT_FOUND { "F0003" } else { "F0002" };
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": hint })),
    }
}

fn bad_param(message: impl Into<String>, hint: &str) -> RpcError {
    error(INVALID_PARAMS, "ui_bad_param", message, hint)
}

/// The node a call names: `node` (or `entity`) as entity bits, else `name` as an exact `NodeName`.
fn target(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("node")
        .or_else(|| params.get("entity"))
        .filter(|v| !v.is_null());
    let entity = if let Some(bits) = bits {
        bits.as_u64()
            .and_then(Entity::try_from_bits)
            .ok_or_else(|| bad_param("`node` must be a node id", "take ids from ui.query or ui.pick_at"))?
    } else if let Some(name) = params.get("name").filter(|v| !v.is_null()) {
        let name = name
            .as_str()
            .ok_or_else(|| bad_param("`name` must be a string", "the node's NodeName"))?;
        let mut q = world.query::<(Entity, &NodeName)>();
        let found: Vec<Entity> = q
            .iter(world)
            .filter(|(_, n)| n.0 == name)
            .map(|(e, _)| e)
            .collect();
        match found.as_slice() {
            [one] => *one,
            [] => {
                return Err(error(
                    NOT_FOUND,
                    "ui_node_not_found",
                    format!("no node is named `{name}`"),
                    "list names with ui.query",
                ));
            }
            many => {
                return Err(error(
                    INVALID_PARAMS,
                    "ui_node_ambiguous",
                    format!("{} nodes are named `{name}`", many.len()),
                    "pass the `node` id instead",
                ));
            }
        }
    } else {
        return Err(error(
            INVALID_PARAMS,
            "ui_node_required",
            "name the node: `node` (an id) or `name`",
            "call ui.query or ui.pick_at to find it",
        ));
    };
    if world.get_entity(entity).is_err() || !ui_inspect::is_ui_node(world, entity) {
        return Err(error(
            NOT_FOUND,
            "ui_node_not_found",
            "no UI node has that id",
            "take ids from ui.query or ui.pick_at; nodes despawn when a screen closes",
        ));
    }
    Ok(entity)
}

/// `ui.inspect { node | name }`.
pub(crate) fn inspect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = target(world, params)?;
    let mut out = ui_inspect::inspect(world, entity);
    if let Some(widget) = world.get::<regia_framework::loading_widget::PipelineLoadingWidget>(entity) {
        out["pipeline_loading"] = json!({
            "state": widget.state(),
            "notice": widget.shader_notice(),
            "notice_settings": widget.notice_settings,
            "hide_when_ready": widget.hide_when_ready,
            "show_progress": widget.show_progress,
            "label": widget.label.to_bits(), "bar": widget.bar.to_bits(),
            "notice_node": widget.notice.to_bits(),
        });
    }
    Ok(out)
}

/// `ui.inspect_events {node | name}`: bounded, non-draining event history.
pub(crate) fn inspect_events(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = target(world, params).map_err(live_error)?;
    Ok(ui_inspect::inspect_events(world, entity))
}

/// `ui.set_live_style {node | name, field, value}`: transient component edits only.
pub(crate) fn set_live_style(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = target(world, params).map_err(live_error)?;
    let field = params
        .get("field")
        .and_then(Value::as_str)
        .ok_or_else(|| live_error(bad_param("field is required", "text or style.<field>")))?;
    let value = params
        .get("value")
        .cloned()
        .ok_or_else(|| live_error(bad_param("value is required", "use the field's JSON value")))?;
    ui_inspect::set_live_style(world, entity, field, value)
        .map_err(|e| RpcError::new(INVALID_PARAMS, e.reason, e.message, e.hint))
}

// New live-inspection methods expose their stable lowercase reason as the protocol code too.
// The older inspector methods retain their established F0002/F0003 envelopes.
fn live_error(mut error: RpcError) -> RpcError {
    if let Some(data) = error.data.as_mut()
        && let Some(reason) = data.get("reason").cloned()
    {
        data["code"] = reason;
    }
    error
}

fn coordinate(params: &Value, key: &str) -> Result<f32, RpcError> {
    params
        .get(key)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map(|v| v as f32)
        .ok_or_else(|| {
            bad_param(
                format!("`{key}` must be a finite number"),
                "screen position in physical px",
            )
        })
}

/// `ui.pick_at { x, y, top_layer_only?, stack? }`.
pub(crate) fn pick_at(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let point = [coordinate(params, "x")?, coordinate(params, "y")?];
    let flag = |key: &str| match params.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad_param(
            format!("`{key}` must be true or false"),
            "pass a JSON boolean",
        )),
    };
    let top_layer_only = flag("top_layer_only")?;
    let with_stack = flag("stack")?;
    let stack = ui_inspect::pick_stack(world, point, top_layer_only);
    let world: &World = world;
    let entry = |e: Entity| {
        json!({
            "id": e.to_bits(),
            "name": world.get::<NodeName>(e).map(|n| n.0.clone()),
            "z": world.get::<ComputedNode>(e).map(|c| c.z),
        })
    };
    let top = stack.first().copied();
    Ok(json!({
        "x": point[0],
        "y": point[1],
        "node": top.map(Entity::to_bits),
        "path": top.map(|e| ui_inspect::node_path(world, e)),
        "count": stack.len(),
        "stack": with_stack.then(|| stack.iter().copied().map(entry).collect::<Vec<_>>()),
    }))
}

/// `ui.highlight { node | name | clear: true }`: outlines the node through the debug overlay.
pub(crate) fn highlight(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let clear = crate::params::opt_bool(params, "clear")? == Some(true);
    let entity = if clear { None } else { Some(target(world, params)?) };
    {
        let mut overlay = world.get_resource_or_insert_with(UiDebugOverlay::default);
        overlay.highlight = entity;
        overlay.enabled = entity.is_some() || overlay.enabled;
    }
    // The overlay draws only when the UI does: ask for a full rebuild.
    world.get_resource_or_insert_with(UiDamageSet::default).full = true;
    world.get_resource_or_insert_with(UiDamage::default).0 = true;
    let rect = entity.and_then(|e| {
        let c = world.get::<ComputedNode>(e)?;
        Some(json!({ "x": c.rect[0], "y": c.rect[1], "w": c.rect[2], "h": c.rect[3] }))
    });
    let enabled = world.get_resource::<UiDebugOverlay>().is_some_and(|o| o.enabled);
    Ok(json!({
        "highlight": entity.map(Entity::to_bits),
        "rect": rect,
        "overlay_enabled": enabled,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use regia_ui::node::UiNode;

    fn reason(e: &RpcError) -> &str {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    fn node(world: &mut World, z: f32, order: u32, rect: [f32; 4], name: &str) -> Entity {
        world
            .spawn((
                UiNode,
                NodeName(name.into()),
                ComputedNode {
                    rect,
                    clip_rect: [0.0, 0.0, 1000.0, 1000.0],
                    z,
                    paint_order: order,
                    scale: 1.0,
                    ..Default::default()
                },
            ))
            .id()
    }

    #[test]
    fn ui_inspect_reports_layout_and_the_name_path() {
        let mut w = World::new();
        let e = node(&mut w, 1.0, 0, [10.0, 20.0, 100.0, 40.0], "save");
        let out = inspect(&mut w, &json!({ "node": e.to_bits() })).unwrap();
        assert_eq!(out["layout"]["rect"]["w"], 100.0);
        assert_eq!(out["name"], "save");
        assert_eq!(out["path"][0]["id"], e.to_bits());
        let by_name = inspect(&mut w, &json!({ "name": "save" })).unwrap();
        assert_eq!(by_name["node"], e.to_bits());
    }

    #[test]
    fn ui_inspect_errors_use_the_documented_codes() {
        let mut w = World::new();
        let e = node(&mut w, 1.0, 0, [0.0; 4], "a");
        node(&mut w, 1.0, 1, [0.0; 4], "a");
        let gone = w.spawn_empty().id();
        w.despawn(gone);
        let missing = inspect(&mut w, &json!({ "node": gone.to_bits() })).unwrap_err();
        assert_eq!(reason(&missing), "ui_node_not_found");
        assert_eq!(missing.code, NOT_FOUND);
        let not_ui = w.spawn_empty().id();
        let err = inspect(&mut w, &json!({ "node": not_ui.to_bits() })).unwrap_err();
        assert_eq!(reason(&err), "ui_node_not_found");
        assert_eq!(
            reason(&inspect(&mut w, &json!({})).unwrap_err()),
            "ui_node_required"
        );
        assert_eq!(
            reason(&inspect(&mut w, &json!({ "name": "nope" })).unwrap_err()),
            "ui_node_not_found"
        );
        assert_eq!(
            reason(&inspect(&mut w, &json!({ "name": "a" })).unwrap_err()),
            "ui_node_ambiguous"
        );
        assert_eq!(
            reason(&inspect(&mut w, &json!({ "node": "x" })).unwrap_err()),
            "ui_bad_param"
        );
        assert!(inspect(&mut w, &json!({ "node": e.to_bits() })).is_ok());
    }

    #[test]
    fn ui_pick_at_returns_the_top_node_and_the_stack() {
        let mut w = World::new();
        let low = node(&mut w, 1.0, 0, [0.0, 0.0, 100.0, 100.0], "low");
        let high = node(&mut w, 2.0, 1, [50.0, 50.0, 100.0, 100.0], "high");
        let out = pick_at(&mut w, &json!({ "x": 60.0, "y": 60.0, "stack": true })).unwrap();
        assert_eq!(out["node"], high.to_bits());
        assert_eq!(out["count"], 2);
        assert_eq!(out["stack"][1]["id"], low.to_bits());
        let out = pick_at(&mut w, &json!({ "x": 10.0, "y": 10.0 })).unwrap();
        assert_eq!(out["node"], low.to_bits());
        assert!(out["stack"].is_null());
        let none = pick_at(&mut w, &json!({ "x": 900.0, "y": 900.0 })).unwrap();
        assert!(none["node"].is_null());
        assert_eq!(
            reason(&pick_at(&mut w, &json!({ "x": 1.0 })).unwrap_err()),
            "ui_bad_param"
        );
        assert_eq!(
            reason(&pick_at(&mut w, &json!({ "x": 1.0, "y": 1.0, "stack": 3 })).unwrap_err()),
            "ui_bad_param"
        );
    }

    #[test]
    fn ui_highlight_sets_and_clears_the_overlay_outline() {
        let mut w = World::new();
        let e = node(&mut w, 1.0, 0, [5.0, 6.0, 70.0, 80.0], "n");
        let out = highlight(&mut w, &json!({ "node": e.to_bits() })).unwrap();
        assert_eq!(out["highlight"], e.to_bits());
        assert_eq!(out["rect"]["h"], 80.0);
        assert_eq!(w.resource::<UiDebugOverlay>().highlight, Some(e));
        assert!(w.resource::<UiDebugOverlay>().enabled);
        let cleared = highlight(&mut w, &json!({ "clear": true })).unwrap();
        assert!(cleared["highlight"].is_null());
        assert_eq!(w.resource::<UiDebugOverlay>().highlight, None);
        assert_eq!(
            reason(&highlight(&mut w, &json!({ "node": 7 })).unwrap_err()),
            "ui_node_not_found"
        );
    }
}
