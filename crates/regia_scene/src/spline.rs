// SPDX-License-Identifier: MPL-2.0
//! General splines (`docs/specs/64-splines-polygons-roads.md` § 2).
//!
//! A [`Spline`] is a component holding control points in the entity's local space. Each segment
//! is a cubic Hermite curve; [`TangentMode`] says where its end tangents come from (`Auto` is
//! centripetal Catmull-Rom, the same curve camera paths use). Queries go through a
//! [`SplineCurve`], the evaluator built once from a spline by [`Spline::curve`] (local space) or
//! [`Spline::world_curve`] (world space): arc length, samples with frames, closest point,
//! polylines and bounds. The `Spline` methods of the same names build a curve per call, which is
//! fine for one query; hold a `SplineCurve` for many.
//!
//! - Arc length: each segment keeps a table of [`TABLE_SAMPLES`] uniform-parameter intervals with
//!   the cumulative length (4-point Gauss-Legendre per interval, accumulated in `f64`); distance →
//!   parameter inverts the table linearly and refines with two Newton steps on the true derivative.
//! - Frames: `RotationMinimizing` propagates the up vector across the table samples by the
//!   double-reflection method (Wang, Jüttler, Zheng and Liu, ACM TOG 2008); a closed spline's
//!   leftover twist is spread linearly over its length so the frame closes. Roll is applied about
//!   the tangent after the frame.
//!
//! Everything is a pure function of the control data: no clocks, no state, no randomness, and no
//! platform libm calls (angles use the series in the private `det` module), so the same spline
//! gives the same bits on every platform (spec 64 § 8). Provenance: `docs/provenance.md`
//! ("Splines").

use crate::spawn::ReflectFormatVersion;
use regia_core::math::{Affine3A, Vec2, Vec3};
use regia_ecs::prelude::*;

/// `format_version` of the saved [`Spline`] component (check-formats name `spline`).
pub const SPLINE_FORMAT_VERSION: u32 = 1;

/// The most control points a spline may hold (agent and node validation).
pub const SPLINE_MAX_POINTS: usize = 4096;

/// Uniform-parameter intervals per segment in the arc-length and frame tables.
pub const TABLE_SAMPLES: usize = 32;

/// Smallest centripetal knot interval, so coincident points never divide by zero.
const MIN_KNOT_STEP: f32 = 1.0e-4;

/// The shape fingerprint of the saved `spline` component (`cargo xtask check-formats`): field and
/// variant names from reflection, so a field edit changes it.
pub fn spline_schema_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    fn names(info: &TypeInfo) -> String {
        match info {
            TypeInfo::Struct(s) => s.iter().map(|f| f.name()).collect::<Vec<_>>().join(","),
            TypeInfo::Enum(e) => e.iter().map(|v| v.name()).collect::<Vec<_>>().join("|"),
            _ => String::new(),
        }
    }
    format!(
        "Spline({});SplinePoint({});SplineUp({});TangentMode({});SplineAttrDecl({});SplineAttrType({});SplineValue({})",
        names(Spline::type_info()),
        names(SplinePoint::type_info()),
        names(SplineUp::type_info()),
        names(TangentMode::type_info()),
        names(SplineAttrDecl::type_info()),
        names(SplineAttrType::type_info()),
        names(SplineValue::type_info()),
    )
}

/// `spline` migration sources (none: version 1 is the first).
pub fn spline_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// How the up vector of a spline's frames is chosen.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[type_path = "regia_scene"]
pub enum SplineUp {
    /// Propagated along the curve with minimal rotation from world Y at the start (the default).
    #[default]
    RotationMinimizing,
    /// World up projected off the tangent (falls back to rotation-minimizing within 1° of
    /// vertical).
    WorldY,
}

/// Where a control point's tangents come from.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[type_path = "regia_scene"]
pub enum TangentMode {
    /// Centripetal Catmull-Rom over the neighbors (α = 0.5); open ends use the one-sided
    /// difference. The default.
    #[default]
    Auto,
    /// Tangents point at the neighbors (straight segments; a corner next to a curved point).
    Linear,
    /// `tangent_in` and `tangent_out` as stored.
    Free,
    /// `tangent_out` as stored; `tangent_in` is its direction at the stored `tangent_in` length.
    Aligned,
    /// The segment starting here stays at this point (discrete attribute tracks).
    Step,
}

impl TangentMode {
    /// The agent name (`auto`, `linear`, `free`, `aligned`, `step`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Linear => "linear",
            Self::Free => "free",
            Self::Aligned => "aligned",
            Self::Step => "step",
        }
    }

    /// Parses [`TangentMode::name`] (also the capitalized variant name).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "linear" => Some(Self::Linear),
            "free" => Some(Self::Free),
            "aligned" => Some(Self::Aligned),
            "step" => Some(Self::Step),
            _ => None,
        }
    }
}

/// The type of a per-point custom attribute.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[type_path = "regia_scene"]
pub enum SplineAttrType {
    /// A number, interpolated linearly.
    #[default]
    F32,
    /// A 3-vector, interpolated linearly.
    Vec3,
    /// A name, held from the segment's start point.
    Name,
}

impl SplineAttrType {
    /// The agent name (`f32`, `vec3`, `name`).
    pub fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::Vec3 => "vec3",
            Self::Name => "name",
        }
    }

    /// Parses [`SplineAttrType::name`] (also the capitalized variant name).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "f32" => Some(Self::F32),
            "vec3" => Some(Self::Vec3),
            "name" => Some(Self::Name),
            _ => None,
        }
    }
}

/// The declaration of one per-point custom attribute.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[type_path = "regia_scene"]
pub struct SplineAttrDecl {
    /// Attribute name.
    pub name: String,
    /// Value type.
    pub ty: SplineAttrType,
}

/// One custom attribute value of a control point.
#[derive(Reflect, Clone, Debug, PartialEq)]
#[type_path = "regia_scene"]
pub enum SplineValue {
    /// A number.
    F32(f32),
    /// A 3-vector.
    Vec3(Vec3),
    /// A name.
    Name(String),
}

impl SplineValue {
    /// The value's type.
    pub fn ty(&self) -> SplineAttrType {
        match self {
            Self::F32(_) => SplineAttrType::F32,
            Self::Vec3(_) => SplineAttrType::Vec3,
            Self::Name(_) => SplineAttrType::Name,
        }
    }

    /// The default value of a type (0, zero vector, empty name).
    pub fn default_of(ty: SplineAttrType) -> Self {
        match ty {
            SplineAttrType::F32 => Self::F32(0.0),
            SplineAttrType::Vec3 => Self::Vec3(Vec3::ZERO),
            SplineAttrType::Name => Self::Name(String::new()),
        }
    }

    fn lerp(&self, other: &Self, u: f32) -> Self {
        match (self, other) {
            (Self::F32(a), Self::F32(b)) => Self::F32(a + (b - a) * u),
            (Self::Vec3(a), Self::Vec3(b)) => Self::Vec3(*a + (*b - *a) * u),
            _ => self.clone(),
        }
    }
}

/// One control point.
#[derive(Reflect, Clone, Debug, PartialEq)]
#[type_path = "regia_scene"]
#[reflect(Default)]
pub struct SplinePoint {
    /// Position in the entity's local space, meters.
    pub position: Vec3,
    /// Incoming tangent (derivative over the segment parameter), used by `Free` and `Aligned`.
    pub tangent_in: Vec3,
    /// Outgoing tangent, used by `Free` and `Aligned`.
    pub tangent_out: Vec3,
    /// Tangent mode (default `Auto`).
    pub mode: TangentMode,
    /// Roll about the tangent, degrees (positive turns up toward right).
    pub roll_deg: f32,
    /// `(width, height)` factors, default `(1, 1)`.
    pub scale: Vec2,
    /// One value per [`Spline::attrs`] entry, same order.
    pub values: Vec<SplineValue>,
}

impl Default for SplinePoint {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            tangent_in: Vec3::ZERO,
            tangent_out: Vec3::ZERO,
            mode: TangentMode::Auto,
            roll_deg: 0.0,
            scale: Vec2::ONE,
            values: Vec::new(),
        }
    }
}

impl SplinePoint {
    /// An `Auto` point at a position.
    pub fn at(position: Vec3) -> Self {
        Self {
            position,
            ..Self::default()
        }
    }

    /// A `Free` point with explicit tangents.
    pub fn free(position: Vec3, tangent_in: Vec3, tangent_out: Vec3) -> Self {
        Self {
            position,
            tangent_in,
            tangent_out,
            mode: TangentMode::Free,
            ..Self::default()
        }
    }

    /// The same point with another mode.
    #[must_use]
    pub fn with_mode(mut self, mode: TangentMode) -> Self {
        self.mode = mode;
        self
    }
}

