// SPDX-License-Identifier: MPL-2.0
//! The rendered CDLOD surface on the CPU, and how far it departs from the true heightfield.
//!
//! Coarse CDLOD levels draw a prefiltered, coarser triangle grid ([`crate::height_mips`]), so
//! away from the camera the visible ground sits a little above or below the bilinear
//! heightfield the simulation uses. Anything placed on the ground by the true height (scatter
//! rocks, decals, props) then floats or sinks by that difference. [`rendered_height`] is the
//! exact surface the GPU draws at a point (the same morph, pyramid level and triangle split as
//! `terrain.wgsl`), and [`lod_height_error`] measures the departure per level so a placement
//! system can bound it (sink a rock by the local error, or snap it to [`rendered_height`]).

use crate::cdlod::{
    CdlodConfig, NodeDraw, QuadNode, eval_node_vertex_filtered, node_world_size, vertex_morph_t,
};
use crate::height_mips::HeightMips;
use crate::sampler::TerrainMap;

/// Height of the drawn surface of `draw` at world `(x, z)`, with the camera at `camera_xz`.
///
/// Mirrors the vertex shader: grid vertices morph toward the parent grid by their distance to
/// the camera, read the height pyramid at their level, and the cell is split along the
/// `(gx, gz+1)`–`(gx+1, gz)` diagonal like the shared index buffer. Points outside the node are
/// clamped to its edge cells.
#[must_use]
#[allow(clippy::too_many_arguments)] // mirrors the shader's inputs one for one
pub fn rendered_height(
    map: &TerrainMap,
    mips: &HeightMips,
    config: &CdlodConfig,
    root_level: u32,
    draw: &NodeDraw,
    camera_xz: [f32; 2],
    x: f32,
    z: f32,
) -> f32 {
    let tri = rendered_triangle(map, config, root_level, draw.node, x, z);
    let ts = tri.morph_ts(map, config, root_level, draw, camera_xz);
    tri.height(map, mips, config, root_level, draw.node, ts, x, z)
}

/// The grid triangle of a selected node that covers a point: its three grid vertices, in the
/// order [`rendered_height`] interpolates them. Depends only on the node and the point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RenderedTriangle {
    pub(crate) verts: [(u32, u32); 3],
}

/// The [`RenderedTriangle`] of `node` under `(x, z)` (clamped to the node's edge cells).
pub(crate) fn rendered_triangle(
    map: &TerrainMap,
    config: &CdlodConfig,
    root_level: u32,
    node: QuadNode,
    x: f32,
    z: f32,
) -> RenderedTriangle {
    let (min_x, min_z, size) = node.map_rect(map, root_level);
    let grid = config.grid_size.max(1);
    let cell = size / grid as f32;
    let fx = ((x - min_x) / cell).clamp(0.0, grid as f32);
    let fz = ((z - min_z) / cell).clamp(0.0, grid as f32);
    let gx = (fx.floor() as u32).min(grid - 1);
    let gz = (fz.floor() as u32).min(grid - 1);
    let (u, v) = (fx - gx as f32, fz - gz as f32);
    let verts = if u + v <= 1.0 {
        [(gx, gz), (gx, gz + 1), (gx + 1, gz)]
    } else {
        [(gx + 1, gz + 1), (gx, gz + 1), (gx + 1, gz)]
    };
    RenderedTriangle { verts }
}

impl RenderedTriangle {
    /// Morph factor of each vertex for a camera at `camera_xz` (the vertex shader's rule:
    /// unmorphed grid position's distance to the camera against the node's morph band).
    pub(crate) fn morph_ts(
        &self,
        map: &TerrainMap,
        config: &CdlodConfig,
        root_level: u32,
        draw: &NodeDraw,
        camera_xz: [f32; 2],
    ) -> [f32; 3] {
        self.morph_ts_slack(map, config, root_level, draw, camera_xz).0
    }

    /// [`Self::morph_ts`] plus the slack: how far the camera can move before any vertex leaves
    /// its clamped morph state (0 inside the band, infinite for an unmorphed root node).
    pub(crate) fn morph_ts_slack(
        &self,
        map: &TerrainMap,
        config: &CdlodConfig,
        root_level: u32,
        draw: &NodeDraw,
        camera_xz: [f32; 2],
    ) -> ([f32; 3], f32) {
        let (min_x, min_z, size) = draw.node.map_rect(map, root_level);
        let cell = size / config.grid_size.max(1) as f32;
        let mut slack = f32::INFINITY;
        let ts = self.verts.map(|(vx, vz)| {
            let wx = min_x + vx as f32 * cell;
            let wz = min_z + vz as f32 * cell;
            let d = ((wx - camera_xz[0]).powi(2) + (wz - camera_xz[1]).powi(2)).sqrt();
            let t = vertex_morph_t(draw.morph_start, draw.morph_end, d);
            if draw.morph_end > draw.morph_start {
                let s = if t <= 0.0 {
                    draw.morph_start - d
                } else if t >= 1.0 {
                    d - draw.morph_end
                } else {
                    0.0
                };
                slack = slack.min(s.max(0.0));
            }
            t
        });
        (ts, slack)
    }

