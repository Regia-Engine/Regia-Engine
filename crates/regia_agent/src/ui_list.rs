// SPDX-License-Identifier: MPL-2.0
//! `ui.list` and `ui.scroll_to`: read and drive virtualised lists and scroll views (book
//! `agents/ui-list.md`, ADR-273 lane 8, ADR-344).
//!
//! The target is the node id `ui.query` returns, as `entity` (or its alias `node`).
//!
//! Selection changes apply at once (the answer shows the new selection). Scrolling a list to an item
//! is queued for the next frame's layout pass; scrolling a plain scroll view to a position or node
//! applies at once.

use regia_ecs::prelude::*;
use regia_ui::list::{ListError, SelectOp, list_info, list_items, request_scroll_to_item, select_items};
use regia_ui::scroll::{ScrollVelocity, focus_shift};
use regia_ui::{ComputedNode, LayoutDirty, ListSelection, ScrollAlign, ScrollOffset, SelectionMode};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn list_error(e: ListError) -> RpcError {
    match e {
        ListError::NotAList => RpcError::new(
            NOT_FOUND,
            e.code(),
            "the entity is not a list",
            "find lists with ui.query { kind: \"ListView\" }",
        ),
        ListError::IndexOutOfRange => RpcError::new(
            INVALID_PARAMS,
            e.code(),
            "the index is past the last item",
            "read the item count with ui.list { entity }",
        ),
        ListError::NoSelection => RpcError::new(
            INVALID_PARAMS,
            e.code(),
            "the list does not allow selection",
            "turn it on with ui.list { entity, action: \"select\", mode: \"single\" | \"multi\" }",
        ),
    }
}

fn unknown_entity() -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "unknown_entity",
        "the entity does not exist",
        "take entity ids from a ui.query result",
    )
}

fn entity_param(params: &Value) -> Result<Entity, RpcError> {
    let bad = || RpcError::invalid_params("`entity` (or `node`) must be the node id from ui.query");
    let value = params
        .get("entity")
        .or_else(|| params.get("node"))
        .filter(|v| !v.is_null())
        .ok_or_else(|| RpcError::invalid_params("`entity` (the node id from ui.query) is required"))?;
    value.as_u64().and_then(Entity::try_from_bits).ok_or_else(bad)
}

fn usize_param(params: &Value, key: &str) -> Result<Option<usize>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn f32_param(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .map(|n| Some(n as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number"))),
    }
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a string"))),
    }
}

fn align_param(params: &Value) -> Result<ScrollAlign, RpcError> {
    match str_param(params, "align")? {
        None => Ok(ScrollAlign::Nearest),
        Some(name) => ScrollAlign::parse(name).ok_or_else(|| {
            RpcError::invalid_params("`align` must be \"nearest\", \"start\", \"center\" or \"end\"")
        }),
    }
}

fn indices_param(params: &Value) -> Result<Vec<usize>, RpcError> {
    let mut out = Vec::new();
    if let Some(index) = usize_param(params, "index")? {
        out.push(index);
    }
    match params.get("indices").filter(|v| !v.is_null()) {
        None => {}
        Some(Value::Array(items)) => {
            for item in items {
                let index = item
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| RpcError::invalid_params("`indices` must be non-negative integers"))?;
                out.push(index);
            }
        }
        Some(_) => return Err(RpcError::invalid_params("`indices` must be an array")),
    }
    Ok(out)
}

fn info_json(world: &World, entity: Entity) -> Result<Value, RpcError> {
    let info = list_info(world, entity).ok_or_else(|| list_error(ListError::NotAList))?;
    Ok(json!({
        "entity": entity.to_bits(),
        "count": info.count,
        "range": [info.range.start, info.range.end],
        "live_rows": info.live_rows,
        "spawned_rows": info.spawned_rows,
        "rebinds": info.rebinds,
        "total_height": info.total_height,
        "offset": info.offset,
        "layout": info.layout,
        "selection": {
            "mode": info.selection_mode,
            "selected": info.selected,
            "cursor": info.cursor,
        },
        "headers": info.headers,
    }))
}

