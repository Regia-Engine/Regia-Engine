// SPDX-License-Identifier: MPL-2.0
//! `ui.fuzzy`, `ui.set_anchor_preset` and `ui.touch` (ADR-273 W17, Y01, I08; ADR-1842; books
//! `agents/ui-fuzzy.md`, `agents/ui-anchors.md`, `agents/ui-touch.md`).
//!
//! All three are thin shells over pure `regia_ui` code, so the agent can test them without a
//! window: `ui.fuzzy` ranks a list, `ui.set_anchor_preset` edits a node's style and marks the
//! layout dirty, `ui.touch` replays a recorded sample sequence through a fresh gesture recognizer.

use regia_ecs::prelude::*;
use regia_ui::anchors::{AnchorError, AnchorPreset};
use regia_ui::anim_timeline::{AnimError, UiAnimDef, UiAnimLibrary};
use regia_ui::color_picker::ColorPicker;
use regia_ui::debug_overlay::{BoxModel, UiDebugOverlay};
use regia_ui::extract::{UiDamage, UiDamageSet};
use regia_ui::fuzzy::fuzzy_rank;
use regia_ui::gesture::{Gesture, GestureConfig, GestureRecognizer, TouchSample, TouchStage};
use regia_ui::keyboard::{KeyboardMode, KeyboardPolicy};
use regia_ui::layout::LayoutDirty;
use regia_ui::node::ComputedNode;
use regia_ui::node::NodeStyle;
use regia_ui::scroll::ListItems;
use regia_ui::soft_cursor::{CursorStick, SoftCursor, step_cursor};
use regia_ui::tree::{TreeKey, TreeSpec, UiTree};
use regia_ui::tween::{TweenPlan, TweenValue, UiTween};
use regia_ui::ui_sounds::{UiSoundCue, UiSoundLog, UiSounds};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most items one `ui.fuzzy` call may rank.
const MAX_ITEMS: usize = 10_000;
/// Most samples one `ui.touch` call may replay.
const MAX_SAMPLES: usize = 4096;
/// Results `ui.fuzzy` returns when `limit` is omitted.
const DEFAULT_LIMIT: usize = 20;

fn error(class: i64, code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(class, code, message, hint)
}

/// `ui.fuzzy { query, items: [string], limit? }`.
pub(crate) fn fuzzy(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = params.get("query").and_then(Value::as_str).ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "bad_query",
            "`query` must be a string",
            "pass the text the player typed; an empty string matches every item",
        )
    })?;
    let items: Vec<&str> = crate::params::opt_array(params, "items")?
        .and_then(|a| a.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        .ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_items",
                "`items` must be an array of strings",
                "pass the candidate labels, for example command names",
            )
        })?;
    if items.len() > MAX_ITEMS {
        return Err(error(
            INVALID_PARAMS,
            "bad_items",
            format!("at most {MAX_ITEMS} items per call"),
            "rank a filtered subset",
        ));
    }
    let limit = match params.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= MAX_ITEMS as u64)
            .map(|n| n as usize)
            .ok_or_else(|| {
                error(
                    INVALID_PARAMS,
                    "bad_limit",
                    "`limit` must be a whole number from 0 to 10000",
                    "0 returns every match",
                )
            })?,
    };
    let ranked = fuzzy_rank(query, &items, limit);
    let matches: Vec<Value> = ranked
        .iter()
        .map(|m| {
            json!({
                "index": m.index,
                "item": items[m.index],
                "score": m.score,
                "positions": m.positions,
            })
        })
        .collect();
    Ok(json!({ "query": query, "count": matches.len(), "matches": matches }))
}

fn entity_of(world: &World, params: &Value) -> Result<Entity, RpcError> {
    entity_named(world, params, "entity")
}

fn entity_named(world: &World, params: &Value, field: &str) -> Result<Entity, RpcError> {
    let entity = crate::params::opt_u64(params, field)?
        .and_then(Entity::try_from_bits)
        .ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_entity",
                format!("`{field}` must be entity bits from ui.query"),
                "take entity ids from a ui.query result",
            )
        })?;
    if world.get_entity(entity).is_err() {
        return Err(error(
            NOT_FOUND,
            "unknown_entity",
            "the entity does not exist",
            "take entity ids from a ui.query result",
        ));
    }
    Ok(entity)
}

/// `ui.set_anchor_preset { entity, preset, offsets?: [l, t, r, b] }`.
pub(crate) fn set_anchor_preset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let preset = crate::params::opt_str(params, "preset")?
        .and_then(AnchorPreset::from_name)
        .ok_or_else(|| {
            let names: Vec<&str> = AnchorPreset::ALL.iter().map(|p| p.name()).collect();
            error(
                INVALID_PARAMS,
                "bad_preset",
                "`preset` is missing or unknown",
                format!("use one of: {}", names.join(", ")),
            )
        })?;
    let offsets = match params.get("offsets") {
        None | Some(Value::Null) => [0.0; 4],
        Some(v) => v
            .as_array()
            .filter(|a| a.len() == 4)
            .and_then(|a| a.iter().map(Value::as_f64).collect::<Option<Vec<_>>>())
            .map(|a| [a[0] as f32, a[1] as f32, a[2] as f32, a[3] as f32])
            .ok_or_else(|| {
                error(
                    INVALID_PARAMS,
                    "bad_offsets",
                    "`offsets` must be four numbers [left, top, right, bottom]",
                    "distances from the parent's edges in UI pixels",
                )
            })?,
    };
    let entity = entity_of(world, params)?;
    let Some(mut node) = world.get_mut::<NodeStyle>(entity) else {
        return Err(error(
            NOT_FOUND,
            "unknown_entity",
            "the entity is not a UI node with a style",
            "take entity ids from a ui.query result",
        ));
    };
    if let Err(AnchorError::SizeRequired) = preset.apply(&mut node.0, offsets) {
        return Err(error(
            INVALID_PARAMS,
            "size_required",
            "a centred axis needs a fixed width or height",
            "set the node's size in points or percent first",
        ));
    }
    let style = serde_json::to_value(node.0.inset).unwrap_or(Value::Null);
    let pivot = preset.pivot_ratio();
    world.get_resource_or_insert_with(LayoutDirty::default).mark();
    Ok(json!({
        "entity": entity.to_bits(),
        "preset": preset.name(),
        "inset": style,
        "pivot_ratio": pivot,
    }))
}

fn flag(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(error(
            INVALID_PARAMS,
            "bad_flag",
            format!("`{name}` must be true or false"),
            "pass a JSON boolean",
        )),
    }
}

fn rect_json(r: [f32; 4]) -> Value {
    json!({ "x": r[0], "y": r[1], "w": r[2], "h": r[3] })
}

