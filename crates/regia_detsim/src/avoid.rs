// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/avoid.rs, commit d6ed37677), ADR-8012.
//! Integer reciprocal local avoidance in the XZ plane (ADR-8150).
//!
//! Positions and radii use millimeters; velocities use millimeters per second.
//! Candidates sort by `(distance_squared, id)` before taking the configured nearest
//! neighbors. Supply unique neighbor IDs and a snapshot from the same tick for
//! every mover. The kernel has no clock, global state, SIMD or float solver math.
//!
//! This Stage A compatibility kernel projects each threatening pair's half-plane
//! once in fixed order. It is best effort: later projections can violate earlier
//! constraints. The head-on right bias changes the preferred velocity *before*
//! the speed clamp. Its lateral direction is `[preferred_z, -preferred_x]`, the
//! source simulation's keep-right convention; hosts using another handedness
//! convert positions and velocities consistently at their boundary. Floor roots
//! and integer truncation also remain unchanged.
//!
//! # Input domain
//!
//! Every intermediate fits its integer type for inputs inside this domain
//! (ADR-8377 has the site-by-site proof):
//!
//! | Quantity | Bound |
//! |---|---|
//! | position, each axis, every body | `|x|, |z| <=` `ORCA_MAX_POSITION_MM` (2^31 mm) |
//! | velocity, each axis, every body and the preference | `<=` `ORCA_MAX_SPEED_MM_S` (2^20 mm/s) |
//! | radius | `0..=``ORCA_MAX_RADIUS_MM` (2^16 mm) |
//! | `horizon_ms` | `0..=``ORCA_MAX_HORIZON_MS` (2^20 ms) |
//! | `keep_right_mm_s`, `min_preferred_mm_s` | `0..=``ORCA_MAX_SPEED_MM_S` |
//! | `head_on_range_mm`, indexed look distance | `0..=``ORCA_MAX_RANGE_MM` (2^33 mm) |
//! | retained neighbors `min(max_neighbors, candidates)` | `<=` `ORCA_MAX_NEIGHBOR_PLANES` (256) |
//!
//! Entries carrying the mover's own ID never enter the arithmetic and are not
//! checked. Inside the domain the result is the pinned source's, bit for bit.
//! Outside it, `try_orca_velocity` returns an `OrcaDomainError` and the plain
//! functions return the preferred velocity clamped per axis to
//! `±ORCA_MAX_SPEED_MM_S`, identically in debug and release builds: the check
//! runs before any arithmetic, so no profile can panic or wrap.
//!
//! # Allocation and neighbors
//!
//! `OrcaScratch` owns the ordering and plane buffers; `orca_velocity_into`
//! allocates nothing once they have grown. Candidates are partially selected,
//! then sorted, so the cost is O(n + k log k) for n candidates and cap k.
//! `orca_velocity_indexed` gathers candidates from `DetSpatial` within the
//! look distance instead of a host-built list. The kernel adds no ECS or tracing
//! dependency; hosts wrap the crowd step in their own profiling span.
//!
//! # Solvers and walls (ADR-8555)
//!
//! `OrcaSettings::solver` picks the kernel. `OrcaSolver::StageA` (the default) is
//! the pinned single pass above, bit for bit. `OrcaSolver::Lp` is an opt-in,
//! integer-only linear program after the public ORCA formulation: the paper's
//! half-plane per neighbor (half responsibility), one per static wall segment
//! (`OrcaSegment`, full responsibility), the incremental 2D program over them in a
//! fixed order (walls first, then neighbors by `(distance_squared, id)`) inside the
//! speed circle `max(max_speed_mm_s, |preferred|)`, and, when no velocity satisfies
//! every half-plane, the fallback that minimizes the largest penetration while
//! keeping the walls (ties go to the velocity nearest the preference, and walls that
//! only conflict through the 2 mm/s margin stay hard without it, ADR-8703).
//! Switching solvers changes results. Stage A ignores segments.
//! `OrcaScratch::last_lp` and `OrcaScratch::lp_planes` show the last LP call; the
//! units and rounding rules are in ADR-8555.

use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use crate::spatial::DetSpatial;

mod lp;

/// Default maximum retained neighbors; candidate gathering and sorting still scale with input size.
pub const ORCA_MAX_NEIGHBOURS: usize = 8;
/// How far ahead a collision is predicted, in milliseconds (2 s).
pub const ORCA_HORIZON_MS: i64 = 2000;
/// Lateral bias added to the preferred velocity in a head-on meeting, mm/s.
pub const ORCA_KEEP_RIGHT_MM_S: i64 = 600;
/// Default preferred speed threshold for head-on bias only, in mm/s.
/// Contact planes are still projected below this speed.
pub const ORCA_MIN_PREFERRED_MM_S: i64 = 500;
/// Head-on bias only applies inside this range, in millimeters (25 m).
pub const ORCA_HEADON_RANGE_MM: i64 = 25_000;
/// Neighbors considered by the caller for the solver, in meters.
/// The solver itself culls again by the time horizon.
pub const ORCA_LOOK_M: f32 = 20.0;

/// Largest in-domain absolute position on each axis, in millimeters (2^31, about 2,147 km).
pub const ORCA_MAX_POSITION_MM: i64 = 1 << 31;
/// Largest in-domain velocity component (bodies and preference) and speed setting, in mm/s (2^20).
pub const ORCA_MAX_SPEED_MM_S: i64 = 1 << 20;
/// Largest in-domain disc radius, in millimeters (2^16, about 65.5 m).
pub const ORCA_MAX_RADIUS_MM: i64 = 1 << 16;
/// Largest in-domain prediction horizon, in milliseconds (2^20, about 17 minutes).
pub const ORCA_MAX_HORIZON_MS: i64 = 1 << 20;
/// Largest in-domain head-on range and indexed look distance, in millimeters (2^33).
pub const ORCA_MAX_RANGE_MM: i64 = 1 << 33;
/// Largest in-domain number of retained neighbors (projected planes) per call.
pub const ORCA_MAX_NEIGHBOR_PLANES: usize = 256;
/// Largest in-domain number of wall segments per LP call.
pub const ORCA_MAX_SEGMENTS: usize = 256;
/// Default LP horizon for wall segments, in milliseconds (1 s).
pub const ORCA_LP_OBSTACLE_HORIZON_MS: i64 = 1000;
/// Default LP time to leave an overlap (a neighbor or a wall), in milliseconds.
pub const ORCA_LP_CONTACT_MS: i64 = 100;
/// LP velocity sub-units per mm/s: [`OrcaHalfPlane::point_q`] is in `1/256` mm/s.
pub const ORCA_LP_VELOCITY_SCALE: i64 = lp::Q;
/// Length of an LP direction vector ([`OrcaHalfPlane::direction`]), 2^24.
pub const ORCA_LP_UNIT: i64 = lp::UNIT;

/// The avoidance kernel. All peers must use the same one; switching changes results.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrcaSolver {
    /// The pinned Stage A single projection pass (ADR-8150); the default.
    #[default]
    StageA,
    /// The integer linear program with walls, the speed circle and the
    /// least-penetration fallback (ADR-8555).
    Lp,
}

