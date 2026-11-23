// SPDX-License-Identifier: MPL-2.0
//! Caller-configured procedural presentation gait for any leg count and pattern (ADR-8060,
//! ADR-8653).
//!
//! Portions adapted from Direct Order 27 `crates/ir-sim/src/gait.rs`, d6ed37677
//! (regia-absorb-1), under the owner's 2026-11-16 copy exception (ADR-8012). Game content and
//! tuning values are excluded. See `docs/ported-sources.toml` and `docs/provenance.md`.
//!
//! A [`GaitPattern`] (ADR-8653) says which feet are in stance at each point of the stride:
//! per-foot phase offsets and a duty factor, for 1 to [`MAX_LEGS`] feet. Presets cover the
//! six-leg alternating tripod (the original stepper, bit for bit), ripple and metachronal
//! wave gaits, the quadruped walk, trot, pace and bound, and the biped walk. A
//! [`GaitPatternSet`] switches patterns by ground speed with hysteresis, blending offsets and
//! duty over one stride so no foot jumps. [`GaitState::stability_margin`] reports the static
//! stability margin of the planted feet.
//!
//! Two entry points share one feet core:
//!
//! - [`step_gait_driven`] (driven mode, ADR-8474): the host's simulation, physics mover or
//!   navigation owns the body. Its position, yaw and velocity are copied bit for bit into the
//!   state; the stepper only advances phase from ground speed, places feet and follows stance
//!   height. Use this whenever gameplay moves the unit.
//! - [`step_gait`] (integrating mode): the stepper integrates the body from controls itself.
//!   Kept for toys, previews and tests.
//!
//! | quantity | integrating ([`step_gait`]) | driven ([`step_gait_driven`]) |
//! |---|---|---|
//! | position, velocity | world meters, +Y up | world meters, +Y up (copied; body Y ignored) |
//! | yaw | rotation about +Y | rotation about +Y (same sense) |
//! | local forward | **+Z**: forward is `[sin(yaw), 0, cos(yaw)]` | **-Z** (object convention): forward is `[-sin(yaw), 0, -cos(yaw)]` |
//! | `hip_offsets[i]` | `[+X, +Z]` local | `[right (+X), forward (-Z)]` local |
//!
//! Both modes turn by the same yaw about +Y; only the local forward axis differs. Offsets
//! authored for [`step_gait`] give the same world feet in driven mode, at the same yaw
//! values, after negating each offset's second component. In Regia's right-handed world a
//! body facing +Z has its right side at -X, so the integrating mode's foot names are
//! mirrored; the preset leg names hold in driven mode.
//!
//! Below `idle_speed` both modes settle the feet by stepping and turn in place by stepping
//! (ADR-8539): no planted foot slides or snaps, and [`GaitState::settled`] reports rest.
//!
//! Posture (ADR-8706): [`GaitParams::posture`] eases body height toward a target from every
//! foot (feet in the air weighted by [`GaitPosture::swing_weight`]) under a slew limit, and
//! pitches and rolls the body with a least-squares plane through the planted feet
//! ([`GaitState::attitude`]); [`GaitPosture::UPRIGHT`] keeps the upright stepper bit for bit.
//!
//! Ground (ADR-8792) is any [`GaitGround`]: a plain `Fn(x, z) -> y` height closure, or a host
//! adapter that answers the layer near the walker (tunnels, bridges, stacked floors), a
//! surface normal, or no ground. Refused landings search ring footholds, a foot beyond its
//! `max_reach` takes a forced step, and a foot with no ground hangs instead of failing.
//!
//! This is presentation math, using Regia's deterministic trigonometry; it is not the
//! lockstep math pin and must never feed a simulation checksum. The host calls a stepper
//! with its own timestep and ground, then feeds the targets to its leg IK. No physics,
//! collision or scheduling is implied, and this crate never queries physics or terrain.

mod pattern;
mod posture;
mod stability;

pub use pattern::{
    GaitPattern, GaitPatternSet, MAX_DUTY_STEP, MAX_LEGS, MAX_PATTERNS, TRIPOD_LEGS, leg_array,
};
pub use posture::GaitPosture;
pub use stability::support_margin;

/// [`GaitState::stability_margin`] with no foot planted (flight phases, every foot
/// hanging): the most negative finite `f32`, so agents read a number and `lt`/`lte`
/// comparisons against it hold (ADR-8736). [`support_margin`] itself returns negative
/// infinity for no points.
pub const NO_SUPPORT_MARGIN: f32 = -f32::MAX;

use pattern::{EVENT_EPS, Schedule, offset_delta, wrap_unit};
use regia_core::math::detmath::det_sin_cos;
use regia_ecs::prelude::{Component, Reflect, ReflectComponent, bevy_ecs, bevy_reflect};

/// Number of feet of the six-leg tripod, ordered front-left/right, middle-left/right,
/// rear-left/right.
#[deprecated(
    since = "0.1.28",
    note = "patterns drive 1..=MAX_LEGS feet (ADR-8653): use TRIPOD_LEGS or GaitPattern::legs"
)]
pub const LEG_COUNT: usize = TRIPOD_LEGS;

/// Motion and geometry settings. All values are supplied by the host; no game tuning defaults.
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct GaitParams {
    /// Maximum forward speed in meters per second, nonnegative.
    pub forward_speed: f32,
    /// Maximum reverse speed (magnitude), nonnegative.
    pub reverse_speed: f32,
    /// Maximum lateral speed (magnitude), nonnegative.
    pub strafe_speed: f32,
    /// Acceleration and braking rate in meters per second squared, positive.
    pub acceleration: f32,
    /// Body travel in meters during one stance (a foot's stance spans `step_length` along
    /// the ground under the body), positive. For the tripod this is the travel per tripod
    /// period; a pattern's stride is `step_length / duty_factor`.
    pub step_length: f32,
    /// Body height above the stance floor in meters (the mean planted foot height with
    /// [`GaitPosture::UPRIGHT`]; see [`Self::posture`]), nonnegative.
    pub clearance: f32,
    /// Swing height above the line between lift-off and touchdown, nonnegative.
    pub swing_lift: f32,
    /// Maximum absolute rise/run allowed for a footstep, nonnegative.
    pub max_slope: f32,
    /// Ground speed at or below which the gait is idle: feet settle to neutral and turn in
    /// place by stepping (see `settle_distance`), nonnegative.
    pub idle_speed: f32,
    /// Maximum yaw rate in radians per second, nonnegative.
    pub max_yaw_rate: f32,
    /// Local `[right, forward]` hip positions in foot order; finite meters. Entries past
    /// the pattern's leg count are ignored (build with [`leg_array`]).
    pub hip_offsets: [[f32; 2]; MAX_LEGS],
    /// Idle, not turning: a foot farther than this from its neutral (the hip under the body),
    /// in meters on XZ, makes its lift-off group step there. Nonnegative.
    pub settle_distance: f32,
    /// Duration in seconds of one idle beat (one lift-off group's turn; one idle swing for
    /// the tripod), positive.
    pub settle_period: f32,
    /// Idle and turning: a planted foot farther than this from its rotated neutral, in meters
    /// on XZ, makes its group step there. Nonnegative.
    pub turn_step_distance: f32,
    /// Idle and turning: a planted foot whose direction from the body center differs from
    /// its rotated neutral's by more than this many radians makes its group step. In
    /// `[0, pi]`; `pi` leaves only `turn_step_distance`.
    pub turn_step_angle: f32,
    /// Half-height in meters of the band around the expected floor (body height minus
    /// `clearance`) that a ground answer must fall in (ADR-8792). A [`GaitGround`] with
    /// several layers (tunnel floor under terrain, deck under a bridge) answers the layer in
    /// this band; a height outside it counts as no ground. Positive; infinity accepts any
    /// height (single-layer worlds).
    pub ground_band: f32,
    /// Ring points tried around a refused landing before the foot is held, at most
    /// [`MAX_FOOTHOLD_CANDIDATES`]: 8 per ring, rings `foothold_spacing` apart. 0 disables the
    /// search.
    pub foothold_candidates: u32,
    /// Distance in meters between foothold search rings, positive and finite.
    pub foothold_spacing: f32,
    /// Per-leg reach in meters: the 3D distance from the hip (its XZ under the body, at body
    /// height) beyond which a planted foot is forced to step. Positive; infinity disables it.
    /// Entries past the pattern's leg count are ignored.
    pub max_reach: [f32; MAX_LEGS],
    /// How far in meters below the expected floor a foot with no ground hangs, nonnegative.
    pub hang_drop: f32,
    /// Gait patterns by ground speed (ADR-8653); [`GaitPatternSet::tripod`] is the six-leg
    /// stepper of ADR-8060. The set's leg count is the number of feet.
    pub patterns: GaitPatternSet,
    /// Body height smoothing and pitch/roll (ADR-8706); [`GaitPosture::UPRIGHT`] is the
    /// upright stepper of ADR-8060 bit for bit.
    pub posture: GaitPosture,
}

/// Upper bound of [`GaitParams::foothold_candidates`]: per-step ground queries stay bounded.
pub const MAX_FOOTHOLD_CANDIDATES: u32 = 64;

impl GaitParams {
    /// Check settings before constructing or advancing a gait: [`GaitError::Pattern`] for an
    /// invalid pattern set, [`GaitError::Settings`] for everything else.
    pub fn validate(&self) -> Result<(), GaitError> {
        self.patterns.validate()?;
        let legs = self.legs();
        let nonnegative = [
            self.forward_speed,
            self.reverse_speed,
            self.strafe_speed,
            self.clearance,
            self.swing_lift,
            self.max_slope,
            self.idle_speed,
            self.max_yaw_rate,
            self.settle_distance,
            self.turn_step_distance,
        ];
        if nonnegative.iter().any(|x| !x.is_finite() || *x < 0.0)
            || !self.acceleration.is_finite()
            || self.acceleration <= 0.0
            || !self.step_length.is_finite()
            || self.step_length <= 0.0
            || self.hip_offsets[..legs].iter().flatten().any(|x| !x.is_finite())
            || !self.settle_period.is_finite()
            || self.settle_period <= 0.0
            || !(0.0..=core::f32::consts::PI).contains(&self.turn_step_angle)
            || self.ground_band.is_nan()
            || self.ground_band <= 0.0
            || self.foothold_candidates > MAX_FOOTHOLD_CANDIDATES
            || !self.foothold_spacing.is_finite()
            || self.foothold_spacing <= 0.0
            || self.max_reach[..legs].iter().any(|r| r.is_nan() || *r <= 0.0)
            || !self.hang_drop.is_finite()
            || self.hang_drop < 0.0
            || !self.posture.is_valid()
        {
            return Err(GaitError::Settings);
        }
        Ok(())
    }

    /// Number of feet: the pattern set's leg count, at most [`MAX_LEGS`].
    #[must_use]
    pub fn legs(&self) -> usize {
        self.patterns.leg_count().min(MAX_LEGS)
    }

