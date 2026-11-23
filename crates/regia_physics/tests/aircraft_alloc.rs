// SPDX-License-Identifier: MPL-2.0
//! Where a flying and a taxiing aircraft's per-step update allocates, and where it does not
//! (ADR-3374).
//!
//! Its own test binary: it installs a counting global allocator (the pattern of `tests/cloth_alloc.rs`,
//! ADR-1672). Only `Aircraft::update_with_layers` is counted; the world step is rapier's. The full
//! update keeps one allocation per step that belongs to rapier 0.36 itself (its modified-bodies
//! list is `mem::take`n by `PhysicsPipeline::step` and regrown by the first `get_mut` after each
//! step — every per-step body mutation in the crate pays it, including hover and the vehicles;
//! see the ignored tests' docs). `the_owned_paths_allocate_nothing` pins the parts this crate
//! owns at zero.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::{ColliderBuilder, Ray};
use rapier3d::pipeline::QueryFilter;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::aero::{
    AeroEnvironment, Aircraft, AircraftInput, Kinematics, SurfaceState, evaluate_loads, trainer,
};
use regia_physics::convert::to_rapier_vec3;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

fn step(physics: &mut PhysicsWorld) {
    let integration = IntegrationParameters {
        dt: 1.0 / 60.0,
        ..Default::default()
    };
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
        ..
    } = physics;
    pipeline.step(
        to_rapier_vec3(Vec3::new(0.0, -9.81, 0.0)),
        &integration,
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
}

/// Counts the allocations of `steps` updates of `plane` after a 30-step warm-up (the world steps between).
fn count_updates(
    physics: &mut PhysicsWorld,
    plane: &mut Aircraft,
    input: AircraftInput,
    steps: usize,
) -> usize {
    let env = AeroEnvironment {
        wind: Vec3::new(2.0, 0.0, 1.0),
        ..AeroEnvironment::default()
    };
    for _ in 0..30 {
        plane
            .update_with_layers(physics, input, &env, 1.0 / 60.0, None)
            .expect("update");
        step(physics);
    }
    let mut counted = 0;
    for _ in 0..steps {
        let before = allocations();
        plane
            .update_with_layers(physics, input, &env, 1.0 / 60.0, None)
            .expect("update");
        counted += allocations() - before;
        step(physics);
    }
    counted
}

fn ground_world() -> PhysicsWorld {
    let mut physics = PhysicsWorld::default();
    let ground = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(0.0, -0.5, 0.0)))
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = &mut physics;
    colliders.insert_with_parent(ColliderBuilder::cuboid(500.0, 0.5, 500.0).build(), ground, bodies);
    physics
}

/// Open (ADR-3374): one allocation per update, in flight and on the ground. Provenance (backtrace
/// captured on the test machine, 2026-11-07): rapier 0.36 `PhysicsPipeline::step` takes the body's
/// modified-objects list with `mem::take` (`physics_pipeline/substep.rs`), leaving a zero-capacity
/// `Vec`; the first public `RigidBodySet::get_mut` after each step (ours, to add the forces)
/// regrows that Vec (32 bytes). Every rapier body mutated per step pays this — the hover craft and
/// the ground vehicles do too — and no public rapier API applies a force without `get_mut`, so the
/// lane cannot remove it (patching the dependency is out of scope). Everything the crate itself
/// allocates on this path is covered by `the_owned_paths_allocate_nothing`. Run with `--ignored`
/// to re-measure.
#[test]
#[ignore = "known: rapier 0.36 regrows its modified-bodies Vec once per stepped body (ADR-3374)"]
fn a_flying_update_allocates_nothing() {
    let mut physics = ground_world();
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 300.0, 0.0),
        Quat::from_rotation_x(0.05),
        Vec3::new(0.0, 0.0, -50.0),
    )
    .expect("spawn in the air");
    let input = AircraftInput {
        throttle: 0.7,
        pitch: 0.1,
        roll: 0.05,
        ..AircraftInput::default()
    };
    let counted = count_updates(&mut physics, &mut plane, input, 120);
    assert_eq!(
        counted, 0,
        "flying updates allocated {counted} times in 120 steps"
    );
}

/// Same provenance as `a_flying_update_allocates_nothing` (ADR-3374). Run with `--ignored` to
/// re-measure.
#[test]
#[ignore = "known: rapier 0.36 regrows its modified-bodies Vec once per stepped body (ADR-3374)"]
fn a_taxiing_update_allocates_nothing() {
    let mut physics = ground_world();
    let mut plane = Aircraft::spawn(
        &mut physics,
        trainer(),
        Vec3::new(0.0, 1.3, 0.0),
        Quat::IDENTITY,
        Vec3::ZERO,
    )
    .expect("spawn on the ground");
    let input = AircraftInput {
        throttle: 0.3,
        yaw: 0.2,
        brakes: 0.1,
        ..AircraftInput::default()
    };
    let counted = count_updates(&mut physics, &mut plane, input, 120);
    assert!(
        plane.gear().iter().any(|g| g.grounded),
        "the taxiing plane is on its wheels"
    );
    assert_eq!(
        counted, 0,
        "taxiing updates allocated {counted} times in 120 steps"
    );
}

/// The paths the crate owns allocate nothing: the load evaluation and the gear-ray query machinery
/// (ADR-3374). The one residual per-step allocation of a full update lives inside rapier 0.36's
/// modified-body tracking (see `a_flying_update_allocates_nothing`).
#[test]
fn the_owned_paths_allocate_nothing() {
    let config = trainer();
    let kin = Kinematics {
        center_of_mass: Vec3::new(0.0, 300.0, 0.0),
        rotation: Quat::from_rotation_x(0.05),
        linear_velocity: Vec3::new(0.0, 0.0, -50.0),
        angular_velocity: Vec3::ZERO,
    };
    let input = AircraftInput {
        throttle: 0.7,
        pitch: 0.1,
        roll: 0.05,
        ..AircraftInput::default()
    };
    let env = AeroEnvironment {
        wind: Vec3::new(2.0, 0.0, 1.0),
        ..AeroEnvironment::default()
    };
    let mut surfaces = vec![SurfaceState::default(); config.surfaces.len()];
    let mut thrusts = vec![0.0_f32; config.engines.len()];
    let before = allocations();
    for _ in 0..120 {
        let loads = evaluate_loads(&config, &kin, &input, &env, &mut surfaces, &mut thrusts);
        std::hint::black_box(loads);
    }
    assert_eq!(
        allocations() - before,
        0,
        "evaluate_loads allocated {} times in 120 calls",
        allocations() - before
    );

    let physics = ground_world();
    let filter = QueryFilter::default().exclude_sensors();
    let before = allocations();
    for _ in 0..120 {
        let pipeline = physics.query_pipeline(filter);
        let ray = Ray::new(
            to_rapier_vec3(Vec3::new(0.0, 300.0, 0.0)),
            to_rapier_vec3(-Vec3::Y),
        );
        let hit = pipeline.cast_ray_and_get_normal(&ray, 2.0, true);
        std::hint::black_box(hit);
    }
    assert_eq!(
        allocations() - before,
        0,
        "the gear-ray query machinery allocated {} times in 120 casts",
        allocations() - before
    );
}
