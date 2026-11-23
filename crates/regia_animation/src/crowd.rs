// SPDX-License-Identifier: MPL-2.0
//! Crowd animation: hundreds of rigs on shared, retargeted locomotion clips, deterministically.
//!
//! Specification: `docs/specs/85-retarget-crowds.md` § 3. A [`CrowdArchetype`] is one rig family:
//! a skeleton and its locomotion clips (idle, walk, run, …, usually baked through a
//! [`Retargeter`]) sorted by their measured ground speed. Clips without a root-motion track, the
//! importer's default, are measured too: from the joint that carries the travel, or for treadmill
//! clips from the stance foot (spec § 3.2). A [`CrowdAnimator`] advances many agents per fixed
//! step:
//!
//! 1. **Phase.** One normalised phase per agent, shared by every gait (phase-synchronised
//!    blending). The ground speed picks the gait pair and blend weight; the phase advances by the
//!    ground speed over the blended stride length (stride matching), the playback rate clamped.
//!    Gait speeds scale with the agent's hip-height ratio when it has its own proportions.
//! 2. **Due agents.** A level-of-detail tier per agent (distance to the viewer) sets its update
//!    interval; agents are staggered by id so far agents spread over steps.
//! 3. **Pose sharing.** In tiers with phase buckets, agents in the same (archetype, gait pair,
//!    weight bucket, phase bucket) share one pose evaluated at the bucket's value.
//! 4. **Evaluation** of the unique poses and **finish** (instance retarget, forward kinematics,
//!    skin palette) of the due agents run in parallel chunks over disjoint slots, so the result is
//!    bit-identical for every chunk size and thread count.
//! 5. **Foot locking** (exact tiers): each gait's foot-contact table, weighted by the blend, says
//!    which feet are planted; a planted foot is held where it touched down by two-bone leg IK and
//!    fades out over a few steps once it lifts. Blending gaits with different duty factors at a
//!    shared phase otherwise slides planted feet however exact the stride matching is.
//!
//! The ECS bridge ([`CrowdAnimationPlugin`]) steps a [`CrowdAnimation`] resource in `FixedUpdate`
//! and writes [`SkinPalette`] / [`JointGlobals`] on [`CrowdMember`] rigs.
//!
//! Provenance (`docs/provenance.md`): speed-indexed gait blending with normalised-phase sync and
//! stride matching (playback rate = ground speed / clip speed) follows Bruderlin & Williams,
//! "Motion Signal Processing", SIGGRAPH 1995 and Park, Shin & Shin, "On-line Locomotion
//! Generation Based on Motion Blending", SCA 2002; sharing poses between crowd members by
//! quantised playback state and updating far members less often are the level-of-detail
//! approaches surveyed in Thalmann & Musse, "Crowd Simulation" (Springer, 2nd ed. 2013), ch. 7.
//! Foot locking enforces footplant constraints with IK as in Kovar, Schreiner & Gleicher,
//! "Footskate Cleanup for Motion Capture Editing", SCA 2002.
//! The bucketing, stagger and digest are ours.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};
use regia_scene::Transform;

use crate::bind_pose::{InstanceBindPose, retarget_to_instance};
use crate::clip::AnimationClip;
use crate::components::{JointGlobals, SkinPalette, SkinnedMesh};
use crate::foot_ik::lock::{PlantLock, PlantRelease};
use crate::ik::{IkChain, solve_two_bone_with_globals};
use crate::num::{det_atan2, det_rotation_y};
use crate::pose::Pose;
use crate::retarget::{HumanoidBone, RetargetProfile, Retargeter, hip_height, position_of};
use crate::retarget_check::stance_velocity;
use crate::skeleton::Skeleton;

/// Errors building crowd archetypes and agents.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CrowdError {
    /// An archetype needs at least one clip.
    #[error("archetype {0:?} has no clips")]
    NoGaits(String),
    /// A clip has a non-finite or non-positive duration.
    #[error("clip {0:?} has no usable duration")]
    BadClip(String),
    /// An agent names an archetype that does not exist.
    #[error("archetype {0} does not exist")]
    UnknownArchetype(usize),
    /// An agent's proportions do not match its archetype's skeleton.
    #[error("the agent's proportions have {got} joints, the archetype has {expected}")]
    ProportionsMismatch {
        /// Joints in the proportions.
        got: usize,
        /// Joints in the archetype skeleton.
        expected: usize,
    },
    /// A position, velocity, yaw or phase is not finite, or a gait speed is negative.
    #[error("agent value {0} is not usable")]
    BadValue(&'static str),
    /// [`CrowdArchetype::with_gait_speed`] names a clip the archetype does not have.
    #[error("archetype has no clip {0:?}")]
    UnknownClip(String),
}

impl CrowdError {
    /// The stable reason code (agent protocol, `book/agents/retarget-crowds.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CrowdError::NoGaits(_) => "no_gaits",
            CrowdError::BadClip(_) => "bad_clip",
            CrowdError::UnknownArchetype(_) => "unknown_archetype",
            CrowdError::ProportionsMismatch { .. } => "proportions_mismatch",
            CrowdError::BadValue(_) => "bad_value",
            CrowdError::UnknownClip(_) => "unknown_clip",
        }
    }
}

/// One locomotion clip of an archetype.
#[derive(Clone, Debug)]
pub struct CrowdGait {
    /// The clip (in the archetype skeleton's space).
    pub clip: Arc<AnimationClip>,
    /// Ground speed over one lap, m/s: measured (spec 85 § 3.2) or set with
    /// [`CrowdArchetype::with_gait_speed`]. 0 for a clip that goes nowhere.
    pub speed: f32,
    /// Cycle length (the clip duration), seconds.
    pub cycle: f32,
    /// Ground velocity in the archetype's object space, m/s (`speed` long).
    pub velocity: Vec3,
    /// The joint whose ground travel is taken out of the pose, if any.
    pin: Option<GroundPin>,
}

impl CrowdGait {
    /// Sample the clip at normalized `phase` into `pose`, the ground travel taken out.
    fn sample_pinned(&self, phase: f32, bind: &Pose, pose: &mut Pose) {
        pose.copy_from(bind);
        self.clip.sample_pose(phase * self.cycle, pose);
        if let Some(pin) = &self.pin {
            pin.apply(pose);
        }
    }
}

/// Holds a joint in place on the object's ground plane; the height stays animated.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GroundPin {
    joint: usize,
    /// The joint's translation at the start of the clip.
    origin: Vec3,
    /// The object's up direction in the frame of the joint's parent (unit length).
    up: Vec3,
}

impl GroundPin {
    fn apply(&self, pose: &mut Pose) {
        if let Some(translation) = pose.translations.get_mut(self.joint) {
            let delta = *translation - self.origin;
            *translation = self.origin + self.up * delta.dot(self.up);
        }
    }
}

/// One rig family of a crowd: a skeleton and its locomotion clips sorted by speed.
#[derive(Clone, Debug)]
pub struct CrowdArchetype {
    /// Archetype name.
    pub name: String,
    /// The shared skeleton.
    pub skeleton: Arc<Skeleton>,
    gaits: Vec<CrowdGait>,
    bind: Pose,
    hips: Option<usize>,
    hip_height: f32,
    forward: Vec3,
    /// Left and right upper leg → lower leg → foot chains (foot locking).
    legs: Option<[IkChain; 2]>,
    /// Per gait (same order as `gaits`), per leg: the foot's contact window.
    contacts: Vec<[ContactWindow; 2]>,
}

/// When a gait plants one foot, in normalised phase.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ContactWindow {
    /// The foot never lifts (an idle): it follows the other gait of a blend.
    Always,
    /// Planted from phase `start` for `length` (wrapping; `length` 0 = never planted).
    Span {
        /// Touch-down phase.
        start: f32,
        /// Planted fraction of the cycle.
        length: f32,
    },
}

impl ContactWindow {
    /// The longest circular run of planted samples.
    fn from_table(row: &[bool; CONTACT_SAMPLES]) -> Self {
        if row.iter().all(|&p| p) {
            return ContactWindow::Always;
        }
        // Start scanning just after a lifted sample so no run wraps past the scan's end.
        let lifted = row.iter().position(|&p| !p).unwrap_or(0);
        let (mut best, mut run) = ((0usize, 0usize), (0usize, 0usize));
        for n in 1..=CONTACT_SAMPLES {
            let k = (lifted + n) % CONTACT_SAMPLES;
            if row[k] {
                if run.1 == 0 {
                    run.0 = k;
                }
                run.1 += 1;
                if run.1 > best.1 {
                    best = run;
                }
            } else {
                run.1 = 0;
            }
        }
        ContactWindow::Span {
            start: best.0 as f32 / CONTACT_SAMPLES as f32,
            length: best.1 as f32 / CONTACT_SAMPLES as f32,
        }
    }

