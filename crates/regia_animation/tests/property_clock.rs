// SPDX-License-Identifier: MPL-2.0
//! Property animation runs on the simulation clock, once per fixed step. Regression tests for
//! the read-only determinism audit of `animation3` (finding H1): `apply_property_animators`
//! used to run in `PostUpdate` and advance by the *presentation* frame delta, so two peers
//! whose frames covered a different number of 60 Hz fixed steps simulated a kinematic door
//! (and everything resting on it) against different transforms. Fix (ADR-4653): the system
//! runs in `FixedUpdate` (`regia_app::FixedUpdateSet::Animation`), where `Time::delta` is one
//! fixed step, ordered before the physics step consumes kinematic `Transform`s.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::AnimationPlugin;
use regia_animation::clip::{AnimationClip, PropertyTrack, TrackCurve};
use regia_animation::property::{PropertyAnimator, apply_property_animators};
use regia_app::prelude::*;
use regia_scene::Transform;

/// One fixed step at the default 60 Hz (rounded the same way `FixedTime::step` rounds).
const STEP_NS: u64 = 16_666_667;
/// Two fixed steps plus one nanosecond, dropped by the accumulator: one 1/30 s frame.
const DOUBLE_STEP_NS: u64 = 33_333_334;

/// Clip time is the door height in metres (y = t over a 2 s clip).
fn door_clip() -> Arc<AnimationClip> {
    let mut clip = AnimationClip::empty("door", 2.0);
    clip.property_tracks.push(PropertyTrack {
        path: "Transform.translation.y".into(),
        curve: TrackCurve::Animated {
            times: vec![0.0, 2.0],
            values: vec![0.0, 2.0],
            exact_rotation: false,
        },
    });
    Arc::new(clip)
}

/// Door height at the start of each fixed step: exactly what a kinematic consumer of
/// `Transform` (the physics step) sees on that step.
#[derive(Resource, Default)]
struct Samples(Vec<f32>);

fn record(door: Query<&Transform, With<PropertyAnimator>>, mut samples: ResMut<Samples>) {
    for transform in &door {
        samples.0.push(transform.translation.y);
    }
}

fn app(frame: Duration) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.register_type::<Transform>();
    app.init_resource::<Samples>();
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(frame));
    // Record before the animator applies, like a consumer inside the same fixed step.
    app.add_systems(regia_app::FixedUpdate, record.before(apply_property_animators));
    app.update();
    let door = app
        .world_mut()
        .spawn((
            Transform::default(),
            PropertyAnimator {
                clip: door_clip(),
                time: 0.0,
                speed: 1.0,
                looping: false,
            },
        ))
        .id();
    (app, door)
}

/// Two peers cover the same two fixed steps — one in two 1/60 s frames, the other in one
/// 1/30 s frame — and every step must see the same door height.
#[test]
fn fixed_steps_see_the_same_transforms_whatever_the_frame_pacing() {
    let (mut peer_a, _) = app(Duration::from_nanos(STEP_NS));
    peer_a.update();
    peer_a.update();

    let (mut peer_b, _) = app(Duration::from_nanos(DOUBLE_STEP_NS));
    peer_b.update();

    let a = &peer_a.world().resource::<Samples>().0;
    let b = &peer_b.world().resource::<Samples>().0;
    assert_eq!(a.len(), 2, "peer A ran two fixed steps");
    assert_eq!(b.len(), 2, "peer B ran both fixed steps inside one frame");
    assert_eq!(a, b, "per-step door heights diverge with frame pacing");
    assert_eq!(a[0], 0.0, "the first step sees the closed door");
    assert!(
        (f64::from(a[1]) - 1.0 / 60.0).abs() < 1e-6,
        "the second step sees one fixed step of motion: {}",
        a[1]
    );
}

/// With a `SimStep` installed the simulation clock is pinned to the step; the presentation
/// pacing must not leak into the animator.
#[test]
fn sim_step_advances_the_animator_once_per_fixed_step() {
    let mut times = Vec::new();
    let mut heights = Vec::new();
    for frame in [
        Duration::from_nanos(STEP_NS),
        Duration::from_nanos(DOUBLE_STEP_NS),
    ] {
        let (mut app, door) = app(frame);
        app.insert_resource(regia_app::SimStep(Duration::from_nanos(STEP_NS)));
        for _ in 0..60 {
            app.update();
        }
        times.push(
            app.world()
                .get::<PropertyAnimator>(door)
                .expect("animator")
                .time
                .to_bits(),
        );
        heights.push(
            app.world()
                .get::<Transform>(door)
                .expect("transform")
                .translation
                .y
                .to_bits(),
        );
    }
    assert_eq!(
        times[0], times[1],
        "60 fixed steps advanced the clock differently by pacing"
    );
    assert_eq!(
        heights[0], heights[1],
        "the door height diverged with frame pacing"
    );
    assert!(
        (f64::from(f32::from_bits(times[0])) - 1.0).abs() < 1e-4,
        "60 steps of 1/60 s leave the animator at t = 1: {}",
        f32::from_bits(times[0])
    );
}
