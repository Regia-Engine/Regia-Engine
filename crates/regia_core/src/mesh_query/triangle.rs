// SPDX-License-Identifier: MPL-2.0
//! Single-triangle primitives: closest point, ray hit, triangle/box and triangle/triangle overlap,
//! and the solid angle a triangle subtends. The f32 functions here convert to `f64`, do the work in
//! `f64` and convert back; the mesh queries call the `f64` forms directly. `solid_angle` is the
//! exception: it returns `f64` steradians and is not converted back.
//!
//! Sources (`docs/provenance.md`, "Core / mesh queries"):
//! - closest point: C. Ericson, *Real-Time Collision Detection* (2005) § 5.1.5, Voronoi-region
//!   method; zero-area triangles fall back to the closest of the three edges;
//! - ray hit: T. Möller, B. Trumbore, "Fast, Minimum Storage Ray/Triangle Intersection", JGT 1997;
//! - triangle/box: T. Akenine-Möller, "Fast 3D Triangle-Box Overlap Testing", JGT 2001 (13 axes);
//! - triangle/triangle: the separating axis theorem (Ericson § 4.4 / § 5.2), with the in-plane edge
//!   normals added so coplanar and degenerate pairs are handled;
//! - solid angle: A. van Oosterom, J. Strackee, "The Solid Angle of a Plane Triangle", IEEE Trans.
//!   Biomed. Eng. 30(2), 1983.

use glam::{DVec3, Vec3};

use crate::math::detmath::det_atan2_f64;

/// `sin²` of the angle between two edges below which a triangle counts as degenerate (zero area).
const DEGENERATE_SIN2: f64 = 1e-20;

/// Relative barycentric slack for ray hits, so a ray through a shared edge or vertex hits at
/// least one of the triangles that meet there.
const RAY_EDGE_SLACK: f64 = 1e-9;

/// `true` when the triangle has (numerically) zero area: coincident vertices or collinear edges.
pub fn is_degenerate(a: Vec3, b: Vec3, c: Vec3) -> bool {
    is_degenerate_d(a.as_dvec3(), b.as_dvec3(), c.as_dvec3())
}

pub(crate) fn is_degenerate_d(a: DVec3, b: DVec3, c: DVec3) -> bool {
    let (ab, ac) = (b - a, c - a);
    let n2 = ab.cross(ac).length_squared();
    n2 <= DEGENERATE_SIN2 * ab.length_squared() * ac.length_squared()
}

/// The closest point to `p` on triangle `abc` and its barycentric coordinates `(u, v, w)`
/// (`point = u·a + v·b + w·c`). Works for degenerate triangles (segments and points).
pub fn closest_point(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> (Vec3, Vec3) {
    let (q, bary) = closest_point_d(p.as_dvec3(), a.as_dvec3(), b.as_dvec3(), c.as_dvec3());
    (q.as_vec3(), bary.as_vec3())
}

/// Closest point on segment `ab` to `p`, as the parameter `t` (point = a + t·(b − a)).
fn segment_param(p: DVec3, a: DVec3, b: DVec3) -> f64 {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 == 0.0 {
        0.0
    } else {
        ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
    }
}

pub(crate) fn closest_point_d(p: DVec3, a: DVec3, b: DVec3, c: DVec3) -> (DVec3, DVec3) {
    if is_degenerate_d(a, b, c) {
        // Zero area: the closest point is on one of the edges.
        let candidates = [(a, b, 0usize, 1usize), (b, c, 1, 2), (c, a, 2, 0)];
        let mut best = (a, DVec3::X, f64::INFINITY);
        for (s0, s1, i0, i1) in candidates {
            let t = segment_param(p, s0, s1);
            let q = s0 + (s1 - s0) * t;
            let d2 = q.distance_squared(p);
            if d2 < best.2 {
                let mut bary = DVec3::ZERO;
                bary[i0] = 1.0 - t;
                bary[i1] += t;
                best = (q, bary, d2);
            }
        }
        return (best.0, best.1);
    }
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return (a, DVec3::X);
    }
    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return (b, DVec3::Y);
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return (a + ab * v, DVec3::new(1.0 - v, v, 0.0));
    }
    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return (c, DVec3::Z);
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return (a + ac * w, DVec3::new(1.0 - w, 0.0, w));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return (b + (c - b) * w, DVec3::new(0.0, 1.0 - w, w));
    }
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    (a + ab * v + ac * w, DVec3::new(1.0 - v - w, v, w))
}

