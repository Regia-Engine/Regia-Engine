// SPDX-License-Identifier: MPL-2.0
//! Terrain ▸ Import source (ADR-8541): a count-based terrain source (`terrain.inspect_source`'s
//! JSON or PNG, with an optional `heightfield.json` cook mapping) imported onto the heights of a
//! terrain in the open level, with a preview before anything changes.
//!
//! The workflow has four steps, the same for the Terrain panel, the agent and scripts:
//!
//! 1. **Start** ([`start`], `editor.terrain_source_start`): the destination terrain's heights are
//!    snapshot on the editor thread, then a worker (the cancellable editor job
//!    `terrain_source:<n>`, [`crate::editor_jobs::WorkerJob`]) reads and decodes the source through
//!    the shared service ([`regia_agent::read_project_source`], the reader of
//!    `terrain.inspect_source` and `regia terrain source-inspect`), re-encodes it in the
//!    destination's 16-bit encoding and compares it with the snapshot. Nothing in the world or on
//!    disk changes.
//! 2. **Preview** ([`status`], `editor.terrain_source_status`): when the worker ends the import is
//!    *staged*: the source summary (bounds, datum, units, effective limits: the very value
//!    `terrain.inspect_source` answers), the destination mapping, the mapping comparison (grid,
//!    registration, height range, re-quantisation), the changed-sample summary and the undo
//!    disclosure.
//! 3. **Apply** ([`apply`], `editor.terrain_source_apply`): refused while the destination mapping
//!    differs and the matching conversion was not chosen (`terrain_source_mapping_mismatch`), when
//!    the source files changed since the preview (`terrain_source_stale`) or the terrain's heights
//!    did (`terrain_source_destination_changed`). An import of at most the undo limit changed
//!    samples is one undo step (the base heightmap import's behaviour); a larger one must be
//!    acknowledged (`terrain_source_undo_limit`), and before it applies the original heights are
//!    written as a 16-bit PNG backup under the project's state directory (the Terrain panel's
//!    Import heightmap restores it bit for bit), so the history it clears is never the only copy.
//! 4. **Cancel** ([`cancel`], `editor.terrain_source_cancel`): before apply, at any time. A running
//!    worker stops at its next check; a staged import is dropped. Nothing was written either way.
//!
//! An applied import is recorded (source parameters and conversions, by terrain) in
//! `<state>/terrain_sources.json`; **reimport** ([`reimport`], `editor.terrain_source_reimport`)
//! starts the same import again from the record, through the same preview and guards. A save
//! then re-cooks only the tiles the import changed (`crate::terrain_tool::save_terrain_edits`,
//! atomic writes), so no other cooked output is touched.
//!
//! Every transition is an agent event of kind [`EVENT_KIND`] (`{ "event": "<job> <state>" }`), so
//! `play.wait_until { events_contain }` waits on it without polling.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::Receiver;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use regia_asset::Heightfield;
use regia_terrain::{Terrain, TerrainEdits};
use serde_json::{Value, json};

use crate::command::UndoStack;
use crate::editor_jobs::{WorkerContext, WorkerJob};
use crate::terrain_tool::TerrainStrokeCommand;
use crate::viewport_agent::Handler;

/// The job kind (`terrain_source:<n>`).
pub const JOB_KIND: &str = "terrain_source";

/// The background-task row id.
const TASK: &str = "terrain_source_import";

/// The agent event kind every transition is recorded under.
pub const EVENT_KIND: &str = "terrain_source";

/// Distance (metres) within which a source's cell size and first-sample position count as the
/// destination's: the registration is then the destination's.
pub const REGISTRATION_TOLERANCE_M: f32 = 1e-3;

/// Rows between two cancel checks of the worker's encode and compare passes.
const CANCEL_ROWS: usize = 64;

const HINT: &str = "see book/agents/terrain-source.md";

/// An import refusal: a stable lowercase code and a message.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceImportError {
    /// The code (`terrain_source_*`, or a source reader code such as `height_io_decode`).
    pub code: String,
    /// What happened.
    pub message: String,
    /// Extra data for the agent (`needs`, `changed`, `undo_limit`, ...).
    pub data: Value,
}

impl SourceImportError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            data: Value::Null,
        }
    }

    fn with(mut self, data: Value) -> Self {
        self.data = data;
        self
    }

    fn from_rpc(error: &RpcError) -> Self {
        let code = error
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .filter(|c| {
                c.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            })
            .filter(|c| !c.is_empty())
            .unwrap_or("terrain_source_bad_params")
            .to_owned();
        Self {
            code,
            message: error.message.clone(),
            data: error.data.clone().unwrap_or(Value::Null),
        }
    }

    /// The agent error.
    #[must_use]
    pub fn to_rpc(&self) -> RpcError {
        let status = if self.code == "terrain_source_not_found" {
            NOT_FOUND
        } else {
            INVALID_PARAMS
        };
        let mut error = RpcError::new(status, &self.code, self.message.clone(), HINT);
        if let (Some(Value::Object(data)), Value::Object(extra)) = (error.data.as_mut(), &self.data) {
            for (k, v) in extra {
                if k != "code" {
                    data.insert(k.clone(), v.clone());
                }
            }
        }
        error
    }
}

impl std::fmt::Display for SourceImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Conversions an apply accepts when the source's mapping is not the destination's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Conversion {
    /// `registration`: place sample `(c, r)` on the destination's sample `(c, r)` although the
    /// source's cell size or first-sample position differ (the preview gives the shift).
    pub registration: bool,
    /// `clamp_heights`: clamp heights outside the destination's height range to its ends.
    pub clamp_heights: bool,
}

impl Conversion {
    /// Parses `["registration", "clamp_heights"]` (absent or null: none).
    ///
    /// # Errors
    ///
    /// `terrain_source_bad_params` for another value.
    pub fn from_json(value: Option<&Value>) -> Result<Self, SourceImportError> {
        let mut out = Self::default();
        let Some(value) = value.filter(|v| !v.is_null()) else {
            return Ok(out);
        };
        let items = value.as_array().ok_or_else(|| {
            SourceImportError::new(
                "terrain_source_bad_params",
                "`conversion` is a list of \"registration\" and \"clamp_heights\"",
            )
        })?;
        for item in items {
            match item.as_str() {
                Some("registration") => out.registration = true,
                Some("clamp_heights") => out.clamp_heights = true,
                _ => {
                    return Err(SourceImportError::new(
                        "terrain_source_bad_params",
                        format!("unknown conversion {item}; use \"registration\" or \"clamp_heights\""),
                    ));
                }
            }
        }
        Ok(out)
    }

    /// The JSON list.
    #[must_use]
    pub fn to_json(self) -> Value {
        let mut list = Vec::new();
        if self.registration {
            list.push("registration");
        }
        if self.clamp_heights {
            list.push("clamp_heights");
        }
        json!(list)
    }
}

