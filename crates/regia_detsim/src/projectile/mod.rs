// SPDX-License-Identifier: MPL-2.0
//! Deterministic projectiles for a lockstep simulation (ADR-8509).
//!
//! A projectile is a point (or a sphere of [`ProjectileParams::radius_m`]) with a position and a
//! velocity, stepped once per fixed tick by [`step`]: gravity and air drag in the order of
//! [`crate::ballistic::step_debris`], then a swept segment from the old position to the new one
//! through a [`SegmentHits`] (so a fast shot never passes through a thin wall), then bounces, the
//! fuse, the lifetime and the range. Every step is IEEE `f32` `+ - * / sqrt` in source order, with
//! sign-sensitive `min`/`max`/`clamp` through [`crate::float`]; there are no transcendentals on the
//! tick. Two peers that step the same projectiles against the same hits get the same bits.
//!
//! - [`ProjectileParams`] is the projectile's kind: gravity scale, drag, wind response, radius,
//!   lifetime, range, fuse and bounce numbers. [`ProjectileParams::DEFAULT`] is the old debris step:
//!   with it, [`step`]'s velocity equals [`crate::ballistic::step_debris`] bit for bit.
//! - [`ProjectileState`] is one projectile's state; [`step`] advances it one tick and returns an
//!   [`Impact`] on the tick it ends.
//! - [`SegmentHits`] is the hit seam: a swept-sphere query along a segment. [`LinearScanHits`]
//!   is the engine's reference implementation (planes, boxes and body spheres, tested one by one);
//!   a game plugs in its spatial index (the entity side and the world side) behind the same trait,
//!   and a pair `(world, bodies)` is itself a [`SegmentHits`].
//! - [`ProjectilePool`] keeps up to a fixed number of projectiles in slots issued by
//!   [`crate::ids::DetIdAllocator`], steps them in ascending slot order and appends their impacts
//!   in that order, with no heap allocation after construction.
//!
//! ```
//! use regia_detsim::projectile::{
//!     HitPlane, Impact, LinearScanHits, ProjectileParams, ProjectilePool, ProjectileState,
//! };
//! use regia_detsim::site_physics::WorldPhysics;
//! use regia_detsim::tick::tick_dt;
//!
//! let mut ground = LinearScanHits::default();
//! ground.planes.push(HitPlane::ground(0.0));
//! let mut pool = ProjectilePool::new(64); // the capacity is the host's setting
//! let mut impacts: Vec<Impact> = Vec::with_capacity(pool.capacity() as usize);
//! let shell = ProjectileParams { radius_m: 0.05, ..ProjectileParams::DEFAULT };
//! let slot = pool.spawn(shell, ProjectileState::launch([0.0, 1.0, 0.0], [30.0, 30.0, 0.0], None));
//! assert_eq!(slot, Some(1));
//! for _ in 0..600 {
//!     impacts.clear();
//!     pool.step_all(&WorldPhysics::EARTH, [0.0; 3], tick_dt(), &mut ground, &mut impacts);
//!     if !impacts.is_empty() {
//!         break;
//!     }
//! }
//! assert_eq!(impacts.len(), 1);
//! assert!(impacts[0].point[0] > 100.0); // about 180 m without drag
//! ```

use core::fmt;

use crate::ballistic::{DRAG_PER_M, DRAG_REF_AIR, map_g};
use crate::float::{det_clamp, det_min};
use crate::math;
use crate::site_physics::WorldPhysics;

mod hits;
mod pool;
mod scenario;

pub use hits::{HitBox, HitPlane, HitSphere, LinearScanHits, NoHits};
pub use pool::{PoolEntry, ProjectilePool};
pub use scenario::ProjectileMix;

