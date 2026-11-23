// SPDX-License-Identifier: MPL-2.0
//! Probe leak control at bake time (ADR-366): which grid neighbors of a probe are behind a wall.
//!
//! The distance moments of a probe ([`crate::probe`]) see a thin wall only as well as their
//! octahedral texels resolve it: a 4×4 map averages a 20 cm partition with the open space around
//! it, and the Chebyshev bound then lets the light of the far side through. This module answers
//! the question exactly, on the CPU, for the segments the volume interpolates along: the straight
//! segment between two neighboring probes either crosses scene geometry (a mesh triangle, or the
//! terrain surface) or it does not.
//!
//! - [`LeakGeometry`]: one BVH per mesh in object space, placed by the inverse of each instance
//!   transform ([`crate::bvh`]), plus the terrain heightfield. An instance whose transform cannot
//!   be inverted is expanded into world space instead, so a singular placement is not dropped.
//! - [`NeighborWalls::scan`]: for every probe, a mask of its 26 grid neighbors (same terrain
//!   layer or brick, [`VolumeLayout::neighbors`]) whose segment crosses geometry, and for the
//!   three neighbors ahead of it along +X, +Y and +Z where the wall lies on the segment. The bake
//!   writes those positions into the probe flags ([`ProbeFlags::WALLS`]) for the runtime lookup,
//!   which keeps the corners on the receiver's side of the wall, and the 3×3×3 SH filter skips
//!   every blocked neighbor.
//!
//! Algorithms (see `docs/provenance.md`): segment–triangle intersection (Möller & Trumbore,
//! "Fast, Minimum Storage Ray/Triangle Intersection", 1997), slab test against the BVH boxes
//! (Kay & Kajiya 1986), the terrain sampled along the segment at half its grid spacing.

#[cfg(test)]
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use regia_asset::baked_gi::{ProbeFlags, VolumeLayout};
use regia_core::glam::{Mat4, Vec3};

#[cfg(test)]
thread_local! {
    /// Heap traversal stacks [`LeakGeometry`] allocated for a segment walk on this thread (tests).
    pub(crate) static MESH_SPAN_HEAP_STACKS: Cell<u64> = const { Cell::new(0) };
}

use crate::bvh::{Aabb, Bvh, MAX_DEPTH};
use crate::scene::{PtHeightfield, PtMesh, PtScene};

/// Explicit walk stack. A tree of [`MAX_DEPTH`] that pushes both children holds about
/// `MAX_DEPTH + 1` deferred nodes at the deepest interior; one extra slot keeps a full tree
/// off the heap. A walk that still overflows spills to the heap and counts it, so a short
/// stack cannot drop a subtree.
const MESH_STACK: usize = MAX_DEPTH + 2;

/// A hit closer to either end than this fraction of the segment is the probe touching a surface,
/// not a wall between the probes.
const END_EPSILON: f32 = 1e-4;
/// Triangle boxes grow by this much (meters) so that flat, axis-aligned walls keep a volume.
const BOX_PAD: f32 = 1e-4;
/// A point this far (meters) below the terrain surface counts as under it.
const TERRAIN_TOLERANCE: f32 = 1e-3;
/// Most terrain samples along one segment.
const MAX_TERRAIN_SAMPLES: usize = 4096;
/// Bit of the 3×3×3 neighborhood center (never set in a mask).
const CENTER_BIT: u32 = 13;

/// One mesh's triangles in object space, BVH leaf order.
#[derive(Debug)]
struct MeshAccel {
    tris: Vec<[Vec3; 3]>,
    bvh: Bvh,
}

/// A placed copy. The segment is tested in object space. An affine map does not change `t`, so
/// the fraction along the world segment is the fraction along the object-space segment. The
/// direction is not normalized: scaling it would change `t`.
#[derive(Debug)]
struct Placement {
    accel: Arc<MeshAccel>,
    /// World → object.
    object_from_world: Mat4,
    /// World box of the padded object-space root, for a reject before the walk.
    world_min: [f32; 3],
    world_max: [f32; 3],
}