    /// Duration of one stance in seconds (the tripod period); infinity at idle. Validate
    /// settings before calling.
    #[must_use]
    pub fn step_period_at_speed(&self, speed: f32) -> f32 {
        if speed <= self.idle_speed {
            f32::INFINITY
        } else {
            self.step_length / speed
        }
    }
}

/// A foot's world target and contact flag.
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct FootTarget {
    /// World `[x, y, z]` in meters.
    pub position: [f32; 3],
    /// Whether the foot is a load-bearing plant.
    pub planted: bool,
    /// Unit ground normal at the foot (planted) or at its landing target (swinging), for
    /// foot tilt; `[0, 1, 0]` while hanging or on a height-only floor (ADR-8792).
    pub normal: [f32; 3],
}

/// A ground answer near a walker: the height of the layer that holds the foot and its
/// surface normal (any nonzero length; the stepper normalizes it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroundHit {
    /// World height in meters.
    pub y: f32,
    /// Surface normal, pointing out of the ground.
    pub normal: [f32; 3],
}

/// Where a foot can stand (ADR-8792). Hosts write adapters over their own ground: a
/// physics raycast down from `expected_y + band`, a terrain heightfield, a multi-layer
/// tunnel or floor query. The stepper never calls physics, terrain or world geometry itself.
///
/// `expected_y` is the walker's body height minus `clearance` (where its floor should be)
/// and `band` is [`GaitParams::ground_band`]: answer the layer nearest `expected_y` within
/// the band, so a walker in a tunnel gets the tunnel floor, not the terrain above. Return
/// `None` where there is no ground (a cliff edge, a hole, outside the world): that foot
/// searches for another foothold or hangs, never an error. A nonfinite height or normal is
/// a [`GaitError::Floor`]. Answers must be deterministic for identical inputs and should not
/// allocate (the stepper runs every frame).
///
/// Every `Fn(f32, f32) -> f32` height closure is a ground: always `Some`, normal `+Y`, the
/// band ignored by the closure (the stepper still applies it).
pub trait GaitGround {
    /// The ground under world `(x, z)` near `expected_y`, or `None` for no ground.
    fn sample(&self, x: f32, z: f32, expected_y: f32, band: f32) -> Option<GroundHit>;
}

impl<F: Fn(f32, f32) -> f32> GaitGround for F {
    fn sample(&self, x: f32, z: f32, _expected_y: f32, _band: f32) -> Option<GroundHit> {
        Some(GroundHit {
            y: self(x, z),
            normal: UP,
        })
    }
}

const UP: [f32; 3] = [0.0, 1.0, 0.0];

/// What a foot is doing apart from its pattern turn (ADR-8792); reflected for agents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum FootMode {
    /// Follows the pattern: planted in stance, swinging on its turn. (The name is kept from
    /// the six-leg tripod stepper.)
    #[default]
    Tripod,
    /// In an out-of-phase step on its own clock of `settle_period`: forced by `max_reach`,
    /// or finishing a swing that a pattern switch cut short when the gait went idle.
    Forced,
    /// Landed a forced step while its pattern turn still swings: planted until that turn
    /// ends.
    Waiting,
    /// No ground to stand on: hangs `hang_drop` below the expected floor under its hip,
    /// `planted == false`, until a later swing finds a foothold.
    Hanging,
}

/// Which entry point last advanced a [`GaitState`]; reflected for agents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum GaitMode {
    /// [`step_gait`]: the stepper integrates the body from controls (+Z forward).
    #[default]
    Integrating,
    /// [`step_gait_driven`]: the host supplies the body (object convention, -Z forward).
    Driven,
}

/// The body a host's simulation, physics mover or navigation computed for this step.
///
/// Object convention: +Y up, +X right, -Z forward, `yaw` turns about +Y, so forward is
/// `[-sin(yaw), 0, -cos(yaw)]`. Take `yaw` from your transform's rotation about +Y.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GaitBody {
    /// Body center in world meters. X and Z are copied; Y is ignored (the presentation
    /// body height follows the planted feet).
    pub position: [f32; 3],
    /// Heading in radians about +Y in the object convention; copied bit for bit.
    pub yaw: f32,
    /// World velocity in meters per second; copied bit for bit. Its XZ length is the
    /// ground speed that advances the gait phase.
    pub velocity: [f32; 3],
}

impl GaitBody {
    fn is_finite(&self) -> bool {
        self.position
            .iter()
            .chain(&self.velocity)
            .chain([&self.yaw])
            .all(|v| v.is_finite())
    }
}

/// Complete presentation state, reflected by [`crate::AnimationPlugin`].
///
/// Construct with [`Self::at_rest_driven`] for a simulation-driven body, or
/// [`Self::at_rest`] for the integrating mode. During stepping, planted XZ stays fixed
/// until that foot swings, walking or idle: below `idle_speed` feet settle and turn in
/// place by stepping, never by sliding (ADR-8539). Floor height may change each call.
/// The host owns origin rebasing: call [`Self::shift_origin`] for stored targets.
///
/// Per-foot arrays have [`MAX_LEGS`] entries; only the first `legs` are used (see
/// [`Self::feet`]), the rest stay at their defaults.
#[derive(Component, Clone, Copy, Debug, PartialEq, Reflect)]
#[reflect(Component, Clone)]
pub struct GaitState {
    /// Body center in world meters. Y follows the feet plus clearance (ADR-8706): eased
    /// toward [`Self::body_height_target`]; the mean planted height plus clearance with
    /// [`GaitPosture::UPRIGHT`].
    pub position: [f32; 3],
    /// Heading about +Y. Integrating mode: forward is `[sin(yaw), 0, cos(yaw)]`, wrapped to
    /// `(-pi, pi]`. Driven mode: the host's yaw bits, forward `[-sin(yaw), 0, -cos(yaw)]`.
    pub yaw: f32,
    /// World velocity in meters per second. Integrating mode resets Y to zero; driven
    /// mode holds the host's velocity bits.
    pub velocity: [f32; 3],
    /// Progress through the current beat, in `[0, 1)`. A beat is the interval between two
    /// consecutive lift-off groups of the pattern (ADR-8653); for the tripod it is the
    /// tripod period.
    pub phase: f32,
    /// True while the current beat belongs to foot 0's lift-off group. For the tripod: true
    /// when feet 0, 3, 4 swing, false when feet 1, 2, 5 swing.
    pub swing_a: bool,
    /// Freeze all state and skip floor queries while power is lost.
    pub power_lost: bool,
    /// Targets to pass to leg IK, in the pattern's foot order; the first `legs` are used.
    pub feet: [FootTarget; MAX_LEGS],
    /// The entry point that last advanced this state, which also fixes the yaw convention.
    pub mode: GaitMode,
    /// Neutral replants made instead of a step: driven body jumps longer than
    /// `step_length` and mode switches. Stays 0 over a continuous walk; saturates.
    pub replants: u32,
    /// Idle with no swing in progress and every foot within its settle threshold: the feet
    /// stay put (only following floor height) until the body moves or turns. False while a
    /// forced step is in progress.
    pub settled: bool,
    /// Footholds refused (ADR-8792): a landing or initial plant that found no valid foothold
    /// (no ground, outside `ground_band`, steeper than `max_slope`, beyond `max_reach`) at
    /// the nominal point or on any search ring, and planted feet whose ground disappeared.
    /// Saturates.
    pub refused: u32,
    /// Out-of-phase steps forced by `max_reach`. Stays 0 on a walk over valid ground with a
    /// reach longer than the stride; saturates.
    pub forced_steps: u32,
    /// Per-foot mode beside its pattern turn: forced, waiting or hanging.
    pub foot_modes: [FootMode; MAX_LEGS],
    /// Number of feet (ADR-8653), fixed at construction from the pattern set.
    pub legs: u8,
    /// Index of the current beat in the pattern's lift-off schedule (ADR-8653).
    pub beat: u8,
    /// The pattern in effect this step (mid-switch: the blend of the two).
    pub pattern: GaitPattern,
    /// Index in [`GaitParams::patterns`] of the pattern in use, or being switched to.
    pub pattern_index: u8,
    /// Whether a pattern switch is blending offsets and duty.
    pub blending: bool,
    /// Progress of the pattern switch in cycles, in `[0, 1)`; 0 when none is in progress.
    pub blend: f32,
    /// Each foot's pattern phase in `[0, 1)`: 0 at touchdown, the duty factor at lift-off.
    pub foot_phase: [f32; MAX_LEGS],
    /// Static stability margin in meters (ADR-8653): signed XZ distance from the body
    /// center to the support polygon of the planted feet, positive inside, negative outside
    /// (statically unstable), [`NO_SUPPORT_MARGIN`] (finite) with no foot planted. See
    /// [`support_margin`].
    pub stability_margin: f32,
    /// Body height the posture eases toward (ADR-8706), world meters: the weighted floor of
    /// the feet plus clearance (see [`GaitPosture::swing_weight`]). Held while no foot
    /// stands on or swings over ground.
    pub body_height_target: f32,
    /// Body pitch in radians after clamp, weight and ease (ADR-8706): positive raises the
    /// nose. 0 with [`GaitPosture::attitude_weight`] 0.
    pub pitch: f32,
    /// Body roll in radians after clamp, weight and ease (ADR-8706): positive raises the
    /// body's local +X side (feet with `hip_offsets[i][0] > 0`).
    pub roll: f32,
    /// Body tilt quaternion `[x, y, z, w]` in the stepper's local frame (ADR-8706): +X the
    /// local right axis, +Y up, +Z forward in integrating mode and back in driven mode. It
    /// is [`Self::pitch`] about +X, then [`Self::roll`] about the pitched forward axis.
    /// Apply it after the yaw: world rotation = yaw rotation x `attitude`. Exactly
    /// `[0, 0, 0, 1]` on level ground and with [`GaitPosture::attitude_weight`] 0.
    pub attitude: [f32; 4],
    // Reflected so inspection includes the entire state, including swing history.
    lift_off: [[f32; 3]; MAX_LEGS],
    // Travel heading on XZ: walking stores the unit travel direction; idle shrinks it to
    // zero. Forced steps land `step_length / 2` along it from their neutral.
    swing_heading: [f32; 2],
    // Progress of each foot's forced step in `[0, 1)`; 0 unless that foot is `Forced`.
    forced_phase: [f32; MAX_LEGS],
    // Whether each foot is in its pattern swing (lifted on its turn, not yet landed).
    swinging: [bool; MAX_LEGS],
    // Swing progress `[0, 1]` of each swinging foot.
    swing_progress: [f32; MAX_LEGS],
    // Scheduled swing progress `[0, 1)` at which each swinging foot lifted: 0 unless it
    // lifted late (a walk from rest with overlapping swings, ADR-8736); its arc then runs
    // from 0 to 1 over the rest of its scheduled swing.
    swing_start: [f32; MAX_LEGS],
    // Per-foot landing heading: the travel heading, shrunk per foot at idle so each swing
    // lands at neutral at its own touchdown.
    leg_heading: [[f32; 2]; MAX_LEGS],
    // The pattern a switch blends from.
    blend_from: GaitPattern,
    // Cycle position during a switch, in `[0, 1)`.
    cycle: f32,
    // Ground line under each foot in the air (ADR-8706): lift-off height blended to the
    // landing height by swing progress. Read only while `swing_grounded` says it lands on
    // ground.
    swing_floor: [f32; MAX_LEGS],
    // Whether each foot in the air swings toward ground (false toward a hang point).
    swing_grounded: [bool; MAX_LEGS],
    // Seconds the idle turn thresholds stay in force after the last yaw change (ADR-10130):
    // `settle_period` after a step that turned, counting down to 0. A host that hands the
    // driven stepper a new yaw on only some steps still turns in place by turn steps.
    // Not reflected: transient timing that a scene or save holding a `GaitState` written
    // before it does not carry; a loaded state starts with no hold.
    #[reflect(ignore)]
    turn_hold: f32,
}

