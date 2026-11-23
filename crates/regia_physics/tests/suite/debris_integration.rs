// SPDX-License-Identifier: MPL-2.0
//! Integration tests for M14 cosmetic debris physics.
//!
//! These tests exercise the full physics pipeline: heightfield ground creation,
//! debris spawn, simulation stepping, budget enforcement, lifetime despawn,
//! and the no-feedback-to-sim contract.
//!
//! Provenance: debris physics tests use rapier3d public API (Apache-2.0),
//! rapier.rs/docs.

use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::{
    BodyBudget, BodyKind, BodyMarker, ChunkDesc, ColliderShape, DebrisTag, PhysicsPlugin, PhysicsSettings,
    PhysicsWorld,
};
use regia_scene::Transform;

// ---------------------------------------------------------------------------
// Helpers for pure-physics (non-ECS) tests
// ---------------------------------------------------------------------------

/// Creates a small heightfield ground plane (3×3 samples, z=0 everywhere).
fn make_flat_heightfield() -> ColliderShape {
    let nx = 3usize;
    let nz = 3usize;
    let heights: Vec<f32> = (0..nx * nz).map(|_| 0.0f32).collect();
    ColliderShape::HeightField {
        nx,
        nz,
        heights,
        scale: Vec3::new(10.0, 1.0, 10.0),
    }
}

/// Inserts a static body with the given shape into the physics world.
fn insert_static_body(pw: &mut PhysicsWorld, shape: &ColliderShape) {
    use rapier3d::dynamics::{RigidBodyBuilder, RigidBodyType};
    use rapier3d::geometry::ColliderBuilder;
    use regia_physics::convert::{to_rapier_angvec, to_rapier_vec3};

    let body = RigidBodyBuilder::new(RigidBodyType::Fixed)
        .translation(to_rapier_vec3(Vec3::new(0.0, -5.0, 0.0)))
        .rotation(to_rapier_angvec(Quat::IDENTITY))
        .build();
    let body_handle = pw.bodies.insert(body);

    let shared =
        regia_physics::step::shape_to_shared_shape(shape).expect("heightfield shape should be valid");
    let collider = ColliderBuilder::new(shared).build();
    let PhysicsWorld {
        ref mut bodies,
        ref mut colliders,
        ..
    } = *pw;
    colliders.insert_with_parent(collider, body_handle, bodies);
}

/// Steps the physics simulation by `dt_secs` at the given fixed rate.
fn step(pw: &mut PhysicsWorld, dt_secs: f64, fixed_hz: f64, gravity: Vec3) {
    use rapier3d::dynamics::IntegrationParameters;

    pw.accumulator += dt_secs;
    let step_dt = 1.0 / fixed_hz.max(1.0);

    let integration_params = IntegrationParameters {
        dt: step_dt as f32,
        ..Default::default()
    };

    let gravity_vec: rapier3d::math::Vector = rapier3d::math::Vector::new(gravity.x, gravity.y, gravity.z);

    let max_steps = 20u32;
    let mut stepped = 0u32;
    while pw.accumulator >= step_dt && stepped < max_steps {
        let PhysicsWorld {
            pipeline,
            islands,
            broad_phase,
            narrow_phase,
            bodies,
            colliders,
            impulse_joints,
            multibody_joints,
            soft_bodies,
            ccd_solver,
            accumulator,
            ..
        } = &mut *pw;

        pipeline.step(
            gravity_vec,
            &integration_params,
            islands,
            broad_phase,
            narrow_phase,
            bodies,
            colliders,
            impulse_joints,
            multibody_joints,
            soft_bodies,
            ccd_solver,
            &(),
            &(),
        );

        *accumulator -= step_dt;
        stepped += 1;
    }
    pw.accumulator = pw.accumulator.min(step_dt * max_steps as f64);
}

/// Creates a pre-fractured mesh with box chunks arranged in a ring.
fn make_chunks(count: usize, radius: f32, _height: f32) -> Vec<ChunkDesc> {
    (0..count)
        .map(|i| {
            let angle = i as f32 * std::f32::consts::TAU / count as f32;
            ChunkDesc {
                local_translation: Vec3::new(radius * angle.cos(), 0.0, radius * angle.sin()),
                local_rotation: Quat::IDENTITY,
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.3, 0.3, 0.3),
                },
                mass: 1.0,
            }
        })
        .collect()
}