    /// The window of a blend `weight` of the way from `self` to `other`: touch-down phase
    /// (on the short way round) and length interpolated, so the planted span follows the
    /// blended stance instead of switching between the two gaits' stances.
    fn blend(self, other: Self, weight: f32) -> Self {
        match (self, other) {
            (ContactWindow::Always, x) | (x, ContactWindow::Always) => x,
            (ContactWindow::Span { start: a, length: la }, ContactWindow::Span { start: b, length: lb }) => {
                let delta = (b - a + 0.5).rem_euclid(1.0) - 0.5;
                ContactWindow::Span {
                    start: (a + delta * weight).rem_euclid(1.0),
                    length: la + (lb - la) * weight,
                }
            }
        }
    }

    /// The planted share of the cycle.
    fn share(self) -> f32 {
        match self {
            ContactWindow::Always => 1.0,
            ContactWindow::Span { length, .. } => length,
        }
    }

    /// Whether `phase` lies in the window.
    fn contains(self, phase: f32) -> bool {
        match self {
            ContactWindow::Always => true,
            ContactWindow::Span { start, length } => (phase - start).rem_euclid(1.0) < length,
        }
    }
}

/// Phase samples of a gait's foot-contact table.
const CONTACT_SAMPLES: usize = 64;

/// A foot is planted within this fraction of the hip height above its lowest point in the
/// gait (3 cm on a 1 m hip), or within `CONTACT_RANGE` of its lift if that is more.
const CONTACT_HEIGHT: f32 = 0.03;

/// A foot is planted within this fraction of its height range over the gait. Authored motion
/// holds a stance foot flat; in captured motion the ankle dips twice in stance, by 3 to 4 cm on a
/// 1 m hip, so a fixed band alone splits the stance.
const CONTACT_RANGE: f32 = 0.25;

/// A planted foot moves forward over the ground, or sideways, at most this fraction of the gait
/// speed, plus `CONTACT_SPEED_FLOOR`. A swinging foot moves forward at speed / (1 − duty), well
/// above this.
const CONTACT_SPEED: f32 = 0.5;

/// A planted foot lags backward over the ground at most this fraction of the gait speed, plus
/// `CONTACT_SPEED_FLOOR`. Only a forward foot can be a swing: on the CesiumMan walk one stance
/// foot peaks at 1.8 times the gait speed against the ground's motion, which is still stance.
const CONTACT_LAG: f32 = 1.0;

/// Longest gap, in phase samples, closed in a planted run when the foot stayed low through it
/// (about 3 % of the cycle). A clip whose first key comes after time 0 clamps there, and the
/// foot looks stopped for two samples in the middle of its stance.
const CONTACT_GAP: usize = 2;

/// Ground speed always allowed to a planted foot, m/s.
const CONTACT_SPEED_FLOOR: f32 = 0.1;

/// Steps over which a released foot lock fades out.
const LOCK_RELEASE_STEPS: f32 = 4.0;

/// A locked foot further than this fraction of the hip height from its animated position
/// re-locks where the animation puts it.
const LOCK_MAX_SLIP: f32 = 0.25;

/// Ground speed, in hip heights per second, under which a clip without a root-motion track counts
/// as going nowhere: a mocap idle's drift, a sway of the feet.
const MIN_GAIT_SPEED: f32 = 0.1;

/// The contiguous upper leg → lower leg → foot chains of a humanoid skeleton.
fn leg_chains(skeleton: &Skeleton) -> Option<[IkChain; 2]> {
    let roles = RetargetProfile::guess_humanoid(skeleton).roles;
    let joint = |bone: HumanoidBone| roles.get(bone.name()).and_then(|name| skeleton.joint_index(name));
    let chain = |upper, lower, foot| {
        let chain = IkChain {
            root: joint(upper)?,
            mid: joint(lower)?,
            tip: joint(foot)?,
        };
        let parent = |j: u32| skeleton.parents.get(j as usize).copied().flatten();
        (parent(chain.mid) == Some(chain.root) && parent(chain.tip) == Some(chain.mid)).then_some(chain)
    };
    Some([
        chain(
            HumanoidBone::LeftUpperLeg,
            HumanoidBone::LeftLowerLeg,
            HumanoidBone::LeftFoot,
        )?,
        chain(
            HumanoidBone::RightUpperLeg,
            HumanoidBone::RightLowerLeg,
            HumanoidBone::RightFoot,
        )?,
    ])
}

/// Both feet (left, right) of one gait at each phase sample, the ground travel pinned.
fn foot_samples(
    skeleton: &Skeleton,
    bind: &Pose,
    gait: &CrowdGait,
    legs: &[IkChain; 2],
) -> [[Vec3; 2]; CONTACT_SAMPLES] {
    let mut samples = [[Vec3::ZERO; 2]; CONTACT_SAMPLES];
    let mut pose = Pose::default();
    let mut globals = Vec::new();
    for (k, sample) in samples.iter_mut().enumerate() {
        gait.sample_pinned(k as f32 / CONTACT_SAMPLES as f32, bind, &mut pose);
        globals_into(skeleton, &pose, &mut globals);
        *sample = legs.map(|leg| {
            globals
                .get(leg.tip as usize)
                .map_or(Vec3::ZERO, |g| position_of(*g))
        });
    }
    samples
}

/// Foot-contact table of one gait: mark the phases where each foot is low (`CONTACT_HEIGHT` or
/// `CONTACT_RANGE` over its lowest height) and on the ground (its root-pinned velocity cancels
/// the gait's ground velocity, within `CONTACT_SPEED` forward and sideways and `CONTACT_LAG`
/// backward), the height and velocity footplant test of Kovar, Schreiner & Gleicher 2002. Short
/// gaps are then closed (`close_contact_gaps`).
fn contact_table(
    samples: &[[Vec3; 2]; CONTACT_SAMPLES],
    gait: &CrowdGait,
    hip_height: f32,
) -> [[bool; CONTACT_SAMPLES]; 2] {
    let feet: [[Vec3; CONTACT_SAMPLES]; 2] = [0, 1].map(|side| std::array::from_fn(|k| samples[k][side]));
    let dt = gait.cycle / CONTACT_SAMPLES as f32;
    let max_speed = CONTACT_SPEED * gait.speed + CONTACT_SPEED_FLOOR;
    let max_lag = CONTACT_LAG * gait.speed + CONTACT_SPEED_FLOOR;
    let forward = gait.velocity.normalize_or_zero();
    let mut table = [[false; CONTACT_SAMPLES]; 2];
    for (row, foot) in table.iter_mut().zip(&feet) {
        let lowest = foot.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let highest = foot.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
        let band = (CONTACT_HEIGHT * hip_height).max(CONTACT_RANGE * (highest - lowest));
        let low: [bool; CONTACT_SAMPLES] = std::array::from_fn(|k| foot[k].y <= lowest + band);
        for (k, planted) in row.iter_mut().enumerate() {
            let before = foot[(k + CONTACT_SAMPLES - 1) % CONTACT_SAMPLES];
            let after = foot[(k + 1) % CONTACT_SAMPLES];
            let world_velocity = horizontal(after - before) / (2.0 * dt) + gait.velocity;
            let along = world_velocity.dot(forward);
            let across = (world_velocity - forward * along).length();
            *planted = low[k] && (-max_lag..=max_speed).contains(&along) && across <= max_speed;
        }
        close_contact_gaps(row, &low);
    }
    table
}

/// Mark as planted every gap of at most `CONTACT_GAP` samples that the foot spent low.
fn close_contact_gaps(row: &mut [bool; CONTACT_SAMPLES], low: &[bool; CONTACT_SAMPLES]) {
    // Start from a planted sample so no gap wraps past the scan's end.
    let Some(first) = row.iter().position(|&planted| planted) else {
        return;
    };
    let at = |k: usize| (first + k) % CONTACT_SAMPLES;
    let mut k = 0;
    while k < CONTACT_SAMPLES {
        let gap = (k..CONTACT_SAMPLES).take_while(|&n| !row[at(n)]).count();
        if gap > 0 && gap <= CONTACT_GAP && (k..k + gap).all(|n| low[at(n)]) {
            (k..k + gap).for_each(|n| row[at(n)] = true);
        }
        k += gap.max(1);
    }
}

fn horizontal(v: Vec3) -> Vec3 {
    Vec3::new(v.x, 0.0, v.z)
}

/// Object-space ground travel of `joint` over one lap of `clip`, and the pin that takes it out of
/// the pose. The travel goes through the parent's bind frame, so a joint under a scaled or turned
/// root measures meters on the ground. `None` when no track drives the joint.
fn joint_ground_travel(
    skeleton: &Skeleton,
    bind_globals: &[Mat4],
    clip: &AnimationClip,
    joint: u32,
) -> Option<(Vec3, GroundPin)> {
    let track = clip.tracks.iter().find(|t| t.joint == joint)?;
    let origin = track.translations.sample(0.0);
    let delta = track.translations.sample(clip.duration) - origin;
    let parent = skeleton
        .parents
        .get(joint as usize)
        .copied()
        .flatten()
        .and_then(|p| bind_globals.get(p as usize));
    let (travel, up) = match parent {
        Some(parent) => {
            let (_, rotation, _) = parent.to_scale_rotation_translation();
            (
                parent.transform_vector3(delta),
                (rotation.inverse() * Vec3::Y).normalize(),
            )
        }
        None => (delta, Vec3::Y),
    };
    let pin = GroundPin {
        joint: joint as usize,
        origin,
        up,
    };
    Some((horizontal(travel), pin))
}

