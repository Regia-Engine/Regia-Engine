// SPDX-License-Identifier: MPL-2.0
//! The water mesh the GPU draws: one vertex buffer for every body of a frame.
//!
//! A ribbon becomes seven columns per centerline point: the two outer columns sit a wet band
//! beyond the banks (`WaterLook::wet_width`), so the wet-bank darkening has pixels to run on; the
//! other five span the water. Each vertex carries the surface velocity (the centerline speed times
//! [`lateral_profile`], along the local tangent), the unit vector to the right of the flow, and its
//! signed distance from the centerline. A plane becomes a level grid with a uniform drift.
//!
//! A plane with a swell (ADR-1620) is not in this mesh: it is drawn on the camera-centered ring
//! grid ([`build_ring_grid`], [`build_ring_mesh`]), whose vertices hold offsets from the grid's
//! center that the shader moves to the snapped camera, clamps to the body's rectangle and
//! displaces.

use bytemuck::{Pod, Zeroable};

use super::swell::RingGrid;
use super::{MAX_BODIES, WaterBody, WaterShape};

/// Grid cells per side of a plane body.
pub const PLANE_GRID: u32 = 8;

/// Half widths beyond which a vertex is "no bank" (a plane): the shader skips the lateral tests.
pub const PLANE_HALF_WIDTH: f32 = 1.0e9;

/// One water vertex (48 bytes; the layout `water_body.wgsl` reads).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct WaterVertex {
    /// World position on the water surface.
    pub position: [f32; 3],
    /// Index of the body in the frame's body table.
    pub body: u32,
    /// Surface velocity along x and z (m/s).
    pub flow: [f32; 2],
    /// Unit vector to the right of the flow, x and z (a plane: +x).
    pub right: [f32; 2],
    /// Signed distance from a ribbon's centerline (m); 0 on a plane.
    pub lateral: f32,
    /// Half the water width (m); [`PLANE_HALF_WIDTH`] on a plane.
    pub half_width: f32,
    /// Ripple coordinates (m) in the flow's frame: along the flow (`-right` turned a quarter, the
    /// tangent) and across it (`right`). A ribbon uses its distance from the source and its
    /// lateral offset, which stay continuous around bends; a plane uses its world position.
    pub ripple: [f32; 2],
}

/// The vertices and indices of a frame's water bodies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaterMesh {
    /// Vertices of every body.
    pub vertices: Vec<WaterVertex>,
    /// Triangle list indices.
    pub indices: Vec<u32>,
}

/// Velocity across a stream relative to its centerline (`x` = lateral distance over half width):
/// `(1 − x²)^¼`, 1 on the centerline, 0 at the banks and beyond. The quarter power keeps most of
/// the width near full speed and drops steeply in the last tenth, as a turbulent open-channel
/// profile does.
#[must_use]
pub fn lateral_profile(x: f32) -> f32 {
    let s = 1.0 - x * x;
    if s <= 0.0 { 0.0 } else { s.sqrt().sqrt() }
}

/// Builds the mesh of up to [`MAX_BODIES`] bodies, in order (body index = position in `bodies`).
/// Invalid bodies ([`WaterBody::validate`]) and those past the limit are skipped, and so is their
/// index: the returned list says which body index each input got. A swell plane
/// ([`WaterBody::active_swell`]) gets its index but no vertices (the ring grid draws it).
#[must_use]
pub fn build_mesh(bodies: &[&WaterBody]) -> (WaterMesh, Vec<Option<u32>>) {
    let mut mesh = WaterMesh::default();
    let mut slots = Vec::with_capacity(bodies.len());
    let mut next = 0u32;
    for body in bodies {
        if next as usize >= MAX_BODIES || body.validate().is_err() {
            slots.push(None);
            continue;
        }
        match &body.shape {
            WaterShape::Plane { .. } if body.active_swell().is_some() => {}
            WaterShape::Plane {
                center,
                half_extent,
                flow,
            } => push_plane(&mut mesh, next, *center, *half_extent, *flow),
            WaterShape::Ribbon { .. } => push_ribbon(&mut mesh, next, body),
        }
        slots.push(Some(next));
        next += 1;
    }
    (mesh, slots)
}

