// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for logic graphs (`docs/specs/102-logic-graph.md` § 10, phase 1,
//! `book/agents/logic-graph.md`): `logic.compile`, `logic.control`, `logic.emit`, `logic.reload` and
//! `logic.status`; phase 3 adds `logic.world` and `logic.world_set` (the world blackboards).
//!
//! Errors are `F0002` with `data.reason`; compile failures use the reasons of
//! `regia_ai::logic::LogicGraphError::reason`.

use regia_ai::logic::{
    LogicGraph, LogicGraphDef, LogicGraphError, LogicGraphStatus, LogicInbox, LogicReloadOutcome,
    LogicReloadRecord, LogicReloads, LogicWorldBlackboards,
};
use regia_ai::{BbValue, BtContext, BtTaskRegistry};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

const PAGE: &str = "see book/agents/logic-graph.md";

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn missing(param: &str) -> RpcError {
    refused("missing_param", format!("`{param}` is required"), PAGE)
}

/// A compile error as the protocol shows it.
fn error_json(err: &LogicGraphError) -> Value {
    let mut out = json!({ "reason": err.reason(), "detail": err.to_string() });
    if let (Some(rule), Some(map)) = (err.rule(), out.as_object_mut()) {
        map.insert("rule".into(), json!(rule));
    }
    out
}

fn read_project_text(world: &World, path: &str) -> Option<String> {
    // ADR-8798: the shared root (`AgentSettings::project_dir` counts when no asset server is set).
    let file = crate::req_path::project_file(world, "path", path).ok()?;
    std::fs::read_to_string(file).ok()
}

/// The definition text from `text`, or from the project file `path`.
fn source_text(world: &World, params: &Value) -> Result<String, RpcError> {
    if let Some(text) = crate::params::opt_str(params, "text")? {
        return Ok(text.to_string());
    }
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("text` or `path"))?;
    if regia_core::project_path::check_relative(path).is_err() {
        return Err(RpcError::invalid_params(
            "`path` must be a relative path inside the project (no `..`)",
        ));
    }
    read_project_text(world, path).ok_or_else(|| {
        RpcError::not_found(
            format!("{path}: cannot be read"),
            "check the path with assets.list or ls",
        )
    })
}

fn compile_text(world: &World, text: &str) -> Result<LogicGraph, LogicGraphError> {
    let def = LogicGraphDef::from_ron(text)?;
    let empty = BtTaskRegistry::default();
    let registry = world.get_resource::<BtTaskRegistry>().unwrap_or(&empty);
    LogicGraph::compile(&def, registry)
}

fn entity_not_found() -> RpcError {
    refused(
        "entity_not_found",
        "no such entity",
        "use scene.query to find the graph entity",
    )
}

fn no_logic_graph() -> RpcError {
    refused(
        "no_logic_graph",
        "the entity has no LogicGraph",
        "attach a compiled regia_ai::logic::LogicGraph to the entity",
    )
}

/// The `entity` parameter: a live entity that holds a `LogicGraph`.
fn graph_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| missing("entity"))?;
    let entity = Entity::try_from_bits(bits).ok_or_else(entity_not_found)?;
    let Ok(e) = world.get_entity(entity) else {
        return Err(entity_not_found());
    };
    if !e.contains::<LogicGraph>() {
        return Err(no_logic_graph());
    }
    Ok(entity)
}

/// Runs `f` on the entity's graph with the graph taken out of the world (the tick system's
/// take-run-reinsert), then refreshes its `LogicGraphStatus`.
fn with_graph<R>(
    world: &mut World,
    entity: Entity,
    f: impl FnOnce(&mut LogicGraph, &mut BtContext<'_>) -> R,
) -> Result<R, RpcError> {
    let mut graph = world
        .get_entity_mut(entity)
        .ok()
        .and_then(|mut e| e.take::<LogicGraph>())
        .ok_or_else(no_logic_graph)?;
    let dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f32());
    let result = f(&mut graph, &mut BtContext { world, entity, dt });
    let status = LogicGraphStatus::of(&graph);
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert((graph, status));
    }
    Ok(result)
}

