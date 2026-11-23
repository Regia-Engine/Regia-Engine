// SPDX-License-Identifier: MPL-2.0
//! Probe placement (baked light phase 3b, ADR-418): where a volume's probes go, decided on the CPU
//! from the scene's geometry before the GPU trace.
//!
//! - [`PlacementGeometry`]: the scene's triangles in world space under a BVH ([`crate::bvh`]) and
//!   its terrain heightfield, with the queries placement needs: closest hit along a ray (front or
//!   back face), whether a segment crosses a surface, whether a box touches a surface, whether a
//!   point is inside a solid or under the terrain.
//! - [`adaptive_bricks`]: detail bricks over a region, denser near geometry. The region is cut
//!   into blocks of `block_cells³` probe cells; a block that geometry touches splits into eight
//!   children at half the spacing, down to `levels` splits. A leaf block that touches nothing and
//!   lies inside a solid (or under the terrain) is dropped. The leaves of each level merge into
//!   as few boxes as a greedy sweep finds, and each box becomes one brick padded by one cell on
//!   every side, so wherever the region is kept some brick holds the point at least one of its
//!   own cells deep: the runtime's deepest-brick lookup (`baked_gi.wgsl`) picks the finest brick
//!   inside its own region and reads full coverage at every seam.
//! - [`place_probes`]: per probe, before the trace. A probe under the terrain is flagged as the
//!   bake flags it. A probe inside a solid (more than `inside_fraction` of its rays hit back
//!   faces, the rule of `GiBakeSettings::back_face_invalid_fraction`) moves out through the
//!   nearest back face when that exit is within `max_offset_fraction` of its cell, lands outside
//!   every solid and crosses no other surface on the way; otherwise it is culled
//!   ([`ProbeFlags::INSIDE_GEOMETRY`], never traced). A probe closer to a front face than
//!   `min_clearance_fraction` of its cell is pushed straight away from it, never across any
//!   surface (the leak-safe offset) and never past the same cell bound. `bake_gi_volume_placed`
//!   (`crate::probe`) starts its relocation rounds from this result.
//!
//! Everything here is deterministic: the same geometry and settings give the same bricks,
//! offsets and flags whatever the triangle order and thread count.
//!
//! Algorithms (see `docs/provenance.md`): octree subdivision (Meagher 1982) of a probe grid,
//! refined where surfaces are near because that is where irradiance changes fastest (non-uniform
//! probe placement: Wang et al., "Fast Non-Uniform Radiance Probe Placement and Tracing", I3D
//! 2019); triangle/box overlap by the separating axis test (Akenine-Möller, "Fast 3D
//! Triangle-Box Overlap Testing", 2001); ray/triangle intersection (Möller & Trumbore 1997); slab
//! test against the BVH boxes (Kay & Kajiya 1986); greedy box merging of voxels (the sweep used
//! for voxel meshing, Lysenko 2012); spherical Fibonacci directions (Keinert et al. 2015);
//! back-face probe classification and relocation (Majercik et al. 2021).

use std::cell::Cell;
use std::collections::HashSet;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Heap buffers a placement BVH walk creates. A shallow tree stays at zero once the stack is inline.
#[cfg(test)]
static WALK_HEAP_ALLOCS: AtomicU64 = AtomicU64::new(0);

/// Traversal stack: 64 entries inline (the lightmap tracer's `NodeStack`), spilling past that.
struct WalkStack {
    fixed: [usize; 64],
    len: usize,
    spill: Vec<usize>,
}

impl Default for WalkStack {
    fn default() -> Self {
        Self {
            fixed: [0; 64],
            len: 0,
            spill: Vec::new(),
        }
    }
}

impl WalkStack {
    fn push(&mut self, node: usize) {
        if self.len < self.fixed.len() {
            self.fixed[self.len] = node;
            self.len += 1;
            return;
        }
        #[cfg(test)]
        if self.spill.len() == self.spill.capacity() {
            WALK_HEAP_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        self.spill.push(node);
    }

    fn pop(&mut self) -> Option<usize> {
        if let Some(node) = self.spill.pop() {
            return Some(node);
        }
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.fixed[self.len])
    }
}

/// Heap buffers created while walking a placement BVH on any thread.
#[cfg(test)]
#[must_use]
pub(crate) fn placement_walk_heap_allocs() -> u64 {
    WALK_HEAP_ALLOCS.load(Ordering::Relaxed)
}

use regia_asset::baked_gi::{DetailBrick, ProbeFlags, VolumeLayout};
use regia_core::glam::Vec3;
use serde::Serialize;

use crate::PtError;
use crate::bvh::{Aabb, Bvh, BvhNode};
use crate::scene::{PtHeightfield, PtScene};

/// Triangle boxes grow by this much (metres) so flat, axis-aligned walls keep a volume.
const BOX_PAD: f32 = 1e-4;
/// Most terrain samples along one ray.
const MAX_TERRAIN_SAMPLES: usize = 4096;
/// Bisection steps that refine a terrain crossing.
const TERRAIN_BISECT: usize = 12;
/// Deepest refinement [`adaptive_bricks`] accepts.
pub const MAX_LEVELS: u32 = 6;
/// Most blocks (leaves and inner nodes) one [`adaptive_bricks`] run visits.
pub const MAX_BLOCKS: usize = 1 << 22;
/// Back hits an inside probe tries as exits, nearest first.
const EXIT_CANDIDATES: usize = 4;
/// Push rounds of an outside probe near more than one surface (a corner).
const PUSH_ROUNDS: usize = 2;

/// A placement error: an error code (`placement_invalid`, `placement_budget`) and a message.
/// The codes are documented in `book/agents/baked-gi-probe-placement.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlacementError {
    /// Stable error code.
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

impl PlacementError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "placement_invalid",
            message: message.into(),
        }
    }

    fn budget(message: impl Into<String>) -> Self {
        Self {
            code: "placement_budget",
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PlacementError {}

impl From<PlacementError> for PtError {
    fn from(e: PlacementError) -> Self {
        PtError::Scene(e.to_string())
    }
}

/// A ray's closest surface hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// Distance along the (unit) ray, metres.
    pub t: f32,
    /// The ray met the back of the surface (from inside a closed mesh, or from under the terrain).
    pub back_face: bool,
}

/// The geometry placement runs against: the scene's mesh triangles in world space and its
/// terrain heightfield.
#[derive(Debug)]
pub struct PlacementGeometry<'a> {
    /// Triangles in BVH leaf order.
    tris: Vec<[Vec3; 3]>,
    bvh: Bvh,
    terrain: Option<&'a PtHeightfield>,
}