/// A piecewise cubic curve in the entity's local space (spec 64 § 2.1).
///
/// Saved in scenes with `format_version` 1 under the pinned type path `regia_scene::Spline`.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
#[type_path = "regia_scene"]
pub struct Spline {
    /// Control points.
    pub points: Vec<SplinePoint>,
    /// Whether the last point connects back to the first.
    pub closed: bool,
    /// How frames choose their up vector.
    pub up: SplineUp,
    /// Per-point custom attribute declarations.
    pub attrs: Vec<SplineAttrDecl>,
    /// Always [`SPLINE_FORMAT_VERSION`].
    pub format_version: u32,
}

impl crate::spawn::FormatVersion for Spline {
    const CURRENT: u32 = SPLINE_FORMAT_VERSION;
}

impl Default for Spline {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            closed: false,
            up: SplineUp::RotationMinimizing,
            attrs: Vec::new(),
            format_version: SPLINE_FORMAT_VERSION,
        }
    }
}

/// A spline that fails validation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SplineError {
    /// More than [`SPLINE_MAX_POINTS`] points (`spline_too_many_points`).
    #[error("spline has {count} points, the limit is {SPLINE_MAX_POINTS}")]
    TooManyPoints {
        /// The point count.
        count: usize,
    },
    /// A non-finite number in a point.
    #[error("spline point {index} holds a non-finite number")]
    NonFinite {
        /// The point index.
        index: usize,
    },
    /// A point whose values do not match the attribute declarations.
    #[error("spline point {index} has values that do not match the attribute declarations")]
    Values {
        /// The point index.
        index: usize,
    },
    /// A `format_version` newer than this build reads.
    #[error("spline format_version {0} is newer than {SPLINE_FORMAT_VERSION}")]
    Version(u32),
}

impl SplineError {
    /// The agent `data.reason` code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooManyPoints { .. } => "spline_too_many_points",
            Self::NonFinite { .. } | Self::Values { .. } => "spline_invalid",
            Self::Version(_) => "spline_version",
        }
    }
}

impl Spline {
    /// An open spline of `Auto` points through positions.
    pub fn through(positions: impl IntoIterator<Item = Vec3>) -> Self {
        Self {
            points: positions.into_iter().map(SplinePoint::at).collect(),
            ..Self::default()
        }
    }

    /// The same spline, closed.
    #[must_use]
    pub fn closed(mut self) -> Self {
        self.closed = true;
        self
    }

    /// Upgrades an older saved shape (none exists yet: version 1 is the first).
    ///
    /// # Errors
    ///
    /// [`SplineError::Version`] for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), SplineError> {
        if self.format_version > SPLINE_FORMAT_VERSION {
            return Err(SplineError::Version(self.format_version));
        }
        self.format_version = SPLINE_FORMAT_VERSION;
        Ok(())
    }

    /// Checks the point count, finite numbers and the value lists against `attrs`.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), SplineError> {
        if self.points.len() > SPLINE_MAX_POINTS {
            return Err(SplineError::TooManyPoints {
                count: self.points.len(),
            });
        }
        for (index, p) in self.points.iter().enumerate() {
            let finite = p.position.is_finite()
                && p.tangent_in.is_finite()
                && p.tangent_out.is_finite()
                && p.roll_deg.is_finite()
                && p.scale.is_finite()
                && p.values.iter().all(|v| match v {
                    SplineValue::F32(x) => x.is_finite(),
                    SplineValue::Vec3(x) => x.is_finite(),
                    SplineValue::Name(_) => true,
                });
            if !finite {
                return Err(SplineError::NonFinite { index });
            }
            if p.values.len() != self.attrs.len()
                || p.values.iter().zip(&self.attrs).any(|(v, d)| v.ty() != d.ty)
            {
                return Err(SplineError::Values { index });
            }
        }
        Ok(())
    }

    /// The index of a custom attribute.
    pub fn attr_index(&self, name: &str) -> Option<usize> {
        self.attrs.iter().position(|a| a.name == name)
    }

    /// Number of segments: points − 1 when open, points when closed (0 below 2 points).
    pub fn segment_count(&self) -> usize {
        segment_count(self.points.len(), self.closed)
    }

    /// The evaluator in local space.
    pub fn curve(&self) -> SplineCurve {
        SplineCurve::new(self, None)
    }

    /// The evaluator in world space: positions through the full transform, tangents through its
    /// linear part (non-uniform scale included), arc length measured in world space.
    pub fn world_curve(&self, transform: &Affine3A) -> SplineCurve {
        SplineCurve::new(self, Some(transform))
    }

    /// Total length, meters (builds a [`SplineCurve`]).
    pub fn length(&self) -> f32 {
        self.curve().length()
    }

    /// Length of segment `i` (0 out of range).
    pub fn segment_length(&self, i: usize) -> f32 {
        self.curve().segment_length(i)
    }

    /// The sample at a distance along the curve.
    pub fn at_distance(&self, d: f32) -> SplineSample {
        self.curve().at_distance(d)
    }

    /// The sample at `t` ∈ [0, 1] over the whole spline by length.
    pub fn at_param(&self, t: f32) -> SplineSample {
        self.curve().at_param(t)
    }

    /// The sample at key `k` (segment index + local parameter).
    pub fn at_key(&self, k: f32) -> SplineSample {
        self.curve().at_key(k)
    }

    /// The closest point on the curve to `p`.
    pub fn closest(&self, p: Vec3) -> ClosestHit {
        self.curve().closest(p)
    }

    /// A polyline within `max_error_m` of the curve.
    pub fn to_polyline(&self, max_error_m: f32) -> Vec<Vec3> {
        self.curve().to_polyline(max_error_m)
    }

    /// An axis-aligned box `(min, max)` containing the curve.
    pub fn bounds(&self) -> (Vec3, Vec3) {
        self.curve().bounds()
    }
}

fn segment_count(points: usize, closed: bool) -> usize {
    match (points, closed) {
        (0 | 1, _) => 0,
        (n, true) => n,
        (n, false) => n - 1,
    }
}

/// One evaluated point of a spline.
#[derive(Clone, Debug, PartialEq)]
pub struct SplineSample {
    /// Position.
    pub position: Vec3,
    /// Unit tangent (direction of travel).
    pub tangent: Vec3,
    /// Unit up vector of the frame (after roll).
    pub up: Vec3,
    /// Unit right vector: `tangent × up`.
    pub right: Vec3,
    /// Interpolated roll, degrees.
    pub roll_deg: f32,
    /// Interpolated `(width, height)` factors.
    pub scale: Vec2,
    /// Curvature, 1/m.
    pub curvature: f32,
    /// Distance from the start along the curve, meters.
    pub along_m: f32,
    /// Key: segment index + local parameter.
    pub key: f32,
    /// Custom values, one per attribute declaration.
    pub values: Vec<SplineValue>,
}

/// The result of [`SplineCurve::closest`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClosestHit {
    /// Key of the closest point.
    pub key: f32,
    /// Distance from the query to the curve, meters.
    pub distance_m: f32,
    /// Distance of the closest point along the curve, meters.
    pub along_m: f32,
    /// The closest point.
    pub position: Vec3,
}

/// Centripetal knot interval (`|b − a|^0.5`, by `sqrt`).
pub fn centripetal_knot_step(a: Vec3, b: Vec3) -> f32 {
    a.distance(b).sqrt().max(MIN_KNOT_STEP)
}

/// The point at parameter `u` (`0..=1`) of the Catmull-Rom segment from `p1` to `p2` with
/// neighbors `p0` and `p3`, on the knot sequence `knot_step` builds (Barry-Goldman pyramid).
/// `u = 0` returns `p1` and `u = 1` returns `p2` exactly. Camera paths
/// (`regia_framework::shots::spline`) call it with their own centripetal (`sqrt`) knot step.
#[must_use]
pub fn catmull_rom_segment_with(
    p0: Vec3,
    p1: Vec3,
    p2: Vec3,
    p3: Vec3,
    u: f32,
    knot_step: impl Fn(Vec3, Vec3) -> f32,
) -> Vec3 {
    if u <= 0.0 {
        return p1;
    }
    if u >= 1.0 {
        return p2;
    }
    let t0 = 0.0;
    let t1 = t0 + knot_step(p0, p1);
    let t2 = t1 + knot_step(p1, p2);
    let t3 = t2 + knot_step(p2, p3);
    let t = t1 + (t2 - t1) * u;

    let a1 = p0 * ((t1 - t) / (t1 - t0)) + p1 * ((t - t0) / (t1 - t0));
    let a2 = p1 * ((t2 - t) / (t2 - t1)) + p2 * ((t - t1) / (t2 - t1));
    let a3 = p2 * ((t3 - t) / (t3 - t2)) + p3 * ((t - t2) / (t3 - t2));
    let b1 = a1 * ((t2 - t) / (t2 - t0)) + a2 * ((t - t0) / (t2 - t0));
    let b2 = a2 * ((t3 - t) / (t3 - t1)) + a3 * ((t - t1) / (t3 - t1));
    b1 * ((t2 - t) / (t2 - t1)) + b2 * ((t - t1) / (t2 - t1))
}

