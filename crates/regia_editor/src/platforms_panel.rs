// SPDX-License-Identifier: MPL-2.0
//! Platform setup checks, using the CLI's doctor JSON contract on an editor worker job.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::editor_jobs::{WorkerContext, WorkerJob};
use crate::viewport_agent::Handler;

/// A row of `regia doctor --json` schema 1, including its actionable fix text.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlatformCheck {
    /// Section of the doctor report.
    pub section: String,
    /// Tool or SDK component.
    pub name: String,
    /// `ok`, `warn`, `fail` or `info`.
    pub status: String,
    /// Doctor's stable diagnostic code.
    pub code: String,
    /// What the check found.
    pub detail: String,
    /// Suggested repair; displayed as text, never executed by the editor.
    pub fix: Option<String>,
}

/// The existing doctor wire document, validated before the panel accepts it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlatformReport {
    /// Doctor JSON schema version (1).
    pub schema: u32,
    /// Document discriminator (`regia_doctor`).
    pub kind: String,
    /// Host operating system.
    pub host: String,
    /// Whether all required checks passed.
    pub ok: bool,
    /// Ordered check rows.
    pub checks: Vec<PlatformCheck>,
}

impl PlatformReport {
    /// Validates a CLI document. A nonzero doctor exit can still produce a valid failed report.
    ///
    /// # Errors
    /// Malformed, unsupported or inconsistent doctor JSON.
    pub fn from_json(value: Value) -> Result<Self, String> {
        let report: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        if report.schema != 1 || report.kind != "regia_doctor" || report.checks.is_empty() {
            return Err("Expected a nonempty regia_doctor schema 1 report".into());
        }
        if report
            .checks
            .iter()
            .any(|c| !matches!(c.status.as_str(), "ok" | "warn" | "fail" | "info"))
            || report.ok == report.checks.iter().any(|c| c.status == "fail")
        {
            return Err("Doctor report has inconsistent check statuses".into());
        }
        Ok(report)
    }
}

/// Platforms panel state and the most recent check for each target.
#[derive(Resource, Default)]
pub struct PlatformsPanel {
    /// Whether the Platforms window is visible.
    pub open: bool,
    /// Platform selected for the next check; empty means the host platform.
    pub platform: String,
    /// Completed reports by platform (including failures).
    pub reports: BTreeMap<String, PlatformReport>,
    job: Option<WorkerJob<Result<PlatformReport, String>>>,
    checking: String,
    project: Option<PathBuf>,
    error: Option<String>,
    handled_frame: Option<u64>,
}

impl Drop for PlatformsPanel {
    fn drop(&mut self) {
        if let Some(job) = &mut self.job {
            job.cancel();
            job.wait();
        }
    }
}

impl PlatformsPanel {
    /// Waits for the current worker to return, for headless callers and deterministic tests.
    /// Poll the panel's agent method afterwards to publish the result to the world.
    pub fn wait(&mut self) {
        if let Some(job) = &mut self.job {
            job.wait();
        }
    }
}

pub(crate) fn error(code: &'static str, message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        code,
        message.into(),
        "See book/agents/editor-launch.md",
    )
}

fn platform_name(name: &str) -> Result<&str, RpcError> {
    match name {
        "" | "standalone" => Ok(regia_asset_pipeline::host_platform()),
        "linux" | "windows" | "android" | "macos" => Ok(name),
        _ => Err(error(
            "platform_bad_target",
            "Choose linux, windows, android or macos",
        )),
    }
}

// CLI output is bounded; the child and its descendants are killed on cancel/timeout. This worker
// helper is also used for the existing apk-size JSON command, so the editor never links the CLI.
pub(crate) fn run_cli(
    program: &Path,
    root: &Path,
    args: &[String],
    context: &WorkerContext,
) -> Result<Value, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    regia_platform::child_bind::bind_to_parent(&mut cmd);
    let group = regia_platform::child_bind::ChildGroup::new();
    let mut child = group
        .spawn(&mut cmd)
        .map_err(|e| format!("Cannot run {}: {e}", program.display()))?
        .child;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = stdout {
            pipe.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        }
        Ok::<_, std::io::Error>(bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = stderr {
            pipe.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        }
        Ok::<_, std::io::Error>(bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut failure = None;
    loop {
        if context.cancelled() || Instant::now() >= deadline {
            failure = Some(
                if context.cancelled() {
                    "Cancelled"
                } else {
                    "CLI check timed out"
                }
                .to_string(),
            );
            break;
        }
        // Keep the child unreaped while descendants may still hold its output pipes. That
        // reserves its process-group ID until cancellation/timeout can kill the owned group.
        if !out.is_finished() || !err.is_finished() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
    }
    #[cfg(unix)]
    if failure.is_some() {
        let _ = regia_platform::child_bind::kill_process_group(child.id());
    }
    #[cfg(windows)]
    drop(group);
    let _ = child.kill();
    let _ = child.wait();
    let bytes = out
        .join()
        .map_err(|_| "CLI output reader stopped".to_string())?
        .map_err(|e| e.to_string())?;
    let detail = err
        .join()
        .map_err(|_| "CLI error reader stopped".to_string())?
        .map_err(|e| e.to_string())?;
    if let Some(failure) = failure {
        return Err(failure);
    }
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("CLI report exceeds 8 MiB".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "CLI returned no valid JSON: {e}; {}",
            String::from_utf8_lossy(&detail)
        )
    })
}

