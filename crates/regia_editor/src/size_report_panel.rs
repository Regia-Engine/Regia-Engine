// SPDX-License-Identifier: MPL-2.0
//! Package byte accounting for desktop bundles and Android archives (P-344, ADR-7843).
//!
//! Counts actual package files, never source estimates. Pak payloads use their stored sizes and
//! retain a separate overhead row. Unobserved plugin systems are not evidence of unused features.

use crate::editor_jobs::{WorkerContext, WorkerJob};
use crate::viewport_agent::Handler;
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Maximum number of package files or archive entries accepted by one report.
pub const MAX_ENTRIES: usize = 100_000;

/// One physical file or payload within a pak.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SizeEntry {
    /// Package-relative path; pak payload paths use `pak-path::asset-path`.
    pub path: String,
    /// Category compatible with apk-size, with finer desktop asset kinds.
    pub category: String,
    /// Stored bytes; archive and pak overhead is listed separately.
    pub bytes: u64,
}

/// A completed size report; categories sum to `content_bytes`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SizeReport {
    /// Input package or directory.
    pub path: PathBuf,
    /// `installed_contents` for directories, `archive_entries` for APK/AAB/ZIP.
    pub measurement: String,
    /// Actual sum of counted files, or archive size including overhead.
    pub content_bytes: u64,
    /// Total stored bytes per category.
    pub categories: BTreeMap<String, u64>,
    /// Largest physical files, ordered by bytes descending then path.
    pub largest: Vec<SizeEntry>,
    /// Largest individual asset payloads, separate from their containing pak files.
    pub largest_assets: Vec<SizeEntry>,
    /// Caveats and existing apk-size packaging warnings.
    pub warnings: Vec<String>,
}

/// Size Report window and its background work.
#[derive(Resource, Default)]
pub struct SizeReportPanel {
    /// Whether the window is visible.
    pub open: bool,
    /// Latest completed report.
    pub report: Option<SizeReport>,
    job: Option<WorkerJob<Result<SizeReport, String>>>,
    path: Option<PathBuf>,
    project: Option<PathBuf>,
    seen_package: Option<String>,
    seen_launch: Option<u32>,
    error: Option<String>,
    handled_frame: Option<u64>,
}

impl Drop for SizeReportPanel {
    fn drop(&mut self) {
        if let Some(job) = &mut self.job {
            job.cancel();
            job.wait();
        }
    }
}

impl SizeReportPanel {
    /// Waits for the current worker to return, for headless callers and deterministic tests.
    /// Poll the panel's agent method afterwards to publish the result to the world.
    pub fn wait(&mut self) {
        if let Some(job) = &mut self.job {
            job.wait();
        }
    }
}

fn err(code: &'static str, message: impl Into<String>) -> RpcError {
    crate::platforms_panel::error(code, message)
}

fn kind(path: &str, type_path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    let ty = type_path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    if ty.contains("audio") || matches!(ext, "wav" | "ogg" | "opus" | "mp3" | "flac") {
        "audio".into()
    } else if ty.contains("shader") || matches!(ext, "wgsl" | "spv" | "pipeline_cache" | "fpc") {
        "shaders".into()
    } else if ty.contains("texture") || matches!(ext, "png" | "jpg" | "jpeg" | "ktx2" | "dds") {
        "textures".into()
    } else if ty.contains("mesh") || matches!(ext, "gltf" | "glb") {
        "meshes".into()
    } else if ty.contains("animation") {
        "animations".into()
    } else if lower.ends_with(".level.ron") || lower.ends_with(".prefab.ron") {
        "levels_and_prefabs".into()
    } else if lower.ends_with(".fpak") {
        "paks".into()
    } else if matches!(ext, "exe" | "dll" | "so" | "dylib") {
        "engine_binary".into()
    } else if lower.starts_with("assets/") || !type_path.is_empty() {
        "raw_assets".into()
    } else {
        "other".into()
    }
}

fn add(totals: &mut BTreeMap<String, u64>, category: String, bytes: u64) -> Result<(), String> {
    let total = totals.entry(category).or_default();
    *total = total.checked_add(bytes).ok_or("Package size overflow")?;
    Ok(())
}

fn top(entries: &mut Vec<SizeEntry>) {
    entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    entries.truncate(20);
}