/// `ui.list { entity, action?, ... }`.
pub(crate) fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    if world.get_entity(entity).is_err() {
        return Err(unknown_entity());
    }
    let action = str_param(params, "action")?.unwrap_or("info");
    match action {
        "info" => info_json(world, entity),
        "items" => {
            let start = usize_param(params, "start")?.unwrap_or(0);
            let limit = usize_param(params, "limit")?.unwrap_or(100);
            let rows =
                list_items(world, entity, start, limit).ok_or_else(|| list_error(ListError::NotAList))?;
            let rows: Vec<Value> = rows
                .into_iter()
                .map(|(index, label, selected)| json!({ "index": index, "label": label, "selected": selected }))
                .collect();
            Ok(json!({ "entity": entity.to_bits(), "start": start, "items": rows }))
        }
        "select" => {
            let mode = str_param(params, "mode")?
                .map(|name| {
                    SelectionMode::parse(name).ok_or_else(|| {
                        RpcError::invalid_params("`mode` must be \"none\", \"single\" or \"multi\"")
                    })
                })
                .transpose()?;
            let op = match str_param(params, "op")? {
                None => SelectOp::Set,
                Some(name) => SelectOp::parse(name).ok_or_else(|| {
                    RpcError::invalid_params(
                        "`op` must be \"set\", \"add\", \"toggle\", \"range\", \"all\" or \"clear\"",
                    )
                })?,
            };
            let indices = indices_param(params)?;
            // Only a pure mode change (no op, no indices) skips the selection call.
            let mode_only = params.get("op").is_none() && indices.is_empty() && params.get("mode").is_some();
            if !mode_only && matches!(op, SelectOp::Range) && indices.is_empty() {
                return Err(RpcError::invalid_params(
                    "`range` needs `indices` (first and last item)",
                ));
            }
            let previous_mode = world.get::<ListSelection>(entity).map(|selection| selection.mode);
            if let Some(mode) = mode {
                if world.get::<regia_ui::ListView>(entity).is_none() {
                    return Err(list_error(ListError::NotAList));
                }
                match world.get_mut::<ListSelection>(entity) {
                    Some(mut selection) => selection.mode = mode,
                    None => {
                        world.entity_mut(entity).insert(ListSelection::new(mode));
                    }
                }
            }
            if !mode_only && let Err(error) = select_items(world, entity, op, &indices) {
                if mode.is_some() {
                    match previous_mode {
                        Some(previous) => {
                            if let Some(mut selection) = world.get_mut::<ListSelection>(entity) {
                                selection.mode = previous;
                            }
                        }
                        None => {
                            world.entity_mut(entity).remove::<ListSelection>();
                        }
                    }
                }
                return Err(list_error(error));
            }
            info_json(world, entity)
        }
        "scroll_to" => {
            let index = usize_param(params, "index")?
                .ok_or_else(|| RpcError::invalid_params("`index` is required for scroll_to"))?;
            let align = align_param(params)?;
            request_scroll_to_item(world, entity, index, align).map_err(list_error)?;
            Ok(json!({ "entity": entity.to_bits(), "index": index, "queued": true }))
        }
        _ => Err(RpcError::invalid_params(
            "`action` must be \"info\", \"items\", \"select\" or \"scroll_to\"",
        )),
    }
}

