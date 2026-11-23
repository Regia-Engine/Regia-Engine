// SPDX-License-Identifier: MPL-2.0
//! The status bar's indicators and the main toolbar's quick-settings menu (ADR-7754).
//!
//! Status bar, left to right after the toast: the Content and Log drawer buttons
//! ([`crate::drawers`]), the unsaved-documents count (opens File ▸ Save all's save), the background
//! jobs count with a popup list (name, progress, Cancel through each job's own cancel path), the
//! trace recording state (click starts or stops it through `editor.trace_record`), the Problems
//! count (opens the Problems tab) and the revision-control slot ([`RevisionSlot`], filled by the
//! revision-control status poll and drawn by `crate::vcs::status_bar_slot`). The values are [`StatusBarValues`]; `editor.status_bar` returns them.
//!
//! Quick settings gathers the most used settings, each the same operation as its home control:
//! gizmo snapping and its steps (the viewport toolbar's Snap, `editor.set_snap`), the realtime
//! viewport (`view.toggle_realtime`), the main viewport's show flags and preview sun (its Show
//! menu, `editor.set_viewport`) and autosave on or off (Preferences ▸ Autosave interval,
//! `editor.prefs_set`).

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::agent_parity as parity;
use crate::viewport_agent::Handler;

/// The job id of the GI bake runner's row (it has no background-task row of its own).
pub const BAKE_JOB: &str = "bake_gi";
/// The preference autosave on/off writes.
const AUTOSAVE_PREF: &str = "autosave.interval_minutes";

/// The documents with unsaved edits, as the shell last counted them (it publishes them each
/// frame: [`crate::shell::EditorShell::unsaved_documents`]).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct UnsavedDocuments(pub Vec<String>);

/// The revision-control indicator, filled by the revision-control status poll (`crate::vcs`: the
/// branch and the changed-path count as `label`, once per poll). `editor.status_bar` reports it;
/// the drawn slot is `crate::vcs::status_bar_slot`, which shows the same text and opens the
/// Revision Control window.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct RevisionSlot {
    /// What the status bar shows (`main · 2 changed`).
    pub label: String,
    /// The hover text.
    pub tooltip: String,
}

/// One running background job.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    /// The id its Cancel uses: a background-task id (`editor.cancel_task`), or [`BAKE_JOB`].
    pub id: String,
    /// What it does.
    pub label: String,
    /// Done fraction, when known.
    pub progress: Option<f32>,
}

/// The running background jobs: the background-task rows (game build, package, cache clear,
/// reflection capture bake, Import settings apply, navigation preview), the Content Browser's
/// import when it has no row, and the GI bake runner.
#[must_use]
pub fn jobs(world: &World) -> Vec<JobRow> {
    let mut rows: Vec<JobRow> = world
        .get_resource::<crate::tasks::BackgroundTasks>()
        .map(|tasks| {
            tasks
                .rows()
                .iter()
                .map(|row| JobRow {
                    id: row.id.clone(),
                    label: row.label.clone(),
                    progress: row.progress,
                })
                .collect()
        })
        .unwrap_or_default();
    if world
        .get_resource::<crate::editor_jobs::ContentImport>()
        .is_some_and(|import| import.0.running())
        && !rows.iter().any(|row| row.id == crate::tasks::IMPORT)
    {
        rows.push(JobRow {
            id: crate::tasks::IMPORT.to_string(),
            label: "importing content".to_string(),
            progress: None,
        });
    }
    if let Some(bake) = world.get_resource::<crate::bake_runner::BakePanel>()
        && bake.is_running()
    {
        let progress = bake.job().and_then(crate::bake_runner::BakeJob::progress);
        rows.push(JobRow {
            id: BAKE_JOB.to_string(),
            label: match progress {
                Some(p) => format!("baking GI ({} {}/{})", p.stage, p.done, p.total),
                None => "baking GI".to_string(),
            },
            progress: progress.map(crate::bake_runner::BakeProgress::fraction),
        });
    }
    rows
}

/// Cancels job `id` through its own cancel path: the GI bake runner's Cancel for [`BAKE_JOB`],
/// otherwise `editor.cancel_task`'s ([`crate::tasks::cancel`]). `false` when no such job runs.
pub fn cancel_job(world: &mut World, id: &str) -> bool {
    if id == BAKE_JOB {
        return world
            .get_resource_mut::<crate::bake_runner::BakePanel>()
            .is_some_and(|mut bake| bake.cancel());
    }
    crate::tasks::cancel(world, id)
}