/// The ground travel of `clip` and its pin: the root-motion joint's, or without a root-motion
/// track the hips or the ancestor that travels furthest, at `min_speed` or more.
fn clip_ground_travel(
    skeleton: &Skeleton,
    bind_globals: &[Mat4],
    clip: &AnimationClip,
    hips: Option<usize>,
    min_speed: f32,
) -> Option<(Vec3, GroundPin)> {
    if let Some(motion) = &clip.root_motion {
        return joint_ground_travel(skeleton, bind_globals, clip, motion.joint);
    }
    let mut best: Option<(Vec3, GroundPin)> = None;
    let mut joint = hips.map(|h| h as u32);
    while let Some(j) = joint {
        if let Some(found) = joint_ground_travel(skeleton, bind_globals, clip, j)
            && found.0.length() > best.map_or(0.0, |b| b.0.length())
        {
            best = Some(found);
        }
        joint = skeleton.parents.get(j as usize).copied().flatten();
    }
    best.filter(|(travel, _)| travel.length() / clip.duration >= min_speed)
}

impl CrowdArchetype {
    /// An archetype from clips already in `skeleton`'s space. Each clip's ground speed is measured
    /// (spec 85 § 3.2): from its root motion; without a root-motion track from the hips or the
    /// ancestor that travels; for an in-place clip from the stance foot, which slides back at the
    /// gait speed. The forward direction is the fastest clip's travel direction (−Z when no clip
    /// travels).
    ///
    /// # Errors
    ///
    /// [`CrowdError::NoGaits`] without clips, [`CrowdError::BadClip`] for a clip without a
    /// finite positive duration.
    pub fn new(
        name: impl Into<String>,
        skeleton: Arc<Skeleton>,
        clips: Vec<AnimationClip>,
    ) -> Result<Self, CrowdError> {
        let name = name.into();
        if clips.is_empty() {
            return Err(CrowdError::NoGaits(name));
        }
        let bind = skeleton.bind_pose_pose();
        let globals = skeleton.globals(&bind, None);
        let hips = RetargetProfile::guess_humanoid(&skeleton)
            .roles
            .get(HumanoidBone::Hips.name())
            .and_then(|j| skeleton.joint_index(j))
            .map(|j| j as usize);
        let hip_height = hips.and_then(|h| hip_height(&globals, h)).unwrap_or(1.0);
        let legs = leg_chains(&skeleton);
        let min_speed = MIN_GAIT_SPEED * hip_height;
        let mut gaits = Vec::with_capacity(clips.len());
        for clip in clips {
            if !(clip.duration.is_finite() && clip.duration > 1e-4) {
                return Err(CrowdError::BadClip(clip.name));
            }
            let found = clip_ground_travel(&skeleton, &globals, &clip, hips, min_speed);
            let velocity = found.map_or(Vec3::ZERO, |(travel, _)| travel) / clip.duration;
            let mut gait = CrowdGait {
                speed: velocity.length(),
                cycle: clip.duration,
                velocity,
                pin: found.map(|(_, pin)| pin),
                clip: Arc::new(clip),
            };
            if !gait.speed.is_finite() {
                gait.speed = 0.0;
                gait.velocity = Vec3::ZERO;
            }
            if gait.speed < min_speed
                && let Some(legs) = &legs
            {
                let samples = foot_samples(&skeleton, &bind, &gait, legs);
                let stance = stance_velocity(&samples, gait.cycle / CONTACT_SAMPLES as f32);
                if stance.is_finite() && stance.length() >= min_speed {
                    gait.speed = stance.length();
                    gait.velocity = stance;
                }
            }
            gaits.push(gait);
        }
        let forward = gaits
            .iter()
            .fold((0.0f32, Vec3::NEG_Z), |best, g| {
                if g.speed > best.0 {
                    (g.speed, g.velocity.normalize_or_zero())
                } else {
                    best
                }
            })
            .1;
        let forward = if forward.length_squared() > 0.5 {
            forward
        } else {
            Vec3::NEG_Z
        };
        let mut archetype = CrowdArchetype {
            name,
            bind,
            skeleton,
            gaits,
            hips,
            hip_height,
            forward,
            legs,
            contacts: Vec::new(),
        };
        archetype.sort_gaits();
        Ok(archetype)
    }

    /// Set the ground speed of the clip named `clip` by hand, for a rig whose speed cannot be
    /// measured (no root motion and no legs the role guesser finds) or is measured wrong. An
    /// unmeasured clip travels along [`CrowdArchetype::forward`], −Z unless another clip travels.
    ///
    /// # Errors
    ///
    /// [`CrowdError::UnknownClip`] for a clip the archetype does not have,
    /// [`CrowdError::BadValue`] for a negative or non-finite speed.
    pub fn with_gait_speed(mut self, clip: &str, speed: f32) -> Result<Self, CrowdError> {
        if !(speed.is_finite() && speed >= 0.0) {
            return Err(CrowdError::BadValue("speed"));
        }
        let forward = self.forward;
        let gait = self
            .gaits
            .iter_mut()
            .find(|g| g.clip.name == clip)
            .ok_or_else(|| CrowdError::UnknownClip(clip.to_string()))?;
        gait.speed = speed;
        gait.velocity = gait.velocity.normalize_or(forward) * speed;
        self.sort_gaits();
        Ok(self)
    }

    /// Sort the gaits by speed and rebuild their contact windows.
    fn sort_gaits(&mut self) {
        self.gaits.sort_by(|a, b| {
            a.speed
                .total_cmp(&b.speed)
                .then_with(|| a.clip.name.cmp(&b.clip.name))
        });
        self.contacts = match &self.legs {
            Some(legs) => self
                .gaits
                .iter()
                .map(|gait| {
                    let samples = foot_samples(&self.skeleton, &self.bind, gait, legs);
                    contact_table(&samples, gait, self.hip_height).map(|row| ContactWindow::from_table(&row))
                })
                .collect(),
            None => Vec::new(),
        };
    }

    /// An archetype for `rt`'s target skeleton, with `source_clips` baked through `rt`.
    ///
    /// # Errors
    ///
    /// As [`CrowdArchetype::new`].
    pub fn retargeted(
        name: impl Into<String>,
        rt: &Retargeter,
        source_clips: &[AnimationClip],
    ) -> Result<Self, CrowdError> {
        Self::new(
            name,
            Arc::new(rt.target().clone()),
            rt.retarget_library(source_clips),
        )
    }

    /// The gaits, slowest first.
    #[must_use]
    pub fn gaits(&self) -> &[CrowdGait] {
        &self.gaits
    }

    /// The bind-pose hip height used for the instance speed ratio, meters.
    #[must_use]
    pub fn hip_height(&self) -> f32 {
        self.hip_height
    }

    /// The archetype's forward direction in its object space (ground plane, unit length).
    #[must_use]
    pub fn forward(&self) -> Vec3 {
        self.forward
    }

    /// Whether foot locking can run: both legs have a contiguous upper leg → lower leg → foot
    /// chain.
    #[must_use]
    pub fn has_legs(&self) -> bool {
        self.legs.is_some()
    }

    /// The share of gait `gait`'s cycle each foot (left, right) is planted, 1 for a foot that
    /// never lifts. `None` for a rig without legs. A moving gait near 0 has no usable contact
    /// windows, and foot locking does nothing for it.
    #[must_use]
    pub fn planted_share(&self, gait: usize) -> Option<[f32; 2]> {
        self.contacts.get(gait).map(|c| c.map(ContactWindow::share))
    }

    /// Whether each foot is planted for gait pair `(gait, gait + 1)` at weight `weight` and
    /// phase `phase`: the two gaits' contact windows blended by the weight.
    fn planted(&self, gait: usize, weight: f32, phase: f32) -> [bool; 2] {
        let next = (gait + 1).min(self.gaits.len().saturating_sub(1));
        let (Some(a), Some(b)) = (self.contacts.get(gait), self.contacts.get(next)) else {
            return [false; 2];
        };
        [0, 1].map(|side| a[side].blend(b[side], weight).contains(phase.rem_euclid(1.0)))
    }

