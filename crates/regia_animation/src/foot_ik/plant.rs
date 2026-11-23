// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/foot_plant.rs, commit d6ed37677), ADR-8012.
// Full source commit d6ed3767703b1cabb12348cb3deb97ac27ac4fcb (tag regia-absorb-1); mark "adapt".
//! Foot planting: foot tilt, plant lock and smoothing on top of the foot-IK planner
//! (`docs/specs/17-animation.md` § 6).
//!
//! [`plan_foot_ik`](super::plan_foot_ik) is stateless: every frame it aims each leg straight at the
//! ground point under its animated foot. That leaves three artifacts a stateful planner removes:
//!
//! * **Popping.** A ground probe that jumps (a stair edge, a rock) moves the IK target in one frame.
//!   [`FootPlanter`] keeps a per-leg ground offset with a dead band, an exponential ease and a slew
//!   limit, eases the pelvis, and fades the whole effect in and out (airborne, prone, ragdoll).
//! * **Flat feet on slopes.** The sole stays level while the ground tilts. The planter turns the
//!   foot toward the ground normal, limited to [`FootPlantSettings::max_tilt`], and lifts the ankle
//!   so the tilted sole still touches the ground.
//! * **Foot skate.** When the clip's planted foot does not move exactly against the character's
//!   motion (a speed the clip was not authored for, a turn, root-motion mismatch), the foot slides
//!   over the ground. The planter pins a planted foot where it touched down, in **world** space, and
//!   holds it there until the clip lifts the foot; its held offset from the clip foot then fades out over
//!   [`FootPlantSettings::lock_release_time`]. A pin that drifts farther than
//!   [`FootPlantSettings::lock_max_drift`] from the animated foot (a teleport, a sharp turn) is set
//!   again where the foot is, so a leg is never stretched. A foot that moves faster than 1.5 times
//!   [`FootPlantSettings::lock_max_speed`] over the ground ends its stance, at any frame rate.
//!   Ground that carries the whole body (a platform: the host's ground sample moved with the rig
//!   this frame) carries the pin with it.
//!
//! A foot counts as planted when the clip holds it within [`FootPlantSettings::plant_lift`] of its
//! bind-pose ankle height (the rig origin sits at the soles), or when the host passes a contact
//! label ([`FootPlantLeg::contact`], e.g. from motion-matching contacts). Between `plant_lift` and
//! [`FootPlantSettings::swing_lift`] the foot blends smoothly from planted to swinging; a swinging
//! foot keeps the clip's own lift over the ground.
//!
//! The output is a [`FootPlant`]: one object-space ankle target and foot tilt per leg plus a pelvis
//! offset. [`apply_foot_plant`] poses a skeleton with it (root joint,
//! [`solve_two_bone`](crate::ik::solve_two_bone), foot rotation); [`FootPlant::ik_plan`] turns it
//! into a [`FootIkPlan`] for graphs that use `TwoBoneIK` nodes. Everything advances only by the
//! supplied `dt` and uses the crate's deterministic `exp`/`atan2`/`sin`/`cos`, so the same inputs
//! give the same bits on one target (a replay matches exactly). Across architectures the default
//! build differs: glam's SSE2 and NEON paths sum and fuse differently (ADR-5176). With the
//! `cross-platform-determinism` feature (glam's scalar path) x86_64 and aarch64 give the same bits,
//! pinned by `plant_scenario_digest_is_pinned_across_architectures` (ADR-8728).
//!
//! Steady state allocates nothing: [`FootPlanter::plan_into`] fills a caller-kept [`FootPlant`],
//! [`FootPlanter::report_into`] a caller-kept [`FootPlantReport`], and
//! [`apply_foot_plant_into`] refreshes forward kinematics along each leg chain in a caller-kept
//! buffer instead of rebuilding every joint per leg.
//!
//! Provenance: adapted from Direct Order 27's foot planner (ADR-8012, `docs/ported-sources.toml`):
//! the dead-band, ease and slew smoothing, the tilt toward the ground normal with the
//! `ankle / cos(tilt)` lift, the lift-based planted weight and the reach-limited pelvis drop. Regia
//! changed: world-space input (any rig transform), any number of legs, the constants as
//! [`FootPlantSettings`], `nlerp` instead of `slerp`, deterministic transcendentals, and the
//! horizontal plant lock (new; the source pinned the foot vertically only).

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;

use super::lock::{PlantLock, PlantRelease};
use super::shape::{FootSole, SoleFrame, SoleTuning};
use super::{FootGround, FootIkLeg, FootIkLegPlan, FootIkPlan};
use crate::ik::{refresh_global, solve_two_bone_with_globals};
use crate::num::{det_atan2, det_exp, det_sin_cos_f64, finite_or};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Tuning for a [`FootPlanter`]. Lengths are meters in the rig's object space, rates per second,
/// times seconds. Every value read from data is sanitized: a negative or non-finite value falls
/// back to a safe one, so the planter never panics.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize, Reflect)]
#[serde(default)]
#[reflect(Default)]
pub struct FootPlantSettings {
    /// Ground offsets beyond this, up or down, are clamped (a cliff edge is not a step). Default 0.6.
    pub max_ground_offset: f32,
    /// The most the pelvis sinks. Default 0.35.
    pub max_pelvis_drop: f32,
    /// Largest foot tilt toward the ground normal, radians. Default 0.436 (25 degrees).
    pub max_tilt: f32,
    /// The clip foot counts as planted below this lift over its bind ankle height. Default 0.03.
    pub plant_lift: f32,
    /// Above this lift the clip foot is swinging and keeps its own height. Default 0.08.
    pub swing_lift: f32,
    /// The leg never extends past this fraction of its length, so the knee keeps a slight bend.
    /// Default 0.995.
    pub max_extension: f32,
    /// Effect weight rise per second when the planter turns active. Default 5.
    pub blend_in_per_s: f32,
    /// Effect weight fall per second when the planter turns inactive. Default 10.
    pub blend_out_per_s: f32,
    /// Time constant of the ground-offset ease. Default 0.05.
    pub offset_tau: f32,
    /// Time constant of the pelvis ease. Default 0.07.
    pub pelvis_tau: f32,
    /// Time constant of the foot-tilt ease. Default 0.08.
    pub tilt_tau: f32,
    /// A ground sample must move more than this before the foot follows (hysteresis). Default 0.004.
    pub deadband: f32,
    /// The ground offset never moves faster than this, meters per second. Default 3.5.
    pub max_slew: f32,
    /// Pin planted feet in world space (the plant lock). Default `true`.
    pub lock: bool,
    /// A pin farther than this from the animated foot, horizontally, is set again where the foot is.
    /// Default 0.3.
    pub lock_max_drift: f32,
    /// Seconds over which a released pin hands the foot back to the clip (0 releases at once).
    /// Default 0.15.
    pub lock_release_time: f32,
    /// A clip foot moving faster than this over the ground, meters per second, is swinging even
    /// when it is low: it is not pinned at touchdown, and a stance ends above 1.5 times this.
    /// Default 1.0.
    pub lock_max_speed: f32,
    /// Heel and toe ground heights that differ by more than this, meters, over ground that is
    /// not a ramp, are a step (the stair rule of [`FootPlanter::plan_with_soles`]). Default 0.04.
    pub step_edge: f32,
    /// On a step, the foot rests flat on the higher tread when at least this fraction of its
    /// heel-to-toe length is over it; otherwise it moves onto the tread it mostly covers.
    /// Default 0.5.
    pub min_support: f32,
    /// The farthest the stair rule moves a foot along its heel-to-toe axis, meters. A foot that
    /// would need more rests on the higher tread. Default 0.12.
    pub max_edge_shift: f32,
}

impl Default for FootPlantSettings {
    fn default() -> Self {
        Self {
            max_ground_offset: 0.6,
            max_pelvis_drop: 0.35,
            max_tilt: 0.436,
            plant_lift: 0.03,
            swing_lift: 0.08,
            max_extension: 0.995,
            blend_in_per_s: 5.0,
            blend_out_per_s: 10.0,
            offset_tau: 0.05,
            pelvis_tau: 0.07,
            tilt_tau: 0.08,
            deadband: 0.004,
            max_slew: 3.5,
            lock: true,
            lock_max_drift: 0.3,
            lock_release_time: 0.15,
            lock_max_speed: 1.0,
            step_edge: 0.04,
            min_support: 0.5,
            max_edge_shift: 0.12,
        }
    }
}

/// A stance ends when the planted weight falls below this (the foot lifted about a fifth of the way
/// from `plant_lift` to `swing_lift`).
const STANCE_EXIT: f32 = 0.9;

/// A released foot stands on the tread it lifted from until it is this far from the lift point,
/// horizontally, meters. Going down a stair the ankle lifts near the nosing, so a few centimeters
/// on the foot is over the drop: a longer reach floats the sole over the lower tread, a shorter
/// one lets it dip into the edge of the tread it is leaving (ADR-8801).
const LIFT_FLOOR_REACH: f32 = 0.05;

/// A ground sample that moved within this fraction of the rig's own step moved with the rig
/// (a platform carried the body), so the pin moves with it.
const CARRY_MATCH: f32 = 0.1;

/// A pin that drifted past `lock_max_drift` was left behind by a jump of the rig when the clip
/// foot moved at most this share of its world travel relative to the body.
const RIG_JUMP_SHARE: f32 = 0.5;

/// [`FootPlantSettings`] with every value made safe to compute with.
#[derive(Clone, Copy, Debug)]
struct Tuning {
    max_offset: f32,
    max_drop: f32,
    max_tilt: f32,
    plant_lift: f32,
    swing_lift: f32,
    max_extension: f32,
    blend_in: f32,
    blend_out: f32,
    offset_tau: f32,
    pelvis_tau: f32,
    tilt_tau: f32,
    deadband: f32,
    max_slew: f32,
    lock: bool,
    max_drift: f32,
    release: f32,
    max_speed: f32,
    sole: SoleTuning,
}

impl Tuning {
    fn from(s: &FootPlantSettings) -> Self {
        let pos = |v: f32, fallback: f32| finite_or(v, fallback).abs();
        let plant_lift = pos(s.plant_lift, 0.03);
        let max_tilt = pos(s.max_tilt, 0.436).min(std::f32::consts::FRAC_PI_2);
        Self {
            max_offset: pos(s.max_ground_offset, 0.6),
            max_drop: pos(s.max_pelvis_drop, 0.35),
            max_tilt,
            plant_lift,
            swing_lift: pos(s.swing_lift, 0.08).max(plant_lift + 1.0e-4),
            max_extension: pos(s.max_extension, 0.995).min(1.0),
            blend_in: pos(s.blend_in_per_s, 5.0),
            blend_out: pos(s.blend_out_per_s, 10.0),
            offset_tau: pos(s.offset_tau, 0.05),
            pelvis_tau: pos(s.pelvis_tau, 0.07),
            tilt_tau: pos(s.tilt_tau, 0.08),
            deadband: pos(s.deadband, 0.004),
            max_slew: pos(s.max_slew, 3.5),
            lock: s.lock,
            max_drift: if s.lock_max_drift.is_finite() {
                s.lock_max_drift.abs()
            } else {
                f32::INFINITY
            },
            release: pos(s.lock_release_time, 0.15),
            max_speed: pos(s.lock_max_speed, 1.0),
            sole: SoleTuning::from(s, max_tilt),
        }
    }
}

/// One leg as the planter sees it this frame, **world** space (the clip pose before any IK).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootPlantLeg {
    /// The hip (chain root joint) position.
    pub hip: Vec3,
    /// The animated ankle (chain tip joint) position.
    pub foot: Vec3,
    /// Thigh plus shin length, meters.
    pub length: f32,
    /// Bind-pose height of the ankle joint over the sole, meters (object space).
    pub ankle_height: f32,
    /// The sole point on the ground under the foot ([`FootGround::None`] = the flat floor the clip
    /// assumes, at the rig origin's height).
    pub ground: FootGround,
    /// The ground normal under the foot (`Vec3::Y` when unknown; a zero or downward normal tilts
    /// nothing).
    pub normal: Vec3,
    /// A contact weight from the host, `[0, 1]` (1 = planted; values outside are clamped): the
    /// clip's cooked `contact.<ankle joint>` curve as the graph blended it (see
    /// [`contact_weight`](super::contact::contact_weight)), or a motion-matching contact label.
    /// `None` (or a non-finite value) derives it from the clip's foot lift over `ankle_height`.
    pub contact: Option<f32>,
}