/// `ui.scroll_to { entity, index? | reveal? | x?, y? | fraction?, align? }`.
pub(crate) fn scroll_to(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    if world.get_entity(entity).is_err() {
        return Err(unknown_entity());
    }
    if let Some(index) = usize_param(params, "index")? {
        let align = align_param(params)?;
        request_scroll_to_item(world, entity, index, align).map_err(list_error)?;
        return Ok(json!({ "entity": entity.to_bits(), "index": index, "queued": true }));
    }
    let Some(current) = world.get::<ScrollOffset>(entity).map(|o| o.0) else {
        return Err(RpcError::new(
            NOT_FOUND,
            "not_scrollable",
            "the entity has no scroll offset",
            "pass a ScrollView or ListView entity from ui.query",
        ));
    };
    let bounds = world
        .get::<ComputedNode>(entity)
        .map(|c| (c.rect, c.content_size));
    let max = bounds.map_or([f32::MAX; 2], |(rect, content)| {
        [(content[0] - rect[2]).max(0.0), (content[1] - rect[3]).max(0.0)]
    });
    let mut next = current;
    let mut given = false;
    if let Some(value) = params.get("reveal").filter(|v| !v.is_null()) {
        let node = value
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`reveal` must be a node id from ui.query"))?;
        let node = Entity::try_from_bits(node)
            .filter(|n| world.get_entity(*n).is_ok())
            .ok_or_else(unknown_entity)?;
        let (Some((view, _)), Some(target)) = (bounds, world.get::<ComputedNode>(node).map(|c| c.rect))
        else {
            return Err(RpcError::invalid_params(
                "both the view and the node need a computed layout",
            ));
        };
        let shift = focus_shift(view, target);
        next = [current[0] + shift[0], current[1] + shift[1]];
        given = true;
    }
    if let Some(x) = f32_param(params, "x")? {
        next[0] = x;
        given = true;
    }
    if let Some(y) = f32_param(params, "y")? {
        next[1] = y;
        given = true;
    }
    if let Some(fraction) = f32_param(params, "fraction")? {
        next[1] = fraction.clamp(0.0, 1.0) * max[1];
        given = true;
    }
    if !given {
        return Err(RpcError::invalid_params(
            "give `index` (list item), `reveal` (a node), `x` and/or `y`, or `fraction`",
        ));
    }
    next = [next[0].clamp(0.0, max[0]), next[1].clamp(0.0, max[1])];
    if let Some(mut offset) = world.get_mut::<ScrollOffset>(entity) {
        offset.0 = next;
    }
    if let Some(mut velocity) = world.get_mut::<ScrollVelocity>(entity) {
        velocity.v = [0.0; 2];
    }
    if let Some(mut dirty) = world.get_resource_mut::<LayoutDirty>() {
        dirty.mark();
    }
    Ok(json!({ "entity": entity.to_bits(), "offset": next, "max": [max[0].min(1.0e9), max[1].min(1.0e9)] }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::{ItemCount, ListItem, ListItems, ListView};

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<LayoutDirty>();
        world.init_resource::<regia_app::prelude::Messages<regia_ui::ListSelectionChanged>>();
        world
    }

    fn spawn_list(world: &mut World, count: usize) -> Entity {
        world.spawn((ListView::new(20.0, 2), ItemCount(count))).id()
    }

    #[test]
    fn info_reads_a_list() {
        let mut world = world();
        let l = spawn_list(&mut world, 7);
        let out = list(&mut world, &json!({ "entity": l.to_bits() })).expect("info");
        assert_eq!(out["entity"], json!(l.to_bits()));
        assert_eq!(out["selection"]["mode"], json!("none"));
        assert_eq!(out["layout"], json!("rows"));
        // No virtualise pass: count and height still follow the spawned `ItemCount`.
        assert_eq!(out["count"], json!(7));
        assert_eq!(out["total_height"].as_f64(), Some(140.0));
        assert_eq!(out["range"], json!([0, 0]));
    }

    #[test]
    fn items_lists_labels() {
        let mut world = world();
        let items = ListItems(vec![
            ListItem {
                label: "a".to_owned(),
                subtitle: String::new(),
            },
            ListItem {
                label: "b".to_owned(),
                subtitle: String::new(),
            },
            ListItem {
                label: "c".to_owned(),
                subtitle: String::new(),
            },
        ]);
        let l = world.spawn((ListView::new(20.0, 2), items)).id();
        let out = list(
            &mut world,
            &json!({ "entity": l.to_bits(), "action": "items", "start": 1, "limit": 5 }),
        )
        .expect("items");
        assert_eq!(out["items"][0]["label"], json!("b"));
        assert_eq!(out["items"][1]["index"], json!(2));
        assert_eq!(out["items"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn select_sets_mode_and_applies_ops() {
        let mut world = world();
        let l = spawn_list(&mut world, 10);
        // No selection model yet.
        let err = list(
            &mut world,
            &json!({ "entity": l.to_bits(), "action": "select", "indices": [1] }),
        )
        .expect_err("no selection");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("no_selection")
        );
        let out = list(
            &mut world,
            &json!({ "entity": l.to_bits(), "action": "select", "mode": "multi", "indices": [1, 2] }),
        )
        .expect("select");
        assert_eq!(out["selection"]["selected"], json!([1, 2]));
        assert_eq!(out["selection"]["mode"], json!("multi"));
        assert_eq!(out["count"], json!(10));
        let selected = out["selection"]["selected"].as_array().expect("selected");
        let count = out["count"].as_u64().expect("count");
        assert!(
            selected
                .iter()
                .all(|index| index.as_u64().expect("index") < count)
        );
        let out = list(
            &mut world,
            &json!({ "entity": l.to_bits(), "action": "select", "op": "toggle", "index": 2 }),
        )
        .expect("toggle");
        assert_eq!(out["selection"]["selected"], json!([1]));
        let out = list(
            &mut world,
            &json!({ "entity": l.to_bits(), "action": "select", "op": "all" }),
        )
        .expect("all");
        assert_eq!(out["selection"]["selected"].as_array().map(Vec::len), Some(10));
        let out = list(
            &mut world,
            &json!({ "entity": l.to_bits(), "action": "select", "op": "clear" }),
        )
        .expect("clear");
        assert_eq!(out["selection"]["selected"], json!([]));
        // A pure mode change keeps the selection.
        list(
            &mut world,
            &json!({ "entity": l.to_bits(), "action": "select", "indices": [4] }),
        )
        .expect("set");
        let out = list(
            &mut world,
            &json!({ "entity": l.to_bits(), "action": "select", "mode": "single" }),
        )
        .expect("mode");
        assert_eq!(out["selection"]["mode"], json!("single"));
        assert_eq!(out["selection"]["selected"], json!([4]));
    }

    #[test]
    fn rejected_selection_preserves_mode_component_and_selected_items() {
        for initial in [None, Some(SelectionMode::Single)] {
            for extra in [
                json!({ "mode": "multi", "index": 100 }),
                json!({ "mode": "none", "index": 0 }),
                json!({ "mode": "multi", "op": "nope" }),
                json!({ "mode": "single", "index": 999 }),
                json!({ "mode": "multi", "indices": [0, "bad"] }),
                json!({ "mode": "multi", "op": "range" }),
            ] {
                let mut world = world();
                let l = spawn_list(&mut world, 10);
                if let Some(mode) = initial {
                    world.entity_mut(l).insert(ListSelection::new(mode));
                    select_items(&mut world, l, SelectOp::Set, &[2]).expect("initial selection");
                }
                let before = info_json(&world, l).expect("before");
                let events = world
                    .resource::<regia_app::prelude::Messages<regia_ui::ListSelectionChanged>>()
                    .len();
                let mut params = extra;
                params["entity"] = json!(l.to_bits());
                params["action"] = json!("select");
                let err = list(&mut world, &params).expect_err("rejected selection");
                assert_eq!(err.code, INVALID_PARAMS, "{params}");
                assert_eq!(world.get::<ListSelection>(l).map(|s| s.mode), initial, "{params}");
                assert_eq!(info_json(&world, l).expect("after"), before, "{params}");
                assert_eq!(
                    world
                        .resource::<regia_app::prelude::Messages<regia_ui::ListSelectionChanged>>()
                        .len(),
                    events
                );
            }
        }
    }

    #[test]
    fn errors_are_stable() {
        let mut world = world();
        let l = spawn_list(&mut world, 3);
        let code = |r: Result<Value, RpcError>| {
            r.expect_err("error")
                .data
                .as_ref()
                .and_then(|d| d["code"].as_str().map(str::to_owned))
        };
        assert_eq!(code(list(&mut world, &json!({}))), Some("F0002".to_owned()));
        let other = world.spawn_empty().id();
        assert_eq!(
            code(list(&mut world, &json!({ "entity": other.to_bits() }))),
            Some("not_a_list".to_owned())
        );
        world.despawn(other);
        assert_eq!(
            code(list(&mut world, &json!({ "entity": other.to_bits() }))),
            Some("unknown_entity".to_owned())
        );
        assert_eq!(
            code(list(
                &mut world,
                &json!({ "entity": l.to_bits(), "action": "scroll_to", "index": 3 })
            )),
            Some("index_out_of_range".to_owned())
        );
        assert_eq!(
            code(list(
                &mut world,
                &json!({ "entity": l.to_bits(), "action": "fly" })
            )),
            Some("F0002".to_owned())
        );
        assert_eq!(
            code(list(
                &mut world,
                &json!({ "entity": l.to_bits(), "action": "scroll_to", "index": 1, "align": "middle" })
            )),
            Some("F0002".to_owned())
        );
    }

    #[test]
    fn list_scroll_to_is_queued() {
        let mut world = world();
        let l = spawn_list(&mut world, 100);
        let out = scroll_to(
            &mut world,
            &json!({ "entity": l.to_bits(), "index": 50, "align": "center" }),
        )
        .expect("queued");
        assert_eq!(out["queued"], json!(true));
        let queued = world.get::<regia_ui::ScrollToItem>(l).expect("request");
        assert_eq!((queued.index, queued.align), (50, ScrollAlign::Center));
    }

    fn view(world: &mut World) -> Entity {
        let rect = [0.0, 0.0, 100.0, 100.0];
        world
            .spawn((
                ScrollOffset::default(),
                ComputedNode {
                    rect,
                    clip_rect: rect,
                    content_size: [100.0, 1000.0],
                    scale: 1.0,
                    ..Default::default()
                },
            ))
            .id()
    }

    #[test]
    fn scroll_to_positions_clamp() {
        let mut world = world();
        let v = view(&mut world);
        let out = scroll_to(&mut world, &json!({ "entity": v.to_bits(), "y": 300 })).expect("y");
        assert_eq!(out["offset"], json!([0.0, 300.0]));
        let out = scroll_to(&mut world, &json!({ "entity": v.to_bits(), "y": 5000 })).expect("clamped");
        assert_eq!(out["offset"], json!([0.0, 900.0]));
        let out =
            scroll_to(&mut world, &json!({ "entity": v.to_bits(), "fraction": 0.5 })).expect("fraction");
        assert_eq!(out["offset"], json!([0.0, 450.0]));
        assert_eq!(world.get::<ScrollOffset>(v).map(|o| o.0), Some([0.0, 450.0]));
    }

    #[test]
    fn scroll_to_node_brings_it_into_view() {
        let mut world = world();
        let v = view(&mut world);
        let rect = [0.0, 180.0, 50.0, 40.0];
        let node = world
            .spawn(ComputedNode {
                rect,
                clip_rect: rect,
                scale: 1.0,
                ..Default::default()
            })
            .id();
        let out = scroll_to(
            &mut world,
            &json!({ "entity": v.to_bits(), "reveal": node.to_bits() }),
        )
        .expect("node");
        assert_eq!(out["offset"], json!([0.0, 120.0]));
    }

    #[test]
    fn scroll_to_validates() {
        let mut world = world();
        let v = view(&mut world);
        let plain = world.spawn_empty().id();
        let code = |r: Result<Value, RpcError>| {
            r.expect_err("error")
                .data
                .as_ref()
                .and_then(|d| d["code"].as_str().map(str::to_owned))
        };
        assert_eq!(
            code(scroll_to(
                &mut world,
                &json!({ "entity": plain.to_bits(), "y": 1 })
            )),
            Some("not_scrollable".to_owned())
        );
        assert_eq!(
            code(scroll_to(&mut world, &json!({ "entity": v.to_bits() }))),
            Some("F0002".to_owned())
        );
        assert_eq!(
            code(scroll_to(
                &mut world,
                &json!({ "entity": v.to_bits(), "y": "far" })
            )),
            Some("F0002".to_owned())
        );
    }
}
