// SPDX-License-Identifier: MPL-2.0
//! Tiled, multi-scale and anchored runtime generation (`docs/specs/66-tiled-and-runtime-generation.md`).
//!
//! - **Tiles** (§ 2): a generator with [`TileSettings`] runs per world-aligned cell over the cell
//!   grown by a halo; every output belongs to the cell containing its final XZ position
//!   ([`run_tiled`], [`run_cell`]).
//! - **Tiers** (§ 3): `tiles.tier` nodes put the nodes downstream of them on coarser or finer cell
//!   grids; data flows coarse to fine only ([`TierPlan`]).
//! - **Anchors** (§ 4): [`GeneratorTiles::step`] is the deterministic planning half of runtime
//!   generation around [`AnchorView`]s: wanted and released cells with hysteresis, priority, a view
//!   gate, start and apply budgets. [`ProcGenTiles`] holds one [`GeneratorTiles`] per generator.
//!
//! Cell indices and distances are the world streaming grid's (`regia_scene::stream_grid`, ADR-1566):
//! cell `(i, j)` covers `[i·c, (i+1)·c) × [j·c, (j+1)·c)`. The scheduler itself is this module's (spec
//! 66 § 1): per-tier radii, the view gate and frame budgets are not in the world streaming planner.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use regia_app::prelude::*;
use regia_core::math::{Frustum, Vec3};
use regia_render::quality::QualityTier;
use regia_scene::stream_grid::StreamGrid;

use crate::data::{Bundle, Payload, ShapeData};
use crate::exec::{
    CancelToken, Evaluator, NodePlan, RunError, RunInputs, RunOutput, RunReport, SelfInfo, WorldSnapshot,
};
use crate::graph::{CompiledGraph, GraphError, GraphErrors};
use crate::seeds;
use crate::value::SettingValue;
use regia_scene::spawn::ReflectFormatVersion;

/// `format_version` of the saved [`ProcGenAnchor`] component (`procgen-anchor`).
pub const PROCGEN_ANCHOR_FORMAT_VERSION: u32 = 1;

/// Smallest base or tier cell, meters.
pub const MIN_CELL_M: f64 = 8.0;
/// Largest base or tier cell, meters.
pub const MAX_CELL_M: f64 = 4096.0;
/// Smallest default halo, meters.
pub const MIN_HALO_M: f64 = 4.0;
/// Width of the band inside the halo edge whose neighbors make a decision seam-risky, meters.
pub const SEAM_RISK_BAND_M: f32 = 1.0;

// ---------------------------------------------------------------------------------------------
// Grid math
// ---------------------------------------------------------------------------------------------

pub use regia_scene::stream_grid::CellCoord;

/// Tile math on the streaming grid's cell address (spec 66 § 1, ADR-1566): cells and distances come
/// from [`StreamGrid`], so a tile cell and a streaming cell of the same size are the same rectangle.
pub trait TileCell: Sized {
    /// The cell containing `(x, z)` (floor division: a point on a cell's max edge belongs to the
    /// next cell). Positions the grid refuses (not finite, past its range) saturate.
    fn containing(x: f64, z: f64, cell_m: f64) -> Self;
    /// The cell's half-open rectangle `[min_x, min_z, max_x, max_z]`.
    fn rect(self, cell_m: f64) -> [f64; 4];
    /// `cell_key = mix(i as u32, j as u32)` (spec 62 § 6.2): feeds per-run draws only.
    fn key(self) -> u32;
}

impl TileCell for CellCoord {
    fn containing(x: f64, z: f64, cell_m: f64) -> Self {
        StreamGrid::new(cell_m)
            .and_then(|g| g.cell_of(x, z))
            .unwrap_or_else(|_| Self::new((x / cell_m).floor() as i64, (z / cell_m).floor() as i64))
    }

    fn rect(self, cell_m: f64) -> [f64; 4] {
        // `cell_m` is validated by `TileSettings::validate`; a bad size only reaches here from a
        // direct call and gives the same arithmetic without the grid.
        match StreamGrid::new(cell_m) {
            Ok(g) => {
                let (lo, hi) = (g.cell_min(self), g.cell_max(self));
                [lo[0], lo[1], hi[0], hi[1]]
            }
            Err(_) => [
                self.x as f64 * cell_m,
                self.z as f64 * cell_m,
                (self.x as f64 + 1.0) * cell_m,
                (self.z as f64 + 1.0) * cell_m,
            ],
        }
    }

    fn key(self) -> u32 {
        seeds::cell_key(self.x as i32, self.z as i32)
    }
}

/// Distance in XZ from `p` to the nearest point of a rectangle (0 inside).
pub fn distance_to_rect(p: [f64; 2], r: [f64; 4]) -> f64 {
    let dx = (r[0] - p[0]).max(0.0).max(p[0] - r[2]);
    let dz = (r[1] - p[1]).max(0.0).max(p[1] - r[3]);
    (dx * dx + dz * dz).sqrt()
}

/// Distance in XZ from `p` to cell `c` ([`StreamGrid::distance_to_cell`]).
pub fn distance_to_cell(p: [f64; 2], c: CellCoord, cell_m: f64) -> f64 {
    StreamGrid::new(cell_m).map_or_else(
        |_| distance_to_rect(p, c.rect(cell_m)),
        |g| g.distance_to_cell(p[0], p[1], c),
    )
}

/// How many times [`cells_in_rect`] has built a cell list in this process. A second scheduler
/// step with the same anchors does not increment it.
pub fn cells_in_rect_calls() -> u64 {
    CELLS_IN_RECT_CALLS.load(Ordering::Relaxed)
}

static CELLS_IN_RECT_CALLS: AtomicU64 = AtomicU64::new(0);

/// Every cell of size `cell_m` touching the closed rectangle `r`, in `(x, z)` order.
pub fn cells_in_rect(r: [f64; 4], cell_m: f64) -> Vec<CellCoord> {
    CELLS_IN_RECT_CALLS.fetch_add(1, Ordering::Relaxed);
    let lo = CellCoord::containing(r[0], r[1], cell_m);
    let hi = CellCoord::containing(r[2], r[3], cell_m);
    let mut v = Vec::new();
    for x in lo.x..=hi.x {
        for z in lo.z..=hi.z {
            v.push(CellCoord::new(x, z));
        }
    }
    v
}

/// `r` grown by `m` on every side.
pub fn grow(r: [f64; 4], m: f64) -> [f64; 4] {
    [r[0] - m, r[1] - m, r[2] + m, r[3] + m]
}

/// Intersection of two rectangles (empty ones have `min > max`).
pub fn intersect(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])]
}

fn overlaps(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] <= b[2] && b[0] <= a[2] && a[1] <= b[3] && b[1] <= a[3]
}

/// Whether `(x, z)` lies in the half-open rectangle `[min, max)`.
pub fn in_half_open(r: [f64; 4], x: f64, z: f64) -> bool {
    x >= r[0] && x < r[2] && z >= r[1] && z < r[3]
}

/// Whether `v` is a power of two times `base` (k any integer) within `[MIN_CELL_M, MAX_CELL_M]`.
pub fn nests_with(base: f64, v: f64) -> bool {
    if !(MIN_CELL_M..=MAX_CELL_M).contains(&v) || base <= 0.0 {
        return false;
    }
    let ratio = v / base;
    let k = crate::detmath::log2(ratio).round();
    (crate::detmath::exp2(k) * base - v).abs() <= 1e-9 * v
}

// ---------------------------------------------------------------------------------------------
// Settings (part of the ProcGenerator v1 shape)
// ---------------------------------------------------------------------------------------------

/// Which entities are anchors (§ 4.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum AnchorSource {
    /// The primary view is an implicit anchor.
    #[default]
    PrimaryView,
    /// Entities with [`ProcGenAnchor`].
    Tagged,
    /// Both.
    Both,
}

/// Build only cells near the view frustum (§ 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct ViewGate {
    /// Cells within the frustum widened by this many degrees are built.
    pub build_margin_deg: f32,
    /// Resident cells outside the frustum widened by this many degrees are released (clamped to at
    /// least the build margin).
    pub release_margin_deg: f32,
}

impl Default for ViewGate {
    fn default() -> Self {
        Self {
            build_margin_deg: 10.0,
            release_margin_deg: 25.0,
        }
    }
}

/// Radii of one tier (§ 4.2), matched to the graph's tiers by `cell_m`.
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct TierReach {
    /// The tier's cell size, meters.
    pub cell_m: f32,
    /// Cells within this distance of an anchor are wanted.
    pub build_m: f32,
    /// Release beyond `build_m × release_factor` (clamped to ≥ 1.1).
    pub release_factor: f32,
    /// 0..1: how much facing raises priority.
    pub view_bias: f32,
    /// Build only cells near the view frustum.
    pub view_gate: Option<ViewGate>,
}

impl Default for TierReach {
    fn default() -> Self {
        Self {
            cell_m: 128.0,
            build_m: 256.0,
            release_factor: 1.25,
            view_bias: 0.5,
            view_gate: None,
        }
    }
}

impl TierReach {
    /// `release_factor` clamped to the hysteresis minimum.
    pub fn release_factor(&self) -> f32 {
        self.release_factor.max(1.1)
    }
}

/// Runtime generation around anchors (§ 4.2).
///
/// `#[reflect(Clone)]`: a reflected clone uses `Clone`, because `quality_reach` (`[f32; 4]`) has
/// none of its own and `snapshot_scene` (the editor's level save) would drop the whole
/// `ProcGenerator` (ADR-7287).
#[derive(Clone, Debug, PartialEq, Reflect)]
#[reflect(Clone)]
pub struct AnchorSettings {
    /// Which entities pull generation.
    pub anchors: AnchorSource,
    /// Build radius of tiers without a [`TierReach`] and of untiled `Anchored` generators.
    pub base_reach_m: f32,
    /// Per-tier radii.
    pub tiers: Vec<TierReach>,
    /// Cells entering `capturing` per frame (`None` = desktop 8, Android 4).
    pub max_new_per_frame: Option<u32>,
    /// Cells evaluating at once (`None` = desktop 4, Android 2).
    pub max_running: Option<u32>,
    /// Main-thread apply budget per frame (`None` = desktop 1.0, Android 0.5).
    pub apply_budget_ms: Option<f32>,
    /// Deterministic apply budget in elements per frame (tests and render-out).
    pub apply_budget_elements: Option<u32>,
    /// Cells' worth of instance buffers kept for reuse (0 disables pooling).
    pub pool_cells: u32,
    /// Resident memory cap per generator (`None` = desktop 512, Android 128).
    pub max_resident_mb: Option<u32>,
    /// Level load waits for the cells around the start anchor.
    pub wait_on_load: bool,
    /// Radius scale by quality (low, medium, high, ultra).
    pub quality_reach: [f32; 4],
}

impl Default for AnchorSettings {
    fn default() -> Self {
        Self {
            anchors: AnchorSource::PrimaryView,
            base_reach_m: 256.0,
            tiers: Vec::new(),
            max_new_per_frame: None,
            max_running: None,
            apply_budget_ms: None,
            apply_budget_elements: None,
            pool_cells: 64,
            max_resident_mb: None,
            wait_on_load: true,
            quality_reach: [0.5, 0.75, 1.0, 1.25],
        }
    }
}

impl AnchorSettings {
    /// Cells entering `capturing` per frame.
    pub fn max_new(&self) -> u32 {
        self.max_new_per_frame
            .unwrap_or(if cfg!(target_os = "android") { 4 } else { 8 })
    }

    /// Cells evaluating at once.
    pub fn max_running(&self) -> u32 {
        self.max_running
            .unwrap_or(if cfg!(target_os = "android") { 2 } else { 4 })
    }

    /// Apply budget, milliseconds.
    pub fn apply_ms(&self) -> f32 {
        self.apply_budget_ms
            .unwrap_or(if cfg!(target_os = "android") { 0.5 } else { 1.0 })
    }

    /// Resident memory cap, MiB.
    pub fn resident_mb(&self) -> u32 {
        self.max_resident_mb
            .unwrap_or(if cfg!(target_os = "android") { 128 } else { 512 })
    }

    /// The radius scale for a quality tier (ADR-1585: `MobileLow`, `MobileHigh`, `Low` → 0; `Deck`,
    /// `Medium` → 1; `High` → 2; `Ultra` → 3).
    pub fn quality_scale(&self, tier: QualityTier) -> f32 {
        self.quality_reach[quality_reach_index(tier)]
    }

    /// The reach of the tier with cell size `cell_m` (`None` = the whole tier or untiled).
    pub fn reach_for(&self, cell_m: Option<f64>) -> TierReach {
        if let Some(c) = cell_m
            && let Some(t) = self
                .tiers
                .iter()
                .find(|t| (f64::from(t.cell_m) - c).abs() <= 1e-6 * c)
        {
            return *t;
        }
        TierReach {
            cell_m: cell_m.unwrap_or(0.0) as f32,
            build_m: self.base_reach_m,
            release_factor: 1.25,
            ..TierReach::default()
        }
    }
}

/// Index into [`AnchorSettings::quality_reach`] for a renderer tier.
pub fn quality_reach_index(tier: QualityTier) -> usize {
    match tier {
        QualityTier::MobileLow | QualityTier::MobileHigh | QualityTier::Low | QualityTier::VeryLow => 0,
        QualityTier::Deck | QualityTier::Medium => 1,
        QualityTier::High => 2,
        QualityTier::Ultra => 3,
    }
}

/// Splits a generator into world-aligned cells (§ 2). Part of the `ProcGenerator` v1 shape.
#[derive(Clone, Debug, PartialEq, Reflect)]
pub struct TileSettings {
    /// Base cell size: a power of two in meters between 8 and 4096 (default 128).
    pub cell_m: f32,
    /// Halo; `None` = 2 × the graph's declared reach, at least 4 m.
    pub halo_m: Option<f32>,
    /// Runtime generation around anchors; required when the mode is `Anchored`.
    pub runtime: Option<AnchorSettings>,
}

impl Default for TileSettings {
    fn default() -> Self {
        Self {
            cell_m: 128.0,
            halo_m: None,
            runtime: None,
        }
    }
}

