// SPDX-License-Identifier: MPL-2.0
// Sources (docs/provenance.md, ADR-3117): pure pursuit from R. C. Coulter, "Implementation of the Pure
// Pursuit Path Tracking Algorithm", CMU-RI-TR-92-01 (1992); the intelligent driver model from Treiber,
// Hennecke and Helbing, Phys. Rev. E 62, 1805 (2000); conditional-integration anti-windup as described
// in Åström and Murray, *Feedback Systems* (integrator windup). The speed plan, the neighbour grid and the stuck
// recovery are original design. All code is Regia Engine's own.

//! AI drivers for physical vehicles on lane graphs (ADR-3117, `docs/specs/vehicle-ai.md`).
//!
//! A [`VehicleDriver`] on a vehicle entity (one made by `regia_physics::vehicle::spawn_vehicle`)
//! drives it along a [`NavLaneGraph`] route by writing the entity's [`VehicleInput`] every fixed
//! step, as a player would. It never touches the vehicle's internals, so the car skids, understeers
//! and collides under the same physics as the player's car, with either drivetrain.
//!
//! - **Route**: [`VehicleDriver::drive_to`] takes a [`DriveTarget`]: a lane and a distance along it,
//!   a world point snapped to the nearest lane, or a loop through a lane. The route comes from
//!   [`NavLaneGraph::find_route`] and the path from [`NavLaneGraph::route_points`] with the
//!   driver's lateral offset, resampled to a uniform spacing once.
//! - **Speed plan** (once per route): a curve speed `sqrt(a_lat / κ)` per path point capped by the
//!   lane's limit, a backward pass for the braking limit (ending at 0 at the route end) and a forward
//!   pass for the acceleration limit.
//! - **Steering**: pure pursuit from the rear axle with a speed-scaled look-ahead, clamped to the
//!   config's `max_steer_angle`; a goal behind the car gives full lock toward it.
//! - **Pedals**: acceleration feedforward from the plan plus a speed-error term, then a PI pedal
//!   controller with anti-windup. Braking uses `brake`; a negative throttle only reverses.
//! - **Following**: an intelligent driver model against the nearest vehicle ahead on the driver's
//!   path, found through a per-step grid of every vehicle. A disabled lane on the route is a stop line.
//! - **Stuck recovery**: counted in fixed steps (no wall clock): reverse with counter-steer, then
//!   resume; after [`VehicleDriverParams::max_recoveries`] failed attempts the driver reports
//!   [`VehicleDriverState::Stuck`] and holds the brake.
//!
//! Add [`VehicleAiPlugin`] after `PhysicsPlugin` and `VehiclePlugin`, and insert a [`NavLaneGraph`]
//! resource. The agent methods are `vehicle_ai.*` (`book/agents/vehicle-ai.md`).

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det_atan, det_powf};
use regia_ecs::spawn_order::{SpawnKey, SpawnOrder};
use regia_nav::{NavLaneGraph, NavLaneRoute};
use regia_physics::convert::{from_rapier_pose, from_rapier_vec3};
use regia_physics::step::sync_velocity_and_forces;
use regia_physics::vehicle::{VehicleConfig, VehicleController, VehicleInput, drive_vehicles};
use regia_physics::vehicle_drivetrain::Drivetrain;
use regia_physics::{PhysicsStep, PhysicsWorld};
use regia_scene::layers::LayerParked;
use thiserror::Error;

/// The most points one planned path holds (a loop holds two laps of points).
pub const MAX_PATH_POINTS: usize = 20_000;

/// The side of one neighbour-grid cell, metres.
const GRID_CELL: f32 = 32.0;

/// Spacing of the resampled path, metres.
const PATH_SPACING: f32 = 1.0;

/// Points on each side of a path point used for its curvature (about 3 m at the path spacing).
const CURVATURE_SPAN: usize = 3;

/// Segments behind and ahead of the cursor searched for the car's nearest path point each step.
const CURSOR_BACK: usize = 3;
const CURSOR_AHEAD: usize = 60;

/// Planning distance for disabled lanes ahead, metres.
const BLOCK_HORIZON: f32 = 200.0;

/// Clearance kept in front of a disabled lane's start, metres (beyond the car's half length).
const BLOCK_MARGIN: f32 = 1.0;

/// Below this curvature a path point counts as straight (1/m).
const MIN_CURVATURE: f32 = 1.0e-5;

/// Target speed the plan never goes under at the car's own position, so a standing car sets off (m/s).
const START_SPEED: f32 = 1.5;

/// Lowest full-throttle acceleration the pedal controller assumes (m/s²): a heavy car on a tiny
/// engine would otherwise get huge pedal gains.
const MIN_FULL_ACCEL: f32 = 0.5;

/// The full-throttle acceleration assumed for a geared drivetrain, whose force depends on the gear
/// (m/s²). The integral term absorbs the difference.
const GEARED_FULL_ACCEL: f32 = 3.0;

/// Extra clearance a car queued behind a stopped leader keeps beyond the IDM minimum gap, metres.
const QUEUE_MARGIN: f32 = 0.5;

/// The integral term's bound, in pedal units.
const INTEGRAL_LIMIT: f32 = 0.5;

// ---------------------------------------------------------------------------
// Errors, states, parameters
// ---------------------------------------------------------------------------

/// Why a driver could not plan or accept a setting. [`VehicleAiError::code`] is the stable code
/// agents see.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VehicleAiError {
    /// There is no `NavLaneGraph` resource.
    #[error("no NavLaneGraph resource")]
    NoLaneGraph,
    /// The lane does not exist.
    #[error("lane {0} does not exist")]
    UnknownLane(u32),
    /// No open lane leads from the car's lane to the target.
    #[error("no route from lane {from} to lane {to}")]
    NoRoute {
        /// The car's lane.
        from: u32,
        /// The target lane.
        to: u32,
    },
    /// The car (or the target point) is farther than the snap reach from every open lane.
    #[error("no open lane within reach of the {0}")]
    OffLanes(&'static str),
    /// The path would have more than [`MAX_PATH_POINTS`] points.
    #[error("the route is longer than the path limit")]
    RouteTooLong,
    /// A parameter is out of range.
    #[error("parameter `{0}` is out of range")]
    InvalidParam(&'static str),
    /// The vehicle's chassis is not in the physics world.
    #[error("the vehicle's chassis is not in the physics world")]
    NoChassis,
}

impl VehicleAiError {
    /// The stable error code (lowercase ascii, `vehicle_ai_*`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoLaneGraph => "vehicle_ai_no_lane_graph",
            Self::UnknownLane(_) => "vehicle_ai_unknown_lane",
            Self::NoRoute { .. } => "vehicle_ai_no_route",
            Self::OffLanes(_) => "vehicle_ai_off_lanes",
            Self::RouteTooLong => "vehicle_ai_route_too_long",
            Self::InvalidParam(_) => "vehicle_ai_bad_param",
            Self::NoChassis => "vehicle_ai_no_chassis",
        }
    }
}

/// What a driver is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VehicleDriverState {
    /// No target: the driver holds the brake.
    #[default]
    Idle,
    /// Driving the route at the planned speed.
    Driving,
    /// Driving, slowed by the vehicle ahead.
    Following,
    /// Stopped (or stopping) in front of a disabled lane on the route; drives on when it reopens.
    Blocked,
    /// Stopped at the route end; holds the brake.
    Arrived,
    /// Reversing with counter-steer out of a stuck position.
    Recovering,
    /// Gave up after too many failed recoveries; holds the brake until a new target.
    Stuck,
}

impl VehicleDriverState {
    /// The state's name as agents see it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Driving => "driving",
            Self::Following => "following",
            Self::Blocked => "blocked",
            Self::Arrived => "arrived",
            Self::Recovering => "recovering",
            Self::Stuck => "stuck",
        }
    }
}

/// Where a driver goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriveTarget {
    /// A distance along a lane (clamped to it). A target behind the car on its own lane goes once
    /// around through the lane's successors.
    Lane {
        /// The lane index.
        lane: u32,
        /// Metres along the lane.
        distance: f32,
    },
    /// A world point, snapped to the nearest open lane within `reach` metres.
    Point {
        /// The point.
        point: Vec3,
        /// Snap reach, metres.
        reach: f32,
    },
    /// Laps the shortest cycle through `lane` forever (no stop).
    Loop {
        /// A lane of the cycle.
        lane: u32,
    },
}

