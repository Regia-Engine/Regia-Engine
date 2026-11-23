// SPDX-License-Identifier: MPL-2.0
//! Phase-3 acceptance tests of spec 102 § 13 that live in `regia_ai` (world scope, the sequence,
//! net and input event arms, the sim/presentation split). The bridges from the sequencer, the
//! audio cue path, Lua and the net layer are tested where they are registered (`regia_framework`,
//! `regia`). Fixed steps and counters only.

use super::*;
use crate::blackboard::Blackboard;
use crate::tree::BtTaskRegistry;

fn compile(text: &str) -> Result<LogicGraph, LogicGraphError> {
    LogicGraph::compile(&LogicGraphDef::from_ron(text)?, &BtTaskRegistry::default())
}

fn reason(text: &str) -> &'static str {
    match compile(text) {
        Ok(_) => "ok",
        Err(e) => e.reason(),
    }
}

fn spawn(world: &mut World, text: &str) -> Entity {
    let graph = compile(text).expect("compiles");
    world
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id()
}

fn step(world: &mut World) {
    tick_logic_graphs(world);
}

fn fires(world: &World, e: Entity, rule: &str) -> u64 {
    world
        .get::<LogicGraphStatus>(e)
        .and_then(|s| s.rules.iter().find(|r| r.name == rule).map(|r| r.fires))
        .expect("rule status")
}

fn local(world: &World, e: Entity, name: &str) -> Option<BbValue> {
    world
        .get::<LogicGraph>(e)
        .and_then(|g| g.locals().get(name).cloned())
}

fn emit(world: &mut World, e: Entity, name: &str) {
    world.get_mut::<LogicInbox>(e).expect("inbox").push(name);
}

/// The gatehouse graph raises `world/gatehouse/alarm` on an intruder; it counts its raises in the
/// world board too.
const GATEHOUSE: &str = r#"(format_version: 1, world: ["gatehouse"], rules: [
    (name: "raise", event: EntityEvent("intruder"), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("world/gatehouse/alarm")), "value": Literal(Bool(true)) }),
        (task: "logic::SetBlackboard", set: { "key": Literal(String("world/gatehouse/raised")), "op": Literal(String("increment")) }),
    ]),
    (name: "clear", event: EntityEvent("all_clear"), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("world/gatehouse/alarm")), "value": Literal(Bool(false)) }),
    ]),
])"#;

/// The patrol graph, on another entity, reacts to the gatehouse's board: a watch on the world key
/// and a world-scoped condition; it copies a world value into a local through a `BbRef`.
const PATROL: &str = r#"(format_version: 1, world: ["gatehouse"],
    locals: [(name: "responses", default: Int(0)), (name: "seen", default: Int(0))],
    rules: [
    (name: "respond", event: BlackboardChanged(key: "world/gatehouse/alarm"), condition: Key("world/gatehouse/alarm"), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/responses")), "op": Literal(String("increment")) }),
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/seen")), "value": BbRef("world/gatehouse/raised") }),
    ]),
])"#;

#[test]
fn p3_4_two_graphs_coordinate_through_a_declared_world_blackboard() {
    let mut world = World::new();
    let gatehouse = spawn(&mut world, GATEHOUSE);
    let patrol = spawn(&mut world, PATROL);
    step(&mut world);
    emit(&mut world, gatehouse, "intruder");
    step(&mut world);
    // The write lands during this step; the patrol graph sees it on its next tick at the latest
    // (it may tick after the gatehouse in the same step).
    step(&mut world);
    assert_eq!(fires(&world, gatehouse, "raise"), 1);
    assert_eq!(fires(&world, patrol, "respond"), 1, "the patrol saw the alarm");
    assert_eq!(local(&world, patrol, "responses"), Some(BbValue::Int(1)));
    assert_eq!(
        local(&world, patrol, "seen"),
        Some(BbValue::Int(1)),
        "a BbRef reads the world board"
    );
    // Clearing the alarm writes the watched key again, but the condition rejects it.
    emit(&mut world, gatehouse, "all_clear");
    step(&mut world);
    step(&mut world);
    assert_eq!(
        fires(&world, patrol, "respond"),
        1,
        "a false world condition rejects"
    );
    let trace = &world.get::<LogicGraphStatus>(patrol).expect("status").trace;
    assert!(
        trace
            .iter()
            .any(|l| l.contains("rule_rejected respond on changed:world/gatehouse/alarm")),
        "{trace:?}"
    );
}

