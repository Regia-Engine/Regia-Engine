// SPDX-License-Identifier: MPL-2.0
//! Tools ▸ Bake reflection captures: a cancellable editor job that bakes every placed
//! [`LocalCapture`] of the edit world and writes the level's `.fcap` (`docs/specs/19-editor.md`
//! § 2 Tools; ADR-2860, ADR-7592; `book/agents/reflection-captures.md`).
//!
//! The job does what `regia bake-captures` makes a running game do, in the editor's own process:
//! every capture entity is placed on the world's [`RuntimeRenderer`] at its world position (the
//! placement `regia_render::sync_local_captures` makes in a game), then one capture is baked per
//! editor frame ([`tick`]) with the renderer that draws the viewport
//! (`FrameRenderer::bake_local_capture` over the edit world's extracted frame), so the UI stays
//! live and a cancel lands between two captures. When the last capture is baked the renderer's
//! baked captures are written to `assets/<file>` in the same layout, order and bytes as
//! `render.save_local_captures` (the CLI's last step), through
//! `regia_asset::local_capture_bake::FcapFile::write` (a temporary file renamed over the target),
//! and the file is named in the open level's `look.captures` (save the level to keep it: a new
//! naming marks the level unsaved). A cancel, or a failed bake, writes nothing: the previous file
//! stays as it was. A run that ends, however it ends, takes its placements off the renderer again
//! (the editor runs no `sync_local_captures`, so a kept one would go stale when a capture moves).
//!
//! A run belongs to the level and the project it started on: when the open level changes (File ▸
//! Open, New, Revert, a project switch, any new edit session) the run ends `cancelled` with
//! `bake_level_changed` before it bakes another capture, and writes nothing.
//!
//! It also belongs to the captures as they were when it started: a capture moved, resized,
//! deleted or added while it runs (by hand, an agent, undo or a duplicate) ends it `cancelled`
//! with `bake_capture_changed` at the next step, naming the capture (`changed_capture` in the
//! status), and writes nothing, so a file never holds a map baked where a capture no longer is
//! (ADR-7663). Bake again once the edit is done.
//!
//! The editor viewport draws the level's baked captures as a game does (ADR-7663): the level's
//! bakes are published in the edit world's [`LocalCaptureBakes`] (read from the `.fcap` its
//! `look.captures` names when the level opens or the file changes, and replaced by a finished
//! run's maps), and every frame [`tick`] places each capture whose bake was made at its position
//! on the renderer with that bake and runs the renderer's capture selection from the editor
//! camera ([`ViewportCaptures`]). A capture with no matching bake (new, moved since the bake) is
//! not drawn: the editor renders no capture faces outside a run. While a run is in progress it
//! owns the renderer's placements; the next frame after it ends places the baked set again.
//!
//! The run is the background task [`BAKE_TASK`] (`editor.cancel_task { id: "bake_captures" }`
//! and the progress window's Cancel stop it). Agent methods: `editor.bake_captures` starts it
//! (the menu item's path), `editor.bake_captures_status` reports it.
//!
//! Error codes (`data.reason`): `bake_busy`, `bake_no_project`, `bake_no_renderer`, `bad_file`,
//! `path_outside_project`, `captures_none`, `capture_duplicate_id`, `capture_bad_value`,
//! `captures_full`; a run ends `failed` with `bake_failed` (a capture did not bake, for example
//! no GPU device yet) or `fcap_io` (the file could not be written), or `cancelled` with
//! `bake_cancelled` (the user's cancel), `bake_level_changed` (the open level or project
//! changed) or `bake_capture_changed` (a capture moved, resized, was deleted or was added).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::local_capture_bake::{FcapCapture, FcapFile};
use regia_ecs::prelude::*;
use regia_render::{LocalCapture, LocalCaptureBakes, LocalCaptureDesc, LocalCaptureInfo, RuntimeRenderer};
use regia_scene::prelude::{LevelId, LevelManager, Transform};
use serde_json::{Value, json};

use crate::tasks::{BackgroundTasks, CancelToken, TaskRow};
use crate::viewport_agent::Handler;

/// The background-task id of the bake (`editor.cancel_task { id: "bake_captures" }`).
pub const BAKE_TASK: &str = "bake_captures";
/// The file a level that names no bake gets (the `regia bake-captures --file` default).
pub const DEFAULT_FILE: &str = "maps/captures.fcap";
/// Where the book documents the job and its codes.
const HINT: &str = "see book/agents/reflection-captures.md";

/// Bakes one placed capture of the world's [`RuntimeRenderer`] by id. The editor's default is
/// [`gpu_bake`]; a test installs a deterministic one through [`CaptureBaker`].
pub type CaptureBakeFn = fn(&mut World, u64) -> Result<(), String>;

/// The bake function [`tick`] uses (default [`gpu_bake`]).
#[derive(Resource, Clone, Copy)]
pub struct CaptureBaker(pub CaptureBakeFn);

impl Default for CaptureBaker {
    fn default() -> Self {
        Self(gpu_bake)
    }
}

/// Where a bake run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BakeState {
    /// Captures are still being baked.
    Running,
    /// Every capture baked and the file was written.
    Done,
    /// A capture did not bake or the file could not be written; nothing was written.
    Failed,
    /// Cancelled before the file was written; nothing was written.
    Cancelled,
}

impl BakeState {
    /// The protocol name (`running`, `done`, `failed`, `cancelled`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The current (or last) bake run, an edit-world resource.
#[derive(Resource, Debug, Clone)]
pub struct CaptureBakeJob {
    /// The asset path written (`maps/hall.fcap`), under the project's `assets/`.
    pub file: String,
    /// The file's absolute path.
    pub path: PathBuf,
    /// The capture ids, in bake order (the level's entity order).
    pub ids: Vec<u64>,
    /// How many of [`Self::ids`] are baked.
    pub baked: usize,
    /// Where the run is.
    pub state: BakeState,
    /// The `data.reason`-style code of a failed or cancelled run.
    pub reason: Option<&'static str>,
    /// What to show the user.
    pub message: String,
    /// Bytes written (0 until done).
    pub bytes: u64,
    /// The run's cancel flag (also the background task row's).
    pub cancel: CancelToken,
    /// The open level the run started on (`LevelManager::current`); another one ends the run
    /// `bake_level_changed`.
    pub level: Option<LevelId>,
    /// The project root the run started in; another one ends the run `bake_level_changed`.
    pub project: PathBuf,
    /// Every capture's placement as the run started (id, world center, box, weight, fade), in
    /// [`Self::ids`] order. A capture of the level that no longer matches it (moved, resized,
    /// deleted, or one added) ends the run `cancelled` with `bake_capture_changed` (ADR-7663).
    pub placed: Vec<regia_render::LocalCaptureDesc>,
    /// The capture whose change cancelled the run (`bake_capture_changed`).
    pub changed_capture: Option<u64>,
}

impl CaptureBakeJob {
    /// Whether the run is still baking.
    #[must_use]
    pub fn running(&self) -> bool {
        self.state == BakeState::Running
    }

    /// The `editor.bake_captures_status` reply.
    #[must_use]
    pub fn status_json(&self) -> Value {
        json!({
            "task": BAKE_TASK,
            "state": self.state.name(),
            "file": self.file,
            "ids": self.ids,
            "baked": self.baked,
            "total": self.ids.len(),
            "reason": self.reason,
            "message": self.message,
            "bytes": self.bytes,
            "changed_capture": self.changed_capture,
        })
    }
}

/// Why a bake could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakeError {
    /// The lowercase protocol reason (`data.reason`).
    pub reason: &'static str,
    /// What to show the user.
    pub message: String,
}

impl BakeError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }

    /// The protocol error: `invalid_params` with the reason in `data.reason`.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        RpcError {
            code: INVALID_PARAMS,
            message: self.message.clone(),
            data: Some(json!({ "code": "F0002", "reason": self.reason, "hint": HINT })),
        }
    }
}

/// The file a bake writes when none is given: the one the open level names in `look.captures`,
/// else [`DEFAULT_FILE`].
#[must_use]
pub fn level_file(world: &World) -> String {
    world
        .get_resource::<regia_scene::level::LevelSettings>()
        .and_then(|settings| settings.look.captures.as_ref())
        .map_or_else(|| DEFAULT_FILE.to_string(), |c| c.file.clone())
}

/// Every capture entity's placement, in entity order, with its world position.
fn level_captures(world: &mut World) -> Vec<(u64, regia_render::LocalCaptureDesc)> {
    let mut query = world.query::<(Entity, &LocalCapture)>();
    let found: Vec<(Entity, LocalCapture)> = query.iter(world).map(|(e, c)| (e, *c)).collect();
    found
        .into_iter()
        .map(|(entity, capture)| {
            let center = composed_position(world, entity).to_array();
            (capture.id, capture.desc(center))
        })
        .collect()
}

