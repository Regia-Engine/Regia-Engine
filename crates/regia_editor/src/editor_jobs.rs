// SPDX-License-Identifier: MPL-2.0
//! Cancellable editor jobs (`docs/specs/19-editor.md` § 7, ADR-7205): the content import and the
//! game build run in the background and stop on `editor.cancel_job { id }`.
//!
//! - **Import.** `editor.import { async: true }` runs the pipeline import on a worker thread as job
//!   `import:<n>` (extension importers still run first, on the editor thread). The worker checks
//!   the job's cancel token before each source, before the cook and after it; a cancelled import
//!   removes every file it had copied and ends `cancelled` ([`crate::content::import_files_cancellable`]).
//! - **Game build.** `agent.hot_reload`'s job `hot:<n>` cancels the build itself (cargo is killed,
//!   nothing reaches the hot dir; [`crate::HotReloadHost::cancel_compile`]). A build the toolbar or a
//!   source change started has no job: `editor.cancel_job { id: "build" }` stops it.
//!
//! - **Worker jobs** (ADR-7664). Content ▸ Import settings ▸ Apply and `editor.set_import_settings`
//!   (`import_settings:<n>`: the `.meta` write and the cook) and Show ▸ Build navigation preview
//!   (`nav_preview:<n>`: the voxel build) run on a [`WorkerJob`]; [`tick`] applies their ends to
//!   the world every frame, so the UI thread never cooks or bakes.
//!
//! Job ids are the agent job registry's (`agent.job_status`, `agent.job_cancel` see them too).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use regia_agent::jobs::{JobHandle, JobRegistry, JobState};
use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use serde_json::{Value, json};

use crate::engine::ecs::prelude::*;
use crate::viewport_agent::Handler;

/// The id `editor.cancel_job` takes for the running game build (toolbar, Ctrl+Alt+F11, a source
/// change), which has no job of its own.
pub const BUILD_JOB_ID: &str = "build";

/// Handles of the editor's cancellable jobs, by job id.
#[derive(Resource, Debug, Clone, Default)]
pub struct EditorJobs {
    handles: BTreeMap<String, JobHandle>,
}

impl EditorJobs {
    /// Remembers `handle` under `id` so `editor.cancel_job` can stop it.
    pub fn register(&mut self, id: &str, handle: JobHandle) {
        self.handles.retain(|_, h| !h.state().finished());
        self.handles.insert(id.to_string(), handle);
    }

    /// The handle of job `id`, when the editor started it.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&JobHandle> {
        self.handles.get(id)
    }
}

/// Set by `editor.cancel_job { id: "build" }`; the editor loop's hot-reload service stops the
/// running game build and clears it ([`crate::code_reload::service_hot_reload_job`]).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct CancelBuildRequest;

fn reasoned(code: i64, reason: &str, message: impl Into<String>) -> RpcError {
    let regia_code = if code == NOT_FOUND { "F0003" } else { "F0002" };
    let mut error = RpcError::new(code, regia_code, message, "see book/agents/pie-modes.md");
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), Value::String(reason.to_owned()));
    }
    error
}

/// `editor.cancel_job { id }`.
///
/// # Errors
///
/// `invalid_params` without an id; `job_not_found` for an id the editor did not start.
pub fn cancel_job(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(id) = params
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Err(RpcError::invalid_params(
            "`id` (a job id such as `import:3`, `hot:1`, or `build`) is required",
        ));
    };
    if id == BUILD_JOB_ID {
        let compiling = world
            .get_resource::<crate::code_reload::CodeReloadStatus>()
            .and_then(|status| status.state.clone())
            .is_some_and(|state| matches!(state, crate::hot_reload::CompileState::Compiling(_)));
        if compiling {
            world.insert_resource(CancelBuildRequest);
        }
        return Ok(
            json!({ "job": id, "requested": compiling, "state": if compiling { "running" } else { "idle" } }),
        );
    }
    let handle = world
        .get_resource::<EditorJobs>()
        .and_then(|jobs| jobs.get(id).cloned());
    let Some(handle) = handle else {
        return Err(reasoned(
            NOT_FOUND,
            "job_not_found",
            format!("the editor started no job {id}; agent.job_status lists every job"),
        ));
    };
    let state = handle.state();
    let requested = state == JobState::Running;
    if requested {
        handle.request_cancel();
    }
    Ok(json!({ "job": id, "requested": requested, "state": state.name() }))
}

/// The outcome slot an import worker fills before it ends its job.
type ImportOutcome = Arc<Mutex<Option<Result<crate::content::ImportReport, String>>>>;

