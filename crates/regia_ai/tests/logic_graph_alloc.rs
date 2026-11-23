// SPDX-License-Identifier: MPL-2.0
//! Spec 102 § 13 P4 acceptance 3: 100 logic graphs of 25 compare rules each take zero heap
//! allocations per fixed step in steady state over 1,000 steps, counted with a counting allocator
//! and no wall clock. The steady steps are not idle: rules fire, write the local, entity and world
//! blackboards, the inbox delivers events, and every graph traces lines that its
//! `LogicGraphStatus` mirrors. Every step runs on this thread, so the thread-local count sees the
//! whole of it.
//!
//! The spec's "within the `AiTickBudget` slice" is asserted in the budget's own counted terms (all
//! 100 graphs tick in every step, none deferred); the wall-clock number is the `#[ignore]`d
//! microbenchmark at the end, run by hand (ADR-7282).
//!
//! Its own test binary: it installs a counting global allocator.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_ai::blackboard::{BbValue, Blackboard};
use regia_ai::logic::LogicWorldBlackboards;
use regia_ai::{
    AiImportance, AiPlugin, AiTickBudget, AiTickStats, AiTickTier, BtTaskRegistry, LogicGraph, LogicGraphDef,
    LogicGraphStatus, LogicInbox,
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

const GRAPHS: usize = 100;
const WARM_UP_STEPS: usize = 200;
const MEASURED_STEPS: usize = 1_000;

/// One `logic::SetBlackboard` increment of `key`.
fn inc(key: &str) -> String {
    format!(
        "(task: \"logic::SetBlackboard\", set: {{ \"key\": Literal(String(\"{key}\")), \"op\": Literal(String(\"increment\")) }})"
    )
}

/// The 25-rule graph: every rule has a compare condition. Eight rules carry the work a steady graph
/// does (a timer that increments a local, one that increments an entity key, one that increments a
/// world key, a watch on a local, an inbox event that sets a local from a literal, one rejected
/// every step, a timer that emits an event to its own entity through a `BbRef` target and the rule
/// that takes it); seventeen more timers compare and either match (some inside a cooldown) or are
/// rejected, so the trace takes every common line kind each step.
fn graph_text() -> String {
    let mut rules = vec![
        format!(
            "(name: \"count\", event: Timer(every_ticks: 1), condition: Compare(key: \"local/n\", op: Ge, value: 0.0), actions: [{}])",
            inc("local/n")
        ),
        format!(
            "(name: \"score\", event: Timer(every_ticks: 2), condition: Compare(key: \"hp\", op: Gt, value: 1.0), actions: [{}])",
            inc("score")
        ),
        format!(
            "(name: \"shared\", event: Timer(every_ticks: 3, jitter_ticks: 2), condition: Compare(key: \"hp\", op: Ge, value: 0.0), actions: [{}])",
            inc("world/w/count")
        ),
        "(name: \"never\", event: Timer(every_ticks: 1), condition: Compare(key: \"world/w/count\", op: Lt, value: 0.0))".to_string(),
        "(name: \"seen\", event: BlackboardChanged(key: \"local/n\"), condition: Compare(key: \"local/n\", op: Gt, value: 0.0), actions: [(task: \"logic::SetBlackboard\", set: { \"key\": Literal(String(\"local/m\")), \"op\": Literal(String(\"increment\")), \"value\": Literal(Float(0.5)) })])".to_string(),
        "(name: \"ping\", event: EntityEvent(\"ping\"), condition: Compare(key: \"hp\", op: Gt, value: 0.0), actions: [(task: \"logic::SetBlackboard\", set: { \"key\": Literal(String(\"local/last\")), \"value\": Literal(Int(7)) })])".to_string(),
    ];
    rules.push(
        "(name: \"echo\", event: Timer(every_ticks: 2), condition: Compare(key: \"hp\", op: Gt, value: 0.0), actions: [(task: \"logic::EmitEvent\", set: { \"event\": Literal(String(\"pong\")), \"target\": BbRef(\"me\") })])".to_string(),
    );
    rules.push(
        "(name: \"pong\", event: EntityEvent(\"pong\"), condition: Compare(key: \"local/n\", op: Ge, value: 0.0))".to_string(),
    );
    for i in 0..17 {
        let every = 2 + i % 5;
        let (op, value) = if i % 2 == 0 { ("Gt", 1.0) } else { ("Lt", -1.0) };
        let cooldown = if i % 3 == 0 { 9 } else { 0 };
        rules.push(format!(
            "(name: \"r{i:02}\", event: Timer(every_ticks: {every}), cooldown_ticks: {cooldown}, condition: Compare(key: \"hp\", op: {op}, value: {value}))"
        ));
    }
    assert_eq!(rules.len(), 25);
    format!(
        "(format_version: 1, seed: 11, world: [\"w\"], locals: [(name: \"n\", default: Int(0)), (name: \"m\", default: Float(0.0)), (name: \"last\", default: Int(0))], rules: [{}])",
        rules.join(", ")
    )
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    app.update();
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app
}

fn spawn_graphs(app: &mut App) -> Vec<Entity> {
    let def = LogicGraphDef::from_ron(&graph_text()).expect("definition");
    (0..GRAPHS)
        .map(|_| {
            let graph = LogicGraph::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("graph");
            let mut board = Blackboard::new();
            board.insert("hp", BbValue::Float(40.0));
            let e = app
                .world_mut()
                .spawn((graph, board, LogicInbox::default(), AiImportance::default()))
                .id();
            if let Some(mut board) = app.world_mut().get_mut::<Blackboard>(e) {
                board.insert("me", BbValue::Entity(e.to_bits()));
            }
            e
        })
        .collect()
}

/// What a frame does between fixed steps, outside the counted part: `App::update` empties the
/// removal log (a tick takes each graph out of its entity), and gameplay code sends events. The
/// event names are made here, so the count is what the graphs do with them: the inbox drain, the
/// delivery, the trace and the drop.
fn end_frame(app: &mut App, graphs: &[Entity], step: usize) {
    app.world_mut().clear_trackers();
    for &e in graphs {
        if let Some(mut inbox) = app.world_mut().get_mut::<LogicInbox>(e) {
            if step.is_multiple_of(3) {
                inbox.push("ping");
            }
            if step % 7 == 1 {
                inbox.push("noise");
            }
        }
    }
}

/// What the executor allocates by itself in one run of `label` (see `state_tree_alloc.rs`,
/// ADR-2012).
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

/// Heap allocations in [`MEASURED_STEPS`] steps after [`WARM_UP_STEPS`], above the executor's own.
/// Prints the steps that allocated and the first status trace lines, so a failure names its cause.
fn allocations_per_steady_run(app: &mut App, graphs: &[Entity]) -> usize {
    for step in 0..WARM_UP_STEPS {
        app.world_mut().run_schedule(FixedUpdate);
        end_frame(app, graphs, step);
    }
    let executor = executor_allocations(app, FixedUpdate);
    // A fixed array: this bookkeeping must not allocate either.
    let mut spent = [0; MEASURED_STEPS];
    for (i, count) in spent.iter_mut().enumerate() {
        let before = allocations();
        app.world_mut().run_schedule(FixedUpdate);
        *count = (allocations() - before).abs_diff(executor);
        end_frame(app, graphs, WARM_UP_STEPS + i);
    }
    let total = spent.iter().sum();
    if total > 0 {
        let at: Vec<(usize, usize)> = spent
            .iter()
            .copied()
            .enumerate()
            .filter(|t| t.1 > 0)
            .take(20)
            .collect();
        eprintln!(
            "{total} allocations in {MEASURED_STEPS} steps ({} per graph-step); first steps: {at:?}",
            total as f64 / (MEASURED_STEPS * graphs.len()) as f64
        );
    }
    total
}

/// The graphs did the work the zero is about.
fn assert_busy(app: &App, graphs: &[Entity]) {
    let steps = (WARM_UP_STEPS + MEASURED_STEPS) as u64;
    for &e in graphs {
        let status = app.world().get::<LogicGraphStatus>(e).expect("status");
        assert_eq!(status.ticks, steps);
        let fires = |name: &str| {
            status
                .rules
                .iter()
                .find(|r| r.name == name)
                .map_or(0, |r| r.fires)
        };
        assert_eq!(fires("count"), steps);
        assert!(fires("score") >= steps / 2 - 1, "score {}", fires("score"));
        assert!(fires("shared") > 0 && fires("seen") > 0 && fires("ping") > 0);
        assert!(
            fires("echo") >= steps / 2 - 1 && fires("pong") >= steps / 2 - 1,
            "the emit loop ran"
        );
        assert_eq!(fires("never"), 0);
        assert!(status.trace.iter().any(|l| l.contains("rule_matched")));
        assert!(status.trace.iter().any(|l| l.contains("rule_rejected")));
        assert!(status.trace.iter().any(|l| l.contains("action_started")));
        let local = |name: &str| {
            status
                .locals
                .iter()
                .find(|l| l.name == name)
                .map(|l| l.value.clone())
        };
        assert_eq!(local("n"), Some(BbValue::Int(steps as i64)));
        assert_eq!(local("last"), Some(BbValue::Int(7)));
        let board = app.world().get::<Blackboard>(e).expect("blackboard");
        assert!(board.get_int("score") > 0);
    }
    let shared = app
        .world()
        .resource::<LogicWorldBlackboards>()
        .get("w")
        .map_or(0, |b| b.get_int("count"));
    assert!(shared >= GRAPHS as i64, "the world board was written: {shared}");
}

/// The control: a system that does allocate is seen, so a zero below means none happened.
#[test]
fn the_count_sees_an_allocation_inside_a_step() {
    fn allocates() {
        std::hint::black_box(Box::new(1_u64));
    }
    let mut app = app();
    app.add_systems(FixedUpdate, allocates);
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    assert!(allocations_per_steady_run(&mut app, &[]) >= MEASURED_STEPS);
}

/// P4 acceptance 3: 100 graphs × 25 compare rules, 1,000 steady steps, zero allocations.
#[test]
fn a_hundred_graphs_of_twenty_five_rules_allocate_nothing_per_step() {
    let mut app = app();
    let graphs = spawn_graphs(&mut app);
    assert_eq!(allocations_per_steady_run(&mut app, &graphs), 0);
    assert_busy(&app, &graphs);
}

/// The same under an `AiTickBudget` whose slice holds every graph: each step ticks all 100 (none
/// deferred), and the budgeted path allocates nothing either.
#[test]
fn a_budgeted_hundred_graphs_fit_the_slice_and_allocate_nothing() {
    let mut app = app();
    app.world_mut().insert_resource(AiTickBudget {
        enabled: true,
        max_trees_per_step: GRAPHS as u32,
        tiers: vec![AiTickTier {
            max_distance: f32::MAX,
            cadence_steps: 1,
            max_trees_per_step: 0,
        }],
        focus: None,
    });
    let graphs = spawn_graphs(&mut app);
    assert_eq!(allocations_per_steady_run(&mut app, &graphs), 0);
    assert_busy(&app, &graphs);
    let stats = app.world().resource::<AiTickStats>();
    assert_eq!(stats.ticks_last_step, GRAPHS as u32);
    assert_eq!(stats.deferred_last_step, 0);
    assert_eq!(stats.late_ticks_total, 0);
}

/// The wall-clock side of acceptance 3, run by hand on a quiet host (never by the gate; the
/// number goes to `docs/perf-log.md` from that run):
///
/// ```text
/// cargo test -p regia_ai --release --test logic_graph_alloc -- --ignored --nocapture
/// ```
///
/// The ceiling is generous: the median step of 100 graphs stays under one 60 Hz fixed step.
#[test]
#[ignore = "hand-run microbenchmark (wall clock)"]
fn a_hundred_graphs_tick_inside_one_fixed_step() {
    use std::time::{Duration, Instant};
    let mut app = app();
    let graphs = spawn_graphs(&mut app);
    for step in 0..WARM_UP_STEPS {
        app.world_mut().run_schedule(FixedUpdate);
        end_frame(&mut app, &graphs, step);
    }
    let mut samples: Vec<Duration> = Vec::with_capacity(MEASURED_STEPS);
    for i in 0..MEASURED_STEPS {
        let start = Instant::now();
        app.world_mut().run_schedule(FixedUpdate);
        samples.push(start.elapsed());
        end_frame(&mut app, &graphs, WARM_UP_STEPS + i);
    }
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "perf-micro logic_graph 100x25: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
    assert!(at(0.5) < Duration::from_micros(16_667), "median {:?}", at(0.5));
    assert_busy(&app, &graphs);
}
