// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for behavior trees (`docs/specs/22-tier2.md` § T2.2, ADR-2408,
//! `book/agents/behavior-tree-editor.md`): `ai.bt_get`, `ai.bt_edit`, `ai.bt_validate` and
//! `ai.bt_live`. The first three work on `.bt.ron` files in the project; `ai.bt_live` reads a
//! ticking tree through the read-only [`regia_ai::BtDebugger`].
//!
//! Errors are `F0002` with `data.reason`; the reasons are lowercase and listed in the book page.

use std::path::{Path, PathBuf};

use regia_ai::bt_edit::{self, BtIssue, BtOp, BtSeverity};
use regia_ai::{BehaviorTree, BehaviorTreeStatus, BtDebugger, BtStatus, BtTaskRegistry};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most operations one `ai.bt_edit` call applies.
pub const MAX_OPS: usize = 512;

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
        "see book/agents/behavior-tree-editor.md for the parameters",
    )
}

/// A project-relative `.bt.ron` path, confined to the project (no `..`, no root, no symlink out).
fn resolve(world: &World, params: &Value) -> Result<(String, PathBuf), RpcError> {
    let rel = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("path"))?;
    let clean = regia_core::project_path::check_relative(rel);
    let Some(rel_path) = clean.as_deref().ok().filter(|_| rel.ends_with(".bt.ron")) else {
        return Err(refused(
            "bad_path",
            format!("{rel:?} is not a project-relative .bt.ron path"),
            "use a path like assets/ai/guard.bt.ron (no `..`)",
        ));
    };
    // ADR-8798: the shared root (asset server, then `AgentSettings::project_dir`, then the working
    // directory only when neither is set), never a second one.
    let root = crate::req_path::project_root(world);
    let full = root.join(rel_path);
    confine(&root, &full, rel)?;
    Ok((rel.to_string(), full))
}

/// Refuses a path whose deepest existing part (the file itself, else the nearest existing
/// ancestor, so a missing sub-folder chain is covered) resolves outside `root` through a symlink,
/// a dangling symlink, and a root that cannot be resolved (ADR-8798: the old walk allowed the path
/// when the root did not canonicalize).
fn confine(root: &Path, full: &Path, rel: &str) -> Result<(), RpcError> {
    regia_core::project_path::check_under_root(root, full).map_err(|e| {
        refused(
            "bad_path",
            format!("{rel} leads outside the project ({e})"),
            "keep the file inside the project folder",
        )
    })
}

fn load(full: &Path, rel: &str) -> Result<regia_ai::BehaviorTreeDef, RpcError> {
    let text = std::fs::read_to_string(full).map_err(|_| {
        refused(
            "not_found",
            format!("{rel}: cannot be read"),
            "check the path with assets.list, or pass create: true to ai.bt_edit",
        )
    })?;
    bt_edit::parse_bt_ron(&text).map_err(|e| {
        refused(
            "parse_failed",
            format!("{rel}: {e}"),
            "the file is not a valid .bt.ron; see docs/formats/bt-ron.md",
        )
    })
}

fn write_atomic(path: &Path, text: &str) -> Result<(), RpcError> {
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

fn issues_json(issues: &[BtIssue]) -> Value {
    json!(
        issues
            .iter()
            .map(|i| json!({
                "code": i.code,
                "severity": if i.severity == BtSeverity::Error { "error" } else { "warning" },
                "node": i.node,
                "message": i.message,
            }))
            .collect::<Vec<_>>()
    )
}

/// Checks `def` against the world's registered tasks (skipped while no task is registered, which
/// is the case in an editor with no game loaded; the answer says so).
fn check(world: &World, def: &regia_ai::BehaviorTreeDef) -> (Vec<BtIssue>, bool) {
    let registry = world.get_resource::<BtTaskRegistry>().filter(|r| !r.is_empty());
    (bt_edit::validate(def, registry), registry.is_some())
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
                }
                v
            })
            .collect::<Vec<_>>()
    )
}

fn summary(world: &World, def: &regia_ai::BehaviorTreeDef) -> serde_json::Map<String, Value> {
    let (issues, tasks_checked) = check(world, def);
    let mut out = serde_json::Map::new();
    out.insert("root".into(), json!(def.root));
    out.insert("nodes".into(), nodes_json(def));
    out.insert("valid".into(), json!(bt_edit::is_valid(&issues)));
    out.insert("tasks_checked".into(), json!(tasks_checked));
    out.insert("issues".into(), issues_json(&issues));
    out
}

