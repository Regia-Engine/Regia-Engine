// SPDX-License-Identifier: MPL-2.0
// Source: K. Mamou, F. Ghorbel, "A simple and efficient approach for 3D mesh approximate convex
// decomposition", IEEE ICIP 2009, and the public V-HACD description (voxel parts cut by
// axis-aligned planes, concavity as hull volume minus part volume); I. E. Sutherland,
// G. W. Hodgman, "Reentrant polygon clipping", CACM 17(1), 1974.

//! The voxel side of the decomposition: the solid grid (surface cells by triangle/box overlap,
//! outside by a flood fill from the padding, the rest interior), voxel parts with the exact mesh
//! polygons clipped to their box, and the search for the cutting plane.
//!
//! A part's convex hull is measured two ways. The plane search uses the hull of the part's voxel
//! corners (integer coordinates, cheap and exact for the voxel set); the stop test and the output
//! use the hull of the mesh polygons clipped to the part's box, plus the box corners that lie inside
//! the solid, so a part's hull follows the mesh instead of the voxel staircase.

use glam::DVec3;
use regia_core::mesh_query::{TriMesh, WindingNumberTree, triangle};

use super::quickhull::quickhull;

const EMPTY: u8 = 0;
const SURFACE: u8 = 1;
const INTERIOR: u8 = 2;
const EXTERIOR: u8 = 3;

/// Bit that marks a packed voxel as a surface cell.
const SURFACE_BIT: u32 = 1 << 30;

/// How many cut positions the coarse plane search tries along an axis before refining.
const COARSE_CUTS: usize = 16;

/// Weight of the cell-count difference of the two sides in the cut cost.
const BALANCE_WEIGHT: f64 = 0.05;

/// Weight of the cut axis being shorter than the part's longest extent, as a fraction of the part's
/// own concavity.
const AXIS_WEIGHT: f64 = 0.2;

/// Inside threshold on the winding-number magnitude for a grid vertex next to a surface cell.
/// The band below `0.5` absorbs the f32 accumulation differences in the solid-angle sum (a
/// different summation order shifts the low bits; ADR-4773 pins the kernels themselves): a
/// vertex whose magnitude is outside the band classifies the same on every host. A vertex
/// inside the band — within `1e-6` of the surface — is treated as on the surface and belongs
/// to the hull; the classification of such a vertex can still differ between hosts whose sums
/// disagree by more than the band.
const WINDING_INSIDE_MIN: f32 = 0.5 - 1e-6;

/// `true` when a winding-number magnitude counts as inside for a surface-adjacent grid vertex
/// (see [`WINDING_INSIDE_MIN`]).
fn inside_from_winding(w: f32) -> bool {
    w.abs() >= WINDING_INSIDE_MIN
}

/// The solid voxel grid around a mesh. Cell `(x, y, z)` spans `origin + [x, x + 1] * h` (and so on);
/// voxel centers fall on `bounds.min + k * h`, so a face on the bounds lies in one cell layer, and a
/// one-cell padding on every side is always outside.
pub(crate) struct Grid {
    pub(crate) dims: [usize; 3],
    pub(crate) origin: DVec3,
    pub(crate) h: f64,
    cells: Vec<u8>,
    winding: Option<WindingNumberTree>,
}

impl Grid {
    /// Voxelizes `mesh` (non-empty, bounds with a positive extent) with `resolution` cells along
    /// the longest axis.
    pub(crate) fn build(mesh: &TriMesh, min: DVec3, max: DVec3, resolution: u32) -> Self {
        let extent = max - min;
        let h = extent.max_element() / f64::from(resolution);
        let dims: [usize; 3] = std::array::from_fn(|a| (extent[a] / h - 1e-9).ceil().max(0.0) as usize + 3);
        let origin = min - DVec3::splat(1.5 * h);
        let mut grid = Self {
            dims,
            origin,
            h,
            cells: vec![EMPTY; dims[0] * dims[1] * dims[2]],
            winding: WindingNumberTree::build(mesh.clone()).ok(),
        };
        grid.mark_surface(mesh);
        grid.flood_outside();
        grid
    }

