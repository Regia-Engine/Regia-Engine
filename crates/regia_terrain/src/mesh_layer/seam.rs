// SPDX-License-Identifier: MPL-2.0
//! Ring attach and the seam report (spec 26 § 2a.4 step 5 and § 2a.11, ADR-8291).
//!
//! Each ring vertex is placed on the triangle the CDLOD draw would use: the finest selected
//! node's reference triangle, barycentric weights in the unmorphed grid, and the morphed
//! corners from [`eval_node_vertex_filtered`]. The seam report walks the straight segment
//! between those placed vertices. A lattice edge lies in one reference triangle, and a fully
//! morphed border vertex snaps onto that triangle's coarse edge, so the segment stays on the
//! drawn surface.

use crate::Terrain;
use crate::cdlod::{
    CdlodConfig, LodSelection, NodeDraw, QuadNode, eval_node_vertex_filtered, vertex_morph_t,
};
use crate::height_mips::HeightMips;
use crate::sampler::TerrainMap;

use super::{SeamMouthReport, TerrainMeshLayers};

/// One ring vertex placed on the drawn triangle.
#[derive(Clone, Copy, Debug)]
pub struct RingAttach {
    /// The morphed position, barycentric in the drawn corners.
    pub position: [f32; 3],
    /// The node whose grid holds the vertex. The root when the selection misses it.
    pub node: QuadNode,
    /// Grid columns of the three corners, matching [`Self::bary`].
    pub corners_gx: [u32; 3],
    /// Grid rows of the three corners.
    pub corners_gz: [u32; 3],
    /// Barycentric weights of the unmorphed corners. They sum to 1.
    pub bary: [f32; 3],
    /// Morph factor of each corner (`1` = parent grid).
    pub morph_t: [f32; 3],
    /// `false` when no selected node covers the vertex (the answer is the LOD-0 sample).
    pub covered: bool,
}

/// Places every vertex of `ring` on the drawn terrain.
///
/// `ring` positions contribute their XZ only; the height is the morphed triangle. The same
/// function serves the seam report and the tests.
#[must_use]
pub fn attach_ring(
    terrain: &Terrain,
    mips: &HeightMips,
    selection: &LodSelection,
    morph_eye: [f32; 2],
    ring: &[[f32; 3]],
) -> Vec<[f32; 3]> {
    attach_ring_detail(terrain, mips, selection, morph_eye, ring)
        .into_iter()
        .map(|v| v.position)
        .collect()
}

/// [`attach_ring`] plus the corners, weights and morph the placement used.
#[must_use]
pub fn attach_ring_detail(
    terrain: &Terrain,
    mips: &HeightMips,
    selection: &LodSelection,
    morph_eye: [f32; 2],
    ring: &[[f32; 3]],
) -> Vec<RingAttach> {
    let _span = tracing::info_span!("terrain_attach_ring", vertices = ring.len()).entered();
    ring.iter()
        .map(|p| place_one(terrain, mips, selection, morph_eye, p[0], p[2]).attach)
        .collect()
}

/// A placed ring vertex plus the drawn corners of its reference triangle.
struct Placed {
    attach: RingAttach,
    corners: [[f32; 3]; 3],
}

