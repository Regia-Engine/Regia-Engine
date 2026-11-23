// SPDX-License-Identifier: MPL-2.0
//! Region re-voxelize (ADR-1934): new level geometry replaces the stored walkable cells under a
//! world box, and only the tiles whose cells changed are queued.
//!
//! A cell's walkable state depends on its own column and on the raw surface within the erosion
//! reach `r` (`ceil(radius / cell_size)` cells, Manhattan distance). So for a dirty rectangle `D`:
//! the cells that can change are `A = D + r`, and judging them takes the raw surface of
//! `W = A + r + 1`. The window is rasterised on the cache's own grid, eroded, and `A` is copied
//! into the stored grid. The extra row keeps the window's own edge (seeded like a grid border by
//! the erosion flood) out of reach of `A`, so `A` comes out as a full build would make it, and a
//! window clipped by the grid keeps the real border's erosion.

use regia_core::math::Vec3;

use super::{NavTileCache, NavTileCacheError};
use crate::heightfield::{HeightField, erode_walkable};
use crate::mesh::CellRect;

/// What one [`NavTileCache::rebuild_region`] call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavRegionRebuild {
    /// The tiles queued for rebuilding, `(tile_x, tile_z)`, in rebuild order: the tiles holding a
    /// changed cell. [`NavTileCache::update`] rebuilds them.
    pub queued: Vec<(u32, u32)>,
    /// Stored cells whose walkable state or height changed.
    pub cells_changed: usize,
    /// The box lies wholly outside the grid; nothing was done.
    pub outside: bool,
    /// Tiles under the box that are not loaded (a streamed cache): their cells are left alone and
    /// the box is replayed when they load.
    pub deferred: Vec<(u32, u32)>,
}

/// The rectangles of one dirty box.
struct RegionRects {
    /// The cells that may change.
    apply: CellRect,
    /// The cells to rasterise.
    window: CellRect,
}

impl NavTileCache {
    /// The cell reach of agent-radius erosion.
    fn erosion_reach(&self) -> usize {
        let build = &self.params.build;
        let cells = (build.agent.radius / build.cell_size).max(0.0).ceil();
        // Reach beyond the grid changes nothing; the cap keeps `2 * r + 1` from overflowing.
        (cells as usize).min(self.base.width.max(self.base.height))
    }

    /// The dirty cells of a box (one extra cell each way: a triangle edge inside the box can move
    /// the cell centres next to it) grown to the apply and window rectangles, or `None` outside
    /// the grid.
    fn region_rects(&self, lo: Vec3, hi: Vec3) -> Option<RegionRects> {
        let g = &self.base;
        let c = g.cell_size;
        let (dx0, dx1) = Self::cell_span(lo.x - c, hi.x + c, g.origin.x, c, g.width)?;
        let (dz0, dz1) = Self::cell_span(lo.z - c, hi.z + c, g.origin.z, c, g.height)?;
        let grown = |by: usize| CellRect {
            x0: dx0.saturating_sub(by),
            z0: dz0.saturating_sub(by),
            x1: (dx1 + 1 + by).min(g.width),
            z1: (dz1 + 1 + by).min(g.height),
        };
        let r = self.erosion_reach();
        Some(RegionRects {
            apply: grown(r),
            window: grown(2 * r + 1),
        })
    }

    /// The world XZ box (`min`, `max`, y spanning every height) of `rect`.
    fn rect_bounds(&self, rect: CellRect) -> (Vec3, Vec3) {
        let g = &self.base;
        let c = g.cell_size;
        (
            Vec3::new(
                g.origin.x + rect.x0 as f32 * c,
                f32::NEG_INFINITY,
                g.origin.z + rect.z0 as f32 * c,
            ),
            Vec3::new(
                g.origin.x + rect.x1 as f32 * c,
                f32::INFINITY,
                g.origin.z + rect.z1 as f32 * c,
            ),
        )
    }

    /// The world XZ box that [`Self::rebuild_region`] of `min..max` rasterises (y is unbounded), or
    /// `None` when the box misses the grid or is not finite. Hand
    /// [`Self::rebuild_region`] every level triangle touching it: it holds more than the box
    /// because erosion looks past the box.
    pub fn region_window(&self, min: Vec3, max: Vec3) -> Option<(Vec3, Vec3)> {
        if !min.is_finite() || !max.is_finite() {
            return None;
        }
        let rects = self.region_rects(min.min(max), min.max(max))?;
        Some(self.rect_bounds(rects.window))
    }