impl<'a> PlacementGeometry<'a> {
    /// Every instance's triangles (object → world by its transform; non-finite and out-of-range
    /// triangles are skipped) and the scene's terrain.
    #[must_use]
    pub fn from_scene(scene: &'a PtScene) -> Self {
        let _span = tracing::info_span!("pt_placement_geometry", instances = scene.instances.len()).entered();
        let mut tris = Vec::new();
        for inst in &scene.instances {
            let Some(mesh) = scene.meshes.get(inst.mesh) else {
                continue;
            };
            let world: Vec<Vec3> = mesh
                .positions
                .iter()
                .map(|p| inst.transform.transform_point3(Vec3::from_array(*p)))
                .collect();
            for t in mesh.indices.as_chunks::<3>().0 {
                if let (Some(a), Some(b), Some(c)) = (
                    world.get(t[0] as usize),
                    world.get(t[1] as usize),
                    world.get(t[2] as usize),
                ) {
                    tris.push([*a, *b, *c]);
                }
            }
        }
        Self::from_triangles(tris, scene.terrain.as_ref())
    }

    /// Geometry from world-space triangles (front faces wind counter-clockwise, the tracer's
    /// convention) and an optional terrain. Non-finite triangles are dropped.
    #[must_use]
    pub fn from_triangles(tris: Vec<[Vec3; 3]>, terrain: Option<&'a PtHeightfield>) -> Self {
        let tris: Vec<[Vec3; 3]> = tris
            .into_iter()
            .filter(|t| t.iter().all(|v| v.is_finite()))
            .collect();
        let bounds: Vec<Aabb> = tris
            .iter()
            .map(|t| {
                let mut b = Aabb::from_points(&[t[0].to_array(), t[1].to_array(), t[2].to_array()]);
                for a in 0..3 {
                    b.min[a] -= BOX_PAD;
                    b.max[a] += BOX_PAD;
                }
                b
            })
            .collect();
        let bvh = Bvh::build(&bounds);
        let tris = bvh.order.iter().map(|&i| tris[i as usize]).collect();
        Self { tris, bvh, terrain }
    }

