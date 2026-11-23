// SPDX-License-Identifier: MPL-2.0
//! The state tree editor's agent-protocol surface: `editor.statetree_*` methods that open, inspect,
//! edit and save the tree the panel shows (`book/agents/state-tree-editor.md`, ADR-3342).
//!
//! Every mutating method goes through [`super::edit`], so it lands on the editor's undo stack
//! exactly like a panel edit (`editor.undo` reverts it). Operations are the `ai.statetree_edit` ones
//! (`regia_ai::state_tree_edit::StOp`). Errors are `F0002` with `data.reason`.

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::state_tree_edit::{self, StIssue, StOp, StSeverity};
use regia_ecs::prelude::*;

use super::{StEditor, diagnose, edit, palette_tasks, resolve_path, save};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// Most operations one `editor.statetree_edit` call applies.
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

/// Every state-tree-editor method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.statetree_open",
                "statetree_open",
                "Open a .statetree.ron in the state tree editor, or start a new tree (create: true, with an \
                 optional root state name).",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            false,
                            "Project-relative or absolute .statetree.ron path.",
                        ),
                        p(
                            "create",
                            "boolean",
                            false,
                            "Start a new tree (saved to `path` later).",
                        ),
                        p(
                            "root",
                            "string",
                            false,
                            "With create: the root state's name (default \"Root\").",
                        ),
                    ]
                },
                "{ path, states, valid, revision, discarded_unsaved }",
                true,
            ),
            statetree_open as Handler,
        ),
        (
            m(
                "editor.statetree_state",
                "statetree_state",
                "The open tree: states in hierarchy order (name, parent, depth, children, selection, tasks, \
                 parameters, transitions), validation issues with codes and paths, the selection, and while \
                 the game plays the agents, the watched tree's active path and what the last save did to \
                 running agents (kept, restarted, rejected). Optionally the palette and the canonical text.",
                &const {
                    [
                        p(
                            "palette",
                            "boolean",
                            false,
                            "Include the registered tasks, triggers, targets and selections.",
                        ),
                        p(
                            "ron",
                            "boolean",
                            false,
                            "Include the .statetree.ron text it would save.",
                        ),
                    ]
                },
                "{ path, dirty, revision, root, states, valid, issues, selected, agents, live, reload, saves, \
                 palette?, ron? }",
                false,
            ),
            statetree_state as Handler,
        ),
        (
            m(
                "editor.statetree_edit",
                "statetree_edit",
                "One undoable edit of the open tree: `ops` as for ai.statetree_edit (add_state, remove_state, \
                 rename_state, reparent_state, set_root, set_state, set_tasks, set_ambient, set_param, \
                 add_transition, remove_transition, set_transition, move_transition, add_include, \
                 remove_include, set_include, set_tree), applied all or nothing. `select` picks a state, \
                 `watch` picks the play-world agent whose active path to show.",
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
                            "string",
                            false,
                            "Select this state (an empty string clears the selection).",
                        ),
                        p(
                            "watch",
                            "number",
                            false,
                            "Show this play-world agent's active path (entity bits; 0 picks automatically).",
                        ),
                        p("label", "string", false, "The undo entry's label."),
                    ]
                },
                "{ revision, valid, issues, selected, watch }",
                true,
            ),
            statetree_edit_method as Handler,
        ),
        (
            m(
                "editor.statetree_save",
                "statetree_save",
                "Save the open tree as canonical .statetree.ron (to `path`, or where it was opened), atomically. \
                 A tree with validation errors is refused (invalid_tree) and nothing is written. While the \
                 game plays, agents whose StateTreeSource names the file reload it; editor.statetree_state \
                 then shows the outcome (kept or restarted).",
                &const { [p("path", "string", false, "Save as this path.")] },
                "{ path, valid, saves, playing }",
                true,
            ),
            statetree_save as Handler,
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
    if world.get_resource::<StEditor>().is_some_and(StEditor::is_open) {
        Ok(())
    } else {
        Err(refused(
            "no_tree_open",
            "no state tree is open",
            "call editor.statetree_open { path } or { create: true } first",
        ))
    }
}

/// The issues as the protocol shows them.
fn issues_list(issues: &[StIssue]) -> Value {
    json!(
        issues
            .iter()
            .map(|i| json!({
                "code": i.code,
                "severity": if i.severity == StSeverity::Error { "error" } else { "warning" },
                "path": i.path,
                "state": i.state(),
                "message": i.message,
            }))
            .collect::<Vec<_>>()
    )
}

fn issues_json(world: &World) -> (bool, Value) {
    let issues = diagnose(world);
    (state_tree_edit::is_valid(&issues), issues_list(&issues))
}

