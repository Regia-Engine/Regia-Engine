// SPDX-License-Identifier: MPL-2.0
//! LOD seam check: walks every edge two selected CDLOD nodes share and measures how far the
//! two drawn edges are apart, on the CPU mirror of the vertex shader.
//!
//! [`crate::cdlod`] argues that a CDLOD selection never cracks: neighbours differ by at most one
//! level, the finer node's border vertices are fully morphed onto the coarser lattice, and the
//! coarser node's border vertices are not morphed at all. This module checks that argument on a
//! real selection instead of trusting it: for each shared edge it evaluates the border vertices
//! of both nodes exactly as `terrain.wgsl` `morph_grid` does (per-vertex morph factor from the
//! horizontal distance to the morph eye, height read from the box-filtered pyramid at
//! [`crate::height_mips::vertex_mip`]) and compares the two edge polylines at every vertex of
//! either. A crack shows up as a non-zero `max_gap_m`; a coarse border vertex that has started
//! to morph shows up in `coarse_edge_morphs`.
//!
//! Scope: nodes drawn on the terrain's regular grid. The dense cliff-relief nodes
//! (`grid_size` twice the terrain's) first morph their own border onto the regular grid in the
//! shader, so their border equals the regular node's and is covered by the same check.
//!
//! Used by the tests below, by `terrain.parity` on the agent protocol and by hosts that want a
//! seam assertion in their own tests.

use std::collections::HashSet;

use crate::cdlod::{
    CdlodConfig, LodSelection, NodeDraw, QuadNode, eval_node_vertex_filtered, quadtree_span, vertex_morph_t,
};
use crate::height_mips::HeightMips;
use crate::sampler::TerrainMap;

/// What [`lod_seam_report`] found over one selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct SeamReport {
    /// Selected nodes checked.
    pub nodes: usize,
    /// Shared edges between two nodes of the same level.
    pub same_level_edges: usize,
    /// Shared edges between a node and a coarser neighbour.
    pub level_change_edges: usize,
    /// Largest level difference across a shared edge (the guarantee is 1).
    pub max_level_jump: u32,
    /// Largest vertical distance between the two drawn edges of any shared edge, metres.
    pub max_gap_m: f32,
    /// World XZ of the largest gap (`None` when every edge matches exactly).
    pub worst_at: Option<[f32; 2]>,
    /// Border vertices of the coarser node of a level change whose morph factor is above
    /// 0.001 (each one is a T-junction the finer node does not follow).
    pub coarse_edge_morphs: usize,
    /// Border vertices of the finer node of a level change whose morph factor is below 0.999.
    pub fine_edge_unmorphed: usize,
}

/// Morph factors this close to 0 or 1 count as exact: a vertex exactly at the end of its band
/// may land one rounding step inside it.
const MORPH_SLACK: f32 = 1e-3;

impl SeamReport {
    /// `true` when no shared edge is apart by more than `tolerance_m` and no level jump
    /// exceeds one.
    #[must_use]
    pub fn is_crack_free(&self, tolerance_m: f32) -> bool {
        self.max_level_jump <= 1 && self.max_gap_m <= tolerance_m
    }
}

/// One drawn border vertex: position along the edge, height and morph factor.
#[derive(Clone, Copy, Debug)]
struct EdgeVertex {
    along: f32,
    height: f32,
    t: f32,
}

/// Which border of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    MinX,
    MaxX,
    MinZ,
    MaxZ,
}

impl Side {
    const ALL: [Side; 4] = [Side::MinX, Side::MaxX, Side::MinZ, Side::MaxZ];

    /// The opposite border (the one a neighbour across this border shares).
    fn opposite(self) -> Side {
        match self {
            Side::MinX => Side::MaxX,
            Side::MaxX => Side::MinX,
            Side::MinZ => Side::MaxZ,
            Side::MaxZ => Side::MinZ,
        }
    }
}

/// Inputs shared by every vertex evaluation of one report.
struct Ctx<'a> {
    map: &'a TerrainMap,
    mips: &'a HeightMips,
    config: &'a CdlodConfig,
    root_level: u32,
    eye: [f32; 2],
}

