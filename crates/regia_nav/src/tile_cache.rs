// SPDX-License-Identifier: Zlib
// Portions ported from Recast & Detour (https://github.com/recastnavigation/recastnavigation,
// commit 9f4ce64458), DetourTileCache/Source/DetourTileCache.cpp (obstacle requests, touched-tile
// bookkeeping, cylinder and box obstacle bounds), rewritten in Rust for Regia Engine.
// Copyright (c) 2009-2010 Mikko Mononen memon@inside.org, zlib licence.
// Altered from the original: this is not the original software (see the notes below).
//
// This software is provided 'as-is', without any express or implied
// warranty.  In no event will the authors be held liable for any damages
// arising from the use of this software.
// Permission is granted to anyone to use this software for any purpose,
// including commercial applications, and to alter it and redistribute it
// freely, subject to the following restrictions:
// 1. The origin of this software must not be misrepresented; you must not
//    claim that you wrote the original software. If you use this software
//    in a product, an acknowledgment in the product documentation would be
//    appreciated but is not required.
// 2. Altered source versions must be plainly marked as such, and must not be
//    misrepresented as being the original software.
// 3. This notice may not be removed or altered from any source distribution.

//! The navmesh tile cache (`docs/specs/22-tier2.md` § T2.2, ADR-1484): the walkable surface is
//! split into square tiles, temporary obstacles (cylinders and boxes) are added and removed at
//! runtime, and only the tiles an obstacle touches are rebuilt.
//!
//! # How it works
//!
//! [`NavTileCache::build`] runs the voxelisation and walkable marking of [`crate::NavMeshBuilder`]
//! once and keeps the resulting walkable cell grid: that grid is the cache's stored "layer" (the
//! upstream tile cache keeps compressed layers; here the grid stays uncompressed, which ADR-1484
//! records). The grid is cut into tiles of [`NavTileCacheParams::tile_size`] cells.
//!
//! Adding or removing an obstacle only queues the tiles its footprint touches (the footprint is
//! inflated by the agent radius, because the walkable surface describes where an agent's centre
//! may stand). [`NavTileCache::update`] then rebuilds each queued tile: it restores the tile's
//! cells from the stored grid, removes the cells under every obstacle touching that tile, and
//! polygonises the tile. Untouched tiles keep their polygons. Finally the tiles are stitched into
//! one [`NavMesh`] (a linear pass that concatenates the tile polygons and links polygons across
//! tile borders), which [`NavTileCache::mesh`] and [`NavTileCache::query`] expose.
//!
//! Polygon indices change whenever a tile is rebuilt, so anything that stores indices (a
//! [`crate::NavCrowd`] corridor, a [`crate::NavPath`]) must replan after an update that rebuilt
//! tiles; [`NavTileCache::revision`] increments on every such update, and on every link change.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_core::math::{Vec3, det_sin_cos};
use thiserror::Error;

use crate::areas::{
    MAX_NAV_AREA_VOLUMES, MAX_NAV_AREAS, NavAreaCosts, NavAreaError, NavAreaVolume, painted_area,
};
use crate::builder::{NavBuildError, NavMeshBuilder};
use crate::config::NavMeshBuildParams;
use crate::contours::Contour;
use crate::heightfield::WalkableGrid;
use crate::links::{LinkBlockers, MAX_NAV_LINKS, NavLinkError, NavLinkState, UnloadedTiles, validate_links};
use crate::mesh::{CellRect, NavMesh, NavPoly, OffMeshLink, add_portal, build_polys_rect, cell_edge_portal};
use crate::query::NavQuery;

mod grow;
mod region;
mod stream;

pub use grow::NavGridGrowth;
pub use region::NavRegionRebuild;
pub use stream::{
    MAX_STREAMED_GRID_CELLS, NAV_CELL_FORMAT_VERSION, NavCellLoad, NavPathOutcome, NavStreamedPath,
    NavTileLayout, NavTileStreamStatus,
};
// The cooked cell's layout fingerprint for `cargo xtask check-formats` (ADR-2316).
pub use stream::{NAV_CELL_LAYOUT, nav_cell_migration_sources, nav_cell_schema_fingerprint};

/// The most obstacles one cache holds at once.
pub const MAX_TILE_CACHE_OBSTACLES: usize = 4096;

/// The largest accepted tile edge, in cells.
pub const MAX_TILE_SIZE: u32 = 4096;

/// Parameters of a [`NavTileCache`]: the voxelisation and agent of the mesh, plus the tile size.
#[derive(Debug, Clone, PartialEq)]
pub struct NavTileCacheParams {
    /// Voxelisation and agent parameters (as for [`crate::NavMeshBuilder`]).
    pub build: NavMeshBuildParams,
    /// Tile edge length in cells (`1..=MAX_TILE_SIZE`). A tile covers
    /// `tile_size * build.cell_size` metres on each side.
    pub tile_size: u32,
}

impl Default for NavTileCacheParams {
    fn default() -> Self {
        Self {
            build: NavMeshBuildParams::default(),
            tile_size: 32,
        }
    }
}

/// The shape of a temporary obstacle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NavObstacleShape {
    /// An upright cylinder standing on `base` (the centre of its bottom disc).
    Cylinder {
        /// Centre of the bottom disc (world space).
        base: Vec3,
        /// Radius (metres, `> 0`).
        radius: f32,
        /// Height above `base` (metres, `> 0`).
        height: f32,
    },
    /// An axis-aligned box (the corners may be given in any order).
    Box {
        /// One corner (world space).
        min: Vec3,
        /// The opposite corner (world space).
        max: Vec3,
    },
    /// A box turned about +Y (ADR-1821): a door, a crate set down at an angle.
    OrientedBox {
        /// The box centre (world space).
        center: Vec3,
        /// Half the box size along its own axes (each `> 0`).
        half_extents: Vec3,
        /// The turn about +Y in radians (a positive yaw turns +X toward -Z).
        yaw: f32,
    },
}

impl NavObstacleShape {
    /// Checks the shape: every number finite, a cylinder's radius and height positive, a box's
    /// XZ footprint non-empty. Returns the normalised shape (box corners ordered).
    pub fn validated(self) -> Result<Self, NavTileCacheError> {
        match self {
            Self::Cylinder { base, radius, height } => {
                if !base.is_finite() || !radius.is_finite() || !height.is_finite() {
                    return Err(NavTileCacheError::InvalidObstacle(
                        "cylinder position, radius and height must be finite",
                    ));
                }
                if radius <= 0.0 {
                    return Err(NavTileCacheError::InvalidObstacle(
                        "cylinder radius must be positive",
                    ));
                }
                if height <= 0.0 {
                    return Err(NavTileCacheError::InvalidObstacle(
                        "cylinder height must be positive",
                    ));
                }
                Ok(self)
            }
            Self::Box { min, max } => {
                if !min.is_finite() || !max.is_finite() {
                    return Err(NavTileCacheError::InvalidObstacle("box corners must be finite"));
                }
                let (lo, hi) = (min.min(max), min.max(max));
                if hi.x - lo.x <= 0.0 || hi.z - lo.z <= 0.0 {
                    return Err(NavTileCacheError::InvalidObstacle(
                        "box footprint must have a positive width and depth",
                    ));
                }
                Ok(Self::Box { min: lo, max: hi })
            }
            Self::OrientedBox {
                center,
                half_extents,
                yaw,
            } => {
                if !center.is_finite() || !half_extents.is_finite() || !yaw.is_finite() {
                    return Err(NavTileCacheError::InvalidObstacle(
                        "oriented box center, half extents and yaw must be finite",
                    ));
                }
                if half_extents.cmple(Vec3::ZERO).any() {
                    return Err(NavTileCacheError::InvalidObstacle(
                        "oriented box half extents must be positive",
                    ));
                }
                Ok(self)
            }
        }
    }

    /// The oriented box's axes in XZ: its local +X and +Z as world `(x, z)` directions. From the
    /// software trig so the cell test at an axis boundary is the same on every target (audit
    /// sprint-audit-g-nav-determinism M5); yaw 0 is exactly `(1, -0)` / `(0, 1)` everywhere.
    fn yaw_axes(yaw: f32) -> ((f32, f32), (f32, f32)) {
        let (sin, cos) = det_sin_cos(yaw);
        // A rotation about +Y by `yaw`: local +X → (cos, -sin), local +Z → (sin, cos).
        ((cos, -sin), (sin, cos))
    }

    /// The same shape moved by `delta`.
    pub(crate) fn translated(self, delta: Vec3) -> Self {
        match self {
            Self::Cylinder { base, radius, height } => Self::Cylinder {
                base: base + delta,
                radius,
                height,
            },
            Self::Box { min, max } => Self::Box {
                min: min + delta,
                max: max + delta,
            },
            Self::OrientedBox {
                center,
                half_extents,
                yaw,
            } => Self::OrientedBox {
                center: center + delta,
                half_extents,
                yaw,
            },
        }
    }

