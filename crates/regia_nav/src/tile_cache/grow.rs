// SPDX-License-Identifier: MPL-2.0
//! Grid growth (ADR-2589): a cache built whole grows its cell grid by whole tiles when level
//! geometry is added past its edge, so the new ground gets navmesh without a full rebuild.
//!
//! Growth keeps every existing tile's cells, polygons, rebuild count and identity: toward +X and
//! +Z the tile coordinates `(tile_x, tile_z)` of the existing tiles do not change; toward -X or
//! -Z the grid origin moves back by whole tiles and every existing tile's coordinates move up by
//! [`NavGridGrowth::tile_offset`] (the tile is the same, its polygons stay where they are in the
//! world). Only the tiles whose cell rectangle changed are re-polygonised: the new (empty) tiles
//! and the old partial tiles on a far edge that the growth completes. The new cells are not
//! walkable until [`NavTileCache::rebuild_region`] voxelises geometry there
//! ([`NavTileCache::rebuild_region_growing`] does both).

use std::collections::BTreeSet;

use regia_core::math::Vec3;

use super::stream::MAX_STREAMED_GRID_CELLS;
use super::{NavTileCache, NavTileCacheError, Tile};
use crate::heightfield::WalkableGrid;
use crate::mesh::CellRect;
use crate::tile_cache::NavRegionRebuild;

/// What one [`NavTileCache::grow_to`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavGridGrowth {
    /// The grid already covered the box: nothing changed.
    pub unchanged: bool,
    /// Tile columns added on the -X side, the +X side, rows on the -Z side and the +Z side.
    pub added: [u32; 4],
    /// How far every existing tile's `(tile_x, tile_z)` moved: `(0, 0)` unless tiles were added
    /// on the -X or -Z side.
    pub tile_offset: (u32, u32),
    /// The tile grid before and after.
    pub tiles_before: (u32, u32),
    /// The tile grid after.
    pub tiles_after: (u32, u32),
    /// The tiles re-polygonised (new coordinates): the new tiles and the completed edge tiles.
    pub rebuilt: Vec<(u32, u32)>,
}

