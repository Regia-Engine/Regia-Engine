// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/hover.rs, commit d6ed37677), ADR-8012.
// Mark: adapt. The step, the hop, the lean, the flight rules and every test are DO27's, the arithmetic in the same order
// (DO27's switch pins depend on it). Named changes (ADR-8079, ADR-8164):
//   1. ADR-8012 A-8: the retail-baseline parameter names are neutral names (the map is in docs/ported-sources.toml),
//      `HoverPhysicsParams` is `HoverParams` and `step_physics{,_world,_tuned}` is `step{,_world,_tuned}`.
//   2. `Default` read DO27's hull class table. The table, and every number DO27 measured from a baseline game, stay in
//      DO27 (ADR-8012 A-8, P4): the default is `HoverParams::DEFAULT_HULL`, an engine-authored hull (ADR-8164), and the
//      measured release-drop, roll-spring and cushion constants the step read from DO27's handling file are
//      `HoverParams` fields, so a game passes its own at runtime (the fan-grip and lift numbers already were
//      `ClimbTuning` / `FlightRules` fields; their defaults are Regia's too).
//   3. `ClimbTuning::active()` read DO27's handling file. A host picks the flight rules (`FlightMode`, a match
//      setting) and calls `step_tuned`; `step`/`step_world` fly the default air-time rules, as DO27's shipped file did.
//   4. serde derives are behind the crate's `serde` feature; DO27 names are gone from the doc comments and tests.
//   5. Std float `min`/`max`/`clamp` are `crate::float::det_*` (ADR-8085): a zero result is `+0` on every host.
//   6. ADR-8349: the model limits (hop, air steer, thresholds, crest, lookahead, grade and attitude limits) are the
//      fields of `HoverLimits` (`HoverLimits::DEFAULT` = the old constants, read in the same order), the world is a
//      `WorldPhysics`, and `step_with` takes one value per concern; `step`, `step_world` and `step_tuned` wrap it.

//! The lockstep hover step (ADR-8079, plan row S-07): a pure
//! [`step(params, state, input, floor, dt)`](step) for a hover hull.
//!
//! Throttle and brake dynamics, drag, speed-dependent spin-vs-turn yaw rates, an altitude PD
//! spring with a lookahead, fan grip on climbs, the cushion's roll and pitch, auto-level, jump and
//! hop. The model is a constant-acceleration throttle with a taper near the speed cap,
//! constant-deceleration drag, speed-dependent yaw, and a PD altitude spring. Every number a game
//! tunes is a field of [`HoverParams`] (per hull) or [`ClimbTuning`] (per match);
//! [`HoverParams::DEFAULT_HULL`] is an engine-authored generic hull to start from (ADR-8164).
//! The model's own limits (hop apex and cooldown, air steer, thresholds, crest, lookahead, grade and
//! attitude limits) are a [`HoverLimits`] value, and the world (gravity, air, vacuum, low-g mode) is a
//! [`WorldPhysics`]: [`step_with`] takes one of each (ADR-8349), and [`step`], [`step_world`] and
//! [`step_tuned`] are thin wrappers that fly [`HoverLimits::DEFAULT`].
//!
//! # Ownership contract
//!
//! This module owns **velocity, yaw, attitude and altitude only**. Position integration is the
//! caller's job: each tick the caller advances world XZ by `velocity * dt` and derives world Y as
//! `terrain_height + state.altitude` (altitude is height above ground level, see
//! [`HoverState::altitude`]). When the terrain height under the craft changes between ticks, the
//! step reads the change from `terrain_height` and the previous tick's value and lets the spring
//! absorb it (the hull flies over crests and settles into dips).
//!
//! # Coordinate/trig conventions
//!
//! Yaw is rotation about +Y. Forward in the XZ plane is `(sin(yaw), cos(yaw))`; the right/strafe
//! direction is `(cos(yaw), -sin(yaw))`.
//!
//! # Determinism
//!
//! [`step`] is a pure function of its arguments: f32 arithmetic in a fixed order, transcendentals
//! through [`crate::math`] only, no wall clock, no RNG, no map iteration. Identical inputs produce
//! bit-identical state on every target (ADR-8012 A-4). The libm-1000 vector
//! (`tests/hover_libm_1000.rs`) and the `hover_libm_1000` golden scenario pin it.
//!
//! ```
//! use regia_detsim::hover::{self, HoverParams, HoverState};
//! use regia_detsim::tick::tick_dt;
//!
//! let params = HoverParams::DEFAULT_HULL;
//! let mut state = HoverState::default();
//! let (mut x, mut z) = (0.0_f32, 0.0_f32);
//! for _ in 0..60 {
//!     // Full throttle on flat ground at height 0.
//!     hover::step(&params, &mut state, &[1.0, 0.0, 0.0], 0.0, tick_dt());
//!     x += state.velocity[0] * tick_dt();
//!     z += state.velocity[2] * tick_dt();
//! }
//! assert!(z > 5.0 && x == 0.0);
//! assert!(state.altitude > 0.5, "the spring lifted the hull toward its ride height");
//! ```

use crate::float::{canonical, det_clamp, det_max, det_min};
use crate::math;

mod handshake;
pub mod host;
mod limits;
mod scenario;
pub mod tuning;
mod validate;

pub use crate::site_physics::WorldPhysics;
pub use handshake::{
    HULL_HANDSHAKE_BYTES, HullHandshakeMismatch, hull_handshake_bytes, hull_handshake_mismatch,
};
pub use limits::HoverLimits;
pub use scenario::{
    HoverBadInput, HoverFleetTerrain, HoverLibm1000, HoverMixed, HoverMixedCoverage, HoverRateModel,
    LowgBallisticCoverage, LowgBallisticMixed, RollingField, mixed_floor,
};
pub use validate::{FieldRange, HoverParamsError, HoverParamsErrorKind};

use crate::ballistic::LowG;
use tuning::{CREST_DOWN_FORCE, HOVER_LEAD_CLIMB_MAX, HOVER_LEAD_MAX_RISE_M, HOVER_RIDE_MAX_M};

/// Gravitational acceleration (m/s²) [`step`] applies while airborne: the gravity of
/// [`DEFAULT_WORLD`], and what a non-finite gravity reads as in every hover step.
///
/// 9.8, not [`crate::site_physics::EARTH_G`] (9.81, the gravity of [`WorldPhysics::EARTH`]): the
/// hover step was ported with its own round 9.8, and the libm-1000 vector, the hover goldens and
/// the hover tests are pinned on it. Changing it would move every airborne tick of every hull
/// stepped through [`step`], so it is a sim-version change (ADR-8019). A host that wants Earth's
/// 9.81 passes [`WorldPhysics::EARTH`] (or its own world) to [`step_with`] or [`step_world`].
pub const GRAVITY: f32 = 9.8;

/// The world [`step`] flies: [`GRAVITY`] (9.8 m/s²) in vacuum (no drag), low-g mode A.
pub const DEFAULT_WORLD: WorldPhysics = WorldPhysics {
    gravity_m_s2: GRAVITY,
    air_density_kg_m3: 0.0,
    vacuum: true,
    low_g: LowG::Governor,
};

/// The default hop and jump gravity floor, m/s² ([`HoverLimits::jump_feel_gravity_min`] of
/// [`HoverLimits::DEFAULT`]).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.jump_feel_gravity_min` or pass your own `HoverLimits`"
)]
pub const JUMP_FEEL_GRAVITY_MIN: f32 = HoverLimits::DEFAULT.jump_feel_gravity_min;

/// Gravity for an explicit hull hop or suit jump under the default limits
/// ([`HoverLimits::jump_gravity`] of [`HoverLimits::DEFAULT`]); other ballistics keep map gravity.
pub fn jump_gravity(map_gravity: f32) -> f32 {
    HoverLimits::DEFAULT.jump_gravity(map_gravity)
}

/// The climbing behaviour as tunable numbers, per match: [`ClimbTuning::AIR_TIME`] is the
/// engine default, and a game passes its own through [`step_tuned`]. A hull does not lose speed to
/// a slope as such: on a steep rise the hover lookahead lifts the hull well clear of the ground, and
/// a hull riding high has no fan grip: it stops thrusting and slows down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClimbTuning {
    /// Uphill slope (degrees) the fans still push at full strength.
    /// Valid: 0 to 90.
    pub slope_free_deg: f32,
    /// Further degrees of slope over which the fan grip falls to nothing.
    /// Valid: 0 to 90.
    pub slope_band_deg: f32,
    /// Metres above hover height the fans still push at full strength.
    /// Valid: 0 to 100.
    pub fan_free_m: f32,
    /// Further metres over which the fan grip falls to nothing.
    /// Valid: 0 to 100.
    pub fan_band_m: f32,
    /// Deceleration (m/s2) of a hull with no fan grip.
    /// Valid: 0 to 1000.
    pub air_decel: f32,
    /// Fastest ground climb rate (m/s) the hover lookahead follows.
    /// Valid: 0 to 1000.
    pub lead_climb_max: f32,
    /// Most the lookahead may lift the hover aim above hover height, metres.
    /// Valid: 0 to 100.
    pub lead_max_rise: f32,
    /// How the hull flies once the ground falls away or the ramp ends.
    pub flight: FlightRules,
}

/// The vertical rules over crests and ramps. [`FlightRules::AIR_TIME`] gives air time: a hull rides
/// high on a climb, keeps its upward speed over the crest and falls at no more than gravity (floored
/// at [`HoverLimits::crest_fall_min_m_s2`]) until the cushion catches it, and its fans push up at a
/// finite rate. [`FlightRules::CLAMPED_1014`] is the clamped set (2 m ride ceiling, lift cut over
/// falling ground, 2.5x down force, downhill hug): hulls hug the ground and hop 2 m at most.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlightRules {
    /// Most height above hover height the spring may aim at, metres (the lookahead is also capped by `lead_max_rise`).
    /// Valid: 0 to 100.
    pub ride_max_m: f32,
    /// Zero the upward speed at the ride ceiling and over ground that is falling away (no carry over a crest).
    pub cut_lift_on_crest: bool,
    /// Fall acceleration over falling ground as a multiple of the usual fall.
    /// Valid: 0 to 10.
    pub crest_down_force: f32,
    /// Over a downhill gentler than [`HoverLimits::hover_cliff_slope`] take on the ground's descent at once (hug the slope).
    pub hug_downhill: bool,
    /// Most upward acceleration the fans give the hull, m/s2 (0 = no limit).
    /// Valid: 0 to 1000.
    pub lift_accel_max: f32,
    /// Ground contact keeps an upward speed the hull already has (air time); `false` zeroes it (the clamped rule).
    pub contact_keeps_lift: bool,
    /// The damper works through the whole fall, so the cushion catches a falling hull softly. `false`: the damper fades
    /// in only over the last [`HoverLimits::crest_soft_band_m`] (hard landings).
    pub fall_damped: bool,
}