/// The host's import policy. Hosts insert it after loading project policy; an absent resource
/// uses the defaults.
#[derive(Resource, Debug, Clone, Copy)]
pub struct TerrainSourceImportPolicy {
    /// Changed samples an apply keeps as one undo step; a larger import needs acknowledgement and
    /// writes a backup first. Never above `regia_terrain::MAX_IMPORT_UNDO_SAMPLES`.
    pub undo_limit_samples: usize,
}

impl Default for TerrainSourceImportPolicy {
    fn default() -> Self {
        Self {
            undo_limit_samples: regia_terrain::MAX_IMPORT_UNDO_SAMPLES,
        }
    }
}

impl TerrainSourceImportPolicy {
    fn effective_limit(world: &World) -> usize {
        world
            .get_resource::<Self>()
            .map_or(regia_terrain::MAX_IMPORT_UNDO_SAMPLES, |p| p.undo_limit_samples)
            .min(regia_terrain::MAX_IMPORT_UNDO_SAMPLES)
    }
}

/// A gate the next started worker waits on before it reads the source: tests and scripted hosts
/// hold the worker at a known point (a cancel before the read), then send `()` or drop the sender.
/// [`start`] takes it out of the world.
#[derive(Resource, Debug)]
pub struct TerrainSourceStartGate(pub Mutex<Option<Receiver<()>>>);

/// What the start captured of the destination terrain.
#[derive(Debug, Clone)]
struct Destination {
    entity: Entity,
    cols: u32,
    rows: u32,
    cell_m: f32,
    world_min: [f32; 2],
    world_max: [f32; 2],
    scale: f32,
    offset: f32,
    generation: u64,
}

impl Destination {
    fn to_json(&self) -> Value {
        json!({
            "entity": self.entity.to_bits(),
            "cols": self.cols, "rows": self.rows, "cell_m": self.cell_m,
            "sample0_m": self.world_min, "world_min": self.world_min, "world_max": self.world_max,
            "height_range_m": [self.offset, self.offset + self.scale],
            "generation": format!("{:016x}", self.generation),
        })
    }
}

/// A started (or reimported) import's request.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceImportRequest {
    /// The `terrain.inspect_source` parameters: `path`, optional `png`, `mapping`, `limits`.
    pub source: Value,
    /// The destination terrain (`None`: the selected terrain, else the lowest-id one).
    pub entity: Option<Entity>,
    /// Conversions chosen up front (an apply may add them).
    pub conversion: Conversion,
}

impl SourceImportRequest {
    /// The request an agent call carries: the source keys of `params`, `entity` and `conversion`.
    ///
    /// # Errors
    ///
    /// `terrain_source_bad_params` for a malformed field.
    pub fn from_params(params: &Value) -> Result<Self, SourceImportError> {
        let mut source = serde_json::Map::new();
        for key in ["path", "png", "mapping", "limits"] {
            if let Some(v) = params.get(key).filter(|v| !v.is_null()) {
                source.insert(key.to_owned(), v.clone());
            }
        }
        if !source.contains_key("path") {
            return Err(SourceImportError::new(
                "terrain_source_bad_params",
                "`path` is required",
            ));
        }
        let entity = match params.get("entity").filter(|v| !v.is_null()) {
            None => None,
            Some(v) => Some(v.as_u64().and_then(Entity::try_from_bits).ok_or_else(|| {
                SourceImportError::new("terrain_source_bad_params", format!("{v} is not an entity id"))
            })?),
        };
        Ok(Self {
            source: Value::Object(source),
            entity,
            conversion: Conversion::from_json(params.get("conversion"))?,
        })
    }
}

/// The worker's staged result.
#[derive(Debug)]
struct Staged {
    summary: Value,
    digest: String,
    heightfield: Heightfield,
    changes: Changes,
    mapping: MappingCheck,
}

#[derive(Debug, Clone, Copy, Default)]
struct Changes {
    changed: u64,
    rect: Option<[u32; 4]>,
    max_delta_m: f32,
    clamped_low: u64,
    clamped_high: u64,
}

#[derive(Debug, Clone, Copy)]
struct MappingCheck {
    registration_match: bool,
    shift_m: [f32; 2],
    cell_ratio: f32,
    requantized: bool,
}

/// How the worker ended.
enum WorkerEnd {
    Staged(Box<Staged>),
    Cancelled,
    Failed(SourceImportError),
}

/// Where an import is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportState {
    /// The worker reads, encodes and compares.
    Running,
    /// The preview is ready; nothing has changed yet.
    Staged,
    /// Applied to the terrain.
    Applied,
    /// Cancelled before apply: nothing changed.
    Cancelled,
    /// Failed before apply: nothing changed.
    Failed,
}

impl ImportState {
    /// `running`, `staged`, `applied`, `cancelled` or `failed`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Staged => "staged",
            Self::Applied => "applied",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// The current import (one at a time): its worker while it runs, its preview while staged.
#[derive(Resource)]
pub struct TerrainSourceImport {
    id: String,
    request: SourceImportRequest,
    destination: Destination,
    root: PathBuf,
    undo_limit: usize,
    job: Option<WorkerJob<WorkerEnd>>,
    staged: Option<Box<Staged>>,
}

impl std::fmt::Debug for TerrainSourceImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerrainSourceImport")
            .field("id", &self.id)
            .field("staged", &self.staged.is_some())
            .finish_non_exhaustive()
    }
}

impl TerrainSourceImport {
    /// The job id (`terrain_source:<n>`).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Running or staged.
    #[must_use]
    pub fn state(&self) -> ImportState {
        if self.staged.is_some() {
            ImportState::Staged
        } else {
            ImportState::Running
        }
    }
}

/// The last ended import's report (applied, cancelled or failed), for `editor.terrain_source_status`.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct TerrainSourceLast {
    /// The job id.
    pub id: String,
    /// How it ended.
    pub state: ImportState,
    /// The report (`editor.terrain_source_status`'s answer).
    pub report: Value,
}

fn project_root(world: &World) -> Result<PathBuf, SourceImportError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| SourceImportError::new("terrain_source_no_project", "open a project first"))
}

fn destination_entity(world: &mut World, entity: Option<Entity>) -> Result<Entity, SourceImportError> {
    match entity {
        Some(e) if world.get::<Terrain>(e).is_some() => Ok(e),
        Some(e) => Err(SourceImportError::new(
            "terrain_source_no_terrain",
            format!("entity {} has no terrain", e.to_bits()),
        )),
        None => crate::terrain_panel::panel_terrain(world).ok_or_else(|| {
            SourceImportError::new(
                "terrain_source_no_terrain",
                "the open level has no terrain to import onto",
            )
        }),
    }
}

