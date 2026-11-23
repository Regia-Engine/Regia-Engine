// SPDX-License-Identifier: MPL-2.0
//! Continuous collision detection (CCD): time of impact between two convex shapes swept
//! along rigid paths, by conservative advancement over a GJK distance query.
//!
//! This crate otherwise delegates collision queries to rapier/parry (see
//! [`crate::queries`]); it has no distance query of its own. This module is a
//! self-contained, deterministic query for gameplay and tooling code that needs a
//! time of impact (TOI) between two [`ConvexInput`] shapes without a physics world:
//! projectiles, kinematic sweeps, placement checks, agent-protocol probes.
//!
//! # Conventions
//!
//! - Poses are [`Isometry`] values (rotation + translation, no scale), the rigid-pose type
//!   of `regia_core::math`. A shape's local geometry is centred as in
//!   [`ColliderShape`]; bake any scale into the shape numbers.
//! - `t` is normalised to the sweep interval: pose `xf_*0` at `t = 0`, pose `xf_*1` at
//!   `t = 1`. Translation interpolates linearly and rotation by slerp
//!   ([`Isometry::interpolate`]). Multiply `t` by your step `dt` for seconds.
//! - In a [`ToiResult::Hit`], `normal` is a unit vector pointing **from shape A toward
//!   shape B**, and `point` is the witness point **on shape A** (world space) at time `t`.
//!   The witness point on B is `point + normal * gap_at_t`.
//! - `target_gap` is the separation at which the sweep stops (a skin width); `0.0` means
//!   surface contact. `tolerance` is how close to `target_gap` the reported separation
//!   must be, in metres. Both are distances, not times.
//! - Everything is deterministic: fixed iteration order, first-index tie-breaking in every
//!   support query, no hash maps, no clock. Same inputs give bit-identical outputs.
//!
//! # Algorithm
//!
//! Distance is the Gilbert–Johnson–Keerthi (GJK) algorithm on the Minkowski difference
//! `A − B`, with the closest point on the simplex found by exhaustively evaluating the
//! subsimplices (at most 15 for a tetrahedron) and keeping the smallest, which is simple
//! to make robust for degenerate (flat, linear, point) shapes; that solve runs in f64.
//! Termination uses the duality gap `|v| − v·w/|v|` (a lower bound on the distance from
//! the support plane), a relative zero test for touching/overlap, and a progress check;
//! when the loop has to stop with the gap still open (f32 support noise on curved shapes),
//! the reported distance is the proven lower bound, so advancement stays conservative.
//!
//! TOI is conservative advancement: at the current `t`, with separation `d` along the
//! witness normal `n`, advance by `(d − target_gap) / closing`, where `closing` bounds
//! how fast the separation along the fixed axis `n` can shrink:
//! `max(0, −n·v_rel) + ω_a·R_a + ω_b·R_b` (`v_rel` the relative translation per unit `t`,
//! `ω` a bound on each shape's swept angle per unit `t`, `R` its bounding radius about
//! its pose origin). Because the true distance is bounded below by the projected
//! separation along any fixed axis, and that projected separation cannot shrink faster
//! than `closing`, a step can never jump past `target_gap`: the sweep cannot tunnel.
//! If floating-point error ever does overshoot, the interval is bisected back to the
//! last safe `t` before a hit is reported.
//!
//! Provenance: Gilbert, Johnson & Keerthi, "A Fast Procedure for Computing the Distance
//! Between Complex Objects in Three-Dimensional Space", IEEE Journal of Robotics and
//! Automation 4(2), 1988 (GJK and the Minkowski-difference support iteration); van den
//! Bergen, "A Fast and Robust GJK Implementation for Collision Detection of Convex
//! Objects", Journal of Graphics Tools 4(2), 1999 (duality-gap termination, numerical
//! robustness); Mirtich, "Impulse-based Dynamic Simulation of Rigid Body Systems", PhD
//! thesis, UC Berkeley, 1996 (conservative advancement, with the motion bound of linear
//! speed plus angular speed times radius). Full entries are in `docs/provenance.md`.
//! All code here is original.

use glam::{DMat3, DVec3};
use regia_core::math::{Isometry, Quat, Vec3, det_acos, det_quat_from_scaled_axis};

use crate::components::{ColliderShape, ShapeAxis};

/// A convex shape for [`time_of_impact`] and [`convex_distance`], in its own local space.
///
/// Mirrors the convex variants of [`ColliderShape`] so a collider
/// converts with [`ConvexInput::from_collider_shape`]. All sizes are in metres and must be
/// finite and non-negative; zero sizes are legal and give degenerate shapes (a zero-radius
/// sphere is a point, a box with a zero half-extent is a flat plate), which the queries
/// handle without panicking.
#[derive(Debug, Clone, PartialEq)]
pub enum ConvexInput {
    /// A single point at the local origin.
    Point,
    /// A sphere centred on the local origin.
    Sphere {
        /// Radius in metres (`>= 0`).
        radius: f32,
    },
    /// An axis-aligned box (cuboid) centred on the local origin.
    Box {
        /// Half-extents (width/2, height/2, depth/2), each `>= 0`.
        half_extents: Vec3,
    },
    /// A capsule: a segment along `axis` swept by a sphere of `radius`.
    Capsule {
        /// Half the length of the central segment (`>= 0`).
        half_height: f32,
        /// Radius of the swept sphere (`>= 0`).
        radius: f32,
        /// Long axis of the capsule.
        axis: ShapeAxis,
    },
    /// A finite cylinder along `axis`, centred on the local origin.
    Cylinder {
        /// Half-height along `axis` (`>= 0`).
        half_height: f32,
        /// Radius (`>= 0`).
        radius: f32,
        /// Long axis of the cylinder.
        axis: ShapeAxis,
    },
    /// A finite cone along `axis`: apex at `+axis * half_height`, base disc of `radius`
    /// at `-axis * half_height`.
    Cone {
        /// Half-height along `axis` (`>= 0`).
        half_height: f32,
        /// Base radius (`>= 0`).
        radius: f32,
        /// Long axis of the cone, from base centre toward the apex.
        axis: ShapeAxis,
    },
    /// A line segment between two local points (a degenerate convex shape).
    Segment {
        /// First endpoint, local space.
        a: Vec3,
        /// Second endpoint, local space.
        b: Vec3,
    },
    /// A convex hull given by points in local space. The points need not be hull vertices
    /// only (interior points are harmless) and their order does not matter, except that
    /// support ties resolve to the lowest index. Must be non-empty.
    ConvexHull {
        /// Points in local space, all finite.
        points: Vec<Vec3>,
    },
}

impl ConvexInput {
    /// A point at the local origin.
    pub const fn point() -> Self {
        Self::Point
    }

    /// A sphere of `radius` centred on the local origin. Validity is checked by the
    /// queries, not here, so an invalid radius yields [`ToiFailure`], never a panic.
    pub const fn sphere(radius: f32) -> Self {
        Self::Sphere { radius }
    }

    /// A box with the given `half_extents`, centred on the local origin.
    pub const fn cuboid(half_extents: Vec3) -> Self {
        Self::Box { half_extents }
    }

    /// Converts a [`ColliderShape`] to a [`ConvexInput`].
    ///
    /// Returns `None` for shapes that are not a single convex body (`TriMesh`,
    /// `HeightField`, `Compound`) and for invalid numbers (non-finite or negative sizes,
    /// an empty hull). For a `Compound`, convert and query each convex child separately.
    pub fn from_collider_shape(shape: &ColliderShape) -> Option<Self> {
        let input = match shape {
            ColliderShape::Box { half_extents } => Self::Box {
                half_extents: *half_extents,
            },
            ColliderShape::Sphere { radius } => Self::Sphere { radius: *radius },
            ColliderShape::Capsule {
                half_height,
                radius,
                axis,
            } => Self::Capsule {
                half_height: *half_height,
                radius: *radius,
                axis: *axis,
            },
            ColliderShape::Cylinder {
                half_height,
                radius,
                axis,
            } => Self::Cylinder {
                half_height: *half_height,
                radius: *radius,
                axis: *axis,
            },
            ColliderShape::Cone {
                half_height,
                radius,
                axis,
            } => Self::Cone {
                half_height: *half_height,
                radius: *radius,
                axis: *axis,
            },
            ColliderShape::ConvexHull { points } => Self::ConvexHull {
                points: points.clone(),
            },
            ColliderShape::TriMesh { .. }
            | ColliderShape::HeightField { .. }
            | ColliderShape::Compound { .. } => return None,
        };
        match validate_shape(&input) {
            Ok(()) => Some(input),
            Err(_) => None,
        }
    }

    /// The bounding radius about the local origin: every point `p` of the shape has
    /// `|p| <= bounding_radius()`. Used for the angular term of conservative advancement;
    /// returns `0.0` for a point. Assumes a valid shape (see the queries for validation);
    /// the result for non-finite input is whatever the arithmetic gives.
    pub fn bounding_radius(&self) -> f32 {
        match self {
            Self::Point => 0.0,
            Self::Sphere { radius } => *radius,
            Self::Box { half_extents } => half_extents.length(),
            Self::Capsule {
                half_height, radius, ..
            } => half_height + radius,
            Self::Cylinder {
                half_height, radius, ..
            }
            | Self::Cone {
                half_height, radius, ..
            } => (half_height * half_height + radius * radius).sqrt(),
            Self::Segment { a, b } => a.length().max(b.length()),
            Self::ConvexHull { points } => {
                let mut best = 0.0_f32;
                for p in points {
                    best = best.max(p.length());
                }
                best
            }
        }
    }

    /// The furthest point of the shape, placed at pose `xf`, along world direction `dir`
    /// (the support mapping). `dir` need not be unit length; for a zero direction every
    /// point ties and the per-variant tie choice is returned. Ties resolve
    /// deterministically (lowest hull index, fixed sign choices), so repeated calls are
    /// bit-identical.
    pub fn support_point(&self, xf: Isometry, dir: Vec3) -> Vec3 {
        let local_dir = xf.rotation.inverse() * dir;
        xf.transform_point(self.support_local(local_dir))
    }

    /// A reference centre in local space (the origin for centred shapes, the midpoint of
    /// a segment, the average of hull points). Only used to seed the GJK iteration.
    fn local_center(&self) -> Vec3 {
        match self {
            Self::Segment { a, b } => (*a + *b) * 0.5,
            Self::ConvexHull { points } => {
                if points.is_empty() {
                    return Vec3::ZERO;
                }
                let mut sum = Vec3::ZERO;
                for p in points {
                    sum += *p;
                }
                sum / points.len() as f32
            }
            _ => Vec3::ZERO,
        }
    }

    /// The support mapping in local space: the point maximising `dir · p`.
    fn support_local(&self, dir: Vec3) -> Vec3 {
        match self {
            Self::Point => Vec3::ZERO,
            Self::Sphere { radius } => dir.normalize_or_zero() * *radius,
            Self::Box { half_extents } => Vec3::new(
                if dir.x >= 0.0 {
                    half_extents.x
                } else {
                    -half_extents.x
                },
                if dir.y >= 0.0 {
                    half_extents.y
                } else {
                    -half_extents.y
                },
                if dir.z >= 0.0 {
                    half_extents.z
                } else {
                    -half_extents.z
                },
            ),
            Self::Capsule {
                half_height,
                radius,
                axis,
            } => {
                let a = axis_vec(*axis);
                let end = if dir.dot(a) >= 0.0 {
                    a * *half_height
                } else {
                    a * -*half_height
                };
                end + dir.normalize_or_zero() * *radius
            }
            Self::Cylinder {
                half_height,
                radius,
                axis,
            } => {
                let a = axis_vec(*axis);
                let axial = dir.dot(a);
                let radial = dir - a * axial;
                let cap = if axial >= 0.0 {
                    a * *half_height
                } else {
                    a * -*half_height
                };
                cap + radial.normalize_or_zero() * *radius
            }
            Self::Cone {
                half_height,
                radius,
                axis,
            } => {
                // The cone is the convex hull of its apex and its base disc, so its
                // support is whichever of the two supports goes further along `dir`.
                let a = axis_vec(*axis);
                let apex = a * *half_height;
                let axial = dir.dot(a);
                let radial = dir - a * axial;
                let base = a * -*half_height + radial.normalize_or_zero() * *radius;
                if dir.dot(apex) >= dir.dot(base) {
                    apex
                } else {
                    base
                }
            }
            Self::Segment { a, b } => {
                if dir.dot(*a) >= dir.dot(*b) {
                    *a
                } else {
                    *b
                }
            }
            Self::ConvexHull { points } => {
                let mut best = Vec3::ZERO;
                let mut best_dot = f32::NEG_INFINITY;
                for p in points {
                    let d = dir.dot(*p);
                    if d > best_dot {
                        best_dot = d;
                        best = *p;
                    }
                }
                best
            }
        }
    }
}

/// The unit vector for a [`ShapeAxis`].
fn axis_vec(axis: ShapeAxis) -> Vec3 {
    match axis {
        ShapeAxis::X => Vec3::X,
        ShapeAxis::Y => Vec3::Y,
        ShapeAxis::Z => Vec3::Z,
    }
}

/// Why a CCD query could not produce an answer. Returned inside
/// [`ToiResult::Failed`] (and as the error of [`convex_distance`]) instead of panicking,
/// for every bad input the caller can supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToiFailure {
    /// A pose, shape number, `target_gap`, `tolerance` or angle bound was NaN or infinite.
    NonFiniteInput,
    /// A shape was structurally invalid: a negative size, or an empty convex hull.
    InvalidShape,
    /// A pose rotation was a zero (or near-zero) quaternion, which has no rotation.
    InvalidPose,
    /// A scalar parameter was out of range: `target_gap < 0`, `tolerance < 0`,
    /// `max_iterations == 0`, or a negative angle bound.
    InvalidParameter,
    /// Conservative advancement did not reach `target_gap` (or prove separation) within
    /// `max_iterations` advances. Raise the budget or the tolerance; near-grazing sweeps
    /// with a tiny tolerance are the usual cause.
    MaxIterations,
    /// The GJK iteration failed to converge, or an advance made no progress. Reported
    /// rather than guessed, so a caller can fall back (subdivide the interval, treat as
    /// a hit at the current pose, etc.).
    NumericalFailure,
    /// The closing speed bound was not finite (a motion so large that the bound overflows
    /// f32, or a NaN born inside the bound), or so large that a conservative advance no
    /// longer moves `t` at f32 resolution. Refused rather than looped or guessed (ADR-2423).
    DegenerateMotion,
}

