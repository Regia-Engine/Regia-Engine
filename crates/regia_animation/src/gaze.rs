// SPDX-License-Identifier: MPL-2.0
//! Eye gaze with eyelid follow (Tincal C6).
//!
//! Specification: `docs/specs/17-animation.md` § 3a. A [`Gaze`] component on a rig turns the
//! two eye joints towards a look-at target with per-eye limits, a saccade speed limit and
//! optional convergence, makes the eyelids follow the eyes' vertical motion, and optionally
//! turns the head (an [`AimChain`], reusing [`crate::aim_ik::solve_body_aim`]) before the eyes take the rest.
//!
//! [`solve_gaze`] is the pure solver (a skeleton, a pose, a rig description and a target in rig
//! space). The animation apply system runs it on every [`Gaze`] rig after the ragdoll blend and
//! before the rig drivers and the spring bones (on the same working pose), so drivers and
//! head-rooted springs see the gazed head and eyes, and the eye and lid joints land in the pose
//! the joint transforms and the palette are built from. A rig with an [`InstanceBindPose`] is
//! solved in its own proportions.
//!
//! Eyelid follow has two outputs. Lid *joints* ([`LidSpec`]) are rotated by a fraction of the
//! eye's vertical angle. Morph *channels* ([`EyeChannels`], named like the ARKit eye blend
//! shapes `eyeLookUpLeft` … `eyeLookOutRight`) are written through the [`GazeMorphSink`] trait,
//! so the solver does not depend on how a host stores morph weights; [`IndexedMorphs`] writes
//! them into an animator's by-index morph list through a [`LidMorphMap`].
//!
//! Provenance (`docs/provenance.md`): the look-at is the standard yaw/pitch decomposition of a
//! direction in a joint's rest frame; saccade speed limiting and eyelid follow (the lids track
//! the globe's vertical rotation) follow the textbook description of eye movements (Leigh and
//! Zee, "The Neurology of Eye Movements"); the channel names are the public ARKit blend-shape
//! location names. The convergence blend and the soft channel normalization are our own.

use std::collections::HashMap;

use regia_core::math::{Mat3, Mat4, Quat, Vec3, det};
use regia_ecs::prelude::*;
use regia_scene::Transform;
use serde::{Deserialize, Serialize};

use crate::aim_ik::{AimChain, solve_body_aim_into};
use crate::bind_pose::{InstanceBindPose, retarget_to_instance};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Which side of the face an eye (or a lid) is on, from the character's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EyeSide {
    /// The character's left eye (−X for a character that faces −Z with +Y up).
    Left,
    /// The character's right eye.
    Right,
}

impl EyeSide {
    fn index(self) -> usize {
        match self {
            EyeSide::Left => 0,
            EyeSide::Right => 1,
        }
    }
}

/// Rotation limits of one eye, radians from its rest direction.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EyeLimits {
    /// Towards the nose.
    pub inward: f32,
    /// Away from the nose.
    pub outward: f32,
    /// Looking up.
    pub up: f32,
    /// Looking down.
    pub down: f32,
}

impl Default for EyeLimits {
    /// Comfortable human limits: 35° in, 40° out, 26° up, 31° down.
    fn default() -> Self {
        EyeLimits {
            inward: 35f32.to_radians(),
            outward: 40f32.to_radians(),
            up: 26f32.to_radians(),
            down: 31f32.to_radians(),
        }
    }
}

/// One eye joint.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EyeSpec {
    /// Joint id of the eyeball.
    pub joint: u32,
    /// Rotation limits.
    pub limits: EyeLimits,
}

impl EyeSpec {
    /// An eye with the default human limits.
    pub fn new(joint: u32) -> Self {
        EyeSpec {
            joint,
            limits: EyeLimits::default(),
        }
    }
}

/// An eyelid joint that follows the eye's vertical angle.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LidSpec {
    /// Joint id of the lid.
    pub joint: u32,
    /// The eye this lid follows.
    pub side: EyeSide,
    /// Axis (the lid joint's local frame) the lid turns about; positive angle follows the eye
    /// looking up.
    pub axis: Vec3,
    /// Radians of lid rotation per radian of eye pitch (positive pitch = up). An upper lid
    /// follows strongly (about 0.7), a lower lid weakly (about 0.2).
    pub follow: f32,
    /// Largest lid rotation, radians.
    pub max_angle: f32,
}

impl LidSpec {
    /// A lid turning about local +X, following `follow` of the eye pitch, capped at 0.5 rad.
    pub fn new(joint: u32, side: EyeSide, follow: f32) -> Self {
        LidSpec {
            joint,
            side,
            axis: Vec3::X,
            follow,
            max_angle: 0.5,
        }
    }
}

/// Head follow: the head (and neck) turn towards the target before the eyes do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeadFollow {
    /// The aim chain (neck, head; `aim_joint` is usually the head).
    pub chain: AimChain,
    /// 0 = the head ignores the target, 1 = the head aims fully.
    pub weight: f32,
}

/// The description of a character's eyes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GazeRig {
    /// The character's left eye.
    pub left: EyeSpec,
    /// The character's right eye.
    pub right: EyeSpec,
    /// Eyelid joints.
    pub lids: Vec<LidSpec>,
    /// Optional head follow.
    pub head: Option<HeadFollow>,
    /// The eye joints' forward axis in their own frame (Regia Engine forward is −Z).
    pub forward: Vec3,
    /// The eye joints' up axis in their own frame.
    pub up: Vec3,
    /// 0 = both eyes look parallel (as at infinity), 1 = each eye aims at the target.
    pub convergence: f32,
    /// Targets nearer than this (metres from the midpoint of the eyes) are pushed out to it so
    /// the eyes never cross absurdly.
    pub min_distance: f32,
    /// Largest eye angular speed, radians per second (a saccade is about 9 rad/s = 500°/s).
    /// Zero or less = instantaneous.
    pub saccade_speed: f32,
}

impl GazeRig {
    /// A rig with the two eye joints, forward −Z, up +Y, full convergence from 0.25 m and a
    /// 9 rad/s saccade limit.
    pub fn new(left: u32, right: u32) -> Self {
        GazeRig {
            left: EyeSpec::new(left),
            right: EyeSpec::new(right),
            lids: Vec::new(),
            head: None,
            forward: Vec3::NEG_Z,
            up: Vec3::Y,
            convergence: 1.0,
            min_distance: 0.25,
            saccade_speed: 9.0,
        }
    }

