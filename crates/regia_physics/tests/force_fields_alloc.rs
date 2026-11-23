// SPDX-License-Identifier: MPL-2.0
//! The force-field system allocates nothing in steady state (ADR-3348). Its own test binary:
//! it installs a counting global allocator (the pattern of `cloth_alloc.rs`).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::ecs::schedule::{Schedule, SingleThreadedExecutor};
use regia_physics::{
    Collider, ColliderShape, Falloff, FieldKind, FieldVolume, ForceField, Mass, PhysicsPlugin,
    PhysicsSettings, RigidBody, apply_force_fields,
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

/// What the executor allocates by itself in one run of a one-system schedule, measured on a no-op
/// system: in a whole-workspace build `tracing` has its `log` feature, and with no subscriber the
/// ECS then formats each system's name for a span, one `String` per system per run, none under
/// `cargo test -p` (ADR-2012, the pattern of `regia_ai`'s `perception_alloc.rs`).
fn executor_allocations() -> usize {
    fn no_op() {}
    let mut schedule = Schedule::default();
    schedule.set_executor(SingleThreadedExecutor::new());
    schedule.add_systems(no_op);
    let mut world = regia_ecs::ecs::world::World::new();
    schedule.run(&mut world);
    let before = allocations();
    schedule.run(&mut world);
    allocations() - before
}

#[test]
fn force_field_system_allocates_nothing_in_steady_state() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.insert_resource(PhysicsSettings::default());
    app.add_plugins(PhysicsPlugin);
    let fields = [
        ForceField::new(FieldKind::Directional { vector: Vec3::X }, FieldVolume::Global),
        ForceField::new(
            FieldKind::Radial {
                strength: 5.0,
                falloff: Falloff::Curve(vec![(0.0, 1.0), (5.0, 0.0)]),
            },
            FieldVolume::Sphere { radius: 5.0 },
        ),
        ForceField::new(FieldKind::LinearDrag { coefficient: 0.1 }, FieldVolume::Global),
        ForceField::new(
            FieldKind::ImpulsePad { impulse: Vec3::Y },
            FieldVolume::Box {
                half_extents: Vec3::new(2.0, 2.0, 2.0),
            },
        ),
    ];
    for field in fields {
        app.world_mut()
            .spawn((field, Transform::from_translation(Vec3::new(0.0, 1.0, 0.0))));
    }
    for i in 0..40 {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(i as f32 * 0.2 - 4.0, 1.0, 0.0)),
            RigidBody::dynamic(),
            Mass::Explicit {
                mass: 1.0,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(0.1),
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.05 },
                sensor: true,
                density: 0.0,
                ..Collider::default()
            },
        ));
    }
    for _ in 0..10 {
        app.update();
    }
    let mut schedule = Schedule::default();
    schedule.set_executor(SingleThreadedExecutor::new());
    schedule.add_systems(apply_force_fields);
    // Warm-up: initialises the system and grows the scratch buffers.
    for _ in 0..5 {
        schedule.run(app.world_mut());
    }
    let allowance = executor_allocations() * 100;
    let before = allocations();
    for _ in 0..100 {
        schedule.run(app.world_mut());
    }
    let after = allocations();
    assert_eq!(
        (after - before).saturating_sub(allowance),
        0,
        "allocations in 100 steady-state field steps beyond the executor's own {allowance}"
    );
}
