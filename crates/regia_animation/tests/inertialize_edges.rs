// SPDX-License-Identifier: MPL-2.0
//! Inertialization at its boundaries: decay shapes for fixed-seed parameters, hostile numbers,
//! mismatched pose lengths, interruption, and the layer weight smoother. Everything is stepped
//! with explicit frame deltas.

use regia_animation::Pose;
use regia_animation::inertialize::{
    DecayMethod, Inertializer, QuinticDecay, SPRING_RATE, Switch, WeightInertia,
};
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
}

#[test]
fn quintic_decay_starts_at_x0_ends_at_zero_and_never_overshoots() {
    let mut rng = Lcg(11);
    for i in 0..300 {
        let x0 = 0.001 + rng.unit() * 3.0;
        let v0 = (rng.unit() - 0.7) * 20.0; // mostly shrinking, sometimes growing
        let duration = 0.05 + rng.unit() * 2.0;
        let d = QuinticDecay::new(x0, v0, duration);
        assert!(
            d.end() > 0.0 && d.end() <= duration + 1e-6,
            "#{i}: end {} duration {duration}",
            d.end()
        );
        assert!(
            (d.eval(0.0) - x0).abs() < 1e-4 * (1.0 + x0),
            "#{i}: starts at {}",
            d.eval(0.0)
        );
        assert_eq!(d.eval(d.end()), 0.0);
        assert_eq!(d.eval(d.end() + 10.0), 0.0);
        let mut previous = f32::MAX;
        for k in 0..=64 {
            let t = d.end() * k as f32 / 64.0;
            let x = d.eval(t);
            assert!(x.is_finite() && x >= -1e-3 * (1.0 + x0), "#{i} t {t}: {x}");
            // The decay never grows past its start (Bollo's clamps).
            assert!(x <= x0 * (1.0 + 1e-3) + 1e-4, "#{i} t {t}: {x} > {x0}");
            // And it is non-increasing up to rounding.
            assert!(x <= previous + 1e-3 * (1.0 + x0), "#{i} t {t}: {x} > {previous}");
            previous = x;
        }
        assert_eq!(d, QuinticDecay::new(x0, v0, duration), "deterministic");
    }
}

#[test]
fn quintic_decay_with_hostile_numbers_is_an_empty_decay() {
    let empty = QuinticDecay::default();
    for (x0, v0, duration) in [
        (0.0, 0.0, 1.0),
        (-1.0, 0.0, 1.0),
        (f32::NAN, 0.0, 1.0),
        (f32::INFINITY, 0.0, 1.0),
        (1.0, 0.0, 0.0),
        (1.0, 0.0, -1.0),
    ] {
        assert_eq!(QuinticDecay::new(x0, v0, duration), empty, "{x0} {v0} {duration}");
        assert_eq!(empty.eval(0.0), 0.0);
    }
    // A NaN or infinite velocity is read as zero; a velocity pointing away is clamped.
    let base = QuinticDecay::new(1.0, 0.0, 1.0);
    assert_eq!(QuinticDecay::new(1.0, f32::NAN, 1.0), base);
    assert_eq!(QuinticDecay::new(1.0, f32::INFINITY, 1.0), base);
    assert_eq!(QuinticDecay::new(1.0, 5.0, 1.0), base);
    // A very fast arrival shortens the decay.
    assert!(QuinticDecay::new(1.0, -100.0, 1.0).end() < 0.1);
    // Negative time reads as the start; NaN time never produces NaN.
    assert!((base.eval(-5.0) - 1.0).abs() < 1e-5);
    assert!(
        (base.eval(f32::NAN) - 1.0).abs() < 1e-5,
        "a NaN time reads as the start"
    );
}

fn pose_at(angle: f32, x: f32, n: usize) -> Pose {
    let mut pose = Pose::from_identity(n);
    for i in 0..n {
        pose.rotations[i] = Quat::from_rotation_z(angle);
        pose.translations[i] = Vec3::new(x, 0.0, 0.0);
    }
    pose
}

