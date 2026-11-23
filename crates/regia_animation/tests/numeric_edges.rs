// SPDX-License-Identifier: MPL-2.0
//! Edge cases of the small numeric building blocks that read authored or game-supplied data:
//! transition curves, montage blend progress, comparison operators, angle wrapping, warp frames
//! and the physics pose blend. Everything is deterministic; NaN and infinity are inputs here
//! because they reach these functions from graph files and gameplay code.

use regia_animation::Skeleton;
use regia_animation::montage::MontageBlend;
use regia_animation::params::ParameterSet;
use regia_animation::physics_blend::{PhysicsPoseBlend, blend_physics_pose};
use regia_animation::pose::translated;
use regia_animation::state_machine::{CmpOp, Condition, StateMachineDef, TransitionBlend, TransitionCurve};
use regia_animation::warp::{WarpInput, WarpMode, warp_frame, wrap_angle, yaw_of, yaw_toward};
use regia_core::math::{Quat, Vec3, det};

#[test]
fn transition_curve_alpha_is_always_a_unit_weight() {
    for curve in [TransitionCurve::Linear, TransitionCurve::Smooth] {
        for t in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0, 7.0] {
            let a = curve.alpha(t);
            assert!(
                a.is_finite() && (0.0..=1.0).contains(&a),
                "{curve:?}.alpha({t}) = {a}"
            );
        }
    }
    assert_eq!(TransitionCurve::Linear.alpha(f32::INFINITY), 1.0);
    assert_eq!(TransitionCurve::Smooth.alpha(f32::NEG_INFINITY), 0.0);
}

#[test]
fn montage_blend_progress_survives_non_finite_input() {
    let blend = MontageBlend {
        time: 0.5,
        curve: TransitionCurve::Linear,
    };
    assert_eq!(blend.progress(0.25), 0.5);
    assert_eq!(
        blend.progress(f32::NAN),
        0.0,
        "a NaN clock shows no blend, not a NaN weight"
    );
    assert_eq!(blend.progress(f32::INFINITY), 1.0);
    for time in [f32::NAN, 0.0, -1.0] {
        let b = MontageBlend {
            time,
            curve: TransitionCurve::Smooth,
        };
        assert_eq!(b.progress(0.1), 1.0, "time {time} blends immediately");
    }
    let inf = MontageBlend {
        time: f32::INFINITY,
        curve: TransitionCurve::Linear,
    };
    assert_eq!(inf.progress(1.0), 0.0);
}

#[test]
fn comparison_operators_agree_with_their_complements() {
    let values = [
        0.0,
        1.0,
        -1.0,
        1e-7,
        5e-7,
        2e-6,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
    ];
    for &a in &values {
        for &b in &values {
            assert_eq!(
                CmpOp::Eq.test_f32(a, b),
                !CmpOp::Ne.test_f32(a, b),
                "Eq and Ne must be complements for ({a}, {b})"
            );
            assert_eq!(CmpOp::Lt.test_f32(a, b), CmpOp::Gt.test_f32(b, a));
            assert_eq!(CmpOp::Le.test_f32(a, b), CmpOp::Ge.test_f32(b, a));
        }
    }
    assert!(
        CmpOp::Eq.test_f32(f32::INFINITY, f32::INFINITY),
        "inf equals itself"
    );
    assert!(CmpOp::Eq.test_f32(1.0, 1.0 + 5e-7));
    assert!(!CmpOp::Eq.test_f32(1.0, 1.0 + 1e-3));
    assert!(CmpOp::Ne.test_f32(f32::NAN, 0.0));
}

#[test]
fn integer_comparisons_at_the_extremes() {
    assert!(CmpOp::Lt.test_i32(i32::MIN, i32::MAX));
    assert!(CmpOp::Ge.test_i32(i32::MAX, i32::MAX));
    assert!(CmpOp::Ne.test_i32(i32::MIN, i32::MAX));
    assert!(CmpOp::Eq.test_i32(i32::MIN, i32::MIN));
    assert!(!CmpOp::Gt.test_i32(i32::MIN, i32::MIN));
}

