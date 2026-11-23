// SPDX-License-Identifier: MPL-2.0
//! Spring bones keep their skeleton-sized scratch across frames.
//!
//! The second `update` of one chain must not allocate another world-position buffer, collider
//! list or per-substep quaternion vector.

use regia_animation::hot_count::{self, spring_buffer_allocs};
use regia_animation::skeleton::LocalBind;
use regia_animation::{SPRING_FORMAT_VERSION, Skeleton, SpringBoneSetup, SpringBones, SpringChain};
use regia_core::math::Vec3;
use regia_scene::Transform;

fn chain_skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["j0".into(), "j1".into(), "j2".into()],
        vec![None, Some(0), Some(1)],
        vec![
            LocalBind::default(),
            LocalBind {
                translation: Vec3::Y * 0.1,
                ..LocalBind::default()
            },
            LocalBind {
                translation: Vec3::Y * 0.1,
                ..LocalBind::default()
            },
        ],
    )
    .expect("skeleton")
}

#[test]
fn update_reuses_scratch_across_frames() {
    let _guard = hot_count::lock_for_test();
    let skeleton = chain_skeleton();
    let setup = SpringBoneSetup {
        format_version: SPRING_FORMAT_VERSION,
        step_hz: 60.0,
        lod: Default::default(),
        chains: vec![SpringChain {
            name: "hair".into(),
            joints: vec!["j1".into(), "j2".into()],
            stiffness: 0.2,
            damping: 0.1,
            gravity_dir: Vec3::NEG_Y,
            gravity_power: 0.0,
            hit_radius: 0.0,
            colliders: Vec::new(),
            enabled: true,
        }],
        colliders: Vec::new(),
    };
    let mut springs = SpringBones::new(setup);
    let mut pose = skeleton.bind_pose_pose();
    let rig = Transform::default();
    let dt = 1.0 / 60.0;
    springs.update(&skeleton, &mut pose, &rig, dt, None);
    let before = spring_buffer_allocs();
    springs.update(&skeleton, &mut pose, &rig, dt, None);
    let delta = spring_buffer_allocs() - before;
    assert_eq!(
        delta, 0,
        "the second spring update allocated {delta} scratch buffers"
    );
}
