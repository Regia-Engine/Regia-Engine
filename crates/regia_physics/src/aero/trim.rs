// SPDX-License-Identifier: MPL-2.0
//! Offline trim: the pitch attitude, elevator and throttle (or flight-path angle) that make an
//! aircraft's forces and pitching moment vanish in steady straight flight (ADR-3374).
//!
//! The state is wings level with no sideslip and no rotation, flying along world −Z at `speed`,
//! climbing at flight-path angle γ (`velocity = V·(0, sin γ, −cos γ)`), with the body pitched up by θ
//! about +X. The unknowns are θ, the pitch input and either the throttle (level flight, γ = 0) or γ
//! (a fixed throttle, e.g. 0 for a glide). The residual is the vertical force, the horizontal force
//! (both over the weight) and the pitching moment (over weight × √S_ref) from [`evaluate_loads`]
//! plus gravity; Newton's method with a forward-difference Jacobian solves it. Thrust is linear in
//! throttle, so the solve reads it as `loads(0) + u·(loads(1) − loads(0))` and may leave 0..1 while it
//! iterates; a final value outside the input ranges is [`AeroError::TrimUnreachable`].

use regia_core::math::{Quat, Vec3};

use super::{
    AeroEnvironment, AeroError, AircraftConfig, AircraftInput, Kinematics, SurfaceState, evaluate_loads,
};

/// Newton iterations before [`AeroError::TrimFailed`].
pub const TRIM_MAX_ITERATIONS: u32 = 60;
/// Largest normalized residual a trim accepts.
pub const TRIM_TOLERANCE: f32 = 2.0e-5;

/// What to trim for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrimTarget {
    /// True airspeed, m/s (1..=1000).
    pub speed: f32,
    /// Altitude above sea level, meters (the ISA density's input).
    pub altitude: f32,
    /// `None`: level flight, solve the throttle. `Some(t)`: hold throttle `t` (0..=1) and solve the
    /// flight-path angle (a glide at 0).
    pub throttle: Option<f32>,
    /// Flap setting held during the solve, 0..=1.
    pub flaps: f32,
    /// Gravity's magnitude, m/s² (the physics world's default is 9.81).
    pub gravity: f32,
}

impl TrimTarget {
    /// Level flight at `speed` and `altitude` under 9.81 m/s².
    #[must_use]
    pub fn level(speed: f32, altitude: f32) -> Self {
        Self {
            speed,
            altitude,
            throttle: None,
            flaps: 0.0,
            gravity: 9.81,
        }
    }

    /// A glide (throttle 0) at `speed` and `altitude` under 9.81 m/s².
    #[must_use]
    pub fn glide(speed: f32, altitude: f32) -> Self {
        Self {
            throttle: Some(0.0),
            ..Self::level(speed, altitude)
        }
    }
}

/// A trimmed flight condition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trim {
    /// Pitch attitude θ, radians (+ nose up).
    pub pitch_attitude: f32,
    /// Angle of attack θ − γ, radians.
    pub alpha: f32,
    /// Flight-path angle γ, radians (+ climbing).
    pub flight_path: f32,
    /// The pitch input that holds the moment at zero.
    pub pitch_input: f32,
    /// The throttle (solved for level flight, the target's otherwise).
    pub throttle: f32,
    /// The flap setting held.
    pub flaps: f32,
    /// Aircraft lift coefficient at the trim.
    pub cl: f32,
    /// Aircraft drag coefficient at the trim.
    pub cd: f32,
    /// Some wing surface is past its stall angle at the trim.
    pub stalled: bool,
    /// Newton iterations used.
    pub iterations: u32,
    /// Normalized residual at the solution.
    pub residual: f32,
    /// True airspeed, m/s.
    pub speed: f32,
}

impl Trim {
    /// The body rotation of the trim (heading −Z).
    #[must_use]
    pub fn rotation(&self) -> Quat {
        Quat::from_rotation_x(self.pitch_attitude)
    }

