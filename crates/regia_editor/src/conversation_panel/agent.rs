// SPDX-License-Identifier: MPL-2.0
//! The conversation editor's agent-protocol surface: `editor.conversation_*` methods that open,
//! inspect, edit, save and preview the conversation the panel shows
//! (`book/agents/conversation-editor.md`).
//!
//! Every mutating method goes through [`super::edit()`] or [`super::move_node`], so it lands on the
//! editor's undo stack exactly like a panel edit (`editor.undo` reverts it). Errors are `F0002`
//! with `data.reason`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_framework::conversation::Step;

use super::edit::ConvOp;
use super::graph::ConvGraph;
use super::preview::PreviewState;
use super::{ConvEditor, EditError, edit, move_node};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// The most ops one `editor.conversation_edit` call may carry.
pub const MAX_OPS: usize = 512;

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

/// Every conversation-editor method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.conversation_open",
                "conversation_open",
                "Open a .conv.ron in the conversation graph editor. A file that does not parse opens \
                 read-only with the parse error (path:line:col).",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            true,
                            "Project-relative or absolute .conv.ron path.",
                        ),
                        p(
                            "project",
                            "string",
                            false,
                            "The project root (default: the editor's project, else the directory above assets/).",
                        ),
                    ]
                },
                "{ path, id, read_only, parse_error, nodes, edges, issues, revision, discarded_unsaved }",
                true,
            ),
            conversation_open as Handler,
        ),
        (
            m(
                "editor.conversation_graph",
                "conversation_graph",
                "The open conversation as a graph: nodes (id, kind, steps, label, position, findings) and \
                 edges (from, to, kind, label, option, cond), the findings list, and optionally the nodes \
                 matching a search and the file text.",
                &const {
                    [
                        p(
                            "search",
                            "string",
                            false,
                            "Also answer the node ids whose text, keys, labels or speakers contain this.",
                        ),
                        p("text", "boolean", false, "Include the file text it would save."),
                    ]
                },
                "{ path, id, read_only, parse_error, dirty, revision, entry, nodes, edges, issues, \
                 selected, matches?, text? }",
                false,
            ),
            conversation_graph as Handler,
        ),
        (
            m(
                "editor.conversation_edit",
                "conversation_edit",
                "One undoable edit of the open conversation: `ops` (add_line, add_step, delete_step, \
                 move_step, set_line, set_prompt, add_option, set_option, delete_option, move_option, \
                 set_cond, set_event, retarget, rename_label) applied all or nothing; `move` pins a node's \
                 position in the layout sidecar; `select` picks a node.",
                &const {
                    [
                        p(
                            "ops",
                            "array",
                            false,
                            "The operations, applied in order (at most 512).",
                        ),
                        p(
                            "move",
                            "object",
                            false,
                            "{ node, x, y } pins a node; { node, reset: true } returns it to the automatic layout.",
                        ),
                        p(
                            "select",
                            "string",
                            false,
                            "Select this node id (empty clears the selection).",
                        ),
                        p("label", "string", false, "The undo entry's label."),
                    ]
                },
                "{ revision, dirty, nodes, edges, issues, selected }",
                true,
            ),
            conversation_edit as Handler,
        ),
        (
            m(
                "editor.conversation_save",
                "conversation_save",
                "Save the open conversation (to `path`, or where it was opened) and its layout sidecar. \
                 Saving without edits writes the bytes that were read.",
                &const { [p("path", "string", false, "Save as this .conv.ron path.")] },
                "{ path, sidecar }",
                true,
            ),
            conversation_save as Handler,
        ),
        (
            m(
                "editor.conversation_preview_step",
                "conversation_preview_step",
                "Step a preview of the open conversation: `start` begins at a node with the condition table; \
                 otherwise the preview moves past the shown line or takes option `choose` of the waiting \
                 choice. Answers the current node, the line or the offered options, and the trace.",
                &const {
                    [
                        p(
                            "start",
                            "string",
                            false,
                            "Start (or restart) at this node id (`\"\"`: the entry node).",
                        ),
                        p(
                            "conditions",
                            "object",
                            false,
                            "Condition name to true/false; a name not listed is unknown (false).",
                        ),
                        p("choose", "number", false, "Index among the offered options."),
                        p(
                            "locale",
                            "string",
                            false,
                            "Resolve text and clips in this locale.",
                        ),
                        p(
                            "play",
                            "boolean",
                            false,
                            "Play the line's voice clip on the editor's preview output (default false).",
                        ),
                        p("stop", "boolean", false, "End the preview."),
                    ]
                },
                "{ state, node, step, line?, options?, voice?, events, trace, reason?, detail? }",
                true,
            ),
            conversation_preview_step as Handler,
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

fn edit_error(e: &EditError) -> RpcError {
    let hint = match e {
        EditError::NotOpen => "call editor.conversation_open { path } first",
        EditError::ReadOnly(_) => "fix the parse error in the file, then open it again",
        EditError::Op(_) => "see book/agents/conversation-editor.md for each op's fields",
        EditError::Serialize(_) => "report this as a bug",
    };
    refused(e.reason(), e.to_string(), hint)
}

fn require_open(world: &World) -> Result<&ConvEditor, RpcError> {
    world
        .get_resource::<ConvEditor>()
        .filter(|e| e.is_open())
        .ok_or_else(|| edit_error(&EditError::NotOpen))
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(refused(
            "bad_param",
            format!("`{name}` must be a string"),
            "see the method schema",
        )),
    }
}

