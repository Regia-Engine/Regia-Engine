// SPDX-License-Identifier: MPL-2.0
//! The sim ground lattice (spec 121 § 3, lane W-10, ADR-8214): the heightfield's own sample lattice,
//! triangulated with the fixed `(x0, z1)-(x1, z0)` diagonal, vertices on integer millimetres, the index
//! `i / (n - 1)` everywhere.
//!
//! [`GroundLattice`] reads a cooked canonical section ([`WorldGeomCooked`]) and nothing else: the
//! lattice constants, the level of every ground tile, the count blocks at those levels, the
//! hanging-vertex overrides, the ring pins and the mouth-cut bitmaps. It answers the ground height
//! under a point with the arithmetic of spec 121 § 5.3 rule 2 (IEEE `f32` `+ - * /` in source order,
//! no fused multiply-add, no transcendental), allocation free.
//!
//! - **Vertex positions.** Vertex `i` sits at `x_mm(i)` ([`LatticeConstants::vertex_mm`]), read as
//!   `f32` through the pinned [`mm_to_m`] only, so a lattice vertex and a canonical triangle vertex
//!   with the same world millimetre have the same bits (the ring pin).
//! - **Vertex heights.** An override (a ring pin or a hanging vertex) is an absolute millimetre height
//!   read through [`mm_to_m`]; any other vertex is `offset_m + count * step_m` (one multiply, one add).
//! - **Point location.** The cell estimate `floor((x - x(0)) / s_m)`, clamped, then corrected against
//!   the `f32` vertex positions, at most one step each way ([`GroundLattice::locate`] reports the
//!   steps). Inside a cell the diagonal's edge function, evaluated from the diagonal's canonical end
//!   (the vertex with the smaller `(x_mm, z_mm)`, which is `(x0, z1)`), picks the triangle; a point on
//!   the diagonal belongs to the lower triangle `[v00, v01, v10]`, the one that walks the diagonal in
//!   its canonical direction. A point on a cell's west edge belongs to that cell, a point on its south
//!   edge to the cell below (each edge goes to the triangle that walks it in its canonical
//!   direction). The query core (W-11) extends the same rule to canonical triangles.
//! - **Nested tiles.** A ground tile at level `k` is triangulated on every `2^k`-th lattice vertex; its
//!   border vertices that a coarser neighbor lacks carry hanging overrides (seam rule 4), so the
//!   ground steps at most 1 mm across a pitch border.

use crate::cooked::{CutBitmap, LatticeConstants, VertexHeightMm, WorldGeomCooked};
use crate::units::mm_to_m;

/// A lattice triangle of one cell: `0` is `[v00, v01, v10]` (the lower one), `1` is
/// `[v10, v01, v11]`.
pub type CellTriangle = u8;

/// What the ground is at a point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GroundAt {
    /// The ground height, metres.
    Ground(f32),
    /// An open mouth cuts the lattice triangle under the point (the layer is the floor there).
    Cut,
    /// The point is off the map.
    OffMap,
}

/// The `f32` position of lattice vertex index `i` along one axis, metres: `mm_to_m(0, x_mm(i))`.
#[must_use]
pub fn vertex_m(lattice: &LatticeConstants, i: u32) -> f32 {
    // `vertex_mm` is within the map extent, which `LatticeConstants` checks below 2^24 mm.
    mm_to_m(0, lattice.vertex_mm(i) as i32)
}

/// The cut bit of lattice triangle `t` of cell `(ci, cj)` local to a ground tile `cols` cells wide.
#[must_use]
pub fn cut_bit_index(cols: u32, ci: u32, cj: u32, t: CellTriangle) -> u64 {
    2 * (u64::from(cj) * u64::from(cols) + u64::from(ci)) + u64::from(t)
}