    /// The world bounds of the shape (`min`, `max`).
    pub fn bounds(&self) -> (Vec3, Vec3) {
        match *self {
            Self::Cylinder { base, radius, height } => (
                Vec3::new(base.x - radius, base.y, base.z - radius),
                Vec3::new(base.x + radius, base.y + height, base.z + radius),
            ),
            Self::Box { min, max } => (min.min(max), min.max(max)),
            Self::OrientedBox {
                center,
                half_extents: h,
                yaw,
            } => {
                let ((ax, az), (bx, bz)) = Self::yaw_axes(yaw);
                let ex = (ax * h.x).abs() + (bx * h.z).abs();
                let ez = (az * h.x).abs() + (bz * h.z).abs();
                (
                    Vec3::new(center.x - ex, center.y - h.y, center.z - ez),
                    Vec3::new(center.x + ex, center.y + h.y, center.z + ez),
                )
            }
        }
    }

    /// Whether the cell centre `(x, z)` with surface height `y` is blocked by the shape inflated
    /// by `inflate` metres horizontally. The vertical test accepts surfaces from `climb` below the
    /// shape's bottom up to its top, so an obstacle standing on a floor blocks it even when the
    /// voxelised floor height sits slightly under the obstacle's base.
    pub(crate) fn blocks(&self, x: f32, y: f32, z: f32, inflate: f32, climb: f32) -> bool {
        let (lo, hi) = self.bounds();
        if !(lo.y - climb..=hi.y).contains(&y) {
            return false;
        }
        match *self {
            Self::Cylinder { base, radius, .. } => {
                let (dx, dz) = (x - base.x, z - base.z);
                let r = radius + inflate;
                dx * dx + dz * dz <= r * r
            }
            Self::Box { .. } => {
                (lo.x - inflate..=hi.x + inflate).contains(&x)
                    && (lo.z - inflate..=hi.z + inflate).contains(&z)
            }
            Self::OrientedBox {
                center,
                half_extents: h,
                yaw,
            } => {
                // Into the box's frame: project on its axes.
                let ((ax, az), (bx, bz)) = Self::yaw_axes(yaw);
                let (dx, dz) = (x - center.x, z - center.z);
                let u = dx * ax + dz * az;
                let v = dx * bx + dz * bz;
                u.abs() <= h.x + inflate && v.abs() <= h.z + inflate
            }
        }
    }
}

/// The id of an area volume in a [`NavTileCache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NavAreaVolumeId(pub u64);

impl std::fmt::Display for NavAreaVolumeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The id of an obstacle in a [`NavTileCache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NavObstacleId(pub u64);

impl std::fmt::Display for NavObstacleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Why a tile-cache operation failed. [`Self::code`] is the stable agent-protocol reason.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum NavTileCacheError {
    /// Voxelising the level geometry failed.
    #[error("tile cache build failed: {0}")]
    Build(#[from] NavBuildError),
    /// `tile_size` was 0 or larger than `MAX_TILE_SIZE`.
    #[error("tile_size {0} is outside 1..=4096")]
    InvalidTileSize(u32),
    /// The obstacle shape is invalid (non-finite, zero radius or height, empty footprint).
    #[error("invalid obstacle: {0}")]
    InvalidObstacle(&'static str),
    /// No obstacle has this id.
    #[error("no obstacle with id {0}")]
    UnknownObstacle(NavObstacleId),
    /// The cache already holds `MAX_TILE_CACHE_OBSTACLES` obstacles.
    #[error("the tile cache already holds the maximum of 4096 obstacles")]
    TooManyObstacles,
    /// A region rebuild got a non-finite box or non-finite geometry.
    #[error("invalid region: {0}")]
    InvalidRegion(&'static str),
    /// A streaming call on a cache built whole with [`NavTileCache::build`].
    #[error("this tile cache is not streamed")]
    NotStreamed,
    /// A tile coordinate outside the tile grid.
    #[error("tile ({0}, {1}) is outside the tile grid")]
    TileOutOfRange(u32, u32),
    /// A streamed layout with no cells or too many, or a non-finite origin.
    #[error("invalid tile layout: {0}")]
    InvalidLayout(&'static str),
    /// A cooked cell blob that is truncated or damaged.
    #[error("corrupt nav cell blob: {0}")]
    CorruptCell(&'static str),
    /// A cooked cell blob written by a newer format version.
    #[error("nav cell blob version {found} is newer than the supported {supported}")]
    NewerCellVersion {
        /// The version in the blob.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// A cooked cell blob cooked for another tile layout or other build parameters.
    #[error("nav cell blob was cooked for a different tile layout")]
    LayoutMismatch,
    /// No loaded cell has this id.
    #[error("no loaded nav cell with id {0}")]
    UnknownCell(u64),
    /// An invalid area id or cost.
    #[error("{0}")]
    Area(#[from] NavAreaError),
    /// No area volume has this id.
    #[error("no area volume with id {0}")]
    UnknownAreaVolume(NavAreaVolumeId),
    /// The cache already holds `MAX_NAV_AREA_VOLUMES` volumes.
    #[error("the tile cache already holds the maximum of 4096 area volumes")]
    TooManyAreaVolumes,
    /// The grid cannot grow (ADR-2589, ADR-5007): a streamed cache, a non-finite box, too many
    /// cells, or a -X/-Z cell shift that is not exactly representable as f32.
    #[error("the tile grid cannot grow: {0}")]
    GridGrowth(&'static str),
}

impl NavTileCacheError {
    /// The stable lowercase reason code (agent protocol `data.reason`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Build(NavBuildError::NoGeometry) => "nav_no_geometry",
            Self::Build(NavBuildError::NoWalkableSurface) => "nav_no_walkable_surface",
            Self::Build(NavBuildError::InvalidInput(_)) => "nav_invalid_build_input",
            Self::InvalidTileSize(_) => "nav_invalid_tile_size",
            Self::InvalidObstacle(_) => "nav_invalid_obstacle",
            Self::UnknownObstacle(_) => "nav_unknown_obstacle",
            Self::TooManyObstacles => "nav_too_many_obstacles",
            Self::InvalidRegion(_) => "nav_invalid_region",
            Self::NotStreamed => "nav_not_streamed",
            Self::TileOutOfRange(..) => "nav_tile_out_of_range",
            Self::InvalidLayout(_) => "nav_invalid_layout",
            Self::CorruptCell(_) => "nav_cell_corrupt",
            Self::NewerCellVersion { .. } => "nav_cell_version",
            Self::LayoutMismatch => "nav_cell_layout_mismatch",
            Self::UnknownCell(_) => "nav_unknown_cell",
            Self::Area(e) => e.code(),
            Self::UnknownAreaVolume(_) => "nav_unknown_area_volume",
            Self::TooManyAreaVolumes => "nav_too_many_area_volumes",
            Self::GridGrowth(_) => "nav_grid_growth",
        }
    }
}

/// What one [`NavTileCache::update`] call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavTileUpdate {
    /// The tiles rebuilt by this call, `(tile_x, tile_z)`, in rebuild order.
    pub rebuilt: Vec<(u32, u32)>,
    /// Tiles still queued (only non-zero for [`NavTileCache::update_limited`]).
    pub remaining: usize,
}

/// What one tile rebuild changed in the stitched polygon numbering.
///
/// `replaced` are polygon-index ranges in the numbering from **before** the stitch. An agent's
/// corridor that contains any of those indices has to be planned again. Every other corridor
/// index `p` becomes `p + sum of deltas` for each `(start, delta)` in `shifts` with `p >= start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavTileRevision {
    /// [`NavTileCache::revision`] after the rebuild.
    pub revision: u64,
    /// Pre-rebuild polygon ranges whose polygons were replaced.
    pub replaced: Vec<std::ops::Range<u32>>,
    /// `(first_old_index, delta)` added to every old index at or above `first_old_index`.
    pub shifts: Vec<(u32, i32)>,
}

/// One tile: its cell rectangle and its current polygons.
#[derive(Debug, Clone)]
struct Tile {
    /// The tile's cells in the full grid.
    rect: CellRect,
    /// The tile's polygons; portal neighbours index this list (stitching offsets them).
    polys: Vec<NavPoly>,
    /// Rect-local cell → index into `polys` (`u32::MAX` = not walkable).
    owner: Vec<u32>,
    /// The tile's traced region contours (region ids local to the tile).
    contours: Vec<Contour>,
    /// How many regions the tile's contours use.
    regions: u32,
    /// Rebuilds since the cache was built (the initial build is not counted).
    rebuilds: u64,
}

/// The links one tile owns ([`NavTileCache::link_tiles`], ADR-2589).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavTileLinks {
    /// The tile, `(tile_x, tile_z)`.
    pub tile: (u32, u32),
    /// The tile is loaded (always, for a cache built whole).
    pub loaded: bool,
    /// Of `links`: the ones it streamed in with its cell.
    pub streamed: usize,
    /// Links whose `from` point lies on the tile.
    pub links: usize,
    /// Of `links`: graph edges in the current mesh.
    pub active: usize,
    /// Of `links`: unusable because the tile under their `to` point is not loaded.
    pub waiting: usize,
}

