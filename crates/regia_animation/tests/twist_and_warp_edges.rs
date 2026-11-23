// SPDX-License-Identifier: MPL-2.0
//! Boundary cases for twist distribution, helper joints and scripted warps: authored numbers
//! that are NaN or infinite, shorter poses than skeletons, double-cover quaternions, huge steps
//! and the error codes. All inputs are fixed; nothing here reads a clock.

use regia_animation::montage_player::RigPlacement;
use regia_animation::scripted_warps::{ScriptedWarp, ScriptedWarps, WarpError, WarpEvent};
use regia_animation::skeleton::LocalBind;
use regia_animation::twist::{
    CompiledHelper, CompiledTwist, HelperJoint, HelperOffsetKey, TwistChain, TwistTarget, rotation_angle,
    swing_twist,
};
use regia_animation::warp::{WarpTarget, wrap_angle, yaw_of};
use regia_animation::{Pose, Skeleton};
use regia_core::math::{Quat, Vec3, det};

fn arm() -> Skeleton {
    let mk = |y: f32| LocalBind {
        translation: Vec3::new(0.0, y, 0.0),
        ..LocalBind::default()
    };
    Skeleton::from_local_bind(
        ["root", "upper", "fore", "twist1", "hand"]
            .map(String::from)
            .to_vec(),
        vec![None, Some(0), Some(1), Some(2), Some(2)],
        vec![mk(0.0), mk(0.3), mk(0.3), mk(0.1), mk(0.3)],
    )
    .expect("valid")
}

fn chain() -> TwistChain {
    TwistChain {
        name: "fore".into(),
        source: "hand".into(),
        axis: Vec3::Y,
        driven: vec![TwistTarget {
            joint: "twist1".into(),
            weight: 0.5,
            axis: None,
        }],
        source_keep: 1.0,
    }
}

/// A tiny deterministic generator so the property loops have fixed seeds.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
    fn quat(&mut self) -> Quat {
        loop {
            let q = Quat::from_xyzw(self.next(), self.next(), self.next(), self.next());
            if q.length() > 0.1 {
                return q.normalize();
            }
        }
    }
}

#[test]
fn swing_twist_reconstructs_the_rotation_for_fixed_seed_inputs() {
    let mut rng = Lcg(0x5eed);
    for _ in 0..200 {
        let q = rng.quat();
        let (swing, angle) = swing_twist(q, Vec3::Y);
        assert!(angle.abs() <= std::f32::consts::PI + 1e-5, "angle {angle}");
        let rebuilt = swing * Quat::from_axis_angle(Vec3::Y, angle);
        assert!(rebuilt.dot(q).abs() > 1.0 - 1e-4, "q {q:?} rebuilt {rebuilt:?}");
        assert!((swing.length() - 1.0).abs() < 1e-4);
        // The swing carries no roll about the axis.
        let (_, residual) = swing_twist(swing, Vec3::Y);
        assert!(residual.abs() < 2e-3, "residual twist {residual}");
    }
}

#[test]
fn swing_twist_double_cover_gives_the_same_angle() {
    let q = Quat::from_axis_angle(Vec3::Y, 1.0) * Quat::from_axis_angle(Vec3::X, 0.4);
    let (_, a) = swing_twist(q, Vec3::Y);
    let (_, b) = swing_twist(-q, Vec3::Y);
    assert!((a - b).abs() < 1e-5);
    // A pure half turn about the axis: the twist is ±π and the swing is identity.
    let (swing, angle) = swing_twist(Quat::from_axis_angle(Vec3::Y, std::f32::consts::PI), Vec3::Y);
    assert!((angle.abs() - std::f32::consts::PI).abs() < 1e-4);
    assert!(swing.angle_between(Quat::IDENTITY) < 1e-3);
}

#[test]
fn rotation_angle_covers_zero_to_pi() {
    assert_eq!(rotation_angle(Quat::IDENTITY), 0.0);
    assert_eq!(rotation_angle(-Quat::IDENTITY), 0.0, "-q is the same rotation");
    assert!(
        (rotation_angle(Quat::from_rotation_x(std::f32::consts::PI)) - std::f32::consts::PI).abs() < 1e-3
    );
    // A slightly non-unit quaternion must not make acos return NaN.
    let big = Quat::from_xyzw(0.0, 0.0, 0.0, 1.0001);
    assert_eq!(rotation_angle(big), 0.0);
    assert!(rotation_angle(Quat::from_xyzw(0.0, 0.0, 0.0, 1.0)).is_finite());
}