/// The geometry a wall test runs against: one object-space BVH per mesh, each instance's inverse
/// transform, and the scene's terrain heightfield.
#[derive(Debug)]
pub struct LeakGeometry<'a> {
    placements: Vec<Placement>,
    /// Unique stored triangles: each mesh once, plus triangles expanded from a singular instance.
    triangles: usize,
    terrain: Option<&'a PtHeightfield>,
}

impl<'a> LeakGeometry<'a> {
    /// One BVH per mesh. Each invertible instance stores the inverse transform and is tested in
    /// object space; a transform that cannot be inverted (determinant at most `1e-12`, or a
    /// non-finite inverse) is expanded into one shared world-space mesh so the triangles are
    /// still tested. Non-finite triangles are skipped. `triangle_count` is the stored triangles,
    /// not instances times triangles.
    #[must_use]
    pub fn from_scene(scene: &'a PtScene) -> Self {
        let _span = tracing::info_span!("pt_leak_geometry", instances = scene.instances.len()).entered();
        let mut accels: HashMap<usize, Arc<MeshAccel>> = HashMap::new();
        let mut placements = Vec::new();
        let mut triangles = 0usize;
        let mut fallback: Vec<[Vec3; 3]> = Vec::new();
        for inst in &scene.instances {
            let Some(mesh) = scene.meshes.get(inst.mesh) else {
                continue;
            };
            let Some(object_from_world) = invertible(inst.transform) else {
                push_world_tris(mesh, inst.transform, &mut fallback);
                continue;
            };
            let accel = accels
                .entry(inst.mesh)
                .or_insert_with(|| {
                    let built = Arc::new(accel_from_tris(object_tris(mesh)));
                    triangles += built.tris.len();
                    built
                })
                .clone();
            if let Some(placed) = place(accel, object_from_world, inst.transform) {
                placements.push(placed);
            }
        }
        if !fallback.is_empty() {
            let accel = Arc::new(accel_from_tris(std::mem::take(&mut fallback)));
            triangles += accel.tris.len();
            if let Some(placed) = place(accel, Mat4::IDENTITY, Mat4::IDENTITY) {
                placements.push(placed);
            }
        }
        Self {
            placements,
            triangles,
            terrain: scene.terrain.as_ref(),
        }
    }

    /// World-space triangles (one mesh, identity transform) and an optional terrain.
    #[must_use]
    pub fn from_triangles(tris: Vec<[Vec3; 3]>, terrain: Option<&'a PtHeightfield>) -> Self {
        let accel = Arc::new(accel_from_tris(tris));
        let triangles = accel.tris.len();
        let mut placements = Vec::new();
        if let Some(placed) = place(accel, Mat4::IDENTITY, Mat4::IDENTITY) {
            placements.push(placed);
        }
        Self {
            placements,
            triangles,
            terrain,
        }
    }

