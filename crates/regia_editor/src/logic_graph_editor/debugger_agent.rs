// SPDX-License-Identifier: MPL-2.0
//! The agent surface of the Logic Graph panel's debugger and authoring helpers (ADR-8021,
//! `book/agents/logic-graph.md`): `editor.logic_palette_search`, `editor.logic_variables`,
//! `editor.logic_variable_set`, `editor.logic_variable_remove`, `editor.logic_breakpoint_set`,
//! `editor.logic_breakpoint_list`, `editor.logic_watch`, `editor.logic_evaluate` and
//! `editor.logic_goto_node`. Each calls the same function as the panel control it mirrors. Errors are
//! `F0002` with `data.reason`.

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::logic_edit::NodeId;
use regia_ai::state_tree::Literal;
use regia_ecs::prelude::*;

use super::LogicEditor;
use super::breakpoints::{BreakpointChange, BreakpointKind};
use super::panel::WireFrom;
use super::variables::{VarScope, VarType, VariableEdit};
use super::watch::WatchItem;

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// Most hits `editor.logic_palette_search` returns by default.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;

const fn p(name: &'static str, kind: &'static str, required: bool, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const fn m(
    name: &'static str,
    tool: &'static str,
    description: &'static str,
    params: &'static [ParamSchema],
    result: &'static str,
    mutating: bool,
) -> MethodSchema {
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating,
        permission: if mutating {
            Permission::EditScene
        } else {
            Permission::Read
        },
    }
}

/// Every debugger method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.logic_palette_search",
                "logic_palette_search",
                "Fuzzy search over the Logic Graph palette (events, conditions, actions), best first, \
                 optionally filtered to what a wire dragged from node `from` can connect to (`side`: \
                 output, the default, or input). Each hit carries the op editor.logic_edit adds it with.",
                &const {
                    [
                        p(
                            "query",
                            "string",
                            true,
                            "The search text (empty lists everything).",
                        ),
                        p("from", "number", false, "The node the wire was dragged from."),
                        p(
                            "side",
                            "string",
                            false,
                            "output (the node's output pin) or input.",
                        ),
                        p("limit", "number", false, "Most hits (default 20)."),
                    ]
                },
                "{ hits: [{ group, label, ops }], total }",
                false,
            ),
            palette_search as Handler,
        ),
        (
            m(
                "editor.logic_variables",
                "logic_variables",
                "The open graph's Variables list: locals (type, default), declared world boards and the \
                 entity keys the graph uses, each with its use count.",
                &[],
                "{ variables: [{ scope, name, type, default, uses }] }",
                false,
            ),
            variables as Handler,
        ),
        (
            m(
                "editor.logic_variable_set",
                "logic_variable_set",
                "Declare a local, an entity variable or a world variable (`<board>/<key>`; .logic.ron v2) \
                 or change its type or default, declare a world board, or rename a local, board, world \
                 variable or entity key (`rename_to`; every reference in the graph is rewritten). One undo \
                 step, the Variables list's edit.",
                &const {
                    [
                        p("scope", "string", true, "local, world or entity."),
                        p(
                            "name",
                            "string",
                            true,
                            "The variable (board) name, without prefix.",
                        ),
                        p(
                            "type",
                            "string",
                            false,
                            "A local's, entity variable's or world variable's type: bool, int, float or string.",
                        ),
                        p(
                            "default",
                            "any",
                            false,
                            "A local's, entity variable's or world variable's default (a JSON bool, number or string).",
                        ),
                        p("rename_to", "string", false, "Rename it to this name."),
                    ]
                },
                "{ revision, variables }",
                true,
            ),
            variable_set as Handler,
        ),
        (
            m(
                "editor.logic_variable_remove",
                "logic_variable_remove",
                "Remove a local's, a world board's, an entity variable's or a world variable's declaration \
                 (one undo step; references stay and show as issues).",
                &const {
                    [
                        p("scope", "string", true, "local, world or entity."),
                        p(
                            "name",
                            "string",
                            true,
                            "The name (a world variable is `<board>/<key>`).",
                        ),
                    ]
                },
                "{ revision, variables }",
                true,
            ),
            variable_remove as Handler,
        ),
        (
            m(
                "editor.logic_breakpoint_set",
                "logic_breakpoint_set",
                "Set, enable, disable or remove a rule or event breakpoint of the open graph (saved per \
                 user in .regia/logic_breakpoints.ron, never in the asset; applied to running graphs of \
                 the file in development builds). A rule breakpoint pauses at the end of a tick the \
                 rule fires in, an event breakpoint at the end of the tick its inbox event arrives in.",
                &const {
                    [
                        p("kind", "string", true, "rule or event."),
                        p("name", "string", true, "The rule name or inbox event name."),
                        p(
                            "enabled",
                            "boolean",
                            false,
                            "Enable (default true) or disable it.",
                        ),
                        p("remove", "boolean", false, "Remove it."),
                        p(
                            "toggle",
                            "boolean",
                            false,
                            "Add it when missing, remove it when present (F9).",
                        ),
                    ]
                },
                "{ exists, breakpoints }",
                true,
            ),
            breakpoint_set as Handler,
        ),
        (
            m(
                "editor.logic_breakpoint_list",
                "logic_breakpoint_list",
                "The open graph's breakpoints (rule and event, enabled or not, with the node Go to \
                 selects), the breakpoints that paused the watched graph on its newest tick, the \
                 breakpoint file and whether this build applies them.",
                &[],
                "{ graph, file, applies, rules, events, hits, step_to_fire }",
                false,
            ),
            breakpoint_list as Handler,
        ),
        (
            m(
                "editor.logic_watch",
                "logic_watch",
                "The Watch list: pin or unpin variables (`add`/`remove`: local/x, world/<board>/x, a \
                 bare entity key) and condition results (`add_condition`/`remove_condition`: a node), \
                 and turn Live on (refresh every frame while playing) or off (only when the graph \
                 pauses). Answers each entry's value at the latest snapshot and the snapshot cost.",
                &const {
                    [
                        p("add", "string", false, "A variable key to pin."),
                        p("remove", "string", false, "A variable key to unpin."),
                        p("add_condition", "number", false, "A condition node to pin."),
                        p("remove_condition", "number", false, "A condition node to unpin."),
                        p("live", "boolean", false, "Refresh every frame while playing."),
                    ]
                },
                "{ live, items: [{ label, key?, node?, value?, error? }], snapshot, refreshes }",
                false,
            ),
            watch as Handler,
        ),
        (
            m(
                "editor.logic_evaluate",
                "logic_evaluate",
                "While the watched graph is paused, evaluate a condition in the graph's own syntax (a \
                 Condition in RON, e.g. Compare(key: \"local/n\", op: Gt, value: 0.0)) over the paused \
                 instance's local, entity and world variables, by the runtime's scope rule.",
                &const { [p("expression", "string", true, "The condition (RON).")] },
                "{ value, entity, tick }",
                false,
            ),
            evaluate as Handler,
        ),
        (
            m(
                "editor.logic_goto_node",
                "logic_goto_node",
                "Open the Logic Graph panel at a node of the open graph (select it and frame it): what a \
                 Problems row's link and the Breakpoints list's Go to do.",
                &const { [p("node", "number", true, "The node id.")] },
                "{ selected }",
                false,
            ),
            goto_node as Handler,
        ),
    ]
}

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn require_open(world: &World) -> Result<(), RpcError> {
    if world
        .get_resource::<LogicEditor>()
        .is_some_and(LogicEditor::is_open)
    {
        Ok(())
    } else {
        Err(refused(
            "no_graph_open",
            "no logic graph is open",
            "call editor.logic_open { path } or { create: true } first",
        ))
    }
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params.get(name).and_then(Value::as_str).ok_or_else(|| {
        refused(
            "missing_param",
            format!("`{name}` (a string) is required"),
            "see book/agents/logic-graph.md",
        )
    })
}

