// SPDX-License-Identifier: MPL-2.0
//! Geometry of post-volume shapes (`docs/specs/30-post-volumes-and-physical-camera.md` § 10,
//! ADR-2323): the exact signed distance of a convex hull of up to [`MAX_HULL_PLANES`] planes, and
//! the outline of every shape and of its blend band (the editor gizmo's data, headless).
//!
//! # Convex hull distance
//!
//! A hull is the intersection of half-spaces `n · x ≤ d` in the entity's local space. The planes
//! are moved to world space exactly (an affine map takes a plane to a plane: the normal by the
//! inverse transpose), so the distance is the world distance in meters under any rotation, scale
//! or shear. Inside, the signed distance is `max_i (n̂_i · p − d_i)` (negative): for a convex
//! region the nearest boundary point lies on the nearest face plane. Outside, the nearest point
//! of the hull lies in the relative interior of a face, an edge or a vertex, and for each it is
//! the projection of `p` onto that face's plane, edge's line or the vertex itself; the distance
//! is the smallest over the candidates that satisfy every plane. Outside, `max_i` is a lower
//! bound of the true distance, so a point beyond the blend band is rejected without the search.
//! The math runs in `f64` so the result does not depend on plane order beyond the last bits.

use regia_core::math::{Affine3A, Vec3};
use regia_scene::GlobalTransform;

/// The most planes a [`crate::PostVolumeShape::ConvexHull`] may have; a hull with more (or none)
/// is invalid and contributes nothing.
pub const MAX_HULL_PLANES: usize = 16;

/// Segments per circle of a sphere outline.
pub const SPHERE_OUTLINE_SEGMENTS: usize = 24;

/// A unit-normal world plane: `n · x − d` is the signed distance of `x`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Plane {
    n: [f64; 3],
    d: f64,
}

/// World planes of a hull, on the stack.
#[derive(Clone, Copy, Debug)]
struct WorldHull {
    planes: [Plane; MAX_HULL_PLANES],
    len: usize,
}

