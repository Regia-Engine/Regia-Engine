// SPDX-License-Identifier: MPL-2.0
//! Hand-tracking snapshots (spec 58 § 2.7) and the CPU parts of the eye targets (`box_resize`,
//! the acquire/release tracker). No GPU: `SimEyeTargets` itself is not touched.

use regia_core::glam::{Quat, Vec3};
use regia_platform::xr::{Eye, FramePhase, Hand, Pose, XrError};
use regia_xr::eye_targets::AcquireTracker;
use regia_xr::{HAND_JOINT_COUNT, HandJoint, HandTrackingSupport, JointLocation, XrHands, box_resize};

fn tracked(x: f32) -> JointLocation {
    JointLocation {
        pose: Pose::new(Vec3::new(x, 1.0, -0.2), Quat::IDENTITY),
        radius: 0.01,
        position_valid: true,
        orientation_valid: true,
        tracked: true,
    }
}

/// Catches an enum reorder: the joint order is the OpenXR hand-tracking standard's, and every
/// finger's metacarpal and tip index is fixed by it.
#[test]
fn joint_indices_follow_the_standard_for_every_finger() {
    let expected = [
        (HandJoint::Palm, 0),
        (HandJoint::Wrist, 1),
        (HandJoint::ThumbMetacarpal, 2),
        (HandJoint::ThumbProximal, 3),
        (HandJoint::ThumbDistal, 4),
        (HandJoint::ThumbTip, 5),
        (HandJoint::IndexMetacarpal, 6),
        (HandJoint::IndexProximal, 7),
        (HandJoint::IndexIntermediate, 8),
        (HandJoint::IndexDistal, 9),
        (HandJoint::IndexTip, 10),
        (HandJoint::MiddleMetacarpal, 11),
        (HandJoint::MiddleProximal, 12),
        (HandJoint::MiddleIntermediate, 13),
        (HandJoint::MiddleDistal, 14),
        (HandJoint::MiddleTip, 15),
        (HandJoint::RingMetacarpal, 16),
        (HandJoint::RingProximal, 17),
        (HandJoint::RingIntermediate, 18),
        (HandJoint::RingDistal, 19),
        (HandJoint::RingTip, 20),
        (HandJoint::LittleMetacarpal, 21),
        (HandJoint::LittleProximal, 22),
        (HandJoint::LittleIntermediate, 23),
        (HandJoint::LittleDistal, 24),
        (HandJoint::LittleTip, 25),
    ];
    assert_eq!(expected.len(), HAND_JOINT_COUNT);
    for (joint, index) in expected {
        assert_eq!(joint.index(), index, "{joint:?}");
        assert_eq!(HandJoint::ALL[index], joint);
    }
}

/// Catches a default that claims tracking: a fresh snapshot is unsupported and every joint is
/// the invalid origin joint, on both hands.
#[test]
fn a_fresh_snapshot_is_unsupported_with_invalid_joints() {
    let hands = XrHands::default();
    assert_eq!(hands.support, HandTrackingSupport::Unsupported);
    assert_eq!(hands.bad_joints(), 0);
    for hand in Hand::BOTH {
        assert_eq!(hands.joints(hand).len(), HAND_JOINT_COUNT);
        for joint in HandJoint::ALL {
            assert_eq!(hands.joint(hand, joint), JointLocation::INVALID);
        }
    }
    let invalid = JointLocation::INVALID;
    assert!(!invalid.position_valid && !invalid.tracked);
}

/// Catches the two hands sharing storage.
#[test]
fn updating_one_hand_leaves_the_other_alone() {
    let mut hands = XrHands::default();
    let mut left = [tracked(0.1); HAND_JOINT_COUNT];
    left[HandJoint::Wrist.index()] = tracked(0.7);
    assert_eq!(hands.update(Hand::Left, &left), 0);
    assert_eq!(hands.joint(Hand::Left, HandJoint::Wrist).pose.position.x, 0.7);
    assert_eq!(hands.joint(Hand::Left, HandJoint::Palm).pose.position.x, 0.1);
    assert_eq!(hands.joint(Hand::Right, HandJoint::Wrist), JointLocation::INVALID);
    let right = [tracked(-0.4); HAND_JOINT_COUNT];
    assert_eq!(hands.update(Hand::Right, &right), 0);
    assert_eq!(
        hands.joint(Hand::Left, HandJoint::Wrist).pose.position.x,
        0.7,
        "left kept"
    );
    assert_eq!(hands.joint(Hand::Right, HandJoint::Wrist).pose.position.x, -0.4);
}