    fn index(&self, x: usize, y: usize, z: usize) -> usize {
        x + self.dims[0] * (y + self.dims[1] * z)
    }

    fn cell_of(&self, v: f64, axis: usize) -> usize {
        let c = ((v - self.origin[axis]) / self.h).floor();
        (c.max(0.0) as usize).min(self.dims[axis] - 1)
    }

    fn mark_surface(&mut self, mesh: &TriMesh) {
        let positions = mesh.positions();
        for &[a, b, c] in mesh.triangles() {
            let (pa, pb, pc) = (
                positions[a as usize],
                positions[b as usize],
                positions[c as usize],
            );
            let tmin = pa.min(pb).min(pc).as_dvec3();
            let tmax = pa.max(pb).max(pc).as_dvec3();
            let lo: [usize; 3] = std::array::from_fn(|k| self.cell_of(tmin[k], k));
            let hi: [usize; 3] = std::array::from_fn(|k| self.cell_of(tmax[k], k));
            for z in lo[2]..=hi[2] {
                for y in lo[1]..=hi[1] {
                    for x in lo[0]..=hi[0] {
                        let i = self.index(x, y, z);
                        if self.cells[i] == SURFACE {
                            continue;
                        }
                        let cmin = self.origin + DVec3::new(x as f64, y as f64, z as f64) * self.h;
                        let cmax = cmin + DVec3::splat(self.h);
                        if triangle::overlaps_aabb(pa, pb, pc, cmin.as_vec3(), cmax.as_vec3()) {
                            self.cells[i] = SURFACE;
                        }
                    }
                }
            }
        }
    }

    fn flood_outside(&mut self) {
        let [dx, dy, dz] = self.dims;
        let mut stack = Vec::new();
        for z in 0..dz {
            for y in 0..dy {
                for x in 0..dx {
                    let border = x == 0 || y == 0 || z == 0 || x + 1 == dx || y + 1 == dy || z + 1 == dz;
                    let i = self.index(x, y, z);
                    if border && self.cells[i] == EMPTY {
                        self.cells[i] = EXTERIOR;
                        stack.push((x, y, z));
                    }
                }
            }
        }
        while let Some((x, y, z)) = stack.pop() {
            let neighbors = [
                (x.wrapping_sub(1), y, z),
                (x + 1, y, z),
                (x, y.wrapping_sub(1), z),
                (x, y + 1, z),
                (x, y, z.wrapping_sub(1)),
                (x, y, z + 1),
            ];
            for (nx, ny, nz) in neighbors {
                if nx < dx && ny < dy && nz < dz {
                    let i = self.index(nx, ny, nz);
                    if self.cells[i] == EMPTY {
                        self.cells[i] = EXTERIOR;
                        stack.push((nx, ny, nz));
                    }
                }
            }
        }
        for cell in &mut self.cells {
            if *cell == EMPTY {
                *cell = INTERIOR;
            }
        }
    }

    /// World position of grid vertex `g`.
    fn vertex_position(&self, g: [u32; 3]) -> DVec3 {
        self.origin + DVec3::new(f64::from(g[0]), f64::from(g[1]), f64::from(g[2])) * self.h
    }

    /// `true` when grid vertex `g` is inside the solid: all eight cells around it are solid, and,
    /// when one of them is a surface cell, the mesh's winding number there is above one half in
    /// magnitude.
    fn vertex_inside(&self, g: [u32; 3]) -> bool {
        let mut near_surface = false;
        for dz in 0..2u32 {
            for dy in 0..2u32 {
                for dx in 0..2u32 {
                    let c = [
                        g[0].wrapping_sub(dx),
                        g[1].wrapping_sub(dy),
                        g[2].wrapping_sub(dz),
                    ];
                    if c.iter().zip(&self.dims).any(|(&ci, &d)| ci as usize >= d) {
                        return false;
                    }
                    match self.cells[self.index(c[0] as usize, c[1] as usize, c[2] as usize)] {
                        SURFACE => near_surface = true,
                        INTERIOR => {}
                        _ => return false,
                    }
                }
            }
        }
        if !near_surface {
            return true;
        }
        let p = self.vertex_position(g).as_vec3();
        // Either winding: an inside-out mesh has a winding number near -1 inside.
        self.winding
            .as_ref()
            .is_some_and(|w| inside_from_winding(w.winding_number(p)))
    }