fn push_plane(mesh: &mut WaterMesh, body: u32, center: [f32; 3], half: [f32; 2], flow: [f32; 2]) {
    let base = mesh.vertices.len() as u32;
    let n = PLANE_GRID;
    for j in 0..=n {
        for i in 0..=n {
            let u = i as f32 / n as f32 * 2.0 - 1.0;
            let v = j as f32 / n as f32 * 2.0 - 1.0;
            mesh.vertices.push(WaterVertex {
                position: [center[0] + u * half[0], center[1], center[2] + v * half[1]],
                body,
                flow,
                right: [1.0, 0.0],
                lateral: 0.0,
                half_width: PLANE_HALF_WIDTH,
                // Tangent (right.y, -right.x) = (0, -1) and right (1, 0).
                ripple: [-(center[2] + v * half[1]), center[0] + u * half[0]],
            });
        }
    }
    push_grid_indices(mesh, base, n + 1, n + 1);
}

/// Lateral positions of a ribbon's seven columns, as multiples of the half width; the outer two
/// are replaced by the wet band.
const COLUMNS: [f32; 7] = [-1.0, -1.0, -0.5, 0.0, 0.5, 1.0, 1.0];

fn push_ribbon(mesh: &mut WaterMesh, body: u32, water: &WaterBody) {
    let WaterShape::Ribbon { points } = &water.shape else {
        return;
    };
    let speeds = water.point_speeds();
    let band = water.look.wet_width.max(0.05);
    let base = mesh.vertices.len() as u32;
    let count = points.len();
    let mut along = 0.0f32;
    for i in 0..count {
        if i > 0 {
            let a = points[i - 1].position;
            let b = points[i].position;
            along += ((b[0] - a[0]).powi(2) + (b[2] - a[2]).powi(2)).sqrt();
        }
        let prev = points[i.saturating_sub(1)].position;
        let next = points[(i + 1).min(count - 1)].position;
        let (mut tx, mut tz) = (next[0] - prev[0], next[2] - prev[2]);
        let len = (tx * tx + tz * tz).sqrt().max(1.0e-6);
        tx /= len;
        tz /= len;
        let right = [-tz, tx];
        let p = points[i];
        let half = 0.5 * p.width;
        for (c, frac) in COLUMNS.iter().enumerate() {
            let mut lateral = frac * half;
            if c == 0 {
                lateral -= band;
            } else if c == COLUMNS.len() - 1 {
                lateral += band;
            }
            let profile = lateral_profile(lateral / half.max(1.0e-4));
            let speed = speeds[i] * profile;
            mesh.vertices.push(WaterVertex {
                position: [
                    p.position[0] + right[0] * lateral,
                    p.position[1],
                    p.position[2] + right[1] * lateral,
                ],
                body,
                flow: [tx * speed, tz * speed],
                right,
                lateral,
                half_width: half,
                ripple: [along, lateral],
            });
        }
    }
    push_grid_indices(mesh, base, COLUMNS.len() as u32, count as u32);
}

/// The ring grid in offsets from its center (x, z) and its triangle list: an inner square of
/// `2 half_cells` cells of `cell` meters, then `rings − 1` rings, each a square annulus from the
/// last one's edge to twice as far, with cells twice as large. The first loop of each ring repeats
/// the last one's edge vertex for vertex (bit-identical offsets: `index · spacing` with a power-of-
/// two spacing), and the strip from it to the coarser loops is zipped 2:1, so there are no
/// T-junctions and the displaced surface has no cracks.
#[must_use]
pub fn build_ring_grid(grid: &RingGrid) -> (Vec<[f32; 2]>, Vec<u32>) {
    let (points, indices, _) = build_ring_grid_chunks(grid);
    (points, indices)
}

/// A run of the ring grid's triangles with a bound, for frustum culling (ADR-2342): the inner
/// square, or one side of one ring. The bound is in offsets from the grid's center.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RingChunk {
    /// First index of the run in one body's copy of the grid.
    pub first_index: u32,
    /// Indices in the run (a multiple of 3).
    pub index_count: u32,
    /// Lower corner (x, z) of the offsets the run's vertices span.
    pub min: [f32; 2],
    /// Upper corner (x, z).
    pub max: [f32; 2],
    /// How far the per-ring snap can move the run's vertices (m): zero for the inner square and
    /// for a grid whose rings do not snap.
    pub snap_pad: f32,
}

