// SPDX-License-Identifier: MPL-2.0
//! Analytic IK post-processes: two-bone IK and look-at.
//!
//! Specification: `docs/specs/17-animation.md` § 3. Solvers operate on a local [`Pose`] plus
//! its [`Skeleton`], computing forward kinematics internally and writing back adjusted local
//! rotations. Chains are assumed to have unit scale (the standard case for imported rigs).
//!
//! Provenance: analytic two-bone solve (law of cosines triangle solve in the plane defined by
//! the pole vector), a standard technique surveyed in Aristidou & Lasenby, "Inverse Kinematics
//! Techniques in Computer Graphics: A Survey" (2011); aim-with-limits derived from quaternion
//! arc rotation (`Quat::from_rotation_arc`). See `docs/provenance.md`.

use regia_core::math::{Mat4, Quat, Vec3, det};
use serde::{Deserialize, Serialize};

use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// A three-joint IK chain: root, mid, tip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IkChain {
    /// Chain root joint id (shoulder/hip).
    pub root: u32,
    /// Middle joint id (elbow/knee).
    pub mid: u32,
    /// End joint id (hand/foot).
    pub tip: u32,
}

/// Rotate `pose` so the chain's tip reaches `target` (object space), bending in the plane
/// implied by `pole` (object space; `None` preserves the current bend direction). `weight`
/// in `0..=1` blends the result with the input pose.
///
/// Unreachable targets clamp to the maximum reach along the target direction. Degenerate
/// chains (zero-length bones), non-finite targets and joints outside the pose or skeleton are
/// left unchanged; a non-finite pole is ignored.
pub fn solve_two_bone(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &IkChain,
    target: Vec3,
    pole: Option<Vec3>,
    weight: f32,
) {
    let mut globals = Vec::new();
    solve_two_bone_with_globals(skeleton, pose, chain, target, pole, weight, &mut globals);
}

/// [`solve_two_bone`] using a caller-owned forward-kinematics buffer.
///
/// `globals` is refilled from the starting pose (every joint), then the chain is solved. A
/// buffer that already fits the skeleton is reused, so a second solve of the same rig does not
/// allocate another joint-matrix vector.
pub fn solve_two_bone_into(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &IkChain,
    target: Vec3,
    pole: Option<Vec3>,
    weight: f32,
    globals: &mut Vec<Mat4>,
) {
    skeleton.globals_into(pose, None, globals);
    solve_two_bone_with_globals(skeleton, pose, chain, target, pole, weight, globals);
}