    /// Volume of one cell.
    pub(crate) fn cell_volume(&self) -> f64 {
        self.h * self.h * self.h
    }

    /// The part holding every solid cell, with every mesh triangle as its polygons.
    pub(crate) fn root_part(&self, mesh: &TriMesh) -> Part {
        let mut voxels = Vec::new();
        for z in 0..self.dims[2] {
            for y in 0..self.dims[1] {
                for x in 0..self.dims[0] {
                    match self.cells[self.index(x, y, z)] {
                        SURFACE => voxels.push(pack(x, y, z) | SURFACE_BIT),
                        INTERIOR => voxels.push(pack(x, y, z)),
                        _ => {}
                    }
                }
            }
        }
        let positions = mesh.positions();
        let polygons = mesh
            .triangles()
            .iter()
            .map(|tri| tri.iter().map(|&i| positions[i as usize].as_dvec3()).collect())
            .collect();
        let region_hi = self.dims.map(|d| d as u32);
        Part::new(self, voxels, [0; 3], region_hi, polygons, 0)
    }
}

fn pack(x: usize, y: usize, z: usize) -> u32 {
    x as u32 | ((y as u32) << 10) | ((z as u32) << 20)
}

fn unpack(v: u32) -> [u32; 3] {
    [v & 0x3ff, (v >> 10) & 0x3ff, (v >> 20) & 0x3ff]
}

/// A set of solid cells inside an axis-aligned box of the grid, with the mesh polygons clipped to
/// that box and the exact hull measured from them.
pub(crate) struct Part {
    /// Packed cells (`x | y << 10 | z << 20`, plus `SURFACE_BIT`), in grid order.
    voxels: Vec<u32>,
    /// Smallest and largest cell coordinate per axis (inclusive).
    lo: [u32; 3],
    hi: [u32; 3],
    /// The part's box as grid vertex indices.
    region_lo: [u32; 3],
    region_hi: [u32; 3],
    /// Mesh polygons clipped to the box.
    polygons: Vec<Vec<DVec3>>,
    /// How many cuts made this part.
    pub(crate) depth: u32,
    /// Solid volume estimate: interior cells plus half of each surface cell.
    pub(crate) solid_volume: f64,
    /// Vertices of the part's hull.
    pub(crate) hull_points: Vec<DVec3>,
    /// Volume of the part's hull.
    pub(crate) hull_volume: f64,
    /// `false` once a cut was tried and none split the part.
    pub(crate) splittable: bool,
}

impl Part {
    fn new(
        grid: &Grid,
        voxels: Vec<u32>,
        region_lo: [u32; 3],
        region_hi: [u32; 3],
        polygons: Vec<Vec<DVec3>>,
        depth: u32,
    ) -> Self {
        let mut lo = [u32::MAX; 3];
        let mut hi = [0u32; 3];
        let mut surface = 0usize;
        for &v in &voxels {
            let c = unpack(v);
            for ((l, h), &ci) in lo.iter_mut().zip(hi.iter_mut()).zip(&c) {
                *l = (*l).min(ci);
                *h = (*h).max(ci);
            }
            if v & SURFACE_BIT != 0 {
                surface += 1;
            }
        }
        let interior = voxels.len() - surface;
        let solid_volume = (interior as f64 + 0.5 * surface as f64) * grid.cell_volume();
        let mut part = Self {
            voxels,
            lo,
            hi,
            region_lo,
            region_hi,
            polygons,
            depth,
            solid_volume,
            hull_points: Vec::new(),
            hull_volume: 0.0,
            splittable: true,
        };
        part.measure_hull(grid);
        part
    }

