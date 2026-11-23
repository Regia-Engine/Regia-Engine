// SPDX-License-Identifier: MPL-2.0
//! The fine column directory, the box table and the per-hit evaluation of the native column
//! queries (`support`, `floors_at`, `surface_at`, the roof lookups; spec 121 § 5.3, ADR-8817).
//!
//! The column grid of [`crate::query_index`] files floors and roofs in `index_cell_m` cells (16 m
//! by default), found by a binary search; one cell lists every floor and roof of a tunnel reach, and
//! each was converted to metres and boxed against the point on every call. This module adds:
//!
//! - **A directory of 8 m cells** over the floors and roofs: 8 x 8 cells per block, one presence
//!   mask and one base per block, one offset per occupied cell, and per cell the positions of its
//!   triangles in `TriRef` order (the order the column grid lists them in). A lookup is two
//!   multiplies, two table reads and a population count. A triangle is filed in every cell its XZ
//!   box touches; `cell_of` is monotone, so a point inside the box is always in one of those cells.
//! - **The XZ box of every floor and roof** in `f32` metres ([`ColumnBox`], 20 bytes): the bounds
//!   `Plane::new` computes, so a candidate is boxed with four comparisons and no conversion.
//! - **One evaluation per box hit** ([`NearTri`]): its canonical edges, where the point sits on
//!   each, and its plane, built once per call and without a data-dependent branch per edge.
//!
//! None of them changes an answer: the directory lists a superset of the triangles whose box
//! holds the point, in the same relative order; the box is the same closed box; the evaluation is
//! the same arithmetic. The queries keep the per-call form (`WorldGeom::crossings_reference`) for a
//! column with more box hits or floors than the fast path's fixed buffers hold, and the tests
//! compare the two bit for bit.

use crate::cooked::{CookedTriangle, TriKind};
use crate::geom::GeomTriangle;
use crate::query_index::{cross_n, plane_valid};
use crate::units::mm_to_m;
use regia_detsim::float::{det_max, det_min};

/// The directory's cell side before any coarsening, metres. A power of two, so `v * (1 / side)`
/// is exact.
const FINE_M: f32 = 8.0;
/// Cells per block side (64 cells per block: one `u64` mask).
const BLOCK_SHIFT: u32 = 3;
/// The most blocks the directory may hold; a wider map doubles its cell side until it fits.
const MAX_BLOCKS: u64 = 4096;
/// A cell coordinate past this is outside any cooked map (`MAX_EXTENT_MM` over the cell side).
const MAX_CELL: f32 = 8_388_608.0;
/// The most box filings (one per box per cell its box touches) the directory holds: the column
/// grid's own reference cap (`Grid::build`). Past it the directory disables and the queries read the
/// column grid, so the lists stay at most 32 MB (narrow) or 64 MB (wide) and their `u32`
/// positions cannot wrap (ADR-8884).
const MAX_FILED: u64 = 16_000_000;

/// The XZ box of one floor or roof, `f32` metres (the bounds `Plane::new` computes), and its id.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColumnBox {
    min_x: f32,
    max_x: f32,
    min_z: f32,
    max_z: f32,
    /// The triangle's packed id (`tile << 16 | local`).
    pub(crate) id: u32,
}

impl ColumnBox {
    /// `Plane::bounds_contain`: the closed box holds the point.
    #[inline]
    pub(crate) fn holds(&self, x: f32, z: f32) -> bool {
        (x >= self.min_x) & (x <= self.max_x) & (z >= self.min_z) & (z <= self.max_z)
    }
}