impl TileSettings {
    /// Checks the base cell size (`tile_cell_invalid`).
    ///
    /// # Errors
    ///
    /// `(code, message)` when `cell_m` is not a power of two within 8..=4096 m.
    pub fn validate(&self) -> Result<(), (String, String)> {
        let c = f64::from(self.cell_m);
        let pow2 = c.is_finite() && c >= 1.0 && (c as u64).is_power_of_two() && (c as u64) as f64 == c;
        if !pow2 || !(MIN_CELL_M..=MAX_CELL_M).contains(&c) {
            return Err((
                "tile_cell_invalid".to_string(),
                format!("cell_m {c} is not a power of two between 8 and 4096 m"),
            ));
        }
        if let Some(h) = self.halo_m
            && !(h.is_finite() && h >= 0.0)
        {
            return Err((
                "tile_cell_invalid".to_string(),
                format!("halo_m {h} must be finite and ≥ 0"),
            ));
        }
        Ok(())
    }

    /// The halo for a graph with this declared reach.
    pub fn halo(&self, reach_m: f64) -> f64 {
        self.halo_m.map_or((2.0 * reach_m).max(MIN_HALO_M), f64::from)
    }
}

/// Marks an entity whose position pulls anchored generation toward it (§ 4.1). Saved
/// (`procgen-anchor` v1).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
pub struct ProcGenAnchor {
    /// Multiplies every tier's build radius for this anchor (default 1).
    pub reach_scale: f32,
    /// Camera anchors: weight cells in front higher (default true).
    pub use_view_direction: bool,
    /// Always [`PROCGEN_ANCHOR_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for ProcGenAnchor {
    const CURRENT: u32 = PROCGEN_ANCHOR_FORMAT_VERSION;
}

impl Default for ProcGenAnchor {
    fn default() -> Self {
        Self {
            reach_scale: 1.0,
            use_view_direction: true,
            format_version: PROCGEN_ANCHOR_FORMAT_VERSION,
        }
    }
}

impl ProcGenAnchor {
    /// Upgrades an older saved shape (none exists: version 1 is the first).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        if self.format_version > PROCGEN_ANCHOR_FORMAT_VERSION {
            return Err(format!(
                "procgen-anchor format_version {} is newer than {PROCGEN_ANCHOR_FORMAT_VERSION}",
                self.format_version
            ));
        }
        self.format_version = PROCGEN_ANCHOR_FORMAT_VERSION;
        Ok(())
    }
}

/// The shape fingerprint of the saved `procgen-anchor` component (`cargo xtask check-formats`).
pub fn procgen_anchor_schema_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    match ProcGenAnchor::type_info() {
        TypeInfo::Struct(s) => format!(
            "ProcGenAnchor({})",
            s.iter().map(|f| f.name()).collect::<Vec<_>>().join(",")
        ),
        _ => String::new(),
    }
}

/// `procgen-anchor` migration sources (none: version 1 is the first).
pub fn procgen_anchor_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Field names of the tile settings types, for the `procgen-generator` fingerprint.
pub fn tile_settings_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    fn names(info: &TypeInfo) -> String {
        match info {
            TypeInfo::Struct(s) => s.iter().map(|f| f.name()).collect::<Vec<_>>().join(","),
            TypeInfo::Enum(e) => e.iter().map(|v| v.name()).collect::<Vec<_>>().join("|"),
            _ => String::new(),
        }
    }
    format!(
        "TileSettings({});AnchorSettings({});AnchorSource({});TierReach({});ViewGate({})",
        names(TileSettings::type_info()),
        names(AnchorSettings::type_info()),
        names(AnchorSource::type_info()),
        names(TierReach::type_info()),
        names(ViewGate::type_info())
    )
}

// ---------------------------------------------------------------------------------------------
// Tiers
// ---------------------------------------------------------------------------------------------

/// The `cell_m` setting of a `tiles.tier` node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TierSize {
    /// The generator's base cell.
    Base,
    /// One cell equal to the generator bounds.
    Whole,
    /// A cell size in meters (base × 2^k).
    Meters(f64),
}

impl TierSize {
    /// Parses `Base`, `Whole`, `Meters(m)` or a bare number.
    pub fn from_setting(v: &SettingValue) -> Option<Self> {
        match v {
            SettingValue::Ident(s) if s == "Base" => Some(Self::Base),
            SettingValue::Ident(s) if s == "Whole" => Some(Self::Whole),
            SettingValue::Str(s) if s == "Base" => Some(Self::Base),
            SettingValue::Str(s) if s == "Whole" => Some(Self::Whole),
            SettingValue::Call(n, args) if n == "Meters" && args.len() == 1 => {
                args[0].as_f64().filter(|m| *m > 0.0).map(Self::Meters)
            }
            other => other.as_f64().filter(|m| *m > 0.0).map(Self::Meters),
        }
    }
}

/// One tier of a graph: its index and cell size (`None` = whole).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TierInfo {
    /// Tier index: whole first (when present), then by decreasing cell size.
    pub index: u8,
    /// Cell size, meters; `None` for the whole tier.
    pub cell_m: Option<f64>,
}

/// Where a compiled node runs.
#[derive(Clone, Debug, PartialEq)]
enum NodeTier {
    /// Runs at this tier (index into [`TierPlan::tiers`]).
    At(u8),
    /// A source node (no inputs): runs in each consuming tier.
    Neutral(BTreeSet<u8>),
}

/// The tier analysis of a compiled graph (§ 3).
#[derive(Clone, Debug)]
pub struct TierPlan {
    /// The generator's base cell size.
    pub base_cell_m: f64,
    /// Tiers in index order (coarsest first).
    pub tiers: Vec<TierInfo>,
    /// The graph's declared reach (max over its nodes), meters.
    pub reach_m: f64,
    node_tier: Vec<NodeTier>,
    no_clip: Vec<bool>,
}

/// Sort key of a tier size: whole is coarsest.
fn size_key(s: Option<f64>) -> f64 {
    s.unwrap_or(f64::INFINITY)
}

impl TierPlan {
    /// Assigns every node a tier and checks nesting and edge direction.
    ///
    /// # Errors
    ///
    /// `tier_not_nested` for a tier size that is not base × 2^k within 8..=4096 m,
    /// `tier_upward_edge` for an edge from a finer tier into a coarser one.
    pub fn build(graph: &CompiledGraph, base_cell_m: f64) -> Result<Self, GraphErrors> {
        let n = graph.nodes.len();
        let mut errors = Vec::new();
        // Pass 1 (forward): sizes. `None` inside the Option = neutral / transparent.
        let mut size: Vec<Option<Option<f64>>> = vec![None; n];
        let no_clip: Vec<bool> = graph
            .nodes
            .iter()
            .map(|nd| nd.kind.id() == "tiles.no_clip")
            .collect();
        for (k, node) in graph.nodes.iter().enumerate() {
            let has_inputs = node.inputs.iter().any(|e| !e.is_empty());
            // Finest (smallest) size among non-neutral, non-transparent inputs.
            let mut finest: Option<Option<f64>> = None;
            for &(src, _) in node.inputs.iter().flatten() {
                if let Some(s) = size[src] {
                    finest = Some(match finest {
                        Some(f) if size_key(f) <= size_key(s) => f,
                        _ => s,
                    });
                }
            }
            if node.kind.id() == "tiles.tier" {
                let declared = match node.settings.get("cell_m").and_then(TierSize::from_setting) {
                    Some(TierSize::Whole) => None,
                    Some(TierSize::Meters(m)) => {
                        if !nests_with(base_cell_m, m) {
                            errors.push(GraphError::new(
                                Some(node.id),
                                "tier_not_nested",
                                format!(
                                    "tier cell {m} m is not the base cell {base_cell_m} m × 2^k within 8 to 4096 m"
                                ),
                            ));
                        }
                        Some(m)
                    }
                    _ => Some(base_cell_m),
                };
                if let Some(f) = finest
                    && size_key(f) < size_key(declared)
                {
                    errors.push(GraphError::new(
                        Some(node.id),
                        "tier_upward_edge",
                        format!(
                            "an edge brings {} data into the coarser {} tier",
                            tier_name(f),
                            tier_name(declared)
                        ),
                    ));
                }
                size[k] = Some(declared);
            } else if !has_inputs || no_clip[k] {
                size[k] = None;
            } else {
                // Fed only by tier-neutral nodes: neutral too (it adopts its consumers' tiers).
                size[k] = finest;
            }
        }
        // Pass 2 (backward): a neutral node runs in every tier that consumes it (a sink, or a node
        // whose consumers all are unplaced, runs at the base tier); `tiles.no_clip` takes the
        // finest tier among its consumers.
        let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (k, node) in graph.nodes.iter().enumerate() {
            for &(src, _) in node.inputs.iter().flatten() {
                consumers[src].push(k);
            }
        }
        let mut uses: Vec<Vec<Option<f64>>> = vec![Vec::new(); n];
        for k in (0..n).rev() {
            if size[k].is_some() {
                continue;
            }
            let mut set: Vec<Option<f64>> = Vec::new();
            for &c in &consumers[k] {
                let theirs: Vec<Option<f64>> = match size[c] {
                    Some(s) => vec![s],
                    None => uses[c].clone(),
                };
                for t in theirs {
                    if !set.iter().any(|x| same_size(*x, t)) {
                        set.push(t);
                    }
                }
            }
            if set.is_empty() {
                set.push(Some(base_cell_m));
            }
            if no_clip[k] {
                let own = set
                    .iter()
                    .copied()
                    .min_by(|a, b| size_key(*a).total_cmp(&size_key(*b)))
                    .unwrap_or(Some(base_cell_m));
                let input = graph.nodes[k]
                    .inputs
                    .iter()
                    .flatten()
                    .find_map(|&(src, _)| size[src]);
                if let Some(i) = input
                    && size_key(i) < size_key(own)
                {
                    errors.push(GraphError::new(
                        Some(graph.nodes[k].id),
                        "tier_upward_edge",
                        format!(
                            "an edge brings {} data into the coarser {} tier",
                            tier_name(i),
                            tier_name(own)
                        ),
                    ));
                }
                size[k] = Some(own);
            } else if consumers[k].is_empty() {
                size[k] = Some(set[0]);
            } else {
                uses[k] = set;
            }
        }
        if !errors.is_empty() {
            return Err(GraphErrors(errors));
        }
        // Tier indices: whole first, then by decreasing cell size.
        let mut sizes: Vec<Option<f64>> = Vec::new();
        for s in size.iter().flatten().chain(uses.iter().flatten()) {
            if !sizes.iter().any(|x| same_size(*x, *s)) {
                sizes.push(*s);
            }
        }
        if sizes.is_empty() {
            sizes.push(Some(base_cell_m));
        }
        sizes.sort_by(|a, b| size_key(*b).total_cmp(&size_key(*a)));
        let tiers: Vec<TierInfo> = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| TierInfo {
                index: i as u8,
                cell_m: *s,
            })
            .collect();
        let index_of = |s: Option<f64>| -> u8 {
            tiers
                .iter()
                .find(|t| same_size(t.cell_m, s))
                .map_or(0, |t| t.index)
        };
        let node_tier: Vec<NodeTier> = (0..n)
            .map(|k| match size[k] {
                Some(s) => NodeTier::At(index_of(s)),
                None => NodeTier::Neutral(uses[k].iter().map(|s| index_of(*s)).collect()),
            })
            .collect();
        let reach_m = graph
            .nodes
            .iter()
            .map(|nd| nd.kind.reach_m(&nd.settings))
            .fold(0.0f64, f64::max);
        Ok(Self {
            base_cell_m,
            tiers,
            reach_m,
            node_tier,
            no_clip,
        })
    }

    /// The tier of the compiled node at evaluation index `k` (`None` for a source node).
    pub fn node_tier(&self, k: usize) -> Option<u8> {
        match self.node_tier.get(k) {
            Some(NodeTier::At(t)) => Some(*t),
            _ => None,
        }
    }

    /// The tier with index `t`.
    pub fn tier(&self, t: u8) -> Option<TierInfo> {
        self.tiers.get(usize::from(t)).copied()
    }

    /// Whether the graph has more than one tier.
    pub fn is_multi_tier(&self) -> bool {
        self.tiers.len() > 1
    }

    /// The tier index with this cell size (`None` = whole).
    pub fn tier_for_size(&self, cell_m: Option<f64>) -> Option<u8> {
        self.tiers
            .iter()
            .find(|t| same_size(t.cell_m, cell_m))
            .map(|t| t.index)
    }

    /// Warnings for anchor settings against the tiers: `tier_reach_order` when a finer tier
    /// builds farther than a coarser one.
    pub fn check_reach(&self, settings: &AnchorSettings) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for w in self.tiers.windows(2) {
            let (coarse, fine) = (settings.reach_for(w[0].cell_m), settings.reach_for(w[1].cell_m));
            if w[0].cell_m.is_some() && fine.build_m > coarse.build_m {
                out.push((
                    "tier_reach_order".to_string(),
                    format!(
                        "tier {} builds to {} m, farther than its coarser tier {} ({} m)",
                        w[1].index, fine.build_m, w[0].index, coarse.build_m
                    ),
                ));
            }
        }
        out
    }

    /// The per-node plan of one tier's run, given the coarse data available for it.
    fn plan(&self, graph: &CompiledGraph, tier: u8, coarse: &CoarseInputs) -> Vec<NodePlan> {
        let n = graph.nodes.len();
        let mut plan = vec![NodePlan::Skip; n];
        let mut evaluated = vec![false; n];
        for (k, node) in graph.nodes.iter().enumerate() {
            match &self.node_tier[k] {
                NodeTier::At(t) if *t == tier => {
                    if self.no_clip[k] {
                        // A no-clip node fed by a coarser tier passes the coarse data whole.
                        let src = node.inputs.iter().flatten().next().copied();
                        if let Some((s, pin)) = src
                            && matches!(self.node_tier[s], NodeTier::At(st) if st < tier)
                        {
                            let b = coarse
                                .whole
                                .get(&s)
                                .and_then(|o| o.get(pin))
                                .cloned()
                                .unwrap_or_default();
                            plan[k] = NodePlan::Inject(vec![b]);
                            continue;
                        }
                    }
                    plan[k] = NodePlan::Eval;
                    evaluated[k] = true;
                }
                NodeTier::Neutral(set) if set.contains(&tier) => {
                    plan[k] = NodePlan::Eval;
                    evaluated[k] = true;
                }
                _ => {}
            }
        }
        for (k, node) in graph.nodes.iter().enumerate() {
            if !evaluated[k] {
                continue;
            }
            for &(src, _) in node.inputs.iter().flatten() {
                if matches!(self.node_tier[src], NodeTier::At(st) if st < tier)
                    && let Some(b) = coarse.clipped.get(&src)
                {
                    plan[src] = NodePlan::Inject(b.clone());
                }
            }
        }
        plan
    }

    /// Nodes of tier `t` whose outputs feed a finer tier (kept after a coarse cell's run).
    fn feeds_finer(&self, graph: &CompiledGraph, t: u8) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for (k, node) in graph.nodes.iter().enumerate() {
            let finer = match &self.node_tier[k] {
                NodeTier::At(kt) => *kt > t,
                NodeTier::Neutral(_) => false,
            };
            if !finer {
                continue;
            }
            for &(src, _) in node.inputs.iter().flatten() {
                if self.node_tier[src] == NodeTier::At(t) {
                    out.insert(src);
                }
            }
        }
        out
    }

    /// Coarse-tier nodes read by tier `t` (directly).
    fn reads_from(&self, graph: &CompiledGraph, t: u8) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for (k, node) in graph.nodes.iter().enumerate() {
            if self.node_tier[k] != NodeTier::At(t) {
                continue;
            }
            for &(src, _) in node.inputs.iter().flatten() {
                if matches!(self.node_tier[src], NodeTier::At(st) if st < t) {
                    out.insert(src);
                }
            }
        }
        out
    }
}

