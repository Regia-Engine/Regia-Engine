// SPDX-License-Identifier: MPL-2.0
//! Launch angles for a target, without drag (closed form) and with drag (a fixed number of secant
//! steps on the pitch), and the time of flight (ADR-8509).
//!
//! Every function here is pure and deterministic (IEEE `f32`, transcendentals through
//! [`crate::math`]), so the simulation (an AI picking its pitch on a tick) and the UI (a trajectory
//! arc, aim assist) may both call it and get the same bits. The arc a UI draws from it is
//! presentation; the shot itself is whatever the simulation steps.
//!
//! The closed form is the standard drag-free projectile result: to hit a point `x` metres away
//! horizontally and `y` metres up at muzzle speed `v` under gravity `g`,
//! `tan(pitch) = (v² ± sqrt(v⁴ - g (g x² + 2 y v²))) / (g x)`; a negative root means the target is
//! out of reach. The `-` root is the low (flat) arc, the `+` root the high (lobbed) arc. It is the
//! continuous path: a fixed-tick simulation of the same shot ([`crate::projectile::step`]) lands
//! within about `g * t * dt / 2` of it (0.25 m for a 3 s flight at 60 Hz).
//! [`solve_with_drag`] flies the real tick step, so it also closes that gap.
//!
//! ```
//! use regia_detsim::aim;
//!
//! // A target 100 m away and 10 m up, at 60 m/s under Earth gravity.
//! let (low, high) = aim::launch_angles([100.0, 10.0, 0.0], 60.0, 9.81).unwrap();
//! assert!(low < high);
//! let t = aim::time_of_flight([100.0, 10.0, 0.0], 60.0, low, 9.81).unwrap();
//! assert!(t > 1.0 && t < 3.0);
//! assert!(aim::launch_angles([1000.0, 0.0, 0.0], 60.0, 9.81).is_none()); // out of reach
//! ```

use crate::ballistic::map_g;
use crate::float::det_clamp;
use crate::math;
use crate::projectile::{ProjectileParams, ProjectileState, step_free};
use crate::site_physics::WorldPhysics;
use crate::tick::tick_dt;

/// Horizontal distances below this (m) count as straight up or down.
const VERTICAL_M: f32 = 1.0e-6;

/// A quarter turn.
const HALF_PI: f32 = core::f32::consts::FRAC_PI_2;

fn horizontal(delta: [f32; 3]) -> f32 {
    math::sqrt(delta[0] * delta[0] + delta[2] * delta[2])
}

fn finite3(v: [f32; 3]) -> bool {
    v[0].is_finite() && v[1].is_finite() && v[2].is_finite()
}

/// The two drag-free launch pitches (radians above the horizontal) that put a shot of muzzle speed
/// `speed` from the origin through `delta` under gravity `g` (m/s², downward): `(low, high)`, with
/// `low <= high`. `None` when the target is out of reach, or `speed` or `g` is not a positive finite
/// number, or `delta` is not finite. A target straight above (within 1e-6 m horizontally) gives
/// `(π/2, π/2)` when the shot reaches its height; one straight below gives `(-π/2, π/2)`.
pub fn launch_angles(delta: [f32; 3], speed: f32, g: f32) -> Option<(f32, f32)> {
    if !finite3(delta) || !(speed.is_finite() && speed > 0.0) || !(g.is_finite() && g > 0.0) {
        return None;
    }
    let x = horizontal(delta);
    let y = delta[1];
    let v2 = speed * speed;
    if x < VERTICAL_M {
        if y > 0.0 {
            return (v2 >= 2.0 * g * y).then_some((HALF_PI, HALF_PI));
        }
        return Some((-HALF_PI, HALF_PI));
    }
    let disc = v2 * v2 - g * (g * x * x + 2.0 * y * v2);
    if disc.is_nan() || disc < 0.0 {
        return None;
    }
    let root = math::sqrt(disc);
    let gx = g * x;
    Some((math::atan2(v2 - root, gx), math::atan2(v2 + root, gx)))
}

/// The heading (radians, `atan2(dz, dx)`) toward `delta`; 0 for a target straight up or down.
pub fn yaw_to(delta: [f32; 3]) -> f32 {
    if horizontal(delta) < VERTICAL_M {
        return 0.0;
    }
    math::atan2(delta[2], delta[0])
}

/// The launch velocity of a shot at `speed` and `pitch` toward `delta`'s heading:
/// `[v cos(pitch) dx / x, v sin(pitch), v cos(pitch) dz / x]` with `x` the horizontal distance
/// (`+X` for a target straight up or down).
pub fn launch_velocity(delta: [f32; 3], speed: f32, pitch: f32) -> [f32; 3] {
    let x = horizontal(delta);
    let (hx, hz) = if x < VERTICAL_M {
        (1.0, 0.0)
    } else {
        (delta[0] / x, delta[2] / x)
    };
    let flat = speed * math::cos(pitch);
    [flat * hx, speed * math::sin(pitch), flat * hz]
}