/// The point on the open Catmull-Rom curve through `points` at segment `index` and parameter
/// `u`, phantom end points mirrored (`P_-1 = 2 P_0 − P_1`). Needs at least two points and
/// `index + 1 < points.len()`; anything else returns the nearest point (the origin for none).
#[must_use]
pub fn catmull_rom_with(
    points: &[Vec3],
    index: usize,
    u: f32,
    knot_step: impl Fn(Vec3, Vec3) -> f32,
) -> Vec3 {
    let n = points.len();
    if n == 0 {
        return Vec3::ZERO;
    }
    if n == 1 || index + 1 >= n {
        return points[index.min(n - 1)];
    }
    let p1 = points[index];
    let p2 = points[index + 1];
    let p0 = if index == 0 {
        p1 * 2.0 - p2
    } else {
        points[index - 1]
    };
    let p3 = if index + 2 < n {
        points[index + 2]
    } else {
        p2 * 2.0 - p1
    };
    catmull_rom_segment_with(p0, p1, p2, p3, u, knot_step)
}

/// Small `f64` vector for the evaluator's accumulation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct D3([f64; 3]);

impl D3 {
    fn from_v(v: Vec3) -> Self {
        Self([f64::from(v.x), f64::from(v.y), f64::from(v.z)])
    }
    fn v(self) -> Vec3 {
        Vec3::new(self.0[0] as f32, self.0[1] as f32, self.0[2] as f32)
    }
    fn dot(self, o: Self) -> f64 {
        self.0[0] * o.0[0] + self.0[1] * o.0[1] + self.0[2] * o.0[2]
    }
    fn cross(self, o: Self) -> Self {
        let (a, b) = (self.0, o.0);
        Self([
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ])
    }
    fn len(self) -> f64 {
        self.dot(self).sqrt()
    }
    fn norm_or(self, fallback: Self) -> Self {
        let l = self.len();
        if l > 1.0e-12 && l.is_finite() {
            self * (1.0 / l)
        } else {
            fallback
        }
    }
}

impl std::ops::Add for D3 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self([self.0[0] + o.0[0], self.0[1] + o.0[1], self.0[2] + o.0[2]])
    }
}

impl std::ops::Sub for D3 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self([self.0[0] - o.0[0], self.0[1] - o.0[1], self.0[2] - o.0[2]])
    }
}

impl std::ops::Mul<f64> for D3 {
    type Output = Self;
    fn mul(self, s: f64) -> Self {
        Self([self.0[0] * s, self.0[1] * s, self.0[2] * s])
    }
}

const Y: D3 = D3([0.0, 1.0, 0.0]);
const Z: D3 = D3([0.0, 0.0, 1.0]);

/// One Hermite segment with its tables.
#[derive(Clone, Debug)]
struct Seg {
    p0: D3,
    m0: D3,
    p1: D3,
    m1: D3,
    /// Constant at `p0` (`Step` start point).
    step: bool,
    /// Index of the start and end control points.
    a: usize,
    b: usize,
    /// Cumulative length at each of the `TABLE_SAMPLES + 1` table parameters.
    table: [f64; TABLE_SAMPLES + 1],
    /// Distance of the segment start along the curve.
    start: f64,
}

impl Seg {
    fn pos(&self, u: f64) -> D3 {
        if self.step {
            return self.p0;
        }
        let (u2, u3) = (u * u, u * u * u);
        self.p0 * (2.0 * u3 - 3.0 * u2 + 1.0)
            + self.m0 * (u3 - 2.0 * u2 + u)
            + self.p1 * (-2.0 * u3 + 3.0 * u2)
            + self.m1 * (u3 - u2)
    }
    fn d1(&self, u: f64) -> D3 {
        if self.step {
            return D3::default();
        }
        let u2 = u * u;
        self.p0 * (6.0 * u2 - 6.0 * u)
            + self.m0 * (3.0 * u2 - 4.0 * u + 1.0)
            + self.p1 * (-6.0 * u2 + 6.0 * u)
            + self.m1 * (3.0 * u2 - 2.0 * u)
    }
    fn d2(&self, u: f64) -> D3 {
        if self.step {
            return D3::default();
        }
        self.p0 * (12.0 * u - 6.0)
            + self.m0 * (6.0 * u - 4.0)
            + self.p1 * (-12.0 * u + 6.0)
            + self.m1 * (6.0 * u - 2.0)
    }
    fn speed(&self, u: f64) -> f64 {
        self.d1(u).len()
    }
    /// Arc length over `[u0, u1]` by 4-point Gauss-Legendre.
    fn arc(&self, u0: f64, u1: f64) -> f64 {
        const X: [f64; 2] = [0.339_981_043_584_856_3, 0.861_136_311_594_052_6];
        const W: [f64; 2] = [0.652_145_154_862_546_1, 0.347_854_845_137_453_9];
        let (c, h) = ((u0 + u1) * 0.5, (u1 - u0) * 0.5);
        let mut s = 0.0;
        for (x, w) in X.iter().zip(W) {
            s += w * (self.speed(c - h * x) + self.speed(c + h * x));
        }
        s * h
    }
    fn length(&self) -> f64 {
        self.table[TABLE_SAMPLES]
    }
    /// The cumulative length within the segment at `u`.
    fn s_at(&self, u: f64) -> f64 {
        let n = TABLE_SAMPLES as f64;
        let j = ((u * n).floor() as usize).min(TABLE_SAMPLES - 1);
        let uj = j as f64 / n;
        self.table[j] + self.arc(uj, u)
    }
    /// The parameter at a length within the segment.
    fn u_at(&self, s: f64) -> f64 {
        let len = self.length();
        if len <= 0.0 || s <= 0.0 {
            return 0.0;
        }
        if s >= len {
            return 1.0;
        }
        // First table entry at or beyond `s` (the table is non-decreasing).
        let j = self.table.partition_point(|&x| x < s).clamp(1, TABLE_SAMPLES);
        let (s0, s1) = (self.table[j - 1], self.table[j]);
        let n = TABLE_SAMPLES as f64;
        let (lo, hi) = ((j - 1) as f64 / n, j as f64 / n);
        let mut u = if s1 > s0 {
            lo + (hi - lo) * (s - s0) / (s1 - s0)
        } else {
            lo
        };
        for _ in 0..2 {
            let sp = self.speed(u);
            if sp <= 1.0e-12 {
                break;
            }
            u = (u - (self.s_at(u) - s) / sp).clamp(lo, hi);
        }
        u
    }
}

/// The evaluator of a [`Spline`]: segments with arc-length tables and frames.
#[derive(Clone, Debug)]
pub struct SplineCurve {
    segs: Vec<Seg>,
    /// Roll, scale and values per control point.
    rolls: Vec<f32>,
    scales: Vec<Vec2>,
    values: Vec<Vec<SplineValue>>,
    /// The single point of a spline with fewer than two points.
    lone: Vec3,
    length: f64,
    up_mode: SplineUp,
    attrs: Vec<SplineAttrDecl>,
    closed: bool,
    /// Propagated rotation-minimizing up per table sample, `TABLE_SAMPLES + 1` per segment.
    frames: Vec<D3>,
    /// Twist (radians) spread over a closed spline so its frame closes.
    twist: f64,
    /// Per segment, the axis-aligned box `[min, max]` of its `TABLE_SAMPLES + 1` table sample
    /// positions (`closest` skips a segment whose box is out of reach; ADR-7238).
    boxes: Vec<[D3; 2]>,
    /// Longest step between consecutive table samples of one segment, over the whole curve.
    max_step: f64,
}

