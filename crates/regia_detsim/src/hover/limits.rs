// SPDX-License-Identifier: MPL-2.0
//! The hover model's limits as data (ADR-8349): [`HoverLimits`].
//!
//! The hop, the air steer, the thresholds that decide when a hull is in the air or under thrust, and
//! the crest, lookahead, grade and attitude limits used to be `pub const`s (and private constants)
//! a game could not change. They are the fields of one value now, passed to
//! [`step_with`](super::step_with); [`HoverLimits::DEFAULT`] holds the numbers the constants held,
//! so a host that passes it steps bit for bit as before. The old public constants stay one release
//! as deprecated aliases of the `DEFAULT` fields.
//!
//! A hull's own numbers are [`HoverParams`](super::HoverParams) and a match's climb and flight rules
//! are [`ClimbTuning`](super::ClimbTuning); the limits are the third layer, the model itself. A game
//! (a racing hover, a sports hover, a light hovercraft) passes its own once per match: every peer must
//! step the same limits, so they belong in the settings the lockstep handshake compares: bytes 181 to
//! 272 of [`hull_handshake_bytes`](super::hull_handshake_bytes).

use crate::float::det_max;

use super::validate::{FieldRange, RATE_MIN, check_all, sanitize_all};
use super::{HoverParams, HoverParamsError, HoverState};

/// A [`HoverLimits`] table row (the hover validator's `field_range!` for this type).
macro_rules! limit_range {
    ($name:literal, $field:ident, $min:expr, $max:expr) => {
        limit_range!($name, $field, $min, $max, false)
    };
    ($name:literal, $field:ident, $min:expr, $max:expr, rate) => {
        limit_range!($name, $field, $min, $max, true)
    };
    ($name:literal, $field:ident, $min:expr, $max:expr, $rate:expr) => {
        FieldRange::<HoverLimits> {
            name: $name,
            min: $min,
            max: $max,
            rate: $rate,
            get: |p: &HoverLimits| p.$field,
            set: |p: &mut HoverLimits, v: f32| p.$field = v,
        }
    };
}

/// The hover model's limits (ADR-8349): every number of the step that is neither a hull field
/// ([`HoverParams`]) nor a match climb field ([`ClimbTuning`](super::ClimbTuning)).
///
/// Each field's doc gives its unit and valid range ([`HoverLimits::FIELD_RANGES`]). A value from
/// user content is checked with [`HoverLimits::validate`] and stepped as
/// [`HoverLimits::sanitized`]; the step itself does not validate.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HoverLimits {
    /// Apex of a hull hop above its launch altitude, metres, on every world: the height
    /// [`hop_launch_speed_for`](super::hop_launch_speed_for) solves the launch speed for.
    /// Valid: 0 to 100.
    pub hop_apex_m: f32,
    /// Seconds between hull hops: [`apply_hull_hop_with`](super::apply_hull_hop_with) sets
    /// [`HoverState::hop_cooldown`] to it and refuses a press while it runs.
    /// Valid: 0 to 60.
    pub hop_cooldown_s: f32,
    /// Share of the downward speed a falling hull keeps when the hop key is pressed (a soft landing).
    /// Valid: 0 to 1.
    pub hop_land_keep: f32,
    /// Floor of the gravity an explicit hop or jump arc falls under, m/s2: on a world lighter than
    /// this a hop rises and falls under this value instead (a shorter hang time for the same apex);
    /// other ballistics keep the world's gravity.
    /// Valid: 0 to 1000.
    pub jump_feel_gravity_min: f32,
    /// Yaw-rate tracking rate of the modern aim preset ([`HoverState::yaw_modern`]), 1/s, at every
    /// speed, ramp-in and release alike.
    /// Valid: 0 to 1000.
    pub aim_modern_yaw_alpha: f32,
    /// Lateral grip while driving with the steer quiet, 1/s: the floor under
    /// [`HoverParams::lat_grip`], so the slip a turn leaves behind dies out once the steer is released.
    /// Valid: 0 to 1000.
    pub lat_straighten_grip: f32,
    /// Share of the commanded yaw rate a hull keeps while airborne (no ground to push against).
    /// Valid: 0 to 10.
    pub airborne_steer_authority: f32,
    /// Height over the ride height under which a descending airborne hull rejoins the hover spring,
    /// metres.
    /// Valid: 0 to 100.
    pub airborne_eps_m: f32,
    /// Throttle or strafe magnitude above which the under-thrust drag
    /// ([`HoverParams::thrust_drag`]) applies instead of the coast drag.
    /// Valid: 0 to 1.
    pub drag_input_threshold: f32,
    /// Floor of the fall acceleration over a crest and of the gravity the slope pull reads, m/s2, so
    /// a low-g world still settles the hull promptly.
    /// Valid: 0 to 1000.
    pub crest_fall_min_m_s2: f32,
    /// Height over the hover aim over which the damper fades back in under the hard-landing rules
    /// ([`FlightRules::fall_damped`](super::FlightRules::fall_damped) off), metres. Must be above 0.
    /// Valid: above 0, at most 100 (0 is `zero_rate`).
    pub crest_soft_band_m: f32,
    /// Steepest slope the along-slope pull reads, as the sine of its angle.
    /// Valid: 0 to 1.
    pub slope_sin_max: f32,
    /// Smoothing time of the ground climb rate the hover lookahead reads, seconds. Must be above 0.
    /// Valid: above 0, at most 100 (0 is `zero_rate`).
    pub hover_lead_smooth_s: f32,
    /// Steepest downhill (rise over run) a hull hugs under the hug-downhill rule
    /// ([`FlightRules::hug_downhill`](super::FlightRules::hug_downhill)); over a steeper face (a cliff)
    /// it falls. The default 1.25 is about 51 degrees.
    /// Valid: 0 to 1000.
    pub hover_cliff_slope: f32,
    /// Smoothing time of the slope estimate ([`HoverState::slope`]), seconds. Must be above 0.
    /// Valid: above 0, at most 100 (0 is `zero_rate`).
    pub grade_smooth_s: f32,
    /// A ground change bigger than this in one tick, metres, is a teleport, a respawn or a ledge, not
    /// a slope: the slope estimate ignores it.
    /// Valid: 0 to 10000.
    pub grade_teleport_m: f32,
    /// Bleed of a speed over the hull's caps (an expired boost, a drift plus thrust), as a share of
    /// [`HoverParams::thrust_accel`] per second.
    /// Valid: 0 to 100.
    pub overspeed_bleed: f32,
    /// Height over the ride height past which a hull counts as in the air for the attitude rules
    /// (auto-level, the softer compressed cushion), metres.
    /// Valid: 0 to 100.
    pub air_attitude_m: f32,
    /// Auto-level off in the air: most upward push the tilted thrust may give, as a share of gravity.
    /// Under 1 a pitched-up hull stretches a jump but cannot fly.
    /// Valid: 0 to 10.
    pub air_lift_max_g: f32,
    /// Auto-level off in the air: roll rate at full strafe, rad/s.
    /// Valid: 0 to 50.
    pub free_roll_rate_rad_s: f32,
    /// Auto-level off in the air: how fast the roll rate follows the strafe, 1/s.
    /// Valid: 0 to 1000.
    pub free_roll_track: f32,
    /// Largest lean (strafe lean plus turn bank plus ground roll) the cushion holds, radians.
    /// Valid: 0 to pi.
    pub roll_lean_max: f32,
    /// Largest cushion pitch ([`HoverState::cushion_pitch`]), radians.
    /// Valid: 0 to pi/2.
    pub cushion_pitch_max: f32,
}