#[test]
fn compiling_a_twist_reports_every_problem_and_never_half_succeeds() {
    let sk = arm();
    let mut bad = chain();
    bad.source = "nope".into();
    bad.driven.push(TwistTarget {
        joint: "also_nope".into(),
        weight: 1.0,
        axis: None,
    });
    bad.driven.push(TwistTarget {
        joint: "twist1".into(),
        weight: 1.0,
        axis: Some(Vec3::ZERO),
    });
    let problems = CompiledTwist::compile(&bad, &sk).expect_err("three problems");
    assert_eq!(problems.len(), 3, "{problems:?}");
    // A zero (or NaN) chain axis is refused too.
    for axis in [
        Vec3::ZERO,
        Vec3::new(f32::NAN, 0.0, 0.0),
        Vec3::splat(f32::INFINITY),
    ] {
        let mut c = chain();
        c.axis = axis;
        assert!(CompiledTwist::compile(&c, &sk).is_err(), "axis {axis:?}");
    }
}

#[test]
fn a_non_finite_twist_number_is_refused_at_compile_time() {
    let sk = arm();
    let mut weight = chain();
    weight.driven[0].weight = f32::NAN;
    assert!(CompiledTwist::compile(&weight, &sk).is_err(), "NaN weight");
    let mut keep = chain();
    keep.source_keep = f32::INFINITY;
    assert!(
        CompiledTwist::compile(&keep, &sk).is_err(),
        "infinite source_keep"
    );
    let mut driven_axis = chain();
    driven_axis.driven[0].axis = Some(Vec3::new(0.0, f32::NAN, 0.0));
    assert!(
        CompiledTwist::compile(&driven_axis, &sk).is_err(),
        "NaN target axis"
    );
}

#[test]
fn applying_a_twist_to_a_short_pose_is_a_no_op_not_a_panic() {
    let sk = arm();
    let compiled = CompiledTwist::compile(&chain(), &sk).expect("compile");
    let mut pose = sk.bind_pose_pose();
    pose.translations.truncate(2);
    pose.rotations.truncate(2);
    pose.scales.truncate(2);
    let before = pose.clone();
    assert_eq!(compiled.apply(&mut pose), 0.0);
    assert_eq!(pose, before);
}

#[test]
fn twist_zero_weight_and_zero_keep_edges() {
    let sk = arm();
    let hand = sk.joint_index("hand").expect("hand") as usize;
    let twist1 = sk.joint_index("twist1").expect("twist1") as usize;
    let mut c = chain();
    c.driven[0].weight = 0.0;
    c.source_keep = 0.0;
    let compiled = CompiledTwist::compile(&c, &sk).expect("compile");
    let mut pose = sk.bind_pose_pose();
    pose.rotations[hand] = Quat::from_axis_angle(Vec3::Y, 1.0);
    let degrees = compiled.apply(&mut pose);
    assert!((degrees - 1.0f32.to_degrees()).abs() < 1e-3, "{degrees}");
    assert_eq!(pose.rotations[twist1], Quat::IDENTITY, "zero weight adds no roll");
    assert!(
        pose.rotations[hand].angle_between(Quat::IDENTITY) < 1e-4,
        "keep 0 removes the roll"
    );
    // Repeating the same input on a fresh pose gives the bit-identical result.
    let mut again = sk.bind_pose_pose();
    again.rotations[hand] = Quat::from_axis_angle(Vec3::Y, 1.0);
    compiled.apply(&mut again);
    assert_eq!(again, pose);
}

fn helper() -> HelperJoint {
    HelperJoint {
        name: "elbow".into(),
        joint: "twist1".into(),
        source: "hand".into(),
        follow: 0.5,
        offset: vec![],
    }
}

