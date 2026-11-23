// SPDX-License-Identifier: MPL-2.0
//! J24 water acceptance (v0.5 burn-down C11): floating bodies on a moving swell, with the shipped
//! water clock (`regia_terrain::WaterPlugin`) and the physics bridge ([`BuoyancyPlugin`]).
//! `buoyancy.rs` covers still water and a stream; here the surface moves every step:
//!
//! - A half-density ball rides a single long wave: its forces are taken at the step's own water
//!   tick, its center heaves with the surface query at its position (the same query the renderer's
//!   vertices match, `water_flight.rs`), and it stays near where it was dropped.
//! - After a floating-origin rebase (spec 78 § 12.2) the same ball, spawned at the same place in
//!   local coordinates, rides the same swell.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_framework::buoyancy::BuoyancyPlugin;
use regia_physics::{Collider, ColliderShape, Mass, PhysicsPlugin, PhysicsPose, RigidBody};
use regia_scene::Transform;
use regia_scene::streaming::OriginOffset;
use regia_terrain::WaterPlugin;
use regia_terrain::water_body::float::{FRESH_WATER_DENSITY, Floater, FloaterState, sphere_volume};
use regia_terrain::water_body::{Swell, SwellWave, WaterBody, WaterClock, WaterLook};

const RADIUS: f32 = 0.5;
const LEVEL: f32 = 2.0;
/// A rebase by whole 1024 m cells and 64 m up (exact in `f32` and `f64`): the floater's forces go
/// through `WaterQuery`'s local conversion on every axis.
const SHIFT: [f64; 3] = [1024.0, 64.0, -1024.0];
/// Where the ball is dropped (world).
const DROP: [f32; 3] = [3.0, LEVEL + 0.3, -1.0];
/// Steps before the ride is measured (the drop settles).
const SETTLE: usize = 180;
/// Steps measured (6 s, about 1.7 wave periods).
const RIDE: usize = 360;

fn app(rebase: bool) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, WaterPlugin, BuoyancyPlugin));
    if rebase {
        app.insert_resource(OriginOffset(SHIFT));
    }
    app
}

/// A 1 m long-crested wave, 20 m long (a period of about 3.6 s), on a 50 m lake at `LEVEL`.
fn swell_lake() -> WaterBody {
    let mut lake = WaterBody::plane([0.0, LEVEL, 0.0], [50.0, 50.0], WaterLook::lake());
    lake.swell = Some(Swell {
        waves: vec![SwellWave {
            wavelength: 20.0,
            height: 1.0,
            direction: [1.0, 0.0],
            steepness: 0.4,
            phase: 0.0,
        }],
        ..Swell::default()
    });
    lake
}

fn spawn_ball(app: &mut App, at: Vec3) -> Entity {
    let mass = 0.5 * FRESH_WATER_DENSITY * sphere_volume(RADIUS);
    let inertia = 0.4 * mass * RADIUS * RADIUS;
    let mut floater = Floater::sphere(RADIUS);
    floater.linear_drag = 600.0;
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

/// One measured step: the ball's center and the surface height under it (both in world space).
#[derive(Debug, Clone, Copy)]
struct Step {
    center: Vec3,
    surface: f32,
}

/// Drops the ball on the swell and records the ride, in world coordinates.
fn ride(rebase: bool) -> Vec<Step> {
    let mut app = app(rebase);
    let lake = swell_lake();
    app.world_mut().spawn(lake.clone());
    let shift = if rebase {
        Vec3::new(SHIFT[0] as f32, SHIFT[1] as f32, SHIFT[2] as f32)
    } else {
        Vec3::ZERO
    };
    let ball = spawn_ball(&mut app, Vec3::from_array(DROP) - shift);
    let mut steps = Vec::with_capacity(RIDE);
    for i in 0..SETTLE + RIDE {
        app.update();
        let clock = *app.world().resource::<WaterClock>();
        // The first update may run no fixed step yet (the clock starts at zero elapsed time).
        let Some(state) = app.world().get::<FloaterState>(ball).copied() else {
            assert!(i < 2, "no floater state after {} updates", i + 1);
            continue;
        };
        assert_eq!(
            state.tick, clock.tick,
            "step {i}: the forces were taken at another water tick"
        );
        if i < SETTLE {
            continue;
        }
        let center = pose(&app, ball) + shift;
        let surface = lake
            .surface(center.x, center.z, &clock)
            .expect("the ball stays over the lake")
            .height;
        steps.push(Step { center, surface });
    }
    assert!(
        app.world().resource::<WaterClock>().tick >= (SETTLE + RIDE) as u64,
        "the water clock did not run"
    );
    steps
}

fn range(values: impl Iterator<Item = f32>) -> f32 {
    let (lo, hi) = values.fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    hi - lo
}

/// The ball heaves with the swell: its center follows the surface query under it (a half-density
/// ball floats with its center on the surface), it moves about as far as the surface does, and it
/// does not wander off.
#[test]
fn a_floater_rides_the_swell_it_is_queried_on() {
    let steps = ride(false);
    let surface_range = range(steps.iter().map(|s| s.surface));
    let heave = range(steps.iter().map(|s| s.center.y));
    assert!(
        surface_range > 0.8,
        "the swell under the ball is flat: {surface_range}"
    );
    assert!(
        heave > 0.7 * surface_range && heave < 1.4 * surface_range,
        "heave {heave} m on a surface moving {surface_range} m"
    );
    let errors: Vec<f32> = steps.iter().map(|s| (s.center.y - s.surface).abs()).collect();
    let mean = errors.iter().sum::<f32>() / errors.len() as f32;
    let worst = errors.iter().copied().fold(0.0f32, f32::max);
    eprintln!(
        "ride: surface range {surface_range:.3} m, heave {heave:.3} m, |center - surface| mean {mean:.3} m, worst {worst:.3} m"
    );
    assert!(mean < 0.15, "the ball lags the surface: mean {mean} m");
    assert!(worst < 0.3, "the ball left the surface: worst {worst} m");
    for s in &steps {
        let drift = ((s.center.x - DROP[0]).powi(2) + (s.center.z - DROP[2]).powi(2)).sqrt();
        assert!(drift < 3.0, "the ball wandered {drift} m: {s:?}");
    }
}

/// A rebase keeps the ride: the ball dropped at the same world place in local coordinates rides
/// the same swell (the queries speak local coordinates, the bodies stay world-anchored).
#[test]
fn a_rebased_floater_rides_the_same_swell() {
    let world = ride(false);
    let rebased = ride(true);
    let mut worst = 0.0f32;
    for (a, b) in world.iter().zip(&rebased) {
        worst = worst.max((a.center - b.center).abs().max_element());
        assert!(
            (a.surface - b.surface).abs() < 1.0e-3,
            "the surface under the ball moved: {a:?} vs {b:?}"
        );
    }
    eprintln!("rebased ride: worst center difference {worst:.4} m");
    assert!(worst < 0.02, "the rebased ball rode differently: {worst} m");
}