/// Spawns debris chunks into the physics world and returns their body handles.
fn spawn_chunks(
    pw: &mut PhysicsWorld,
    chunks: &[ChunkDesc],
    origin: Vec3,
    impulse_strength: f32,
    lifetime_secs: f64,
) -> Vec<(
    rapier3d::dynamics::RigidBodyHandle,
    rapier3d::geometry::ColliderHandle,
    f64,
)> {
    use rapier3d::dynamics::{RigidBodyBuilder, RigidBodyType};
    use rapier3d::geometry::ColliderBuilder;
    use regia_physics::convert::to_rapier_vec3;

    let mut handles = Vec::new();

    for chunk in chunks {
        let world_pos = origin + chunk.local_translation;
        let world_rot = chunk.local_rotation;

        let dir = (world_pos - origin).normalize_or_zero();
        let mass = if chunk.mass > 0.0 { chunk.mass } else { 1.0 };
        let speed = impulse_strength + (mass.max(0.1)).recip().min(impulse_strength);
        let impulse = dir * speed;

        let angvec = regia_physics::convert::to_rapier_angvec(world_rot);
        let body = RigidBodyBuilder::new(RigidBodyType::Dynamic)
            .translation(to_rapier_vec3(world_pos))
            .rotation(angvec)
            .linvel(to_rapier_vec3(impulse))
            .can_sleep(true)
            .sleeping(false)
            .additional_mass(mass)
            .build();
        let body_handle = pw.bodies.insert(body);

        let shared = regia_physics::step::shape_to_shared_shape(&chunk.shape).expect("valid box shape");
        let collider = ColliderBuilder::new(shared).build();
        let PhysicsWorld {
            ref mut bodies,
            ref mut colliders,
            ..
        } = *pw;
        let collider_handle = colliders.insert_with_parent(collider, body_handle, bodies);

        handles.push((body_handle, collider_handle, lifetime_secs));
    }

    handles
}

/// Returns the Y position of a body. None if the body doesn't exist.
fn body_y(pw: &PhysicsWorld, handle: rapier3d::dynamics::RigidBodyHandle) -> Option<f32> {
    pw.bodies.get(handle).map(|b| b.position().translation.y)
}

/// Returns true if a body is sleeping.
fn body_is_sleeping(pw: &PhysicsWorld, handle: rapier3d::dynamics::RigidBodyHandle) -> Option<bool> {
    pw.bodies.get(handle).map(|b| b.is_sleeping())
}

// ---------------------------------------------------------------------------
// Helpers for App-level (ECS-aware) tests
// ---------------------------------------------------------------------------

/// Phase 1: insert a rapier dynamic body at `translation`, return raw handle bits.
fn insert_dynamic_body(app: &mut App, translation: Vec3) -> u32 {
    use rapier3d::dynamics::{RigidBodyBuilder, RigidBodyType};
    use regia_physics::convert::to_rapier_vec3;

    let mut pw = app.world_mut().resource_mut::<PhysicsWorld>();
    let body = RigidBodyBuilder::new(RigidBodyType::Dynamic)
        .translation(to_rapier_vec3(translation))
        .build();
    pw.bodies.insert(body).into_raw_parts().0
}

/// Phase 2: spawn an ECS debris entity with the given rapier body handle and components.
fn spawn_ecs_debris(
    app: &mut App,
    body_raw: u32,
    translation: Vec3,
    lifetime_secs: f64,
    spawn_time_secs: Option<f64>,
) -> Entity {
    app.world_mut()
        .spawn((
            BodyMarker::from_raw(body_raw),
            Transform {
                translation,
                ..Default::default()
            },
            BodyKind::Dynamic,
            ColliderShape::Box {
                half_extents: Vec3::new(0.2, 0.2, 0.2),
            },
            DebrisTag {
                spawn_time_secs,
                lifetime_secs,
            },
        ))
        .id()
}

// ---------------------------------------------------------------------------
// Pure-physics tests (no ECS App needed)
// ---------------------------------------------------------------------------