    /// Unique mesh triangles stored under the BVHs. Copies of one mesh count once. Triangles
    /// expanded from a singular instance count as well.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.triangles
    }

    /// `true` when the open segment `a`–`b` crosses a mesh triangle (either side) or passes
    /// under the terrain surface. Crossings within `END_EPSILON` of an end do not count, so a
    /// probe resting on a surface is not walled off from its neighbors by that surface.
    #[must_use]
    pub fn segment_blocked(&self, a: [f32; 3], b: [f32; 3]) -> bool {
        self.crossings(a, b, true).is_some()
    }

    /// Where the open segment `a`–`b` crosses geometry: the fractions of the segment (from `a`)
    /// of its first and last crossing, or `None` when it crosses nothing
    /// ([`Self::segment_blocked`]).
    #[must_use]
    pub fn segment_crossing(&self, a: [f32; 3], b: [f32; 3]) -> Option<(f32, f32)> {
        self.crossings(a, b, false)
    }

    fn crossings(&self, a: [f32; 3], b: [f32; 3], first_only: bool) -> Option<(f32, f32)> {
        let (o, e) = (Vec3::from_array(a), Vec3::from_array(b));
        let d = e - o;
        if !(o.is_finite() && e.is_finite()) || d.length_squared() == 0.0 {
            return None;
        }
        let terrain = self.terrain_span(o, d, first_only);
        if first_only && terrain.is_some() {
            return terrain;
        }
        widen(terrain, self.mesh_span(o, d, first_only))
    }

    /// The samples along the segment (every half terrain spacing, ends excluded) that lie
    /// under the terrain surface.
    fn terrain_span(&self, o: Vec3, d: Vec3, first_only: bool) -> Option<(f32, f32)> {
        let t = self.terrain?;
        let step = 0.5 * t.spacing[0].min(t.spacing[1]);
        if step.is_nan() || step <= 0.0 {
            return None;
        }
        let samples = ((d.length() / step).ceil() as usize).clamp(2, MAX_TERRAIN_SAMPLES);
        let mut span = None;
        for k in 1..samples {
            let at = k as f32 / samples as f32;
            let p = o + d * at;
            if t.height_at(p.x, p.z).is_some_and(|h| p.y < h - TERRAIN_TOLERANCE) {
                span = widen(span, Some((at, at)));
                if first_only {
                    break;
                }
            }
        }
        span
    }

    fn mesh_span(&self, o: Vec3, d: Vec3, first_only: bool) -> Option<(f32, f32)> {
        self.mesh_span_capped(o, d, first_only, MESH_STACK)
    }

    fn mesh_span_capped(&self, o: Vec3, d: Vec3, first_only: bool, cap: usize) -> Option<(f32, f32)> {
        if self.placements.is_empty() {
            return None;
        }
        let mut span = None;
        let world_inv = d.recip();
        for place in &self.placements {
            if !slab(place.world_min, place.world_max, o, world_inv) {
                continue;
            }
            let local_o = place.object_from_world.transform_point3(o);
            // A vector, not a normalized direction: normalizing would change `t`.
            let local_d = place.object_from_world.transform_vector3(d);
            if let Some(hit) = walk(&place.accel, local_o, local_d, first_only, cap) {
                span = widen(span, Some(hit));
                if first_only {
                    return span;
                }
            }
        }
        span
    }
}

fn accel_from_tris(tris: Vec<[Vec3; 3]>) -> MeshAccel {
    let bounds: Vec<Aabb> = tris
        .iter()
        .map(|t| {
            let mut b = Aabb::from_points(&[t[0].to_array(), t[1].to_array(), t[2].to_array()]);
            for k in 0..3 {
                b.min[k] -= BOX_PAD;
                b.max[k] += BOX_PAD;
            }
            b
        })
        .collect();
    let bvh = Bvh::build(&bounds);
    let ordered = bvh.order.iter().map(|&i| tris[i as usize]).collect();
    MeshAccel { tris: ordered, bvh }
}

fn object_tris(mesh: &PtMesh) -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    for t in mesh.indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (
            mesh.positions.get(t[0] as usize).copied(),
            mesh.positions.get(t[1] as usize).copied(),
            mesh.positions.get(t[2] as usize).copied(),
        ) else {
            continue;
        };
        let tri = [Vec3::from_array(a), Vec3::from_array(b), Vec3::from_array(c)];
        if tri.iter().all(|p| p.is_finite()) {
            tris.push(tri);
        }
    }
    tris
}

fn push_world_tris(mesh: &PtMesh, world_from_object: Mat4, out: &mut Vec<[Vec3; 3]>) {
    for tri in object_tris(mesh) {
        let world = [
            world_from_object.transform_point3(tri[0]),
            world_from_object.transform_point3(tri[1]),
            world_from_object.transform_point3(tri[2]),
        ];
        if world.iter().all(|p| p.is_finite()) {
            out.push(world);
        }
    }
}