impl FootPlantLeg {
    /// A leg on the clip's flat floor with no contact label: set `ground` and `normal` from a
    /// probe.
    pub fn new(hip: Vec3, foot: Vec3, length: f32, ankle_height: f32) -> Self {
        Self {
            hip,
            foot,
            length,
            ankle_height,
            ground: FootGround::None,
            normal: Vec3::Y,
            contact: None,
        }
    }
}

/// One frame of input for [`FootPlanter::plan`].
#[derive(Clone, Copy, Debug)]
pub struct FootPlantInput<'a> {
    /// The rig entity's world translation.
    pub rig_translation: Vec3,
    /// The rig entity's world rotation.
    pub rig_rotation: Quat,
    /// One entry per leg, in a fixed order across frames.
    pub legs: &'a [FootPlantLeg],
    /// `false` in the air, prone, sliding, vaulting, ragdolled or dead: the effect fades out.
    pub active: bool,
    /// Seconds since the previous call.
    pub dt: f32,
}

/// What the planter decided for one leg.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootPlantLegOut {
    /// Ankle target, object space, for a rig that is **not** moved by the pelvis offset (the clip
    /// tip at weight 0).
    pub target: Vec3,
    /// Rotation to pre-multiply onto the foot joint's object-space rotation (identity when flat).
    pub tilt: Quat,
    /// 1 for a planted foot, 0 for a swinging one (and 0 for every foot while the effect is faded out).
    pub planted: f32,
    /// Weight of the world-space pin on this foot, `[0, 1]` (falls to 0 over the release time).
    pub lock: f32,
    /// Rotation to pre-multiply onto the toe joint's object-space rotation after the foot tilt
    /// (the toe bend, [`apply_toe_bend`]); identity without a ball in the leg's [`FootSole`].
    pub toe: Quat,
    /// Height of the placed heel over the surface under it, meters (negative: below the
    /// ground). A heel hanging past a step edge the sole rests on is measured to that edge's
    /// tread. 0 for a swinging foot or a leg without heel and toe samples.
    pub heel_gap: f32,
    /// Height of the placed toe tip over the surface under it, meters, as `heel_gap`.
    pub toe_gap: f32,
}

/// The plan for one body this frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FootPlant {
    /// Y offset for the pelvis (root joint), object space, zero or negative.
    pub pelvis_offset: f32,
    /// Effect weight `[0, 1]`.
    pub weight: f32,
    /// One entry per input leg.
    pub legs: Vec<FootPlantLegOut>,
}

impl FootPlant {
    /// True when the plan changes nothing.
    pub fn is_identity(&self) -> bool {
        self.weight <= 0.0
    }

    /// The plan as a [`FootIkPlan`], for graphs whose `TwoBoneIK` nodes read the leg targets and a
    /// rig entity moved by [`FootIkPlan::pelvis_offset`] (the targets are re-based for the moved
    /// rig, as [`plan_foot_ik`](super::plan_foot_ik) does). The foot tilt is not part of it: apply
    /// [`FootPlantLegOut::tilt`] to the foot joint, or use [`apply_foot_plant`].
    pub fn ik_plan(&self) -> FootIkPlan {
        let weight = if self.weight > 0.0 { 1.0 } else { 0.0 };
        FootIkPlan {
            pelvis_offset: self.pelvis_offset,
            legs: self
                .legs
                .iter()
                .map(|leg| FootIkLegPlan {
                    target: leg.target - Vec3::Y * self.pelvis_offset,
                    weight,
                })
                .collect(),
        }
    }
}

/// Per-leg smoothing and lock state.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LegState {
    /// Smoothed ground, and the sample it is moving toward, both world-space heights.
    ///
    /// Stored in world Y rather than object Y so a rig that climbs a slope does not drag the
    /// sole behind it: the object height is rebuilt from this value and the current rig each
    /// frame. A planted foot whose sample point moved stands on the new ground immediately;
    /// a sample that stayed put is still limited by `max_slew` (a platform must not pop).
    goal: f32,
    offset: f32,
    tilt: Quat,
    /// The world-space plant lock (the crate's one lock state machine; only the pin's x and z
    /// are used).
    lock: PlantLock,
    /// World-space target when the stance began (or the pin was set again), for the slide report.
    slide_from: Option<Vec3>,
    slide: f32,
    /// The clip foot's world position last frame, for its ground speed.
    last: Option<Vec3>,
    /// World xz of the ground sample last frame. A planted foot that moved to a new sample
    /// stands on that ground at once; a sample that stayed put keeps the slew limit.
    sample_xz: Option<Vec3>,
    /// The stair rule's smoothed move along the foot axis, meters (heel and toe legs only).
    shift: f32,
    /// The clip foot's object-space position last frame (before the lock), for telling a jump
    /// of the rig (the foot keeps its place on the body) from a fast foot.
    last_object: Option<Vec3>,
    /// While a released pin hands the foot back: the ground under the pin when the stance
    /// ended (world point, height = the smoothed ground) and the ground normal there. The
    /// placed foot stands on it while it is within `LIFT_FLOOR_REACH` of that point.
    floor: Option<(Vec3, Vec3)>,
}

impl Default for LegState {
    fn default() -> Self {
        Self {
            goal: 0.0,
            offset: 0.0,
            tilt: Quat::IDENTITY,
            lock: PlantLock::default(),
            slide_from: None,
            slide: 0.0,
            last: None,
            sample_xz: None,
            shift: 0.0,
            last_object: None,
            floor: None,
        }
    }
}

/// Stateful foot planter: keep one per character and call [`FootPlanter::plan`] once per frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FootPlanter {
    /// The tuning.
    pub settings: FootPlantSettings,
    weight: f32,
    primed: bool,
    pelvis: f32,
    legs: Vec<LegState>,
    /// The rig translation last frame (moving-ground carry).
    last_origin: Option<Vec3>,
}

