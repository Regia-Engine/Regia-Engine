// SPDX-License-Identifier: MPL-2.0
// Source: C. Ericson, "Real-Time Collision Detection", Morgan Kaufmann 2004 (§ 5.1.5 closest
// point on a triangle by Voronoi regions; § 5.1.3 closest point on an oriented box); signed
// distance of a box and of a convex polytope as the largest plane distance (standard
// half-space representation). Original code (spec 55 § 2.4).

//! World snapshots (spec 55 § 2.4): a read-only, key-sorted copy of the rigid shapes near a
//! cloth, taken once per fixed step and handed to [`ClothSolver::set_snapshot`]. The solver
//! reads only the snapshot (never the rigid sets), so solvers can step in parallel.
//!
//! Shapes move from `prev` to `cur` across the substeps like moving colliders; contacts take the
//! shape's velocity at the contact point. A convex shape whose surface moves more than the
//! thickness within one substep is tested along the particle's path relative to it (swept), so a
//! fast shape carries the cloth instead of passing through it. Boxes and hulls push particles out to the thickness
//! along the nearest face ("push out to thickness, no launch speed"); triangle lists push along
//! the face normal on the front side, and a particle that was behind a triangle at the start of
//! the substep is not rescued. Triangle lists are binned in a uniform grid over their local
//! vertices, so a particle tests the cells around its end point instead of every triangle.
//!
//! [`ClothSolver::set_snapshot`]: super::ClothSolver::set_snapshot

use std::cell::RefCell;

use regia_core::math::{Isometry, Quat, Vec3};
use regia_ecs::spawn_order::SpawnKey;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Triangles the narrow phase actually tested. `cargo test --lib` only.
#[cfg(test)]
pub(crate) static TRIANGLE_VISITS: AtomicUsize = AtomicUsize::new(0);

use super::{ClothError, EPSILON, finite, finite_vec};

/// Most triangles one snapshot may hold (`snapshot_triangles_max`).
pub const SNAPSHOT_TRIANGLES_MAX: usize = 4_096;

/// Most planes a hull may have. The world-snapshot builder gives a larger hull as its
/// bounding box. [`WorldSnapshot::new`] still returns [`ClothError::SnapshotTooLarge`] for a
/// hand-built hull over this cap.
pub const SNAPSHOT_HULL_PLANES_MAX: usize = 64;

/// Most vertices a snapshot hull keeps. A hull with more vertices is given as its bounding box.
pub const SNAPSHOT_HULL_VERTICES_MAX: usize = 64;

/// Outward face planes of the convex hull of `points`, for a cloth world snapshot.
///
/// Each plane is a unit outward half-space `n · x <= d` in the points' frame. Returns `None`
/// when the points do not enclose a volume (fewer than four, flat, or non-finite), when the
/// hull has more than [`SNAPSHOT_HULL_VERTICES_MAX`] vertices, or when the distinct face
/// planes exceed [`SNAPSHOT_HULL_PLANES_MAX`]. The builder then uses the points' bounding
/// box. Coplanar triangles of one face share a plane.
#[must_use]
pub fn snapshot_hull_planes(points: &[Vec3]) -> Option<Vec<(Vec3, f32)>> {
    let hull = crate::convex_decomposition::convex_hull_of_points(points, 0)?;
    if hull.vertices.is_empty() || hull.vertices.len() > SNAPSHOT_HULL_VERTICES_MAX {
        return None;
    }
    let mut planes: Vec<(Vec3, f32)> = Vec::new();
    for tri in &hull.triangles {
        let [ia, ib, ic] = *tri;
        let (Some(a), Some(b), Some(c)) = (
            hull.vertices.get(ia as usize).copied(),
            hull.vertices.get(ib as usize).copied(),
            hull.vertices.get(ic as usize).copied(),
        ) else {
            return None;
        };
        let n = (b - a).cross(c - a);
        let len = n.length();
        if len <= EPSILON {
            continue;
        }
        let n = n / len;
        let d = n.dot(a);
        let duplicate = planes
            .iter()
            .any(|(pn, pd)| pn.dot(n) > 0.999 && (pd - d).abs() <= 1.0e-4 * (1.0 + pd.abs()));
        if !duplicate {
            planes.push((n, d));
        }
    }
    if planes.is_empty() || planes.len() > SNAPSHOT_HULL_PLANES_MAX {
        return None;
    }
    Some(planes)
}