/// `ui.debug_overlay { enabled?, outlines?, highlight?: entity | null }`.
pub(crate) fn debug_overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = flag(params, "enabled")?;
    let outlines = flag(params, "outlines")?;
    let focus_graph = flag(params, "focus_graph")?;
    let highlight = match params.get("highlight") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(_) => Some(Some(entity_named(world, params, "highlight")?)),
    };
    let changed = enabled.is_some() || outlines.is_some() || highlight.is_some() || focus_graph.is_some();
    if let Some(v) = focus_graph {
        world
            .get_resource_or_insert_with(regia_ui::debug_overlay::UiFocusGraphOverlay::default)
            .enabled = v;
    }
    if changed {
        let mut o = world.get_resource_or_insert_with(UiDebugOverlay::default);
        if let Some(v) = enabled {
            o.enabled = v;
        }
        if let Some(v) = outlines {
            o.outlines = v;
        }
        if let Some(v) = highlight {
            o.highlight = v;
        }
        // The overlay is redrawn only when the UI is: ask for a full rebuild.
        world.get_resource_or_insert_with(UiDamageSet::default).full = true;
        world.get_resource_or_insert_with(UiDamage::default).0 = true;
    }
    let o = world
        .get_resource::<UiDebugOverlay>()
        .cloned()
        .unwrap_or_default();
    let boxes = o.highlight.and_then(|e| {
        let node = world.get::<ComputedNode>(e)?;
        let style = world.get::<NodeStyle>(e)?;
        let scale = if node.scale > 0.0 { node.scale } else { 1.0 };
        let m = BoxModel::new(node.rect, scale, &style.0);
        Some(json!({
            "margin": rect_json(m.margin),
            "border": rect_json(m.border),
            "padding": rect_json(m.padding),
            "content": rect_json(m.content),
        }))
    });
    // The focus graph (ADR-2045): where each arrow press goes from the focused node.
    let graph_on = world
        .get_resource::<regia_ui::debug_overlay::UiFocusGraphOverlay>()
        .is_some_and(|f| f.enabled);
    let focus = if graph_on {
        let focused = world
            .get_resource::<regia_ui::focus::FocusState>()
            .and_then(|f| f.focused);
        let graph = regia_ui::focus_graph::world_graph(world);
        let edges: Vec<Value> = focused
            .and_then(|f| world.get::<ComputedNode>(f).map(|c| (f, c.rect)))
            .map(|(f, rect)| {
                regia_ui::debug_overlay::focus_edges(&graph, f, rect)
                    .into_iter()
                    .map(|(d, t)| {
                        json!({
                            "direction": regia_ui::focus_graph::direction_name(d),
                            "entity": t.to_bits(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "focused": focused.map(Entity::to_bits),
            "candidates": graph.nodes().len(),
            "edges": edges,
        })
    } else {
        Value::Null
    };
    Ok(json!({
        "enabled": o.enabled,
        "outlines": o.outlines,
        "highlight": o.highlight.map(Entity::to_bits),
        "boxes": boxes,
        "focus_graph": graph_on,
        "focus": focus,
    }))
}

fn anim_error(e: &AnimError) -> RpcError {
    error(
        INVALID_PARAMS,
        "bad_animation",
        e.to_string(),
        "tracks need known properties, ascending key times, fitting values and known ease names",
    )
}

fn anim_of(world: &World, params: &Value) -> Result<UiAnimDef, RpcError> {
    if let Some(v) = params.get("animation") {
        let def: UiAnimDef = serde_json::from_value(v.clone()).map_err(|e| {
            error(
                INVALID_PARAMS,
                "bad_animation",
                format!("`animation` is not a valid animation: {e}"),
                "see book/agents/ui-timeline.md for the shape",
            )
        })?;
        def.validate().map_err(|e| anim_error(&e))?;
        return Ok(def);
    }
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "bad_animation",
            "pass `name` of a defined animation or an inline `animation`",
            "list the defined animations with {action: list}",
        )
    })?;
    world
        .get_resource::<UiAnimLibrary>()
        .and_then(|l| l.animations.get(name))
        .cloned()
        .ok_or_else(|| {
            error(
                NOT_FOUND,
                "unknown_animation",
                format!("no animation named `{name}`"),
                "define it with {action: define, name, animation}",
            )
        })
}

fn reverse_and_speed(params: &Value) -> Result<(bool, Option<f32>), RpcError> {
    let reverse = flag(params, "reverse")?.unwrap_or(false);
    let speed = match params.get("speed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_f64().map(|n| n as f32).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_animation",
                "`speed` must be a number",
                "0.05 to 20",
            )
        })?),
    };
    Ok((reverse, speed))
}