/// Drag-free time (s) for a shot at `speed` and `pitch` to reach `delta`: the horizontal distance
/// over the horizontal speed, or, for a target straight above or below (or a vertical shot), the
/// later root of the height equation. `None` when the shot never gets there.
pub fn time_of_flight(delta: [f32; 3], speed: f32, pitch: f32, g: f32) -> Option<f32> {
    if !finite3(delta) || !speed.is_finite() || !pitch.is_finite() || !(g.is_finite() && g > 0.0) {
        return None;
    }
    let x = horizontal(delta);
    let flat = speed * math::cos(pitch);
    if x >= VERTICAL_M && flat > VERTICAL_M {
        return Some(x / flat);
    }
    let vy = speed * math::sin(pitch);
    let disc = vy * vy - 2.0 * g * delta[1];
    if disc.is_nan() || disc < 0.0 {
        return None;
    }
    let t = (vy + math::sqrt(disc)) / g;
    (t >= 0.0).then_some(t)
}

/// Which of the two arcs [`solve_with_drag`] solves for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AimArc {
    /// The flat arc (the lower pitch).
    #[default]
    Low,
    /// The lobbed arc (the higher pitch).
    High,
}

/// Settings of [`solve_with_drag`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragAim {
    /// Tick length of the probe flights, s (the simulation's).
    pub dt: f32,
    /// Ticks a probe flies at most before it counts as falling short.
    pub max_ticks: u32,
    /// The arc to solve.
    pub arc: AimArc,
    /// The largest miss (m, vertical, at the target's horizontal distance) that counts as a solution.
    pub tolerance_m: f32,
}

impl DragAim {
    /// 60 Hz probes of at most 60 s, the low arc, a 0.05 m tolerance.
    pub const DEFAULT: DragAim = DragAim {
        dt: tick_dt(),
        max_ticks: 3600,
        arc: AimArc::Low,
        tolerance_m: 0.05,
    };
}

impl Default for DragAim {
    fn default() -> Self {
        DragAim::DEFAULT
    }
}

/// A solved shot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AimSolution {
    /// Launch pitch, radians above the horizontal.
    pub pitch: f32,
    /// Heading, radians ([`yaw_to`]).
    pub yaw: f32,
    /// Launch velocity ([`launch_velocity`]).
    pub velocity: [f32; 3],
    /// Time to the target's horizontal distance, s (interpolated between ticks).
    pub time_s: f32,
    /// Signed vertical miss at the target's horizontal distance, m (positive: above the target).
    pub miss_m: f32,
    /// Probe flights flown (at most `iterations + 2`).
    pub probes: u32,
}

/// One probe: fly the shot at `pitch` until it passes the target's horizontal distance; the signed
/// height miss there and the time, or `None` when it ends or stalls first.
fn probe(
    params: &ProjectileParams,
    world: &WorldPhysics,
    delta: [f32; 3],
    speed: f32,
    pitch: f32,
    opts: &DragAim,
) -> Option<(f32, f32)> {
    let x = horizontal(delta);
    let (hx, hz) = (delta[0] / x, delta[2] / x);
    let mut state = ProjectileState::launch([0.0; 3], launch_velocity(delta, speed, pitch), None);
    let mut prev_along = 0.0_f32;
    let mut prev_y = 0.0_f32;
    for tick in 0..opts.max_ticks {
        let ended = step_free(params, world, &mut state, opts.dt);
        let along = state.pos[0] * hx + state.pos[2] * hz;
        if along >= x {
            let span = along - prev_along;
            let f = if span > 0.0 {
                det_clamp((x - prev_along) / span, 0.0, 1.0)
            } else {
                1.0
            };
            let y = prev_y + (state.pos[1] - prev_y) * f;
            return Some((y - delta[1], (tick as f32 + f) * opts.dt));
        }
        if ended.is_some() || state.vel[0] * hx + state.vel[2] * hz <= 0.0 {
            return None;
        }
        prev_along = along;
        prev_y = state.pos[1];
    }
    None
}

/// The probes of [`solve_with_drag`] so far: the best one, and the latest pitch below and above the
/// target (a bracket once both are known).
#[derive(Default)]
struct Track {
    /// `(pitch, miss, time)` of the smallest miss.
    best: Option<(f32, f32, f32)>,
    /// Latest pitch whose shot passed below the target.
    below: Option<f32>,
    /// Latest pitch whose shot passed above it.
    above: Option<f32>,
}

