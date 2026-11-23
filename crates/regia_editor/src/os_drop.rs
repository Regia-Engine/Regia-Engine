// SPDX-License-Identifier: MPL-2.0
//! Files dropped from the operating system onto the editor window (spec addendum § 8.7, ADR-7788).
//!
//! winit hands a drop to egui as `RawInput::dropped_files`; [`handle_os_drops`] (called by the shell
//! after the frame's UI, where the viewport drop runs too) decides where the files go:
//!
//! - **Viewport.** When the pointer is over a viewport pane this frame (the pane wrote
//!   [`crate::content::ViewportDropAim`]), the files are imported into the Content Browser's
//!   current folder and every placeable result (a mesh, a prefab, a sound) is placed at the drop
//!   aim, as ONE undo step.
//! - **Content Browser.** Over the browser, the files import into the folder row under the pointer,
//!   else the shown folder ([`BrowserDropZone`], written by the browser each frame).
//! - **Anywhere else** the files import into the shown folder (nothing is placed).
//!
//! Files are always copied into the project, never referenced in place: a source outside the
//! project goes through the normal import path (copy, cook, provenance). A file already under the
//! project's `assets/` is not copied a second time: it is used where it is (a viewport drop places
//! it). Files whose type no importer reads are collected into one toast line.
//!
//! winit 0.30 gives no position with a dropped file, so the target is decided by the pointer's
//! last known position. The import runs as the background `import:<n>` job (the Asset Jobs panel
//! lists it); a placement waits for that job and uses the aim captured at the drop.
//!
//! Agents call `editor.import_files { paths, folder?, place? }`, which runs the same import and
//! placement synchronously.

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use serde_json::{Value, json};

use crate::command::{CommandGroup, UndoStack};
use crate::content::{ViewportDropAim, slash_path};
use crate::dnd::{ContentKind, kind_of_path};
use crate::engine::ecs::prelude::*;
use crate::level_io::{self, OpenLevel};
use crate::viewport_agent::Handler;

/// Where the Content Browser is on screen this frame and which folder row the pointer is over.
///
/// The browser writes it while it draws; [`handle_os_drops`] reads it after the UI.
#[derive(Resource, Debug, Clone, Default)]
pub struct BrowserDropZone {
    /// The browser panel's rectangle (points), when it was drawn this frame.
    pub rect: Option<egui::Rect>,
    /// The folder row under the pointer (`assets/...`, or `""` for the `assets` root row).
    pub hovered_folder: Option<String>,
    /// The folder the browser shows (`""` = `assets/`).
    pub current_folder: String,
    /// The egui frame (`cumulative_frame_nr`) the zone was written in (a stale zone is ignored).
    pub pass: u64,
}

/// Records the browser's drop zone for this frame (called by the Content Browser as it draws).
pub fn note_browser(
    world: &mut World,
    rect: egui::Rect,
    hovered_folder: Option<String>,
    current: &str,
    frame: u64,
) {
    world.insert_resource(BrowserDropZone {
        rect: Some(rect),
        hovered_folder,
        current_folder: current.to_string(),
        pass: frame,
    });
}

/// Where a drop goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropTarget {
    /// Import into this folder (project-relative under `assets/`, `""` = `assets/`).
    Folder(String),
    /// Import into this folder, then place the results at the viewport's drop aim.
    Viewport(String),
}

impl DropTarget {
    /// The folder the files import into.
    #[must_use]
    pub fn folder(&self) -> &str {
        match self {
            Self::Folder(folder) | Self::Viewport(folder) => folder,
        }
    }
}

/// Decides where a drop goes: the viewport when a pane is aimed at this frame, the browser's
/// hovered folder when the pointer is over the browser, else the browser's shown folder.
#[must_use]
pub fn route_drop(pointer: Option<egui::Pos2>, viewport_aimed: bool, zone: &BrowserDropZone) -> DropTarget {
    let current = zone.current_folder.clone();
    if viewport_aimed {
        return DropTarget::Viewport(current);
    }
    let over_browser = matches!((pointer, zone.rect), (Some(p), Some(rect)) if rect.contains(p));
    if over_browser && let Some(folder) = &zone.hovered_folder {
        return DropTarget::Folder(folder.clone());
    }
    DropTarget::Folder(current)
}

