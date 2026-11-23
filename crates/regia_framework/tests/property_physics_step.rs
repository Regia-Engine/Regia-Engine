// SPDX-License-Identifier: MPL-2.0
//! End-to-end for ADR-4653 (audit H1 of `animation3`): a property-animated kinematic platform
//! and a dynamic box resting on it simulate identically whether the fixed steps arrive as two
//! 1/60 s frames or as one 1/30 s frame carrying both steps — the per-step platform pose is
//! what physics consumes, not the per-frame pose.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::AnimationPlugin;
use regia_animation::clip::{AnimationClip, PropertyTrack, TrackCurve};
use regia_animation::property::PropertyAnimator;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsPose, RigidBody};
use regia_scene::Transform;

const STEP_NS: u64 = 16_666_667;
/// Two fixed steps plus one nanosecond, dropped by the accumulator: one 1/30 s frame.
const DOUBLE_STEP_NS: u64 = 33_333_334;

/// Clip time is the platform height in metres (y = t over a 10 s clip).
fn lift_clip() -> Arc<AnimationClip> {
    let mut clip = AnimationClip::empty("lift", 10.0);
    clip.property_tracks.push(PropertyTrack {
        path: "Transform.translation.y".into(),
        curve: TrackCurve::Animated {
            times: vec![0.0, 10.0],
            values: vec![0.0, 10.0],
            exact_rotation: false,
        },
    });
    Arc::new(clip)
}

fn app(frame_ns: u64) -> (App, Entity) {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_nanos(frame_ns)));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, AnimationPlugin));
    app.register_type::<Transform>();
    app.world_mut().spawn((
        Transform::default(),
        RigidBody::kinematic(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(2.0, 0.1, 2.0),
            },
            ..Collider::default()
        },
        PropertyAnimator {
            clip: lift_clip(),
            time: 0.0,
            speed: 1.0,
            looping: false,
        },
    ));
    // A box resting on the platform: carried only if the step sees the platform's new pose.
    let box_id = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.36, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.25),
                },
                ..Collider::default()
            },
        ))
        .id();
    (app, box_id)
}

fn pose(app: &App, body: Entity) -> Vec3 {
    app.world()
        .get::<PhysicsPose>(body)
        .map(|p| p.translation)
        .expect("the box has been stepped")
}

/// Ten fixed steps of a rising platform, taken as 10×1/60 s frames or 5×1/30 s frames, carry
/// the box to the same pose, bit for bit.
#[test]
fn the_box_rides_the_platform_identically_at_either_frame_pacing() {
    let (mut peer_a, box_a) = app(STEP_NS);
    for _ in 0..10 {
        peer_a.update();
    }

    let (mut peer_b, box_b) = app(DOUBLE_STEP_NS);
    for _ in 0..5 {
        peer_b.update();
    }

    let a = pose(&peer_a, box_a);
    let b = pose(&peer_b, box_b);
    assert_eq!(
        a.to_array().map(f32::to_bits),
        b.to_array().map(f32::to_bits),
        "the box diverged between frame pacings: A {a:?} B {b:?}"
    );
    // The platform rose 10 steps of 1/60 m and carried the box with it.
    assert!(a.y > 0.1, "the box was carried as the platform rose: {a:?}");
}