/// A driver's tuning. Every field must be finite; [`VehicleDriverParams::validate`] names the first
/// one out of range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VehicleDriverParams {
    /// Lane filter bits for routing and snapping (`u16::MAX` = any lane).
    pub flags: u16,
    /// Path offset right of the lane centre, metres (negative = left).
    pub lateral_offset: f32,
    /// How far the car may be from a lane when it plans, metres (> 0).
    pub snap_reach: f32,
    /// Speed cap, m/s (> 0). The lane's limit applies too.
    pub max_speed: f32,
    /// Lateral acceleration the plan allows in curves, m/s² (> 0).
    pub max_lateral_accel: f32,
    /// Acceleration the plan and the controller allow, m/s² (> 0).
    pub max_accel: f32,
    /// Deceleration the plan uses before curves and stops, m/s² (> 0).
    pub max_decel: f32,
    /// Deceleration the controller may command in an emergency (a car cutting in), m/s²
    /// (≥ `max_decel`).
    pub emergency_decel: f32,
    /// Shortest look-ahead, metres (> 0).
    pub look_ahead_min: f32,
    /// Look-ahead per m/s of speed, seconds (≥ 0).
    pub look_ahead_time: f32,
    /// Longest look-ahead, metres (≥ `look_ahead_min`).
    pub look_ahead_max: f32,
    /// How far ahead (seconds of travel) the target speed is read from the plan (≥ 0).
    pub speed_preview: f32,
    /// Speed-error gain, 1/s (> 0).
    pub speed_gain: f32,
    /// Integral gain of the pedal controller, pedal per metre (≥ 0).
    pub speed_integral: f32,
    /// IDM desired time headway, seconds (> 0).
    pub time_headway: f32,
    /// IDM minimum bumper-to-bumper gap, metres (> 0).
    pub min_gap: f32,
    /// IDM acceleration exponent (> 0, usually 4).
    pub idm_exponent: f32,
    /// IDM comfortable deceleration, m/s² (> 0).
    pub comfort_decel: f32,
    /// How far ahead the driver looks for a leader, metres (> 0, at most 200).
    pub follow_range: f32,
    /// The route end counts as reached within this distance at a crawl, metres (> 0).
    pub arrive_distance: f32,
    /// Steps with throttle asked but no progress before a recovery (> 0).
    pub stuck_steps: u32,
    /// Forward speed under which a car asked to move counts as making no progress, m/s (> 0).
    pub stuck_speed: f32,
    /// Steps one recovery reverses for (> 0).
    pub reverse_steps: u32,
    /// Throttle while reversing, in (0, 1].
    pub reverse_throttle: f32,
    /// Failed recoveries in a row before the driver gives up.
    pub max_recoveries: u32,
    /// Metres a car must advance after a recovery for it to count as a success (> 0).
    pub recovery_progress: f32,
}

impl Default for VehicleDriverParams {
    fn default() -> Self {
        Self {
            flags: u16::MAX,
            lateral_offset: 0.0,
            snap_reach: 10.0,
            max_speed: 25.0,
            max_lateral_accel: 4.0,
            max_accel: 2.5,
            max_decel: 4.0,
            emergency_decel: 9.0,
            look_ahead_min: 4.0,
            look_ahead_time: 0.6,
            look_ahead_max: 20.0,
            speed_preview: 0.4,
            speed_gain: 2.0,
            speed_integral: 0.05,
            time_headway: 1.2,
            min_gap: 2.0,
            idm_exponent: 4.0,
            comfort_decel: 3.0,
            follow_range: 60.0,
            arrive_distance: 1.0,
            stuck_steps: 90,
            stuck_speed: 0.3,
            reverse_steps: 80,
            reverse_throttle: 0.6,
            max_recoveries: 3,
            recovery_progress: 4.0,
        }
    }
}

impl VehicleDriverParams {
    /// The first field out of range, as [`VehicleAiError::InvalidParam`].
    pub fn validate(&self) -> Result<(), VehicleAiError> {
        let pos = |v: f32| v.is_finite() && v > 0.0;
        let non_neg = |v: f32| v.is_finite() && v >= 0.0;
        let checks: [(bool, &'static str); 24] = [
            (self.lateral_offset.is_finite(), "lateral_offset"),
            (pos(self.snap_reach), "snap_reach"),
            (pos(self.max_speed), "max_speed"),
            (pos(self.max_lateral_accel), "max_lateral_accel"),
            (pos(self.max_accel), "max_accel"),
            (pos(self.max_decel), "max_decel"),
            (
                pos(self.emergency_decel) && self.emergency_decel >= self.max_decel,
                "emergency_decel",
            ),
            (pos(self.look_ahead_min), "look_ahead_min"),
            (non_neg(self.look_ahead_time), "look_ahead_time"),
            (
                self.look_ahead_max.is_finite() && self.look_ahead_max >= self.look_ahead_min,
                "look_ahead_max",
            ),
            (non_neg(self.speed_preview), "speed_preview"),
            (pos(self.speed_gain), "speed_gain"),
            (non_neg(self.speed_integral), "speed_integral"),
            (pos(self.time_headway), "time_headway"),
            (pos(self.min_gap), "min_gap"),
            (pos(self.idm_exponent), "idm_exponent"),
            (pos(self.comfort_decel), "comfort_decel"),
            (
                pos(self.follow_range) && self.follow_range <= 200.0,
                "follow_range",
            ),
            (pos(self.arrive_distance), "arrive_distance"),
            (self.stuck_steps > 0, "stuck_steps"),
            (pos(self.stuck_speed), "stuck_speed"),
            (self.reverse_steps > 0, "reverse_steps"),
            (
                pos(self.reverse_throttle) && self.reverse_throttle <= 1.0,
                "reverse_throttle",
            ),
            (pos(self.recovery_progress), "recovery_progress"),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, name)) => Err(VehicleAiError::InvalidParam(name)),
            None => Ok(()),
        }
    }
}

/// What a driver saw and did at its last step.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VehicleDriverStatus {
    /// The driver's state.
    pub state: VehicleDriverState,
    /// Distance of the car along its path, metres (within the current lap for a loop).
    pub station: f32,
    /// Distance left to the stop line (route end or a disabled lane), metres; `None` on a loop
    /// with no disabled lane ahead.
    pub remaining: Option<f32>,
    /// Forward speed, m/s (negative when reversing).
    pub speed: f32,
    /// The speed the controller aimed for, m/s.
    pub target_speed: f32,
    /// The planned curve speed at the car's position (`min(limit, max_speed, sqrt(a_lat/κ))`), m/s.
    pub curve_speed: f32,
    /// Signed distance of the chassis from the path, metres (positive = right of it).
    pub cross_track: f32,
    /// Bumper-to-bumper gap to the leader, metres.
    pub gap: Option<f32>,
    /// The vehicle ahead on the path within the follow range.
    pub leader: Option<Entity>,
    /// The input written this step.
    pub input: VehicleInput,
    /// Recoveries started since the last target.
    pub recoveries: u32,
    /// Recoveries in a row that ended stuck again.
    pub failed_recoveries: u32,
    /// Completed laps of a loop.
    pub laps: u32,
    /// Steps driven since the last target.
    pub steps: u64,
    /// The last planning error, if any.
    pub last_error: Option<VehicleAiError>,
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// A planned path: uniform points with their curve and plan speeds.
#[derive(Debug, Clone, Default, PartialEq)]
struct Plan {
    /// The route's lanes (two laps of the cycle for a loop).
    route: Vec<u32>,
    /// Path station where each route lane starts.
    lane_start: Vec<f32>,
    /// Points, uniform spacing.
    points: Vec<Vec3>,
    /// The route index of each point's lane.
    tags: Vec<u32>,
    /// Curve speed at each point.
    curve: Vec<f32>,
    /// Plan speed at each point.
    speed: Vec<f32>,
    /// Half the lane width at each point.
    half_width: Vec<f32>,
    /// Spacing of the points, metres.
    spacing: f32,
    /// Points per lap (loops only; 0 otherwise).
    lap_points: usize,
    /// The nearest segment's start index.
    cursor: usize,
}

impl Plan {
    fn looped(&self) -> bool {
        self.lap_points > 0
    }

    fn end_station(&self) -> f32 {
        self.points.len().saturating_sub(1) as f32 * self.spacing
    }

    /// The point and unit direction at `station` (clamped to the path).
    fn sample(&self, station: f32) -> (Vec3, Vec3) {
        let n = self.points.len();
        if n < 2 {
            return (self.points.first().copied().unwrap_or(Vec3::ZERO), Vec3::ZERO);
        }
        let f = (station / self.spacing).clamp(0.0, (n - 1) as f32);
        let i = (f as usize).min(n - 2);
        let t = f - i as f32;
        let (a, b) = (self.points[i], self.points[i + 1]);
        (a + (b - a) * t, flat(b - a).normalize_or_zero())
    }

    /// Linear read of `values` at `station`.
    fn read(&self, values: &[f32], station: f32) -> f32 {
        let n = values.len();
        if n == 0 {
            return 0.0;
        }
        if n == 1 {
            return values[0];
        }
        let f = (station / self.spacing).clamp(0.0, (n - 1) as f32);
        let i = (f as usize).min(n - 2);
        let t = f - i as f32;
        values[i] + (values[i + 1] - values[i]) * t
    }

    /// Projects `p` on the segments `from..to` (start indices): `(station, signed lateral, segment)`.
    fn project(&self, p: Vec3, from: usize, to: usize) -> Option<(f32, f32, usize)> {
        let last = self.points.len().checked_sub(2)?;
        let (from, to) = (from.min(last), to.min(last));
        let mut best: Option<(f32, f32, f32, usize)> = None;
        for i in from..=to {
            let (a, b) = (self.points[i], self.points[i + 1]);
            let ab = flat(b - a);
            let len2 = ab.length_squared();
            if len2 <= 0.0 {
                continue;
            }
            let ap = flat(p - a);
            let t = (ap.dot(ab) / len2).clamp(0.0, 1.0);
            let off = ap - ab * t;
            let d2 = off.length_squared();
            if best.is_none_or(|(bd, ..)| d2 < bd) {
                let right = right_of(ab.normalize_or_zero());
                best = Some((d2, (i as f32 + t) * self.spacing, off.dot(right), i));
            }
        }
        best.map(|(_, s, lat, i)| (s, lat, i))
    }
}

