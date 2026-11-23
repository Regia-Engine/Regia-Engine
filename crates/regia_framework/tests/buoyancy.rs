// SPDX-License-Identifier: MPL-2.0
//! Floating bodies with rapier (spec 87 § 5.2, ADR-1828): a sphere of half the water's density
//! settles with its center on the level, a floater in a stream drifts downstream, and two runs
//! of the same steps end bit-identical.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_framework::buoyancy::BuoyancyPlugin;
use regia_physics::{Collider, ColliderShape, ExternalForce, Mass, PhysicsPlugin, PhysicsPose, RigidBody};
use regia_scene::Transform;
use regia_terrain::water_body::float::{FRESH_WATER_DENSITY, Floater, FloaterState, sphere_volume};
use regia_terrain::water_body::{RibbonPoint, WaterBody, WaterLook};

const RADIUS: f32 = 0.5;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, BuoyancyPlugin));
    app
}

fn spawn_ball(app: &mut App, at: Vec3, drag: f32) -> Entity {
    let mass = 0.5 * FRESH_WATER_DENSITY * sphere_volume(RADIUS);
    let inertia = 0.4 * mass * RADIUS * RADIUS;
    let mut floater = Floater::sphere(RADIUS);
    floater.linear_drag = drag;
    floater.angular_drag = 50.0;
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Sphere { radius: RADIUS },
                ..Collider::default()
            },
            Mass::Explicit {
                mass,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(inertia),
            },
            floater,
        ))
        .id()
}

fn pose(app: &App, ball: Entity) -> Vec3 {
    app.world()
        .get::<PhysicsPose>(ball)
        .map(|p| p.translation)
        .expect("the ball has been stepped")
}

/// Drops the ball onto a lake at y = 2 and runs 10 s; returns the final pose and state.
fn settle() -> (Vec3, FloaterState) {
    let mut app = app();
    app.world_mut()
        .spawn(WaterBody::plane([0.0, 2.0, 0.0], [50.0, 50.0], WaterLook::lake()));
    let ball = spawn_ball(&mut app, Vec3::new(3.0, 3.5, -1.0), 1500.0);
    for _ in 0..600 {
        app.update();
    }
    let state = *app.world().get::<FloaterState>(ball).expect("floater state");
    (pose(&app, ball), state)
}

#[test]
fn half_density_ball_floats_on_the_level() {
    let (at, state) = settle();
    assert!((at.y - 2.0).abs() < 0.01, "center at {at}");
    assert!(
        (at.x - 3.0).abs() < 1.0e-3 && (at.z + 1.0).abs() < 1.0e-3,
        "no drift on still water: {at}"
    );
    assert!((state.forces.submerged_fraction - 0.5).abs() < 0.02, "{state:?}");
    assert_eq!(state.forces.wet_points, 1);
}

#[test]
fn same_steps_same_bits() {
    let (a, sa) = settle();
    let (b, sb) = settle();
    assert_eq!(a.to_array().map(f32::to_bits), b.to_array().map(f32::to_bits));
    assert_eq!(sa, sb);
}

/// Spec 87 § 5.2: the bridge writes `ExternalForce` only while the body is in the water and
/// removes the force it wrote when the body leaves it (here: the lake is despawned).
#[test]
fn the_force_is_removed_when_the_body_leaves_the_water() {
    let mut app = app();
    let lake = app
        .world_mut()
        .spawn(WaterBody::plane([0.0, 2.0, 0.0], [50.0, 50.0], WaterLook::lake()))
        .id();
    let ball = spawn_ball(&mut app, Vec3::new(0.0, 8.0, 0.0), 1500.0);
    for _ in 0..5 {
        app.update();
    }
    assert!(
        app.world().get::<ExternalForce>(ball).is_none(),
        "a falling, dry ball gets no force from the bridge"
    );
    for _ in 0..600 {
        app.update();
    }
    let wet = app
        .world()
        .get::<ExternalForce>(ball)
        .cloned()
        .expect("afloat: the bridge writes the force");
    assert!(wet.force.y > 0.0, "{wet:?}");
    app.world_mut().despawn(lake);
    app.update();
    assert!(
        app.world().get::<ExternalForce>(ball).is_none(),
        "out of the water: the bridge removed its force"
    );
    let state = *app.world().get::<FloaterState>(ball).expect("floater state");
    assert_eq!(state.forces.wet_points, 0, "{state:?}");
    let before = pose(&app, ball).y;
    for _ in 0..30 {
        app.update();
    }
    // Half a second of free fall drops about 1.2 m; no stale buoyancy holds it up.
    assert!(
        pose(&app, ball).y < before - 0.8,
        "{} -> {}",
        before,
        pose(&app, ball).y
    );
}

/// A force a host puts on a floater that stays dry is not the bridge's: it is left alone.
#[test]
fn a_host_force_on_a_dry_floater_is_kept() {
    let mut app = app();
    let ball = spawn_ball(&mut app, Vec3::new(0.0, 4.0, 0.0), 0.0);
    let host = ExternalForce {
        force: Vec3::new(0.0, 1.0, 0.0),
        torque: Vec3::ZERO,
    };
    app.world_mut().entity_mut(ball).insert(host.clone());
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(app.world().get::<ExternalForce>(ball), Some(&host));
}

#[test]
fn stream_carries_a_floater_downstream() {
    let mut app = app();
    let points = (0..2)
        .map(|i| RibbonPoint {
            position: [-20.0 + 60.0 * i as f32, 0.0, 0.0],
            width: 8.0,
            depth: 1.5,
            speed: Some(1.2),
        })
        .collect();
    app.world_mut()
        .spawn(WaterBody::ribbon(points, WaterLook::clear_stream()));
    let ball = spawn_ball(&mut app, Vec3::new(0.0, 0.0, 0.0), 600.0);
    for _ in 0..300 {
        app.update();
    }
    let at = pose(&app, ball);
    // Five seconds at up to 1.2 m/s: well downstream, still near the centerline and afloat.
    assert!(at.x > 2.0 && at.x < 6.5, "{at}");
    assert!(at.z.abs() < 0.5, "{at}");
    assert!(at.y.abs() < 0.2, "{at}");
}