/// A 64-bit FNV-1a digest of the terrain's height bits (the destination's content generation).
fn heights_generation(heights: &[f32]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for h in heights {
        for byte in h.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn emit(world: &mut World, id: &str, state: ImportState) {
    let frame = world.get_resource::<Time>().map_or(0, |t| t.frame_count());
    if let Some(mut log) = world.get_resource_mut::<regia_agent::AgentEventLog>() {
        log.push_host(
            frame,
            EVENT_KIND,
            json!({ "event": format!("{id} {}", state.name()), "job": id, "state": state.name() }),
        );
    }
}

/// Starts an import as job `terrain_source:<n>` and answers its id. Nothing changes until
/// [`apply`].
///
/// # Errors
///
/// `terrain_source_no_project`, `terrain_source_no_terrain`, `terrain_source_busy` (an import is
/// running or staged: apply or cancel it), `terrain_source_failed` (the worker cannot start).
pub fn start(world: &mut World, request: SourceImportRequest) -> Result<String, SourceImportError> {
    if let Some(running) = world.get_resource::<TerrainSourceImport>() {
        return Err(SourceImportError::new(
            "terrain_source_busy",
            format!(
                "{} is {}; apply or cancel it first",
                running.id,
                running.state().name()
            ),
        ));
    }
    let root = project_root(world)?;
    let entity = destination_entity(world, request.entity)?;
    let terrain = world
        .get::<Terrain>(entity)
        .ok_or_else(|| SourceImportError::new("terrain_source_no_terrain", "the terrain is gone"))?;
    let map = &terrain.map;
    let destination = Destination {
        entity,
        cols: map.width(),
        rows: map.height(),
        cell_m: map.sample_spacing(),
        world_min: map.world_min(),
        world_max: map.world_max(),
        scale: map.height_scale(),
        offset: map.height_offset(),
        generation: heights_generation(map.heights()),
    };
    let snapshot = map.heights().to_vec();
    let policy = world
        .get_resource::<regia_agent::TerrainSourcePolicy>()
        .copied()
        .unwrap_or_default()
        .limits;
    let gate = world
        .remove_resource::<TerrainSourceStartGate>()
        .and_then(|g| g.0.into_inner().ok().flatten());
    let worker_root = root.clone();
    let worker_source = request.source.clone();
    let worker_destination = destination.clone();
    let path = request
        .source
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned();
    let job = WorkerJob::spawn(
        world,
        JOB_KIND,
        TASK,
        format!("importing terrain source {path}"),
        "regia-terrain-source",
        move |cx: WorkerContext| {
            if let Some(gate) = gate {
                cx.progress(Some(0.0), "waiting");
                let _ = gate.recv();
            }
            run_stage(
                &worker_root,
                policy,
                &worker_source,
                &worker_destination,
                &snapshot,
                &cx,
            )
        },
    )
    .map_err(|e| SourceImportError::new("terrain_source_failed", e))?;
    let id = job.id.clone();
    let undo_limit = TerrainSourceImportPolicy::effective_limit(world);
    world.insert_resource(TerrainSourceImport {
        id: id.clone(),
        request,
        destination,
        root,
        undo_limit,
        job: Some(job),
        staged: None,
    });
    emit(world, &id, ImportState::Running);
    Ok(id)
}

/// The worker: read and decode, encode in the destination's 16 bits, compare.
fn run_stage(
    root: &Path,
    policy: regia_terrain::source::ImportLimits,
    source: &Value,
    destination: &Destination,
    snapshot: &[f32],
    cx: &WorkerContext,
) -> WorkerEnd {
    let _span = tracing::info_span!(target: "regia_editor", "terrain_source.stage").entered();
    if cx.cancelled() {
        return WorkerEnd::Cancelled;
    }
    cx.progress(Some(0.05), "reading the source");
    // A source tile set (ADR-8595) reports each tile read and stops at a cancel between tiles.
    let mut observe = |decoded: u64, tiles: usize| {
        if cx.cancelled() {
            return std::ops::ControlFlow::Break(());
        }
        let fraction = 0.05 + 0.3 * (decoded as f64 / tiles.max(1) as f64).min(1.0);
        cx.progress(
            Some(fraction),
            &format!("reading source tile {} of {tiles}", decoded + 1),
        );
        std::ops::ControlFlow::Continue(())
    };
    let read = match regia_agent::read_project_source_observed(root, policy, source, &mut observe) {
        Ok(read) => read,
        Err(_) if cx.cancelled() => return WorkerEnd::Cancelled,
        Err(e) => return WorkerEnd::Failed(SourceImportError::from_rpc(&e)),
    };
    if cx.cancelled() {
        return WorkerEnd::Cancelled;
    }
    let src = &read.source;
    // A raster source with unequal spacing or missing samples (ADR-8542) cannot land on a regular
    // terrain grid without resampling or inventing heights: refuse it like the cook does.
    if let Err(e) = src.count_mapping() {
        return WorkerEnd::Failed(SourceImportError::new(e.code(), e.to_string()));
    }
    let [cols, rows] = src.dimensions();
    if [cols, rows] != [destination.cols, destination.rows] {
        return WorkerEnd::Failed(
            SourceImportError::new(
                "terrain_source_grid_mismatch",
                format!(
                    "the source is {cols}x{rows} samples and the terrain {}x{}; an import never resamples",
                    destination.cols, destination.rows
                ),
            )
            .with(json!({ "source": [cols, rows], "destination": [destination.cols, destination.rows] })),
        );
    }
    cx.progress(Some(0.4), "encoding");
    let heights = src.heights_m();
    let (scale, offset) = (destination.scale, destination.offset);
    let inv = if scale == 0.0 { 0.0 } else { 65535.0 / scale };
    let mut samples = Vec::with_capacity(heights.len());
    let mut changes = Changes::default();
    let width = cols as usize;
    for (row, chunk) in heights.chunks(width).enumerate() {
        if row.is_multiple_of(CANCEL_ROWS) && cx.cancelled() {
            return WorkerEnd::Cancelled;
        }
        for (col, &h) in chunk.iter().enumerate() {
            let raw = ((h - offset) * inv).round();
            changes.clamped_low += u64::from(raw < 0.0);
            changes.clamped_high += u64::from(raw > 65535.0);
            let s = raw.clamp(0.0, 65535.0) as u16;
            samples.push(s);
            // The same dequantisation `import_heights` applies, so the counts agree with apply.
            let after = f32::from(s) / 65535.0 * scale + offset;
            let i = row * width + col;
            let before = snapshot.get(i).copied().unwrap_or(f32::NAN);
            if before.to_bits() != after.to_bits() {
                changes.changed += 1;
                changes.max_delta_m = changes.max_delta_m.max((after - before).abs());
                let (c, r) = (col as u32, row as u32);
                changes.rect = Some(match changes.rect {
                    None => [c, c + 1, r, r + 1],
                    Some([c0, c1, r0, r1]) => [c0.min(c), c1.max(c + 1), r0.min(r), r1.max(r + 1)],
                });
            }
        }
        if row.is_multiple_of(CANCEL_ROWS) {
            cx.progress(Some(0.4 + 0.55 * row as f64 / rows.max(1) as f64), "comparing");
        }
    }
    if cx.cancelled() {
        return WorkerEnd::Cancelled;
    }
    let shift_m = [
        src.sample0()[0] - destination.world_min[0],
        src.sample0()[1] - destination.world_min[1],
    ];
    let registration_match = (src.cell_m() - destination.cell_m).abs() <= REGISTRATION_TOLERANCE_M
        && shift_m.iter().all(|d| d.abs() <= REGISTRATION_TOLERANCE_M);
    let mpc = src.meters_per_count();
    let requantized = (mpc * 65535.0).to_bits() != scale.to_bits()
        || (-f32::from(src.zero_counts()) * mpc).to_bits() != offset.to_bits();
    let mapping = MappingCheck {
        registration_match,
        shift_m,
        cell_ratio: if destination.cell_m == 0.0 {
            0.0
        } else {
            src.cell_m() / destination.cell_m
        },
        requantized,
    };
    cx.progress(Some(1.0), "staged");
    WorkerEnd::Staged(Box::new(Staged {
        summary: regia_agent::source_summary(src, read.limits),
        digest: read.digest,
        heightfield: Heightfield {
            width: cols,
            height: rows,
            samples,
        },
        changes,
        mapping,
    }))
}

impl Staged {
    fn needs(&self) -> Vec<&'static str> {
        let mut needs = Vec::new();
        if !self.mapping.registration_match {
            needs.push("registration");
        }
        if self.changes.clamped_low + self.changes.clamped_high > 0 {
            needs.push("clamp_heights");
        }
        needs
    }
}

/// The preview of a staged import (the agent's `preview`, the panel's text).
fn preview_json(import: &TerrainSourceImport, staged: &Staged) -> Value {
    let c = &staged.changes;
    let m = &staged.mapping;
    let total = u64::from(import.destination.cols) * u64::from(import.destination.rows);
    json!({
        "source": staged.summary,
        "digest": staged.digest,
        "destination": import.destination.to_json(),
        "mapping": {
            "grid_match": true,
            "registration_match": m.registration_match,
            "registration_shift_m": m.shift_m,
            "cell_ratio": m.cell_ratio,
            "heights_in_range": c.clamped_low + c.clamped_high == 0,
            "requantized": m.requantized,
            "needs": staged.needs(),
        },
        "changes": {
            "changed": c.changed,
            "total": total,
            "dirty": c.rect.map(|[c0, c1, r0, r1]| json!({ "col_start": c0, "col_end": c1, "row_start": r0, "row_end": r1 })),
            "max_delta_m": c.max_delta_m,
            "clamped_low": c.clamped_low,
            "clamped_high": c.clamped_high,
        },
        "undo": {
            "limit": import.undo_limit,
            "undoable": c.changed as usize <= import.undo_limit,
            "acknowledge_required": c.changed as usize > import.undo_limit,
            "backup": "a 16-bit PNG of the original heights under <state>/terrain_source_backups/",
        },
        "conversion": import.request.conversion.to_json(),
    })
}

/// Polls the running import (every frame, [`crate::editor_jobs::tick`]): a returned worker is
/// staged, cancelled or failed. Never waits.
pub fn tick_job(world: &mut World) {
    let Some(mut import) = world.remove_resource::<TerrainSourceImport>() else {
        return;
    };
    let Some(mut job) = import.job.take() else {
        world.insert_resource(import);
        return;
    };
    let Some(outcome) = job.take() else {
        let message = job.context().message();
        job.sync_row(world, &message, job.context().fraction());
        import.job = Some(job);
        world.insert_resource(import);
        return;
    };
    let id = import.id.clone();
    match outcome {
        Ok(WorkerEnd::Staged(staged)) if !job.context().cancelled() => {
            let preview = preview_json(&import, &staged);
            job.end(
                world,
                Ok((json!({ "state": "staged", "preview": preview }), false)),
            );
            import.staged = Some(staged);
            world.insert_resource(import);
            emit(world, &id, ImportState::Staged);
        }
        Ok(WorkerEnd::Staged(_) | WorkerEnd::Cancelled) => {
            job.end(
                world,
                Ok((json!({ "state": "cancelled", "changed_assets": false }), true)),
            );
            end(
                world,
                &id,
                ImportState::Cancelled,
                json!({ "changed_assets": false }),
            );
        }
        Ok(WorkerEnd::Failed(e)) => {
            job.end(world, Err((e.code.as_str(), e.message.clone())));
            end(
                world,
                &id,
                ImportState::Failed,
                json!({ "error": { "code": e.code, "message": e.message, "data": e.data }, "changed_assets": false }),
            );
        }
        Err(panic) => {
            job.end(world, Err(("terrain_source_failed", panic.clone())));
            end(
                world,
                &id,
                ImportState::Failed,
                json!({ "error": { "code": "terrain_source_failed", "message": panic }, "changed_assets": false }),
            );
        }
    }
}

fn end(world: &mut World, id: &str, state: ImportState, extra: Value) {
    let mut report = json!({ "job": id, "state": state.name() });
    if let (Value::Object(r), Value::Object(e)) = (&mut report, extra) {
        r.extend(e);
    }
    world.insert_resource(TerrainSourceLast {
        id: id.to_owned(),
        state,
        report,
    });
    emit(world, id, state);
}

/// Waits for the running worker to return without applying its end: the next [`tick_job`] (the
/// next frame) stages or ends it. Tests use it to make the frame the preview appears in
/// deterministic.
pub fn wait_worker(world: &mut World) {
    if let Some(mut import) = world.get_resource_mut::<TerrainSourceImport>()
        && let Some(job) = import.job.as_mut()
    {
        job.wait();
    }
}

/// Waits for a running worker and stages (or ends) it: for blocking callers and tests.
pub fn finish_now(world: &mut World) {
    if let Some(mut import) = world.get_resource_mut::<TerrainSourceImport>()
        && let Some(job) = import.job.as_mut()
    {
        job.wait();
    }
    tick_job(world);
}

/// The import's state and, once staged, its preview: the current import when `id` is `None` or
/// matches, else the last ended one.
///
/// # Errors
///
/// `terrain_source_not_found` for an id the editor has no record of.
pub fn status(world: &World, id: Option<&str>) -> Result<Value, SourceImportError> {
    if let Some(import) = world.get_resource::<TerrainSourceImport>()
        && id.is_none_or(|id| id == import.id)
    {
        let mut out = json!({ "job": import.id, "state": import.state().name() });
        if let Some(job) = &import.job {
            out["progress"] = json!({
                "fraction": job.context().fraction(),
                "message": job.context().message(),
            });
        }
        if let Some(staged) = &import.staged {
            out["preview"] = preview_json(import, staged);
        }
        return Ok(out);
    }
    if let Some(last) = world.get_resource::<TerrainSourceLast>()
        && id.is_none_or(|id| id == last.id)
    {
        return Ok(last.report.clone());
    }
    Err(SourceImportError::new(
        "terrain_source_not_found",
        match id {
            Some(id) => format!("no terrain source import {id}"),
            None => "no terrain source import has run".to_owned(),
        },
    ))
}

fn current_id_matches(world: &World, id: Option<&str>) -> Result<(), SourceImportError> {
    match world.get_resource::<TerrainSourceImport>() {
        Some(import) if id.is_none_or(|id| id == import.id) => Ok(()),
        _ => Err(SourceImportError::new(
            "terrain_source_not_found",
            format!(
                "no running or staged terrain source import{}",
                id.map(|id| format!(" {id}")).unwrap_or_default()
            ),
        )),
    }
}

/// Cancels the current import before apply: a running worker stops at its next check (the job
/// then ends `cancelled`), a staged import is dropped at once. Nothing was written either way.
///
/// # Errors
///
/// `terrain_source_not_found` without a running or staged import of that id.
pub fn cancel(world: &mut World, id: Option<&str>) -> Result<Value, SourceImportError> {
    current_id_matches(world, id)?;
    let Some(import) = world.remove_resource::<TerrainSourceImport>() else {
        return Err(SourceImportError::new("terrain_source_not_found", "no import"));
    };
    let job_id = import.id.clone();
    match import.job {
        Some(job) => {
            job.cancel();
            let state = ImportState::Running.name();
            world.insert_resource(TerrainSourceImport {
                job: Some(job),
                ..import
            });
            Ok(json!({ "job": job_id, "requested": true, "state": state, "changed_assets": false }))
        }
        None => {
            end(
                world,
                &job_id,
                ImportState::Cancelled,
                json!({ "changed_assets": false }),
            );
            Ok(json!({ "job": job_id, "requested": true, "state": "cancelled", "changed_assets": false }))
        }
    }
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedImport {
    /// The job id.
    pub job: String,
    /// Samples whose height changed.
    pub changed: u64,
    /// One undo step (at most the undo limit changed samples).
    pub undoable: bool,
    /// The original heights' backup (a large import), project-relative.
    pub backup: Option<String>,
}

impl AppliedImport {
    /// The agent's answer.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "job": self.job, "state": "applied", "changed": self.changed,
            "undoable": self.undoable, "backup": self.backup,
        })
    }
}

