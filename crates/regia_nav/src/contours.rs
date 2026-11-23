// SPDX-License-Identifier: MPL-2.0
//! Contour extraction (`docs/specs/22-tier2.md` § T2.2): tracing each region's boundary into
//! closed loops of world-space points (Recast's "build contours" stage).
//!
//! Boundary edges are the cell sides where a walkable cell meets a non-walkable cell (or a cell of
//! another region). Loops are traced by the left-hand rule — at each corner, take the leftmost
//! turn that stays on the boundary — so a region and its holes come out as oriented loops.

use regia_core::math::Vec3;

use crate::heightfield::WalkableGrid;
use crate::regions::Regions;

/// The traced boundary of one region: one or more closed loops (the outer outline and any holes).
#[derive(Debug, Clone, Default)]
pub struct Contour {
    /// The region id.
    pub region: u32,
    /// Boundary loops (world-space XZ outlines; `y = 0`).
    pub loops: Vec<Vec<Vec3>>,
}

/// A grid corner in cell coordinates (X, Z), integers in `0..=width` / `0..=height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Corner(i32, i32);

impl Corner {
    /// The cell coordinates are the corner offsets; world = origin + corner * cell_size.
    fn world(self, grid: &WalkableGrid) -> Vec3 {
        Vec3::new(
            grid.origin.x + self.0 as f32 * grid.cell_size,
            0.0,
            grid.origin.z + self.1 as f32 * grid.cell_size,
        )
    }
}

/// Traces the boundaries of every region.
pub(crate) fn trace_contours(regions: &Regions, grid: &WalkableGrid) -> Vec<Contour> {
    // Collect the directed boundary edges of every region.
    let mut edges: std::collections::HashMap<u32, Vec<(Corner, Corner)>> = std::collections::HashMap::new();
    for z in 0..regions.height {
        for x in 0..regions.width {
            let Some(region) = regions.region_at(x, z) else {
                continue;
            };
            let entry = edges.entry(region).or_default();
            for (dx, dz) in [(0isize, -1isize), (1, 0), (0, 1), (-1, 0)] {
                let (nx, nz) = (x as isize + dx, z as isize + dz);
                let same =
                    (nx >= 0 && nz >= 0 && (nx as usize) < regions.width && (nz as usize) < regions.height)
                        && regions.region_at(nx as usize, nz as usize) == Some(region);
                if same {
                    continue;
                }
                // The boundary side of the cell, traced so the region is on the left.
                let (from, to) = match (dx, dz) {
                    (0, -1) => (Corner(x as i32, z as i32), Corner(x as i32 + 1, z as i32)),
                    (1, 0) => (Corner(x as i32 + 1, z as i32), Corner(x as i32 + 1, z as i32 + 1)),
                    (0, 1) => (Corner(x as i32 + 1, z as i32 + 1), Corner(x as i32, z as i32 + 1)),
                    (-1, 0) => (Corner(x as i32, z as i32 + 1), Corner(x as i32, z as i32)),
                    _ => unreachable!(),
                };
                entry.push((from, to));
            }
        }
    }

    let mut contours = Vec::with_capacity(edges.len());
    let mut regions_sorted: Vec<u32> = edges.keys().copied().collect();
    regions_sorted.sort_unstable();
    for region in regions_sorted {
        let loops = trace_region(&edges[&region], grid);
        contours.push(Contour { region, loops });
    }
    contours
}

/// Traces a region's directed boundary edges into closed loops.
fn trace_region(edges: &[(Corner, Corner)], grid: &WalkableGrid) -> Vec<Vec<Vec3>> {
    // Directed adjacency: from a corner, all outgoing corners.
    let mut outgoing: std::collections::HashMap<Corner, Vec<Corner>> = std::collections::HashMap::new();
    for &(from, to) in edges {
        outgoing.entry(from).or_default().push(to);
    }
    let mut visited: std::collections::HashSet<(Corner, Corner)> = std::collections::HashSet::new();
    let mut loops = Vec::new();

    for &(start_from, start_to) in edges {
        if visited.contains(&(start_from, start_to)) {
            continue;
        }
        let mut loop_points = Vec::new();
        let mut from = start_from;
        let mut to = start_to;
        loop {
            if visited.contains(&(from, to)) {
                break;
            }
            visited.insert((from, to));
            loop_points.push(from.world(grid));
            // At `to`, pick the leftmost-turn outgoing edge (excluding going straight back).
            let candidates: Vec<Corner> = outgoing.get(&to).cloned().unwrap_or_default();
            let next = leftmost_turn(from, to, &candidates);
            from = to;
            to = next;
        }
        // Close the loop back onto its first point (the trace above ends when it returns).
        if !loop_points.is_empty() {
            loop_points.push(loop_points[0]);
            loops.push(loop_points);
        }
    }

    loops
}

