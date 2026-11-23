// SPDX-License-Identifier: MPL-2.0
//! Clip sampling, stepping, notifies, slicing and root motion at their boundaries: NaN and
//! infinite times, unsorted or mismatched keys, laps, reverse playback and empty clips. Fixed
//! inputs only; the property loops use a fixed-seed generator.

use regia_animation::clip::{ClipStep, JointTrack, Notify, TrackCurve};
use regia_animation::{AnimationClip, Pose, RootMotionMode, RootMotionTrack};
use regia_core::math::{Quat, Vec3};

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

fn ramp() -> TrackCurve<f32> {
    TrackCurve::Animated {
        times: vec![0.0, 1.0, 3.0],
        values: vec![0.0, 10.0, 30.0],
        exact_rotation: false,
    }
}

#[test]
fn sampling_at_a_non_finite_time_never_returns_nan() {
    let f = ramp();
    let v = TrackCurve::Animated {
        times: vec![0.0, 1.0, 2.0],
        values: vec![Vec3::ZERO, Vec3::X, Vec3::Y],
        exact_rotation: false,
    };
    let q = TrackCurve::Animated {
        times: vec![0.0, 1.0, 2.0],
        values: vec![
            Quat::IDENTITY,
            Quat::from_rotation_x(1.0),
            Quat::from_rotation_x(2.0),
        ],
        exact_rotation: true,
    };
    for time in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(f.sample(time).is_finite(), "f32 curve at {time}");
        assert!(v.sample(time).is_finite(), "vec3 curve at {time}");
        assert!(q.sample(time).is_finite(), "quat curve at {time}");
    }
    assert_eq!(f.sample(f32::INFINITY), 30.0);
    assert_eq!(f.sample(f32::NEG_INFINITY), 0.0);
    assert_eq!(f.sample(f32::NAN), 0.0, "a NaN clock samples the start");
}

#[test]
fn sampling_is_clamped_exact_at_keys_and_linear_between() {
    let f = ramp();
    assert_eq!(f.sample(-5.0), 0.0);
    assert_eq!(f.sample(0.0), 0.0);
    assert_eq!(f.sample(0.5), 5.0);
    assert_eq!(f.sample(1.0), 10.0);
    assert_eq!(f.sample(2.0), 20.0);
    assert_eq!(f.sample(3.0), 30.0);
    assert_eq!(f.sample(99.0), 30.0);
}

