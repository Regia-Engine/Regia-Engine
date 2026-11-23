// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/rate_gate.rs, commit d6ed37677), ADR-8012.
// Mark: as-is for the gate values, `integrate_pose`'s loop and `flat_shot_impact_ms` (the bytes of the
// arithmetic are DO27's). Named changes: the hover step and its default parameters are the game's
// (plan row S-07), so `integrate_pose` drives any `RateModel` instead of `hover::step_physics` with
// `HoverPhysicsParams::default()`; the gates are a `RateGates` setting with DO27's values as the
// default; `measure` and `RateGateReport::failures` let a host run the same gates on its own model.

//! 60 Hz vs 120 Hz agreement gates (ADR-8046).
//!
//! The same input stream at both rates; position, heading and flat-shot time-to-impact must agree
//! within the gates. A model is not retuned to force a pass. [`measure`] runs the four checks on a
//! [`RateModel`]; the engine's [`Vehicle`](super::scenarios::Vehicle) passes them in the test
//! `rate_60_vs_120_position_heading_and_flat_shot`, and a game runs them on its own movement model.

use crate::float::det_max;
use crate::math;

/// A movement model the rate gates can drive: reset to rest, step with an input at a `dt`, and read
/// back the planar velocity and heading.
pub trait RateModel {
    /// Back to the start state (at rest, heading 0).
    fn reset(&mut self);
    /// Advance by `dt` with `input = [throttle, strafe, steer]`.
    fn step(&mut self, input: [f32; 3], dt: f32);
    /// Velocity `[x, z]` after the last step.
    fn velocity_xz(&self) -> [f32; 2];
    /// Heading (rad) after the last step.
    fn yaw(&self) -> f32;
}

/// Run constant `input` for `duration_s` seconds at `hz` from rest. Position integrates
/// `velocity * dt` after each step. Returns `(x, z, yaw, distance)`.
pub fn integrate_pose(
    model: &mut dyn RateModel,
    hz: u32,
    duration_s: f32,
    input: [f32; 3],
) -> (f32, f32, f32, f32) {
    let dt = 1.0 / hz as f32;
    model.reset();
    let mut x = 0.0f32;
    let mut z = 0.0f32;
    let ticks = (duration_s * hz as f32).round() as usize;
    for _ in 0..ticks {
        model.step(input, dt);
        let v = model.velocity_xz();
        x += v[0] * dt;
        z += v[1] * dt;
    }
    (x, z, model.yaw(), math::sqrt(x * x + z * z))
}

/// Flat bolt: constant horizontal speed, impact when `x >= range`; capped at 10 s.
pub fn flat_shot_impact_ms(hz: u32, speed_per_sec: f32, range: f32) -> f32 {
    let dt = 1.0 / hz as f32;
    let mut x = 0.0f32;
    let mut t_ms = 0.0f32;
    let step = speed_per_sec * dt;
    // Cap at 10 s wall time.
    let max_ticks = hz * 10;
    for _ in 0..max_ticks {
        x += step;
        t_ms += dt * 1000.0;
        if x >= range {
            return t_ms;
        }
    }
    t_ms
}

/// The gate limits. [`RateGates::default`] is the published set the test uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateGates {
    /// Horizontal position error after 1 s of full throttle (m).
    pub position_1s_m: f32,
    /// Horizontal position error after 5 s of full throttle (m).
    pub position_5s_m: f32,
    /// Heading error after 2 s of full steer at rest (degrees).
    pub heading_2s_deg: f32,
    /// Flat-shot muzzle speed (units/s).
    pub shot_speed: f32,
    /// Flat-shot range (units).
    pub shot_range: f32,
    /// Floor of the flat-shot gate (ms); the gate is `max(floor, one 60 Hz tick)`.
    pub shot_floor_ms: f32,
}

impl Default for RateGates {
    fn default() -> Self {
        RateGates {
            position_1s_m: 0.25,
            position_5s_m: 1.0,
            heading_2s_deg: 2.0,
            shot_speed: 160.0,
            shot_range: 80.0,
            shot_floor_ms: 16.0,
        }
    }
}

/// What [`measure`] found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateGateReport {
    /// Position error after 1 s (m).
    pub position_1s_err_m: f32,
    /// Position error after 5 s (m).
    pub position_5s_err_m: f32,
    /// Heading error after 2 s (degrees).
    pub heading_2s_err_deg: f32,
    /// Flat-shot time to impact at 60 Hz (ms).
    pub shot_60_ms: f32,
    /// Flat-shot time to impact at 120 Hz (ms).
    pub shot_120_ms: f32,
    /// The flat-shot gate (ms).
    pub shot_gate_ms: f32,
}

impl RateGateReport {
    /// One line per gate that failed (empty if all passed).
    pub fn failures(&self, gates: &RateGates) -> Vec<String> {
        let mut out = Vec::new();
        if self.position_1s_err_m > gates.position_1s_m {
            out.push(format!(
                "position gate @ 1 s: horizontal error {} m > {}",
                self.position_1s_err_m, gates.position_1s_m
            ));
        }
        if self.position_5s_err_m > gates.position_5s_m {
            out.push(format!(
                "position gate @ 5 s: horizontal error {} m > {}",
                self.position_5s_err_m, gates.position_5s_m
            ));
        }
        if self.heading_2s_err_deg > gates.heading_2s_deg {
            out.push(format!(
                "heading gate @ 2 s: |dyaw|={} deg > {}",
                self.heading_2s_err_deg, gates.heading_2s_deg
            ));
        }
        let delta = (self.shot_60_ms - self.shot_120_ms).abs();
        if delta > self.shot_gate_ms {
            out.push(format!(
                "flat-shot gate: |dt|={delta} ms > {} (60={} 120={})",
                self.shot_gate_ms, self.shot_60_ms, self.shot_120_ms
            ));
        }
        out
    }
}

/// Run the four checks on `model` (it is reset before each run).
pub fn measure(model: &mut dyn RateModel, gates: &RateGates) -> RateGateReport {
    let err = |a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)| {
        let dx = a.0 - b.0;
        let dz = a.1 - b.1;
        math::sqrt(dx * dx + dz * dz)
    };
    let throttle = [1.0, 0.0, 0.0];
    let p60_1 = integrate_pose(model, 60, 1.0, throttle);
    let p120_1 = integrate_pose(model, 120, 1.0, throttle);
    let p60_5 = integrate_pose(model, 60, 5.0, throttle);
    let p120_5 = integrate_pose(model, 120, 5.0, throttle);
    let steer = [0.0, 0.0, 1.0];
    let (_, _, yaw60, _) = integrate_pose(model, 60, 2.0, steer);
    let (_, _, yaw120, _) = integrate_pose(model, 120, 2.0, steer);
    let dt60_ms: f32 = 1000.0 / 60.0;
    RateGateReport {
        position_1s_err_m: err(p60_1, p120_1),
        position_5s_err_m: err(p60_5, p120_5),
        heading_2s_err_deg: (yaw60 - yaw120).abs() * 180.0 / core::f32::consts::PI,
        shot_60_ms: flat_shot_impact_ms(60, gates.shot_speed, gates.shot_range),
        shot_120_ms: flat_shot_impact_ms(120, gates.shot_speed, gates.shot_range),
        shot_gate_ms: det_max(dt60_ms, gates.shot_floor_ms),
    }
}