impl ToiFailure {
    /// A stable lowercase code for logs and the agent protocol (`physics.time_of_impact`
    /// reports it as `reason`): `non_finite_input`, `invalid_shape`, `invalid_pose`,
    /// `invalid_parameter`, `max_iterations`, `numerical_failure`, `degenerate_motion`.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NonFiniteInput => "non_finite_input",
            Self::InvalidShape => "invalid_shape",
            Self::InvalidPose => "invalid_pose",
            Self::InvalidParameter => "invalid_parameter",
            Self::MaxIterations => "max_iterations",
            Self::NumericalFailure => "numerical_failure",
            Self::DegenerateMotion => "degenerate_motion",
        }
    }
}

impl core::fmt::Display for ToiFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            Self::NonFiniteInput => "non-finite input to CCD query",
            Self::InvalidShape => "invalid convex shape for CCD query",
            Self::InvalidPose => "invalid pose rotation for CCD query",
            Self::InvalidParameter => "invalid parameter for CCD query",
            Self::MaxIterations => "CCD query exceeded its iteration budget",
            Self::NumericalFailure => "CCD query failed to converge numerically",
            Self::DegenerateMotion => "CCD query closing speed is not finite or too large to resolve",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for ToiFailure {}

/// The result of a time-of-impact query over the normalised interval `t ∈ [0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToiResult {
    /// The shapes stay further apart than `target_gap + tolerance` for the whole sweep.
    Separated,
    /// The shapes first reach `target_gap` (within `tolerance`) at time `t`.
    Hit {
        /// Normalised time of impact in `[0, 1]`.
        t: f32,
        /// Unit world-space normal pointing from shape A toward shape B at `t`.
        normal: Vec3,
        /// World-space witness point on shape A at `t`.
        point: Vec3,
    },
    /// The shapes already interpenetrate at `t = 0`, so no first-contact time exists in
    /// the interval. (Merely touching, or already within `target_gap`, at `t = 0` is a
    /// [`ToiResult::Hit`] with `t = 0` instead.)
    OverlappedAtStart,
    /// The query could not answer; see [`ToiFailure`].
    Failed(ToiFailure),
}

/// The result of [`convex_distance`]: the closest points between two placed convex shapes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConvexDistance {
    /// Euclidean distance between the shapes in metres; `0.0` when they touch or overlap.
    /// Accurate to about 1e-6 m when the iteration closes its duality gap; when f32 noise
    /// stops it first (curved shapes, a few percent of random pairs), this is the proven
    /// lower bound instead, below the true distance by at most a few 1e-4 m (and below the
    /// witness-point gap by the same amount).
    pub distance: f32,
    /// World-space witness point on shape A (a closest point; inside B when overlapped).
    pub point_a: Vec3,
    /// World-space witness point on shape B.
    pub point_b: Vec3,
    /// Unit world-space direction from `point_a` toward `point_b`. When the shapes touch
    /// or overlap this falls back to the centre-to-centre direction (or `+X` when the
    /// centres coincide); check [`ConvexDistance::overlapped`] before relying on it.
    pub normal: Vec3,
    /// Whether the shapes interpenetrate (the origin lies inside the Minkowski difference,
    /// with a relative tolerance of 1e-6 of the shapes' extent, which is f32 resolution:
    /// touching within that tolerance may report either way).
    pub overlapped: bool,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Checks a shape's numbers: finite (else [`ToiFailure::NonFiniteInput`]), sizes
/// non-negative and hull non-empty (else [`ToiFailure::InvalidShape`]).
fn validate_shape(shape: &ConvexInput) -> Result<(), ToiFailure> {
    let size = |v: f32| -> Result<(), ToiFailure> {
        if !v.is_finite() {
            Err(ToiFailure::NonFiniteInput)
        } else if v < 0.0 {
            Err(ToiFailure::InvalidShape)
        } else {
            Ok(())
        }
    };
    let finite_vec = |v: Vec3| -> Result<(), ToiFailure> {
        if v.is_finite() {
            Ok(())
        } else {
            Err(ToiFailure::NonFiniteInput)
        }
    };
    match shape {
        ConvexInput::Point => Ok(()),
        ConvexInput::Sphere { radius } => size(*radius),
        ConvexInput::Box { half_extents } => {
            finite_vec(*half_extents)?;
            size(half_extents.x)?;
            size(half_extents.y)?;
            size(half_extents.z)
        }
        ConvexInput::Capsule {
            half_height, radius, ..
        }
        | ConvexInput::Cylinder {
            half_height, radius, ..
        }
        | ConvexInput::Cone {
            half_height, radius, ..
        } => {
            size(*half_height)?;
            size(*radius)
        }
        ConvexInput::Segment { a, b } => {
            finite_vec(*a)?;
            finite_vec(*b)
        }
        ConvexInput::ConvexHull { points } => {
            if points.is_empty() {
                return Err(ToiFailure::InvalidShape);
            }
            for p in points {
                finite_vec(*p)?;
            }
            Ok(())
        }
    }
}

/// Checks a pose and returns it with a normalised rotation.
fn validate_pose(xf: Isometry) -> Result<Isometry, ToiFailure> {
    if !xf.translation.is_finite() || !xf.rotation.is_finite() {
        return Err(ToiFailure::NonFiniteInput);
    }
    let len = xf.rotation.length();
    if !len.is_finite() || len < 1e-12 {
        return Err(ToiFailure::InvalidPose);
    }
    Ok(Isometry::new(xf.rotation.normalize(), xf.translation))
}

/// The (smallest, `<= pi`) angle between two unit quaternions, in radians. The deterministic
/// `acos` keeps the swept-angle bound (and so the reported time of impact) bit-stable across
/// platform libms (ADR-4773).
fn quat_angle_between(a: Quat, b: Quat) -> f32 {
    let d = a.dot(b).abs().clamp(0.0, 1.0);
    2.0 * det_acos(d)
}

// ---------------------------------------------------------------------------
// GJK distance (Gilbert, Johnson & Keerthi 1988)
// ---------------------------------------------------------------------------

/// One simplex vertex: a Minkowski-difference point `w = pa - pb` with its witnesses.
#[derive(Debug, Clone, Copy)]
struct SupportPoint {
    /// Minkowski-difference point `pa - pb`, world space.
    w: Vec3,
    /// Witness point on shape A, world space.
    pa: Vec3,
    /// Witness point on shape B, world space.
    pb: Vec3,
}

/// The support point of `A - B` along world direction `dir`.
fn support_difference(
    a: &ConvexInput,
    xf_a: Isometry,
    b: &ConvexInput,
    xf_b: Isometry,
    dir: Vec3,
) -> SupportPoint {
    let pa = a.support_point(xf_a, dir);
    let pb = b.support_point(xf_b, -dir);
    SupportPoint { w: pa - pb, pa, pb }
}

/// The closest point to the origin on a simplex, with barycentric weights over the kept
/// subsimplex. `indices[..count]`/`weights[..count]` are aligned.
struct SimplexClosest {
    /// Closest point on the simplex to the origin.
    point: Vec3,
    /// `|point|^2`, computed in f64 (the subsimplex solve runs in f64).
    dist_sq: f64,
    /// Indices into the input simplex of the kept vertices.
    indices: [usize; 4],
    /// Barycentric weights of the kept vertices (non-negative, sum to 1).
    weights: [f32; 4],
    /// Number of kept vertices (1 to 4).
    count: usize,
}

/// Clamps tiny negative barycentric weights to zero and renormalises them to sum to 1.
/// Returns `false` if the weights are unusable (non-finite or summing to ~0).
fn normalise_weights(weights: &mut [f64]) -> bool {
    let mut sum = 0.0_f64;
    for w in weights.iter_mut() {
        if !w.is_finite() {
            return false;
        }
        *w = w.max(0.0);
        sum += *w;
    }
    if sum <= 1e-20 {
        return false;
    }
    for w in weights.iter_mut() {
        *w /= sum;
    }
    true
}

/// The closest point to the origin on the convex hull of `simplex` (1 to 5 points: a
/// working tetrahedron plus the candidate just added), by exhaustive evaluation of
/// every subsimplex of up to 4 points. Smaller subsimplices win ties, which keeps the
/// working simplex minimal and the result deterministic.
fn closest_point_on_simplex(simplex: &[SupportPoint]) -> SimplexClosest {
    let n = simplex.len();
    let mut best = SimplexClosest {
        point: simplex[0].w,
        dist_sq: simplex[0].w.as_dvec3().length_squared(),
        indices: [0, 0, 0, 0],
        weights: [1.0, 0.0, 0.0, 0.0],
        count: 1,
    };
    let mut best_dist_sq = best.dist_sq;
    for size in 1..=n.min(4) {
        for mask in 1u32..(1u32 << n) {
            if mask.count_ones() as usize != size {
                continue;
            }
            let mut idx = [0usize; 4];
            let mut count = 0usize;
            for i in 0..n {
                if mask & (1u32 << i) != 0 {
                    idx[count] = i;
                    count += 1;
                }
            }
            let Some(candidate) = closest_on_subsimplex(simplex, &idx[..count]) else {
                continue;
            };
            let dist_sq = candidate.dist_sq;
            if dist_sq.is_finite() && dist_sq < best_dist_sq * (1.0 - 1e-12) {
                best_dist_sq = dist_sq;
                best = candidate;
            }
        }
    }
    best
}

/// The closest point to the origin on one subsimplex (point, segment, triangle face or
/// tetrahedron), or `None` when the affine-hull closest point falls outside the
/// subsimplex (a smaller subsimplex then holds the true closest point) or the
/// subsimplex is degenerate.
fn closest_on_subsimplex(simplex: &[SupportPoint], idx: &[usize]) -> Option<SimplexClosest> {
    // Solved in f64: the Gram-matrix and determinant terms below cancel badly in f32 for
    // thin triangles and flat tetrahedra, which rejected the right subsimplex and stalled
    // the iteration (found by the verification lane, ADR-2248).
    let w = |k: usize| simplex[idx[k]].w.as_dvec3();
    let w0 = w(0);
    let mut weights = [0.0_f64; 4];
    match idx.len() {
        1 => weights[0] = 1.0,
        2 => {
            let e = w(1) - w0;
            let denom = e.length_squared();
            let s = if denom > 0.0 { (-w0).dot(e) / denom } else { 0.0 }.clamp(0.0, 1.0);
            weights[0] = 1.0 - s;
            weights[1] = s;
        }
        3 => {
            let e0 = w(1) - w0;
            let e1 = w(2) - w0;
            let g00 = e0.dot(e0);
            let g01 = e0.dot(e1);
            let g11 = e1.dot(e1);
            let det = g00 * g11 - g01 * g01;
            if !det.is_finite() || det <= 1e-14 * g00 * g11 || det <= 0.0 {
                return None;
            }
            let r0 = -e0.dot(w0);
            let r1 = -e1.dot(w0);
            let u = (r0 * g11 - r1 * g01) / det;
            let v = (g00 * r1 - g01 * r0) / det;
            weights[0] = 1.0 - u - v;
            weights[1] = u;
            weights[2] = v;
            if weights[..3].iter().any(|w| *w < -1e-9) {
                return None;
            }
        }
        4 => {
            let e0 = w(1) - w0;
            let e1 = w(2) - w0;
            let e2 = w(3) - w0;
            let m = DMat3::from_cols(e0, e1, e2);
            let det = m.determinant();
            let scale = e0.length() * e1.length() * e2.length();
            if !det.is_finite() || scale <= 0.0 || det.abs() <= 1e-12 * scale {
                return None;
            }
            let x = m.inverse() * (-w0);
            weights[0] = 1.0 - x.x - x.y - x.z;
            weights[1] = x.x;
            weights[2] = x.y;
            weights[3] = x.z;
            if weights.iter().any(|w| *w < -1e-9) {
                return None;
            }
        }
        _ => return None,
    }
    if !normalise_weights(&mut weights[..idx.len()]) {
        return None;
    }
    let mut point = DVec3::ZERO;
    let mut indices = [0usize; 4];
    let mut weights_f32 = [0.0_f32; 4];
    for (k, &i) in idx.iter().enumerate() {
        point += simplex[i].w.as_dvec3() * weights[k];
        indices[k] = i;
        weights_f32[k] = weights[k] as f32;
    }
    if !point.is_finite() {
        return None;
    }
    Some(SimplexClosest {
        point: point.as_vec3(),
        dist_sq: point.length_squared(),
        indices,
        weights: weights_f32,
        count: idx.len(),
    })
}