#[test]
fn conditions_hold_edges() {
    let def = StateMachineDef {
        initial: "A".into(),
        states: vec![],
        transitions: vec![],
    };
    let mut params = ParameterSet::new();
    assert!(
        def.conditions_hold(&[], &params, false, &[]),
        "no conditions always hold"
    );
    // Absent parameters read as zero / false.
    let absent = [Condition::Float {
        param: "missing".into(),
        op: CmpOp::Eq,
        value: 0.0,
    }];
    assert!(def.conditions_hold(&absent, &params, false, &[]));
    assert!(!def.conditions_hold(
        &[Condition::Trigger {
            param: "missing".into()
        }],
        &params,
        false,
        &[]
    ));
    // A Notify must match exactly, case-sensitively.
    let notify = [Condition::Notify("Step".into())];
    assert!(!def.conditions_hold(&notify, &params, false, &["step".to_string()]));
    assert!(def.conditions_hold(&notify, &params, false, &["a".to_string(), "Step".to_string()]));
    // Checking a trigger does not consume it; consuming only touches trigger conditions.
    params.set_trigger("Jump");
    params.set_bool("Flag", true);
    let conds = [
        Condition::Trigger { param: "Jump".into() },
        Condition::Bool {
            param: "Flag".into(),
            value: true,
        },
    ];
    assert!(def.conditions_hold(&conds, &params, false, &[]));
    assert!(
        def.conditions_hold(&conds, &params, false, &[]),
        "checking is idempotent"
    );
    StateMachineDef::consume_condition_triggers(&conds, &mut params);
    assert!(!params.trigger("Jump"));
    assert!(params.bool("Flag"), "non-trigger parameters are left alone");
    assert!(!def.conditions_hold(&conds, &params, false, &[]));
}

#[test]
fn transition_blend_names_and_decay_round_trip() {
    assert_eq!(TransitionBlend::ALL.len(), 3);
    let names: Vec<_> = TransitionBlend::ALL.iter().map(|b| b.as_str()).collect();
    assert_eq!(names, ["crossfade", "inertialize", "inertialize_spring"]);
    for b in TransitionBlend::ALL {
        assert_eq!(b.is_inertial(), b.decay().is_some());
    }
}

#[test]
fn wrap_angle_stays_in_range_for_every_finite_input() {
    let pi = std::f32::consts::PI;
    for a in [
        0.0,
        1.0,
        -1.0,
        pi,
        -pi,
        3.0 * pi,
        -3.0 * pi,
        1e6,
        -1e6,
        1e-9,
        -1e-9,
        f32::MAX,
        f32::MIN,
    ] {
        let w = wrap_angle(a);
        assert!(w > -pi - 1e-6 && w <= pi + 1e-6, "wrap_angle({a}) = {w}");
    }
    assert_eq!(wrap_angle(0.5), 0.5);
    assert!((wrap_angle(2.0 * pi + 0.5) - 0.5).abs() < 1e-4);
    assert!((wrap_angle(-2.0 * pi - 0.5) + 0.5).abs() < 1e-4);
    // Non-finite angles have no meaningful wrap; they must not escape as NaN.
    assert_eq!(wrap_angle(f32::NAN), 0.0);
    assert_eq!(wrap_angle(f32::INFINITY), 0.0);
    assert_eq!(wrap_angle(f32::NEG_INFINITY), 0.0);
}

#[test]
fn yaw_helpers() {
    assert_eq!(yaw_toward(Vec3::Y), None, "straight up has no heading");
    assert_eq!(yaw_toward(Vec3::ZERO), None);
    assert!((yaw_toward(Vec3::NEG_Z).expect("forward")).abs() < 1e-6);
    let toward_x = yaw_toward(Vec3::X).expect("heading");
    let rotated = Quat::from_rotation_y(toward_x) * Vec3::NEG_Z;
    assert!((rotated - Vec3::X).length() < 1e-5, "{rotated}");
    assert!((yaw_of(Quat::from_rotation_y(0.7)) - 0.7).abs() < 1e-5);
    assert!((yaw_toward(Vec3::new(1.0, 5.0, 0.0)).expect("flat part") - toward_x).abs() < 1e-6);
}

