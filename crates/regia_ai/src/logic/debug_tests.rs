// SPDX-License-Identifier: MPL-2.0
//! Phase-4 debugging tests of spec 102 § 13 (P4.1 breakpoints and step, P4.2 disabled rules and
//! event breakpoints). Fixed steps and counters only.

use super::*;
use crate::blackboard::Blackboard;
use crate::tree::BtTaskRegistry;

const GRAPH: &str = r#"(format_version: 1, locals: [(name: "a", default: Int(0)), (name: "b", default: Int(0))], rules: [
    (name: "high", event: EntityEvent("go"), priority: 10, actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/a")), "op": Literal(String("increment")) }) ]),
    (name: "low", event: EntityEvent("go"), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/b")), "op": Literal(String("increment")) }) ]),
    (name: "tick", event: Timer(every_ticks: 1)),
])"#;

fn spawn(world: &mut World) -> Entity {
    let def = LogicGraphDef::from_ron(GRAPH).expect("parses");
    let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
    world
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id()
}

fn emit(world: &mut World, e: Entity, name: &str) {
    world.get_mut::<LogicInbox>(e).expect("inbox").push(name);
}

fn local(world: &World, e: Entity, name: &str) -> i64 {
    match world
        .get::<LogicGraph>(e)
        .and_then(|g| g.locals().get(name).cloned())
    {
        Some(BbValue::Int(n)) => n,
        other => panic!("local {name}: {other:?}"),
    }
}

fn ticks(world: &World, e: Entity) -> u64 {
    world.get::<LogicGraph>(e).map_or(0, LogicGraph::ticks)
}

fn paused(world: &World, e: Entity) -> bool {
    world.get::<LogicGraph>(e).is_some_and(LogicGraph::is_paused)
}

fn debug(world: &mut World, e: Entity, debug: LogicGraphDebug) {
    world.entity_mut(e).insert(debug);
}

/// P4.1: a `pause_on_rule` breakpoint pauses at the end of the firing tick (the rest of that tick
/// still runs), step advances exactly one tick, resume continues.
#[test]
fn p4_1_a_rule_breakpoint_pauses_at_the_end_of_its_tick_step_runs_one_tick_resume_continues() {
    let mut world = World::new();
    let e = spawn(&mut world);
    tick_logic_graphs(&mut world);
    debug(
        &mut world,
        e,
        LogicGraphDebug {
            pause_on_rule: vec!["high".into()],
            ..Default::default()
        },
    );
    emit(&mut world, e, "go");
    tick_logic_graphs(&mut world);
    // Both rules ran in the firing tick; the pause applies at its end.
    assert_eq!((local(&world, e, "a"), local(&world, e, "b")), (1, 1));
    assert!(paused(&world, e));
    let at_break = ticks(&world, e);
    assert!(
        world
            .get::<LogicGraphStatus>(e)
            .is_some_and(|s| s.paused && s.trace.iter().any(|l| l.ends_with("breakpoint rule high")))
    );
    // Paused: nothing advances, the inbox waits.
    emit(&mut world, e, "go");
    for _ in 0..3 {
        tick_logic_graphs(&mut world);
    }
    assert_eq!(ticks(&world, e), at_break);
    assert_eq!(local(&world, e, "a"), 1);
    // Step: exactly one tick (it delivers the waiting event, whose `high` fire hits the breakpoint
    // again), then paused.
    world.get_mut::<LogicGraph>(e).expect("graph").step();
    tick_logic_graphs(&mut world);
    assert_eq!(ticks(&world, e), at_break + 1);
    assert_eq!(local(&world, e, "a"), 2);
    assert!(paused(&world, e));
    tick_logic_graphs(&mut world);
    assert_eq!(ticks(&world, e), at_break + 1, "one step is one tick");
    // Without the breakpoint, resume continues tick after tick.
    world.entity_mut(e).remove::<LogicGraphDebug>();
    world.get_mut::<LogicGraph>(e).expect("graph").resume();
    emit(&mut world, e, "go");
    tick_logic_graphs(&mut world);
    tick_logic_graphs(&mut world);
    assert!(!paused(&world, e));
    assert_eq!(ticks(&world, e), at_break + 3);
    assert_eq!(local(&world, e, "a"), 3);
    // Step on a running graph: one more tick, then paused.
    world.get_mut::<LogicGraph>(e).expect("graph").step();
    tick_logic_graphs(&mut world);
    tick_logic_graphs(&mut world);
    assert_eq!(ticks(&world, e), at_break + 4);
    assert!(paused(&world, e));
}

