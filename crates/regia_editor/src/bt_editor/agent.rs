// SPDX-License-Identifier: MPL-2.0
//! The behavior tree editor's agent-protocol surface: `editor.bt_*` methods that open, inspect,
//! edit and save the tree the panel shows (`book/agents/behavior-tree-editor.md`).
//!
//! Every mutating method goes through [`super::edit`], so it lands on the editor's undo stack
//! exactly like a panel edit (`editor.undo` reverts it). Operations are the `ai.bt_edit` ones
//! (`regia_ai::bt_edit::BtOp`). Errors are `F0002` with `data.reason`.

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::bt_edit::{self, BtOp, BtSeverity};
use regia_ecs::prelude::*;

use super::panel::{self, kind_name};
use super::{BtEditor, diagnose, edit, palette_tasks, resolve_path};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

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

/// Every behavior-tree-editor method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.bt_open",
                "bt_open",
                "Open a .bt.ron in the behavior tree editor, or start a new tree (create: true).",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            false,
                            "Project-relative or absolute .bt.ron path.",
                        ),
                        p(
                            "create",
                            "boolean",
                            false,
                            "Start an empty tree (saved to `path` later).",
                        ),
                    ]
                },
                "{ path, nodes, valid, revision, discarded_unsaved }",
                true,
            ),
            bt_open as Handler,
        ),
        (
            m(
                "editor.bt_state",
                "bt_state",
                "The open tree: nodes (index, name, kind, parameters, children, parent), validation issues \
                 with codes, the selection, the live state while the game plays, and optionally the palette \
                 and the canonical text.",
                &const {
                    [
                        p(
                            "palette",
                            "boolean",
                            false,
                            "Include the node kinds and the registered leaf tasks.",
                        ),
                        p("ron", "boolean", false, "Include the .bt.ron text it would save."),
                    ]
                },
                "{ path, dirty, revision, root, nodes, valid, issues, selected, agents, live, palette?, ron? }",
                false,
            ),
            bt_state as Handler,
        ),
        (
            m(
                "editor.bt_edit",
                "bt_edit",
                "One undoable edit of the open tree: `ops` as for ai.bt_edit (add_node, remove_node, connect, \
                 disconnect, reorder, set_node, set_root), applied all or nothing. `select` picks a node.",
                &const {
                    [
                        p(
                            "ops",
                            "array",
                            false,
                            "The operations, applied in order (at most 512).",
                        ),
                        p(
                            "select",
                            "number",
                            false,
                            "Select this node (-1 clears the selection).",
                        ),
                        p("label", "string", false, "The undo entry's label."),
                    ]
                },
                "{ created, revision, valid, issues, selected }",
                true,
            ),
            bt_edit_method as Handler,
        ),
        (
            m(
                "editor.bt_save",
                "bt_save",
                "Save the open tree as canonical .bt.ron (to `path`, or where it was opened). A tree with \
                 errors is saved too.",
                &const { [p("path", "string", false, "Save as this path.")] },
                "{ path, valid }",
                true,
            ),
            bt_save as Handler,
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

fn not_open() -> RpcError {
    refused(
        "no_tree_open",
        "no behavior tree is open",
        "call editor.bt_open { path } or { create: true } first",
    )
}

fn require_open(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<BtEditor>().is_some_and(BtEditor::is_open) {
        Ok(())
    } else {
        Err(not_open())
    }
}

fn issues_json(world: &World) -> (bool, Value) {
    let issues = diagnose(world);
    let valid = bt_edit::is_valid(&issues);
    let list = issues
        .iter()
        .map(|i| {
            json!({
                "code": i.code,
                "severity": if i.severity == BtSeverity::Error { "error" } else { "warning" },
                "node": i.node,
                "message": i.message,
            })
        })
        .collect::<Vec<_>>();
    (valid, json!(list))
}

fn nodes_json(def: &regia_ai::BehaviorTreeDef) -> Value {
    json!(
        def.nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let mut v = serde_json::to_value(node).unwrap_or(Value::Null);
                if let Some(map) = v.as_object_mut() {
                    map.insert("index".into(), json!(index));
                    let parent = def.nodes.iter().position(|n| n.children.contains(&index));
                    map.insert("parent".into(), json!(parent));
                }
                v
            })
            .collect::<Vec<_>>()
    )
}