/// The seam of every open mouth under one already-built selection.
///
/// `seam_report` builds the height mips itself. This takes them so a sweep can reuse one
/// pyramid.
#[must_use]
pub fn seam_report_with_mips(
    terrain: &Terrain,
    mips: &HeightMips,
    layers: &TerrainMeshLayers,
    selection: &LodSelection,
    eye: [f32; 2],
) -> SeamMouthReport {
    let _span = tracing::info_span!("terrain_seam_report").entered();
    let mut report = SeamMouthReport {
        max_gap_m: 0.0,
        max_lip_m: 0.0,
        worst_at: [0.0, 0.0, 0.0],
        ring_vertices: 0,
        pass_levels: Vec::new(),
    };
    for mouth in layers.mouths() {
        if !mouth.open {
            continue;
        }
        for ring in &mouth.rings {
            report.ring_vertices += ring.len() as u32;
            if ring.len() < 2 {
                continue;
            }
            let placed: Vec<Placed> = ring
                .iter()
                .map(|p| place_one(terrain, mips, selection, eye, p[0], p[2]))
                .collect();
            for v in &placed {
                if !report.pass_levels.contains(&v.attach.node.level) {
                    report.pass_levels.push(v.attach.node.level);
                }
            }
            let n = placed.len();
            for i in 0..n {
                let p0 = ring[i];
                let p1 = ring[(i + 1) % n];
                let a = placed[i].attach.position;
                let b = placed[(i + 1) % n].attach.position;
                // The lattice edge's own triangle, plus each endpoint's. A border vertex can
                // sit on the finer node while the edge itself lies in the coarser one.
                let mid = place_one(
                    terrain,
                    mips,
                    selection,
                    eye,
                    (p0[0] + p1[0]) * 0.5,
                    (p0[2] + p1[2]) * 0.5,
                );
                let tris = [placed[i].corners, placed[(i + 1) % n].corners, mid.corners];
                let nodes = [
                    placed[i].attach.node,
                    placed[(i + 1) % n].attach.node,
                    mid.attach.node,
                ];
                for s in 0..8 {
                    let t = s as f32 / 8.0;
                    let sample = [
                        a[0] + (b[0] - a[0]) * t,
                        a[1] + (b[1] - a[1]) * t,
                        a[2] + (b[2] - a[2]) * t,
                    ];
                    let miss = surface_miss(terrain, mips, selection, eye, sample, &tris, &nodes);
                    // Positive miss: the ring is above the drawn surface (a lip).
                    let gap = (-miss).max(0.0);
                    let lip = miss.max(0.0);
                    if gap > report.max_gap_m {
                        report.max_gap_m = gap;
                        report.worst_at = sample;
                    }
                    if lip > report.max_lip_m {
                        report.max_lip_m = lip;
                        if lip >= gap {
                            report.worst_at = sample;
                        }
                    }
                }
            }
        }
    }
    report
}

fn place_one(
    terrain: &Terrain,
    mips: &HeightMips,
    selection: &LodSelection,
    eye: [f32; 2],
    x: f32,
    z: f32,
) -> Placed {
    place_on(
        &terrain.map,
        &terrain.cdlod,
        terrain.root_level,
        mips,
        selection,
        eye,
        x,
        z,
    )
}