fn same_size(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
        _ => false,
    }
}

fn tier_name(s: Option<f64>) -> String {
    s.map_or_else(|| "whole".to_string(), |m| format!("{m} m"))
}

// ---------------------------------------------------------------------------------------------
// Cell runs
// ---------------------------------------------------------------------------------------------

/// One cell of one tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TileKey {
    /// Tier index (whole = 0 when the graph has a whole tier).
    pub tier: u8,
    /// The cell (`(0, 0)` for the whole tier).
    pub cell: CellCoord,
}

impl Default for TileKey {
    fn default() -> Self {
        Self {
            tier: 0,
            cell: CellCoord::new(0, 0),
        }
    }
}

/// Coarse data for one tier run: per coarse node, outputs clipped to the cell plus halo, and whole.
#[derive(Default)]
struct CoarseInputs {
    clipped: BTreeMap<usize, Vec<Bundle>>,
    whole: BTreeMap<usize, Vec<Bundle>>,
    dropped: u64,
}

/// What one cell produced (owned outputs only).
#[derive(Clone, Debug)]
pub struct CellOutput {
    /// The cell.
    pub key: TileKey,
    /// The owned half-open rectangle.
    pub rect: [f64; 4],
    /// The rectangle the run covered (owned rectangle plus halo, clipped to the bounds).
    pub run_rect: [f64; 4],
    /// Owned outputs, shared with the cell's spawned [`crate::world::ProcOutput`].
    pub output: Arc<RunOutput>,
    /// [`RunOutput::hash`] of the owned outputs.
    pub output_hash: [u8; 32],
    /// The run report.
    pub report: RunReport,
    /// Points whose decision used a neighbor within 1 m of the halo edge.
    pub seam_mismatch_risk: u64,
    /// Coarse points dropped by the clip to this cell plus halo.
    pub clip_dropped: u64,
    /// Every node's output, when the run kept debug data (`procgen.inspect { tier, cell }`).
    pub debug: Option<Arc<crate::world::DebugOutputs>>,
}

/// The cells of a tiled run.
#[derive(Clone, Debug)]
pub struct TiledRun {
    /// Per cell, in key order.
    pub cells: BTreeMap<TileKey, CellOutput>,
    /// The union of the cells' owned outputs (instances, prefabs and points concatenated in key
    /// order; tables and masks per cell, named `name@tier/i/j`).
    pub output: RunOutput,
    /// [`RunOutput::hash`] of `output`.
    pub output_hash: [u8; 32],
    /// The halo used, meters.
    pub halo_m: f64,
    /// The tier plan the cells ran under.
    pub plan: TierPlan,
    /// The generator bounds the cells ran over.
    pub bounds: ShapeData,
    /// The generator seed and quality tier of the run: a re-run of single cells needs both unchanged.
    pub key: (u32, QualityTier),
}

/// A snapshot with the generator bounds cut to one cell's run rectangle.
struct CellSnapshot {
    inner: Arc<dyn WorldSnapshot>,
    self_info: Option<SelfInfo>,
}

impl WorldSnapshot for CellSnapshot {
    fn self_info(&self) -> Option<&SelfInfo> {
        self.self_info.as_ref()
    }
    fn ground(&self) -> Option<Arc<crate::data::GroundData>> {
        self.inner.ground()
    }
    fn entities(&self) -> &[crate::exec::SnapEntity] {
        self.inner.entities()
    }
    fn table(&self, path: &str) -> Option<&crate::data::Table> {
        self.inner.table(path)
    }
    fn generator_export(&self, generator: &str, name: &str) -> Option<crate::data::DataItem> {
        self.inner.generator_export(generator, name)
    }
    fn mask_image(&self, path: &str) -> Option<&crate::exec::MaskImage> {
        self.inner.mask_image(path)
    }
    fn mesh(&self, path: &str) -> Option<Arc<crate::data::MeshData>> {
        self.inner.mesh(path)
    }
    fn rays(&self, node: u32) -> &[crate::exec::RayHitRecord] {
        self.inner.rays(node)
    }
    fn colliders(&self, node: u32) -> &[crate::exec::ColliderBox] {
        self.inner.colliders(node)
    }
    fn painted(&self) -> Option<&crate::data::PointSet> {
        self.inner.painted()
    }
    fn road_profile(&self, path: &str) -> Option<&crate::road::RoadProfile> {
        self.inner.road_profile(path)
    }
    fn biome(&self) -> Option<Arc<crate::biome::BiomeWorld>> {
        self.inner.biome()
    }
    fn self_items(&self) -> Vec<crate::data::DataItem> {
        self.inner.self_items()
    }
    fn content_hash(&self) -> Option<[u8; 32]> {
        self.inner.content_hash()
    }
}

/// The XZ rectangle of a shape's world AABB.
pub fn bounds_rect(bounds: &ShapeData) -> [f64; 4] {
    let (lo, hi) = bounds.world_aabb();
    [
        f64::from(lo[0]),
        f64::from(lo[2]),
        f64::from(hi[0]),
        f64::from(hi[2]),
    ]
}

/// Every cell of the tier that intersects the generator bounds, in key order.
pub fn tier_cells(plan: &TierPlan, tier: u8, bounds: &ShapeData) -> Vec<TileKey> {
    match plan.tier(tier).and_then(|t| t.cell_m) {
        None => vec![TileKey {
            tier,
            cell: CellCoord::new(0, 0),
        }],
        Some(c) => cells_in_rect(bounds_rect(bounds), c)
            .into_iter()
            .map(|cell| TileKey { tier, cell })
            .collect(),
    }
}

/// The owned rectangle of a cell (`None`: the whole tier owns everything).
pub fn owned_rect(plan: &TierPlan, key: TileKey) -> Option<[f64; 4]> {
    plan.tier(key.tier)
        .and_then(|t| t.cell_m)
        .map(|c| key.cell.rect(c))
}

/// Drops every output whose final XZ position is outside the half-open rectangle `r`.
pub fn keep_owned(output: &mut RunOutput, r: [f64; 4]) {
    for set in &mut output.instance_sets {
        set.instances
            .retain(|i| in_half_open(r, f64::from(i.translation[0]), f64::from(i.translation[2])));
    }
    output.instance_sets.retain(|s| !s.instances.is_empty());
    output
        .prefabs
        .retain(|p| in_half_open(r, f64::from(p.translation[0]), f64::from(p.translation[2])));
    for p in output.points.values_mut() {
        let keep: Vec<usize> = (0..p.len())
            .filter(|&i| in_half_open(r, f64::from(p.position[i][0]), f64::from(p.position[i][2])))
            .collect();
        *p = p.select(&keep);
    }
}

/// Points of `b` inside the half-open rectangle `r`; other payloads whole. Returns the drop count.
fn clip_bundle(b: &Bundle, r: [f64; 4]) -> (Bundle, u64) {
    let mut dropped = 0u64;
    let items = b
        .0
        .iter()
        .map(|item| match &item.payload {
            Payload::Points(p) => {
                let keep: Vec<usize> = (0..p.len())
                    .filter(|&i| in_half_open(r, f64::from(p.position[i][0]), f64::from(p.position[i][2])))
                    .collect();
                dropped += (p.len() - keep.len()) as u64;
                item.with_payload(Payload::Points(p.select(&keep)))
            }
            _ => item.clone(),
        })
        .collect();
    (Bundle(items), dropped)
}

/// A cell run's owned output and the outputs it keeps for finer tiers.
type CellRun = Result<(CellOutput, BTreeMap<usize, Vec<Bundle>>), RunError>;

/// Coarse cell outputs kept for finer tiers: per coarse key, per node index, the output bundles.
type CoarseStore = BTreeMap<TileKey, BTreeMap<usize, Vec<Bundle>>>;

/// Runs one cell (§ 2) with the coarse data `store` holds for it; returns the owned output and
/// the outputs of this tier's nodes that feed finer tiers.
fn run_cell_inner(job: &TileJob<'_>, key: TileKey, store: &CoarseStore, cancel: &CancelToken) -> CellRun {
    let TileJob {
        evaluator: eval,
        graph,
        plan,
        inputs,
        bounds,
        halo_m: halo,
    } = *job;
    let tier = plan.tier(key.tier).unwrap_or(TierInfo {
        index: key.tier,
        cell_m: Some(plan.base_cell_m),
    });
    let _span =
        tracing::info_span!("procgen.cell", tier = key.tier, i = key.cell.x, j = key.cell.z).entered();
    let brect = bounds_rect(bounds);
    let owned = tier.cell_m.map(|c| key.cell.rect(c));
    let run_rect = owned.map_or(brect, |r| intersect(grow(r, halo), brect));
    let fine_rect = owned.map_or(grow(brect, halo), |r| grow(r, halo));

    // Coarse data: from the coarse cell containing this one, clipped to this cell plus halo.
    let mut coarse = CoarseInputs::default();
    for src in plan.reads_from(graph, key.tier) {
        let Some(NodeTier::At(st)) = plan.node_tier.get(src).cloned() else {
            continue;
        };
        let ckey = coarse_key(plan, st, key);
        let Some(outs) = store.get(&ckey).and_then(|m| m.get(&src)) else {
            continue;
        };
        let mut clipped = Vec::with_capacity(outs.len());
        for b in outs {
            let (c, d) = clip_bundle(b, fine_rect);
            coarse.dropped += d;
            clipped.push(c);
        }
        coarse.clipped.insert(src, clipped);
        coarse.whole.insert(src, outs.clone());
    }
    let node_plan = plan.plan(graph, key.tier, &coarse);

    let self_info = inputs.world.self_info().cloned().map(|mut s| {
        s.bounds.clip_xz = Some([
            run_rect[0] as f32,
            run_rect[1] as f32,
            run_rect[2] as f32,
            run_rect[3] as f32,
        ]);
        s
    });
    let mut cell_inputs = inputs.clone();
    cell_inputs.world = Arc::new(CellSnapshot {
        inner: inputs.world.clone(),
        self_info,
    });
    cell_inputs.tier_index = u32::from(key.tier);
    cell_inputs.cell = tier.cell_m.map(|_| (key.cell.x as i32, key.cell.z as i32));
    cell_inputs.run_rect = owned.map(|_| {
        [
            run_rect[0] as f32,
            run_rect[1] as f32,
            run_rect[2] as f32,
            run_rect[3] as f32,
        ]
    });
    cell_inputs.tile = tier
        .cell_m
        .zip(owned)
        .map(|(cell_m, rect)| crate::exec::TileFrame { cell_m, rect });
    let (mut result, outs) = eval.run_planned(graph, &cell_inputs, cancel, &node_plan)?;
    if let Some(r) = owned {
        keep_owned(&mut result.output, r);
    }
    let output_hash = result.output.hash();
    let debug = result.debug.take().map(Arc::new);
    let seam = result
        .report
        .nodes
        .iter()
        .filter_map(|l| l.counters.get("seam_mismatch_risk"))
        .sum();
    let keep: BTreeMap<usize, Vec<Bundle>> = plan
        .feeds_finer(graph, key.tier)
        .into_iter()
        .filter_map(|k| outs.get(k).map(|o| (k, o.clone())))
        .collect();
    Ok((
        CellOutput {
            key,
            rect: owned.unwrap_or(brect),
            run_rect,
            output: Arc::new(result.output),
            output_hash,
            report: result.report,
            seam_mismatch_risk: seam,
            clip_dropped: coarse.dropped,
            debug,
        },
        keep,
    ))
}

/// The cell of coarser tier `coarse_tier` containing `key`'s cell.
fn coarse_key(plan: &TierPlan, coarse_tier: u8, key: TileKey) -> TileKey {
    let fine = plan.tier(key.tier).and_then(|t| t.cell_m);
    let coarse = plan.tier(coarse_tier).and_then(|t| t.cell_m);
    match (fine, coarse) {
        (Some(f), Some(c)) => {
            let r = key.cell.rect(f);
            TileKey {
                tier: coarse_tier,
                cell: CellCoord::containing((r[0] + r[2]) * 0.5, (r[1] + r[3]) * 0.5, c),
            }
        }
        _ => TileKey {
            tier: coarse_tier,
            cell: CellCoord::new(0, 0),
        },
    }
}

/// A tiled run's fixed inputs.
pub struct TileJob<'a> {
    /// The evaluator (with or without a cache).
    pub evaluator: &'a Evaluator,
    /// The compiled graph.
    pub graph: &'a CompiledGraph,
    /// Its tier plan.
    pub plan: &'a TierPlan,
    /// Run inputs (snapshot over the whole bounds, seed, quality, limits).
    pub inputs: &'a RunInputs,
    /// The generator's world bounds.
    pub bounds: &'a ShapeData,
    /// The halo, meters.
    pub halo_m: f64,
}

