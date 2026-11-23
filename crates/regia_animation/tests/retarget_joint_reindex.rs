// SPDX-License-Identifier: MPL-2.0
//! Legal storage-order invariance (spec 17 §§ 1, 4, 6; spec 85 §§ 2, 6).
//! Fixtures use authored object-space geometry, not a reference retargeting solver.

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::pose::Pose;
use regia_animation::retarget::{RetargetProfile, Retargeter};
use regia_animation::skeleton::{LocalBind, Skeleton};
use regia_core::math::{Mat4, Quat, Vec3};

const N: usize = 17;
const NAMES: [&str; N] = [
    "root",
    "hips",
    "spine",
    "chest",
    "leftUpperArm",
    "leftLowerArm",
    "leftHand",
    "rightUpperArm",
    "rightLowerArm",
    "rightHand",
    "leftUpperLeg",
    "leftLowerLeg",
    "leftFoot",
    "rightUpperLeg",
    "rightLowerLeg",
    "rightFoot",
    "helper",
];
const PARENTS: [Option<usize>; N] = [
    None,
    Some(0),
    Some(1),
    Some(2),
    Some(3),
    Some(4),
    Some(5),
    Some(3),
    Some(7),
    Some(8),
    Some(1),
    Some(10),
    Some(11),
    Some(1),
    Some(13),
    Some(14),
    Some(5),
];
const ORDERS: [[usize; N]; 3] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
    [0, 1, 13, 10, 2, 14, 3, 11, 7, 4, 15, 12, 8, 5, 9, 16, 6],
    [0, 1, 2, 10, 13, 3, 4, 7, 11, 14, 5, 8, 12, 15, 16, 6, 9],
];
const TIMES: [f32; 9] = [0.0, 0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875, 1.0];
// Unit-scale meter rigs, depth <= 6: 3e-5 allows accumulated f32 matrix/quaternion
// roundoff (including bind inversion), while resolving 0.03 mm / ~0.002 degrees.
const EPS: f32 = 3e-5;

struct Rig {
    skeleton: Skeleton,
    profile: RetargetProfile,
    clip: AnimationClip,
    authored: Vec<Mat4>,
}

fn index(skeleton: &Skeleton, stable: usize) -> usize {
    skeleton.joint_index(NAMES[stable]).expect("fixture name") as usize
}

fn curve<T>(values: Vec<T>) -> TrackCurve<T> {
    TrackCurve::Animated {
        times: vec![0.0, 0.5, 1.0],
        values,
        exact_rotation: false,
    }
}

fn fixture(target: bool) -> Rig {
    fixture_with_branch_rest(target, false)
}

