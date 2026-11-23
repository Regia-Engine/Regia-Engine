// SPDX-License-Identifier: MPL-2.0
//! The heightfield's [`SunCover`] provider (v0.7 item E, ADR-8874; KI-RN-004).
//!
//! Sun sealing (ADR-8414, `regia_render::viscells::sun`) takes the draws of a cell the sun cannot
//! reach out of the sun's cascades, but only for cells whose host vouches that they lie under cover
//! that stays in the cascades. A buried tunnel's cover is the terrain over it. With
//! [`TerrainSunCover`] inserted, [`update_terrain_sun_cover`] fills the [`SunCover`] resource
//! itself, from every [`Terrain`]'s min-max pyramid and holes, for the frame's sun:
//!
//! - A cell is covered when every one of its boxes is ([`box_covered`]): the box's footprint swept
//!   toward the sun until the ray from its bottom has risen to the terrain's highest point, and the
//!   lowest the terrain is drawn over that sweep lies above the box's top.
//! - "The lowest the terrain is drawn" ([`terrain_min_cover`]) is the lower of two bounds. The
//!   pyramid's minimum over the swept rectangle grown by [`TerrainSunCover::grow_m`] bounds every
//!   shadow level whose vertices read samples no further than that growth. A coarser level (by
//!   default the root and the level below it, whose box-filtered vertices read samples up to
//!   about three root cells away) is bounded by the mesh it draws over the sweep: the lowest
//!   filtered read of a vertex of a triangle that can overlap it, over its whole morph
//!   (ADR-10161). So no drawn shadow triangle over the sweep dips below the minimum the cover
//!   trusts; the default growth stays two cells of the coarsest level a shadow pass draws
//!   ([`coarsest_shadow_cell_m`], the root's).
//! - A hole (the [`Terrain::holes`] mask, or an open mesh-layer mouth's lattice cut,
//!   [`TerrainMeshLayers::lattice_hole_cut`]) anywhere over the grown sweep, or a sweep that leaves
//!   the map, is a gap: not covered.
//!
//! The cover is recomputed only when the sun moves (the installed cover no longer matches it),
//! the cells or a terrain change, or the settings change. Without [`TerrainSunCover`] the host's
//! [`SunCover`] is left alone.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::ExtractedFrame;
use regia_render::viscells::{CellBox, SUN_SEAL_SLACK_M, SunCover, VisCells, box_covered};

use crate::cdlod::{QuadNode, node_world_size};
use crate::height_mips::{level_dims, mip_level_count, parent_taps, vertex_mip};
use crate::holes::LatticeHoleCut;
use crate::sampler::TerrainMap;
use crate::{Terrain, TerrainMeshLayers};

/// Most pyramid nodes one [`terrain_min_cover`] reads along a side: it steps up a level past this
/// (a coarser node's minimum is lower or equal, so the answer only gets more careful).
const MAX_NODES_PER_SIDE: u32 = 16;

/// Opt-in: the terrain fills [`SunCover`] (module docs). Agent:
/// `render.set_viscells { sun_cover: "terrain" }`.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
pub struct TerrainSunCover {
    /// How far the swept rectangle is grown before the pyramid is read, metres. `None` (the
    /// default): [`coarsest_shadow_cell_m`] of each terrain, twice. Any growth is safe: the
    /// shadow levels that read further are bounded by their drawn mesh (module docs); a smaller
    /// growth only moves more levels onto that slower bound.
    pub grow_m: Option<f32>,
}

/// What [`update_terrain_sun_cover`] did (agent `render.viscells` `sun_cover`).
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct TerrainSunCoverStats {
    /// Times the cover was computed.
    pub fills: u64,
    /// Indoor cells tested on the last fill.
    pub cells_tested: usize,
    /// Of those, cells the terrain covers.
    pub cells_covered: usize,
    /// The growth used on the last fill, metres (the largest over the terrains).
    pub grow_m: f32,
}

/// The cell of the coarsest level a terrain's shadow pass can draw, metres: the root node's
/// decimated grid ([`crate::gpu::SHADOW_GRID_SIZE`] cells a side).
#[must_use]
pub fn coarsest_shadow_cell_m(terrain: &Terrain) -> f32 {
    let world = terrain.map.quadtree_width();
    let grid = terrain.cdlod.grid_size.clamp(1, crate::gpu::SHADOW_GRID_SIZE);
    node_world_size(world, terrain.root_level, terrain.root_level) / grid as f32
}

/// The lowest `terrain`'s shadow mesh can be drawn over the world XZ rectangle `lo..hi` (module
/// docs: the pyramid over the rectangle grown by `grow` metres, and the coarse levels' drawn
/// mesh), or `None` for a gap: the grown rectangle leaves the map or touches a hole of
/// `terrain.holes` or of `lattice`.
#[must_use]
pub fn terrain_min_cover(
    terrain: &Terrain,
    lattice: Option<&LatticeHoleCut>,
    lo: [f32; 2],
    hi: [f32; 2],
    grow: f32,
) -> Option<f32> {
    let grow = if grow.is_finite() {
        grow.max(0.0)
    } else {
        return None;
    };
    let (rect_lo, rect_hi) = (lo, hi);
    let lo = [lo[0] - grow, lo[1] - grow];
    let hi = [hi[0] + grow, hi[1] + grow];
    if !(lo.iter().chain(&hi).all(|v| v.is_finite()) && lo[0] <= hi[0] && lo[1] <= hi[1]) {
        return None;
    }
    let map = &terrain.map;
    let (map_lo, map_hi) = (map.world_min(), map.world_max());
    if lo[0] < map_lo[0] || lo[1] < map_lo[1] || hi[0] > map_hi[0] || hi[1] > map_hi[1] {
        return None;
    }
    if let Some(holes) = &terrain.holes {
        // The mask tests squares: the square from `lo` holding the rectangle (wider: careful).
        let size = (hi[0] - lo[0]).max(hi[1] - lo[1]);
        if holes.intersects_rect(lo[0], lo[1], size) {
            return None;
        }
    }
    if lattice.is_some_and(|cut| cut.touches_rect(lo, hi)) {
        return None;
    }
    // The pyramid over the grown rectangle bounds the shadow levels that reach no further than
    // `grow`; the coarser ones are bounded by their drawn mesh (ADR-10161).
    let lowest = pyramid_min(terrain, lo, hi).min(coarse_shadow_min(terrain, rect_lo, rect_hi, grow));
    lowest.is_finite().then_some(lowest)
}