/// `entity`'s world position composed from the local `Transform`s of its parent chain, as it is
/// now: an edit (an agent's `editor.set_property` on `Transform`) made earlier in the same frame
/// counts, before transform propagation writes the `GlobalTransform` (rev-claude-64 (1)
/// finding 3). An entity of the chain without a `Transform` counts as the identity.
fn composed_position(world: &World, entity: Entity) -> regia_core::math::Vec3 {
    if world.get::<Transform>(entity).is_none() {
        return crate::viewport::world_position(world, entity).unwrap_or_default();
    }
    let mut matrix = regia_core::math::Mat4::IDENTITY;
    let mut cursor = Some(entity);
    // A malformed `ChildOf` cycle (bevy_ecs does not prevent one on direct inserts) stops at the
    // bound instead of looping.
    for _ in 0..4096 {
        let Some(current) = cursor else {
            break;
        };
        if let Some(transform) = world.get::<Transform>(current) {
            matrix = regia_core::math::Mat4::from(transform.compute_affine()) * matrix;
        }
        cursor = world.get::<ChildOf>(current).map(ChildOf::parent);
    }
    matrix.w_axis.truncate()
}

/// The first capture of the level that differs from the run's `placed` set, and how: `moved`
/// (its world center, box, weight or fade changed), `was deleted`, or `was added` (an id the run
/// does not bake, or a second capture with a baked id). `None` when the level's captures are the
/// ones the run started with.
fn changed_capture(
    world: &mut World,
    placed: &[regia_render::LocalCaptureDesc],
) -> Option<(u64, &'static str)> {
    let now = level_captures(world);
    for start in placed {
        let mut same_id = now.iter().filter(|(id, _)| *id == start.id);
        match same_id.next() {
            None => return Some((start.id, "was deleted")),
            Some((_, desc)) if desc != start => return Some((start.id, "moved")),
            Some(_) => {}
        }
        if same_id.next().is_some() {
            return Some((start.id, "was added"));
        }
    }
    now.iter()
        .find(|(id, _)| !placed.iter().any(|p| p.id == *id))
        .map(|(id, _)| (*id, "was added"))
}

/// The open level a run belongs to.
fn current_level(world: &World) -> Option<LevelId> {
    world
        .get_resource::<LevelManager>()
        .and_then(LevelManager::current)
}

/// Starts a bake of every placed capture into `assets/<file>` (default [`level_file`]).
///
/// # Errors
/// See the module docs' codes: a run in progress, no project, no renderer, a bad file, no
/// captures, two captures sharing an id, a capture's bad value, more than
/// `regia_render::MAX_LOCAL_CAPTURES`.
pub fn start(world: &mut World, file: Option<&str>) -> Result<(), BakeError> {
    if world
        .get_resource::<CaptureBakeJob>()
        .is_some_and(CaptureBakeJob::running)
    {
        return Err(BakeError::new(
            "bake_busy",
            "a reflection capture bake is running: cancel it (editor.cancel_task { id: \"bake_captures\" }) or wait",
        ));
    }
    let root = crate::mesh_source::project_root(world).ok_or_else(|| {
        BakeError::new(
            "bake_no_project",
            "open a project first: the bake is written under its assets/",
        )
    })?;
    if !world.contains_resource::<RuntimeRenderer>() {
        return Err(BakeError::new(
            "bake_no_renderer",
            "this edit world has no RuntimeRenderer to bake with",
        ));
    }
    let file = file
        .map(str::trim)
        .map_or_else(|| level_file(world), str::to_string);
    if file.is_empty() || !file.ends_with(".fcap") {
        return Err(BakeError::new(
            "bad_file",
            format!("`{file}` must be an asset path ending in .fcap (maps/hall.fcap)"),
        ));
    }
    let path = regia_core::project_path::resolve_write(&root, &format!("assets/{file}")).map_err(|e| {
        BakeError::new(
            "path_outside_project",
            format!("{file}: must be a project-relative path inside the project: {e}"),
        )
    })?;
    let captures = level_captures(world);
    if captures.is_empty() {
        return Err(BakeError::new(
            "captures_none",
            "the level has no reflection capture: add one with Create ▸ Reflection capture",
        ));
    }
    if captures.len() > regia_render::MAX_LOCAL_CAPTURES {
        return Err(BakeError::new(
            "captures_full",
            format!(
                "{} reflection captures; a level bakes at most {}",
                captures.len(),
                regia_render::MAX_LOCAL_CAPTURES
            ),
        ));
    }
    let mut seen = BTreeSet::new();
    for (id, desc) in &captures {
        if !seen.insert(*id) {
            return Err(BakeError::new(
                "capture_duplicate_id",
                format!("two reflection captures use id {id}: give each its own id"),
            ));
        }
        desc.validate()
            .map_err(|e| BakeError::new("capture_bad_value", format!("reflection capture {id}: {e}")))?;
    }
    // Mirror the level onto the renderer, as a game's `sync_local_captures` does: captures the
    // level no longer has go, every level capture is placed at its world position.
    {
        let mut runtime = world.resource_mut::<RuntimeRenderer>();
        let stale: Vec<u64> = runtime
            .0
            .local_captures()
            .iter()
            .map(|info| info.desc.id)
            .filter(|id| !seen.contains(id))
            .collect();
        for id in stale {
            runtime.0.remove_local_capture(id);
        }
        for (id, desc) in &captures {
            runtime
                .0
                .place_local_capture(desc.clone())
                .map_err(|e| BakeError::new("capture_bad_value", format!("reflection capture {id}: {e}")))?;
        }
    }
    let cancel = CancelToken::new();
    let ids: Vec<u64> = captures.iter().map(|(id, _)| *id).collect();
    let placed: Vec<regia_render::LocalCaptureDesc> = captures.into_iter().map(|(_, desc)| desc).collect();
    let job = CaptureBakeJob {
        placed,
        changed_capture: None,
        file,
        path,
        message: format!("baking {} reflection captures", ids.len()),
        ids,
        baked: 0,
        state: BakeState::Running,
        reason: None,
        bytes: 0,
        cancel,
        level: current_level(world),
        project: root,
    };
    publish_row(world, &job);
    world.insert_resource(job);
    Ok(())
}

fn publish_row(world: &mut World, job: &CaptureBakeJob) {
    let total = job.ids.len().max(1);
    world
        .get_resource_or_insert_with(BackgroundTasks::default)
        .publish(TaskRow {
            id: BAKE_TASK.to_string(),
            label: format!("baking reflection captures ({}/{})", job.baked, job.ids.len()),
            progress: Some(job.baked as f32 / total as f32),
            cancel: job.cancel.clone(),
        });
}

fn end(
    world: &mut World,
    job: &mut CaptureBakeJob,
    state: BakeState,
    reason: Option<&'static str>,
    message: String,
) {
    job.state = state;
    job.reason = reason;
    job.message = message;
    if let Some(mut tasks) = world.get_resource_mut::<BackgroundTasks>() {
        tasks.finish(BAKE_TASK);
    }
    // The editor runs no `sync_local_captures`: a placement kept after the run would draw a stale
    // reflection box in the viewport once a capture moves or goes (rev-claude-64 (1) finding 5).
    if let Some(mut runtime) = world.get_resource_mut::<RuntimeRenderer>() {
        for id in &job.ids {
            runtime.0.remove_local_capture(*id);
        }
    }
}

/// Ends a running bake `cancelled` with `bake_level_changed`: what [`tick`] does when the open
/// level or the project is no longer the one the run started on. `crate::level_io::reset_edit_state`
/// calls it for every new edit session (a level replaced, even under the same id). Does nothing
/// without a running job.
pub fn cancel_for_level_change(world: &mut World) {
    let Some(mut job) = world.get_resource::<CaptureBakeJob>().cloned() else {
        return;
    };
    if !job.running() {
        return;
    }
    let message = format!(
        "the open level or project changed after {} of {} captures; {} was not written",
        job.baked,
        job.ids.len(),
        job.file
    );
    job.cancel.cancel();
    end(
        world,
        &mut job,
        BakeState::Cancelled,
        Some("bake_level_changed"),
        message,
    );
    world.insert_resource(job);
}

/// The `.fcap` of the renderer's baked captures among `ids`, in slot order: the layout
/// `render.save_local_captures` writes.
#[must_use]
pub fn fcap_of(infos: &[LocalCaptureInfo], ids: &[u64]) -> FcapFile {
    let captures = infos
        .iter()
        .filter(|info| ids.contains(&info.desc.id))
        .filter_map(|info| {
            let bake = info.desc.bake.as_ref()?;
            Some(FcapCapture {
                id: info.desc.id,
                center: info.desc.center,
                half_extent: info.desc.half_extent,
                weight: info.desc.weight,
                fade: info.desc.fade,
                coverage: bake.coverage,
                mips: bake.mips.clone(),
            })
        })
        .collect();
    FcapFile {
        width: regia_render::LOCAL_CAPTURE_WIDTH,
        captures,
    }
}

/// The editor's per-frame capture work (the shell calls it once per editor frame): first the
/// viewport's baked captures ([`sync_viewport_captures`]), then one step of a running bake: a
/// cancel or a changed capture ends the run with nothing written; otherwise one capture is baked,
/// and after the last one the file is written (temporary file + rename), named in the open level
/// and published to the viewport. The step does nothing without a running job.
pub fn tick(world: &mut World) {
    sync_viewport_captures(world);
    step(world);
}

