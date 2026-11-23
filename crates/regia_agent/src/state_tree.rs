// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for state trees (`docs/specs/38-state-trees.md` § 7,
//! `book/agents/state-tree.md`): `statetree.compile`, `statetree.control`, `statetree.reload`
//! and `statetree.debug`. Reading and steering a running tree through `scene.get_entity` /
//! `scene.set_property` on `StateTreeStatus` and `StateTreeInbox` is unchanged. The file methods
//! `ai.statetree_validate` and `ai.statetree_edit` (ADR-3342, `book/agents/state-tree-editor.md`)
//! check and edit `.statetree.ron` files through `regia_ai::state_tree_edit`.
//!
//! Errors are `F0002` with `data.reason`; compile failures use the reasons of
//! `regia_ai::state_tree::StateTreeError::reason`.

use regia_ai::state_tree::{ReloadOutcome, StateTree, StateTreeDef, StateTreeError, StateTreeStatus};
use regia_ai::state_tree_edit::{self, StIssue, StOp, StSeverity, ValidateOptions};
use regia_ai::{BtContext, BtTaskRegistry};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn missing(param: &str) -> RpcError {
    refused(
        "missing_param",
        format!("`{param}` is required"),
        "see book/agents/state-tree.md for the parameters",
    )
}

/// A compile error as the protocol shows it.
fn error_json(err: &StateTreeError) -> Value {
    let mut out = json!({ "reason": err.reason(), "detail": err.to_string() });
    if let (Some(state), Some(map)) = (err.state(), out.as_object_mut()) {
        map.insert("state".into(), json!(state));
    }
    out
}

fn compile_refused(err: &StateTreeError) -> RpcError {
    refused(
        err.reason(),
        err.to_string(),
        "fix the definition; see book/agents/state-tree.md",
    )
}

/// Reads a project-relative text file (`regia_core::project_path` rules).
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

/// Parses and compiles `text` with includes resolved through the project files.
fn compile_text(world: &World, text: &str, includes: bool) -> Result<StateTree, StateTreeError> {
    let mut def = StateTreeDef::from_ron(text)?;
    if !includes {
        def.includes.clear();
    }
    let empty = BtTaskRegistry::default();
    let registry = world.get_resource::<BtTaskRegistry>().unwrap_or(&empty);
    StateTree::compile_with(&def, registry, &|path| read_project_text(world, path))
}

fn entity_field(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| missing("entity"))?;
    let entity = Entity::try_from_bits(bits).ok_or_else(entity_not_found)?;
    if world.get_entity(entity).is_err() {
        return Err(entity_not_found());
    }
    Ok(entity)
}

fn entity_not_found() -> RpcError {
    refused(
        "entity_not_found",
        "no such entity",
        "use scene.query to find the agent entity",
    )
}

fn no_state_tree() -> RpcError {
    refused(
        "no_state_tree",
        "the entity has no StateTree",
        "attach a compiled regia_ai::StateTree to the agent entity",
    )
}

/// Runs `f` on the entity's tree with the tree taken out of the world (the same take-run-reinsert
/// the tick system uses), then refreshes its `StateTreeStatus`.
fn with_tree<R>(
    world: &mut World,
    entity: Entity,
    f: impl FnOnce(&mut StateTree, &mut BtContext<'_>) -> R,
) -> Result<R, RpcError> {
    let mut tree = world
        .get_entity_mut(entity)
        .ok()
        .and_then(|mut e| e.take::<StateTree>())
        .ok_or_else(no_state_tree)?;
    let dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f32());
    let result = f(&mut tree, &mut BtContext { world, entity, dt });
    let status = StateTreeStatus::of(&tree);
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert((tree, status));
    }
    Ok(result)
}

/// `statetree.compile`: parses and compiles a definition (from `text` or a project `path`)
/// without building an entity. Never mutates the world.
pub fn compile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = source_text(world, params)?;
    let includes = crate::params::opt_bool(params, "includes")?.unwrap_or(true);
    Ok(match compile_text(world, &text, includes) {
        Ok(tree) => json!({ "ok": true, "states": tree.def().states.len(), "errors": [] }),
        Err(err) => json!({ "ok": false, "states": 0, "errors": [error_json(&err)] }),
    })
}

/// `statetree.control`: `pause`, `resume` or `restart` the tree on `entity`.
pub fn control(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_field(world, params)?;
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("op"))?;
    if !matches!(op, "pause" | "resume" | "restart") {
        return Err(refused(
            "bad_op",
            format!("unknown op {op:?}"),
            "op is one of pause, resume, restart",
        ));
    }
    with_tree(world, entity, |tree, ctx| {
        match op {
            "pause" => tree.pause(),
            "resume" => tree.resume(),
            _ => tree.restart(ctx),
        }
        json!({ "status": format!("{:?}", tree.status()), "paused": tree.is_paused() })
    })
}