/// `1 - e^(-dt/tau)`: the fraction of the way an exponential ease covers in `dt`.
fn ease(dt: f32, tau: f32) -> f32 {
    1.0 - det_exp(-dt.max(0.0) / tau.max(1.0e-4))
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Rotation that takes +Y onto `normal`, at most `max` radians. Identity for a flat, degenerate,
/// downward or non-finite normal. Platform-independent (no platform trigonometry).
pub fn align_to_normal(normal: Vec3, max: f32) -> Quat {
    let n = normal.normalize_or_zero();
    if !n.is_finite() || n.y <= 1.0e-4 {
        return Quat::IDENTITY;
    }
    let horizontal = (n.x * n.x + n.z * n.z).sqrt();
    let angle = det_atan2(horizontal, n.y).min(finite_or(max, 0.0).abs());
    // +Y × n = (n.z, 0, -n.x).
    let axis = Vec3::new(n.z, 0.0, -n.x);
    if angle < 1.0e-5 || axis.length_squared() < 1.0e-12 {
        return Quat::IDENTITY;
    }
    let axis = axis.normalize();
    let (sin, cos) = det_sin_cos_f64(f64::from(angle) * 0.5);
    let sin = sin as f32;
    Quat::from_xyzw(axis.x * sin, axis.y * sin, axis.z * sin, cos as f32).normalize()
}

/// The rotation angle of a unit quaternion, radians, platform-independent.
pub fn tilt_angle(q: Quat) -> f32 {
    let v = (q.x * q.x + q.y * q.y + q.z * q.z).sqrt();
    2.0 * det_atan2(v, q.w.abs())
}

/// `cos` of the rotation angle of a unit quaternion (`2w² - 1`, no trigonometry).
fn cos_angle(q: Quat) -> f32 {
    (2.0 * q.w * q.w - 1.0).clamp(-1.0, 1.0)
}

/// Largest pelvis offset (zero or negative) that keeps `target` within `len * extension` of a hip
/// lowered by that offset. Zero when the target is out of reach sideways (no drop helps).
pub fn pelvis_for_reach(hip: Vec3, len: f32, extension: f32, target: Vec3) -> f32 {
    let reach = len * extension;
    let horizontal = Vec3::new(target.x - hip.x, 0.0, target.z - hip.z).length();
    if horizontal.is_nan() || horizontal >= reach {
        return 0.0;
    }
    let max_drop_gap = (reach * reach - horizontal * horizontal).sqrt();
    // hip.y + p - target.y <= max_drop_gap
    (target.y + max_drop_gap - hip.y).min(0.0)
}

impl FootPlanter {
    /// A planter with `settings`, faded out, with no leg state yet.
    pub fn new(settings: FootPlantSettings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }

    /// Forget every smoothed value and pin (after a teleport or a respawn).
    pub fn reset(&mut self) {
        self.weight = 0.0;
        self.primed = false;
        self.pelvis = 0.0;
        self.legs.clear();
        self.last_origin = None;
    }

    /// The current effect weight.
    pub fn weight(&self) -> f32 {
        self.weight
    }

    /// The world-space pin of leg `leg` while the stance holds the foot there, else `None`. Hosts
    /// probe the ground under the pin instead of under the animated foot while it is held.
    ///
    /// A released pin that still hands the foot back to the clip (its lock weight falling to 0)
    /// is not reported: the foot is on its way to the clip foot, so hosts probe under the clip
    /// foot and the planter sees the ground the foot is going to (the next stair tread). It
    /// never lets the foot sink below the ground it lifted from on the way.
    pub fn pin(&self, leg: usize) -> Option<Vec3> {
        self.legs
            .get(leg)
            .filter(|state| state.lock.is_down() && state.lock.pinned() && state.lock.weight() > 0.0)
            .map(|state| state.lock.pin())
    }

    /// Moves leg `leg`'s pin with the ground under it by `delta` (world space, only x and z are
    /// used): the host's ground moved this frame, e.g. a platform's velocity at the probe hit
    /// times `dt` ([`FootPlacement`](super::component::FootPlacement) does this with
    /// [`GroundProbe::probe_moving`](super::component::GroundProbe::probe_moving)). Call it
    /// before [`FootPlanter::plan`]. Only a foot in stance is carried. The planter's own
    /// moving-ground rule then sees the last sample where the pin went, so the same motion is not
    /// carried twice. A non-finite delta, or a leg that does not exist yet, does nothing.
    pub fn carry(&mut self, leg: usize, delta: Vec3) {
        let delta = Vec3::new(delta.x, 0.0, delta.z);
        if !delta.is_finite() || delta == Vec3::ZERO {
            return;
        }
        let Some(state) = self.legs.get_mut(leg) else {
            return;
        };
        if !state.lock.is_down() {
            return;
        }
        state.lock.carry(delta);
        if let Some(last) = state.last.as_mut() {
            *last += delta;
        }
        if let Some(from) = state.slide_from.as_mut() {
            *from += delta;
        }
        if let Some(sample) = state.sample_xz.as_mut() {
            *sample += delta;
        }
    }

    /// How far, horizontally, leg `leg`'s world-space target has moved since the foot last touched
    /// down (meters; 0 for a leg that is not planted). With the lock on it stays near zero.
    pub fn slide(&self, leg: usize) -> f32 {
        self.legs.get(leg).map_or(0.0, |state| state.slide)
    }

    /// The agent-readable summary of `plan` (the latest result of [`FootPlanter::plan`]).
    /// Allocates a new report; [`FootPlanter::report_into`] refills one.
    pub fn report(&self, plan: &FootPlant) -> FootPlantReport {
        let mut report = FootPlantReport::default();
        self.report_into(plan, &mut report);
        report
    }

    /// [`FootPlanter::report`] into `report`, reusing its vectors: once they have grown to the
    /// leg count, a frame allocates nothing.
    pub fn report_into(&self, plan: &FootPlant, report: &mut FootPlantReport) {
        report.weight = plan.weight;
        report.pelvis_offset = plan.pelvis_offset;
        report.planted.clear();
        report.lock.clear();
        report.tilt_deg.clear();
        report.slide.clear();
        report.heel_gap.clear();
        report.toe_gap.clear();
        for (i, leg) in plan.legs.iter().enumerate() {
            report.planted.push(leg.planted);
            report.lock.push(leg.lock);
            report.tilt_deg.push(tilt_angle(leg.tilt).to_degrees());
            report.slide.push(self.slide(i));
            report.heel_gap.push(leg.heel_gap);
            report.toe_gap.push(leg.toe_gap);
        }
        report.max_slide = report.slide.iter().copied().fold(0.0, f32::max);
    }

    /// Plans one frame (the module docs describe the strategy). Allocates the returned plan;
    /// [`FootPlanter::plan_into`] refills one.
    pub fn plan(&mut self, input: &FootPlantInput<'_>) -> FootPlant {
        let mut plan = FootPlant::default();
        self.plan_with_soles_into(input, &[], &mut plan);
        plan
    }

    /// [`FootPlanter::plan`] into `out`, reusing `out.legs`: after the first frames (the leg
    /// state and `out.legs` grown to the leg count) a frame allocates nothing. The result is
    /// bit-identical to [`FootPlanter::plan`].
    pub fn plan_into(&mut self, input: &FootPlantInput<'_>, out: &mut FootPlant) {
        self.plan_with_soles_into(input, &[], out);
    }

    /// Plans one frame with heel and toe samples: `soles[i]` belongs to `input.legs[i]`
    /// (module [`shape`](super::shape)). A leg with no entry, or with
    /// [`FootSole::default`], is planned exactly as [`FootPlanter::plan`] plans it. A leg with
    /// both samples takes its pitch from them, follows the stair rule, keeps every planted
    /// sole point on or above the ground, and bends its toe when the sole has a ball.
    pub fn plan_with_soles(&mut self, input: &FootPlantInput<'_>, soles: &[FootSole]) -> FootPlant {
        let mut plan = FootPlant::default();
        self.plan_with_soles_into(input, soles, &mut plan);
        plan
    }

    /// [`FootPlanter::plan_with_soles`] into `out`, reusing `out.legs` (see
    /// [`FootPlanter::plan_into`]).
    pub fn plan_with_soles_into(
        &mut self,
        input: &FootPlantInput<'_>,
        soles: &[FootSole],
        out: &mut FootPlant,
    ) {
        let _span = tracing::debug_span!(target: "regia_animation", "foot_plant_plan").entered();
        let t = Tuning::from(&self.settings);
        let dt = finite_or(input.dt, 0.0).max(0.0);
        let rotation = if input.rig_rotation.is_finite() && input.rig_rotation.length_squared() > 1.0e-12 {
            input.rig_rotation.normalize()
        } else {
            Quat::IDENTITY
        };
        let origin = if input.rig_translation.is_finite() {
            input.rig_translation
        } else {
            Vec3::ZERO
        };
        let inverse = rotation.inverse();
        let to_object = |p: Vec3| inverse * (p - origin);
        let to_world = |p: Vec3| rotation * p + origin;
        let n = input.legs.len();
        self.legs.resize(n, LegState::default());

        // Effect weight: linear rise and fall.
        let (goal_w, rate) = if input.active {
            (1.0, t.blend_in)
        } else {
            (0.0, t.blend_out)
        };
        let step = rate * dt;
        self.weight = if self.weight < goal_w {
            (self.weight + step).min(goal_w)
        } else {
            (self.weight - step).max(goal_w)
        };

        // Clip pose in object space; a non-finite leg plans the clip pose.
        let tip_of = |leg: &FootPlantLeg| {
            let tip = to_object(leg.foot);
            if tip.is_finite() { tip } else { Vec3::ZERO }
        };
        out.legs.clear();

        let rig_step = self.last_origin.map_or(Vec3::ZERO, |last| origin - last);
        self.last_origin = Some(origin);

        if self.weight <= 0.0 {
            // Forget the old ground so the next landing starts from what is under the feet.
            // The effect is off (airborne, prone, ragdolled), so no foot is planted.
            self.primed = false;
            self.pelvis = 0.0;
            for state in &mut self.legs {
                *state = LegState::default();
            }
            out.pelvis_offset = 0.0;
            out.weight = 0.0;
            out.legs.extend(input.legs.iter().map(|leg| FootPlantLegOut {
                target: tip_of(leg),
                tilt: Quat::IDENTITY,
                planted: 0.0,
                lock: 0.0,
                toe: Quat::IDENTITY,
                heel_gap: 0.0,
                toe_gap: 0.0,
            }));
            return;
        }
        let w = self.weight;

        // A fresh landing starts the smoothed ground at the ground under each foot.
        let prime = !self.primed;
        self.primed = true;

        out.legs.reserve(n);
        let mut need = 0.0_f32;
        // The pelvis a planted heel-and-toe leg needs; `None` while no such leg is planted.
        let mut sole_need: Option<f32> = None;
        for (i, leg) in input.legs.iter().enumerate() {
            let tip = tip_of(leg);
            let ankle = finite_or(leg.ankle_height, 0.0).max(0.0);
            let ankle_ground = match leg.ground {
                FootGround::Contact(point) => Some(point),
                FootGround::None => None,
            };
            // Heel and toe samples (module `shape`); `None` plans the leg as a single probe.
            let frame = soles
                .get(i)
                .and_then(|sole| SoleFrame::build(sole, to_object, |d| inverse * d, ankle_ground, &t.sole));
            let state = &mut self.legs[i];

            // With a sole the clip's lift is its lowest sole point over the floor, so a heel
            // the clip lifts about a planted ball keeps the foot planted until the ball lifts.
            let lift = match &frame {
                Some(sole) => sole.clearance(tip.y),
                None => tip.y - ankle,
            };
            let planted = match leg.contact {
                Some(weight) if weight.is_finite() => weight.clamp(0.0, 1.0),
                _ => 1.0 - smoothstep(t.plant_lift, t.swing_lift, lift),
            };
            // World-space ground height. No contact tracks the rig origin, which is object
            // offset 0. A leg with a sole uses the height its heel, ball and toe ask for at the
            // ankle (pitch and stair rule), expressed as ground under a tilted ankle.
            let mut ground = match leg.ground {
                FootGround::Contact(point) if point.y.is_finite() => point.y,
                _ => origin.y,
            };
            let sole_goal = frame.as_ref().map(|sole| {
                let pitch = sole.pitch(&t.sole);
                let tilt = sole.tilt(to_object_dir(inverse, leg.normal), pitch, &t.sole);
                let tip_offset = sole.tip(sole.heel_lift() * planted);
                let placed = |shift: f32| {
                    let at = sole.anchor() + sole.forward() * shift;
                    Vec3::new(at.x, sole.support(tilt, at, tip_offset), at.z)
                };
                // The stair rule never moves a foot out of the leg's reach (from a hip lowered by
                // the largest pelvis drop): when the move would overextend the leg and resting on
                // the higher tread would not, the foot rests on the higher tread.
                let mut shift = sole.shift(&t.sole);
                let hip = to_object(leg.hip) - Vec3::Y * t.max_drop;
                let reach = finite_or(leg.length, 0.0).abs() * t.max_extension;
                if shift != 0.0 && hip.is_finite() && reach > 0.0 {
                    let moved = (placed(shift) - hip).length();
                    if moved > reach && (placed(0.0) - hip).length() < moved {
                        shift = 0.0;
                    }
                }
                let anchor = sole.anchor() + sole.forward() * shift;
                let support = sole.support(tilt, anchor, tip_offset);
                let under = support - ankle / cos_angle(tilt).max(0.5);
                let world = to_world(Vec3::new(anchor.x, under, anchor.z)).y;
                if world.is_finite() {
                    ground = world;
                }
                (pitch, shift)
            });
            // Where the ground was measured (world xz): the host's contact point, else the
            // clip foot.
            let sample_xz = match leg.ground {
                FootGround::Contact(point) => Vec3::new(point.x, 0.0, point.z),
                _ => Vec3::new(leg.foot.x, 0.0, leg.foot.z),
            };
            let previous_sample = state.sample_xz;
            // Moving ground (a platform, a conveyor). When the sample the host measured moved
            // with the rig this frame, the ground under the foot carried the whole body: the
            // pin moves with it. A clip that skates over still ground (the rig stays put) and a
            // walk (the rig moves, the planted foot and its sample stay) keep the world pin.
            if let (FootGround::Contact(_), Some(previous)) = (leg.ground, previous_sample)
                && state.lock.is_down()
            {
                let rig = Vec3::new(rig_step.x, 0.0, rig_step.z);
                let rig_len = rig.length();
                let ground_step = sample_xz - previous;
                if rig_len > 1.0e-5
                    && rig_len.is_finite()
                    && horizontal_distance(ground_step, rig) <= CARRY_MATCH * rig_len
                {
                    state.lock.carry(rig);
                    if let Some(last) = state.last.as_mut() {
                        *last += rig;
                    }
                    if let Some(from) = state.slide_from.as_mut() {
                        *from += rig;
                    }
                }
            }
            // A released pin is no longer the probe point (`FootPlanter::pin`): the host
            // measured under the clip foot, where the foot is going. The ground the foot lifted
            // from is a floor under the placed foot until the release ends (applied below).
            let releasing = state.lock.pinned() && !state.lock.is_down();
            if !releasing {
                state.floor = None;
            }
            if prime {
                state.goal = ground;
                state.offset = ground;
            }
            // Ground offset: hysteresis, then a slew-limited ease. The height is world Y, so a
            // rig that climbs does not drag a pinned sole behind it. A planted foot that moved
            // to a new sample (the next step on a slope) stands on that ground this frame. A
            // sample that stayed put, and every swing, stays inside `max_slew` so a platform
            // or a cliff cannot pop the target.
            if (ground - state.goal).abs() > t.deadband {
                state.goal = ground;
            }
            let moved = previous_sample
                .map(|previous| horizontal_distance(previous, sample_xz))
                .unwrap_or(f32::MAX);
            state.sample_xz = Some(sample_xz);
            let want = state.goal - state.offset;
            let cap = (t.max_slew * dt).max(1.0e-6);
            let mut step = if planted > 0.5 {
                want
            } else {
                want * ease(dt, t.offset_tau)
            };
            // A sample that did not move (a platform, a cliff under a still foot) keeps the
            // slew cap. Any real horizontal move is a new place on the ground, so a planted
            // foot stands on it this frame instead of easing in over the step.
            if !(planted > 0.5 && moved > 1.0e-4) {
                step = step.clamp(-cap, cap);
            }
            state.offset += step;

            // Tilt toward the ground normal, only as much as the foot is planted.
            let normal = to_object_dir(inverse, leg.normal);
            let aligned = match (&frame, sole_goal) {
                (Some(sole), Some((pitch, _))) => sole.tilt(normal, pitch, &t.sole),
                _ => align_to_normal(normal, t.max_tilt),
            };
            let goal_tilt = Quat::IDENTITY.lerp(aligned, planted);
            state.tilt = state.tilt.lerp(goal_tilt, ease(dt, t.tilt_tau)).normalize();
            let cos_tilt = cos_angle(state.tilt).max(0.5);
            let held = lift.max(0.0) * (1.0 - planted);
            // The host probed under the pin while the stance held it, else under the clip foot.
            let held_pin = state.lock.is_down() && state.lock.pinned();
            let sample = if held_pin { state.lock.pin() } else { leg.foot };
            let mut object_ground = to_object(Vec3::new(sample.x, state.offset, sample.z)).y;
            if !object_ground.is_finite() {
                object_ground = 0.0;
            }
            object_ground = object_ground.clamp(-t.max_offset, t.max_offset);
            let full = Vec3::new(tip.x, object_ground + ankle / cos_tilt + held, tip.z);

            // Plant lock, world space. A stance starts when the foot is fully planted and ends as
            // soon as the clip lifts it past `STANCE_EXIT` (a small hysteresis band, so a foot at
            // the threshold does not flicker). Holding the pin until the foot is half-lifted would
            // drag it through the start of the swing.
            // A low but fast foot is still swinging (a shuffle, the start and end of a flat swing):
            // a stance ends above 1.5 times `max_speed`, whatever one frame's travel is, so the
            // decision is the same at every frame rate. A pin left farther behind than
            // `max_drift` by a jump of the rig (a teleport, a snap turn: the clip foot kept its
            // place on the body) stays down and re-pins.
            let full_world = to_world(full);
            let world_step = state.last.map(|last| horizontal_distance(last, full_world));
            let speed = match world_step {
                Some(step) if dt > 0.0 => step / dt,
                _ => 0.0,
            };
            let object_step = state.last_object.map(|last| horizontal_distance(last, full));
            state.last = Some(full_world);
            state.last_object = Some(full);
            let teleport = state
                .lock
                .drifted(horizontal_distance(state.lock.pin(), full_world), t.max_drift);
            let rig_jump = teleport
                && match (object_step, world_step) {
                    (Some(body), Some(world)) => body <= RIG_JUMP_SHARE * world,
                    _ => true,
                };
            let was_down = state.lock.is_down();
            let touchdown = !was_down && planted >= 1.0 - 1.0e-3 && speed <= t.max_speed;
            let down = if was_down {
                planted >= STANCE_EXIT && (rig_jump || speed <= t.max_speed * 1.5)
            } else {
                touchdown
            };
            let lift_pin = state.lock.pin();
            if t.lock {
                // A touchdown always pins where the foot landed, also while an older pin is still
                // releasing; a pin left too far behind (a teleport) is set again. When the stance
                // ends, the held offset from the moving clip foot fades (`PlantRelease::Follow`),
                // rather than the target blending toward a fixed pin (which, with the foot already
                // swinging away, would catch up at twice the swing speed).
                let step = state
                    .lock
                    .update(down, teleport, full_world, dt, t.release, PlantRelease::Follow);
                if step.repinned {
                    state.slide_from = None;
                }
                if step.lifted {
                    // The ground the foot lifts from: the host probed under the pin this frame.
                    state.floor = Some((Vec3::new(lift_pin.x, state.offset, lift_pin.z), leg.normal));
                }
            } else {
                state.lock.unlock(down);
            }
            let full = if state.lock.weight() > 0.0 {
                // Held, or releasing (the pin then rides with the foot by the fading offset).
                let pin = state.lock.pin();
                to_object(Vec3::new(pin.x, full_world.y, pin.z))
            } else {
                full
            };
            // A release slides the foot horizontally toward the clip. On a slope that move
            // changes the ground height. Keep the smoothed height at the sample and add the
            // plane's rise out to the placed point, so the sole stays on the ground. A
            // vertical normal (flat ground) or an unmoved sample adds nothing, and the
            // target stays bit-identical.
            let placed_world = to_world(full);
            let rise = slope_rise(
                Vec3::new(sample.x, state.offset, sample.z),
                leg.normal,
                placed_world.x,
                placed_world.z,
                t.max_tilt,
            );
            let mut full = if rise == 0.0 {
                full
            } else {
                to_object(Vec3::new(placed_world.x, placed_world.y + rise, placed_world.z))
            };
            // Stepping down: while the release slides the foot off the tread it lifted from, the
            // sole never stands below that tread (its plane, extended along its slope), so it
            // does not sink into the tread it is leaving. The tread has an edge: a foot more than
            // `LIFT_FLOOR_REACH` from where it lifted is past it and stands on the ground the
            // host measured, the lower tread (ADR-8801). The floor raises the placed foot
            // directly, not the smoothed ground, so leaving it is not held back by the slew
            // limit. A foot over higher ground (stepping up) is above the floor already.
            if let Some((at, at_normal)) = state.floor
                && horizontal_distance(placed_world, at) <= LIFT_FLOOR_REACH
            {
                let floor = at.y + slope_rise(at, at_normal, placed_world.x, placed_world.z, t.max_tilt);
                let raise = floor - (state.offset + rise);
                if raise > 0.0 && raise.is_finite() {
                    let placed = to_world(full);
                    full = to_object(Vec3::new(placed.x, placed.y + raise, placed.z));
                }
            }
            // Heel and toe: the stair rule's move along the foot (eased like the ground, at
            // once for a planted foot on a new sample), the toe bend, and a planted foot never
            // leaves its heel, ball or toe under the ground.
            let mut bend = 0.0;
            if let (Some(sole), Some((_, shift))) = (&frame, sole_goal) {
                state.shift = if planted > 0.5 && moved > 1.0e-4 {
                    shift
                } else {
                    state.shift + ((shift - state.shift) * ease(dt, t.offset_tau)).clamp(-cap, cap)
                };
                full += sole.forward() * state.shift;
                bend = sole.heel_lift() * planted;
                if planted > 0.5 {
                    let floor = sole.support(state.tilt, full, sole.tip(bend));
                    if floor.is_finite() && full.y < floor {
                        full.y = floor;
                    }
                }
            } else {
                state.shift = 0.0;
            }

            let target = tip.lerp(full, w);
            // Slide report: how far the world target moved since this stance began.
            let target_world = to_world(target);
            if down {
                let from = *state.slide_from.get_or_insert(target_world);
                state.slide = horizontal_distance(target_world, from);
            } else {
                state.slide_from = None;
                state.slide = 0.0;
            }
            if planted > 0.5 {
                let hip = to_object(leg.hip);
                if hip.is_finite() {
                    let length = finite_or(leg.length, 0.0).abs();
                    let reach = pelvis_for_reach(hip, length, t.max_extension, target);
                    need = need.min(reach);
                    if frame.is_some() {
                        sole_need = Some(sole_need.map_or(reach, |n| n.min(reach)));
                    }
                }
            }
            let tilt = Quat::IDENTITY.lerp(state.tilt, w);
            let (toe, heel_gap, toe_gap) = match &frame {
                Some(sole) => {
                    let bend = bend * w;
                    let (heel_gap, toe_gap) = if planted > 0.5 {
                        sole.gaps(target, tilt, sole.tip(bend))
                    } else {
                        (0.0, 0.0)
                    };
                    (
                        sole.toe_rotation(tilt, bend),
                        finite_or(heel_gap, 0.0),
                        finite_or(toe_gap, 0.0),
                    )
                }
                None => (Quat::IDENTITY, 0.0, 0.0),
            };
            out.legs.push(FootPlantLegOut {
                target,
                tilt,
                planted,
                lock: state.lock.weight(),
                toe,
                heel_gap,
                toe_gap,
            });
        }

        // Pelvis: drop only as far as the farthest-down planted foot needs; rise back eased.
        let need = need.max(-t.max_drop);
        if need < self.pelvis {
            // Sink at once (at most `max_slew` per second) when a leg needs the reach: an eased
            // drop lags a pinned foot that falls behind the hips, and the leg would come up short.
            self.pelvis = (self.pelvis - t.max_slew * dt).max(need);
            // A planted heel-and-toe leg gets its reach this frame: a pelvis a frame late leaves
            // the leg short, and the solve pulls the heel back into the riser it was placed past.
            if let Some(sole) = sole_need {
                self.pelvis = self.pelvis.min(sole.max(-t.max_drop));
            }
        } else {
            self.pelvis += (need - self.pelvis) * ease(dt, t.pelvis_tau);
        }
        out.pelvis_offset = self.pelvis * w;
        out.weight = w;
    }
}

/// What a [`FootPlanter`] did this frame, as a reflected component an agent can read and assert
/// (`scene.get_entity`, `play.expect`). Hosts that plan feet by hand insert it on the rig entity
/// each frame from [`FootPlanter::report`]. A rig with [`FootPlacement`](super::component::FootPlacement)
/// receives it from the engine. The planter itself is plain data and never touches the ECS.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct FootPlantReport {
    /// Effect weight `[0, 1]` (0 in the air).
    pub weight: f32,
    /// Pelvis offset, object space, meters (zero or negative).
    pub pelvis_offset: f32,
    /// Per leg: 1 planted, 0 swinging (0 while the effect is faded out, weight 0).
    pub planted: Vec<f32>,
    /// Per leg: weight of the world-space pin `[0, 1]`.
    pub lock: Vec<f32>,
    /// Per leg: foot tilt toward the ground normal, degrees.
    pub tilt_deg: Vec<f32>,
    /// Per leg: horizontal distance the leg's world target moved since its stance began, meters.
    pub slide: Vec<f32>,
    /// The largest `slide` of any leg this frame, meters. With the lock on it stays under 0.01.
    pub max_slide: f32,
    /// Per leg: height of the planted heel over the surface under it, meters (negative: below
    /// the ground; a heel hanging past a step edge the sole rests on is measured to that
    /// edge's tread). 0 while the foot swings or without heel and toe samples.
    pub heel_gap: Vec<f32>,
    /// Per leg: height of the planted toe tip over the ground under it, meters, as `heel_gap`.
    pub toe_gap: Vec<f32>,
}