/// Catches a finiteness check that looks at the position only: orientation, radius and infinities
/// are rejected too, each counted, and the previous value stays (the invalid origin joint on a
/// first update).
#[test]
fn every_non_finite_component_is_rejected_and_counted_across_updates() {
    let mut hands = XrHands::default();
    let mut located = [tracked(0.2); HAND_JOINT_COUNT];
    located[HandJoint::ThumbTip.index()].pose.orientation = Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0);
    located[HandJoint::IndexTip.index()].radius = f32::INFINITY;
    located[HandJoint::MiddleTip.index()].pose.position.y = f32::NEG_INFINITY;
    assert_eq!(hands.update(Hand::Left, &located), 3);
    assert_eq!(hands.bad_joints(), 3);
    for joint in [HandJoint::ThumbTip, HandJoint::IndexTip, HandJoint::MiddleTip] {
        assert_eq!(
            hands.joint(Hand::Left, joint),
            JointLocation::INVALID,
            "{joint:?}: nothing good to keep on the first update"
        );
    }
    assert_eq!(hands.joint(Hand::Left, HandJoint::RingTip), tracked(0.2));
    let mut again = [tracked(0.5); HAND_JOINT_COUNT];
    again[HandJoint::RingTip.index()].radius = f32::NAN;
    again[HandJoint::LittleTip.index()].pose.position.z = f32::NAN;
    assert_eq!(hands.update(Hand::Left, &again), 2);
    assert_eq!(hands.bad_joints(), 5, "the counter accumulates");
    assert_eq!(
        hands.joint(Hand::Left, HandJoint::RingTip),
        tracked(0.2),
        "previous kept"
    );
    assert_eq!(
        hands.joint(Hand::Left, HandJoint::Palm),
        tracked(0.5),
        "the good joints moved on"
    );
    assert_eq!(hands.bad_joints(), 5);
}

/// Catches an off-by-one in the pair-by-pair stride: three rows of RGBA, hand-computed.
#[test]
fn box_resize_non_integer_ratios() {
    let row = |vals: &[u8]| -> Vec<u8> { vals.iter().flat_map(|&v| [v, v, v, v]).collect() };
    // 2 -> 3: the middle output pixel is half of each source pixel.
    assert_eq!(box_resize(&row(&[0, 90]), 2, 1, 3, 1), Some(row(&[0, 45, 90])));
    // 4 -> 3: the weights are (3,1), (2,2), (1,3) over a total of 4.
    assert_eq!(
        box_resize(&row(&[0, 12, 24, 36]), 4, 1, 3, 1),
        Some(row(&[3, 18, 33]))
    );
    // The same on the vertical axis (1 wide, 4 tall -> 3 tall) pins the row stride.
    assert_eq!(
        box_resize(&row(&[0, 12, 24, 36]), 1, 4, 1, 3),
        Some(row(&[3, 18, 33]))
    );
    // 2 tall -> 3 tall.
    assert_eq!(box_resize(&row(&[0, 90]), 1, 2, 1, 3), Some(row(&[0, 45, 90])));
}

/// Catches channel mixing and rounding errors: a 2x2 block collapses per channel, rounding half
/// up, and alpha is averaged like the colours.
#[test]
fn box_resize_averages_each_channel_independently_with_round_half_up() {
    // Pixels (row major): ch0 = 0,255,255,0 (mean 127.5 -> 128); ch1 = 10,20,30,41 (25.25 -> 25);
    // ch2 = 1,1,1,2 (1.25 -> 1); ch3 = 0,0,0,3 (0.75 -> 1).
    let src = [0, 10, 1, 0, 255, 20, 1, 0, 255, 30, 1, 0, 0, 41, 2, 3];
    assert_eq!(box_resize(&src, 2, 2, 1, 1), Some(vec![128, 25, 1, 1]));
}

