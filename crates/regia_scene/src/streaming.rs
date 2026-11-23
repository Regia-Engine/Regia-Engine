// SPDX-License-Identifier: MPL-2.0
//! World streaming authoring types, messages and readiness gates (`docs/specs/78-world-streaming.md`).
//!
//! The runtime is `regia_world`. It lives in its own crate because it needs the asset server, and
//! this module holds what every subsystem shares with it: physics, render, terrain and nav depend on
//! `regia_scene`, never on `regia_world`, and talk to it only through the messages, gates and sets
//! here. Book page: `book/agents/streaming-runtime.md`.
//!
//! - [`LevelStreaming`]: the streaming settings of a level, a resource in its `resources` map.
//! - [`StreamPlacement`], [`StreamPriorityVolume`], [`StreamingSource`]: authored components.
//! - [`CellMember`], [`StreamFrozen`], [`StreamGuarded`], [`CellRenderHidden`], [`StreamAutoMember`]:
//!   runtime components.
//! - [`CellLoaded`], [`CellReady`], [`CellUnloading`], [`CellFailed`], [`OriginShifted`]: messages.
//! - [`StreamGates`]: named readiness checks; a cell is ready when every gate of a scope says so.
//! - [`StreamViews`]: the sampled source views, for every system that streams around the same points.
//! - [`StreamSet`]: the system sets of the streaming frame order (§ 4.3).

use std::collections::{BTreeMap, BTreeSet};

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::floating_origin::{LargeSpace, Rebase};
use crate::hlod::HlodConfig;
use crate::level::LevelId;
use crate::stream_grid::{CellCoord, StreamError, StreamGrid};
use crate::stream_plan::{SourceView, StreamConfig};

/// The current [`LevelStreaming::version`].
pub const LEVEL_STREAMING_VERSION: u32 = 1;

/// Which default budgets apply (spec 78 § 20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPlatform {
    /// Desktop reference machines.
    Desktop,
    /// Steam Deck.
    Deck,
    /// Android phones, Pixel 6 class.
    Android,
    /// A dedicated server (no render, no proxies).
    Server,
}

impl StreamPlatform {
    /// The platform of this process, from the build target and the Deck's environment marker. A
    /// dedicated server asks for [`StreamPlatform::Server`] itself.
    pub fn host() -> Self {
        if cfg!(target_os = "android") {
            Self::Android
        } else if regia_core::brand::env_var("SteamDeck").is_ok_and(|v| v == "1") {
            Self::Deck
        } else {
            Self::Desktop
        }
    }
}

/// Per-frame and memory budgets of the streaming runtime (spec 78 § 8.4). The defaults are the
/// desktop column of § 20.
#[derive(Debug, Clone, Copy, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Default, Clone)]
#[serde(default)]
pub struct StreamBudgets {
    /// The most loads one plan orders; 0 means no limit.
    pub max_loads_per_step: u32,
    /// The most cells in `Loading` at once.
    pub max_inflight: u32,
    /// Decoded cell bytes waiting for apply; IO pauses above this.
    pub max_pending_bytes: u64,
    /// Main-thread spawn and despawn time per frame in async mode, milliseconds.
    pub apply_budget_ms: f32,
    /// Entities spawned or despawned per run: the limit in lockstep mode, and in async mode the
    /// floor that keeps progress going when the time budget is used up.
    pub apply_budget_entities: u32,
    /// Resident cell memory, the planner's hard bound.
    pub memory_budget_bytes: u64,
    /// Estimated cell VRAM.
    pub vram_budget_bytes: u64,
    /// Resident HLOD proxy meshes.
    pub hlod_budget_bytes: u64,
}

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

impl Default for StreamBudgets {
    fn default() -> Self {
        Self::for_kind(StreamPlatform::Desktop)
    }
}

impl StreamBudgets {
    /// The defaults for this process (spec 78 § 20).
    pub fn for_platform() -> Self {
        Self::for_kind(StreamPlatform::host())
    }

    /// The defaults of one column of spec 78 § 20.
    pub fn for_kind(platform: StreamPlatform) -> Self {
        match platform {
            StreamPlatform::Desktop => Self {
                max_loads_per_step: 8,
                max_inflight: 8,
                max_pending_bytes: 256 * MIB,
                apply_budget_ms: 2.0,
                apply_budget_entities: 400,
                memory_budget_bytes: 2 * GIB,
                vram_budget_bytes: 2 * GIB + 512 * MIB,
                hlod_budget_bytes: 256 * MIB,
            },
            StreamPlatform::Deck => Self {
                max_loads_per_step: 4,
                max_inflight: 4,
                max_pending_bytes: 128 * MIB,
                apply_budget_ms: 1.5,
                apply_budget_entities: 300,
                memory_budget_bytes: GIB,
                vram_budget_bytes: GIB,
                hlod_budget_bytes: 128 * MIB,
            },
            StreamPlatform::Android => Self {
                max_loads_per_step: 2,
                max_inflight: 4,
                max_pending_bytes: 64 * MIB,
                apply_budget_ms: 1.0,
                apply_budget_entities: 150,
                memory_budget_bytes: 512 * MIB,
                vram_budget_bytes: 384 * MIB,
                hlod_budget_bytes: 64 * MIB,
            },
            StreamPlatform::Server => Self {
                max_loads_per_step: 16,
                max_inflight: 16,
                max_pending_bytes: 256 * MIB,
                apply_budget_ms: 2.0,
                apply_budget_entities: 400,
                memory_budget_bytes: 4 * GIB,
                vram_budget_bytes: 0,
                hlod_budget_bytes: 0,
            },
        }
    }

    /// Check the fields a runtime divides or waits on. The error names the field.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_inflight == 0 {
            return Err("max_inflight must be at least 1".into());
        }
        if self.apply_budget_entities == 0 {
            return Err("apply_budget_entities must be at least 1".into());
        }
        if !self.apply_budget_ms.is_finite() || self.apply_budget_ms < 0.0 {
            return Err("apply_budget_ms must be finite and not negative".into());
        }
        Ok(())
    }
}

/// Behavior knobs that are not budgets (spec 78 § 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Default, Clone)]
#[serde(default)]
pub struct StreamTuning {
    /// Steps between plans when nothing else triggers one.
    pub plan_interval_steps: u32,
    /// Steps a failed cell waits before it retries.
    pub retry_after_steps: u32,
    /// Retries before a failed cell stays failed until `stream.retry` or until it is wanted anew.
    pub max_retries: u32,
    /// Bodies this close to a cell that is not ready freeze too, meters.
    pub guard_margin_m: f32,
    /// Level open waits for the cells around the start sources.
    pub wait_on_open: bool,
    /// Cross-fade length of an HLOD switch, frames. Absent (`None`) means 0 on every platform
    /// (instant switches). `Some(8)` is an 8-frame dither an author opts into (ADR-2022).
    pub hlod_fade_frames: Option<u32>,
    /// Lockstep timing: steps from issuing a load to applying it.
    pub lockstep_latency_steps: u32,
    /// Lockstep timing: how long the main thread waits for late IO before the run stalls.
    pub lockstep_wait_cap_ms: u32,
}

impl Default for StreamTuning {
    fn default() -> Self {
        Self {
            plan_interval_steps: 30,
            retry_after_steps: 300,
            max_retries: 3,
            guard_margin_m: 4.0,
            wait_on_open: true,
            hlod_fade_frames: None,
            lockstep_latency_steps: 2,
            lockstep_wait_cap_ms: 10_000,
        }
    }
}

impl StreamTuning {
    /// The cross-fade length: the authored value, else 0 (instant switches) on every platform.
    /// `Some(8)` is an 8-frame dither (ADR-2022).
    pub fn fade_frames(&self) -> u32 {
        self.hlod_fade_frames.unwrap_or(0)
    }
}