impl NavTileCache {
    /// Grows the grid by whole tiles so it covers the XZ box `min..max` (see the module docs).
    /// The mesh is restitched at once and [`Self::revision`] moves when anything changed.
    /// Errors: a streamed cache (its layout is the cook's), a non-finite box, a grid that
    /// would pass `MAX_STREAMED_GRID_CELLS` cells, or a -X/-Z cell shift that is not exactly
    /// representable as f32 (ADR-5007). Every error is `nav_grid_growth` and changes nothing.
    pub fn grow_to(&mut self, min: Vec3, max: Vec3) -> Result<NavGridGrowth, NavTileCacheError> {
        let _span = tracing::info_span!("nav_tile_cache_grow").entered();
        if self.stream.is_some() {
            return Err(NavTileCacheError::GridGrowth(
                "a streamed cache keeps the layout it was cooked with",
            ));
        }
        if !min.is_finite() || !max.is_finite() {
            return Err(NavTileCacheError::GridGrowth("box corners must be finite"));
        }
        let (lo, hi) = (min.min(max), min.max(max));
        let g = &self.base;
        let c = g.cell_size;
        let ts = self.params.tile_size as usize;
        let before = (self.tiles_x, self.tiles_z);
        // Cell coordinates of the box's first and last cells, relative to the current origin.
        let cell = |v: f32, o: f32| ((v - o) / c).floor() as f64;
        let (cx0, cx1) = (cell(lo.x, g.origin.x), cell(hi.x, g.origin.x));
        let (cz0, cz1) = (cell(lo.z, g.origin.z), cell(hi.z, g.origin.z));
        let tiles_for = |cells: f64| (cells / ts as f64).ceil().max(0.0);
        let west = tiles_for(-cx0);
        let south = tiles_for(-cz0);
        let past = |last: f64, n: usize| (last + 1.0 - n as f64).max(0.0);
        // Whole tiles past the last full tile boundary: a partial edge tile is completed first.
        let east_cells = past(cx1, g.width);
        let north_cells = past(cz1, g.height);
        let full = |n: usize| n.div_ceil(ts) * ts;
        let east = tiles_for(east_cells - (full(g.width) - g.width) as f64);
        let north = tiles_for(north_cells - (full(g.height) - g.height) as f64);
        if west == 0.0 && south == 0.0 && east_cells == 0.0 && north_cells == 0.0 {
            return Ok(NavGridGrowth {
                unchanged: true,
                tiles_before: before,
                tiles_after: before,
                ..Default::default()
            });
        }
        let limit = MAX_STREAMED_GRID_CELLS as f64;
        if [west, south, east, north].iter().any(|&t| t * ts as f64 > limit) {
            return Err(NavTileCacheError::GridGrowth("the grid would pass 64 Mi cells"));
        }
        let (west, south, east, north) = (west as usize, south as usize, east as usize, north as usize);
        let new_w = west * ts
            + if east_cells > 0.0 {
                full(g.width) + east * ts
            } else {
                g.width
            };
        let new_h = south * ts
            + if north_cells > 0.0 {
                full(g.height) + north * ts
            } else {
                g.height
            };
        if new_w
            .checked_mul(new_h)
            .is_none_or(|n| n > MAX_STREAMED_GRID_CELLS)
        {
            return Err(NavTileCacheError::GridGrowth("the grid would pass 64 Mi cells"));
        }
        let (dx, dz) = (west * ts, south * ts);
        // The cells move by the integer `(dx, dz)`, but the origin is an f32. Past 2^24 a cell
        // count rounds, and `tile_at` then puts the old floor on an empty tile (ADR-5007).
        if !cell_shift_fits_f32(dx) || !cell_shift_fits_f32(dz) {
            return Err(NavTileCacheError::GridGrowth(
                "the cell shift is not exactly representable as f32",
            ));
        }
        let origin = Vec3::new(g.origin.x - dx as f32 * c, g.origin.y, g.origin.z - dz as f32 * c);
        self.base = moved_grid(&self.base, origin, new_w, new_h, dx, dz);
        self.work = moved_grid(&self.work, origin, new_w, new_h, dx, dz);

        // Lay the tiles out again; a tile whose rectangle (moved by the offset) is unchanged keeps
        // its polygons, owner map, contours and rebuild count.
        let (old_tx, old_tz) = (self.tiles_x as usize, self.tiles_z as usize);
        let mut old: Vec<Option<Tile>> = std::mem::take(&mut self.tiles).into_iter().map(Some).collect();
        let tiles_x = new_w.div_ceil(ts);
        let tiles_z = new_h.div_ceil(ts);
        let mut rebuild = Vec::new();
        for tz in 0..tiles_z {
            for tx in 0..tiles_x {
                let rect = CellRect {
                    x0: tx * ts,
                    z0: tz * ts,
                    x1: ((tx + 1) * ts).min(new_w),
                    z1: ((tz + 1) * ts).min(new_h),
                };
                let prior = if tx >= west && tz >= south && tx - west < old_tx && tz - south < old_tz {
                    old[(tz - south) * old_tx + (tx - west)].take()
                } else {
                    None
                };
                let t = (tz * tiles_x + tx) as u32;
                let moved = |r: CellRect| CellRect {
                    x0: r.x0 + dx,
                    z0: r.z0 + dz,
                    x1: r.x1 + dx,
                    z1: r.z1 + dz,
                };
                match prior {
                    Some(tile) if moved(tile.rect) == rect => self.tiles.push(Tile { rect, ..tile }),
                    prior => {
                        self.tiles.push(Tile {
                            rect,
                            polys: Vec::new(),
                            owner: Vec::new(),
                            contours: Vec::new(),
                            regions: 0,
                            rebuilds: prior.map_or(0, |p| p.rebuilds),
                        });
                        rebuild.push(t);
                    }
                }
            }
        }
        // Queued tiles keep their place in the new numbering.
        let remap = |t: u32| {
            let (x, z) = (t as usize % old_tx, t as usize / old_tx);
            ((z + south) * tiles_x + x + west) as u32
        };
        self.pending = self.pending.iter().copied().map(remap).collect::<BTreeSet<u32>>();
        self.tiles_x = tiles_x as u32;
        self.tiles_z = tiles_z as u32;
        let reach = self.params.build.agent.radius;
        let obstacle_tiles: Vec<(u64, Vec<u32>)> = self
            .obstacles
            .iter()
            .map(|(&id, e)| (id, self.tiles_under(&e.shape, reach)))
            .collect();
        for (id, touched) in obstacle_tiles {
            if let Some(e) = self.obstacles.get_mut(&id) {
                e.touched = touched;
            }
        }
        let volume_tiles: Vec<(u64, Vec<u32>)> = self
            .area_volumes
            .iter()
            .map(|(&id, e)| (id, self.tiles_under(&e.volume.shape, 0.0)))
            .collect();
        for (id, touched) in volume_tiles {
            if let Some(e) = self.area_volumes.get_mut(&id) {
                e.touched = touched;
            }
        }
        for &t in &rebuild {
            self.rebuild_tile(t);
        }
        self.stitch();
        self.revision += 1;
        // The tiles were renumbered and restitched: a patch still waiting describes the old
        // numbering, so the next crowd sync must replan everything instead of applying it.
        self.pending_revision = None;
        Ok(NavGridGrowth {
            unchanged: false,
            added: [west as u32, east as u32, south as u32, north as u32],
            tile_offset: (west as u32, south as u32),
            tiles_before: before,
            tiles_after: (self.tiles_x, self.tiles_z),
            rebuilt: rebuild.iter().map(|&t| self.tile_xz(t)).collect(),
        })
    }