#[test]
fn malformed_curves_do_not_panic_or_poison_the_pose() {
    // More times than values, more values than times, empty, duplicate times, unsorted times.
    let shapes = [
        (vec![0.0, 1.0, 2.0], vec![1.0, 2.0]),
        (vec![0.0, 1.0], vec![1.0, 2.0, 3.0]),
        (vec![], vec![]),
        (vec![1.0, 1.0, 1.0], vec![1.0, 2.0, 3.0]),
        (vec![2.0, 0.0, 1.0], vec![1.0, 2.0, 3.0]),
    ];
    for (times, values) in shapes {
        let curve = TrackCurve::Animated {
            times,
            values,
            exact_rotation: false,
        };
        for time in [-1.0, 0.0, 0.5, 1.0, 2.0, 9.0] {
            assert!(curve.sample(time).is_finite(), "{curve:?} at {time}");
        }
    }
    // sample_pose leaves a joint with an unsampleable curve at its previous value.
    let mut clip = AnimationClip::empty("bad", 1.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![],
            values: vec![],
            exact_rotation: false,
        },
        rotations: TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Quat::IDENTITY],
            exact_rotation: false,
        },
        scales: TrackCurve::Static(Vec3::splat(2.0)),
    });
    clip.tracks.push(JointTrack {
        joint: 99,
        translations: TrackCurve::Static(Vec3::ONE),
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let mut pose = Pose::from_identity(2);
    pose.translations[0] = Vec3::splat(7.0);
    clip.sample_pose(0.5, &mut pose);
    assert_eq!(
        pose.translations[0],
        Vec3::splat(7.0),
        "empty curve keeps the old value"
    );
    assert_eq!(pose.scales[0], Vec3::splat(2.0));
    assert_eq!(pose.len(), 2, "a track past the pose is skipped");
}

#[test]
fn track_curve_metadata() {
    let f = ramp();
    assert_eq!((f.key_count(), f.start_time(), f.end_time()), (3, 0.0, 3.0));
    let s: TrackCurve<f32> = TrackCurve::Static(1.0);
    assert_eq!((s.key_count(), s.start_time(), s.end_time()), (1, 0.0, 0.0));
    let empty: TrackCurve<f32> = TrackCurve::Animated {
        times: vec![],
        values: vec![],
        exact_rotation: false,
    };
    assert_eq!(
        (empty.key_count(), empty.start_time(), empty.end_time()),
        (0, 0.0, 0.0)
    );
    assert!(!empty.is_sampleable());
    assert_eq!(empty.sample(1.0), 0.0);
}

#[test]
fn clip_step_is_well_formed_for_fixed_seed_inputs() {
    let mut rng = Lcg(0xc11b);
    let specials = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        0.0,
        -0.0,
        1e30,
        -1e30,
        f32::MAX,
    ];
    for i in 0..500 {
        let duration = 0.1 + rng.unit() * 4.0;
        let prev = if i % 7 == 0 {
            specials[i % specials.len()]
        } else {
            rng.unit() * duration * 1.5 - 0.1
        };
        let delta = if i % 5 == 0 {
            specials[(i / 5) % specials.len()]
        } else {
            (rng.unit() - 0.5) * duration * 6.0
        };
        for looping in [false, true] {
            let s = ClipStep::advance(prev, delta, duration, looping);
            assert!(
                s.prev.is_finite() && s.time.is_finite(),
                "{prev} {delta} {duration} -> {s:?}"
            );
            assert!((0.0..=duration).contains(&s.prev), "{s:?}");
            assert!((0.0..=duration).contains(&s.time), "{s:?}");
            if !looping {
                assert_eq!(s.loops, 0);
            } else if delta.is_finite() && delta.abs() < 1e6 {
                // The unwrapped clock is conserved (to f32 precision of the lap count).
                let unwrapped = s.time + s.loops as f32 * duration;
                assert!(
                    (unwrapped - (s.prev + delta)).abs() < 1e-2 * (1.0 + s.loops.abs() as f32),
                    "{s:?} delta {delta}"
                );
            }
            let again = ClipStep::advance(prev, delta, duration, looping);
            assert_eq!(s, again, "deterministic");
        }
    }
}

#[test]
fn clip_step_boundaries() {
    // Non-positive or non-finite durations yield the default step.
    for d in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(
            ClipStep::advance(0.5, 0.5, d, true),
            ClipStep::default(),
            "duration {d}"
        );
    }
    let s = ClipStep::advance(0.9, 0.2, 1.0, true);
    assert_eq!(s.loops, 1);
    assert!((s.time - 0.1).abs() < 1e-5);
    assert!(!s.is_reverse());
    let r = ClipStep::advance(0.1, -0.2, 1.0, true);
    assert_eq!(r.loops, -1);
    assert!((r.time - 0.9).abs() < 1e-5);
    assert!(r.is_reverse());
    let two = ClipStep::advance(0.0, 2.5, 1.0, true);
    assert_eq!(two.loops, 2);
    let clamped = ClipStep::advance(0.5, 9.0, 1.0, false);
    assert_eq!((clamped.time, clamped.loops), (1.0, 0));
    let back = ClipStep::advance(0.5, -9.0, 1.0, false);
    assert_eq!((back.time, back.loops), (0.0, 0));
    assert!(back.is_reverse());
    // A zero delta in the middle is a no-op step in neither direction.
    let still = ClipStep::advance(0.4, 0.0, 1.0, true);
    assert_eq!((still.time, still.loops), (0.4, 0));
    assert!(!still.is_reverse());
    assert!(AnimationClip::advance(0.9, 0.2, 1.0, true).1);
    // A huge delta saturates the lap count instead of overflowing.
    let huge = ClipStep::advance(0.0, 1e30, 1.0, true);
    assert!(huge.time.is_finite() && huge.loops > 0);
}

fn notify_clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("n", 1.0);
    for (time, name) in [(0.0, "start"), (0.25, "a"), (0.5, "b"), (1.0, "end")] {
        clip.notifies.push(Notify {
            time,
            name: name.into(),
            payload: None,
        });
    }
    clip
}

fn names(out: &[Notify]) -> Vec<&str> {
    out.iter().map(|n| n.name.as_str()).collect()
}

