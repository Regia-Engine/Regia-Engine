// SPDX-License-Identifier: MPL-2.0
//! `ui.nav_graph`, `ui.nav_move` and `ui.nav_lint`: see, drive and check the UI focus graph
//! (book `agents/ui-navigation.md`, ADR-516).
//!
//! The graph is the one keyboard and gamepad presses use (`regia_ui::focus_graph`): explicit
//! links, focus groups with wrap/stop/escape edges, Tab indices, remembered group children and
//! focus recovery. `ui.nav_move` applies a press at once, so its answer already holds the new
//! focus.

use regia_ecs::prelude::*;
use regia_ui::focus::{FocusState, NavDirection, UiInputMode};
use regia_ui::focus_graph::{
    ALL_DIRECTIONS, FocusMemory, NavError, NavGraph, direction_name, nav_move, parse_direction,
    world_defaults, world_graph, world_scope,
};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

/// Most nodes `ui.nav_graph` returns in one reply.
const MAX_NODES: usize = 512;

fn error(e: NavError) -> RpcError {
    match e {
        NavError::NotInstalled => RpcError::new(
            UNSUPPORTED,
            e.code(),
            e.to_string(),
            "the app needs regia_ui::UiPlugin",
        ),
        NavError::NoFocusables => RpcError::new(
            NOT_FOUND,
            e.code(),
            e.to_string(),
            "spawn Focusable nodes with a laid-out rect, or check ui.layers for a layer that hides them",
        ),
    }
}

fn bits(e: Option<Entity>) -> Value {
    e.map_or(Value::Null, |e| json!(e.to_bits()))
}

fn rect_json(r: [f32; 4]) -> Value {
    json!({ "x": r[0], "y": r[1], "w": r[2], "h": r[3] })
}

fn input_mode(world: &World) -> &'static str {
    match world.get_resource::<UiInputMode>() {
        Some(UiInputMode::Navigation) => "navigation",
        _ => "pointer",
    }
}

fn installed(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<FocusState>().is_none() {
        return Err(error(NavError::NotInstalled));
    }
    Ok(())
}

fn focused(world: &World) -> Option<Entity> {
    world.get_resource::<FocusState>().and_then(|s| s.focused)
}

/// `ui.nav_graph {}`: every navigable node of the active scope with where each of the six moves
/// goes from it now, and the focus groups.
pub(crate) fn nav_graph(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let graph = world_graph(world);
    let memory = world.get_resource::<FocusMemory>().cloned().unwrap_or_default();
    let nodes: Vec<Value> = graph
        .nodes()
        .iter()
        .take(MAX_NODES)
        .map(|n| {
            let mut moves = Map::new();
            for dir in ALL_DIRECTIONS {
                let step = graph.step(n.entity, n.rect, dir, &memory);
                moves.insert(
                    direction_name(dir).to_owned(),
                    json!({ "target": bits(step.target), "reason": step.reason.name() }),
                );
            }
            json!({
                "entity": n.entity.to_bits(),
                "rect": rect_json(n.rect),
                "tab_index": n.tab_index,
                "groups": n.groups.iter().map(|&g| graph.groups()[g].entity.to_bits()).collect::<Vec<_>>(),
                "links": {
                    "up": bits(n.links.up),
                    "down": bits(n.links.down),
                    "left": bits(n.links.left),
                    "right": bits(n.links.right),
                },
                "moves": moves,
                // `.ui.ron` focus neighbors by name (ADR-7827): what the document declared, next to
                // the resolved `links`; null when the node declares none.
                "name": world.get::<regia_ui::NodeName>(n.entity).map(|name| name.0.clone()),
                "declared": world
                    .get::<regia_ui::authoring::DeclaredNav>(n.entity)
                    .map(|d| json!({
                        "up": d.def.up,
                        "down": d.def.down,
                        "left": d.def.left,
                        "right": d.def.right,
                        "resolved": d.resolved,
                    })),
            })
        })
        .collect();
    let groups: Vec<Value> = graph
        .groups()
        .iter()
        .map(|g| {
            json!({
                "entity": g.entity.to_bits(),
                "edges": {
                    "up": g.rules.up.name(),
                    "down": g.rules.down.name(),
                    "left": g.rules.left.name(),
                    "right": g.rules.right.name(),
                },
                "tab": g.rules.tab.name(),
                "remember": g.rules.remember,
                "remembered": bits(memory.remembered(g.entity)),
                "members": g.members.iter().map(|&i| graph.nodes()[i].entity.to_bits()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "scope": bits(world_scope(world)),
        "focused": bits(focused(world)),
        "input_mode": input_mode(world),
        "nodes": nodes,
        "groups": groups,
        "count": graph.nodes().len(),
        "truncated": graph.nodes().len() > MAX_NODES,
    }))
}

fn direction_param(params: &Value) -> Result<NavDirection, RpcError> {
    let name = params.get("direction").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`direction` is required: up, down, left, right, next or prev")
    })?;
    parse_direction(name).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "unknown_direction",
            format!("unknown direction {name:?}"),
            "use up, down, left, right, next or prev",
        )
    })
}

