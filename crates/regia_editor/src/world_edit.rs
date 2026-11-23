// SPDX-License-Identifier: MPL-2.0
//! Editor-side world streaming (P-241, ADR-7802, `book/agents/world-streaming-editor.md`): load and
//! unload the cells of a streamed level in the edit world, and save a cell's edits back to the
//! entity files of that cell.
//!
//! The runtime streams cooked cells (`docs/specs/78-world-streaming.md`); the editor edits authored
//! entities. This module joins the two:
//!
//! - **The manifest** is the level's cooked world index (`.wstream`, read with
//!   `regia_world::format::WorldIndex`): the grid, the bounds and one record per non-empty cell.
//!   [`WorldEdit::open`] reads it from the project's cooked cache. A cell whose blob file is missing
//!   is marked with the runtime's reason (`cell_missing`) and cannot be loaded.
//! - **A cell's members** are the guids of its cooked blob (`CellBlob::from_bytes`, which checks the
//!   record's hash as the runtime does: a blob the runtime would refuse is refused here with the same
//!   reason, `cell_corrupt`, `cell_decode` or `cell_format_newer`).
//! - **The edit source** is the level's directory form (`<name>.level.d`, spec 37): one file per
//!   entity. Loading a cell reads its members' entity files and spawns them; saving a cell writes only
//!   the files of its members whose captured record changed, so the files of every other cell keep
//!   their bytes.
//!
//! Loading runs on a [`WorkerJob`] (`world_load:<n>`, cancellable with `editor.cancel_job`); its end
//! is applied on the editor thread by [`tick`]. A cancel that arrives before that apply discards
//! everything the worker read: the edit world is unchanged. Unloading a cell with unsaved edits is
//! refused (`world_unsaved`) until the caller says `save: true` or `save: false`.
//!
//! Entities whose guid is already in the edit world (the level opened in full through File ▸ Open
//! level) are left alone: the cell does not adopt them, so unloading it never removes entities the
//! open level owns. Unloading clears the undo history, as opening a level does: its commands
//! address entities that are gone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia::world::format::{CellBlob, WorldIndex, cell_blob_id, world_index_id};
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_scene::components::EntityGuid;
use regia_scene::format::{SceneEntity, SceneFile};
use regia_scene::level::LevelId;
use regia_scene::level_dir::{
    LevelForm, entity_file_text, entity_rel_path, level_form, level_paths, parse_entity_file,
};
use regia_scene::spawn::{
    SnapshotFilter, SpawnOptions, UnknownTypePolicyOption, snapshot_scene, spawn_scene,
};
use regia_scene::stream_grid::{CellCoord, StreamGrid};
use serde_json::{Value, json};

use crate::editor_jobs::{WorkerContext, WorkerJob};
use crate::engine::ecs::prelude::*;
use crate::viewport_agent::Handler;

/// Most cells one `editor.world_load` or "Load around camera" may ask for.
pub const MAX_LOAD_CELLS: usize = 4096;

/// A refused world action: a stable lowercase `code` (listed in the book page) and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldEditError {
    /// The stable reason (`world_not_open`, `world_unsaved`, `cell_missing`, ...).
    pub code: &'static str,
    /// What happened, for people.
    pub message: String,
    /// The cells the error is about, when it names any.
    pub cells: Vec<CellCoord>,
}

impl WorldEditError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            cells: Vec::new(),
        }
    }

    fn at(mut self, cells: Vec<CellCoord>) -> Self {
        self.cells = cells;
        self
    }

    /// The agent error: `data.reason` is [`Self::code`], `data.cells` the cell labels.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let mut err = RpcError::new(
            INVALID_PARAMS,
            "F0002",
            format!("[{}] {}", self.code, self.message),
            "see book/agents/world-streaming-editor.md",
        );
        if let Some(Value::Object(data)) = err.data.as_mut() {
            data.insert("reason".into(), json!(self.code));
            data.insert("cells".into(), json!(labels(&self.cells)));
        }
        err
    }
}

impl std::fmt::Display for WorldEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

fn labels(cells: &[CellCoord]) -> Vec<String> {
    cells.iter().map(CellCoord::label).collect()
}

/// Whether a cell can be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellStatus {
    /// Its blob is present (its hash is checked when it loads).
    Ready,
    /// The runtime would refuse it, with this reason (`cell_missing`, `cell_corrupt`, ...).
    Error(&'static str),
}

/// One cell of the manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldCell {
    /// Entities the cook put in it.
    pub entity_count: u32,
    /// Lowest point of its content, meters.
    pub min_y: f64,
    /// Highest point of its content, meters (the map shades by it).
    pub max_y: f64,
    /// Whether the terrain collides under it.
    pub has_terrain_collision: bool,
    /// Whether it can be loaded.
    pub status: CellStatus,
    /// The blob's hash from the record.
    blob_hash: [u8; 32],
}

/// A cell spawned into the edit world.
#[derive(Debug, Clone, Default)]
pub struct LoadedCell {
    /// Spawned members: guid → (entity, canonical entity-file text captured right after the spawn).
    members: BTreeMap<Guid, (Entity, String)>,
    /// Members left alone because the edit world already had their guid.
    pub skipped: Vec<Guid>,
}

impl LoadedCell {
    /// The spawned members' entities as spawned, in guid order (a play session may give them new
    /// ids; the model itself finds members by guid).
    #[must_use]
    pub fn entities(&self) -> Vec<Entity> {
        self.members.values().map(|(entity, _)| *entity).collect()
    }
}

/// The streamed level the World Map edits.
#[derive(Debug, Clone)]
pub struct OpenWorld {
    /// The level's directory form (`<name>.level.d`).
    pub dir: PathBuf,
    /// The cooked cache the manifest was read from.
    pub cache_dir: PathBuf,
    /// The level.
    pub level: LevelId,
    /// The grid of the cooked settings.
    pub grid: StreamGrid,
    /// Lowest and highest cell, inclusive.
    pub bounds: [CellCoord; 2],
    /// Every non-empty cell of the manifest.
    pub cells: BTreeMap<CellCoord, WorldCell>,
    /// Cells spawned into the edit world.
    pub loaded: BTreeMap<CellCoord, LoadedCell>,
}

/// The World Map's model: the open world, the running load and the last outcome. A world resource,
/// so the panel and the agent see the same state.
#[derive(Resource, Debug, Default)]
pub struct WorldEdit {
    /// The open world, if any.
    pub world: Option<OpenWorld>,
    /// The running load job.
    job: Option<WorldLoadJob>,
    /// The last load's outcome, for the panel's status line.
    pub last: Option<String>,
}

/// A running load: its worker and the cells it asked for.
#[derive(Debug)]
struct WorldLoadJob {
    worker: WorkerJob<Result<Vec<ReadCell>, WorldEditError>>,
    cells: Vec<CellCoord>,
}

/// What the worker read for one cell.
#[derive(Debug, Clone)]
struct ReadCell {
    cell: CellCoord,
    entities: Vec<SceneEntity>,
}

/// The editor-only marker of a spawned cell member (never saved: it is not reflected).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldCellMember(pub CellCoord);

fn cache_file(cache_dir: &Path, id: regia_asset::AssetId) -> PathBuf {
    cache_dir.join(regia_asset::cooked_file_name(id))
}