impl OrcaSolver {
    /// Stable lowercase name for agent rows: `stage_a` or `lp`.
    pub fn name(self) -> &'static str {
        match self {
            Self::StageA => "stage_a",
            Self::Lp => "lp",
        }
    }
}

/// A static wall segment in the XZ plane, in millimeters. Two-sided: a body on
/// either side keeps its radius off it. Convex obstacle edges, wall runs, or the
/// blocked-cell boundary of the det grid (`regia_nav::det::walls`) all work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrcaSegment {
    /// First endpoint `[x, z]`.
    pub a_mm: [i64; 2],
    /// Second endpoint `[x, z]`.
    pub b_mm: [i64; 2],
}

impl OrcaSegment {
    /// Check the endpoints against `±ORCA_MAX_POSITION_MM`.
    pub fn validate(&self) -> Result<(), OrcaDomainError> {
        let ok = |p: [i64; 2]| in_span(p[0], ORCA_MAX_POSITION_MM) && in_span(p[1], ORCA_MAX_POSITION_MM);
        if ok(self.a_mm) && ok(self.b_mm) {
            Ok(())
        } else {
            Err(OrcaDomainError::Segment)
        }
    }
}

/// One LP constraint as built (before the solver's inward margin): a velocity `v`
/// (mm/s) is admitted when `det(direction, v * ORCA_LP_VELOCITY_SCALE - point_q) >= 0`,
/// with `det(a, b) = a[0] * b[1] - a[1] * b[0]`; the free side is left of `direction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrcaHalfPlane {
    /// A point on the boundary, in `1/ORCA_LP_VELOCITY_SCALE` mm/s.
    pub point_q: [i64; 2],
    /// Boundary direction, length `ORCA_LP_UNIT` within one unit.
    pub direction: [i64; 2],
    /// True for a wall segment's plane, false for a neighbor's.
    pub obstacle: bool,
}

impl OrcaHalfPlane {
    /// True when the velocity (mm/s) lies on the free side, exactly.
    pub fn admits(&self, v_mm_s: [i64; 2]) -> bool {
        let s = i128::from(ORCA_LP_VELOCITY_SCALE);
        let x = i128::from(v_mm_s[0]) * s - i128::from(self.point_q[0]);
        let z = i128::from(v_mm_s[1]) * s - i128::from(self.point_q[1]);
        i128::from(self.direction[0]) * z - i128::from(self.direction[1]) * x >= 0
    }
}

/// What the last LP call found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrcaLpOutcome {
    /// Some velocity met every half-plane (with the 2 mm/s margin); the result does.
    pub feasible: bool,
    /// The result's largest penetration of a half-plane in mm/s, rounded up; 0 when feasible.
    pub worst_violation_mm_s: i64,
    /// Half-planes built (walls plus neighbors within reach).
    pub planes: u32,
}

/// Cumulative LP counters since construction or [`OrcaScratch::reset_stats`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OrcaLpStats {
    /// LP solver calls that reached the solve (in domain).
    pub calls: u64,
    /// Calls where every half-plane held.
    pub feasible: u64,
    /// Calls that used the least-penetration fallback.
    pub fallbacks: u64,
    /// Half-planes built for neighbors.
    pub agent_planes: u64,
    /// Half-planes built for wall segments.
    pub obstacle_planes: u64,
    /// Neighbors and segments skipped because they cannot reach the speed circle (and
    /// zero-length segments on the body center, which have no outward direction).
    pub skipped_far: u64,
    /// Segments passed to Stage A calls, which ignore them.
    pub stage_a_segments_ignored: u64,
    /// Largest `worst_violation_mm_s` seen.
    pub worst_violation_mm_s: u64,
}

impl OrcaLpStats {
    /// Named counters for agent rows, in a fixed order.
    pub fn fields(&self) -> [(&'static str, u64); 8] {
        [
            ("orca_lp_calls", self.calls),
            ("orca_lp_feasible", self.feasible),
            ("orca_lp_fallbacks", self.fallbacks),
            ("orca_lp_agent_planes", self.agent_planes),
            ("orca_lp_obstacle_planes", self.obstacle_planes),
            ("orca_lp_skipped_far", self.skipped_far),
            ("orca_lp_stage_a_segments_ignored", self.stage_a_segments_ignored),
            ("orca_lp_worst_violation_mm_s", self.worst_violation_mm_s),
        ]
    }
}

/// One disc in the solver. See the module-level input domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrcaBody {
    /// Stable host ID; distinct from every other neighbor ID.
    pub id: u32,
    /// X position in millimeters.
    pub x_mm: i64,
    /// Z position in millimeters.
    pub z_mm: i64,
    /// Current X velocity in millimeters per second.
    pub vx_mm_s: i64,
    /// Current Z velocity in millimeters per second.
    pub vz_mm_s: i64,
    /// Nonnegative disc radius in millimeters.
    pub r_mm: i64,
}

impl OrcaBody {
    /// Check this body against the module's input domain.
    pub fn validate(&self) -> Result<(), OrcaDomainError> {
        let id = self.id;
        if !in_span(self.x_mm, ORCA_MAX_POSITION_MM) || !in_span(self.z_mm, ORCA_MAX_POSITION_MM) {
            return Err(OrcaDomainError::Position { id });
        }
        if !in_span(self.vx_mm_s, ORCA_MAX_SPEED_MM_S) || !in_span(self.vz_mm_s, ORCA_MAX_SPEED_MM_S) {
            return Err(OrcaDomainError::Velocity { id });
        }
        if !(0..=ORCA_MAX_RADIUS_MM).contains(&self.r_mm) {
            return Err(OrcaDomainError::Radius { id });
        }
        Ok(())
    }

    /// XZ position in meters (`mm_to_m` per axis), for building [`crate::spatial::SpatialRow`]s
    /// that [`orca_velocity_indexed`] queries.
    pub fn position_m(&self) -> [f32; 2] {
        [mm_to_m(self.x_mm), mm_to_m(self.z_mm)]
    }
}

#[inline]
fn in_span(v: i64, max: i64) -> bool {
    (-max..=max).contains(&v)
}

/// An input outside the module's arithmetic domain. Codes are stable agent reason codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OrcaDomainError {
    /// A body's position exceeds [`ORCA_MAX_POSITION_MM`] on an axis.
    Position {
        /// The body's ID.
        id: u32,
    },
    /// A body's velocity exceeds [`ORCA_MAX_SPEED_MM_S`] on an axis.
    Velocity {
        /// The body's ID.
        id: u32,
    },
    /// A body's radius is negative or exceeds [`ORCA_MAX_RADIUS_MM`].
    Radius {
        /// The body's ID.
        id: u32,
    },
    /// The preferred velocity exceeds [`ORCA_MAX_SPEED_MM_S`] on an axis.
    Preferred,
    /// `horizon_ms` is negative or exceeds [`ORCA_MAX_HORIZON_MS`].
    Horizon,
    /// `keep_right_mm_s` is negative or exceeds [`ORCA_MAX_SPEED_MM_S`].
    KeepRight,
    /// `min_preferred_mm_s` is negative or exceeds [`ORCA_MAX_SPEED_MM_S`].
    MinPreferred,
    /// `head_on_range_mm` is negative or exceeds [`ORCA_MAX_RANGE_MM`].
    HeadOnRange,
    /// More than [`ORCA_MAX_NEIGHBOR_PLANES`] neighbors would be retained.
    NeighborCap,
    /// The indexed look distance is negative or exceeds [`ORCA_MAX_RANGE_MM`].
    Look,
    /// The mover's slot, or a gathered index slot, has no body (index and bodies disagree).
    Slot,
    /// LP only: `max_speed_mm_s` is negative or exceeds [`ORCA_MAX_SPEED_MM_S`].
    MaxSpeed,
    /// LP only: `obstacle_horizon_ms` is outside `1..=ORCA_MAX_HORIZON_MS`.
    ObstacleHorizon,
    /// LP only: `contact_ms` is outside `1..=ORCA_MAX_HORIZON_MS`.
    Contact,
    /// LP only: a segment endpoint exceeds [`ORCA_MAX_POSITION_MM`] on an axis.
    Segment,
    /// LP only: more than [`ORCA_MAX_SEGMENTS`] segments.
    SegmentCap,
}

