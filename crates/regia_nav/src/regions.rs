// SPDX-License-Identifier: MPL-2.0
//! Region generation (`docs/specs/22-tier2.md` § T2.2): flood-filling the compact walkable grid
//! into contiguous regions, respecting the agent's step height.
//!
//! Two walkable cells belong to the same region when they are 4-connected and their surface
//! heights differ by no more than `max_climb` — the agent can step from one onto the other. This
//! is the "build compact heightfield → build regions" stage of the Recast pipeline, reduced to a
//! union-find flood fill over the 2D grid (single surface per column).

use crate::config::NavAgentParams;
use crate::heightfield::WalkableGrid;

/// Region ids assigned to walkable cells (`z * width + x`; `u32::MAX` = not walkable).
#[derive(Debug, Clone)]
pub(crate) struct Regions {
    /// Number of cells along X.
    pub width: usize,
    /// Number of cells along Z.
    pub height: usize,
    /// Region id per cell (row-major); `u32::MAX` marks a non-walkable cell.
    pub ids: Vec<u32>,
    /// Number of distinct regions found.
    pub count: u32,
}

impl Regions {
    /// The flat index of cell `(x, z)`.
    pub fn index(&self, x: usize, z: usize) -> usize {
        z * self.width + x
    }

    /// The region id of cell `(x, z)`, or `None` if it is not walkable.
    pub fn region_at(&self, x: usize, z: usize) -> Option<u32> {
        let id = self.ids[self.index(x, z)];
        (id != u32::MAX).then_some(id)
    }
}

/// Flood-fills `grid` into regions: 4-connected cells whose walkable heights differ by at most
/// `agent.max_climb` share a region.
pub(crate) fn regionize(grid: &WalkableGrid, agent: &NavAgentParams) -> Regions {
    let width = grid.width;
    let height = grid.height;
    let mut ids = vec![u32::MAX; width * height];
    let mut next_region = 0u32;
    let mut stack: Vec<usize> = Vec::new();

    for z in 0..height {
        for x in 0..width {
            let idx = z * width + x;
            if ids[idx] != u32::MAX {
                continue;
            }
            if grid.cells[idx].is_none() {
                continue;
            }
            // Start a new region and flood-fill.
            ids[idx] = next_region;
            stack.push(idx);
            while let Some(cur) = stack.pop() {
                let cx = cur % width;
                let cz = cur / width;
                let cur_height = grid.cells[cur].expect("region cells are walkable");
                for (nx, nz) in neighbors(cx, cz, width, height) {
                    let nidx = nz * width + nx;
                    if ids[nidx] != u32::MAX {
                        continue;
                    }
                    let Some(n_height) = grid.cells[nidx] else {
                        continue;
                    };
                    if (n_height - cur_height).abs() <= agent.max_climb + 1e-4 {
                        ids[nidx] = next_region;
                        stack.push(nidx);
                    }
                }
            }
            next_region += 1;
        }
    }

    Regions {
        width,
        height,
        count: next_region,
        ids,
    }
}

/// The 4-neighbours of `(x, z)` inside the grid bounds.
fn neighbors(x: usize, z: usize, width: usize, height: usize) -> impl Iterator<Item = (usize, usize)> {
    let mut out = [(0usize, 0usize); 4];
    let mut n = 0;
    if x > 0 {
        out[n] = (x - 1, z);
        n += 1;
    }
    if x + 1 < width {
        out[n] = (x + 1, z);
        n += 1;
    }
    if z > 0 {
        out[n] = (x, z - 1);
        n += 1;
    }
    if z + 1 < height {
        out[n] = (x, z + 1);
        n += 1;
    }
    (0..n).map(move |i| out[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heightfield::WalkableGrid;

    fn grid(width: usize, height: usize, walkable: &[(usize, usize, f32)]) -> WalkableGrid {
        let mut cells = vec![None; width * height];
        for &(x, z, y) in walkable {
            cells[z * width + x] = Some(y);
        }
        WalkableGrid {
            origin: regia_core::math::Vec3::ZERO,
            cell_size: 1.0,
            width,
            height,
            cells,
            areas: Vec::new(),
        }
    }

    #[test]
    fn connected_floor_is_one_region() {
        let g = grid(
            4,
            4,
            &[
                (0, 0, 0.0),
                (1, 0, 0.0),
                (2, 0, 0.0),
                (3, 0, 0.0),
                (0, 1, 0.0),
                (1, 1, 0.0),
            ],
        );
        let r = regionize(&g, &NavAgentParams::default());
        assert_eq!(r.count, 1);
    }

    #[test]
    fn a_gap_splits_regions() {
        // Two isolated walkable islands.
        let g = grid(4, 2, &[(0, 0, 0.0), (3, 0, 0.0)]);
        let r = regionize(&g, &NavAgentParams::default());
        assert_eq!(r.count, 2);
    }

    #[test]
    fn a_step_taller_than_max_climb_splits_regions() {
        let agent = NavAgentParams {
            max_climb: 0.5,
            ..Default::default()
        };
        // A 2 m step between adjacent cells is not climbable.
        let g = grid(2, 1, &[(0, 0, 0.0), (1, 0, 2.0)]);
        let r = regionize(&g, &agent);
        assert_eq!(r.count, 2, "too-high step separates regions");

        // A 0.4 m step (within max_climb) joins them.
        let g2 = grid(2, 1, &[(0, 0, 0.0), (1, 0, 0.4)]);
        let r2 = regionize(&g2, &agent);
        assert_eq!(r2.count, 1, "small step joins regions");
    }

    #[test]
    fn region_at_reports_walkable_cells_only() {
        let g = grid(2, 2, &[(0, 0, 0.0)]);
        let r = regionize(&g, &NavAgentParams::default());
        assert!(r.region_at(0, 0).is_some());
        assert!(r.region_at(1, 0).is_none());
    }
}
