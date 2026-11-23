// SPDX-License-Identifier: MPL-2.0
//! The tick budget's scheduler takes no heap allocation per step in steady state (ADR-2709), counted over
//! fixed steps with no wall clock: 1,000 state trees, which do not allocate themselves (spec 38 § 9), on
//! a budget that runs a quarter of them per step.
//!
//! Its own test binary: it installs a counting global allocator.

use regia_ai::blackboard::Blackboard;
use regia_ai::state_tree::{StateTree, StateTreeDef};
use regia_ai::{
    AiPlugin, AiTickBudget, AiTickStats, AiTickTier, BtContext, BtStatus, BtTask, BtTaskRegistration,
    BtTaskRegistry,
};
use regia_app::prelude::*;
use regia_ecs::ecs::schedule::{Schedule, ScheduleLabel, Schedules, SingleThreadedExecutor};
use regia_ecs::ecs::world::World;

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

const WARM_UP_STEPS: usize = 40;
const MEASURED_STEPS: usize = 120;

#[derive(Default)]
struct Hold;

impl BtTask for Hold {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Running
    }
}

impl BtTaskRegistration for Hold {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Hold)
    }

    fn task_type_path() -> &'static str {
        "alloc_test::Hold"
    }
}

/// What the executor allocates by itself in one run of `label` (see `state_tree_alloc.rs`).
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
fn a_steady_budgeted_step_allocates_nothing() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register::<Hold>();
    app.update();
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app.world_mut().insert_resource(AiTickBudget {
        enabled: true,
        max_trees_per_step: 300,
        tiers: vec![AiTickTier {
            max_distance: f32::MAX,
            cadence_steps: 4,
            max_trees_per_step: 0,
        }],
        focus: None,
    });
    let def = StateTreeDef::from_ron(
        r#"(format_version: 2, root: "Root", states: [(name: "Root", tasks: ["alloc_test::Hold"])])"#,
    )
    .expect("definition");
    for _ in 0..1_000 {
        let tree = StateTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("tree");
        app.world_mut().spawn((tree, Blackboard::new()));
    }
    for _ in 0..WARM_UP_STEPS {
        app.world_mut().run_schedule(FixedUpdate);
        app.world_mut().clear_trackers();
    }
    let executor = executor_allocations(&app, FixedUpdate);
    let mut spent = [0; MEASURED_STEPS];
    for step in &mut spent {
        let before = allocations();
        app.world_mut().run_schedule(FixedUpdate);
        *step = (allocations() - before).abs_diff(executor);
        app.world_mut().clear_trackers();
    }
    let at: Vec<(usize, usize)> = spent.iter().copied().enumerate().filter(|s| s.1 > 0).collect();
    assert_eq!(at, Vec::new(), "allocations by measured step");
    let stats = app.world().resource::<AiTickStats>();
    assert_eq!(stats.trees, 1_000);
    assert_eq!(stats.steps, 160);
    assert_eq!(
        stats.ticks_total, 40_000,
        "the scheduler did run: a quarter of the trees per step"
    );
    assert!(stats.ticks_last_step <= 300);
}
