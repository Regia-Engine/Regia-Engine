// SPDX-License-Identifier: MPL-2.0
//! The motion-database editor (`docs/specs/52-motion-matching.md` § 6, part A; ADR-7740): a
//! document editor for `.motiondb.ron` sources. It opens a file, edits it through [`ops::MmDbOp`]
//! (the schema form, the clip list, the tag chips), tracks dirty state, saves the canonical text
//! (`MotionDatabaseSource::to_ron_string`, the runtime's own writer) and rebuilds the database in
//! memory as a cancellable editor job (ADR-7664's [`WorkerJob`]) to list the runtime builder's
//! warnings and the stats (poses, clip coverage, feature ranges). The cooked file is never written.
//!
//! State lives in the [`MmDbEditor`] world resource, so the window ([`panel`]) and the agent
//! methods (`editor.mm_db_*`, [`agent`]) edit the same document. Every edit is an [`MmDbEdit`]
//! snapshot command on the editor's shared [`UndoStack`], so `editor.undo` and Ctrl+Z revert it in
//! order with level edits. A file with RON comments opens with a warning: a save writes the
//! canonical text, which has none.
//!
//! The Motion Matching debug panel (`crate::mm_panel`) opens the selected character's database
//! here ("Edit database"); Content ▸ New ▸ Motion database creates a file
//! ([`crate::content::NewAssetKind::MotionDatabase`]).
//!
//! Part B (w11-motiondb-editor-b): the span timeline per entry ([`timeline`], span ops
//! `add_span`/`set_span`/`remove_span`), the pose preview with the scrubbed frame's feature vector
//! ([`preview`], `editor.mm_db_scrub`), live rebuilds after edits ([`live`]) and document autosave
//! (kind `motion_db`).

pub mod agent;
pub(crate) mod autosave;
pub mod live;
pub mod model;
pub mod ops;
pub mod panel;
pub mod preview;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_b;
pub mod timeline;

use std::path::{Path, PathBuf};

use regia_animation::motion_matching::{MotionDatabaseSource, SchemaSource};
use regia_asset_pipeline::motiondb_cook::{MemoryInputs, MotionDbInputs, ProjectInputs};
use regia_ecs::prelude::*;

use crate::command::{EditorCommand, UndoStack};
use crate::editor_jobs::WorkerJob;
use model::{RebuildError, RebuildReport};
use ops::{BatchError, MmDbOp};

/// The source file extension.
pub const MOTIONDB_EXTENSION: &str = "motiondb.ron";

/// What the editor tells the user (and the agent state carries) when the open file has comments.
pub const COMMENTS_WARNING: &str = "This file has comments: saving writes the canonical text and drops them.";

/// A refused editor request: a stable code and a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MmDbError {
    /// `no_database_open`, `bad_path`, `not_found`, `bad_source`, `unsaved_changes`,
    /// `save_failed`, `no_project`, `rebuild_running`, `no_rebuild`, `job_start_failed`, or an
    /// op code (`bad_value`, `bad_index`, `unknown_group`, `unknown_tag`, `duplicate`, `too_many`).
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

impl MmDbError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The last rebuild's outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum RebuildView {
    /// Built: the report, and the document revision it was built from.
    Done {
        /// The report.
        report: RebuildReport,
        /// [`MmDbEditor::revision`] at the start.
        revision: u64,
    },
    /// Did not build.
    Failed(RebuildError),
    /// Cancelled before it finished.
    Cancelled,
}

/// The motion-database editor's world resource.
#[derive(Resource, Default)]
pub struct MmDbEditor {
    /// The open file (absolute).
    pub path: Option<PathBuf>,
    /// The open document.
    pub doc: Option<MotionDatabaseSource>,
    /// The document as opened or last saved.
    saved: Option<MotionDatabaseSource>,
    /// Which document is open: undo entries of another document do nothing.
    session: u64,
    /// Bumped by every document change (edit, undo, redo, open).
    pub revision: u64,
    /// The open file had RON comments ([`COMMENTS_WARNING`]).
    pub comments_warning: bool,
    /// Last action's status line.
    pub toast: String,
    /// The last finished rebuild.
    pub rebuild: Option<RebuildView>,
    /// Window state.
    pub ui: panel::PanelState,
}

