// SPDX-License-Identifier: MPL-2.0
//! Terrain ▸ Overview (ADR-8596): a count source's north-up shaded overview baked off the editor
//! thread as the cancellable job `terrain_overview:<n>` and published as a registered pair (the PNG
//! and its descriptor `<name>.overview.json`, `docs/formats/terrain.md` § 16).
//!
//! The job runs the same service as `terrain.bake_overview { register: true }` and
//! `regia terrain source-overview` ([`regia_agent::publish_project_overview`]), so the three agree on
//! settings, markers, cache and output by construction. Entity markers are resolved on the editor
//! thread when the job starts.
//!
//! - **Start**: [`start`] / `editor.terrain_overview_start` (the `terrain.bake_overview`
//!   parameters). A current pair is not baked again (`cached: true` in the result).
//! - **Wait**: the agent event [`EVENT_KIND`] `"<job> done"` (or `cancelled`, `failed`);
//!   `play.wait_until { events_contain }` waits on it without polling.
//! - **Status**: [`status`] / `editor.terrain_overview_status`, or `agent.job_status { id }`.
//! - **Cancel**: `editor.cancel_job { id }` (or `agent.job_cancel`, or the task row). The worker
//!   checks the cancel at every bake step and once more before publication; a cancelled job leaves
//!   no PNG, descriptor or temporary file, and an earlier pair untouched.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::editor_jobs::{WorkerContext, WorkerJob};
use crate::viewport_agent::Handler;

/// The job kind (`terrain_overview:<n>`).
pub const JOB_KIND: &str = "terrain_overview";

/// The background-task row id.
const TASK: &str = "terrain_overview";

/// The agent event kind every transition is recorded under.
pub const EVENT_KIND: &str = "terrain_overview";

const HINT: &str = "see book/agents/terrain-source.md";

fn coded(status: i64, code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(status, code, message, HINT)
}

type Outcome = Result<Value, RpcError>;

/// The running overview job.
#[derive(Resource, Debug)]
pub struct TerrainOverviewJob {
    /// The job id (`terrain_overview:<n>`).
    pub id: String,
    /// The request (`terrain.bake_overview`'s parameters).
    pub params: Value,
    job: WorkerJob<Outcome>,
}

/// How the last overview job ended, and its report (what `editor.terrain_overview_status` answers
/// for it).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct TerrainOverviewLast {
    /// The job id.
    pub id: String,
    /// `done`, `cancelled` or `failed`.
    pub state: &'static str,
    /// `{ job, state, result? , error? }`.
    pub report: Value,
}

/// Tests: the next job's worker waits for this channel before it bakes, so a test cancels it at a
/// known point without sleeping.
#[derive(Resource)]
pub struct TerrainOverviewStartGate(pub Mutex<Option<Receiver<()>>>);

fn emit(world: &mut World, id: &str, state: &str) {
    let frame = world.get_resource::<Time>().map_or(0, |t| t.frame_count());
    if let Some(mut log) = world.get_resource_mut::<regia_agent::AgentEventLog>() {
        log.push_host(
            frame,
            EVENT_KIND,
            json!({ "event": format!("{id} {state}"), "job": id, "state": state }),
        );
    }
}

fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            coded(
                INVALID_PARAMS,
                "terrain_overview_no_project",
                "open a project first",
            )
        })
}