/// GJK distance between two validated shapes at validated (normalised) poses.
///
/// # Errors
///
/// Returns [`ToiFailure::NumericalFailure`] if the iteration does not converge within its
/// internal budget (it does not guess a distance).
fn gjk_distance(
    a: &ConvexInput,
    xf_a: Isometry,
    b: &ConvexInput,
    xf_b: Isometry,
) -> Result<ConvexDistance, ToiFailure> {
    const MAX_GJK_ITERATIONS: u32 = 128;
    let center_a = xf_a.transform_point(a.local_center());
    let center_b = xf_b.transform_point(b.local_center());
    let mut dir = center_a - center_b;
    if dir.length_squared() < 1e-20 {
        dir = Vec3::X;
    }
    let first = support_difference(a, xf_a, b, xf_b, dir);
    if !first.w.is_finite() {
        return Err(ToiFailure::NumericalFailure);
    }
    // At most four kept points plus the support just added. Unused slots stay outside
    // every slice so they cannot change the scale or the duplicate test.
    let mut simplex = [first; 5];
    let mut simplex_len = 1usize;
    let mut closest = closest_point_on_simplex(&simplex[..simplex_len]);
    let mut converged = false;
    // The best lower bound on the distance seen so far (each `v·w/|v|` is one), and
    // whether the loop ended without closing the duality gap (stall or repeat).
    let mut best_lower = f32::NEG_INFINITY;
    let mut gap_open = false;
    for _ in 0..MAX_GJK_ITERATIONS {
        let v = closest.point;
        let dist = v.length();
        if !dist.is_finite() {
            return Err(ToiFailure::NumericalFailure);
        }
        // Zero test relative to the simplex size (van den Bergen 1999): in f32, an origin
        // on a face of the simplex lands about 1e-7 * |w| away from it, never at an
        // absolute 1e-9, so a deep overlap would stall instead of being detected.
        let scale = simplex[..simplex_len]
            .iter()
            .fold(0.0_f32, |m, s| m.max(s.w.length()));
        if dist <= 1e-6 * scale.max(1e-12) {
            converged = true;
            break;
        }
        let w = support_difference(a, xf_a, b, xf_b, -v);
        if !w.w.is_finite() {
            return Err(ToiFailure::NumericalFailure);
        }
        // Duality gap: the distance is at least the projected lower bound
        // `v·w / |v|` (from the support plane) and at most `|v|`. When the two are
        // within tolerance, `v` is the closest point. A repeated support point also
        // proves no further progress is possible.
        let lower_bound = v.dot(w.w) / dist;
        best_lower = best_lower.max(lower_bound);
        let closed = dist - lower_bound <= 1e-6 * dist.max(1.0);
        let duplicate = simplex[..simplex_len]
            .iter()
            .any(|s| (s.w - w.w).length_squared() <= 1e-14);
        if duplicate || closed {
            gap_open = !closed;
            converged = true;
            break;
        }
        simplex[simplex_len] = w;
        simplex_len += 1;
        let candidate = closest_point_on_simplex(&simplex[..simplex_len]);
        // Progress check (van den Bergen 1999): `|v|` must decrease (here by a relative
        // 5e-7, about f32 resolution; a strict check lets f32 creep run out the
        // iteration budget on curved shapes). Near
        // touching contact, f32 noise can make the subsimplex search drop the new
        // support point and return the same closest point again; without this check
        // the loop repeats that forever. No progress means `v` is as close as f32
        // allows, so keep the previous simplex and stop (the reported distance then
        // falls back to the lower bound, below).
        if candidate.dist_sq >= closest.dist_sq * (1.0 - 1e-6) {
            simplex_len -= 1;
            gap_open = true;
            converged = true;
            break;
        }
        closest = candidate;
        let count = closest.count;
        let mut kept = [simplex[0]; 4];
        for k in 0..count {
            kept[k] = simplex[closest.indices[k]];
        }
        simplex[..count].copy_from_slice(&kept[..count]);
        simplex_len = count;
        // Rebase the weights' indices onto the kept simplex (identity order).
        closest.indices = [0, 1, 2, 3];
    }
    if !converged {
        return Err(ToiFailure::NumericalFailure);
    }
    let mut point_a = Vec3::ZERO;
    let mut point_b = Vec3::ZERO;
    let mut min_weight = f32::INFINITY;
    for k in 0..closest.count {
        let s = simplex[closest.indices[k]];
        point_a += s.pa * closest.weights[k];
        point_b += s.pb * closest.weights[k];
        min_weight = min_weight.min(closest.weights[k]);
    }
    let v = closest.point;
    let v_len = v.length();
    // The origin strictly inside the kept subsimplex (which lies in the Minkowski
    // difference) means interpenetration; the origin merely on it means touching.
    let scale = simplex[..simplex_len]
        .iter()
        .fold(0.0_f32, |m, s| m.max(s.w.length()));
    let overlapped = v_len <= 1e-6 * scale.max(1e-12) && closest.count >= 2 && min_weight > 1e-4;
    // `|v|` is an upper bound on the distance. When the loop stopped with the duality
    // gap still open, report the proven lower bound instead (clamped at 0): conservative
    // advancement steps by this distance, and an overestimate could step past contact.
    let distance = if gap_open && best_lower < v_len {
        best_lower.max(0.0)
    } else {
        v_len
    };
    let normal = if v_len > 1e-9 {
        -v / v_len
    } else {
        let between = center_b - center_a;
        if between.length_squared() > 1e-20 {
            between.normalize_or_zero()
        } else {
            Vec3::X
        }
    };
    Ok(ConvexDistance {
        distance,
        point_a,
        point_b,
        normal,
        overlapped,
    })
}

/// The distance and witness points between two convex shapes at fixed poses (GJK).
///
/// This is the distance query [`time_of_impact`] advances over; it is useful on its own
/// for proximity checks and for verifying a TOI result.
///
/// # Errors
///
/// Returns a [`ToiFailure`] for non-finite inputs, invalid shapes or poses, or if the
/// GJK iteration fails to converge. Never panics.
///
/// # Example
///
/// ```
/// use regia_core::math::{Isometry, Vec3};
/// use regia_physics::ccd::{convex_distance, ConvexInput};
///
/// let ball = ConvexInput::sphere(0.5);
/// let block = ConvexInput::cuboid(Vec3::splat(0.5));
/// let Ok(d) = convex_distance(
///     &ball,
///     Isometry::from_translation(Vec3::new(-3.0, 0.0, 0.0)),
///     &block,
///     Isometry::IDENTITY,
/// ) else {
///     panic!("valid shapes must produce a distance");
/// };
/// assert!((d.distance - 2.0).abs() < 1e-5, "gap is 3 - 0.5 - 0.5: {}", d.distance);
/// assert!(!d.overlapped);
/// ```
pub fn convex_distance(
    a: &ConvexInput,
    xf_a: Isometry,
    b: &ConvexInput,
    xf_b: Isometry,
) -> Result<ConvexDistance, ToiFailure> {
    validate_shape(a)?;
    validate_shape(b)?;
    let xf_a = validate_pose(xf_a)?;
    let xf_b = validate_pose(xf_b)?;
    gjk_distance(a, xf_a, b, xf_b)
}

// ---------------------------------------------------------------------------
// Conservative advancement (Mirtich 1996; Redon, Kheddar & Coquillart 2002)
// ---------------------------------------------------------------------------

/// Builds a [`ToiResult::Hit`] from a distance result at time `t`.
fn hit_from(t: f32, d: &ConvexDistance, fallback_normal: Vec3) -> ToiResult {
    let mut normal = d.point_b - d.point_a;
    if normal.length_squared() > 1e-18 {
        normal = normal.normalize_or_zero();
    } else if d.distance > 1e-9 {
        normal = d.normal;
    } else {
        normal = fallback_normal;
    }
    if normal.length_squared() < 1e-18 || !normal.is_finite() {
        normal = Vec3::X;
    }
    ToiResult::Hit {
        t,
        normal,
        point: d.point_a,
    }
}

/// Shared conservative-advancement core. `angle_bound_*`, when present, raise (never
/// lower) the endpoint slerp angle as the bound on swept angle per unit `t`.
#[allow(clippy::too_many_arguments)] // the signature the CCD task specified (ADR-2248)
fn toi_impl(
    a: &ConvexInput,
    xf_a0: Isometry,
    xf_a1: Isometry,
    b: &ConvexInput,
    xf_b0: Isometry,
    xf_b1: Isometry,
    angle_bound_a: Option<f32>,
    angle_bound_b: Option<f32>,
    target_gap: f32,
    tolerance: f32,
    max_iterations: u32,
) -> ToiResult {
    if !target_gap.is_finite() || !tolerance.is_finite() {
        return ToiResult::Failed(ToiFailure::NonFiniteInput);
    }
    if target_gap < 0.0 || tolerance < 0.0 || max_iterations == 0 {
        return ToiResult::Failed(ToiFailure::InvalidParameter);
    }
    for bound in [angle_bound_a, angle_bound_b].into_iter().flatten() {
        if !bound.is_finite() {
            return ToiResult::Failed(ToiFailure::NonFiniteInput);
        }
        if bound < 0.0 {
            return ToiResult::Failed(ToiFailure::InvalidParameter);
        }
    }
    if let Err(e) = validate_shape(a) {
        return ToiResult::Failed(e);
    }
    if let Err(e) = validate_shape(b) {
        return ToiResult::Failed(e);
    }
    let (xf_a0, xf_a1, xf_b0, xf_b1) = match (
        validate_pose(xf_a0),
        validate_pose(xf_a1),
        validate_pose(xf_b0),
        validate_pose(xf_b1),
    ) {
        (Ok(p), Ok(q), Ok(r), Ok(s)) => (p, q, r, s),
        (Err(e), _, _, _) | (_, Err(e), _, _) | (_, _, Err(e), _) | (_, _, _, Err(e)) => {
            return ToiResult::Failed(e);
        }
    };
    let tol = tolerance.max(1e-7);
    let radius_a = a.bounding_radius();
    let radius_b = b.bounding_radius();
    if !radius_a.is_finite() || !radius_b.is_finite() {
        return ToiResult::Failed(ToiFailure::NonFiniteInput);
    }
    let omega_a = match angle_bound_a {
        Some(bound) => bound.max(quat_angle_between(xf_a0.rotation, xf_a1.rotation)),
        None => quat_angle_between(xf_a0.rotation, xf_a1.rotation),
    };
    let omega_b = match angle_bound_b {
        Some(bound) => bound.max(quat_angle_between(xf_b0.rotation, xf_b1.rotation)),
        None => quat_angle_between(xf_b0.rotation, xf_b1.rotation),
    };
    let vel_rel = (xf_b1.translation - xf_b0.translation) - (xf_a1.translation - xf_a0.translation);
    if !vel_rel.is_finite() {
        // Finite poses whose difference overflows f32 (ADR-2423): the closing term would be
        // infinite or NaN, and `max(0, -inf)` would read as "no motion".
        return ToiResult::Failed(ToiFailure::DegenerateMotion);
    }
    let fallback_normal = {
        let between = xf_b0.translation - xf_a0.translation;
        if between.length_squared() > 1e-20 {
            between.normalize_or_zero()
        } else if vel_rel.length_squared() > 1e-20 {
            (-vel_rel).normalize_or_zero()
        } else {
            Vec3::X
        }
    };
    let eval_at = |t: f32| -> Result<ConvexDistance, ToiFailure> {
        gjk_distance(a, xf_a0.interpolate(&xf_a1, t), b, xf_b0.interpolate(&xf_b1, t))
    };

    let mut t = 0.0_f32;
    let mut t_safe = 0.0_f32;
    for _ in 0..max_iterations {
        let d = match eval_at(t) {
            Ok(d) => d,
            Err(e) => return ToiResult::Failed(e),
        };
        if d.overlapped {
            if t <= 0.0 {
                return ToiResult::OverlappedAtStart;
            }
            // Numerical overshoot past contact: refine between the last safe time
            // and here instead of reporting a tunneled hit.
            return refine_hit(&eval_at, t_safe, t, target_gap, tol, fallback_normal);
        }
        let excess = d.distance - target_gap;
        if excess <= tol {
            if t <= 0.0 || excess >= -tol {
                return hit_from(t, &d, fallback_normal);
            }
            return refine_hit(&eval_at, t_safe, t, target_gap, tol, fallback_normal);
        }
        if t >= 1.0 {
            return ToiResult::Separated;
        }
        t_safe = t;
        // Bound on how fast separation along the fixed witness axis can shrink.
        // The true distance is bounded below by that projected separation, so
        // advancing by `excess / closing` cannot pass `target_gap`.
        let closing = (-d.normal.dot(vel_rel)).max(0.0) + omega_a * radius_a + omega_b * radius_b;
        if !closing.is_finite() {
            // A NaN or overflowed bound cannot bound anything: refuse (ADR-2423). Before
            // that ADR a NaN closing speed was read as "no motion" and reported `Separated`.
            return ToiResult::Failed(ToiFailure::DegenerateMotion);
        }
        if closing <= 1e-12 {
            // No closing motion along this axis and no rotation: the separating
            // plane persists for the rest of the sweep.
            return ToiResult::Separated;
        }
        // Aim at the middle of the accepted band `[target_gap, target_gap + tol]`, not
        // at `target_gap` itself: landing exactly on contact (`target_gap = 0`) leaves
        // a separation at f32 noise level, where the witness points (and so the hit
        // normal) are meaningless and a shallow overlap can flip the normal.
        let dt = (excess - 0.5 * tol) / closing;
        let t_next = if dt.is_finite() { (t + dt).min(1.0) } else { 1.0 };
        if t_next <= t + 1e-9 && t_next < 1.0 {
            return ToiResult::Failed(ToiFailure::NumericalFailure);
        }
        t = t_next;
    }
    ToiResult::Failed(ToiFailure::MaxIterations)
}

/// Bisects `[t_lo, t_hi]` (separated at `t_lo`, past `target_gap` at `t_hi`) for the
/// first time the separation reaches `target_gap` within `tol`. Used only when
/// floating-point error overshoots an advance; bounded to 48 evaluations.
fn refine_hit(
    eval_at: &dyn Fn(f32) -> Result<ConvexDistance, ToiFailure>,
    t_lo: f32,
    t_hi: f32,
    target_gap: f32,
    tol: f32,
    fallback_normal: Vec3,
) -> ToiResult {
    let mut lo = t_lo;
    let mut hi = t_hi;
    for _ in 0..48 {
        let mid = 0.5 * (lo + hi);
        let d = match eval_at(mid) {
            Ok(d) => d,
            Err(e) => return ToiResult::Failed(e),
        };
        if !d.overlapped && (d.distance - target_gap).abs() <= tol {
            return hit_from(mid, &d, fallback_normal);
        }
        if d.overlapped || d.distance < target_gap {
            hi = mid;
        } else {
            lo = mid;
        }
        if hi - lo <= 1e-7 {
            break;
        }
    }
    match eval_at(lo) {
        Ok(d) => hit_from(lo, &d, fallback_normal),
        Err(e) => ToiResult::Failed(e),
    }
}

/// Time of impact between two convex shapes moving along linear paths.
///
/// Shape A moves from pose `xf_a0` to `xf_a1` and shape B from `xf_b0` to `xf_b1` as the
/// normalised time goes from 0 to 1 (translation lerped, rotation slerped). Returns the
/// first `t` at which the shapes are `target_gap` apart (within `tolerance` metres of
/// separation), [`ToiResult::Separated`] if they never get that close,
/// [`ToiResult::OverlappedAtStart`] if they already interpenetrate at `t = 0`, or
/// [`ToiResult::Failed`] for invalid input or an exhausted `max_iterations` budget.
///
/// Rotation between the endpoint poses is accounted for conservatively: the swept angle
/// of each shape (from its endpoint orientations) enters the advance bound through its
/// bounding radius, so a spinning shape cannot tunnel either. When the caller knows a
/// looser bound on the swept angle than the endpoints imply, use
/// [`time_of_impact_angular`].
///
/// `max_iterations` bounds the number of conservative advances (32–128 is ample for
/// ordinary gameplay sweeps; grazing near-misses with a very small `tolerance` need
/// more). Invalid input of any kind — NaN or infinite numbers, negative sizes, empty
/// hulls, zero quaternions, negative `target_gap`/`tolerance`, `max_iterations == 0` —
/// returns `Failed`, never panics.
///
/// # Example
///
/// ```
/// use regia_core::math::{Isometry, Vec3};
/// use regia_physics::ccd::{time_of_impact, ConvexInput, ToiResult};
///
/// // A 10 cm sphere flying 10 m in one step at a 1 cm wall: it must not tunnel.
/// let ball = ConvexInput::sphere(0.1);
/// let wall = ConvexInput::cuboid(Vec3::new(0.005, 2.0, 2.0));
/// let hit = time_of_impact(
///     &ball,
///     Isometry::from_translation(Vec3::new(-5.0, 0.0, 0.0)),
///     Isometry::from_translation(Vec3::new(5.0, 0.0, 0.0)),
///     &wall,
///     Isometry::IDENTITY,
///     Isometry::IDENTITY,
///     0.0,
///     1e-4,
///     64,
/// );
/// match hit {
///     ToiResult::Hit { t, normal, .. } => {
///         // Contact when the centre reaches x = -(0.005 + 0.1): t = 4.895 / 10.
///         assert!((t - 0.4895).abs() < 1e-3, "t = {t}");
///         assert!(normal.dot(Vec3::X) > 0.999, "normal A->B is +X: {normal}");
///     }
///     other => panic!("expected a hit, got {other:?}"),
/// }
/// ```
#[allow(clippy::too_many_arguments)] // the signature the CCD task specified (ADR-2248)
pub fn time_of_impact(
    a: &ConvexInput,
    xf_a0: Isometry,
    xf_a1: Isometry,
    b: &ConvexInput,
    xf_b0: Isometry,
    xf_b1: Isometry,
    target_gap: f32,
    tolerance: f32,
    max_iterations: u32,
) -> ToiResult {
    toi_impl(
        a,
        xf_a0,
        xf_a1,
        b,
        xf_b0,
        xf_b1,
        None,
        None,
        target_gap,
        tolerance,
        max_iterations,
    )
}