/// `v` on the ground plane.
fn flat(v: Vec3) -> Vec3 {
    Vec3::new(v.x, 0.0, v.z)
}

/// The horizontal right of a unit direction (the engine's forward is −Z and right +X).
fn right_of(dir: Vec3) -> Vec3 {
    Vec3::new(-dir.z, 0.0, dir.x).normalize_or_zero()
}

/// Curvature of the circle through three points on the ground plane, 1/m (0 when they are in a
/// line or two coincide).
pub fn menger_curvature(a: Vec3, b: Vec3, c: Vec3) -> f32 {
    let (ab, bc, ca) = (flat(b - a), flat(c - b), flat(a - c));
    let cross = ab.x * (-ca).z - ab.z * (-ca).x;
    let denom = ab.length() * bc.length() * ca.length();
    if denom <= 1.0e-9 {
        0.0
    } else {
        2.0 * cross.abs() / denom
    }
}

/// The curvature-limited speed plan over uniformly spaced `points`: returns `(curve, speed)`.
/// `limits` caps each point (the lane's speed limit and the driver's cap); the curve speed uses
/// `params.max_lateral_accel`, the backward pass `params.max_decel` down to `end_speed` at the last
/// point, and the forward pass `params.max_accel` from `start_speed` at index `start` (never under
/// a small start-off speed, so a standing car sets off).
pub fn plan_speeds(
    points: &[Vec3],
    limits: &[f32],
    spacing: f32,
    params: &VehicleDriverParams,
    end_speed: f32,
    start: usize,
    start_speed: f32,
) -> (Vec<f32>, Vec<f32>) {
    let (max_lateral_accel, max_accel, max_decel) =
        (params.max_lateral_accel, params.max_accel, params.max_decel);
    let n = points.len();
    let mut curve = Vec::with_capacity(n);
    for i in 0..n {
        let a = points[i.saturating_sub(CURVATURE_SPAN)];
        let c = points[(i + CURVATURE_SPAN).min(n - 1)];
        let k = menger_curvature(a, points[i], c);
        let limit = limits.get(i).copied().unwrap_or(f32::INFINITY);
        let v = if k > MIN_CURVATURE {
            (max_lateral_accel / k).sqrt()
        } else {
            f32::INFINITY
        };
        curve.push(limit.min(v));
    }
    let mut speed = curve.clone();
    if let Some(last) = speed.last_mut() {
        *last = last.min(end_speed.max(0.0));
    }
    for i in (0..n.saturating_sub(1)).rev() {
        let next = speed[i + 1];
        speed[i] = speed[i].min((next * next + 2.0 * max_decel * spacing).sqrt());
    }
    if start < n {
        speed[start] = speed[start].min(start_speed.max(START_SPEED));
        for i in start + 1..n {
            let prev = speed[i - 1];
            speed[i] = speed[i].min((prev * prev + 2.0 * max_accel * spacing).sqrt());
        }
    }
    (curve, speed)
}

/// Pure-pursuit steering. `rear` is the rear axle, `forward`/`right` the chassis axes, `goal` the
/// look-ahead point. Returns the commanded path curvature (1/m, positive = right) and whether the
/// goal is behind the car.
pub fn pure_pursuit(rear: Vec3, forward: Vec3, right: Vec3, goal: Vec3) -> (f32, bool) {
    let d = flat(goal - rear);
    let x = d.dot(flat(forward));
    let y = d.dot(flat(right));
    let l2 = x * x + y * y;
    if l2 <= 1.0e-6 {
        return (0.0, false);
    }
    (2.0 * y / l2, x <= 0.0)
}

/// The interaction term of the intelligent driver model, `−a_max (s*/s)²` (≤ 0): what the driver
/// adds to its free-road acceleration for a leader `gap` metres ahead closing at `closing` m/s.
pub fn idm_interaction(params: &VehicleDriverParams, v: f32, gap: f32, closing: f32) -> f32 {
    let a = params.max_accel;
    let b = params.comfort_decel;
    let v = v.max(0.0);
    let dynamic = v * params.time_headway + v * closing / (2.0 * (a * b).sqrt());
    let s_star = params.min_gap + dynamic.max(0.0);
    let s = gap.max(0.1);
    -a * (s_star / s) * (s_star / s)
}

/// Intelligent-driver-model acceleration at speed `v` (m/s) with desired speed `v0`, a gap `gap`
/// (metres, bumper to bumper) and a closing speed `closing = v − v_leader`.
pub fn idm_accel(params: &VehicleDriverParams, v: f32, v0: f32, gap: f32, closing: f32) -> f32 {
    // Deterministic kernel (ADR-5128): the driver feeds the vehicle input every fixed step.
    let free = 1.0 - det_powf(v.max(0.0) / v0.max(0.1), params.idm_exponent);
    params.max_accel * free + idm_interaction(params, v, gap, closing)
}

/// Pure-pursuit steering input for `curvature` (1/m) on a car with `wheelbase` (m) and full lock
/// `max_steer_angle` (radians): the bicycle-model wheel angle `atan(L κ)` over full lock, unclamped.
fn pursuit_steer(wheelbase: f32, curvature: f32, max_steer_angle: f32) -> f32 {
    det_atan(wheelbase * curvature) / max_steer_angle.max(1.0e-3)
}

// ---------------------------------------------------------------------------
// The component
// ---------------------------------------------------------------------------

/// Controller memory between steps.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Memory {
    /// The pedal controller's integral, pedal units.
    integral: f32,
    /// Consecutive steps without progress.
    stuck_count: u32,
    /// Steps left in the current recovery.
    reverse_left: u32,
    /// Steer while reversing.
    reverse_steer: f32,
    /// Station where the last recovery ended (progress is measured from it).
    progress_mark: f32,
    /// Whether a recovery is waiting for its progress check.
    recovery_open: bool,
}

/// An AI driver on a vehicle entity (see the module docs). The system plans on the first step after
/// [`Self::drive_to`] and writes the entity's [`VehicleInput`] every fixed step after that.
#[derive(Component, Debug, Clone)]
pub struct VehicleDriver {
    params: VehicleDriverParams,
    target: Option<DriveTarget>,
    pending: bool,
    plan: Plan,
    status: VehicleDriverStatus,
    memory: Memory,
}

impl Default for VehicleDriver {
    fn default() -> Self {
        Self::new(VehicleDriverParams::default())
    }
}

impl VehicleDriver {
    /// A driver with `params` and no target (it holds the brake). Out-of-range params are replaced
    /// by the defaults; use [`Self::set_params`] to see the error.
    pub fn new(params: VehicleDriverParams) -> Self {
        let params = if params.validate().is_ok() {
            params
        } else {
            VehicleDriverParams::default()
        };
        Self {
            params,
            target: None,
            pending: false,
            plan: Plan::default(),
            status: VehicleDriverStatus::default(),
            memory: Memory::default(),
        }
    }

    /// A driver with the default params, headed for `target`.
    pub fn to(target: DriveTarget) -> Self {
        let mut driver = Self::default();
        driver.drive_to(target);
        driver
    }

    /// Sets a new target; the route is planned on the next step (or now, with [`Self::plan`]).
    pub fn drive_to(&mut self, target: DriveTarget) {
        self.target = Some(target);
        self.pending = true;
        self.status.last_error = None;
    }

    /// Drops the target: the driver brakes to a stop and holds it ([`VehicleDriverState::Idle`]).
    pub fn stop(&mut self) {
        self.target = None;
        self.pending = false;
        self.plan = Plan::default();
        self.memory = Memory::default();
        self.status.state = VehicleDriverState::Idle;
        self.status.leader = None;
        self.status.gap = None;
        self.status.remaining = None;
    }

    /// The tuning.
    pub fn params(&self) -> &VehicleDriverParams {
        &self.params
    }

    /// Replaces the tuning; on error nothing changes. A new lateral offset, speed or acceleration
    /// limit replans the current target on the next step.
    pub fn set_params(&mut self, params: VehicleDriverParams) -> Result<(), VehicleAiError> {
        params.validate()?;
        self.params = params;
        if self.target.is_some() {
            self.pending = true;
        }
        Ok(())
    }

    /// The target, if any.
    pub fn target(&self) -> Option<DriveTarget> {
        self.target
    }

    /// The driver's state.
    pub fn state(&self) -> VehicleDriverState {
        self.status.state
    }

    /// What the driver saw and did at its last step.
    pub fn status(&self) -> &VehicleDriverStatus {
        &self.status
    }

    /// The planned route's lanes (two laps of the cycle for a loop).
    pub fn route(&self) -> &[u32] {
        &self.plan.route
    }

