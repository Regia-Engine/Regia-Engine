// SPDX-License-Identifier: MPL-2.0
//! Agent surface of autosave, crash recovery and background-task cancel (ADR-9770,
//! `book/agents/editor-autosave.md`): `editor.autosave_state`, `editor.autosave_now`,
//! `editor.recovery_list`, `editor.recovery_restore`, `editor.recovery_discard` and
//! `editor.cancel_task`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::World;
use serde_json::{Value, json};

use crate::doc_autosave::{self, AutosaveState, DocAutosaveRegistry, RecoveryOffers, RecoveryOutcome};
use crate::viewport_agent::Handler;

/// How long `editor.autosave_now` waits for the worker.
const NOW_BOUND: Duration = Duration::from_secs(30);

const IDS_PARAM: ParamSchema = ParamSchema {
    name: "ids",
    kind: "array",
    required: false,
    description: "Row ids from editor.recovery_list (`level:<file>` or `doc:<id>`); omitted or empty = every row.",
};

/// The six methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.autosave_state",
                tool: "autosave_state",
                description: "Autosave state: interval, ticks, writes finished and queued, the last tick's main-thread cost, the last level autosave and error, the registered editor kinds, the dirty documents, the running background tasks and what the last level snapshot reflected (ADR-7748).",
                params: &[],
                result: "{ interval_s, ticks, completed, in_flight, last_tick_us, last_level, last_error, kinds: [string], dirty: [{ kind, label, original }], tasks: [{ id, label }], level_snapshot: { reflected, reused, full } | null }",
                mutating: false,
                permission: Permission::Read,
            },
            autosave_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.autosave_now",
                tool: "autosave_now",
                description: "Autosaves the dirty level and every dirty asset document now (snapshot here, write on the autosave worker) and waits for the write.",
                params: &[],
                result: "{ level, docs: [string], main_thread_us, errors: [string] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            autosave_now as Handler,
        ),
        (
            MethodSchema {
                name: "editor.recovery_list",
                tool: "recovery_list",
                description: "What a crashed session left behind: level autosaves newer than their level and every document in .regia/autosave/docs/, with time and size.",
                params: &[],
                result: "{ items: [{ id, kind, label, original, autosave, size, saved_unix_ms }] }",
                mutating: false,
                permission: Permission::Read,
            },
            recovery_list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.recovery_restore",
                tool: "recovery_restore",
                description: "Restores recovery rows: a level opens as the open level, a document opens in its panel at its original path; both are dirty until saved.",
                params: &[IDS_PARAM],
                result: "{ restored: [string], failed: [{ id, error }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            recovery_restore as Handler,
        ),
        (
            MethodSchema {
                name: "editor.recovery_discard",
                tool: "recovery_discard",
                description: "Discards recovery rows: deletes their autosave files and index rows.",
                params: &[IDS_PARAM],
                result: "{ discarded: [string], failed: [{ id, error }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            recovery_discard as Handler,
        ),
        (
            MethodSchema {
                name: "editor.cancel_task",
                tool: "cancel_task",
                description: "Cancels a running background task: `game_build` (kills cargo; the loaded game library stays; the same token as editor.cancel_job { id: \"build\" }) or `import` (the Content Browser's background import job: it stops and removes the files it had copied, ADR-7381).",
                params: &[ParamSchema {
                    name: "id",
                    kind: "string",
                    required: true,
                    description: "`game_build` or `import` (editor.autosave_state's tasks list the game build).",
                }],
                result: "{ cancelled: true, id }",
                mutating: true,
                permission: Permission::EditScene,
            },
            cancel_task as Handler,
        ),
    ]
}

fn project_root(world: &World) -> Option<PathBuf> {
    crate::matgraph_editor::project_root(world)
}

fn no_project() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "no_project",
        "no project is open",
        "open a project first (editor.open_project)",
    )
}

/// The offers of the open project: the shell's when it published them, else a scan.
fn offers(world: &mut World) -> Result<RecoveryOffers, RpcError> {
    let root = project_root(world).ok_or_else(no_project)?;
    doc_autosave::install(world);
    let current = world.resource::<RecoveryOffers>();
    if current.project_root.as_deref() == Some(root.as_path()) {
        return Ok(current.clone());
    }
    let mut scanned = RecoveryOffers::scan(&root);
    scanned.revision = current.revision + 1;
    world.insert_resource(scanned.clone());
    Ok(scanned)
}