/// `ai.bt_get`: reads a `.bt.ron` and answers its nodes, its issues and its canonical text.
pub fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (rel, full) = resolve(world, params)?;
    let def = load(&full, &rel)?;
    let mut out = summary(world, &def);
    out.insert("path".into(), json!(rel));
    out.insert(
        "ron".into(),
        json!(bt_edit::to_canonical_ron(&def).unwrap_or_default()),
    );
    Ok(Value::Object(out))
}

/// `ai.bt_validate`: lists every problem of a `.bt.ron` (or of `text`) with a stable code.
pub fn validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (label, def) = if let Some(text) = crate::params::opt_str(params, "text")? {
        let def = bt_edit::parse_bt_ron(text).map_err(|e| {
            refused(
                "parse_failed",
                format!("text: {e}"),
                "the text is not a valid .bt.ron; see docs/formats/bt-ron.md",
            )
        })?;
        (Value::Null, def)
    } else {
        let (rel, full) = resolve(world, params)?;
        let def = load(&full, &rel)?;
        (json!(rel), def)
    };
    let (issues, tasks_checked) = check(world, &def);
    let errors = issues.iter().filter(|i| i.severity == BtSeverity::Error).count();
    Ok(json!({
        "path": label,
        "ok": errors == 0,
        "errors": errors,
        "warnings": issues.len() - errors,
        "tasks_checked": tasks_checked,
        "issues": issues_json(&issues),
    }))
}

/// `ai.bt_edit`: applies `ops` to a `.bt.ron` all or nothing, validates, and saves the canonical
/// text (unless `save` is false). A tree with errors is saved too, so a build-up can pass through
/// invalid states; the answer carries the issues.
pub fn edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (rel, full) = resolve(world, params)?;
    let ops_json = params
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| missing("ops"))?;
    if ops_json.len() > MAX_OPS {
        return Err(refused(
            "too_many_ops",
            format!("{} operations; the limit is {MAX_OPS}", ops_json.len()),
            "split the edit into several calls",
        ));
    }
    let mut ops = Vec::with_capacity(ops_json.len());
    for (i, op) in ops_json.iter().enumerate() {
        ops.push(serde_json::from_value::<BtOp>(op.clone()).map_err(|e| {
            refused(
                "bad_ops",
                format!("op {i}: {e}"),
                "each op is { \"op\": add_node | remove_node | connect | disconnect | reorder | set_node | set_root, ... }",
            )
        })?);
    }
    let create = crate::params::opt_bool(params, "create")?.unwrap_or(false);
    let save = crate::params::opt_bool(params, "save")?.unwrap_or(true);
    let mut def = if create && !full.exists() {
        regia_ai::BehaviorTreeDef::default()
    } else {
        load(&full, &rel)?
    };
    let created = bt_edit::apply_ops(&mut def, &ops).map_err(|e| {
        refused(
            e.error.code,
            format!("op {}: {}", e.op_index, e.error.message),
            "nothing was changed; fix the op and call again",
        )
    })?;
    let text =
        bt_edit::to_canonical_ron(&def).map_err(|e| refused("bad_value", e, "seconds must be finite"))?;
    if save {
        write_atomic(&full, &text)?;
    }
    let mut out = summary(world, &def);
    out.insert("path".into(), json!(rel));
    out.insert("saved".into(), json!(save));
    out.insert("created".into(), json!(created));
    out.insert("ron".into(), json!(text));
    Ok(Value::Object(out))
}

fn status_name(status: BtStatus) -> &'static str {
    match status {
        BtStatus::Success => "success",
        BtStatus::Failure => "failure",
        BtStatus::Running => "running",
    }
}

