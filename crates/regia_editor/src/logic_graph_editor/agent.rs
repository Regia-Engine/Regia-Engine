// SPDX-License-Identifier: MPL-2.0
//! The Logic Graph panel's agent-protocol surface (spec 102 § 10, `book/agents/logic-graph.md`):
//! `editor.logic_open`, `editor.logic_state`, `editor.logic_edit`, `editor.logic_save` and
//! `editor.logic_validate` open, inspect, edit and save the graph the panel shows.
//!
//! Every mutating method goes through [`super::edit`], so an agent edit lands on the editor's undo
//! stack exactly like a panel gesture (`editor.undo` reverts it). Operations are
//! `regia_ai::logic_edit::LogicOp`. Errors are `F0002` with `data.reason`.

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::logic_edit::{self, LogicGraphDoc, LogicIssue, LogicOp, LogicSeverity};
use regia_ecs::prelude::*;

use super::{LogicEditor, diagnose, edit, palette_tasks, save};
use crate::state_tree_editor::resolve_path;

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// Most operations one `editor.logic_edit` call applies.
pub const MAX_OPS: usize = 1024;

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

/// Every Logic Graph editor method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.logic_open",
                "logic_open",
                "Open a .logic.ron (and its .logic.layout.ron sidecar) in the Logic Graph panel, or start a new, \
                 empty graph (create: true).",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            false,
                            "Project-relative or absolute .logic.ron path.",
                        ),
                        p(
                            "create",
                            "boolean",
                            false,
                            "Start a new graph (saved to `path` later).",
                        ),
                    ]
                },
                "{ path, rules, nodes, valid, revision, discarded_unsaved }",
                true,
            ),
            logic_open as Handler,
        ),
        (
            m(
                "editor.logic_state",
                "logic_state",
                "The open graph: nodes (id, kind, fields, position), wires, rules in declaration order with \
                 their chains, locals, validation issues with codes and nodes, the selection, and while the \
                 game plays the graph entities, the watched graph's live activity and variables and what \
                 the last save did to running graphs. Optionally the palette and the .logic.ron text.",
                &const {
                    [
                        p(
                            "palette",
                            "boolean",
                            false,
                            "Include the event arms, condition kinds and action tasks.",
                        ),
                        p(
                            "ron",
                            "boolean",
                            false,
                            "Include the .logic.ron text it would save.",
                        ),
                    ]
                },
                "{ path, dirty, revision, description, seed, locals, nodes, wires, rules, valid, issues, \
                 selected, agents, watch, live, reload, saves, next_id, palette?, ron? }",
                false,
            ),
            logic_state as Handler,
        ),
        (
            m(
                "editor.logic_edit",
                "logic_edit",
                "One undoable edit of the open graph: `ops` (add_event, add_condition, add_action, \
                 remove_node, move_node, connect, disconnect, set_event, set_condition, \
                 set_condition_mode, set_task, set_binding, set_local, set_graph, move_rule), applied all or \
                 nothing, the same operations the panel's gestures make. `select` picks a node, `watch` \
                 the play-world graph to show live, `control` pauses, resumes or steps the watched graph.",
                &const {
                    [
                        p(
                            "ops",
                            "array",
                            false,
                            "The operations, applied in order (at most 1024).",
                        ),
                        p(
                            "select",
                            "number",
                            false,
                            "Select this node id (0 clears the selection).",
                        ),
                        p(
                            "watch",
                            "number",
                            false,
                            "Show this play-world graph live (entity bits; 0 picks automatically).",
                        ),
                        p(
                            "control",
                            "string",
                            false,
                            "pause, resume, step or step_to_fire (run to the next tick a rule fires in) the \
                             watched play-world graph, as the live view's buttons do (applied at the next pump; \
                             not an undo step).",
                        ),
                        p("label", "string", false, "The undo entry's label."),
                    ]
                },
                "{ revision, created, valid, issues, selected, watch }",
                true,
            ),
            logic_edit_method as Handler,
        ),
        (
            m(
                "editor.logic_save",
                "logic_save",
                "Save the open graph as canonical .logic.ron (to `path`, or where it was opened) and its \
                 layout sidecar, atomically. A graph with errors is refused (invalid_graph) and nothing is \
                 written. While the game plays, graphs that ran the file reload it (kept or restarted, \
                 shown by editor.logic_state reload).",
                &const { [p("path", "string", false, "Save as this path.")] },
                "{ path, layout, valid, saves, playing }",
                true,
            ),
            logic_save as Handler,
        ),
        (
            m(
                "editor.logic_validate",
                "logic_validate",
                "Validate a graph without opening it: `text` (.logic.ron), `path`, or with neither the open \
                 graph. Issues carry the codes of spec 102 § 6 (dangling_wire, duplicate_name, \
                 unknown_task, unknown_scope, condition_too_deep, too_many_rules, too_many_actions, \
                 bad_value, ...) and the node they are about.",
                &const {
                    [
                        p("text", "string", false, ".logic.ron text to validate."),
                        p("path", "string", false, "A .logic.ron file to validate."),
                    ]
                },
                "{ valid, issues, rules }",
                false,
            ),
            logic_validate as Handler,
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

/// The issues as the protocol shows them.
fn issues_list(issues: &[LogicIssue]) -> Value {
    json!(
        issues
            .iter()
            .map(|i| json!({
                "code": i.code,
                "severity": if i.severity == LogicSeverity::Error { "error" } else { "warning" },
                "node": i.node,
                "rule": i.rule,
                "message": i.message,
            }))
            .collect::<Vec<_>>()
    )
}

/// The nodes, wires and rule chains of a document.
fn doc_json(doc: &LogicGraphDoc) -> (Value, Value, Value) {
    let nodes: Vec<Value> = doc
        .nodes
        .iter()
        .map(|(id, n)| {
            let mut v = serde_json::to_value(n).unwrap_or(Value::Null);
            if let Some(map) = v.as_object_mut() {
                map.insert("id".into(), json!(id));
            }
            v
        })
        .collect();
    let wires: Vec<Value> = doc
        .wires
        .iter()
        .map(|(from, to)| json!({ "from": from, "to": to }))
        .collect();
    let rules: Vec<Value> = doc
        .chains()
        .into_iter()
        .map(|c| {
            json!({
                "name": doc.rule_name(c.event),
                "event": c.event,
                "conditions": c.conditions,
                "actions": c.actions,
            })
        })
        .collect();
    (json!(nodes), json!(wires), json!(rules))
}

/// A `.logic.ron` path confined to the agent's allowed roots (ADR-1816).
fn request_path(world: &World, path: &str) -> Result<std::path::PathBuf, RpcError> {
    if !path.ends_with(".logic.ron") {
        return Err(refused(
            "bad_path",
            "`path` must name a `.logic.ron` file",
            "use a path like assets/logic/gatehouse.logic.ron",
        ));
    }
    let resolved = resolve_path(world, path);
    crate::agent_ext::allowed_request_path(world, "path", &resolved.to_string_lossy())
}

fn logic_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(|p| request_path(world, p))
        .transpose()?;
    let mut editor = world.get_resource_or_insert_with(LogicEditor::default);
    let unsaved = editor.has_unsaved_work();
    if create {
        editor.new_graph(path);
    } else {
        let path = path.ok_or_else(|| {
            refused(
                "missing_param",
                "`path` is required unless `create: true`",
                "pass a .logic.ron path",
            )
        })?;
        if !path.is_file() {
            return Err(RpcError::not_found(
                format!("no file {}", path.display()),
                "check the path (project-relative or absolute)",
            ));
        }
        editor
            .open(&path)
            .map_err(|e| refused("open_failed", e, "the file is not a readable .logic.ron"))?;
    }
    let rules = editor.doc.rules.len();
    let nodes = editor.doc.nodes.len();
    let revision = editor.revision;
    let shown = editor.path.as_ref().map(|p| p.display().to_string());
    let valid = logic_edit::is_valid(&diagnose(world));
    Ok(json!({
        "path": shown,
        "rules": rules,
        "nodes": nodes,
        "valid": valid,
        "revision": revision,
        "discarded_unsaved": unsaved,
    }))
}