/// A projectile kind's numbers (ADR-8509). A match setting: every peer steps the same values.
///
/// [`ProjectileParams::validate`] checks values read from user content; [`step`] never validates
/// (it keeps its own guard against a bad `dt`).
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProjectileParams {
    /// Multiplies the world's gravity (1: the world's, 0: no drop). Valid: 0 to 100.
    pub gravity_scale: f32,
    /// Quadratic drag, 1/m, at 1.2 kg/m³ of air ([`DRAG_REF_AIR`]); scaled by the world's air
    /// density over 1.2, zero in vacuum. Each tick the velocity loses
    /// `min(drag_per_m * air / 1.2 * speed * dt, 1)` of itself. Valid: 0 to 10.
    pub drag_per_m: f32,
    /// Linear drag, 1/s, at 1.2 kg/m³ of air, scaled like `drag_per_m`: the velocity loses
    /// `min(linear_drag_per_s * air / 1.2 * dt, 1)` of itself per tick, after the quadratic drag.
    /// Valid: 0 to 1000.
    pub linear_drag_per_s: f32,
    /// How much the wind moves the air the drag works against: 0 (the wind is ignored, the old
    /// step) to 1 (the drag pulls the projectile toward the wind's velocity). Valid: 0 to 1.
    pub wind_response: f32,
    /// Radius of the swept sphere, m (0: a point). Valid: 0 to 100.
    pub radius_m: f32,
    /// Lifetime, s; the projectile ends with [`ImpactReason::Expired`] on the tick it reaches it
    /// ([`timer_ticks`]). 0: no lifetime. Valid: 0 to 86400.
    pub lifetime_s: f32,
    /// Path length, m, after which the projectile ends with [`ImpactReason::Range`]. The distance
    /// travelled along its path (bounces included), not the distance from the launch point. 0: no
    /// limit. Valid: 0 to 1e7.
    pub max_range_m: f32,
    /// Fuse, s: the projectile ends with [`ImpactReason::Fuse`] on the tick it reaches it, wherever
    /// it is (in flight or resting). `None`: no fuse. Valid: above 0, at most 86400.
    pub fuse_s: Option<f32>,
    /// Share of the normal speed kept by a bounce. Valid: 0 to 1.
    pub restitution: f32,
    /// Share of the tangential speed a bounce takes away. Valid: 0 to 1.
    pub friction: f32,
    /// Bounces before the projectile stops at a hit. With no bounce left, a fused projectile rests
    /// where it hit and waits for its fuse; an unfused one ends with [`ImpactReason::Hit`].
    pub max_bounces: u16,
    /// A bounce that leaves the projectile at this speed (m/s) or slower stops it, as if it had no
    /// bounce left. Valid: 0 to 1e5.
    pub stop_speed_m_s: f32,
}

impl ProjectileParams {
    /// The old debris step: [`DRAG_PER_M`] (0.002 per m), the world's gravity, no wind, a point,
    /// no lifetime, range, fuse or bounces. With it, [`step`]'s velocity is
    /// [`crate::ballistic::step_debris`]'s bit for bit.
    pub const DEFAULT: ProjectileParams = ProjectileParams {
        gravity_scale: 1.0,
        drag_per_m: DRAG_PER_M,
        linear_drag_per_s: 0.0,
        wind_response: 0.0,
        radius_m: 0.0,
        lifetime_s: 0.0,
        max_range_m: 0.0,
        fuse_s: None,
        restitution: 0.0,
        friction: 0.0,
        max_bounces: 0,
        stop_speed_m_s: 0.0,
    };

    /// Every `f32` field with its valid range `[min, max]`, in declaration order. `fuse_s` (when
    /// set) must also be above 0.
    pub const FIELD_RANGES: &'static [(&'static str, f32, f32)] = &[
        ("gravity_scale", 0.0, 100.0),
        ("drag_per_m", 0.0, 10.0),
        ("linear_drag_per_s", 0.0, 1000.0),
        ("wind_response", 0.0, 1.0),
        ("radius_m", 0.0, 100.0),
        ("lifetime_s", 0.0, 86_400.0),
        ("max_range_m", 0.0, 1.0e7),
        ("fuse_s", 0.0, 86_400.0),
        ("restitution", 0.0, 1.0),
        ("friction", 0.0, 1.0),
        ("stop_speed_m_s", 0.0, 1.0e5),
    ];

    fn field_values(&self) -> [Option<f32>; 11] {
        [
            Some(self.gravity_scale),
            Some(self.drag_per_m),
            Some(self.linear_drag_per_s),
            Some(self.wind_response),
            Some(self.radius_m),
            Some(self.lifetime_s),
            Some(self.max_range_m),
            self.fuse_s,
            Some(self.restitution),
            Some(self.friction),
            Some(self.stop_speed_m_s),
        ]
    }

    /// `Ok` when every field is finite and inside [`ProjectileParams::FIELD_RANGES`] and a set fuse
    /// is above 0; otherwise the first bad field, in declaration order. Call it when a projectile
    /// kind is loaded (a mod, a settings message), never per tick. [`ProjectileParams::DEFAULT`] is
    /// valid.
    pub fn validate(&self) -> Result<(), ProjectileParamsError> {
        for (&(field, min, max), value) in Self::FIELD_RANGES.iter().zip(self.field_values()) {
            let Some(value) = value else { continue };
            let kind = if !value.is_finite() {
                Some(ProjectileParamsErrorKind::NotFinite)
            } else if value < 0.0 {
                Some(ProjectileParamsErrorKind::Negative)
            } else if value < min || value > max || (field == "fuse_s" && value <= 0.0) {
                Some(ProjectileParamsErrorKind::OutOfRange)
            } else {
                None
            };
            if let Some(kind) = kind {
                return Err(ProjectileParamsError {
                    field,
                    kind,
                    value,
                    range: [min, max],
                });
            }
        }
        Ok(())
    }
}

