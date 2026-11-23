// SPDX-License-Identifier: MPL-2.0
//! Region volumes (`docs/specs/64-splines-polygons-roads.md` § 2.4).
//!
//! A [`Shape`] is a simple volume in the entity's local space that generators, biome areas,
//! exclusions and tools use as a region. It is not a collider. Queries: [`Shape::contains`],
//! [`Shape::signed_distance`] (negative inside), [`Shape::aabb`], and the world-space forms
//! [`Shape::world_signed_distance`] and [`Shape::region_value`].
//!
//! Hulls are the convex hull of their points (incremental construction, O(n²) for the at most
//! [`SHAPE_HULL_MAX_POINTS`] points). Build a [`PreparedShape`] once with [`Shape::prepare`] when
//! many queries hit the same hull; the `Shape` methods prepare per call. Provenance:
//! `docs/provenance.md` ("Shapes").

use crate::spawn::ReflectFormatVersion;
use regia_core::math::{Affine3A, Vec3};
use regia_ecs::prelude::*;

/// `format_version` of the saved [`Shape`] component (check-formats name `shape`).
pub const SHAPE_FORMAT_VERSION: u32 = 1;

/// The most points a [`ShapeKind::Hull`] may hold.
pub const SHAPE_HULL_MAX_POINTS: usize = 256;

/// The shape fingerprint of the saved `shape` component (`cargo xtask check-formats`).
pub fn shape_schema_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    fn names(info: &TypeInfo) -> String {
        match info {
            TypeInfo::Struct(s) => s.iter().map(|f| f.name()).collect::<Vec<_>>().join(","),
            TypeInfo::Enum(e) => e.iter().map(|v| v.name()).collect::<Vec<_>>().join("|"),
            _ => String::new(),
        }
    }
    format!(
        "Shape({});ShapeKind({})",
        names(Shape::type_info()),
        names(ShapeKind::type_info())
    )
}

/// `shape` migration sources (none: version 1 is the first).
pub fn shape_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The form of a [`Shape`], in local space centered on the entity.
#[derive(Reflect, Clone, Debug, PartialEq)]
#[type_path = "regia_scene"]
pub enum ShapeKind {
    /// A box.
    Box {
        /// Half extents, meters.
        half_extents: Vec3,
    },
    /// A sphere.
    Sphere {
        /// Radius, meters.
        radius: f32,
    },
    /// A capsule along local Y: a segment from `-half_height` to `+half_height` swept by `radius`.
    Capsule {
        /// Radius, meters.
        radius: f32,
        /// Half the length of the core segment (the caps add `radius` at each end).
        half_height: f32,
    },
    /// The convex hull of the points (at most [`SHAPE_HULL_MAX_POINTS`]).
    Hull {
        /// The points.
        points: Vec<Vec3>,
    },
}

impl Default for ShapeKind {
    fn default() -> Self {
        Self::Box {
            half_extents: Vec3::splat(0.5),
        }
    }
}

/// A region volume in the entity's local space (not a collider).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
#[type_path = "regia_scene"]
pub struct Shape {
    /// The form.
    pub kind: ShapeKind,
    /// Soft band outside the surface for region values, meters (0 = hard edge).
    pub edge_m: f32,
    /// Always [`SHAPE_FORMAT_VERSION`].
    pub format_version: u32,
}

impl crate::spawn::FormatVersion for Shape {
    const CURRENT: u32 = SHAPE_FORMAT_VERSION;
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            kind: ShapeKind::default(),
            edge_m: 0.0,
            format_version: SHAPE_FORMAT_VERSION,
        }
    }
}

