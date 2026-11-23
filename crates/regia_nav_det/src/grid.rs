// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/grid.rs, commit d6ed37677), ADR-8012.
// Also from the same commit: ir-sim/src/pathing/mod.rs lines 587-604 (the obstacle disc in
// centimeters, now `Stamp::from_world`), 885-940 (the id-sorted stamp merge, now
// `NavGrid::sync_stamps`) and 3128-3136 (the disc that covers a square footprint, now
// `Stamp::covering_square`). The `HeightCells` and `GroundPatch` samplers follow
// ir-content/src/heightmap.rs lines 217-236 and 258-269 and ir-content/src/terrain_patch.rs lines
// 66-89 (commit d6ed37677): bilinear sampling, a patch only lowers the ground; the patch's
// ragged-edge rule and unset cells are not carried over.
// Mark: adapt (plan row S-01, ADR-8049). Every computation that decides a cell is unchanged; the
// named changes are:
// - the movement classes, size classes and profiles were enums with fixed numbers; they are now
//   `GridSettings` data (the preset `GridSettings::hover_ground_jet` has the same numbers);
// - the flat canvas (256 cells of 5 m) is `GridSettings::{cell_m, canvas_cells}`;
// - the game's obstacle kinds no longer reach the grid: a host turns whatever blocks (buildings,
//   wrecks, parked vehicles, debris) into `Stamp`s and hands them to `sync_stamps`;
// - the heightfield type is the `GridGround` trait; the tunnel layer, the crawlway graph and the
//   single kill volume became a host-supplied closed-cell mask and `ExclusionDisc`s;
// - errors replace the panics on an empty ground or bad settings.
// Review R1 / ADR-8193: patch minima and sampler clamps use deterministic float ordering;
// every float bit encoder folds signed zeros and NaNs through the canonical float policy.
// New code, not from the source (ADR-8379, ai-nav-13): areas (`AreaDef`, `paint_area`), the cost
// overlay, `StampShape` and its integer rasterizer, `sync_stamps_from`/`sync_shapes_from` with their
// reused buffers, `generation`, the neutral class/profile names and the test-only
// `clamp_by_compare`. At default settings every computation above is unchanged.

//! The deterministic navigation grid: terrain cost per movement class, dynamic obstacle stamps per
//! size class, and per-cluster regions for a coarse search.
//!
//! One nav cell per ground sample. Cell `i` is centered on sample `i`: world
//! `x = origin_x + i * cell_m`. Cell index `i = z * width + x` (rows along Z, `width` cells per
//! row along X). Everything a decision reads is an integer (costs, counts, labels) or a float
//! built from `+ - * /` and [`regia_detsim::math::sqrt`], so every peer builds the same grid.
//!
//! ```
//! use std::sync::Arc;
//! use regia_nav_det::grid::{GridSettings, NavGrid, NavStatic, Profile, Stamp};
//!
//! # fn main() -> Result<(), regia_nav_det::grid::GridError> {
//! // No heightfield: the flat canvas from the settings (256 x 256 cells of 5 m).
//! let settings = Arc::new(GridSettings::hover_ground_jet());
//! let stat = Arc::new(NavStatic::build(None, settings)?);
//! let mut grid = NavGrid::new(stat)?;
//! let at = grid.cell_at(100.0, 40.0);
//! assert!(grid.passable(Profile::HOVER, at));
//!
//! // A host stamps its obstacles (id-sorted or not) once per tick.
//! let sync = grid.sync_stamps(vec![(7, Stamp::from_world(100.0, 40.0, 6.0))]);
//! assert!(sync.changed);
//! assert!(!grid.passable(Profile::HOVER, at));
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use regia_detsim::float::{canonical, det_clamp, det_min};
use regia_detsim::math;
use serde::{Deserialize, Serialize};

/// Cells per cluster side of the default settings (40 m clusters on 5 m cells); a grid's own size is
/// [`GridSettings::cluster_cells`] ([`NavGrid::cluster_cells`]).
pub const CLUSTER: usize = 8;
/// Most regions one default cluster can hold (one per cell); a grid's own bound is
/// [`NavGrid::max_regions`].
pub const MAX_REGIONS: usize = CLUSTER * CLUSTER;
/// Label of a cell a profile cannot stand in (one-byte labels).
pub const BLOCKED: u8 = 0xFF;
/// [`NavGrid::region_label`] of a cell a profile cannot stand in, whatever the label width.
pub const NO_REGION: u16 = u16::MAX;
/// Largest [`GridSettings::cluster_cells`].
pub const MAX_CLUSTER_CELLS: usize = 128;
/// Cost multiplier of flat, open ground (tenths).
pub const COST_FLAT: u32 = 10;
/// Cost multiplier at the steep limit (4x flat).
pub const COST_STEEP: u32 = 40;
/// Cost multiplier just under the cliff limit (25x flat).
pub const COST_FACE: u32 = 250;

/// Why a grid could not be built or changed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GridError {
    /// `cell_m` is not a finite, positive length.
    #[error("cell size {0} m is not a finite, positive length")]
    BadCellSize(f32),
    /// The flat canvas has fewer than 2 cells on an axis.
    #[error("canvas {0:?} cells: each axis needs at least 2")]
    CanvasTooSmall([usize; 2]),
    /// A settings list that must not be empty is empty (`move_classes`, `size_classes`, `profiles`).
    #[error("settings list `{0}` is empty")]
    EmptyList(&'static str),
    /// More profiles than a `Profile` (`u8`) can name.
    #[error("{0} profiles: at most 255")]
    TooManyProfiles(usize),
    /// A movement class's limits are not `0 <= free < steep < cliff`, all finite.
    #[error("movement class {index}: limits must be finite with 0 <= free < steep < cliff")]
    BadMoveClass {
        /// Index into `GridSettings::move_classes`.
        index: usize,
    },
    /// A size class's inflation or radius bound is negative or not finite (the last bound may be infinite).
    #[error("size class {index}: inflate_m must be finite and >= 0, max_radius_m >= 0")]
    BadSizeClass {
        /// Index into `GridSettings::size_classes`.
        index: usize,
    },
    /// A profile names a movement or size class that does not exist.
    #[error("profile {index} names a missing class (move {move_class}, size {size_class})")]
    ProfileClassMissing {
        /// Index into `GridSettings::profiles`.
        index: usize,
        /// The profile's movement class.
        move_class: u8,
        /// The profile's size class.
        size_class: u8,
    },
    /// The ground has fewer than 2 samples on an axis, or a spacing that is not a positive length.
    #[error("ground {cols} x {rows} samples at {spacing} m: needs 2 x 2 and a positive spacing")]
    GroundTooSmall {
        /// Samples per row (X).
        cols: usize,
        /// Rows (Z).
        rows: usize,
        /// Sample spacing (m).
        spacing: f32,
    },
    /// More cells than a `u32` cell index can name.
    #[error("{0} cells: a cell index is a u32")]
    TooManyCells(usize),
    /// `cluster_cells` is not a power of two from 2 to [`MAX_CLUSTER_CELLS`] (its square must fit
    /// the label type: one byte up to 8, two bytes above).
    #[error("cluster size {0} cells: a power of two from 2 to 128")]
    BadClusterSize(usize),
    /// A closed-cell mask whose length is not the grid's cell count.
    #[error("closed-cell mask has {got} entries, the grid has {cells} cells")]
    ClosedMaskLength {
        /// Entries in the mask.
        got: usize,
        /// Cells in the grid.
        cells: usize,
    },
    /// An area's `cost_pct` is 0 (a cost of 0 means blocked: use `blocked_for` instead).
    #[error("area {index}: cost_pct must be at least 1")]
    BadAreaCost {
        /// Index into `GridSettings::areas` (area id `index + 1`).
        index: usize,
    },
    /// Two areas share a name, or an area is named [`DEFAULT_AREA_NAME`].
    #[error("area name `{0}` is used twice (or is the reserved default name)")]
    DuplicateAreaName(String),
    /// More areas than a one-byte area id can name (area 0 is the default).
    #[error("{0} areas: at most 255")]
    TooManyAreas(usize),
    /// [`NavGrid::paint_area`] named an area id the settings do not define.
    #[error("area id {0} is not defined")]
    AreaMissing(u8),
    /// A cost overlay whose length is not the grid's cell count.
    #[error("cost overlay has {got} entries, the grid has {cells} cells")]
    OverlayLength {
        /// Entries in the overlay.
        got: usize,
        /// Cells in the grid.
        cells: usize,
    },
    /// A paint polygon with fewer than 3 vertices or no area.
    #[error("paint polygon with {0} vertices has no area")]
    BadPolygon(usize),
}

/// Movement class: slope limits as rise over run.
///
/// Ground up to `free` costs flat; the cost climbs to 4x at `steep` and to 25x at `cliff`; steeper
/// ground is a cliff (a move up or down it is refused).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MoveClass {
    /// Steepness that still costs flat.
    pub free: f32,
    /// Steepness at which the cost reaches 4x flat.
    pub steep: f32,
    /// Steepest ground the class can stand on or cross; steeper is a cliff.
    pub cliff: f32,
}

impl MoveClass {
    /// The least slope-tolerant class of [`GridSettings::three_class`]: flat to 0.35, 4x at 0.53,
    /// cliff past 0.78 (about 38 degrees). Movers that ride most ground but stop at true cliffs.
    #[doc(alias = "HOVER")]
    pub const CLIMB_LOW: MoveClass = MoveClass::new(0.35, 0.53, 0.78);
    /// The middle class of [`GridSettings::three_class`]: flat to 0.30, 4x at 0.75, cliff past 1.5
    /// (about 56 degrees). Walkers and people on foot.
    #[doc(alias = "GROUND")]
    pub const CLIMB_MID: MoveClass = MoveClass::new(0.30, 0.75, 1.5);
    /// The most slope-tolerant class of [`GridSettings::three_class`]: flat to 0.60, 4x at 2.0,
    /// cliff past 4.0. Jumping or jet-assisted movers that hop up steps a walker cannot.
    #[doc(alias = "JET")]
    pub const CLIMB_HIGH: MoveClass = MoveClass::new(0.60, 2.0, 4.0);
    /// The name [`Self::CLIMB_LOW`] had in the source game (hovering vehicles); same bits.
    pub const HOVER: MoveClass = MoveClass::CLIMB_LOW;
    /// The name [`Self::CLIMB_MID`] had in the source game (walkers, people on foot); same bits.
    pub const GROUND: MoveClass = MoveClass::CLIMB_MID;
    /// The name [`Self::CLIMB_HIGH`] had in the source game (jet-assisted movers); same bits.
    pub const JET: MoveClass = MoveClass::CLIMB_HIGH;

    /// A class from its three limits.
    pub const fn new(free: f32, steep: f32, cliff: f32) -> MoveClass {
        MoveClass { free, steep, cliff }
    }

    /// Cost multiplier (tenths) for a cell of this steepness; 0 is a cliff.
    pub fn cost(&self, steep: f32) -> u8 {
        let (free, soft, hard) = (self.free, self.steep, self.cliff);
        if steep > hard {
            return 0;
        }
        if steep <= free {
            return COST_FLAT as u8;
        }
        if steep <= soft {
            let t = (steep - free) / (soft - free);
            let extra = (t * (COST_STEEP - COST_FLAT) as f32) as u32;
            return (COST_FLAT + extra.min(COST_STEEP - COST_FLAT)) as u8;
        }
        let t = (steep - soft) / (hard - soft);
        let extra = (t * (COST_FACE - COST_STEEP) as f32) as u32;
        (COST_STEEP + extra.min(COST_FACE - COST_STEEP)) as u8
    }

    fn is_valid(&self) -> bool {
        self.free.is_finite()
            && self.steep.is_finite()
            && self.cliff.is_finite()
            && self.free >= 0.0
            && self.free < self.steep
            && self.steep < self.cliff
    }
}

/// Obstacle inflation class: a mover's center must stay this far outside every stamped disc.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SizeClass {
    /// Inflation added to every stamped disc's radius on this class's layer (m).
    pub inflate_m: f32,
    /// Largest collision radius (m) [`GridSettings::size_for_radius`] maps to this class. The
    /// classes are tried in order; a radius past every bound takes the last class.
    pub max_radius_m: f32,
}

impl SizeClass {
    /// People on foot (radius up to 1.5 m, 1 m inflation).
    pub const SMALL: SizeClass = SizeClass {
        inflate_m: 1.0,
        max_radius_m: 1.5,
    };
    /// Vehicles up to 5 m radius (4.5 m inflation).
    pub const MEDIUM: SizeClass = SizeClass {
        inflate_m: 4.5,
        max_radius_m: 5.0,
    };
    /// Large vehicles past 5 m radius (17.5 m inflation).
    pub const LARGE: SizeClass = SizeClass {
        inflate_m: 17.5,
        max_radius_m: f32::INFINITY,
    };
}

/// One profile: a movement class and a size class, by index into [`GridSettings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDef {
    /// Index into [`GridSettings::move_classes`].
    pub move_class: u8,
    /// Index into [`GridSettings::size_classes`].
    pub size_class: u8,
}

/// A movement class and a size: what one mover can stand on. An index into
/// [`GridSettings::profiles`]; an index past the end means the last profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct Profile(pub u8);

