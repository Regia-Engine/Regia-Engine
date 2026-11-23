// SPDX-License-Identifier: MPL-2.0
//! Window ▸ Asset Jobs (spec addendum § 8.9, ADR-7788): the asset processing the editor runs in
//! the background, in one list.
//!
//! Rows come from the agent job registry (`agent.job_status`): the Content Browser import and OS drops
//! (`import:<n>`), Import settings ▸ Apply (`import_settings:<n>`), cooks (`cook:<n>`) and package
//! export and import (`package:<n>`). The thumbnail cache adds one row while pictures are queued or
//! being made. Each row shows its state (queued, running, done, failed, cancelled), its progress and
//! message; a failed row shows its error, and activating it filters the Output Log to that error. A
//! running row that can be cancelled has a Cancel button (`editor.cancel_job`).
//!
//! [`job_counts`] is the hook the status bar reads (running plus queued jobs); the panel does no work
//! while it is closed. Agents read the same rows with `editor.asset_jobs`.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use serde_json::{Value, json};

use crate::engine::ecs::prelude::*;
use crate::viewport_agent::Handler;

/// The job kinds the panel lists (registry id prefixes).
pub const ASSET_JOB_KINDS: [&str; 4] = ["import", "import_settings", "cook", "package"];

/// The panel's state (a world resource; view-only, never on the undo stack).
#[derive(Resource, Debug, Clone, Default)]
pub struct AssetJobsPanel {
    /// Whether the window shows.
    pub open: bool,
    /// The row the keyboard selection is on (a job id).
    pub selected: Option<String>,
}

/// One row of the panel.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    /// The job id (`import:3`), or `thumbnails` for the thumbnail queue.
    pub id: String,
    /// The job kind (`import`, `import_settings`, `cook`, `package`, `thumbnails`).
    pub kind: String,
    /// What the job does.
    pub label: String,
    /// `queued`, `running`, `done`, `failed` or `cancelled`.
    pub state: String,
    /// Progress 0..=1 when known.
    pub fraction: Option<f64>,
    /// What the job is doing now.
    pub message: String,
    /// A failed job's error message.
    pub error: Option<String>,
    /// Whether Cancel applies (a running, cancellable job).
    pub cancellable: bool,
}

impl JobRow {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "label": self.label,
            "state": self.state,
            "fraction": self.fraction,
            "message": self.message,
            "error": self.error,
            "cancellable": self.cancellable,
        })
    }
}

/// Counts by state, for the status bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobCounts {
    /// Waiting to start.
    pub queued: usize,
    /// Running now.
    pub running: usize,
    /// Ended well (or cancelled).
    pub done: usize,
    /// Ended with an error.
    pub failed: usize,
}

impl JobCounts {
    /// The number the status bar shows: jobs still to finish.
    #[must_use]
    pub fn active(&self) -> usize {
        self.queued + self.running
    }
}

fn no_provider(_world: &mut World, method: &str, _params: &Value) -> Result<Value, RpcError> {
    Err(RpcError::internal(format!("{method} is not an asset job")))
}

