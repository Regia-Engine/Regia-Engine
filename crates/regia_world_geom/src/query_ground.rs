// SPDX-License-Identifier: MPL-2.0
//! Lattice point location for native ground queries (spec 121 §3).
//!
//! Since ADR-8817 the axis positions (`vertex_m` of the cooked lattice) are kept in one table of
//! `lattice_n` values, and each ground tile has a flag byte that says whether a ring pin, a hanging
//! vertex or a cut bitmap can apply inside it. The table holds the values `vertex_m` returns and the
//! flags only skip searches that would find nothing, so every answer keeps its bits (ADR-8246 R7
//! recomputed both per call, for memory; spec 121 § 11's 35 MB has room for about 20 KB).

use crate::cooked::{VertexHeightMm, WorldGeomCooked};
use crate::ground::{GroundTri, cut_bit_index, vertex_m};

/// A ring pin lies in the tile's closed vertex range.
const TILE_HAS_PIN: u8 = 1;
/// A hanging-vertex override lies in the tile's closed vertex range.
const TILE_HAS_HANGING: u8 = 2;
/// The tile has a cut bitmap.
const TILE_HAS_CUT: u8 = 4;

#[derive(Clone, Debug)]
pub(crate) struct GroundCoordinates {
    pitch: f32,
    cells: u32,
    tile_shift: u32,
    per_side: u32,
    /// `vertex_m(lattice, i)` for `i` in `0..lattice_n`.
    axis: Vec<f32>,
    /// `TILE_HAS_*` bits per ground tile (`b * per_side + a`).
    tile_flags: Vec<u8>,
}

