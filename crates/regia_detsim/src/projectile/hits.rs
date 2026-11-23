// SPDX-License-Identifier: MPL-2.0
//! [`LinearScanHits`]: the reference [`SegmentHits`], every shape tested in turn (ADR-8509).
//!
//! Swept sphere against a plane (exact), a box grown by the radius on each axis (the slab test; at
//! the box's edges and corners the grown box is square where the exact shape is rounded, so a
//! grazing shot may touch a little early) and a body sphere (exact). The earliest contact wins; at
//! the same `t`, planes before boxes before spheres, then the lower index. Arithmetic is IEEE `f32`
//! in source order with `sqrt` through [`crate::math`], so the answers are the same on every peer.

use crate::float::{det_max, det_min};
use crate::math;

use super::{Hit, HitTarget, SegmentHits, SegmentQuery};

/// No geometry: every query misses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoHits;

impl SegmentHits for NoHits {
    fn first_hit(&mut self, _query: &SegmentQuery) -> Option<Hit> {
        None
    }
}

/// A solid half-space: the points `p` with `dot(normal, p) < offset` are inside.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HitPlane {
    /// Unit normal, pointing out of the solid.
    pub normal: [f32; 3],
    /// `dot(normal, p)` on the surface.
    pub offset: f32,
}

impl HitPlane {
    /// Flat ground at height `y` (solid below, normal +Y).
    pub const fn ground(y: f32) -> Self {
        HitPlane {
            normal: [0.0, 1.0, 0.0],
            offset: y,
        }
    }
}

/// A solid axis-aligned box.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HitBox {
    /// Smallest corner.
    pub min: [f32; 3],
    /// Largest corner.
    pub max: [f32; 3],
}

/// A body: a sphere with the caller's id.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HitSphere {
    /// Centre.
    pub center: [f32; 3],
    /// Radius, m.
    pub radius_m: f32,
    /// The body's id, reported as [`HitTarget::Body`].
    pub body: u32,
}

/// The reference [`SegmentHits`]: planes and boxes ([`HitTarget::World`]) and body spheres
/// ([`HitTarget::Body`]), each tested against every query. Cost is linear in the shapes; a game with
/// many shapes puts a spatial index behind [`SegmentHits`] and checks it against this one.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LinearScanHits {
    /// Half-spaces (ground, sea floor, a level's outer walls).
    pub planes: Vec<HitPlane>,
    /// Boxes (walls, crates).
    pub boxes: Vec<HitBox>,
    /// Bodies.
    pub spheres: Vec<HitSphere>,
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn lerp(a: [f32; 3], d: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + d[0] * t, a[1] + d[1] * t, a[2] + d[2] * t]
}

/// The first contact of the sphere with the plane: `t`.
fn plane_hit(plane: &HitPlane, q: &SegmentQuery) -> Option<f32> {
    let d0 = dot(plane.normal, q.from) - plane.offset - q.radius_m;
    let d1 = dot(plane.normal, q.to) - plane.offset - q.radius_m;
    // Only a sphere that moves into the surface and ends the tick past it.
    if d1 >= 0.0 || d1 >= d0 {
        return None;
    }
    Some(if d0 <= 0.0 { 0.0 } else { d0 / (d0 - d1) })
}

