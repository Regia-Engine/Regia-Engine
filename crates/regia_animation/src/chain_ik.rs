// SPDX-License-Identifier: MPL-2.0
//! Iterative IK over joint chains of any length: FABRIK and CCD, with per-joint limits.
//!
//! Specification: `docs/specs/17-animation.md` § 3 (IK post-processes). Where
//! [`crate::ik::solve_two_bone`] is analytic for three-joint limbs, these solvers handle
//! spines, tails, tentacles, cables and multi-segment mechanical arms. Both operate on a
//! local [`Pose`] plus its [`Skeleton`], rotate only the chain's joints (the tip's own
//! rotation is left alone) and blend the result by `weight`.
//!
//! Joint limits ([`JointLimit`]) are expressed relative to each joint's **bind** local
//! rotation, split into swing and twist about the bone axis (the direction from the joint to
//! the next chain joint in the bind pose).
//!
//! Provenance (`docs/provenance.md`):
//! - FABRIK: Aristidou & Lasenby, "FABRIK: A fast, iterative solver for the Inverse
//!   Kinematics problem", Graphical Models 73(5), 2011 — backward/forward reaching passes over
//!   joint positions. Limits are enforced by converting the reached positions back to
//!   rotations and projecting each joint into its limit, plus one CCD sweep per iteration on
//!   constrained chains (our own formulation).
//! - CCD: Wang & Chen, "A combined optimization method for solving the inverse kinematics
//!   problem of mechanical manipulators", IEEE Trans. Robotics and Automation 7(4), 1991;
//!   surveyed in Aristidou & Lasenby, "Inverse Kinematics Techniques in Computer Graphics: A
//!   Survey", Computer Graphics Forum 2018.
//! - Swing–twist decomposition of a quaternion about an axis: Dobrowolski, "Swing-twist
//!   decomposition in Clifford algebra", arXiv:1506.05481 (2015).

use regia_core::math::{Mat4, Quat, Vec3, det};
use serde::{Deserialize, Serialize};

use crate::num::clamp_between;
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Rotation limits of one joint, relative to its bind local rotation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum JointLimit {
    /// No limit.
    #[default]
    Free,
    /// Ball joint: the bone may swing up to `max_swing` radians away from its bind direction
    /// and twist about itself within `[min_twist, max_twist]` radians.
    Cone {
        /// Maximum swing angle, radians.
        max_swing: f32,
        /// Minimum twist about the bone axis, radians.
        min_twist: f32,
        /// Maximum twist about the bone axis, radians.
        max_twist: f32,
    },
    /// Hinge: rotation only about `axis` (in the joint's bind local frame), within
    /// `[min, max]` radians (knees, elbows, mechanical joints).
    Hinge {
        /// Hinge axis in the joint's bind local frame.
        axis: Vec3,
        /// Minimum angle, radians.
        min: f32,
        /// Maximum angle, radians.
        max: f32,
    },
}

impl JointLimit {
    /// Project a local rotation into this limit. `bind` is the joint's bind local rotation and
    /// `bone_axis` the bone direction in the joint's own frame (the twist axis).
    pub fn constrain(&self, local: Quat, bind: Quat, bone_axis: Vec3) -> Quat {
        let offset = (bind.inverse() * local).normalize();
        let constrained = match *self {
            JointLimit::Free => return local,
            JointLimit::Cone {
                max_swing,
                min_twist,
                max_twist,
            } => {
                let axis = bone_axis.normalize_or(Vec3::Y);
                let (swing, twist) = swing_twist(offset, axis);
                // Authored limits may come in either order (or be NaN): never panic.
                let twist_angle = clamp_between(signed_angle_about(twist, axis), min_twist, max_twist);
                let swing = clamp_angle(
                    swing,
                    crate::num::finite_or(max_swing, std::f32::consts::PI).max(0.0),
                );
                swing * det::quat_from_axis_angle(axis, twist_angle)
            }
            JointLimit::Hinge { axis, min, max } => {
                let axis = axis.normalize_or(Vec3::X);
                let (_, twist) = swing_twist(offset, axis);
                let angle = clamp_between(signed_angle_about(twist, axis), min, max);
                det::quat_from_axis_angle(axis, angle)
            }
        };
        (bind * constrained).normalize()
    }
}

/// Split `q` into `swing * twist`, where `twist` rotates about `axis` (unit) and `swing`
/// rotates about an axis perpendicular to it.
pub fn swing_twist(q: Quat, axis: Vec3) -> (Quat, Quat) {
    let v = Vec3::new(q.x, q.y, q.z);
    let p = axis * v.dot(axis);
    let twist = Quat::from_xyzw(p.x, p.y, p.z, q.w);
    let twist = if twist.length_squared() < 1e-12 {
        // 180° swing: twist is undefined; pick identity.
        Quat::IDENTITY
    } else {
        twist.normalize()
    };
    let swing = (q * twist.inverse()).normalize();
    (swing, twist)
}