impl WorldHull {
    fn planes(&self) -> &[Plane] {
        &self.planes[..self.len]
    }
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn axpy(a: f64, x: [f64; 3], y: [f64; 3]) -> [f64; 3] {
    [a * x[0] + y[0], a * x[1] + y[1], a * x[2] + y[2]]
}

fn length(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

/// The local planes are valid: 1 to [`MAX_HULL_PLANES`] planes, every value finite and every
/// normal longer than `1e-6`.
#[must_use]
pub fn hull_planes_valid(planes: &[[f32; 4]]) -> bool {
    (1..=MAX_HULL_PLANES).contains(&planes.len())
        && planes
            .iter()
            .all(|p| p.iter().all(|v| v.is_finite()) && Vec3::new(p[0], p[1], p[2]).length() > 1.0e-6)
}

/// Moves local planes to unit-normal world planes, `None` for an invalid hull or a degenerate
/// transform. `offset` pushes every plane out by that many world meters (the band outline).
fn world_hull(planes: &[[f32; 4]], world_from_local: &Affine3A, offset: f64) -> Option<WorldHull> {
    if !hull_planes_valid(planes) {
        return None;
    }
    let m = world_from_local.matrix3;
    let det = m.determinant();
    if !det.is_finite() || det.abs() <= 1.0e-12 {
        return None;
    }
    // A local point l maps to x = M l + t; n · l ≤ d becomes (M⁻ᵀ n) · (x − t) ≤ d.
    let inverse_transpose = m.inverse().transpose();
    let t = Vec3::from(world_from_local.translation);
    let mut out = WorldHull {
        planes: [Plane::default(); MAX_HULL_PLANES],
        len: planes.len(),
    };
    for (dst, src) in out.planes.iter_mut().zip(planes) {
        let n = inverse_transpose * Vec3::new(src[0], src[1], src[2]);
        let n = [f64::from(n.x), f64::from(n.y), f64::from(n.z)];
        let len = length(n);
        if !len.is_finite() || len <= 1.0e-12 {
            return None;
        }
        let n = n.map(|v| v / len);
        let t = [f64::from(t.x), f64::from(t.y), f64::from(t.z)];
        *dst = Plane {
            n,
            d: f64::from(src[3]) / len + dot(n, t) + offset,
        };
    }
    Some(out)
}

/// `max_i (n̂_i · p − d_i)`: the exact signed distance inside, a lower bound outside.
fn max_plane_distance(hull: &WorldHull, p: [f64; 3]) -> f64 {
    hull.planes()
        .iter()
        .map(|pl| dot(pl.n, p) - pl.d)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// The tolerance a candidate point may violate a plane by and still count as on the hull.
fn tolerance(p: [f64; 3]) -> f64 {
    1.0e-9 * (1.0 + p.iter().fold(0.0f64, |m, v| m.max(v.abs())))
}

fn feasible(hull: &WorldHull, x: [f64; 3], eps: f64) -> bool {
    hull.planes().iter().all(|pl| dot(pl.n, x) - pl.d <= eps)
}

/// The point where planes `a`, `b` and `c` meet (`None` when two are parallel).
fn vertex(a: &Plane, b: &Plane, c: &Plane) -> Option<[f64; 3]> {
    let bc = cross(b.n, c.n);
    let det = dot(a.n, bc);
    if det.abs() < 1.0e-12 {
        return None;
    }
    let x = axpy(
        a.d,
        bc,
        axpy(b.d, cross(c.n, a.n), cross(a.n, b.n).map(|v| v * c.d)),
    );
    Some(x.map(|v| v / det))
}

/// The projection of `p` onto the line where planes `a` and `b` meet.
fn edge_projection(a: &Plane, b: &Plane, p: [f64; 3]) -> Option<[f64; 3]> {
    let c = dot(a.n, b.n);
    let det = 1.0 - c * c;
    if det < 1.0e-12 {
        return None;
    }
    // x = p − α a − β b with a · x = d_a and b · x = d_b.
    let ra = dot(a.n, p) - a.d;
    let rb = dot(b.n, p) - b.d;
    let alpha = (ra - c * rb) / det;
    let beta = (rb - c * ra) / det;
    Some(axpy(-beta, b.n, axpy(-alpha, a.n, p)))
}

/// Exact distance from an outside `p` to the hull (`f64::INFINITY` for an empty hull).
fn outside_distance(hull: &WorldHull, p: [f64; 3]) -> f64 {
    let eps = tolerance(p);
    let planes = hull.planes();
    let mut best = f64::INFINITY;
    let mut consider = |x: [f64; 3]| {
        if feasible(hull, x, eps) {
            best = best.min(length(sub(p, x)));
        }
    };
    for (i, a) in planes.iter().enumerate() {
        let r = dot(a.n, p) - a.d;
        if r > 0.0 {
            consider(axpy(-r, a.n, p));
        }
        for (j, b) in planes.iter().enumerate().skip(i + 1) {
            if let Some(x) = edge_projection(a, b, p) {
                consider(x);
            }
            for c in planes.iter().skip(j + 1) {
                if let Some(x) = vertex(a, b, c) {
                    consider(x);
                }
            }
        }
    }
    best
}

/// World signed distance from `point` to the hull of local `planes` placed by `world_from_local`
/// (meters, positive outside). An invalid hull, a degenerate transform or a non-finite point is
/// `f32::INFINITY` (never inside); an empty intersection is `f32::INFINITY` too.
#[must_use]
pub fn hull_signed_distance(planes: &[[f32; 4]], world_from_local: &GlobalTransform, point: [f32; 3]) -> f32 {
    hull_signed_distance_within(planes, world_from_local, point, f32::INFINITY)
}

/// [`hull_signed_distance`], but any point whose distance is certainly above `reach` returns
/// the lower bound instead of the exact distance (cheap rejection of far volumes).
#[must_use]
pub fn hull_signed_distance_within(
    planes: &[[f32; 4]],
    world_from_local: &GlobalTransform,
    point: [f32; 3],
    reach: f32,
) -> f32 {
    if !point.iter().all(|v| v.is_finite()) {
        return f32::INFINITY;
    }
    let Some(hull) = world_hull(planes, &world_from_local.0, 0.0) else {
        return f32::INFINITY;
    };
    let p = point.map(f64::from);
    let bound = max_plane_distance(&hull, p);
    if bound <= 0.0 {
        // Inside every plane (so the hull is not empty): the nearest face plane is exact.
        return bound as f32;
    }
    if f64::from(reach) < bound {
        return bound as f32;
    }
    outside_distance(&hull, p) as f32
}

/// The edges of the hull's boundary in world space (pairs of vertices on two planes), and
/// whether the hull is bounded (every plane pair's edge closes; a hull that is a slab, a wedge
/// or a single half-space has rays this outline leaves out).
fn hull_edges(hull: &WorldHull, out: &mut Vec<[[f32; 3]; 2]>) -> bool {
    let planes = hull.planes();
    // A bounded polyhedron needs at least four half-spaces.
    let mut bounded = planes.len() >= 4;
    for (i, a) in planes.iter().enumerate() {
        for (j, b) in planes.iter().enumerate().skip(i + 1) {
            let dir = cross(a.n, b.n);
            if length(dir) < 1.0e-9 {
                continue;
            }
            // Vertices on this line: where a third plane cuts it, inside the hull.
            let mut lo: Option<(f64, [f64; 3])> = None;
            let mut hi: Option<(f64, [f64; 3])> = None;
            for (k, c) in planes.iter().enumerate() {
                if k == i || k == j {
                    continue;
                }
                let Some(x) = vertex(a, b, c) else {
                    continue;
                };
                if !feasible(hull, x, tolerance(x) * 1.0e3) {
                    continue;
                }
                let s = dot(dir, x);
                if lo.is_none_or(|(ls, _)| s < ls) {
                    lo = Some((s, x));
                }
                if hi.is_none_or(|(hs, _)| s > hs) {
                    hi = Some((s, x));
                }
            }
            match (lo, hi) {
                (Some((ls, x0)), Some((hs, x1))) if hs - ls > 1.0e-9 => {
                    out.push([x0.map(|v| v as f32), x1.map(|v| v as f32)]);
                }
                _ => {
                    // At most one vertex on this line: the hull reaches along it to infinity (a
                    // ray or a whole line) when a point a step away from the vertex, or from the
                    // origin's projection without one, is still inside.
                    let Some(base) = lo.map(|(_, x)| x).or_else(|| edge_projection(a, b, [0.0; 3])) else {
                        continue;
                    };
                    let unit = dir.map(|v| v / length(dir));
                    let step = 1.0 + base.iter().fold(0.0f64, |m, v| m.max(v.abs()));
                    let eps = tolerance(base) * 1.0e3;
                    if feasible(hull, axpy(step, unit, base), eps)
                        || feasible(hull, axpy(-step, unit, base), eps)
                    {
                        bounded = false;
                    }
                }
            }
        }
    }
    bounded
}

/// The editor gizmo's data for one post volume, in world space: line segments of the shape
/// and of the outer edge of its blend band.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VolumeOutline {
    /// Segments of the shape itself.
    pub shape: Vec<[[f32; 3]; 2]>,
    /// Segments of the band's outer edge (empty without a blend band). For a box and a hull
    /// this is the shape with every face pushed out by the blend distance (the exact band has
    /// rounded edges and corners; the outline bounds it); for a sphere it is exact.
    pub band: Vec<[[f32; 3]; 2]>,
    /// Every edge of the shape is drawn (`false` for a hull with rays, which the outline omits).
    pub bounded: bool,
}

fn transform_point(world: &Affine3A, p: Vec3) -> [f32; 3] {
    world.transform_point3(p).to_array()
}

fn box_segments(world: &Affine3A, half: Vec3, out: &mut Vec<[[f32; 3]; 2]>) {
    let corner = |i: usize| {
        Vec3::new(
            if i & 1 == 0 { -half.x } else { half.x },
            if i & 2 == 0 { -half.y } else { half.y },
            if i & 4 == 0 { -half.z } else { half.z },
        )
    };
    for i in 0..8usize {
        for bit in [1usize, 2, 4] {
            if i & bit == 0 {
                out.push([
                    transform_point(world, corner(i)),
                    transform_point(world, corner(i | bit)),
                ]);
            }
        }
    }
}

fn circle_segments(center: Vec3, u: Vec3, v: Vec3, radius: f32, out: &mut Vec<[[f32; 3]; 2]>) {
    let at = |k: usize| {
        let a = std::f32::consts::TAU * k as f32 / SPHERE_OUTLINE_SEGMENTS as f32;
        (center + radius * (a.cos() * u + a.sin() * v)).to_array()
    };
    for k in 0..SPHERE_OUTLINE_SEGMENTS {
        out.push([at(k), at(k + 1)]);
    }
}

fn sphere_segments(center: Vec3, radius: f32, out: &mut Vec<[[f32; 3]; 2]>) {
    circle_segments(center, Vec3::X, Vec3::Y, radius, out);
    circle_segments(center, Vec3::Y, Vec3::Z, radius, out);
    circle_segments(center, Vec3::Z, Vec3::X, radius, out);
}

/// The outline of `volume` placed by `world_from_local`, `None` for an unbound volume (it covers
/// the world), an invalid shape or a degenerate transform. A disabled volume still outlines (the
/// editor greys it); the caller decides whether to draw it.
#[must_use]
pub fn post_volume_outline(
    volume: &crate::PostVolume,
    world_from_local: &GlobalTransform,
) -> Option<VolumeOutline> {
    use crate::PostVolumeShape;
    let band =
        (volume.blend_distance.is_finite() && volume.blend_distance > 0.0).then_some(volume.blend_distance);
    let world = &world_from_local.0;
    let mut outline = VolumeOutline {
        bounded: true,
        ..VolumeOutline::default()
    };
    match &volume.shape {
        PostVolumeShape::Unbound => return None,
        PostVolumeShape::Box { half_extents } => {
            let half = Vec3::from(*half_extents).abs();
            if !half.is_finite() {
                return None;
            }
            box_segments(world, half, &mut outline.shape);
            if let Some(band) = band {
                // Push each face out by `band` world meters: in local units that is `band` over
                // the length of the local axis in world space.
                let axis_len = Vec3::new(
                    world.matrix3.x_axis.length(),
                    world.matrix3.y_axis.length(),
                    world.matrix3.z_axis.length(),
                );
                if axis_len.min_element() <= 0.0 || !axis_len.is_finite() {
                    return None;
                }
                box_segments(world, half + Vec3::splat(band) / axis_len, &mut outline.band);
            }
        }
        PostVolumeShape::Sphere { radius } => {
            let (scale, _, translation) = world.to_scale_rotation_translation();
            let r = radius.abs() * 0.5 * (scale.x.abs() + scale.z.abs());
            if !r.is_finite() {
                return None;
            }
            sphere_segments(translation, r, &mut outline.shape);
            if let Some(band) = band {
                sphere_segments(translation, r + band, &mut outline.band);
            }
        }
        PostVolumeShape::ConvexHull { planes } => {
            let hull = world_hull(planes, world, 0.0)?;
            outline.bounded = hull_edges(&hull, &mut outline.shape);
            if let Some(band) = band {
                let grown = world_hull(planes, world, f64::from(band))?;
                hull_edges(&grown, &mut outline.band);
            }
        }
    }
    Some(outline)
}

/// The six planes of a box of `half_extents` centred on the origin, as hull planes (a test and
/// authoring helper: the hull then matches [`crate::PostVolumeShape::Box`]).
#[must_use]
pub fn box_hull_planes(half_extents: [f32; 3]) -> Vec<[f32; 4]> {
    let [x, y, z] = half_extents.map(f32::abs);
    vec![
        [1.0, 0.0, 0.0, x],
        [-1.0, 0.0, 0.0, x],
        [0.0, 1.0, 0.0, y],
        [0.0, -1.0, 0.0, y],
        [0.0, 0.0, 1.0, z],
        [0.0, 0.0, -1.0, z],
    ]
}