/// A registered obstacle and the tiles its footprint touches.
#[derive(Debug, Clone)]
struct ObstacleEntry {
    shape: NavObstacleShape,
    touched: Vec<u32>,
}

/// A registered area volume and the tiles it paints.
#[derive(Debug, Clone)]
struct AreaEntry {
    volume: NavAreaVolume,
    touched: Vec<u32>,
}

/// A tiled navmesh with runtime obstacles (see the module docs). Insert it as a resource for the
/// agent-protocol `nav.tile_*` methods and [`crate::NavCrowdPlugin`].
#[derive(Resource, Debug, Clone)]
pub struct NavTileCache {
    params: NavTileCacheParams,
    /// The pristine walkable grid (no obstacles): the stored layer every rebuild starts from.
    base: WalkableGrid,
    /// The walkable grid with the current obstacles applied to every built tile.
    work: WalkableGrid,
    tiles_x: u32,
    tiles_z: u32,
    tiles: Vec<Tile>,
    obstacles: BTreeMap<u64, ObstacleEntry>,
    next_obstacle: u64,
    area_volumes: BTreeMap<u64, AreaEntry>,
    next_area_volume: u64,
    area_costs: NavAreaCosts,
    pending: BTreeSet<u32>,
    off_mesh_links: Vec<OffMeshLink>,
    mesh: NavMesh,
    total_rebuilds: u64,
    revision: u64,
    /// Walkable-grid cells copied into the mesh by the last [`Self::update`] or
    /// [`Self::update_limited`] (0 when nothing was rebuilt).
    cells_copied_last: usize,
    /// The polygon ranges the last tile rebuild replaced, in the pre-rebuild numbering.
    pending_revision: Option<NavTileRevision>,
    /// Streaming state; `None` for a cache built whole.
    stream: Option<stream::StreamState>,
}

impl NavTileCache {
    /// Voxelises `triangles` once and builds every tile. Errors: the builder's (no geometry, no
    /// walkable surface, invalid input) and an out-of-range tile size.
    pub fn build(params: NavTileCacheParams, triangles: &[[Vec3; 3]]) -> Result<Self, NavTileCacheError> {
        Self::build_with_triangle_areas(params, triangles, &[])
    }

    /// [`Self::build`] with an area id per triangle (ADR-2420, see
    /// [`crate::NavMeshBuilder::build_with_triangle_areas`]): the walkable cells keep their
    /// triangle's area, [`Self::cook_cell`] writes it into the cooked tiles, and a region rebuild
    /// keeps it (a rebuild replaces the heights of its cells, not their areas). Area volumes paint
    /// over it. With no painted triangle (an empty slice or all zeros) the cache, and every cell
    /// it cooks, is byte for byte the one [`Self::build`] makes. Errors: those of [`Self::build`],
    /// and an area list that is neither empty nor one id below 64 per triangle.
    pub fn build_with_triangle_areas(
        params: NavTileCacheParams,
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
    ) -> Result<Self, NavTileCacheError> {
        let _span = tracing::info_span!("nav_tile_cache_build").entered();
        if params.tile_size == 0 || params.tile_size > MAX_TILE_SIZE {
            return Err(NavTileCacheError::InvalidTileSize(params.tile_size));
        }
        let grid =
            NavMeshBuilder::new(params.build.clone()).build_grid_with_areas(triangles, triangle_areas)?;
        let mut cache = Self::with_grid(params, grid);
        for t in 0..cache.tiles.len() {
            cache.rebuild_tile(t as u32);
        }
        cache.stitch();
        Ok(cache)
    }

    /// A cache over `grid` with its tiles laid out and none polygonised yet.
    fn with_grid(params: NavTileCacheParams, mut grid: WalkableGrid) -> Self {
        grid.areas.resize(grid.cells.len(), 0);
        let ts = params.tile_size as usize;
        let tiles_x = grid.width.div_ceil(ts);
        let tiles_z = grid.height.div_ceil(ts);
        let mut tiles = Vec::with_capacity(tiles_x * tiles_z);
        for tz in 0..tiles_z {
            for tx in 0..tiles_x {
                let rect = CellRect {
                    x0: tx * ts,
                    z0: tz * ts,
                    x1: ((tx + 1) * ts).min(grid.width),
                    z1: ((tz + 1) * ts).min(grid.height),
                };
                tiles.push(Tile {
                    rect,
                    polys: Vec::new(),
                    owner: Vec::new(),
                    contours: Vec::new(),
                    regions: 0,
                    rebuilds: 0,
                });
            }
        }
        let placeholder = NavMesh::from_parts(
            Vec::new(),
            grid.clone(),
            params.build.clone(),
            0,
            Vec::new(),
            Vec::new(),
        );
        Self {
            params,
            work: grid.clone(),
            base: grid,
            tiles_x: tiles_x as u32,
            tiles_z: tiles_z as u32,
            tiles,
            obstacles: BTreeMap::new(),
            next_obstacle: 0,
            area_volumes: BTreeMap::new(),
            next_area_volume: 0,
            area_costs: NavAreaCosts::default(),
            pending: BTreeSet::new(),
            off_mesh_links: Vec::new(),
            mesh: placeholder,
            total_rebuilds: 0,
            revision: 0,
            cells_copied_last: 0,
            pending_revision: None,
            stream: None,
        }
    }

    /// The parameters the cache was built with.
    pub fn params(&self) -> &NavTileCacheParams {
        &self.params
    }

    /// The tile grid dimensions `(tiles_x, tiles_z)`.
    pub fn tile_grid(&self) -> (u32, u32) {
        (self.tiles_x, self.tiles_z)
    }

    /// The total number of tiles.
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// The world XZ bounds of tile `(tx, tz)` as `(min, max)` (y is 0), or `None` out of range.
    pub fn tile_bounds(&self, tx: u32, tz: u32) -> Option<(Vec3, Vec3)> {
        let tile = self.tiles.get(self.tile_index(tx, tz)?)?;
        let c = self.base.cell_size;
        let o = self.base.origin;
        Some((
            Vec3::new(o.x + tile.rect.x0 as f32 * c, 0.0, o.z + tile.rect.z0 as f32 * c),
            Vec3::new(o.x + tile.rect.x1 as f32 * c, 0.0, o.z + tile.rect.z1 as f32 * c),
        ))
    }

    /// The tile containing the world point's XZ position, or `None` outside the grid.
    pub fn tile_at(&self, point: Vec3) -> Option<(u32, u32)> {
        if !point.is_finite() {
            return None;
        }
        let c = self.base.cell_size;
        let fx = ((point.x - self.base.origin.x) / c).floor();
        let fz = ((point.z - self.base.origin.z) / c).floor();
        // Compared as integers: `width as f32` rounds above 2^24 and would drop the last column.
        let inside_x = fx >= 0.0 && (fx as usize) < self.base.width;
        let inside_z = fz >= 0.0 && (fz as usize) < self.base.height;
        if !(inside_x && inside_z) {
            return None;
        }
        let ts = self.params.tile_size;
        Some((fx as u32 / ts, fz as u32 / ts))
    }

    /// How many times tile `(tx, tz)` was rebuilt since the cache was built, or `None` out of range.
    pub fn tile_rebuild_count(&self, tx: u32, tz: u32) -> Option<u64> {
        Some(self.tiles.get(self.tile_index(tx, tz)?)?.rebuilds)
    }

    /// The polygon count of tile `(tx, tz)`, or `None` out of range.
    pub fn tile_poly_count(&self, tx: u32, tz: u32) -> Option<usize> {
        Some(self.tiles.get(self.tile_index(tx, tz)?)?.polys.len())
    }

    /// Tile rebuilds since the cache was built (all tiles; the initial build is not counted).
    pub fn total_rebuilds(&self) -> u64 {
        self.total_rebuilds
    }

    /// Increments every time an update rebuilds at least one tile (polygon indices changed) and
    /// whenever the links change (the routes did): a crowd following it replans.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The queued tiles, `(tile_x, tile_z)`, in rebuild order.
    pub fn pending_tiles(&self) -> Vec<(u32, u32)> {
        self.pending.iter().map(|&t| self.tile_xz(t)).collect()
    }

    /// How many tiles are queued, without listing them.
    pub fn pending_tile_count(&self) -> usize {
        self.pending.len()
    }

