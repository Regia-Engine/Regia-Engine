// SPDX-License-Identifier: MPL-2.0
//! Stride and orientation warping shared by motion matching and the animation graph
//! (`docs/specs/52-motion-matching.md` § 2.10, `docs/specs/17-animation.md` § 3).
//!
//! - **Stride warping** ([`StrideWarp`], [`apply_stride_pose`]): the ratio of the character's
//!   actual ground speed to the clip's speed, clamped and smoothed, either speeds playback up
//!   or down ([`StrideMode::Rate`]) or lengthens and shortens the stride with leg IK
//!   ([`StrideMode::Pose`]).
//! - **Orientation warping** ([`orientation_delta`], [`apply_orientation_warp`]): the lower body
//!   turns toward the desired movement heading while a spine chain turns the upper body back.
//!
//! Crate-private: the public path of every item is [`crate::motion_matching::warp`]. The
//! motion-matching player and the graph's `StrideWarp` and `OrientationWarp` nodes (format v4)
//! both use them. The graph nodes call the scratch variants ([`apply_stride_pose_scratch`],
//! [`apply_orientation_warp_with_globals`]) so a warm rig allocates nothing per frame; the
//! math is the same code, so results are bit-identical to the allocating functions.

// Source: A. Witkin and Z. Popovic, "Motion Warping", SIGGRAPH 1995 (retiming and offsetting an
// authored motion to meet a constraint); M. Gleicher, "Motion path editing", I3D 2001 (moving a
// motion along a new path, arc-length parametrization of the root path). The ratio smoothing
// is the standard closed-form solution of a critically damped spring. Original code; nothing
// ported.

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::{Reflect, ReflectDefault, bevy_reflect};
use serde::{Deserialize, Serialize};

use crate::foot_ik::FootIkLeg;
use crate::ik::{refresh_global, solve_two_bone_with_globals};
use crate::num::{det_exp, det_rotation_y};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Why warp settings or inputs were refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WarpError {
    /// A strength is not finite or outside `[0, 1]`.
    #[error("strength {0} must be finite and within [0, 1]")]
    BadStrength(f32),
    /// A speed threshold is negative or not finite.
    #[error("speed threshold {0} must be finite and not negative")]
    BadSpeed(f32),
    /// The stride ratio range is empty, not positive or not finite.
    #[error("stride ratio range {min}..{max} needs finite 0 < min <= 1 <= max")]
    BadRatioRange {
        /// The rejected lower limit.
        min: f32,
        /// The rejected upper limit.
        max: f32,
    },
    /// The spring response is not a positive finite number.
    #[error("warp response {0} must be finite and above zero")]
    BadResponse(f32),
    /// The orientation limit is negative, not finite or above half a turn.
    #[error("max delta {0} must be finite and within [0, pi]")]
    BadMaxDelta(f32),
    /// The spine joint and weight lists differ in length.
    #[error("{joints} spine joints but {weights} spine weights")]
    SpineLengthMismatch {
        /// Spine joints given.
        joints: usize,
        /// Spine weights given.
        weights: usize,
    },
    /// A spine weight is negative or not finite.
    #[error("spine weight {index} is {value}; it must be finite and not negative")]
    BadSpineWeight {
        /// Index in the weight list.
        index: usize,
        /// The rejected weight.
        value: f32,
    },
    /// The spine weights do not sum to 1.
    #[error("spine weights sum to {0}; they must sum to 1")]
    SpineWeightSum(f32),
    /// A joint is not in the skeleton.
    #[error("joint {joint} is not in the skeleton ({joints} joints)")]
    JointOutOfRange {
        /// The joint id.
        joint: u32,
        /// The skeleton's joint count.
        joints: usize,
    },
}

fn check_strength(strength: f32) -> Result<(), WarpError> {
    if strength.is_finite() && (0.0..=1.0).contains(&strength) {
        Ok(())
    } else {
        Err(WarpError::BadStrength(strength))
    }
}

