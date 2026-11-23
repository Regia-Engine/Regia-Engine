// SPDX-License-Identifier: MPL-2.0
//! `Curve` sampling at its numeric edges (NaN and infinite times, many keys, key-to-key
//! continuity) and the Hermite rules for each value type.

use regia_core::glam::{Quat, Vec3};
use regia_core::math::{Curve, Interp, Keyframe};

fn linear_ramp() -> Curve<f32> {
    Curve::linear([(0.0, 0.0), (1.0, 10.0), (3.0, 30.0), (4.0, 0.0)])
}

#[test]
fn non_finite_sample_times_clamp_to_the_ends() {
    let c = linear_ramp();
    assert_eq!(c.sample(f32::NAN), Some(0.0), "NaN is before every key");
    assert_eq!(c.sample(f32::NEG_INFINITY), Some(0.0));
    assert_eq!(
        c.sample(f32::INFINITY),
        Some(0.0),
        "after the last key: its value"
    );
    assert_eq!(c.sample(-0.0), Some(0.0));
    assert_eq!(c.sample(f32::MAX), Some(0.0));
}

#[test]
fn linear_curve_hits_every_key_exactly_and_is_continuous_between() {
    let c = linear_ramp();
    for k in c.keys() {
        assert_eq!(c.sample(k.time), Some(k.value), "key at {}", k.time);
    }
    assert_eq!(c.sample(0.5), Some(5.0));
    assert_eq!(c.sample(2.0), Some(20.0));
    assert_eq!(c.sample(3.5), Some(15.0));
    let mut prev = c.sample(0.0).expect("non-empty");
    for i in 1..=4000 {
        let v = c.sample(i as f32 * 0.001).expect("non-empty");
        assert!((v - prev).abs() < 0.05, "jump at {i}: {prev} -> {v}");
        prev = v;
    }
}

#[test]
fn sampling_is_deterministic_and_does_not_depend_on_insertion_order() {
    let keys: Vec<_> = (0..50)
        .map(|i| Keyframe::new(i as f32 * 0.25, (i * i) as f32, Interp::Linear, 0.0))
        .collect();
    let forward = Curve::new(keys.clone());
    let mut shuffled = keys.clone();
    // A fixed permutation: reverse, then swap pairs.
    shuffled.reverse();
    for pair in shuffled.as_chunks_mut::<2>().0 {
        pair.swap(0, 1);
    }
    let backward = Curve::new(shuffled);
    assert_eq!(forward, backward);
    let mut inserted = Curve::default();
    for key in keys.into_iter().rev() {
        inserted.insert(key);
    }
    assert_eq!(forward, inserted);
    for i in 0..500 {
        let t = i as f32 * 0.03;
        assert_eq!(
            forward.sample(t).map(f32::to_bits),
            inserted.sample(t).map(f32::to_bits)
        );
    }
}

#[test]
fn hermite_with_the_chord_slope_as_tangent_is_a_straight_line() {
    // Keys (0, 0) and (2, 8) with slope 4 on both tangents: the Hermite curve is the line 4t.
    let a = Keyframe {
        time: 0.0,
        value: 0.0f32,
        interp: Interp::CubicHermite,
        in_tangent: 4.0,
        out_tangent: 4.0,
    };
    let b = Keyframe {
        time: 2.0,
        value: 8.0f32,
        interp: Interp::CubicHermite,
        in_tangent: 4.0,
        out_tangent: 4.0,
    };
    let c = Curve::new(vec![a, b]);
    for i in 0..=20 {
        let t = i as f32 * 0.1;
        let v = c.sample(t).expect("non-empty");
        assert!((v - 4.0 * t).abs() < 1e-5, "t={t}: {v}");
    }
}

#[test]
fn hermite_tangent_units_are_value_per_curve_time() {
    // The same shape over twice the duration with half the slope gives the same values at matching phase.
    let seg = |len: f32, slope: f32| {
        Curve::new(vec![
            Keyframe {
                time: 0.0,
                value: 0.0f32,
                interp: Interp::CubicHermite,
                in_tangent: slope,
                out_tangent: slope,
            },
            Keyframe {
                time: len,
                value: 1.0f32,
                interp: Interp::CubicHermite,
                in_tangent: slope,
                out_tangent: slope,
            },
        ])
    };
    let short = seg(1.0, 2.0);
    let long = seg(2.0, 1.0);
    for i in 0..=10 {
        let phase = i as f32 / 10.0;
        let (a, b) = (
            short.sample(phase).expect("a"),
            long.sample(phase * 2.0).expect("b"),
        );
        assert!((a - b).abs() < 1e-5, "phase {phase}: {a} vs {b}");
    }
}