#[test]
fn notifies_fire_on_the_half_open_interval_in_playback_order() {
    let clip = notify_clip();
    let mut out = Vec::new();
    clip.notifies_crossed(0.0, 0.5, false, &mut out);
    assert_eq!(
        names(&out),
        ["a", "b"],
        "(prev, end]: start excluded, end included"
    );
    out.clear();
    clip.notifies_in_step(
        &ClipStep {
            prev: 0.0,
            time: 0.25,
            loops: 0,
        },
        true,
        &mut out,
    );
    assert_eq!(
        names(&out),
        ["start", "a"],
        "include_start adds a notify exactly at prev"
    );
    out.clear();
    clip.notifies_in_step(
        &ClipStep {
            prev: 0.5,
            time: 0.0,
            loops: 0,
        },
        false,
        &mut out,
    );
    assert_eq!(
        names(&out),
        ["a", "start"],
        "reverse: [end, prev) newest first, prev excluded"
    );
    out.clear();
    clip.notifies_in_step(
        &ClipStep {
            prev: 0.5,
            time: 0.5,
            loops: 0,
        },
        false,
        &mut out,
    );
    assert!(out.is_empty(), "a zero-length step fires nothing");
    out.clear();
    // Across a wrap the notify at 0 fires as well as the one at the end.
    clip.notifies_crossed(0.75, 0.3, true, &mut out);
    assert_eq!(
        names(&out),
        ["start", "end", "a"],
        "same-instant notifies keep clip order"
    );
}

#[test]
fn many_laps_cap_the_repeats_and_keep_time_order() {
    let clip = notify_clip();
    let mut out = Vec::new();
    let step = ClipStep {
        prev: 0.0,
        time: 0.0,
        loops: 1000,
    };
    clip.notifies_in_step(&step, false, &mut out);
    assert_eq!(out.len(), 4 * regia_animation::clip::MAX_NOTIFY_REPEATS as usize);
    // Time order on the unwrapped clock; notifies at the same instant (the end of lap one is
    // the start of lap two) keep their clip order.
    assert_eq!(names(&out[..4]), ["a", "b", "start", "end"]);
    // A clip with no notifies, or a degenerate duration, is a no-op.
    let mut empty = AnimationClip::empty("e", 1.0);
    let mut sink = Vec::new();
    empty.notifies_in_step(&step, false, &mut sink);
    assert!(sink.is_empty());
    empty.duration = 0.0;
    empty.notifies = notify_clip().notifies;
    empty.notifies_in_step(
        &ClipStep {
            prev: 0.0,
            time: 0.0,
            loops: 3,
        },
        false,
        &mut sink,
    );
    assert!(
        sink.len() <= 4,
        "a zero-duration clip falls back to the single-lap rule"
    );
}