/// The boxes filed in one directory cell, as positions in [`ColumnIndex::boxes`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum CellList<'a> {
    /// At most 65 536 floors and roofs on the map.
    Narrow(&'a [u16]),
    /// More.
    Wide(&'a [u32]),
}

/// The directory of fine column cells (see the module docs).
#[derive(Clone, Debug)]
pub(crate) struct ColumnIndex {
    /// `1 / side`, exact (the side is a power of two). Zero when the directory is disabled.
    inv_side: f32,
    /// The cell coordinates of the directory's first cell.
    origin: [i32; 2],
    /// Blocks along x and z.
    blocks: [u32; 2],
    /// Per block (`bz * blocks[0] + bx`): the occupied cells, bit `cz * 8 + cx`.
    masks: Vec<u64>,
    /// Per block: the index in `cells` of its first occupied cell.
    bases: Vec<u32>,
    /// Per occupied cell, in block then bit order: the start of its list. One sentinel at the end.
    cells: Vec<u32>,
    /// Every occupied cell's box positions, each cell ascending (`TriRef` order); 16-bit when the
    /// map has at most 65 536 floors and roofs, else in `wide`.
    narrow: Vec<u16>,
    wide: Vec<u32>,
    /// The floors and roofs in `TriRef` order.
    boxes: Vec<ColumnBox>,
}

impl ColumnIndex {
    /// A directory that answers nothing; the queries then read the column grid.
    fn disabled() -> Self {
        Self {
            inv_side: 0.0,
            origin: [0; 2],
            blocks: [0; 2],
            masks: Vec::new(),
            bases: Vec::new(),
            cells: Vec::new(),
            narrow: Vec::new(),
            wide: Vec::new(),
            boxes: Vec::new(),
        }
    }

    /// Whether the directory was built. A map with no floor or roof, whose floors and roofs do
    /// not fit the block bound even at the widest cell, or whose boxes file more than `MAX_FILED`
    /// times, has none.
    #[inline]
    pub(crate) fn enabled(&self) -> bool {
        self.inv_side > 0.0
    }

    /// The cell coordinate of `v` along one axis: `floor(v * inv_side)`, saturating, monotone in
    /// `v`. Truncation and one comparison, so no libm call on targets without a rounding instruction.
    #[inline]
    fn cell_of(inv_side: f32, v: f32) -> i32 {
        let scaled = v * inv_side;
        let truncated = scaled as i32;
        truncated.saturating_sub(i32::from(scaled < truncated as f32))
    }

    /// Files every floor and roof of `tris` (`ids[i]` is the packed id of `tris[i]`, in `TriRef`
    /// order) in the cells its XZ box touches. Load time; allocates.
    pub(crate) fn build(tris: &[GeomTriangle], ids: &[u32]) -> Self {
        let mut boxes: Vec<ColumnBox> = Vec::new();
        for (tri, &id) in tris.iter().zip(ids) {
            if tri.kind != TriKind::Floor && tri.kind != TriKind::Roof {
                continue;
            }
            // A cooked vertex is an integer millimetre, so this never fires on a loaded map.
            if tri.v.iter().any(|p| !p[0].is_finite() || !p[2].is_finite()) {
                return Self::disabled();
            }
            let [a, b, c] = tri.v;
            boxes.push(ColumnBox {
                min_x: det_min(a[0], det_min(b[0], c[0])),
                max_x: det_max(a[0], det_max(b[0], c[0])),
                min_z: det_min(a[2], det_min(b[2], c[2])),
                max_z: det_max(a[2], det_max(b[2], c[2])),
                id,
            });
        }
        if boxes.is_empty() {
            return Self::disabled();
        }
        boxes.shrink_to_fit();
        let mut side = FINE_M;
        let (inv_side, origin, blocks) = loop {
            let inv = 1.0 / side;
            let mut lo = [i32::MAX; 2];
            let mut hi = [i32::MIN; 2];
            for b in &boxes {
                if [b.min_x, b.max_x, b.min_z, b.max_z]
                    .iter()
                    .any(|v| (v * inv).abs() > MAX_CELL)
                {
                    return Self::disabled();
                }
                lo = [
                    lo[0].min(Self::cell_of(inv, b.min_x)),
                    lo[1].min(Self::cell_of(inv, b.min_z)),
                ];
                hi = [
                    hi[0].max(Self::cell_of(inv, b.max_x)),
                    hi[1].max(Self::cell_of(inv, b.max_z)),
                ];
            }
            let span = |axis: usize| ((i64::from(hi[axis]) - i64::from(lo[axis])) >> BLOCK_SHIFT) as u64 + 1;
            let (bx, bz) = (span(0), span(1));
            if bx * bz <= MAX_BLOCKS {
                break (inv, lo, [bx as u32, bz as u32]);
            }
            side *= 2.0;
            if side > 65_536.0 {
                return Self::disabled();
            }
        };
        let per_block = 1usize << BLOCK_SHIFT;
        let width = blocks[0] as usize * per_block;
        let depth = blocks[1] as usize * per_block;
        // The cell range of a box, relative to the origin.
        let range = |b: &ColumnBox| {
            let x0 = (Self::cell_of(inv_side, b.min_x) - origin[0]) as usize;
            let x1 = (Self::cell_of(inv_side, b.max_x) - origin[0]) as usize;
            let z0 = (Self::cell_of(inv_side, b.min_z) - origin[1]) as usize;
            let z1 = (Self::cell_of(inv_side, b.max_z) - origin[1]) as usize;
            (x0, x1, z0, z1)
        };
        // The filings, counted before any list is sized: past `MAX_FILED` the directory disables
        // (ADR-8884), the way it does past `MAX_CELL` or the block bound.
        let mut filed = 0u64;
        for b in &boxes {
            let (x0, x1, z0, z1) = range(b);
            filed += (x1 - x0 + 1) as u64 * (z1 - z0 + 1) as u64;
            if filed > MAX_FILED {
                return Self::disabled();
            }
        }
        // Pass 1: boxes per cell. Pass 2: the directory in block then bit order. Pass 3: the
        // positions, in input order, so each cell keeps `TriRef` order.
        let mut cursor = vec![0u32; width * depth];
        for b in &boxes {
            let (x0, x1, z0, z1) = range(b);
            for z in z0..=z1 {
                for slot in &mut cursor[z * width + x0..=z * width + x1] {
                    *slot += 1;
                }
            }
        }
        let block_count = blocks[0] as usize * blocks[1] as usize;
        let mut masks = Vec::with_capacity(block_count);
        let mut bases = Vec::with_capacity(block_count);
        let occupied = cursor.iter().filter(|&&n| n > 0).count();
        let mut cells = Vec::with_capacity(occupied + 1);
        let mut total = 0u32;
        for bz in 0..blocks[1] as usize {
            for bx in 0..blocks[0] as usize {
                let mut mask = 0u64;
                bases.push(cells.len() as u32);
                for cz in 0..per_block {
                    for cx in 0..per_block {
                        let slot = &mut cursor[(bz * per_block + cz) * width + bx * per_block + cx];
                        let n = *slot;
                        // From a count to the cell's write position.
                        *slot = total;
                        if n > 0 {
                            mask |= 1 << (cz * per_block + cx);
                            cells.push(total);
                            total += n;
                        }
                    }
                }
                masks.push(mask);
            }
        }
        cells.push(total);
        let narrow_fits = boxes.len() <= usize::from(u16::MAX) + 1;
        let mut narrow = vec![0u16; if narrow_fits { total as usize } else { 0 }];
        let mut wide = vec![0u32; if narrow_fits { 0 } else { total as usize }];
        for (position, b) in boxes.iter().enumerate() {
            let (x0, x1, z0, z1) = range(b);
            for z in z0..=z1 {
                for slot in &mut cursor[z * width + x0..=z * width + x1] {
                    if narrow_fits {
                        narrow[*slot as usize] = position as u16;
                    } else {
                        wide[*slot as usize] = position as u32;
                    }
                    *slot += 1;
                }
            }
        }
        Self {
            inv_side,
            origin,
            blocks,
            masks,
            bases,
            cells,
            narrow,
            wide,
            boxes,
        }
    }

    /// The boxes filed in the cell holding `(x, z)`, ascending (`TriRef` order): every floor and
    /// roof whose XZ box holds the point, and some whose box does not. `x` and `z` must be finite
    /// and the directory enabled.
    #[inline]
    pub(crate) fn at(&self, x: f32, z: f32) -> CellList<'_> {
        const EMPTY: CellList<'static> = CellList::Narrow(&[]);
        let cx = i64::from(Self::cell_of(self.inv_side, x)) - i64::from(self.origin[0]);
        let cz = i64::from(Self::cell_of(self.inv_side, z)) - i64::from(self.origin[1]);
        let width = i64::from(self.blocks[0]) << BLOCK_SHIFT;
        let depth = i64::from(self.blocks[1]) << BLOCK_SHIFT;
        if cx < 0 || cz < 0 || cx >= width || cz >= depth {
            return EMPTY;
        }
        let (cx, cz) = (cx as usize, cz as usize);
        let block = (cz >> BLOCK_SHIFT) * self.blocks[0] as usize + (cx >> BLOCK_SHIFT);
        let low = (1usize << BLOCK_SHIFT) - 1;
        let bit = ((cz & low) << BLOCK_SHIFT) | (cx & low);
        let mask = self.masks[block];
        if (mask >> bit) & 1 == 0 {
            return EMPTY;
        }
        let cell = self.bases[block] as usize + (mask & ((1u64 << bit) - 1)).count_ones() as usize;
        let range = self.cells[cell] as usize..self.cells[cell + 1] as usize;
        if self.wide.is_empty() {
            CellList::Narrow(&self.narrow[range])
        } else {
            CellList::Wide(&self.wide[range])
        }
    }

    /// The box at a position a [`CellList`] names.
    #[inline]
    pub(crate) fn boxed(&self, position: u32) -> &ColumnBox {
        &self.boxes[position as usize]
    }

    pub(crate) fn resident_bytes(&self) -> usize {
        use std::mem::size_of;
        self.masks.capacity() * size_of::<u64>()
            + (self.bases.capacity() + self.cells.capacity() + self.wide.capacity()) * size_of::<u32>()
            + self.narrow.capacity() * size_of::<u16>()
            + self.boxes.capacity() * size_of::<ColumnBox>()
    }
}