#[test]
fn a_hermite_segment_after_a_step_key_uses_the_step_rule_for_that_segment_only() {
    let c = Curve::new(vec![
        Keyframe::new(0.0, 1.0f32, Interp::Step, 0.0),
        Keyframe::new(1.0, 5.0, Interp::Linear, 0.0),
        Keyframe::new(2.0, 9.0, Interp::Step, 0.0),
    ]);
    assert_eq!(c.sample(0.99), Some(1.0));
    assert_eq!(c.sample(1.5), Some(7.0));
    assert_eq!(c.sample(5.0), Some(9.0));
}

#[test]
fn vector_and_rotation_hermite_stay_well_formed() {
    let v = Curve::new(vec![
        Keyframe {
            time: 0.0,
            value: Vec3::ZERO,
            interp: Interp::CubicHermite,
            in_tangent: Vec3::X,
            out_tangent: Vec3::X,
        },
        Keyframe {
            time: 1.0,
            value: Vec3::Y,
            interp: Interp::CubicHermite,
            in_tangent: Vec3::Y,
            out_tangent: Vec3::Y,
        },
    ]);
    assert_eq!(v.sample(0.0), Some(Vec3::ZERO));
    assert_eq!(v.sample(1.0), Some(Vec3::Y));
    assert!(v.sample(0.5).expect("mid").is_finite());

    let q0 = Quat::IDENTITY;
    let q1 = Quat::from_rotation_z(1.2);
    let zero = Quat::from_xyzw(0.0, 0.0, 0.0, 0.0);
    let q = Curve::new(vec![
        Keyframe {
            time: 0.0,
            value: q0,
            interp: Interp::CubicHermite,
            in_tangent: zero,
            out_tangent: zero,
        },
        Keyframe {
            time: 1.0,
            value: q1,
            interp: Interp::CubicHermite,
            in_tangent: zero,
            out_tangent: zero,
        },
    ]);
    for i in 0..=10 {
        let r = q.sample(i as f32 / 10.0).expect("non-empty");
        assert!(
            (r.length() - 1.0).abs() < 1e-5,
            "unit quaternion at {i}: {}",
            r.length()
        );
        assert!(r.is_finite());
    }
    // Linear rotation takes the short way round: 350 degrees is 10 degrees backwards.
    let spin = Curve::linear([
        (0.0, Quat::IDENTITY),
        (1.0, Quat::from_rotation_z(350f32.to_radians())),
    ]);
    let half = spin.sample(0.5).expect("mid");
    assert!((half.to_euler(regia_core::glam::EulerRot::ZXY).0 + 5f32.to_radians()).abs() < 1e-4);
}

#[test]
fn remove_and_insert_keep_the_curve_consistent() {
    let mut c = linear_ramp();
    assert_eq!(c.remove(99), None);
    let removed = c.remove(1).expect("second key");
    assert_eq!(removed.time, 1.0);
    let across = c.sample(1.0).expect("non-empty");
    assert!(
        (across - 10.0).abs() < 1e-4,
        "interpolates across the gap: {across}"
    );
    assert_eq!(c.keys().len(), 3);
    c.insert(Keyframe::new(1.0, 99.0, Interp::Linear, 0.0));
    assert_eq!(c.keys()[1].value, 99.0, "re-inserted in order");
    while c.remove(0).is_some() {}
    assert!(c.is_empty());
    assert_eq!((c.start_time(), c.end_time(), c.duration()), (0.0, 0.0, 0.0));
    assert_eq!(c.sample(1.0), None);
    assert_eq!(c.sample_or(1.0, 7.0), 7.0);
}

#[test]
fn curve_drops_nan_key_times() {
    use regia_core::math::{Curve, Interp, Keyframe};
    let k = |t: f32, v: f32| Keyframe::new(t, v, Interp::Linear, v);
    let mut c = Curve::new(vec![
        k(f32::NAN, 9.0),
        k(0.0, 0.0),
        k(1.0, 1.0),
        k(-f32::NAN, 7.0),
    ]);
    c.insert(k(f32::NAN, 5.0));
    assert_eq!(c.keys().len(), 2);
    assert_eq!(c.sample(5.0), Some(1.0));
    assert_eq!(c.sample(-5.0), Some(0.0));
}