impl Ctx<'_> {
    /// The drawn border vertices of `draw` along `side`, sorted along the edge (X for the Z
    /// borders, Z for the X borders), as `morph_grid` + `vertex_at` place them.
    fn edge(&self, draw: &NodeDraw, side: Side) -> Vec<EdgeVertex> {
        let g = self.config.grid_size;
        let (min_x, min_z, size) = draw.node.draw_rect(self.map, self.root_level, g);
        let step = size / g as f32;
        let mut out = Vec::with_capacity(g as usize + 1);
        for i in 0..=g {
            let (gx, gz) = match side {
                Side::MinX => (0, i),
                Side::MaxX => (g, i),
                Side::MinZ => (i, 0),
                Side::MaxZ => (i, g),
            };
            // The shader's per-vertex distance: the unmorphed grid position to the eye.
            let fx = min_x + gx as f32 * step;
            let fz = min_z + gz as f32 * step;
            let d = ((fx - self.eye[0]).powi(2) + (fz - self.eye[1]).powi(2)).sqrt();
            let t = vertex_morph_t(draw.morph_start, draw.morph_end, d);
            let [x, height, z] = eval_node_vertex_filtered(
                self.map,
                self.mips,
                self.config,
                draw.node,
                self.root_level,
                gx,
                gz,
                t,
            );
            let along = match side {
                Side::MinX | Side::MaxX => z,
                Side::MinZ | Side::MaxZ => x,
            };
            out.push(EdgeVertex { along, height, t });
        }
        out.sort_by(|a, b| a.along.total_cmp(&b.along));
        out
    }
}

/// Heights of the polyline `edge` at `along` as an interval `(low, high)`, clamped to its ends.
/// Vertices at the same position (a morphing odd vertex that reached its even neighbour) may
/// read different mips: the drawn edge then has a vertical segment there, and every height
/// between them is on it. Elsewhere the interval is the one interpolated height.
fn polyline_heights(edge: &[EdgeVertex], along: f32) -> (f32, f32) {
    let (Some(first), Some(last)) = (edge.first(), edge.last()) else {
        return (0.0, 0.0);
    };
    let along = along.clamp(first.along, last.along);
    let mut at = edge.iter().filter(|v| v.along == along).map(|v| v.height);
    if let Some(h) = at.next() {
        return at.fold((h, h), |(lo, hi), h| (lo.min(h), hi.max(h)));
    }
    for pair in edge.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if b.along > a.along && along >= a.along && along <= b.along {
            let f = (along - a.along) / (b.along - a.along);
            let h = a.height + (b.height - a.height) * f;
            return (h, h);
        }
    }
    (last.height, last.height)
}

/// Vertical distance between two height intervals (0 when they overlap).
fn interval_gap(a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - b.1).max(b.0 - a.1).max(0.0)
}

/// The node across `side` of `node` at the same level, if it lies inside the map.
fn same_level_neighbour(node: QuadNode, side: Side, root_level: u32) -> Option<QuadNode> {
    let dim = 1u32 << (root_level - node.level);
    let (x, z) = match side {
        Side::MinX => (node.x.checked_sub(1)?, node.z),
        Side::MaxX => (node.x + 1, node.z),
        Side::MinZ => (node.x, node.z.checked_sub(1)?),
        Side::MaxZ => (node.x, node.z + 1),
    };
    (x < dim && z < dim).then_some(QuadNode {
        level: node.level,
        x,
        z,
    })
}

/// The edge segment `[lo, hi]` (along the edge axis) of `node`'s `side`.
fn side_span(node: QuadNode, side: Side, world_width: f32, root_level: u32, origin: [f32; 2]) -> (f32, f32) {
    let (min_x, min_z, size) = node.world_rect_at(world_width, root_level, origin);
    match side {
        Side::MinX | Side::MaxX => (min_z, min_z + size),
        Side::MinZ | Side::MaxZ => (min_x, min_x + size),
    }
}