impl GaitState {
    /// Plant every foot in a neutral stance and set body clearance from the floor.
    ///
    /// `position[1]` is the body height the ground is asked near (`expected_y` is it minus
    /// `clearance`). A neutral with no valid ground is searched on rings like a landing
    /// (initial plants ignore `max_reach`); a foot with none hangs and counts in `refused`.
    pub fn at_rest(
        params: &GaitParams,
        position: [f32; 3],
        yaw: f32,
        ground: impl GaitGround,
    ) -> Result<Self, GaitError> {
        params.validate()?;
        let mut state = Self::blank(params, position, yaw, GaitMode::Integrating, 0.0);
        state.validate()?;
        state.yaw = wrap_yaw(yaw);
        let frame = Frame::of(GaitMode::Integrating, state.yaw);
        state.plant_neutral(params, frame, &ground)?;
        state.follow_stance(params, None);
        state.follow_attitude(params, frame, None);
        state.report();
        state.validate()?;
        Ok(state)
    }

    /// Plant every foot at neutral under a host-supplied body, in driven mode.
    ///
    /// The body's XZ, yaw and velocity are copied bit for bit; Y follows the stance. The
    /// body's Y only picks the ground layer: `expected_y` is it minus `clearance`. The
    /// pattern is the set's choice for the body's ground speed.
    pub fn at_rest_driven(
        params: &GaitParams,
        body: GaitBody,
        ground: impl GaitGround,
    ) -> Result<Self, GaitError> {
        params.validate()?;
        if !body.is_finite() {
            return Err(GaitError::Input);
        }
        let speed = body_speed(body)?;
        let mut state = Self::blank(params, body.position, body.yaw, GaitMode::Driven, speed);
        state.adopt(body);
        state.validate()?;
        let frame = Frame::of(GaitMode::Driven, state.yaw);
        state.plant_neutral(params, frame, &ground)?;
        state.follow_stance(params, None);
        state.follow_attitude(params, frame, None);
        state.report();
        state.validate()?;
        Ok(state)
    }

    fn blank(params: &GaitParams, position: [f32; 3], yaw: f32, mode: GaitMode, speed: f32) -> Self {
        let index = params.patterns.select_start(speed);
        let pattern = params.patterns.patterns[index];
        Self {
            position,
            yaw,
            velocity: [0.0; 3],
            phase: 0.0,
            swing_a: true,
            power_lost: false,
            feet: [FootTarget {
                position: [0.0; 3],
                planted: false,
                normal: UP,
            }; MAX_LEGS],
            mode,
            replants: 0,
            settled: true,
            refused: 0,
            forced_steps: 0,
            foot_modes: [FootMode::Tripod; MAX_LEGS],
            legs: params.legs() as u8,
            beat: 0,
            pattern,
            pattern_index: index as u8,
            blending: false,
            blend: 0.0,
            foot_phase: [0.0; MAX_LEGS],
            stability_margin: 0.0,
            body_height_target: position[1],
            pitch: 0.0,
            roll: 0.0,
            attitude: posture::IDENTITY,
            lift_off: [[0.0; 3]; MAX_LEGS],
            swing_heading: [0.0; 2],
            forced_phase: [0.0; MAX_LEGS],
            swinging: [false; MAX_LEGS],
            swing_progress: [0.0; MAX_LEGS],
            swing_start: [0.0; MAX_LEGS],
            leg_heading: [[0.0; 2]; MAX_LEGS],
            blend_from: pattern,
            cycle: 0.0,
            swing_floor: [0.0; MAX_LEGS],
            swing_grounded: [false; MAX_LEGS],
            turn_hold: 0.0,
        }
    }

    /// The used feet: the first `legs` entries of [`Self::feet`].
    #[must_use]
    pub fn feet(&self) -> &[FootTarget] {
        &self.feet[..self.leg_count()]
    }

    /// Number of feet as a `usize`.
    #[must_use]
    pub fn leg_count(&self) -> usize {
        usize::from(self.legs).min(MAX_LEGS)
    }

    /// Whether foot `leg` is in its pattern swing (lifted on its turn and not yet landed).
    #[must_use]
    pub fn is_swinging(&self, leg: usize) -> bool {
        self.swinging.get(leg).copied().unwrap_or(false)
    }

    /// Copy the host body into the state: XZ, yaw and velocity bits, never Y.
    fn adopt(&mut self, body: GaitBody) {
        self.position[0] = body.position[0];
        self.position[2] = body.position[2];
        self.yaw = body.yaw;
        self.velocity = body.velocity;
    }

    /// Plant at neutral hips and restart the cycle, counting it; the caller's frame decides.
    fn replant(
        &mut self,
        params: &GaitParams,
        frame: Frame,
        ground: &impl GaitGround,
    ) -> Result<(), GaitError> {
        self.plant_neutral(params, frame, ground)?;
        self.phase = 0.0;
        self.beat = 0;
        self.swinging = [false; MAX_LEGS];
        self.swing_progress = [0.0; MAX_LEGS];
        self.swing_start = [0.0; MAX_LEGS];
        self.blending = false;
        self.blend = 0.0;
        self.pattern = params.patterns.patterns[usize::from(self.pattern_index)];
        self.swing_heading = [0.0; 2];
        self.leg_heading = [[0.0; 2]; MAX_LEGS];
        self.settled = true;
        self.replants = self.replants.saturating_add(1);
        self.follow_stance(params, None);
        self.follow_attitude(params, frame, None);
        Ok(())
    }

    /// Subtract a world-origin shift from the body, feet and swing history atomically.
    pub fn shift_origin(&mut self, shift: [f32; 3]) -> Result<(), GaitError> {
        if shift.iter().any(|x| !x.is_finite()) {
            return Err(GaitError::Input);
        }
        let mut next = *self;
        let legs = next.leg_count();
        for point in std::iter::once(&mut next.position)
            .chain(next.feet[..legs].iter_mut().map(|foot| &mut foot.position))
            .chain(next.lift_off[..legs].iter_mut())
        {
            for (value, offset) in point.iter_mut().zip(shift) {
                *value -= offset;
            }
        }
        next.body_height_target -= shift[1];
        for floor in &mut next.swing_floor[..legs] {
            *floor -= shift[1];
        }
        next.validate()?;
        *self = next;
        Ok(())
    }

    fn validate(&self) -> Result<(), GaitError> {
        let finite = self
            .position
            .iter()
            .chain(&self.velocity)
            .chain(self.feet.iter().flat_map(|f| f.position.iter().chain(&f.normal)))
            .chain(self.lift_off.iter().flatten())
            .chain(&self.swing_heading)
            .chain(self.leg_heading.iter().flatten())
            .chain(&self.swing_floor)
            .chain(&self.attitude)
            .chain([&self.body_height_target, &self.pitch, &self.roll, &self.turn_hold])
            .all(|v| v.is_finite());
        let legs = usize::from(self.legs);
        if !finite
            || !self.yaw.is_finite()
            || !(0.0..1.0).contains(&self.phase)
            || !(0.0..1.0).contains(&self.blend)
            || !(0.0..1.0).contains(&self.cycle)
            || self.forced_phase.iter().any(|p| !(0.0..1.0).contains(p))
            || self.swing_progress.iter().any(|p| !(0.0..=1.0).contains(p))
            || self.swing_start.iter().any(|p| !(0.0..1.0).contains(p))
            || self.foot_phase.iter().any(|p| !(0.0..1.0).contains(p))
            || legs == 0
            || legs > MAX_LEGS
        {
            return Err(GaitError::State);
        }
        if self.pattern.legs != self.legs || self.blend_from.legs != self.legs {
            return Err(GaitError::Pattern);
        }
        // The pattern in effect is reflected and host-writable (ADR-8736): a duty of 0 would
        // freeze a switch, so both blend ends are checked like the settings' patterns.
        self.pattern.validate()?;
        self.blend_from.validate()
    }

    /// Plant every foot at its neutral (or the best ring foothold around it), or hang it.
    /// Initial plants ignore `max_reach`: the body height is not known yet.
    fn plant_neutral(
        &mut self,
        params: &GaitParams,
        frame: Frame,
        ground: &impl GaitGround,
    ) -> Result<(), GaitError> {
        for i in 0..self.leg_count() {
            let neutral = hip(self.position, frame, params.hip_offsets[i]);
            let query = FootQuery {
                leg: i,
                frame,
                start: None,
                check_reach: false,
            };
            let found = choose_foothold(params, self, query, neutral, ground)?;
            self.foot_modes[i] = FootMode::Tripod;
            self.forced_phase[i] = 0.0;
            self.feet[i] = match found {
                Some((footing, _)) => footing.planted(),
                None => self.hang(params, i, frame),
            };
            self.lift_off[i] = self.feet[i].position;
        }
        Ok(())
    }

    /// Make foot `i` hang below its hip and count the refusal; returns its target.
    fn hang(&mut self, params: &GaitParams, i: usize, frame: Frame) -> FootTarget {
        self.foot_modes[i] = FootMode::Hanging;
        self.refused = self.refused.saturating_add(1);
        FootTarget {
            position: self.hang_point(params, i, frame),
            planted: false,
            normal: UP,
        }
    }

    /// Where a foot with no ground hangs: under its hip, `hang_drop` below the expected floor.
    fn hang_point(&self, params: &GaitParams, i: usize, frame: Frame) -> [f32; 3] {
        let [x, z] = hip(self.position, frame, params.hip_offsets[i]);
        [x, self.expected_floor(params) - params.hang_drop, z]
    }

    /// Where the floor should be (the ground's `expected_y`): the body height target minus
    /// clearance (ADR-10053). The target is the floor the posture chases; the visual body
    /// height `position[1]` may lag it under `height_tau` / `height_slew`, and asking near the
    /// lagging height would drop a climbing walker's footholds out of `ground_band`. At
    /// construction and on a driven replant the target is the host body's Y.
    fn expected_floor(&self, params: &GaitParams) -> f32 {
        self.body_height_target - params.clearance
    }