/// `Some(inverse)` when the transform is affine and invertible.
fn invertible(m: Mat4) -> Option<Mat4> {
    let det = m.determinant();
    if !det.is_finite() || det.abs() <= 1e-12 {
        return None;
    }
    let inv = m.inverse();
    inv.to_cols_array().iter().all(|v| v.is_finite()).then_some(inv)
}

fn transformed_aabb(min: [f32; 3], max: [f32; 3], world_from_object: Mat4) -> Option<([f32; 3], [f32; 3])> {
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for corner in 0..8 {
        let p = Vec3::new(
            if corner & 1 == 0 { min[0] } else { max[0] },
            if corner & 2 == 0 { min[1] } else { max[1] },
            if corner & 4 == 0 { min[2] } else { max[2] },
        );
        let world = world_from_object.transform_point3(p);
        if !world.is_finite() {
            return None;
        }
        for k in 0..3 {
            lo[k] = lo[k].min(world[k]);
            hi[k] = hi[k].max(world[k]);
        }
    }
    Some((lo, hi))
}

fn place(accel: Arc<MeshAccel>, object_from_world: Mat4, world_from_object: Mat4) -> Option<Placement> {
    if accel.tris.is_empty() {
        return None;
    }
    let root = accel.bvh.nodes.first()?;
    let (world_min, world_max) = transformed_aabb(root.min, root.max, world_from_object)?;
    Some(Placement {
        accel,
        object_from_world,
        world_min,
        world_max,
    })
}

fn walk(accel: &MeshAccel, o: Vec3, d: Vec3, first_only: bool, cap: usize) -> Option<(f32, f32)> {
    if accel.tris.is_empty() || accel.bvh.nodes.is_empty() {
        return None;
    }
    let inv = d.recip();
    let mut span = None;
    let cap = cap.clamp(1, MESH_STACK);
    let mut stack = [0u32; MESH_STACK];
    let mut sp = 1usize;
    stack[0] = 0;
    // Empty until a walk exceeds `cap`. `Vec::new` does not allocate.
    let mut spill: Vec<u32> = Vec::new();
    loop {
        let at = if sp > 0 {
            sp -= 1;
            stack[sp]
        } else if let Some(next) = spill.pop() {
            next
        } else {
            break;
        };
        let Some(node) = accel.bvh.nodes.get(at as usize) else {
            continue;
        };
        if !slab(node.min, node.max, o, inv) {
            continue;
        }
        if node.is_leaf() {
            let start = node.a as usize;
            let end = (start + node.count as usize).min(accel.tris.len());
            for tri in &accel.tris[start.min(end)..end] {
                if let Some(t) = segment_hits_triangle(tri, o, d) {
                    span = widen(span, Some((t, t)));
                    if first_only {
                        return span;
                    }
                }
            }
        } else {
            push_children(&mut stack, &mut sp, cap, &mut spill, node.a, node.a + 1);
        }
    }
    span
}

fn push_children(
    stack: &mut [u32; MESH_STACK],
    sp: &mut usize,
    cap: usize,
    spill: &mut Vec<u32>,
    left: u32,
    right: u32,
) {
    if spill.is_empty() && *sp + 2 <= cap {
        stack[*sp] = left;
        stack[*sp + 1] = right;
        *sp += 2;
        return;
    }
    if spill.is_empty() {
        #[cfg(test)]
        MESH_SPAN_HEAP_STACKS.with(|n| n.set(n.get() + 1));
        spill.reserve(cap + 2);
        let mut i = *sp;
        while i > 0 {
            i -= 1;
            spill.push(stack[i]);
        }
        *sp = 0;
    }
    spill.push(left);
    spill.push(right);
}

/// The smallest span holding both.
fn widen(a: Option<(f32, f32)>, b: Option<(f32, f32)>) -> Option<(f32, f32)> {
    match (a, b) {
        (Some(x), Some(y)) => Some((x.0.min(y.0), x.1.max(y.1))),
        (x, None) => x,
        (None, y) => y,
    }
}