/// Applies the staged import (see the module docs for the guards).
///
/// # Errors
///
/// `terrain_source_not_found`, `terrain_source_not_ready` (the worker still runs),
/// `terrain_source_mapping_mismatch` (with `needs`), `terrain_source_stale`,
/// `terrain_source_destination_changed`, `terrain_source_undo_limit` (with `changed`,
/// `undo_limit`), `terrain_source_backup_failed`, `terrain_source_no_terrain`. The import stays
/// staged after a refusal (except `terrain_source_stale` and `terrain_source_destination_changed`,
/// which end it: start again for a fresh preview).
pub fn apply(
    world: &mut World,
    id: Option<&str>,
    conversion: Conversion,
    acknowledge_undo_limit: bool,
) -> Result<AppliedImport, SourceImportError> {
    current_id_matches(world, id)?;
    let Some(import) = world.remove_resource::<TerrainSourceImport>() else {
        return Err(SourceImportError::new("terrain_source_not_found", "no import"));
    };
    let result = apply_staged(world, &import, conversion, acknowledge_undo_limit);
    match &result {
        Ok(applied) => {
            record(world, &import, conversion);
            end(world, &import.id, ImportState::Applied, applied.to_json());
        }
        Err(e)
            if matches!(
                e.code.as_str(),
                "terrain_source_stale" | "terrain_source_destination_changed"
            ) =>
        {
            end(
                world,
                &import.id,
                ImportState::Failed,
                json!({ "error": { "code": e.code, "message": e.message }, "changed_assets": false }),
            );
        }
        Err(_) => world.insert_resource(import),
    }
    result
}