fn fixture_with_branch_rest(target: bool, branch_rest: bool) -> Rig {
    let (hip, upper, lower, arm) = if target {
        (1.3, 0.62, 0.51, 0.46)
    } else {
        (1.0, 0.45, 0.42, 0.31)
    };
    let root = Vec3::new(0.2, 0.1, -0.3);
    let arm_y = if target { -0.18 } else { 0.06 };
    let shoulder_y = hip + if target && branch_rest { 0.33 } else { 0.43 };
    let positions = [
        Vec3::ZERO,
        Vec3::new(0.0, hip, 0.0),
        Vec3::new(0.0, hip + 0.2, 0.0),
        Vec3::new(0.0, hip + 0.45, 0.0),
        Vec3::new(-0.23, shoulder_y, 0.0),
        Vec3::new(-0.23 - arm, hip + 0.43 + arm_y, 0.03),
        Vec3::new(-0.23 - arm * 1.8, hip + 0.4 + arm_y, -0.02),
        Vec3::new(0.23, shoulder_y, 0.0),
        Vec3::new(0.23 + arm, hip + 0.43 + arm_y, 0.03),
        Vec3::new(0.23 + arm * 1.8, hip + 0.4 + arm_y, -0.02),
        Vec3::new(-0.14, hip - 0.05, 0.0),
        Vec3::new(-0.16, hip - upper, 0.04),
        Vec3::new(-0.16, hip - upper - lower, -0.06),
        Vec3::new(0.14, hip - 0.05, 0.0),
        Vec3::new(0.16, hip - upper, 0.04),
        Vec3::new(0.16, hip - upper - lower, -0.06),
        Vec3::new(-0.25 - arm, hip + 0.5 + arm_y, 0.12),
    ];
    let authored: Vec<Mat4> = positions
        .iter()
        .enumerate()
        .map(|(j, &p)| {
            let roll = (j as f32 + 1.0) * if target { -0.17 } else { 0.11 };
            Mat4::from_rotation_translation(
                Quat::from_rotation_z(roll) * Quat::from_rotation_y(roll * 0.4),
                p + root,
            )
        })
        .collect();
    let bind_pose = authored
        .iter()
        .enumerate()
        .map(|(j, &global)| {
            let local = PARENTS[j].map_or(global, |p| authored[p].inverse() * global);
            let (scale, rotation, translation) = local.to_scale_rotation_translation();
            LocalBind {
                scale,
                rotation,
                translation,
            }
        })
        .collect();
    let skeleton = Skeleton {
        joint_names: NAMES.iter().map(|n| (*n).to_owned()).collect(),
        parents: PARENTS.iter().map(|p| p.map(|p| p as u32)).collect(),
        bind_pose,
        inverse_bind_matrices: authored.iter().map(|m| m.inverse()).collect(),
        sockets: Vec::new(),
    };
    skeleton.validate().expect("valid original fixture");
    // Deliberately explicit, excluding only the helper; role resolution must follow names.
    let profile = NAMES[..N - 1]
        .iter()
        .fold(RetargetProfile::default(), |p, n| p.with(*n, *n));
    let mut clip = AnimationClip::empty("asymmetric-motion", 1.0);
    clip.sample_rate = 4.0;
    for (j, bind) in skeleton.bind_pose.iter().enumerate() {
        let amount = (j + 1) as f32 * 0.027;
        let translations = if j == 0 || j == 1 || j == N - 1 {
            curve(vec![
                bind.translation,
                bind.translation + Vec3::new(0.19, 0.07, -0.31),
                bind.translation + Vec3::new(0.43, -0.03, -0.72),
            ])
        } else {
            TrackCurve::Static(bind.translation)
        };
        clip.tracks.push(JointTrack {
            joint: j as u32,
            translations,
            rotations: curve(vec![
                bind.rotation,
                bind.rotation * Quat::from_rotation_x(amount),
                bind.rotation * Quat::from_rotation_z(-amount * 0.7),
            ]),
            scales: TrackCurve::Static(bind.scale),
        });
    }
    clip.root_motion = Some(RootMotionTrack {
        joint: 0,
        mode: RootMotionMode::XYZ,
    });
    Rig {
        skeleton,
        profile,
        clip,
        authored,
    }
}

fn reorder(rig: &Rig, order: &[usize; N]) -> Rig {
    let mut old_to_new = [usize::MAX; N];
    for (new, &old) in order.iter().enumerate() {
        assert_eq!(old_to_new[old], usize::MAX, "order is a permutation");
        old_to_new[old] = new;
    }
    let old = &rig.skeleton;
    let skeleton = Skeleton {
        joint_names: order.iter().map(|&j| old.joint_names[j].clone()).collect(),
        parents: order
            .iter()
            .map(|&j| old.parents[j].map(|p| old_to_new[p as usize] as u32))
            .collect(),
        bind_pose: order.iter().map(|&j| old.bind_pose[j]).collect(),
        inverse_bind_matrices: order.iter().map(|&j| old.inverse_bind_matrices[j]).collect(),
        sockets: Vec::new(),
    };
    skeleton.validate().expect("legal parent-before-child reorder");
    let mut clip = rig.clip.clone();
    for track in &mut clip.tracks {
        track.joint = old_to_new[track.joint as usize] as u32;
    }
    clip.tracks.sort_by_key(|t| t.joint);
    if let Some(rm) = &mut clip.root_motion {
        rm.joint = old_to_new[rm.joint as usize] as u32;
    }
    // Profiles contain names, not indices: rebuild the explicit map through the permutation.
    let profile = rig
        .profile
        .roles
        .iter()
        .fold(RetargetProfile::default(), |p, (role, name)| {
            let old_index = old.joint_index(name).expect("mapped fixture joint") as usize;
            p.with(role.clone(), skeleton.joint_names[old_to_new[old_index]].clone())
        });
    Rig {
        skeleton,
        profile,
        clip,
        authored: rig.authored.clone(),
    }
}