impl SplineCurve {
    /// Builds the evaluator; `transform` maps local to world space when given.
    pub fn new(spline: &Spline, transform: Option<&Affine3A>) -> Self {
        let pts: Vec<D3> = spline
            .points
            .iter()
            .map(|p| D3::from_v(transform.map_or(p.position, |t| t.transform_point3(p.position))))
            .collect();
        let tv = |v: Vec3| D3::from_v(transform.map_or(v, |t| t.transform_vector3(v)));
        let n = pts.len();
        let closed = spline.closed;
        let seg_n = segment_count(n, closed);
        let knot = |a: D3, b: D3| ((b - a).len().sqrt()).max(f64::from(MIN_KNOT_STEP));
        // Centripetal velocity (per knot unit) at point `i`, or `None` at an open end.
        let velocity = |i: usize| -> D3 {
            let (prev, next) = if closed {
                ((i + n - 1) % n, (i + 1) % n)
            } else if i == 0 {
                // One-sided difference: the mirrored phantom point gives exactly this.
                let dt = knot(pts[0], pts[1]);
                return (pts[1] - pts[0]) * (1.0 / dt);
            } else if i == n - 1 {
                let dt = knot(pts[n - 2], pts[n - 1]);
                return (pts[n - 1] - pts[n - 2]) * (1.0 / dt);
            } else {
                (i - 1, i + 1)
            };
            let (pm, p, pn) = (pts[prev], pts[i], pts[next]);
            let (d0, d1) = (knot(pm, p), knot(p, pn));
            (p - pm) * (1.0 / d0) - (pn - pm) * (1.0 / (d0 + d1)) + (pn - p) * (1.0 / d1)
        };
        let mut segs = Vec::with_capacity(seg_n);
        let mut start = 0.0f64;
        for i in 0..seg_n {
            let (a, b) = (i, (i + 1) % n);
            let (pa, pb) = (&spline.points[a], &spline.points[b]);
            let (p0, p1) = (pts[a], pts[b]);
            let dt = knot(p0, p1);
            let m0 = match pa.mode {
                TangentMode::Auto => velocity(a) * dt,
                TangentMode::Linear | TangentMode::Step => p1 - p0,
                TangentMode::Free | TangentMode::Aligned => tv(pa.tangent_out),
            };
            let m1 = match pb.mode {
                TangentMode::Auto => velocity(b) * dt,
                TangentMode::Linear | TangentMode::Step => p1 - p0,
                TangentMode::Free => tv(pb.tangent_in),
                TangentMode::Aligned => {
                    let out = tv(pb.tangent_out);
                    out.norm_or(D3::default()) * tv(pb.tangent_in).len()
                }
            };
            let mut seg = Seg {
                p0,
                m0,
                p1,
                m1,
                step: pa.mode == TangentMode::Step,
                a,
                b,
                table: [0.0; TABLE_SAMPLES + 1],
                start,
            };
            let nf = TABLE_SAMPLES as f64;
            for j in 0..TABLE_SAMPLES {
                let s = seg.table[j] + seg.arc(j as f64 / nf, (j + 1) as f64 / nf);
                seg.table[j + 1] = s;
            }
            start += seg.length();
            segs.push(seg);
        }
        let mut curve = Self {
            segs,
            rolls: spline.points.iter().map(|p| p.roll_deg).collect(),
            scales: spline.points.iter().map(|p| p.scale).collect(),
            values: spline.points.iter().map(|p| p.values.clone()).collect(),
            lone: pts.first().map_or(Vec3::ZERO, |p| p.v()),
            length: start,
            up_mode: spline.up,
            attrs: spline.attrs.clone(),
            closed,
            frames: Vec::new(),
            twist: 0.0,
            boxes: Vec::new(),
            max_step: 0.0,
        };
        curve.build_frames(closed);
        curve
    }

    /// The unit tangent at `(segment, u)`, falling back to the chord, then to +X.
    fn tangent(&self, seg: &Seg, u: f64) -> D3 {
        let d = seg.d1(u);
        if d.len() > 1.0e-9 {
            return d.norm_or(D3([1.0, 0.0, 0.0]));
        }
        // A cusp or a zero-length segment: nudge inward, then the chord.
        let du = if u < 0.5 { 1.0e-4 } else { -1.0e-4 };
        let d = seg.d1(u + du);
        if d.len() > 1.0e-9 {
            return d.norm_or(D3([1.0, 0.0, 0.0]));
        }
        (seg.p1 - seg.p0).norm_or(D3([1.0, 0.0, 0.0]))
    }

    fn build_frames(&mut self, closed: bool) {
        if self.segs.is_empty() {
            return;
        }
        let nf = TABLE_SAMPLES as f64;
        let mut frames = Vec::with_capacity(self.segs.len() * (TABLE_SAMPLES + 1));
        let t0 = self.tangent(&self.segs[0], 0.0);
        let mut up = initial_up(t0);
        let mut prev_x = self.segs[0].pos(0.0);
        let mut prev_t = t0;
        let mut boxes = Vec::with_capacity(self.segs.len());
        let mut max_step = 0.0f64;
        for seg in &self.segs {
            let mut lo = D3([f64::INFINITY; 3]);
            let mut hi = D3([f64::NEG_INFINITY; 3]);
            for j in 0..=TABLE_SAMPLES {
                let u = j as f64 / nf;
                let x = seg.pos(u);
                // The table samples `closest` scans: their box and the step between them.
                for k in 0..3 {
                    lo.0[k] = lo.0[k].min(x.0[k]);
                    hi.0[k] = hi.0[k].max(x.0[k]);
                }
                if j > 0 {
                    max_step = max_step.max((x - prev_x).len());
                }
                let t = self.tangent(seg, u);
                up = double_reflect(prev_x, prev_t, up, x, t);
                frames.push(up);
                prev_x = x;
                prev_t = t;
            }
            boxes.push([lo, hi]);
        }
        self.boxes = boxes;
        self.max_step = max_step;
        if closed && self.length > 0.0 {
            // Twist from the propagated end frame back to the start frame, about the tangent.
            let end_up = up;
            let start_up = frames[0];
            let axis = prev_t;
            let s = end_up.cross(start_up).dot(axis);
            let c = end_up.dot(start_up);
            self.twist = det::atan2(s, c);
        }
        self.frames = frames;
    }

    /// Whether the spline is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The segment and its parameter at distance `d` (clamped; a boundary belongs to the next
    /// segment).
    pub fn locate_distance(&self, d: f64) -> (usize, f64) {
        if self.segs.is_empty() {
            return (0, 0.0);
        }
        let (i, s) = self.locate(if d.is_nan() { 0.0 } else { d });
        (i, self.segs[i].u_at(s))
    }

    /// The Hermite form `[p0, m0, p1, m1]` of segment `i` (`None` out of range).
    pub fn segment_hermite(&self, i: usize) -> Option<[Vec3; 4]> {
        self.segs.get(i).map(|s| [s.p0.v(), s.m0.v(), s.p1.v(), s.m1.v()])
    }

    /// The control point of the curve at `(i, u)` with `tangent_in`/`tangent_out` for sub-segments
    /// of parameter widths `w_in` and `w_out`.
    fn cut_point(&self, i: usize, u: f64, w_in: f64, w_out: f64) -> SplinePoint {
        let seg = &self.segs[i];
        let uf = u as f32;
        let d = seg.d1(u);
        SplinePoint {
            position: seg.pos(u).v(),
            tangent_in: (d * w_in).v(),
            tangent_out: (d * w_out).v(),
            mode: if seg.step {
                TangentMode::Step
            } else {
                TangentMode::Free
            },
            roll_deg: self.rolls[seg.a] + (self.rolls[seg.b] - self.rolls[seg.a]) * uf,
            scale: self.scales[seg.a] + (self.scales[seg.b] - self.scales[seg.a]) * uf,
            values: self.values[seg.a]
                .iter()
                .zip(&self.values[seg.b])
                .map(|(a, b)| a.lerp(b, uf))
                .collect(),
        }
    }

    /// The open sub-spline between distances `d0 < d1`: an exact copy of that range of the curve
    /// with `Free` control points (tangents preserved at the cuts and at every interior control
    /// point). Empty when the range has no length.
    pub fn range(&self, d0: f64, d1: f64) -> Spline {
        let mut out = Spline {
            up: self.up_mode,
            attrs: self.attrs.clone(),
            ..Spline::default()
        };
        if self.segs.is_empty() || d1.partial_cmp(&d0) != Some(std::cmp::Ordering::Greater) {
            return out;
        }
        let (i0, u0) = self.locate_distance(d0);
        let (mut i1, mut u1) = self.locate_distance(d1);
        if u1 <= 0.0 && i1 > i0 {
            i1 -= 1;
            u1 = 1.0;
        }
        let mut prev_w = 0.0f64;
        for i in i0..=i1 {
            let ua = if i == i0 { u0 } else { 0.0 };
            let ub = if i == i1 { u1 } else { 1.0 };
            let w = ub - ua;
            if w <= 0.0 {
                continue;
            }
            let start = self.cut_point(i, ua, if out.points.is_empty() { w } else { prev_w }, w);
            if let Some(last) = out.points.last_mut() {
                // The previous sub-segment ended here: keep its incoming tangent.
                last.tangent_out = start.tangent_out;
                last.mode = start.mode;
            } else {
                out.points.push(start);
            }
            out.points.push(self.cut_point(i, ub, w, w));
            prev_w = w;
        }
        out
    }