/// Shape of a [`SnapshotShape`], in the shape's local frame.
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotKind {
    /// A ball at the local origin.
    Sphere {
        /// Radius in meters.
        radius: f32,
    },
    /// A capsule along the local Y axis.
    Capsule {
        /// Half the length of the core segment.
        half_height: f32,
        /// Radius in meters.
        radius: f32,
    },
    /// A box centered on the local origin.
    Box {
        /// Half extents in meters (each positive).
        half_extents: Vec3,
    },
    /// A convex hull as outward half-spaces `n · x <= d` (unit `n`; at most
    /// [`SNAPSHOT_HULL_PLANES_MAX`]).
    Hull {
        /// `(n, d)` per face.
        planes: Vec<(Vec3, f32)>,
    },
    /// The half-space `normal · x >= offset` (local frame).
    Plane {
        /// Outward unit normal.
        normal: Vec3,
        /// Offset along the normal.
        offset: f32,
    },
    /// Static triangles (heightfield or mesh pieces) in the local frame, counter-clockwise seen
    /// from the front.
    Triangles {
        /// The triangles.
        triangles: Vec<[Vec3; 3]>,
    },
}

/// One rigid shape in a [`WorldSnapshot`].
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotShape {
    /// Stable key (the collider's spawn-arrival order, `regia_ecs::spawn_order::SpawnKey`,
    /// ADR-1347); the solver sorts and groups by it.
    pub key: SpawnKey,
    /// The shape.
    pub kind: SnapshotKind,
    /// Pose at the previous fixed step.
    pub prev: Isometry,
    /// Pose at the current fixed step.
    pub cur: Isometry,
    /// Coulomb friction coefficient (zero or more).
    pub friction: f32,
}

/// Rigid shapes near a cloth for one fixed step, sorted by key.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorldSnapshot {
    shapes: Vec<SnapshotShape>,
    /// One slot per shape. `Some` for a non-empty triangle list, built from local vertices.
    tri_grids: Vec<Option<TriGrid>>,
}

/// Cells per axis of a snapshot triangle grid.
const TRI_GRID_MAX_AXIS: u32 = 16;

/// An axis thinner than this is one cell, so a particle just off a flat mesh still shares it.
const TRI_GRID_FLAT: f32 = 1.0e-4;

/// Uniform grid over one triangle list's local vertices (Ericson, uniform grid). A pure
/// function of those vertices, so a `WorldSnapshot` compares equal when its shapes do.
#[derive(Debug, Clone, PartialEq)]
struct TriGrid {
    origin: Vec3,
    cell: Vec3,
    dims: [u32; 3],
    /// CSR starts, one past the last cell.
    start: Vec<u32>,
    indices: Vec<u32>,
    tri_count: u32,
}

/// Stamp buffer for a multi-cell triangle query. One per thread: solvers step in parallel
/// on separate threads, and a query never holds this across the contact callback.
struct TriQuery {
    generation: u32,
    seen: Vec<u32>,
    hits: Vec<u32>,
}

thread_local! {
    static TRI_QUERY: RefCell<TriQuery> = const {
        RefCell::new(TriQuery {
            generation: 1,
            seen: Vec::new(),
            hits: Vec::new(),
        })
    };
}

fn tri_aabb(tri: &[Vec3; 3]) -> (Vec3, Vec3) {
    let lo = tri[0].min(tri[1]).min(tri[2]);
    let hi = tri[0].max(tri[1]).max(tri[2]);
    (lo, hi)
}

fn grid_cell(p: Vec3, origin: Vec3, cell: Vec3, dims: [u32; 3]) -> [u32; 3] {
    let mut out = [0u32; 3];
    for k in 0..3 {
        let last = dims[k] - 1;
        if last == 0 || cell[k] <= 0.0 || cell[k].is_nan() {
            continue;
        }
        let t = ((p[k] - origin[k]) / cell[k]).floor();
        if !t.is_finite() {
            continue;
        }
        // Float-to-int `as` saturates (Rust 1.45+).
        let c = t as i64;
        out[k] = c.clamp(0, last as i64) as u32;
    }
    out
}

fn cell_index(x: u32, y: u32, z: u32, dims: [u32; 3]) -> usize {
    ((z * dims[1] + y) * dims[0] + x) as usize
}