/// Dropped files sorted out: the ones to import (outside the project's `assets/`), the ones already
/// in the project (`assets/...` paths), and the ones no importer reads (their file names).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SortedDrop {
    /// Sources to copy in through the import path.
    pub import: Vec<PathBuf>,
    /// Project assets the drop names directly (project-relative).
    pub in_project: Vec<String>,
    /// File names no importer reads (folders included).
    pub unsupported: Vec<String>,
}

/// Sorts dropped `paths` against the project at `root` and the importable extensions `types`
/// (lowercase, without the dot).
#[must_use]
pub fn sort_drop(root: &Path, paths: &[PathBuf], types: &[String]) -> SortedDrop {
    let mut sorted = SortedDrop::default();
    let assets = root.join("assets");
    let assets_canonical = assets.canonicalize().ok();
    for path in paths {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !path.is_file() || !types.contains(&ext) {
            sorted.unsupported.push(name);
            continue;
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let inside = assets_canonical
            .as_ref()
            .and_then(|assets| canonical.strip_prefix(assets).ok())
            .map(|rel| format!("assets/{}", slash_path(rel)));
        match inside {
            Some(rel) => sorted.in_project.push(rel),
            None => sorted.import.push(path.clone()),
        }
    }
    sorted
}

/// The one toast line for files no importer reads (`None` when every file was supported).
#[must_use]
pub fn unsupported_toast(unsupported: &[String]) -> Option<String> {
    (!unsupported.is_empty())
        .then(|| format!("Not imported (unsupported file type): {}", unsupported.join(", ")))
}

/// What one drop did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DropOutcome {
    /// Project-relative paths the import created.
    pub imported: Vec<String>,
    /// Project-relative paths placed in the viewport.
    pub placed: Vec<String>,
    /// File names no importer reads.
    pub unsupported: Vec<String>,
    /// Import or placement failures: (path, message).
    pub errors: Vec<(String, String)>,
    /// The folder the files went into (`""` = `assets/`).
    pub folder: String,
}

impl DropOutcome {
    /// One status-bar line.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.imported.is_empty() {
            parts.push(format!("imported {}", self.imported.len()));
        }
        if !self.placed.is_empty() {
            parts.push(format!("placed {}", self.placed.len()));
        }
        if !self.errors.is_empty() {
            parts.push(format!("{} failed (see the Output Log)", self.errors.len()));
        }
        let mut line = if parts.is_empty() {
            "nothing imported".to_string()
        } else {
            parts.join(", ")
        };
        if let Some(toast) = unsupported_toast(&self.unsupported) {
            line.push_str(" · ");
            line.push_str(&toast);
        }
        line
    }

    /// The agent reply.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "imported": self.imported,
            "placed": self.placed,
            "unsupported": self.unsupported,
            "errors": self
                .errors
                .iter()
                .map(|(path, message)| json!({ "path": path, "error": message }))
                .collect::<Vec<_>>(),
            "folder": self.folder,
        })
    }
}

/// Whether a viewport drop places this asset (a mesh, a prefab or a sound; a level would replace
/// the open level and a material needs a mesh under the cursor, so neither is placed by a file drop).
fn placeable(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    matches!(kind_of_path(rel), ContentKind::Mesh | ContentKind::Prefab)
        || crate::asset_viewer::sound_extensions()
            .iter()
            .any(|ext| lower.ends_with(ext))
}

