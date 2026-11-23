// SPDX-License-Identifier: MPL-2.0
// Source: original derivation: wheel-normal geometry gives R = L / tan(steer); balancing
// gravity and centripetal acceleration gives tan(lean) = v² / (R g). The first-order tracker,
// low-speed spring and fall rule are original design, not a physical roll-stability solver.
// Gillespie, Fundamentals of Vehicle Dynamics (SAE 1992, doi:10.4271/R-114), is background
// reading; its publisher record and chapter 6 title were verified, but its text was not read.
// No third-party code.

//! Two-wheel balance: lean, steer, speed and two wheel contacts (ADR-4017).
//!
//! This model does not drive the car solver and does not read a chassis body. The caller passes
//! the steer, the speed, the drive command and the two wheel-ray lengths each step. Wheel axles
//! lie on the chassis +X, forward is local −Z and up is +Y, the same axes as the wheeled chassis.
//! Positive steer and positive lean both point toward +X (a right turn).
//!
//! # Closed form
//!
//! For a no-slip single-track turn the radius about the rear contact is `R = L / tan(δ)` with `L`
//! the wheelbase and `δ` the steer, so the lateral acceleration is `v² · tan(δ) / L`. The lean that
//! balances that acceleration against gravity is
//!
//! ```text
//! φ = atan(v² · tan(δ) / (L · g))
//! ```
//!
//! when `|v|` is at least [`BikeConfig::upright_speed`] (2 m/s on the default configuration). Below
//! that speed the target lean is 0: a spring pulls the lean back upright and the `v²` term is not
//! applied. For a small steer, `tan(δ) ≈ δ`, so the acceleration term is `v² · steer / wheelbase`.
//!
//! # Lean rate
//!
//! Above the speed threshold the lean rate tracks the closed form,
//! `clamp(response · (φ − lean), ±max_lean_rate)`, integrated exactly while the rate is inside the
//! clamp and at the clamped rate otherwise. [`BikeConfig::lean_response`] is 4/s and
//! [`BikeConfig::max_lean_rate`] is 2.5 rad/s on the default configuration. Below the threshold the
//! same clamp applies to `−upright_rate · lean` ([`BikeConfig::upright_rate`], 4/s).
//!
//! # Fall
//!
//! A lean past [`BikeConfig::max_lean`] sets [`BikeState::fallen`] and stores drive `0`. This is
//! checked both before and after integration, so an over-limit initial lean cannot spring upright. Later steps
//! keep the lean and the fallen flag and keep the drive at 0. Steer, speed and the contact flags
//! still take the caller's new values.
//!
//! # Rays
//!
//! A finite ray length in `0..=contact_reach` meters is a wheel in contact. A longer finite ray, or
//! `+∞` (the ray missed), is not in contact. The flags are recorded for the caller; the lean
//! equation does not read them. A NaN ray or a negative length is [`BikeError::BadInput`] and does
//! not change the state.
//!
//! Angles are radians, distances meters, speeds m/s. [`bike_step`] is deterministic: the same
//! configuration, state and inputs produce the same state and the same [`BikeState::digest`].

use regia_core::math::{det_atan, det_exp, det_tan};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug_span;

/// Fixed step used by the acceptance checks and the bike lab, in seconds (1/60).
pub const FIXED_STEP_S: f32 = 1.0 / 60.0;

/// Largest accepted steer, radians. Past this `tan` leaves the range the kinematic bicycle is
/// written for.
pub const MAX_STEER_RAD: f32 = 1.2;

/// Largest accepted speed, m/s.
pub const MAX_SPEED_MPS: f32 = 150.0;

/// Largest accepted time step, seconds. One default response step stays inside a single time
/// constant.
pub const MAX_STEP_S: f32 = 0.25;

/// Default wheelbase, meters.
pub const DEFAULT_WHEELBASE_M: f32 = 1.5;

/// Default gravity, m/s².
pub const DEFAULT_GRAVITY_MPS2: f32 = 9.81;