impl Profile {
    /// [`GridSettings::three_class`] profile 0: [`MoveClass::CLIMB_LOW`] with the medium size
    /// class (up to 5 m radius).
    #[doc(alias = "HOVER")]
    pub const LOW_CLIMB_MEDIUM: Profile = Profile(0);
    /// [`GridSettings::three_class`] profile 1: [`MoveClass::CLIMB_LOW`] with the large size class.
    #[doc(alias = "HOVER_LARGE")]
    pub const LOW_CLIMB_LARGE: Profile = Profile(1);
    /// [`GridSettings::three_class`] profile 2: [`MoveClass::CLIMB_MID`] with the small size class.
    #[doc(alias = "FOOT")]
    pub const MID_CLIMB_SMALL: Profile = Profile(2);
    /// [`GridSettings::three_class`] profile 3: [`MoveClass::CLIMB_MID`] with the medium size class.
    #[doc(alias = "WALKER")]
    pub const MID_CLIMB_MEDIUM: Profile = Profile(3);
    /// [`GridSettings::three_class`] profile 4: [`MoveClass::CLIMB_HIGH`] with the small size class.
    #[doc(alias = "JET")]
    pub const HIGH_CLIMB_SMALL: Profile = Profile(4);
    /// The source game's name for [`Self::LOW_CLIMB_MEDIUM`] (hovering vehicles up to 5 m radius).
    pub const HOVER: Profile = Profile::LOW_CLIMB_MEDIUM;
    /// The source game's name for [`Self::LOW_CLIMB_LARGE`] (large hovering vehicles).
    pub const HOVER_LARGE: Profile = Profile::LOW_CLIMB_LARGE;
    /// The source game's name for [`Self::MID_CLIMB_SMALL`] (a person on foot).
    pub const FOOT: Profile = Profile::MID_CLIMB_SMALL;
    /// The source game's name for [`Self::MID_CLIMB_MEDIUM`] (a walking machine).
    pub const WALKER: Profile = Profile::MID_CLIMB_MEDIUM;
    /// The source game's name for [`Self::HIGH_CLIMB_SMALL`] (a person with a jetpack).
    pub const JET: Profile = Profile::HIGH_CLIMB_SMALL;
}

/// A set of profiles by index (256 bits: every [`Profile`] a `u8` can name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct ProfileMask(pub [u64; 4]);

impl ProfileMask {
    /// No profile.
    pub const NONE: ProfileMask = ProfileMask([0; 4]);
    /// Every profile.
    pub const ALL: ProfileMask = ProfileMask([u64::MAX; 4]);

    /// The mask holding exactly these profiles.
    pub fn of(profiles: &[Profile]) -> ProfileMask {
        profiles.iter().fold(ProfileMask::NONE, |m, p| m.with(*p))
    }

    /// This mask plus `profile`.
    #[must_use]
    pub const fn with(mut self, profile: Profile) -> ProfileMask {
        let i = profile.0 as usize;
        self.0[i / 64] |= 1u64 << (i % 64);
        self
    }

    /// True when `profile` (by its raw index) is in the mask.
    #[inline]
    pub const fn contains(&self, profile: Profile) -> bool {
        let i = profile.0 as usize;
        self.0[i / 64] & (1u64 << (i % 64)) != 0
    }
}

/// Name of area 0, the implicit default area (100 % cost, blocks nobody).
pub const DEFAULT_AREA_NAME: &str = "default";
/// Most areas [`GridSettings::areas`] may define (ids 1..=255; 0 is the default).
pub const MAX_AREAS: usize = 255;

/// An area type: a cost scale and the profiles it blocks, painted onto cells with
/// [`NavGrid::paint_area`] (roads, mud, water, a no-go zone for some movers).
///
/// A cell's cost for a profile is `max(1, (terrain * cost_pct + 50) / 100)` (integer math, half
/// rounds up; a cliff stays a cliff), or blocked when `blocked_for` holds the profile.
///
/// ```
/// use std::sync::Arc;
/// use regia_nav_det::grid::{
///     AreaDef, CostOverlay, GridSettings, NavGrid, NavStatic, PaintRegion, Profile, ProfileMask,
/// };
///
/// # fn main() -> Result<(), regia_nav_det::grid::GridError> {
/// let mut settings = GridSettings::three_class();
/// settings.areas.push(AreaDef { name: "road".into(), cost_pct: 50, blocked_for: ProfileMask::NONE });
/// settings.areas.push(AreaDef {
///     name: "deep_water".into(),
///     cost_pct: 100,
///     blocked_for: ProfileMask::of(&[Profile::MID_CLIMB_SMALL]),
/// });
/// let stat = Arc::new(NavStatic::build(None, Arc::new(settings))?);
/// let mut grid = NavGrid::new(stat)?;
/// let road = grid.settings().area_id("road").unwrap_or(0);
/// grid.paint_area(&PaintRegion::CellRect { min: [0, 100], max: [255, 101] }, road)?;
/// assert_eq!(grid.cost(Profile::MID_CLIMB_SMALL, grid.index(10, 100)), 5);
///
/// // A danger zone for one profile, rewritten whenever the game's threat map changes.
/// let mut cells = vec![0u16; grid.width() * grid.height()];
/// cells[grid.index(20, 20) as usize] = 400;
/// let threat = CostOverlay { profiles: ProfileMask::of(&[Profile::MID_CLIMB_SMALL]), cells };
/// let before = grid.generation();
/// grid.set_overlay(Some(Arc::new(threat)))?;
/// assert!(grid.generation() > before);
/// assert_eq!(grid.cost(Profile::MID_CLIMB_SMALL, grid.index(20, 20)), 410);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaDef {
    /// Unique name (not [`DEFAULT_AREA_NAME`]); `GridSettings::area_id` finds it.
    pub name: String,
    /// Cost scale in percent of the terrain cost (100 = unchanged, 50 = a road at half cost). At
    /// least 1.
    pub cost_pct: u16,
    /// Profiles that cannot stand in this area at all.
    #[serde(default)]
    pub blocked_for: ProfileMask,
}

/// The grid's settings: the flat canvas, the movement and size classes, and the profiles.
///
/// Every peer of a lockstep match must use the same settings: they decide every cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridSettings {
    /// Cell size (m) of the flat canvas used when there is no ground. A loaded ground uses its own
    /// sample spacing.
    pub cell_m: f32,
    /// Flat canvas size in cells `[x, z]`, used when there is no ground and no declared extent.
    pub canvas_cells: [usize; 2],
    /// Movement classes (slope limits), indexed by [`ProfileDef::move_class`].
    pub move_classes: Vec<MoveClass>,
    /// Size classes (obstacle inflation), indexed by [`ProfileDef::size_class`]. Each stamped disc
    /// lands on every size layer.
    pub size_classes: Vec<SizeClass>,
    /// Profiles, indexed by [`Profile`].
    pub profiles: Vec<ProfileDef>,
    /// Cells per cluster side for the coarse search (default 8, DO27's 40 m on 5 m cells). A power
    /// of two from 2 to [`MAX_CLUSTER_CELLS`]. Up to 8 the labels are one byte (at most 64 regions
    /// per cluster); 16 and above store two-byte labels ([`LabelWidth::U16`]). Larger clusters mean
    /// fewer coarse nodes on a big map and a wider corridor for the cell search.
    #[serde(default = "default_cluster_cells")]
    pub cluster_cells: usize,
    /// Area types painted with [`NavGrid::paint_area`]: `areas[k]` is area id `k + 1`; id 0 is the
    /// implicit default (100 %, blocks nobody). Empty (the default) keeps every cost the terrain's.
    #[serde(default)]
    pub areas: Vec<AreaDef>,
}

fn default_cluster_cells() -> usize {
    CLUSTER
}

/// How many bytes a grid stores per region label ([`GridSettings::label_width`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelWidth {
    /// One byte: clusters up to 8 x 8 (labels 0..=63, [`BLOCKED`] for a blocked cell).
    U8,
    /// Two bytes: clusters of 16 x 16 and more (labels up to `cluster_cells^2 - 1`).
    U16,
}

impl Default for GridSettings {
    fn default() -> Self {
        GridSettings::three_class()
    }
}

impl GridSettings {
    /// The source game's name for [`Self::three_class`]; the same settings bit for bit.
    pub fn hover_ground_jet() -> GridSettings {
        GridSettings::three_class()
    }

    /// The preset with three movement classes ([`MoveClass::CLIMB_LOW`], [`MoveClass::CLIMB_MID`],
    /// [`MoveClass::CLIMB_HIGH`]), three size classes ([`SizeClass::SMALL`], [`SizeClass::MEDIUM`],
    /// [`SizeClass::LARGE`]), the five [`Profile`] constants, a 1280 m canvas of 5 m cells and no
    /// areas.
    #[doc(alias = "hover_ground_jet")]
    pub fn three_class() -> GridSettings {
        GridSettings {
            cell_m: 5.0,
            canvas_cells: [256, 256],
            move_classes: vec![MoveClass::CLIMB_LOW, MoveClass::CLIMB_MID, MoveClass::CLIMB_HIGH],
            size_classes: vec![SizeClass::SMALL, SizeClass::MEDIUM, SizeClass::LARGE],
            profiles: vec![
                ProfileDef {
                    move_class: 0,
                    size_class: 1,
                },
                ProfileDef {
                    move_class: 0,
                    size_class: 2,
                },
                ProfileDef {
                    move_class: 1,
                    size_class: 0,
                },
                ProfileDef {
                    move_class: 1,
                    size_class: 1,
                },
                ProfileDef {
                    move_class: 2,
                    size_class: 0,
                },
            ],
            cluster_cells: CLUSTER,
            areas: Vec::new(),
        }
    }

    /// The label width the grid stores for these settings: one byte while `cluster_cells^2` stays
    /// below the two reserved values (blocked and the flood's open mark), two bytes above.
    pub fn label_width(&self) -> LabelWidth {
        // Labels 0..=253: 254 is the flood's open mark, 255 a blocked cell.
        if self.cluster_cells.saturating_mul(self.cluster_cells) < usize::from(u8::MAX) {
            LabelWidth::U8
        } else {
            LabelWidth::U16
        }
    }

    /// The id of the area named `name` ([`DEFAULT_AREA_NAME`] is 0), or `None`.
    pub fn area_id(&self, name: &str) -> Option<u8> {
        if name == DEFAULT_AREA_NAME {
            return Some(0);
        }
        let k = self.areas.iter().position(|a| a.name == name)?;
        u8::try_from(k + 1).ok()
    }

    /// The smallest cost scale any area applies (100 without areas). A search whose heuristic
    /// assumes flat ground costs [`COST_FLAT`] per cell scales it by this to stay admissible.
    pub fn min_area_pct(&self) -> u16 {
        self.areas.iter().map(|a| a.cost_pct).fold(100, u16::min)
    }

    /// Check the settings; [`NavStatic::build`] calls this.
    pub fn validate(&self) -> Result<(), GridError> {
        if !positive_length(self.cell_m) {
            return Err(GridError::BadCellSize(self.cell_m));
        }
        if self.canvas_cells[0] < 2 || self.canvas_cells[1] < 2 {
            return Err(GridError::CanvasTooSmall(self.canvas_cells));
        }
        if self.move_classes.is_empty() {
            return Err(GridError::EmptyList("move_classes"));
        }
        if self.size_classes.is_empty() {
            return Err(GridError::EmptyList("size_classes"));
        }
        if self.profiles.is_empty() {
            return Err(GridError::EmptyList("profiles"));
        }
        if self.profiles.len() > 255 {
            return Err(GridError::TooManyProfiles(self.profiles.len()));
        }
        let k = self.cluster_cells;
        // A power of two keeps the cluster of a cell a shift; its square must fit the label type
        // (128^2 = 16384 regions < u16::MAX - 1), and a local coordinate fits a byte.
        if !k.is_power_of_two() || !(2..=MAX_CLUSTER_CELLS).contains(&k) {
            return Err(GridError::BadClusterSize(k));
        }
        for (index, class) in self.move_classes.iter().enumerate() {
            if !class.is_valid() {
                return Err(GridError::BadMoveClass { index });
            }
        }
        for (index, class) in self.size_classes.iter().enumerate() {
            let ok = class.inflate_m.is_finite() && class.inflate_m >= 0.0 && class.max_radius_m >= 0.0;
            if !ok {
                return Err(GridError::BadSizeClass { index });
            }
        }
        for (index, p) in self.profiles.iter().enumerate() {
            if usize::from(p.move_class) >= self.move_classes.len()
                || usize::from(p.size_class) >= self.size_classes.len()
            {
                return Err(GridError::ProfileClassMissing {
                    index,
                    move_class: p.move_class,
                    size_class: p.size_class,
                });
            }
        }
        let cells = self.canvas_cells[0].saturating_mul(self.canvas_cells[1]);
        if cells > u32::MAX as usize {
            return Err(GridError::TooManyCells(cells));
        }
        if self.areas.len() > MAX_AREAS {
            return Err(GridError::TooManyAreas(self.areas.len()));
        }
        for (index, area) in self.areas.iter().enumerate() {
            if area.cost_pct == 0 {
                return Err(GridError::BadAreaCost { index });
            }
            let reused =
                area.name == DEFAULT_AREA_NAME || self.areas[..index].iter().any(|a| a.name == area.name);
            if reused {
                return Err(GridError::DuplicateAreaName(area.name.clone()));
            }
        }
        Ok(())
    }

    /// The size class for a collision radius (m): the first class whose `max_radius_m` holds it,
    /// else the last class.
    pub fn size_for_radius(&self, radius: f32) -> u8 {
        let last = self.size_classes.len().saturating_sub(1);
        let index = self
            .size_classes
            .iter()
            .position(|c| radius <= c.max_radius_m)
            .unwrap_or(last);
        index as u8
    }

    /// Index of a profile into [`Self::profiles`] (an index past the end is the last profile).
    #[inline]
    pub fn profile_index(&self, profile: Profile) -> usize {
        usize::from(profile.0).min(self.profiles.len().saturating_sub(1))
    }

    /// The profile's definition.
    #[inline]
    pub fn profile(&self, profile: Profile) -> ProfileDef {
        self.profiles[self.profile_index(profile)]
    }

    /// Every profile, in index order.
    pub fn all_profiles(&self) -> impl Iterator<Item = Profile> + use<> {
        (0..self.profiles.len()).map(|p| Profile(p as u8))
    }

    /// The canvas size in meters `[x, z]`.
    pub fn canvas_m(&self) -> [f32; 2] {
        [
            self.canvas_cells[0] as f32 * self.cell_m,
            self.canvas_cells[1] as f32 * self.cell_m,
        ]
    }
}