    /// The planned path's points (uniform spacing; two laps for a loop).
    pub fn path(&self) -> &[Vec3] {
        &self.plan.points
    }

    /// The plan speed at each path point, m/s.
    pub fn planned_speeds(&self) -> &[f32] {
        &self.plan.speed
    }

    /// The curve speed at each path point, m/s.
    pub fn curve_speeds(&self) -> &[f32] {
        &self.plan.curve
    }

    /// The path length, metres (one lap for a loop).
    pub fn path_length(&self) -> f32 {
        if self.plan.looped() {
            self.plan.lap_points as f32 * self.plan.spacing
        } else {
            self.plan.end_station()
        }
    }

    /// Whether a target waits to be planned.
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// Plans the current target from a car at `position` moving at `speed` (m/s forward). The system
    /// calls this on the step after [`Self::drive_to`]; call it directly to see a planning error at
    /// once. On error the driver keeps no plan, holds the brake and records the error in its status.
    pub fn plan(&mut self, graph: &NavLaneGraph, position: Vec3, speed: f32) -> Result<(), VehicleAiError> {
        self.pending = false;
        let Some(target) = self.target else {
            return Ok(());
        };
        let _span = tracing::debug_span!("vehicle_ai_plan").entered();
        match build_plan(graph, &self.params, target, position, speed) {
            Ok(plan) => {
                self.plan = plan;
                self.memory = Memory::default();
                self.status = VehicleDriverStatus {
                    state: VehicleDriverState::Driving,
                    ..VehicleDriverStatus::default()
                };
                Ok(())
            }
            Err(error) => {
                self.plan = Plan::default();
                self.status.state = VehicleDriverState::Idle;
                self.status.last_error = Some(error.clone());
                Err(error)
            }
        }
    }
}

/// The lane under `position` for routing.
fn snap(
    graph: &NavLaneGraph,
    point: Vec3,
    reach: f32,
    flags: u16,
    what: &'static str,
) -> Result<(u32, f32), VehicleAiError> {
    graph
        .nearest(point, reach, flags)
        .map(|(lane, along, _)| (lane, along))
        .ok_or(VehicleAiError::OffLanes(what))
}

/// A lane-graph routing error as the driver reports it.
fn route_error(error: regia_nav::NavLaneError, from: u32, to: u32) -> VehicleAiError {
    match error {
        regia_nav::NavLaneError::UnknownLane(lane) => VehicleAiError::UnknownLane(lane),
        _ => VehicleAiError::NoRoute { from, to },
    }
}

/// The shortest cycle through `lane`: `lane` then the lanes back to it (not repeated).
fn cycle_through(graph: &NavLaneGraph, lane: u32, flags: u16) -> Result<Vec<u32>, VehicleAiError> {
    let first = graph.lane(lane).ok_or(VehicleAiError::UnknownLane(lane))?;
    let mut best: Option<NavLaneRoute> = None;
    for &next in &first.next {
        if let Ok(route) = graph.find_route(next, lane, flags)
            && best.as_ref().is_none_or(|b| route.length < b.length)
        {
            best = Some(route);
        }
    }
    let route = best.ok_or(VehicleAiError::NoRoute { from: lane, to: lane })?;
    let mut lanes = Vec::with_capacity(route.lanes.len());
    lanes.push(lane);
    lanes.extend(route.lanes.iter().take(route.lanes.len().saturating_sub(1)));
    Ok(lanes)
}

/// The lanes from the car's lane to `distance` along `to`, and that distance.
fn lane_route(
    graph: &NavLaneGraph,
    params: &VehicleDriverParams,
    position: Vec3,
    to: u32,
    distance: f32,
) -> Result<(Vec<u32>, Option<f32>), VehicleAiError> {
    let (from, along) = snap(graph, position, params.snap_reach, params.flags, "vehicle")?;
    let lanes = if from == to && distance >= along {
        vec![from]
    } else if from == to {
        // Behind the car on its own lane: once around.
        let mut lanes = cycle_through(graph, from, params.flags)?;
        lanes.push(from);
        lanes
    } else {
        graph
            .find_route(from, to, params.flags)
            .map_err(|e| route_error(e, from, to))?
            .lanes
    };
    Ok((lanes, Some(distance)))
}

/// Plans `target` for a car at `position` moving at `speed`.
fn build_plan(
    graph: &NavLaneGraph,
    params: &VehicleDriverParams,
    target: DriveTarget,
    position: Vec3,
    speed: f32,
) -> Result<Plan, VehicleAiError> {
    // The route's lanes and the end distance on the last lane (None for a loop).
    let (route, end_along) = match target {
        DriveTarget::Loop { lane } => {
            let cycle = cycle_through(graph, lane, params.flags)?;
            let mut twice = cycle.clone();
            twice.extend_from_slice(&cycle);
            (twice, None)
        }
        DriveTarget::Lane { lane, distance } => {
            let goal = graph.lane(lane).ok_or(VehicleAiError::UnknownLane(lane))?;
            if !distance.is_finite() {
                return Err(VehicleAiError::InvalidParam("distance"));
            }
            lane_route(graph, params, position, lane, distance.clamp(0.0, goal.length()))?
        }
        DriveTarget::Point { point, reach } => {
            if !(point.is_finite() && reach.is_finite() && reach > 0.0) {
                return Err(VehicleAiError::InvalidParam("point"));
            }
            let (lane, distance) = snap(graph, point, reach, params.flags, "target point")?;
            lane_route(graph, params, position, lane, distance)?
        }
    };
    // Raw points lane by lane, each tagged with its route index; `lane_start` is each lane's first
    // station along the raw polyline (the resampled path keeps arc length, so stations carry over).
    let mut raw: Vec<(Vec3, u32)> = Vec::new();
    let mut lane_start: Vec<f32> = Vec::with_capacity(route.len());
    let mut total = 0.0f32;
    let mut last_lane = (0.0f32, 0.0f32);
    for (k, &lane) in route.iter().enumerate() {
        let one = NavLaneRoute {
            lanes: vec![lane],
            length: 0.0,
        };
        let start = total;
        lane_start.push(start);
        for p in graph.route_points(&one, params.lateral_offset) {
            if let Some((last, _)) = raw.last() {
                let step = (p - *last).length();
                if step <= 1.0e-3 {
                    continue;
                }
                total += step;
            }
            raw.push((p, k as u32));
        }
        last_lane = (start, graph.lane(lane).map_or(0.0, |l| l.length()));
    }
    if raw.len() < 2 {
        return Err(VehicleAiError::NoRoute {
            from: route.first().copied().unwrap_or(0),
            to: route.last().copied().unwrap_or(0),
        });
    }
    // Segments and spacing: a whole number of segments per lap for a loop (so point i and i + lap
    // coincide), a whole number to the end station otherwise.
    let (segments, spacing, lap_points) = match end_along {
        None => {
            let lap = total * 0.5;
            let n = (lap / PATH_SPACING).round().max(4.0) as usize;
            (2 * n, lap / n as f32, n)
        }
        Some(distance) => {
            // The target distance on the last lane, scaled to the offset path's length there.
            let (start, centre) = last_lane;
            let scale = if centre > 0.0 {
                (total - start) / centre
            } else {
                1.0
            };
            let end = (start + distance * scale).min(total).max(0.1);
            let n = (end / PATH_SPACING).ceil().max(1.0) as usize;
            (n, end / n as f32, 0)
        }
    };
    if segments + 1 > MAX_PATH_POINTS {
        return Err(VehicleAiError::RouteTooLong);
    }
    let mut points = Vec::with_capacity(segments + 1);
    let mut tags = Vec::with_capacity(segments + 1);
    let mut seg = 0usize;
    let mut seg_start = 0.0f32;
    for i in 0..=segments {
        let s = i as f32 * spacing;
        while seg + 2 < raw.len() {
            let len = (raw[seg + 1].0 - raw[seg].0).length();
            if seg_start + len >= s {
                break;
            }
            seg_start += len;
            seg += 1;
        }
        let (a, b) = (raw[seg].0, raw[seg + 1].0);
        let len = (b - a).length();
        let t = if len > 0.0 {
            ((s - seg_start) / len).clamp(0.0, 1.0)
        } else {
            0.0
        };
        points.push(a + (b - a) * t);
        tags.push(raw[seg].1);
    }
    let mut limits = Vec::with_capacity(points.len());
    let mut half_width = Vec::with_capacity(points.len());
    for &tag in &tags {
        let lane = route.get(tag as usize).and_then(|&l| graph.lane(l));
        limits.push(lane.map_or(params.max_speed, |l| l.speed).min(params.max_speed));
        half_width.push(lane.map_or(1.0, |l| l.width * 0.5));
    }
    let mut plan = Plan {
        route,
        lane_start,
        points,
        tags,
        curve: Vec::new(),
        speed: Vec::new(),
        half_width,
        spacing,
        lap_points,
        cursor: 0,
    };
    // The car's place on the path: within the first lap of a loop, on the first lane of a route.
    let looped = plan.looped();
    let search_to = if looped {
        lap_points
    } else {
        plan.tags.iter().position(|&t| t > 0).unwrap_or(plan.points.len())
    };
    let start = plan
        .project(position, 0, search_to.max(1))
        .map_or(0, |(_, _, i)| i);
    plan.cursor = start;
    let (curve, mut speeds) = if looped {
        plan_speeds(
            &plan.points,
            &limits,
            spacing,
            params,
            f32::INFINITY,
            0,
            f32::INFINITY,
        )
    } else {
        plan_speeds(&plan.points, &limits, spacing, params, 0.0, start, speed)
    };
    if looped {
        // Steady state: the first lap's plan is the second's, which saw the first lap's corners
        // behind it and the second lap's ahead.
        for i in 0..lap_points.min(speeds.len().saturating_sub(lap_points)) {
            speeds[i] = speeds[i + lap_points];
        }
    }
    plan.curve = curve;
    plan.speed = speeds;
    Ok(plan)
}

