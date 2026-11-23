// SPDX-License-Identifier: MPL-2.0
//! Full-body aim and hand placement.
//!
//! Specification: `docs/specs/17-animation.md` § 3 (IK post-processes). [`solve_body_aim`]
//! turns a character's spine, neck and head (or a turret's yaw/pitch stack) so an aim
//! joint's axis points at a target, spreading the turn over the chain by per-joint shares and
//! respecting [`JointLimit`]s. Hand grips then re-attach the hands to the aimed frame (a
//! two-handed weapon stays in both hands) with the analytic two-bone solve.
//! [`place_hand`] is the stand-alone hand placement (lean on a wall, grab a ledge, rest a hand
//! on a console): reach a position and match an orientation.
//!
//! Provenance (`docs/provenance.md`): distributing an aim rotation over a joint chain by
//! weights is a standard technique (e.g. the "look-at with multiple bones" approach described
//! in Aristidou et al., "Inverse Kinematics Techniques in Computer Graphics: A Survey", 2018,
//! § 6); the share-of-remaining scheme and the parallax re-pass are our own formulation.

use regia_core::math::{Mat4, Quat, Vec3, det};
use serde::{Deserialize, Serialize};

use crate::chain_ik::JointLimit;
use crate::ik::{IkChain, solve_two_bone_into};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// A chain of joints that share an aim, bottom (pelvis side) first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AimChain {
    /// Joints that rotate, bottom first (e.g. spine, chest, neck, head).
    pub joints: Vec<u32>,
    /// Relative share of the aim rotation each joint takes (same length as `joints`; empty =
    /// equal shares). The last joint with a non-zero share takes whatever is left.
    pub shares: Vec<f32>,
    /// Per-joint limits relative to the bind rotation (empty = free).
    pub limits: Vec<JointLimit>,
    /// The joint whose axis should point at the target (the head, a weapon or a barrel). It
    /// must be one of `joints` or a descendant of the last.
    pub aim_joint: u32,
    /// The aiming direction in `aim_joint`'s local frame (Regia Engine forward is −Z).
    pub aim_axis: Vec3,
}

/// A hand that should stay on (or move to) a grip point while the body aims.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HandGrip {
    /// Shoulder (upper arm), elbow, hand.
    pub chain: IkChain,
    /// Grip frame relative to the aim joint. `None` captures it from the input pose, so the
    /// hand stays where the animation held it relative to the weapon.
    pub grip: Option<GripFrame>,
    /// Pole (rig space) hinting where the elbow points; `None` keeps the current bend.
    pub pole: Option<Vec3>,
}

/// A rigid transform relative to the aim joint.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GripFrame {
    /// Translation in the aim joint's frame.
    pub translation: Vec3,
    /// Rotation in the aim joint's frame.
    pub rotation: Quat,
}

/// Result of [`solve_body_aim`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AimReport {
    /// Angle between the solved aim axis and the direction to the target, radians.
    pub angle_error: f32,
    /// Worst distance between a hand and its grip after the solve, metres.
    pub grip_error: f32,
}

fn rotation_of(m: Mat4) -> Quat {
    m.to_scale_rotation_translation().1.normalize()
}

fn bone_axis(skeleton: &Skeleton, joint: usize) -> Vec3 {
    // First child's bind offset (the bone direction), or +Y for leaves.
    skeleton
        .parents
        .iter()
        .position(|p| *p == Some(joint as u32))
        .map(|c| skeleton.bind_pose[c].translation.normalize_or(Vec3::Y))
        .unwrap_or(Vec3::Y)
}

/// Aim `aim.aim_joint`'s axis at `target` (rig space) by turning the chain's joints by their
/// shares, then re-attach `grips`. `weight` blends everything with the input pose.
pub fn solve_body_aim(
    skeleton: &Skeleton,
    pose: &mut Pose,
    aim: &AimChain,
    target: Vec3,
    grips: &[HandGrip],
    weight: f32,
) -> AimReport {
    let mut fk = Vec::new();
    solve_body_aim_into(skeleton, pose, aim, target, grips, weight, &mut fk)
}