fn root(world: &World) -> Result<PathBuf, RpcError> {
    crate::mesh_source::project_root(world)
        .ok_or_else(|| error("platform_no_project", "Open a project first"))
}

fn reset_for_project(world: &mut World) {
    let current = crate::mesh_source::project_root(world);
    let mut panel = world.get_resource_or_insert_with(PlatformsPanel::default);
    if panel.project != current {
        if let Some(job) = &panel.job {
            job.cancel();
        }
        panel.reports.clear();
        panel.checking.clear();
        panel.error = None;
        panel.project = current;
        if panel.platform.is_empty() {
            panel.platform = regia_asset_pipeline::host_platform().into();
        }
    }
}

/// Starts a platform check as a cancellable editor job. Never waits for the CLI in the frame.
///
/// # Errors
/// No project, invalid platform, a check already running, or a worker could not start.
pub fn recheck(world: &mut World, platform: &str) -> Result<String, RpcError> {
    tick(world);
    let platform = platform_name(platform)?.to_string();
    let root = root(world)?;
    if world
        .get_resource::<PlatformsPanel>()
        .is_some_and(|p| p.job.is_some())
    {
        return Err(error("platform_busy", "A platform check is running"));
    }
    let program = crate::package_job::program(world);
    let args = vec![
        "doctor".into(),
        "--json".into(),
        "--platform".into(),
        platform.clone(),
        "--project".into(),
        root.display().to_string(),
    ];
    let job = WorkerJob::spawn(
        world,
        "platform_check",
        "platform_check",
        format!("Checking {platform}"),
        "platform-check",
        move |context| run_cli(&program, &root, &args, &context).and_then(PlatformReport::from_json),
    )
    .map_err(|e| error("platform_check_failed", e))?;
    let id = job.id.clone();
    let mut panel = world.get_resource_or_insert_with(PlatformsPanel::default);
    panel.checking = platform.clone();
    panel.platform = platform;
    panel.error = None;
    panel.job = Some(job);
    Ok(id)
}

/// Polls the worker and publishes its result; also invalidates checks when the project changes.
#[tracing::instrument(skip_all)]
pub fn tick(world: &mut World) {
    reset_for_project(world);
    let Some(mut panel) = world.remove_resource::<PlatformsPanel>() else {
        return;
    };
    if let Some(mut job) = panel.job.take() {
        if let Some(result) = job.take() {
            let result = result.and_then(|r| r);
            let cancelled = job.context().cancelled();
            match result {
                Ok(report) if !cancelled => {
                    job.end(world, Ok((json!(&report), false)));
                    panel.reports.insert(panel.checking.clone(), report);
                }
                other => {
                    let message = other.err().unwrap_or_else(|| "Cancelled".into());
                    if cancelled {
                        job.end(world, Ok((json!({"cancelled":true}), true)));
                    } else {
                        job.end(world, Err(("platform_check_failed", message.clone())));
                    }
                    panel.error = Some(message);
                }
            }
        } else {
            job.sync_row(world, &format!("Checking {}", panel.checking), None);
            panel.job = Some(job);
        }
    }
    world.insert_resource(panel);
}

/// Whether the chosen launch target has a failed check in the current project.
#[must_use]
pub fn target_failed(world: &World, target: crate::launch::LaunchTarget) -> bool {
    let key = if target == crate::launch::LaunchTarget::Android {
        "android"
    } else {
        regia_asset_pipeline::host_platform()
    };
    world.get_resource::<PlatformsPanel>().is_some_and(|p| {
        p.project == crate::mesh_source::project_root(world)
            && (p.reports.get(key).is_some_and(|r| !r.ok) || (p.checking == key && p.error.is_some()))
    })
}

fn controls(ui: &mut egui::Ui, panel: &mut PlatformsPanel) -> egui::Response {
    egui::ComboBox::from_label("Platform")
        .selected_text(&panel.platform)
        .show_ui(ui, |ui| {
            for platform in ["linux", "windows", "android", "macos"] {
                ui.selectable_value(&mut panel.platform, platform.into(), platform);
            }
        });
    let response = crate::agent_parity::action_enabled(
        ui,
        panel.job.is_none(),
        &crate::agent_parity::PLATFORMS_RECHECK,
        None,
    );
    if panel.job.is_some() {
        ui.spinner();
        ui.label(format!("Checking {}", panel.checking));
    }
    if let Some(error) = &panel.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if let Some(report) = panel.reports.get(&panel.platform) {
        ui.label(if report.ok {
            "Platform checks passed"
        } else {
            "Platform setup needs attention"
        });
        egui::ScrollArea::vertical().show(ui, |ui| {
            for check in &report.checks {
                ui.group(|ui| {
                    ui.label(format!("{} / {}: {}", check.section, check.name, check.status));
                    ui.label(&check.detail);
                    if let Some(fix) = &check.fix {
                        ui.label(format!("Fix: {fix}"));
                    }
                });
            }
        });
    } else {
        ui.label("No check has run for this platform.");
    }
    response
}