/// A running background import ([`spawn_import`]).
#[derive(Debug, Clone)]
pub struct ImportJob {
    /// The job id (`import:<n>`).
    pub id: String,
    handle: JobHandle,
    outcome: ImportOutcome,
}

impl ImportJob {
    /// Whether the worker still runs.
    #[must_use]
    pub fn running(&self) -> bool {
        !self.handle.state().finished()
    }

    /// Asks the worker to stop (files it copied are removed).
    pub fn cancel(&self) {
        self.handle.request_cancel();
    }

    /// An import job that has already ended with `report` (tests of what runs after an import).
    #[cfg(test)]
    pub(crate) fn finished_for_test(world: &mut World, report: crate::content::ImportReport) -> Self {
        let (id, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("import", "test import", true);
        handle.finish(serde_json::json!({}));
        Self {
            id,
            handle,
            outcome: Arc::new(Mutex::new(Some(Ok(report)))),
        }
    }

    /// The report (or the error) once the worker has ended; `None` while it runs.
    #[must_use]
    pub fn take_outcome(&self) -> Option<Result<crate::content::ImportReport, String>> {
        if self.running() {
            return None;
        }
        self.outcome.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// The Content Browser's running import, if any (its Cancel button stops it).
#[derive(Resource, Debug, Clone)]
pub struct ContentImport(pub ImportJob);

/// Starts the pipeline import of `sources` on a worker thread as job `import:<n>`, registered in
/// the agent job registry and [`EditorJobs`].
///
/// # Errors
///
/// When the worker thread cannot start (the job is failed too).
pub fn spawn_import(
    world: &mut World,
    root: PathBuf,
    folder: String,
    sources: Vec<PathBuf>,
    client: Option<String>,
) -> Result<ImportJob, String> {
    let (job, handle) = world
        .get_resource_or_insert_with::<JobRegistry>(Default::default)
        .start("import", format!("import {} file(s)", sources.len()), true);
    world
        .get_resource_or_insert_with::<EditorJobs>(Default::default)
        .register(&job, handle.clone());
    handle.progress(Some(0.0), "importing");
    let outcome: ImportOutcome = Arc::new(Mutex::new(None));
    let worker = handle.clone();
    let slot = Arc::clone(&outcome);
    let spawned = std::thread::Builder::new()
        .name("regia-import".into())
        .spawn(move || {
            let cancelled = || worker.cancel_requested();
            let result = crate::content::import_files_cancellable(
                &root,
                &folder,
                &sources,
                client.as_deref(),
                &cancelled,
            );
            end_import_job(&worker, &slot, result);
        });
    if let Err(e) = spawned {
        let message = format!("cannot start the import worker: {e}");
        handle.fail("import_failed", message.clone());
        return Err(message);
    }
    Ok(ImportJob {
        id: job,
        handle,
        outcome,
    })
}

/// Ends an import worker's job with `result`: logs the per-file errors, puts the outcome in
/// `slot` before the job reads as ended, then finishes (or fails) the job.
fn end_import_job(
    worker: &JobHandle,
    slot: &ImportOutcome,
    result: Result<crate::content::ImportReport, String>,
) {
    let cancelled = result.as_ref().is_ok_and(|report| report.cancelled);
    let ended = match &result {
        Ok(report) => {
            for (path, message) in &report.errors {
                tracing::error!(target: "regia_editor", "import {path}: {message}");
            }
            let errors: Vec<Value> = report
                .errors
                .iter()
                .map(|(path, message)| json!({ "path": path, "error": message }))
                .collect();
            Ok(json!({
                "imported": report.imported,
                "cooked": report.cooked,
                "skipped": report.skipped,
                "errors": errors,
                "cancelled": report.cancelled,
            }))
        }
        Err(message) => Err(message.clone()),
    };
    // The outcome is in place before the job reads as ended.
    if let Ok(mut out) = slot.lock() {
        *out = Some(result);
    }
    match ended {
        // The report decides: a cancel that came after the worker's last check did not stop the
        // import (its files are in), so the job ends `done`, not `cancelled` over them.
        Ok(value) => worker.finish_as(value, cancelled),
        Err(message) => worker.fail("import_failed", message),
    }
}

/// `editor.import { async: true }`: starts the pipeline import of `sources` on a worker thread as
/// job `import:<n>` and answers its id.
///
/// # Errors
///
/// `import_failed` when the worker cannot start.
pub fn start_import_job(
    world: &mut World,
    root: PathBuf,
    folder: String,
    sources: Vec<PathBuf>,
    client: Option<String>,
) -> Result<Value, RpcError> {
    let job = spawn_import(world, root, folder, sources, client)
        .map_err(|message| reasoned(INVALID_PARAMS, "import_failed", message))?;
    Ok(json!({ "job": job.id }))
}

/// The methods this module answers.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.cancel_job",
            tool: "cancel_job",
            description: "Cancel a background editor job (specs/19 § 7): an `editor.import { async: true }` \
                job (`import:<n>`: files it copied are removed, the job ends `cancelled`), an \
                `agent.hot_reload` job (`hot:<n>`: the game build is stopped) or `build` (the running \
                game build the toolbar or a source change started). Poll agent.job_status for the end. \
                Also an import-settings write (`import_settings:<n>`: the old .meta and cooked output \
                come back) or a navigation preview bake (`nav_preview:<n>`: the old preview stays).",
            params: &[ParamSchema {
                name: "id",
                kind: "string",
                required: true,
                description: "The job id (`import:3`, `hot:1`) or `build`.",
            }],
            result: "{ job, requested, state }",
            mutating: true,
            permission: Permission::Assets,
        },
        cancel_job as Handler,
    )]
}