/// A world direction in object space; a non-finite direction reads as +Y.
fn to_object_dir(inverse: Quat, direction: Vec3) -> Vec3 {
    let d = inverse * direction;
    if d.is_finite() { d } else { Vec3::Y }
}

fn horizontal_distance(a: Vec3, b: Vec3) -> f32 {
    let (dx, dz) = (a.x - b.x, a.z - b.z);
    (dx * dx + dz * dz).sqrt()
}

/// How much higher the ground plane through `sample` is at `(x, z)`. The plane is never steeper
/// than `max_tilt` (the most the planter tilts a foot: a steeper normal is a wall or a bad
/// sample, and its plane would throw the sole by decimetres over a few centimetres). Zero for a
/// flat, degenerate, downward or non-finite normal (as [`align_to_normal`] tilts nothing for
/// those), and zero at the sample itself.
fn slope_rise(sample: Vec3, normal: Vec3, x: f32, z: f32, max_tilt: f32) -> f32 {
    if !sample.is_finite() || !normal.is_finite() || !x.is_finite() || !z.is_finite() {
        return 0.0;
    }
    let n = normal.normalize_or_zero();
    if !n.is_finite() || n.y <= 1.0e-4 {
        return 0.0;
    }
    let horizontal = (n.x * n.x + n.z * n.z).sqrt();
    if horizontal < 1.0e-12 {
        return 0.0;
    }
    let (sin, cos) = det_sin_cos_f64(f64::from(finite_or(max_tilt, 0.0).abs()));
    let limit = if cos > 1.0e-9 {
        (sin / cos) as f32
    } else {
        f32::INFINITY
    };
    let gradient = (horizontal / n.y).min(limit);
    // Downhill direction is +n (horizontal part); the plane rises against it.
    let along = (n.x * (x - sample.x) + n.z * (z - sample.z)) / horizontal;
    let rise = -along * gradient;
    if rise.is_finite() { rise } else { 0.0 }
}

/// Bends each leg's toe joint by [`FootPlantLegOut::toe`], after [`apply_foot_plant`] posed the
/// legs. `toes[i]` is leg `i`'s toe joint (`None`, or a missing entry, skips the leg). A
/// faded-out plan or an identity bend changes nothing.
pub fn apply_toe_bend(skeleton: &Skeleton, pose: &mut Pose, plan: &FootPlant, toes: &[Option<u32>]) {
    if plan.is_identity() {
        return;
    }
    // Forward kinematics along each toe's chain only (the toe and its ancestors).
    let mut globals = Vec::new();
    for (out, toe) in plan.legs.iter().zip(toes) {
        let Some(toe) = *toe else {
            continue;
        };
        let toe = toe as usize;
        if toe >= pose.len() || tilt_angle(out.toe) < 1.0e-6 {
            continue;
        }
        refresh_global(skeleton, pose, toe, &mut globals);
        let Some(joint) = globals.get(toe) else {
            continue;
        };
        let (_, rotation, _) = joint.to_scale_rotation_translation();
        let parent_rotation = skeleton
            .parents
            .get(toe)
            .copied()
            .flatten()
            .and_then(|p| globals.get(p as usize))
            .map_or(Quat::IDENTITY, |m| m.to_scale_rotation_translation().1);
        // A joint matrix with a zero scale axis has no rotation (NaN): leave the toe as it was.
        write_finite(
            &mut pose.rotations[toe],
            (parent_rotation.inverse() * out.toe * rotation).normalize(),
        );
    }
}

/// Writes `rotation` into `slot` only when it is finite. A joint (or parent) matrix with a zero
/// scale axis decomposes to a NaN rotation; the two-bone solve already refuses such a matrix
/// (`rotation_of_matrix` in `ik.rs`), and the tilt and toe writes leave the joint as the solve
/// left it instead of putting NaN in the pose.
fn write_finite(slot: &mut Quat, rotation: Quat) {
    if rotation.is_finite() {
        *slot = rotation;
    }
}