/// The ground of a cooked canonical section (spec 121 § 3).
#[derive(Clone, Copy, Debug)]
pub struct GroundLattice<'a> {
    lattice: LatticeConstants,
    tile_levels: &'a [u8],
    ground_counts: &'a [Vec<u16>],
    hanging: &'a [VertexHeightMm],
    ring_pins: &'a [VertexHeightMm],
    cuts: &'a [CutBitmap],
    pitch_m: f32,
    x_first: f32,
    #[cfg(feature = "stage_b")]
    edits: Option<&'a crate::edit::GroundEdits>,
}

/// A located lattice cell and how many correction steps the estimate needed on each axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Located {
    /// Cell column (`0..n - 1`).
    pub i: u32,
    /// Cell row (`0..n - 1`).
    pub j: u32,
    /// Correction steps taken on x and z (spec 121 § 3: never more than one).
    pub steps: [u32; 2],
}

impl<'a> GroundLattice<'a> {
    /// The ground of `cooked` (its canonical section must have passed [`WorldGeomCooked::check`]).
    #[must_use]
    pub fn new(cooked: &'a WorldGeomCooked) -> Self {
        let lattice = cooked.lattice;
        let cells = lattice.cells().max(1);
        // The pitch in metres as `f32`: the width over the cell count (one division of two exactly
        // representable values), used only for the cell estimate.
        let pitch_m = mm_to_m(0, lattice.width_mm) / cells as f32;
        Self {
            lattice,
            tile_levels: &cooked.tile_levels,
            ground_counts: &cooked.ground_counts,
            hanging: &cooked.hanging_overrides,
            ring_pins: &cooked.ring_pins,
            cuts: &cooked.cut_bitmaps,
            pitch_m,
            x_first: vertex_m(&lattice, 0),
            #[cfg(feature = "stage_b")]
            edits: None,
        }
    }

    /// The same ground with a `GeomState`'s D1 overrides and promoted tiles over it (spec 121 § 6).
    /// An empty edit set reads exactly the cooked ground.
    #[cfg(feature = "stage_b")]
    #[must_use]
    pub(crate) fn with_edits(mut self, edits: &'a crate::edit::GroundEdits) -> Self {
        self.edits = (!edits.is_empty()).then_some(edits);
        self
    }

    /// The level the tile's counts are stored at (its cooked level, whatever a D1 edit promoted).
    fn cooked_level(&self, tile: u32) -> u8 {
        self.tile_levels.get(tile as usize).copied().unwrap_or(0)
    }

    /// The lattice constants.
    #[must_use]
    pub fn lattice(&self) -> &LatticeConstants {
        &self.lattice
    }

    /// The ground tile index (`b * tiles_per_side + a`) holding cell `(i, j)`.
    #[must_use]
    pub fn tile_of_cell(&self, i: u32, j: u32) -> u32 {
        let g = u32::from(self.lattice.ground_tile_cells);
        (j / g) * self.lattice.tiles_per_side() + i / g
    }

    /// The level of ground tile `tile` (0 = full pitch): its cooked level, or 0 once a D1 edit
    /// promoted it (spec 121 § 3 seam rule 6).
    #[must_use]
    pub fn tile_level(&self, tile: u32) -> u8 {
        #[cfg(feature = "stage_b")]
        if self.edits.is_some_and(|e| e.is_promoted(tile)) {
            return 0;
        }
        self.cooked_level(tile)
    }

    /// The absolute override of a vertex: a ring pin (never edited, E5), then a D1 edit, then a
    /// cooked hanging vertex.
    pub(crate) fn override_at(&self, vertex: u32) -> Option<i32> {
        let find = |list: &[VertexHeightMm]| {
            list.binary_search_by_key(&vertex, |v| v.vertex)
                .ok()
                .map(|k| list[k].height_mm)
        };
        if let Some(mm) = find(self.ring_pins) {
            return Some(mm);
        }
        #[cfg(feature = "stage_b")]
        if let Some(mm) = self.edits.and_then(|e| e.get(&self.lattice, vertex)) {
            return Some(mm);
        }
        find(self.hanging)
    }