impl TriGrid {
    fn build(triangles: &[[Vec3; 3]]) -> Option<Self> {
        if triangles.is_empty() {
            return None;
        }
        let mut lo = Vec3::splat(f32::INFINITY);
        let mut hi = Vec3::splat(f32::NEG_INFINITY);
        for tri in triangles {
            for v in tri {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
        }
        let extent = hi - lo;
        let mut dims = [1u32; 3];
        let mut cell = Vec3::splat(1.0);
        for k in 0..3 {
            if extent[k] > TRI_GRID_FLAT {
                dims[k] = TRI_GRID_MAX_AXIS;
                cell[k] = extent[k] / (TRI_GRID_MAX_AXIS as f32);
            }
        }
        let ncells = (dims[0] * dims[1] * dims[2]) as usize;
        let mut counts = vec![0u32; ncells];
        let mut ranges = Vec::with_capacity(triangles.len());
        for tri in triangles {
            let (a, b) = tri_aabb(tri);
            let c0 = grid_cell(a, lo, cell, dims);
            let c1 = grid_cell(b, lo, cell, dims);
            ranges.push((c0, c1));
            for z in c0[2]..=c1[2] {
                for y in c0[1]..=c1[1] {
                    for x in c0[0]..=c1[0] {
                        counts[cell_index(x, y, z, dims)] += 1;
                    }
                }
            }
        }
        let mut start = vec![0u32; ncells + 1];
        for (i, count) in counts.iter().enumerate() {
            start[i + 1] = start[i] + count;
        }
        let total = start[ncells] as usize;
        let mut indices = vec![0u32; total];
        let mut cursor = start.clone();
        for (ti, (c0, c1)) in ranges.iter().enumerate() {
            for z in c0[2]..=c1[2] {
                for y in c0[1]..=c1[1] {
                    for x in c0[0]..=c1[0] {
                        let at = cell_index(x, y, z, dims);
                        indices[cursor[at] as usize] = ti as u32;
                        cursor[at] += 1;
                    }
                }
            }
        }
        Some(Self {
            origin: lo,
            cell,
            dims,
            start,
            indices,
            tri_count: triangles.len() as u32,
        })
    }

    /// Triangles whose vertex box can meet `point` expanded by `margin`. Each index once.
    fn for_each_near(&self, point: Vec3, margin: f32, mut visit: impl FnMut(u32)) {
        let pad = Vec3::splat(margin.abs());
        let c0 = grid_cell(point - pad, self.origin, self.cell, self.dims);
        let c1 = grid_cell(point + pad, self.origin, self.cell, self.dims);
        if c0 == c1 {
            let idx = cell_index(c0[0], c0[1], c0[2], self.dims);
            let (s, e) = (self.start[idx] as usize, self.start[idx + 1] as usize);
            for &ti in &self.indices[s..e] {
                visit(ti);
            }
            return;
        }
        let mut hits = TRI_QUERY.with(|slot| {
            let mut q = slot.borrow_mut();
            q.generation = q.generation.wrapping_add(1);
            if q.generation == 0 {
                q.seen.fill(0);
                q.generation = 1;
            }
            let n = self.tri_count as usize;
            if q.seen.len() < n {
                q.seen.resize(n, 0);
            }
            let generation = q.generation;
            q.hits.clear();
            for z in c0[2]..=c1[2] {
                for y in c0[1]..=c1[1] {
                    for x in c0[0]..=c1[0] {
                        let idx = cell_index(x, y, z, self.dims);
                        let (s, e) = (self.start[idx] as usize, self.start[idx + 1] as usize);
                        for &ti in &self.indices[s..e] {
                            let seen = &mut q.seen[ti as usize];
                            if *seen != generation {
                                *seen = generation;
                                q.hits.push(ti);
                            }
                        }
                    }
                }
            }
            std::mem::take(&mut q.hits)
        });
        // Ascending index matches the full-list scan, so the last contact written
        // (normal, friction, velocity) stays the same triangle.
        hits.sort_unstable();
        for ti in &hits {
            visit(*ti);
        }
        TRI_QUERY.with(|slot| {
            let mut q = slot.borrow_mut();
            if q.hits.capacity() < hits.capacity() {
                q.hits = hits;
            }
        });
    }
}

fn check_pose(pose: &Isometry) -> Result<Isometry, ClothError> {
    finite_vec(pose.translation, "snapshot pose")?;
    let q = pose.rotation;
    if !q.is_finite() || q.length_squared() <= EPSILON {
        return Err(ClothError::NonFinite {
            what: "snapshot pose",
        });
    }
    Ok(Isometry::new(q.normalize(), pose.translation))
}

fn positive(v: f32, what: &'static str) -> Result<f32, ClothError> {
    if finite(v, what)? < 0.0 {
        Err(ClothError::Negative { what })
    } else {
        Ok(v)
    }
}

impl WorldSnapshot {
    /// Validates `shapes` and sorts them by key (stable: equal keys keep their order).
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] / [`ClothError::Negative`] for bad numbers,
    /// [`ClothError::Degenerate`] for a zero plane or hull normal or a non-positive box extent,
    /// [`ClothError::SnapshotTooLarge`] for more than [`SNAPSHOT_TRIANGLES_MAX`] triangles in
    /// all or a hull with more than [`SNAPSHOT_HULL_PLANES_MAX`] planes.
    pub fn new(mut shapes: Vec<SnapshotShape>) -> Result<Self, ClothError> {
        let mut triangles = 0usize;
        for shape in &mut shapes {
            shape.prev = check_pose(&shape.prev)?;
            shape.cur = check_pose(&shape.cur)?;
            positive(shape.friction, "friction")?;
            match &mut shape.kind {
                SnapshotKind::Sphere { radius } => {
                    positive(*radius, "radius")?;
                }
                SnapshotKind::Capsule { half_height, radius } => {
                    positive(*half_height, "half height")?;
                    positive(*radius, "radius")?;
                }
                SnapshotKind::Box { half_extents } => {
                    let he = finite_vec(*half_extents, "half extents")?;
                    if he.min_element() <= 0.0 {
                        return Err(ClothError::Degenerate { what: "box extent" });
                    }
                }
                SnapshotKind::Hull { planes } => {
                    if planes.len() > SNAPSHOT_HULL_PLANES_MAX {
                        return Err(ClothError::SnapshotTooLarge {
                            what: "hull planes",
                            len: planes.len(),
                        });
                    }
                    for (n, d) in planes.iter_mut() {
                        let len = finite_vec(*n, "hull normal")?.length();
                        finite(*d, "hull offset")?;
                        if len <= EPSILON {
                            return Err(ClothError::Degenerate { what: "hull normal" });
                        }
                        *n /= len;
                        *d /= len;
                    }
                }
                SnapshotKind::Plane { normal, offset } => {
                    let len = finite_vec(*normal, "plane normal")?.length();
                    finite(*offset, "plane offset")?;
                    if len <= EPSILON {
                        return Err(ClothError::Degenerate { what: "plane normal" });
                    }
                    *normal /= len;
                    *offset /= len;
                }
                SnapshotKind::Triangles { triangles: list } => {
                    triangles += list.len();
                    for t in list.iter() {
                        for v in t {
                            finite_vec(*v, "triangle")?;
                        }
                    }
                }
            }
        }
        if triangles > SNAPSHOT_TRIANGLES_MAX {
            return Err(ClothError::SnapshotTooLarge {
                what: "triangles",
                len: triangles,
            });
        }
        shapes.sort_by_key(|s| s.key);
        let tri_grids = shapes
            .iter()
            .map(|shape| match &shape.kind {
                SnapshotKind::Triangles { triangles } => TriGrid::build(triangles),
                _ => None,
            })
            .collect();
        Ok(Self { shapes, tri_grids })
    }