fn live_json(editor: &LogicEditor) -> Value {
    let Some(l) = editor.live.as_ref() else {
        return Value::Null;
    };
    let pairs = |v: &[(String, String)]| -> Value {
        json!(
            v.iter()
                .map(|(k, v)| json!({ "key": k, "value": v }))
                .collect::<Vec<_>>()
        )
    };
    json!({
        "entity": l.entity,
        "running": l.running,
        "ticks": l.ticks,
        "activity_tick": l.activity_tick,
        "events": l.events,
        "activity": l.activity.iter().map(|(k, v)| (k.clone(), json!(v.name()))).collect::<serde_json::Map<_, _>>(),
        "actions_started": l.actions_started,
        "fires": l.fires,
        "locals": pairs(&l.locals),
        "entity_vars": pairs(&l.entity_vars),
        "world_vars": pairs(&l.world_vars),
        "matches_document": l.matches_document,
    })
}

fn logic_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let issues = diagnose(world);
    let editor = world.resource::<LogicEditor>();
    let (nodes, wires, rules) = doc_json(&editor.doc);
    let mut out = json!({
        "path": editor.path.as_ref().map(|p| p.display().to_string()),
        "dirty": editor.dirty(),
        "revision": editor.revision,
        "description": editor.doc.description,
        "seed": editor.doc.seed,
        "locals": serde_json::to_value(&editor.doc.locals).unwrap_or(Value::Null),
        "nodes": nodes,
        "wires": wires,
        "rules": rules,
        "next_id": editor.doc.next_id,
        "valid": logic_edit::is_valid(&issues),
        "issues": issues_list(&issues),
        "selected": editor.selected,
        "agents": editor.agents,
        "watch": editor.watch,
        "live": live_json(editor),
        "reload": editor.reload.as_ref().map(|r| json!({
            "save": r.save,
            "queued": r.queued,
            "kept": r.kept,
            "restarted": r.restarted,
            "rejected": r.rejected,
            "reason": r.reason,
        })),
        "saves": editor.saves,
    });
    if params.get("palette").and_then(Value::as_bool).unwrap_or(false) {
        out["palette"] = json!({
            "events": ["Start", "Timer", "BlackboardChanged", "EntityEvent"],
            "conditions": ["Always", "Key", "Has", "Compare", "Text", "All", "Any", "Not"],
            "modes": ["leaf", "all", "any", "not"],
            "tasks": palette_tasks(world),
            "literals": ["Bool", "Int", "Float", "String"],
        });
    }
    if params.get("ron").and_then(Value::as_bool).unwrap_or(false) {
        out["ron"] = json!(world.resource::<LogicEditor>().doc.to_ron().unwrap_or_default());
    }
    Ok(out)
}