/// [`solve_body_aim`] using one forward-kinematics buffer for the whole solve.
///
/// `fk` is filled once from `pose` at entry (whatever it held before: a buffer kept from an
/// earlier frame is stale). Later reads refresh the joints that moved instead of allocating
/// another buffer. On return, `fk` matches `pose`.
pub fn solve_body_aim_into(
    skeleton: &Skeleton,
    pose: &mut Pose,
    aim: &AimChain,
    target: Vec3,
    grips: &[HandGrip],
    weight: f32,
    fk: &mut Vec<Mat4>,
) -> AimReport {
    let mut scratch = AimScratch::default();
    body_aim_core(
        skeleton,
        pose,
        aim,
        target,
        grips,
        weight,
        (2, false),
        fk,
        &mut scratch,
    )
}

/// Buffers a body-aim solve reuses besides the forward-kinematics buffer. Kept between frames
/// (the graph's `BodyAim` node keeps one per rig), a solve allocates nothing once warm.
#[derive(Clone, Debug, Default)]
pub struct AimScratch {
    original: Vec<Quat>,
    shares: Vec<f32>,
    grip_frames: Vec<Mat4>,
}

/// [`solve_body_aim_into`] with a chosen number of `passes` (at least one) and kept buffers.
///
/// Each pass spreads the remaining turn over the chain by the shares; the joint taking the
/// last share points the axis exactly from where the aim joint is at that moment, and the next
/// pass absorbs the parallax of an aim joint that sits away from that joint (a barrel ahead of
/// a turret's pivot). [`solve_body_aim`] runs two passes.
///
/// Unlike [`solve_body_aim_into`], each turn is built from the half-angle of the two
/// directions, so a remaining miss under about 1.4 mrad is still corrected (glam's
/// `Quat::from_rotation_arc` treats such directions as equal, which leaves several
/// millimeters at 10 m).
#[allow(clippy::too_many_arguments)] // one solve: rig, pose, chain, goal, passes and buffers
pub fn solve_body_aim_with(
    skeleton: &Skeleton,
    pose: &mut Pose,
    aim: &AimChain,
    target: Vec3,
    grips: &[HandGrip],
    weight: f32,
    passes: u32,
    fk: &mut Vec<Mat4>,
    scratch: &mut AimScratch,
) -> AimReport {
    body_aim_core(
        skeleton,
        pose,
        aim,
        target,
        grips,
        weight,
        (passes, true),
        fk,
        scratch,
    )
}

/// The rotation taking unit `from` onto unit `to`, from their half-angle quaternion
/// (`cross`, `1 + dot`), exact for small angles. Nearly opposite directions fall back to
/// `Quat::from_rotation_arc`, which picks a perpendicular axis.
fn fine_arc(from: Vec3, to: Vec3) -> Quat {
    let dot = from.dot(to);
    if dot < -0.999_999 {
        return Quat::from_rotation_arc(from, to);
    }
    let axis = from.cross(to);
    Quat::from_xyzw(axis.x, axis.y, axis.z, 1.0 + dot).normalize()
}