fn base_input() -> WarpInput {
    WarpInput {
        frame: (Vec3::new(0.0, 0.0, -0.1), Quat::IDENTITY),
        rest: (Vec3::new(0.0, 0.0, -0.9), Quat::IDENTITY),
        to_target: Vec3::new(0.0, 0.0, -2.0),
        yaw_to_target: None,
        frame_time: 0.1,
        rest_time: 0.9,
        horizontal: true,
        vertical: false,
        mode: WarpMode::Similarity,
        rotation_frame_time: 0.1,
        rotation_rest_time: 0.9,
    }
}

#[test]
fn warp_frame_scales_a_straight_dash_and_is_deterministic() {
    let input = base_input();
    let (t, r) = warp_frame(&input);
    assert!(
        (t.z + 0.2).abs() < 1e-5,
        "a 1 m dash toward a 2 m target doubles: {t}"
    );
    assert_eq!(r, Quat::IDENTITY);
    assert_eq!(warp_frame(&input), (t, r), "repeated calls agree bit for bit");
}

#[test]
fn warp_frame_returns_the_frame_unchanged_for_bad_input() {
    let base = base_input();
    let bad_inputs = [
        WarpInput {
            to_target: Vec3::new(f32::NAN, 0.0, 0.0),
            ..base
        },
        WarpInput {
            frame_time: f32::NAN,
            ..base
        },
        WarpInput {
            rest_time: f32::INFINITY,
            ..base
        },
        WarpInput {
            frame: (Vec3::new(f32::INFINITY, 0.0, 0.0), Quat::IDENTITY),
            ..base
        },
        WarpInput {
            frame: (Vec3::Z, Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0)),
            ..base
        },
        WarpInput {
            rest: (Vec3::new(0.0, f32::NAN, 0.0), Quat::IDENTITY),
            ..base
        },
        WarpInput {
            frame_time: 0.0,
            ..base
        },
        WarpInput {
            frame_time: -1.0,
            ..base
        },
    ];
    for input in bad_inputs {
        let out = warp_frame(&input);
        assert_eq!(
            out.0.to_array().map(f32::to_bits),
            input.frame.0.to_array().map(f32::to_bits)
        );
        assert_eq!(
            out.1.to_array().map(f32::to_bits),
            input.frame.1.to_array().map(f32::to_bits)
        );
    }
}

#[test]
fn warp_frame_with_a_target_already_reached_stops_horizontal_motion() {
    let input = WarpInput {
        to_target: Vec3::new(0.0, 0.0, 1e-5),
        ..base_input()
    };
    let (t, _) = warp_frame(&input);
    assert_eq!((t.x, t.z), (0.0, 0.0));
}

#[test]
fn warp_frame_with_no_authored_motion_distributes_the_offset_by_time_share() {
    // Nothing authored (a = 0): the whole offset is spread over the window by time share.
    let input = WarpInput {
        frame: (Vec3::ZERO, Quat::IDENTITY),
        rest: (Vec3::ZERO, Quat::IDENTITY),
        to_target: Vec3::new(1.0, 0.0, 0.0),
        ..base_input()
    };
    let (t, _) = warp_frame(&input);
    assert!((t.x - 0.1).abs() < 1e-5, "{t}");
    // A zero-length rest: the frame takes everything.
    let last = WarpInput {
        rest_time: 0.0,
        ..input
    };
    assert!((warp_frame(&last).0.x - 1.0).abs() < 1e-5);
    // A negative rest time is treated as none.
    let negative = WarpInput {
        rest_time: -4.0,
        ..input
    };
    assert!((warp_frame(&negative).0.x - 1.0).abs() < 1e-5);
}

