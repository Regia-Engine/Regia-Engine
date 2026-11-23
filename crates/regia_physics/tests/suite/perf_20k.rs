// SPDX-License-Identifier: MPL-2.0
//! Physics at 20,000 bodies (ADR-870): a measurement, not a budget check.
//!
//! Ignored in normal runs (it takes tens of seconds and its numbers depend on the machine).
//! Run it in release on a quiet box and read the printed table:
//!
//! ```text
//! cargo test --release -p regia_physics --test suite perf_20k -- --ignored --nocapture
//! ```
//!
//! Phases: 20,000 boxes spawned in a 40 × 50 × 10 grid fall onto a floor and pile up
//! (active), settle until they sleep (asleep), then all despawn at once. For each phase it
//! prints the mean and worst frame time (the whole fixed-step chain: sync, rapier step,
//! write-back, interpolation) and rapier's own step time (`PhysicsStats::last_step_ms`).
//! The functional checks are counters only: every body exists, nothing is NaN, everything
//! despawns, and most of the pile sleeps once settled.
//!
//! Provenance: measurement harness is Regia Engine's own; the simulation is rapier3d (Apache-2.0).

use std::time::{Duration, Instant};

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsPose, PhysicsWorld, RigidBody};
use regia_scene::Transform;

const COUNT: usize = 20_000;

/// Mean and worst wall time of `frames` updates, plus the mean rapier step time.
struct Phase {
    name: &'static str,
    mean_ms: f64,
    worst_ms: f64,
    rapier_ms: f64,
}

fn measure(app: &mut App, name: &'static str, frames: usize) -> Phase {
    let mut total = 0.0;
    let mut worst = 0.0f64;
    let mut rapier = 0.0;
    for _ in 0..frames {
        let start = Instant::now();
        app.update();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        total += ms;
        worst = worst.max(ms);
        rapier += app.world().resource::<PhysicsWorld>().stats().last_step_ms;
    }
    Phase {
        name,
        mean_ms: total / frames as f64,
        worst_ms: worst,
        rapier_ms: rapier / frames as f64,
    }
}

#[test]
#[ignore = "perf measurement at 20,000 bodies; run in release with --ignored --nocapture"]
fn perf_step_time_20000_bodies() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);

    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(60.0, 0.5, 60.0),
            },
            ..Collider::default()
        },
    ));
    let half = 0.25f32;
    let mut boxes: Vec<Entity> = Vec::with_capacity(COUNT);
    for layer in 0..10 {
        for i in 0..40 {
            for k in 0..50 {
                let at = Vec3::new(
                    (i as f32 - 20.0) * 0.6,
                    half + 0.05 + layer as f32 * 0.6,
                    (k as f32 - 25.0) * 0.6,
                );
                let id = app
                    .world_mut()
                    .spawn((
                        Transform::from_translation(at),
                        RigidBody::dynamic(),
                        Collider {
                            shape: ColliderShape::Box {
                                half_extents: Vec3::splat(half),
                            },
                            ..Collider::default()
                        },
                    ))
                    .id();
                boxes.push(id);
            }
        }
    }
    assert_eq!(boxes.len(), COUNT);

    let first = measure(&mut app, "first frame (create 20k)", 1);
    assert_eq!(
        app.world().resource::<PhysicsWorld>().bodies.len(),
        COUNT + 1,
        "every body exists"
    );
    let active = measure(&mut app, "active (falling, piling)", 300);
    let finite = boxes.iter().all(|&e| {
        app.world()
            .get::<PhysicsPose>(e)
            .is_some_and(|p| p.translation.is_finite() && p.rotation.is_finite())
    });
    assert!(finite, "no NaN at 20k bodies");

    // Settle: up to 30 s of simulated time, until 95 % of the pile sleeps.
    let mut settle_frames = 0;
    while settle_frames < 30 * 60 {
        app.update();
        settle_frames += 1;
        let s = app.world().resource::<PhysicsWorld>().stats();
        if s.sleeping_dynamic * 20 >= COUNT * 19 {
            break;
        }
    }
    let settled = app.world().resource::<PhysicsWorld>().stats();
    let asleep = measure(&mut app, "settled (mostly asleep)", 120);

    for &e in &boxes {
        app.world_mut().despawn(e);
    }
    let despawn = measure(&mut app, "despawn 20k", 1);
    let after = app.world().resource::<PhysicsWorld>().stats();

    println!("\nregia_physics perf at {COUNT} boxes (half extent {half} m)");
    println!("| phase | frames | mean frame ms | worst frame ms | mean rapier step ms |");
    println!("|---|---|---|---|---|");
    for (phase, frames) in [(&first, 1), (&active, 300), (&asleep, 120), (&despawn, 1)] {
        println!(
            "| {} | {frames} | {:.2} | {:.2} | {:.2} |",
            phase.name, phase.mean_ms, phase.worst_ms, phase.rapier_ms
        );
    }
    println!(
        "settled after {settle_frames} frames: {} asleep, {} awake, {} contact pairs",
        settled.sleeping_dynamic, settled.awake_dynamic, settled.contact_pairs
    );
    println!("after despawn: {after:?}");

    assert_eq!(after.bodies, 1, "only the floor is left after the mass despawn");
    assert_eq!(after.colliders, 1);
    assert!(
        settled.sleeping_dynamic * 2 >= COUNT,
        "most of a settled pile sleeps: {settled:?}"
    );
}