impl FlightRules {
    /// Air-time flight: the fall is gravity floored at [`HoverLimits::crest_fall_min_m_s2`], the fans lift
    /// at up to 2 g.
    ///
    /// `lift_accel_max` 20 m/s2 is two standard gravities, rounded: enough for a hull meeting a ramp
    /// at speed to rise with it instead of being thrown, never a launch (ADR-8164).
    pub const AIR_TIME: FlightRules = FlightRules {
        ride_max_m: HOVER_LEAD_MAX_RISE_M,
        cut_lift_on_crest: false,
        crest_down_force: 1.0,
        hug_downhill: false,
        lift_accel_max: 20.0,
        contact_keeps_lift: true,
        fall_damped: true,
    };
    /// The clamped rules: hulls hug the ground, hops of 2 m at most.
    pub const CLAMPED_1014: FlightRules = FlightRules {
        ride_max_m: HOVER_RIDE_MAX_M,
        cut_lift_on_crest: true,
        crest_down_force: CREST_DOWN_FORCE,
        hug_downhill: true,
        lift_accel_max: 0.0,
        contact_keeps_lift: false,
        fall_damped: false,
    };
}

impl ClimbTuning {
    /// The engine's default climb with the air-time flight rules: what [`step`] and [`step_world`]
    /// fly. Engine-authored round numbers (ADR-8164), not a fit to any game:
    ///
    /// - full fan grip up to a 15 degree grade, gone at 40 degrees (15 + 25): a 30 t hull on its
    ///   own fans climbs a steep road grade at speed and stalls short of a scree slope;
    /// - full grip up to 1.5 ride heights over the hover height, gone 6 m further up (the fans lose
    ///   the ground well before a hull is airborne);
    /// - 3 m/s2 of deceleration with no grip (about 0.3 g of air and skirt drag);
    /// - the lookahead follows climbs up to [`tuning::HOVER_LEAD_CLIMB_MAX`] and lifts the aim at
    ///   most [`tuning::HOVER_LEAD_MAX_RISE_M`].
    pub const AIR_TIME: ClimbTuning = ClimbTuning {
        slope_free_deg: 15.0,
        slope_band_deg: 25.0,
        fan_free_m: 1.5,
        fan_band_m: 6.0,
        air_decel: 3.0,
        lead_climb_max: HOVER_LEAD_CLIMB_MAX,
        lead_max_rise: HOVER_LEAD_MAX_RISE_M,
        flight: FlightRules::AIR_TIME,
    };
    /// [`Self::AIR_TIME`] with the clamped flight rules.
    pub const CLAMPED_1014: ClimbTuning = ClimbTuning {
        flight: FlightRules::CLAMPED_1014,
        ..ClimbTuning::AIR_TIME
    };
}

impl Default for ClimbTuning {
    fn default() -> Self {
        ClimbTuning::AIR_TIME
    }
}

/// The flight rules a match flies, by name. A match setting: every peer must use the same one. It is
/// byte 180 of [`hull_handshake_bytes`] (0 `air_time`, 1 `clamped`), so a peer that flies another
/// mode fails the hull handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlightMode {
    /// [`FlightRules::AIR_TIME`] (`"air_time"`, the default).
    #[default]
    AirTime,
    /// [`FlightRules::CLAMPED_1014`] (`"clamped"`).
    Clamped,
}

impl FlightMode {
    /// The mode called `name` (`"air_time"` or `"clamped"`); `None` for anything else, so a typo in
    /// a settings file never silently picks a mode.
    pub fn from_name(name: &str) -> Option<FlightMode> {
        match name {
            "air_time" => Some(FlightMode::AirTime),
            "clamped" => Some(FlightMode::Clamped),
            _ => None,
        }
    }

    /// The mode's name, as [`FlightMode::from_name`] reads it.
    pub const fn name(self) -> &'static str {
        match self {
            FlightMode::AirTime => "air_time",
            FlightMode::Clamped => "clamped",
        }
    }

    /// The climb tuning this mode steps with (pass it to [`step_tuned`]).
    pub const fn climb(self) -> &'static ClimbTuning {
        match self {
            FlightMode::AirTime => &ClimbTuning::AIR_TIME,
            FlightMode::Clamped => &ClimbTuning::CLAMPED_1014,
        }
    }
}