fn bool_param(params: &Value, name: &str) -> Result<bool, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(refused(
            "bad_param",
            format!("`{name}` must be a boolean"),
            "see the method schema",
        )),
    }
}

/// The issues as JSON (with the node id each is drawn on).
fn issues_json(editor: &ConvEditor) -> Value {
    json!(
        editor
            .issues
            .iter()
            .map(|i| {
                let mut v = i.issue.to_json();
                if let Some(map) = v.as_object_mut() {
                    map.insert(
                        "node".into(),
                        json!(
                            i.node
                                .and_then(|n| editor.graph.nodes.get(n))
                                .map(|n| n.id.as_str())
                        ),
                    );
                }
                v
            })
            .collect::<Vec<_>>()
    )
}

fn nodes_json(editor: &ConvEditor) -> Value {
    let Some(asset) = &editor.asset else {
        return json!([]);
    };
    json!(
        editor
            .graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let steps: Vec<Value> = (node.first..node.end)
                    .map(|s| {
                        let mut v = ConvGraph::step_json(asset, &editor.graph.steps[s]);
                        if let Some(map) = v.as_object_mut() {
                            map.insert("index".into(), json!(s));
                            if let Step::Line(id) = &editor.graph.steps[s]
                                && let Some(text) = asset.line(id).and_then(|l| editor.project.text(&l.text))
                            {
                                map.insert("display".into(), json!(text));
                            }
                        }
                        v
                    })
                    .collect();
                let codes: Vec<&str> = editor
                    .issues
                    .iter()
                    .filter(|f| f.node == Some(i))
                    .map(|f| f.issue.code)
                    .collect();
                let (x, y) = editor.layout.positions.get(i).copied().unwrap_or_default();
                json!({
                    "id": node.id,
                    "kind": node.kind.name(),
                    "label": node.label,
                    "first_step": node.first,
                    "steps": steps,
                    "x": x,
                    "y": y,
                    "pinned": editor.layout.pinned.get(i).copied().unwrap_or(false),
                    "layer": editor.layout.layers.get(i).copied().unwrap_or(0),
                    "issues": codes,
                })
            })
            .collect::<Vec<_>>()
    )
}

fn edges_json(editor: &ConvEditor) -> Value {
    let id = |n: usize| editor.graph.nodes.get(n).map(|n| n.id.clone());
    json!(
        editor
            .graph
            .edges
            .iter()
            .map(|e| json!({
                "from": id(e.from),
                "to": e.to.and_then(id),
                "kind": e.kind.name(),
                "step": e.step,
                "label": e.label,
                "option": e.option,
                "cond": e.cond,
                "text": e.text,
            }))
            .collect::<Vec<_>>()
    )
}

fn conversation_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?
        .ok_or_else(|| refused("missing_param", "`path` is required", "pass a .conv.ron path"))?;
    if !path.ends_with(".conv.ron") {
        return Err(refused(
            "bad_path",
            format!("{path} is not a .conv.ron path"),
            "conversations are assets/conversations/<id>.conv.ron",
        ));
    }
    // The editor's one document-path rule (ADR-7388, audit rc1-sec-paths-z finding 2): `..`,
    // absolute paths outside the project and the temp dir, and symlinks out are refused.
    let full = crate::agent_ext::document_request_path(world, "path", path)?;
    let project = match str_param(params, "project")? {
        Some(p) => Some(crate::agent_ext::document_request_path(world, "project", p)?),
        None => crate::bt_editor::project_root(world),
    };
    let mut editor = world.get_resource_or_insert_with(ConvEditor::default);
    let discarded = editor.is_open() && editor.dirty();
    editor
        .open(&full, project.as_deref())
        .map_err(|e| refused("io_error", e, "check the path"))?;
    Ok(json!({
        "path": full.display().to_string(),
        "id": editor.asset.as_ref().map(|a| a.id.clone()),
        "read_only": editor.read_only(),
        "parse_error": editor.parse_error,
        "nodes": editor.graph.nodes.len(),
        "edges": editor.graph.edges.len(),
        "issues": editor.issues.len(),
        "revision": editor.revision,
        "discarded_unsaved": discarded,
    }))
}