fn node_param(params: &Value, name: &str) -> Result<Option<NodeId>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| NodeId::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                refused(
                    "bad_value",
                    format!("`{name}` must be a node id"),
                    "see editor.logic_state",
                )
            }),
    }
}

fn palette_search(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let query = str_param(params, "query")?;
    let from = node_param(params, "from")?;
    let doc = world.resource::<LogicEditor>().doc.clone();
    let from = match from {
        None => None,
        Some(id) => {
            if !doc.nodes.contains_key(&id) {
                return Err(refused(
                    "unknown_node",
                    format!("no node {id}"),
                    "see editor.logic_state nodes",
                ));
            }
            Some(
                match params.get("side").and_then(Value::as_str).unwrap_or("output") {
                    "output" => WireFrom::Output(id),
                    "input" => WireFrom::Input(id),
                    other => {
                        return Err(refused(
                            "bad_value",
                            format!("`side` must be output or input, not {other:?}"),
                            "see book/agents/logic-graph.md",
                        ));
                    }
                },
            )
        }
    };
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_SEARCH_LIMIT, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let tasks = super::palette_tasks(world);
    let hits = super::panel::palette_search(&doc, &tasks, query, from);
    let total = hits.len();
    let hits: Vec<Value> = hits
        .into_iter()
        .take(limit)
        .map(|(group, label, op)| {
            let op = match from {
                Some(from) => json!(super::panel::add_wired_ops(&doc, &op, [0.0, 0.0], Some(from))),
                None => json!([op]),
            };
            json!({ "group": group, "label": label, "ops": op })
        })
        .collect();
    Ok(json!({ "hits": hits, "total": total }))
}