impl Default for ProjectileParams {
    fn default() -> Self {
        ProjectileParams::DEFAULT
    }
}

/// Why a projectile number is invalid. [`ProjectileParamsErrorKind::code`] is the protocol code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProjectileParamsErrorKind {
    /// NaN or infinite (`not_finite`).
    NotFinite,
    /// Below 0 (`negative`).
    Negative,
    /// Outside the field's documented range, or a fuse of 0 (`out_of_range`).
    OutOfRange,
}

impl ProjectileParamsErrorKind {
    /// Every kind, in declaration order.
    pub const ALL: [ProjectileParamsErrorKind; 3] = [
        ProjectileParamsErrorKind::NotFinite,
        ProjectileParamsErrorKind::Negative,
        ProjectileParamsErrorKind::OutOfRange,
    ];

    /// The error code: lowercase ascii, documented in `book/agents/det-projectiles.md`.
    pub const fn code(self) -> &'static str {
        match self {
            ProjectileParamsErrorKind::NotFinite => "not_finite",
            ProjectileParamsErrorKind::Negative => "negative",
            ProjectileParamsErrorKind::OutOfRange => "out_of_range",
        }
    }
}

/// The first invalid field [`ProjectileParams::validate`] found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectileParamsError {
    /// The field's name, as in [`ProjectileParams::FIELD_RANGES`].
    pub field: &'static str,
    /// What is wrong with it.
    pub kind: ProjectileParamsErrorKind,
    /// The value it holds.
    pub value: f32,
    /// The valid range, `[min, max]`.
    pub range: [f32; 2],
}

impl fmt::Display for ProjectileParamsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [min, max] = self.range;
        write!(
            f,
            "{}: projectile field `{}` = {} (valid {min} to {max})",
            self.kind.code(),
            self.field,
            self.value
        )
    }
}

impl std::error::Error for ProjectileParamsError {}

/// One projectile's state. The caller owns it (or a [`ProjectilePool`] does).
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProjectileState {
    /// Position of the centre, m, +Y up.
    pub pos: [f32; 3],
    /// Velocity, m/s.
    pub vel: [f32; 3],
    /// Ticks stepped since launch. The age in seconds is `age_ticks * dt`; timers count ticks
    /// ([`timer_ticks`]) so a fuse fires on the same tick on every peer.
    pub age_ticks: u32,
    /// Path length flown so far, m (bounces included).
    pub travelled_m: f32,
    /// Bounces so far.
    pub bounces: u16,
    /// At rest after its last bounce, waiting for its fuse or lifetime: it no longer moves or
    /// queries hits.
    pub resting: bool,
    /// False once the projectile has ended; [`step`] then does nothing.
    pub alive: bool,
    /// The body that fired it: its hit queries skip that body ([`SegmentQuery::ignore_body`]).
    pub owner: Option<u32>,
}

impl ProjectileState {
    /// A live projectile at `pos` with velocity `vel`, fired by `owner` (or by no body).
    pub const fn launch(pos: [f32; 3], vel: [f32; 3], owner: Option<u32>) -> Self {
        ProjectileState {
            pos,
            vel,
            age_ticks: 0,
            travelled_m: 0.0,
            bounces: 0,
            resting: false,
            alive: true,
            owner,
        }
    }
}