    /// Keeps the [`SNAPSHOT_TRIANGLES_MAX`] triangles nearest to `near` (by centroid distance,
    /// ties by original order) across every triangle shape, dropping the rest. For builders that
    /// gather triangles from a heightfield or mesh before calling [`WorldSnapshot::new`].
    pub fn cap_triangles(shapes: &mut [SnapshotShape], near: Vec3) {
        let mut all: Vec<(f32, usize, usize)> = Vec::new();
        for (si, shape) in shapes.iter().enumerate() {
            if let SnapshotKind::Triangles { triangles } = &shape.kind {
                for (ti, t) in triangles.iter().enumerate() {
                    let world = shape.cur.transform_point((t[0] + t[1] + t[2]) / 3.0);
                    all.push((world.distance_squared(near), si, ti));
                }
            }
        }
        if all.len() <= SNAPSHOT_TRIANGLES_MAX {
            return;
        }
        all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let mut keep: Vec<Vec<bool>> = shapes
            .iter()
            .map(|s| match &s.kind {
                SnapshotKind::Triangles { triangles } => vec![false; triangles.len()],
                _ => Vec::new(),
            })
            .collect();
        for &(_, si, ti) in &all[..SNAPSHOT_TRIANGLES_MAX] {
            keep[si][ti] = true;
        }
        for (shape, keep) in shapes.iter_mut().zip(keep) {
            if let SnapshotKind::Triangles { triangles } = &mut shape.kind {
                let mut flags = keep.into_iter();
                triangles.retain(|_| flags.next().unwrap_or(false));
            }
        }
    }

    /// The shapes, sorted by key.
    #[must_use]
    pub fn shapes(&self) -> &[SnapshotShape] {
        &self.shapes
    }

    /// Total triangles in the snapshot.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.shapes
            .iter()
            .map(|s| match &s.kind {
                SnapshotKind::Triangles { triangles } => triangles.len(),
                _ => 0,
            })
            .sum()
    }

    /// After a fixed step: every shape rests at its current pose.
    pub(crate) fn advance(&mut self) {
        for shape in &mut self.shapes {
            shape.prev = shape.cur;
        }
    }

    /// Moves every pose by `-shift` after an origin rebase (spec 78 § 12.2).
    pub(crate) fn shift_origin(&mut self, shift: Vec3) {
        for shape in &mut self.shapes {
            shape.prev.translation -= shift;
            shape.cur.translation -= shift;
        }
    }

    /// Whether the snapshot holds no shape.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    /// Contacts of shape `index` with the point that moved from `x_prev` to `x`.
    /// An index past the snapshot does nothing.
    pub(crate) fn collide_shape(
        &self,
        index: usize,
        x: Vec3,
        x_prev: Vec3,
        frame: SubstepFrame,
        hit: impl FnMut(SnapshotContact),
    ) {
        let Some(shape) = self.shapes.get(index) else {
            return;
        };
        let grid = self.tri_grids.get(index).and_then(|g| g.as_ref());
        collide(shape, grid, x, x_prev, frame, hit);
    }
}