fn apply_staged(
    world: &mut World,
    import: &TerrainSourceImport,
    conversion: Conversion,
    acknowledge_undo_limit: bool,
) -> Result<AppliedImport, SourceImportError> {
    let _span = tracing::info_span!(target: "regia_editor", "terrain_source.apply").entered();
    let Some(staged) = import.staged.as_ref() else {
        return Err(SourceImportError::new(
            "terrain_source_not_ready",
            format!("{} is still running; wait for `staged`", import.id),
        ));
    };
    let chosen = Conversion {
        registration: conversion.registration || import.request.conversion.registration,
        clamp_heights: conversion.clamp_heights || import.request.conversion.clamp_heights,
    };
    let missing: Vec<&str> = staged
        .needs()
        .into_iter()
        .filter(|n| match *n {
            "registration" => !chosen.registration,
            _ => !chosen.clamp_heights,
        })
        .collect();
    if !missing.is_empty() {
        return Err(SourceImportError::new(
            "terrain_source_mapping_mismatch",
            format!(
                "the source's mapping is not the terrain's; choose the conversion {} to apply",
                missing.join(" and ")
            ),
        )
        .with(json!({ "needs": missing, "mapping_shift_m": staged.mapping.shift_m })));
    }
    // The source must be the bytes the preview read.
    let policy = world
        .get_resource::<regia_agent::TerrainSourcePolicy>()
        .copied()
        .unwrap_or_default()
        .limits;
    let now =
        regia_agent::source_inputs_digest(&import.root, policy, &import.request.source).map_err(|e| {
            SourceImportError::new(
                "terrain_source_stale",
                format!("the source cannot be read again: {}", e.message),
            )
        })?;
    if now != staged.digest {
        return Err(SourceImportError::new(
            "terrain_source_stale",
            "the source changed after the preview; start the import again",
        ));
    }
    let entity = import.destination.entity;
    let Some(terrain) = world.get::<Terrain>(entity) else {
        return Err(SourceImportError::new(
            "terrain_source_no_terrain",
            "the terrain is gone",
        ));
    };
    if heights_generation(terrain.map.heights()) != import.destination.generation
        || terrain.map.width() != import.destination.cols
        || terrain.map.height() != import.destination.rows
    {
        return Err(SourceImportError::new(
            "terrain_source_destination_changed",
            "the terrain's heights changed after the preview; start the import again",
        ));
    }
    let large = staged.changes.changed as usize > import.undo_limit;
    let mut backup = None;
    if large {
        if !acknowledge_undo_limit {
            return Err(SourceImportError::new(
                "terrain_source_undo_limit",
                format!(
                    "{} changed samples is over the undo limit of {}: the import would apply without an undo step \
                     and clear the undo history. Apply with acknowledge_undo_limit: true; the original heights are \
                     backed up first",
                    staged.changes.changed, import.undo_limit
                ),
            )
            .with(json!({ "changed": staged.changes.changed, "undo_limit": import.undo_limit })),
        );
        }
        let original = regia_terrain::export_heights(&terrain.map).heightfield;
        let bytes = crate::terrain_panel::encode_png16(&original)
            .map_err(|e| SourceImportError::new("terrain_source_backup_failed", e))?;
        let rel = format!(
            "terrain_source_backups/{}-{}.png",
            import.id.replace(':', "_"),
            entity.to_bits()
        );
        let path = regia_core::brand::project_state_dir(&import.root).join(&rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                SourceImportError::new("terrain_source_backup_failed", format!("{}: {e}", dir.display()))
            })?;
        }
        crate::level_io::write_atomic(&path, &bytes)
            .map_err(|e| SourceImportError::new("terrain_source_backup_failed", e))?;
        backup = Some(
            path.strip_prefix(&import.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/"),
        );
    }
    if !world.entity(entity).contains::<TerrainEdits>() {
        world.entity_mut(entity).insert(TerrainEdits::default());
    }
    let mut query = world.query::<(&mut Terrain, &mut TerrainEdits)>();
    let (mut terrain, mut edits) = query
        .get_mut(world, entity)
        .map_err(|_| SourceImportError::new("terrain_source_no_terrain", "the terrain is gone"))?;
    let applied = regia_terrain::height_io::import_heights_with_undo_cap(
        &mut terrain,
        &mut edits,
        &staged.heightfield,
        import.undo_limit,
    )
    .map_err(|e| SourceImportError::new("terrain_source_grid_mismatch", e.to_string()))?;
    let undoable = applied.undo.is_some() || applied.changed == 0;
    if let Some(undo) = applied.undo
        && applied.changed > 0
    {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(TerrainStrokeCommand::new(entity, undo)));
    } else if applied.changed > 0 {
        // As the heightmap import (ADR-5575): terrain entries of the old history would stamp
        // pre-import heights into the imported map. The backup above is the way back.
        let mut fresh = UndoStack::new();
        fresh.mark_dirty();
        world.insert_resource(fresh);
    }
    Ok(AppliedImport {
        job: import.id.clone(),
        changed: applied.changed,
        undoable,
        backup,
    })
}