fn value_json(value: &BbValue) -> Value {
    match value {
        BbValue::Bool(v) => json!(v),
        BbValue::Int(v) => json!(v),
        BbValue::Float(v) => json!(v),
        BbValue::String(v) => json!(v),
        BbValue::Vec3(v) => json!([v.x, v.y, v.z]),
        BbValue::Entity(bits) => json!({ "entity": bits }),
    }
}

/// A `LogicGraphStatus` as JSON.
pub fn status_json(status: &LogicGraphStatus) -> Value {
    json!({
        "running": status.running,
        "paused": status.paused,
        "ticks": status.ticks,
        "evaluations": status.evaluations,
        "rules": status.rules.iter().map(|r| json!({
            "name": r.name,
            "fires": r.fires,
            "matches": r.matches,
            "last_fired_tick": r.last_fired_tick,
            "cooldown_remaining": r.cooldown_remaining,
            "chain_busy": r.chain_busy,
        })).collect::<Vec<_>>(),
        "locals": status.locals.iter().map(|l| (l.name.clone(), value_json(&l.value))).collect::<serde_json::Map<_, _>>(),
        "world": status.world.iter().map(|l| (l.name.clone(), value_json(&l.value))).collect::<serde_json::Map<_, _>>(),
        "trace": status.trace,
    })
}

/// `logic.compile`: parses and compiles a `.logic.ron` (from `text` or a project `path`) without
/// building an entity. Never mutates the world.
pub fn compile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = source_text(world, params)?;
    Ok(match compile_text(world, &text) {
        Ok(graph) => json!({
            "ok": true,
            "rules": graph.rule_order().collect::<Vec<_>>(),
            "errors": [],
        }),
        Err(err) => json!({ "ok": false, "rules": [], "errors": [error_json(&err)] }),
    })
}

/// `logic.control`: `pause`, `resume` or `restart` the graph on `entity`.
pub fn control(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = graph_entity(world, params)?;
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("op"))?;
    if !matches!(op, "pause" | "resume" | "restart" | "step") {
        return Err(refused(
            "bad_op",
            format!("unknown op {op:?}"),
            "op is one of pause, resume, restart, step",
        ));
    }
    with_graph(world, entity, |graph, ctx| {
        match op {
            "pause" => graph.pause(),
            "resume" => graph.resume(),
            // One more tick, then paused (spec 102 § 13 P4.1).
            "step" => graph.step(),
            _ => graph.restart(ctx),
        }
        let status = if graph.is_paused() {
            "paused"
        } else if graph.is_started() {
            "running"
        } else {
            "not_started"
        };
        json!({ "status": status, "paused": graph.is_paused() })
    })
}

/// A list-of-names parameter: absent is `None`; anything but an array of strings is `bad_value`.
#[cfg(debug_assertions)]
fn name_list(params: &Value, field: &str) -> Result<Option<Vec<String>>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    refused(
                        "bad_value",
                        format!("`{field}` must be an array of names"),
                        "pass names as strings",
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(refused(
            "bad_value",
            format!("`{field}` must be an array of names"),
            "pass names as strings",
        )),
    }
}