// ---------------------------------------------------------------------------
// The step
// ---------------------------------------------------------------------------

/// One vehicle as the drivers see it this step.
#[derive(Debug, Clone, Copy)]
struct CarSample {
    entity: Entity,
    key: SpawnKey,
    position: Vec3,
    rotation: Quat,
    velocity: Vec3,
    half_length: f32,
    half_width: f32,
}

/// Per-step scratch reused across steps (no allocation after warm-up).
#[derive(Debug, Default)]
pub struct VehicleAiScratch {
    cars: Vec<CarSample>,
    grid: Vec<(u64, u32)>,
}

fn cell_of(p: Vec3) -> (i64, i64) {
    ((p.x / GRID_CELL).floor() as i64, (p.z / GRID_CELL).floor() as i64)
}

fn grid_key(cx: i64, cz: i64) -> u64 {
    let bias = |c: i64| (c.clamp(-(1 << 30), 1 << 30) + (1 << 31)) as u64;
    (bias(cx) << 32) | bias(cz)
}

/// Vehicle geometry the driver needs from its config.
struct Geometry {
    /// Distance from the body origin back to the rear axle, metres.
    rear: f32,
    /// Wheelbase, metres.
    wheelbase: f32,
    /// Full-throttle and full-brake accelerations, m/s².
    full_accel: f32,
    full_brake: f32,
}

fn geometry(config: &VehicleConfig) -> Geometry {
    let avg = |steer: bool| {
        let (sum, n) = config
            .wheels
            .iter()
            .filter(|w| w.steer == steer)
            .fold((0.0f32, 0u32), |(s, n), w| (s + w.position.z, n + 1));
        (n > 0).then(|| sum / n as f32)
    };
    let half = config.chassis_half_extents.z.max(0.1);
    let rear = avg(false).unwrap_or(half * 0.6);
    let front = avg(true).unwrap_or(-half * 0.6);
    let wheelbase = (rear - front).abs().max(0.5);
    let mass = config.chassis_mass.max(1.0);
    let driven = config.wheels.iter().filter(|w| w.drive).count() as f32;
    let braked = config.wheels.iter().filter(|w| w.brake).count() as f32;
    let full_accel = match config.drivetrain {
        Drivetrain::Simple => config.max_engine_force * driven / mass,
        Drivetrain::Geared(_) => GEARED_FULL_ACCEL,
    }
    .max(MIN_FULL_ACCEL);
    let full_brake = (config.max_brake_force * braked / mass).max(MIN_FULL_ACCEL);
    Geometry {
        rear,
        wheelbase,
        full_accel,
        full_brake,
    }
}

/// The best leader of `me` on its plan: `(entity, gap, closing speed)`.
fn find_leader(
    plan: &Plan,
    params: &VehicleDriverParams,
    me: &CarSample,
    station: f32,
    cars: &[CarSample],
    grid: &[(u64, u32)],
) -> Option<(Entity, f32, f32)> {
    let range = params.follow_range;
    let reach = (range / GRID_CELL).ceil() as i64;
    let (cx, cz) = cell_of(me.position);
    let forward = flat(me.rotation * Vec3::NEG_Z).normalize_or_zero();
    let from = plan.cursor;
    let to = from + (range / plan.spacing).ceil() as usize + 1;
    let (_, my_dir) = plan.sample(station);
    let my_speed = me.velocity.dot(my_dir);
    let mut best: Option<(Entity, f32, f32)> = None;
    for x in cx - reach..=cx + reach {
        let lo = grid_key(x, cz - reach);
        let hi = grid_key(x, cz + reach);
        let start = grid.partition_point(|e| e.0 < lo);
        let end = grid.partition_point(|e| e.0 <= hi);
        for &(_, index) in &grid[start..end] {
            let Some(other) = cars.get(index as usize) else {
                continue;
            };
            if other.entity == me.entity {
                continue;
            }
            let d = flat(other.position - me.position);
            if d.dot(forward) <= 0.0
                || d.length_squared() > (range + other.half_length) * (range + other.half_length)
            {
                continue;
            }
            let Some((s, lateral, seg)) = plan.project(other.position, from, to) else {
                continue;
            };
            let lane_half = plan.half_width.get(seg).copied().unwrap_or(1.0);
            if lateral.abs() > lane_half + other.half_width * 0.5 || s <= station {
                continue;
            }
            let gap = s - station - me.half_length - other.half_length;
            if best.is_none_or(|(_, g, _)| gap < g) {
                let (_, dir) = plan.sample(s);
                best = Some((other.entity, gap, my_speed - other.velocity.dot(dir)));
            }
        }
    }
    best
}

/// The pedal controller: `(throttle, brake)` for a commanded acceleration `a_cmd` and a speed
/// error `v_err`. `frozen` stops the integral (a gear shift, or a clipped acceleration command).
fn pedals(
    memory: &mut Memory,
    params: &VehicleDriverParams,
    geo: &Geometry,
    a_cmd: f32,
    (v_err, dt): (f32, f32),
    frozen: bool,
) -> (f32, f32) {
    let u = a_cmd / geo.full_accel + memory.integral;
    let saturated = (u >= 1.0 && v_err > 0.0) || (u <= -1.0 && v_err < 0.0);
    if !saturated && !frozen {
        memory.integral =
            (memory.integral + params.speed_integral * v_err * dt).clamp(-INTEGRAL_LIMIT, INTEGRAL_LIMIT);
    }
    if u >= 0.0 {
        (u.min(1.0), 0.0)
    } else {
        (0.0, (-u * geo.full_accel / geo.full_brake).min(1.0))
    }
}

/// The driven car this step.
struct Car<'a> {
    sample: &'a CarSample,
    config: &'a VehicleConfig,
    /// A geared drivetrain is between gears (the throttle does nothing).
    shifting: bool,
    /// A negative throttle reverses (simple drivetrain or an automatic gearbox).
    can_reverse: bool,
}

/// What every driver shares this step.
struct Env<'a> {
    graph: Option<&'a NavLaneGraph>,
    cars: &'a [CarSample],
    grid: &'a [(u64, u32)],
    dt: f32,
}

