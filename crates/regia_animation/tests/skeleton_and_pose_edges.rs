// SPDX-License-Identifier: MPL-2.0
//! Error paths and boundary behavior of skeleton construction, forward kinematics, pose
//! blending and joint masks. Malformed skeleton data comes from asset files, so these must
//! return errors rather than panic.

use regia_animation::pose::{JointMask, Pose, translated};
use regia_animation::skeleton::{LocalBind, Skeleton, SkeletonError};
use regia_core::math::{Mat4, Quat, Vec3};

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

fn chain3() -> Skeleton {
    Skeleton::from_local_bind(
        names(&["root", "mid", "tip"]),
        vec![None, Some(0), Some(1)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(0.0, 0.0, 2.0)),
        ],
    )
    .expect("valid chain")
}

#[test]
fn from_local_bind_rejects_short_parent_array_without_panicking() {
    let err = Skeleton::from_local_bind(
        names(&["a", "b", "c"]),
        vec![None, Some(0)],
        vec![LocalBind::default(); 3],
    )
    .expect_err("three joints but two parents");
    assert!(matches!(
        err,
        SkeletonError::MismatchedLengths {
            names: 3,
            parents: 2,
            bind: 3,
            ..
        }
    ));
}

#[test]
fn from_local_bind_rejects_short_bind_array_without_panicking() {
    let err = Skeleton::from_local_bind(
        names(&["a", "b"]),
        vec![None, Some(0)],
        vec![LocalBind::default()],
    )
    .expect_err("two joints but one bind transform");
    assert!(matches!(
        err,
        SkeletonError::MismatchedLengths {
            names: 2,
            bind: 1,
            ..
        }
    ));
}

#[test]
fn from_local_bind_rejects_a_parent_index_past_the_end() {
    let err = Skeleton::from_local_bind(
        names(&["a", "b"]),
        vec![None, Some(9)],
        vec![LocalBind::default(); 2],
    )
    .expect_err("parent 9 does not exist");
    assert_eq!(err, SkeletonError::ParentNotEarlier { joint: 1, parent: 9 });
}

#[test]
fn from_local_bind_rejects_a_self_parent() {
    let err = Skeleton::from_local_bind(
        names(&["a", "b"]),
        vec![None, Some(1)],
        vec![LocalBind::default(); 2],
    )
    .expect_err("a joint cannot be its own parent");
    assert_eq!(err, SkeletonError::ParentNotEarlier { joint: 1, parent: 1 });
}

#[test]
fn from_local_bind_of_zero_joints_is_valid_and_empty() {
    let skeleton = Skeleton::from_local_bind(Vec::new(), Vec::new(), Vec::new()).expect("empty skeleton");
    assert!(skeleton.is_empty());
    assert_eq!(skeleton.len(), 0);
    assert!(skeleton.globals(&Pose::from_identity(0), None).is_empty());
    assert!(skeleton.max_descendant_distance().is_empty());
}

#[test]
fn validate_reports_mismatched_inverse_binds() {
    let mut skeleton = chain3();
    skeleton.inverse_bind_matrices.pop();
    assert_eq!(
        skeleton.validate(),
        Err(SkeletonError::MismatchedLengths {
            names: 3,
            parents: 3,
            bind: 3,
            ibm: 2
        })
    );
}

#[test]
fn skeleton_error_messages_name_the_problem() {
    let text = SkeletonError::DuplicateName { name: "hip".into() }.to_string();
    assert!(text.contains("hip"), "{text}");
    let text = SkeletonError::EmptyName { joint: 4 }.to_string();
    assert!(text.contains('4'), "{text}");
}

#[test]
fn joint_index_of_unknown_or_empty_name_is_none() {
    let skeleton = chain3();
    assert_eq!(skeleton.joint_index("tip"), Some(2));
    assert_eq!(skeleton.joint_index("nope"), None);
    assert_eq!(skeleton.joint_index(""), None);
}

#[test]
fn globals_honor_the_base_matrix_and_compose_down_the_chain() {
    let skeleton = chain3();
    let pose = skeleton.bind_pose_pose();
    let base = Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0));
    let globals = skeleton.globals(&pose, Some(base));
    assert_eq!(globals.len(), 3);
    let tip = globals[2].transform_point3(Vec3::ZERO);
    assert!((tip - Vec3::new(10.0, 1.0, 2.0)).length() < 1e-5, "{tip:?}");
    // Without a base the same pose is not offset.
    let plain = skeleton.globals(&pose, None);
    let tip = plain[2].transform_point3(Vec3::ZERO);
    assert!((tip - Vec3::new(0.0, 1.0, 2.0)).length() < 1e-5, "{tip:?}");
}