impl OpenWorld {
    /// Reads the manifest of the streamed level at `path` (either spelling of a level whose
    /// directory form exists) from `cache_dir`.
    ///
    /// # Errors
    ///
    /// `world_needs_level_dir` (the level is a single `.level.ron`: split it with `regia level
    /// split`), `level_not_found`, `world_not_cooked` (no index in the cache: cook the project),
    /// `world_index_corrupt` / `world_index_decode` / `world_format_newer` (the index did not read),
    /// `stream_settings_invalid` (its settings do not validate).
    pub fn open(path: &Path, cache_dir: &Path) -> Result<Self, WorldEditError> {
        let _span = tracing::info_span!("world_edit_open").entered();
        let (_, dir) = level_paths(path);
        match level_form(path) {
            Ok(LevelForm::Dir) => {}
            Ok(LevelForm::File) => {
                return Err(WorldEditError::new(
                    "world_needs_level_dir",
                    format!(
                        "{} is a single-file level; cells save per entity file, so split it first (regia level split)",
                        path.display()
                    ),
                ));
            }
            Err(e) => return Err(WorldEditError::new("level_not_found", e.to_string())),
        }
        let level = regia_framework::level_assets::level_id_of(&dir);
        let index_path = cache_file(cache_dir, world_index_id(level));
        let bytes = std::fs::read(&index_path).map_err(|e| {
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                "world_not_cooked"
            } else {
                "world_index_corrupt"
            };
            WorldEditError::new(code, format!("{}: {e}", index_path.display()))
        })?;
        let index = WorldIndex::from_bytes(&bytes, level).map_err(|e| {
            let code = match e {
                regia::world::format::WorldFormatError::Corrupt(_) => "world_index_corrupt",
                regia::world::format::WorldFormatError::Newer { .. } => "world_format_newer",
                regia::world::format::WorldFormatError::Decode(_) => "world_index_decode",
            };
            WorldEditError::new(code, e.to_string())
        })?;
        index
            .settings
            .validate()
            .map_err(|e| WorldEditError::new("stream_settings_invalid", e))?;
        let grid = index
            .settings
            .grid()
            .map_err(|e| WorldEditError::new("stream_settings_invalid", e.to_string()))?;
        let cells = index
            .cells
            .iter()
            .map(|record| {
                // The map marks a cell whose file is gone; the hash is checked when it loads.
                let status = match std::fs::metadata(cache_file(cache_dir, record.blob)) {
                    Ok(meta) if meta.is_file() => CellStatus::Ready,
                    Ok(_) => CellStatus::Error("cell_corrupt"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => CellStatus::Error("cell_missing"),
                    Err(_) => CellStatus::Error("cell_corrupt"),
                };
                (
                    record.coord,
                    WorldCell {
                        entity_count: record.entity_count,
                        min_y: record.min_y,
                        max_y: record.max_y,
                        has_terrain_collision: record.has_terrain_collision,
                        status,
                        blob_hash: record.blob_hash,
                    },
                )
            })
            .collect();
        Ok(Self {
            dir,
            cache_dir: cache_dir.to_path_buf(),
            level,
            grid,
            bounds: index.bounds,
            cells,
            loaded: BTreeMap::new(),
        })
    }

    /// The manifest cells within `radius_m` of the ground point `(x, z)`, in cell order.
    ///
    /// # Errors
    ///
    /// `world_radius_invalid` for a negative or non-finite radius or one covering more than
    /// [`MAX_LOAD_CELLS`] cells.
    pub fn cells_around(&self, x: f64, z: f64, radius_m: f64) -> Result<Vec<CellCoord>, WorldEditError> {
        self.grid
            .cells_within(x, z, radius_m, MAX_LOAD_CELLS as u64)
            .map(|cells| cells.into_iter().filter(|c| self.cells.contains_key(c)).collect())
            .map_err(|e| WorldEditError::new("world_radius_invalid", e.to_string()))
    }
}

/// Reads one cell for the load job: its blob (checked as the runtime checks it) and its members'
/// entity files. Off the editor thread.
fn read_cell(
    dir: &Path,
    cache_dir: &Path,
    level: LevelId,
    cell: CellCoord,
    hash: &[u8; 32],
    cx: &WorkerContext,
) -> Result<Option<ReadCell>, WorldEditError> {
    let id = cell_blob_id(level, cell);
    let bytes = std::fs::read(cache_file(cache_dir, id)).map_err(|e| {
        let code = if e.kind() == std::io::ErrorKind::NotFound {
            "cell_missing"
        } else {
            "cell_corrupt"
        };
        WorldEditError::new(code, format!("cell {cell}: {e}")).at(vec![cell])
    })?;
    let blob = CellBlob::from_bytes(&bytes, id, hash)
        .map_err(|e| WorldEditError::new(e.cell_reason(), format!("cell {cell}: {e}")).at(vec![cell]))?;
    let mut entities = Vec::with_capacity(blob.scene.entities.len());
    for member in &blob.scene.entities {
        if cx.cancelled() {
            return Ok(None);
        }
        let path = dir.join(entity_rel_path(member.guid));
        let text = std::fs::read_to_string(&path).map_err(|e| {
            WorldEditError::new(
                "world_entity_file_missing",
                format!("cell {cell}: entity {}: {}: {e}", member.guid, path.display()),
            )
            .at(vec![cell])
        })?;
        let file = parse_entity_file(&path, &text).map_err(|e| {
            WorldEditError::new("world_entity_file_invalid", format!("cell {cell}: {e}")).at(vec![cell])
        })?;
        entities.push(file.entity);
    }
    Ok(Some(ReadCell { cell, entities }))
}

/// Whether `cells` names a cell twice, is empty or is too long.
fn check_cells(cells: &[CellCoord]) -> Result<(), WorldEditError> {
    if cells.is_empty() {
        return Err(WorldEditError::new("world_cells_empty", "no cells were given"));
    }
    if cells.len() > MAX_LOAD_CELLS {
        return Err(WorldEditError::new(
            "world_too_many_cells",
            format!("{} cells; at most {MAX_LOAD_CELLS} at once", cells.len()),
        ));
    }
    Ok(())
}

impl WorldEdit {
    /// Opens the streamed level at `path` with the manifest in `cache_dir`. The cells of a world
    /// that was open before must be unloaded first (`world_cells_loaded`).
    ///
    /// # Errors
    ///
    /// As [`OpenWorld::open`]; `world_cells_loaded`, `world_load_running`.
    pub fn open(&mut self, path: &Path, cache_dir: &Path) -> Result<(), WorldEditError> {
        if self.job.is_some() {
            return Err(WorldEditError::new(
                "world_load_running",
                "a load is running; cancel it first",
            ));
        }
        if let Some(open) = &self.world
            && !open.loaded.is_empty()
        {
            return Err(WorldEditError::new(
                "world_cells_loaded",
                "unload the loaded cells of the open world first",
            )
            .at(open.loaded.keys().copied().collect()));
        }
        self.world = Some(OpenWorld::open(path, cache_dir)?);
        self.last = None;
        Ok(())
    }

    /// The running load's job id.
    #[must_use]
    pub fn job_id(&self) -> Option<&str> {
        self.job.as_ref().map(|job| job.worker.id.as_str())
    }

    /// The cells the running load asked for.
    #[must_use]
    pub fn job_cells(&self) -> &[CellCoord] {
        self.job.as_ref().map_or(&[], |job| job.cells.as_slice())
    }

    /// Asks the running load to stop; [`tick`] then ends it `cancelled` with nothing spawned.
    pub fn cancel_load(&self) {
        if let Some(job) = &self.job {
            job.worker.cancel();
        }
    }
}

fn open_world(world: &World) -> Result<&OpenWorld, WorldEditError> {
    world
        .get_resource::<WorldEdit>()
        .and_then(|edit| edit.world.as_ref())
        .ok_or_else(|| WorldEditError::new("world_not_open", "no world is open in the World Map"))
}