/// Catches x/y swaps and a wrong stride: identity at several rectangular sizes, and a 4x2 block
/// halved in each direction keeps its quadrants.
#[test]
fn box_resize_keeps_row_major_layout() {
    for (w, h) in [(3_u32, 5_u32), (7, 2), (1, 9), (6, 6)] {
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        assert_eq!(box_resize(&src, w, h, w, h).as_deref(), Some(&src[..]), "{w}x{h}");
    }
    // 4x2 -> 2x1: left half 10s, right half 200s stay separate, so columns are not mixed.
    let mut src = Vec::new();
    for _ in 0..2 {
        for x in 0..4 {
            let v = if x < 2 { 10 } else { 200 };
            src.extend_from_slice(&[v, v, v, 255]);
        }
    }
    assert_eq!(
        box_resize(&src, 4, 2, 2, 1),
        Some(vec![10, 10, 10, 255, 200, 200, 200, 255])
    );
    // 2x4 -> 1x2: top half 10s, bottom half 200s.
    let mut src = Vec::new();
    for y in 0..4 {
        let v = if y < 2 { 10 } else { 200 };
        for _ in 0..2 {
            src.extend_from_slice(&[v, v, v, 255]);
        }
    }
    assert_eq!(
        box_resize(&src, 2, 4, 1, 2),
        Some(vec![10, 10, 10, 255, 200, 200, 200, 255])
    );
}

/// Catches rounding bias: a flat image survives any resize exactly, and the output length is
/// always `nw * nh * 4`.
#[test]
fn box_resize_preserves_flat_colour_for_awkward_sizes() {
    let pixel = [13_u8, 99, 201, 250];
    for (w, h, nw, nh) in [
        (7_u32, 5_u32, 3_u32, 4_u32),
        (1, 1, 5, 7),
        (100, 3, 7, 1),
        (5, 5, 5, 11),
        (9, 9, 2, 2),
    ] {
        let src: Vec<u8> = pixel.iter().copied().cycle().take((w * h * 4) as usize).collect();
        let out = box_resize(&src, w, h, nw, nh).expect("resizes");
        assert_eq!(out.len(), (nw * nh * 4) as usize);
        for chunk in out.as_chunks::<4>().0 {
            assert_eq!(chunk, &pixel, "{w}x{h} -> {nw}x{nh}");
        }
    }
}

/// Catches a length check that accepts a source that is too long, or a zero axis slipping through.
#[test]
fn box_resize_refuses_bad_inputs() {
    let px = [1_u8, 2, 3, 4];
    let two = [1_u8, 2, 3, 4, 5, 6, 7, 8];
    assert!(box_resize(&px, 1, 1, 1, 1).is_some());
    assert_eq!(box_resize(&two, 1, 1, 1, 1), None, "too long");
    assert_eq!(box_resize(&px[..3], 1, 1, 1, 1), None, "too short");
    assert_eq!(box_resize(&px, 0, 1, 1, 1), None);
    assert_eq!(box_resize(&px, 1, 0, 1, 1), None);
    assert_eq!(box_resize(&px, 1, 1, 1, 0), None);
    assert_eq!(box_resize(&[], 0, 0, 1, 1), None);
}

/// Catches tracker bookkeeping errors: errors carry the right call and phase, a failed release
/// does not count, an eye can be acquired again after release, and the eyes are independent.
#[test]
fn the_acquire_tracker_errors_and_counts() {
    let mut t = AcquireTracker::default();
    assert_eq!(t.releases(), 0);
    assert_eq!(
        t.release(Eye::Right),
        Err(XrError::FrameOutOfOrder {
            call: "release_eye",
            phase: FramePhase::Idle
        })
    );
    assert_eq!(t.releases(), 0, "a failed release is not counted");
    t.acquire(Eye::Right).expect("acquire");
    assert_eq!(
        t.acquire(Eye::Right),
        Err(XrError::FrameOutOfOrder {
            call: "acquire_eye",
            phase: FramePhase::Begun
        })
    );
    assert!(t.is_acquired(Eye::Right) && !t.is_acquired(Eye::Left));
    t.release(Eye::Right).expect("release");
    assert_eq!(
        t.release(Eye::Left).map_err(|e| e.code()),
        Err("xr_frame_out_of_order")
    );
    for round in 1..=3_u64 {
        for eye in Eye::BOTH {
            t.acquire(eye).expect("acquire again");
        }
        for eye in Eye::BOTH {
            t.release(eye).expect("release");
        }
        assert_eq!(t.releases(), 1 + 2 * round);
    }
}