/// HLOD settings of a streamed level (spec 78 § 11).
#[derive(Debug, Clone, Copy, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Default, Clone)]
#[serde(default)]
pub struct HlodSettings {
    /// A proxy is drawn at or below this fraction of the viewport height. `None`: the cook derives
    /// it so a leaf switches just inside the load radius.
    pub screen_size_threshold: Option<f64>,
    /// The camera field of view the threshold is judged at, radians.
    pub fov_y_rad: f64,
    /// Children per parent along each axis.
    pub merge_factor: u32,
    /// The most merge passes.
    pub max_levels: u32,
    /// A parent proxy's bytes as a fraction of its children's.
    pub proxy_byte_ratio: f64,
    /// A cell proxy's triangles as a fraction of the cell's.
    pub leaf_triangle_ratio: f32,
    /// A parent proxy's triangles as a fraction of its children's.
    pub parent_triangle_ratio: f32,
    /// The least triangles a proxy keeps.
    pub min_triangles: u32,
}

impl Default for HlodSettings {
    fn default() -> Self {
        let config = HlodConfig::default();
        Self {
            screen_size_threshold: None,
            fov_y_rad: config.fov_y_rad,
            merge_factor: config.merge_factor,
            max_levels: config.max_levels,
            proxy_byte_ratio: config.proxy_byte_ratio,
            leaf_triangle_ratio: 0.05,
            parent_triangle_ratio: 0.25,
            min_triangles: 64,
        }
    }
}

impl HlodSettings {
    /// The tree configuration for these settings. An unset threshold becomes
    /// `threshold = r_leaf / (0.9 · load_radius · tan(fov_y / 2))`, clamped to `(0, 1]`, so a leaf of
    /// radius `r_leaf` switches at 90 % of the load radius (spec 78 § 11.2).
    pub fn config(&self, load_radius_m: f64, r_leaf: f64) -> HlodConfig {
        let derived = r_leaf / (0.9 * load_radius_m * (self.fov_y_rad * 0.5).tan());
        HlodConfig {
            fov_y_rad: self.fov_y_rad,
            screen_size_threshold: self
                .screen_size_threshold
                .unwrap_or(derived)
                .clamp(f64::MIN_POSITIVE, 1.0),
            merge_factor: self.merge_factor,
            max_levels: self.max_levels,
            proxy_byte_ratio: self.proxy_byte_ratio,
        }
    }
}

/// Floating-origin settings of a streamed level (spec 78 § 12.1).
#[derive(Debug, Clone, Copy, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Default, Clone)]
#[serde(default)]
pub struct OriginSettings {
    /// Rebase at all.
    pub enabled: bool,
    /// Origin cell size, meters. A whole number from 1 through
    /// [`MAX_ORIGIN_CELL_SIZE`](crate::floating_origin::MAX_ORIGIN_CELL_SIZE) (10 000 000), so a
    /// whole-cell shift stays exact in `f32`. [`LevelStreaming::validate`] refuses anything
    /// [`LargeSpace::new`](crate::floating_origin::LargeSpace::new) refuses.
    pub cell_size_m: f64,
    /// Rebase when the focus is further than this from the origin, meters.
    pub threshold_m: f32,
}

impl Default for OriginSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            cell_size_m: 1024.0,
            threshold_m: 5000.0,
        }
    }
}

/// Streaming settings of a level (spec 78 § 5.1), stored in the level's `resources` map under
/// `regia_scene::streaming::LevelStreaming`. Absent, or `enabled: false`, means not streamed.
#[derive(Resource, Debug, Clone, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Resource, Default, Clone)]
#[serde(default)]
pub struct LevelStreaming {
    /// Shape version of this resource ([`LEVEL_STREAMING_VERSION`]).
    pub version: u32,
    /// Stream the level.
    pub enabled: bool,
    /// Side of a cell, meters, `8..=4096`.
    pub cell_size_m: f64,
    /// A cell this close to a source loads, meters.
    pub load_radius_m: f64,
    /// A cell beyond this from every source unloads, meters. At least `load_radius_m`.
    pub unload_radius_m: f64,
    /// Entities wider than this many cells go to the always-loaded set.
    pub max_entity_extent_cells: f64,
    /// HLOD settings; `None` turns proxies off.
    pub hlod: Option<HlodSettings>,
    /// Cook nav tiles into the cells; `None` cooks none (spec 78 § 13.2).
    pub nav: Option<StreamNavCook>,
    /// Floating origin.
    pub origin: OriginSettings,
    /// Budgets; `None` takes [`StreamBudgets::for_platform`].
    pub budgets: Option<StreamBudgets>,
    /// Behavior knobs.
    pub tuning: StreamTuning,
    /// Baked HLOD proxy atlas density, texels per meter of proxy surface at tree level 0 (a level
    /// `L` node bakes at this over `2^L`); 0, the default, keeps flat-color proxies (ADR-2483).
    pub hlod_atlas_px: u32,
    /// The largest side of one proxy's atlas, texels (16..=4096). A proxy whose density would
    /// need more is baked coarser, and the cook warns `stream_hlod_atlas_capped`.
    pub hlod_atlas_max_px: u32,
}

/// The default [`LevelStreaming::hlod_atlas_max_px`].
pub const HLOD_ATLAS_MAX_PX_DEFAULT: u32 = 1024;

impl Default for LevelStreaming {
    fn default() -> Self {
        Self {
            version: LEVEL_STREAMING_VERSION,
            enabled: false,
            cell_size_m: 128.0,
            load_radius_m: 512.0,
            unload_radius_m: 640.0,
            max_entity_extent_cells: 2.0,
            hlod: Some(HlodSettings::default()),
            origin: OriginSettings::default(),
            budgets: None,
            nav: None,
            tuning: StreamTuning::default(),
            hlod_atlas_px: 0,
            hlod_atlas_max_px: HLOD_ATLAS_MAX_PX_DEFAULT,
        }
    }
}

impl LevelStreaming {
    /// Whether the cook bakes textured HLOD proxies: HLOD on and [`Self::hlod_atlas_px`] above 0.
    pub fn hlod_atlas_enabled(&self) -> bool {
        self.hlod.is_some() && self.hlod_atlas_px > 0
    }

    /// The cook-time and load-time check (spec 78 § 5.1). The error names the field.
    /// When the origin is enabled, `origin.cell_size_m` must also be a size
    /// [`LargeSpace::new`](crate::floating_origin::LargeSpace::new) accepts (at most
    /// 10 000 000 m).
    pub fn validate(&self) -> Result<(), String> {
        if self.version > LEVEL_STREAMING_VERSION {
            return Err(format!(
                "version {} is newer than {LEVEL_STREAMING_VERSION}",
                self.version
            ));
        }
        if !(8.0..=4096.0).contains(&self.cell_size_m) {
            return Err(format!("cell_size_m {} is not in 8..=4096", self.cell_size_m));
        }
        if !(self.max_entity_extent_cells.is_finite() && self.max_entity_extent_cells >= 1.0) {
            return Err(format!(
                "max_entity_extent_cells {} is below 1",
                self.max_entity_extent_cells
            ));
        }
        self.planner_config(&self.effective_budgets())
            .validate()
            .map_err(|e| match e {
                StreamError::NoHysteresis { load, unload } => {
                    format!("unload_radius_m {unload} is below load_radius_m {load}")
                }
                StreamError::BadRadius { what, value } => format!("{what} {value} is not usable"),
                other => other.to_string(),
            })?;
        if let Some(hlod) = &self.hlod {
            hlod.config(self.load_radius_m.max(1.0), self.cell_size_m)
                .validate()
                .map_err(|e| format!("hlod: {e}"))?;
        }
        if self.hlod_atlas_px > 0 && !(16..=4096).contains(&self.hlod_atlas_max_px) {
            return Err(format!(
                "hlod_atlas_max_px {} is not in 16..=4096",
                self.hlod_atlas_max_px
            ));
        }
        if self.hlod_atlas_px > 4096 {
            return Err(format!("hlod_atlas_px {} is above 4096", self.hlod_atlas_px));
        }
        if self.origin.enabled {
            let size = self.origin.cell_size_m;
            if !(size.is_finite() && size >= 1.0 && size.fract() == 0.0) {
                return Err(format!(
                    "origin.cell_size_m {size} is not a whole number of meters"
                ));
            }
            if let Err(err) = LargeSpace::new(size) {
                return Err(format!("origin.cell_size_m {size}: {err}"));
            }
            let threshold = f64::from(self.origin.threshold_m);
            if !threshold.is_finite() || threshold < size * 0.5 {
                return Err(format!(
                    "origin.threshold_m {} is below half of origin.cell_size_m",
                    self.origin.threshold_m
                ));
            }
        }
        self.budgets
            .as_ref()
            .map_or(Ok(()), StreamBudgets::validate)
            .map_err(|e| format!("budgets: {e}"))
    }