/// `statetree.reload`: compiles new text and hot-swaps it into the entity's tree. A definition
/// that does not compile leaves the running tree untouched.
pub fn reload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_field(world, params)?;
    let text = source_text(world, params)?;
    if world.get::<StateTree>(entity).is_none() {
        return Err(no_state_tree());
    }
    let next = compile_text(world, &text, true).map_err(|err| compile_refused(&err))?;
    let outcome = with_tree(world, entity, |tree, ctx| tree.reload(next, ctx))?;
    Ok(json!({
        "outcome": match outcome {
            ReloadOutcome::Kept => "kept",
            ReloadOutcome::Restarted => "restarted",
        }
    }))
}

#[cfg(debug_assertions)]
fn name_list(params: &Value, field: &str) -> Result<Option<Vec<String>>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an array of state names"))),
        Some(_) => Err(RpcError::invalid_params(format!(
            "`{field}` must be an array of state names"
        ))),
    }
}

/// `statetree.debug` (dev builds): sets the states to skip and the states that pause the tree
/// when entered. Omitted lists keep their current value.
#[cfg(debug_assertions)]
pub fn debug(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_ai::StateTreeDebug;
    let entity = entity_field(world, params)?;
    let disabled = name_list(params, "disabled")?;
    let pause = name_list(params, "pause_on_enter")?;
    let tree = world.get::<StateTree>(entity).ok_or_else(no_state_tree)?;
    for name in disabled.iter().chain(pause.iter()).flatten() {
        if !tree.has_state(name) {
            return Err(refused(
                "state_not_found",
                format!("the tree has no state named {name:?}"),
                "use the names in StateTreeStatus.active_path or the definition",
            ));
        }
    }
    let (cur_disabled, cur_pause) = {
        let (d, p) = tree.debug_lists();
        (d.to_vec(), p.to_vec())
    };
    let disabled = disabled.unwrap_or(cur_disabled);
    let pause = pause.unwrap_or(cur_pause);
    if let Some(mut tree) = world.get_mut::<StateTree>(entity) {
        tree.set_debug(&disabled, &pause);
    }
    world.entity_mut(entity).insert(StateTreeDebug {
        disabled: disabled.clone(),
        pause_on_enter: pause.clone(),
    });
    Ok(json!({ "disabled": disabled, "pause_on_enter": pause }))
}

/// `statetree.debug` in a shipping build: always `dev_only`.
#[cfg(not(debug_assertions))]
pub fn debug(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Err(refused(
        "dev_only",
        "statetree.debug is available in development builds only",
        "run a debug build",
    ))
}

// -- File methods: `ai.statetree_validate`, `ai.statetree_edit` (ADR-3342) --------------------------

/// Most operations one `ai.statetree_edit` call applies.
pub const MAX_EDIT_OPS: usize = 512;

/// A project-relative `.statetree.ron` path, confined to the project (no `..`, no root, no symlink
/// out). Answers the relative text and the full path.
fn resolve_file(world: &World, params: &Value) -> Result<(String, std::path::PathBuf), RpcError> {
    let rel = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("path"))?;
    if regia_core::project_path::check_relative(rel).is_err() || !rel.ends_with(".statetree.ron") {
        return Err(refused(
            "bad_path",
            format!("{rel:?} is not a project-relative .statetree.ron path"),
            "use a path like assets/ai/guard.statetree.ron (no `..`)",
        ));
    }
    // ADR-8798: the shared root (asset server, then `AgentSettings::project_dir`, then the working
    // directory only when neither is set). The deepest existing ancestor (the file itself when it
    // exists), symlinks resolved, must lie under it; a root or an ancestor that cannot be resolved,
    // or a dangling symlink, is refused (the old check let the path through when the parent did not
    // canonicalize).
    let root = crate::req_path::project_root(world);
    let full = regia_core::project_path::resolve_write(&root, rel).map_err(|e| {
        refused(
            "bad_path",
            format!("{rel} leads outside the project ({e})"),
            "keep the file inside the project folder",
        )
    })?;
    Ok((rel.to_string(), full))
}

fn load_def(full: &std::path::Path, rel: &str) -> Result<StateTreeDef, RpcError> {
    let text = std::fs::read_to_string(full).map_err(|_| {
        refused(
            "not_found",
            format!("{rel}: cannot be read"),
            "check the path with assets.list, or pass create: true to ai.statetree_edit",
        )
    })?;
    parse_def(&text, rel)
}