/// Default speed below which the lean springs back to 0, m/s.
pub const DEFAULT_UPRIGHT_SPEED_MPS: f32 = 2.0;

/// Why [`bike_step`] refused a step. [`BikeError::code`] is the stable protocol identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BikeError {
    /// A steer, speed, drive, time step, ray or configuration field is not finite or is outside
    /// the documented range. The state is unchanged.
    #[error("bike input or config is not finite or is outside the documented range")]
    BadInput,
}

impl BikeError {
    /// Stable identifier: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::BadInput => "bike_bad_input",
        }
    }
}

/// Lengths and rates for one bike. The default is the configuration the book page documents.
///
/// The serde shape is the `config` block of a `.bike.ron` ([`asset`]): named fields, each
/// optional (a missing one takes [`BikeConfig::default`]'s value), an unknown name refused.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BikeConfig {
    /// Distance between the wheel contacts, meters (`L` in the closed form).
    pub wheelbase: f32,
    /// Gravity, m/s² (`g` in the closed form).
    pub gravity: f32,
    /// A lean past this, radians, is a fall.
    pub max_lean: f32,
    /// Clamp on the absolute lean rate, rad/s.
    pub max_lean_rate: f32,
    /// Below this speed, m/s, the lean springs toward 0 and the steer term is off.
    pub upright_speed: f32,
    /// Spring rate below [`Self::upright_speed`], 1/s.
    pub upright_rate: f32,
    /// How fast the lean tracks the closed form at or above [`Self::upright_speed`], 1/s.
    pub lean_response: f32,
    /// A ray at or under this length, meters, counts as that wheel in contact.
    pub contact_reach: f32,
}

impl Default for BikeConfig {
    fn default() -> Self {
        Self {
            wheelbase: DEFAULT_WHEELBASE_M,
            gravity: DEFAULT_GRAVITY_MPS2,
            max_lean: 1.05,
            max_lean_rate: 2.5,
            upright_speed: DEFAULT_UPRIGHT_SPEED_MPS,
            upright_rate: 4.0,
            lean_response: 4.0,
            contact_reach: 0.7,
        }
    }
}

/// Lean, the rider's commands and the two contact flags.
///
/// The default is upright: lean, steer, speed and drive are 0, both wheels are out of contact
/// and the bike has not fallen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BikeState {
    /// Lean about the forward axis, radians. Positive leans toward +X.
    pub lean: f32,
    /// Front steer, radians. Positive steers toward +X.
    pub steer: f32,
    /// Travel speed, m/s. The lean law uses `speed²`, so reverse matches forward.
    pub speed: f32,
    /// Drive command as the caller passed it, or 0 once the bike has fallen.
    pub drive: f32,
    /// The front wheel ray is within [`BikeConfig::contact_reach`].
    pub front_contact: bool,
    /// The rear wheel ray is within [`BikeConfig::contact_reach`].
    pub rear_contact: bool,
    /// Set when the lean passes [`BikeConfig::max_lean`]. Stays set.
    pub fallen: bool,
}