fn conversation_graph(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let search = str_param(params, "search")?;
    let with_text = bool_param(params, "text")?;
    let editor = require_open(world)?;
    let mut out = json!({
        "path": editor.path.as_ref().map(|p| p.display().to_string()),
        "id": editor.asset.as_ref().map(|a| a.id.clone()),
        "read_only": editor.read_only(),
        "parse_error": editor.parse_error,
        "dirty": editor.dirty(),
        "revision": editor.revision,
        "entry": editor.graph.nodes.first().map(|n| n.id.clone()),
        "nodes": nodes_json(editor),
        "edges": edges_json(editor),
        "issues": issues_json(editor),
        "selected": editor.selected,
    });
    if let Some(map) = out.as_object_mut() {
        if let Some(q) = search {
            let matches: Vec<&str> = editor
                .search(q)
                .into_iter()
                .filter_map(|n| editor.graph.nodes.get(n).map(|n| n.id.as_str()))
                .collect();
            map.insert("matches".into(), json!(matches));
        }
        if with_text {
            map.insert("text".into(), json!(editor.text()));
        }
    }
    Ok(out)
}

fn conversation_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let label = str_param(params, "label")?.unwrap_or("agent edit").to_string();
    let ops: Option<Vec<ConvOp>> = match params.get("ops") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let list = v
                .as_array()
                .ok_or_else(|| refused("bad_ops", "`ops` must be an array", "see the op list"))?;
            if list.len() > MAX_OPS {
                return Err(refused(
                    "too_many_ops",
                    format!("{} ops (at most {MAX_OPS})", list.len()),
                    "split the batch",
                ));
            }
            Some(
                serde_json::from_value(v.clone())
                    .map_err(|e| refused("bad_ops", format!("bad ops: {e}"), "see the op list"))?,
            )
        }
    };
    let mv = params.get("move").filter(|v| !v.is_null());
    let select = str_param(params, "select")?;
    if ops.is_none() && mv.is_none() && select.is_none() {
        return Err(refused(
            "missing_param",
            "pass `ops`, `move` or `select`",
            "see the method schema",
        ));
    }
    if let Some(ops) = ops {
        edit(world, &label, None, &ops).map_err(|e| edit_error(&e))?;
    }
    if let Some(mv) = mv {
        let node = mv
            .get("node")
            .and_then(Value::as_str)
            .ok_or_else(|| refused("bad_move", "`move.node` must be a node id", "see the schema"))?;
        let pos = if mv.get("reset").and_then(Value::as_bool).unwrap_or(false) {
            None
        } else {
            let x = mv.get("x").and_then(Value::as_f64);
            let y = mv.get("y").and_then(Value::as_f64);
            match (x, y) {
                (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Some((x as f32, y as f32)),
                _ => {
                    return Err(refused(
                        "bad_move",
                        "`move` needs finite `x` and `y`, or `reset: true`",
                        "see the schema",
                    ));
                }
            }
        };
        move_node(world, node, pos).map_err(|e| edit_error(&e))?;
    }
    if let Some(sel) = select {
        let mut editor = world.resource_mut::<ConvEditor>();
        if sel.is_empty() {
            editor.selected = None;
        } else if editor.graph.node_index(sel).is_some() {
            editor.selected = Some(sel.to_string());
        } else {
            return Err(refused(
                "bad_node",
                format!("no node `{sel}`"),
                "list nodes with editor.conversation_graph",
            ));
        }
    }
    let editor = world.resource::<ConvEditor>();
    Ok(json!({
        "revision": editor.revision,
        "dirty": editor.dirty(),
        "nodes": editor.graph.nodes.len(),
        "edges": editor.graph.edges.len(),
        "issues": issues_json(editor),
        "selected": editor.selected,
    }))
}

fn conversation_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    // ADR-7388 (audit rc1-sec-paths-z finding 2), as in `conversation_open`; the save itself
    // refuses a path that is not a `.conv.ron`.
    let path = match str_param(params, "path")? {
        Some(p) => Some(crate::agent_ext::document_request_path(world, "path", p)?),
        None => None,
    };
    let mut editor = world.resource_mut::<ConvEditor>();
    let saved = editor
        .save(path.as_deref())
        .map_err(|e| refused("save_failed", e, "check the path and permissions"))?;
    let sidecar = editor
        .sidecar_path()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string());
    Ok(json!({ "path": saved.display().to_string(), "sidecar": sidecar }))
}