    /// Gait pair, weight, playback rate and cycle for ground speed `v` with gait speeds scaled
    /// by `ratio`.
    fn select(&self, v: f32, ratio: f32, settings: &CrowdSettings) -> (usize, f32, f32, f32) {
        let n = self.gaits.len();
        let speed = |k: usize| self.gaits[k].speed * ratio;
        let (i, w) = if n == 1 || v <= speed(0) {
            (0, 0.0)
        } else if v >= speed(n - 1) {
            (n - 2, 1.0)
        } else {
            let i = (0..n - 1).rfind(|&k| speed(k) <= v).unwrap_or(0);
            let span = speed(i + 1) - speed(i);
            (
                i,
                if span > 1e-6 {
                    ((v - speed(i)) / span).clamp(0.0, 1.0)
                } else {
                    0.0
                },
            )
        };
        let j = (i + 1).min(n - 1);
        let cycle = self.gaits[i].cycle + (self.gaits[j].cycle - self.gaits[i].cycle) * w;
        // Stride matching: a planted foot of gait k slides back by speed·cycle per lap, so the
        // blended pose slides back by the blended stride per lap. Laps per second = v / stride.
        let stride = |k: usize| speed(k) * self.gaits[k].cycle;
        let blended_stride = stride(i) + (stride(j) - stride(i)) * w;
        let rate = if blended_stride > 1e-4 {
            let (lo, hi) = (settings.min_rate, settings.max_rate);
            let raw = v * cycle / blended_stride;
            if lo.is_finite() && hi.is_finite() && lo <= hi {
                raw.clamp(lo, hi)
            } else {
                1.0
            }
        } else {
            1.0
        };
        (i, w, rate, cycle)
    }

    /// Evaluate the local pose of gait pair `(gait, gait + 1)` at weight `weight` and phase
    /// `phase` into `out`, the ground travel pinned.
    fn evaluate(&self, gait: usize, weight: f32, phase: f32, out: &mut Pose, scratch: &mut Pose) {
        self.gaits[gait].sample_pinned(phase, &self.bind, out);
        let next = gait + 1;
        if weight > 0.0 && next < self.gaits.len() {
            self.gaits[next].sample_pinned(phase, &self.bind, scratch);
            out.blend_from(scratch, weight);
        }
    }
}

/// One level-of-detail tier of a crowd.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrowdLod {
    /// Agents at most this far from the viewer use this tier (the last tier takes the rest).
    pub distance: f32,
    /// Update every `interval` fixed steps (1 = every step).
    pub interval: u32,
    /// Phase buckets per cycle for pose sharing; 0 = every agent gets its own exact pose.
    pub phase_buckets: u32,
}

/// Crowd settings.
#[derive(Clone, Debug, PartialEq)]
pub struct CrowdSettings {
    /// The fixed step, seconds.
    pub fixed_step: f32,
    /// Level-of-detail tiers, nearest first. Without a viewer every agent is in the first tier.
    pub lods: Vec<CrowdLod>,
    /// Lowest playback rate of stride matching.
    pub min_rate: f32,
    /// Highest playback rate of stride matching.
    pub max_rate: f32,
    /// Agents (and unique poses) per parallel chunk; 0 runs serially on the caller's thread.
    pub chunk: usize,
    /// Lock planted feet in place (two-bone leg IK) for agents in exact tiers (`interval` 1,
    /// no phase buckets) whose archetype has legs.
    pub foot_lock: bool,
}

impl Default for CrowdSettings {
    fn default() -> Self {
        CrowdSettings {
            fixed_step: 1.0 / 30.0,
            lods: vec![
                CrowdLod {
                    distance: 15.0,
                    interval: 1,
                    phase_buckets: 0,
                },
                CrowdLod {
                    distance: 40.0,
                    interval: 2,
                    phase_buckets: 64,
                },
                CrowdLod {
                    distance: f32::INFINITY,
                    interval: 4,
                    phase_buckets: 16,
                },
            ],
            min_rate: 0.6,
            max_rate: 1.6,
            chunk: 32,
            foot_lock: true,
        }
    }
}

/// An agent's handle in a [`CrowdAnimator`] (its index; agents are never removed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CrowdAgentId(pub u32);

/// What to add to a crowd.
#[derive(Clone, Debug, Default)]
pub struct CrowdAgentDesc {
    /// Archetype index ([`CrowdAnimator::add_archetype`]).
    pub archetype: usize,
    /// World position.
    pub position: Vec3,
    /// Initial yaw (radians about +Y, applied to the archetype's forward).
    pub yaw: f32,
    /// World velocity, m/s.
    pub velocity: Vec3,
    /// Initial normalised phase (0..1); spreads agents out of step.
    pub phase: f32,
    /// The agent's own proportions (over the archetype skeleton).
    pub proportions: Option<InstanceBindPose>,
}

/// An agent's playback state (read-only view).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrowdAgentState {
    /// Archetype index.
    pub archetype: usize,
    /// LOD tier of the last step.
    pub tier: usize,
    /// Lower gait of the blended pair.
    pub gait: usize,
    /// Blend weight towards `gait + 1`.
    pub weight: f32,
    /// Normalised phase.
    pub phase: f32,
    /// Playback rate (stride matching).
    pub rate: f32,
    /// Ground speed, m/s.
    pub speed: f32,
    /// Yaw, radians.
    pub yaw: f32,
    /// World position.
    pub position: Vec3,
    /// Hip-height ratio of the agent's proportions (1 without).
    pub hip_ratio: f32,
    /// Whether each foot (left, right) is held by a foot lock after the last step.
    pub feet_locked: [bool; 2],
    /// Whether the gait plants each foot (left, right) at the last step: the blended foot-contact
    /// tables, in exact tiers of archetypes with legs (always false elsewhere).
    pub feet_planted: [bool; 2],
}

/// Counters of the last [`CrowdAnimator::step`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CrowdStats {
    /// Agents in the crowd.
    pub agents: usize,
    /// Archetypes.
    pub archetypes: usize,
    /// Fixed steps run.
    pub step: u64,
    /// Poses evaluated in the last step (unique keys).
    pub evaluated_poses: usize,
    /// Due agents that reused a pose another agent of the step evaluated.
    pub shared_hits: usize,
    /// Agents updated (due) in the last step.
    pub updated: usize,
    /// Agents skipped by their LOD interval in the last step.
    pub skipped: usize,
    /// Agents per LOD tier in the last step.
    pub per_tier: Vec<usize>,
    /// Feet held by a foot lock after the last step.
    pub locked_feet: usize,
}

/// One foot's lock: the crate's lock state machine ([`crate::foot_ik::lock::PlantLock`], a fixed
/// pin released over `LOCK_RELEASE_STEPS` whole steps) and the world position it holds the foot at
/// (`None` once released).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct FootLock {
    target: Option<Vec3>,
    lock: PlantLock,
}

#[derive(Clone, Debug)]
struct Agent {
    archetype: usize,
    position: Vec3,
    velocity: Vec3,
    yaw: f32,
    phase: f32,
    proportions: Option<InstanceBindPose>,
    hip_ratio: f32,
    gait: usize,
    weight: f32,
    rate: f32,
    speed: f32,
    tier: usize,
    due: Option<usize>,
    evaluated: bool,
    /// Foot locking runs for this agent this step, and which feet the gait plants.
    lock: Option<[bool; 2]>,
    /// Which feet the gait plants this step (exact tiers with legs; else none).
    planted: [bool; 2],
    feet: [FootLock; 2],
    pose: Pose,
    globals: Vec<Mat4>,
    palette: Vec<Mat4>,
}

/// Integer identity of a pose to share (or an agent's own, for exact tiers).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum KeyId {
    Shared {
        archetype: u32,
        gait: u32,
        weight: u32,
        bucket: u32,
        buckets: u32,
    },
    Exact(u32),
}

/// One unique pose of a step: its archetype, gait, the values it is evaluated at and buffers.
#[derive(Clone, Debug, Default)]
struct PoseSlot {
    archetype: usize,
    gait: usize,
    weight: f32,
    phase: f32,
    pose: Pose,
    scratch: Pose,
}

/// Weight buckets of pose sharing (1/16 steps).
const WEIGHT_BUCKETS: f32 = 16.0;

/// The crowd evaluator (module docs).
#[derive(Clone, Debug, Default)]
pub struct CrowdAnimator {
    settings: CrowdSettings,
    archetypes: Vec<CrowdArchetype>,
    agents: Vec<Agent>,
    viewer: Option<Vec3>,
    step: u64,
    accumulator: f32,
    slots: Vec<PoseSlot>,
    stats: CrowdStats,
    /// Pose-key map kept across steps. Cleared each step. Exact tiers do not insert.
    keys: HashMap<KeyId, usize>,
    /// Per-tier agent counts, reused each step.
    per_tier_buf: Vec<usize>,
}

fn globals_into(skeleton: &Skeleton, pose: &Pose, out: &mut Vec<Mat4>) {
    let n = skeleton.len();
    out.clear();
    out.resize(n, Mat4::IDENTITY);
    for i in 0..n.min(pose.len()) {
        let local =
            Mat4::from_scale_rotation_translation(pose.scales[i], pose.rotations[i], pose.translations[i]);
        out[i] = match skeleton.parents.get(i).copied().flatten() {
            Some(p) if (p as usize) < i => out[p as usize] * local,
            _ => local,
        };
    }
}