/// [`attach_ring`] on a draw's own map, CDLOD config and root level (no [`Terrain`] component):
/// the terrain draw places the mouth bands with it every pass (spec 26 § 2a.4 step 5).
#[must_use]
pub fn attach_ring_on(
    map: &TerrainMap,
    config: &CdlodConfig,
    root_level: u32,
    mips: &HeightMips,
    selection: &LodSelection,
    morph_eye: [f32; 2],
    ring: &[[f32; 3]],
) -> Vec<[f32; 3]> {
    ring.iter()
        .map(|p| {
            place_on(map, config, root_level, mips, selection, morph_eye, p[0], p[2])
                .attach
                .position
        })
        .collect()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the attach inputs of one point; a struct would only rename them"
)]
fn place_on(
    map: &TerrainMap,
    config: &CdlodConfig,
    root_level: u32,
    mips: &HeightMips,
    selection: &LodSelection,
    eye: [f32; 2],
    x: f32,
    z: f32,
) -> Placed {
    let grid = config.grid_size.max(1);
    let mut best: Option<&NodeDraw> = None;
    for draw in &selection.nodes {
        let (min_x, min_z, size) = draw.node.draw_rect(map, root_level, grid);
        if contains_half_open(min_x, min_z, size, x, z) && best.is_none_or(|b| draw.node.level < b.node.level)
        {
            best = Some(draw);
        }
    }
    if best.is_none() {
        for draw in &selection.nodes {
            let (min_x, min_z, size) = draw.node.draw_rect(map, root_level, grid);
            if contains_closed(min_x, min_z, size, x, z)
                && best.is_none_or(|b| draw.node.level < b.node.level)
            {
                best = Some(draw);
            }
        }
    }
    let Some(draw) = best else {
        return uncovered(map, root_level, x, z);
    };
    let (min_x, min_z, size) = draw.node.draw_rect(map, root_level, grid);
    let step = size / grid as f32;
    let gx = if step > 0.0 {
        ((x - min_x) / step).clamp(0.0, grid as f32)
    } else {
        0.0
    };
    let gz = if step > 0.0 {
        ((z - min_z) / step).clamp(0.0, grid as f32)
    } else {
        0.0
    };
    let cx = (gx.floor() as u32).min(grid - 1);
    let cz = (gz.floor() as u32).min(grid - 1);
    let u = (gx - cx as f32).clamp(0.0, 1.0);
    let v = (gz - cz as f32).clamp(0.0, 1.0);
    let (corners_gx, corners_gz, bary) = if u + v <= 1.0 {
        ([cx, cx, cx + 1], [cz, cz + 1, cz], [1.0 - u - v, v, u])
    } else {
        (
            [cx + 1, cx, cx + 1],
            [cz + 1, cz + 1, cz],
            [u + v - 1.0, 1.0 - u, 1.0 - v],
        )
    };
    let mut morph_t = [0.0; 3];
    let mut drawn = [[0.0; 3]; 3];
    for i in 0..3 {
        let fx = min_x + corners_gx[i] as f32 * step;
        let fz = min_z + corners_gz[i] as f32 * step;
        let d = ((fx - eye[0]).powi(2) + (fz - eye[1]).powi(2)).sqrt();
        morph_t[i] = vertex_morph_t(draw.morph_start, draw.morph_end, d);
        drawn[i] = eval_node_vertex_filtered(
            map,
            mips,
            config,
            draw.node,
            root_level,
            corners_gx[i],
            corners_gz[i],
            morph_t[i],
        );
    }
    let bary = clean_bary(bary);
    let position = [
        bary[0] * drawn[0][0] + bary[1] * drawn[1][0] + bary[2] * drawn[2][0],
        bary[0] * drawn[0][1] + bary[1] * drawn[1][1] + bary[2] * drawn[2][1],
        bary[0] * drawn[0][2] + bary[1] * drawn[1][2] + bary[2] * drawn[2][2],
    ];
    Placed {
        corners: drawn,
        attach: RingAttach {
            position,
            node: draw.node,
            corners_gx,
            corners_gz,
            bary,
            morph_t,
            covered: true,
        },
    }
}

fn clean_bary(mut b: [f32; 3]) -> [f32; 3] {
    for c in &mut b {
        if *c < 0.0 && *c > -1.0e-5 {
            *c = 0.0;
        }
    }
    let s = b[0] + b[1] + b[2];
    if s > 0.0 {
        b[0] /= s;
        b[1] /= s;
        b[2] /= s;
    }
    b
}

fn contains_half_open(min_x: f32, min_z: f32, size: f32, x: f32, z: f32) -> bool {
    x >= min_x && z >= min_z && x < min_x + size && z < min_z + size
}

fn contains_closed(min_x: f32, min_z: f32, size: f32, x: f32, z: f32) -> bool {
    x >= min_x && z >= min_z && x <= min_x + size + 1.0e-5 && z <= min_z + size + 1.0e-5
}

fn uncovered(map: &TerrainMap, root_level: u32, x: f32, z: f32) -> Placed {
    let position = [x, map.sample_bilinear(x, z), z];
    Placed {
        corners: [position, position, position],
        attach: RingAttach {
            position,
            node: QuadNode::root(root_level),
            corners_gx: [0, 0, 0],
            corners_gz: [0, 0, 0],
            bary: [1.0, 0.0, 0.0],
            morph_t: [0.0, 0.0, 0.0],
            covered: false,
        },
    }
}

