// SPDX-License-Identifier: MPL-2.0
//! ADR-3281: updating a coupled trailer and reading it back allocates nothing in steady state.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `tests/cloth_alloc.rs`).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::convert::to_rapier_vec3;
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleInput};
use regia_physics::vehicle_trailer::{Trailer, TrailerBrakes, TrailerConfig, car_tow_hitch};

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

const DT: f32 = 1.0 / 60.0;

fn step(physics: &mut PhysicsWorld) {
    let integration = IntegrationParameters {
        dt: DT,
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

#[test]
fn trailer_update_and_readout_allocate_nothing_after_warm_up() {
    let mut physics = PhysicsWorld::default();
    let ground = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(0.0, -1.0, 0.0)))
            .build(),
    );
    {
        let PhysicsWorld {
            bodies, colliders, ..
        } = &mut physics;
        colliders.insert_with_parent(ColliderBuilder::cuboid(500.0, 1.0, 500.0).build(), ground, bodies);
    }
    let mut car = Vehicle::spawn(
        &mut physics,
        VehicleConfig::default(),
        Vec3::new(0.0, 0.85, 0.0),
        Quat::IDENTITY,
    )
    .expect("car");
    let mut follow = Trailer::spawn_coupled(
        &mut physics,
        TrailerConfig::car_trailer(),
        car.chassis(),
        car_tow_hitch(),
    )
    .expect("trailer");
    let mut surge_config = TrailerConfig::car_trailer();
    surge_config.brakes = TrailerBrakes::Surge {
        threshold: 300.0,
        full_force: 3000.0,
        response: 0.2,
    };
    surge_config.rear_hitch = None;
    let mut surge = Trailer::spawn(
        &mut physics,
        surge_config,
        Vec3::new(20.0, 0.62, 0.0),
        Quat::IDENTITY,
    )
    .expect("surge trailer");
    let input = |i: usize| VehicleInput {
        throttle: 0.6,
        brake: if i % 50 < 10 { 0.5 } else { 0.0 },
        steer: 0.2,
    };
    for i in 0..120 {
        car.update(&mut physics, input(i), DT).expect("car");
        follow
            .update(&mut physics, input(i).brake, DT, None)
            .expect("trailer");
        surge.update(&mut physics, 0.0, DT, None).expect("surge");
        step(&mut physics);
    }
    let mut counted = 0;
    let mut checksum = 0.0_f32;
    for i in 120..420 {
        car.update(&mut physics, input(i), DT).expect("car");
        let before = allocations();
        follow
            .update(&mut physics, input(i).brake, DT, None)
            .expect("trailer");
        surge.update(&mut physics, 0.0, DT, None).expect("surge");
        let readout = follow.readout(&physics).expect("readout");
        checksum += readout.articulation_angle + readout.brake;
        counted += allocations() - before;
        step(&mut physics);
    }
    assert!(checksum.is_finite());
    assert_eq!(
        counted, 0,
        "trailer update and readout allocated {counted} times in 300 steps"
    );
}