/// `ui.play_anim { action: list | define | play | sample, ... }`.
pub(crate) fn play_anim(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = crate::params::opt_str(params, "action")?.unwrap_or("list");
    // Read for every action so a wrong type is refused even where `events` does not use it.
    let clear = crate::params::opt_bool(params, "clear")?.unwrap_or(false);
    match action {
        "list" => {
            let lib = world.get_resource::<UiAnimLibrary>();
            let items: Vec<Value> = lib
                .map(|l| {
                    l.animations
                        .iter()
                        .map(|(name, d)| {
                            json!({
                                "name": name,
                                "duration": d.duration(),
                                "tracks": d.tracks.len(),
                                "mode": serde_json::to_value(d.mode).unwrap_or(Value::Null),
                                "loops": d.loops,
                                "speed": d.speed,
                                "events": d.events.len(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({ "count": items.len(), "animations": items }))
        }
        "define" => {
            let name = crate::params::opt_str(params, "name")?
                .filter(|n| !n.is_empty())
                .ok_or_else(|| {
                    error(
                        INVALID_PARAMS,
                        "bad_animation",
                        "`name` is required to define an animation",
                        "pass a non-empty name",
                    )
                })?;
            let def = anim_of(
                world,
                &json!({ "animation": params.get("animation").cloned().unwrap_or(Value::Null) }),
            )?;
            let duration = def.duration();
            world
                .get_resource_or_insert_with(UiAnimLibrary::default)
                .animations
                .insert(name.to_owned(), def);
            Ok(json!({ "defined": name, "duration": duration }))
        }
        // The frame events the tween system sent (`.uianim.ron` v2, ADR-8900), oldest first.
        "events" => {
            let mut log = world.get_resource_or_insert_with(regia_ui::UiAnimEventLog::default);
            let events: Vec<Value> = log
                .recent
                .iter()
                .map(|e| {
                    json!({
                        "entity": e.entity.to_bits(),
                        "name": e.name,
                        "payload": e.payload,
                        "at": e.at,
                    })
                })
                .collect();
            let total = log.total;
            if clear {
                *log = regia_ui::UiAnimEventLog::default();
            }
            Ok(json!({ "total": total, "events": events, "cleared": clear }))
        }
        "play" | "sample" => {
            let def = anim_of(world, params)?;
            let (reverse, speed) = reverse_and_speed(params)?;
            let plan = def.compile(reverse, speed).map_err(|e| anim_error(&e))?;
            let events = def.compile_events(reverse, speed).map_err(|e| anim_error(&e))?;
            if action == "sample" {
                let t = crate::params::opt_f64(params, "time")?
                    .filter(|t| t.is_finite() && *t >= 0.0)
                    .ok_or_else(|| {
                        error(
                            INVALID_PARAMS,
                            "bad_time",
                            "`time` must be a non-negative number of seconds",
                            "sample at 0 for the start",
                        )
                    })? as f32;
                let mut out = Vec::new();
                plan.sample(t, &mut out);
                // The last entry for a property is the answer.
                let mut values = serde_json::Map::new();
                for (p, v) in out {
                    let v = match v {
                        TweenValue::Number(n) => json!(n),
                        TweenValue::Color(c) => json!(c.0),
                    };
                    values.insert(p.name().to_owned(), v);
                }
                // The frame events a playback from 0 has fired by `t`, in order: the same cursor
                // `advance_tweens` moves (elapsed clamped to the plan's length, and every event
                // once the tween is done), so sample returns exactly what playback sends.
                let duration = plan.duration();
                let mut cursor = UiTween::new(TweenPlan::Sequence(Vec::new())).with_events(events);
                let elapsed = t.min(duration);
                let range = cursor.take_crossed_events(elapsed, elapsed >= duration);
                let crossed: Vec<Value> = cursor.events[range]
                    .iter()
                    .map(|e| json!({ "name": e.name, "payload": e.payload, "at": e.at }))
                    .collect();
                return Ok(json!({
                    "time": t,
                    "duration": duration,
                    "values": values,
                    "events": crossed,
                }));
            }
            let entity = entity_of(world, params)?;
            let count = events.len();
            world
                .entity_mut(entity)
                .insert(UiTween::new(plan).with_events(events));
            let duration = world.get::<UiTween>(entity).map_or(0.0, UiTween::duration);
            Ok(json!({ "entity": entity.to_bits(), "playing": true, "duration": duration, "events": count }))
        }
        _ => Err(error(
            INVALID_PARAMS,
            "bad_action",
            "`action` is not one the method knows",
            "use list, define, play, sample or events",
        )),
    }
}

/// `ui.sounds { enabled?, max_per_frame?, sounds?: { cue: name | null }, clear_log? }`: reads or edits
/// the UI sound table and reports the table and the recent requests.
pub(crate) fn sounds(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = flag(params, "enabled")?;
    let clear = flag(params, "clear_log")?.unwrap_or(false);
    let max = match params.get("max_per_frame") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().filter(|n| (1..=64).contains(n)).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_max",
                "`max_per_frame` must be a whole number from 1 to 64",
                "4 is the default",
            )
        })? as usize),
    };
    let mut edits: Vec<(UiSoundCue, Option<String>)> = Vec::new();
    if let Some(v) = params.get("sounds").filter(|v| !v.is_null()) {
        let map = v.as_object().ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_sounds",
                "`sounds` must be an object of cue name to sound name or null",
                "cues: hover, press, focus, confirm, toggle, cancel",
            )
        })?;
        for (k, v) in map {
            let cue = UiSoundCue::parse(k).ok_or_else(|| {
                error(
                    INVALID_PARAMS,
                    "bad_sounds",
                    format!("`{k}` is not a UI sound cue"),
                    "cues: hover, press, focus, confirm, toggle, cancel",
                )
            })?;
            let name = match v {
                Value::Null => None,
                Value::String(s) if s.len() <= 256 => Some(s.clone()),
                _ => {
                    return Err(error(
                        INVALID_PARAMS,
                        "bad_sounds",
                        format!("the sound for `{k}` must be a string or null"),
                        "use an audio cue name, or null to silence the cue",
                    ));
                }
            };
            edits.push((cue, name));
        }
    }
    if enabled.is_some() || max.is_some() || !edits.is_empty() {
        let mut t = world.get_resource_or_insert_with(UiSounds::default);
        if let Some(v) = enabled {
            t.enabled = v;
        }
        if let Some(v) = max {
            t.max_per_frame = v;
        }
        for (cue, name) in edits {
            match name {
                Some(n) => {
                    t.sounds.insert(cue, n);
                }
                None => {
                    t.sounds.remove(&cue);
                }
            }
        }
    }
    if clear && let Some(mut log) = world.get_resource_mut::<UiSoundLog>() {
        log.recent.clear();
    }
    let t = world.get_resource::<UiSounds>().cloned().unwrap_or_default();
    let log = world.get_resource::<UiSoundLog>();
    let recent: Vec<Value> = log
        .map(|l| {
            l.recent
                .iter()
                .map(|r| json!({ "cue": r.cue.name(), "sound": r.sound, "entity": r.entity.to_bits() }))
                .collect()
        })
        .unwrap_or_default();
    let sounds: serde_json::Map<String, Value> = t
        .sounds
        .iter()
        .map(|(c, n)| (c.name().to_owned(), json!(n)))
        .collect();
    Ok(json!({
        "enabled": t.enabled,
        "max_per_frame": t.max_per_frame,
        "sounds": sounds,
        "recent": recent,
        "dropped": log.map_or(0, |l| l.dropped),
    }))
}

/// Most rows one `ui.tree` query returns.
const MAX_TREE_ROWS: usize = 500;

fn tree_node(params: &Value, tree: &UiTree) -> Result<usize, RpcError> {
    crate::params::opt_u64(params, "node")?
        .map(|n| n as usize)
        .filter(|n| *n < tree.len())
        .ok_or_else(|| {
            error(
                NOT_FOUND,
                "unknown_node",
                "`node` is missing or not a node id of the tree",
                "node ids are preorder indexes; list them with {action: query}",
            )
        })
}

fn tree_snapshot(tree: &UiTree, params: &Value) -> Result<Value, RpcError> {
    let offset = crate::params::opt_u64(params, "offset")?.unwrap_or(0) as usize;
    let limit = crate::params::opt_u64(params, "limit")?.map_or(100, |n| (n as usize).min(MAX_TREE_ROWS));
    let rows: Vec<Value> = tree
        .rows()
        .iter()
        .enumerate()
        .skip(offset)
        .take(limit)
        .map(|(i, r)| {
            json!({
                "row": i,
                "node": r.node,
                "label": tree.label(r.node),
                "depth": r.depth,
                "has_children": r.has_children,
                "expanded": r.expanded,
                "selected": tree.selected == Some(r.node),
            })
        })
        .collect();
    Ok(json!({
        "nodes": tree.len(),
        "visible": tree.rows().len(),
        "selected": tree.selected,
        "rows": rows,
    }))
}