#[test]
fn heightfield_shape_round_trip() {
    let hf = make_flat_heightfield();
    let shared = regia_physics::step::shape_to_shared_shape(&hf);
    assert!(shared.is_some(), "heightfield should convert");
    assert!(matches!(
        shared.expect("heightfield should convert").as_typed_shape(),
        rapier3d::geometry::TypedShape::HeightField(_)
    ));
}

#[test]
fn heightfield_rejects_invalid_dimensions() {
    // nx < 2
    let hf = ColliderShape::HeightField {
        nx: 1,
        nz: 3,
        heights: vec![0.0; 3],
        scale: Vec3::ONE,
    };
    assert!(regia_physics::step::shape_to_shared_shape(&hf).is_none());

    // Wrong data length
    let hf = ColliderShape::HeightField {
        nx: 3,
        nz: 3,
        heights: vec![0.0; 5], // 3*3 = 9, but only 5 elements
        scale: Vec3::ONE,
    };
    assert!(regia_physics::step::shape_to_shared_shape(&hf).is_none());
}

#[test]
fn chunks_fall_and_come_to_rest_on_heightfield() {
    let mut pw = PhysicsWorld::default();
    let hf = make_flat_heightfield();
    insert_static_body(&mut pw, &hf);

    let chunks = make_chunks(8, 1.0, 5.0);
    let origin = Vec3::new(0.0, 5.0, 0.0);
    let handles = spawn_chunks(&mut pw, &chunks, origin, 2.0, 10.0);

    assert_eq!(handles.len(), 8, "all 8 chunks should be spawned");
    assert_eq!(pw.bodies.len(), 9, "8 dynamic + 1 static body");

    for &(bh, _, _) in &handles {
        let y = body_y(&pw, bh).expect("body should exist");
        assert!(y > 4.0, "chunk should start above ground: y={y}");
    }

    let gravity = Vec3::new(0.0, -9.81, 0.0);
    for _ in 0..180 {
        step(&mut pw, 1.0 / 60.0, 60.0, gravity);
    }

    for &(bh, _, _) in &handles {
        let y = body_y(&pw, bh).expect("body should still exist");
        assert!(y < 3.0, "chunk should have fallen toward ground: y={y}");
    }

    for _ in 0..300 {
        step(&mut pw, 1.0 / 60.0, 60.0, gravity);
    }

    let mut sleeping_count = 0usize;
    for &(bh, _, _) in &handles {
        if body_is_sleeping(&pw, bh).unwrap_or(false) {
            sleeping_count += 1;
        }
    }
    assert!(
        sleeping_count > 0,
        "at least some chunks should be sleeping: {sleeping_count}/{}",
        handles.len()
    );
}

#[test]
fn multiple_dynamic_bodies_collide_with_heightfield() {
    let mut pw = PhysicsWorld::default();
    let hf = make_flat_heightfield();
    insert_static_body(&mut pw, &hf);

    let chunks = make_chunks(16, 2.0, 5.0);
    let origin = Vec3::new(0.0, 5.0, 0.0);
    let handles = spawn_chunks(&mut pw, &chunks, origin, 3.0, 20.0);

    assert_eq!(handles.len(), 16);

    let gravity = Vec3::new(0.0, -9.81, 0.0);

    for &(bh, _, _) in &handles {
        assert!(body_y(&pw, bh).unwrap_or(-999.0) > 3.0);
    }

    for _ in 0..300 {
        step(&mut pw, 1.0 / 60.0, 60.0, gravity);
    }

    for &(bh, _, _) in &handles {
        let y = body_y(&pw, bh).expect("body should still exist");
        assert!(y < 6.0, "body y={y} should be below start height 5");
    }
}