// ---------------------------------------------------------------------------------------------
// Records and reimport
// ---------------------------------------------------------------------------------------------

/// The file applied imports are recorded in, under the project's state directory.
pub const RECORDS_FILE: &str = "terrain_sources.json";

fn records_path(root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(root).join(RECORDS_FILE)
}

/// The record key of a terrain: its level terrain's layers path, else `entity:<bits>`.
fn record_key(world: &World, entity: Entity) -> String {
    world
        .get::<regia_terrain::LevelTerrainSource>(entity)
        .and_then(|s| s.terrain.as_ref().map(|t| t.layers.clone()))
        .unwrap_or_else(|| format!("entity:{}", entity.to_bits()))
}

fn read_records(root: &Path) -> serde_json::Map<String, Value> {
    std::fs::read(records_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("terrains").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

fn record(world: &mut World, import: &TerrainSourceImport, conversion: Conversion) {
    let key = record_key(world, import.destination.entity);
    let mut records = read_records(&import.root);
    let chosen = Conversion {
        registration: conversion.registration || import.request.conversion.registration,
        clamp_heights: conversion.clamp_heights || import.request.conversion.clamp_heights,
    };
    records.insert(
        key,
        json!({ "source": import.request.source, "conversion": chosen.to_json() }),
    );
    let doc = json!({ "version": 1, "terrains": records });
    let path = records_path(&import.root);
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            let text = serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?;
            crate::level_io::write_atomic(&path, &text)
        });
    if let Err(e) = written {
        tracing::warn!(target: "regia_editor", "terrain source record {}: {e}", path.display());
    }
}

/// The recorded import of a terrain (`None` when none was applied).
#[must_use]
pub fn recorded(world: &mut World, entity: Entity) -> Option<SourceImportRequest> {
    let root = project_root(world).ok()?;
    let key = record_key(world, entity);
    let entry = read_records(&root).remove(&key)?;
    Some(SourceImportRequest {
        source: entry.get("source")?.clone(),
        entity: Some(entity),
        conversion: Conversion::from_json(entry.get("conversion")).ok()?,
    })
}

/// Starts the recorded import of a terrain again (its source, settings and conversions).
///
/// # Errors
///
/// `terrain_source_no_record` when the terrain has none, else [`start`]'s.
pub fn reimport(
    world: &mut World,
    entity: Option<Entity>,
) -> Result<(String, SourceImportRequest), SourceImportError> {
    let entity = destination_entity(world, entity)?;
    let request = recorded(world, entity).ok_or_else(|| {
        SourceImportError::new(
            "terrain_source_no_record",
            format!(
                "terrain {} has no applied source import to repeat",
                entity.to_bits()
            ),
        )
    })?;
    let id = start(world, request.clone())?;
    Ok((id, request))
}

// ---------------------------------------------------------------------------------------------
// The panel's text
// ---------------------------------------------------------------------------------------------

/// The preview as the Terrain panel shows it, one line each: the source's size, units and datum,
/// its bounds, the destination's, the mapping verdict, the changes and the undo disclosure.
#[must_use]
pub fn preview_lines(preview: &Value) -> Vec<String> {
    let s = &preview["source"];
    let d = &preview["destination"];
    let m = &preview["mapping"];
    let c = &preview["changes"];
    let u = &preview["undo"];
    let pair = |v: &Value| format!("({}, {})", v[0], v[1]);
    let mut lines = vec![
        format!(
            "source {}x{} samples, {} m per count, datum {} counts, cell {} m",
            s["width"], s["height"], s["meters_per_count"], s["zero_counts"], s["cell_m"]
        ),
        format!(
            "source bounds {} to {} m, heights {} to {} m",
            pair(&s["world_min"]),
            pair(&s["world_max"]),
            s["min_m"],
            s["max_m"]
        ),
        format!(
            "effective limits {} bytes, {} samples",
            s["effective_limits"]["max_bytes"], s["effective_limits"]["max_samples"]
        ),
        format!(
            "destination {}x{}, bounds {} to {} m, height range {} m",
            d["cols"],
            d["rows"],
            pair(&d["world_min"]),
            pair(&d["world_max"]),
            pair(&d["height_range_m"])
        ),
    ];
    let needs: Vec<&str> = m["needs"]
        .as_array()
        .map(|n| n.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    lines.push(if needs.is_empty() {
        "mapping matches the terrain".to_owned()
    } else {
        format!(
            "mapping differs (shift {} m): choose {} to apply",
            pair(&m["registration_shift_m"]),
            needs.join(" and ")
        )
    });
    lines.push(format!(
        "{} of {} samples change (largest {} m), {} clamped",
        c["changed"],
        c["total"],
        c["max_delta_m"],
        c["clamped_low"].as_u64().unwrap_or(0) + c["clamped_high"].as_u64().unwrap_or(0)
    ));
    lines.push(if u["undoable"].as_bool().unwrap_or(false) {
        format!("one undo step (limit {} samples)", u["limit"])
    } else {
        format!(
            "over the undo limit of {} samples: no undo step, the undo history is cleared, the original heights \
             are backed up first",
            u["limit"]
        )
    });
    lines
}

// ---------------------------------------------------------------------------------------------
// The panel section
// ---------------------------------------------------------------------------------------------

/// The Terrain panel's Import source fields.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct TerrainSourcePanel {
    /// The source, project-relative (`.json` or `.png`).
    pub path: String,
    /// An optional `heightfield.json` with a schema-2 count mapping.
    pub mapping: String,
    /// PNG settings as JSON (`{"meters_per_count": .., "cell_m": ..}`); empty for JSON sources.
    pub png: String,
    /// Conversion: place samples on the terrain's samples although the registration differs.
    pub registration: bool,
    /// Conversion: clamp heights outside the terrain's range.
    pub clamp_heights: bool,
    /// Acknowledged: an import over the undo limit clears the history (a backup is written).
    pub acknowledge_undo_limit: bool,
    /// The last result or error.
    pub message: String,
}