    /// Height of the drawn triangle at `(x, z)` with vertex morph factors `ts`.
    #[allow(clippy::too_many_arguments)] // the shader's inputs
    pub(crate) fn height(
        &self,
        map: &TerrainMap,
        mips: &HeightMips,
        config: &CdlodConfig,
        root_level: u32,
        node: QuadNode,
        ts: [f32; 3],
        x: f32,
        z: f32,
    ) -> f32 {
        let [a, b, c] = [0, 1, 2].map(|i| {
            let (vx, vz) = self.verts[i];
            eval_node_vertex_filtered(map, mips, config, node, root_level, vx, vz, ts[i])
        });
        barycentric_height(a, b, c, x, z)
    }
}

/// Height at `(x, z)` on the plane through three world points (barycentric in XZ).
fn barycentric_height(a: [f32; 3], b: [f32; 3], c: [f32; 3], x: f32, z: f32) -> f32 {
    let det = (b[2] - c[2]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[2] - c[2]);
    if det.abs() < 1e-9 {
        return (a[1] + b[1] + c[1]) / 3.0;
    }
    let l1 = ((b[2] - c[2]) * (x - c[0]) + (c[0] - b[0]) * (z - c[2])) / det;
    let l2 = ((c[2] - a[2]) * (x - c[0]) + (a[0] - c[0]) * (z - c[2])) / det;
    let l3 = 1.0 - l1 - l2;
    a[1] * l1 + b[1] * l2 + c[1] * l3
}

/// Vertical departure of one CDLOD level's (unmorphed) surface from the bilinear heightfield.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LodError {
    /// Quadtree level (0 = finest).
    pub level: u32,
    /// Grid cell of the level, metres.
    pub cell_m: f32,
    /// Largest |rendered − true| over the samples, metres.
    pub max_m: f32,
    /// 99th percentile, metres.
    pub p99_m: f32,
    /// Root mean square, metres.
    pub rms_m: f32,
}

