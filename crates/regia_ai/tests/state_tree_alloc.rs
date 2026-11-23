// SPDX-License-Identifier: MPL-2.0
//! Spec 38 § 9: 1,000 trees of 6 states with `Tick` guards on blackboard compares take zero heap
//! allocations per tick in steady state, counted over fixed ticks with no wall clock. Every tick
//! runs on this thread, so the thread-local count sees the whole of it.
//!
//! Its own test binary: it installs a counting global allocator.

use regia_ai::blackboard::{BbValue, Blackboard};
use regia_ai::state_tree::{StateTree, StateTreeDef, StateTreeInbox, StateTreeStatus};
use regia_ai::{AiPlugin, BtContext, BtStatus, BtTask, BtTaskRegistration, BtTaskRegistry};
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

const TREES: usize = 1_000;
const WARM_UP_TICKS: usize = 40;
const MEASURED_TICKS: usize = 120;

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

/// 6 states: a root and five leaves, each leaf with a `Tick` guard on a blackboard compare that
/// stays false, so the steady state is one leaf running and every guard evaluated.
const TREE: &str = r#"(
    format_version: 2,
    root: "Root",
    state_key: "mode",
    states: [
        (name: "Root", children: ["Idle", "Alert", "Chase", "Search", "Flee"], tasks: ["alloc_test::Hold"]),
        (name: "Idle", tasks: ["alloc_test::Hold"], transitions: [
            (trigger: Tick, condition: Compare(key: "threat", op: Lt, value: 12.0), target: State("Alert")),
            (trigger: Tick, condition: All([Key("hurt"), Compare(key: "health", op: Lt, value: 5.0)]),
             target: State("Flee")),
            (trigger: Tick, condition: Compare(key: "threat", op: Lt, value: 2.0), hold: 0.25,
             target: State("Chase")),
        ]),
        (name: "Alert", transitions: [
            (trigger: Tick, condition: Compare(key: "threat", op: Gt, value: 20.0), target: State("Idle")),
        ]),
        (name: "Chase", transitions: [
            (trigger: Tick, condition: Compare(key: "threat", op: Gt, value: 20.0), target: State("Search")),
        ]),
        (name: "Search", transitions: [
            (trigger: Tick, condition: Compare(key: "threat", op: Lt, value: 12.0), target: State("Chase")),
        ]),
        (name: "Flee", transitions: [
            (trigger: Tick, condition: Compare(key: "health", op: Gt, value: 50.0), target: State("Idle")),
        ]),
    ],
)"#;

fn app() -> App {
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
    app
}

fn tree(app: &App) -> StateTree {
    let def = StateTreeDef::from_ron(TREE).expect("definition");
    StateTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("tree")
}

fn blackboard() -> Blackboard {
    let mut board = Blackboard::new();
    board.insert("threat".to_owned(), BbValue::Float(100.0));
    board.insert("health".to_owned(), BbValue::Float(40.0));
    board
}

fn spawn_trees(app: &mut App) -> Vec<Entity> {
    (0..TREES)
        .map(|_| {
            let tree = tree(app);
            app.world_mut().spawn((tree, blackboard())).id()
        })
        .collect()
}

fn step(app: &mut App) {
    app.world_mut().run_schedule(FixedUpdate);
}

/// What a frame does after its steps, outside the counted part: `App::update` empties the removal
/// log. A tick takes each tree out of its entity, 1,000 removals a tick, so a run of bare steps
/// would grow that log by doubling and count it against the tree.
fn end_frame(app: &mut App) {
    app.world_mut().clear_trackers();
}

/// What the executor allocates by itself in one run of `label`, whatever the systems do, measured
/// on a schedule of as many no-op systems. In a whole-workspace build `tracing` has its `log`
/// feature (feature unification), and then the ECS formats each system's name for a span even with
/// no subscriber: one `String` per system per run, none under `cargo test -p` (ADR-2012). The
/// counts in this file are what the engine's code does above that, so they stay exact in both
/// builds, and the allowance cannot grow past one per system unnoticed.
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
    // The least of three runs: one run that caught a lazy one-off allocation would inflate the
    // allowance subtracted from every measured step (audit-v035-flaky L5).
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