impl OrcaDomainError {
    /// Stable lowercase reason code for agent rows and logs.
    pub fn code(self) -> &'static str {
        match self {
            Self::Position { .. } => "orca_position",
            Self::Velocity { .. } => "orca_velocity",
            Self::Radius { .. } => "orca_radius",
            Self::Preferred => "orca_preferred",
            Self::Horizon => "orca_horizon",
            Self::KeepRight => "orca_keep_right",
            Self::MinPreferred => "orca_min_preferred",
            Self::HeadOnRange => "orca_head_on_range",
            Self::NeighborCap => "orca_neighbor_cap",
            Self::Look => "orca_look",
            Self::Slot => "orca_slot",
            Self::MaxSpeed => "orca_max_speed",
            Self::ObstacleHorizon => "orca_obstacle_horizon",
            Self::Contact => "orca_contact",
            Self::Segment => "orca_segment",
            Self::SegmentCap => "orca_segment_cap",
        }
    }
}

impl std::fmt::Display for OrcaDomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Position { id } | Self::Velocity { id } | Self::Radius { id } => {
                write!(
                    f,
                    "{}: body {id} is outside the avoidance input domain",
                    self.code()
                )
            }
            _ => write!(f, "{}: input outside the avoidance input domain", self.code()),
        }
    }
}

impl std::error::Error for OrcaDomainError {}

/// Host-owned avoidance settings. All peers must use identical values.
///
/// Defaults preserve the pinned source. Zero `max_neighbors` returns the preferred
/// velocity unchanged. `look_m` belongs to the host's neighbor query and is not used
/// by the solver. Other numeric fields must be nonnegative and within the module's
/// input domain ([`OrcaSettings::validate`]). Changing settings changes simulation behavior.
/// `solver`, `max_speed_mm_s`, `obstacle_horizon_ms` and `contact_ms` are read by the
/// LP only. Deserializing fills missing fields from the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrcaSettings {
    /// Maximum neighbors retained after distance/ID sorting.
    pub max_neighbors: usize,
    /// Prediction horizon in milliseconds.
    pub horizon_ms: i64,
    /// Lateral head-on preference bias in millimeters per second.
    pub keep_right_mm_s: i64,
    /// Preferred speed threshold for head-on bias in millimeters per second.
    pub min_preferred_mm_s: i64,
    /// Maximum head-on detection distance in millimeters.
    pub head_on_range_mm: i64,
    /// Look distance in meters: the host's broadphase distance and the default for
    /// [`OrcaNeighborQuery::from_settings`]; the solver itself does not read it.
    pub look_m: f32,
    /// The kernel; [`OrcaSolver::StageA`] by default.
    pub solver: OrcaSolver,
    /// LP speed circle radius in mm/s; the circle is never smaller than the preferred
    /// speed, so 0 (the default) means "never faster than preferred". A larger value
    /// lets a body speed up to dodge.
    pub max_speed_mm_s: i64,
    /// LP horizon for wall segments in milliseconds.
    pub obstacle_horizon_ms: i64,
    /// LP time to leave an overlap with a neighbor or a wall, in milliseconds.
    pub contact_ms: i64,
}

impl OrcaSettings {
    /// Check the numeric settings against the module's input domain. The retained
    /// neighbor bound depends on the candidate count and is checked per call.
    pub fn validate(&self) -> Result<(), OrcaDomainError> {
        if !(0..=ORCA_MAX_HORIZON_MS).contains(&self.horizon_ms) {
            return Err(OrcaDomainError::Horizon);
        }
        if !(0..=ORCA_MAX_SPEED_MM_S).contains(&self.keep_right_mm_s) {
            return Err(OrcaDomainError::KeepRight);
        }
        if !(0..=ORCA_MAX_SPEED_MM_S).contains(&self.min_preferred_mm_s) {
            return Err(OrcaDomainError::MinPreferred);
        }
        if !(0..=ORCA_MAX_RANGE_MM).contains(&self.head_on_range_mm) {
            return Err(OrcaDomainError::HeadOnRange);
        }
        if self.solver == OrcaSolver::Lp {
            if self.horizon_ms < 1 {
                return Err(OrcaDomainError::Horizon);
            }
            if !(0..=ORCA_MAX_SPEED_MM_S).contains(&self.max_speed_mm_s) {
                return Err(OrcaDomainError::MaxSpeed);
            }
            if !(1..=ORCA_MAX_HORIZON_MS).contains(&self.obstacle_horizon_ms) {
                return Err(OrcaDomainError::ObstacleHorizon);
            }
            if !(1..=ORCA_MAX_HORIZON_MS).contains(&self.contact_ms) {
                return Err(OrcaDomainError::Contact);
            }
        }
        Ok(())
    }
}

impl Default for OrcaSettings {
    fn default() -> Self {
        Self {
            max_neighbors: ORCA_MAX_NEIGHBOURS,
            horizon_ms: ORCA_HORIZON_MS,
            keep_right_mm_s: ORCA_KEEP_RIGHT_MM_S,
            min_preferred_mm_s: ORCA_MIN_PREFERRED_MM_S,
            head_on_range_mm: ORCA_HEADON_RANGE_MM,
            look_m: ORCA_LOOK_M,
            solver: OrcaSolver::StageA,
            max_speed_mm_s: 0,
            obstacle_horizon_ms: ORCA_LP_OBSTACLE_HORIZON_MS,
            contact_ms: ORCA_LP_CONTACT_MS,
        }
    }
}

/// Meters to whole millimeters, half away from zero.
#[inline]
pub fn m_to_mm(v: f32) -> i64 {
    (v * 1000.0).round() as i64
}

/// Millimeters back to meters.
#[inline]
pub fn mm_to_m(v: i64) -> f32 {
    v as f32 / 1000.0
}

/// mm/s back to m/s.
#[inline]
pub fn mm_s_to_m_s(v: i64) -> f32 {
    v as f32 / 1000.0
}