    /// Find the eye joints and lids of a skeleton by common names (`eye_l`, `LeftEye`,
    /// `upper_lid.R`, …). `None` when either eyeball is missing. Lids are matched on `lid` plus
    /// `upper`/`lower` and a side marker; upper lids follow 0.7, lower 0.2.
    pub fn from_skeleton(skeleton: &Skeleton) -> Option<Self> {
        let side_of = |name: &str| -> Option<EyeSide> {
            let n = name.to_ascii_lowercase();
            let left = n.contains("left") || n.ends_with("_l") || n.ends_with(".l") || n.contains("_l_");
            let right = n.contains("right") || n.ends_with("_r") || n.ends_with(".r") || n.contains("_r_");
            match (left, right) {
                (true, false) => Some(EyeSide::Left),
                (false, true) => Some(EyeSide::Right),
                _ => None,
            }
        };
        let mut eyes: [Option<u32>; 2] = [None, None];
        let mut lids = Vec::new();
        for (i, name) in skeleton.joint_names.iter().enumerate() {
            let lower = name.to_ascii_lowercase();
            let Some(side) = side_of(name) else { continue };
            if lower.contains("lid") {
                let follow = if lower.contains("upper") {
                    0.7
                } else if lower.contains("lower") {
                    0.2
                } else {
                    continue;
                };
                lids.push(LidSpec::new(i as u32, side, follow));
            } else if lower.contains("eye") && !lower.contains("brow") && !lower.contains("lash") {
                eyes[side.index()].get_or_insert(i as u32);
            }
        }
        let mut rig = GazeRig::new(eyes[0]?, eyes[1]?);
        rig.lids = lids;
        Some(rig)
    }
}

/// What a [`Gaze`] looks at.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum GazeTarget {
    /// Nothing: the eyes return to their rest direction.
    #[default]
    None,
    /// A world-space point.
    Point(Vec3),
    /// An entity (its world position).
    Entity(Entity),
}

/// The four directional channels of one eye, each 0..1: the angle in that direction over the
/// eye's limit in it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EyeChannels {
    /// Looking up.
    pub up: f32,
    /// Looking down.
    pub down: f32,
    /// Towards the nose.
    pub inward: f32,
    /// Away from the nose.
    pub outward: f32,
}

/// One gaze output channel (the ARKit eye look blend shapes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EyeChannel {
    /// `eyeLookUp*`.
    Up,
    /// `eyeLookDown*`.
    Down,
    /// `eyeLookIn*`.
    In,
    /// `eyeLookOut*`.
    Out,
}

impl EyeChannel {
    /// All channels.
    pub const ALL: [EyeChannel; 4] = [EyeChannel::Up, EyeChannel::Down, EyeChannel::In, EyeChannel::Out];

    /// The ARKit blend-shape name of this channel on `side` (for example `eyeLookUpLeft`).
    pub fn arkit_name(self, side: EyeSide) -> &'static str {
        match (self, side) {
            (EyeChannel::Up, EyeSide::Left) => "eyeLookUpLeft",
            (EyeChannel::Up, EyeSide::Right) => "eyeLookUpRight",
            (EyeChannel::Down, EyeSide::Left) => "eyeLookDownLeft",
            (EyeChannel::Down, EyeSide::Right) => "eyeLookDownRight",
            (EyeChannel::In, EyeSide::Left) => "eyeLookInLeft",
            (EyeChannel::In, EyeSide::Right) => "eyeLookInRight",
            (EyeChannel::Out, EyeSide::Left) => "eyeLookOutLeft",
            (EyeChannel::Out, EyeSide::Right) => "eyeLookOutRight",
        }
    }
}

impl EyeChannels {
    /// The value of one channel.
    pub fn get(&self, channel: EyeChannel) -> f32 {
        match channel {
            EyeChannel::Up => self.up,
            EyeChannel::Down => self.down,
            EyeChannel::In => self.inward,
            EyeChannel::Out => self.outward,
        }
    }
}

/// Receives the gaze morph channels. Implement this for however a host stores morph weights;
/// [`IndexedMorphs`] (an animator's by-index list) and `HashMap<String, f32>` (by ARKit name)
/// are provided.
pub trait GazeMorphSink {
    /// Set one channel of one eye to `weight` (0..1).
    fn set_gaze_channel(&mut self, side: EyeSide, channel: EyeChannel, weight: f32);
}

impl GazeMorphSink for HashMap<String, f32> {
    fn set_gaze_channel(&mut self, side: EyeSide, channel: EyeChannel, weight: f32) {
        self.insert(channel.arkit_name(side).to_string(), weight);
    }
}

/// Which morph target index each gaze channel drives.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LidMorphMap {
    /// `(side, channel, morph target index)`.
    pub entries: Vec<(EyeSide, EyeChannel, u32)>,
}

impl LidMorphMap {
    /// Map every ARKit eye look name found in `morph_names` (index = target index).
    pub fn from_arkit_names(morph_names: &[String]) -> Self {
        let mut entries = Vec::new();
        for side in [EyeSide::Left, EyeSide::Right] {
            for channel in EyeChannel::ALL {
                let wanted = channel.arkit_name(side);
                if let Some(i) = morph_names.iter().position(|n| n.eq_ignore_ascii_case(wanted)) {
                    entries.push((side, channel, i as u32));
                }
            }
        }
        LidMorphMap { entries }
    }
}

/// A [`GazeMorphSink`] that replaces entries of an animator's by-index morph list.
pub struct IndexedMorphs<'a> {
    /// The channel → target index map.
    pub map: &'a LidMorphMap,
    /// The `(target index, weight)` list to write into.
    pub out: &'a mut Vec<(u32, f32)>,
}

impl GazeMorphSink for IndexedMorphs<'_> {
    fn set_gaze_channel(&mut self, side: EyeSide, channel: EyeChannel, weight: f32) {
        for &(s, c, index) in &self.map.entries {
            if s == side && c == channel {
                match self.out.iter_mut().find(|(i, _)| *i == index) {
                    Some(slot) => slot.1 = weight,
                    None => self.out.push((index, weight)),
                }
            }
        }
    }
}