    /// The world velocity of the trim.
    #[must_use]
    pub fn velocity(&self) -> Vec3 {
        let (sin, cos) = self.flight_path.sin_cos();
        Vec3::new(0.0, sin, -cos) * self.speed
    }

    /// The pilot input that holds the trim.
    #[must_use]
    pub fn input(&self) -> AircraftInput {
        AircraftInput {
            pitch: self.pitch_input,
            throttle: self.throttle,
            flaps: self.flaps,
            ..AircraftInput::default()
        }
    }
}

struct Problem<'a> {
    config: &'a AircraftConfig,
    target: TrimTarget,
    surfaces: Vec<SurfaceState>,
    thrusts: Vec<f32>,
    weight: f32,
    moment_scale: f32,
}

/// The loads at one guess: residual vector, plus CL, CD and the stall flag.
struct Eval {
    residual: [f64; 3],
    cl: f32,
    cd: f32,
    stalled: bool,
}

impl Problem<'_> {
    /// `x = [θ, pitch input, throttle or γ]`.
    fn eval(&mut self, x: [f64; 3]) -> Eval {
        let (theta, pitch) = (x[0] as f32, x[1] as f32);
        let (throttle, gamma) = match self.target.throttle {
            None => (x[2] as f32, 0.0),
            Some(t) => (t, x[2] as f32),
        };
        let (sin, cos) = gamma.sin_cos();
        let kin = Kinematics {
            center_of_mass: Vec3::new(0.0, self.target.altitude, 0.0),
            rotation: Quat::from_rotation_x(theta),
            linear_velocity: Vec3::new(0.0, sin, -cos) * self.target.speed,
            angular_velocity: Vec3::ZERO,
        };
        let env = AeroEnvironment::default();
        let base = AircraftInput {
            pitch,
            flaps: self.target.flaps,
            ..AircraftInput::default()
        };
        let idle = evaluate_loads(
            self.config,
            &kin,
            &base,
            &env,
            &mut self.surfaces,
            &mut self.thrusts,
        );
        let full_input = AircraftInput {
            throttle: 1.0,
            ..base
        };
        let full = evaluate_loads(
            self.config,
            &kin,
            &full_input,
            &env,
            &mut self.surfaces,
            &mut self.thrusts,
        );
        let force = idle.aero_force
            + (full.thrust_force - idle.thrust_force) * throttle
            + Vec3::new(0.0, -self.weight, 0.0);
        let torque = idle.torque + (full.torque - idle.torque) * throttle;
        Eval {
            residual: [
                f64::from(force.y / self.weight),
                f64::from(force.z / self.weight),
                f64::from(torque.x / self.moment_scale),
            ],
            cl: idle.cl,
            cd: idle.cd,
            stalled: idle.stalled,
        }
    }
}

fn norm(r: &[f64; 3]) -> f64 {
    r.iter().fold(0.0_f64, |m, v| m.max(v.abs()))
}