/// The preview's state as JSON.
#[must_use]
pub fn preview_json(editor: &ConvEditor) -> Value {
    let Some(preview) = &editor.preview else {
        return json!({ "state": "stopped", "node": null, "step": null, "events": [], "trace": [] });
    };
    let node = preview
        .state
        .step()
        .and_then(|s| editor.graph.node_of_step(s))
        .and_then(|n| editor.graph.nodes.get(n))
        .map(|n| n.id.clone());
    let trace: Vec<Value> = preview
        .trace
        .iter()
        .map(|&s| {
            json!({
                "step": s,
                "node": editor.graph.node_of_step(s).and_then(|n| editor.graph.nodes.get(n)).map(|n| n.id.clone()),
            })
        })
        .collect();
    let locale = if editor.preview_locale.is_empty() {
        editor.project.source.clone()
    } else {
        editor.preview_locale.clone()
    };
    let mut out = json!({
        "node": node,
        "step": preview.state.step(),
        "events": preview.events,
        "trace": trace,
        "locale": locale,
    });
    let Some(map) = out.as_object_mut() else {
        return out;
    };
    match &preview.state {
        PreviewState::Line { line, .. } => {
            let l = editor.asset.as_ref().and_then(|a| a.line(line));
            map.insert("state".into(), json!("line"));
            map.insert(
                "line".into(),
                json!({
                    "id": line,
                    "speaker": l.map(|l| l.speaker.clone()),
                    "text": l.map(|l| l.text.clone()),
                    "display": l.and_then(|l| editor.project.text_in(&locale, &l.text)),
                }),
            );
            map.insert(
                "voice".into(),
                json!(
                    editor
                        .preview_voice
                        .as_ref()
                        .map(|(clip, played)| json!({ "clip": clip, "played": played }))
                ),
            );
        }
        PreviewState::Choice { offered, .. } => {
            map.insert("state".into(), json!("choice"));
            map.insert(
                "options".into(),
                json!(
                    offered
                        .iter()
                        .enumerate()
                        .map(|(k, (option, text, goto))| json!({
                            "index": k,
                            "option": option,
                            "text": text,
                            "display": editor.project.text_in(&locale, text),
                            "goto": goto,
                        }))
                        .collect::<Vec<_>>()
                ),
            );
        }
        PreviewState::Finished { reason, detail } => {
            map.insert("state".into(), json!("finished"));
            map.insert("reason".into(), json!(reason));
            map.insert("detail".into(), json!(detail));
        }
    }
    out
}

fn conversation_preview_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let start = str_param(params, "start")?.map(str::to_string);
    let play = bool_param(params, "play")?;
    let stop = bool_param(params, "stop")?;
    let locale = str_param(params, "locale")?.map(str::to_string);
    let choose = match params.get("choose") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| refused("bad_param", "`choose` must be a whole number", "see the schema"))?,
        ),
    };
    let conditions: Option<BTreeMap<String, bool>> = match params.get("conditions") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value(v.clone()).map_err(|e| {
            refused(
                "bad_param",
                format!("`conditions` must map names to booleans: {e}"),
                "{ \"met_captain\": true }",
            )
        })?),
    };
    let mut editor = world.resource_mut::<ConvEditor>();
    if editor.read_only() {
        let e = EditError::ReadOnly(editor.parse_error.clone().unwrap_or_default());
        return Err(edit_error(&e));
    }
    if let Some(c) = conditions {
        editor.conditions = c;
    }
    if let Some(l) = locale {
        if !l.is_empty() && !editor.project.locales.contains(&l) {
            return Err(refused(
                "locale_not_loaded",
                format!("locale `{l}` is not in the project"),
                "use one of the project's localization directories",
            ));
        }
        editor.preview_locale = l;
    }
    if stop {
        editor.preview = None;
        editor.preview_voice = None;
        return Ok(preview_json(&editor));
    }
    if let Some(start) = start {
        let node = if start.is_empty() {
            0
        } else {
            editor.graph.node_index(&start).ok_or_else(|| {
                refused(
                    "bad_node",
                    format!("no node `{start}`"),
                    "list nodes with editor.conversation_graph",
                )
            })?
        };
        editor.preview_from(node, play);
    } else {
        editor.preview_step(choose, play).map_err(|reason| {
            let hint = match reason {
                "no_preview" => "pass `start` (a node id, or \"\" for the entry) first",
                "choice_required" => "pass `choose` with an index among the offered options",
                _ => "see book/agents/conversation-editor.md",
            };
            refused(reason, format!("the preview cannot step: {reason}"), hint)
        })?;
    }
    Ok(preview_json(&editor))
}