/// `ui.tree { entity, action?: query | set | expand | collapse | toggle | expand_all | collapse_all | select | key, ... }`.
pub(crate) fn tree(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(world, params)?;
    let action = crate::params::opt_str(params, "action")?.unwrap_or("query");
    if action == "set" {
        let specs: Vec<TreeSpec> = params
            .get("tree")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .ok_or_else(|| {
                error(
                    INVALID_PARAMS,
                    "bad_tree",
                    "`tree` must be an array of { label, children? }",
                    "children nest the same shape",
                )
            })?;
        let built = UiTree::new(&specs)
            .map_err(|e| error(INVALID_PARAMS, "bad_tree", e.to_string(), "shrink the tree"))?;
        let snapshot = tree_snapshot(&built, params)?;
        let mut e = world.entity_mut(entity);
        e.insert(built);
        if e.get::<ListItems>().is_none() {
            e.insert(ListItems::default());
        }
        return Ok(snapshot);
    }
    let Some(mut t) = world.get_mut::<UiTree>(entity) else {
        return Err(error(
            NOT_FOUND,
            "no_tree",
            "the entity has no tree",
            "create one with {action: set, entity, tree}",
        ));
    };
    match action {
        "query" => {}
        "expand" | "collapse" | "toggle" | "select" => {
            let n = tree_node(params, &t)?;
            match action {
                "expand" => t.expand(n),
                "collapse" => t.collapse(n),
                "toggle" => t.toggle(n),
                _ => t.select(n),
            };
        }
        "expand_all" => t.expand_all(),
        "collapse_all" => t.collapse_all(),
        "key" => {
            let key = match crate::params::opt_str(params, "key")? {
                Some("up") => TreeKey::Up,
                Some("down") => TreeKey::Down,
                Some("left") => TreeKey::Left,
                Some("right") => TreeKey::Right,
                Some("home") => TreeKey::Home,
                Some("end") => TreeKey::End,
                _ => {
                    return Err(error(
                        INVALID_PARAMS,
                        "bad_key",
                        "`key` must be up, down, left, right, home or end",
                        "pass one of the six names",
                    ));
                }
            };
            t.key(key);
        }
        _ => {
            return Err(error(
                INVALID_PARAMS,
                "bad_action",
                "`action` is not one the method knows",
                "use query, set, expand, collapse, toggle, expand_all, collapse_all, select or key",
            ));
        }
    }
    tree_snapshot(&t, params)
}

fn picker_json(p: &ColorPicker) -> Value {
    let (sx, sy) = p.square_pos();
    json!({
        "hex": p.to_hex(),
        "hue": p.hue,
        "saturation": p.sat,
        "value": p.val,
        "alpha": p.alpha,
        "square": [sx, sy],
        "hue_pos": p.hue_pos(),
    })
}

fn fraction_pair(v: &Value) -> Option<(f32, f32)> {
    let a = v.as_array().filter(|a| a.len() == 2)?;
    Some((a[0].as_f64()? as f32, a[1].as_f64()? as f32))
}

/// `ui.color_picker { entity, hex?, square?: [x, y], hue?, alpha? }`: with only `entity` it reads (and
/// creates a white picker on the node when it has none).
pub(crate) fn color_picker(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(world, params)?;
    let bad = |field: &str, hint: &str| {
        error(
            INVALID_PARAMS,
            "bad_color",
            format!("`{field}` is invalid"),
            hint.to_owned(),
        )
    };
    let mut picker = world.get::<ColorPicker>(entity).copied().unwrap_or_default();
    if let Some(v) = params.get("hex").filter(|v| !v.is_null()) {
        let ok = v.as_str().is_some_and(|s| picker.set_hex(s));
        if !ok {
            return Err(bad("hex", "use #RRGGBB or #RRGGBBAA"));
        }
    }
    if let Some(v) = params.get("square").filter(|v| !v.is_null()) {
        let (x, y) = fraction_pair(v).ok_or_else(|| bad("square", "two fractions [x, y] from 0 to 1"))?;
        picker.pick_square(x, y);
    }
    for (name, apply) in [
        ("hue", ColorPicker::pick_hue as fn(&mut ColorPicker, f32)),
        ("alpha", ColorPicker::pick_alpha as fn(&mut ColorPicker, f32)),
    ] {
        if let Some(v) = params.get(name).filter(|v| !v.is_null()) {
            let t = v.as_f64().ok_or_else(|| bad(name, "a fraction from 0 to 1"))? as f32;
            apply(&mut picker, t);
        }
    }
    world.entity_mut(entity).insert(picker);
    Ok(json!({ "entity": entity.to_bits(), "color": picker_json(&picker) }))
}

fn num_field(params: &Value, name: &str, min: f64, max: f64) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite() && (min..=max).contains(n))
            .map(|n| Some(n as f32))
            .ok_or_else(|| {
                error(
                    INVALID_PARAMS,
                    "bad_cursor",
                    format!("`{name}` must be a number from {min} to {max}"),
                    "see the parameter list of ui.soft_cursor",
                )
            }),
    }
}