#[test]
fn applying_at_the_switch_reproduces_the_source_and_settles_at_the_target() {
    for method in [DecayMethod::Quintic, DecayMethod::Spring] {
        let source = pose_at(1.0, 0.5, 3);
        let target = pose_at(-0.4, -0.2, 3);
        let mut inertializer = Inertializer::default();
        assert!(!inertializer.is_active());
        inertializer.start(
            method,
            0.4,
            Switch {
                source: &source,
                source_prev: None,
                target: &target,
                target_prev: None,
                dt: 1.0 / 60.0,
            },
        );
        assert!(inertializer.is_active());
        assert_eq!(inertializer.method(), method);
        assert_eq!(inertializer.duration(), 0.4);
        let mut shown = target.clone();
        inertializer.apply(&mut shown);
        for i in 0..3 {
            assert!(
                shown.rotations[i].angle_between(source.rotations[i]) < 1e-4,
                "{method:?} joint {i}"
            );
            assert!((shown.translations[i] - source.translations[i]).length() < 1e-4);
        }
        assert!(inertializer.max_rotation_offset() > 1.0);
        assert!(inertializer.max_translation_offset() > 0.5);
        // Step to the end: the offset shrinks and the transition finishes at the duration.
        let mut last = f32::MAX;
        let mut frames = 0;
        while inertializer.is_active() {
            inertializer.advance(1.0 / 60.0);
            let mut p = target.clone();
            inertializer.apply(&mut p);
            let offset = inertializer.max_rotation_offset();
            assert!(offset <= last + 1e-4, "{method:?}: offset grew {offset} > {last}");
            last = offset;
            frames += 1;
            assert!(frames < 200, "{method:?} never finished");
        }
        assert!(
            (24..=26).contains(&frames),
            "{method:?}: {frames} frames for 0.4 s at 60 Hz"
        );
        let mut settled = target.clone();
        inertializer.apply(&mut settled);
        assert_eq!(settled, target, "inactive apply is a no-op");
        assert_eq!(inertializer.max_rotation_offset(), 0.0);
    }
}

#[test]
fn start_edge_cases() {
    let a = pose_at(1.0, 0.0, 2);
    let b = pose_at(0.0, 0.0, 2);
    fn switch<'a>(source: &'a Pose, target: &'a Pose, dt: f32) -> Switch<'a> {
        Switch {
            source,
            source_prev: None,
            target,
            target_prev: None,
            dt,
        }
    }
    let mut x = Inertializer::default();
    // Zero, negative, NaN and infinite durations are cuts.
    for duration in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        x.start(DecayMethod::Quintic, duration, switch(&a, &b, 0.016));
        assert!(!x.is_active(), "duration {duration}");
        let mut p = b.clone();
        x.apply(&mut p);
        assert_eq!(p, b);
    }
    // Empty poses cannot be inertialized.
    let nothing = Pose::from_identity(0);
    x.start(DecayMethod::Spring, 0.5, switch(&nothing, &b, 0.016));
    assert!(!x.is_active());
    // Mismatched lengths use the common prefix and never panic on apply.
    let five = pose_at(1.0, 0.0, 5);
    x.start(DecayMethod::Quintic, 0.5, switch(&five, &b, 0.016));
    assert!(x.is_active());
    let mut long = pose_at(0.0, 0.0, 9);
    x.apply(&mut long);
    assert!(long.rotations[1].angle_between(Quat::from_rotation_z(1.0)) < 1e-4);
    assert_eq!(
        long.rotations[2],
        Quat::IDENTITY,
        "joints past the switch pose are untouched"
    );
    let mut short = pose_at(0.0, 0.0, 1);
    x.apply(&mut short);
    // A previous pose shorter than the switch pose, or a zero dt, means "at rest".
    let short_prev = pose_at(0.9, 0.0, 1);
    let mut y = Inertializer::default();
    y.start(
        DecayMethod::Quintic,
        0.5,
        Switch {
            source: &a,
            source_prev: Some(&short_prev),
            target: &b,
            target_prev: Some(&short_prev),
            dt: 0.016,
        },
    );
    let mut z = Inertializer::default();
    z.start(DecayMethod::Quintic, 0.5, switch(&a, &b, 0.016));
    let (mut py, mut pz) = (b.clone(), b.clone());
    y.advance(0.1);
    z.advance(0.1);
    y.apply(&mut py);
    z.apply(&mut pz);
    assert_eq!(py, pz);
    // Negative and NaN frame deltas do not move the clock backwards.
    let before = z.elapsed();
    z.advance(-5.0);
    z.advance(f32::NAN);
    assert_eq!(z.elapsed(), before);
    // Stop is idempotent; a restart replaces the running transition.
    z.stop();
    z.stop();
    assert!(!z.is_active());
    z.start(DecayMethod::Spring, 0.3, switch(&a, &b, 0.016));
    assert_eq!(
        (z.elapsed(), z.duration(), z.method()),
        (0.0, 0.3, DecayMethod::Spring)
    );
}