    /// The budgets that apply: the authored ones, else the platform defaults.
    pub fn effective_budgets(&self) -> StreamBudgets {
        self.budgets.unwrap_or_else(StreamBudgets::for_platform)
    }

    /// The grid of this level.
    pub fn grid(&self) -> Result<StreamGrid, StreamError> {
        StreamGrid::new(self.cell_size_m)
    }

    /// The planner configuration for `budgets`.
    pub fn planner_config(&self, budgets: &StreamBudgets) -> StreamConfig {
        StreamConfig {
            load_radius: self.load_radius_m,
            unload_radius: self.unload_radius_m,
            memory_budget_bytes: budgets.memory_budget_bytes,
            max_loads_per_step: budgets.max_loads_per_step,
        }
    }
}

/// Where an entity of a streamed level goes at cook time (spec 78 § 5.3).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Reflect, Serialize, Deserialize)]
#[reflect(Component, Default, Clone)]
pub enum StreamPlacement {
    /// The cell containing the center of its bounds.
    #[default]
    Auto,
    /// The always-loaded set.
    AlwaysLoaded,
    /// This cell.
    Cell {
        /// Index along X.
        x: i64,
        /// Index along Z.
        z: i64,
    },
}

/// Raises the load priority of the cells this volume overlaps. Cook-time: the record of a cell takes
/// the highest priority over the volumes overlapping it.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Component, Default, Clone)]
pub struct StreamPriorityVolume {
    /// Half extents around the entity, meters.
    pub half_extents: Vec3,
    /// The priority.
    pub priority: i32,
}

/// What a [`StreamingSource`] stands for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Reflect, Serialize, Deserialize)]
#[reflect(Default, Clone)]
pub enum SourceKind {
    /// A render camera.
    #[default]
    Camera,
    /// A player's pawn.
    Player,
    /// Script-driven: cutscenes, teleport targets, AI far from the player.
    Scripted,
}

impl SourceKind {
    /// The agent spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Camera => "camera",
            Self::Player => "player",
            Self::Scripted => "scripted",
        }
    }
}

/// A streaming source (spec 78 § 7), on any entity with a transform.
#[derive(Component, Debug, Clone, Copy, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Component, Default, Clone)]
#[serde(default)]
pub struct StreamingSource {
    /// What it stands for.
    pub kind: SourceKind,
    /// Whether it counts.
    pub enabled: bool,
    /// Divides distances, `0.1..=8`: a source with scale 2 wants cells twice as far.
    pub radius_scale: f32,
    /// Added to the priority of every cell this source wants.
    pub priority: i32,
    /// How much the view direction pulls cells forward in the load order, `0..=1`. Ordering only.
    pub view_bias: f32,
    /// Adds a look-ahead point at `pos + velocity · predict_s`, `0..=4` seconds.
    pub predict_s: f32,
    /// This source is the floating-origin focus. At most one source has it.
    pub drives_origin: bool,
    /// Only pulls cells ahead of something else (a cutscene's next stretch): never the origin's
    /// focus, and the active camera's implicit source stays.
    pub look_ahead: bool,
}

impl Default for StreamingSource {
    fn default() -> Self {
        Self {
            kind: SourceKind::Camera,
            enabled: true,
            radius_scale: 1.0,
            priority: 0,
            view_bias: 0.5,
            predict_s: 0.0,
            drives_origin: false,
            look_ahead: false,
        }
    }
}

/// The cell an entity belongs to; it unloads with that cell. Set on every entity spawned from a cell
/// blob (spec 78 § 5.2).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Reflect)]
#[reflect(Component, Clone)]
pub struct CellMember(pub CellCoord);

/// A body the guard froze because a cell near it is not ready (spec 78 § 10.4). `regia_physics`
/// disables a frozen body and keeps its velocity for when it wakes.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Reflect)]
#[reflect(Component, Default, Clone)]
pub struct StreamFrozen;

/// A body the guard watches. `regia_physics` keeps this on dynamic bodies and characters, so the
/// runtime can find them without depending on the physics crate. Kinematic bodies and static
/// colliders never carry it.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Reflect)]
#[reflect(Component, Default, Clone)]
pub struct StreamGuarded;

/// A member of a cell that a proxy covers: it keeps simulating, the renderer skips it (§ 11.2).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Reflect)]
#[reflect(Component, Default, Clone)]
pub struct CellRenderHidden;

/// Put on a runtime-spawned entity that should belong to the cell it stands in (§ 10.5).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Reflect, Serialize, Deserialize)]
#[reflect(Component, Default, Clone)]
pub struct StreamAutoMember;

/// How ready a cell is.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Reflect, Serialize, Deserialize,
)]
#[reflect(Default, Clone)]
pub enum Readiness {
    /// Every simulation gate is ready: colliders, terrain collision, nav.
    #[default]
    SimReady,
    /// Every render gate is ready too: meshes uploaded, base texture mips resident.
    Visible,
}

impl Readiness {
    /// The agent spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SimReady => "sim_ready",
            Self::Visible => "visible",
        }
    }

    /// Parses the agent spelling.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "sim_ready" => Some(Self::SimReady),
            "visible" => Some(Self::Visible),
            _ => None,
        }
    }
}

/// A cell finished spawning: it reached `Spawned`. Written once per load.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct CellLoaded {
    /// The level.
    pub level: LevelId,
    /// The cell.
    pub cell: CellCoord,
    /// Every entity of the cell.
    pub entities: Vec<Entity>,
    /// The cell's cooked nav tiles (`CellBlob::nav`), for the `nav` gate's hook (spec 78 § 13.2).
    pub nav: Option<std::sync::Arc<[u8]>>,
}

/// A cell reached a readiness level.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellReady {
    /// The level.
    pub level: LevelId,
    /// The cell.
    pub cell: CellCoord,
    /// How ready it is.
    pub readiness: Readiness,
}

/// A cell starts unloading on the next run of the apply step, so game systems can read its members
/// and save what must survive (spec 78 § 10.6).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellUnloading {
    /// The level.
    pub level: LevelId,
    /// The cell.
    pub cell: CellCoord,
}

/// A cell failed to load.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct CellFailed {
    /// The level.
    pub level: LevelId,
    /// The cell.
    pub cell: CellCoord,
    /// One of the cell failure reasons of spec 78 § 17 (`cell_corrupt`, ...).
    pub reason: String,
}

/// The floating origin moved (spec 78 § 12). Everything that holds a local position outside
/// `Transform` and `GlobalTransform` handles this in the same frame.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct OriginShifted {
    /// The shift: apply [`Rebase::apply`] to every local position.
    pub rebase: Rebase,
    /// The new origin cell.
    pub origin_cell: [i64; 3],
}