/// Where a stride warp puts its ratio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect, Serialize, Deserialize)]
#[reflect(Default, PartialEq)]
pub enum StrideMode {
    /// Playback speeds up or slows down by the ratio; the pose is untouched (no IK).
    Rate,
    /// Playback keeps its rate; each foot moves along the movement direction so the stride is
    /// the ratio times longer, and leg IK solves the legs ([`apply_stride_pose`]). Needs leg
    /// chains: a player without them should use [`StrideMode::Rate`].
    #[default]
    Pose,
}

/// Settings of a stride warp. A graph `StrideWarp` node (`.animgraph.ron` v4) stores them
/// with every field written out; a node without `settings` uses
/// [`StrideWarpSettings::graph_default`].
#[derive(Clone, Copy, Debug, PartialEq, Reflect, Serialize, Deserialize)]
#[reflect(Default, PartialEq)]
pub struct StrideWarpSettings {
    /// How much of the correction is applied, `[0, 1]`; 0 (the default) turns the warp off.
    pub strength: f32,
    /// Below this ground speed (actual or clip, m/s) the warp is off. Default 0.1.
    pub min_speed: f32,
    /// Smallest ratio, default 0.8.
    pub ratio_min: f32,
    /// Largest ratio, default 1.25.
    pub ratio_max: f32,
    /// Stiffness of the critically damped spring that smooths the ratio, per second
    /// (default 12).
    pub response: f32,
    /// Where the ratio goes.
    pub mode: StrideMode,
}

impl Default for StrideWarpSettings {
    fn default() -> Self {
        Self {
            strength: 0.0,
            min_speed: 0.1,
            ratio_min: 0.8,
            ratio_max: 1.25,
            response: 12.0,
            mode: StrideMode::default(),
        }
    }
}

impl StrideWarpSettings {
    /// The settings of a graph `StrideWarp` node that names none: [`Default`] at full strength
    /// (a node that is in the graph is meant to warp).
    #[must_use]
    pub fn graph_default() -> Self {
        Self {
            strength: 1.0,
            ..Self::default()
        }
    }

    /// Check the settings.
    ///
    /// # Errors
    ///
    /// [`WarpError::BadStrength`], [`WarpError::BadSpeed`], [`WarpError::BadRatioRange`] or
    /// [`WarpError::BadResponse`].
    pub fn validate(&self) -> Result<(), WarpError> {
        check_strength(self.strength)?;
        if !self.min_speed.is_finite() || self.min_speed < 0.0 {
            return Err(WarpError::BadSpeed(self.min_speed));
        }
        let (min, max) = (self.ratio_min, self.ratio_max);
        if !(min.is_finite() && max.is_finite() && min > 0.0 && min <= 1.0 && max >= 1.0) {
            return Err(WarpError::BadRatioRange { min, max });
        }
        if !self.response.is_finite() || self.response <= 0.0 {
            return Err(WarpError::BadResponse(self.response));
        }
        Ok(())
    }

    /// The ratio the spring moves toward for these speeds (1 when the warp is off or the
    /// settings are invalid).
    pub fn target_ratio(&self, actual_speed: f32, clip_speed: f32) -> f32 {
        if self.validate().is_err() || self.strength <= 0.0 {
            return 1.0;
        }
        let usable = |s: f32| s.is_finite() && s >= self.min_speed && s > 0.0;
        if !usable(actual_speed) || !usable(clip_speed) {
            return 1.0;
        }
        let ratio = (actual_speed / clip_speed).clamp(self.ratio_min, self.ratio_max);
        1.0 + self.strength * (ratio - 1.0)
    }
}

/// The running state of a stride warp: the smoothed ratio and its rate of change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StrideWarp {
    ratio: f32,
    velocity: f32,
}

impl Default for StrideWarp {
    fn default() -> Self {
        Self {
            ratio: 1.0,
            velocity: 0.0,
        }
    }
}