    /// Number of cells.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.voxels.len()
    }

    /// Hull volume above the solid volume (never negative).
    pub(crate) fn concavity(&self) -> f64 {
        (self.hull_volume - self.solid_volume).max(0.0)
    }

    fn measure_hull(&mut self, grid: &Grid) {
        let mut points: Vec<DVec3> = self.polygons.iter().flatten().copied().collect();
        for corner in 0..8u32 {
            let g: [u32; 3] = std::array::from_fn(|axis| {
                if corner & (1 << axis) == 0 {
                    self.region_lo[axis]
                } else {
                    self.region_hi[axis]
                }
            });
            if grid.vertex_inside(g) {
                points.push(grid.vertex_position(g));
            }
        }
        if let Some(hull) = quickhull(&points, 0) {
            self.hull_volume = hull.volume();
            self.hull_points = hull.vertices;
            return;
        }
        // The clipped polygons are flat (a sheet with no volume inside this box): fall back to the
        // hull of the part's cells.
        if let Some(rows) = AxisRows::build(self, 0) {
            let corners = rows.corner_points(0, rows.na - 1);
            let world: Vec<DVec3> = corners.iter().map(|&p| rows.to_world(grid, p)).collect();
            if let Some(hull) = quickhull(&world, 0) {
                self.hull_volume = hull.volume();
                self.hull_points = hull.vertices;
            }
        }
    }

    /// The cut that minimizes the cut cost, as `(axis, c)`: cells with coordinate below grid
    /// vertex `c` on `axis` go to the first side. `None` when the part is one cell thick along
    /// every axis.
    ///
    /// The cost is the summed voxel concavity of the two sides, plus `BALANCE_WEIGHT` times the
    /// difference of their cell counts, plus `AXIS_WEIGHT` times the part's own voxel concavity scaled
    /// by how much shorter the cut axis is than the part's longest extent (so a cut along a short
    /// axis that removes most of the concavity still wins). The two bias terms matter where
    /// no single cut lowers the concavity much (a ring): they favor cuts through the middle and
    /// across the long extents over trimming a cap or slicing the ring flat.
    pub(crate) fn best_cut(&self) -> Option<(usize, u32)> {
        let tables: [Option<AxisRows>; 3] = std::array::from_fn(|axis| AxisRows::build(self, axis));
        let extent: [f64; 3] = std::array::from_fn(|axis| f64::from(self.hi[axis] - self.lo[axis] + 1));
        let longest = extent[0].max(extent[1]).max(extent[2]);
        let own = tables[0]
            .as_ref()
            .map_or(0.0, |rows| rows.side_concavity(0, rows.na - 1, self.voxels.len()));
        let cost_of = |axis: usize, rows: &AxisRows, k: usize| {
            let (concavity, left, right) = rows.cut_cost(k)?;
            let balance = (left as f64 - right as f64).abs();
            let thin = 1.0 - extent[axis] / longest;
            Some(concavity + BALANCE_WEIGHT * balance + AXIS_WEIGHT * thin * own)
        };
        // (cost, axis, local cut, stride)
        let mut best: Option<(f64, usize, usize, usize)> = None;
        for (axis, rows) in tables.iter().enumerate() {
            let Some(rows) = rows else { continue };
            let stride = rows.na.div_ceil(COARSE_CUTS).max(1);
            let mut k = stride;
            while k < rows.na {
                if let Some(cost) = cost_of(axis, rows, k)
                    && best.is_none_or(|(b, ..)| cost < b)
                {
                    best = Some((cost, axis, k, stride));
                }
                k += stride;
            }
        }
        let (mut cost, axis, k, stride) = best?;
        let rows = tables[axis].as_ref()?;
        let mut cut = k;
        let first = k.saturating_sub(stride - 1).max(1);
        let last = (k + stride - 1).min(rows.na - 1);
        for candidate in first..=last {
            if let Some(c) = cost_of(axis, rows, candidate)
                && c < cost
            {
                (cost, cut) = (c, candidate);
            }
        }
        Some((axis, self.lo[axis] + cut as u32))
    }

    /// Splits the part at grid vertex `c` on `axis`: cells and polygons below the plane go to the
    /// first part. `None` when one side would be empty.
    pub(crate) fn split(&self, grid: &Grid, axis: usize, c: u32) -> Option<(Part, Part)> {
        let (below, above): (Vec<u32>, Vec<u32>) =
            self.voxels.iter().copied().partition(|&v| unpack(v)[axis] < c);
        if below.is_empty() || above.is_empty() {
            return None;
        }
        let plane = grid.origin[axis] + f64::from(c) * grid.h;
        let eps = grid.h * 1e-9;
        let mut polys_below = Vec::new();
        let mut polys_above = Vec::new();
        for poly in &self.polygons {
            let b = clip(poly, axis, plane, eps, true);
            if !b.is_empty() {
                polys_below.push(b);
            }
            let a = clip(poly, axis, plane, eps, false);
            if !a.is_empty() {
                polys_above.push(a);
            }
        }
        let mut hi_below = self.region_hi;
        hi_below[axis] = c;
        let mut lo_above = self.region_lo;
        lo_above[axis] = c;
        let first = Part::new(grid, below, self.region_lo, hi_below, polys_below, self.depth + 1);
        let second = Part::new(grid, above, lo_above, self.region_hi, polys_above, self.depth + 1);
        Some((first, second))
    }
}