#[test]
fn slice_matches_the_original_curve_at_fixed_seed_times() {
    let mut clip = AnimationClip::empty("src", 3.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, 1.0, 3.0],
            values: vec![Vec3::ZERO, Vec3::X, Vec3::new(5.0, 0.0, 0.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.notifies = notify_clip().notifies;
    let part = clip.slice("part", 0.5, 2.0);
    assert_eq!(part.duration, 1.5);
    let mut rng = Lcg(77);
    for _ in 0..100 {
        let t = rng.unit() * 1.5;
        let original = clip.tracks[0].translations.sample(0.5 + t);
        let sliced = part.tracks[0].translations.sample(t);
        assert!(
            (original - sliced).length() < 1e-5,
            "t {t}: {original} vs {sliced}"
        );
    }
    let times: Vec<_> = part.notifies.iter().map(|n| (n.name.as_str(), n.time)).collect();
    assert_eq!(
        times,
        [("b", 0.0), ("end", 0.5)],
        "only notifies inside [0.5, 2.0], shifted to the slice start"
    );

    // Reversed, out-of-range and non-finite bounds clamp instead of panicking.
    assert_eq!(clip.slice("r", 2.0, 1.0).duration, 0.0);
    assert_eq!(clip.slice("o", -5.0, 99.0).duration, 3.0);
    assert_eq!(clip.slice("n", f32::NAN, f32::NAN).duration, 3.0);
    assert_eq!(clip.slice("i", f32::NEG_INFINITY, f32::INFINITY).duration, 3.0);
    let mut weird = clip.clone();
    weird.duration = f32::NAN;
    assert_eq!(weird.slice("w", 0.0, 1.0).duration, 0.0);
}

fn moving_clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("walk", 2.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, 2.0],
            values: vec![Vec3::new(1.0, 0.5, 0.0), Vec3::new(1.0, 0.5, -4.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Animated {
            times: vec![0.0, 2.0],
            values: vec![Quat::IDENTITY, Quat::from_rotation_y(1.0)],
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.root_motion = Some(RootMotionTrack {
        joint: 0,
        mode: RootMotionMode::XZ,
    });
    clip
}

#[test]
fn root_motion_accumulates_across_laps_and_reverses_exactly() {
    let clip = moving_clip();
    let one = clip.root_motion_delta_full(1.5, 0.5, true).expect("delta");
    // 0.5 s to the end + 0.5 s from the start: 1 m of the 4 m lap... (0.5/2)*4 each.
    assert!((one.0.z + 2.0).abs() < 1e-5, "{:?}", one.0);
    // Two full laps from the start add exactly twice one lap's displacement and turn.
    let laps = clip
        .root_motion_delta_step(&ClipStep {
            prev: 0.0,
            time: 0.0,
            loops: 2,
        })
        .expect("delta");
    assert!((laps.0.z + 8.0).abs() < 1e-4);
    assert!(laps.1.angle_between(Quat::from_rotation_y(2.0)) < 1e-4);
    // Reverse is the inverse of forward.
    let fwd = clip
        .root_motion_delta_step(&ClipStep {
            prev: 0.5,
            time: 1.5,
            loops: 0,
        })
        .expect("f");
    let rev = clip
        .root_motion_delta_step(&ClipStep {
            prev: 1.5,
            time: 0.5,
            loops: 0,
        })
        .expect("r");
    assert!((fwd.0 + rev.0).length() < 1e-5);
    assert!((fwd.1 * rev.1).angle_between(Quat::IDENTITY) < 1e-4);
    // The mode filters: XZ keeps horizontal translation and drops rotation's pitch/roll only.
    let filtered = clip.root_motion_delta(0.0, 1.0, false).expect("filtered");
    assert_eq!(filtered.0.y, 0.0);
    // No root motion, or a root joint without a track: None.
    assert!(
        AnimationClip::empty("none", 1.0)
            .root_motion_delta_full(0.0, 0.5, false)
            .is_none()
    );
    let mut orphan = moving_clip();
    orphan.root_motion = Some(RootMotionTrack {
        joint: 9,
        mode: RootMotionMode::XYZ,
    });
    assert!(orphan.root_motion_delta_full(0.0, 1.0, false).is_none());
    // Repeated evaluation is bit-identical.
    let a = clip.root_motion_delta_step(&ClipStep {
        prev: 0.3,
        time: 0.1,
        loops: 3,
    });
    let b = clip.root_motion_delta_step(&ClipStep {
        prev: 0.3,
        time: 0.1,
        loops: 3,
    });
    assert_eq!(a, b);
}

#[test]
fn pin_root_in_place_modes() {
    let clip = moving_clip();
    let mut pose = Pose::from_identity(1);
    pose.translations[0] = Vec3::new(9.0, 8.0, 7.0);
    clip.pin_root_in_place(&mut pose, RootMotionMode::None);
    assert_eq!(pose.translations[0], Vec3::new(9.0, 8.0, 7.0));
    clip.pin_root_in_place(&mut pose, RootMotionMode::XZ);
    assert_eq!(
        pose.translations[0],
        Vec3::new(1.0, 8.0, 0.0),
        "x/z pinned, height animated"
    );
    clip.pin_root_in_place(&mut pose, RootMotionMode::XYZ);
    assert_eq!(pose.translations[0], Vec3::new(1.0, 0.5, 0.0));
    pose.rotations[0] = Quat::from_rotation_y(1.0) * Quat::from_rotation_x(0.3);
    clip.pin_root_in_place(&mut pose, RootMotionMode::Yaw);
    let (yaw, pitch, _) = pose.rotations[0].to_euler(regia_core::glam::EulerRot::YXZ);
    assert!(yaw.abs() < 1e-4, "yaw pinned to the time-zero yaw: {yaw}");
    assert!((pitch - 0.3).abs() < 1e-4, "pitch stays animated: {pitch}");
    // A pose too short for the root joint is left alone.
    let mut empty = Pose::from_identity(0);
    clip.pin_root_in_place(&mut empty, RootMotionMode::XYZ);
    assert!(empty.is_empty());
}