/// Floor integer square root using the original descending Newton iteration.
#[allow(
    clippy::manual_div_ceil,
    clippy::while_immutable_condition,
    reason = "ADR-8150: preserve Stage A expressions; descending Newton reaches floor before the unreachable upward loop"
)]
fn isqrt_floor(v: u64) -> u64 {
    if v < 2 {
        return v;
    }
    let bits = 64 - v.leading_zeros();
    let mut x = 1u64 << ((bits + 1) / 2);
    loop {
        let y = (x + v / x) / 2;
        if y >= x {
            break;
        }
        x = y;
    }
    let x128 = x as u128;
    let v_wide = v as u128;
    while (x128 + 1) * (x128 + 1) <= v_wide {
        x += 1;
    }
    while (x as u128) * (x as u128) > v_wide {
        x -= 1;
    }
    x
}

/// Ordering key: distance squared, ID, then the candidate's index in the body
/// slice. Sorting the whole key (unique per entry) equals the source's stable sort
/// by `(distance_squared, id)`, which kept input order on ties.
type OrderKey = (u128, u32, usize);
/// A half-plane: normal in thousandths and a point in mm/s.
type Plane = ([i64; 2], [i64; 2]);

/// Cumulative kernel counters since construction or [`OrcaScratch::reset_stats`].
/// No wall-clock reads; the agent surface publishes these as rows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OrcaStats {
    /// Solver calls through this scratch, including out-of-domain ones.
    pub calls: u64,
    /// Neighbors retained after the distance/ID sort and the cap.
    pub neighbors: u64,
    /// Half-planes built (threatening retained neighbors).
    pub planes: u64,
    /// Planes that moved the velocity (violated constraints projected).
    pub projections: u64,
    /// Calls that returned the out-of-domain fallback or error.
    pub out_of_domain: u64,
    /// Candidates the spatial index delivered to indexed calls (before the exact look test).
    pub index_candidates: u64,
}

impl OrcaStats {
    /// Named counters for agent rows, in a fixed order.
    pub fn fields(&self) -> [(&'static str, u64); 6] {
        [
            ("orca_calls", self.calls),
            ("orca_neighbors", self.neighbors),
            ("orca_planes", self.planes),
            ("orca_projections", self.projections),
            ("orca_out_of_domain", self.out_of_domain),
            ("orca_index_candidates", self.index_candidates),
        ]
    }
}

/// Reusable solver buffers and counters. One per solving thread; reuse it for
/// every mover of every tick. After the buffers reach the largest candidate count
/// and cap seen, [`orca_velocity_into`] and [`orca_velocity_indexed`] allocate nothing.
#[derive(Debug, Clone, Default)]
pub struct OrcaScratch {
    order: Vec<OrderKey>,
    planes: Vec<Plane>,
    stats: OrcaStats,
    lp: lp::LpScratch,
}

impl OrcaScratch {
    /// Empty buffers; they grow on first use.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-sized buffers for up to `candidates` candidates and `planes` retained neighbors.
    pub fn with_capacity(candidates: usize, planes: usize) -> Self {
        Self {
            order: Vec::with_capacity(candidates),
            planes: Vec::with_capacity(planes),
            stats: OrcaStats::default(),
            lp: lp::LpScratch::with_capacity(planes),
        }
    }

    /// Copy the counters without resetting them.
    pub fn stats(&self) -> OrcaStats {
        self.stats
    }

    /// Reset the counters (both kernels'); buffers keep their capacity.
    pub fn reset_stats(&mut self) {
        self.stats = OrcaStats::default();
        self.lp.stats = OrcaLpStats::default();
    }

    /// Copy the LP counters without resetting them.
    pub fn lp_stats(&self) -> OrcaLpStats {
        self.lp.stats
    }

    /// The outcome of the last LP call through this scratch; `None` after a Stage A
    /// or out-of-domain call.
    pub fn last_lp(&self) -> Option<OrcaLpOutcome> {
        self.lp.last
    }

    /// The half-planes of the last LP call (walls first, then neighbors by
    /// `(distance_squared, id)`), as built, before the solver's inward margin.
    pub fn lp_planes(&self) -> &[OrcaHalfPlane] {
        &self.lp.planes
    }

    /// The half-planes of the last Stage A call: `(normal in thousandths, point in
    /// mm/s)`, admitted when `(v - point) . normal >= 0`. Read-only evidence for the
    /// single-pass behavior; Stage A projects each once in this order.
    pub fn stage_a_planes(&self) -> &[([i64; 2], [i64; 2])] {
        &self.planes
    }
}

/// One solver call's inputs, for checking the domain without solving.
#[derive(Debug, Clone, Copy)]
pub struct OrcaInput<'a> {
    /// The mover.
    pub me: &'a OrcaBody,
    /// Preferred velocity in mm/s.
    pub preferred: [i64; 2],
    /// Candidate neighbors; entries with the mover's ID are ignored.
    pub others: &'a [OrcaBody],
}

impl OrcaInput<'_> {
    /// `Ok` when every value the full-list solver reads lies inside the module's
    /// input domain. Reports the first failure: settings, mover, preference,
    /// neighbors in slice order, then the retained-neighbor bound.
    pub fn validate(&self, settings: &OrcaSettings) -> Result<(), OrcaDomainError> {
        settings.validate()?;
        validate_mover(self.me, self.preferred)?;
        let mut candidates = 0;
        for o in self.others {
            if o.id != self.me.id {
                o.validate()?;
                candidates += 1;
            }
        }
        check_cap(candidates, settings)
    }
}

fn validate_mover(me: &OrcaBody, preferred: [i64; 2]) -> Result<(), OrcaDomainError> {
    me.validate()?;
    if !in_span(preferred[0], ORCA_MAX_SPEED_MM_S) || !in_span(preferred[1], ORCA_MAX_SPEED_MM_S) {
        return Err(OrcaDomainError::Preferred);
    }
    Ok(())
}

fn check_cap(candidates: usize, settings: &OrcaSettings) -> Result<(), OrcaDomainError> {
    if candidates.min(settings.max_neighbors) > ORCA_MAX_NEIGHBOR_PLANES {
        return Err(OrcaDomainError::NeighborCap);
    }
    Ok(())
}

/// The documented out-of-domain result: the preferred velocity clamped per axis
/// to `±ORCA_MAX_SPEED_MM_S`. Identical in every build profile.
pub fn orca_out_of_domain_velocity(preferred: [i64; 2]) -> [i64; 2] {
    let max = ORCA_MAX_SPEED_MM_S;
    [preferred[0].clamp(-max, max), preferred[1].clamp(-max, max)]
}

/// Choose a best-effort avoidance velocity with compatibility defaults.
///
/// Inputs and output are millimeters per second. Equal distance ties use stable
/// IDs; self entries are ignored. See [`orca_velocity_with_settings`] for the
/// explicit settings variant and the module documentation for the input domain.
pub fn orca_velocity(me: &OrcaBody, preferred: [i64; 2], others: &[OrcaBody]) -> [i64; 2] {
    orca_velocity_with_settings(me, preferred, others, &OrcaSettings::default())
}

