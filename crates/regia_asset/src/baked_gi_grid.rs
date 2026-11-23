// SPDX-License-Identifier: MPL-2.0
//! The brick lookup grid of the baked-GI volume buffer (ADR-1835, ADR-357 plan item 1).
//!
//! The screen pass picks one detail brick per half-resolution texel. Scanning every brick for
//! each texel costs time linear in the brick count; this grid bins the bricks into a coarse
//! uniform grid so the lookup tests only the candidates of the texel's cell. The grid is derived
//! from the brick table (nothing is stored in the `.fgiv` file), uploaded after the brick table,
//! and lists each cell's bricks in ascending brick index, so a lookup that keeps the first brick
//! of the highest selection key picks the same brick as the linear scan, bit for bit.
//!
//! ## Words (all `u32`; floats as bits)
//!
//! - header (8): origin xyz, 1 / cell edge, dims xyz, first list word (relative to the grid);
//! - cells: dims.x · dims.y · dims.z pairs (first list entry, count), x fastest, then y, then z;
//! - list: brick indices.

use std::cell::Cell;

use crate::baked_gi::DetailBrick;

thread_local! {
    static BRICK_GRID_BUILDS: Cell<u64> = const { Cell::new(0) };
}

/// How many times [`BrickGrid::build`] has run on this thread.
///
/// A volume that keeps its brick grid does not increment this on a later read. The count is
/// per thread, so parallel tests do not share it.
#[must_use]
pub fn brick_grid_builds() -> u64 {
    BRICK_GRID_BUILDS.with(Cell::get)
}

fn note_brick_grid_build() {
    BRICK_GRID_BUILDS.with(|count| count.set(count.get().saturating_add(1)));
}

/// Words of the grid header.
pub const BRICK_GRID_HEADER_WORDS: usize = 8;
/// Most cells a grid has (its cell table costs two words each).
pub const BRICK_GRID_MAX_CELLS: usize = 4096;
/// Most cells along one axis.
pub const BRICK_GRID_MAX_AXIS: usize = 256;
/// Cells by which an interior brick holds a point outside its probe box (the shader's
/// `BGI_INTERIOR_EDGE_SLACK`).
pub const BRICK_GRID_INTERIOR_SLACK: f32 = 0.125;

/// Brick indices binned into a uniform grid; see the module docs for the word layout.
#[derive(Clone, Debug, PartialEq)]
pub struct BrickGrid {
    origin: [f32; 3],
    inv_cell: f32,
    dims: [u32; 3],
    cells: Vec<[u32; 2]>,
    list: Vec<u32>,
}

/// The box (min, max) in which `brick` can be selected: its probe box, widened for an interior
/// brick by the edge slack and by the half-cell push along the normal (`|n| <= 1`), and by a
/// hair everywhere so float noise at a face cannot drop a candidate. `None` for a brick that can
/// never be selected (no probes, or a non-finite box).
fn reach(brick: &DetailBrick) -> Option<([f32; 3], [f32; 3])> {
    if brick.counts.contains(&0) {
        return None;
    }
    let smin = brick.spacing.iter().copied().fold(f32::INFINITY, f32::min);
    let mut lo = [0.0f32; 3];
    let mut hi = [0.0f32; 3];
    for a in 0..3 {
        let top = brick.origin[a] + (brick.counts[a] - 1) as f32 * brick.spacing[a];
        let mut pad = 1.0e-3 * brick.spacing[a];
        if brick.is_interior() {
            pad += BRICK_GRID_INTERIOR_SLACK * brick.spacing[a] + 0.5 * smin;
        }
        lo[a] = brick.origin[a] - pad;
        hi[a] = top + pad;
        if !(lo[a].is_finite() && hi[a].is_finite()) {
            return None;
        }
    }
    Some((lo, hi))
}