/// `ai.bt_live`: the per-node state of a ticking tree. The first call attaches the read-only
/// debugger to the entity (from then on each tick is recorded); `detach: true` removes it.
pub fn live(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| missing("entity"))?;
    let not_found = || {
        refused(
            "entity_not_found",
            "no such entity",
            "use scene.query_entities with BehaviorTreeStatus to find the agent",
        )
    };
    let entity = Entity::try_from_bits(bits).ok_or_else(not_found)?;
    if world.get_entity(entity).is_err() {
        return Err(not_found());
    }
    if crate::params::opt_bool(params, "detach")?.unwrap_or(false) {
        let was = world
            .get_resource_mut::<BtDebugger>()
            .is_some_and(|mut d| d.detach(entity));
        return Ok(json!({ "entity": bits, "attached": false, "was_attached": was }));
    }
    let def = world
        .get::<BehaviorTree>(entity)
        .map(|t| t.def.clone())
        .ok_or_else(|| {
            refused(
                "no_behavior_tree",
                "the entity has no BehaviorTree",
                "attach a compiled regia_ai::BehaviorTree to the agent entity",
            )
        })?;
    let fresh = !world
        .get_resource::<BtDebugger>()
        .is_some_and(|d| d.watches(entity));
    BtDebugger::attach(world, entity);
    let recorded = world
        .resource::<BtDebugger>()
        .snapshot(entity)
        .cloned()
        .unwrap_or_default();
    // Before the first recorded tick, show what the tree last reported on its own.
    let (ticks, last, statuses, changed): (u64, BtStatus, Vec<BtStatus>, Vec<u64>) = if recorded.ticks == 0 {
        let reported = world.get::<BehaviorTreeStatus>(entity);
        let statuses = reported.map(|s| s.node_status.clone()).unwrap_or_default();
        (
            0,
            reported.map_or(BtStatus::Running, |s| s.last_status),
            statuses.clone(),
            vec![0; statuses.len()],
        )
    } else {
        (
            recorded.ticks,
            recorded.last_status,
            recorded.node_status.clone(),
            recorded.changed_at.clone(),
        )
    };
    let nodes: Vec<Value> = def
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let status = statuses.get(i).copied();
            json!({
                "index": i,
                "name": node.name,
                "kind": serde_json::to_value(node.kind).unwrap_or(Value::Null),
                "status": status.map(status_name),
                "running": status == Some(BtStatus::Running),
                "changed_at": changed.get(i).copied().unwrap_or(0),
            })
        })
        .collect();
    let running: Vec<usize> = statuses
        .iter()
        .enumerate()
        .filter(|(_, s)| **s == BtStatus::Running)
        .map(|(i, _)| i)
        .collect();
    Ok(json!({
        "entity": bits,
        "attached": true,
        "newly_attached": fresh,
        "ticks": ticks,
        "root": def.root,
        "last_status": status_name(last),
        "running": running,
        "nodes": nodes,
        "move_to": move_to_progress(world, entity),
    }))
}