// ---------------------------------------------------------------------------------------------
// Worker jobs (ADR-7664): Import settings ▸ Apply and Show ▸ Build navigation preview
// ---------------------------------------------------------------------------------------------

/// What a [`WorkerJob`]'s worker sees: its job handle (progress, the agent's cancel) and the
/// background-task row's cancel flag (`editor.cancel_task`, the window's Cancel button).
#[derive(Debug, Clone)]
pub struct WorkerContext {
    /// The job's handle in the agent job registry.
    pub handle: JobHandle,
    /// The task row's cancel flag.
    pub token: crate::tasks::CancelToken,
    /// The last progress: fraction (0..=1, when known) and what the worker does.
    status: Arc<Mutex<(Option<f32>, String)>>,
}

impl WorkerContext {
    /// Whether a cancel was asked for, through any route (`editor.cancel_job`, `agent.job_cancel`,
    /// `editor.cancel_task`, a Cancel button).
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.handle.cancel_requested() || self.token.is_cancelled()
    }

    /// Sets the job's progress (the task row follows at the next [`WorkerJob::sync_row`]).
    pub fn progress(&self, fraction: Option<f64>, message: &str) {
        self.handle.progress(fraction, message);
        if let Ok(mut status) = self.status.lock() {
            *status = (fraction.map(|f| f.clamp(0.0, 1.0) as f32), message.to_string());
        }
    }

    /// The last progress fraction (0..=1), when the worker knows it.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        self.status.lock().ok().and_then(|status| status.0)
    }

    /// What the worker said it does last.
    #[must_use]
    pub fn message(&self) -> String {
        self.status
            .lock()
            .map(|status| status.1.clone())
            .unwrap_or_default()
    }
}

/// A job whose work runs on a worker thread while the editor (and the agent) stays live, and
/// whose end the editor thread applies to the world ([`tick`] polls it each frame). It is a job
/// of the agent registry (`<kind>:<n>`: `agent.job_status`, `editor.cancel_job`,
/// `agent.job_cancel`) and a background-task row (`editor.cancel_task { id: <task> }`).
///
/// The job reads `running` until the editor thread ends it (with [`Self::end`] after
/// [`Self::take`]): what the worker did and what the world shows agree when the job reads ended.
#[derive(Debug)]
pub struct WorkerJob<T> {
    /// The job id (`<kind>:<n>`).
    pub id: String,
    /// The background-task id of its row.
    pub task: &'static str,
    context: WorkerContext,
    thread: Option<std::thread::JoinHandle<T>>,
    /// The outcome [`Self::wait`] collected, until [`Self::take`] hands it out.
    ended: Option<Result<T, String>>,
}