    /// The registered obstacles in id order.
    pub fn obstacles(&self) -> impl Iterator<Item = (NavObstacleId, NavObstacleShape)> + '_ {
        self.obstacles.iter().map(|(&id, e)| (NavObstacleId(id), e.shape))
    }

    /// How many obstacles are registered.
    pub fn obstacle_count(&self) -> usize {
        self.obstacles.len()
    }

    /// The tiles obstacle `id` touches, or `None` for an unknown id.
    pub fn touched_tiles(&self, id: NavObstacleId) -> Option<Vec<(u32, u32)>> {
        let entry = self.obstacles.get(&id.0)?;
        Some(entry.touched.iter().map(|&t| self.tile_xz(t)).collect())
    }

    /// The current stitched mesh (obstacles applied up to the last [`Self::update`]).
    pub fn mesh(&self) -> &NavMesh {
        &self.mesh
    }

    /// A query over the current mesh. Obstacles are already baked into the tiles, so the query
    /// carries no extra [`crate::NavObstacles`].
    pub fn query(&self) -> NavQuery<'_> {
        NavQuery {
            mesh: &self.mesh,
            obstacles: None,
        }
    }

    /// Sets the off-mesh links carried by the stitched mesh (applied immediately; a link whose
    /// end lies under an obstacle is blocked until the obstacle goes).
    ///
    /// A link [`Self::try_set_off_mesh_links`] would refuse (a non-finite endpoint, a radius or cost
    /// scale that is not finite and positive, a negative or non-finite duration, an area past 63)
    /// is left out with a warning, and the list stops at `MAX_NAV_LINKS`: a streamed cache would
    /// otherwise cook it into cell blobs its own loader rejects. The kept links keep their order.
    pub fn set_off_mesh_links(&mut self, mut links: Vec<OffMeshLink>) {
        let before = links.len();
        links.retain(|link| check_link(link).is_ok());
        links.truncate(MAX_NAV_LINKS);
        if links.len() != before {
            tracing::warn!(
                target: "regia::nav",
                dropped = before - links.len(),
                kept = links.len(),
                "off-mesh links left out: invalid values or past the link cap"
            );
        }
        self.store_links(links);
    }

    /// [`Self::set_off_mesh_links`] that refuses the whole list when any link is invalid or the
    /// list is longer than `MAX_NAV_LINKS` (nothing changes then).
    pub fn try_set_off_mesh_links(&mut self, links: Vec<OffMeshLink>) -> Result<(), NavLinkError> {
        validate_links(&links)?;
        if let Some(index) = links.iter().position(|l| check_link(l).is_err()) {
            return Err(NavLinkError::InvalidLink {
                index,
                reason: "area must be below 64",
            });
        }
        self.store_links(links);
        Ok(())
    }

    /// Stores checked links and applies them.
    fn store_links(&mut self, links: Vec<OffMeshLink>) {
        self.off_mesh_links = links;
        self.apply_links();
        self.revision += 1;
    }

    /// The off-mesh links the cache carries through tile rebuilds (its own: a streamed cache's
    /// mesh adds the links of its loaded tiles after these, see [`Self::tile_links`]).
    pub fn off_mesh_links(&self) -> &[OffMeshLink] {
        &self.off_mesh_links
    }

    /// The links tile `(tx, tz)` streamed in with its cell (ADR-2589); empty for a tile that is
    /// not loaded, holds none, or is out of range, and for a cache built whole.
    pub fn tile_links(&self, tx: u32, tz: u32) -> &[OffMeshLink] {
        self.tile_index(tx, tz)
            .and_then(|t| self.stream.as_ref()?.tile_links.get(&(t as u32)))
            .map_or(&[], Vec::as_slice)
    }

    /// The tile that owns `link` and streams it with its cell: the one under its `from` point,
    /// `(tile_x, tile_z)`, or `None` off the grid (such a link is never cooked).
    pub fn link_tile(&self, link: &OffMeshLink) -> Option<(u32, u32)> {
        self.tile_at(link.from)
    }

    /// The links of every tile, for the `nav.tile_links` agent method (ADR-2589): one row per
    /// tile that owns a link (its own links by `from` point plus the ones it streamed in), in
    /// tile order, with the mesh states of those links.
    pub fn link_tiles(&self) -> Vec<NavTileLinks> {
        let mut rows: BTreeMap<u32, NavTileLinks> = BTreeMap::new();
        let states = self.mesh.link_statuses();
        let owners = self.off_mesh_links.iter().map(|l| self.link_owner(l)).chain(
            self.stream
                .iter()
                .flat_map(|s| s.tile_links.iter())
                .flat_map(|(&t, links)| links.iter().map(move |_| Some(t))),
        );
        // Mesh order is own links, then the streamed ones: the states line up once the mesh carries
        // the current list (a load or unload applies on the next update).
        let current = self.mesh.off_mesh_links == self.mesh_links();
        for (i, owner) in owners.enumerate() {
            let Some(t) = owner else { continue };
            let (tx, tz) = self.tile_xz(t);
            let row = rows.entry(t).or_insert_with(|| NavTileLinks {
                tile: (tx, tz),
                loaded: self.tile_is_loaded(t),
                streamed: 0,
                links: 0,
                active: 0,
                waiting: 0,
            });
            row.links += 1;
            if i >= self.off_mesh_links.len() {
                row.streamed += 1;
            }
            let state = if current {
                states.get(i).map(|s| s.state)
            } else {
                None
            };
            match state {
                Some(NavLinkState::Active { .. }) => row.active += 1,
                Some(NavLinkState::Unresolved { .. }) => {
                    let link = self.mesh.off_mesh_links.get(i);
                    let other = link.and_then(|l| self.tile_at(l.to));
                    if other.is_some_and(|(x, z)| self.tile_loaded(x, z) == Some(false)) {
                        row.waiting += 1;
                    }
                }
                _ => {}
            }
        }
        rows.into_values().collect()
    }

    /// Switches every link with `user_id` on or off (applied immediately); returns how many
    /// changed.
    pub fn set_link_enabled(&mut self, user_id: u64, enabled: bool) -> usize {
        let mut changed = 0;
        let streamed = self
            .stream
            .iter_mut()
            .flat_map(|s| s.tile_links.values_mut().flatten());
        for link in self.off_mesh_links.iter_mut().chain(streamed) {
            if link.user_id == user_id && link.enabled != enabled {
                link.enabled = enabled;
                changed += 1;
            }
        }
        if changed > 0 {
            self.apply_links();
            self.revision += 1;
        }
        changed
    }

    /// Replaces obstacle `id`'s shape (a move, a turn or a resize) and keeps its id. Queues the
    /// tiles of both the old and the new footprint and returns them (sorted). Errors: an unknown
    /// id or an invalid shape.
    pub fn move_obstacle(
        &mut self,
        id: NavObstacleId,
        shape: NavObstacleShape,
    ) -> Result<Vec<(u32, u32)>, NavTileCacheError> {
        let shape = shape.validated()?;
        let touched = self.touched_by(&shape);
        let entry = self
            .obstacles
            .get_mut(&id.0)
            .ok_or(NavTileCacheError::UnknownObstacle(id))?;
        let mut queued: BTreeSet<u32> = entry.touched.iter().copied().collect();
        queued.extend(touched.iter().copied());
        entry.shape = shape;
        entry.touched = touched;
        self.pending.extend(queued.iter().copied());
        Ok(queued.into_iter().map(|t| self.tile_xz(t)).collect())
    }

    /// Registers an obstacle and queues the tiles it touches; the mesh changes on the next
    /// [`Self::update`]. Errors: an invalid shape, or the cache is full.
    pub fn add_obstacle(&mut self, shape: NavObstacleShape) -> Result<NavObstacleId, NavTileCacheError> {
        let shape = shape.validated()?;
        if self.obstacles.len() >= MAX_TILE_CACHE_OBSTACLES {
            return Err(NavTileCacheError::TooManyObstacles);
        }
        let touched = self.touched_by(&shape);
        self.pending.extend(touched.iter().copied());
        let id = self.next_obstacle;
        self.next_obstacle = self.next_obstacle.wrapping_add(1);
        self.obstacles.insert(id, ObstacleEntry { shape, touched });
        Ok(NavObstacleId(id))
    }

    /// Removes an obstacle and queues the tiles it touched. Errors: an unknown id.
    pub fn remove_obstacle(&mut self, id: NavObstacleId) -> Result<NavObstacleShape, NavTileCacheError> {
        let entry = self
            .obstacles
            .remove(&id.0)
            .ok_or(NavTileCacheError::UnknownObstacle(id))?;
        self.pending.extend(entry.touched.iter().copied());
        Ok(entry.shape)
    }

    /// Registers an area volume and queues the tiles it paints; the mesh changes on the next
    /// [`Self::update`]. Volumes survive a tile unload and paint cells loaded later, over the area
    /// a cooked cell carries. Errors: an invalid shape or area id, or the cache is full.
    pub fn add_area_volume(&mut self, volume: NavAreaVolume) -> Result<NavAreaVolumeId, NavTileCacheError> {
        let volume = volume.validated()?;
        if self.area_volumes.len() >= MAX_NAV_AREA_VOLUMES {
            return Err(NavTileCacheError::TooManyAreaVolumes);
        }
        let touched = self.tiles_under(&volume.shape, 0.0);
        self.pending.extend(touched.iter().copied());
        let id = self.next_area_volume;
        self.next_area_volume = self.next_area_volume.wrapping_add(1);
        self.area_volumes.insert(id, AreaEntry { volume, touched });
        Ok(NavAreaVolumeId(id))
    }

    /// Removes an area volume and queues the tiles it painted. Errors: an unknown id.
    pub fn remove_area_volume(&mut self, id: NavAreaVolumeId) -> Result<NavAreaVolume, NavTileCacheError> {
        let entry = self
            .area_volumes
            .remove(&id.0)
            .ok_or(NavTileCacheError::UnknownAreaVolume(id))?;
        self.pending.extend(entry.touched.iter().copied());
        Ok(entry.volume)
    }

    /// The registered area volumes in id order (the paint order: later ids win).
    pub fn area_volumes(&self) -> impl Iterator<Item = (NavAreaVolumeId, NavAreaVolume)> + '_ {
        self.area_volumes
            .iter()
            .map(|(&id, e)| (NavAreaVolumeId(id), e.volume))
    }

    /// The registered volume `id`.
    pub fn area_volume(&self, id: NavAreaVolumeId) -> Option<NavAreaVolume> {
        self.area_volumes.get(&id.0).map(|e| e.volume)
    }

    /// What each area costs per metre on this cache's mesh.
    pub fn area_costs(&self) -> &NavAreaCosts {
        &self.area_costs
    }

    /// Replaces the area cost table, applied immediately. [`Self::revision`] moves, so a following
    /// crowd replans.
    pub fn set_area_costs(&mut self, costs: NavAreaCosts) {
        self.area_costs = costs;
        self.mesh.set_area_costs(costs);
        self.revision += 1;
        // Area costs are not a tile patch: the next crowd sync replans every agent.
        self.pending_revision = None;
    }

    /// Rebuilds every queued tile and restitches the mesh (when any tile was rebuilt).
    pub fn update(&mut self) -> NavTileUpdate {
        self.update_limited(usize::MAX)
    }

    /// Rebuilds at most `max_tiles` queued tiles (lowest tile index first) and restitches the
    /// mesh; the rest stay queued for the next call. Use it to spread rebuilds over frames.
    pub fn update_limited(&mut self, max_tiles: usize) -> NavTileUpdate {
        let _span = tracing::info_span!("nav_tile_cache_update", pending = self.pending.len()).entered();
        // Two updates before the crowd consumes the first one: the ranges no longer describe one
        // stitch, so the next sync replans every agent instead of applying a partial patch.
        let had_unconsumed = self.pending_revision.is_some();
        let old_counts: Vec<u32> = self.tiles.iter().map(|t| t.polys.len() as u32).collect();
        let mut rebuilt = Vec::new();
        let mut rebuilt_idx = Vec::new();
        self.cells_copied_last = 0;
        while rebuilt.len() < max_tiles {
            let Some(t) = self.pending.pop_first() else {
                break;
            };
            self.rebuild_tile(t);
            self.note_tile_rebuilt(t);
            self.tiles[t as usize].rebuilds += 1;
            self.total_rebuilds += 1;
            rebuilt_idx.push(t);
            rebuilt.push(self.tile_xz(t));
        }
        if !rebuilt_idx.is_empty() {
            self.stitch_tiles(&rebuilt_idx);
            self.revision += 1;
            if had_unconsumed {
                // The waiting ranges described the previous stitch, not this one.
                self.pending_revision = None;
            } else {
                let (replaced, shifts) = polygon_shift(&old_counts, &self.tiles, &rebuilt_idx);
                self.pending_revision = Some(NavTileRevision {
                    revision: self.revision,
                    replaced,
                    shifts,
                });
            }
        }
        NavTileUpdate {
            rebuilt,
            remaining: self.pending.len(),
        }
    }

    /// The polygon ranges the last tile rebuild replaced, removing them so a second sync does not
    /// apply the same patch again. `None` when the last update rebuilt nothing, or when an earlier
    /// patch was still waiting (that case needs a full replan).
    pub fn take_partial(&mut self) -> Option<NavTileRevision> {
        self.pending_revision.take()
    }

    /// Walkable-grid cells copied into the mesh by the last [`Self::update`] or
    /// [`Self::update_limited`]. A rebuild that does not run copies nothing.
    pub fn cells_copied(&self) -> usize {
        self.cells_copied_last
    }

    /// The polygon ranges the last tile rebuild replaced, if that call rebuilt anything.
    ///
    /// Ranges use the polygon numbering from before the stitch. [`crate::NavCrowd::sync_tile_revision`]
    /// replans an agent whose corridor contains one of them and renumbers the others by `shifts`.
    pub fn last_partial_revision(&self) -> Option<&NavTileRevision> {
        self.pending_revision.as_ref()
    }

    fn tile_index(&self, tx: u32, tz: u32) -> Option<usize> {
        (tx < self.tiles_x && tz < self.tiles_z).then(|| tz as usize * self.tiles_x as usize + tx as usize)
    }

    fn tile_xz(&self, t: u32) -> (u32, u32) {
        (t % self.tiles_x, t / self.tiles_x)
    }

    /// The inclusive cell range whose centres fall inside `[lo, hi]` along one axis, clamped to
    /// `0..n`, or `None` when empty.
    fn cell_span(lo: f32, hi: f32, origin: f32, cell: f32, n: usize) -> Option<(usize, usize)> {
        let first = ((lo - origin) / cell - 0.5).ceil();
        let last = ((hi - origin) / cell - 0.5).floor();
        let top = n.checked_sub(1)?;
        // Clamp in integers: `n as f32 - 1.0` rounds back to `n` above 2^24 cells.
        if last < 0.0 || first > last {
            return None;
        }
        let (first, last) = (first.max(0.0) as usize, last.max(0.0) as usize);
        if first > top {
            return None;
        }
        Some((first, last.min(top)))
    }

    /// The tiles whose cells the inflated shape can block (sorted tile indices).
    fn touched_by(&self, shape: &NavObstacleShape) -> Vec<u32> {
        self.tiles_under(shape, self.params.build.agent.radius)
    }

    /// The tiles whose cells `shape`, grown by `r` metres, reaches (sorted tile indices).
    fn tiles_under(&self, shape: &NavObstacleShape, r: f32) -> Vec<u32> {
        let (lo, hi) = shape.bounds();
        let g = &self.base;
        let (Some((x0, x1)), Some((z0, z1))) = (
            Self::cell_span(lo.x - r, hi.x + r, g.origin.x, g.cell_size, g.width),
            Self::cell_span(lo.z - r, hi.z + r, g.origin.z, g.cell_size, g.height),
        ) else {
            return Vec::new();
        };
        let ts = self.params.tile_size as usize;
        let mut out = Vec::new();
        for tz in z0 / ts..=z1 / ts {
            for tx in x0 / ts..=x1 / ts {
                let t = tz * self.tiles_x as usize + tx;
                if t < self.tiles.len() {
                    out.push(t as u32);
                }
            }
        }
        out
    }

    /// Restores tile `t`'s cells from the stored grid, removes the cells under every obstacle
    /// touching it, and polygonises the tile.
    fn rebuild_tile(&mut self, t: u32) {
        let rect = self.tiles[t as usize].rect;
        let g = &self.base;
        let inflate = self.params.build.agent.radius;
        let climb = self.params.build.agent.max_climb;
        let touching: Vec<NavObstacleShape> = self
            .obstacles
            .values()
            .filter(|e| e.touched.contains(&t))
            .map(|e| e.shape)
            .collect();
        let painting = self.volumes_on(t);
        for z in rect.z0..rect.z1 {
            for x in rect.x0..rect.x1 {
                let idx = z * g.width + x;
                let mut cell = g.cells[idx];
                let mut area = g.area_at(idx);
                if let Some(y) = cell {
                    let cx = g.origin.x + (x as f32 + 0.5) * g.cell_size;
                    let cz = g.origin.z + (z as f32 + 0.5) * g.cell_size;
                    if touching.iter().any(|s| s.blocks(cx, y, cz, inflate, climb)) {
                        cell = None;
                    } else {
                        area = painted_area(g, &painting, (x, z, y), climb, area);
                    }
                }
                self.work.cells[idx] = cell;
                self.work.areas[idx] = area;
            }
        }
        let (polys, owner) = build_polys_rect(&self.work, &self.params.build, rect);
        let sub = sub_grid(&self.work, rect);
        let regions = crate::regions::regionize(&sub, &self.params.build.agent);
        let contours = crate::contours::trace_contours(&regions, &sub);
        let tile = &mut self.tiles[t as usize];
        tile.polys = polys;
        tile.owner = owner;
        tile.contours = contours;
        tile.regions = regions.count;
    }

    /// The volumes painting tile `t`, in paint order.
    fn volumes_on(&self, t: u32) -> Vec<NavAreaVolume> {
        self.area_volumes
            .values()
            .filter(|e| e.touched.contains(&t))
            .map(|e| e.volume)
            .collect()
    }

    /// Concatenates the tiles' polygons, links polygons across tile borders and rebuilds the
    /// mesh's spatial index (linear in polygons plus border cells; no tile is re-polygonised).
    /// Every tile's cells are copied into the mesh grid.
    fn stitch(&mut self) {
        let all: Vec<u32> = (0..self.tiles.len() as u32).collect();
        self.stitch_tiles(&all);
    }

    /// [`Self::stitch`] copying only `dirty` tiles' cells when the mesh grid still has this
    /// cache's layout. `dirty` is tile indices.
    fn stitch_tiles(&mut self, dirty: &[u32]) {
        let _span = tracing::info_span!("nav_tile_cache_stitch").entered();
        let total: usize = self.tiles.iter().map(|t| t.polys.len()).sum();
        let mut polys = Vec::with_capacity(total);
        let mut offsets = Vec::with_capacity(self.tiles.len());
        let mut contours = Vec::new();
        let mut region_base = 0u32;
        for tile in &self.tiles {
            let offset = polys.len() as u32;
            offsets.push(offset);
            for poly in &tile.polys {
                let mut p = poly.clone();
                for portal in &mut p.portals {
                    portal.neighbor += offset;
                }
                polys.push(p);
            }
            for contour in &tile.contours {
                let mut c = contour.clone();
                c.region += region_base;
                contours.push(c);
            }
            region_base += tile.regions;
        }
        let tx_n = self.tiles_x as usize;
        let tz_n = self.tiles_z as usize;
        for tz in 0..tz_n {
            for tx in 0..tx_n {
                let i = tz * tx_n + tx;
                let rect = self.tiles[i].rect;
                // The +X neighbour: its first column touches this tile's last column.
                if tx + 1 < tx_n {
                    let j = i + 1;
                    let other = self.tiles[j].rect;
                    let x = rect.x1 - 1;
                    for z in rect.z0..rect.z1 {
                        let a = self.tiles[i].owner[rect.local(x, z)];
                        let b = self.tiles[j].owner[other.local(rect.x1, z)];
                        if a == u32::MAX || b == u32::MAX {
                            continue;
                        }
                        if let Some((pa, pb)) = cell_edge_portal(&self.work, &self.params.build, x, z, true) {
                            add_portal(&mut polys, offsets[i] + a, offsets[j] + b, pa, pb);
                        }
                    }
                }
                // The +Z neighbour.
                if tz + 1 < tz_n {
                    let j = i + tx_n;
                    let other = self.tiles[j].rect;
                    let z = rect.z1 - 1;
                    for x in rect.x0..rect.x1 {
                        let a = self.tiles[i].owner[rect.local(x, z)];
                        let b = self.tiles[j].owner[other.local(x, rect.z1)];
                        if a == u32::MAX || b == u32::MAX {
                            continue;
                        }
                        if let Some((pa, pb)) = cell_edge_portal(&self.work, &self.params.build, x, z, false)
                        {
                            add_portal(&mut polys, offsets[i] + a, offsets[j] + b, pa, pb);
                        }
                    }
                }
            }
        }
        let region_count = connected_components(&polys);
        self.cells_copied_last = copy_dirty_cells(&mut self.mesh.base_grid, &self.work, &self.tiles, dirty);
        let grid = take_walkable_grid(&mut self.mesh.base_grid);
        let mut mesh = NavMesh::from_parts(
            polys,
            grid,
            self.params.build.clone(),
            region_count,
            contours,
            Vec::new(),
        );
        mesh.set_area_costs(self.area_costs);
        self.mesh = mesh;
        self.apply_links();
    }

    /// Resolves the mesh's links once: the cache's own links, then each loaded tile's streamed
    /// links in tile order (ADR-2589), with the obstacles blocking the ends they cover and the
    /// unloaded tiles leaving theirs unresolved.
    fn apply_links(&mut self) {
        let unloaded = self.stream.is_some().then(|| UnloadedTiles {
            origin_x: self.base.origin.x,
            origin_z: self.base.origin.z,
            cell_size: self.base.cell_size,
            cells: (self.base.width, self.base.height),
            tile_size: self.params.tile_size as usize,
            tiles_x: self.tiles_x as usize,
            missing: (0..self.tiles.len() as u32)
                .map(|t| !self.tile_is_loaded(t))
                .collect(),
        });
        // One resolve: the list goes in first, the blockers' setter resolves the table.
        self.mesh.off_mesh_links = self.mesh_links();
        self.mesh.set_link_blockers(LinkBlockers {
            shapes: self.obstacles.values().map(|e| e.shape).collect(),
            inflate: self.params.build.agent.radius,
            climb: self.params.build.agent.max_climb,
            unloaded,
        });
    }

    /// The links the mesh carries, in mesh order: [`Self::off_mesh_links`], then each loaded
    /// tile's streamed links in tile order.
    fn mesh_links(&self) -> Vec<OffMeshLink> {
        let mut links = self.off_mesh_links.clone();
        if let Some(stream) = &self.stream {
            links.extend(stream.tile_links.values().flatten().copied());
        }
        links
    }

    /// The tile owning `link`: the one under its `from` point (ADR-2589), or `None` off the grid.
    fn link_owner(&self, link: &OffMeshLink) -> Option<u32> {
        let (tx, tz) = self.tile_at(link.from)?;
        self.tile_index(tx, tz).map(|t| t as u32)
    }
}