    /// Re-voxelises the box `min..max` from `triangles` and queues the tiles whose stored cells
    /// changed; the mesh changes on the next [`Self::update`]. `triangles` must hold the current
    /// level geometry touching [`Self::region_window`]: anything missing there reads as a hole.
    /// Triangles outside the window are ignored, so the whole level is a valid input.
    ///
    /// The result inside the box equals a full [`Self::build`] from the same geometry on the same
    /// grid. The grid itself does not grow here: geometry past its edge is not seen (grow it first
    /// with [`Self::grow_to`], or call [`Self::rebuild_region_growing`]). Obstacles stay.
    /// Corners may come in any order. Errors: a non-finite box or vertex (nothing changes).
    pub fn rebuild_region(
        &mut self,
        min: Vec3,
        max: Vec3,
        triangles: &[[Vec3; 3]],
    ) -> Result<NavRegionRebuild, NavTileCacheError> {
        let _span =
            tracing::info_span!("nav_tile_cache_rebuild_region", triangles = triangles.len()).entered();
        if !min.is_finite() || !max.is_finite() {
            return Err(NavTileCacheError::InvalidRegion("box corners must be finite"));
        }
        if !triangles.iter().flatten().all(|v| v.is_finite()) {
            return Err(NavTileCacheError::InvalidRegion(
                "geometry contains non-finite vertices",
            ));
        }
        let (lo, hi) = (min.min(max), min.max(max));
        let Some(rects) = self.region_rects(lo, hi) else {
            return Ok(NavRegionRebuild {
                outside: true,
                ..Default::default()
            });
        };
        let cells = self.voxelize_window(rects.window, triangles);

        let ts = self.params.tile_size as usize;
        let width = self.base.width;
        let mut queued = std::collections::BTreeSet::new();
        let mut deferred = std::collections::BTreeSet::new();
        let mut cells_changed = 0;
        for z in rects.apply.z0..rects.apply.z1 {
            for x in rects.apply.x0..rects.apply.x1 {
                let t = ((z / ts) * self.tiles_x as usize + x / ts) as u32;
                if !self.tile_is_loaded(t) {
                    deferred.insert(t);
                    continue;
                }
                let new = cells[rects.window.local(x, z)];
                let old = &mut self.base.cells[z * width + x];
                if *old != new {
                    *old = new;
                    cells_changed += 1;
                    queued.insert(t);
                }
            }
        }
        self.pending.extend(queued.iter().copied());
        self.note_region_applied(lo, hi, rects.apply, ts);
        Ok(NavRegionRebuild {
            queued: queued.iter().map(|&t| self.tile_xz(t)).collect(),
            cells_changed,
            outside: false,
            deferred: deferred.iter().map(|&t| self.tile_xz(t)).collect(),
        })
    }

    /// The walkable cells of `window` from `triangles`. Cells of tiles that are not loaded read as
    /// walkable: their geometry may be gone with them, and a missing neighbour must not erode the
    /// loaded cells beside it. Their box is replayed when they load.
    fn voxelize_window(&self, window: CellRect, triangles: &[[Vec3; 3]]) -> Vec<Option<f32>> {
        let build = &self.params.build;
        let field = HeightField::rasterize_window(
            triangles,
            self.base.origin,
            build.cell_size,
            build.cell_height,
            window,
        );
        let mut cells = field.walkable_tops(&build.agent);
        field.drop_partial_tops(&build.agent, build.agent.radius, &mut cells, &mut []);
        if self.stream.is_some() {
            let ts = self.params.tile_size as usize;
            for z in window.z0..window.z1 {
                for x in window.x0..window.x1 {
                    let t = ((z / ts) * self.tiles_x as usize + x / ts) as u32;
                    if !self.tile_is_loaded(t) {
                        cells[window.local(x, z)] = Some(0.0);
                    }
                }
            }
        }
        erode_walkable(
            &mut cells,
            field.width,
            field.height,
            build.cell_size,
            build.agent.radius,
        );
        cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile_cache::{NavTileCacheParams, NavTileLayout};

    #[test]
    fn erosion_reach_is_capped_to_the_grid() {
        let mut params = NavTileCacheParams::default();
        params.build.agent.radius = 1e20;
        let layout = NavTileLayout {
            origin: Vec3::ZERO,
            width: 64,
            height: 64,
        };
        let cache = NavTileCache::streamed(params, layout).expect("streamed cache");
        assert_eq!(cache.erosion_reach(), 64);
        let rects = cache.region_rects(Vec3::ZERO, Vec3::new(0.01, 0.0, 0.01));
        let rects = rects.expect("box in the grid");
        assert!(rects.apply.x1 >= 1 && rects.apply.z1 >= 1);
    }
}