/// The modern aim preset's default yaw-rate tracking rate (1/s), at every speed, ramp-in and release
/// alike ([`HoverLimits::aim_modern_yaw_alpha`] of [`HoverLimits::DEFAULT`]): a 40 ms time constant,
/// about two sim ticks, where a hull's own ramp ([`HoverParams::yaw_response`]) is a few tenths of a
/// second.
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.aim_modern_yaw_alpha` or pass your own `HoverLimits`"
)]
pub const AIM_MODERN_YAW_ALPHA: f32 = HoverLimits::DEFAULT.aim_modern_yaw_alpha;

/// Hover physics parameters of one hull (ADR-8012 A-8 names).
///
/// Each field's doc gives its valid range ([`HoverParams::FIELD_RANGES`]). A hull read from user
/// content (a mod, a map file, a settings message) is checked with [`HoverParams::validate`] and
/// stepped as [`HoverParams::sanitized`]; the step itself does not validate.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HoverParams {
    /// Target hover altitude above terrain (AGL), metres.
    /// Valid: 0 to 50.
    pub ride_height: f32,
    /// Constant deceleration with input released (m/s² — linear bleed-off, the long drift).
    /// Valid: 0 to 1000.
    pub coast_drag: f32,
    /// Constant deceleration while under thrust (m/s², smaller so net
    /// thrust stays positive up to the velocity cap).
    /// Valid: 0 to 1000.
    pub thrust_drag: f32,
    /// Altitude spring proportional gain (PD controller).
    /// Valid: 0 to 400.
    pub height_spring: f32,
    /// Altitude spring damping gain on vertical velocity (PD controller).
    /// Valid: 0 to 50.
    pub height_damper: f32,
    /// Visual pitch per unit of throttle input.
    /// Valid: -10 to 10.
    pub lean_pitch_per_throttle: f32,
    /// Visual pitch per unit of normalized forward speed.
    /// Valid: -10 to 10.
    pub lean_pitch_per_speed: f32,
    /// Roll per unit of strafe input (the cushion's lean target and the visual lean).
    /// Valid: -10 to 10.
    pub lean_roll_per_strafe: f32,
    /// Visual roll per unit of yaw rate.
    /// Valid: -10 to 10.
    pub lean_roll_per_yaw_rate: f32,
    /// Forward speed limit (m/s, positive along facing).
    /// Valid: 0 to 1000.
    pub max_forward_speed: f32,
    /// Reverse speed limit (m/s, magnitude).
    /// Valid: 0 to 1000.
    pub max_reverse_speed: f32,
    /// Strafe speed limit (m/s, lateral, magnitude).
    /// Valid: 0 to 1000.
    pub max_strafe_speed: f32,
    /// Acceleration under full throttle (m/s^2), also used for strafe.
    /// Valid: 0 to 1000.
    pub thrust_accel: f32,
    /// Deceleration while braking, i.e. throttle opposing motion (m/s^2).
    /// Valid: 0 to 1000.
    pub brake_accel: f32,
    /// Maximum commanded yaw rate at standstill (rad/s).
    /// Valid: 0 to 50.
    pub yaw_rate_at_rest: f32,
    /// Maximum commanded yaw rate at full speed (rad/s).
    /// Valid: 0 to 50.
    pub yaw_rate_at_speed: f32,
    /// First-order tracking rate toward the commanded yaw rate (1/s).
    /// Valid: 0 to 1000.
    pub yaw_response: f32,
    /// Vertical impulse applied by [`apply_jump`] (m/s).
    /// Valid: 0 to 1000.
    pub jump_speed: f32,
    /// Extra constant deceleration on the sideways (lateral) velocity, m/s2. 0 is a pure
    /// glide: the hull slides until the forward drag eats it. Separate from forward drag
    /// (`coast_drag`, `thrust_drag`) and from vertical damping (`height_damper`).
    /// Valid: 0 to 1000.
    #[cfg_attr(feature = "serde", serde(default))]
    pub lateral_drag: f32,
    /// Thrust taper toward the speed cap, 0..1. Thrust is scaled by `1 - taper * (v/cap)^2`,
    /// so 0 is a flat linear launch and larger values ease off near top speed.
    /// Valid: 0 to 1.
    #[cfg_attr(feature = "serde", serde(default))]
    pub thrust_taper: f32,
    /// Yaw tracking rate while steer is released (1/s). 0 means the same as `yaw_response`.
    /// Ramp-in uses `yaw_response`; ramp-out uses this.
    /// Valid: 0 to 1000.
    #[cfg_attr(feature = "serde", serde(default))]
    pub yaw_release_response: f32,
    /// Yaw tracking rate at full speed (1/s); the rate lerps from `yaw_response` (rest) to this with
    /// speed, like the yaw cap. 0 means `yaw_response` at every speed. Larger than `yaw_response`, a
    /// hull answers the steer faster when it is moving.
    /// Valid: 0 to 1000.
    #[cfg_attr(feature = "serde", serde(default))]
    pub yaw_response_at_speed: f32,
    /// Steer mapping at speed (rad/s at full speed). 0 keeps the plain mapping (commanded = steer *
    /// cap, the cap lerping `yaw_rate_at_rest` -> `yaw_rate_at_speed`). Above 0: commanded = steer *
    /// `yaw_rate_at_rest` minus this much (scaled by speed), clamped to the cap: a small steer at
    /// speed turns gently while full lock still reaches the cap.
    /// Valid: 0 to 50, and at most `yaw_rate_at_rest` (`inverted_range`).
    #[cfg_attr(feature = "serde", serde(default))]
    pub yaw_deadband: f32,
    /// Visual bank per m/s of lateral slip, added to the roll (rad per m/s).
    /// Valid: -10 to 10.
    #[cfg_attr(feature = "serde", serde(default))]
    pub roll_slip: f32,
    /// Hover lookahead, seconds: the hull aims at the ground this far ahead of itself, judged from
    /// how fast the ground under it is rising. On a long slope it rides high and floats off the
    /// crest; 0 = hug the ground.
    /// Valid: 0 to 10.
    #[cfg_attr(feature = "serde", serde(default))]
    pub hover_lead: f32,
    /// Share of gravity along the ground that slows an uphill hull (0 = none).
    /// Valid: 0 to 4.
    #[cfg_attr(feature = "serde", serde(default))]
    pub slope_pull: f32,
    /// Forward drag (m/s2) while released and faster than `drag_knee`: the quick first drop of a
    /// two-stage stop. 0 = use `coast_drag` throughout.
    /// Valid: 0 to 1000.
    #[cfg_attr(feature = "serde", serde(default))]
    pub accel_drag_high: f32,
    /// Speed (m/s) below which released drag falls to `coast_drag`.
    /// Valid: 0 to 1000.
    #[cfg_attr(feature = "serde", serde(default))]
    pub drag_knee: f32,
    /// This hull's boost multiplier on speed and thrust. 0 = use the match tuning. The step does not
    /// read it; a host applies boost to a copy of the params.
    /// Valid: 0 to 100.
    #[cfg_attr(feature = "serde", serde(default))]
    pub turbo_scale: f32,
    /// Sideways velocity bled per second as a share (1/s): how hard the hull grips in a turn. 0 = pure slide.
    /// Valid: 0 to 1000.
    #[cfg_attr(feature = "serde", serde(default))]
    pub lat_grip: f32,
    /// Speed lost per radian of heading change while turning (1/rad): the cost of cornering.
    /// Valid: 0 to 100.
    #[cfg_attr(feature = "serde", serde(default))]
    pub turn_bleed: f32,
    /// Strafe thrust as a share of `thrust_accel` (0 = 1.0). A share, so every scaling of
    /// `thrust_accel` reaches it too.
    /// Valid: 0 to 10.
    #[cfg_attr(feature = "serde", serde(default))]
    pub strafe_thrust_share: f32,
    /// Reverse thrust from rest as a share of `thrust_accel` (0 = 1.0).
    /// Valid: 0 to 10.
    #[cfg_attr(feature = "serde", serde(default))]
    pub reverse_thrust_share: f32,
    /// Release drop, m/s: when the hull stops thrusting (no throttle and no strafe) it sheds
    /// `coast_drop + coast_drop_share * speed` with the time constant
    /// [`HoverParams::coast_drop_tau`], then coasts at `coast_drag`: a quick first drop that scales
    /// with the speed at release, then a steady drift. Both 0 = no drop (the two-stage
    /// `accel_drag_high` / `drag_knee` stop still works for hulls that set it).
    /// Valid: 0 to 1000.
    #[cfg_attr(feature = "serde", serde(default))]
    pub coast_drop: f32,
    /// Release drop per m/s of speed at release (see `coast_drop`).
    /// Valid: 0 to 10.
    #[cfg_attr(feature = "serde", serde(default))]
    pub coast_drop_share: f32,
    /// Bank into a turn, rad per m/s2 of centripetal acceleration (yaw rate x forward speed). With
    /// `lean_roll_per_strafe` (rad per unit of strafe, into the strafe) it is the hull's real roll
    /// target on the cushion: sim state, drawn on the hull and on a rigid cockpit camera.
    /// Valid: -10 to 10.
    #[cfg_attr(feature = "serde", serde(default))]
    pub roll_bank: f32,
    /// Time constant of the release drop ([`HoverParams::coast_drop`]), seconds. Must be above 0.
    /// Valid: above 0, at most 100 (0 is `zero_rate`; `sanitized` raises 0 or less to 0.001).
    #[cfg_attr(feature = "serde", serde(default = "serde_defaults::coast_drop_tau"))]
    pub coast_drop_tau: f32,
    /// Natural frequency of the hull's roll spring (cushion lean and auto-level), rad/s. Must be
    /// above 0.
    /// Valid: above 0, at most 20 (0 is `zero_rate`; `sanitized` raises 0 or less to 0.001).
    #[cfg_attr(feature = "serde", serde(default = "serde_defaults::roll_spring_omega"))]
    pub roll_spring_omega: f32,
    /// Damping ratio of the roll spring (1 = critically damped).
    /// Valid: 0 to 1.25.
    #[cfg_attr(feature = "serde", serde(default = "serde_defaults::roll_spring_zeta"))]
    pub roll_spring_zeta: f32,
    /// The cushion's damper while it is being compressed (the hull moving down within
    /// [`HoverLimits::air_attitude_m`] of its ride height), as a share of `height_damper`. Under 1 the
    /// hull dips a little under its ride height after a hop and sags past a crest instead of reading
    /// as locked to it; rising (a climb, the rebound) keeps the full damper.
    /// Valid: 0 to 1.
    #[cfg_attr(
        feature = "serde",
        serde(default = "serde_defaults::cushion_compress_damp_share")
    )]
    pub cushion_compress_damp_share: f32,
    /// Share of the ground's pitch and roll under the hull that the cushion follows, 0..1.
    /// Valid: 0 to 1.
    #[cfg_attr(feature = "serde", serde(default = "serde_defaults::cushion_terrain_share"))]
    pub cushion_terrain_share: f32,
    /// Natural frequency of the cushion's pitch spring toward that share, rad/s.
    /// Valid: 0 to 20.
    #[cfg_attr(feature = "serde", serde(default = "serde_defaults::cushion_pitch_omega"))]
    pub cushion_pitch_omega: f32,
    /// Damping ratio of the cushion's pitch spring.
    /// Valid: 0 to 1.25.
    #[cfg_attr(feature = "serde", serde(default = "serde_defaults::cushion_pitch_zeta"))]
    pub cushion_pitch_zeta: f32,
    /// How far over its ride height the cushion still follows the ground in full, metres: riding
    /// high on the lookahead up a climb is not being in the air. Past this the share fades out over
    /// `cushion_reach_band_m` (the fans lose the ground).
    /// Valid: 0 to 100.
    #[cfg_attr(feature = "serde", serde(default = "serde_defaults::cushion_reach_full_m"))]
    pub cushion_reach_full_m: f32,
    /// Metres over which the cushion's reach fades out past `cushion_reach_full_m`. Must be above 0.
    /// Valid: above 0, at most 100 (0 is `zero_rate`; `sanitized` raises 0 or less to 0.001).
    #[cfg_attr(feature = "serde", serde(default = "serde_defaults::cushion_reach_band_m"))]
    pub cushion_reach_band_m: f32,
}

/// serde defaults of the fields a file written before ADR-8164 lacks: the [`HoverParams::DEFAULT_HULL`] values.
#[cfg(feature = "serde")]
mod serde_defaults {
    use super::HoverParams;
    pub(super) fn coast_drop_tau() -> f32 {
        HoverParams::DEFAULT_HULL.coast_drop_tau
    }
    pub(super) fn roll_spring_omega() -> f32 {
        HoverParams::DEFAULT_HULL.roll_spring_omega
    }
    pub(super) fn roll_spring_zeta() -> f32 {
        HoverParams::DEFAULT_HULL.roll_spring_zeta
    }
    pub(super) fn cushion_compress_damp_share() -> f32 {
        HoverParams::DEFAULT_HULL.cushion_compress_damp_share
    }
    pub(super) fn cushion_terrain_share() -> f32 {
        HoverParams::DEFAULT_HULL.cushion_terrain_share
    }
    pub(super) fn cushion_pitch_omega() -> f32 {
        HoverParams::DEFAULT_HULL.cushion_pitch_omega
    }
    pub(super) fn cushion_pitch_zeta() -> f32 {
        HoverParams::DEFAULT_HULL.cushion_pitch_zeta
    }
    pub(super) fn cushion_reach_full_m() -> f32 {
        HoverParams::DEFAULT_HULL.cushion_reach_full_m
    }
    pub(super) fn cushion_reach_band_m() -> f32 {
        HoverParams::DEFAULT_HULL.cushion_reach_band_m
    }
}

impl HoverParams {
    /// The engine's default hull (ADR-8164): a generic 30 t hover vehicle in round SI numbers,
    /// authored for Regia and fitted to nothing. It is [`HoverParams::default`], what the engine's
    /// hover golden and most hover tests step, and a starting point for a game's own hulls; a game
    /// passes its own `HoverParams` at runtime.
    ///
    /// The derivation, a 30 t hull about 7 m long on a fan-fed air cushion:
    ///
    /// - **Cushion.** Ride height 1 m (about a seventh of the length). The altitude spring is a
    ///   0.8 Hz bob: `height_spring` = ω² = 25 (ω = 5 rad/s) and `height_damper` = 2ζω = 8
    ///   (ζ = 0.8, a small overshoot that settles to a centimetre in about a second). While it is being compressed
    ///   the damper is 0.75 of that, so a landing dips a little. The cushion follows 0.3 of the
    ///   ground's tilt through a 5 rad/s, ζ 0.7 spring, in full up to 3 m over the ride height and
    ///   fading out over the next 5 m. Roll (strafe lean, turn bank, auto-level) is a 3 rad/s,
    ///   ζ 0.7 spring.
    /// - **Speed.** 30 m/s forward (108 km/h), a third of that in reverse (10 m/s), half sideways
    ///   (15 m/s, smaller side fans).
    /// - **Thrust and stopping.** 15 m/s2 of thrust (450 kN on 30 t, 1.5 g), tapered by half toward
    ///   the cap; strafe on 0.75 and reverse on 0.5 of it. Braking (reversed fans plus dumping the
    ///   cushion) 25 m/s2. With the fans pushing, the residual drag is 1 m/s2; released, the skirt
    ///   drags at 4 m/s2 (0.4 g) after a quick drop of 5 m/s plus a quarter of the speed with a
    ///   0.2 s time constant.
    /// - **Turning.** 3 rad/s pivot at rest (a half turn in about a second), 1.5 at full speed;
    ///   the yaw rate follows the steer with a 0.5 s time constant at rest and 0.33 s at speed; a
    ///   0.25 rad/s deadband on the at-speed steer. In a turn the side grip bleeds 4 per second of
    ///   the slip and the hull loses 0.5 of its speed per radian turned.
    /// - **Hops and slopes.** A 10 m/s jump (a 5 m apex at 1 g). The hover lookahead reads the
    ///   ground 1 s ahead; half of gravity along the ground slows a climbing hull.
    /// - **Lean (drawn only).** 0.1 rad of pitch per unit of throttle, 0.2 per unit of speed; 0.1 rad
    ///   of roll per unit of strafe and of yaw rate; 0.005 rad per m/s of slip and 0.002 per m/s2 of
    ///   turn.
    ///
    /// Every field is non-zero where a code path depends on it (taper, deadband, release drop, slip
    /// grip, turn bleed, slope pull, lookahead), so the engine's own tests exercise each path. The
    /// fields that stay 0 (`lateral_drag`, `yaw_release_response`, `accel_drag_high`, `drag_knee`) are
    /// optional models a hull opts into; the tests that cover them set them.
    pub const DEFAULT_HULL: HoverParams = HoverParams {
        ride_height: 1.0,
        coast_drag: 4.0,
        thrust_drag: 1.0,
        height_spring: 25.0,
        height_damper: 8.0,
        lean_pitch_per_throttle: 0.1,
        lean_pitch_per_speed: 0.2,
        lean_roll_per_strafe: 0.1,
        lean_roll_per_yaw_rate: 0.1,
        max_forward_speed: 30.0,
        max_reverse_speed: 10.0,
        max_strafe_speed: 15.0,
        thrust_accel: 15.0,
        brake_accel: 25.0,
        yaw_rate_at_rest: 3.0,
        yaw_rate_at_speed: 1.5,
        yaw_response: 2.0,
        jump_speed: 10.0,
        lateral_drag: 0.0,
        thrust_taper: 0.5,
        yaw_release_response: 0.0,
        yaw_response_at_speed: 3.0,
        yaw_deadband: 0.25,
        roll_slip: 0.005,
        hover_lead: 1.0,
        slope_pull: 0.5,
        accel_drag_high: 0.0,
        drag_knee: 0.0,
        turbo_scale: 1.0,
        lat_grip: 4.0,
        turn_bleed: 0.5,
        strafe_thrust_share: 0.75,
        reverse_thrust_share: 0.5,
        coast_drop: 5.0,
        coast_drop_share: 0.25,
        roll_bank: 0.002,
        coast_drop_tau: 0.2,
        roll_spring_omega: 3.0,
        roll_spring_zeta: 0.7,
        cushion_compress_damp_share: 0.75,
        cushion_terrain_share: 0.3,
        cushion_pitch_omega: 5.0,
        cushion_pitch_zeta: 0.7,
        cushion_reach_full_m: 3.0,
        cushion_reach_band_m: 5.0,
    };
}

impl Default for HoverParams {
    fn default() -> Self {
        HoverParams::DEFAULT_HULL
    }
}

/// Current hover state (velocity, heading, attitude, altitude).
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HoverState {
    /// Velocity vector `[x, y, z]` in world space (m/s). The Y component
    /// always mirrors `vertical_vel` after each tick (wire snapshots read
    /// this field).
    pub velocity: [f32; 3],
    /// Vertical velocity (m/s, positive up). Mirrored into `velocity[1]`.
    pub vertical_vel: f32,
    /// Yaw angle (heading, radians), wrapped to `(-PI, PI]`.
    pub yaw: f32,
    /// Yaw rate (angular velocity around +Y, rad/s).
    pub yaw_rate: f32,
    /// True while ballistic after a jump (hover spring disabled, gravity
    /// applies, steering authority reduced).
    pub airborne: bool,
    /// Explicit hop/jump arc, separate from jetpack flight, ejection and free fall.
    #[cfg_attr(feature = "serde", serde(default))]
    pub jump_active: bool,
    /// Height above ground level (AGL), in meters. `0.0` means resting on
    /// the terrain; the hover spring targets `ride_height`. The caller
    /// derives world Y as `terrain_height + altitude`.
    pub altitude: f32,
    /// Terrain height under the craft on the previous tick. As the craft
    /// moves horizontally over changing terrain the ground rises/falls
    /// beneath it, disturbing the AGL altitude; the PD spring lags in
    /// correcting it, producing the hover bob (flying over crests,
    /// settling into dips). `None` = unseeded (first tick takes zero
    /// disturbance).
    #[cfg_attr(feature = "serde", serde(default))]
    pub prev_terrain: Option<f32>,
    /// Seconds until the next hull hop is allowed. One pulse per key press.
    #[cfg_attr(feature = "serde", serde(default))]
    pub hop_cooldown: f32,
    /// Rate (m/s) the ground rose under the hull on the previous tick, for crest lift-off.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ground_climb: f32,
    /// Smoothed slope (radians, positive uphill) of the ground along the way the hull is travelling.
    #[cfg_attr(feature = "serde", serde(default))]
    pub slope: f32,
    /// Hull roll, radians, right side down positive, wrapped to (-PI, PI]. Sim state (the same on every peer).
    #[cfg_attr(feature = "serde", serde(default))]
    pub roll: f32,
    /// Roll rate, rad/s.
    #[cfg_attr(feature = "serde", serde(default))]
    pub roll_rate: f32,
    /// Hull pitch (nose up), radians: the mouse aim the host applied this tick. Mid-air thrust
    /// follows it with auto-level off.
    #[cfg_attr(feature = "serde", serde(default))]
    pub pitch: f32,
    /// The driver's auto-level is OFF (carried on the input every tick). In the air nothing rights the hull, the strafe
    /// keys roll it, and the thrust pushes along the hull's own axes. On the cushion the hover fans still right it.
    #[cfg_attr(feature = "serde", serde(default))]
    pub level_off: bool,
    /// The driver plays the modern aim preset (carried on the input every tick): the yaw rate tracks
    /// the steer command at [`HoverLimits::aim_modern_yaw_alpha`] instead of the hull's `yaw_response` ramp.
    #[cfg_attr(feature = "serde", serde(default))]
    pub yaw_modern: bool,
    /// Release drop still to shed, m/s ([`HoverParams::coast_drop`]).
    #[cfg_attr(feature = "serde", serde(default))]
    pub coast_drop_left: f32,
    /// The hull was thrusting (throttle or strafe) last tick: a release starts the drop.
    #[cfg_attr(feature = "serde", serde(default))]
    pub thrusting: bool,
    /// The ground's pitch under the hull, radians, positive when it rises ahead. Written by the
    /// host's ground pass (from floor samples around the hull, host and predictor alike) and read
    /// by the next step. 0 = level or unknown.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ground_pitch: f32,
    /// The ground's roll under the hull, radians, positive when it rises on the hull's right.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ground_roll: f32,
    /// The cushion's pitch, radians, nose up positive: the hull following
    /// [`HoverParams::cushion_terrain_share`] of the ground's pitch through a spring (it lags the
    /// ground, and a hull off a crest keeps the pitch it left with). Sim state: a host adds it to
    /// the hull's aim, so the hull, a rigid cockpit and the guns pitch together.
    #[cfg_attr(feature = "serde", serde(default))]
    pub cushion_pitch: f32,
    /// Cushion pitch rate, rad/s.
    #[cfg_attr(feature = "serde", serde(default))]
    pub cushion_pitch_rate: f32,
}

impl Default for HoverState {
    fn default() -> Self {
        Self {
            velocity: [0.0; 3],
            vertical_vel: 0.0,
            yaw: 0.0,
            yaw_rate: 0.0,
            airborne: false,
            jump_active: false,
            altitude: 0.0,
            prev_terrain: None,
            hop_cooldown: 0.0,
            ground_climb: 0.0,
            slope: 0.0,
            roll: 0.0,
            roll_rate: 0.0,
            pitch: 0.0,
            level_off: false,
            yaw_modern: false,
            coast_drop_left: 0.0,
            thrusting: false,
            ground_pitch: 0.0,
            ground_roll: 0.0,
            cushion_pitch: 0.0,
            cushion_pitch_rate: 0.0,
        }
    }
}

/// Largest angle magnitude `wrap_pi` wraps by whole turns. A roll plus one tick of roll rate stays
/// far below it on every valid hull; past it (a hull with absurd spring numbers) the loop would run
/// for a very long time, and forever on an infinity or once one turn is below the angle's ulp.
const WRAP_PI_LIMIT: f32 = 1.0e4;

/// Wrap an angle to (-PI, PI]. Also the ground vehicles' published yaw (ADR-10062).
///
/// A NaN or infinite angle comes back as it is (the next step's state check resets the hull, see
/// [`try_step_tuned`]); a finite angle beyond `WRAP_PI_LIMIT` radians has no meaningful phase
/// left and reads as 0. Below that limit the steps are the original loop, bit for bit.
pub(crate) fn wrap_pi(a: f32) -> f32 {
    if a.is_nan() {
        return a;
    }
    if a.abs() > WRAP_PI_LIMIT {
        return if a.is_finite() { 0.0 } else { a };
    }
    let mut a = a;
    while a > core::f32::consts::PI {
        a -= core::f32::consts::TAU;
    }
    while a <= -core::f32::consts::PI {
        a += core::f32::consts::TAU;
    }
    a
}

/// True while the hull is off its cushion: after a hop, or riding more than
/// [`HoverLimits::air_attitude_m`] of [`HoverLimits::DEFAULT`] over its hover height
/// ([`HoverLimits::hull_in_air`] reads a game's own limits).
pub fn hull_in_air(params: &HoverParams, state: &HoverState) -> bool {
    HoverLimits::DEFAULT.hull_in_air(params, state)
}

/// How much of the ground's tilt the cushion still feels, 0..1: all of it up to
/// [`HoverParams::cushion_reach_full_m`] over the ride height (a hull riding high up a climb still
/// pitches with the slope), fading to nothing over the next [`HoverParams::cushion_reach_band_m`];
/// nothing during a hop.
pub fn cushion_reach(params: &HoverParams, state: &HoverState) -> f32 {
    if params.ride_height <= 0.0 || state.airborne {
        return 0.0;
    }
    let over = state.altitude - params.ride_height;
    det_clamp(
        1.0 - (over - params.cushion_reach_full_m) / params.cushion_reach_band_m,
        0.0,
        1.0,
    )
}

/// Apply one tick of hover physics.
///
/// - `params`: the hull's physics parameters.
/// - `state`: current state, modified in place.
/// - `input`: player input axes `[throttle, strafe, steer]`, each in
///   `[-1.0, 1.0]` (values are clamped defensively; a NaN or infinite axis reads as 0). There is no jump
///   axis; jumps are triggered by the host via [`apply_jump`] or [`apply_hull_hop`].
/// - `terrain_height`: ground height at the craft's XZ position. `altitude` is AGL, so the step
///   reads only its change since the previous tick (the bob, the slope estimate and the lookahead).
/// - `dt`: fixed timestep ([`crate::tick::tick_dt`], 1/60 s at the default rate).
///
/// Bad input never latches a NaN state: see [`try_step_tuned`] for the guards (a `dt` that is not
/// finite and above 0 leaves the state unchanged, a NaN floor reads as the previous one, a poisoned
/// state resets to rest at the floor). Validate a loaded hull with [`HoverParams::validate`].
///
/// The world is [`DEFAULT_WORLD`] (gravity [`GRAVITY`], vacuum), the flight rules are
/// [`ClimbTuning::AIR_TIME`] (air time) and the limits [`HoverLimits::DEFAULT`]; [`step_with`] takes
/// each as a value.
///
/// Braking: when throttle opposes the current forward motion,
/// `brake_accel` decelerates toward zero (clamped at zero within the
/// crossing tick); once stopped, `thrust_accel` accelerates into the
/// opposite direction on subsequent ticks.
///
/// Airborne model (simple deterministic choice): while `airborne`, the PD
/// spring is disabled and only gravity acts on `vertical_vel`; the craft
/// rejoins the spring once it descends back within [`HoverLimits::airborne_eps_m`] (0.25 m) of
/// `ride_height`.
pub fn step(params: &HoverParams, state: &mut HoverState, input: &[f32; 3], terrain_height: f32, dt: f32) {
    step_world(params, state, input, terrain_height, dt, GRAVITY, 0.0);
}

/// [`step`] with the map's gravity and air density (an air of 0 is vacuum: no drag). The same as
/// [`step_with`] on a world of this gravity and air, not in vacuum, in low-g mode A.
pub fn step_world(
    params: &HoverParams,
    state: &mut HoverState,
    input: &[f32; 3],
    terrain_height: f32,
    dt: f32,
    gravity: f32,
    air: f32,
) {
    step_tuned(
        params,
        state,
        input,
        terrain_height,
        dt,
        gravity,
        air,
        &ClimbTuning::AIR_TIME,
    );
}

/// [`step_world`] with the climb numbers and flight rules given (a match's [`FlightMode::climb`],
/// or a fit sweeping them). [`try_step_tuned`] without its report: the guards are the same. New code
/// calls [`step_with`], which takes the world and the limits as values.
#[expect(
    clippy::too_many_arguments,
    reason = "DO27's signature, kept for the switch (ADR-8079): a params struct would change every call site in its sim"
)]
pub fn step_tuned(
    params: &HoverParams,
    state: &mut HoverState,
    input: &[f32; 3],
    terrain_height: f32,
    dt: f32,
    gravity: f32,
    air: f32,
    climb: &ClimbTuning,
) {
    let _ = try_step_tuned(params, state, input, terrain_height, dt, gravity, air, climb);
}

/// What one [`step_with`] (or [`try_step_tuned`]) did besides stepping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StepReport {
    /// The state held a NaN or infinite word on entry and was reset to rest at the floor instead of
    /// stepped (see [`try_step_tuned`]). A host may log it; peers see the same reset on the same tick.
    pub recovered: bool,
}

/// True when every `f32` word of the state is finite.
fn state_is_finite(s: &HoverState) -> bool {
    s.velocity[0].is_finite()
        && s.velocity[1].is_finite()
        && s.velocity[2].is_finite()
        && s.vertical_vel.is_finite()
        && s.yaw.is_finite()
        && s.yaw_rate.is_finite()
        && s.altitude.is_finite()
        && s.prev_terrain.is_none_or(f32::is_finite)
        && s.hop_cooldown.is_finite()
        && s.ground_climb.is_finite()
        && s.slope.is_finite()
        && s.roll.is_finite()
        && s.roll_rate.is_finite()
        && s.pitch.is_finite()
        && s.coast_drop_left.is_finite()
        && s.ground_pitch.is_finite()
        && s.ground_roll.is_finite()
        && s.cushion_pitch.is_finite()
        && s.cushion_pitch_rate.is_finite()
}

/// [`step_tuned`] with its guards against bad input, reporting a recovery: [`step_with`] under
/// [`HoverLimits::DEFAULT`] on a world of this gravity and air (not in vacuum, low-g mode A). The
/// guards run before any arithmetic and change nothing for a finite state, finite inputs and floor, a
/// finite gravity and air density and a finite `dt > 0`: that path is the original step, bit for bit.
///
/// - **`dt`** not finite or not above 0: the state is left unchanged (as
///   [`crate::ballistic::step_debris`] does).
/// - **Input axis** NaN or infinite: read as `0.0`.
/// - **`terrain_height`** NaN or infinite: read as the previous floor (`state.prev_terrain`, or 0
///   when it is unset or not finite itself).
/// - **`gravity`** not finite: read as [`GRAVITY`]; **`air`** not finite: read as 0 (vacuum).
/// - **Poisoned state** (a NaN or infinite `f32` word on entry, from a host write or an earlier tick
///   stepped with invalid [`HoverParams`]): the hull is reset to rest at the floor
///   ([`HoverState::default`] with `altitude` = the ride height and `prev_terrain` = the floor,
///   keeping the host's `level_off` and `yaw_modern` flags), this tick is not stepped, and the report
///   says `recovered`.
///
/// The step never validates `params` or `climb` (it runs every tick): a host validates a hull when
/// it loads it ([`HoverParams::validate`]) and steps [`HoverParams::sanitized`] numbers. With
/// sanitized numbers, floors within ±1e9 m and a `dt` of at most 1/30 s, every state word stays
/// finite (book page `det-hover.md`, "Bad input").
#[expect(
    clippy::too_many_arguments,
    reason = "the step_tuned signature plus the report: DO27's call sites keep their shape (ADR-8079)"
)]
pub fn try_step_tuned(
    params: &HoverParams,
    state: &mut HoverState,
    input: &[f32; 3],
    terrain_height: f32,
    dt: f32,
    gravity: f32,
    air: f32,
    climb: &ClimbTuning,
) -> StepReport {
    let world = WorldPhysics {
        gravity_m_s2: gravity,
        air_density_kg_m3: air,
        vacuum: false,
        low_g: LowG::Governor,
    };
    step_with(
        params,
        &HoverLimits::DEFAULT,
        climb,
        &world,
        state,
        input,
        terrain_height,
        dt,
    )
}

/// One tick of hover physics with one value per concern (ADR-8349): the hull (`params`), the model's
/// limits (`limits`, [`HoverLimits::DEFAULT`] for the engine's model), the match's climb and flight
/// rules (`climb`, a [`FlightMode::climb`]), and the world (`world`: gravity, air, vacuum; the low-g
/// mode is not read by the hover step). Then the state, the input `[throttle, strafe, steer]`, the
/// floor height under the hull and the fixed `dt`, as in [`step`].
///
/// Every value but the state, input and floor is a match setting: every peer must step the same
/// ones, so they belong in the settings the lockstep handshake compares
/// ([`hull_handshake_bytes`] for the hull, the limits and the [`FlightMode`];
/// [`WorldPhysics::to_le_bytes`] for the world). A `climb` that is not a [`FlightMode::climb`] is not
/// in those bytes: a host that steps its own compares it itself. The step validates none of them (it runs every
/// tick): a host validates what it loads ([`HoverParams::validate`], [`ClimbTuning::validate`],
/// [`HoverLimits::validate`], [`WorldPhysics::validate`]).
///
/// [`step`], [`step_world`], [`step_tuned`] and [`try_step_tuned`] are this function under
/// [`HoverLimits::DEFAULT`], bit for bit. The guards against bad input are those of
/// [`try_step_tuned`]: a `dt` that is not finite and above 0 leaves the state unchanged, a non-finite
/// input axis reads as 0, a non-finite floor as the previous one, a non-finite gravity as
/// [`GRAVITY`], a non-finite air as vacuum, and a poisoned state resets to rest at the floor
/// (`recovered` in the report).
///
/// ```
/// use regia_detsim::hover::{self, ClimbTuning, HoverLimits, HoverParams, HoverState, WorldPhysics};
/// use regia_detsim::tick::tick_dt;
///
/// // A light, twitchy hover: short hops, quick repeats, sharp air steer.
/// let limits = HoverLimits {
///     hop_apex_m: 1.5,
///     hop_cooldown_s: 0.3,
///     airborne_steer_authority: 0.8,
///     ..HoverLimits::DEFAULT
/// };
/// assert!(limits.validate().is_ok());
/// let world = WorldPhysics::EARTH;
/// let params = HoverParams::DEFAULT_HULL;
/// let mut state = HoverState::default();
/// for _ in 0..60 {
///     let report = hover::step_with(
///         &params, &limits, &ClimbTuning::AIR_TIME, &world, &mut state, &[1.0, 0.0, 0.2], 0.0, tick_dt(),
///     );
///     assert!(!report.recovered);
/// }
/// assert!(state.velocity[2] > 5.0);
/// ```
#[expect(
    clippy::too_many_arguments,
    reason = "ADR-8349: one value per concern (hull, model limits, match climb, world, state, input, floor, dt); no loose tuning numbers are left"
)]
pub fn step_with(
    params: &HoverParams,
    limits: &HoverLimits,
    climb: &ClimbTuning,
    world: &WorldPhysics,
    state: &mut HoverState,
    input: &[f32; 3],
    terrain_height: f32,
    dt: f32,
) -> StepReport {
    let gravity = world.gravity_m_s2;
    let air = world.drag_air();
    if !(dt.is_finite() && dt > 0.0) {
        return StepReport::default();
    }
    let floor = if terrain_height.is_finite() {
        terrain_height
    } else {
        match state.prev_terrain {
            Some(prev) if prev.is_finite() => prev,
            _ => 0.0,
        }
    };
    if !state_is_finite(state) {
        let ride = if params.ride_height.is_finite() && params.ride_height > 0.0 {
            params.ride_height
        } else {
            0.0
        };
        *state = HoverState {
            altitude: ride,
            prev_terrain: Some(floor),
            level_off: state.level_off,
            yaw_modern: state.yaw_modern,
            ..HoverState::default()
        };
        return StepReport { recovered: true };
    }
    let axis = |x: f32| if x.is_finite() { x } else { 0.0 };
    let input = [axis(input[0]), axis(input[1]), axis(input[2])];
    let gravity = if gravity.is_finite() { gravity } else { GRAVITY };
    let air = if air.is_finite() { air } else { 0.0 };
    step_body(params, limits, state, &input, floor, dt, gravity, air, climb);
    StepReport::default()
}