/// One driver's step. Returns the input to write.
fn step_driver(driver: &mut VehicleDriver, car: &Car<'_>, env: &Env<'_>) -> VehicleInput {
    let (me, config, graph) = (car.sample, car.config, env.graph);
    let forward = me.rotation * Vec3::NEG_Z;
    let right = me.rotation * Vec3::X;
    let speed = me.velocity.dot(forward);
    let hold = VehicleInput {
        throttle: 0.0,
        brake: 1.0,
        steer: 0.0,
    };
    if driver.pending {
        match graph {
            Some(graph) => {
                // The error is kept in the status; the driver holds the brake.
                let _ = driver.plan(graph, me.position, speed);
            }
            None => {
                driver.pending = false;
                driver.plan = Plan::default();
                driver.status.state = VehicleDriverState::Idle;
                driver.status.last_error = Some(VehicleAiError::NoLaneGraph);
            }
        }
    }
    driver.status.speed = speed;
    if driver.target.is_none() || driver.plan.points.len() < 2 {
        driver.status.state = VehicleDriverState::Idle;
        driver.status.input = hold;
        return hold;
    }
    driver.status.steps += 1;
    let params = driver.params;
    let geo = geometry(config);
    let plan = &mut driver.plan;
    let memory = &mut driver.memory;
    let status = &mut driver.status;

    // -- Where the car is on its path. ------------------------------------------------------------
    let last_seg = plan.points.len() - 2;
    let from = plan.cursor.saturating_sub(CURSOR_BACK);
    let to = (plan.cursor + CURSOR_AHEAD).min(last_seg);
    if let Some((_, _, seg)) = plan.project(me.position, from, to) {
        plan.cursor = seg;
    }
    if plan.looped() && plan.cursor >= plan.lap_points {
        plan.cursor -= plan.lap_points;
        memory.progress_mark -= plan.lap_points as f32 * plan.spacing;
        status.laps += 1;
    }
    let (station, cross_track, _) = plan
        .project(me.position, plan.cursor, plan.cursor + 1)
        .unwrap_or((0.0, 0.0, plan.cursor));
    status.station = station;
    status.cross_track = cross_track;
    status.curve_speed = plan.read(&plan.curve, station);

    // -- Stop line: the route end, or the first disabled lane ahead. ------------------------------
    let mut stop_line = (!plan.looped()).then(|| plan.end_station());
    let mut blocked = false;
    if let Some(graph) = graph {
        let here = plan.tags.get(plan.cursor).copied().unwrap_or(0) as usize;
        for k in here + 1..plan.route.len() {
            let Some(&start) = plan.lane_start.get(k) else {
                break;
            };
            if start > station + BLOCK_HORIZON {
                break;
            }
            if graph.lane(plan.route[k]).is_some_and(|l| !l.enabled) {
                let line = start - me.half_length - BLOCK_MARGIN;
                if stop_line.is_none_or(|s| line < s) {
                    stop_line = Some(line);
                    blocked = true;
                }
                break;
            }
        }
    }
    let remaining = stop_line.map(|s| s - station);
    status.remaining = remaining;

    // -- Steering: pure pursuit from the rear axle. -----------------------------------------------
    let rear = me.position + forward * -geo.rear;
    let look = (params.look_ahead_min + params.look_ahead_time * speed.max(0.0))
        .clamp(params.look_ahead_min, params.look_ahead_max);
    let goal_station = (station - geo.rear + look).min(plan.end_station());
    let (goal, _) = plan.sample(goal_station);
    let (curvature, behind) = pure_pursuit(rear, forward, right, goal);
    let mut steer = if behind {
        // Low-speed fallback: full lock toward a goal behind the car.
        if flat(goal - rear).dot(flat(right)) >= 0.0 {
            1.0
        } else {
            -1.0
        }
    } else {
        pursuit_steer(geo.wheelbase, curvature, config.max_steer_angle)
    };
    steer = steer.clamp(-1.0, 1.0);

    // -- Speed: the plan, the stop line, the steering, the leader. --------------------------------
    let preview = station + params.speed_preview * speed.max(0.0) + plan.spacing;
    // The preview anticipates braking; it never lifts the target over the curve speed under the car.
    let mut target = plan.read(&plan.speed, preview).min(status.curve_speed);
    let ahead = plan.read(&plan.speed, station + plan.spacing);
    let here_speed = plan.read(&plan.speed, station);
    // The plan's slope at the car's own speed (`a = v dv/ds`): a car standing still on a braking
    // stretch of the plan gets no deceleration feedforward, so it never holds short of a stop line.
    let mut feedforward = speed.max(0.0) * (ahead - here_speed) / plan.spacing;
    if let Some(rem) = remaining {
        let stop = (2.0 * params.max_decel * rem.max(0.0)).sqrt();
        // Inside the stop line's braking zone the profile is read at the car, not at the preview
        // (the preview reaches the line first and would stop the car short of it).
        if stop < target || stop <= here_speed + 0.1 {
            target = stop;
            feedforward = -params
                .max_decel
                .min(speed.max(0.0) * speed.max(0.0) / (2.0 * rem.max(0.05)));
        }
        if rem < 0.25 {
            target = 0.0;
        }
    }
    if behind {
        target = target.min(START_SPEED * 2.0);
    } else if curvature.abs() > MIN_CURVATURE {
        target = target.min(
            (params.max_lateral_accel / curvature.abs())
                .sqrt()
                .max(START_SPEED * 2.0),
        );
    }
    let leader = find_leader(plan, &params, me, station, env.cars, env.grid);
    // Behind a leader: never faster than lets the car stop `min_gap` behind it if it brakes to a
    // standstill at `max_decel` (a braking-distance bound under the IDM), and hold the brake once
    // the car is queued at the minimum gap behind a stopped leader.
    let mut queued = false;
    if let Some((_, gap, closing)) = leader {
        let v_lead = (speed - closing).max(0.0);
        let room = (gap - params.min_gap - QUEUE_MARGIN).max(0.0);
        target = target.min((v_lead * v_lead + 2.0 * params.max_decel * room).sqrt());
        queued = v_lead < params.stuck_speed && room < params.arrive_distance;
    }
    let a_raw = feedforward + params.speed_gain * (target - speed);
    let a_free = a_raw.clamp(-params.emergency_decel, params.max_accel);
    // The command limit is a saturation too: no integration while it clips (anti-windup).
    let clipped = a_raw != a_free;
    let mut a_cmd = a_free;
    let mut following = queued;
    match leader {
        Some((leader, gap, closing)) => {
            // The IDM's interaction term on top of the free-road command (ADR-3117).
            let a_int = idm_interaction(&params, speed, gap, closing);
            a_cmd = (a_free + a_int).max(-params.emergency_decel);
            following |= a_int < -0.1;
            status.leader = Some(leader);
            status.gap = Some(gap);
        }
        None => {
            status.leader = None;
            status.gap = None;
        }
    }
    status.target_speed = target;

    // -- States. ----------------------------------------------------------------------------------
    let crawl = speed.abs() < 0.3;
    if matches!(status.state, VehicleDriverState::Stuck) {
        status.input = hold;
        return hold;
    }
    if memory.recovery_open && station > memory.progress_mark + params.recovery_progress {
        memory.recovery_open = false;
        status.failed_recoveries = 0;
    }
    if memory.reverse_left > 0 {
        memory.reverse_left -= 1;
        status.state = VehicleDriverState::Recovering;
        memory.integral = 0.0;
        let input = if speed > 0.5 {
            VehicleInput {
                throttle: 0.0,
                brake: 1.0,
                steer: memory.reverse_steer,
            }
        } else {
            VehicleInput {
                throttle: -params.reverse_throttle,
                brake: 0.0,
                steer: memory.reverse_steer,
            }
        };
        if memory.reverse_left == 0 {
            memory.stuck_count = 0;
            memory.progress_mark = station;
            memory.recovery_open = true;
        }
        status.input = input;
        return input;
    }
    if let Some(rem) = remaining
        && rem < params.arrive_distance
        && crawl
    {
        status.state = if blocked {
            VehicleDriverState::Blocked
        } else {
            VehicleDriverState::Arrived
        };
        memory.integral = 0.0;
        memory.stuck_count = 0;
        status.input = hold;
        return hold;
    }
    if queued && crawl {
        // Waiting in a queue is not being stuck.
        status.state = VehicleDriverState::Following;
        memory.integral = 0.0;
        memory.stuck_count = 0;
        status.input = hold;
        return hold;
    }
    status.state = if blocked && remaining.is_some_and(|r| r < 15.0) {
        VehicleDriverState::Blocked
    } else if following {
        VehicleDriverState::Following
    } else {
        VehicleDriverState::Driving
    };

    // -- Pedals. ----------------------------------------------------------------------------------
    let v_err = target - speed;
    let (throttle, brake) = pedals(
        memory,
        &params,
        &geo,
        a_cmd,
        (v_err, env.dt),
        car.shifting || clipped,
    );
    let input = VehicleInput {
        throttle,
        brake,
        steer,
    };

    // -- Stuck detection (counted in fixed steps). ------------------------------------------------
    if throttle > 0.2 && a_cmd > 0.3 && speed < params.stuck_speed && target > 1.0 {
        memory.stuck_count += 1;
    } else {
        memory.stuck_count = 0;
    }
    if memory.stuck_count >= params.stuck_steps {
        memory.stuck_count = 0;
        // Stuck again before the last recovery made progress: that recovery failed.
        if memory.recovery_open {
            status.failed_recoveries += 1;
        }
        memory.recovery_open = false;
        if !car.can_reverse || status.failed_recoveries >= params.max_recoveries {
            status.state = VehicleDriverState::Stuck;
            status.input = hold;
            return hold;
        }
        status.recoveries += 1;
        memory.reverse_left = params.reverse_steps;
        memory.reverse_steer = if steer.abs() < 0.05 { 1.0 } else { -steer.signum() };
        memory.integral = 0.0;
        status.state = VehicleDriverState::Recovering;
        let input = VehicleInput {
            throttle: 0.0,
            brake: 1.0,
            steer: memory.reverse_steer,
        };
        status.input = input;
        return input;
    }
    status.input = input;
    input
}