    /// Replace non-finite posture outputs so a step can run (ADR-10053): a scene file can
    /// store a NaN or an infinity in these reflected fields. Pitch or roll become 0, an
    /// attitude with a non-finite component becomes the identity, and a non-finite height
    /// target becomes the body height when that is finite. The step then rebuilds them.
    fn heal_posture(&mut self) {
        if !self.pitch.is_finite() {
            self.pitch = 0.0;
        }
        if !self.roll.is_finite() {
            self.roll = 0.0;
        }
        if !self.attitude.iter().all(|v| v.is_finite()) {
            self.attitude = posture::IDENTITY;
        }
        if !self.body_height_target.is_finite() && self.position[1].is_finite() {
            self.body_height_target = self.position[1];
        }
    }

    /// 3D distance from foot `i` to its hip (XZ under the body, at body height).
    fn reach(&self, params: &GaitParams, i: usize, frame: Frame) -> f32 {
        let [hx, hz] = hip(self.position, frame, params.hip_offsets[i]);
        let foot = self.feet[i].position;
        let (dx, dy, dz) = (foot[0] - hx, self.position[1] - foot[1], foot[2] - hz);
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    /// Refresh the reported fields: `swing_a`, `foot_phase` and `stability_margin`.
    fn report(&mut self) {
        let legs = self.leg_count();
        let schedule = Schedule::of(&self.pattern);
        let beat = usize::from(self.beat).min(schedule.groups - 1);
        self.swing_a = beat == usize::from(schedule.group[0]);
        if !self.blending {
            for i in 0..legs {
                self.foot_phase[i] = schedule.foot_phase(i, beat, self.phase);
            }
        }
        let mut points = [[0.0_f32; 2]; MAX_LEGS];
        let mut n = 0;
        for foot in &self.feet[..legs] {
            if foot.planted {
                points[n] = [foot.position[0], foot.position[2]];
                n += 1;
            }
        }
        self.stability_margin = if n == 0 {
            NO_SUPPORT_MARGIN
        } else {
            support_margin(&points[..n], [self.position[0], self.position[2]])
        };
    }
}

/// A refused operation. State is unchanged on every error; callbacks may already have run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GaitError {
    /// Invalid geometry or motion settings.
    #[error("gait_settings: settings must be finite and satisfy their documented ranges")]
    Settings,
    /// Nonfinite controls, timestep or origin shift, or a driven body whose velocity or
    /// ground speed is not finite (a finite velocity whose length overflows `f32`).
    #[error("gait_input: input and timestep must be finite")]
    Input,
    /// Invalid or overflowing state.
    #[error("gait_state: state must be finite and phase must be in [0, 1)")]
    State,
    /// A ground query returned a nonfinite height or normal (or a zero normal), or its
    /// coordinates overflowed. No ground (`None`) is not an error.
    #[error("gait_floor: ground coordinates, height and normal must be finite")]
    Floor,
    /// The step advances by more than one stance (one tripod period); subdivide the
    /// timestep.
    #[error("gait_step_too_large: subdivide the timestep to at most one tripod period")]
    StepTooLarge,
    /// An invalid gait pattern or pattern set (ADR-8653): legs 0 or above `MAX_LEGS`, an
    /// offset not finite or outside `[0, 1)`, a duty factor outside `(0, 1)`, patterns with
    /// different leg counts, bad switch speeds or hysteresis; a state built for another
    /// leg count than the settings, a state whose `pattern_index` is past the set (a host
    /// that shrinks its set rebuilds the state), or a state whose reflected `pattern` is
    /// invalid (ADR-8736).
    #[error(
        "gait_pattern: legs must be 1..=16, offsets finite in [0, 1), duty in (0, 1), one leg \
         count per set and per state"
    )]
    Pattern,
}

/// Advance one integrating presentation step with `[forward, strafe, steer]` controls
/// clamped to `[-1, 1]`; the stepper moves the body itself (+Z forward, see the module table).
/// Games whose body comes from a simulation, physics mover or navigation use
/// [`step_gait_driven`] instead. A state last stepped in driven mode is replanted at neutral
/// once (counted in [`GaitState::replants`]).
///
/// Power loss or finite `dt <= 0` is a no-op. Otherwise invalid input, state, settings,
/// pattern or ground samples are refused atomically. A step may advance at most one stance
/// (one tripod period); hosts should use small fixed steps.
///
/// Patterns (ADR-8653): each beat boundary lifts one group of feet; a foot lands when its
/// swing (`1 - duty_factor` of the cycle) ends. While walking, [`GaitParams::patterns`]
/// picks the pattern by ground speed with hysteresis; a switch blends offsets and duty over
/// one cycle, every foot's phase still advancing, so no foot jumps. A switch that is still
/// blending when the gait goes idle ends at once: feet in the air finish their swing as
/// forced steps.
///
/// Ground (ADR-8792): `ground` is any [`GaitGround`], including a plain height closure. A
/// landing is refused when its point has no ground, lies outside `ground_band` around the
/// expected floor, is steeper than `max_slope` (the surface normal or the step from lift-off)
/// or lies beyond the leg's `max_reach`. A refused landing tries `foothold_candidates` points
/// on rings `foothold_spacing` apart around it and takes the closest ring's valid point
/// with the smallest height change (ties: search order). With none valid the foot holds its
/// lift-off point, including at touchdown, and [`GaitState::refused`] counts it; if that
/// ground is gone too the foot hangs (`planted == false`, [`FootMode::Hanging`]). A planted
/// foot farther than `max_reach` from its hip takes a forced out-of-phase step of
/// `settle_period` ([`GaitState::forced_steps`]); only then, or while a foot hangs, may
/// fewer feet be planted than the pattern says. None of this blocks body travel or provides
/// collision response.
///
/// At or below `idle_speed` (ADR-8539) the swings in progress finish on their arcs with
/// their landings bent to neutral, one beat per `settle_period` (a landing that is still far
/// from neutral late in its swing stretches that beat so its bend never moves it more than
/// `2 * step_length` per `settle_period`, ADR-10130, ADR-10162); then each lift-off group
/// with a foot farther than `settle_distance` from neutral steps there, one group at a time
/// in pattern order; [`GaitState::settled`] then turns true and the feet stay put. While
/// yaw changes at idle (turning in place) the thresholds are `turn_step_distance` and
/// `turn_step_angle`. An idle step longer than one `settle_period` is refused like a walking
/// one.
#[tracing::instrument(skip_all)]
pub fn step_gait(
    params: &GaitParams,
    state: &mut GaitState,
    input: [f32; 3],
    dt: f32,
    ground: impl GaitGround,
) -> Result<(), GaitError> {
    if state.power_lost {
        return Ok(());
    }
    if !dt.is_finite() {
        return Err(GaitError::Input);
    }
    if dt <= 0.0 {
        return Ok(());
    }
    if input.iter().any(|x| !x.is_finite()) {
        return Err(GaitError::Input);
    }
    let mut next = *state;
    next.heal_posture();
    check(params, &next)?;
    advance(params, &mut next, input.map(|v| v.clamp(-1.0, 1.0)), dt, &ground)?;
    next.report();
    next.validate()?;
    *state = next;
    Ok(())
}

/// Settings, state and their leg counts agree.
fn check(params: &GaitParams, state: &GaitState) -> Result<(), GaitError> {
    params.validate()?;
    state.validate()?;
    if params.legs() != state.leg_count()
        || usize::from(state.pattern_index) >= usize::from(params.patterns.count)
    {
        return Err(GaitError::Pattern);
    }
    Ok(())
}

fn advance(
    params: &GaitParams,
    state: &mut GaitState,
    input: [f32; 3],
    dt: f32,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    let yaw = state.yaw + input[2] * params.max_yaw_rate * dt;
    if !yaw.is_finite() {
        return Err(GaitError::State);
    }
    let before = state.yaw;
    state.yaw = wrap_yaw(yaw);
    let turned = state.yaw != before;
    let frame = Frame::of(GaitMode::Integrating, state.yaw);
    let [sin, cos] = frame.forward;
    let forward = input[0]
        * if input[0] >= 0.0 {
            params.forward_speed
        } else {
            params.reverse_speed
        };
    let lateral = input[1] * params.strafe_speed;
    let dx = sin * forward + cos * lateral - state.velocity[0];
    let dz = cos * forward - sin * lateral - state.velocity[2];
    let delta = (dx * dx + dz * dz).sqrt();
    if !delta.is_finite() {
        return Err(GaitError::State);
    }
    if delta > 1e-8 {
        let scale = (params.acceleration * dt / delta).min(1.0);
        state.velocity[0] += dx * scale;
        state.velocity[2] += dz * scale;
    }
    state.velocity[1] = 0.0;
    state.position[0] += state.velocity[0] * dt;
    state.position[2] += state.velocity[2] * dt;
    if state.mode != GaitMode::Integrating {
        // Feet placed in the driven frame do not belong to this body: replant once.
        state.mode = GaitMode::Integrating;
        return state.replant(params, frame, ground);
    }
    step_feet(params, state, frame, dt, turned, ground)
}

/// Advance one driven presentation step: the host's simulation owns the body.
///
/// `body` (object convention, see [`GaitBody`]) is copied into the state bit for bit
/// (XZ, yaw, velocity); body Y follows the stance as in [`step_gait`]. The stepper only
/// advances the gait phase from the body's ground speed, places feet and follows stance
/// height. A body that moved more than `params.step_length` horizontally since the last
/// step (teleport, origin shift by the simulation, lag correction), or a state last stepped
/// in the other mode, replants every foot at neutral like [`GaitState::at_rest_driven`] and
/// increments [`GaitState::replants`] instead of stretching legs. A replant asks the ground
/// near the host body's Y (a teleport into a tunnel picks the tunnel floor); continuous
/// steps ask near the stance height. Ground and patterns are handled as in [`step_gait`]
/// (ADR-8792, ADR-8653).
///
/// Power loss or finite `dt <= 0` is a no-op. Otherwise invalid body, timestep, state,
/// settings, pattern or ground samples are refused atomically. If the body's ground speed
/// would advance more than one stance, the step is refused: subdivide `dt` and interpolate
/// the body. Feet never feed back into the body; gameplay footsteps belong to the
/// simulation's own cadence (ADR-8474).
#[tracing::instrument(skip_all)]
pub fn step_gait_driven(
    params: &GaitParams,
    state: &mut GaitState,
    body: GaitBody,
    dt: f32,
    ground: impl GaitGround,
) -> Result<(), GaitError> {
    if state.power_lost {
        return Ok(());
    }
    if !dt.is_finite() {
        return Err(GaitError::Input);
    }
    if dt <= 0.0 {
        return Ok(());
    }
    if !body.is_finite() {
        return Err(GaitError::Input);
    }
    body_speed(body)?;
    let mut next = *state;
    next.heal_posture();
    check(params, &next)?;
    let dx = body.position[0] - next.position[0];
    let dz = body.position[2] - next.position[2];
    let jumped = dx * dx + dz * dz > params.step_length * params.step_length;
    let switched = next.mode != GaitMode::Driven;
    let turned = body.yaw != next.yaw;
    next.mode = GaitMode::Driven;
    next.adopt(body);
    let frame = Frame::of(GaitMode::Driven, next.yaw);
    if jumped || switched {
        // The replant asks the ground near the host body's Y (`expected_floor`).
        next.position[1] = body.position[1];
        next.body_height_target = body.position[1];
        next.replant(params, frame, &ground)?;
    } else {
        step_feet(params, &mut next, frame, dt, turned, &ground)?;
    }
    next.report();
    next.validate()?;
    *state = next;
    Ok(())
}