/// Runtime state of a gaze: the current eye angles (the saccade limiter moves them).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GazeState {
    /// `(yaw, pitch)` per eye, radians; yaw positive = towards the character's left, pitch
    /// positive = up. Index 0 = left eye.
    pub angles: [(f32, f32); 2],
    /// False until the first solve, which snaps to the goal.
    pub initialized: bool,
}

/// What [`solve_gaze`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GazeResult {
    /// False when the rig named joints outside the skeleton (nothing was written).
    pub valid: bool,
    /// Current `(yaw, pitch)` per eye after the speed limit, radians (left eye first).
    pub angles: [(f32, f32); 2],
    /// The `(yaw, pitch)` each eye wants (clamped to its limits, before the speed limit).
    pub goal: [(f32, f32); 2],
    /// True when the target lay outside the eye's limits (the eye is pinned at the limit).
    pub saturated: [bool; 2],
    /// Worst angle between an eye's gaze direction and the direction to the target, radians.
    pub error: f32,
    /// Directional channels per eye (left first), already multiplied by the weight.
    pub channels: [EyeChannels; 2],
}

impl GazeResult {
    /// Write every channel of both eyes into `sink`.
    pub fn write_morphs(&self, sink: &mut dyn GazeMorphSink) {
        for side in [EyeSide::Left, EyeSide::Right] {
            for channel in EyeChannel::ALL {
                sink.set_gaze_channel(side, channel, self.channels[side.index()].get(channel));
            }
        }
    }
}

fn rotation_of(m: Mat4) -> Quat {
    m.to_scale_rotation_translation().1.normalize()
}

/// The global rotation of `joint`. A joint (or an ancestor) scaled to zero, as when a head is
/// hidden for a first-person view, has no rotation in its matrix (decomposing it gives NaN), so
/// the rotation then comes from the pose's local rotations along the chain, which scale does not
/// touch.
fn global_rotation(skeleton: &Skeleton, pose: &Pose, globals: &[Mat4], joint: usize) -> Quat {
    let q = rotation_of(globals[joint]);
    if q.is_finite() {
        return q;
    }
    let mut q = Quat::IDENTITY;
    let mut at = Some(joint as u32);
    // At most one step per joint, so a malformed parent cycle cannot hang.
    for _ in 0..skeleton.len() {
        let Some(j) = at else { break };
        q = pose.rotations.get(j as usize).copied().unwrap_or(Quat::IDENTITY) * q;
        at = skeleton.parents.get(j as usize).copied().flatten();
    }
    let q = q.normalize();
    if q.is_finite() { q } else { Quat::IDENTITY }
}

/// The rotation taking (−Z forward, +Y up) to the given axes.
fn frame_of(forward: Vec3, up: Vec3) -> Quat {
    let f = forward.normalize_or(Vec3::NEG_Z);
    let right = f.cross(up).normalize_or(Vec3::X);
    let u = right.cross(f);
    Quat::from_mat3(&Mat3::from_cols(right, u, -f)).normalize()
}