/// Keeps the part of `poly` on one side of the plane `p[axis] = plane` (Sutherland–Hodgman):
/// `below` keeps `p[axis] <= plane`, otherwise `p[axis] >= plane`. Points within `eps` of the
/// plane count as on both sides and are moved onto it; new points land exactly on the plane.
fn clip(poly: &[DVec3], axis: usize, plane: f64, eps: f64, below: bool) -> Vec<DVec3> {
    let inside = |p: DVec3| {
        if below {
            p[axis] <= plane + eps
        } else {
            p[axis] >= plane - eps
        }
    };
    // Snap points within `eps` onto the plane, so a part's points never sit a rounding error
    // outside its own cut face (the hull would then see that face as barely visible).
    let snap = |mut p: DVec3| {
        if (p[axis] - plane).abs() <= eps {
            p[axis] = plane;
        }
        p
    };
    if poly.iter().all(|&p| inside(p)) {
        return poly.iter().map(|&p| snap(p)).collect();
    }
    let mut out = Vec::with_capacity(poly.len() + 2);
    let Some(&last) = poly.last() else {
        return out;
    };
    let mut prev = last;
    let mut prev_in = inside(prev);
    for &cur in poly {
        let cur_in = inside(cur);
        if cur_in != prev_in {
            let denom = cur[axis] - prev[axis];
            let t = if denom == 0.0 {
                0.0
            } else {
                ((plane - prev[axis]) / denom).clamp(0.0, 1.0)
            };
            let mut hit = prev + (cur - prev) * t;
            hit[axis] = plane;
            out.push(hit);
        }
        if cur_in {
            out.push(snap(cur));
        }
        prev = cur;
        prev_in = cur_in;
    }
    out
}

/// A part's cells as rows along axis `b`, one row per (slab along `axis`, column along `w`), each
/// row reduced to its first and last cell: enough to find the hull of any run of slabs.
struct AxisRows {
    axis: usize,
    b: usize,
    w: usize,
    /// Number of slabs and columns.
    na: usize,
    nw: usize,
    /// Lowest coordinate of the part along `b` (so local values start at 0).
    b0: u32,
    /// `(first, last)` local cell along `b` per row, `(i32::MAX, i32::MIN)` when empty.
    rows: Vec<(i32, i32)>,
    /// Prefix sums of cells per slab (`na + 1` entries).
    prefix: Vec<usize>,
    /// The part's lowest cell along `axis` and `w`.
    a0: u32,
    w0: u32,
}