/// `logic.debug` (development builds, spec 102 § 9): sets the rules that pause the graph when they
/// fire, the inbox events that pause it when they arrive and the rules that never match, through the
/// entity's `LogicGraphDebug`. Omitted lists keep their current value; an empty list clears one.
#[cfg(debug_assertions)]
pub fn debug(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_ai::logic::LogicGraphDebug;
    let entity = graph_entity(world, params)?;
    let pause_on_rule = name_list(params, "pause_on_rule")?;
    let pause_on_event = name_list(params, "pause_on_event")?;
    let disabled = name_list(params, "disabled")?;
    let graph = world.get::<LogicGraph>(entity).ok_or_else(no_logic_graph)?;
    for name in pause_on_rule.iter().chain(disabled.iter()).flatten() {
        if !graph.has_rule(name) {
            return Err(refused(
                "rule_not_found",
                format!("the graph has no rule named {name:?}"),
                "use the rule names in LogicGraphStatus.rules",
            ));
        }
    }
    if pause_on_event.iter().flatten().any(String::is_empty) {
        return Err(refused(
            "bad_value",
            "an event name is empty",
            "name the inbox event to pause on",
        ));
    }
    let current = world.get::<LogicGraphDebug>(entity).cloned().unwrap_or_default();
    let next = LogicGraphDebug {
        pause_on_rule: pause_on_rule.unwrap_or(current.pause_on_rule),
        pause_on_event: pause_on_event.unwrap_or(current.pause_on_event),
        disabled: disabled.unwrap_or(current.disabled),
    };
    if let Some(mut graph) = world.get_mut::<LogicGraph>(entity) {
        graph.set_debug(&next.disabled, &next.pause_on_rule, &next.pause_on_event);
    }
    let out = json!({
        "pause_on_rule": next.pause_on_rule,
        "pause_on_event": next.pause_on_event,
        "disabled": next.disabled,
    });
    world.entity_mut(entity).insert(next);
    Ok(out)
}

/// `logic.debug` in a shipping build: always `dev_only`.
#[cfg(not(debug_assertions))]
pub fn debug(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Err(refused(
        "dev_only",
        "logic.debug is available in development builds only",
        "run a debug build",
    ))
}

/// `logic.emit`: appends `event` to the entity's `LogicInbox`; the graph sees it on its next tick.
pub fn emit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = graph_entity(world, params)?;
    let event = params
        .get("event")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("event"))?;
    if event.is_empty() {
        return Err(refused(
            "bad_value",
            "`event` is empty",
            "name the event a rule listens for",
        ));
    }
    let mut e = world.entity_mut(entity);
    let queued = match e.get_mut::<LogicInbox>() {
        Some(mut inbox) => {
            inbox.push(event);
            inbox.events.len()
        }
        None => {
            e.insert(LogicInbox {
                events: vec![event.to_string()],
            });
            1
        }
    };
    Ok(json!({ "queued": queued }))
}

/// `logic.reload`: compiles new text and hot-swaps it into the entity's graph. A definition that
/// does not compile leaves the running graph untouched (and is reported with its reason).
pub fn reload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = graph_entity(world, params)?;
    let text = source_text(world, params)?;
    let source = crate::params::opt_str(params, "path")?
        .filter(|_| params.get("text").is_none())
        .unwrap_or("agent")
        .to_string();
    let next = match compile_text(world, &text) {
        Ok(next) => next,
        Err(err) => {
            if let Some(mut reloads) = world.get_resource_mut::<LogicReloads>() {
                reloads.reject(entity, source, err.reason());
            }
            return Err(refused(
                err.reason(),
                err.to_string(),
                "fix the definition; see book/agents/logic-graph.md",
            ));
        }
    };
    let outcome = with_graph(world, entity, |graph, ctx| graph.reload(next, ctx))?;
    if let Some(mut reloads) = world.get_resource_mut::<LogicReloads>() {
        reloads.push_record(LogicReloadRecord {
            entity,
            source,
            result: Ok(outcome),
        });
    }
    Ok(json!({
        "outcome": match outcome {
            LogicReloadOutcome::Kept => "kept",
            LogicReloadOutcome::Restarted => "restarted",
        }
    }))
}

/// `logic.status`: the entity's `LogicGraphStatus` (as of its last tick, or a fresh snapshot when it
/// has not ticked yet).
pub fn status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = graph_entity(world, params)?;
    let status = match world.get::<LogicGraphStatus>(entity) {
        Some(status) => status.clone(),
        None => world
            .get::<LogicGraph>(entity)
            .map(|g| LogicGraphStatus::of_in(g, world.get_resource::<LogicWorldBlackboards>()))
            .ok_or_else(no_logic_graph)?,
    };
    Ok(status_json(&status))
}

fn board_json(board: &regia_ai::Blackboard) -> Value {
    Value::Object(
        board
            .values
            .iter()
            .map(|(k, v)| (k.clone(), value_json(v)))
            .collect(),
    )
}