fn variables_json(world: &World) -> Value {
    let doc = &world.resource::<LogicEditor>().doc;
    json!(
        super::variables::rows(doc)
            .iter()
            .map(|r| json!({
                "scope": r.scope.name(),
                "name": r.name,
                "type": r.default.as_ref().map(|d| VarType::of(d).name()),
                "default": r.default.as_ref().map(|d| match d {
                    Literal::Bool(b) => json!(b),
                    Literal::Int(i) => json!(i),
                    Literal::Float(f) => json!(f),
                    Literal::String(s) => json!(s),
                }),
                "uses": r.uses,
            }))
            .collect::<Vec<_>>()
    )
}

fn variables(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    Ok(json!({ "variables": variables_json(world) }))
}

fn scope_param(params: &Value) -> Result<VarScope, RpcError> {
    let s = str_param(params, "scope")?;
    VarScope::parse(s).ok_or_else(|| {
        refused(
            "bad_scope",
            format!("`scope` must be local, world or entity, not {s:?}"),
            "see book/agents/logic-graph.md",
        )
    })
}

fn apply_variable(world: &mut World, edit: &VariableEdit) -> Result<Value, RpcError> {
    super::variables::apply(world, edit).map_err(|(code, message)| {
        refused(code, message, "nothing was changed; see editor.logic_variables")
    })?;
    let revision = world.resource::<LogicEditor>().revision;
    Ok(json!({ "revision": revision, "variables": variables_json(world) }))
}

fn variable_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let scope = scope_param(params)?;
    let name = str_param(params, "name")?.to_string();
    if let Some(to) = params.get("rename_to").and_then(Value::as_str) {
        return apply_variable(
            world,
            &VariableEdit::Rename {
                scope,
                from: name,
                to: to.to_string(),
            },
        );
    }
    let ty = match params.get("type").and_then(Value::as_str) {
        None => None,
        Some(t) => Some(VarType::parse(t).ok_or_else(|| {
            refused(
                "bad_value",
                format!("`type` must be bool, int, float or string, not {t:?}"),
                "see book/agents/logic-graph.md",
            )
        })?),
    };
    let default = match params.get("default") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(Literal::Bool(*b)),
        Some(Value::String(s)) => Some(Literal::String(s.clone())),
        Some(Value::Number(n)) => Some(match (n.as_i64(), ty) {
            (Some(i), None | Some(VarType::Int)) => Literal::Int(i),
            _ => Literal::Float(n.as_f64().unwrap_or(0.0) as f32),
        }),
        Some(_) => {
            return Err(refused(
                "bad_value",
                "`default` must be a bool, a number or a string",
                "see book/agents/logic-graph.md",
            ));
        }
    };
    apply_variable(
        world,
        &VariableEdit::Set {
            scope,
            name,
            ty,
            default,
        },
    )
}

fn variable_remove(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let scope = scope_param(params)?;
    let name = str_param(params, "name")?.to_string();
    apply_variable(world, &VariableEdit::Remove { scope, name })
}

fn kind_param(params: &Value) -> Result<BreakpointKind, RpcError> {
    match str_param(params, "kind")? {
        "rule" => Ok(BreakpointKind::Rule),
        "event" => Ok(BreakpointKind::Event),
        other => Err(refused(
            "bad_value",
            format!("`kind` must be rule or event, not {other:?}"),
            "see book/agents/logic-graph.md",
        )),
    }
}

fn breakpoint_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let kind = kind_param(params)?;
    let name = str_param(params, "name")?.to_string();
    let flag = |key: &str| params.get(key).and_then(Value::as_bool);
    let change = if flag("remove") == Some(true) {
        BreakpointChange::Remove
    } else if flag("toggle") == Some(true) {
        BreakpointChange::Toggle
    } else {
        BreakpointChange::Enable(flag("enabled").unwrap_or(true))
    };
    let exists = super::set_breakpoint(world, kind, &name, change)
        .map_err(|e| refused(e.code, e.message, "see editor.logic_breakpoint_list"))?;
    let list = breakpoint_list(world, &json!({}))?;
    Ok(json!({ "exists": exists, "breakpoints": list }))
}

