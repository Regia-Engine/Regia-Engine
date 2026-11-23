// SPDX-License-Identifier: MPL-2.0
//! audit-v035-numerics M2 (fix-audit-wave12, ADR-2395): the two-bone, look-at, FABRIK, CCD and
//! body-aim solvers decomposed joint matrices without a zero-scale guard. A joint (or an
//! ancestor) scaled to zero on one axis — the first-person head-hide pattern — with the bones
//! along another axis passed every length check, decomposed to a NaN rotation and wrote NaN
//! quaternions into the pose. Each solve now leaves the pose unchanged. Failed on 34e3b4e5f.

use regia_animation::aim_ik::{AimChain, place_hand, solve_body_aim};
use regia_animation::chain_ik::{ChainIkSettings, JointChain, solve_ccd, solve_fabrik};
use regia_animation::ik::{IkChain, LookAtSettings, solve_look_at, solve_two_bone};
use regia_animation::pose::translated;
use regia_animation::{Pose, Skeleton};
use regia_core::math::{Mat4, Quat, Vec3};

/// A vertical column j0..j3, 0.5 m apart along +Y.
fn column() -> Skeleton {
    let names = (0..4).map(|i| format!("j{i}")).collect();
    let parents = vec![None, Some(0), Some(1), Some(2)];
    let binds = (0..4)
        .map(|i| {
            translated(if i == 0 {
                Vec3::ZERO
            } else {
                Vec3::new(0.0, 0.5, 0.0)
            })
        })
        .collect();
    Skeleton::from_local_bind(names, parents, binds).expect("valid skeleton")
}

/// The bind pose with the chain's parent `j0` flattened on X (bones run along Y).
fn flattened(skeleton: &Skeleton) -> Pose {
    let mut pose = skeleton.bind_pose_pose();
    pose.scales[0] = Vec3::new(0.0, 1.0, 1.0);
    pose
}

fn assert_unchanged(what: &str, before: &Pose, after: &Pose) {
    for (i, q) in after.rotations.iter().enumerate() {
        assert!(q.is_finite(), "{what}: joint {i} rotation is {q:?}");
    }
    assert_eq!(
        before.rotations, after.rotations,
        "{what}: the pose stays as it was"
    );
}

const TARGET: Vec3 = Vec3::new(0.3, 1.0, 0.2);

#[test]
fn two_bone_ik_under_a_zero_scaled_parent_leaves_the_pose() {
    let sk = column();
    let before = flattened(&sk);
    let mut pose = before.clone();
    let chain = IkChain {
        root: 1,
        mid: 2,
        tip: 3,
    };
    solve_two_bone(&sk, &mut pose, &chain, TARGET, None, 1.0);
    assert_unchanged("two-bone", &before, &pose);
}

#[test]
fn look_at_under_a_zero_scaled_parent_leaves_the_pose() {
    let sk = column();
    let before = flattened(&sk);
    let mut pose = before.clone();
    let settings = LookAtSettings {
        max_yaw: 1.0,
        max_pitch: 1.0,
        weight: 1.0,
    };
    solve_look_at(&sk, &mut pose, 3, Quat::IDENTITY, TARGET, settings);
    assert_unchanged("look-at", &before, &pose);
}

#[test]
fn fabrik_and_ccd_under_a_zero_scaled_parent_leave_the_pose() {
    let sk = column();
    let chain = JointChain::between(&sk, "j1", "j3").expect("chain");
    for weight in [1.0, 0.5] {
        let settings = ChainIkSettings {
            weight,
            ..ChainIkSettings::default()
        };
        let before = flattened(&sk);
        let mut pose = before.clone();
        let report = solve_fabrik(&sk, &mut pose, &chain, TARGET, settings);
        assert_unchanged("fabrik", &before, &pose);
        assert_eq!(report.error, f32::INFINITY, "fabrik reports the failed solve");
        let mut pose = before.clone();
        let report = solve_ccd(&sk, &mut pose, &chain, TARGET, settings);
        assert_unchanged("ccd", &before, &pose);
        assert_eq!(report.error, f32::INFINITY, "ccd reports the failed solve");
    }
}

/// The body-aim loop itself already skipped (its aim axis goes through `normalize_or_zero`,
/// which reads a NaN axis as none); hand placement wrote the NaN parent rotation into the hand.
#[test]
fn place_hand_under_a_zero_scaled_parent_leaves_the_pose() {
    let sk = column();
    let before = flattened(&sk);
    let mut pose = before.clone();
    let chain = IkChain {
        root: 1,
        mid: 2,
        tip: 3,
    };
    let target = Mat4::from_rotation_translation(Quat::IDENTITY, TARGET);
    let error = place_hand(&sk, &mut pose, &chain, target, None, 1.0);
    assert_unchanged("place hand", &before, &pose);
    assert_eq!(error, f32::INFINITY, "the failed placement is reported");
}

/// A guard: passed on the base too (see `place_hand_under_a_zero_scaled_parent_leaves_the_pose`).
#[test]
fn body_aim_under_a_zero_scaled_parent_leaves_the_pose() {
    let sk = column();
    let before = flattened(&sk);
    let mut pose = before.clone();
    let aim = AimChain {
        joints: vec![1, 2],
        shares: Vec::new(),
        limits: Vec::new(),
        aim_joint: 3,
        aim_axis: Vec3::Y,
    };
    let report = solve_body_aim(&sk, &mut pose, &aim, TARGET, &[], 1.0);
    assert_unchanged("body aim", &before, &pose);
    assert!(report.angle_error.is_finite());
}