/// `ui.nav_move { direction }`: one navigation press, applied at once.
pub(crate) fn nav_move_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = direction_param(params)?;
    let moved = nav_move(world, dir).map_err(error)?;
    Ok(json!({
        "direction": direction_name(dir),
        "from": bits(moved.from),
        "to": bits(moved.step.target),
        "moved": moved.focused != moved.from,
        "reason": moved.step.reason.name(),
        "focused": bits(moved.focused),
        "input_mode": input_mode(world),
    }))
}

/// `ui.nav_lint { start? }`: structural problems of the active scope's graph.
pub(crate) fn nav_lint(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let start = match params.get("start") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params("`start` must be entity bits from ui.nav_graph"))?,
        ),
    };
    let graph: NavGraph = world_graph(world);
    if let Some(s) = start
        && !graph.contains(s)
    {
        return Err(RpcError::new(
            NOT_FOUND,
            "not_navigable",
            "the start node is not navigable in the active scope",
            "take a node from ui.nav_graph",
        ));
    }
    let defaults = world_defaults(world);
    let start = start.or_else(|| focused(world).filter(|f| graph.contains(*f)));
    let issues: Vec<Value> = graph
        .lint(start, &defaults)
        .iter()
        .map(|i| {
            json!({
                "code": i.kind.code(),
                "node": i.node.to_bits(),
                "other": bits(i.other),
                "direction": i.direction.map(direction_name),
            })
        })
        .collect();
    Ok(json!({
        "start": bits(start.or_else(|| graph.initial(&defaults))),
        "nodes": graph.nodes().len(),
        "clean": issues.is_empty(),
        "issues": issues,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::focus::{Focusable, NavLinks};
    use regia_ui::focus_graph::FocusGroup;
    use regia_ui::{ComputedNode, Focused, UiEvent};

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<FocusState>();
        world.init_resource::<FocusMemory>();
        world.init_resource::<UiInputMode>();
        world.init_resource::<Messages<UiEvent>>();
        world
    }

    fn node(world: &mut World, rect: [f32; 4]) -> Entity {
        world
            .spawn((
                Focusable::default(),
                ComputedNode {
                    rect,
                    clip_rect: rect,
                    scale: 1.0,
                    ..Default::default()
                },
            ))
            .id()
    }

    fn code(e: RpcError) -> String {
        e.data.expect("data")["code"].as_str().expect("code").to_owned()
    }

    #[test]
    fn graph_lists_moves_and_groups() {
        let mut world = world();
        let row = world.spawn(FocusGroup::wrap_horizontal()).id();
        let a = node(&mut world, [0.0, 0.0, 100.0, 40.0]);
        let b = node(&mut world, [120.0, 0.0, 100.0, 40.0]);
        world.entity_mut(a).insert(ChildOf(row));
        world.entity_mut(b).insert(ChildOf(row));
        let out = nav_graph(&mut world, &json!({})).expect("graph");
        assert_eq!(out["count"], json!(2));
        assert_eq!(out["nodes"][0]["entity"], json!(a.to_bits()));
        assert_eq!(out["nodes"][0]["moves"]["right"]["target"], json!(b.to_bits()));
        assert_eq!(out["nodes"][0]["moves"]["right"]["reason"], json!("geometry"));
        assert_eq!(out["nodes"][1]["moves"]["right"]["reason"], json!("wrap"));
        assert_eq!(out["nodes"][1]["moves"]["right"]["target"], json!(a.to_bits()));
        assert_eq!(out["nodes"][0]["moves"]["down"]["reason"], json!("edge"));
        assert_eq!(out["groups"][0]["edges"]["left"], json!("wrap"));
        assert_eq!(out["groups"][0]["edges"]["up"], json!("escape"));
        assert_eq!(out["groups"][0]["members"], json!([a.to_bits(), b.to_bits()]));
        assert_eq!(out["truncated"], json!(false));
    }

    /// ADR-7827: `ui.nav_graph` shows each node's name and the focus neighbors its `.ui.ron`
    /// declared (by name), beside the resolved links.
    #[test]
    fn graph_shows_declared_neighbors_by_name() {
        let mut world = world();
        let a = node(&mut world, [0.0, 0.0, 100.0, 40.0]);
        let b = node(&mut world, [0.0, 60.0, 100.0, 40.0]);
        world.entity_mut(a).insert((
            regia_ui::NodeName("apply".to_owned()),
            regia_ui::authoring::DeclaredNav {
                def: regia_ui::authoring::NavDef {
                    down: Some("quit".to_owned()),
                    ..Default::default()
                },
                resolved: true,
            },
            Focusable {
                nav: NavLinks {
                    down: Some(b),
                    ..NavLinks::default()
                },
            },
        ));
        // Through the dispatcher, as an agent calls it.
        let out = crate::dispatch::call_in_process(&mut world, "ui.nav_graph", json!({}))
            .expect("answered at once")
            .expect("graph");
        let first = &out["nodes"][0];
        assert_eq!(first["name"], json!("apply"));
        assert_eq!(first["declared"]["down"], json!("quit"));
        assert_eq!(first["declared"]["up"], Value::Null);
        assert_eq!(first["declared"]["resolved"], json!(true));
        assert_eq!(first["links"]["down"], json!(b.to_bits()));
        assert_eq!(first["moves"]["down"]["reason"], json!("explicit"));
        assert_eq!(out["nodes"][1]["declared"], Value::Null);
    }

    #[test]
    fn move_applies_at_once_and_reports_why() {
        let mut world = world();
        let a = node(&mut world, [0.0, 0.0, 100.0, 40.0]);
        let b = node(&mut world, [0.0, 60.0, 100.0, 40.0]);
        let out = nav_move_method(&mut world, &json!({ "direction": "down" })).expect("reveal");
        assert_eq!(out["reason"], json!("initial"));
        assert_eq!(out["focused"], json!(a.to_bits()));
        assert_eq!(out["input_mode"], json!("navigation"));
        let out = nav_move_method(&mut world, &json!({ "direction": "down" })).expect("move");
        assert_eq!(out["from"], json!(a.to_bits()));
        assert_eq!(out["to"], json!(b.to_bits()));
        assert_eq!(out["moved"], json!(true));
        assert!(world.get::<Focused>(b).is_some());
        let out = nav_move_method(&mut world, &json!({ "direction": "down" })).expect("edge");
        assert_eq!(out["moved"], json!(false));
        assert_eq!(out["reason"], json!("edge"));
    }

    #[test]
    fn lint_reports_bad_links_with_codes() {
        let mut world = world();
        let a = node(&mut world, [0.0, 0.0, 100.0, 40.0]);
        let gone = world.spawn_empty().id();
        world.entity_mut(a).insert(Focusable {
            nav: NavLinks {
                right: Some(gone),
                ..NavLinks::default()
            },
        });
        node(&mut world, [0.0, 60.0, 100.0, 40.0]);
        let out = nav_lint(&mut world, &json!({})).expect("lint");
        assert_eq!(out["clean"], json!(false));
        let codes: Vec<&str> = out["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|i| i["code"].as_str())
            .collect();
        assert!(codes.contains(&"dead_link"), "{out}");
        assert_eq!(out["issues"][0]["direction"], json!("right"));
    }

    #[test]
    fn errors_carry_lowercase_codes() {
        let mut world = world();
        let err = nav_move_method(&mut world, &json!({ "direction": "down" })).expect_err("empty");
        assert_eq!(code(err), "no_focusables");
        crate::contract::assert_listed("ui.nav_move", "no_focusables");
        let err = nav_move_method(&mut world, &json!({ "direction": "sideways" })).expect_err("bad");
        assert_eq!(code(err), "unknown_direction");
        assert!(
            nav_move_method(&mut world, &json!({})).is_err(),
            "direction required"
        );
        let stray = world.spawn_empty().id();
        node(&mut world, [0.0, 0.0, 10.0, 10.0]);
        let err = nav_lint(&mut world, &json!({ "start": stray.to_bits() })).expect_err("start");
        assert_eq!(code(err), "not_navigable");
        crate::contract::assert_listed("ui.nav_lint", "not_navigable");
        let mut bare = World::new();
        let err = nav_graph(&mut bare, &json!({})).expect_err("no ui");
        assert_eq!(code(err), "ui_not_installed");
        let err = nav_move_method(&mut bare, &json!({ "direction": "up" })).expect_err("no ui");
        assert_eq!(code(err), "ui_not_installed");
    }
}