    /// Pieces between sorted cut distances (cuts outside `(0, length)` are ignored). A closed
    /// spline with cuts joins its last and first piece across the seam.
    pub fn split_at(&self, cuts: &[f64]) -> Vec<Spline> {
        let mut c: Vec<f64> = cuts
            .iter()
            .copied()
            .filter(|d| d.is_finite() && *d > 0.0 && *d < self.length)
            .collect();
        c.sort_by(f64::total_cmp);
        c.dedup();
        if self.closed && !c.is_empty() {
            let first = c[0];
            let mut pieces: Vec<Spline> = c.windows(2).map(|w| self.range(w[0], w[1])).collect();
            let mut seam = self.range(c[c.len() - 1], self.length);
            let head = self.range(0.0, first);
            join(&mut seam, head);
            pieces.push(seam);
            return pieces.into_iter().filter(|p| p.points.len() >= 2).collect();
        }
        let mut bounds = vec![0.0];
        bounds.extend(c);
        bounds.push(self.length);
        bounds
            .windows(2)
            .map(|w| self.range(w[0], w[1]))
            .filter(|p| p.points.len() >= 2)
            .collect()
    }

    /// The whole spline with extra control points at the cut distances; its shape is unchanged
    /// (every point becomes `Free`, `Step` points stay `Step`).
    pub fn with_points_at(&self, cuts: &[f64]) -> Spline {
        let mut c: Vec<f64> = cuts
            .iter()
            .copied()
            .filter(|d| d.is_finite() && *d > 0.0 && *d < self.length)
            .collect();
        c.sort_by(f64::total_cmp);
        c.dedup();
        let mut bounds = vec![0.0];
        bounds.extend(c);
        bounds.push(self.length);
        let mut out = self.range(0.0, 0.0);
        for w in bounds.windows(2) {
            join(&mut out, self.range(w[0], w[1]));
        }
        if self.closed && out.points.len() > 2 {
            // The last point repeats the first: fold its incoming tangent into the first.
            if let Some(last) = out.points.pop()
                && let Some(first) = out.points.first_mut()
            {
                first.tangent_in = last.tangent_in;
            }
            out.closed = true;
        }
        out
    }

    /// Number of segments.
    pub fn segment_count(&self) -> usize {
        self.segs.len()
    }

    /// Total length, meters.
    pub fn length(&self) -> f32 {
        self.length as f32
    }

    /// Total length in `f64`.
    pub fn length_f64(&self) -> f64 {
        self.length
    }

    /// Length of segment `i` (0 out of range).
    pub fn segment_length(&self, i: usize) -> f32 {
        self.segs.get(i).map_or(0.0, |s| s.length() as f32)
    }

    /// Distance along the curve at the start of each segment.
    pub fn segment_starts(&self) -> Vec<f32> {
        self.segs.iter().map(|s| s.start as f32).collect()
    }

    /// The segment containing distance `d` (clamped), and the distance within it.
    fn locate(&self, d: f64) -> (usize, f64) {
        // `max`/`min` (not `clamp`): a NaN length from non-finite control points must not abort.
        let d = d.max(0.0).min(self.length);
        // The last segment that starts at or before `d` (a boundary belongs to the next segment).
        let i = self.segs.partition_point(|s| s.start <= d).saturating_sub(1);
        (i, d - self.segs[i].start)
    }

    /// The sample at a distance along the curve, clamped to `[0, length]`.
    pub fn at_distance(&self, d: f32) -> SplineSample {
        self.at_distance_f64(f64::from(d))
    }

    /// [`SplineCurve::at_distance`] with an `f64` distance.
    pub fn at_distance_f64(&self, d: f64) -> SplineSample {
        if self.segs.is_empty() {
            return self.lone_sample();
        }
        let d = if d.is_nan() { 0.0 } else { d };
        let (i, s) = self.locate(d);
        let u = self.segs[i].u_at(s);
        self.sample(i, u)
    }

    /// The sample at `t` ∈ [0, 1] over the whole spline by length.
    pub fn at_param(&self, t: f32) -> SplineSample {
        self.at_distance_f64(f64::from(t.clamp(0.0, 1.0)) * self.length)
    }

    /// The sample at key `k` (segment index + local parameter), clamped to the spline.
    pub fn at_key(&self, k: f32) -> SplineSample {
        if self.segs.is_empty() {
            return self.lone_sample();
        }
        let k = if k.is_nan() {
            0.0
        } else {
            k.clamp(0.0, self.segs.len() as f32)
        };
        let i = (k.floor() as usize).min(self.segs.len() - 1);
        let u = f64::from(k) - i as f64;
        self.sample(i, u.clamp(0.0, 1.0))
    }

    fn lone_sample(&self) -> SplineSample {
        SplineSample {
            position: self.lone,
            tangent: Vec3::X,
            up: Vec3::Y,
            right: Vec3::Z,
            roll_deg: self.rolls.first().copied().unwrap_or(0.0),
            scale: self.scales.first().copied().unwrap_or(Vec2::ONE),
            curvature: 0.0,
            along_m: 0.0,
            key: 0.0,
            values: self.values.first().cloned().unwrap_or_default(),
        }
    }

    /// The full sample at segment `i`, parameter `u`.
    fn sample(&self, i: usize, u: f64) -> SplineSample {
        let seg = &self.segs[i];
        let pos = seg.pos(u);
        let t = self.tangent(seg, u);
        let along = seg.start + seg.s_at(u);
        let (d1, d2) = (seg.d1(u), seg.d2(u));
        let sp = d1.len();
        let curvature = if sp > 1.0e-9 {
            d1.cross(d2).len() / (sp * sp * sp)
        } else {
            0.0
        };
        let uf = u as f32;
        let roll = self.rolls[seg.a] + (self.rolls[seg.b] - self.rolls[seg.a]) * uf;
        let scale = self.scales[seg.a] + (self.scales[seg.b] - self.scales[seg.a]) * uf;
        let values = self.values[seg.a]
            .iter()
            .zip(&self.values[seg.b])
            .map(|(a, b)| a.lerp(b, uf))
            .collect();
        let up = self.frame_up(i, u, t, along);
        // Roll about the tangent: up' = up cos θ + right sin θ.
        let right = t.cross(up);
        let (sn, cs) = det::sin_cos(f64::from(roll).to_radians());
        let up = (up * cs + right * sn).norm_or(up);
        let right = t.cross(up).norm_or(right);
        SplineSample {
            position: pos.v(),
            tangent: t.v(),
            up: up.v(),
            right: right.v(),
            roll_deg: roll,
            scale,
            curvature: curvature as f32,
            along_m: along as f32,
            key: (i as f64 + u) as f32,
            values,
        }
    }

    /// The frame's up vector (before roll) at `(i, u)` with unit tangent `t`.
    fn frame_up(&self, i: usize, u: f64, t: D3, along: f64) -> D3 {
        if self.up_mode == SplineUp::WorldY {
            // Within 1° of vertical: cos(1°) ≈ 0.999 847 7.
            if t.dot(Y).abs() < 0.999_847_7 {
                return (Y - t * t.dot(Y)).norm_or(Y);
            }
        }
        let nf = TABLE_SAMPLES as f64;
        let j = ((u * nf).floor() as usize).min(TABLE_SAMPLES - 1);
        let f = u * nf - j as f64;
        let base = i * (TABLE_SAMPLES + 1);
        let (a, b) = (self.frames[base + j], self.frames[base + j + 1]);
        let r = a * (1.0 - f) + b * f;
        let mut up = (r - t * r.dot(t)).norm_or(initial_up(t));
        if self.twist != 0.0 && self.length > 0.0 {
            let ang = self.twist * (along / self.length);
            let (sn, cs) = det::sin_cos(ang);
            // Rodrigues about `t` for a vector ⊥ t.
            up = (up * cs + t.cross(up) * sn).norm_or(up);
        }
        up
    }