/// The agent's `MoveTo` runs (ADR-8293), one object per progress key, sorted; `[]` when the tree
/// has no `MoveTo` leaf (or none has ticked yet).
fn move_to_progress(world: &World, entity: Entity) -> Value {
    let Some(status) = world.get::<regia_ai::MoveToStatus>(entity) else {
        return json!([]);
    };
    status
        .entries
        .iter()
        .map(|e| {
            json!({
                "key": e.key,
                "provider": e.provider,
                "state": e.state.name(),
                "crowd": e.crowd,
                "target": [e.target.x, e.target.y, e.target.z],
                "remaining": e.remaining,
                "waypoint": e.waypoint,
                "waypoints": e.waypoints,
                "replans": e.replans,
                "ticks": e.ticks,
                "error": e.error,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn confine_refuses_a_symlinked_folder_even_when_the_subfolder_is_missing() {
        let base = regia_test_utils::unique_temp_dir("bt-confine");
        let (root, outside) = (base.join("project"), base.join("outside"));
        std::fs::create_dir_all(root.join("assets")).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, root.join("assets/link")).expect("symlink");
        let bad = root.join("assets/link/new/x.bt.ron");
        assert!(super::confine(&root, &bad, "assets/link/new/x.bt.ron").is_err());
        let good = root.join("assets/new/x.bt.ron");
        assert!(super::confine(&root, &good, "assets/new/x.bt.ron").is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    use regia_ai::{BehaviorTreeBuilder, BtContext, BtTask, BtTaskRegistration};
    use regia_test_utils::unique_temp_dir;

    use super::*;

    #[derive(Default)]
    struct Patrol;
    impl BtTask for Patrol {
        fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
            BtStatus::Running
        }
    }
    impl BtTaskRegistration for Patrol {
        fn construct() -> Box<dyn BtTask> {
            Box::new(Patrol)
        }
        fn task_type_path() -> &'static str {
            "game::Patrol"
        }
    }

    fn reason(err: &RpcError) -> String {
        assert_eq!(err.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0002")));
        err.data
            .as_ref()
            .map(|d| d["reason"].as_str().unwrap_or("").to_string())
            .unwrap_or_default()
    }

    fn world_in(dir: &Path) -> World {
        let mut world = World::new();
        let mut registry = BtTaskRegistry::default();
        registry.register::<Patrol>();
        world.insert_resource(registry);
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: dir.to_path_buf(),
            platform: std::env::consts::OS.to_string(),
            content_dir: None,
        })
        .expect("opens");
        world.insert_resource(server);
        world
    }

    fn build_ops() -> Value {
        json!([
            { "op": "add_node", "kind": "selector", "name": "root" },
            { "op": "add_node", "kind": "leaf", "name": "patrol", "parent": 0, "task": "game::Patrol" },
            { "op": "add_node", "kind": "condition", "name": "sees", "parent": 0, "key": "enemy", "index": 0 },
        ])
    }

    #[test]
    fn edit_builds_saves_and_get_reads_back_the_same_tree() {
        let dir = unique_temp_dir("bt-agent");
        let mut world = world_in(&dir);
        let out = edit(
            &mut world,
            &json!({ "path": "ai/guard.bt.ron", "create": true, "ops": build_ops() }),
        )
        .expect("edits");
        assert_eq!(out["valid"], true);
        assert_eq!(out["created"], json!([0, 1, 2]));
        assert_eq!(out["nodes"][0]["children"], json!([2, 1]));
        let on_disk = std::fs::read_to_string(dir.join("ai/guard.bt.ron")).expect("saved");
        assert_eq!(on_disk, out["ron"].as_str().expect("text"));
        let got = get(&mut world, &json!({ "path": "ai/guard.bt.ron" })).expect("reads");
        assert_eq!(got["nodes"], out["nodes"]);
        assert_eq!(got["ron"], out["ron"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reactive_nodes_round_trip_through_agent_edits_and_report_type_errors() {
        let dir = unique_temp_dir("bt-agent-reactive");
        let mut world = world_in(&dir);
        let path = "ai/reactive.bt.ron";
        let out = edit(
            &mut world,
            &json!({ "path": path, "create": true, "ops": [
                { "op": "add_node", "kind": "selector", "name": "root" },
                { "op": "add_node", "kind": "compare", "parent": 0,
                  "key": "threat", "abort": "both", "compare": "ge",
                  "value_kind": "float", "literal": { "float": 0.8 } },
                { "op": "add_node", "kind": "state_tree", "parent": 0,
                  "task": "ai/chase.statetree.ron" }
            ] }),
        )
        .expect("edits new node kinds");
        assert_eq!(out["valid"], true, "{out}");
        let got = get(&mut world, &json!({ "path": path })).expect("reads v2");
        assert_eq!(got["nodes"], out["nodes"]);
        assert_eq!(got["nodes"][1]["abort"], "both");
        assert_eq!(got["nodes"][1]["op"], "ge");
        assert_eq!(
            got["nodes"][1]["literal"]["float"].as_f64().expect("float") as f32,
            0.8_f32
        );
        assert!(got["ron"].as_str().expect("RON").contains("format_version: 2"));

        let out = edit(
            &mut world,
            &json!({ "path": path, "ops": [
                { "op": "set_node", "node": 1, "compare": "lt",
                  "clear_literal": true, "rhs_key": "limit", "abort": "lower_priority" }
            ] }),
        )
        .expect("sets a key comparison");
        assert_eq!(out["valid"], true, "{out}");
        assert_eq!(out["nodes"][1]["literal"], Value::Null);
        assert_eq!(out["nodes"][1]["rhs_key"], "limit");
        assert_eq!(out["nodes"][1]["op"], "lt");
        let validation = validate(&mut world, &json!({ "path": path })).expect("validates v2");
        assert_eq!(validation["ok"], true, "{validation}");

        edit(
            &mut world,
            &json!({ "path": path, "ops": [
                { "op": "set_node", "node": 1, "rhs_key": "", "literal": { "bool": true } }
            ] }),
        )
        .expect("saves an invalid edit for diagnosis");
        let validation = validate(&mut world, &json!({ "path": path })).expect("reports mismatch");
        assert_eq!(validation["ok"], false);
        assert!(
            validation["issues"]
                .as_array()
                .expect("issues")
                .iter()
                .any(|issue| { issue["code"] == "type_mismatch" && issue["node"] == 1 })
        );
        drop(world);
        std::fs::remove_dir_all(&dir).expect("cleans project");
    }

    #[test]
    fn invalid_trees_report_codes_and_a_failed_batch_changes_nothing() {
        let dir = unique_temp_dir("bt-agent-bad");
        let mut world = world_in(&dir);
        edit(
            &mut world,
            &json!({ "path": "bad.bt.ron", "create": true, "ops": [
                { "op": "add_node", "kind": "selector" },
                { "op": "add_node", "kind": "leaf", "parent": 0, "task": "no::Such" },
                { "op": "add_node", "kind": "inverter", "parent": 0 },
            ] }),
        )
        .expect("a tree with errors is still saved");
        let v = validate(&mut world, &json!({ "path": "bad.bt.ron" })).expect("validates");
        assert_eq!(v["ok"], false);
        let codes: Vec<&str> = v["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|i| i["code"].as_str())
            .collect();
        assert!(
            codes.contains(&"unknown_task") && codes.contains(&"decorator_arity"),
            "{codes:?}"
        );
        let before = std::fs::read_to_string(dir.join("bad.bt.ron")).expect("saved");
        let err = edit(
            &mut world,
            &json!({ "path": "bad.bt.ron", "ops": [
                { "op": "set_node", "node": 0, "name": "renamed" },
                { "op": "connect", "parent": 1, "child": 2 },
            ] }),
        )
        .expect_err("refused");
        assert_eq!(reason(&err), "no_children_allowed");
        assert_eq!(
            std::fs::read_to_string(dir.join("bad.bt.ron")).expect("kept"),
            before
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_and_ops_are_checked() {
        let dir = unique_temp_dir("bt-agent-args");
        let mut world = world_in(&dir);
        for path in ["../x.bt.ron", "/etc/x.bt.ron", "x.ron", ""] {
            let err = get(&mut world, &json!({ "path": path })).expect_err("refused");
            assert_eq!(reason(&err), "bad_path", "{path}");
        }
        assert_eq!(
            reason(&get(&mut world, &json!({})).expect_err("m")),
            "missing_param"
        );
        assert_eq!(
            reason(&get(&mut world, &json!({ "path": "none.bt.ron" })).expect_err("m")),
            "not_found"
        );
        std::fs::write(dir.join("junk.bt.ron"), "(not ron").expect("writes");
        assert_eq!(
            reason(&get(&mut world, &json!({ "path": "junk.bt.ron" })).expect_err("m")),
            "parse_failed"
        );
        for ops in [
            json!([{ "op": "frobnicate" }]),
            json!([{ "op": "set_root" }]),
            json!([{ "op": "set_root", "node": 0, "x": 1 }]),
        ] {
            let err = edit(
                &mut world,
                &json!({ "path": "a.bt.ron", "create": true, "ops": ops }),
            )
            .expect_err("bad");
            assert_eq!(reason(&err), "bad_ops");
        }
        let many: Vec<Value> = (0..=MAX_OPS)
            .map(|_| json!({ "op": "set_root", "node": 0 }))
            .collect();
        let err = edit(
            &mut world,
            &json!({ "path": "a.bt.ron", "create": true, "ops": many }),
        )
        .expect_err("big");
        assert_eq!(reason(&err), "too_many_ops");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_accepts_text_and_get_skips_task_check_without_a_registry() {
        let mut world = World::new();
        let text =
            bt_edit::to_canonical_ron(&regia_ai::BehaviorTreeDef::leaf("l", "any::Task")).expect("writes");
        let v = validate(&mut world, &json!({ "text": text })).expect("validates");
        assert_eq!(v["ok"], true);
        assert_eq!(v["tasks_checked"], false, "no registered tasks: not checked");
        let err = validate(&mut world, &json!({ "text": "(oops" })).expect_err("bad");
        assert_eq!(reason(&err), "parse_failed");
    }

    #[test]
    fn live_attaches_then_follows_the_ticks() {
        let dir = unique_temp_dir("bt-agent-live");
        let mut world = world_in(&dir);
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("patrol", "game::Patrol");
        let root = b.sequence("root", vec![leaf]);
        let tree =
            BehaviorTree::compile(&b.build(root), world.resource::<BtTaskRegistry>()).expect("compiles");
        let agent = world.spawn(tree).id();
        let first = live(&mut world, &json!({ "entity": agent.to_bits() })).expect("attaches");
        assert_eq!(first["newly_attached"], true);
        assert_eq!(first["ticks"], 0);
        // Tick by hand the way the plugin does: take, tick, record.
        for _ in 0..3 {
            let mut tree = world.entity_mut(agent).take::<BehaviorTree>().expect("tree");
            let mut ctx = BtContext {
                world: &mut world,
                entity: agent,
                dt: 1.0 / 60.0,
            };
            tree.tick(&mut ctx);
            world.resource_mut::<BtDebugger>().record(agent, &tree);
            world.entity_mut(agent).insert(tree);
        }
        let out = live(&mut world, &json!({ "entity": agent.to_bits() })).expect("reads");
        assert_eq!(out["newly_attached"], false);
        assert_eq!(out["ticks"], 3);
        assert_eq!(out["running"], json!([0, 1]));
        assert_eq!(out["nodes"][0]["name"], "patrol");
        assert_eq!(out["nodes"][0]["status"], "running");
        let off = live(&mut world, &json!({ "entity": agent.to_bits(), "detach": true })).expect("detaches");
        assert_eq!(off["was_attached"], true);
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        let err = live(&mut world, &json!({ "entity": gone.to_bits() })).expect_err("gone");
        assert_eq!(reason(&err), "entity_not_found");
        let plain = world.spawn_empty().id();
        let err = live(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("none");
        assert_eq!(reason(&err), "no_behavior_tree");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ai.bt_live` reports a `MoveTo` leaf's progress (ADR-8293): provider, state, distance left,
    /// waypoints; an agent without one reports `[]`.
    #[test]
    fn live_shows_move_to_progress() {
        use regia_ai::{MoveTarget, MoveTo, PathProviders};
        use regia_core::math::Vec3;
        let dir = unique_temp_dir("bt-agent-live-move");
        let mut world = world_in(&dir);
        let h = 10.0;
        let floor = [
            [
                Vec3::new(-h, 0.0, -h),
                Vec3::new(h, 0.0, -h),
                Vec3::new(h, 0.0, h),
            ],
            [
                Vec3::new(-h, 0.0, -h),
                Vec3::new(h, 0.0, h),
                Vec3::new(-h, 0.0, h),
            ],
        ];
        world.insert_resource(regia_nav::NavMeshBuilder::default().build(&floor).expect("floor"));
        world.insert_resource(PathProviders::with_defaults());
        MoveTo::new(MoveTarget::Point(Vec3::new(8.0, 0.0, 0.0)), 0.25)
            .register(&mut world.resource_mut::<BtTaskRegistry>(), "game::Go");
        let mut b = BehaviorTreeBuilder::new();
        let leaf = b.leaf("go", "game::Go");
        let tree =
            BehaviorTree::compile(&b.build(leaf), world.resource::<BtTaskRegistry>()).expect("compiles");
        let start = regia_scene::Transform::from_translation(Vec3::new(-8.0, 0.0, 0.0));
        // `Transform` requires a `GlobalTransform`; give it the matching one (nothing propagates here).
        let global = regia_scene::GlobalTransform::IDENTITY * start;
        let agent = world
            .spawn((tree, start, global, regia_ai::Blackboard::new()))
            .id();
        let first = live(&mut world, &json!({ "entity": agent.to_bits() })).expect("attaches");
        assert_eq!(first["move_to"], json!([]), "nothing ticked yet");
        for _ in 0..10 {
            let mut tree = world.entity_mut(agent).take::<BehaviorTree>().expect("tree");
            let mut ctx = BtContext {
                world: &mut world,
                entity: agent,
                dt: 1.0 / 60.0,
            };
            tree.tick(&mut ctx);
            world.entity_mut(agent).insert(tree);
        }
        let out = live(&mut world, &json!({ "entity": agent.to_bits() })).expect("reads");
        let progress = &out["move_to"][0];
        assert_eq!(progress["key"], "move_to");
        assert_eq!(progress["provider"], "navmesh");
        assert_eq!(progress["state"], "following");
        assert_eq!(progress["crowd"], false);
        assert_eq!(progress["ticks"], 10);
        assert_eq!(progress["error"], Value::Null);
        let remaining = progress["remaining"].as_f64().expect("remaining");
        assert!(remaining > 14.0 && remaining < 16.0, "{remaining}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