/// Runs one cell standalone: coarser cells it reads from are run first (and discarded).
///
/// # Errors
///
/// The first failing run's [`RunError`].
pub fn run_cell(job: &TileJob<'_>, key: TileKey, cancel: &CancelToken) -> Result<CellOutput, RunError> {
    let mut store = CoarseStore::new();
    // Coarse chain: every coarser tier's containing cell, coarsest first.
    for t in 0..key.tier {
        let ck = coarse_key(job.plan, t, key);
        if job.plan.feeds_finer(job.graph, t).is_empty() {
            continue;
        }
        let (_, keep) = run_cell_inner(job, ck, &store, cancel)?;
        store.insert(ck, keep);
    }
    run_cell_inner(job, key, &store, cancel).map(|(c, _)| c)
}

/// Runs every cell of every tier over the bounds, coarse tiers first, `threads` cells of a tier at
/// once (1 = sequential). The result does not depend on `threads`.
///
/// # Errors
///
/// The first failing cell's [`RunError`] in key order, or the merge's `attr_type_conflict` when
/// two cells disagree on a point attribute's type.
pub fn run_tiled(job: &TileJob<'_>, threads: usize, cancel: &CancelToken) -> Result<TiledRun, RunError> {
    let mut store = CoarseStore::new();
    let mut cells = BTreeMap::new();
    for t in &job.plan.tiers {
        let keys = tier_cells(job.plan, t.index, job.bounds);
        let results: Vec<CellRun> = if threads <= 1 {
            keys.iter()
                .map(|k| run_cell_inner(job, *k, &store, cancel))
                .collect()
        } else {
            let next = AtomicUsize::new(0);
            let slots: Vec<StdMutex<Option<_>>> = keys.iter().map(|_| StdMutex::new(None)).collect();
            std::thread::scope(|scope| {
                for _ in 0..threads.min(keys.len().max(1)) {
                    scope.spawn(|| {
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(k) = keys.get(i) else {
                                break;
                            };
                            let r = run_cell_inner(job, *k, &store, cancel);
                            if let Some(slot) = slots.get(i)
                                && let Ok(mut s) = slot.lock()
                            {
                                *s = Some(r);
                            }
                        }
                    });
                }
            });
            slots
                .into_iter()
                .map(|s| {
                    s.into_inner().ok().flatten().unwrap_or(Err(RunError::Node {
                        node: 0,
                        code: "procgen_run_failed".into(),
                        message: "a cell worker ended without a result".into(),
                    }))
                })
                .collect()
        };
        for r in results {
            let (cell, keep) = r?;
            if !keep.is_empty() {
                store.insert(cell.key, keep);
            }
            cells.insert(cell.key, cell);
        }
    }
    let output = merge_cells(&cells)?;
    let output_hash = output.hash();
    Ok(TiledRun {
        cells,
        output,
        output_hash,
        halo_m: job.halo_m,
        plan: job.plan.clone(),
        bounds: *job.bounds,
        key: (job.inputs.gen_seed, job.inputs.quality),
    })
}

/// `keys` plus the cells of finer tiers that read a re-run coarse cell (they sit inside it).
fn with_finer_readers(job: &TileJob<'_>, keys: &[TileKey]) -> BTreeSet<TileKey> {
    let mut out: BTreeSet<TileKey> = keys.iter().copied().collect();
    for key in keys {
        if job.plan.feeds_finer(job.graph, key.tier).is_empty() {
            continue;
        }
        for t in job.plan.tiers.iter().filter(|t| t.index > key.tier) {
            for fine in tier_cells(job.plan, t.index, job.bounds) {
                if coarse_key(job.plan, key.tier, fine) == *key {
                    out.insert(fine);
                }
            }
        }
    }
    out
}

/// Re-runs `keys` (and the fine cells that read them) over a finished run and merges again: the
/// other cells keep their outputs, so an edit costs only the cells it touches (spec 66 § 2).
///
/// # Errors
///
/// The first failing run's [`RunError`].
pub fn rerun_cells(
    job: &TileJob<'_>,
    previous: &TiledRun,
    keys: &[TileKey],
    cancel: &CancelToken,
) -> Result<TiledRun, RunError> {
    let mut run = previous.clone();
    for key in with_finer_readers(job, keys) {
        run.cells.insert(key, run_cell(job, key, cancel)?);
    }
    run.output = merge_cells(&run.cells)?;
    run.output_hash = run.output.hash();
    Ok(run)
}

/// The union of cell outputs (see [`TiledRun::output`]).
///
/// # Errors
///
/// [`RunError`] with the code `attr_type_conflict` when two cells give the same point-set name an
/// attribute of a different type (see [`merge_outputs`]).
pub fn merge_cells<'a>(
    cells: impl IntoIterator<Item = (&'a TileKey, &'a CellOutput)>,
) -> Result<RunOutput, RunError> {
    merge_outputs(cells.into_iter().map(|(k, c)| (k, c.output.as_ref())))
}

/// The union of owned cell outputs in the given order: instances, prefabs and points concatenated,
/// tables and masks named `name@tier/i/j` (what [`TiledRun::output`] and tiled `.pgout` files hold).
///
/// Points from different cells of one tiled output share a schema; cells that give the same
/// point-set name an attribute of a different type fail the merge with `attr_type_conflict`
/// (naming both cells and the attribute) instead of silently dropping the second cell's points.
///
/// # Errors
///
/// [`RunError`] with the code `attr_type_conflict` when two cells disagree on an attribute's type.
pub fn merge_outputs<'a>(
    cells: impl IntoIterator<Item = (&'a TileKey, &'a RunOutput)>,
) -> Result<RunOutput, RunError> {
    let mut out = RunOutput::default();
    // The cell that first wrote each point-set name: the conflict error names both cells.
    let mut first_point_cell: BTreeMap<String, TileKey> = BTreeMap::new();
    for (k, o) in cells {
        for set in &o.instance_sets {
            out.set_for(set).instances.extend_from_slice(&set.instances);
        }
        out.prefabs.extend(o.prefabs.iter().cloned());
        for (name, p) in &o.points {
            let slot = out.points.entry(name.clone()).or_default();
            if let Err(attr) = slot.append(p) {
                return Err(RunError::Node {
                    node: 0,
                    code: "attr_type_conflict".to_string(),
                    message: format!(
                        "cells {} and {} give point set \"{name}\" an attribute \"{attr}\" of different types",
                        cell_name(first_point_cell.get(name).unwrap_or(k)),
                        cell_name(k),
                    ),
                });
            }
            first_point_cell.entry(name.clone()).or_insert_with(|| *k);
        }
        let suffix = format!("@{}/{}/{}", k.tier, k.cell.x, k.cell.z);
        for (name, t) in &o.tables {
            out.tables.insert(format!("{name}{suffix}"), t.clone());
        }
        for (name, m) in &o.masks {
            out.masks.insert(format!("{name}{suffix}"), m.clone());
        }
        for m in &o.meshes {
            crate::exec::GeneratedMesh::merge_into(&mut out.meshes, m.clone());
        }
        out.terrain_edits.extend(o.terrain_edits.iter().cloned());
        out.road_decals.extend_from_slice(&o.road_decals);
    }
    Ok(out)
}

/// A cell's name in the `attr_type_conflict` message: `tier 0 cell (1, -2)`.
fn cell_name(k: &TileKey) -> String {
    format!("tier {} cell ({}, {})", k.tier, k.cell.x, k.cell.z)
}

/// Cells (of every tier) whose halo-grown rectangle intersects the XZ rectangle `change` (§ 2
/// edits): what an edit there marks stale.
pub fn stale_cells(plan: &TierPlan, bounds: &ShapeData, halo: f64, change: [f64; 4]) -> Vec<TileKey> {
    let brect = bounds_rect(bounds);
    let mut out = Vec::new();
    for t in &plan.tiers {
        match t.cell_m {
            None => {
                if overlaps(grow(brect, halo), change) {
                    out.push(TileKey {
                        tier: t.index,
                        cell: CellCoord::new(0, 0),
                    });
                }
            }
            Some(c) => {
                for cell in cells_in_rect(intersect(grow(change, halo), brect), c) {
                    if overlaps(grow(cell.rect(c), halo), change) && overlaps(cell.rect(c), brect) {
                        out.push(TileKey { tier: t.index, cell });
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Scheduler (planning half)
// ---------------------------------------------------------------------------------------------

/// Cell states (§ 4.2a); [`TileState::name`] gives the agent strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TileState {
    /// Wanted, waiting for a start slot.
    Queued,
    /// Started this frame: the host captures its inputs.
    Capturing,
    /// Evaluating.
    Running,
    /// Finished, being applied within the apply budget.
    Applying,
    /// Fully applied and visible.
    Resident,
    /// Released: the host removes its outputs.
    Releasing,
    /// The run failed; stays until rebuilt or no longer wanted.
    Failed,
}

impl TileState {
    /// The agent string.
    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Capturing => "capturing",
            Self::Running => "running",
            Self::Applying => "applying",
            Self::Resident => "resident",
            Self::Releasing => "releasing",
            Self::Failed => "failed",
        }
    }

    /// Parses [`TileState::name`].
    pub fn from_name(s: &str) -> Option<Self> {
        [
            Self::Queued,
            Self::Capturing,
            Self::Running,
            Self::Applying,
            Self::Resident,
            Self::Releasing,
            Self::Failed,
        ]
        .into_iter()
        .find(|t| t.name() == s)
    }

    fn in_flight(self) -> bool {
        matches!(self, Self::Capturing | Self::Running)
    }
}

/// An anchor as the scheduler sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnchorView {
    /// World position.
    pub pos: Vec3,
    /// View direction (camera anchors with `use_view_direction`).
    pub view_dir: Option<Vec3>,
    /// The view frustum, for view-gated tiers.
    pub frustum: Option<Frustum>,
    /// Multiplies every build radius.
    pub reach_scale: f32,
}

impl AnchorView {
    /// An anchor at `pos` without view data.
    pub fn at(pos: Vec3) -> Self {
        Self {
            pos,
            view_dir: None,
            frustum: None,
            reach_scale: 1.0,
        }
    }
}

/// The per-frame apply budget.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ApplyBudget {
    /// Elements that may still be applied this frame (`None` = whole cells, the host times them).
    pub elements: Option<u64>,
}

/// One cell's scheduler entry.
#[derive(Clone, Debug, PartialEq)]
pub struct TileEntry {
    /// State.
    pub state: TileState,
    /// Priority at the last step (lower first).
    pub priority: f64,
    /// Step at which it was queued.
    pub queued_at: u64,
    /// Elements to apply (set when the run finished).
    pub elements: u64,
    /// Elements applied so far.
    pub applied: u64,
    /// Estimated resident bytes.
    pub bytes: u64,
    /// Run time, milliseconds (reported by the host).
    pub ms: f64,
    /// Cell output hash once finished.
    pub output_hash: Option<[u8; 32]>,
}

/// Counters of one generator's scheduler.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileCounters {
    /// Cells queued.
    pub requested: u64,
    /// Cells started.
    pub started: u64,
    /// Cells released.
    pub released: u64,
    /// Queued cells dropped before starting.
    pub dropped: u64,
    /// Running cells cancelled.
    pub cancelled: u64,
    /// Teleports detected.
    pub teleports: u64,
    /// Cells released early by the memory cap.
    pub memory_released: u64,
    /// Starts served from the pool.
    pub pool_hits: u64,
    /// Steps taken.
    pub steps: u64,
    /// Cells loaded from the baked file (§ 4.3 "Baked + anchors").
    pub cells_loaded: u64,
}

/// What one step decided; the host acts on it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StepPlan {
    /// Cells to capture and run (now `capturing`).
    pub start: Vec<TileKey>,
    /// Running cells to cancel (removed).
    pub cancel: Vec<TileKey>,
    /// Queued cells dropped.
    pub dropped: Vec<TileKey>,
    /// Cells now `releasing`: remove their outputs.
    pub release: Vec<TileKey>,
    /// Apply progress `(key, from, to)` in elements; a cell is visible when `to` reaches its count.
    pub apply: Vec<(TileKey, u64, u64)>,
    /// Cells that became resident this step.
    pub resident: Vec<TileKey>,
    /// The memory cap forced early releases.
    pub memory_capped: bool,
}

/// One tier as the scheduler sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SchedTier {
    /// Tier index.
    pub index: u8,
    /// Cell size (`None` = whole / untiled: one cell equal to the bounds).
    pub cell_m: Option<f64>,
}

/// The scheduler of one anchored generator (§ 4.2a, § 4.3).
#[derive(Clone, Debug, Default)]
pub struct GeneratorTiles {
    /// The generator bounds' XZ rectangle.
    pub bounds: [f64; 4],
    /// Tiers, coarsest first.
    pub tiers: Vec<SchedTier>,
    /// Entries by key.
    pub entries: BTreeMap<TileKey, TileEntry>,
    /// Pooled cells' worth of buffers available.
    pub pool_free: u32,
    /// Counters.
    pub counters: TileCounters,
    /// Radius scale from the running quality tier (default 1).
    pub quality_scale: f32,
    /// `ProcGenSettings::paused`: scheduling frozen, resident cells stay.
    pub paused: bool,
    last_anchors: Vec<Vec3>,
    /// Cells the memory cap released while still wanted, with the bytes they held. One is not
    /// queued again until it fits under the cap beside the resident cells, or no anchor wants it
    /// any more (audit wave 6, stream H1: it was regenerated and released again every step).
    capped: BTreeMap<TileKey, u64>,
    /// Last wanted set. Reused while the anchors, settings, quality scale, bounds and tiers match.
    wanted_memo: Option<WantedMemo>,
}

/// The wanted set of one step, so the next step can skip [`cells_in_rect`] when nothing that
/// selects cells has changed.
#[derive(Clone, Debug)]
struct WantedMemo {
    anchors: Vec<AnchorView>,
    settings: AnchorSettings,
    quality_scale: f32,
    bounds: [f64; 4],
    tiers: Vec<SchedTier>,
    map: BTreeMap<TileKey, f64>,
}

impl GeneratorTiles {
    /// A scheduler over a bounds rectangle and tiers.
    pub fn new(bounds: [f64; 4], tiers: Vec<SchedTier>) -> Self {
        Self {
            bounds,
            tiers,
            quality_scale: 1.0,
            ..Self::default()
        }
    }