    /// Mesh triangles.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.tris.len()
    }

    /// Visits the triangles under every BVH node `enter` accepts; `visit` returns `true` to stop.
    /// Returns whether a visit stopped the walk.
    fn walk(&self, enter: impl Fn(&BvhNode) -> bool, mut visit: impl FnMut(&[Vec3; 3]) -> bool) -> bool {
        if self.tris.is_empty() {
            return false;
        }
        // 64 inline entries cover a shallow scene. The heap spill is counted only when it grows.
        let mut stack = WalkStack::default();
        stack.push(0);
        while let Some(i) = stack.pop() {
            let Some(node) = self.bvh.nodes.get(i) else {
                continue;
            };
            if !enter(node) {
                continue;
            }
            if node.is_leaf() {
                let start = node.a as usize;
                let end = (start + node.count as usize).min(self.tris.len());
                for tri in self.tris.get(start..end).unwrap_or(&[]) {
                    if visit(tri) {
                        return true;
                    }
                }
            } else {
                stack.push(node.a as usize);
                stack.push(node.a as usize + 1);
            }
        }
        false
    }

    /// Closest mesh hit with `t_min < t <= t_max` along the unit ray `o + t·d`.
    fn mesh_hit(&self, o: Vec3, d: Vec3, t_min: f32, t_max: f32) -> Option<Hit> {
        let inv = Vec3::ONE / d;
        let best = Cell::new(t_max);
        let mut hit: Option<Hit> = None;
        self.walk(
            |n| ray_box(o, inv, n.min, n.max, best.get()),
            |tri| {
                if let Some((t, back_face)) = ray_triangle(o, d, tri, t_min, best.get())
                    && hit.is_none_or(|h| t < h.t || (t == h.t && back_face && !h.back_face))
                {
                    best.set(t);
                    hit = Some(Hit { t, back_face });
                }
                false
            },
        );
        hit
    }

    /// First terrain crossing with `t_min < t <= t_max`: marched at half the grid spacing (at most
    /// [`MAX_TERRAIN_SAMPLES`] steps), refined by bisection. From above it is a front face, from
    /// below a back face; a hole (no ground) is not a surface.
    fn terrain_hit(&self, o: Vec3, d: Vec3, t_min: f32, t_max: f32) -> Option<Hit> {
        let terrain = self.terrain?;
        if !(t_max > t_min && t_max.is_finite()) {
            return None;
        }
        let span = t_max - t_min;
        let step = (0.5 * terrain.spacing[0].min(terrain.spacing[1])).max(span / MAX_TERRAIN_SAMPLES as f32);
        if !(step > 0.0 && step.is_finite()) {
            return None;
        }
        let f = |s: f32| {
            let p = o + d * s;
            terrain.height_at(p.x, p.z).map(|h| p.y - h)
        };
        let steps = (span / step).ceil() as usize;
        let mut prev = f(t_min).map(|v| (t_min, v));
        for k in 1..=steps {
            let s = (t_min + k as f32 * step).min(t_max);
            let cur = f(s);
            // A sample exactly on the surface is a crossing too: from below, `v1 == 0` reads as
            // "not above", and the next step skips a zero `v0`, so the hit was lost.
            if let (Some((s0, v0)), Some(v1)) = (prev, cur)
                && v0 != 0.0
                && ((v0 > 0.0) != (v1 > 0.0) || v1 == 0.0)
            {
                let (mut lo, mut hi) = (s0, s);
                for _ in 0..TERRAIN_BISECT {
                    let mid = 0.5 * (lo + hi);
                    match f(mid) {
                        Some(v) if (v > 0.0) == (v0 > 0.0) => lo = mid,
                        _ => hi = mid,
                    }
                }
                return Some(Hit {
                    t: hi,
                    back_face: v0 < 0.0,
                });
            }
            prev = cur.map(|v| (s, v));
        }
        None
    }

    fn hit_between(&self, o: Vec3, d: Vec3, t_min: f32, t_max: f32) -> Option<Hit> {
        let mesh = self.mesh_hit(o, d, t_min, t_max);
        let terrain = self.terrain_hit(o, d, t_min, mesh.map_or(t_max, |h| h.t));
        match (mesh, terrain) {
            (Some(m), Some(t)) if t.t < m.t => Some(t),
            (Some(m), _) => Some(m),
            (None, t) => t,
        }
    }

    /// The closest surface (mesh or terrain) along the ray from `origin` toward `dir` within
    /// `t_max` metres.
    #[must_use]
    pub fn closest_hit(&self, origin: [f32; 3], dir: [f32; 3], t_max: f32) -> Option<Hit> {
        let o = Vec3::from_array(origin);
        let d = Vec3::from_array(dir).normalize_or_zero();
        if d == Vec3::ZERO || !(o.is_finite() && t_max > 0.0) {
            return None;
        }
        self.hit_between(o, d, 0.0, t_max)
    }

    /// The surface the open segment `a → b` crosses first, if any. Hits within a hair of either
    /// end (a probe sitting on a surface) do not count.
    #[must_use]
    pub fn segment_hit(&self, a: [f32; 3], b: [f32; 3]) -> Option<Hit> {
        let (a, b) = (Vec3::from_array(a), Vec3::from_array(b));
        let len = (b - a).length();
        if !(len > 0.0 && len.is_finite()) {
            return None;
        }
        let eps = 1e-5 + 1e-4 * len;
        if len <= 2.0 * eps {
            return None;
        }
        self.hit_between(a, (b - a) / len, eps, len - eps)
    }

    /// Lowest and highest terrain sample over the cells the footprint `min..max` (x, z) touches,
    /// and whether the footprint lies wholly on the grid with no hole in it.
    fn terrain_range(&self, min: [f32; 2], max: [f32; 2]) -> Option<(f32, f32, bool)> {
        let t = self.terrain?;
        if t.width < 2 || t.depth < 2 || t.heights.len() < (t.width * t.depth) as usize {
            return None;
        }
        let (w, h) = ((t.width - 1) as f32, (t.depth - 1) as f32);
        let gx = |x: f32| (x - t.origin[0]) / t.spacing[0];
        let gz = |z: f32| (z - t.origin[1]) / t.spacing[1];
        let (x0, x1, z0, z1) = (gx(min[0]), gx(max[0]), gz(min[1]), gz(max[1]));
        if !(x0.is_finite() && x1.is_finite() && z0.is_finite() && z1.is_finite())
            || x1 < 0.0
            || z1 < 0.0
            || x0 > w
            || z0 > h
        {
            return None;
        }
        let on_grid = x0 >= 0.0 && z0 >= 0.0 && x1 <= w && z1 <= h;
        let i0 = x0.floor().clamp(0.0, w) as u32;
        let i1 = x1.ceil().clamp(0.0, w) as u32;
        let j0 = z0.floor().clamp(0.0, h) as u32;
        let j1 = z1.ceil().clamp(0.0, h) as u32;
        // Stride so one query reads at most ~1M samples; a stride above 1 drops `complete`.
        let samples = u64::from(i1 - i0 + 1) * u64::from(j1 - j0 + 1);
        let stride = ((samples as f64 / 1.0e6).sqrt().ceil() as u32).max(1);
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        let mut complete = on_grid && stride == 1;
        for j in (j0..=j1).step_by(stride as usize) {
            for i in (i0..=i1).step_by(stride as usize) {
                let v = t.height(i, j);
                if v.is_finite() {
                    lo = lo.min(v);
                    hi = hi.max(v);
                } else {
                    complete = false;
                }
            }
        }
        (lo <= hi).then_some((lo, hi, complete))
    }

    /// `true` when `p` lies below the terrain surface (the bake's `UNDER_TERRAIN` test).
    #[must_use]
    pub fn under_terrain(&self, p: [f32; 3]) -> bool {
        self.terrain
            .and_then(|t| t.height_at(p[0], p[2]))
            .is_some_and(|h| p[1] < h)
    }

    /// `true` when a mesh triangle (or, with `terrain`, possibly the terrain surface: the test is
    /// conservative there) touches the box `min..=max`.
    #[must_use]
    pub fn box_touches(&self, min: [f32; 3], max: [f32; 3], terrain: bool) -> bool {
        let lo = Vec3::from_array(min);
        let hi = Vec3::from_array(max);
        let centre = 0.5 * (lo + hi);
        let half = 0.5 * (hi - lo);
        let mesh = self.walk(
            |n| (0..3).all(|a| n.min[a] <= max[a] && min[a] <= n.max[a]),
            |tri| tri_box_overlap(centre, half, tri),
        );
        mesh || (terrain
            && self
                .terrain_range([min[0], min[2]], [max[0], max[2]])
                .is_some_and(|(tlo, thi, _)| tlo <= max[1] && thi >= min[1]))
    }

    /// `true` when the box `min..=max` lies wholly under the terrain: its footprint is on the grid
    /// with no hole and its top is below the lowest ground over it.
    #[must_use]
    pub fn box_under_terrain(&self, min: [f32; 3], max: [f32; 3]) -> bool {
        self.terrain_range([min[0], min[2]], [max[0], max[2]])
            .is_some_and(|(lo, _, complete)| complete && max[1] < lo)
    }

    /// What a probe at `p` sees along `directions` (unit vectors) within `t_max`.
    #[must_use]
    pub fn view(&self, p: [f32; 3], directions: &[[f32; 3]], t_max: f32) -> ProbeView {
        let mut view = ProbeView::default();
        self.view_into(p, directions, t_max, &mut view);
        view
    }

    /// Fills `view` from `p`, reusing `view.back_hits`'s buffer.
    fn view_into(&self, p: [f32; 3], directions: &[[f32; 3]], t_max: f32, view: &mut ProbeView) {
        view.rays = directions.len() as u32;
        view.back_hits.clear();
        view.closest_front = None;
        for d in directions {
            let Some(hit) = self.closest_hit(p, *d, t_max) else {
                continue;
            };
            if hit.back_face {
                view.back_hits.push((*d, hit.t));
            } else if view.closest_front.is_none_or(|(_, t)| hit.t < t) {
                view.closest_front = Some((*d, hit.t));
            }
        }
        // Stable: equal distances keep the direction order.
        view.back_hits.sort_by(|a, b| a.1.total_cmp(&b.1));
    }

    /// `true` when more than `inside_fraction` of the rays from `p` along `directions` hit a back
    /// face first (the bake's inside-geometry rule).
    #[must_use]
    pub fn is_inside(&self, p: [f32; 3], directions: &[[f32; 3]], t_max: f32, inside_fraction: f32) -> bool {
        self.view(p, directions, t_max).inside(inside_fraction)
    }
}

/// What a probe sees from its position ([`PlacementGeometry::view`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeView {
    /// Rays cast.
    pub rays: u32,
    /// Direction and distance of every ray whose first hit was a back face, nearest first.
    pub back_hits: Vec<([f32; 3], f32)>,
    /// Direction and distance of the closest front-face hit.
    pub closest_front: Option<([f32; 3], f32)>,
}

impl ProbeView {
    /// Fraction of the rays that hit a back face first.
    #[must_use]
    pub fn back_face_fraction(&self) -> f32 {
        if self.rays == 0 {
            0.0
        } else {
            self.back_hits.len() as f32 / self.rays as f32
        }
    }

    /// More than `inside_fraction` of the rays hit a back face first.
    #[must_use]
    pub fn inside(&self, inside_fraction: f32) -> bool {
        self.back_face_fraction() > inside_fraction
    }
}

