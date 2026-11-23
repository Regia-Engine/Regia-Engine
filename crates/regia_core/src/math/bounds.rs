// SPDX-License-Identifier: MPL-2.0
//! Bounding volumes and geometric queries: [`Aabb`], [`Sphere`], [`Plane`], [`Frustum`], [`Ray`], [`Rect`].

use glam::{Affine3A, Mat3A, Mat4, Vec2, Vec3, Vec3A, Vec4};
use serde::{Deserialize, Serialize};

/// Axis-aligned bounding box in 3D. A box with `min > max` on any axis is empty.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Aabb {
    /// Minimum corner.
    pub min: Vec3,
    /// Maximum corner.
    pub max: Vec3,
}

impl Default for Aabb {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Aabb {
    /// The empty box: the identity for [`Aabb::union`] and [`Aabb::extend`].
    pub const EMPTY: Self = Self {
        min: Vec3::INFINITY,
        max: Vec3::NEG_INFINITY,
    };

    /// Creates a box from its corners (not reordered).
    #[inline]
    pub const fn new(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    /// Creates a box from its centre and half extents.
    #[inline]
    pub fn from_center_half_extents(center: Vec3, half_extents: Vec3) -> Self {
        Self::new(center - half_extents, center + half_extents)
    }

    /// The tightest box containing all `points`, or `None` if there are none.
    pub fn from_points(points: impl IntoIterator<Item = Vec3>) -> Option<Self> {
        let mut points = points.into_iter();
        let first = points.next()?;
        let mut aabb = Self::new(first, first);
        for p in points {
            aabb.extend(p);
        }
        Some(aabb)
    }

    /// `true` if the box contains no points.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.min.cmpgt(self.max).any()
    }

    /// Centre point. Halves each corner before the sum, so huge same-sign corners do not overflow
    /// (scaling by 0.5 is exact, so other boxes get the same bits). [`Aabb::EMPTY`] has no centre
    /// (all NaN); check [`Aabb::is_empty`] first.
    #[inline]
    pub fn center(&self) -> Vec3 {
        self.min * 0.5 + self.max * 0.5
    }

    /// Half the size on each axis.
    #[inline]
    pub fn half_extents(&self) -> Vec3 {
        (self.max - self.min) * 0.5
    }

    /// Size on each axis.
    #[inline]
    pub fn size(&self) -> Vec3 {
        self.max - self.min
    }

    /// Grows the box to include `point`.
    #[inline]
    pub fn extend(&mut self, point: Vec3) {
        self.min = self.min.min(point);
        self.max = self.max.max(point);
    }

    /// The smallest box containing both boxes.
    #[inline]
    pub fn union(&self, other: &Self) -> Self {
        Self::new(self.min.min(other.min), self.max.max(other.max))
    }

    /// `true` if `point` is inside or on the boundary.
    #[inline]
    pub fn contains_point(&self, point: Vec3) -> bool {
        point.cmpge(self.min).all() && point.cmple(self.max).all()
    }

    /// `true` if `other` lies entirely inside this box.
    #[inline]
    pub fn contains(&self, other: &Self) -> bool {
        // An empty box (`Aabb::EMPTY`) has inverted infinite corners that would pass the corner test.
        if self.is_empty() || other.is_empty() {
            return false;
        }
        other.min.cmpge(self.min).all() && other.max.cmple(self.max).all()
    }

    /// `true` if the boxes overlap (touching counts). An empty box overlaps nothing, including an
    /// inverted box that is not exactly [`Aabb::EMPTY`].
    #[inline]
    pub fn intersects(&self, other: &Self) -> bool {
        self.min.cmple(other.max).all()
            && other.min.cmple(self.max).all()
            && self.min.cmple(self.max).all()
            && other.min.cmple(other.max).all()
    }

    /// The box enclosing this box after `transform` (exact for the transformed box's corners; the
    /// centre/extent method from Arvo, "Transforming Axis-Aligned Bounding Boxes", Graphics Gems 1990).
    pub fn transformed(&self, transform: &Affine3A) -> Self {
        if self.is_empty() {
            return *self;
        }
        let center = transform.transform_point3a(Vec3A::from(self.center()));
        let m = transform.matrix3;
        let abs = Mat3A::from_cols(m.x_axis.abs(), m.y_axis.abs(), m.z_axis.abs());
        let half = abs * Vec3A::from(self.half_extents());
        Self::new((center - half).into(), (center + half).into())
    }
}

/// A sphere.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Sphere {
    /// Centre point.
    pub center: Vec3,
    /// Radius (≥ 0).
    pub radius: f32,
}