/// The panel's rows, newest last: registry jobs of the asset kinds, then the thumbnail queue.
#[must_use]
pub fn job_rows(world: &mut World) -> Vec<JobRow> {
    let listed = regia_agent::jobs::job_status(world, &json!({}), &mut no_provider).unwrap_or_default();
    let mut rows = Vec::new();
    for job in listed["jobs"].as_array().into_iter().flatten() {
        let kind = job["kind"].as_str().unwrap_or_default();
        if !ASSET_JOB_KINDS.contains(&kind) {
            continue;
        }
        let id = job["job"].as_str().unwrap_or_default().to_string();
        let state = job["state"].as_str().unwrap_or_default().to_string();
        let error = if state == "failed" {
            regia_agent::jobs::job_status(world, &json!({ "job": id }), &mut no_provider)
                .ok()
                .and_then(|detail| detail["error"]["message"].as_str().map(str::to_string))
        } else {
            None
        };
        rows.push(JobRow {
            cancellable: state == "running" && job["cancellable"].as_bool().unwrap_or(false),
            id,
            kind: kind.to_string(),
            label: job["label"].as_str().unwrap_or_default().to_string(),
            fraction: job["fraction"].as_f64(),
            message: job["message"].as_str().unwrap_or_default().to_string(),
            state,
            error,
        });
    }
    if let Some(thumbs) = world.get_resource::<crate::thumb_cache::ThumbCache>() {
        let queued = thumbs.queued() + thumbs.gpu_waiting();
        let working = thumbs.in_flight();
        if queued + working > 0 {
            rows.push(JobRow {
                id: "thumbnails".to_string(),
                kind: "thumbnails".to_string(),
                label: format!("thumbnails: {working} in progress, {queued} queued"),
                state: if working > 0 { "running" } else { "queued" }.to_string(),
                fraction: None,
                message: String::new(),
                error: None,
                cancellable: false,
            });
        }
    }
    rows
}

/// Counts of [`job_rows`] by state (the status bar's hook).
#[must_use]
pub fn job_counts(world: &mut World) -> JobCounts {
    count_rows(&job_rows(world))
}

fn count_rows(rows: &[JobRow]) -> JobCounts {
    let mut counts = JobCounts::default();
    for row in rows {
        match row.state.as_str() {
            "queued" => counts.queued += 1,
            "running" => counts.running += 1,
            "failed" => counts.failed += 1,
            _ => counts.done += 1,
        }
    }
    counts
}

/// What a click or key in the panel asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobAction {
    /// Cancel this job.
    Cancel(String),
    /// Show this error in the Output Log.
    ShowError(String),
}

/// Filters the Output Log to `text` (the shell applies it to the log panel's search).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct LogFocus(pub String);

/// Applies a panel action. Returns a status line.
pub fn apply_action(world: &mut World, action: &JobAction) -> String {
    match action {
        JobAction::Cancel(id) => match crate::editor_jobs::cancel_job(world, &json!({ "id": id })) {
            Ok(reply) => format!("cancel {id}: {}", reply["state"].as_str().unwrap_or("?")),
            Err(err) => err.message,
        },
        JobAction::ShowError(message) => {
            tracing::error!(target: "regia_editor", "asset job: {message}");
            let focus: String = message.chars().take(80).collect();
            world.insert_resource(LogFocus(focus));
            "the Output Log shows the error".to_string()
        }
    }
}

/// Moves the keyboard selection by `step` rows (wrapping), or onto the first row.
#[must_use]
pub fn step_selection(rows: &[JobRow], selected: Option<&str>, step: i32) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let len = rows.len() as i32;
    let next = match selected.and_then(|id| rows.iter().position(|row| row.id == id)) {
        Some(at) => (at as i32 + step).rem_euclid(len),
        None => 0,
    };
    rows.get(next as usize).map(|row| row.id.clone())
}

/// The action the keyboard asks for on the selected row: Delete cancels a running job; Enter shows a
/// failed job's error in the Output Log.
#[must_use]
pub fn key_action(row: &JobRow, delete: bool, enter: bool) -> Option<JobAction> {
    if delete && row.cancellable {
        return Some(JobAction::Cancel(row.id.clone()));
    }
    if enter && let Some(error) = &row.error {
        return Some(JobAction::ShowError(error.clone()));
    }
    None
}