impl StrideWarp {
    /// Move the ratio toward [`StrideWarpSettings::target_ratio`] for `dt` seconds and return
    /// it. The spring is the exact solution of a critically damped spring of stiffness
    /// `response` over the step (`x(t) = g + (y₀ + (v₀ + ω y₀) t) e^{−ωt}`), so it never
    /// overshoots a constant target from rest and the result only depends on the steps given.
    /// A non-finite or negative `dt` counts as zero.
    pub fn update(
        &mut self,
        settings: &StrideWarpSettings,
        actual_speed: f32,
        clip_speed: f32,
        dt: f32,
    ) -> f32 {
        let target = settings.target_ratio(actual_speed, clip_speed);
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let omega = if settings.response.is_finite() && settings.response > 0.0 {
            settings.response
        } else {
            StrideWarpSettings::default().response
        };
        let y = self.ratio - target;
        let j = self.velocity + omega * y;
        let decay = det_exp(-omega * dt);
        self.ratio = target + (y + j * dt) * decay;
        self.velocity = (self.velocity - omega * j * dt) * decay;
        if !(self.ratio.is_finite() && self.velocity.is_finite()) {
            *self = Self::default();
        }
        self.ratio
    }

    /// The current smoothed ratio (1 = no warp).
    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    /// The playback-rate multiplier for `mode`: the ratio for [`StrideMode::Rate`], 1 for
    /// [`StrideMode::Pose`] (the stride carries the ratio there).
    pub fn rate_multiplier(&self, mode: StrideMode) -> f32 {
        match mode {
            StrideMode::Rate => self.ratio,
            StrideMode::Pose => 1.0,
        }
    }

    /// The ratio [`apply_stride_pose`] should use for `mode`: 1 for [`StrideMode::Rate`].
    pub fn stride_ratio(&self, mode: StrideMode) -> f32 {
        match mode {
            StrideMode::Rate => 1.0,
            StrideMode::Pose => self.ratio,
        }
    }

    /// Back to ratio 1 at rest.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Smallest hip drop that brings a target within reach, given the quadratic discriminant
/// (`r_y² − excess`). A negative discriminant means the target is horizontally out of reach at
/// every hip height, so lowering the hips cannot help and the drop is zero.
pub(crate) fn hip_drop(r_y: f32, disc: f32) -> f32 {
    if disc >= 0.0 {
        (-r_y - disc.sqrt()).max(0.0)
    } else {
        0.0
    }
}

/// Lengthen or shorten the stride of `pose` by `ratio` (object space).
///
/// Each foot's offset from `hips` along the ground direction `movement_dir`, limited to
/// `±stride_length`, is scaled by `ratio`: the foot target moves by `(ratio − 1)` times that
/// offset, keeping its height. When a target is out of a leg's reach the `hips` joint is lowered
/// (never raised) just enough for every leg to reach it, then each leg is solved with the
/// two-bone IK ([`crate::ik::solve_two_bone`]), bending the knee toward the movement direction. Returns the
/// pelvis drop applied, meters (0 or more).
///
/// Assumptions: every leg chain is a descendant of `hips`, chains have unit scale (as the IK
/// solver assumes), and object-space `+Y` is up. A ratio within `1e-6` of 1, a non-finite
/// input, a vertical or zero `movement_dir` or an out-of-range `hips` leaves the pose unchanged.
#[tracing::instrument(target = "regia_animation", name = "motion_matching.warp", skip_all)]
pub fn apply_stride_pose(
    skeleton: &Skeleton,
    pose: &mut Pose,
    legs: &[FootIkLeg],
    hips: u32,
    movement_dir: Vec3,
    ratio: f32,
    stride_length: f32,
) -> f32 {
    let mut globals = Vec::new();
    apply_stride_pose_with_globals(
        skeleton,
        pose,
        legs,
        StrideRequest {
            hips,
            movement_dir,
            ratio,
            stride_length,
        },
        &mut globals,
    )
}

/// Hips, ground direction, length ratio and clamp for one stride-IK correction (the
/// arguments of [`apply_stride_pose`] after the legs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StrideRequest {
    /// Hips joint the legs descend from.
    pub hips: u32,
    /// Object-space movement direction. The vertical part is ignored.
    pub movement_dir: Vec3,
    /// Stride length scale. 1 leaves the pose unchanged.
    pub ratio: f32,
    /// Maximum foot offset from the hips along the ground direction, in meters.
    pub stride_length: f32,
}