    /// A scheduler for a tier plan.
    pub fn for_plan(bounds: [f64; 4], plan: &TierPlan) -> Self {
        Self::new(
            bounds,
            plan.tiers
                .iter()
                .map(|t| SchedTier {
                    index: t.index,
                    cell_m: t.cell_m,
                })
                .collect(),
        )
    }

    /// The state of a cell.
    pub fn state(&self, key: TileKey) -> Option<TileState> {
        self.entries.get(&key).map(|e| e.state)
    }

    fn tier_rect(&self, t: &SchedTier, cell: CellCoord) -> [f64; 4] {
        t.cell_m
            .map_or(self.bounds, |c| intersect(cell.rect(c), self.bounds))
    }

    fn radii(&self, t: &SchedTier, settings: &AnchorSettings) -> (TierReach, f64, f64) {
        let r = settings.reach_for(t.cell_m);
        let scale = f64::from(self.quality_scale.max(0.0));
        let build = f64::from(r.build_m) * scale;
        let release = if t.cell_m.is_none() && settings.tiers.is_empty() {
            build * 1.25
        } else {
            build * f64::from(r.release_factor())
        };
        (r, build, release)
    }

    /// `(distance / reach_scale, priority)` of a rectangle for one anchor.
    fn measure(a: &AnchorView, rect: [f64; 4], build: f64, view_bias: f32) -> (f64, f64) {
        let p = [f64::from(a.pos.x), f64::from(a.pos.z)];
        let d = distance_to_rect(p, rect) / f64::from(a.reach_scale.max(1e-3));
        let facing = a.view_dir.map_or(0.0, |v| {
            let to = [(rect[0] + rect[2]) * 0.5 - p[0], (rect[1] + rect[3]) * 0.5 - p[1]];
            let vl = (f64::from(v.x).powi(2) + f64::from(v.z).powi(2)).sqrt();
            let tl = (to[0] * to[0] + to[1] * to[1]).sqrt();
            if vl <= 1e-9 || tl <= 1e-9 {
                0.0
            } else {
                ((f64::from(v.x) * to[0] + f64::from(v.z) * to[1]) / (vl * tl)).max(0.0)
            }
        });
        let pr = (d / build.max(1e-6)) * (1.0 - f64::from(view_bias.clamp(0.0, 1.0)) * facing);
        (d, pr)
    }

    /// Whether the rectangle is inside the anchor's frustum widened by `margin_deg`: for each side
    /// plane (through the eye), the angle by which the cell's bounding circle lies outside it is at
    /// most the margin. The far plane uses the plain sphere test; the near plane is ignored.
    ///
    /// The cell is a column: its content may sit at any height (the gate does not know the
    /// ground). It is tested at the eye's height and, for a camera pitched up or down, also at the
    /// point of the column nearest the view axis, so the ground ahead of a camera looking down is
    /// not gated out for lying below the eye (audit wave 5, procgen-g2 High). A level camera's
    /// axis point is the eye-height point, so its answers are unchanged.
    fn in_view(a: &AnchorView, rect: [f64; 4], margin_deg: f32) -> bool {
        let Some(f) = a.frustum else {
            return true;
        };
        let (cx, cz) = (
            ((rect[0] + rect[2]) * 0.5) as f32,
            ((rect[1] + rect[3]) * 0.5) as f32,
        );
        let half = ((rect[2] - rect[0]).powi(2) + (rect[3] - rect[1]).powi(2)).sqrt() * 0.5;
        let planes = f.planes();
        let axis = a
            .view_dir
            .or_else(|| planes.get(4).map(|p| p.normal))
            .filter(|v| v.length() > 1e-6)
            .map(Vec3::normalize);
        if Self::point_in_view(a, planes, axis, Vec3::new(cx, a.pos.y, cz), half, margin_deg) {
            return true;
        }
        let Some(u) = axis.filter(|u| u.y.abs() > 1e-6) else {
            return false;
        };
        let (hx, hz) = (cx - a.pos.x, cz - a.pos.z);
        let flat = u.x * u.x + u.z * u.z;
        let along = if flat > 1e-6 {
            (hx * u.x + hz * u.z) / flat
        } else {
            4.0 * (hx * hx + hz * hz).sqrt()
        };
        if along <= 0.0 {
            return false;
        }
        let y = a.pos.y + u.y * along;
        Self::point_in_view(a, planes, axis, Vec3::new(cx, y, cz), half, margin_deg)
    }

    /// [`Self::in_view`] for the cell's bounding circle of radius `half` centered at `c`.
    fn point_in_view(
        a: &AnchorView,
        planes: &[regia_core::math::Plane],
        axis: Option<Vec3>,
        c: Vec3,
        half: f64,
        margin_deg: f32,
    ) -> bool {
        let dist = f64::from((c - a.pos).length());
        if dist <= half {
            return true;
        }
        let ang_r = crate::detmath::asin((half / dist).min(1.0));
        let margin = f64::from(margin_deg.clamp(0.0, 89.0)).to_radians();
        // Behind the eye the side planes fold back; reject what is more than 90° + margin off the
        // view axis (the half field of view is below 90°, so this never rejects a visible cell).
        if let Some(axis) = axis {
            let u = (c - a.pos) / dist as f32;
            let cos = f64::from(axis.dot(u)).clamp(-1.0, 1.0);
            let off = crate::detmath::acos(cos);
            if off - ang_r > std::f64::consts::FRAC_PI_2 + margin {
                return false;
            }
        }
        let sides = planes.len().min(4);
        for p in &planes[..sides] {
            let sd = f64::from(p.signed_distance(c)) / dist;
            let outside = -crate::detmath::asin(sd.clamp(-1.0, 1.0));
            if outside - ang_r > margin {
                return false;
            }
        }
        if let Some(far) = planes.get(5)
            && f64::from(far.signed_distance(c)) < -half
        {
            return false;
        }
        true
    }

    /// Every cell the anchors want, with its best priority.
    fn wanted(&self, anchors: &[AnchorView], settings: &AnchorSettings) -> BTreeMap<TileKey, f64> {
        let mut out = BTreeMap::new();
        for t in &self.tiers {
            let (reach, build, _) = self.radii(t, settings);
            for a in anchors {
                let r = build * f64::from(a.reach_scale.max(1e-3));
                let candidates: Vec<CellCoord> = match t.cell_m {
                    None => vec![CellCoord::new(0, 0)],
                    Some(c) => {
                        let p = [f64::from(a.pos.x), f64::from(a.pos.z)];
                        let area = intersect([p[0] - r, p[1] - r, p[0] + r, p[1] + r], self.bounds);
                        if area[0] > area[2] || area[1] > area[3] {
                            continue;
                        }
                        cells_in_rect(area, c)
                    }
                };
                for cell in candidates {
                    let rect = self.tier_rect(t, cell);
                    if rect[0] > rect[2] || rect[1] > rect[3] {
                        continue;
                    }
                    let (d, pr) = Self::measure(a, rect, build, reach.view_bias);
                    if d > build {
                        continue;
                    }
                    if let Some(g) = reach.view_gate
                        && !Self::in_view(a, rect, g.build_margin_deg)
                    {
                        continue;
                    }
                    let key = TileKey { tier: t.index, cell };
                    let e = out.entry(key).or_insert(pr);
                    if pr < *e {
                        *e = pr;
                    }
                }
            }
        }
        out
    }

    /// Whether some anchor still holds the cell (within the release distance and view margin).
    fn held(&self, key: TileKey, anchors: &[AnchorView], settings: &AnchorSettings) -> bool {
        let Some(t) = self.tiers.iter().find(|t| t.index == key.tier) else {
            return false;
        };
        let (reach, build, release) = self.radii(t, settings);
        let rect = self.tier_rect(t, key.cell);
        anchors.iter().any(|a| {
            let (d, _) = Self::measure(a, rect, build, reach.view_bias);
            if d > release {
                return false;
            }
            match reach.view_gate {
                Some(g) => Self::in_view(a, rect, g.release_margin_deg.max(g.build_margin_deg)),
                None => true,
            }
        })
    }

    /// One planning step (§ 4.3): release, drop, cancel, queue by priority, start within the
    /// budgets, apply within the apply budget. Deterministic: the same entries, anchors and
    /// settings give the same plan.
    pub fn step(
        &mut self,
        anchors: &[AnchorView],
        settings: &AnchorSettings,
        budget: &mut ApplyBudget,
    ) -> StepPlan {
        let _span = tracing::info_span!("procgen.schedule", cells = self.entries.len()).entered();
        let mut plan = StepPlan::default();
        self.counters.steps += 1;
        let now = self.counters.steps;
        // Cells released last step are gone now.
        self.entries.retain(|_, e| e.state != TileState::Releasing);
        if self.paused {
            return plan;
        }
        // Teleport: a jump of more than 2 × the coarsest tier's build radius.
        let coarsest_build = self.tiers.first().map_or(0.0, |t| self.radii(t, settings).1);
        if anchors.len() == self.last_anchors.len()
            && anchors.iter().zip(&self.last_anchors).any(|(a, b)| {
                f64::from(Vec3::new(a.pos.x - b.x, 0.0, a.pos.z - b.z).length()) > 2.0 * coarsest_build
            })
        {
            self.counters.teleports += 1;
        }
        self.last_anchors = anchors.iter().map(|a| a.pos).collect();

        let wanted = self.wanted_cached(anchors, settings);
        // Release, drop, cancel what no anchor holds.
        let keys: Vec<TileKey> = self.entries.keys().copied().collect();
        for key in keys {
            if wanted.contains_key(&key) || self.held(key, anchors, settings) {
                continue;
            }
            let Some(state) = self.state(key) else {
                continue;
            };
            match state {
                TileState::Queued => {
                    self.entries.remove(&key);
                    self.counters.dropped += 1;
                    plan.dropped.push(key);
                }
                TileState::Capturing | TileState::Running => {
                    self.entries.remove(&key);
                    self.counters.cancelled += 1;
                    plan.cancel.push(key);
                }
                TileState::Failed => {
                    self.entries.remove(&key);
                }
                TileState::Applying | TileState::Resident => {
                    self.release_cell(key, settings, &mut plan);
                }
                TileState::Releasing => {}
            }
        }
        // A cell the cap released is forgotten once no anchor wants it.
        self.capped.retain(|key, _| wanted.contains_key(key));
        let cap = u64::from(settings.resident_mb()) * 1024 * 1024;
        let held_bytes: u64 = self.entries.values().map(|e| e.bytes).sum();
        // The farthest finest-tier cell holding memory: a capped cell nearer than it comes back,
        // and the cap then releases that one instead (an anchor moving onto a capped cell).
        let finest = self.tiers.last().map_or(0, |t| t.index);
        let worst_held = self
            .entries
            .iter()
            .filter(|(k, e)| k.tier == finest && e.bytes > 0)
            .map(|(_, e)| e.priority)
            .max_by(f64::total_cmp);
        // Queue new wanted cells; refresh priorities.
        for (key, pr) in &wanted {
            match self.entries.get_mut(key) {
                Some(e) => e.priority = *pr,
                None => {
                    if let Some(bytes) = self.capped.get(key) {
                        let fits = held_bytes.saturating_add(*bytes) <= cap;
                        let nearer = worst_held.is_some_and(|worst| *pr < worst);
                        if !fits && !nearer {
                            continue;
                        }
                        self.capped.remove(key);
                    }
                    self.entries.insert(
                        *key,
                        TileEntry {
                            state: TileState::Queued,
                            priority: *pr,
                            queued_at: now,
                            elements: 0,
                            applied: 0,
                            bytes: 0,
                            ms: 0.0,
                            output_hash: None,
                        },
                    );
                    self.counters.requested += 1;
                }
            }
        }
        // Memory cap: release the farthest resident cells of the finest tier.
        let mut resident_bytes = held_bytes;
        if resident_bytes > cap {
            let mut far: Vec<(f64, TileKey)> = self
                .entries
                .iter()
                // Applying cells hold their bytes too: one that just finished farther away goes
                // before a resident cell nearer the anchor (audit wave 6, stream H1).
                .filter(|(k, e)| {
                    k.tier == finest && matches!(e.state, TileState::Resident | TileState::Applying)
                })
                .map(|(k, e)| (e.priority, *k))
                .collect();
            far.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            for (_, k) in far {
                if resident_bytes <= cap {
                    break;
                }
                let bytes = self.entries.get(&k).map_or(0, |e| e.bytes);
                resident_bytes -= bytes;
                self.capped.insert(k, bytes);
                self.release_cell(k, settings, &mut plan);
                self.counters.memory_released += 1;
                plan.memory_capped = true;
            }
        }
        // Start queued cells by priority; ties coarser tier first, then cell ascending.
        let running = self.entries.values().filter(|e| e.state.in_flight()).count() as u32;
        let mut slots = settings
            .max_running()
            .saturating_sub(running)
            .min(settings.max_new());
        if slots > 0 {
            let mut queued: Vec<(f64, TileKey)> = self
                .entries
                .iter()
                .filter(|(_, e)| e.state == TileState::Queued)
                .map(|(k, e)| (e.priority, *k))
                .collect();
            queued.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            for (_, k) in queued {
                if slots == 0 {
                    break;
                }
                if !self.parent_ready(k) {
                    continue;
                }
                if let Some(e) = self.entries.get_mut(&k) {
                    e.state = TileState::Capturing;
                }
                if self.pool_free > 0 {
                    self.pool_free -= 1;
                    self.counters.pool_hits += 1;
                }
                self.counters.started += 1;
                plan.start.push(k);
                slots -= 1;
            }
        }
        // Apply finished cells within the budget, by priority.
        let mut applying: Vec<(f64, TileKey)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.state == TileState::Applying)
            .map(|(k, e)| (e.priority, *k))
            .collect();
        applying.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        for (_, k) in applying {
            let Some(e) = self.entries.get_mut(&k) else {
                continue;
            };
            let left = e.elements - e.applied;
            let take = match budget.elements {
                Some(b) => left.min(b),
                None => left,
            };
            if take == 0 && left > 0 {
                break;
            }
            let from = e.applied;
            e.applied += take;
            if let Some(b) = budget.elements.as_mut() {
                *b -= take;
            }
            plan.apply.push((k, from, e.applied));
            if e.applied >= e.elements {
                e.state = TileState::Resident;
                plan.resident.push(k);
            }
        }
        self.wanted_memo = Some(WantedMemo {
            anchors: anchors.to_vec(),
            settings: settings.clone(),
            quality_scale: self.quality_scale,
            bounds: self.bounds,
            tiers: self.tiers.clone(),
            map: wanted,
        });
        plan
    }

    /// `wanted`, or the previous step's set when the inputs that select cells are unchanged.
    fn wanted_cached(&mut self, anchors: &[AnchorView], settings: &AnchorSettings) -> BTreeMap<TileKey, f64> {
        let hit = self.wanted_memo.as_ref().is_some_and(|memo| {
            memo.anchors.as_slice() == anchors
                && memo.settings == *settings
                && memo.quality_scale == self.quality_scale
                && memo.bounds == self.bounds
                && memo.tiers == self.tiers
        });
        if hit {
            match self.wanted_memo.take() {
                Some(memo) => memo.map,
                None => self.wanted(anchors, settings),
            }
        } else {
            self.wanted(anchors, settings)
        }
    }

    /// Whether every coarser tier's cell containing `key` is resident (the whole tier first).
    fn parent_ready(&self, key: TileKey) -> bool {
        let Some(fine) = self.tiers.iter().find(|t| t.index == key.tier) else {
            return true;
        };
        for t in self.tiers.iter().filter(|t| t.index < key.tier) {
            let cell = match (fine.cell_m, t.cell_m) {
                (Some(f), Some(c)) => {
                    let r = key.cell.rect(f);
                    CellCoord::containing((r[0] + r[2]) * 0.5, (r[1] + r[3]) * 0.5, c)
                }
                _ => CellCoord::new(0, 0),
            };
            let ck = TileKey { tier: t.index, cell };
            if self.state(ck) != Some(TileState::Resident) {
                return false;
            }
        }
        true
    }

    fn release_cell(&mut self, key: TileKey, settings: &AnchorSettings, plan: &mut StepPlan) {
        if let Some(e) = self.entries.get_mut(&key) {
            e.state = TileState::Releasing;
            e.bytes = 0;
        }
        if settings.pool_cells > 0 {
            self.pool_free = (self.pool_free + 1).min(settings.pool_cells);
        }
        self.counters.released += 1;
        plan.release.push(key);
    }

    /// The host started evaluating a capturing cell.
    pub fn set_running(&mut self, key: TileKey) {
        if let Some(e) = self.entries.get_mut(&key)
            && e.state == TileState::Capturing
        {
            e.state = TileState::Running;
        }
    }

    /// A cell's run finished with `elements` to apply and `bytes` resident.
    pub fn finished(&mut self, key: TileKey, elements: u64, bytes: u64, output_hash: [u8; 32], ms: f64) {
        if let Some(e) = self.entries.get_mut(&key)
            && e.state.in_flight()
        {
            e.state = TileState::Applying;
            e.elements = elements;
            e.applied = 0;
            e.bytes = bytes;
            e.output_hash = Some(output_hash);
            e.ms = ms;
        }
    }

    /// A cell's run failed.
    pub fn failed(&mut self, key: TileKey) {
        if let Some(e) = self.entries.get_mut(&key)
            && e.state.in_flight()
        {
            e.state = TileState::Failed;
        }
    }

    /// Re-runs cells: resident, applying and failed ones go back to the queue (`procgen.tiles_rebuild`).
    /// Returns the keys re-queued.
    pub fn rebuild(&mut self, keys: Option<&[TileKey]>) -> Vec<TileKey> {
        let mut out = Vec::new();
        for (k, e) in &mut self.entries {
            if keys.is_some_and(|ks| !ks.contains(k)) {
                continue;
            }
            if matches!(
                e.state,
                TileState::Resident | TileState::Applying | TileState::Failed
            ) {
                e.state = TileState::Queued;
                e.applied = 0;
                out.push(*k);
            }
        }
        out
    }

    /// Whether every cell the anchor wants (optionally of one tier) is resident.
    pub fn all_ready_near(&self, anchor: &AnchorView, tier: Option<u8>, settings: &AnchorSettings) -> bool {
        self.wanted(std::slice::from_ref(anchor), settings)
            .keys()
            .filter(|k| tier.is_none_or(|t| k.tier == t))
            .all(|k| self.state(*k) == Some(TileState::Resident))
    }

    /// Cells wanted by these anchors right now (for waits and reports).
    pub fn wanted_keys(&self, anchors: &[AnchorView], settings: &AnchorSettings) -> Vec<TileKey> {
        self.wanted(anchors, settings).into_keys().collect()
    }

    /// Resident memory, bytes.
    pub fn resident_bytes(&self) -> u64 {
        self.entries.values().map(|e| e.bytes).sum()
    }
}

