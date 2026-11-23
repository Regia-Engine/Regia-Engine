// SPDX-License-Identifier: MPL-2.0
//! Hit-shape geometry for lag-compensated queries (spec 75 § 5 steps 2-3, spec 90 § 2): world-space
//! posed shapes, their bounds, and ray, sphere-cast and sphere-overlap tests.
//!
//! A sphere cast is a ray against the shape grown by the cast radius (the Minkowski sum). For a
//! sphere and a capsule that is the same kind of shape with a larger radius; for an oriented box
//! it is the union of three boxes each grown along one axis and twelve edge capsules, which is
//! exact. All math is `f32` in a fixed operation order.

use glam::{Quat, Vec3};
use regia_core::math::{Aabb, Ray};

use super::HitShapeKind;

/// Below this the ray is treated as parallel to a capsule axis or a box slab.
const PARALLEL_EPS: f32 = 1e-8;

/// A hit shape placed in the world at one tick: its kind, center and orientation. A capsule's
/// axis is its local Y; `half_height` is half the length of the segment between the cap centers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PosedShape {
    /// The shape and its size.
    pub kind: HitShapeKind,
    /// World-space center.
    pub position: Vec3,
    /// World-space orientation (unit quaternion).
    pub rotation: Quat,
}

/// The result of a narrow-phase test against one shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeHit {
    /// Distance along the ray (0 when the ray starts inside the grown shape).
    pub distance: f32,
    /// The contact point on the shape's surface (not the grown surface).
    pub point: Vec3,
    /// The outward surface normal at the contact.
    pub normal: Vec3,
}

impl PosedShape {
    /// The axis-aligned box enclosing the shape.
    pub fn aabb(&self) -> Aabb {
        let ext = match self.kind {
            HitShapeKind::Sphere { radius } => Vec3::splat(radius),
            HitShapeKind::Capsule { radius, half_height } => {
                (self.rotation * Vec3::Y * half_height).abs() + Vec3::splat(radius)
            }
            HitShapeKind::Box { half_extents } => {
                let x = (self.rotation * Vec3::X).abs() * half_extents.x;
                let y = (self.rotation * Vec3::Y).abs() * half_extents.y;
                let z = (self.rotation * Vec3::Z).abs() * half_extents.z;
                x + y + z
            }
        };
        Aabb {
            min: self.position - ext,
            max: self.position + ext,
        }
    }

    /// The capsule segment end points (both equal the center for other shapes).
    fn segment(&self) -> (Vec3, Vec3) {
        match self.kind {
            HitShapeKind::Capsule { half_height, .. } => {
                let axis = self.rotation * Vec3::Y * half_height;
                (self.position - axis, self.position + axis)
            }
            _ => (self.position, self.position),
        }
    }

    /// The point of the shape nearest `p` (`p` itself when inside).
    pub fn closest_point(&self, p: Vec3) -> Vec3 {
        match self.kind {
            HitShapeKind::Sphere { radius } => round_closest(self.position, radius, p),
            HitShapeKind::Capsule { radius, .. } => {
                let (a, b) = self.segment();
                round_closest(closest_on_segment(a, b, p), radius, p)
            }
            HitShapeKind::Box { half_extents } => {
                let local = self.rotation.inverse() * (p - self.position);
                let clamped = local.clamp(-half_extents, half_extents);
                self.position + self.rotation * clamped
            }
        }
    }

    /// The outward normal for a point on (or near) the surface, `fallback` when undefined.
    fn normal_at(&self, p: Vec3, fallback: Vec3) -> Vec3 {
        match self.kind {
            HitShapeKind::Sphere { .. } => (p - self.position).try_normalize().unwrap_or(fallback),
            HitShapeKind::Capsule { .. } => {
                let (a, b) = self.segment();
                (p - closest_on_segment(a, b, p))
                    .try_normalize()
                    .unwrap_or(fallback)
            }
            HitShapeKind::Box { half_extents } => {
                let local = self.rotation.inverse() * (p - self.position);
                let outside = local - local.clamp(-half_extents, half_extents);
                if let Some(n) = outside.try_normalize() {
                    return self.rotation * n;
                }
                // On or inside the box: the face whose plane is nearest.
                let gap = half_extents - local.abs();
                let axis = if gap.x <= gap.y && gap.x <= gap.z {
                    Vec3::X * local.x.signum()
                } else if gap.y <= gap.z {
                    Vec3::Y * local.y.signum()
                } else {
                    Vec3::Z * local.z.signum()
                };
                self.rotation * axis
            }
        }
    }