/// `n` spherical Fibonacci directions (Keinert et al. 2015), fixed for a given `n` (clamped to
/// 4..=1024). None lies on a coordinate axis, so axis-aligned walls do not graze them.
#[must_use]
pub fn fibonacci_directions(n: u32) -> Vec<[f32; 3]> {
    let n = n.clamp(4, 1024);
    let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
    (0..n)
        .map(|i| {
            let y = 1.0 - (2 * i + 1) as f32 / n as f32;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let phi = i as f32 * golden + 0.5;
            [r * phi.cos(), y, r * phi.sin()]
        })
        .collect()
}

/// Möller–Trumbore, both faces: `(t, back_face)` for `t_min < t <= t_max`.
fn ray_triangle(o: Vec3, d: Vec3, tri: &[Vec3; 3], t_min: f32, t_max: f32) -> Option<(f32, bool)> {
    let e1 = tri[1] - tri[0];
    let e2 = tri[2] - tri[0];
    let p = d.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = o - tri[0];
    let u = s.dot(p) * inv;
    if !(-1e-6..=1.0 + 1e-6).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = d.dot(q) * inv;
    if v < -1e-6 || u + v > 1.0 + 1e-6 {
        return None;
    }
    let t = e2.dot(q) * inv;
    if !(t > t_min && t <= t_max) {
        return None;
    }
    // det = −d·(e1 × e2): positive when the ray meets the front (counter-clockwise) face.
    Some((t, det < 0.0))
}

/// Slab test of the unit ray against a box over `0..=t_max` (Kay & Kajiya 1986).
fn ray_box(o: Vec3, inv: Vec3, min: [f32; 3], max: [f32; 3], t_max: f32) -> bool {
    let mut t0 = 0.0f32;
    let mut t1 = t_max;
    for a in 0..3 {
        let lo = (min[a] - o[a]) * inv[a];
        let hi = (max[a] - o[a]) * inv[a];
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        // `max`/`min` ignore a NaN (a ray in the slab's plane): the test stays conservative.
        t0 = t0.max(lo);
        t1 = t1.min(hi);
        if t0 > t1 {
            return false;
        }
    }
    true
}

/// Separating axis test of a triangle against the box `centre ± half` (Akenine-Möller 2001):
/// the box normals, the triangle normal and the nine edge × axis products.
fn tri_box_overlap(centre: Vec3, half: Vec3, tri: &[Vec3; 3]) -> bool {
    let v = [tri[0] - centre, tri[1] - centre, tri[2] - centre];
    for a in 0..3 {
        let lo = v[0][a].min(v[1][a]).min(v[2][a]);
        let hi = v[0][a].max(v[1][a]).max(v[2][a]);
        if lo > half[a] || hi < -half[a] {
            return false;
        }
    }
    let radius = |axis: Vec3| half.x * axis.x.abs() + half.y * axis.y.abs() + half.z * axis.z.abs();
    let edges = [v[1] - v[0], v[2] - v[1], v[0] - v[2]];
    let n = edges[0].cross(edges[1]);
    if n.dot(v[0]).abs() > radius(n) {
        return false;
    }
    for e in edges {
        for axis in [Vec3::X.cross(e), Vec3::Y.cross(e), Vec3::Z.cross(e)] {
            if axis.length_squared() < 1e-20 {
                continue;
            }
            let p = [axis.dot(v[0]), axis.dot(v[1]), axis.dot(v[2])];
            let r = radius(axis);
            if p[0].min(p[1]).min(p[2]) > r || p[0].max(p[1]).max(p[2]) < -r {
                return false;
            }
        }
    }
    true
}

/// Settings of [`adaptive_bricks`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct AdaptiveSettings {
    /// Region to cover, minimum corner (metres). It is rounded up to whole level-0 blocks.
    pub min: [f32; 3],
    /// Region, maximum corner.
    pub max: [f32; 3],
    /// Probe spacing of level 0 (the coarsest), metres.
    pub spacing: f32,
    /// Refinement levels: the finest spacing is `spacing / 2^levels` (0..=[`MAX_LEVELS`]).
    pub levels: u32,
    /// Probe cells per block edge (1..=64).
    pub block_cells: u32,
    /// A block refines when geometry lies within this distance of it, metres.
    pub margin: f32,
    /// Most probes in all the bricks; over it the finest level is dropped until they fit.
    pub max_probes: usize,
    /// Most bricks (the runtime visits every brick per shaded point); over it the finest level is
    /// dropped until they fit.
    pub max_bricks: usize,
    /// Drop leaf blocks that touch no geometry and lie inside a solid or under the terrain.
    pub cull_solid: bool,
    /// Refine near the terrain surface too (the terrain layer usually covers it).
    pub refine_terrain: bool,
    /// Back-face fraction above which a point is inside a solid (the bake uses 0.25).
    pub inside_fraction: f32,
    /// Rays of the inside test.
    pub directions: u32,
    /// Ray length of the inside test, metres.
    pub max_distance: f32,
}

impl AdaptiveSettings {
    /// Defaults over the region `min..max` at level-0 `spacing`: two levels, 4-cell blocks, a
    /// 0.5 m margin, 1 Mi probes, 128 bricks, solid culling on, terrain refinement off.
    #[must_use]
    pub fn new(min: [f32; 3], max: [f32; 3], spacing: f32) -> Self {
        Self {
            min,
            max,
            spacing,
            levels: 2,
            block_cells: 4,
            margin: 0.5,
            max_probes: 1 << 20,
            max_bricks: 128,
            cull_solid: true,
            refine_terrain: false,
            inside_fraction: 0.25,
            directions: 32,
            max_distance: 32.0,
        }
    }

    fn validate(&self) -> Result<(), PlacementError> {
        let finite = |v: &[f32; 3]| v.iter().all(|x| x.is_finite());
        if !finite(&self.min) || !finite(&self.max) || (0..3).any(|a| self.min[a] >= self.max[a]) {
            return Err(PlacementError::invalid(
                "the region must be a finite box with min < max",
            ));
        }
        if !(self.spacing > 0.0 && self.spacing.is_finite()) {
            return Err(PlacementError::invalid("spacing must be positive"));
        }
        if self.levels > MAX_LEVELS {
            return Err(PlacementError::invalid(format!(
                "levels {} (0..={MAX_LEVELS})",
                self.levels
            )));
        }
        if !(1..=64).contains(&self.block_cells) {
            return Err(PlacementError::invalid(format!(
                "block_cells {} (1..=64)",
                self.block_cells
            )));
        }
        if !(self.margin >= 0.0 && self.margin.is_finite()) {
            return Err(PlacementError::invalid("margin must be zero or positive"));
        }
        if self.max_probes == 0 || self.max_bricks == 0 {
            return Err(PlacementError::invalid(
                "max_probes and max_bricks must be positive",
            ));
        }
        if !(self.inside_fraction > 0.0 && self.inside_fraction <= 1.0) {
            return Err(PlacementError::invalid("inside_fraction must be in (0, 1]"));
        }
        if !(self.max_distance > 0.0 && self.max_distance.is_finite()) {
            return Err(PlacementError::invalid("max_distance must be positive"));
        }
        Ok(())
    }
}