/// The world position of this process's local origin, `f64` meters: what a local position adds to
/// become a world position (spec 78 § 12.1). The streaming runtime keeps it equal to its
/// `WorldOrigin`; it stays zero without a streamed level. Crates below the runtime read it at their
/// edges, such as the net wire (spec 78 § 14, ADR-2022).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct OriginOffset(pub [f64; 3]);

/// Subsystems that hold positions a floating-origin rebase cannot move yet, by name (ADR-2022).
/// While any is registered the runtime takes no rebase: content far from the origin loses precision
/// instead of drawing in the wrong place. A host store that cannot follow a rebase yet may register
/// one. The engine registers [`TERRAIN_STROKE_BLOCKER`] and [`VEHICLE_CONTACT_BLOCKER`] through
/// [`RebaseHolds`] while such an operation is open (ADR-2435).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct RebaseBlockers(pub BTreeSet<&'static str>);

/// The [`RebaseBlockers`] name an open terrain brush stroke holds (`regia_terrain`'s
/// `TerrainStrokeSession`): its points arrive over several frames, and a point picked before a
/// rebase must not be smeared to one picked after it.
pub const TERRAIN_STROKE_BLOCKER: &str = "terrain_brush_stroke";

/// The [`RebaseBlockers`] name a vehicle contact solve holds while it runs (`regia_physics`'s
/// `drive_vehicles`): the wheel rays and the impulses they give are one solve in one frame.
pub const VEHICLE_CONTACT_BLOCKER: &str = "vehicle_contact_solve";

/// Counted holds on [`RebaseBlockers`] names: several owners may hold one name (two terrains
/// stroked at once), and the name stays registered until the last one lets go (ADR-2435).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct RebaseHolds(BTreeMap<&'static str, u32>);

impl RebaseHolds {
    /// Takes one hold on `name` and registers it in `blockers`.
    pub fn acquire(&mut self, blockers: &mut RebaseBlockers, name: &'static str) {
        let count = self.0.entry(name).or_insert(0);
        *count = count.saturating_add(1);
        blockers.0.insert(name);
    }

    /// Lets one hold on `name` go; the last one unregisters it from `blockers`. Returns `false`
    /// when nothing held `name` (the call changes nothing then).
    pub fn release(&mut self, blockers: &mut RebaseBlockers, name: &'static str) -> bool {
        let Some(count) = self.0.get_mut(name) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.0.remove(name);
            blockers.0.remove(name);
        }
        true
    }

    /// Holds on `name` now.
    #[must_use]
    pub fn count(&self, name: &str) -> u32 {
        self.0.get(name).copied().unwrap_or(0)
    }

    /// Every held name with its count, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, u32)> + '_ {
        self.0.iter().map(|(name, count)| (*name, *count))
    }
}

/// [`RebaseHolds::acquire`] on a world: inserts both resources when missing.
pub fn acquire_rebase_hold(world: &mut World, name: &'static str) {
    world.init_resource::<RebaseBlockers>();
    world.init_resource::<RebaseHolds>();
    world.resource_scope(|world, mut holds: Mut<RebaseHolds>| {
        holds.acquire(&mut world.resource_mut::<RebaseBlockers>(), name);
    });
}

/// [`RebaseHolds::release`] on a world; `false` when nothing held `name`.
pub fn release_rebase_hold(world: &mut World, name: &'static str) -> bool {
    let Some(mut holds) = world.remove_resource::<RebaseHolds>() else {
        return false;
    };
    world.init_resource::<RebaseBlockers>();
    let released = holds.release(&mut world.resource_mut::<RebaseBlockers>(), name);
    world.insert_resource(holds);
    released
}

/// How a streamed level's nav tiles are cooked (spec 78 § 13.2): the voxel grid and the agent. The
/// ground is every static collider of the level. The values mean what `regia_nav`'s build parameters
/// mean; this type sits here because the settings are authored on [`LevelStreaming`] below `regia_nav`.
#[derive(Debug, Clone, Copy, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Default, Clone)]
#[serde(default)]
pub struct StreamNavCook {
    /// Grid cell width, meters.
    pub cell_size: f32,
    /// Grid cell height, meters.
    pub cell_height: f32,
    /// Margin around the geometry, meters.
    pub border_size: f32,
    /// Grid cells per tile side.
    pub tile_size: u32,
    /// Agent radius, meters.
    pub agent_radius: f32,
    /// Agent height, meters.
    pub agent_height: f32,
    /// Step an agent climbs, meters.
    pub agent_max_climb: f32,
    /// Steepest walkable slope, radians.
    pub agent_max_slope: f32,
}

impl Default for StreamNavCook {
    fn default() -> Self {
        Self {
            cell_size: 0.5,
            cell_height: 0.2,
            border_size: 1.0,
            tile_size: 32,
            agent_radius: 0.35,
            agent_height: 1.6,
            agent_max_climb: 0.5,
            agent_max_slope: std::f32::consts::FRAC_PI_4,
        }
    }
}

/// The grid a streamed level's nav tiles were cooked on, from its world index: what the app's streamed
/// tile cache must be built with, in world meters (spec 78 § 13.2). The runtime inserts it when such a
/// level opens; `regia_nav`'s stream hook builds the cache from it.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamNavLayout {
    /// The cook settings.
    pub cook: StreamNavCook,
    /// The grid's lowest corner, world meters.
    pub origin: [f32; 3],
    /// Grid cells along X.
    pub width: u32,
    /// Grid cells along Z.
    pub height: u32,
}

/// What [`StreamedLevelOpener`] did with a level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamedOpen {
    /// The level has no cooked world index: open it the ordinary way.
    NotCooked,
    /// The level opened through the streaming runtime.
    Opened,
    /// The world index is newer than this build (`stream_format_newer`). The level is open with
    /// its always-loaded set only; cell streaming did not start (spec 78 § 6.5).
    AlwaysLoaded,
    /// The level has a cooked world index and it did not open.
    Failed(String),
}

/// Opens a level cooked for streaming (spec 78 § 6.5), for code that opens levels by id and cannot
/// name the runtime: `regia run --level` and the dedicated server's rotation. The runtime's plugin
/// installs it; without it every level opens the ordinary way.
#[derive(Resource, Clone, Copy)]
pub struct StreamedLevelOpener(
    pub fn(&mut World, crate::level::LevelId, crate::level::LoadMode) -> StreamedOpen,
);

/// The gate of a level's terrain collider (spec 78 § 13.1).
pub const TERRAIN_COLLISION_GATE: &str = "terrain_collision";

/// A gate's id in [`StreamGates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GateId(u32);

#[derive(Debug, Clone)]
struct Gate {
    name: &'static str,
    scope: Readiness,
    /// The gate also applies to cells with no record (terrain collision does).
    covers_empty: bool,
    ready: BTreeSet<(LevelId, CellCoord)>,
    awaiting: BTreeSet<(LevelId, CellCoord)>,
    /// Levels the gate holds no cell back in: one whole-map answer instead of one per cell.
    open: BTreeSet<LevelId>,
    /// The gate applies only to levels in `claimed`.
    scoped: bool,
    claimed: BTreeSet<LevelId>,
    /// The gate holds nothing back in any level.
    open_everywhere: bool,
}

impl Gate {
    fn passes(&self, level: LevelId, cell: CellCoord) -> bool {
        self.open_everywhere
            || (self.scoped && !self.claimed.contains(&level))
            || self.open.contains(&level)
            || self.ready.contains(&(level, cell))
    }
}

/// Named readiness checks (spec 78 § 10.3). A cell is `SimReady` when every `SimReady` gate marked
/// it and `Visible` when every `Visible` gate did too. A gate that never answers shows up by name in
/// `stream.cells`, which is how an agent finds a stuck cell.
#[derive(Resource, Debug, Clone, Default)]
pub struct StreamGates {
    gates: Vec<Gate>,
    revision: u64,
}