impl MmDbEditor {
    /// Whether a document is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.doc.is_some()
    }

    /// Whether the document differs from the file.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.doc.is_some() && self.doc != self.saved
    }

    /// The open session id.
    #[must_use]
    pub fn session(&self) -> u64 {
        self.session
    }

    /// Opens a recovered document (an autosave): at `path`, never saved in this session, so dirty.
    fn recover(&mut self, path: Option<PathBuf>, doc: MotionDatabaseSource) {
        self.session += 1;
        self.revision += 1;
        self.path = path;
        self.saved = None;
        self.doc = Some(doc);
        self.comments_warning = false;
        self.rebuild = None;
        self.ui.reset();
        self.ui.window_open = true;
    }

    fn load(&mut self, path: PathBuf, doc: MotionDatabaseSource, comments: bool) {
        self.session += 1;
        self.revision += 1;
        self.path = Some(path);
        self.saved = Some(doc.clone());
        self.doc = Some(doc);
        self.comments_warning = comments;
        self.rebuild = None;
        self.ui.reset();
    }
}

/// Memory inputs for the rebuild (tests, tools and samples that make their clips in code). With
/// none, the rebuild reads the project's cooked cache (`ProjectInputs`).
#[derive(Resource, Clone, Debug, Default)]
pub struct MmDbMemoryInputs(pub MemoryInputs);

/// A new database document: the schema defaults, no tags, no entries (what Content ▸ New writes).
#[must_use]
pub fn new_document(skeleton: &str) -> MotionDatabaseSource {
    MotionDatabaseSource {
        format_version: regia_animation::motion_matching::MOTIONDB_FORMAT_VERSION,
        skeleton: skeleton.to_string(),
        schema: SchemaSource {
            root_joint: "root".to_string(),
            forward: [0.0, 0.0, 1.0],
            feet: Vec::new(),
            trajectory_times: regia_animation::motion_matching::DEFAULT_TRAJECTORY_TIMES.to_vec(),
            history_times: Vec::new(),
            sample_rate: regia_animation::motion_matching::DEFAULT_SAMPLE_RATE,
            weights: Default::default(),
            normalization: Default::default(),
            pose_joints: Vec::new(),
            sample_weights: Default::default(),
            contacts: None,
        },
        tags: Vec::new(),
        mirror_pairs: Vec::new(),
        entries: Vec::new(),
    }
}

/// The canonical text of a document (the runtime's writer).
///
/// # Errors
///
/// `bad_source` when serialization fails.
pub fn canonical_text(doc: &MotionDatabaseSource) -> Result<String, MmDbError> {
    doc.to_ron_string()
        .map_err(|e| MmDbError::new("bad_source", e.to_string()))
}

fn check_extension(path: &Path) -> Result<(), MmDbError> {
    if path
        .to_string_lossy()
        .ends_with(&format!(".{MOTIONDB_EXTENSION}"))
    {
        Ok(())
    } else {
        Err(MmDbError::new(
            "bad_path",
            format!("{} is not a .{MOTIONDB_EXTENSION} file", path.display()),
        ))
    }
}

/// Opens `path` (absolute, or relative to the working directory). Refused with
/// `unsaved_changes` when the open document is dirty and `discard` is false.
///
/// # Errors
///
/// `bad_path`, `not_found`, `bad_source`, `unsaved_changes`.
pub fn open(world: &mut World, path: &Path, discard: bool) -> Result<(), MmDbError> {
    check_extension(path)?;
    let editor = world.get_resource_or_insert_with(MmDbEditor::default);
    if editor.is_dirty() && !discard {
        return Err(MmDbError::new(
            "unsaved_changes",
            "the open database has unsaved changes: save it, or open with discard",
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| MmDbError::new("not_found", format!("cannot read {}: {e}", path.display())))?;
    let doc =
        MotionDatabaseSource::from_ron_str(&text).map_err(|e| MmDbError::new("bad_source", e.to_string()))?;
    let comments = crate::abilities_panel::form::has_comments(&text);
    let mut editor = world.get_resource_or_insert_with(MmDbEditor::default);
    editor.load(path.to_path_buf(), doc, comments);
    editor.toast = format!("opened {}", path.display());
    Ok(())
}

/// Saves the document as canonical text to `path` (or where it was opened), atomically. A
/// document with validator errors is saved too (the source loader reads it; the issue list says
/// what the build will refuse).
///
/// # Errors
///
/// `no_database_open`, `bad_path`, `bad_source`, `save_failed`.
pub fn save(world: &mut World, path: Option<&Path>) -> Result<PathBuf, MmDbError> {
    let mut editor = world
        .get_resource_mut::<MmDbEditor>()
        .filter(|e| e.is_open())
        .ok_or_else(|| MmDbError::new("no_database_open", "no motion database is open"))?;
    let path = path
        .map(Path::to_path_buf)
        .or_else(|| editor.path.clone())
        .ok_or_else(|| MmDbError::new("bad_path", "no path: pass one to save as"))?;
    check_extension(&path)?;
    let doc = editor.doc.clone().unwrap_or_else(|| new_document(""));
    let text = canonical_text(&doc)?;
    write_atomic(&path, &text).map_err(|e| MmDbError::new("save_failed", e))?;
    editor.path = Some(path.clone());
    editor.saved = Some(doc);
    editor.comments_warning = false;
    editor.toast = format!("saved {}", path.display());
    Ok(path)
}

/// Writes `text` to `path` through a temp file and a rename (a reader never sees half a file).
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("ron.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename to {}: {e}", path.display()))
}