/// The ground a grid is built from: a regular heightfield, row-major, centered on the origin by
/// `half_x`/`half_z`. A host implements it for its own heightfield type.
///
/// Every peer must answer with the same bits: implement the queries with `+ - * /` (and
/// [`regia_detsim::math`]) in a fixed order.
pub trait GridGround {
    /// Samples per row (X).
    fn cols(&self) -> usize;
    /// Rows (Z).
    fn rows(&self) -> usize;
    /// Meters between adjacent samples (the same on X and Z).
    fn spacing(&self) -> f32;
    /// World half-extent along X: sample 0 is at `x = -half_x`.
    fn half_x(&self) -> f32;
    /// World half-extent along Z: sample 0 is at `z = -half_z`.
    fn half_z(&self) -> f32;
    /// The height subtracted from every raw sample (part of the grid's key).
    fn center_height(&self) -> f32;
    /// Height (m) of sample `(x, z)`.
    fn height_at_index(&self, x: usize, z: usize) -> f32;
    /// Height (m) at a world point, with any baked ground patches applied. Only read when
    /// [`Self::has_patches`] is true.
    fn height_at_world(&self, x: f32, z: f32) -> f32;
    /// True when baked ground patches (for example a lowered ramp or funnel) lie over the samples.
    fn has_patches(&self) -> bool {
        false
    }
    /// A hash of the patches ([`patch_signature`] over them), 0 without patches. Part of the key.
    fn patch_signature(&self) -> u64 {
        0
    }
}

/// The patch hash a [`GridGround`] reports: for each patch `(cols, rows, heights)` in order, its
/// shape and every 7th height's canonical bits, FNV-style (NaNs and signed zeros folded).
pub fn patch_signature<'a>(patches: impl IntoIterator<Item = (usize, usize, &'a [f32])>) -> u64 {
    let mut sig = 0u64;
    for (cols, rows, heights) in patches {
        sig = sig.wrapping_mul(0x100_0000_01b3) ^ (cols * 4099 + rows) as u64;
        for h in heights.iter().step_by(7) {
            sig = sig.wrapping_mul(0x100_0000_01b3) ^ u64::from(canonical(*h).to_bits());
        }
    }
    sig
}

/// What a grid was built from. A different key rebuilds it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GroundKey {
    /// Cells per row (X).
    pub width: usize,
    /// Rows (Z).
    pub height: usize,
    /// Cell size (m).
    pub spacing: f32,
    /// World half-extent along X.
    pub half_x: f32,
    /// World half-extent along Z.
    pub half_z: f32,
    /// The ground's center height.
    pub center_height: f32,
    /// A hash of 64 samples on a diagonal lattice (0 for a flat canvas).
    pub probe: u64,
    /// Hash of the baked ground patches, 0 without any.
    pub patch_sig: u64,
    /// True for a flat canvas (no ground).
    pub flat: bool,
}

impl GroundKey {
    /// The flat canvas of the settings. Returned for a missing ground and for a declared extent
    /// equal to the canvas, so the key matches [`Self::of`]`(None, ..)` bit for bit.
    pub fn flat_canvas(settings: &GridSettings) -> GroundKey {
        let [cols, rows] = settings.canvas_cells;
        GroundKey {
            width: cols,
            height: rows,
            spacing: settings.cell_m,
            half_x: cols as f32 * settings.cell_m * 0.5,
            half_z: rows as f32 * settings.cell_m * 0.5,
            center_height: 0.0,
            probe: 0,
            patch_sig: 0,
            flat: true,
        }
    }

    /// Flat key for a declared playfield in meters. Both sides within 1e-3 of the canvas (or a
    /// non-finite size) return [`Self::flat_canvas`] without rounding; otherwise the cells are
    /// `round(side / cell_m)`, at least 2.
    pub fn flat_for_extent(width_m: f32, depth_m: f32, settings: &GridSettings) -> GroundKey {
        let [canvas_w, canvas_d] = settings.canvas_m();
        if !width_m.is_finite()
            || !depth_m.is_finite()
            || ((width_m - canvas_w).abs() <= 1.0e-3 && (depth_m - canvas_d).abs() <= 1.0e-3)
        {
            return Self::flat_canvas(settings);
        }
        let cell_m = settings.cell_m;
        let cols = ((width_m / cell_m).round() as usize).max(2);
        let rows = ((depth_m / cell_m).round() as usize).max(2);
        GroundKey {
            width: cols,
            height: rows,
            spacing: cell_m,
            half_x: cols as f32 * cell_m * 0.5,
            half_z: rows as f32 * cell_m * 0.5,
            center_height: 0.0,
            probe: 0,
            patch_sig: 0,
            flat: true,
        }
    }

    /// Key for a ground, or the flat canvas without one.
    pub fn of(ground: Option<&dyn GridGround>, settings: &GridSettings) -> GroundKey {
        Self::covering(ground, None, settings)
    }

    /// Key for a ground, or for a declared `[width, depth]` (m) when the ground is missing. A
    /// loaded ground wins: the declared size cannot drift the grid.
    pub fn covering(
        ground: Option<&dyn GridGround>,
        extent: Option<[f32; 2]>,
        settings: &GridSettings,
    ) -> GroundKey {
        match ground {
            None => match extent {
                Some([w, d]) => Self::flat_for_extent(w, d, settings),
                None => Self::flat_canvas(settings),
            },
            Some(t) => {
                // 64 samples on a diagonal lattice: cheap, and a new map changes them.
                let width = t.cols();
                let height = t.rows();
                let mut probe = 0xcbf2_9ce4_8422_2325u64;
                for k in 0..64usize {
                    let x = (k * 37) % width;
                    let z = (k * 101 + 7) % height;
                    probe ^= u64::from(canonical(t.height_at_index(x, z)).to_bits());
                    probe = probe.wrapping_mul(0x100_0000_01b3);
                }
                GroundKey {
                    width,
                    height,
                    spacing: t.spacing(),
                    half_x: t.half_x(),
                    half_z: t.half_z(),
                    center_height: t.center_height(),
                    probe,
                    patch_sig: t.patch_signature(),
                    flat: false,
                }
            }
        }
    }
}

/// The terrain half: built once per map, shared between clones of the grid.
///
/// Rectangular maps are supported: `width` cells per row (X), `height` rows (Z), one cell size on
/// both axes. Cell index `i = z * width + x`.
#[derive(Debug)]
pub struct NavStatic {
    /// Cells per row (X); also the row stride.
    pub width: usize,
    /// Rows (Z).
    pub height: usize,
    /// Cell size (m).
    pub cell_m: f32,
    /// World X of cell 0's center.
    pub origin_x: f32,
    /// World Z of cell 0's center.
    pub origin_z: f32,
    /// Per movement class: cost multiplier per cell from its own slope, 0 = a face too steep to
    /// stand on.
    pub cost: Vec<Vec<u8>>,
    /// Per movement class: bit k set when the move `NEIGHBOURS_8[k]` out of the cell stays under
    /// the class's cliff limit (rise over the move's run).
    pub edges: Vec<Vec<u8>>,
    /// Slope per cell (central differences, rise over run), for costs and debug views.
    pub steep: Vec<f32>,
    /// Ground height per cell (m).
    pub height_cells: Vec<f32>,
    /// 1 where a baked ground patch lowers the ground by more than 0.5 m (a drawn ramp or funnel),
    /// else 0.
    pub lowered: Vec<u8>,
    /// What the grid was built from.
    pub key: GroundKey,
    /// The settings the grid was built with.
    pub settings: Arc<GridSettings>,
}

impl NavStatic {
    /// Build from a ground, or from the settings' flat canvas without one.
    pub fn build(
        ground: Option<&dyn GridGround>,
        settings: Arc<GridSettings>,
    ) -> Result<NavStatic, GridError> {
        Self::build_sized(ground, None, settings)
    }

    /// As [`Self::build`], using `declared` (`[width, depth]` in m) only when `ground` is `None`.
    pub fn build_sized(
        ground: Option<&dyn GridGround>,
        declared: Option<[f32; 2]>,
        settings: Arc<GridSettings>,
    ) -> Result<NavStatic, GridError> {
        settings.validate()?;
        if let Some(t) = ground {
            let (cols, rows, spacing) = (t.cols(), t.rows(), t.spacing());
            if cols < 2 || rows < 2 || !positive_length(spacing) {
                return Err(GridError::GroundTooSmall { cols, rows, spacing });
            }
        }
        let key = GroundKey::covering(ground, declared, &settings);
        let width = key.width;
        let height = key.height;
        let n = width.saturating_mul(height);
        if n > u32::MAX as usize {
            return Err(GridError::TooManyCells(n));
        }
        let cell_m = key.spacing;
        let origin_x = -key.half_x;
        let origin_z = -key.half_z;
        let mut heights = vec![0.0f32; n];
        let mut lowered = vec![0u8; n];
        if let Some(t) = ground {
            for z in 0..height {
                for x in 0..width {
                    heights[z * width + x] = t.height_at_index(x, z);
                }
            }
            // A drawn patch is walkable ground: take the baked patch height at the cell center (a
            // patch only lowers the ground), so slopes and edges follow it instead of reading it as
            // a cliff down to the ramp.
            if t.has_patches() {
                for z in 0..height {
                    for x in 0..width {
                        let (wx, wz) = (origin_x + x as f32 * cell_m, origin_z + z as f32 * cell_m);
                        let h = &mut heights[z * width + x];
                        let patched = t.height_at_world(wx, wz);
                        if patched < *h - 0.5 {
                            lowered[z * width + x] = 1;
                        }
                        *h = det_min(*h, patched);
                    }
                }
            }
        }
        let classes = settings.move_classes.len();
        let diag = cell_m * math::sqrt(2.0);
        let mut steep = vec![0.0f32; n];
        let mut cost: Vec<Vec<u8>> = vec![vec![COST_FLAT as u8; n]; classes];
        let mut edges: Vec<Vec<u8>> = vec![vec![0xFF; n]; classes];
        if ground.is_some() {
            let h = |x: usize, z: usize| heights[z * width + x];
            for z in 0..height {
                for x in 0..width {
                    // Slope of the cell itself: central differences (one-sided at the border).
                    let (xa, xb) = (x.saturating_sub(1), (x + 1).min(width - 1));
                    let (za, zb) = (z.saturating_sub(1), (z + 1).min(height - 1));
                    let x_span = (xb - xa).max(1);
                    let z_span = (zb - za).max(1);
                    let gx = (h(xb, z) - h(xa, z)) / (cell_m * x_span as f32);
                    let gz = (h(x, zb) - h(x, za)) / (cell_m * z_span as f32);
                    steep[z * width + x] = math::sqrt(gx * gx + gz * gz);
                }
            }
            for (t, class) in settings.move_classes.iter().enumerate() {
                let hard = class.cliff;
                for z in 0..height {
                    for x in 0..width {
                        let i = z * width + x;
                        cost[t][i] = class.cost(steep[i]);
                        let mut mask = 0u8;
                        for (k, (dx, dz)) in NEIGHBOURS_8.iter().enumerate() {
                            let nx = x as i32 + dx;
                            let nz = z as i32 + dz;
                            if nx < 0 || nz < 0 || nx >= width as i32 || nz >= height as i32 {
                                continue;
                            }
                            let run = if *dx != 0 && *dz != 0 { diag } else { cell_m };
                            let rise = (heights[nz as usize * width + nx as usize] - heights[i]).abs();
                            if rise <= hard * run {
                                mask |= 1 << k;
                            }
                        }
                        edges[t][i] = mask;
                    }
                }
            }
        }
        Ok(NavStatic {
            width,
            height,
            cell_m,
            origin_x,
            origin_z,
            cost,
            edges,
            steep,
            height_cells: heights,
            lowered,
            key,
            settings,
        })
    }

    /// FNV-1a 64 over every field that decides a cell (shape, origin, costs, edges, slopes,
    /// heights, lowered flags), with NaNs and signed zeros folded. The same on every peer and
    /// architecture for the same inputs.
    pub fn bits_hash(&self) -> u64 {
        let mut h = Fnv::new();
        h.u64(self.width as u64);
        h.u64(self.height as u64);
        h.f32(self.cell_m);
        h.f32(self.origin_x);
        h.f32(self.origin_z);
        for layer in self.cost.iter().chain(self.edges.iter()) {
            h.bytes(layer);
        }
        for v in self.steep.iter().chain(self.height_cells.iter()) {
            h.f32(*v);
        }
        h.bytes(&self.lowered);
        h.finish()
    }
}

/// Index into [`NEIGHBOURS_8`] for a unit step, or `None`.
#[inline]
pub fn dir_index(dx: i32, dz: i32) -> Option<usize> {
    NEIGHBOURS_8.iter().position(|d| *d == (dx, dz))
}

/// E, W, S, N, then the diagonals. Search order is part of determinism.
pub const NEIGHBOURS_8: [(i32, i32); 8] = [
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, 1),
    (-1, 1),
    (1, -1),
    (-1, -1),
];

/// Regions of one profile inside one cluster: index = local region id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionSet {
    /// Representative cell (nearest the region's centroid).
    pub reps: Vec<u32>,
    /// Mean cost multiplier of the region's cells.
    pub costs: Vec<u16>,
}

/// One obstacle disc, in centimeters so a stamp compares exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Stamp {
    /// Center X (cm).
    pub x_cm: i32,
    /// Center Z (cm).
    pub z_cm: i32,
    /// Radius (cm), before each size class's inflation.
    pub r_cm: i32,
}

impl Stamp {
    /// A disc of `radius` meters at world `(x, z)`, each rounded to the nearest centimeter.
    pub fn from_world(x: f32, z: f32, radius: f32) -> Stamp {
        Stamp {
            x_cm: (x * 100.0).round() as i32,
            z_cm: (z * 100.0).round() as i32,
            r_cm: (radius * 100.0).round() as i32,
        }
    }

    /// The disc that covers a square footprint `side_m` wide centered at world `(x, z)`: radius
    /// `side * 0.725` (half the side times 1.45, more than the half diagonal).
    pub fn covering_square(x: f32, z: f32, side_m: f32) -> Stamp {
        Stamp {
            x_cm: (x * 100.0).round() as i32,
            z_cm: (z * 100.0).round() as i32,
            r_cm: (side_m * 72.5).round() as i32,
        }
    }
}

/// One in [`Q14_ONE`]: the fixed-point scale of [`StampShape::OrientedBox`]'s rotation.
pub const Q14_ONE: i32 = 1 << 14;
/// Shape coordinates, computed corners, cell centers and inflations are clamped to this many
/// centimeters (about 10,700 km) either way, so every product of the integer tests fits an `i128`.
pub const SHAPE_LIMIT_CM: i64 = 1 << 30;