/// Draws the Platforms window. All controls use egui's keyboard focus and inherited theme/scale.
#[tracing::instrument(skip_all)]
pub fn show(ctx: &egui::Context, world: &mut World) {
    tick(world);
    let Some(mut panel) = world.remove_resource::<PlatformsPanel>() else {
        return;
    };
    if panel.platform.is_empty() {
        panel.platform = regia_asset_pipeline::host_platform().into();
    }
    let mut open = panel.open;
    let mut recheck_clicked = false;
    egui::Window::new("Platforms").open(&mut open).show(ctx, |ui| {
        recheck_clicked = controls(ui, &mut panel).clicked();
    });
    panel.open = open;
    let frame = ctx.cumulative_frame_nr();
    let request = (recheck_clicked && panel.handled_frame != Some(frame)).then(|| {
        panel.handled_frame = Some(frame);
        panel.platform.clone()
    });
    world.insert_resource(panel);
    if let Some(platform) = request
        && let Err(e) = recheck(world, &platform)
    {
        world.resource_mut::<PlatformsPanel>().error = Some(e.message);
    }
}

fn platform_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick(world);
    if let Some(open) = params.get("open") {
        let open = open
            .as_bool()
            .ok_or_else(|| error("platform_bad_params", "open must be boolean"))?;
        world.resource_mut::<PlatformsPanel>().open = open;
    }
    let op = match params.get("op") {
        None => "status",
        Some(Value::String(op)) => op,
        _ => return Err(error("platform_bad_params", "op must be a string")),
    };
    if op == "recheck" {
        let platform = match params.get("platform") {
            None => "standalone",
            Some(Value::String(platform)) => platform,
            _ => return Err(error("platform_bad_params", "platform must be a string")),
        };
        recheck(world, platform)?;
    } else if op != "status" {
        return Err(error("platform_bad_op", "op is status or recheck"));
    }
    let panel = world.resource::<PlatformsPanel>();
    Ok(
        json!({"state": if panel.job.is_some() { "running" } else if panel.error.is_some() { "failed" } else if panel.reports.is_empty() { "idle" } else { "done" },
        "job": panel.job.as_ref().map(|j| &j.id), "reports": panel.reports, "error": panel.error, "open": panel.open}),
    )
}

/// Platform check route and schema.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.platform_check",
            tool: "platform_check",
            description: "Read cached platform checks or recheck with regia doctor as a cancellable editor job. Fix text is displayed, never executed. Errors: platform_no_project, platform_bad_target, platform_busy, platform_bad_op, platform_bad_params, platform_check_failed.",
            params: &[
                ParamSchema {
                    name: "op",
                    kind: "string",
                    required: false,
                    description: "status (default) or recheck.",
                },
                ParamSchema {
                    name: "platform",
                    kind: "string",
                    required: false,
                    description: "standalone (host), linux, windows, android or macos for recheck.",
                },
                ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "Show or hide the Platforms window.",
                },
            ],
            result: "{ state, job, reports, error, open }",
            mutating: true,
            permission: Permission::Build,
        },
        platform_check as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platforms_panel_preserves_failed_checks_and_fix_text() {
        let report = PlatformReport::from_json(json!({"schema":1,"kind":"regia_doctor","host":"linux","ok":false,
            "checks":[{"section":"android","name":"sdk","status":"fail","code":"android_sdk_missing","detail":"missing","fix":"Install the SDK"}]})).expect("failed checks are valid reports");
        assert!(!report.ok);
        assert_eq!(report.checks[0].fix.as_deref(), Some("Install the SDK"));
        let mut future = json!(report);
        future["schema"] = json!(2);
        assert!(PlatformReport::from_json(future).is_err());
    }

    #[test]
    fn platforms_panel_recheck_is_keyboard_operable() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        for scale in [1.0, 1.5] {
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(scale);
            let mut panel = PlatformsPanel {
                platform: "android".into(),
                open: false,
                reports: BTreeMap::new(),
                job: None,
                checking: String::new(),
                project: None,
                error: None,
                handled_frame: None,
            };
            let mut activated = false;
            let mut draw = |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    activated |= controls(ui, &mut panel).clicked();
                });
            };
            frame(&ctx, [1000.0, 1200.0], vec![], &mut draw);
            tab_to(&ctx, [1000.0, 1200.0], "Re-check", false, 60, &mut draw);
            frame(
                &ctx,
                [1000.0, 1200.0],
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                &mut draw,
            );
            assert!(
                activated,
                "Tab reaches and Enter activates Re-check at scale {scale}"
            );
        }
    }
}