/// Measures a desktop package directory without following symlinks or decompressing pak payloads.
///
/// # Errors
/// Unreadable files/paks, symlinks, cancellation, excessive entry count or size overflow.
pub fn directory_report(root: &Path, cancelled: impl Fn() -> bool) -> Result<SizeReport, String> {
    if std::fs::symlink_metadata(root)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Package directory is a symlink".into());
    }
    let mut stack = vec![root.to_owned()];
    let mut files = Vec::new();
    let mut assets = Vec::new();
    let mut categories = BTreeMap::new();
    let mut content_bytes = 0u64;
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            if cancelled() {
                return Err("Cancelled".into());
            }
            visited += 1;
            if visited > MAX_ENTRIES {
                return Err("Package exceeds 100000 entries".into());
            }
            let entry = entry.map_err(|e| e.to_string())?;
            let file_type = entry.file_type().map_err(|e| e.to_string())?;
            if file_type.is_symlink() {
                return Err(format!(
                    "Package symlink is not followed: {}",
                    entry.path().display()
                ));
            }
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                return Err(format!("Not a regular package file: {}", entry.path().display()));
            }
            let path = entry.path();
            let name = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = entry.metadata().map_err(|e| e.to_string())?.len();
            content_bytes = content_bytes.checked_add(bytes).ok_or("Package size overflow")?;
            let mut category = kind(&name, "");
            if category == "other" {
                let mut magic = [0; 4];
                let count = std::fs::File::open(&path)
                    .and_then(|mut f| f.read(&mut magic))
                    .map_err(|e| e.to_string())?;
                if (count >= 4 && magic == *b"\x7fELF") || (count >= 2 && magic[..2] == *b"MZ") {
                    category = "engine_binary".into();
                }
            }
            files.push(SizeEntry {
                path: name.clone(),
                category: category.clone(),
                bytes,
            });
            if category != "paks" {
                add(&mut categories, category, bytes)?;
                continue;
            }
            let pak = regia_asset::PakReader::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut payload_bytes = 0u64;
            let mut seen = BTreeSet::new();
            for asset in pak.manifest() {
                if cancelled() {
                    return Err("Cancelled".into());
                }
                if !seen.insert(asset.id) {
                    continue;
                }
                visited += 1;
                if visited > MAX_ENTRIES {
                    return Err("Package exceeds 100000 entries".into());
                }
                let entry = pak
                    .entry(asset.id)
                    .ok_or_else(|| format!("Missing pak entry for {}", asset.path))?;
                let category = kind(&asset.path, &asset.type_path);
                payload_bytes = payload_bytes
                    .checked_add(entry.compressed_size)
                    .ok_or("Pak size overflow")?;
                add(&mut categories, category.clone(), entry.compressed_size)?;
                assets.push(SizeEntry {
                    path: format!("{name}::{}", asset.path),
                    category,
                    bytes: entry.compressed_size,
                });
            }
            let overhead = bytes
                .checked_sub(payload_bytes)
                .ok_or("Pak payloads exceed the physical file size")?;
            add(&mut categories, "pak_overhead".into(), overhead)?;
        }
    }
    top(&mut files);
    top(&mut assets);
    Ok(SizeReport {
        path: root.into(),
        measurement: "installed_contents".into(),
        content_bytes,
        categories,
        largest: files,
        largest_assets: assets,
        warnings: Vec::new(),
    })
}