/// Parses `params.ops`.
fn parse_ops(params: &Value) -> Result<Vec<LogicOp>, RpcError> {
    let Some(list) = params.get("ops") else {
        return Ok(Vec::new());
    };
    let list = list.as_array().ok_or_else(|| {
        refused(
            "bad_ops",
            "`ops` must be an array",
            "see book/agents/logic-graph.md",
        )
    })?;
    if list.len() > MAX_OPS {
        return Err(refused(
            "too_many_ops",
            format!("at most {MAX_OPS} operations"),
            "split the edit",
        ));
    }
    list.iter()
        .enumerate()
        .map(|(i, op)| {
            serde_json::from_value::<LogicOp>(op.clone()).map_err(|e| {
                refused(
                    "bad_ops",
                    format!("op {i}: {e}"),
                    "see book/agents/logic-graph.md",
                )
            })
        })
        .collect()
}

fn logic_edit_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let ops = parse_ops(params)?;
    let select = params.get("select").and_then(Value::as_u64);
    let watch = params.get("watch").and_then(Value::as_u64);
    // The live view's Pause / Resume / Step buttons (`PanelEdit::Control`, ADR-7774).
    let control = match params.get("control") {
        None | Some(Value::Null) => None,
        Some(value) => Some(match value.as_str() {
            Some("pause") => super::LiveControl::Pause,
            Some("resume") => super::LiveControl::Resume,
            Some("step") => super::LiveControl::Step,
            Some("step_to_fire") => super::LiveControl::StepToFire,
            _ => {
                return Err(refused(
                    "bad_control",
                    format!("`control` must be pause, resume, step or step_to_fire, not {value}"),
                    "see book/agents/logic-graph.md",
                ));
            }
        }),
    };
    if ops.is_empty() && select.is_none() && watch.is_none() && control.is_none() {
        return Err(refused(
            "missing_param",
            "pass `ops`, `select`, `watch` or `control`",
            "see book/agents/logic-graph.md",
        ));
    }
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("edit")
        .to_string();
    let created = if ops.is_empty() {
        Vec::new()
    } else {
        edit(world, &label, None, &ops).map_err(|e| {
            refused(
                e.error.code,
                format!("op {}: {}", e.op_index, e.error.message),
                "nothing was changed; fix the op and call again",
            )
        })?
    };
    let mut editor = world.resource_mut::<LogicEditor>();
    if let Some(id) = select {
        if id != 0 && !u32::try_from(id).is_ok_and(|id| editor.doc.nodes.contains_key(&id)) {
            return Err(refused(
                "unknown_node",
                format!("no node {id}"),
                "see editor.logic_state nodes",
            ));
        }
        editor.selected = u32::try_from(id).ok().filter(|id| *id != 0);
    }
    if let Some(bits) = watch {
        editor.watch = (bits != 0).then_some(bits);
    }
    if control.is_some() {
        editor.pending_control = control;
    }
    let revision = editor.revision;
    let selected = editor.selected;
    let watching = editor.watch;
    let issues = diagnose(world);
    Ok(json!({
        "revision": revision,
        "created": created,
        "valid": logic_edit::is_valid(&issues),
        "issues": issues_list(&issues),
        "selected": selected,
        "watch": watching,
    }))
}