/// Places `paths` at `aim` (or the origin without one) as ONE undo step. Returns the placed paths
/// and the failures.
pub fn place_all(
    world: &mut World,
    root: &Path,
    paths: &[String],
    aim: Option<ViewportDropAim>,
) -> (Vec<String>, Vec<(String, String)>) {
    let previous_aim = world.remove_resource::<ViewportDropAim>();
    if let Some(aim) = aim {
        world.insert_resource(aim);
    }
    let mut open = world
        .get_resource::<OpenLevel>()
        .cloned()
        .unwrap_or_else(|| level_io::new_level(world));
    let depth_before = world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .undo_depth();
    world.resource_mut::<UndoStack>().seal();
    let mut placed = Vec::new();
    let mut errors = Vec::new();
    for rel in paths.iter().filter(|rel| placeable(rel)) {
        let depth = world.resource::<UndoStack>().undo_depth();
        let toast = crate::content::apply_content_drop(world, &mut open, root, rel, kind_of_path(rel));
        if world.resource::<UndoStack>().undo_depth() > depth {
            placed.push(rel.clone());
        } else {
            errors.push((rel.clone(), toast));
        }
        world.resource_mut::<UndoStack>().seal();
    }
    // Several placements become one entry: one Ctrl+Z removes everything the drop placed.
    let added = world
        .resource::<UndoStack>()
        .undo_depth()
        .saturating_sub(depth_before);
    if added > 1 {
        let mut stack = world.resource_mut::<UndoStack>();
        let mut commands = Vec::new();
        for _ in 0..added {
            if let Some(command) = stack.take_top() {
                commands.push(command);
            }
        }
        commands.reverse();
        let mut group = CommandGroup::new(format!("Place {} dropped files", commands.len()));
        for command in commands {
            group.push(command);
        }
        stack.push(Box::new(group));
        stack.seal();
    }
    world.insert_resource(open);
    world.remove_resource::<ViewportDropAim>();
    if let Some(previous) = previous_aim {
        world.insert_resource(previous);
    }
    (placed, errors)
}

/// Imports `sorted` into `folder` synchronously and, for a viewport target, places the results at
/// `aim` (one undo step). Extension importers run first, as for Import….
///
/// # Errors
///
/// A message when the import cannot start (bad folder, no file to import).
pub fn import_and_place(
    world: &mut World,
    root: &Path,
    folder: &str,
    sorted: &SortedDrop,
    place: Option<Option<ViewportDropAim>>,
    client: Option<&str>,
) -> Result<DropOutcome, String> {
    let _span = tracing::info_span!(target: "regia_editor", "os_drop_import").entered();
    let mut outcome = DropOutcome {
        unsupported: sorted.unsupported.clone(),
        folder: folder.to_string(),
        ..DropOutcome::default()
    };
    if !sorted.import.is_empty() {
        let report =
            crate::extension::host::import_with_extensions(world, root, folder, &sorted.import, client)?;
        outcome.imported = report.imported;
        outcome.errors = report.errors;
    }
    crate::content_index::invalidate(world);
    if let Some(aim) = place {
        let mut targets = outcome.imported.clone();
        targets.extend(sorted.in_project.iter().cloned());
        let (placed, errors) = place_all(world, root, &targets, aim);
        outcome.placed = placed;
        outcome.errors.extend(errors);
    }
    Ok(outcome)
}

/// How the shell runs an OS drop's import (tests run it on the frame).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsDropMode {
    /// A background `import:<n>` job; placement waits for it (the default).
    Background,
    /// On the frame that received the drop.
    Inline,
}

/// A drop whose import runs in the background; its placement (if any) waits for it.
#[derive(Debug, Clone)]
pub struct PendingDrop {
    /// The import job.
    pub job: crate::editor_jobs::ImportJob,
    /// The aim captured at the drop, when the drop was on the viewport.
    pub place: Option<Option<ViewportDropAim>>,
    /// Files already in the project the drop named (placed with the imports).
    pub in_project: Vec<String>,
    /// File names no importer reads.
    pub unsupported: Vec<String>,
    /// The folder the files go into.
    pub folder: String,
}

/// Background drops still importing.
#[derive(Resource, Debug, Clone, Default)]
pub struct PendingDrops(pub Vec<PendingDrop>);