/// `ui.soft_cursor { enabled?, speed?, dead_zone?, stick?, position?, nudge?: { stick: [x, y], dt } }`.
pub(crate) fn soft_cursor(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = flag(params, "enabled")?;
    let speed = num_field(params, "speed", 1.0, 20_000.0)?;
    let dead_zone = num_field(params, "dead_zone", 0.0, 0.95)?;
    let stick = match crate::params::opt_str(params, "stick")? {
        None => None,
        Some("left") => Some(CursorStick::Left),
        Some("right") => Some(CursorStick::Right),
        Some(_) => {
            return Err(error(
                INVALID_PARAMS,
                "bad_cursor",
                "`stick` must be left or right",
                "the stick that moves the cursor",
            ));
        }
    };
    let position = match params.get("position").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(fraction_pair(v).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_cursor",
                "`position` must be [x, y] in physical px",
                "two numbers",
            )
        })?),
    };
    let nudge = match params.get("nudge").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let stick = v.get("stick").and_then(fraction_pair);
            let dt = crate::params::opt_f64(v, "dt")?.filter(|d| d.is_finite() && (0.0..=1.0).contains(d));
            match (stick, dt) {
                (Some(s), Some(d)) => Some((s, d as f32)),
                _ => {
                    return Err(error(
                        INVALID_PARAMS,
                        "bad_cursor",
                        "`nudge` must be { stick: [x, y], dt } with dt from 0 to 1 second",
                        "moves the cursor as a stick held for dt seconds would",
                    ));
                }
            }
        }
    };
    let window = world
        .get_resource::<regia_ui::UiScales>()
        .map_or([1920.0, 1080.0], |s| s.window_size);
    let mut c = world.get_resource_or_insert_with(SoftCursor::default);
    if let Some(v) = enabled {
        c.enabled = v;
        if !v {
            c.active = false;
        }
    }
    if let Some(v) = speed {
        c.speed = v;
    }
    if let Some(v) = dead_zone {
        c.dead_zone = v;
    }
    if let Some(v) = stick {
        c.stick = v;
    }
    if let Some((x, y)) = position {
        c.pos = [x, y];
        c.active = c.enabled;
    }
    if let Some((s, dt)) = nudge {
        c.pos = step_cursor(c.pos, [s.0, s.1], dt, c.speed, c.dead_zone, window);
        c.active = c.enabled;
    }
    let out = json!({
        "enabled": c.enabled,
        "active": c.active,
        "position": c.pos,
        "speed": c.speed,
        "dead_zone": c.dead_zone,
        "stick": match c.stick { CursorStick::Left => "left", CursorStick::Right => "right" },
    });
    if let Some(mut p) = world.get_resource_mut::<regia_ui::UiPointer>()
        && enabled.is_none()
        && (position.is_some() || nudge.is_some())
    {
        let pos = out["position"].clone();
        if let Ok(pos) = serde_json::from_value::<[f32; 2]>(pos) {
            p.cursor = Some(pos);
        }
    }
    Ok(out)
}

/// `ui.keyboard { mode?: auto | off | on }`.
pub(crate) fn keyboard(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = match params.get("mode") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().and_then(KeyboardMode::parse).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_mode",
                "`mode` must be auto, off or on",
                "auto follows text focus",
            )
        })?),
    };
    let mut policy = world.get_resource_or_insert_with(KeyboardPolicy::default);
    if let Some(m) = mode {
        policy.mode = m;
    }
    let out = json!({ "mode": policy.mode.name(), "wanted": policy.wanted });
    let ime = world
        .query_filtered::<&regia_platform::Window, With<regia_platform::PrimaryWindow>>()
        .iter(world)
        .next()
        .map(|w| w.ime_enabled);
    Ok(json!({ "mode": out["mode"], "wanted": out["wanted"], "window_ime": ime }))
}

fn stage_of(v: &Value) -> Option<TouchStage> {
    match v.as_str()? {
        "down" => Some(TouchStage::Down),
        "move" => Some(TouchStage::Move),
        "up" => Some(TouchStage::Up),
        "cancel" => Some(TouchStage::Cancel),
        _ => None,
    }
}

fn sample_of(v: &Value) -> Option<TouchSample> {
    let pos = v.get("pos")?.as_array()?;
    if pos.len() != 2 {
        return None;
    }
    Some(TouchSample {
        id: v.get("id")?.as_u64()?,
        stage: stage_of(v.get("stage")?)?,
        pos: [pos[0].as_f64()? as f32, pos[1].as_f64()? as f32],
        time: v.get("time")?.as_f64()? as f32,
    })
}

fn gesture_json(g: &Gesture) -> Value {
    match g {
        Gesture::Pinch { scale, center } => json!({ "kind": "pinch", "scale": scale, "center": center }),
        Gesture::TwoFingerScroll { delta } => json!({ "kind": "two_finger_scroll", "delta": delta }),
        Gesture::Swipe {
            dir,
            distance,
            duration,
        } => json!({ "kind": "swipe", "direction": dir.name(), "distance": distance, "duration": duration }),
    }
}

fn live_json(world: &World) -> Value {
    let live = world.get_resource::<regia_ui::gesture::UiGestureInput>();
    json!({
        "enabled": live.is_some_and(|l| l.enabled),
        "count": live.map_or(0, |l| l.count),
        "active_touches": live.map_or(0, |l| l.recognizer.active_touches()),
        "recent": live.map(|l| l.recent.iter().map(gesture_json).collect::<Vec<_>>()).unwrap_or_default(),
    })
}

/// `ui.touch_live { enabled? }` (ADR-2045): switches and reports the live gesture input.
pub(crate) fn touch_live(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(v) = flag(params, "enabled")? {
        world
            .get_resource_or_insert_with(regia_ui::gesture::UiGestureInput::default)
            .enabled = v;
    }
    Ok(live_json(world))
}

/// `ui.touch { samples: [{ id, stage, pos: [x, y], time }], config? }`.
pub(crate) fn touch(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bad = |what: &str| {
        error(
            INVALID_PARAMS,
            "bad_samples",
            format!("`samples` is missing or malformed: {what}"),
            "each sample is { id, stage: down | move | up | cancel, pos: [x, y], time } in UI pixels and seconds",
        )
    };
    let list = params
        .get("samples")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("expected an array"))?;
    if list.len() > MAX_SAMPLES {
        return Err(bad("too many samples"));
    }
    let mut config = GestureConfig::default();
    if let Some(c) = crate::params::opt_object(params, "config")? {
        for (key, slot) in [
            ("pinch_threshold", &mut config.pinch_threshold),
            ("scroll_threshold", &mut config.scroll_threshold),
            ("swipe_distance", &mut config.swipe_distance),
            ("swipe_time", &mut config.swipe_time),
        ] {
            if let Some(v) = c.get(key) {
                let n = v.as_f64().filter(|n| n.is_finite() && *n >= 0.0).ok_or_else(|| {
                    error(
                        INVALID_PARAMS,
                        "bad_config",
                        format!("`config.{key}` must be a non-negative number"),
                        "omit it for the default",
                    )
                })?;
                *slot = n as f32;
            }
        }
    }
    let mut recognizer = GestureRecognizer::new(config);
    let mut gestures = Vec::new();
    for (i, s) in list.iter().enumerate() {
        let sample = sample_of(s).ok_or_else(|| bad(&format!("sample {i}")))?;
        for g in recognizer.feed(sample) {
            gestures.push(gesture_json(&g));
        }
    }
    Ok(json!({
        "gestures": gestures,
        "count": gestures.len(),
        "active_touches": recognizer.active_touches(),
    }))
}