/// The feet core shared by both modes: the body is already in place for this step.
/// `turned` is whether the body's yaw changed in this step; the idle turn thresholds hold for
/// `settle_period` after it (ADR-10130).
fn step_feet(
    params: &GaitParams,
    state: &mut GaitState,
    frame: Frame,
    dt: f32,
    turned: bool,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    let speed = ground_speed(state.velocity)?;
    let walking = speed > params.idle_speed;
    let start_height = state.position[1];
    let turned = state.hold_turn(params, dt, turned);
    if walking {
        choose_pattern(params, state, speed);
    } else {
        settle_pattern(params, state);
    }
    let pattern_swinging = if !walking {
        settle_feet(params, state, frame, dt, turned, ground)?
    } else if state.blending {
        blend_feet(params, state, frame, speed, dt, ground)?;
        true
    } else {
        walk_feet(params, state, frame, speed, dt, ground)?;
        true
    };
    advance_forced(params, state, frame, dt, pattern_swinging, ground)?;
    enforce_reach(params, state, frame, (start_height, dt));
    state.follow_attitude(params, frame, Some(dt));
    if state.foot_modes[..state.leg_count()].contains(&FootMode::Forced) {
        state.settled = false;
    }
    Ok(())
}

/// While walking and not already switching: start a switch when the set picks another
/// pattern at this speed, or when the host changed the pattern in use.
fn choose_pattern(params: &GaitParams, state: &mut GaitState, speed: f32) {
    if state.blending {
        return;
    }
    let current = usize::from(state.pattern_index).min(usize::from(params.patterns.count) - 1);
    let index = params.patterns.select(current, speed);
    let target = params.patterns.patterns[index];
    if index == usize::from(state.pattern_index) && target == state.pattern {
        return;
    }
    let schedule = Schedule::of(&state.pattern);
    let beat = usize::from(state.beat).min(schedule.groups - 1);
    state.cycle = schedule.cycle(beat, state.phase);
    // Each foot's phase at the switch: a swinging foot keeps its scheduled swing progress
    // (a late lift's compressed arc is mapped back through `swing_start`, ADR-10053, so its
    // phase does not run backward and it lands on its schedule); a planted foot whose turn
    // the clock skipped (idle, or a walk from rest) lifts at once.
    let duty = schedule.duty;
    for i in 0..state.leg_count() {
        state.foot_phase[i] = if state.swinging[i] {
            let scheduled = state.scheduled_swing(i);
            (duty + scheduled * schedule.swing).min(1.0 - f32::EPSILON)
        } else {
            schedule.foot_phase(i, beat, state.phase).min(duty)
        };
    }
    // `swing_start` stays: `blend_feet` maps each late lift's scheduled progress to its arc.
    state.pattern_index = index as u8;
    state.blend_from = state.pattern;
    state.blending = true;
    state.blend = 0.0;
}

/// At idle: a switch still blending ends at once (feet in the air finish as forced steps);
/// a host-changed pattern is adopted while no foot swings.
fn settle_pattern(params: &GaitParams, state: &mut GaitState) {
    let index = usize::from(state.pattern_index).min(usize::from(params.patterns.count) - 1);
    let target = params.patterns.patterns[index];
    let legs = state.leg_count();
    if state.blending {
        for i in 0..legs {
            if state.swinging[i] {
                state.swinging[i] = false;
                state.swing_start[i] = 0.0;
                if matches!(state.foot_modes[i], FootMode::Tripod | FootMode::Hanging) {
                    state.foot_modes[i] = FootMode::Forced;
                    state.forced_phase[i] = state.swing_progress[i].min(1.0 - f32::EPSILON);
                } else if state.foot_modes[i] == FootMode::Waiting {
                    state.foot_modes[i] = FootMode::Tripod;
                }
            }
        }
        state.blending = false;
        state.blend = 0.0;
        state.pattern = target;
        state.pattern_index = index as u8;
        let schedule = Schedule::of(&target);
        state.beat = schedule.locate(state.cycle).0 as u8;
        state.phase = 0.0;
        state.cycle = 0.0;
    } else if state.pattern != target && state.phase == 0.0 && !state.swinging[..legs].contains(&true) {
        state.pattern = target;
        state.pattern_index = index as u8;
        let schedule = Schedule::of(&target);
        state.beat = usize::from(state.beat).min(schedule.groups - 1) as u8;
    }
}

/// Walking on a steady pattern: advance the beat clock, landing and lifting groups at each
/// beat boundary, then place the feet.
fn walk_feet(
    params: &GaitParams,
    state: &mut GaitState,
    frame: Frame,
    speed: f32,
    dt: f32,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    state.settled = false;
    let period = params.step_period_at_speed(speed);
    let progress = dt / period;
    if !progress.is_finite() || progress > 1.0 {
        return Err(GaitError::StepTooLarge);
    }
    let direction = [state.velocity[0] / speed, state.velocity[2] / speed];
    state.swing_heading = direction;
    let legs = state.leg_count();
    for heading in &mut state.leg_heading[..legs] {
        *heading = direction;
    }
    let schedule = Schedule::of(&state.pattern);
    let cycle_time = period / schedule.duty;
    let mut beat = usize::from(state.beat).min(schedule.groups - 1);
    let starting = state.phase == 0.0;
    let from_rest = starting && !state.swinging[..legs].contains(&true);
    let mut phase = state.phase + dt / (schedule.len[beat] * cycle_time);
    // The beat that starts at this step lifts its group before any boundary is crossed, so
    // a first step that crosses a whole beat (a short beat at a low rate) lifts and lands
    // it there instead of skipping it (ADR-8736).
    if starting {
        state.lift_group(&schedule, beat);
        if from_rest {
            // Walking from rest with overlapping swings (ADR-8736): a foot the schedule
            // already has mid-swing lifts now and lands on its scheduled touchdown, its arc
            // compressed into the rest of that swing, so contact matches `foot_phase`.
            for i in 0..legs {
                if state.swinging[i] {
                    continue;
                }
                if let Some(start) = schedule.swing_progress(i, beat, 0.0) {
                    state.lift_leg(i);
                    state.swing_start[i] = start.min(1.0 - f32::EPSILON);
                }
            }
        }
    }
    let mut crossed = 0;
    while phase >= 1.0 {
        crossed += 1;
        if crossed > schedule.groups {
            return Err(GaitError::StepTooLarge);
        }
        land_finished(params, state, &schedule, frame, beat, 1.0, ground)?;
        let next = (beat + 1) % schedule.groups;
        phase = (phase - 1.0) * (schedule.len[beat] / schedule.len[next]);
        beat = next;
        state.lift_group(&schedule, beat);
    }
    state.beat = beat as u8;
    state.phase = phase;
    land_finished(params, state, &schedule, frame, beat, phase, ground)?;
    state.progress_from_schedule(&schedule);
    place_feet(params, state, frame, false, ground)
}

/// Walking through a pattern switch: each foot's phase advances by the cycle progress plus
/// its offset's share of the blend; a foot lifts when its phase passes the blended duty and
/// lands when its phase wraps. Offsets move the short way (at most half a cycle) and
/// neighboring duty factors differ by at most [`MAX_DUTY_STEP`], so phases never run
/// backward.
fn blend_feet(
    params: &GaitParams,
    state: &mut GaitState,
    frame: Frame,
    speed: f32,
    dt: f32,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    state.settled = false;
    let period = params.step_period_at_speed(speed);
    let progress = dt / period;
    if !progress.is_finite() || progress > 1.0 {
        return Err(GaitError::StepTooLarge);
    }
    let direction = [state.velocity[0] / speed, state.velocity[2] / speed];
    state.swing_heading = direction;
    let legs = state.leg_count();
    for heading in &mut state.leg_heading[..legs] {
        *heading = direction;
    }
    let from = state.blend_from;
    let target = params.patterns.patterns[usize::from(state.pattern_index)];
    let duty_now = from.duty_factor + (target.duty_factor - from.duty_factor) * state.blend;
    let advance = progress * duty_now;
    let blend = (state.blend + advance).min(1.0);
    let duty = from.duty_factor + (target.duty_factor - from.duty_factor) * blend;
    let mut next_phase = [0.0_f32; MAX_LEGS];
    for (i, next) in next_phase.iter_mut().enumerate().take(legs) {
        let delta = offset_delta(from.phase_offset[i], target.phase_offset[i]);
        *next = state.foot_phase[i] + advance + delta * (blend - state.blend);
    }
    // Landings first, then lift-offs, as at a beat boundary.
    let was_swinging = state.swinging;
    for (i, &phase) in next_phase.iter().enumerate().take(legs) {
        if state.swinging[i] && phase >= 1.0 {
            land_leg(params, state, i, frame, ground)?;
        }
    }
    for (i, &phase) in next_phase.iter().enumerate().take(legs) {
        let wrapped = phase >= 1.0;
        let phase = wrap_unit(phase);
        let scheduled = ((phase - duty) / (1.0 - duty)).clamp(0.0, 1.0);
        if state.swinging[i] {
            let u = state.arc_of(i, scheduled).clamp(state.swing_progress[i], 1.0);
            state.swing_progress[i] = u;
        } else {
            if !was_swinging[i] && wrapped {
                // The phase crossed the whole swing window in this step (ADR-8736): step
                // now, lift and land, as the beat clock does, so no stride is skipped.
                state.lift_leg(i);
                land_leg(params, state, i, frame, ground)?;
            }
            // A phase that wrapped into the next swing window lifts again (ADR-10053), so
            // contact agrees with `foot_phase`; a planted foot past the duty lifts.
            if phase >= duty && (wrapped || !was_swinging[i]) {
                state.lift_leg(i);
                state.swing_progress[i] = scheduled;
            }
        }
        state.foot_phase[i] = phase;
    }
    state.cycle = wrap_unit(state.cycle + advance);
    if blend >= 1.0 {
        finish_blend(params, state, &target, frame, ground)?;
    } else {
        state.blend = blend;
        state.pattern = from.blend(&target, blend);
        let schedule = Schedule::of(&state.pattern);
        let (beat, phase) = schedule.locate(state.cycle);
        state.beat = beat as u8;
        state.phase = phase;
    }
    place_feet(params, state, frame, false, ground)
}