impl StreamGates {
    /// Registers a gate, or returns the id of the gate with this name.
    pub fn register(&mut self, name: &'static str, scope: Readiness) -> GateId {
        self.add(name, scope, false)
    }

    /// Registers a gate that also applies to cells with no record: they have nothing to load, but
    /// the gate's own work still covers them (terrain collision, spec 78 § 6.2).
    pub fn register_covering_empty(&mut self, name: &'static str, scope: Readiness) -> GateId {
        self.add(name, scope, true)
    }

    /// Registers a gate like [`Self::register_covering_empty`] that applies only to levels that
    /// [`Self::claim_level`]: a level without terrain never waits on a terrain gate, even in a process
    /// that opened one with terrain before.
    pub fn register_for_claimed_levels(&mut self, name: &'static str, scope: Readiness) -> GateId {
        let gate = self.add(name, scope, true);
        if let Some(g) = self.gates.get_mut(gate.0 as usize) {
            g.scoped = true;
        }
        gate
    }

    /// Makes a gate of [`Self::register_for_claimed_levels`] apply to `level`.
    pub fn claim_level(&mut self, gate: GateId, level: LevelId) {
        if let Some(g) = self.gates.get_mut(gate.0 as usize)
            && g.claimed.insert(level)
        {
            self.revision += 1;
        }
    }

    /// Marks every cell of every level ready for a gate: the answer of work owned by no level.
    pub fn open_everywhere(&mut self, gate: GateId) {
        if let Some(g) = self.gates.get_mut(gate.0 as usize)
            && !g.open_everywhere
        {
            g.open_everywhere = true;
            g.awaiting.clear();
            self.revision += 1;
        }
    }

    fn add(&mut self, name: &'static str, scope: Readiness, covers_empty: bool) -> GateId {
        if let Some(index) = self.gates.iter().position(|g| g.name == name) {
            return GateId(index as u32);
        }
        self.gates.push(Gate {
            name,
            scope,
            covers_empty,
            ready: BTreeSet::new(),
            awaiting: BTreeSet::new(),
            open: BTreeSet::new(),
            scoped: false,
            claimed: BTreeSet::new(),
            open_everywhere: false,
        });
        self.revision += 1;
        GateId(self.gates.len() as u32 - 1)
    }

    /// Marks a cell ready for a gate.
    pub fn set_ready(&mut self, gate: GateId, level: LevelId, cell: CellCoord) {
        if let Some(g) = self.gates.get_mut(gate.0 as usize) {
            g.awaiting.remove(&(level, cell));
            if g.ready.insert((level, cell)) {
                self.revision += 1;
            }
        }
    }

    /// Marks every cell of `level` ready for a gate, now and later: the answer of a gate whose work
    /// covers the whole map, such as a single terrain collider.
    pub fn open_level(&mut self, gate: GateId, level: LevelId) {
        if let Some(g) = self.gates.get_mut(gate.0 as usize)
            && g.open.insert(level)
        {
            g.awaiting.retain(|(l, _)| *l != level);
            self.revision += 1;
        }
    }

    /// Takes a cell's mark back: it moves down a readiness level until the gate marks it again.
    pub fn clear(&mut self, gate: GateId, level: LevelId, cell: CellCoord) {
        if let Some(g) = self.gates.get_mut(gate.0 as usize)
            && g.ready.remove(&(level, cell))
        {
            self.revision += 1;
        }
    }

    /// Whether every gate of `scope` marked the cell, and for `Visible` every `SimReady` gate too.
    pub fn is_ready(&self, level: LevelId, cell: CellCoord, scope: Readiness) -> bool {
        self.gates
            .iter()
            .filter(|g| g.scope <= scope)
            .all(|g| g.passes(level, cell))
    }

    /// [`Self::is_ready`] for a cell with no record: only gates that cover empty cells count.
    pub fn is_empty_cell_ready(&self, level: LevelId, cell: CellCoord, scope: Readiness) -> bool {
        self.gates
            .iter()
            .filter(|g| g.scope <= scope && g.covers_empty)
            .all(|g| g.passes(level, cell))
    }

    /// The names of the gates still missing for a cell, in registration order.
    pub fn missing(&self, level: LevelId, cell: CellCoord) -> Vec<&'static str> {
        self.gates
            .iter()
            .filter(|g| !g.passes(level, cell))
            .map(|g| g.name)
            .collect()
    }

    /// Whether any gate of `scope` is registered.
    pub fn has_scope(&self, scope: Readiness) -> bool {
        self.gates.iter().any(|g| g.scope == scope)
    }

    /// Tells every gate that a cell spawned and waits for it. Called by the runtime.
    pub fn note_spawned(&mut self, level: LevelId, cell: CellCoord) {
        for g in &mut self.gates {
            if !g.passes(level, cell) {
                g.awaiting.insert((level, cell));
            }
        }
    }

    /// The cells that spawned and that `gate` has not marked yet, in `(level, cell)` order. A gate's
    /// system reads this each run: messages can be missed by systems in a fixed step that does not
    /// run every frame, this list cannot.
    pub fn awaiting(&self, gate: GateId) -> impl Iterator<Item = (LevelId, CellCoord)> + '_ {
        self.gates
            .get(gate.0 as usize)
            .into_iter()
            .flat_map(|g| g.awaiting.iter().copied())
    }

    /// Whether `gate` itself marked `cell` (one gate, unlike [`Self::is_ready`]). A hook that keeps
    /// work per spawned cell reads this with [`Self::awaiting`]: a cell in neither list unloaded.
    pub fn is_marked(&self, gate: GateId, level: LevelId, cell: CellCoord) -> bool {
        self.gates
            .get(gate.0 as usize)
            .is_some_and(|g| g.ready.contains(&(level, cell)))
    }

    /// Forgets a cell in every gate (it unloaded).
    pub fn forget_cell(&mut self, level: LevelId, cell: CellCoord) {
        for g in &mut self.gates {
            g.awaiting.remove(&(level, cell));
            g.ready.remove(&(level, cell));
        }
        self.revision += 1;
    }

    /// Forgets a level in every gate.
    pub fn forget_level(&mut self, level: LevelId) {
        for g in &mut self.gates {
            g.awaiting.retain(|(l, _)| *l != level);
            g.ready.retain(|(l, _)| *l != level);
            g.open.remove(&level);
            g.claimed.remove(&level);
        }
        self.revision += 1;
    }

    /// Changes whenever a mark changes, so the runtime can skip work when nothing moved.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// Whether a streamed level is open. The world runtime writes it as levels open and close; physics
/// tags dynamic bodies and characters with [`StreamGuarded`] only while it is set, so an app that never
/// streams keeps its components, archetypes and simulation order exactly as without streaming.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamingActive(pub bool);

/// The source views sampled for this step (spec 78 § 7.3), sorted by source id. Terrain tile
/// requests, procgen anchors and audio prefetch read this, so everything streams around the same
/// points.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct StreamViews(pub Vec<SourceView>);

/// The view of the active render camera, for the implicit source of a streamed level with no
/// enabled source (spec 78 § 7.1). The renderer's side writes this; without one, such a level has no
/// sources and stays as it is.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct PrimaryStreamView {
    /// Local-space position and view direction of the primary camera, if there is one.
    pub view: Option<([f32; 3], [f32; 3])>,
}

/// One draw of the repaired HLOD cut (spec 78 § 11.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HlodCutEntry {
    /// The tree node.
    pub node: u32,
    /// Its proxy, or (false) the full content of a leaf.
    pub proxy: bool,
    /// The cell of a leaf.
    pub cell: Option<CellCoord>,
    /// Dithered cross-fade, `0..=1`; 0 is a settled draw. The renderer may ignore it.
    pub fade: f32,
}

