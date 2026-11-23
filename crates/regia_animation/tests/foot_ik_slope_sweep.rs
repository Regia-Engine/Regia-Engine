// SPDX-License-Identifier: MPL-2.0
//! Showcase look check (a), ADR-2486: the demo walk on slopes of 0 to 25 degrees, with and without foot IK.
//!
//! The walk is authored on flat ground (planted feet at ankle height, swing feet lifted). On a slope the
//! character's capsule stands on the ground under its hips, so a planted foot ahead of the hips (uphill)
//! sinks into the ground and one behind floats. Each frame of one cycle is planned with [`plan_foot_ik`]
//! (contacts: each animated foot raised by the ground height under it, so swing lift is kept) and solved with
//! [`solve_foot_ik`]. The error is the distance of a planted foot from the slope surface (plus ankle height).
//! Prints `LOOK foot_ik slope=..` lines for the look-check report.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_animation::demo_rig::{DEMO_LEGS, DemoGait, DemoRig, demo_humanoid, demo_locomotion};
use regia_animation::foot_ik::{
    FootGround, FootIkInput, FootIkLeg, FootIkSettings, plan_foot_ik, solve_foot_ik,
};
use regia_animation::ik::{IkChain, joint_position};
use regia_core::math::{Quat, Vec3};

struct Sweep {
    /// Largest planted-foot error without IK, metres.
    without: f32,
    /// Largest planted-foot error with IK, metres.
    with: f32,
    /// Lowest pelvis offset the planner chose, metres.
    pelvis_min: f32,
    /// Frames where a planted leg got no IK weight (out of reach).
    unreached: usize,
    /// Planted-foot samples measured.
    planted: usize,
}

fn sweep(slope_deg: f32) -> Sweep {
    let skeleton = demo_humanoid(&DemoRig::default()).expect("demo rig");
    let walk = DemoGait::standard()
        .into_iter()
        .find(|g| g.name == "walk")
        .expect("walk gait");
    let clip = demo_locomotion(&skeleton, std::slice::from_ref(&walk))
        .pop()
        .expect("walk clip");
    let legs: Vec<FootIkLeg> = DEMO_LEGS
        .iter()
        .map(|[hip, knee, ankle]| {
            FootIkLeg::new(IkChain {
                root: *hip as u32,
                mid: *knee as u32,
                tip: *ankle as u32,
            })
        })
        .collect();
    let bind = skeleton.bind_pose_pose();
    let ankle_height = joint_position(&skeleton, &bind, DEMO_LEGS[0][2] as u32).y;
    let tan = slope_deg.to_radians().tan();
    let settings = FootIkSettings::default();
    let mut out = Sweep {
        without: 0.0,
        with: 0.0,
        pelvis_min: 0.0,
        unreached: 0,
        planted: 0,
    };
    let frames = 60;
    for k in 0..frames {
        let t = walk.cycle * k as f32 / frames as f32;
        let mut pose = bind.clone();
        clip.sample_pose(t, &mut pose);
        // The capsule stands on the slope under the hips: ground rises along the walk (−Z) from the hips.
        let hip_z = joint_position(&skeleton, &pose, 0).z;
        let ground = |z: f32| tan * (hip_z - z);
        let feet: Vec<Vec3> = legs
            .iter()
            .map(|l| joint_position(&skeleton, &pose, l.chain.tip))
            .collect();
        // Planted in the authored walk: at ankle height (no swing lift).
        let planted: Vec<bool> = feet.iter().map(|f| (f.y - ankle_height).abs() < 1e-3).collect();
        let contacts: Vec<FootGround> = feet
            .iter()
            .map(|f| FootGround::Contact(*f + Vec3::Y * ground(f.z)))
            .collect();
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &contacts,
            },
            &legs,
            &settings,
        );
        out.pelvis_min = out.pelvis_min.min(plan.pelvis_offset);
        let mut solved = pose.clone();
        solve_foot_ik(&skeleton, &mut solved, &plan, &legs, None);
        for (i, leg) in legs.iter().enumerate() {
            if !planted[i] {
                continue;
            }
            out.planted += 1;
            if plan.legs[i].weight <= 0.0 {
                out.unreached += 1;
            }
            let before = feet[i];
            out.without = out
                .without
                .max((before.y - (ground(before.z) + ankle_height)).abs());
            let after = joint_position(&skeleton, &solved, leg.chain.tip);
            out.with = out.with.max((after.y - (ground(after.z) + ankle_height)).abs());
        }
    }
    out
}

#[test]
fn foot_ik_plants_the_demo_walk_on_slopes() {
    for slope in [0.0f32, 5.0, 10.0, 15.0, 20.0, 25.0] {
        let s = sweep(slope);
        println!(
            "LOOK foot_ik slope={slope:>4.1}deg planted_samples={} max_error_without_ik={:.3}m max_error_with_ik={:.3}m pelvis_min={:.3}m unreached={}",
            s.planted, s.without, s.with, s.pelvis_min, s.unreached
        );
        assert!(s.planted > 20, "the walk has planted frames: {}", s.planted);
        if slope == 0.0 {
            assert!(
                s.without < 1e-3 && s.with < 1e-3,
                "flat ground is a no-op: {} {}",
                s.without,
                s.with
            );
        }
        // Up to 20 degrees every planted foot lands on the slope within a centimetre.
        if slope <= 20.0 {
            assert_eq!(s.unreached, 0, "slope {slope}: a planted leg was out of reach");
            assert!(
                s.with < 0.01,
                "slope {slope}: planted foot {} m off the ground",
                s.with
            );
            if slope > 0.0 {
                assert!(
                    s.with < s.without * 0.25,
                    "slope {slope}: IK {} vs none {}",
                    s.with,
                    s.without
                );
            }
        }
    }
}