/// Starts a load of `cells` as job `world_load:<n>` and answers its id. Cells already loaded are
/// skipped; a cell the runtime would refuse refuses the whole load.
///
/// # Errors
///
/// `world_not_open`, `world_cells_empty`, `world_too_many_cells`, `world_cell_unknown` (not in the
/// manifest), `world_cell_error` (marked with a runtime reason), `world_load_running`,
/// `job_start_failed`.
pub fn start_load(world: &mut World, cells: &[CellCoord]) -> Result<String, WorldEditError> {
    check_cells(cells)?;
    let open = open_world(world)?;
    if world.resource::<WorldEdit>().job.is_some() {
        return Err(WorldEditError::new(
            "world_load_running",
            "a load is running; cancel it or wait",
        ));
    }
    let unknown: Vec<CellCoord> = cells
        .iter()
        .copied()
        .filter(|c| !open.cells.contains_key(c))
        .collect();
    if !unknown.is_empty() {
        return Err(WorldEditError::new("world_cell_unknown", "the manifest has no such cell").at(unknown));
    }
    let failed: Vec<CellCoord> = cells
        .iter()
        .copied()
        .filter(|c| matches!(open.cells[c].status, CellStatus::Error(_)))
        .collect();
    if let Some(first) = failed.first() {
        let reason = match open.cells[first].status {
            CellStatus::Error(reason) => reason,
            CellStatus::Ready => "cell_corrupt",
        };
        return Err(WorldEditError::new(
            "world_cell_error",
            format!("cell {first}: the runtime would refuse it ({reason})"),
        )
        .at(failed));
    }
    let wanted: BTreeSet<CellCoord> = cells
        .iter()
        .copied()
        .filter(|c| !open.loaded.contains_key(c))
        .collect();
    let plan: Vec<(CellCoord, [u8; 32])> = wanted.iter().map(|c| (*c, open.cells[c].blob_hash)).collect();
    let (dir, cache_dir, level) = (open.dir.clone(), open.cache_dir.clone(), open.level);
    let label = format!("Load {} world cells", plan.len());
    let worker = WorkerJob::spawn(
        world,
        "world_load",
        "world_load",
        label,
        "regia-world-load",
        move |cx| {
            let total = plan.len().max(1) as f64;
            let mut read = Vec::with_capacity(plan.len());
            for (i, (cell, hash)) in plan.iter().enumerate() {
                if cx.cancelled() {
                    return Ok(Vec::new());
                }
                cx.progress(Some(i as f64 / total), &format!("cell {cell}"));
                match read_cell(&dir, &cache_dir, level, *cell, hash, &cx)? {
                    Some(cell) => read.push(cell),
                    None => return Ok(Vec::new()),
                }
            }
            Ok(read)
        },
    )
    .map_err(|e| WorldEditError::new("job_start_failed", e))?;
    let id = worker.id.clone();
    let mut edit = world.resource_mut::<WorldEdit>();
    edit.job = Some(WorldLoadJob {
        worker,
        cells: wanted.into_iter().collect(),
    });
    edit.last = None;
    Ok(id)
}

/// Waits for the running load's worker (tests and `wait: true` callers); [`tick`] then applies it.
pub fn wait_load(world: &mut World) {
    if let Some(job) = world
        .get_resource_mut::<WorldEdit>()
        .and_then(|mut e| e.job.take())
    {
        let mut job = job;
        job.worker.wait();
        world.resource_mut::<WorldEdit>().job = Some(job);
    }
}

/// Applies an ended load on the editor thread: spawns the cells it read, or, when a cancel was
/// asked for at any time before this apply, nothing at all. A failed read marks its cell with the
/// reason and spawns nothing. Runs every frame from `editor_jobs::tick` and from the panel.
pub fn tick(world: &mut World) {
    let Some(mut edit) = world.get_resource_mut::<WorldEdit>() else {
        return;
    };
    let Some(job) = edit.job.as_mut() else {
        return;
    };
    let Some(outcome) = job.worker.take() else {
        let fraction = job.worker.context().fraction();
        let message = job.worker.context().message();
        let Some(job) = edit.job.take() else {
            return;
        };
        job.worker.sync_row(world, &message, fraction);
        world.resource_mut::<WorldEdit>().job = Some(job);
        return;
    };
    let Some(job) = edit.job.take() else {
        return;
    };
    let _span = tracing::info_span!("world_edit_apply", cells = job.cells.len()).entered();
    if job.worker.context().cancelled() {
        job.worker.end(world, Ok((json!({ "cells": [] }), true)));
        world.resource_mut::<WorldEdit>().last = Some("Load canceled: nothing changed".into());
        return;
    }
    match outcome {
        Err(panic) => {
            job.worker.end(world, Err(("world_load_failed", panic.clone())));
            world.resource_mut::<WorldEdit>().last = Some(panic);
        }
        Ok(Err(error)) => {
            if let Some(open) = world.resource_mut::<WorldEdit>().world.as_mut() {
                for cell in &error.cells {
                    if error.code.starts_with("cell_")
                        && let Some(info) = open.cells.get_mut(cell)
                    {
                        info.status = CellStatus::Error(error.code);
                    }
                }
            }
            job.worker.end(world, Err((error.code, error.message.clone())));
            world.resource_mut::<WorldEdit>().last = Some(error.to_string());
        }
        Ok(Ok(read)) => {
            let result = apply_read(world, read);
            let failed = result["failed"].as_array().map_or(0, Vec::len);
            let mut summary = format!(
                "Loaded {} cells, {} entities",
                result["cells"].as_array().map_or(0, Vec::len),
                result["spawned"].as_u64().unwrap_or(0)
            );
            if failed > 0 {
                summary.push_str(&format!("; {failed} cells did not spawn (world_spawn_failed)"));
            }
            job.worker.end(world, Ok((result, false)));
            world.resource_mut::<WorldEdit>().last = Some(summary);
        }
    }
}

/// Spawns what the worker read and records each member's captured text.
fn apply_read(world: &mut World, read: Vec<ReadCell>) -> Value {
    let Ok(level) = open_world(world).map(|open| open.level) else {
        return json!({ "cells": [], "spawned": 0 });
    };
    let existing: BTreeSet<Guid> = world.query::<&EntityGuid>().iter(world).map(|g| g.0).collect();
    let mut spawned_total = 0_usize;
    let mut done = Vec::new();
    let mut failed = Vec::new();
    let mut loaded = Vec::new();
    for ReadCell { cell, entities } in read {
        let (keep, skip): (Vec<SceneEntity>, Vec<SceneEntity>) =
            entities.into_iter().partition(|e| !existing.contains(&e.guid));
        let skipped: Vec<Guid> = skip.iter().map(|e| e.guid).collect();
        let guids: BTreeSet<Guid> = keep.iter().map(|e| e.guid).collect();
        let mut file = SceneFile::new();
        file.entities = keep;
        let options = SpawnOptions {
            level: Some(level),
            unknown_types: UnknownTypePolicyOption::Preserve,
            ..Default::default()
        };
        if let Err(e) = spawn_scene(world, &file, &options) {
            tracing::warn!(target: "regia_editor", %cell, "world cell spawn: {e}");
            failed.push(
                json!({ "cell": cell.label(), "reason": "world_spawn_failed", "message": e.to_string() }),
            );
            continue;
        }
        let members: Vec<(Guid, Entity)> = world
            .query::<(Entity, &EntityGuid)>()
            .iter(world)
            .filter(|(_, g)| guids.contains(&g.0))
            .map(|(e, g)| (g.0, e))
            .collect();
        for (_, entity) in &members {
            world.entity_mut(*entity).insert(WorldCellMember(cell));
        }
        spawned_total += members.len();
        done.push(cell.label());
        loaded.push((cell, members, skipped));
    }
    // The captured text: a record as the editor would save it right after the spawn, so a save
    // writes only what an edit changed (a spawn may add defaulted components).
    let captured = capture_members(world, level);
    if let Some(open) = world.resource_mut::<WorldEdit>().world.as_mut() {
        for (cell, members, skipped) in loaded {
            let members = members
                .into_iter()
                .map(|(guid, entity)| {
                    let text = captured.get(&guid).cloned().unwrap_or_default();
                    (guid, (entity, text))
                })
                .collect();
            open.loaded.insert(cell, LoadedCell { members, skipped });
        }
    }
    json!({ "cells": done, "spawned": spawned_total, "failed": failed })
}