impl<T: Send + 'static> WorkerJob<T> {
    /// Starts `work` on a thread named `thread_name` as a cancellable job of `kind`, registered in
    /// the agent job registry and [`EditorJobs`], with a background-task row `task`.
    ///
    /// # Errors
    ///
    /// When the thread cannot start (the job is failed with `job_start_failed` and the row goes).
    pub fn spawn(
        world: &mut World,
        kind: &'static str,
        task: &'static str,
        label: String,
        thread_name: &str,
        work: impl FnOnce(WorkerContext) -> T + Send + 'static,
    ) -> Result<Self, String> {
        let (id, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start(kind, label.clone(), true);
        world
            .get_resource_or_insert_with::<EditorJobs>(Default::default)
            .register(&id, handle.clone());
        let context = WorkerContext {
            handle,
            token: crate::tasks::CancelToken::new(),
            status: Arc::default(),
        };
        context.progress(Some(0.0), &label);
        world
            .get_resource_or_insert_with(crate::tasks::BackgroundTasks::default)
            .publish(crate::tasks::TaskRow {
                id: task.to_string(),
                label,
                progress: Some(0.0),
                cancel: context.token.clone(),
            });
        let worker = context.clone();
        match std::thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || work(worker))
        {
            Ok(thread) => Ok(Self {
                id,
                task,
                context,
                thread: Some(thread),
                ended: None,
            }),
            Err(e) => {
                let message = format!("cannot start the {kind} worker: {e}");
                context.handle.fail("job_start_failed", message.clone());
                if let Some(mut tasks) = world.get_resource_mut::<crate::tasks::BackgroundTasks>() {
                    tasks.finish(task);
                }
                Err(message)
            }
        }
    }

    /// The worker's context (its handle and cancel flag).
    #[must_use]
    pub fn context(&self) -> &WorkerContext {
        &self.context
    }

    /// Asks the worker to stop.
    pub fn cancel(&self) {
        self.context.handle.request_cancel();
    }

    /// Whether the worker thread has returned (its outcome waits for [`Self::take`]).
    #[must_use]
    pub fn worker_done(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// The worker's outcome once its thread has returned (`Err` when it panicked); `None` while
    /// it runs or after the outcome was taken. Never waits.
    pub fn take(&mut self) -> Option<Result<T, String>> {
        if !self.worker_done() {
            return None;
        }
        self.join()
    }

    /// Waits for the worker to return and keeps its outcome for the next [`Self::take`] (the
    /// editor's poll then applies it as usual). For tests and blocking callers.
    pub fn wait(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.ended = Some(Self::joined(thread));
        }
    }

    /// Waits for the worker and answers its outcome (`None` after it was taken).
    pub fn join(&mut self) -> Option<Result<T, String>> {
        self.wait();
        self.ended.take()
    }

    fn joined(thread: std::thread::JoinHandle<T>) -> Result<T, String> {
        thread.join().map_err(|panic| {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic".to_string());
            format!("the worker stopped: {what}")
        })
    }

    /// Copies the job's progress into its background-task row.
    pub fn sync_row(&self, world: &mut World, label: &str, fraction: Option<f32>) {
        if let Some(mut tasks) = world.get_resource_mut::<crate::tasks::BackgroundTasks>() {
            tasks.publish(crate::tasks::TaskRow {
                id: self.task.to_string(),
                label: label.to_string(),
                progress: fraction,
                cancel: self.context.token.clone(),
            });
        }
    }

    /// Ends the job: `Ok((result, cancelled))` finishes it `done` or `cancelled`, `Err((reason,
    /// message))` fails it. Removes the task row.
    pub fn end(&self, world: &mut World, outcome: Result<(Value, bool), (&str, String)>) {
        match outcome {
            Ok((result, cancelled)) => self.context.handle.finish_as(result, cancelled),
            Err((reason, message)) => self.context.handle.fail(reason, message),
        }
        if let Some(mut tasks) = world.get_resource_mut::<crate::tasks::BackgroundTasks>() {
            tasks.finish(self.task);
        }
    }
}

/// Polls the editor's worker jobs and applies the ended ones to the world: the Import settings
/// Apply ([`crate::import_settings_panel::tick_apply_job`]) and the navigation preview bake
/// ([`crate::nav_preview::tick_job`]). The editor app runs it in `Last` every frame
/// ([`tick_system`]); the windowed Import settings window and the agent methods of both also poll.
pub fn tick(world: &mut World) {
    crate::import_settings_panel::tick_apply_job(world);
    crate::nav_preview::tick_job(world);
    // Sequence ▸ Render movie…: progress, events and cancel of the running render-out (ADR-7726).
    crate::render_out_job::tick(world);
    crate::terrain_create::tick_job(world);
    // The motion-database rebuild (ADR-7740): polled here too, so a headless editor (no shell
    // frame drawing the window) ends it and frees the next start (review sprint-w11-rev-a-k #6).
    crate::mm_db_editor::tick_job(world);
    crate::project_health::tick_job(world);
    // The Retarget panel's "Retarget clips…" batch (ADR-7801).
    crate::anim::retarget_panel::tick_batch(world);
    crate::world_edit::tick(world);
    crate::blockout::tick(world);
    crate::platforms_panel::tick(world);
    crate::size_report_panel::tick(world);
    crate::content_lint_jobs::tick(world);
    // Terrain ▸ Import source (ADR-8541): a returned worker's preview is staged.
    crate::terrain_source_import::tick_job(world);
    // Terrain ▸ Overview (ADR-8596): a returned bake ends its job.
    crate::terrain_overview_job::tick_job(world);
}

