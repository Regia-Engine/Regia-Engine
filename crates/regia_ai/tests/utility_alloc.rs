// SPDX-License-Identifier: MPL-2.0
//! Audit finding 5: a steady utility decision must reuse its report instead of cloning every
//! action name and consideration key. Counts allocations. No wall clock.
//!
//! Its own test binary: it installs a counting global allocator.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_ai::Blackboard;
use regia_ai::utility::{ActionDef, ConsiderationDef, ResponseCurve, UtilityAiDef};
use regia_ai::utility_ecs::{UtilityPlugin, UtilityReasoner};
use regia_app::prelude::*;
use regia_ecs::ecs::schedule::{Schedule, ScheduleLabel, Schedules, SingleThreadedExecutor};
use regia_ecs::ecs::world::World;

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

const REASONERS: usize = 32;
const WARM_UP_TICKS: usize = 8;
const MEASURED_TICKS: usize = 16;

fn action(name: &str, key: &str) -> ActionDef {
    ActionDef {
        name: name.to_string(),
        considerations: vec![ConsiderationDef {
            key: key.to_string(),
            min: 0.0,
            max: 100.0,
            curve: ResponseCurve::default(),
        }],
        weight: 1.0,
        cooldown: 0.0,
        momentum: 0.0,
    }
}

fn definition() -> UtilityAiDef {
    UtilityAiDef {
        actions: vec![action("attack", "health"), action("flee", "threat")],
        ..UtilityAiDef::default()
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, UtilityPlugin));
    enable_fixed_step(&mut app, 60.0);
    let def = definition();
    for _ in 0..REASONERS {
        let mut reasoner = UtilityReasoner::from_def(&def).expect("valid");
        reasoner.interval_ticks = 1;
        let mut board = Blackboard::default();
        board.insert("health", 90.0f32);
        board.insert("threat", 10.0f32);
        app.world_mut().spawn((reasoner, board));
    }
    app.update();
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app
}

fn step(app: &mut App) {
    app.world_mut().run_schedule(FixedUpdate);
}

fn end_frame(app: &mut App) {
    app.world_mut().clear_trackers();
}

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
    let spent = (0..3)
        .map(|_| {
            let before = allocations();
            schedule.run(&mut world);
            allocations() - before
        })
        .min()
        .unwrap_or(0);
    assert!(spent <= systems, "{spent} allocations by {systems} no-op systems");
    spent
}

#[test]
fn steady_decisions_allocate_nothing() {
    let mut app = app();
    for _ in 0..WARM_UP_TICKS {
        step(&mut app);
        end_frame(&mut app);
    }
    let executor = executor_allocations(&app, FixedUpdate);
    let mut spent = [0; MEASURED_TICKS];
    for ticks in &mut spent {
        let before = allocations();
        step(&mut app);
        *ticks = (allocations() - before).abs_diff(executor);
        end_frame(&mut app);
    }
    let total: usize = spent.iter().sum();
    if total > 0 {
        let at: Vec<(usize, usize)> = spent.iter().copied().enumerate().filter(|t| t.1 > 0).collect();
        eprintln!("allocations by measured tick: {at:?}");
    }
    assert_eq!(
        total, 0,
        "{total} heap allocations across {MEASURED_TICKS} steady decisions of {REASONERS} reasoners"
    );
    let mut query = app.world_mut().query::<&UtilityReasoner>();
    let mut decided = Vec::new();
    for reasoner in query.iter(app.world()) {
        assert_eq!(reasoner.current_action, "attack");
        assert!(
            reasoner.last_report().is_some(),
            "the decision still publishes a report"
        );
        decided.push(reasoner.decisions);
    }
    assert_eq!(decided.len(), REASONERS);
    assert!(
        decided
            .iter()
            .all(|n| *n >= (WARM_UP_TICKS + MEASURED_TICKS) as u64),
        "decisions {decided:?}"
    );
}