impl Default for TerrainSourcePanel {
    fn default() -> Self {
        Self {
            path: "assets/maps/source.json".into(),
            mapping: String::new(),
            png: String::new(),
            registration: false,
            clamp_heights: false,
            acknowledge_undo_limit: false,
            message: String::new(),
        }
    }
}

impl TerrainSourcePanel {
    /// The start request of the fields.
    ///
    /// # Errors
    ///
    /// `terrain_source_bad_params` when the PNG settings are not a JSON object.
    pub fn request(&self) -> Result<SourceImportRequest, SourceImportError> {
        let mut params = json!({ "path": self.path.trim() });
        if !self.mapping.trim().is_empty() {
            params["mapping"] = json!(self.mapping.trim());
        }
        if !self.png.trim().is_empty() {
            let png: Value = serde_json::from_str(&self.png)
                .ok()
                .filter(Value::is_object)
                .ok_or_else(|| {
                    SourceImportError::new("terrain_source_bad_params", "PNG settings must be a JSON object")
                })?;
            params["png"] = png;
        }
        let mut request = SourceImportRequest::from_params(&params)?;
        request.conversion = self.conversion();
        Ok(request)
    }

    /// The conversions ticked.
    #[must_use]
    pub fn conversion(&self) -> Conversion {
        Conversion {
            registration: self.registration,
            clamp_heights: self.clamp_heights,
        }
    }
}

/// The Terrain panel's Import source section: fields, Preview, the preview text, Apply / Cancel and
/// Reimport. Every button calls the function the agent method calls.
pub fn panel_ui(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world
        .get_resource::<TerrainSourcePanel>()
        .cloned()
        .unwrap_or_default();
    ui.heading("Import source");
    ui.label("Count-based heights with a datum and units (JSON schema 2 or 16-bit PNG).");
    ui.horizontal(|ui| {
        ui.label("Source");
        ui.text_edit_singleline(&mut panel.path);
    });
    ui.horizontal(|ui| {
        ui.label("Mapping");
        ui.text_edit_singleline(&mut panel.mapping);
    });
    ui.horizontal(|ui| {
        ui.label("PNG settings");
        ui.text_edit_singleline(&mut panel.png);
    });
    let current = world
        .get_resource::<TerrainSourceImport>()
        .map(|i| (i.id.clone(), i.state()));
    match &current {
        None => {
            ui.horizontal(|ui| {
                if ui.button("Preview").clicked() {
                    panel.message = match panel.request().and_then(|r| start(world, r)) {
                        Ok(id) => format!("{id}: reading the source"),
                        Err(e) => e.to_string(),
                    };
                }
                if ui.button("Reimport").clicked() {
                    panel.message = match reimport(world, None) {
                        Ok((id, _)) => format!("{id}: reimporting the recorded source"),
                        Err(e) => e.to_string(),
                    };
                }
            });
        }
        Some((id, ImportState::Running)) => {
            let progress = status(world, Some(id)).ok();
            let fraction = progress
                .as_ref()
                .and_then(|s| s["progress"]["fraction"].as_f64())
                .unwrap_or(0.0);
            ui.add(egui::ProgressBar::new(fraction as f32).text(id.as_str()));
            if ui.button("Cancel").clicked() {
                panel.message = match cancel(world, Some(id)) {
                    Ok(_) => format!("{id}: cancelling; nothing was changed"),
                    Err(e) => e.to_string(),
                };
            }
        }
        Some((id, _)) => {
            if let Ok(report) = status(world, Some(id)) {
                for line in preview_lines(&report["preview"]) {
                    ui.label(line);
                }
            }
            ui.checkbox(&mut panel.registration, "Use the terrain's sample positions");
            ui.checkbox(&mut panel.clamp_heights, "Clamp heights to the terrain's range");
            ui.checkbox(&mut panel.acknowledge_undo_limit, "Apply over the undo limit");
            ui.label("Over the limit: the original heights are backed up and the undo history is cleared.");
            ui.horizontal(|ui| {
                if ui.button("Apply").clicked() {
                    panel.message =
                        match apply(world, Some(id), panel.conversion(), panel.acknowledge_undo_limit) {
                            Ok(a) => match &a.backup {
                                Some(b) => {
                                    format!("applied: {} samples changed; original heights in {b}", a.changed)
                                }
                                None => format!("applied: {} samples changed (one undo step)", a.changed),
                            },
                            Err(e) => e.to_string(),
                        };
                }
                if ui.button("Cancel import").clicked() {
                    panel.message = match cancel(world, Some(id)) {
                        Ok(_) => "cancelled; nothing was changed".to_owned(),
                        Err(e) => e.to_string(),
                    };
                }
            });
        }
    }
    if !panel.message.is_empty() {
        ui.label(&panel.message);
    }
    world.insert_resource(panel);
}

// ---------------------------------------------------------------------------------------------
// Agent methods
// ---------------------------------------------------------------------------------------------

fn rpc<T>(result: Result<T, SourceImportError>) -> Result<T, RpcError> {
    result.map_err(|e| e.to_rpc())
}

fn job_param(params: &Value) -> Option<&str> {
    params
        .get("job")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn agent_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let request = rpc(SourceImportRequest::from_params(params))?;
    let id = rpc(start(world, request))?;
    Ok(json!({ "job": id, "state": "running" }))
}

fn agent_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick_job(world);
    rpc(status(world, job_param(params)))
}

fn agent_cancel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    rpc(cancel(world, job_param(params)))
}