/// [`apply_stride_pose`] writing chain matrices into `globals` (resized once, then reused).
pub(crate) fn apply_stride_pose_with_globals(
    skeleton: &Skeleton,
    pose: &mut Pose,
    legs: &[FootIkLeg],
    request: StrideRequest,
    globals: &mut Vec<Mat4>,
) -> f32 {
    let mut targets = Vec::new();
    stride_pose_core(skeleton, pose, legs, request, globals, &mut targets)
}

/// Buffers [`apply_stride_pose_scratch`] keeps between frames: a warm scratch allocates
/// nothing.
#[derive(Clone, Debug, Default)]
pub struct StrideWarpScratch {
    globals: Vec<Mat4>,
    targets: Vec<Option<(Vec3, Vec3)>>,
}

/// [`apply_stride_pose`] on kept buffers (the graph's `StrideWarp` node). Same math and the
/// same result bits; nothing is allocated once `scratch` has grown to the rig.
pub fn apply_stride_pose_scratch(
    skeleton: &Skeleton,
    pose: &mut Pose,
    legs: &[FootIkLeg],
    request: StrideRequest,
    scratch: &mut StrideWarpScratch,
) -> f32 {
    stride_pose_core(
        skeleton,
        pose,
        legs,
        request,
        &mut scratch.globals,
        &mut scratch.targets,
    )
}

/// The stride correction; `targets` is cleared and refilled (one entry per leg).
fn stride_pose_core(
    skeleton: &Skeleton,
    pose: &mut Pose,
    legs: &[FootIkLeg],
    request: StrideRequest,
    globals: &mut Vec<Mat4>,
    targets: &mut Vec<Option<(Vec3, Vec3)>>,
) -> f32 {
    let StrideRequest {
        hips,
        movement_dir,
        ratio,
        stride_length,
    } = request;
    let joints = skeleton.len().min(pose.len());
    let ground = Vec3::new(movement_dir.x, 0.0, movement_dir.z);
    if !ratio.is_finite()
        || (ratio - 1.0).abs() < 1e-6
        || !stride_length.is_finite()
        || !ground.is_finite()
        || ground.length_squared() < 1e-12
        || hips as usize >= joints
    {
        return 0.0;
    }
    let dir = ground.normalize();
    let limit = stride_length.abs();
    refresh_global(skeleton, pose, hips as usize, globals);
    let hips_at = globals[hips as usize].w_axis.truncate();

    targets.clear();
    let mut drop = 0.0f32;
    for leg in legs {
        let chain = leg.chain;
        if [chain.root, chain.mid, chain.tip]
            .iter()
            .any(|&j| j as usize >= joints)
        {
            targets.push(None);
            continue;
        }
        refresh_global(skeleton, pose, chain.tip as usize, globals);
        let position = |joint: u32| globals[joint as usize].w_axis.truncate();
        let (a, b, c) = (position(chain.root), position(chain.mid), position(chain.tip));
        let offset = (c - hips_at).dot(dir).clamp(-limit, limit);
        let target = c + dir * (offset * (ratio - 1.0));
        // Lowering the hips by `dy` moves the leg root to `a − Y·dy`; the smallest `dy` with
        // `|target − a + Y·dy| = reach` solves `dy² + 2·r_y·dy + |r|² − reach² = 0`.
        let reach = ((b - a).length() + (c - b).length()) * (1.0 - 1e-3);
        let r = target - a;
        let excess = r.length_squared() - reach * reach;
        if excess > 0.0 {
            let disc = r.y * r.y - excess;
            drop = drop.max(hip_drop(r.y, disc));
        }
        targets.push(Some((target, a)));
    }

    if drop > 0.0 {
        let parent_rotation = skeleton
            .parent_of(hips as usize)
            .filter(|&p| p < joints)
            .map_or(Quat::IDENTITY, |p| rotation_of(globals[p]));
        pose.translations[hips as usize] += parent_rotation.inverse() * (Vec3::NEG_Y * drop);
    }
    for (leg, target) in legs.iter().zip(targets.iter()) {
        if let Some((target, root)) = *target {
            let pole = root - Vec3::Y * drop + dir;
            solve_two_bone_with_globals(skeleton, pose, &leg.chain, target, Some(pole), 1.0, globals);
        }
    }
    drop
}