/// The switch reached its target: return to the beat clock at the current cycle position.
/// Feet the target schedule disagrees with by rounding land or lift now; a planted foot
/// further into its swing waits for its next turn.
fn finish_blend(
    params: &GaitParams,
    state: &mut GaitState,
    target: &GaitPattern,
    frame: Frame,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    state.blending = false;
    state.blend = 0.0;
    state.pattern = *target;
    let schedule = Schedule::of(target);
    let (beat, phase) = schedule.locate(state.cycle);
    state.cycle = 0.0;
    state.beat = beat as u8;
    state.phase = phase;
    let legs = state.leg_count();
    for i in 0..legs {
        if state.swinging[i] && schedule.swing_progress(i, beat, phase).is_none() {
            land_leg(params, state, i, frame, ground)?;
        }
    }
    for i in 0..legs {
        if !state.swinging[i] && schedule.elapsed(i, beat, phase) <= 0.05 * schedule.swing {
            state.lift_leg(i);
        }
    }
    state.progress_from_schedule(&schedule);
    Ok(())
}

/// Land every swinging foot whose swing has ended at `phase` through `beat`. At a beat
/// boundary (`phase == 1`) a swing within [`EVENT_EPS`] of its end lands too, so a foot that should land as
/// the next group lifts does.
fn land_finished(
    params: &GaitParams,
    state: &mut GaitState,
    schedule: &Schedule,
    frame: Frame,
    beat: usize,
    phase: f32,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    // Phase 1 is the end of the beat: its boundary.
    let limit = if phase >= 1.0 {
        schedule.swing - EVENT_EPS
    } else {
        schedule.swing
    };
    for i in 0..state.leg_count() {
        if state.swinging[i] && schedule.elapsed(i, beat, phase) >= limit {
            land_leg(params, state, i, frame, ground)?;
        }
    }
    Ok(())
}

/// End foot `i`'s pattern swing: plant it at its landing target (holding or hanging when
/// none is valid). A forced foot keeps its own swing; a waiting foot (landed out of phase)
/// rejoins its pattern.
fn land_leg(
    params: &GaitParams,
    state: &mut GaitState,
    i: usize,
    frame: Frame,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    state.swinging[i] = false;
    state.swing_progress[i] = 0.0;
    state.swing_start[i] = 0.0;
    match state.foot_modes[i] {
        FootMode::Forced => {}
        FootMode::Waiting => state.foot_modes[i] = FootMode::Tripod,
        FootMode::Tripod | FootMode::Hanging => {
            state.feet[i] = match landing(params, state, i, frame, state.leg_heading[i], ground)? {
                Landing::Plant(footing) => {
                    state.foot_modes[i] = FootMode::Tripod;
                    footing.planted()
                }
                Landing::Hold(footing) => {
                    state.refused = state.refused.saturating_add(1);
                    footing.planted()
                }
                Landing::Hang => state.hang(params, i, frame),
            };
        }
    }
    Ok(())
}

/// Idle feet (ADR-8539): the swings in progress finish on their arcs, their landings bent to
/// neutral; then each lift-off group with a foot off neutral (`settle_distance`, or the turn
/// thresholds while yaw changes) swings to neutral in pattern order, one at a time, one beat
/// per `settle_period`. With no swing in progress, planted feet keep XZ and follow the floor.
/// Returns whether a pattern swing is in progress after this step.
fn settle_feet(
    params: &GaitParams,
    state: &mut GaitState,
    frame: Frame,
    dt: f32,
    turned: bool,
    ground: &impl GaitGround,
) -> Result<bool, GaitError> {
    let legs = state.leg_count();
    if state.phase == 0.0 {
        // A swing lifted at a beat boundary that has not moved yet is no swing.
        for i in 0..legs {
            if state.swinging[i] && state.swing_progress[i] == 0.0 {
                state.swinging[i] = false;
            }
        }
    }
    let in_progress = state.phase > 0.0 || state.swinging[..legs].contains(&true);
    let swinging = in_progress || state.start_idle_swing(params, frame, turned, ground)?;
    if !swinging {
        state.settled = true;
        place_feet(params, state, frame, true, ground)?;
        return Ok(false);
    }
    let progress = dt / params.settle_period;
    if !progress.is_finite() || progress > 1.0 {
        return Err(GaitError::StepTooLarge);
    }
    let schedule = Schedule::of(&state.pattern);
    let beat = usize::from(state.beat).min(schedule.groups - 1);
    // A late landing stretches its swing (ADR-10162): the clock slows until the bend is done.
    let mut phase = state.phase + progress * idle_clock_rate(params, state, &schedule, beat, progress);
    state.settled = false;
    if phase >= 1.0 {
        state.swing_heading = [0.0; 2];
        for i in 0..legs {
            if state.swinging[i] && schedule.elapsed(i, beat, 1.0) >= schedule.swing - EVENT_EPS {
                bend_heading(params, state, i, 1.0, progress);
            }
        }
        land_finished(params, state, &schedule, frame, beat, 1.0, ground)?;
        phase -= 1.0;
        let next = (beat + 1) % schedule.groups;
        state.beat = next as u8;
        if state.swinging[..legs].contains(&true) {
            // Overlapping swings continue; the next group steps only if it is due.
            shrink_headings(params, state, &schedule, (next, phase), progress);
            if state.group_due(params, &schedule, next, frame, turned, ground)? {
                for i in 0..legs {
                    if usize::from(schedule.group[i]) == next {
                        state.leg_heading[i] = [0.0; 2];
                    }
                }
                state.lift_group(&schedule, next);
            }
        } else if !state.start_idle_swing(params, frame, turned, ground)? {
            phase = 0.0;
            state.settled = true;
        }
    } else {
        // Shrink the walking heading so each landing reaches neutral exactly at touchdown.
        let keep = (1.0 - phase) / (1.0 - state.phase);
        state.swing_heading = state.swing_heading.map(|h| h * keep);
        shrink_headings(params, state, &schedule, (beat, phase), progress);
        land_finished(params, state, &schedule, frame, beat, phase, ground)?;
    }
    state.phase = phase;
    state.progress_from_schedule(&schedule);
    let at_rest = state.phase == 0.0 && state.settled;
    place_feet(params, state, frame, at_rest, ground)?;
    Ok(!at_rest)
}

/// Shrink each swinging foot's landing heading by its remaining swing, from its stored
/// progress to its progress at the new clock, so it lands at neutral at its touchdown (see
/// `bend_heading` for the per-step limit).
fn shrink_headings(
    params: &GaitParams,
    state: &mut GaitState,
    schedule: &Schedule,
    (beat, phase): (usize, f32),
    progress: f32,
) {
    for i in 0..state.leg_count() {
        if !state.swinging[i] {
            continue;
        }
        let after = state.arc_progress(schedule, i, beat, phase);
        bend_heading(params, state, i, after, progress);
    }
}

/// Bend swinging foot `i`'s landing toward neutral as its arc goes from its stored progress
/// to `after`: the heading shrinks by the share of the remaining swing this step covers, so a
/// swing that keeps going idle lands at neutral exactly at touchdown (ADR-8539). One step may
/// move the landing at most `2 * step_length` per `settle_period` of time (`progress` is this
/// step's share of a `settle_period`; ADR-10130): the re-aim a swing is allowed by the swing
/// bound of `anim.gait_simulate`. A swing that went idle just before touchdown used to drop its
/// whole heading in one step, and the foot jumped up to `step_length / 2`; such a swing now
/// stretches (`idle_clock_rate`, ADR-10162) so the limited bend still completes by touchdown.
/// The limit stays as the backstop: a landing it leaves short is finished by a settle step.
fn bend_heading(params: &GaitParams, state: &mut GaitState, i: usize, after: f32, progress: f32) {
    let before = state.swing_progress[i];
    let keep = if after >= 1.0 || before >= 1.0 {
        0.0
    } else {
        (1.0 - after) / (1.0 - before)
    };
    let heading = state.leg_heading[i];
    let shift = landing_shift(params, heading);
    let allowed = bend_limit(params, progress);
    let least = if shift > allowed && shift > 0.0 {
        1.0 - allowed / shift
    } else {
        0.0
    };
    state.leg_heading[i] = heading.map(|h| h * keep.clamp(0.0, 1.0).max(least));
}

/// Distance of a landing from its neutral for landing heading `heading`: the landing sits
/// `step_length / 2` along the heading.
fn landing_shift(params: &GaitParams, heading: [f32; 2]) -> f32 {
    (heading[0] * heading[0] + heading[1] * heading[1]).sqrt() * params.step_length * 0.5
}

/// The most one idle step may bend a landing (ADR-10130): `2 * step_length` per
/// `settle_period`, `progress` being the step's share of a `settle_period`.
fn bend_limit(params: &GaitParams, progress: f32) -> f32 {
    2.0 * params.step_length * progress.max(0.0)
}

/// The share of a full idle clock step (`progress`) the beat clock advances in this step
/// (ADR-10162). It is 1 unless a swinging foot's landing is still so far from neutral that
/// the proportional bend of `bend_heading` over a full clock step would pass `bend_limit`;
/// then the clock slows until that bend equals the limit, so the swing stretches and its
/// landing still reaches neutral exactly at touchdown. The remaining heading and remaining
/// arc then shrink together, so the rate holds for the rest of the swing, and a walking
/// landing (`step_length / 2` from neutral) needs at most a quarter of a `settle_period`
/// of swing. Swings that went idle before about three quarters of their arc never slow the
/// clock. Feet that land without their heading (forced, waiting) are not counted.
fn idle_clock_rate(
    params: &GaitParams,
    state: &GaitState,
    schedule: &Schedule,
    beat: usize,
    progress: f32,
) -> f32 {
    let allowed = bend_limit(params, progress);
    let mut rate = 1.0_f32;
    for i in 0..state.leg_count() {
        if !state.swinging[i] || !matches!(state.foot_modes[i], FootMode::Tripod | FootMode::Hanging) {
            continue;
        }
        let before = state.swing_progress[i];
        if before >= 1.0 {
            continue;
        }
        // Arc progress per unit of beat phase while the swing lasts: scheduled swing
        // progress grows by `len / swing` per beat, a late lift's arc by `1 / (1 - start)`
        // of that (`arc_of`).
        let start = state.swing_start[i];
        let late = if start > 0.0 { 1.0 - start } else { 1.0 };
        let slope = schedule.len[beat] / (schedule.swing * late);
        let bend = landing_shift(params, state.leg_heading[i]) * slope * progress / (1.0 - before);
        if bend > allowed {
            rate = rate.min(allowed / bend);
        }
    }
    if rate.is_finite() && rate > 0.0 { rate } else { 1.0 }
}