/// What a hit query hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum HitTarget {
    /// World geometry (ground, walls, props that are not bodies).
    World,
    /// A body, by the caller's id (an entity slot).
    Body(u32),
}

/// A swept-sphere query: a sphere of `radius_m` moving from `from` to `to` in one tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentQuery {
    /// Centre at the start of the tick.
    pub from: [f32; 3],
    /// Centre at the end of the tick, if nothing is in the way.
    pub to: [f32; 3],
    /// Sphere radius, m (0: a ray).
    pub radius_m: f32,
    /// A body the query must not report (the shooter).
    pub ignore_body: Option<u32>,
}

/// The first thing a [`SegmentQuery`] touches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Fraction of the segment, 0 to 1, at which the sphere first touches. 0 when it starts in
    /// contact and moves in.
    pub t: f32,
    /// The sphere's centre at that moment (`from + (to - from) * t`); the surface point is
    /// `point - normal * radius_m`.
    pub point: [f32; 3],
    /// Unit surface normal at the contact, pointing out of the thing hit.
    pub normal: [f32; 3],
    /// What was hit.
    pub target: HitTarget,
}

/// The hit seam (ADR-8509): the first contact of a swept sphere along a segment.
///
/// An implementation must be deterministic: the same query against the same contents gives the same
/// answer on every peer, ties (two contacts at the same `t`) broken by a fixed rule, not by a hash or
/// an address. It must report a contact only when the sphere moves into the surface (a projectile
/// that just bounced off a surface is not stopped by it again), and it skips
/// [`SegmentQuery::ignore_body`]. [`LinearScanHits`] is the reference implementation: a game's
/// spatial index (the entity side, the world side) must answer the same as a linear scan over the
/// same shapes.
pub trait SegmentHits {
    /// The first contact along `query`, or `None`.
    fn first_hit(&mut self, query: &SegmentQuery) -> Option<Hit>;
}

impl<T: SegmentHits + ?Sized> SegmentHits for &mut T {
    fn first_hit(&mut self, query: &SegmentQuery) -> Option<Hit> {
        (**self).first_hit(query)
    }
}

/// Two sources (the world and the bodies): the earlier contact; at the same `t`, the first one.
impl<A: SegmentHits, B: SegmentHits> SegmentHits for (A, B) {
    fn first_hit(&mut self, query: &SegmentQuery) -> Option<Hit> {
        let a = self.0.first_hit(query);
        let b = self.1.first_hit(query);
        match (a, b) {
            (Some(a), Some(b)) => Some(if b.t < a.t { b } else { a }),
            (a, None) => a,
            (None, b) => b,
        }
    }
}

/// Why a projectile ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ImpactReason {
    /// It hit something with no bounce left and no fuse, or a bounce stopped an unfused one.
    Hit,
    /// Its fuse ran out.
    Fuse,
    /// Its lifetime ran out.
    Expired,
    /// It flew its maximum range.
    Range,
}

impl ImpactReason {
    /// The reason's name: `hit`, `fuse`, `expired` or `range`.
    pub const fn name(self) -> &'static str {
        match self {
            ImpactReason::Hit => "hit",
            ImpactReason::Fuse => "fuse",
            ImpactReason::Expired => "expired",
            ImpactReason::Range => "range",
        }
    }
}

/// A projectile's end, on the tick it happened.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Impact {
    /// The pool slot (0 from a lone [`step`]).
    pub slot: u16,
    /// Position in this tick's impact list (0 from a lone [`step`]): impacts come in ascending
    /// slot order.
    pub order: u16,
    /// Where it ended: the centre at the contact for a hit, its position for the others (the point
    /// at exactly `max_range_m` of path for a range end in flight).
    pub point: [f32; 3],
    /// Surface normal of a hit; `[0, 0, 0]` for the other reasons.
    pub normal: [f32; 3],
    /// What it hit; `None` unless the reason is [`ImpactReason::Hit`].
    pub target: Option<HitTarget>,
    /// Speed at the end, m/s (before any bounce response).
    pub speed: f32,
    /// Why it ended.
    pub reason: ImpactReason,
    /// Its age in ticks at the end.
    pub age_ticks: u32,
}

