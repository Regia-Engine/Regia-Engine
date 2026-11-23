// SPDX-License-Identifier: MPL-2.0
//! A warm character move must not build a new shared capsule or a new collision buffer
//! (audit G, finding 5). Rapier's own contact-manifold buffer inside `move_shape` stays.
//!
//! Its own binary: a counting global allocator. Only `move_character` is counted.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_physics::{
    CharacterBody, Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, RigidBody, move_character,
};
use regia_scene::Transform;

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

const RADIUS: f32 = 0.3;
const HALF_HEIGHT: f32 = 0.6;
const DT: f32 = 1.0 / 60.0;
const REST_Y: f32 = HALF_HEIGHT + RADIUS + 0.02;

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

#[test]
fn warm_character_move_does_not_allocate_a_capsule() {
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(20.0, 0.5, 20.0),
            },
            friction: 0.8,
            ..Collider::default()
        },
    ));
    for _ in 0..5 {
        app.update();
    }
    let mut pos = Vec3::new(0.0, REST_Y, 0.0);
    let mut last = 0usize;
    let mut hits = 0usize;
    for i in 0..8 {
        let before = allocations();
        let result = {
            let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
            move_character(
                &mut physics,
                &CharacterBody::default(),
                RADIUS,
                HALF_HEIGHT,
                (pos, Quat::IDENTITY),
                Vec3::new(0.05, -0.05, 0.0),
                50.0_f32.to_radians(),
                0.3,
                0.2,
                None,
                DT,
                DT,
            )
        };
        let n = allocations() - before;
        pos += result.translation;
        if i >= 4 {
            last = n;
            hits = result.hits.len();
            assert!(result.grounded, "the character stays on the plate");
        }
    }
    assert!(hits >= 1, "the measured move touched the ground");
    // The pre-fix move allocated 11 times on this floor: a fresh shared capsule plus the
    // collision buffers. Those are gone. A warm move still allocates the returned `hits`
    // list and rapier's own casts inside `move_shape` (measured 9).
    assert!(
        last <= 9,
        "a warm character move allocated {last} times while touching {hits} colliders"
    );
}
