// SPDX-License-Identifier: MPL-2.0
//! Automatic trajectory prediction for motion matching (`docs/specs/52-motion-matching.md`
//! § 2.6a, ADR-2858): the desired path of the query, made from a desired velocity and facing
//! instead of being written by hand.
//!
//! A [`TrajectoryPredictor`] simulates the character's ground motion as two **critically damped
//! springs**: the velocity springs toward the desired velocity, the facing yaw toward the
//! desired yaw. Both have closed-form solutions, so the future trajectory at every
//! [`super::FeatureSchema::trajectory_times`] entry is *evaluated* from the current state, never
//! stepped, and advancing the state by one large `dt` gives the same result as many small ones
//! (no instability at a frame-time spike).
//!
//! With `y = 2 ln 2 / halflife`, `j0 = v0 − v*` and `j1 = a0 + j0·y`, the velocity spring's
//! solution is `v(t) = v* + (j0 + j1 t)·e^(−y t)`, its acceleration
//! `a(t) = (a0 − y j1 t)·e^(−y t)`, and the position, integrated in closed form,
//! `x(t) = x0 + v* t + j0/y + j1/y² − e^(−y t)·(j0/y + j1 t/y + j1/y²)`. The facing spring is
//! the same equation on the wrapped yaw error. The halflife sets the envelope `e^(−y t)`, which
//! falls to a quarter in one halflife: from rest the error `(1 + y t)·e^(−y t)` is then at 0.597
//! of its start, and at half about 1.2 halflives in.
//!
//! **Strafe**: with [`FacingMode::Aim`] the facing goal is [`TrajectoryPredictor::aim_yaw`],
//! independent of the velocity, so a character can walk sideways or backward while looking one
//! way; with [`FacingMode::Velocity`] (the default) it faces where it moves.
//!
//! The predictor also keeps its own **history** (ground position and yaw, sampled at every
//! update), from which [`TrajectoryPredictor::history_points`] reads past positions in the
//! character frame (extrapolated backward at the current velocity before the history is long
//! enough, the rule of spec 52 § 2.2). [`run_trajectory_predictors`] writes the predicted path
//! into the entity's [`MotionMatching::trajectory`] and the simulated ground position into
//! [`MotionMatching::ground`], so the matcher's history samples come from the same motion.

// Source, cited from memory and not re-read for this code: D. Holden, "Spring-It-On: The Game
// Developer's Spring-Roll-Call", theorangeduck.com, 2021 (the critically damped spring with a halflife parameter, its exact solution, and its use
// to predict a character's future trajectory from a desired velocity for motion matching). The
// closed-form position integral is re-derived here from the velocity solution; the facing modes
// (velocity or a separate aim yaw), the history buffer and the ECS glue are original design.
// Original code; nothing ported.

use std::collections::VecDeque;
use std::f32::consts::{LN_2, PI, TAU};

use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;

use super::{MotionMatching, TrajectoryPoint};
use crate::num::{det_atan2, det_exp, det_rotation_y, det_sin_cos};

/// Where the predicted character faces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacingMode {
    /// Face the desired velocity (hold the last facing while the desired velocity is zero).
    #[default]
    Velocity,
    /// Face [`TrajectoryPredictor::aim_yaw`] whatever the velocity (strafing).
    Aim,
}

impl FacingMode {
    /// The snake_case name (`velocity`, `aim`).
    pub fn name(self) -> &'static str {
        match self {
            FacingMode::Velocity => "velocity",
            FacingMode::Aim => "aim",
        }
    }

    /// The mode named `name` ([`FacingMode::name`]).
    pub fn from_name(name: &str) -> Option<FacingMode> {
        [FacingMode::Velocity, FacingMode::Aim]
            .into_iter()
            .find(|m| m.name() == name)
    }
}

/// The largest turn rate the springs track, radians per second; beyond it [`advance_state`]
/// clamps (the solution squares the rate, so an unbounded one overflows to NaN).
pub const MAX_TURN_RATE: f32 = 100.0;

/// Turn rates of the goals the springs track, radians per second (0 = fixed goals, clamped to
/// [`MAX_TURN_RATE`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Turning {
    /// The desired velocity turns at this rate.
    pub velocity: f32,
    /// The desired facing turns at this rate.
    pub facing: f32,
}

impl Turning {
    /// Fixed goals.
    pub const NONE: Turning = Turning {
        velocity: 0.0,
        facing: 0.0,
    };
}

/// Spring halflives of a [`TrajectoryPredictor`].
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PredictorSettings {
    /// Velocity spring halflife, seconds (default 0.2): the envelope `e^(−y t)` with
    /// `y = 2 ln 2 / halflife` falls to a quarter in this time. Clamped to `[1 ms, 10 000 s]`.
    pub velocity_halflife: f32,
    /// Facing spring halflife, seconds (default 0.15), the same way. Clamped to `[1 ms, 10 000 s]`.
    pub facing_halflife: f32,
    /// Seconds of history kept (default 1.5).
    pub history_seconds: f32,
}