impl GroundCoordinates {
    pub(crate) fn new(cooked: &WorldGeomCooked) -> Self {
        let l = &cooked.lattice;
        let cells = l.lattice_n - 1;
        let tile_shift = u32::from(l.ground_tile_cells).trailing_zeros();
        let per_side = l.tiles_per_side();
        let axis: Vec<f32> = (0..l.lattice_n).map(|i| vertex_m(l, i)).collect();
        let mut tile_flags = vec![0u8; (per_side as usize) * (per_side as usize)];
        // The tiles whose closed vertex range `[a * g, min(a * g + g, cells)]` holds index `v`.
        let holders = |v: u32| {
            let hi = v >> tile_shift;
            let lo = if hi > 0 && v == hi << tile_shift {
                hi - 1
            } else {
                hi
            };
            lo..=hi.min(per_side.saturating_sub(1))
        };
        let mut mark = |list: &[VertexHeightMm], bit: u8| {
            for entry in list {
                let (vi, vj) = (entry.vertex % l.lattice_n, entry.vertex / l.lattice_n);
                if vj > cells {
                    continue;
                }
                for b in holders(vj) {
                    for a in holders(vi) {
                        tile_flags[(b * per_side + a) as usize] |= bit;
                    }
                }
            }
        };
        mark(&cooked.ring_pins, TILE_HAS_PIN);
        mark(&cooked.hanging_overrides, TILE_HAS_HANGING);
        for cut in &cooked.cut_bitmaps {
            if let Some(flags) = tile_flags.get_mut(cut.tile as usize) {
                *flags |= TILE_HAS_CUT;
            }
        }
        Self {
            pitch: crate::mm_to_m(0, l.width_mm) / l.cells() as f32,
            cells,
            tile_shift,
            per_side,
            axis,
            tile_flags,
        }
    }
    /// The axis table and the tile flags.
    pub(crate) fn resident_bytes(&self) -> usize {
        self.axis.capacity() * std::mem::size_of::<f32>() + self.tile_flags.capacity()
    }
    /// `vertex_m(lattice, i)`: from the table for a lattice vertex, computed for an index past it.
    #[inline]
    fn pos(&self, lattice: &crate::cooked::LatticeConstants, i: u32) -> f32 {
        match self.axis.get(i as usize) {
            Some(&v) => v,
            None => vertex_m(lattice, i),
        }
    }
    #[inline(always)]
    fn locate(&self, v: f32) -> Option<u32> {
        let n = self.cells as usize;
        // The table holds `lattice_n = cells + 1` positions, and every index below is at most `cells`.
        let axis = &self.axis[..=n];
        let first = axis[0];
        let last = axis[n];
        if !v.is_finite() || v < first || v > last {
            return None;
        }
        // `v >= first` here, so the quotient is not negative and the cast truncates to its floor
        // (the explicit `floor` was a libm call on targets without a rounding instruction).
        let estimate = (v - first) / self.pitch;
        let mut i = (estimate as usize).min(n - 1);
        while i > 0 && v < axis[i] {
            i -= 1;
        }
        while i + 1 < n && v >= axis[i + 1] {
            i += 1;
        }
        Some(i as u32)
    }
    /// The cell range `[start, end)` of tile column (or row) `a`: `LatticeConstants::tile_cells`.
    #[inline]
    fn tile_cells(&self, a: u32) -> (u32, u32) {
        let start = a << self.tile_shift;
        (start, (start + (1 << self.tile_shift)).min(self.cells))
    }
    /// `GroundLattice::count_stored` for tile `(a, b)`, with shifts for the divisions (a level's
    /// step and the tile side are powers of two). Same `Option`, same count.
    #[inline]
    fn stored(&self, c: &WorldGeomCooked, a: u32, b: u32, vi: u32, vj: u32) -> Option<u16> {
        let tile = (b * self.per_side + a) as usize;
        let level = u32::from(c.tile_levels.get(tile).copied().unwrap_or(0));
        let (x0, x1) = self.tile_cells(a);
        let (z0, z1) = self.tile_cells(b);
        if vi < x0 || vi > x1 || vj < z0 || vj > z1 {
            return None;
        }
        let mask = (1u32 << level) - 1;
        let (di, dj) = (vi - x0, vj - z0);
        if di & mask != 0 || dj & mask != 0 {
            return None;
        }
        let full_cols = ((x1 - x0) >> level) + 1;
        let full_rows = ((z1 - z0) >> level) + 1;
        let owned_cols = if a + 1 == self.per_side {
            full_cols
        } else {
            full_cols - 1
        };
        let owned_rows = if b + 1 == self.per_side {
            full_rows
        } else {
            full_rows - 1
        };
        let block = c.ground_counts.get(tile)?;
        let cols = if block.len() == (owned_cols * owned_rows) as usize {
            owned_cols
        } else if block.len() == (full_cols * full_rows) as usize {
            full_cols
        } else {
            return None;
        };
        let (di, dj) = (di >> level, dj >> level);
        if di >= cols {
            return None;
        }
        block.get((dj * cols + di) as usize).copied()
    }
    /// `GroundLattice::count_in_tile` for tile `(a, b)`: the tile's own block, else the neighbor
    /// that owns an east or north border vertex.
    #[inline]
    fn count(&self, c: &WorldGeomCooked, a: u32, b: u32, vi: u32, vj: u32) -> Option<u16> {
        if let Some(count) = self.stored(c, a, b, vi, vj) {
            return Some(count);
        }
        let (_, x1) = self.tile_cells(a);
        let (_, z1) = self.tile_cells(b);
        let east = vi == x1 && a + 1 != self.per_side;
        let north = vj == z1 && b + 1 != self.per_side;
        match (east, north) {
            (true, true) => self.stored(c, a + 1, b + 1, vi, vj),
            (true, false) => self.stored(c, a + 1, b, vi, vj),
            (false, true) => self.stored(c, a, b + 1, vi, vj),
            (false, false) => None,
        }
    }
    /// `GroundLattice::is_cut` for cell `(i, j)` of tile `(a, b)`.
    #[inline]
    fn is_cut(&self, c: &WorldGeomCooked, a: u32, b: u32, i: u32, j: u32, upper: bool) -> bool {
        let tile = b * self.per_side + a;
        let Ok(k) = c.cut_bitmaps.binary_search_by_key(&tile, |cut| cut.tile) else {
            return false;
        };
        let (x0, x1) = self.tile_cells(a);
        let (z0, _) = self.tile_cells(b);
        let bit = cut_bit_index(x1 - x0, i - x0, j - z0, u8::from(upper));
        c.cut_bitmaps[k]
            .bits
            .get((bit / 64) as usize)
            .is_some_and(|w| (w >> (bit % 64)) & 1 == 1)
    }
    #[inline]
    pub(crate) fn triangle(
        &self,
        c: &WorldGeomCooked,
        edits: &crate::edit::GroundEdits,
        x: f32,
        z: f32,
    ) -> GroundTri {
        let l = &c.lattice;
        let Some(i) = self.locate(x) else {
            return GroundTri::OffMap;
        };
        let Some(mut j) = self.locate(z) else {
            return GroundTri::OffMap;
        };
        if j > 0 && z == self.pos(l, j) {
            j -= 1;
        }
        let (a, b) = (i >> self.tile_shift, j >> self.tile_shift);
        let tile = (b * self.per_side + a) as usize;
        let edited = !edits.is_empty();
        let shift = if edited && edits.is_promoted(tile as u32) {
            0
        } else {
            c.tile_levels[tile]
        };
        let step = 1 << shift;
        let tx0 = a << self.tile_shift;
        let tz0 = b << self.tile_shift;
        let i0 = tx0 + ((i - tx0) >> shift << shift);
        let j0 = tz0 + ((j - tz0) >> shift << shift);
        let (i1, j1) = (i0 + step, j0 + step);
        let flags = self.tile_flags.get(tile).copied().unwrap_or(u8::MAX);
        let (_, tx1) = self.tile_cells(a);
        let (_, tz1) = self.tile_cells(b);
        // The common cell: no D1 edit in the state, no pin or hanging vertex in the tile, and the
        // cell's four vertices in the tile's own count block. The four heights are then four reads
        // of that block, with the index arithmetic of `stored` done once for the cell.
        if !edited
            && flags & (TILE_HAS_PIN | TILE_HAS_HANGING) == 0
            && i1 <= tx1
            && j1 <= tz1
            && let Some(block) = c.ground_counts.get(tile)
        {
            let level = u32::from(shift);
            let full_cols = ((tx1 - tx0) >> level) + 1;
            let full_rows = ((tz1 - tz0) >> level) + 1;
            let owned_cols = if a + 1 == self.per_side {
                full_cols
            } else {
                full_cols - 1
            };
            let owned_rows = if b + 1 == self.per_side {
                full_rows
            } else {
                full_rows - 1
            };
            let cols = if block.len() == (owned_cols * owned_rows) as usize {
                owned_cols
            } else if block.len() == (full_cols * full_rows) as usize {
                full_cols
            } else {
                0
            };
            let (di, dj) = ((i0 - tx0) >> level, (j0 - tz0) >> level);
            let at = (dj * cols + di) as usize;
            let far = at + cols as usize + 1;
            if di + 1 < cols
                && far < block.len()
                && let (Some(&x0), Some(&x1), Some(&z0), Some(&z1)) = (
                    self.axis.get(i0 as usize),
                    self.axis.get(i1 as usize),
                    self.axis.get(j0 as usize),
                    self.axis.get(j1 as usize),
                )
            {
                let height = |count: u16| l.offset_m() + count as f32 * l.step_m();
                let v01 = [x0, height(block[at + cols as usize]), z1];
                let v10 = [x1, height(block[at + 1]), z0];
                let e = (v10[0] - v01[0]) * (z - v01[2]) - (v10[2] - v01[2]) * (x - v01[0]);
                let upper = e > 0.0;
                if shift == 0 && flags & TILE_HAS_CUT != 0 && self.is_cut(c, a, b, i, j, upper) {
                    return GroundTri::Cut;
                }
                return GroundTri::Tri(if upper {
                    [v10, v01, [x1, height(block[far]), z1]]
                } else {
                    [[x0, height(block[at]), z0], v01, v10]
                });
            }
        }
        // No pin and no hanging vertex lies in this tile's closed vertex range, so neither search
        // can find a vertex of that range. A vertex past the range keeps both searches.
        let no_cooked_override = flags & (TILE_HAS_PIN | TILE_HAS_HANGING) == 0;
        let vertex = |vi: u32, vj: u32| -> Option<[f32; 3]> {
            let key = vj * l.lattice_n + vi;
            // A ring pin first (never edited, E5), then a D1 edit, then a hanging vertex.
            let edit = || if edited { edits.get(l, key) } else { None };
            let over = if no_cooked_override && vi <= tx1 && vj <= tz1 {
                edit()
            } else {
                let find = |list: &[VertexHeightMm]| {
                    list.binary_search_by_key(&key, |v| v.vertex)
                        .ok()
                        .map(|k| list[k].height_mm)
                };
                find(&c.ring_pins)
                    .or_else(edit)
                    .or_else(|| find(&c.hanging_overrides))
            };
            let y = if let Some(mm) = over {
                crate::mm_to_m(0, mm)
            } else {
                let count = self.count(c, a, b, vi, vj)?;
                l.offset_m() + count as f32 * l.step_m()
            };
            Some([self.pos(l, vi), y, self.pos(l, vj)])
        };
        let Some(v00) = vertex(i0, j0) else {
            return GroundTri::OffMap;
        };
        let Some(v01) = vertex(i0, j1) else {
            return GroundTri::OffMap;
        };
        let Some(v10) = vertex(i1, j0) else {
            return GroundTri::OffMap;
        };
        let e = (v10[0] - v01[0]) * (z - v01[2]) - (v10[2] - v01[2]) * (x - v01[0]);
        let upper = e > 0.0;
        if shift == 0 && flags & TILE_HAS_CUT != 0 && self.is_cut(c, a, b, i, j, upper) {
            return GroundTri::Cut;
        }
        let tri = if upper {
            let Some(v11) = vertex(i1, j1) else {
                return GroundTri::OffMap;
            };
            [v10, v01, v11]
        } else {
            [v00, v01, v10]
        };
        GroundTri::Tri(tri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cook::{CookInputs, GroundInput, cook};
    use crate::cooked::CutBitmap;
    use crate::ground::GroundLattice;

    /// The point location as it was before ADR-8817: axis positions from `vertex_m`, both override
    /// searches for every vertex, counts and cut bits through `GroundLattice`.
    fn triangle_before_adr_8817(c: &WorldGeomCooked, x: f32, z: f32) -> GroundTri {
        let l = &c.lattice;
        let pitch = crate::mm_to_m(0, l.width_mm) / l.cells() as f32;
        let cells = l.lattice_n - 1;
        let tile_shift = u32::from(l.ground_tile_cells).trailing_zeros();
        let per_side = l.tiles_per_side();
        let locate = |v: f32| -> Option<u32> {
            let n = cells as usize;
            let first = vertex_m(l, 0);
            let last = vertex_m(l, cells);
            if !v.is_finite() || v < first || v > last {
                return None;
            }
            let estimate = ((v - first) / pitch).floor();
            let mut i = (estimate as usize).min(n - 1);
            while i > 0 && v < vertex_m(l, i as u32) {
                i -= 1;
            }
            while i + 1 < n && v >= vertex_m(l, (i + 1) as u32) {
                i += 1;
            }
            Some(i as u32)
        };
        let Some(i) = locate(x) else {
            return GroundTri::OffMap;
        };
        let Some(mut j) = locate(z) else {
            return GroundTri::OffMap;
        };
        if j > 0 && z == vertex_m(l, j) {
            j -= 1;
        }
        let (a, b) = (i >> tile_shift, j >> tile_shift);
        let tile = (b * per_side + a) as usize;
        let shift = c.tile_levels[tile];
        let step = 1 << shift;
        let tx0 = a << tile_shift;
        let tz0 = b << tile_shift;
        let i0 = tx0 + ((i - tx0) >> shift << shift);
        let j0 = tz0 + ((j - tz0) >> shift << shift);
        let (i1, j1) = (i0 + step, j0 + step);
        let ground = GroundLattice::new(c);
        let vertex = |vi: u32, vj: u32| -> Option<[f32; 3]> {
            let key = vj * l.lattice_n + vi;
            let find = |list: &[VertexHeightMm]| {
                list.binary_search_by_key(&key, |v| v.vertex)
                    .ok()
                    .map(|k| list[k].height_mm)
            };
            let y = if let Some(mm) = find(&c.ring_pins).or_else(|| find(&c.hanging_overrides)) {
                crate::mm_to_m(0, mm)
            } else {
                let count = ground.count_in_tile(tile as u32, vi, vj)?;
                l.offset_m() + count as f32 * l.step_m()
            };
            Some([vertex_m(l, vi), y, vertex_m(l, vj)])
        };
        let Some(v00) = vertex(i0, j0) else {
            return GroundTri::OffMap;
        };
        let Some(v01) = vertex(i0, j1) else {
            return GroundTri::OffMap;
        };
        let Some(v10) = vertex(i1, j0) else {
            return GroundTri::OffMap;
        };
        let e = (v10[0] - v01[0]) * (z - v01[2]) - (v10[2] - v01[2]) * (x - v01[0]);
        let upper = e > 0.0;
        if shift == 0 && !c.cut_bitmaps.is_empty() && ground.is_cut(i, j, u8::from(upper)) {
            return GroundTri::Cut;
        }
        let tri = if upper {
            let Some(v11) = vertex(i1, j1) else {
                return GroundTri::OffMap;
            };
            [v10, v01, v11]
        } else {
            [v00, v01, v10]
        };
        GroundTri::Tri(tri)
    }

    fn assert_same_everywhere(c: &WorldGeomCooked, what: &str) -> [u32; 3] {
        let cache = GroundCoordinates::new(c);
        let l = &c.lattice;
        let cells = l.cells() as usize;
        let mut seen = [0u32; 3];
        for j in 0..=2 * cells {
            for i in 0..=2 * cells {
                let at = |k: usize| {
                    if k.is_multiple_of(2) {
                        vertex_m(l, (k / 2) as u32)
                    } else {
                        (vertex_m(l, (k / 2) as u32) + vertex_m(l, (k / 2 + 1) as u32)) * 0.5
                    }
                };
                let (x, z) = (at(i), at(j));
                for (x, z) in [
                    (x, z),
                    (x.next_down(), z),
                    (x.next_up(), z),
                    (x, z.next_down()),
                    (x, z.next_up()),
                    (x + 0.01, z + 0.02),
                ] {
                    let got = cache.triangle(c, &crate::edit::GroundEdits::default(), x, z);
                    let want = triangle_before_adr_8817(c, x, z);
                    match (got, want) {
                        (GroundTri::Tri(g), GroundTri::Tri(w)) => {
                            seen[0] += 1;
                            assert_eq!(
                                g.map(|v| v.map(f32::to_bits)),
                                w.map(|v| v.map(f32::to_bits)),
                                "{what} x={x} z={z}"
                            );
                        }
                        _ => {
                            seen[1] += u32::from(want == GroundTri::Cut);
                            seen[2] += u32::from(want == GroundTri::OffMap);
                            assert_eq!(got, want, "{what} x={x} z={z}");
                        }
                    }
                }
            }
        }
        seen
    }

    /// ADR-8817: the axis table, the per-tile override and cut flags and the shift-based count
    /// lookup return the triangle the per-call form returned, bit for bit, at every lattice vertex,
    /// cell centre and their neighbours: with a clipped last tile (49 cells in tiles of 16), ring
    /// pins on tile corners, borders and the map edge, hanging vertices, cut bitmaps, the file's
    /// count layout and the runtime's (borders dropped), and at every nested level.
    #[test]
    fn table_and_flags_return_the_per_call_triangles() {
        let n = 50u32;
        let input = GroundInput {
            lattice_n: n,
            width_m: 49.0 * 0.3125,
            offset_m: -32.0,
            step_m: 0.03125,
            counts: (0..n * n).map(|i| ((i * 71) % 2048) as u16).collect(),
        };
        let base = cook(&CookInputs {
            document: b"(version:1, ground_tile_cells:16, layers:[])",
            ground: &input,
            terrain: None,
            read_file: &|_| Err("no source".into()),
        })
        .expect("synthetic ground")
        .file
        .canonical;
        assert_eq!(
            base.lattice.tiles_per_side(),
            4,
            "three full tiles and a clipped one"
        );
        let mut c = base.clone();
        // Pins: a tile corner, an east border, a north border, an interior vertex, the last vertex.
        for (vi, vj, mm) in [
            (16, 16, -1234),
            (32, 5, 777),
            (7, 48, -31),
            (20, 21, 15_000),
            (49, 49, 42),
            (48, 49, -5),
        ] {
            c.ring_pins.push(VertexHeightMm {
                vertex: vj * n + vi,
                height_mm: mm,
            });
        }
        c.ring_pins.sort();
        for (vi, vj, mm) in [(40, 16, 333), (41, 40, -900), (0, 0, 5), (16, 32, 64)] {
            c.hanging_overrides.push(VertexHeightMm {
                vertex: vj * n + vi,
                height_mm: mm,
            });
        }
        c.hanging_overrides.sort();
        // Cut bits on a full tile and on the clipped corner tile (1 x 1 cell: two bits).
        c.cut_bitmaps = vec![
            CutBitmap {
                tile: 5,
                bits: (0..8u64)
                    .map(|w| 0x9e37_79b9_7f4a_7c15u64.rotate_left(w as u32 * 7))
                    .collect(),
            },
            CutBitmap {
                tile: 15,
                bits: vec![0b10],
            },
        ];
        let file_layout = assert_same_everywhere(&c, "file layout");
        assert!(file_layout[0] > 0 && file_layout[1] > 0, "{file_layout:?}");
        let mut runtime = c.clone();
        crate::ground::compact_border_counts(&mut runtime);
        assert!(runtime.ground_counts[0].len() < c.ground_counts[0].len());
        // The two layouts differ in which border vertices a clipped tile can read, before and after
        // alike, so only the comparison inside each layout is asserted.
        let runtime_layout = assert_same_everywhere(&runtime, "runtime layout");
        assert!(
            runtime_layout[0] > 0 && runtime_layout[1] > 0,
            "{runtime_layout:?}"
        );
        // Nested levels on a lattice of whole tiles, each with pins and hanging vertices.
        let input = GroundInput {
            lattice_n: 65,
            width_m: 40.03,
            offset_m: -32.0,
            step_m: 0.03125,
            counts: (0..65 * 65).map(|i| ((i * 71) % 2048) as u16).collect(),
        };
        let base = cook(&CookInputs {
            document: b"(version:1, ground_tile_cells:16, layers:[])",
            ground: &input,
            terrain: None,
            read_file: &|_| Err("no source".into()),
        })
        .expect("synthetic ground")
        .file
        .canonical;
        for level in 0..=4usize {
            let mut c = base.clone();
            for (tile, block) in c.ground_counts.iter_mut().enumerate() {
                // Mixed levels: every third tile stays at full pitch.
                let level = if tile % 3 == 0 { 0 } else { level };
                let old = block.clone();
                let mut counts = Vec::new();
                for j in (0..17).step_by(1 << level) {
                    for i in (0..17).step_by(1 << level) {
                        counts.push(old[j * 17 + i]);
                    }
                }
                *block = counts;
                c.tile_levels[tile] = level as u8;
            }
            for (vi, vj, mm) in [(32u32, 32u32, -1234), (16, 40, 99), (64, 64, 7), (3, 3, -8)] {
                c.ring_pins.push(VertexHeightMm {
                    vertex: vj * 65 + vi,
                    height_mm: mm,
                });
            }
            c.ring_pins.sort();
            for (vi, vj, mm) in [(48u32, 8u32, 250), (8, 48, -250)] {
                c.hanging_overrides.push(VertexHeightMm {
                    vertex: vj * 65 + vi,
                    height_mm: mm,
                });
            }
            c.hanging_overrides.sort();
            c.cut_bitmaps = vec![CutBitmap {
                tile: 0,
                bits: vec![0x0123_4567_89ab_cdef; 8],
            }];
            let seen = assert_same_everywhere(&c, &format!("level {level}, file layout"));
            assert!(seen[0] > 0, "level {level}");
            let mut runtime = c.clone();
            crate::ground::compact_border_counts(&mut runtime);
            let seen = assert_same_everywhere(&runtime, &format!("level {level}, runtime layout"));
            assert!(seen[0] > 0, "level {level}");
        }
    }
    #[test]
    fn cached_coordinates_preserve_ground_triangles_at_every_nested_level() {
        let input = GroundInput {
            lattice_n: 65,
            width_m: 40.03,
            offset_m: -32.0,
            step_m: 0.03125,
            counts: (0..65 * 65).map(|i| ((i * 71) % 2048) as u16).collect(),
        };
        let base = cook(&CookInputs {
            document: b"(version:1, ground_tile_cells:16, layers:[])",
            ground: &input,
            terrain: None,
            read_file: &|_| Err("no source".into()),
        })
        .expect("synthetic ground")
        .file
        .canonical;
        for level in 0..=4 {
            let mut c = base.clone();
            for (tile, block) in c.ground_counts.iter_mut().enumerate() {
                let old = block.clone();
                let mut counts = Vec::new();
                for j in (0..17).step_by(1 << level) {
                    for i in (0..17).step_by(1 << level) {
                        counts.push(old[j * 17 + i]);
                    }
                }
                *block = counts;
                c.tile_levels[tile] = level as u8;
            }
            c.ring_pins.push(crate::cooked::VertexHeightMm {
                vertex: 32 * 65 + 32,
                height_mm: -1234,
            });
            let cache = GroundCoordinates::new(&c);
            let reference = GroundLattice::new(&c);
            for j in 0..129 {
                for i in 0..129 {
                    let at = |k: usize| {
                        let lattice = &c.lattice;
                        if k.is_multiple_of(2) {
                            vertex_m(lattice, (k / 2) as u32)
                        } else {
                            (vertex_m(lattice, (k / 2) as u32) + vertex_m(lattice, (k / 2 + 1) as u32)) * 0.5
                        }
                    };
                    let (x, z) = (at(i), at(j));
                    for (x, z) in [
                        (x, z),
                        (x.next_down(), z),
                        (x.next_up(), z),
                        (x, z.next_down()),
                        (x, z.next_up()),
                    ] {
                        let got = cache.triangle(&c, &crate::edit::GroundEdits::default(), x, z);
                        let want = reference.ground_triangle_at(x, z);
                        match (got, want) {
                            (GroundTri::Tri(a), GroundTri::Tri(b)) => assert_eq!(
                                a.map(|v| v.map(f32::to_bits)),
                                b.map(|v| v.map(f32::to_bits)),
                                "level={level} x={x} z={z}"
                            ),
                            _ => assert_eq!(got, want),
                        }
                    }
                }
            }
        }
    }
}