/// An obstacle of any shape for [`NavGrid::sync_shapes_from`], in centimeters.
///
/// [`StampShape::Disc`] rasterizes exactly as [`Stamp`] does today. The other shapes are tested
/// with integers: a cell is covered on a size layer when its center (cm) lies strictly inside the
/// shape, or strictly closer than the size class's `inflate_m` (rounded to cm) to its boundary;
/// that is, inside the shape grown by the inflation, the rule discs use.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum StampShape {
    /// A disc, rasterized like [`NavGrid::stamp`].
    Disc(Stamp),
    /// An axis-aligned square of half side `half_cm` centered at `(x_cm, z_cm)` (not the covering
    /// disc of [`Stamp::covering_square`]: the square itself).
    Square {
        /// Center X (cm).
        x_cm: i32,
        /// Center Z (cm).
        z_cm: i32,
        /// Half the side (cm).
        half_cm: i32,
    },
    /// A rotated box: half extents along its own X and Z, turned by `(cos, sin)` in units of
    /// [`Q14_ONE`]. Corner `(sx * half_x, sz * half_z)` lands at
    /// `center + round((sx*half_x*cos - sz*half_z*sin) / Q14_ONE, (sx*half_x*sin + sz*half_z*cos) / Q14_ONE)`,
    /// half away from zero; the box is that four-corner polygon.
    OrientedBox {
        /// Center X (cm).
        x_cm: i32,
        /// Center Z (cm).
        z_cm: i32,
        /// Half extent along the box's own X (cm).
        half_x_cm: i32,
        /// Half extent along the box's own Z (cm).
        half_z_cm: i32,
        /// `(cos, sin)` of the turn from world X to the box's X, in units of [`Q14_ONE`].
        cos_sin_q: [i16; 2],
    },
    /// A convex polygon, vertices in cm, either winding. A polygon with fewer than 3 vertices or
    /// no area has no inside: only the inflation band around its points and edges is covered.
    /// A non-convex list covers the intersection of its edges' inner half-planes plus that band.
    ConvexPolygon(Vec<[i32; 2]>),
}

impl StampShape {
    /// A box `half_x` by `half_z` meters centered at world `(x, z)` whose own X axis points along
    /// `(dir_x, dir_z)` (any length; a zero or non-finite direction means world X). Rounded to cm
    /// and [`Q14_ONE`] with [`regia_detsim::math::sqrt`], so every peer gets the same shape.
    pub fn oriented_box(x: f32, z: f32, half_x: f32, half_z: f32, dir_x: f32, dir_z: f32) -> StampShape {
        let len = math::sqrt(dir_x * dir_x + dir_z * dir_z);
        let q = Q14_ONE as f32;
        let cos_sin_q = if len.is_finite() && len > 0.0 {
            [(dir_x / len * q).round() as i16, (dir_z / len * q).round() as i16]
        } else {
            [Q14_ONE as i16, 0]
        };
        StampShape::OrientedBox {
            x_cm: (x * 100.0).round() as i32,
            z_cm: (z * 100.0).round() as i32,
            half_x_cm: (half_x * 100.0).round() as i32,
            half_z_cm: (half_z * 100.0).round() as i32,
            cos_sin_q,
        }
    }

    /// A convex polygon from world points (m), each rounded to the nearest centimeter.
    pub fn polygon_from_world(points: &[[f32; 2]]) -> StampShape {
        StampShape::ConvexPolygon(
            points
                .iter()
                .map(|p| [(p[0] * 100.0).round() as i32, (p[1] * 100.0).round() as i32])
                .collect(),
        )
    }

    /// Write the polygon the integer tests use into `out` (cleared first); false (and `out` empty)
    /// for a disc. Corners of a square or box follow the rule on [`StampShape::OrientedBox`]; every
    /// coordinate is clamped to [`SHAPE_LIMIT_CM`].
    pub fn polygon_cm(&self, out: &mut Vec<[i64; 2]>) -> bool {
        out.clear();
        let lim = |v: i64| v.clamp(-SHAPE_LIMIT_CM, SHAPE_LIMIT_CM);
        match self {
            StampShape::Disc(_) => return false,
            StampShape::Square { x_cm, z_cm, half_cm } => {
                let (x, z, h) = (i64::from(*x_cm), i64::from(*z_cm), i64::from(*half_cm).abs());
                for (sx, sz) in [(-1, -1), (1, -1), (1, 1), (-1, 1)] {
                    out.push([lim(x + sx * h), lim(z + sz * h)]);
                }
            }
            StampShape::OrientedBox {
                x_cm,
                z_cm,
                half_x_cm,
                half_z_cm,
                cos_sin_q,
            } => {
                let (x, z) = (i64::from(*x_cm), i64::from(*z_cm));
                let (hx, hz) = (i64::from(*half_x_cm).abs(), i64::from(*half_z_cm).abs());
                let (c, s) = (i64::from(cos_sin_q[0]), i64::from(cos_sin_q[1]));
                for (sx, sz) in [(-1, -1), (1, -1), (1, 1), (-1, 1)] {
                    let (lx, lz) = (sx * hx, sz * hz);
                    let wx = div_round_q14(lx * c - lz * s);
                    let wz = div_round_q14(lx * s + lz * c);
                    out.push([lim(x + wx), lim(z + wz)]);
                }
            }
            StampShape::ConvexPolygon(points) => {
                out.extend(
                    points
                        .iter()
                        .map(|p| [lim(i64::from(p[0])), lim(i64::from(p[1]))]),
                );
            }
        }
        true
    }

    fn hash_into(&self, h: &mut Fnv) {
        match self {
            StampShape::Disc(s) => {
                h.u32(0);
                h.u32(s.x_cm as u32);
                h.u32(s.z_cm as u32);
                h.u32(s.r_cm as u32);
            }
            StampShape::Square { x_cm, z_cm, half_cm } => {
                h.u32(1);
                h.u32(*x_cm as u32);
                h.u32(*z_cm as u32);
                h.u32(*half_cm as u32);
            }
            StampShape::OrientedBox {
                x_cm,
                z_cm,
                half_x_cm,
                half_z_cm,
                cos_sin_q,
            } => {
                h.u32(2);
                h.u32(*x_cm as u32);
                h.u32(*z_cm as u32);
                h.u32(*half_x_cm as u32);
                h.u32(*half_z_cm as u32);
                h.u32(u32::from(cos_sin_q[0] as u16) | (u32::from(cos_sin_q[1] as u16) << 16));
            }
            StampShape::ConvexPolygon(points) => {
                h.u32(3);
                h.u64(points.len() as u64);
                for p in points {
                    h.u32(p[0] as u32);
                    h.u32(p[1] as u32);
                }
            }
        }
    }

    /// Become a copy of `other`, reusing this polygon's buffer when both are polygons (no
    /// allocation in a steady shape set).
    fn assign_from(&mut self, other: &StampShape) {
        match (&mut *self, other) {
            (StampShape::ConvexPolygon(a), StampShape::ConvexPolygon(b)) => {
                a.clear();
                a.extend_from_slice(b);
            }
            _ => *self = other.clone(),
        }
    }
}

/// `n / Q14_ONE` rounded to the nearest integer, half away from zero.
#[inline]
fn div_round_q14(n: i64) -> i64 {
    let q = i64::from(Q14_ONE);
    if n >= 0 {
        (n + q / 2) / q
    } else {
        -((-n + q / 2) / q)
    }
}

/// Twice the signed area of a polygon (positive counter-clockwise in X-right, Z-up terms).
fn twice_area(poly: &[[i64; 2]]) -> i128 {
    edges_of(poly)
        .map(|(a, b)| i128::from(a[0]) * i128::from(b[1]) - i128::from(b[0]) * i128::from(a[1]))
        .sum()
}

/// The polygon's edges `(a, b)` in order, the last one closing back to the first vertex.
#[inline]
fn edges_of(poly: &[[i64; 2]]) -> impl Iterator<Item = ([i64; 2], [i64; 2])> + '_ {
    poly.iter()
        .zip(poly.iter().cycle().skip(1))
        .map(|(a, b)| (*a, *b))
}

/// Cross product of `b - a` with `p - a`.
#[inline]
fn cross_at(a: [i64; 2], b: [i64; 2], p: [i64; 2]) -> i128 {
    let (ex, ez) = (i128::from(b[0] - a[0]), i128::from(b[1] - a[1]));
    let (px, pz) = (i128::from(p[0] - a[0]), i128::from(p[1] - a[1]));
    ex * pz - ez * px
}

/// True when `p` is closer than `sqrt(r2)` to the segment `a..b` (all exact integers).
#[inline]
fn segment_closer_than(a: [i64; 2], b: [i64; 2], p: [i64; 2], r2: i128) -> bool {
    let (ex, ez) = (i128::from(b[0] - a[0]), i128::from(b[1] - a[1]));
    let (px, pz) = (i128::from(p[0] - a[0]), i128::from(p[1] - a[1]));
    let len2 = ex * ex + ez * ez;
    let t = px * ex + pz * ez;
    if len2 == 0 || t <= 0 {
        return px * px + pz * pz < r2;
    }
    if t >= len2 {
        let (qx, qz) = (i128::from(p[0] - b[0]), i128::from(p[1] - b[1]));
        return qx * qx + qz * qz < r2;
    }
    let c = ex * pz - ez * px;
    c * c < r2 * len2
}

/// The shape rule of [`StampShape`]: `p` strictly inside the polygon (`area2` is
/// [`twice_area`], 0 for no inside), or strictly closer than `sqrt(r2)` to its boundary.
fn polygon_covers(poly: &[[i64; 2]], area2: i128, p: [i64; 2], r2: i128) -> bool {
    let n = poly.len();
    if n == 0 {
        return false;
    }
    if area2 != 0 {
        let sign = area2.signum();
        let inside = edges_of(poly).all(|(a, b)| cross_at(a, b, p) * sign > 0);
        if inside {
            return true;
        }
    }
    if r2 == 0 {
        return false;
    }
    if n == 1 {
        return segment_closer_than(poly[0], poly[0], p, r2);
    }
    edges_of(poly).any(|(a, b)| segment_closer_than(a, b, p, r2))
}

/// The paint rule of [`PaintRegion::Polygon`]: `p` inside or on the boundary of the polygon.
fn polygon_holds(poly: &[[i64; 2]], area2: i128, p: [i64; 2]) -> bool {
    let sign = area2.signum();
    sign != 0 && edges_of(poly).all(|(a, b)| cross_at(a, b, p) * sign >= 0)
}

/// Where [`NavGrid::paint_area`] paints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaintRegion {
    /// The cells `min..=max` (cell coordinates `[x, z]`, clamped onto the grid).
    CellRect {
        /// Lowest cell `[x, z]`.
        min: [u32; 2],
        /// Highest cell `[x, z]` (inclusive).
        max: [u32; 2],
    },
    /// The cells whose center (cm, see [`NavGrid::cell_center_cm`]) lies inside or on a convex
    /// polygon (vertices in cm, either winding, at least 3, some area).
    Polygon(Vec<[i32; 2]>),
}

/// Extra cost per cell on top of terrain and area costs, for the profiles in `profiles`: a threat
/// map, a danger zone, congestion. A game rewrites it per tick or per cadence and hands it to
/// [`NavGrid::set_overlay`]. Additive in cost units (flat ground costs [`COST_FLAT`] = 10); it
/// never opens or closes a cell.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CostOverlay {
    /// The profiles that pay the overlay.
    pub profiles: ProfileMask,
    /// Extra cost per cell, one entry per grid cell (row-major, like the grid).
    pub cells: Vec<u16>,
}

/// A no-go disc: cells whose center lies within `radius` of the center and whose ground is below
/// `below_y` are closed to every profile (the inside of a pit or shaft).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExclusionDisc {
    /// Center X (m).
    pub center_x: f32,
    /// Center Z (m).
    pub center_z: f32,
    /// Radius (m).
    pub radius: f32,
    /// Ground below this height (m) inside the radius is closed.
    pub below_y: f32,
}

/// What [`NavGrid::sync_stamps`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StampSync {
    /// False when the stamp set was already current (nothing was touched).
    pub changed: bool,
    /// Clusters that were relabeled, ascending (`cz * clusters_x + cx`). Routes through them need
    /// a re-plan.
    pub dirty_clusters: Vec<u32>,
}

/// Terrain plus live obstacles. Cloned with the sim state.
#[derive(Debug, Clone)]
pub struct NavGrid {
    /// The terrain half (shared between clones).
    pub stat: Arc<NavStatic>,
    /// Per size class: how many inflated discs cover each cell.
    pub blockers: Vec<Vec<u8>>,
    /// Per profile: local region label per cell, [`BLOCKED`] when impassable. One-byte grids
    /// ([`LabelWidth::U8`]) only; on a two-byte grid every entry is empty and the labels are in
    /// [`Self::wide_labels`].
    pub labels: Vec<Vec<u8>>,
    /// Per profile: two-byte region labels ([`LabelWidth::U16`], [`NO_REGION`] when impassable).
    /// Empty on a one-byte grid.
    pub wide_labels: Vec<Vec<u16>>,
    /// Per profile, per cluster.
    pub regions: Vec<Vec<RegionSet>>,
    /// Clusters per row of clusters (X); a cluster id is `cz * clusters_x + cx`.
    pub clusters_x: usize,
    /// Rows of clusters (Z).
    pub clusters_z: usize,
    /// Cells the host closed (for example a surface cut over a tunnel opening); empty for none.
    pub closed: Arc<Vec<u8>>,
    /// Cells inside an [`ExclusionDisc`]: hard no-go for every profile. 1 = excluded.
    pub excluded: Vec<u8>,
    exclusions: Vec<ExclusionDisc>,
    stamps: Vec<(u32, Stamp)>,
    /// Cells per cluster side and its log2.
    cluster: usize,
    shift: u32,
    /// Bumped by every relabel and stamp; not part of [`Self::state_hash`].
    generation: u64,
    /// Per cluster: [`Self::generation`] at its last relabel.
    cluster_gen: Vec<u64>,
    /// The shaped obstacle set ([`Self::sync_shapes_from`]), sorted by id; its own id space.
    shapes: Vec<(u32, StampShape)>,
    /// Area id per cell ([`Self::paint_area`]); empty while every cell is the default area.
    area: Arc<Vec<u8>>,
    /// Per profile index, per area id: the cost scale in percent, 0 when the area blocks the profile.
    area_pct: Arc<Vec<u16>>,
    /// The dynamic cost overlay ([`Self::set_overlay`]).
    overlay: Option<Arc<CostOverlay>>,
    /// Scratch of the stamp syncs: per cluster, touched this sync.
    sync_dirty: Vec<bool>,
    /// Clusters the last changing sync relabeled, ascending.
    dirty_clusters: Vec<u32>,
    /// Scratch polygon of the shape rasterizer.
    poly_scratch: Vec<[i64; 2]>,
    /// Scratch stack of the cluster flood (`cluster_cells^2` entries once warm).
    flood_stack: Vec<(u8, u8)>,
}