/// The scheduler of every anchored generator (`ProcGenSet::Schedule`).
#[derive(Resource, Default)]
pub struct ProcGenTiles {
    /// Per generator entity.
    pub generators: BTreeMap<Entity, GeneratorTiles>,
    /// Anchored generators that cannot be planned: `(code, message)` (compile or tile errors).
    pub errors: BTreeMap<Entity, (String, String)>,
}

impl ProcGenTiles {
    /// The state of a generator's cell.
    pub fn state(&self, generator: Entity, key: TileKey) -> Option<TileState> {
        self.generators.get(&generator).and_then(|g| g.state(key))
    }

    /// Whether every cell around `anchor` (optionally of one tier) is resident.
    pub fn all_ready_near(
        &self,
        generator: Entity,
        anchor: Vec3,
        tier: Option<u8>,
        settings: &AnchorSettings,
    ) -> bool {
        self.generators
            .get(&generator)
            .is_some_and(|g| g.all_ready_near(&AnchorView::at(anchor), tier, settings))
    }
}

// ---------------------------------------------------------------------------------------------
// ECS host (`ProcGenSet::Schedule`)
// ---------------------------------------------------------------------------------------------

/// A resident cell's output entity: carries the cell's [`crate::world::ProcOutput`] (drawn like a
/// generator's) and parents its spawned prefabs; despawned on release.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcTileCell {
    /// The generator.
    pub generator: Entity,
    /// The cell.
    pub key: TileKey,
}

/// A finished cell run delivered by a worker.
type CellJob = std::sync::mpsc::Receiver<Result<CellOutput, RunError>>;

/// Per anchored generator: the compiled graph, captured inputs and the cells' runs and outputs.
#[derive(Component)]
pub struct ProcGenTileHost {
    graph: Arc<CompiledGraph>,
    plan: Arc<TierPlan>,
    inputs: RunInputs,
    bounds: ShapeData,
    halo_m: f64,
    /// Finished cells (applying or resident).
    outputs: BTreeMap<TileKey, Arc<CellOutput>>,
    /// Output entities of resident cells.
    roots: BTreeMap<TileKey, Entity>,
    pending: BTreeMap<TileKey, (CancelToken, StdMutex<CellJob>)>,
    /// `(graph hash, param hash)` the host was built for.
    hashes: ([u8; 32], [u8; 32]),
    /// `Baked` streaming: the file cells load from (no evaluation).
    file: Option<Arc<crate::pgout::PgoutFile>>,
    /// Graph path this host was compiled from.
    source_path: String,
    /// Parameter text this host was compiled from.
    source_params: BTreeMap<String, String>,
    /// Content hash of each file the compile read. A later check uses [`crate::world::file_content_hash`],
    /// not [`crate::world::read_graph_text`]. `None` (failed compiles only): the compile asked for the
    /// file and could not read it, so the file appearing reads as a change.
    source_files: BTreeMap<String, Option<[u8; 32]>>,
    /// Frames since this host last hashed its source files. Independent of the shared watch counter.
    frames_since_file_check: u32,
    /// The current graph path, parameters or files failed to compile: the cells stay, the error stays
    /// reported, and nothing recompiles until one of them changes.
    source_failed: bool,
    /// The last init error, if the generator could not be planned.
    pub error: Option<(String, String)>,
}

impl ProcGenTileHost {
    /// The tier plan.
    pub fn plan(&self) -> &TierPlan {
        &self.plan
    }

    /// The halo, meters.
    pub fn halo_m(&self) -> f64 {
        self.halo_m
    }

    /// A finished cell's output.
    pub fn cell(&self, key: TileKey) -> Option<&CellOutput> {
        self.outputs.get(&key).map(|c| c.as_ref())
    }
}

/// Whether a generator is scheduled per cell around anchors (mode `Anchored` with runtime settings).
pub fn is_anchored_runtime(g: &crate::world::ProcGenerator) -> bool {
    g.mode == crate::world::ProcGenMode::Anchored && g.tiles.as_ref().is_some_and(|t| t.runtime.is_some())
}

/// Whether a generator streams its baked cells around anchors (§ 4.3 "Baked + anchors": load and
/// apply only, no evaluation).
pub fn is_baked_streaming(g: &crate::world::ProcGenerator) -> bool {
    g.mode == crate::world::ProcGenMode::Baked && g.tiles.as_ref().is_some_and(|t| t.runtime.is_some())
}

/// Whether the anchored scheduler drives this generator.
pub fn is_scheduled(g: &crate::world::ProcGenerator) -> bool {
    is_anchored_runtime(g) || is_baked_streaming(g)
}

/// Records the cells of a finished whole tiled run (`Baked`/`OnLoad`) as resident, for
/// `procgen.tiles`.
pub(crate) fn record_baked_cells(world: &mut World, entity: Entity, run: &TiledRun) {
    let bounds = crate::world::generator_bounds(world, entity);
    let mut tiles = world.resource_mut::<ProcGenTiles>();
    let tiers: BTreeSet<u8> = run.cells.keys().map(|k| k.tier).collect();
    let mut g = GeneratorTiles::new(bounds_rect(&bounds), Vec::new());
    g.tiers = tiers
        .into_iter()
        .map(|t| SchedTier {
            index: t,
            cell_m: run.cells.iter().find(|(k, _)| k.tier == t).and_then(|(_, c)| {
                let w = c.rect[2] - c.rect[0];
                (c.rect != bounds_rect(&bounds)).then_some(w)
            }),
        })
        .collect();
    for (k, c) in &run.cells {
        let n = c.output.instance_count() as u64 + c.output.prefabs.len() as u64;
        g.entries.insert(
            *k,
            TileEntry {
                state: TileState::Resident,
                priority: 0.0,
                queued_at: 0,
                elements: n,
                applied: n,
                bytes: estimate_output_bytes(c.output.as_ref()),
                ms: c.report.total_ms,
                output_hash: Some(c.output_hash),
            },
        );
    }
    tiles.generators.insert(entity, g);
}

/// Estimated resident bytes of a cell output (instances 80 bytes, prefab records 128, generated
/// meshes by their streams).
pub fn estimate_output_bytes(o: &RunOutput) -> u64 {
    let meshes: u64 = o
        .meshes
        .iter()
        .map(|m| m.positions.len() as u64 * 32 + m.indices.len() as u64 * 4)
        .sum();
    o.instance_count() as u64 * 80 + o.prefabs.len() as u64 * 128 + meshes
}

/// Re-queues cells of an anchored generator (`procgen.tiles_rebuild`, `procgen.run`): resident,
/// applying and failed cells (all, or `cells` of `tier`) run again. Returns the keys re-queued.
///
/// The next update also re-hashes the host's source files, so an unsaved graph edit
/// (`ProcGraphOverlay`) or a file written just before the request is what the cells run, not the
/// graph of the last periodic file check.
pub fn rebuild_cells(
    world: &mut World,
    entity: Entity,
    tier: Option<u8>,
    cells: Option<&[CellCoord]>,
) -> Vec<TileKey> {
    if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
        host.frames_since_file_check = u32::MAX;
    }
    let mut tiles = world.resource_mut::<ProcGenTiles>();
    let Some(g) = tiles.generators.get_mut(&entity) else {
        return Vec::new();
    };
    let keys: Vec<TileKey> = g
        .entries
        .keys()
        .filter(|k| tier.is_none_or(|t| k.tier == t))
        .filter(|k| cells.is_none_or(|cs| cs.contains(&k.cell)))
        .copied()
        .collect();
    g.rebuild(Some(&keys))
}

/// Extra anchors a host supplies for every anchored generator whatever its [`AnchorSource`]: the
/// editor's "generate around view" switch puts the editor camera here (spec 66 § 4.1, spec 67).
/// Empty by default; the resource need not exist.
#[derive(Resource, Default, Clone, Debug)]
pub struct ProcGenExtraAnchors(pub Vec<AnchorView>);

/// The anchors of one generator's source, as the scheduler sees them: the source's own anchors
/// plus any [`ProcGenExtraAnchors`] a host supplies (the editor's "generate around view" switch).
pub fn gather_anchors(world: &mut World, source: AnchorSource) -> Vec<AnchorView> {
    gather_anchors_impl(world, source, true)
}

/// The anchors of one generator's source only, without [`ProcGenExtraAnchors`]. Use this when the
/// answer must be bucketed by source (for example `procgen.anchors` labels its results
/// `primary_view`/`tagged`); extras would otherwise leak into every bucket.
pub fn gather_source_anchors(world: &mut World, source: AnchorSource) -> Vec<AnchorView> {
    gather_anchors_impl(world, source, false)
}

