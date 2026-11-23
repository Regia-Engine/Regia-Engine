// SPDX-License-Identifier: MPL-2.0
//! Audit findings 1 and 2: a steady behaviour-tree step must not clone child-index vectors or
//! condition keys, and must not republish an unchanged [`BehaviorTreeStatus`]. Counts allocations
//! and `Changed<BehaviorTreeStatus>` rows. No wall clock.
//!
//! Its own test binary: it installs a counting global allocator.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_ai::{
    AiPlugin, AiStep, BehaviorTree, BehaviorTreeBuilder, BehaviorTreeStatus, Blackboard, BtContext, BtStatus,
    BtTask, BtTaskRegistration, BtTaskRegistry,
};
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

const LEAVES: usize = 80;
const RUNNING: usize = 80;
const GATED: usize = 40;
const WARM_UP_TICKS: usize = 12;
const MEASURED_TICKS: usize = 24;

/// Rows whose [`BehaviorTreeStatus`] was written this step.
#[derive(Resource, Default)]
struct StatusWrites(u64);

/// A task that keeps running, like a patrol loop.
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

fn count_status_writes(query: Query<(), Changed<BehaviorTreeStatus>>, mut hits: ResMut<StatusWrites>) {
    hits.0 += query.iter().count() as u64;
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    app.init_resource::<StatusWrites>();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register::<Hold>();
    app.add_systems(FixedUpdate, count_status_writes.after(AiStep));
    app.update();
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app
}

fn compile(app: &App, def: BehaviorTreeBuilder, root: usize) -> BehaviorTree {
    let def = def.build(root);
    BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("tree")
}

/// One leaf. The status snapshot is the only per-tick heap traffic this shape can have.
fn leaf_tree(app: &App) -> BehaviorTree {
    let mut builder = BehaviorTreeBuilder::new();
    let leaf = builder.leaf("hold", "alloc_test::Hold");
    compile(app, builder, leaf)
}

/// Selector and sequences that stay resumed on a running leaf, plus a condition that fails before
/// that leaf so the selector still walks a composite every tick.
fn running_tree(app: &App) -> BehaviorTree {
    let mut builder = BehaviorTreeBuilder::new();
    let hold = builder.leaf("hold", "alloc_test::Hold");
    let go = builder.condition("go", "patrol");
    let inner = builder.sequence("inner", vec![go, hold]);
    let mid = builder.sequence("mid", vec![inner]);
    let skip = builder.condition("skip", "absent");
    let skip_seq = builder.sequence("skip_seq", vec![skip]);
    let selector = builder.selector("sel", vec![skip_seq, mid]);
    let root = builder.selector("root", vec![selector]);
    compile(app, builder, root)
}

/// A sequence whose first child is a false condition, so the key is read every tick.
fn gated_tree(app: &App) -> BehaviorTree {
    let mut builder = BehaviorTreeBuilder::new();
    let gate = builder.condition("gate", "absent");
    let root = builder.sequence("root", vec![gate]);
    compile(app, builder, root)
}

fn spawn_trees(app: &mut App) {
    for _ in 0..LEAVES {
        let tree = leaf_tree(app);
        app.world_mut().spawn((tree, Blackboard::default()));
    }
    for _ in 0..RUNNING {
        let tree = running_tree(app);
        let mut board = Blackboard::default();
        board.insert("patrol", true);
        app.world_mut().spawn((tree, board));
    }
    for _ in 0..GATED {
        let tree = gated_tree(app);
        app.world_mut().spawn((tree, Blackboard::default()));
    }
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

/// Heap allocations above the executor allowance, and status writes, over the measured ticks.
fn measure(app: &mut App) -> (usize, u64) {
    for _ in 0..WARM_UP_TICKS {
        step(app);
        end_frame(app);
    }
    app.world_mut().resource_mut::<StatusWrites>().0 = 0;
    let executor = executor_allocations(app, FixedUpdate);
    let mut spent = [0; MEASURED_TICKS];
    for ticks in &mut spent {
        let before = allocations();
        step(app);
        *ticks = (allocations() - before).abs_diff(executor);
        end_frame(app);
    }
    let total = spent.iter().sum();
    if total > 0 {
        let at: Vec<(usize, usize)> = spent.iter().copied().enumerate().filter(|t| t.1 > 0).collect();
        eprintln!("allocations by measured tick: {at:?}");
    }
    (total, app.world().resource::<StatusWrites>().0)
}

#[test]
fn steady_trees_allocate_nothing_and_do_not_rewrite_status() {
    let mut app = app();
    spawn_trees(&mut app);
    let (allocs, writes) = measure(&mut app);
    assert_eq!(
        allocs, 0,
        "{allocs} heap allocations across {MEASURED_TICKS} steady ticks \
         ({LEAVES} leaves, {RUNNING} resumed composites, {GATED} conditions)"
    );
    assert_eq!(
        writes, 0,
        "{writes} BehaviorTreeStatus writes across {MEASURED_TICKS} ticks with no status change"
    );
    let trees = (LEAVES + RUNNING + GATED) as u64;
    let mut running = 0u64;
    let mut failed = 0u64;
    let mut query = app.world_mut().query::<&BehaviorTreeStatus>();
    for status in query.iter(app.world()) {
        match status.last_status {
            BtStatus::Running => running += 1,
            BtStatus::Failure => failed += 1,
            BtStatus::Success => {}
        }
    }
    assert_eq!(running, (LEAVES + RUNNING) as u64);
    assert_eq!(failed, GATED as u64);
    assert_eq!(running + failed, trees);
}
