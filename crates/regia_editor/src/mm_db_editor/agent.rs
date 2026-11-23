// SPDX-License-Identifier: MPL-2.0
//! The motion-database editor's agent surface (`book/agents/motion-database-editor.md`):
//! `editor.mm_db_open`, `editor.mm_db_state`, `editor.mm_db_edit`, `editor.mm_db_save`,
//! `editor.mm_db_rebuild` and (part B) `editor.mm_db_scrub`, one route per window action. Span
//! timeline edits are `editor.mm_db_edit` ops (`add_span`, `set_span`, `remove_span`). Undo and redo are the shared
//! `editor.undo` / `editor.redo`. Refusals are `F0002` with `data.reason` set to a code listed on
//! the guide page.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::model::{issues, lints};
use super::ops::MmDbOp;
use super::{MmDbEditor, MmDbError, MmDbRebuildJob, RebuildView};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/motion-database-editor.md";

/// Most ops one `editor.mm_db_edit` call takes.
pub const MAX_OPS: usize = 512;

fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

impl From<MmDbError> for RpcError {
    fn from(e: MmDbError) -> Self {
        refused(e.code, e.message)
    }
}

/// The editor's state as the agent sees it (and `editor.mm_db_state` returns).
#[must_use]
pub fn state_json(world: &World) -> Value {
    let running = world.get_resource::<MmDbRebuildJob>().map(|j| j.job.id.clone());
    let Some(editor) = world.get_resource::<MmDbEditor>().filter(|e| e.is_open()) else {
        return json!({
            "open": false,
            "path": null,
            "rebuild": { "state": if running.is_some() { "running" } else { "none" }, "job": running },
        });
    };
    let Some(doc) = editor.doc.as_ref() else {
        return json!({ "open": false });
    };
    let undo = world.get_resource::<crate::command::UndoStack>();
    let rebuild = match (&running, &editor.rebuild) {
        (Some(id), _) => json!({ "state": "running", "job": id }),
        (None, None) => json!({ "state": "none" }),
        (None, Some(RebuildView::Done { report, revision })) => json!({
            "state": "done",
            "stale": *revision != editor.revision,
            "report": report.json(),
        }),
        (None, Some(RebuildView::Failed(e))) => json!({
            "state": "failed",
            "code": e.code,
            "message": e.message,
        }),
        (None, Some(RebuildView::Cancelled)) => json!({ "state": "cancelled" }),
    };
    let issues: Vec<Value> = issues(doc).iter().map(super::model::Issue::json).collect();
    let checks: Vec<Value> = lints(doc).iter().map(super::model::Issue::json).collect();
    json!({
        "open": true,
        "path": editor.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "dirty": editor.is_dirty(),
        "revision": editor.revision,
        "comments_warning": editor.comments_warning.then_some(super::COMMENTS_WARNING),
        "document": serde_json::to_value(doc).unwrap_or(Value::Null),
        "valid": issues.is_empty(),
        "issues": issues,
        "checks": checks,
        "undo_depth": undo.map_or(0, crate::command::UndoStack::undo_depth),
        "redo_depth": undo.map_or(0, crate::command::UndoStack::redo_depth),
        "rebuild": rebuild,
        "live": live_json(world),
        "cursor": editor.ui.timeline.cursor.map(|c| json!({
            "entry": c.entry, "frame": c.frame, "mirrored": c.mirrored,
        })),
        "toast": editor.toast,
    })
}

fn live_json(world: &World) -> Value {
    let live = world.get_resource::<super::live::MmDbLive>();
    json!({
        "enabled": live.is_none_or(|l| l.enabled),
        "runs": live.map_or(0, |l| l.runs),
        "error": live.and_then(|l| l.last_error.clone()),
    })
}

/// A `.motiondb.ron` request path (project-relative or absolute) under the editor's one
/// document-path rule (ADR-7388).
fn request_path(world: &World, path: &str) -> Result<std::path::PathBuf, RpcError> {
    if !path.ends_with(&format!(".{}", super::MOTIONDB_EXTENSION)) {
        return Err(refused("bad_path", "`path` must name a `.motiondb.ron` file"));
    }
    let resolved = crate::state_tree_editor::resolve_path(world, path);
    crate::agent_ext::document_request_path(world, "path", &resolved.to_string_lossy())
}

fn mm_db_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| refused("missing_param", "`path` is required"))?;
    let path = request_path(world, path)?;
    let discard = params.get("discard").and_then(Value::as_bool).unwrap_or(false);
    super::open(world, &path, discard)?;
    world
        .get_resource_or_insert_with(MmDbEditor::default)
        .ui
        .window_open = true;
    Ok(state_json(world))
}

fn mm_db_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    super::tick_job(world);
    Ok(state_json(world))
}