#[test]
fn twist_chain_driven_quaternion_is_pinned() {
    let sk = arm();
    let compiled = CompiledTwist::compile(&chain(), &sk).expect("compile");
    let hand = sk.joint_index("hand").expect("hand") as usize;
    let twist1 = sk.joint_index("twist1").expect("twist1") as usize;
    let mut pose = sk.bind_pose_pose();
    // Roll the source 40 deg about +Y. The input quaternion is built with the det kernel so
    // the test itself never calls the platform libm.
    pose.rotations[hand] = det::quat_from_axis_angle(Vec3::Y, 0.6981317);
    compiled.apply(&mut pose);
    // The driven joint takes weight 0.5 of the measured roll. The angles come from the series
    // kernels in `regia_core::math::det`, which are platform-independent; the last step is glam's
    // `Quat::normalize`, whose four-lane dot groups the lanes differently on SSE2 and NEON
    // (ADR-5176), so `w`'s last bit is one value per instruction set (ADR-5550). The check below
    // this one recomputes the same value on any target.
    let q = pose.rotations[twist1];
    let bits = [q.x.to_bits(), q.y.to_bits(), q.z.to_bits(), q.w.to_bits()];
    // glam scalar-math (`cross-platform-determinism`, ADR-5176) gives x86_64 the aarch64 bits
    // (measured on the build server, lane gate-lavapipe-coverage), so that build takes the aarch64 pin.
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "x86"),
        not(feature = "cross-platform-determinism")
    ))]
    assert_eq!(
        bits,
        [0x0000_0000, 0x3E31_D0D5, 0x0000_0000, 0x3F7C_1C5D],
        "driven twist quat {q:?}"
    );
    #[cfg(any(target_arch = "aarch64", feature = "cross-platform-determinism"))]
    assert_eq!(
        bits,
        [0x0000_0000, 0x3E31_D0D5, 0x0000_0000, 0x3F7C_1C5E],
        "driven twist quat {q:?}"
    );
    // The same value, computed with the det functions directly (no libm in the test).
    let roll = det::quat_from_axis_angle(Vec3::Y, 0.6981317);
    let (_, angle) = swing_twist(roll, Vec3::Y);
    let expected = (Quat::IDENTITY * det::quat_from_axis_angle(Vec3::Y, angle * 0.5)).normalize();
    assert_eq!(q, expected);
}

#[test]
fn helper_compile_errors_and_non_finite_numbers() {
    let sk = arm();
    let mut both = helper();
    both.joint = "x".into();
    both.source = "y".into();
    assert_eq!(CompiledHelper::compile(&both, &sk).expect_err("two").len(), 2);
    let mut me = helper();
    me.source = "twist1".into();
    assert!(CompiledHelper::compile(&me, &sk).is_err());
    let mut nan = helper();
    nan.follow = f32::NAN;
    assert!(CompiledHelper::compile(&nan, &sk).is_err(), "NaN follow");
    let mut key = helper();
    key.offset = vec![HelperOffsetKey {
        angle_deg: f32::NAN,
        offset: Vec3::X,
    }];
    assert!(CompiledHelper::compile(&key, &sk).is_err(), "NaN key angle");
    key.offset = vec![HelperOffsetKey {
        angle_deg: 10.0,
        offset: Vec3::new(0.0, f32::INFINITY, 0.0),
    }];
    assert!(CompiledHelper::compile(&key, &sk).is_err(), "infinite offset");
}

#[test]
fn helper_offsets_clamp_interpolate_and_sort() {
    let sk = arm();
    let hand = sk.joint_index("hand").expect("hand") as usize;
    let twist1 = sk.joint_index("twist1").expect("twist1") as usize;
    let mut h = helper();
    // Unsorted keys, with a duplicate angle (a step).
    h.offset = vec![
        HelperOffsetKey {
            angle_deg: 90.0,
            offset: Vec3::new(0.0, 0.0, 2.0),
        },
        HelperOffsetKey {
            angle_deg: 0.0,
            offset: Vec3::ZERO,
        },
        HelperOffsetKey {
            angle_deg: 90.0,
            offset: Vec3::new(0.0, 0.0, 4.0),
        },
    ];
    let compiled = CompiledHelper::compile(&h, &sk).expect("compile");
    let at = |deg: f32| {
        let mut pose = sk.bind_pose_pose();
        pose.rotations[hand] = Quat::from_rotation_x(deg.to_radians());
        let bind = pose.translations[twist1];
        let angle = compiled.apply(&mut pose);
        (pose.translations[twist1] - bind, angle)
    };
    let (zero, a0) = at(0.0);
    assert_eq!(zero, Vec3::ZERO);
    assert!(a0.abs() < 1e-3);
    let (half, a45) = at(45.0);
    assert!((a45 - 45.0).abs() < 1e-3);
    assert!(
        (half.z - 1.0).abs() < 1e-3,
        "midway between 0 and the first 90° key: {half}"
    );
    let (past, _) = at(170.0);
    assert!((past.z - 4.0).abs() < 1e-4, "clamped to the last key: {past}");
    // The negative rotation measures the same total angle (double cover).
    let (neg, aneg) = at(-45.0);
    assert!((aneg - 45.0).abs() < 1e-3);
    assert!((neg.z - 1.0).abs() < 1e-3);
}