/// The minimum of the min-max pyramid's nodes over the world XZ rectangle `lo..hi` (at most
/// [`MAX_NODES_PER_SIDE`] nodes a side; a coarser level when it is wider).
fn pyramid_min(terrain: &Terrain, lo: [f32; 2], hi: [f32; 2]) -> f32 {
    let map = &terrain.map;
    let root = terrain.root_level;
    let (root_x, root_z, root_size) = QuadNode::root(root).map_rect(map, root);
    let span = (hi[0] - lo[0]).max(hi[1] - lo[1]);
    let mut level = 0u32;
    while level < root && span / (root_size / (1u32 << (root - level)) as f32) > MAX_NODES_PER_SIDE as f32 {
        level += 1;
    }
    let dim = 1u32 << (root - level);
    let size = root_size / dim as f32;
    let index = |v: f32, origin: f32| (((v - origin) / size).floor().max(0.0) as u32).min(dim - 1);
    let (x0, x1) = (index(lo[0], root_x), index(hi[0], root_x));
    let (z0, z1) = (index(lo[1], root_z), index(hi[1], root_z));
    let mut lowest = f32::INFINITY;
    for z in z0..=z1 {
        for x in x0..=x1 {
            lowest = lowest.min(terrain.minmax.node_min_max(QuadNode { level, x, z })[0]);
        }
    }
    lowest
}

/// Highest height-pyramid level [`coarse_shadow_min`] averages itself (a level-`k` texel reads
/// about `4^k` samples); a shadow level reading a coarser one is bounded by the min-max pyramid
/// over its whole reach instead.
const EXACT_MIP_MAX: u32 = 6;

/// Most shadow-mesh vertices [`coarse_shadow_min`] bounds one at a time on one level; a level
/// with more is bounded by the min-max pyramid over its whole reach instead.
const MAX_EXACT_VERTICES: usize = 4096;

