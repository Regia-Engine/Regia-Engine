// SPDX-License-Identifier: MPL-2.0
//! IK solvers at their boundaries: unreachable and degenerate targets, non-finite settings,
//! bad chains, joint limits as projections, corrupt skeletons. Targets come from fixed lists
//! or a fixed-seed generator.

use regia_animation::chain_ik::{
    ChainError, ChainIkReport, ChainIkSettings, JointChain, JointLimit, solve_ccd, solve_fabrik, swing_twist,
};
use regia_animation::ik::{IkChain, LookAtSettings, joint_position, solve_look_at, solve_two_bone};
use regia_animation::pose::translated;
use regia_animation::{Pose, Skeleton};
use regia_core::math::{Quat, Vec3, det};

struct Lcg(u64);
impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
    fn vec(&mut self, scale: f32) -> Vec3 {
        Vec3::new(self.unit() - 0.5, self.unit() - 0.5, self.unit() - 0.5) * 2.0 * scale
    }
}

fn column(n: usize, segment: f32) -> Skeleton {
    let names = (0..n).map(|i| format!("j{i}")).collect();
    let parents = (0..n).map(|i| i.checked_sub(1).map(|p| p as u32)).collect();
    let binds = (0..n)
        .map(|i| {
            translated(if i == 0 {
                Vec3::ZERO
            } else {
                Vec3::new(0.0, segment, 0.0)
            })
        })
        .collect();
    Skeleton::from_local_bind(names, parents, binds).expect("valid skeleton")
}

fn tip(sk: &Skeleton, pose: &Pose) -> Vec3 {
    sk.globals(pose, None)[sk.len() - 1].w_axis.truncate()
}

/// The shared signature of the two chain solvers.
type ChainSolver = fn(&Skeleton, &mut Pose, &JointChain, Vec3, ChainIkSettings) -> ChainIkReport;

fn all_finite_unit(pose: &Pose) -> bool {
    pose.rotations
        .iter()
        .all(|q| q.is_finite() && (q.length() - 1.0).abs() < 1e-3)
        && pose.translations.iter().all(|t| t.is_finite())
}

#[test]
fn two_bone_reaches_fixed_seed_targets_and_keeps_bone_lengths() {
    let sk = column(3, 1.0);
    let chain = IkChain {
        root: 0,
        mid: 1,
        tip: 2,
    };
    let mut rng = Lcg(31337);
    for i in 0..200 {
        // Targets inside the annulus [0.05, 1.9] around the root.
        let dir = rng.vec(1.0).normalize_or(Vec3::X);
        let target = dir * (0.05 + rng.unit() * 1.85);
        let pole = rng.vec(2.0);
        let mut pose = sk.bind_pose_pose();
        solve_two_bone(&sk, &mut pose, &chain, target, Some(pole), 1.0);
        assert!(all_finite_unit(&pose), "#{i}");
        let reached = tip(&sk, &pose);
        assert!(
            (reached - target).length() < 2e-3,
            "#{i} target {target} reached {reached}"
        );
        let g = sk.globals(&pose, None);
        let upper = (g[1].w_axis - g[0].w_axis).truncate().length();
        let lower = (g[2].w_axis - g[1].w_axis).truncate().length();
        assert!(
            (upper - 1.0).abs() < 1e-4 && (lower - 1.0).abs() < 1e-4,
            "#{i} bones {upper} {lower}"
        );
    }
}

#[test]
fn two_bone_unreachable_and_degenerate_targets() {
    let sk = column(3, 1.0);
    let chain = IkChain {
        root: 0,
        mid: 1,
        tip: 2,
    };
    // Unreachable: stretched along the target direction, never past full extension.
    let mut pose = sk.bind_pose_pose();
    solve_two_bone(&sk, &mut pose, &chain, Vec3::new(10.0, 0.0, 0.0), None, 1.0);
    let reached = tip(&sk, &pose);
    assert!((reached.length() - 2.0).abs() < 1e-2, "{reached}");
    assert!(reached.normalize().dot(Vec3::X) > 0.999);
    // Too close (inside the folded limit) still yields a valid pose.
    let mut near = sk.bind_pose_pose();
    solve_two_bone(&sk, &mut near, &chain, Vec3::new(1e-3, 0.0, 0.0), None, 1.0);
    assert!(all_finite_unit(&near));
    // The root's own position, non-finite targets and poles, and bad weights.
    let bind = sk.bind_pose_pose();
    for target in [
        Vec3::ZERO,
        Vec3::splat(f32::NAN),
        Vec3::new(f32::INFINITY, 0.0, 0.0),
    ] {
        let mut p = bind.clone();
        solve_two_bone(&sk, &mut p, &chain, target, None, 1.0);
        assert_eq!(p, bind, "target {target:?} changes nothing");
    }
    let mut p = bind.clone();
    solve_two_bone(
        &sk,
        &mut p,
        &chain,
        Vec3::new(0.5, 0.5, 0.0),
        Some(Vec3::splat(f32::NAN)),
        1.0,
    );
    assert!(
        (tip(&sk, &p) - Vec3::new(0.5, 0.5, 0.0)).length() < 2e-3,
        "a NaN pole is ignored"
    );
    for weight in [0.0, -1.0, f32::NAN, f32::NEG_INFINITY] {
        let mut p = bind.clone();
        solve_two_bone(&sk, &mut p, &chain, Vec3::new(0.5, 0.5, 0.0), None, weight);
        assert_eq!(p, bind, "weight {weight} is no solve");
    }
    // A pole on the chain axis falls back to the current bend.
    let mut axis_pole = bind.clone();
    solve_two_bone(
        &sk,
        &mut axis_pole,
        &chain,
        Vec3::new(1.0, 1.0, 0.0),
        Some(Vec3::new(2.0, 2.0, 0.0)),
        1.0,
    );
    assert!(all_finite_unit(&axis_pole));
    // Half weight lands between the bind and the full solve.
    let mut half = bind.clone();
    let mut full = bind.clone();
    solve_two_bone(&sk, &mut half, &chain, Vec3::new(1.2, 0.6, 0.0), None, 0.5);
    solve_two_bone(&sk, &mut full, &chain, Vec3::new(1.2, 0.6, 0.0), None, 1.0);
    let d_half = (tip(&sk, &half) - tip(&sk, &bind)).length();
    let d_full = (tip(&sk, &full) - tip(&sk, &bind)).length();
    assert!(d_half > 0.0 && d_half < d_full);
}