impl HoverLimits {
    /// The model's limits as the step has always flown them: the values of the former constants,
    /// bit for bit. [`step`](super::step), [`step_world`](super::step_world) and
    /// [`step_tuned`](super::step_tuned) step these; a change to one of them is a sim-version change
    /// (ADR-8019), so a game that wants other numbers passes its own value to
    /// [`step_with`](super::step_with) instead.
    ///
    /// - **Hop.** A 3 m apex on every world, 0.8 s between hops, a press while falling keeps a
    ///   quarter of the drop; hop and jump arcs fall under at least 6 m/s2.
    /// - **Steering.** The modern aim tracks at 25/s (a 40 ms time constant, about two ticks at
    ///   60 Hz); a hull driving with the steer quiet grips at 25/s; in the air the yaw command is
    ///   0.4 of the ground one.
    /// - **Thresholds.** An airborne hull rejoins the spring 0.25 m over its ride height; an axis
    ///   over 0.1 is thrust.
    /// - **Crest, slope and grade.** Falls and the slope pull read at least 6 m/s2; the damper
    ///   fades in over 0.5 m under the hard-landing rules; the slope pull reads at most a
    ///   35 degree slope (sine 0.574); the climb rate smooths over 0.25 s and the slope over
    ///   0.12 s; a 4 m step in one tick is no slope; a downhill steeper than 1.25 is a cliff;
    ///   overspeed bleeds at 1.2 thrust accelerations.
    /// - **Attitude.** 1 m over the ride height is in the air; with auto-level off the tilted thrust
    ///   lifts at most 0.6 g and the strafe rolls at 2.5 rad/s, tracked at 6/s; the cushion leans
    ///   and pitches at most 0.35 rad (20 degrees).
    pub const DEFAULT: HoverLimits = HoverLimits {
        hop_apex_m: 3.0,
        hop_cooldown_s: 0.8,
        hop_land_keep: 0.25,
        jump_feel_gravity_min: 6.0,
        aim_modern_yaw_alpha: 25.0,
        lat_straighten_grip: 25.0,
        airborne_steer_authority: 0.4,
        airborne_eps_m: 0.25,
        drag_input_threshold: 0.1,
        crest_fall_min_m_s2: 6.0,
        crest_soft_band_m: 0.5,
        slope_sin_max: 0.574,
        hover_lead_smooth_s: 0.25,
        hover_cliff_slope: 1.25,
        grade_smooth_s: 0.12,
        grade_teleport_m: 4.0,
        overspeed_bleed: 1.2,
        air_attitude_m: 1.0,
        air_lift_max_g: 0.6,
        free_roll_rate_rad_s: 2.5,
        free_roll_track: 6.0,
        roll_lean_max: 0.35,
        cushion_pitch_max: 0.35,
    };