/// `Edge::on_segment` for a point on the edge's line: whether `t = num / len2` is in `0..=1`,
/// where `len2` is the edge's squared length and `num` the point's projection on it.
///
/// For a squared length in the normal range the quotient is not needed: `t >= 0` exactly when
/// `num >= 0` (a negative `num` gives a negative quotient unless it underflows to `-0`, which the
/// `-1e-30` guard and the `1e12` bound exclude), and `t <= 1` exactly when `num <= len2` (the
/// next `f32` above `len2` is more than `len2 * (1 + 2^-24)`, so its quotient rounds above 1).
/// Outside those ranges the division runs, as in `Edge::on_segment`. A point on an edge's line is
/// the common case for a body standing on a mesh vertex or seam (ADR-8817).
#[inline]
fn on_segment(len2: f32, num: f32) -> bool {
    if (1.0e-30..=1.0e12).contains(&len2) && !(num < 0.0 && num > -1.0e-30) {
        (num >= 0.0) & (num <= len2)
    } else {
        len2 != 0.0 && (0.0..=1.0).contains(&(num / len2))
    }
}

/// `if first { a } else { b }` on the bits, in registers: no branch on the triangle's shape.
#[inline(always)]
fn pick(first: bool, a: f32, b: f32) -> f32 {
    let mask = u32::from(first).wrapping_neg();
    f32::from_bits((a.to_bits() & mask) | (b.to_bits() & !mask))
}