/// A copy of the cells inside `rect` as a standalone grid (origin moved to the rect's corner).
fn grid_layout_matches(a: &WalkableGrid, b: &WalkableGrid) -> bool {
    a.origin.x.to_bits() == b.origin.x.to_bits()
        && a.origin.y.to_bits() == b.origin.y.to_bits()
        && a.origin.z.to_bits() == b.origin.z.to_bits()
        && a.cell_size.to_bits() == b.cell_size.to_bits()
        && a.width == b.width
        && a.height == b.height
        && a.cells.len() == b.cells.len()
        && a.areas.len() == b.areas.len()
}

/// Copies `dirty` tile rectangles from `src` into `dest` when the two grids share a layout.
/// Otherwise replaces `dest` with a clone of `src`. Returns how many cells were written.
fn copy_dirty_cells(dest: &mut WalkableGrid, src: &WalkableGrid, tiles: &[Tile], dirty: &[u32]) -> usize {
    if !grid_layout_matches(dest, src) {
        let n = src.cells.len();
        *dest = src.clone();
        return n;
    }
    let mut copied = 0usize;
    let width = dest.width;
    for &t in dirty {
        let rect = tiles[t as usize].rect;
        for z in rect.z0..rect.z1 {
            for x in rect.x0..rect.x1 {
                let i = z * width + x;
                dest.cells[i] = src.cells[i];
                dest.areas[i] = src.areas[i];
                copied += 1;
            }
        }
    }
    copied
}