#[test]
fn two_bone_refuses_broken_chains() {
    let sk = column(4, 1.0);
    let bind = sk.bind_pose_pose();
    let target = Vec3::new(1.0, 1.0, 0.0);
    for chain in [
        IkChain {
            root: 0,
            mid: 2,
            tip: 3,
        }, // not contiguous
        IkChain {
            root: 0,
            mid: 1,
            tip: 9,
        }, // tip outside the skeleton
        IkChain {
            root: 9,
            mid: 1,
            tip: 2,
        },
        IkChain {
            root: 2,
            mid: 1,
            tip: 0,
        }, // reversed
        IkChain {
            root: 1,
            mid: 1,
            tip: 1,
        },
    ] {
        let mut pose = bind.clone();
        solve_two_bone(&sk, &mut pose, &chain, target, None, 1.0);
        assert_eq!(pose, bind, "{chain:?}");
    }
    // A pose shorter than the skeleton counts the chain as outside.
    let mut short = Pose::from_identity(2);
    solve_two_bone(
        &sk,
        &mut short,
        &IkChain {
            root: 0,
            mid: 1,
            tip: 2,
        },
        target,
        None,
        1.0,
    );
    assert_eq!(short, Pose::from_identity(2));
    // Zero-length bones are skipped.
    let flat = Skeleton::from_local_bind(
        vec!["a".into(), "b".into(), "c".into()],
        vec![None, Some(0), Some(1)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::ZERO),
            translated(Vec3::Y),
        ],
    )
    .expect("skeleton");
    let mut pose = flat.bind_pose_pose();
    let before = pose.clone();
    solve_two_bone(
        &flat,
        &mut pose,
        &IkChain {
            root: 0,
            mid: 1,
            tip: 2,
        },
        target,
        None,
        1.0,
    );
    assert_eq!(pose, before);
}

#[test]
fn look_at_limits_weights_and_degenerate_targets() {
    let sk = column(2, 1.0);
    let bind = sk.bind_pose_pose();
    let settings = |max_yaw: f32, max_pitch: f32, weight: f32| LookAtSettings {
        max_yaw,
        max_pitch,
        weight,
    };
    // Straight ahead (−Z) is already aimed: nothing changes.
    let mut ahead = bind.clone();
    solve_look_at(
        &sk,
        &mut ahead,
        1,
        Quat::IDENTITY,
        Vec3::new(0.0, 1.0, -5.0),
        settings(1.0, 1.0, 1.0),
    );
    assert!(ahead.rotations[1].angle_between(Quat::IDENTITY) < 1e-4);
    // Behind with generous limits: finite, normalized.
    let mut behind = bind.clone();
    solve_look_at(
        &sk,
        &mut behind,
        1,
        Quat::IDENTITY,
        Vec3::new(0.0, 1.0, 5.0),
        settings(4.0, 4.0, 1.0),
    );
    assert!(all_finite_unit(&behind));
    // Yaw is limited, and a negative limit means the same magnitude.
    for limit in [0.3, -0.3] {
        let mut p = bind.clone();
        solve_look_at(
            &sk,
            &mut p,
            1,
            Quat::IDENTITY,
            Vec3::new(5.0, 1.0, 0.0),
            settings(limit, 1.5, 1.0),
        );
        let (yaw, _, _) = p.rotations[1].to_euler(regia_core::glam::EulerRot::YXZ);
        assert!((yaw.abs() - 0.3).abs() < 1e-3, "limit {limit}: yaw {yaw}");
    }
    // NaN limits mean no limit: the joint turns fully toward +X.
    let mut free = bind.clone();
    solve_look_at(
        &sk,
        &mut free,
        1,
        Quat::IDENTITY,
        Vec3::new(5.0, 1.0, 0.0),
        settings(f32::NAN, f32::NAN, 1.0),
    );
    let aim = free.rotations[1] * Vec3::NEG_Z;
    assert!(aim.dot(Vec3::X) > 0.999, "{aim}");
    // No-ops: bad weight, target at the joint, non-finite target, joint outside.
    for (joint, target, weight) in [
        (1, Vec3::new(5.0, 1.0, 0.0), 0.0),
        (1, Vec3::new(5.0, 1.0, 0.0), f32::NAN),
        (1, Vec3::new(0.0, 1.0, 0.0), 1.0),
        (1, Vec3::splat(f32::NAN), 1.0),
        (7, Vec3::X, 1.0),
    ] {
        let mut p = bind.clone();
        solve_look_at(
            &sk,
            &mut p,
            joint,
            Quat::IDENTITY,
            target,
            settings(1.0, 1.0, weight),
        );
        assert_eq!(p, bind, "joint {joint} target {target:?} weight {weight}");
    }
}