/// The canonical entity-file text of every entity of `level` in the edit world, by guid.
fn capture_members(world: &mut World, level: LevelId) -> BTreeMap<Guid, String> {
    let filter = SnapshotFilter {
        level: Some(level),
        resources: Vec::new(),
    };
    match snapshot_scene(world, &filter) {
        Ok(scene) => scene
            .entities
            .iter()
            .filter_map(|e| entity_file_text(e).ok().map(|text| (e.guid, text)))
            .collect(),
        Err(e) => {
            tracing::warn!(target: "regia_editor", "world cell capture: {e}");
            BTreeMap::new()
        }
    }
}

/// What a save of one cell would do: the members whose record changed (with the new text) and the
/// members deleted in the editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellChanges {
    /// Guid → the entity file's new text.
    pub changed: BTreeMap<Guid, String>,
    /// Members no longer in the edit world.
    pub deleted: Vec<Guid>,
}

impl CellChanges {
    /// Whether the cell has nothing to save.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.deleted.is_empty()
    }
}

/// A loaded member: its guid, its entity as spawned and the text captured after the spawn.
type Member = (Guid, Entity, String);

/// The unsaved edits of each loaded cell in `cells` (all loaded cells when `None`).
#[must_use]
pub fn unsaved(world: &mut World, cells: Option<&[CellCoord]>) -> BTreeMap<CellCoord, CellChanges> {
    let Ok(open) = open_world(world) else {
        return BTreeMap::new();
    };
    let level = open.level;
    let wanted: Vec<(CellCoord, Vec<Member>)> = open
        .loaded
        .iter()
        .filter(|(c, _)| cells.is_none_or(|cells| cells.contains(c)))
        .map(|(c, loaded)| {
            (
                *c,
                loaded
                    .members
                    .iter()
                    .map(|(g, (e, text))| (*g, *e, text.clone()))
                    .collect(),
            )
        })
        .collect();
    let now = capture_members(world, level);
    wanted
        .into_iter()
        .map(|(cell, members)| {
            let mut changes = CellChanges::default();
            // By guid, not by entity id: a play session restores the edit world with new ids.
            for (guid, _, before) in members {
                match now.get(&guid) {
                    Some(text) if *text != before => {
                        changes.changed.insert(guid, text.clone());
                    }
                    Some(_) => {}
                    None => changes.deleted.push(guid),
                }
            }
            (cell, changes)
        })
        .collect()
}

/// What an unload did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnloadReport {
    /// Cells unloaded.
    pub cells: Vec<CellCoord>,
    /// Entity files written (relative to the level directory).
    pub written: Vec<PathBuf>,
    /// Entity files removed (members deleted in the editor).
    pub deleted: Vec<PathBuf>,
    /// Members whose edits were dropped (`save: false`).
    pub discarded: usize,
    /// Entities despawned.
    pub despawned: usize,
}

/// Unloads `cells`. With unsaved edits, `save: None` refuses (`world_unsaved`, nothing changes),
/// `Some(true)` writes the changed members' entity files first (atomically; deleted members' files
/// are removed), `Some(false)` drops the edits.
///
/// # Errors
///
/// `world_not_open`, `world_cells_empty`, `world_load_running`, `world_cell_not_loaded`,
/// `world_unsaved`, `world_save_failed` (no cell was unloaded).
pub fn unload(
    world: &mut World,
    cells: &[CellCoord],
    save: Option<bool>,
) -> Result<UnloadReport, WorldEditError> {
    check_cells(cells)?;
    let open = open_world(world)?;
    if world.resource::<WorldEdit>().job.is_some() {
        return Err(WorldEditError::new(
            "world_load_running",
            "a load is running; cancel it or wait",
        ));
    }
    let not_loaded: Vec<CellCoord> = cells
        .iter()
        .copied()
        .filter(|c| !open.loaded.contains_key(c))
        .collect();
    if !not_loaded.is_empty() {
        return Err(WorldEditError::new("world_cell_not_loaded", "the cell is not loaded").at(not_loaded));
    }
    let dir = open.dir.clone();
    let changes = unsaved(world, Some(cells));
    let dirty: Vec<CellCoord> = changes
        .iter()
        .filter(|(_, c)| !c.is_empty())
        .map(|(cell, _)| *cell)
        .collect();
    let mut report = UnloadReport::default();
    match save {
        None if !dirty.is_empty() => {
            return Err(WorldEditError::new(
                "world_unsaved",
                "the cell has unsaved edits: save (save: true) or discard them (save: false)",
            )
            .at(dirty));
        }
        Some(true) => {
            let _span = tracing::info_span!("world_edit_save", cells = dirty.len()).entered();
            for change in changes.values() {
                for (guid, text) in &change.changed {
                    let rel = entity_rel_path(*guid);
                    crate::atomic_write::write_atomic(&dir.join(&rel), text.as_bytes())
                        .map_err(|e| WorldEditError::new("world_save_failed", e).at(dirty.clone()))?;
                    report.written.push(rel);
                }
                for guid in &change.deleted {
                    let rel = entity_rel_path(*guid);
                    match std::fs::remove_file(dir.join(&rel)) {
                        Ok(()) => report.deleted.push(rel),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(WorldEditError::new(
                                "world_save_failed",
                                format!("{}: {e}", rel.display()),
                            )
                            .at(dirty.clone()));
                        }
                    }
                }
            }
        }
        Some(false) => {
            report.discarded = changes.values().map(|c| c.changed.len() + c.deleted.len()).sum();
        }
        None => {}
    }
    let _span = tracing::info_span!("world_edit_unload", cells = cells.len()).entered();
    let mut gone = BTreeSet::new();
    let mut level = None;
    if let Some(open) = world.resource_mut::<WorldEdit>().world.as_mut() {
        level = Some(open.level);
        for cell in cells {
            if let Some(loaded) = open.loaded.remove(cell) {
                gone.extend(loaded.members.into_keys());
                report.cells.push(*cell);
            }
        }
    }
    // By guid among the streamed level's members (entity ids change across a play session).
    let gone: Vec<Entity> = world
        .query::<(Entity, &EntityGuid, &regia_scene::level::LevelMember)>()
        .iter(world)
        .filter(|(_, guid, member)| gone.contains(&guid.0) && Some(member.level()) == level)
        .map(|(entity, _, _)| entity)
        .collect();
    for entity in gone {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
            report.despawned += 1;
        }
    }
    if let Some(mut selection) = world.get_resource_mut::<crate::selection::Selection>() {
        selection.clear();
    }
    // Undo commands address entities that are gone (their ids may be reused).
    if world.contains_resource::<crate::command::UndoStack>() {
        world.insert_resource(crate::command::UndoStack::new());
    }
    Ok(report)
}