/// The edge after edge `i` and the vertex opposite edge `i`, as `Plane::new` pairs them.
const NEXT: [usize; 3] = [1, 2, 0];
const THIRD: [usize; 3] = [2, 0, 1];

/// One box hit of a column query, evaluated at the query point: what `Plane::new` and
/// `Plane::crossing` of [`crate::query_index`] compute for it, once per call, without the box
/// (already tested) and without a data-dependent branch per edge.
///
/// A point on a shared edge or vertex asks, for every triangle around it, whether another box hit
/// owns that edge. The per-call form rebuilt the asked triangle's plane for each question; here
/// each hit carries its canonical edges and whether it holds the point, so a question is a few
/// comparisons (ADR-8817). The arithmetic is `Edge::new`, `Edge::relation`, `cross_n` and
/// `Plane::height`, expression for expression; only the control flow differs (selects for the
/// canonical end and the half-plane, bit masks for the three relations).
#[derive(Clone, Copy)]
pub(crate) struct NearTri {
    /// Per edge: the canonical lower end `[x, z]` and the vector to the other end.
    lo: [[f32; 2]; 3],
    delta: [[f32; 2]; 3],
    n: [f32; 3],
    anchor: [f32; 3],
    /// Bit `i`: edge `i` walks its segment in the owning direction (`side < 0`).
    owner: u8,
    /// Bit `i`: the point is on edge `i`'s segment and the edge does not own it.
    unowned: u8,
    /// `Plane::contains_closed` at the query point: a valid plane and no edge with the point
    /// outside (the box holds the point by construction).
    closed: bool,
    /// The triangle's kind.
    pub(crate) kind: TriKind,
    /// Its `TRI_FLAG_*` bits.
    pub(crate) flags: u8,
}

impl NearTri {
    /// A slot no query reads.
    pub(crate) const EMPTY: NearTri = NearTri {
        lo: [[0.0; 2]; 3],
        delta: [[0.0; 2]; 3],
        n: [0.0; 3],
        anchor: [0.0; 3],
        owner: 0,
        unowned: 0,
        closed: false,
        kind: TriKind::Wall,
        flags: 0,
    };