/// [`solve_two_bone`] using `globals` for the chain's forward kinematics.
///
/// `globals` is resized to the skeleton on the first call and then overwritten only along
/// the chain (the tip and its ancestors). A second solve in the same buffer sees pose
/// writes from the first.
pub(crate) fn solve_two_bone_with_globals(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &IkChain,
    target: Vec3,
    pole: Option<Vec3>,
    weight: f32,
    globals: &mut Vec<Mat4>,
) {
    let (root, mid, tip) = (chain.root as usize, chain.mid as usize, chain.tip as usize);
    let len = pose.len().min(skeleton.len());
    if root >= len || mid >= len || tip >= len || !target.is_finite() {
        return;
    }
    // The solve writes the mid joint's local relative to the root, so the chain must be
    // contiguous (root -> mid -> tip); anything between would fold the limb.
    if skeleton.parents.get(mid).copied().flatten() != Some(chain.root)
        || skeleton.parents.get(tip).copied().flatten() != Some(chain.mid)
    {
        return;
    }
    let weight = crate::num::unit_weight(weight);
    if weight <= 0.0 {
        return;
    }
    let pole = pole.filter(|p| p.is_finite());
    refresh_global(skeleton, pose, tip, globals);
    let a = globals[root].w_axis.truncate(); // chain root position
    let b = globals[mid].w_axis.truncate(); // mid
    let c = globals[tip].w_axis.truncate(); // tip
    let len_ab = (b - a).length();
    let len_bc = (c - b).length();
    if !(len_ab.is_finite() && len_bc.is_finite()) || len_ab < 1e-6 || len_bc < 1e-6 {
        return;
    }

    // Clamp the target to the reachable annulus. The margins keep the triangle solve away from
    // the straight and folded singularities; they shrink with the bones so a chain shorter
    // than the margins (a 0.05 mm helper bone) still has a valid range.
    let to_target = target - a;
    let dist_raw = to_target.length();
    if dist_raw < 1e-6 {
        return;
    }
    let margin = 1e-4f32.min(0.25 * len_ab.min(len_bc));
    let max_reach = len_ab + len_bc - margin;
    let min_reach = ((len_ab - len_bc).abs() + margin).min(max_reach);
    let dist = dist_raw.clamp(min_reach, max_reach);
    let dir = to_target / dist_raw;
    let clamped_target = a + dir * dist;

    // Triangle solve (law of cosines): `x` is the distance along a→target to the
    // perpendicular foot of the mid joint; `h` is the bend offset in the pole plane.
    let x = (len_ab * len_ab - len_bc * len_bc + dist * dist) / (2.0 * dist);
    let h = (len_ab * len_ab - x * x).max(0.0).sqrt();
    let foot = a + dir * x;
    let bend = bend_direction(a, b, dir, pole);
    let new_b = foot + bend * h;

    // Object-space reasoning: compute both new *global* rotations from the pre-solve
    // globals, then convert to locals relative to their parents (the root's new rotation
    // included for the mid joint).
    let rotation_of = rotation_of_matrix;
    // A joint or parent scaled to zero on an axis has no rotation to solve from.
    let (Some(root_global), Some(mid_global)) = (rotation_of(globals[root]), rotation_of(globals[mid]))
    else {
        return;
    };
    // A parent that is not an earlier joint (malformed skeleton) is a root, as in `globals`.
    let root_parent_global = match skeleton.parents.get(root).copied().flatten() {
        Some(p) if (p as usize) < root => match rotation_of(globals[p as usize]) {
            Some(rotation) => rotation,
            None => return,
        },
        _ => Quat::IDENTITY,
    };

    // Root: aim its bone direction (a→b) at new_b, preserving twist.
    let root_bone_dir = root_global.inverse() * (b - a).normalize_or_zero();
    let desired_dir = (new_b - a).normalize_or_zero();
    let new_root_global = arc_to(root_global, root_bone_dir, desired_dir);

    // Mid: aim (b→c) at the clamped target, relative to the *new* root global rotation.
    let mid_bone_dir = mid_global.inverse() * (c - b).normalize_or_zero();
    let mut tip_dir = (clamped_target - new_b).normalize_or_zero();
    if tip_dir.length_squared() < 0.5 {
        tip_dir = dir;
    }
    let new_mid_global = arc_to(mid_global, mid_bone_dir, tip_dir);

    // Blend by weight, then write back locals. The mid's parent rotation is the *blended*
    // root global so partial weights stay consistent.
    let blended_root = det::quat_slerp(root_global, new_root_global, weight);
    pose.rotations[root] = (root_parent_global.inverse() * blended_root).normalize();
    pose.rotations[mid] =
        (blended_root.inverse() * det::quat_slerp(mid_global, new_mid_global, weight)).normalize();
}

/// Yaw and pitch limits, plus the blend weight, for [`solve_look_at`].
#[derive(Clone, Copy, Debug)]
pub struct LookAtSettings {
    /// Maximum yaw from the bind rotation, radians.
    pub max_yaw: f32,
    /// Maximum pitch from the bind rotation, radians.
    pub max_pitch: f32,
    /// Blend of the aimed rotation, `0..=1`.
    pub weight: f32,
}

/// Rotate a joint so its local −Z (Regia Engine forward) aims at `target` (object space), with yaw
/// and pitch clamped relative to the joint's bind local rotation. `settings.weight` blends
/// the result.
///
/// The solve uses the joint's object-space orientation, so a turret mounted on a rotated
/// parent still aims in world (object) space. Limits are applied in the joint's local frame;
/// their sign is ignored (a limit is a magnitude) and a non-finite limit means no limit. A
/// non-finite target leaves the pose unchanged.
pub fn solve_look_at(
    skeleton: &Skeleton,
    pose: &mut Pose,
    joint: u32,
    bind_rotation: Quat,
    target: Vec3,
    settings: LookAtSettings,
) {
    let mut globals = Vec::new();
    solve_look_at_with_globals(
        skeleton,
        pose,
        joint,
        bind_rotation,
        target,
        settings,
        &mut globals,
    );
}

