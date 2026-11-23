// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane: `DebrisSpawner` inputs the unit tests do not reach (non-finite impulse
//! strength, empty chunk lists, invalid shapes, a full budget).

use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::world::CommandQueue;
use regia_ecs::prelude::*;
use regia_physics::{
    BodyBudget, ChunkDesc, ColliderShape, DebrisSpawner, PhysicsSettings, PhysicsWorld, SpawnConfig,
    SpawnResult,
};

fn chunk(x: f32, mass: f32) -> ChunkDesc {
    ChunkDesc {
        local_translation: Vec3::new(x, 0.0, 0.0),
        local_rotation: Quat::IDENTITY,
        shape: ColliderShape::Box {
            half_extents: Vec3::splat(0.2),
        },
        mass,
    }
}

fn spawn(
    chunks: &[ChunkDesc],
    origin: Vec3,
    impulse_strength: f32,
    max_debris_bodies: u32,
    budget: &mut BodyBudget,
) -> (SpawnResult, PhysicsWorld) {
    let mut world = World::new();
    let mut queue = CommandQueue::default();
    let mut physics = PhysicsWorld::default();
    let settings = PhysicsSettings {
        max_debris_bodies,
        ..PhysicsSettings::default()
    };
    let config = SpawnConfig {
        chunks,
        origin,
        impulse_strength,
    };
    let result = {
        let mut commands = Commands::new(&mut queue, &world);
        DebrisSpawner::new(5.0).spawn(&mut commands, &mut physics, budget, &settings, &config)
    };
    queue.apply(&mut world);
    (result, physics)
}

#[test]
fn non_finite_impulse_strength_never_puts_a_non_finite_velocity_into_rapier() {
    for strength in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let chunks = [chunk(1.0, 1.0), chunk(-1.0, 0.0), chunk(2.0, f32::NAN)];
        let mut budget = BodyBudget::default();
        let (result, physics) = spawn(&chunks, Vec3::ZERO, strength, 0, &mut budget);
        // The NaN mass chunk never reaches rapier: it counts as skipped (ADR-1660).
        assert_eq!(result.spawned, 2, "{strength}");
        assert_eq!(result.skipped, 1, "{strength}");
        assert_eq!(physics.bodies.len(), 2, "{strength}");
        for (_, body) in physics.bodies.iter() {
            let v = body.linvel();
            assert!(
                v.x.is_finite() && v.y.is_finite() && v.z.is_finite(),
                "{strength}: {v:?}"
            );
            let p = body.translation();
            assert!(
                p.x.is_finite() && p.y.is_finite() && p.z.is_finite(),
                "{strength}: {p:?}"
            );
        }
    }
}

#[test]
fn impulse_strength_is_an_outward_speed_plus_a_bounded_mass_term() {
    // speed = strength + min(1/max(authored mass, 0.1), strength). Non-positive authored mass
    // becomes additional_mass 1 kg, but the speed term still sees the authored mass.
    let strength = 15.0;
    let cases = [(1.0_f32, 16.0_f32), (10.0, 15.1), (0.0, 25.0), (-5.0, 25.0)];
    let mut masses = Vec::new();
    for (mass, speed) in cases {
        let mut budget = BodyBudget::default();
        let (result, mut physics) = spawn(&[chunk(1.0, mass)], Vec3::ZERO, strength, 0, &mut budget);
        assert_eq!(result.spawned, 1, "mass {mass}");
        let regia_physics::PhysicsWorld {
            ref mut bodies,
            ref colliders,
            ..
        } = physics;
        let (_, body) = bodies.iter_mut().next().expect("body");
        assert!(
            (body.linvel().x - speed).abs() < 1e-4,
            "mass {mass}: {}",
            body.linvel().x
        );
        assert!(body.linvel().y.abs() < 1e-5 && body.linvel().z.abs() < 1e-5);
        // The collider's density mass is added on attach. `additional_mass` joins it when mass
        // properties are recomputed, which the next physics step does.
        body.recompute_mass_properties_from_colliders(colliders);
        masses.push(body.mass());
    }
    // Mass 1, 0 and −5 all pass additional_mass 1 kg, so their simulated masses match. Mass 10
    // is 9 kg more. The collider's default density adds the same volume term to each.
    assert!((masses[0] - masses[2]).abs() < 1e-4, "1 kg and 0: {masses:?}");
    assert!((masses[2] - masses[3]).abs() < 1e-4, "0 and −5: {masses:?}");
    assert!(
        (masses[1] - masses[0] - 9.0).abs() < 1e-3,
        "10 kg is 9 kg heavier: {masses:?}"
    );
}