#[test]
fn helper_apply_skips_joints_outside_a_short_pose() {
    let sk = arm();
    let compiled = CompiledHelper::compile(&helper(), &sk).expect("compile");
    let mut pose = Pose::from_identity(2);
    let before = pose.clone();
    assert_eq!(compiled.apply(&mut pose), 0.0);
    assert_eq!(pose, before);
}

const NO_BONE: &dyn Fn(&str) -> Option<Vec3> = &|_| None;

#[test]
fn scripted_warp_requests_are_validated_with_stable_codes() {
    let mut warps = ScriptedWarps::new();
    for duration in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(
            warps.add(ScriptedWarp::new("a", "t", duration)),
            Err(WarpError::BadDuration),
            "duration {duration}"
        );
    }
    assert_eq!(
        warps.add(ScriptedWarp::new("", "t", 1.0)),
        Err(WarpError::BadName)
    );
    assert_eq!(
        warps.add(ScriptedWarp::new("a", "", 1.0)),
        Err(WarpError::BadName)
    );
    for fraction in [0.0, -0.5, 1.5, f32::NAN, f32::INFINITY] {
        let mut warp = ScriptedWarp::new("a", "t", 1.0);
        warp.rotation_fraction = fraction;
        assert_eq!(
            warps.add(warp),
            Err(WarpError::BadFraction),
            "fraction {fraction}"
        );
    }
    let mut edge = ScriptedWarp::new("a", "t", 1.0);
    edge.rotation_fraction = 1.0;
    assert!(warps.add(edge).is_ok(), "1.0 is allowed");
    assert!(!warps.list().is_empty());
    // Refused requests changed nothing; the codes are the documented lowercase ones.
    assert_eq!(warps.list().len(), 1);
    for (error, code) in [
        (WarpError::BadDuration, "warp_bad_duration"),
        (WarpError::BadName, "warp_bad_name"),
        (WarpError::BadFraction, "warp_bad_fraction"),
    ] {
        let text = error.to_string();
        assert!(text.starts_with(code), "{text}");
        assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
    }
}

#[test]
fn scripted_warp_apply_survives_bad_steps_and_placements() {
    let mut warps = ScriptedWarps::new();
    warps.set_target("t", WarpTarget::at(Vec3::new(0.0, 0.0, -3.0)));
    warps.add(ScriptedWarp::new("w", "t", 1.0)).expect("add");
    let root = (Vec3::new(0.0, 0.0, -0.1), Quat::IDENTITY);
    for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 0.0] {
        let out = warps.apply(dt, RigPlacement::default(), root, NO_BONE);
        assert_eq!(out, root, "dt {dt} passes the motion through");
        assert_eq!(warps.list()[0].elapsed, 0.0, "and consumes no time");
    }
    // A zero-scale rig and a NaN-scale rig do not divide by zero.
    for scale in [Vec3::ZERO, Vec3::splat(f32::NAN)] {
        let rig = RigPlacement {
            scale,
            ..RigPlacement::default()
        };
        let (t, r) = warps.apply(0.1, rig, root, NO_BONE);
        assert!(t.is_finite() && r.is_finite(), "scale {scale:?}: {t}");
    }
    // A non-finite target leaves the motion alone but time still passes.
    warps.set_target("t", WarpTarget::at(Vec3::splat(f32::NAN)));
    let before = warps.list()[0].elapsed;
    let out = warps.apply(0.1, RigPlacement::default(), root, NO_BONE);
    assert_eq!(out, root);
    assert!(warps.list()[0].elapsed > before);
}

#[test]
fn a_step_longer_than_the_warp_finishes_it_in_one_frame() {
    let mut warps = ScriptedWarps::new();
    warps.set_target("t", WarpTarget::at(Vec3::new(0.0, 0.0, -2.0)));
    warps.add(ScriptedWarp::new("w", "t", 0.5)).expect("add");
    let (t, _) = warps.apply(
        10.0,
        RigPlacement::default(),
        (Vec3::new(0.0, 0.0, -0.4), Quat::IDENTITY),
        NO_BONE,
    );
    // ADR-2604 (spec 34 § 4a): the warp covers 0.5 s of the 10 s frame. That share
    // (0.02 m authored) lands on the target, and the remaining 9.5 s pass through as
    // authored (0.38 m). Before the split, the whole frame was mapped onto the target.
    assert!(
        (t.z + 2.38).abs() < 1e-4,
        "the warped share lands on the target, then the authored rest follows: {t}"
    );
    assert!(!warps.is_active(), "finished warps remove themselves");
    assert_eq!(warps.take_events(), vec![WarpEvent::Finished { id: "w".into() }]);
    assert!(warps.take_events().is_empty(), "events drain");
}