#[test]
fn joint_position_outside_the_skeleton_is_the_origin() {
    let sk = column(3, 0.5);
    let pose = sk.bind_pose_pose();
    assert_eq!(joint_position(&sk, &pose, 2), Vec3::new(0.0, 1.0, 0.0));
    assert_eq!(joint_position(&sk, &pose, 3), Vec3::ZERO);
    assert_eq!(joint_position(&sk, &pose, u32::MAX), Vec3::ZERO);
}

#[test]
fn joint_chain_between_reports_each_failure() {
    let sk = column(4, 1.0);
    assert!(
        matches!(JointChain::between(&sk, "j0", "nope"), Err(ChainError::MissingJoint(n)) if n == "nope")
    );
    assert!(
        matches!(JointChain::between(&sk, "nope", "j2"), Err(ChainError::MissingJoint(n)) if n == "nope")
    );
    assert_eq!(JointChain::between(&sk, "j2", "j2"), Err(ChainError::TooShort));
    assert!(matches!(
        JointChain::between(&sk, "j3", "j1"),
        Err(ChainError::NotDescendant { .. })
    ));
    let chain = JointChain::between(&sk, "j1", "j3").expect("chain");
    assert_eq!(chain.joints, [1, 2, 3]);
    assert!(chain.limits.is_empty());
    let limited = chain.with_uniform_limit(JointLimit::Free);
    assert_eq!(limited.limits.len(), 3);
    // A forest: tip under a different root.
    let forest = Skeleton::from_local_bind(
        vec!["r1".into(), "r2".into(), "c".into()],
        vec![None, None, Some(1)],
        vec![translated(Vec3::ZERO); 3],
    )
    .expect("forest");
    assert!(matches!(
        JointChain::between(&forest, "r1", "c"),
        Err(ChainError::NotDescendant { .. })
    ));
    // A corrupt skeleton with a parent cycle terminates instead of looping.
    let mut cyclic = column(3, 1.0);
    cyclic.parents[1] = Some(2);
    cyclic.parents[2] = Some(1);
    assert!(matches!(
        JointChain::between(&cyclic, "j0", "j2"),
        Err(ChainError::NotDescendant { .. })
    ));
}

#[test]
fn chain_solvers_agree_on_error_reporting_and_refuse_bad_input() {
    let sk = column(4, 0.5);
    let chain = JointChain::between(&sk, "j0", "j3").expect("chain");
    let bind = sk.bind_pose_pose();
    let solvers: [(&str, ChainSolver); 2] = [("fabrik", solve_fabrik), ("ccd", solve_ccd)];
    for (name, solve) in solvers {
        for target in [Vec3::splat(f32::NAN), Vec3::new(0.0, f32::INFINITY, 0.0)] {
            let mut p = bind.clone();
            let report = solve(&sk, &mut p, &chain, target, ChainIkSettings::default());
            assert_eq!(p, bind, "{name}");
            assert_eq!(report.error, f32::INFINITY);
            assert_eq!(report.iterations, 0);
        }
        let broken = JointChain {
            joints: vec![0, 2, 3],
            limits: vec![],
        };
        let mut p = bind.clone();
        assert_eq!(
            solve(&sk, &mut p, &broken, Vec3::X, ChainIkSettings::default()).error,
            f32::INFINITY
        );
        let outside = JointChain {
            joints: vec![2, 3, 4],
            limits: vec![],
        };
        assert_eq!(
            solve(&sk, &mut p, &outside, Vec3::X, ChainIkSettings::default()).error,
            f32::INFINITY
        );
        let single = JointChain {
            joints: vec![1],
            limits: vec![],
        };
        assert_eq!(
            solve(&sk, &mut p, &single, Vec3::X, ChainIkSettings::default()).error,
            f32::INFINITY
        );
        assert_eq!(p, bind, "{name}: refused chains leave the pose alone");
        // Zero iterations: the report is the starting error.
        let report = solve(
            &sk,
            &mut p,
            &chain,
            Vec3::new(0.5, 0.5, 0.0),
            ChainIkSettings {
                iterations: 0,
                ..ChainIkSettings::default()
            },
        );
        assert_eq!(report.iterations, 0, "{name}");
        assert!(
            (report.error - (Vec3::new(0.0, 1.5, 0.0) - Vec3::new(0.5, 0.5, 0.0)).length()).abs() < 1e-4,
            "{name}"
        );
        // Already at the target: converged, pose unchanged (FABRIK counts a target at full reach
        // as the straightening case and spends one iteration on it).
        let report = solve(
            &sk,
            &mut p,
            &chain,
            Vec3::new(0.0, 1.5, 0.0),
            ChainIkSettings::default(),
        );
        assert!(
            report.iterations <= 1 && report.error < 1e-3,
            "{name}: {report:?}"
        );
        assert_eq!(p, bind, "{name}");
    }
}