fn breakpoint_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let root = crate::agent_ext::agent_root(world);
    let mut editor = world.resource_mut::<LogicEditor>();
    editor.breakpoints.ensure_loaded(&root);
    let key = super::breakpoint_key(&editor, &root);
    let set = key
        .as_deref()
        .map(|k| editor.breakpoints.of(k))
        .unwrap_or_default();
    let doc = &editor.doc;
    let rules: Vec<Value> = set
        .rules
        .iter()
        .map(|b| json!({ "name": b.name, "enabled": b.enabled, "node": doc.find_rule(&b.name) }))
        .collect();
    let events: Vec<Value> = set
        .events
        .iter()
        .map(|b| {
            let node = doc.rules.iter().copied().find(|id| {
                matches!(
                    doc.nodes.get(id).map(|n| &n.kind),
                    Some(regia_ai::logic_edit::LogicNodeKind::Event { event, .. })
                        if super::breakpoints::event_breakpoint_name(event).as_deref() == Some(b.name.as_str())
                )
            });
            json!({ "name": b.name, "enabled": b.enabled, "node": node })
        })
        .collect();
    Ok(json!({
        "graph": key,
        "file": root.join(super::breakpoints::BREAKPOINTS_FILE).display().to_string(),
        "applies": cfg!(debug_assertions),
        "rules": rules,
        "events": events,
        "hits": editor.breakpoints.hits,
        "step_to_fire": editor.step_fire,
        "error": editor.breakpoints.error,
    }))
}

fn watch(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let add_condition = node_param(params, "add_condition")?;
    let remove_condition = node_param(params, "remove_condition")?;
    let mut editor = world.resource_mut::<LogicEditor>();
    let editor = &mut *editor;
    if let Some(id) = add_condition
        && !matches!(
            editor.doc.nodes.get(&id).map(|n| &n.kind),
            Some(regia_ai::logic_edit::LogicNodeKind::Condition { .. })
        )
    {
        return Err(refused(
            "not_a_condition",
            format!("node {id} is not a condition node"),
            "see editor.logic_state nodes",
        ));
    }
    let mut adds = Vec::new();
    if let Some(key) = params.get("add").and_then(Value::as_str) {
        if key.trim().is_empty() {
            return Err(refused("bad_value", "`add` is empty", "pass a key like local/n"));
        }
        adds.push(WatchItem::Key(key.trim().to_string()));
    }
    adds.extend(add_condition.map(WatchItem::Condition));
    for item in adds {
        editor
            .watches
            .add(item)
            .map_err(|e| refused("too_many_watches", e, "unpin an entry first"))?;
    }
    if let Some(key) = params.get("remove").and_then(Value::as_str) {
        editor.watches.remove(&WatchItem::Key(key.trim().to_string()));
    }
    if let Some(id) = remove_condition {
        editor.watches.remove(&WatchItem::Condition(id));
    }
    if let Some(live) = params.get("live").and_then(Value::as_bool) {
        editor.watches.live = live;
    }
    editor.watches.recompute(&editor.doc);
    let w = &editor.watches;
    let items: Vec<Value> = w
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let mut v = json!({ "label": item.label(&editor.doc) });
            match item {
                WatchItem::Key(k) => v["key"] = json!(k),
                WatchItem::Condition(id) => v["node"] = json!(id),
            }
            match w.values.get(i) {
                Some(Ok(value)) => v["value"] = json!(value),
                Some(Err(e)) => v["error"] = json!(e),
                None => {}
            }
            v
        })
        .collect();
    Ok(json!({
        "live": w.live,
        "items": items,
        "snapshot": w.snapshot.as_ref().map(|s| json!({ "entity": s.entity, "tick": s.ticks, "paused": s.paused })),
        "refreshes": w.refreshes,
    }))
}

fn evaluate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let text = str_param(params, "expression")?.to_string();
    let mut editor = world.resource_mut::<LogicEditor>();
    let result = super::watch::evaluate(editor.watches.snapshot.as_ref(), &text);
    editor.watches.evaluation = Some((text, result.clone()));
    let value = result.map_err(|e| {
        let (code, message) = e.split_once(": ").unwrap_or(("bad_value", e.as_str()));
        // The runtime's compile codes are already lowercase ascii (`unknown_scope`, ...).
        let code = match code {
            "not_paused" | "parse" | "unknown_scope" | "condition_too_deep" | "bad_value" => code,
            _ => "bad_value",
        };
        refused(
            code,
            message,
            "pause the watched graph; the expression is a Condition in RON",
        )
    })?;
    let snapshot = editor.watches.snapshot.as_ref();
    Ok(json!({
        "value": value,
        "entity": snapshot.map(|s| s.entity),
        "tick": snapshot.map(|s| s.ticks),
    }))
}

fn goto_node(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let node = node_param(params, "node")?.ok_or_else(|| {
        refused(
            "missing_param",
            "`node` is required",
            "see editor.logic_state nodes",
        )
    })?;
    super::problems::go_to_node(world, node)
        .map_err(|(code, message)| refused(code, message, "see editor.logic_state nodes"))?;
    Ok(json!({ "selected": node }))
}