/// Stance feet follow floor height; `at_rest` (phase 0, settled) makes every foot stance.
/// Swinging feet follow their parabola from lift-off to their landing target. Stance feet
/// are placed first, then swinging feet, each in foot order.
fn place_feet(
    params: &GaitParams,
    state: &mut GaitState,
    frame: Frame,
    at_rest: bool,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    let legs = state.leg_count();
    for i in 0..legs {
        if at_rest || !state.swinging[i] {
            stance(params, state, i, frame, ground)?;
        }
    }
    if at_rest {
        return Ok(());
    }
    for i in 0..legs {
        if !state.swinging[i] {
            continue;
        }
        match state.foot_modes[i] {
            FootMode::Forced => continue,
            FootMode::Waiting => {
                stance(params, state, i, frame, ground)?;
                continue;
            }
            FootMode::Tripod | FootMode::Hanging => {}
        }
        let u = state.swing_progress[i];
        let start = state.lift_off[i];
        state.feet[i] = match landing(params, state, i, frame, state.leg_heading[i], ground)? {
            Landing::Plant(footing) => {
                state.swing_floor[i] = start[1] + (footing.point[1] - start[1]) * u;
                state.swing_grounded[i] = true;
                FootTarget {
                    position: arc(params, start, footing.point, u),
                    planted: false,
                    normal: footing.normal,
                }
            }
            Landing::Hold(footing) => footing.planted(),
            Landing::Hang => {
                state.swing_grounded[i] = false;
                FootTarget {
                    position: arc(params, start, state.hang_point(params, i, frame), u),
                    planted: false,
                    normal: UP,
                }
            }
        };
    }
    Ok(())
}

/// The swing parabola from `start` to `target` at progress `u`, `swing_lift` high halfway.
fn arc(params: &GaitParams, start: [f32; 3], target: [f32; 3], u: f32) -> [f32; 3] {
    [
        start[0] + (target[0] - start[0]) * u,
        start[1] + (target[1] - start[1]) * u + 4.0 * u * (1.0 - u) * params.swing_lift,
        start[2] + (target[2] - start[2]) * u,
    ]
}

/// A planted foot keeps its XZ and follows floor height; with its ground gone it hangs.
/// A hanging foot follows its hip; a forced foot is left to its own swing.
fn stance(
    params: &GaitParams,
    state: &mut GaitState,
    i: usize,
    frame: Frame,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    match state.foot_modes[i] {
        FootMode::Forced => {}
        FootMode::Hanging => {
            state.feet[i] = FootTarget {
                position: state.hang_point(params, i, frame),
                planted: false,
                normal: UP,
            };
        }
        FootMode::Tripod | FootMode::Waiting => {
            let [x, _, z] = state.feet[i].position;
            let expected = state.expected_floor(params);
            match sample_ground(ground, x, z, expected, params.ground_band)? {
                Some(hit) => {
                    let foot = &mut state.feet[i];
                    foot.position[1] = hit.y;
                    foot.normal = hit.normal;
                    foot.planted = true;
                }
                None => state.feet[i] = state.hang(params, i, frame),
            }
        }
    }
    Ok(())
}

/// Forced steps (ADR-8792): each forced foot swings on its own clock of `settle_period` from
/// where it lifted toward the best foothold around its nominal landing. At touchdown it
/// plants (waiting for its pattern turn if that turn is mid-swing) or, with no valid
/// foothold, hangs. A timestep longer than `settle_period` lands it at once.
fn advance_forced(
    params: &GaitParams,
    state: &mut GaitState,
    frame: Frame,
    dt: f32,
    pattern_swinging: bool,
    ground: &impl GaitGround,
) -> Result<(), GaitError> {
    let progress = dt / params.settle_period;
    if !progress.is_finite() {
        return Err(GaitError::StepTooLarge);
    }
    for i in 0..state.leg_count() {
        if state.foot_modes[i] != FootMode::Forced {
            continue;
        }
        let nominal = nominal_landing(params, state, i, frame, state.swing_heading);
        let query = FootQuery {
            leg: i,
            frame,
            start: Some(state.lift_off[i]),
            check_reach: true,
        };
        let found = choose_foothold(params, state, query, nominal, ground)?;
        let phase = state.forced_phase[i] + progress;
        if phase >= 1.0 {
            state.forced_phase[i] = 0.0;
            state.feet[i] = match found {
                Some((footing, _)) => {
                    state.foot_modes[i] = if pattern_swinging && state.swinging[i] {
                        FootMode::Waiting
                    } else {
                        FootMode::Tripod
                    };
                    footing.planted()
                }
                None => state.hang(params, i, frame),
            };
        } else {
            state.forced_phase[i] = phase;
            let (target, normal) = match found {
                Some((footing, _)) => (footing.point, footing.normal),
                None => (state.hang_point(params, i, frame), UP),
            };
            let start = state.lift_off[i][1];
            state.swing_floor[i] = start + (target[1] - start) * phase;
            state.swing_grounded[i] = found.is_some();
            state.feet[i] = FootTarget {
                position: arc(params, state.lift_off[i], target, phase),
                planted: false,
                normal,
            };
        }
    }
    Ok(())
}

/// Follow the stance, then force a step for every planted foot beyond its `max_reach`; the
/// stance height changes when a foot lifts, so repeat until none is forced (at most once per
/// foot).
fn enforce_reach(params: &GaitParams, state: &mut GaitState, frame: Frame, ease: (f32, f32)) {
    let legs = state.leg_count();
    for _ in 0..legs {
        state.follow_stance(params, Some(ease));
        let mut forced = false;
        for i in 0..legs {
            if state.feet[i].planted && state.reach(params, i, frame) > params.max_reach[i] {
                state.foot_modes[i] = FootMode::Forced;
                state.forced_phase[i] = 0.0;
                state.lift_off[i] = state.feet[i].position;
                state.feet[i].planted = false;
                state.swing_floor[i] = state.feet[i].position[1];
                state.swing_grounded[i] = true;
                state.forced_steps = state.forced_steps.saturating_add(1);
                forced = true;
            }
        }
        if !forced {
            return;
        }
    }
    state.follow_stance(params, Some(ease));
}

impl GaitState {
    /// Whether the idle turn thresholds apply in this step: the yaw changed in it, or in one
    /// of the steps of the last `settle_period` seconds (KI-AN-005, ADR-10130). Without the
    /// hold, a body yaw that changes on only some steps (a 20 Hz heading under a 60 Hz
    /// presentation) switched between the turn thresholds and the tighter settle threshold
    /// every few steps and stepped the feet more often than the same turn every step.
    fn hold_turn(&mut self, params: &GaitParams, dt: f32, turned: bool) -> bool {
        if turned {
            self.turn_hold = params.settle_period;
            return true;
        }
        let held = self.turn_hold > 0.0;
        self.turn_hold = (self.turn_hold - dt).max(0.0);
        held
    }

    /// Start foot `i`'s pattern swing; record lift-off unless it is forced or waiting.
    fn lift_leg(&mut self, i: usize) {
        self.swinging[i] = true;
        self.swing_progress[i] = 0.0;
        self.swing_start[i] = 0.0;
        if matches!(self.foot_modes[i], FootMode::Tripod | FootMode::Hanging) {
            self.lift_off[i] = self.feet[i].position;
        }
    }

    /// Start the pattern swing of every foot in lift-off group `group`.
    fn lift_group(&mut self, schedule: &Schedule, group: usize) {
        for i in 0..self.leg_count() {
            if usize::from(schedule.group[i]) == group {
                self.lift_leg(i);
            }
        }
    }

    /// Swing progress of every swinging foot from the beat clock.
    fn progress_from_schedule(&mut self, schedule: &Schedule) {
        let beat = usize::from(self.beat).min(schedule.groups - 1);
        for i in 0..self.leg_count() {
            self.swing_progress[i] = if self.swinging[i] {
                self.arc_progress(schedule, i, beat, self.phase)
            } else {
                0.0
            };
        }
    }

    /// Arc progress `[0, 1]` of swinging foot `i` at the clock: its scheduled swing
    /// progress (1 once that swing has ended), remapped from its late lift-off point.
    fn arc_progress(&self, schedule: &Schedule, i: usize, beat: usize, phase: f32) -> f32 {
        let scheduled = schedule.swing_progress(i, beat, phase).unwrap_or(1.0);
        self.arc_of(i, scheduled)
    }

    /// Arc progress of swinging foot `i` at scheduled swing progress `scheduled`: the
    /// scheduled progress itself, or for a late lift the remap from its lift-off point.
    fn arc_of(&self, i: usize, scheduled: f32) -> f32 {
        let start = self.swing_start[i];
        if start > 0.0 {
            ((scheduled - start) / (1.0 - start)).clamp(0.0, 1.0)
        } else {
            scheduled
        }
    }

    /// Scheduled swing progress of swinging foot `i`: the inverse of [`Self::arc_of`] (a late
    /// lift's arc mapped back onto its scheduled swing, ADR-10053).
    fn scheduled_swing(&self, i: usize) -> f32 {
        let start = self.swing_start[i];
        let arc = self.swing_progress[i];
        if start > 0.0 {
            (start + arc * (1.0 - start)).min(1.0)
        } else {
            arc
        }
    }