/// [`solve_look_at`] using a caller-owned forward-kinematics buffer.
///
/// `globals` is refilled from the starting pose (every joint), then the joint is aimed. A buffer
/// that already fits the skeleton is reused.
pub fn solve_look_at_into(
    skeleton: &Skeleton,
    pose: &mut Pose,
    joint: u32,
    bind_rotation: Quat,
    target: Vec3,
    settings: LookAtSettings,
    globals: &mut Vec<Mat4>,
) {
    skeleton.globals_into(pose, None, globals);
    solve_look_at_with_globals(skeleton, pose, joint, bind_rotation, target, settings, globals);
}

/// [`solve_look_at`] using `globals` for the joint's forward kinematics.
///
/// `globals` is resized to the skeleton on the first call and then overwritten only along
/// the joint's ancestor chain.
pub(crate) fn solve_look_at_with_globals(
    skeleton: &Skeleton,
    pose: &mut Pose,
    joint: u32,
    bind_rotation: Quat,
    target: Vec3,
    settings: LookAtSettings,
    globals: &mut Vec<Mat4>,
) {
    let limit = |l: f32| if l.is_nan() { f32::INFINITY } else { l.abs() };
    let max_yaw = limit(settings.max_yaw);
    let max_pitch = limit(settings.max_pitch);
    let weight = crate::num::unit_weight(settings.weight);
    let joint = joint as usize;
    if joint >= pose.len() || joint >= skeleton.len() || !target.is_finite() || weight <= 0.0 {
        return;
    }
    refresh_global(skeleton, pose, joint, globals);
    let position = globals[joint].w_axis.truncate();
    let to_target = target - position;
    if to_target.length() < 1e-6 {
        return;
    }
    let desired_dir = to_target.normalize();
    let Some(current_global) = rotation_of_matrix(globals[joint]) else {
        return;
    };
    // A parent that is not an earlier joint (malformed skeleton) is a root, as in `globals`.
    let parent_global = match skeleton.parents.get(joint).copied().flatten() {
        Some(p) if (p as usize) < joint => match rotation_of_matrix(globals[p as usize]) {
            Some(rotation) => rotation,
            None => return,
        },
        _ => Quat::IDENTITY,
    };
    let aim = current_global * Vec3::NEG_Z;
    let swung = if aim.dot(desired_dir) > 0.999_999 {
        current_global
    } else {
        let swing = Quat::from_rotation_arc(aim, desired_dir);
        (swing * current_global).normalize()
    };

    // Clamp yaw/pitch of the local rotation relative to the bind local rotation.
    let new_local = parent_global.inverse() * swung;
    let offset = bind_rotation.inverse() * new_local;
    let (yaw, pitch, roll) = det::quat_to_euler_yxz(offset);
    let clamped_local = bind_rotation
        * det::quat_from_euler_yxz(
            yaw.clamp(-max_yaw, max_yaw),
            pitch.clamp(-max_pitch, max_pitch),
            roll,
        );
    let clamped_global = parent_global * clamped_local;
    let blended = det::quat_slerp(current_global, clamped_global, weight);
    pose.rotations[joint] = (parent_global.inverse() * blended).normalize();
}

/// Direction, perpendicular to `dir`, in which the mid joint bends.
///
/// The pole's component orthogonal to the target direction defines the plane. A pole parallel
/// to the chain falls back to the current bend, then to an arbitrary perpendicular.
fn bend_direction(root: Vec3, current_mid: Vec3, dir: Vec3, pole: Option<Vec3>) -> Vec3 {
    if let Some(p) = pole {
        let to_pole = p - root;
        let perp = to_pole - dir * to_pole.dot(dir);
        if perp.length() > 1e-6 {
            return perp.normalize();
        }
    }
    let current = current_mid - root;
    let perp = current - dir * current.dot(dir);
    if perp.length() > 1e-6 {
        return perp.normalize();
    }
    let up = if dir.y.abs() < 0.9 { Vec3::Y } else { Vec3::X };
    dir.cross(up).normalize()
}

/// The rotation of a joint matrix; `None` when a scale axis is zero (the decomposed quaternion
/// is NaN, audit-v035-numerics M2) or the matrix is not finite, so the solve leaves the pose
/// unchanged.
fn rotation_of_matrix(m: Mat4) -> Option<Quat> {
    let rotation = m.to_scale_rotation_translation().1;
    rotation.is_finite().then_some(rotation)
}