/// [`build_ring_grid`] plus the chunks: the inner square first, then each ring's four sides in
/// order (the triangles are ordered by chunk, so a chunk is one index range). The chunks tile the
/// index list exactly.
#[must_use]
pub fn build_ring_grid_chunks(grid: &RingGrid) -> (Vec<[f32; 2]>, Vec<u32>, Vec<RingChunk>) {
    let cell = grid.cell();
    let m = grid.half_cells.max(2) & !1;
    let mut points: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    // Inner square.
    let side = 2 * m + 1;
    for j in 0..side {
        for i in 0..side {
            let (x, z) = (i as i64 - i64::from(m), j as i64 - i64::from(m));
            points.push([x as f32 * cell, z as f32 * cell]);
        }
    }
    for j in 0..side - 1 {
        for i in 0..side - 1 {
            let a = j * side + i;
            let (b, c) = (a + 1, a + side);
            indices.extend_from_slice(&[a, c, b, b, c, c + 1]);
        }
    }
    let inner = m as f32 * cell;
    let mut chunks = vec![RingChunk {
        first_index: 0,
        index_count: indices.len() as u32,
        min: [-inner, -inner],
        max: [inner, inner],
        snap_pad: 0.0,
    }];
    for ring in 1..grid.rings {
        let fine = cell * (1u64 << (ring - 1)) as f32;
        let coarse = fine * 2.0;
        let mut loops: Vec<(u32, f32)> = vec![(m, fine)];
        loops.extend((m / 2 + 1..=m).map(|r| (r, coarse)));
        let mut prev = push_loop(&mut points, loops[0].0, loops[0].1);
        let mut strips: Vec<(Vec<u32>, Vec<u32>)> = Vec::new();
        for &(r, s) in &loops[1..] {
            let next = push_loop(&mut points, r, s);
            strips.push((prev, next.clone()));
            prev = next;
        }
        let ri = m as f32 * fine;
        let ro = m as f32 * coarse;
        for side in 0..4u64 {
            let first = indices.len() as u32;
            for (inner_loop, outer_loop) in &strips {
                zip_side(&mut indices, inner_loop, outer_loop, side);
            }
            let (min, max) = match side {
                0 => ([-ro, -ro], [ro, -ri]),
                1 => ([ri, -ro], [ro, ro]),
                2 => ([-ro, ri], [ro, ro]),
                _ => ([-ro, -ro], [-ri, ro]),
            };
            chunks.push(RingChunk {
                first_index: first,
                index_count: indices.len() as u32 - first,
                min,
                max,
                snap_pad: if grid.snaps_rings() {
                    cell * (1u64 << (ring + 1)) as f32
                } else {
                    0.0
                },
            });
        }
    }
    (points, indices, chunks)
}

/// Pushes the square loop of half size `r · spacing` (8r points, from the (−r, −r) corner along
/// +x, +z, −x, −z) and returns its point indices.
fn push_loop(points: &mut Vec<[f32; 2]>, r: u32, spacing: f32) -> Vec<u32> {
    let r = i64::from(r);
    let mut out = Vec::with_capacity(8 * r as usize);
    for side in 0..4 {
        for m in 0..2 * r {
            let (x, z) = match side {
                0 => (-r + m, -r),
                1 => (r, -r + m),
                2 => (r - m, r),
                _ => (-r, r - m),
            };
            out.push(points.len() as u32);
            points.push([x as f32 * spacing, z as f32 * spacing]);
        }
    }
    out
}

/// Triangulates one side of the strip between two concentric square loops, advancing on
/// whichever loop's next point comes first along the side.
fn zip_side(indices: &mut Vec<u32>, inner: &[u32], outer: &[u32], side: u64) {
    let na = (inner.len() / 4) as u64;
    let nb = (outer.len() / 4) as u64;
    {
        let a = |i: u64| inner[((side * na + i) % (4 * na)) as usize];
        let b = |j: u64| outer[((side * nb + j) % (4 * nb)) as usize];
        let (mut i, mut j) = (0u64, 0u64);
        while i < na || j < nb {
            // Advance the inner loop when its next point is not past the outer loop's next one.
            if j == nb || (i < na && (i + 1) * nb <= (j + 1) * na) {
                indices.extend_from_slice(&[a(i), b(j), a(i + 1)]);
                i += 1;
            } else {
                indices.extend_from_slice(&[a(i), b(j), b(j + 1)]);
                j += 1;
            }
        }
    }
}

/// The ring grid's vertices for each swell plane `(body index, drift)`, one copy per body: the
/// position holds the offset from the grid's center (`y` = 0), and the shader places, clamps and
/// displaces it. Ripple coordinates follow from the placed point in the shader.
#[must_use]
pub fn build_ring_mesh(grid: &RingGrid, bodies: &[(u32, [f32; 2])]) -> WaterMesh {
    let (points, indices) = build_ring_grid(grid);
    let mut mesh = WaterMesh::default();
    for (body, flow) in bodies {
        let base = mesh.vertices.len() as u32;
        mesh.vertices.extend(points.iter().map(|p| WaterVertex {
            position: [p[0], 0.0, p[1]],
            body: *body,
            flow: *flow,
            right: [1.0, 0.0],
            lateral: 0.0,
            half_width: PLANE_HALF_WIDTH,
            ripple: [0.0, 0.0],
        }));
        mesh.indices.extend(indices.iter().map(|i| base + i));
    }
    mesh
}