#[test]
fn velocity_inputs_never_produce_non_finite_poses() {
    let a = pose_at(1.0, 0.5, 2);
    let b = pose_at(0.0, 0.0, 2);
    let fast = pose_at(-3.0, 40.0, 2);
    for method in [DecayMethod::Quintic, DecayMethod::Spring] {
        for dt in [0.0, 1e-9, 0.016, 1.0, f32::NAN, f32::INFINITY] {
            let mut x = Inertializer::default();
            x.start(
                method,
                0.3,
                Switch {
                    source: &a,
                    source_prev: Some(&fast),
                    target: &b,
                    target_prev: Some(&fast),
                    dt,
                },
            );
            for _ in 0..30 {
                x.advance(0.016);
                let mut p = b.clone();
                x.apply(&mut p);
                assert!(
                    p.rotations.iter().all(|q| q.is_finite()) && p.translations.iter().all(|t| t.is_finite()),
                    "{method:?} dt {dt}"
                );
            }
        }
    }
}

#[test]
fn weight_inertia_eases_and_survives_hostile_numbers() {
    let mut w = WeightInertia::default();
    assert_eq!(w.update(0.0, 0.3, 1.0 / 60.0), 0.0, "the first call snaps");
    let mut values = Vec::new();
    for _ in 0..40 {
        values.push(w.update(1.0, 0.3, 1.0 / 60.0));
    }
    assert!(values[0] < 0.5, "an aim layer no longer pops: {}", values[0]);
    assert!(
        (values.last().copied().unwrap_or(0.0) - 1.0).abs() < 1e-4,
        "settles on the target"
    );
    assert!(
        values.windows(2).all(|p| p[1] >= p[0] - 1e-4),
        "monotone rise: {values:?}"
    );
    assert!(values.iter().all(|v| (0.0..=1.0).contains(v)));
    assert_eq!(w.output(), 1.0);
    // Hostile inputs: the output stays a finite weight in 0..=1 and recovers.
    let mut h = WeightInertia::default();
    h.update(0.5, 0.3, 0.016);
    for (target, duration, dt) in [
        (f32::NAN, 0.3, 0.016),
        (f32::INFINITY, 0.3, 0.016),
        (1.0, f32::NAN, 0.016),
        (1.0, f32::INFINITY, 0.016),
        (0.0, 0.3, f32::NAN),
        (1.0, 0.3, f32::INFINITY),
        (0.0, 0.3, -1.0),
        (0.2, -4.0, 0.016),
    ] {
        let out = h.update(target, duration, dt);
        assert!(
            (0.0..=1.0).contains(&out),
            "({target}, {duration}, {dt}) -> {out}"
        );
    }
    let mut settle = 0.0;
    for _ in 0..60 {
        settle = h.update(0.8, 0.3, 0.016);
    }
    assert!((settle - 0.8).abs() < 1e-3, "recovers: {settle}");
    // Duration 0 follows directly.
    let mut direct = WeightInertia::default();
    direct.update(0.0, 0.0, 0.016);
    assert_eq!(direct.update(1.0, 0.0, 0.016), 1.0);
}

/// A spring-decay transition with a 90° joint offset, stepped one 60 Hz frame: the
/// resulting joint rotation is pinned bit for bit (audit-v040 M5). The pinned bits
/// come from the `det` kernel, so they hold on Linux, Windows and Android alike.
#[test]
fn spring_channel_offset_is_pinned_and_platform_stable() {
    let mut source = Pose::from_identity(1);
    source.rotations[0] = det::quat_from_rotation_z(std::f32::consts::FRAC_PI_2);
    let target = Pose::from_identity(1);
    let mut inertializer = Inertializer::default();
    inertializer.start(
        DecayMethod::Spring,
        0.25,
        Switch {
            source: &source,
            source_prev: None,
            target: &target,
            target_prev: None,
            dt: 1.0 / 60.0,
        },
    );
    inertializer.advance(1.0 / 60.0);
    let mut shown = target.clone();
    inertializer.apply(&mut shown);

    // Rebuild the expected rotation through the same det math the engine uses: the
    // target is the identity and the switch is from rest, so the offset velocity is
    // zero and x(t) = (x0 + (v0 + y x0) t) e^(−y t) about the offset axis.
    let t = 1.0f32 / 60.0;
    let y = SPRING_RATE / 0.25;
    let x0 = det::quat_to_scaled_axis(source.rotations[0]);
    let j = x0 * y;
    let r = (x0 + j * t) * det::exp(-y * t);
    let angle = r.length();
    let expected = det::quat_from_axis_angle(r / angle, angle);
    assert_eq!(shown.rotations[0], expected);
    assert_eq!(
        (
            shown.rotations[0].x.to_bits(),
            shown.rotations[0].y.to_bits(),
            shown.rotations[0].z.to_bits(),
            shown.rotations[0].w.to_bits(),
        ),
        (0x0000_0000, 0x0000_0000, 0x3f27_c060, 0x3f41_6108),
    );
    // Sanity: one frame into a 0.25 s spring the offset has decayed but not settled.
    assert!(shown.rotations[0].angle_between(source.rotations[0]) < 0.2);
    assert!(inertializer.max_rotation_offset() > 1.0);
}