/// Distance along a ray (`origin`, unit `direction`) to triangle `abc`, with the barycentric
/// coordinates `(v, w)` of the hit for `b` and `c`. Both faces count; hits behind the origin and
/// rays parallel to the triangle's plane (including every degenerate triangle) miss.
pub fn ray_hit(origin: Vec3, direction: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<(f32, f32, f32)> {
    ray_hit_d(
        origin.as_dvec3(),
        direction.as_dvec3(),
        a.as_dvec3(),
        b.as_dvec3(),
        c.as_dvec3(),
    )
    .map(|(t, v, w)| (t as f32, v as f32, w as f32))
}

pub(crate) fn ray_hit_d(o: DVec3, d: DVec3, a: DVec3, b: DVec3, c: DVec3) -> Option<(f64, f64, f64)> {
    let e1 = b - a;
    let e2 = c - a;
    let pvec = d.cross(e2);
    let det = e1.dot(pvec);
    // Parallel to the plane, or zero area: no single hit point.
    let scale = e1.length() * e2.length() * d.length();
    if det.is_nan() || det.abs() <= 1e-12 * scale {
        return None;
    }
    let inv = 1.0 / det;
    let tvec = o - a;
    let v = tvec.dot(pvec) * inv;
    if !(-RAY_EDGE_SLACK..=1.0 + RAY_EDGE_SLACK).contains(&v) {
        return None;
    }
    let qvec = tvec.cross(e1);
    let w = d.dot(qvec) * inv;
    if w < -RAY_EDGE_SLACK || v + w > 1.0 + RAY_EDGE_SLACK {
        return None;
    }
    let t = e2.dot(qvec) * inv;
    if t < 0.0 || !t.is_finite() {
        return None;
    }
    Some((t, v.clamp(0.0, 1.0), w.clamp(0.0, 1.0)))
}

/// `true` if triangle `abc` and the box `[min, max]` overlap (touching counts).
pub fn overlaps_aabb(a: Vec3, b: Vec3, c: Vec3, min: Vec3, max: Vec3) -> bool {
    overlaps_aabb_d(
        [a.as_dvec3(), b.as_dvec3(), c.as_dvec3()],
        min.as_dvec3(),
        max.as_dvec3(),
    )
}

pub(crate) fn overlaps_aabb_d(tri: [DVec3; 3], min: DVec3, max: DVec3) -> bool {
    let center = (min + max) * 0.5;
    let h = (max - min) * 0.5;
    let v = [tri[0] - center, tri[1] - center, tri[2] - center];
    // The three box axes.
    for axis in 0..3 {
        let lo = v[0][axis].min(v[1][axis]).min(v[2][axis]);
        let hi = v[0][axis].max(v[1][axis]).max(v[2][axis]);
        if lo > h[axis] || hi < -h[axis] {
            return false;
        }
    }
    let e = [v[1] - v[0], v[2] - v[1], v[0] - v[2]];
    // The triangle's plane (a zero normal never separates).
    let n = e[0].cross(e[1]);
    if n.dot(v[0]).abs() > h.dot(n.abs()) {
        return false;
    }
    // The nine edge × box-axis axes.
    for edge in e {
        for unit in [DVec3::X, DVec3::Y, DVec3::Z] {
            let axis = edge.cross(unit);
            let r = h.dot(axis.abs());
            let p = [axis.dot(v[0]), axis.dot(v[1]), axis.dot(v[2])];
            let lo = p[0].min(p[1]).min(p[2]);
            let hi = p[0].max(p[1]).max(p[2]);
            if lo > r || hi < -r {
                return false;
            }
        }
    }
    true
}

/// `true` if the two triangles intersect or touch. Coplanar and degenerate triangles are handled;
/// two degenerate triangles that are collinear segments on one line, or coplanar parallel
/// segments, may report a touch they do not have (the test is conservative there).
pub fn triangles_intersect(t0: [Vec3; 3], t1: [Vec3; 3]) -> bool {
    triangles_intersect_d(t0.map(Vec3::as_dvec3), t1.map(Vec3::as_dvec3))
}

fn project(axis: DVec3, t: &[DVec3; 3]) -> (f64, f64) {
    let p = [axis.dot(t[0]), axis.dot(t[1]), axis.dot(t[2])];
    (p[0].min(p[1]).min(p[2]), p[0].max(p[1]).max(p[2]))
}

pub(crate) fn triangles_intersect_d(t0: [DVec3; 3], t1: [DVec3; 3]) -> bool {
    // Work relative to the first triangle's centroid, so projection rounding scales with the
    // triangles' size rather than their distance from the world origin.
    let c = (t0[0] + t0[1] + t0[2]) / 3.0;
    let a = t0.map(|p| p - c);
    let b = t1.map(|p| p - c);
    let ea = [a[1] - a[0], a[2] - a[1], a[0] - a[2]];
    let eb = [b[1] - b[0], b[2] - b[1], b[0] - b[2]];
    let na = ea[0].cross(ea[1]);
    let nb = eb[0].cross(eb[1]);
    let separates = |axis: DVec3| {
        if axis == DVec3::ZERO {
            return false;
        }
        let (a0, a1) = project(axis, &a);
        let (b0, b1) = project(axis, &b);
        a1 < b0 || b1 < a0
    };
    if separates(na) || separates(nb) {
        return false;
    }
    for &x in &ea {
        for &y in &eb {
            if separates(x.cross(y)) {
                return false;
            }
        }
    }
    // In-plane edge normals: needed when the triangles are coplanar (or one is a segment).
    for n in [na, nb] {
        for edge in ea.iter().chain(eb.iter()) {
            if separates(n.cross(*edge)) {
                return false;
            }
        }
    }
    true
}

/// Signed solid angle (steradians) that triangle `abc` subtends at `p`: positive when `p` sees the
/// triangle's back, that is when `p` is on the side opposite to the counter-clockwise normal
/// `(b − a) × (c − a)`. Inside a closed, outward-facing mesh the solid angles sum to 4π.
pub fn solid_angle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> f64 {
    solid_angle_d(p.as_dvec3(), a.as_dvec3(), b.as_dvec3(), c.as_dvec3())
}

pub(crate) fn solid_angle_d(p: DVec3, a: DVec3, b: DVec3, c: DVec3) -> f64 {
    let (a, b, c) = (a - p, b - p, c - p);
    let (la, lb, lc) = (a.length(), b.length(), c.length());
    let num = a.dot(b.cross(c));
    let den = la * lb * lc + a.dot(b) * lc + b.dot(c) * la + c.dot(a) * lb;
    // The deterministic `atan2`: this sum decides inside/outside at cook time and must not
    // depend on the cook host's libm (ADR-4773).
    2.0 * det_atan2_f64(num, den)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri() -> [Vec3; 3] {
        [Vec3::ZERO, Vec3::X, Vec3::Y]
    }

    #[test]
    fn closest_point_regions() {
        let [a, b, c] = tri();
        let (q, bary) = closest_point(Vec3::new(0.25, 0.25, 2.0), a, b, c);
        assert!((q - Vec3::new(0.25, 0.25, 0.0)).length() < 1e-6);
        assert!((bary - Vec3::new(0.5, 0.25, 0.25)).length() < 1e-6);
        let (q, _) = closest_point(Vec3::new(-1.0, -1.0, 0.0), a, b, c);
        assert_eq!(q, a);
        let (q, _) = closest_point(Vec3::new(2.0, -0.5, 0.0), a, b, c);
        assert_eq!(q, b);
        let (q, _) = closest_point(Vec3::new(1.0, 1.0, 0.0), a, b, c);
        assert!((q - Vec3::new(0.5, 0.5, 0.0)).length() < 1e-6);
    }

    #[test]
    fn degenerate_triangles_have_closest_points() {
        // Collinear: a segment from 0 to 2 on x with a middle vertex.
        let (q, bary) = closest_point(
            Vec3::new(1.5, 1.0, 0.0),
            Vec3::ZERO,
            Vec3::X,
            Vec3::new(2.0, 0.0, 0.0),
        );
        assert!((q - Vec3::new(1.5, 0.0, 0.0)).length() < 1e-6, "{q}");
        assert!((bary.x + bary.y + bary.z - 1.0).abs() < 1e-6);
        // All three vertices equal.
        let p = Vec3::splat(3.0);
        let (q, _) = closest_point(Vec3::ZERO, p, p, p);
        assert_eq!(q, p);
        assert!(is_degenerate(p, p, p));
        assert!(!is_degenerate(Vec3::ZERO, Vec3::X, Vec3::Y));
    }

    #[test]
    fn ray_hits_both_faces_and_misses_degenerate() {
        let [a, b, c] = tri();
        let hit = ray_hit(Vec3::new(0.2, 0.2, 1.0), -Vec3::Z, a, b, c).expect("front");
        assert!((hit.0 - 1.0).abs() < 1e-6);
        assert!(ray_hit(Vec3::new(0.2, 0.2, -1.0), Vec3::Z, a, b, c).is_some());
        assert!(
            ray_hit(Vec3::new(0.2, 0.2, 1.0), Vec3::Z, a, b, c).is_none(),
            "behind"
        );
        assert!(
            ray_hit(Vec3::new(0.8, 0.8, 1.0), -Vec3::Z, a, b, c).is_none(),
            "outside"
        );
        assert!(
            ray_hit(
                Vec3::new(0.5, 0.0, 1.0),
                -Vec3::Z,
                Vec3::ZERO,
                Vec3::X,
                Vec3::X * 2.0
            )
            .is_none()
        );
    }

    #[test]
    fn triangle_box_overlap() {
        let [a, b, c] = tri();
        assert!(overlaps_aabb(a, b, c, Vec3::splat(-0.1), Vec3::splat(0.1)));
        assert!(!overlaps_aabb(
            a,
            b,
            c,
            Vec3::new(0.6, 0.6, -0.1),
            Vec3::new(1.0, 1.0, 0.1)
        ));
        assert!(!overlaps_aabb(
            a,
            b,
            c,
            Vec3::new(0.1, 0.1, 0.5),
            Vec3::new(0.2, 0.2, 0.6)
        ));
        // A degenerate (segment) triangle crossing a box.
        let s = [
            Vec3::new(-1.0, 0.5, 0.5),
            Vec3::new(2.0, 0.5, 0.5),
            Vec3::new(2.0, 0.5, 0.5),
        ];
        assert!(overlaps_aabb(s[0], s[1], s[2], Vec3::ZERO, Vec3::ONE));
    }

    #[test]
    fn triangle_triangle_cases() {
        let t = tri();
        // Crossing through the plane.
        let x = [
            Vec3::new(0.2, 0.2, -1.0),
            Vec3::new(0.2, 0.2, 1.0),
            Vec3::new(0.3, 0.1, 1.0),
        ];
        assert!(triangles_intersect(t, x));
        // Parallel and offset.
        assert!(!triangles_intersect(t, t.map(|p| p + Vec3::Z * 0.01)));
        // Coplanar overlapping and coplanar disjoint.
        assert!(triangles_intersect(t, t.map(|p| p + Vec3::new(0.2, 0.2, 0.0))));
        assert!(!triangles_intersect(t, t.map(|p| p + Vec3::new(0.6, 0.6, 0.0))));
        // Touching at a shared vertex.
        assert!(triangles_intersect(
            t,
            [Vec3::X, Vec3::new(2.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 1.0)]
        ));
        // A degenerate segment lying in the plane through the triangle, and one beside it.
        let seg = [
            Vec3::new(-1.0, 0.3, 0.0),
            Vec3::new(2.0, 0.3, 0.0),
            Vec3::new(2.0, 0.3, 0.0),
        ];
        assert!(triangles_intersect(t, seg));
        let off = [
            Vec3::new(-1.0, 1.3, 0.0),
            Vec3::new(0.0, 1.3, 0.0),
            Vec3::new(0.0, 1.3, 0.0),
        ];
        assert!(!triangles_intersect(t, off));
    }

    #[test]
    fn solid_angle_signs() {
        // A point above the CCW (normal +z) triangle sees its front: negative.
        let [a, b, c] = tri();
        assert!(solid_angle(Vec3::new(0.2, 0.2, 1.0), a, b, c) < 0.0);
        assert!(solid_angle(Vec3::new(0.2, 0.2, -1.0), a, b, c) > 0.0);
        // In the plane, outside the triangle: zero.
        assert!(solid_angle(Vec3::new(3.0, 3.0, 0.0), a, b, c).abs() < 1e-12);
        // Just behind the center: almost a hemisphere (2π).
        let w = solid_angle(Vec3::new(0.25, 0.25, -1e-7), a, b, c);
        assert!((w - std::f64::consts::TAU).abs() < 1e-4, "{w}");
    }

    /// Solid-angle bits for the points below, pinned (ADR-4773, audit G finding 6): the
    /// van Oosterom–Strackee `atan2` is the deterministic kernel, so a vertex near the 0.5
    /// winding threshold keeps its inside/outside bit on every cook host.
    const SOLID_ANGLE_BITS: [u64; 3] = [
        13_824_544_341_174_189_708,
        4_618_760_254_118_347_903,
        13_812_965_991_524_857_577,
    ];

    #[test]
    fn solid_angle_bits_are_platform_independent() {
        let [a, b, c] = tri();
        let bits = [
            solid_angle(Vec3::new(0.2, 0.2, 1.0), a, b, c).to_bits(),
            solid_angle(Vec3::new(0.25, 0.25, -1e-7), a, b, c).to_bits(),
            solid_angle(Vec3::new(0.6, -0.4, 2.5), a, b, c).to_bits(),
        ];
        assert_eq!(bits, SOLID_ANGLE_BITS);
    }
}