impl Shape {
    /// A shape of a kind with no edge band.
    pub fn new(kind: ShapeKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    /// Upgrades an older saved shape (none exists yet: version 1 is the first).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        if self.format_version > SHAPE_FORMAT_VERSION {
            return Err(format!(
                "shape format_version {} is newer than {SHAPE_FORMAT_VERSION}",
                self.format_version
            ));
        }
        self.format_version = SHAPE_FORMAT_VERSION;
        Ok(())
    }

    /// The query form (hull planes built once).
    pub fn prepare(&self) -> PreparedShape {
        PreparedShape::new(self)
    }

    /// Whether a local point is inside or on the surface.
    pub fn contains(&self, p: Vec3) -> bool {
        self.signed_distance(p) <= 0.0
    }

    /// Signed distance from a local point to the surface: negative inside. Exact for box,
    /// sphere and capsule; for hulls the largest face-plane distance (exact inside, a lower
    /// bound outside).
    pub fn signed_distance(&self, p: Vec3) -> f32 {
        self.prepare().signed_distance(p)
    }

    /// World-space AABB `(min, max)` of the shape under `transform` (edge band not included).
    pub fn aabb(&self, transform: &Affine3A) -> (Vec3, Vec3) {
        self.prepare().aabb(transform)
    }

    /// Signed distance of a world point: computed in local space and scaled by the smallest axis
    /// scale of `transform` (approximate under non-uniform scale).
    pub fn world_signed_distance(&self, transform: &Affine3A, p: Vec3) -> f32 {
        self.prepare().world_signed_distance(transform, p)
    }

    /// The region value at a world point: 1 inside, falling linearly to 0 over `edge_m` outside.
    pub fn region_value(&self, transform: &Affine3A, p: Vec3) -> f32 {
        self.prepare().region_value(transform, p)
    }
}

/// A [`Shape`] ready for many queries.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedShape {
    form: Form,
    edge_m: f32,
}

#[derive(Clone, Debug, PartialEq)]
enum Form {
    Box(Vec3),
    Sphere(f32),
    Capsule(f32, f32),
    /// Outward unit normals and offsets (`n · x = d` on the plane), plus the hull points.
    Hull(Vec<(Vec3, f32)>, Vec<Vec3>),
    /// A hull with no volume: its points' bounding box (center, half extents).
    FlatHull(Vec3, Vec3),
}