/// Starts an overview bake as job `terrain_overview:<n>` and answers its id.
///
/// # Errors
///
/// `terrain_overview_busy` (a job runs: wait for it or cancel it), `terrain_overview_no_project`,
/// `invalid_params` for malformed settings or markers, `terrain_overview_failed` (the worker
/// cannot start).
pub fn start(world: &mut World, params: Value) -> Result<String, RpcError> {
    if let Some(running) = world.get_resource::<TerrainOverviewJob>() {
        return Err(coded(
            INVALID_PARAMS,
            "terrain_overview_busy",
            format!("{} is running; wait for it or cancel it first", running.id),
        ));
    }
    for key in ["path", "output"] {
        if params.get(key).and_then(Value::as_str).is_none() {
            return Err(RpcError::invalid_params(format!("{key} is required")));
        }
    }
    // Settings and markers are checked (and entity markers resolved) before the job starts.
    regia_agent::overview_settings(&params)?;
    let markers = regia_agent::overview_markers(Some(world), &params)?;
    let root = project_root(world)?;
    let policy = world
        .get_resource::<regia_agent::TerrainSourcePolicy>()
        .copied()
        .unwrap_or_default()
        .limits;
    let gate = world
        .remove_resource::<TerrainOverviewStartGate>()
        .and_then(|g| g.0.into_inner().ok().flatten());
    let worker_params = params.clone();
    let output = params
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned();
    let job = WorkerJob::spawn(
        world,
        JOB_KIND,
        TASK,
        format!("baking terrain overview {output}"),
        "regia-terrain-overview",
        move |cx: WorkerContext| {
            if let Some(gate) = gate {
                cx.progress(Some(0.0), "waiting");
                let _ = gate.recv();
            }
            run(&root, policy, &worker_params, markers, &cx)
        },
    )
    .map_err(|e| coded(INVALID_PARAMS, "terrain_overview_failed", e))?;
    let id = job.id.clone();
    world.insert_resource(TerrainOverviewJob {
        id: id.clone(),
        params,
        job,
    });
    emit(world, &id, "running");
    Ok(id)
}

fn run(
    root: &std::path::Path,
    policy: regia_terrain::source::ImportLimits,
    params: &Value,
    markers: Vec<regia_terrain::overview::OverviewMarker>,
    cx: &WorkerContext,
) -> Outcome {
    let _span = tracing::info_span!(target: "regia_editor", "terrain_overview.job").entered();
    regia_agent::publish_project_overview(root, policy, params, markers, |p| {
        cx.progress(
            Some(f64::from(p.step) / f64::from(p.steps.max(1))),
            if p.step == p.steps { "publishing" } else { "baking" },
        );
        if cx.cancelled() {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    })
}

fn error_code(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("terrain_overview_failed")
        .to_owned()
}

/// Polls the running job (every frame, [`crate::editor_jobs::tick`]) and ends it when its worker
/// returned. Never waits.
pub fn tick_job(world: &mut World) {
    let Some(mut running) = world.remove_resource::<TerrainOverviewJob>() else {
        return;
    };
    let Some(outcome) = running.job.take() else {
        let message = running.job.context().message();
        running
            .job
            .sync_row(world, &message, running.job.context().fraction());
        world.insert_resource(running);
        return;
    };
    let id = running.id.clone();
    let (state, report) = match outcome {
        Ok(Ok(result)) => {
            running.job.end(world, Ok((result.clone(), false)));
            ("done", json!({ "job": id, "state": "done", "result": result }))
        }
        Ok(Err(e)) if error_code(&e) == "terrain_overview_cancelled" => {
            let result = json!({ "state": "cancelled", "published": false });
            running.job.end(world, Ok((result, true)));
            (
                "cancelled",
                json!({ "job": id, "state": "cancelled", "published": false }),
            )
        }
        Ok(Err(e)) => {
            let code = error_code(&e);
            running.job.end(world, Err((code.as_str(), e.message.clone())));
            (
                "failed",
                json!({ "job": id, "state": "failed", "error": { "code": code, "message": e.message, "data": e.data } }),
            )
        }
        Err(panic) => {
            running
                .job
                .end(world, Err(("terrain_overview_failed", panic.clone())));
            (
                "failed",
                json!({ "job": id, "state": "failed", "error": { "code": "terrain_overview_failed", "message": panic } }),
            )
        }
    };
    world.insert_resource(TerrainOverviewLast {
        id: id.clone(),
        state,
        report,
    });
    emit(world, &id, state);
}

/// Waits for a running worker and ends its job: for blocking callers and tests.
pub fn finish_now(world: &mut World) {
    if let Some(mut running) = world.get_resource_mut::<TerrainOverviewJob>() {
        running.job.wait();
    }
    tick_job(world);
}

/// Waits for the running worker to return without ending its job: the next [`tick_job`] (the next
/// frame) does, so the frame the end event appears in is deterministic.
pub fn wait_worker(world: &mut World) {
    if let Some(mut running) = world.get_resource_mut::<TerrainOverviewJob>() {
        running.job.wait();
    }
}

/// The job's state: the running one when `id` is `None` or matches (with progress), else the last
/// ended one (with its result or error).
///
/// # Errors
///
/// `terrain_overview_not_found` for an id the editor has no record of.
pub fn status(world: &World, id: Option<&str>) -> Result<Value, RpcError> {
    if let Some(running) = world.get_resource::<TerrainOverviewJob>()
        && id.is_none_or(|id| id == running.id)
    {
        return Ok(json!({
            "job": running.id, "state": "running",
            "progress": { "fraction": running.job.context().fraction(), "message": running.job.context().message() },
            "params": running.params,
        }));
    }
    if let Some(last) = world.get_resource::<TerrainOverviewLast>()
        && id.is_none_or(|id| id == last.id)
    {
        return Ok(last.report.clone());
    }
    Err(coded(
        NOT_FOUND,
        "terrain_overview_not_found",
        match id {
            Some(id) => format!("no terrain overview job {id}"),
            None => "no terrain overview job has run".to_owned(),
        },
    ))
}

/// The Terrain panel's Overview fields.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct TerrainOverviewPanel {
    /// The overview PNG, project-relative (the descriptor goes beside it).
    pub output: String,
    /// Keep the source's aspect ratio (letterbox) instead of the legacy stretch.
    pub preserve_aspect: bool,
    /// Output width and height, pixels.
    pub size: [u32; 2],
    /// The last result or error.
    pub message: String,
}