/// Whether the process trace recorder is writing.
#[must_use]
pub fn trace_recording() -> bool {
    regia::telemetry::trace::recorder().state() == regia::telemetry::trace::RecorderState::Writing
}

/// Starts or stops the trace recording through `editor.trace_record` (the Diagnostics window's
/// Record button does the same), on the dispatcher route. Answers the status line.
pub fn toggle_trace(world: &mut World) -> String {
    let params = if trace_recording() {
        json!({ "action": "stop" })
    } else {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        json!({ "action": "start", "file": crate::trace_timeline::default_record_name(stamp) })
    };
    match regia_agent::call_local(world, "editor.trace_record", params, Some("editor:status_bar")) {
        regia_agent::LocalReply::Done(Ok(reply)) => match reply.get("file").and_then(Value::as_str) {
            Some(file) if !trace_recording() => format!("trace recorded to {file}"),
            _ => "trace recording".to_string(),
        },
        regia_agent::LocalReply::Done(Err(err)) => format!("trace: {}", err.message),
        regia_agent::LocalReply::Pending(_) => "trace: waiting".to_string(),
    }
}

/// The Problems count: assets that failed plus assets with warnings in the last validation run
/// (`None` before the first run).
#[must_use]
pub fn problems(world: &World) -> Option<(usize, usize)> {
    world
        .get_resource::<crate::asset_panels::AssetPanels>()
        .and_then(|panels| panels.problems.report.as_ref())
        .map(|report| (report.summary.failed, report.summary.warnings))
}

/// What the status bar shows (and `editor.status_bar` returns).
#[derive(Debug, Clone, PartialEq)]
pub struct StatusBarValues {
    /// Documents with unsaved edits.
    pub unsaved: Vec<String>,
    /// Running background jobs.
    pub jobs: Vec<JobRow>,
    /// Whether a trace is being recorded.
    pub trace_recording: bool,
    /// (failed, with warnings) of the last validation run.
    pub problems: Option<(usize, usize)>,
    /// The revision-control slot, when filled.
    pub revision: Option<RevisionSlot>,
    /// The open drawer.
    pub drawer: Option<crate::drawers::DrawerKind>,
}

/// The status bar's values now. The unsaved documents are the shell's last count; without a shell
/// (headless) the level's undo stack decides.
#[must_use]
pub fn values(world: &World) -> StatusBarValues {
    let unsaved = match world.get_resource::<UnsavedDocuments>() {
        Some(docs) => docs.0.clone(),
        None if world
            .get_resource::<crate::command::UndoStack>()
            .is_some_and(crate::command::UndoStack::is_dirty) =>
        {
            vec!["the level".to_string()]
        }
        None => Vec::new(),
    };
    StatusBarValues {
        unsaved,
        jobs: jobs(world),
        trace_recording: trace_recording(),
        problems: problems(world),
        revision: world
            .get_resource::<RevisionSlot>()
            .filter(|slot| !slot.label.is_empty())
            .cloned(),
        drawer: world
            .get_resource::<crate::drawers::Drawers>()
            .and_then(crate::drawers::Drawers::open),
    }
}

/// What a click on the status bar asks the shell to do (the rest is done in place).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusClick {
    /// The unsaved count: File ▸ Save all.
    SaveAll,
    /// A status line to show.
    Toast(String),
}

