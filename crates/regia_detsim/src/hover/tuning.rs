// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/handling.rs, commit d6ed37677), ADR-8012.
// Mark: adapt. Lines 494-573: the model limits `hover.rs` imports that DO27 did not measure (crest, lookahead, ride
// ceiling, cliff, grade, overspeed, air-attitude and free-roll limits), values and arithmetic unchanged. Named changes:
// the doc comments no longer cite DO27's files; the hull class table above them (lines 1-493) stays in DO27; and every
// constant DO27's text says was measured from a baseline game (the lift cap, the fan grip, the release-drop time
// constant, the roll spring and the cushion constants) is not copied (ADR-8012 A-8, P4; ADR-8164): those are
// `HoverParams` / `ClimbTuning` / `FlightRules` fields with Regia's own defaults. ADR-8349: the limits the step reads
// directly are `HoverLimits` fields now (`HoverLimits::DEFAULT` holds these values); the constants here stay one release as
// deprecated aliases, and the four that only seed `ClimbTuning` / `FlightRules` defaults stay as they are.

//! The hover model's default limits (ADR-8079, ADR-8164, ADR-8349).
//!
//! The limits the step reads are the fields of [`HoverLimits`](super::HoverLimits) now, and
//! [`HoverLimits::DEFAULT`](super::HoverLimits::DEFAULT) holds the values below; the deprecated
//! constants are aliases of its fields for one release. The four constants that are not deprecated
//! seed the defaults of [`ClimbTuning`](super::ClimbTuning) and
//! [`FlightRules`](super::FlightRules), which a host already passes per match. The engine's hover
//! goldens and the ported tests pin the defaults: a change to one is a sim-version change (ADR-8019).