/// Slab test of the segment against the box grown by the radius: `(t, normal)`.
fn box_hit(b: &HitBox, q: &SegmentQuery) -> Option<(f32, [f32; 3])> {
    let d = [q.to[0] - q.from[0], q.to[1] - q.from[1], q.to[2] - q.from[2]];
    let r = q.radius_m;
    let mut enter = f32::NEG_INFINITY;
    let mut exit = f32::INFINITY;
    let mut normal = [0.0_f32; 3];
    for (axis, &da) in d.iter().enumerate() {
        let lo = b.min[axis] - r;
        let hi = b.max[axis] + r;
        let o = q.from[axis];
        if da == 0.0 {
            if o < lo || o > hi {
                return None;
            }
            continue;
        }
        let t_lo = (lo - o) / da;
        let t_hi = (hi - o) / da;
        let (near, far, sign) = if t_lo < t_hi {
            (t_lo, t_hi, -1.0)
        } else {
            (t_hi, t_lo, 1.0)
        };
        if near > enter {
            enter = near;
            normal = [0.0; 3];
            normal[axis] = sign;
        }
        exit = det_min(exit, far);
    }
    if enter > exit || exit < 0.0 || enter > 1.0 {
        return None;
    }
    if enter < 0.0 {
        // Starting inside or on the surface: the nearest face is the one in contact (lower axis,
        // then the low face, on a tie); a contact at 0 only when the motion goes into it.
        let mut depth = f32::INFINITY;
        let mut face = [0.0_f32; 3];
        for (axis, &o) in q.from.iter().enumerate() {
            for (gap, sign) in [(o - (b.min[axis] - r), -1.0), ((b.max[axis] + r) - o, 1.0)] {
                if gap < depth {
                    depth = gap;
                    face = [0.0; 3];
                    face[axis] = sign;
                }
            }
        }
        if dot(face, d) >= 0.0 {
            return None;
        }
        return Some((0.0, face));
    }
    if dot(normal, d) >= 0.0 {
        return None;
    }
    Some((det_max(enter, 0.0), normal))
}

/// Swept sphere against a sphere: `t`.
fn sphere_hit(s: &HitSphere, q: &SegmentQuery) -> Option<f32> {
    let rr = q.radius_m + s.radius_m;
    let m = [
        q.from[0] - s.center[0],
        q.from[1] - s.center[1],
        q.from[2] - s.center[2],
    ];
    let d = [q.to[0] - q.from[0], q.to[1] - q.from[1], q.to[2] - q.from[2]];
    let a = dot(d, d);
    let b = dot(m, d);
    let c = dot(m, m) - rr * rr;
    if b >= 0.0 {
        // Moving away from (or along the surface of) the sphere.
        return None;
    }
    if c <= 0.0 {
        return Some(0.0);
    }
    if a <= 0.0 {
        return None;
    }
    let disc = b * b - a * c;
    if disc < 0.0 {
        return None;
    }
    let t = (-b - math::sqrt(disc)) / a;
    if t > 1.0 { None } else { Some(det_max(t, 0.0)) }
}

fn sphere_normal(s: &HitSphere, point: [f32; 3]) -> [f32; 3] {
    let n = [
        point[0] - s.center[0],
        point[1] - s.center[1],
        point[2] - s.center[2],
    ];
    let len = math::sqrt(dot(n, n));
    if len > 1.0e-12 {
        [n[0] / len, n[1] / len, n[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

impl SegmentHits for LinearScanHits {
    fn first_hit(&mut self, q: &SegmentQuery) -> Option<Hit> {
        let d = [q.to[0] - q.from[0], q.to[1] - q.from[1], q.to[2] - q.from[2]];
        let mut best: Option<(f32, [f32; 3], HitTarget)> = None;
        let mut take = |t: f32, normal: [f32; 3], target: HitTarget| {
            if best.is_none_or(|(bt, _, _)| t < bt) {
                best = Some((t, normal, target));
            }
        };
        for plane in &self.planes {
            if let Some(t) = plane_hit(plane, q) {
                take(t, plane.normal, HitTarget::World);
            }
        }
        for b in &self.boxes {
            if let Some((t, normal)) = box_hit(b, q) {
                take(t, normal, HitTarget::World);
            }
        }
        for s in &self.spheres {
            if q.ignore_body == Some(s.body) {
                continue;
            }
            if let Some(t) = sphere_hit(s, q) {
                take(t, sphere_normal(s, lerp(q.from, d, t)), HitTarget::Body(s.body));
            }
        }
        best.map(|(t, normal, target)| Hit {
            t,
            point: lerp(q.from, d, t),
            normal,
            target,
        })
    }
}