    /// With no swing in progress, pick the lift-off group to step next in pattern order
    /// (the one due to lift first, then the following ones) if it has a foot off neutral with
    /// a valid foothold at or around its neutral (or a hanging foot that found one); jump the
    /// beat clock to it and lift it. Returns whether a swing started.
    fn start_idle_swing(
        &mut self,
        params: &GaitParams,
        frame: Frame,
        turned: bool,
        ground: &impl GaitGround,
    ) -> Result<bool, GaitError> {
        let schedule = Schedule::of(&self.pattern);
        let first = usize::from(self.beat).min(schedule.groups - 1);
        for k in 0..schedule.groups {
            let group = (first + k) % schedule.groups;
            if self.group_due(params, &schedule, group, frame, turned, ground)? {
                self.beat = group as u8;
                self.swing_heading = [0.0; 2];
                for i in 0..self.leg_count() {
                    if usize::from(schedule.group[i]) == group {
                        self.leg_heading[i] = [0.0; 2];
                    }
                }
                self.lift_group(&schedule, group);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether any foot of lift-off group `group` should step to its neutral (every foot of
    /// the group is checked, in foot order).
    fn group_due(
        &self,
        params: &GaitParams,
        schedule: &Schedule,
        group: usize,
        frame: Frame,
        turned: bool,
        ground: &impl GaitGround,
    ) -> Result<bool, GaitError> {
        let cos_limit = if turned {
            det_sin_cos(params.turn_step_angle).1
        } else {
            1.0
        };
        let mut due = false;
        for i in 0..self.leg_count() {
            if usize::from(schedule.group[i]) == group {
                due |= self.off_neutral(params, frame, i, turned, cos_limit, ground)?;
            }
        }
        Ok(due)
    }

    /// Whether foot `i` should step to its neutral: past `settle_distance` at rest, past
    /// `turn_step_distance` or `turn_step_angle` (as `cos_limit`) while turning, and with a
    /// valid foothold: the neutral itself, or a ring point more than `settle_distance` from
    /// the foot (else it would step in place forever). A hanging foot is due once any
    /// foothold is valid; forced, waiting and swinging feet are never due.
    fn off_neutral(
        &self,
        params: &GaitParams,
        frame: Frame,
        i: usize,
        turned: bool,
        cos_limit: f32,
        ground: &impl GaitGround,
    ) -> Result<bool, GaitError> {
        if self.swinging[i] {
            return Ok(false);
        }
        let neutral = hip(self.position, frame, params.hip_offsets[i]);
        let foot = self.feet[i].position;
        let mut query = FootQuery {
            leg: i,
            frame,
            start: Some(foot),
            check_reach: true,
        };
        match self.foot_modes[i] {
            FootMode::Forced | FootMode::Waiting => return Ok(false),
            FootMode::Hanging => {
                query.start = None;
                return Ok(choose_foothold(params, self, query, neutral, ground)?.is_some());
            }
            FootMode::Tripod => {}
        }
        let [hx, hz] = neutral;
        let (dx, dz) = (foot[0] - hx, foot[2] - hz);
        let offset = (dx * dx + dz * dz).sqrt();
        let due = if turned {
            let (ux, uz) = (foot[0] - self.position[0], foot[2] - self.position[2]);
            let (vx, vz) = (hx - self.position[0], hz - self.position[2]);
            let dot = ux * vx + uz * vz;
            offset > params.turn_step_distance
                || dot < cos_limit * ((ux * ux + uz * uz) * (vx * vx + vz * vz)).sqrt()
        } else {
            offset > params.settle_distance
        };
        if !due {
            return Ok(false);
        }
        Ok(match choose_foothold(params, self, query, neutral, ground)? {
            None => false,
            Some((_, true)) => true,
            Some((footing, false)) => {
                let (dx, dz) = (footing.point[0] - foot[0], footing.point[2] - foot[2]);
                (dx * dx + dz * dz).sqrt() > params.settle_distance
            }
        })
    }
}

/// A chosen place to stand.
#[derive(Clone, Copy)]
struct Footing {
    point: [f32; 3],
    normal: [f32; 3],
}

impl Footing {
    fn planted(self) -> FootTarget {
        FootTarget {
            position: self.point,
            planted: true,
            normal: self.normal,
        }
    }
}

/// The outcome of a landing: plant at a foothold, hold the planted lift-off point (no
/// foothold valid), or hang (no ground there either, or the foot was already hanging).
enum Landing {
    Plant(Footing),
    Hold(Footing),
    Hang,
}

/// Which leg a foothold is for and what limits it: the step slope from `start` (none for
/// a hanging foot) and the leg's reach (initial plants skip it).
#[derive(Clone, Copy)]
struct FootQuery {
    leg: usize,
    frame: Frame,
    start: Option<[f32; 3]>,
    check_reach: bool,
}

/// `step_length / 2` along `direction` from the leg's hip under the body.
fn nominal_landing(
    params: &GaitParams,
    state: &GaitState,
    i: usize,
    frame: Frame,
    direction: [f32; 2],
) -> [f32; 2] {
    let center = hip(state.position, frame, params.hip_offsets[i]);
    [
        center[0] + direction[0] * (params.step_length * 0.5),
        center[1] + direction[1] * (params.step_length * 0.5),
    ]
}

fn landing(
    params: &GaitParams,
    state: &GaitState,
    i: usize,
    frame: Frame,
    direction: [f32; 2],
    ground: &impl GaitGround,
) -> Result<Landing, GaitError> {
    let nominal = nominal_landing(params, state, i, frame, direction);
    let planted_start = state.foot_modes[i] != FootMode::Hanging;
    let start = state.lift_off[i];
    let query = FootQuery {
        leg: i,
        frame,
        start: planted_start.then_some(start),
        check_reach: true,
    };
    if let Some((footing, _)) = choose_foothold(params, state, query, nominal, ground)? {
        return Ok(Landing::Plant(footing));
    }
    if planted_start {
        let expected = state.expected_floor(params);
        if let Some(hit) = sample_ground(ground, start[0], start[2], expected, params.ground_band)? {
            return Ok(Landing::Hold(Footing {
                point: [start[0], hit.y, start[2]],
                normal: hit.normal,
            }));
        }
    }
    Ok(Landing::Hang)
}

/// `[right, forward]` unit directions of the points on one search ring, in search order.
const RING: [[f32; 2]; 8] = {
    let d = core::f32::consts::FRAC_1_SQRT_2;
    [
        [1.0, 0.0],
        [d, d],
        [0.0, 1.0],
        [-d, d],
        [-1.0, 0.0],
        [-d, -d],
        [0.0, -1.0],
        [d, -d],
    ]
};

/// Foothold search (ADR-8792): the nominal point if valid; else the first
/// `foothold_candidates` ring points (8 per ring, ring `k` at `k * foothold_spacing`, in
/// `RING` order in the body frame), scored by ring (distance to nominal), then height
/// change from the lift-off point (or from the expected floor for a hanging foot), then
/// search order. The search stops after the first ring that holds a valid point. Returns the
/// footing and whether it is the nominal point.
fn choose_foothold(
    params: &GaitParams,
    state: &GaitState,
    query: FootQuery,
    nominal: [f32; 2],
    ground: &impl GaitGround,
) -> Result<Option<(Footing, bool)>, GaitError> {
    if let Some(footing) = try_foothold(params, state, query, nominal, ground)? {
        return Ok(Some((footing, true)));
    }
    let from = query.start.map_or(state.expected_floor(params), |s| s[1]);
    let (right, forward) = (query.frame.right, query.frame.forward);
    let mut best: Option<(u32, f32, Footing)> = None;
    for k in 0..params.foothold_candidates {
        let ring = k / 8 + 1;
        if best.is_some_and(|(found, _, _)| found < ring) {
            break;
        }
        let [a, b] = RING[(k % 8) as usize];
        let radius = params.foothold_spacing * ring as f32;
        let point = [
            nominal[0] + radius * (a * right[0] + b * forward[0]),
            nominal[1] + radius * (a * right[1] + b * forward[1]),
        ];
        if let Some(footing) = try_foothold(params, state, query, point, ground)? {
            let change = (footing.point[1] - from).abs();
            if best.is_none_or(|(_, least, _)| change < least) {
                best = Some((ring, change, footing));
            }
        }
    }
    Ok(best.map(|(_, _, footing)| (footing, false)))
}

/// A valid foothold at `[x, z]`: ground in the band, a surface no steeper than `max_slope`,
/// a step from the start no steeper than `max_slope`, and within the leg's reach.
fn try_foothold(
    params: &GaitParams,
    state: &GaitState,
    query: FootQuery,
    [x, z]: [f32; 2],
    ground: &impl GaitGround,
) -> Result<Option<Footing>, GaitError> {
    let expected = state.expected_floor(params);
    let Some(hit) = sample_ground(ground, x, z, expected, params.ground_band)? else {
        return Ok(None);
    };
    let point = [x, hit.y, z];
    let n = hit.normal;
    let steep_surface = n[1] <= 0.0 || (n[0] * n[0] + n[2] * n[2]).sqrt() > params.max_slope * n[1];
    let steep_step = query
        .start
        .is_some_and(|start| slope_refused(params, start, point));
    let too_far = query.check_reach && {
        let [hx, hz] = hip(state.position, query.frame, params.hip_offsets[query.leg]);
        let (dx, dy, dz) = (x - hx, state.position[1] - hit.y, z - hz);
        (dx * dx + dy * dy + dz * dz).sqrt() > params.max_reach[query.leg]
    };
    Ok((!steep_surface && !steep_step && !too_far).then_some(Footing { point, normal: n }))
}

/// Whether a footstep from `start` to `target` is steeper than `max_slope`.
fn slope_refused(params: &GaitParams, start: [f32; 3], target: [f32; 3]) -> bool {
    let dx = target[0] - start[0];
    let dz = target[2] - start[2];
    let run = (dx * dx + dz * dz).sqrt().max(1e-3);
    (target[1] - start[1]).abs() / run > params.max_slope
}

/// World XZ axes of a body's local hip offsets `[a, b]`: `a` along `right`, `b` along `forward`.
#[derive(Clone, Copy)]
struct Frame {
    right: [f32; 2],
    forward: [f32; 2],
}

impl Frame {
    /// Both modes rotate by `yaw` about +Y; only the local forward axis (+Z or -Z) differs.
    /// Negations are exact, so each mode's arithmetic matches the original formula bit for bit.
    fn of(mode: GaitMode, yaw: f32) -> Self {
        let (sin, cos) = det_sin_cos(yaw);
        match mode {
            GaitMode::Integrating => Self {
                right: [cos, -sin],
                forward: [sin, cos],
            },
            GaitMode::Driven => Self {
                right: [cos, -sin],
                forward: [-sin, -cos],
            },
        }
    }
}

fn hip(position: [f32; 3], frame: Frame, offset: [f32; 2]) -> [f32; 2] {
    [
        position[0] + frame.right[0] * offset[0] + frame.forward[0] * offset[1],
        position[2] + frame.right[1] * offset[0] + frame.forward[1] * offset[1],
    ]
}

/// A host body's ground speed; one that overflows `f32` is the host's input
/// ([`GaitError::Input`], ADR-8736), not a state fault.
fn body_speed(body: GaitBody) -> Result<f32, GaitError> {
    ground_speed(body.velocity).map_err(|_| GaitError::Input)
}

fn ground_speed(velocity: [f32; 3]) -> Result<f32, GaitError> {
    let speed = (velocity[0] * velocity[0] + velocity[2] * velocity[2]).sqrt();
    if speed.is_finite() {
        Ok(speed)
    } else {
        Err(GaitError::State)
    }
}

/// Query the ground: `None` for no ground or a height outside the band; a nonfinite
/// coordinate, height or normal (or a zero normal) is [`GaitError::Floor`]. The normal comes
/// back unit length (`[0, 1, 0]` stays bit-exact).
fn sample_ground(
    ground: &impl GaitGround,
    x: f32,
    z: f32,
    expected: f32,
    band: f32,
) -> Result<Option<GroundHit>, GaitError> {
    if !x.is_finite() || !z.is_finite() {
        return Err(GaitError::Floor);
    }
    let Some(hit) = ground.sample(x, z, expected, band) else {
        return Ok(None);
    };
    let n = hit.normal;
    let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if !hit.y.is_finite() || !length.is_finite() || length <= 0.0 {
        return Err(GaitError::Floor);
    }
    if (hit.y - expected).abs() > band {
        return Ok(None);
    }
    let normal = if length == 1.0 { n } else { n.map(|v| v / length) };
    Ok(Some(GroundHit { y: hit.y, normal }))
}

fn wrap_yaw(yaw: f32) -> f32 {
    // Bounded work even for very large finite angles; no iterative subtraction.
    let value = yaw % core::f32::consts::TAU;
    if value <= -core::f32::consts::PI {
        value + core::f32::consts::TAU
    } else if value > core::f32::consts::PI {
        value - core::f32::consts::TAU
    } else {
        value
    }
}

// Read-only views for debug draw (the locomotion gizmos, ADR-8729).
mod inspect;
pub use inspect::{FootSwing, MIN_LANDING_PROGRESS, swing_arc_point};
