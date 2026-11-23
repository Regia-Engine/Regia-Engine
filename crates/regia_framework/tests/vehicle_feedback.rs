// SPDX-License-Identifier: MPL-2.0
//! Vehicle feedback (ADR-3276): a car driven by the real plugins at a fixed 60 Hz, no wall clock.
//!
//! Straight on flat ground gives no rumble; a turn on tires with a pneumatic trail gives the low
//! channel; a landing gives the high channel; every request stays in `[0, 1]`; a steady state
//! allocates nothing (its own binary: it installs a counting global allocator, the pattern of
//! `feedback_cost.rs`).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::{Entity, Messages};
use regia_framework::vehicle_feedback::{VehicleFeedback, VehicleFeedbackMap, VehicleFeedbackPlugin};
use regia_physics::vehicle::{VehicleConfig, VehicleController, VehicleInput, VehiclePlugin, spawn_vehicle};
use regia_physics::vehicle_tire::{SlipTire, TireModel};
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia_platform::RumbleRequest;
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

const DEVICE: u64 = 7;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin, VehicleFeedbackPlugin));
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(500.0, 0.5, 500.0),
            },
            ..Collider::default()
        },
    ));
    app
}

/// The default car on slip tires with a 4 cm trail, no rolling resistance, at `height`.
fn car(app: &mut App, height: f32) -> Entity {
    let mut config = VehicleConfig {
        rolling_resistance: 0.0,
        ..VehicleConfig::default()
    };
    for wheel in &mut config.wheels {
        wheel.tire_model = TireModel::Slip(SlipTire {
            pneumatic_trail: 0.04,
            relaxation_length: 0.4,
            ..SlipTire::default()
        });
    }
    let entity = spawn_vehicle(
        app.world_mut(),
        config,
        Vec3::new(0.0, height, 0.0),
        Quat::IDENTITY,
    )
    .expect("car");
    app.world_mut()
        .entity_mut(entity)
        .insert(VehicleFeedback::new(VehicleFeedbackMap::default(), Some(DEVICE)));
    entity
}

/// Runs `frames` frames with `input`, returning the strengths of the requests sent (all devices).
fn run(app: &mut App, car: Entity, input: VehicleInput, frames: usize) -> Vec<f32> {
    let mut sent = Vec::new();
    for _ in 0..frames {
        app.world_mut().entity_mut(car).insert(input);
        app.update();
        let mut messages = app.world_mut().resource_mut::<Messages<RumbleRequest>>();
        for request in messages.drain() {
            assert_eq!(request.device, DEVICE);
            assert!(
                (0.0..=1.0).contains(&request.strength) && request.strength > 0.0,
                "{request:?}"
            );
            sent.push(request.strength);
        }
    }
    sent
}

fn feedback(app: &App, car: Entity) -> VehicleFeedback {
    app.world().get::<VehicleFeedback>(car).expect("feedback").clone()
}

#[test]
fn straight_on_flat_ground_is_silent_and_a_turn_and_a_landing_are_felt() {
    let mut app = app();
    let car = car(&mut app, 0.9);
    // Settle on the springs, then roll straight ahead.
    run(&mut app, car, VehicleInput::default(), 90);
    let straight = VehicleInput {
        throttle: 0.4,
        brake: 0.0,
        steer: 0.0,
    };
    run(&mut app, car, straight, 60);
    let sent = run(&mut app, car, straight, 120);
    let state = feedback(&app, car);
    assert!(
        sent.is_empty(),
        "driving straight on flat ground rumbled: {sent:?}"
    );
    assert_eq!((state.low, state.high, state.strength), (0.0, 0.0, 0.0));
    let speed = app
        .world()
        .get::<VehicleController>(car)
        .map(|c| c.vehicle.steps())
        .expect("steps");
    assert!(speed > 200, "the car was driven ({speed} steps)");
    // A firm turn: the trail pushes the wheels back, the low channel rises.
    let turning = VehicleInput {
        throttle: 0.4,
        brake: 0.0,
        steer: 0.8,
    };
    let sent = run(&mut app, car, turning, 60);
    let state = feedback(&app, car);
    assert!(state.low > 0.0, "{state:?}");
    assert!(
        state.steering_torque > 0.0,
        "a right turn: the tires push the wheels back left: {state:?}"
    );
    assert_eq!(state.axles.len(), 1);
    assert!(!sent.is_empty());

    // Dropped from 0.6 m above its ride height, the landing jolts.
    let mut app = self::app();
    let car = self::car(&mut app, 1.5);
    let sent = run(&mut app, car, VehicleInput::default(), 90);
    assert!(sent.iter().any(|s| *s > 0.2), "{sent:?}");
    let peak_high = feedback(&app, car).high;
    run(&mut app, car, VehicleInput::default(), 120);
    let settled = feedback(&app, car);
    assert!(
        settled.high < 0.05 && settled.high <= peak_high.max(1.0),
        "{settled:?}"
    );
}

#[test]
fn a_steady_feedback_step_allocates_nothing() {
    let mut app = app();
    let car = car(&mut app, 0.9);
    let turning = VehicleInput {
        throttle: 0.3,
        brake: 0.0,
        steer: 0.5,
    };
    run(&mut app, car, turning, 120);
    let world = app.world_mut();
    let controller = world.get::<VehicleController>(car).expect("car").clone();
    let mut feedback = world.get::<VehicleFeedback>(car).expect("feedback").clone();
    // Warm the buffers, then count.
    feedback.step(&controller, 1.0 / 60.0);
    let before = allocations();
    for _ in 0..600 {
        let strength = feedback.step(&controller, 1.0 / 60.0);
        assert!((0.0..=1.0).contains(&strength));
    }
    assert_eq!(allocations() - before, 0, "feedback steps allocated");
}