    /// Every field with its valid range, in declaration order. The ranges are also in each field's
    /// doc comment.
    pub const FIELD_RANGES: &'static [FieldRange<HoverLimits>] = &[
        limit_range!("hop_apex_m", hop_apex_m, 0.0, 100.0),
        limit_range!("hop_cooldown_s", hop_cooldown_s, 0.0, 60.0),
        limit_range!("hop_land_keep", hop_land_keep, 0.0, 1.0),
        limit_range!("jump_feel_gravity_min", jump_feel_gravity_min, 0.0, 1000.0),
        limit_range!("aim_modern_yaw_alpha", aim_modern_yaw_alpha, 0.0, 1000.0),
        limit_range!("lat_straighten_grip", lat_straighten_grip, 0.0, 1000.0),
        limit_range!("airborne_steer_authority", airborne_steer_authority, 0.0, 10.0),
        limit_range!("airborne_eps_m", airborne_eps_m, 0.0, 100.0),
        limit_range!("drag_input_threshold", drag_input_threshold, 0.0, 1.0),
        limit_range!("crest_fall_min_m_s2", crest_fall_min_m_s2, 0.0, 1000.0),
        limit_range!("crest_soft_band_m", crest_soft_band_m, RATE_MIN, 100.0, rate),
        limit_range!("slope_sin_max", slope_sin_max, 0.0, 1.0),
        limit_range!("hover_lead_smooth_s", hover_lead_smooth_s, RATE_MIN, 100.0, rate),
        limit_range!("hover_cliff_slope", hover_cliff_slope, 0.0, 1000.0),
        limit_range!("grade_smooth_s", grade_smooth_s, RATE_MIN, 100.0, rate),
        limit_range!("grade_teleport_m", grade_teleport_m, 0.0, 10000.0),
        limit_range!("overspeed_bleed", overspeed_bleed, 0.0, 100.0),
        limit_range!("air_attitude_m", air_attitude_m, 0.0, 100.0),
        limit_range!("air_lift_max_g", air_lift_max_g, 0.0, 10.0),
        limit_range!("free_roll_rate_rad_s", free_roll_rate_rad_s, 0.0, 50.0),
        limit_range!("free_roll_track", free_roll_track, 0.0, 1000.0),
        limit_range!("roll_lean_max", roll_lean_max, 0.0, core::f32::consts::PI),
        limit_range!(
            "cushion_pitch_max",
            cushion_pitch_max,
            0.0,
            core::f32::consts::FRAC_PI_2
        ),
    ];

    /// `Ok` when every field is finite and inside its documented range
    /// ([`HoverLimits::FIELD_RANGES`]); otherwise the first bad field, in declaration order, with the
    /// same error codes as [`HoverParams::validate`]. Call it when the limits are loaded (a match
    /// setting, a mod), never per tick. [`HoverLimits::DEFAULT`] is valid.
    pub fn validate(&self) -> Result<(), HoverParamsError> {
        check_all(Self::FIELD_RANGES, self)
    }

    /// The limits with every field made valid: a non-finite field takes the
    /// [`HoverLimits::DEFAULT`] value, a value outside its range is clamped to the nearer bound (a
    /// zero or negative smoothing time to its smallest valid value). Valid limits come back bit for
    /// bit; the result always passes [`HoverLimits::validate`].
    pub fn sanitized(&self) -> HoverLimits {
        sanitize_all(Self::FIELD_RANGES, self, &HoverLimits::DEFAULT)
    }

    /// Gravity an explicit hop or jump arc falls under on a world of gravity `map_gravity`: the
    /// world's, floored at [`HoverLimits::jump_feel_gravity_min`].
    pub fn jump_gravity(&self, map_gravity: f32) -> f32 {
        det_max(map_gravity, self.jump_feel_gravity_min)
    }

    /// True while the hull is off its cushion: after a hop, or riding more than
    /// [`HoverLimits::air_attitude_m`] over its hover height.
    pub fn hull_in_air(&self, params: &HoverParams, state: &HoverState) -> bool {
        params.ride_height > 0.0
            && (state.airborne || state.altitude > params.ride_height + self.air_attitude_m)
    }
}

impl Default for HoverLimits {
    fn default() -> Self {
        HoverLimits::DEFAULT
    }
}