/// Fall acceleration over a crest, floored so a low-g world still settles the hull promptly.
/// The default of [`HoverLimits::crest_fall_min_m_s2`](super::HoverLimits::crest_fall_min_m_s2).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.crest_fall_min_m_s2` or pass your own `HoverLimits`"
)]
pub const CREST_FALL_MIN_M_S2: f32 = super::HoverLimits::DEFAULT.crest_fall_min_m_s2;
/// Height above hover height over which the full hover-spring damping fades out.
/// The default of [`HoverLimits::crest_soft_band_m`](super::HoverLimits::crest_soft_band_m).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.crest_soft_band_m` or pass your own `HoverLimits`"
)]
pub const CREST_SOFT_BAND_M: f32 = super::HoverLimits::DEFAULT.crest_soft_band_m;
/// Steepest slope (sine, 35 degrees) the along-slope pull reads.
/// The default of [`HoverLimits::slope_sin_max`](super::HoverLimits::slope_sin_max).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.slope_sin_max` or pass your own `HoverLimits`"
)]
pub const SLOPE_SIN_MAX: f32 = super::HoverLimits::DEFAULT.slope_sin_max;
/// Climb rates beyond this (m/s) are ignored by the hover lookahead (teleports, cliffs).
pub const HOVER_LEAD_CLIMB_MAX: f32 = 30.0;
/// Most the lookahead may lift the hover aim above hover height, metres.
pub const HOVER_LEAD_MAX_RISE_M: f32 = 25.0;
/// Smoothing time of the ground climb rate the lookahead reads, seconds.
/// The default of [`HoverLimits::hover_lead_smooth_s`](super::HoverLimits::hover_lead_smooth_s).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.hover_lead_smooth_s` or pass your own `HoverLimits`"
)]
pub const HOVER_LEAD_SMOOTH_S: f32 = super::HoverLimits::DEFAULT.hover_lead_smooth_s;
/// Most height above hover height a spring-driven hull may climb under the clamped flight rules,
/// metres: small hops over crests, never a launch. The lookahead used to lift the aim up to
/// [`HOVER_LEAD_MAX_RISE_M`] on a steep ramp and the spring built the speed to follow it, so a hull
/// left a steep ramp's top 15 to 21 m over the plain. A hull already higher (the ground fell away)
/// is not pulled down by this; it falls.
pub const HOVER_RIDE_MAX_M: f32 = 2.0;
/// Fall acceleration over a falling crest as a multiple of the usual fall, under the clamped flight
/// rules: the fans push the hull back onto the ground.
pub const CREST_DOWN_FORCE: f32 = 2.5;
/// A downhill face steeper than this (rise over run, about 51 degrees)
/// is a cliff. Under the clamped flight rules a hull over a gentler one takes on the ground's
/// descent and hugs it; over a cliff it falls.
/// The default of [`HoverLimits::hover_cliff_slope`](super::HoverLimits::hover_cliff_slope).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.hover_cliff_slope` or pass your own `HoverLimits`"
)]
pub const HOVER_CLIFF_SLOPE: f32 = super::HoverLimits::DEFAULT.hover_cliff_slope;
/// Smoothing time of the slope estimate, seconds.
/// The default of [`HoverLimits::grade_smooth_s`](super::HoverLimits::grade_smooth_s).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.grade_smooth_s` or pass your own `HoverLimits`"
)]
pub const GRADE_SMOOTH_S: f32 = super::HoverLimits::DEFAULT.grade_smooth_s;
/// A ground jump bigger than this in one tick (metres) is a teleport or a respawn, not a slope.
/// The default of [`HoverLimits::grade_teleport_m`](super::HoverLimits::grade_teleport_m).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.grade_teleport_m` or pass your own `HoverLimits`"
)]
pub const GRADE_TELEPORT_M: f32 = super::HoverLimits::DEFAULT.grade_teleport_m;
/// Overspeed bleed as a share of the thrust acceleration (expired boost, drift plus thrust).
/// The default of [`HoverLimits::overspeed_bleed`](super::HoverLimits::overspeed_bleed).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.overspeed_bleed` or pass your own `HoverLimits`"
)]
pub const OVERSPEED_BLEED: f32 = super::HoverLimits::DEFAULT.overspeed_bleed;
/// Height over hover height past which a hull counts as in the air for the attitude (auto-level)
/// rules, metres.
/// The default of [`HoverLimits::air_attitude_m`](super::HoverLimits::air_attitude_m).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.air_attitude_m` or pass your own `HoverLimits`"
)]
pub const AIR_ATTITUDE_M: f32 = super::HoverLimits::DEFAULT.air_attitude_m;
/// Auto-level OFF in the air: most upward push the tilted thrust may give, as a share of gravity.
/// Under 1, so a hull pitched up stretches a jump but cannot fly.
/// The default of [`HoverLimits::air_lift_max_g`](super::HoverLimits::air_lift_max_g).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.air_lift_max_g` or pass your own `HoverLimits`"
)]
pub const AIR_LIFT_MAX_G: f32 = super::HoverLimits::DEFAULT.air_lift_max_g;
/// Auto-level OFF in the air: roll rate at full strafe, rad/s (a full roll in about 2.5 s).
/// The default of [`HoverLimits::free_roll_rate_rad_s`](super::HoverLimits::free_roll_rate_rad_s).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.free_roll_rate_rad_s` or pass your own `HoverLimits`"
)]
pub const FREE_ROLL_RATE_RAD_S: f32 = super::HoverLimits::DEFAULT.free_roll_rate_rad_s;
/// How fast the free roll rate follows the strafe keys, 1/s.
/// The default of [`HoverLimits::free_roll_track`](super::HoverLimits::free_roll_track).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.free_roll_track` or pass your own `HoverLimits`"
)]
pub const FREE_ROLL_TRACK: f32 = super::HoverLimits::DEFAULT.free_roll_track;
/// Largest lean the cushion holds (strafe plus turn bank), radians (20 degrees).
/// The default of [`HoverLimits::roll_lean_max`](super::HoverLimits::roll_lean_max).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.roll_lean_max` or pass your own `HoverLimits`"
)]
pub const ROLL_LEAN_MAX: f32 = super::HoverLimits::DEFAULT.roll_lean_max;
/// Most cushion pitch, radians (20 degrees, as the roll lean).
/// The default of [`HoverLimits::cushion_pitch_max`](super::HoverLimits::cushion_pitch_max).
#[deprecated(
    since = "0.2.23",
    note = "a model limit as data (ADR-8349): read `HoverLimits::DEFAULT.cushion_pitch_max` or pass your own `HoverLimits`"
)]
pub const CUSHION_PITCH_MAX: f32 = super::HoverLimits::DEFAULT.cushion_pitch_max;