/// The step itself (the original `step_tuned` body, with the model limits read from `limits` in the
/// places and order the constants were, ADR-8349): every argument already guarded by [`step_with`].
#[expect(
    clippy::too_many_arguments,
    reason = "the step_tuned signature plus the limits (ADR-8079, ADR-8349)"
)]
fn step_body(
    params: &HoverParams,
    limits: &HoverLimits,
    state: &mut HoverState,
    input: &[f32; 3],
    terrain_height: f32,
    dt: f32,
    gravity: f32,
    air: f32,
    climb: &ClimbTuning,
) {
    // Fan grip: a hull riding high above its hover height (the lookahead lifts it over a rise) has no grip to push on. 1 at
    // hover height, falling to 0 over `fan_band_m` once it is `fan_free_m` above it. Read from the altitude at the tick's start.
    let fan_grip = if state.airborne {
        1.0
    } else {
        let over = state.altitude - params.ride_height;
        // Height cuts the fans only while the ground is rising. That is the lookahead lifting the hull over a
        // climb. On a descent the ground falls away and AGL jumps even though the fans still have a cushion;
        // cutting thrust there slowed a downhill hull (the grade limit is uphill only).
        let by_height = if state.ground_climb <= 0.0 {
            1.0
        } else {
            det_clamp(
                1.0 - (over - climb.fan_free_m) / det_max(climb.fan_band_m, 1.0e-3),
                0.0,
                1.0,
            )
        };
        let slope_deg = state.slope * (180.0 / core::f32::consts::PI);
        let by_slope = det_clamp(
            1.0 - (slope_deg - climb.slope_free_deg) / det_max(climb.slope_band_deg, 1.0e-3),
            0.0,
            1.0,
        );
        det_min(by_height, by_slope)
    };
    let throttle = det_clamp(input[0], -1.0, 1.0);
    let strafe = det_clamp(input[1], -1.0, 1.0);
    let steer = det_clamp(input[2], -1.0, 1.0);
    state.hop_cooldown = det_max(state.hop_cooldown - dt, 0.0);

    // Facing basis in the XZ plane.
    let (sin_yaw, cos_yaw) = (math::sin(state.yaw), math::cos(state.yaw));
    let fwd = [sin_yaw, cos_yaw];
    let right = [cos_yaw, -sin_yaw];

    // --- Drag ---
    // CONSTANT deceleration on the horizontal velocity (these are
    // accelerations in m/s²): the classic long linear coast, and — critically — a fixed drag cost
    // that fast classes overcome up to their velocity cap (proportional
    // drag would terminal-limit a light hull at thrust-accel/drag = 20 m/s,
    // below its 25 m/s forward-speed).
    let under_thrust =
        throttle.abs() > limits.drag_input_threshold || strafe.abs() > limits.drag_input_threshold;
    let speed_now = math::sqrt(state.velocity[0] * state.velocity[0] + state.velocity[2] * state.velocity[2]);
    let drag_rate = if under_thrust {
        params.thrust_drag
    } else if params.accel_drag_high > 0.0 && speed_now > params.drag_knee {
        params.accel_drag_high
    } else {
        params.coast_drag
    };
    // The release stop is a short drop that scales with the speed at release, then a steady coast.
    if under_thrust {
        state.coast_drop_left = 0.0;
    } else if state.thrusting && (params.coast_drop > 0.0 || params.coast_drop_share > 0.0) {
        state.coast_drop_left = params.coast_drop + params.coast_drop_share * speed_now;
    }
    state.thrusting = under_thrust;
    let mut drop_shed = 0.0_f32;
    if state.coast_drop_left > 0.0 {
        drop_shed = state.coast_drop_left * det_min(dt / params.coast_drop_tau, 1.0);
        state.coast_drop_left -= drop_shed;
        if state.coast_drop_left < 1.0e-3 {
            state.coast_drop_left = 0.0;
        }
    }
    let horizontal_speed =
        math::sqrt(state.velocity[0] * state.velocity[0] + state.velocity[2] * state.velocity[2]);
    if horizontal_speed > 0.0 {
        let slowed = det_max(horizontal_speed - drag_rate * dt - drop_shed, 0.0);
        let drag_keep = slowed / horizontal_speed;
        state.velocity[0] *= drag_keep;
        state.velocity[2] *= drag_keep;
    }

    // --- Slope of the ground along the way the hull is going (smoothed), for the fan grip ---
    {
        let rise = terrain_height - state.prev_terrain.unwrap_or(terrain_height);
        let run = horizontal_speed * dt;
        let blend = det_min(dt / limits.grade_smooth_s, 1.0);
        if !state.airborne && run > 1.0e-3 && rise.abs() < limits.grade_teleport_m {
            let inst = math::atan2(rise, run);
            state.slope += (inst - state.slope) * blend;
        } else {
            state.slope -= state.slope * blend;
        }
    }

    // --- Decompose horizontal velocity into forward/lateral components ---
    let mut fwd_speed = state.velocity[0] * fwd[0] + state.velocity[2] * fwd[1];
    let mut lat_speed = state.velocity[0] * right[0] + state.velocity[2] * right[1];

    // --- Lateral drag (separate from forward drag; 0 = pure glide) ---
    if params.lateral_drag > 0.0 {
        let cut = params.lateral_drag * dt;
        lat_speed = if lat_speed > 0.0 {
            det_max(lat_speed - cut, 0.0)
        } else {
            det_min(lat_speed + cut, 0.0)
        };
    }

    // Lateral grip bleeds sideways speed: the hull realigns with the way it points. It runs while the hull drives (throttle
    // held) and while it steers, so the slip left over from a turn dies out as soon as the steer is released instead of the
    // hull gliding ~95 m sideways. While the steer is quiet the grip is much stronger than while carving
    // (`limits.lat_straighten_grip`): a hull driving straight bites, one sliding through a held turn drifts at the slip
    // `lat_grip` sets. The grip stays OFF for a commanded strafe (the strafe key is pushing the hull the way it is already
    // sliding) and for a pure coast, where the two-stage stop drag owns the curve.
    // `signum` is sign-sensitive: the canonical zero keeps a -0 slip from flipping the test (DO27 DET_FLOAT, ADR-8085).
    let strafe_commands_this_slide = strafe.abs() >= 0.05 && strafe * canonical(lat_speed).signum() > 0.0;
    if params.lat_grip > 0.0 && (throttle.abs() >= 0.05 || steer.abs() > 0.05) && !strafe_commands_this_slide
    {
        let grip = if steer.abs() > 0.05 {
            params.lat_grip
        } else {
            det_max(params.lat_grip, limits.lat_straighten_grip)
        };
        lat_speed -= lat_speed * det_min(grip * dt, 1.0);
    }

    // Auto-level OFF in the air: the thrust pushes along the hull's own axes, so a pitched or rolled hull also pushes up or
    // down (`air_lift`, m/s of vertical speed this tick). Auto-level ON, or on the cushion, the fans push level.
    let free_air = state.level_off && limits.hull_in_air(params, state);
    let (pitch_cos, pitch_sin) = if free_air {
        (math::cos(state.pitch), math::sin(state.pitch))
    } else {
        (1.0, 0.0)
    };
    let (roll_cos, roll_sin) = if free_air {
        (math::cos(state.roll), math::sin(state.roll))
    } else {
        (1.0, 0.0)
    };
    let mut air_lift = 0.0_f32;

    // --- Thrust / brake along facing ---
    // Thrust only adds up to the cap and never clips speed the hull already carries: a drift
    // keeps its sideways momentum, and an expired boost bleeds off (see below) instead of
    // snapping to cruise.
    if throttle != 0.0 {
        if throttle * fwd_speed < 0.0 {
            // Throttle opposes current motion: brake toward zero.
            let step = params.brake_accel * throttle.abs() * dt;
            fwd_speed = if fwd_speed > 0.0 {
                det_max(fwd_speed - step, 0.0)
            } else {
                det_min(fwd_speed + step, 0.0)
            };
        } else {
            let limit = if throttle > 0.0 {
                params.max_forward_speed
            } else {
                -params.max_reverse_speed
            };
            // Acceleration curve: thrust tapers toward the cap (`thrust_taper`, 0 = linear).
            let ratio = det_min(fwd_speed.abs() / det_max(params.max_forward_speed, 0.001), 1.0);
            let curve = 1.0 - params.thrust_taper * ratio * ratio;
            let share = if throttle < 0.0 && params.reverse_thrust_share > 0.0 {
                params.reverse_thrust_share
            } else {
                1.0
            };
            let push = throttle * params.thrust_accel * share * curve * fan_grip * dt;
            air_lift += push * pitch_sin;
            let next = fwd_speed + push * pitch_cos;
            fwd_speed = if throttle > 0.0 {
                det_min(next, det_max(limit, fwd_speed))
            } else {
                det_max(next, det_min(limit, fwd_speed))
            };
        }
    }

    // --- Strafe (lateral thrust, capped at max_strafe_speed) ---
    if strafe != 0.0 {
        let share = if params.strafe_thrust_share > 0.0 {
            params.strafe_thrust_share
        } else {
            1.0
        };
        let push = strafe * params.thrust_accel * share * fan_grip * dt;
        // Roll is right side down, so a rolled hull's right axis points down by sin(roll).
        air_lift -= push * roll_sin;
        let next = lat_speed + push * roll_cos;
        let cap = params.max_strafe_speed;
        lat_speed = if strafe > 0.0 {
            det_min(next, det_max(cap, lat_speed))
        } else {
            det_max(next, det_min(-cap, lat_speed))
        };
    }

    // --- No fan grip: the hull slows down on its own ---
    if fan_grip < 1.0 {
        let total_now = math::sqrt(fwd_speed * fwd_speed + lat_speed * lat_speed);
        if total_now > 0.0 {
            let keep = (det_max(total_now - climb.air_decel * (1.0 - fan_grip) * dt, 0.0)) / total_now;
            fwd_speed *= keep;
            lat_speed *= keep;
        }
    }

    // --- Slope: gravity along the ground slows a climbing hull and speeds a descending one ---
    // The rise was sampled along the path just travelled, so the pull lies along that velocity. Applying it to the
    // nose only rotated a slipping hull further sideways on a grade.
    let step_len = horizontal_speed * dt;
    if step_len > 1.0e-4 && !state.airborne {
        let rise = terrain_height - state.prev_terrain.unwrap_or(terrain_height);
        let sin_slope = det_clamp(
            rise / math::sqrt(step_len * step_len + rise * rise),
            -limits.slope_sin_max,
            limits.slope_sin_max,
        );
        let pull = params.slope_pull * det_max(gravity, limits.crest_fall_min_m_s2) * sin_slope * dt;
        let speed = math::sqrt(fwd_speed * fwd_speed + lat_speed * lat_speed);
        if speed > 1.0e-4 {
            let scale = (det_max(speed - pull, 0.0)) / speed;
            fwd_speed *= scale;
            lat_speed *= scale;
        }
    }

    // --- Overspeed bleed (expired boost, drift plus thrust) ---
    let top = det_max(params.max_forward_speed, params.max_strafe_speed);
    let bleed = limits.overspeed_bleed * params.thrust_accel * dt;
    if fwd_speed < -params.max_reverse_speed {
        fwd_speed = det_min(fwd_speed + bleed, -params.max_reverse_speed);
    }
    let total = math::sqrt(fwd_speed * fwd_speed + lat_speed * lat_speed);
    if total > top && total > 0.0 {
        let keep = det_max(total - bleed, top) / total;
        fwd_speed *= keep;
        lat_speed *= keep;
    }

    // Recompose world-space horizontal velocity.
    state.velocity[0] = fwd_speed * fwd[0] + lat_speed * right[0];
    state.velocity[2] = fwd_speed * fwd[1] + lat_speed * right[1];

    // --- Yaw dynamics ---
    // Commanded rate cap lerps from yaw_rate_at_rest (standstill) to yaw_rate_at_speed
    // (full speed); the lerp factor is clamped to [0, 1] so over-speed
    // never inverts the turn direction.
    let speed = math::sqrt(fwd_speed * fwd_speed + lat_speed * lat_speed);
    // A held throttle already counts as full speed: a hull turns at its slow rate the moment
    // you drive, not only once it has built up speed.
    let lerp_t = det_clamp(
        det_max(speed / params.max_forward_speed, throttle.abs()),
        0.0,
        1.0,
    );
    let rate_cap = params.yaw_rate_at_rest + (params.yaw_rate_at_speed - params.yaw_rate_at_rest) * lerp_t;
    let mut commanded_rate = if params.yaw_deadband > 0.0 {
        let want = det_max(
            steer.abs() * params.yaw_rate_at_rest - params.yaw_deadband * lerp_t,
            0.0,
        );
        // A zero steer is +0 (det_clamp, and canonical at this sign-sensitive sink), so no -0 reaches the yaw rate.
        det_min(want, rate_cap.abs()) * canonical(steer).signum()
    } else {
        steer * rate_cap
    };
    if state.airborne {
        commanded_rate *= limits.airborne_steer_authority;
    }
    // First-order tracking toward the commanded rate at yaw_response.
    let ramp_in = if params.yaw_response_at_speed > 0.0 {
        params.yaw_response + (params.yaw_response_at_speed - params.yaw_response) * lerp_t
    } else {
        params.yaw_response
    };
    let release = if params.yaw_release_response > 0.0 {
        params.yaw_release_response
    } else {
        ramp_in
    };
    let (ramp_in, release) = if state.yaw_modern {
        (limits.aim_modern_yaw_alpha, limits.aim_modern_yaw_alpha)
    } else {
        (ramp_in, release)
    };
    let rate = if steer.abs() < 0.05 { release } else { ramp_in };
    let track = det_min(rate * dt, 1.0);
    state.yaw_rate += (commanded_rate - state.yaw_rate) * track;
    if params.turn_bleed > 0.0 && steer.abs() > 0.05 {
        let keep = det_max(1.0 - params.turn_bleed * state.yaw_rate.abs() * dt, 0.0);
        state.velocity[0] *= keep;
        state.velocity[2] *= keep;
    }
    state.yaw += state.yaw_rate * dt;
    if state.yaw > core::f32::consts::PI {
        state.yaw -= core::f32::consts::TAU;
    } else if state.yaw <= -core::f32::consts::PI {
        state.yaw += core::f32::consts::TAU;
    }

    // --- Hull roll ---
    // On the cushion the hover fans hold the hull at its lean: into a strafe (`lean_roll_per_strafe`) and into a turn
    // (`roll_bank` per m/s2 of centripetal acceleration), through the roll spring, whatever the auto-level setting (that
    // is the hover physics, not the leveller). In the air, auto-level ON levels the hull through the same spring; auto-level
    // OFF leaves it alone: nothing rights it and the strafe keys roll it, so a hull in the air can roll all the way round.
    if params.ride_height > 0.0 {
        if free_air {
            let want = strafe * limits.free_roll_rate_rad_s;
            state.roll_rate += (want - state.roll_rate) * det_min(limits.free_roll_track * dt, 1.0);
        } else {
            let lean = params.lean_roll_per_strafe * strafe + params.roll_bank * state.yaw_rate * fwd_speed;
            // On the cushion the hull also follows a share of the ground's roll (ground rising on the right lifts the right
            // side, a negative roll here), while the fans reach the ground (`cushion_reach`). Past that, or in a hop, there is
            // no ground to follow: auto-level ON levels the hull.
            let ground = -params.cushion_terrain_share * cushion_reach(params, state) * state.ground_roll;
            let target = det_clamp(lean + ground, -limits.roll_lean_max, limits.roll_lean_max);
            let err = wrap_pi(target - state.roll);
            let acc = params.roll_spring_omega * params.roll_spring_omega * err
                - 2.0 * params.roll_spring_zeta * params.roll_spring_omega * state.roll_rate;
            state.roll_rate += acc * dt;
        }
        state.roll = wrap_pi(state.roll + state.roll_rate * dt);
        // --- Cushion pitch ---
        // On the cushion the hull pitches toward a share of the ground's pitch with a lag, also while
        // it rides high on the lookahead (`cushion_reach`). Out of reach or in a hop, auto-level ON levels it through the
        // same spring; auto-level OFF in the air leaves it alone (no righting torque: it keeps the pitch and the rate it
        // left the ground with, bounded so a hull never tumbles on it).
        if free_air {
            state.cushion_pitch += state.cushion_pitch_rate * dt;
        } else {
            let target = params.cushion_terrain_share * cushion_reach(params, state) * state.ground_pitch;
            let target = det_clamp(target, -limits.cushion_pitch_max, limits.cushion_pitch_max);
            let acc =
                params.cushion_pitch_omega * params.cushion_pitch_omega * (target - state.cushion_pitch)
                    - 2.0 * params.cushion_pitch_zeta * params.cushion_pitch_omega * state.cushion_pitch_rate;
            state.cushion_pitch_rate += acc * dt;
            state.cushion_pitch += state.cushion_pitch_rate * dt;
        }
        if state.cushion_pitch.abs() > limits.cushion_pitch_max {
            state.cushion_pitch = det_clamp(
                state.cushion_pitch,
                -limits.cushion_pitch_max,
                limits.cushion_pitch_max,
            );
            state.cushion_pitch_rate = 0.0;
        }
    } else {
        state.roll = 0.0;
        state.roll_rate = 0.0;
        state.cushion_pitch = 0.0;
        state.cushion_pitch_rate = 0.0;
    }

    // --- Terrain-following disturbance (hover bob) ---
    // The craft's world Y barely changed during this tick's horizontal
    // move, but the ground beneath it did — so its AGL height changes by
    // the negative of the terrain change. The PD spring below then lags
    // in restoring `ride_height`, so the hull flies over crests and
    // settles into dips instead of rigidly hugging the ground.
    let terrain_delta = terrain_height - state.prev_terrain.unwrap_or(terrain_height);
    state.altitude -= terrain_delta;
    state.prev_terrain = Some(terrain_height);
    // Hover lookahead: a smoothed climb rate of the ground under the hull.
    let climb_now = det_clamp(terrain_delta / dt, -climb.lead_climb_max, climb.lead_climb_max);
    let blend = det_min(dt / limits.hover_lead_smooth_s, 1.0);
    state.ground_climb += (climb_now - state.ground_climb) * blend;
    // A hull flies by the chosen rules; a body on foot (hover height 0) always keeps the clamped ground rules.
    let flight = if params.ride_height > 0.0 {
        climb.flight
    } else {
        FlightRules::CLAMPED_1014
    };
    let ride_ceiling = params.ride_height + flight.ride_max_m;
    let aim = det_clamp(
        params.ride_height + params.hover_lead * state.ground_climb,
        params.ride_height * 0.4,
        det_min(params.ride_height + climb.lead_max_rise, ride_ceiling),
    );

    // --- Vertical: PD hover spring, or ballistic while airborne ---
    // The cushion is softer while it is being compressed (moving down near the ground) than while it pushes the hull up: it
    // dips on a landing and over a crest instead of stopping dead at the ride height.
    let damp = if state.vertical_vel < 0.0 && state.altitude < params.ride_height + limits.air_attitude_m {
        params.height_damper * params.cushion_compress_damp_share
    } else {
        params.height_damper
    };
    if state.airborne {
        let next = crate::ballistic::step_debris(
            [state.velocity[0], state.vertical_vel, state.velocity[2]],
            if state.jump_active {
                limits.jump_gravity(gravity)
            } else {
                gravity
            },
            air,
            dt,
        );
        state.velocity[0] = next[0];
        state.vertical_vel = next[1];
        state.velocity[2] = next[2];
        if state.vertical_vel <= 0.0 && state.altitude <= params.ride_height + limits.airborne_eps_m {
            state.airborne = false;
            state.jump_active = false;
        }
    } else {
        // The fans never carry a hull above the ride ceiling, and a hull over ground that is falling away loses its lift.
        // (Clamped rules only: under the air-time rules a hull keeps its upward speed over a crest.)
        if flight.cut_lift_on_crest
            && (state.altitude >= ride_ceiling
                || (state.ground_climb < 0.0 && state.altitude > params.ride_height))
        {
            state.vertical_vel = det_min(state.vertical_vel, 0.0);
        }
        let altitude_error = aim - state.altitude;
        if altitude_error >= 0.0 {
            // Air time: the fans push up at a finite rate, so a hull meeting a ramp at speed rises over it instead of being
            // thrown.
            let mut lift = params.height_spring * altitude_error - damp * state.vertical_vel;
            if flight.lift_accel_max > 0.0 {
                lift = det_min(lift, flight.lift_accel_max);
            }
            state.vertical_vel += lift * dt;
        } else {
            // Above hover height (a crest fell away): the fans let go and the hull drops under
            // `fall`, then the damper fades back in over the last half metre so it settles softly.
            // Over a downhill (not a cliff) the hull takes on the ground's descent, so it hugs the slope instead of flying off it.
            let downhill = -math::sin(state.slope) / det_max(math::cos(state.slope), 1.0e-3);
            // (Clamped rules only: under the air-time rules a hull flies off a downhill at speed and falls back onto it.)
            if flight.hug_downhill
                && climb_now < 0.0
                && det_max(downhill, -terrain_delta / det_max(horizontal_speed * dt, 1.0e-3))
                    <= limits.hover_cliff_slope
            {
                state.vertical_vel = det_min(state.vertical_vel, climb_now);
            }
            let fall = det_max(gravity, limits.crest_fall_min_m_s2)
                * if state.ground_climb < 0.0 {
                    flight.crest_down_force
                } else {
                    1.0
                };
            if flight.fall_damped {
                // Air time: spring and damper together, never pulling down harder than `fall` nor pushing up harder than the
                // fans.
                let mut acc = det_max(
                    params.height_spring * altitude_error - damp * state.vertical_vel,
                    -fall,
                );
                if flight.lift_accel_max > 0.0 {
                    acc = det_min(acc, flight.lift_accel_max);
                }
                state.vertical_vel += acc * dt;
            } else {
                let pull = det_max(params.height_spring * altitude_error, -fall);
                let soft = det_clamp(1.0 + altitude_error / limits.crest_soft_band_m, 0.0, 1.0);
                state.vertical_vel += (pull - damp * soft * state.vertical_vel) * dt;
            }
        }
    }
    // Auto-level OFF in the air: the share of the thrust along the hull's tilted axes. Pushing down is free; pushing up is
    // capped below gravity (`limits.air_lift_max_g`), so a pitched-up hull stretches a jump and dives on a pitch-down, but never flies.
    if air_lift != 0.0 {
        let cap = limits.air_lift_max_g * det_max(gravity, 0.0) * dt;
        state.vertical_vel += det_min(air_lift, cap);
    }
    state.altitude += state.vertical_vel * dt;

    // --- Ground collision (altitude is AGL, so the ground plane is 0) ---
    // Air time: a hull the ground pushes up keeps the upward speed it already has; the clamped rule zeroes it every contact.
    if state.altitude <= 0.0 {
        state.altitude = 0.0;
        state.vertical_vel = if flight.contact_keeps_lift {
            det_max(state.vertical_vel, 0.0)
        } else {
            0.0
        };
        state.airborne = false;
        state.jump_active = false;
    }

    // Mirror vertical velocity into the wire-visible velocity vector.
    state.velocity[1] = state.vertical_vel;
}