impl AxisRows {
    fn build(part: &Part, axis: usize) -> Option<Self> {
        if part.voxels.is_empty() {
            return None;
        }
        let b = (axis + 1) % 3;
        let w = (axis + 2) % 3;
        let na = (part.hi[axis] - part.lo[axis] + 1) as usize;
        let nw = (part.hi[w] - part.lo[w] + 1) as usize;
        let mut rows = vec![(i32::MAX, i32::MIN); na * nw];
        let mut counts = vec![0usize; na];
        for &v in &part.voxels {
            let c = unpack(v);
            let x = (c[axis] - part.lo[axis]) as usize;
            let z = (c[w] - part.lo[w]) as usize;
            let y = (c[b] - part.lo[b]) as i32;
            let row = &mut rows[x * nw + z];
            row.0 = row.0.min(y);
            row.1 = row.1.max(y);
            counts[x] += 1;
        }
        let mut prefix = Vec::with_capacity(na + 1);
        prefix.push(0);
        for count in counts {
            prefix.push(prefix[prefix.len() - 1] + count);
        }
        Some(Self {
            axis,
            b,
            w,
            na,
            nw,
            b0: part.lo[b],
            rows,
            prefix,
            a0: part.lo[axis],
            w0: part.lo[w],
        })
    }

    /// The summed voxel concavity of slabs `[0, k)` and `[k, na)` with the two sides' cell counts,
    /// or `None` when the cut is not strictly inside or a side has no cells.
    fn cut_cost(&self, k: usize) -> Option<(f64, usize, usize)> {
        if k == 0 || k >= self.na {
            return None;
        }
        let left = self.prefix[k];
        let right = self.prefix[self.na] - left;
        if left == 0 || right == 0 {
            return None;
        }
        let concavity = self.side_concavity(0, k - 1, left) + self.side_concavity(k, self.na - 1, right);
        Some((concavity, left, right))
    }

    fn side_concavity(&self, x0: usize, x1: usize, count: usize) -> f64 {
        let points = self.corner_points(x0, x1);
        let hull = quickhull(&points, 0).map_or(0.0, |h| h.volume());
        (hull - count as f64).max(0.0)
    }