/// Foot locking for one agent whose local pose and globals are current: a planted foot is
/// held at the world position where it was planted (two-bone leg IK), re-locked when the
/// animation drifts more than `LOCK_MAX_SLIP` of the hip height away, and faded out over
/// `LOCK_RELEASE_STEPS` once it lifts. Returns whether the pose changed.
fn lock_feet(
    agent: &mut Agent,
    planted: [bool; 2],
    legs: &[IkChain; 2],
    skeleton: &Skeleton,
    hip_height: f32,
) -> bool {
    let world = Mat4::from_rotation_translation(det_rotation_y(agent.yaw), agent.position);
    let to_object = world.inverse();
    let max_slip = LOCK_MAX_SLIP * hip_height * agent.hip_ratio;
    // Planted positions come from the pose globals already written for this step. Copy them
    // before the solves, which refresh the chain inside `agent.globals`.
    let mut animated_at = [Vec3::ZERO; 2];
    for (side, leg) in legs.iter().enumerate() {
        animated_at[side] = agent
            .globals
            .get(leg.tip as usize)
            .map_or(Vec3::NAN, |global| world.transform_point3(position_of(*global)));
    }
    let mut changed = false;
    for (side, leg) in legs.iter().enumerate() {
        let animated = animated_at[side];
        if !animated.is_finite() {
            continue;
        }
        let (target, weight) = {
            let foot = &mut agent.feet[side];
            // A foot planted last step that drifted more than `max_slip` from its pin is pinned
            // again where the animation puts it (the squared test is the crowd's own).
            // `!(slip <= limit)` is true for a greater slip and for an incomparable (non-finite)
            // pin. `partial_cmp` keeps both cases without a negated compare.
            let slip = horizontal(foot.lock.pin() - animated).length_squared();
            let limit = max_slip * max_slip;
            let repin = !matches!(slip.partial_cmp(&limit), Some(Ordering::Less | Ordering::Equal));
            // Steps, not seconds: one step lowers the release weight by `1 / LOCK_RELEASE_STEPS`.
            foot.lock.update(
                planted[side],
                repin,
                animated,
                1.0,
                LOCK_RELEASE_STEPS,
                PlantRelease::Fixed,
            );
            foot.target = foot.lock.pinned().then(|| foot.lock.pin());
            (foot.target, foot.lock.weight())
        };
        if let Some(target) = target {
            solve_two_bone_with_globals(
                skeleton,
                &mut agent.pose,
                leg,
                to_object.transform_point3(target),
                None,
                weight,
                &mut agent.globals,
            );
            changed = true;
        }
    }
    changed
}

