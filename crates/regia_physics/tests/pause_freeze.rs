// SPDX-License-Identifier: MPL-2.0
//! The gameplay pause freezes physics (ADR-10105): with `Time::paused` set, the fixed loop takes
//! zero steps, no body moves (the interpolated `Transform` included: the blend factor is the
//! banked overstep, which a paused frame does not change), velocities and joints hold, and
//! unpausing continues the simulation exactly where it stopped: the run is bit-identical to one
//! that never paused, compared at the same number of unpaused frames.
//!
//! The step rate (50 Hz) differs from the frame rate (60 Hz), so the render blend is never a
//! whole step and a moving interpolation would show.

use std::time::Duration;

use regia_app::{App, FixedTime, Time, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_physics::{
    Collider, ColliderShape, Joint, JointKind, PhysicsPlugin, PhysicsWorld, RigidBody, Velocity,
};
use regia_scene::Transform;

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 50.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

/// A floor, a falling spinning box and a two-link pendulum swinging from a static anchor.
fn scene(app: &mut App) -> Vec<Entity> {
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(20.0, 0.5, 20.0),
            },
            ..Collider::default()
        },
    ));
    let faller = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 8.0, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.4),
                },
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(1.5, 0.0, -0.5),
                angular: Vec3::new(0.0, 3.0, 1.0),
            },
        ))
        .id();
    let anchor = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(6.0, 6.0, 0.0)),
            RigidBody::static_body(),
            Collider::default(),
        ))
        .id();
    let mut bodies = vec![faller];
    let mut parent = anchor;
    for i in 0..2 {
        let link = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(6.0, 5.0 - i as f32, 0.0)),
                RigidBody::dynamic(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::splat(0.2),
                    },
                    ..Collider::default()
                },
                Velocity {
                    linear: Vec3::new(2.0, 0.0, 1.0),
                    angular: Vec3::ZERO,
                },
            ))
            .id();
        app.world_mut().entity_mut(parent).insert(Joint {
            kind: JointKind::Spherical { cone_limit: None },
            connected: link,
            anchor: Vec3::new(0.0, -0.5, 0.0),
            connected_anchor: Vec3::new(0.0, 0.5, 0.0),
            break_force: 0.0,
        });
        bodies.push(link);
        parent = link;
    }
    bodies
}

/// Every body's rendered pose and velocity, as bits, plus the world digest.
fn snapshot(app: &App, bodies: &[Entity]) -> (Vec<[u32; 13]>, u64) {
    let world = app.world();
    let poses = bodies
        .iter()
        .map(|&body| {
            let t = world.get::<Transform>(body).expect("transform");
            let v = world.get::<Velocity>(body).cloned().unwrap_or_default();
            let f = [
                t.translation.x,
                t.translation.y,
                t.translation.z,
                t.rotation.x,
                t.rotation.y,
                t.rotation.z,
                t.rotation.w,
                v.linear.x,
                v.linear.y,
                v.linear.z,
                v.angular.x,
                v.angular.y,
                v.angular.z,
            ];
            f.map(f32::to_bits)
        })
        .collect();
    (poses, world.resource::<PhysicsWorld>().state_digest())
}

#[test]
fn a_paused_game_takes_no_physics_step_and_resumes_bit_identically() {
    const BEFORE: usize = 37;
    const PAUSED: usize = 300;
    const AFTER: usize = 45;

    let mut paused_run = physics_app();
    let bodies = scene(&mut paused_run);
    for _ in 0..BEFORE {
        paused_run.update();
    }
    let moving = snapshot(&paused_run, &bodies);
    paused_run.update();
    assert_ne!(
        snapshot(&paused_run, &bodies),
        moving,
        "the scene moves before the pause"
    );

    paused_run.world_mut().resource_mut::<Time>().paused = true;
    paused_run.update();
    let held = snapshot(&paused_run, &bodies);
    let overstep = paused_run.world().resource::<FixedTime>().overstep_fraction();
    for frame in 1..=PAUSED {
        paused_run.update();
        assert_eq!(
            snapshot(&paused_run, &bodies),
            held,
            "paused frame {frame}: something moved"
        );
        assert_eq!(
            paused_run.world().resource::<FixedTime>().overstep_fraction(),
            overstep,
            "paused frame {frame}: the interpolation blend moved"
        );
    }

    paused_run.world_mut().resource_mut::<Time>().paused = false;
    for _ in 0..AFTER {
        paused_run.update();
    }

    // Control: the same unpaused frames, never paused.
    let mut control = physics_app();
    let control_bodies = scene(&mut control);
    for _ in 0..BEFORE + 1 + AFTER {
        control.update();
    }
    assert_eq!(
        snapshot(&paused_run, &bodies),
        snapshot(&control, &control_bodies),
        "after the pause the simulation is where a never-paused run is"
    );
}
