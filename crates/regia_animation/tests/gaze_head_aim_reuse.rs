// SPDX-License-Identifier: MPL-2.0
//! Head aim fills one skeleton-sized global buffer per gaze step.
//!
//! A 4-joint head chain used to run forward kinematics once per joint per pass. Thirty steps
//! of one rig fill the buffer thirty times.

use regia_animation::Skeleton;
use regia_animation::aim_ik::AimChain;
use regia_animation::gaze::{GazeRig, GazeState, HeadFollow, solve_gaze};
use regia_animation::hot_count::{self, globals_fills};
use regia_animation::pose::Pose;
use regia_animation::skeleton::LocalBind;
use regia_core::math::Vec3;

fn spine(n: usize) -> Skeleton {
    let mut names = Vec::with_capacity(n);
    let mut parents = Vec::with_capacity(n);
    let mut binds = Vec::with_capacity(n);
    for i in 0..n {
        names.push(format!("j{i}"));
        parents.push(if i == 0 { None } else { Some((i - 1) as u32) });
        binds.push(LocalBind {
            translation: Vec3::Y * 0.05,
            ..LocalBind::default()
        });
    }
    Skeleton::from_local_bind(names, parents, binds).expect("skeleton")
}

#[test]
fn head_aim_fills_globals_once_per_step() {
    let _guard = hot_count::lock_for_test();
    let skeleton = spine(64);
    let mut rig = GazeRig::new(1, 2);
    rig.head = Some(HeadFollow {
        chain: AimChain {
            joints: vec![60, 61, 62, 63],
            shares: Vec::new(),
            limits: Vec::new(),
            aim_joint: 63,
            aim_axis: Vec3::NEG_Z,
        },
        weight: 1.0,
    });
    let mut pose = Pose::from_identity(skeleton.len());
    let mut state = GazeState::default();
    let target = Vec3::new(0.2, 1.6, -2.0);
    let before = globals_fills();
    for _ in 0..30 {
        solve_gaze(
            &skeleton,
            &mut pose,
            &rig,
            &mut state,
            Some(target),
            1.0,
            1.0 / 30.0,
        );
    }
    let fills = globals_fills() - before;
    assert_eq!(
        fills, 30,
        "thirty head-aim steps filled the global buffer {fills} times"
    );
}