/// Smallest vertical miss of `sample` against the hint triangles and the cells around
/// `sample`'s XZ on each hint node. Morph moves a vertex by at most one grid step, so the
/// drawn segment can sit on a neighbour cell of the reference triangle.
fn surface_miss(
    terrain: &Terrain,
    mips: &HeightMips,
    selection: &LodSelection,
    eye: [f32; 2],
    sample: [f32; 3],
    hints: &[[[f32; 3]; 3]],
    nodes: &[QuadNode],
) -> f32 {
    let mut best: Option<f32> = None;
    for tri in hints {
        if let Some(miss) = vertical_miss(tri, sample) {
            keep_tighter(&mut best, miss);
        }
    }
    if best.is_some_and(|m| m.abs() <= 1.0e-5) {
        return best.unwrap_or(0.0);
    }
    for node in nodes {
        let Some(draw) = selection.nodes.iter().find(|d| d.node == *node) else {
            continue;
        };
        if let Some(miss) = miss_on_node(terrain, mips, draw, eye, sample) {
            keep_tighter(&mut best, miss);
        }
    }
    best.unwrap_or_else(|| off_surface(hints, sample))
}

fn keep_tighter(best: &mut Option<f32>, miss: f32) {
    match *best {
        None => *best = Some(miss),
        Some(old) if miss.abs() < old.abs() => *best = Some(miss),
        Some(_) => {}
    }
}

/// Best vertical miss of the cells whose unmorphed rect is within one step of `sample`.
fn miss_on_node(
    terrain: &Terrain,
    mips: &HeightMips,
    draw: &NodeDraw,
    eye: [f32; 2],
    sample: [f32; 3],
) -> Option<f32> {
    let map = &terrain.map;
    let config = &terrain.cdlod;
    let grid = config.grid_size.max(1);
    let (min_x, min_z, size) = draw.node.draw_rect(map, terrain.root_level, grid);
    let step = size / grid as f32;
    if step <= 0.0 {
        return None;
    }
    let gx = (sample[0] - min_x) / step;
    let gz = (sample[2] - min_z) / step;
    if gx < -1.0 || gz < -1.0 || gx > grid as f32 + 1.0 || gz > grid as f32 + 1.0 {
        return None;
    }
    let corner = |ix: u32, iz: u32| -> [f32; 3] {
        let fx = min_x + ix as f32 * step;
        let fz = min_z + iz as f32 * step;
        let dist = ((fx - eye[0]).powi(2) + (fz - eye[1]).powi(2)).sqrt();
        let morph = vertex_morph_t(draw.morph_start, draw.morph_end, dist);
        eval_node_vertex_filtered(map, mips, config, draw.node, terrain.root_level, ix, iz, morph)
    };
    let last = grid as i32 - 1;
    let cx0 = (gx.floor() as i32 - 1).clamp(0, last);
    let cz0 = (gz.floor() as i32 - 1).clamp(0, last);
    let cx1 = (gx.floor() as i32 + 1).clamp(0, last);
    let cz1 = (gz.floor() as i32 + 1).clamp(0, last);
    let mut best: Option<f32> = None;
    for cz in cz0..=cz1 {
        for cx in cx0..=cx1 {
            let cx = cx as u32;
            let cz = cz as u32;
            let lower = [corner(cx, cz), corner(cx, cz + 1), corner(cx + 1, cz)];
            let upper = [corner(cx + 1, cz + 1), corner(cx, cz + 1), corner(cx + 1, cz)];
            for tri in [lower, upper] {
                let Some(miss) = vertical_miss(&tri, sample) else {
                    continue;
                };
                best = Some(best.map_or(miss, |old| if miss.abs() < old.abs() { miss } else { old }));
            }
        }
    }
    best
}