/// The states in hierarchy order with their fields, parent and depth.
fn states_json(def: &regia_ai::state_tree::StateTreeDef) -> Value {
    let parents = state_tree_edit::parents(def);
    json!(
        state_tree_edit::hierarchy(def)
            .into_iter()
            .map(|(index, depth)| {
                let state = &def.states[index];
                let mut v = serde_json::to_value(state).unwrap_or(Value::Null);
                if let Some(map) = v.as_object_mut() {
                    map.insert("index".into(), json!(index));
                    map.insert("depth".into(), json!(depth));
                    map.insert(
                        "parent".into(),
                        json!(parents.get(&state.name).cloned().flatten()),
                    );
                }
                v
            })
            .collect::<Vec<_>>()
    )
}

/// A `.statetree.ron` path (project-relative or absolute)
/// confined by the editor's one document-path rule (ADR-7388): `..`, symlinks out and paths outside
/// the project and the temp dir are `path_outside_project`.
fn request_path(world: &World, path: &str) -> Result<std::path::PathBuf, RpcError> {
    if !path.ends_with(".statetree.ron") {
        return Err(refused(
            "bad_path",
            "`path` must name a `.statetree.ron` file",
            "use a path like assets/ai/guard.statetree.ron",
        ));
    }
    let resolved = resolve_path(world, path);
    crate::agent_ext::document_request_path(world, "path", &resolved.to_string_lossy())
}

fn statetree_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(|p| request_path(world, p))
        .transpose()?;
    let root = params.get("root").and_then(Value::as_str).unwrap_or("Root");
    let mut editor = world.get_resource_or_insert_with(StEditor::default);
    let unsaved = editor.has_unsaved_work();
    if create {
        editor.new_tree(path, root);
    } else {
        let path = path.ok_or_else(|| {
            refused(
                "missing_param",
                "`path` is required unless `create: true`",
                "pass a .statetree.ron path",
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
            .map_err(|e| refused("open_failed", e, "the file is not a readable .statetree.ron"))?;
    }
    let states = editor.doc.states.len();
    let revision = editor.revision;
    let shown = editor.path.as_ref().map(|p| p.display().to_string());
    let (valid, _) = issues_json(world);
    Ok(json!({
        "path": shown,
        "states": states,
        "valid": valid,
        "revision": revision,
        "discarded_unsaved": unsaved,
    }))
}

fn statetree_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let (valid, issues) = issues_json(world);
    let editor = world.resource::<StEditor>();
    let mut out = json!({
        "path": editor.path.as_ref().map(|p| p.display().to_string()),
        "dirty": editor.dirty(),
        "revision": editor.revision,
        "root": editor.doc.root,
        "description": editor.doc.description,
        "ambient": editor.doc.ambient,
        "includes": serde_json::to_value(&editor.doc.includes).unwrap_or(Value::Null),
        "states": states_json(&editor.doc),
        "valid": valid,
        "issues": issues,
        "selected": editor.selected,
        "agents": editor.agents,
        "watch": editor.watch,
        "live": editor.live.as_ref().map(|l| json!({
            "entity": l.entity,
            "status": l.status,
            "active_path": l.active_path,
            "ticks": l.ticks,
            "uses_file": l.uses_file,
            "matches_document": l.matches_document,
        })),
        "reload": editor.reload.as_ref().map(|r| json!({
            "save": r.save,
            "delivered": r.delivered,
            "kept": r.kept,
            "restarted": r.restarted,
            "rejected": r.rejected,
            "last": r.last,
            "reason": r.reason,
        })),
        "saves": editor.saves,
    });
    if params.get("palette").and_then(Value::as_bool).unwrap_or(false) {
        out["palette"] = json!({
            "tasks": palette_tasks(world),
            "triggers": ["Tick", "Event", "Succeeded", "Failed", "Completed"],
            "targets": ["State", "Next", "NextEnterable", "Parent", "Stay", "Succeed", "Fail"],
            "selections": ["First", "Utility", "Weighted"],
            "literals": ["Bool", "Int", "Float", "String"],
        });
    }
    if params.get("ron").and_then(Value::as_bool).unwrap_or(false) {
        out["ron"] =
            json!(state_tree_edit::to_canonical_ron(&world.resource::<StEditor>().doc).unwrap_or_default());
    }
    Ok(out)
}

fn statetree_edit_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_open(world)?;
    let mut ops = Vec::new();
    if let Some(list) = params.get("ops") {
        let list = list.as_array().ok_or_else(|| {
            refused(
                "bad_ops",
                "`ops` must be an array",
                "see book/agents/state-tree-editor.md",
            )
        })?;
        if list.len() > MAX_OPS {
            return Err(refused(
                "too_many_ops",
                format!("at most {MAX_OPS} operations"),
                "split the edit",
            ));
        }
        for (i, op) in list.iter().enumerate() {
            ops.push(serde_json::from_value::<StOp>(op.clone()).map_err(|e| {
                refused(
                    "bad_ops",
                    format!("op {i}: {e}"),
                    "see book/agents/state-tree-editor.md",
                )
            })?);
        }
    }
    let select = params.get("select").and_then(Value::as_str).map(str::to_string);
    let watch = params.get("watch").and_then(Value::as_u64);
    if ops.is_empty() && select.is_none() && watch.is_none() {
        return Err(refused(
            "missing_param",
            "pass `ops`, `select` or `watch`",
            "see book/agents/state-tree-editor.md",
        ));
    }
    if let Some(name) = select.as_deref()
        && !name.is_empty()
    {
        // Checked before the edit, against the document the edit will produce.
        let mut preview = world.get_resource_or_insert_with(StEditor::default).doc.clone();
        if state_tree_edit::apply_ops(&mut preview, &ops).is_ok()
            && !preview.states.iter().any(|s| s.name == name)
        {
            return Err(refused(
                "unknown_state",
                format!("no state is named {name:?}"),
                "see editor.statetree_state states",
            ));
        }
    }
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("edit")
        .to_string();
    if !ops.is_empty() {
        edit(world, &label, None, &ops).map_err(|e| {
            refused(
                e.error.code,
                format!("op {}: {}", e.op_index, e.error.message),
                "nothing was changed; fix the op and call again",
            )
        })?;
    }
    let mut editor = world.resource_mut::<StEditor>();
    if let Some(name) = select {
        editor.selected = (!name.is_empty()).then_some(name);
    }
    if let Some(bits) = watch {
        editor.watch = (bits != 0).then_some(bits);
    }
    let revision = editor.revision;
    let selected = editor.selected.clone();
    let watching = editor.watch;
    let (valid, issues) = issues_json(world);
    Ok(json!({
        "revision": revision,
        "valid": valid,
        "issues": issues,
        "selected": selected,
        "watch": watching,
    }))
}