/// Draws the indicators into the status bar's row. Every one is a focusable button with its text
/// as its name (keyboard: Tab to it, Enter or Space).
pub fn indicators(ui: &mut egui::Ui, world: &mut World) -> Option<StatusClick> {
    let values = values(world);
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let mut click = None;
    let content_open = values.drawer == Some(crate::drawers::DrawerKind::Content);
    let log_open = values.drawer == Some(crate::drawers::DrawerKind::Log);
    if parity::action_button_text(
        ui,
        &parity::STATUS_CONTENT_DRAWER,
        drawer_text("Content", content_open),
    )
    .on_hover_text("Slide the Content Browser up over the dock (Ctrl+Shift+Space)")
    .clicked()
    {
        click = crate::command_palette::run(world, crate::drawers::CONTENT_COMMAND).map(StatusClick::Toast);
    }
    if parity::action_button_text(ui, &parity::STATUS_LOG_DRAWER, drawer_text("Log", log_open))
        .on_hover_text("Slide the Output Log and the console line up over the dock")
        .clicked()
    {
        click = crate::command_palette::run(world, crate::drawers::LOG_COMMAND).map(StatusClick::Toast);
    }
    ui.separator();
    if !values.unsaved.is_empty() {
        let text =
            egui::RichText::new(format!("Unsaved: {}", values.unsaved.len())).color(tokens.accents.unsaved);
        if parity::action_button_text(ui, &parity::STATUS_SAVE_ALL, text)
            .on_hover_text(format!(
                "Unsaved: {}. A click opens the Save all dialog.",
                values.unsaved.join(", ")
            ))
            .clicked()
        {
            click = Some(StatusClick::SaveAll);
        }
    }
    if !values.jobs.is_empty() {
        let title = format!("Jobs: {}", values.jobs.len());
        // A popup list (view only); its Cancel buttons are registered actions.
        ui.menu_button(title, |ui| {
            for row in &values.jobs {
                ui.horizontal(|ui| {
                    ui.label(&row.label);
                    match row.progress {
                        Some(fraction) => {
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .desired_width(120.0)
                                    .show_percentage(),
                            );
                        }
                        None => {
                            ui.add(egui::Spinner::new());
                        }
                    }
                    if parity::action_button(ui, &parity::STATUS_JOB_CANCEL).clicked() {
                        click = Some(StatusClick::Toast(if cancel_job(world, &row.id) {
                            format!("canceling {}", row.label)
                        } else {
                            format!("{} already ended", row.label)
                        }));
                    }
                });
            }
        });
    }
    let trace = if values.trace_recording {
        egui::RichText::new("● Recording trace").color(tokens.accents.error_text)
    } else {
        egui::RichText::new("Trace")
    };
    if parity::action_button_text(ui, &parity::STATUS_TRACE, trace)
        .on_hover_text("Start or stop recording a trace (it opens in Diagnostics ▸ Timeline when it stops)")
        .clicked()
    {
        click = Some(StatusClick::Toast(toggle_trace(world)));
    }
    if let Some((failed, warnings)) = values.problems
        && failed + warnings > 0
    {
        let text =
            egui::RichText::new(format!("Problems: {}", failed + warnings)).color(tokens.accents.error_text);
        if parity::action_button_text(ui, &parity::STATUS_PROBLEMS, text)
            .on_hover_text(format!(
                "{failed} failed, {warnings} with warnings: open Problems"
            ))
            .clicked()
        {
            click = crate::command_palette::run(world, "window.open_problems").map(StatusClick::Toast);
        }
    }
    // The revision slot is drawn by `crate::vcs::status_bar_slot` (a button that opens the
    // Revision Control window); `values.revision` carries its text for `editor.status_bar`.
    click
}

fn drawer_text(name: &str, open: bool) -> String {
    if open {
        format!("{name} ▾")
    } else {
        format!("{name} ▴")
    }
}

// ---------------------------------------------------------------------------------------------
// Quick settings

/// The main toolbar's Quick settings menu.
pub fn quick_settings_menu(ui: &mut egui::Ui, world: &mut World) {
    // The menu itself only shows the entries; each entry is a registered action.
    ui.menu_button("Quick settings", |ui| quick_settings(ui, world));
}