fn mm_db_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let raw = params
        .get("ops")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| refused("missing_param", "`ops` must be a non-empty array of ops"))?;
    if raw.len() > MAX_OPS {
        return Err(refused("too_many", format!("at most {MAX_OPS} ops per call")));
    }
    let ops = raw
        .iter()
        .enumerate()
        .map(|(i, v)| {
            serde_json::from_value::<MmDbOp>(v.clone()).map_err(|e| refused("bad_op", format!("op {i}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let label = params
        .get("label")
        .and_then(Value::as_str)
        .map_or_else(|| ops[0].name().replace('_', " "), str::to_string);
    let changed = super::edit(world, &label, None, &ops)?;
    let mut out = state_json(world);
    if let Some(map) = out.as_object_mut() {
        map.insert("changed".into(), json!(changed));
    }
    Ok(out)
}

fn mm_db_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(|p| request_path(world, p))
        .transpose()?;
    let saved = super::save(world, path.as_deref())?;
    let mut out = state_json(world);
    if let Some(map) = out.as_object_mut() {
        map.insert("saved".into(), json!(saved.to_string_lossy()));
    }
    Ok(out)
}

fn mm_db_rebuild(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or("run");
    match action {
        "run" | "start" => {
            let id = super::start_rebuild(world)?;
            if action == "run" {
                let _ = super::finish_now(world);
            }
            let mut out = state_json(world);
            if let Some(map) = out.as_object_mut() {
                map.insert("job".into(), json!(id));
            }
            Ok(out)
        }
        "cancel" => {
            if !super::cancel_rebuild(world) {
                return Err(refused("no_rebuild", "no rebuild is running"));
            }
            Ok(state_json(world))
        }
        "live" => {
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| refused("missing_param", "action live needs `enabled` (boolean)"))?;
            world
                .get_resource_or_insert_with(super::live::MmDbLive::default)
                .enabled = enabled;
            Ok(state_json(world))
        }
        other => Err(refused(
            "bad_action",
            format!("action must be run, start, cancel or live, got {other:?}"),
        )),
    }
}

fn mm_db_scrub(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entry = params
        .get("entry")
        .and_then(Value::as_u64)
        .ok_or_else(|| refused("missing_param", "`entry` (an entry index) is required"))?
        as usize;
    let mirrored = params.get("mirrored").and_then(Value::as_bool).unwrap_or(false);
    let rate = world
        .get_resource::<MmDbEditor>()
        .and_then(|e| e.doc.as_ref().map(|d| d.schema.sample_rate))
        .ok_or_else(|| refused("no_database_open", "no motion database is open"))?;
    let frame = match (params.get("frame"), params.get("time")) {
        (Some(_), Some(_)) => return Err(refused("bad_value", "give `frame` or `time`, not both")),
        (Some(f), None) => {
            f.as_u64()
                .ok_or_else(|| refused("bad_value", "`frame` must be a whole number"))? as usize
        }
        (None, Some(t)) => {
            let t = t
                .as_f64()
                .filter(|t| t.is_finite() && *t >= 0.0)
                .ok_or_else(|| refused("bad_value", "`time` must be seconds, not negative"))?;
            super::ops::time_frame(rate, t as f32) as usize
        }
        (None, None) => 0,
    };
    let view =
        super::preview::frame_view(world, entry, frame, mirrored).map_err(|e| refused(e.code, e.message))?;
    super::timeline::select_frame(world, entry, frame as u32, mirrored);
    let spans: Vec<Value> = world
        .get_resource::<MmDbEditor>()
        .and_then(|e| e.doc.as_ref()?.entries.get(entry).cloned())
        .map(|e| {
            e.spans
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let (a, b) = super::timeline::span_frames(rate, s);
                    json!({ "span": i, "kind": serde_json::to_value(&s.kind).unwrap_or(Value::Null),
                        "range": [s.range.0, s.range.1], "frames": [a, b] })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out = view.json();
    if let Some(map) = out.as_object_mut() {
        map.insert("spans".into(), json!(spans));
    }
    Ok(out)
}

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

const STATE: &str = "{ open, path, dirty, revision, comments_warning, document, valid, issues: [{ severity, code, entry, span, \
    message }], checks: [{ severity, code, entry, message }], undo_depth, redo_depth, rebuild: { state: none | running | \
    done | failed | cancelled, job, stale, report: { frames, dimensions, ranges, targets, untagged_frames, tag_frames, \
    coverage, groups, kd, warnings, content_hash }, code, message }, live: { enabled, runs, error }, cursor: { entry, \
    frame, mirrored } | null, toast }";

/// The `ops` parameter's description: every op `editor.mm_db_edit` takes (a test keeps it equal to
/// [`super::ops::OP_NAMES`]).
pub const OPS_DOC: &str = "Ops, applied in order as one undo step (all or nothing): { op: set_skeleton, path } | \
    set_root_joint { joint } | set_forward { axis: [x, y, z] } | set_feet { feet } | set_trajectory_times { times } | \
    set_history_times { times } | set_sample_rate { rate } | set_weight { group, value } | set_normalization { mode } | \
    set_sample_weight { list: trajectory | history, index, value } | add_pose_joint { joint } | remove_pose_joint { index } | \
    set_pose_joint { index, position?, velocity?, weight? } | set_contacts { contacts: null | { height_on, height_off, \
    speed_on, speed_off, phase } } | add_mirror_pair { left, right } | remove_mirror_pair { index } | add_tag { name } | \
    remove_tag { name } | rename_tag { name, to } | add_clip { clip, index? } | remove_clip { index } | move_clip { from, to } | \
    set_clip { index, clip?, looping?, mirrored?, distance_matched?, trim? } | add_clip_tag { index, tag } | \
    remove_clip_tag { index, tag } | add_span { index, kind, range? | frames? } | set_span { index, span, kind?, range? | \
    frames? } | remove_span { index, span }. Span kind: pass_through | unindexed | { landing_bias: b } | { tag: [names] }; \
    range is [start, end) clip seconds, frames is [start, end) database frames. Same-kind pass_through, unindexed and \
    same-tags tag spans that overlap or touch merge; landing_bias spans stack. Weights are clamped to the runtime \
    validator's ranges.";

/// The motion-database editor's methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.mm_db_open",
                "mm_db_open",
                "Open a .motiondb.ron in the motion-database editor (the window opens too). Refused with \
                 unsaved_changes while the open one is dirty, unless discard: true.",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            true,
                            "Project-relative or absolute .motiondb.ron path.",
                        ),
                        p(
                            "discard",
                            "boolean",
                            false,
                            "Drop the open database's unsaved changes.",
                        ),
                    ]
                },
                STATE,
                true,
            ),
            mm_db_open as Handler,
        ),
        (
            m(
                "editor.mm_db_state",
                "mm_db_state",
                "The open motion database: the document (schema, tags, mirror pairs, entries), dirty, the \
                 validator's errors (the runtime's own checks), the editor's checks (untagged clips, zero-weight \
                 groups), the comment-loss warning, undo depths and the last rebuild (poses, clip coverage, \
                 feature ranges, the runtime builder's warnings).",
                &[],
                STATE,
                false,
            ),
            mm_db_state as Handler,
        ),
        (
            m(
                "editor.mm_db_edit",
                "mm_db_edit",
                "Edit the open motion database: the schema form, the clip list and the tag chips. One call is one \
                 undo step (editor.undo reverts it).",
                &const {
                    [
                        p("ops", "array", true, OPS_DOC),
                        p(
                            "label",
                            "string",
                            false,
                            "The undo entry's label (default: the first op's name).",
                        ),
                    ]
                },
                STATE,
                true,
            ),
            mm_db_edit as Handler,
        ),
        (
            m(
                "editor.mm_db_save",
                "mm_db_save",
                "Save the open motion database as canonical text (the runtime's writer; comments are dropped), to \
                 where it was opened or to path.",
                &const { [p("path", "string", false, "Save-as .motiondb.ron path.")] },
                STATE,
                true,
            ),
            mm_db_save as Handler,
        ),
        (
            m(
                "editor.mm_db_rebuild",
                "mm_db_rebuild",
                "Rebuild the open motion database in memory with the runtime builder (the cooked file is not \
                 written) and report poses, clip coverage, feature ranges and the builder's warnings. action: run \
                 (default, waits), start (returns job mm_db_rebuild:<n>; editor.cancel_job and agent.job_status \
                 see it), cancel, or live with enabled (rebuild in the background after edits settle).",
                &const {
                    [
                        p("action", "string", false, "run (default), start, cancel or live."),
                        p(
                            "enabled",
                            "boolean",
                            false,
                            "With action live: turn live rebuilds on or off.",
                        ),
                    ]
                },
                STATE,
                true,
            ),
            mm_db_rebuild as Handler,
        ),
        (
            m(
                "editor.mm_db_scrub",
                "mm_db_scrub",
                "Scrub an entry's span timeline: the preview poses the skeleton at the frame and the reply has \
                 the frame's raw feature vector (the runtime builder's row), split by group, its baked tags, \
                 landing bias and flags, and the entry's spans with their frames.",
                &const {
                    [
                        p("entry", "integer", true, "Entry index."),
                        p("frame", "integer", false, "Frame within the entry (default 0)."),
                        p(
                            "time",
                            "number",
                            false,
                            "Clip seconds instead of frame (the nearest frame).",
                        ),
                        p("mirrored", "boolean", false, "The entry's mirrored copy."),
                    ]
                },
                SCRUB,
                false,
            ),
            mm_db_scrub as Handler,
        ),
    ]
}

const SCRUB: &str = "{ entry, frame, frames, time, mirrored, features: [f32], groups: [{ group, start, values }], meta: { \
    tags, bias, pass_through, unindexed, target } | null, joints, spans: [{ span, kind, range, frames }] }";