#[test]
fn two_warps_finish_in_the_order_they_were_added_and_pause_stops_time() {
    let mut warps = ScriptedWarps::new();
    warps.set_target("t", WarpTarget::at(Vec3::new(0.0, 0.0, -1.0)));
    warps.add(ScriptedWarp::new("first", "t", 0.2)).expect("add");
    warps.add(ScriptedWarp::new("second", "t", 0.2)).expect("add");
    warps.pause_all();
    let root = (Vec3::new(0.0, 0.0, -0.1), Quat::IDENTITY);
    assert_eq!(warps.apply(1.0, RigPlacement::default(), root, NO_BONE), root);
    assert_eq!(warps.list().iter().map(|s| s.elapsed).sum::<f32>(), 0.0);
    warps.resume_all();
    warps.apply(0.2, RigPlacement::default(), root, NO_BONE);
    let ids: Vec<_> = warps
        .take_events()
        .into_iter()
        .map(|WarpEvent::Finished { id }| id)
        .collect();
    assert_eq!(ids, ["first", "second"]);
}

#[test]
fn a_rotation_warp_with_a_partial_fraction_finishes_the_turn_early() {
    let mut warps = ScriptedWarps::new();
    let facing = Quat::from_rotation_y(1.0);
    warps.set_target(
        "t",
        WarpTarget {
            position: Vec3::ZERO,
            facing: Some(facing),
        },
    );
    let mut warp = ScriptedWarp::new("w", "t", 1.0);
    warp.rotation = true;
    warp.translation = false;
    warp.rotation_fraction = 0.5;
    warps.add(warp).expect("add");
    let mut rig = RigPlacement::default();
    let mut yaw = 0.0f32;
    let mut at_half = 0.0;
    for step in 0..10 {
        let (_, r) = warps.apply(0.1, rig, (Vec3::ZERO, Quat::IDENTITY), NO_BONE);
        yaw += yaw_of(r);
        rig.rotation = Quat::from_rotation_y(yaw);
        if step == 4 {
            at_half = yaw;
        }
    }
    assert!(
        (at_half - 1.0).abs() < 1e-3,
        "turn complete at half the duration: {at_half}"
    );
    assert!((yaw - 1.0).abs() < 1e-3, "and it stays there: {yaw}");
}

/// One scripted rotation warp, pinned bit for bit through the public `ScriptedWarps::apply`.
/// The expected rotation is rebuilt with `regia_core::math::det` only (never the platform
/// libm), so the pin holds on every target and catches a regression back to libm trig
/// (audit-v040 M3). The frame carries a rotation, so the axis-angle extrapolation of the
/// rest rotation is on the path.
#[test]
fn scripted_warp_yaw_uses_deterministic_math() {
    let facing = det::quat_from_rotation_y(1.0);
    let frame_rotation = det::quat_from_rotation_y(0.3);
    let mut warps = ScriptedWarps::new();
    warps.set_target(
        "t",
        WarpTarget {
            position: Vec3::ZERO,
            facing: Some(facing),
        },
    );
    let mut warp = ScriptedWarp::new("w", "t", 1.0);
    warp.rotation = true;
    warp.translation = false;
    warps.add(warp).expect("add");
    let (_, r) = warps.apply(
        0.1,
        RigPlacement::default(),
        (Vec3::ZERO, frame_rotation),
        NO_BONE,
    );
    // Rebuild `warp_one` + `warp_frame` on the same inputs (elapsed 0, frame_time 0.1,
    // rest_time 0.9, identity rig placement): the rest rotation is the frame rotation's
    // scaled axis multiplied by the extrapolation ratio, angle wrapped.
    let scaled = det::quat_to_scaled_axis(frame_rotation) * 9.0;
    let angle = scaled.length();
    let rest_rotation = det::quat_from_axis_angle(scaled / angle, wrap_angle(angle));
    let rig_yaw = det::quat_to_euler_yxz(Quat::IDENTITY).0;
    let needed = wrap_angle(det::quat_to_euler_yxz(facing).0 - rig_yaw);
    let authored = det::quat_to_euler_yxz(frame_rotation * rest_rotation).0;
    let rotation_share = (0.1f32 / (0.1 + 0.9)).clamp(0.0, 1.0);
    let yaw = det::quat_to_euler_yxz(frame_rotation).0 + wrap_angle(needed - authored) * rotation_share;
    let expected = det::quat_from_rotation_y(yaw);
    assert_eq!(
        r.to_array().map(f32::to_bits),
        expected.to_array().map(f32::to_bits),
        "the scripted warp's rotation is pinned: {r}"
    );
}