fn fnv(hash: &mut u64, bits: u32) {
    for byte in bits.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

impl CrowdAnimator {
    /// An empty crowd with `settings`.
    #[must_use]
    pub fn new(settings: CrowdSettings) -> Self {
        CrowdAnimator {
            settings,
            ..Default::default()
        }
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &CrowdSettings {
        &self.settings
    }

    /// Replace the settings (takes effect at the next step).
    pub fn set_settings(&mut self, settings: CrowdSettings) {
        self.settings = settings;
    }

    /// Add an archetype; returns its index.
    pub fn add_archetype(&mut self, archetype: CrowdArchetype) -> usize {
        self.archetypes.push(archetype);
        self.archetypes.len() - 1
    }

    /// The archetypes.
    #[must_use]
    pub fn archetypes(&self) -> &[CrowdArchetype] {
        &self.archetypes
    }

    /// Add an agent.
    ///
    /// # Errors
    ///
    /// An unknown archetype, proportions over another skeleton, or non-finite values.
    pub fn add_agent(&mut self, desc: CrowdAgentDesc) -> Result<CrowdAgentId, CrowdError> {
        let archetype = self
            .archetypes
            .get(desc.archetype)
            .ok_or(CrowdError::UnknownArchetype(desc.archetype))?;
        if !desc.position.is_finite() {
            return Err(CrowdError::BadValue("position"));
        }
        if !desc.velocity.is_finite() {
            return Err(CrowdError::BadValue("velocity"));
        }
        if !desc.yaw.is_finite() {
            return Err(CrowdError::BadValue("yaw"));
        }
        if !desc.phase.is_finite() {
            return Err(CrowdError::BadValue("phase"));
        }
        let mut hip_ratio = 1.0;
        if let Some(instance) = &desc.proportions {
            // The public fields can disagree: a short inverse-bind list would cut the palette.
            let joints = archetype.skeleton.len();
            let got = [
                instance.len(),
                instance.inverse_bind_matrices.len(),
                instance.length_ratio.len(),
            ]
            .into_iter()
            .find(|&len| len != joints);
            if let Some(got) = got {
                return Err(CrowdError::ProportionsMismatch {
                    got,
                    expected: joints,
                });
            }
            if let Some(h) = archetype.hips {
                let pose = Pose {
                    translations: instance.bind_pose.iter().map(|b| b.translation).collect(),
                    rotations: instance.bind_pose.iter().map(|b| b.rotation).collect(),
                    scales: instance.bind_pose.iter().map(|b| b.scale).collect(),
                };
                let globals = archetype.skeleton.globals(&pose, None);
                if let Some(height) = hip_height(&globals, h)
                    && archetype.hip_height > 1e-4
                {
                    hip_ratio = height / archetype.hip_height;
                }
            }
        }
        let id = CrowdAgentId(self.agents.len() as u32);
        self.agents.push(Agent {
            archetype: desc.archetype,
            position: desc.position,
            velocity: desc.velocity,
            yaw: desc.yaw,
            phase: desc.phase.rem_euclid(1.0),
            proportions: desc.proportions,
            hip_ratio,
            gait: 0,
            weight: 0.0,
            rate: 1.0,
            speed: 0.0,
            tier: 0,
            due: None,
            evaluated: false,
            lock: None,
            planted: [false; 2],
            feet: [FootLock::default(); 2],
            pose: Pose::default(),
            globals: Vec::new(),
            palette: Vec::new(),
        });
        Ok(id)
    }

    /// Number of agents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether the crowd has no agents.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Set an agent's world position and velocity (from navigation or game code). Non-finite
    /// values are ignored. Returns whether the agent exists.
    pub fn set_motion(&mut self, id: CrowdAgentId, position: Vec3, velocity: Vec3) -> bool {
        let Some(agent) = self.agents.get_mut(id.0 as usize) else {
            return false;
        };
        if position.is_finite() {
            agent.position = position;
        }
        if velocity.is_finite() {
            agent.velocity = velocity;
        }
        true
    }

    /// The viewer position for LOD tiers (`None`: every agent in the first tier).
    pub fn set_viewer(&mut self, position: Option<Vec3>) {
        self.viewer = position.filter(|p| p.is_finite());
    }

    /// Run whole fixed steps for `dt` seconds (the remainder carries over); returns the steps
    /// run. At most 64 steps run per call: a longer hitch drops the rest of its time.
    pub fn advance(&mut self, dt: f32) -> u32 {
        let step = self.settings.fixed_step;
        if !(step.is_finite() && step > 0.0) || !dt.is_finite() || dt <= 0.0 {
            return 0;
        }
        self.accumulator += dt;
        let mut steps = 0;
        while self.accumulator >= step && steps < 64 {
            self.accumulator -= step;
            self.step();
            steps += 1;
        }
        if steps == 64 {
            self.accumulator = 0.0;
        }
        steps
    }

    /// One fixed step (module docs).
    pub fn step(&mut self) {
        let _span = tracing::info_span!("crowd.step", agents = self.agents.len()).entered();
        let CrowdAnimator {
            settings,
            archetypes,
            agents,
            viewer,
            step,
            slots,
            stats,
            keys,
            per_tier_buf,
            ..
        } = self;
        let dt = if settings.fixed_step.is_finite() && settings.fixed_step > 0.0 {
            settings.fixed_step
        } else {
            0.0
        };
        let tiers = settings.lods.len().max(1);
        keys.clear();
        per_tier_buf.clear();
        per_tier_buf.resize(tiers, 0);
        let per_tier = per_tier_buf;
        let mut used = 0usize;
        let (mut updated, mut skipped, mut shared_hits) = (0usize, 0usize, 0usize);
        for (index, agent) in agents.iter_mut().enumerate() {
            let archetype = &archetypes[agent.archetype];
            // 1. Phase.
            let ground = horizontal(agent.velocity);
            let v = ground.length();
            let v = if v.is_finite() { v } else { 0.0 };
            let (gait, weight, rate, cycle) = archetype.select(v, agent.hip_ratio, settings);
            agent.gait = gait;
            agent.weight = weight;
            agent.rate = rate;
            agent.speed = v;
            if cycle > 1e-4 {
                agent.phase = (agent.phase + dt * rate / cycle).rem_euclid(1.0);
            }
            if v > 0.05 {
                let f = archetype.forward;
                let d = ground / v;
                agent.yaw = det_atan2(f.cross(d).y, f.dot(d));
            }
            // 2. Tier and due.
            let tier = match *viewer {
                None => 0,
                Some(view) => {
                    let distance = agent.position.distance(view);
                    settings
                        .lods
                        .iter()
                        .position(|l| distance <= l.distance)
                        .unwrap_or(tiers - 1)
                }
            };
            agent.tier = tier;
            per_tier[tier] += 1;
            let lod = settings.lods.get(tier).copied().unwrap_or(CrowdLod {
                distance: f32::INFINITY,
                interval: 1,
                phase_buckets: 0,
            });
            let interval = u64::from(lod.interval.max(1));
            let exact = lod.phase_buckets == 0 && interval == 1;
            agent.planted = if exact && archetype.legs.is_some() {
                archetype.planted(gait, weight, agent.phase)
            } else {
                [false; 2]
            };
            agent.lock = (settings.foot_lock && exact && archetype.legs.is_some()).then_some(agent.planted);
            if agent.lock.is_none() {
                agent.feet = [FootLock::default(); 2];
            }
            if agent.evaluated && !(*step + index as u64).is_multiple_of(interval) {
                agent.due = None;
                skipped += 1;
                continue;
            }
            updated += 1;
            // 3. Pose key.
            let (id, eval_weight, eval_phase) = if lod.phase_buckets == 0 {
                (KeyId::Exact(index as u32), weight, agent.phase)
            } else {
                let buckets = lod.phase_buckets;
                let bucket = ((agent.phase * buckets as f32).round() as u32) % buckets;
                let wq = (weight * WEIGHT_BUCKETS).round() as u32;
                (
                    KeyId::Shared {
                        archetype: agent.archetype as u32,
                        gait: gait as u32,
                        weight: wq,
                        bucket,
                        buckets,
                    },
                    wq as f32 / WEIGHT_BUCKETS,
                    bucket as f32 / buckets as f32,
                )
            };
            // Exact keys are one per agent, so hashing them never deduplicates.
            let (slot, fresh) = if matches!(id, KeyId::Exact(_)) {
                let slot = used;
                used += 1;
                (slot, true)
            } else {
                match keys.get(&id) {
                    Some(&slot) => {
                        shared_hits += 1;
                        (slot, false)
                    }
                    None => {
                        let slot = used;
                        used += 1;
                        keys.insert(id, slot);
                        (slot, true)
                    }
                }
            };
            if fresh {
                if slots.len() < used {
                    slots.push(PoseSlot::default());
                }
                let s = &mut slots[slot];
                s.archetype = agent.archetype;
                s.gait = gait;
                s.weight = eval_weight;
                s.phase = eval_phase;
            }
            agent.due = Some(slot);
        }

        // 4. Evaluate the unique poses (disjoint slots).
        let archetypes_ref: &[CrowdArchetype] = archetypes;
        let evaluate = |chunk: &mut [PoseSlot]| {
            for slot in chunk {
                let archetype = &archetypes_ref[slot.archetype];
                let PoseSlot {
                    gait,
                    weight,
                    phase,
                    pose,
                    scratch,
                    ..
                } = slot;
                archetype.evaluate(*gait, *weight, *phase, pose, scratch);
            }
        };
        let live = &mut slots[..used];
        if settings.chunk == 0 {
            evaluate(live);
        } else {
            regia_core::tasks::par_chunks_mut(live, settings.chunk, |_, chunk| evaluate(chunk));
        }

        // 5. Finish the due agents (disjoint agent slots).
        let slots_ref: &[PoseSlot] = &slots[..used];
        let finish = |chunk: &mut [Agent]| {
            for agent in chunk {
                let Some(slot) = agent.due else { continue };
                let archetype = &archetypes_ref[agent.archetype];
                let local = &slots_ref[slot].pose;
                let skeleton = &archetype.skeleton;
                match &agent.proportions {
                    Some(instance) => retarget_to_instance(skeleton, instance, local, &mut agent.pose),
                    None => agent.pose.clone_from(local),
                }
                globals_into(skeleton, &agent.pose, &mut agent.globals);
                if let (Some(planted), Some(legs)) = (agent.lock, &archetype.legs)
                    && lock_feet(agent, planted, legs, skeleton, archetype.hip_height)
                {
                    globals_into(skeleton, &agent.pose, &mut agent.globals);
                }
                let inverse_binds = match &agent.proportions {
                    Some(instance) => &instance.inverse_bind_matrices,
                    None => &skeleton.inverse_bind_matrices,
                };
                agent.palette.clear();
                agent
                    .palette
                    .extend(agent.globals.iter().zip(inverse_binds).map(|(g, ibm)| *g * *ibm));
                agent.evaluated = true;
            }
        };
        if settings.chunk == 0 {
            finish(agents.as_mut_slice());
        } else {
            regia_core::tasks::par_chunks_mut(agents.as_mut_slice(), settings.chunk, |_, chunk| {
                finish(chunk)
            });
        }

        let locked_feet = agents
            .iter()
            .flat_map(|a| a.feet.iter())
            .filter(|f| f.target.is_some())
            .count();
        *step += 1;
        stats.agents = agents.len();
        stats.archetypes = archetypes.len();
        stats.step = *step;
        stats.evaluated_poses = used;
        stats.shared_hits = shared_hits;
        stats.updated = updated;
        stats.skipped = skipped;
        stats.locked_feet = locked_feet;
        stats.per_tier.clone_from(per_tier);
    }

    /// Capacity of the pose-key map kept between steps.
    ///
    /// Exact tiers leave it at 0. A shared tier's capacity stays flat after the map has
    /// seen that step's distinct keys.
    #[must_use]
    pub fn key_map_capacity(&self) -> usize {
        self.keys.capacity()
    }

    /// Counters of the last step.
    #[must_use]
    pub fn stats(&self) -> &CrowdStats {
        &self.stats
    }

    /// An agent's local pose (after its instance retarget), once evaluated.
    #[must_use]
    pub fn pose(&self, id: CrowdAgentId) -> Option<&Pose> {
        self.agents
            .get(id.0 as usize)
            .filter(|a| a.evaluated)
            .map(|a| &a.pose)
    }

    /// An agent's object-space joint matrices, once evaluated.
    #[must_use]
    pub fn globals(&self, id: CrowdAgentId) -> Option<&[Mat4]> {
        self.agents
            .get(id.0 as usize)
            .filter(|a| a.evaluated)
            .map(|a| a.globals.as_slice())
    }

    /// An agent's skin palette, once evaluated.
    #[must_use]
    pub fn palette(&self, id: CrowdAgentId) -> Option<&[Mat4]> {
        self.agents
            .get(id.0 as usize)
            .filter(|a| a.evaluated)
            .map(|a| a.palette.as_slice())
    }

    /// An agent's world transform (position and yaw).
    #[must_use]
    pub fn world_transform(&self, id: CrowdAgentId) -> Option<Mat4> {
        let agent = self.agents.get(id.0 as usize)?;
        Some(Mat4::from_rotation_translation(
            det_rotation_y(agent.yaw),
            agent.position,
        ))
    }

    /// An agent's playback state.
    #[must_use]
    pub fn agent_state(&self, id: CrowdAgentId) -> Option<CrowdAgentState> {
        let a = self.agents.get(id.0 as usize)?;
        Some(CrowdAgentState {
            archetype: a.archetype,
            tier: a.tier,
            gait: a.gait,
            weight: a.weight,
            phase: a.phase,
            rate: a.rate,
            speed: a.speed,
            yaw: a.yaw,
            position: a.position,
            hip_ratio: a.hip_ratio,
            feet_locked: a.feet.map(|f| f.target.is_some()),
            feet_planted: a.planted,
        })
    }

    /// World position of joint `joint` of an agent (its world transform times the joint's
    /// object-space position), once evaluated.
    #[must_use]
    pub fn joint_world_position(&self, id: CrowdAgentId, joint: usize) -> Option<Vec3> {
        let world = self.world_transform(id)?;
        let global = self.globals(id)?.get(joint)?;
        Some(world.transform_point3(position_of(*global)))
    }

    /// FNV-1a over every agent's phase, yaw and palette bits, in id order. Equal digests mean
    /// bit-identical crowd output.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for agent in &self.agents {
            fnv(&mut hash, agent.phase.to_bits());
            fnv(&mut hash, agent.yaw.to_bits());
            for m in &agent.palette {
                for v in m.to_cols_array() {
                    fnv(&mut hash, v.to_bits());
                }
            }
        }
        hash
    }
}

/// The ECS crowd: a [`CrowdAnimator`] as a resource.
#[derive(Resource, Debug, Default)]
pub struct CrowdAnimation(pub CrowdAnimator);

/// A rig entity driven by a crowd agent: [`CrowdAnimationPlugin`] writes its [`SkinPalette`] and
/// [`JointGlobals`] (and its `Transform` when `drive_transform`). The rig needs a
/// [`SkinnedMesh`] and no `Animator`.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct CrowdMember {
    /// The crowd agent.
    pub agent: CrowdAgentId,
    /// Write the agent's position and yaw into the rig's `Transform`.
    pub drive_transform: bool,
}