/// This frame's repaired HLOD cut. Presentation: simulation never reads it.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct HlodCut {
    /// The draws, by node.
    pub entries: Vec<HlodCutEntry>,
    /// Leaves nothing covers.
    pub holes: u64,
    /// Cells covered by a proxy draw, and that draw's node.
    pub hidden: BTreeMap<CellCoord, u32>,
    /// Draws that left the cut and still dissolve out during an HLOD cross-fade (ADR-2022), each
    /// with the share of pixels it still covers as `fade`. Empty without a cross-fade.
    pub leaving: Vec<HlodCutEntry>,
}

/// An HLOD proxy's mesh, on the entity the runtime keeps for a resident proxy. The renderer's side
/// turns it into a draw; a proxy has no collision and no nav.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct HlodProxyMesh {
    /// The tree node.
    pub node: u32,
    /// The cooked mesh, as a guid.
    pub mesh: regia_core::id::Guid,
    /// The size hint for logs.
    pub bytes: u64,
    /// The proxy's flat linear RGBA base color, or `None` for the default material.
    pub color: Option<[f32; 4]>,
    /// The proxy's baked atlas material (a cooked standard material, as a guid; ADR-2483), or
    /// `None` to draw with [`Self::color`]. A renderer that cannot read it falls back to the color.
    pub material: Option<regia_core::id::Guid>,
}

/// The dither of an HLOD cross-fade on a proxy or on the members of a cell (spec 78 § 11.2,
/// ADR-2022): `c` in `(0, 1)` keeps that share of the pixels (the incoming side), `-c` keeps the
/// other `1 - c` (the outgoing side). The renderer's side draws it as its LOD dither. Presentation:
/// simulation never reads it and it is not reflected, so state hashes leave it out.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct HlodFade(pub f32);

/// Which HLOD proxies are ready to draw, by tree node. The side that loads a proxy mesh (the renderer
/// hook) marks it; the runtime's repair only draws what this holds.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct HlodProxyResidency {
    /// Resident nodes.
    pub resident: BTreeSet<u32>,
}

/// Asks physics to teleport a body to its `Transform` on the next step: the runtime moved it and
/// cannot name the physics crate's own marker.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamTeleported;

/// The system sets of the streaming frame order (spec 78 § 4.3). In `FixedUpdate` they run in the
/// order `Guard`, physics, `Sources`, `Rebase`, `Plan`, `Complete`, `Apply`; `Complete` and `Apply`
/// also exist in `PreUpdate` and `Update`, where they run in async timing, and `Hlod` in `PostUpdate`.
/// Gameplay that moves a source should run before `Sources`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamSet {
    /// Freeze or wake bodies near cells that are not ready, reassign moved members.
    Guard,
    /// Sample every source into [`StreamViews`].
    Sources,
    /// Move the floating origin and shift everything with it.
    Rebase,
    /// Run the planner when a trigger fired; queue loads, cancel unwanted work.
    Plan,
    /// Take finished reads and decodes.
    Complete,
    /// Spawn and despawn within the budget.
    Apply,
    /// The HLOD cut and its repair.
    Hlod,
}

/// Registers the streaming types and messages. Part of [`crate::ScenePlugin`]; safe to call twice,
/// so the runtime's plugin can run in an app that adds the scene plugin later.
pub fn add_to_app(app: &mut regia_app::App) {
    app.register_type::<LevelStreaming>()
        .register_type::<StreamPlacement>()
        .register_type::<StreamPriorityVolume>()
        .register_type::<StreamingSource>()
        .register_type::<CellMember>()
        .register_type::<StreamFrozen>()
        .register_type::<StreamGuarded>()
        .register_type::<CellRenderHidden>()
        .register_type::<StreamAutoMember>()
        .register_type::<TerrainTileCollision>()
        .init_resource::<StreamLevelLayouts>()
        .init_resource::<StreamGates>()
        .init_resource::<StreamViews>()
        .init_resource::<StreamingActive>()
        .init_resource::<RebaseBlockers>()
        .init_resource::<RebaseHolds>()
        .init_resource::<PrimaryStreamView>()
        .init_resource::<HlodCut>()
        .init_resource::<HlodProxyResidency>()
        .init_resource::<HlodVoxelResidency>()
        .add_message::<CellLoaded>()
        .add_message::<CellReady>()
        .add_message::<CellUnloading>()
        .add_message::<CellFailed>()
        .add_message::<OriginShifted>();
}

/// The replicated level entities that live in streamed cells, per open streamed level (spec 78
/// § 14, spec 71 § 4.5). `regia_world` fills it from the world index's `replicated` table when a
/// streamed level opens and forgets the level when it closes. A net client keeps the bind of a guid
/// listed here until its cell spawns the entity, instead of reporting a content mismatch.
#[derive(Resource, Debug, Default, Clone)]
pub struct StreamedReplicated {
    levels: BTreeMap<u128, BTreeMap<u128, CellCoord>>,
    revision: u64,
}

impl StreamedReplicated {
    /// Records a level's table (`(guid, cell)` pairs), replacing what the level had.
    pub fn insert_level(&mut self, level: LevelId, entries: impl IntoIterator<Item = (u128, CellCoord)>) {
        self.levels
            .insert(level.0.as_u128(), entries.into_iter().collect());
        self.revision += 1;
    }

    /// Forgets a level. True when it was recorded.
    pub fn remove_level(&mut self, level: LevelId) -> bool {
        let had = self.levels.remove(&level.0.as_u128()).is_some();
        if had {
            self.revision += 1;
        }
        had
    }

    /// The cell of a replicated level entity when `guid` of `level` (the `LevelMember` id) lives in
    /// a streamed cell.
    pub fn cell_of(&self, level: u128, guid: u128) -> Option<CellCoord> {
        self.levels.get(&level)?.get(&guid).copied()
    }

    /// Whether `level` (the `LevelMember` id) is recorded.
    pub fn has_level(&self, level: u128) -> bool {
        self.levels.contains_key(&level)
    }

    /// Entries over every level.
    pub fn len(&self) -> usize {
        self.levels.values().map(BTreeMap::len).sum()
    }

    /// No entries.
    pub fn is_empty(&self) -> bool {
        self.levels.values().all(BTreeMap::is_empty)
    }

    /// Changes on every insert and removal: a reader that keeps derived state checks it.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// Opt-in on a terrain's heightfield collider: physics builds the heightfield as square tiles, each a
/// collider of its own, and keeps only the tiles that spawned cells and the sources around them need
/// (spec 78 § 13.1, ADR-2317). Without it the heightfield is one collider over the whole map and the
/// `terrain_collision` gate opens the whole level at once.
///
/// A tile is kept while any spawned cell covering it, or any source within `view_radius_m` of it,
/// holds it (a reference count per tile), so a tile shared by four cells stays while any of them is
/// loaded. The gate marks a cell once every tile under its rectangle is in the physics world; a cell
/// with no content is marked while the tiles under it are kept by the sources. Tiles and cells need
/// not align; the cook warns `stream_terrain_misaligned` when neither size divides the other.
///
/// ```
/// use regia_scene::streaming::TerrainTileCollision;
/// let tiles = TerrainTileCollision::default();
/// assert_eq!(tiles.tile_m, 128.0);
/// assert!(tiles.view_radius_m > 0.0);
/// ```
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Component, Default, Clone)]
#[serde(default)]
pub struct TerrainTileCollision {
    /// Tile edge, meters; rounded to whole heightfield cells (at least one).
    pub tile_m: f32,
    /// Tiles within this distance of a source's ground position are kept for the source (cells with
    /// no content have no other holder), meters.
    pub view_radius_m: f32,
    /// The most tile colliders built per fixed step; the rest wait for the next step. 0 means no
    /// limit. A fixed count, so lockstep runs mark the same cells on the same steps.
    pub max_tiles_per_step: u32,
}