/// `ui.set_disabled`: local and inherited input policies share the runtime API.
pub(crate) fn set_disabled(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(world, params)?;
    if !regia_ui::inspect::is_ui_node(world, entity) {
        return Err(error(
            NOT_FOUND,
            "unknown_entity",
            "the entity is not a UI node",
            "use ui.query",
        ));
    }
    let disabled = crate::params::opt_bool(params, "disabled")?.ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "bad_flag",
            "disabled must be a boolean",
            "pass disabled: true or false",
        )
    })?;
    let recursive = crate::params::opt_bool(params, "recursive")?.unwrap_or(false);
    regia_ui::node::set_disabled(world, entity, disabled, recursive);
    Ok(
        json!({ "entity": entity.to_bits(), "disabled": regia_ui::node::is_disabled(world, entity),
        "local": world.get::<regia_ui::Disabled>(entity).is_some(),
        "recursive": world.get::<regia_ui::node::DisabledSubtree>(entity).is_some() }),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use regia_ui::style::{Dimension, Style};

    fn code(e: &RpcError) -> &str {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    #[test]
    fn fuzzy_ranks_and_validates() {
        let mut w = World::new();
        let out = fuzzy(&mut w, &json!({ "query": "fb", "items": ["fabric", "foo_bar"] })).unwrap();
        assert_eq!(out["matches"][0]["item"], "foo_bar");
        assert_eq!(out["matches"][0]["positions"], json!([0, 4]));
        let err = fuzzy(&mut w, &json!({ "items": [] })).unwrap_err();
        assert_eq!(code(&err), "bad_query");
        let err = fuzzy(&mut w, &json!({ "query": "", "items": [1] })).unwrap_err();
        assert_eq!(code(&err), "bad_items");
        let out = fuzzy(
            &mut w,
            &json!({ "query": "", "items": ["a", "b", "c"], "limit": 2 }),
        )
        .unwrap();
        assert_eq!(out["count"], 2);
    }

    #[test]
    fn anchor_preset_edits_the_style_and_marks_layout() {
        let mut w = World::new();
        let e = w
            .spawn(NodeStyle(Style {
                size: [Dimension::Points(100.0), Dimension::Points(40.0)],
                ..Style::default()
            }))
            .id();
        w.insert_resource(LayoutDirty(false));
        let out = set_anchor_preset(
            &mut w,
            &json!({ "entity": e.to_bits(), "preset": "bottom_right", "offsets": [0, 0, 16, 16] }),
        )
        .unwrap();
        assert_eq!(out["pivot_ratio"], json!([1.0, 1.0]));
        let style = &w.get::<NodeStyle>(e).unwrap().0;
        assert_eq!(style.inset.right, Dimension::Points(16.0));
        assert!(w.resource::<LayoutDirty>().0);
        let err = set_anchor_preset(&mut w, &json!({ "entity": e.to_bits(), "preset": "x" })).unwrap_err();
        assert_eq!(code(&err), "bad_preset");
        let auto = w.spawn(NodeStyle(Style::default())).id();
        let err =
            set_anchor_preset(&mut w, &json!({ "entity": auto.to_bits(), "preset": "center" })).unwrap_err();
        assert_eq!(code(&err), "size_required");
        let err =
            set_anchor_preset(&mut w, &json!({ "entity": 1u64 << 40, "preset": "center" })).unwrap_err();
        assert!(code(&err) == "bad_entity" || code(&err) == "unknown_entity");
    }

    /// `.uianim.ron` v2 frame events on the agent surface (ADR-8900): `sample` lists the events a
    /// playback has crossed by `time`, `play` hands them to the tween, `events` reads and clears the
    /// fired log.
    #[test]
    fn play_anim_reports_frame_events() {
        let mut w = World::new();
        let def = json!({
            "tracks": [{ "property": "opacity", "keys": [{ "time": 0.0, "value": 0.0 }, { "time": 2.0, "value": 1.0 }] }],
            "events": [{ "time": 1.5, "name": "late" }, { "time": 0.5, "name": "early", "payload": "x" }],
        });
        play_anim(
            &mut w,
            &json!({ "action": "define", "name": "ev", "animation": def }),
        )
        .unwrap();
        let list = play_anim(&mut w, &json!({})).unwrap();
        assert_eq!(list["animations"][0]["events"], 2);
        let s = play_anim(&mut w, &json!({ "action": "sample", "name": "ev", "time": 1.0 })).unwrap();
        assert_eq!(
            s["events"],
            json!([{ "name": "early", "payload": "x", "at": 0.5 }])
        );
        let s = play_anim(
            &mut w,
            &json!({ "action": "sample", "name": "ev", "time": 1.0, "reverse": true }),
        )
        .unwrap();
        assert_eq!(s["events"][0]["name"], "late", "reverse meets `late` at 0.5 s");
        let e = w.spawn_empty().id();
        let p = play_anim(
            &mut w,
            &json!({ "action": "play", "name": "ev", "entity": e.to_bits() }),
        )
        .unwrap();
        assert_eq!(p["events"], 2);
        assert_eq!(w.get::<UiTween>(e).unwrap().events.len(), 2);
        w.get_resource_or_insert_with(regia_ui::UiAnimEventLog::default)
            .push(regia_ui::UiAnimEventFired {
                entity: e,
                name: "early".into(),
                payload: "x".into(),
                at: 0.5,
            });
        let log = play_anim(&mut w, &json!({ "action": "events", "clear": true })).unwrap();
        assert_eq!(log["total"], 1);
        assert_eq!(log["events"][0]["name"], "early");
        let log = play_anim(&mut w, &json!({ "action": "events" })).unwrap();
        assert_eq!(log["total"], 0);
        let bad = json!({ "tracks": [{ "property": "opacity", "keys": [{ "time": 0.0, "value": 0.0 }] }],
            "events": [{ "time": 3.0, "name": "past" }] });
        let err = play_anim(
            &mut w,
            &json!({ "action": "define", "name": "b", "animation": bad }),
        )
        .unwrap_err();
        assert_eq!(code(&err), "bad_animation");
    }

    /// A headless UI app whose game clock moves `step_ms` per update.
    fn playback_app(step_ms: u64) -> regia_app::App {
        use regia_app::prelude::*;
        let mut app = App::new();
        app.add_plugins(regia_ui::UiPlugin::code_only());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(step_ms),
        ));
        app
    }

    fn event_rows(v: &Value) -> Vec<(String, f32)> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_owned(),
                    e["at"].as_f64().unwrap() as f32,
                )
            })
            .collect()
    }

    /// `sample` lists exactly the frame events a real playback (`advance_tweens`) has sent by the
    /// same elapsed time, at every update of looped animations whose pass boundaries are not
    /// exact in f32 (the review's 0.1 s x 8 and 0.3 s x 7 loops, a ping-pong, reverse, speed 3, a
    /// 70 ms step that lands between events). At and past the plan's duration it lists every
    /// event, each pass end once (ADR-10132).
    #[test]
    fn play_anim_sample_matches_what_playback_fires() {
        use regia_app::prelude::*;
        let cases = [
            (0.1f32, "loop", 8u32, 1.0f32, false, 100u64),
            (0.3, "loop", 7, 1.0, false, 100),
            (0.1, "ping_pong", 9, 1.0, false, 70),
            (0.3, "loop", 5, 3.0, true, 70),
            (0.7, "ping_pong", 6, 1.7, true, 100),
        ];
        for (len, mode, loops, speed, reverse, step) in cases {
            let label = format!("{len} s {mode} x{loops} speed {speed} reverse {reverse} step {step} ms");
            let mut app = playback_app(step);
            let def = json!({
                "tracks": [
                    { "property": "opacity", "keys": [{ "time": 0.0, "value": 1.0 }, { "time": len, "value": 0.0 }] },
                    { "property": "scale", "keys": [{ "time": len / 3.0, "value": 1.0 }, { "time": len, "value": 2.0 }] },
                ],
                "mode": mode,
                "loops": loops,
                "speed": speed,
                "events": [
                    { "time": 0.0, "name": "start" },
                    { "time": len / 3.0, "name": "third" },
                    { "time": len, "name": "end" },
                ],
            });
            let w = app.world_mut();
            play_anim(w, &json!({ "action": "define", "name": "a", "animation": def })).unwrap();
            let e = w.spawn(regia_ui::Opacity(1.0)).id();
            let p = play_anim(
                w,
                &json!({ "action": "play", "name": "a", "entity": e.to_bits(), "reverse": reverse, "speed": speed }),
            )
            .unwrap();
            let total = p["duration"].as_f64().unwrap() as f32;
            assert_eq!(p["events"], 3 * loops, "{label}");
            let sample = |w: &mut World, t: f32| {
                let s = play_anim(
                    w,
                    &json!({ "action": "sample", "name": "a", "time": t, "reverse": reverse, "speed": speed }),
                )
                .unwrap();
                assert_eq!(s["duration"].as_f64().unwrap() as f32, total, "{label}");
                event_rows(&s["events"])
            };
            let mut fired: Vec<(String, f32)> = Vec::new();
            let mut updates = 0;
            loop {
                app.update();
                updates += 1;
                fired.extend(
                    app.world()
                        .resource::<Messages<regia_ui::UiAnimEventFired>>()
                        .iter_current_update_messages()
                        .map(|m| (m.name.clone(), m.at)),
                );
                let elapsed = app.world().get::<UiTween>(e).map_or(total, |t| t.elapsed);
                assert_eq!(
                    sample(app.world_mut(), elapsed),
                    fired,
                    "{label}: sample at elapsed {elapsed} (update {updates})"
                );
                if app.world().get::<UiTween>(e).is_none() {
                    break;
                }
                assert!(updates < 10_000, "{label}: the tween never ended");
            }
            assert_eq!(
                fired.len(),
                3 * loops as usize,
                "{label}: every event once per pass"
            );
            assert!(
                fired.windows(2).all(|w| w[0].1 <= w[1].1),
                "{label}: in time order"
            );
            let ends = fired.iter().filter(|f| f.0 == "end").count();
            assert_eq!(ends, loops as usize, "{label}: each pass end once");
            let w = app.world_mut();
            assert_eq!(sample(w, total), fired, "{label}: sample at the duration");
            assert_eq!(sample(w, total + 5.0), fired, "{label}: sample past the end");
        }
    }

    #[test]
    fn play_anim_defines_samples_and_plays() {
        let mut w = World::new();
        let def = json!({ "tracks": [{ "property": "opacity", "keys": [
            { "time": 0.0, "value": 0.0 }, { "time": 2.0, "value": 1.0, "ease": "linear" }] }] });
        play_anim(
            &mut w,
            &json!({ "action": "define", "name": "fade", "animation": def }),
        )
        .unwrap();
        let list = play_anim(&mut w, &json!({})).unwrap();
        assert_eq!(list["animations"][0]["name"], "fade");
        let s = play_anim(
            &mut w,
            &json!({ "action": "sample", "name": "fade", "time": 1.0 }),
        )
        .unwrap();
        assert_eq!(s["values"]["opacity"], 0.5);
        let s = play_anim(
            &mut w,
            &json!({ "action": "sample", "name": "fade", "time": 0.0, "reverse": true }),
        )
        .unwrap();
        assert_eq!(s["values"]["opacity"], 1.0);
        let e = w.spawn_empty().id();
        let p = play_anim(
            &mut w,
            &json!({ "action": "play", "name": "fade", "entity": e.to_bits(), "speed": 2.0 }),
        )
        .unwrap();
        assert_eq!(p["duration"], 1.0);
        assert!(w.get::<UiTween>(e).is_some());
        let err = play_anim(&mut w, &json!({ "action": "sample", "name": "nope", "time": 0 })).unwrap_err();
        assert_eq!(code(&err), "unknown_animation");
        let err = play_anim(
            &mut w,
            &json!({ "action": "define", "name": "x", "animation": { "tracks": [] } }),
        )
        .unwrap_err();
        assert_eq!(code(&err), "bad_animation");
        let err = play_anim(&mut w, &json!({ "action": "zap" })).unwrap_err();
        assert_eq!(code(&err), "bad_action");
    }

    #[test]
    fn sounds_table_edits_and_validation() {
        let mut w = World::new();
        let out = sounds(
            &mut w,
            &json!({ "sounds": { "hover": "ui_hover", "press": "ui_press" }, "max_per_frame": 2 }),
        )
        .unwrap();
        assert_eq!(out["sounds"]["hover"], "ui_hover");
        assert_eq!(out["max_per_frame"], 2);
        let out = sounds(&mut w, &json!({ "sounds": { "hover": null }, "enabled": false })).unwrap();
        assert!(out["sounds"].get("hover").is_none());
        assert_eq!(out["enabled"], false);
        assert_eq!(
            code(&sounds(&mut w, &json!({ "sounds": { "zap": "x" } })).unwrap_err()),
            "bad_sounds"
        );
        assert_eq!(
            code(&sounds(&mut w, &json!({ "max_per_frame": 0 })).unwrap_err()),
            "bad_max"
        );
    }

    #[test]
    fn tree_set_expand_select_key() {
        let mut w = World::new();
        let e = w.spawn_empty().id();
        let spec =
            json!([{ "label": "a", "children": [{ "label": "a1" }, { "label": "a2" }] }, { "label": "b" }]);
        let out = tree(
            &mut w,
            &json!({ "entity": e.to_bits(), "action": "set", "tree": spec }),
        )
        .unwrap();
        assert_eq!(out["visible"], 2);
        assert!(w.get::<ListItems>(e).is_some());
        let out = tree(
            &mut w,
            &json!({ "entity": e.to_bits(), "action": "expand", "node": 0 }),
        )
        .unwrap();
        assert_eq!(out["visible"], 4);
        let out = tree(
            &mut w,
            &json!({ "entity": e.to_bits(), "action": "key", "key": "down" }),
        )
        .unwrap();
        assert_eq!(out["selected"], 0);
        let out = tree(
            &mut w,
            &json!({ "entity": e.to_bits(), "action": "select", "node": 2 }),
        )
        .unwrap();
        assert_eq!(out["selected"], 2);
        assert_eq!(
            code(
                &tree(
                    &mut w,
                    &json!({ "entity": e.to_bits(), "action": "expand", "node": 99 })
                )
                .unwrap_err()
            ),
            "unknown_node"
        );
        assert_eq!(
            code(
                &tree(
                    &mut w,
                    &json!({ "entity": e.to_bits(), "action": "key", "key": "x" })
                )
                .unwrap_err()
            ),
            "bad_key"
        );
        let other = w.spawn_empty().id();
        assert_eq!(
            code(&tree(&mut w, &json!({ "entity": other.to_bits() })).unwrap_err()),
            "no_tree"
        );
        assert_eq!(
            code(
                &tree(
                    &mut w,
                    &json!({ "entity": other.to_bits(), "action": "set", "tree": 5 })
                )
                .unwrap_err()
            ),
            "bad_tree"
        );
    }

    #[test]
    fn color_picker_edits() {
        let mut w = World::new();
        let e = w.spawn_empty().id();
        let out = color_picker(&mut w, &json!({ "entity": e.to_bits(), "hex": "#336699" })).unwrap();
        assert_eq!(out["color"]["hex"], "#336699");
        let out = color_picker(
            &mut w,
            &json!({ "entity": e.to_bits(), "hue": 0.0, "square": [1.0, 0.0] }),
        )
        .unwrap();
        assert_eq!(out["color"]["hex"], "#FF0000");
        let err = color_picker(&mut w, &json!({ "entity": e.to_bits(), "hex": "nope" })).unwrap_err();
        assert_eq!(code(&err), "bad_color");
        assert_eq!(
            w.get::<ColorPicker>(e).unwrap().to_hex(),
            "#FF0000",
            "a failed call changes nothing"
        );
    }

    #[test]
    fn soft_cursor_settings_and_nudge() {
        let mut w = World::new();
        let out = soft_cursor(
            &mut w,
            &json!({ "enabled": true, "speed": 800.0, "dead_zone": 0.0, "position": [100, 100] }),
        )
        .unwrap();
        assert_eq!(out["active"], true);
        let out = soft_cursor(&mut w, &json!({ "nudge": { "stick": [1.0, 0.0], "dt": 0.5 } })).unwrap();
        assert_eq!(out["position"], json!([500.0, 100.0]));
        assert_eq!(
            code(&soft_cursor(&mut w, &json!({ "stick": "up" })).unwrap_err()),
            "bad_cursor"
        );
        assert_eq!(
            code(&soft_cursor(&mut w, &json!({ "speed": -1 })).unwrap_err()),
            "bad_cursor"
        );
        let out = soft_cursor(&mut w, &json!({ "enabled": false })).unwrap();
        assert_eq!(out["active"], false);
    }

    #[test]
    fn keyboard_mode_reports_and_validates() {
        let mut w = World::new();
        let out = keyboard(&mut w, &json!({})).unwrap();
        assert_eq!(out["mode"], "auto");
        assert_eq!(out["window_ime"], Value::Null);
        let out = keyboard(&mut w, &json!({ "mode": "off" })).unwrap();
        assert_eq!(out["mode"], "off");
        assert_eq!(
            code(&keyboard(&mut w, &json!({ "mode": "loud" })).unwrap_err()),
            "bad_mode"
        );
    }

    #[test]
    fn touch_live_switches_and_reports() {
        let mut w = World::new();
        let out = touch_live(&mut w, &json!({})).unwrap();
        assert_eq!(out["enabled"], false);
        assert_eq!(out["recent"], json!([]));
        let out = touch_live(&mut w, &json!({ "enabled": true })).unwrap();
        assert_eq!(out["enabled"], true);
        assert!(w.resource::<regia_ui::gesture::UiGestureInput>().enabled);
        let err = touch_live(&mut w, &json!({ "enabled": 1 })).unwrap_err();
        assert_eq!(code(&err), "bad_flag");
    }

    #[test]
    fn touch_replays_a_swipe_and_a_pinch() {
        let mut w = World::new();
        let swipe = json!({ "samples": [
            { "id": 1, "stage": "down", "pos": [0, 0], "time": 0.0 },
            { "id": 1, "stage": "up", "pos": [0, -150], "time": 0.1 },
        ]});
        let out = touch(&mut w, &swipe).unwrap();
        assert_eq!(out["gestures"][0]["kind"], "swipe");
        assert_eq!(out["gestures"][0]["direction"], "up");
        let pinch = json!({ "samples": [
            { "id": 1, "stage": "down", "pos": [100, 0], "time": 0.0 },
            { "id": 2, "stage": "down", "pos": [200, 0], "time": 0.0 },
            { "id": 2, "stage": "move", "pos": [300, 0], "time": 0.1 },
        ]});
        let out = touch(&mut w, &pinch).unwrap();
        assert_eq!(out["gestures"][0]["kind"], "pinch");
        assert_eq!(out["active_touches"], 2);
        let err = touch(&mut w, &json!({ "samples": [{ "id": 1 }] })).unwrap_err();
        assert_eq!(code(&err), "bad_samples");
    }
}