/// A timer of `secs` at a tick of `dt` seconds, in ticks: `ceil(secs / dt)`, except that a quotient
/// within 1/1000 of a tick above a whole number rounds down to it (so 0.1 s at 60 Hz is 6 ticks
/// whatever the last bit of `dt`), and at least 1. A timer fires at the end of the step that brings
/// [`ProjectileState::age_ticks`] to this count. `u32::MAX` for a quotient that does not fit.
pub fn timer_ticks(secs: f32, dt: f32) -> u32 {
    let q = secs / dt;
    if !q.is_finite() || q >= 4_294_967_040.0 {
        return u32::MAX;
    }
    if q <= 1.0 {
        return 1;
    }
    let whole = q as u32;
    if q - whole as f32 > 1.0e-3 {
        whole + 1
    } else {
        whole
    }
}

/// One tick of velocity: gravity times `gravity_scale`, then the quadratic drag against the air
/// (moving with `wind * wind_response`), then the linear drag. With [`ProjectileParams::DEFAULT`] it
/// is [`crate::ballistic::step_debris`] bit for bit: the same operations in the same order, and the
/// wind is read only when `wind_response > 0`. Returns `vel` unchanged for a `dt` that is not a
/// positive finite number.
pub fn step_velocity(
    params: &ProjectileParams,
    gravity: f32,
    air: f32,
    wind: [f32; 3],
    mut vel: [f32; 3],
    dt: f32,
) -> [f32; 3] {
    if !(dt.is_finite() && dt > 0.0) {
        return vel;
    }
    vel[1] -= map_g(gravity) * params.gravity_scale * dt;
    if !air.is_finite() || air <= 0.0 {
        return vel;
    }
    if params.wind_response > 0.0 {
        let w = [
            wind[0] * params.wind_response,
            wind[1] * params.wind_response,
            wind[2] * params.wind_response,
        ];
        let mut rel = [vel[0] - w[0], vel[1] - w[1], vel[2] - w[2]];
        if quadratic_drag(&mut rel, params.drag_per_m, air, dt) {
            vel = [w[0] + rel[0], w[1] + rel[1], w[2] + rel[2]];
        }
    } else {
        quadratic_drag(&mut vel, params.drag_per_m, air, dt);
    }
    if params.linear_drag_per_s > 0.0 {
        let keep = 1.0 - det_min(params.linear_drag_per_s * (air / DRAG_REF_AIR) * dt, 1.0);
        vel[0] *= keep;
        vel[1] *= keep;
        vel[2] *= keep;
    }
    vel
}

/// The quadratic drag of `ballistic::step_debris` (its `apply_drag`, same operations and early
/// returns) with the drag constant as a parameter. `air` is already known positive. Returns whether
/// it changed `vel`.
fn quadratic_drag(vel: &mut [f32; 3], drag_per_m: f32, air: f32, dt: f32) -> bool {
    let speed = math::sqrt(vel[0] * vel[0] + vel[1] * vel[1] + vel[2] * vel[2]);
    if speed < 1.0e-6 {
        return false;
    }
    let drop = det_min(drag_per_m * (air / DRAG_REF_AIR) * speed * dt, 1.0);
    let keep = 1.0 - drop;
    vel[0] *= keep;
    vel[1] *= keep;
    vel[2] *= keep;
    true
}

fn length(v: [f32; 3]) -> f32 {
    math::sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2])
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    length([b[0] - a[0], b[1] - a[1], b[2] - a[2]])
}