/// Choose a best-effort avoidance velocity with host-supplied settings.
///
/// The solver sorts candidates by distance squared and ID, truncates to
/// `settings.max_neighbors`, applies a right bias when head-on, then projects
/// each threatening neighbor's plane once. The speed clamp uses the biased
/// preference. A later projection may violate an earlier constraint.
///
/// Caller responsibilities: distinct neighbor IDs, a deterministic input snapshot
/// and matching settings across peers. Input outside the module's domain returns
/// [`orca_out_of_domain_velocity`]. This form allocates its buffers per call; use
/// [`orca_velocity_into`] with an [`OrcaScratch`] in a crowd step.
pub fn orca_velocity_with_settings(
    me: &OrcaBody,
    preferred: [i64; 2],
    others: &[OrcaBody],
    settings: &OrcaSettings,
) -> [i64; 2] {
    orca_velocity_into(&mut OrcaScratch::new(), me, preferred, others, settings)
}

/// [`orca_velocity_with_settings`] that reports out-of-domain input instead of
/// returning the fallback velocity.
pub fn try_orca_velocity(
    me: &OrcaBody,
    preferred: [i64; 2],
    others: &[OrcaBody],
    settings: &OrcaSettings,
) -> Result<[i64; 2], OrcaDomainError> {
    try_orca_velocity_into(&mut OrcaScratch::new(), me, preferred, others, settings)
}

/// [`orca_velocity_with_settings`] with caller-owned buffers: the same bits, and
/// no allocation once `scratch` has grown to the candidate count and cap.
pub fn orca_velocity_into(
    scratch: &mut OrcaScratch,
    me: &OrcaBody,
    preferred: [i64; 2],
    others: &[OrcaBody],
    settings: &OrcaSettings,
) -> [i64; 2] {
    try_orca_velocity_into(scratch, me, preferred, others, settings)
        .unwrap_or_else(|_| orca_out_of_domain_velocity(preferred))
}

/// [`orca_velocity_into`] that reports out-of-domain input as an error.
pub fn try_orca_velocity_into(
    scratch: &mut OrcaScratch,
    me: &OrcaBody,
    preferred: [i64; 2],
    others: &[OrcaBody],
    settings: &OrcaSettings,
) -> Result<[i64; 2], OrcaDomainError> {
    scratch.stats.calls += 1;
    let gathered = gather_list(&mut scratch.order, me, preferred, others, settings);
    finish(scratch, gathered.map(|()| *me), preferred, others, &[], settings)
}

/// [`orca_velocity_into`] with static wall segments. The LP solver builds one
/// half-plane per segment within reach and keeps the walls even in its fallback;
/// Stage A ignores segments (counted in [`OrcaLpStats::stage_a_segments_ignored`]).
/// Segments are read in slice order; each one's endpoint order does not matter.
/// Out-of-domain input, including more than [`ORCA_MAX_SEGMENTS`] segments or an
/// endpoint beyond [`ORCA_MAX_POSITION_MM`] (LP only), returns
/// [`orca_out_of_domain_velocity`].
///
/// ```
/// use regia_detsim::avoid::{OrcaBody, OrcaScratch, OrcaSegment, OrcaSettings, OrcaSolver, orca_velocity_obstacles_into};
///
/// let settings = OrcaSettings { solver: OrcaSolver::Lp, max_speed_mm_s: 2_000, ..OrcaSettings::default() };
/// let me = OrcaBody { id: 1, x_mm: 0, z_mm: 0, vx_mm_s: 2_000, vz_mm_s: 0, r_mm: 500 };
/// // A wall across the path, 1.5 m ahead.
/// let wall = [OrcaSegment { a_mm: [1_500, -3_000], b_mm: [1_500, 3_000] }];
/// let mut scratch = OrcaScratch::new();
/// let v = orca_velocity_obstacles_into(&mut scratch, &me, [2_000, 0], &[], &wall, &settings);
/// assert!(v[0] < 2_000, "slows for the wall");
/// assert!(scratch.last_lp().is_some_and(|o| o.feasible));
/// assert!(scratch.lp_planes().iter().all(|p| p.admits(v)));
/// ```
pub fn orca_velocity_obstacles_into(
    scratch: &mut OrcaScratch,
    me: &OrcaBody,
    preferred: [i64; 2],
    others: &[OrcaBody],
    segments: &[OrcaSegment],
    settings: &OrcaSettings,
) -> [i64; 2] {
    try_orca_velocity_obstacles_into(scratch, me, preferred, others, segments, settings)
        .unwrap_or_else(|_| orca_out_of_domain_velocity(preferred))
}

/// [`orca_velocity_obstacles_into`] that reports out-of-domain input as an error.
pub fn try_orca_velocity_obstacles_into(
    scratch: &mut OrcaScratch,
    me: &OrcaBody,
    preferred: [i64; 2],
    others: &[OrcaBody],
    segments: &[OrcaSegment],
    settings: &OrcaSettings,
) -> Result<[i64; 2], OrcaDomainError> {
    scratch.stats.calls += 1;
    let gathered = gather_list(&mut scratch.order, me, preferred, others, settings);
    finish(
        scratch,
        gathered.map(|()| *me),
        preferred,
        others,
        segments,
        settings,
    )
}

/// Neighbor rule for [`orca_velocity_indexed`]: the look distance and group mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrcaNeighborQuery {
    /// Exact look distance in millimeters: a neighbor counts when its center lies
    /// within this distance (`distance_squared <= look_mm^2`).
    pub look_mm: i64,
    /// Index group mask; a row counts when it shares a bit with this mask.
    pub mask: u64,
}

impl OrcaNeighborQuery {
    /// The settings' `look_m` (rounded by [`m_to_mm`]) with the given mask.
    pub fn from_settings(settings: &OrcaSettings, mask: u64) -> Self {
        Self {
            look_mm: m_to_mm(settings.look_m),
            mask,
        }
    }

    /// Check the look distance against `0..=ORCA_MAX_RANGE_MM`.
    pub fn validate(&self) -> Result<(), OrcaDomainError> {
        if !(0..=ORCA_MAX_RANGE_MM).contains(&self.look_mm) {
            return Err(OrcaDomainError::Look);
        }
        Ok(())
    }
}