#[test]
fn heightfield_with_varying_heights() {
    let mut pw = PhysicsWorld::default();

    let nx = 5usize;
    let nz = 3usize;
    let heights: Vec<f32> = (0..nx * nz)
        .map(|idx| {
            let i = idx % nx;
            i as f32 * 0.5
        })
        .collect();
    let hf = ColliderShape::HeightField {
        nx,
        nz,
        heights,
        scale: Vec3::new(2.0, 1.0, 2.0),
    };

    let shared = regia_physics::step::shape_to_shared_shape(&hf);
    assert!(shared.is_some(), "non-uniform heightfield should convert");
    assert!(matches!(
        shared.expect("heightfield should convert").as_typed_shape(),
        rapier3d::geometry::TypedShape::HeightField(_)
    ));

    insert_static_body(&mut pw, &hf);

    let chunk = ChunkDesc {
        local_translation: Vec3::ZERO,
        local_rotation: Quat::IDENTITY,
        shape: ColliderShape::Box {
            half_extents: Vec3::new(0.2, 0.2, 0.2),
        },
        mass: 0.5,
    };
    let handles = spawn_chunks(&mut pw, &[chunk], Vec3::new(0.0, 3.0, 0.0), 0.5, 10.0);
    assert_eq!(handles.len(), 1);

    let gravity = Vec3::new(0.0, -9.81, 0.0);
    for _ in 0..120 {
        step(&mut pw, 1.0 / 60.0, 60.0, gravity);
    }

    let y = body_y(&pw, handles[0].0).expect("body exists");
    assert!(y < 3.0, "body y={y} should be below start height");
}

// ---------------------------------------------------------------------------
// App-level tests (ECS + PhysicsPlugin)
// ---------------------------------------------------------------------------

#[test]
fn debris_budget_enforced_during_spawn() {
    // With max_debris_bodies = N, spawning more than N leaves exactly N,
    // and the oldest (first spawned) entities are evicted first.
    const MAX: u32 = 3;
    const SPAWN_COUNT: u32 = 8;

    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.add_plugins(PhysicsPlugin);
    // Override after the plugin to prevent the default from clobbering our value.
    app.insert_resource(PhysicsSettings {
        max_debris_bodies: MAX,
        ..Default::default()
    });

    // Spawn entities tracking insertion order in budget.
    let mut spawned_entities: Vec<Entity> = Vec::new();

    for i in 0..SPAWN_COUNT {
        let translation = Vec3::new(i as f32, 1.0, 0.0);
        let body_raw = insert_dynamic_body(&mut app, translation);
        let entity = spawn_ecs_debris(&mut app, body_raw, translation, 60.0, Some(0.0));
        app.world_mut().resource_mut::<BodyBudget>().push(entity);
        spawned_entities.push(entity);
    }

    assert_eq!(spawned_entities.len(), SPAWN_COUNT as usize);
    assert_eq!(app.world().resource::<BodyBudget>().len(), SPAWN_COUNT as usize);

    // Run one update — enforce_body_budget should evict oldest entities.
    app.update();

    let world = app.world();
    let budget = world.resource::<BodyBudget>();
    assert_eq!(
        budget.len(),
        MAX as usize,
        "budget should contain exactly {MAX} entities after enforcement"
    );

    // The first [0..(SPAWN_COUNT-MAX)] entities should be gone (oldest evicted first).
    for (i, &e) in spawned_entities.iter().enumerate() {
        let exists = world.get::<DebrisTag>(e).is_some();
        if i < (SPAWN_COUNT - MAX) as usize {
            assert!(!exists, "entity {i} (oldest) should be evicted");
        } else {
            assert!(exists, "entity {i} should survive budget enforcement");
        }
    }

    // Rapier bodies should also be cleaned up.
    let pw = world.resource::<PhysicsWorld>();
    assert_eq!(
        pw.bodies.len(),
        MAX as usize,
        "only {MAX} rapier bodies should remain"
    );
}