/// Turn the eyes (and optionally the head) of `rig` towards `target` (rig space) and make the
/// lids follow. `weight` blends with the incoming pose (0 = untouched, 1 = fully gazing);
/// `dt` is the time since the last solve for the saccade limiter. Lid rotations compose on the
/// pose's incoming lid rotation, so blinks from clips survive.
pub fn solve_gaze(
    skeleton: &Skeleton,
    pose: &mut Pose,
    rig: &GazeRig,
    state: &mut GazeState,
    target: Option<Vec3>,
    weight: f32,
    dt: f32,
) -> GazeResult {
    let n = skeleton.len().min(pose.len());
    let eyes = [rig.left, rig.right];
    let joints_ok = eyes.iter().all(|e| (e.joint as usize) < n)
        && rig.lids.iter().all(|l| (l.joint as usize) < n)
        && rig.left.joint != rig.right.joint;
    if !joints_ok {
        return GazeResult::default();
    }
    let w = crate::num::unit_weight(weight);

    let mut globals = Vec::new();
    if let (Some(head), Some(target)) = (&rig.head, target) {
        solve_body_aim_into(
            skeleton,
            pose,
            &head.chain,
            target,
            &[],
            w * crate::num::unit_weight(head.weight),
            &mut globals,
        );
    }
    if globals.len() != skeleton.len() {
        skeleton.globals_into(pose, None, &mut globals);
    }
    let basis = frame_of(rig.forward, rig.up);
    let positions = [
        globals[rig.left.joint as usize].w_axis.truncate(),
        globals[rig.right.joint as usize].w_axis.truncate(),
    ];
    let mid = (positions[0] + positions[1]) * 0.5;

    // Per-eye world direction: blend between parallel and fully converged.
    let directions: Option<[Vec3; 2]> = target.and_then(|t| {
        let to = t - mid;
        let distance = to.length().max(rig.min_distance.max(1e-3));
        let along = to.normalize_or_zero();
        if along == Vec3::ZERO {
            return None;
        }
        let point = mid + along * distance;
        let c = rig.convergence.clamp(0.0, 1.0);
        let dir = |p: Vec3| (along.lerp((point - p).normalize_or(along), c)).normalize_or(along);
        Some([dir(positions[0]), dir(positions[1])])
    });

    let mut result = GazeResult {
        valid: true,
        ..GazeResult::default()
    };
    let mut frames = [Quat::IDENTITY; 2];
    for (k, eye) in eyes.iter().enumerate() {
        let joint = eye.joint as usize;
        let parent = skeleton
            .parent_of(joint)
            .map_or(Quat::IDENTITY, |p| global_rotation(skeleton, pose, &globals, p));
        let rest = skeleton.bind_pose[joint].rotation;
        // The eye's rest frame with forward = −Z, up = +Y.
        let frame = (parent * rest * basis).normalize();
        frames[k] = frame;
        let goal = match directions {
            Some(dirs) => {
                let local = frame.inverse() * dirs[k];
                // The deterministic kernels keep the eye goals bit-identical across platform
                // libm (audit-v040 L10).
                let yaw = det::atan2(-local.x, -local.z);
                let pitch = det::asin(local.y.clamp(-1.0, 1.0));
                let side = if k == 0 { EyeSide::Left } else { EyeSide::Right };
                clamp_angles(yaw, pitch, &eye.limits, side, &mut result.saturated[k])
            }
            None => (0.0, 0.0),
        };
        // A degenerate frame (non-finite input pose) looks straight ahead rather than writing NaN.
        let goal = if goal.0.is_finite() && goal.1.is_finite() {
            goal
        } else {
            (0.0, 0.0)
        };
        result.goal[k] = goal;
    }

    // Saccade limiter.
    for k in 0..2 {
        let (goal_yaw, goal_pitch) = result.goal[k];
        // A non-finite state (a host wrote it, or an older build poisoned it) snaps to the goal
        // instead of staying NaN forever.
        let stale = !(state.angles[k].0.is_finite() && state.angles[k].1.is_finite());
        if !state.initialized || stale || rig.saccade_speed.is_nan() || rig.saccade_speed <= 0.0 {
            state.angles[k] = (goal_yaw, goal_pitch);
        } else {
            let step = rig.saccade_speed * dt.max(0.0);
            let (yaw, pitch) = state.angles[k];
            let (dy, dp) = (goal_yaw - yaw, goal_pitch - pitch);
            let distance = (dy * dy + dp * dp).sqrt();
            state.angles[k] = if distance <= step || distance < 1e-6 {
                (goal_yaw, goal_pitch)
            } else {
                (yaw + dy / distance * step, pitch + dp / distance * step)
            };
        }
    }
    state.initialized = true;
    result.angles = state.angles;

    // Write eye rotations (blended with the incoming pose by `w`).
    for (k, eye) in eyes.iter().enumerate() {
        let joint = eye.joint as usize;
        let (yaw, pitch) = state.angles[k];
        // The det kernel, not glam's libm: the gazed pose bits must match on every platform
        // (fixrev-06-k H2).
        let look = det::quat_from_rotation_y(yaw) * det::quat_from_rotation_x(pitch);
        let gazed = (skeleton.bind_pose[joint].rotation * basis * look * basis.inverse()).normalize();
        pose.rotations[joint] = det::quat_slerp(pose.rotations[joint], gazed, w).normalize();

        if let Some(dirs) = directions {
            let achieved = frames[k] * (look * Vec3::NEG_Z);
            result.error = result.error.max(achieved.angle_between(dirs[k]));
        }
        let limits = eye.limits;
        let safe = |v: f32, limit: f32| {
            if limit > 1e-6 {
                (v / limit).clamp(0.0, 1.0) * w
            } else {
                0.0
            }
        };
        let side_is_left = k == 0;
        // Positive yaw turns towards the character's left: outward for the left eye.
        let (out_amount, in_amount) = if side_is_left {
            (yaw.max(0.0), (-yaw).max(0.0))
        } else {
            ((-yaw).max(0.0), yaw.max(0.0))
        };
        result.channels[k] = EyeChannels {
            up: safe(pitch.max(0.0), limits.up),
            down: safe((-pitch).max(0.0), limits.down),
            inward: safe(in_amount, limits.inward),
            outward: safe(out_amount, limits.outward),
        };
    }

    // Eyelid follow.
    for lid in &rig.lids {
        let pitch = state.angles[lid.side.index()].1;
        let limit = lid.max_angle.abs();
        let angle = crate::num::clamp_between(pitch * lid.follow, -limit, limit) * w;
        let joint = lid.joint as usize;
        let axis = lid.axis.normalize_or(Vec3::X);
        // Deterministic kernel, like the eye write above (fixrev-06-k H2).
        pose.rotations[joint] = (pose.rotations[joint] * det::quat_from_axis_angle(axis, angle)).normalize();
    }
    result
}

fn clamp_angles(yaw: f32, pitch: f32, limits: &EyeLimits, side: EyeSide, saturated: &mut bool) -> (f32, f32) {
    // Positive yaw = towards the character's left = outward for the left eye.
    let (max_pos, max_neg) = match side {
        EyeSide::Left => (limits.outward, limits.inward),
        EyeSide::Right => (limits.inward, limits.outward),
    };
    let clamped_yaw = yaw.clamp(-max_neg.max(0.0), max_pos.max(0.0));
    let clamped_pitch = pitch.clamp(-limits.down.max(0.0), limits.up.max(0.0));
    *saturated = (clamped_yaw - yaw).abs() > 1e-5 || (clamped_pitch - pitch).abs() > 1e-5;
    (clamped_yaw, clamped_pitch)
}

/// A rig's gaze: the description, the target and the runtime state. Add it to an entity that
/// has an [`Animator`](crate::Animator) and a [`SkinnedMesh`](crate::SkinnedMesh); the animation
/// apply system does the rest.
#[derive(Component, Clone, Debug)]
pub struct Gaze {
    /// The eyes, lids and head.
    pub rig: GazeRig,
    /// What to look at.
    pub target: GazeTarget,
    /// 0 = animation only, 1 = fully gazing.
    pub weight: f32,
    /// Morph targets the eye channels drive (empty = joints only).
    pub morphs: LidMorphMap,
    pub(crate) state: GazeState,
    pub(crate) last: GazeResult,
    /// The shared skeleton with the rig's [`InstanceBindPose`] rest pose (rebuilt when either
    /// changes) and the instance-space working pose, for rigs with their own proportions.
    pub(crate) instance: Option<Box<(Skeleton, Pose)>>,
    /// Rotations from before the instance-space solve, reused so the mapping back does not
    /// clone the pose.
    original_rotations: Vec<Quat>,
}

impl Gaze {
    /// A gaze with no target, full weight and no morph targets.
    pub fn new(rig: GazeRig) -> Self {
        Gaze {
            rig,
            target: GazeTarget::None,
            weight: 1.0,
            morphs: LidMorphMap::default(),
            state: GazeState::default(),
            last: GazeResult::default(),
            instance: None,
            original_rotations: Vec::new(),
        }
    }

    /// The result of the last solve.
    pub fn result(&self) -> &GazeResult {
        &self.last
    }
}