/// A lower bound of the shadow mesh `terrain` draws over the XZ rectangle `lo..hi` on every
/// level whose reach ([`crate::gpu::shadow_vertex_reach`] at full morph) is longer than `grow`:
/// the pyramid minimum over the rectangle grown by `grow` already bounds the finer levels
/// (ADR-10161).
///
/// Such a level draws `SHADOW_GRID_SIZE` cells a node side, and each vertex reads the
/// box-filtered height pyramid (`crate::height_mips`) at its morphed position, blending the levels
/// of [`crate::height_mips::vertex_mip`]. A point of a drawn triangle is a convex mix of its three
/// vertices, so the mesh over the rectangle is never lower than the lowest vertex of a triangle
/// that can overlap it; a vertex is never lower than the lowest bilinear read of the levels it
/// blends over the box its morph moves it across. Both are computed here from the samples, with
/// the pyramid's own filter. A level on which cliff relief can act (its fine grid and normal taps
/// are not modelled), that reads a level above [`EXACT_MIP_MAX`], or that has more than
/// [`MAX_EXACT_VERTICES`] candidate vertices takes the min-max pyramid over the rectangle grown
/// by its reach instead: lower, never wrong.
fn coarse_shadow_min(terrain: &Terrain, lo: [f32; 2], hi: [f32; 2], grow: f32) -> f32 {
    let map = &terrain.map;
    let root = terrain.root_level;
    let grid = terrain.cdlod.grid_size.max(1);
    let shadow_grid = grid.clamp(1, crate::gpu::SHADOW_GRID_SIZE);
    let stride = (grid / crate::gpu::SHADOW_GRID_SIZE).max(1);
    let texel = map.sample_spacing().max(map.sample_spacing_z());
    let top = (mip_level_count(map.width(), map.height()) - 1) as f32;
    let (root_x, root_z, span) = QuadNode::root(root).draw_rect(map, root, grid);
    let (map_lo, map_hi) = (map.world_min(), map.world_max());
    let mut texels = MipTexels::default();
    let mut lowest = f32::INFINITY;
    for level in 0..=root {
        let size = node_world_size(span, root, level);
        let step = size / grid as f32;
        let cell = (step * stride as f32).max(size / shadow_grid as f32);
        let (reach, cliff) =
            crate::gpu::shadow_vertex_reach(step, size / shadow_grid as f32, texel, true, 1.0);
        if reach <= grow {
            continue;
        }
        // The vertices of the triangles that can overlap the rectangle: the morph moves a vertex
        // at most one step toward +X and +Z (to the next even grid index).
        let (cx0, cx1) = (lo[0] - cell - step, hi[0] + cell);
        let (cz0, cz1) = (lo[1] - cell - step, hi[1] + cell);
        let m0 = vertex_mip(step, map.sample_spacing(), 0.0).min(top);
        let m1 = vertex_mip(step, map.sample_spacing(), 1.0).min(top);
        let (k0, k1) = (m0.floor() as u32, m1.ceil() as u32);
        let per_side = |a: f32, b: f32| ((b - a) / (step * stride as f32)).max(0.0) as usize + 2;
        let candidates = per_side(cx0, cx1).saturating_mul(per_side(cz0, cz1));
        if cliff || k1 > EXACT_MIP_MAX || candidates > MAX_EXACT_VERTICES {
            let r_lo = [(lo[0] - reach).max(map_lo[0]), (lo[1] - reach).max(map_lo[1])];
            let r_hi = [(hi[0] + reach).min(map_hi[0]), (hi[1] + reach).min(map_hi[1])];
            lowest = lowest.min(pyramid_min(terrain, r_lo, r_hi));
            continue;
        }
        let dim = 1u32 << (root - level);
        let node_index = |v: f32, origin: f32| (((v - origin) / size).floor().max(0.0) as u32).min(dim - 1);
        let (nx0, nx1) = (node_index(cx0, root_x), node_index(cx1, root_x));
        let (nz0, nz1) = (node_index(cz0, root_z), node_index(cz1, root_z));
        // Grid indices `g` (multiples of `stride`, `0..=grid`) of a node at `origin` whose
        // vertex lies in `a..=b`.
        let indices = |origin: f32, a: f32, b: f32| {
            let first = ((a - origin) / step).ceil().max(0.0) as u32;
            let first = first.div_ceil(stride) * stride;
            let last = (((b - origin) / step).floor().max(-1.0) as i64).min(i64::from(grid));
            (first, last)
        };
        for nz in nz0..=nz1 {
            for nx in nx0..=nx1 {
                let (min_x, min_z, _) = QuadNode { level, x: nx, z: nz }.draw_rect(map, root, grid);
                let (gx0, gx1) = indices(min_x, cx0, cx1);
                let (gz0, gz1) = indices(min_z, cz0, cz1);
                let mut gz = gz0;
                while i64::from(gz) <= gz1 {
                    let mut gx = gx0;
                    while i64::from(gx) <= gx1 {
                        let at = |ix: u32, iz: u32| {
                            let [x, z] =
                                map.clamp_to_rect(min_x + ix as f32 * step, min_z + iz as f32 * step);
                            map.world_to_grid(x, z)
                        };
                        let (a, b) = (at(gx, gz), at(gx.div_ceil(2) * 2, gz.div_ceil(2) * 2));
                        let s_lo = [a[0].min(b[0]), a[1].min(b[1])];
                        let s_hi = [a[0].max(b[0]), a[1].max(b[1])];
                        for k in k0..=k1 {
                            lowest = lowest.min(texels.min_over_box(map, k, s_lo, s_hi));
                        }
                        gx += stride;
                    }
                    gz += stride;
                }
            }
        }
    }
    lowest
}

/// Texels of the box-filtered height pyramid, averaged on demand from the samples with the
/// pyramid's own filter ([`parent_taps`], `HeightMips::update_region`) and kept for the call.
#[derive(Default)]
struct MipTexels {
    known: std::collections::HashMap<(u32, u32, u32), f32>,
}

impl MipTexels {
    /// Size of level `k` (level 0: the map).
    fn dims(map: &TerrainMap, k: u32) -> (u32, u32) {
        if k == 0 {
            (map.width(), map.height())
        } else {
            level_dims(map.width(), map.height(), k)
        }
    }

    /// Texel `(col, row)` of level `k`.
    fn texel(&mut self, map: &TerrainMap, k: u32, col: u32, row: u32) -> f32 {
        if k == 0 {
            return map.heights()[(row * map.width() + col) as usize];
        }
        if let Some(v) = self.known.get(&(k, col, row)) {
            return *v;
        }
        let (w, h) = Self::dims(map, k);
        let (src_w, src_h) = Self::dims(map, k - 1);
        let cols = parent_taps(col, w, src_w);
        let rows = parent_taps(row, h, src_h);
        let value = if cols.1 - cols.0 <= 1 && rows.1 - rows.0 <= 1 {
            let (x0, x1, y0, y1) = (cols.0, cols.1, rows.0, rows.1);
            let (a, b) = (self.texel(map, k - 1, x0, y0), self.texel(map, k - 1, x1, y0));
            let (c, d) = (self.texel(map, k - 1, x0, y1), self.texel(map, k - 1, x1, y1));
            0.25 * (a + b + c + d)
        } else {
            let mut sum = 0.0f64;
            for y in rows.0..=rows.1 {
                for x in cols.0..=cols.1 {
                    sum += f64::from(self.texel(map, k - 1, x, y));
                }
            }
            let count = (cols.1 - cols.0 + 1) * (rows.1 - rows.0 + 1);
            (sum / f64::from(count)) as f32
        };
        self.known.insert((k, col, row), value);
        value
    }