/// P4.2: a disabled rule never matches; `pause_on_event` triggers on the named inbox event only
/// (not on a timer, not on another event).
#[test]
fn p4_2_a_disabled_rule_never_matches_and_an_event_breakpoint_needs_its_inbox_event() {
    let mut world = World::new();
    let e = spawn(&mut world);
    debug(
        &mut world,
        e,
        LogicGraphDebug {
            disabled: vec!["high".into()],
            pause_on_event: vec!["stop_here".into()],
            ..Default::default()
        },
    );
    for _ in 0..3 {
        emit(&mut world, e, "go");
        tick_logic_graphs(&mut world);
    }
    assert_eq!((local(&world, e, "a"), local(&world, e, "b")), (0, 3));
    let status = world.get::<LogicGraphStatus>(e).cloned().expect("status");
    let high = status.rules.iter().find(|r| r.name == "high").expect("rule");
    assert_eq!((high.fires, high.matches), (0, 0));
    assert!(!status.trace.iter().any(|l| l.contains("rule_matched high")));
    // Timers fire every tick and `go` arrived three times: no pause.
    assert!(!paused(&world, e));
    emit(&mut world, e, "stop_here");
    tick_logic_graphs(&mut world);
    assert!(paused(&world, e));
    assert!(
        world
            .get::<LogicGraphStatus>(e)
            .is_some_and(|s| s.trace.iter().any(|l| l.ends_with("breakpoint event stop_here")))
    );
    // Clearing the debug component re-enables the rule.
    world.entity_mut(e).remove::<LogicGraphDebug>();
    world.get_mut::<LogicGraph>(e).expect("graph").resume();
    emit(&mut world, e, "go");
    tick_logic_graphs(&mut world);
    assert_eq!(local(&world, e, "a"), 1);
}

/// A reload keeps the debug lists and a pending step.
#[test]
fn a_reload_keeps_debug_lists_and_a_pending_step() {
    let mut world = World::new();
    let e = spawn(&mut world);
    tick_logic_graphs(&mut world);
    let mut graph = world.entity_mut(e).take::<LogicGraph>().expect("graph");
    graph.set_debug(&["low".into()], &["high".into()], &[]);
    graph.step();
    let def = LogicGraphDef::from_ron(GRAPH).expect("parses");
    let next = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
    let mut ctx = crate::task::BtContext {
        world: &mut world,
        entity: e,
        dt: 1.0 / 60.0,
    };
    graph.reload(next, &mut ctx);
    assert_eq!(
        graph.debug_lists(),
        (&["low".to_string()][..], &["high".to_string()][..], &[][..])
    );
    assert!(graph.is_stepping());
    assert!(graph.has_rule("tick") && !graph.has_rule("nope"));
}

/// The debugger reads of ADR-8021 (`LogicScopeView`, behind `editor.logic_watch` and
/// `editor.logic_evaluate`): a condition and a key outside a tick resolve their scopes exactly as a rule does (`local/`, a bare entity key,
/// `world/<board>/`), an undeclared board is `unknown_scope`, and neither read changes the graph.
#[test]
fn w12_evaluate_condition_and_read_key_use_the_rule_scopes() {
    use crate::state_tree::{CompareOp, Condition};
    let text = r#"(format_version: 1, world: ["gate"], locals: [(name: "n", default: Int(3))], rules: [
        (name: "go", event: EntityEvent("go")),
    ])"#;
    let def = LogicGraphDef::from_ron(text).expect("parses");
    let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
    let mut world = World::new();
    let mut bb = Blackboard::new();
    bb.insert("door", 5_i64);
    let e = world.spawn((graph, bb, LogicInbox::default())).id();
    world
        .get_resource_or_insert_with(LogicWorldBlackboards::default)
        .board_mut("gate")
        .insert("open", true);
    let graph = world.get::<LogicGraph>(e).expect("graph");
    let view = graph.scope_view(&world, e);
    let gt = |key: &str, value: f32| Condition::Compare {
        key: key.into(),
        op: CompareOp::Gt,
        value,
    };
    assert_eq!(view.evaluate(&gt("local/n", 2.0)), Ok(true));
    assert_eq!(view.evaluate(&gt("door", 6.0)), Ok(false));
    assert_eq!(
        view.evaluate(&Condition::All(vec![
            Condition::Key("world/gate/open".into()),
            gt("entity/door", 4.0)
        ])),
        Ok(true)
    );
    let err = view
        .evaluate(&Condition::Key("world/other/x".into()))
        .expect_err("undeclared board");
    assert_eq!(err.reason(), "unknown_scope");
    let mut deep = Condition::Always;
    for _ in 0..=crate::state_tree::MAX_CONDITION_DEPTH {
        deep = Condition::Not(Box::new(deep));
    }
    assert_eq!(
        view.evaluate(&deep).map_err(|e| e.reason()),
        Err("condition_too_deep")
    );
    assert_eq!(view.read("local/n"), Ok(Some(&BbValue::Int(3))));
    assert_eq!(view.read("door"), Ok(Some(&BbValue::Int(5))));
    assert_eq!(view.read("world/gate/open"), Ok(Some(&BbValue::Bool(true))));
    assert_eq!(view.read("local/missing"), Ok(None));
    assert_eq!(
        view.read("world/other/x").map_err(|e| e.reason()),
        Err("unknown_scope")
    );
    assert_eq!(graph.ticks(), 0, "reads tick nothing");
}