/// The world XZ of the point `along` on `node`'s `side`.
fn side_point(
    node: QuadNode,
    side: Side,
    along: f32,
    world_width: f32,
    root_level: u32,
    origin: [f32; 2],
) -> [f32; 2] {
    let (min_x, min_z, size) = node.world_rect_at(world_width, root_level, origin);
    match side {
        Side::MinX => [min_x, along],
        Side::MaxX => [min_x + size, along],
        Side::MinZ => [along, min_z],
        Side::MaxZ => [along, min_z + size],
    }
}

/// Checks every shared edge of `selection` (selected from `eye`, the camera or a shadow
/// cascade's morph eye) for cracks. `mips` must be built from `map`
/// ([`HeightMips::build`]); the selection must have been made with `config` and `root_level`.
///
/// Cost: `O(nodes · grid_size · root_level)`; a few milliseconds for a typical selection. Meant
/// for tests and on-demand checks, not for every frame.
#[must_use]
pub fn lod_seam_report(
    map: &TerrainMap,
    mips: &HeightMips,
    config: &CdlodConfig,
    root_level: u32,
    selection: &LodSelection,
    eye: [f32; 2],
) -> SeamReport {
    let _span = tracing::info_span!(target: "regia_terrain", "lod_seam_report").entered();
    let ctx = Ctx {
        map,
        mips,
        config,
        root_level,
        eye,
    };
    let world_width = quadtree_span(
        map.quadtree_width(),
        map.quadtree_samples(),
        config.grid_size,
        root_level,
    );
    let selected: std::collections::HashMap<QuadNode, &NodeDraw> =
        selection.nodes.iter().map(|d| (d.node, d)).collect();
    let mut report = SeamReport {
        nodes: selection.nodes.len(),
        ..SeamReport::default()
    };
    let mut seen_same: HashSet<(QuadNode, QuadNode)> = HashSet::new();
    for draw in &selection.nodes {
        for side in Side::ALL {
            let Some(next) = same_level_neighbour(draw.node, side, root_level) else {
                continue;
            };
            // The neighbour is the same-level node, or the first selected ancestor of it.
            let mut found = None;
            let mut probe = next;
            loop {
                if let Some(other) = selected.get(&probe) {
                    found = Some(*other);
                    break;
                }
                if probe.level >= root_level {
                    break;
                }
                probe = QuadNode {
                    level: probe.level + 1,
                    x: probe.x / 2,
                    z: probe.z / 2,
                };
            }
            // Not selected at this level or coarser: finer nodes (they report this edge from
            // their side) or culled.
            let Some(other) = found else { continue };
            let jump = other.node.level - draw.node.level;
            if jump == 0 {
                let key = if (draw.node.x, draw.node.z) < (other.node.x, other.node.z) {
                    (draw.node, other.node)
                } else {
                    (other.node, draw.node)
                };
                if !seen_same.insert(key) {
                    continue;
                }
                report.same_level_edges += 1;
            } else {
                report.level_change_edges += 1;
                report.max_level_jump = report.max_level_jump.max(jump);
            }
            let fine = ctx.edge(draw, side);
            let coarse_all = ctx.edge(other, side.opposite());
            // The finer node's whole border lies on the coarser node's border.
            let (lo, hi) = side_span(draw.node, side, world_width, root_level, ctx.map.grid_origin());
            let eps = (hi - lo) * 1e-5;
            let coarse: Vec<EdgeVertex> = coarse_all
                .into_iter()
                .filter(|v| v.along >= lo - eps && v.along <= hi + eps)
                .collect();
            if jump > 0 {
                report.coarse_edge_morphs += coarse.iter().filter(|v| v.t > MORPH_SLACK).count();
                report.fine_edge_unmorphed += fine.iter().filter(|v| v.t < 1.0 - MORPH_SLACK).count();
            }
            let mut check = |along: f32| {
                let gap = interval_gap(polyline_heights(&fine, along), polyline_heights(&coarse, along));
                if gap > report.max_gap_m {
                    report.max_gap_m = gap;
                    report.worst_at = Some(side_point(
                        draw.node,
                        side,
                        along,
                        world_width,
                        root_level,
                        ctx.map.grid_origin(),
                    ));
                }
            };
            for v in fine.iter().chain(&coarse) {
                check(v.along);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdlod::{MinMaxPyramid, select_nodes};
    use regia_core::math::Rng;

    /// 257×257 samples over 256 m: rolling hills plus a sharp ridge, so a morphing border
    /// vertex is visibly off its neighbour's edge.
    fn hills() -> TerrainMap {
        let n = 257u32;
        let width = 256.0f32;
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
                let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
                let ridge = (6.0 - (x - 0.3 * z).abs() * 0.5).max(0.0);
                let h = 9.0 * (x * 0.031).sin() * (z * 0.027).cos()
                    + 4.0 * ((x + 2.0 * z) * 0.083).sin()
                    + 1.5 * (x * 0.41).sin() * (z * 0.37).sin()
                    + ridge;
                heights.push(h);
            }
        }
        TerrainMap::from_heights(n, n, heights, width, 1.0, 0.0).expect("valid map")
    }

    /// Worst report over `cameras` random eyes in and around the map.
    fn sweep(config: &CdlodConfig, cameras: usize, seed: u64) -> SeamReport {
        let map = hills();
        let mips = HeightMips::build(&map);
        let root_level = 5; // 8 m leaves
        let pyramid = MinMaxPyramid::build(&map, root_level).expect("pyramid");
        let mut rng = Rng::new(seed);
        let mut worst = SeamReport::default();
        for _ in 0..cameras {
            let eye = [rng.range(-300.0..300.0), rng.range(-300.0..300.0)];
            let selection = select_nodes(&map, &pyramid, config, root_level, eye, None);
            let r = lod_seam_report(&map, &mips, config, root_level, &selection, eye);
            assert!(r.nodes > 0);
            worst.level_change_edges += r.level_change_edges;
            worst.same_level_edges += r.same_level_edges;
            worst.coarse_edge_morphs += r.coarse_edge_morphs;
            worst.fine_edge_unmorphed += r.fine_edge_unmorphed;
            worst.max_level_jump = worst.max_level_jump.max(r.max_level_jump);
            if r.max_gap_m > worst.max_gap_m {
                worst.max_gap_m = r.max_gap_m;
                worst.worst_at = r.worst_at;
            }
        }
        worst
    }

    #[test]
    fn default_config_is_crack_free_over_a_camera_sweep() {
        let report = sweep(&CdlodConfig::default(), 120, 5);
        assert!(report.level_change_edges > 100, "{report:?}");
        assert!(report.same_level_edges > 100, "{report:?}");
        assert_eq!(report.coarse_edge_morphs, 0, "{report:?}");
        assert_eq!(report.fine_edge_unmorphed, 0, "{report:?}");
        assert!(report.is_crack_free(1e-4), "{report:?}");
    }

    /// The smallest range factor the config accepts with the widest morph band it accepts:
    /// the morph used to start inside the ring a finer neighbour's border reaches, so coarse
    /// border vertices slid off the finer node's lattice (a T-junction crack).
    #[test]
    fn widest_morph_band_at_the_minimum_range_factor_is_crack_free() {
        let config = CdlodConfig {
            range_factor: 2.0,
            morph_fraction: 0.5,
            ..CdlodConfig::with_grid_size(16)
        };
        assert!(config.validate().is_ok());
        let report = sweep(&config, 300, 17);
        assert!(report.level_change_edges > 100, "{report:?}");
        assert_eq!(report.coarse_edge_morphs, 0, "{report:?}");
        assert!(report.is_crack_free(1e-4), "{report:?}");
    }

    #[test]
    fn every_accepted_config_keeps_the_morph_out_of_the_finer_ring() {
        let map = hills();
        let root_level = 5;
        let pyramid = MinMaxPyramid::build(&map, root_level).expect("pyramid");
        for range_factor in [2.0f32, 2.25, 2.5, 3.0, 4.0, 6.0] {
            for morph_fraction in [0.05f32, 0.25, 0.3, 0.4, 0.5] {
                let config = CdlodConfig {
                    range_factor,
                    morph_fraction,
                    ..CdlodConfig::with_grid_size(16)
                };
                let selection = select_nodes(&map, &pyramid, &config, root_level, [10.0, -20.0], None);
                for d in &selection.nodes {
                    if d.node.level >= root_level {
                        continue;
                    }
                    let size = crate::cdlod::node_world_size(map.world_width(), root_level, d.node.level);
                    let range = range_factor * size;
                    assert!(
                        d.morph_start - range >= std::f32::consts::SQRT_2 * size * (1.0 - 1e-5),
                        "rf {range_factor} mf {morph_fraction} level {}: morph starts {} past the range, ring {}",
                        d.node.level,
                        d.morph_start - range,
                        std::f32::consts::SQRT_2 * size
                    );
                    assert!(d.morph_end > d.morph_start);
                }
            }
        }
    }

    #[test]
    fn shipped_configs_keep_their_morph_bands_bit_for_bit() {
        // The cap only binds where `(1 - morph_fraction) * range_factor < sqrt(2)`: every
        // configuration in use (DO27 profiles 2.5 / 3.5 / 5.0, the default 4.0, the 2.0 look
        // test, all with the default 0.25 band) keeps the band it had, so no image changes.
        for range_factor in [2.0f32, 2.5, 3.5, 4.0, 5.0] {
            let config = CdlodConfig {
                range_factor,
                ..CdlodConfig::default()
            };
            assert_eq!(config.effective_morph_fraction(), config.morph_fraction);
        }
        let capped = CdlodConfig {
            range_factor: 2.0,
            morph_fraction: 0.5,
            ..CdlodConfig::default()
        };
        let expected = 1.0 - std::f32::consts::SQRT_2 / 2.0;
        assert!((capped.effective_morph_fraction() - expected).abs() < 1e-6);
    }

    #[test]
    fn a_forced_morph_on_a_coarse_border_is_reported_as_a_gap() {
        // The checker itself: make every coarse node morph fully (morph band at distance 0)
        // and the level-change edges must show the T-junctions.
        let map = hills();
        let mips = HeightMips::build(&map);
        let config = CdlodConfig::with_grid_size(16);
        let root_level = 5;
        let pyramid = MinMaxPyramid::build(&map, root_level).expect("pyramid");
        let eye = [3.0, 7.0];
        let mut selection = select_nodes(&map, &pyramid, &config, root_level, eye, None);
        let clean = lod_seam_report(&map, &mips, &config, root_level, &selection, eye);
        assert!(clean.is_crack_free(1e-4), "{clean:?}");
        let finest = selection.nodes.iter().map(|d| d.node.level).min().unwrap_or(0);
        for d in &mut selection.nodes {
            if d.node.level > finest && d.node.level < root_level {
                d.morph_start = 0.0;
                d.morph_end = 1e-3;
            }
        }
        let broken = lod_seam_report(&map, &mips, &config, root_level, &selection, eye);
        assert!(broken.coarse_edge_morphs > 0, "{broken:?}");
        assert!(broken.max_gap_m > 0.01, "{broken:?}");
        assert!(broken.worst_at.is_some());
        assert!(!broken.is_crack_free(1e-4));
    }

    #[test]
    fn a_two_level_jump_is_reported() {
        let map = hills();
        let mips = HeightMips::build(&map);
        let config = CdlodConfig::with_grid_size(16);
        let root_level = 3;
        // Hand-made selection: one level-0 node next to a level-2 node (and the rest of the
        // map at level 2 / 3 is left out: only the reported jump matters here).
        let selection = LodSelection {
            nodes: vec![
                NodeDraw {
                    node: QuadNode { level: 0, x: 3, z: 0 },
                    morph_start: 1e6,
                    morph_end: 2e6,
                },
                NodeDraw {
                    node: QuadNode { level: 2, x: 1, z: 0 },
                    morph_start: 1e6,
                    morph_end: 2e6,
                },
            ],
        };
        let report = lod_seam_report(&map, &mips, &config, root_level, &selection, [0.0, 0.0]);
        assert_eq!(report.max_level_jump, 2, "{report:?}");
        assert_eq!(report.level_change_edges, 1);
        assert!(!report.is_crack_free(1.0));
    }
}