// Independent FK oracle: walk each joint's ancestry, then multiply its root-to-joint chain.
// This does not rely on the storage-order single pass used by Skeleton::globals.
fn globals(skeleton: &Skeleton, pose: &Pose) -> Vec<Mat4> {
    (0..skeleton.len())
        .map(|j| {
            let mut chain = vec![j];
            while let Some(p) = skeleton.parents[*chain.last().expect("nonempty chain")] {
                chain.push(p as usize);
                assert!(chain.len() <= skeleton.len(), "acyclic fixture");
            }
            chain.iter().rev().fold(Mat4::IDENTITY, |m, &i| {
                m * Mat4::from_scale_rotation_translation(
                    pose.scales[i],
                    pose.rotations[i],
                    pose.translations[i],
                )
            })
        })
        .collect()
}

fn matrix_error(a: Mat4, b: Mat4) -> f32 {
    if !a.is_finite() || !b.is_finite() {
        return f32::INFINITY;
    }
    a.to_cols_array()
        .into_iter()
        .zip(b.to_cols_array())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max)
}

fn comparison_error(a: &Skeleton, ap: &Pose, b: &Skeleton, bp: &Pose, context: &str) -> Option<String> {
    let (ag, bg) = (globals(a, ap), globals(b, bp));
    for (j, name) in NAMES.iter().enumerate() {
        let (am, bm) = (ag[index(a, j)], bg[index(b, j)]);
        let error = matrix_error(am, bm);
        if !am.is_finite() || !bm.is_finite() || error > EPS {
            return Some(format!("{context}: {name} matrix error {error} > {EPS}"));
        }
    }
    None
}

fn compare(a: &Skeleton, ap: &Pose, b: &Skeleton, bp: &Pose, context: &str) {
    assert_eq!(comparison_error(a, ap, b, bp, context), None);
}

fn retargeter(source: &Rig, target: &Rig) -> Retargeter {
    source
        .skeleton
        .validate()
        .expect("source validates before retargeting");
    target
        .skeleton
        .validate()
        .expect("target validates before retargeting");
    let rt = Retargeter::new(
        &source.skeleton,
        &source.profile,
        &target.skeleton,
        &target.profile,
    )
    .expect("explicit profiles build");
    assert_eq!(rt.mapped_joints(), N - 1);
    for j in 0..N - 1 {
        assert_eq!(
            rt.source_of(index(&target.skeleton, j) as u32),
            Some(index(&source.skeleton, j) as u32)
        );
    }
    assert_eq!(rt.source_of(index(&target.skeleton, N - 1) as u32), None);
    rt
}

fn sample(rig: &Rig, clip: &AnimationClip, time: f32) -> Pose {
    let mut pose = rig.skeleton.bind_pose_pose();
    clip.sample_pose(time, &mut pose);
    pose
}

#[test]
fn legal_permutations_preserve_authored_geometry_tracks_and_inverse_binds() {
    for (target, branch_rest) in [(false, false), (true, false), (true, true)] {
        let original = fixture_with_branch_rest(target, branch_rest);
        for order in &ORDERS {
            let rig = reorder(&original, order);
            let bind = globals(&rig.skeleton, &rig.skeleton.bind_pose_pose());
            for (j, name) in NAMES.iter().enumerate() {
                let i = index(&rig.skeleton, j);
                assert!(
                    matrix_error(bind[i], rig.authored[j]) <= EPS,
                    "authored bind {}",
                    name
                );
                assert!(matrix_error(bind[i] * rig.skeleton.inverse_bind_matrices[i], Mat4::IDENTITY) <= EPS);
            }
            for time in TIMES {
                compare(
                    &original.skeleton,
                    &sample(&original, &original.clip, time),
                    &rig.skeleton,
                    &sample(&rig, &rig.clip, time),
                    &format!("input time {time}"),
                );
            }
        }
    }
}