/// What [`adaptive_bricks`] did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AdaptiveReport {
    /// Levels asked for.
    pub requested_levels: u32,
    /// Levels used (fewer when `max_probes` or `max_bricks` dropped the finest).
    pub levels: u32,
    /// Probe spacing per level used, coarsest first.
    pub spacings: Vec<f32>,
    /// Kept leaf blocks per level.
    pub leaf_blocks: Vec<usize>,
    /// Bricks per level.
    pub bricks_per_level: Vec<usize>,
    /// Leaf blocks dropped as inside a solid.
    pub culled_solid: usize,
    /// Leaf blocks dropped as under the terrain.
    pub culled_under_terrain: usize,
    /// Probes in all the bricks.
    pub probes: usize,
}

/// Bricks placed by [`adaptive_bricks`].
#[derive(Clone, Debug, PartialEq)]
pub struct AdaptiveBricks {
    /// Bricks, coarsest level first; within a level in merge order.
    pub bricks: Vec<DetailBrick>,
    /// Refinement level of each brick.
    pub brick_levels: Vec<u32>,
    /// Summary.
    pub report: AdaptiveReport,
}

/// A box of same-level blocks: first block and size, in blocks of that level.
type BlockBox = ([u32; 3], [u32; 3]);

/// Places detail bricks over `settings.min..max`, denser near `geometry` (see the module docs).
///
/// # Errors
///
/// `placement_invalid` for bad settings or a region that needs more than [`MAX_BLOCKS`] blocks;
/// `placement_budget` when even level 0 alone exceeds `max_probes` or `max_bricks`.
pub fn adaptive_bricks(
    geometry: &PlacementGeometry<'_>,
    settings: &AdaptiveSettings,
) -> Result<AdaptiveBricks, PlacementError> {
    let _span = tracing::info_span!("pt_adaptive_bricks", levels = settings.levels).entered();
    settings.validate()?;
    let mut levels = settings.levels;
    loop {
        let placed = adaptive_at(geometry, settings, levels)?;
        let (probes, bricks) = (placed.report.probes, placed.bricks.len());
        if probes <= settings.max_probes && bricks <= settings.max_bricks {
            return Ok(placed);
        }
        if levels == 0 {
            return Err(PlacementError::budget(format!(
                "level 0 alone needs {probes} probes in {bricks} bricks; max_probes is {} and \
                 max_bricks {}: raise them, the spacing or block_cells, or shrink the region",
                settings.max_probes, settings.max_bricks
            )));
        }
        tracing::debug!(target: "regia_pt", levels, probes, bricks, "adaptive bricks over budget: dropping a level");
        levels -= 1;
    }
}

fn adaptive_at(
    geometry: &PlacementGeometry<'_>,
    s: &AdaptiveSettings,
    levels: u32,
) -> Result<AdaptiveBricks, PlacementError> {
    let origin = Vec3::from_array(s.min);
    let edge0 = s.spacing * s.block_cells as f32;
    let counts: [u32; 3] = std::array::from_fn(|a| (((s.max[a] - s.min[a]) / edge0).ceil() as u32).max(1));
    let top_wide = counts.iter().map(|&c| u128::from(c)).product::<u128>();
    if top_wide > MAX_BLOCKS as u128 {
        return Err(PlacementError::invalid(format!(
            "the region holds {top_wide} level-0 blocks (at most {MAX_BLOCKS}): raise the spacing or block_cells"
        )));
    }
    let top = top_wide as usize;
    let edge = |level: u32| edge0 / (1u32 << level) as f32;
    let cell = |level: u32| edge(level) / s.block_cells as f32;
    let directions = fibonacci_directions(s.directions);
    let mut report = AdaptiveReport {
        requested_levels: s.levels,
        levels,
        spacings: (0..=levels).map(cell).collect(),
        leaf_blocks: vec![0; levels as usize + 1],
        bricks_per_level: vec![0; levels as usize + 1],
        ..AdaptiveReport::default()
    };
    let mut leaves: Vec<Vec<[u32; 3]>> = vec![Vec::new(); levels as usize + 1];
    let mut stack: Vec<(u32, [u32; 3])> = Vec::with_capacity(top as usize);
    for z in 0..counts[2] {
        for y in 0..counts[1] {
            for x in 0..counts[0] {
                stack.push((0, [x, y, z]));
            }
        }
    }
    stack.reverse();
    let mut visited = 0usize;
    while let Some((level, c)) = stack.pop() {
        visited += 1;
        if visited > MAX_BLOCKS {
            return Err(PlacementError::invalid(format!(
                "refinement visits more than {MAX_BLOCKS} blocks: lower levels or raise the spacing"
            )));
        }
        let e = edge(level);
        let lo = origin + Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) * e;
        let hi = lo + Vec3::splat(e);
        let m = Vec3::splat(s.margin);
        if level < levels && geometry.box_touches((lo - m).to_array(), (hi + m).to_array(), s.refine_terrain)
        {
            // Children in reverse so they pop in (z, y, x) order.
            for k in (0..8u32).rev() {
                let child = [
                    c[0] * 2 + (k & 1),
                    c[1] * 2 + ((k >> 1) & 1),
                    c[2] * 2 + ((k >> 2) & 1),
                ];
                stack.push((level + 1, child));
            }
            continue;
        }
        if s.cull_solid {
            // The brick reaches one cell past the block: only a block whose padded box touches
            // nothing is wholly on one side of every surface.
            let pad = Vec3::splat(cell(level));
            let (plo, phi) = ((lo - pad).to_array(), (hi + pad).to_array());
            if !geometry.box_touches(plo, phi, true) {
                if geometry.box_under_terrain(plo, phi) {
                    report.culled_under_terrain += 1;
                    continue;
                }
                let centre = (0.5 * (lo + hi)).to_array();
                if geometry.is_inside(centre, &directions, s.max_distance, s.inside_fraction) {
                    report.culled_solid += 1;
                    continue;
                }
            }
        }
        leaves[level as usize].push(c);
    }
    let mut bricks = Vec::new();
    let mut brick_levels = Vec::new();
    for (level, blocks) in leaves.iter().enumerate() {
        report.leaf_blocks[level] = blocks.len();
        let level = level as u32;
        let sp = cell(level);
        let e = edge(level);
        for (first, size) in merge_blocks(blocks) {
            let counts: [u32; 3] = std::array::from_fn(|a| size[a] * s.block_cells + 3);
            let brick = DetailBrick {
                origin: std::array::from_fn(|a| s.min[a] + first[a] as f32 * e - sp),
                spacing: [sp; 3],
                counts,
                first_probe: 0,
                flags: 0,
            };
            report.probes += brick.probe_count();
            report.bricks_per_level[level as usize] += 1;
            bricks.push(brick);
            brick_levels.push(level);
        }
    }
    Ok(AdaptiveBricks {
        bricks,
        brick_levels,
        report,
    })
}