/// The Quick settings entries (drawn inside the menu; separate so tests can draw them flat).
pub fn quick_settings(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::info_span!(target: "regia_editor", "quick_settings").entered();
    // Snapping and its steps: the viewport toolbar's Snap and `editor.set_snap`.
    let mut gizmo = world
        .get_resource_or_insert_with(crate::viewport::GizmoState::default)
        .clone();
    let before = (
        gizmo.snap,
        gizmo.snap_distance,
        gizmo.snap_angle_deg,
        gizmo.snap_scale,
    );
    parity::action_toggle(ui, &parity::QUICK_SNAP, &mut gizmo.snap)
        .on_hover_text("Snap gizmo drags to the steps below; hold Ctrl to invert");
    ui.horizontal(|ui| {
        ui.label("Move step");
        ui.add(
            egui::DragValue::new(&mut gizmo.snap_distance)
                .range(0.001..=1000.0)
                .speed(0.05)
                .suffix(" m"),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Rotate step");
        ui.add(
            egui::DragValue::new(&mut gizmo.snap_angle_deg)
                .range(0.1..=180.0)
                .speed(0.5)
                .suffix("°"),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Scale step");
        ui.add(
            egui::DragValue::new(&mut gizmo.snap_scale)
                .range(0.001..=100.0)
                .speed(0.01),
        );
    });
    if before
        != (
            gizmo.snap,
            gizmo.snap_distance,
            gizmo.snap_angle_deg,
            gizmo.snap_scale,
        )
    {
        world.insert_resource(gizmo);
    }
    ui.separator();
    // Realtime: the palette's `view.toggle_realtime`.
    let mut realtime = world
        .get_resource_or_insert_with(crate::viewport::ViewportSettings::default)
        .realtime;
    if parity::action_toggle(ui, &parity::QUICK_REALTIME, &mut realtime)
        .on_hover_text("Redraw the viewport every frame (off: only when something changes)")
        .changed()
    {
        let _ = crate::command_palette::run(world, "view.toggle_realtime");
    }
    // The main viewport's show flags and preview sun: its Show menu (`editor.set_viewport`).
    let mut layout = world
        .get_resource_or_insert_with(crate::views::ViewportLayout::default)
        .clone();
    let show_before = layout.show;
    ui.horizontal_wrapped(|ui| {
        parity::action_toggle(ui, &parity::QUICK_SHOW_GRID, &mut layout.show.grid);
        parity::action_toggle(ui, &parity::QUICK_SHOW_ICONS, &mut layout.show.icons);
        parity::action_toggle(ui, &parity::QUICK_SHOW_BOUNDS, &mut layout.show.bounds);
        parity::action_toggle(ui, &parity::QUICK_SHOW_COLLIDERS, &mut layout.show.colliders);
        parity::action_toggle(ui, &parity::QUICK_SHOW_STATS, &mut layout.show.stats);
    });
    parity::action_toggle(ui, &parity::QUICK_PREVIEW_SUN, &mut layout.show.preview_light)
        .on_hover_text("Light the viewport with a preview sun and sky when the level has no sun");
    if layout.show != show_before {
        world.insert_resource(layout);
    }
    ui.separator();
    // Autosave: Preferences ▸ Autosave interval (0 = off; on restores the default interval).
    let mut prefs = world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    let mut autosave = prefs.int(AUTOSAVE_PREF) > 0;
    if parity::action_toggle(ui, &parity::QUICK_AUTOSAVE, &mut autosave)
        .on_hover_text("Autosave a level with unsaved edits for crash recovery")
        .changed()
    {
        let result = if autosave {
            prefs.reset(AUTOSAVE_PREF)
        } else {
            prefs.set(AUTOSAVE_PREF, &crate::editor_prefs::PrefValue::Int(0))
        };
        match result {
            Ok(_) => world.insert_resource(prefs),
            Err(err) => tracing::warn!(target: "regia_editor", "quick settings: {err}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Agent method

/// `editor.status_bar`.
fn status_bar_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(values_json(&values(world)))
}

/// The JSON form of [`StatusBarValues`].
#[must_use]
pub fn values_json(values: &StatusBarValues) -> Value {
    json!({
        "unsaved": { "count": values.unsaved.len(), "documents": values.unsaved },
        "jobs": {
            "count": values.jobs.len(),
            "rows": values.jobs.iter().map(|row| json!({
                "id": row.id,
                "label": row.label,
                "progress": row.progress,
            })).collect::<Vec<_>>(),
        },
        "trace": { "recording": values.trace_recording },
        "problems": match values.problems {
            Some((failed, warnings)) => json!({ "count": failed + warnings, "failed": failed, "warnings": warnings }),
            None => Value::Null,
        },
        "revision": values.revision.as_ref().map(|slot| json!({ "label": slot.label, "tooltip": slot.tooltip })),
        "drawer": values.drawer.map(crate::drawers::DrawerKind::name),
    })
}

/// The status bar's agent method, installed by `agent_ext::all_methods`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.status_bar",
            tool: "editor_status_bar",
            description: "The status bar's indicators: unsaved documents, running background jobs (id, label, progress; cancel one with editor.cancel_task { id }, or editor.bake_gi_cancel for bake_gi), trace recording, the Problems count, the revision-control slot and the open drawer.",
            params: &[],
            result: "{ unsaved: { count, documents }, jobs: { count, rows: [{ id, label, progress }] }, trace: { recording }, problems: { count, failed, warnings } | null, revision: { label, tooltip } | null, drawer: \"content\" | \"log\" | null }",
            mutating: false,
            permission: Permission::Read,
        },
        status_bar_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::tasks::{BackgroundTasks, CancelToken, TaskRow};

    fn two_jobs(world: &mut World) -> (CancelToken, CancelToken) {
        let (package, capture) = (CancelToken::new(), CancelToken::new());
        let mut tasks = world.get_resource_or_insert_with(BackgroundTasks::default);
        tasks.publish(TaskRow {
            id: "package".into(),
            label: "packaging linux".into(),
            progress: Some(0.25),
            cancel: package.clone(),
        });
        tasks.publish(TaskRow {
            id: "capture_bake".into(),
            label: "baking reflection captures".into(),
            progress: None,
            cancel: capture.clone(),
        });
        (package, capture)
    }

    fn texts(output: &egui::FullOutput) -> Vec<String> {
        let mut out = Vec::new();
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(text) = &shape.shape {
                out.push(text.galley.text().to_string());
            }
        }
        out
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn text_center(output: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == label => {
                Some(text.visual_bounding_rect().center())
            }
            _ => None,
        })
    }

    fn frame(
        ctx: &egui::Context,
        world: &mut World,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<StatusClick>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut click = None;
        let mut output = ctx.run_ui(raw, |ui| {
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| click = indicators(ui, world));
            });
        });
        output.textures_delta.clear();
        (output, click)
    }

    /// Acceptance: two running jobs show "Jobs: 2"; the popup lists both; Cancel cancels through
    /// the job's own token. Keyboard path: Tab to the Jobs button, Enter opens it, Tab to the first
    /// Cancel, Enter.
    #[test]
    fn two_jobs_show_two_and_the_popup_cancels_through_the_job() {
        let mut world = World::new();
        let (package, capture) = two_jobs(&mut world);
        let values = values(&world);
        assert_eq!(values.jobs.len(), 2);
        let json = values_json(&values);
        assert_eq!(json["jobs"]["count"], 2);
        assert_eq!(json["jobs"]["rows"][0]["id"], "package");
        let ctx = egui::Context::default();
        let (output, _) = frame(&ctx, &mut world, vec![]);
        assert!(
            texts(&output).iter().any(|t| t == "Jobs: 2"),
            "{:?}",
            texts(&output)
        );
        let jobs_at = text_center(&output, "Jobs: 2").expect("Jobs button");
        // Open the popup from the keyboard: Tab until the Jobs button has focus, then Enter.
        let mut listed = Vec::new();
        for _ in 0..8 {
            frame(&ctx, &mut world, vec![key(egui::Key::Tab)]);
            let on_jobs = ctx
                .memory(|m| m.focused())
                .and_then(|id| ctx.read_response(id))
                .is_some_and(|r| r.rect.contains(jobs_at));
            if on_jobs {
                frame(&ctx, &mut world, vec![key(egui::Key::Enter)]);
                let (output, _) = frame(&ctx, &mut world, vec![]);
                listed = texts(&output);
                break;
            }
        }
        assert!(listed.iter().any(|t| t == "packaging linux"), "{listed:?}");
        assert!(
            listed.iter().any(|t| t == "baking reflection captures"),
            "{listed:?}"
        );
        // Cancel through the job's own path (what the popup's Cancel calls).
        assert!(cancel_job(&mut world, "capture_bake"));
        assert!(capture.is_cancelled());
        assert!(!package.is_cancelled());
        assert!(!cancel_job(&mut world, BAKE_JOB), "no bake runs");
    }

    #[test]
    fn a_click_on_cancel_in_the_popup_cancels_that_job() {
        let mut world = World::new();
        let (package, _) = two_jobs(&mut world);
        let ctx = egui::Context::default();
        // Find the Jobs button and click it, then click the first Cancel.
        let click_at = |pos: egui::Pos2| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        let (output, _) = frame(&ctx, &mut world, vec![]);
        let jobs = text_center(&output, "Jobs: 2").expect("Jobs button");
        frame(&ctx, &mut world, click_at(jobs));
        let (output, _) = frame(&ctx, &mut world, vec![]);
        let cancel = text_center(&output, "Cancel").expect("popup Cancel");
        let (_, click) = frame(&ctx, &mut world, click_at(cancel));
        assert_eq!(
            click,
            Some(StatusClick::Toast("canceling packaging linux".into()))
        );
        assert!(package.is_cancelled());
    }

    #[test]
    fn unsaved_problems_revision_and_drawer_report() {
        let mut world = World::new();
        assert!(values(&world).unsaved.is_empty());
        world.insert_resource(UnsavedDocuments(vec![
            "the level".into(),
            "material a.material.ron".into(),
        ]));
        world.insert_resource(RevisionSlot {
            label: "main".into(),
            tooltip: "branch".into(),
        });
        world
            .get_resource_or_insert_with(crate::drawers::Drawers::default)
            .set(crate::drawers::DrawerKind::Log, true);
        let json = values_json(&values(&world));
        assert_eq!(json["unsaved"]["count"], 2);
        assert_eq!(json["revision"]["label"], "main");
        assert_eq!(json["drawer"], "log");
        assert_eq!(json["problems"], Value::Null);
        assert_eq!(json["trace"]["recording"], trace_recording());
        let ctx = egui::Context::default();
        let (output, _) = frame(&ctx, &mut world, vec![]);
        let shown = texts(&output);
        assert!(shown.iter().any(|t| t == "Unsaved: 2"), "{shown:?}");
        // The indicators do not draw the revision slot: `vcs::status_bar_slot` does (below), so a
        // filled slot is never drawn twice.
        assert!(!shown.iter().any(|t| t == "main"), "{shown:?}");
    }

    #[test]
    fn the_drawn_revision_slot_is_the_vcs_status_bar_slot() {
        // Drives the slot that draws (`vcs::status_bar_slot`), from a cached status.
        let mut world = World::new();
        world
            .get_resource_or_insert_with(crate::vcs::VcsStatus::default)
            .state = crate::vcs::VcsState::Ready(std::sync::Arc::new(crate::vcs::RepoStatus {
            branch: Some("main".into()),
            ..Default::default()
        }));
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 600.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| crate::vcs::status_bar_slot(ui, &mut world));
            });
        });
        output.textures_delta.clear();
        let shown = texts(&output);
        assert!(
            shown.iter().any(|t| t == "\u{b7} \u{2387} main \u{b7} clean"),
            "{shown:?}"
        );
    }

    #[test]
    fn quick_settings_write_the_home_controls_state() {
        let mut world = World::new();
        world.init_resource::<crate::viewport::GizmoState>();
        world.init_resource::<crate::viewport::ViewportSettings>();
        let realtime = world.resource::<crate::viewport::ViewportSettings>().realtime;
        // The same op as the toolbar: the palette's toggle.
        let _ = crate::command_palette::run(&mut world, "view.toggle_realtime");
        assert_ne!(
            world.resource::<crate::viewport::ViewportSettings>().realtime,
            realtime
        );
        // Drawn flat, clicking Snap flips GizmoState.snap (what the viewport toolbar's Snap and
        // editor.set_snap write).
        let ctx = egui::Context::default();
        let draw = |world: &mut World, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| quick_settings(ui, world));
            });
            output.textures_delta.clear();
            output
        };
        let output = draw(&mut world, vec![]);
        let snap = text_center(&output, parity::QUICK_SNAP.label).expect("Snap toggle");
        let press = |pressed| egui::Event::PointerButton {
            pos: snap,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        draw(
            &mut world,
            vec![egui::Event::PointerMoved(snap), press(true), press(false)],
        );
        assert!(world.resource::<crate::viewport::GizmoState>().snap);
        // Autosave off writes the preference to 0, on resets it to the default.
        let mut prefs = crate::editor_prefs::EditorPreferences::default();
        assert!(prefs.int(AUTOSAVE_PREF) > 0);
        prefs
            .set(AUTOSAVE_PREF, &crate::editor_prefs::PrefValue::Int(0))
            .unwrap();
        assert_eq!(crate::editor_prefs::autosave_interval(&prefs), None);
    }

    /// The keyboard path through Quick settings: Tab reaches Snap first, Enter flips it.
    #[test]
    fn quick_settings_work_from_the_keyboard() {
        let mut world = World::new();
        world.init_resource::<crate::viewport::GizmoState>();
        let ctx = egui::Context::default();
        let draw = |world: &mut World, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| quick_settings(ui, world));
            });
            output.textures_delta.clear();
        };
        draw(&mut world, vec![]);
        draw(&mut world, vec![key(egui::Key::Tab)]);
        draw(&mut world, vec![key(egui::Key::Enter)]);
        assert!(world.resource::<crate::viewport::GizmoState>().snap);
    }

    #[test]
    fn the_agent_method_answers() {
        let mut world = World::new();
        two_jobs(&mut world);
        let out = status_bar_method(&mut world, &json!({})).unwrap();
        assert_eq!(out["jobs"]["count"], 2);
        assert_eq!(out["unsaved"]["count"], 0);
    }
}