impl Default for PredictorSettings {
    fn default() -> Self {
        Self {
            velocity_halflife: 0.2,
            facing_halflife: 0.15,
            history_seconds: 1.5,
        }
    }
}

/// The simulated ground motion: position and velocity on the XZ plane (Y is ignored), facing
/// yaw about +Y (0 = facing +Z, positive toward +X).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PredictorState {
    /// Ground position, world space, meters.
    pub position: Vec3,
    /// Velocity, meters per second.
    pub velocity: Vec3,
    /// Acceleration, meters per second squared.
    pub acceleration: Vec3,
    /// Facing yaw, radians.
    pub yaw: f32,
    /// Yaw rate, radians per second.
    pub yaw_rate: f32,
}

/// Wrap an angle to `(-pi, pi]`.
fn wrap(angle: f32) -> f32 {
    let wrapped = (angle + PI).rem_euclid(TAU) - PI;
    if wrapped <= -PI { wrapped + TAU } else { wrapped }
}

/// The spring decay rate of a halflife (`2 ln 2 / halflife`, halflife in `[1 ms, 10 000 s]`).
///
/// The upper bound keeps `y⁴` (in the turning spring's `|y + iw|²` squared) above the f32
/// underflow: a halflife of 1e12 s made it 0, the amplitude 0 / 0, and the predictor NaN for
/// good (audit corr-animation).
fn decay(halflife: f32) -> f32 {
    let h = if halflife.is_finite() {
        halflife.clamp(1e-3, 1e4)
    } else {
        0.2
    };
    2.0 * LN_2 / h
}

/// The critically damped spring of one scalar `x` whose rate `v` springs toward `goal`
/// (`v' = a`, `a' = −y²(v − goal) − 2y a`): `(x, v, a)` after `t` seconds, closed form.
#[cfg(test)]
fn velocity_spring(x: f32, v: f32, a: f32, goal: f32, y: f32, t: f32) -> (f32, f32, f32) {
    let j0 = v - goal;
    let j1 = a + j0 * y;
    let e = det_exp(-y * t);
    let x_t = x + goal * t + j0 / y + j1 / (y * y) - e * (j0 / y + j1 * t / y + j1 / (y * y));
    let v_t = goal + (j0 + j1 * t) * e;
    let a_t = (a - y * j1 * t) * e;
    (x_t, v_t, a_t)
}

/// A complex number for the planar spring (`re` = Z, `im` = X, so a heading `h` is `e^(ih)`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct C {
    re: f32,
    im: f32,
}