/// The shell's hook: takes this frame's OS file drops from egui and imports (and places) them.
/// Also finishes background drops whose import ended. Returns a toast line for the status bar.
///
/// Called once per frame after the UI; `playing` refuses placement into a play session.
pub fn handle_os_drops(
    ctx: &egui::Context,
    world: &mut World,
    root: Option<&Path>,
    playing: bool,
) -> Option<String> {
    let mut toast = finish_pending(world, root, playing);
    let dropped: Vec<PathBuf> = ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .map(|file| file.path().to_path_buf())
            .filter(|path| !path.as_os_str().is_empty())
            .collect()
    });
    if dropped.is_empty() {
        return toast;
    }
    let Some(root) = root else {
        return Some("open a project to import files".to_string());
    };
    let pointer = ctx.input(|i| i.pointer.latest_pos());
    let aim = world.get_resource::<ViewportDropAim>().copied();
    let mut zone = world
        .get_resource::<BrowserDropZone>()
        .cloned()
        .unwrap_or_default();
    if zone.pass + 1 < ctx.cumulative_frame_nr() {
        // The browser was not drawn this frame: only its folder still counts.
        zone.rect = None;
        zone.hovered_folder = None;
    }
    let target = route_drop(pointer, aim.is_some() && !playing, &zone);
    let types = crate::extension::host::import_file_types(world);
    let sorted = sort_drop(root, &dropped, &types);
    let place = matches!(target, DropTarget::Viewport(_)).then_some(aim);
    let folder = target.folder().to_string();
    let mode = world
        .get_resource::<OsDropMode>()
        .copied()
        .unwrap_or(OsDropMode::Background);
    tracing::info!(target: "regia_editor", files = dropped.len(), ?target, "os file drop");
    if mode == OsDropMode::Inline || sorted.import.is_empty() {
        let line = match import_and_place(world, root, &folder, &sorted, place, None) {
            Ok(outcome) => {
                for (path, message) in &outcome.errors {
                    tracing::error!(target: "regia_editor", "drop {path}: {message}");
                }
                outcome.summary()
            }
            Err(err) => err,
        };
        toast = Some(line);
        return toast;
    }
    // Extension importers run here (they need the world); the pipeline import is the job.
    let (rest, from_extensions) = crate::extension::host::run_importers(world, root, &folder, &sorted.import);
    let mut in_project = sorted.in_project.clone();
    in_project.extend(from_extensions.imported.iter().cloned());
    if rest.is_empty() {
        let follow_up = SortedDrop {
            import: Vec::new(),
            in_project,
            unsupported: sorted.unsupported.clone(),
        };
        return import_and_place(world, root, &folder, &follow_up, place, None)
            .map_or_else(Some, |outcome| Some(outcome.summary()));
    }
    match crate::editor_jobs::spawn_import(world, root.to_path_buf(), folder.clone(), rest, None) {
        Ok(job) => {
            world
                .get_resource_or_insert_with::<PendingDrops>(PendingDrops::default)
                .0
                .push(PendingDrop {
                    job,
                    place,
                    in_project,
                    unsupported: sorted.unsupported.clone(),
                    folder,
                });
            toast = Some(match unsupported_toast(&sorted.unsupported) {
                Some(line) => format!("importing dropped files… · {line}"),
                None => "importing dropped files…".to_string(),
            });
        }
        Err(err) => toast = Some(err),
    }
    toast
}