fn parse_def(text: &str, label: &str) -> Result<StateTreeDef, RpcError> {
    StateTreeDef::from_ron(text).map_err(|e| {
        refused(
            "parse_failed",
            format!("{label}: {e}"),
            "the text is not a valid .statetree.ron; see docs/formats/statetree-ron.md",
        )
    })
}

/// Writes `text` to `path` through a temporary file and a rename, so a reader never sees half a file.
fn write_atomic(path: &std::path::Path, text: &str) -> Result<(), RpcError> {
    let fail = |e: std::io::Error| {
        refused(
            "write_failed",
            format!("cannot write {}: {e}", path.display()),
            "check the folder exists and is writable",
        )
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    let tmp = path.with_extension("ron.new");
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    crate::req_path::write_new_temp(&tmp, text.as_bytes()).map_err(fail)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        fail(e)
    })
}

/// The issues as the protocol shows them.
pub fn issues_json(issues: &[StIssue]) -> Value {
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

/// Validates `def` against the world's registered tasks (skipped while none is registered, as in an
/// editor with no game loaded; `tasks_checked` says so) with includes read from the project.
fn check_def(world: &World, def: &StateTreeDef, self_path: Option<&str>) -> (Vec<StIssue>, bool) {
    let registry = world.get_resource::<BtTaskRegistry>().filter(|r| !r.is_empty());
    let load = |path: &str| read_project_text(world, path);
    let issues = state_tree_edit::validate(
        def,
        ValidateOptions {
            registry,
            load: Some(&load),
            self_path,
        },
    );
    (issues, registry.is_some())
}

/// `ai.statetree_validate`: every problem of a `.statetree.ron` (or of `text`), each with a code and
/// a path. Never mutates anything.
pub fn statetree_validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // A wrong-typed `text` is refused, not skipped for the file (ADR-2622).
    let (label, def) = if let Some(text) = crate::params::opt_str(params, "text")? {
        (None, parse_def(text, "text")?)
    } else {
        let (rel, full) = resolve_file(world, params)?;
        let def = load_def(&full, &rel)?;
        (Some(rel), def)
    };
    let (issues, tasks_checked) = check_def(world, &def, label.as_deref());
    let errors = issues.iter().filter(|i| i.severity == StSeverity::Error).count();
    Ok(json!({
        "path": label,
        "ok": errors == 0,
        "errors": errors,
        "warnings": issues.len() - errors,
        "tasks_checked": tasks_checked,
        "issues": issues_json(&issues),
    }))
}