impl C {
    fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
    fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
    fn sub(self, o: C) -> C {
        C::new(self.re - o.re, self.im - o.im)
    }
    fn mul(self, o: C) -> C {
        C::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
    fn scale(self, k: f32) -> C {
        C::new(self.re * k, self.im * k)
    }
    fn div(self, o: C) -> C {
        let d = o.re * o.re + o.im * o.im;
        C::new(
            (self.re * o.re + self.im * o.im) / d,
            (self.im * o.re - self.re * o.im) / d,
        )
    }
    fn cis(angle: f32) -> C {
        let (s, c) = det_sin_cos(angle);
        C::new(c, s)
    }
}

/// The critically damped spring of a planar velocity `v` (acceleration `a`, position `x`) whose
/// goal `g` turns at `w` rad/s (`g(t) = g · e^(iwt)`; `w = 0` is a fixed goal): `(x, v, a)` after
/// `t` seconds, closed form. With `A = y² g / (y + iw)²` the particular solution is `A e^(iwt)`,
/// the homogeneous part `(c0 + c1 t) e^(-yt)` with `c0 = v0 - A`, `c1 = a0 - iwA + y c0`, and the
/// position integrates both exactly.
fn turning_velocity_spring(x: C, v: C, a: C, g: C, w: f32, y: f32, t: f32) -> (C, C, C) {
    let iw = C::new(0.0, w);
    let denom = C::new(y, w).mul(C::new(y, w));
    let amp = g.scale(y * y).div(denom);
    let c0 = v.sub(amp);
    let c1 = a.sub(iw.mul(amp)).add(c0.scale(y));
    let e = det_exp(-y * t);
    let rot = C::cis(w * t);
    // (e^(iwt) - 1) / (iw) = t sinc(wt / 2) e^(iwt / 2): `cos(wt) - 1` would lose all its digits in
    // f32 at a small turn rate, and this form is also exact at w = 0.
    let half = 0.5 * w * t;
    let sinc = if half.abs() > 1e-3 {
        det_sin_cos(half).0 / half
    } else {
        1.0 - half * half / 6.0
    };
    let swept = C::cis(half).scale(t * sinc);
    let x_t = x
        .add(amp.mul(swept))
        .add(c0.scale((1.0 - e) / y))
        .add(c1.scale(1.0 / (y * y) - e * (t / y + 1.0 / (y * y))));
    let v_t = amp.mul(rot).add(c0.add(c1.scale(t)).scale(e));
    let a_t = iw
        .mul(amp)
        .mul(rot)
        .add(c1.sub(c0.add(c1.scale(t)).scale(y)).scale(e));
    (x_t, v_t, a_t)
}

/// The critically damped spring of an angle toward a goal that turns at `w` rad/s
/// (`goal + w t`; the error wrapped to `(-pi, pi]`): `(angle, rate)` after `t` seconds. The
/// particular solution lags the ramp by `2w / y`.
fn turning_angle_spring(angle: f32, rate: f32, goal: f32, w: f32, y: f32, t: f32) -> (f32, f32) {
    let lag = 2.0 * w / y;
    let c0 = wrap(angle - goal) + lag;
    let c1 = rate - w + y * c0;
    let e = det_exp(-y * t);
    (
        goal + w * t - lag + (c0 + c1 * t) * e,
        w + (c1 - y * (c0 + c1 * t)) * e,
    )
}

/// Clean a turn rate: non-finite is 0, the rest within [`MAX_TURN_RATE`].
fn clean_turn_rate(w: f32) -> f32 {
    if w.is_finite() {
        w.clamp(-MAX_TURN_RATE, MAX_TURN_RATE)
    } else {
        0.0
    }
}

/// Clean a time step: non-finite or negative is 0.
fn clean_dt(dt: f32) -> f32 {
    if dt.is_finite() { dt.max(0.0) } else { 0.0 }
}

/// The state `t` seconds after `state` with springs toward `desired_velocity` (XZ) and
/// `desired_yaw` (closed form; any `t`, including a large one, is exact).
pub fn advance_state(
    state: &PredictorState,
    desired_velocity: Vec3,
    desired_yaw: f32,
    turn_rate: Turning,
    settings: &PredictorSettings,
    t: f32,
) -> PredictorState {
    let t = clean_dt(t);
    if t == 0.0 {
        return *state;
    }
    let goal = if desired_velocity.is_finite() {
        desired_velocity
    } else {
        Vec3::ZERO
    };
    let goal_yaw = if desired_yaw.is_finite() {
        desired_yaw
    } else {
        state.yaw
    };
    let yv = decay(settings.velocity_halflife);
    let yf = decay(settings.facing_halflife);
    let w_velocity = clean_turn_rate(turn_rate.velocity);
    let w_facing = clean_turn_rate(turn_rate.facing);
    let planar = |v: Vec3| C::new(v.z, v.x);
    let (p, v, a) = turning_velocity_spring(
        planar(state.position),
        planar(state.velocity),
        planar(state.acceleration),
        planar(goal),
        w_velocity,
        yv,
        t,
    );
    let (px, pz, vx, vz, ax, az) = (p.im, p.re, v.im, v.re, a.im, a.re);
    let (yaw, yaw_rate) = turning_angle_spring(state.yaw, state.yaw_rate, goal_yaw, w_facing, yf, t);
    PredictorState {
        position: Vec3::new(px, state.position.y, pz),
        velocity: Vec3::new(vx, 0.0, vz),
        acceleration: Vec3::new(ax, 0.0, az),
        yaw: wrap(yaw),
        yaw_rate,
    }
}

/// The future trajectory of `state` at each of `times` seconds ahead, in the character frame of
/// `state` (`x` right, `y` forward, the frame motion-matching queries use): positions relative
/// to the current ground position, turned by the current yaw, and facing directions relative to
/// the current facing.
pub fn predict_trajectory(
    state: &PredictorState,
    desired_velocity: Vec3,
    desired_yaw: f32,
    turn_rate: Turning,
    settings: &PredictorSettings,
    times: &[f32],
) -> Vec<TrajectoryPoint> {
    let to_local = det_rotation_y(-state.yaw);
    times
        .iter()
        .map(|&t| {
            let future = advance_state(state, desired_velocity, desired_yaw, turn_rate, settings, t);
            let offset = to_local
                * Vec3::new(
                    future.position.x - state.position.x,
                    0.0,
                    future.position.z - state.position.z,
                );
            let turn = future.yaw - state.yaw;
            TrajectoryPoint {
                position: Vec2::new(offset.x, offset.z),
                direction: {
                    let (sin, cos) = det_sin_cos(turn);
                    Vec2::new(sin, cos)
                },
            }
        })
        .collect()
}

/// A character's desired motion and its spring-simulated ground motion: writes the motion
/// matcher's desired trajectory every frame ([`run_trajectory_predictors`]).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct TrajectoryPredictor {
    /// The springs' halflives.
    pub settings: PredictorSettings,
    /// The velocity the game wants, world space (Y ignored), meters per second.
    pub desired_velocity: Vec3,
    /// What the facing springs toward.
    pub facing: FacingMode,
    /// The facing goal with [`FacingMode::Aim`] (strafing), radians (0 = +Z, positive toward +X).
    pub aim_yaw: f32,
    /// How fast the desired velocity is turning, radians per second (positive toward +X): the
    /// prediction turns the goal at this rate over its horizon, so a steady turn predicts a
    /// curved path (0 = the desired velocity is held; clamped to [`MAX_TURN_RATE`]).
    pub desired_turn_rate: f32,
    /// The simulated state (a host may overwrite the position and yaw to keep the simulation on
    /// the animated character).
    pub state: PredictorState,
    clock: f32,
    goal_yaw: f32,
    history: VecDeque<(f32, Vec3, f32)>,
}