/// One profile's cost inputs, borrowed field by field so a relabel can write labels meanwhile.
struct CostView<'a> {
    terrain: &'a [u8],
    /// Area id per cell, empty for all default.
    area: &'a [u8],
    /// This profile's row of the area table (percent, 0 = blocked).
    pct: &'a [u16],
    /// The overlay's cells when it applies to this profile.
    overlay: Option<&'a [u16]>,
}

impl CostView<'_> {
    /// The cell's cost before obstacles, closed and excluded cells: 0 when the terrain or the area
    /// blocks it, else terrain scaled by the area's percent (half up, at least 1) plus the overlay.
    #[inline]
    fn cost(&self, i: usize) -> u32 {
        let t = u32::from(self.terrain[i]);
        if t == 0 {
            return 0;
        }
        let mut c = t;
        if !self.area.is_empty() {
            let pct = u32::from(self.pct[usize::from(self.area[i])]);
            if pct == 0 {
                return 0;
            }
            if pct != 100 {
                c = ((t * pct + 50) / 100).max(1);
            }
        }
        if let Some(extra) = self.overlay {
            c += u32::from(extra[i]);
        }
        c
    }
}

/// A label type the flood can write: one or two bytes.
pub(crate) trait RegionLabel: Copy + Eq {
    /// A cell the profile cannot stand in.
    const BLOCKED: Self;
    /// An open cell the flood has not reached yet.
    const OPEN: Self;
    /// A region id as a label (ids are below the cluster's cell count).
    fn from_id(id: usize) -> Self;
}

impl RegionLabel for u8 {
    const BLOCKED: u8 = BLOCKED;
    const OPEN: u8 = 0xFE;
    #[inline]
    fn from_id(id: usize) -> u8 {
        id as u8
    }
}

impl RegionLabel for u16 {
    const BLOCKED: u16 = NO_REGION;
    const OPEN: u16 = 0xFFFE;
    #[inline]
    fn from_id(id: usize) -> u16 {
        id as u16
    }
}

/// One profile's labels, either width. `raw` is the stored value; [`Self::blocked`] is the stored
/// value of a blocked cell (255 or 65535).
#[derive(Debug, Clone, Copy)]
pub(crate) enum LabelsRef<'a> {
    Narrow(&'a [u8]),
    Wide(&'a [u16]),
}

impl LabelsRef<'_> {
    #[inline]
    pub(crate) fn raw(&self, i: usize) -> u32 {
        match self {
            LabelsRef::Narrow(l) => u32::from(l[i]),
            LabelsRef::Wide(l) => u32::from(l[i]),
        }
    }

    #[inline]
    pub(crate) fn blocked(&self) -> u32 {
        match self {
            LabelsRef::Narrow(_) => u32::from(BLOCKED),
            LabelsRef::Wide(_) => u32::from(NO_REGION),
        }
    }
}

impl NavGrid {
    /// The grid with no obstacles and no closed cells.
    pub fn new(stat: Arc<NavStatic>) -> Result<NavGrid, GridError> {
        Self::with_closed_cells(stat, Vec::new())
    }

    /// The grid plus a host's closed-cell mask (1 = closed; empty for none).
    pub fn with_closed_cells(stat: Arc<NavStatic>, closed: Vec<u8>) -> Result<NavGrid, GridError> {
        let n = stat.width * stat.height;
        if !closed.is_empty() && closed.len() != n {
            return Err(GridError::ClosedMaskLength {
                got: closed.len(),
                cells: n,
            });
        }
        // `NavStatic` validated the settings; a hand-built one still gets a sane cluster.
        stat.settings.validate()?;
        let cluster = stat.settings.cluster_cells;
        let (clusters_x, clusters_z) = (stat.width.div_ceil(cluster), stat.height.div_ceil(cluster));
        let profiles = stat.settings.profiles.len();
        let sizes = stat.settings.size_classes.len();
        let (labels, wide_labels) = match stat.settings.label_width() {
            LabelWidth::U8 => (vec![vec![BLOCKED; n]; profiles], Vec::new()),
            LabelWidth::U16 => (vec![Vec::new(); profiles], vec![vec![NO_REGION; n]; profiles]),
        };
        let area_stride = stat.settings.areas.len() + 1;
        let mut area_pct = vec![100u16; profiles * area_stride];
        for (pi, row) in area_pct.chunks_mut(area_stride).enumerate() {
            for (k, def) in stat.settings.areas.iter().enumerate() {
                let blocked = def.blocked_for.contains(Profile(pi as u8));
                row[k + 1] = if blocked { 0 } else { def.cost_pct };
            }
        }
        let mut grid = NavGrid {
            shapes: Vec::new(),
            area: Arc::new(Vec::new()),
            area_pct: Arc::new(area_pct),
            overlay: None,
            sync_dirty: vec![false; clusters_x * clusters_z],
            dirty_clusters: Vec::new(),
            poly_scratch: Vec::new(),
            flood_stack: vec![(0, 0); cluster * cluster],
            blockers: vec![vec![0; n]; sizes],
            labels,
            wide_labels,
            regions: vec![vec![RegionSet::default(); clusters_x * clusters_z]; profiles],
            clusters_x,
            clusters_z,
            stat,
            closed: Arc::new(closed),
            excluded: vec![0; n],
            exclusions: Vec::new(),
            stamps: Vec::new(),
            cluster,
            shift: cluster.trailing_zeros(),
            generation: 0,
            cluster_gen: vec![0; clusters_x * clusters_z],
        };
        for c in 0..clusters_x * clusters_z {
            grid.relabel_cluster(c);
        }
        Ok(grid)
    }

    /// The settings the grid was built with.
    #[inline]
    pub fn settings(&self) -> &GridSettings {
        &self.stat.settings
    }

    /// Cells per cluster side ([`GridSettings::cluster_cells`]).
    #[inline]
    pub fn cluster_cells(&self) -> usize {
        self.cluster
    }

    /// Most regions one cluster can hold (`cluster_cells^2`); a coarse node id is
    /// `cluster * max_regions + label`.
    #[inline]
    pub fn max_regions(&self) -> usize {
        self.cluster * self.cluster
    }

    /// The label width this grid stores.
    #[inline]
    pub fn label_width(&self) -> LabelWidth {
        if self.wide_labels.is_empty() {
            LabelWidth::U8
        } else {
            LabelWidth::U16
        }
    }

    /// A counter bumped by every cluster relabel and every [`Self::stamp`]. Two grids with the same
    /// history have the same generation; it is not part of [`Self::state_hash`]. A cached route or
    /// region graph built at one generation is current while the generation is unchanged.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// [`Self::generation`] at the cluster's last relabel (0 before the first). A region graph
    /// compares these to find the clusters it must repair.
    #[inline]
    pub fn cluster_generation(&self, cluster: usize) -> u64 {
        self.cluster_gen.get(cluster).copied().unwrap_or(0)
    }