    /// The first hit of `ray` within `max_dist` against the shape grown by `inflate` (0 for a
    /// ray, the cast radius for a sphere cast).
    pub fn cast(&self, ray: &Ray, max_dist: f32, inflate: f32) -> Option<ShapeHit> {
        let t = match self.kind {
            HitShapeKind::Sphere { radius } => ray_sphere(ray, self.position, radius + inflate),
            HitShapeKind::Capsule { radius, .. } => {
                let (a, b) = self.segment();
                ray_capsule(ray, a, b, radius + inflate)
            }
            HitShapeKind::Box { half_extents } => self.ray_box(ray, half_extents, inflate),
        }?;
        if t > max_dist {
            return None;
        }
        let center = ray.at(t);
        let normal = if inflate > 0.0 {
            let surface = self.closest_point(center);
            (center - surface)
                .try_normalize()
                .unwrap_or_else(|| self.normal_at(center, -ray.direction))
        } else {
            self.normal_at(center, -ray.direction)
        };
        let point = if inflate > 0.0 {
            self.closest_point(center)
        } else {
            center
        };
        Some(ShapeHit {
            distance: t,
            point,
            normal,
        })
    }

    /// Whether a sphere at `center` with `radius` touches the shape; the contact is the shape
    /// point nearest the center and `distance` is the gap from the center to it.
    pub fn overlap_sphere(&self, center: Vec3, radius: f32) -> Option<ShapeHit> {
        let q = self.closest_point(center);
        let d = center.distance(q);
        if d > radius {
            return None;
        }
        let normal = (center - q)
            .try_normalize()
            .unwrap_or_else(|| self.normal_at(center, Vec3::Y));
        Some(ShapeHit {
            distance: d,
            point: q,
            normal,
        })
    }

    fn ray_box(&self, ray: &Ray, half: Vec3, inflate: f32) -> Option<f32> {
        let inv = self.rotation.inverse();
        let o = inv * (ray.origin - self.position);
        let d = inv * ray.direction;
        if inflate <= 0.0 {
            return ray_local_box(o, d, half);
        }
        let mut best = None;
        for grow in [Vec3::X, Vec3::Y, Vec3::Z] {
            best = min_opt(best, ray_local_box(o, d, half + grow * inflate));
        }
        // The twelve edges: along each axis, at the four (+/-, +/-) corners of the other two.
        for axis in 0..3 {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            for su in [-1.0f32, 1.0] {
                for sv in [-1.0f32, 1.0] {
                    let mut a = Vec3::ZERO;
                    a[u] = half[u] * su;
                    a[v] = half[v] * sv;
                    let mut b = a;
                    a[axis] = -half[axis];
                    b[axis] = half[axis];
                    let local_ray = Ray {
                        origin: o,
                        direction: d,
                    };
                    best = min_opt(best, ray_capsule(&local_ray, a, b, inflate));
                }
            }
        }
        best
    }
}

fn min_opt(a: Option<f32>, b: Option<f32>) -> Option<f32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn round_closest(core: Vec3, radius: f32, p: Vec3) -> Vec3 {
    let off = p - core;
    let len = off.length();
    if len <= radius {
        p
    } else {
        core + off * (radius / len)
    }
}