/// Draws the rows; returns the action a click or key asked for. Pure UI over `rows` (tests drive it).
pub fn jobs_ui(ui: &mut egui::Ui, rows: &[JobRow], panel: &mut AssetJobsPanel) -> Option<JobAction> {
    let mut action = None;
    let counts = count_rows(rows);
    ui.label(format!(
        "{} running · {} queued · {} done · {} failed",
        counts.running, counts.queued, counts.done, counts.failed
    ));
    ui.separator();
    if rows.is_empty() {
        ui.weak("No asset jobs yet. Imports, cooks, thumbnails and packages show here.");
        return None;
    }
    // Keyboard: Up/Down move the selection, Delete cancels, Enter shows a failed job's error.
    let (up, down, delete, enter) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::Delete),
            i.key_pressed(egui::Key::Enter),
        )
    });
    let list_has_focus = panel.selected.is_some();
    if list_has_focus && (up || down) {
        panel.selected = step_selection(rows, panel.selected.as_deref(), if up { -1 } else { 1 });
    }
    if list_has_focus && let Some(row) = rows.iter().find(|row| Some(&row.id) == panel.selected.as_ref()) {
        action = key_action(row, delete, enter);
    }
    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
        for row in rows.iter().rev() {
            let chosen = panel.selected.as_deref() == Some(row.id.as_str());
            ui.horizontal(|ui| {
                let state = match row.state.as_str() {
                    "failed" => egui::RichText::new("failed").color(ui.visuals().error_fg_color),
                    "running" | "queued" => egui::RichText::new(row.state.as_str()).strong(),
                    other => egui::RichText::new(other).weak(),
                };
                let pick = ui
                    .selectable_label(chosen, format!("{} · {}", row.id, row.label))
                    .on_hover_text(
                        "Up/Down: move · Delete: cancel · Enter: show the error in the Output Log",
                    );
                if pick.clicked() {
                    panel.selected = Some(row.id.clone());
                }
                ui.label(state);
                if let Some(fraction) = row.fraction
                    && row.state == "running"
                {
                    ui.add(
                        egui::ProgressBar::new(fraction as f32)
                            .desired_width(80.0)
                            .show_percentage(),
                    );
                } else if row.state == "running" {
                    ui.spinner();
                }
                if !row.message.is_empty() {
                    ui.weak(row.message.as_str());
                }
                if row.cancellable && ui.button("Cancel").on_hover_text("Stop this job").clicked() {
                    action = Some(JobAction::Cancel(row.id.clone()));
                }
            });
            if let Some(error) = &row.error {
                let link = ui
                    .link(egui::RichText::new(error.as_str()).color(ui.visuals().error_fg_color))
                    .on_hover_text("Show this error in the Output Log");
                if link.clicked() {
                    action = Some(JobAction::ShowError(error.clone()));
                }
            }
        }
    });
    action
}

/// Draws Window ▸ Asset Jobs when it is open (called every editor frame; nothing runs while closed).
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(mut panel) = world.get_resource::<AssetJobsPanel>().cloned() else {
        return;
    };
    if !panel.open {
        return;
    }
    let _span = tracing::info_span!(target: "regia_editor", "asset_jobs_panel").entered();
    let rows = job_rows(world);
    let mut open = panel.open;
    let mut action = None;
    egui::Window::new("Asset Jobs")
        .default_pos(crate::tool_windows::default_pos(ctx, "Asset Jobs"))
        .open(&mut open)
        .default_width(560.0)
        .show(ctx, |ui| {
            action = jobs_ui(ui, &rows, &mut panel);
        });
    panel.open = open;
    if rows
        .iter()
        .any(|row| row.state == "running" || row.state == "queued")
    {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
    world.insert_resource(panel);
    if let Some(action) = action {
        let line = apply_action(world, &action);
        tracing::info!(target: "regia_editor", "{line}");
    }
}

/// Toggles the window (Window ▸ Asset Jobs, the `window.asset_jobs` palette command).
pub fn toggle(world: &mut World) -> String {
    let mut panel = world.get_resource_or_insert_with(AssetJobsPanel::default);
    panel.open = !panel.open;
    format!("Asset Jobs {}", if panel.open { "shown" } else { "hidden" })
}