/// Steps the [`CrowdAnimation`] resource once per `FixedUpdate` tick (its fixed step follows
/// the app's) and publishes member palettes in `PostUpdate` (`AnimationApply`).
#[derive(Default)]
pub struct CrowdAnimationPlugin;

impl Plugin for CrowdAnimationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CrowdAnimation>()
            .add_systems(FixedUpdate, step_crowd_animation)
            .add_systems(
                PostUpdate,
                // Before the sockets, which read the joint globals and rig transforms it writes.
                apply_crowd_animation
                    .before(crate::systems::resolve_sockets)
                    .in_set(PostUpdateSet::AnimationApply),
            );
    }
}

/// One crowd step per `FixedUpdate` tick.
pub fn step_crowd_animation(mut crowd: ResMut<CrowdAnimation>, fixed: Res<regia_app::FixedTime>) {
    let step = fixed.step().as_secs_f32();
    if step.is_finite() && step > 0.0 && (crowd.0.settings.fixed_step - step).abs() > f32::EPSILON {
        crowd.0.settings.fixed_step = step;
    }
    crowd.0.step();
}

/// The data `apply_crowd_animation` writes on a member rig.
type CrowdMemberRig = (
    Entity,
    &'static CrowdMember,
    Option<&'static mut SkinPalette>,
    Option<&'static mut JointGlobals>,
    Option<&'static mut Transform>,
);