impl Sphere {
    /// Creates a sphere.
    #[inline]
    pub const fn new(center: Vec3, radius: f32) -> Self {
        Self { center, radius }
    }

    /// The sphere through the corners of `aabb` (centred on the box). An empty box gives the
    /// zero-radius sphere at the origin rather than a `NaN` sphere.
    pub fn from_aabb(aabb: &Aabb) -> Self {
        if aabb.is_empty() {
            return Self::default();
        }
        Self::new(aabb.center(), aabb.half_extents().length())
    }

    /// `true` if `point` is inside or on the surface.
    #[inline]
    pub fn contains_point(&self, point: Vec3) -> bool {
        self.center.distance_squared(point) <= self.radius * self.radius
    }

    /// `true` if the spheres overlap (touching counts).
    #[inline]
    pub fn intersects_sphere(&self, other: &Self) -> bool {
        let r = self.radius + other.radius;
        self.center.distance_squared(other.center) <= r * r
    }

    /// `true` if the sphere overlaps `aabb` (touching counts).
    #[inline]
    pub fn intersects_aabb(&self, aabb: &Aabb) -> bool {
        if aabb.is_empty() {
            return false;
        }
        let closest = self.center.clamp(aabb.min, aabb.max);
        self.center.distance_squared(closest) <= self.radius * self.radius
    }
}

/// A plane `dot(normal, p) + d = 0` with a unit `normal`; the positive half-space is "in front".
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Plane {
    /// Unit normal.
    pub normal: Vec3,
    /// Signed offset: `-dot(normal, point_on_plane)`.
    pub d: f32,
}

impl Plane {
    /// The plane through `point` facing `normal` (normalised here; `None` for a zero normal).
    pub fn from_point_normal(point: Vec3, normal: Vec3) -> Option<Self> {
        let normal = normal.try_normalize()?;
        Some(Self {
            normal,
            d: -normal.dot(point),
        })
    }

    /// Normalises the plane equation `(a, b, c, d)`. `None` if `(a, b, c)` is zero, not finite, or
    /// negligible next to `d` (a degenerate plane, such as the far plane of an infinite projection,
    /// whose normal is zero up to rounding). "Negligible" is a plane farther than 1e12 units from the
    /// origin, far past any `f32` world, so planes of a camera far from the origin are kept.
    pub fn from_vec4(v: Vec4) -> Option<Self> {
        let len = v.truncate().length();
        if !len.is_finite() || len == 0.0 || len <= 1e-12 * v.w.abs() {
            return None;
        }
        Some(Self {
            normal: v.truncate() / len,
            d: v.w / len,
        })
    }

    /// Signed distance from the plane to `point` (positive in front).
    #[inline]
    pub fn signed_distance(&self, point: Vec3) -> f32 {
        self.normal.dot(point) + self.d
    }

    /// The plane equation as `(a, b, c, d)`.
    #[inline]
    pub fn to_vec4(&self) -> Vec4 {
        self.normal.extend(self.d)
    }
}

/// A view frustum: up to six inward-facing planes extracted from a view-projection matrix.
///
/// Extraction follows Gribb & Hartmann, "Fast Extraction of Viewing Frustum Planes from the
/// World-View-Projection Matrix" (2001), adapted to wgpu clip space (`0 ≤ z ≤ w`). The two depth planes
/// are `z ≥ 0` and `z ≤ w`; which one is near depends on the depth convention, so standard and
/// reversed-Z matrices both work. A degenerate depth plane — the far plane of Regia Engine's reversed-Z
/// infinite projection — is dropped, leaving five planes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frustum {
    planes: [Plane; 6],
    len: usize,
}