    #[cfg(all(feature = "stage_b", any(test, feature = "stats")))]
    /// The cooked override of a vertex (a ring pin or a hanging vertex), ignoring every D1 edit
    /// (the reference edit walk, ADR-8818).
    pub(crate) fn cooked_override_at(&self, vertex: u32) -> Option<i32> {
        let find = |list: &[VertexHeightMm]| {
            list.binary_search_by_key(&vertex, |v| v.vertex)
                .ok()
                .map(|k| list[k].height_mm)
        };
        find(self.ring_pins).or_else(|| find(self.hanging))
    }

    #[cfg(all(feature = "stage_b", any(test, feature = "stats")))]
    /// Whether `vertex` is a mouth-ring vertex (E5: a D1 edit never moves it; the reference edit
    /// walk, ADR-8818).
    pub(crate) fn is_ring_vertex(&self, vertex: u32) -> bool {
        self.ring_pins.binary_search_by_key(&vertex, |v| v.vertex).is_ok()
    }

    /// The count of lattice vertex `(vi, vj)` as stored in ground tile `tile` (at its level); `None`
    /// when that tile does not hold the vertex. A runtime block may omit the east and north borders
    /// (ADR-8246 R7); those vertices are read from the tile that owns them. A file block still stores
    /// the borders, and this lookup accepts both layouts.
    pub(crate) fn count_in_tile(&self, tile: u32, vi: u32, vj: u32) -> Option<u16> {
        if let Some(count) = self.count_stored(tile, vi, vj) {
            return Some(count);
        }
        let per_side = self.lattice.tiles_per_side();
        let (a, b) = (tile % per_side, tile / per_side);
        let (_, x1) = self.lattice.tile_cells(a);
        let (_, z1) = self.lattice.tile_cells(b);
        let last_x = a + 1 == per_side;
        let last_z = b + 1 == per_side;
        if vi == x1 && !last_x && vj == z1 && !last_z {
            return self.count_stored(tile + per_side + 1, vi, vj);
        }
        if vi == x1 && !last_x {
            return self.count_stored(tile + 1, vi, vj);
        }
        if vj == z1 && !last_z {
            return self.count_stored(tile + per_side, vi, vj);
        }
        None
    }
    fn count_stored(&self, tile: u32, vi: u32, vj: u32) -> Option<u16> {
        let per_side = self.lattice.tiles_per_side();
        let (a, b) = (tile % per_side, tile / per_side);
        let level = self.cooked_level(tile);
        let (x0, x1) = self.lattice.tile_cells(a);
        let (z0, z1) = self.lattice.tile_cells(b);
        if vi < x0 || vi > x1 || vj < z0 || vj > z1 {
            return None;
        }
        let step = 1u32 << level;
        let (di, dj) = (vi - x0, vj - z0);
        if !di.is_multiple_of(step) || !dj.is_multiple_of(step) {
            return None;
        }
        let full_cols = (x1 - x0) / step + 1;
        let full_rows = (z1 - z0) / step + 1;
        let last_x = a + 1 == per_side;
        let last_z = b + 1 == per_side;
        let owned_cols = if last_x { full_cols } else { full_cols - 1 };
        let owned_rows = if last_z { full_rows } else { full_rows - 1 };
        let block = self.ground_counts.get(tile as usize)?;
        let cols = if block.len() == (owned_cols * owned_rows) as usize {
            owned_cols
        } else if block.len() == (full_cols * full_rows) as usize {
            full_cols
        } else {
            return None;
        };
        let (di, dj) = (di / step, dj / step);
        if di >= cols {
            return None;
        }
        block.get((dj * cols + di) as usize).copied()
    }

    /// The height of lattice vertex `(vi, vj)` as ground tile `tile` stores it, metres: its override
    /// if it has one, else `offset_m + count * step_m`.
    #[must_use]
    pub fn vertex_height_in_tile(&self, tile: u32, vi: u32, vj: u32) -> Option<f32> {
        let n = self.lattice.lattice_n;
        if let Some(mm) = self.override_at(vj * n + vi) {
            return Some(mm_to_m(0, mm));
        }
        let c = self.count_in_tile(tile, vi, vj)?;
        Some(self.lattice.offset_m() + (c as f32) * self.lattice.step_m())
    }