/// The body-aim solve. `(passes, fine)`: the pass count and whether turns use [`fine_arc`]
/// (the two-pass `solve_body_aim` keeps glam's arc, so its output bits do not move).
#[allow(clippy::too_many_arguments)] // one solve: rig, pose, chain, goal, passes and buffers
fn body_aim_core(
    skeleton: &Skeleton,
    pose: &mut Pose,
    aim: &AimChain,
    target: Vec3,
    grips: &[HandGrip],
    weight: f32,
    (passes, fine): (u32, bool),
    fk: &mut Vec<Mat4>,
    scratch: &mut AimScratch,
) -> AimReport {
    let n = skeleton.len().min(pose.len());
    let aim_joint = aim.aim_joint as usize;
    if aim.joints.is_empty() || aim_joint >= n || aim.joints.iter().any(|&j| j as usize >= n) {
        return AimReport {
            angle_error: std::f32::consts::PI,
            grip_error: 0.0,
        };
    }
    let AimScratch {
        original,
        shares,
        grip_frames,
    } = scratch;
    original.clear();
    original.extend_from_slice(&pose.rotations[..n]);
    // A NaN weight (a parameter computed as 0 / 0) means no aim, like a graph weight.
    let w = crate::num::unit_weight(weight);

    // Capture grips relative to the aim joint before anything moves. Always filled: a kept
    // buffer of the right length holds last frame's matrices (audit corr-animation).
    skeleton.globals_into(pose, None, fk);
    grip_frames.clear();
    grip_frames.extend(grips.iter().map(|g| match g.grip {
        Some(frame) => Mat4::from_rotation_translation(frame.rotation, frame.translation),
        None => fk[aim_joint].inverse() * fk[(g.chain.tip as usize).min(n - 1)],
    }));

    shares.clear();
    if aim.shares.len() == aim.joints.len() {
        shares.extend(aim.shares.iter().map(|s| crate::num::finite_or(*s, 0.0).max(0.0)));
    } else {
        shares.resize(aim.joints.len(), 1.0);
    }
    // Two passes by default: the second absorbs the parallax of the aim joint moving as the
    // spine turns.
    for _pass in 0..passes.max(1) {
        for (k, &joint) in aim.joints.iter().enumerate() {
            let remaining: f32 = shares[k..].iter().sum();
            if shares[k] <= 0.0 || remaining <= 0.0 {
                continue;
            }
            let fraction = shares[k] / remaining;
            let aim_global = fk[aim_joint];
            let from = (rotation_of(aim_global) * aim.aim_axis).normalize_or_zero();
            let to = (target - aim_global.w_axis.truncate()).normalize_or_zero();
            if from.length_squared() < 0.5 || to.length_squared() < 0.5 {
                continue;
            }
            let arc = if fine {
                fine_arc(from, to)
            } else {
                Quat::from_rotation_arc(from, to)
            };
            let delta = det::quat_slerp(Quat::IDENTITY, arc, fraction);
            let j = joint as usize;
            let parent = skeleton
                .parent_of(j)
                .map_or(Quat::IDENTITY, |p| rotation_of(fk[p]));
            let new_global = (delta * rotation_of(fk[j])).normalize();
            let local = (parent.inverse() * new_global).normalize();
            let limit = aim.limits.get(k).copied().unwrap_or_default();
            pose.rotations[j] =
                limit.constrain(local, skeleton.bind_pose[j].rotation, bone_axis(skeleton, j));
            skeleton.globals_refresh_from(pose, None, fk, j);
        }
    }

    // Hands back onto their grips in the aimed frame.
    let mut grip_error = 0.0f32;
    for (grip, frame) in grips.iter().zip(grip_frames.iter()) {
        let aim_global = fk[aim_joint];
        let target_frame = aim_global * *frame;
        grip_error = grip_error.max(place_hand_into(
            skeleton,
            pose,
            &grip.chain,
            target_frame,
            grip.pole,
            1.0,
            fk,
        ));
    }

    let aim_global = fk[aim_joint];
    let from = (rotation_of(aim_global) * aim.aim_axis).normalize_or_zero();
    let to = (target - aim_global.w_axis.truncate()).normalize_or_zero();
    // An axis or target that gives no direction (a zero axis, a non-finite target) is as wrong
    // as it can be, not NaN.
    let angle_error = if from.length_squared() < 0.5 || to.length_squared() < 0.5 {
        std::f32::consts::PI
    } else {
        from.angle_between(to)
    };
    let report = AimReport {
        angle_error,
        grip_error,
    };
    if w < 1.0 {
        for (slot, was) in pose.rotations[..n].iter_mut().zip(original.iter()) {
            // The deterministic kernel, not glam's libm slerp: the blended pose bits must
            // match on every platform (fixrev-06-k H2).
            *slot = det::quat_slerp(*was, *slot, w).normalize();
        }
        skeleton.globals_refresh_from(pose, None, fk, 0);
    }
    // A joint scaled to zero on an axis decomposes to a NaN rotation (audit-v035-numerics M2):
    // such a solve is undone and reported as missing the target entirely.
    if pose.rotations[..n].iter().any(|q| !q.is_finite()) {
        pose.rotations[..n].copy_from_slice(original);
        skeleton.globals_refresh_from(pose, None, fk, 0);
        return AimReport {
            angle_error: std::f32::consts::PI,
            grip_error: f32::INFINITY,
        };
    }
    report
}