/// Settings of an orientation warp.
#[derive(Clone, Debug, PartialEq, Reflect)]
#[reflect(Default, PartialEq)]
pub struct OrientationWarpSettings {
    /// How much of the correction is applied, `[0, 1]`; 0 (the default) turns the warp off.
    pub strength: f32,
    /// Largest turn, radians (default 90 degrees); 0 disables the warp. A larger delta is
    /// clipped to this, not wrapped.
    pub max_delta: f32,
    /// The spine chain that turns the upper body back, from low to high.
    pub spine_joints: Vec<u32>,
    /// One weight per spine joint, summing to 1, the share of the counter-rotation each takes.
    pub spine_weights: Vec<f32>,
}

impl Default for OrientationWarpSettings {
    fn default() -> Self {
        Self {
            strength: 0.0,
            max_delta: FRAC_PI_2,
            spine_joints: Vec::new(),
            spine_weights: Vec::new(),
        }
    }
}

impl OrientationWarpSettings {
    /// Check the settings.
    ///
    /// # Errors
    ///
    /// [`WarpError::BadStrength`], [`WarpError::BadMaxDelta`],
    /// [`WarpError::SpineLengthMismatch`], [`WarpError::BadSpineWeight`] or
    /// [`WarpError::SpineWeightSum`] (a non-empty list must sum to 1 within `1e-3`).
    pub fn validate(&self) -> Result<(), WarpError> {
        check_strength(self.strength)?;
        if !self.max_delta.is_finite() || !(0.0..=PI).contains(&self.max_delta) {
            return Err(WarpError::BadMaxDelta(self.max_delta));
        }
        if self.spine_joints.len() != self.spine_weights.len() {
            return Err(WarpError::SpineLengthMismatch {
                joints: self.spine_joints.len(),
                weights: self.spine_weights.len(),
            });
        }
        if let Some((index, &value)) = self
            .spine_weights
            .iter()
            .enumerate()
            .find(|(_, w)| !w.is_finite() || **w < 0.0)
        {
            return Err(WarpError::BadSpineWeight { index, value });
        }
        let sum: f32 = self.spine_weights.iter().sum();
        if !self.spine_weights.is_empty() && (sum - 1.0).abs() > 1e-3 {
            return Err(WarpError::SpineWeightSum(sum));
        }
        Ok(())
    }

    /// Also check every joint against `skeleton` (see [`OrientationWarpSettings::validate`]).
    ///
    /// # Errors
    ///
    /// The errors of [`OrientationWarpSettings::validate`] and [`WarpError::JointOutOfRange`].
    pub fn validate_for(&self, skeleton: &Skeleton) -> Result<(), WarpError> {
        self.validate()?;
        let joints = skeleton.len();
        match self.spine_joints.iter().find(|&&j| j as usize >= joints) {
            Some(&joint) => Err(WarpError::JointOutOfRange { joint, joints }),
            None => Ok(()),
        }
    }
}

/// An angle wrapped to `(−π, π]`.
fn wrap_angle(angle: f32) -> f32 {
    let wrapped = (angle + PI).rem_euclid(TAU) - PI;
    if wrapped <= -PI { wrapped + TAU } else { wrapped }
}