/// Drives every [`VehicleDriver`] one fixed step: samples every vehicle, fills the neighbour grid,
/// and writes each driver's [`VehicleInput`]. Vehicles are visited in spawn order (ADR-2396), so a
/// run replays exactly. A vehicle of a parked data layer is not driven.
pub fn drive_vehicle_ai(
    fixed: Option<Res<FixedTime>>,
    physics: Option<Res<PhysicsWorld>>,
    graph: Option<Res<NavLaneGraph>>,
    order: Option<Res<SpawnOrder<VehicleController>>>,
    mut scratch: Local<VehicleAiScratch>,
    vehicles: Query<(Entity, &VehicleController)>,
    mut drivers: Query<(&VehicleController, &mut VehicleDriver, &mut VehicleInput), Without<LayerParked>>,
) {
    let (Some(fixed), Some(physics)) = (fixed, physics) else {
        return;
    };
    if drivers.is_empty() {
        return;
    }
    let _span = tracing::debug_span!("drive_vehicle_ai").entered();
    let dt = fixed.step().as_secs_f32();
    let VehicleAiScratch { cars, grid } = &mut *scratch;
    cars.clear();
    for (entity, controller) in &vehicles {
        let Some(body) = physics.bodies.get(controller.vehicle.chassis().to_handle()) else {
            continue;
        };
        let (position, rotation) = from_rapier_pose(body.position());
        let half = controller.vehicle.config().chassis_half_extents;
        cars.push(CarSample {
            entity,
            key: order
                .as_deref()
                .map_or((u64::MAX, entity.index().index(), entity.to_bits()), |o| {
                    o.key(entity)
                }),
            position,
            rotation,
            velocity: from_rapier_vec3(body.linvel()),
            half_length: half.z,
            half_width: half.x,
        });
    }
    cars.sort_unstable_by_key(|car| car.key);
    grid.clear();
    for (i, car) in cars.iter().enumerate() {
        let (cx, cz) = cell_of(car.position);
        grid.push((grid_key(cx, cz), i as u32));
    }
    grid.sort_unstable();
    let env = Env {
        graph: graph.as_deref(),
        cars,
        grid,
        dt,
    };
    for sample in cars.iter() {
        let Ok((controller, mut driver, mut input)) = drivers.get_mut(sample.entity) else {
            continue;
        };
        let drivetrain = controller.vehicle.drivetrain();
        let car = Car {
            sample,
            config: controller.vehicle.config(),
            shifting: drivetrain.is_some_and(|d| d.is_shifting()),
            can_reverse: drivetrain.is_none_or(|d| d.is_automatic()),
        };
        let next = step_driver(&mut driver, &car, &env);
        if *input != next {
            *input = next;
        }
    }
    // A driver whose chassis left the physics world holds still and says why.
    for (controller, mut driver, _) in &mut drivers {
        if physics
            .bodies
            .get(controller.vehicle.chassis().to_handle())
            .is_none()
            && driver.status.last_error != Some(VehicleAiError::NoChassis)
        {
            driver.status.last_error = Some(VehicleAiError::NoChassis);
        }
    }
}

/// Runs [`drive_vehicle_ai`] in `FixedUpdate`, in the physics chain after the velocity sync and
/// before `drive_vehicles`. Add it after `PhysicsPlugin` and `VehiclePlugin`.
pub struct VehicleAiPlugin;