/// [`LodError`] for every level, sampled on a `samples_per_axis²` lattice over the map
/// (offset off the grid lines). `max_slope` (rise over run) restricts the statistics to ground
/// no steeper than that, e.g. the driveable ground scatter sits on; `f32::INFINITY` keeps all.
#[must_use]
pub fn lod_height_error(
    map: &TerrainMap,
    mips: &HeightMips,
    config: &CdlodConfig,
    root_level: u32,
    samples_per_axis: u32,
    max_slope: f32,
) -> Vec<LodError> {
    // Points over the map's rectangle; nodes counted from the quadtree square's corner (the
    // same corner on a square map, ADR-8369).
    let half = map.quadtree_width() * 0.5;
    let (half_x, half_z) = (map.world_width() * 0.5, map.world_depth() * 0.5);
    let n = samples_per_axis.max(2);
    let (step_x, step_z) = (map.world_width() / n as f32, map.world_depth() / n as f32);
    let mut points = Vec::with_capacity((n * n) as usize);
    for j in 0..n {
        for i in 0..n {
            let x = -half_x + (i as f32 + 0.37) * step_x;
            let z = -half_z + (j as f32 + 0.61) * step_z;
            let normal = map.normal_at(x, z);
            let slope = (1.0 - normal[1] * normal[1]).max(0.0).sqrt() / normal[1].max(1e-4);
            if slope <= max_slope {
                points.push((x, z));
            }
        }
    }
    (0..=root_level)
        .map(|level| {
            let size = node_world_size(map.quadtree_width(), root_level, level);
            let mut errors: Vec<f32> = points
                .iter()
                .map(|&(x, z)| {
                    let node = QuadNode {
                        level,
                        x: (((x + half) / size) as u32).min((1u32 << (root_level - level)) - 1),
                        z: (((z + half) / size) as u32).min((1u32 << (root_level - level)) - 1),
                    };
                    let draw = NodeDraw {
                        node,
                        morph_start: 0.0,
                        morph_end: 0.0,
                    };
                    let rendered = rendered_height(map, mips, config, root_level, &draw, [x, z], x, z);
                    (rendered - map.sample_bilinear(x, z)).abs()
                })
                .collect();
            errors.sort_by(f32::total_cmp);
            let count = errors.len().max(1) as f32;
            let rms = (errors.iter().map(|e| e * e).sum::<f32>() / count).sqrt();
            let p99 = errors
                .get(((errors.len() as f32 * 0.99) as usize).min(errors.len().saturating_sub(1)))
                .copied()
                .unwrap_or(0.0);
            LodError {
                level,
                cell_m: size / config.grid_size.max(1) as f32,
                max_m: errors.last().copied().unwrap_or(0.0),
                p99_m: p99,
                rms_m: rms,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdlod::{MinMaxPyramid, select_nodes};

    /// 1025² over 320 m (0.31 m texels): rolling ground with decimetre crust, like the DO27
    /// plains scatter sits on.
    fn plain() -> TerrainMap {
        let n = 1025u32;
        let width = 320.0f32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
                let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
                heights.push(
                    6.0 * (x * 0.021).sin() * (z * 0.017).cos()
                        + 1.5 * crate::noise::fbm2(x / 9.0, z / 9.0, 3)
                        + 0.25 * crate::noise::fbm2(x / 1.7, z / 1.7, 4),
                );
            }
        }
        TerrainMap::from_heights(n, n, heights, width, 16.0, -8.0).expect("map")
    }

    #[test]
    fn rendered_height_at_the_leaf_is_the_heightfield() {
        let map = plain();
        let mips = HeightMips::build(&map);
        let config = CdlodConfig::with_grid_size(16);
        // 320 / 2^6 = 5 m leaves, 0.3125 m cells: level 0 of the pyramid at every vertex.
        let leaf = NodeDraw {
            node: QuadNode {
                level: 0,
                x: 30,
                z: 33,
            },
            morph_start: 0.0,
            morph_end: 0.0,
        };
        let (min_x, min_z, _) = leaf.node.world_rect(320.0, 6);
        for (dx, dz) in [(0.0, 0.0), (1.0, 2.5), (3.3, 4.9)] {
            let (x, z) = (min_x + dx, min_z + dz);
            let r = rendered_height(&map, &mips, &config, 6, &leaf, [x, z], x, z);
            let exact = crate::cdlod::terrain_surface_height(&map, 0.3125, x, z);
            assert!((r - exact).abs() < 2e-3, "{r} vs {exact}");
        }
    }

    /// Worst |drawn − true| within 200 m of a camera over the plain, for a grid size.
    fn worst_within_200_m(grid: u32) -> f32 {
        let map = plain();
        let mips = HeightMips::build(&map);
        let config = CdlodConfig::with_grid_size(grid);
        let root = crate::gpu::full_detail_root_level(&map, &config);
        let pyramid = MinMaxPyramid::build(&map, root).expect("pyramid");
        let camera = [-150.0f32, -140.0];
        let selection = select_nodes(&map, &pyramid, &config, root, camera, None);
        let mut worst = 0.0f32;
        let mut checked = 0u32;
        for draw in &selection.nodes {
            let (min_x, min_z, size) = draw.node.world_rect(320.0, root);
            for j in 0..12 {
                for i in 0..12 {
                    let x = min_x + (i as f32 + 0.5) / 12.0 * size;
                    let z = min_z + (j as f32 + 0.5) / 12.0 * size;
                    if ((x - camera[0]).powi(2) + (z - camera[1]).powi(2)).sqrt() > 200.0 {
                        continue;
                    }
                    checked += 1;
                    let r = rendered_height(&map, &mips, &config, root, draw, camera, x, z);
                    worst = worst.max((r - map.sample_bilinear(x, z)).abs());
                }
            }
        }
        assert!(checked > 0, "no sample fell inside 200 m of the camera");
        worst
    }

    #[test]
    fn runtime_grid_keeps_the_far_surface_within_25_cm_inside_200_m() {
        // The runtime frame's grid (`SHARED_MIN_GRID_SIZE` = 32) and default ranges: the
        // 0.25 m crust the cells cannot resolve sets the error (0.224 m; 16 cells: 0.36 m).
        // Scatter that must touch the drawn ground reads `TerrainSharedDraw::rendered_heights`.
        let worst = worst_within_200_m(crate::gpu::SHARED_MIN_GRID_SIZE);
        eprintln!(
            "grid {}: worst drawn-vs-true error within 200 m {worst:.3} m",
            crate::gpu::SHARED_MIN_GRID_SIZE
        );
        assert!(
            worst < 0.25,
            "far LOD departs {worst:.3} m from the heightfield within 200 m"
        );
    }

    #[test]
    fn a_64_cell_grid_meets_15_cm_inside_200_m() {
        // The round-2 target (0.15 m) needs 64 cells per node; held back by the terrain budget
        // until a 7900 run confirms it (see docs/evidence/terrain-look/round2/README.md).
        let worst = worst_within_200_m(64);
        eprintln!("grid 64: worst drawn-vs-true error within 200 m {worst:.3} m");
        assert!(worst < 0.15, "64 cells: {worst:.3} m");
    }

    #[test]
    fn coarser_levels_depart_more_and_the_table_reports_it() {
        let map = plain();
        let mips = HeightMips::build(&map);
        let table = lod_height_error(&map, &mips, &CdlodConfig::default(), 6, 96, f32::INFINITY);
        for row in &table {
            eprintln!("{row:?}");
        }
        assert_eq!(table.len(), 7);
        assert!(table[0].max_m < 0.02, "leaf error {}", table[0].max_m);
        assert!(table[6].rms_m > table[2].rms_m);
        for row in &table {
            assert!(row.p99_m <= row.max_m && row.rms_m <= row.max_m);
        }
    }
}