/// Signed rotation angle of `q` about `axis` (assumes `q` is a pure rotation about ±axis),
/// in `(-π, π]`.
fn signed_angle_about(q: Quat, axis: Vec3) -> f32 {
    let s = Vec3::new(q.x, q.y, q.z).dot(axis);
    let mut angle = 2.0 * det::atan2(s, q.w);
    if angle > std::f32::consts::PI {
        angle -= std::f32::consts::TAU;
    } else if angle < -std::f32::consts::PI {
        angle += std::f32::consts::TAU;
    }
    angle
}

/// Limit the rotation angle of `q` to `max` radians, keeping its axis.
fn clamp_angle(q: Quat, max: f32) -> Quat {
    let q = if q.w < 0.0 { -q } else { q };
    let angle = 2.0 * det::acos(q.w.clamp(-1.0, 1.0));
    if angle <= max {
        return q;
    }
    let axis = Vec3::new(q.x, q.y, q.z).normalize_or(Vec3::X);
    det::quat_from_axis_angle(axis, max)
}

/// An ordered joint chain, root first; each joint is the direct parent of the next.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JointChain {
    /// Joint ids, root to tip.
    pub joints: Vec<u32>,
    /// One limit per joint (same length as `joints`), or empty for an unconstrained chain.
    /// The tip's entry is unused (solvers never rotate the tip).
    pub limits: Vec<JointLimit>,
}

/// Errors building a [`JointChain`].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ChainError {
    /// A named joint does not exist in the skeleton.
    #[error("joint {0:?} not found in the skeleton")]
    MissingJoint(String),
    /// `tip` is not a descendant of `root`.
    #[error("joint {tip:?} is not a descendant of {root:?}")]
    NotDescendant {
        /// Requested chain root.
        root: String,
        /// Requested chain tip.
        tip: String,
    },
    /// A chain needs at least two joints.
    #[error("an IK chain needs at least two joints")]
    TooShort,
}

impl JointChain {
    /// The chain from `root` down to `tip` (inclusive) by following parents up from `tip`.
    ///
    /// # Errors
    ///
    /// [`ChainError`] when a joint is missing, `tip` is not under `root`, or the chain has
    /// fewer than two joints.
    pub fn between(skeleton: &Skeleton, root: &str, tip: &str) -> Result<Self, ChainError> {
        let root_id = skeleton
            .joint_index(root)
            .ok_or_else(|| ChainError::MissingJoint(root.to_string()))?;
        let tip_id = skeleton
            .joint_index(tip)
            .ok_or_else(|| ChainError::MissingJoint(tip.to_string()))?;
        let mut joints = vec![tip_id];
        let mut current = tip_id;
        while current != root_id {
            // A parent cycle (a skeleton that skipped validation) would never reach the root.
            if joints.len() > skeleton.parents.len() {
                return Err(ChainError::NotDescendant {
                    root: root.to_string(),
                    tip: tip.to_string(),
                });
            }
            match skeleton.parents.get(current as usize).copied().flatten() {
                Some(parent) => {
                    joints.push(parent);
                    current = parent;
                }
                None => {
                    return Err(ChainError::NotDescendant {
                        root: root.to_string(),
                        tip: tip.to_string(),
                    });
                }
            }
        }
        joints.reverse();
        if joints.len() < 2 {
            return Err(ChainError::TooShort);
        }
        Ok(JointChain {
            joints,
            limits: Vec::new(),
        })
    }

    /// Attach the same limit to every rotating joint of the chain.
    pub fn with_uniform_limit(mut self, limit: JointLimit) -> Self {
        self.limits = vec![limit; self.joints.len()];
        self
    }

    fn limit(&self, i: usize) -> JointLimit {
        self.limits.get(i).copied().unwrap_or_default()
    }

    fn valid(&self, skeleton: &Skeleton, pose: &Pose) -> bool {
        let n = skeleton.len().min(pose.len());
        self.joints.len() >= 2
            && self.joints.iter().all(|&j| (j as usize) < n)
            && self
                .joints
                .windows(2)
                // `parent_of`, not indexing: a skeleton that skipped validation may list fewer
                // parents than names, or a parent after its child, and that is an invalid
                // chain, not a panic (correctness audit, finding 3; deep-zai-animation 2).
                .all(|w| skeleton.parent_of(w[1] as usize) == Some(w[0] as usize))
    }
}

/// Iteration budget, tolerance and blend weight shared by the chain solvers.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChainIkSettings {
    /// Maximum solver iterations.
    pub iterations: u32,
    /// Stop when the tip is within this distance of the target, metres.
    pub tolerance: f32,
    /// Blend of the solved rotations over the input pose, `0..=1`.
    pub weight: f32,
}

impl Default for ChainIkSettings {
    fn default() -> Self {
        ChainIkSettings {
            iterations: 24,
            tolerance: 1e-3,
            weight: 1.0,
        }
    }
}

/// What a chain solve achieved.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChainIkReport {
    /// Distance from the (unblended) solved tip to the target, metres.
    pub error: f32,
    /// Iterations used.
    pub iterations: u32,
}

/// Which iterative solver a chain solve runs ([`solve_chain`], the graph's `ChainIK` node).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChainSolver {
    /// Forward and backward reaching over joint positions ([`solve_fabrik`]).
    #[default]
    Fabrik,
    /// Cyclic coordinate descent ([`solve_ccd`]).
    Ccd,
}