impl Frustum {
    /// Extracts the frustum of `view_projection` (clip = `view_projection * world`).
    pub fn from_view_projection(view_projection: &Mat4) -> Self {
        let r0 = view_projection.row(0);
        let r1 = view_projection.row(1);
        let r2 = view_projection.row(2);
        let r3 = view_projection.row(3);
        let candidates = [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2];
        let mut planes = [Plane {
            normal: Vec3::ZERO,
            d: 0.0,
        }; 6];
        let mut len = 0;
        for plane in candidates.into_iter().filter_map(Plane::from_vec4) {
            planes[len] = plane;
            len += 1;
        }
        Self { planes, len }
    }

    /// `false` when no plane survived extraction (a zero or fully degenerate matrix). Such a frustum
    /// contains nothing, so a broken camera matrix culls everything instead of disabling culling.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.len > 0
    }

    /// The active planes, normals pointing into the frustum: left, right, bottom, top, then the
    /// `z ≥ 0` and `z ≤ w` depth planes (either may be missing if degenerate). A dropped plane is
    /// left out, not padded, so the order is relative: index by position only when all six survive.
    #[inline]
    pub fn planes(&self) -> &[Plane] {
        &self.planes[..self.len]
    }

    /// `true` if `point` is inside or on the boundary.
    pub fn contains_point(&self, point: Vec3) -> bool {
        self.is_valid() && self.planes().iter().all(|p| p.signed_distance(point) >= 0.0)
    }

    /// `true` if the sphere is at least partly inside. Conservative: spheres near a frustum corner may
    /// report `true` while outside.
    pub fn intersects_sphere(&self, sphere: &Sphere) -> bool {
        self.is_valid()
            && self
                .planes()
                .iter()
                .all(|p| p.signed_distance(sphere.center) >= -sphere.radius)
    }

    /// `true` if the box is at least partly inside. Conservative in the same way as
    /// [`Frustum::intersects_sphere`]; never `false` for a visible box.
    pub fn intersects_aabb(&self, aabb: &Aabb) -> bool {
        if aabb.is_empty() || !self.is_valid() {
            return false;
        }
        let center = aabb.center();
        let half = aabb.half_extents();
        self.planes()
            .iter()
            .all(|p| p.signed_distance(center) + p.normal.abs().dot(half) >= 0.0)
    }
}

/// A half-line from `origin` along a unit `direction`.
///
/// Deserializing normalizes a non-unit `direction` as [`Ray::new`] does (a unit direction keeps its
/// bits), so a ray read from data meets the intersection tests' assumption.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(from = "RayFields")]
pub struct Ray {
    /// Start point.
    pub origin: Vec3,
    /// Unit direction (zero if constructed from a zero vector; such a ray hits nothing).
    pub direction: Vec3,
}

/// The serialized shape of [`Ray`] (field names unchanged).
#[derive(Deserialize)]
#[serde(rename = "Ray")]
struct RayFields {
    origin: Vec3,
    direction: Vec3,
}

impl From<RayFields> for Ray {
    fn from(fields: RayFields) -> Self {
        // Only a direction that is not already unit length is rescaled, so data that was written
        // from a `Ray::new` direction reads back bit for bit.
        let unit = (fields.direction.length_squared() - 1.0).abs() <= 4.0 * f32::EPSILON;
        Self {
            origin: fields.origin,
            direction: if unit {
                fields.direction
            } else {
                fields.direction.normalize_or_zero()
            },
        }
    }
}

impl Ray {
    /// Creates a ray, normalising `direction`.
    #[inline]
    pub fn new(origin: Vec3, direction: Vec3) -> Self {
        Self {
            origin,
            direction: direction.normalize_or_zero(),
        }
    }

    /// The point at distance `t` along the ray.
    #[inline]
    pub fn at(&self, t: f32) -> Vec3 {
        self.origin + self.direction * t
    }