/// Solve for the body in `me_slot` with neighbors gathered from a spatial index.
///
/// `index` holds one row per body, `bodies[slot]` being the body of row `slot`,
/// with each row's XZ within `1 mm + |p| * 2^-22` of the body's millimeter
/// position (use [`OrcaBody::position_m`]) and a nonnegative radius. The neighbor
/// set is every other body whose row matches `query.mask` and the index's floor
/// rule against the mover's row, with `distance_squared <= look_mm^2` exactly in
/// integers. The result equals [`orca_velocity_into`] over that set in slot order;
/// when the set holds at least `max_neighbors` bodies it also equals the full list
/// of all bodies (ADR-8377). Out-of-domain input, including an index/body mismatch,
/// returns [`orca_out_of_domain_velocity`].
///
/// ```
/// use regia_detsim::avoid::{OrcaBody, OrcaNeighborQuery, OrcaScratch, OrcaSettings, orca_velocity_indexed};
/// use regia_detsim::broadphase::BroadBody;
/// use regia_detsim::spatial::{DetSpatial, SpatialRow};
///
/// fn crowd_step(
///     bodies: &[OrcaBody],
///     prefs: &[[i64; 2]],
///     out: &mut [[i64; 2]],
///     index: &mut DetSpatial,
///     rows: &mut Vec<SpatialRow>,
///     scratch: &mut OrcaScratch,
/// ) {
///     let settings = OrcaSettings::default();
///     let query = OrcaNeighborQuery::from_settings(&settings, u64::MAX);
///     rows.clear();
///     rows.extend(bodies.iter().enumerate().map(|(slot, b)| {
///         let [x, z] = b.position_m();
///         SpatialRow { slot, body: BroadBody::new([x, 0.0, z], 0.0, [0.0, 2.0]), group_mask: 1, flags: 0 }
///     }));
///     if index.build(rows).is_err() {
///         return;
///     }
///     for (slot, v) in out.iter_mut().enumerate() {
///         *v = orca_velocity_indexed(scratch, index, bodies, slot, prefs[slot], &settings, &query);
///     }
/// }
///
/// let body = |id, x_mm, vx_mm_s| OrcaBody { id, x_mm, z_mm: 0, vx_mm_s, vz_mm_s: 0, r_mm: 2_000 };
/// let bodies = [body(1, -10_000, 5_000), body(2, 10_000, -5_000)];
/// let mut out = [[0; 2]; 2];
/// let (mut index, mut rows, mut scratch) = (DetSpatial::default(), Vec::new(), OrcaScratch::new());
/// crowd_step(&bodies, &[[8_000, 0], [-8_000, 0]], &mut out, &mut index, &mut rows, &mut scratch);
/// assert_eq!(out, [[4_000, -600], [-4_000, 600]]);
/// ```
pub fn orca_velocity_indexed(
    scratch: &mut OrcaScratch,
    index: &mut DetSpatial,
    bodies: &[OrcaBody],
    me_slot: usize,
    preferred: [i64; 2],
    settings: &OrcaSettings,
    query: &OrcaNeighborQuery,
) -> [i64; 2] {
    try_orca_velocity_indexed(scratch, index, bodies, me_slot, preferred, settings, query)
        .unwrap_or_else(|_| orca_out_of_domain_velocity(preferred))
}

/// [`orca_velocity_indexed`] that reports out-of-domain input as an error. Only
/// bodies inside the look distance are checked against the domain.
pub fn try_orca_velocity_indexed(
    scratch: &mut OrcaScratch,
    index: &mut DetSpatial,
    bodies: &[OrcaBody],
    me_slot: usize,
    preferred: [i64; 2],
    settings: &OrcaSettings,
    query: &OrcaNeighborQuery,
) -> Result<[i64; 2], OrcaDomainError> {
    scratch.stats.calls += 1;
    let gathered = gather_indexed(scratch, index, bodies, me_slot, preferred, settings, query);
    finish(scratch, gathered, preferred, bodies, &[], settings)
}

fn finish(
    scratch: &mut OrcaScratch,
    gathered: Result<OrcaBody, OrcaDomainError>,
    preferred: [i64; 2],
    bodies: &[OrcaBody],
    segments: &[OrcaSegment],
    settings: &OrcaSettings,
) -> Result<[i64; 2], OrcaDomainError> {
    scratch.lp.last = None;
    let checked = gathered.and_then(|me| match settings.solver {
        OrcaSolver::StageA => Ok(me),
        OrcaSolver::Lp => {
            if segments.len() > ORCA_MAX_SEGMENTS {
                return Err(OrcaDomainError::SegmentCap);
            }
            segments.iter().try_for_each(OrcaSegment::validate)?;
            Ok(me)
        }
    });
    let me = match checked {
        Ok(me) => me,
        Err(e) => {
            scratch.stats.out_of_domain += 1;
            return Err(e);
        }
    };
    Ok(match settings.solver {
        OrcaSolver::StageA => {
            scratch.lp.stats.stage_a_segments_ignored += segments.len() as u64;
            solve(scratch, &me, preferred, bodies, settings)
        }
        OrcaSolver::Lp => solve_lp(scratch, &me, preferred, bodies, segments, settings),
    })
}

/// The LP path: the same neighbor selection and head-on bias as [`solve`] (repeated
/// here so the Stage A kernel stays byte-for-byte as pinned), then `lp::solve`.
fn solve_lp(
    scratch: &mut OrcaScratch,
    me: &OrcaBody,
    preferred: [i64; 2],
    bodies: &[OrcaBody],
    segments: &[OrcaSegment],
    settings: &OrcaSettings,
) -> [i64; 2] {
    let OrcaScratch { order, stats, lp, .. } = scratch;
    let cap = settings.max_neighbors;
    if order.len() > cap {
        order.select_nth_unstable(cap);
        order.truncate(cap);
    }
    order.sort_unstable();
    stats.neighbors += order.len() as u64;
    let mut pref = preferred;
    if prefers_motion(pref, settings) && head_on(me, order, bodies, settings) {
        let plen = isqrt_floor((pref[0] as i128 * pref[0] as i128 + pref[1] as i128 * pref[1] as i128) as u64)
            as i64;
        if plen > 0 {
            let bx = pref[1] * settings.keep_right_mm_s / plen;
            let b_z = -pref[0] * settings.keep_right_mm_s / plen;
            pref[0] += bx;
            pref[1] += b_z;
        }
    }
    let v = lp::solve(lp, me, [pref, preferred], order, bodies, segments, settings);
    stats.planes += lp.planes.len() as u64;
    v
}

/// The source's distance square; in the domain it is below 2^65.
#[inline]
fn dist2(me: &OrcaBody, o: &OrcaBody) -> u128 {
    let dx = o.x_mm as i128 - me.x_mm as i128;
    let dz = o.z_mm as i128 - me.z_mm as i128;
    (dx * dx + dz * dz) as u128
}

fn gather_list(
    order: &mut Vec<OrderKey>,
    me: &OrcaBody,
    preferred: [i64; 2],
    others: &[OrcaBody],
    settings: &OrcaSettings,
) -> Result<(), OrcaDomainError> {
    settings.validate()?;
    validate_mover(me, preferred)?;
    order.clear();
    for (i, o) in others.iter().enumerate() {
        if o.id == me.id {
            continue;
        }
        o.validate()?;
        order.push((dist2(me, o), o.id, i));
    }
    check_cap(order.len(), settings)
}

/// Index query radius in meters that covers every body within `look_mm`: the
/// look distance plus both rows' position tolerance (`1 mm + |p| * 2^-22` each,
/// where the neighbor's `|p| <= |p_me| + look`), with a 4x and 0.1 % margin for
/// the f32 conversion. Over-gathering is safe: the exact integer test follows.
fn index_query_radius_m(me: &OrcaBody, look_mm: i64) -> f32 {
    let reach = me.x_mm.unsigned_abs().max(me.z_mm.unsigned_abs());
    let look = look_mm.unsigned_abs();
    let slack = 4 + ((2 * reach + look) >> 20);
    let total = look + slack;
    total as f32 * (1.001 / 1000.0)
}