    /// The `f32` world position of lattice vertex `(vi, vj)` with its height in `tile`.
    fn vertex_in_tile(&self, tile: u32, vi: u32, vj: u32) -> Option<[f32; 3]> {
        let y = self.vertex_height_in_tile(tile, vi, vj)?;
        Some([vertex_m(&self.lattice, vi), y, vertex_m(&self.lattice, vj)])
    }

    fn locate_axis(&self, v: f32) -> Option<(u32, u32)> {
        let cells = self.lattice.cells();
        if cells == 0 {
            return None;
        }
        let last = vertex_m(&self.lattice, cells);
        if v.is_nan() || v < self.x_first || v > last {
            return None;
        }
        let est = ((v - self.x_first) / self.pitch_m).floor();
        let mut i = if est <= 0.0 {
            0
        } else if est >= (cells - 1) as f32 {
            cells - 1
        } else {
            est as u32
        };
        let mut steps = 0;
        while i > 0 && v < vertex_m(&self.lattice, i) {
            i -= 1;
            steps += 1;
        }
        while i + 1 < cells && v >= vertex_m(&self.lattice, i + 1) {
            i += 1;
            steps += 1;
        }
        Some((i, steps))
    }

    /// The full-pitch lattice cell holding world `(x, z)` (`x` in `[x(i), x(i + 1))`, the last cell
    /// closed), or `None` off the map or for a non-finite point.
    #[must_use]
    pub fn locate(&self, x: f32, z: f32) -> Option<Located> {
        let (i, sx) = self.locate_axis(x)?;
        let (j, sz) = self.locate_axis(z)?;
        Some(Located {
            i,
            j,
            steps: [sx, sz],
        })
    }

    /// The lattice triangle of tile `tile` (at its level) that owns world `(x, z)`, with its three
    /// vertices: the coarse cell holding the point, then the diagonal's edge function from its
    /// canonical end `(x0, z1)`.
    fn triangle_in_tile(&self, tile: u32, x: f32, z: f32, cell: Located) -> Option<[[f32; 3]; 3]> {
        let per_side = self.lattice.tiles_per_side();
        let (a, b) = (tile % per_side, tile / per_side);
        let level = self.tile_level(tile);
        let step = 1u32 << level;
        let (tx0, tx1) = self.lattice.tile_cells(a);
        let (tz0, tz1) = self.lattice.tile_cells(b);
        let ci = cell.i.clamp(tx0, tx1 - 1);
        let cj = cell.j.clamp(tz0, tz1 - 1);
        let i0 = tx0 + ((ci - tx0) / step) * step;
        let j0 = tz0 + ((cj - tz0) / step) * step;
        let (i1, j1) = (i0 + step, j0 + step);
        let v00 = self.vertex_in_tile(tile, i0, j0)?;
        let v01 = self.vertex_in_tile(tile, i0, j1)?;
        let v10 = self.vertex_in_tile(tile, i1, j0)?;
        let v11 = self.vertex_in_tile(tile, i1, j1)?;
        // Edge function of the diagonal from (x0, z1) to (x1, z0); negative on the v00 side.
        let e = (v10[0] - v01[0]) * (z - v01[2]) - (v10[2] - v01[2]) * (x - v01[0]);
        Some(if e <= 0.0 {
            [v00, v01, v10]
        } else {
            [v10, v01, v11]
        })
    }