/// Publish each member's palette, joint globals and (optionally) transform.
#[tracing::instrument(skip_all)]
pub fn apply_crowd_animation(
    mut commands: Commands,
    crowd: Res<CrowdAnimation>,
    mut members: Query<CrowdMemberRig, With<SkinnedMesh>>,
) {
    let _span = tracing::debug_span!("apply_crowd_animation").entered();
    for (entity, member, palette, globals, transform) in &mut members {
        let (Some(p), Some(g)) = (crowd.0.palette(member.agent), crowd.0.globals(member.agent)) else {
            continue;
        };
        match palette {
            Some(mut palette) => {
                palette.matrices.clear();
                palette.matrices.extend_from_slice(p);
            }
            None => {
                commands
                    .entity(entity)
                    .insert(SkinPalette { matrices: p.to_vec() });
            }
        }
        match globals {
            Some(mut globals) => {
                globals.globals.clear();
                globals.globals.extend_from_slice(g);
            }
            None => {
                commands
                    .entity(entity)
                    .insert(JointGlobals { globals: g.to_vec() });
            }
        }
        if member.drive_transform
            && let (Some(mut transform), Some(state)) = (transform, crowd.0.agent_state(member.agent))
        {
            transform.translation = state.position;
            transform.rotation = det_rotation_y(state.yaw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(planted: impl Fn(usize) -> bool) -> [bool; CONTACT_SAMPLES] {
        std::array::from_fn(planted)
    }

    #[test]
    fn contact_windows_take_the_longest_wrapping_run_and_blend_on_the_short_way() {
        assert_eq!(ContactWindow::from_table(&row(|_| true)), ContactWindow::Always);
        let never = ContactWindow::from_table(&row(|_| false));
        assert!(!never.contains(0.0) && !never.contains(0.5));
        // Planted over samples 56..64 and 0..8: one run of 16 wrapping through phase 0.
        let wrap = ContactWindow::from_table(&row(|k| !(8..56).contains(&k)));
        assert_eq!(
            wrap,
            ContactWindow::Span {
                start: 56.0 / 64.0,
                length: 0.25
            }
        );
        assert!(wrap.contains(0.9) && wrap.contains(0.05) && !wrap.contains(0.2));
        // Touch-down at 0.9 and 0.1 blend halfway to 0.0 (not 0.5), lengths average.
        let a = ContactWindow::Span {
            start: 0.9,
            length: 0.5,
        };
        let b = ContactWindow::Span {
            start: 0.1,
            length: 0.3,
        };
        let ContactWindow::Span { start, length } = a.blend(b, 0.5) else {
            panic!("a span")
        };
        assert!(
            start.min(1.0 - start) < 1e-5 && (length - 0.4).abs() < 1e-5,
            "{start} {length}"
        );
        // An idle follows the moving gait whatever the weight.
        assert_eq!(ContactWindow::Always.blend(b, 0.1), b);
        assert_eq!(b.blend(ContactWindow::Always, 0.9), b);
    }

    /// One foot (height, position along the walk) at 48 even phases of the Khronos CesiumMan walk
    /// (CC BY 4.0) played in place on a 1 m hip rig: 2 s cycle, 1.15 m/s, stance from about
    /// sample 20 to 35. The ankle dips twice in stance and its speed swings from 0.5 to 1.7 m/s.
    const CAPTURED_FOOT: [(f32, f32); 48] = [
        (0.346, 0.624),
        (0.350, 0.619),
        (0.378, 0.586),
        (0.424, 0.527),
        (0.485, 0.449),
        (0.536, 0.366),
        (0.565, 0.294),
        (0.572, 0.228),
        (0.567, 0.162),
        (0.550, 0.095),
        (0.525, 0.030),
        (0.492, -0.033),
        (0.448, -0.094),
        (0.366, -0.156),
        (0.276, -0.227),
        (0.201, -0.297),
        (0.154, -0.353),
        (0.137, -0.381),
        (0.131, -0.390),
        (0.124, -0.385),
        (0.113, -0.365),
        (0.104, -0.328),
        (0.104, -0.282),
        (0.110, -0.232),
        (0.113, -0.184),
        (0.100, -0.133),
        (0.086, -0.073),
        (0.079, -0.006),
        (0.083, 0.064),
        (0.099, 0.127),
        (0.118, 0.180),
        (0.136, 0.220),
        (0.152, 0.251),
        (0.166, 0.275),
        (0.180, 0.298),
        (0.195, 0.323),
        (0.210, 0.353),
        (0.223, 0.394),
        (0.237, 0.440),
        (0.253, 0.486),
        (0.269, 0.531),
        (0.285, 0.567),
        (0.300, 0.594),
        (0.312, 0.612),
        (0.321, 0.625),
        (0.327, 0.631),
        (0.332, 0.634),
        (0.334, 0.634),
    ];

    /// The other foot of the same bake and samples. It is not `CAPTURED_FOOT` half a cycle away:
    /// stance from about sample 42 to 8, a peak of 1.7 times the gait speed at samples 3 to 5
    /// (the foot lags the ground), and samples 0 and 1 stopped by the clip's first key.
    const CAPTURED_RIGHT_FOOT: [(f32, f32); 48] = [
        (0.099, -0.202),
        (0.097, -0.194),
        (0.084, -0.145),
        (0.077, -0.069),
        (0.084, 0.023),
        (0.102, 0.113),
        (0.123, 0.189),
        (0.140, 0.255),
        (0.159, 318.0 / 1000.0), // As a quotient: the literal 0.318 reads as 1/π to clippy.
        (0.178, 0.379),
        (0.201, 0.435),
        (0.226, 0.485),
        (0.251, 0.529),
        (0.275, 0.566),
        (0.298, 0.600),
        (0.321, 0.629),
        (0.343, 0.654),
        (0.361, 0.672),
        (0.376, 0.687),
        (0.387, 0.698),
        (0.394, 0.709),
        (0.395, 0.714),
        (0.394, 0.714),
        (0.390, 0.708),
        (0.385, 0.692),
        (0.387, 0.657),
        (0.402, 0.601),
        (0.434, 0.524),
        (0.480, 0.432),
        (0.522, 0.342),
        (0.550, 0.266),
        (0.560, 0.197),
        (0.556, 0.129),
        (0.537, 0.057),
        (0.513, -0.017),
        (0.485, -0.092),
        (0.451, -0.163),
        (0.384, -0.230),
        (0.307, -0.298),
        (0.239, -0.357),
        (0.191, -0.398),
        (0.167, -0.414),
        (0.153, -0.415),
        (0.138, -0.402),
        (0.118, -0.371),
        (0.101, -0.324),
        (0.097, -0.278),
        (0.101, -0.240),
    ];

    /// Captured motion is not flat like authored motion: the stance must still be one window.
    #[test]
    fn a_captured_stance_makes_one_contact_window() {
        let sample = |path: &[(f32, f32); 48], phase: f32| {
            let at = phase.rem_euclid(1.0) * 48.0;
            let (k, u) = (at as usize % 48, at.fract());
            let (a, b) = (path[k], path[(k + 1) % 48]);
            Vec3::new(0.0, a.0 + (b.0 - a.0) * u, a.1 + (b.1 - a.1) * u)
        };
        let samples: [[Vec3; 2]; CONTACT_SAMPLES] = std::array::from_fn(|k| {
            let phase = k as f32 / CONTACT_SAMPLES as f32;
            [sample(&CAPTURED_FOOT, phase), sample(&CAPTURED_RIGHT_FOOT, phase)]
        });
        let gait = CrowdGait {
            clip: Arc::new(AnimationClip::empty("walk", 2.0)),
            speed: 1.15,
            cycle: 2.0,
            velocity: Vec3::NEG_Z * 1.15,
            pin: None,
        };
        let [left, right] = contact_table(&samples, &gait, 1.0).map(|row| ContactWindow::from_table(&row));
        let ContactWindow::Span { start, length } = left else {
            panic!("{left:?}")
        };
        assert!((0.25..=0.45).contains(&length), "stance {length}");
        assert!((0.39..=0.5).contains(&start), "touch-down {start}");
        assert!(left.contains(0.5) && !left.contains(0.1), "{left:?}");
        // The right foot's stance is measured too, not assumed from the left one.
        assert!((0.25..=0.45).contains(&right.share()), "{right:?}");
        assert!(right.contains(0.0) && right.contains(0.1), "{right:?}");
        assert!(!right.contains(0.5) && !right.contains(0.6), "{right:?}");
    }

    /// One foot on a flat 1 m/s treadmill walk (forward is −Z, so a stance foot moves +Z), with
    /// `stance` samples on the ground and a fast swing back through the air.
    fn treadmill_foot(stance: usize) -> [[Vec3; 2]; CONTACT_SAMPLES] {
        let dt = 1.0 / CONTACT_SAMPLES as f32;
        let (stride, swing) = (stance as f32 * dt, (CONTACT_SAMPLES - stance) as f32);
        std::array::from_fn(|k| {
            let foot = if k < stance {
                Vec3::new(0.0, 0.0, k as f32 * dt)
            } else {
                let u = (k - stance) as f32 / swing;
                Vec3::new(0.0, 0.3, stride * (1.0 - u))
            };
            [foot, Vec3::ZERO]
        })
    }

    fn treadmill_gait() -> CrowdGait {
        CrowdGait {
            clip: Arc::new(AnimationClip::empty("walk", 1.0)),
            speed: 1.0,
            cycle: 1.0,
            velocity: Vec3::NEG_Z,
            pin: None,
        }
    }

    #[test]
    fn a_foot_lagging_the_ground_is_planted_and_one_moving_forward_is_not() {
        let gait = treadmill_gait();
        // Sixteen samples of the stance run backward at 1.8 m/s, 0.8 m/s behind the ground.
        let mut lagging = treadmill_foot(32);
        for (k, sample) in lagging.iter_mut().enumerate().take(32).skip(8) {
            let extra = (k.min(24) - 8) as f32 * 0.8 / CONTACT_SAMPLES as f32;
            sample[0].z += extra;
        }
        let row = contact_table(&lagging, &gait, 1.0)[0];
        assert!((8..24).all(|k| row[k]), "a lag is stance: {row:?}");

        // The same stretch moving forward, 1.8 m/s over the ground, is no stance.
        let mut forward = treadmill_foot(32);
        for (k, sample) in forward.iter_mut().enumerate().take(32).skip(8) {
            sample[0].z =
                8.0 / CONTACT_SAMPLES as f32 - (k.min(24) - 8) as f32 * 0.8 / CONTACT_SAMPLES as f32;
        }
        let row = contact_table(&forward, &gait, 1.0)[0];
        assert!((10..22).all(|k| !row[k]), "forward is a swing: {row:?}");
    }

    #[test]
    fn short_gaps_close_only_while_the_foot_stays_low() {
        let low = [true; CONTACT_SAMPLES];
        let planted = |gap: std::ops::Range<usize>| {
            let mut row = [false; CONTACT_SAMPLES];
            row[..32].fill(true);
            row[gap].fill(false);
            row
        };
        let mut row = planted(10..12);
        close_contact_gaps(&mut row, &low);
        assert_eq!(row, planted(0..0), "a stall of two samples closes");

        // The gap wraps round the end of the table.
        let mut row = [true; CONTACT_SAMPLES];
        (row[0], row[CONTACT_SAMPLES - 1]) = (false, false);
        close_contact_gaps(&mut row, &low);
        assert_eq!(row, [true; CONTACT_SAMPLES], "a gap across the wrap closes");

        let mut row = planted(10..13);
        close_contact_gaps(&mut row, &low);
        assert_eq!(row, planted(10..13), "three samples is a real gap");

        let mut lifted = low;
        lifted[11] = false;
        let mut row = planted(10..12);
        close_contact_gaps(&mut row, &lifted);
        assert_eq!(row, planted(10..12), "a foot that left the ground stays lifted");

        let mut none = [false; CONTACT_SAMPLES];
        close_contact_gaps(&mut none, &low);
        assert_eq!(none, [false; CONTACT_SAMPLES]);
    }

    /// Unordered or NaN rate limits must not reach `clamp` (which panics on them).
    #[test]
    fn unordered_rate_limits_fall_back_to_unit_rate() {
        let skeleton = Arc::new(Skeleton::default());
        let archetype = CrowdArchetype {
            name: "t".into(),
            skeleton,
            gaits: vec![treadmill_gait(), treadmill_gait()],
            bind: Pose::from_identity(0),
            hips: None,
            hip_height: 1.0,
            forward: Vec3::NEG_Z,
            legs: None,
            contacts: vec![[ContactWindow::Always; 2]; 2],
        };
        for (lo, hi) in [(2.0, 1.0), (f32::NAN, 1.6), (0.6, f32::NAN)] {
            let settings = CrowdSettings {
                min_rate: lo,
                max_rate: hi,
                ..CrowdSettings::default()
            };
            let (_, _, rate, _) = archetype.select(1.0, 1.0, &settings);
            assert_eq!(rate, 1.0, "{lo} {hi}");
        }
    }

    #[test]
    fn foot_lock_uses_existing_globals() {
        use crate::ik::solve_two_bone;
        use crate::pose::translated;
        use crate::skeleton::LocalBind;

        let names: Vec<String> = ["root", "hip_l", "knee_l", "ankle_l", "hip_r", "knee_r", "ankle_r"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let skeleton = Skeleton::from_local_bind(
            names,
            vec![None, Some(0), Some(1), Some(2), Some(0), Some(4), Some(5)],
            vec![
                LocalBind::default(),
                translated(Vec3::new(-0.1, 0.9, 0.0)),
                translated(Vec3::new(0.0, -0.45, 0.0)),
                translated(Vec3::new(0.0, -0.45, 0.0)),
                translated(Vec3::new(0.1, 0.9, 0.0)),
                translated(Vec3::new(0.0, -0.45, 0.05)),
                translated(Vec3::new(0.0, -0.45, 0.0)),
            ],
        )
        .expect("skeleton");
        let legs = [
            IkChain {
                root: 1,
                mid: 2,
                tip: 3,
            },
            IkChain {
                root: 4,
                mid: 5,
                tip: 6,
            },
        ];
        let mut pose = skeleton.bind_pose_pose();
        pose.rotations[2] = regia_core::math::Quat::from_rotation_x(0.4);
        pose.rotations[5] = regia_core::math::Quat::from_rotation_x(-0.3);
        let mut agent = Agent {
            archetype: 0,
            position: Vec3::new(1.0, 0.0, 2.0),
            velocity: Vec3::ZERO,
            yaw: 0.4,
            phase: 0.0,
            proportions: None,
            hip_ratio: 1.0,
            gait: 0,
            weight: 1.0,
            rate: 1.0,
            speed: 0.0,
            tier: 0,
            due: None,
            evaluated: true,
            lock: Some([true, true]),
            planted: [true, true],
            feet: [FootLock::default(); 2],
            pose: pose.clone(),
            globals: Vec::new(),
            palette: Vec::new(),
        };
        globals_into(&skeleton, &agent.pose, &mut agent.globals);
        let cap = agent.globals.capacity();
        assert!(cap >= skeleton.len());
        let world = Mat4::from_rotation_translation(det_rotation_y(agent.yaw), agent.position);
        let to_object = world.inverse();
        let animated =
            [0, 1].map(|side| world.transform_point3(position_of(agent.globals[legs[side].tip as usize])));
        assert!(lock_feet(&mut agent, [true, true], &legs, &skeleton, 1.0));
        assert_eq!(agent.globals.capacity(), cap);
        assert_eq!(agent.feet[0].target, Some(animated[0]));
        assert_eq!(agent.feet[1].target, Some(animated[1]));
        let mut oracle = pose;
        solve_two_bone(
            &skeleton,
            &mut oracle,
            &legs[0],
            to_object.transform_point3(animated[0]),
            None,
            1.0,
        );
        solve_two_bone(
            &skeleton,
            &mut oracle,
            &legs[1],
            to_object.transform_point3(animated[1]),
            None,
            1.0,
        );
        assert_eq!(agent.pose.rotations, oracle.rotations);
        lock_feet(&mut agent, [true, true], &legs, &skeleton, 1.0);
        assert_eq!(agent.globals.capacity(), cap);
    }
}
