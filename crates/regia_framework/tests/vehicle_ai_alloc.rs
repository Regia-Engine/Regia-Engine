// SPDX-License-Identifier: MPL-2.0
//! ADR-3117 acceptance (allocation half): 64 AI drivers allocate nothing per step after warm-up.
//!
//! Its own test binary: it installs a counting global allocator. The driver system is run by hand
//! (not through the schedule), so only its own work is counted: the app's update steps the physics
//! and the vehicles, then `drive_vehicle_ai` runs once per step under the counter.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use regia_app::prelude::*;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::system::System as _;
use regia_framework::vehicle_ai::{DriveTarget, VehicleDriver, VehicleDriverState, drive_vehicle_ai};
use regia_nav::{NavLaneDef, NavLaneGraph};
use regia_physics::vehicle::{VehicleConfig, VehiclePlugin, spawn_vehicle};
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
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

/// A long oval (300 m straights, 60 m radius ends): lanes 0 → 1 → 2 → 3 → 0.
fn oval() -> NavLaneGraph {
    let (r, len) = (60.0f32, 300.0f32);
    let lane = |points: Vec<(f32, f32)>| {
        let p: Vec<Vec3> = points.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
        let mut def = NavLaneDef::new(&p);
        def.speed = 15.0;
        def.width = 3.5;
        def
    };
    let arc = |cx: f32, cz: f32, sign: f32| {
        (0..=32)
            .map(|i| {
                let t = std::f32::consts::PI * i as f32 / 32.0;
                (cx + sign * r * t.sin(), cz + sign * r * t.cos())
            })
            .collect::<Vec<_>>()
    };
    let mut lanes = vec![
        lane(vec![(0.0, 0.0), (len, 0.0)]),
        lane(arc(len, -r, 1.0)),
        lane(vec![(len, -2.0 * r), (0.0, -2.0 * r)]),
        lane(arc(0.0, -r, -1.0)),
    ];
    for (i, l) in lanes.iter_mut().enumerate() {
        l.next = vec![((i + 1) % 4) as u32];
    }
    NavLaneGraph::build(&lanes, 0.0).expect("oval")
}

#[test]
fn sixty_four_drivers_allocate_nothing_per_step_after_warm_up() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin));
    app.world_mut().spawn((
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(800.0, 0.5, 800.0),
            },
            ..Collider::default()
        },
        Transform {
            translation: Vec3::new(0.0, -0.5, 0.0),
            ..Transform::IDENTITY
        },
    ));
    let graph = oval();
    let lap = graph.total_length();
    let mut cars = Vec::new();
    for k in 0..64 {
        let mut along = k as f32 * lap / 64.0;
        let mut index = 0u32;
        while let Some(l) = graph.lane(index) {
            if along <= l.length() {
                break;
            }
            along -= l.length();
            index += 1;
        }
        let (p, dir) = graph.lane(index).expect("lane").sample(along);
        let yaw = Quat::from_rotation_y((-dir.x).atan2(-dir.z));
        let at = Vec3::new(p.x, 0.2 + 0.35 - 9.81 / 120.0 + 0.35 + 0.02, p.z);
        let e = spawn_vehicle(app.world_mut(), VehicleConfig::default(), at, yaw).expect("spawn");
        app.world_mut()
            .entity_mut(e)
            .insert(VehicleDriver::to(DriveTarget::Loop { lane: index }));
        cars.push(e);
    }
    app.insert_resource(graph);
    let mut system = IntoSystem::into_system(drive_vehicle_ai);
    let _access = system.initialize(app.world_mut());
    // Warm-up: plans, scratch growth, lazy statics.
    for _ in 0..90 {
        app.update();
        system.run((), app.world_mut()).expect("driver system");
    }
    let mut counted = 0;
    for _ in 0..120 {
        app.update();
        let before = allocations();
        system.run((), app.world_mut()).expect("driver system");
        counted += allocations() - before;
    }
    let moving = cars
        .iter()
        .filter(|&&e| {
            app.world()
                .get::<VehicleDriver>(e)
                .is_some_and(|d| d.state() != VehicleDriverState::Idle && d.status().speed > 2.0)
        })
        .count();
    println!("64 drivers: {counted} allocations over 120 steps, {moving} moving");
    assert_eq!(moving, 64, "every driver is driving");
    assert_eq!(counted, 0, "allocations per step after warm-up");
}