/// The edit-world camera's ground position (the eye's X and Z), when the editor has a camera.
#[must_use]
pub fn camera_ground(world: &World) -> Option<[f64; 2]> {
    world
        .get_resource::<crate::viewport::EditorCamera>()
        .map(|camera| {
            let eye = camera.eye();
            [f64::from(eye.x), f64::from(eye.z)]
        })
}

// ---------------------------------------------------------------------------------------------
// Agent surface: editor.world_open, editor.world_cells, editor.world_load, editor.world_unload
// ---------------------------------------------------------------------------------------------

/// The state the agent and the panel report.
#[must_use]
pub fn state_json(world: &mut World) -> Value {
    let changes = unsaved(world, None);
    let camera = camera_ground(world);
    let Some(edit) = world.get_resource::<WorldEdit>() else {
        return json!({ "open": false });
    };
    let job = edit
        .job_id()
        .map(|id| json!({ "id": id, "cells": labels(edit.job_cells()) }));
    let Some(open) = &edit.world else {
        return json!({ "open": false, "job": job, "last": edit.last });
    };
    let cells: Vec<Value> = open
        .cells
        .iter()
        .map(|(coord, cell)| {
            let (status, reason) = match cell.status {
                CellStatus::Ready => ("ready", None),
                CellStatus::Error(reason) => ("error", Some(reason)),
            };
            let loaded = open.loaded.get(coord);
            let dirty = changes
                .get(coord)
                .map_or(0, |c| c.changed.len() + c.deleted.len());
            json!({
                "cell": coord.label(),
                "x": coord.x,
                "z": coord.z,
                "entities": cell.entity_count,
                "min_y": cell.min_y,
                "max_y": cell.max_y,
                "terrain": cell.has_terrain_collision,
                "status": status,
                "reason": reason,
                "loaded": loaded.is_some(),
                "spawned": loaded.map_or(0, |l| l.members.len()),
                "skipped": loaded.map_or(0, |l| l.skipped.len()),
                "unsaved": dirty,
            })
        })
        .collect();
    let camera = camera
        .map(|[x, z]| json!({ "x": x, "z": z, "cell": open.grid.cell_of(x, z).ok().map(|c| c.label()) }));
    json!({
        "open": true,
        "path": open.dir.display().to_string(),
        "level": open.level.0.to_string(),
        "cell_size_m": open.grid.cell_size(),
        "bounds": [open.bounds[0].label(), open.bounds[1].label()],
        "cells": cells,
        "loaded": labels(&open.loaded.keys().copied().collect::<Vec<_>>()),
        "camera": camera,
        "job": job,
        "last": edit.last,
    })
}

/// Parses `params[key]`: an array of `"<x>_<z>"` labels.
fn cells_param(params: &Value, key: &str) -> Result<Option<Vec<CellCoord>>, RpcError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(RpcError::wrong_type(
            key,
            "an array of cell labels such as \"0_-1\"",
        ));
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .and_then(|s| CellCoord::from_label(s).ok())
                .ok_or_else(|| RpcError::wrong_type(key, "an array of cell labels such as \"0_-1\""))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// The cooked cache of the editor's project for the host platform.
fn default_cache_dir(world: &World) -> Option<PathBuf> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())?;
    Some(regia_asset::cooked_cache_dir(
        &root,
        regia_asset_pipeline::host_platform(),
    ))
}

/// The streamed level a panel field or `editor.world_open { path }` names, under the editor's
/// document-path rule (relative under the project, absolute only under it or the temp directory,
/// never `..` or a symlink leading out): `path_outside_project` otherwise (review
/// sprint-w12-rev-g7 #11), so a later save cannot write entity files outside the project.
///
/// # Errors
///
/// `path_outside_project` with the reason.
pub fn world_path(world: &World, path: &str) -> Result<PathBuf, WorldEditError> {
    crate::agent_ext::document_request_path(world, "path", path)
        .map_err(|e| WorldEditError::new("path_outside_project", e.message))
}

/// Resolves the cache-directory parameter against the project root.
fn project_path(world: &World, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return path;
    }
    match world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
    {
        Some(root) => root.join(path),
        None => path,
    }
}

/// Opens `path` (the cache defaults to the project's) and answers the state.
///
/// # Errors
///
/// As [`WorldEdit::open`]; `world_no_cache` when there is no project and no `cache_dir`.
pub fn open_path(world: &mut World, path: &Path, cache_dir: Option<&Path>) -> Result<(), WorldEditError> {
    let cache = match cache_dir {
        Some(cache) => cache.to_path_buf(),
        None => default_cache_dir(world).ok_or_else(|| {
            WorldEditError::new(
                "world_no_cache",
                "no project is open; pass cache_dir (the cooked cache)",
            )
        })?,
    };
    world
        .get_resource_or_insert_with::<WorldEdit>(Default::default)
        .open(path, &cache)
}

/// `editor.world_open { path, cache_dir? }`.
fn world_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (the streamed level, .level.d) is required"))?;
    let path = world_path(world, path).map_err(|e| e.rpc())?;
    let cache = params
        .get("cache_dir")
        .and_then(Value::as_str)
        .map(|c| project_path(world, c));
    open_path(world, &path, cache.as_deref()).map_err(|e| e.rpc())?;
    Ok(state_json(world))
}

/// `editor.world_cells`.
fn world_cells(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    tick(world);
    Ok(state_json(world))
}

/// `editor.world_load { cells | radius_m, wait? }`.
fn world_load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick(world);
    let cells = match (cells_param(params, "cells")?, params.get("radius_m")) {
        (Some(cells), None) => cells,
        (None, Some(radius)) => {
            let radius = radius
                .as_f64()
                .ok_or_else(|| RpcError::wrong_type("radius_m", "a number of meters"))?;
            let [x, z] = camera_ground(world).ok_or_else(|| {
                WorldEditError::new("world_no_camera", "the editor has no camera to load around").rpc()
            })?;
            let open = open_world(world).map_err(|e| e.rpc())?;
            let cells = open.cells_around(x, z, radius).map_err(|e| e.rpc())?;
            if cells.is_empty() {
                return Err(WorldEditError::new(
                    "world_cells_empty",
                    "no cell of the manifest is within that radius of the camera",
                )
                .rpc());
            }
            cells
        }
        _ => {
            return Err(RpcError::invalid_params(
                "pass exactly one of `cells` or `radius_m`",
            ));
        }
    };
    let id = start_load(world, &cells).map_err(|e| e.rpc())?;
    if params.get("wait").and_then(Value::as_bool) == Some(true) {
        wait_load(world);
        tick(world);
    }
    let mut state = state_json(world);
    state["load_job"] = json!(id);
    Ok(state)
}

/// `editor.world_unload { cells, save? }`.
fn world_unload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick(world);
    let cells =
        cells_param(params, "cells")?.ok_or_else(|| RpcError::invalid_params("`cells` is required"))?;
    let save = match params.get("save") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(RpcError::wrong_type("save", "true, false or null")),
    };
    let report = unload(world, &cells, save).map_err(|e| e.rpc())?;
    let path = |p: &PathBuf| p.to_string_lossy().replace('\\', "/");
    let mut state = state_json(world);
    state["unloaded"] = json!(labels(&report.cells));
    state["written"] = json!(report.written.iter().map(path).collect::<Vec<_>>());
    state["deleted"] = json!(report.deleted.iter().map(path).collect::<Vec<_>>());
    state["discarded"] = json!(report.discarded);
    state["despawned"] = json!(report.despawned);
    Ok(state)
}

const OPEN_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "path",
        kind: "string",
        required: true,
        description: "The streamed level in its directory form (`.level.d`), absolute or under the project.",
    },
    ParamSchema {
        name: "cache_dir",
        kind: "string",
        required: false,
        description: "The cooked cache holding the level's world index. Default: the project's cache for the host platform.",
    },
];