    /// The closest point on the curve to `p` (coarse search over the tables, then Newton; ties
    /// go to the lower key).
    ///
    /// The result is the one of a scan over every table sample of every segment, bit for bit;
    /// segments whose sample box cannot hold the minimum or a candidate within reach are skipped
    /// (ADR-7238).
    pub fn closest(&self, p: Vec3) -> ClosestHit {
        let q = D3::from_v(p);
        if self.segs.is_empty() {
            return ClosestHit {
                key: 0.0,
                distance_m: (D3::from_v(self.lone) - q).len() as f32,
                along_m: 0.0,
                position: self.lone,
            };
        }
        let nf = TABLE_SAMPLES as f64;
        // A lower bound of every sample's squared distance in segment `i`: per axis the gap to the
        // box (never more than the sample's own difference, rounding is monotonic), summed in the
        // distance's operation order, so it never exceeds a computed distance. A NaN gap becomes
        // the other operand or 0, which only weakens the bound.
        let floor = |i: usize| -> f64 {
            let [lo, hi] = self.boxes[i];
            let mut g = D3::default();
            for k in 0..3 {
                g.0[k] = (lo.0[k] - q.0[k]).max(q.0[k] - hi.0[k]).max(0.0);
            }
            g.dot(g)
        };
        let row = |seg: &Seg, out: &mut [f64; TABLE_SAMPLES + 1]| {
            #[cfg(test)]
            closest_rows::bump();
            for (j, d) in out.iter_mut().enumerate() {
                let x = seg.pos(j as f64 / nf);
                *d = (x - q).dot(x - q);
            }
        };
        let mut d2 = [0.0f64; TABLE_SAMPLES + 1];
        // Coarse pass: the smallest squared sample distance. Start in the nearest box, then skip
        // every box that cannot go below the running minimum.
        let mut first = 0;
        let mut first_floor = f64::INFINITY;
        for i in 0..self.segs.len() {
            let f = floor(i);
            if f < first_floor {
                first = i;
                first_floor = f;
            }
        }
        let mut best = f64::INFINITY;
        for i in std::iter::once(first).chain((0..self.segs.len()).filter(|&i| i != first)) {
            if floor(i) >= best {
                continue;
            }
            row(&self.segs[i], &mut d2);
            for d in d2 {
                best = best.min(d);
            }
        }
        // Refine every local minimum that could beat the best coarse sample.
        let reach = best.sqrt() + self.max_step;
        let reach2 = reach * reach;
        let mut hit: Option<(f64, usize, f64)> = None;
        for (i, seg) in self.segs.iter().enumerate() {
            if floor(i) > reach2 {
                // Every sample is out of reach: none is a candidate.
                continue;
            }
            row(seg, &mut d2);
            for j in 0..=TABLE_SAMPLES {
                if d2[j] > reach2 {
                    continue;
                }
                let left = j == 0 || d2[j - 1] >= d2[j];
                let right = j == TABLE_SAMPLES || d2[j + 1] >= d2[j];
                if !(left && right) {
                    continue;
                }
                let lo = (j.saturating_sub(1)) as f64 / nf;
                let hi = ((j + 1).min(TABLE_SAMPLES)) as f64 / nf;
                let u = refine_closest(seg, q, j as f64 / nf, lo, hi);
                let x = seg.pos(u);
                let d = (x - q).len();
                let better = match hit {
                    None => true,
                    Some((bd, bi, bu)) => d < bd || (d == bd && (i as f64 + u) < (bi as f64 + bu)),
                };
                if better {
                    hit = Some((d, i, u));
                }
            }
        }
        let (d, i, u) = hit.unwrap_or((best.sqrt(), 0, 0.0));
        let seg = &self.segs[i];
        ClosestHit {
            key: (i as f64 + u) as f32,
            distance_m: d as f32,
            along_m: (seg.start + seg.s_at(u)) as f32,
            position: seg.pos(u).v(),
        }
    }

    /// A polyline within `max_error_m` of the curve (adaptive subdivision; every control point is
    /// a vertex; a closed spline repeats its first vertex at the end).
    pub fn to_polyline(&self, max_error_m: f32) -> Vec<Vec3> {
        let tol = f64::from(max_error_m.max(1.0e-4));
        let Some(first) = self.segs.first() else {
            return vec![self.lone];
        };
        let mut out = vec![first.pos(0.0).v()];
        for seg in &self.segs {
            subdivide(seg, 0.0, 1.0, tol, 0, &mut out);
        }
        out
    }

    /// An axis-aligned box `(min, max)` containing the curve (the hull of each segment's Bézier
    /// control points).
    pub fn bounds(&self) -> (Vec3, Vec3) {
        if self.segs.is_empty() {
            return (self.lone, self.lone);
        }
        let mut lo = Vec3::splat(f32::INFINITY);
        let mut hi = Vec3::splat(f32::NEG_INFINITY);
        for seg in &self.segs {
            let c = if seg.step {
                [seg.p0; 4]
            } else {
                [
                    seg.p0,
                    seg.p0 + seg.m0 * (1.0 / 3.0),
                    seg.p1 - seg.m1 * (1.0 / 3.0),
                    seg.p1,
                ]
            };
            for p in c {
                lo = lo.min(p.v());
                hi = hi.max(p.v());
            }
        }
        (lo, hi)
    }
}

/// Appends `tail` to `head`, merging the shared point (head's incoming, tail's outgoing tangent).
fn join(head: &mut Spline, tail: Spline) {
    let mut it = tail.points.into_iter();
    if let Some(first) = it.next() {
        match head.points.last_mut() {
            Some(last) => {
                last.tangent_out = first.tangent_out;
                last.mode = first.mode;
            }
            None => head.points.push(first),
        }
    }
    head.points.extend(it);
}

fn subdivide(seg: &Seg, u0: f64, u1: f64, tol: f64, depth: u32, out: &mut Vec<Vec3>) {
    let (a, b) = (seg.pos(u0), seg.pos(u1));
    let um = (u0 + u1) * 0.5;
    let m = seg.pos(um);
    // Distance of the midpoint (and the quarter points) from the chord.
    let chord_err = |x: D3| {
        let ab = b - a;
        let l2 = ab.dot(ab);
        if l2 <= 1.0e-18 {
            return (x - a).len();
        }
        let t = ((x - a).dot(ab) / l2).clamp(0.0, 1.0);
        (x - (a + ab * t)).len()
    };
    let err = chord_err(m)
        .max(chord_err(seg.pos((u0 + um) * 0.5)))
        .max(chord_err(seg.pos((um + u1) * 0.5)));
    if depth < 16 && err > tol {
        subdivide(seg, u0, um, tol, depth + 1, out);
        subdivide(seg, um, u1, tol, depth + 1, out);
    } else {
        out.push(b.v());
    }
}

/// Newton iterations on `(x(u) − q)·x'(u) = 0` inside `[lo, hi]`, keeping the best iterate.
fn refine_closest(seg: &Seg, q: D3, u0: f64, lo: f64, hi: f64) -> f64 {
    let dist = |u: f64| {
        let x = seg.pos(u);
        (x - q).dot(x - q)
    };
    let mut u = u0;
    let mut best = (dist(u), u);
    for _ in 0..12 {
        let x = seg.pos(u);
        let (d1, d2) = (seg.d1(u), seg.d2(u));
        let f = (x - q).dot(d1);
        let fp = d1.dot(d1) + (x - q).dot(d2);
        if fp.abs() <= 1.0e-18 {
            break;
        }
        let mut next = u - f / fp;
        if !next.is_finite() || fp < 0.0 {
            // Not a minimum direction: bisect toward the lower neighbor.
            next = if f > 0.0 { (u + lo) * 0.5 } else { (u + hi) * 0.5 };
        }
        u = next.clamp(lo, hi);
        let d = dist(u);
        if d < best.0 {
            best = (d, u);
        }
    }
    best.1
}

/// Test-only work counter of [`SplineCurve::closest`]: segment rows of table samples it evaluated.
#[cfg(test)]
mod closest_rows {
    use std::cell::Cell;

    thread_local! {
        static ROWS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn bump() {
        ROWS.with(|r| r.set(r.get() + 1));
    }

    /// Returns the count and resets it.
    pub(super) fn take() -> usize {
        ROWS.with(|r| r.replace(0))
    }
}

/// World Y projected off the tangent, or world Z when the tangent is vertical.
fn initial_up(t: D3) -> D3 {
    let base = if t.dot(Y).abs() > 0.9999 { Z } else { Y };
    (base - t * base.dot(t)).norm_or(Z)
}

/// One double-reflection step (Wang et al. 2008): carries the reference vector `r` from
/// `(x0, t0)` to `(x1, t1)`.
fn double_reflect(x0: D3, t0: D3, r: D3, x1: D3, t1: D3) -> D3 {
    let v1 = x1 - x0;
    let c1 = v1.dot(v1);
    let (r_l, t_l) = if c1 > 1.0e-18 {
        (r - v1 * (2.0 / c1 * v1.dot(r)), t0 - v1 * (2.0 / c1 * v1.dot(t0)))
    } else {
        (r, t0)
    };
    let v2 = t1 - t_l;
    let c2 = v2.dot(v2);
    let r1 = if c2 > 1.0e-18 {
        r_l - v2 * (2.0 / c2 * v2.dot(r_l))
    } else {
        r_l
    };
    (r1 - t1 * r1.dot(t1)).norm_or(initial_up(t1))
}

/// Platform-independent angle functions (only `+ − × ÷`, `floor` and `sqrt`, which IEEE 754
/// defines exactly). The same textbook method as `regia_procgen::detmath` (range reduction plus
/// truncated Taylor series in `f64`); a private copy because this crate sits below it.
mod det {
    use std::f64::consts::{FRAC_PI_2, PI};