    /// The corners of the cells of slabs `[x0, x1]` that can be hull vertices, in local
    /// `(slab, b, w)` coordinates. Each grid line along `b` keeps its lowest and highest cell
    /// corner; a corner is dropped when it lies on or behind the midpoint of its two neighbors
    /// along the slab or column direction (it is then inside the segment from that midpoint to
    /// the corner at the other end of its line, so it cannot be a hull vertex).
    fn corner_points(&self, x0: usize, x1: usize) -> Vec<DVec3> {
        let ncx = x1 - x0 + 2;
        let ncw = self.nw + 1;
        let mut lo_f = vec![i32::MAX; ncx * ncw];
        let mut hi_f = vec![i32::MIN; ncx * ncw];
        for x in x0..=x1 {
            for z in 0..self.nw {
                let (first, last) = self.rows[x * self.nw + z];
                if first > last {
                    continue;
                }
                for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let k = (x - x0 + dx) * ncw + z + dz;
                    lo_f[k] = lo_f[k].min(first);
                    hi_f[k] = hi_f[k].max(last + 1);
                }
            }
        }
        let behind = |field: &[i32], k: usize, step: usize, low: bool| {
            let (a, b) = (field[k - step], field[k + step]);
            if a == i32::MAX || a == i32::MIN || b == i32::MAX || b == i32::MIN {
                return false;
            }
            let twice = 2 * i64::from(field[k]);
            let sum = i64::from(a) + i64::from(b);
            if low { twice >= sum } else { twice <= sum }
        };
        let mut points = Vec::new();
        for cx in 0..ncx {
            for cw in 0..ncw {
                let k = cx * ncw + cw;
                if lo_f[k] == i32::MAX {
                    continue;
                }
                let inner_x = cx > 0 && cx + 1 < ncx;
                let inner_w = cw > 0 && cw + 1 < ncw;
                for (field, low) in [(lo_f.as_slice(), true), (hi_f.as_slice(), false)] {
                    let dropped =
                        (inner_x && behind(field, k, ncw, low)) || (inner_w && behind(field, k, 1, low));
                    if !dropped {
                        points.push(DVec3::new((x0 + cx) as f64, f64::from(field[k]), cw as f64));
                    }
                }
            }
        }
        points
    }

    /// A local `(slab, b, w)` corner as a world position.
    fn to_world(&self, grid: &Grid, p: DVec3) -> DVec3 {
        let mut g = DVec3::ZERO;
        g[self.axis] = p.x + f64::from(self.a0);
        g[self.b] = p.y + f64::from(self.b0);
        g[self.w] = p.z + f64::from(self.w0);
        grid.origin + g * grid.h
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;

    #[test]
    fn inside_from_winding_is_stable_across_the_band() {
        // ADR-4773 (audit G finding 6): values within the deterministic band below 0.5 count
        // as inside, so a 1-ulp change in the f64 solid-angle sum cannot flip a surface-cell
        // vertex — and so the cooked hull — between cook hosts.
        for w in [0.5 - 1e-6, 0.5 - 3e-7, 0.5, 0.5 + 3e-7, 1.0] {
            assert!(inside_from_winding(w), "{w}");
            let neg = -w;
            assert!(inside_from_winding(neg), "{neg}");
        }
        for w in [0.5 - 3e-6, 0.499, 0.4] {
            assert!(!inside_from_winding(w), "{w}");
        }
        // Perturbations of one f32 ulp around the threshold cannot flip the bit.
        let half = 0.5_f32.to_bits();
        for d in -3..=3_i32 {
            let w = f32::from_bits(half.wrapping_add(d as u32));
            assert!(inside_from_winding(w), "{w}");
            let neg = -w;
            assert!(inside_from_winding(neg), "{neg}");
        }
    }

    fn box_mesh(min: Vec3, max: Vec3) -> TriMesh {
        let p = |x: bool, y: bool, z: bool| {
            Vec3::new(
                if x { max.x } else { min.x },
                if y { max.y } else { min.y },
                if z { max.z } else { min.z },
            )
        };
        let positions = vec![
            p(false, false, false),
            p(true, false, false),
            p(true, true, false),
            p(false, true, false),
            p(false, false, true),
            p(true, false, true),
            p(true, true, true),
            p(false, true, true),
        ];
        let triangles = vec![
            [0, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [3, 6, 2],
            [3, 7, 6],
            [0, 4, 7],
            [0, 7, 3],
            [1, 2, 6],
            [1, 6, 5],
        ];
        TriMesh::new(positions, triangles).expect("box")
    }

    #[test]
    fn box_voxelizes_to_a_solid_block() {
        let mesh = box_mesh(Vec3::ZERO, Vec3::new(2.0, 1.0, 1.0));
        let grid = Grid::build(&mesh, DVec3::ZERO, DVec3::new(2.0, 1.0, 1.0), 16);
        // 16 cells per 2 units: centers at 0, 1/8, ..., 2 (17) and 0..1 (9), plus padding.
        assert_eq!(grid.dims, [19, 11, 11]);
        let part = grid.root_part(&mesh);
        assert_eq!(part.len(), 17 * 9 * 9);
        let interior = part.voxels.iter().filter(|&&v| v & SURFACE_BIT == 0).count();
        assert_eq!(interior, 15 * 7 * 7);
        // The hull is the box itself. The solid estimate counts half of each surface cell, so the
        // edge and corner cells overshoot a little: (735 + 642 / 2) / 512 = 2.0625.
        assert!((part.hull_volume - 2.0).abs() < 1e-9, "hull {}", part.hull_volume);
        assert!(
            (part.solid_volume - 2.0625).abs() < 1e-12,
            "solid {}",
            part.solid_volume
        );
        assert_eq!(part.concavity(), 0.0);
    }

    #[test]
    fn voxel_corner_hull_of_a_block_is_exact() {
        let mesh = box_mesh(Vec3::ZERO, Vec3::new(2.0, 1.0, 1.0));
        let grid = Grid::build(&mesh, DVec3::ZERO, DVec3::new(2.0, 1.0, 1.0), 16);
        let part = grid.root_part(&mesh);
        for axis in 0..3 {
            let rows = AxisRows::build(&part, axis).expect("rows");
            let points = rows.corner_points(0, rows.na - 1);
            // A block keeps only its eight corners.
            assert_eq!(points.len(), 8, "axis {axis}");
            let hull = quickhull(&points, 0).expect("hull");
            assert!((hull.volume() - part.len() as f64).abs() < 1e-9);
            // Any cut through a block leaves two blocks: no concavity.
            let (concavity, left, right) = rows.cut_cost(rows.na / 2).expect("cut");
            assert_eq!(concavity, 0.0);
            assert_eq!(left + right, part.len());
            assert_eq!(rows.cut_cost(0), None);
            assert_eq!(rows.cut_cost(rows.na), None);
        }
    }

    #[test]
    fn clipping_keeps_points_on_the_plane_on_both_sides() {
        let tri = [DVec3::ZERO, DVec3::new(2.0, 0.0, 0.0), DVec3::new(0.0, 2.0, 0.0)];
        let below = clip(&tri, 0, 1.0, 1e-12, true);
        let above = clip(&tri, 0, 1.0, 1e-12, false);
        assert_eq!(below.len(), 4);
        assert_eq!(above.len(), 3);
        assert!(below.iter().all(|p| p.x <= 1.0));
        assert!(above.iter().all(|p| p.x >= 1.0));
        assert!(above.contains(&DVec3::new(1.0, 0.0, 0.0)));
        assert!(above.contains(&DVec3::new(1.0, 1.0, 0.0)));
        assert!(clip(&tri, 0, -1.0, 1e-12, true).is_empty());
        assert_eq!(clip(&tri, 0, 5.0, 1e-12, true), tri.to_vec());
    }

    #[test]
    fn hollow_box_shell_has_interior_cells() {
        // A closed box with inverted winding still voxelizes the same way: the flood fill does not
        // read the winding.
        let mut mesh = box_mesh(Vec3::splat(-1.0), Vec3::splat(1.0));
        mesh.flip();
        let grid = Grid::build(&mesh, DVec3::splat(-1.0), DVec3::splat(1.0), 8);
        let part = grid.root_part(&mesh);
        assert_eq!(part.len(), 9 * 9 * 9);
        let interior = part.voxels.iter().filter(|&&v| v & SURFACE_BIT == 0).count();
        assert_eq!(interior, 7 * 7 * 7);
    }

    #[test]
    fn grid_vertex_inside_test() {
        let mesh = box_mesh(Vec3::splat(-1.0), Vec3::splat(1.0));
        let grid = Grid::build(&mesh, DVec3::splat(-1.0), DVec3::splat(1.0), 8);
        // Cell size 0.25, origin -1.375: vertex 5 is at -0.125 (deep inside), vertex 1 at -1.125
        // (outside), vertex 2 at -0.875 (inside, next to the surface layer).
        assert!(grid.vertex_inside([5, 5, 5]));
        assert!(!grid.vertex_inside([1, 5, 5]));
        assert!(grid.vertex_inside([2, 5, 5]));
        assert!(!grid.vertex_inside([0, 0, 0]));
        assert!(!grid.vertex_inside([u32::MAX, 5, 5]));
    }
}