    /// Distance to the first hit with `aabb` (0 if the origin is inside), using the slab method
    /// (Kay & Kajiya, "Ray Tracing Complex Scenes", SIGGRAPH 1986).
    pub fn intersect_aabb(&self, aabb: &Aabb) -> Option<f32> {
        if aabb.is_empty() || self.direction == Vec3::ZERO {
            return None;
        }
        let mut t_near = 0.0_f32;
        let mut t_far = f32::INFINITY;
        for axis in 0..3 {
            let (o, d) = (self.origin[axis], self.direction[axis]);
            let (lo, hi) = (aabb.min[axis], aabb.max[axis]);
            let inv = d.recip();
            if d == 0.0 || !inv.is_finite() {
                // Parallel to this slab (a denormal `d` too: its reciprocal overflows, `0 * inf` would be
                // NaN on the slab plane, and the ray cannot leave the slab within f32 range): inside it
                // for all t, or never.
                if o < lo || o > hi {
                    return None;
                }
                continue;
            }
            let (t1, t2) = ((lo - o) * inv, (hi - o) * inv);
            t_near = t_near.max(t1.min(t2));
            t_far = t_far.min(t1.max(t2));
            if t_far < t_near {
                return None;
            }
        }
        Some(t_near)
    }

    /// Distance to the hit with `plane` (either side), or `None` if parallel or behind the origin.
    pub fn intersect_plane(&self, plane: &Plane) -> Option<f32> {
        let denom = plane.normal.dot(self.direction);
        if denom.abs() <= f32::EPSILON {
            return None;
        }
        let t = -plane.signed_distance(self.origin) / denom;
        (t >= 0.0).then_some(t)
    }

    /// Distance to the first hit with `sphere` (0 if the origin is inside).
    pub fn intersect_sphere(&self, sphere: &Sphere) -> Option<f32> {
        if self.direction == Vec3::ZERO {
            return None;
        }
        let oc = self.origin - sphere.center;
        let b = oc.dot(self.direction);
        let c = oc.length_squared() - sphere.radius * sphere.radius;
        let disc = b * b - c;
        if disc < 0.0 {
            return None;
        }
        let s = disc.sqrt();
        let t_far = -b + s;
        if t_far < 0.0 {
            return None;
        }
        Some((-b - s).max(0.0))
    }
}

/// Axis-aligned rectangle in 2D (UI, screen space, texture regions).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    /// Minimum corner.
    pub min: Vec2,
    /// Maximum corner.
    pub max: Vec2,
}

impl Rect {
    /// Creates a rectangle from its corners (not reordered).
    #[inline]
    pub const fn new(min: Vec2, max: Vec2) -> Self {
        Self { min, max }
    }

    /// Creates a rectangle from its minimum corner and size.
    #[inline]
    pub fn from_min_size(min: Vec2, size: Vec2) -> Self {
        Self::new(min, min + size)
    }

    /// Creates a rectangle from its centre and size.
    #[inline]
    pub fn from_center_size(center: Vec2, size: Vec2) -> Self {
        Self::new(center - size * 0.5, center + size * 0.5)
    }

    /// Width.
    #[inline]
    pub fn width(&self) -> f32 {
        self.max.x - self.min.x
    }

    /// Height.
    #[inline]
    pub fn height(&self) -> f32 {
        self.max.y - self.min.y
    }

    /// Size.
    #[inline]
    pub fn size(&self) -> Vec2 {
        self.max - self.min
    }

    /// Centre point (each corner halved before the sum, as [`Aabb::center`]).
    #[inline]
    pub fn center(&self) -> Vec2 {
        self.min * 0.5 + self.max * 0.5
    }