/// Place a hand: two-bone IK to `target`'s position (rig space) bending towards `pole`, then
/// match the hand's global rotation to `target`'s. Returns the remaining position error
/// (non-zero when the target is out of reach; infinite, with the pose unchanged, for a chain
/// outside the skeleton or a non-finite target). `weight` blends with the input pose.
pub fn place_hand(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &IkChain,
    target: Mat4,
    pole: Option<Vec3>,
    weight: f32,
) -> f32 {
    let mut fk = Vec::new();
    place_hand_into(skeleton, pose, chain, target, pole, weight, &mut fk)
}

/// [`place_hand`] using a caller-owned forward-kinematics buffer.
pub fn place_hand_into(
    skeleton: &Skeleton,
    pose: &mut Pose,
    chain: &IkChain,
    target: Mat4,
    pole: Option<Vec3>,
    weight: f32,
    fk: &mut Vec<Mat4>,
) -> f32 {
    let n = skeleton.len().min(pose.len());
    let (root, mid, tip) = (chain.root as usize, chain.mid as usize, chain.tip as usize);
    // A non-finite target frame would write a NaN hand rotation.
    if root >= n || mid >= n || tip >= n || !target.is_finite() {
        return f32::INFINITY;
    }
    let w = crate::num::unit_weight(weight);
    let before = [pose.rotations[root], pose.rotations[mid], pose.rotations[tip]];
    let position = target.w_axis.truncate();
    solve_two_bone_into(skeleton, pose, chain, position, pole, 1.0, fk);
    // `solve_two_bone_into` fills `fk` from the pose it started with and then writes new
    // locals. Those matrices are stale until they are recomputed. A buffer that does not
    // yet cover the skeleton is filled; one that does is refreshed from the chain root,
    // which does not count as another fill.
    if fk.len() != skeleton.len() {
        skeleton.globals_into(pose, None, fk);
    } else {
        skeleton.globals_refresh_from(pose, None, fk, root);
    }
    let parent = skeleton
        .parent_of(tip)
        .map_or(Quat::IDENTITY, |p| rotation_of(fk[p]));
    pose.rotations[tip] = (parent.inverse() * rotation_of(target)).normalize();
    skeleton.globals_refresh_from(pose, None, fk, tip);
    let error = (fk[tip].w_axis.truncate() - position).length();
    if w < 1.0 {
        for (k, j) in [root, mid, tip].into_iter().enumerate() {
            // Deterministic kernel, not glam's libm slerp (fixrev-06-k H2).
            pose.rotations[j] = det::quat_slerp(before[k], pose.rotations[j], w).normalize();
        }
        // Keep the documented `fk`-matches-`pose` contract after the partial blend back.
        skeleton.globals_refresh_from(pose, None, fk, root.min(mid).min(tip));
    }
    // A zero-scaled parent decomposes to a NaN rotation (audit-v035-numerics M2): undo.
    if [root, mid, tip]
        .into_iter()
        .any(|j| !pose.rotations[j].is_finite())
    {
        for (k, j) in [root, mid, tip].into_iter().enumerate() {
            pose.rotations[j] = before[k];
        }
        skeleton.globals_refresh_from(pose, None, fk, root.min(mid).min(tip));
        return f32::INFINITY;
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::translated;
    use crate::skeleton::LocalBind;

    /// pelvis → spine → chest → head (aim −Z), chest → r_shoulder → r_elbow → r_hand and the
    /// same on the left; a "rifle" frame is the head (for the test the head carries the aim).
    fn upper_body() -> Skeleton {
        let names = [
            "pelvis",
            "spine",
            "chest",
            "head",
            "r_shoulder",
            "r_elbow",
            "r_hand",
            "l_shoulder",
            "l_elbow",
            "l_hand",
        ];
        let parents = vec![
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(2),
            Some(4),
            Some(5),
            Some(2),
            Some(7),
            Some(8),
        ];
        let binds: Vec<LocalBind> = vec![
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(0.0, 0.2, 0.0)),
            translated(Vec3::new(0.0, 0.3, 0.0)),
            translated(Vec3::new(0.0, 0.3, 0.0)),
            translated(Vec3::new(-0.2, 0.2, 0.0)),
            translated(Vec3::new(0.0, -0.1, -0.3)),
            translated(Vec3::new(0.0, 0.0, -0.3)),
            translated(Vec3::new(0.2, 0.2, 0.0)),
            translated(Vec3::new(0.0, -0.1, -0.3)),
            translated(Vec3::new(-0.15, 0.0, -0.3)),
        ];
        Skeleton::from_local_bind(names.iter().map(|s| s.to_string()).collect(), parents, binds)
            .expect("valid skeleton")
    }

    fn aim_chain() -> AimChain {
        AimChain {
            joints: vec![1, 2, 3],
            shares: vec![0.3, 0.3, 0.4],
            limits: Vec::new(),
            aim_joint: 3,
            aim_axis: Vec3::NEG_Z,
        }
    }

    /// Audit (corr-animation): a kept `fk` buffer of the right length was trusted as is, so the
    /// next frame aimed from last frame's root and left `fk` stale below the chain.
    #[test]
    fn a_kept_fk_buffer_does_not_leak_last_frames_pose() {
        let sk = upper_body();
        let target = Vec3::new(1.0, 1.8, -2.0);
        let mut fk = Vec::new();
        let mut warm = sk.bind_pose_pose();
        solve_body_aim_into(&sk, &mut warm, &aim_chain(), target, &[], 1.0, &mut fk);
        let mut fresh = sk.bind_pose_pose();
        fresh.rotations[0] = Quat::from_rotation_y(1.2);
        let mut kept = fresh.clone();
        solve_body_aim(&sk, &mut fresh, &aim_chain(), target, &[], 1.0);
        solve_body_aim_into(&sk, &mut kept, &aim_chain(), target, &[], 1.0, &mut fk);
        assert_eq!(fresh.rotations, kept.rotations);
        let expected = sk.globals(&kept, None);
        assert_eq!(fk.len(), expected.len());
        for (got, want) in fk.iter().zip(&expected) {
            assert!(got.abs_diff_eq(*want, 1e-5), "{got:?} vs {want:?}");
        }
    }

    /// Audit (corr-animation): a partial-weight hand placement returned `fk` for the full solve.
    #[test]
    fn a_partial_hand_placement_leaves_fk_matching_the_pose() {
        let sk = upper_body();
        let chain = IkChain {
            root: 4,
            mid: 5,
            tip: 6,
        };
        let target = Mat4::from_rotation_translation(Quat::from_rotation_x(0.4), Vec3::new(-0.3, 1.3, -0.4));
        let mut pose = sk.bind_pose_pose();
        let mut fk = Vec::new();
        place_hand_into(&sk, &mut pose, &chain, target, None, 0.5, &mut fk);
        let expected = sk.globals(&pose, None);
        for (got, want) in fk.iter().zip(&expected) {
            assert!(got.abs_diff_eq(*want, 1e-5), "{got:?} vs {want:?}");
        }
    }

    #[test]
    fn body_aim_points_the_aim_axis_at_the_target() {
        let sk = upper_body();
        let mut pose = sk.bind_pose_pose();
        let target = Vec3::new(4.0, 3.5, -3.0);
        let report = solve_body_aim(&sk, &mut pose, &aim_chain(), target, &[], 1.0);
        assert!(report.angle_error < 0.01, "angle error {}", report.angle_error);
        // The turn is spread: every chain joint moved.
        for j in [1usize, 2, 3] {
            assert!(
                pose.rotations[j].angle_between(Quat::IDENTITY) > 0.05,
                "joint {j} turned"
            );
        }
    }

    #[test]
    fn hands_follow_the_aimed_frame() {
        let sk = upper_body();
        let mut pose = sk.bind_pose_pose();
        let grips = [
            HandGrip {
                chain: IkChain {
                    root: 4,
                    mid: 5,
                    tip: 6,
                },
                grip: None,
                pole: None,
            },
            HandGrip {
                chain: IkChain {
                    root: 7,
                    mid: 8,
                    tip: 9,
                },
                grip: None,
                pole: None,
            },
        ];
        let before = sk.globals(&pose, None);
        let rel_r = before[3].inverse() * before[6];
        let target = Vec3::new(-3.0, 1.0, -4.0);
        // The torso turns; the head (the weapon frame here) rides on the chest.
        let chain = AimChain {
            joints: vec![1, 2],
            shares: vec![0.5, 0.5],
            ..aim_chain()
        };
        let report = solve_body_aim(&sk, &mut pose, &chain, target, &grips, 1.0);
        assert!(report.angle_error < 0.01);
        assert!(report.grip_error < 1e-3, "grip error {}", report.grip_error);
        let after = sk.globals(&pose, None);
        let rel_after = after[3].inverse() * after[6];
        assert!(
            (rel_after.w_axis - rel_r.w_axis).truncate().length() < 1e-3,
            "right hand stays on the grip relative to the aim joint"
        );
    }

    #[test]
    fn limits_cap_each_joint() {
        let sk = upper_body();
        let mut pose = sk.bind_pose_pose();
        let mut chain = aim_chain();
        let cap = JointLimit::Cone {
            max_swing: 0.2,
            min_twist: -0.2,
            max_twist: 0.2,
        };
        chain.limits = vec![cap; 3];
        let report = solve_body_aim(&sk, &mut pose, &chain, Vec3::new(0.0, 1.5, 5.0), &[], 1.0);
        // Target directly behind: unreachable within ±0.2 rad per joint.
        assert!(report.angle_error > 1.0);
        for j in [1usize, 2, 3] {
            assert!(pose.rotations[j].angle_between(Quat::IDENTITY) <= 0.41);
        }
    }

    #[test]
    fn place_hand_reaches_and_orients() {
        let sk = upper_body();
        let mut pose = sk.bind_pose_pose();
        let chain = IkChain {
            root: 4,
            mid: 5,
            tip: 6,
        };
        let want_rotation = Quat::from_rotation_y(0.7) * Quat::from_rotation_x(-0.4);
        let target = Mat4::from_rotation_translation(want_rotation, Vec3::new(-0.4, 1.4, -0.4));
        let error = place_hand(
            &sk,
            &mut pose,
            &chain,
            target,
            Some(Vec3::new(-1.0, 0.0, 0.0)),
            1.0,
        );
        assert!(error < 1e-3, "error {error}");
        let hand = sk.globals(&pose, None)[6];
        assert!(rotation_of(hand).angle_between(want_rotation) < 1e-3);
    }

    #[test]
    fn non_finite_targets_and_weights_change_nothing() {
        let sk = upper_body();
        let bind = sk.bind_pose_pose();
        let mut pose = bind.clone();
        let error = place_hand(
            &sk,
            &mut pose,
            &IkChain {
                root: 4,
                mid: 5,
                tip: 6,
            },
            Mat4::from_translation(Vec3::new(f32::NAN, 1.0, 0.0)),
            None,
            1.0,
        );
        // The hand's rotation used to come out NaN.
        assert_eq!(error, f32::INFINITY);
        assert_eq!(pose, bind);
        // A NaN weight means no aim (it used to apply the full aim).
        let mut pose = bind.clone();
        solve_body_aim(
            &sk,
            &mut pose,
            &aim_chain(),
            Vec3::new(4.0, 3.5, -3.0),
            &[],
            f32::NAN,
        );
        for (j, (a, b)) in pose.rotations.iter().zip(&bind.rotations).enumerate() {
            // `rotation_error`, not `angle_between`: an `f32` acos reads a 1-ulp slerp wobble as 7e-4.
            assert!(crate::compress::rotation_error(*a, *b) < 1e-4, "joint {j} moved");
        }
    }
}