    /// Whether lattice triangle `t` of full-pitch cell `(i, j)` is cut by an open mouth.
    #[must_use]
    pub fn is_cut(&self, i: u32, j: u32, t: CellTriangle) -> bool {
        let tile = self.tile_of_cell(i, j);
        let Ok(k) = self.cuts.binary_search_by_key(&tile, |c| c.tile) else {
            return false;
        };
        let per_side = self.lattice.tiles_per_side();
        let (a, b) = (tile % per_side, tile / per_side);
        let (x0, x1) = self.lattice.tile_cells(a);
        let (z0, _) = self.lattice.tile_cells(b);
        let bit = cut_bit_index(x1 - x0, i - x0, j - z0, t);
        self.cuts[k]
            .bits
            .get((bit / 64) as usize)
            .is_some_and(|w| (w >> (bit % 64)) & 1 == 1)
    }

    /// The ground at world `(x, z)` as ground tile `tile`'s triangles draw it, ignoring the cut; for
    /// the seam check, which compares the two sides of a pitch border at one point.
    #[must_use]
    pub fn height_in_tile(&self, tile: u32, x: f32, z: f32) -> Option<f32> {
        let cell = self.locate(x, z)?;
        let tri = self.triangle_in_tile(tile, x, z, cell)?;
        plane_height(tri, x, z)
    }

    /// The ground at world `(x, z)` (spec 121 § 3 and § 5.3 rule 3): the owning lattice triangle's
    /// plane height, [`GroundAt::Cut`] where an open mouth cuts it, [`GroundAt::OffMap`] off the map.
    #[must_use]
    pub fn ground_at(&self, x: f32, z: f32) -> GroundAt {
        match self.ground_triangle_at(x, z) {
            GroundTri::Tri(tri) => match plane_height(tri, x, z) {
                Some(y) => GroundAt::Ground(y),
                None => GroundAt::OffMap,
            },
            GroundTri::Cut => GroundAt::Cut,
            GroundTri::OffMap => GroundAt::OffMap,
        }
    }

    /// The lattice triangle that owns world `(x, z)` (the one [`GroundLattice::ground_at`] takes the
    /// height of), [`GroundTri::Cut`] where an open mouth cuts it, [`GroundTri::OffMap`] off the map.
    /// The query core (lane W-11) reads the triangle for the reported normal and the segment test.
    #[must_use]
    pub fn ground_triangle_at(&self, x: f32, z: f32) -> GroundTri {
        let Some(mut cell) = self.locate(x, z) else {
            return GroundTri::OffMap;
        };
        // A point on the south edge of its cell belongs to the cell below (the triangle that walks
        // that edge in its canonical direction).
        if cell.j > 0 && z == vertex_m(&self.lattice, cell.j) {
            cell.j -= 1;
        }
        let tile = self.tile_of_cell(cell.i, cell.j);
        let Some(tri) = self.triangle_in_tile(tile, x, z, cell) else {
            return GroundTri::OffMap;
        };
        if self.tile_level(tile) == 0 {
            // The lower triangle starts at v00, which shares v01's x; the upper starts at v10.
            let t = u8::from(tri[0][0] != tri[1][0]);
            if self.is_cut(cell.i, cell.j, t) {
                return GroundTri::Cut;
            }
        }
        GroundTri::Tri(tri)
    }

    /// The two lattice triangles over full-pitch cell `(i, j)` as its ground tile draws them (at the
    /// tile's level, so a coarse tile answers its coarse cell), each with its cut bit (cut bits exist
    /// at level 0 only). `None` off the lattice. For the segment test (lane W-11).
    #[must_use]
    pub fn cell_triangles(&self, i: u32, j: u32) -> Option<[([[f32; 3]; 3], bool); 2]> {
        let cells = self.lattice.cells();
        if i >= cells || j >= cells {
            return None;
        }
        let tile = self.tile_of_cell(i, j);
        let per_side = self.lattice.tiles_per_side();
        let (a, b) = (tile % per_side, tile / per_side);
        let level = self.tile_level(tile);
        let step = 1u32 << level;
        let (tx0, _) = self.lattice.tile_cells(a);
        let (tz0, _) = self.lattice.tile_cells(b);
        let i0 = tx0 + ((i - tx0) / step) * step;
        let j0 = tz0 + ((j - tz0) / step) * step;
        let (i1, j1) = (i0 + step, j0 + step);
        let v00 = self.vertex_in_tile(tile, i0, j0)?;
        let v01 = self.vertex_in_tile(tile, i0, j1)?;
        let v10 = self.vertex_in_tile(tile, i1, j0)?;
        let v11 = self.vertex_in_tile(tile, i1, j1)?;
        let (cut0, cut1) = if level == 0 {
            (self.is_cut(i, j, 0), self.is_cut(i, j, 1))
        } else {
            (false, false)
        };
        Some([([v00, v01, v10], cut0), ([v10, v01, v11], cut1)])
    }
}