/// Where a [`GazeTarget`] is in world space; `position_of` finds an entity target's world
/// position (`None` when the entity is gone or has no transform).
pub fn gaze_target_world(
    target: GazeTarget,
    position_of: impl FnOnce(Entity) -> Option<Vec3>,
) -> Option<Vec3> {
    match target {
        GazeTarget::None => None,
        GazeTarget::Point(p) => Some(p),
        GazeTarget::Entity(e) => position_of(e),
    }
}

/// A [`GazeMorphSink`] over a dense morph weight list (index = target index), growing it as needed.
struct DenseMorphs<'a> {
    map: &'a LidMorphMap,
    out: &'a mut Vec<f32>,
}

impl GazeMorphSink for DenseMorphs<'_> {
    fn set_gaze_channel(&mut self, side: EyeSide, channel: EyeChannel, weight: f32) {
        for &(s, c, index) in &self.map.entries {
            if s == side && c == channel {
                let index = index as usize;
                if index >= self.out.len() {
                    self.out.resize(index + 1, 0.0);
                }
                self.out[index] = weight;
            }
        }
    }
}

/// Solve one rig's [`Gaze`] on `pose` (rig space) and write its eye-look channels into the dense
/// `morphs` list. `crate::systems::apply_animation` calls it on its working copy of the pose
/// after the ragdoll blend and before the rig drivers and the spring bones, so twist and
/// corrective drivers and head-rooted spring chains see the gazed head and eyes. `rig` is the
/// rig's world transform and `world_target` the target in world space.
///
/// With an `instance` bind pose the eyes are solved in the instance's own proportions (eye
/// height, interocular distance and convergence of the avatar as drawn), then the rotations
/// are mapped back onto the shared skeleton's pose, which the rest of the chain works on.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_gaze(
    gaze: &mut Gaze,
    skeleton: &Skeleton,
    instance: Option<&InstanceBindPose>,
    pose: &mut Pose,
    morphs: &mut Vec<f32>,
    rig: &Transform,
    world_target: Option<Vec3>,
    dt: f32,
) {
    let _span = tracing::debug_span!("apply_gaze").entered();
    let rig_matrix = Mat4::from_scale_rotation_translation(rig.scale, rig.rotation, rig.translation);
    let target = world_target.map(|p| rig_matrix.inverse().transform_point3(p));
    let instance = instance.filter(|i| i.len() == skeleton.len() && pose.len() == skeleton.len());
    let result = match instance {
        None => {
            gaze.instance = None;
            solve_gaze(
                skeleton,
                pose,
                &gaze.rig,
                &mut gaze.state,
                target,
                gaze.weight,
                dt,
            )
        }
        Some(instance) => {
            let stale = gaze.instance.as_ref().is_none_or(|cached| {
                cached.0.bind_pose != instance.bind_pose || cached.0.parents != skeleton.parents
            });
            if stale {
                let mut own = skeleton.clone();
                own.bind_pose.clone_from(&instance.bind_pose);
                own.inverse_bind_matrices
                    .clone_from(&instance.inverse_bind_matrices);
                gaze.instance = Some(Box::new((own, Pose::default())));
            }
            let Some(cached) = gaze.instance.as_deref_mut() else {
                return;
            };
            let (own, scratch) = cached;
            retarget_to_instance(skeleton, instance, pose, scratch);
            let mut before = std::mem::take(&mut gaze.original_rotations);
            before.clear();
            before.extend_from_slice(&scratch.rotations);
            let result = solve_gaze(own, scratch, &gaze.rig, &mut gaze.state, target, gaze.weight, dt);
            // Back to the shared skeleton: the inverse of `retarget_to_instance`'s rotation map,
            // for the joints the solve turned (the others keep their exact incoming rotation).
            for (i, (now, was)) in scratch.rotations.iter().zip(&before).enumerate() {
                if now != was {
                    let base = skeleton.bind_pose[i].rotation;
                    let inst = instance.bind_pose[i].rotation;
                    pose.rotations[i] = (base * inst.inverse() * *now).normalize();
                }
            }
            gaze.original_rotations = before;
            result
        }
    };
    if result.valid && !gaze.morphs.entries.is_empty() {
        result.write_morphs(&mut DenseMorphs {
            map: &gaze.morphs,
            out: morphs,
        });
    }
    gaze.last = result;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::translated;
    use crate::skeleton::LocalBind;

    /// root → head (1.6 m up); head → eye_l / eye_r (±3 cm, 8 cm forward), upper_lid_l / _r.
    fn face() -> Skeleton {
        let names = [
            "root",
            "head",
            "eye_l",
            "eye_r",
            "upper_lid_l",
            "upper_lid_r",
            "neck",
        ];
        let parents = vec![None, Some(0), Some(1), Some(1), Some(1), Some(1), Some(0)];
        let binds: Vec<LocalBind> = vec![
            translated(Vec3::ZERO),
            translated(Vec3::new(0.0, 1.6, 0.0)),
            translated(Vec3::new(-0.03, 0.05, -0.08)),
            translated(Vec3::new(0.03, 0.05, -0.08)),
            translated(Vec3::new(-0.03, 0.07, -0.08)),
            translated(Vec3::new(0.03, 0.07, -0.08)),
            translated(Vec3::new(0.0, 1.4, 0.0)),
        ];
        Skeleton::from_local_bind(names.iter().map(|s| s.to_string()).collect(), parents, binds)
            .expect("valid skeleton")
    }

    fn rig() -> GazeRig {
        let mut rig = GazeRig::new(2, 3);
        rig.lids = vec![
            LidSpec::new(4, EyeSide::Left, 0.7),
            LidSpec::new(5, EyeSide::Right, 0.7),
        ];
        rig
    }

    fn gaze_error(sk: &Skeleton, pose: &Pose, eye: usize, target: Vec3) -> f32 {
        let globals = sk.globals(pose, None);
        let forward = rotation_of(globals[eye]) * Vec3::NEG_Z;
        forward.angle_between((target - globals[eye].w_axis.truncate()).normalize())
    }

    #[test]
    fn eyes_point_at_the_target_each() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let target = Vec3::new(-0.6, 1.8, -2.0);
        let r = solve_gaze(&sk, &mut pose, &rig(), &mut state, Some(target), 1.0, 0.016);
        assert!(r.valid);
        assert!(r.error < 1e-3, "error {}", r.error);
        assert!(r.saturated == [false, false]);
        for eye in [2usize, 3] {
            assert!(gaze_error(&sk, &pose, eye, target) < 1e-3, "eye {eye}");
        }
        // Looking left and up: positive yaw and pitch on both eyes.
        for angle in r.angles {
            assert!(angle.0 > 0.1 && angle.1 > 0.0, "{angle:?}");
        }
        // Left eye looks outward, so the outward channel fills; right eye looks inward.
        assert!(r.channels[0].outward > 0.0 && r.channels[0].inward == 0.0);
        assert!(r.channels[1].inward > 0.0 && r.channels[1].outward == 0.0);
        assert!(r.channels[0].up > 0.0 && r.channels[0].down == 0.0);
    }

    /// fixrev-06-k H2: a half-weight gaze write blends the solved eye rotation into the pose
    /// with the deterministic kernels (`quat_from_rotation_y/x`, `quat_slerp`), not glam's
    /// libm: the written bits are pinned literally, and the det and libm constructions are
    /// asserted to disagree on this host so the pin cannot pass on the libm path.
    #[test]
    fn weighted_gaze_pose_bits_are_pinned_and_platform_stable() {
        // Anti-vacuousness guard: the det kernels are correctly rounded like glibc's, so the
        // `from_rotation_*` constructions agree with libm on most inputs; the weight blend's
        // slerp is where this host's libm and the kernel provably diverge (same pair as
        // `weighted_look_at_blend_is_pinned_and_platform_stable`). If no candidate diverges
        // on some future libm, the pin below is vacuous there — fail loudly instead.
        let a = Quat::from_xyzw(0.1, 0.2, 0.3, 0.9).normalize();
        let b = Quat::from_xyzw(0.3, -0.1, 0.2, 0.8).normalize();
        assert_ne!(
            det::quat_slerp(a, b, 0.5).to_array().map(f32::to_bits),
            a.slerp(b, 0.5).to_array().map(f32::to_bits),
            "det and libm slerp agree on this host: the pose pin below is vacuous"
        );

        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(
            &sk,
            &mut pose,
            &rig(),
            &mut state,
            Some(Vec3::new(-0.6, 1.8, -2.0)),
            0.5,
            0.016,
        );
        assert!(r.valid);
        // The angles and the slerp are the det kernels, the same bits on every libm. The lid's last
        // step is glam's `Quat` product and `normalize`, whose four-lane dot groups the lanes
        // differently on SSE2 and NEON (ADR-5176), so the lid's bits are one value per instruction
        // set (ADR-5860, measured on the M5); the eyes agree on both.
        // glam scalar-math (`cross-platform-determinism`, ADR-5176) gives x86_64 the aarch64 bits
        // (measured on the build server, lane gate-lavapipe-coverage), so that build takes the aarch64 pin.
        #[cfg(not(any(target_arch = "aarch64", feature = "cross-platform-determinism")))]
        const UPPER_LID_L: [u32; 4] = [0x3c5654a4, 0x0, 0x0, 0x3f7ffa65];
        #[cfg(any(target_arch = "aarch64", feature = "cross-platform-determinism"))]
        const UPPER_LID_L: [u32; 4] = [0x3c5654a5, 0x0, 0x0, 0x3f7ffa66];
        for (joint, name) in [(2usize, "eye_l"), (3, "eye_r"), (4, "upper_lid_l")] {
            let q = pose.rotations[joint];
            let bits = q.to_array().map(f32::to_bits);
            let expected: [u32; 4] = match joint {
                2 => [0x3c97e490, 0x3d938d17, 0xbb309086, 0x3f7f4a28],
                3 => [0x3c964ea9, 0x3da21439, 0xbb403c79, 0x3f7f2719],
                _ => UPPER_LID_L,
            };
            assert_eq!(bits, expected, "{name} quat {q:?}");
        }
    }

    #[test]
    fn straight_ahead_leaves_the_eyes_at_rest() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let mut parallel = rig();
        parallel.convergence = 0.0;
        let r = solve_gaze(
            &sk,
            &mut pose,
            &parallel,
            &mut state,
            Some(Vec3::new(0.0, 1.65, -10.0)),
            1.0,
            0.016,
        );
        assert!(r.angles.iter().all(|a| a.0.abs() < 1e-4 && a.1.abs() < 1e-4));
        assert!(pose.rotations[2].angle_between(Quat::IDENTITY) < 1e-4);
    }

    #[test]
    fn targets_outside_the_limits_pin_the_eye() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(
            &sk,
            &mut pose,
            &rig(),
            &mut state,
            Some(Vec3::new(5.0, 1.65, 0.0)),
            1.0,
            0.016,
        );
        assert!(r.saturated == [true, true]);
        // Hard to the character's right: the left eye looks inward, the right eye outward.
        let limits = EyeLimits::default();
        assert!((r.angles[0].0 + limits.inward).abs() < 1e-4, "{:?}", r.angles);
        assert!((r.angles[1].0 + limits.outward).abs() < 1e-4, "{:?}", r.angles);
        assert!(r.error > 0.3);
    }

    #[test]
    fn saccades_are_speed_limited() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let g = rig();
        let first = Vec3::new(0.0, 1.65, -3.0);
        let _ = solve_gaze(&sk, &mut pose, &g, &mut state, Some(first), 1.0, 0.016);
        let far = Vec3::new(-3.0, 1.65, -3.0); // 45° to the left, beyond the outward limit? no: 0.7 rad
        let before = state.angles[0].0;
        let r = solve_gaze(&sk, &mut pose, &g, &mut state, Some(far), 1.0, 0.016);
        let moved = (r.angles[0].0 - before).abs();
        assert!(moved <= 9.0 * 0.016 + 1e-5, "moved {moved}");
        assert!(moved > 0.1, "it does move: {moved}");
        let mut frames = 0;
        while (state.angles[0].0 - r.goal[0].0).abs() > 1e-4 && frames < 100 {
            let _ = solve_gaze(&sk, &mut pose, &g, &mut state, Some(far), 1.0, 0.016);
            frames += 1;
        }
        assert!(frames < 20, "arrived after {frames} frames");
    }

    #[test]
    fn near_targets_converge_and_parallel_does_not() {
        let sk = face();
        let near = Vec3::new(0.0, 1.65, -0.6);
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(&sk, &mut pose, &rig(), &mut state, Some(near), 1.0, 0.016);
        // Left eye turns right (negative yaw), right eye turns left (positive).
        assert!(r.angles[0].0 < -0.02 && r.angles[1].0 > 0.02, "{:?}", r.angles);
        let mut parallel = rig();
        parallel.convergence = 0.0;
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(&sk, &mut pose, &parallel, &mut state, Some(near), 1.0, 0.016);
        assert!(r.angles.iter().all(|a| a.0.abs() < 1e-4));
    }

    #[test]
    fn very_near_targets_are_pushed_out_to_min_distance() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(
            &sk,
            &mut pose,
            &rig(),
            &mut state,
            Some(Vec3::new(0.0, 1.65, -0.15)),
            1.0,
            0.016,
        );
        // Convergence angle is at most atan(0.03 / 0.25) per eye.
        let cap = (0.03f32 / 0.25).atan() + 1e-3;
        assert!(r.angles.iter().all(|a| a.0.abs() <= cap), "{:?}", r.angles);
    }

    #[test]
    fn lids_follow_the_eye_pitch_and_keep_the_animation() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        // The clip closes the left lid a little; the follow composes on top.
        let blink = Quat::from_rotation_x(-0.2);
        pose.rotations[4] = blink;
        let mut state = GazeState::default();
        let r = solve_gaze(
            &sk,
            &mut pose,
            &rig(),
            &mut state,
            Some(Vec3::new(0.0, 2.6, -2.0)),
            1.0,
            0.016,
        );
        let pitch = r.angles[0].1;
        assert!(pitch > 0.2);
        let expected = blink * Quat::from_rotation_x((pitch * 0.7).min(0.5));
        assert!(pose.rotations[4].angle_between(expected) < 1e-4);
        // Looking down lowers the lid.
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(
            &sk,
            &mut pose,
            &rig(),
            &mut state,
            Some(Vec3::new(0.0, 0.2, -1.5)),
            1.0,
            0.016,
        );
        assert!(r.angles[0].1 < -0.2);
        let (axis, angle) = pose.rotations[5].to_axis_angle();
        assert!(axis.x.signum() * angle < 0.0 || angle < 1e-6, "lid goes down");
        assert!(r.channels[0].down > 0.0 && r.channels[0].up == 0.0);
    }

    #[test]
    fn weight_blends_and_zero_is_a_no_op() {
        let sk = face();
        let target = Vec3::new(-1.0, 1.7, -2.0);
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(&sk, &mut pose, &rig(), &mut state, Some(target), 0.0, 0.016);
        assert_eq!(pose, sk.bind_pose_pose());
        assert!(r.channels.iter().all(|c| *c == EyeChannels::default()));
        let mut full = sk.bind_pose_pose();
        let mut half = sk.bind_pose_pose();
        let (mut s1, mut s2) = (GazeState::default(), GazeState::default());
        solve_gaze(&sk, &mut full, &rig(), &mut s1, Some(target), 1.0, 0.016);
        solve_gaze(&sk, &mut half, &rig(), &mut s2, Some(target), 0.5, 0.016);
        let a_full = full.rotations[2].angle_between(Quat::IDENTITY);
        let a_half = half.rotations[2].angle_between(Quat::IDENTITY);
        assert!((a_half - a_full * 0.5).abs() < 0.02, "{a_half} vs {a_full}");
    }

    #[test]
    fn no_target_returns_the_eyes_to_rest() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let mut instant = rig();
        instant.saccade_speed = 0.0;
        solve_gaze(
            &sk,
            &mut pose,
            &instant,
            &mut state,
            Some(Vec3::new(-1.0, 1.7, -2.0)),
            1.0,
            0.016,
        );
        assert!(pose.rotations[2].angle_between(Quat::IDENTITY) > 0.1);
        solve_gaze(&sk, &mut pose, &instant, &mut state, None, 1.0, 0.016);
        assert!(pose.rotations[2].angle_between(Quat::IDENTITY) < 1e-4);
    }

    #[test]
    fn bad_joints_are_rejected_without_writing() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(
            &sk,
            &mut pose,
            &GazeRig::new(2, 99),
            &mut state,
            Some(Vec3::Z),
            1.0,
            0.016,
        );
        assert!(!r.valid);
        assert_eq!(pose, sk.bind_pose_pose());
    }

    /// Review w11 #3: a head scaled to zero (hidden for first person) leaves the eye angles and
    /// rotations finite, and the gaze recovers when the scale comes back. Also a state poisoned by
    /// NaN (an older build) snaps back to the goal.
    #[test]
    fn a_zero_scaled_parent_does_not_poison_the_gaze() {
        let sk = face();
        let g = rig();
        let target = Vec3::new(0.4, 1.7, -1.0);
        let mut state = GazeState::default();
        let mut pose = sk.bind_pose_pose();
        solve_gaze(&sk, &mut pose, &g, &mut state, Some(target), 1.0, 0.016);
        let mut hidden = sk.bind_pose_pose();
        hidden.scales[1] = Vec3::ZERO;
        hidden.rotations[1] = Quat::from_rotation_y(0.3);
        for _ in 0..3 {
            let r = solve_gaze(&sk, &mut hidden, &g, &mut state, Some(target), 1.0, 0.016);
            assert!(r.valid);
            for k in 0..2 {
                assert!(r.angles[k].0.is_finite() && r.angles[k].1.is_finite(), "{r:?}");
            }
            for j in [2usize, 3, 4, 5] {
                assert!(
                    hidden.rotations[j].is_finite(),
                    "joint {j}: {:?}",
                    hidden.rotations[j]
                );
            }
        }
        // Scale restored: the eyes point at the target again (the limiter catches up).
        let pose = (0..60).fold(sk.bind_pose_pose(), |_, _| {
            let mut pose = sk.bind_pose_pose();
            solve_gaze(&sk, &mut pose, &g, &mut state, Some(target), 1.0, 0.016);
            pose
        });
        for eye in [2usize, 3] {
            let err = gaze_error(&sk, &pose, eye, target);
            assert!(err < 0.01, "eye {eye} after the scale returns: {err} rad");
        }
        // A NaN state recovers on the next solve.
        state.angles = [(f32::NAN, 0.0), (0.0, f32::NAN)];
        let mut pose = sk.bind_pose_pose();
        let r = solve_gaze(&sk, &mut pose, &g, &mut state, Some(target), 1.0, 0.016);
        assert!(
            r.angles.iter().all(|a| a.0.is_finite() && a.1.is_finite()),
            "{r:?}"
        );
        assert!(gaze_error(&sk, &pose, 2, target) < 0.01);
    }

    #[test]
    fn head_follow_turns_the_head_first() {
        let sk = face();
        let mut g = rig();
        g.head = Some(HeadFollow {
            chain: AimChain {
                joints: vec![1],
                shares: Vec::new(),
                limits: Vec::new(),
                aim_joint: 1,
                aim_axis: Vec3::NEG_Z,
            },
            weight: 1.0,
        });
        let target = Vec3::new(-2.0, 1.65, -2.0);
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(&sk, &mut pose, &g, &mut state, Some(target), 1.0, 0.016);
        assert!(
            pose.rotations[1].angle_between(Quat::IDENTITY) > 0.5,
            "head turned"
        );
        // With the head already facing the target, the eyes sit near their rest direction.
        assert!(r.angles.iter().all(|a| a.0.abs() < 0.1), "{:?}", r.angles);
    }

    #[test]
    fn channels_reach_sinks_by_name_and_by_index() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let r = solve_gaze(
            &sk,
            &mut pose,
            &rig(),
            &mut state,
            Some(Vec3::new(0.0, 2.6, -2.0)),
            1.0,
            0.016,
        );
        let mut named = HashMap::<String, f32>::new();
        r.write_morphs(&mut named);
        assert!(named["eyeLookUpLeft"] > 0.0);
        assert_eq!(named["eyeLookDownLeft"], 0.0);
        assert_eq!(named.len(), 8);

        let names: Vec<String> = ["jawOpen", "eyeLookUpLeft", "eyeLookUpRight"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let map = LidMorphMap::from_arkit_names(&names);
        assert_eq!(map.entries.len(), 2);
        let mut out = vec![(1u32, 0.9f32), (0, 0.4)];
        r.write_morphs(&mut IndexedMorphs {
            map: &map,
            out: &mut out,
        });
        assert_eq!(out.len(), 3, "existing entries replaced, new ones appended");
        assert!((out[0].1 - r.channels[0].up).abs() < 1e-6);
        assert_eq!(out[1], (0, 0.4), "unrelated morphs untouched");
    }

    #[test]
    fn from_skeleton_finds_eyes_and_lids_by_name() {
        let sk = face();
        let rig = GazeRig::from_skeleton(&sk).expect("eyes found");
        assert_eq!((rig.left.joint, rig.right.joint), (2, 3));
        assert_eq!(rig.lids.len(), 2);
        assert!(rig.lids.iter().any(|l| l.joint == 4 && l.side == EyeSide::Left));
        let no_eyes = Skeleton::from_local_bind(vec!["a".into()], vec![None], vec![LocalBind::default()])
            .expect("skeleton");
        assert!(GazeRig::from_skeleton(&no_eyes).is_none());
    }

    #[test]
    fn gaze_morphs_are_written_before_a_driver_can_replace_them() {
        use crate::curve::{Curve, CurveKey, CurveKeyKind};
        use crate::driver::{Driver, DriverBlend, DriverInput, DriverOutput, DriverSet, RigState};
        use crate::params::ParameterSet;

        let sk = face();
        let mut gaze = Gaze::new(rig());
        gaze.morphs = LidMorphMap {
            entries: vec![(EyeSide::Left, EyeChannel::Up, 3)],
        };
        let mut pose = sk.bind_pose_pose();
        let mut morphs = vec![0.0; 4];
        apply_gaze(
            &mut gaze,
            &sk,
            None,
            &mut pose,
            &mut morphs,
            &Transform::IDENTITY,
            Some(Vec3::new(0.0, 2.6, -2.0)),
            0.016,
        );
        let gazed = morphs[3];
        assert!(
            gazed > 0.0,
            "the up channel is written before drivers: {morphs:?}"
        );

        let names = ["a", "b", "c", "eyeLookUpLeft"].map(str::to_string).to_vec();
        let flat = |value: f32| {
            Curve::from(vec![
                CurveKey::new(0.0, value, CurveKeyKind::Linear),
                CurveKey::new(1.0, value, CurveKeyKind::Linear),
            ])
        };
        let driver = |name: &str, value: f32, blend: DriverBlend| Driver {
            name: name.into(),
            input: DriverInput::Param("unused".into()),
            curve: flat(value),
            output: DriverOutput::Morph("eyeLookUpLeft".into()),
            blend,
            weight: 1.0,
        };
        let clear = DriverSet {
            drivers: vec![driver("clear", 0.0, DriverBlend::Set)],
            ..DriverSet::default()
        };
        clear.compile(&sk, &names).expect("compiles").evaluate(
            &mut pose,
            &ParameterSet::new(),
            &mut morphs,
            &mut RigState::default(),
        );
        assert_eq!(morphs[3], 0.0, "Set replaces the gaze weight");

        morphs[3] = gazed;
        let lift = DriverSet {
            drivers: vec![driver("lift", 0.2, DriverBlend::Add)],
            ..DriverSet::default()
        };
        lift.compile(&sk, &names).expect("compiles").evaluate(
            &mut pose,
            &ParameterSet::new(),
            &mut morphs,
            &mut RigState::default(),
        );
        assert!((morphs[3] - (gazed + 0.2)).abs() < 1e-5, "{}", morphs[3]);
    }

    #[test]
    fn a_nan_weight_leaves_the_pose_finite() {
        let sk = face();
        let mut pose = sk.bind_pose_pose();
        let mut state = GazeState::default();
        let target = Vec3::new(-0.6, 1.8, -2.0);
        solve_gaze(&sk, &mut pose, &rig(), &mut state, Some(target), f32::NAN, 0.016);
        assert!(pose.rotations.iter().all(|r| r.is_finite()));
    }
}