/// The point of segment `a..b` nearest `p`.
pub fn closest_on_segment(a: Vec3, b: Vec3, p: Vec3) -> Vec3 {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 <= PARALLEL_EPS {
        return a;
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    a + ab * t
}

/// First non-negative distance at which `ray` enters the sphere (0 when it starts inside).
fn ray_sphere(ray: &Ray, center: Vec3, radius: f32) -> Option<f32> {
    let oc = ray.origin - center;
    let c = oc.length_squared() - radius * radius;
    if c <= 0.0 {
        return Some(0.0);
    }
    let b = oc.dot(ray.direction);
    if b > 0.0 {
        return None;
    }
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    Some((-b - disc.sqrt()).max(0.0))
}

/// First non-negative distance at which `ray` enters the capsule around segment `a..b`.
fn ray_capsule(ray: &Ray, a: Vec3, b: Vec3, radius: f32) -> Option<f32> {
    if ray.origin.distance_squared(closest_on_segment(a, b, ray.origin)) <= radius * radius {
        return Some(0.0);
    }
    let mut best = min_opt(ray_sphere(ray, a, radius), ray_sphere(ray, b, radius));
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 > PARALLEL_EPS {
        // The tube: distance to the axis line equals the radius, between the caps.
        let axis = ab / len2.sqrt();
        let oa = ray.origin - a;
        let d_perp = ray.direction - axis * ray.direction.dot(axis);
        let o_perp = oa - axis * oa.dot(axis);
        let qa = d_perp.length_squared();
        if qa > PARALLEL_EPS {
            let qb = o_perp.dot(d_perp);
            let qc = o_perp.length_squared() - radius * radius;
            let disc = qb * qb - qa * qc;
            if disc >= 0.0 {
                let t = (-qb - disc.sqrt()) / qa;
                if t >= 0.0 {
                    let along = (oa + ray.direction * t).dot(axis);
                    if along >= 0.0 && along * along <= len2 {
                        best = min_opt(best, Some(t));
                    }
                }
            }
        }
    }
    best
}

/// Slab test in box space: first non-negative entry distance (0 when inside).
fn ray_local_box(o: Vec3, d: Vec3, half: Vec3) -> Option<f32> {
    let mut t_min = 0.0f32;
    let mut t_max = f32::INFINITY;
    for ((oi, di), hi) in o.to_array().into_iter().zip(d.to_array()).zip(half.to_array()) {
        if di.abs() < PARALLEL_EPS {
            if oi < -hi || oi > hi {
                return None;
            }
            continue;
        }
        let inv = 1.0 / di;
        let mut t0 = (-hi - oi) * inv;
        let mut t1 = (hi - oi) * inv;
        if t0 > t1 {
            std::mem::swap(&mut t0, &mut t1);
        }
        t_min = t_min.max(t0);
        t_max = t_max.min(t1);
        if t_min > t_max {
            return None;
        }
    }
    Some(t_min)
}

/// Blends two poses of one shape: position `lerp`, rotation sign-corrected `nlerp`. The rotation
/// normalizes in the codec's scalar order (spec 71 § 12): glam's SIMD dot4 pairs components
/// differently on SSE2 and NEON, and a rewound query must blend to the same pose on every
/// platform.
pub fn blend(a: &PosedShape, b: &PosedShape, t: f32) -> PosedShape {
    let rb = if crate::scalar::quat_dot(a.rotation, b.rotation) < 0.0 {
        -b.rotation
    } else {
        b.rotation
    };
    let q = a.rotation * (1.0 - t) + rb * t;
    // Degenerate input (a non-finite or zero-length blend): keep the unnormalized value,
    // matching the recorded-pose sites and the release notes, rather than a fallback pose.
    let rotation = crate::scalar::normalize4(q.to_array()).map_or(q, Quat::from_array);
    PosedShape {
        kind: a.kind,
        position: a.position.lerp(b.position, t),
        rotation,
    }
}

/// The union of two boxes.
pub fn union(a: &Aabb, b: &Aabb) -> Aabb {
    Aabb {
        min: a.min.min(b.min),
        max: a.max.max(b.max),
    }
}

/// `a` grown by `r` on every side.
pub fn grow(a: &Aabb, r: f32) -> Aabb {
    Aabb {
        min: a.min - Vec3::splat(r),
        max: a.max + Vec3::splat(r),
    }
}

/// Whether two boxes overlap (touching counts).
pub fn overlaps(a: &Aabb, b: &Aabb) -> bool {
    a.min.cmple(b.max).all() && b.min.cmple(a.max).all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(kind: HitShapeKind, position: Vec3) -> PosedShape {
        PosedShape {
            kind,
            position,
            rotation: Quat::IDENTITY,
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn ray_hits_sphere_front_and_reports_normal() {
        let s = at(HitShapeKind::Sphere { radius: 1.0 }, Vec3::new(0.0, 0.0, 10.0));
        let ray = Ray::new(Vec3::ZERO, Vec3::Z);
        let h = s.cast(&ray, 100.0, 0.0).expect("hit");
        assert!(close(h.distance, 9.0));
        assert!(h.normal.distance(-Vec3::Z) < 1e-4);
        assert!(s.cast(&ray, 8.5, 0.0).is_none(), "max distance");
        let miss = Ray::new(Vec3::new(1.5, 0.0, 0.0), Vec3::Z);
        assert!(s.cast(&miss, 100.0, 0.0).is_none());
        assert!(
            s.cast(&miss, 100.0, 0.6).is_some(),
            "sphere cast grows the target"
        );
        assert!(
            s.cast(&Ray::new(Vec3::ZERO, -Vec3::Z), 100.0, 0.0).is_none(),
            "behind"
        );
    }

    #[test]
    fn ray_hits_capsule_tube_and_caps() {
        let c = at(
            HitShapeKind::Capsule {
                radius: 0.5,
                half_height: 1.0,
            },
            Vec3::new(0.0, 0.0, 5.0),
        );
        // Tube.
        let h = c.cast(&Ray::new(Vec3::ZERO, Vec3::Z), 100.0, 0.0).expect("tube");
        assert!(close(h.distance, 4.5));
        // Cap: at y = 1.3 the tube is past its end, the top sphere is hit.
        let h = c
            .cast(&Ray::new(Vec3::new(0.0, 1.3, 0.0), Vec3::Z), 100.0, 0.0)
            .expect("cap");
        let expect = 5.0 - (0.25f32 - 0.09).sqrt();
        assert!(close(h.distance, expect), "{} vs {expect}", h.distance);
        // Above the cap: miss.
        assert!(
            c.cast(&Ray::new(Vec3::new(0.0, 1.6, 0.0), Vec3::Z), 100.0, 0.0)
                .is_none()
        );
        // Along the axis, from below.
        let h = c
            .cast(&Ray::new(Vec3::new(0.0, -10.0, 5.0), Vec3::Y), 100.0, 0.0)
            .expect("axis");
        assert!(close(h.distance, 8.5));
        // Rotated 90 degrees about Z: the capsule lies along X.
        let mut r = c;
        r.rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        assert!(
            r.cast(&Ray::new(Vec3::new(1.3, 0.0, 0.0), Vec3::Z), 100.0, 0.0)
                .is_some()
        );
        assert!(
            r.cast(&Ray::new(Vec3::new(0.0, 1.3, 0.0), Vec3::Z), 100.0, 0.0)
                .is_none()
        );
    }

    #[test]
    fn ray_hits_oriented_box() {
        let b = PosedShape {
            kind: HitShapeKind::Box {
                half_extents: Vec3::new(1.0, 0.5, 0.25),
            },
            position: Vec3::new(0.0, 0.0, 5.0),
            rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        };
        // Rotated about Y: the 1 m half extent now lies along Z.
        let h = b.cast(&Ray::new(Vec3::ZERO, Vec3::Z), 100.0, 0.0).expect("hit");
        assert!(close(h.distance, 4.0), "{}", h.distance);
        assert!(h.normal.distance(-Vec3::Z) < 1e-4, "{:?}", h.normal);
        assert!(
            b.cast(&Ray::new(Vec3::new(0.3, 0.0, 0.0), Vec3::Z), 100.0, 0.0)
                .is_none()
        );
        assert!(
            b.cast(&Ray::new(Vec3::new(0.2, 0.0, 0.0), Vec3::Z), 100.0, 0.0)
                .is_some()
        );
    }

    /// A sphere cast past a box corner hits the rounded corner, not the inflated square corner.
    #[test]
    fn sphere_cast_box_rounds_corners() {
        let b = at(
            HitShapeKind::Box {
                half_extents: Vec3::splat(1.0),
            },
            Vec3::new(0.0, 0.0, 5.0),
        );
        // Offset so the line passes 0.4 m outside the corner diagonally (|(1.3,1.3)-(1,1)| = 0.42).
        let ray = Ray::new(Vec3::new(1.3, 1.3, 0.0), Vec3::Z);
        assert!(b.cast(&ray, 100.0, 0.4).is_none(), "square inflation would hit");
        assert!(b.cast(&ray, 100.0, 0.45).is_some());
        // Face contact: the contact point is on the box, the normal faces the cast.
        let h = b.cast(&Ray::new(Vec3::ZERO, Vec3::Z), 100.0, 0.5).expect("face");
        assert!(close(h.distance, 3.5));
        assert!(close(h.point.z, 4.0));
        assert!(h.normal.distance(-Vec3::Z) < 1e-4);
    }

    #[test]
    fn origin_inside_is_distance_zero() {
        let s = at(HitShapeKind::Sphere { radius: 1.0 }, Vec3::ZERO);
        assert_eq!(
            s.cast(&Ray::new(Vec3::ZERO, Vec3::Z), 10.0, 0.0)
                .map(|h| h.distance),
            Some(0.0)
        );
        let c = at(
            HitShapeKind::Capsule {
                radius: 0.5,
                half_height: 1.0,
            },
            Vec3::ZERO,
        );
        assert_eq!(
            c.cast(&Ray::new(Vec3::new(0.0, 0.9, 0.0), Vec3::Z), 10.0, 0.0)
                .map(|h| h.distance),
            Some(0.0)
        );
        let b = at(
            HitShapeKind::Box {
                half_extents: Vec3::ONE,
            },
            Vec3::ZERO,
        );
        assert_eq!(
            b.cast(&Ray::new(Vec3::ZERO, Vec3::X), 10.0, 0.0)
                .map(|h| h.distance),
            Some(0.0)
        );
    }

    #[test]
    fn overlap_and_bounds() {
        let c = PosedShape {
            kind: HitShapeKind::Capsule {
                radius: 0.5,
                half_height: 1.0,
            },
            position: Vec3::ZERO,
            rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
        };
        let bb = c.aabb();
        assert!(bb.min.distance(Vec3::new(-1.5, -0.5, -0.5)) < 1e-4, "{bb:?}");
        assert!(bb.max.distance(Vec3::new(1.5, 0.5, 0.5)) < 1e-4, "{bb:?}");
        let h = c.overlap_sphere(Vec3::new(0.0, 1.0, 0.0), 0.6).expect("touch");
        assert!(close(h.distance, 0.5));
        assert!(h.normal.distance(Vec3::Y) < 1e-4);
        assert!(c.overlap_sphere(Vec3::new(0.0, 1.0, 0.0), 0.4).is_none());
    }

    #[test]
    fn blend_is_halfway_and_takes_the_short_arc() {
        let a = at(HitShapeKind::Sphere { radius: 1.0 }, Vec3::ZERO);
        let mut b = at(HitShapeKind::Sphere { radius: 1.0 }, Vec3::new(2.0, 0.0, 0.0));
        b.rotation = -Quat::from_rotation_y(0.5);
        let m = blend(&a, &b, 0.5);
        assert_eq!(m.position, Vec3::new(1.0, 0.0, 0.0));
        let angle = m.rotation.angle_between(Quat::from_rotation_y(0.25));
        assert!(angle < 1e-3, "short arc: {angle}");
    }
}