/// The segment `o + t·d`, `t ∈ [0, 1]`, overlaps the box (`inv` = 1 / d per axis; Rust's
/// `min`/`max` drop the NaN of a zero component on a box face).
fn slab(min: [f32; 3], max: [f32; 3], o: Vec3, inv: Vec3) -> bool {
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for k in 0..3 {
        let a = (min[k] - o[k]) * inv[k];
        let b = (max[k] - o[k]) * inv[k];
        t0 = t0.max(a.min(b));
        t1 = t1.min(a.max(b));
    }
    t0 <= t1
}

/// Möller–Trumbore, two-sided, on the open segment (ends excluded by `END_EPSILON`): the
/// fraction of the segment at the crossing.
fn segment_hits_triangle(t: &[Vec3; 3], o: Vec3, d: Vec3) -> Option<f32> {
    let e1 = t[1] - t[0];
    let e2 = t[2] - t[0];
    let p = d.cross(e2);
    let det = e1.dot(p);
    let scale = e1.length() * e2.length() * d.length();
    if det.is_nan() || det.abs() <= 1e-7 * scale {
        // Parallel to the triangle's plane, or a degenerate triangle.
        return None;
    }
    let inv = 1.0 / det;
    let s = o - t[0];
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = d.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let at = e2.dot(q) * inv;
    (at > END_EPSILON && at < 1.0 - END_EPSILON).then_some(at)
}

/// Bit of a neighbor step `(dx, dy, dz)` (each −1..=1, not all 0) in a [`NeighborWalls`] mask.
#[must_use]
pub fn step_bit(step: [i32; 3]) -> Option<u32> {
    if step.iter().any(|s| !(-1..=1).contains(s)) {
        return None;
    }
    let bit = ((step[2] + 1) * 9 + (step[1] + 1) * 3 + (step[0] + 1)) as u32;
    (bit != CENTER_BIT).then_some(bit)
}

/// For every probe of a layout, which of its 26 grid neighbors lie behind geometry, and where
/// the walls toward its three forward face neighbors are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NeighborWalls {
    /// Per probe: bit [`step_bit`] set when the segment to that neighbor crosses geometry.
    masks: Vec<u32>,
    /// Per probe: [`ProbeFlags::WALLS`] codes toward the +X, +Y and +Z neighbors.
    codes: Vec<u16>,
    /// Neighbor pairs tested (each pair once).
    tested: usize,
    /// Neighbor pairs whose segment crosses geometry.
    blocked: usize,
}

impl NeighborWalls {
    /// No walls (the wall test off): every lookup and the filter behave as before ADR-366.
    #[must_use]
    pub fn none(probes: usize) -> Self {
        Self {
            masks: vec![0; probes],
            codes: vec![0; probes],
            tested: 0,
            blocked: 0,
        }
    }