#[test]
fn chain_solver_weight_blends_and_survives_nan() {
    let sk = column(4, 0.5);
    let chain = JointChain::between(&sk, "j0", "j3").expect("chain");
    let bind = sk.bind_pose_pose();
    let target = Vec3::new(0.8, 0.6, 0.2);
    for solve in [solve_fabrik, solve_ccd] {
        let run = |weight: f32| {
            let mut p = bind.clone();
            solve(
                &sk,
                &mut p,
                &chain,
                target,
                ChainIkSettings {
                    weight,
                    ..ChainIkSettings::default()
                },
            );
            p
        };
        let full = run(1.0);
        let none = run(0.0);
        assert_eq!(none, bind, "weight 0 restores the input rotations");
        let half = run(0.5);
        let d_half = (tip(&sk, &half) - tip(&sk, &bind)).length();
        let d_full = (tip(&sk, &full) - tip(&sk, &bind)).length();
        assert!(d_half > 0.0 && d_half < d_full, "{d_half} vs {d_full}");
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 9.0, -9.0] {
            let p = run(bad);
            assert!(all_finite_unit(&p), "weight {bad}");
        }
        assert_eq!(run(f32::NAN), bind, "a NaN weight is no solve");
        assert_eq!(run(1.0), full, "deterministic");
    }
}

#[test]
fn chain_solvers_converge_on_fixed_seed_targets_with_limits() {
    let sk = column(5, 0.5);
    let base = JointChain::between(&sk, "j0", "j4").expect("chain");
    let limited = base.clone().with_uniform_limit(JointLimit::Cone {
        max_swing: 1.0,
        min_twist: -0.5,
        max_twist: 0.5,
    });
    let mut rng = Lcg(99);
    for i in 0..40 {
        let target = rng.vec(1.2);
        for chain in [&base, &limited] {
            let mut pose = sk.bind_pose_pose();
            let report = solve_fabrik(&sk, &mut pose, chain, target, ChainIkSettings::default());
            assert!(all_finite_unit(&pose), "#{i}");
            assert!(report.error.is_finite());
            if chain.limits.is_empty() && target.length() < 1.9 {
                assert!(report.error < 2e-3, "#{i} {target}: {}", report.error);
            }
        }
    }
}

#[test]
fn joint_limits_are_projections_inside_their_range() {
    let bind = Quat::from_rotation_z(0.2);
    let mut rng = Lcg(5);
    let hinge = JointLimit::Hinge {
        axis: Vec3::X,
        min: -0.5,
        max: 1.0,
    };
    let cone = JointLimit::Cone {
        max_swing: 0.7,
        min_twist: -0.3,
        max_twist: 0.4,
    };
    for _ in 0..200 {
        let local = (Quat::from_xyzw(
            rng.unit() - 0.5,
            rng.unit() - 0.5,
            rng.unit() - 0.5,
            rng.unit() - 0.5,
        ))
        .normalize();
        // Projections are idempotent: constraining twice equals constraining once.
        for limit in [hinge, cone] {
            let once = limit.constrain(local, bind, Vec3::Y);
            let twice = limit.constrain(once, bind, Vec3::Y);
            assert!(once.is_finite() && (once.length() - 1.0).abs() < 1e-4);
            assert!(once.angle_between(twice) < 2e-3, "{limit:?}");
        }
        // The hinge keeps only rotation about its axis within [min, max].
        let h = hinge.constrain(local, bind, Vec3::Y);
        let offset = (bind.inverse() * h).normalize();
        let (axis, angle) = offset.to_axis_angle();
        assert!(
            angle < 1e-3 || axis.dot(Vec3::X).abs() > 0.999,
            "off-axis hinge: {axis} {angle}"
        );
        let signed = if axis.x < 0.0 { -angle } else { angle };
        assert!(signed > -0.5 - 1e-3 && signed < 1.0 + 1e-3, "{signed}");
    }
    // Free is the identity; reversed and NaN authored limits do not panic.
    assert_eq!(
        JointLimit::Free.constrain(Quat::from_rotation_y(2.0), bind, Vec3::Y),
        Quat::from_rotation_y(2.0)
    );
    let odd = [
        JointLimit::Cone {
            max_swing: f32::NAN,
            min_twist: 1.0,
            max_twist: -1.0,
        },
        JointLimit::Cone {
            max_swing: -1.0,
            min_twist: f32::NAN,
            max_twist: f32::NAN,
        },
        JointLimit::Hinge {
            axis: Vec3::ZERO,
            min: 2.0,
            max: -2.0,
        },
        JointLimit::Hinge {
            axis: Vec3::splat(f32::NAN),
            min: f32::NAN,
            max: 1.0,
        },
    ];
    for limit in odd {
        let q = limit.constrain(Quat::from_rotation_x(1.0), bind, Vec3::splat(f32::NAN));
        assert!(q.is_finite(), "{limit:?}");
    }
}