#[test]
fn a_finite_strength_pushes_chunks_outward_by_their_own_direction() {
    let chunks = [chunk(1.0, 1.0), chunk(-1.0, 1.0)];
    let mut budget = BodyBudget::default();
    let (result, physics) = spawn(&chunks, Vec3::ZERO, 4.0, 0, &mut budget);
    assert_eq!(result.spawned, 2);
    let mut speeds: Vec<f32> = physics.bodies.iter().map(|(_, b)| b.linvel().x).collect();
    speeds.sort_by(f32::total_cmp);
    assert!(speeds[0] < -3.9 && speeds[1] > 3.9, "{speeds:?}");
}

#[test]
fn empty_chunk_lists_and_zero_strength_are_fine() {
    let mut budget = BodyBudget::default();
    let (result, physics) = spawn(&[], Vec3::ZERO, 5.0, 10, &mut budget);
    assert_eq!((result.spawned, result.skipped), (0, 0));
    assert_eq!(physics.bodies.len(), 0);
    assert_eq!(budget.len(), 0);
    let (result, _) = spawn(&[chunk(1.0, 1.0)], Vec3::ZERO, 0.0, 10, &mut budget);
    assert_eq!(result.spawned, 1);
}

#[test]
fn a_chunk_at_the_origin_has_no_direction_and_still_spawns() {
    let mut budget = BodyBudget::default();
    let (result, physics) = spawn(&[chunk(0.0, 1.0)], Vec3::new(3.0, 3.0, 3.0), 5.0, 0, &mut budget);
    assert_eq!(result.spawned, 1);
    let (_, body) = physics.bodies.iter().next().expect("body");
    assert_eq!(body.linvel().x, 0.0);
}

#[test]
fn non_finite_poses_and_invalid_shapes_are_skipped_and_leave_no_body_behind() {
    let mut nan_pos = chunk(1.0, 1.0);
    nan_pos.local_translation.y = f32::NAN;
    let mut nan_rot = chunk(2.0, 1.0);
    nan_rot.local_rotation = Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0);
    let mut bad_shape = chunk(3.0, 1.0);
    bad_shape.shape = ColliderShape::Sphere { radius: -1.0 };
    let good = chunk(4.0, 1.0);
    let mut budget = BodyBudget::default();
    let (result, physics) = spawn(
        &[nan_pos, nan_rot, bad_shape, good],
        Vec3::ZERO,
        1.0,
        0,
        &mut budget,
    );
    assert_eq!((result.spawned, result.skipped), (1, 3));
    assert_eq!(
        physics.bodies.len(),
        1,
        "the invalid-shape body was removed again"
    );
    assert_eq!(budget.len(), 1);
    assert!(result.evicted.is_empty());
    // A non-finite origin skips everything.
    let mut budget = BodyBudget::default();
    let (result, physics) = spawn(
        &[chunk(1.0, 1.0)],
        Vec3::new(f32::INFINITY, 0.0, 0.0),
        1.0,
        0,
        &mut budget,
    );
    assert_eq!((result.spawned, result.skipped), (0, 1));
    assert_eq!(physics.bodies.len(), 0);
}

#[test]
fn a_full_budget_skips_every_chunk_and_a_partial_one_the_tail() {
    let chunks: Vec<ChunkDesc> = (0..5).map(|i| chunk(i as f32 + 1.0, 1.0)).collect();
    let mut budget = BodyBudget::default();
    for i in 0..8 {
        budget.push(Entity::from_raw_u32(i).expect("entity"));
    }
    let (result, physics) = spawn(&chunks, Vec3::ZERO, 1.0, 8, &mut budget);
    assert_eq!((result.spawned, result.skipped), (0, 5), "budget exactly full");
    assert_eq!(physics.bodies.len(), 0);
    let (result, _) = spawn(&chunks, Vec3::ZERO, 1.0, 10, &mut budget);
    assert_eq!(
        (result.spawned, result.skipped),
        (2, 3),
        "two free slots, the first two chunks"
    );
    // Over-full (budget lowered after the fact) behaves like full, not like an underflow.
    let (result, _) = spawn(&chunks, Vec3::ZERO, 1.0, 3, &mut budget);
    assert_eq!((result.spawned, result.skipped), (0, 5));
}