    /// Tests every pair of grid neighbors of `layout` (probe positions `positions`, nominal plus
    /// relocation) against `geometry`. Each pair is tested once; the work is split over the
    /// available cores, and the result does not depend on the split. A forward face segment
    /// that crosses geometry records where (the middle of its first and last crossing).
    #[must_use]
    pub fn scan(layout: &VolumeLayout, positions: &[[f32; 3]], geometry: &LeakGeometry<'_>) -> Self {
        let n = layout.probe_count().min(positions.len());
        let _span = tracing::info_span!("pt_neighbor_walls", probes = n).entered();
        // Forward half of the neighborhood (bit above the center): each pair once.
        let forward = |i: usize| -> (u32, u16, usize) {
            let mut mask = 0u32;
            let mut code = 0u16;
            let mut tested = 0usize;
            for (j, step) in layout.neighbors(i) {
                let Some(bit) = step_bit(step) else {
                    continue;
                };
                if bit < CENTER_BIT || j >= positions.len() {
                    continue;
                }
                tested += 1;
                let face = match step {
                    [1, 0, 0] => Some(0),
                    [0, 1, 0] => Some(1),
                    [0, 0, 1] => Some(2),
                    _ => None,
                };
                if let Some(axis) = face {
                    if let Some((first, last)) = geometry.segment_crossing(positions[i], positions[j]) {
                        mask |= 1 << bit;
                        code |= ProbeFlags::wall(axis, 0.5 * (first + last));
                    }
                } else if geometry.segment_blocked(positions[i], positions[j]) {
                    mask |= 1 << bit;
                }
            }
            (mask, code, tested)
        };
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .clamp(1, 64);
        // At least 256 probes a thread: small layouts do not pay for a thread per probe.
        let chunk = n.div_ceil(threads).max(256);
        let mut halves: Vec<(u32, u16, usize)> = vec![(0, 0, 0); n];
        std::thread::scope(|scope| {
            for (c, out) in halves.chunks_mut(chunk).enumerate() {
                let forward = &forward;
                scope.spawn(move || {
                    for (k, slot) in out.iter_mut().enumerate() {
                        *slot = forward(c * chunk + k);
                    }
                });
            }
        });
        let mut masks = vec![0u32; positions.len()];
        let mut codes = vec![0u16; positions.len()];
        let mut tested = 0usize;
        let mut blocked = 0usize;
        for (i, &(mask, code, count)) in halves.iter().enumerate() {
            tested += count;
            codes[i] = code;
            if mask == 0 {
                continue;
            }
            masks[i] |= mask;
            for (j, step) in layout.neighbors(i) {
                let (Some(bit), Some(back)) = (step_bit(step), step_bit([-step[0], -step[1], -step[2]]))
                else {
                    continue;
                };
                if mask & (1 << bit) != 0 && j < masks.len() {
                    masks[j] |= 1 << back;
                    blocked += 1;
                }
            }
        }
        Self {
            masks,
            codes,
            tested,
            blocked,
        }
    }

    /// The segment from probe `i` to its neighbor at `step` crosses geometry.
    #[must_use]
    pub fn blocked(&self, i: usize, step: [i32; 3]) -> bool {
        match (self.masks.get(i), step_bit(step)) {
            (Some(m), Some(bit)) => m & (1 << bit) != 0,
            _ => false,
        }
    }

    /// Probe `i`'s [`ProbeFlags::WALLS`] codes: where the walls toward its +X, +Y and +Z
    /// neighbors are (the probe at the low end of a segment carries its wall).
    #[must_use]
    pub fn probe_flags(&self, i: usize) -> u16 {
        self.codes.get(i).copied().unwrap_or(0)
    }

    /// Neighbor pairs tested.
    #[must_use]
    pub fn tested_pairs(&self) -> usize {
        self.tested
    }

    /// Neighbor pairs whose segment crosses geometry.
    #[must_use]
    pub fn blocked_pairs(&self) -> usize {
        self.blocked
    }