fn agent_apply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    tick_job(world);
    let conversion = rpc(Conversion::from_json(params.get("conversion")))?;
    let ack = params
        .get("acknowledge_undo_limit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let applied = rpc(apply(world, job_param(params), conversion, ack))?;
    Ok(applied.to_json())
}

fn agent_reimport(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = match params.get("entity").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params(format!("{v} is not an entity id")))?,
        ),
    };
    let (id, request) = rpc(reimport(world, entity))?;
    Ok(
        json!({ "job": id, "state": "running", "source": request.source, "conversion": request.conversion.to_json() }),
    )
}

const SOURCE_PARAMS: [ParamSchema; 6] = [
    ParamSchema {
        name: "path",
        kind: "string",
        required: true,
        description: "Project-relative .json or .png count source (terrain.inspect_source).",
    },
    ParamSchema {
        name: "png",
        kind: "object",
        required: false,
        description: "PNG settings as terrain.inspect_source takes them.",
    },
    ParamSchema {
        name: "mapping",
        kind: "string",
        required: false,
        description: "Project-relative heightfield.json with a schema-2 count mapping.",
    },
    ParamSchema {
        name: "limits",
        kind: "object",
        required: false,
        description: "Lower import limits (never above the host policy).",
    },
    ParamSchema {
        name: "entity",
        kind: "integer",
        required: false,
        description: "Destination terrain entity (default: the selected terrain, else the lowest id).",
    },
    ParamSchema {
        name: "conversion",
        kind: "array",
        required: false,
        description: "Conversions chosen up front: \"registration\", \"clamp_heights\".",
    },
];

const JOB_PARAM: ParamSchema = ParamSchema {
    name: "job",
    kind: "string",
    required: false,
    description: "The job id (`terrain_source:<n>`); default the current import.",
};

/// The agent methods of the import workflow.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.terrain_source_start",
                tool: "terrain_source_start",
                description: "Start a count-source import onto a terrain of the open level (ADR-8541): a worker job \
                    `terrain_source:<n>` reads the source as terrain.inspect_source does, encodes it in the \
                    terrain's 16 bits and compares; nothing changes until editor.terrain_source_apply. Wait \
                    for the agent event `terrain_source` \"<job> staged\" (play.wait_until events_contain), then \
                    read the preview with editor.terrain_source_status. Errors terrain_source_bad_params, \
                    terrain_source_no_project, terrain_source_no_terrain, terrain_source_busy, \
                    terrain_source_failed.",
                params: &SOURCE_PARAMS,
                result: "{ job, state }",
                mutating: false,
                permission: Permission::Read,
            },
            agent_start as Handler,
        ),
        (
            MethodSchema {
                name: "editor.terrain_source_status",
                tool: "terrain_source_status",
                description: "State of a terrain source import: running (with progress), staged (with `preview`: \
                    source summary, destination, mapping comparison and `needs`, changed-sample summary, undo \
                    disclosure), applied, cancelled or failed (with `error`). Errors terrain_source_not_found.",
                params: &[JOB_PARAM],
                result: "{ job, state, progress?, preview?, error? }",
                mutating: false,
                permission: Permission::Read,
            },
            agent_status as Handler,
        ),
        (
            MethodSchema {
                name: "editor.terrain_source_cancel",
                tool: "terrain_source_cancel",
                description: "Cancel a terrain source import before apply: a running worker stops at its next check, \
                    a staged import is dropped. No asset or height changes. Errors terrain_source_not_found.",
                params: &[JOB_PARAM],
                result: "{ job, requested, state, changed_assets }",
                mutating: true,
                permission: Permission::Assets,
            },
            agent_cancel as Handler,
        ),
        (
            MethodSchema {
                name: "editor.terrain_source_apply",
                tool: "terrain_source_apply",
                description: "Apply a staged terrain source import to the terrain's heights. Refused with \
                    terrain_source_mapping_mismatch (data.needs) until the needed conversions are chosen, \
                    terrain_source_stale when the source changed after the preview, \
                    terrain_source_destination_changed when the heights did, terrain_source_undo_limit above the \
                    undo limit without acknowledge_undo_limit (then the original heights are backed up first and \
                    the undo history is cleared), terrain_source_not_ready, terrain_source_backup_failed, \
                    terrain_source_not_found. At most the undo limit changed samples is one undo step.",
                params: &[
                    JOB_PARAM,
                    ParamSchema {
                        name: "conversion",
                        kind: "array",
                        required: false,
                        description: "\"registration\" and/or \"clamp_heights\" (the preview's `needs`).",
                    },
                    ParamSchema {
                        name: "acknowledge_undo_limit",
                        kind: "boolean",
                        required: false,
                        description: "Apply an import over the undo limit (backup first, history cleared).",
                    },
                ],
                result: "{ job, state, changed, undoable, backup }",
                mutating: true,
                permission: Permission::EditScene,
            },
            agent_apply as Handler,
        ),
        (
            MethodSchema {
                name: "editor.terrain_source_reimport",
                tool: "terrain_source_reimport",
                description: "Start the recorded source import of a terrain again (its source parameters and \
                    conversions from the last apply), through the same preview and guards. Errors \
                    terrain_source_no_record, terrain_source_no_terrain, terrain_source_busy.",
                params: &[ParamSchema {
                    name: "entity",
                    kind: "integer",
                    required: false,
                    description: "The terrain (default: the selected terrain, else the lowest id).",
                }],
                result: "{ job, state, source, conversion }",
                mutating: false,
                permission: Permission::Read,
            },
            agent_reimport as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_lists_round_trip_and_refuse_unknown_entries() {
        let both = Conversion::from_json(Some(&json!(["registration", "clamp_heights"]))).expect("both");
        assert_eq!(
            both,
            Conversion {
                registration: true,
                clamp_heights: true
            }
        );
        assert_eq!(Conversion::from_json(Some(&both.to_json())).expect("again"), both);
        assert_eq!(Conversion::from_json(None).expect("none"), Conversion::default());
        let bad = Conversion::from_json(Some(&json!(["resample"]))).expect_err("unknown");
        assert_eq!(bad.code, "terrain_source_bad_params");
    }

    #[test]
    fn error_codes_reach_the_agent_lowercase_with_their_data() {
        let e = SourceImportError::new("terrain_source_undo_limit", "x").with(json!({ "changed": 5 }));
        let rpc = e.to_rpc();
        let data = rpc.data.expect("data");
        assert_eq!(data["code"], "terrain_source_undo_limit");
        assert_eq!(data["changed"], 5);
    }

    #[test]
    fn the_generation_changes_with_any_height_bit() {
        let a = heights_generation(&[1.0, 2.0]);
        assert_eq!(a, heights_generation(&[1.0, 2.0]));
        assert_ne!(
            a,
            heights_generation(&[1.0, f32::from_bits(2.0f32.to_bits() + 1)])
        );
    }
}