#[cfg(test)]
mod recursive_disabled_dispatch_tests {
    use super::*;

    #[test]
    fn disabled_subtrees_are_inspected_and_local_flags_survive_enabling() {
        let mut world = World::new();
        world.init_resource::<regia_app::Time>();
        let root = world.spawn(regia_ui::UiNode).id();
        let child = world.spawn((regia_ui::UiNode, ChildOf(root))).id();
        let local = world
            .spawn((regia_ui::UiNode, regia_ui::Disabled, ChildOf(root)))
            .id();
        let call = |world: &mut World, disabled| {
            crate::dispatch::call_in_process(
                world,
                "ui.set_disabled",
                json!({"entity":root.to_bits(), "disabled":disabled, "recursive":true}),
            )
            .expect("sync")
            .expect("set disabled")
        };
        call(&mut world, true);
        assert!(regia_ui::node::is_disabled(&world, child));
        let inspected =
            crate::dispatch::call_in_process(&mut world, "ui.inspect", json!({"entity":child.to_bits()}))
                .expect("sync")
                .expect("inspect");
        assert!(
            inspected["states"]
                .as_array()
                .is_some_and(|states| states.contains(&json!("disabled"))),
            "{inspected}"
        );
        call(&mut world, false);
        assert!(!regia_ui::node::is_disabled(&world, child));
        assert!(regia_ui::node::is_disabled(&world, local));
    }
}