    /// Lowest bilinear read of level `k` (`HeightMips::sample` at that level) over the box
    /// `s_lo..s_hi` in level-0 sample coordinates. A bilinear patch is linear along each axis,
    /// so over a box it is lowest at a corner of the box cut by the texel lines: those are read.
    fn min_over_box(&mut self, map: &TerrainMap, k: u32, s_lo: [f32; 2], s_hi: [f32; 2]) -> f32 {
        let (w, h) = Self::dims(map, k);
        let scale = (1u32 << k) as f32;
        let centre = (scale - 1.0) * 0.5;
        let to_level = |s: f32, side: u32| {
            let s = if s.is_nan() { 0.0 } else { s };
            ((s - centre) / scale).clamp(0.0, (side - 1) as f32)
        };
        let (u0, u1) = (to_level(s_lo[0], w), to_level(s_hi[0], w));
        let (v0, v1) = (to_level(s_lo[1], h), to_level(s_hi[1], h));
        let mut lowest = f32::INFINITY;
        let mut v = v0;
        loop {
            let mut u = u0;
            loop {
                lowest = lowest.min(self.bilinear(map, k, w, h, u, v));
                if u >= u1 {
                    break;
                }
                u = (u.floor() + 1.0).min(u1);
            }
            if v >= v1 {
                break;
            }
            v = (v.floor() + 1.0).min(v1);
        }
        lowest
    }

    /// Bilinear read of level `k` at level texel coordinates `(u, v)`, already clamped.
    fn bilinear(&mut self, map: &TerrainMap, k: u32, w: u32, h: u32, u: f32, v: f32) -> f32 {
        let x0 = (u.floor() as u32).min(w - 1);
        let y0 = (v.floor() as u32).min(h - 1);
        let x1 = (x0 + 1).min(w - 1);
        let y1 = (y0 + 1).min(h - 1);
        let (fx, fy) = (u - x0 as f32, v - y0 as f32);
        let (h00, h10) = (self.texel(map, k, x0, y0), self.texel(map, k, x1, y0));
        let (h01, h11) = (self.texel(map, k, x0, y1), self.texel(map, k, x1, y1));
        let a = h00 + (h10 - h00) * fx;
        let b = h01 + (h11 - h01) * fx;
        a + (b - a) * fy
    }
}

/// Whether `cell_box` lies under `terrain` for the sun travelling along `travel` ([`box_covered`]
/// with [`terrain_min_cover`] and the terrain's highest point).
#[must_use]
pub fn terrain_covers_box(
    terrain: &Terrain,
    lattice: Option<&LatticeHoleCut>,
    cell_box: &CellBox,
    travel: Vec3,
    grow: f32,
) -> bool {
    let top = terrain.minmax.node_min_max(QuadNode::root(terrain.root_level))[1];
    box_covered(cell_box, -travel, SUN_SEAL_SLACK_M, top, |lo, hi| {
        terrain_min_cover(terrain, lattice, lo, hi, grow)
    })
}

/// Fills `cover` with the indoor cells of `cells` whose every box some terrain covers for the sun
/// travelling along `travel` (module docs). `terrains` holds each terrain with its open mouths'
/// lattice cut. Returns `(cells tested, cells covered, largest growth used)`.
pub fn fill_terrain_sun_cover(
    cover: &mut SunCover,
    cells: &VisCells,
    terrains: &[(&Terrain, Option<&LatticeHoleCut>)],
    travel: [f32; 3],
    grow_m: Option<f32>,
) -> (usize, usize, f32) {
    let sun = Vec3::from_array(travel);
    let grow_of = |t: &Terrain| grow_m.unwrap_or_else(|| 2.0 * coarsest_shadow_cell_m(t));
    let grow_used = terrains.iter().map(|(t, _)| grow_of(t)).fold(0.0f32, f32::max);
    let mut tested = 0;
    let mut covered = Vec::new();
    for cell in cells
        .cells
        .iter()
        .filter(|c| c.id != regia_render::viscells::OUTDOOR_CELL)
    {
        tested += 1;
        let all = !cell.bounds.is_empty()
            && cell.bounds.iter().all(|b| {
                terrains
                    .iter()
                    .any(|(t, lattice)| terrain_covers_box(t, *lattice, b, sun, grow_of(t)))
            });
        if all {
            covered.push(cell.id);
        }
    }
    let count = covered.len();
    cover.set(covered, travel);
    (tested, count, grow_used)
}

/// What the last fill was made from, and the open mouths' lattice cuts it used.
#[derive(Default)]
pub struct TerrainSunCoverMemo {
    settings: Option<TerrainSunCover>,
    lattices: Vec<Option<LatticeHoleCut>>,
}

/// What [`update_terrain_sun_cover`] reads besides the terrains: the opt-in, the cells and the
/// frame (for its sun).
type CoverInputs<'w> = (
    Option<Res<'w, TerrainSunCover>>,
    Option<Res<'w, VisCells>>,
    Option<Res<'w, ExtractedFrame>>,
);

/// The terrains whose heights or mesh layers changed since the last run.
type ChangedTerrains<'w, 's> = Query<'w, 's, (), Or<(Changed<Terrain>, Changed<TerrainMeshLayers>)>>;