/// `editor.asset_jobs { open?, cancel? }`.
fn asset_jobs_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(open) = params.get("open") {
        let open = open
            .as_bool()
            .ok_or_else(|| jobs_error("`open` must be a boolean", "jobs_bad_params"))?;
        world.get_resource_or_insert_with(AssetJobsPanel::default).open = open;
    }
    if let Some(id) = params.get("cancel") {
        let id = id
            .as_str()
            .ok_or_else(|| jobs_error("`cancel` must be a job id", "jobs_bad_params"))?;
        if !job_rows(world).iter().any(|row| row.id == id) {
            return Err(jobs_error(format!("no asset job {id}"), "jobs_not_found"));
        }
        crate::editor_jobs::cancel_job(world, &json!({ "id": id }))?;
    }
    let rows = job_rows(world);
    let counts = count_rows(&rows);
    Ok(json!({
        "open": world.get_resource::<AssetJobsPanel>().is_some_and(|p| p.open),
        "jobs": rows.iter().map(JobRow::to_json).collect::<Vec<_>>(),
        "counts": {
            "queued": counts.queued,
            "running": counts.running,
            "done": counts.done,
            "failed": counts.failed,
            "active": counts.active(),
        },
    }))
}

fn jobs_error(message: impl Into<String>, code: &str) -> RpcError {
    let mut error = RpcError::invalid_params(message);
    if let Some(data) = error.data.as_mut() {
        data["jobs_error"] = json!(code);
    }
    error
}