fn take_walkable_grid(grid: &mut WalkableGrid) -> WalkableGrid {
    std::mem::replace(
        grid,
        WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: 0,
            height: 0,
            cells: Vec::new(),
            areas: Vec::new(),
        },
    )
}

/// Pre-rebuild polygon ranges of `rebuilt` tiles, and the index shifts for tiles after a count change.
fn polygon_shift(
    old_counts: &[u32],
    tiles: &[Tile],
    rebuilt: &[u32],
) -> (Vec<std::ops::Range<u32>>, Vec<(u32, i32)>) {
    let mut replaced = Vec::new();
    let mut shifts = Vec::new();
    let mut old_off = 0u32;
    for (i, tile) in tiles.iter().enumerate() {
        let old_n = old_counts.get(i).copied().unwrap_or(0);
        let new_n = tile.polys.len() as u32;
        if rebuilt.contains(&(i as u32)) {
            if old_n > 0 {
                replaced.push(old_off..old_off + old_n);
            }
            let delta = new_n as i32 - old_n as i32;
            if delta != 0 {
                shifts.push((old_off + old_n, delta));
            }
        }
        old_off += old_n;
    }
    (replaced, shifts)
}

fn sub_grid(grid: &WalkableGrid, rect: CellRect) -> WalkableGrid {
    let width = rect.width();
    let height = rect.z1 - rect.z0;
    let mut cells = Vec::with_capacity(width * height);
    for z in rect.z0..rect.z1 {
        cells.extend_from_slice(&grid.cells[z * grid.width + rect.x0..z * grid.width + rect.x1]);
    }
    WalkableGrid {
        origin: Vec3::new(
            grid.origin.x + rect.x0 as f32 * grid.cell_size,
            grid.origin.y,
            grid.origin.z + rect.z0 as f32 * grid.cell_size,
        ),
        cell_size: grid.cell_size,
        width,
        height,
        cells,
        areas: Vec::new(),
    }
}

/// The number of connected components of the polygon graph (portals only), i.e. the walkable
/// regions of the stitched mesh.
pub(crate) fn connected_components(polys: &[NavPoly]) -> u32 {
    let mut parent: Vec<u32> = (0..polys.len() as u32).collect();
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let up = parent[parent[x as usize] as usize];
            parent[x as usize] = up;
            x = up;
        }
        x
    }
    for (i, poly) in polys.iter().enumerate() {
        for portal in &poly.portals {
            let a = find(&mut parent, i as u32);
            let b = find(&mut parent, portal.neighbor);
            if a != b {
                parent[a.max(b) as usize] = a.min(b);
            }
        }
    }
    (0..polys.len() as u32)
        .filter(|&i| find(&mut parent, i) == i)
        .count() as u32
}