/// `Render`, after `extract_frame` and before the cell cull: with [`TerrainSunCover`] inserted,
/// keeps [`SunCover`] filled from the terrain for the frame's sun (module docs). Recomputes only
/// when the sun, the cells, a terrain or the settings change.
pub fn update_terrain_sun_cover(
    (settings, cells, extracted): CoverInputs<'_>,
    terrains: Query<(&Terrain, Option<&TerrainMeshLayers>)>,
    changed: ChangedTerrains<'_, '_>,
    cover: Option<ResMut<SunCover>>,
    stats: Option<ResMut<TerrainSunCoverStats>>,
    mut commands: Commands,
    mut memo: Local<TerrainSunCoverMemo>,
) {
    let Some(settings) = settings else {
        memo.settings = None;
        memo.lattices.clear();
        return;
    };
    let (Some(cells), Some(extracted)) = (cells, extracted) else {
        return;
    };
    let Some(travel) = extracted.directional.as_ref().map(|sun| sun.direction) else {
        return;
    };
    let terrain_changed = terrains.iter().count() != memo.lattices.len() || !changed.is_empty();
    let stale = memo.settings != Some(*settings)
        || cells.is_changed()
        || terrain_changed
        || cover.as_ref().is_none_or(|c| !c.matches_sun(travel));
    if !stale {
        return;
    }
    let _span = tracing::info_span!("terrain.sun_cover").entered();
    if terrain_changed {
        memo.lattices.clear();
        memo.lattices.extend(
            terrains
                .iter()
                .map(|(t, layers)| layers.and_then(|l| l.lattice_hole_cut(&t.map))),
        );
    }
    let memo = &mut *memo;
    let list: Vec<(&Terrain, Option<&LatticeHoleCut>)> = terrains
        .iter()
        .zip(&memo.lattices)
        .map(|((t, _), lattice)| (t, lattice.as_ref()))
        .collect();
    let mut fresh = SunCover::default();
    let (tested, covered, grow) = fill_terrain_sun_cover(&mut fresh, &cells, &list, travel, settings.grow_m);
    match cover {
        Some(mut cover) => *cover = fresh,
        None => commands.insert_resource(fresh),
    }
    memo.settings = Some(*settings);
    match stats {
        Some(mut stats) => {
            stats.fills += 1;
            stats.cells_tested = tested;
            stats.cells_covered = covered;
            stats.grow_m = grow;
        }
        None => commands.insert_resource(TerrainSunCoverStats {
            fills: 1,
            cells_tested: tested,
            cells_covered: covered,
            grow_m: grow,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdlod::{CdlodConfig, QuadNode};
    use crate::holes::{HoleEllipse, HolesMask};
    use crate::layers::TerrainLayers;
    use crate::sampler::TerrainMap;

    /// A flat 128 m map (x and z from −64 to 64) at height 0, root level 4: its coarsest shadow
    /// cell is 8 m, the default growth 16 m.
    fn flat() -> Terrain {
        let n = 33u32;
        let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 128.0, 10.0, 0.0).expect("map");
        Terrain::new(
            &map.to_heightfield(),
            128.0,
            10.0,
            0.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    fn tube(x: f32) -> CellBox {
        CellBox {
            center: [x, -10.0, 0.0],
            half_extents: [3.0, 3.0, 10.0],
            rotation_radians: 0.0,
        }
    }

    const DOWN: Vec3 = Vec3::new(0.0, -1.0, 0.0);

    #[test]
    fn the_growth_is_the_root_shadow_cell() {
        let t = flat();
        let (lo, hi) = (t.map.world_min(), t.map.world_max());
        assert_eq!((lo, hi), ([-64.0, -64.0], [64.0, 64.0]), "the test map's extent");
        assert_eq!(coarsest_shadow_cell_m(&t), 8.0);
    }

    #[test]
    fn a_buried_box_is_covered_and_a_box_at_the_surface_is_not() {
        let t = flat();
        assert!(terrain_covers_box(&t, None, &tube(0.0), DOWN, 16.0));
        let mut shallow = tube(0.0);
        shallow.center[1] = -2.0;
        assert!(
            !terrain_covers_box(&t, None, &shallow, DOWN, 16.0),
            "its top is above the ground"
        );
        // A 45° sun sweeps the footprint 13 m toward it: still on the map.
        assert!(terrain_covers_box(
            &t,
            None,
            &tube(0.0),
            Vec3::new(-1.0, -1.0, 0.0),
            16.0
        ));
    }

    #[test]
    fn the_map_edge_is_a_gap() {
        let t = flat();
        // 30 m from the edge: the 16 m growth fits, the sweep stays on the map.
        assert!(terrain_covers_box(&t, None, &tube(30.0), DOWN, 16.0));
        // 14 m from the edge: the grown sweep leaves the map.
        assert!(!terrain_covers_box(&t, None, &tube(47.0), DOWN, 16.0));
        assert!(
            terrain_covers_box(&t, None, &tube(47.0), DOWN, 10.0),
            "a smaller growth fits"
        );
        // A low sun from +X sweeps the footprint off the map's +X edge.
        assert!(!terrain_covers_box(
            &t,
            None,
            &tube(0.0),
            Vec3::new(-1.0, -0.2, 0.0),
            16.0
        ));
    }

    #[test]
    fn a_hole_over_the_sweep_is_a_gap() {
        let mut t = flat();
        let hole = |x: f32| {
            HolesMask::new(vec![HoleEllipse {
                center: [x, 0.0],
                radius: [2.0, 2.0],
                rotation_radians: 0.0,
            }])
            .expect("mask")
        };
        // Right over the tube.
        t.holes = Some(hole(0.0));
        assert!(!terrain_covers_box(&t, None, &tube(0.0), DOWN, 16.0));
        // 15 m off the footprint's side: inside the 16 m growth.
        t.holes = Some(hole(20.0));
        assert!(!terrain_covers_box(&t, None, &tube(0.0), DOWN, 16.0));
        // 40 m away: no gap.
        t.holes = Some(hole(45.0));
        assert!(terrain_covers_box(&t, None, &tube(0.0), DOWN, 16.0));
    }

    #[test]
    fn the_min_reads_the_lowest_node_over_the_grown_rectangle() {
        let n = 33u32;
        // A valley 20 m deep along x in 40..48 m (columns 26..28), flat 0 elsewhere.
        let heights = (0..n * n)
            .map(|i| if (26..=28).contains(&(i % n)) { -20.0 } else { 0.0 })
            .collect();
        // Heights −20 to 20 (the cooked samples cannot go below the offset).
        let map = TerrainMap::from_heights(n, n, heights, 128.0, 40.0, -20.0).expect("map");
        let t = Terrain::new(
            &map.to_heightfield(),
            128.0,
            40.0,
            -20.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain");
        let low = terrain_min_cover(&t, None, [0.0, -5.0], [10.0, 5.0], 0.0).expect("on the map");
        assert!(low > -1.0, "no valley under the plain: {low}");
        let near = terrain_min_cover(&t, None, [20.0, -5.0], [30.0, 5.0], 16.0).expect("on the map");
        assert!(near <= -19.0, "the growth reaches the valley: {near}");
        // A tube 10 m down near the valley is not covered: the ground there lies below its top.
        assert!(!terrain_covers_box(&t, None, &tube(25.0), DOWN, 16.0));
        assert!(terrain_covers_box(&t, None, &tube(-20.0), DOWN, 16.0));
    }

    #[test]
    fn the_system_fills_the_cover_and_refills_only_when_the_sun_moves() {
        let cells = VisCells::from_ron(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, -10.0, 0.0), half_extents: (3.0, 3.0, 10.0))]),
                (id: 2, bounds: [(center: (47.0, -10.0, 0.0), half_extents: (3.0, 3.0, 10.0))]),
                (id: 3, bounds: [(center: (0.0, -2.0, 30.0), half_extents: (3.0, 3.0, 3.0))]),
            ], portals: [])",
        )
        .expect("cells");
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.world_mut().resource_mut::<ExtractedFrame>().directional = Some(
            regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 1000.0),
        );
        app.insert_resource(cells);
        app.add_systems(Render, update_terrain_sun_cover);
        app.world_mut().spawn(flat());
        app.update();
        assert!(app.world().get_resource::<SunCover>().is_none(), "opt-in only");
        app.insert_resource(TerrainSunCover::default());
        app.update();
        let cover = app.world().resource::<SunCover>().clone();
        assert_eq!(
            cover.cells(),
            &[1],
            "cell 2 is near the edge, cell 3 pokes out of the ground"
        );
        assert!(cover.matches_sun([0.0, -1.0, 0.0]));
        let stats = app.world().resource::<TerrainSunCoverStats>().clone();
        assert_eq!((stats.fills, stats.cells_tested, stats.cells_covered), (1, 3, 1));
        assert_eq!(stats.grow_m, 16.0);
        app.update();
        assert_eq!(
            app.world().resource::<TerrainSunCoverStats>().fills,
            1,
            "same sun: kept"
        );
        app.world_mut().resource_mut::<ExtractedFrame>().directional = Some(
            regia_render::DirectionalLightUniform::new([0.3, -1.0, 0.0], [1.0; 3], 1000.0),
        );
        app.update();
        assert_eq!(
            app.world().resource::<TerrainSunCoverStats>().fills,
            2,
            "the sun moved"
        );
        assert!(app.world().resource::<SunCover>().matches_sun([0.3, -1.0, 0.0]));
        app.insert_resource(TerrainSunCover { grow_m: Some(10.0) });
        app.update();
        assert_eq!(
            app.world().resource::<SunCover>().cells(),
            &[1, 2],
            "less growth fits cell 2"
        );
    }

    /// The XZ rectangle [`box_covered`] tests: the grown box swept toward the sun until a ray from
    /// its bottom has risen to the terrain's highest point.
    fn swept_rect(terrain: &Terrain, cell: &CellBox, travel: Vec3) -> ([f32; 2], [f32; 2]) {
        let top = terrain.minmax.node_min_max(QuadNode::root(terrain.root_level))[1];
        let to_sun = (-travel).normalize_or_zero();
        let grown = CellBox {
            half_extents: cell.half_extents.map(|h| h + SUN_SEAL_SLACK_M),
            ..*cell
        };
        let corners = grown.corners();
        let (mut lo, mut hi) = (corners[0], corners[0]);
        for corner in &corners[1..] {
            lo = lo.min(*corner);
            hi = hi.max(*corner);
        }
        let run = (top - lo.y) / to_sun.y;
        let shift = [to_sun.x * run, to_sun.z * run];
        (
            [lo.x.min(lo.x + shift[0]), lo.z.min(lo.z + shift[1])],
            [hi.x.max(hi.x + shift[0]), hi.z.max(hi.z + shift[1])],
        )
    }

    /// The XZ rectangle [`box_covered`] tests for an unrotated box under a straight-down sun, with
    /// this map's high point at 0.
    fn footprint(cell: &CellBox) -> ([f32; 2], [f32; 2]) {
        let half = cell.half_extents.map(|h| h + SUN_SEAL_SLACK_M);
        (
            [cell.center[0] - half[0], cell.center[2] - half[2]],
            [cell.center[0] + half[0], cell.center[2] + half[2]],
        )
    }

    /// Flat 0, one sample at −20. Offset −20 and scale 40 keep that sample (the cooked range).
    fn with_valley(col: u32, row: u32) -> Terrain {
        let n = 33u32;
        let mut heights = vec![0.0; (n * n) as usize];
        heights[(row * n + col) as usize] = -20.0;
        let map = TerrainMap::from_heights(n, n, heights, 128.0, 40.0, -20.0).expect("map");
        Terrain::new(
            &map.to_heightfield(),
            128.0,
            40.0,
            -20.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    /// Lowest height the root's decimated shadow mesh draws over `lo..hi`, at morph 0, 1/2 and 1.
    ///
    /// The same grid [`coarsest_shadow_cell_m`] names (`SHADOW_GRID_SIZE` cells on the root) and
    /// the same filtered vertex as the shadow vertex shader ([`crate::cdlod::eval_node_vertex_filtered`]).
    /// A triangle's height over the rectangle is the minimum at the vertices of the clipped polygon.
    fn root_shadow_min_over(terrain: &Terrain, lo: [f32; 2], hi: [f32; 2]) -> f32 {
        let mips = crate::height_mips::HeightMips::build(&terrain.map);
        let node = crate::cdlod::QuadNode::root(terrain.root_level);
        let grid = terrain.cdlod.grid_size.max(1);
        let stride = (grid / crate::gpu::SHADOW_GRID_SIZE).max(1);
        let cells = grid / stride;
        let mut lowest = f32::INFINITY;
        for step in 0..=2 {
            let t = step as f32 / 2.0;
            let at = |gx: u32, gz: u32| {
                crate::cdlod::eval_node_vertex_filtered(
                    &terrain.map,
                    &mips,
                    &terrain.cdlod,
                    node,
                    terrain.root_level,
                    gx,
                    gz,
                    t,
                )
            };
            for iz in 0..cells {
                for ix in 0..cells {
                    let gx = ix * stride;
                    let gz = iz * stride;
                    let (a, b, c, d) = (
                        at(gx, gz),
                        at(gx + stride, gz),
                        at(gx + stride, gz + stride),
                        at(gx, gz + stride),
                    );
                    lowest = lowest.min(tri_min_over_rect(a, b, c, lo, hi));
                    lowest = lowest.min(tri_min_over_rect(a, c, d, lo, hi));
                }
            }
        }
        lowest
    }

    /// Minimum Y of triangle `a, b, c` inside the XZ rectangle `lo..hi` (`INFINITY` when it misses).
    fn tri_min_over_rect(a: [f32; 3], b: [f32; 3], c: [f32; 3], lo: [f32; 2], hi: [f32; 2]) -> f32 {
        let mut buf = [[0.0f32; 3]; 8];
        let mut tmp = [[0.0f32; 3]; 8];
        buf[0] = a;
        buf[1] = b;
        buf[2] = c;
        let mut n = clip_half(&buf, 3, 0, lo[0], true, &mut tmp);
        n = clip_half(&tmp, n, 0, hi[0], false, &mut buf);
        n = clip_half(&buf, n, 2, lo[1], true, &mut tmp);
        n = clip_half(&tmp, n, 2, hi[1], false, &mut buf);
        buf[..n].iter().fold(f32::INFINITY, |low, p| low.min(p[1]))
    }

    /// Clip polygon `src[..n]` to `axis >= bound` (`keep_above`) or `axis <= bound`.
    fn clip_half(
        src: &[[f32; 3]; 8],
        n: usize,
        axis: usize,
        bound: f32,
        keep_above: bool,
        dst: &mut [[f32; 3]; 8],
    ) -> usize {
        if n == 0 {
            return 0;
        }
        let inside = |p: [f32; 3]| {
            if keep_above {
                p[axis] >= bound
            } else {
                p[axis] <= bound
            }
        };
        let mut m = 0;
        for i in 0..n {
            let cur = src[i];
            let prev = src[(i + n - 1) % n];
            if inside(cur) != inside(prev) {
                let delta = cur[axis] - prev[axis];
                let t = if delta.abs() < 1.0e-12 {
                    0.0
                } else {
                    ((bound - prev[axis]) / delta).clamp(0.0, 1.0)
                };
                dst[m] = std::array::from_fn(|k| prev[k] + (cur[k] - prev[k]) * t);
                m += 1;
            }
            if inside(cur) {
                dst[m] = cur;
                m += 1;
            }
        }
        m
    }

    /// The cover's promise (module docs): no root shadow triangle over the swept footprint dips
    /// below the pyramid minimum the default growth reports. A box whose top sits between those
    /// two heights would be sealed while the drawn terrain does not close it.
    #[test]
    fn the_root_shadow_mesh_over_the_sweep_stays_above_the_grown_pyramid() {
        // Column 23 is x = 28, outside the 16 m growth of the tube at the origin (lip near x = 3).
        let terrain = with_valley(23, 16);
        let cell = tube(0.0);
        let grow = 2.0 * coarsest_shadow_cell_m(&terrain);
        let (lo, hi) = footprint(&cell);
        let [x, z] = terrain.map.sample_to_world(23, 16);
        assert!(
            z.abs() < 1.0e-3 && x > hi[0] + grow,
            "the valley at ({x}, {z}) must sit outside the grown footprint {lo:?}..{hi:?} + {grow}"
        );
        let reported =
            terrain_min_cover(&terrain, None, lo, hi, grow).expect("the footprint stays on the map");
        assert!(
            reported > -1.0,
            "the grown pyramid misses the valley, got {reported}"
        );
        let drawn = root_shadow_min_over(&terrain, lo, hi);
        assert!(
            drawn.is_finite() && drawn + 0.05 >= reported,
            "the root shadow mesh over the footprint draws {drawn}, below the pyramid minimum {reported} the cover trusts"
        );
    }

    /// Same promise when the sun sits at 45° in the +X sky. The sweep runs toward the sun; a
    /// valley past the grown lip must not pull a root shadow triangle over that sweep below the
    /// pyramid minimum. A box whose top sits between the two heights would be sealed while the
    /// drawn terrain does not close it.
    #[test]
    fn a_grazing_suns_root_shadow_mesh_stays_above_the_grown_pyramid() {
        // Light travels toward −X. Column 27 is x = 44, past the grown lip of the swept tube.
        let travel = Vec3::new(-1.0, -1.0, 0.0);
        let terrain = with_valley(27, 16);
        let cell = tube(0.0);
        let grow = 2.0 * coarsest_shadow_cell_m(&terrain);
        let (lo, hi) = swept_rect(&terrain, &cell, travel);
        let [x, z] = terrain.map.sample_to_world(27, 16);
        assert!(
            z.abs() < 1.0e-3 && x > hi[0] + grow,
            "the valley at ({x}, {z}) must sit outside the grown sweep {lo:?}..{hi:?} + {grow}"
        );
        let reported = terrain_min_cover(&terrain, None, lo, hi, grow).expect("the sweep stays on the map");
        assert!(
            reported > -1.0,
            "the grown pyramid misses the valley, got {reported}"
        );
        let drawn = root_shadow_min_over(&terrain, lo, hi);
        let covers = terrain_covers_box(&terrain, None, &cell, travel, grow);
        assert!(
            drawn.is_finite() && drawn + 0.05 >= reported,
            "the root shadow mesh over the sweep draws {drawn}, below the pyramid minimum {reported} (terrain_covers_box is {covers})"
        );
    }

    /// Lowest height `level`'s decimated shadow mesh draws over `lo..hi` at morph `t`, over every
    /// node of the level (the root helper above, for any level).
    fn level_shadow_min_over(
        terrain: &Terrain,
        mips: &crate::height_mips::HeightMips,
        level: u32,
        t: f32,
        lo: [f32; 2],
        hi: [f32; 2],
    ) -> f32 {
        let grid = terrain.cdlod.grid_size.max(1);
        let stride = (grid / crate::gpu::SHADOW_GRID_SIZE).max(1);
        let cells = grid / stride;
        let dim = 1u32 << (terrain.root_level - level);
        let mut lowest = f32::INFINITY;
        for nz in 0..dim {
            for nx in 0..dim {
                let node = QuadNode { level, x: nx, z: nz };
                // Only nodes whose triangles can reach the rectangle (morph: one step more).
                let (min_x, min_z, size) = node.draw_rect(&terrain.map, terrain.root_level, grid);
                let pad = size / grid as f32;
                if min_x > hi[0] + pad
                    || min_x + size + pad < lo[0]
                    || min_z > hi[1] + pad
                    || min_z + size + pad < lo[1]
                {
                    continue;
                }
                let at = |gx: u32, gz: u32| {
                    crate::cdlod::eval_node_vertex_filtered(
                        &terrain.map,
                        mips,
                        &terrain.cdlod,
                        node,
                        terrain.root_level,
                        gx,
                        gz,
                        t,
                    )
                };
                for iz in 0..cells {
                    for ix in 0..cells {
                        let (gx, gz) = (ix * stride, iz * stride);
                        let (a, b, c, d) = (
                            at(gx, gz),
                            at(gx + stride, gz),
                            at(gx + stride, gz + stride),
                            at(gx, gz + stride),
                        );
                        lowest = lowest.min(tri_min_over_rect(a, b, c, lo, hi));
                        lowest = lowest.min(tri_min_over_rect(a, c, d, lo, hi));
                    }
                }
            }
        }
        lowest
    }

    /// ADR-10161: on bumpy maps with narrow deep dips, for many rectangles and growths, the
    /// minimum `terrain_min_cover` reports never lies above the shadow mesh any level draws over
    /// the rectangle, at any of five morph factors (the CPU mirror of the shadow vertex).
    #[test]
    fn the_reported_minimum_bounds_every_levels_drawn_shadow_mesh() {
        let mut checked = 0;
        for (samples, seed) in [(33u32, 3u64), (65, 5)] {
            let mut rng = regia_core::math::Rng::new(seed);
            let mut heights: Vec<f32> = (0..samples * samples).map(|_| rng.range(-1.0..1.0)).collect();
            for _ in 0..6 {
                let i = rng.range(0..samples * samples) as usize;
                heights[i] = -20.0;
            }
            let map = TerrainMap::from_heights(samples, samples, heights, 128.0, 40.0, -20.0).expect("map");
            let terrain = Terrain::new(
                &map.to_heightfield(),
                128.0,
                40.0,
                -20.0,
                CdlodConfig::default(),
                4,
                TerrainLayers::default(),
            )
            .expect("terrain");
            let mips = crate::height_mips::HeightMips::build(&terrain.map);
            for grow in [0.0f32, 4.0, 16.0] {
                for _ in 0..12 {
                    let room = 60.0 - grow - 10.0;
                    let (cx, cz) = (rng.range(-room..room), rng.range(-room..room));
                    let (hx, hz) = (rng.range(1.0..10.0f32), rng.range(1.0..10.0f32));
                    let (lo, hi) = ([cx - hx, cz - hz], [cx + hx, cz + hz]);
                    let reported = terrain_min_cover(&terrain, None, lo, hi, grow).expect("on the map");
                    for level in 0..=terrain.root_level {
                        for step in 0..=4 {
                            let t = step as f32 / 4.0;
                            let drawn = level_shadow_min_over(&terrain, &mips, level, t, lo, hi);
                            assert!(
                                drawn.is_finite() && reported <= drawn + 1.0e-4,
                                "{samples} samples, grow {grow}, rect {lo:?}..{hi:?}: level {level} at \
                                 morph {t} draws {drawn}, below the reported minimum {reported}"
                            );
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 2 * 3 * 12 * 5 * 5);
    }
}