#[test]
fn p3_4_an_undeclared_or_badly_named_world_scope_is_a_compile_error() {
    // The key names a board the graph does not declare.
    assert_eq!(
        reason(
            r#"(format_version: 1, world: ["gatehouse"], rules: [(name: "s", event: Start, condition: Key("world/keep/x"))])"#
        ),
        "unknown_scope"
    );
    assert_eq!(
        reason(
            r#"(format_version: 1, rules: [(name: "s", event: Start, actions: [(task: "logic::SetBlackboard", set: { "key": Literal(String("world/gatehouse/x")), "value": Literal(Int(1)) })])])"#
        ),
        "unknown_scope"
    );
    assert_eq!(
        reason(
            r#"(format_version: 1, world: ["gatehouse"], rules: [(name: "s", event: BlackboardChanged(key: "world/keep/x"))])"#
        ),
        "unknown_scope"
    );
    // A world key needs a board name and a key.
    assert_eq!(
        reason(
            r#"(format_version: 1, world: ["gatehouse"], rules: [(name: "s", event: Start, condition: Key("world/gatehouse/"))])"#
        ),
        "bad_value"
    );
    assert_eq!(
        reason(
            r#"(format_version: 1, world: ["gatehouse"], rules: [(name: "s", event: Start, condition: Key("world/gatehouse"))])"#
        ),
        "unknown_scope"
    );
    // Board names: non-empty, no `/`, unique.
    assert_eq!(reason(r#"(format_version: 1, world: [""])"#), "bad_value");
    assert_eq!(reason(r#"(format_version: 1, world: ["a/b"])"#), "bad_value");
    assert_eq!(
        reason(r#"(format_version: 1, world: ["g", "g"])"#),
        "duplicate_name"
    );
    assert_eq!(reason(r#"(format_version: 1, world: ["gatehouse"])"#), "ok");
}

#[test]
fn p3_the_sequence_net_and_input_arms_fire_from_their_inbox_events() {
    let text = r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
        (name: "marker", event: OnSequenceMarker(sequence: "door.seq.ron", marker: "creak"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })]),
        (name: "net", event: OnNetMessage(message: "ring")),
        (name: "input", event: InputAction(action: "use")),
    ])"#;
    let mut world = World::new();
    let e = spawn(&mut world, text);
    step(&mut world);
    // Plain names never match an arm: the arms have their own inbox spellings.
    for name in ["creak", "ring", "use", "sequence_marker:other.seq.ron#creak"] {
        emit(&mut world, e, name);
    }
    step(&mut world);
    assert_eq!(fires(&world, e, "marker"), 0);
    emit(&mut world, e, "sequence_marker:door.seq.ron#creak");
    emit(&mut world, e, "net_message:ring");
    emit(&mut world, e, "input_action:use");
    step(&mut world);
    assert_eq!(fires(&world, e, "marker"), 1);
    assert_eq!(fires(&world, e, "net"), 1);
    assert_eq!(fires(&world, e, "input"), 1);
    assert_eq!(local(&world, e, "n"), Some(BbValue::Int(1)));
}

// ---- the sim / presentation split and the parameter hand-over ------------------------------------------

/// `test::Show` (marked presentation): counts its runs in `shown` and keeps its last parameters in
/// `params` (key=value pairs joined by `,`).
#[derive(Default)]
struct Show(Vec<(String, BbValue)>);
impl crate::task::BtTask for Show {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> crate::task::BtStatus {
        let params = self
            .0
            .iter()
            .map(|(k, v)| format!("{k}={v:?}"))
            .collect::<Vec<_>>()
            .join(",");
        if let Some(mut bb) = ctx.blackboard_mut() {
            let n = bb.get_int("shown");
            bb.insert("shown", n + 1);
            bb.insert("params", params);
        }
        crate::task::BtStatus::Success
    }
    fn logic_params(&mut self, params: &[(String, BbValue)]) {
        self.0 = params.to_vec();
    }
}

/// `test::Hit` (a sim task): counts its runs in `hits`.
#[derive(Default)]
struct Hit;
impl crate::task::BtTask for Hit {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> crate::task::BtStatus {
        if let Some(mut bb) = ctx.blackboard_mut() {
            let n = bb.get_int("hits");
            bb.insert("hits", n + 1);
        }
        crate::task::BtStatus::Success
    }
}

fn split_registry() -> BtTaskRegistry {
    let mut r = BtTaskRegistry::default();
    r.register_factory("test::Show", || Box::new(Show::default()));
    r.register_factory("test::Hit", || Box::new(Hit));
    r.mark_presentation("test::Show");
    r
}

const SPLIT: &str = r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
    (name: "go", event: EntityEvent("go"), actions: [
        (task: "test::Hit"),
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) }),
        (task: "test::Show", set: { "cue": Literal(String("creak")), "volume": Literal(Float(0.5)) }),
    ]),
])"#;