/// What the streamed cell decoder checks on every link: [`OffMeshLink::validate`] plus an area
/// below 64. A link set in code passes the same check, so the cooker never writes one the loader
/// rejects.
fn check_link(link: &OffMeshLink) -> Result<(), &'static str> {
    link.check()?;
    if link.area as usize >= MAX_NAV_AREAS {
        return Err("area must be below 64");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M5 (audit sprint-audit-g-nav-determinism): an oriented box's cell test must use the
    /// software trig. At this yaw the kernel's cos is one ulp below glibc's (`0x3f7ffda2`), and
    /// the cell centre sits exactly on the kernel's `u` boundary, so `blocks` accepts with the
    /// kernel and would reject under platform libm (the boundary comparison is inclusive).
    #[test]
    fn oriented_box_boundary_uses_the_software_trig() {
        let yaw = f32::from_bits(0x3c0b_32b8);
        let (sin, cos) = det_sin_cos(yaw);
        assert_eq!(cos.to_bits(), 0x3f7f_fda2);
        // The cell centre one metre out on the local +X axis: u = cos*cos + (-sin)*(-sin).
        let u = cos * cos + (-sin) * (-sin);
        assert_eq!(u.to_bits(), 0x3f7f_ffff);
        let shape = NavObstacleShape::OrientedBox {
            center: Vec3::new(0.0, 1.0, 0.0),
            half_extents: Vec3::new(u, 1.0, 1.0),
            yaw,
        };
        // On the boundary: blocked (the comparison is `u <= h.x`).
        assert!(shape.blocks(cos, 1.0, -sin, 0.0, 0.5));
        // The same centre one ulp inside the half extent: free.
        let inner = NavObstacleShape::OrientedBox {
            center: Vec3::new(0.0, 1.0, 0.0),
            half_extents: Vec3::new(f32::from_bits(0x3f7f_fffe), 1.0, 1.0),
            yaw,
        };
        assert!(!inner.blocks(cos, 1.0, -sin, 0.0, 0.5));

        // Yaw 0 is exact everywhere: the axis-aligned boundary holds bit for bit.
        let axis = NavObstacleShape::OrientedBox {
            center: Vec3::ZERO,
            half_extents: Vec3::ONE,
            yaw: 0.0,
        };
        assert!(axis.blocks(1.0, 0.0, 0.0, 0.0, 0.0));
        assert!(!axis.blocks(1.0 + f32::EPSILON, 0.0, 0.0, 0.0, 0.0));
    }

    /// A flat square floor `[-half, half]²` at `y = 0`.
    fn floor(half: f32) -> Vec<[Vec3; 3]> {
        vec![
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, -half),
                Vec3::new(half, 0.0, half),
            ],
            [
                Vec3::new(-half, 0.0, -half),
                Vec3::new(half, 0.0, half),
                Vec3::new(-half, 0.0, half),
            ],
        ]
    }

    fn params(tile_size: u32) -> NavTileCacheParams {
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size,
        }
    }

    #[test]
    fn a_tile_size_of_zero_or_too_large_is_a_typed_error() {
        for bad in [0, MAX_TILE_SIZE + 1] {
            let err = NavTileCache::build(params(bad), &floor(4.0)).expect_err("rejected");
            assert_eq!(err, NavTileCacheError::InvalidTileSize(bad));
            assert_eq!(err.code(), "nav_invalid_tile_size");
        }
    }

    #[test]
    fn builder_errors_pass_through_with_codes() {
        let err = NavTileCache::build(params(16), &[]).expect_err("no geometry");
        assert_eq!(err.code(), "nav_no_geometry");
        let mut tris = floor(4.0);
        tris[0][0].x = f32::NAN;
        let err = NavTileCache::build(params(16), &tris).expect_err("nan");
        assert_eq!(err.code(), "nav_invalid_build_input");
    }

    #[test]
    fn one_big_tile_matches_the_untiled_mesh() {
        let tris = floor(4.0);
        let untiled = NavMeshBuilder::new(params(1).build).build(&tris).expect("builds");
        let cache = NavTileCache::build(params(MAX_TILE_SIZE), &tris).expect("builds");
        assert_eq!(cache.tile_count(), 1);
        assert_eq!(cache.mesh().polys, untiled.polys, "same polygons, same portals");
        assert_eq!(cache.mesh().region_count, untiled.region_count);
        assert_eq!(cache.mesh().min, untiled.min);
        assert_eq!(cache.mesh().max, untiled.max);
    }

    #[test]
    fn tiles_cover_the_grid_and_stitch_into_one_region() {
        let cache = NavTileCache::build(params(8), &floor(4.0)).expect("builds");
        let (tx, tz) = cache.tile_grid();
        assert!(tx >= 4 && tz >= 4, "{tx}x{tz} tiles");
        assert_eq!(cache.mesh().region_count, 1, "cross-tile portals join every tile");
        // A path across several tiles exists and its waypoints stay on the mesh.
        let path = cache
            .query()
            .find_path(
                Vec3::new(-3.0, 0.0, -3.0),
                Vec3::new(3.0, 0.0, 3.0),
                Vec3::splat(1.0),
            )
            .expect("a route across tiles");
        assert!(path.polys.len() > 2, "the route crosses tiles: {:?}", path.polys);
        // An open floor: the string-pulled path is as long as the straight line (any corners
        // are tile corners lying on that line).
        let straight = (Vec3::new(3.0, 0.0, 3.0) - Vec3::new(-3.0, 0.0, -3.0)).length();
        assert!(
            (path.length - straight).abs() < 1e-3,
            "{} vs {straight}",
            path.length
        );
        crate::query::assert_waypoints_in_corridor(cache.mesh(), &path.polys, &path.waypoints);
        // Every tile was built once and none was rebuilt.
        assert_eq!(cache.total_rebuilds(), 0);
    }

    #[test]
    fn invalid_obstacles_are_typed_errors() {
        let mut cache = NavTileCache::build(params(8), &floor(4.0)).expect("builds");
        let bad = [
            NavObstacleShape::Cylinder {
                base: Vec3::ZERO,
                radius: 0.0,
                height: 1.0,
            },
            NavObstacleShape::Cylinder {
                base: Vec3::ZERO,
                radius: -1.0,
                height: 1.0,
            },
            NavObstacleShape::Cylinder {
                base: Vec3::ZERO,
                radius: 1.0,
                height: 0.0,
            },
            NavObstacleShape::Cylinder {
                base: Vec3::new(f32::NAN, 0.0, 0.0),
                radius: 1.0,
                height: 1.0,
            },
            NavObstacleShape::Cylinder {
                base: Vec3::ZERO,
                radius: f32::NAN,
                height: 1.0,
            },
            NavObstacleShape::Box {
                min: Vec3::ZERO,
                max: Vec3::new(0.0, 1.0, 1.0),
            },
            NavObstacleShape::Box {
                min: Vec3::ZERO,
                max: Vec3::new(f32::INFINITY, 1.0, 1.0),
            },
        ];
        for shape in bad {
            let err = cache.add_obstacle(shape).expect_err("rejected");
            assert!(matches!(err, NavTileCacheError::InvalidObstacle(_)), "{shape:?}");
            assert_eq!(err.code(), "nav_invalid_obstacle");
        }
        assert_eq!(cache.obstacle_count(), 0);
        assert!(
            cache.pending_tiles().is_empty(),
            "a rejected obstacle queues nothing"
        );
        let err = cache.remove_obstacle(NavObstacleId(7)).expect_err("unknown");
        assert_eq!(err, NavTileCacheError::UnknownObstacle(NavObstacleId(7)));
        assert_eq!(err.code(), "nav_unknown_obstacle");
    }

    #[test]
    fn only_tiles_overlapping_the_obstacle_are_rebuilt() {
        // 8 m floor, 0.25 m cells, 8-cell (2 m) tiles.
        let mut cache = NavTileCache::build(params(8), &floor(4.0)).expect("builds");
        let tile = cache.tile_at(Vec3::new(1.0, 0.0, 1.0)).expect("on the grid");
        let (tmin, tmax) = cache.tile_bounds(tile.0, tile.1).expect("tile");
        let centre = (tmin + tmax) * 0.5;
        // A small cylinder in the middle of one tile: the inflated footprint stays inside it.
        let id = cache
            .add_obstacle(NavObstacleShape::Cylinder {
                base: Vec3::new(centre.x, 0.0, centre.z),
                radius: 0.3,
                height: 2.0,
            })
            .expect("adds");
        assert_eq!(cache.touched_tiles(id), Some(vec![tile]));
        assert_eq!(cache.pending_tiles(), vec![tile]);
        let before = cache.mesh().poly_count();
        let update = cache.update();
        assert_eq!(update.rebuilt, vec![tile], "exactly the one overlapped tile");
        assert_eq!(update.remaining, 0);
        assert_eq!(cache.total_rebuilds(), 1);
        let (tx_n, tz_n) = cache.tile_grid();
        for tz in 0..tz_n {
            for tx in 0..tx_n {
                let expected = u64::from((tx, tz) == tile);
                assert_eq!(
                    cache.tile_rebuild_count(tx, tz),
                    Some(expected),
                    "tile ({tx}, {tz})"
                );
            }
        }
        assert!(
            cache.mesh().poly_count() > before,
            "the hole splits the tile's polygon"
        );
        assert_eq!(cache.revision(), 1);

        // A box straddling the corner shared by four tiles rebuilds exactly those four.
        let corner = tmax;
        let id2 = cache
            .add_obstacle(NavObstacleShape::Box {
                min: Vec3::new(corner.x - 0.2, -0.5, corner.z - 0.2),
                max: Vec3::new(corner.x + 0.2, 1.5, corner.z + 0.2),
            })
            .expect("adds");
        let mut touched = cache.touched_tiles(id2).expect("known");
        touched.sort_unstable();
        let mut expected = vec![
            tile,
            (tile.0 + 1, tile.1),
            (tile.0, tile.1 + 1),
            (tile.0 + 1, tile.1 + 1),
        ];
        expected.sort_unstable();
        assert_eq!(touched, expected);
        let update = cache.update();
        assert_eq!(update.rebuilt.len(), 4);
        assert_eq!(cache.total_rebuilds(), 5);

        // Removing the cylinder rebuilds its single tile again.
        cache.remove_obstacle(id).expect("removes");
        assert_eq!(cache.update().rebuilt, vec![tile]);
        assert_eq!(cache.tile_rebuild_count(tile.0, tile.1), Some(3));
        assert_eq!(cache.total_rebuilds(), 6);
    }

    #[test]
    fn an_update_with_nothing_queued_rebuilds_nothing() {
        let mut cache = NavTileCache::build(params(8), &floor(4.0)).expect("builds");
        let update = cache.update();
        assert!(update.rebuilt.is_empty());
        assert_eq!(cache.revision(), 0);
        // An obstacle far outside the grid touches no tile.
        let id = cache
            .add_obstacle(NavObstacleShape::Cylinder {
                base: Vec3::new(100.0, 0.0, 100.0),
                radius: 1.0,
                height: 1.0,
            })
            .expect("adds");
        assert_eq!(cache.touched_tiles(id), Some(Vec::new()));
        assert!(cache.update().rebuilt.is_empty());
    }

    #[test]
    fn obstacle_add_changes_the_path_and_remove_restores_it() {
        // A 12 x 4 m corridor along X; a wall across it with a gap forces a detour.
        let mut cache = NavTileCache::build(params(8), &floor(6.0)).expect("builds");
        let from = Vec3::new(-5.0, 0.0, 0.0);
        let to = Vec3::new(5.0, 0.0, 0.0);
        let ext = Vec3::splat(1.0);
        let original = cache.query().find_path(from, to, ext).expect("open floor");
        assert_eq!(original.waypoints.len(), 2, "straight: {:?}", original.waypoints);

        let wall = cache
            .add_obstacle(NavObstacleShape::Box {
                min: Vec3::new(-0.25, -0.5, -6.5),
                max: Vec3::new(0.25, 2.0, 3.0),
            })
            .expect("adds");
        // Before the update the mesh is unchanged.
        assert_eq!(cache.query().find_path(from, to, ext), Some(original.clone()));
        let update = cache.update();
        assert!(!update.rebuilt.is_empty());
        assert!(
            update.rebuilt.len() < cache.tile_count(),
            "{} of {} tiles rebuilt",
            update.rebuilt.len(),
            cache.tile_count()
        );
        let detour = cache
            .query()
            .find_path(from, to, ext)
            .expect("a route through the gap");
        assert!(
            detour.waypoints.len() >= 3,
            "bends around the wall: {:?}",
            detour.waypoints
        );
        assert!(detour.length > original.length + 1.0);
        // The detour passes through the gap (z > 3 + radius) where it crosses x = 0.
        assert!(
            detour.waypoints.iter().any(|w| w.z > 3.0),
            "through the gap: {:?}",
            detour.waypoints
        );
        crate::query::assert_waypoints_in_corridor(cache.mesh(), &detour.polys, &detour.waypoints);

        // A cylinder plugging the gap blocks every route.
        let plug = cache
            .add_obstacle(NavObstacleShape::Cylinder {
                base: Vec3::new(0.0, 0.0, 4.75),
                radius: 1.8,
                height: 2.0,
            })
            .expect("adds");
        cache.update();
        assert_eq!(cache.query().find_path(from, to, ext), None, "fully blocked");
        assert_eq!(cache.mesh().region_count, 2, "the wall splits the floor in two");

        cache.remove_obstacle(plug).expect("removes");
        cache.remove_obstacle(wall).expect("removes");
        cache.update();
        let restored = cache.query().find_path(from, to, ext).expect("reopened");
        assert_eq!(
            restored.waypoints, original.waypoints,
            "the original path is restored"
        );
        assert_eq!(restored.length, original.length);
        assert_eq!(cache.mesh().polys.len(), {
            let fresh = NavTileCache::build(params(8), &floor(6.0)).expect("builds");
            fresh.mesh().polys.len()
        });
    }

    #[test]
    fn a_restored_cache_is_identical_to_a_fresh_build() {
        let tris = floor(4.0);
        let fresh = NavTileCache::build(params(8), &tris).expect("builds");
        let mut cache = fresh.clone();
        let a = cache
            .add_obstacle(NavObstacleShape::Cylinder {
                base: Vec3::new(0.3, 0.0, -1.1),
                radius: 0.7,
                height: 1.0,
            })
            .expect("adds");
        let b = cache
            .add_obstacle(NavObstacleShape::Box {
                min: Vec3::new(1.0, -1.0, 1.0),
                max: Vec3::new(2.5, 1.0, 1.6),
            })
            .expect("adds");
        cache.update();
        assert_ne!(cache.mesh().polys, fresh.mesh().polys);
        cache.remove_obstacle(b).expect("removes");
        cache.remove_obstacle(a).expect("removes");
        cache.update();
        assert_eq!(cache.mesh().polys, fresh.mesh().polys);
        assert_eq!(cache.mesh().region_count, fresh.mesh().region_count);
    }

    #[test]
    fn a_limited_update_spreads_rebuilds_over_calls() {
        let mut cache = NavTileCache::build(params(4), &floor(4.0)).expect("builds");
        cache
            .add_obstacle(NavObstacleShape::Box {
                min: Vec3::new(-3.0, -1.0, -0.2),
                max: Vec3::new(3.0, 1.0, 0.2),
            })
            .expect("adds");
        let queued = cache.pending_tiles().len();
        assert!(queued > 2, "{queued}");
        let first = cache.update_limited(2);
        assert_eq!(first.rebuilt.len(), 2);
        assert_eq!(first.remaining, queued - 2);
        let rest = cache.update();
        assert_eq!(rest.rebuilt.len(), queued - 2);
        assert_eq!(rest.remaining, 0);
        assert_eq!(cache.revision(), 2);
    }

    #[test]
    fn an_obstacle_above_the_floor_does_not_block_it() {
        let mut cache = NavTileCache::build(params(8), &floor(4.0)).expect("builds");
        let before = cache.mesh().polys.clone();
        cache
            .add_obstacle(NavObstacleShape::Cylinder {
                base: Vec3::new(0.0, 3.0, 0.0),
                radius: 1.0,
                height: 1.0,
            })
            .expect("adds");
        let update = cache.update();
        assert!(!update.rebuilt.is_empty(), "touched tiles are still rebuilt");
        assert_eq!(
            cache.mesh().polys,
            before,
            "but nothing under the floating cylinder changes"
        );
    }

    /// A grid wider than 2^24 columns: the last column is still on the grid (`width as f32` rounds).
    #[test]
    fn tile_lookup_keeps_the_last_column_of_a_very_wide_grid() {
        let mut p = params(32);
        p.build.cell_size = 1.0;
        let layout = NavTileLayout {
            origin: Vec3::ZERO,
            width: 16_777_217,
            height: 1,
        };
        let cache = NavTileCache::streamed(p, layout).expect("streamed");
        assert!(cache.tile_at(Vec3::new(16_777_216.0, 0.0, 0.5)).is_some());
        assert!(cache.tile_at(Vec3::new(-0.5, 0.0, 0.5)).is_none());
    }

    #[test]
    fn tile_lookup_rejects_points_off_the_grid() {
        let cache = NavTileCache::build(params(8), &floor(4.0)).expect("builds");
        assert!(cache.tile_at(Vec3::new(0.0, 0.0, 0.0)).is_some());
        assert!(cache.tile_at(Vec3::new(500.0, 0.0, 0.0)).is_none());
        assert!(cache.tile_at(Vec3::new(f32::NAN, 0.0, 0.0)).is_none());
        let (tx, tz) = cache.tile_grid();
        assert!(cache.tile_bounds(tx, 0).is_none());
        assert!(cache.tile_rebuild_count(0, tz).is_none());
    }

    #[test]
    fn cell_span_clamps_in_integers_above_f32_precision() {
        // 16_777_248 cells: `n as f32 - 1.0` rounds back to `n`, one past the last cell.
        let n = 16_777_248usize;
        let span = NavTileCache::cell_span(0.0, 4_194_312.0, 0.0, 0.25, n);
        assert_eq!(span, Some((0, n - 1)));
        assert_eq!(NavTileCache::cell_span(0.0, 1.0, 0.0, 0.25, 0), None);
    }
}