fn gather_indexed(
    scratch: &mut OrcaScratch,
    index: &mut DetSpatial,
    bodies: &[OrcaBody],
    me_slot: usize,
    preferred: [i64; 2],
    settings: &OrcaSettings,
    query: &OrcaNeighborQuery,
) -> Result<OrcaBody, OrcaDomainError> {
    settings.validate()?;
    query.validate()?;
    let me = *bodies.get(me_slot).ok_or(OrcaDomainError::Slot)?;
    validate_mover(&me, preferred)?;
    let center = index.row(me_slot).ok_or(OrcaDomainError::Slot)?.body;
    let radius = index_query_radius_m(&me, query.look_mm);
    let look = query.look_mm.unsigned_abs() as u128;
    let look2 = look * look;
    let order = &mut scratch.order;
    order.clear();
    let mut delivered = 0_u64;
    let mut failure = None;
    let _ = index.within(center, radius, query.mask, |slot| {
        delivered += 1;
        let Some(o) = bodies.get(slot) else {
            failure = Some(OrcaDomainError::Slot);
            return ControlFlow::Break(());
        };
        if o.id == me.id {
            return ControlFlow::Continue(());
        }
        // Wide distance square valid for any i64 position: each squared axis
        // offset fits u128; the sum saturates (far outside any look distance).
        let ax = (o.x_mm as i128 - me.x_mm as i128).unsigned_abs();
        let az = (o.z_mm as i128 - me.z_mm as i128).unsigned_abs();
        let d2 = (ax * ax).saturating_add(az * az);
        if d2 > look2 {
            return ControlFlow::Continue(());
        }
        if let Err(e) = o.validate() {
            failure = Some(e);
            return ControlFlow::Break(());
        }
        // Inside the look distance the sum cannot saturate (< 2^67), so this is
        // exactly the source's distance square.
        order.push((d2, o.id, slot));
        ControlFlow::Continue(())
    });
    scratch.stats.index_candidates += delivered;
    if let Some(e) = failure {
        return Err(e);
    }
    check_cap(scratch.order.len(), settings)?;
    Ok(me)
}

/// The pinned source arithmetic over an already gathered, in-domain candidate list.
fn solve(
    scratch: &mut OrcaScratch,
    me: &OrcaBody,
    preferred: [i64; 2],
    bodies: &[OrcaBody],
    settings: &OrcaSettings,
) -> [i64; 2] {
    let OrcaScratch {
        order, planes, stats, ..
    } = scratch;
    // Fixed order: nearest first, ids break distance ties. Partial selection
    // keeps the cap's members; the full key makes the result unique.
    let cap = settings.max_neighbors;
    if order.len() > cap {
        order.select_nth_unstable(cap);
        order.truncate(cap);
    }
    order.sort_unstable();
    stats.neighbors += order.len() as u64;

    let mut pref = preferred;
    // Head-on: both units shift to their own right before solving, so the
    // pair passes on a steady side. Each unit does this in its own frame,
    // which makes the pair reciprocal in the world frame.
    if prefers_motion(pref, settings) && head_on(me, order, bodies, settings) {
        let plen = isqrt_floor((pref[0] as i128 * pref[0] as i128 + pref[1] as i128 * pref[1] as i128) as u64)
            as i64;
        if plen > 0 {
            // Right of the direction of travel: [z, -x], as in the old rule.
            let bx = pref[1] * settings.keep_right_mm_s / plen;
            let b_z = -pref[0] * settings.keep_right_mm_s / plen;
            pref[0] += bx;
            pref[1] += b_z;
        }
    }

    // One half-plane per threatening neighbor: normal (in thousandths)
    // and a point (mm/s) the new velocity must stay on the free side of.
    planes.clear();
    for (_, _, i) in order.iter() {
        if let Some(plane) = plane_for(me, &bodies[*i], settings) {
            planes.push(plane);
        }
    }
    stats.planes += planes.len() as u64;

    // Single pass in fixed order: project back onto each violated line.
    // One division per axis: quotient-then-multiply would truncate small
    // but real corrections away to zero.
    let mut v = pref;
    for (n, p) in planes.iter() {
        let dot = (v[0] - p[0]) * n[0] + (v[1] - p[1]) * n[1];
        if dot < 0 {
            let nn = n[0] * n[0] + n[1] * n[1];
            if nn == 0 {
                continue;
            }
            stats.projections += 1;
            v[0] = (v[0] as i128 - dot as i128 * n[0] as i128 / nn as i128) as i64;
            v[1] = (v[1] as i128 - dot as i128 * n[1] as i128 / nn as i128) as i64;
        }
    }
    // Source speed clamp: uses the biased preference and floor integer roots.
    let smax2 = pref[0] * pref[0] + pref[1] * pref[1];
    let v2 = v[0] * v[0] + v[1] * v[1];
    if v2 > smax2 {
        let len = isqrt_floor(v2 as u64) as i64;
        let max = isqrt_floor(smax2 as u64) as i64;
        if len == 0 {
            return [0, 0];
        }
        v[0] = v[0] * max / len;
        v[1] = v[1] * max / len;
    }
    v
}

#[inline]
fn prefers_motion(pref: [i64; 2], settings: &OrcaSettings) -> bool {
    pref[0] * pref[0] + pref[1] * pref[1] >= settings.min_preferred_mm_s * settings.min_preferred_mm_s
}

/// A neighbor is head-on when it approaches with a lateral offset under
/// the combined radius: `cross^2 < rr^2 * |w|^2` avoids any division.
fn head_on(
    me: &OrcaBody,
    order: &[(u128, u32, usize)],
    others: &[OrcaBody],
    settings: &OrcaSettings,
) -> bool {
    let range2 = settings.head_on_range_mm as i128 * settings.head_on_range_mm as i128;
    for (d2, _, i) in order {
        if (*d2 as i128) > range2 {
            continue;
        }
        let o = &others[*i];
        let px = o.x_mm as i128 - me.x_mm as i128;
        let pz = o.z_mm as i128 - me.z_mm as i128;
        let rr = me.r_mm as i128 + o.r_mm as i128;
        let wx = me.vx_mm_s as i128 - o.vx_mm_s as i128;
        let wz = me.vz_mm_s as i128 - o.vz_mm_s as i128;
        let ww = wx * wx + wz * wz;
        if ww == 0 {
            continue;
        }
        // Closing when the pair shrinks the gap: p(t) = p - w t, so the
        // distance falls while p . w stays positive.
        if px * wx + pz * wz <= 0 {
            continue;
        }
        let cross = px * wz - pz * wx;
        if cross * cross < rr * rr * ww {
            return true;
        }
    }
    false
}