/// `sample.y - surface_y` when `sample`'s XZ lies on `tri`.
///
/// A partially morphed cell collapses to a sliver. Two of its edges can then cover the same
/// XZ with different mip heights, and the drawn triangle fills the vertical interval between
/// them (the same rule as `lod_seams`). A sample inside that interval is on the surface.
fn vertical_miss(tri: &[[f32; 3]; 3], sample: [f32; 3]) -> Option<f32> {
    let mut best = bary_height(tri, sample[0], sample[2]).map(|y| sample[1] - y);
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    let mut hits = 0u32;
    for e in 0..3 {
        let Some((y0, y1)) = edge_cover(tri[e], tri[(e + 1) % 3], sample[0], sample[2]) else {
            continue;
        };
        hits += 1;
        lo = lo.min(y0.min(y1));
        hi = hi.max(y0.max(y1));
    }
    if hits > 0 {
        let miss = if sample[1] < lo {
            sample[1] - lo
        } else if sample[1] > hi {
            sample[1] - hi
        } else {
            0.0
        };
        best = Some(match best {
            Some(old) if old.abs() <= miss.abs() => old,
            _ => miss,
        });
    }
    best
}

/// Height on `tri` at `(x, z)` when the triangle covers that XZ.
fn bary_height(tri: &[[f32; 3]; 3], x: f32, z: f32) -> Option<f32> {
    let (x0, z0) = (tri[0][0], tri[0][2]);
    let (x1, z1) = (tri[1][0], tri[1][2]);
    let (x2, z2) = (tri[2][0], tri[2][2]);
    let den = (z1 - z2) * (x0 - x2) + (x2 - x1) * (z0 - z2);
    if den.abs() <= 1.0e-8 {
        return None;
    }
    let b0 = ((z1 - z2) * (x - x2) + (x2 - x1) * (z - z2)) / den;
    let b1 = ((z2 - z0) * (x - x2) + (x0 - x2) * (z - z2)) / den;
    let b2 = 1.0 - b0 - b1;
    if b0 < -1.0e-4 || b1 < -1.0e-4 || b2 < -1.0e-4 {
        return None;
    }
    let b0 = b0.max(0.0);
    let b1 = b1.max(0.0);
    let b2 = b2.max(0.0);
    let s = b0 + b1 + b2;
    if s <= 0.0 {
        return None;
    }
    Some((b0 * tri[0][1] + b1 * tri[1][1] + b2 * tri[2][1]) / s)
}

/// Y range of segment `a`--`b` at `(x, z)` when the segment's XZ comes within 0.1 mm.
fn edge_cover(a: [f32; 3], b: [f32; 3], x: f32, z: f32) -> Option<(f32, f32)> {
    let abx = b[0] - a[0];
    let abz = b[2] - a[2];
    let len2 = abx * abx + abz * abz;
    if len2 <= 1.0e-12 {
        let dx = x - a[0];
        let dz = z - a[2];
        if dx * dx + dz * dz > 1.0e-8 {
            return None;
        }
        return Some((a[1].min(b[1]), a[1].max(b[1])));
    }
    let t = (((x - a[0]) * abx + (z - a[2]) * abz) / len2).clamp(0.0, 1.0);
    let cx = a[0] + abx * t;
    let cz = a[2] + abz * t;
    let dx = x - cx;
    let dz = z - cz;
    if dx * dx + dz * dz > 1.0e-8 {
        return None;
    }
    let cy = a[1] + (b[1] - a[1]) * t;
    Some((cy, cy))
}

/// Distance from `p` to the nearest hint edge, when no triangle covers it.
fn off_surface(tris: &[[[f32; 3]; 3]], p: [f32; 3]) -> f32 {
    let mut best = f32::MAX;
    for tri in tris {
        for e in 0..3 {
            best = best.min(dist_point_seg(tri[e], tri[(e + 1) % 3], p));
        }
    }
    best
}

fn dist_point_seg(a: [f32; 3], b: [f32; 3], p: [f32; 3]) -> f32 {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
    let t = if len2 <= 1.0e-20 {
        0.0
    } else {
        (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1] + (p[2] - a[2]) * ab[2]) / len2).clamp(0.0, 1.0)
    };
    let d = [
        p[0] - (a[0] + ab[0] * t),
        p[1] - (a[1] + ab[1] * t),
        p[2] - (a[2] + ab[2] * t),
    ];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}