/// Greedy merge of same-level blocks into boxes: in (z, y, x) order, each unclaimed block grows
/// along x, then the row along y, then the slab along z, as far as every block it takes is
/// present and unclaimed.
fn merge_blocks(blocks: &[[u32; 3]]) -> Vec<BlockBox> {
    let present: HashSet<[u32; 3]> = blocks.iter().copied().collect();
    let mut order: Vec<[u32; 3]> = present.iter().copied().collect();
    order.sort_by_key(|c| (c[2], c[1], c[0]));
    let mut claimed: HashSet<[u32; 3]> = HashSet::with_capacity(order.len());
    let free = |c: [u32; 3], claimed: &HashSet<[u32; 3]>| present.contains(&c) && !claimed.contains(&c);
    let mut out = Vec::new();
    for c in order {
        if claimed.contains(&c) {
            continue;
        }
        let mut w = 1u32;
        while free([c[0] + w, c[1], c[2]], &claimed) {
            w += 1;
        }
        let mut h = 1u32;
        while (0..w).all(|dx| free([c[0] + dx, c[1] + h, c[2]], &claimed)) {
            h += 1;
        }
        let mut d = 1u32;
        while (0..h).all(|dy| (0..w).all(|dx| free([c[0] + dx, c[1] + dy, c[2] + d], &claimed))) {
            d += 1;
        }
        for dz in 0..d {
            for dy in 0..h {
                for dx in 0..w {
                    claimed.insert([c[0] + dx, c[1] + dy, c[2] + dz]);
                }
            }
        }
        out.push((c, [w, h, d]));
    }
    out
}

/// Settings of [`place_probes`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct PlacementSettings {
    /// Back-face fraction above which a probe is inside a solid (the bake's
    /// `back_face_invalid_fraction`, 0.25).
    pub inside_fraction: f32,
    /// A probe closer to a front face than this fraction of its smallest cell edge is pushed away
    /// (the bake's `min_front_face_fraction`, 0.1).
    pub min_clearance_fraction: f32,
    /// Most offset per axis, as a fraction of the probe's cell (0.45: the bake's relocation bound
    /// of 0.9 half cells).
    pub max_offset_fraction: f32,
    /// Rays per probe.
    pub directions: u32,
    /// Ray length, metres (0 = the layout's `max_distance`).
    pub max_distance: f32,
    /// Worker threads (0 = every core). The result does not depend on it.
    pub threads: usize,
}

impl Default for PlacementSettings {
    fn default() -> Self {
        Self {
            inside_fraction: 0.25,
            min_clearance_fraction: 0.1,
            max_offset_fraction: 0.45,
            directions: 32,
            max_distance: 0.0,
            threads: 0,
        }
    }
}

/// What [`place_probes`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PlacementReport {
    /// Probes in the layout.
    pub probes: usize,
    /// Probes left at their nominal position.
    pub kept: usize,
    /// Outside probes pushed away from a close surface.
    pub pushed: usize,
    /// Inside probes moved out of their solid.
    pub escaped: usize,
    /// Inside probes with no exit within their cell: culled (never traced).
    pub culled: usize,
    /// Probes under the terrain (never traced, as in every bake).
    pub under_terrain: usize,
}

/// Per-probe offsets and flags from [`place_probes`], in the layout's probe order.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbePlacement {
    /// Offset from the nominal position, metres.
    pub offsets: Vec<[f32; 3]>,
    /// [`ProbeFlags`] bits: `VALID` (with `RELOCATED` when moved), `INSIDE_GEOMETRY` (culled) or
    /// `UNDER_TERRAIN`.
    pub flags: Vec<u16>,
    /// Summary.
    pub report: PlacementReport,
}

impl ProbePlacement {
    /// Final positions (nominal + offset) for `layout`, the layout the placement was made for.
    #[must_use]
    pub fn positions(&self, layout: &VolumeLayout) -> Vec<[f32; 3]> {
        layout
            .nominal_positions()
            .iter()
            .zip(&self.offsets)
            .map(|(n, o)| [n[0] + o[0], n[1] + o[1], n[2] + o[2]])
            .collect()
    }