/// Pose at fraction `t` from `a` to `b` (normalized linear interpolation of the rotation).
pub(crate) fn pose_at(a: &Isometry, b: &Isometry, t: f32) -> Isometry {
    if t >= 1.0 {
        return *b;
    }
    let mut rb: Quat = b.rotation;
    if a.rotation.dot(rb) < 0.0 {
        rb = -rb;
    }
    Isometry::new(
        a.rotation.lerp(rb, t).normalize(),
        a.translation.lerp_monotonic(b.translation, t),
    )
}

/// Signed distance and outward normal of a local-frame point to a convex local shape
/// (triangles are handled separately).
fn local_distance(kind: &SnapshotKind, p: Vec3) -> Option<(f32, Vec3)> {
    let sphere = |c: Vec3, r: f32| {
        let d = p - c;
        let len = d.length();
        if len > EPSILON {
            (len - r, d / len)
        } else {
            (-r, Vec3::Y)
        }
    };
    Some(match kind {
        SnapshotKind::Sphere { radius } => sphere(Vec3::ZERO, *radius),
        SnapshotKind::Capsule { half_height, radius } => {
            let y = p.y.clamp(-*half_height, *half_height);
            sphere(Vec3::new(0.0, y, 0.0), *radius)
        }
        SnapshotKind::Box { half_extents } => {
            let q = p.abs() - *half_extents;
            let outside = q.max(Vec3::ZERO);
            if outside.length_squared() > 0.0 {
                let n = (outside * p.signum()).normalize_or_zero();
                (outside.length(), n)
            } else {
                // Inside: leave through the nearest face (ties: x, then y, then z).
                let axis = if q.x >= q.y && q.x >= q.z {
                    0
                } else if q.y >= q.z {
                    1
                } else {
                    2
                };
                let mut n = Vec3::ZERO;
                n[axis] = if p[axis] >= 0.0 { 1.0 } else { -1.0 };
                (q[axis], n)
            }
        }
        SnapshotKind::Hull { planes } => {
            let mut best: Option<(f32, Vec3)> = None;
            for &(n, d) in planes {
                let s = n.dot(p) - d;
                if best.is_none_or(|(b, _)| s > b) {
                    best = Some((s, n));
                }
            }
            best?
        }
        SnapshotKind::Plane { normal, offset } => (normal.dot(p) - offset, *normal),
        SnapshotKind::Triangles { .. } => return None,
    })
}

/// Barycentric inside test of the projection of `p` onto triangle `t` (with unit normal `n`).
fn projects_inside(t: &[Vec3; 3], n: Vec3, p: Vec3) -> bool {
    for k in 0..3 {
        let (a, b) = (t[k], t[(k + 1) % 3]);
        if (b - a).cross(p - a).dot(n) < 0.0 {
            return false;
        }
    }
    true
}

/// One snapshot contact on a particle: the push, the normal and the surface velocity.
pub(crate) struct SnapshotContact {
    pub push: Vec3,
    pub normal: Vec3,
    pub velocity: Vec3,
    pub friction: f32,
}

/// The substep a contact query runs in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SubstepFrame {
    /// Fraction of the fixed step at the substep start.
    pub t0: f32,
    /// Fraction of the fixed step at the substep end.
    pub t1: f32,
    /// Substep length in seconds.
    pub h: f32,
    /// Collision margin in meters.
    pub thickness: f32,
}