/// Time of impact with caller-supplied bounds on each shape's swept rotation angle.
///
/// Identical to [`time_of_impact`], except the angular term of the advance bound uses
/// the larger of `angle_bound_a`/`angle_bound_b` (radians swept over the whole interval)
/// and the angle between the endpoint orientations, so a bound can only ever make the
/// sweep more conservative than the endpoints imply. Pass a bound at least as large as the
/// angle the shape actually turns through on its slerp path (for example
/// `|angular_velocity| * dt` from a physics step, which overestimates the slerp angle
/// when the step also integrates other motion). A larger bound only makes the steps
/// smaller — the reported hit still satisfies the `target_gap`/`tolerance` contract at
/// the interpolated poses — so overestimating is safe and underestimating is a caller
/// error. This does **not** model multi-turn spins whose intermediate orientations leave
/// the slerp path between the endpoints; subdivide the interval for those.
///
/// # Example
///
/// ```
/// use regia_core::math::{Isometry, Quat, Vec3};
/// use regia_physics::ccd::{time_of_impact_angular, ConvexInput, ToiResult};
///
/// // A 2 m thin rod spinning a half turn about its centre, past a pin at (0, 0.9, 0).
/// let rod = ConvexInput::cuboid(Vec3::new(1.0, 0.01, 0.01));
/// let pin = ConvexInput::sphere(0.05);
/// let pin_pose = Isometry::from_translation(Vec3::new(0.0, 0.9, 0.0));
/// let hit = time_of_impact_angular(
///     &rod,
///     Isometry::IDENTITY,
///     Isometry::from_rotation(Quat::from_rotation_z(core::f32::consts::PI)),
///     &pin,
///     pin_pose,
///     pin_pose,
///     core::f32::consts::PI,
///     0.0,
///     0.0,
///     1e-4,
///     128,
/// );
/// assert!(matches!(hit, ToiResult::Hit { .. }), "rod sweeps the pin: {hit:?}");
/// ```
#[allow(clippy::too_many_arguments)] // the signature the CCD task specified (ADR-2248)
pub fn time_of_impact_angular(
    a: &ConvexInput,
    xf_a0: Isometry,
    xf_a1: Isometry,
    b: &ConvexInput,
    xf_b0: Isometry,
    xf_b1: Isometry,
    angle_bound_a: f32,
    angle_bound_b: f32,
    target_gap: f32,
    tolerance: f32,
    max_iterations: u32,
) -> ToiResult {
    toi_impl(
        a,
        xf_a0,
        xf_a1,
        b,
        xf_b0,
        xf_b1,
        Some(angle_bound_a),
        Some(angle_bound_b),
        target_gap,
        tolerance,
        max_iterations,
    )
}

/// Sweeps a sphere from `center0` to `center1` against a convex shape sweeping from
/// `xf_b0` to `xf_b1`. A convenience wrapper over [`time_of_impact`] for projectiles,
/// debris and probe casts; the sphere is shape A, so the hit `point` is on the sphere
/// and `normal` points from the sphere toward the other shape.
#[allow(clippy::too_many_arguments)] // the signature the CCD task specified (ADR-2248)
pub fn sphere_sweep(
    center0: Vec3,
    center1: Vec3,
    radius: f32,
    b: &ConvexInput,
    xf_b0: Isometry,
    xf_b1: Isometry,
    target_gap: f32,
    tolerance: f32,
    max_iterations: u32,
) -> ToiResult {
    if !center0.is_finite() || !center1.is_finite() || !radius.is_finite() {
        return ToiResult::Failed(ToiFailure::NonFiniteInput);
    }
    let sphere = ConvexInput::sphere(radius);
    time_of_impact(
        &sphere,
        Isometry::from_translation(center0),
        Isometry::from_translation(center1),
        b,
        xf_b0,
        xf_b1,
        target_gap,
        tolerance,
        max_iterations,
    )
}

/// Sweeps a box from `xf_box0` to `xf_box1` against a static thin wall (a box of
/// `wall_half_extents` at pose `xf_wall`). A convenience wrapper over
/// [`time_of_impact`] for the classic fast-box-through-thin-wall case (vehicles, crates,
/// character proxies); the moving box is shape A. For a moving wall, call
/// [`time_of_impact`] directly.
#[allow(clippy::too_many_arguments)] // the signature the CCD task specified (ADR-2248)
pub fn swept_box_vs_thin_wall(
    box_half_extents: Vec3,
    xf_box0: Isometry,
    xf_box1: Isometry,
    wall_half_extents: Vec3,
    xf_wall: Isometry,
    target_gap: f32,
    tolerance: f32,
    max_iterations: u32,
) -> ToiResult {
    if !box_half_extents.is_finite() || !wall_half_extents.is_finite() {
        return ToiResult::Failed(ToiFailure::NonFiniteInput);
    }
    let moving = ConvexInput::cuboid(box_half_extents);
    let wall = ConvexInput::cuboid(wall_half_extents);
    time_of_impact(
        &moving,
        xf_box0,
        xf_box1,
        &wall,
        xf_wall,
        xf_wall,
        target_gap,
        tolerance,
        max_iterations,
    )
}

// ---------------------------------------------------------------------------
// Angular sweep: conservative advancement along a constant-velocity rigid motion
// (ADR-2423; Mirtich 1996, with a directional angular bound derived below)
// ---------------------------------------------------------------------------

/// A rigid motion over the normalized interval `t ∈ [0, 1]`: the shape starts at `start`
/// and its rotation centre (`local_center`, in the shape's local frame, usually the body's
/// centre of mass) moves by `linear` while the shape turns about that centre by the
/// rotation vector `angular` (world axis times angle in radians, any magnitude: several
/// turns are fine). This is the motion a physics step integrates with constant velocities,
/// so a body spinning faster than half a turn per step is swept along its real path, not
/// along the shortest rotation between its endpoint poses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RigidMotion {
    /// The pose at `t = 0`.
    pub start: Isometry,
    /// The rotation centre in the shape's local frame.
    pub local_center: Vec3,
    /// Displacement of the rotation centre over the whole interval, world space (metres).
    pub linear: Vec3,
    /// Rotation over the whole interval about the moving centre, as a world-space
    /// rotation vector (unit axis times radians).
    pub angular: Vec3,
}

impl RigidMotion {
    /// A shape that does not move: `pose` for the whole interval.
    pub const fn stationary(pose: Isometry) -> Self {
        Self {
            start: pose,
            local_center: Vec3::ZERO,
            linear: Vec3::ZERO,
            angular: Vec3::ZERO,
        }
    }

    /// The motion of a body over a step of `dt` seconds with constant linear velocity
    /// `linvel` (of the centre, m/s) and angular velocity `angvel` (world space, rad/s).
    pub fn from_velocities(start: Isometry, local_center: Vec3, linvel: Vec3, angvel: Vec3, dt: f32) -> Self {
        Self {
            start,
            local_center,
            linear: linvel * dt,
            angular: angvel * dt,
        }
    }

    /// The pose at normalized time `t`: the centre moves linearly, the rotation is
    /// `exp(angular * t) * start.rotation` about the centre. `exp` uses the deterministic
    /// `sin`/`cos` kernels so the sampled poses — and every witness distance and reported
    /// impact time built on them — are bit-identical on every target (ADR-4773).
    pub fn pose_at(&self, t: f32) -> Isometry {
        let center0 = self.start.transform_point(self.local_center);
        let turn = det_quat_from_scaled_axis(self.angular * t);
        let rotation = (turn * self.start.rotation).normalize();
        let center = center0 + self.linear * t;
        Isometry::new(rotation, center - rotation * self.local_center)
    }

    /// Checks every number and normalizes the start rotation.
    fn validated(&self) -> Result<Self, ToiFailure> {
        if !self.local_center.is_finite() || !self.linear.is_finite() || !self.angular.is_finite() {
            return Err(ToiFailure::NonFiniteInput);
        }
        Ok(Self {
            start: validate_pose(self.start)?,
            ..*self
        })
    }
}

/// Tuning for [`time_of_impact_motion`]: the same meaning as the scalar parameters of
/// [`time_of_impact`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToiOptions {
    /// Separation at which the sweep stops (a skin width), metres, `>= 0`.
    pub target_gap: f32,
    /// Accepted band above `target_gap`, metres, `>= 0`.
    pub tolerance: f32,
    /// Maximum conservative advances, `>= 1`.
    pub max_iterations: u32,
}

impl Default for ToiOptions {
    /// Surface contact, 0.1 mm band, 128 advances.
    fn default() -> Self {
        Self {
            target_gap: 0.0,
            tolerance: 1e-4,
            max_iterations: 128,
        }
    }
}

/// The largest distance from `center` (local space) to any point of `shape`: exact for
/// points, segments and hulls, `|center| + bounding_radius` (an upper bound) otherwise.
pub(crate) fn radius_about(shape: &ConvexInput, center: Vec3) -> f32 {
    match shape {
        ConvexInput::Point => center.length(),
        ConvexInput::Segment { a, b } => (*a - center).length().max((*b - center).length()),
        ConvexInput::ConvexHull { points } => {
            points.iter().fold(0.0_f32, |m, p| m.max((*p - center).length()))
        }
        ConvexInput::Box { half_extents } => {
            // The farthest box corner from `center`.
            let far = Vec3::new(
                half_extents.x + center.x.abs(),
                half_extents.y + center.y.abs(),
                half_extents.z + center.z.abs(),
            );
            far.length()
        }
        _ => center.length() + shape.bounding_radius(),
    }
}