#[test]
fn hinge_limit_clamp_is_pinned() {
    // A hinge about +X limited to [-0.5, 0.5] rad, driven 1 rad over the limit. The input
    // quaternion is built with the det kernel so the test itself never calls the platform libm.
    let limit = JointLimit::Hinge {
        axis: Vec3::X,
        min: -0.5,
        max: 0.5,
    };
    let local = det::quat_from_axis_angle(Vec3::X, 1.0);
    let q = limit.constrain(local, Quat::IDENTITY, Vec3::Y);
    // The bits below come from the series kernels in `regia_core::math::det` alone, so they
    // hold on every platform.
    let bits = [q.x.to_bits(), q.y.to_bits(), q.z.to_bits(), q.w.to_bits()];
    assert_eq!(
        bits,
        [0x3E7D_5777, 0x0000_0000, 0x0000_0000, 0x3F78_0AA5],
        "clamped hinge quat {q:?}"
    );
    // The same value, computed with the det functions directly (no libm in the test).
    let expected = det::quat_from_axis_angle(Vec3::X, 0.5).normalize();
    assert_eq!(q, expected);
}

#[test]
fn swing_twist_splits_a_rotation_exactly() {
    let mut rng = Lcg(8);
    for _ in 0..100 {
        let q = Quat::from_xyzw(
            rng.unit() - 0.5,
            rng.unit() - 0.5,
            rng.unit() - 0.5,
            rng.unit() - 0.5,
        )
        .normalize();
        let (swing, twist) = swing_twist(q, Vec3::Y);
        assert!((swing * twist).angle_between(q) < 1e-3);
        // The twist is a rotation about the axis only.
        assert!(twist.x.abs() < 1e-4 && twist.z.abs() < 1e-4);
    }
    // A 180° swing about a perpendicular axis has no defined twist: identity, no NaN.
    let (swing, twist) = swing_twist(Quat::from_rotation_x(std::f32::consts::PI), Vec3::Y);
    assert!(swing.is_finite() && twist.is_finite());
}

mod foot {
    use super::*;
    use regia_animation::foot_ik::{
        FootGround, FootIkInput, FootIkLeg, FootIkPlan, FootIkSettings, plan_foot_ik, solve_foot_ik,
    };

    fn biped() -> Skeleton {
        let names: Vec<String> = ["Pelvis", "LHip", "LKnee", "LFoot", "RHip", "RKnee", "RFoot"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parents = vec![None, Some(0), Some(1), Some(2), Some(0), Some(4), Some(5)];
        let bind = vec![
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(-0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
        ];
        Skeleton::from_local_bind(names, parents, bind).expect("biped")
    }

    fn legs() -> Vec<FootIkLeg> {
        vec![
            FootIkLeg::new(IkChain {
                root: 1,
                mid: 2,
                tip: 3,
            }),
            FootIkLeg::new(IkChain {
                root: 4,
                mid: 5,
                tip: 6,
            }),
        ]
    }

    fn plan(
        feet: &[Vec3],
        ground: &[FootGround],
        legs: &[FootIkLeg],
        settings: &FootIkSettings,
    ) -> FootIkPlan {
        plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: feet,
                ground,
            },
            legs,
            settings,
        )
    }

    #[test]
    fn non_finite_probes_plan_no_adjustment() {
        let feet = [Vec3::new(-0.15, 0.1, 0.0), Vec3::new(0.15, 0.1, 0.0)];
        let settings = FootIkSettings::default();
        let nan_ground = [
            FootGround::Contact(Vec3::new(f32::NAN, f32::NAN, 0.0)),
            FootGround::Contact(Vec3::new(0.15, 0.05, 0.0)),
        ];
        let p = plan(&feet, &nan_ground, &legs(), &settings);
        assert_eq!(p.legs[0].weight, 0.0, "a NaN probe leaves the leg animated");
        assert!(p.pelvis_offset.is_finite());
        assert!(p.legs[1].weight > 0.0 && p.legs[1].target.is_finite());
        let nan_feet = [Vec3::new(f32::NAN, f32::NAN, f32::NAN), feet[1]];
        let ground = [
            FootGround::Contact(Vec3::ZERO),
            FootGround::Contact(Vec3::new(0.15, 0.05, 0.0)),
        ];
        let p = plan(&nan_feet, &ground, &legs(), &settings);
        assert_eq!(p.legs[0].weight, 0.0);
        assert!(p.pelvis_offset.is_finite());
        let inf_ground = [
            FootGround::Contact(Vec3::new(0.0, f32::INFINITY, 0.0)),
            FootGround::None,
        ];
        let p = plan(&feet, &inf_ground, &legs(), &settings);
        assert_eq!(p.legs[0].weight, 0.0);
        assert!(p.pelvis_offset.is_finite());
    }

    #[test]
    fn a_nan_reach_or_fade_never_makes_the_weight_one_everywhere() {
        let feet = [Vec3::new(-0.15, 0.1, 0.0)];
        // The ground is a full metre below the animated foot: far outside any sane reach.
        let ground = [FootGround::Contact(Vec3::new(-0.15, -0.9, 0.0))];
        let mut leg = FootIkLeg::new(IkChain {
            root: 1,
            mid: 2,
            tip: 3,
        });
        leg.max_reach_offset = f32::NAN;
        assert_eq!(
            plan(&feet, &ground, &[leg], &FootIkSettings::default()).legs[0].weight,
            0.0
        );
        let nan_fade = FootIkSettings {
            fade_distance: f32::NAN,
            ..FootIkSettings::default()
        };
        let w = plan(&feet, &ground, &[FootIkLeg::new(leg.chain)], &nan_fade).legs[0].weight;
        assert!((0.0..=1.0).contains(&w) && w == 0.0, "{w}");
    }