/// Contacts of the point `x` (which was at `x_prev` at the substep start) with `shape` during
/// `frame`, keeping it `frame.thickness` outside. Calls `hit` for each. `grid` is the triangle
/// bin for this shape; `None` tests every triangle.
fn collide(
    shape: &SnapshotShape,
    grid: Option<&TriGrid>,
    x: Vec3,
    x_prev: Vec3,
    frame: SubstepFrame,
    mut hit: impl FnMut(SnapshotContact),
) {
    let SubstepFrame { t0, t1, h, thickness } = frame;
    let pose1 = pose_at(&shape.prev, &shape.cur, t1);
    let inv1 = pose1.inverse();
    let local = inv1.transform_point(x);
    let velocity_at = |local_point: Vec3| {
        let pose0 = pose_at(&shape.prev, &shape.cur, t0);
        if h > 0.0 {
            (pose1.transform_point(local_point) - pose0.transform_point(local_point)) / h
        } else {
            Vec3::ZERO
        }
    };
    if let SnapshotKind::Triangles { triangles } = &shape.kind {
        // The start-of-substep position in this substep's frame (static shapes: the same frame).
        let local_prev = inv1.transform_point(x_prev);
        let mut test_one = |ti: usize| {
            let Some(t) = triangles.get(ti) else {
                return;
            };
            #[cfg(test)]
            TRIANGLE_VISITS.fetch_add(1, Ordering::Relaxed);
            let cross = (t[1] - t[0]).cross(t[2] - t[0]);
            let len = cross.length();
            if len <= EPSILON {
                return;
            }
            let n = cross / len;
            let s = (local - t[0]).dot(n);
            let s_prev = (local_prev - t[0]).dot(n);
            if s >= thickness || s_prev < 0.0 || !projects_inside(t, n, local) {
                return;
            }
            let push = pose1.rotation * (n * (thickness - s));
            hit(SnapshotContact {
                push,
                normal: pose1.rotation * n,
                velocity: velocity_at(local),
                friction: shape.friction,
            });
        };
        if let Some(grid) = grid {
            // A contact's projection of `local` sits in the triangle's vertex box, `|s|` away
            // from `local`. A contact has `s < thickness` and `s_prev >= 0`, so `|s|` is at most
            // `thickness`, or the substep's travel `|local - local_prev|` for a particle that
            // crossed the plane (the fall-through rescue): the query covers both (corr-physics
            // audit: a `thickness`-only query missed a fast particle's triangle in the next cell).
            let reach = thickness.max(local.distance(local_prev));
            grid.for_each_near(local, reach + 1.0e-5, |ti| test_one(ti as usize));
        } else {
            for ti in 0..triangles.len() {
                test_one(ti);
            }
        }
        return;
    }
    let Some((d, n)) = local_distance(&shape.kind, local) else {
        return;
    };
    if d < thickness {
        let normal = pose1.rotation * n;
        hit(SnapshotContact {
            push: normal * (thickness - d),
            normal,
            velocity: velocity_at(local),
            friction: shape.friction,
        });
    } else if let Some((out, n)) = swept_local(shape, x_prev, frame, local) {
        let normal = pose1.rotation * n;
        hit(SnapshotContact {
            push: pose1.transform_point(out) - x,
            normal,
            velocity: velocity_at(out),
            friction: shape.friction,
        });
    }
}