#[test]
fn globals_of_a_shorter_pose_leave_the_remaining_joints_at_identity() {
    let skeleton = chain3();
    let globals = skeleton.globals(&Pose::from_identity(1), None);
    assert_eq!(globals.len(), 3, "always one matrix per skeleton joint");
    assert_eq!(globals[1], Mat4::IDENTITY);
    assert_eq!(globals[2], Mat4::IDENTITY);
}

#[test]
fn skin_palette_of_the_bind_pose_is_identity() {
    let skeleton = chain3();
    let globals = skeleton.globals(&skeleton.bind_pose_pose(), None);
    for (i, m) in skeleton.skin_palette(&globals).iter().enumerate() {
        assert!(m.abs_diff_eq(Mat4::IDENTITY, 1e-5), "joint {i}: {m:?}");
    }
}

#[test]
fn skin_palette_truncates_to_the_shorter_input() {
    let skeleton = chain3();
    let palette = skeleton.skin_palette(&[Mat4::IDENTITY]);
    assert_eq!(palette.len(), 1);
}

#[test]
fn max_descendant_distance_takes_the_longest_branch() {
    // root has two children: a short branch and a two-bone long branch.
    let skeleton = Skeleton::from_local_bind(
        names(&["root", "short", "long", "long_tip"]),
        vec![None, Some(0), Some(0), Some(2)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::new(1.0, 0.0, 0.0)),
            translated(Vec3::new(0.0, 2.0, 0.0)),
            translated(Vec3::new(0.0, 0.0, 3.0)),
        ],
    )
    .expect("valid tree");
    let d = skeleton.max_descendant_distance();
    assert_eq!(d.len(), 4);
    assert!((d[0] - 5.0).abs() < 1e-5, "2 + 3 along the long branch: {}", d[0]);
    assert!((d[2] - 3.0).abs() < 1e-5);
    assert_eq!(d[1], 0.0);
    assert_eq!(d[3], 0.0);
}

#[test]
fn blend_with_a_shorter_pose_leaves_the_extra_joints_untouched() {
    let mut a = Pose::from_identity(3);
    let mut b = Pose::from_identity(1);
    b.translations[0] = Vec3::new(2.0, 0.0, 0.0);
    a.blend_from(&b, 0.5);
    assert!((a.translations[0].x - 1.0).abs() < 1e-6);
    assert_eq!(a.translations[1], Vec3::ZERO);
    assert_eq!(a.translations[2], Vec3::ZERO);
    assert_eq!(a.len(), 3);
}

#[test]
fn blend_of_copies_the_first_pose_then_blends_the_second() {
    let mut a = Pose::from_identity(2);
    a.translations[1] = Vec3::new(4.0, 0.0, 0.0);
    let mut b = Pose::from_identity(2);
    b.translations[1] = Vec3::new(8.0, 0.0, 0.0);
    // The destination starts with a different length and stale data.
    let mut out = Pose::from_identity(5);
    out.translations[0] = Vec3::splat(99.0);
    out.blend_of(&a, &b, 0.25);
    assert_eq!(out.len(), 2, "takes the length of the first input");
    assert_eq!(out.translations[0], Vec3::ZERO);
    assert!((out.translations[1].x - 5.0).abs() < 1e-6);
}

#[test]
fn delta_from_a_zero_scale_reference_does_not_divide_by_zero() {
    let mut pose = Pose::from_identity(1);
    pose.scales[0] = Vec3::new(2.0, 2.0, 2.0);
    let mut reference = Pose::from_identity(1);
    reference.scales[0] = Vec3::new(0.0, 4.0, 0.0);
    let delta = pose.delta_from(&reference);
    assert_eq!(
        delta.scales[0].x, 1.0,
        "a zero reference component yields a neutral ratio"
    );
    assert_eq!(delta.scales[0].y, 0.5);
    assert_eq!(delta.scales[0].z, 1.0);
    assert!(delta.scales[0].is_finite());
}

#[test]
fn delta_then_additive_reproduces_the_original_pose() {
    let mut reference = Pose::from_identity(1);
    reference.translations[0] = Vec3::new(1.0, 0.0, 0.0);
    reference.rotations[0] = Quat::from_rotation_y(0.3);
    let mut pose = reference.clone();
    pose.translations[0] += Vec3::new(0.0, 2.0, 0.0);
    pose.rotations[0] = reference.rotations[0] * Quat::from_rotation_x(0.4);
    let delta = pose.delta_from(&reference);
    let mut rebuilt = reference.clone();
    rebuilt.additive_from(&delta, 1.0);
    assert!((rebuilt.translations[0] - pose.translations[0]).length() < 1e-5);
    assert!(rebuilt.rotations[0].angle_between(pose.rotations[0]) < 1e-4);
}