    #[test]
    fn plans_follow_the_shortest_input_and_clamp_the_pelvis() {
        let settings = FootIkSettings::default();
        let feet = [Vec3::new(-0.15, 0.1, 0.0), Vec3::new(0.15, 0.1, 0.0)];
        let ground = [FootGround::Contact(Vec3::new(-0.15, 0.1, 0.0))];
        assert_eq!(
            plan(&feet, &ground, &legs(), &settings).legs.len(),
            1,
            "legs, feet and ground are zipped"
        );
        assert!(plan(&feet, &ground, &[], &settings).legs.is_empty());
        assert_eq!(plan(&[], &[], &legs(), &settings), FootIkPlan::default());
        // The lowest ground drives the pelvis, clamped to the drop/rise magnitudes.
        let low = [
            FootGround::Contact(Vec3::new(-0.15, -3.0, 0.0)),
            FootGround::Contact(Vec3::new(0.15, 0.1, 0.0)),
        ];
        assert_eq!(plan(&feet, &low, &legs(), &settings).pelvis_offset, -0.25);
        let high = [
            FootGround::Contact(Vec3::new(-0.15, 3.0, 0.0)),
            FootGround::Contact(Vec3::new(0.15, 3.0, 0.0)),
        ];
        assert_eq!(plan(&feet, &high, &legs(), &settings).pelvis_offset, 0.2);
        // A negative or NaN range reads as a magnitude / zero and never panics.
        let odd = FootIkSettings {
            max_pelvis_drop: -0.1,
            max_pelvis_rise: f32::NAN,
            fade_distance: 0.5,
        };
        assert_eq!(plan(&feet, &low, &legs(), &odd).pelvis_offset, -0.1);
        assert_eq!(plan(&feet, &high, &legs(), &odd).pelvis_offset, 0.0);
        // Repeated planning is identical.
        assert_eq!(
            plan(&feet, &low, &legs(), &settings),
            plan(&feet, &low, &legs(), &settings)
        );
    }

    #[test]
    fn the_weight_fades_across_the_reach_band() {
        let settings = FootIkSettings::default(); // reach 0.35, fade 0.5
        let feet = [Vec3::new(-0.15, 0.1, 0.0)];
        // The pelvis absorbs part of the error, so probe the band with a pelvis that cannot move.
        let fixed = FootIkSettings {
            max_pelvis_drop: 0.0,
            max_pelvis_rise: 0.0,
            ..settings
        };
        let w = |error: f32| {
            plan(
                &feet,
                &[FootGround::Contact(Vec3::new(-0.15, 0.1 + error, 0.0))],
                &legs()[..1],
                &fixed,
            )
            .legs[0]
                .weight
        };
        assert_eq!(w(0.0), 1.0);
        assert_eq!(w(0.35), 1.0);
        assert!((w(0.6) - 0.5).abs() < 1e-5, "{}", w(0.6));
        assert_eq!(w(0.85), 0.0);
        assert_eq!(w(-0.6), w(0.6), "symmetric in the error sign");
        assert_eq!(w(5.0), 0.0);
    }

    #[test]
    fn solve_foot_ik_applies_the_pelvis_and_skips_zero_weight_legs() {
        let sk = biped();
        let bind = sk.bind_pose_pose();
        // No plan, no change.
        let mut pose = bind.clone();
        solve_foot_ik(&sk, &mut pose, &FootIkPlan::default(), &legs(), None);
        assert_eq!(pose, bind);
        // Only the pelvis offset.
        let mut pose = bind.clone();
        let plan = FootIkPlan {
            pelvis_offset: -0.1,
            legs: vec![],
        };
        solve_foot_ik(&sk, &mut pose, &plan, &legs(), None);
        assert!((pose.translations[0].y - 0.9).abs() < 1e-6);
        // A leg plan with zero weight does nothing, even with a wild target.
        let mut pose = bind.clone();
        let plan = FootIkPlan {
            pelvis_offset: 0.0,
            legs: vec![
                regia_animation::foot_ik::FootIkLegPlan {
                    target: Vec3::splat(9.0),
                    weight: 0.0
                };
                2
            ],
        };
        solve_foot_ik(&sk, &mut pose, &plan, &legs(), Some(Vec3::Z));
        assert_eq!(pose, bind);
        // A NaN target is refused by the solver, leaving rotations finite.
        let nan = FootIkPlan {
            pelvis_offset: 0.0,
            legs: vec![
                regia_animation::foot_ik::FootIkLegPlan {
                    target: Vec3::splat(f32::NAN),
                    weight: 1.0
                };
                2
            ],
        };
        let mut pose = bind.clone();
        solve_foot_ik(&sk, &mut pose, &nan, &legs(), None);
        assert_eq!(pose, bind);
        // A pose shorter than the root: no panic.
        let mut tiny = Pose::from_identity(0);
        solve_foot_ik(
            &sk,
            &mut tiny,
            &FootIkPlan {
                pelvis_offset: 1.0,
                legs: vec![],
            },
            &legs(),
            None,
        );
        assert!(tiny.is_empty());
    }
}

mod aim {
    use super::*;
    use regia_animation::aim_ik::{AimChain, GripFrame, HandGrip, place_hand, solve_body_aim};
    use regia_core::math::Mat4;