fn logic_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(|p| request_path(world, p))
        .transpose()?;
    let saved = save(world, path.as_deref()).map_err(|e| {
        let mut error = refused(
            e.code,
            e.message,
            "fix the issues editor.logic_state lists, or pass a .logic.ron path that can be written",
        );
        if e.code == "invalid_graph"
            && let Some(Value::Object(data)) = error.data.as_mut()
        {
            data.insert("issues".into(), issues_list(&diagnose(world)));
        }
        error
    })?;
    let playing = crate::gameplay_panel::play_running(world);
    let saves = world.resource::<LogicEditor>().saves;
    Ok(json!({
        "path": saved.display().to_string(),
        "layout": logic_edit::layout_path(&saved).display().to_string(),
        "valid": true,
        "saves": saves,
        "playing": playing,
    }))
}

fn logic_validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params.get("text").and_then(Value::as_str);
    let path = params.get("path").and_then(Value::as_str);
    let doc = match (text, path) {
        (Some(_), Some(_)) => {
            return Err(refused(
                "bad_params",
                "pass `text` or `path`, not both",
                "see book/agents/logic-graph.md",
            ));
        }
        (Some(text), None) => Some(text.to_string()),
        (None, Some(path)) => {
            let path = request_path(world, path)?;
            Some(std::fs::read_to_string(&path).map_err(|e| {
                RpcError::not_found(format!("cannot read {}: {e}", path.display()), "check the path")
            })?)
        }
        (None, None) => None,
    };
    let issues = match doc {
        Some(text) => {
            let doc = LogicGraphDoc::from_ron(&text, &Default::default()).map_err(|e| {
                refused(
                    e.reason(),
                    e.to_string(),
                    "the text is not a version-1 .logic.ron",
                )
            })?;
            let registry = world
                .get_resource::<regia_ai::BtTaskRegistry>()
                .filter(|r| !r.is_empty());
            let issues = logic_edit::validate(&doc, registry);
            (issues, doc.rules.len())
        }
        None => {
            require_open(world)?;
            (diagnose(world), world.resource::<LogicEditor>().doc.rules.len())
        }
    };
    Ok(json!({
        "valid": logic_edit::is_valid(&issues.0),
        "issues": issues_list(&issues.0),
        "rules": issues.1,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(s, _)| s.name == method)
            .unwrap_or_else(|| panic!("no method {method}"));
        handler(world, &params)
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn method_names_are_unique_and_namespaced() {
        let names: Vec<&str> = methods().iter().map(|(s, _)| s.name).collect();
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        assert!(names.iter().all(|n| n.starts_with("editor.logic_")));
    }

    #[test]
    fn calls_before_open_say_how_to_open() {
        let mut world = World::new();
        for method in [
            "editor.logic_state",
            "editor.logic_edit",
            "editor.logic_save",
            "editor.logic_validate",
        ] {
            let err = call(&mut world, method, json!({})).expect_err("error");
            assert_eq!(reason(&err), "no_graph_open", "{method}");
        }
    }

    #[test]
    fn errors_carry_their_codes_and_an_invalid_graph_is_not_saved() {
        let dir = regia_test_utils::unique_temp_dir("logic-editor-agent-errors");
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let path = dir.join("gate.logic.ron");
        let err = call(&mut world, "editor.logic_open", json!({})).expect_err("no path");
        assert_eq!(reason(&err), "missing_param");
        let err = call(&mut world, "editor.logic_open", json!({ "path": "x.bt.ron" })).expect_err("bad path");
        assert_eq!(reason(&err), "bad_path");
        call(
            &mut world,
            "editor.logic_open",
            json!({ "create": true, "path": path.display().to_string() }),
        )
        .expect("open");
        let err = call(&mut world, "editor.logic_edit", json!({})).expect_err("nothing to do");
        assert_eq!(reason(&err), "missing_param");
        let err = call(
            &mut world,
            "editor.logic_edit",
            json!({ "ops": [{ "op": "jump" }] }),
        )
        .expect_err("bad op");
        assert_eq!(reason(&err), "bad_ops");
        let err = call(&mut world, "editor.logic_edit", json!({ "ops": "x" })).expect_err("bad ops");
        assert_eq!(reason(&err), "bad_ops");
        let many: Vec<Value> = (0..=MAX_OPS)
            .map(|_| json!({ "op": "set_graph", "seed": 1 }))
            .collect();
        let err = call(&mut world, "editor.logic_edit", json!({ "ops": many })).expect_err("too many");
        assert_eq!(reason(&err), "too_many_ops");
        let out = call(
            &mut world,
            "editor.logic_edit",
            json!({ "ops": [
                { "op": "add_event", "rule": "go", "event": { "EntityEvent": "go" }, "id": 1 },
                { "op": "add_condition", "condition": "Always", "id": 2 },
                { "op": "add_action", "task": "logic::EmitEvent", "set": { "event": { "Literal": { "String": "x" } } }, "id": 3 },
                { "op": "connect", "from": 1, "to": 3 },
            ] }),
        )
        .expect("applies");
        assert_eq!(out["created"], json!([1, 2, 3]));
        assert_eq!(out["valid"], false, "{out}");
        assert_eq!(out["issues"][0]["code"], "dangling_wire");
        assert_eq!(out["issues"][0]["node"], 2);
        for (op, code) in [
            (json!({ "op": "connect", "from": 3, "to": 2 }), "pin_type"),
            (json!({ "op": "connect", "from": 2, "to": 1 }), "pin_type"),
            (json!({ "op": "connect", "from": 1, "to": 2 }), "pin_busy"),
            (json!({ "op": "connect", "from": 3, "to": 3 }), "cycle"),
            (json!({ "op": "disconnect", "from": 2, "to": 3 }), "no_wire"),
            (json!({ "op": "remove_node", "node": 99 }), "unknown_node"),
            (json!({ "op": "set_task", "node": 2, "task": "x" }), "wrong_kind"),
            (
                json!({ "op": "add_action", "task": "x", "id": 1 }),
                "duplicate_id",
            ),
        ] {
            let err = call(&mut world, "editor.logic_edit", json!({ "ops": [op] })).expect_err("refused");
            assert_eq!(reason(&err), code);
        }
        let err = call(&mut world, "editor.logic_edit", json!({ "select": 77 })).expect_err("unknown");
        assert_eq!(reason(&err), "unknown_node");
        let err = call(&mut world, "editor.logic_save", json!({})).expect_err("invalid");
        assert_eq!(reason(&err), "invalid_graph");
        assert_eq!(
            err.data.as_ref().map(|d| d["issues"][0]["code"].clone()),
            Some(json!("dangling_wire"))
        );
        assert!(!path.exists(), "nothing was written");
        let err = call(&mut world, "editor.logic_save", json!({ "path": "x.ron" })).expect_err("bad path");
        assert_eq!(reason(&err), "bad_path");
        // Removing the stray node makes it save.
        call(
            &mut world,
            "editor.logic_edit",
            json!({ "ops": [{ "op": "remove_node", "node": 2 }] }),
        )
        .expect("remove");
        let saved = call(&mut world, "editor.logic_save", json!({})).expect("saves");
        assert_eq!(saved["saves"], 1);
        assert!(path.exists() && logic_edit::layout_path(&path).exists());
        // Validate: text, path, and both.
        let err = call(
            &mut world,
            "editor.logic_validate",
            json!({ "text": "x", "path": "y.logic.ron" }),
        )
        .expect_err("both");
        assert_eq!(reason(&err), "bad_params");
        let err = call(&mut world, "editor.logic_validate", json!({ "text": "(" })).expect_err("parse");
        assert_eq!(reason(&err), "parse");
        let err = call(
            &mut world,
            "editor.logic_validate",
            json!({ "text": "(format_version: 9)" }),
        )
        .expect_err("version");
        assert_eq!(reason(&err), "unsupported_version");
        let by_path = call(
            &mut world,
            "editor.logic_validate",
            json!({ "path": path.display().to_string() }),
        )
        .expect("validate path");
        assert_eq!(
            (by_path["valid"].clone(), by_path["rules"].clone()),
            (json!(true), json!(1))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
