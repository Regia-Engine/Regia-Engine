// SPDX-License-Identifier: MPL-2.0
//! A steady perception step takes zero heap allocations (ADR-2565): agents keep seeing the same
//! targets, memory refreshes in place and the blackboard write-through compares without writing.
//! Counted over fixed steps with no wall clock. Every step runs on this thread, so the thread-local
//! count sees the whole of it.
//!
//! Its own test binary: it installs a counting global allocator.

use regia_ai::perception_system::{
    Perceivable, Perceived, PerceptionPlugin, PerceptionSettings, PerceptionStats,
};
use regia_ai::{Blackboard, Perception};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::schedule::{Schedule, ScheduleLabel, Schedules, SingleThreadedExecutor};
use regia_ecs::ecs::world::World;
use regia_scene::Transform;

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

/// What the executor allocates by itself in one run of `label`, whatever the systems do, measured on
/// a schedule of as many no-op systems. In a whole-workspace build `tracing` has its `log` feature,
/// and the ECS then formats each system's name for a span (see `state_tree_alloc.rs`).
fn executor_allocations(app: &App, label: impl ScheduleLabel) -> usize {
    fn no_op() {}
    let systems = app
        .world()
        .resource::<Schedules>()
        .get(label.intern())
        .map_or(0, Schedule::systems_len);
    let mut schedule = Schedule::default();
    schedule.set_executor(SingleThreadedExecutor::new());
    for _ in 0..systems {
        schedule.add_systems(no_op);
    }
    let mut world = World::new();
    schedule.run(&mut world);
    let before = allocations();
    schedule.run(&mut world);
    allocations() - before
}

#[test]
fn a_steady_step_allocates_nothing() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(PerceptionPlugin);
    app.update();
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    {
        let mut settings = app.world_mut().resource_mut::<PerceptionSettings>();
        settings.sight_interval_ticks = 1;
        settings.sight_checks_per_tick = 256;
    }
    for i in 0..200 {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(i as f32 * 0.01, 0.0, 0.0)),
            Perception::default(),
            // Room for every target: past the capacity, memory churns and writes messages each step,
            // which is not a steady state.
            Perceived {
                capacity: 64,
                ..Perceived::default()
            },
            Blackboard::new(),
        ));
    }
    for i in 0..50 {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(i as f32 * 0.01, 0.0, -5.0)),
            Perceivable {
                team: 1,
                ..Perceivable::default()
            },
        ));
    }
    // Long enough for several full sweeps: every memory is full and every buffer has its size.
    for _ in 0..200 {
        app.world_mut().run_schedule(FixedUpdate);
    }
    let allowance = executor_allocations(&app, FixedUpdate);
    let mut most = 0;
    let checks_before = app.world().resource::<PerceptionStats>().sight_checks_total;
    for _ in 0..120 {
        let before = allocations();
        app.world_mut().run_schedule(FixedUpdate);
        most = most.max(allocations() - before);
    }
    let stats = app.world().resource::<PerceptionStats>();
    // 256 a step, less on the step a sweep of 10 000 pairs runs out.
    let checks = stats.sight_checks_total - checks_before;
    assert!((120 * 200..=120 * 256).contains(&checks), "{checks} checks");
    assert!(
        most <= allowance,
        "{most} allocations in a step, the executor accounts for {allowance}"
    );
}