/// `ai.statetree_edit`: applies `ops` to a `.statetree.ron` all or nothing, validates, and saves the
/// canonical text atomically (unless `save` is false). A result with errors is refused with
/// `invalid_tree` (the issues are in `data.issues`) and the file is not written.
pub fn statetree_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (rel, full) = resolve_file(world, params)?;
    let ops_json = params
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| missing("ops"))?;
    if ops_json.len() > MAX_EDIT_OPS {
        return Err(refused(
            "too_many_ops",
            format!("{} operations; the limit is {MAX_EDIT_OPS}", ops_json.len()),
            "split the edit into several calls",
        ));
    }
    let mut ops = Vec::with_capacity(ops_json.len());
    for (i, op) in ops_json.iter().enumerate() {
        ops.push(serde_json::from_value::<StOp>(op.clone()).map_err(|e| {
            refused(
                "bad_ops",
                format!("op {i}: {e}"),
                "each op is { \"op\": add_state | remove_state | rename_state | reparent_state | set_root | \
                 set_state | set_tasks | set_ambient | set_param | add_transition | remove_transition | \
                 set_transition | move_transition | add_include | remove_include | set_include | set_tree, ... }",
            )
        })?);
    }
    // Wrong-typed flags are refused, not read as their defaults (ADR-2622).
    let create = crate::params::opt_bool(params, "create")?.unwrap_or(false);
    let save = crate::params::opt_bool(params, "save")?.unwrap_or(true);
    let mut def = if create && !full.exists() {
        StateTreeDef::new("")
    } else {
        load_def(&full, &rel)?
    };
    state_tree_edit::apply_ops(&mut def, &ops).map_err(|e| {
        refused(
            e.error.code,
            format!("op {}: {}", e.op_index, e.error.message),
            "nothing was changed; fix the op and call again",
        )
    })?;
    let text = state_tree_edit::to_canonical_ron(&def)
        .map_err(|e| refused(e.code, e.message, "numbers must be finite"))?;
    let (issues, tasks_checked) = check_def(world, &def, Some(&rel));
    let valid = state_tree_edit::is_valid(&issues);
    if save && !valid {
        let mut error = refused(
            "invalid_tree",
            format!(
                "{rel}: the edit leaves {} problem(s); the file was not written",
                issues.iter().filter(|i| i.severity == StSeverity::Error).count()
            ),
            "fix the problems in data.issues (or pass save: false to preview)",
        );
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("issues".into(), issues_json(&issues));
        }
        return Err(error);
    }
    if save {
        write_atomic(&full, &text)?;
    }
    Ok(json!({
        "path": rel,
        "saved": save,
        "valid": valid,
        "tasks_checked": tasks_checked,
        "issues": issues_json(&issues),
        "states": def.states.len(),
        "root": def.root,
        "ron": text,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ai::Blackboard;

    /// ADR-8798: `resolve_file` joins `AgentSettings::project_dir` when no asset server is set, takes
    /// a new nested path under it, and refuses a root it cannot resolve and a symlink (dangling, or a
    /// folder leading out) instead of letting the path through when a parent does not canonicalize.
    #[test]
    fn resolve_file_uses_the_agent_project_and_refuses_what_it_cannot_check() {
        let base = regia_test_utils::unique_temp_dir("statetree-resolve");
        let (project, outside) = (base.join("project"), base.join("outside"));
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&outside).expect("outside");
        let with_dir = |dir: std::path::PathBuf| {
            let mut world = World::new();
            world.insert_resource(crate::AgentSettings {
                project_dir: Some(dir),
            });
            world
        };
        let world = with_dir(project.clone());
        let (_, full) =
            resolve_file(&world, &json!({ "path": "ai/new/guard.statetree.ron" })).expect("nested new path");
        assert_eq!(full, project.join("ai/new/guard.statetree.ron"));
        let missing = with_dir(base.join("no-such-project"));
        let error =
            resolve_file(&missing, &json!({ "path": "guard.statetree.ron" })).expect_err("unresolvable root");
        assert_eq!(
            error.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("bad_path"))
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.join("gone.statetree.ron"),
                project.join("dangling.statetree.ron"),
            )
            .expect("dangling link");
            assert!(resolve_file(&world, &json!({ "path": "dangling.statetree.ron" })).is_err());
            std::os::unix::fs::symlink(&outside, project.join("escape")).expect("dir link");
            assert!(resolve_file(&world, &json!({ "path": "escape/deeper/x.statetree.ron" })).is_err());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    const TREE: &str = r#"(
        format_version: 2,
        root: "Root",
        states: [
            (name: "Root", children: ["Idle", "Alert"]),
            (name: "Idle", transitions: [(trigger: Event("heard"), target: State("Alert"))]),
            (name: "Alert"),
        ],
    )"#;

    fn world_with_tree() -> (World, Entity) {
        let mut world = World::new();
        world.init_resource::<BtTaskRegistry>();
        let tree = compile_text(&world, TREE, true).expect("compiles");
        let entity = world.spawn((Blackboard::default(), tree)).id();
        (world, entity)
    }

    fn reason(err: &RpcError) -> String {
        assert_eq!(err.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0002")));
        err.data
            .as_ref()
            .map(|d| d["reason"].as_str().unwrap_or("").to_string())
            .unwrap_or_default()
    }

    #[test]
    fn compile_reports_ok_and_each_error_reason() {
        let (mut world, _) = world_with_tree();
        let ok = compile(&mut world, &json!({ "text": TREE })).expect("compiles");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["states"], 3);
        let bad = compile(
            &mut world,
            &json!({ "text": "(format_version: 2, root: \"R\", states: [])" }),
        )
        .expect("answers");
        assert_eq!(bad["ok"], false);
        assert_eq!(bad["errors"][0]["reason"], "empty");
        let parse = compile(&mut world, &json!({ "text": "nonsense (" })).expect("answers");
        assert_eq!(parse["errors"][0]["reason"], "parse");
        let version = compile(
            &mut world,
            &json!({ "text": "(format_version: 9, root: \"R\", states: [])" }),
        )
        .expect("answers");
        assert_eq!(version["errors"][0]["reason"], "unsupported_version");
        let task = compile(
            &mut world,
            &json!({ "text": "(format_version: 2, root: \"R\", states: [(name: \"R\", tasks: [\"x::Nope\"])])" }),
        )
        .expect("answers");
        assert_eq!(task["errors"][0]["reason"], "unknown_task");
        assert_eq!(task["errors"][0]["state"], "R");
        let include = compile(
            &mut world,
            &json!({ "text": "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"missing.statetree.ron\")], states: [(name: \"R\")])" }),
        )
        .expect("answers");
        assert_eq!(include["errors"][0]["reason"], "include_missing");
        let skipped = compile(
            &mut world,
            &json!({ "includes": false, "text": "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"missing.statetree.ron\")], states: [(name: \"R\")])" }),
        )
        .expect("answers");
        assert_eq!(skipped["ok"], true);
    }

    /// Spec 38 § 7 / § 8 test 11: every compile reason of § 4 reaches `statetree.compile` as its
    /// snake_case `reason` (the six of `compile_reports_ok_and_each_error_reason` plus these).
    #[test]
    fn compile_answers_every_remaining_compile_reason() {
        let (mut world, _) = world_with_tree();
        let deep = format!("{}Key(\"k\"){}", "Not(".repeat(20), ")".repeat(20));
        let many: Vec<String> = (0..4097).map(|i| format!("(name: \"s{i}\")")).collect();
        let cases: Vec<(String, &str)> = vec![
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\"), (name: \"\")])".into(),
                "empty_name",
            ),
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\"), (name: \"R\")])".into(),
                "duplicate_name",
            ),
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\", children: [\"Ghost\"])])".into(),
                "unknown_state",
            ),
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\", children: [\"A\", \"A\"]), (name: \"A\")])"
                    .into(),
                "multiple_parents",
            ),
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\"), (name: \"Lost\")])".into(),
                "unreachable",
            ),
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\", children: [\"A\"]), (name: \"A\", transitions: [(trigger: Tick, target: Next)])])"
                    .into(),
                "no_next_sibling",
            ),
            (
                format!("(format_version: 2, root: \"R\", states: [(name: \"R\", enter: {deep})])"),
                "condition_too_deep",
            ),
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\", transitions: [(trigger: Tick, hold: -1.0, target: Stay)])])"
                    .into(),
                "bad_value",
            ),
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\", transitions: [(trigger: Event(\"e\"), hold: 1.0, target: Stay)])])"
                    .into(),
                "hold_on_event",
            ),
            (
                "(format_version: 2, root: \"R\", states: [(name: \"R\", transitions: [(trigger: Tick, target: Parent)])])"
                    .into(),
                "parent_of_root",
            ),
            (
                format!("(format_version: 2, root: \"s0\", states: [{}])", many.join(", ")),
                "too_many_states",
            ),
        ];
        for (text, expected) in cases {
            let out = compile(&mut world, &json!({ "text": text })).expect("answers");
            assert_eq!(out["ok"], false, "{expected}");
            assert_eq!(out["errors"][0]["reason"], expected, "{text:.120}");
        }

        // The include reasons that need files: a cycle and a chain five deep, read through the
        // project asset root.
        let root = regia_test_utils::unique_temp_dir("agent-statetree-include-reasons");
        let file = |next: &str| {
            format!(
                "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"{next}\")], states: [(name: \"R\", children: [\"p/R\"])])"
            )
        };
        std::fs::write(root.join("a.statetree.ron"), file("b.statetree.ron")).expect("a");
        std::fs::write(root.join("b.statetree.ron"), file("a.statetree.ron")).expect("b");
        for n in 1..=5 {
            let text = if n < 5 {
                file(&format!("d{}.statetree.ron", n + 1))
            } else {
                "(format_version: 2, root: \"R\", states: [(name: \"R\")])".to_string()
            };
            std::fs::write(root.join(format!("d{n}.statetree.ron")), text).expect("chain");
        }
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: regia_asset_pipeline::host_platform().to_string(),
            content_dir: None,
        })
        .expect("opens");
        world.insert_resource(server);
        let cycle = compile(&mut world, &json!({ "path": "a.statetree.ron" })).expect("answers");
        assert_eq!(cycle["errors"][0]["reason"], "include_cycle");
        let deep = compile(&mut world, &json!({ "text": file("d1.statetree.ron") })).expect("answers");
        assert_eq!(deep["errors"][0]["reason"], "include_depth");
        let four = compile(&mut world, &json!({ "text": file("d2.statetree.ron") })).expect("answers");
        assert_eq!(four["ok"], true, "four deep is allowed: {four}");
        std::fs::remove_dir_all(root).ok();
    }

    /// The `statetree.reload` and `statetree.debug` refusals the other tests leave out.
    #[test]
    fn reload_and_debug_refuse_missing_and_unknown_entities() {
        let (mut world, entity) = world_with_tree();
        let plain = world.spawn(Blackboard::default()).id();
        let gone = world.spawn_empty().id();
        world.despawn(gone);

        let err = reload(&mut world, &json!({ "text": TREE })).expect_err("no entity");
        assert_eq!(reason(&err), "missing_param");
        let err = reload(&mut world, &json!({ "entity": gone.to_bits(), "text": TREE })).expect_err("gone");
        assert_eq!(reason(&err), "entity_not_found");
        let err =
            reload(&mut world, &json!({ "entity": plain.to_bits(), "text": TREE })).expect_err("no tree");
        assert_eq!(reason(&err), "no_state_tree");
        let err = reload(&mut world, &json!({ "entity": entity.to_bits() })).expect_err("no text");
        assert_eq!(reason(&err), "missing_param");

        #[cfg(debug_assertions)]
        {
            let err = debug(&mut world, &json!({ "disabled": [] })).expect_err("no entity");
            assert_eq!(reason(&err), "missing_param");
            let err = debug(&mut world, &json!({ "entity": gone.to_bits() })).expect_err("gone");
            assert_eq!(reason(&err), "entity_not_found");
        }
        #[cfg(not(debug_assertions))]
        {
            let err = debug(&mut world, &json!({ "entity": entity.to_bits() })).expect_err("shipping");
            assert_eq!(reason(&err), "dev_only");
        }
    }

    #[test]
    fn compile_needs_text_or_a_confined_path() {
        let (mut world, _) = world_with_tree();
        let err = compile(&mut world, &json!({})).expect_err("missing");
        assert_eq!(reason(&err), "missing_param");
        let err = compile(&mut world, &json!({ "path": "../secret.statetree.ron" })).expect_err("escapes");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let err = compile(&mut world, &json!({ "path": "nope.statetree.ron" })).expect_err("absent");
        assert_eq!(err.code, protocol::NOT_FOUND);
    }

    #[test]
    fn compile_reads_a_project_file() {
        let root = regia_test_utils::unique_temp_dir("agent-statetree-compile");
        std::fs::write(root.join("guard.statetree.ron"), TREE).expect("writes");
        let mut world = World::new();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: regia_asset_pipeline::host_platform().to_string(),
            content_dir: None,
        })
        .expect("opens");
        world.insert_resource(server);
        let ok = compile(&mut world, &json!({ "path": "guard.statetree.ron" })).expect("compiles");
        assert_eq!(ok["ok"], true);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn control_pauses_resumes_and_restarts() {
        let (mut world, entity) = world_with_tree();
        let id = entity.to_bits();
        let paused = control(&mut world, &json!({ "entity": id, "op": "pause" })).expect("pauses");
        assert_eq!(paused["paused"], true);
        assert!(world.get::<StateTreeStatus>(entity).expect("status").paused);
        let resumed = control(&mut world, &json!({ "entity": id, "op": "resume" })).expect("resumes");
        assert_eq!(resumed["paused"], false);
        let restarted = control(&mut world, &json!({ "entity": id, "op": "restart" })).expect("restarts");
        assert_eq!(restarted["status"], "NotStarted");
    }

    #[test]
    fn control_reports_each_reason() {
        let (mut world, entity) = world_with_tree();
        let id = entity.to_bits();
        let err = control(&mut world, &json!({ "op": "pause" })).expect_err("no entity");
        assert_eq!(reason(&err), "missing_param");
        let err = control(&mut world, &json!({ "entity": id })).expect_err("no op");
        assert_eq!(reason(&err), "missing_param");
        let err = control(&mut world, &json!({ "entity": id, "op": "stop" })).expect_err("bad op");
        assert_eq!(reason(&err), "bad_op");
        let err =
            control(&mut world, &json!({ "entity": 99_999_999_999u64, "op": "pause" })).expect_err("gone");
        assert_eq!(reason(&err), "entity_not_found");
        let bare = world.spawn_empty().id();
        let err =
            control(&mut world, &json!({ "entity": bare.to_bits(), "op": "pause" })).expect_err("no tree");
        assert_eq!(reason(&err), "no_state_tree");
    }

    #[test]
    fn reload_keeps_the_path_and_a_bad_definition_leaves_the_tree_running() {
        let (mut world, entity) = world_with_tree();
        let id = entity.to_bits();
        // Start the tree through the take-run-reinsert helper so it has an active path.
        with_tree(&mut world, entity, |tree, ctx| tree.tick(ctx)).expect("ticks");
        assert_eq!(
            world.get::<StateTreeStatus>(entity).expect("status").active_path,
            ["Root", "Idle"]
        );

        let kept = reload(&mut world, &json!({ "entity": id, "text": TREE })).expect("reloads");
        assert_eq!(kept["outcome"], "kept");

        let err = reload(
            &mut world,
            &json!({ "entity": id, "text": "(format_version: 2, root: \"Nope\", states: [(name: \"R\")])" }),
        )
        .expect_err("does not compile");
        assert_eq!(reason(&err), "unknown_state");
        assert_eq!(
            world.get::<StateTreeStatus>(entity).expect("status").active_path,
            ["Root", "Idle"]
        );

        let renamed = TREE.replace("Idle", "Calm");
        let restarted = reload(&mut world, &json!({ "entity": id, "text": renamed })).expect("reloads");
        assert_eq!(restarted["outcome"], "restarted");
        assert!(
            world
                .get::<StateTreeStatus>(entity)
                .expect("status")
                .active_path
                .is_empty()
        );

        let err = reload(&mut world, &json!({ "entity": id })).expect_err("no text");
        assert_eq!(reason(&err), "missing_param");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_sets_the_lists_and_names_the_unknown_state() {
        let (mut world, entity) = world_with_tree();
        let id = entity.to_bits();
        let set = debug(
            &mut world,
            &json!({ "entity": id, "disabled": ["Idle"], "pause_on_enter": ["Alert"] }),
        )
        .expect("sets");
        assert_eq!(set["disabled"], json!(["Idle"]));
        assert_eq!(set["pause_on_enter"], json!(["Alert"]));
        let kept = debug(&mut world, &json!({ "entity": id, "disabled": [] })).expect("clears one list");
        assert_eq!(kept["disabled"], json!([]));
        assert_eq!(kept["pause_on_enter"], json!(["Alert"]));
        let err = debug(&mut world, &json!({ "entity": id, "disabled": ["Nope"] })).expect_err("unknown");
        assert_eq!(reason(&err), "state_not_found");
        let err = debug(&mut world, &json!({ "entity": id, "disabled": "Idle" })).expect_err("not a list");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let bare = world.spawn_empty().id();
        let err = debug(&mut world, &json!({ "entity": bare.to_bits() })).expect_err("no tree");
        assert_eq!(reason(&err), "no_state_tree");
    }

    fn project_world(label: &str) -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(label);
        let mut world = World::new();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: regia_asset_pipeline::host_platform().to_string(),
            content_dir: None,
        })
        .expect("opens");
        world.insert_resource(server);
        (world, root)
    }

    /// What the build-up below must save as (the canonical layout, written by hand).
    const BUILT: &str = r#"(
    format_version: 2,
    root: "Root",
    states: [
        (
            name: "Root",
            children: ["Idle", "Alert"],
        ),
        (
            name: "Idle",
            transitions: [
                (trigger: Event("heard"), target: State("Alert")),
            ],
        ),
        (
            name: "Alert",
            enter_set: {"speed": Float(2.5)},
            transitions: [
                (trigger: Tick, priority: 1, hold: 0.5, target: State("Idle")),
            ],
        ),
    ],
)
"#;

    fn build_ops() -> Value {
        json!([
            { "op": "add_state", "name": "Root" },
            { "op": "add_state", "name": "Idle", "parent": "Root" },
            { "op": "add_state", "name": "Alert", "parent": "Root" },
            { "op": "add_transition", "state": "Idle", "trigger": { "Event": "heard" }, "target": { "State": "Alert" } },
            { "op": "add_transition", "state": "Alert", "trigger": "Tick", "priority": 1, "hold": 0.5, "target": { "State": "Idle" } },
            { "op": "set_param", "state": "Alert", "key": "speed", "value": { "Float": 2.5 } },
        ])
    }

    #[test]
    fn statetree_edit_creates_saves_the_canonical_text_and_it_compiles() {
        let (mut world, root) = project_world("agent-statetree-edit");
        let out = statetree_edit(
            &mut world,
            &json!({ "path": "ai/guard.statetree.ron", "create": true, "ops": build_ops() }),
        )
        .expect("edits");
        assert_eq!(out["saved"], true);
        assert_eq!(out["valid"], true);
        assert_eq!(out["states"], 3);
        let written = std::fs::read_to_string(root.join("ai/guard.statetree.ron")).expect("written");
        assert_eq!(written, BUILT);
        assert_eq!(out["ron"], BUILT);
        assert!(
            !root.join("ai/guard.statetree.new").exists()
                && !root.join("ai/guard.statetree.ron.new").exists()
        );
        // It compiles, and a rename of a state that transitions point at keeps it compiling.
        compile_text(&world, &written, true).expect("compiles");
        let renamed = statetree_edit(
            &mut world,
            &json!({ "path": "ai/guard.statetree.ron", "ops": [{ "op": "rename_state", "name": "Alert", "to": "Alarm" }] }),
        )
        .expect("renames");
        assert_eq!(renamed["valid"], true);
        let written = std::fs::read_to_string(root.join("ai/guard.statetree.ron")).expect("written");
        assert_eq!(written, BUILT.replace("\"Alert\"", "\"Alarm\""));
        compile_text(&world, &written, true).expect("compiles after the rename");
        // The validator agrees.
        let checked =
            statetree_validate(&mut world, &json!({ "path": "ai/guard.statetree.ron" })).expect("validates");
        assert_eq!(
            (checked["ok"].clone(), checked["errors"].clone()),
            (json!(true), json!(0))
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_edit_that_leaves_errors_is_refused_and_the_file_is_not_written() {
        let (mut world, root) = project_world("agent-statetree-invalid");
        statetree_edit(
            &mut world,
            &json!({ "path": "guard.statetree.ron", "create": true, "ops": build_ops() }),
        )
        .expect("edits");
        let file = root.join("guard.statetree.ron");
        let before = std::fs::read(&file).expect("reads");
        let err = statetree_edit(
            &mut world,
            &json!({ "path": "guard.statetree.ron", "ops": [
                { "op": "add_transition", "state": "Idle", "trigger": "Tick", "target": { "State": "Ghost" } },
            ] }),
        )
        .expect_err("refused");
        assert_eq!(reason(&err), "invalid_tree");
        let data = err.data.as_ref().expect("data");
        assert_eq!(data["issues"][0]["code"], "missing_target");
        assert_eq!(data["issues"][0]["path"], "states/Idle/transitions/1/target");
        assert_eq!(data["issues"][0]["state"], "Idle");
        assert_eq!(
            std::fs::read(&file).expect("reads"),
            before,
            "the file is untouched"
        );
        // save: false previews the result without writing or refusing.
        let preview = statetree_edit(
            &mut world,
            &json!({ "path": "guard.statetree.ron", "save": false, "ops": [
                { "op": "add_state", "name": "Lost" },
            ] }),
        )
        .expect("previews");
        assert_eq!(preview["valid"], false);
        assert_eq!(preview["issues"][0]["code"], "unreachable_state");
        assert_eq!(std::fs::read(&file).expect("reads"), before);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn statetree_edit_and_validate_report_each_reason() {
        let (mut world, root) = project_world("agent-statetree-reasons");
        let edit = |world: &mut World, params: Value| statetree_edit(world, &params).expect_err("refused");
        assert_eq!(reason(&edit(&mut world, json!({ "ops": [] }))), "missing_param");
        assert_eq!(
            reason(&edit(&mut world, json!({ "path": "guard.statetree.ron" }))),
            "missing_param"
        );
        assert_eq!(
            reason(&edit(
                &mut world,
                json!({ "path": "../out.statetree.ron", "ops": [] })
            )),
            "bad_path"
        );
        assert_eq!(
            reason(&edit(&mut world, json!({ "path": "guard.bt.ron", "ops": [] }))),
            "bad_path"
        );
        assert_eq!(
            reason(&edit(
                &mut world,
                json!({ "path": "nope.statetree.ron", "ops": [] })
            )),
            "not_found"
        );
        assert_eq!(
            reason(&edit(
                &mut world,
                json!({ "path": "guard.statetree.ron", "create": true, "ops": [{ "op": "fly" }] })
            )),
            "bad_ops"
        );
        let many: Vec<Value> = (0..=MAX_EDIT_OPS)
            .map(|_| json!({ "op": "set_ambient", "tasks": [] }))
            .collect();
        assert_eq!(
            reason(&edit(
                &mut world,
                json!({ "path": "guard.statetree.ron", "create": true, "ops": many })
            )),
            "too_many_ops"
        );
        // An op that does not fit names the edit code and writes nothing.
        let err = edit(
            &mut world,
            json!({ "path": "guard.statetree.ron", "create": true, "ops": [
                { "op": "add_state", "name": "R" },
                { "op": "add_state", "name": "R" },
            ] }),
        );
        assert_eq!(reason(&err), "duplicate_name");
        assert!(!root.join("guard.statetree.ron").exists());
        std::fs::write(root.join("broken.statetree.ron"), "(nonsense").expect("writes");
        assert_eq!(
            reason(&edit(
                &mut world,
                json!({ "path": "broken.statetree.ron", "ops": [] })
            )),
            "parse_failed"
        );
        let err = statetree_validate(&mut world, &json!({})).expect_err("no path");
        assert_eq!(reason(&err), "missing_param");
        let err = statetree_validate(&mut world, &json!({ "text": "(" })).expect_err("bad text");
        assert_eq!(reason(&err), "parse_failed");
        let listed = statetree_validate(
            &mut world,
            &json!({ "text": "(format_version: 2, root: \"R\", states: [(name: \"R\", children: [\"Ghost\"]), (name: \"Lost\")])" }),
        )
        .expect("answers");
        assert_eq!(listed["ok"], false);
        assert_eq!(listed["errors"], 2);
        assert_eq!(listed["tasks_checked"], false);
        std::fs::remove_dir_all(root).ok();
    }
}