impl TrajectoryPredictor {
    /// A predictor at `position` facing `yaw`, standing still.
    pub fn new(position: Vec3, yaw: f32) -> Self {
        let mut predictor = Self {
            state: PredictorState {
                position,
                yaw,
                ..PredictorState::default()
            },
            goal_yaw: yaw,
            ..Self::default()
        };
        predictor.record();
        predictor
    }

    /// The yaw the facing springs toward now: the aim yaw ([`FacingMode::Aim`]), or the desired
    /// velocity's heading ([`FacingMode::Velocity`]; the last heading while the desired velocity
    /// is below 1 cm/s).
    pub fn desired_yaw(&self) -> f32 {
        match self.facing {
            FacingMode::Aim if self.aim_yaw.is_finite() => self.aim_yaw,
            FacingMode::Aim => self.goal_yaw,
            FacingMode::Velocity => {
                let v = self.desired_velocity;
                if v.is_finite() && v.x * v.x + v.z * v.z > 1e-4 {
                    det_atan2(v.x, v.z)
                } else {
                    self.goal_yaw
                }
            }
        }
    }

    /// Advance the simulation by `dt` seconds (closed form; a non-finite or negative `dt` counts
    /// as zero) and record the new state in the history.
    #[tracing::instrument(target = "regia_animation", name = "motion_matching.predict", skip_all)]
    pub fn advance(&mut self, dt: f32) {
        let dt = clean_dt(dt);
        self.goal_yaw = self.desired_yaw();
        self.state = advance_state(
            &self.state,
            self.desired_velocity,
            self.goal_yaw,
            self.turning(),
            &self.settings,
            dt,
        );
        self.clock += dt;
        if self.clock >= 4096.0 {
            // Rebase the clock exactly (a multiple of 2048) so history differences stay precise.
            let shift = ((self.clock / 2048.0).floor() - 1.0) * 2048.0;
            self.clock -= shift;
            for (at, _, _) in &mut self.history {
                *at -= shift;
            }
        }
        self.record();
    }

    fn record(&mut self) {
        let (clock, position, yaw) = (self.clock, self.state.position, self.state.yaw);
        if !(position.is_finite() && yaw.is_finite()) {
            return;
        }
        if self.history.back().is_some_and(|&(at, _, _)| at >= clock) {
            self.history.pop_back();
        }
        self.history
            .push_back((clock, Vec3::new(position.x, 0.0, position.z), yaw));
        let keep = if self.settings.history_seconds.is_finite() {
            self.settings.history_seconds.clamp(0.0, 60.0)
        } else {
            1.5
        };
        while self.history.get(1).is_some_and(|&(at, _, _)| clock - at >= keep) {
            self.history.pop_front();
        }
    }

    /// The predicted path at `times` seconds ahead, in the character frame.
    pub fn trajectory(&self, times: &[f32]) -> Vec<TrajectoryPoint> {
        predict_trajectory(
            &self.state,
            self.desired_velocity,
            self.desired_yaw(),
            self.turning(),
            &self.settings,
            times,
        )
    }

    /// The turn rates the springs track: the desired velocity's, and the facing's too unless it
    /// holds an aim.
    pub fn turning(&self) -> Turning {
        Turning {
            velocity: self.desired_turn_rate,
            facing: match self.facing {
                FacingMode::Velocity => self.desired_turn_rate,
                FacingMode::Aim => 0.0,
            },
        }
    }

    /// Recorded `(seconds ago, ground position, yaw)` samples, newest first.
    pub fn history(&self) -> impl Iterator<Item = (f32, Vec3, f32)> + '_ {
        self.history
            .iter()
            .rev()
            .map(move |&(at, position, yaw)| (self.clock - at, position, yaw))
    }

    /// Past ground positions `behind` seconds ago (each), in the current character frame:
    /// interpolated between recorded samples, extrapolated backward at the current velocity
    /// before the oldest one.
    pub fn history_points(&self, behind: &[f32]) -> Vec<Vec2> {
        let now = Vec3::new(self.state.position.x, 0.0, self.state.position.z);
        let to_local = det_rotation_y(-self.state.yaw);
        behind
            .iter()
            .map(|&b| {
                let at = self.clock - clean_dt(b);
                let past = self.position_at(at).unwrap_or(now);
                let offset = to_local * (past - now);
                Vec2::new(offset.x, offset.z)
            })
            .collect()
    }

    fn position_at(&self, at: f32) -> Option<Vec3> {
        let &(first_at, first, _) = self.history.front()?;
        let &(last_at, last, _) = self.history.back()?;
        if at <= first_at {
            let v = Vec3::new(self.state.velocity.x, 0.0, self.state.velocity.z);
            return Some(first - v * (first_at - at));
        }
        if at >= last_at {
            return Some(last);
        }
        let index = self.history.partition_point(|&(t, _, _)| t <= at);
        let (t0, p0, _) = self.history[index - 1];
        let (t1, p1, _) = self.history[index];
        let u = if t1 - t0 > 1e-6 {
            (at - t0) / (t1 - t0)
        } else {
            0.0
        };
        Some(p0.lerp(p1, u))
    }

    /// Advance by `dt` and write the predicted path into `matching` (and the simulated ground
    /// position and yaw, so the matcher's history samples follow the same motion).
    pub fn drive(&mut self, matching: &mut MotionMatching, dt: f32) {
        self.advance(dt);
        let times = matching.database.schema().trajectory_times.clone();
        matching.trajectory = self.trajectory(&times);
        matching.ground = Some((self.state.position, self.state.yaw));
    }
}