/// Apply a jump impulse. Not driven by the input array (there is no jump
/// axis); the host calls this when a jump command arrives. No-op while
/// already airborne.
pub fn apply_jump(state: &mut HoverState, params: &HoverParams) {
    if !state.airborne {
        state.vertical_vel += params.jump_speed;
        state.airborne = true;
        state.jump_active = true;
    }
}

/// The default hull hop apex above its hover altitude, metres, on every world
/// ([`HoverLimits::hop_apex_m`] of [`HoverLimits::DEFAULT`]).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.hop_apex_m` or pass your own `HoverLimits`"
)]
pub const HOP_APEX_M: f32 = HoverLimits::DEFAULT.hop_apex_m;
/// The default seconds between hull hops ([`HoverLimits::hop_cooldown_s`] of [`HoverLimits::DEFAULT`]).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.hop_cooldown_s` or pass your own `HoverLimits`"
)]
pub const HOP_COOLDOWN_S: f32 = HoverLimits::DEFAULT.hop_cooldown_s;
/// The default share of downward speed a falling hull keeps on a hop press
/// ([`HoverLimits::hop_land_keep`] of [`HoverLimits::DEFAULT`]).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.hop_land_keep` or pass your own `HoverLimits`"
)]
pub const HOP_LAND_KEEP: f32 = HoverLimits::DEFAULT.hop_land_keep;