fn split_run(authority: bool) -> (i64, i64, Option<BbValue>, Option<BbValue>) {
    let mut world = World::new();
    if !authority {
        world.insert_resource(LogicAuthority(|_, _| false));
    }
    let graph = LogicGraph::compile(
        &LogicGraphDef::from_ron(SPLIT).expect("parses"),
        &split_registry(),
    )
    .expect("compiles");
    let e = world
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id();
    emit(&mut world, e, "go");
    step(&mut world);
    let bb = world.get::<Blackboard>(e).expect("bb");
    (
        bb.get_int("hits"),
        bb.get_int("shown"),
        local(&world, e, "n"),
        bb.get("params").cloned(),
    )
}

#[test]
fn p3_5_a_peer_without_authority_runs_presentation_actions_only() {
    let params = Some(BbValue::String("cue=String(\"creak\"),volume=Float(0.5)".into()));
    assert_eq!(
        split_run(true),
        (1, 1, Some(BbValue::Int(1)), params.clone()),
        "the authority runs everything"
    );
    assert_eq!(
        split_run(false),
        (0, 1, Some(BbValue::Int(0)), params),
        "a proxy skips the sim task and the built-in, but runs the presentation task"
    );
    assert!(split_registry().is_presentation("test::Show"));
    assert!(!split_registry().is_presentation("test::Hit"));
}

#[test]
fn p3_4_the_status_lists_the_declared_world_boards() {
    let mut world = World::new();
    let gatehouse = spawn(&mut world, GATEHOUSE);
    // A board another graph does not declare stays out of this graph's status.
    world
        .get_resource_or_insert_with(LogicWorldBlackboards::default)
        .board_mut("keep")
        .insert("x", 1);
    step(&mut world);
    assert!(
        world
            .resource::<LogicWorldBlackboards>()
            .get("gatehouse")
            .is_some(),
        "a graph creates its declared boards on its first tick"
    );
    emit(&mut world, gatehouse, "intruder");
    step(&mut world);
    let s = world.get::<LogicGraphStatus>(gatehouse).expect("status");
    let names: Vec<&str> = s.world.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["gatehouse/alarm", "gatehouse/raised"]);
    assert_eq!(s.world[0].value, BbValue::Bool(true));
    let before = s.world.as_ptr();
    step(&mut world);
    assert_eq!(
        world
            .get::<LogicGraphStatus>(gatehouse)
            .expect("status")
            .world
            .as_ptr(),
        before,
        "a steady board is not copied again"
    );
}

#[test]
fn p3_the_arm_spellings_come_from_the_helpers() {
    assert_eq!(
        LogicEvent::OnSequenceMarker {
            sequence: "door.seq.ron".into(),
            marker: "creak".into()
        }
        .inbox_name()
        .as_deref(),
        Some("sequence_marker:door.seq.ron#creak")
    );
    assert_eq!(sequence_marker_event("a", "b"), "sequence_marker:a#b");
    assert_eq!(net_message_event("ring"), "net_message:ring");
    assert_eq!(input_action_event("use"), "input_action:use");
    assert_eq!(LogicEvent::Start.inbox_name(), None);
    assert_eq!(
        reason(r#"(format_version: 1, rules: [(name: "s", event: InputAction(action: ""))])"#),
        "bad_value"
    );
}
