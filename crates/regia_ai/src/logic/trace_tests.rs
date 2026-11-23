// SPDX-License-Identifier: MPL-2.0
//! Trace text parity (spec 102 § 9, § 13 P4.3, ADR-7282): a fixed script that produces every trace
//! line kind (`event_received` of each event kind and a dropped one, `rule_matched` with and
//! without a cooldown, `rule_rejected`, `chain_busy`, `action_started`, `action_failed`,
//! `breakpoint rule` / `event`, `inbox_overflow`, `reload_kept`, `reload_restarted`) must print the
//! same text in `LogicGraphStatus::trace` (what `logic.status` and the editor's live view show) and
//! through `LogicGraph::trace` as the text the phase-1 runtime printed. `trace_parity.txt` was
//! written by this test on the base commit (`b39eb23c8`), before the trace became typed records.
//! Fixed steps only.

use super::*;
use crate::blackboard::Blackboard;
use crate::task::{BtStatus, BtTask, BtTaskRegistration};
use crate::tree::BtTaskRegistry;

/// `test::Fail`: always fails.
#[derive(Default)]
struct Fail;
impl BtTask for Fail {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Failure
    }
}
impl BtTaskRegistration for Fail {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self)
    }
    fn task_type_path() -> &'static str {
        "test::Fail"
    }
}

/// `test::Wait3`: running for two ticks, succeeds on the third.
#[derive(Default)]
struct Wait3(u8);
impl BtTask for Wait3 {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        self.0 += 1;
        if self.0 >= 3 {
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }
    fn reset(&mut self, _ctx: &mut BtContext<'_>) {
        self.0 = 0;
    }
}
impl BtTaskRegistration for Wait3 {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self(0))
    }
    fn task_type_path() -> &'static str {
        "test::Wait3"
    }
}

fn registry() -> BtTaskRegistry {
    let mut r = BtTaskRegistry::default();
    r.register::<Fail>();
    r.register::<Wait3>();
    r
}

const SET_HP: &str =
    r#"(task: "logic::SetBlackboard", set: { "key": Literal(String("hp")), "value": Literal(Int(10)) })"#;

fn graph_text(timer_rule: &str, extra: &str) -> String {
    format!(
        r#"(format_version: 1, seed: 5, world: ["w"],
    locals: [(name: "n", default: Int(0)), (name: "log", default: String(""))],
    rules: [
      (name: "boot", event: Start, actions: [{SET_HP}, (task: "logic::SetBlackboard", set: {{ "key": Literal(String("world/w/count")), "op": Literal(String("increment")) }})]),
      (name: "{timer_rule}", event: Timer(every_ticks: 3, jitter_ticks: 2), actions: [(task: "logic::SetBlackboard", set: {{ "key": Literal(String("local/n")), "op": Literal(String("increment")) }})]),
      (name: "slow", event: Timer(every_ticks: 5), cooldown_ticks: 12, actions: [(task: "logic::EmitEvent", set: {{ "event": Literal(String("pong")) }})]),
      (name: "hp", event: BlackboardChanged(key: "hp"), condition: Compare(key: "hp", op: Gt, value: 3.0),
         actions: [(task: "logic::SetBlackboard", set: {{ "key": Literal(String("local/log")), "op": Literal(String("append")), "value": Literal(String("h")) }})]),
      (name: "n_seen", event: BlackboardChanged(key: "local/n"), condition: Compare(key: "local/n", op: Lt, value: 1000.0)),
      (name: "w_seen", event: BlackboardChanged(key: "world/w/count")),
      (name: "ping", event: EntityEvent("ping"), priority: 3, actions: [(task: "test::Wait3"), (task: "logic::EmitEvent", set: {{ "event": Literal(String("pong")) }})]),
      (name: "pong", event: EntityEvent("pong"), condition: Not(Compare(key: "local/n", op: Gt, value: 20.0))),
      (name: "boom", event: EntityEvent("boom"), actions: [(task: "test::Fail")]),
      (name: "bad", event: EntityEvent("bad"), actions: [(task: "logic::SetBlackboard", set: {{ "key": BbRef("missing"), "value": Literal(Float(1.5)) }})]),
      (name: "aim", event: EntityEvent("aim"), actions: [(task: "logic::EmitEvent", set: {{ "event": Literal(String("x")), "target": BbRef("local/nobody") }})]),
      (name: "marker", event: OnSequenceMarker(sequence: "seq/a.seq.ron", marker: "door"), actions: [{SET_HP}]),
      {extra}
    ])"#
    )
}