/// [`hop_launch_speed_for`] under [`HoverLimits::DEFAULT`] (a 3 m apex) on a world of this gravity
/// and air density (an air of 0 is vacuum). A gravity that is not finite and above 0 reads as
/// [`GRAVITY`]; a `dt` that is not finite and above 0 as 1/60 s.
pub fn hop_launch_speed(gravity: f32, air: f32, dt: f32) -> f32 {
    let world = WorldPhysics {
        gravity_m_s2: gravity,
        air_density_kg_m3: air,
        vacuum: false,
        low_g: LowG::Governor,
    };
    hop_launch_speed_for(&HoverLimits::DEFAULT, &world, dt)
}

/// Launch speed (m/s) that peaks [`HoverLimits::hop_apex_m`] above the launch point under the
/// world's jump-feel gravity ([`HoverLimits::jump_gravity`]) and air drag
/// ([`WorldPhysics::drag_air`]). Deterministic: bisection over the same ascent step the sim flies
/// ([`crate::ballistic::step_debris`]). A gravity that is not finite and above 0 reads as
/// [`GRAVITY`]; a `dt` that is not finite and above 0 as 1/60 s.
///
/// A bounded tool, not a per-tick call: 24 bisection steps, each flying a whole ascent (up to
/// 20,000 steps). Call it once per world (limits, gravity, air, tick rate) and cache the result;
/// never per tick or per hull.
pub fn hop_launch_speed_for(limits: &HoverLimits, world: &WorldPhysics, dt: f32) -> f32 {
    let gravity = world.gravity_m_s2;
    let air = world.drag_air();
    let g = if gravity.is_finite() && gravity > 0.0 {
        gravity
    } else {
        GRAVITY
    };
    let g = limits.jump_gravity(g);
    let dt = if dt.is_finite() && dt > 0.0 {
        dt
    } else {
        1.0 / 60.0
    };
    let apex_of = |v0: f32| -> f32 {
        let mut vel = [0.0, v0, 0.0];
        let mut y = 0.0_f32;
        let mut top = 0.0_f32;
        for _ in 0..20_000 {
            vel = crate::ballistic::step_debris(vel, g, air, dt);
            y += vel[1] * dt;
            top = det_max(top, y);
            if vel[1] <= 0.0 {
                break;
            }
        }
        top
    };
    let apex = limits.hop_apex_m;
    let mut lo = 0.0_f32;
    let mut hi = math::sqrt(2.0 * g * apex) * 4.0 + 4.0;
    for _ in 0..24 {
        let mid = 0.5 * (lo + hi);
        if apex_of(mid) < apex {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// [`apply_hull_hop_with`] under [`HoverLimits::DEFAULT`] (0.8 s cooldown, a falling press keeps a
/// quarter of the drop).
pub fn apply_hull_hop(state: &mut HoverState, launch_speed: f32) -> bool {
    apply_hull_hop_with(&HoverLimits::DEFAULT, state, launch_speed)
}

/// Fan-pulse hop. `launch_speed` comes from [`hop_launch_speed_for`] with the same `limits`. Climbs
/// from the ground and is refused while the cooldown runs; firing starts a cooldown of
/// [`HoverLimits::hop_cooldown_s`]. While falling, a press keeps [`HoverLimits::hop_land_keep`] of
/// the drop so the hull lands softly. Returns whether the hop fired.
pub fn apply_hull_hop_with(limits: &HoverLimits, state: &mut HoverState, launch_speed: f32) -> bool {
    if state.hop_cooldown > 0.0 {
        return false;
    }
    if state.airborne {
        if state.vertical_vel < 0.0 {
            state.vertical_vel *= limits.hop_land_keep;
            state.velocity[1] = state.vertical_vel;
            state.hop_cooldown = limits.hop_cooldown_s;
            return true;
        }
        return false;
    }
    state.vertical_vel = launch_speed;
    state.velocity[1] = state.vertical_vel;
    state.airborne = true;
    state.jump_active = true;
    state.hop_cooldown = limits.hop_cooldown_s;
    true
}

/// Compute visual lean/tilt angles `(pitch, roll)` from physics state.
/// These are renderer-only transforms, not gameplay state.
///
/// Pitch combines throttle input (`lean_pitch_per_throttle`) with forward speed as a
/// fraction of `max_forward_speed` (`lean_pitch_per_speed`); roll combines strafe input
/// (`lean_roll_per_strafe`) with yaw rate (`lean_roll_per_yaw_rate`) and lateral slip (`roll_slip`).
pub fn compute_lean(params: &HoverParams, state: &HoverState, input: &[f32; 3]) -> (f32, f32) {
    let [throttle, strafe, _steer] = *input;

    // Forward speed component (velocity projected onto facing), normalized.
    let fwd_speed = state.velocity[0] * math::sin(state.yaw) + state.velocity[2] * math::cos(state.yaw);
    let speed_frac = if params.max_forward_speed > 0.0 {
        det_clamp(fwd_speed / params.max_forward_speed, -1.0, 1.0)
    } else {
        0.0
    };

    let pitch = throttle * params.lean_pitch_per_throttle + speed_frac * params.lean_pitch_per_speed;
    let lat_speed = state.velocity[0] * math::cos(state.yaw) - state.velocity[2] * math::sin(state.yaw);
    let roll = strafe * params.lean_roll_per_strafe
        + state.yaw_rate * params.lean_roll_per_yaw_rate
        + lat_speed * params.roll_slip;
    (pitch, roll)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_limits;