/// Drop tile-border counts the runtime must not keep (ADR-8246 R7). Each tile owns its vertices
/// except the east and north borders; the map's own east and north edges stay on the last tiles.
/// A block that is already the owned length is left alone, so a re-encoded runtime section loads
/// again. The on-disk cook still writes full borders; this runs after [`WorldGeomCooked::check`].
pub(crate) fn compact_border_counts(c: &mut WorldGeomCooked) {
    let per = c.lattice.tiles_per_side();
    for t in 0..c.ground_counts.len() {
        let level = c.tile_levels.get(t).copied().unwrap_or(0);
        let (a, b) = ((t as u32) % per, (t as u32) / per);
        let (x0, x1) = c.lattice.tile_cells(a);
        let (z0, z1) = c.lattice.tile_cells(b);
        let step = 1u32 << u32::from(level);
        if step == 0 || x1 <= x0 || z1 <= z0 {
            continue;
        }
        let full_cols = (x1 - x0) / step + 1;
        let full_rows = (z1 - z0) / step + 1;
        let last_x = a + 1 == per;
        let last_z = b + 1 == per;
        let owned_cols = if last_x { full_cols } else { full_cols - 1 };
        let owned_rows = if last_z { full_rows } else { full_rows - 1 };
        let owned_len = (owned_cols * owned_rows) as usize;
        let full_len = (full_cols * full_rows) as usize;
        if c.ground_counts[t].len() == owned_len {
            c.ground_counts[t].shrink_to_fit();
            continue;
        }
        if c.ground_counts[t].len() != full_len {
            continue;
        }
        let full = std::mem::take(&mut c.ground_counts[t]);
        let mut owned = Vec::with_capacity(owned_len);
        for row in 0..owned_rows {
            let start = (row * full_cols) as usize;
            owned.extend_from_slice(&full[start..start + owned_cols as usize]);
        }
        c.ground_counts[t] = owned;
    }
}

/// The lattice triangle that owns a point (see [`GroundLattice::ground_triangle_at`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GroundTri {
    /// The owning triangle `[a, b, c]` in the lattice's winding (`[v00, v01, v10]` or
    /// `[v10, v01, v11]`), `f32` metres.
    Tri([[f32; 3]; 3]),
    /// An open mouth cuts the lattice triangle under the point.
    Cut,
    /// The point is off the map (or not finite).
    OffMap,
}

/// The height of the plane through `tri` at `(x, z)` in spec 121 § 5.3 rule 2's order:
/// `n = (b - a) x (c - a)` unnormalized, `y = a.y - (n.x * (x - a.x) + n.z * (z - a.z)) / n.y`.
/// `None` for a vertical triangle (`|n.y| < 1e-6`).
#[must_use]
pub fn plane_height(tri: [[f32; 3]; 3], x: f32, z: f32) -> Option<f32> {
    let [a, b, c] = tri;
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let nx = u[1] * v[2] - u[2] * v[1];
    let ny = u[2] * v[0] - u[0] * v[2];
    let nz = u[0] * v[1] - u[1] * v[0];
    if ny.abs() < 1.0e-6 {
        return None;
    }
    Some(a[1] - (nx * (x - a[0]) + nz * (z - a[2])) / ny)
}