#[test]
fn additive_at_zero_weight_changes_nothing() {
    let mut pose = Pose::from_identity(1);
    pose.rotations[0] = Quat::from_rotation_z(0.2);
    let before = pose.clone();
    let mut add = Pose::from_identity(1);
    add.translations[0] = Vec3::splat(5.0);
    add.rotations[0] = Quat::from_rotation_x(1.0);
    add.scales[0] = Vec3::splat(3.0);
    pose.additive_from(&add, 0.0);
    assert!((pose.translations[0] - before.translations[0]).length() < 1e-6);
    assert!(pose.rotations[0].angle_between(before.rotations[0]) < 1e-5);
    assert!((pose.scales[0] - before.scales[0]).length() < 1e-6);
}

#[test]
fn masked_blend_skips_zero_and_negative_effective_weights() {
    let mut base = Pose::from_identity(3);
    let mut other = Pose::from_identity(3);
    for t in &mut other.translations {
        *t = Vec3::new(10.0, 0.0, 0.0);
    }
    // Joint 0 masked out, joint 1 half, joint 2 negative (treated as masked out).
    base.masked_blend_from(&other, &[0.0, 0.5, -1.0], 1.0);
    assert_eq!(base.translations[0].x, 0.0);
    assert!((base.translations[1].x - 5.0).abs() < 1e-6);
    assert_eq!(base.translations[2].x, 0.0);
}

#[test]
fn masked_blend_with_a_short_mask_only_touches_the_covered_joints() {
    let mut base = Pose::from_identity(3);
    let mut other = Pose::from_identity(3);
    for t in &mut other.translations {
        *t = Vec3::new(2.0, 0.0, 0.0);
    }
    base.masked_blend_from(&other, &[1.0], 1.0);
    assert_eq!(base.translations[0].x, 2.0);
    assert_eq!(base.translations[1].x, 0.0);
    assert_eq!(base.translations[2].x, 0.0);
}

#[test]
fn masked_additive_scales_the_delta_by_mask_and_weight() {
    let mut base = Pose::from_identity(2);
    let mut add = Pose::from_identity(2);
    add.translations[0] = Vec3::new(4.0, 0.0, 0.0);
    add.translations[1] = Vec3::new(4.0, 0.0, 0.0);
    base.masked_additive_from(&add, &[1.0, 0.0], 0.5);
    assert!((base.translations[0].x - 2.0).abs() < 1e-6);
    assert_eq!(base.translations[1].x, 0.0);
}

#[test]
fn copy_from_resizes_the_destination() {
    let mut dst = Pose::from_identity(4);
    let mut src = Pose::from_identity(2);
    src.translations[1] = Vec3::new(1.0, 2.0, 3.0);
    dst.copy_from(&src);
    assert_eq!(dst, src);
}

#[test]
fn joint_mask_ignores_out_of_range_joints() {
    let mask = JointMask {
        joints: vec![1, 99],
        include_children: false,
    };
    let w = mask.weights(&[None, Some(0), Some(1)], 3);
    assert_eq!(w, [0.0, 1.0, 0.0]);
}

#[test]
fn joint_mask_with_children_covers_the_subtree_only() {
    //        0
    //       / \
    //      1   2
    //      |
    //      3
    let parents = [None, Some(0), Some(0), Some(1)];
    let mask = JointMask {
        joints: vec![1],
        include_children: true,
    };
    assert_eq!(mask.weights(&parents, 4), [0.0, 1.0, 0.0, 1.0]);
}

#[test]
fn joint_mask_with_a_short_parent_table_does_not_panic() {
    // A mask built for a longer skeleton than the parent table describes.
    let mask = JointMask {
        joints: vec![0],
        include_children: true,
    };
    let w = mask.weights(&[None], 3);
    assert_eq!(w, [1.0, 0.0, 0.0]);
    // A parent index past the end of the table is ignored too.
    let w = mask.weights(&[None, Some(7)], 2);
    assert_eq!(w, [1.0, 0.0]);
}

#[test]
fn empty_joint_mask_masks_nothing() {
    let mask = JointMask::default();
    assert_eq!(mask.weights(&[None, Some(0)], 2), [0.0, 0.0]);
    assert!(mask.weights(&[], 0).is_empty());
}