/// The lower-body turn, radians: `desired_heading − clip_heading` wrapped to `(−π, π]`, clipped
/// to `±max_delta`, times the strength. 0 when the warp is off, `max_delta` is 0 or less, or an
/// input is not finite. Headings are yaw about `+Y` (0 faces `+Z`).
pub fn orientation_delta(settings: &OrientationWarpSettings, desired_heading: f32, clip_heading: f32) -> f32 {
    let limit = settings.max_delta;
    let strength = settings.strength;
    if !(limit.is_finite()
        && limit > 0.0
        && strength.is_finite()
        && strength > 0.0
        && desired_heading.is_finite()
        && clip_heading.is_finite())
    {
        return 0.0;
    }
    let delta = wrap_angle(desired_heading - clip_heading).clamp(-limit, limit);
    delta * strength.min(1.0)
}

/// The rotation part of an object-space joint matrix.
pub(crate) fn rotation_of(m: Mat4) -> Quat {
    m.to_scale_rotation_translation().1
}

/// Turn the lower body of `pose` by `delta` radians about object-space `+Y` (the `pelvis`
/// joint's rotation, so everything under it turns about it) and turn each spine joint back by
/// `delta · weight`. The turn is expressed in each joint's parent space from the pose's own
/// globals, so it is correct for rotated parents. With weights summing to 1 the top of the
/// spine (and everything above it) keeps its object-space orientation.
///
/// Assumptions: the spine joints are descendants of `pelvis` given from low to high, and
/// object-space `+Y` is up. Invalid settings ([`OrientationWarpSettings::validate_for`]), a
/// non-finite or zero `delta` or an out-of-range `pelvis` leave the pose unchanged. The legs
/// turn with the pelvis; a foot-planting pass (foot IK) runs afterwards when the feet must stay
/// put.
#[tracing::instrument(target = "regia_animation", name = "motion_matching.warp", skip_all)]
pub fn apply_orientation_warp(
    skeleton: &Skeleton,
    pose: &mut Pose,
    pelvis: u32,
    settings: &OrientationWarpSettings,
    delta: f32,
) {
    let mut globals = Vec::new();
    apply_orientation_warp_with_globals(skeleton, pose, pelvis, settings, delta, &mut globals);
}

/// [`apply_orientation_warp`] writing the pose's object-space matrices into `globals` (resized
/// once, then reused): the graph's `OrientationWarp` node. Same math, same result bits.
pub fn apply_orientation_warp_with_globals(
    skeleton: &Skeleton,
    pose: &mut Pose,
    pelvis: u32,
    settings: &OrientationWarpSettings,
    delta: f32,
    globals: &mut Vec<Mat4>,
) {
    let joints = skeleton.len().min(pose.len());
    if !delta.is_finite()
        || delta == 0.0
        || pelvis as usize >= joints
        || settings.validate_for(skeleton).is_err()
    {
        return;
    }
    // Every change pre-multiplies a subtree's object-space rotation by a turn about +Y, and
    // turns about one axis commute, so each parent-space turn can be taken from the original
    // globals: `P⁻¹ · R · P` is the same before and after the turns above it.
    skeleton.globals_into(pose, None, globals);
    let globals = &*globals;
    let mut turn = |joint: u32, angle: f32| {
        let index = joint as usize;
        let parent = skeleton
            .parent_of(index)
            .filter(|&p| p < joints)
            .map_or(Quat::IDENTITY, |p| rotation_of(globals[p]));
        let local = parent.inverse() * det_rotation_y(angle) * parent;
        pose.rotations[index] = (local * pose.rotations[index]).normalize();
    };
    turn(pelvis, delta);
    for (&joint, &weight) in settings.spine_joints.iter().zip(&settings.spine_weights) {
        if (joint as usize) < joints && weight > 0.0 {
            turn(joint, -delta * weight);
        }
    }
}