/// The determinant of a 3×3 matrix (rows).
fn det3(m: &[[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// Solves `a · x = b` for a 3×3 system by Cramer's rule; `None` when (nearly) singular.
fn solve3(a: [[f64; 3]; 3], b: [f64; 3]) -> Option<[f64; 3]> {
    let det = det3(&a);
    let scale = a.iter().flatten().fold(0.0_f64, |m, v| m.max(v.abs()));
    if det.is_nan() || det.abs() <= 1.0e-12 * scale * scale * scale {
        return None;
    }
    let column = |k: usize| {
        let mut m = a;
        for (row, value) in m.iter_mut().zip(b) {
            row[k] = value;
        }
        det3(&m) / det
    };
    let x = [column(0), column(1), column(2)];
    x.iter().all(|v| v.is_finite()).then_some(x)
}

/// Trims `config` for `target`.
///
/// # Errors
/// An invalid configuration; [`AeroError::TrimInvalidTarget`] for a bad target;
/// [`AeroError::TrimFailed`] when Newton does not converge from any start;
/// [`AeroError::TrimUnreachable`] when the solution needs a pitch input outside ±1 or a throttle outside
/// 0..1.
pub fn solve(config: &AircraftConfig, target: &TrimTarget) -> Result<Trim, AeroError> {
    config.validate()?;
    let range = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
    if !range(target.speed, 1.0, 1000.0) {
        return Err(AeroError::TrimInvalidTarget { field: "speed" });
    }
    if !range(target.altitude, super::ISA_FLOOR, super::ISA_CEILING) {
        return Err(AeroError::TrimInvalidTarget { field: "altitude" });
    }
    if target.throttle.is_some_and(|t| !range(t, 0.0, 1.0)) {
        return Err(AeroError::TrimInvalidTarget { field: "throttle" });
    }
    if !range(target.flaps, 0.0, 1.0) {
        return Err(AeroError::TrimInvalidTarget { field: "flaps" });
    }
    if !range(target.gravity, 0.01, 100.0) {
        return Err(AeroError::TrimInvalidTarget { field: "gravity" });
    }
    let weight = config.mass * target.gravity;
    let mut problem = Problem {
        config,
        target: *target,
        surfaces: vec![SurfaceState::default(); config.surfaces.len()],
        thrusts: vec![0.0; config.engines.len()],
        weight,
        moment_scale: weight * config.reference_area().sqrt(),
    };
    let third = if target.throttle.is_some() { -0.05 } else { 0.5 };
    let mut best: Option<([f64; 3], Eval, u32)> = None;
    for theta in [0.05, 0.15, -0.05, 0.25] {
        let (x, eval, iterations) = newton(&mut problem, [theta, 0.0, third]);
        if norm(&eval.residual) <= f64::from(TRIM_TOLERANCE) {
            best = Some((x, eval, iterations));
            break;
        }
        if best
            .as_ref()
            .is_none_or(|(_, b, _)| norm(&eval.residual) < norm(&b.residual))
        {
            best = Some((x, eval, iterations));
        }
    }
    let Some((x, eval, iterations)) = best else {
        return Err(AeroError::TrimFailed { residual: f32::NAN });
    };
    let residual = norm(&eval.residual) as f32;
    if residual.is_nan() || residual > TRIM_TOLERANCE {
        return Err(AeroError::TrimFailed { residual });
    }
    let pitch_input = x[1] as f32;
    let (throttle, flight_path) = match target.throttle {
        None => (x[2] as f32, 0.0),
        Some(t) => (t, x[2] as f32),
    };
    if !(-1.0..=1.0).contains(&pitch_input) {
        return Err(AeroError::TrimUnreachable {
            control: "pitch",
            value: pitch_input,
        });
    }
    if !(0.0..=1.0).contains(&throttle) {
        return Err(AeroError::TrimUnreachable {
            control: "throttle",
            value: throttle,
        });
    }
    let pitch_attitude = x[0] as f32;
    Ok(Trim {
        pitch_attitude,
        alpha: pitch_attitude - flight_path,
        flight_path,
        pitch_input,
        throttle,
        flaps: target.flaps,
        cl: eval.cl,
        cd: eval.cd,
        stalled: eval.stalled,
        iterations,
        residual,
        speed: target.speed,
    })
}

/// Newton from `x`; returns the last point, its evaluation and the iterations used.
fn newton(problem: &mut Problem<'_>, mut x: [f64; 3]) -> ([f64; 3], Eval, u32) {
    const STEP: [f64; 3] = [1.0e-3, 1.0e-3, 1.0e-3];
    const LIMIT: [f64; 3] = [0.1, 0.5, 0.2];
    let mut eval = problem.eval(x);
    for iteration in 1..=TRIM_MAX_ITERATIONS {
        if norm(&eval.residual) <= f64::from(TRIM_TOLERANCE) {
            return (x, eval, iteration - 1);
        }
        let mut jacobian = [[0.0; 3]; 3];
        for (k, step) in STEP.iter().enumerate() {
            let mut probe = x;
            probe[k] += step;
            let shifted = problem.eval(probe);
            for (row, values) in jacobian.iter_mut().enumerate() {
                values[k] = (shifted.residual[row] - eval.residual[row]) / step;
            }
        }
        let rhs = eval.residual.map(|r| -r);
        let Some(delta) = solve3(jacobian, rhs) else {
            return (x, eval, iteration);
        };
        // Scale the step so no unknown moves more than its limit.
        let scale = delta
            .iter()
            .zip(LIMIT)
            .fold(1.0_f64, |s, (d, limit)| s.min(limit / d.abs().max(1.0e-12)));
        for (value, d) in x.iter_mut().zip(delta) {
            *value += d * scale;
        }
        eval = problem.eval(x);
    }
    (x, eval, TRIM_MAX_ITERATIONS)
}

/// The aircraft's lift and drag coefficients at body angle of attack `alpha` (radians) with every
/// control neutral, no thrust, at sea level: its own polar, for glide checks (`L/D = CL/CD`).
#[must_use]
pub fn polar(config: &AircraftConfig, alpha: f32) -> (f32, f32) {
    let mut surfaces = vec![SurfaceState::default(); config.surfaces.len()];
    let mut thrusts = vec![0.0; config.engines.len()];
    let kin = Kinematics {
        center_of_mass: Vec3::ZERO,
        rotation: Quat::from_rotation_x(alpha),
        linear_velocity: Vec3::new(0.0, 0.0, -50.0),
        angular_velocity: Vec3::ZERO,
    };
    let loads = evaluate_loads(
        config,
        &kin,
        &AircraftInput::default(),
        &AeroEnvironment::default(),
        &mut surfaces,
        &mut thrusts,
    );
    (loads.cl, loads.cd)
}

/// The largest `CL/CD` of [`polar`] over −5°..=20° in 0.1° steps, and the angle of attack (radians)
/// where it occurs.
#[must_use]
pub fn max_lift_to_drag(config: &AircraftConfig) -> (f32, f32) {
    (0..=250)
        .map(|i| (-5.0 + i as f32 * 0.1).to_radians())
        .map(|alpha| {
            let (cl, cd) = polar(config, alpha);
            (if cd > 0.0 { cl / cd } else { 0.0 }, alpha)
        })
        .fold(
            (f32::MIN, 0.0),
            |best, item| if item.0 > best.0 { item } else { best },
        )
}

/// The aircraft's lift and drag coefficients at body angle of attack `alpha` (radians) with the pitch
/// input that zeroes the pitching moment (no thrust, sea level, other controls neutral): its trimmed
/// polar, the curve a steady glide flies on. Also the pitch input; `None` when no input within ±1.5
/// trims the moment.
#[must_use]
pub fn trimmed_polar(config: &AircraftConfig, alpha: f32) -> Option<(f32, f32, f32)> {
    let mut surfaces = vec![SurfaceState::default(); config.surfaces.len()];
    let mut thrusts = vec![0.0; config.engines.len()];
    let kin = Kinematics {
        center_of_mass: Vec3::ZERO,
        rotation: Quat::from_rotation_x(alpha),
        linear_velocity: Vec3::new(0.0, 0.0, -50.0),
        angular_velocity: Vec3::ZERO,
    };
    let env = AeroEnvironment::default();
    let mut eval = |pitch: f32| {
        let input = AircraftInput {
            pitch,
            ..AircraftInput::default()
        };
        evaluate_loads(config, &kin, &input, &env, &mut surfaces, &mut thrusts)
    };
    let mut pitch = 0.0_f32;
    for _ in 0..30 {
        let loads = eval(pitch);
        let moment = loads.torque.x;
        let slope = (eval(pitch + 1.0e-3).torque.x - moment) / 1.0e-3;
        if moment.abs() < 1.0e-3 * config.mass {
            return Some((loads.cl, loads.cd, pitch));
        }
        if slope.abs() < 1.0e-6 || !slope.is_finite() {
            return None;
        }
        pitch = (pitch - moment / slope).clamp(-1.5, 1.5);
    }
    None
}

/// The largest `CL/CD` of [`trimmed_polar`] over −5°..=20° in 0.1° steps, and the angle of attack
/// (radians) where it occurs.
#[must_use]
pub fn max_trimmed_lift_to_drag(config: &AircraftConfig) -> (f32, f32) {
    (0..=250)
        .map(|i| (-5.0 + i as f32 * 0.1).to_radians())
        .filter_map(|alpha| {
            let (cl, cd, pitch) = trimmed_polar(config, alpha)?;
            (cd > 0.0 && pitch.abs() <= 1.0).then_some((cl / cd, alpha))
        })
        .fold((0.0, 0.0), |best, item| if item.0 > best.0 { item } else { best })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aero::trainer;

    #[test]
    fn the_trainer_trims_for_level_flight() {
        let trim = solve(&trainer(), &TrimTarget::level(50.0, 1000.0)).expect("trim");
        assert!(trim.residual <= TRIM_TOLERANCE);
        assert!(trim.throttle > 0.0 && trim.throttle < 1.0, "{trim:?}");
        assert!(trim.pitch_input.abs() < 1.0, "{trim:?}");
        assert!(trim.alpha > -0.1 && trim.alpha < 0.2, "{trim:?}");
        assert!(!trim.stalled);
        assert_eq!(trim.flight_path, 0.0);
    }

    #[test]
    fn a_glide_descends() {
        let trim = solve(&trainer(), &TrimTarget::glide(35.0, 1000.0)).expect("glide");
        assert!(trim.flight_path < 0.0, "{trim:?}");
        assert_eq!(trim.throttle, 0.0);
    }

    #[test]
    fn bad_targets_are_named() {
        let code = |t: TrimTarget| solve(&trainer(), &t).map_err(|e| e.code());
        assert_eq!(
            code(TrimTarget::level(f32::NAN, 0.0)),
            Err("aircraft_trim_invalid_target")
        );
        assert_eq!(
            code(TrimTarget::level(50.0, 1.0e6)),
            Err("aircraft_trim_invalid_target")
        );
        // Far too slow to hold level flight: the elevator or the wing runs out.
        assert!(matches!(
            code(TrimTarget::level(8.0, 0.0)),
            Err("aircraft_trim_unreachable" | "aircraft_trim_failed")
        ));
    }

    #[test]
    fn solve3_solves() {
        let a = [[2.0, 1.0, 0.0], [1.0, 3.0, 1.0], [0.0, 1.0, 4.0]];
        let x = solve3(a, [3.0, 5.0, 5.0]).expect("solve");
        for (got, want) in x.iter().zip([1.0, 1.0, 1.0]) {
            assert!((got - want).abs() < 1.0e-12);
        }
        assert!(solve3([[0.0; 3]; 3], [1.0; 3]).is_none());
    }

    #[test]
    fn the_trimmed_polar_costs_trim_drag() {
        let (plain, _) = max_lift_to_drag(&trainer());
        let (trimmed, alpha) = max_trimmed_lift_to_drag(&trainer());
        assert!(trimmed > 6.0 && trimmed <= plain + 1.0e-3, "{trimmed} vs {plain}");
        let (_, _, pitch) = trimmed_polar(&trainer(), alpha).expect("trims");
        assert!(pitch.abs() <= 1.0);
    }

    #[test]
    fn the_polar_has_a_sensible_best_glide() {
        let (ratio, alpha) = max_lift_to_drag(&trainer());
        assert!(ratio > 6.0 && ratio < 20.0, "{ratio}");
        assert!(alpha > 0.0 && alpha < 0.2, "{alpha}");
    }
}