fn archive_report(
    path: &Path,
    program: &Path,
    root: &Path,
    context: &WorkerContext,
) -> Result<SizeReport, String> {
    let args = vec![
        "apk-size".into(),
        path.display().to_string(),
        "--json".into(),
        "--top".into(),
        MAX_ENTRIES.to_string(),
    ];
    let value = crate::platforms_panel::run_cli(program, root, &args, context)?;
    let entries = value["largest"]
        .as_array()
        .ok_or("apk-size returned no entries")?;
    let count = value["entries"]
        .as_u64()
        .ok_or("apk-size returned no entry count")?;
    if count > MAX_ENTRIES as u64 {
        return Err("Archive exceeds 100000 entries".into());
    }
    let content_bytes = value["file_bytes"]
        .as_u64()
        .ok_or("apk-size returned no archive size")?;
    let mut categories = BTreeMap::new();
    let mut files = Vec::new();
    let mut payload = 0u64;
    for entry in entries {
        let name = entry["name"].as_str().ok_or("Invalid archive entry name")?;
        let bytes = entry["compressed"].as_u64().ok_or("Invalid archive entry size")?;
        // Preserve apk-size's container categories; refine only raw assets and desktop binaries.
        let archive_roots = ["lib/", "assets/", "res/", "dex/", "manifest/"];
        let bare = if archive_roots.iter().any(|prefix| name.starts_with(prefix)) {
            name
        } else {
            name.split_once('/')
                .map(|(_, rest)| rest)
                .filter(|rest| {
                    archive_roots.iter().any(|prefix| rest.starts_with(prefix)) || *rest == "resources.pb"
                })
                .unwrap_or(name)
        };
        let category = if bare.starts_with("lib/") {
            "engine_binary".into()
        } else if bare.starts_with("META-INF/") {
            "signature".into()
        } else if bare.ends_with(".dex") {
            "dex".into()
        } else if bare.starts_with("res/")
            || bare.ends_with("resources.arsc")
            || bare.ends_with("resources.pb")
        {
            "resources".into()
        } else if bare.ends_with("AndroidManifest.xml") {
            "manifest".into()
        } else {
            kind(bare, "")
        };
        add(&mut categories, category.clone(), bytes)?;
        payload = payload.checked_add(bytes).ok_or("Archive size overflow")?;
        files.push(SizeEntry {
            path: name.into(),
            category,
            bytes,
        });
    }
    add(
        &mut categories,
        "archive_overhead".into(),
        content_bytes
            .checked_sub(payload)
            .ok_or("Archive entries exceed file size")?,
    )?;
    top(&mut files);
    let mut warnings: Vec<String> = value["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    warnings.push("Archive pak payloads are grouped as paks; per-kind payload sizes require a desktop package directory.".into());
    Ok(SizeReport {
        path: path.into(),
        measurement: "archive_entries".into(),
        content_bytes,
        categories,
        largest: files,
        largest_assets: Vec::new(),
        warnings,
    })
}

fn start(world: &mut World, path: PathBuf) -> Result<String, RpcError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| err("package_size_no_project", "Open a project first"))?;
    if world
        .get_resource::<SizeReportPanel>()
        .is_some_and(|p| p.job.is_some())
    {
        return Err(err("package_size_busy", "A size report is running"));
    }
    let program = crate::package_job::program(world);
    let input = path.clone();
    let job = WorkerJob::spawn(
        world,
        "package_size_report",
        "package_size_report",
        "Reading package sizes".into(),
        "package-size",
        move |context| {
            if input.is_dir() {
                directory_report(&input, || context.cancelled())
            } else if input
                .extension()
                .is_some_and(|e| matches!(e.to_str(), Some("apk" | "aab" | "zip")))
            {
                archive_report(&input, &program, &root, &context)
            } else {
                Err("Choose the published desktop package directory, an APK, AAB or ZIP".into())
            }
        },
    )
    .map_err(|e| err("package_size_failed", e))?;
    let id = job.id.clone();
    let mut panel = world.get_resource_or_insert_with(SizeReportPanel::default);
    panel.job = Some(job);
    panel.path = Some(path);
    panel.error = None;
    panel.report = None;
    Ok(id)
}