fn gather_anchors_impl(world: &mut World, source: AnchorSource, include_extra: bool) -> Vec<AnchorView> {
    use regia_scene::{GlobalTransform, Transform};
    let mut out = Vec::new();
    if matches!(source, AnchorSource::PrimaryView | AnchorSource::Both) {
        let mut cams: Vec<(i32, Entity, Transform, regia_render::Camera)> = world
            .query::<(
                Entity,
                &regia_render::Camera,
                Option<&GlobalTransform>,
                Option<&Transform>,
                Option<&ChildOf>,
            )>()
            .iter(world)
            .filter(|(_, c, ..)| matches!(c.target, regia_render::RenderTarget::Window))
            .map(|(e, c, g, t, p)| {
                (
                    c.order,
                    e,
                    crate::world::world_transform(g, t, p.is_some()),
                    c.clone(),
                )
            })
            .collect();
        cams.sort_by_key(|c| (c.0, c.1));
        if let Some((_, e, tf, cam)) = cams.into_iter().next()
            && !(matches!(source, AnchorSource::Both) && world.get::<ProcGenAnchor>(e).is_some())
        {
            out.push(camera_anchor(&tf, &cam, 1.0, true));
        }
    }
    if matches!(source, AnchorSource::Tagged | AnchorSource::Both) {
        let mut tagged: Vec<(Entity, Transform, ProcGenAnchor, Option<regia_render::Camera>)> = world
            .query::<(
                Entity,
                &ProcGenAnchor,
                Option<&GlobalTransform>,
                Option<&Transform>,
                Option<&ChildOf>,
                Option<&regia_render::Camera>,
            )>()
            .iter(world)
            .map(|(e, a, g, t, p, c)| {
                (
                    e,
                    crate::world::world_transform(g, t, p.is_some()),
                    a.clone(),
                    c.cloned(),
                )
            })
            .collect();
        tagged.sort_by_key(|t| t.0);
        for (_, tf, a, cam) in tagged {
            match cam {
                Some(c) => out.push(camera_anchor(&tf, &c, a.reach_scale, a.use_view_direction)),
                None => out.push(AnchorView {
                    pos: tf.translation,
                    view_dir: a.use_view_direction.then(|| tf.rotation * Vec3::NEG_Z),
                    frustum: None,
                    reach_scale: a.reach_scale,
                }),
            }
        }
    }
    if include_extra && let Some(extra) = world.get_resource::<ProcGenExtraAnchors>() {
        out.extend(extra.0.iter().copied());
    }
    // With a streamed level open, its sources are tagged anchors too (spec 78 § 7.4): a teleport
    // target or a far pin gets its generated cells with its world cells. A camera that is an anchor
    // already stays as it is, with its frustum.
    if let Some(views) = world.get_resource::<regia_scene::streaming::StreamViews>() {
        let offset = world
            .get_resource::<regia_scene::streaming::OriginOffset>()
            .map_or([0.0; 3], |o| o.0);
        for view in &views.0 {
            let pos = Vec3::from_array(std::array::from_fn(|axis| (view.pos[axis] - offset[axis]) as f32));
            if !out.iter().any(|anchor| anchor.pos.distance_squared(pos) < 1.0e-4) {
                out.push(AnchorView {
                    pos,
                    view_dir: None,
                    frustum: None,
                    reach_scale: view.radius_scale,
                });
            }
        }
    }
    out
}

/// An anchor for a camera: position, forward (−Z) and a 16:9 perspective frustum.
fn camera_anchor(
    tf: &regia_scene::Transform,
    cam: &regia_render::Camera,
    reach: f32,
    view: bool,
) -> AnchorView {
    let forward = tf.rotation * Vec3::NEG_Z;
    let viewm = regia_core::glam::camera::rh::view::look_to_mat4(tf.translation, forward, Vec3::Y);
    let far = if cam.far.is_finite() && cam.far > cam.near {
        cam.far
    } else {
        100_000.0
    };
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        cam.fov_y,
        16.0 / 9.0,
        cam.near.max(0.01),
        far,
    );
    AnchorView {
        pos: tf.translation,
        view_dir: view.then_some(forward),
        frustum: Some(Frustum::from_view_projection(&(proj * viewm))),
        reach_scale: reach,
    }
}

/// Whether an existing host can keep its cells this frame.
enum SourceGate {
    /// The compiled graph is still current. Resident cells stay.
    Keep,
    /// Compile again. A hash change drops the cells; the same hash does not.
    Rebuild,
}

/// Builds (or rebuilds after a graph or parameter change) the host of an anchored generator.
///
/// An existing host hashes its source files every `file_check_frames` updates (0 checks every
/// update after the first). A failed reread keeps the resident cells and tries again next update.
/// `reset_host` is what reopens a baked file.
fn ensure_host(world: &mut World, entity: Entity) -> bool {
    let Some(generator) = world.get::<crate::world::ProcGenerator>(entity).cloned() else {
        return false;
    };
    // No file yet: one warning, then nothing per frame until the bake appears (re-checked below,
    // deep-zai finding 8's remainder), a bake (`reset_host`) or a new bake directory
    // (`watch_bake_dir`, a level change).
    if world
        .resource::<ProcGenTiles>()
        .errors
        .get(&entity)
        .is_some_and(|(code, _)| code == "procgen_bake_missing")
    {
        // The latch keeps a missing file from re-planning the host every update. It must still
        // clear when the wait is over without a directory change: the file appeared in the same
        // directory (an external copy or sync), or the generator stopped being baked-streaming
        // (a stale latch would then hold forever).
        let retry = if !is_baked_streaming(&generator) {
            true
        } else {
            world
                .resource::<crate::world::ProcGenSettings>()
                .bake_dir
                .as_ref()
                .is_some_and(|dir| {
                    let guid = world
                        .get::<regia_scene::components::EntityGuid>(entity)
                        .map_or(regia_core::id::Guid::NIL, |g| g.0);
                    crate::world::bake_path(dir, guid).exists()
                })
        };
        if !retry {
            return false;
        }
        world.resource_mut::<ProcGenTiles>().errors.remove(&entity);
        if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
            host.error = None;
        }
    }
    let every = world
        .resource::<crate::world::ProcGenSettings>()
        .file_check_frames;
    if matches!(source_gate(world, entity, &generator, every), SourceGate::Keep) {
        return true;
    }
    let (result, asked) = crate::world::compile_for_tracked(world, entity);
    let (compiled, files) = match result {
        Ok(pair) => pair,
        Err((code, message, _)) => {
            if world.get::<ProcGenTileHost>(entity).is_some() {
                tracing::warn!(
                    target: "regia::procgen",
                    %code,
                    %message,
                    "anchored generator kept its cells after a failed reread"
                );
                // The cells stay, but `procgen.tiles` reports the error, and the host waits for the
                // path, the parameters or a source file to change instead of recompiling the same
                // failure every update (corr-procgen).
                set_host_error(world, entity, code, message);
                // Record the current stamps of every file the failed compile asked for: an
                // unchanged broken file then stops recompiling every `file_check_frames`, and
                // reverting it to its last good text reads as a change, recompiles and clears
                // `source_failed` and the error (fixrev2-procgen-k). A file it asked for but could
                // not read (a newly called graph, a variant's missing base) is stamped `None`, so
                // creating it reads as a change too (fixrev5-procgen-missing-calls: stamping only
                // the last good compile's files left that error stuck).
                let stamps = asked
                    .into_iter()
                    .map(|path| {
                        let now = crate::world::file_content_hash(world, &path);
                        (path, now)
                    })
                    .collect();
                if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
                    host.source_path.clone_from(&generator.graph);
                    host.source_params.clone_from(&generator.params);
                    host.source_files = stamps;
                    host.frames_since_file_check = 0;
                    host.source_failed = true;
                }
                return true;
            }
            set_host_error(world, entity, code, message);
            return false;
        }
    };
    let hashes = (compiled.graph_hash, compiled.param_hash);
    // Same graph and parameters: refresh the file stamps and leave the cells in place.
    if world
        .get::<ProcGenTileHost>(entity)
        .is_some_and(|h| h.hashes == hashes)
    {
        let source_files = source_stamps(world, &files);
        if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
            host.source_path.clone_from(&generator.graph);
            host.source_params.clone_from(&generator.params);
            host.source_files = source_files;
            host.frames_since_file_check = 0;
            host.source_failed = false;
            host.error = None;
        }
        world.resource_mut::<ProcGenTiles>().errors.remove(&entity);
        return true;
    }
    let bounds = crate::world::generator_bounds(world, entity);
    let ctx = match crate::world::tile_ctx(&generator, &compiled, bounds) {
        Ok(Some(c)) => c,
        Ok(None) => return false,
        Err((code, message)) => {
            set_host_error(world, entity, code, message);
            return false;
        }
    };
    // Spec 66 § 4.2 validation: a finer tier building farther than its parent is wasted work.
    // Logged once per host build (graph or parameter change); `procgen.tiles` reports it too.
    if let Some(runtime) = generator.tiles.as_ref().and_then(|t| t.runtime.as_ref()) {
        for (code, message) in ctx.plan.check_reach(runtime) {
            tracing::warn!(target: "regia::procgen", %code, %message, "anchored generator tier reach");
        }
    }
    // Baked streaming reads cells from the file: it captures nothing, and no file is an error.
    let settings = world.resource::<crate::world::ProcGenSettings>().clone();
    let file = if is_baked_streaming(&generator) {
        let Some(dir) = settings.bake_dir.clone() else {
            set_host_error(
                world,
                entity,
                "procgen_no_bake_dir".to_string(),
                "no bake directory is set".to_string(),
            );
            return false;
        };
        // The primary directory first; a placed or additive level's generator falls back to its
        // own level's directory under its source guid (ADR-7287).
        let (path, level) = crate::world::locate_cell_bake(world, entity, &dir);
        // A packaged game has the bake only in its paks (ADR-7214): read it whole from there.
        let opened = if path.exists() {
            (
                crate::pgout::PgoutFile::open(&path),
                crate::world::ProcBakeSource::Disk,
            )
        } else {
            match crate::world::read_bake(world, &path) {
                Some((bytes, source)) => (crate::pgout::PgoutFile::from_bytes(&path, bytes.into()), source),
                None => {
                    if let Ok(mut e) = world.get_entity_mut(entity) {
                        e.remove::<crate::world::ProcBakeOrigin>();
                    }
                    let message = format!("{} is missing", path.display());
                    set_host_error(world, entity, "procgen_bake_missing".to_string(), message);
                    return false;
                }
            }
        };
        match opened {
            (Ok(f), source) => {
                if let Ok(mut e) = world.get_entity_mut(entity) {
                    e.insert(crate::world::ProcBakeOrigin {
                        path: path.clone(),
                        level,
                        source,
                    });
                }
                // The file is served, but marked stale when the graph, its parameters or the seed
                // changed since the bake, as a whole bake is on load (spec 62 § 8).
                let seed = crate::world::generator_seed(world, entity);
                let h = &f.header;
                if (h.graph_hash, h.param_hash, h.seed) != (hashes.0, hashes.1, seed) {
                    tracing::warn!(target: "regia_procgen", "{}: the bake does not match the current graph, parameters or seed; re-bake", path.display());
                    if let Some(mut st) = crate::world::state_mut(world, entity) {
                        st.stale = true;
                    }
                }
                Some(Arc::new(f))
            }
            (Err(e), _) => {
                set_host_error(world, entity, e.code.to_string(), e.message);
                return false;
            }
        }
    } else {
        None
    };
    let (snap, _deps) = if file.is_some() {
        (crate::exec::StaticSnapshot::default(), Vec::new())
    } else {
        crate::world::capture(world, entity, &compiled)
    };
    let mut inputs = RunInputs::new(Arc::new(snap), crate::world::generator_seed(world, entity));
    inputs.quality = settings.running_tier;
    // Drop the old host's cells.
    if let Some(old) = world.get::<ProcGenTileHost>(entity) {
        let roots: Vec<Entity> = old.roots.values().copied().collect();
        for (t, _) in old.pending.values() {
            t.cancel();
        }
        for r in roots {
            if let Ok(e) = world.get_entity_mut(r) {
                e.despawn();
            }
        }
    }
    let mut g = GeneratorTiles::for_plan(bounds_rect(&ctx.bounds), &ctx.plan);
    g.quality_scale = generator
        .tiles
        .as_ref()
        .and_then(|t| t.runtime.as_ref())
        .map_or(1.0, |r| r.quality_scale(settings.running_tier));
    {
        let mut tiles = world.resource_mut::<ProcGenTiles>();
        tiles.generators.insert(entity, g);
        tiles.errors.remove(&entity);
    }
    let source_files = source_stamps(world, &files);
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(ProcGenTileHost {
            graph: Arc::new(compiled),
            plan: Arc::new(ctx.plan),
            inputs,
            bounds: ctx.bounds,
            halo_m: ctx.halo_m,
            outputs: BTreeMap::new(),
            roots: BTreeMap::new(),
            pending: BTreeMap::new(),
            hashes,
            file,
            source_path: generator.graph,
            source_params: generator.params,
            source_files,
            frames_since_file_check: 0,
            source_failed: false,
            error: None,
        });
    }
    true
}

/// Content hashes of the files a compile read. Missing files are omitted; an empty map makes the
/// next due frame compile again.
fn source_stamps(world: &World, files: &[String]) -> BTreeMap<String, Option<[u8; 32]>> {
    files
        .iter()
        .filter_map(|path| {
            crate::world::file_content_hash(world, path).map(|hash| (path.clone(), Some(hash)))
        })
        .collect()
}

/// Keeps a host that already has cells. `clear_error` drops a stale planning error so the next
/// unchanged frame does not rebuild; a host whose current sources failed to compile keeps its error.
fn remember_kept_host(world: &mut World, entity: Entity, frames: u32, clear_error: bool) {
    let mut clear_error = clear_error;
    if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
        host.frames_since_file_check = frames;
        clear_error &= !host.source_failed;
        if clear_error {
            host.error = None;
        }
    }
    if clear_error {
        world.resource_mut::<ProcGenTiles>().errors.remove(&entity);
    }
}

/// Hashes stored source files when the interval is due. A changed file compiles again, even when
/// another stored file is gone (a variant repointed at a renamed base must not wait for the old
/// base to come back; a compile that still needs a gone file fails, keeps the cells and watches the
/// missing file). With nothing changed, a gone file is a failed reread: the cells stay, and the
/// next update checks again.
fn source_gate(
    world: &mut World,
    entity: Entity,
    generator: &crate::world::ProcGenerator,
    every: u32,
) -> SourceGate {
    enum Snap {
        NotDue(u32),
        Due(BTreeMap<String, Option<[u8; 32]>>),
    }
    let snap = {
        let Some(host) = world.get::<ProcGenTileHost>(entity) else {
            return SourceGate::Rebuild;
        };
        if host.source_path != generator.graph || host.source_params != generator.params {
            return SourceGate::Rebuild;
        }
        let frames = host.frames_since_file_check.saturating_add(1);
        if frames <= every {
            Snap::NotDue(frames)
        } else if host.source_files.is_empty() {
            return SourceGate::Rebuild;
        } else {
            Snap::Due(host.source_files.clone())
        }
    };
    match snap {
        Snap::NotDue(frames) => {
            remember_kept_host(world, entity, frames, true);
            SourceGate::Keep
        }
        Snap::Due(files) => {
            let mut failed = false;
            let mut changed = false;
            for (path, stamp) in &files {
                match (stamp, crate::world::file_content_hash(world, path)) {
                    // A file the compile read is gone: a failed reread.
                    (Some(_), None) => failed = true,
                    (Some(was), Some(now)) if was != &now => changed = true,
                    // A file a failed compile could not read now reads: compile again.
                    (None, Some(_)) => changed = true,
                    // Still missing, or unchanged.
                    (None, None) | (Some(_), Some(_)) => {}
                }
            }
            if changed {
                SourceGate::Rebuild
            } else if failed {
                remember_kept_host(world, entity, every, false);
                SourceGate::Keep
            } else {
                remember_kept_host(world, entity, 0, true);
                SourceGate::Keep
            }
        }
    }
}