fn autosave_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    doc_autosave::install(world);
    let dirty: Vec<Value> = doc_autosave::dirty_documents(world)
        .into_iter()
        .map(|key| {
            json!({
                "kind": key.kind,
                "label": key.label,
                "original": key.original.map(|p| p.display().to_string()),
            })
        })
        .collect();
    let kinds = world.resource::<DocAutosaveRegistry>().kinds();
    let tasks: Vec<Value> = world
        .get_resource::<crate::tasks::BackgroundTasks>()
        .map(|t| {
            t.rows()
                .iter()
                .map(|r| json!({ "id": r.id, "label": r.label }))
                .collect()
        })
        .unwrap_or_default();
    let level_snapshot = crate::autosave_incremental::last_stats(world)
        .map(|stats| json!({ "reflected": stats.reflected, "reused": stats.reused, "full": stats.full }));
    let mut state = world.resource_mut::<AutosaveState>();
    let finished = state.worker.poll();
    state.absorb(finished);
    Ok(json!({
        "interval_s": state.interval.map(|d| d.as_secs()),
        "ticks": state.ticks,
        "completed": state.completed,
        "in_flight": state.worker.in_flight(),
        "last_tick_us": state.last_tick_us,
        "last_level": state.last_level.as_ref().map(|p| p.display().to_string()),
        "last_error": state.last_error,
        "kinds": kinds,
        "dirty": dirty,
        "tasks": tasks,
        "level_snapshot": level_snapshot,
    }))
}

fn autosave_now(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world).ok_or_else(no_project)?;
    // A level held by a recovery offer is never overwritten: use the offers of this project.
    let _ = offers(world)?;
    let errors_before = world
        .get_resource::<AutosaveState>()
        .and_then(|s| s.last_error.clone());
    let report = doc_autosave::tick(
        world,
        doc_autosave::TickInput {
            project_root: &root,
            now: Instant::now(),
            force: true,
        },
    );
    doc_autosave::flush(world, NOW_BOUND);
    let state = world.resource::<AutosaveState>();
    let errors: Vec<String> = state
        .last_error
        .clone()
        .filter(|e| Some(e) != errors_before.as_ref())
        .into_iter()
        .collect();
    Ok(json!({
        "level": report.level.map(|p| p.display().to_string()),
        "docs": report.docs,
        "main_thread_us": report.main_thread_us,
        "errors": errors,
    }))
}

fn recovery_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let items: Vec<Value> = offers(world)?
        .rows()
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "kind": row.kind,
                "label": row.label,
                "original": row.original.map(|p| p.display().to_string()),
                "autosave": row.autosave.display().to_string(),
                "size": row.size,
                "saved_unix_ms": row.saved_unix_ms,
            })
        })
        .collect();
    Ok(json!({ "items": items }))
}

fn ids_param(params: &Value) -> Result<Vec<String>, RpcError> {
    match params.get("ids") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    RpcError::new(
                        INVALID_PARAMS,
                        "invalid_ids",
                        "`ids` must be an array of strings",
                        "pass ids from editor.recovery_list",
                    )
                })
            })
            .collect(),
        Some(_) => Err(RpcError::new(
            INVALID_PARAMS,
            "invalid_ids",
            "`ids` must be an array of strings",
            "pass ids from editor.recovery_list",
        )),
    }
}

fn outcome_json(verb: &str, outcome: RecoveryOutcome) -> Value {
    let failed: Vec<Value> = outcome
        .failed
        .into_iter()
        .map(|(id, error)| json!({ "id": id, "error": error }))
        .collect();
    let mut out = serde_json::Map::new();
    out.insert(verb.to_string(), json!(outcome.done));
    out.insert("failed".to_string(), Value::Array(failed));
    Value::Object(out)
}

fn recovery_restore(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let ids = ids_param(params)?;
    let offers = offers(world)?;
    // A level row replaces the open level: refused as a whole while a prefab session holds the
    // level in its stash (`autosave::apply_restore` refuses the same), so nothing changes.
    if crate::prefab_edit::in_prefab_mode(world) {
        let levels: Vec<String> = offers.levels.iter().map(doc_autosave::level_row_id).collect();
        let wants_level = if ids.is_empty() {
            !levels.is_empty()
        } else {
            ids.iter().any(|id| levels.contains(id))
        };
        if wants_level {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "prefab_open",
                crate::autosave::RESTORE_IN_PREFAB_MODE,
                "close it with editor.close_prefab, or restore only `doc:` rows",
            ));
        }
    }
    Ok(outcome_json("restored", doc_autosave::restore(world, &ids)))
}

fn recovery_discard(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let ids = ids_param(params)?;
    let _ = offers(world)?;
    Ok(outcome_json("discarded", doc_autosave::discard(world, &ids)))
}

fn cancel_task(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = params.get("id").and_then(Value::as_str).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "invalid_task_id",
            "`id` is required",
            "pass `game_build` or `import`",
        )
    })?;
    if crate::tasks::cancel(world, id) {
        Ok(json!({ "cancelled": true, "id": id }))
    } else {
        Err(RpcError::new(
            NOT_FOUND,
            "no_such_task",
            format!("no background task `{id}` is running"),
            "editor.autosave_state lists the running tasks",
        ))
    }
}