    /// Copies the offsets and flags into a bake's working arrays.
    pub(crate) fn seed(&self, offsets: &mut [[f32; 3]], flags: &mut [u16]) -> Result<(), PtError> {
        if self.offsets.len() != offsets.len() || self.flags.len() != flags.len() {
            return Err(PtError::Scene(format!(
                "the probe placement holds {} probes but the layout has {}: place the probes for this layout",
                self.offsets.len(),
                offsets.len()
            )));
        }
        offsets.copy_from_slice(&self.offsets);
        flags.copy_from_slice(&self.flags);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Placed {
    Kept,
    Pushed(Vec3),
    Escaped(Vec3),
    Culled,
    UnderTerrain,
}

/// Classifies and offsets every probe of `layout` against `geometry` (see the module docs).
#[must_use]
pub fn place_probes(
    layout: &VolumeLayout,
    geometry: &PlacementGeometry<'_>,
    settings: &PlacementSettings,
) -> ProbePlacement {
    let nominal = layout.nominal_positions();
    let half = layout.half_cells();
    let _span = tracing::info_span!("pt_place_probes", probes = nominal.len()).entered();
    let directions = fibonacci_directions(settings.directions);
    let t_max = if settings.max_distance > 0.0 {
        settings.max_distance
    } else {
        layout.max_distance.max(1e-3)
    };
    let threads = if settings.threads == 0 {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    } else {
        settings.threads
    };
    let n = nominal.len();
    let chunk = n.div_ceil(threads.max(1)).max(1);
    let placed: Vec<Placed> = std::thread::scope(|scope| {
        let workers: Vec<_> = nominal
            .chunks(chunk)
            .zip(half.chunks(chunk))
            .map(|(ps, hs)| {
                let directions = &directions;
                let worker = scope.spawn(move || {
                    ps.iter()
                        .zip(hs)
                        .map(|(p, h)| {
                            place_one(
                                geometry,
                                Vec3::from_array(*p),
                                Vec3::from_array(*h),
                                directions,
                                t_max,
                                settings,
                            )
                        })
                        .collect::<Vec<_>>()
                });
                (ps.len(), worker)
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|(len, w)| {
                // A panicking worker is a bug here: its probes stay at their nominal positions
                // (the GPU relocation still sees them) rather than losing the bake.
                w.join().unwrap_or_else(|_| vec![Placed::Kept; len])
            })
            .collect()
    });
    let mut out = ProbePlacement {
        offsets: vec![[0.0; 3]; n],
        flags: vec![ProbeFlags::VALID; n],
        report: PlacementReport {
            probes: n,
            ..PlacementReport::default()
        },
    };
    for (i, p) in placed.iter().enumerate() {
        let r = &mut out.report;
        match *p {
            Placed::Kept => r.kept += 1,
            Placed::Pushed(o) => {
                r.pushed += 1;
                out.offsets[i] = o.to_array();
                out.flags[i] = ProbeFlags::VALID | ProbeFlags::RELOCATED;
            }
            Placed::Escaped(o) => {
                r.escaped += 1;
                out.offsets[i] = o.to_array();
                out.flags[i] = ProbeFlags::VALID | ProbeFlags::RELOCATED;
            }
            Placed::Culled => {
                r.culled += 1;
                out.flags[i] = ProbeFlags::INSIDE_GEOMETRY;
            }
            Placed::UnderTerrain => {
                r.under_terrain += 1;
                out.flags[i] = ProbeFlags::UNDER_TERRAIN;
            }
        }
    }
    tracing::info!(
        target: "regia_pt",
        probes = n,
        pushed = out.report.pushed,
        escaped = out.report.escaped,
        culled = out.report.culled,
        "probes placed"
    );
    out
}

fn place_one(
    geometry: &PlacementGeometry<'_>,
    p: Vec3,
    half: Vec3,
    directions: &[[f32; 3]],
    t_max: f32,
    s: &PlacementSettings,
) -> Placed {
    if geometry.under_terrain(p.to_array()) {
        return Placed::UnderTerrain;
    }
    let cell = 2.0 * half.min_element();
    let clearance = (s.min_clearance_fraction * cell).max(1e-3);
    let limit = half * 2.0 * s.max_offset_fraction.max(0.0);
    let mut view = ProbeView::default();
    let mut scratch = ProbeView::default();
    geometry.view_into(p.to_array(), directions, t_max, &mut view);
    if view.inside(s.inside_fraction) {
        // Out through the nearest back faces first. An exit is never clamped: a shortened step
        // would stay inside. The candidate view goes in `scratch` so this loop can keep reading
        // `view.back_hits`.
        let exits = view.back_hits.len().min(EXIT_CANDIDATES);
        for i in 0..exits {
            let (dir_a, t) = view.back_hits[i];
            let dir = Vec3::from_array(dir_a);
            let step = dir * (t + clearance);
            if step.abs().cmpgt(limit).any() {
                continue;
            }
            let q = p + step;
            if geometry.under_terrain(q.to_array()) {
                continue;
            }
            // Leak-safe: past the face it leaves through, the step crosses nothing.
            let past = p + dir * (t + 0.5 * clearance);
            if geometry.segment_hit(past.to_array(), q.to_array()).is_some() {
                continue;
            }
            geometry.view_into(q.to_array(), directions, t_max, &mut scratch);
            if scratch.inside(s.inside_fraction) {
                continue;
            }
            return Placed::Escaped(step);
        }
        return Placed::Culled;
    }
    // Outside: away from a too-close front face, never across a surface, never into a solid,
    // only when the clearance improves; a second round handles a corner.
    let mut offset = Vec3::ZERO;
    for _ in 0..PUSH_ROUNDS {
        let Some((dir, t)) = view.closest_front else {
            break;
        };
        if t >= clearance {
            break;
        }
        let target = (offset - Vec3::from_array(dir) * (clearance - t)).clamp(-limit, limit);
        if target == offset {
            break;
        }
        let (from, q) = (p + offset, p + target);
        if geometry.under_terrain(q.to_array())
            || geometry.segment_hit(from.to_array(), q.to_array()).is_some()
        {
            break;
        }
        geometry.view_into(q.to_array(), directions, t_max, &mut scratch);
        if scratch.inside(s.inside_fraction) || scratch.closest_front.map_or(f32::INFINITY, |(_, d)| d) <= t {
            break;
        }
        offset = target;
        std::mem::swap(&mut view, &mut scratch);
    }
    if offset == Vec3::ZERO {
        Placed::Kept
    } else {
        Placed::Pushed(offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed axis-aligned box, faces wound outward.
    fn cube(lo: [f32; 3], hi: [f32; 3]) -> Vec<[Vec3; 3]> {
        let c = |x: usize, y: usize, z: usize| {
            Vec3::new(
                if x == 0 { lo[0] } else { hi[0] },
                if y == 0 { lo[1] } else { hi[1] },
                if z == 0 { lo[2] } else { hi[2] },
            )
        };
        // Each face as a quad seen from outside, counter-clockwise.
        let faces = [
            [c(0, 0, 0), c(0, 0, 1), c(0, 1, 1), c(0, 1, 0)], // -X
            [c(1, 0, 0), c(1, 1, 0), c(1, 1, 1), c(1, 0, 1)], // +X
            [c(0, 0, 0), c(1, 0, 0), c(1, 0, 1), c(0, 0, 1)], // -Y
            [c(0, 1, 0), c(0, 1, 1), c(1, 1, 1), c(1, 1, 0)], // +Y
            [c(0, 0, 0), c(0, 1, 0), c(1, 1, 0), c(1, 0, 0)], // -Z
            [c(0, 0, 1), c(1, 0, 1), c(1, 1, 1), c(0, 1, 1)], // +Z
        ];
        faces
            .iter()
            .flat_map(|q| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]])
            .collect()
    }

    #[test]
    fn cube_faces_point_outward() {
        let tris = cube([0.0; 3], [1.0; 3]);
        let centre = Vec3::splat(0.5);
        for t in &tris {
            let n = (t[1] - t[0]).cross(t[2] - t[0]);
            let mid = (t[0] + t[1] + t[2]) / 3.0;
            assert!(n.dot(mid - centre) > 0.0, "{t:?}");
        }
    }

    #[test]
    fn rays_see_front_and_back_faces() {
        let g = PlacementGeometry::from_triangles(cube([0.0; 3], [1.0; 3]), None);
        let from_outside = g
            .closest_hit([-1.0, 0.5, 0.5], [1.0, 0.0, 0.0], 10.0)
            .expect("hit");
        assert!((from_outside.t - 1.0).abs() < 1e-5 && !from_outside.back_face);
        let from_inside = g
            .closest_hit([0.25, 0.5, 0.5], [1.0, 0.0, 0.0], 10.0)
            .expect("hit");
        assert!((from_inside.t - 0.75).abs() < 1e-5 && from_inside.back_face);
        assert!(g.closest_hit([-1.0, 0.5, 0.5], [-1.0, 0.0, 0.0], 10.0).is_none());
        assert!(g.segment_hit([-1.0, 0.5, 0.5], [2.0, 0.5, 0.5]).is_some());
        assert!(g.segment_hit([-1.0, 0.5, 0.5], [-0.1, 0.5, 0.5]).is_none());
        // A segment that ends on the face does not cross it.
        assert!(g.segment_hit([-1.0, 0.5, 0.5], [0.0, 0.5, 0.5]).is_none());
    }

    #[test]
    fn inside_test_and_box_overlap() {
        let g = PlacementGeometry::from_triangles(cube([0.0; 3], [1.0; 3]), None);
        let dirs = fibonacci_directions(32);
        assert!(g.is_inside([0.5; 3], &dirs, 10.0, 0.25));
        assert!(!g.is_inside([2.0, 0.5, 0.5], &dirs, 10.0, 0.25));
        // A box inside the cube, away from its faces, touches no triangle.
        assert!(!g.box_touches([0.2; 3], [0.8; 3], false));
        assert!(g.box_touches([0.9, 0.2, 0.2], [1.1, 0.8, 0.8], false));
        assert!(!g.box_touches([1.2, 0.0, 0.0], [2.0, 1.0, 1.0], false));
    }

    #[test]
    fn triangle_box_overlap_separates_on_edge_axes() {
        // A diagonal triangle near the corner of a box: the box axes and the triangle normal
        // overlap, only an edge cross axis separates them.
        let tri = [
            Vec3::new(1.2, 0.0, 1.2),
            Vec3::new(1.2, 2.0, 1.2),
            Vec3::new(2.5, 1.0, 0.1),
        ];
        assert!(!tri_box_overlap(Vec3::splat(0.5), Vec3::splat(0.5), &tri));
        assert!(tri_box_overlap(Vec3::splat(1.0), Vec3::splat(0.5), &tri));
    }

    #[test]
    fn terrain_hits_from_above_are_front_faces() {
        let hf = PtHeightfield {
            width: 5,
            depth: 5,
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            heights: vec![1.0; 25],
            material: 0,
        };
        let g = PlacementGeometry::from_triangles(Vec::new(), Some(&hf));
        let down = g
            .closest_hit([2.0, 3.0, 2.0], [0.0, -1.0, 0.0], 10.0)
            .expect("ground");
        assert!((down.t - 2.0).abs() < 1e-3 && !down.back_face);
        let up = g
            .closest_hit([2.0, 0.0, 2.0], [0.0, 1.0, 0.0], 10.0)
            .expect("underside");
        assert!((up.t - 1.0).abs() < 1e-3 && up.back_face);
        assert!(g.under_terrain([2.0, 0.5, 2.0]));
        assert!(g.box_under_terrain([1.0, -2.0, 1.0], [3.0, 0.5, 3.0]));
        assert!(!g.box_under_terrain([1.0, -2.0, 1.0], [3.0, 1.5, 3.0]));
        // Off the grid nothing is under the terrain.
        assert!(!g.box_under_terrain([3.0, -2.0, 3.0], [6.0, 0.5, 6.0]));
    }

    #[test]
    fn greedy_merge_makes_one_box_from_a_slab() {
        let blocks: Vec<[u32; 3]> = (0..2)
            .flat_map(|z| (0..3).flat_map(move |y| (0..4).map(move |x| [x, y, z])))
            .collect();
        assert_eq!(merge_blocks(&blocks), vec![([0, 0, 0], [4, 3, 2])]);
        // An L shape needs two boxes and claims every block once.
        let l = [[0, 0, 0], [1, 0, 0], [0, 1, 0]];
        let boxes = merge_blocks(&l);
        assert_eq!(boxes.len(), 2);
        let total: u32 = boxes.iter().map(|(_, s)| s[0] * s[1] * s[2]).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn fibonacci_directions_are_unit_and_off_axis() {
        let dirs = fibonacci_directions(32);
        assert_eq!(dirs.len(), 32);
        for d in &dirs {
            let v = Vec3::from_array(*d);
            assert!((v.length() - 1.0).abs() < 1e-5);
            assert!(d.iter().filter(|c| c.abs() > 1e-4).count() >= 2, "{d:?}");
        }
        let sum: Vec3 = dirs.iter().map(|d| Vec3::from_array(*d)).sum();
        assert!(sum.length() < 0.1, "{sum:?}");
    }

    #[test]
    fn a_region_whose_block_count_overflows_usize_is_invalid() {
        // 2^22 blocks per axis: the product is 2^66, which wrapped to 0 in a usize.
        let g = PlacementGeometry::from_triangles(Vec::new(), None);
        let s = AdaptiveSettings::new([0.0; 3], [16_777_216.0; 3], 1.0);
        let err = adaptive_bricks(&g, &s).expect_err("must be refused");
        assert_eq!(err.code, "placement_invalid");
    }

    /// Placing probes in a shallow scene does not allocate a BVH stack per ray.
    #[test]
    fn placing_probes_does_not_allocate_a_stack_per_ray() {
        use crate::probe::detail_brick;

        let g = PlacementGeometry::from_triangles(cube([0.0; 3], [2.0; 3]), None);
        let mut layout = VolumeLayout {
            terrain: None,
            bricks: vec![detail_brick([0.2; 3], [1.8; 3], 0.8)],
            moment_resolution: 4,
            max_distance: 4.0,
        };
        layout.assign_brick_offsets();
        let settings = PlacementSettings {
            directions: 16,
            threads: 1,
            max_distance: 4.0,
            ..PlacementSettings::default()
        };
        let before = placement_walk_heap_allocs();
        let placed = place_probes(&layout, &g, &settings);
        let allocs = placement_walk_heap_allocs() - before;
        assert_eq!(allocs, 0, "heap stacks {allocs}");
        // The inline stack is LIFO with the same push order as the old `Vec` stack, so these
        // bytes are the placement that stack produced (one centre probe stays inside).
        let kept = ProbeFlags::VALID | ProbeFlags::RELOCATED;
        let inside = ProbeFlags::INSIDE_GEOMETRY;
        let expected_flags = [
            kept, kept, kept, kept, kept, kept, kept, kept, kept, kept, kept, kept, kept, inside, kept, kept,
            kept, kept, kept, kept, kept, kept, kept, kept, kept, kept, kept,
        ];
        let a = [0.0058722133, 0.017527241, -0.279826];
        let b = [-0.27811888, -0.05340299, -0.030300481];
        let c = [0.03701979, -0.275, -0.09512612];
        let d = [0.2758252, 0.090941176, -0.01838461];
        let e = [0.0, 0.0, 0.0];
        let f = [0.08957986, 0.27500007, 0.0489377];
        let g_off = [-0.053735524, -0.13420734, 0.27055934];
        let expected_offsets = [
            a, a, a, a, a, a, a, a, a, b, c, d, b, e, d, b, f, d, b, c, d, b, g_off, d, b, f, d,
        ];
        assert_eq!(placed.flags, expected_flags);
        assert_eq!(placed.offsets, expected_offsets);
        let again = place_probes(&layout, &g, &settings);
        assert_eq!(placed.offsets, again.offsets);
        assert_eq!(placed.flags, again.flags);
    }
}