impl Default for TerrainOverviewPanel {
    fn default() -> Self {
        Self {
            output: "assets/maps/overview.png".into(),
            preserve_aspect: true,
            size: [512, 512],
            message: String::new(),
        }
    }
}

impl TerrainOverviewPanel {
    /// The `editor.terrain_overview_start` parameters for the Import source fields' source.
    #[must_use]
    pub fn params(&self, source: &crate::terrain_source_import::TerrainSourcePanel) -> Value {
        let fit = if self.preserve_aspect {
            "preserve_aspect"
        } else {
            "stretch"
        };
        let mut params = json!({
            "path": source.path,
            "output": self.output,
            "overview": {
                "size": self.size,
                "fit": fit,
            },
        });
        if !source.mapping.trim().is_empty() {
            params["mapping"] = json!(source.mapping.trim());
        }
        if let Ok(png @ Value::Object(_)) = serde_json::from_str::<Value>(&source.png) {
            params["png"] = png;
        }
        params
    }
}

/// The Terrain panel's Overview section: the Import source fields' source, an output, fit and size;
/// Bake (`editor.terrain_overview_start`) and, while it runs, Cancel (`editor.cancel_job`).
pub fn panel_ui(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world
        .get_resource::<TerrainOverviewPanel>()
        .cloned()
        .unwrap_or_default();
    ui.heading("Overview");
    ui.label("North-up shaded image of the source above, with its world mapping (.overview.json).");
    ui.horizontal(|ui| {
        ui.label("Output");
        ui.text_edit_singleline(&mut panel.output);
    });
    ui.checkbox(&mut panel.preserve_aspect, "Keep aspect ratio (letterbox)");
    ui.horizontal(|ui| {
        ui.label("Size");
        ui.add(egui::DragValue::new(&mut panel.size[0]).range(2..=2048));
        ui.add(egui::DragValue::new(&mut panel.size[1]).range(2..=2048));
    });
    let running = world.get_resource::<TerrainOverviewJob>().map(|r| r.id.clone());
    match running {
        None => {
            if ui.button("Bake overview").clicked() {
                let source = world
                    .get_resource::<crate::terrain_source_import::TerrainSourcePanel>()
                    .cloned()
                    .unwrap_or_default();
                panel.message = match start(world, panel.params(&source)) {
                    Ok(id) => format!("{id}: baking"),
                    Err(e) => e.message,
                };
            }
        }
        Some(id) => {
            let fraction = status(world, Some(&id))
                .ok()
                .and_then(|s| s.pointer("/progress/fraction").and_then(Value::as_f64))
                .unwrap_or(0.0);
            ui.label(format!("{id}: {:.0} %", fraction * 100.0));
            if ui.button("Cancel overview").clicked() {
                panel.message = match crate::editor_jobs::cancel_job(world, &json!({ "id": id })) {
                    Ok(_) => format!("{id}: cancelling"),
                    Err(e) => e.message,
                };
            }
        }
    }
    if let Some(last) = world.get_resource::<TerrainOverviewLast>() {
        let r = &last.report;
        let line = match last.state {
            "done" => format!(
                "{}: {} ({}), {} m/px, scale bar {} m{}",
                last.id,
                r.pointer("/result/output").and_then(Value::as_str).unwrap_or("?"),
                r.pointer("/result/descriptor")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                r.pointer("/result/mapping/meters_per_pixel/0")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                r.pointer("/result/legend/scale_bar_m")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                if r.pointer("/result/cached").and_then(Value::as_bool) == Some(true) {
                    ", up to date"
                } else {
                    ""
                },
            ),
            state => format!(
                "{}: {state} {}",
                last.id,
                r.pointer("/error/message").and_then(Value::as_str).unwrap_or("")
            ),
        };
        ui.label(line);
    }
    if !panel.message.is_empty() {
        ui.label(&panel.message);
    }
    world.insert_resource(panel);
}