    /// `true` if the rectangle has no area.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.min.cmpge(self.max).any()
    }

    /// `true` if `point` is inside or on the boundary.
    #[inline]
    pub fn contains(&self, point: Vec2) -> bool {
        point.cmpge(self.min).all() && point.cmple(self.max).all()
    }

    /// `true` if the rectangles overlap with positive area.
    #[inline]
    pub fn intersects(&self, other: &Self) -> bool {
        self.min.cmplt(other.max).all() && other.min.cmplt(self.max).all()
    }

    /// The overlap of both rectangles, or `None` if they do not overlap with positive area.
    pub fn intersection(&self, other: &Self) -> Option<Self> {
        let r = Self::new(self.min.max(other.min), self.max.min(other.max));
        (!r.is_empty()).then_some(r)
    }

    /// The smallest rectangle containing both.
    #[inline]
    pub fn union(&self, other: &Self) -> Self {
        Self::new(self.min.min(other.min), self.max.max(other.max))
    }

    /// Grows (or with a negative `amount`, shrinks) every side by `amount`.
    #[inline]
    pub fn inflate(&self, amount: f32) -> Self {
        Self::new(self.min - Vec2::splat(amount), self.max + Vec2::splat(amount))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Quat;
    use glam::camera::rh::proj::directx as proj;
    use std::f32::consts::FRAC_PI_2;

    fn unit_box(center: Vec3) -> Aabb {
        Aabb::from_center_half_extents(center, Vec3::splat(0.5))
    }

    /// Regia Engine's projection: reversed-Z, infinite far plane, 90° vertical FOV, near 0.1.
    fn reversed_infinite(view: Mat4) -> Frustum {
        let proj = proj::perspective_infinite_reverse(FRAC_PI_2, 1.0, 0.1);
        Frustum::from_view_projection(&(proj * view))
    }

    #[test]
    fn aabb_basics() {
        let a = Aabb::from_points([Vec3::new(1.0, -2.0, 3.0), Vec3::new(-1.0, 4.0, 0.0), Vec3::ZERO])
            .expect("points given");
        assert_eq!(a.min, Vec3::new(-1.0, -2.0, 0.0));
        assert_eq!(a.max, Vec3::new(1.0, 4.0, 3.0));
        assert!(Aabb::from_points(std::iter::empty()).is_none());
        assert!(Aabb::EMPTY.is_empty());
        assert!(!a.is_empty());
        assert_eq!(Aabb::EMPTY.union(&a), a);
        assert!(a.contains_point(Vec3::new(0.0, 0.0, 1.5)));
        assert!(a.contains_point(a.max));
        assert!(!a.contains_point(Vec3::new(0.0, 5.0, 1.0)));
        assert!(a.intersects(&unit_box(Vec3::new(1.4, 0.0, 0.0))));
        assert!(!a.intersects(&unit_box(Vec3::new(1.6, 0.0, 0.0))));
        assert!(a.contains(&unit_box(Vec3::new(0.0, 0.0, 1.0))));
        assert!(!a.contains(&unit_box(Vec3::new(0.0, 0.0, 3.0))));
    }

    #[test]
    fn aabb_transformed_matches_transformed_corners() {
        let a = Aabb::new(Vec3::new(-1.0, 0.0, 2.0), Vec3::new(3.0, 1.0, 4.0));
        let t = Affine3A::from_scale_rotation_translation(
            Vec3::new(2.0, 1.0, 0.5),
            Quat::from_euler(glam::EulerRot::YXZ, 0.7, -0.3, 1.1),
            Vec3::new(5.0, -2.0, 1.0),
        );
        let corners = (0..8).map(|i| {
            let c = Vec3::new(
                if i & 1 == 0 { a.min.x } else { a.max.x },
                if i & 2 == 0 { a.min.y } else { a.max.y },
                if i & 4 == 0 { a.min.z } else { a.max.z },
            );
            t.transform_point3(c)
        });
        let expected = Aabb::from_points(corners).expect("eight corners");
        let got = a.transformed(&t);
        assert!(got.min.abs_diff_eq(expected.min, 1e-4), "{got:?} vs {expected:?}");
        assert!(got.max.abs_diff_eq(expected.max, 1e-4), "{got:?} vs {expected:?}");
    }

    #[test]
    fn sphere_and_plane() {
        let s = Sphere::new(Vec3::ZERO, 2.0);
        assert!(s.contains_point(Vec3::new(0.0, 2.0, 0.0)));
        assert!(!s.contains_point(Vec3::new(0.0, 2.1, 0.0)));
        assert!(s.intersects_sphere(&Sphere::new(Vec3::new(3.0, 0.0, 0.0), 1.0)));
        assert!(!s.intersects_sphere(&Sphere::new(Vec3::new(3.1, 0.0, 0.0), 1.0)));
        assert!(s.intersects_aabb(&unit_box(Vec3::new(2.4, 0.0, 0.0))));
        assert!(!s.intersects_aabb(&unit_box(Vec3::new(2.0, 2.0, 0.0))));

        let p = Plane::from_point_normal(Vec3::new(0.0, 3.0, 0.0), Vec3::new(0.0, 5.0, 0.0)).expect("normal");
        assert_eq!(p.normal, Vec3::Y);
        assert_eq!(p.signed_distance(Vec3::new(7.0, 5.0, 1.0)), 2.0);
        assert_eq!(p.signed_distance(Vec3::ZERO), -3.0);
        assert!(Plane::from_point_normal(Vec3::ZERO, Vec3::ZERO).is_none());
        let q = Plane::from_vec4(Vec4::new(0.0, 0.0, 2.0, 4.0)).expect("valid");
        assert_eq!(q.to_vec4(), Vec4::new(0.0, 0.0, 1.0, 2.0));
        assert!(Plane::from_vec4(Vec4::new(0.0, 0.0, 0.0, 0.1)).is_none());
    }

    #[test]
    fn reversed_z_infinite_frustum_drops_far_plane() {
        let f = reversed_infinite(Mat4::IDENTITY);
        assert_eq!(f.planes().len(), 5);
        // Camera at the origin looking down −Z.
        assert!(f.contains_point(Vec3::new(0.0, 0.0, -10.0)));
        assert!(f.contains_point(Vec3::new(0.0, 0.0, -1.0e6)), "no far plane");
        assert!(!f.contains_point(Vec3::new(0.0, 0.0, -0.05)), "closer than near");
        assert!(!f.contains_point(Vec3::new(0.0, 0.0, 10.0)), "behind");
        // 90° FOV: at depth 10 the half-width is 10.
        assert!(f.contains_point(Vec3::new(9.9, 0.0, -10.0)));
        assert!(!f.contains_point(Vec3::new(10.1, 0.0, -10.0)));
        assert!(!f.contains_point(Vec3::new(0.0, -10.1, -10.0)));
        // Near plane distance is 0.1.
        let near = f
            .planes()
            .iter()
            .find(|p| p.normal.z < -0.99)
            .expect("near plane");
        assert!((near.signed_distance(Vec3::ZERO) + 0.1).abs() < 1e-6);
    }

    #[test]
    fn frustum_aabb_and_sphere_culling() {
        // Camera at (0, 0, 20) looking at the origin.
        let view = glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 0.0, 20.0), Vec3::ZERO, Vec3::Y);
        let f = reversed_infinite(view);
        assert!(f.intersects_aabb(&unit_box(Vec3::ZERO)), "centred");
        assert!(
            f.intersects_aabb(&unit_box(Vec3::new(0.0, 0.0, -5000.0))),
            "very far"
        );
        assert!(
            !f.intersects_aabb(&unit_box(Vec3::new(0.0, 0.0, 25.0))),
            "behind camera"
        );
        assert!(
            !f.intersects_aabb(&unit_box(Vec3::new(-30.0, 0.0, 0.0))),
            "far left"
        );
        // Straddles the left plane (x = -20 at the origin's depth).
        assert!(f.intersects_aabb(&Aabb::new(
            Vec3::new(-25.0, -1.0, -1.0),
            Vec3::new(-19.0, 1.0, 1.0)
        )));
        assert!(
            f.intersects_aabb(&Aabb::new(Vec3::splat(-1000.0), Vec3::splat(1000.0))),
            "encloses camera"
        );
        assert!(!f.intersects_aabb(&Aabb::EMPTY));
        assert!(f.intersects_sphere(&Sphere::new(Vec3::new(-20.5, 0.0, 0.0), 1.0)));
        assert!(!f.intersects_sphere(&Sphere::new(Vec3::new(-23.0, 0.0, 0.0), 1.0)));
        assert!(!f.intersects_sphere(&Sphere::new(Vec3::new(0.0, 0.0, 22.0), 1.0)));
    }

    #[test]
    fn standard_and_reversed_finite_frusta() {
        let standard = Frustum::from_view_projection(&proj::perspective(FRAC_PI_2, 1.0, 0.1, 100.0));
        let reversed = Frustum::from_view_projection(&proj::perspective(FRAC_PI_2, 1.0, 100.0, 0.1));
        for f in [standard, reversed] {
            assert_eq!(f.planes().len(), 6);
            assert!(f.contains_point(Vec3::new(0.0, 0.0, -50.0)));
            assert!(!f.contains_point(Vec3::new(0.0, 0.0, -150.0)), "beyond far");
            assert!(!f.contains_point(Vec3::new(0.0, 0.0, -0.05)), "before near");
            assert!(!f.intersects_aabb(&unit_box(Vec3::new(0.0, 0.0, -101.0))));
            assert!(f.intersects_aabb(&unit_box(Vec3::new(0.0, 0.0, -100.2))));
        }
        let ortho = Frustum::from_view_projection(&proj::orthographic(-5.0, 5.0, -5.0, 5.0, 100.0, 0.0));
        assert!(ortho.contains_point(Vec3::new(4.0, -4.0, -50.0)));
        assert!(!ortho.contains_point(Vec3::new(6.0, 0.0, -50.0)));
    }

    #[test]
    fn ray_queries() {
        let ray = Ray::new(Vec3::new(0.0, 0.0, 10.0), Vec3::new(0.0, 0.0, -2.0));
        assert_eq!(ray.direction, Vec3::NEG_Z);
        assert_eq!(ray.at(4.0), Vec3::new(0.0, 0.0, 6.0));
        assert_eq!(ray.intersect_aabb(&unit_box(Vec3::ZERO)), Some(9.5));
        assert_eq!(ray.intersect_aabb(&unit_box(Vec3::new(2.0, 0.0, 0.0))), None);
        assert_eq!(
            ray.intersect_aabb(&unit_box(Vec3::new(0.0, 0.0, 20.0))),
            None,
            "behind"
        );
        let inside = Ray::new(Vec3::ZERO, Vec3::X);
        assert_eq!(inside.intersect_aabb(&unit_box(Vec3::ZERO)), Some(0.0));
        // Origin exactly on a slab boundary, travelling parallel to it (touching counts).
        let grazing = Ray::new(Vec3::new(-5.0, 0.5, 0.0), Vec3::X);
        assert_eq!(grazing.intersect_aabb(&unit_box(Vec3::ZERO)), Some(4.5));

        let ground = Plane::from_point_normal(Vec3::ZERO, Vec3::Y).expect("normal");
        let down = Ray::new(Vec3::new(1.0, 3.0, 0.0), Vec3::new(0.0, -1.0, 0.0));
        assert_eq!(down.intersect_plane(&ground), Some(3.0));
        assert_eq!(Ray::new(Vec3::Y, Vec3::Y).intersect_plane(&ground), None);
        assert_eq!(Ray::new(Vec3::Y, Vec3::X).intersect_plane(&ground), None);

        let s = Sphere::new(Vec3::ZERO, 1.0);
        assert_eq!(ray.intersect_sphere(&s), Some(9.0));
        assert_eq!(inside.intersect_sphere(&s), Some(0.0));
        assert_eq!(
            Ray::new(Vec3::new(0.0, 2.0, 10.0), Vec3::NEG_Z).intersect_sphere(&s),
            None
        );
        assert_eq!(
            Ray::new(Vec3::new(0.0, 0.0, 5.0), Vec3::Z).intersect_sphere(&s),
            None
        );
        assert_eq!(Ray::new(Vec3::ZERO, Vec3::ZERO).intersect_sphere(&s), None);
    }

    #[test]
    fn rect_queries() {
        let r = Rect::from_min_size(Vec2::new(10.0, 20.0), Vec2::new(100.0, 50.0));
        assert_eq!(r.width(), 100.0);
        assert_eq!(r.height(), 50.0);
        assert_eq!(r.center(), Vec2::new(60.0, 45.0));
        assert_eq!(Rect::from_center_size(r.center(), r.size()), r);
        assert!(r.contains(Vec2::new(10.0, 70.0)));
        assert!(!r.contains(Vec2::new(9.0, 30.0)));
        let other = Rect::new(Vec2::new(100.0, 60.0), Vec2::new(200.0, 200.0));
        assert!(r.intersects(&other));
        assert_eq!(
            r.intersection(&other),
            Some(Rect::new(Vec2::new(100.0, 60.0), Vec2::new(110.0, 70.0)))
        );
        let touching = Rect::new(Vec2::new(110.0, 20.0), Vec2::new(120.0, 30.0));
        assert!(!r.intersects(&touching));
        assert_eq!(r.intersection(&touching), None);
        assert_eq!(
            r.union(&other),
            Rect::new(Vec2::new(10.0, 20.0), Vec2::new(200.0, 200.0))
        );
        assert_eq!(r.inflate(5.0).size(), Vec2::new(110.0, 60.0));
    }

    #[test]
    fn empty_aabb_is_never_contained_and_has_no_nan_sphere() {
        let big = unit_box(Vec3::ZERO);
        assert!(!big.contains(&Aabb::EMPTY));
        assert!(!Aabb::EMPTY.contains(&big));
        let sphere = Sphere::from_aabb(&Aabb::EMPTY);
        assert!(sphere.center.is_finite() && sphere.radius == 0.0);
    }

    /// audit-v034b-numerics L6: `(min + max) * 0.5` overflowed for huge same-sign corners.
    #[test]
    fn centres_of_huge_boxes_do_not_overflow() {
        let a = Aabb::new(Vec3::splat(2e38), Vec3::splat(3e38));
        assert_eq!(a.center(), Vec3::splat(2.5e38));
        let r = Rect::new(Vec2::splat(-3e38), Vec2::splat(-2e38));
        assert_eq!(r.center(), Vec2::splat(-2.5e38));
        assert_eq!(crate::math::geom::Range::new(3e38_f32, 2e38).middle(), 2.5e38);
        // Ordinary boxes keep their bit-exact centre (scaling by 0.5 is exact).
        let b = Aabb::new(Vec3::new(-1.0, 0.25, 3.0), Vec3::new(2.0, 0.5, 7.0));
        assert_eq!(b.center(), Vec3::new(0.5, 0.375, 5.0));
    }

    /// audit-v034b-numerics L5: a plane more than 1e7 units from the origin was dropped as
    /// degenerate, so a camera far from the origin lost its side planes.
    #[test]
    fn far_planes_and_far_cameras_keep_their_planes() {
        let p = Plane::from_vec4(Vec4::new(1.0, 0.0, 0.0, 2e7)).expect("a plane 2e7 from the origin");
        assert_eq!(p.signed_distance(Vec3::new(-2e7, 0.0, 0.0)), 0.0);
        let view = glam::camera::rh::view::look_at_mat4(
            Vec3::new(3e7, 0.0, 0.0),
            Vec3::new(3e7, 0.0, -1.0),
            Vec3::Y,
        );
        let f = reversed_infinite(view);
        assert_eq!(f.planes().len(), 5, "four sides and the near plane");
        let eye = Vec3::new(3e7, 0.0, 0.0);
        assert!(f.contains_point(eye + Vec3::new(0.0, 0.0, -100.0)));
        assert!(
            !f.contains_point(eye + Vec3::new(1000.0, 0.0, -100.0)),
            "right of the view"
        );
        // The far plane of the infinite projection is still dropped.
        assert_eq!(reversed_infinite(Mat4::IDENTITY).planes().len(), 5);
    }

    /// audit-v034b-numerics L7: a deserialized ray kept a non-unit direction, which the sphere
    /// and plane tests assume.
    #[test]
    fn a_deserialized_ray_has_a_unit_direction() {
        let ray: Ray = ron::from_str("(origin: (0.0, 0.0, 0.0), direction: (0.0, 0.0, 5.0))").expect("ray");
        assert_eq!(ray.direction, Vec3::Z);
        let t = ray
            .intersect_sphere(&Sphere::new(Vec3::new(0.0, 0.0, 5.0), 1.0))
            .expect("hit");
        assert!((t - 4.0).abs() < 1e-5, "{t}");
        // A unit direction round-trips bit for bit.
        let unit = Ray::new(Vec3::ONE, Vec3::new(0.3, -0.4, 0.5));
        let back: Ray = ron::from_str(&ron::to_string(&unit).expect("ser")).expect("de");
        assert_eq!(back, unit);
    }

    #[test]
    fn degenerate_view_projection_culls_everything() {
        let f = Frustum::from_view_projection(&Mat4::ZERO);
        assert!(!f.is_valid());
        assert!(!f.contains_point(Vec3::ZERO));
        assert!(!f.intersects_sphere(&Sphere::new(Vec3::ZERO, 1.0)));
        assert!(!f.intersects_aabb(&unit_box(Vec3::ZERO)));
    }
}
