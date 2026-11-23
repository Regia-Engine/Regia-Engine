// SPDX-License-Identifier: MPL-2.0
//! Ambient life allocates nothing per frame in steady state (ADR-3024): its three `Update`
//! systems, the birds' flight and the draw's frame build, counted over fixed frames with no wall
//! clock. Every system runs on this thread (a single-threaded schedule), so the thread-local count
//! sees all of it.
//!
//! Its own test binary: it installs a counting global allocator.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::schedule::{Schedule, SingleThreadedExecutor};
use regia_ecs::ecs::world::World;
use regia_render::TimeOfDay;
use regia_scene::Transform;
use regia_vfx::ambient::{follow_ambient_birds, step_ambient_birds, step_ambient_life};
use regia_vfx::{
    AmbientBirds, AmbientLifeHandle, AmbientLifeSettings, FollowAmbientBird, VfxView, Weather, WeatherConfig,
    WeatherState,
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

const WARM_UP_FRAMES: usize = 30;
const MEASURED_FRAMES: usize = 240;

/// What the executor alone spends per run on a schedule of the same shape as the systems below:
/// three chained no-op systems, the first two with `Commands`, as in the plugin. Each node costs an
/// allocation in a build with the ECS's system spans on (the workspace's feature set).
fn executor_baseline() -> usize {
    let mut schedule = Schedule::default();
    schedule.set_executor(SingleThreadedExecutor::new());
    schedule.add_systems(((|_: Commands| {}), (|_: Commands| {}), (|| {})).chain_ignore_deferred());
    let mut world = World::new();
    schedule.run(&mut world);
    let before = allocations();
    schedule.run(&mut world);
    allocations() - before
}

#[test]
fn steady_frames_allocate_nothing() {
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(TimeOfDay {
        hour: 15.0,
        day_of_year: 200,
        latitude_deg: 45.0,
    });
    world.insert_resource(VfxView::default());
    world.insert_resource(Weather::new(WeatherState::CLEAR));
    world.insert_resource(WeatherConfig::default());
    world.insert_resource(AmbientLifeHandle::default());
    world.insert_resource(AmbientLifeSettings {
        enabled: true,
        ..AmbientLifeSettings::default()
    });
    let flock = world
        .spawn((
            AmbientBirds {
                count: 32,
                ..AmbientBirds::flock()
            },
            Transform::from_translation(Vec3::new(0.0, 0.0, -60.0)),
        ))
        .id();
    world.spawn((Transform::IDENTITY, FollowAmbientBird { flock, bird: 1 }));
    let mut schedule = Schedule::default();
    schedule.set_executor(SingleThreadedExecutor::new());
    schedule
        .add_systems((step_ambient_life, step_ambient_birds, follow_ambient_birds).chain_ignore_deferred());
    let frame = |world: &mut World, schedule: &mut Schedule| {
        world.resource_mut::<Time>().advance(Duration::from_millis(16));
        schedule.run(world);
        world.clear_trackers();
        // The render hook's side: the frame the draw uploads.
        let handle = world.resource::<AmbientLifeHandle>().clone();
        let frame = handle
            .state
            .lock()
            .map(|state| state.frame_at([1.0, 1.7, 2.0]))
            .expect("state lock");
        assert!(!frame.is_empty());
    };
    for _ in 0..WARM_UP_FRAMES {
        frame(&mut world, &mut schedule);
    }
    let baseline = executor_baseline();
    let before = allocations();
    for _ in 0..MEASURED_FRAMES {
        frame(&mut world, &mut schedule);
    }
    let spent = allocations() - before;
    assert!(
        spent <= baseline * MEASURED_FRAMES,
        "{spent} allocations over {MEASURED_FRAMES} frames (the executor alone: {baseline} per run)"
    );
    let handle = world.resource::<AmbientLifeHandle>().clone();
    let birds = handle.state.lock().map(|s| s.bird_count).expect("state lock");
    assert!(birds > 0, "the flock flew and drew");
}
