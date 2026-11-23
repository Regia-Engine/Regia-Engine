// SPDX-License-Identifier: MIT-0
//! Spec 17 § 6 acceptance, automated (ADR-7216): in the anim_lab, two-bone IK and look-at reach
//! targets that move every frame, and the turret aims at its moving target, all within 1 mm.
//!
//! No network and no downloaded model: the rigs are built in code and the graphs are the sample's
//! checked-in `assets/reach.animgraph.ron` and `assets/turret.animgraph.ron`. Fixed 16 ms frames;
//! the targets follow fixed paths indexed by frame number, so every run is the same.
//!
//! Bounds. IK: the hand joint's position is within 1 mm of the target. Look-at and turret: the
//! aimed joint's −Z ray passes within 1 mm of the target (the perpendicular distance from the
//! target to the ray, which is the 1 mm bound expressed at the joint-to-target distance: at 2 m
//! it is an angle of 0.5 mrad). Every frame is measured after that frame's update: the solvers
//! read the parameters set before the update, so there is no one-frame lag to allow for.

use anim_lab::reach::{self, FRAMES, MISS_BOUND};

#[test]
fn two_bone_ik_and_look_at_reach_moving_targets_within_one_millimeter() {
    let report = reach::run_character(FRAMES).expect("the reach case runs");
    eprintln!("{report:?}");
    assert_eq!(report.frames, FRAMES);
    assert!(
        report.ik_worst <= MISS_BOUND,
        "IK missed a moving hand target by {:.6} m at frame {}",
        report.ik_worst,
        report.ik_worst_frame
    );
    assert!(
        report.look_worst <= MISS_BOUND,
        "look-at missed a moving target by {:.6} m at frame {}",
        report.look_worst,
        report.look_worst_frame
    );
    // The targets really moved, and the measurement is not trivially satisfied.
    assert!(
        report.hand_travel > 0.5,
        "hand target travelled {} m",
        report.hand_travel
    );
    assert!(report.look_sweep > 0.5, "head turned {} rad", report.look_sweep);
    assert!(
        report.ik_off_miss > 0.05,
        "with IK weight 0 the hand still sits {} m from the target",
        report.ik_off_miss
    );
    assert!(
        report.look_off_miss > 0.05,
        "with look weight 0 the head ray still passes {} m from the target",
        report.look_off_miss
    );
}

#[test]
fn the_turret_aims_at_a_moving_target_within_one_millimeter() {
    let report = reach::run_turret(FRAMES).expect("the turret case runs");
    eprintln!("{report:?}");
    assert_eq!(report.frames, FRAMES);
    assert!(
        report.worst <= MISS_BOUND,
        "the turret barrel missed AimTarget by {:.6} m at frame {}",
        report.worst,
        report.worst_frame
    );
    assert!(report.sweep > 1.0, "barrel turned {} rad", report.sweep);
}

#[test]
fn the_reach_graphs_are_the_checked_in_fixtures() {
    assert!(reach::REACH_GRAPH_RON.contains("TwoBoneIK"));
    assert!(reach::TURRET_GRAPH_RON.contains("AimTarget"));
    reach::reach_graph().expect("reach.animgraph.ron parses");
    reach::turret_graph().expect("turret.animgraph.ron parses");
}