/// Swept test of a fast convex snapshot shape (spec 55 § 2.4, ADR-2425): when the shape's
/// surface at the particle moved more than the thickness during the substep, the particle's
/// path relative to the shape (from `x_prev` in the substep-start frame to `local`, its end point
/// in the substep-end frame) is sampled. At the first sample that enters the shape the particle
/// is pushed out to the thickness there; the shape-local result carries it with the shape.
/// Returns that local point and the local normal.
fn swept_local(
    shape: &SnapshotShape,
    x_prev: Vec3,
    frame: SubstepFrame,
    local: Vec3,
) -> Option<(Vec3, Vec3)> {
    let SubstepFrame {
        t0, t1, thickness, ..
    } = frame;
    if thickness.is_nan() || thickness <= 0.0 {
        return None;
    }
    let pose0 = pose_at(&shape.prev, &shape.cur, t0);
    let pose1 = pose_at(&shape.prev, &shape.cur, t1);
    let travel = (pose1.transform_point(local) - pose0.transform_point(local)).length();
    if travel <= thickness {
        return None;
    }
    let start = pose0.inverse().transform_point(x_prev);
    let samples = ((travel / thickness).ceil() as u32).clamp(2, super::SWEEP_SAMPLES_MAX);
    let mut was_outside = local_distance(&shape.kind, start).is_some_and(|(d, _)| d >= thickness);
    for j in 1..=samples {
        let at = start.lerp(local, j as f32 / samples as f32);
        let (d, n) = local_distance(&shape.kind, at)?;
        let inside = d < thickness;
        if inside && was_outside {
            return Some((at + n * (thickness - d), n));
        }
        was_outside = !inside;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn box_distance_inside_and_outside() {
        let kind = SnapshotKind::Box {
            half_extents: Vec3::new(1.0, 0.5, 2.0),
        };
        let (d, n) = local_distance(&kind, Vec3::new(0.0, 0.8, 0.0)).expect("box");
        assert!((d - 0.3).abs() < 1e-6 && n == Vec3::Y);
        let (d, n) = local_distance(&kind, Vec3::new(0.9, 0.0, 0.0)).expect("box");
        assert!((d + 0.1).abs() < 1e-6 && n == Vec3::X, "{d} {n}");
        let (d, _) = local_distance(&kind, Vec3::new(2.0, 1.5, 0.0)).expect("box");
        assert!((d - 2.0f32.sqrt()).abs() < 1e-6);
    }

    #[test]
    fn hull_uses_the_largest_plane_distance() {
        // The unit cube as six planes.
        let planes = vec![
            (Vec3::X, 0.5),
            (Vec3::NEG_X, 0.5),
            (Vec3::Y, 0.5),
            (Vec3::NEG_Y, 0.5),
            (Vec3::Z, 0.5),
            (Vec3::NEG_Z, 0.5),
        ];
        let kind = SnapshotKind::Hull { planes };
        let (d, n) = local_distance(&kind, Vec3::new(0.0, 0.45, 0.1)).expect("hull");
        assert!((d + 0.05).abs() < 1e-6 && n == Vec3::Y, "{d} {n}");
    }

    #[test]
    fn snapshot_sorts_by_key_and_refuses_too_many_triangles() {
        let shape = |key| SnapshotShape {
            key,
            kind: SnapshotKind::Sphere { radius: 1.0 },
            prev: Isometry::IDENTITY,
            cur: Isometry::IDENTITY,
            friction: 0.3,
        };
        let snap =
            WorldSnapshot::new(vec![shape((7, 0, 0)), shape((2, 0, 0)), shape((5, 0, 0))]).expect("snapshot");
        let keys: Vec<SpawnKey> = snap.shapes().iter().map(|s| s.key).collect();
        assert_eq!(keys, [(2, 0, 0), (5, 0, 0), (7, 0, 0)]);
        let tri = [Vec3::ZERO, Vec3::X, Vec3::Z];
        let mut big = vec![SnapshotShape {
            key: (1, 0, 0),
            kind: SnapshotKind::Triangles {
                triangles: vec![tri; SNAPSHOT_TRIANGLES_MAX + 10],
            },
            prev: Isometry::IDENTITY,
            cur: Isometry::IDENTITY,
            friction: 0.3,
        }];
        let err = WorldSnapshot::new(big.clone()).expect_err("too many");
        assert_eq!(err.code(), "cloth_snapshot_too_large");
        WorldSnapshot::cap_triangles(&mut big, Vec3::ZERO);
        let snap = WorldSnapshot::new(big).expect("capped");
        assert_eq!(snap.triangle_count(), SNAPSHOT_TRIANGLES_MAX);
    }

    #[test]
    fn one_particle_skips_distant_snapshot_triangles() {
        // CCW from +Y: cross(Z, X) = +Y.
        let near = [Vec3::ZERO, Vec3::Z, Vec3::X];
        let mut crowded = vec![near];
        for i in 0..SNAPSHOT_TRIANGLES_MAX - 1 {
            let x = 1000.0 + i as f32;
            crowded.push([
                Vec3::new(x, 0.0, 0.0),
                Vec3::new(x, 0.0, 1.0),
                Vec3::new(x + 1.0, 0.0, 0.0),
            ]);
        }
        let shape_of = |triangles: Vec<[Vec3; 3]>| SnapshotShape {
            key: (1, 0, 0),
            kind: SnapshotKind::Triangles { triangles },
            prev: Isometry::IDENTITY,
            cur: Isometry::IDENTITY,
            friction: 0.2,
        };
        let frame = SubstepFrame {
            t0: 0.0,
            t1: 1.0,
            h: 1.0 / 60.0,
            thickness: 0.05,
        };
        let point = Vec3::new(0.2, 0.02, 0.2);
        let lone = WorldSnapshot::new(vec![shape_of(vec![near])]).expect("near triangle");
        let mut lone_hits = Vec::new();
        lone.collide_shape(0, point, point, frame, |hit| lone_hits.push(hit.push));
        TRIANGLE_VISITS.store(0, Ordering::Relaxed);
        let crowded_snap = WorldSnapshot::new(vec![shape_of(crowded)]).expect("crowded");
        let mut hits = Vec::new();
        crowded_snap.collide_shape(0, point, point, frame, |hit| {
            hits.push((hit.push, hit.normal));
        });
        let visits = TRIANGLE_VISITS.load(Ordering::Relaxed);
        assert_eq!(lone_hits.len(), 1, "the near triangle must contact");
        assert_eq!(hits.len(), 1, "distant triangles must not add contacts");
        let (push, normal) = hits[0];
        assert!(
            (push - lone_hits[0]).length() < 1e-5,
            "push {push} vs {}",
            lone_hits[0]
        );
        assert!((normal - Vec3::Y).length() < 1e-5, "normal {normal}");
        assert!((push - Vec3::new(0.0, 0.03, 0.0)).length() < 1e-5, "push {push}");
        assert!(
            (1..64).contains(&visits),
            "triangle visits {visits} (full list is {})",
            SNAPSHOT_TRIANGLES_MAX
        );
        eprintln!("snapshot_triangle_visits {visits}");
    }

    #[test]
    fn the_triangle_grid_rescues_a_particle_that_crossed_into_the_next_cell() {
        // Corr-physics audit: the grid query only reached `thickness` around the end point, so a
        // particle that crossed a triangle's plane by more than that, into the next grid cell,
        // was never tested against it (the full-list scan rescues it).
        let near = [
            Vec3::new(0.0, 5.0, 0.0),
            Vec3::new(0.0, 5.0, 1.0),
            Vec3::new(1.0, 5.0, 0.0),
        ];
        let low = [
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(10.0, 0.0, 1.0),
            Vec3::new(11.0, 0.0, 0.0),
        ];
        let high = [
            Vec3::new(10.0, 16.0, 0.0),
            Vec3::new(10.0, 16.0, 1.0),
            Vec3::new(11.0, 16.0, 0.0),
        ];
        let shape = SnapshotShape {
            key: (1, 0, 0),
            kind: SnapshotKind::Triangles {
                triangles: vec![near, low, high],
            },
            prev: Isometry::IDENTITY,
            cur: Isometry::IDENTITY,
            friction: 0.2,
        };
        let frame = SubstepFrame {
            t0: 0.0,
            t1: 1.0,
            h: 1.0 / 60.0,
            thickness: 0.05,
        };
        let (x, x_prev) = (Vec3::new(0.2, 4.9, 0.2), Vec3::new(0.2, 5.02, 0.2));
        let mut full = Vec::new();
        collide(&shape, None, x, x_prev, frame, |hit| full.push(hit.push));
        assert_eq!(full.len(), 1, "the full scan rescues the particle");
        let snap = WorldSnapshot::new(vec![shape]).expect("snapshot");
        let mut gridded = Vec::new();
        snap.collide_shape(0, x, x_prev, frame, |hit| gridded.push(hit.push));
        assert_eq!(gridded, full, "the grid must match the full scan");
        assert!(
            (full[0] - Vec3::new(0.0, 0.15, 0.0)).length() < 1e-5,
            "{:?}",
            full[0]
        );
    }

    #[test]
    fn a_tetrahedron_stays_a_hull_and_its_aabb_corner_is_outside() {
        let points = [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z];
        let planes = snapshot_hull_planes(&points).expect("four points enclose a volume");
        assert!(
            (4..=SNAPSHOT_HULL_PLANES_MAX).contains(&planes.len()),
            "{planes:?}"
        );
        let distance = |p: Vec3| planes.iter().map(|(n, d)| n.dot(p) - *d).fold(f32::MIN, f32::max);
        // (0.9, 0.9, 0.9) is inside the points' cube and outside the tetrahedron x+y+z <= 1.
        assert!(
            distance(Vec3::new(0.9, 0.9, 0.9)) > 0.5,
            "the cube corner is outside the hull"
        );
        assert!(
            distance(Vec3::new(0.1, 0.1, 0.1)) < 0.0,
            "a point inside the tetrahedron is inside the hull"
        );
        assert!(snapshot_hull_planes(&[Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::new(1.0, 1.0, 0.0)]).is_none());
    }

    #[test]
    fn a_hull_with_too_many_vertices_is_refused_and_sixty_five_planes_are_an_error() {
        let mut points = Vec::with_capacity(96);
        let golden = std::f32::consts::PI * (3.0 - 5.0_f32.sqrt());
        for i in 0..96 {
            let y = 1.0 - (i as f32 / 95.0) * 2.0;
            let radius = (1.0 - y * y).sqrt();
            let theta = golden * i as f32;
            points.push(Vec3::new(theta.cos() * radius, y, theta.sin() * radius));
        }
        let hull = crate::convex_decomposition::convex_hull_of_points(&points, 0).expect("sphere");
        assert!(
            hull.vertices.len() > SNAPSHOT_HULL_VERTICES_MAX,
            "the sphere hull has {} vertices",
            hull.vertices.len()
        );
        assert!(snapshot_hull_planes(&points).is_none());

        let shape = SnapshotShape {
            key: (1, 0, 0),
            kind: SnapshotKind::Hull {
                planes: vec![(Vec3::X, 1.0); SNAPSHOT_HULL_PLANES_MAX + 1],
            },
            prev: Isometry::IDENTITY,
            cur: Isometry::IDENTITY,
            friction: 0.0,
        };
        let err = WorldSnapshot::new(vec![shape]).expect_err("65 planes");
        assert_eq!(err.code(), "cloth_snapshot_too_large");
    }
}