/// A snapshot edit of the document on the shared undo stack. It belongs to the document it was
/// made in: once another file is open it does nothing.
pub struct MmDbEdit {
    label: String,
    merge_key: Option<String>,
    session: u64,
    before: MotionDatabaseSource,
    after: MotionDatabaseSource,
}

impl MmDbEdit {
    fn set(world: &mut World, session: u64, doc: &MotionDatabaseSource) {
        let mut editor = world.get_resource_or_insert_with(MmDbEditor::default);
        if editor.session != session {
            editor.toast = "that undo step belongs to a motion database that is no longer open".into();
            return;
        }
        editor.doc = Some(doc.clone());
        editor.revision += 1;
    }
}

impl EditorCommand for MmDbEdit {
    fn apply(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.after);
    }

    fn revert(&mut self, world: &mut World) {
        Self::set(world, self.session, &self.before);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<MmDbEdit>() else {
            return false;
        };
        if self.merge_key.is_some() && self.merge_key == next.merge_key && self.session == next.session {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        format!("Motion database: {}", self.label)
    }
}

/// Applies `ops` as one undo step (merged into the previous step when both carry the same
/// `merge_key`, e.g. dragging one weight). All or nothing; `Ok(false)` when nothing changed.
///
/// # Errors
///
/// `no_database_open`, or the first op that failed (its code; the message names its position).
pub fn edit(
    world: &mut World,
    label: &str,
    merge_key: Option<String>,
    ops: &[MmDbOp],
) -> Result<bool, MmDbError> {
    let (before, session) = {
        let editor = world.get_resource_or_insert_with(MmDbEditor::default);
        let doc = editor
            .doc
            .clone()
            .ok_or_else(|| MmDbError::new("no_database_open", "no motion database is open"))?;
        (doc, editor.session)
    };
    let after = ops::apply_ops(&before, ops).map_err(|BatchError { at, error }| {
        MmDbError::new(error.code, format!("op {at}: {}", error.message))
    })?;
    if after == before {
        return Ok(false);
    }
    let mut command = MmDbEdit {
        label: label.to_string(),
        merge_key,
        session,
        before,
        after,
    };
    command.apply(world);
    let mut stack = world.get_resource_or_insert_with(UndoStack::new);
    stack.push(Box::new(command));
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// The rebuild job (ADR-7664)
// ---------------------------------------------------------------------------------------------

type RebuildOutcome = Result<Option<RebuildReport>, RebuildError>;

/// The running rebuild.
#[derive(Resource, Debug)]
pub struct MmDbRebuildJob {
    /// The worker job (`mm_db_rebuild:<n>`).
    pub job: WorkerJob<RebuildOutcome>,
    session: u64,
    revision: u64,
}

/// The background-task row of the rebuild.
pub const REBUILD_TASK: &str = "mm_db_rebuild";

pub(crate) fn rebuild_inputs(world: &World) -> Result<Box<dyn MotionDbInputs + Send>, MmDbError> {
    if let Some(memory) = world.get_resource::<MmDbMemoryInputs>() {
        return Ok(Box::new(memory.0.clone()));
    }
    let root = crate::matgraph_editor::project_root(world).ok_or_else(|| {
        MmDbError::new(
            "no_project",
            "no project is open: the rebuild reads the skeleton and clips from its cooked cache",
        )
    })?;
    let cache = regia_asset::cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
    Ok(Box::new(ProjectInputs::new(&root, &cache)))
}

/// Starts rebuilding the open document in memory as job `mm_db_rebuild:<n>`; returns its id.
///
/// # Errors
///
/// `no_database_open`, `rebuild_running`, `no_project`, `job_start_failed`.
pub fn start_rebuild(world: &mut World) -> Result<String, MmDbError> {
    if world.contains_resource::<MmDbRebuildJob>() {
        return Err(MmDbError::new("rebuild_running", "a rebuild is already running"));
    }
    let (doc, session, revision) = {
        let editor = world.get_resource_or_insert_with(MmDbEditor::default);
        let doc = editor
            .doc
            .clone()
            .ok_or_else(|| MmDbError::new("no_database_open", "no motion database is open"))?;
        (doc, editor.session, editor.revision)
    };
    let mut inputs = rebuild_inputs(world)?;
    let label = format!("Rebuilding motion database ({} clips)", doc.entries.len());
    let job = WorkerJob::spawn(
        world,
        "mm_db_rebuild",
        REBUILD_TASK,
        label,
        "mm-db-rebuild",
        move |cx| {
            let cancelled = || cx.cancelled();
            let progress = |f: f64, m: &str| cx.progress(Some(f), m);
            model::rebuild(&doc, inputs.as_mut(), &cancelled, &progress)
        },
    )
    .map_err(|e| MmDbError::new("job_start_failed", e))?;
    let id = job.id.clone();
    world.insert_resource(MmDbRebuildJob {
        job,
        session,
        revision,
    });
    Ok(id)
}

fn finish(
    world: &mut World,
    job: &MmDbRebuildJob,
    outcome: Option<Result<RebuildOutcome, String>>,
) -> RebuildView {
    let view = match outcome {
        None | Some(Ok(Ok(None))) => RebuildView::Cancelled,
        Some(Ok(Ok(Some(report)))) => RebuildView::Done {
            report,
            revision: job.revision,
        },
        Some(Ok(Err(e))) => RebuildView::Failed(e),
        Some(Err(panic)) => RebuildView::Failed(RebuildError {
            code: "build_failed",
            message: panic,
        }),
    };
    let result = match &view {
        RebuildView::Done { report, .. } => Ok((report.json(), false)),
        RebuildView::Cancelled => Ok((serde_json::Value::Null, true)),
        RebuildView::Failed(e) => Err((e.code, e.message.clone())),
    };
    job.job.end(world, result);
    let mut editor = world.get_resource_or_insert_with(MmDbEditor::default);
    if editor.session == job.session {
        editor.toast = match &view {
            RebuildView::Done { report, .. } => format!(
                "rebuilt: {} poses, {} warnings",
                report.frames,
                report.warnings.len()
            ),
            RebuildView::Failed(e) => format!("rebuild failed: {} ({})", e.message, e.code),
            RebuildView::Cancelled => "rebuild cancelled".to_string(),
        };
        editor.rebuild = Some(view.clone());
    }
    view
}

/// Polls the running rebuild and applies its end (every frame, from the window and the agent).
pub fn tick_job(world: &mut World) {
    let Some(mut job) = world.remove_resource::<MmDbRebuildJob>() else {
        return;
    };
    if job.job.context().cancelled() {
        finish(world, &job, None);
        return;
    }
    match job.job.take() {
        Some(outcome) => {
            finish(world, &job, Some(outcome));
        }
        None => {
            let message = job.job.context().message();
            job.job.sync_row(world, &message, job.job.context().fraction());
            world.insert_resource(job);
        }
    }
}

/// Waits for the running rebuild and applies its end; `None` without one.
pub fn finish_now(world: &mut World) -> Option<RebuildView> {
    let mut job = world.remove_resource::<MmDbRebuildJob>()?;
    let outcome = job.job.join();
    let outcome = if job.job.context().cancelled() {
        None
    } else {
        outcome
    };
    Some(finish(world, &job, outcome))
}

/// Cancels the running rebuild (its job ends `cancelled` at once). `true` when one ran.
pub fn cancel_rebuild(world: &mut World) -> bool {
    let Some(job) = world.get_resource::<MmDbRebuildJob>() else {
        return false;
    };
    job.job.cancel();
    tick_job(world);
    true
}

/// The project-relative or absolute file of a database asset path (what a character's
/// `MotionDatabases` entry names): under the project root, or under its `assets/`.
#[must_use]
pub fn asset_file(world: &World, asset: &str) -> PathBuf {
    let p = PathBuf::from(asset);
    if p.is_absolute() {
        return p;
    }
    match crate::matgraph_editor::project_root(world) {
        Some(root) => {
            let direct = root.join(&p);
            if direct.is_file() {
                direct
            } else {
                root.join("assets").join(p)
            }
        }
        None => p,
    }
}