/// Time of impact between two convex shapes moving along constant-velocity rigid motions,
/// rotation included (a true angular sweep, ADR-2423).
///
/// Unlike [`time_of_impact`], which slerps between two endpoint poses (so it cannot see a
/// spin of more than half a turn, or a rotation about a centre other than the pose origin),
/// this follows each [`RigidMotion`] exactly: the centre of rotation translates linearly
/// and the shape turns about it at a constant angular velocity, any number of turns.
///
/// The advance is conservative advancement (Mirtich 1996). At the current `t` the closest
/// points give the distance `d` and the unit normal `n` (A toward B). Along the fixed axis
/// `n`, a point `p` of A moves at `v_a + ω_a × r` with `r = p − c_a(t)`, `|r| <= R_a`, so
/// `n · ṗ = n · v_a + r · (n × ω_a) <= n · v_a + |n × ω_a| R_a`; the same holds for B with
/// the signs swapped. The separation along `n`, a lower bound on the distance, therefore
/// shrinks no faster than `closing = max(0, n · (v_a − v_b)) + |n × ω_a| R_a + |n × ω_b| R_b`,
/// and advancing `t` by `(d − target_gap) / closing` cannot pass contact. The `|n × ω|`
/// factor (rather than `|ω|`) means a spin about the contact normal costs nothing.
///
/// Refusals, never loops: every number is validated (NaN or infinity →
/// [`ToiFailure::NonFiniteInput`]); a closing bound that is not finite, or so large that an
/// advance does not move `t` at f32 resolution, is [`ToiFailure::DegenerateMotion`];
/// otherwise the work is bounded by `options.max_iterations` advances plus at most 48
/// bisection steps ([`ToiFailure::MaxIterations`] when the budget runs out). A zero closing
/// speed (no relative motion toward each other) is a proof of separation for the rest of the
/// interval, so it returns [`ToiResult::Separated`].
///
/// Deterministic: the same inputs give bit-identical results on one platform. Poses use
/// `sin`/`cos`, whose last bit may differ between platform maths libraries.
///
/// # Example
///
/// ```
/// use regia_core::math::{Isometry, Vec3};
/// use regia_physics::ccd::{time_of_impact_motion, ConvexInput, RigidMotion, ToiOptions, ToiResult};
///
/// // A 2 m bar along X spinning three whole turns about Z in one step, under a thin
/// // slab whose lower face is at y = 0.49. Its endpoint poses are identical, so an
/// // endpoint slerp sees no rotation at all; the real path hits within a quarter turn.
/// let bar = ConvexInput::cuboid(Vec3::new(1.0, 0.02, 0.02));
/// let spin = RigidMotion {
///     start: Isometry::IDENTITY,
///     local_center: Vec3::ZERO,
///     linear: Vec3::ZERO,
///     angular: Vec3::new(0.0, 0.0, 6.0 * core::f32::consts::PI),
/// };
/// let slab = ConvexInput::cuboid(Vec3::new(2.0, 0.01, 2.0));
/// let slab_pose = Isometry::from_translation(Vec3::new(0.0, 0.5, 0.0));
/// let hit = time_of_impact_motion(&bar, &spin, &slab, &RigidMotion::stationary(slab_pose), ToiOptions::default());
/// let ToiResult::Hit { t, .. } = hit else { panic!("the spinning bar must hit the slab: {hit:?}") };
/// assert!(t < 1.0 / 12.0, "within the first quarter turn: t = {t}");
/// ```
pub fn time_of_impact_motion(
    a: &ConvexInput,
    motion_a: &RigidMotion,
    b: &ConvexInput,
    motion_b: &RigidMotion,
    options: ToiOptions,
) -> ToiResult {
    let ToiOptions {
        target_gap,
        tolerance,
        max_iterations,
    } = options;
    if !target_gap.is_finite() || !tolerance.is_finite() {
        return ToiResult::Failed(ToiFailure::NonFiniteInput);
    }
    if target_gap < 0.0 || tolerance < 0.0 || max_iterations == 0 {
        return ToiResult::Failed(ToiFailure::InvalidParameter);
    }
    if let Err(e) = validate_shape(a).and_then(|()| validate_shape(b)) {
        return ToiResult::Failed(e);
    }
    let (ma, mb) = match (motion_a.validated(), motion_b.validated()) {
        (Ok(ma), Ok(mb)) => (ma, mb),
        (Err(e), _) | (_, Err(e)) => return ToiResult::Failed(e),
    };
    let tol = tolerance.max(1e-7);
    let radius_a = radius_about(a, ma.local_center);
    let radius_b = radius_about(b, mb.local_center);
    let vel_rel = mb.linear - ma.linear;
    if !radius_a.is_finite() || !radius_b.is_finite() || !vel_rel.is_finite() {
        return ToiResult::Failed(ToiFailure::DegenerateMotion);
    }
    let fallback_normal = {
        let between = mb.start.translation - ma.start.translation;
        if between.length_squared() > 1e-20 {
            between.normalize_or_zero()
        } else if vel_rel.length_squared() > 1e-20 {
            (-vel_rel).normalize_or_zero()
        } else {
            Vec3::X
        }
    };
    let eval_at =
        |t: f32| -> Result<ConvexDistance, ToiFailure> { gjk_distance(a, ma.pose_at(t), b, mb.pose_at(t)) };

    let mut t = 0.0_f32;
    let mut t_safe = 0.0_f32;
    for _ in 0..max_iterations {
        let d = match eval_at(t) {
            Ok(d) => d,
            Err(e) => return ToiResult::Failed(e),
        };
        if d.overlapped {
            if t <= 0.0 {
                return ToiResult::OverlappedAtStart;
            }
            return refine_hit(&eval_at, t_safe, t, target_gap, tol, fallback_normal);
        }
        let excess = d.distance - target_gap;
        if excess <= tol {
            if t <= 0.0 || excess >= -tol {
                return hit_from(t, &d, fallback_normal);
            }
            return refine_hit(&eval_at, t_safe, t, target_gap, tol, fallback_normal);
        }
        if t >= 1.0 {
            return ToiResult::Separated;
        }
        t_safe = t;
        let n = d.normal;
        let closing = (-n.dot(vel_rel)).max(0.0)
            + n.cross(ma.angular).length() * radius_a
            + n.cross(mb.angular).length() * radius_b;
        if !closing.is_finite() {
            return ToiResult::Failed(ToiFailure::DegenerateMotion);
        }
        if closing <= 1e-12 {
            // Nothing moves toward the other along the fixed axis `n`: the separation along
            // it can only grow, so the distance stays above `target_gap` to `t = 1`.
            return ToiResult::Separated;
        }
        let dt = (excess - 0.5 * tol) / closing;
        let t_next = if dt.is_finite() { (t + dt).min(1.0) } else { 1.0 };
        if t_next <= t {
            // The advance underflows f32 time resolution: the closing bound is too large
            // for the gap left. Refuse instead of spinning out the budget.
            return ToiResult::Failed(ToiFailure::DegenerateMotion);
        }
        t = t_next;
    }
    ToiResult::Failed(ToiFailure::MaxIterations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    // --- ADR-4773 determinism pins (audit G finding 2): swept poses, the endpoint angle and
    // the slerp samples are bit-identical on every host. The quaternions are literal (no libm
    // in the test itself) so the pins isolate the code under test.
    const POSE_AT_ROT_BITS: [u32; 4] = [0, 1_041_508_787, 0, 1_065_176_669];
    const POSE_AT_TRANSLATION_BITS: [u32; 3] = [1_046_696_082, 0, 1_022_005_479];
    const ANGLE_BETWEEN_BITS: u32 = 1_065_686_846;
    const SLERP_QUAT_BITS: [u32; 4] = [1_033_679_770, 3_189_552_026, 1_054_079_642, 1_063_575_564];
    /// `(t, normal.xyz, point.xyz)` of the three-turn spinning bar below, pinned (ADR-4773).
    const SPIN_BAR_HIT_BITS: [u32; 7] = [
        1_020_642_573,
        3_123_680_508,
        1_065_353_208,
        976_196_860,
        1_063_205_762,
        1_056_626_078,
        0,
    ];

    /// Rotation and translation bits of `motion.pose_at(0.37)` for the motion below, pinned.
    #[test]
    fn pose_at_uses_deterministic_rotation_bits() {
        let motion = RigidMotion {
            start: tr(0.0, 0.0, 0.0),
            local_center: Vec3::new(0.1, 0.0, 0.0),
            linear: Vec3::new(1.5, 0.0, 0.0),
            angular: Vec3::new(0.0, 2.0, 0.0),
        };
        // 0.1452: an angle where platform libm sin and the deterministic kernel are one ulp
        // apart after normalization (scanned), so the pin binds on every host.
        let pose = motion.pose_at(0.1452);
        let bits = [
            pose.rotation.x.to_bits(),
            pose.rotation.y.to_bits(),
            pose.rotation.z.to_bits(),
            pose.rotation.w.to_bits(),
        ];
        assert_eq!(
            bits, POSE_AT_ROT_BITS,
            "from_scaled_axis must use the deterministic sin/cos kernels"
        );
        let translation = [
            pose.translation.x.to_bits(),
            pose.translation.y.to_bits(),
            pose.translation.z.to_bits(),
        ];
        assert_eq!(
            translation, POSE_AT_TRANSLATION_BITS,
            "translation is rotation * -local_center plus the linear advance"
        );
    }

    #[test]
    fn angle_between_and_slerp_use_deterministic_kernels() {
        use core::f32::consts::FRAC_1_SQRT_2;
        let a = Quat::from_xyzw(0.1, -0.2, 0.3, 0.927_361_85);
        let b = Quat::from_xyzw(0.0, 0.0, FRAC_1_SQRT_2, FRAC_1_SQRT_2);
        assert_eq!(
            quat_angle_between(a, b).to_bits(),
            ANGLE_BETWEEN_BITS,
            "the swept-angle bound must not depend on the platform acos"
        );
        let mid = Isometry::new(a, Vec3::new(1.0, 2.0, 3.0))
            .interpolate(&Isometry::new(b, Vec3::new(-2.0, 0.5, 4.0)), 0.25);
        assert_eq!(
            [
                mid.rotation.x.to_bits(),
                mid.rotation.y.to_bits(),
                mid.rotation.z.to_bits(),
                mid.rotation.w.to_bits(),
            ],
            SLERP_QUAT_BITS,
            "the time-of-impact samples go through this slerp"
        );
        assert_eq!(
            mid.translation,
            Vec3::new(1.0, 2.0, 3.0).lerp(Vec3::new(-2.0, 0.5, 4.0), 0.25)
        );
    }

    /// A translation-only pose.
    fn tr(x: f32, y: f32, z: f32) -> Isometry {
        Isometry::from_translation(Vec3::new(x, y, z))
    }

    /// Unwraps a hit by hand (tests follow the crate's no-`unwrap` rule too).
    fn expect_hit(result: ToiResult) -> (f32, Vec3, Vec3) {
        match result {
            ToiResult::Hit { t, normal, point } => (t, normal, point),
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    /// Unwraps a distance result by hand.
    fn expect_distance(result: Result<ConvexDistance, ToiFailure>) -> ConvexDistance {
        match result {
            Ok(d) => d,
            Err(e) => panic!("expected a distance, got {e:?}"),
        }
    }

    /// A tiny deterministic PRNG (xorshift64*) with an explicit seed, for the
    /// property test. No wall clock, no entropy.
    struct XorShift(u64);

    impl XorShift {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// A value in `[lo, hi)`.
        fn range(&mut self, lo: f32, hi: f32) -> f32 {
            let unit = (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
            lo + unit * (hi - lo)
        }
    }

    #[test]
    fn distance_sphere_box_face_is_analytic() {
        let d = expect_distance(convex_distance(
            &ConvexInput::sphere(0.5),
            tr(-3.0, 0.0, 0.0),
            &ConvexInput::cuboid(Vec3::splat(0.5)),
            Isometry::IDENTITY,
        ));
        assert!((d.distance - 2.0).abs() < 1e-5, "distance {}", d.distance);
        assert!((d.point_a - Vec3::new(-2.5, 0.0, 0.0)).length() < 5e-4);
        assert!((d.point_b - Vec3::new(-0.5, 0.0, 0.0)).length() < 5e-4);
        assert!(d.normal.dot(Vec3::X) > 0.999);
        assert!(!d.overlapped);
    }

    #[test]
    fn distance_box_box_corner_is_analytic() {
        // Two unit cubes offset by (2, 2, 0): closest features are corners, the gap
        // vector is (1, 1, 0), so the distance is sqrt(2).
        let d = expect_distance(convex_distance(
            &ConvexInput::cuboid(Vec3::splat(0.5)),
            Isometry::IDENTITY,
            &ConvexInput::cuboid(Vec3::splat(0.5)),
            tr(2.0, 2.0, 0.0),
        ));
        assert!(
            (d.distance - core::f32::consts::SQRT_2).abs() < 1e-5,
            "distance {}",
            d.distance
        );
        assert!((d.point_a.x - 0.5).abs() < 1e-4 && (d.point_a.y - 0.5).abs() < 1e-4);
        assert!((d.point_b.x - 1.5).abs() < 1e-4 && (d.point_b.y - 1.5).abs() < 1e-4);
        let expected = Vec3::new(1.0, 1.0, 0.0).normalize_or_zero();
        assert!(d.normal.dot(expected) > 0.999, "normal {}", d.normal);
    }

    #[test]
    fn distance_overlapping_spheres_reports_overlap() {
        let d = expect_distance(convex_distance(
            &ConvexInput::sphere(1.0),
            Isometry::IDENTITY,
            &ConvexInput::sphere(1.0),
            tr(0.5, 0.0, 0.0),
        ));
        assert_eq!(d.distance, 0.0);
        assert!(d.overlapped, "centres 0.5 apart, radii 1 + 1: deep overlap");
    }

    #[test]
    fn distance_touching_spheres_is_not_overlap() {
        let d = expect_distance(convex_distance(
            &ConvexInput::sphere(1.0),
            Isometry::IDENTITY,
            &ConvexInput::sphere(1.0),
            tr(3.0, 0.0, 0.0),
        ));
        assert!((d.distance - 1.0).abs() < 1e-5);
        assert!(!d.overlapped);
    }

    #[test]
    fn fast_sphere_does_not_tunnel_through_1cm_wall_at_500_mps() {
        // 500 m/s over a 20 ms step is 10 m of travel, straight through a 1 cm wall.
        // A discrete test at either endpoint sees empty space; the sweep must not.
        let ball = ConvexInput::sphere(0.1);
        let wall = ConvexInput::cuboid(Vec3::new(0.005, 2.0, 2.0));
        let (t, normal, point) = expect_hit(time_of_impact(
            &ball,
            tr(-5.0, 0.0, 0.0),
            tr(5.0, 0.0, 0.0),
            &wall,
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.0,
            1e-4,
            64,
        ));
        // Contact when the centre is at x = -(0.005 + 0.1) = -0.105: t = 4.895 / 10.
        assert!((t - 0.4895).abs() < 1e-3, "t = {t}");
        assert!(t < 0.5, "hit must be on the near face, not the far side: t = {t}");
        assert!(normal.dot(Vec3::X) > 0.999, "normal {normal}");
        assert!(
            (point.x + 0.005).abs() < 2e-3,
            "witness on the wall face: {point}"
        );
    }

    #[test]
    fn box_passing_box_corner_matches_analytic_t() {
        // Two axis-aligned cubes (half 0.5). B sits at the origin. A's centre travels
        // p(t) = (-3, 2, 0) + t*(6, -3, 0). The Minkowski sum is the cube of half 1
        // around B, and p enters it exactly through its corner (-1, 1, *):
        // p_x = -1 at t = 2/6 = 1/3, where p_y = 2 - 3*(1/3) = 1. So first contact is
        // corner-to-corner at t = 1/3, with gap closing along (2, -1, 0)/sqrt(5)
        // (the corner-to-corner vector is (1-3t)*(2, -1, 0) before contact).
        let a = ConvexInput::cuboid(Vec3::splat(0.5));
        let b = ConvexInput::cuboid(Vec3::splat(0.5));
        let (t, normal, _point) = expect_hit(time_of_impact(
            &a,
            tr(-3.0, 2.0, 0.0),
            tr(3.0, -1.0, 0.0),
            &b,
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.0,
            1e-4,
            128,
        ));
        assert!((t - 1.0 / 3.0).abs() < 1e-3, "t = {t}, analytic 1/3");
        let expected = Vec3::new(2.0, -1.0, 0.0).normalize_or_zero();
        assert!(
            normal.dot(expected) > 0.9,
            "normal {normal}, expected ~{expected}"
        );
    }

    #[test]
    fn box_grazing_past_box_corner_stays_separated() {
        // Same direction (6, -3), but the path grazes the Minkowski cube's far corner
        // (1, 1) instead of entering it: p(t) = (-3, 3.15, 0) + t*(6, -3, 0). (The path
        // as delivered, from (-3, 2.15), descends through the top face y = 1 at x = -0.7,
        // t = 0.3833, so it is a hit, not a miss.) While x < 1 the path is at least 0.15
        // above the top face; the closest approach is to the corner (1, 1), at
        // perpendicular distance 0.15 * 2/sqrt(5) = 0.1342 (foot (1.06, 1.12), inside
        // the corner's Voronoi region). So: no hit at gap 0 or 0.13, a hit at 0.14.
        let a = ConvexInput::cuboid(Vec3::splat(0.5));
        let b = ConvexInput::cuboid(Vec3::splat(0.5));
        let sweep = |gap: f32| {
            time_of_impact(
                &a,
                tr(-3.0, 3.15, 0.0),
                tr(3.0, 0.15, 0.0),
                &b,
                Isometry::IDENTITY,
                Isometry::IDENTITY,
                gap,
                1e-4,
                256,
            )
        };
        assert_eq!(sweep(0.0), ToiResult::Separated);
        assert_eq!(sweep(0.13), ToiResult::Separated);
        assert!(
            matches!(sweep(0.14), ToiResult::Hit { .. }),
            "min gap is 0.1342, so a 0.14 skin is reached: {:?}",
            sweep(0.14)
        );
    }

    #[test]
    fn rotation_bounded_thin_rod_spinning_past_pin() {
        // Rod: 2 m long, 2 cm thick, spinning about Z through a half turn about its
        // centre at the origin. Pin: sphere r = 0.05 at (0, 0.9, 0). Treating the rod
        // as a segment of half-length 1 with half-thickness 0.01, the pin centre's
        // distance to the rod's centre line is 0.9*|cos(theta)|, and first contact is
        // when 0.9*cos(theta) = 0.05 + 0.01, i.e. theta = acos(0.06/0.9), with the
        // foot of the perpendicular at 0.9*sin(theta) < 1 (on the rod). The rod sweeps
        // theta = pi*t, so t_hit = acos(1/15)/pi.
        let rod = ConvexInput::cuboid(Vec3::new(1.0, 0.01, 0.01));
        let pin = ConvexInput::sphere(0.05);
        let pin_pose = tr(0.0, 0.9, 0.0);
        let (t, _normal, _point) = expect_hit(time_of_impact_angular(
            &rod,
            Isometry::IDENTITY,
            Isometry::from_rotation(Quat::from_rotation_z(PI)),
            &pin,
            pin_pose,
            pin_pose,
            PI,
            0.0,
            0.0,
            1e-4,
            256,
        ));
        let analytic = (1.0_f32 / 15.0).acos() / PI;
        assert!((t - analytic).abs() < 5e-3, "t = {t}, analytic {analytic}");
    }

    #[test]
    fn spinning_rod_misses_a_far_pin() {
        let rod = ConvexInput::cuboid(Vec3::new(1.0, 0.01, 0.01));
        let pin = ConvexInput::sphere(0.05);
        let pin_pose = tr(0.0, 1.3, 0.0);
        let result = time_of_impact_angular(
            &rod,
            Isometry::IDENTITY,
            Isometry::from_rotation(Quat::from_rotation_z(PI)),
            &pin,
            pin_pose,
            pin_pose,
            PI,
            0.0,
            0.0,
            1e-4,
            256,
        );
        assert_eq!(result, ToiResult::Separated);
    }

    #[test]
    fn started_overlapping_is_reported() {
        // Even moving apart, an initially interpenetrating pair has no first contact.
        let result = time_of_impact(
            &ConvexInput::sphere(1.0),
            Isometry::IDENTITY,
            tr(-5.0, 0.0, 0.0),
            &ConvexInput::sphere(1.0),
            tr(0.5, 0.0, 0.0),
            tr(5.0, 0.0, 0.0),
            0.0,
            1e-4,
            64,
        );
        assert_eq!(result, ToiResult::OverlappedAtStart);
    }

    #[test]
    fn zero_motion_separated_stays_separated() {
        let result = time_of_impact(
            &ConvexInput::sphere(0.5),
            tr(-2.0, 0.0, 0.0),
            tr(-2.0, 0.0, 0.0),
            &ConvexInput::sphere(0.5),
            tr(2.0, 0.0, 0.0),
            tr(2.0, 0.0, 0.0),
            0.0,
            1e-4,
            64,
        );
        assert_eq!(result, ToiResult::Separated);
    }

    #[test]
    fn zero_motion_within_target_gap_hits_at_zero() {
        // Centres 1.2 apart, radii 0.5 + 0.5, so the surface gap is 0.2, inside a
        // 0.25 target gap: hit at t = 0.
        let (t, normal, _point) = expect_hit(time_of_impact(
            &ConvexInput::sphere(0.5),
            tr(-0.6, 0.0, 0.0),
            tr(-0.6, 0.0, 0.0),
            &ConvexInput::sphere(0.5),
            tr(0.6, 0.0, 0.0),
            tr(0.6, 0.0, 0.0),
            0.25,
            1e-4,
            64,
        ));
        assert_eq!(t, 0.0);
        assert!(normal.dot(Vec3::X) > 0.999);
    }

    #[test]
    fn zero_size_sphere_point_hits_box_face() {
        // A degenerate r = 0 sphere (a point) travelling +X into the -X face (x = -0.5)
        // of a unit cube: contact when the point reaches x = -0.5, from x = -4 to
        // x = +4, so t = 3.5/8.
        let (t, normal, point) = expect_hit(time_of_impact(
            &ConvexInput::sphere(0.0),
            tr(-4.0, 0.2, -0.1),
            tr(4.0, 0.2, -0.1),
            &ConvexInput::cuboid(Vec3::splat(0.5)),
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.0,
            1e-4,
            64,
        ));
        assert!((t - 3.5 / 8.0).abs() < 1e-3, "t = {t}");
        assert!(normal.dot(Vec3::X) > 0.999);
        assert!((point.x + 0.5).abs() < 2e-3, "point {point}");
    }

    #[test]
    fn degenerate_flat_box_and_point_shapes_do_not_panic() {
        // A flat plate (zero Z extent) against a point shape, moving apart: the query
        // must answer cleanly, whichever way the degenerate geometry resolves.
        let result = time_of_impact(
            &ConvexInput::cuboid(Vec3::new(1.0, 1.0, 0.0)),
            Isometry::IDENTITY,
            tr(0.0, 0.0, -2.0),
            &ConvexInput::point(),
            tr(0.0, 0.0, 3.0),
            tr(0.0, 0.0, 5.0),
            0.0,
            1e-4,
            64,
        );
        assert_eq!(result, ToiResult::Separated);
        // Two coincident points touch at t = 0.
        let (t, _n, _p) = expect_hit(time_of_impact(
            &ConvexInput::point(),
            Isometry::IDENTITY,
            tr(1.0, 0.0, 0.0),
            &ConvexInput::point(),
            Isometry::IDENTITY,
            tr(-1.0, 0.0, 0.0),
            0.0,
            1e-4,
            64,
        ));
        assert_eq!(t, 0.0);
    }

    #[test]
    fn nan_inputs_return_failed() {
        let ball = ConvexInput::sphere(0.5);
        let block = ConvexInput::cuboid(Vec3::splat(0.5));
        let nan_pose = tr(f32::NAN, 0.0, 0.0);
        assert_eq!(
            time_of_impact(
                &ball,
                nan_pose,
                tr(1.0, 0.0, 0.0),
                &block,
                Isometry::IDENTITY,
                Isometry::IDENTITY,
                0.0,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::NonFiniteInput)
        );
        assert_eq!(
            time_of_impact(
                &ConvexInput::sphere(f32::NAN),
                Isometry::IDENTITY,
                tr(1.0, 0.0, 0.0),
                &block,
                Isometry::IDENTITY,
                Isometry::IDENTITY,
                0.0,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::NonFiniteInput)
        );
        assert_eq!(
            time_of_impact(
                &ball,
                Isometry::IDENTITY,
                tr(1.0, 0.0, 0.0),
                &block,
                Isometry::IDENTITY,
                Isometry::IDENTITY,
                f32::NAN,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::NonFiniteInput)
        );
        assert_eq!(
            time_of_impact(
                &ball,
                Isometry::IDENTITY,
                tr(1.0, 0.0, 0.0),
                &block,
                Isometry::IDENTITY,
                Isometry::IDENTITY,
                0.0,
                f32::INFINITY,
                64
            ),
            ToiResult::Failed(ToiFailure::NonFiniteInput)
        );
        // A NaN hull point is rejected by the distance query too.
        let hull = ConvexInput::ConvexHull {
            points: vec![Vec3::ZERO, Vec3::new(f32::NAN, 0.0, 0.0)],
        };
        assert_eq!(
            convex_distance(&hull, Isometry::IDENTITY, &ball, Isometry::IDENTITY),
            Err(ToiFailure::NonFiniteInput)
        );
    }

    #[test]
    fn invalid_inputs_return_failed_not_panic() {
        let ball = ConvexInput::sphere(0.5);
        let block = ConvexInput::cuboid(Vec3::splat(0.5));
        // Negative radius.
        assert_eq!(
            time_of_impact(
                &ConvexInput::sphere(-1.0),
                Isometry::IDENTITY,
                tr(1.0, 0.0, 0.0),
                &block,
                Isometry::IDENTITY,
                Isometry::IDENTITY,
                0.0,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::InvalidShape)
        );
        // Empty hull.
        let empty = ConvexInput::ConvexHull { points: Vec::new() };
        assert_eq!(
            convex_distance(&empty, Isometry::IDENTITY, &ball, Isometry::IDENTITY),
            Err(ToiFailure::InvalidShape)
        );
        // Negative target gap.
        assert_eq!(
            time_of_impact(
                &ball,
                Isometry::IDENTITY,
                tr(1.0, 0.0, 0.0),
                &block,
                tr(3.0, 0.0, 0.0),
                tr(3.0, 0.0, 0.0),
                -0.1,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::InvalidParameter)
        );
        // Zero iteration budget.
        assert_eq!(
            time_of_impact(
                &ball,
                Isometry::IDENTITY,
                tr(1.0, 0.0, 0.0),
                &block,
                tr(3.0, 0.0, 0.0),
                tr(3.0, 0.0, 0.0),
                0.0,
                1e-4,
                0
            ),
            ToiResult::Failed(ToiFailure::InvalidParameter)
        );
        // Zero quaternion pose.
        let bad_pose = Isometry::new(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0), Vec3::ZERO);
        assert_eq!(
            time_of_impact(
                &ball,
                bad_pose,
                tr(1.0, 0.0, 0.0),
                &block,
                tr(3.0, 0.0, 0.0),
                tr(3.0, 0.0, 0.0),
                0.0,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::InvalidPose)
        );
        // Negative angle bound.
        assert_eq!(
            time_of_impact_angular(
                &ball,
                Isometry::IDENTITY,
                tr(1.0, 0.0, 0.0),
                &block,
                tr(3.0, 0.0, 0.0),
                tr(3.0, 0.0, 0.0),
                -1.0,
                0.0,
                0.0,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::InvalidParameter)
        );
    }

    #[test]
    fn moving_apart_is_separated() {
        let result = time_of_impact(
            &ConvexInput::sphere(0.5),
            tr(-1.5, 0.0, 0.0),
            tr(-6.0, 0.0, 0.0),
            &ConvexInput::sphere(0.5),
            tr(1.5, 0.0, 0.0),
            tr(6.0, 0.0, 0.0),
            0.0,
            1e-4,
            64,
        );
        assert_eq!(result, ToiResult::Separated);
    }

    #[test]
    fn both_moving_head_on_uses_relative_speed() {
        // Surfaces start 4 m apart (centres at -2.5 and +2.5, radii 0.5). Each sphere
        // covers 3 m toward the other over the interval, closing 6 m in total, so
        // contact is at t = 4/6 = 2/3.
        let (t, _n, _p) = expect_hit(time_of_impact(
            &ConvexInput::sphere(0.5),
            tr(-2.5, 0.0, 0.0),
            tr(0.5, 0.0, 0.0),
            &ConvexInput::sphere(0.5),
            tr(2.5, 0.0, 0.0),
            tr(-0.5, 0.0, 0.0),
            0.0,
            1e-4,
            64,
        ));
        assert!((t - 2.0 / 3.0).abs() < 1e-3, "t = {t}");
    }

    #[test]
    fn positive_target_gap_stops_early() {
        // As the head-on wall test, but with a 0.25 target gap: contact (at the gap)
        // when the centre reaches x = -(0.005 + 0.1 + 0.25) = -0.355, t = 4.645/10.
        let (t, _n, _p) = expect_hit(time_of_impact(
            &ConvexInput::sphere(0.1),
            tr(-5.0, 0.0, 0.0),
            tr(5.0, 0.0, 0.0),
            &ConvexInput::cuboid(Vec3::new(0.005, 2.0, 2.0)),
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.25,
            1e-4,
            64,
        ));
        assert!((t - 0.4645).abs() < 1e-3, "t = {t}");
    }

    #[test]
    fn hit_exactly_at_interval_end() {
        // The sphere ends exactly touching: centre from -3 to -1, radius 0.5, cube
        // face at -0.5. At t = 1 the gap is exactly 0.
        let (t, _n, _p) = expect_hit(time_of_impact(
            &ConvexInput::sphere(0.5),
            tr(-3.0, 0.0, 0.0),
            tr(-1.0, 0.0, 0.0),
            &ConvexInput::cuboid(Vec3::splat(0.5)),
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.0,
            1e-4,
            64,
        ));
        assert!((t - 1.0).abs() < 1e-3, "t = {t}");
    }

    #[test]
    fn sphere_sweep_helper_matches_direct_query() {
        let wall = ConvexInput::cuboid(Vec3::new(0.25, 3.0, 3.0));
        let (t, normal, _p) = expect_hit(sphere_sweep(
            Vec3::new(0.0, 5.0, 0.0),
            Vec3::new(0.0, -5.0, 0.0),
            0.5,
            &wall,
            Isometry::from_rotation(Quat::from_rotation_z(core::f32::consts::FRAC_PI_2)),
            Isometry::from_rotation(Quat::from_rotation_z(core::f32::consts::FRAC_PI_2)),
            0.0,
            1e-4,
            64,
        ));
        // The wall rotated 90 degrees about Z is a horizontal slab of half-thickness
        // 0.25 in Y. The falling sphere (r 0.5) touches its top (y = 0.25) with centre
        // at y = 0.75: t = (5 - 0.75)/10 = 0.425, normal A->B is -Y.
        assert!((t - 0.425).abs() < 1e-3, "t = {t}");
        assert!(normal.dot(Vec3::NEG_Y) > 0.99, "normal {normal}");
    }

    #[test]
    fn sphere_sweep_helper_rejects_nan() {
        let wall = ConvexInput::cuboid(Vec3::splat(0.5));
        assert_eq!(
            sphere_sweep(
                Vec3::new(f32::NAN, 0.0, 0.0),
                Vec3::ZERO,
                0.5,
                &wall,
                Isometry::IDENTITY,
                Isometry::IDENTITY,
                0.0,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::NonFiniteInput)
        );
    }

    #[test]
    fn swept_box_vs_thin_wall_helper_does_not_tunnel() {
        // A 1 m crate covering 20 m in one step into a 2 cm wall at x = 0. Contact
        // when the crate centre reaches x = -(0.01 + 0.5) = -0.51: t = 9.49/20.
        let (t, normal, _p) = expect_hit(swept_box_vs_thin_wall(
            Vec3::splat(0.5),
            tr(-10.0, 0.0, 0.0),
            tr(10.0, 0.0, 0.0),
            Vec3::new(0.01, 5.0, 5.0),
            Isometry::IDENTITY,
            0.0,
            1e-4,
            64,
        ));
        assert!((t - 9.49 / 20.0).abs() < 1e-3, "t = {t}");
        assert!(t < 0.5, "near face, not tunneled: t = {t}");
        assert!(normal.dot(Vec3::X) > 0.999);
    }

    #[test]
    fn capsule_falling_onto_sphere_is_analytic() {
        // Capsule along Y (half-height 0.5, radius 0.25) centred above a sphere of
        // radius 0.75 at the origin. The capsule's lower cap centre is at
        // centre_y - 0.5; contact when that is 0.25 + 0.75 = 1.0 above the sphere
        // centre, i.e. capsule centre at y = 1.5. From y = 5 to y = -1 (travel 6):
        // t = 3.5/6.
        let capsule = ConvexInput::Capsule {
            half_height: 0.5,
            radius: 0.25,
            axis: ShapeAxis::Y,
        };
        let (t, normal, _p) = expect_hit(time_of_impact(
            &capsule,
            tr(0.0, 5.0, 0.0),
            tr(0.0, -1.0, 0.0),
            &ConvexInput::sphere(0.75),
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.0,
            1e-4,
            64,
        ));
        assert!((t - 3.5 / 6.0).abs() < 1e-3, "t = {t}");
        assert!(normal.dot(Vec3::NEG_Y) > 0.99, "normal {normal}");
    }

    #[test]
    fn cylinder_cap_and_cone_apex_distances_are_sane() {
        // Cylinder along Y, half-height 1, radius 0.5, centred at y = 3 over a point
        // at the origin: the lower cap face is at y = 2, distance 2.
        let cylinder = ConvexInput::Cylinder {
            half_height: 1.0,
            radius: 0.5,
            axis: ShapeAxis::Y,
        };
        let d = expect_distance(convex_distance(
            &cylinder,
            tr(0.0, 3.0, 0.0),
            &ConvexInput::point(),
            Isometry::IDENTITY,
        ));
        assert!((d.distance - 2.0).abs() < 1e-5, "distance {}", d.distance);
        // Cone along Y (apex up), half-height 1, base radius 0.5, centred at y = 3:
        // its base disc is at y = 2, so the distance to the origin point is also 2.
        let cone = ConvexInput::Cone {
            half_height: 1.0,
            radius: 0.5,
            axis: ShapeAxis::Y,
        };
        let d = expect_distance(convex_distance(
            &cone,
            tr(0.0, 3.0, 0.0),
            &ConvexInput::point(),
            Isometry::IDENTITY,
        ));
        assert!((d.distance - 2.0).abs() < 1e-5, "distance {}", d.distance);
    }

    #[test]
    fn convex_hull_of_cube_corners_behaves_like_the_cube() {
        let mut points = Vec::new();
        for sx in [-0.5, 0.5] {
            for sy in [-0.5, 0.5] {
                for sz in [-0.5, 0.5] {
                    points.push(Vec3::new(sx, sy, sz));
                }
            }
        }
        // An interior point must not change anything.
        points.push(Vec3::ZERO);
        let hull = ConvexInput::ConvexHull { points };
        let d = expect_distance(convex_distance(
            &ConvexInput::sphere(0.5),
            tr(-3.0, 0.0, 0.0),
            &hull,
            Isometry::IDENTITY,
        ));
        assert!((d.distance - 2.0).abs() < 1e-5, "distance {}", d.distance);
    }

    #[test]
    fn segment_vs_sphere_distance_is_analytic() {
        // Segment from (-1, 1, 0) to (1, 1, 0); sphere r = 0.25 at the origin. Closest
        // segment point is (0, 1, 0), so the distance is 1 - 0.25 = 0.75.
        let segment = ConvexInput::Segment {
            a: Vec3::new(-1.0, 0.0, 0.0),
            b: Vec3::new(1.0, 0.0, 0.0),
        };
        let d = expect_distance(convex_distance(
            &segment,
            tr(0.0, 1.0, 0.0),
            &ConvexInput::sphere(0.25),
            Isometry::IDENTITY,
        ));
        assert!((d.distance - 0.75).abs() < 1e-5, "distance {}", d.distance);
    }

    #[test]
    fn rotated_box_distance_uses_the_pose() {
        // A long box (half 1 x 0.1 x 0.1) at (3, 0, 0). Unrotated, its near end is at
        // x = 2, distance 1.5 from a sphere r = 0.5 at the origin. Rotated 90 degrees
        // about Z it stands across the line of centres: near face at x = 2.9,
        // distance 2.4.
        let bar = ConvexInput::cuboid(Vec3::new(1.0, 0.1, 0.1));
        let ball = ConvexInput::sphere(0.5);
        let flat = expect_distance(convex_distance(
            &ball,
            Isometry::IDENTITY,
            &bar,
            tr(3.0, 0.0, 0.0),
        ));
        assert!((flat.distance - 1.5).abs() < 1e-5, "distance {}", flat.distance);
        let turned = Isometry::new(
            Quat::from_rotation_z(core::f32::consts::FRAC_PI_2),
            Vec3::new(3.0, 0.0, 0.0),
        );
        let stood = expect_distance(convex_distance(&ball, Isometry::IDENTITY, &bar, turned));
        assert!((stood.distance - 2.4).abs() < 1e-5, "distance {}", stood.distance);
    }

    #[test]
    fn swapping_a_and_b_keeps_t_and_flips_the_normal() {
        let a = ConvexInput::cuboid(Vec3::splat(0.5));
        let b = ConvexInput::cuboid(Vec3::splat(0.5));
        let (t1, n1, _p1) = expect_hit(time_of_impact(
            &a,
            tr(-3.0, 2.0, 0.0),
            tr(3.0, -1.0, 0.0),
            &b,
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.0,
            1e-4,
            128,
        ));
        let (t2, n2, _p2) = expect_hit(time_of_impact(
            &b,
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            &a,
            tr(-3.0, 2.0, 0.0),
            tr(3.0, -1.0, 0.0),
            0.0,
            1e-4,
            128,
        ));
        assert!((t1 - t2).abs() < 1e-3, "t1 {t1} t2 {t2}");
        assert!(n1.dot(n2) < -0.9, "normals {n1} {n2} must oppose");
    }

    #[test]
    fn tiny_iteration_budget_reports_max_iterations() {
        // One advance only evaluates t = 0 and steps; it cannot confirm the hit.
        let result = time_of_impact(
            &ConvexInput::cuboid(Vec3::splat(0.5)),
            tr(-3.0, 2.0, 0.0),
            tr(3.0, -1.0, 0.0),
            &ConvexInput::cuboid(Vec3::splat(0.5)),
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.0,
            1e-4,
            1,
        );
        assert_eq!(result, ToiResult::Failed(ToiFailure::MaxIterations));
    }

    #[test]
    fn determinism_same_inputs_same_bits_100_repeats() {
        let a = ConvexInput::cuboid(Vec3::splat(0.5));
        let b = ConvexInput::cuboid(Vec3::splat(0.5));
        let first = expect_hit(time_of_impact(
            &a,
            tr(-3.0, 2.0, 0.0),
            tr(3.0, -1.0, 0.0),
            &b,
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            0.0,
            1e-4,
            128,
        ));
        for _ in 0..100 {
            let again = expect_hit(time_of_impact(
                &a,
                tr(-3.0, 2.0, 0.0),
                tr(3.0, -1.0, 0.0),
                &b,
                Isometry::IDENTITY,
                Isometry::IDENTITY,
                0.0,
                1e-4,
                128,
            ));
            assert_eq!(again.0.to_bits(), first.0.to_bits(), "t bits differ");
            assert_eq!(again.1.x.to_bits(), first.1.x.to_bits());
            assert_eq!(again.1.y.to_bits(), first.1.y.to_bits());
            assert_eq!(again.1.z.to_bits(), first.1.z.to_bits());
            assert_eq!(again.2.x.to_bits(), first.2.x.to_bits());
            assert_eq!(again.2.y.to_bits(), first.2.y.to_bits());
            assert_eq!(again.2.z.to_bits(), first.2.z.to_bits());
        }
    }

    #[test]
    fn from_collider_shape_converts_convex_and_rejects_the_rest() {
        let sphere = ConvexInput::from_collider_shape(&ColliderShape::Sphere { radius: 0.5 });
        assert_eq!(sphere, Some(ConvexInput::sphere(0.5)));
        let capsule = ConvexInput::from_collider_shape(&ColliderShape::Capsule {
            half_height: 0.5,
            radius: 0.25,
            axis: ShapeAxis::Z,
        });
        assert_eq!(
            capsule,
            Some(ConvexInput::Capsule {
                half_height: 0.5,
                radius: 0.25,
                axis: ShapeAxis::Z,
            })
        );
        let trimesh = ConvexInput::from_collider_shape(&ColliderShape::TriMesh {
            vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Y],
            indices: vec![[0, 1, 2]],
        });
        assert_eq!(trimesh, None);
        let bad = ConvexInput::from_collider_shape(&ColliderShape::Sphere { radius: -1.0 });
        assert_eq!(bad, None);
    }

    #[test]
    fn property_500_seeded_random_pairs_gap_at_reported_t() {
        // Seeded random sphere/box pairs. A starts 3-6 m from B in a random direction
        // and travels roughly toward (and usually past) B with a random sideways
        // offset, so hits and near misses both occur. For every reported hit:
        //   * the separation at the reported t is in [target_gap, target_gap + tolerance]
        //     (within 1e-5, f32 resolution of the poses; the delivered test allowed
        //     2e-3 on top of the tolerance), and
        //   * slightly before t the shapes are still separated (not overlapped, and
        //     at least target_gap - tolerance apart),
        // which together are the no-tunnelling contract. Cases that start overlapped
        // or exhaust the budget are counted, not asserted on.
        let mut rng = XorShift(0x5EED_1234_5678_9ABC);
        let target_gap = 0.02;
        let tolerance = 1e-3;
        let mut hits = 0_u32;
        let mut separated = 0_u32;
        for case in 0..500 {
            let shape_a = if rng.next_u64().is_multiple_of(2) {
                ConvexInput::sphere(rng.range(0.1, 0.6))
            } else {
                ConvexInput::cuboid(Vec3::new(
                    rng.range(0.1, 0.5),
                    rng.range(0.1, 0.5),
                    rng.range(0.1, 0.5),
                ))
            };
            let shape_b = if rng.next_u64().is_multiple_of(2) {
                ConvexInput::sphere(rng.range(0.1, 0.6))
            } else {
                ConvexInput::cuboid(Vec3::new(
                    rng.range(0.1, 0.5),
                    rng.range(0.1, 0.5),
                    rng.range(0.1, 0.5),
                ))
            };
            // Random approach direction (rejection-free: normalise a cube sample,
            // falling back to +X for a near-zero sample).
            let mut dir = Vec3::new(rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0));
            if dir.length_squared() < 1e-6 {
                dir = Vec3::X;
            }
            let dir = dir.normalize_or_zero();
            let start = -dir * rng.range(3.0, 6.0);
            // A sideways offset, built from a component of a random vector across dir.
            let side_raw = Vec3::new(rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0));
            let side = (side_raw - dir * side_raw.dot(dir)) * rng.range(0.0, 0.8);
            let reach = rng.range(0.8, 1.6);
            let end = start + (dir * start.length() * reach) + side;
            let rot_a = Quat::from_rotation_y(rng.range(-PI, PI));
            let rot_b = Quat::from_rotation_z(rng.range(-PI, PI));
            let xf_a0 = Isometry::new(rot_a, start);
            let xf_a1 = Isometry::new(rot_a, end);
            let xf_b = Isometry::new(rot_b, Vec3::ZERO);
            let result = time_of_impact(
                &shape_a, xf_a0, xf_a1, &shape_b, xf_b, xf_b, target_gap, tolerance, 256,
            );
            match result {
                ToiResult::Hit { t, .. } => {
                    hits += 1;
                    assert!((0.0..=1.0).contains(&t), "case {case}: t {t} out of range");
                    let at = expect_distance(convex_distance(
                        &shape_a,
                        xf_a0.interpolate(&xf_a1, t),
                        &shape_b,
                        xf_b,
                    ));
                    assert!(!at.overlapped, "case {case}: overlapped at reported t {t}");
                    assert!(
                        at.distance <= target_gap + tolerance + 1e-5,
                        "case {case}: gap {} at t {t} exceeds target + tolerance",
                        at.distance
                    );
                    assert!(
                        at.distance >= target_gap - 1e-5,
                        "case {case}: gap {} at t {t} is below target - tolerance",
                        at.distance
                    );
                    if t > 0.01 {
                        let before = expect_distance(convex_distance(
                            &shape_a,
                            xf_a0.interpolate(&xf_a1, t - 0.005),
                            &shape_b,
                            xf_b,
                        ));
                        assert!(!before.overlapped, "case {case}: already overlapped before t {t}");
                        assert!(
                            before.distance >= target_gap - tolerance,
                            "case {case}: gap {} before t {t} already inside target",
                            before.distance
                        );
                    }
                }
                ToiResult::Separated => separated += 1,
                ToiResult::OverlappedAtStart => {
                    panic!("case {case}: starts 3+ m away, cannot overlap at start");
                }
                ToiResult::Failed(reason) => {
                    panic!("case {case}: unexpected failure {reason:?}");
                }
            }
        }
        assert!(hits >= 50, "property test must exercise hits, got {hits}");
        assert!(
            separated >= 10,
            "property test must exercise misses, got {separated}"
        );
    }

    // -- Angular sweep along a rigid motion (ADR-2423) ------------------------------

    /// A spin of `turns` whole turns about +Z in one interval.
    fn spin_z(start: Isometry, turns: f32) -> RigidMotion {
        RigidMotion {
            start,
            local_center: Vec3::ZERO,
            linear: Vec3::ZERO,
            angular: Vec3::new(0.0, 0.0, turns * 2.0 * PI),
        }
    }

    #[test]
    fn motion_pose_at_rotates_about_the_moving_centre() {
        // A box whose rotation centre is 1 m along its local X: a half turn about Z swings
        // the origin to the far side of the centre, while the centre itself translates.
        let motion = RigidMotion {
            start: tr(0.0, 0.0, 0.0),
            local_center: Vec3::new(1.0, 0.0, 0.0),
            linear: Vec3::new(0.0, 2.0, 0.0),
            angular: Vec3::new(0.0, 0.0, PI),
        };
        let start = motion.pose_at(0.0);
        assert!((start.translation - Vec3::ZERO).length() < 1e-6);
        let end = motion.pose_at(1.0);
        // Centre ends at (1, 2, 0); the origin is 1 m from it, on the -X side rotated by pi.
        assert!(
            (end.translation - Vec3::new(2.0, 2.0, 0.0)).length() < 1e-5,
            "{:?}",
            end.translation
        );
        assert!((end.transform_point(Vec3::new(1.0, 0.0, 0.0)) - Vec3::new(1.0, 2.0, 0.0)).length() < 1e-5);
        // Halfway: centre at (1, 1, 0), origin rotated a quarter turn: (1, 0, 0).
        let mid = motion.pose_at(0.5);
        assert!(
            (mid.translation - Vec3::new(1.0, 0.0, 0.0)).length() < 1e-5,
            "{:?}",
            mid.translation
        );
    }

    #[test]
    fn multi_turn_spin_hits_where_the_endpoint_slerp_sees_nothing() {
        let bar = ConvexInput::cuboid(Vec3::new(1.0, 0.02, 0.02));
        let slab = ConvexInput::cuboid(Vec3::new(2.0, 0.01, 2.0));
        let slab_pose = tr(0.0, 0.5, 0.0);
        let options = ToiOptions::default();
        // Three whole turns: identical endpoint poses.
        let slerp = time_of_impact(
            &bar,
            Isometry::IDENTITY,
            Isometry::IDENTITY,
            &slab,
            slab_pose,
            slab_pose,
            0.0,
            options.tolerance,
            options.max_iterations,
        );
        assert_eq!(
            slerp,
            ToiResult::Separated,
            "endpoint slerp cannot see a whole-turn spin"
        );
        let motion = spin_z(Isometry::IDENTITY, 3.0);
        let result =
            time_of_impact_motion(&bar, &motion, &slab, &RigidMotion::stationary(slab_pose), options);
        let (t, normal, point) = expect_hit(result);
        // The whole sweep is pinned (ADR-4773): every sampled pose goes through the
        // deterministic kernels, so the reported impact must be these bits on every host.
        assert_eq!(
            [t, normal.x, normal.y, normal.z, point.x, point.y, point.z].map(f32::to_bits),
            SPIN_BAR_HIT_BITS,
            "the spinning bar's time of impact is platform-independent"
        );
        // The tip corner (1, 0.02) reaches y = 0.49 when its polar angle atan2(0.02, 1) + theta
        // gives a height of 0.49 at radius |(1, 0.02)|.
        let r = Vec3::new(1.0, 0.02, 0.0).length();
        let theta = (0.49_f32 / r).asin() - 0.02_f32.atan2(1.0);
        let expected = theta / (6.0 * PI);
        assert!((t - expected).abs() < 2e-4, "t {t} vs analytic {expected}");
        assert!(
            normal.dot(Vec3::Y) > 0.99,
            "normal points from the bar up into the slab: {normal}"
        );
        let d = expect_distance(convex_distance(&bar, motion.pose_at(t), &slab, slab_pose));
        assert!(
            !d.overlapped && d.distance <= options.tolerance + 1e-6,
            "gap at t: {}",
            d.distance
        );
    }

    #[test]
    fn spin_about_the_contact_normal_costs_no_advance() {
        // A disc spinning fast about its own axis, 1 cm above a floor: the closing bound is
        // zero along the normal, so one evaluation proves separation.
        let disc = ConvexInput::Cylinder {
            half_height: 0.05,
            radius: 0.5,
            axis: ShapeAxis::Y,
        };
        let floor = ConvexInput::cuboid(Vec3::new(5.0, 0.5, 5.0));
        let motion = RigidMotion {
            start: tr(0.0, 0.56, 0.0),
            local_center: Vec3::ZERO,
            linear: Vec3::ZERO,
            angular: Vec3::new(0.0, 40.0 * PI, 0.0),
        };
        let options = ToiOptions {
            max_iterations: 1,
            ..ToiOptions::default()
        };
        let result = time_of_impact_motion(
            &disc,
            &motion,
            &floor,
            &RigidMotion::stationary(Isometry::IDENTITY),
            options,
        );
        assert_eq!(result, ToiResult::Separated);
    }

    #[test]
    fn motion_refuses_nan_and_infinity_with_a_code() {
        let ball = ConvexInput::sphere(0.1);
        let wall = ConvexInput::cuboid(Vec3::new(0.01, 1.0, 1.0));
        let still = RigidMotion::stationary(Isometry::IDENTITY);
        let good = RigidMotion {
            start: tr(-2.0, 0.0, 0.0),
            local_center: Vec3::ZERO,
            linear: Vec3::new(4.0, 0.0, 0.0),
            angular: Vec3::ZERO,
        };
        let options = ToiOptions::default();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let cases = [
                RigidMotion {
                    linear: Vec3::new(bad, 0.0, 0.0),
                    ..good
                },
                RigidMotion {
                    angular: Vec3::new(0.0, bad, 0.0),
                    ..good
                },
                RigidMotion {
                    local_center: Vec3::new(0.0, 0.0, bad),
                    ..good
                },
                RigidMotion {
                    start: Isometry::new(Quat::from_xyzw(bad, 0.0, 0.0, 1.0), Vec3::ZERO),
                    ..good
                },
                RigidMotion {
                    start: tr(bad, 0.0, 0.0),
                    ..good
                },
            ];
            for (i, motion) in cases.iter().enumerate() {
                let result = time_of_impact_motion(&ball, motion, &wall, &still, options);
                assert_eq!(
                    result,
                    ToiResult::Failed(ToiFailure::NonFiniteInput),
                    "case {i} with {bad}"
                );
                // Either side.
                let result = time_of_impact_motion(&wall, &still, &ball, motion, options);
                assert_eq!(
                    result,
                    ToiResult::Failed(ToiFailure::NonFiniteInput),
                    "swapped case {i} with {bad}"
                );
            }
            let nan_options = ToiOptions {
                target_gap: bad,
                ..options
            };
            assert_eq!(
                time_of_impact_motion(&ball, &good, &wall, &still, nan_options),
                ToiResult::Failed(ToiFailure::NonFiniteInput)
            );
            assert_eq!(
                time_of_impact_motion(&ConvexInput::sphere(bad), &good, &wall, &still, options),
                ToiResult::Failed(ToiFailure::NonFiniteInput)
            );
        }
        assert_eq!(ToiFailure::NonFiniteInput.code(), "non_finite_input");
    }

    #[test]
    fn overflowing_closing_speeds_are_refused_not_looped() {
        let ball = ConvexInput::sphere(0.1);
        let bar = ConvexInput::cuboid(Vec3::new(2.0, 0.1, 0.1));
        let options = ToiOptions::default();
        // Finite velocities whose difference overflows f32.
        let fast_a = RigidMotion {
            start: tr(-5.0, 0.0, 0.0),
            local_center: Vec3::ZERO,
            linear: Vec3::new(3e38, 0.0, 0.0),
            angular: Vec3::ZERO,
        };
        let fast_b = RigidMotion {
            start: tr(5.0, 0.0, 0.0),
            local_center: Vec3::ZERO,
            linear: Vec3::new(-3e38, 0.0, 0.0),
            angular: Vec3::ZERO,
        };
        assert_eq!(
            time_of_impact_motion(&ball, &fast_a, &ball, &fast_b, options),
            ToiResult::Failed(ToiFailure::DegenerateMotion)
        );
        // An angular speed whose bound |n x w| R overflows.
        let wild = RigidMotion {
            start: tr(0.0, 3.0, 0.0),
            local_center: Vec3::ZERO,
            linear: Vec3::ZERO,
            angular: Vec3::new(0.0, 0.0, 3e38),
        };
        assert_eq!(
            time_of_impact_motion(
                &bar,
                &wild,
                &ball,
                &RigidMotion::stationary(Isometry::IDENTITY),
                options
            ),
            ToiResult::Failed(ToiFailure::DegenerateMotion)
        );
        // The endpoint-slerp query refuses the same overflow instead of reading it as rest.
        assert_eq!(
            time_of_impact(
                &ball,
                tr(-3e38, 0.0, 0.0),
                tr(3e38, 0.0, 0.0),
                &ball,
                tr(3e38, 0.0, 0.0),
                tr(-3e38, 0.0, 0.0),
                0.0,
                1e-4,
                64
            ),
            ToiResult::Failed(ToiFailure::DegenerateMotion)
        );
        // A huge but finite spin is bounded by the budget: it returns, whatever it says.
        let huge = RigidMotion {
            angular: Vec3::new(0.0, 0.0, 1e30),
            ..wild
        };
        let result = time_of_impact_motion(
            &bar,
            &huge,
            &ball,
            &RigidMotion::stationary(Isometry::IDENTITY),
            options,
        );
        assert!(
            matches!(
                result,
                ToiResult::Failed(ToiFailure::MaxIterations | ToiFailure::DegenerateMotion)
                    | ToiResult::Hit { .. }
            ),
            "{result:?}"
        );
        assert_eq!(ToiFailure::DegenerateMotion.code(), "degenerate_motion");
    }

    #[test]
    fn motion_sweeps_are_bit_identical_over_repeats() {
        let bar = ConvexInput::cuboid(Vec3::new(0.8, 0.03, 0.05));
        let hull = ConvexInput::ConvexHull {
            points: vec![
                Vec3::new(0.3, 0.0, 0.0),
                Vec3::new(-0.2, 0.25, 0.1),
                Vec3::new(0.0, -0.3, 0.2),
                Vec3::new(0.1, 0.1, -0.3),
            ],
        };
        let a = RigidMotion {
            start: Isometry::new(Quat::from_rotation_y(0.3), Vec3::new(-2.0, 0.1, 0.0)),
            local_center: Vec3::new(0.1, 0.0, 0.0),
            linear: Vec3::new(3.5, 0.2, 0.0),
            angular: Vec3::new(1.0, 9.0, 4.0),
        };
        let b = RigidMotion {
            start: tr(1.0, 0.0, 0.0),
            local_center: Vec3::ZERO,
            linear: Vec3::new(-0.5, 0.0, 0.0),
            angular: Vec3::new(0.0, 3.0, 0.0),
        };
        let first = time_of_impact_motion(&bar, &a, &hull, &b, ToiOptions::default());
        assert!(matches!(first, ToiResult::Hit { .. }), "{first:?}");
        let bits = |r: ToiResult| match r {
            ToiResult::Hit { t, normal, point } => {
                [t, normal.x, normal.y, normal.z, point.x, point.y, point.z].map(f32::to_bits)
            }
            _ => [0; 7],
        };
        for _ in 0..100 {
            assert_eq!(
                bits(time_of_impact_motion(&bar, &a, &hull, &b, ToiOptions::default())),
                bits(first)
            );
        }
    }

    #[test]
    fn random_rigid_motions_never_report_late_hits_or_false_misses() {
        let mut rng = XorShift(0x5EED_CCD5_2423_0001);
        let options = ToiOptions {
            target_gap: 0.0,
            tolerance: 1e-3,
            max_iterations: 512,
        };
        let (mut hits, mut separated) = (0, 0);
        for case in 0..300 {
            let shape_a = match case % 3 {
                0 => ConvexInput::cuboid(Vec3::new(
                    rng.range(0.3, 1.2),
                    rng.range(0.01, 0.1),
                    rng.range(0.01, 0.1),
                )),
                1 => ConvexInput::Capsule {
                    half_height: rng.range(0.1, 0.8),
                    radius: rng.range(0.02, 0.2),
                    axis: ShapeAxis::X,
                },
                _ => ConvexInput::sphere(rng.range(0.05, 0.5)),
            };
            let shape_b = ConvexInput::cuboid(Vec3::new(
                rng.range(0.01, 0.5),
                rng.range(0.2, 1.0),
                rng.range(0.2, 1.0),
            ));
            let a = RigidMotion {
                start: Isometry::new(
                    Quat::from_scaled_axis(Vec3::new(
                        rng.range(-3.0, 3.0),
                        rng.range(-3.0, 3.0),
                        rng.range(-3.0, 3.0),
                    )),
                    Vec3::new(rng.range(-4.0, -2.5), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0)),
                ),
                local_center: Vec3::new(rng.range(-0.2, 0.2), rng.range(-0.1, 0.1), 0.0),
                linear: Vec3::new(rng.range(0.0, 6.0), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0)),
                angular: Vec3::new(
                    rng.range(-15.0, 15.0),
                    rng.range(-15.0, 15.0),
                    rng.range(-15.0, 15.0),
                ),
            };
            let b = RigidMotion {
                start: Isometry::new(Quat::from_rotation_y(rng.range(-0.5, 0.5)), Vec3::ZERO),
                local_center: Vec3::ZERO,
                linear: Vec3::new(rng.range(-1.0, 0.0), 0.0, 0.0),
                angular: Vec3::new(0.0, rng.range(-2.0, 2.0), 0.0),
            };
            let result = time_of_impact_motion(&shape_a, &a, &shape_b, &b, options);
            let gap =
                |t: f32| expect_distance(convex_distance(&shape_a, a.pose_at(t), &shape_b, b.pose_at(t)));
            match result {
                ToiResult::Hit { t, .. } => {
                    hits += 1;
                    let at = gap(t);
                    assert!(
                        !at.overlapped && at.distance <= options.tolerance + 1e-5,
                        "case {case}: gap {} at t {t}",
                        at.distance
                    );
                    for k in 0..200 {
                        let s = t * k as f32 / 200.0;
                        let d = gap(s);
                        assert!(
                            !d.overlapped && d.distance >= -1e-5,
                            "case {case}: contact at {s} before t {t}"
                        );
                    }
                }
                ToiResult::Separated => {
                    separated += 1;
                    for k in 0..=400 {
                        let s = k as f32 / 400.0;
                        let d = gap(s);
                        assert!(
                            !d.overlapped && d.distance > options.target_gap,
                            "case {case}: Separated but gap {} at {s}",
                            d.distance
                        );
                    }
                }
                other => panic!("case {case}: unexpected {other:?}"),
            }
        }
        assert!(hits >= 60, "property test must exercise hits, got {hits}");
        assert!(
            separated >= 30,
            "property test must exercise misses, got {separated}"
        );
    }

    #[test]
    fn gjk_distance_does_not_allocate() {
        let ball = ConvexInput::sphere(0.5);
        let block = ConvexInput::cuboid(Vec3::splat(0.5));
        let motion_a = RigidMotion::from_velocities(
            Isometry::from_translation(Vec3::new(-3.0, 0.0, 0.0)),
            Vec3::ZERO,
            Vec3::new(4.0, 0.0, 0.0),
            Vec3::ZERO,
            1.0,
        );
        let motion_b = RigidMotion::stationary(Isometry::IDENTITY);
        let options = ToiOptions::default();
        let _ = time_of_impact_motion(&ball, &motion_a, &block, &motion_b, options);
        let before = crate::alloc_count::allocations();
        let hit = time_of_impact_motion(&ball, &motion_a, &block, &motion_b, options);
        let delta = crate::alloc_count::allocations() - before;
        assert!(
            matches!(hit, ToiResult::Hit { .. }),
            "the closing sphere must hit the box: {hit:?}"
        );
        assert_eq!(delta, 0, "sphere-box sweep allocated {delta} times");
        eprintln!("gjk_sweep_allocs {delta}");
    }
}