    fn chain(shares: Vec<f32>) -> AimChain {
        AimChain {
            joints: vec![1, 2, 3],
            shares,
            limits: vec![],
            aim_joint: 4,
            aim_axis: Vec3::Y,
        }
    }

    #[test]
    fn a_free_chain_aims_at_fixed_seed_directions() {
        let sk = column(5, 0.3);
        let mut rng = Lcg(808);
        for i in 0..60 {
            let target = rng.vec(3.0) + Vec3::new(0.0, 0.8, 0.0);
            if (target - Vec3::new(0.0, 1.2, 0.0)).length() < 0.5 {
                continue;
            }
            let mut pose = sk.bind_pose_pose();
            let report = solve_body_aim(&sk, &mut pose, &chain(vec![]), target, &[], 1.0);
            assert!(all_finite_unit(&pose), "#{i}");
            assert!(
                report.angle_error.is_finite() && report.angle_error < 0.35,
                "#{i} {target}: {report:?}"
            );
            let again = {
                let mut p = sk.bind_pose_pose();
                solve_body_aim(&sk, &mut p, &chain(vec![]), target, &[], 1.0);
                p
            };
            assert_eq!(again, pose, "#{i} deterministic");
        }
    }

    #[test]
    fn bad_input_reports_the_worst_error_and_leaves_the_pose_alone() {
        let sk = column(5, 0.3);
        let bind = sk.bind_pose_pose();
        let cases: Vec<(&str, AimChain, Vec3, f32)> = vec![
            ("nan target", chain(vec![]), Vec3::splat(f32::NAN), 1.0),
            (
                "inf target",
                chain(vec![]),
                Vec3::new(f32::INFINITY, 0.0, 0.0),
                1.0,
            ),
            (
                "zero axis",
                AimChain {
                    aim_axis: Vec3::ZERO,
                    ..chain(vec![])
                },
                Vec3::X,
                1.0,
            ),
            (
                "no joints",
                AimChain {
                    joints: vec![],
                    ..chain(vec![])
                },
                Vec3::X,
                1.0,
            ),
            (
                "joint outside",
                AimChain {
                    joints: vec![1, 99],
                    ..chain(vec![])
                },
                Vec3::X,
                1.0,
            ),
            (
                "aim joint outside",
                AimChain {
                    aim_joint: 99,
                    ..chain(vec![])
                },
                Vec3::X,
                1.0,
            ),
            ("zero weight", chain(vec![]), Vec3::X, 0.0),
            ("nan weight", chain(vec![]), Vec3::X, f32::NAN),
            ("all-zero shares", chain(vec![0.0, 0.0, 0.0]), Vec3::X, 1.0),
        ];
        for (name, aim, target, weight) in cases {
            let mut pose = bind.clone();
            let report = solve_body_aim(&sk, &mut pose, &aim, target, &[], weight);
            assert_eq!(pose, bind, "{name}");
            assert!(report.angle_error.is_finite(), "{name}: {report:?}");
        }
        let mut pose = bind.clone();
        let report = solve_body_aim(
            &sk,
            &mut pose,
            &AimChain {
                aim_joint: 99,
                ..chain(vec![])
            },
            Vec3::X,
            &[],
            1.0,
        );
        assert_eq!(report.angle_error, std::f32::consts::PI);
    }

    #[test]
    fn shares_hostile_values_do_not_poison_the_pose() {
        let sk = column(5, 0.3);
        for shares in [
            vec![f32::NAN, 1.0, 1.0],
            vec![-1.0, 1.0, 1.0],
            vec![1.0, f32::INFINITY, 1.0],
            vec![1.0, 1.0], // wrong length: equal shares
        ] {
            let mut pose = sk.bind_pose_pose();
            let report = solve_body_aim(
                &sk,
                &mut pose,
                &chain(shares.clone()),
                Vec3::new(1.5, 1.5, 0.5),
                &[],
                1.0,
            );
            assert!(all_finite_unit(&pose), "{shares:?}");
            assert!(report.angle_error.is_finite(), "{shares:?}: {report:?}");
        }
    }

    #[test]
    fn weight_blends_the_pose_while_the_report_describes_the_full_solve() {
        let sk = column(5, 0.3);
        let bind = sk.bind_pose_pose();
        let target = Vec3::new(2.0, 1.2, 0.0);
        let run = |w: f32| {
            let mut p = bind.clone();
            let report = solve_body_aim(&sk, &mut p, &chain(vec![]), target, &[], w);
            (p, report)
        };
        let (full_pose, full) = run(1.0);
        let (half_pose, half) = run(0.5);
        let (none, zero) = run(0.0);
        assert_eq!(none, bind);
        assert_eq!(
            half.angle_error, full.angle_error,
            "the report is of the unblended solve"
        );
        assert_eq!(zero.angle_error, full.angle_error);
        let turned = |p: &Pose| p.rotations[1].angle_between(Quat::IDENTITY);
        assert!(turned(&half_pose) > 0.0 && turned(&half_pose) < turned(&full_pose));
    }