fn check_order_combinations(baked: bool, reorder_target: bool, branch_rest: bool) {
    let (source, target) = (fixture(false), fixture_with_branch_rest(true, branch_rest));
    let reference = retargeter(&source, &target);
    let reference_clip = reference.retarget_clip(&source.clip);
    let mut failures = Vec::new();
    for (s, so) in ORDERS.iter().enumerate() {
        for (t, to) in ORDERS.iter().enumerate() {
            if !reorder_target && t != 0 {
                continue;
            }
            let (sr, tr) = (reorder(&source, so), reorder(&target, to));
            let rt = retargeter(&sr, &tr);
            let clip = rt.retarget_clip(&sr.clip);
            for time in TIMES {
                let (mut expected, mut actual) =
                    (target.skeleton.bind_pose_pose(), tr.skeleton.bind_pose_pose());
                if baked {
                    reference_clip.sample_pose(time, &mut expected);
                    clip.sample_pose(time, &mut actual);
                } else {
                    reference.retarget_pose(&sample(&source, &source.clip, time), &mut expected);
                    rt.retarget_pose(&sample(&sr, &sr.clip, time), &mut actual);
                }
                let helper = index(&tr.skeleton, N - 1);
                let bind = tr.skeleton.bind_pose[helper];
                assert!(
                    matrix_error(
                        Mat4::from_scale_rotation_translation(
                            actual.scales[helper],
                            actual.rotations[helper],
                            actual.translations[helper]
                        ),
                        Mat4::from_scale_rotation_translation(bind.scale, bind.rotation, bind.translation)
                    ) <= EPS,
                    "unmapped helper retains local bind"
                );
                if let Some(error) = comparison_error(
                    &target.skeleton,
                    &expected,
                    &tr.skeleton,
                    &actual,
                    &format!("baked={baked} source order {s} target order {t} time {time}"),
                ) {
                    failures.push(error);
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} order/sample comparisons failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn source_joint_order_preserves_direct_and_baked_object_space_motion() {
    for branch_rest in [false, true] {
        check_order_combinations(false, false, branch_rest);
        check_order_combinations(true, false, branch_rest);
    }
}

#[test]
fn target_joint_order_preserves_direct_object_space_motion() {
    check_order_combinations(false, true, false);
}

#[test]
fn target_joint_order_preserves_baked_object_space_motion() {
    check_order_combinations(true, true, false);
}

#[test]
fn changed_branch_rest_preserves_direct_motion_under_legal_joint_orders() {
    check_order_combinations(false, true, true);
}

#[test]
fn changed_branch_rest_preserves_baked_motion_under_legal_joint_orders() {
    check_order_combinations(true, true, true);
}

#[test]
fn all_joint_orders_preserve_root_motion_at_keys_and_intermediate_samples() {
    let source = fixture(false);
    for branch_rest in [false, true] {
        let target = fixture_with_branch_rest(true, branch_rest);
        let reference = retargeter(&source, &target).retarget_clip(&source.clip);
        for so in &ORDERS {
            for to in &ORDERS {
                let (sr, tr) = (reorder(&source, so), reorder(&target, to));
                let rt = retargeter(&sr, &tr);
                let baked = rt.retarget_clip(&sr.clip);
                let rm = baked.root_motion.as_ref().expect("root motion retained");
                assert_eq!(tr.skeleton.joint_names[rm.joint as usize], "root");
                assert_eq!(rm.mode, RootMotionMode::XYZ);
                for times in TIMES.windows(2) {
                    let (a, b) = (times[0], times[1]);
                    let (et, eq) = reference
                        .root_motion_delta(a, b, false)
                        .expect("reference motion");
                    let (at, aq) = baked.root_motion_delta(a, b, false).expect("reordered motion");
                    assert!(et.length() > 0.01, "nonzero travel at every interval");
                    assert!((et - at).length() <= EPS);
                    assert!(matrix_error(Mat4::from_quat(eq), Mat4::from_quat(aq)) <= EPS);
                    // Root translation is piecewise linear with a key at 0.5, also a bake key.
                    let (st, sq) = source.clip.root_motion_delta(a, b, false).expect("source motion");
                    assert!(
                        (at - st * rt.scale()).length() <= EPS,
                        "independent scaled travel"
                    );
                    assert!(
                        matrix_error(Mat4::from_quat(aq), Mat4::from_quat(sq)) <= EPS,
                        "root turn is unchanged for rigs with the same facing"
                    );
                }
            }
        }
    }
}