/// Poses `skeleton` with a plan made with heel and toe samples: as [`apply_foot_plant`], but each
/// foot keeps the clip's object-space orientation turned by its tilt (the two-bone solve would
/// otherwise turn the foot with the shin, and the heel and toe would leave the ground the plan
/// placed them on), and each toe joint in `toes` (parallel to `legs`) is bent by
/// [`FootPlantLegOut::toe`]. A faded-out plan changes nothing.
pub fn apply_foot_plant_with_toes(
    skeleton: &Skeleton,
    pose: &mut Pose,
    plan: &FootPlant,
    legs: &[FootIkLeg],
    toes: &[Option<u32>],
    pole: Option<Vec3>,
) {
    apply_foot_plant_with_toes_into(
        skeleton,
        pose,
        plan,
        legs,
        toes,
        pole,
        &mut ApplyBuffers::default(),
    );
}

/// Joint-matrix buffers for `apply_foot_plant_with_toes_into` and [`apply_foot_plant_into`],
/// kept across frames.
#[derive(Clone, Debug, Default)]
pub(crate) struct ApplyBuffers {
    /// The incoming pose's object matrices (each leg tip's chain).
    clip: Vec<Mat4>,
    /// Reused by the solves.
    pub(crate) scratch: Vec<Mat4>,
}

/// [`apply_foot_plant_with_toes`] with caller-owned joint-matrix buffers, so a settled frame does
/// not allocate.
pub(crate) fn apply_foot_plant_with_toes_into(
    skeleton: &Skeleton,
    pose: &mut Pose,
    plan: &FootPlant,
    legs: &[FootIkLeg],
    toes: &[Option<u32>],
    pole: Option<Vec3>,
    buffers: &mut ApplyBuffers,
) {
    let ApplyBuffers { clip, scratch } = buffers;
    if plan.is_identity() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_animation", "apply_foot_plant").entered();
    // The clip's foot orientations, before the IK turns the shins: forward kinematics along each
    // tip's chain (bit-identical to a full rebuild, without the joints no leg reads).
    for leg in legs.iter().take(plan.legs.len()) {
        refresh_global(skeleton, pose, leg.chain.tip as usize, clip);
    }
    if plan.pelvis_offset != 0.0
        && let Some(root) = skeleton.parents.iter().position(|p| p.is_none())
        && root < pose.len()
    {
        pose.translations[root].y += plan.pelvis_offset;
    }
    let rotation_of = |globals: &[Mat4], joint: usize| {
        globals
            .get(joint)
            .map_or(Quat::IDENTITY, |m| m.to_scale_rotation_translation().1)
    };
    let parent = |joint: usize| skeleton.parents.get(joint).copied().flatten().map(|p| p as usize);
    for (i, (leg, out)) in legs.iter().zip(&plan.legs).enumerate() {
        solve_two_bone_with_globals(skeleton, pose, &leg.chain, out.target, pole, 1.0, scratch);
        let tip = leg.chain.tip as usize;
        if tip >= pose.len() || tip >= clip.len() {
            continue;
        }
        refresh_global(skeleton, pose, tip, scratch);
        let parent_rotation = parent(tip).map_or(Quat::IDENTITY, |p| rotation_of(scratch, p));
        let foot = (out.tilt * rotation_of(clip, tip)).normalize();
        write_finite(
            &mut pose.rotations[tip],
            (parent_rotation.inverse() * foot).normalize(),
        );
        let Some(toe) = toes.get(i).copied().flatten().map(|t| t as usize) else {
            continue;
        };
        if toe >= pose.len() || tilt_angle(out.toe) < 1.0e-6 {
            continue;
        }
        refresh_global(skeleton, pose, toe, scratch);
        let toe_parent = parent(toe).map_or(Quat::IDENTITY, |p| rotation_of(scratch, p));
        let bent = (out.toe * rotation_of(scratch, toe)).normalize();
        write_finite(
            &mut pose.rotations[toe],
            (toe_parent.inverse() * bent).normalize(),
        );
    }
}

/// Graph output (ADR-10130): the graph's `TwoBoneIK` nodes already moved the legs to the plan's
/// targets, so only the foot tilt and the toe bend are applied here, on the evaluated pose. Each
/// foot joint (`legs[i].chain.tip`) turns by [`FootPlantLegOut::tilt`] in object space (as
/// [`apply_foot_plant`] does after its own solve) and each toe joint in `toes` by
/// [`FootPlantLegOut::toe`] after that. Joint positions do not move; a faded-out plan, or flat
/// feet with no toe bend, change nothing.
pub(crate) fn apply_foot_turns_into(
    skeleton: &Skeleton,
    pose: &mut Pose,
    plan: &FootPlant,
    legs: &[FootIkLeg],
    toes: &[Option<u32>],
    globals: &mut Vec<Mat4>,
) {
    if plan.is_identity() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_animation", "apply_foot_turns").entered();
    let rotation_of = |globals: &[Mat4], joint: usize| {
        globals
            .get(joint)
            .map_or(Quat::IDENTITY, |m| m.to_scale_rotation_translation().1)
    };
    let parent = |joint: usize| skeleton.parents.get(joint).copied().flatten().map(|p| p as usize);
    for (i, (leg, out)) in legs.iter().zip(&plan.legs).enumerate() {
        let tip = leg.chain.tip as usize;
        if tip < pose.len() && tilt_angle(out.tilt) >= 1.0e-6 {
            refresh_global(skeleton, pose, tip, globals);
            let parent_rotation = parent(tip).map_or(Quat::IDENTITY, |p| rotation_of(globals, p));
            let foot = (out.tilt * rotation_of(globals, tip)).normalize();
            write_finite(
                &mut pose.rotations[tip],
                (parent_rotation.inverse() * foot).normalize(),
            );
        }
        let Some(toe) = toes.get(i).copied().flatten().map(|t| t as usize) else {
            continue;
        };
        if toe >= pose.len() || tilt_angle(out.toe) < 1.0e-6 {
            continue;
        }
        refresh_global(skeleton, pose, toe, globals);
        let toe_parent = parent(toe).map_or(Quat::IDENTITY, |p| rotation_of(globals, p));
        let bent = (out.toe * rotation_of(globals, toe)).normalize();
        write_finite(
            &mut pose.rotations[toe],
            (toe_parent.inverse() * bent).normalize(),
        );
    }
}

/// Whether [`apply_foot_turns_into`] would change a pose: an active plan with a tilted foot or a
/// bent toe.
pub(crate) fn plan_turns_feet(plan: &FootPlant) -> bool {
    !plan.is_identity()
        && plan
            .legs
            .iter()
            .any(|leg| tilt_angle(leg.tilt) >= 1.0e-6 || tilt_angle(leg.toe) >= 1.0e-6)
}

/// Poses `skeleton` with `plan`: the root joint moves by [`FootPlant::pelvis_offset`] (object space,
/// +Y up), each leg is solved toward its target with
/// [`solve_two_bone`](crate::ik::solve_two_bone), and each foot joint is turned by its tilt in
/// object space. `pole` (object space) steers the knees; `None` keeps the current bend. Legs and
/// plan entries pair in order. A faded-out plan changes nothing.
///
/// Allocates one joint-matrix buffer per call; [`apply_foot_plant_into`] reuses a caller-kept one.
pub fn apply_foot_plant(
    skeleton: &Skeleton,
    pose: &mut Pose,
    plan: &FootPlant,
    legs: &[FootIkLeg],
    pole: Option<Vec3>,
) {
    apply_foot_plant_into(skeleton, pose, plan, legs, pole, &mut Vec::new());
}