impl BikeState {
    /// FNV-1a 64 over the lean, steer, speed and drive bits (little-endian) and the three flags.
    ///
    /// Equal states produce equal digests. Lean is integrated with the deterministic `det_tan`,
    /// `det_atan` and `det_exp` kernels, so the bits do not follow the platform libm (spec 15 § 5).
    #[must_use]
    pub fn digest(self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for bits in [
            self.lean.to_bits(),
            self.steer.to_bits(),
            self.speed.to_bits(),
            self.drive.to_bits(),
        ] {
            for byte in bits.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        for flag in [self.front_contact, self.rear_contact, self.fallen] {
            hash ^= u64::from(flag);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}

/// One step's commands and the two ray lengths, meters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BikeInput {
    /// Time step, seconds. Finite, in `(0, MAX_STEP_S]`.
    pub dt: f32,
    /// Front steer, radians. Finite, in `[-MAX_STEER_RAD, MAX_STEER_RAD]`.
    pub steer: f32,
    /// Travel speed, m/s. Finite, in `[-MAX_SPEED_MPS, MAX_SPEED_MPS]`.
    pub speed: f32,
    /// Drive command. Finite. Stored as given until a fall stores 0.
    pub drive: f32,
    /// Front wheel ray length, meters. `+∞` means the ray missed.
    pub front_ray: f32,
    /// Rear wheel ray length, meters. `+∞` means the ray missed.
    pub rear_ray: f32,
}

/// Steady lean, radians, for a constant `speed` and `steer` on `config`.
///
/// This is the closed form above: `atan(v² · tan(steer) / (wheelbase · g))` when `|speed|` is at
/// least `config.upright_speed`, and `0` below that. A non-finite argument or an unusable
/// configuration returns `0` (it is not an error; [`bike_step`] refuses those inputs).
#[must_use]
pub fn steady_lean(speed: f32, steer: f32, config: &BikeConfig) -> f32 {
    if !config_ok(config) || !speed.is_finite() || !steer.is_finite() || speed.abs() < config.upright_speed {
        return 0.0;
    }
    let lateral = speed * speed * det_tan(steer) / config.wheelbase;
    let lean = det_atan(lateral / config.gravity);
    if lean.is_finite() { lean } else { 0.0 }
}

/// Advance `state` by one step.
///
/// On [`BikeError::BadInput`] `state` is left as it was. The span `bike_step` covers the step,
/// including a refused one.
///
/// # Errors
///
/// [`BikeError::BadInput`] when the configuration or the input is not finite or is outside the
/// ranges documented on [`BikeConfig`] and [`BikeInput`].
pub fn bike_step(state: &mut BikeState, config: &BikeConfig, input: &BikeInput) -> Result<(), BikeError> {
    let _span = debug_span!("bike_step").entered();
    if !state.lean.is_finite() || !config_ok(config) || !input_ok(input) {
        return Err(BikeError::BadInput);
    }
    let front = wheel_in_contact(input.front_ray, config.contact_reach)?;
    let rear = wheel_in_contact(input.rear_ray, config.contact_reach)?;

    state.steer = input.steer;
    state.speed = input.speed;
    state.front_contact = front;
    state.rear_contact = rear;
    if state.fallen || state.lean.abs() > config.max_lean {
        state.fallen = true;
        state.drive = 0.0;
        return Ok(());
    }
    state.drive = input.drive;

    let below = input.speed.abs() < config.upright_speed;
    let target = if below {
        0.0
    } else {
        steady_lean(input.speed, input.steer, config)
    };
    let response = if below {
        config.upright_rate
    } else {
        config.lean_response
    };
    let raw = response * (target - state.lean);
    if raw.abs() <= config.max_lean_rate {
        let decay = det_exp(-response * input.dt);
        state.lean = target + (state.lean - target) * decay;
    } else {
        let rate = raw.clamp(-config.max_lean_rate, config.max_lean_rate);
        // A long step moves the clamped rate farther than the remaining distance; step to
        // the target instead of swinging past it, which would oscillate across it forever
        // (a stiff response on a long dt used to never settle).
        let step = rate * input.dt;
        let remaining = target - state.lean;
        state.lean += if step.abs() >= remaining.abs() {
            remaining
        } else {
            step
        };
    }
    if state.lean.abs() > config.max_lean {
        state.fallen = true;
        state.drive = 0.0;
    }
    Ok(())
}

impl BikeConfig {
    /// Each field's name and inclusive valid range `[min, max]`, in declaration order. A value
    /// outside its range, or one that is not finite, is [`BikeError::BadInput`].
    pub const FIELD_RANGES: [(&'static str, [f32; 2]); 8] = [
        ("wheelbase", [0.2, 10.0]),
        ("gravity", [0.1, 30.0]),
        ("max_lean", [0.05, 3.15]),
        ("max_lean_rate", [0.1, 20.0]),
        ("upright_speed", [0.0, MAX_SPEED_MPS]),
        ("upright_rate", [0.1, 40.0]),
        ("lean_response", [0.1, 40.0]),
        ("contact_reach", [0.0, 5.0]),
    ];

    /// The field values in [`Self::FIELD_RANGES`] order.
    fn values(&self) -> [f32; 8] {
        [
            self.wheelbase,
            self.gravity,
            self.max_lean,
            self.max_lean_rate,
            self.upright_speed,
            self.upright_rate,
            self.lean_response,
            self.contact_reach,
        ]
    }

    /// The first field, in [`Self::FIELD_RANGES`] order, that is not finite or is outside its
    /// range: its name, its value and its range. `None` for a configuration [`bike_step`] accepts.
    #[must_use]
    pub fn first_bad_field(&self) -> Option<(&'static str, f32, [f32; 2])> {
        Self::FIELD_RANGES
            .iter()
            .zip(self.values())
            .find(|((_, [min, max]), value)| !in_range(*value, *min, *max))
            .map(|((name, range), value)| (*name, value, *range))
    }
}

fn config_ok(config: &BikeConfig) -> bool {
    config.first_bad_field().is_none()
}

fn input_ok(input: &BikeInput) -> bool {
    input.dt.is_finite()
        && input.dt > 0.0
        && input.dt <= MAX_STEP_S
        && in_range(input.steer, -MAX_STEER_RAD, MAX_STEER_RAD)
        && in_range(input.speed, -MAX_SPEED_MPS, MAX_SPEED_MPS)
        && input.drive.is_finite()
}

fn in_range(value: f32, min: f32, max: f32) -> bool {
    value.is_finite() && (min..=max).contains(&value)
}

fn wheel_in_contact(ray: f32, reach: f32) -> Result<bool, BikeError> {
    // +∞ is a ray that did not hit. NaN and any negative length, including −∞, are refused.
    if ray.is_infinite() && ray.is_sign_positive() {
        return Ok(false);
    }
    if !ray.is_finite() || ray < 0.0 {
        return Err(BikeError::BadInput);
    }
    Ok(ray <= reach)
}

// `.bike.ron` v1 (ADR-8630): a named `BikeConfig`. See `docs/formats/bike-ron.md`.
pub mod asset;

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BikeConfig {
        BikeConfig::default()
    }

    fn input(steer: f32, speed: f32) -> BikeInput {
        BikeInput {
            dt: FIXED_STEP_S,
            steer,
            speed,
            drive: 1.0,
            front_ray: 0.3,
            rear_ray: 0.3,
        }
    }

    fn run(state: &mut BikeState, config: &BikeConfig, input: &BikeInput, steps: u32) {
        for _ in 0..steps {
            bike_step(state, config, input).expect("step");
        }
    }

    /// glibc `cosf(0x3d001e53)` is one ulp above [`det_cos`], so `tan` of that steer
    /// (and the lean `atan` built from it) must come from the deterministic kernels.
    /// The digest hashes `lean`, so a libm last digit splits lockstep peers (spec 15 § 5).
    #[test]
    fn steady_lean_uses_the_deterministic_tan_and_atan() {
        use regia_core::math::{det_atan, det_tan};
        let steer = f32::from_bits(0x3d00_1e53);
        let cfg = config();
        let speed = 8.0;
        let lateral = speed * speed * det_tan(steer) / cfg.wheelbase;
        let kernel = det_atan(lateral / cfg.gravity);
        assert_eq!(
            steady_lean(speed, steer, &cfg).to_bits(),
            kernel.to_bits(),
            "steady lean {:08x} vs kernel {:08x}",
            steady_lean(speed, steer, &cfg).to_bits(),
            kernel.to_bits()
        );
    }

    #[test]
    fn steer_zero_at_speed_holds_lean_near_zero() {
        let mut state = BikeState::default();
        run(&mut state, &config(), &input(0.0, 8.0), 5 * 60);
        assert!(state.lean.abs() < 1.0e-3, "lean {}", state.lean);
        assert!(!state.fallen);
        assert!(state.front_contact && state.rear_contact);
    }

    #[test]
    fn constant_steer_reaches_the_closed_form_lean() {
        let cfg = config();
        let steer: f32 = 0.08;
        let speed: f32 = 8.0;
        let expected = (speed * speed * steer.tan() / (cfg.wheelbase * cfg.gravity)).atan();
        assert!((steady_lean(speed, steer, &cfg) - expected).abs() < 1.0e-6);
        assert!(
            expected > 0.2,
            "the fixture lean should be well clear of 0, got {expected}"
        );

        let mut state = BikeState::default();
        run(&mut state, &cfg, &input(steer, speed), 5 * 60);
        assert_eq!(state.lean.signum(), steer.signum());
        let error = (state.lean - expected).abs();
        assert!(
            error <= 0.10 * expected.abs(),
            "lean {} expected {expected} (error {error})",
            state.lean
        );

        let mut opposite = BikeState::default();
        run(&mut opposite, &cfg, &input(-steer, speed), 5 * 60);
        assert!(opposite.lean < 0.0);
        assert!((opposite.lean + state.lean).abs() <= 1.0e-4);
    }

    #[test]
    fn low_speed_lean_returns_toward_upright() {
        let cfg = config();
        let mut state = BikeState {
            lean: 0.2,
            ..BikeState::default()
        };
        // Speed 0 is under the 2 m/s threshold, and the steer must not hold the lean out.
        run(&mut state, &cfg, &input(0.4, 0.0), 60);
        assert!(
            state.lean > 0.0,
            "the spring does not cross through upright in one second"
        );
        assert!(
            state.lean < 0.1,
            "lean should have moved toward 0, got {}",
            state.lean
        );
        assert!(!state.fallen);

        let mut held = BikeState {
            lean: 0.2,
            ..BikeState::default()
        };
        run(&mut held, &cfg, &input(0.4, 1.5), 60);
        assert!(
            held.lean < 0.1,
            "below 2 m/s the steer term stays off, got {}",
            held.lean
        );
    }

    #[test]
    fn non_finite_steer_is_bike_bad_input_and_keeps_state() {
        let cfg = config();
        let seeded = BikeState {
            lean: 0.2,
            steer: 0.1,
            speed: 3.0,
            drive: 0.5,
            front_contact: true,
            rear_contact: true,
            fallen: false,
        };
        for steer in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut state = seeded;
            let error = bike_step(&mut state, &cfg, &input(steer, 8.0)).expect_err("refused");
            assert_eq!(error.code(), "bike_bad_input");
            assert_eq!(state, seeded);
        }
    }

    #[test]
    fn repeated_runs_share_a_digest() {
        let cfg = config();
        let mut once = BikeState::default();
        let mut twice = BikeState::default();
        let command = input(0.08, 8.0);
        run(&mut once, &cfg, &command, 300);
        run(&mut twice, &cfg, &command, 300);
        assert_eq!(once.digest(), twice.digest());
        assert_eq!(once, twice);

        let mut other = BikeState::default();
        run(&mut other, &cfg, &input(-0.08, 8.0), 300);
        assert_ne!(other.digest(), once.digest());
    }

    #[test]
    fn a_lean_past_the_maximum_falls_and_zeroes_drive() {
        let cfg = BikeConfig {
            max_lean: 0.15,
            ..BikeConfig::default()
        };
        let mut state = BikeState::default();
        let mut command = input(0.25, 8.0);
        command.drive = 0.75;
        run(&mut state, &cfg, &command, 30);
        assert!(state.fallen, "lean {}", state.lean);
        assert!(state.lean.abs() > cfg.max_lean);
        assert_eq!(state.drive, 0.0);

        let lean = state.lean;
        command.drive = 1.0;
        command.steer = -0.2;
        command.rear_ray = f32::INFINITY;
        bike_step(&mut state, &cfg, &command).expect("after the fall");
        assert!(state.fallen);
        assert_eq!(state.drive, 0.0);
        assert_eq!(state.lean.to_bits(), lean.to_bits());
        assert!(!state.rear_contact);
        assert!(state.front_contact);
        assert!((state.steer - command.steer).abs() < 1.0e-6);
    }

    #[test]
    fn an_initial_lean_past_the_limit_falls_before_the_upright_spring() {
        let cfg = config();
        for sign in [-1.0, 1.0] {
            let mut state = BikeState {
                lean: sign * (cfg.max_lean + 0.01),
                drive: 0.75,
                ..BikeState::default()
            };
            let lean = state.lean;
            bike_step(&mut state, &cfg, &input(0.0, 0.0)).expect("valid step");
            assert!(
                state.fallen,
                "an over-limit initial lean must already count as fallen"
            );
            assert_eq!(state.drive, 0.0);
            assert_eq!(state.lean.to_bits(), lean.to_bits());
            assert!(state.front_contact && state.rear_contact);
        }
    }

    /// corr-low P2: a NaN lean handed in by the caller compares false against `max_lean`, so
    /// it never fell and every later step stayed NaN. It is refused and the state is kept.
    #[test]
    fn a_nan_lean_is_bike_bad_input_and_keeps_state() {
        let cfg = config();
        let seeded = BikeState {
            lean: f32::NAN,
            steer: 0.1,
            speed: 3.0,
            drive: 0.5,
            ..BikeState::default()
        };
        for command in [input(0.08, 8.0), input(0.0, 0.0)] {
            let mut state = seeded;
            let error = bike_step(&mut state, &cfg, &command).expect_err("a NaN lean is refused");
            assert_eq!(error.code(), "bike_bad_input");
            assert_eq!(state.digest(), seeded.digest(), "the state is unchanged");
        }
    }

    /// corr-low P1: with `response · dt > 1` the clamped-rate branch stepped past the target
    /// (40/s on a 0.25 s step moves 0.625 rad toward a target 0.3125 rad away) and swung
    /// across it every step, forever. The step stops at the target.
    #[test]
    fn a_stiff_response_on_a_long_step_never_steps_past_the_target() {
        let cfg = BikeConfig {
            upright_rate: 40.0,
            lean_response: 40.0,
            ..BikeConfig::default()
        };
        // Below the upright speed (target 0) and above it (the closed form).
        for speed in [0.0, 8.0] {
            let mut command = input(0.08, speed);
            command.dt = MAX_STEP_S;
            let target = steady_lean(speed, command.steer, &cfg);
            let mut state = BikeState {
                lean: target + 0.3125,
                ..BikeState::default()
            };
            let mut error = state.lean - target;
            for step in 0..20 {
                bike_step(&mut state, &cfg, &command).expect("valid step");
                let next = state.lean - target;
                assert!(
                    next >= 0.0 && next <= error,
                    "speed {speed}, step {step}: error {error} became {next} (lean {})",
                    state.lean
                );
                error = next;
            }
            assert!(
                error.abs() < 1.0e-6,
                "speed {speed}: settled {error} off the target"
            );
            assert!(!state.fallen);
        }
    }

    #[test]
    fn ray_lengths_set_the_two_contact_flags() {
        let cfg = config();
        let mut state = BikeState::default();
        let mut command = input(0.0, 8.0);
        command.front_ray = 0.7;
        command.rear_ray = 0.71;
        bike_step(&mut state, &cfg, &command).expect("step");
        assert!(state.front_contact);
        assert!(!state.rear_contact);

        command.front_ray = f32::INFINITY;
        command.rear_ray = 0.0;
        bike_step(&mut state, &cfg, &command).expect("step");
        assert!(!state.front_contact);
        assert!(state.rear_contact);

        let before = state;
        command.rear_ray = -0.01;
        let error = bike_step(&mut state, &cfg, &command).expect_err("negative ray");
        assert_eq!(error.code(), "bike_bad_input");
        assert_eq!(state, before);
    }
}