/// Buffers a chain solve reuses. Keep one between frames (the graph's `ChainIK` node keeps one
/// per rig) and a solve allocates nothing once the buffers have grown to the chain and skeleton.
#[derive(Clone, Debug, Default)]
pub struct ChainIkScratch {
    original: Vec<Quat>,
    positions: Vec<Vec3>,
    desired: Vec<Vec3>,
    lengths: Vec<f32>,
    globals: Vec<Mat4>,
    fk: Vec<Mat4>,
}

#[cfg(test)]
thread_local! {
    /// Joint transform compositions the chain solvers did on this thread (tests: the work of a
    /// solve is bounded by its joints and iterations, review-c finding 6).
    static COMPOSITIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count `n` joint transform compositions (a local transform built and multiplied into a
/// global); compiled out of non-test builds.
#[inline]
fn count_compositions(n: usize) {
    #[cfg(test)]
    COMPOSITIONS.with(|c| c.set(c.get() + n as u64));
    #[cfg(not(test))]
    let _ = n;
}

/// Rig-space globals of the chain's joints given the pose (the chain root's parent global is
/// `base`), written into `out`.
fn chain_globals_into(pose: &Pose, chain: &JointChain, base: Mat4, out: &mut Vec<Mat4>) {
    out.clear();
    let mut parent = base;
    count_compositions(chain.joints.len());
    for &j in &chain.joints {
        let j = j as usize;
        let local =
            Mat4::from_scale_rotation_translation(pose.scales[j], pose.rotations[j], pose.translations[j]);
        parent *= local;
        out.push(parent);
    }
}

fn base_of(skeleton: &Skeleton, pose: &Pose, chain: &JointChain, fk: &mut Vec<Mat4>) -> Mat4 {
    // A root parent past the list or after the root (unvalidated skeleton) is no parent.
    match skeleton.parent_of(chain.joints[0] as usize) {
        Some(p) => {
            skeleton.globals_into(pose, None, fk);
            fk.get(p).copied().unwrap_or(Mat4::IDENTITY)
        }
        None => Mat4::IDENTITY,
    }
}

fn rotation_of(m: Mat4) -> Quat {
    m.to_scale_rotation_translation().1.normalize()
}

/// Bone axis of chain joint `i` in its own frame: the bind translation of the next joint.
fn bone_axis(skeleton: &Skeleton, chain: &JointChain, i: usize) -> Vec3 {
    chain
        .joints
        .get(i + 1)
        .map(|&c| skeleton.bind_pose[c as usize].translation.normalize_or(Vec3::Y))
        .unwrap_or(Vec3::Y)
}

/// Rotate chain joint `i` so its global rotation becomes `new_global`, then apply its limit.
fn set_global_rotation(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    i: usize,
    parent_rotation: Quat,
    new_global: Quat,
) {
    let j = chain.joints[i] as usize;
    let local = (parent_rotation.inverse() * new_global).normalize();
    let bind = skeleton.bind_pose[j].rotation;
    pose.rotations[j] = chain
        .limit(i)
        .constrain(local, bind, bone_axis(skeleton, chain, i));
}

/// Turn desired joint positions into chain rotations (root first, preserving twist), with
/// limits applied; writes the positions the constrained chain actually reaches to `reached`.
fn apply_positions_into(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    base: Mat4,
    desired: &[Vec3],
    reached: &mut Vec<Vec3>,
) {
    let n = chain.joints.len();
    let mut parent = base;
    reached.clear();
    count_compositions(n);
    for i in 0..n {
        let j = chain.joints[i] as usize;
        let local =
            Mat4::from_scale_rotation_translation(pose.scales[j], pose.rotations[j], pose.translations[j]);
        let mut global = parent * local;
        let position = global.w_axis.truncate();
        if i + 1 < n {
            let c = chain.joints[i + 1] as usize;
            let child = global.transform_point3(pose.translations[c]);
            let from = (child - position).normalize_or_zero();
            let to = (desired[i + 1] - position).normalize_or_zero();
            if from.length_squared() > 0.5 && to.length_squared() > 0.5 {
                let new_global = (Quat::from_rotation_arc(from, to) * rotation_of(global)).normalize();
                set_global_rotation(skeleton, pose, chain, i, rotation_of(parent), new_global);
                global = parent
                    * Mat4::from_scale_rotation_translation(
                        pose.scales[j],
                        pose.rotations[j],
                        pose.translations[j],
                    );
            }
        }
        reached.push(position);
        parent = global;
    }
}

fn blend_back(pose: &mut Pose, chain: &JointChain, original: &[Quat], weight: f32) {
    let w = crate::num::unit_weight(weight);
    if w >= 1.0 {
        return;
    }
    for (k, &j) in chain.joints.iter().enumerate() {
        let j = j as usize;
        // Deterministic kernel, not glam's libm slerp (fixrev-06-k H2).
        pose.rotations[j] = det::quat_slerp(original[k], pose.rotations[j], w).normalize();
    }
}

/// One CCD sweep, tip side first: rotate each chain joint so the tip swings towards
/// `target`, applying limits per joint.
///
/// O(joints) per sweep (review-c finding 6, ADR-8735): turning joint `i` moves only joints
/// `i..`, so the globals of `0..=i` computed once at the start of the sweep stay exact for the
/// joints still to turn, and the tip relative to joint `i` is kept as the product of the
/// locals below it, extended by one joint per step (including the turns this sweep made).
/// `globals` holds the chain's globals from the start of the sweep afterwards.
fn ccd_sweep(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    base: Mat4,
    target: Vec3,
    globals: &mut Vec<Mat4>,
) {
    let n = chain.joints.len();
    chain_globals_into(pose, chain, base, globals);
    // The tip in the frame of joint `i`: the locals of joints `i + 1 ..= n - 1`.
    let mut below = Mat4::IDENTITY;
    count_compositions(n - 1);
    for i in (0..n - 1).rev() {
        let c = chain.joints[i + 1] as usize;
        below =
            Mat4::from_scale_rotation_translation(pose.scales[c], pose.rotations[c], pose.translations[c])
                * below;
        let position = globals[i].w_axis.truncate();
        let tip = globals[i].transform_point3(below.w_axis.truncate());
        let to_tip = (tip - position).normalize_or_zero();
        let to_target = (target - position).normalize_or_zero();
        if to_tip.length_squared() < 0.5 || to_target.length_squared() < 0.5 {
            continue;
        }
        let parent_rotation = if i == 0 {
            rotation_of(base)
        } else {
            rotation_of(globals[i - 1])
        };
        let new_global = (Quat::from_rotation_arc(to_tip, to_target) * rotation_of(globals[i])).normalize();
        set_global_rotation(skeleton, pose, chain, i, parent_rotation, new_global);
    }
}

/// The chain tip's rig-space position.
fn tip_position(pose: &Pose, chain: &JointChain, base: Mat4, globals: &mut Vec<Mat4>) -> Vec3 {
    chain_globals_into(pose, chain, base, globals);
    globals[chain.joints.len() - 1].w_axis.truncate()
}

/// FABRIK: move the chain's tip to `target` (rig space) by alternating backward and forward
/// reaching passes over joint positions, projecting through the joint limits after each
/// iteration. Chains with limits finish every iteration with one CCD sweep (the reaching
/// passes cannot change a joint's twist, which a hinge may need). Unreachable targets
/// stretch the chain straight towards them.
///
/// Invalid chains (joints out of range or not parent-linked) and non-finite targets leave the
/// pose unchanged and report `error = f32::INFINITY`. Allocates its buffers per call; a caller
/// that solves every frame uses [`solve_chain`] with a kept [`ChainIkScratch`].
pub fn solve_fabrik(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    target: Vec3,
    settings: ChainIkSettings,
) -> ChainIkReport {
    let mut scratch = ChainIkScratch::default();
    solve_chain(
        skeleton,
        pose,
        chain,
        target,
        None,
        ChainSolver::Fabrik,
        settings,
        &mut scratch,
    )
}

/// CCD (cyclic coordinate descent): from the joint nearest the tip back to the root, rotate
/// each joint so the tip swings towards `target` (rig space), applying limits per joint.
///
/// Invalid chains and non-finite targets leave the pose unchanged and report
/// `error = f32::INFINITY`. Allocates its buffers per call; see [`solve_chain`].
pub fn solve_ccd(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    target: Vec3,
    settings: ChainIkSettings,
) -> ChainIkReport {
    let mut scratch = ChainIkScratch::default();
    solve_chain(
        skeleton,
        pose,
        chain,
        target,
        None,
        ChainSolver::Ccd,
        settings,
        &mut scratch,
    )
}

/// Solve `chain` towards `target` (rig space) with `solver`, reusing `scratch`'s buffers.
///
/// With a `pole` (rig space), the solved chain is then turned about the line from its root to
/// its tip so its bend (the mean of its inner joints) faces the pole: the tip stays where the
/// solve put it, unless the root's limit stops the turn. A chain of two joints has no bend and
/// ignores the pole, as does a non-finite pole. The result is blended over the input pose by
/// `settings.weight`. [`solve_fabrik`] and [`solve_ccd`] are this call without a pole.
///
/// Invalid chains and non-finite targets leave the pose unchanged and report
/// `error = f32::INFINITY`.
#[allow(clippy::too_many_arguments)] // one solve: rig, pose, chain, goal, solver and buffers
pub fn solve_chain(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    target: Vec3,
    pole: Option<Vec3>,
    solver: ChainSolver,
    settings: ChainIkSettings,
    scratch: &mut ChainIkScratch,
) -> ChainIkReport {
    // A NaN target would turn the reaching passes' directions into the +Y fallback and
    // straighten the chain upwards.
    if !chain.valid(skeleton, pose) || !target.is_finite() {
        return ChainIkReport {
            error: f32::INFINITY,
            iterations: 0,
        };
    }
    scratch.original.clear();
    scratch
        .original
        .extend(chain.joints.iter().map(|&j| pose.rotations[j as usize]));
    let base = base_of(skeleton, pose, chain, &mut scratch.fk);
    let mut report = match solver {
        ChainSolver::Fabrik => {
            let (report, solved) = fabrik_passes(skeleton, pose, chain, base, target, settings, scratch);
            if !solved {
                // A chain of zero length: nothing moved, the input bits stay.
                return report;
            }
            report
        }
        ChainSolver::Ccd => ccd_passes(skeleton, pose, chain, base, target, settings, scratch),
    };
    if let Some(pole) = pole.filter(|p| p.is_finite()) {
        turn_towards_pole(skeleton, pose, chain, base, pole, &mut scratch.globals);
        report.error = (tip_position(pose, chain, base, &mut scratch.globals) - target).length();
    }
    blend_back(pose, chain, &scratch.original, settings.weight);
    keep_finite(pose, chain, &scratch.original, report)
}

/// The FABRIK iterations at full weight. The flag is false when the chain has no length (the
/// pose is untouched and the caller returns at once).
fn fabrik_passes(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    base: Mat4,
    target: Vec3,
    settings: ChainIkSettings,
    scratch: &mut ChainIkScratch,
) -> (ChainIkReport, bool) {
    let ChainIkScratch {
        positions,
        desired,
        lengths,
        globals,
        ..
    } = scratch;
    chain_globals_into(pose, chain, base, globals);
    positions.clear();
    positions.extend(globals.iter().map(|g| g.w_axis.truncate()));
    let n = positions.len();
    lengths.clear();
    lengths.extend(positions.windows(2).map(|w| (w[1] - w[0]).length()));
    let total: f32 = lengths.iter().sum();
    let root = positions[0];
    let constrained = chain.limits.iter().any(|l| *l != JointLimit::Free);
    let mut report = ChainIkReport {
        error: (positions[n - 1] - target).length(),
        iterations: 0,
    };
    if total < 1e-6 {
        return (report, false);
    }

    if (target - root).length() >= total {
        // Out of reach: straighten towards the target.
        let dir = (target - root).normalize_or(Vec3::Y);
        desired.clear();
        desired.resize(n, root);
        for i in 1..n {
            desired[i] = desired[i - 1] + dir * lengths[i - 1];
        }
        apply_positions_into(skeleton, pose, chain, base, desired, positions);
        report.iterations = 1;
    } else {
        for iteration in 0..settings.iterations {
            if report.error <= settings.tolerance {
                break;
            }
            // Backward: pin the tip on the target and walk towards the root.
            positions[n - 1] = target;
            for i in (0..n - 1).rev() {
                let dir = (positions[i] - positions[i + 1]).normalize_or(Vec3::Y);
                positions[i] = positions[i + 1] + dir * lengths[i];
            }
            // Forward: pin the root back in place and walk towards the tip.
            positions[0] = root;
            for i in 0..n - 1 {
                let dir = (positions[i + 1] - positions[i]).normalize_or(Vec3::Y);
                positions[i + 1] = positions[i] + dir * lengths[i];
            }
            // Rotations + limits; continue from what the constrained chain reaches.
            std::mem::swap(positions, desired);
            apply_positions_into(skeleton, pose, chain, base, desired, positions);
            if constrained {
                // Position-space passes cannot change a joint's twist, so a hinge whose plane
                // misses the target never converges; a CCD sweep rotates about any axis.
                ccd_sweep(skeleton, pose, chain, base, target, globals);
                chain_globals_into(pose, chain, base, globals);
                positions.clear();
                positions.extend(globals.iter().map(|g| g.w_axis.truncate()));
            }
            report.iterations = iteration + 1;
            report.error = (positions[n - 1] - target).length();
        }
    }
    report.error = (positions[n - 1] - target).length();
    (report, true)
}

/// The CCD iterations at full weight.
fn ccd_passes(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    base: Mat4,
    target: Vec3,
    settings: ChainIkSettings,
    scratch: &mut ChainIkScratch,
) -> ChainIkReport {
    let globals = &mut scratch.globals;
    let mut report = ChainIkReport {
        error: (tip_position(pose, chain, base, globals) - target).length(),
        iterations: 0,
    };
    for iteration in 0..settings.iterations {
        if report.error <= settings.tolerance {
            break;
        }
        ccd_sweep(skeleton, pose, chain, base, target, globals);
        report.iterations = iteration + 1;
        report.error = (tip_position(pose, chain, base, globals) - target).length();
    }
    report
}

/// Turn the chain about the line from its root to its tip so the mean of its inner joints
/// moves into the half-plane of `pole` (the chain's bend faces the pole). The root takes the
/// turn, through its limit.
fn turn_towards_pole(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &JointChain,
    base: Mat4,
    pole: Vec3,
    globals: &mut Vec<Mat4>,
) {
    let n = chain.joints.len();
    if n < 3 {
        return;
    }
    chain_globals_into(pose, chain, base, globals);
    let root = globals[0].w_axis.truncate();
    let axis = (globals[n - 1].w_axis.truncate() - root).normalize_or_zero();
    if axis.length_squared() < 0.5 {
        return;
    }
    let mut bend = Vec3::ZERO;
    for g in &globals[1..n - 1] {
        bend += g.w_axis.truncate();
    }
    bend = bend / (n - 2) as f32 - root;
    let toward = pole - root;
    let from = (bend - axis * bend.dot(axis)).normalize_or_zero();
    let to = (toward - axis * toward.dot(axis)).normalize_or_zero();
    if from.length_squared() < 0.5 || to.length_squared() < 0.5 {
        return;
    }
    // Both directions are perpendicular to the axis, so the turn is about the axis (sign from
    // the triple product), and the tip on the axis stays put.
    let angle = det::atan2(from.cross(to).dot(axis), from.dot(to));
    let turn = det::quat_from_axis_angle(axis, angle);
    let new_global = (turn * rotation_of(globals[0])).normalize();
    set_global_rotation(skeleton, pose, chain, 0, rotation_of(base), new_global);
}

/// A solve that wrote a non-finite rotation (a chain joint or the chain's parent scaled to zero
/// on an axis decomposes to a NaN rotation, audit-v035-numerics M2) is undone: the pose keeps
/// its input rotations and the report says the solve failed (`error = f32::INFINITY`).
fn keep_finite(
    pose: &mut Pose,
    chain: &JointChain,
    original: &[Quat],
    report: ChainIkReport,
) -> ChainIkReport {
    if chain
        .joints
        .iter()
        .all(|&j| pose.rotations[j as usize].is_finite())
    {
        return report;
    }
    for (k, &j) in chain.joints.iter().enumerate() {
        pose.rotations[j as usize] = original[k];
    }
    ChainIkReport {
        error: f32::INFINITY,
        iterations: report.iterations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::translated;

    /// A straight vertical chain of `n` joints, `segment` metres apart along +Y.
    fn column(n: usize, segment: f32) -> Skeleton {
        let names = (0..n).map(|i| format!("j{i}")).collect();
        let parents = (0..n).map(|i| i.checked_sub(1).map(|p| p as u32)).collect();
        let binds = (0..n)
            .map(|i| {
                translated(if i == 0 {
                    Vec3::ZERO
                } else {
                    Vec3::new(0.0, segment, 0.0)
                })
            })
            .collect();
        Skeleton::from_local_bind(names, parents, binds).expect("valid skeleton")
    }

    fn tip(skeleton: &Skeleton, pose: &Pose) -> Vec3 {
        skeleton.globals(pose, None)[skeleton.len() - 1].w_axis.truncate()
    }

    fn lengths(skeleton: &Skeleton, pose: &Pose) -> Vec<f32> {
        let g = skeleton.globals(pose, None);
        g.windows(2)
            .map(|w| (w[1].w_axis - w[0].w_axis).truncate().length())
            .collect()
    }

    #[test]
    fn fabrik_reaches_reachable_targets_within_a_millimetre() {
        let sk = column(5, 0.5); // reach 2 m
        let chain = JointChain::between(&sk, "j0", "j4").expect("chain");
        for target in [
            Vec3::new(1.0, 1.2, 0.3),
            Vec3::new(-0.6, 0.4, 0.9),
            Vec3::new(0.2, -1.5, 0.1),
        ] {
            let mut pose = sk.bind_pose_pose();
            let report = solve_fabrik(&sk, &mut pose, &chain, target, ChainIkSettings::default());
            assert!(report.error < 1e-3, "target {target}: error {}", report.error);
            assert!((tip(&sk, &pose) - target).length() < 1e-3);
            for l in lengths(&sk, &pose) {
                assert!((l - 0.5).abs() < 1e-4, "bones keep their length");
            }
        }
    }

    #[test]
    fn short_parent_list_makes_the_chain_invalid_not_a_panic() {
        // A skeleton built without validation (hand-constructed or deserialized without the
        // cooked-asset check) may list fewer parents than names; the solvers must report an
        // invalid chain, not panic indexing the short list (correctness audit, finding 3).
        let skeleton = Skeleton {
            joint_names: vec!["a".into(), "b".into(), "c".into()],
            parents: Vec::new(),
            bind_pose: vec![translated(Vec3::ZERO); 3],
            inverse_bind_matrices: Vec::new(),
            sockets: Vec::new(),
        };
        let chain = JointChain {
            joints: vec![0, 1],
            limits: Vec::new(),
        };
        let target = Vec3::new(1.0, 1.0, 0.0);
        let mut pose = Pose::from_identity(3);
        let before = pose.rotations.clone();
        let fabrik = solve_fabrik(&skeleton, &mut pose, &chain, target, ChainIkSettings::default());
        assert_eq!(fabrik.error, f32::INFINITY);
        assert_eq!(pose.rotations, before, "the pose stays unchanged");
        let ccd = solve_ccd(&skeleton, &mut pose, &chain, target, ChainIkSettings::default());
        assert_eq!(ccd.error, f32::INFINITY);
        assert_eq!(pose.rotations, before, "the pose stays unchanged");
    }

    #[test]
    fn ccd_reaches_reachable_targets_within_a_millimetre() {
        let sk = column(4, 0.5);
        let chain = JointChain::between(&sk, "j0", "j3").expect("chain");
        let target = Vec3::new(0.7, 0.9, -0.4);
        let mut pose = sk.bind_pose_pose();
        let settings = ChainIkSettings {
            iterations: 64,
            ..Default::default()
        };
        let report = solve_ccd(&sk, &mut pose, &chain, target, settings);
        assert!(report.error < 1e-3, "error {}", report.error);
    }

    #[test]
    fn unreachable_target_stretches_towards_it() {
        let sk = column(3, 1.0);
        let chain = JointChain::between(&sk, "j0", "j2").expect("chain");
        let target = Vec3::new(10.0, 0.0, 0.0);
        let mut pose = sk.bind_pose_pose();
        let report = solve_fabrik(&sk, &mut pose, &chain, target, ChainIkSettings::default());
        let t = tip(&sk, &pose);
        assert!((t - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-3, "tip {t}");
        assert!((report.error - 8.0).abs() < 1e-3);
    }

    #[test]
    fn hinge_limits_hold_and_restrict_the_solution() {
        let sk = column(3, 1.0);
        // Middle joint may only bend about +X, 0..90°.
        let mut chain = JointChain::between(&sk, "j0", "j2").expect("chain");
        chain.limits = vec![
            JointLimit::Free,
            JointLimit::Hinge {
                axis: Vec3::X,
                min: 0.0,
                max: std::f32::consts::FRAC_PI_2,
            },
            JointLimit::Free,
        ];
        for (name, solve) in [
            (
                "fabrik",
                solve_fabrik as fn(&Skeleton, &mut Pose, &JointChain, Vec3, ChainIkSettings) -> ChainIkReport,
            ),
            ("ccd", solve_ccd),
        ] {
            let mut pose = sk.bind_pose_pose();
            // |target| = 1.556 m needs a ~78° bend at j1, inside the 0..90° hinge.
            let target = Vec3::new(0.3, 1.3, 0.8);
            let settings = ChainIkSettings {
                iterations: 64,
                ..Default::default()
            };
            let report = solve(&sk, &mut pose, &chain, target, settings);
            assert!(
                report.error < 5e-3,
                "{name}: reachable within the hinge: {}",
                report.error
            );
            let mid = pose.rotations[1];
            let (axis, angle) = mid.to_axis_angle();
            assert!(
                angle < 1e-3 || (axis.cross(Vec3::X).length() < 1e-3),
                "only rotates about X: {axis} {angle}"
            );
            let signed = 2.0 * mid.x.atan2(mid.w);
            assert!(
                (-1e-3..=std::f32::consts::FRAC_PI_2 + 1e-3).contains(&signed),
                "{signed}"
            );
        }
    }

    #[test]
    fn cone_limit_clamps_swing_and_twist() {
        let limit = JointLimit::Cone {
            max_swing: 0.5,
            min_twist: -0.2,
            max_twist: 0.2,
        };
        let local = Quat::from_rotation_x(1.2) * Quat::from_rotation_y(0.9);
        let out = limit.constrain(local, Quat::IDENTITY, Vec3::Y);
        let (swing, twist) = swing_twist(out, Vec3::Y);
        let swing_angle = swing.angle_between(Quat::IDENTITY);
        let twist_angle = signed_angle_about(twist, Vec3::Y);
        // The input is outside both limits, so the projection sits on the boundary of each.
        assert!((swing_angle - 0.5).abs() < 1e-3, "swing {swing_angle}");
        assert!((twist_angle.abs() - 0.2).abs() < 1e-3, "twist {twist_angle}");
        // A rotation already inside the limit is untouched.
        let inside = Quat::from_rotation_x(0.3);
        assert!(
            limit
                .constrain(inside, Quat::IDENTITY, Vec3::Y)
                .angle_between(inside)
                < 1e-5
        );
    }

    #[test]
    fn weight_zero_leaves_pose_and_half_weight_is_between() {
        let sk = column(4, 0.5);
        let chain = JointChain::between(&sk, "j0", "j3").expect("chain");
        let target = Vec3::new(1.0, 0.8, 0.0);
        let bind = sk.bind_pose_pose();
        let mut pose = bind.clone();
        solve_fabrik(
            &sk,
            &mut pose,
            &chain,
            target,
            ChainIkSettings {
                weight: 0.0,
                ..Default::default()
            },
        );
        assert_eq!(pose, bind);
        let mut half = bind.clone();
        solve_fabrik(
            &sk,
            &mut half,
            &chain,
            target,
            ChainIkSettings {
                weight: 0.5,
                ..Default::default()
            },
        );
        let start = (tip(&sk, &bind) - target).length();
        let mid = (tip(&sk, &half) - target).length();
        assert!(mid < start && mid > 1e-3, "{start} {mid}");
    }

    #[test]
    fn chain_between_validates() {
        let sk = column(3, 1.0);
        assert!(matches!(
            JointChain::between(&sk, "j2", "j0"),
            Err(ChainError::NotDescendant { .. })
        ));
        assert!(matches!(
            JointChain::between(&sk, "j0", "nope"),
            Err(ChainError::MissingJoint(_))
        ));
        assert_eq!(JointChain::between(&sk, "j1", "j1"), Err(ChainError::TooShort));
    }

    #[test]
    fn reversed_or_nan_limits_constrain_without_panicking() {
        // Authored limits can arrive min > max (a sign typo) or NaN; `f32::clamp` panicked.
        let reversed = JointLimit::Hinge {
            axis: Vec3::X,
            min: std::f32::consts::FRAC_PI_2,
            max: 0.0,
        };
        let out = reversed.constrain(Quat::from_rotation_x(2.0), Quat::IDENTITY, Vec3::Y);
        let angle = 2.0 * out.x.atan2(out.w);
        assert!((angle - std::f32::consts::FRAC_PI_2).abs() < 1e-4, "{angle}");
        let cone = JointLimit::Cone {
            max_swing: f32::NAN,
            min_twist: 0.2,
            max_twist: -0.2,
        };
        let out = cone.constrain(Quat::from_rotation_y(1.0), Quat::IDENTITY, Vec3::Y);
        let (_, twist) = swing_twist(out, Vec3::Y);
        assert!(signed_angle_about(twist, Vec3::Y).abs() <= 0.2 + 1e-4);
        let nan_hinge = JointLimit::Hinge {
            axis: Vec3::X,
            min: f32::NAN,
            max: f32::NAN,
        };
        assert!(
            nan_hinge
                .constrain(Quat::from_rotation_x(0.5), Quat::IDENTITY, Vec3::Y)
                .is_finite()
        );
        // The solvers run with such limits too.
        let sk = column(3, 1.0);
        let mut chain = JointChain::between(&sk, "j0", "j2").expect("chain");
        chain.limits = vec![reversed, reversed, JointLimit::Free];
        let mut pose = sk.bind_pose_pose();
        let report = solve_fabrik(
            &sk,
            &mut pose,
            &chain,
            Vec3::new(0.3, 1.3, 0.8),
            ChainIkSettings::default(),
        );
        assert!(report.error.is_finite());
    }

    #[test]
    fn non_finite_targets_leave_the_chain_alone() {
        let sk = column(4, 0.5);
        let chain = JointChain::between(&sk, "j0", "j3").expect("chain");
        let bind = sk.bind_pose_pose();
        for target in [Vec3::new(f32::NAN, 0.5, 0.0), Vec3::splat(f32::INFINITY)] {
            for solve in [
                solve_fabrik as fn(&Skeleton, &mut Pose, &JointChain, Vec3, ChainIkSettings) -> ChainIkReport,
                solve_ccd,
            ] {
                let mut pose = bind.clone();
                let report = solve(&sk, &mut pose, &chain, target, ChainIkSettings::default());
                // FABRIK used to straighten the chain along its +Y fallback direction.
                assert_eq!(pose, bind, "{target}");
                assert_eq!(report.error, f32::INFINITY);
            }
        }
    }

    /// Joint transform compositions of one `solve_chain` call.
    fn compositions_of_one_solve(joints: usize, solver: ChainSolver, limited: bool) -> u64 {
        let sk = column(joints, 0.1);
        let mut chain = JointChain::between(&sk, "j0", &format!("j{}", joints - 1)).expect("chain");
        if limited {
            chain = chain.with_uniform_limit(JointLimit::Cone {
                max_swing: 0.05,
                min_twist: 0.0,
                max_twist: 0.0,
            });
        }
        let mut pose = sk.bind_pose_pose();
        let mut scratch = ChainIkScratch::default();
        // Behind the root and inside the reach: the limited chain cannot fold there, the free
        // one never settles within a zero tolerance, so every solve runs to the cap.
        let target = Vec3::new(0.05, 0.02 * joints as f32, 0.05);
        let settings = ChainIkSettings {
            iterations: 32,
            tolerance: 0.0,
            weight: 1.0,
        };
        let before = COMPOSITIONS.with(std::cell::Cell::get);
        let report = solve_chain(
            &sk,
            &mut pose,
            &chain,
            target,
            None,
            solver,
            settings,
            &mut scratch,
        );
        assert_eq!(report.iterations, 32, "{solver:?} {joints} joints ran to the cap");
        COMPOSITIONS.with(std::cell::Cell::get) - before
    }

    /// Review-c finding 6 (ADR-8735): a CCD sweep recomputed the chain's globals once per joint,
    /// so a solve was O(iterations x joints²) and a long cable at the iteration cap cost about a
    /// million compositions per frame. A sweep is now O(joints): the work of a solve at the cap
    /// is linear in the chain length (doubling the chain at most doubles the work, plus slack)
    /// and at most four compositions per joint and iteration, for CCD and for limited FABRIK
    /// (which runs one CCD sweep per iteration). Counted, never timed.
    #[test]
    fn chain_solve_work_is_linear_in_the_chain_length() {
        for (solver, limited) in [
            (ChainSolver::Ccd, false),
            (ChainSolver::Ccd, true),
            (ChainSolver::Fabrik, true),
        ] {
            let short = compositions_of_one_solve(32, solver, limited);
            let long = compositions_of_one_solve(64, solver, limited);
            let longer = compositions_of_one_solve(128, solver, limited);
            for (joints, work) in [(32u64, short), (64, long), (128, longer)] {
                assert!(
                    work <= 4 * 32 * joints + 4 * joints,
                    "{solver:?} limited {limited}: {joints} joints took {work} compositions over 32 iterations"
                );
            }
            assert!(
                long * 100 <= short * 210 && longer * 100 <= long * 210,
                "{solver:?} limited {limited}: work grows faster than the chain: {short} / {long} / {longer}"
            );
        }
    }
}