    /// One profile's labels by profile index, either width.
    #[inline]
    pub(crate) fn labels_ref(&self, pi: usize) -> LabelsRef<'_> {
        if self.wide_labels.is_empty() {
            LabelsRef::Narrow(&self.labels[pi])
        } else {
            LabelsRef::Wide(&self.wide_labels[pi])
        }
    }

    /// Set the no-go discs (an empty slice: none). Cells whose center falls inside a disc's radius
    /// while their ground sits below its `below_y` become hard no-go for every profile, and every
    /// cluster is relabeled. A no-op when the discs are unchanged. Deterministic: fixed cell order.
    pub fn set_exclusions(&mut self, discs: &[ExclusionDisc]) {
        if self.exclusions == discs {
            return;
        }
        self.exclusions = discs.to_vec();
        let (width, height) = (self.stat.width, self.stat.height);
        self.excluded = vec![0; width * height];
        for v in discs {
            let floor = v.below_y;
            let r2 = v.radius * v.radius;
            for z in 0..height {
                for x in 0..width {
                    let i = z * width + x;
                    if self.stat.height_cells[i] >= floor {
                        continue;
                    }
                    let wx = self.stat.origin_x + x as f32 * self.stat.cell_m - v.center_x;
                    let wz = self.stat.origin_z + z as f32 * self.stat.cell_m - v.center_z;
                    if wx * wx + wz * wz <= r2 {
                        self.excluded[i] = 1;
                    }
                }
            }
        }
        let clusters = self.clusters_x * self.clusters_z;
        for c in 0..clusters {
            self.relabel_cluster(c);
        }
    }

    /// The current no-go discs.
    pub fn exclusions(&self) -> &[ExclusionDisc] {
        &self.exclusions
    }

    /// The current stamp set, sorted by id.
    pub fn stamps(&self) -> &[(u32, Stamp)] {
        &self.stamps
    }

    /// Cells per row (X); the row stride of every cell index.
    #[inline]
    pub fn width(&self) -> usize {
        self.stat.width
    }

    /// Rows (Z).
    #[inline]
    pub fn height(&self) -> usize {
        self.stat.height
    }

    /// Cell size (m).
    #[inline]
    pub fn cell_m(&self) -> f32 {
        self.stat.cell_m
    }

    /// Nearest cell coordinates, not clamped (may fall off the grid).
    pub fn cell_coords(&self, x: f32, z: f32) -> (i32, i32) {
        let fx = (x - self.stat.origin_x) / self.stat.cell_m;
        let fz = (z - self.stat.origin_z) / self.stat.cell_m;
        ((fx + 0.5).floor() as i32, (fz + 0.5).floor() as i32)
    }

    /// Clamp cell coordinates onto the grid.
    pub fn clamp_coords(&self, cx: i32, cz: i32) -> (usize, usize) {
        let max_x = self.stat.width as i32 - 1;
        let max_z = self.stat.height as i32 - 1;
        (cx.clamp(0, max_x) as usize, cz.clamp(0, max_z) as usize)
    }

    /// Cell index under a world point, clamped onto the grid.
    pub fn cell_at(&self, x: f32, z: f32) -> u32 {
        let (cx, cz) = self.cell_coords(x, z);
        let (cx, cz) = self.clamp_coords(cx, cz);
        (cz * self.stat.width + cx) as u32
    }

    /// Cell coordinates `(x, z)` of a cell index.
    #[inline]
    pub fn xz(&self, cell: u32) -> (usize, usize) {
        let stride = self.stat.width;
        (cell as usize % stride, cell as usize / stride)
    }

    /// Cell index of cell coordinates.
    #[inline]
    pub fn index(&self, x: usize, z: usize) -> u32 {
        (z * self.stat.width + x) as u32
    }

    /// World XZ of a cell center.
    pub fn center(&self, cell: u32) -> [f32; 2] {
        let (x, z) = self.xz(cell);
        [
            self.stat.origin_x + x as f32 * self.stat.cell_m,
            self.stat.origin_z + z as f32 * self.stat.cell_m,
        ]
    }

    /// True when the world point's nearest cell is on the grid.
    pub fn in_bounds_world(&self, x: f32, z: f32) -> bool {
        let (cx, cz) = self.cell_coords(x, z);
        cx >= 0 && cz >= 0 && (cx as usize) < self.stat.width && (cz as usize) < self.stat.height
    }

    #[inline]
    fn classes_of(&self, profile: Profile) -> (usize, usize) {
        let def = self.stat.settings.profile(profile);
        (usize::from(def.move_class), usize::from(def.size_class))
    }

    #[inline]
    fn open_mask(&self, i: usize) -> bool {
        (self.closed.is_empty() || self.closed[i] == 0) && self.excluded[i] == 0
    }

    /// True unless the cell's area ([`Self::paint_area`]) blocks the profile (by index).
    #[inline]
    fn area_open(&self, pi: usize, i: usize) -> bool {
        self.area.is_empty() || {
            let stride = self.stat.settings.areas.len() + 1;
            self.area_pct[pi * stride + usize::from(self.area[i])] != 0
        }
    }

    /// One profile's cost inputs (profile index `pi`, movement class `m`).
    #[inline]
    fn cost_view(&self, pi: usize, m: usize) -> CostView<'_> {
        let stride = self.stat.settings.areas.len() + 1;
        CostView {
            terrain: &self.stat.cost[m],
            area: &self.area,
            pct: &self.area_pct[pi * stride..(pi + 1) * stride],
            overlay: overlay_for(self.overlay.as_deref(), pi),
        }
    }

    /// The profile can stand in the cell: not a cliff, not under an inflated stamp, not closed,
    /// not excluded, not in an area that blocks the profile.
    #[inline]
    pub fn passable(&self, profile: Profile, cell: u32) -> bool {
        let i = cell as usize;
        let pi = self.stat.settings.profile_index(profile);
        let (m, s) = self.classes_of(profile);
        self.stat.cost[m][i] != 0 && self.blockers[s][i] == 0 && self.open_mask(i) && self.area_open(pi, i)
    }

    /// True when the cell is inside a no-go disc, whatever the profile. Routes snap goals out of
    /// it; steering never aims into it.
    #[inline]
    pub fn excluded(&self, cell: u32) -> bool {
        self.excluded[cell as usize] != 0
    }

    /// Ground height under a world point (nearest cell).
    pub fn surface_y(&self, x: f32, z: f32) -> f32 {
        let cell = self.cell_at(x, z) as usize;
        self.stat.height_cells.get(cell).copied().unwrap_or(0.0)
    }

    /// Cost multiplier of standing in the cell (tenths: flat open ground is [`COST_FLAT`]); 0 when
    /// the cell is blocked for the profile.
    ///
    /// The terrain cost scaled by the cell's area, `max(1, (terrain * cost_pct + 50) / 100)` (area 0
    /// leaves it unchanged), plus the [`CostOverlay`] entry when the overlay applies to the profile.
    /// Integer math only. Searches read every cell cost through here.
    #[inline]
    pub fn cost(&self, profile: Profile, cell: u32) -> u32 {
        let i = cell as usize;
        let pi = self.stat.settings.profile_index(profile);
        let (m, s) = self.classes_of(profile);
        if self.blockers[s][i] != 0 || !self.open_mask(i) {
            return 0;
        }
        self.cost_view(pi, m).cost(i)
    }

    /// Terrain alone: a cliff for this profile, obstacles ignored.
    #[inline]
    pub fn terrain_blocked(&self, profile: Profile, cell: u32) -> bool {
        let (m, _) = self.classes_of(profile);
        self.stat.cost[m][cell as usize] == 0
    }

    /// The terrain lets this profile step from `cell` by `(dx, dz)` (one cell, any of 8 ways).
    #[inline]
    pub fn can_step(&self, profile: Profile, cell: u32, dx: i32, dz: i32) -> bool {
        if dx == 0 && dz == 0 {
            return true;
        }
        let (m, _) = self.classes_of(profile);
        match dir_index(dx, dz) {
            Some(k) => self.stat.edges[m][cell as usize] & (1 << k) != 0,
            None => false,
        }
    }

    /// Cluster id of a cell.
    #[inline]
    pub fn cluster_of(&self, cell: u32) -> usize {
        let (x, z) = self.xz(cell);
        (z >> self.shift) * self.clusters_x + (x >> self.shift)
    }

    /// Local region label of a cell for a profile ([`BLOCKED`] when impassable). On a two-byte grid
    /// ([`LabelWidth::U16`]) a label above 254 reads as 254: use [`Self::region_label`] there.
    #[inline]
    pub fn label(&self, profile: Profile, cell: u32) -> u8 {
        let pi = self.stat.settings.profile_index(profile);
        if self.wide_labels.is_empty() {
            return self.labels[pi][cell as usize];
        }
        match self.wide_labels[pi][cell as usize] {
            NO_REGION => BLOCKED,
            v => v.min(254) as u8,
        }
    }

    /// Local region label of a cell for a profile at any label width ([`NO_REGION`] when
    /// impassable).
    #[inline]
    pub fn region_label(&self, profile: Profile, cell: u32) -> u16 {
        let pi = self.stat.settings.profile_index(profile);
        if self.wide_labels.is_empty() {
            match self.labels[pi][cell as usize] {
                BLOCKED => NO_REGION,
                v => u16::from(v),
            }
        } else {
            self.wide_labels[pi][cell as usize]
        }
    }

    /// Add (`add`) or remove one disc on every size layer. Returns the touched cluster range
    /// `(x0, z0, x1, z1)` (inclusive), or `None` off-grid. Does not relabel: call
    /// [`Self::relabel_cluster`] on the range, or use [`Self::sync_stamps`], which does both.
    pub fn stamp(&mut self, stamp: Stamp, add: bool) -> Option<(usize, usize, usize, usize)> {
        let x = stamp.x_cm as f32 / 100.0;
        let z = stamp.z_cm as f32 / 100.0;
        let r = stamp.r_cm as f32 / 100.0;
        let (max_x, max_z) = (self.stat.width as i32 - 1, self.stat.height as i32 - 1);
        let cell_m = self.stat.cell_m;
        let mut touched: Option<(usize, usize, usize, usize)> = None;
        for (size, class) in self.stat.settings.size_classes.iter().enumerate() {
            let reach = r + class.inflate_m;
            let reach2 = reach * reach;
            let (c0x, c0z) = self.cell_coords(x - reach, z - reach);
            let (c1x, c1z) = self.cell_coords(x + reach, z + reach);
            let (lx, lz) = (c0x.max(0), c0z.max(0));
            let (hx, hz) = (c1x.min(max_x), c1z.min(max_z));
            if lx > hx || lz > hz {
                continue;
            }
            let layer = &mut self.blockers[size];
            for cz in lz..=hz {
                for cx in lx..=hx {
                    let wx = self.stat.origin_x + cx as f32 * cell_m - x;
                    let wz = self.stat.origin_z + cz as f32 * cell_m - z;
                    if wx * wx + wz * wz >= reach2 {
                        continue;
                    }
                    let i = cz as usize * self.stat.width + cx as usize;
                    if add {
                        layer[i] = layer[i].saturating_add(1);
                    } else {
                        layer[i] = layer[i].saturating_sub(1);
                    }
                }
            }
            let k = self.cluster;
            let span = (lx as usize / k, lz as usize / k, hx as usize / k, hz as usize / k);
            touched = Some(match touched {
                None => span,
                Some((a, b, c, d)) => (a.min(span.0), b.min(span.1), c.max(span.2), d.max(span.3)),
            });
        }
        self.generation += 1;
        touched
    }

    /// Make `now` the grid's obstacle set: lift stamps that are gone or moved, stamp new and moved
    /// ones (each disc on every size layer, inflated per size class), and relabel the touched
    /// clusters in ascending order.
    ///
    /// `now` is `(id, disc)` per obstacle, ids unique, in any order (it is sorted here). The host
    /// decides what blocks: buildings, wrecks, parked vehicles, debris piles all become discs
    /// ([`Stamp::from_world`], [`Stamp::covering_square`]) or shapes ([`Self::sync_shapes_from`]).
    /// An unchanged set touches nothing. Allocates the returned list; a per-tick caller uses
    /// [`Self::sync_stamps_from`], which allocates nothing once warm.
    pub fn sync_stamps(&mut self, mut now: Vec<(u32, Stamp)>) -> StampSync {
        if !self.sync_stamps_from(&mut now) {
            return StampSync::default();
        }
        StampSync {
            changed: true,
            dirty_clusters: self.dirty_clusters.clone(),
        }
    }

    /// [`Self::sync_stamps`] without allocating once warm: `now` is sorted in place (and keeps the
    /// new set), the grid copies it into its own buffer, and the relabeled clusters are in
    /// [`Self::dirty_clusters`]. Returns true when the set changed. The same bits as
    /// [`Self::sync_stamps`].
    pub fn sync_stamps_from(&mut self, now: &mut [(u32, Stamp)]) -> bool {
        let _span = tracing::trace_span!("det_grid_sync_stamps").entered();
        self.dirty_clusters.clear();
        now.sort_unstable();
        if *now == *self.stamps {
            return false;
        }
        let mut dirty = std::mem::take(&mut self.sync_dirty);
        dirty.clear();
        dirty.resize(self.clusters_x * self.clusters_z, false);
        // Merge the two id-sorted lists.
        let (mut i, mut j) = (0usize, 0usize);
        let mut old = std::mem::take(&mut self.stamps);
        while i < old.len() || j < now.len() {
            let take_old = j >= now.len() || (i < old.len() && old[i].0 < now[j].0);
            let take_new = i >= old.len() || (j < now.len() && now[j].0 < old[i].0);
            if take_old {
                let span = self.stamp(old[i].1, false);
                self.mark_dirty(&mut dirty, span);
                i += 1;
            } else if take_new {
                let span = self.stamp(now[j].1, true);
                self.mark_dirty(&mut dirty, span);
                j += 1;
            } else {
                if old[i].1 != now[j].1 {
                    let span = self.stamp(old[i].1, false);
                    self.mark_dirty(&mut dirty, span);
                    let span = self.stamp(now[j].1, true);
                    self.mark_dirty(&mut dirty, span);
                }
                i += 1;
                j += 1;
            }
        }
        old.clear();
        old.extend_from_slice(now);
        self.stamps = old;
        self.relabel_dirty(&dirty);
        self.sync_dirty = dirty;
        true
    }

    /// Make `now` the grid's shaped obstacle set, like [`Self::sync_stamps_from`]: ids unique in
    /// their own id space (apart from the discs'), any order (sorted here, in place). Gone and
    /// changed shapes are lifted, new and changed ones stamped on every size layer by the rule on
    /// [`StampShape`], and the touched clusters relabeled ([`Self::dirty_clusters`]). Returns true
    /// when the set changed. Allocates nothing once warm while polygons keep their slots.
    pub fn sync_shapes_from(&mut self, now: &mut [(u32, StampShape)]) -> bool {
        let _span = tracing::trace_span!("det_grid_sync_shapes").entered();
        self.dirty_clusters.clear();
        now.sort_unstable();
        if *now == *self.shapes {
            return false;
        }
        let mut dirty = std::mem::take(&mut self.sync_dirty);
        dirty.clear();
        dirty.resize(self.clusters_x * self.clusters_z, false);
        let (mut i, mut j) = (0usize, 0usize);
        let mut old = std::mem::take(&mut self.shapes);
        while i < old.len() || j < now.len() {
            let take_old = j >= now.len() || (i < old.len() && old[i].0 < now[j].0);
            let take_new = i >= old.len() || (j < now.len() && now[j].0 < old[i].0);
            if take_old {
                let span = self.stamp_shape(&old[i].1, false);
                self.mark_dirty(&mut dirty, span);
                i += 1;
            } else if take_new {
                let span = self.stamp_shape(&now[j].1, true);
                self.mark_dirty(&mut dirty, span);
                j += 1;
            } else {
                if old[i].1 != now[j].1 {
                    let span = self.stamp_shape(&old[i].1, false);
                    self.mark_dirty(&mut dirty, span);
                    let span = self.stamp_shape(&now[j].1, true);
                    self.mark_dirty(&mut dirty, span);
                }
                i += 1;
                j += 1;
            }
        }
        old.truncate(now.len());
        for (dst, src) in old.iter_mut().zip(now.iter()) {
            dst.0 = src.0;
            dst.1.assign_from(&src.1);
        }
        let kept = old.len();
        old.extend(now[kept..].iter().cloned());
        self.shapes = old;
        self.relabel_dirty(&dirty);
        self.sync_dirty = dirty;
        true
    }

    /// The current shaped obstacle set, sorted by id.
    pub fn shapes(&self) -> &[(u32, StampShape)] {
        &self.shapes
    }

    /// Clusters the last changing [`Self::sync_stamps_from`] or [`Self::sync_shapes_from`]
    /// relabeled, ascending (empty after a sync that changed nothing). Routes through them need a
    /// re-plan.
    pub fn dirty_clusters(&self) -> &[u32] {
        &self.dirty_clusters
    }

    /// Mark a touched cluster range in `dirty`.
    fn mark_dirty(&self, dirty: &mut [bool], span: Option<(usize, usize, usize, usize)>) {
        let (cx_n, cz_n) = (self.clusters_x, self.clusters_z);
        if let Some((x0, z0, x1, z1)) = span {
            for z in z0..=z1.min(cz_n - 1) {
                for x in x0..=x1.min(cx_n - 1) {
                    dirty[z * cx_n + x] = true;
                }
            }
        }
    }

    /// Relabel the marked clusters in ascending order into [`Self::dirty_clusters`].
    fn relabel_dirty(&mut self, dirty: &[bool]) {
        for (c, d) in dirty.iter().enumerate() {
            if *d {
                self.relabel_cluster(c);
                self.dirty_clusters.push(c as u32);
            }
        }
    }

    /// Center of cell `(x, z)` in centimeters, the point the integer shape and paint tests use:
    /// `round(origin * 100) + x * round(cell_m * 100)` per axis, clamped to [`SHAPE_LIMIT_CM`].
    pub fn cell_center_cm(&self, x: usize, z: usize) -> [i64; 2] {
        let (ox, oz, step) = self.cm_frame();
        [
            (ox + x as i64 * step).clamp(-SHAPE_LIMIT_CM, SHAPE_LIMIT_CM),
            (oz + z as i64 * step).clamp(-SHAPE_LIMIT_CM, SHAPE_LIMIT_CM),
        ]
    }

    /// `(origin_x_cm, origin_z_cm, cell_cm)` of [`Self::cell_center_cm`].
    fn cm_frame(&self) -> (i64, i64, i64) {
        let cm = |v: f32| (v * 100.0).round() as i64;
        (
            cm(self.stat.origin_x),
            cm(self.stat.origin_z),
            cm(self.stat.cell_m).max(1),
        )
    }

    /// The cell range `[lo, hi]` (inclusive, clamped onto an axis of `cells` cells) whose centers
    /// `origin + k * step` fall within `[min, max]` cm, or `None`.
    fn cell_span(origin: i64, step: i64, min: i64, max: i64, cells: usize) -> Option<(usize, usize)> {
        let lo = (min - origin + step - 1).div_euclid(step).max(0);
        let hi = (max - origin).div_euclid(step).min(cells as i64 - 1);
        (lo <= hi).then_some((lo as usize, hi as usize))
    }

    /// Add (`add`) or remove one shape on every size layer, by the rule on [`StampShape`]. A disc
    /// goes through [`Self::stamp`] unchanged. Returns the touched cluster range like
    /// [`Self::stamp`]; does not relabel.
    pub fn stamp_shape(&mut self, shape: &StampShape, add: bool) -> Option<(usize, usize, usize, usize)> {
        if let StampShape::Disc(disc) = shape {
            return self.stamp(*disc, add);
        }
        let mut poly = std::mem::take(&mut self.poly_scratch);
        shape.polygon_cm(&mut poly);
        let touched = self.stamp_polygon(&poly, add);
        self.poly_scratch = poly;
        self.generation += 1;
        touched
    }

    fn stamp_polygon(&mut self, poly: &[[i64; 2]], add: bool) -> Option<(usize, usize, usize, usize)> {
        let first = *poly.first()?;
        let (mut min, mut max) = (first, first);
        for p in poly {
            min = [min[0].min(p[0]), min[1].min(p[1])];
            max = [max[0].max(p[0]), max[1].max(p[1])];
        }
        let area2 = twice_area(poly);
        let (ox, oz, step) = self.cm_frame();
        let (w, h) = (self.stat.width, self.stat.height);
        let mut touched: Option<(usize, usize, usize, usize)> = None;
        let stat = Arc::clone(&self.stat);
        for (size, class) in stat.settings.size_classes.iter().enumerate() {
            let r_cm = (class.inflate_m * 100.0).round() as i64;
            let r = r_cm.clamp(0, SHAPE_LIMIT_CM);
            let r2 = i128::from(r) * i128::from(r);
            let xs = Self::cell_span(ox, step, min[0] - r, max[0] + r, w);
            let zs = Self::cell_span(oz, step, min[1] - r, max[1] + r, h);
            let (Some((lx, hx)), Some((lz, hz))) = (xs, zs) else {
                continue;
            };
            for cz in lz..=hz {
                for cx in lx..=hx {
                    let p = self.cell_center_cm(cx, cz);
                    if !polygon_covers(poly, area2, p, r2) {
                        continue;
                    }
                    let i = cz * w + cx;
                    let layer = &mut self.blockers[size];
                    if add {
                        layer[i] = layer[i].saturating_add(1);
                    } else {
                        layer[i] = layer[i].saturating_sub(1);
                    }
                }
            }
            let k = self.cluster;
            let span = (lx / k, lz / k, hx / k, hz / k);
            touched = Some(match touched {
                None => span,
                Some((a, b, c, d)) => (a.min(span.0), b.min(span.1), c.max(span.2), d.max(span.3)),
            });
        }
        touched
    }

    /// Paint `area` (an id of [`GridSettings::areas`], 0 = back to default) onto a region and
    /// relabel the touched clusters (region costs follow the area). Returns the cells whose area
    /// changed. A grid whose cells are all area 0 again has exactly the bits of a never-painted one.
    pub fn paint_area(&mut self, region: &PaintRegion, area: u8) -> Result<usize, GridError> {
        let _span = tracing::trace_span!("det_grid_paint_area").entered();
        if usize::from(area) > self.stat.settings.areas.len() {
            return Err(GridError::AreaMissing(area));
        }
        let (w, h) = (self.stat.width, self.stat.height);
        let mut poly = Vec::new();
        let (x_span, z_span) = match region {
            PaintRegion::CellRect { min, max } => {
                let x0 = (min[0] as usize).min(w - 1);
                let z0 = (min[1] as usize).min(h - 1);
                let x1 = (max[0] as usize).min(w - 1);
                let z1 = (max[1] as usize).min(h - 1);
                if x0 > x1 || z0 > z1 {
                    return Ok(0);
                }
                ((x0, x1), (z0, z1))
            }
            PaintRegion::Polygon(points) => {
                StampShape::ConvexPolygon(points.clone()).polygon_cm(&mut poly);
                if poly.len() < 3 || twice_area(&poly) == 0 {
                    return Err(GridError::BadPolygon(points.len()));
                }
                let (mut min, mut max) = (poly[0], poly[0]);
                for p in &poly {
                    min = [min[0].min(p[0]), min[1].min(p[1])];
                    max = [max[0].max(p[0]), max[1].max(p[1])];
                }
                let (ox, oz, step) = self.cm_frame();
                let xs = Self::cell_span(ox, step, min[0], max[0], w);
                let zs = Self::cell_span(oz, step, min[1], max[1], h);
                let (Some(xs), Some(zs)) = (xs, zs) else {
                    return Ok(0);
                };
                (xs, zs)
            }
        };
        let area2 = twice_area(&poly);
        let mut changed = 0usize;
        let mut layer = std::mem::take(&mut self.area);
        {
            let cells = Arc::make_mut(&mut layer);
            if cells.is_empty() {
                if area == 0 {
                    self.area = layer;
                    return Ok(0);
                }
                cells.resize(w * h, 0);
            }
            for cz in z_span.0..=z_span.1 {
                for cx in x_span.0..=x_span.1 {
                    if !poly.is_empty() && !polygon_holds(&poly, area2, self.cell_center_cm(cx, cz)) {
                        continue;
                    }
                    let i = cz * w + cx;
                    if cells[i] != area {
                        cells[i] = area;
                        changed += 1;
                    }
                }
            }
            if cells.iter().all(|a| *a == 0) {
                cells.clear();
                cells.shrink_to_fit();
            }
        }
        self.area = layer;
        if changed > 0 {
            let k = self.cluster;
            for cz in z_span.0 / k..=z_span.1 / k {
                for cx in x_span.0 / k..=x_span.1 / k {
                    self.relabel_cluster(cz * self.clusters_x + cx);
                }
            }
        }
        Ok(changed)
    }

    /// The area id of a cell (0 when unpainted).
    #[inline]
    pub fn area_of(&self, cell: u32) -> u8 {
        self.area.get(cell as usize).copied().unwrap_or(0)
    }

    /// Cells painted with an area other than 0.
    pub fn painted_cells(&self) -> usize {
        self.area.iter().filter(|a| **a != 0).count()
    }

    /// Set (or clear, `None`) the dynamic cost overlay. A new overlay relabels every cluster so the
    /// coarse region costs include it, and bumps [`Self::generation`]; the same `Arc` again is a
    /// no-op. Fails when the overlay's length is not the cell count.
    pub fn set_overlay(&mut self, overlay: Option<Arc<CostOverlay>>) -> Result<(), GridError> {
        let _span = tracing::trace_span!("det_grid_set_overlay").entered();
        let n = self.stat.width * self.stat.height;
        if let Some(o) = &overlay
            && o.cells.len() != n
        {
            return Err(GridError::OverlayLength {
                got: o.cells.len(),
                cells: n,
            });
        }
        let same = match (&self.overlay, &overlay) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if same {
            return Ok(());
        }
        self.overlay = overlay;
        for c in 0..self.clusters_x * self.clusters_z {
            self.relabel_cluster(c);
        }
        Ok(())
    }

    /// The current cost overlay.
    pub fn overlay(&self) -> Option<&Arc<CostOverlay>> {
        self.overlay.as_ref()
    }

    /// Re-flood one cluster's regions for every profile (4-connected), with region costs from
    /// [`Self::cost`]'s cell costs (terrain, area, overlay), and bump [`Self::generation`] and the
    /// cluster's [`Self::cluster_generation`]. Reuses the cluster's region buffers (no allocation
    /// once warm).
    pub fn relabel_cluster(&mut self, cluster: usize) {
        let stride = self.stat.width;
        let (w, h) = (self.stat.width, self.stat.height);
        let k = self.cluster;
        let cx0 = (cluster % self.clusters_x) * k;
        let cz0 = (cluster / self.clusters_x) * k;
        let span = Span {
            stride,
            cx0,
            cz0,
            cx1: (cx0 + k).min(w),
            cz1: (cz0 + k).min(h),
        };
        // Each cell is pushed at most once (it is labeled when pushed). The stack is the grid's
        // reused scratch, so a warm relabel allocates nothing (`sync_from_allocates_nothing_after_warmup`).
        let mut stack = std::mem::take(&mut self.flood_stack);
        stack.resize(k * k, (0, 0));
        for (p, def) in self.stat.settings.profiles.iter().enumerate() {
            let (m, s) = (usize::from(def.move_class), usize::from(def.size_class));
            let area_stride = self.stat.settings.areas.len() + 1;
            let cells = Cells {
                view: CostView {
                    terrain: &self.stat.cost[m],
                    area: &self.area,
                    pct: &self.area_pct[p * area_stride..(p + 1) * area_stride],
                    overlay: overlay_for(self.overlay.as_deref(), p),
                },
                edges: &self.stat.edges[m],
                blockers: &self.blockers[s],
                cut: &self.closed,
                pit: &self.excluded,
            };
            // Reuse the cluster's region buffers (no allocation once warm).
            let mut set = std::mem::take(&mut self.regions[p][cluster]);
            if self.wide_labels.is_empty() {
                flood(&cells, &span, &mut self.labels[p], &mut stack, &mut set);
            } else {
                flood(&cells, &span, &mut self.wide_labels[p], &mut stack, &mut set);
            }
            self.regions[p][cluster] = set;
        }
        self.flood_stack = stack;
        self.generation += 1;
        if let Some(g) = self.cluster_gen.get_mut(cluster) {
            *g = self.generation;
        }
    }

    /// Nearest cell the profile can stand in, within `max_ring` cells (Chebyshev rings, nearest by
    /// Euclidean distance, lowest index on a tie).
    pub fn nearest_passable(&self, profile: Profile, from: u32, max_ring: i32) -> Option<u32> {
        if self.passable(profile, from) {
            return Some(from);
        }
        let (fx, fz) = self.xz(from);
        let (fx, fz) = (fx as i32, fz as i32);
        let (w, h) = (self.stat.width as i32, self.stat.height as i32);
        let stride = self.stat.width as i32;
        let mut best: Option<(i32, u32)> = None;
        for ring in 1..=max_ring {
            if let Some((d, _)) = best
                && ring * ring > d
            {
                break;
            }
            for dz in -ring..=ring {
                for dx in -ring..=ring {
                    if dx.abs() != ring && dz.abs() != ring {
                        continue;
                    }
                    let (x, z) = (fx + dx, fz + dz);
                    if x < 0 || z < 0 || x >= w || z >= h {
                        continue;
                    }
                    let cell = (z * stride + x) as u32;
                    if !self.passable(profile, cell) {
                        continue;
                    }
                    let d = dx * dx + dz * dz;
                    if best.is_none_or(|(bd, bc)| d < bd || (d == bd && cell < bc)) {
                        best = Some((d, cell));
                    }
                }
            }
        }
        best.map(|(_, cell)| cell)
    }

    /// Line of sight on the grid (supercover): every cell the segment between two cell centers
    /// touches is passable and no costlier than `max_cost`. `ignore` lets cells within that many
    /// cells of `to` through (a target inside its own obstacle).
    pub fn los(&self, profile: Profile, from: u32, to: u32, max_cost: u32, ignore: i32) -> bool {
        let (x0, z0) = self.xz(from);
        let (x1, z1) = self.xz(to);
        let (x0, z0, x1, z1) = (x0 as i32, z0 as i32, x1 as i32, z1 as i32);
        let ok = |px: i32, pz: i32, x: i32, z: i32| -> bool {
            if ignore > 0 && (x - x1).abs() <= ignore && (z - z1).abs() <= ignore {
                return true;
            }
            let cell = self.index(x as usize, z as usize);
            let c = self.cost(profile, cell);
            let from = self.index(px as usize, pz as usize);
            c != 0 && c <= max_cost && self.can_step(profile, from, x - px, z - pz)
        };
        supercover(x0, z0, x1, z1, ok)
    }

    /// FNV-1a 64 over the live state (blockers, labels, regions, closed and excluded cells, the
    /// stamp set), for desync checks and goldens. The terrain half is [`NavStatic::bits_hash`].
    pub fn state_hash(&self) -> u64 {
        let mut h = Fnv::new();
        for layer in self.blockers.iter().chain(self.labels.iter()) {
            h.bytes(layer);
        }
        // Two-byte labels (empty on a one-byte grid, so its hash is the source's).
        for layer in &self.wide_labels {
            for v in layer {
                h.bytes(&v.to_le_bytes());
            }
        }
        for per_profile in &self.regions {
            for set in per_profile {
                h.u64(set.reps.len() as u64);
                for (rep, cost) in set.reps.iter().zip(&set.costs) {
                    h.u32(*rep);
                    h.u32(u32::from(*cost));
                }
            }
        }
        h.bytes(&self.closed);
        h.bytes(&self.excluded);
        for (id, s) in &self.stamps {
            h.u32(*id);
            h.u32(s.x_cm as u32);
            h.u32(s.z_cm as u32);
            h.u32(s.r_cm as u32);
        }
        // Shapes, areas and the overlay only when present, so a grid without them keeps the
        // terrain-only hash.
        if !self.shapes.is_empty() {
            h.u64(self.shapes.len() as u64);
            for (id, shape) in &self.shapes {
                h.u32(*id);
                shape.hash_into(&mut h);
            }
        }
        if !self.area.is_empty() {
            h.u32(0x4152_4541);
            h.bytes(&self.area);
        }
        if let Some(o) = &self.overlay {
            h.u32(0x4f56_524c);
            for word in o.profiles.0 {
                h.u64(word);
            }
            for c in &o.cells {
                h.bytes(&c.to_le_bytes());
            }
        }
        h.finish()
    }

    /// A serializable summary for logs, tools and agents.
    pub fn summary(&self) -> GridSummary {
        let n = self.width() * self.height();
        let profiles = self
            .settings()
            .all_profiles()
            .map(|p| {
                let pi = self.settings().profile_index(p);
                ProfileSummary {
                    profile: p.0,
                    passable_cells: (0..n as u32).filter(|&c| self.passable(p, c)).count(),
                    regions: self.regions[pi].iter().map(|s| s.reps.len()).sum(),
                }
            })
            .collect();
        GridSummary {
            width: self.width(),
            height: self.height(),
            cell_m: self.cell_m(),
            clusters: [self.clusters_x, self.clusters_z],
            flat: self.stat.key.flat,
            stamps: self.stamps.len(),
            shapes: self.shapes.len(),
            painted_cells: self.painted_cells(),
            overlay: self.overlay.is_some(),
            closed_cells: self.closed.iter().filter(|c| **c != 0).count(),
            excluded_cells: self.excluded.iter().filter(|c| **c != 0).count(),
            lowered_cells: self.stat.lowered.iter().filter(|c| **c != 0).count(),
            profiles,
            static_hash: format!("{:016x}", self.stat.bits_hash()),
            state_hash: format!("{:016x}", self.state_hash()),
        }
    }
}

