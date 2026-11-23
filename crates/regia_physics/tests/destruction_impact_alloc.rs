// SPDX-License-Identifier: MPL-2.0
//! Impact damage (ADR-3272) allocates nothing per step in steady state: a box resting on an
//! opted-in wall is read every step (one contact pair, refused by the speed gate) and
//! `DestructionImpact::convert` keeps its buffers.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `tests/cloth_alloc.rs`; ADR-1672 names it).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use rapier3d::dynamics::RigidBodyBuilder;
use rapier3d::geometry::ColliderBuilder;
use regia_core::math::Vec3;
use regia_physics::PhysicsWorld;
use regia_physics::convert::to_rapier_vec3;
use regia_physics::destruction::{
    Authority, BoxGrid, DestructibleAsset, DestructiblePose, DestructionImpact, DestructionMaterial,
    DestructionPhysics, DestructionWorld, GridSupport, add_fixed_box, step_standalone,
};

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

#[test]
fn converting_a_resting_contact_allocates_nothing_after_warm_up() {
    const DT: f32 = 1.0 / 60.0;
    let mut physics = PhysicsWorld::default();
    add_fixed_box(&mut physics, [0.0, -0.5, 0.0], [50.0, 0.5, 50.0]);
    let mut world = DestructionWorld::new(Authority::Server);
    let mut bridge = DestructionPhysics::default();
    let asset = DestructibleAsset::box_grid(&BoxGrid {
        name: "wall".into(),
        counts: [6, 4, 1],
        cell: [1.0; 3],
        density: 2400.0,
        strength_per_area: 1.0e9,
        support: GridSupport::Bottom,
        material: DestructionMaterial::default(),
    })
    .expect("wall");
    let id = world
        .define(Arc::new(asset), DestructiblePose::default(), 1)
        .expect("define");
    bridge.attach(&mut physics, &world, id).expect("attach");
    let body = physics.bodies.insert(
        RigidBodyBuilder::dynamic()
            .translation(to_rapier_vec3(Vec3::new(-0.5, 4.2, 0.0)))
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = &mut physics;
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(0.2, 0.2, 0.2).mass(50.0).build(),
        body,
        bodies,
    );
    let mut impact = DestructionImpact::default();
    impact.enable(&world, id).expect("enable");
    let mut tick =
        |physics: &mut PhysicsWorld, world: &mut DestructionWorld, impact: &mut DestructionImpact| {
            world.step(DT);
            bridge.sync(physics, world, DT);
            step_standalone(physics, DT, [0.0, -9.81, 0.0]);
            let before = allocations();
            impact.convert(physics, &bridge, world, DT);
            allocations() - before
        };
    for _ in 0..120 {
        tick(&mut physics, &mut world, &mut impact);
    }
    let mut converted = 0usize;
    let pairs_before = impact.totals().pairs;
    for _ in 0..600 {
        converted += tick(&mut physics, &mut world, &mut impact);
    }
    assert!(
        impact.totals().pairs > pairs_before,
        "the resting contact was read"
    );
    assert_eq!(impact.totals().hits, 0);
    assert_eq!(
        converted, 0,
        "convert allocated {converted} times over 600 steady steps"
    );
}