/// Two triangles per cell of a `columns × rows` vertex grid stored row by row.
fn push_grid_indices(mesh: &mut WaterMesh, base: u32, columns: u32, rows: u32) {
    for j in 0..rows.saturating_sub(1) {
        for i in 0..columns.saturating_sub(1) {
            let a = base + j * columns + i;
            let b = a + 1;
            let c = a + columns;
            let d = c + 1;
            mesh.indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water_body::{RibbonPoint, WaterLook};

    fn bend() -> WaterBody {
        let points = (0..5)
            .map(|i| {
                let a = i as f32 * 0.3;
                RibbonPoint {
                    position: [10.0 * a.cos(), 5.0 - 0.1 * i as f32, 10.0 * a.sin()],
                    width: 2.0,
                    depth: 0.3,
                    speed: None,
                }
            })
            .collect();
        WaterBody::ribbon(points, WaterLook::clear_stream())
    }

    #[test]
    fn vertex_is_48_bytes() {
        assert_eq!(size_of::<WaterVertex>(), 48);
    }

    #[test]
    fn ribbon_has_seven_columns_with_a_wet_band() {
        let body = bend();
        let (mesh, slots) = build_mesh(&[&body]);
        assert_eq!(slots, vec![Some(0)]);
        assert_eq!(mesh.vertices.len(), 5 * 7);
        assert_eq!(mesh.indices.len(), 4 * 6 * 6);
        assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.vertices.len()));
        let row = &mesh.vertices[14..21];
        let band = body.look.wet_width;
        assert!((row[0].lateral + 1.0 + band).abs() < 1.0e-5, "{:?}", row[0]);
        assert!((row[6].lateral - 1.0 - band).abs() < 1.0e-5);
        assert_eq!(row[3].lateral, 0.0);
        // Ripple coordinates: distance from the source along the bend, lateral across.
        let arc = 2.0 * 10.0 * (0.15f32).sin() * 2.0;
        assert!((row[3].ripple[0] - arc).abs() < 1.0e-3, "{:?}", row[3].ripple);
        assert_eq!(row[0].ripple[1], row[0].lateral);
        // The surface follows the centerline height (sloped), the banks are still.
        assert!((row[3].position[1] - 4.8).abs() < 1.0e-5);
        assert_eq!(row[0].flow, [0.0, 0.0]);
        assert_eq!(row[1].flow, [0.0, 0.0]);
        let center_speed = (row[3].flow[0].powi(2) + row[3].flow[1].powi(2)).sqrt();
        let half_speed = (row[2].flow[0].powi(2) + row[2].flow[1].powi(2)).sqrt();
        assert!(center_speed > half_speed && half_speed > 0.0);
        // Right is perpendicular to the flow and points to +lateral.
        for v in row {
            let dot = v.right[0] * row[3].flow[0] + v.right[1] * row[3].flow[1];
            assert!(dot.abs() < 1.0e-4);
        }
        let side = [
            row[5].position[0] - row[3].position[0],
            row[5].position[2] - row[3].position[2],
        ];
        assert!(side[0] * row[3].right[0] + side[1] * row[3].right[1] > 0.99);
    }

    #[test]
    fn plane_grid_and_invalid_bodies() {
        let lake = WaterBody::plane([0.0, 1.0, 0.0], [20.0, 10.0], WaterLook::lake());
        let mut bad = bend();
        bad.look.roughness = -1.0;
        let (mesh, slots) = build_mesh(&[&bad, &lake, &bend()]);
        assert_eq!(slots, vec![None, Some(0), Some(1)]);
        let grid = ((PLANE_GRID + 1) * (PLANE_GRID + 1)) as usize;
        assert_eq!(mesh.vertices.len(), grid + 35);
        assert!(
            mesh.vertices[..grid]
                .iter()
                .all(|v| v.body == 0 && v.position[1] == 1.0)
        );
        assert!(mesh.vertices[grid..].iter().all(|v| v.body == 1));
        let xs: Vec<f32> = mesh.vertices[..grid].iter().map(|v| v.position[0]).collect();
        assert_eq!(xs.iter().copied().fold(f32::MAX, f32::min), -20.0);
        assert_eq!(xs.iter().copied().fold(f32::MIN, f32::max), 20.0);
    }

    #[test]
    fn at_most_max_bodies() {
        let lake = WaterBody::plane([0.0, 1.0, 0.0], [2.0, 2.0], WaterLook::lake());
        let many: Vec<&WaterBody> = std::iter::repeat_n(&lake, MAX_BODIES + 3).collect();
        let (_, slots) = build_mesh(&many);
        assert_eq!(slots.iter().flatten().count(), MAX_BODIES);
        assert_eq!(slots[MAX_BODIES], None);
    }

    fn grid(half_cells: u32, rings: u32) -> RingGrid {
        RingGrid {
            cell_bits: 0.5f32.to_bits(),
            half_cells,
            rings,
        }
    }

    /// An edge as its two end points' bit patterns, smaller first.
    type Edge = ((u32, u32), (u32, u32));

    /// Every edge of the ring grid is shared by two triangles except the outer border's: no
    /// T-junctions, no cracks. Positions are compared bit for bit (the shared loops repeat them).
    #[test]
    fn ring_grid_is_watertight() {
        use std::collections::HashMap;
        for (m, rings) in [(4u32, 4u32), (16, 3), (6, 2)] {
            let g = grid(m, rings);
            let (points, indices) = build_ring_grid(&g);
            assert!(indices.len().is_multiple_of(3));
            let key = |i: u32| {
                let p = points[i as usize];
                (p[0].to_bits(), p[1].to_bits())
            };
            let mut edges: HashMap<Edge, u32> = HashMap::new();
            let mut area = 0.0f64;
            for t in indices.as_chunks::<3>().0 {
                let [a, b, c] = *t;
                let (pa, pb, pc) = (points[a as usize], points[b as usize], points[c as usize]);
                let cross = f64::from((pb[0] - pa[0]) * (pc[1] - pa[1]) - (pb[1] - pa[1]) * (pc[0] - pa[0]));
                assert!(cross.abs() > 1.0e-9, "degenerate triangle {t:?}");
                area += 0.5 * cross.abs();
                for (u, v) in [(a, b), (b, c), (c, a)] {
                    let (ku, kv) = (key(u), key(v));
                    let e = if ku < kv { (ku, kv) } else { (kv, ku) };
                    *edges.entry(e).or_default() += 1;
                }
            }
            let half = g.outer_half_size();
            let border = |k: (u32, u32)| {
                let (x, z) = (f32::from_bits(k.0), f32::from_bits(k.1));
                x.abs() == half || z.abs() == half
            };
            for (e, n) in &edges {
                let on_border = border(e.0) && border(e.1);
                assert_eq!(
                    *n,
                    if on_border { 1 } else { 2 },
                    "edge {e:?} (m {m}, rings {rings})"
                );
            }
            // The triangles tile the whole square once.
            let want = f64::from(2.0 * half).powi(2);
            assert!((area - want).abs() < 1.0e-6 * want, "{area} vs {want}");
        }
    }

    #[test]
    fn ring_mesh_has_one_copy_per_swell_body() {
        let g = grid(4, 3);
        let (points, indices) = build_ring_grid(&g);
        let mesh = build_ring_mesh(&g, &[(2, [0.1, 0.0]), (5, [0.0, 0.0])]);
        assert_eq!(mesh.vertices.len(), 2 * points.len());
        assert_eq!(mesh.indices.len(), 2 * indices.len());
        assert!(
            mesh.vertices[..points.len()]
                .iter()
                .all(|v| v.body == 2 && v.flow == [0.1, 0.0])
        );
        assert!(mesh.vertices[points.len()..].iter().all(|v| v.body == 5));
        assert!(
            mesh.indices[indices.len()..]
                .iter()
                .all(|i| *i as usize >= points.len())
        );
    }

    #[test]
    fn swell_planes_leave_the_static_mesh() {
        let mut sea = WaterBody::plane([0.0, 0.0, 0.0], [100.0, 100.0], WaterLook::ocean());
        sea.swell = Some(crate::water_body::Swell::open_sea([1.0, 0.0], 1.0, 30.0, 4));
        let lake = WaterBody::plane([0.0, 1.0, 0.0], [20.0, 10.0], WaterLook::lake());
        let (mesh, slots) = build_mesh(&[&sea, &lake]);
        assert_eq!(slots, vec![Some(0), Some(1)]);
        let grid = ((PLANE_GRID + 1) * (PLANE_GRID + 1)) as usize;
        assert_eq!(mesh.vertices.len(), grid);
        assert!(mesh.vertices.iter().all(|v| v.body == 1));
    }

    #[test]
    fn profile_shape() {
        assert_eq!(lateral_profile(0.0), 1.0);
        assert_eq!(lateral_profile(1.0), 0.0);
        assert_eq!(lateral_profile(-1.5), 0.0);
        assert!(lateral_profile(0.5) > 0.9);
    }
}