/// Global rotation that carries the unit vector `from_local` (expressed in `base`'s local
/// frame) onto `to_global`.
fn arc_to(base: Quat, from_local: Vec3, to_global: Vec3) -> Quat {
    let from_global = base * from_local;
    let swing = Quat::from_rotation_arc(from_global, to_global);
    (swing * base).normalize()
}

/// Write the object-space matrix of `joint` and its ancestors into `globals`.
///
/// `globals` grows to the skeleton length once. Joints off the chain are left as they were.
/// A parent that is not an earlier joint is treated as a root, matching [`Skeleton::globals`]
/// with no base transform. Chains deeper than 64 joints fall back to composing every joint
/// up to `joint` (parents are earlier indices).
pub(crate) fn refresh_global(skeleton: &Skeleton, pose: &Pose, joint: usize, globals: &mut Vec<Mat4>) {
    let n = skeleton.len().min(pose.len());
    if joint >= n {
        return;
    }
    if globals.len() < skeleton.len() {
        if globals.capacity() < skeleton.len() {
            crate::hot_count::count_globals_fill();
        }
        globals.resize(skeleton.len(), Mat4::IDENTITY);
    }
    let local_of = |index: usize| {
        Mat4::from_scale_rotation_translation(
            pose.scales[index],
            pose.rotations[index],
            pose.translations[index],
        )
    };
    let parent_of = |index: usize| match skeleton.parents.get(index).copied().flatten() {
        Some(parent) if (parent as usize) < index => Some(parent as usize),
        _ => None,
    };
    let mut chain = [0usize; 64];
    let mut depth = 0usize;
    let mut cursor = joint;
    let mut deep = false;
    loop {
        if depth == chain.len() {
            deep = true;
            break;
        }
        chain[depth] = cursor;
        depth += 1;
        match parent_of(cursor) {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    if deep {
        for index in 0..=joint {
            let local = local_of(index);
            globals[index] = match parent_of(index) {
                Some(parent) => globals[parent] * local,
                None => local,
            };
        }
        return;
    }
    let mut parent = Mat4::IDENTITY;
    for slot in (0..depth).rev() {
        let index = chain[slot];
        let global = parent * local_of(index);
        globals[index] = global;
        parent = global;
    }
}

/// Object-space position of a joint under the current pose (helper for tests and callers
/// that need to position targets). A joint outside the skeleton is at the origin.
pub fn joint_position(skeleton: &Skeleton, pose: &Pose, joint: u32) -> Vec3 {
    let globals = skeleton.globals(pose, None);
    globals
        .get(joint as usize)
        .map_or(Vec3::ZERO, |m| m.w_axis.truncate())
}

/// Object-space translation of `joint` without a heap allocation.
///
/// Chains deeper than 64 joints fall back to [`joint_position`]. A joint outside the pose is
/// the origin.
pub(crate) fn joint_object_point(skeleton: &Skeleton, pose: &Pose, joint: u32) -> Vec3 {
    let n = skeleton.len().min(pose.len());
    let joint = joint as usize;
    if joint >= n {
        return Vec3::ZERO;
    }
    let mut chain = [0usize; 64];
    let mut depth = 0usize;
    let mut cursor = joint;
    loop {
        if depth == chain.len() {
            return joint_position(skeleton, pose, joint as u32);
        }
        chain[depth] = cursor;
        depth += 1;
        match skeleton.parents.get(cursor).copied().flatten() {
            Some(parent) if (parent as usize) < cursor => cursor = parent as usize,
            _ => break,
        }
    }
    let mut parent = Mat4::IDENTITY;
    for slot in (0..depth).rev() {
        let index = chain[slot];
        let local = Mat4::from_scale_rotation_translation(
            pose.scales[index],
            pose.rotations[index],
            pose.translations[index],
        );
        parent *= local;
    }
    parent.w_axis.truncate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::translated;
    use crate::skeleton::LocalBind;

    fn arm_skeleton() -> Skeleton {
        // Body at the origin; shoulder 1 m up; elbow and hand 0.4 m apart along -Z.
        // Topological order: parents first (Body, Shoulder, Elbow, Hand).
        let names: Vec<String> = ["Body", "Shoulder", "Elbow", "Hand"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parents = vec![None, Some(0), Some(1), Some(2)];
        let bind = vec![
            LocalBind::default(),
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(0.0, 0.0, -0.4)),
            translated(Vec3::new(0.0, 0.0, -0.4)),
        ];
        Skeleton::from_local_bind(names, parents, bind).expect("valid")
    }

    #[test]
    fn two_bone_ik_reaches_target_within_1mm() {
        let sk = arm_skeleton();
        let mut pose = sk.bind_pose_pose();
        let chain = IkChain {
            root: 1,
            mid: 2,
            tip: 3,
        };
        // Reachable target: within 0.8 m of the shoulder.
        let target = Vec3::new(0.5, 0.9, -0.4);
        solve_two_bone(
            &sk,
            &mut pose,
            &chain,
            target,
            Some(Vec3::new(0.0, 2.0, 0.0)),
            1.0,
        );
        let globals = sk.globals(&pose, None);
        let tip = globals[3].w_axis.truncate();
        assert!(
            (tip - target).length() < 1e-3,
            "tip {tip} target {target} dist {}",
            (tip - target).length()
        );
    }

    #[test]
    fn two_bone_ik_ignores_non_contiguous_chain() {
        let sk = arm_skeleton();
        let mut pose = sk.bind_pose_pose();
        let before = pose.clone();
        // Hand's parent is Elbow, not the declared root (Shoulder).
        let chain = IkChain {
            root: 1,
            mid: 3,
            tip: 2,
        };
        solve_two_bone(&sk, &mut pose, &chain, Vec3::new(0.5, 0.9, -0.4), None, 1.0);
        assert_eq!(pose.rotations, before.rotations);
    }

    #[test]
    fn two_bone_ik_clamps_unreachable_targets() {
        let sk = arm_skeleton();
        let mut pose = sk.bind_pose_pose();
        let chain = IkChain {
            root: 1,
            mid: 2,
            tip: 3,
        };
        let target = Vec3::new(5.0, 1.0, 0.0);
        solve_two_bone(
            &sk,
            &mut pose,
            &chain,
            target,
            Some(Vec3::new(0.0, 2.0, 0.0)),
            1.0,
        );
        let globals = sk.globals(&pose, None);
        let shoulder = globals[1].w_axis.truncate();
        let tip = globals[3].w_axis.truncate();
        let dist = (tip - shoulder).length();
        assert!(
            (dist - 0.8).abs() < 2e-3,
            "fully extended arm should reach 0.8 m, got {dist}"
        );
    }

    #[test]
    fn two_bone_ik_partial_weight_blends() {
        let sk = arm_skeleton();
        let mut pose = sk.bind_pose_pose();
        let before = sk.globals(&pose, None)[3].w_axis.truncate();
        let chain = IkChain {
            root: 1,
            mid: 2,
            tip: 3,
        };
        let target = Vec3::new(0.0, 1.0, -0.6);
        solve_two_bone(&sk, &mut pose, &chain, target, None, 0.5);
        let after = sk.globals(&pose, None)[3].w_axis.truncate();
        let full_target_dist = (before - target).length();
        let moved = (before - after).length();
        assert!(moved > 1e-4, "weight 0.5 must move the tip");
        assert!(moved < full_target_dist, "weight 0.5 must not fully reach");
    }

    #[test]
    fn look_at_aims_within_limits() {
        let sk = arm_skeleton();
        let mut pose = sk.bind_pose_pose();
        // The shoulder's bind aims its -Z at (0, 1, -inf); aim straight ahead.
        let target = Vec3::new(0.0, 1.0, -5.0);
        solve_look_at(
            &sk,
            &mut pose,
            1,
            sk.bind_pose[1].rotation,
            target,
            LookAtSettings {
                max_yaw: 1.0,
                max_pitch: 1.0,
                weight: 1.0,
            },
        );
        let globals = sk.globals(&pose, None);
        let pos = globals[1].w_axis.truncate();
        let dir = (target - pos).normalize();
        let aim = pose_to_global_rot(&sk, &pose, 1) * Vec3::NEG_Z;
        assert!(aim.dot(dir) > 0.999, "aim {aim} dir {dir}");
    }

    #[test]
    fn look_at_clamps_beyond_limits() {
        let sk = arm_skeleton();
        let mut pose = sk.bind_pose_pose();
        // Target far to the side; clamp yaw to 0.3 rad.
        let target = Vec3::new(10.0, 1.0, -0.01);
        solve_look_at(
            &sk,
            &mut pose,
            1,
            sk.bind_pose[1].rotation,
            target,
            LookAtSettings {
                max_yaw: 0.3,
                max_pitch: 0.3,
                weight: 1.0,
            },
        );
        let global_rot = pose_to_global_rot(&sk, &pose, 1);
        let offset = sk.bind_pose[1].rotation.inverse() * global_rot;
        let (yaw, _, _) = offset.to_euler(regia_core::glam::EulerRot::YXZ);
        let (_, pitch, _) = offset.to_euler(regia_core::glam::EulerRot::YXZ);
        // The target is far off to the side, so the yaw must sit on the limit (not stay at the
        // bind aim, and not overshoot it).
        assert!(
            (yaw.abs() - 0.3).abs() < 1e-3,
            "yaw {yaw} must sit on the 0.3 limit"
        );
        assert!(pitch.abs() <= 0.3 + 1e-3, "pitch {pitch} exceeds limit");
    }

    fn pose_to_global_rot(sk: &Skeleton, pose: &Pose, joint: usize) -> Quat {
        let globals = sk.globals(pose, None);
        let (_, r, _) = globals[joint].to_scale_rotation_translation();
        r
    }

    #[test]
    fn a_chain_shorter_than_the_reach_margins_does_not_panic() {
        // A 0.05 mm helper bone as the tip: the old 0.1 mm margins made `min > max` and
        // `f32::clamp` panicked.
        let names: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let sk = Skeleton::from_local_bind(
            names,
            vec![None, Some(0), Some(1)],
            vec![
                LocalBind::default(),
                translated(Vec3::new(0.0, 0.0, -0.3)),
                translated(Vec3::new(0.0, 0.0, -0.00005)),
            ],
        )
        .expect("valid");
        let chain = IkChain {
            root: 0,
            mid: 1,
            tip: 2,
        };
        for target in [
            Vec3::new(0.1, 0.0, -0.2),
            Vec3::new(0.0, 0.0, -0.3),
            Vec3::new(5.0, 0.0, 0.0),
        ] {
            let mut pose = sk.bind_pose_pose();
            solve_two_bone(&sk, &mut pose, &chain, target, None, 1.0);
            assert!(pose.rotations.iter().all(|q| q.is_finite()), "{target}");
        }
    }

    #[test]
    fn non_finite_targets_and_poles_leave_the_pose_alone() {
        let sk = arm_skeleton();
        let chain = IkChain {
            root: 1,
            mid: 2,
            tip: 3,
        };
        let bind = sk.bind_pose_pose();
        for target in [Vec3::new(f32::NAN, 1.0, 0.0), Vec3::splat(f32::INFINITY)] {
            let mut pose = bind.clone();
            solve_two_bone(&sk, &mut pose, &chain, target, None, 1.0);
            assert_eq!(pose, bind, "two-bone with {target}");
            let mut pose = bind.clone();
            solve_look_at(
                &sk,
                &mut pose,
                1,
                sk.bind_pose[1].rotation,
                target,
                LookAtSettings {
                    max_yaw: 1.0,
                    max_pitch: 1.0,
                    weight: 1.0,
                },
            );
            assert_eq!(pose, bind, "look-at with {target}");
        }
        // A NaN pole falls back to the current bend and still reaches.
        let mut pose = bind.clone();
        let target = Vec3::new(0.5, 0.9, -0.4);
        solve_two_bone(&sk, &mut pose, &chain, target, Some(Vec3::splat(f32::NAN)), 1.0);
        let tip = sk.globals(&pose, None)[3].w_axis.truncate();
        assert!((tip - target).length() < 1e-3, "{tip}");
        // Joints past the skeleton are ignored.
        let mut pose = bind.clone();
        solve_two_bone(
            &sk,
            &mut pose,
            &IkChain {
                root: 1,
                mid: 2,
                tip: 9,
            },
            target,
            None,
            1.0,
        );
        assert_eq!(pose, bind);
    }

    #[test]
    fn look_at_limits_are_magnitudes_and_never_panic() {
        let sk = arm_skeleton();
        let target = Vec3::new(10.0, 1.0, -0.01);
        let yaw_with = |max_yaw: f32| {
            let mut pose = sk.bind_pose_pose();
            solve_look_at(
                &sk,
                &mut pose,
                1,
                sk.bind_pose[1].rotation,
                target,
                LookAtSettings {
                    max_yaw,
                    max_pitch: 0.3,
                    weight: 1.0,
                },
            );
            let offset = sk.bind_pose[1].rotation.inverse() * pose_to_global_rot(&sk, &pose, 1);
            offset.to_euler(regia_core::glam::EulerRot::YXZ).0
        };
        // A negative limit panicked in `clamp(0.3, -0.3)`; it now means the same as +0.3.
        assert!((yaw_with(-0.3).abs() - yaw_with(0.3).abs()).abs() < 1e-4);
        assert!(yaw_with(-0.3).abs() <= 0.3 + 1e-3);
        // NaN means unlimited: the full turn towards the target.
        assert!(yaw_with(f32::NAN).abs() > 1.0);
    }

    /// Spine, then a shoulder whose parent is the spine (not the skeleton root).
    fn nested_arm() -> (Skeleton, IkChain) {
        let names: Vec<String> = ["Root", "Spine", "Shoulder", "Elbow", "Hand"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let parents = vec![None, Some(0), Some(1), Some(2), Some(3)];
        let bind = vec![
            LocalBind::default(),
            translated(Vec3::new(0.0, 0.5, 0.0)),
            translated(Vec3::new(0.2, 0.4, 0.0)),
            translated(Vec3::new(0.0, -0.3, 0.0)),
            translated(Vec3::new(0.0, -0.3, 0.0)),
        ];
        let skeleton = Skeleton::from_local_bind(names, parents, bind).expect("valid");
        let chain = IkChain {
            root: 2,
            mid: 3,
            tip: 4,
        };
        (skeleton, chain)
    }

    #[test]
    fn two_bone_and_look_at_match_full_globals_solve() {
        let (skeleton, chain) = nested_arm();
        let mut posed = skeleton.bind_pose_pose();
        posed.rotations[1] = Quat::from_rotation_z(0.4);
        posed.rotations[2] = Quat::from_rotation_x(0.2);
        let full = skeleton.globals(&posed, None);
        let mut scratch = Vec::new();
        refresh_global(&skeleton, &posed, chain.tip as usize, &mut scratch);
        for joint in [1usize, 2, 3, 4] {
            assert!(
                scratch[joint].abs_diff_eq(full[joint], 1e-5),
                "joint {joint} chain matrix matches full fk"
            );
        }
        let target = Vec3::new(0.4, 0.2, 0.3);
        let mut via_full = posed.clone();
        solve_two_bone(&skeleton, &mut via_full, &chain, target, Some(Vec3::Y), 0.75);
        let mut via_scratch = posed.clone();
        let mut globals = Vec::new();
        solve_two_bone_with_globals(
            &skeleton,
            &mut via_scratch,
            &chain,
            target,
            Some(Vec3::Y),
            0.75,
            &mut globals,
        );
        assert_eq!(via_scratch.rotations[2], via_full.rotations[2]);
        assert_eq!(via_scratch.rotations[3], via_full.rotations[3]);
        let tip_full = joint_position(&skeleton, &via_full, chain.tip);
        let tip_scratch = joint_position(&skeleton, &via_scratch, chain.tip);
        assert!(tip_scratch.abs_diff_eq(tip_full, 1e-5));

        let mut look_full = posed.clone();
        let mut look_scratch = posed.clone();
        let settings = LookAtSettings {
            max_yaw: 1.0,
            max_pitch: 0.5,
            weight: 0.8,
        };
        let look_target = Vec3::new(1.0, 1.0, 1.0);
        solve_look_at(
            &skeleton,
            &mut look_full,
            chain.root,
            skeleton.bind_pose[chain.root as usize].rotation,
            look_target,
            settings,
        );
        solve_look_at_with_globals(
            &skeleton,
            &mut look_scratch,
            chain.root,
            skeleton.bind_pose[chain.root as usize].rotation,
            look_target,
            settings,
            &mut globals,
        );
        assert_eq!(
            look_scratch.rotations[chain.root as usize],
            look_full.rotations[chain.root as usize]
        );
        let cap = globals.capacity();
        assert!(cap >= skeleton.len());
        for _ in 0..16 {
            solve_two_bone_with_globals(
                &skeleton,
                &mut via_scratch,
                &chain,
                target,
                Some(Vec3::Y),
                1.0,
                &mut globals,
            );
        }
        assert_eq!(globals.capacity(), cap, "chain scratch capacity stays flat");
    }
}