/// The reciprocal half-plane for one neighbor, or `None` when the pair
/// is safely clear for the whole horizon. Predicted collisions share half the
/// needed change; contact instead uses the neighbor's normal velocity.
fn plane_for(me: &OrcaBody, o: &OrcaBody, settings: &OrcaSettings) -> Option<([i64; 2], [i64; 2])> {
    let px = o.x_mm as i128 - me.x_mm as i128;
    let pz = o.z_mm as i128 - me.z_mm as i128;
    let rr = me.r_mm as i128 + o.r_mm as i128;
    let d2 = px * px + pz * pz;
    let wx = me.vx_mm_s as i128 - o.vx_mm_s as i128;
    let wz = me.vz_mm_s as i128 - o.vz_mm_s as i128;
    if d2 < rr * rr {
        // In contact: do not close any faster than the other unit moves.
        // The normal points from self to other; the line sits on the other
        // unit's velocity, so a tailgater matches the leader instead of
        // ramming it, and a hull against a parked one stops.
        let d = isqrt_floor(d2 as u64) as i128;
        let towards = if d == 0 {
            if me.id < o.id { [1000, 0] } else { [-1000, 0] }
        } else {
            [(px * 1000 / d) as i64, (pz * 1000 / d) as i64]
        };
        let n = [-towards[0], -towards[1]];
        return Some((n, [o.vx_mm_s, o.vz_mm_s]));
    }
    let ww = wx * wx + wz * wz;
    if ww == 0 {
        return None;
    }
    // Relative motion is p(t) = p - w t: closing while this is positive.
    let closing = px * wx + pz * wz;
    if closing <= 0 {
        return None;
    }
    let t = closing * 1000 / ww;
    if t <= 0 || t > settings.horizon_ms as i128 {
        return None;
    }
    let cx = px - wx * t / 1000;
    let cz = pz - wz * t / 1000;
    let c2 = cx * cx + cz * cz;
    if c2 >= rr * rr {
        return None;
    }
    // Smallest change to the relative velocity that makes the discs graze.
    // It opposes the out-direction: the pair slows or bends instead of
    // adding to the closing motion.
    let c = isqrt_floor(c2 as u64) as i128;
    let out = if c == 0 {
        if me.id < o.id { [1000, 0] } else { [-1000, 0] }
    } else {
        [(cx * 1000 / c) as i64, (cz * 1000 / c) as i64]
    };
    let depth = rr - c;
    let rate = depth * 1000 / t;
    let ux = -(out[0] as i128) * rate / 1000;
    let uz = -(out[1] as i128) * rate / 1000;
    let mu2 = ux * ux + uz * uz;
    if mu2 == 0 {
        return None;
    }
    let mu = isqrt_floor(mu2 as u64) as i128;
    let n = [(ux * 1000 / mu) as i64, (uz * 1000 / mu) as i64];
    // Half the change is ours; the other unit mirrors this computation.
    let point = [
        (me.vx_mm_s as i128 + ux / 2) as i64,
        (me.vz_mm_s as i128 + uz / 2) as i64,
    ];
    Some((n, point))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(id: u32, x_m: f32, z_m: f32, vx: f32, vz: f32, r_m: f32) -> OrcaBody {
        OrcaBody {
            id,
            x_mm: m_to_mm(x_m),
            z_mm: m_to_mm(z_m),
            vx_mm_s: m_to_mm(vx),
            vz_mm_s: m_to_mm(vz),
            r_mm: m_to_mm(r_m),
        }
    }

    #[test]
    fn clear_path_keeps_preferred_velocity() {
        let me = body(1, 0.0, 0.0, 5.0, 0.0, 2.0);
        let out = orca_velocity(&me, [m_to_mm(8.0), 0], &[]);
        assert_eq!(out, [m_to_mm(8.0), 0]);
    }

    #[test]
    fn head_on_pair_passes_on_opposite_sides() {
        // Two hulls closing on the x axis must pick complementary lateral
        // shifts: what one gains in +z the other gains in -z.
        let a = body(1, -10.0, 0.0, 5.0, 0.0, 2.0);
        let b = body(2, 10.0, 0.0, -5.0, 0.0, 2.0);
        let va = orca_velocity(&a, [m_to_mm(8.0), 0], &[b]);
        let vb = orca_velocity(&b, [m_to_mm(-8.0), 0], &[a]);
        assert_eq!(va[1], -vb[1], "reciprocal lateral shift");
        assert_ne!(va[1], 0, "the pair must separate laterally");
    }

    #[test]
    fn neighbour_order_does_not_change_the_answer() {
        let me = body(1, 0.0, 0.0, 5.0, 0.0, 2.0);
        let others = vec![
            body(2, 12.0, 1.0, -4.0, 0.0, 2.0),
            body(3, 9.0, -3.0, 0.0, 2.0, 2.0),
            body(4, 15.0, 0.5, -6.0, 0.5, 2.0),
        ];
        let mut shuffled = others.clone();
        shuffled.reverse();
        let pref = [m_to_mm(8.0), 0];
        assert_eq!(
            orca_velocity(&me, pref, &others),
            orca_velocity(&me, pref, &shuffled)
        );
    }

    #[test]
    fn only_the_nearest_neighbours_count() {
        let me = body(1, 0.0, 0.0, 5.0, 0.0, 2.0);
        let near: Vec<OrcaBody> = (0..ORCA_MAX_NEIGHBOURS as u32)
            .map(|k| body(10 + k, 8.0 + k as f32, 1.0, -5.0, 0.0, 2.0))
            .collect();
        let far: Vec<OrcaBody> = (0..6)
            .map(|k| body(100 + k, 60.0 + k as f32 * 9.0, 0.0, -5.0, 0.0, 2.0))
            .collect();
        let pref = [m_to_mm(8.0), 0];
        let mut with_far = near.clone();
        with_far.extend(far);
        assert_eq!(
            orca_velocity(&me, pref, &near),
            orca_velocity(&me, pref, &with_far)
        );
    }

    #[test]
    fn parked_hull_is_steered_round_not_through() {
        let me = body(1, 0.0, 0.0, 6.0, 0.0, 2.0);
        let rock = body(2, 12.0, 0.0, 0.0, 0.0, 2.0);
        let out = orca_velocity(&me, [m_to_mm(8.0), 0], &[rock]);
        assert_ne!(out[1], 0, "must deviate round the parked hull");
    }

    #[test]
    fn overlapping_discs_separate() {
        let me = body(1, 0.0, 0.0, 0.0, 0.0, 2.0);
        let o = body(2, 1.0, 0.0, 0.0, 0.0, 2.0);
        let out = orca_velocity(&me, [m_to_mm(2.0), 0], &[o]);
        assert!(out[0] <= m_to_mm(2.0), "contact must not push forward: {out:?}");
    }

    #[test]
    fn isqrt_floor_matches_whole_roots_and_floors() {
        assert_eq!(isqrt_floor(0), 0);
        assert_eq!(isqrt_floor(1), 1);
        assert_eq!(isqrt_floor(144), 12);
        assert_eq!(isqrt_floor(145), 12);
        assert_eq!(isqrt_floor(8_000_000_000_000), 2_828_427);
    }
}

#[cfg(test)]
mod integer_root_domain_tests {
    use super::isqrt_floor;

    fn assert_floor(value: u64) {
        let root = u128::from(isqrt_floor(value));
        let value = u128::from(value);
        assert!(root * root <= value, "root too large for {value}");
        assert!((root + 1) * (root + 1) > value, "root too small for {value}");
    }

    #[test]
    fn descending_newton_reaches_floor_across_the_integer_domain() {
        for value in 0..=u64::from(u16::MAX) {
            assert_floor(value);
        }
        for root in [1_u64, 2, 3, 65_535, 65_536, u64::from(u32::MAX)] {
            let square = root * root;
            for value in [square - 1, square, square + 1] {
                assert_floor(value);
            }
        }
        assert_floor(u64::MAX);
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..4096 {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            assert_floor(state);
        }
    }
}