impl Plugin for VehicleAiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            drive_vehicle_ai
                .after(sync_velocity_and_forces)
                .before(drive_vehicles)
                .in_set(PhysicsStep),
        );
        regia_ecs::spawn_order::track_spawn_order::<VehicleController>(app.world_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_nav::NavLaneDef;

    fn line(points: &[(f32, f32)]) -> NavLaneDef {
        let p: Vec<Vec3> = points.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
        let mut def = NavLaneDef::new(&p);
        def.speed = 20.0;
        def.width = 3.5;
        def
    }

    #[test]
    fn menger_curvature_of_a_circle_is_one_over_its_radius() {
        let r = 25.0f32;
        let p = |a: f32| Vec3::new(r * a.cos(), 0.0, r * a.sin());
        let k = menger_curvature(p(0.0), p(0.1), p(0.2));
        assert!((k - 1.0 / r).abs() < 1.0e-4, "{k}");
        assert_eq!(menger_curvature(Vec3::ZERO, Vec3::X, Vec3::X * 2.0), 0.0);
    }

    #[test]
    fn the_speed_plan_respects_curves_braking_and_acceleration() {
        // 100 m straight, then a 20 m radius quarter circle, then 100 m straight.
        let mut points = Vec::new();
        for i in 0..100 {
            points.push(Vec3::new(0.0, 0.0, -(i as f32)));
        }
        let r = 20.0f32;
        let arc = (r * std::f32::consts::FRAC_PI_2).round() as usize;
        for i in 0..arc {
            let a = i as f32 / r;
            points.push(Vec3::new(r - r * a.cos(), 0.0, -100.0 - r * a.sin()));
        }
        for i in 0..100 {
            points.push(Vec3::new(r + i as f32, 0.0, -120.0));
        }
        let limits = vec![30.0; points.len()];
        let params = VehicleDriverParams {
            max_lateral_accel: 4.0,
            max_accel: 2.0,
            max_decel: 4.0,
            ..Default::default()
        };
        let (curve, speed) = plan_speeds(&points, &limits, 1.0, &params, 0.0, 0, 0.0);
        let mid = 100 + arc / 2;
        assert!(
            (curve[mid] - (4.0f32 * 20.0).sqrt()).abs() < 0.3,
            "curve speed {}",
            curve[mid]
        );
        for i in 0..points.len() {
            assert!(speed[i] <= curve[i] + 1.0e-4);
            if i + 1 < points.len() {
                // Braking and acceleration limits between neighbours.
                let (a, b) = (speed[i], speed[i + 1]);
                assert!(a * a - b * b <= 2.0 * 4.0 + 1.0e-3, "braking at {i}");
                assert!(b * b - a * a <= 2.0 * 2.0 + 1.0e-3, "acceleration at {i}");
            }
        }
        assert_eq!(*speed.last().unwrap_or(&1.0), 0.0, "stops at the end");
        assert!(speed[0] <= START_SPEED + 1.0e-4);
    }

    #[test]
    fn pure_pursuit_turns_toward_the_goal() {
        let rear = Vec3::ZERO;
        let (fwd, right) = (Vec3::NEG_Z, Vec3::X);
        let (k, behind) = pure_pursuit(rear, fwd, right, Vec3::new(1.0, 0.0, -5.0));
        assert!(k > 0.0 && !behind, "right of the car steers right");
        // Coulter: curvature 2x / l² with x the lateral offset.
        assert!((k - 2.0 / 26.0).abs() < 1.0e-6);
        let (k, behind) = pure_pursuit(rear, fwd, right, Vec3::new(-2.0, 0.0, 3.0));
        assert!(k < 0.0 && behind);
    }

    #[test]
    fn idm_brakes_for_a_close_slow_leader_and_cruises_alone() {
        let p = VehicleDriverParams::default();
        assert!(idm_accel(&p, 10.0, 20.0, 1000.0, 0.0) > 0.0);
        assert!(idm_accel(&p, 10.0, 20.0, 5.0, 10.0) < -p.comfort_decel);
        // At the desired speed with a far leader the acceleration is about 0.
        assert!(idm_accel(&p, 20.0, 20.0, 1.0e6, 0.0).abs() < 1.0e-3);
    }

    #[test]
    fn params_validate_names_the_first_bad_field() {
        assert!(VehicleDriverParams::default().validate().is_ok());
        let p = VehicleDriverParams {
            max_speed: -1.0,
            ..Default::default()
        };
        assert_eq!(p.validate(), Err(VehicleAiError::InvalidParam("max_speed")));
        let p = VehicleDriverParams {
            emergency_decel: 1.0,
            ..Default::default()
        };
        assert_eq!(p.validate(), Err(VehicleAiError::InvalidParam("emergency_decel")));
    }

    #[test]
    fn plans_a_route_a_loop_and_reports_errors() {
        // A square loop of four 50 m lanes.
        let lanes = vec![
            {
                let mut l = line(&[(0.0, 0.0), (50.0, 0.0)]);
                l.next = vec![1];
                l
            },
            {
                let mut l = line(&[(50.0, 0.0), (50.0, 50.0)]);
                l.next = vec![2];
                l
            },
            {
                let mut l = line(&[(50.0, 50.0), (0.0, 50.0)]);
                l.next = vec![3];
                l
            },
            {
                let mut l = line(&[(0.0, 50.0), (0.0, 0.0)]);
                l.next = vec![0];
                l
            },
        ];
        let graph = NavLaneGraph::build(&lanes, 0.0).expect("graph");
        let mut driver = VehicleDriver::to(DriveTarget::Lane {
            lane: 2,
            distance: 25.0,
        });
        driver.plan(&graph, Vec3::new(5.0, 0.0, 0.5), 0.0).expect("plan");
        assert_eq!(driver.route(), &[0, 1, 2]);
        assert!(
            (driver.path_length() - 125.0).abs() < 0.6,
            "{}",
            driver.path_length()
        );
        assert_eq!(*driver.planned_speeds().last().unwrap_or(&1.0), 0.0);
        // Behind the car on its own lane: once around.
        driver.drive_to(DriveTarget::Lane {
            lane: 0,
            distance: 2.0,
        });
        driver.plan(&graph, Vec3::new(5.0, 0.0, 0.0), 0.0).expect("lap");
        assert_eq!(driver.route(), &[0, 1, 2, 3, 0]);
        // A loop holds two laps and never plans a stop.
        driver.drive_to(DriveTarget::Loop { lane: 1 });
        driver
            .plan(&graph, Vec3::new(50.0, 0.0, 10.0), 0.0)
            .expect("loop");
        assert_eq!(driver.route(), &[1, 2, 3, 0, 1, 2, 3, 0]);
        assert!((driver.path_length() - 200.0).abs() < 1.0e-2);
        assert!(driver.planned_speeds().iter().all(|&v| v > 0.0));
        // Errors.
        driver.drive_to(DriveTarget::Lane {
            lane: 9,
            distance: 0.0,
        });
        assert_eq!(
            driver.plan(&graph, Vec3::ZERO, 0.0),
            Err(VehicleAiError::UnknownLane(9))
        );
        assert_eq!(driver.state(), VehicleDriverState::Idle);
        driver.drive_to(DriveTarget::Lane {
            lane: 1,
            distance: 0.0,
        });
        assert_eq!(
            driver
                .plan(&graph, Vec3::new(500.0, 0.0, 0.0), 0.0)
                .map_err(|e| e.code()),
            Err("vehicle_ai_off_lanes")
        );
    }

    /// A square loop of four 50 m lanes, 0 → 1 → 2 → 3 → 0, lane 0 along +X from the origin.
    fn square() -> NavLaneGraph {
        let corners = [(0.0, 0.0), (50.0, 0.0), (50.0, 50.0), (0.0, 50.0)];
        let lanes: Vec<NavLaneDef> = (0..4)
            .map(|i| {
                let mut l = line(&[corners[i], corners[(i + 1) % 4]]);
                l.next = vec![((i + 1) % 4) as u32];
                l
            })
            .collect();
        NavLaneGraph::build(&lanes, 0.0).expect("graph")
    }

    /// Spec § 4: a path holds at most `MAX_PATH_POINTS` points; a longer route is refused with
    /// `vehicle_ai_route_too_long` and the driver keeps no plan.
    #[test]
    fn a_route_past_the_path_limit_is_refused() {
        let graph = NavLaneGraph::build(&[line(&[(0.0, 0.0), (25_000.0, 0.0)])], 0.0).expect("road");
        // 19 km at the 1 m spacing is 19,001 points: inside the limit.
        let mut driver = VehicleDriver::to(DriveTarget::Lane {
            lane: 0,
            distance: 19_000.0,
        });
        driver
            .plan(&graph, Vec3::new(0.5, 0.0, 0.0), 0.0)
            .expect("19 km plans");
        assert!(driver.path().len() <= MAX_PATH_POINTS);
        assert!((driver.path_length() - 19_000.0).abs() < 1.0);
        // 24 km is 24,001 points: refused.
        driver.drive_to(DriveTarget::Lane {
            lane: 0,
            distance: 24_000.0,
        });
        let error = driver.plan(&graph, Vec3::new(0.5, 0.0, 0.0), 0.0);
        assert_eq!(error, Err(VehicleAiError::RouteTooLong));
        assert_eq!(VehicleAiError::RouteTooLong.code(), "vehicle_ai_route_too_long");
        assert_eq!(driver.state(), VehicleDriverState::Idle);
        assert_eq!(driver.status().last_error, Some(VehicleAiError::RouteTooLong));
        assert!(driver.path().is_empty(), "no plan is kept");
    }

    /// Spec § 2 / § 4: `set_params` validates the whole set (nothing changes on error) and replans
    /// the current target; the new lateral offset moves the path.
    #[test]
    fn set_params_validates_and_replans_the_target() {
        let graph = square();
        let mut driver = VehicleDriver::to(DriveTarget::Lane {
            lane: 0,
            distance: 40.0,
        });
        driver.plan(&graph, Vec3::new(2.0, 0.0, 0.0), 0.0).expect("plan");
        assert!(!driver.is_pending());
        let centre = driver.path()[10];
        assert!(centre.z.abs() < 1.0e-4, "on the lane centre: {centre:?}");

        let bad = VehicleDriverParams {
            look_ahead_max: 1.0,
            ..Default::default()
        };
        assert_eq!(
            driver.set_params(bad),
            Err(VehicleAiError::InvalidParam("look_ahead_max"))
        );
        assert_eq!(
            *driver.params(),
            VehicleDriverParams::default(),
            "nothing changed"
        );
        assert!(!driver.is_pending(), "a refused set does not replan");

        let offset = VehicleDriverParams {
            lateral_offset: 1.0,
            ..Default::default()
        };
        driver.set_params(offset).expect("valid");
        assert!(driver.is_pending(), "a driver with a target replans");
        driver
            .plan(&graph, Vec3::new(2.0, 0.0, 1.0), 0.0)
            .expect("replan");
        // Lane 0 runs along +X, so its right is +Z.
        let shifted = driver.path()[10];
        assert!((shifted.z - 1.0).abs() < 1.0e-4, "one metre right: {shifted:?}");

        // Without a target, set_params does not leave a pending plan.
        driver.stop();
        driver.set_params(VehicleDriverParams::default()).expect("valid");
        assert!(!driver.is_pending());
        assert_eq!(driver.state(), VehicleDriverState::Idle);
    }

    /// Spec § 2: `DriveTarget::Point` snaps the point to the nearest open lane within `reach`.
    #[test]
    fn a_point_target_snaps_to_the_nearest_lane() {
        let graph = square();
        let mut driver = VehicleDriver::to(DriveTarget::Point {
            point: Vec3::new(51.5, 0.0, 25.0),
            reach: 3.0,
        });
        driver.plan(&graph, Vec3::new(5.0, 0.0, 0.0), 0.0).expect("plan");
        assert_eq!(driver.route(), &[0, 1]);
        assert!(
            (driver.path_length() - 75.0).abs() < 0.6,
            "{}",
            driver.path_length()
        );
        let end = *driver.path().last().expect("path");
        assert!((end - Vec3::new(50.0, 0.0, 25.0)).length() < 0.6, "{end:?}");
        assert_eq!(
            *driver.planned_speeds().last().unwrap_or(&1.0),
            0.0,
            "stops there"
        );
        // A reach that does not get to any lane.
        driver.drive_to(DriveTarget::Point {
            point: Vec3::new(25.0, 0.0, 25.0),
            reach: 3.0,
        });
        assert_eq!(
            driver.plan(&graph, Vec3::new(5.0, 0.0, 0.0), 0.0),
            Err(VehicleAiError::OffLanes("target point"))
        );
    }

    /// Spec § 2: out-of-range params given to `new` fall back to the defaults; non-finite targets are
    /// refused with `vehicle_ai_bad_param`.
    #[test]
    fn bad_params_and_targets_are_refused() {
        let driver = VehicleDriver::new(VehicleDriverParams {
            max_speed: f32::NAN,
            ..Default::default()
        });
        assert_eq!(*driver.params(), VehicleDriverParams::default());
        let graph = square();
        let mut driver = VehicleDriver::to(DriveTarget::Lane {
            lane: 1,
            distance: f32::INFINITY,
        });
        let error = driver.plan(&graph, Vec3::new(5.0, 0.0, 0.0), 0.0);
        assert_eq!(error, Err(VehicleAiError::InvalidParam("distance")));
        assert_eq!(error.map_err(|e| e.code()), Err("vehicle_ai_bad_param"));
        driver.drive_to(DriveTarget::Point {
            point: Vec3::new(50.0, 0.0, 25.0),
            reach: 0.0,
        });
        assert_eq!(
            driver.plan(&graph, Vec3::new(5.0, 0.0, 0.0), 0.0),
            Err(VehicleAiError::InvalidParam("point"))
        );
        // Every state has its agent name.
        let names: Vec<&str> = [
            VehicleDriverState::Idle,
            VehicleDriverState::Driving,
            VehicleDriverState::Following,
            VehicleDriverState::Blocked,
            VehicleDriverState::Arrived,
            VehicleDriverState::Recovering,
            VehicleDriverState::Stuck,
        ]
        .iter()
        .map(|s| s.name())
        .collect();
        assert_eq!(
            names,
            [
                "idle",
                "driving",
                "following",
                "blocked",
                "arrived",
                "recovering",
                "stuck"
            ]
        );
    }

    #[test]
    fn grid_keys_keep_a_row_contiguous_across_zero() {
        let mut keys: Vec<u64> = (-3..=3).map(|z| grid_key(0, z)).collect();
        let sorted = keys.clone();
        keys.sort_unstable();
        assert_eq!(keys, sorted);
        assert!(grid_key(-1, 1_000) < grid_key(0, -1_000));
    }

    /// ADR-5128: the driver's free-road term and pure-pursuit steering feed the vehicle input every
    /// fixed step, so they use the deterministic kernels; a platform `powf`/`atan` that rounds
    /// differently (another OS, or the 1-ulp LD_PRELOAD shim on Linux) fails these bit checks.
    #[test]
    fn idm_and_pursuit_steering_use_the_deterministic_kernels() {
        let params = VehicleDriverParams::default();
        for i in 0..50 {
            let v = 0.7 * i as f32;
            for (gap, closing) in [(1.0e6_f32, 0.0_f32), (25.0, 3.0), (8.0, -1.5)] {
                let free = 1.0 - det_powf(v / 20.0, params.idm_exponent);
                let want = params.max_accel * free + idm_interaction(&params, v, gap, closing);
                assert_eq!(
                    idm_accel(&params, v, 20.0, gap, closing).to_bits(),
                    want.to_bits(),
                    "idm at v {v}, gap {gap}"
                );
            }
        }
        for i in 0..40 {
            let curvature = -0.2 + 0.4 * i as f32 / 39.0;
            let want = det_atan(2.7 * curvature) / 0.6;
            assert_eq!(
                pursuit_steer(2.7, curvature, 0.6).to_bits(),
                want.to_bits(),
                "steer at curvature {curvature}"
            );
        }
    }
}