    /// `(sin x, cos x)`.
    pub(super) fn sin_cos(x: f64) -> (f64, f64) {
        if !x.is_finite() {
            return (0.0, 1.0);
        }
        let k = (x / FRAC_PI_2 + 0.5).floor();
        let r = x - k * FRAC_PI_2;
        let r2 = r * r;
        let s = r
            * (1.0
                + r2 * (-1.0 / 6.0
                    + r2 * (1.0 / 120.0
                        + r2 * (-1.0 / 5040.0
                            + r2 * (1.0 / 362_880.0 + r2 * (-1.0 / 39_916_800.0 + r2 / 6_227_020_800.0))))));
        let c = 1.0
            + r2 * (-0.5
                + r2 * (1.0 / 24.0
                    + r2 * (-1.0 / 720.0
                        + r2 * (1.0 / 40_320.0 + r2 * (-1.0 / 3_628_800.0 + r2 / 479_001_600.0)))));
        let q = k - 4.0 * (k / 4.0).floor();
        match q as i32 {
            0 => (s, c),
            1 => (c, -s),
            2 => (-s, -c),
            _ => (-c, s),
        }
    }

    /// `atan(t)` for `|t| ≤ 1`: two argument halvings, then the series.
    fn atan_unit(t: f64) -> f64 {
        let u1 = t / (1.0 + (1.0 + t * t).sqrt());
        let u = u1 / (1.0 + (1.0 + u1 * u1).sqrt());
        let u2 = u * u;
        let mut term = u;
        let mut sum = 0.0;
        let mut n = 1.0;
        for _ in 0..12 {
            sum += term / n;
            term *= -u2;
            n += 2.0;
        }
        4.0 * sum
    }