fn published_input(value: &Value) -> Option<PathBuf> {
    if value["ok"] != true || value["report"]["dry_run"] == true {
        return None;
    }
    let artifact = value["artifact"].as_str().map(PathBuf::from)?;
    if artifact.is_dir()
        || artifact
            .extension()
            .is_some_and(|e| matches!(e.to_str(), Some("apk" | "aab" | "zip")))
    {
        return Some(artifact);
    }
    // The CLI retains the desktop directory beside its tar/AppImage. Use only a directory
    // published by this package job; never scan a project's unrelated dist folders.
    value["published"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .find_map(|p| {
            if p.is_dir() && p.join("build_info.json").is_file() {
                Some(p)
            } else if p.file_name().is_some_and(|n| n == "build_info.json") {
                p.parent().map(Path::to_owned)
            } else {
                None
            }
        })
}

/// Applies completed reports and starts one after each successful editor package job.
#[tracing::instrument(skip_all)]
pub fn tick(world: &mut World) {
    let root = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<SizeReportPanel>().unwrap_or_default();
    if panel.project != root {
        if let Some(job) = &panel.job {
            job.cancel();
        }
        let replacing_project = panel.project.is_some();
        panel.project = root;
        panel.report = None;
        panel.path = None;
        panel.error = None;
        panel.seen_package = replacing_project
            .then(|| {
                world
                    .get_resource::<crate::package_job::CliJobs>()
                    .and_then(|jobs| jobs.package.as_ref())
                    .map(|job| job.id.clone())
            })
            .flatten();
        panel.seen_launch = replacing_project
            .then(|| {
                world
                    .get_resource::<crate::launch::Launcher>()
                    .and_then(|l| l.status())
                    .map(|s| s.id)
            })
            .flatten();
    }
    if let Some(mut job) = panel.job.take() {
        if let Some(outcome) = job.take() {
            let cancelled = job.context().cancelled();
            match outcome.and_then(|r| r) {
                Ok(report) if !cancelled => {
                    job.end(world, Ok((json!(&report), false)));
                    panel.report = Some(report);
                }
                other => {
                    let message = other.err().unwrap_or_else(|| "Cancelled".into());
                    if cancelled {
                        job.end(world, Ok((json!({"cancelled":true}), true)));
                    } else {
                        job.end(world, Err(("package_size_failed", message.clone())));
                    }
                    panel.error = Some(message);
                }
            }
        } else {
            job.sync_row(world, "Reading package sizes", None);
            panel.job = Some(job);
        }
    }
    let next = world
        .get_resource::<crate::package_job::CliJobs>()
        .and_then(|jobs| jobs.package.as_ref())
        .filter(|job| !job.running() && panel.job.is_none() && panel.seen_package.as_deref() != Some(&job.id))
        .and_then(|job| job.outcome().map(|value| (job.id.clone(), value)));
    let mut request = None;
    if let Some((id, outcome)) = next {
        panel.seen_package = Some(id);
        request = published_input(&outcome);
        if request.is_some() {
            panel.open = true;
        }
    }
    if request.is_none()
        && panel.job.is_none()
        && let Some(status) = world
            .get_resource::<crate::launch::Launcher>()
            .and_then(|l| l.status())
        && panel.seen_launch != Some(status.id)
        && let Some(path) = status.artifact
        && (path.is_dir() || path.extension().is_some_and(|e| e == "apk" || e == "aab"))
    {
        panel.seen_launch = Some(status.id);
        panel.open = true;
        request = Some(path);
    }
    world.insert_resource(panel);
    if let Some(path) = request
        && let Err(e) = start(world, path)
    {
        world.resource_mut::<SizeReportPanel>().error = Some(e.message);
    }
}

fn controls(ui: &mut egui::Ui, panel: &SizeReportPanel) -> egui::Response {
    let refresh = crate::agent_parity::action_enabled(
        ui,
        panel.path.is_some() && panel.job.is_none(),
        &crate::agent_parity::SIZE_REPORT_REFRESH,
        None,
    );
    if panel.job.is_some() {
        ui.spinner();
    }
    if let Some(error) = &panel.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if let Some(report) = &panel.report {
        ui.label(report.path.display().to_string());
        ui.label(format!(
            "{} bytes ({})",
            report.content_bytes,
            report.measurement.replace('_', " ")
        ));
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("size-categories").striped(true).show(ui, |ui| {
                for (category, bytes) in &report.categories {
                    ui.label(category.replace('_', " "));
                    ui.label(format!("{bytes} bytes"));
                    ui.end_row();
                }
            });
            ui.heading("Largest files");
            for entry in &report.largest {
                ui.label(format!("{} — {} bytes", entry.path, entry.bytes));
            }
            if !report.largest_assets.is_empty() {
                ui.heading("Largest asset payloads");
            }
            for entry in &report.largest_assets {
                ui.label(format!("{} — {} bytes", entry.path, entry.bytes));
            }
            for warning in &report.warnings {
                ui.weak(warning);
            }
        });
    } else {
        ui.label("Package the project to see its size report.");
    }
    ui.heading("Unused engine features");
    ui.weak("Unavailable: registered plugins do not yet report complete play-session execution. No stripping suggestions can be inferred.");
    refresh
}

/// Draws Size Report with keyboard-operable refresh and inherited appearance tokens/scale.
#[tracing::instrument(skip_all)]
pub fn show(ctx: &egui::Context, world: &mut World) {
    tick(world);
    let mut panel = world.remove_resource::<SizeReportPanel>().unwrap_or_default();
    let mut open = panel.open;
    let mut refresh = false;
    egui::Window::new("Size Report").open(&mut open).show(ctx, |ui| {
        refresh = controls(ui, &panel).clicked();
    });
    panel.open = open;
    let frame = ctx.cumulative_frame_nr();
    let request = if refresh && panel.handled_frame != Some(frame) {
        panel.handled_frame = Some(frame);
        panel.path.clone()
    } else {
        None
    };
    world.insert_resource(panel);
    if let Some(path) = request
        && let Err(e) = start(world, path)
    {
        world.resource_mut::<SizeReportPanel>().error = Some(e.message);
    }
}