fn set_host_error(world: &mut World, entity: Entity, code: String, message: String) {
    tracing::warn!(target: "regia::procgen", %code, %message, "anchored generator cannot be planned");
    if let Some(mut h) = world.get_mut::<ProcGenTileHost>(entity) {
        h.error = Some((code.clone(), message.clone()));
    }
    world
        .resource_mut::<ProcGenTiles>()
        .errors
        .insert(entity, (code, message));
}

/// The anchored scheduler (spec 66 § 4.3): per anchored generator, step the plan, start cell runs
/// (inline or on the compute pool), poll them, spawn resident cells' outputs and release far ones.
pub fn procgen_schedule(world: &mut World) {
    let _span = tracing::info_span!("procgen.schedule").entered();
    // A bake directory changed since the last update (a level change) reopens bakes before planning.
    crate::world::watch_bake_dir(world);
    let mut gens: Vec<(Entity, crate::world::ProcGenerator)> = world
        .query::<(Entity, &crate::world::ProcGenerator)>()
        .iter(world)
        .filter(|(_, g)| is_scheduled(g))
        .map(|(e, g)| (e, g.clone()))
        .collect();
    gens.sort_by_key(|g| g.0);
    // Forget generators that are gone.
    let live: BTreeSet<Entity> = world
        .query_filtered::<Entity, With<crate::world::ProcGenerator>>()
        .iter(world)
        .collect();
    world
        .resource_mut::<ProcGenTiles>()
        .generators
        .retain(|e, _| live.contains(e));
    world
        .resource_mut::<ProcGenTiles>()
        .errors
        .retain(|e, _| live.contains(e));
    let settings = world.resource::<crate::world::ProcGenSettings>().clone();
    let mut running_total: u32 = 0;
    for (entity, generator) in gens {
        if !ensure_host(world, entity) {
            continue;
        }
        let Some(anchor_settings) = generator.tiles.as_ref().and_then(|t| t.runtime.clone()) else {
            continue;
        };
        poll_cells(world, entity);
        let anchors = gather_anchors(world, anchor_settings.anchors);
        let mut budget = ApplyBudget {
            elements: anchor_settings.apply_budget_elements.map(u64::from),
        };
        let plan = {
            let mut tiles = world.resource_mut::<ProcGenTiles>();
            let Some(g) = tiles.generators.get_mut(&entity) else {
                continue;
            };
            g.paused = settings.paused;
            g.step(&anchors, &anchor_settings, &mut budget)
        };
        if plan.memory_capped {
            tracing::info!(target: "regia::procgen", code = "procgen_memory_cap", "resident cells over the memory cap; finest cells released early");
        }
        apply_plan(world, entity, &plan, settings.inline, &mut running_total);
    }
}

fn apply_plan(world: &mut World, entity: Entity, plan: &StepPlan, inline: bool, running: &mut u32) {
    let _span = tracing::info_span!(
        "procgen.apply",
        start = plan.start.len(),
        resident = plan.resident.len()
    )
    .entered();
    let generator_guid = world
        .get::<regia_scene::components::EntityGuid>(entity)
        .map_or(regia_core::id::Guid::NIL, |g| g.0);
    // Cancel and release.
    let mut despawn = Vec::new();
    if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
        for k in &plan.cancel {
            if let Some((t, _)) = host.pending.remove(k) {
                t.cancel();
            }
            host.outputs.remove(k);
        }
        for k in &plan.release {
            host.outputs.remove(k);
            if let Some(r) = host.roots.remove(k) {
                despawn.push(r);
            }
        }
    }
    for r in despawn {
        if let Ok(e) = world.get_entity_mut(r) {
            e.despawn();
        }
    }
    // Start.
    let Some(host) = world.get::<ProcGenTileHost>(entity) else {
        return;
    };
    let (graph, tplan, inputs, bounds, halo, file) = (
        host.graph.clone(),
        host.plan.clone(),
        host.inputs.clone(),
        host.bounds,
        host.halo_m,
        host.file.clone(),
    );
    let cache = world.resource::<crate::world::ProcGenCache>().0.clone();
    let mut started = Vec::new();
    for k in &plan.start {
        let token = CancelToken::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let (graph, tplan, inputs, cache, key, tok, file) = (
            graph.clone(),
            tplan.clone(),
            inputs.clone(),
            cache.clone(),
            *k,
            token.clone(),
            file.clone(),
        );
        let work = move || {
            if let Some(file) = file {
                // The receiver may be gone (generator despawned): nothing to deliver to.
                let _ = tx.send(load_cell(&file, &tplan, key));
                return;
            }
            let ev = Evaluator::with_cache(cache);
            let job = TileJob {
                evaluator: &ev,
                graph: &graph,
                plan: &tplan,
                inputs: &inputs,
                bounds: &bounds,
                halo_m: halo,
            };
            // The receiver may be gone (generator despawned): nothing to deliver to.
            let _ = tx.send(run_cell(&job, key, &tok));
        };
        if inline {
            work();
        } else {
            regia_core::tasks::spawn(async move { work() }).detach();
        }
        *running += 1;
        started.push((*k, token, rx));
    }
    let loads = if file.is_some() {
        plan.start.len() as u64
    } else {
        0
    };
    if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
        for (k, t, rx) in started {
            host.pending.insert(k, (t, StdMutex::new(rx)));
        }
    }
    {
        let mut tiles = world.resource_mut::<ProcGenTiles>();
        if let Some(g) = tiles.generators.get_mut(&entity) {
            g.counters.cells_loaded += loads;
            for k in &plan.start {
                g.set_running(*k);
            }
        }
    }
    if inline {
        poll_cells(world, entity);
    }
    // Cells that became resident: spawn their output entity and prefabs.
    for k in &plan.resident {
        let Some(cell) = world
            .get::<ProcGenTileHost>(entity)
            .and_then(|h| h.outputs.get(k).cloned())
        else {
            continue;
        };
        // A rebuilt cell replaces its previous output entity.
        let old = world
            .get_mut::<ProcGenTileHost>(entity)
            .and_then(|mut h| h.roots.remove(k));
        if let Some(old) = old
            && let Ok(e) = world.get_entity_mut(old)
        {
            e.despawn();
        }
        let output = Arc::clone(&cell.output);
        // The prefab root doubles as the cell's output entity: releasing it removes both.
        let scope = cell_scope(generator_guid, *k);
        let (_, root) =
            crate::world::spawn_prefab_root(world, entity, generator_guid, scope, Some(*k), 0, &output);
        if let Ok(mut e) = world.get_entity_mut(root) {
            e.insert((
                ProcTileCell {
                    generator: entity,
                    key: *k,
                },
                crate::world::ProcOutput(output.clone()),
            ));
        }
        if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
            host.roots.insert(*k, root);
        }
    }
}

/// The guid scope of one tile cell's output (see [`crate::world::prefab_instance_guid`]): derived
/// from the generator's guid, the tier and the cell, so every cell's prefab instances get guids
/// distinct from every other cell's and stable across reloads of the same cell.
pub fn cell_scope(generator: regia_core::id::Guid, key: TileKey) -> regia_core::id::Guid {
    use regia_core::id::Guid;
    let tier = regia_scene::placed::derive_guid(generator, Guid::from_u128(u128::from(key.tier) + 1));
    let cell = (u128::from(key.cell.x as u64) << 64) | u128::from(key.cell.z as u64);
    regia_scene::placed::derive_guid(tier, Guid::from_u128(cell))
}

/// One baked cell from its file: what a cell run would have published (§ 4.3 "Baked + anchors").
fn load_cell(file: &crate::pgout::PgoutFile, plan: &TierPlan, key: TileKey) -> Result<CellOutput, RunError> {
    let _span = tracing::info_span!(
        "procgen.cell_load",
        tier = key.tier,
        i = key.cell.x,
        j = key.cell.z
    )
    .entered();
    let output = baked_cell(file, key)
        .map_err(|e| RunError::Node {
            node: 0,
            code: e.code.to_string(),
            message: e.message,
        })?
        .unwrap_or_default();
    let rect = owned_rect(plan, key).unwrap_or([0.0; 4]);
    Ok(CellOutput {
        key,
        rect,
        run_rect: rect,
        output_hash: output.hash(),
        output: Arc::new(output),
        report: RunReport::default(),
        seam_mismatch_risk: 0,
        clip_dropped: 0,
        debug: None,
    })
}

/// The baked section of `key`, or `None`. The file keys cells by `i32`; a cell index outside that
/// range is not in any file (the low 32 bits would name another cell's payload: audit wave 6,
/// stream L1).
fn baked_cell(
    file: &crate::pgout::PgoutFile,
    key: TileKey,
) -> Result<Option<RunOutput>, crate::pgout::PgoutError> {
    let (Ok(i), Ok(j)) = (i32::try_from(key.cell.x), i32::try_from(key.cell.z)) else {
        return Ok(None);
    };
    file.read_cell(key.tier, i, j)
}

/// Drops a streamed generator's host so the next frame reopens its file (after a bake).
pub(crate) fn reset_host(world: &mut World, entity: Entity) {
    let roots: Vec<Entity> = world
        .get::<ProcGenTileHost>(entity)
        .map(|h| h.roots.values().copied().collect())
        .unwrap_or_default();
    for r in roots {
        if let Ok(e) = world.get_entity_mut(r) {
            e.despawn();
        }
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.remove::<(ProcGenTileHost, crate::world::ProcBakeOrigin)>();
    }
    world.resource_mut::<ProcGenTiles>().errors.remove(&entity);
    world.resource_mut::<ProcGenTiles>().generators.remove(&entity);
}

/// Collects finished cell runs into the host and the scheduler.
fn poll_cells(world: &mut World, entity: Entity) {
    let mut done: Vec<(TileKey, Result<CellOutput, RunError>)> = Vec::new();
    if let Some(mut host) = world.get_mut::<ProcGenTileHost>(entity) {
        let keys: Vec<TileKey> = host.pending.keys().copied().collect();
        for k in keys {
            let r = host.pending.get(&k).and_then(|(_, rx)| {
                let rx = rx.lock().ok()?;
                match rx.try_recv() {
                    Ok(r) => Some(r),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(RunError::Node {
                        node: 0,
                        code: "procgen_run_failed".into(),
                        message: "the cell task ended without a result".into(),
                    })),
                }
            });
            if let Some(r) = r {
                host.pending.remove(&k);
                done.push((k, r));
            }
        }
        for (k, r) in &done {
            if let Ok(c) = r {
                host.outputs.insert(*k, Arc::new(c.clone()));
            }
        }
    }
    let mut tiles = world.resource_mut::<ProcGenTiles>();
    let Some(g) = tiles.generators.get_mut(&entity) else {
        return;
    };
    for (k, r) in done {
        match r {
            Ok(c) => g.finished(
                k,
                c.output.instance_count() as u64 + c.output.prefabs.len() as u64,
                estimate_output_bytes(c.output.as_ref()),
                c.output_hash,
                c.report.total_ms,
            ),
            Err(RunError::Cancelled) => {}
            Err(e) => {
                tracing::warn!(target: "regia::procgen", tier = k.tier, i = k.cell.x, j = k.cell.z, code = e.code(), "cell run failed");
                g.failed(k);
            }
        }
    }
}

#[cfg(test)]
mod baked_index_tests {
    use super::*;

    /// A cell past the `i32` range reads nothing, where its low 32 bits used to read the payload
    /// of the cell at `i32::MIN`.
    #[test]
    fn a_cell_index_past_i32_does_not_read_another_cells_bake() {
        let mut out = RunOutput::default();
        out.tables.insert(
            "t".into(),
            crate::data::Table::one_row([("k".to_string(), crate::data::AttrValue::Int(4))]),
        );
        let header = crate::pgout::PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 9,
            input_hash: [3; 32],
            output_hash: out.hash(),
            quality: 0,
        };
        let bytes = crate::pgout::encode_cells(&header, &[(0, i32::MIN, 0, &out)]);
        let dir = regia_test_utils::unique_temp_dir("procgen-baked-index");
        let path = dir.join("cells.pgout");
        std::fs::write(&path, bytes).expect("write");
        let file = crate::pgout::PgoutFile::open(&path).expect("open");
        let at = |x: i64| TileKey {
            tier: 0,
            cell: CellCoord::new(x, 0),
        };
        let own = baked_cell(&file, at(i64::from(i32::MIN))).expect("read");
        assert_eq!(own.map(|o| o.hash()), Some(out.hash()));
        assert_eq!(baked_cell(&file, at(1 << 31)).expect("read"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod perf_g_tests {
    use super::*;
    use crate::data::GroundData;
    use crate::exec::{StaticSnapshot, ground_hash_samples};

    /// Two cell views of one snapshot walk the heights once.
    #[test]
    fn perf_g_two_cells_share_one_ground_walk() {
        let n = 64 * 64;
        let inner: Arc<dyn WorldSnapshot> = Arc::new(StaticSnapshot {
            ground: Some(Arc::new(GroundData {
                rect: [0.0, 0.0, 64.0, 64.0],
                map_size: [64, 64],
                world_width: 64.0,
                origin: [0, 0],
                size: [64, 64],
                heights: vec![0.25; n],
                layers: Vec::new(),
            })),
            ..StaticSnapshot::default()
        });
        let a = CellSnapshot {
            inner: Arc::clone(&inner),
            self_info: None,
        };
        let b = CellSnapshot {
            inner,
            self_info: None,
        };
        let before = ground_hash_samples();
        let left = a.content_hash();
        let right = b.content_hash();
        assert_eq!(left, right);
        assert_eq!(ground_hash_samples() - before, n as u64);
    }
}