/// Advance one projectile one tick on `world`, with `wind` (m/s) and hits from `hits`. Returns the
/// [`Impact`] on the tick it ends (`slot` and `order` 0; a [`ProjectilePool`] fills them in).
///
/// Order within the tick: (1) velocity ([`step_velocity`]); (2) a swept query from the position to
/// `pos + vel * dt`: on a contact the projectile moves to it and drops the rest of the tick's
/// travel, then bounces (reflects the normal speed times `restitution`, keeps `1 - friction` of the
/// tangential speed), or, with no bounce left, rests (fused) or ends with [`ImpactReason::Hit`];
/// (3) `age_ticks` goes up by one; (4) the fuse, then the lifetime, then the range. A hit wins over a
/// timer on the same tick. A resting projectile skips (1) and (2). A projectile that is not alive,
/// or a `dt` that is not a positive finite number, leaves the state unchanged and returns `None`.
pub fn step(
    params: &ProjectileParams,
    world: &WorldPhysics,
    wind: [f32; 3],
    state: &mut ProjectileState,
    dt: f32,
    hits: &mut impl SegmentHits,
) -> Option<Impact> {
    if !state.alive || !(dt.is_finite() && dt > 0.0) {
        return None;
    }
    let mut seg_from = state.pos;
    let mut seg_len = 0.0_f32;
    let mut ended = None;
    if !state.resting {
        state.vel = step_velocity(params, world.gravity_m_s2, world.drag_air(), wind, state.vel, dt);
        let from = state.pos;
        let v = state.vel;
        let to = [from[0] + v[0] * dt, from[1] + v[1] * dt, from[2] + v[2] * dt];
        let query = SegmentQuery {
            from,
            to,
            radius_m: params.radius_m,
            ignore_body: state.owner,
        };
        seg_from = from;
        match hits.first_hit(&query) {
            Some(hit) => {
                seg_len = distance(from, hit.point);
                state.travelled_m += seg_len;
                state.pos = hit.point;
                let speed = length(v);
                let n = hit.normal;
                let vn = v[0] * n[0] + v[1] * n[1] + v[2] * n[2];
                let mut stop = true;
                if state.bounces < params.max_bounces {
                    state.bounces += 1;
                    if vn < 0.0 {
                        let keep_t = 1.0 - params.friction;
                        let push = vn * params.restitution;
                        state.vel = [
                            (v[0] - n[0] * vn) * keep_t - n[0] * push,
                            (v[1] - n[1] * vn) * keep_t - n[1] * push,
                            (v[2] - n[2] * vn) * keep_t - n[2] * push,
                        ];
                    }
                    stop = length(state.vel) <= params.stop_speed_m_s;
                }
                if stop {
                    if params.fuse_s.is_some() {
                        state.vel = [0.0; 3];
                        state.resting = true;
                    } else {
                        state.alive = false;
                        ended = Some(Impact {
                            slot: 0,
                            order: 0,
                            point: hit.point,
                            normal: n,
                            target: Some(hit.target),
                            speed,
                            reason: ImpactReason::Hit,
                            age_ticks: state.age_ticks.saturating_add(1),
                        });
                    }
                }
            }
            None => {
                seg_len = distance(from, to);
                state.travelled_m += seg_len;
                state.pos = to;
            }
        }
    }
    state.age_ticks = state.age_ticks.saturating_add(1);
    if ended.is_some() {
        return ended;
    }
    let reason = if params
        .fuse_s
        .is_some_and(|f| state.age_ticks >= timer_ticks(f, dt))
    {
        Some(ImpactReason::Fuse)
    } else if params.lifetime_s > 0.0 && state.age_ticks >= timer_ticks(params.lifetime_s, dt) {
        Some(ImpactReason::Expired)
    } else if params.max_range_m > 0.0 && state.travelled_m >= params.max_range_m {
        if seg_len > 0.0 {
            // Back up to exactly the range along this tick's segment.
            let back = det_clamp((state.travelled_m - params.max_range_m) / seg_len, 0.0, 1.0);
            let p = state.pos;
            state.pos = [
                p[0] - (p[0] - seg_from[0]) * back,
                p[1] - (p[1] - seg_from[1]) * back,
                p[2] - (p[2] - seg_from[2]) * back,
            ];
            state.travelled_m = params.max_range_m;
        }
        Some(ImpactReason::Range)
    } else {
        None
    };
    let reason = reason?;
    state.alive = false;
    Some(Impact {
        slot: 0,
        order: 0,
        point: state.pos,
        normal: [0.0; 3],
        target: None,
        speed: length(state.vel),
        reason,
        age_ticks: state.age_ticks,
    })
}

/// [`step`] with no hits and no wind: a free-flight tick (the aim solver's probe).
pub fn step_free(
    params: &ProjectileParams,
    world: &WorldPhysics,
    state: &mut ProjectileState,
    dt: f32,
) -> Option<Impact> {
    step(params, world, [0.0; 3], state, dt, &mut NoHits)
}

#[cfg(test)]
mod tests;
