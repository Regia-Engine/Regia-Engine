// SPDX-License-Identifier: MPL-2.0
//! Moving the stride and orientation warps out of motion matching (ADR-8540) changed no bits:
//! the motion-matching re-exports give the hash recorded on the base (0fd207bd5, before the move),
//! and the graph nodes' scratch variants give the same bits as the allocating functions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_animation::demo_rig::{DEMO_LEGS, DemoGait, DemoRig, demo_humanoid, demo_locomotion};
use regia_animation::foot_ik::FootIkLeg;
use regia_animation::ik::IkChain;
use regia_animation::motion_matching::warp::{
    OrientationWarpSettings, StrideMode, StrideWarp, StrideWarpSettings, apply_orientation_warp,
    apply_stride_pose, orientation_delta,
};
use regia_animation::motion_matching::warp::{
    StrideRequest, StrideWarpScratch, apply_orientation_warp_with_globals, apply_stride_pose_scratch,
};
use regia_animation::pose::Pose;
use regia_core::math::{Mat4, Vec3};

/// FNV-1a over every output of the motion-matching warp functions on fixed inputs, recorded by
/// running this same loop on the base before the move (x86_64 Linux).
const BASE_BITS: u64 = 0x796f_e70a_2f94_c0c6;

fn fnv(hash: &mut u64, bits: u32) {
    for byte in bits.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

fn hash_pose(hash: &mut u64, pose: &Pose) {
    for t in &pose.translations {
        for c in t.to_array() {
            fnv(hash, c.to_bits());
        }
    }
    for r in &pose.rotations {
        for c in r.to_array() {
            fnv(hash, c.to_bits());
        }
    }
}

fn mm_warp_hash() -> u64 {
    let skeleton = demo_humanoid(&DemoRig::default()).unwrap();
    let gaits = DemoGait::standard();
    let clips = demo_locomotion(&skeleton, &gaits);
    let legs: Vec<FootIkLeg> = DEMO_LEGS
        .iter()
        .map(|[h, k, a]| {
            FootIkLeg::new(IkChain {
                root: *h as u32,
                mid: *k as u32,
                tip: *a as u32,
            })
        })
        .collect();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for clip in &clips {
        for k in 0..12 {
            let t = k as f32 / 12.0 * clip.duration;
            for ratio in [0.8f32, 1.0, 1.17, 1.3, 1.5] {
                let mut pose = skeleton.bind_pose_pose();
                clip.sample_pose(t, &mut pose);
                let drop = apply_stride_pose(
                    &skeleton,
                    &mut pose,
                    &legs,
                    1,
                    Vec3::new(0.1, 0.0, -1.0),
                    ratio,
                    0.5,
                );
                fnv(&mut hash, drop.to_bits());
                hash_pose(&mut hash, &pose);
            }
            let settings = OrientationWarpSettings {
                strength: 0.8,
                max_delta: 0.7,
                spine_joints: vec![2, 3],
                spine_weights: vec![0.4, 0.6],
            };
            for desired in [-2.9f32, -0.5, 0.0, 0.3, 1.2, 3.1] {
                let delta = orientation_delta(&settings, desired, 0.2);
                fnv(&mut hash, delta.to_bits());
                let mut pose = skeleton.bind_pose_pose();
                clip.sample_pose(t, &mut pose);
                apply_orientation_warp(&skeleton, &mut pose, 1, &settings, delta);
                hash_pose(&mut hash, &pose);
            }
        }
    }
    for mode in [StrideMode::Rate, StrideMode::Pose] {
        let settings = StrideWarpSettings {
            strength: 0.9,
            ratio_max: 1.5,
            mode,
            ..StrideWarpSettings::default()
        };
        let mut warp = StrideWarp::default();
        for k in 0..200 {
            let actual = 1.0 + (k % 37) as f32 * 0.05;
            let r = warp.update(&settings, actual, 1.3, 1.0 / 60.0);
            fnv(&mut hash, r.to_bits());
            fnv(&mut hash, warp.rate_multiplier(mode).to_bits());
            fnv(&mut hash, warp.stride_ratio(mode).to_bits());
        }
    }
    hash
}

#[test]
fn motion_matching_warp_results_are_bit_identical() {
    let hash = mm_warp_hash();
    println!("MM_WARP_BITS=0x{hash:016x}");
    assert_eq!(hash, BASE_BITS, "the motion-matching warp outputs changed bits");

    // The graph nodes' scratch variants: the same bits as the allocating functions, with one
    // scratch kept across every call.
    let skeleton = demo_humanoid(&DemoRig::default()).unwrap();
    let clips = demo_locomotion(&skeleton, &DemoGait::standard());
    let legs: Vec<FootIkLeg> = DEMO_LEGS
        .iter()
        .map(|[h, k, a]| {
            FootIkLeg::new(IkChain {
                root: *h as u32,
                mid: *k as u32,
                tip: *a as u32,
            })
        })
        .collect();
    let mut scratch = StrideWarpScratch::default();
    let mut globals: Vec<Mat4> = Vec::new();
    let settings = OrientationWarpSettings {
        strength: 1.0,
        max_delta: 1.2,
        spine_joints: vec![2, 3],
        spine_weights: vec![0.5, 0.5],
    };
    for clip in &clips {
        for k in 0..10 {
            let t = k as f32 / 10.0 * clip.duration;
            let mut sampled = skeleton.bind_pose_pose();
            clip.sample_pose(t, &mut sampled);
            for ratio in [0.7f32, 1.2, 1.45] {
                let dir = Vec3::new(0.3, 0.0, -1.0);
                let mut a = sampled.clone();
                let mut b = sampled.clone();
                let drop_a = apply_stride_pose(&skeleton, &mut a, &legs, 1, dir, ratio, 0.45);
                let request = StrideRequest {
                    hips: 1,
                    movement_dir: dir,
                    ratio,
                    stride_length: 0.45,
                };
                let drop_b = apply_stride_pose_scratch(&skeleton, &mut b, &legs, request, &mut scratch);
                assert_eq!(drop_a.to_bits(), drop_b.to_bits());
                assert_pose_bits(&a, &b);
            }
            for delta in [-1.0f32, 0.25, 0.9] {
                let mut a = sampled.clone();
                let mut b = sampled.clone();
                apply_orientation_warp(&skeleton, &mut a, 1, &settings, delta);
                apply_orientation_warp_with_globals(&skeleton, &mut b, 1, &settings, delta, &mut globals);
                assert_pose_bits(&a, &b);
            }
        }
    }
}

fn assert_pose_bits(a: &Pose, b: &Pose) {
    let bits = |p: &Pose| {
        let mut v: Vec<u32> = Vec::new();
        for t in &p.translations {
            v.extend(t.to_array().map(f32::to_bits));
        }
        for r in &p.rotations {
            v.extend(r.to_array().map(f32::to_bits));
        }
        v
    };
    assert_eq!(bits(a), bits(b));
}