impl Default for TerrainTileCollision {
    fn default() -> Self {
        Self {
            tile_m: 128.0,
            view_radius_m: 192.0,
            max_tiles_per_step: 16,
        }
    }
}

impl TerrainTileCollision {
    /// Whether tiles of `tile_m` and cells of `cell_m` line up: one size is a whole multiple of the
    /// other (within a millimeter). Misaligned sizes work, but a cell then holds tiles that reach into
    /// its neighbors (`stream_terrain_misaligned`).
    pub fn aligned_with(tile_m: f64, cell_m: f64) -> bool {
        if !(tile_m.is_finite() && cell_m.is_finite() && tile_m > 0.0 && cell_m > 0.0) {
            return false;
        }
        let (big, small) = if tile_m >= cell_m {
            (tile_m, cell_m)
        } else {
            (cell_m, tile_m)
        };
        let ratio = big / small;
        (ratio - ratio.round()).abs() * small < 1.0e-3
    }
}

/// The cell grid of an open streamed level and the cells that have content (spec 78 § 13.1).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamLevelLayout {
    /// The level's cell grid.
    pub grid: crate::stream_grid::StreamGrid,
    /// Lowest and highest cell of the world, inclusive per axis.
    pub bounds: [CellCoord; 2],
    /// The cells with a record (content); every other cell in the bounds has none.
    pub records: BTreeSet<CellCoord>,
}

/// The layouts of the open streamed levels, for hooks that map cells to tiles of their own (terrain
/// collision tiles, spec 78 § 13.1). `regia_world` keeps it in step with the open levels.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct StreamLevelLayouts(pub BTreeMap<crate::level::LevelId, StreamLevelLayout>);

/// An HLOD node's voxel proxy of the terrain scatter under it (ADR-3253, spec 78 § 11.6), on its own
/// entity beside the node's [`HlodProxyMesh`] entity (a node may have either or both). The renderer's
/// side loads the cooked voxel proxy and draws its finest level as voxel boxes; it has no collision
/// and no nav. Presentation only: not reflected, never in a state hash.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct HlodProxyVoxels {
    /// The tree node.
    pub node: u32,
    /// The cooked voxel proxy (`regia_render::VoxelFarProxy`), as a guid.
    pub proxy: regia_core::id::Guid,
    /// Bytes of the cooked proxy, for logs and budgets.
    pub bytes: u64,
    /// Edge of one voxel at the drawn level, meters.
    pub voxel_size_m: f32,
}