/// Chooses the outgoing edge from `to` that turns leftmost from the incoming direction `from → to`.
/// Degenerate cases (no candidate, or only a back edge) fall back to any candidate / a closed loop.
fn leftmost_turn(from: Corner, to: Corner, candidates: &[Corner]) -> Corner {
    let d_in = (to.0 - from.0, to.1 - from.1);
    // Rank each candidate by how far left it turns from `d_in` (larger cross = farther left),
    // preferring a forward move over a straight-back when there is no true turn.
    let best = candidates
        .iter()
        .filter(|&&c| c != from) // never immediately reverse
        .max_by(|&&a, &&b| {
            let da = (a.0 - to.0, a.1 - to.1);
            let db = (b.0 - to.0, b.1 - to.1);
            let ca = cross(d_in, da);
            let cb = cross(d_in, db);
            ca.cmp(&cb)
        });
    match best {
        Some(&c) => c,
        None => candidates.first().copied().unwrap_or(to),
    }
}

/// The 2D cross product of two direction pairs (X as x, Z as y): positive = left turn.
fn cross(a: (i32, i32), b: (i32, i32)) -> i32 {
    a.0 * b.1 - a.1 * b.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NavAgentParams;
    use crate::heightfield::WalkableGrid;
    use crate::regions::regionize;

    fn grid(width: usize, height: usize, walkable: &[(usize, usize, f32)]) -> WalkableGrid {
        let mut cells = vec![None; width * height];
        for &(x, z, y) in walkable {
            cells[z * width + x] = Some(y);
        }
        WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width,
            height,
            cells,
            areas: Vec::new(),
        }
    }

    #[test]
    fn a_single_cell_region_traces_a_unit_square() {
        let g = grid(3, 3, &[(1, 1, 0.0)]);
        let r = regionize(&g, &NavAgentParams::default());
        let contours = trace_contours(&r, &g);
        assert_eq!(contours.len(), 1);
        let loop_pts = &contours[0].loops[0];
        // 4 corners + the closing duplicate = 5 points.
        assert_eq!(loop_pts.len(), 5, "{loop_pts:?}");
        let xs: Vec<i32> = loop_pts.iter().map(|p| p.x as i32).collect();
        assert_eq!(xs, vec![1, 2, 2, 1, 1], "unit square corners {xs:?}");
    }

    #[test]
    fn a_two_by_two_region_traces_its_outer_square() {
        let g = grid(4, 4, &[(1, 1, 0.0), (2, 1, 0.0), (1, 2, 0.0), (2, 2, 0.0)]);
        let r = regionize(&g, &NavAgentParams::default());
        let contours = trace_contours(&r, &g);
        assert_eq!(contours.len(), 1);
        let loop_pts = &contours[0].loops[0];
        assert_eq!(loop_pts.len(), 9, "outer 2x2 square: 8 + closing {loop_pts:?}");
    }

    #[test]
    fn a_hole_is_traced_as_a_second_loop() {
        // A ring of walkable cells around an unwalkable centre.
        let walkable: Vec<(usize, usize, f32)> = (0..3)
            .flat_map(|z| (0..3).map(move |x| (x, z, 0.0)))
            .filter(|&(x, z, _)| !(x == 1 && z == 1))
            .collect();
        let g = grid(3, 3, &walkable);
        let r = regionize(&g, &NavAgentParams::default());
        let contours = trace_contours(&r, &g);
        assert_eq!(contours.len(), 1);
        assert_eq!(
            contours[0].loops.len(),
            2,
            "outer loop + inner hole: {:?}",
            contours[0].loops.len()
        );
    }
}