    #[test]
    fn place_hand_refuses_what_it_cannot_do() {
        let sk = column(4, 0.5);
        let bind = sk.bind_pose_pose();
        let chain = IkChain {
            root: 0,
            mid: 1,
            tip: 2,
        };
        for (name, c, frame) in [
            ("tip outside", IkChain { tip: 9, ..chain }, Mat4::IDENTITY),
            ("root outside", IkChain { root: 9, ..chain }, Mat4::IDENTITY),
            ("nan frame", chain, Mat4::from_translation(Vec3::splat(f32::NAN))),
            (
                "inf frame",
                chain,
                Mat4::from_translation(Vec3::new(f32::INFINITY, 0.0, 0.0)),
            ),
        ] {
            let mut pose = bind.clone();
            assert_eq!(
                place_hand(&sk, &mut pose, &c, frame, None, 1.0),
                f32::INFINITY,
                "{name}"
            );
            assert_eq!(pose, bind, "{name}");
        }
        // A reachable frame is met; weight 0 keeps the input rotations.
        let frame = Mat4::from_rotation_translation(Quat::from_rotation_z(0.5), Vec3::new(0.5, 0.5, 0.0));
        let mut pose = bind.clone();
        let error = place_hand(&sk, &mut pose, &chain, frame, None, 1.0);
        assert!(error < 2e-3, "{error}");
        let mut kept = bind.clone();
        place_hand(&sk, &mut kept, &chain, frame, None, 0.0);
        assert_eq!(kept, bind);
        let mut nan_weight = bind.clone();
        place_hand(&sk, &mut nan_weight, &chain, frame, None, f32::NAN);
        assert_eq!(nan_weight, bind);
    }

    #[test]
    fn grips_follow_the_aim_and_a_bad_grip_chain_reports_infinite_error() {
        let sk = column(5, 0.3);
        let aim = chain(vec![]);
        let grip = HandGrip {
            chain: IkChain {
                root: 2,
                mid: 3,
                tip: 4,
            },
            grip: Some(GripFrame {
                translation: Vec3::new(0.0, 0.0, 0.0),
                rotation: Quat::IDENTITY,
            }),
            pole: None,
        };
        let mut pose = sk.bind_pose_pose();
        let report = solve_body_aim(&sk, &mut pose, &aim, Vec3::new(1.0, 1.5, 0.0), &[grip], 1.0);
        assert!(
            report.grip_error.is_finite() && all_finite_unit(&pose),
            "{report:?}"
        );
        let bad = HandGrip {
            chain: IkChain {
                root: 2,
                mid: 3,
                tip: 77,
            },
            ..grip
        };
        let mut pose = sk.bind_pose_pose();
        let report = solve_body_aim(&sk, &mut pose, &aim, Vec3::new(1.0, 1.5, 0.0), &[bad], 1.0);
        assert_eq!(report.grip_error, f32::INFINITY);
        // A captured grip (None) in an unchanged aim keeps the hand where it is.
        let captured = HandGrip { grip: None, ..grip };
        let mut pose = sk.bind_pose_pose();
        let report = solve_body_aim(&sk, &mut pose, &aim, Vec3::new(0.0, 5.0, 0.0), &[captured], 1.0);
        assert!(report.grip_error < 1e-2, "{report:?}");
    }
}

/// fixrev-06-k H2: a weighted look-at blends the clamped rotation into the current pose with
/// the deterministic slerp, not glam's libm: the blended joint's bits are pinned literally,
/// and the det and libm blends are asserted to disagree on this host so the pin cannot pass
/// on the libm path.
#[test]
fn weighted_look_at_blend_is_pinned_and_platform_stable() {
    // Anti-vacuousness guard: the kernels diverge on this blend (same contract as
    // `predicted_yaw_matches_the_deterministic_kernel`).
    let a = Quat::from_xyzw(0.1, 0.2, 0.3, 0.9).normalize();
    let b = Quat::from_xyzw(0.3, -0.1, 0.2, 0.8).normalize();
    assert_ne!(
        det::quat_slerp(a, b, 0.5).to_array().map(f32::to_bits),
        a.slerp(b, 0.5).to_array().map(f32::to_bits),
        "the pin below is vacuous: det and libm slerp agree on this host"
    );

    let sk = column(2, 1.0);
    let mut pose = sk.bind_pose_pose();
    solve_look_at(
        &sk,
        &mut pose,
        1,
        Quat::IDENTITY,
        Vec3::new(0.4, 1.3, -2.0),
        LookAtSettings {
            max_yaw: 1.2,
            max_pitch: 1.0,
            weight: 0.5,
        },
    );
    let q = pose.rotations[1];
    let bits = q.to_array().map(f32::to_bits);
    // The slerp is the det kernel (the same bits on every libm); the clamp and the products around
    // it are glam `Quat` and `Vec3` math, whose lane grouping differs on SSE2 and NEON (ADR-5176),
    // so the pin is one value per instruction set (ADR-5860, aarch64 measured on the M5). `z` is a
    // cancellation residue near 1e-10, which is why its bits move most.
    // glam scalar-math (`cross-platform-determinism`, ADR-5176) gives x86_64 the aarch64 bits
    // (measured on the build server, lane gate-lavapipe-coverage), so that build takes the aarch64 pin.
    #[cfg(not(any(target_arch = "aarch64", feature = "cross-platform-determinism")))]
    const PIN: [u32; 4] = [1024879545, 3175649188, 2952805773, 1065321761];
    #[cfg(any(target_arch = "aarch64", feature = "cross-platform-determinism"))]
    const PIN: [u32; 4] = [1024879546, 3175649190, 796933517, 1065321761];
    assert_eq!(bits, PIN, "blended look-at quat {q:?}");
}