fn statetree_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
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
            "fix the issues editor.statetree_state lists, or pass a .statetree.ron path that can be written",
        );
        if e.code == "invalid_tree"
            && let Some(Value::Object(data)) = error.data.as_mut()
        {
            data.insert("issues".into(), issues_list(&diagnose(world)));
        }
        error
    })?;
    let playing = crate::gameplay_panel::play_running(world);
    let saves = world.resource::<StEditor>().saves;
    Ok(json!({
        "path": saved.display().to_string(),
        "valid": true,
        "saves": saves,
        "playing": playing,
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
        assert!(names.iter().all(|n| n.starts_with("editor.statetree_")));
    }

    #[test]
    fn calls_before_open_say_how_to_open() {
        let mut world = world();
        for method in [
            "editor.statetree_state",
            "editor.statetree_edit",
            "editor.statetree_save",
        ] {
            let err = call(&mut world, method, json!({})).expect_err("error");
            assert_eq!(reason(&err), "no_tree_open", "{method}");
        }
    }

    /// The headless panel test over the agent: create, build a guard, save, reopen, and the reopened
    /// tree equals the saved one with the same canonical text.
    #[test]
    fn create_build_save_and_reopen() {
        let dir = regia_test_utils::unique_temp_dir("st-editor-agent");
        let mut world = world();
        let path = dir.join("guard.statetree.ron");
        let opened = call(
            &mut world,
            "editor.statetree_open",
            json!({ "create": true, "path": path.display().to_string() }),
        )
        .expect("open");
        assert_eq!(opened["states"], 1);
        let built = call(
            &mut world,
            "editor.statetree_edit",
            json!({ "label": "build", "ops": [
                { "op": "add_state", "name": "Idle", "parent": "Root" },
                { "op": "add_state", "name": "Alert", "parent": "Root" },
                { "op": "add_transition", "state": "Idle", "trigger": { "Event": "heard" }, "target": { "State": "Alert" } },
                { "op": "set_param", "state": "Alert", "key": "speed", "value": { "Float": 2.5 } },
            ] }),
        )
        .expect("edit");
        assert_eq!(built["valid"], true);
        assert_eq!(built["selected"], "Alert", "the last added state is selected");
        let state = call(
            &mut world,
            "editor.statetree_state",
            json!({ "ron": true, "palette": true }),
        )
        .expect("state");
        assert_eq!(state["states"][0]["name"], "Root");
        assert_eq!(state["states"][1]["parent"], "Root");
        assert_eq!(state["states"][1]["depth"], 1);
        assert_eq!(state["dirty"], true);
        assert_eq!(state["palette"]["triggers"].as_array().map(Vec::len), Some(5));
        let saved = call(&mut world, "editor.statetree_save", json!({})).expect("save");
        assert_eq!(
            (saved["valid"].clone(), saved["saves"].clone()),
            (json!(true), json!(1))
        );
        let before = world.resource::<StEditor>().doc.clone();
        let mut fresh = World::new();
        call(
            &mut fresh,
            "editor.statetree_open",
            json!({ "path": path.display().to_string() }),
        )
        .expect("reopen");
        assert_eq!(fresh.resource::<StEditor>().doc, before);
        let again = call(&mut fresh, "editor.statetree_state", json!({ "ron": true })).expect("state");
        assert_eq!(again["ron"], state["ron"]);
        assert_eq!(again["dirty"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_carry_their_codes_and_an_invalid_tree_is_not_saved() {
        let dir = regia_test_utils::unique_temp_dir("st-editor-agent-errors");
        let mut world = world();
        let path = dir.join("guard.statetree.ron");
        let err = call(&mut world, "editor.statetree_open", json!({})).expect_err("no path");
        assert_eq!(reason(&err), "missing_param");
        let err =
            call(&mut world, "editor.statetree_open", json!({ "path": "x.bt.ron" })).expect_err("bad path");
        assert_eq!(reason(&err), "bad_path");
        call(
            &mut world,
            "editor.statetree_open",
            json!({ "create": true, "path": path.display().to_string() }),
        )
        .expect("open");
        let err = call(&mut world, "editor.statetree_edit", json!({})).expect_err("nothing to do");
        assert_eq!(reason(&err), "missing_param");
        let err = call(
            &mut world,
            "editor.statetree_edit",
            json!({ "ops": [{ "op": "jump" }] }),
        )
        .expect_err("bad op");
        assert_eq!(reason(&err), "bad_ops");
        let err = call(
            &mut world,
            "editor.statetree_edit",
            json!({ "ops": [{ "op": "reparent_state", "name": "Root", "parent": "Root" }] }),
        )
        .expect_err("refused op");
        assert_eq!(reason(&err), "is_root");
        let err =
            call(&mut world, "editor.statetree_edit", json!({ "select": "Ghost" })).expect_err("unknown");
        assert_eq!(reason(&err), "unknown_state");
        let many: Vec<Value> = (0..=MAX_OPS)
            .map(|_| json!({ "op": "set_ambient", "tasks": [] }))
            .collect();
        let err = call(&mut world, "editor.statetree_edit", json!({ "ops": many })).expect_err("too many");
        assert_eq!(reason(&err), "too_many_ops");
        // A dangling target: the edit applies, the save is refused with the issues, nothing is written.
        let out = call(
            &mut world,
            "editor.statetree_edit",
            json!({ "ops": [{ "op": "add_transition", "state": "Root", "trigger": "Tick", "target": { "State": "Ghost" } }] }),
        )
        .expect("applies");
        assert_eq!(out["valid"], false);
        assert_eq!(out["issues"][0]["code"], "missing_target");
        let err = call(&mut world, "editor.statetree_save", json!({})).expect_err("invalid");
        assert_eq!(reason(&err), "invalid_tree");
        assert_eq!(
            err.data.as_ref().map(|d| d["issues"][0]["path"].clone()),
            Some(json!("states/Root/transitions/0/target"))
        );
        assert!(!path.exists(), "nothing was written");
        // One undo brings back a tree that saves.
        crate::command::undo_world(&mut world);
        call(&mut world, "editor.statetree_save", json!({})).expect("saves");
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The palette lists the `MoveTo` task `AiPlugin` registers (ADR-8293).
    #[test]
    fn the_palette_lists_the_move_to_task() {
        let dir = regia_test_utils::unique_temp_dir("st-editor-move-to");
        let mut world = world();
        let mut registry = regia_ai::BtTaskRegistry::default();
        regia_ai::path_provider::register_move_to(&mut registry);
        world.insert_resource(registry);
        let path = dir.join("go.statetree.ron");
        call(
            &mut world,
            "editor.statetree_open",
            json!({ "create": true, "path": path.display().to_string() }),
        )
        .expect("open");
        let state = call(&mut world, "editor.statetree_state", json!({ "palette": true })).expect("state");
        assert_eq!(state["palette"]["tasks"], json!([regia_ai::MOVE_TO_TASK]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