#[test]
fn warp_frame_vertical_closes_the_height_gap_and_rotation_turns_toward_the_target() {
    let input = WarpInput {
        horizontal: false,
        vertical: true,
        to_target: Vec3::new(0.0, 1.0, 0.0),
        yaw_to_target: Some(1.0),
        rotation_frame_time: 0.5,
        rotation_rest_time: 0.0,
        ..base_input()
    };
    let (t, r) = warp_frame(&input);
    assert!((t.y - 0.1).abs() < 1e-5, "10% of a 1 m climb: {t}");
    assert_eq!((t.x, t.z), (0.0, -0.1), "horizontal untouched");
    assert!(
        (yaw_of(r) - 1.0).abs() < 1e-4,
        "the last rotation frame completes the turn"
    );
    // A non-finite needed yaw leaves the authored rotation alone.
    let nan = WarpInput {
        yaw_to_target: Some(f32::NAN),
        ..input
    };
    assert_eq!(warp_frame(&nan).1, Quat::IDENTITY);
    // No rotation time in this frame: no rotation warp.
    let none = WarpInput {
        rotation_frame_time: 0.0,
        ..input
    };
    assert_eq!(warp_frame(&none).1, Quat::IDENTITY);
}

/// A `Similarity` warp of a non-axis-aligned heading and a yaw goal, pinned bit for bit. The
/// expected values are rebuilt with `regia_core::math::det` only (never the platform libm), so
/// the pin holds on every target and catches a regression back to libm trig (audit-v040 M3).
#[test]
fn similarity_warp_heading_is_pinned() {
    let rest_rotation = det::quat_from_rotation_y(0.2);
    let input = WarpInput {
        frame: (Vec3::new(0.03, 0.0, -0.1), Quat::IDENTITY),
        rest: (Vec3::new(0.09, 0.0, -0.3), rest_rotation),
        to_target: Vec3::new(0.2, 0.0, -0.05),
        yaw_to_target: Some(0.4),
        frame_time: 0.1,
        rest_time: 0.3,
        horizontal: true,
        vertical: true,
        mode: WarpMode::Similarity,
        rotation_frame_time: 0.1,
        rotation_rest_time: 0.3,
    };
    let (t, r) = warp_frame(&input);
    // Rebuild `warp_frame`'s math on the same inputs.
    let remaining = input.frame.0 + input.frame.1 * input.rest.0;
    let share = (input.frame_time / (input.frame_time + input.rest_time)).clamp(0.0, 1.0);
    let a = Vec3::new(remaining.x, 0.0, remaining.z);
    let b = Vec3::new(input.to_target.x, 0.0, input.to_target.z);
    let flat = Vec3::new(input.frame.0.x, 0.0, input.frame.0.z);
    let angle = wrap_angle(det::atan2(-b.x, -b.z) - det::atan2(-a.x, -a.z));
    let scale = b.length() / a.length();
    let warped = det::quat_from_rotation_y(angle) * flat * scale;
    let expected_t = Vec3::new(
        warped.x,
        input.frame.0.y + (input.to_target.y - remaining.y) * share,
        warped.z,
    );
    let authored = det::quat_to_euler_yxz(input.frame.1 * rest_rotation).0;
    let rotation_share =
        (input.rotation_frame_time / (input.rotation_frame_time + input.rotation_rest_time)).clamp(0.0, 1.0);
    let yaw = det::quat_to_euler_yxz(input.frame.1).0 + wrap_angle(0.4 - authored) * rotation_share;
    let expected_r = det::quat_from_rotation_y(yaw);
    assert_eq!(
        t.to_array().map(f32::to_bits),
        expected_t.to_array().map(f32::to_bits),
        "the warped translation is pinned: {t}"
    );
    assert_eq!(
        r.to_array().map(f32::to_bits),
        expected_r.to_array().map(f32::to_bits),
        "the warped rotation is pinned: {r}"
    );
    // The heading actually turned (the input is not axis-aligned, so the pin is meaningful).
    assert!(angle.abs() > 1e-3, "the heading turn is non-trivial: {angle}");
}

fn chain() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["pelvis".into(), "spine".into(), "head".into()],
        vec![None, Some(0), Some(1)],
        vec![
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(0.0, 0.5, 0.0)),
            translated(Vec3::new(0.0, 0.5, 0.0)),
        ],
    )
    .expect("skeleton")
}