fn spawn(world: &mut World) -> Entity {
    let def = LogicGraphDef::from_ron(&graph_text("tick3", "")).expect("parses");
    let graph = LogicGraph::compile(&def, &registry()).expect("compiles");
    world
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id()
}

fn emit(world: &mut World, e: Entity, name: &str) {
    world.get_mut::<LogicInbox>(e).expect("inbox").push(name);
}

fn with_graph(world: &mut World, e: Entity, f: impl FnOnce(&mut LogicGraph, &mut BtContext<'_>)) {
    let mut graph = world.entity_mut(e).take::<LogicGraph>().expect("graph");
    f(
        &mut graph,
        &mut BtContext {
            world,
            entity: e,
            dt: 1.0 / 60.0,
        },
    );
    world.entity_mut(e).insert(graph);
}

fn reload(world: &mut World, e: Entity, text: &str) {
    let next =
        LogicGraph::compile(&LogicGraphDef::from_ron(text).expect("parses"), &registry()).expect("compiles");
    with_graph(world, e, |graph, ctx| {
        graph.reload(next, ctx);
    });
}

/// Appends the status trace at `step` and checks that `LogicGraph::trace` prints the same lines.
fn checkpoint(world: &World, e: Entity, step: u64, out: &mut String) {
    let status = world.get::<LogicGraphStatus>(e).expect("status");
    let direct: Vec<String> = world
        .get::<LogicGraph>(e)
        .expect("graph")
        .trace()
        .map(|line| line.to_string())
        .collect();
    assert_eq!(
        direct, status.trace,
        "LogicGraph::trace and the status agree at step {step}"
    );
    out.push_str(&format!("== step {step} ({} lines)\n", status.trace.len()));
    for line in &status.trace {
        out.push_str(line);
        out.push('\n');
    }
}

fn run_script() -> String {
    let mut world = World::new();
    let e = spawn(&mut world);
    let mut out = String::new();
    for i in 0..300u64 {
        if i % 5 == 0 {
            let mut board = world.get_mut::<Blackboard>(e).expect("blackboard");
            let hp = board.get_int("hp");
            board.insert("hp", (hp + 3) % 11);
        }
        if i % 7 == 3 {
            emit(&mut world, e, "ping");
        }
        match i {
            40 => emit(&mut world, e, "boom"),
            41 => emit(&mut world, e, "bad"),
            42 => emit(&mut world, e, "aim"),
            50 => emit(&mut world, e, "noise"),
            51 => emit(&mut world, e, &sequence_marker_event("seq/a.seq.ron", "door")),
            // The step-59 `ping` chain is still running: `chain_busy`.
            60 => emit(&mut world, e, "ping"),
            100 => {
                for k in 0..70 {
                    emit(&mut world, e, &format!("flood{k}"));
                }
            }
            150 => {
                world.entity_mut(e).insert(LogicGraphDebug {
                    pause_on_rule: vec!["n_seen".to_string()],
                    pause_on_event: vec!["boom".to_string()],
                    disabled: vec!["pong".to_string()],
                });
            }
            156 | 158 => with_graph(&mut world, e, |graph, _| graph.step()),
            160 => {
                world.entity_mut(e).remove::<LogicGraphDebug>();
                with_graph(&mut world, e, |graph, _| graph.resume());
            }
            170 => {
                world.entity_mut(e).insert(LogicGraphDebug {
                    pause_on_event: vec!["boom".to_string()],
                    ..Default::default()
                });
                emit(&mut world, e, "boom");
            }
            172 => {
                world.entity_mut(e).remove::<LogicGraphDebug>();
                with_graph(&mut world, e, |graph, _| graph.resume());
            }
            // A rename keeps the running state; the lines traced before it keep the old name.
            200 => reload(&mut world, e, &graph_text("tick3b", "")),
            // A new `Start` rule restarts the graph.
            250 => reload(
                &mut world,
                e,
                &graph_text(
                    "tick3b",
                    r#"(name: "boot2", event: Start, actions: [(task: "test::Wait3")]),"#,
                ),
            ),
            _ => {}
        }
        tick_logic_graphs(&mut world);
        if matches!(
            i,
            30 | 45 | 52 | 61 | 100 | 101 | 152 | 157 | 159 | 165 | 171 | 199 | 200 | 201 | 250 | 251 | 299
        ) {
            checkpoint(&world, e, i, &mut out);
        }
    }
    out
}

#[test]
fn the_trace_prints_the_text_of_the_base_runtime() {
    let got = run_script();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/logic/trace_parity.txt");
    // Only to record the base text (`REGIA_WRITE_TRACE_PARITY=1` on the base commit); never set
    // to make a failing run pass.
    if std::env::var_os("REGIA_WRITE_TRACE_PARITY").is_some() {
        std::fs::write(path, &got).expect("writes the parity file");
        return;
    }
    let want = include_str!("trace_parity.txt");
    for kind in [
        "event_received start",
        "event_received timer:",
        "event_received changed:",
        "(dropped: no rule)",
        "rule_matched",
        "(cooldown ",
        "rule_rejected",
        "chain_busy",
        "action_started",
        "action_failed",
        "breakpoint rule",
        "breakpoint event",
        "inbox_overflow",
        "reload_kept",
        "reload_restarted",
        "sequence_marker:",
    ] {
        assert!(want.contains(kind), "the script covers {kind}");
    }
    if got != want {
        let first = got
            .lines()
            .zip(want.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(got.lines().count().min(want.lines().count()));
        panic!(
            "trace text differs from the base at line {}: got {:?}, want {:?}",
            first + 1,
            got.lines().nth(first),
            want.lines().nth(first)
        );
    }
}

/// The status mirrors by trace identity: a graph installed over another (an editor recompile, not a
/// reload) starts a new trace, and the status shows only its lines, never a mix with the old ones.
/// A reload keeps the identity, so the lines from before it stay.
#[test]
fn a_graph_installed_over_another_rewrites_the_status_trace() {
    let mut world = World::new();
    let e = spawn(&mut world);
    for _ in 0..40 {
        tick_logic_graphs(&mut world);
    }
    let fresh = LogicGraph::compile(
        &LogicGraphDef::from_ron(&graph_text("tick3", "")).expect("parses"),
        &registry(),
    )
    .expect("compiles");
    world.entity_mut(e).insert(fresh);
    tick_logic_graphs(&mut world);
    let status = world.get::<LogicGraphStatus>(e).expect("status").trace.clone();
    let graph: Vec<String> = world.get::<LogicGraph>(e).expect("graph").trace().collect();
    assert_eq!(status, graph);
    assert!(status.iter().all(|l| l.starts_with("tick 1: ")), "{status:?}");
    // A reload continues the same trace: the status keeps the earlier lines.
    tick_logic_graphs(&mut world);
    reload(&mut world, e, &graph_text("tick3b", ""));
    tick_logic_graphs(&mut world);
    let status = world.get::<LogicGraphStatus>(e).expect("status").trace.clone();
    let graph: Vec<String> = world.get::<LogicGraph>(e).expect("graph").trace().collect();
    assert_eq!(status, graph);
    assert!(status.iter().any(|l| l.starts_with("tick 1: ")));
    assert!(status.iter().any(|l| l.ends_with("reload_kept")));
}