/// Places the results of background drops whose import ended; returns their toast. While play
/// runs (`playing`) nothing is placed: the panels show the play world, and a placement would go
/// into the edit world unseen (review sprint-w12-rev-g5 #12). Errors go to the Output Log, as on
/// the inline path.
fn finish_pending(world: &mut World, root: Option<&Path>, playing: bool) -> Option<String> {
    let pending = world.get_resource::<PendingDrops>()?;
    if pending.0.iter().all(|drop| drop.job.running()) {
        return None;
    }
    let mut pending = world.remove_resource::<PendingDrops>()?;
    let (done, running): (Vec<_>, Vec<_>) = pending.0.drain(..).partition(|drop| !drop.job.running());
    if !running.is_empty() {
        world.insert_resource(PendingDrops(running));
    }
    let mut line = None;
    for drop in done {
        let mut outcome = DropOutcome {
            unsupported: drop.unsupported,
            folder: drop.folder,
            ..DropOutcome::default()
        };
        match drop.job.take_outcome() {
            Some(Ok(report)) => {
                outcome.imported = report.imported;
                outcome.errors = report.errors;
            }
            Some(Err(err)) => outcome.errors.push(("import".to_string(), err)),
            None => {}
        }
        crate::content_lint_jobs::note_imported(world, &outcome.imported);
        crate::content_index::invalidate(world);
        if let (Some(aim), Some(root)) = (drop.place, root) {
            // The import job logged its own errors; the placement's are logged here.
            let errors = if playing {
                vec![(
                    "place".to_string(),
                    "play is running: the files are imported but not placed".to_string(),
                )]
            } else {
                let mut targets = outcome.imported.clone();
                targets.extend(drop.in_project);
                let (placed, errors) = place_all(world, root, &targets, aim);
                outcome.placed = placed;
                errors
            };
            for (path, message) in &errors {
                tracing::error!(target: "regia_editor", "drop {path}: {message}");
            }
            outcome.errors.extend(errors);
        }
        line = Some(outcome.summary());
    }
    line
}

/// `editor.import_files { paths, folder?, place? }`.
fn import_files_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = crate::agent_ext::project_root(world)?;
    let paths: Vec<PathBuf> = params
        .get("paths")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .and_then(|list| {
            list.iter()
                .map(|p| p.as_str().map(PathBuf::from))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| {
            drop_error(
                RpcError::invalid_params("`paths` must be a non-empty array of file paths"),
                "drop_bad_params",
            )
        })?;
    let folder = match params.get("folder") {
        None | Some(Value::Null) => world
            .get_resource::<BrowserDropZone>()
            .map(|zone| zone.current_folder.clone())
            .unwrap_or_default(),
        Some(Value::String(folder)) => folder.trim_start_matches('/').to_string(),
        Some(_) => {
            return Err(drop_error(
                RpcError::invalid_params("`folder` must be a string"),
                "drop_bad_params",
            ));
        }
    };
    if !folder.is_empty() {
        crate::agent_ext::assets_request_path(&root, "folder", &folder)
            .map_err(|e| drop_error(e, "drop_bad_folder"))?;
    }
    let place = params.get("place").and_then(Value::as_bool).unwrap_or(false);
    let types = crate::extension::host::import_file_types(world);
    let sorted = sort_drop(&root, &paths, &types);
    let aim = world.get_resource::<ViewportDropAim>().copied();
    let client = params.get("client").and_then(Value::as_str);
    let outcome = import_and_place(world, &root, &folder, &sorted, place.then_some(aim), client)
        .map_err(|message| drop_error(RpcError::invalid_params(message), "drop_import_failed"))?;
    Ok(outcome.to_json())
}

/// `error` with `data.drop_error = code`.
fn drop_error(mut error: RpcError, code: &str) -> RpcError {
    if let Some(data) = error.data.as_mut() {
        data["drop_error"] = json!(code);
    }
    error
}

/// The OS-drop agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.import_files",
            tool: "import_files",
            description: "Import files as an OS drop does (spec addendum § 8.7): sources outside the project are \
                copied into `folder` (default: the Content Browser's shown folder) and cooked; files already under \
                assets/ are used where they are. With `place: true` every mesh, prefab and sound is placed at the \
                viewport's drop aim (the origin without one) as one undo step. Unsupported files are listed.",
            params: &[
                ParamSchema {
                    name: "paths",
                    kind: "array",
                    required: true,
                    description: "Absolute paths of the dropped files.",
                },
                ParamSchema {
                    name: "folder",
                    kind: "string",
                    required: false,
                    description: "Destination folder under assets/ (default: the browser's shown folder).",
                },
                ParamSchema {
                    name: "place",
                    kind: "boolean",
                    required: false,
                    description: "Place the results in the viewport as a viewport drop does (default false).",
                },
            ],
            result: "{ imported: [paths], placed: [paths], unsupported: [names], errors: [{ path, error }], folder }",
            mutating: true,
            permission: Permission::Assets,
        },
        import_files_method as Handler,
    )]
}

#[cfg(test)]
pub(crate) mod tests;