/// A serializable summary of a [`NavGrid`] (see [`NavGrid::summary`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GridSummary {
    /// Cells per row (X).
    pub width: usize,
    /// Rows (Z).
    pub height: usize,
    /// Cell size (m).
    pub cell_m: f32,
    /// Clusters `[x, z]`.
    pub clusters: [usize; 2],
    /// True when built from the flat canvas (no ground).
    pub flat: bool,
    /// Stamps in the current obstacle set.
    pub stamps: usize,
    /// Shapes in the current shaped obstacle set.
    pub shapes: usize,
    /// Cells painted with an area other than the default.
    pub painted_cells: usize,
    /// True while a cost overlay is set.
    pub overlay: bool,
    /// Cells the host closed.
    pub closed_cells: usize,
    /// Cells inside an exclusion disc.
    pub excluded_cells: usize,
    /// Cells a ground patch lowered.
    pub lowered_cells: usize,
    /// Per profile: passable cells and regions.
    pub profiles: Vec<ProfileSummary>,
    /// [`NavStatic::bits_hash`] as 16 hex digits.
    pub static_hash: String,
    /// [`NavGrid::state_hash`] as 16 hex digits.
    pub state_hash: String,
}

/// One profile's line in a [`GridSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileSummary {
    /// The profile index.
    pub profile: u8,
    /// Cells the profile can stand in.
    pub passable_cells: usize,
    /// Regions over all clusters.
    pub regions: usize,
}