/// `logic.world`: the world blackboards graphs share (spec 102 § 5), all of them or the one named
/// `board` (`board_not_found` when no graph declared it and no host created it).
pub fn world(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let boards = world.get_resource::<LogicWorldBlackboards>();
    match crate::params::opt_str(params, "board")? {
        Some(name) => {
            let board = boards.and_then(|b| b.get(name)).ok_or_else(|| {
                refused(
                    "board_not_found",
                    format!("no world blackboard named {name:?}"),
                    "a graph creates the boards its world: list declares on its first tick; logic.world lists them",
                )
            })?;
            Ok(json!({ "boards": { name: board_json(board) } }))
        }
        None => Ok(json!({
            "boards": boards
                .map(|b| b.boards.iter().map(|(n, bb)| (n.clone(), board_json(bb))).collect::<serde_json::Map<_, _>>())
                .unwrap_or_default(),
        })),
    }
}

/// A JSON value as a blackboard value: bool, integer, number, string, `[x, y, z]` or
/// `{ "entity": bits }`.
fn json_value(value: &Value) -> Option<BbValue> {
    Some(match value {
        Value::Bool(b) => BbValue::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => BbValue::Int(i),
            None => {
                let f = n.as_f64()? as f32;
                if !f.is_finite() {
                    return None;
                }
                BbValue::Float(f)
            }
        },
        Value::String(s) => BbValue::String(s.clone()),
        Value::Array(items) if items.len() == 3 => {
            let c = |i: usize| items[i].as_f64().map(|v| v as f32).filter(|v| v.is_finite());
            BbValue::Vec3(regia_core::math::Vec3::new(c(0)?, c(1)?, c(2)?))
        }
        Value::Object(map) if map.len() == 1 => BbValue::Entity(map.get("entity")?.as_u64()?),
        _ => return None,
    })
}