/// [`apply_foot_plant`] with a caller-kept joint-matrix buffer. Forward kinematics is refreshed
/// along each leg's chain (the tip and its ancestors), never rebuilt for the whole skeleton per
/// leg, and `globals` is grown once to the skeleton and then reused: a settled frame allocates
/// nothing. The pose it writes is bit-identical to the per-leg rebuild. `globals` holds no state
/// between calls; its contents on return are scratch.
pub fn apply_foot_plant_into(
    skeleton: &Skeleton,
    pose: &mut Pose,
    plan: &FootPlant,
    legs: &[FootIkLeg],
    pole: Option<Vec3>,
    globals: &mut Vec<Mat4>,
) {
    if plan.is_identity() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_animation", "apply_foot_plant").entered();
    if plan.pelvis_offset != 0.0
        && let Some(root) = skeleton.parents.iter().position(|p| p.is_none())
        && root < pose.len()
    {
        pose.translations[root].y += plan.pelvis_offset;
    }
    for (leg, out) in legs.iter().zip(&plan.legs) {
        solve_two_bone_with_globals(skeleton, pose, &leg.chain, out.target, pole, 1.0, globals);
        let tip = leg.chain.tip as usize;
        if tip >= pose.len() || tilt_angle(out.tilt) < 1.0e-6 {
            continue;
        }
        refresh_global(skeleton, pose, tip, globals);
        let Some(foot) = globals.get(tip) else {
            continue;
        };
        let (_, foot_rotation, _) = foot.to_scale_rotation_translation();
        let parent_rotation = skeleton
            .parents
            .get(tip)
            .copied()
            .flatten()
            .and_then(|p| globals.get(p as usize))
            .map_or(Quat::IDENTITY, |m| m.to_scale_rotation_translation().1);
        write_finite(
            &mut pose.rotations[tip],
            (parent_rotation.inverse() * out.tilt * foot_rotation).normalize(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ik::IkChain;
    use crate::pose::translated;

    const ANKLE: f32 = 0.128;
    const HIP_Y: f32 = 0.919;
    const LEN: f32 = 0.80; // a straight leg: hip to ankle is 0.796
    const DT: f32 = 1.0 / 60.0;

    fn settings() -> FootPlantSettings {
        FootPlantSettings::default()
    }

    /// A leg as the source's tests set it up: the rig at the origin, unrotated, so world = object.
    fn leg(side: f32, ground_y: f32, normal: Vec3) -> FootPlantLeg {
        let foot = Vec3::new(0.197 * side, ANKLE, 0.0);
        FootPlantLeg {
            hip: Vec3::new(0.109 * side, HIP_Y, 0.03),
            foot,
            length: LEN,
            ankle_height: ANKLE,
            ground: FootGround::Contact(Vec3::new(foot.x, ground_y, foot.z)),
            normal,
            contact: None,
        }
    }

    fn input(legs: &[FootPlantLeg], active: bool) -> FootPlantInput<'_> {
        FootPlantInput {
            rig_translation: Vec3::ZERO,
            rig_rotation: Quat::IDENTITY,
            legs,
            active,
            dt: DT,
        }
    }

    fn settle(planter: &mut FootPlanter, legs: &[FootPlantLeg], active: bool, frames: usize) -> FootPlant {
        let mut out = planter.plan(&input(legs, active));
        for _ in 0..frames {
            out = planter.plan(&input(legs, active));
        }
        out
    }

    fn slope_normal(deg: f32) -> Vec3 {
        let a = deg.to_radians();
        Vec3::new(-a.sin(), a.cos(), 0.0)
    }

    #[test]
    fn straight_leg_on_flat_ground_is_the_identity() {
        let mut planter = FootPlanter::new(settings());
        let legs = [leg(1.0, 0.0, Vec3::Y), leg(-1.0, 0.0, Vec3::Y)];
        let out = settle(&mut planter, &legs, true, 120);
        assert!((out.weight - 1.0).abs() < 1.0e-6);
        for (i, (got, leg)) in out.legs.iter().zip(&legs).enumerate() {
            assert!(
                (got.target - leg.foot).length() < 1.0e-5,
                "leg {i}: {:?}",
                got.target
            );
            assert!(tilt_angle(got.tilt) < 1.0e-4);
        }
        assert!(out.pelvis_offset.abs() < 0.02, "pelvis {}", out.pelvis_offset);
    }

    #[test]
    fn a_sunk_clip_foot_is_lifted_onto_the_ground() {
        let mut planter = FootPlanter::new(settings());
        let mut l = leg(1.0, 0.0, Vec3::Y);
        l.foot.y = 0.09; // sole 3.8 cm under the floor
        let out = settle(&mut planter, &[l, leg(-1.0, 0.0, Vec3::Y)], true, 90);
        assert!((out.legs[0].target.y - ANKLE).abs() < 1.0e-4);
    }

    #[test]
    fn airborne_blend_is_zero_and_the_clip_pose_is_untouched() {
        let mut planter = FootPlanter::new(settings());
        let legs = [
            leg(1.0, 0.3, slope_normal(20.0)),
            leg(-1.0, -0.3, slope_normal(20.0)),
        ];
        let on = settle(&mut planter, &legs, true, 60);
        assert!(on.weight > 0.99);
        let off = settle(&mut planter, &legs, false, 60);
        assert_eq!(off.weight, 0.0);
        assert!(off.is_identity());
        assert_eq!(off.pelvis_offset, 0.0);
        for (got, leg) in off.legs.iter().zip(&legs) {
            assert_eq!(got.target, leg.foot);
            assert_eq!(got.tilt, Quat::IDENTITY);
            assert_eq!(got.lock, 0.0);
        }
        assert!(planter.pin(0).is_none());
    }

    #[test]
    fn the_blend_fades_over_about_a_fifth_of_a_second() {
        let mut planter = FootPlanter::new(settings());
        let legs = [leg(1.0, 0.2, Vec3::Y), leg(-1.0, 0.0, Vec3::Y)];
        let mut last = 0.0;
        for _ in 0..6 {
            last = planter.plan(&input(&legs, true)).weight;
        }
        assert!(last > 0.3 && last < 0.7, "weight after 0.1 s: {last}");
        let faded = settle(&mut planter, &legs, false, 3);
        assert!(faded.weight < last);
    }

    #[test]
    fn slopes_tilt_the_foot_to_the_ground_within_the_limit() {
        for (deg, want) in [(0.0_f32, 0.0_f32), (15.0, 15.0), (30.0, 25.0), (45.0, 25.0)] {
            let mut planter = FootPlanter::new(settings());
            let n = slope_normal(deg);
            let legs = [leg(1.0, 0.0, n), leg(-1.0, 0.0, n)];
            let out = settle(&mut planter, &legs, true, 240);
            let got = tilt_angle(out.legs[0].tilt).to_degrees();
            assert!((got - want).abs() < 0.8, "slope {deg}: tilt {got} want {want}");
            // The tilted foot takes the sole plane toward the normal: +Y maps close to n.
            let up = out.legs[0].tilt * Vec3::Y;
            let ideal = align_to_normal(n, 0.436) * Vec3::Y;
            assert!((up - ideal).length() < 0.02);
        }
    }

    #[test]
    fn a_slope_lowers_the_pelvis_toward_the_downhill_foot_within_reach() {
        let s = settings();
        for deg in [15.0_f32, 30.0, 45.0] {
            let rise = (0.24 * deg.to_radians().tan().min(1.2)).min(0.5);
            let mut planter = FootPlanter::new(s);
            let n = slope_normal(deg);
            let legs = [leg(1.0, rise * 0.5, n), leg(-1.0, -rise * 0.5, n)];
            let out = settle(&mut planter, &legs, true, 300);
            assert!(
                out.pelvis_offset <= 1.0e-6,
                "slope {deg}: pelvis {}",
                out.pelvis_offset
            );
            assert!(out.pelvis_offset >= -s.max_pelvis_drop - 1.0e-6);
            for (i, (got, leg)) in out.legs.iter().zip(&legs).enumerate() {
                let hip = leg.hip + Vec3::Y * out.pelvis_offset;
                let dist = (got.target - hip).length();
                assert!(
                    dist <= LEN * s.max_extension + 1.0e-3,
                    "slope {deg} leg {i} overextended: {dist} > {}",
                    LEN * s.max_extension
                );
            }
            // The downhill foot ends on the ground it was sent to (within the tilt lift).
            assert!(out.legs[1].target.y >= -rise * 0.5 + ANKLE - 1.0e-3);
        }
    }

    #[test]
    fn stepping_down_drops_the_pelvis_and_stepping_up_keeps_it_level() {
        let s = settings();
        let mut down = FootPlanter::new(s);
        let legs_down = [leg(1.0, -0.3, Vec3::Y), leg(-1.0, -0.3, Vec3::Y)];
        let out = settle(&mut down, &legs_down, true, 300);
        assert!(out.pelvis_offset < -0.2, "step down pelvis {}", out.pelvis_offset);
        assert!(out.pelvis_offset >= -s.max_pelvis_drop);
        assert!((out.legs[0].target.y - (-0.3 + ANKLE)).abs() < 0.01);

        let mut up = FootPlanter::new(s);
        let legs_up = [leg(1.0, 0.3, Vec3::Y), leg(-1.0, 0.3, Vec3::Y)];
        let out = settle(&mut up, &legs_up, true, 300);
        assert!(
            out.pelvis_offset.abs() < 1.0e-3,
            "step up pelvis {}",
            out.pelvis_offset
        );
        assert!((out.legs[0].target.y - (0.3 + ANKLE)).abs() < 0.01);
    }

    #[test]
    fn the_target_does_not_pop_when_the_ground_jumps() {
        let s = settings();
        let mut planter = FootPlanter::new(s);
        let flat = [leg(1.0, 0.0, Vec3::Y), leg(-1.0, 0.0, Vec3::Y)];
        settle(&mut planter, &flat, true, 60);
        let stepped = [leg(1.0, 0.4, Vec3::Y), leg(-1.0, 0.4, Vec3::Y)];
        let mut prev = ANKLE;
        let mut worst: f32 = 0.0;
        for _ in 0..40 {
            let out = planter.plan(&input(&stepped, true));
            worst = worst.max((out.legs[0].target.y - prev).abs());
            prev = out.legs[0].target.y;
        }
        assert!(worst <= s.max_slew / 60.0 + 1.0e-4, "pop {worst}");
        assert!((prev - (0.4 + ANKLE)).abs() < 0.01);
    }

    #[test]
    fn jitter_inside_the_deadband_does_not_move_the_target() {
        let s = settings();
        let mut planter = FootPlanter::new(s);
        let mut l = [leg(1.0, 0.10, Vec3::Y), leg(-1.0, 0.10, Vec3::Y)];
        let base = settle(&mut planter, &l, true, 120).legs[0].target.y;
        l[0].ground = FootGround::Contact(Vec3::new(l[0].foot.x, 0.10 + s.deadband * 0.8, 0.0));
        let out = settle(&mut planter, &l, true, 30);
        assert!((out.legs[0].target.y - base).abs() < 1.0e-5);
    }

    #[test]
    fn a_swinging_foot_keeps_its_lift_over_the_ground() {
        let mut planter = FootPlanter::new(settings());
        let mut l = [leg(1.0, 0.2, Vec3::Y), leg(-1.0, 0.2, Vec3::Y)];
        l[0].foot.y = ANKLE + 0.2; // mid-stride
        let out = settle(&mut planter, &l, true, 120);
        assert!(out.legs[0].planted < 0.01);
        assert_eq!(out.legs[0].lock, 0.0, "a swinging foot is never pinned");
        assert!(
            (out.legs[0].target.y - (0.2 + ANKLE + 0.2)).abs() < 0.01,
            "{}",
            out.legs[0].target.y
        );
        assert!(out.legs[1].planted > 0.99);
        assert_eq!(out.legs[1].lock, 1.0);
    }

    #[test]
    fn the_offset_is_clamped_at_a_cliff_edge() {
        let s = settings();
        let mut planter = FootPlanter::new(s);
        let l = [leg(1.0, 5.0, Vec3::Y), leg(-1.0, -5.0, Vec3::Y)];
        let out = settle(&mut planter, &l, true, 400);
        assert!(out.legs[0].target.y <= s.max_ground_offset + ANKLE + 0.05);
        assert!(out.legs[1].target.y >= -s.max_ground_offset + ANKLE - 0.05);
        assert!(out.pelvis_offset >= -s.max_pelvis_drop - 1.0e-6);
    }

    #[test]
    fn pelvis_for_reach_is_zero_when_there_is_slack_and_negative_when_the_ground_falls() {
        let hip = Vec3::new(0.1, HIP_Y, 0.0);
        assert_eq!(pelvis_for_reach(hip, LEN, 0.995, Vec3::new(0.1, 0.4, 0.0)), 0.0);
        let p = pelvis_for_reach(hip, LEN, 0.995, Vec3::new(0.1, ANKLE - 0.2, 0.0));
        assert!(p < -0.1 && p > -0.25, "{p}");
        // Out of reach sideways: no drop helps.
        assert_eq!(pelvis_for_reach(hip, LEN, 0.995, Vec3::new(2.0, 0.0, 0.0)), 0.0);
        assert_eq!(pelvis_for_reach(hip, f32::NAN, 0.995, Vec3::ZERO), 0.0);
    }

    #[test]
    fn landing_primes_the_offset_from_the_ground_under_the_feet() {
        let mut planter = FootPlanter::new(settings());
        let l = [leg(1.0, 0.3, Vec3::Y), leg(-1.0, 0.3, Vec3::Y)];
        settle(&mut planter, &l, false, 30);
        // First active frame: the weight is small, but the offset is already at the ground.
        let out = planter.plan(&input(&l, true));
        let full = 0.3 + ANKLE;
        let blended = ANKLE + (full - ANKLE) * out.weight;
        assert!((out.legs[0].target.y - blended).abs() < 0.01);
    }

    // ---- Regia additions: plant lock, rig transform, contacts, robustness, determinism. ----

    /// A planted foot whose clip slides (root-motion mismatch) stays where it touched down.
    #[test]
    fn the_lock_holds_a_sliding_planted_foot() {
        let mut planter = FootPlanter::new(settings());
        let mut legs = [leg(1.0, 0.0, Vec3::Y), leg(-1.0, 0.0, Vec3::Y)];
        settle(&mut planter, &legs, true, 60);
        let start = planter.plan(&input(&legs, true)).legs[0].target;
        for _ in 0..30 {
            legs[0].foot.z += 0.004; // 24 cm/s of skate
            legs[0].ground = FootGround::Contact(Vec3::new(legs[0].foot.x, 0.0, legs[0].foot.z));
            let out = planter.plan(&input(&legs, true));
            assert!(
                (out.legs[0].target - start).length() < 1.0e-4,
                "{:?} vs {start:?}",
                out.legs[0].target
            );
            assert_eq!(out.legs[0].lock, 1.0);
        }
        assert!(planter.slide(0) < 1.0e-4);
        assert!(planter.pin(0).is_some());
        let last = planter.plan(&input(&legs, true));
        let report = planter.report(&last);
        assert!(report.max_slide < 0.01, "{report:?}");
        assert_eq!(report.lock, vec![1.0, 1.0]);
    }

    /// When the clip lifts the foot, the pin hands it back over the release time, never in one jump.
    #[test]
    fn a_lifted_foot_is_released_smoothly() {
        let s = settings();
        let mut planter = FootPlanter::new(s);
        let mut legs = [leg(1.0, 0.0, Vec3::Y), leg(-1.0, 0.0, Vec3::Y)];
        settle(&mut planter, &legs, true, 60);
        for _ in 0..20 {
            legs[0].foot.z += 0.005; // the clip foot drifts 10 cm behind the pin
            planter.plan(&input(&legs, true));
        }
        legs[0].foot.y = ANKLE + 0.1; // lift off
        let mut prev = planter.plan(&input(&legs, true));
        let mut frames = 1;
        while prev.legs[0].lock > 0.0 {
            let out = planter.plan(&input(&legs, true));
            let delta = out.legs[0].target - prev.legs[0].target;
            let step = Vec3::new(delta.x, 0.0, delta.z).length();
            assert!(
                step < 0.1 * DT / s.lock_release_time + 1.0e-4,
                "release jump {step}"
            );
            prev = out;
            frames += 1;
            assert!(frames < 60, "the pin never released");
        }
        assert!(
            (frames as f32 * DT - s.lock_release_time).abs() < 2.0 * DT,
            "{frames} frames"
        );
        assert!(
            (prev.legs[0].target.z - legs[0].foot.z).abs() < 1.0e-5,
            "back on the clip"
        );
        assert!(planter.pin(0).is_none());
    }

    /// A pin left farther behind than `lock_max_drift` (a teleport) is set again at the foot.
    #[test]
    fn a_pin_that_drifts_too_far_is_set_again() {
        let s = settings();
        let mut planter = FootPlanter::new(s);
        let legs = [leg(1.0, 0.0, Vec3::Y), leg(-1.0, 0.0, Vec3::Y)];
        settle(&mut planter, &legs, true, 60);
        let mut moved = FootPlantInput {
            rig_translation: Vec3::new(0.0, 0.0, -2.0),
            ..input(&legs, true)
        };
        let world_legs = legs.map(|mut l| {
            l.hip.z -= 2.0;
            l.foot.z -= 2.0;
            l.ground = FootGround::Contact(Vec3::new(l.foot.x, 0.0, l.foot.z));
            l
        });
        moved.legs = &world_legs;
        let out = planter.plan(&moved);
        for (i, l) in out.legs.iter().enumerate() {
            assert!(
                (l.target - legs[i].foot).length() < 1.0e-4,
                "leg {i} stretched: {:?}",
                l.target
            );
        }
        assert_eq!(planter.pin(0).map(|p| p.z), Some(-2.0));
    }

    /// The same body, translated and turned, plans the same object-space result.
    #[test]
    fn the_rig_transform_does_not_change_the_object_plan() {
        let n = slope_normal(12.0);
        let local = [leg(1.0, 0.1, n), leg(-1.0, -0.05, n)];
        let mut a = FootPlanter::new(settings());
        let flat = settle(&mut a, &local, true, 90);
        let rotation = Quat::from_rotation_y(2.3);
        let origin = Vec3::new(31.0, -4.0, 12.5);
        let world = local.map(|mut l| {
            l.hip = rotation * l.hip + origin;
            l.foot = rotation * l.foot + origin;
            if let FootGround::Contact(p) = l.ground {
                l.ground = FootGround::Contact(rotation * p + origin);
            }
            l.normal = rotation * l.normal;
            l
        });
        let mut b = FootPlanter::new(settings());
        let mut moved = flat.clone();
        for _ in 0..91 {
            moved = b.plan(&FootPlantInput {
                rig_translation: origin,
                rig_rotation: rotation,
                legs: &world,
                active: true,
                dt: DT,
            });
        }
        assert!((flat.pelvis_offset - moved.pelvis_offset).abs() < 1.0e-4);
        for (x, y) in flat.legs.iter().zip(&moved.legs) {
            assert!(
                (x.target - y.target).length() < 1.0e-3,
                "{:?} vs {:?}",
                x.target,
                y.target
            );
            assert!((tilt_angle(x.tilt) - tilt_angle(y.tilt)).abs() < 1.0e-3);
        }
    }

    /// A host contact label wins over the clip's lift.
    #[test]
    fn a_contact_label_overrides_the_lift() {
        let mut planter = FootPlanter::new(settings());
        let mut legs = [leg(1.0, 0.0, Vec3::Y), leg(-1.0, 0.0, Vec3::Y)];
        legs[0].foot.y = ANKLE + 0.2;
        legs[0].contact = Some(1.0);
        legs[1].contact = Some(0.0);
        let out = settle(&mut planter, &legs, true, 60);
        assert_eq!(out.legs[0].planted, 1.0);
        assert_eq!(out.legs[0].lock, 1.0);
        assert_eq!(out.legs[1].planted, 0.0);
        assert_eq!(out.legs[1].lock, 0.0);
    }

    /// With the lock off nothing is pinned; with zero legs nothing panics.
    #[test]
    fn the_lock_can_be_turned_off_and_no_legs_is_fine() {
        let mut planter = FootPlanter::new(FootPlantSettings {
            lock: false,
            ..settings()
        });
        let legs = [leg(1.0, 0.0, Vec3::Y), leg(-1.0, 0.0, Vec3::Y)];
        let out = settle(&mut planter, &legs, true, 10);
        assert!(out.legs.iter().all(|l| l.lock == 0.0));
        assert!(planter.pin(0).is_none());
        let none = planter.plan(&input(&[], true));
        assert!(none.legs.is_empty());
    }

    /// NaN or infinite inputs and settings never panic and never leak NaN into the plan.
    #[test]
    fn non_finite_inputs_and_settings_stay_finite() {
        let bad = FootPlantSettings {
            max_ground_offset: f32::NAN,
            max_pelvis_drop: -1.0,
            max_tilt: f32::INFINITY,
            plant_lift: f32::NAN,
            swing_lift: -0.1,
            max_extension: f32::NAN,
            blend_in_per_s: f32::NAN,
            blend_out_per_s: -3.0,
            offset_tau: 0.0,
            pelvis_tau: f32::NAN,
            tilt_tau: -1.0,
            deadband: f32::NAN,
            max_slew: f32::INFINITY,
            lock: true,
            lock_max_drift: f32::NAN,
            lock_release_time: f32::NAN,
            lock_max_speed: f32::NAN,
            step_edge: f32::NAN,
            min_support: f32::INFINITY,
            max_edge_shift: -1.0,
        };
        for settings in [settings(), bad] {
            let mut planter = FootPlanter::new(settings);
            let mut legs = [
                leg(1.0, 0.0, Vec3::new(f32::NAN, 1.0, 0.0)),
                leg(-1.0, 0.0, Vec3::ZERO),
            ];
            legs[0].ground = FootGround::Contact(Vec3::NAN);
            legs[1].foot = Vec3::new(f32::INFINITY, 0.0, 0.0);
            legs[1].length = f32::NAN;
            legs[1].ankle_height = f32::NAN;
            for dt in [DT, f32::NAN, -1.0, f32::INFINITY, DT] {
                let out = planter.plan(&FootPlantInput {
                    rig_translation: Vec3::NAN,
                    rig_rotation: Quat::from_xyzw(0.0, 0.0, 0.0, 0.0),
                    legs: &legs,
                    active: true,
                    dt,
                });
                assert!(out.pelvis_offset.is_finite() && out.weight.is_finite());
                for l in &out.legs {
                    assert!(l.target.is_finite() && l.tilt.is_finite(), "{l:?}");
                }
            }
        }
    }

    /// Same inputs, same bits: the planter uses only IEEE arithmetic and the crate's deterministic
    /// transcendentals, so a replay matches exactly.
    #[test]
    fn two_runs_agree_bit_for_bit() {
        let run = || {
            let mut planter = FootPlanter::new(settings());
            let mut bits = Vec::new();
            for k in 0..200 {
                let phase = k as f32 * 0.05;
                let mut legs = [
                    leg(1.0, 0.1 * (phase * 0.7).fract(), slope_normal(10.0 + phase)),
                    leg(-1.0, -0.05, slope_normal(5.0)),
                ];
                legs[0].foot.y = ANKLE + 0.1 * (phase.fract() - 0.5).max(0.0);
                legs[1].foot.z = 0.2 * (phase * 0.3).fract();
                let out = planter.plan(&FootPlantInput {
                    rig_translation: Vec3::new(phase, 0.0, -phase),
                    rig_rotation: Quat::from_rotation_y(phase * 0.1),
                    legs: &legs,
                    active: k % 70 < 60,
                    dt: DT,
                });
                bits.push(out.pelvis_offset.to_bits());
                for l in &out.legs {
                    bits.extend(l.target.to_array().map(f32::to_bits));
                    bits.extend(l.tilt.to_array().map(f32::to_bits));
                }
            }
            bits
        };
        assert_eq!(run(), run());
    }

    /// The stair rule moves a foot that mostly covers the lower tread back onto it, but not when
    /// that would take it out of the leg's reach: then it rests on the higher tread.
    #[test]
    fn the_stair_move_never_leaves_the_leg_out_of_reach() {
        use crate::foot_ik::shape::{FootSole, SolePoint};
        // Toe 18 cm up, only 20 % of the foot over the riser (edge at 80 % from the heel).
        let sole = FootSole {
            heel: Some(SolePoint {
                offset: Vec3::new(0.0, -ANKLE, 0.06),
                ground: Vec3::new(0.197, 0.0, 0.06),
            }),
            toe: Some(SolePoint {
                offset: Vec3::new(0.0, -ANKLE, -0.17),
                ground: Vec3::new(0.197, 0.18, -0.17),
            }),
            ball: None,
            edge: Some(0.8),
        };
        let plan_with = |hip: Vec3| {
            let mut planter = FootPlanter::new(settings());
            let mut l = leg(1.0, 0.0, Vec3::Y);
            l.hip = hip;
            let mut out = FootPlant::default();
            for _ in 0..60 {
                out = planter.plan_with_soles(&input(std::slice::from_ref(&l), true), &[sole]);
            }
            out.legs[0].target
        };
        // A hip right above: the foot moves back onto the lower tread.
        let near = plan_with(Vec3::new(0.109, HIP_Y, 0.03));
        assert!(near.z > 0.04 && (near.y - ANKLE).abs() < 1.0e-3, "{near}");
        // A hip far ahead and high: moving back would overextend the leg, so it rests up.
        let far = plan_with(Vec3::new(0.109, HIP_Y + 0.25, -0.55));
        assert!(
            far.z.abs() < 1.0e-4 && (far.y - (0.18 + ANKLE)).abs() < 1.0e-3,
            "{far}"
        );
    }

    /// The `FootIkPlan` form re-bases targets for a rig moved by the pelvis offset.
    #[test]
    fn the_ik_plan_rebases_targets_for_a_moved_rig() {
        let mut planter = FootPlanter::new(settings());
        let legs = [leg(1.0, -0.3, Vec3::Y), leg(-1.0, -0.3, Vec3::Y)];
        let plan = settle(&mut planter, &legs, true, 120);
        let ik = plan.ik_plan();
        assert_eq!(ik.pelvis_offset, plan.pelvis_offset);
        for (a, b) in ik.legs.iter().zip(&plan.legs) {
            assert_eq!(a.weight, 1.0);
            assert!((a.target + Vec3::Y * ik.pelvis_offset - b.target).length() < 1.0e-6);
        }
        assert!(FootPlant::default().ik_plan().legs.is_empty());
    }

    /// A small biped: the solver plants the foot on the target and turns it by the tilt.
    #[test]
    fn apply_plants_and_tilts_the_foot() {
        let names: Vec<String> = ["Pelvis", "LHip", "LKnee", "LFoot", "RHip", "RKnee", "RFoot"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parents = vec![None, Some(0), Some(1), Some(2), Some(0), Some(4), Some(5)];
        let bind = vec![
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(-0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
        ];
        let sk = Skeleton::from_local_bind(names, parents, bind).expect("valid biped");
        let ik = [
            FootIkLeg::new(IkChain {
                root: 1,
                mid: 2,
                tip: 3,
            }),
            FootIkLeg::new(IkChain {
                root: 4,
                mid: 5,
                tip: 6,
            }),
        ];
        let pose = sk.bind_pose_pose();
        let at = |p: &Pose, j: u32| crate::ik::joint_position(&sk, p, j);
        let n = slope_normal(15.0);
        let legs: Vec<FootPlantLeg> = ik
            .iter()
            .map(|l| {
                let foot = at(&pose, l.chain.tip);
                FootPlantLeg {
                    hip: at(&pose, l.chain.root),
                    foot,
                    length: 0.9,
                    ankle_height: 0.1,
                    ground: FootGround::Contact(Vec3::new(foot.x, -0.08, foot.z)),
                    normal: n,
                    contact: None,
                }
            })
            .collect();
        let mut planter = FootPlanter::new(settings());
        let plan = settle(&mut planter, &legs, true, 120);
        assert!(
            plan.pelvis_offset < -0.01,
            "the feet are lowered: {}",
            plan.pelvis_offset
        );
        let mut solved = pose.clone();
        apply_foot_plant(&sk, &mut solved, &plan, &ik, Some(Vec3::NEG_Z));
        let globals = sk.globals(&solved, None);
        // The same plan without tilt: the foot turn is the only difference.
        let mut flat_plan = plan.clone();
        for leg in &mut flat_plan.legs {
            leg.tilt = Quat::IDENTITY;
        }
        let mut flat = pose.clone();
        apply_foot_plant(&sk, &mut flat, &flat_plan, &ik, Some(Vec3::NEG_Z));
        let flat_globals = sk.globals(&flat, None);
        for (l, out) in ik.iter().zip(&plan.legs) {
            let tip = l.chain.tip as usize;
            let foot = globals[tip];
            assert!(
                foot.w_axis.truncate().abs_diff_eq(out.target, 2.0e-3),
                "{} vs {}",
                foot.w_axis,
                out.target
            );
            assert!(
                tilt_angle(out.tilt).to_degrees() > 10.0,
                "the foot is tilted: {:?}",
                out.tilt
            );
            let tilted = foot.to_scale_rotation_translation().1;
            let untilted = flat_globals[tip].to_scale_rotation_translation().1;
            assert!(
                (tilted * Vec3::Y - out.tilt * (untilted * Vec3::Y)).length() < 1.0e-3,
                "the foot turned by the tilt in object space"
            );
        }
        // A faded-out plan changes nothing.
        let mut untouched = pose.clone();
        apply_foot_plant(&sk, &mut untouched, &FootPlant::default(), &ik, None);
        assert_eq!(untouched.rotations, pose.rotations);
        assert_eq!(untouched.translations, pose.translations);
    }

    /// The small biped of `apply_plants_and_tilts_the_foot`, planned on a 15 degree slope.
    fn tilted_biped() -> (Skeleton, [FootIkLeg; 2], Pose, FootPlant) {
        let names: Vec<String> = ["Pelvis", "LHip", "LKnee", "LFoot", "RHip", "RKnee", "RFoot"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parents = vec![None, Some(0), Some(1), Some(2), Some(0), Some(4), Some(5)];
        let bind = vec![
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(-0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
        ];
        let sk = Skeleton::from_local_bind(names, parents, bind).expect("valid biped");
        let ik = [
            FootIkLeg::new(IkChain {
                root: 1,
                mid: 2,
                tip: 3,
            }),
            FootIkLeg::new(IkChain {
                root: 4,
                mid: 5,
                tip: 6,
            }),
        ];
        let pose = sk.bind_pose_pose();
        let n = slope_normal(15.0);
        let legs: Vec<FootPlantLeg> = ik
            .iter()
            .map(|l| {
                let foot = crate::ik::joint_position(&sk, &pose, l.chain.tip);
                FootPlantLeg {
                    hip: crate::ik::joint_position(&sk, &pose, l.chain.root),
                    foot,
                    length: 0.9,
                    ankle_height: 0.1,
                    ground: FootGround::Contact(Vec3::new(foot.x, -0.08, foot.z)),
                    normal: n,
                    contact: None,
                }
            })
            .collect();
        let mut planter = FootPlanter::new(settings());
        let plan = settle(&mut planter, &legs, true, 120);
        (sk, ik, pose, plan)
    }

    fn pose_bits(pose: &Pose) -> Vec<u32> {
        let mut bits = Vec::new();
        for i in 0..pose.len() {
            bits.extend(pose.translations[i].to_array().map(f32::to_bits));
            bits.extend(pose.rotations[i].to_array().map(f32::to_bits));
            bits.extend(pose.scales[i].to_array().map(f32::to_bits));
        }
        bits
    }

    /// The apply before animation-11: a full skeleton rebuild for every solve and every foot.
    fn apply_rebuilding_per_leg(
        skeleton: &Skeleton,
        pose: &mut Pose,
        plan: &FootPlant,
        legs: &[FootIkLeg],
        pole: Option<Vec3>,
    ) {
        if plan.is_identity() {
            return;
        }
        if plan.pelvis_offset != 0.0
            && let Some(root) = skeleton.parents.iter().position(|p| p.is_none())
        {
            pose.translations[root].y += plan.pelvis_offset;
        }
        for (leg, out) in legs.iter().zip(&plan.legs) {
            crate::ik::solve_two_bone_into(
                skeleton,
                pose,
                &leg.chain,
                out.target,
                pole,
                1.0,
                &mut Vec::new(),
            );
            let tip = leg.chain.tip as usize;
            if tilt_angle(out.tilt) < 1.0e-6 {
                continue;
            }
            let globals = skeleton.globals(pose, None);
            let foot_rotation = globals[tip].to_scale_rotation_translation().1;
            let parent = skeleton.parents[tip].expect("foot parent") as usize;
            let parent_rotation = globals[parent].to_scale_rotation_translation().1;
            pose.rotations[tip] = (parent_rotation.inverse() * out.tilt * foot_rotation).normalize();
        }
    }

    /// `apply_foot_plant` builds forward kinematics for the whole skeleton at most once per call
    /// (the base rebuilt it once per tilted leg, and the heel-and-toe apply once per solve and
    /// foot), and poses the skeleton bit for bit as the per-leg rebuild did.
    #[test]
    fn apply_foot_plant_builds_globals_once() {
        let (sk, ik, pose, plan) = tilted_biped();
        assert!(
            plan.legs.iter().all(|leg| tilt_angle(leg.tilt) > 1.0e-3),
            "both feet tilt"
        );
        let passes = || crate::skeleton::FULL_GLOBALS_PASSES.with(std::cell::Cell::get);
        let pole = Some(Vec3::NEG_Z);

        let before = passes();
        let mut solved = pose.clone();
        apply_foot_plant(&sk, &mut solved, &plan, &ik, pole);
        let plain = passes() - before;

        let before = passes();
        let mut with_toes = pose.clone();
        apply_foot_plant_with_toes(&sk, &mut with_toes, &plan, &ik, &[None, None], pole);
        let toes = passes() - before;

        let mut buffer = Vec::new();
        let mut reused = pose.clone();
        apply_foot_plant_into(&sk, &mut reused, &plan, &ik, pole, &mut buffer);
        let before = passes();
        let mut again = pose.clone();
        apply_foot_plant_into(&sk, &mut again, &plan, &ik, pole, &mut buffer);
        let warm = passes() - before;

        assert!(
            plain <= 1 && toes <= 1 && warm <= 1,
            "full forward-kinematics passes per apply: plain {plain}, with toes {toes}, warm buffer {warm}"
        );
        let mut reference = pose.clone();
        apply_rebuilding_per_leg(&sk, &mut reference, &plan, &ik, pole);
        assert_eq!(
            pose_bits(&solved),
            pose_bits(&reference),
            "same pose as the per-leg rebuild"
        );
        assert_eq!(
            pose_bits(&again),
            pose_bits(&reference),
            "a reused buffer changes nothing"
        );
        assert_eq!(pose_bits(&reused), pose_bits(&reference));
    }

    fn assert_pose_finite(pose: &Pose) {
        for i in 0..pose.len() {
            assert!(
                pose.rotations[i].is_finite()
                    && pose.translations[i].is_finite()
                    && pose.scales[i].is_finite(),
                "joint {i} is not finite: t {:?} r {:?} s {:?}",
                pose.translations[i],
                pose.rotations[i],
                pose.scales[i]
            );
        }
    }

    /// A foot whose scale has a zero axis must not land a NaN rotation in the pose. The
    /// two-bone solve already refuses that matrix (`rotation_of_matrix`); the tilt write
    /// decomposes it anyway. Review proof for animation-11 (`apply_foot_plant_into`).
    #[test]
    fn review_proof_zero_scale_foot_does_not_put_nan_in_the_pose() {
        let (sk, ik, mut pose, plan) = tilted_biped();
        assert!(
            plan.legs.iter().any(|leg| tilt_angle(leg.tilt) > 1.0e-3),
            "the slope plan tilts a foot, so the tilt write runs"
        );
        pose.scales[ik[0].chain.tip as usize] = Vec3::ZERO;
        pose.scales[ik[1].chain.tip as usize] = Vec3::ZERO;
        apply_foot_plant_into(&sk, &mut pose, &plan, &ik, Some(Vec3::NEG_Z), &mut Vec::new());
        assert_pose_finite(&pose);
    }

    /// Same input through the heel-and-toe apply, which rewrites the foot rotation whenever
    /// the plan is active (no tilt-angle skip).
    #[test]
    fn review_proof_zero_scale_foot_with_toes_does_not_put_nan_in_the_pose() {
        let (sk, ik, mut pose, plan) = tilted_biped();
        pose.scales[ik[0].chain.tip as usize] = Vec3::ZERO;
        pose.scales[ik[1].chain.tip as usize] = Vec3::ZERO;
        apply_foot_plant_with_toes_into(
            &sk,
            &mut pose,
            &plan,
            &ik,
            &[None, None],
            Some(Vec3::NEG_Z),
            &mut ApplyBuffers::default(),
        );
        assert_pose_finite(&pose);
    }

    /// A zero-length foot bone (local translation at the origin, scale 1) makes the solve
    /// return before it writes. The pose stays finite.
    #[test]
    fn review_proof_zero_length_foot_bone_does_not_put_nan_in_the_pose() {
        let (sk, ik, mut pose, plan) = tilted_biped();
        pose.translations[ik[0].chain.tip as usize] = Vec3::ZERO;
        pose.translations[ik[1].chain.tip as usize] = Vec3::ZERO;
        apply_foot_plant_into(&sk, &mut pose, &plan, &ik, Some(Vec3::NEG_Z), &mut Vec::new());
        assert_pose_finite(&pose);
    }

    /// One second of the same grounded inputs at 30, 60 and 144 Hz. Weight is linear in `dt`;
    /// the offset and tilt eases are `1 - exp(-dt/tau)`, so a fixed duration lands on the same
    /// plan (float rounding aside).
    #[test]
    fn review_proof_planter_one_second_agrees_at_30_60_and_144_hz() {
        let legs = [leg(1.0, 0.05, slope_normal(12.0)), leg(-1.0, 0.0, Vec3::Y)];
        let mut plans = Vec::new();
        for hz in [30.0_f32, 60.0, 144.0] {
            let mut planter = FootPlanter::new(settings());
            let mut plan = FootPlant::default();
            let steps = hz as u32;
            let dt = 1.0 / hz;
            for _ in 0..steps {
                planter.plan_into(
                    &FootPlantInput {
                        rig_translation: Vec3::ZERO,
                        rig_rotation: Quat::IDENTITY,
                        legs: &legs,
                        active: true,
                        dt,
                    },
                    &mut plan,
                );
            }
            plans.push(plan);
        }
        let base = &plans[0];
        for (hz, plan) in [60.0_f32, 144.0].into_iter().zip(plans.iter().skip(1)) {
            assert!(
                (plan.weight - base.weight).abs() < 1.0e-4,
                "weight at {hz} Hz: {} vs {}",
                plan.weight,
                base.weight
            );
            assert!(
                (plan.pelvis_offset - base.pelvis_offset).abs() < 1.0e-3,
                "pelvis at {hz} Hz: {} vs {}",
                plan.pelvis_offset,
                base.pelvis_offset
            );
            assert_eq!(plan.legs.len(), base.legs.len());
            for (i, (leg, other)) in plan.legs.iter().zip(&base.legs).enumerate() {
                assert!(
                    leg.target.distance(other.target) < 1.0e-3,
                    "target {i} at {hz} Hz: {:?} vs {:?}",
                    leg.target,
                    other.target
                );
                assert!(
                    (tilt_angle(leg.tilt) - tilt_angle(other.tilt)).abs() < 1.0e-3,
                    "tilt {i} at {hz} Hz"
                );
            }
        }
    }

    /// `plan_into` and `report_into` give the bits `plan` and `report` give, also when the
    /// output buffers hold another frame's (or another rig's) values.
    #[test]
    fn plan_into_and_report_into_match_the_allocating_forms() {
        let legs = [leg(1.0, -0.1, slope_normal(12.0)), leg(-1.0, 0.05, Vec3::Y)];
        let mut a = FootPlanter::new(settings());
        let mut b = FootPlanter::new(settings());
        let mut out = FootPlant {
            pelvis_offset: 7.0,
            weight: 3.0,
            legs: vec![
                FootPlantLegOut {
                    target: Vec3::splat(9.0),
                    tilt: Quat::IDENTITY,
                    planted: 0.5,
                    lock: 0.5,
                    toe: Quat::IDENTITY,
                    heel_gap: 1.0,
                    toe_gap: 1.0,
                };
                5
            ],
        };
        let mut report = FootPlantReport {
            planted: vec![9.0; 7],
            ..FootPlantReport::default()
        };
        for k in 0..90 {
            let input = input(&legs, k % 40 < 30);
            let expected = a.plan(&input);
            b.plan_into(&input, &mut out);
            assert_eq!(out, expected, "frame {k}");
            b.report_into(&out, &mut report);
            assert_eq!(report, a.report(&expected), "frame {k}");
        }
    }
}