/// Advance every [`TrajectoryPredictor`] and write its path into the [`MotionMatching`] on the
/// same entity. The [`super::MotionMatchPlugin`] runs it before the matcher; an entity without a
/// predictor keeps a hand-written trajectory.
#[tracing::instrument(target = "regia_animation", skip_all)]
pub fn run_trajectory_predictors(
    time: Res<regia_app::Time>,
    mut predictors: Query<
        (&mut TrajectoryPredictor, &mut MotionMatching),
        Without<regia_scene::layers::LayerParked>,
    >,
) {
    let dt = time.delta_secs();
    for (mut predictor, mut matching) in &mut predictors {
        predictor.drive(&mut matching, dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RK4 on `x' = v`, `v' = a`, `a' = −y²(v − goal) − 2y a`, in f64 with a tiny step.
    fn integrate(x: f64, v: f64, a: f64, goal: f64, y: f64, t: f64) -> (f64, f64, f64) {
        let f = |s: [f64; 3]| [s[1], s[2], -y * y * (s[1] - goal) - 2.0 * y * s[2]];
        let steps = (t / 1e-4).ceil().max(1.0) as usize;
        let h = t / steps as f64;
        let mut s = [x, v, a];
        for _ in 0..steps {
            let k1 = f(s);
            let k2 = f([0, 1, 2].map(|i| s[i] + 0.5 * h * k1[i]));
            let k3 = f([0, 1, 2].map(|i| s[i] + 0.5 * h * k2[i]));
            let k4 = f([0, 1, 2].map(|i| s[i] + h * k3[i]));
            s = [0, 1, 2].map(|i| s[i] + h / 6.0 * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i]));
        }
        (s[0], s[1], s[2])
    }

    /// Audit (corr-animation): a huge finite halflife underflowed the turning spring's
    /// denominator and left the predictor NaN for good.
    #[test]
    fn a_huge_halflife_keeps_the_predictor_finite() {
        let settings = PredictorSettings {
            velocity_halflife: 1.0e12,
            facing_halflife: 1.0e12,
            ..PredictorSettings::default()
        };
        let still = advance_state(
            &PredictorState::default(),
            Vec3::Z,
            0.0,
            Turning::NONE,
            &settings,
            1.0 / 60.0,
        );
        assert!(
            still.position.is_finite() && still.velocity.is_finite(),
            "{still:?}"
        );
        assert!(
            still.acceleration.is_finite() && still.yaw.is_finite(),
            "{still:?}"
        );
    }

    /// ADR-4477 / audit animation1-determinism H2: spring envelopes and trajectory directions
    /// use the portable series rather than the presentation host's libm.
    #[test]
    fn predictor_uses_portable_transcendental_math() {
        let angle = f32::from_bits(0x3edf_285b);
        let (sin, cos) = crate::num::det_sin_cos(angle);
        let c = C::cis(angle);
        assert_eq!(c.im.to_bits(), sin.to_bits());
        assert_eq!(c.re.to_bits(), cos.to_bits());
        assert_eq!(c.im.to_bits(), 0x3ed8_28b0);

        let y = f32::from_bits(0x413c_feab);
        let e = crate::num::det_exp(-y);
        let (got_angle, got_rate) = turning_angle_spring(0.2, -0.3, 0.7, 0.4, y, 1.0);
        let lag = 2.0 * 0.4 / y;
        let c0 = wrap(0.2 - 0.7) + lag;
        let c1 = -0.3 - 0.4 + y * c0;
        let want_angle = 0.7 + 0.4 - lag + (c0 + c1) * e;
        let want_rate = 0.4 + (c1 - y * (c0 + c1)) * e;
        assert_eq!(got_angle.to_bits(), want_angle.to_bits());
        assert_eq!(got_rate.to_bits(), want_rate.to_bits());

        let state = PredictorState::default();
        let settings = PredictorSettings::default();
        let goal = Vec3::new(1.0, 0.0, 1.0);
        let yaw = crate::num::det_atan2(goal.x, goal.z);
        let future = advance_state(&state, goal, yaw, Turning::NONE, &settings, 1.0);
        let path = predict_trajectory(&state, goal, yaw, Turning::NONE, &settings, &[1.0]);
        let (turn_sin, turn_cos) = crate::num::det_sin_cos(future.yaw - state.yaw);
        assert_eq!(path[0].direction.x.to_bits(), turn_sin.to_bits());
        assert_eq!(path[0].direction.y.to_bits(), turn_cos.to_bits());
        let offset = crate::num::det_rotation_y(-state.yaw) * future.position;
        assert_eq!(path[0].position.x.to_bits(), offset.x.to_bits());
        assert_eq!(path[0].position.y.to_bits(), offset.z.to_bits());
    }

    #[test]
    fn the_closed_form_matches_the_integrated_spring() {
        let settings = PredictorSettings::default();
        let y = decay(settings.velocity_halflife);
        for (x, v, a, goal) in [
            (0.0, 0.0, 0.0, 2.0),
            (1.0, -1.5, 3.0, 0.5),
            (-2.0, 4.0, -8.0, -1.0),
        ] {
            for t in [1.0 / 3.0, 2.0 / 3.0, 1.0, 2.5] {
                let (cx, cv, ca) = velocity_spring(x, v, a, goal, y, t);
                let (rx, rv, ra) = integrate(
                    f64::from(x),
                    f64::from(v),
                    f64::from(a),
                    f64::from(goal),
                    f64::from(y),
                    f64::from(t),
                );
                assert!((f64::from(cx) - rx).abs() < 1e-4, "x {cx} vs {rx} at {t}");
                assert!((f64::from(cv) - rv).abs() < 1e-4, "v {cv} vs {rv} at {t}");
                assert!((f64::from(ca) - ra).abs() < 1e-3, "a {ca} vs {ra} at {t}");
            }
        }
        // The halflife: from rest, the velocity error halves in about `halflife` (the critically
        // damped curve `(1 + y t) e^(-y t)` at `t = halflife` is 0.5966...).
        let (_, v, _) = velocity_spring(0.0, 0.0, 0.0, 1.0, y, settings.velocity_halflife);
        assert!((v - (1.0 - 0.596_6)).abs() < 1e-3, "{v}");
    }

    #[test]
    fn the_turning_closed_form_matches_the_integrated_spring() {
        let y = decay(0.2);
        // Planar: x' = v, v' = a, a' = y^2 (g(t) - v) - 2 y a with g(t) = g0 turned by w t.
        for w in [0.0f64, 1.2, -2.5] {
            let (x0, v0, a0, g0) = ([0.5, -1.0], [1.0, 0.2], [0.0, -3.0], [1.5, 0.0]);
            let yd = f64::from(y);
            let goal = |t: f64| {
                let (s, c) = (w * t).sin_cos();
                [g0[0] * c - g0[1] * s, g0[0] * s + g0[1] * c]
            };
            let f = |t: f64, st: [f64; 6]| {
                let g = goal(t);
                [
                    st[2],
                    st[3],
                    st[4],
                    st[5],
                    yd * yd * (g[0] - st[2]) - 2.0 * yd * st[4],
                    yd * yd * (g[1] - st[3]) - 2.0 * yd * st[5],
                ]
            };
            let mut st = [x0[0], x0[1], v0[0], v0[1], a0[0], a0[1]];
            let mut t = 0.0;
            for target in [1.0 / 3.0, 2.0 / 3.0, 1.0] {
                // Steps that land exactly on the target (a fixed 1e-4 grid overshoots 1/3 s).
                let n = ((target - t) / 1e-4f64).ceil().max(1.0) as usize;
                let h = (target - t) / n as f64;
                for _ in 0..n {
                    let k1 = f(t, st);
                    let k2 = f(t + h / 2.0, std::array::from_fn(|i| st[i] + 0.5 * h * k1[i]));
                    let k3 = f(t + h / 2.0, std::array::from_fn(|i| st[i] + 0.5 * h * k2[i]));
                    let k4 = f(t + h, std::array::from_fn(|i| st[i] + h * k3[i]));
                    st = std::array::from_fn(|i| {
                        st[i] + h / 6.0 * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i])
                    });
                    t += h;
                }
                t = target;
                let c = |p: [f64; 2]| C::new(p[0] as f32, p[1] as f32);
                let (x, v, _) =
                    turning_velocity_spring(c(x0), c(v0), c(a0), c(g0), w as f32, y, target as f32);
                assert!(
                    (f64::from(x.re) - st[0]).abs() < 1e-4 && (f64::from(x.im) - st[1]).abs() < 1e-4,
                    "x w={w} t={target}: {x:?} vs {st:?}"
                );
                assert!(
                    (f64::from(v.re) - st[2]).abs() < 1e-4 && (f64::from(v.im) - st[3]).abs() < 1e-4,
                    "v w={w} t={target}"
                );
            }
        }
        // The angle tracking a ramp: settles `2w / y` behind it, and with w = 0 on the goal.
        let (a, r) = turning_angle_spring(0.0, 0.0, 0.0, 1.0, y, 50.0);
        assert!(
            (a - (50.0 - 2.0 / y)).abs() < 1e-3 && (r - 1.0).abs() < 1e-4,
            "{a} {r}"
        );
        let (a, _) = turning_angle_spring(0.5, 0.0, 0.0, 0.0, y, 50.0);
        assert!(a.abs() < 1e-5);
        // A steady turn predicts a curved path: the far point turns toward the turn.
        let mut p = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
        p.desired_velocity = Vec3::new(0.0, 0.0, 1.5);
        p.state.velocity = p.desired_velocity;
        p.desired_turn_rate = 1.2;
        let path = p.trajectory(&[1.0 / 3.0, 2.0 / 3.0, 1.0]);
        assert!(path[2].position.x > 0.3 && path[2].direction.x > 0.5, "{path:?}");
    }

    /// RK4 of the turning spring in f64, the state `[x, v, a]` (two components each) at each of
    /// the increasing `targets`, every step landing exactly on them.
    fn integrate_turning(start: [f64; 6], g0: [f64; 2], w: f64, y: f64, targets: &[f64]) -> Vec<[f64; 6]> {
        let f = |t: f64, st: [f64; 6]| {
            let (s, c) = (w * t).sin_cos();
            let g = [g0[0] * c - g0[1] * s, g0[0] * s + g0[1] * c];
            [
                st[2],
                st[3],
                st[4],
                st[5],
                y * y * (g[0] - st[2]) - 2.0 * y * st[4],
                y * y * (g[1] - st[3]) - 2.0 * y * st[5],
            ]
        };
        let (mut st, mut t) = (start, 0.0);
        targets
            .iter()
            .map(|&target| {
                let n = ((target - t) / 1e-4f64).ceil().max(1.0) as usize;
                let h = (target - t) / n as f64;
                for _ in 0..n {
                    let k1 = f(t, st);
                    let k2 = f(t + h / 2.0, std::array::from_fn(|i| st[i] + 0.5 * h * k1[i]));
                    let k3 = f(t + h / 2.0, std::array::from_fn(|i| st[i] + 0.5 * h * k2[i]));
                    let k4 = f(t + h, std::array::from_fn(|i| st[i] + h * k3[i]));
                    st = std::array::from_fn(|i| {
                        st[i] + h / 6.0 * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i])
                    });
                    t += h;
                }
                t = target;
                st
            })
            .collect()
    }

    #[test]
    fn a_small_turn_rate_keeps_the_closed_form_accurate() {
        // `w t` just above and well above where `cos(w t)` stops resolving in f32: a sprint speed
        // makes any lost lateral precision visible.
        let y = decay(0.2);
        let targets = [1.0 / 3.0, 2.0 / 3.0, 1.0];
        let c = |p: [f64; 2]| C::new(p[0] as f32, p[1] as f32);
        for w in [
            1.01e-4f64, 2.4e-4, 3.1e-4, 5e-4, 7e-4, 1e-3, 3e-3, 1e-2, -2.4e-4, -5e-4,
        ] {
            let (g0, v0) = ([6.0, 0.0], [6.0, 0.0]);
            let reference =
                integrate_turning([0.0, 0.0, v0[0], v0[1], 0.0, 0.0], g0, w, f64::from(y), &targets);
            for (st, &target) in reference.iter().zip(&targets) {
                let (x, v, _) = turning_velocity_spring(
                    C::new(0.0, 0.0),
                    c(v0),
                    C::new(0.0, 0.0),
                    c(g0),
                    w as f32,
                    y,
                    target as f32,
                );
                assert!(
                    (f64::from(x.re) - st[0]).abs() < 1e-4 && (f64::from(x.im) - st[1]).abs() < 1e-4,
                    "x w={w} t={target}: {x:?} vs {st:?}"
                );
                assert!(
                    (f64::from(v.re) - st[2]).abs() < 1e-4 && (f64::from(v.im) - st[3]).abs() < 1e-4,
                    "v w={w} t={target}: {v:?} vs {st:?}"
                );
            }
        }
    }

    #[test]
    fn an_absurd_turn_rate_leaves_the_state_finite() {
        // `w * w` overflows f32 from about 2e19; a host writes this field directly.
        for w in [1e19f32, 1e30, f32::MAX, -f32::MAX] {
            let mut p = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
            p.desired_velocity = Vec3::new(0.0, 0.0, 1.5);
            p.desired_turn_rate = w;
            p.advance(1.0 / 30.0);
            let s = p.state;
            assert!(
                s.position.is_finite()
                    && s.velocity.is_finite()
                    && s.acceleration.is_finite()
                    && s.yaw.is_finite()
                    && s.yaw_rate.is_finite(),
                "{w}: {s:?}"
            );
            let path = p.trajectory(&[1.0 / 3.0, 1.0]);
            assert!(
                path.iter()
                    .all(|t| t.position.is_finite() && t.direction.is_finite()),
                "{w}: {path:?}"
            );
        }
    }

    #[test]
    fn many_small_steps_equal_one_big_step() {
        let settings = PredictorSettings::default();
        let start = PredictorState {
            position: Vec3::new(1.0, 0.0, -2.0),
            velocity: Vec3::new(0.5, 0.0, 1.0),
            acceleration: Vec3::ZERO,
            yaw: 0.3,
            yaw_rate: -0.5,
        };
        let goal = Vec3::new(-1.0, 0.0, 2.0);
        let mut stepped = start;
        for _ in 0..90 {
            stepped = advance_state(&stepped, goal, 2.0, Turning::NONE, &settings, 1.0 / 90.0);
        }
        let once = advance_state(&start, goal, 2.0, Turning::NONE, &settings, 1.0);
        assert!(
            stepped.position.distance(once.position) < 1e-4,
            "{stepped:?} vs {once:?}"
        );
        assert!(stepped.velocity.distance(once.velocity) < 1e-4);
        assert!((stepped.yaw - once.yaw).abs() < 1e-4);
    }

    #[test]
    fn a_dt_spike_settles_on_the_goal_and_bad_steps_do_nothing() {
        let settings = PredictorSettings::default();
        let start = PredictorState {
            velocity: Vec3::new(3.0, 0.0, 0.0),
            acceleration: Vec3::new(-50.0, 0.0, 10.0),
            yaw_rate: 20.0,
            ..PredictorState::default()
        };
        for spike in [5.0, 1e3, 1e6] {
            let s = advance_state(&start, Vec3::Z, 1.0, Turning::NONE, &settings, spike);
            assert!(
                s.position.is_finite() && s.velocity.is_finite() && s.yaw.is_finite(),
                "{spike}"
            );
            assert!(s.velocity.distance(Vec3::Z) < 1e-4, "{spike}: {:?}", s.velocity);
            assert!((s.yaw - 1.0).abs() < 1e-4, "{spike}: {}", s.yaw);
        }
        for bad in [f32::NAN, -1.0, f32::INFINITY] {
            let same = advance_state(&start, Vec3::Z, 1.0, Turning::NONE, &settings, bad);
            assert_eq!(same.position, start.position, "{bad}");
            assert_eq!(same.velocity, start.velocity, "{bad}");
        }
        let mut predictor = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
        predictor.desired_velocity = Vec3::new(f32::NAN, 0.0, 1.0);
        predictor.advance(1e9);
        assert!(predictor.state.position.is_finite());
    }

    #[test]
    fn the_path_is_in_the_character_frame_and_strafing_keeps_the_facing() {
        let times = [1.0 / 3.0, 2.0 / 3.0, 1.0];
        // Walking forward at steady speed while facing +X (yaw pi/2): the path is straight ahead.
        let mut walker = TrajectoryPredictor::new(Vec3::new(5.0, 0.0, 5.0), PI / 2.0);
        walker.desired_velocity = Vec3::new(1.5, 0.0, 0.0);
        walker.state.velocity = walker.desired_velocity;
        let path = walker.trajectory(&times);
        for (p, &t) in path.iter().zip(&times) {
            assert!(p.position.distance(Vec2::new(0.0, 1.5 * t)) < 1e-4, "{p:?}");
            assert!(p.direction.distance(Vec2::Y) < 1e-4);
        }
        // Strafing right at constant facing: the path goes sideways, the facing stays.
        let mut strafer = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
        strafer.facing = FacingMode::Aim;
        strafer.aim_yaw = 0.0;
        strafer.desired_velocity = Vec3::new(1.5, 0.0, 0.0);
        strafer.state.velocity = strafer.desired_velocity;
        for (p, &t) in strafer.trajectory(&times).iter().zip(&times) {
            assert!(p.position.distance(Vec2::new(1.5 * t, 0.0)) < 1e-4, "{p:?}");
            assert!(p.direction.distance(Vec2::Y) < 1e-4);
        }
        // Velocity facing turns toward the movement instead.
        strafer.facing = FacingMode::Velocity;
        let turned = strafer.trajectory(&times);
        assert!(turned[2].direction.x > 0.9, "{:?}", turned[2]);
        assert_eq!(FacingMode::from_name("aim"), Some(FacingMode::Aim));
    }

    #[test]
    fn history_is_recorded_and_extrapolated_before_it_fills() {
        let mut predictor = TrajectoryPredictor::new(Vec3::ZERO, 0.0);
        predictor.desired_velocity = Vec3::new(0.0, 0.0, 2.0);
        predictor.state.velocity = predictor.desired_velocity;
        // Before any update: extrapolated backward at the current velocity.
        let early = predictor.history_points(&[0.5]);
        assert!(early[0].distance(Vec2::new(0.0, -1.0)) < 1e-5, "{early:?}");
        for _ in 0..60 {
            predictor.advance(1.0 / 60.0);
        }
        let past = predictor.history_points(&[0.25, 0.5]);
        assert!(past[0].distance(Vec2::new(0.0, -0.5)) < 1e-3, "{past:?}");
        assert!(past[1].distance(Vec2::new(0.0, -1.0)) < 1e-3, "{past:?}");
        assert!(predictor.history().count() > 30);
        // Long runs keep the history bounded.
        for _ in 0..600 {
            predictor.advance(1.0 / 60.0);
        }
        assert!(
            predictor.history().count() <= 92,
            "{}",
            predictor.history().count()
        );
    }
}