impl PreparedShape {
    /// Prepares a shape (negative sizes are taken as 0; hulls beyond
    /// [`SHAPE_HULL_MAX_POINTS`] points use the first ones).
    pub fn new(shape: &Shape) -> Self {
        let form = match &shape.kind {
            ShapeKind::Box { half_extents } => Form::Box(half_extents.max(Vec3::ZERO)),
            ShapeKind::Sphere { radius } => Form::Sphere(radius.max(0.0)),
            ShapeKind::Capsule { radius, half_height } => {
                Form::Capsule(radius.max(0.0), half_height.max(0.0))
            }
            ShapeKind::Hull { points } => {
                let pts: Vec<Vec3> = points
                    .iter()
                    .copied()
                    .filter(|p| p.is_finite())
                    .take(SHAPE_HULL_MAX_POINTS)
                    .collect();
                match convex_hull_planes(&pts) {
                    Some(planes) => Form::Hull(planes, pts),
                    None => {
                        let (lo, hi) = pts.iter().fold(
                            (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
                            |(lo, hi), p| (lo.min(*p), hi.max(*p)),
                        );
                        if pts.is_empty() {
                            Form::FlatHull(Vec3::ZERO, Vec3::ZERO)
                        } else {
                            Form::FlatHull((lo + hi) * 0.5, (hi - lo) * 0.5)
                        }
                    }
                }
            }
        };
        Self {
            form,
            edge_m: shape.edge_m.max(0.0),
        }
    }

    /// See [`Shape::signed_distance`].
    pub fn signed_distance(&self, p: Vec3) -> f32 {
        match &self.form {
            Form::Box(h) => box_sd(p, *h),
            Form::FlatHull(c, h) => box_sd(p - *c, *h),
            Form::Sphere(r) => p.length() - r,
            Form::Capsule(r, hh) => {
                let y = p.y.clamp(-hh, *hh);
                (p - Vec3::new(0.0, y, 0.0)).length() - r
            }
            Form::Hull(planes, _) => planes
                .iter()
                .map(|(n, d)| n.dot(p) - d)
                .fold(f32::NEG_INFINITY, f32::max),
        }
    }

    /// See [`Shape::aabb`].
    pub fn aabb(&self, transform: &Affine3A) -> (Vec3, Vec3) {
        let corners = |c: Vec3, h: Vec3| {
            let m = transform.matrix3;
            let ext = Vec3::new(
                m.x_axis.x.abs() * h.x + m.y_axis.x.abs() * h.y + m.z_axis.x.abs() * h.z,
                m.x_axis.y.abs() * h.x + m.y_axis.y.abs() * h.y + m.z_axis.y.abs() * h.z,
                m.x_axis.z.abs() * h.x + m.y_axis.z.abs() * h.y + m.z_axis.z.abs() * h.z,
            );
            let wc = transform.transform_point3(c);
            (wc - ext, wc + ext)
        };
        match &self.form {
            Form::Box(h) => corners(Vec3::ZERO, *h),
            Form::FlatHull(c, h) => corners(*c, *h),
            // A sphere under a non-uniform scale is an ellipsoid; its local box is exact enough.
            Form::Sphere(r) => corners(Vec3::ZERO, Vec3::splat(*r)),
            Form::Capsule(r, hh) => corners(Vec3::ZERO, Vec3::new(*r, hh + r, *r)),
            Form::Hull(_, pts) => pts.iter().fold(
                (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
                |(lo, hi), p| {
                    let w = transform.transform_point3(*p);
                    (lo.min(w), hi.max(w))
                },
            ),
        }
    }

    /// See [`Shape::world_signed_distance`].
    pub fn world_signed_distance(&self, transform: &Affine3A, p: Vec3) -> f32 {
        // A singular matrix's inverse is non-finite. Do not compare the determinant with
        // `f32::EPSILON`: that product of three scales is below epsilon for a uniform scale of
        // about 0.005, so a millimetre scale (0.001, det = 1e-9) was reported as outside.
        if !p.is_finite() {
            return f32::INFINITY;
        }
        let inverse = transform.inverse();
        if !inverse.matrix3.is_finite() || !inverse.translation.is_finite() {
            return f32::INFINITY;
        }
        let local = inverse.transform_point3(p);
        if !local.is_finite() {
            return f32::INFINITY;
        }
        let m = transform.matrix3;
        let s = m.x_axis.length().min(m.y_axis.length()).min(m.z_axis.length());
        let distance = self.signed_distance(local) * s;
        if distance.is_finite() {
            distance
        } else {
            f32::INFINITY
        }
    }

    /// See [`Shape::region_value`].
    pub fn region_value(&self, transform: &Affine3A, p: Vec3) -> f32 {
        let d = self.world_signed_distance(transform, p);
        if d <= 0.0 {
            1.0
        } else if self.edge_m > 0.0 {
            (1.0 - d / self.edge_m).max(0.0)
        } else {
            0.0
        }
    }
}

fn box_sd(p: Vec3, h: Vec3) -> f32 {
    let q = p.abs() - h;
    q.max(Vec3::ZERO).length() + q.x.max(q.y).max(q.z).min(0.0)
}

/// Face planes of the convex hull of `pts` (incremental construction), or `None` when the points
/// span no volume.
fn convex_hull_planes(pts: &[Vec3]) -> Option<Vec<(Vec3, f32)>> {
    if pts.len() < 4 {
        return None;
    }
    let (lo, hi) = pts.iter().fold(
        (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
        |(lo, hi), p| (lo.min(*p), hi.max(*p)),
    );
    let eps = (hi - lo).max_element().max(1e-6) * 1e-5;
    // Initial tetrahedron: farthest point, farthest from the line, farthest from the plane.
    let i0 = 0;
    let i1 = (0..pts.len()).max_by(|&a, &b| {
        pts[a]
            .distance_squared(pts[i0])
            .total_cmp(&pts[b].distance_squared(pts[i0]))
            .then(b.cmp(&a))
    })?;
    let dir = (pts[i1] - pts[i0]).normalize_or_zero();
    if pts[i1].distance(pts[i0]) <= eps {
        return None;
    }
    let line_d = |p: Vec3| {
        let v = p - pts[i0];
        (v - dir * v.dot(dir)).length()
    };
    let i2 = (0..pts.len()).max_by(|&a, &b| line_d(pts[a]).total_cmp(&line_d(pts[b])).then(b.cmp(&a)))?;
    if line_d(pts[i2]) <= eps {
        return None;
    }
    let n0 = (pts[i1] - pts[i0]).cross(pts[i2] - pts[i0]).normalize_or_zero();
    let plane_d = |p: Vec3| (p - pts[i0]).dot(n0);
    let i3 = (0..pts.len()).max_by(|&a, &b| {
        plane_d(pts[a])
            .abs()
            .total_cmp(&plane_d(pts[b]).abs())
            .then(b.cmp(&a))
    })?;
    if plane_d(pts[i3]).abs() <= eps {
        return None;
    }
    let centroid = (pts[i0] + pts[i1] + pts[i2] + pts[i3]) * 0.25;
    let mut faces: Vec<[usize; 3]> = Vec::new();
    let add = |faces: &mut Vec<[usize; 3]>, a: usize, b: usize, c: usize| {
        let n = (pts[b] - pts[a]).cross(pts[c] - pts[a]);
        if n.dot(pts[a] - centroid) < 0.0 {
            faces.push([a, c, b]);
        } else {
            faces.push([a, b, c]);
        }
    };
    add(&mut faces, i0, i1, i2);
    add(&mut faces, i0, i1, i3);
    add(&mut faces, i0, i2, i3);
    add(&mut faces, i1, i2, i3);
    let visible = |f: &[usize; 3], p: Vec3| {
        let n = (pts[f[1]] - pts[f[0]])
            .cross(pts[f[2]] - pts[f[0]])
            .normalize_or_zero();
        n.dot(p - pts[f[0]]) > eps
    };
    for (pi, &p) in pts.iter().enumerate() {
        if [i0, i1, i2, i3].contains(&pi) {
            continue;
        }
        let vis: Vec<bool> = faces.iter().map(|f| visible(f, p)).collect();
        if !vis.iter().any(|v| *v) {
            continue;
        }
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (f, _) in faces.iter().zip(&vis).filter(|(_, v)| **v) {
            edges.extend([(f[0], f[1]), (f[1], f[2]), (f[2], f[0])]);
        }
        let horizon: Vec<(usize, usize)> = edges
            .iter()
            .copied()
            .filter(|&(a, b)| !edges.contains(&(b, a)))
            .collect();
        let mut kept: Vec<[usize; 3]> = faces
            .iter()
            .zip(&vis)
            .filter(|(_, v)| !**v)
            .map(|(f, _)| *f)
            .collect();
        for (a, b) in horizon {
            kept.push([a, b, pi]);
        }
        faces = kept;
    }
    Some(
        faces
            .iter()
            .map(|f| {
                let n = (pts[f[1]] - pts[f[0]])
                    .cross(pts[f[2]] - pts[f[0]])
                    .normalize_or_zero();
                (n, n.dot(pts[f[0]]))
            })
            .filter(|(n, _)| *n != Vec3::ZERO)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_a_unit_box() {
        let s = Shape::default();
        assert_eq!(
            s.kind,
            ShapeKind::Box {
                half_extents: Vec3::splat(0.5)
            }
        );
        assert!(s.contains(Vec3::splat(0.49)) && !s.contains(Vec3::new(0.51, 0.0, 0.0)));
        assert_eq!(s.format_version, 1);
    }

    #[test]
    fn hull_of_a_cube_matches_the_box() {
        let mut pts: Vec<Vec3> = (0..8u8)
            .map(|i| {
                Vec3::new(f32::from(i & 1), f32::from((i >> 1) & 1), f32::from((i >> 2) & 1)) * 2.0 - 1.0
            })
            .collect();
        // Interior points and a duplicate do not change the hull.
        pts.extend([Vec3::ZERO, Vec3::new(0.3, -0.2, 0.1), Vec3::ONE]);
        let hull = Shape::new(ShapeKind::Hull { points: pts }).prepare();
        let bx = Shape::new(ShapeKind::Box {
            half_extents: Vec3::ONE,
        })
        .prepare();
        for p in [
            Vec3::ZERO,
            Vec3::new(0.5, 0.2, -0.9),
            Vec3::new(3.0, 0.0, 0.0),
            Vec3::new(0.0, -2.0, 0.5),
        ] {
            assert!(
                (hull.signed_distance(p) - bx.signed_distance(p)).abs() < 1e-5,
                "{p:?}"
            );
        }
    }

    #[test]
    fn flat_hull_falls_back_to_its_box() {
        let flat = Shape::new(ShapeKind::Hull {
            points: vec![Vec3::ZERO, Vec3::X, Vec3::Z, Vec3::ONE.with_y(0.0)],
        });
        assert!(flat.contains(Vec3::new(0.5, 0.0, 0.5)));
        assert!(!flat.contains(Vec3::new(0.5, 0.1, 0.5)));
    }

    #[test]
    fn singular_transform_is_outside_not_nan() {
        let s = Shape::default();
        let t = Affine3A::from_scale(Vec3::new(0.0, 1.0, 1.0));
        let d = s.world_signed_distance(&t, Vec3::ZERO);
        assert!(d.is_infinite() && d > 0.0);
        assert_eq!(s.region_value(&t, Vec3::ZERO), 0.0);
    }

    /// A uniform scale of 0.001 (a millimetre-authored prop in a metre world) is invertible.
    /// Comparing `det` with `f32::EPSILON` rejects it: `0.001³ = 1e-9 < EPSILON`, so the query
    /// used to report every point outside.
    #[test]
    fn millimetre_scale_stays_inside() {
        let s = Shape::default();
        let t = Affine3A::from_scale(Vec3::splat(0.001));
        let d = s.world_signed_distance(&t, Vec3::ZERO);
        assert!(
            d.is_finite() && d < 0.0,
            "centre of a 0.001-scale unit box must be inside, got {d}"
        );
        // Local half-extent 0.5, scaled by 0.001.
        assert!((d - -0.0005).abs() < 1.0e-6, "got {d}");
    }

    /// A non-finite query or transform must not come back as NaN. `region_value` with a soft
    /// edge would otherwise return NaN and poison a blend weight.
    #[test]
    fn non_finite_shape_query_is_outside_not_nan() {
        let s = Shape {
            edge_m: 1.0,
            ..Shape::default()
        };
        let nan_point = s.world_signed_distance(&Affine3A::IDENTITY, Vec3::NAN);
        assert!(
            nan_point.is_infinite() && nan_point > 0.0,
            "NaN point: {nan_point}"
        );
        assert_eq!(s.region_value(&Affine3A::IDENTITY, Vec3::NAN), 0.0);
        let nan_xform = Affine3A::from_translation(Vec3::NAN);
        let nan_distance = s.world_signed_distance(&nan_xform, Vec3::ZERO);
        assert!(
            nan_distance.is_infinite() && nan_distance > 0.0,
            "NaN translation: {nan_distance}"
        );
        assert_eq!(s.region_value(&nan_xform, Vec3::ZERO), 0.0);
    }

    #[test]
    fn world_queries_scale_and_band() {
        let s = Shape {
            kind: ShapeKind::Sphere { radius: 1.0 },
            edge_m: 2.0,
            ..Shape::default()
        };
        let t = Affine3A::from_scale_rotation_translation(
            Vec3::splat(2.0),
            regia_core::math::Quat::IDENTITY,
            Vec3::new(10.0, 0.0, 0.0),
        );
        assert!((s.world_signed_distance(&t, Vec3::new(13.0, 0.0, 0.0)) - 1.0).abs() < 1e-5);
        assert!((s.region_value(&t, Vec3::new(13.0, 0.0, 0.0)) - 0.5).abs() < 1e-5);
        assert_eq!(s.region_value(&t, Vec3::new(10.0, 0.0, 0.0)), 1.0);
        let (lo, hi) = s.aabb(&t);
        assert!(
            lo.abs_diff_eq(Vec3::new(8.0, -2.0, -2.0), 1e-5)
                && hi.abs_diff_eq(Vec3::new(12.0, 2.0, 2.0), 1e-5)
        );
    }
}