    /// `atan2(y, x)` in `(−π, π]` (0 for the origin).
    pub(super) fn atan2(y: f64, x: f64) -> f64 {
        if !(x.is_finite() && y.is_finite()) || (x == 0.0 && y == 0.0) {
            return 0.0;
        }
        if x.abs() >= y.abs() {
            let a = atan_unit(y / x);
            if x > 0.0 {
                a
            } else if y >= 0.0 {
                a + PI
            } else {
                a - PI
            }
        } else {
            let a = atan_unit(x / y);
            if y > 0.0 { FRAC_PI_2 - a } else { -FRAC_PI_2 - a }
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn matches_std_closely() {
            for i in -2000..2000 {
                let x = f64::from(i) * 0.0137;
                let (s, c) = super::sin_cos(x);
                assert!((s - x.sin()).abs() < 1e-9 && (c - x.cos()).abs() < 1e-9, "{x}");
                let (y, xx) = (x.sin() * 3.0, (x * 0.7).cos() * 2.0);
                assert!((super::atan2(y, xx) - y.atan2(xx)).abs() < 1e-9, "{y} {xx}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `closest` of the v0.5.0 base, kept verbatim as the oracle: every table sample of every
    /// segment, then Newton on each local minimum within reach (ADR-7238).
    fn closest_reference(c: &SplineCurve, p: Vec3) -> ClosestHit {
        let q = D3::from_v(p);
        if c.segs.is_empty() {
            return ClosestHit {
                key: 0.0,
                distance_m: (D3::from_v(c.lone) - q).len() as f32,
                along_m: 0.0,
                position: c.lone,
            };
        }
        let nf = TABLE_SAMPLES as f64;
        let mut d2 = Vec::with_capacity(c.segs.len() * (TABLE_SAMPLES + 1));
        let mut best = f64::INFINITY;
        let mut max_step = 0.0f64;
        for seg in &c.segs {
            let mut prev: Option<D3> = None;
            for j in 0..=TABLE_SAMPLES {
                let x = seg.pos(j as f64 / nf);
                let d = (x - q).dot(x - q);
                best = best.min(d);
                if let Some(px) = prev {
                    max_step = max_step.max((x - px).len());
                }
                prev = Some(x);
                d2.push(d);
            }
        }
        let reach = best.sqrt() + max_step;
        let reach2 = reach * reach;
        let mut hit: Option<(f64, usize, f64)> = None;
        for (i, seg) in c.segs.iter().enumerate() {
            let row = &d2[i * (TABLE_SAMPLES + 1)..(i + 1) * (TABLE_SAMPLES + 1)];
            for j in 0..=TABLE_SAMPLES {
                if row[j] > reach2 {
                    continue;
                }
                let left = j == 0 || row[j - 1] >= row[j];
                let right = j == TABLE_SAMPLES || row[j + 1] >= row[j];
                if !(left && right) {
                    continue;
                }
                let lo = (j.saturating_sub(1)) as f64 / nf;
                let hi = ((j + 1).min(TABLE_SAMPLES)) as f64 / nf;
                let u = refine_closest(seg, q, j as f64 / nf, lo, hi);
                let x = seg.pos(u);
                let d = (x - q).len();
                let better = match hit {
                    None => true,
                    Some((bd, bi, bu)) => d < bd || (d == bd && (i as f64 + u) < (bi as f64 + bu)),
                };
                if better {
                    hit = Some((d, i, u));
                }
            }
        }
        let (d, i, u) = hit.unwrap_or((best.sqrt(), 0, 0.0));
        let seg = &c.segs[i];
        ClosestHit {
            key: (i as f64 + u) as f32,
            distance_m: d as f32,
            along_m: (seg.start + seg.s_at(u)) as f32,
            position: seg.pos(u).v(),
        }
    }

    fn hit_bits(h: &ClosestHit) -> [u32; 6] {
        [
            h.key.to_bits(),
            h.distance_m.to_bits(),
            h.along_m.to_bits(),
            h.position.x.to_bits(),
            h.position.y.to_bits(),
            h.position.z.to_bits(),
        ]
    }

    /// The spec 64 § 10 bench curve (`benches/splines.rs` `winding`).
    fn winding(points: usize, length_m: f32) -> Spline {
        let step = length_m / (points - 1) as f32;
        Spline::through((0..points).map(|i| {
            let x = i as f32 * step;
            let k = i % 8;
            let z = if k < 4 { k as f32 } else { (8 - k) as f32 } * step * 0.2;
            Vec3::new(x, 0.0, z)
        }))
    }

    /// Spec 64 § 10 (`closest` ≤ 20 µs on a 200-point spline): the query evaluates the table rows
    /// of the segments that can hold the answer, not all of them. Counted, not timed.
    #[test]
    fn closest_scans_only_the_segments_near_the_query() {
        let c = winding(200, 2_000.0).curve();
        assert_eq!(c.segment_count(), 199);
        closest_rows::take();
        let queries = 200;
        for i in 0..queries {
            let x = (i * 10) as f32;
            let _ = c.closest(Vec3::new(x, 3.0, 40.0));
        }
        let rows = closest_rows::take();
        let per_query = rows as f64 / f64::from(queries);
        assert!(
            per_query <= 32.0,
            "closest evaluated {per_query:.1} segment rows per query on a 199-segment spline"
        );
    }

    /// The bounded search returns the oracle's hit bit for bit: open, closed, `Step`, `Linear`,
    /// repeated points, a world transform, one and two points, ties (circle center, mirror axis),
    /// points on the table samples, far and non-finite queries.
    #[test]
    fn closest_matches_the_full_scan_bit_for_bit() {
        let mut curves: Vec<SplineCurve> = vec![winding(200, 2_000.0).curve(), winding(9, 40.0).curve()];
        let ring = |n: usize, r: f32| {
            Spline::through((0..n).map(|i| {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                Vec3::new(r * a.cos(), 0.0, r * a.sin())
            }))
        };
        curves.push(ring(12, 10.0).closed().curve());
        curves.push(ring(5, 3.0).closed().curve());
        let helix = Spline::through((0..40).map(|i| {
            let a = i as f32 * 0.4;
            Vec3::new(5.0 * a.cos(), i as f32 * 0.5, 5.0 * a.sin())
        }));
        curves.push(helix.curve());
        let mut mixed = Spline::through([
            Vec3::ZERO,
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(10.0, 4.0, 10.0),
            Vec3::new(0.0, 0.0, 10.0),
            Vec3::new(-5.0, 0.0, 3.0),
        ]);
        mixed.points[1].mode = TangentMode::Linear;
        mixed.points[3].mode = TangentMode::Step;
        curves.push(mixed.curve());
        let u_shape = Spline::through([
            Vec3::new(-10.0, 0.0, 10.0),
            Vec3::new(-10.0, 0.0, 0.0),
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(10.0, 0.0, 10.0),
        ]);
        curves.push(u_shape.curve());
        let t = Affine3A::from_scale_rotation_translation(
            Vec3::new(2.0, 1.0, 0.5),
            regia_core::math::Quat::from_rotation_y(0.7),
            Vec3::new(3.0, 1.0, -2.0),
        );
        curves.push(winding(30, 300.0).world_curve(&t));
        curves.push(Spline::through([Vec3::new(1.0, 2.0, 3.0)]).curve());
        curves.push(Spline::through([Vec3::ZERO, Vec3::X]).curve());
        curves.push(Spline::through([Vec3::ZERO, Vec3::NAN, Vec3::X * 3.0]).curve());
        curves.push(Spline::through([Vec3::ZERO, Vec3::splat(f32::INFINITY)]).curve());
        let mut checked = 0usize;
        for (ci, c) in curves.iter().enumerate() {
            let (lo, hi) = c.bounds();
            let mut queries = vec![
                Vec3::ZERO,
                Vec3::new(0.0, 0.0, 5.0),
                Vec3::new(0.0, 7.0, 0.0),
                Vec3::splat(1.0e5),
                Vec3::new(-3.0e4, 2.0, 9.0),
                Vec3::NAN,
                Vec3::new(f32::INFINITY, 0.0, 0.0),
            ];
            // Every table sample position (exact ties with the coarse pass) and a lattice.
            for seg in c.segs.iter().take(64) {
                for j in [0, 1, 7, 16, TABLE_SAMPLES] {
                    queries.push(seg.pos(j as f64 / TABLE_SAMPLES as f64).v());
                }
            }
            if lo.is_finite() && hi.is_finite() {
                let span = (hi - lo).max(Vec3::ONE);
                for i in 0..=12 {
                    for k in 0..=12 {
                        for y in [-2.0f32, 0.0, 3.0] {
                            let f = Vec3::new(i as f32 / 12.0, 0.5, k as f32 / 12.0);
                            let mut q = lo - span * 0.25 + span * 1.5 * f;
                            q.y += y;
                            queries.push(q);
                        }
                    }
                }
            }
            for q in queries {
                let want = closest_reference(c, q);
                let got = c.closest(q);
                assert_eq!(
                    hit_bits(&got),
                    hit_bits(&want),
                    "curve {ci} query {q:?}: {got:?} vs {want:?}"
                );
                checked += 1;
            }
        }
        assert!(checked > 5_000, "{checked}");
    }

    #[test]
    fn non_finite_points_do_not_abort_evaluation() {
        let s = Spline::through([Vec3::NAN, Vec3::X]);
        let _ = s.at_distance(0.0);
        let _ = s.at_param(0.5);
        let c = s.curve();
        let _ = c.locate_distance(1.0);
        let _ = c.range(0.0, 1.0);
        let _ = c.split_at(&[0.5]);
        let inf = Spline::through([Vec3::splat(f32::INFINITY), Vec3::X]);
        let _ = inf.at_distance(0.0);
        let _ = inf.at_param(1.0);
    }

    #[test]
    fn default_is_empty_open_version_one() {
        let s = Spline::default();
        assert!(s.points.is_empty() && !s.closed && s.format_version == 1);
        assert_eq!(SplinePoint::default().scale, Vec2::ONE);
        assert_eq!(s.length(), 0.0);
        assert_eq!(s.at_distance(3.0).position, Vec3::ZERO);
        let one = Spline::through([Vec3::new(1.0, 2.0, 3.0)]);
        assert_eq!(one.segment_count(), 0);
        assert_eq!(one.at_param(0.5).position, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn linear_and_step_segments() {
        let mut s = Spline::through([Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0), Vec3::new(10.0, 0.0, 10.0)]);
        for p in &mut s.points {
            p.mode = TangentMode::Linear;
        }
        assert!((s.length() - 20.0).abs() < 1e-4);
        let m = s.at_distance(15.0).position;
        assert!(m.abs_diff_eq(Vec3::new(10.0, 0.0, 5.0), 1e-4), "{m:?}");
        s.points[0].mode = TangentMode::Step;
        assert!((s.segment_length(0)).abs() < 1e-9);
        assert_eq!(s.at_key(0.5).position, Vec3::ZERO);
    }

    #[test]
    fn auto_matches_the_camera_path_curve() {
        let pts = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(4.0, 1.0, 0.0),
            Vec3::new(5.0, 0.0, 4.0),
            Vec3::new(1.0, -1.0, 6.0),
        ];
        let c = Spline::through(pts).curve();
        for i in 0..3 {
            for k in 0..=8 {
                let u = k as f32 / 8.0;
                let a = c.at_key(i as f32 + u).position;
                let b = catmull_rom_with(&pts, i, u, centripetal_knot_step);
                assert!(a.abs_diff_eq(b, 1e-4), "{i} {u}: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn values_interpolate_and_names_hold() {
        let mut s = Spline::through([Vec3::ZERO, Vec3::new(4.0, 0.0, 0.0)]);
        s.attrs = vec![
            SplineAttrDecl {
                name: "w".into(),
                ty: SplineAttrType::F32,
            },
            SplineAttrDecl {
                name: "n".into(),
                ty: SplineAttrType::Name,
            },
        ];
        s.points[0].values = vec![SplineValue::F32(0.0), SplineValue::Name("a".into())];
        s.points[1].values = vec![SplineValue::F32(2.0), SplineValue::Name("b".into())];
        s.points[1].roll_deg = 90.0;
        assert!(s.validate().is_ok());
        let m = s.at_param(0.5);
        assert!(matches!(m.values[0], SplineValue::F32(v) if (v - 1.0).abs() < 1e-3));
        assert_eq!(m.values[1], SplineValue::Name("a".into()));
        assert!((m.roll_deg - 45.0).abs() < 0.1);
        // Rolled 90° at the end: up turns to the old right.
        let e = s.at_param(1.0);
        assert!(e.up.abs_diff_eq(Vec3::Z, 1e-4), "{:?}", e.up);
        s.points[0].values.pop();
        assert_eq!(s.validate(), Err(SplineError::Values { index: 0 }));
    }

    #[test]
    fn validation_codes() {
        let mut s = Spline::through((0..SPLINE_MAX_POINTS + 1).map(|i| Vec3::new(i as f32, 0.0, 0.0)));
        assert_eq!(s.validate().map_err(|e| e.code()), Err("spline_too_many_points"));
        s.points.truncate(3);
        s.points[1].position.y = f32::NAN;
        assert_eq!(s.validate(), Err(SplineError::NonFinite { index: 1 }));
        s.format_version = 9;
        assert_eq!(s.migrate(), Err(SplineError::Version(9)));
    }

    #[test]
    fn world_curve_scales_length() {
        let s = Spline::through([Vec3::ZERO, Vec3::new(3.0, 0.0, 0.0)]);
        let t = Affine3A::from_scale_rotation_translation(
            Vec3::new(2.0, 1.0, 1.0),
            regia_core::math::Quat::IDENTITY,
            Vec3::new(0.0, 5.0, 0.0),
        );
        let c = s.world_curve(&t);
        assert!((c.length() - 6.0).abs() < 1e-4);
        assert!(
            c.at_param(0.0)
                .position
                .abs_diff_eq(Vec3::new(0.0, 5.0, 0.0), 1e-5)
        );
    }

    #[test]
    fn polyline_and_bounds_contain_the_curve() {
        let s = Spline::through([
            Vec3::ZERO,
            Vec3::new(10.0, 0.0, 5.0),
            Vec3::new(20.0, 3.0, -5.0),
            Vec3::new(30.0, 0.0, 0.0),
        ]);
        let c = s.curve();
        let line = c.to_polyline(0.01);
        assert!(line.len() > 4);
        assert_eq!(line[0], Vec3::ZERO);
        assert!(
            line.last()
                .is_some_and(|p| p.abs_diff_eq(Vec3::new(30.0, 0.0, 0.0), 1e-5))
        );
        let (lo, hi) = c.bounds();
        for k in 0..=300 {
            let p = c.at_param(k as f32 / 300.0).position;
            assert!(p.cmpge(lo - 1e-4).all() && p.cmple(hi + 1e-4).all());
            // The polyline stays within the error of the curve.
            let near = line
                .windows(2)
                .map(|w| {
                    let ab = w[1] - w[0];
                    let t = ((p - w[0]).dot(ab) / ab.length_squared().max(1e-12)).clamp(0.0, 1.0);
                    (p - (w[0] + ab * t)).length()
                })
                .fold(f32::INFINITY, f32::min);
            assert!(near < 0.011, "{near}");
        }
    }
}