/// A `.bt.ron` path (project-relative or absolute)
/// confined by the editor's one document-path rule (ADR-7388): `..`, symlinks out and paths outside
/// the project and the temp dir are `path_outside_project`.
fn request_path(world: &World, path: &str) -> Result<std::path::PathBuf, RpcError> {
    if !path.ends_with(".bt.ron") {
        return Err(refused(
            "bad_path",
            "`path` must name a `.bt.ron` file",
            "use a path like assets/ai/guard.bt.ron",
        ));
    }
    let resolved = resolve_path(world, path);
    crate::agent_ext::document_request_path(world, "path", &resolved.to_string_lossy())
}

fn bt_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(|p| request_path(world, p))
        .transpose()?;
    let mut editor = world.get_resource_or_insert_with(BtEditor::default);
    let unsaved = editor.has_unsaved_work();
    if create {
        editor.new_tree(path);
    } else {
        let path = path.ok_or_else(|| {
            refused(
                "missing_param",
                "`path` is required unless `create: true`",
                "pass a .bt.ron path",
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
            .map_err(|e| refused("open_failed", e, "the file is not a readable .bt.ron"))?;
    }
    let nodes = editor.doc.nodes.len();
    let revision = editor.revision;
    let shown = editor.path.as_ref().map(|p| p.display().to_string());
    let (valid, _) = issues_json(world);
    Ok(json!({
        "path": shown,
        "nodes": nodes,
        "valid": valid,
        "revision": revision,
        "discarded_unsaved": unsaved,
    }))
}

fn bt_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let (valid, issues) = issues_json(world);
    let editor = world.resource::<BtEditor>();
    let mut out = json!({
        "path": editor.path.as_ref().map(|p| p.display().to_string()),
        "dirty": editor.dirty(),
        "revision": editor.revision,
        "root": editor.doc.root,
        "nodes": nodes_json(&editor.doc),
        "valid": valid,
        "issues": issues,
        "selected": editor.selected,
        "agents": editor.agents,
        "live": editor.live.as_ref().map(|l| json!({
            "entity": l.entity,
            "ticks": l.ticks,
            "last_status": format!("{:?}", l.last_status).to_lowercase(),
            "matches_document": l.matches_document,
            "nodes": l.node_status.iter().enumerate().map(|(i, s)| json!({
                "index": i,
                "status": format!("{s:?}").to_lowercase(),
                "running": *s == regia_ai::BtStatus::Running,
                "changed_at": l.changed_at.get(i).copied().unwrap_or(0),
            })).collect::<Vec<_>>(),
        })),
    });
    if params.get("palette").and_then(Value::as_bool).unwrap_or(false) {
        out["palette"] = json!({
            "kinds": panel::KINDS.iter().copied().map(kind_name).collect::<Vec<_>>(),
            "tasks": palette_tasks(world),
        });
    }
    if params.get("ron").and_then(Value::as_bool).unwrap_or(false) {
        out["ron"] = json!(bt_edit::to_canonical_ron(&world.resource::<BtEditor>().doc).unwrap_or_default());
    }
    Ok(out)
}

fn bt_edit_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let mut ops = Vec::new();
    if let Some(list) = params.get("ops") {
        let list = list.as_array().ok_or_else(|| {
            refused(
                "bad_ops",
                "`ops` must be an array",
                "see book/agents/behavior-tree-editor.md",
            )
        })?;
        if list.len() > 512 {
            return Err(refused(
                "too_many_ops",
                "at most 512 operations",
                "split the edit",
            ));
        }
        for (i, op) in list.iter().enumerate() {
            ops.push(serde_json::from_value::<BtOp>(op.clone()).map_err(|e| {
                refused(
                    "bad_ops",
                    format!("op {i}: {e}"),
                    "see book/agents/behavior-tree-editor.md",
                )
            })?);
        }
    }
    let select = params.get("select").and_then(Value::as_i64);
    if ops.is_empty() && select.is_none() {
        return Err(refused(
            "missing_param",
            "pass `ops` or `select`",
            "see book/agents/behavior-tree-editor.md",
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
    let mut editor = world.resource_mut::<BtEditor>();
    if let Some(node) = created.iter().rev().flatten().next() {
        editor.selected = Some(*node);
    }
    if let Some(s) = select {
        let count = editor.doc.nodes.len();
        editor.selected = match usize::try_from(s) {
            Ok(n) if n < count => Some(n),
            Ok(_) => {
                return Err(refused(
                    "bad_node",
                    format!("node {s} does not exist"),
                    "see editor.bt_state nodes",
                ));
            }
            Err(_) => None,
        };
    }
    let revision = editor.revision;
    let selected = editor.selected;
    let (valid, issues) = issues_json(world);
    Ok(json!({
        "created": created,
        "revision": revision,
        "valid": valid,
        "issues": issues,
        "selected": selected,
    }))
}

fn bt_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(|p| request_path(world, p))
        .transpose()?;
    let (valid, _) = issues_json(world);
    let mut editor = world.resource_mut::<BtEditor>();
    let saved = editor
        .save(path.as_deref(), valid)
        .map_err(|e| refused("save_failed", e, "pass a .bt.ron path that can be written"))?;
    Ok(json!({ "path": saved.display().to_string(), "valid": valid }))
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

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        world
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
        assert!(names.iter().all(|n| n.starts_with("editor.bt_")));
    }

    #[test]
    fn calls_before_open_say_how_to_open() {
        let mut world = world();
        let err = call(&mut world, "editor.bt_state", json!({})).expect_err("error");
        assert_eq!(reason(&err), "no_tree_open");
    }

    /// The headless panel test over the agent: open, add a Selector and two Leafs, connect, save,
    /// reload, and the reloaded tree equals the saved one.
    #[test]
    fn open_add_selector_and_two_leafs_connect_save_and_reload() {
        let dir = regia_test_utils::unique_temp_dir("bt-editor-agent");
        let mut world = world();
        let path = dir.join("guard.bt.ron");
        call(
            &mut world,
            "editor.bt_open",
            json!({ "create": true, "path": path.display().to_string() }),
        )
        .expect("open");
        let added = call(
            &mut world,
            "editor.bt_edit",
            json!({ "label": "build", "ops": [
                { "op": "add_node", "kind": "selector", "name": "root" },
                { "op": "add_node", "kind": "leaf", "name": "patrol", "task": "game::Patrol" },
                { "op": "add_node", "kind": "leaf", "name": "idle", "task": "game::Idle" },
                { "op": "connect", "parent": 0, "child": 1 },
                { "op": "connect", "parent": 0, "child": 2 },
            ] }),
        )
        .expect("edit");
        assert_eq!(added["created"], json!([0, 1, 2, null, null]));
        assert_eq!(added["valid"], true);
        let state = call(&mut world, "editor.bt_state", json!({ "ron": true })).expect("state");
        assert_eq!(state["nodes"][0]["children"], json!([1, 2]));
        assert_eq!(state["nodes"][1]["parent"], 0);
        assert_eq!(state["dirty"], true);
        let saved = call(&mut world, "editor.bt_save", json!({})).expect("save");
        assert_eq!(saved["valid"], true);
        let before = world.resource::<BtEditor>().doc.clone();
        // A fresh editor reads the file back to an equal tree, with the same canonical text.
        let mut fresh = World::new();
        call(
            &mut fresh,
            "editor.bt_open",
            json!({ "path": path.display().to_string() }),
        )
        .expect("reload");
        assert_eq!(fresh.resource::<BtEditor>().doc, before);
        let again = call(&mut fresh, "editor.bt_state", json!({ "ron": true })).expect("state");
        assert_eq!(again["ron"], state["ron"]);
        assert_eq!(again["dirty"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_trees_report_codes_and_edits_undo() {
        let mut world = world();
        call(&mut world, "editor.bt_open", json!({ "create": true })).expect("open");
        let out = call(
            &mut world,
            "editor.bt_edit",
            json!({ "ops": [
                { "op": "add_node", "kind": "selector" },
                { "op": "add_node", "kind": "inverter", "parent": 0 },
                { "op": "add_node", "kind": "leaf", "parent": 0 },
            ] }),
        )
        .expect("edit");
        assert_eq!(out["valid"], false);
        let codes: Vec<&str> = out["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|i| i["code"].as_str())
            .collect();
        assert!(
            codes.contains(&"decorator_arity") && codes.contains(&"empty_task"),
            "{codes:?}"
        );
        // Refused edits name a reason and change nothing.
        let err = call(
            &mut world,
            "editor.bt_edit",
            json!({ "ops": [{ "op": "connect", "parent": 2, "child": 0 }] }),
        )
        .expect_err("cycle");
        assert_eq!(reason(&err), "would_cycle");
        let err = call(&mut world, "editor.bt_edit", json!({ "ops": [{ "op": "nope" }] })).expect_err("bad");
        assert_eq!(reason(&err), "bad_ops");
        // One undo reverts the whole batch.
        crate::command::undo_world(&mut world);
        let state = call(&mut world, "editor.bt_state", json!({})).expect("state");
        assert_eq!(state["nodes"].as_array().map(Vec::len), Some(0));
        assert_eq!(state["issues"][0]["code"], "empty_tree");
    }

    #[test]
    fn select_moves_the_selection_and_checks_the_node() {
        let mut world = world();
        call(&mut world, "editor.bt_open", json!({ "create": true })).expect("open");
        call(
            &mut world,
            "editor.bt_edit",
            json!({ "ops": [{ "op": "add_node", "kind": "selector" }] }),
        )
        .expect("add");
        assert_eq!(
            world.resource::<BtEditor>().selected,
            Some(0),
            "a new node is selected"
        );
        call(&mut world, "editor.bt_edit", json!({ "select": -1 })).expect("clear");
        assert_eq!(world.resource::<BtEditor>().selected, None);
        let err = call(&mut world, "editor.bt_edit", json!({ "select": 9 })).expect_err("bad node");
        assert_eq!(reason(&err), "bad_node");
        let err = call(&mut world, "editor.bt_edit", json!({})).expect_err("nothing to do");
        assert_eq!(reason(&err), "missing_param");
    }

    #[test]
    fn the_palette_lists_kinds_and_registered_tasks() {
        let mut world = world();
        world.insert_resource(regia_ai::BtTaskRegistry::default());
        call(&mut world, "editor.bt_open", json!({ "create": true })).expect("open");
        let state = call(&mut world, "editor.bt_state", json!({ "palette": true })).expect("state");
        assert_eq!(
            state["palette"]["kinds"].as_array().map(Vec::len),
            Some(panel::KINDS.len())
        );
        let kinds = state["palette"]["kinds"].as_array().expect("kinds");
        assert!(kinds.iter().any(|kind| kind == "compare"));
        assert!(kinds.iter().any(|kind| kind == "state_tree"));
        assert_eq!(state["palette"]["tasks"], json!([]));
    }

    /// The palette lists the `MoveTo` leaf `AiPlugin` registers (ADR-8293).
    #[test]
    fn the_palette_lists_the_move_to_leaf() {
        let mut world = world();
        let mut registry = regia_ai::BtTaskRegistry::default();
        regia_ai::path_provider::register_move_to(&mut registry);
        world.insert_resource(registry);
        call(&mut world, "editor.bt_open", json!({ "create": true })).expect("open");
        let state = call(&mut world, "editor.bt_state", json!({ "palette": true })).expect("state");
        assert_eq!(state["palette"]["tasks"], json!([regia_ai::MOVE_TO_TASK]));
    }
}