fn package_size_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick(world);
    if let Some(open) = params.get("open") {
        world.resource_mut::<SizeReportPanel>().open = open
            .as_bool()
            .ok_or_else(|| err("package_size_bad_params", "open must be boolean"))?;
    }
    let op = match params.get("op") {
        None => "status",
        Some(Value::String(op)) => op,
        _ => return Err(err("package_size_bad_params", "op must be a string")),
    };
    match op {
        "status" => {}
        "refresh" => {
            let path = match params.get("path") {
                Some(Value::String(path)) => crate::agent_ext::allowed_request_path(world, "path", path)?,
                None => world
                    .resource::<SizeReportPanel>()
                    .path
                    .clone()
                    .ok_or_else(|| err("package_size_no_package", "No package has been reported"))?,
                _ => return Err(err("package_size_bad_params", "path must be a string")),
            };
            start(world, path)?;
        }
        _ => return Err(err("package_size_bad_params", "op is status or refresh")),
    }
    let panel = world.resource::<SizeReportPanel>();
    Ok(
        json!({"state":if panel.job.is_some(){"running"}else if panel.error.is_some(){"failed"}else if panel.report.is_some(){"done"}else{"idle"},
        "job":panel.job.as_ref().map(|j| &j.id),"report":panel.report,"error":panel.error,"open":panel.open,
        "unused_features":[],"feature_telemetry":"unavailable"}),
    )
}

/// Package size route and schema.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.package_size_report",
            tool: "package_size_report",
            description: "Read or refresh measured package contents by category and largest file. Desktop directories include pak asset kinds; archives reuse apk-size. No feature stripping is applied. Errors: package_size_no_project, package_size_busy, package_size_failed, package_size_bad_params, package_size_no_package.",
            params: &[
                ParamSchema {
                    name: "op",
                    kind: "string",
                    required: false,
                    description: "status (default) or refresh.",
                },
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "For refresh: a package directory, APK, AAB or ZIP. Defaults to the last report input.",
                },
                ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "Show or hide Size Report.",
                },
            ],
            result: "{ state, job, report, error, open, unused_features, feature_telemetry }",
            mutating: true,
            permission: Permission::Read,
        },
        package_size_report as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_report_panel_counts_desktop_categories_and_largest_files() {
        let root = regia_test_utils::unique_temp_dir("desktop-size");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        for (name, bytes) in [
            ("game", b"\x7fELF123456".as_slice()),
            ("assets/music.ogg", b"12345".as_slice()),
            ("assets/shader.wgsl", b"123".as_slice()),
        ] {
            std::fs::write(root.join(name), bytes).expect("file");
        }
        let report = directory_report(&root, || false).expect("report");
        assert_eq!(report.content_bytes, 18);
        assert_eq!(report.categories.values().sum::<u64>(), 18);
        assert_eq!(report.categories["engine_binary"], 10);
        assert_eq!(report.categories["audio"], 5);
        assert_eq!(report.categories["shaders"], 3);
        assert_eq!(report.largest[0].path, "game");
        assert!(directory_report(&root, || true).is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn size_report_panel_refresh_is_keyboard_operable() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        for scale in [1.0, 1.5] {
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(scale);
            let panel = SizeReportPanel {
                path: Some("bundle".into()),
                open: false,
                report: None,
                job: None,
                project: None,
                seen_package: None,
                seen_launch: None,
                error: None,
                handled_frame: None,
            };
            let mut activated = false;
            let mut draw = |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    activated |= controls(ui, &panel).clicked();
                });
            };
            frame(&ctx, [1000.0, 1200.0], vec![], &mut draw);
            tab_to(&ctx, [1000.0, 1200.0], "Refresh report", false, 60, &mut draw);
            frame(
                &ctx,
                [1000.0, 1200.0],
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                &mut draw,
            );
            assert!(
                activated,
                "Tab reaches and Enter activates Refresh report at scale {scale}"
            );
        }
    }
}