    /// Probes whose flags carry a wall code (a blocked +X, +Y or +Z neighbor).
    #[must_use]
    pub fn walled_probes(&self) -> usize {
        self.codes.iter().filter(|&&c| c != 0).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(min: [f32; 3], max: [f32; 3]) -> [[Vec3; 3]; 2] {
        // An axis-aligned rectangle: the axis where min == max is its normal.
        let axis = (0..3).find(|&k| min[k] == max[k]).unwrap_or(0);
        let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
        let corner = |a: f32, b: f32| {
            let mut p = min;
            p[u] = a;
            p[v] = b;
            Vec3::from_array(p)
        };
        let (p00, p10, p11, p01) = (
            corner(min[u], min[v]),
            corner(max[u], min[v]),
            corner(max[u], max[v]),
            corner(min[u], max[v]),
        );
        [[p00, p10, p11], [p00, p11, p01]]
    }

    #[test]
    fn a_wall_blocks_the_segments_that_cross_it_only() {
        let tris: Vec<[Vec3; 3]> = quad([0.5, -1.0, -1.0], [0.5, 1.0, 1.0]).into_iter().collect();
        let g = LeakGeometry::from_triangles(tris, None);
        assert_eq!(g.triangle_count(), 2);
        assert!(g.segment_blocked([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]));
        assert!(
            g.segment_blocked([1.0, 0.2, 0.3], [0.0, -0.2, 0.1]),
            "either side"
        );
        assert!(
            !g.segment_blocked([0.0, 0.0, 0.0], [0.4, 0.0, 0.0]),
            "stops short"
        );
        assert!(
            !g.segment_blocked([0.0, 2.0, 0.0], [1.0, 2.0, 0.0]),
            "passes above"
        );
        assert!(
            !g.segment_blocked([0.5, 0.0, 0.0], [1.5, 0.0, 0.0]),
            "starts on the wall"
        );
        assert!(!g.segment_blocked([0.0, 0.0, 0.0], [0.0, 0.0, 0.0]), "empty");
        let (first, last) = g
            .segment_crossing([0.0, 0.0, 0.0], [2.0, 0.0, 0.0])
            .expect("crosses");
        assert!(
            (first - 0.25).abs() < 1e-5 && (last - 0.25).abs() < 1e-5,
            "{first} {last}"
        );
        assert_eq!(g.segment_crossing([0.0, 2.0, 0.0], [1.0, 2.0, 0.0]), None);
        assert!(!LeakGeometry::from_triangles(Vec::new(), None).segment_blocked([0.0; 3], [1.0, 0.0, 0.0]));
    }

    #[test]
    fn a_ridge_of_the_terrain_blocks_a_segment_over_it() {
        // A 3 × 1 m grid along X with a 2 m ridge in the middle.
        let hf = PtHeightfield {
            width: 3,
            depth: 2,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights: vec![0.0, 2.0, 0.0, 0.0, 2.0, 0.0],
            material: 0,
        };
        let g = LeakGeometry::from_triangles(Vec::new(), Some(&hf));
        assert!(g.segment_blocked([0.0, 0.5, 0.5], [2.0, 0.5, 0.5]));
        assert!(!g.segment_blocked([0.0, 2.5, 0.5], [2.0, 2.5, 0.5]));
    }

    #[test]
    fn steps_map_to_distinct_bits_and_back() {
        let mut seen = 0u32;
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    match step_bit([dx, dy, dz]) {
                        Some(b) => {
                            assert_eq!(seen & (1 << b), 0);
                            seen |= 1 << b;
                            assert_eq!(step_bit([-dx, -dy, -dz]), Some(26 - b));
                        }
                        None => assert_eq!([dx, dy, dz], [0, 0, 0]),
                    }
                }
            }
        }
        assert_eq!(seen.count_ones(), 26);
        assert_eq!(step_bit([2, 0, 0]), None);
    }

    #[test]
    fn mesh_walk_does_not_allocate_a_traversal_stack() {
        let tris: Vec<[Vec3; 3]> = quad([0.5, -1.0, -1.0], [0.5, 1.0, 1.0]).into_iter().collect();
        let g = LeakGeometry::from_triangles(tris, None);
        MESH_SPAN_HEAP_STACKS.with(|n| n.set(0));
        assert!(g.segment_blocked([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]));
        assert!(
            !g.segment_blocked([0.0, 2.0, 0.0], [1.0, 2.0, 0.0]),
            "the miss walks the tree too"
        );
        assert_eq!(
            MESH_SPAN_HEAP_STACKS.with(Cell::get),
            0,
            "each segment walk allocated a heap stack"
        );
    }

    #[test]
    fn mesh_walk_matches_brute_and_a_short_stack_spills() {
        let mut tris = Vec::new();
        for i in 0..32 {
            let x = i as f32 * 3.0;
            tris.extend(quad([x, -1.0, -1.0], [x, 1.0, 1.0]));
        }
        let g = LeakGeometry::from_triangles(tris.clone(), None);
        MESH_SPAN_HEAP_STACKS.with(|n| n.set(0));
        for i in 0..32 {
            let x = i as f32 * 3.0;
            let (a, b) = ([x - 0.5, 0.0, 0.0], [x + 0.5, 0.0, 0.0]);
            let o = Vec3::from_array(a);
            let d = Vec3::from_array(b) - o;
            let brute = tris.iter().any(|t| segment_hits_triangle(t, o, d).is_some());
            assert_eq!(g.segment_blocked(a, b), brute, "wall {i}");
            assert!(
                !g.segment_blocked([x - 0.5, 3.0, 0.0], [x + 0.5, 3.0, 0.0]),
                "above {i}"
            );
        }
        assert_eq!(MESH_SPAN_HEAP_STACKS.with(Cell::get), 0, "the full stack spilled");
        MESH_SPAN_HEAP_STACKS.with(|n| n.set(0));
        let (a, b) = ([-0.5, 0.0, 0.0], [0.5, 0.0, 0.0]);
        let o = Vec3::from_array(a);
        let d = Vec3::from_array(b) - o;
        let brute = tris.iter().any(|t| segment_hits_triangle(t, o, d).is_some());
        let capped = g.mesh_span_capped(o, d, true, 1).is_some();
        assert_eq!(capped, brute, "a stack of one must not drop the hit");
        assert!(
            MESH_SPAN_HEAP_STACKS.with(Cell::get) > 0,
            "a stack of one did not spill"
        );
    }

    #[test]
    fn a_singular_instance_is_still_tested() {
        use crate::scene::{PtCamera, PtMaterial, PtScene};
        use crate::scenes;
        let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
        let material = scene.add_material(PtMaterial::diffuse([0.5; 3]));
        let wall = scenes::quad([
            [0.0, -1.0, -1.0],
            [0.0, -1.0, 1.0],
            [0.0, 1.0, 1.0],
            [0.0, 1.0, -1.0],
        ]);
        let id = scene.add_mesh(wall);
        // The X column is zero, so the determinant is zero. The quad lies in x = 0, so the
        // expanded world triangle is the same wall.
        let singular = Mat4::from_cols(
            regia_core::glam::Vec4::ZERO,
            regia_core::glam::Vec4::Y,
            regia_core::glam::Vec4::Z,
            regia_core::glam::Vec4::W,
        );
        scene.add_instance(id, material, singular);
        let g = LeakGeometry::from_scene(&scene);
        assert_eq!(g.triangle_count(), 2);
        assert!(g.segment_blocked([-0.5, 0.0, 0.0], [0.5, 0.0, 0.0]));
        assert!(!g.segment_blocked([-0.5, 3.0, 0.0], [0.5, 3.0, 0.0]));
    }

    #[test]
    fn a_nonuniform_scale_blocks_the_scaled_wall() {
        use crate::scene::{PtCamera, PtMaterial, PtScene};
        use crate::scenes;
        let mut scene = PtScene::new(PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4));
        let material = scene.add_material(PtMaterial::diffuse([0.5; 3]));
        let wall = scenes::quad([
            [0.0, -1.0, -1.0],
            [0.0, -1.0, 1.0],
            [0.0, 1.0, 1.0],
            [0.0, 1.0, -1.0],
        ]);
        let id = scene.add_mesh(wall);
        // Scale Y by 2, then move the plane to x = 20. y = 1.5 is on the scaled wall and off
        // the unscaled one.
        let xf =
            Mat4::from_translation(Vec3::new(20.0, 0.0, 0.0)) * Mat4::from_scale(Vec3::new(1.0, 2.0, 1.0));
        scene.add_instance(id, material, xf);
        let g = LeakGeometry::from_scene(&scene);
        assert_eq!(g.triangle_count(), 2);
        assert!(g.segment_blocked([19.5, 1.5, 0.0], [20.5, 1.5, 0.0]));
        assert!(!g.segment_blocked([19.5, 2.5, 0.0], [20.5, 2.5, 0.0]));
    }
}