#[test]
fn debris_despawns_after_lifetime() {
    // Debris entities AND their rapier bodies are gone after lifetime_secs,
    // and still present just before it.
    let step = Duration::from_secs_f64(1.0 / 60.0);
    let lifetime = 1.0_f64; // 60 frames at 60 Hz

    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    app.add_plugins(PhysicsPlugin);

    // Spawn 4 debris entities with spawn_time_secs = None.
    // init_debris_spawn_times will record the spawn time as elapsed during
    // the first PostUpdate after spawn (1/60). Despawn triggers when:
    //   age = elapsed - spawn_time > lifetime
    //       = elapsed - 1/60        > 1.0
    //   →   elapsed                  > 61/60
    // 61 updates: age = 60/60 = 1.0, not expired.  62 updates: age > 1.0, expired.
    let mut entity_ids: Vec<Entity> = Vec::new();
    for i in 0..4 {
        let translation = Vec3::new(i as f32 * 0.5, 1.0, 0.0);
        let body_raw = insert_dynamic_body(&mut app, translation);
        let entity = spawn_ecs_debris(&mut app, body_raw, translation, lifetime, None);
        app.world_mut().resource_mut::<BodyBudget>().push(entity);
        entity_ids.push(entity);
    }

    assert_eq!(entity_ids.len(), 4);
    assert_eq!(app.world().resource::<PhysicsWorld>().bodies.len(), 4);

    // Advance to just before lifetime expiry (61 updates → not expired).
    for _ in 0..61 {
        app.update();
    }

    let elapsed_before = app.world().resource::<regia_app::Time>().elapsed_secs() as f64;
    let spawn_time = app
        .world()
        .get::<DebrisTag>(entity_ids[0])
        .and_then(|t| t.spawn_time_secs)
        .unwrap_or(0.0);
    assert!(
        elapsed_before - spawn_time <= lifetime,
        "age {:.6} should be <= lifetime {lifetime} (elapsed {elapsed_before}, spawn {spawn_time})",
        elapsed_before - spawn_time
    );

    for &e in &entity_ids {
        assert!(
            app.world().get::<DebrisTag>(e).is_some(),
            "debris entity should still exist just before lifetime"
        );
    }
    assert_eq!(
        app.world().resource::<PhysicsWorld>().bodies.len(),
        4,
        "rapier bodies should still exist just before lifetime"
    );

    // Advance past lifetime (62nd and 63rd updates → expired).
    app.update();
    app.update();

    let elapsed_after = app.world().resource::<regia_app::Time>().elapsed_secs() as f64;
    assert!(
        elapsed_after - spawn_time > lifetime,
        "age {:.6} should be > lifetime {lifetime} (elapsed {elapsed_after}, spawn {spawn_time})",
        elapsed_after - spawn_time
    );

    for &e in &entity_ids {
        assert!(
            app.world().get::<DebrisTag>(e).is_none(),
            "debris entity should be despawned after lifetime"
        );
    }
    assert_eq!(
        app.world().resource::<PhysicsWorld>().bodies.len(),
        0,
        "rapier bodies should be removed after lifetime"
    );
    assert!(
        app.world().resource::<BodyBudget>().is_empty(),
        "budget should be empty after all debris despawned"
    );
}

/// The physics step on which the debris left the solver, for frames of `frame` length at the
/// default 60 Hz fixed step.
fn debris_expiry_step(frame: Duration) -> u64 {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(frame));
    app.add_plugins(PhysicsPlugin);
    for i in 0..2 {
        let translation = Vec3::new(i as f32, 1.0, 0.0);
        let body_raw = insert_dynamic_body(&mut app, translation);
        let entity = spawn_ecs_debris(&mut app, body_raw, translation, 1.0, None);
        app.world_mut().resource_mut::<BodyBudget>().push(entity);
    }
    for _ in 0..10_000 {
        app.update();
        let pw = app.world().resource::<PhysicsWorld>();
        if pw.bodies.is_empty() {
            return pw.stats().steps;
        }
    }
    panic!("the debris never expired");
}

#[test]
fn debris_expires_on_the_same_physics_step_at_any_frame_rate() {
    // The lifetime ran on the frame clock: at 120 Hz frames the chunks left one step earlier than
    // at 60 Hz, so a replay at another frame rate diverged from that step on (ADR-1660).
    let at_60 = debris_expiry_step(Duration::from_secs_f64(1.0 / 60.0));
    let at_120 = debris_expiry_step(Duration::from_secs_f64(1.0 / 120.0));
    assert_eq!(
        at_60, 62,
        "one second of lifetime, counted from the end of the first step"
    );
    assert_eq!(at_120, at_60);
}

#[test]
fn no_feedback_path_to_any_simulation() {
    // Cosmetic debris must never affect the game simulation.
    let tag = regia_physics::DebrisTag {
        spawn_time_secs: None,
        lifetime_secs: 5.0,
    };
    assert!(tag.spawn_time_secs.is_none());
    assert_eq!(tag.lifetime_secs, 5.0);

    let pw = PhysicsWorld::default();
    assert_eq!(pw.bodies.len(), 0);
}