impl Track {
    fn note(&mut self, pitch: f32, result: Option<(f32, f32)>) {
        let Some((miss, time)) = result else { return };
        if self.best.is_none_or(|(_, m, _)| miss.abs() < m.abs()) {
            self.best = Some((pitch, miss, time));
        }
        if miss < 0.0 {
            self.below = Some(pitch);
        } else if miss > 0.0 {
            self.above = Some(pitch);
        }
    }

    /// `pitch`, or the bracket's midpoint when a bracket is known and `pitch` is not strictly inside
    /// it (or is NaN).
    fn inside_bracket(&self, pitch: f32) -> f32 {
        match (self.below, self.above) {
            (Some(a), Some(b)) => {
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                if pitch > lo && pitch < hi {
                    pitch
                } else {
                    (a + b) * 0.5
                }
            }
            _ => pitch,
        }
    }
}

/// The pitch that hits `delta` with drag: start from the drag-free pitch of `opts.arc` (gravity
/// `map_g(world gravity) * params.gravity_scale`), probe it and a pitch 0.02 rad toward the longer
/// shot, then take `iterations` secant steps on the pitch (fewer if a probe misses by under 0.1 mm;
/// a step that leaves a known bracket is a bisection, and a probe that falls short of the target's
/// distance doubles the step toward the longer shot),
/// each probe a fixed-tick flight of [`crate::projectile::step`] with no wind and no hits. Returns the
/// best probe when it misses by at most `opts.tolerance_m`, else `None` (out of reach with drag, or
/// not converged in `iterations`). `None` too for a target out of drag-free reach, one within 1 mm
/// horizontally (use [`launch_angles`]), or a bad `speed`, `delta`, `dt` or `max_ticks`.
///
/// Bounded: at most `(iterations + 2) * opts.max_ticks` ticks. Call it when an AI picks a target or a
/// UI draws an arc, not per projectile per tick. Same inputs, same bits.
pub fn solve_with_drag(
    params: &ProjectileParams,
    world: &WorldPhysics,
    delta: [f32; 3],
    speed: f32,
    iterations: u8,
    opts: &DragAim,
) -> Option<AimSolution> {
    if !finite3(delta)
        || !(speed.is_finite() && speed > 0.0)
        || !(opts.dt.is_finite() && opts.dt > 0.0)
        || opts.max_ticks == 0
        || horizontal(delta) < 1.0e-3
    {
        return None;
    }
    let g = map_g(world.gravity_m_s2) * params.gravity_scale;
    let start = if g > 0.0 {
        let (low, high) = launch_angles(delta, speed, g)?;
        match opts.arc {
            AimArc::Low => low,
            AimArc::High => high,
        }
    } else {
        math::atan2(delta[1], horizontal(delta))
    };
    // Drag shortens the shot: the low arc needs more pitch, the high arc less. A probe that falls
    // short doubles the step.
    let mut nudge: f32 = match opts.arc {
        AimArc::Low => 0.02,
        AimArc::High => -0.02,
    };
    let limit = HALF_PI - 1.0e-3;
    let mut probes = 0_u32;
    let mut fly = |pitch: f32| {
        probes += 1;
        probe(params, world, delta, speed, pitch, opts)
    };
    let mut track = Track::default();
    let mut p0 = start;
    let mut f0 = fly(p0);
    track.note(p0, f0);
    let mut p1 = det_clamp(start + nudge, -limit, limit);
    let mut f1 = fly(p1);
    track.note(p1, f1);
    for _ in 0..iterations {
        let next = match (f0, f1) {
            (Some((m0, _)), Some((m1, _))) => {
                if m1.abs() < 1.0e-4 {
                    break;
                }
                let secant = if m1 == m0 {
                    p1 + nudge
                } else {
                    p1 - m1 * (p1 - p0) / (m1 - m0)
                };
                track.inside_bracket(secant)
            }
            // The last probe reached the target's distance, the one before fell short.
            (None, Some((m1, _))) => {
                if m1 > 0.0 {
                    (p0 + p1) * 0.5
                } else {
                    p1 + nudge
                }
            }
            // Still short: a longer step toward the longer shot.
            (_, None) => {
                nudge *= 2.0;
                p1 + nudge
            }
        };
        let next = det_clamp(next, -limit, limit);
        p0 = p1;
        f0 = f1;
        p1 = next;
        f1 = fly(p1);
        track.note(p1, f1);
    }
    let (pitch, miss, time) = track.best?;
    if miss.is_nan() || miss.abs() > opts.tolerance_m {
        return None;
    }
    Some(AimSolution {
        pitch,
        yaw: yaw_to(delta),
        velocity: launch_velocity(delta, speed, pitch),
        time_s: time,
        miss_m: miss,
        probes,
    })
}