/// `logic.world_set`: writes `value` under `key` in world blackboard `board` (created when
/// missing). The write counts like a graph's: `BlackboardChanged` arms on `world/<board>/<key>` fire
/// on their next tick.
pub fn world_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let board = params
        .get("board")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("board"))?;
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("key"))?;
    let raw = params.get("value").ok_or_else(|| missing("value"))?;
    if board.is_empty() || board.contains('/') || key.is_empty() {
        return Err(refused(
            "bad_value",
            "`board` must be a non-empty name without `/`, and `key` non-empty",
            PAGE,
        ));
    }
    let value = json_value(raw).ok_or_else(|| {
        refused(
            "bad_value",
            "`value` must be a bool, a number, a string, [x, y, z] or { \"entity\": bits }",
            PAGE,
        )
    })?;
    let mut boards = world.get_resource_or_insert_with(LogicWorldBlackboards::default);
    let target = boards.board_mut(board);
    target.insert(key, value.clone());
    let writes = target.write_count(key);
    Ok(json!({ "board": board, "key": key, "value": value_json(&value), "writes": writes }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRAPH: &str = r#"(format_version: 1,
        locals: [(name: "n", default: Int(0))],
        rules: [
          (name: "count", event: EntityEvent("ping"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) }),
          ]),
        ])"#;

    fn world_with_graph() -> (World, Entity) {
        let mut world = World::new();
        let def = LogicGraphDef::from_ron(GRAPH).expect("parses");
        let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
        let e = world.spawn((graph, LogicInbox::default())).id();
        (world, e)
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
    fn compile_reports_ok_and_each_error_reason() {
        let mut world = World::new();
        let ok = compile(&mut world, &json!({ "text": GRAPH })).expect("answers");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["rules"], json!(["count"]));
        let cases = [
            (
                "(format_version: 1, rules: [(name: \"a\", event: Start), (name: \"a\", event: Start)])",
                "duplicate_name",
            ),
            (
                "(format_version: 1, rules: [(name: \"a\", event: Start, actions: [(task: \"x::Y\")])])",
                "unknown_task",
            ),
            (
                "(format_version: 1, rules: [(name: \"a\", event: Start, condition: Key(\"zone/x\"))])",
                "unknown_scope",
            ),
            // Since phase 3 the input arm and world boards compile (ADR-7268); `unsupported_feature`
            // is no longer raised.
            (
                "(format_version: 1, rules: [(name: \"a\", event: InputAction(action: \"\"))])",
                "bad_value",
            ),
            (
                "(format_version: 1, world: [\"w\"], rules: [(name: \"a\", event: Start, condition: Key(\"world/v/x\"))])",
                "unknown_scope",
            ),
            ("(format_version: 9)", "unsupported_version"),
            (
                "(format_version: 1, rules: [(name: \"\", event: Start)])",
                "bad_value",
            ),
            (
                "(format_version: 1, rules: [(name: \"a\", event: Start, actions: [(task: \"logic::EmitEvent\")])])",
                "missing_param",
            ),
            (
                "(format_version: 1, rules: [(name: \"a\", event: Start, actions: [(task: \"logic::SetBlackboard\", set: { \"key\": Literal(String(\"x\")), \"op\": Literal(String(\"pow\")) })])])",
                "bad_op",
            ),
            ("(", "parse"),
        ];
        for (text, want) in cases {
            let out = compile(&mut world, &json!({ "text": text })).expect("answers");
            assert_eq!(out["ok"], false, "{text}");
            assert_eq!(out["errors"][0]["reason"], want, "{text}");
        }
        let err = compile(&mut world, &json!({})).expect_err("needs text or path");
        assert_eq!(reason(&err), "missing_param");
    }

    #[test]
    fn control_emit_status_and_reload_report_each_reason() {
        let (mut world, e) = world_with_graph();
        let bits = e.to_bits();
        let bare = world.spawn_empty().id().to_bits();
        for method in [control, emit, reload, status] {
            assert_eq!(
                reason(&method(&mut world, &json!({})).expect_err("no entity")),
                "missing_param"
            );
            assert_eq!(
                reason(
                    &method(
                        &mut world,
                        &json!({ "entity": u64::MAX - 7, "op": "pause", "event": "x", "text": GRAPH })
                    )
                    .expect_err("gone")
                ),
                "entity_not_found"
            );
            assert_eq!(
                reason(
                    &method(
                        &mut world,
                        &json!({ "entity": bare, "op": "pause", "event": "x", "text": GRAPH })
                    )
                    .expect_err("no graph")
                ),
                "no_logic_graph"
            );
        }
        assert_eq!(
            reason(&control(&mut world, &json!({ "entity": bits })).expect_err("op")),
            "missing_param"
        );
        assert_eq!(
            reason(&control(&mut world, &json!({ "entity": bits, "op": "fly" })).expect_err("op")),
            "bad_op"
        );
        assert_eq!(
            reason(&emit(&mut world, &json!({ "entity": bits })).expect_err("event")),
            "missing_param"
        );
        assert_eq!(
            reason(&emit(&mut world, &json!({ "entity": bits, "event": "" })).expect_err("event")),
            "bad_value"
        );
        assert_eq!(
            reason(&reload(&mut world, &json!({ "entity": bits })).expect_err("text")),
            "missing_param"
        );
        let bad = reload(&mut world, &json!({ "entity": bits, "text": "(format_version: 1, rules: [(name: \"a\", event: Start, actions: [(task: \"x::Y\")])])" }))
            .expect_err("does not compile");
        assert_eq!(reason(&bad), "unknown_task");
        assert!(
            world.get::<LogicGraph>(e).is_some(),
            "the old graph keeps running"
        );
    }

    #[test]
    fn emit_queues_for_the_next_tick_and_control_and_reload_steer_the_graph() {
        let (mut world, e) = world_with_graph();
        let bits = e.to_bits();
        assert_eq!(
            emit(&mut world, &json!({ "entity": bits, "event": "ping" })).expect("emits")["queued"],
            1
        );
        let paused = control(&mut world, &json!({ "entity": bits, "op": "pause" })).expect("pauses");
        assert_eq!(paused["paused"], true);
        let resumed = control(&mut world, &json!({ "entity": bits, "op": "resume" })).expect("resumes");
        assert_eq!(resumed["status"], "not_started");
        let kept = reload(&mut world, &json!({ "entity": bits, "text": GRAPH })).expect("reloads");
        assert_eq!(kept["outcome"], "kept");
        let s = status(&mut world, &json!({ "entity": bits })).expect("status");
        assert_eq!(s["rules"][0]["name"], "count");
        assert_eq!(s["locals"]["n"], 0);
        assert_eq!(s["trace"][0], "tick 0: reload_kept");
    }

    // -- Dogfood through the dispatcher (spec 102 § 13 P1.10) ---------------------------------

    use crate::dispatch::{self, AgentInbox, RateLimit};
    use crate::playtest::AgentTimeControl;
    use crate::protocol::{Request, Response};
    use crate::safety::{AgentConfig, AgentPermissions};
    use crate::server::Inbound;

    fn dogfood_app() -> App {
        let mut app = App::new();
        app.init_resource::<AppTypeRegistry>()
            .add_plugins(MinimalPlugins)
            .add_plugins(regia_ai::AiPlugin)
            .insert_resource(AgentPermissions {
                config: AgentConfig {
                    rate_limit_per_second: 10_000,
                    heavy_rate_limit_per_second: 10_000,
                    ..AgentConfig::default()
                },
                source: None,
            })
            .init_resource::<crate::safety::AuditLog>()
            .init_resource::<crate::safety::AgentSession>()
            .init_resource::<AgentTimeControl>()
            .init_resource::<RateLimit>();
        app
    }

    /// Sends one request through the dispatcher; a deferred method (`play.wait_until`) is driven
    /// by running fixed steps and polling the waiters, at most `steps` times.
    fn send(app: &mut App, method: &str, params: Value, steps: usize) -> Response {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply: tx,
            })
            .expect("queues");
        app.world_mut().insert_resource(AgentInbox(inbox_rx));
        dispatch::process_requests(app.world_mut());
        drop(inbox_tx);
        for _ in 0..=steps {
            if let Ok(response) = rx.try_recv() {
                return response;
            }
            app.world_mut().run_schedule(FixedUpdate);
            crate::playtest::advance_waiters(app.world_mut());
        }
        panic!("{method} did not answer within {steps} fixed steps");
    }

    fn ok(response: Response) -> Value {
        match (response.result, response.error) {
            (Some(result), None) => result,
            (_, error) => panic!("call failed: {error:?}"),
        }
    }

    #[test]
    fn dogfood_spawn_emit_wait_until_and_pause_resume_through_the_dispatcher() {
        let mut app = dogfood_app();
        let compiled = ok(send(&mut app, "logic.compile", json!({ "text": GRAPH }), 0));
        assert_eq!(compiled["ok"], true);
        let def = LogicGraphDef::from_ron(GRAPH).expect("parses");
        let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
        let e = app.world_mut().spawn((graph, LogicInbox::default())).id();
        let bits = e.to_bits();

        let queued = ok(send(
            &mut app,
            "logic.emit",
            json!({ "entity": bits, "event": "ping" }),
            0,
        ));
        assert_eq!(queued["queued"], 1);
        let waited = ok(send(
            &mut app,
            "play.wait_until",
            json!({
                "condition": { "entity": bits, "component": "LogicGraphStatus", "path": "ticks", "op": "gte", "value": 1 },
                "timeout_ms": 60_000
            }),
            8,
        ));
        assert_eq!(waited["satisfied"], true, "{waited}");
        let status = ok(send(&mut app, "logic.status", json!({ "entity": bits }), 0));
        assert_eq!(status["locals"]["n"], 1, "{status}");
        assert_eq!(status["rules"][0]["fires"], 1);

        let paused = ok(send(
            &mut app,
            "logic.control",
            json!({ "entity": bits, "op": "pause" }),
            0,
        ));
        assert_eq!(paused["paused"], true);
        ok(send(
            &mut app,
            "logic.emit",
            json!({ "entity": bits, "event": "ping" }),
            0,
        ));
        for _ in 0..5 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        let held = ok(send(&mut app, "logic.status", json!({ "entity": bits }), 0));
        assert_eq!(held["locals"]["n"], 1, "a paused graph runs nothing: {held}");
        assert_eq!(held["paused"], true);

        let resumed = ok(send(
            &mut app,
            "logic.control",
            json!({ "entity": bits, "op": "resume" }),
            0,
        ));
        assert_eq!(resumed["status"], "running");
        let caught_up = ok(send(
            &mut app,
            "play.wait_until",
            json!({
                "condition": { "entity": bits, "component": "LogicGraphStatus", "path": "rules[0].fires", "op": "gte", "value": 2 },
                "timeout_ms": 60_000
            }),
            8,
        ));
        assert_eq!(caught_up["satisfied"], true, "{caught_up}");
        let reloaded = ok(send(
            &mut app,
            "logic.reload",
            json!({ "entity": bits, "text": GRAPH }),
            0,
        ));
        assert_eq!(reloaded["outcome"], "kept");
        let reloads = app.world().resource::<LogicReloads>();
        assert_eq!(reloads.applied, 1);
        assert_eq!(
            reloads.log().last().map(|r| r.result.clone()),
            Some(Ok(LogicReloadOutcome::Kept))
        );
    }

    /// P4 through the dispatcher: `logic.debug` sets a rule breakpoint, the graph pauses at the end
    /// of the tick the rule fires in, `logic.control step` runs exactly one more tick, `resume`
    /// continues; a disabled rule never matches. Every documented reason answers.
    #[test]
    fn debug_breakpoint_step_and_resume_through_the_dispatcher() {
        let mut app = dogfood_app();
        let def = LogicGraphDef::from_ron(GRAPH).expect("parses");
        let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
        let e = app.world_mut().spawn((graph, LogicInbox::default())).id();
        let bits = e.to_bits();
        let set = ok(send(
            &mut app,
            "logic.debug",
            json!({ "entity": bits, "pause_on_rule": ["count"] }),
            0,
        ));
        assert_eq!(set["pause_on_rule"], json!(["count"]));
        assert_eq!(set["disabled"], json!([]));
        ok(send(
            &mut app,
            "logic.emit",
            json!({ "entity": bits, "event": "ping" }),
            0,
        ));
        app.world_mut().run_schedule(FixedUpdate);
        let hit = ok(send(&mut app, "logic.status", json!({ "entity": bits }), 0));
        assert_eq!(hit["paused"], true, "{hit}");
        assert_eq!(hit["locals"]["n"], 1);
        let ticks = hit["ticks"].as_u64().expect("ticks");
        for _ in 0..3 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        let held = ok(send(&mut app, "logic.status", json!({ "entity": bits }), 0));
        assert_eq!(held["ticks"].as_u64(), Some(ticks), "paused at the breakpoint");
        let stepped = ok(send(
            &mut app,
            "logic.control",
            json!({ "entity": bits, "op": "step" }),
            0,
        ));
        assert_eq!(stepped["paused"], true);
        app.world_mut().run_schedule(FixedUpdate);
        app.world_mut().run_schedule(FixedUpdate);
        let after = ok(send(&mut app, "logic.status", json!({ "entity": bits }), 0));
        assert_eq!(after["ticks"].as_u64(), Some(ticks + 1), "one step is one tick");
        assert_eq!(after["paused"], true);
        // Disable the rule, clear the breakpoint, resume: the event no longer counts.
        ok(send(
            &mut app,
            "logic.debug",
            json!({ "entity": bits, "pause_on_rule": [], "disabled": ["count"] }),
            0,
        ));
        ok(send(
            &mut app,
            "logic.control",
            json!({ "entity": bits, "op": "resume" }),
            0,
        ));
        ok(send(
            &mut app,
            "logic.emit",
            json!({ "entity": bits, "event": "ping" }),
            0,
        ));
        app.world_mut().run_schedule(FixedUpdate);
        let resumed = ok(send(&mut app, "logic.status", json!({ "entity": bits }), 0));
        assert_eq!(resumed["paused"], false);
        assert_eq!(
            resumed["locals"]["n"], 1,
            "a disabled rule never matches: {resumed}"
        );
        assert_eq!(resumed["ticks"].as_u64(), Some(ticks + 2));

        let err = |app: &mut App, params: Value| -> String {
            let response = send(app, "logic.debug", params, 0);
            response
                .error
                .and_then(|e| e.data)
                .and_then(|d| d.get("reason").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default()
        };
        assert_eq!(err(&mut app, json!({})), "missing_param");
        assert_eq!(
            err(&mut app, json!({ "entity": u64::MAX - 7 })),
            "entity_not_found"
        );
        let bare = app.world_mut().spawn_empty().id().to_bits();
        assert_eq!(err(&mut app, json!({ "entity": bare })), "no_logic_graph");
        assert_eq!(
            err(&mut app, json!({ "entity": bits, "pause_on_rule": ["ghost"] })),
            "rule_not_found"
        );
        assert_eq!(
            err(&mut app, json!({ "entity": bits, "disabled": "count" })),
            "bad_value"
        );
        assert_eq!(
            err(&mut app, json!({ "entity": bits, "pause_on_event": [""] })),
            "bad_value"
        );
    }

    // -- World blackboards (spec 102 § 13 P3.4) --------------------------------------------------

    fn rpc_reason(response: &Response) -> String {
        response
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn world_boards_through_the_dispatcher() {
        let mut app = dogfood_app();
        let watcher = r#"(format_version: 1, world: ["gatehouse"], locals: [(name: "n", default: Int(0))], rules: [
            (name: "alarm", event: BlackboardChanged(key: "world/gatehouse/alarm"), condition: Key("world/gatehouse/alarm"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })])])"#;
        let graph = compile_text(app.world(), watcher).expect("compiles");
        let e = app
            .world_mut()
            .spawn((graph, regia_ai::Blackboard::new(), LogicInbox::default()))
            .id();
        let bits = e.to_bits();
        // Before its first tick no board exists.
        assert_eq!(
            rpc_reason(&send(&mut app, "logic.world", json!({ "board": "gatehouse" }), 0)),
            "board_not_found"
        );
        app.world_mut().run_schedule(FixedUpdate);
        let listed = ok(send(&mut app, "logic.world", json!({}), 0));
        assert_eq!(
            listed["boards"],
            json!({ "gatehouse": {} }),
            "declared on the first tick"
        );
        let set = ok(send(
            &mut app,
            "logic.world_set",
            json!({ "board": "gatehouse", "key": "alarm", "value": true }),
            0,
        ));
        assert_eq!(set["writes"], 1);
        app.world_mut().run_schedule(FixedUpdate);
        let status = ok(send(&mut app, "logic.status", json!({ "entity": bits }), 0));
        assert_eq!(status["locals"]["n"], 1, "the graph saw the agent's write");
        assert_eq!(status["world"]["gatehouse/alarm"], true);
        let read = ok(send(&mut app, "logic.world", json!({ "board": "gatehouse" }), 0));
        assert_eq!(read["boards"]["gatehouse"]["alarm"], true);
        // Every value kind round-trips; each reason is reported.
        for value in [
            json!(3),
            json!(2.5),
            json!("text"),
            json!([1.0, 2.0, 3.0]),
            json!({ "entity": bits }),
        ] {
            let set = ok(send(
                &mut app,
                "logic.world_set",
                json!({ "board": "keep", "key": "v", "value": value.clone() }),
                0,
            ));
            assert_eq!(set["value"], value);
        }
        for (params, want) in [
            (json!({ "key": "x", "value": 1 }), "missing_param"),
            (json!({ "board": "b", "value": 1 }), "missing_param"),
            (json!({ "board": "b", "key": "x" }), "missing_param"),
            (json!({ "board": "a/b", "key": "x", "value": 1 }), "bad_value"),
            (json!({ "board": "b", "key": "", "value": 1 }), "bad_value"),
            (json!({ "board": "b", "key": "x", "value": null }), "bad_value"),
            (json!({ "board": "b", "key": "x", "value": [1, 2] }), "bad_value"),
        ] {
            assert_eq!(
                rpc_reason(&send(&mut app, "logic.world_set", params.clone(), 0)),
                want,
                "{params}"
            );
        }
    }
}