/// One step of the running bake ([`tick`]).
fn step(world: &mut World) {
    let Some(mut job) = world.get_resource::<CaptureBakeJob>().cloned() else {
        return;
    };
    if !job.running() {
        return;
    }
    let _span = tracing::info_span!("editor_capture_bake_tick", baked = job.baked).entered();
    let project = crate::mesh_source::project_root(world);
    if current_level(world) != job.level || project.as_ref() != Some(&job.project) {
        // The ids, centers and file belong to the level and project the run started on: baking
        // them from another world, or naming the file in another level, would be wrong.
        cancel_for_level_change(world);
        return;
    }
    if let Some((id, what)) = changed_capture(world, &job.placed) {
        // A capture baked at its start position would be written for a place it left (or one
        // it no longer has): end the run, write nothing, name the capture (ADR-7663).
        let message = format!(
            "reflection capture {id} {what} while the bake ran (after {} of {} captures); {} was not written: bake again",
            job.baked,
            job.ids.len(),
            job.file
        );
        job.cancel.cancel();
        job.changed_capture = Some(id);
        end(
            world,
            &mut job,
            BakeState::Cancelled,
            Some("bake_capture_changed"),
            message,
        );
        world.insert_resource(job);
        return;
    }
    if job.cancel.is_cancelled() {
        let message = format!(
            "cancelled after {} of {} captures; {} was not written",
            job.baked,
            job.ids.len(),
            job.file
        );
        end(
            world,
            &mut job,
            BakeState::Cancelled,
            Some("bake_cancelled"),
            message,
        );
        world.insert_resource(job);
        return;
    }
    if let Some(id) = job.ids.get(job.baked).copied() {
        let bake = world
            .get_resource::<CaptureBaker>()
            .copied()
            .unwrap_or_default()
            .0;
        if let Err(err) = bake(world, id) {
            let message = format!("reflection capture {id} did not bake: {err}");
            end(world, &mut job, BakeState::Failed, Some("bake_failed"), message);
            world.insert_resource(job);
            return;
        }
        job.baked += 1;
        publish_row(world, &job);
    }
    if job.baked < job.ids.len() {
        job.message = format!("baked {} of {} captures", job.baked, job.ids.len());
        world.insert_resource(job);
        return;
    }
    let infos = world
        .get_resource::<RuntimeRenderer>()
        .map(|r| r.0.local_captures())
        .unwrap_or_default();
    let fcap = fcap_of(&infos, &job.ids);
    if fcap.captures.len() != job.ids.len() {
        let message = format!(
            "{} of {} captures have a bake; {} was not written",
            fcap.captures.len(),
            job.ids.len(),
            job.file
        );
        end(world, &mut job, BakeState::Failed, Some("bake_failed"), message);
        world.insert_resource(job);
        return;
    }
    let written = job
        .path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(|e| e.to_string())
        .and_then(|()| fcap.write(&job.path).map_err(|e| e.to_string()));
    if let Err(err) = written {
        let message = format!("{}: {err}", job.file);
        end(world, &mut job, BakeState::Failed, Some("fcap_io"), message);
        world.insert_resource(job);
        return;
    }
    job.bytes = std::fs::metadata(&job.path).map_or(0, |m| m.len());
    let mut named = false;
    if let Some(mut settings) = world.get_resource_mut::<regia_scene::level::LevelSettings>() {
        let want = Some(regia_scene::level::LevelCaptures {
            file: job.file.clone(),
        });
        if settings.look.captures != want {
            settings.look.captures = want;
            named = true;
        }
    }
    // The naming is outside the undo history: mark the level unsaved, so the close prompt and
    // Save & Exit keep it (rev-claude-64 (1) finding 1; the GI volume assignment does the same).
    if named && let Some(mut undo) = world.get_resource_mut::<crate::command::UndoStack>() {
        undo.mark_dirty();
    }
    // The viewport draws the new maps from the next frame on, as a game that loads the file.
    publish_bakes(world, &fcap, &job);
    let message = format!(
        "baked {} reflection captures into {} (save the level to keep it)",
        job.ids.len(),
        job.file
    );
    end(world, &mut job, BakeState::Done, None, message);
    world.insert_resource(job);
}

/// What the editor viewport draws of the level's reflection capture bakes (ADR-7663), kept by
/// [`sync_viewport_captures`].
#[derive(Resource, Debug, Default)]
pub struct ViewportCaptures {
    /// The `.fcap` (asset path) whose maps are published: the one the level names, or the last
    /// run's; `None` when none is.
    pub file: Option<String>,
    /// Why the file the level names could not be read (`captures_not_found` or the reader's
    /// `fcap_*` code first); `None` when it loaded or none is named.
    pub error: Option<String>,
    /// The capture ids placed on the renderer with their bake, in id order.
    pub drawn: Vec<u64>,
    /// Files read so far (a test counter: a frame where nothing changed reads none).
    pub reads: u32,
    stamp: Option<FileStamp>,
    /// What is placed, by id: the placement without its map, and the bakes generation it came from.
    placed: BTreeMap<u64, (LocalCaptureDesc, u64)>,
}

impl ViewportCaptures {
    /// The `viewport` object of `editor.bake_captures_status`.
    #[must_use]
    pub fn status_json(&self) -> Value {
        json!({ "file": self.file, "drawn": self.drawn, "error": self.error })
    }
}