/// The Asset Jobs agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.asset_jobs",
            tool: "asset_jobs",
            description: "The Asset Jobs window (spec addendum § 8.9): queued, running, done and failed import, \
                cook, import-settings, package and thumbnail jobs with progress and errors, and the counts the \
                status bar shows. `open` shows or hides the window; `cancel` stops a running job (as its Cancel \
                button does).",
            params: &[
                ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "Show (true) or hide (false) the window.",
                },
                ParamSchema {
                    name: "cancel",
                    kind: "string",
                    required: false,
                    description: "A running job's id to cancel (`import:3`).",
                },
            ],
            result: "{ open, jobs: [{ id, kind, label, state, fraction, message, error, cancellable }], counts: { \
                queued, running, done, failed, active } }",
            mutating: true,
            permission: Permission::Assets,
        },
        asset_jobs_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_agent::jobs::JobRegistry;

    fn world_with_jobs() -> (World, Vec<regia_agent::jobs::JobHandle>) {
        let mut world = World::new();
        let mut registry = JobRegistry::default();
        let (_, running) = registry.start("import", "import 2 file(s)", true);
        running.progress(Some(0.5), "cooking");
        let (_, failed) = registry.start("cook", "cook the project", true);
        failed.fail("cook_failed", "assets/rock.glb: no mesh in the file");
        let (_, done) = registry.start("import_settings", "applying import settings", true);
        done.finish(json!({}));
        let (_, other) = registry.start("tests", "unit tests", true);
        world.insert_resource(registry);
        (world, vec![running, failed, done, other])
    }

    #[test]
    fn rows_list_asset_jobs_with_state_progress_and_errors() {
        let (mut world, _handles) = world_with_jobs();
        let rows = job_rows(&mut world);
        let kinds: Vec<&str> = rows.iter().map(|row| row.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["import", "cook", "import_settings"],
            "tests:* is not an asset job"
        );
        assert_eq!(rows[0].state, "running");
        assert_eq!(rows[0].fraction, Some(0.5));
        assert!(rows[0].cancellable);
        assert_eq!(rows[1].state, "failed");
        assert_eq!(
            rows[1].error.as_deref(),
            Some("assets/rock.glb: no mesh in the file")
        );
        assert!(!rows[1].cancellable);
        assert_eq!(rows[2].state, "done");
        let counts = job_counts(&mut world);
        assert_eq!(
            counts,
            JobCounts {
                queued: 0,
                running: 1,
                done: 1,
                failed: 1
            }
        );
        assert_eq!(counts.active(), 1, "the status bar count");
    }

    #[test]
    fn the_thumbnail_queue_is_a_queued_row() {
        let root = regia_test_utils::unique_temp_dir("asset_jobs_thumbs");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        std::fs::write(root.join("assets/m.glb"), crate::os_drop::tests::glb_triangle()).expect("glb");
        let mut world = World::new();
        let mut cache = crate::thumb_cache::ThumbCache::default();
        cache.set_root(Some(&root));
        cache.begin_frame();
        cache.request("assets/m.glb");
        world.insert_resource(cache);
        let rows = job_rows(&mut world);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].kind, "thumbnails");
        assert_eq!(rows[0].state, "queued");
        assert_eq!(job_counts(&mut world).active(), 1);
    }

    #[test]
    fn the_keyboard_moves_the_selection_cancels_and_shows_errors() {
        let (mut world, handles) = world_with_jobs();
        let rows = job_rows(&mut world);
        // Down from nothing selects the first row; Up wraps.
        let first = step_selection(&rows, None, 1);
        assert_eq!(first.as_deref(), Some(rows[0].id.as_str()));
        let wrapped = step_selection(&rows, first.as_deref(), -1);
        assert_eq!(wrapped.as_deref(), Some(rows[2].id.as_str()));
        // Delete on the running import cancels it.
        let cancel = key_action(&rows[0], true, false).expect("cancel");
        assert_eq!(cancel, JobAction::Cancel(rows[0].id.clone()));
        // The editor registers its import jobs for editor.cancel_job.
        world
            .get_resource_or_insert_with(crate::editor_jobs::EditorJobs::default)
            .register(&rows[0].id, handles[0].clone());
        apply_action(&mut world, &cancel);
        assert!(handles[0].cancel_requested(), "Delete reached the job");
        // Enter on the failed cook filters the Output Log to its error.
        let show = key_action(&rows[1], false, true).expect("show");
        apply_action(&mut world, &show);
        assert_eq!(
            world.get_resource::<LogFocus>().map(|f| f.0.as_str()),
            Some("assets/rock.glb: no mesh in the file")
        );
        // Neither key does anything on a finished job.
        assert_eq!(key_action(&rows[2], true, true), None);
    }

    #[test]
    fn the_panel_reacts_to_keys_through_egui() {
        let (mut world, _handles) = world_with_jobs();
        let rows = job_rows(&mut world);
        let ctx = egui::Context::default();
        let mut panel = AssetJobsPanel {
            open: true,
            selected: Some(rows[0].id.clone()),
        };
        let frame = |panel: &mut AssetJobsPanel, key: egui::Key| {
            let mut input = egui::RawInput::default();
            input.events.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
            let mut action = None;
            let mut output = ctx.run_ui(input, |ui| {
                action = jobs_ui(ui, &rows, panel);
            });
            output.textures_delta.clear();
            action
        };
        assert_eq!(frame(&mut panel, egui::Key::ArrowDown), None);
        assert_eq!(panel.selected.as_deref(), Some(rows[1].id.as_str()));
        assert_eq!(
            frame(&mut panel, egui::Key::Enter),
            Some(JobAction::ShowError(
                "assets/rock.glb: no mesh in the file".into()
            ))
        );
        frame(&mut panel, egui::Key::ArrowUp);
        assert_eq!(
            frame(&mut panel, egui::Key::Delete),
            Some(JobAction::Cancel(rows[0].id.clone()))
        );
    }

    #[test]
    fn the_agent_method_lists_opens_and_refuses_unknown_jobs() {
        let (mut world, _handles) = world_with_jobs();
        crate::agent_ext::install(&mut world);
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension.method("editor.asset_jobs").expect("registered");
        let reply = handler(&mut world, &json!({ "open": true })).expect("asset_jobs");
        assert_eq!(reply["open"], json!(true));
        assert_eq!(reply["jobs"].as_array().map(Vec::len), Some(3));
        assert_eq!(reply["counts"]["active"], json!(1));
        let err = handler(&mut world, &json!({ "cancel": "tests:4" })).expect_err("not an asset job");
        assert_eq!(
            err.data.and_then(|d| d.get("jobs_error").cloned()),
            Some(json!("jobs_not_found"))
        );
        assert!(toggle(&mut world).contains("hidden"));
    }
}