#[test]
fn physics_blend_max_weight_ignores_unsimulated_and_bad_weights() {
    let mut blend = PhysicsPoseBlend::new(3);
    assert_eq!(blend.max_weight(), 0.0);
    blend.weights = vec![0.9, f32::NAN, 5.0];
    assert_eq!(
        blend.max_weight(),
        0.0,
        "weights without a simulated joint do not count"
    );
    blend.joints[1] = Some((Vec3::ZERO, Quat::IDENTITY));
    assert_eq!(blend.max_weight(), 0.0, "a NaN weight counts as none");
    blend.joints[2] = Some((Vec3::ZERO, Quat::IDENTITY));
    assert_eq!(blend.max_weight(), 1.0, "weights clamp to 1");
    assert_eq!(PhysicsPoseBlend::new(0).max_weight(), 0.0);
}

#[test]
fn physics_blend_tolerates_mismatched_lengths_and_bad_weights() {
    let sk = chain();
    let bind = sk.bind_pose_pose();
    let lying = Quat::from_rotation_x(-1.0);

    // Blend arrays shorter than the skeleton: the missing joints stay animated.
    let mut short = PhysicsPoseBlend {
        joints: vec![Some((Vec3::new(0.0, 0.2, 0.0), lying))],
        weights: vec![1.0],
    };
    let mut pose = bind.clone();
    blend_physics_pose(&sk, &mut pose, &short);
    assert_eq!(pose.len(), 3);
    assert!(pose.rotations.iter().all(|q| q.is_finite()));
    assert_eq!(pose.rotations[1], Quat::IDENTITY);

    // A simulated joint with no weight entry shows nothing.
    short.weights.clear();
    let mut pose = bind.clone();
    blend_physics_pose(&sk, &mut pose, &short);
    assert_eq!(pose, bind);

    // NaN and out-of-range weights never poison the pose.
    let mut blend = PhysicsPoseBlend::new(3);
    blend.joints[0] = Some((Vec3::new(0.0, 0.2, 0.0), lying));
    blend.joints[1] = Some((Vec3::ZERO, lying));
    blend.weights = vec![f32::NAN, 7.0, -3.0];
    let mut pose = bind.clone();
    blend_physics_pose(&sk, &mut pose, &blend);
    assert!(
        pose.rotations
            .iter()
            .all(|q| q.is_finite() && (q.length() - 1.0).abs() < 1e-4)
    );
    assert!(pose.translations.iter().all(|t| t.is_finite()));
    assert_eq!(
        pose.translations[0], bind.translations[0],
        "NaN weight leaves the pelvis alone"
    );

    // A pose shorter than the skeleton is blended over its own length without panicking.
    let mut truncated = bind.clone();
    truncated.translations.truncate(2);
    truncated.rotations.truncate(2);
    truncated.scales.truncate(2);
    blend.weights = vec![1.0; 3];
    blend_physics_pose(&sk, &mut truncated, &blend);
    assert_eq!(truncated.len(), 2);
}

#[test]
fn physics_blend_is_deterministic_and_idempotent_at_full_weight() {
    let sk = chain();
    let mut blend = PhysicsPoseBlend::new(3);
    let lying = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
    blend.joints[0] = Some((Vec3::new(0.0, 0.2, 0.0), lying));
    blend.weights = vec![1.0, 0.0, 0.0];
    let mut a = sk.bind_pose_pose();
    let mut b = sk.bind_pose_pose();
    blend_physics_pose(&sk, &mut a, &blend);
    blend_physics_pose(&sk, &mut b, &blend);
    assert_eq!(a, b);
    // The opposite-hemisphere quaternion is the same rotation and must blend identically.
    blend.joints[0] = Some((Vec3::new(0.0, 0.2, 0.0), -lying));
    let mut c = sk.bind_pose_pose();
    blend_physics_pose(&sk, &mut c, &blend);
    assert!(a.rotations[0].angle_between(c.rotations[0]) < 1e-4);
}