fn agent_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = start(world, params.clone())?;
    Ok(json!({ "job": id, "state": "running" }))
}

fn agent_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick_job(world);
    status(world, params.get("job").and_then(Value::as_str))
}

/// The agent methods of the overview job.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.terrain_overview_start",
                tool: "terrain_overview_start",
                description: "Start a registered terrain overview bake (ADR-8596) as the cancellable job \
                    `terrain_overview:<n>`: the terrain.bake_overview { register: true } parameters (path, png?, \
                    mapping?, raster?, limits?, output, overview? with fit, markers?). Wait for the agent event \
                    `terrain_overview` \"<job> done\" (play.wait_until events_contain); cancel with \
                    editor.cancel_job { id } (nothing is published). Errors terrain_overview_busy, \
                    terrain_overview_no_project, terrain_overview_failed, invalid_params.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative .json or .png count source.",
                    },
                    ParamSchema {
                        name: "output",
                        kind: "string",
                        required: true,
                        description: "Project-relative overview .png; the descriptor <name>.overview.json goes beside it.",
                    },
                    ParamSchema {
                        name: "overview",
                        kind: "object",
                        required: false,
                        description: "As terrain.bake_overview: size, fit (stretch | preserve_aspect), relief, contour_step_m, blur_passes, light, ambient, contrast, frame_pixels.",
                    },
                    ParamSchema {
                        name: "markers",
                        kind: "array",
                        required: false,
                        description: "As terrain.bake_overview: {label?, xz} or {label?, entity}; entities resolve when the job starts.",
                    },
                    ParamSchema {
                        name: "png",
                        kind: "object",
                        required: false,
                        description: "PNG settings as for terrain.inspect_source.",
                    },
                    ParamSchema {
                        name: "mapping",
                        kind: "string",
                        required: false,
                        description: "A schema-2 heightfield.json, as for terrain.inspect_source.",
                    },
                    ParamSchema {
                        name: "raster",
                        kind: "string|object",
                        required: false,
                        description: "A raster descriptor, as for terrain.inspect_source.",
                    },
                    ParamSchema {
                        name: "limits",
                        kind: "object",
                        required: false,
                        description: "Lower import limits, as for terrain.inspect_source.",
                    },
                ],
                result: "{ job, state }",
                mutating: true,
                permission: Permission::Assets,
            },
            agent_start as Handler,
        ),
        (
            MethodSchema {
                name: "editor.terrain_overview_status",
                tool: "terrain_overview_status",
                description: "State of a terrain overview job: running (progress, params), done (result: the \
                    terrain.bake_overview { register: true } reply: mapping, legend, markers, descriptor, cached), \
                    cancelled (published: false) or failed (error). Errors terrain_overview_not_found.",
                params: &[ParamSchema {
                    name: "job",
                    kind: "string",
                    required: false,
                    description: "The job id (`terrain_overview:<n>`); default the current or last job.",
                }],
                result: "{ job, state, progress?, params?, result?, published?, error? }",
                mutating: false,
                permission: Permission::Read,
            },
            agent_status as Handler,
        ),
    ]
}