    /// The hit for cooked triangle `t` of a tile at `origin`, at the query point `(x, z)`.
    /// Always inlined: the caller's slot is written in place.
    ///
    /// A hit with the point outside an edge is not closed: it neither crosses nor owns an edge, so
    /// nothing but `closed`, `kind` and `flags` is read from it, and its heights, its plane and its
    /// edge records are not built.
    #[inline(always)]
    pub(crate) fn at(origin: [i32; 2], t: &CookedTriangle, x: f32, z: f32) -> Self {
        // XZ of the three vertices; the heights only for a hit that holds the point.
        let xz = |p: &[i32; 3]| [mm_to_m(origin[0], p[0]), mm_to_m(origin[1], p[2])];
        let v = [xz(&t.v[0]), xz(&t.v[1]), xz(&t.v[2])];
        let mut lo = [[0.0f32; 2]; 3];
        let mut delta = [[0.0f32; 2]; 3];
        let (mut owner, mut unowned, mut outside) = (0u8, 0u8, false);
        for i in 0..3 {
            let (a, b, c) = (v[i], v[NEXT[i]], v[THIRD[i]]);
            // `Edge::new`: the lexicographically smaller XZ end first.
            let a_first = (a[0] < b[0]) | ((a[0] == b[0]) & (a[1] < b[1]));
            let l = [pick(a_first, a[0], b[0]), pick(a_first, a[1], b[1])];
            let h = [pick(a_first, b[0], a[0]), pick(a_first, b[1], a[1])];
            let (dx, dz) = (h[0] - l[0], h[1] - l[1]);
            let side = dx * (c[1] - l[1]) - dz * (c[0] - l[0]);
            // `Edge::relation`.
            let e = dx * (z - l[1]) - dz * (x - l[0]);
            let on = e == 0.0 && on_segment(dx * dx + dz * dz, (x - l[0]) * dx + (z - l[1]) * dz);
            let owns = side < 0.0;
            let inside = if owns { e <= 0.0 } else { e > 0.0 };
            let degenerate = side == 0.0;
            outside |= degenerate | (!on & !inside);
            owner |= u8::from(owns) << i;
            unowned |= u8::from(!degenerate & on & !owns) << i;
            lo[i] = l;
            delta[i] = [dx, dz];
        }
        if outside {
            return Self {
                kind: t.kind,
                flags: t.flags,
                ..Self::EMPTY
            };
        }
        let y = |p: &[i32; 3]| mm_to_m(0, p[1]);
        let full = [
            [v[0][0], y(&t.v[0]), v[0][1]],
            [v[1][0], y(&t.v[1]), v[1][1]],
            [v[2][0], y(&t.v[2]), v[2][1]],
        ];
        let n = cross_n(full);
        Self {
            lo,
            delta,
            n,
            anchor: full[0],
            owner,
            unowned,
            closed: plane_valid(n),
            kind: t.kind,
            flags: t.flags,
        }
    }

    /// Whether this hit holds the query point and owns edge `(lo, delta)` (`Plane::contains_closed`
    /// and `Edge::claims_directed`).
    #[inline]
    fn claims(&self, kind: TriKind, lo: [f32; 2], delta: [f32; 2]) -> bool {
        self.kind == kind
            && self.closed
            && (0..3).any(|j| {
                (self.owner >> j) & 1 == 1
                    && self.lo[j][0].to_bits() == lo[0].to_bits()
                    && self.lo[j][1].to_bits() == lo[1].to_bits()
                    && self.delta[j][0].to_bits() == delta[0].to_bits()
                    && self.delta[j][1].to_bits() == delta[1].to_bits()
            })
    }

    /// The unnormalized plane normal (`cross_n` of the triangle), for the reported unit normal.
    #[inline]
    pub(crate) fn plane_n(&self) -> [f32; 3] {
        self.n
    }