/// Which HLOD voxel proxies are ready to draw, by tree node (ADR-3253): the renderer's side marks a
/// node once its voxel proxy loaded. A node with both a mesh and a voxel proxy draws when both are
/// resident ([`HlodProxyResidency`] holds the mesh side).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct HlodVoxelResidency {
    /// Resident nodes.
    pub resident: BTreeSet<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> LevelStreaming {
        LevelStreaming {
            enabled: true,
            ..LevelStreaming::default()
        }
    }

    fn level(n: u128) -> LevelId {
        LevelId(regia_core::id::Guid::from_u128(n))
    }

    #[test]
    fn default_settings_are_valid() {
        enabled().validate().expect("valid");
    }

    #[test]
    fn validation_names_the_field() {
        type Edit = Box<dyn Fn(&mut LevelStreaming)>;
        let cases: Vec<(&str, Edit)> = vec![
            ("cell_size_m", Box::new(|s| s.cell_size_m = 4.0)),
            ("cell_size_m", Box::new(|s| s.cell_size_m = f64::NAN)),
            ("unload_radius_m", Box::new(|s| s.unload_radius_m = 1.0)),
            (
                "max_entity_extent_cells",
                Box::new(|s| s.max_entity_extent_cells = 0.5),
            ),
            (
                "hlod",
                Box::new(|s| {
                    s.hlod = Some(HlodSettings {
                        merge_factor: 1,
                        ..HlodSettings::default()
                    });
                }),
            ),
            ("origin.cell_size_m", Box::new(|s| s.origin.cell_size_m = 0.5)),
            (
                "origin.cell_size_m",
                Box::new(|s| {
                    s.origin.cell_size_m = 20_000_000.0;
                    s.origin.threshold_m = 20_000_000.0;
                }),
            ),
            ("origin.threshold_m", Box::new(|s| s.origin.threshold_m = 1.0)),
            (
                "max_inflight",
                Box::new(|s| {
                    s.budgets = Some(StreamBudgets {
                        max_inflight: 0,
                        ..StreamBudgets::default()
                    });
                }),
            ),
            ("version", Box::new(|s| s.version = LEVEL_STREAMING_VERSION + 1)),
        ];
        for (field, edit) in cases {
            let mut s = enabled();
            edit(&mut s);
            let err = s.validate().expect_err(field);
            assert!(err.contains(field), "{field}: {err}");
        }
    }

    #[test]
    fn budget_columns_match_the_spec_table() {
        let desktop = StreamBudgets::for_kind(StreamPlatform::Desktop);
        assert_eq!(desktop.memory_budget_bytes, 2 * GIB);
        assert_eq!(desktop.max_inflight, 8);
        let android = StreamBudgets::for_kind(StreamPlatform::Android);
        assert_eq!(android.memory_budget_bytes, 512 * MIB);
        assert_eq!(android.max_loads_per_step, 2);
        let server = StreamBudgets::for_kind(StreamPlatform::Server);
        assert_eq!(server.hlod_budget_bytes, 0);
        assert_eq!(server.max_inflight, 16);
        assert_eq!(StreamBudgets::default(), desktop);
    }

    #[test]
    fn the_derived_threshold_switches_a_leaf_inside_the_load_radius() {
        let settings = HlodSettings::default();
        // A 128 m cell has a bounding radius of about 91 m.
        let config = settings.config(512.0, 91.0);
        let switch = config.switch_distance(91.0);
        assert!((switch - 0.9 * 512.0).abs() < 1.0e-6, "{switch}");
        let explicit = HlodSettings {
            screen_size_threshold: Some(0.25),
            ..settings
        };
        assert_eq!(explicit.config(512.0, 91.0).screen_size_threshold, 0.25);
        // 0.34 is the derived threshold for the defaults, not a leaf radius.
        let derived = settings.config(512.0, 91.0).screen_size_threshold;
        assert!(
            (derived - 0.34).abs() < 0.01,
            "a 91 m leaf derives about 0.34, got {derived}"
        );
        let mistaken = settings.config(512.0, 0.34).screen_size_threshold;
        assert!(
            mistaken < 0.01,
            "0.34 as a radius is not the default leaf, got {mistaken}"
        );
    }

    #[test]
    fn omitted_hlod_fade_is_zero_on_every_platform() {
        assert_eq!(StreamTuning::default().hlod_fade_frames, None);
        assert_eq!(StreamTuning::default().fade_frames(), 0);
    }

    #[test]
    fn a_cell_is_ready_when_every_gate_of_the_scope_marked_it() {
        let mut gates = StreamGates::default();
        let (l, c) = (level(1), CellCoord::new(1, 2));
        assert!(
            gates.is_ready(l, c, Readiness::Visible),
            "no gates, nothing to wait for"
        );
        let physics = gates.register("physics", Readiness::SimReady);
        let render = gates.register("render", Readiness::Visible);
        assert_eq!(gates.register("physics", Readiness::SimReady), physics);
        assert_eq!(gates.missing(l, c), vec!["physics", "render"]);
        gates.set_ready(render, l, c);
        assert!(!gates.is_ready(l, c, Readiness::SimReady));
        assert!(
            !gates.is_ready(l, c, Readiness::Visible),
            "Visible needs the sim gates too"
        );
        gates.set_ready(physics, l, c);
        assert!(gates.is_ready(l, c, Readiness::SimReady));
        assert!(gates.is_ready(l, c, Readiness::Visible));
        gates.clear(render, l, c);
        assert!(gates.is_ready(l, c, Readiness::SimReady));
        assert!(!gates.is_ready(l, c, Readiness::Visible));
        gates.forget_cell(l, c);
        assert_eq!(gates.missing(l, c), vec!["physics", "render"]);
    }

    #[test]
    fn awaiting_lists_spawned_cells_until_the_gate_marks_them() {
        let mut gates = StreamGates::default();
        let gate = gates.register("physics", Readiness::SimReady);
        let (l, a, b) = (level(1), CellCoord::new(0, 0), CellCoord::new(1, 0));
        gates.note_spawned(l, b);
        gates.note_spawned(l, a);
        assert_eq!(gates.awaiting(gate).collect::<Vec<_>>(), vec![(l, a), (l, b)]);
        gates.set_ready(gate, l, a);
        assert_eq!(gates.awaiting(gate).collect::<Vec<_>>(), vec![(l, b)]);
        gates.forget_level(l);
        assert_eq!(gates.awaiting(gate).count(), 0);
    }

    #[test]
    fn only_gates_that_cover_empty_cells_hold_an_empty_cell_back() {
        let mut gates = StreamGates::default();
        let (l, c) = (level(1), CellCoord::new(5, 5));
        gates.register("physics", Readiness::SimReady);
        assert!(gates.is_empty_cell_ready(l, c, Readiness::Visible));
        let terrain = gates.register_covering_empty("terrain_collision", Readiness::SimReady);
        assert!(!gates.is_empty_cell_ready(l, c, Readiness::SimReady));
        gates.set_ready(terrain, l, c);
        assert!(gates.is_empty_cell_ready(l, c, Readiness::SimReady));
    }

    #[test]
    fn an_open_gate_passes_every_cell_of_its_level_only() {
        let mut gates = StreamGates::default();
        let (a, b, c) = (level(1), level(2), CellCoord::new(3, 4));
        let terrain = gates.register_covering_empty("terrain_collision", Readiness::SimReady);
        gates.note_spawned(a, c);
        gates.open_level(terrain, a);
        assert!(gates.is_ready(a, c, Readiness::SimReady));
        assert!(gates.is_empty_cell_ready(a, c, Readiness::Visible));
        assert!(!gates.is_ready(b, c, Readiness::SimReady));
        assert_eq!(gates.awaiting(terrain).count(), 0);
        gates.forget_level(a);
        assert!(!gates.is_ready(a, c, Readiness::SimReady));
    }

    #[test]
    fn a_scoped_gate_holds_back_only_the_levels_that_claimed_it() {
        let mut gates = StreamGates::default();
        let (with, without, c) = (level(1), level(2), CellCoord::new(0, 0));
        let terrain = gates.register_for_claimed_levels(TERRAIN_COLLISION_GATE, Readiness::SimReady);
        gates.claim_level(terrain, with);
        assert!(!gates.is_ready(with, c, Readiness::SimReady));
        assert!(!gates.is_empty_cell_ready(with, c, Readiness::SimReady));
        assert!(
            gates.is_ready(without, c, Readiness::SimReady),
            "no terrain, no wait"
        );
        assert!(gates.missing(without, c).is_empty());
        gates.open_level(terrain, with);
        assert!(gates.is_ready(with, c, Readiness::SimReady));
        gates.forget_level(with);
        assert!(
            gates.is_ready(with, c, Readiness::SimReady),
            "the claim went with the level"
        );
    }

    #[test]
    fn a_gate_opened_everywhere_passes_every_level() {
        let mut gates = StreamGates::default();
        let (a, c) = (level(1), CellCoord::new(0, 0));
        let terrain = gates.register_for_claimed_levels(TERRAIN_COLLISION_GATE, Readiness::SimReady);
        gates.claim_level(terrain, a);
        gates.open_everywhere(terrain);
        assert!(gates.is_ready(a, c, Readiness::SimReady));
    }

    #[test]
    fn readiness_spellings_round_trip() {
        for r in [Readiness::SimReady, Readiness::Visible] {
            assert_eq!(Readiness::parse(r.as_str()), Some(r));
        }
        assert_eq!(Readiness::parse("ready"), None);
    }

    #[test]
    fn settings_round_trip_through_ron_with_defaults_for_missing_fields() {
        let text = ron::to_string(&enabled()).expect("ron");
        let back: LevelStreaming = ron::from_str(&text).expect("back");
        assert_eq!(back, enabled());
        let sparse: LevelStreaming = ron::from_str("(enabled: true, cell_size_m: 64.0)").expect("sparse");
        assert_eq!(sparse.cell_size_m, 64.0);
        assert_eq!(sparse.unload_radius_m, 640.0);
        assert_eq!(sparse.tuning, StreamTuning::default());
    }

    #[test]
    fn the_streamed_replicated_table_answers_per_level_and_counts_revisions() {
        let mut table = StreamedReplicated::default();
        assert!(table.is_empty());
        table.insert_level(
            level(7),
            [(100, CellCoord::new(1, 2)), (101, CellCoord::new(0, 0))],
        );
        assert_eq!(table.cell_of(7, 100), Some(CellCoord::new(1, 2)));
        assert_eq!(table.cell_of(7, 102), None);
        assert_eq!(table.cell_of(8, 100), None, "another level's guid is not listed");
        assert_eq!(table.len(), 2);
        let revision = table.revision();
        table.insert_level(level(7), [(102, CellCoord::new(3, 3))]);
        assert_eq!(table.cell_of(7, 100), None, "replaced, not merged");
        assert!(table.revision() > revision);
        assert!(table.remove_level(level(7)));
        assert!(!table.remove_level(level(7)));
        assert!(!table.has_level(7) && table.is_empty());
    }

    /// ADR-2435: two holders of one name keep it registered until both let go; a release with no
    /// hold changes nothing.
    #[test]
    fn rebase_holds_count_owners_of_one_blocker_name() {
        let mut world = World::new();
        acquire_rebase_hold(&mut world, TERRAIN_STROKE_BLOCKER);
        acquire_rebase_hold(&mut world, TERRAIN_STROKE_BLOCKER);
        acquire_rebase_hold(&mut world, VEHICLE_CONTACT_BLOCKER);
        let names = |world: &World| {
            world
                .resource::<RebaseBlockers>()
                .0
                .iter()
                .copied()
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&world), [TERRAIN_STROKE_BLOCKER, VEHICLE_CONTACT_BLOCKER]);
        assert_eq!(world.resource::<RebaseHolds>().count(TERRAIN_STROKE_BLOCKER), 2);
        assert!(release_rebase_hold(&mut world, TERRAIN_STROKE_BLOCKER));
        assert_eq!(names(&world), [TERRAIN_STROKE_BLOCKER, VEHICLE_CONTACT_BLOCKER]);
        assert!(release_rebase_hold(&mut world, TERRAIN_STROKE_BLOCKER));
        assert!(release_rebase_hold(&mut world, VEHICLE_CONTACT_BLOCKER));
        assert!(names(&world).is_empty());
        assert!(!release_rebase_hold(&mut world, VEHICLE_CONTACT_BLOCKER));
        // A host's own blocker is not touched by the engine's holds.
        world.resource_mut::<RebaseBlockers>().0.insert("host_store");
        acquire_rebase_hold(&mut world, VEHICLE_CONTACT_BLOCKER);
        assert!(release_rebase_hold(&mut world, VEHICLE_CONTACT_BLOCKER));
        assert_eq!(names(&world), ["host_store"]);
        assert_eq!(world.resource::<RebaseHolds>().iter().count(), 0);
    }
}