/// One cluster's cell rectangle `[cx0, cx1) x [cz0, cz1)` and the row stride.
struct Span {
    stride: usize,
    cx0: usize,
    cz0: usize,
    cx1: usize,
    cz1: usize,
}

/// The per-cell layers one profile's flood reads.
struct Cells<'a> {
    /// The profile's cell costs (terrain, area, overlay); 0 = impassable.
    view: CostView<'a>,
    edges: &'a [u8],
    blockers: &'a [u8],
    cut: &'a [u8],
    pit: &'a [u8],
}

/// Label one profile's regions inside one cluster (4-connected flood in row order) into `set`
/// (cleared first; its buffers are reused). The same steps at either label width: one-byte labels
/// give the source's bits.
fn flood<L: RegionLabel>(
    c: &Cells,
    span: &Span,
    labels: &mut [L],
    stack: &mut [(u8, u8)],
    set: &mut RegionSet,
) {
    let Span {
        stride,
        cx0,
        cz0,
        cx1,
        cz1,
    } = *span;
    let (view, edges, blockers, cut, pit) = (&c.view, c.edges, c.blockers, c.cut, c.pit);
    let zero = L::from_id(0);
    set.reps.clear();
    set.costs.clear();
    // Fast path: the whole cluster is open (most of every map).
    let mut open = 0usize;
    let mut cost_sum = 0u64;
    for z in cz0..cz1 {
        for x in cx0..cx1 {
            let i = z * stride + x;
            let cost = view.cost(i);
            let pass = cost != 0 && blockers[i] == 0 && (cut.is_empty() || cut[i] == 0) && pit[i] == 0;
            labels[i] = if pass { zero } else { L::BLOCKED };
            if pass {
                open += 1;
                cost_sum += u64::from(cost);
            }
        }
    }
    let cells = (cx1 - cx0) * (cz1 - cz0);
    // Every orthogonal step inside the cluster must be open too.
    let mut steps_open = true;
    if open == cells {
        'scan: for z in cz0..cz1 {
            for x in cx0..cx1 {
                let e = edges[z * stride + x];
                // E (bit 0) and S (bit 2) cover every inside pair once.
                if (x + 1 < cx1 && e & 1 == 0) || (z + 1 < cz1 && e & 4 == 0) {
                    steps_open = false;
                    break 'scan;
                }
            }
        }
    }
    if open == cells && steps_open {
        let mx = cx0 + (cx1 - cx0 - 1) / 2;
        let mz = cz0 + (cz1 - cz0 - 1) / 2;
        set.reps.push((mz * stride + mx) as u32);
        set.costs.push(region_cost(cost_sum, open as u64));
        return;
    }
    // Mark open cells unlabeled, then flood.
    for z in cz0..cz1 {
        for x in cx0..cx1 {
            let i = z * stride + x;
            if labels[i] == zero {
                labels[i] = L::OPEN;
            }
        }
    }
    for z in cz0..cz1 {
        for x in cx0..cx1 {
            let i = z * stride + x;
            if labels[i] != L::OPEN {
                continue;
            }
            let id = L::from_id(set.reps.len());
            labels[i] = id;
            let mut top = 0usize;
            stack[top] = ((x - cx0) as u8, (z - cz0) as u8);
            top += 1;
            let (mut n, mut sum_x, mut sum_z, mut csum) = (0i64, 0i64, 0i64, 0u64);
            while top > 0 {
                top -= 1;
                let (lx, lz) = stack[top];
                let (sx, sz) = (cx0 + lx as usize, cz0 + lz as usize);
                n += 1;
                sum_x += sx as i64;
                sum_z += sz as i64;
                csum += u64::from(view.cost(sz * stride + sx));
                for (dx, dz) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let nx = sx as i32 + dx;
                    let nz = sz as i32 + dz;
                    if nx < cx0 as i32 || nz < cz0 as i32 || nx >= cx1 as i32 || nz >= cz1 as i32 {
                        continue;
                    }
                    let j = nz as usize * stride + nx as usize;
                    let k = dir_index(dx, dz).unwrap_or(0);
                    if labels[j] == L::OPEN && edges[sz * stride + sx] & (1 << k) != 0 {
                        labels[j] = id;
                        stack[top] = ((nx as usize - cx0) as u8, (nz as usize - cz0) as u8);
                        top += 1;
                    }
                }
            }
            // Representative: the cell nearest the centroid, lowest index on a tie.
            let mut best: Option<(i64, u32)> = None;
            for rz in cz0..cz1 {
                for rx in cx0..cx1 {
                    let idx = rz * stride + rx;
                    if labels[idx] != id {
                        continue;
                    }
                    let dx = rx as i64 * n - sum_x;
                    let dz = rz as i64 * n - sum_z;
                    let d = dx * dx + dz * dz;
                    if best.is_none_or(|(bd, bi)| d < bd || (d == bd && (idx as u32) < bi)) {
                        best = Some((d, idx as u32));
                    }
                }
            }
            set.reps.push(best.map(|(_, i)| i).unwrap_or(i as u32));
            set.costs.push(region_cost(csum, n as u64));
        }
    }
}

/// Walk every cell the segment between two cell centers passes through.
/// `visit(from_x, from_z, x, z)` gets each cell and the cell it was entered from (the first cell
/// from itself). Returns false at the first rejection.
pub fn supercover(
    x0: i32,
    z0: i32,
    x1: i32,
    z1: i32,
    mut visit: impl FnMut(i32, i32, i32, i32) -> bool,
) -> bool {
    let dx = (x1 - x0).abs();
    let dz = (z1 - z0).abs();
    let sx = if x1 > x0 { 1 } else { -1 };
    let sz = if z1 > z0 { 1 } else { -1 };
    let (mut x, mut z) = (x0, z0);
    if !visit(x, z, x, z) {
        return false;
    }
    let mut ix = 0;
    let mut iz = 0;
    while ix < dx || iz < dz {
        // Compare (0.5 + ix) / dx with (0.5 + iz) / dz without division.
        let lhs = (1 + 2 * ix) * dz;
        let rhs = (1 + 2 * iz) * dx;
        if lhs == rhs {
            // Exactly through a corner: both side cells count, and the step past them.
            if !visit(x, z, x + sx, z) || !visit(x, z, x, z + sz) || !visit(x + sx, z, x + sx, z + sz) {
                return false;
            }
            x += sx;
            z += sz;
            ix += 1;
            iz += 1;
            continue;
        } else if lhs < rhs {
            x += sx;
            ix += 1;
            if !visit(x - sx, z, x, z) {
                return false;
            }
        } else {
            z += sz;
            iz += 1;
            if !visit(x, z - sz, x, z) {
                return false;
            }
        }
    }
    true
}

/// A plain heightfield that implements [`GridGround`]: `f32` heights row-major, optional ground
/// patches. For tests, tools and hosts without their own heightfield type.
#[derive(Debug, Clone, PartialEq)]
pub struct HeightCells {
    /// Samples per row (X).
    pub cols: usize,
    /// Rows (Z).
    pub rows: usize,
    /// Meters between samples.
    pub spacing: f32,
    /// World half-extent along X (sample 0 at `-half_x`).
    pub half_x: f32,
    /// World half-extent along Z (sample 0 at `-half_z`).
    pub half_z: f32,
    /// Subtracted from every raw height.
    pub center_height: f32,
    /// Raw heights (m), `rows * cols`, row-major.
    pub heights: Vec<f32>,
    /// Baked patches; a patch only lowers the ground. The first patch holding a point answers.
    pub patches: Vec<GroundPatch>,
}

impl HeightCells {
    /// A heightfield centered on the origin with `cell_m` spacing (sample 0 at `-cols * cell_m / 2`).
    /// `heights` shorter than `cols * rows` is padded with 0.
    pub fn centered(cols: usize, rows: usize, cell_m: f32, mut heights: Vec<f32>) -> HeightCells {
        heights.resize(cols * rows, 0.0);
        HeightCells {
            cols,
            rows,
            spacing: cell_m,
            half_x: cols as f32 * cell_m * 0.5,
            half_z: rows as f32 * cell_m * 0.5,
            center_height: 0.0,
            heights,
            patches: Vec::new(),
        }
    }

    /// The same field with these patches.
    pub fn with_patches(mut self, patches: Vec<GroundPatch>) -> HeightCells {
        self.patches = patches;
        self
    }

    /// Bilinear height of the samples alone (positions off the field clamp to its border).
    pub fn grid_height_at_world(&self, world_x: f32, world_z: f32) -> f32 {
        let fx = det_clamp(
            (world_x + self.half_x) / self.spacing,
            0.0,
            (self.cols - 1) as f32,
        );
        let fz = det_clamp(
            (world_z + self.half_z) / self.spacing,
            0.0,
            (self.rows - 1) as f32,
        );
        let x0 = fx.floor() as usize;
        let z0 = fz.floor() as usize;
        let x1 = (x0 + 1).min(self.cols - 1);
        let z1 = (z0 + 1).min(self.rows - 1);
        let tx = fx - x0 as f32;
        let tz = fz - z0 as f32;
        let h00 = self.height_at_index(x0, z0);
        let h10 = self.height_at_index(x1, z0);
        let h01 = self.height_at_index(x0, z1);
        let h11 = self.height_at_index(x1, z1);
        let top = h00 + (h10 - h00) * tx;
        let bottom = h01 + (h11 - h01) * tx;
        top + (bottom - top) * tz
    }
}

impl GridGround for HeightCells {
    fn cols(&self) -> usize {
        self.cols
    }
    fn rows(&self) -> usize {
        self.rows
    }
    fn spacing(&self) -> f32 {
        self.spacing
    }
    fn half_x(&self) -> f32 {
        self.half_x
    }
    fn half_z(&self) -> f32 {
        self.half_z
    }
    fn center_height(&self) -> f32 {
        self.center_height
    }
    fn height_at_index(&self, x: usize, z: usize) -> f32 {
        self.heights.get(z * self.cols + x).copied().unwrap_or(0.0) - self.center_height
    }
    fn height_at_world(&self, x: f32, z: f32) -> f32 {
        let grid = self.grid_height_at_world(x, z);
        for patch in &self.patches {
            if let Some(h) = patch.sample(x, z) {
                return det_min(h, grid);
            }
        }
        grid
    }
    fn has_patches(&self) -> bool {
        !self.patches.is_empty()
    }
    fn patch_signature(&self) -> u64 {
        patch_signature(
            self.patches
                .iter()
                .map(|p| (p.cols, p.rows, p.heights.as_slice())),
        )
    }
}

/// A rectangular patch of ground heights over a [`HeightCells`], row-major. Cell `(c, r)` is at
/// `origin + (c, r) * spacing`.
#[derive(Debug, Clone, PartialEq)]
pub struct GroundPatch {
    /// World XZ of cell (0, 0).
    pub origin: [f32; 2],
    /// Meters between cells.
    pub spacing: f32,
    /// Cells per row.
    pub cols: usize,
    /// Rows.
    pub rows: usize,
    /// Heights (m), `rows * cols`.
    pub heights: Vec<f32>,
}

impl GroundPatch {
    /// Bilinear height at a world point, `None` outside the patch.
    pub fn sample(&self, x: f32, z: f32) -> Option<f32> {
        if self.cols < 2
            || self.rows < 2
            || !positive_length(self.spacing)
            || self.heights.len() < self.cols * self.rows
        {
            return None;
        }
        let fx = (x - self.origin[0]) / self.spacing;
        let fz = (z - self.origin[1]) / self.spacing;
        if fx < 0.0 || fz < 0.0 || fx > (self.cols - 1) as f32 || fz > (self.rows - 1) as f32 {
            return None;
        }
        let c0 = (fx.floor() as usize).min(self.cols - 2);
        let r0 = (fz.floor() as usize).min(self.rows - 2);
        let tx = fx - c0 as f32;
        let tz = fz - r0 as f32;
        let at = |c: usize, r: usize| self.heights[r * self.cols + c];
        let (h00, h10, h01, h11) = (at(c0, r0), at(c0 + 1, r0), at(c0, r0 + 1), at(c0 + 1, r0 + 1));
        let top = h00 + (h10 - h00) * tx;
        let bottom = h01 + (h11 - h01) * tx;
        Some(top + (bottom - top) * tz)
    }
}

/// A finite length above zero.
fn positive_length(x: f32) -> bool {
    x.is_finite() && x > 0.0
}

/// `v` clamped to `[0, hi]` by two comparisons, written out so the result never depends on how a
/// target lowers a float `clamp`: below 0 gives 0, above `hi` gives `hi`, anything else (a NaN, a
/// -0.0) passes through unchanged. The same bits as std's `f32::clamp(v, 0.0, hi)`
/// (`clamp_by_compare_matches_std_clamp`). Test-only since the integration with review R1
/// (ADR-8193): `HeightCells::grid_height_at_world` clamps with `regia_detsim::float::det_clamp`
/// (the canonical float policy); this comparison form stays as the tests' std-clamp reference.
#[cfg(test)]
#[inline]
fn clamp_by_compare(v: f32, hi: f32) -> f32 {
    if v < 0.0 {
        0.0
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// A region's mean cell cost, `sum / cells` rounded down and capped at `u16::MAX` (`cells` 0
/// counts as 1). Without areas and overlay every cost is at most [`COST_FACE`], so the cap never
/// bites and the bits are the terrain-only ones.
#[inline]
fn region_cost(sum: u64, cells: u64) -> u16 {
    let mean = sum / u64::max(cells, 1);
    u16::try_from(mean).unwrap_or(u16::MAX)
}

/// The overlay's cells when it applies to profile index `pi`.
#[inline]
fn overlay_for(overlay: Option<&CostOverlay>, pi: usize) -> Option<&[u16]> {
    overlay
        .filter(|o| o.profiles.contains(Profile(pi as u8)))
        .map(|o| o.cells.as_slice())
}

/// FNV-1a 64 (public domain, http://www.isthe.com/chongo/tech/comp/fnv/).
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    /// NaNs folded to one pattern and either zero to +0, independent of the producing host.
    fn f32(&mut self, v: f32) {
        self.u32(canonical(v).to_bits());
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests;