/// Heap allocations in [`MEASURED_TICKS`] ticks after [`WARM_UP_TICKS`], above the executor's own.
fn allocations_per_steady_run(app: &mut App) -> usize {
    for _ in 0..WARM_UP_TICKS {
        step(app);
        end_frame(app);
    }
    let executor = executor_allocations(app, FixedUpdate);
    // A fixed array: this bookkeeping must not allocate either.
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
    total
}

/// The control: a system that does allocate is seen, so a zero below means none happened.
#[test]
fn the_count_sees_an_allocation_inside_a_tick() {
    fn allocates() {
        std::hint::black_box(Box::new(1_u64));
    }
    let mut app = app();
    app.add_systems(FixedUpdate, allocates);
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    assert!(allocations_per_steady_run(&mut app) >= MEASURED_TICKS);
}

#[test]
fn a_thousand_trees_allocate_nothing_per_tick() {
    let mut app = app();
    let trees = spawn_trees(&mut app);
    assert_eq!(allocations_per_steady_run(&mut app), 0);
    // The zero is about ticking trees: all of them advanced, and sit in their first leaf.
    for entity in trees {
        let status = app.world().get::<StateTreeStatus>(entity).expect("status");
        assert_eq!(status.ticks, (WARM_UP_TICKS + MEASURED_TICKS) as u64);
        assert_eq!(status.active_path, ["Root", "Idle"]);
    }
}

/// A `hold` counter runs while the guard is true: the status lists it, and the tick still
/// allocates nothing while the label stays the same.
#[test]
fn a_running_hold_allocates_nothing_per_tick() {
    let mut app = app();
    let trees = spawn_trees(&mut app);
    let def = TREE.replace(
        r#"Compare(key: "threat", op: Lt, value: 2.0), hold: 0.25"#,
        r#"Compare(key: "threat", op: Gt, value: 2.0), hold: 1000.0"#,
    );
    let def = StateTreeDef::from_ron(&def).expect("definition");
    for &entity in &trees {
        let tree = StateTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("tree");
        app.world_mut().entity_mut(entity).insert(tree);
    }
    assert_eq!(allocations_per_steady_run(&mut app), 0);
    for entity in trees {
        let status = app.world().get::<StateTreeStatus>(entity).expect("status");
        assert_eq!(status.held, ["Idle -> Chase"]);
    }
}

/// A `Tick` guard that stays true while the target's enter condition stays false. The rejection is
/// traced once; the steady tick must not allocate a candidate list or another trace line.
#[test]
fn a_blocked_enter_allocates_nothing_per_tick() {
    const BLOCKED: &str = r#"(
        format_version: 2,
        root: "Root",
        state_key: "mode",
        states: [
            (name: "Root", children: ["Idle", "Chase"], tasks: ["alloc_test::Hold"]),
            (name: "Idle", tasks: ["alloc_test::Hold"], transitions: [
                (trigger: Tick, condition: Compare(key: "threat", op: Lt, value: 12.0), target: State("Chase")),
            ]),
            (name: "Chase", enter: Key("armed"), tasks: ["alloc_test::Hold"]),
        ],
    )"#;
    let mut app = app();
    let def = StateTreeDef::from_ron(BLOCKED).expect("definition");
    let mut entities = Vec::new();
    for _ in 0..TREES {
        let tree = StateTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("tree");
        let mut board = Blackboard::new();
        board.insert("threat".to_owned(), BbValue::Float(0.0));
        entities.push(app.world_mut().spawn((tree, board)).id());
    }
    assert_eq!(allocations_per_steady_run(&mut app), 0);
    for entity in entities {
        let status = app.world().get::<StateTreeStatus>(entity).expect("status");
        assert_eq!(status.active_path, ["Root", "Idle"]);
        assert_eq!(status.ticks, (WARM_UP_TICKS + MEASURED_TICKS) as u64);
    }
}

/// An event that matches nothing is real work a steady tree does; the inbox empties and refills.
#[test]
fn an_empty_inbox_allocates_nothing_per_tick() {
    let mut app = app();
    let trees = spawn_trees(&mut app);
    for entity in trees {
        app.world_mut()
            .entity_mut(entity)
            .insert(StateTreeInbox::default());
    }
    assert_eq!(allocations_per_steady_run(&mut app), 0);
}