const LOAD_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "cells",
        kind: "array",
        required: false,
        description: "Cell labels (`\"<x>_<z>\"`). Exactly one of `cells` and `radius_m`.",
    },
    ParamSchema {
        name: "radius_m",
        kind: "number",
        required: false,
        description: "Load every manifest cell within this many meters of the editor camera.",
    },
    ParamSchema {
        name: "wait",
        kind: "boolean",
        required: false,
        description: "Wait for the load and apply it before answering (default false: poll editor.world_cells).",
    },
];

const UNLOAD_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "cells",
        kind: "array",
        required: true,
        description: "Cell labels (`\"<x>_<z>\"`) of loaded cells.",
    },
    ParamSchema {
        name: "save",
        kind: "boolean",
        required: false,
        description: "With unsaved edits: true saves them to the cells' entity files, false discards them; absent or null refuses with world_unsaved.",
    },
];

const STATE: &str = "{ open, path, level, cell_size_m, bounds, cells: [{ cell, x, z, entities, min_y, max_y, \
    terrain, status: ready|error, reason, loaded, spawned, skipped, unsaved }], loaded, camera: { x, z, cell }, \
    job: { id, cells }, last }";

/// The World Map's agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    // A named constructor (not a closure), so `cargo xtask book-coverage` reads each row's method
    // name from its first argument (ADR-7381).
    fn row(
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
    vec![
        (
            row(
                "editor.world_open",
                "world_open",
                "Open a streamed level in the World Map: its cooked world index is the manifest (grid, bounds, \
                 cells); a cell whose blob file is missing is marked with the runtime's reason.",
                OPEN_PARAMS,
                STATE,
                true,
            ),
            world_open as Handler,
        ),
        (
            row(
                "editor.world_cells",
                "world_cells",
                "The World Map: the open world's cells (status, reason, loaded, unsaved edits), the editor \
                 camera's ground position and cell, and the running load job.",
                &[],
                STATE,
                false,
            ),
            world_cells as Handler,
        ),
        (
            row(
                "editor.world_load",
                "world_load",
                "Load cells into the edit world as a cancellable job (world_load:<n>; editor.cancel_job stops \
                 it and nothing spawns). Their entities appear in the Outliner.",
                LOAD_PARAMS,
                "the editor.world_cells state plus load_job",
                true,
            ),
            world_load as Handler,
        ),
        (
            row(
                "editor.world_unload",
                "world_unload",
                "Unload loaded cells. With unsaved edits: save true writes only the changed entity files of \
                 those cells, save false discards; absent refuses with world_unsaved.",
                UNLOAD_PARAMS,
                "the editor.world_cells state plus unloaded, written, deleted, discarded, despawned",
                true,
            ),
            world_unload as Handler,
        ),
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use regia::world::format::{CELL_BLOB_FORMAT_VERSION, CellRecord, WORLD_INDEX_FORMAT_VERSION};
    use regia_app::prelude::App;
    use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedEntity, CookedScene};
    use regia_scene::streaming::LevelStreaming;

    /// A 3x3 streamed fixture: `world.level.d` with one entity per cell (at the cell center, 100 m
    /// cells) and a cooked cache with the world index and nine cell blobs.
    pub(crate) struct Fixture {
        pub root: PathBuf,
        pub dir: PathBuf,
        pub cache: PathBuf,
        pub level: LevelId,
    }

    /// The BLAKE3 hash of `bytes` (the record's `blob_hash`), through the asset tools' hex form.
    fn hash_of(bytes: &[u8]) -> [u8; 32] {
        let hex = regia_asset::content_hash_hex(bytes);
        let mut out = [0_u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or_default();
        }
        out
    }

    pub(crate) fn guid_of(cell: CellCoord) -> Guid {
        Guid::from_u128(0xc0de_0000 + ((cell.x as u128) << 8) + cell.z as u128 + 1)
    }

    pub(crate) fn fixture(name: &str) -> Fixture {
        let root = regia_test_utils::unique_temp_dir(name);
        let dir = root.join("world.level.d");
        let mut entities = String::new();
        for x in 0..3_i64 {
            for z in 0..3_i64 {
                let cell = CellCoord::new(x, z);
                entities.push_str(&format!(
                    r#"(guid: "{}", name: "Prop {}", components: {{
                        "regia_scene::transform::Transform": (translation: ({}.0, 0.0, {}.0)),
                    }}),"#,
                    guid_of(cell),
                    cell.label(),
                    x * 100 + 50,
                    z * 100 + 50,
                ));
            }
        }
        let scene = SceneFile::from_ron_str(&format!("(format_version: 1, entities: [{entities}])"))
            .expect("fixture scene");
        regia_scene::level_dir::write_level_dir(&dir, &scene).expect("write level dir");
        let level = regia_framework::level_assets::level_id_of(&dir);
        let cache = root.join("cache");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let mut records = Vec::new();
        for x in 0..3_i64 {
            for z in 0..3_i64 {
                let coord = CellCoord::new(x, z);
                let id = cell_blob_id(level, coord);
                let blob = CellBlob {
                    format_version: CELL_BLOB_FORMAT_VERSION,
                    coord,
                    anchor: [0; 3],
                    scene: CookedScene {
                        format_version: COOKED_SCENE_VERSION,
                        strings: vec![],
                        entities: vec![CookedEntity {
                            guid: guid_of(coord),
                            name: None,
                            parent: None,
                            components: vec![],
                            prefab: None,
                        }],
                        resources: vec![],
                    },
                    nav: None,
                };
                let data = blob.to_bytes(id, vec![]).expect("cell blob");
                std::fs::write(cache_file(&cache, id), &data).expect("write blob");
                records.push(CellRecord {
                    coord,
                    blob: id,
                    blob_hash: hash_of(&data),
                    entity_count: 1,
                    own_bytes: data.len() as u64,
                    dep_bytes: 0,
                    vram_bytes: 0,
                    min_y: 0.0,
                    max_y: (x + z) as f64 * 10.0,
                    priority: 0,
                    deps: vec![],
                    nav_tiles: 0,
                    has_terrain_collision: false,
                });
            }
        }
        let index = WorldIndex {
            format_version: WORLD_INDEX_FORMAT_VERSION,
            level: level.0,
            content_hash: WorldIndex::hash_cells(&records),
            cells: records,
            bounds: [CellCoord::new(0, 0), CellCoord::new(2, 2)],
            settings: LevelStreaming {
                enabled: true,
                cell_size_m: 100.0,
                load_radius_m: 150.0,
                unload_radius_m: 200.0,
                ..Default::default()
            },
            hlod: None,
            replicated: vec![],
            nav: None,
        };
        std::fs::write(
            cache_file(&cache, world_index_id(level)),
            index.to_bytes().expect("index bytes"),
        )
        .expect("write index");
        Fixture {
            root,
            dir,
            cache,
            level,
        }
    }

    pub(crate) fn editor_app() -> App {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(crate::command::UndoStack::new());
        app.insert_resource(crate::selection::Selection::default());
        app
    }

    pub(crate) fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(schema, _)| schema.name == method)
            .unwrap_or_else(|| panic!("{method} is a world_edit method"));
        handler(world, &params)
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// Every file under `dir` with its bytes.
    fn tree_bytes(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).expect("read dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).expect("read file"));
                }
            }
        }
        out
    }

    fn entity_of(world: &mut World, guid: Guid) -> Option<Entity> {
        world
            .query::<(Entity, &EntityGuid)>()
            .iter(world)
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
    }

    fn open(world: &mut World, fx: &Fixture) -> Value {
        call(
            world,
            "editor.world_open",
            json!({ "path": fx.dir.display().to_string(), "cache_dir": fx.cache.display().to_string() }),
        )
        .expect("open")
    }

    /// Acceptance: load two of nine cells, edit an entity in one, unload it with save: only that
    /// entity's file changed; the state marks the loaded cell; unload without a choice refuses.
    #[test]
    fn load_two_cells_edit_one_and_unload_with_save_changes_only_that_cells_file() {
        let fx = fixture("world-edit-acceptance");
        let mut app = editor_app();
        let world = app.world_mut();
        let state = open(world, &fx);
        assert_eq!(state["cells"].as_array().map(Vec::len), Some(9));
        assert_eq!(state["cell_size_m"], 100.0);
        let before = tree_bytes(&fx.dir);

        let state = call(
            world,
            "editor.world_load",
            json!({ "cells": ["0_0", "1_0"], "wait": true }),
        )
        .expect("load");
        assert_eq!(state["loaded"], json!(["0_0", "1_0"]));
        assert!(state["job"].is_null(), "the load ended: {state}");
        let a = entity_of(world, guid_of(CellCoord::new(0, 0))).expect("0_0 member spawned");
        let b = entity_of(world, guid_of(CellCoord::new(1, 0))).expect("1_0 member spawned");
        assert!(
            entity_of(world, guid_of(CellCoord::new(2, 0))).is_none(),
            "2_0 not loaded"
        );
        assert_eq!(
            world.get::<WorldCellMember>(b),
            Some(&WorldCellMember(CellCoord::new(1, 0)))
        );
        assert_eq!(
            world
                .get::<regia_scene::components::Name>(a)
                .map(|n| n.as_str().to_string()),
            Some("Prop 0_0".to_string()),
            "the Outliner shows loaded members by name"
        );
        let state = call(world, "editor.world_cells", json!({})).expect("cells");
        let row = |state: &Value, label: &str| {
            state["cells"]
                .as_array()
                .and_then(|cells| cells.iter().find(|c| c["cell"] == label).cloned())
                .unwrap_or_else(|| panic!("row {label}"))
        };
        assert_eq!(row(&state, "1_0")["loaded"], true);
        assert_eq!(
            row(&state, "1_0")["unsaved"],
            0,
            "a fresh load has nothing to save"
        );
        assert_eq!(row(&state, "2_2")["loaded"], false);

        // Edit the 1_0 member.
        world
            .get_mut::<regia_scene::Transform>(b)
            .expect("transform")
            .translation
            .y = 7.0;
        let state = call(world, "editor.world_cells", json!({})).expect("cells");
        assert_eq!(row(&state, "1_0")["unsaved"], 1);
        assert_eq!(row(&state, "0_0")["unsaved"], 0);

        // No choice: refused, nothing changes.
        let err = call(world, "editor.world_unload", json!({ "cells": ["1_0"] })).expect_err("unsaved");
        assert_eq!(reason(&err), "world_unsaved");
        assert!(entity_of(world, guid_of(CellCoord::new(1, 0))).is_some());
        assert_eq!(tree_bytes(&fx.dir), before);

        let state = call(
            world,
            "editor.world_unload",
            json!({ "cells": ["1_0"], "save": true }),
        )
        .expect("unload");
        let rel = entity_rel_path(guid_of(CellCoord::new(1, 0)));
        assert_eq!(
            state["written"],
            json!([rel.to_string_lossy().replace('\\', "/")])
        );
        assert_eq!(
            state["loaded"],
            json!(["0_0"]),
            "the map marks only 0_0 loaded now"
        );
        assert!(
            entity_of(world, guid_of(CellCoord::new(1, 0))).is_none(),
            "1_0 despawned"
        );
        assert!(
            entity_of(world, guid_of(CellCoord::new(0, 0))).is_some(),
            "0_0 stays"
        );

        let after = tree_bytes(&fx.dir);
        let changed: Vec<&PathBuf> = before
            .iter()
            .filter(|(path, bytes)| after.get(*path) != Some(bytes))
            .map(|(path, _)| path)
            .collect();
        assert_eq!(
            changed,
            vec![&fx.dir.join(&rel)],
            "only the edited cell's entity file changed"
        );
        let text = std::fs::read_to_string(fx.dir.join(&rel)).expect("saved file");
        let saved = parse_entity_file(&rel, &text).expect("valid entity file");
        assert!(
            saved
                .entity
                .components
                .values()
                .any(|v| v.get_ron().contains("7.0")),
            "the edit was saved: {text}"
        );
        let _ = std::fs::remove_dir_all(&fx.root);
    }

    /// A cancel before the editor applies the load leaves the edit world as it was, even when the
    /// worker already read everything.
    #[test]
    fn cancel_during_a_load_leaves_the_edit_world_unchanged() {
        let fx = fixture("world-edit-cancel");
        let mut app = editor_app();
        let world = app.world_mut();
        open(world, &fx);
        let count = |world: &mut World| world.query::<&EntityGuid>().iter(world).count();
        let before = count(world);
        let state = call(world, "editor.world_load", json!({ "cells": ["0_0", "2_2"] })).expect("start");
        let id = state["load_job"].as_str().expect("job id").to_string();
        assert!(id.starts_with("world_load:"), "{id}");
        crate::editor_jobs::cancel_job(world, &json!({ "id": id })).expect("cancel_job");
        wait_load(world);
        tick(world);
        assert_eq!(count(world), before, "nothing spawned");
        let state = call(world, "editor.world_cells", json!({})).expect("cells");
        assert_eq!(state["loaded"], json!([]));
        assert!(state["job"].is_null());
        let status = regia_agent::jobs::job_status(world, &json!({ "job": id }), &mut |_, _, _| {
            Err(RpcError::internal("no provider"))
        })
        .expect("job status");
        assert_eq!(status["state"], "cancelled", "{status}");
        let _ = std::fs::remove_dir_all(&fx.root);
    }

    /// A cell whose blob file is gone shows the runtime's reason and refuses to load; a damaged blob
    /// fails its load with `cell_corrupt`, marks the cell and spawns nothing.
    #[test]
    fn cells_the_runtime_would_refuse_show_as_errors_and_do_not_load() {
        let fx = fixture("world-edit-errors");
        std::fs::remove_file(cache_file(
            &fx.cache,
            cell_blob_id(fx.level, CellCoord::new(2, 1)),
        ))
        .expect("rm");
        let damaged = cache_file(&fx.cache, cell_blob_id(fx.level, CellCoord::new(1, 1)));
        let mut bytes = std::fs::read(&damaged).expect("blob");
        bytes.push(0);
        std::fs::write(&damaged, bytes).expect("damage");
        let mut app = editor_app();
        let world = app.world_mut();
        let state = open(world, &fx);
        let missing = state["cells"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["cell"] == "2_1"))
            .cloned()
            .expect("2_1");
        assert_eq!(missing["status"], "error");
        assert_eq!(missing["reason"], "cell_missing");
        let err = call(world, "editor.world_load", json!({ "cells": ["0_0", "2_1"] })).expect_err("refused");
        assert_eq!(reason(&err), "world_cell_error");
        assert_eq!(
            err.data.as_ref().map(|d| d["cells"].clone()),
            Some(json!(["2_1"]))
        );

        let count = |world: &mut World| world.query::<&EntityGuid>().iter(world).count();
        call(
            world,
            "editor.world_load",
            json!({ "cells": ["0_0", "1_1"], "wait": true }),
        )
        .expect("started");
        assert_eq!(count(world), 0, "a failed read spawns no cell");
        let state = call(world, "editor.world_cells", json!({})).expect("cells");
        let damaged = state["cells"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["cell"] == "1_1"))
            .cloned()
            .expect("1_1");
        assert_eq!(damaged["reason"], "cell_corrupt");
        assert!(
            state["last"].as_str().is_some_and(|s| s.contains("cell_corrupt")),
            "{state}"
        );
        let _ = std::fs::remove_dir_all(&fx.root);
    }

    /// Discard drops the edits without writing; load around the camera picks the cells in reach.
    #[test]
    fn discard_writes_nothing_and_load_around_camera_uses_the_radius() {
        let fx = fixture("world-edit-discard");
        let mut app = editor_app();
        let world = app.world_mut();
        world.insert_resource(crate::viewport::EditorCamera {
            target: regia_core::math::Vec3::new(150.0, 0.0, 150.0),
            distance: 0.0,
            ..Default::default()
        });
        open(world, &fx);
        let before = tree_bytes(&fx.dir);
        let state = call(
            world,
            "editor.world_load",
            json!({ "radius_m": 10.0, "wait": true }),
        )
        .expect("around");
        assert_eq!(
            state["loaded"],
            json!(["1_1"]),
            "the camera stands in the middle cell"
        );
        assert_eq!(state["camera"]["cell"], "1_1");
        let e = entity_of(world, guid_of(CellCoord::new(1, 1))).expect("member");
        world.entity_mut(e).despawn();
        let err =
            call(world, "editor.world_unload", json!({ "cells": ["1_1"] })).expect_err("a delete is an edit");
        assert_eq!(reason(&err), "world_unsaved");
        let state = call(
            world,
            "editor.world_unload",
            json!({ "cells": ["1_1"], "save": false }),
        )
        .expect("discard");
        assert_eq!(state["discarded"], 1);
        assert_eq!(tree_bytes(&fx.dir), before, "discard writes nothing");
        let err = call(world, "editor.world_unload", json!({ "cells": ["1_1"] })).expect_err("not loaded");
        assert_eq!(reason(&err), "world_cell_not_loaded");
        let _ = std::fs::remove_dir_all(&fx.root);
    }

    /// End to end through the installed agent extension (the dispatcher's method table): open,
    /// load as a job, cancel through `editor.cancel_job`, load again, unload with save.
    #[test]
    fn the_world_methods_answer_through_the_agent_extension() {
        let fx = fixture("world-edit-dispatch");
        let mut app = editor_app();
        let world = app.world_mut();
        crate::agent_ext::install(world);
        let dispatch = |world: &mut World, method: &str, params: Value| {
            let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
            let (schema, handler) = extension
                .method(method)
                .unwrap_or_else(|| panic!("{method} is registered on the extension"));
            assert_eq!(schema.name, method);
            handler(world, &params)
        };
        dispatch(
            world,
            "editor.world_open",
            json!({ "path": fx.dir.display().to_string(), "cache_dir": fx.cache.display().to_string() }),
        )
        .expect("open");
        let started = dispatch(world, "editor.world_load", json!({ "cells": ["2_0"] })).expect("load");
        let id = started["load_job"].as_str().expect("job").to_string();
        dispatch(world, "editor.cancel_job", json!({ "id": id })).expect("cancel");
        wait_load(world);
        let state = dispatch(world, "editor.world_cells", json!({})).expect("cells");
        assert_eq!(state["loaded"], json!([]), "canceled: {state}");
        let state = dispatch(
            world,
            "editor.world_load",
            json!({ "cells": ["2_0"], "wait": true }),
        )
        .expect("load");
        assert_eq!(state["loaded"], json!(["2_0"]));
        let state = dispatch(
            world,
            "editor.world_unload",
            json!({ "cells": ["2_0"], "save": true }),
        )
        .expect("unload");
        assert_eq!(
            state["written"],
            json!([]),
            "nothing was edited, nothing is written"
        );
        assert_eq!(state["despawned"], 1);
        let _ = std::fs::remove_dir_all(&fx.root);
    }

    /// A member respawned with a new entity id (as a play session's restore does) is the same
    /// member: not a delete, nothing to save, and unloading removes the new entity.
    #[test]
    fn a_member_with_a_new_entity_id_is_still_the_same_member() {
        let fx = fixture("world-edit-respawn");
        let mut app = editor_app();
        let world = app.world_mut();
        open(world, &fx);
        call(
            world,
            "editor.world_load",
            json!({ "cells": ["0_2"], "wait": true }),
        )
        .expect("load");
        let guid = guid_of(CellCoord::new(0, 2));
        let old = entity_of(world, guid).expect("member");
        let record = snapshot_scene(
            world,
            &SnapshotFilter {
                level: Some(fx.level),
                resources: Vec::new(),
            },
        )
        .expect("snapshot");
        world.entity_mut(old).despawn();
        let options = SpawnOptions {
            level: Some(fx.level),
            unknown_types: UnknownTypePolicyOption::Preserve,
            ..Default::default()
        };
        spawn_scene(world, &record, &options).expect("respawn");
        let new = entity_of(world, guid).expect("respawned");
        assert_ne!(new, old);
        let state = call(world, "editor.world_cells", json!({})).expect("cells");
        let row = state["cells"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["cell"] == "0_2"))
            .cloned()
            .expect("0_2");
        assert_eq!(row["unsaved"], 0, "same guid, same record: {row}");
        let state = call(world, "editor.world_unload", json!({ "cells": ["0_2"] })).expect("unload");
        assert_eq!(state["despawned"], 1);
        assert!(
            entity_of(world, guid).is_none(),
            "the respawned member is unloaded too"
        );
        let _ = std::fs::remove_dir_all(&fx.root);
    }

    /// Bad input answers stable codes: no world, a single-file level, an uncooked level, unknown
    /// cells, bad labels.
    #[test]
    fn refusals_have_stable_codes() {
        let fx = fixture("world-edit-refusals");
        let mut app = editor_app();
        let world = app.world_mut();
        let err = call(world, "editor.world_load", json!({ "cells": ["0_0"] })).expect_err("no world");
        assert_eq!(reason(&err), "world_not_open");
        let single = fx.root.join("single.level.ron");
        std::fs::write(&single, "(format_version: 1, entities: [])").expect("single");
        let err = call(
            world,
            "editor.world_open",
            json!({ "path": single.display().to_string(), "cache_dir": fx.cache.display().to_string() }),
        )
        .expect_err("single file");
        assert_eq!(reason(&err), "world_needs_level_dir");
        let other = fx.root.join("other.level.d");
        regia_scene::level_dir::write_level_dir(&other, &SceneFile::new()).expect("other dir");
        let err = call(
            world,
            "editor.world_open",
            json!({ "path": other.display().to_string(), "cache_dir": fx.cache.display().to_string() }),
        )
        .expect_err("not cooked");
        assert_eq!(reason(&err), "world_not_cooked");
        open(world, &fx);
        let err = call(world, "editor.world_load", json!({ "cells": ["5_5"] })).expect_err("unknown");
        assert_eq!(reason(&err), "world_cell_unknown");
        let err = call(world, "editor.world_load", json!({ "cells": ["nope"] })).expect_err("label");
        assert_eq!(err.code, INVALID_PARAMS);
        let err = call(world, "editor.world_load", json!({ "cells": [] })).expect_err("empty");
        assert_eq!(reason(&err), "world_cells_empty");
        let err = call(world, "editor.world_load", json!({})).expect_err("neither");
        assert_eq!(err.code, INVALID_PARAMS);
        let _ = std::fs::remove_dir_all(&fx.root);
    }
}