impl BrickGrid {
    /// Bins `bricks` (in table order). No bricks gives an empty grid ([`Self::word_count`] 0).
    #[must_use]
    pub fn build(bricks: &[DetailBrick]) -> Self {
        note_brick_grid_build();
        let reaches: Vec<Option<([f32; 3], [f32; 3])>> = bricks.iter().map(reach).collect();
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        let mut any = false;
        for (l, h) in reaches.iter().flatten() {
            any = true;
            for a in 0..3 {
                lo[a] = lo[a].min(l[a]);
                hi[a] = hi[a].max(h[a]);
            }
        }
        if bricks.is_empty() {
            return Self {
                origin: [0.0; 3],
                inv_cell: 1.0,
                dims: [0; 3],
                cells: Vec::new(),
                list: Vec::new(),
            };
        }
        if !any {
            // Bricks that can never be selected: one empty cell, so the shader's reads stay
            // inside the grid.
            return Self {
                origin: [0.0; 3],
                inv_cell: 1.0,
                dims: [1; 3],
                cells: vec![[0, 0]],
                list: Vec::new(),
            };
        }
        let ext: [f32; 3] = std::array::from_fn(|a| (hi[a] - lo[a]).max(1.0e-3));
        let target = (reaches.iter().flatten().count() * 2).clamp(1, BRICK_GRID_MAX_CELLS);
        let mut cell = (ext[0] * ext[1] * ext[2] / target as f32).cbrt().max(1.0e-3);
        let dims = loop {
            let d: [usize; 3] =
                std::array::from_fn(|a| ((ext[a] / cell).ceil() as usize).clamp(1, BRICK_GRID_MAX_AXIS));
            if d[0] * d[1] * d[2] <= BRICK_GRID_MAX_CELLS {
                break d;
            }
            cell *= 1.25;
        };
        // Cells cover [lo, lo + dims * cell); the last may reach past `hi`.
        let inv = 1.0 / cell;
        let index =
            |v: f32, a: usize| -> usize { (((v - lo[a]) * inv).floor().max(0.0) as usize).min(dims[a] - 1) };
        let mut per_cell: Vec<Vec<u32>> = vec![Vec::new(); dims[0] * dims[1] * dims[2]];
        for (b, r) in reaches.iter().enumerate() {
            let Some((l, h)) = r else { continue };
            let (x0, x1) = (index(l[0], 0), index(h[0], 0));
            let (y0, y1) = (index(l[1], 1), index(h[1], 1));
            let (z0, z1) = (index(l[2], 2), index(h[2], 2));
            for z in z0..=z1 {
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        per_cell[(z * dims[1] + y) * dims[0] + x].push(b as u32);
                    }
                }
            }
        }
        let mut cells = Vec::with_capacity(per_cell.len());
        let mut list = Vec::new();
        for c in &per_cell {
            cells.push([list.len() as u32, c.len() as u32]);
            list.extend_from_slice(c);
        }
        Self {
            origin: lo,
            inv_cell: inv,
            dims: [dims[0] as u32, dims[1] as u32, dims[2] as u32],
            cells,
            list,
        }
    }

    /// Words [`Self::to_words`] writes (0 for an empty grid).
    #[must_use]
    pub fn word_count(&self) -> usize {
        if self.cells.is_empty() {
            0
        } else {
            BRICK_GRID_HEADER_WORDS + 2 * self.cells.len() + self.list.len()
        }
    }

    /// Cells in the grid.
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Brick references in the cells' lists, counting a brick once per cell it is binned into.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.list.len()
    }

    /// Cells along each axis (zeros for an empty grid).
    #[must_use]
    pub fn dims(&self) -> [u32; 3] {
        self.dims
    }

    /// The bricks (ascending index) that can be selected for a point `p` in the cell holding
    /// it; empty outside the grid, for a non-finite point, and for an empty grid. This is the
    /// CPU mirror of the shader's lookup.
    #[must_use]
    pub fn candidates(&self, p: [f32; 3]) -> &[u32] {
        if self.cells.is_empty() {
            return &[];
        }
        let mut c = [0usize; 3];
        for a in 0..3 {
            let g = (p[a] - self.origin[a]) * self.inv_cell;
            // NaN fails both comparisons.
            if !(g >= 0.0 && g < self.dims[a] as f32) {
                return &[];
            }
            c[a] = g as usize;
        }
        let at = (c[2] * self.dims[1] as usize + c[1]) * self.dims[0] as usize + c[0];
        let [start, count] = self.cells[at];
        &self.list[start as usize..(start + count) as usize]
    }

    /// The grid as the words the renderer uploads after the brick table.
    #[must_use]
    pub fn to_words(&self) -> Vec<u32> {
        if self.cells.is_empty() {
            return Vec::new();
        }
        let list_at = BRICK_GRID_HEADER_WORDS + 2 * self.cells.len();
        let mut w = Vec::with_capacity(self.word_count());
        w.extend(self.origin.iter().map(|v| v.to_bits()));
        w.push(self.inv_cell.to_bits());
        w.extend_from_slice(&self.dims);
        w.push(list_at as u32);
        for c in &self.cells {
            w.extend_from_slice(c);
        }
        w.extend_from_slice(&self.list);
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baked_gi::BrickFlags;

    fn brick(origin: [f32; 3], counts: [u32; 3], spacing: f32, flags: u32) -> DetailBrick {
        DetailBrick {
            origin,
            spacing: [spacing; 3],
            counts,
            first_probe: 0,
            flags,
        }
    }

    #[test]
    fn no_bricks_is_an_empty_grid() {
        let g = BrickGrid::build(&[]);
        assert_eq!(g.word_count(), 0);
        assert!(g.to_words().is_empty());
        assert!(g.candidates([0.0; 3]).is_empty());
    }

    #[test]
    fn words_match_the_count_and_the_header() {
        let bricks = [
            brick([0.0; 3], [5, 5, 5], 1.0, 0),
            brick([20.0, 0.0, 0.0], [5, 5, 5], 1.0, BrickFlags::INTERIOR),
        ];
        let g = BrickGrid::build(&bricks);
        let w = g.to_words();
        assert_eq!(w.len(), g.word_count());
        assert_eq!(&w[4..7], &g.dims());
        let cells = (g.dims()[0] * g.dims()[1] * g.dims()[2]) as usize;
        assert_eq!(w[7] as usize, BRICK_GRID_HEADER_WORDS + 2 * cells);
        assert!(cells <= BRICK_GRID_MAX_CELLS);
    }

    #[test]
    fn a_point_in_a_brick_finds_it_and_a_far_point_finds_none() {
        let bricks = [
            brick([0.0; 3], [5, 5, 5], 1.0, 0),
            brick([100.0, 0.0, 0.0], [5, 5, 5], 1.0, 0),
        ];
        let g = BrickGrid::build(&bricks);
        assert!(g.candidates([2.0, 2.0, 2.0]).contains(&0));
        assert!(g.candidates([102.0, 2.0, 2.0]).contains(&1));
        assert!(!g.candidates([2.0, 2.0, 2.0]).contains(&1));
        assert!(g.candidates([50.0, 2.0, 2.0]).len() <= 1);
        assert!(g.candidates([-500.0, 0.0, 0.0]).is_empty());
        assert!(g.candidates([f32::NAN, 0.0, 0.0]).is_empty());
    }

    #[test]
    fn candidates_are_in_ascending_brick_order() {
        let bricks: Vec<_> = (0..6)
            .map(|i| brick([i as f32 * 0.5; 3], [6, 6, 6], 1.0, 0))
            .collect();
        let g = BrickGrid::build(&bricks);
        let c = g.candidates([2.0, 2.0, 2.0]);
        assert!(c.len() > 1);
        assert!(c.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_dense_layout_stays_within_the_cell_cap() {
        let mut bricks = Vec::new();
        for i in 0..40 {
            for j in 0..40 {
                bricks.push(brick([i as f32 * 3.0, 0.0, j as f32 * 3.0], [3, 3, 3], 1.0, 0));
            }
        }
        let g = BrickGrid::build(&bricks);
        let d = g.dims();
        assert!((d[0] * d[1] * d[2]) as usize <= BRICK_GRID_MAX_CELLS);
        assert!(d.iter().all(|&a| a as usize <= BRICK_GRID_MAX_AXIS));
    }
}