    /// [`Self::grow_to`] the box, then [`Self::rebuild_region`] it from `triangles`: new level
    /// geometry past the grid's edge gets navmesh. Errors: those of both calls (a failed growth
    /// changes nothing).
    pub fn rebuild_region_growing(
        &mut self,
        min: Vec3,
        max: Vec3,
        triangles: &[[Vec3; 3]],
    ) -> Result<(NavGridGrowth, NavRegionRebuild), NavTileCacheError> {
        if !triangles.iter().flatten().all(|v| v.is_finite()) {
            return Err(NavTileCacheError::InvalidRegion(
                "geometry contains non-finite vertices",
            ));
        }
        let growth = self.grow_to(min, max)?;
        let rebuild = self.rebuild_region(min, max, triangles)?;
        Ok((growth, rebuild))
    }
}

/// `true` when `cells` round-trips through f32. Past 2^24 not every integer does.
fn cell_shift_fits_f32(cells: usize) -> bool {
    let shifted = cells as f32;
    shifted.is_finite() && shifted as usize == cells
}

/// `grid` copied into a `width` x `height` grid at `origin`, its cells moved by `(dx, dz)`.
fn moved_grid(
    grid: &WalkableGrid,
    origin: Vec3,
    width: usize,
    height: usize,
    dx: usize,
    dz: usize,
) -> WalkableGrid {
    let mut cells = vec![None; width * height];
    let mut areas = vec![0u8; width * height];
    for z in 0..grid.height {
        let from = z * grid.width;
        let to = (z + dz) * width + dx;
        cells[to..to + grid.width].copy_from_slice(&grid.cells[from..from + grid.width]);
        for x in 0..grid.width {
            areas[to + x] = grid.area_at(from + x);
        }
    }
    WalkableGrid {
        origin,
        cell_size: grid.cell_size,
        width,
        height,
        cells,
        areas,
    }
}