/// [`tick`] as an exclusive system for the editor app's `Last` schedule.
pub fn tick_system(world: &mut World) {
    tick(world);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared worker job: it runs off the calling thread (the worker waits on a channel the
    /// test releases), every cancel route reaches it, and its end removes the task row.
    #[test]
    fn a_worker_job_runs_off_the_caller_and_ends_on_the_editor_thread() {
        let mut world = World::new();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let mut job = WorkerJob::spawn(
            &mut world,
            "import_settings",
            "test_task",
            "test".into(),
            "t",
            move |cx| {
                let _ = gate.recv();
                cx.cancelled()
            },
        )
        .expect("spawn");
        // The spawn returned while the worker waits: nothing to take yet.
        assert!(job.take().is_none());
        assert_eq!(
            world.resource::<JobRegistry>().running("import_settings"),
            Some(job.id.clone())
        );
        assert!(crate::tasks::cancel(&world, "test_task"), "the row's Cancel");
        release.send(()).expect("release");
        let outcome = job.join().expect("joined").expect("no panic");
        assert!(outcome, "the task row's cancel reached the worker");
        job.end(&mut world, Ok((json!({}), true)));
        assert_eq!(job.context().handle.state(), JobState::Cancelled);
        assert!(
            world
                .resource::<crate::tasks::BackgroundTasks>()
                .rows()
                .is_empty()
        );
        assert!(job.join().is_none(), "taken once");
    }

    #[test]
    fn cancel_job_refuses_an_unknown_id_and_reports_a_finished_one() {
        let mut world = World::new();
        let err = cancel_job(&mut world, &json!({ "id": "import:9" })).expect_err("unknown");
        assert_eq!(err.data.expect("data")["reason"], "job_not_found");
        assert!(cancel_job(&mut world, &json!({})).is_err(), "an id is required");

        let (job, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("import", "test", true);
        world
            .get_resource_or_insert_with::<EditorJobs>(Default::default)
            .register(&job, handle.clone());
        let answer = cancel_job(&mut world, &json!({ "id": job })).expect("running");
        assert_eq!(answer["requested"], true);
        assert!(handle.cancel_requested());
        handle.finish(json!({}));
        assert_eq!(
            handle.state(),
            JobState::Cancelled,
            "a cancelled job ends cancelled"
        );

        // No build running: nothing to stop.
        let answer = cancel_job(&mut world, &json!({ "id": "build" })).expect("build");
        assert_eq!(answer["requested"], false);
        assert!(!world.contains_resource::<CancelBuildRequest>());
    }

    /// Review finding 3 (sprint-rev-claude-26-k): a cancel that arrives after the worker's last
    /// cancel check cannot stop the import any more, so the files stay; the job must then end
    /// `done`, as its report says, not `cancelled` over an imported asset.
    #[test]
    fn a_cancel_after_the_last_check_leaves_a_finished_import_done() {
        let mut world = World::new();
        let (_, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("import", "test", true);
        let slot: ImportOutcome = Arc::new(Mutex::new(None));
        let report = crate::content::ImportReport {
            imported: vec!["assets/props/rock.png".to_owned()],
            cooked: 1,
            ..Default::default()
        };
        // The worker passed its last check; then the cancel lands, before the job ends.
        handle.request_cancel();
        end_import_job(&handle, &slot, Ok(report.clone()));
        assert_eq!(
            handle.state(),
            JobState::Done,
            "the import copied its files: the job is done, not cancelled"
        );
        let outcome = slot.lock().expect("slot").take().expect("outcome");
        assert_eq!(outcome, Ok(report));

        // A report the worker cancelled ends the job cancelled, even when the flag reads clear.
        let (_, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("import", "test", true);
        let cancelled = crate::content::ImportReport {
            cancelled: true,
            ..Default::default()
        };
        end_import_job(&handle, &slot, Ok(cancelled));
        assert_eq!(handle.state(), JobState::Cancelled);

        // A failed import fails its job.
        let (_, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("import", "test", true);
        end_import_job(&handle, &slot, Err("copy failed".to_owned()));
        assert_eq!(handle.state(), JobState::Failed);
    }
}