    /// `Plane::crossing`: the plane height at `(x, z)` when this triangle holds the point and owns
    /// every edge the point is on. `hits` are the box hits of the call (this one among them); the
    /// clockwise triangle of the same kind that also holds the point owns a shared edge.
    #[inline]
    pub(crate) fn crossing(&self, x: f32, z: f32, hits: &[NearTri]) -> Option<f32> {
        if !self.closed {
            return None;
        }
        if self.unowned != 0 {
            for i in 0..3 {
                if (self.unowned >> i) & 1 == 1
                    && hits
                        .iter()
                        .any(|other| other.claims(self.kind, self.lo[i], self.delta[i]))
                {
                    return None;
                }
            }
        }
        let a = self.anchor;
        Some(a[1] - (self.n[0] * (x - a[0]) + self.n[2] * (z - a[2])) / self.n[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TriRef;

    /// A small deterministic generator (SplitMix64).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn range(&mut self, lo: i32, hi: i32) -> i32 {
            lo + (self.next() % (hi - lo + 1) as u64) as i32
        }
    }

    fn tri(kind: TriKind, index: u32, v: [[f32; 3]; 3]) -> GeomTriangle {
        GeomTriangle {
            v,
            tri: TriRef {
                layer: 0,
                triangle: index,
            },
            kind,
            flags: 0,
            section: None,
            material: 0,
        }
    }

    fn listed(index: &ColumnIndex, x: f32, z: f32) -> Vec<u32> {
        match index.at(x, z) {
            CellList::Narrow(list) => list.iter().map(|&p| index.boxed(u32::from(p)).id).collect(),
            CellList::Wide(list) => list.iter().map(|&p| index.boxed(p).id).collect(),
        }
    }

    /// ADR-8817: the division-free segment test is `Edge::on_segment`'s `0 <= num / len2 <= 1` for
    /// every pair: equal values, neighbours one ulp either side of the ends, zeros of both signs,
    /// subnormals, values past the guarded range, infinities and NaN.
    #[test]
    fn on_segment_is_the_quotient_test() {
        let by_quotient = |len2: f32, num: f32| len2 != 0.0 && (0.0..=1.0).contains(&(num / len2));
        let mut values: Vec<f32> = vec![
            0.0,
            -0.0,
            f32::MIN_POSITIVE,
            1.0e-45,
            1.0e-40,
            1.0e-31,
            1.0e-30,
            1.0e-29,
            1.0e-9,
            3.6e-9,
            0.001,
            0.5,
            1.0,
            2.0,
            64.0,
            1.0e6,
            1.0e12,
            1.1e12,
            1.0e30,
            f32::MAX,
            f32::INFINITY,
            f32::NAN,
        ];
        let mut rng = Rng(17);
        for _ in 0..400 {
            values.push((rng.range(1, 2_000_000_000) as f32) * 1.0e-6);
        }
        let mut all = Vec::new();
        for v in values {
            all.extend([v, -v, v.next_up(), v.next_down(), -v.next_up(), -v.next_down()]);
        }
        let mut seen = [0u32; 2];
        for &len2 in &all {
            if len2 < 0.0 {
                continue;
            }
            for &num in &all {
                let want = by_quotient(len2, num);
                assert_eq!(on_segment(len2, num), want, "len2={len2:e} num={num:e}");
                seen[usize::from(want)] += 1;
            }
            // The ends of the segment, one ulp around each.
            for num in [
                len2,
                len2.next_up(),
                len2.next_down(),
                len2 * 0.5,
                len2 * 1.000_000_1,
            ] {
                assert_eq!(
                    on_segment(len2, num),
                    by_quotient(len2, num),
                    "len2={len2:e} num={num:e}"
                );
            }
        }
        assert!(seen[0] > 100_000 && seen[1] > 100_000, "{seen:?}");
    }

    /// ADR-8817: the truncating floor is `floor`, saturating, for cell coordinates of any size.
    #[test]
    fn cell_of_is_floor() {
        for v in [
            0.0f32, -0.0, 0.5, -0.5, 7.999, 8.0, -8.0, -8.001, 16.0, -1e-30, 1e-30, 639.99, -640.0, 1.0e9,
            -1.0e9, 3.0e38, -3.0e38,
        ] {
            for inv in [0.125f32, 0.25, 1.0 / 64.0] {
                let want = (v * inv).floor() as i32;
                assert_eq!(ColumnIndex::cell_of(inv, v), want, "v={v} inv={inv}");
            }
        }
        let mut rng = Rng(1);
        for _ in 0..200_000 {
            let v = rng.range(-16_000_000, 16_000_000) as f32 / 1000.0;
            assert_eq!(
                ColumnIndex::cell_of(0.125, v),
                (v * 0.125).floor() as i32,
                "v={v}"
            );
        }
    }

    /// ADR-8817: a cell lists every floor and roof whose XZ box holds the point, in input order,
    /// and never a wall; `ColumnBox::holds` is the closed box. Boxes straddle cell and block
    /// borders, negative coordinates and the directory's edge.
    #[test]
    fn directory_lists_every_box_holding_the_point_in_order() {
        let mut rng = Rng(3);
        let mut tris = Vec::new();
        for i in 0..600u32 {
            let c = [rng.range(-300_000, 300_000), rng.range(-150_000, 500_000)];
            let v: [[f32; 3]; 3] = std::array::from_fn(|_| {
                [
                    mm_to_m(0, c[0] + rng.range(-7_000, 7_000)),
                    mm_to_m(0, rng.range(-20_000, 20_000)),
                    mm_to_m(0, c[1] + rng.range(-7_000, 7_000)),
                ]
            });
            let kind = match i % 5 {
                0 | 1 => TriKind::Floor,
                2 | 3 => TriKind::Roof,
                _ => TriKind::Wall,
            };
            tris.push(tri(kind, i, v));
        }
        let ids: Vec<u32> = (0..tris.len() as u32).map(|i| ((i % 3) << 16) | i).collect();
        let index = ColumnIndex::build(&tris, &ids);
        assert!(index.enabled());
        let inside = |t: &GeomTriangle, x: f32, z: f32| {
            let xs = t.v.map(|p| p[0]);
            let zs = t.v.map(|p| p[2]);
            x >= det_min(xs[0], det_min(xs[1], xs[2]))
                && x <= det_max(xs[0], det_max(xs[1], xs[2]))
                && z >= det_min(zs[0], det_min(zs[1], zs[2]))
                && z <= det_max(zs[0], det_max(zs[1], zs[2]))
        };
        let mut points = Vec::new();
        for t in &tris {
            for p in t.v {
                points.push((p[0], p[2]));
                points.push((p[0].next_up(), p[2].next_down()));
            }
        }
        for k in -90..=90 {
            // Cell and block borders.
            points.push((k as f32 * 8.0, k as f32 * 8.0 - 64.0));
            points.push(((k as f32 * 8.0).next_down(), (k as f32 * 64.0).next_down()));
        }
        points.extend([
            (1e9, 0.0),
            (-1e9, -1e9),
            (0.0, 3.0e38),
            (-400.0, 0.0),
            (0.0, 600.0),
        ]);
        let mut held = 0usize;
        for (x, z) in points {
            let cell = listed(&index, x, z);
            let want: Vec<u32> = tris
                .iter()
                .zip(&ids)
                .filter(|(t, _)| t.kind != TriKind::Wall && inside(t, x, z))
                .map(|(_, &id)| id)
                .collect();
            let got: Vec<u32> = cell.iter().copied().filter(|id| want.contains(id)).collect();
            assert_eq!(got, want, "x={x} z={z}");
            let order: Vec<u32> = cell.iter().map(|id| id & 0xffff).collect();
            assert!(order.windows(2).all(|w| w[0] < w[1]), "x={x} z={z}: {order:?}");
            assert!(
                cell.iter()
                    .all(|id| tris[(id & 0xffff) as usize].kind != TriKind::Wall)
            );
            // The stored box is the closed f32 box.
            let by_box: Vec<u32> = index
                .boxes
                .iter()
                .filter(|b| b.holds(x, z))
                .map(|b| b.id)
                .collect();
            assert_eq!(by_box, want, "x={x} z={z}");
            held += want.len();
        }
        assert!(held > 1_500, "{held}");
    }

    /// A map wider than the block bound at 8 m cells doubles the cell side; one with no floor or
    /// roof, or a non-finite vertex, has no directory; more than 65 536 boxes use the wide lists.
    #[test]
    fn directory_coarsens_widens_or_disables() {
        let flat = |x: f32, z: f32| [[x, 0.0, z], [x + 2.0, 0.0, z], [x, 0.0, z + 2.0]];
        let far = [
            tri(TriKind::Floor, 0, flat(-8_000.0, -8_000.0)),
            tri(TriKind::Roof, 1, flat(8_000.0, 8_000.0)),
        ];
        let index = ColumnIndex::build(&far, &[0, 1]);
        assert!(index.enabled());
        assert!(index.inv_side < 1.0 / FINE_M);
        assert!(u64::from(index.blocks[0]) * u64::from(index.blocks[1]) <= MAX_BLOCKS);
        assert_eq!(listed(&index, -7_999.0, -7_999.5), [0]);
        assert_eq!(listed(&index, 8_001.0, 8_000.5), [1]);
        assert!(listed(&index, 0.0, 0.0).is_empty());
        assert!(!ColumnIndex::build(&[], &[]).enabled());
        assert!(!ColumnIndex::build(&[tri(TriKind::Wall, 0, flat(0.0, 0.0))], &[0]).enabled());
        let mut nan = flat(0.0, 0.0);
        nan[1][0] = f32::NAN;
        assert!(!ColumnIndex::build(&[tri(TriKind::Floor, 0, nan)], &[0]).enabled());
        let many: Vec<GeomTriangle> = (0..70_000u32)
            .map(|i| {
                tri(
                    TriKind::Floor,
                    i,
                    flat((i % 300) as f32 * 2.0, (i / 300) as f32 * 2.0),
                )
            })
            .collect();
        let ids: Vec<u32> = (0..70_000).collect();
        let index = ColumnIndex::build(&many, &ids);
        assert!(index.narrow.is_empty() && !index.wide.is_empty());
        // Box 301 spans x 2..4, z 2..4; its neighbours touch the point too.
        let cell = listed(&index, 3.0, 3.0);
        assert!(
            cell.contains(&301) && cell.windows(2).all(|w| w[0] < w[1]),
            "{cell:?}"
        );
    }

    /// An occupancy sum that does not fit in `u32` disables the directory.
    ///
    /// 16 384 floors each cover all 512 by 512 cells of an 8 m directory (vertices at 0 m and
    /// 4088 m: 64 by 64 blocks, the block cap, so the cell side stays 8 m). The sum is 2^32.
    /// `QueryIndex::build` still accepts the map: `index_cell_m` of 8192 files each floor in four
    /// column cells, 65 536 references, under the 16 million cap in `Grid::build`. The directory
    /// must disable (the column grid then answers), the same way a map past `MAX_CELL` or the
    /// block bound disables, and must not panic on the wrapping add.
    #[test]
    fn directory_disables_instead_of_overflowing_its_u32_lists() {
        const COUNT: u32 = 16_384;
        const SPAN: f32 = 4088.0;
        let tris: Vec<GeomTriangle> = (0..COUNT)
            .map(|i| {
                tri(
                    TriKind::Floor,
                    i,
                    [[0.0, 0.0, 0.0], [SPAN, 0.0, 0.0], [0.0, 0.0, SPAN]],
                )
            })
            .collect();
        let ids: Vec<u32> = (0..COUNT).collect();
        let settings = crate::GeomSettings {
            index_cell_m: 8192.0,
            ..crate::GeomSettings::default()
        };
        let index = crate::query_index::QueryIndex::build(&tris, &ids, &settings)
            .expect("column references stay under the 16 million cap");
        assert!(
            !index.fine.enabled(),
            "an occupancy of 2^32 must disable the directory"
        );
    }

    /// The filing cap is exact: 61 floors that each cover all 262 144 cells of the 8 m directory
    /// (15 990 784 filings) keep it, and every cell lists all 61 in order; 62 (16 252 928) and the
    /// 16 383 of the review's neighbor case (about 8 GB of `u16`) disable it without sizing a list.
    #[test]
    fn directory_keeps_its_filing_cap() {
        const SPAN: f32 = 4088.0;
        let floors = |count: u32| -> (Vec<GeomTriangle>, Vec<u32>) {
            let tris = (0..count)
                .map(|i| {
                    tri(
                        TriKind::Floor,
                        i,
                        [[0.0, 0.0, 0.0], [SPAN, 0.0, 0.0], [0.0, 0.0, SPAN]],
                    )
                })
                .collect();
            (tris, (0..count).collect())
        };
        let (tris, ids) = floors(61);
        let kept = ColumnIndex::build(&tris, &ids);
        assert!(kept.enabled(), "15 990 784 filings are under the cap");
        assert_eq!(kept.narrow.len(), 61 * 512 * 512);
        let all: Vec<u32> = (0..61).collect();
        for (x, z) in [(0.0, 0.0), (4087.5, 0.25), (2000.0, 2000.0), (0.5, 4087.9)] {
            assert_eq!(listed(&kept, x, z), all, "cell at ({x}, {z})");
        }
        for count in [62, 16_383] {
            let (tris, ids) = floors(count);
            let index = ColumnIndex::build(&tris, &ids);
            assert!(!index.enabled(), "{count} floors pass the filing cap");
            assert!(index.narrow.is_empty() && index.wide.is_empty() && index.boxes.is_empty());
        }
    }
}