/// The file a set of bakes came from: a changed one is read again.
#[derive(Debug, Clone, PartialEq)]
struct FileStamp {
    level: Option<LevelId>,
    project: PathBuf,
    file: String,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn stamp_of(world: &World, root: &std::path::Path, file: &str) -> Option<FileStamp> {
    let path = regia_core::project_path::resolve_read(root, &format!("assets/{file}")).ok()?;
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        level: current_level(world),
        project: root.to_path_buf(),
        file: file.to_string(),
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// Replaces the world's [`LocalCaptureBakes`] with `bakes` (a new generation).
fn set_bakes(world: &mut World, bakes: Vec<(u64, [f32; 3], Arc<regia_render::LocalCaptureBake>)>) {
    let mut published = world.get_resource_or_insert_with(LocalCaptureBakes::default);
    published.by_id = bakes.into_iter().map(|(id, at, b)| (id, (at, b))).collect();
    published.generation += 1;
}

/// A finished run's maps become the level's bakes, as if the level had loaded the file.
fn publish_bakes(world: &mut World, fcap: &FcapFile, job: &CaptureBakeJob) {
    set_bakes(world, regia_framework::local_capture_level::bakes_of(fcap));
    let stamp = stamp_of(world, &job.project, &job.file);
    let mut state = world.get_resource_or_insert_with(ViewportCaptures::default);
    state.file = Some(job.file.clone());
    state.error = None;
    state.stamp = stamp;
}

/// Publishes the bakes of the `.fcap` the open level names in [`LocalCaptureBakes`] when the
/// level, the project, the name or the file changed (the editor's `sync_level_captures`).
fn sync_level_file(world: &mut World) {
    // The edit session's settings name the bake, whether or not the level manager has a current
    // level (a run names its file there the same way); a new session replaces them.
    let named = world
        .get_resource::<regia_scene::level::LevelSettings>()
        .and_then(|l| l.look.captures.as_ref())
        .map(|c| c.file.trim().to_string())
        .filter(|f| !f.is_empty());
    if !world.contains_resource::<regia_scene::level::LevelSettings>() {
        // No level settings to read a name from: what a run published stays.
        return;
    }
    let root = crate::mesh_source::project_root(world);
    let (Some(file), Some(root)) = (named, root) else {
        let had = world
            .get_resource::<ViewportCaptures>()
            .is_some_and(|s| s.file.is_some() || s.stamp.is_some());
        if had {
            // The level closed or names no bake: its maps go.
            set_bakes(world, Vec::new());
            let mut state = world.resource_mut::<ViewportCaptures>();
            state.file = None;
            state.error = None;
            state.stamp = None;
        }
        return;
    };
    let stamp = stamp_of(world, &root, &file);
    let state = world.get_resource_or_insert_with(ViewportCaptures::default);
    if stamp.is_some() && state.stamp == stamp && state.file.as_deref() == Some(file.as_str()) {
        return;
    }
    if stamp.is_none() && state.file.as_deref() == Some(file.as_str()) && state.stamp.is_none() {
        // Still missing: reported once.
        return;
    }
    let _span = tracing::info_span!("editor_capture_bakes_read").entered();
    let read = match &stamp {
        None => Err(format!(
            "captures_not_found: {file}: file not found under assets/"
        )),
        Some(_) => regia_core::project_path::resolve_read(&root, &format!("assets/{file}"))
            .map_err(|e| format!("captures_not_found: {file}: {e}"))
            .and_then(|path| FcapFile::read(&path).map_err(|e| format!("{}: {file}: {e}", e.code()))),
    };
    let (bakes, error) = match read {
        Ok(fcap) => (regia_framework::local_capture_level::bakes_of(&fcap), None),
        Err(message) => {
            tracing::warn!(target: "regia_editor", "{message}");
            (Vec::new(), Some(message))
        }
    };
    set_bakes(world, bakes);
    let mut state = world.resource_mut::<ViewportCaptures>();
    state.reads += 1;
    state.file = Some(file);
    state.error = error;
    state.stamp = stamp;
}

/// The editor viewport's half of a game's `sync_local_captures` (ADR-7663), run by [`tick`]:
/// publishes the level's bakes, places every capture whose bake was made at its world position
/// (within 1 mm) on the [`RuntimeRenderer`] with that bake, takes off the ones that moved, went
/// or lost their bake, and runs the renderer's capture selection from the editor camera (the
/// viewport renders offscreen, which does not). A frame where nothing changed places nothing.
/// Does nothing to the renderer while a bake runs (the run owns its placements).
pub fn sync_viewport_captures(world: &mut World) {
    let _span = tracing::debug_span!("editor_viewport_captures").entered();
    if world
        .get_resource::<CaptureBakeJob>()
        .is_some_and(CaptureBakeJob::running)
    {
        let mut state = world.get_resource_or_insert_with(ViewportCaptures::default);
        // The run replaced the placements; place everything again when it ends.
        state.placed.clear();
        state.drawn.clear();
        return;
    }
    if !world.contains_resource::<RuntimeRenderer>() {
        return;
    }
    sync_level_file(world);
    let generation = world
        .get_resource::<LocalCaptureBakes>()
        .map_or(0, |b| b.generation);
    let mut wanted: BTreeMap<u64, (LocalCaptureDesc, Arc<regia_render::LocalCaptureBake>)> = BTreeMap::new();
    for (id, desc) in level_captures(world) {
        if wanted.contains_key(&id) {
            // A second capture with this id: the first one is drawn, as in a game.
            continue;
        }
        let bake = world
            .get_resource::<LocalCaptureBakes>()
            .and_then(|b| b.matching(id, desc.center));
        if let Some(bake) = bake {
            wanted.insert(id, (desc, bake));
        }
    }
    let placed = std::mem::take(
        &mut world
            .get_resource_or_insert_with(ViewportCaptures::default)
            .placed,
    );
    let mut next = BTreeMap::new();
    {
        let mut runtime = world.resource_mut::<RuntimeRenderer>();
        for id in placed.keys() {
            if !wanted.contains_key(id) {
                runtime.0.remove_local_capture(*id);
            }
        }
        for (id, (desc, bake)) in wanted {
            if placed.get(&id) == Some(&(desc.clone(), generation)) {
                next.insert(id, (desc, generation));
                continue;
            }
            let mut with_bake = desc.clone();
            with_bake.bake = Some(bake);
            match runtime.0.place_local_capture(with_bake) {
                Ok(()) => {
                    next.insert(id, (desc, generation));
                }
                Err(err) => {
                    tracing::warn!(target: "regia_editor", %err, id, "baked reflection capture not drawn");
                }
            }
        }
    }
    let drawn: Vec<u64> = next.keys().copied().collect();
    let any = !drawn.is_empty();
    {
        let mut state = world.resource_mut::<ViewportCaptures>();
        state.placed = next;
        state.drawn = drawn;
    }
    if any && let Some(device) = world.get_resource::<regia_render::RenderDevice>().cloned() {
        let eye = world
            .get_resource::<crate::viewport::EditorCamera>()
            .map_or([0.0; 3], |c| c.eye().to_array());
        world.resource_mut::<RuntimeRenderer>().0.select_local_captures(
            &device,
            eye,
            regia_render::LOCAL_CAPTURE_LAYERS,
        );
    }
}

/// The default [`CaptureBakeFn`]: renders capture `id`'s six faces with the world's
/// [`RuntimeRenderer`] over the edit world's extracted frame (what the viewport draws) and
/// installs the filtered map, as a game's frame does for a queued bake.
///
/// # Errors
/// No GPU device yet (the editor window shares it on its first frame; `editor.capture_viewport`
/// opens a headless one), no renderer, or the bake's frames failed.
pub fn gpu_bake(world: &mut World, id: u64) -> Result<(), String> {
    let device = match world.get_resource::<regia_render::RenderDevice>() {
        Some(device) => device.clone(),
        None => {
            // A headless editor (agent only): open one device and keep it for the next captures,
            // as `editor.capture_viewport` does (the window's shell replaces it with its own).
            let device = crate::render::headless_device().map_err(|e| format!("no GPU device: {e}"))?;
            world.insert_resource(device.clone());
            device
        }
    };
    if !world.contains_resource::<RuntimeRenderer>() {
        return Err("no RuntimeRenderer".into());
    }
    let extracted = crate::render::extract(world);
    let size = regia_render::LOCAL_CAPTURE_FACE;
    let scene = extracted.frame_scene(0, size, size);
    let mut runtime = world.resource_mut::<RuntimeRenderer>();
    runtime
        .0
        .bake_local_capture(&device, &scene, id)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The Tools menu's item: starts the bake. Answers the toast line.
pub fn menu_item(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let project = crate::mesh_source::project_root(world).is_some();
    let busy = world
        .get_resource::<CaptureBakeJob>()
        .is_some_and(CaptureBakeJob::running);
    if crate::agent_parity::action_enabled(
        ui,
        project && !busy,
        &crate::agent_parity::TOOLS_BAKE_CAPTURES,
        None,
    )
    .on_hover_text("Bake every reflection capture of the level and write its .fcap (cancellable)")
    .clicked()
    {
        ui.close();
        return Some(match start(world, None) {
            Ok(()) => format!("Baking reflection captures into {}", level_file(world)),
            Err(err) => format!("{}: {}", err.reason, err.message),
        });
    }
    None
}

/// The progress window while a bake runs (with its Cancel), and the last run's result line.
pub fn window(ctx: &egui::Context, world: &mut World) {
    let Some(job) = world.get_resource::<CaptureBakeJob>().cloned() else {
        return;
    };
    if !job.running() {
        return;
    }
    egui::Window::new("Bake reflection captures")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::Vec2::new(-12.0, -12.0))
        .show(ctx, |ui| {
            let total = job.ids.len().max(1);
            ui.label(&job.message);
            ui.add(egui::ProgressBar::new(job.baked as f32 / total as f32).show_percentage());
            if crate::agent_parity::action_button(ui, &crate::agent_parity::TOOLS_BAKE_CAPTURES_CANCEL)
                .clicked()
            {
                crate::tasks::cancel(world, BAKE_TASK);
            }
        });
}

fn bake_captures_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let file = match params.get("file") {
        None | Some(Value::Null) => None,
        Some(Value::String(file)) => Some(file.as_str()),
        Some(_) => return Err(BakeError::new("bad_file", "`file` must be a string").rpc()),
    };
    start(world, file).map_err(|e| e.rpc())?;
    let job = world.resource::<CaptureBakeJob>();
    Ok(json!({
        "task": BAKE_TASK,
        "file": job.file,
        "ids": job.ids,
        "captures": job.ids.len(),
        "state": job.state.name(),
    }))
}

fn bake_captures_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut status = world.get_resource::<CaptureBakeJob>().map_or_else(
        || json!({ "task": BAKE_TASK, "state": "idle" }),
        CaptureBakeJob::status_json,
    );
    status["viewport"] = world.get_resource::<ViewportCaptures>().map_or_else(
        || ViewportCaptures::default().status_json(),
        ViewportCaptures::status_json,
    );
    Ok(status)
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.bake_captures",
                tool: "bake_captures",
                description: "Tools ▸ Bake reflection captures: start a cancellable bake of every placed \
                    LocalCapture of the level with the editor's renderer, one capture per editor frame; \
                    when all are baked, write assets/<file> (the bytes render.save_local_captures \
                    writes, via a temporary file and a rename) and name it in the level's look.captures. \
                    A cancel (editor.cancel_task { id: \"bake_captures\" }) or a failed bake writes \
                    nothing. Poll editor.bake_captures_status. Refusals (data.reason): bake_busy, \
                    bake_no_project, bake_no_renderer, bad_file, path_outside_project, captures_none, \
                    capture_duplicate_id, capture_bad_value, captures_full.",
                params: &[ParamSchema {
                    name: "file",
                    kind: "string",
                    required: false,
                    description: "Asset path ending in .fcap; default the file the level names, else maps/captures.fcap.",
                }],
                result: "{ task, file, ids, captures, state }",
                mutating: true,
                permission: Permission::EditScene,
            },
            bake_captures_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bake_captures_status",
                tool: "bake_captures_status",
                description: "The reflection capture bake: state (idle, running, done, failed, cancelled), \
                    the file, the ids, how many are baked, the bytes written, and the reason of a failed \
                    (bake_failed, fcap_io) or cancelled (bake_cancelled; bake_level_changed when the open \
                    level or project changed; bake_capture_changed, with changed_capture, when a capture \
                    moved, was resized, deleted or added during the run) run. viewport: the baked \
                    captures the editor viewport draws (file, drawn ids, the error reading the file the \
                    level names).",
                params: &[],
                result: "{ task, state, file, ids, baked, total, reason, message, bytes, changed_capture, viewport: { file, drawn, error } }",
                mutating: false,
                permission: Permission::Read,
            },
            bake_captures_status as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::App;
    use std::sync::Arc;

    /// A deterministic stand-in for the GPU bake: a map made from the capture's id and box.
    pub(crate) fn fake_bake(world: &mut World, id: u64) -> Result<(), String> {
        let mut runtime = world.resource_mut::<RuntimeRenderer>();
        let infos = runtime.0.local_captures();
        let info = infos
            .iter()
            .find(|i| i.desc.id == id)
            .ok_or_else(|| format!("no capture {id}"))?;
        let mut desc = info.desc.clone();
        desc.bake = Some(Arc::new(fake_map(id, desc.center)));
        runtime.0.place_local_capture(desc).map_err(|e| e.to_string())
    }

    fn fake_map(id: u64, center: [f32; 3]) -> regia_render::LocalCaptureBake {
        let width = regia_render::LOCAL_CAPTURE_WIDTH;
        let seed = (id as u8).wrapping_mul(37) ^ (center[0] as i32 as u8);
        let mips = (0..regia_render::local_capture_mip_count())
            .map(|m| {
                let n = regia_asset::local_capture_bake::fcap_mip_bytes(width, m);
                (0..n).map(|i| seed.wrapping_add(i as u8)).collect()
            })
            .collect();
        regia_render::LocalCaptureBake {
            width,
            coverage: 0.5,
            mips,
        }
    }

    /// A project with an edit world holding two captures (ids 3 and 7) and the fake baker,
    /// transforms propagated.
    fn project_world(tag: &str) -> (App, PathBuf) {
        let (mut app, root) = project_app(tag);
        // World positions are the propagated transforms, as in the editor's frames.
        app.update();
        (app, root)
    }

    /// [`project_world`] before its first update (plugins can still be added).
    fn project_app(tag: &str) -> (App, PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets/maps")).expect("assets");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.init_resource::<RuntimeRenderer>();
        world.insert_resource(CaptureBaker(fake_bake));
        for (id, x) in [(3u64, -2.0f32), (7, 4.0)] {
            world.spawn((
                regia_scene::prelude::Transform::from_translation(regia_core::math::Vec3::new(x, 1.0, 0.0)),
                LocalCapture::new(id, [2.0, 1.5, 3.0]),
            ));
        }
        (app, root)
    }

    #[test]
    fn capture_bake_writes_every_capture_after_one_step_per_capture() {
        let (mut app, root) = project_world("capture_bake_steps");
        let world = app.world_mut();
        start(world, Some("maps/hall.fcap")).expect("start");
        let path = root.join("assets/maps/hall.fcap");
        assert!(
            world
                .resource::<BackgroundTasks>()
                .rows()
                .iter()
                .any(|r| r.id == BAKE_TASK)
        );
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().baked, 1);
        assert!(!path.exists(), "nothing is written before the last capture");
        tick(world);
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!(job.state, BakeState::Done, "{}", job.message);
        let file = FcapFile::read(&path).expect("the bake reads");
        let ids: Vec<u64> = file.captures.iter().map(|c| c.id).collect();
        assert_eq!(ids, [3, 7]);
        assert_eq!(file.captures[1].center, [4.0, 1.0, 0.0]);
        assert_eq!(file.captures[1].half_extent, [2.0, 1.5, 3.0]);
        assert_eq!(job.bytes, std::fs::metadata(&path).expect("meta").len());
        assert!(world.resource::<BackgroundTasks>().rows().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// ADR-7592 acceptance: a cancel mid-bake leaves the old file byte for byte and no temporary
    /// file beside it.
    #[test]
    fn capture_bake_cancel_mid_bake_leaves_the_old_file_untouched() {
        let (mut app, root) = project_world("capture_bake_cancel");
        let world = app.world_mut();
        let path = root.join("assets/maps/hall.fcap");
        std::fs::write(&path, b"the previous bake").expect("old file");
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        assert!(crate::tasks::cancel(world, BAKE_TASK), "the task row cancels");
        tick(world);
        tick(world);
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!(job.state, BakeState::Cancelled);
        assert_eq!(job.reason, Some("bake_cancelled"));
        assert_eq!(job.baked, 1, "the second capture never baked");
        assert_eq!(std::fs::read(&path).expect("old"), b"the previous bake");
        let names: Vec<String> = std::fs::read_dir(root.join("assets/maps"))
            .expect("dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        assert_eq!(names, ["hall.fcap"], "no temporary file is left");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn capture_bake_refusals_name_their_reason() {
        let (mut app, root) = project_world("capture_bake_refusals");
        let world = app.world_mut();
        let reason = |world: &mut World, file: Option<&str>| start(world, file).err().map(|e| e.reason);
        assert_eq!(reason(world, Some("maps/hall.png")), Some("bad_file"));
        assert_eq!(
            reason(world, Some("../../out.fcap")),
            Some("path_outside_project")
        );
        start(world, None).expect("start");
        assert_eq!(world.resource::<CaptureBakeJob>().file, DEFAULT_FILE);
        assert_eq!(reason(world, None), Some("bake_busy"));
        world.resource::<CaptureBakeJob>().cancel.cancel();
        tick(world);
        // Two captures sharing an id would overwrite each other's slot.
        world.spawn((
            regia_scene::prelude::Transform::default(),
            LocalCapture::new(3, [1.0; 3]),
        ));
        assert_eq!(reason(world, None), Some("capture_duplicate_id"));
        let mut empty = App::new();
        empty.add_plugins(regia_scene::ScenePlugin);
        let ew = empty.world_mut();
        ew.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        ew.init_resource::<RuntimeRenderer>();
        assert_eq!(reason(ew, None), Some("captures_none"));
        ew.remove_resource::<RuntimeRenderer>();
        assert_eq!(reason(ew, None), Some("bake_no_renderer"));
        ew.remove_resource::<crate::agent_ext::EditorProject>();
        assert_eq!(reason(ew, None), Some("bake_no_project"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A failed capture bake ends the run `bake_failed` and writes nothing.
    #[test]
    fn capture_bake_failure_writes_nothing() {
        fn broken(_: &mut World, _: u64) -> Result<(), String> {
            Err("no GPU".into())
        }
        let (mut app, root) = project_world("capture_bake_fail");
        let world = app.world_mut();
        world.insert_resource(CaptureBaker(broken));
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!((job.state, job.reason), (BakeState::Failed, Some("bake_failed")));
        assert!(!root.join("assets/maps/hall.fcap").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A game frame's stand-in for `bake_pending_local_captures`: every queued capture gets the
    /// same deterministic map [`fake_bake`] installs.
    fn fake_gpu_frame(world: &mut World) {
        let pending: Vec<u64> = world
            .resource::<RuntimeRenderer>()
            .0
            .local_captures()
            .iter()
            .filter(|info| info.pending)
            .map(|info| info.desc.id)
            .collect();
        for id in pending {
            fake_bake(world, id).expect("fake bake");
        }
    }

    /// ADR-7592 acceptance: on a two-capture level the editor job writes the bytes the
    /// `regia bake-captures` path writes. The CLI side is its protocol sequence against a running
    /// app over the agent transport (`render.bake_local_capture {}`, `render.list_local_captures`
    /// until none is pending, `render.save_local_captures { file, name_in_level: true }`), with
    /// the game's `sync_local_captures` placing the captures; both sides bake with the same
    /// deterministic stand-in for the GPU.
    #[test]
    fn capture_bake_bytes_equal_the_cli_path() {
        use regia_app::prelude::{Last, MinimalPlugins};
        let (mut game, game_root) = project_app("capture_bake_cli_game");
        game.add_plugins(MinimalPlugins);
        regia_render::register_render_types(&mut game);
        game.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(game_root.clone()),
        });
        game.add_plugins(regia_agent::AgentPlugin);
        game.add_systems(Last, (regia_render::sync_local_captures, fake_gpu_frame).chain());
        game.finish_plugins();
        game.update();
        let project = game_root.clone();
        let cli = std::thread::spawn(move || -> Result<Value, String> {
            let mut client =
                regia_agent::client::Client::connect_project(&project).map_err(|e| e.to_string())?;
            let queued = client
                .call("render.bake_local_capture", json!({}))
                .map_err(|e| e.to_string())?;
            if queued["queued"] != 2 {
                return Err(format!("queued {queued}"));
            }
            loop {
                let list = client
                    .call("render.list_local_captures", json!({}))
                    .map_err(|e| e.to_string())?;
                let pending = list["captures"]
                    .as_array()
                    .map_or(0, |c| c.iter().filter(|c| c["pending"] == true).count());
                if pending == 0 {
                    break;
                }
            }
            client
                .call(
                    "render.save_local_captures",
                    json!({ "file": "maps/hall.fcap", "name_in_level": true }),
                )
                .map_err(|e| e.to_string())
        });
        while !cli.is_finished() {
            game.update();
        }
        let saved = cli.join().expect("the CLI thread").expect("the CLI path");
        assert_eq!(saved["captures"], 2, "{saved}");

        let (mut editor, editor_root) = project_world("capture_bake_cli_editor");
        let world = editor.world_mut();
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Done);
        let cli_bytes = std::fs::read(game_root.join("assets/maps/hall.fcap")).expect("CLI file");
        let editor_bytes = std::fs::read(editor_root.join("assets/maps/hall.fcap")).expect("editor file");
        assert_eq!(
            FcapFile::read(&game_root.join("assets/maps/hall.fcap"))
                .expect("reads")
                .captures
                .len(),
            2
        );
        assert!(
            cli_bytes == editor_bytes,
            "the editor's .fcap equals the CLI's byte for byte"
        );
        drop(game);
        std::fs::remove_dir_all(&game_root).ok();
        std::fs::remove_dir_all(&editor_root).ok();
    }

    /// The agent methods start and report the same run as the menu item, and the status names
    /// the file the level is given.
    #[test]
    fn capture_bake_agent_methods_start_and_report() {
        let (mut app, root) = project_world("capture_bake_agent");
        let world = app.world_mut();
        world.init_resource::<regia_scene::level::LevelSettings>();
        let call = |world: &mut World, name: &str, params: Value| {
            let (_, handler) = crate::agent_ext::all_methods()
                .into_iter()
                .find(|(schema, _)| schema.name == name)
                .unwrap_or_else(|| panic!("{name} is installed"));
            handler(world, &params)
        };
        assert_eq!(
            call(world, "editor.bake_captures_status", json!({})).expect("idle")["state"],
            "idle"
        );
        let started = call(world, "editor.bake_captures", json!({ "file": "maps/a.fcap" })).expect("start");
        assert_eq!(started["ids"], json!([3, 7]));
        let refused = call(world, "editor.bake_captures", json!({})).expect_err("busy");
        assert_eq!(refused.data.as_ref().expect("data")["reason"], "bake_busy");
        tick(world);
        tick(world);
        let status = call(world, "editor.bake_captures_status", json!({})).expect("status");
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["baked"], 2);
        assert_eq!(
            world
                .resource::<regia_scene::level::LevelSettings>()
                .look
                .captures
                .as_ref()
                .map(|c| c.file.as_str()),
            Some("maps/a.fcap"),
            "the level names its bake"
        );
        // editor.cancel_task stops a run.
        call(world, "editor.bake_captures", json!({ "file": "maps/a.fcap" })).expect("again");
        call(world, "editor.cancel_task", json!({ "id": BAKE_TASK })).expect("cancel");
        tick(world);
        let status = call(world, "editor.bake_captures_status", json!({})).expect("status");
        assert_eq!(status["state"], "cancelled");
        std::fs::remove_dir_all(&root).ok();
    }

    /// rev-claude-64 (1) finding 1: a finished bake that names a new file in `look.captures`
    /// leaves a saved level dirty, so the close prompt and Save & Exit keep the naming; a bake
    /// that writes the file the level already names leaves it clean.
    #[test]
    fn capture_bake_done_marks_a_saved_level_dirty_when_the_naming_changes() {
        let (mut app, root) = project_world("capture_bake_dirty");
        let world = app.world_mut();
        world.init_resource::<regia_scene::level::LevelSettings>();
        world.insert_resource(crate::command::UndoStack::new());
        world.resource_mut::<crate::command::UndoStack>().mark_saved();
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Done);
        assert!(
            world.resource::<crate::command::UndoStack>().is_dirty(),
            "the level now names maps/hall.fcap: it must be saved to keep the bake"
        );
        // Saved again, a second bake into the same file changes nothing the level saves.
        world.resource_mut::<crate::command::UndoStack>().mark_saved();
        start(world, Some("maps/hall.fcap")).expect("again");
        tick(world);
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Done);
        assert!(
            !world.resource::<crate::command::UndoStack>().is_dirty(),
            "the level already names the file"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// rev-claude-64 (1) finding 2: File ▸ New level mid-run ends the run `cancelled` with
    /// `bake_level_changed`: the old level's captures are not baked from the new level's world,
    /// no file is written and the new level's `look.captures` stays as it was.
    #[test]
    fn capture_bake_cancels_when_the_level_changes() {
        let (mut app, root) = project_world("capture_bake_new_level");
        let world = app.world_mut();
        crate::level_io::new_level(world);
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        crate::level_io::new_level(world);
        world.get_resource_or_insert_with(regia_scene::level::LevelSettings::default);
        tick(world);
        tick(world);
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!(job.state, BakeState::Cancelled, "{}", job.message);
        assert_eq!(job.reason, Some("bake_level_changed"));
        assert!(!root.join("assets/maps/hall.fcap").exists(), "nothing is written");
        assert_eq!(
            world
                .resource::<regia_scene::level::LevelSettings>()
                .look
                .captures,
            None,
            "the new level is not given the old level's bake"
        );
        assert!(world.resource::<BackgroundTasks>().rows().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// rev-claude-64 (1) finding 2: a level replaced under the same id (File ▸ Open of the same
    /// file, revert, autosave restore) starts a new edit session (`reset_edit_state`), which
    /// cancels the run the same way.
    #[test]
    fn capture_bake_cancels_when_the_edit_session_restarts() {
        let (mut app, root) = project_world("capture_bake_reset");
        let world = app.world_mut();
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        crate::level_io::reset_edit_state(world);
        tick(world);
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!(
            (job.state, job.reason),
            (BakeState::Cancelled, Some("bake_level_changed")),
            "{}",
            job.message
        );
        assert!(!root.join("assets/maps/hall.fcap").exists(), "nothing is written");
        std::fs::remove_dir_all(&root).ok();
    }

    /// rev-claude-64 (1) finding 2: a project switch mid-run cancels it; neither project gets
    /// a file.
    #[test]
    fn capture_bake_cancels_when_the_project_changes() {
        let (mut app, root) = project_world("capture_bake_project");
        let other = regia_test_utils::unique_temp_dir("capture_bake_project_other");
        std::fs::create_dir_all(other.join("assets/maps")).expect("assets");
        let world = app.world_mut();
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(other.clone()),
        });
        tick(world);
        tick(world);
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!(
            (job.state, job.reason),
            (BakeState::Cancelled, Some("bake_level_changed")),
            "{}",
            job.message
        );
        assert!(!root.join("assets/maps/hall.fcap").exists());
        assert!(!other.join("assets/maps/hall.fcap").exists());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// rev-claude-64 (1) finding 3: the bake's centers are the captures' transforms as they are
    /// when the bake starts, parent chain included, even before transform propagation ran (an
    /// agent batch that moves a capture and then starts the bake).
    #[test]
    fn capture_bake_centers_include_unpropagated_moves_and_parents() {
        let (mut app, root) = project_world("capture_bake_unpropagated");
        let world = app.world_mut();
        let seven = world
            .query::<(Entity, &LocalCapture)>()
            .iter(world)
            .find(|(_, c)| c.id == 7)
            .map(|(e, _)| e)
            .expect("capture 7");
        world
            .get_mut::<regia_scene::prelude::Transform>(seven)
            .expect("transform")
            .translation
            .x = 9.0;
        let parent = world
            .spawn(regia_scene::prelude::Transform::from_translation(
                regia_core::math::Vec3::new(10.0, 0.0, 0.0),
            ))
            .id();
        world.spawn((
            regia_scene::prelude::Transform::from_translation(regia_core::math::Vec3::new(1.0, 1.0, 0.0)),
            LocalCapture::new(11, [1.0; 3]),
            ChildOf(parent),
        ));
        start(world, Some("maps/hall.fcap")).expect("start");
        for _ in 0..3 {
            tick(world);
        }
        assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Done);
        let file = FcapFile::read(&root.join("assets/maps/hall.fcap")).expect("the bake reads");
        let centers: Vec<(u64, [f32; 3])> = file.captures.iter().map(|c| (c.id, c.center)).collect();
        assert!(centers.contains(&(7, [9.0, 1.0, 0.0])), "{centers:?}");
        assert!(centers.contains(&(11, [11.0, 1.0, 0.0])), "{centers:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The entity of capture `id`.
    fn capture_entity(world: &mut World, id: u64) -> Entity {
        world
            .query::<(Entity, &LocalCapture)>()
            .iter(world)
            .find(|(_, c)| c.id == id)
            .map(|(e, _)| e)
            .unwrap_or_else(|| panic!("capture {id}"))
    }

    /// Steps a run on `edit` after its first capture baked: the run ends `cancelled` with
    /// `bake_capture_changed` naming `expected`, the old file stays byte for byte, and the
    /// renderer keeps no placement.
    fn stepped_bake_cancels(tag: &str, expected: u64, edit: impl FnOnce(&mut World)) {
        let (mut app, root) = project_world(tag);
        let world = app.world_mut();
        let path = root.join("assets/maps/hall.fcap");
        std::fs::write(&path, b"the previous bake").expect("old file");
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().baked, 1);
        edit(world);
        tick(world);
        tick(world);
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!(
            (job.state, job.reason, job.changed_capture),
            (BakeState::Cancelled, Some("bake_capture_changed"), Some(expected)),
            "{tag}: {}",
            job.message
        );
        assert!(
            job.message.contains(&format!("capture {expected}")),
            "{}",
            job.message
        );
        assert_eq!(job.baked, 1, "{tag}: nothing baked after the change");
        assert_eq!(std::fs::read(&path).expect("old"), b"the previous bake", "{tag}");
        assert_eq!(
            world.resource::<RuntimeRenderer>().0.local_captures().len(),
            0,
            "{tag}"
        );
        assert_eq!(job.status_json()["changed_capture"], expected, "{tag}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// ADR-7663 (w9 known issue): a capture moved, resized, deleted or added while a bake of the
    /// same level runs is never written at its start position: the run stops at its next step
    /// and names the capture. Capture 3 is the one already baked, 7 the one still to bake.
    #[test]
    fn capture_bake_cancels_when_a_capture_changes_mid_run() {
        stepped_bake_cancels("capture_bake_moved_baked", 3, |world| {
            let e = capture_entity(world, 3);
            world.get_mut::<Transform>(e).expect("transform").translation.y += 0.5;
        });
        stepped_bake_cancels("capture_bake_moved_pending", 7, |world| {
            let e = capture_entity(world, 7);
            world.get_mut::<Transform>(e).expect("transform").translation.x -= 1.0;
        });
        stepped_bake_cancels("capture_bake_resized", 7, |world| {
            let e = capture_entity(world, 7);
            world.get_mut::<LocalCapture>(e).expect("capture").fade = 2.0;
        });
        stepped_bake_cancels("capture_bake_deleted", 3, |world| {
            let e = capture_entity(world, 3);
            world.despawn(e);
        });
        stepped_bake_cancels("capture_bake_added", 9, |world| {
            world.spawn((Transform::default(), LocalCapture::new(9, [1.0; 3])));
        });
        // A parent of a capture moving moves the capture.
        stepped_bake_cancels("capture_bake_parent_moved", 7, |world| {
            let parent = world.spawn(Transform::default()).id();
            let e = capture_entity(world, 7);
            world.entity_mut(e).insert(ChildOf(parent));
            world
                .get_mut::<Transform>(parent)
                .expect("transform")
                .translation
                .z = 3.0;
        });
    }

    /// Edits that leave every capture where it was (another entity moved) do not stop a run.
    #[test]
    fn capture_bake_ignores_edits_that_leave_the_captures_alone() {
        let (mut app, root) = project_world("capture_bake_other_edit");
        let world = app.world_mut();
        let other = world.spawn(Transform::default()).id();
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        world
            .get_mut::<Transform>(other)
            .expect("transform")
            .translation
            .x = 5.0;
        // A capture moved and moved back is where the run placed it.
        let e = capture_entity(world, 7);
        world.get_mut::<Transform>(e).expect("transform").translation.x += 1.0;
        world.get_mut::<Transform>(e).expect("transform").translation.x -= 1.0;
        tick(world);
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!(job.state, BakeState::Done, "{}", job.message);
        assert_eq!(job.changed_capture, None);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The renderer's placements: id, center, and whether each holds a map.
    fn placements(world: &World) -> Vec<(u64, [f32; 3], bool)> {
        let mut out: Vec<(u64, [f32; 3], bool)> = world
            .resource::<RuntimeRenderer>()
            .0
            .local_captures()
            .iter()
            .map(|i| (i.desc.id, i.desc.center, i.desc.bake.is_some()))
            .collect();
        out.sort_by_key(|p| p.0);
        out
    }

    /// ADR-7663 (w9 known issue "the editor viewport does not draw baked captures"): after a bake
    /// the edit world's `LocalCaptureBakes` holds the baked set (the maps the file holds, at the
    /// centers they were baked from), the next editor frame places every capture on the
    /// viewport's renderer with its map, and a capture moved since its bake is taken off until it
    /// is baked again; the status reports what the viewport draws.
    #[test]
    fn capture_bake_publishes_the_baked_set_to_the_viewport() {
        let (mut app, root) = project_world("capture_bake_viewport");
        let world = app.world_mut();
        world.init_resource::<regia_scene::level::LevelSettings>();
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Done);
        let file = FcapFile::read(&root.join("assets/maps/hall.fcap")).expect("the bake reads");
        {
            let bakes = world.resource::<LocalCaptureBakes>();
            let mut held: Vec<(u64, [f32; 3])> = bakes.by_id.iter().map(|(id, (at, _))| (*id, *at)).collect();
            held.sort_by_key(|h| h.0);
            assert_eq!(held, [(3, [-2.0, 1.0, 0.0]), (7, [4.0, 1.0, 0.0])]);
            for capture in &file.captures {
                let (_, bake) = &bakes.by_id[&capture.id];
                assert_eq!(bake.mips, capture.mips, "capture {}: the file's map", capture.id);
            }
        }
        tick(world);
        assert_eq!(
            placements(world),
            [(3, [-2.0, 1.0, 0.0], true), (7, [4.0, 1.0, 0.0], true)],
            "the viewport draws both baked captures"
        );
        assert_eq!(world.resource::<ViewportCaptures>().drawn, [3, 7]);
        // A frame where nothing changed places nothing again (same maps, no new generation).
        let bakes_before: Vec<u32> = world
            .resource::<RuntimeRenderer>()
            .0
            .local_captures()
            .iter()
            .map(|i| i.bakes)
            .collect();
        tick(world);
        let bakes_after: Vec<u32> = world
            .resource::<RuntimeRenderer>()
            .0
            .local_captures()
            .iter()
            .map(|i| i.bakes)
            .collect();
        assert_eq!(bakes_before, bakes_after);
        assert_eq!(
            world.resource::<ViewportCaptures>().reads,
            0,
            "the run's own file is not read back"
        );
        // Capture 7 moved: its map was made elsewhere, so the viewport stops drawing it.
        let seven = capture_entity(world, 7);
        world
            .get_mut::<Transform>(seven)
            .expect("transform")
            .translation
            .x = 6.0;
        tick(world);
        assert_eq!(placements(world), [(3, [-2.0, 1.0, 0.0], true)]);
        let status = bake_captures_status(world, &json!({})).expect("status");
        assert_eq!(status["viewport"]["drawn"], json!([3]), "{status}");
        assert_eq!(status["viewport"]["file"], "maps/hall.fcap");
        // Moved back to where it was baked, it is drawn again.
        world
            .get_mut::<Transform>(seven)
            .expect("transform")
            .translation
            .x = 4.0;
        tick(world);
        assert_eq!(world.resource::<ViewportCaptures>().drawn, [3, 7]);
        // A new run takes the placements over; after it the baked set is drawn again.
        start(world, Some("maps/hall.fcap")).expect("again");
        tick(world);
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Done);
        tick(world);
        assert_eq!(placements(world).len(), 2);
        assert!(placements(world).iter().all(|p| p.2));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A level that names a `.fcap` draws its maps in the viewport when it opens (the editor's
    /// half of the game's level install), reads it once, and a missing file reports
    /// `captures_not_found` once and draws nothing.
    #[test]
    fn viewport_draws_the_bake_the_level_names() {
        let (mut app, root) = project_world("capture_bake_level_file");
        let world = app.world_mut();
        world.init_resource::<regia_scene::level::LevelSettings>();
        // Make the file with a run, then forget everything the run published.
        start(world, Some("maps/hall.fcap")).expect("start");
        tick(world);
        tick(world);
        world.remove_resource::<ViewportCaptures>();
        world.remove_resource::<LocalCaptureBakes>();
        for id in [3, 7] {
            world.resource_mut::<RuntimeRenderer>().0.remove_local_capture(id);
        }
        tick(world);
        assert_eq!(world.resource::<ViewportCaptures>().reads, 1);
        assert_eq!(world.resource::<ViewportCaptures>().drawn, [3, 7]);
        assert_eq!(placements(world).len(), 2);
        tick(world);
        assert_eq!(
            world.resource::<ViewportCaptures>().reads,
            1,
            "unchanged: not read again"
        );
        // The level names a file that is not there: nothing drawn, one error.
        world
            .resource_mut::<regia_scene::level::LevelSettings>()
            .look
            .captures = Some(regia_scene::level::LevelCaptures {
            file: "maps/gone.fcap".to_string(),
        });
        tick(world);
        tick(world);
        let state = world.resource::<ViewportCaptures>();
        assert_eq!(state.reads, 2);
        assert!(
            state
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("captures_not_found")),
            "{:?}",
            state.error
        );
        assert!(state.drawn.is_empty());
        assert!(placements(world).is_empty());
        // A level that names nothing draws nothing.
        world
            .resource_mut::<regia_scene::level::LevelSettings>()
            .look
            .captures = None;
        tick(world);
        assert!(world.resource::<LocalCaptureBakes>().by_id.is_empty());
        assert!(world.resource::<ViewportCaptures>().file.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    /// rev-claude-64 (1) finding 5: the editor viewport draws through the world's renderer and
    /// the editor runs no `sync_local_captures`, so a run leaves no capture placed on it (a later
    /// move or delete would leave a stale reflection box): not after a finished run, not after a
    /// cancelled one.
    #[test]
    fn capture_bake_leaves_no_placement_on_the_renderer() {
        let (mut app, root) = project_world("capture_bake_placements");
        let world = app.world_mut();
        let placed = |world: &World| world.resource::<RuntimeRenderer>().0.local_captures().len();
        start(world, Some("maps/hall.fcap")).expect("start");
        assert_eq!(placed(world), 2, "the run places the level's captures");
        tick(world);
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Done);
        assert!(root.join("assets/maps/hall.fcap").exists());
        assert_eq!(placed(world), 0, "a finished run removes its placements");
        start(world, Some("maps/hall.fcap")).expect("again");
        tick(world);
        world.resource::<CaptureBakeJob>().cancel.cancel();
        tick(world);
        assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Cancelled);
        assert_eq!(placed(world), 0, "a cancelled run removes its placements");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A prefab whose template holds a reflection capture (id 5), written under `root`.
    fn write_capture_prefab(root: &std::path::Path) -> PathBuf {
        use regia_ecs::reflect::TypePath as _;
        let mut registry = regia_ecs::reflect::TypeRegistry::new();
        registry.register::<LocalCapture>();
        let ron = regia_ecs::serialize::ReflectCodec::new(&registry)
            .to_ron_typed(LocalCapture::new(5, [1.0, 1.0, 1.0]).as_partial_reflect())
            .expect("capture ron");
        let path = root.join("assets/props/post.prefab.ron");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        let text = format!(
            r#"( format_version: 1, entities: [
                ( guid: "8b6f2a11-0000-4000-8000-0000000000a1", name: "Post",
                  components: {{ "regia_scene::transform::Transform": (), "{}": {} }} ),
            ] )"#,
            LocalCapture::type_path(),
            ron.trim_end()
        );
        regia_test_utils::write_atomic(&path, text.as_bytes()).expect("prefab");
        path
    }

    fn capture_ids(world: &mut World) -> Vec<u64> {
        let mut ids: Vec<u64> = world.query::<&LocalCapture>().iter(world).map(|c| c.id).collect();
        ids.sort_unstable();
        ids
    }

    /// ADR-7716 (the ADR-7699 follow-up): each instance of a prefab whose template holds a
    /// capture gets its own id, so a level with two of them bakes (the bake used to refuse with
    /// `capture_duplicate_id`) and the `.fcap` addresses both. The id is an override on the
    /// instance, so a save and reload, an undo and a redo give the same ids.
    #[test]
    fn two_instances_of_a_prefab_with_a_capture_each_get_their_own_id() {
        let (mut app, root) = project_world("capture_prefab_instances");
        let world = app.world_mut();
        world
            .resource::<AppTypeRegistry>()
            .write()
            .register::<LocalCapture>();
        let prefab = write_capture_prefab(&root);
        let spawn = |world: &mut World, x: f32| -> Entity {
            let (_, handler) = crate::agent_ext::all_methods()
                .into_iter()
                .find(|(schema, _)| schema.name == "editor.spawn_prefab")
                .expect("editor.spawn_prefab is installed");
            let out = handler(
                world,
                &serde_json::json!({ "path": "assets/props/post.prefab.ron", "translation": [x, 1.0, 0.0] }),
            )
            .expect("spawn through the agent method");
            Entity::from_bits(out["entity"].as_u64().expect("entity"))
        };
        let first = spawn(world, 10.0);
        let second = spawn(world, 20.0);
        app.update();
        let world = app.world_mut();
        // The level's own captures hold 3 and 7; the template's 5 is free, so the first instance
        // keeps it and the second takes the next unused id.
        assert_eq!(capture_ids(world), [3, 5, 7, 8]);

        // The bake accepts the level and writes every capture.
        start(world, Some("maps/hall.fcap")).expect("two instances bake");
        for _ in 0..4 {
            tick(world);
        }
        let job = world.resource::<CaptureBakeJob>().clone();
        assert_eq!(job.state, BakeState::Done, "{}", job.message);
        let file = FcapFile::read(&root.join("assets/maps/hall.fcap")).expect("the bake reads");
        let mut ids: Vec<u64> = file.captures.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [3, 5, 7, 8]);

        // The new id is saved with the instance.
        let saved = crate::command::subtree_snapshot(world, &[first.to_bits(), second.to_bits()]);
        let overrides: Vec<String> = saved
            .entities
            .iter()
            .filter_map(|e| e.prefab_instance.as_ref())
            .flat_map(|i| {
                i.overrides
                    .iter()
                    .map(|o| format!("{} {}", o.path, o.value.get_ron()))
            })
            .collect();
        assert_eq!(
            overrides,
            [".id 8"],
            "only the second instance overrides the template's id"
        );

        // A reload of the saved instances into a world with the same level content.
        let (mut reload, reload_root) = project_world("capture_prefab_reload");
        let rw = reload.world_mut();
        rw.resource::<AppTypeRegistry>()
            .write()
            .register::<LocalCapture>();
        let asset_id = crate::content::prefab_asset_id(&prefab).expect("asset id");
        let text = std::fs::read_to_string(&prefab).expect("prefab text");
        let template = regia_scene::prefab::PrefabTemplate::from_scene_file(
            asset_id.0,
            &regia_scene::prelude::SceneFile::from_ron_str(&text).expect("prefab file"),
            &rw.resource::<AppTypeRegistry>().clone().read(),
        )
        .expect("template");
        rw.get_resource_or_insert_with::<regia_scene::prefab::PrefabLibrary>(Default::default)
            .insert(template);
        crate::command::spawn_and_exhume(rw, &saved, None, "reload");
        assert_eq!(capture_ids(rw), [3, 5, 7, 8], "a reload keeps both ids");
        std::fs::remove_dir_all(&reload_root).ok();

        // Undo and redo.
        let world = app.world_mut();
        assert!(crate::command::undo_world(world));
        assert!(crate::command::undo_world(world));
        assert_eq!(capture_ids(world), [3, 7]);
        assert!(crate::command::redo_world(world));
        assert!(crate::command::redo_world(world));
        assert_eq!(
            capture_ids(world),
            [3, 5, 7, 8],
            "redo brings both back with their ids"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Runs the installed agent method `name`.
    fn agent_call(world: &mut World, name: &str, params: serde_json::Value) -> serde_json::Value {
        let (_, handler) = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == name)
            .unwrap_or_else(|| panic!("{name} is installed"));
        handler(world, &params).unwrap_or_else(|e| panic!("{name}: {e:?}"))
    }

    /// ADR-7716: copying a prefab instance (Edit ▸ Duplicate through the agent, Copy then Paste) renumbers
    /// the copy's captures, whether the id came from the template or from an override the original
    /// carries, and the level still bakes. Undo and redo keep the copy's id.
    #[test]
    fn copies_of_a_prefab_instance_get_their_own_capture_ids() {
        let (mut app, root) = project_world("capture_prefab_copies");
        let world = app.world_mut();
        world
            .resource::<AppTypeRegistry>()
            .write()
            .register::<LocalCapture>();
        write_capture_prefab(&root);
        let spawn = |world: &mut World, x: f32| -> Entity {
            let out = agent_call(
                world,
                "editor.spawn_prefab",
                serde_json::json!({ "path": "assets/props/post.prefab.ron", "translation": [x, 1.0, 0.0] }),
            );
            Entity::from_bits(out["entity"].as_u64().expect("entity"))
        };
        let first = spawn(world, 10.0);
        let second = spawn(world, 20.0);
        assert_eq!(capture_ids(world), [3, 5, 7, 8]);
        let bakes = |world: &mut World| {
            start(world, Some("maps/copies.fcap")).expect("the level bakes");
            for _ in 0..8 {
                tick(world);
            }
            assert_eq!(world.resource::<CaptureBakeJob>().state, BakeState::Done);
        };

        // Duplicate the instance that kept the template's id, then the one that overrides it.
        for (original, expect) in [
            (first, &[3u64, 5, 7, 8, 9][..]),
            (second, &[3, 5, 7, 8, 9, 10][..]),
        ] {
            agent_call(
                world,
                "editor.duplicate",
                serde_json::json!({ "entity": original.to_bits() }),
            );
            assert_eq!(capture_ids(world), expect, "Duplicate of {original:?}");
        }
        bakes(world);

        // Copy then Paste twice.
        let mut selection = crate::selection::Selection::default();
        selection.select(first, crate::selection::SelectMode::Replace);
        world.insert_resource(selection);
        agent_call(
            world,
            "editor.menu_invoke",
            serde_json::json!({ "path": "Edit/Copy" }),
        );
        agent_call(
            world,
            "editor.menu_invoke",
            serde_json::json!({ "path": "Edit/Paste" }),
        );
        agent_call(
            world,
            "editor.menu_invoke",
            serde_json::json!({ "path": "Edit/Paste" }),
        );
        assert_eq!(capture_ids(world), [3, 5, 7, 8, 9, 10, 11, 12]);
        bakes(world);

        // Undo the last paste and redo it: the copy comes back with the same id.
        assert!(crate::command::undo_world(world));
        assert_eq!(capture_ids(world), [3, 5, 7, 8, 9, 10, 11]);
        assert!(crate::command::redo_world(world));
        assert_eq!(capture_ids(world), [3, 5, 7, 8, 9, 10, 11, 12]);
        std::fs::remove_dir_all(&root).ok();
    }
}
