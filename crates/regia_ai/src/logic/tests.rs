// SPDX-License-Identifier: MPL-2.0
//! Phase-1 acceptance tests of spec 102 § 13 (P1.1-P1.9, P1.12, P1.13). Fixed steps and counters
//! only: no sleeps, no wall clock.

use super::*;
use crate::blackboard::Blackboard;
use crate::task::{BtStatus, BtTask, BtTaskRegistration};
use crate::tick_budget::{AiTickBudget, AiTickTier};
use crate::tree::BtTaskRegistry;

/// `game::OpenDoor`: succeeds at once and counts its runs in `door_opened`.
#[derive(Default)]
struct OpenDoor;
impl BtTask for OpenDoor {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if let Some(mut bb) = ctx.blackboard_mut() {
            let n = bb.get_int("door_opened");
            bb.insert("door_opened", n + 1);
        }
        BtStatus::Success
    }
}
impl BtTaskRegistration for OpenDoor {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self)
    }
    fn task_type_path() -> &'static str {
        "game::OpenDoor"
    }
}

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

/// `test::SucceedAfter`: running until it has ticked `after` (blackboard int) times; logs each tick
/// in `sa_ticks`.
#[derive(Default)]
struct SucceedAfter(i64);
impl BtTask for SucceedAfter {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        self.0 += 1;
        let Some(mut bb) = ctx.blackboard_mut() else {
            return BtStatus::Failure;
        };
        let total = bb.get_int("sa_ticks");
        bb.insert("sa_ticks", total + 1);
        if self.0 >= bb.get_int("after") {
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }
    fn reset(&mut self, _ctx: &mut BtContext<'_>) {
        self.0 = 0;
    }
}
impl BtTaskRegistration for SucceedAfter {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self(0))
    }
    fn task_type_path() -> &'static str {
        "test::SucceedAfter"
    }
}

/// `test::Latch`: counts its own ticks since its last reset into `latch`.
#[derive(Default)]
struct Latch(i64);
impl BtTask for Latch {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        self.0 += 1;
        if let Some(mut bb) = ctx.blackboard_mut() {
            bb.insert("latch", self.0);
        }
        BtStatus::Success
    }
    fn reset(&mut self, _ctx: &mut BtContext<'_>) {
        self.0 = 0;
    }
}
impl BtTaskRegistration for Latch {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self(0))
    }
    fn task_type_path() -> &'static str {
        "test::Latch"
    }
}

/// `test::RecordDt`: writes the tick's `dt` to `dt`.
#[derive(Default)]
struct RecordDt;
impl BtTask for RecordDt {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let dt = ctx.dt;
        if let Some(mut bb) = ctx.blackboard_mut() {
            bb.insert("dt", dt);
        }
        BtStatus::Success
    }
}
impl BtTaskRegistration for RecordDt {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self)
    }
    fn task_type_path() -> &'static str {
        "test::RecordDt"
    }
}

fn registry() -> BtTaskRegistry {
    let mut r = BtTaskRegistry::default();
    r.register::<OpenDoor>();
    r.register::<Fail>();
    r.register::<SucceedAfter>();
    r.register::<Latch>();
    r.register::<RecordDt>();
    r
}

fn compile(text: &str) -> Result<LogicGraph, LogicGraphError> {
    LogicGraph::compile(&LogicGraphDef::from_ron(text)?, &registry())
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

fn status(world: &World, e: Entity) -> LogicGraphStatus {
    world.get::<LogicGraphStatus>(e).cloned().expect("status")
}

fn rule<'a>(s: &'a LogicGraphStatus, name: &str) -> &'a LogicRuleStatus {
    s.rules.iter().find(|r| r.name == name).expect("rule")
}

fn emit(world: &mut World, e: Entity, name: &str) {
    world.get_mut::<LogicInbox>(e).expect("inbox").push(name);
}

fn bb(world: &mut World, e: Entity) -> Mut<'_, Blackboard> {
    world.get_mut::<Blackboard>(e).expect("blackboard")
}

fn local(world: &World, e: Entity, name: &str) -> Option<BbValue> {
    world
        .get::<LogicGraph>(e)
        .and_then(|g| g.locals().get(name).cloned())
}

fn trace_lines(world: &World, e: Entity, needle: &str) -> Vec<String> {
    status(world, e)
        .trace
        .into_iter()
        .filter(|l| l.contains(needle))
        .collect()
}

fn rules_text(rules: &str) -> String {
    format!("(format_version: 1, locals: [(name: \"n\", default: Int(0))], rules: [{rules}])")
}

const INC_N: &str = "(task: \"logic::SetBlackboard\", set: { \"key\": Literal(String(\"local/n\")), \"op\": Literal(String(\"increment\")) })";

/// The spec 102 § 3 example, verbatim.
const SPEC_EXAMPLE: &str = r#"LogicGraphDef(
  format_version: 1,
  description: "Gatehouse door logic",
  seed: 7,                                  // Timer jitter RNG seed (SplitMix64, § 4.1)
  locals: [ (name: "opened_count", default: Int(0)) ],
  world:   [ "gatehouse" ],                 // declared world blackboards (non-empty is
                                           // `unsupported_feature` until P3, § 13)
  rules: [
    ( name: "open_door",
      event: EntityEvent("trigger_enter"),   // § 4.1
      priority: 10,
      cooldown_ticks: 0,
      condition: All([ Has("door_ref"), Not(Key("locked")) ]),
      actions: [
        ( task: "logic::SetBlackboard", set: { "key": Literal(String("local/opened_count")),
                                               "op": Literal(String("increment")) } ),
        ( task: "game::OpenDoor", set: { "speed": BbRef("local/door_speed") } ),
      ]),
    ( name: "raise_alarm",
      event: EntityEvent("trigger_enter"),
      priority: 20,
      condition: Key("locked"),
      actions: [ ( task: "logic::EmitEvent", set: { "event": Literal(String("alarm")),
                                                    "target": BbRef("entity/door_ref") } ) ]),
  ],
)"#;

// ---- P1.1 compile and limits -------------------------------------------------------------------

#[test]
fn p1_1_the_spec_example_parses_verbatim_and_compiles() {
    let def = LogicGraphDef::from_ron(SPEC_EXAMPLE).expect("the § 3 example parses verbatim");
    assert_eq!(def.rules.len(), 2);
    assert_eq!(def.seed, 7);
    // Since phase 3 the verbatim example compiles, `world:` list included (ADR-7268).
    let graph = LogicGraph::compile(&def, &registry()).expect("compiles verbatim");
    assert_eq!(
        graph.rule_order().collect::<Vec<_>>(),
        ["raise_alarm", "open_door"]
    );
    // Round trip through the canonical writer.
    let again = LogicGraphDef::from_ron(&def.to_ron().expect("writes")).expect("reads");
    assert_eq!(again, def);
}

#[test]
fn p1_1_each_limit_and_problem_fails_with_its_code() {
    let rule = |name: &str| format!("(name: \"{name}\", event: Start)");
    let many = |n: usize| {
        (0..n)
            .map(|i| rule(&format!("r{i}")))
            .collect::<Vec<_>>()
            .join(",")
    };
    assert_eq!(reason(&rules_text(&many(256))), "ok");
    assert_eq!(reason(&rules_text(&many(257))), "too_many_rules");

    let actions = |n: usize| vec![INC_N; n].join(",");
    let with_actions =
        |n: usize| rules_text(&format!("(name: \"a\", event: Start, actions: [{}])", actions(n)));
    assert_eq!(reason(&with_actions(16)), "ok");
    assert_eq!(reason(&with_actions(17)), "too_many_actions");

    let nested = |depth: usize| {
        let mut c = "Key(\"x\")".to_string();
        for _ in 1..depth {
            c = format!("Not({c})");
        }
        rules_text(&format!("(name: \"c\", event: Start, condition: {c})"))
    };
    assert_eq!(reason(&nested(16)), "ok");
    assert_eq!(reason(&nested(17)), "condition_too_deep");

    assert_eq!(
        reason(&rules_text(&format!("{},{}", rule("d"), rule("d")))),
        "duplicate_name"
    );
    assert_eq!(reason(&rules_text(&rule(""))), "bad_value");
    assert_eq!(
        reason(
            "(format_version: 1, locals: [(name: \"x\", default: Int(0)), (name: \"x\", default: Int(1))])"
        ),
        "duplicate_name"
    );
    assert_eq!(
        reason("(format_version: 1, locals: [(name: \"\", default: Int(0))])"),
        "bad_value"
    );
    assert_eq!(
        reason(&rules_text(
            "(name: \"t\", event: Start, actions: [(task: \"game::Nope\")])"
        )),
        "unknown_task"
    );
    assert_eq!(
        reason(&rules_text(
            "(name: \"s\", event: Start, condition: Key(\"elsewhere/x\"))"
        )),
        "unknown_scope"
    );
    assert_eq!(
        reason(&rules_text(
            "(name: \"s\", event: Start, actions: [(task: \"game::OpenDoor\", set: { \"v\": BbRef(\"world/gatehouse/x\") })])"
        )),
        "unknown_scope"
    );
    assert_eq!(
        reason(&rules_text(
            "(name: \"s\", event: Start, actions: [(task: \"logic::SetBlackboard\", set: { \"key\": Literal(String(\"x\")), \"op\": Literal(String(\"multiply\")), \"value\": Literal(Int(2)) })])"
        )),
        "bad_op"
    );
    assert_eq!(
        reason(&rules_text(
            "(name: \"s\", event: Start, actions: [(task: \"logic::EmitEvent\")])"
        )),
        "missing_param"
    );
    assert_eq!(
        reason(&rules_text("(name: \"t\", event: Timer(every_ticks: 0))")),
        "bad_value"
    );
    // The phase-3 arms and the `world:` list compile since phase 3 (ADR-7268); their checks are in
    // `p3_tests`.
    for arm in [
        "OnSequenceMarker(sequence: \"a.seq.ron\", marker: \"m\")",
        "OnNetMessage(message: \"m\")",
        "InputAction(action: \"jump\")",
    ] {
        let text = rules_text(&format!("(name: \"p3\", event: {arm})"));
        LogicGraphDef::from_ron(&text).expect("the arm is in the v1 schema");
        assert_eq!(reason(&text), "ok", "{arm}");
    }
    assert_eq!(reason("(format_version: 1, world: [\"gatehouse\"])"), "ok");
    // One past the current version is refused (v2 since ADR-8900; a v1 file migrates).
    assert_eq!(reason("(format_version: 3)"), "unsupported_version");
    assert_eq!(reason("(format_version: 2)"), "ok");
    assert_eq!(reason("(format_version: 1, rules: [("), "parse");
}

#[test]
fn p1_1_rules_order_by_priority_then_declaration() {
    let graph = compile(&rules_text(
        "(name: \"low\", event: Start), (name: \"high\", event: Start, priority: 5), (name: \"tie\", event: Start, priority: 5), (name: \"neg\", event: Start, priority: -1)",
    ))
    .expect("compiles");
    assert_eq!(
        graph.rule_order().collect::<Vec<_>>(),
        ["high", "tie", "low", "neg"]
    );
}

// ---- P1.2 timers -------------------------------------------------------------------------------

fn fire_ticks(text: &str, steps: u64) -> Vec<u64> {
    let mut world = World::new();
    let e = spawn(&mut world, text);
    let mut out = Vec::new();
    let mut last = Some(BbValue::Int(0));
    for _ in 0..steps {
        step(&mut world);
        let n = local(&world, e, "n");
        if n != last {
            out.push(world.get::<LogicGraph>(e).expect("graph").ticks());
            last = n;
        }
    }
    out
}

/// The pinned SplitMix64 stream of spec 102 § 4.1.
fn splitmix(seed: u64, count: usize) -> Vec<u64> {
    let mut state = seed;
    (0..count)
        .map(|_| {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        })
        .collect()
}

fn timer_text(seed: u64, every: u32, jitter: u32) -> String {
    format!(
        "(format_version: 1, seed: {seed}, locals: [(name: \"n\", default: Int(0))], rules: [(name: \"t\", event: Timer(every_ticks: {every}, jitter_ticks: {jitter}), actions: [{INC_N}])])"
    )
}

#[test]
fn p1_2_a_sixty_tick_timer_fires_at_exactly_sixty_and_one_twenty() {
    assert_eq!(fire_ticks(&timer_text(0, 60, 0), 130), [60, 120]);
}

#[test]
fn p1_2_jitter_draws_follow_the_seed_and_the_pinned_schedule() {
    let a = fire_ticks(&timer_text(7, 60, 10), 400);
    let b = fire_ticks(&timer_text(7, 60, 10), 400);
    let c = fire_ticks(&timer_text(8, 60, 10), 400);
    assert_eq!(a, b, "same seed, same draws");
    assert_ne!(a, c, "another seed draws differently");
    // Period p is due at p * every + draw(p) % (jitter + 1), draw(p) the p-th SplitMix64 output.
    let draws = splitmix(7, 8);
    let expected: Vec<u64> = (1..=6u64)
        .map(|p| p * 60 + draws[(p - 1) as usize] % 11)
        .take_while(|t| *t <= 400)
        .collect();
    assert_eq!(a, expected);
}

#[test]
fn p1_2_a_paused_graph_fires_nothing() {
    let mut world = World::new();
    let e = spawn(&mut world, &timer_text(0, 10, 0));
    world.get_mut::<LogicGraph>(e).expect("graph").pause();
    emit(&mut world, e, "held");
    for _ in 0..100 {
        step(&mut world);
    }
    let s = status(&world, e);
    assert_eq!(rule(&s, "t").fires, 0);
    assert_eq!(s.ticks, 0);
    assert!(s.paused);
    // The inbox waits while paused.
    assert_eq!(world.get::<LogicInbox>(e).expect("inbox").events, ["held"]);
}

#[test]
fn p1_2_a_same_value_write_to_a_watched_key_fires() {
    let text = rules_text(&format!(
        "(name: \"w\", event: BlackboardChanged(key: \"x\"), actions: [{INC_N}])"
    ));
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    bb(&mut world, e).insert("x", 1);
    step(&mut world);
    assert_eq!(
        rule(&status(&world, e), "w").fires,
        0,
        "the start tick takes the baseline"
    );
    bb(&mut world, e).insert("x", 1);
    step(&mut world);
    assert_eq!(
        rule(&status(&world, e), "w").fires,
        1,
        "same value, still a write"
    );
    step(&mut world);
    assert_eq!(rule(&status(&world, e), "w").fires, 1, "no write, no event");
}

// ---- P1.3 BlackboardChanged ----------------------------------------------------------------------

#[test]
fn p1_3_writes_coalesce_per_key_and_keys_come_in_key_order() {
    let text = rules_text(&format!(
        "(name: \"on_b\", event: BlackboardChanged(key: \"b\"), actions: [{INC_N}]), (name: \"on_a\", event: BlackboardChanged(key: \"entity/a\"), actions: [{INC_N}])"
    ));
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    step(&mut world);
    {
        let mut board = bb(&mut world, e);
        board.insert("b", 1);
        board.insert("b", 2);
        board.insert("a", 1);
    }
    step(&mut world);
    let s = status(&world, e);
    assert_eq!(rule(&s, "on_b").fires, 1, "two writes to one key, one event");
    assert_eq!(rule(&s, "on_a").fires, 1);
    let events = trace_lines(&world, e, "event_received changed:");
    assert_eq!(
        events,
        [
            "tick 2: event_received changed:a",
            "tick 2: event_received changed:b"
        ]
    );
}

#[test]
fn p1_3_an_action_write_is_seen_by_the_watch_on_the_next_tick() {
    let text = rules_text(
        "(name: \"set\", event: Start, actions: [(task: \"logic::SetBlackboard\", set: { \"key\": Literal(String(\"x\")), \"value\": Literal(Int(5)) })]),
         (name: \"saw\", event: BlackboardChanged(key: \"x\"))",
    );
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    step(&mut world);
    assert_eq!(rule(&status(&world, e), "saw").fires, 0);
    step(&mut world);
    let s = status(&world, e);
    assert_eq!(rule(&s, "saw").fires, 1);
    assert_eq!(rule(&s, "saw").last_fired_tick, Some(2));
}

// ---- P1.4 inbox -------------------------------------------------------------------------------------

#[test]
fn p1_4_events_drain_fifo_and_an_unmatched_event_is_traced() {
    let text =
        rules_text("(name: \"a\", event: EntityEvent(\"a\")), (name: \"b\", event: EntityEvent(\"b\"))");
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    step(&mut world);
    for name in ["b", "nobody", "a", "b"] {
        emit(&mut world, e, name);
    }
    step(&mut world);
    let received = trace_lines(&world, e, "event_received");
    assert_eq!(
        received,
        [
            "tick 1: event_received start (dropped: no rule)",
            "tick 2: event_received b",
            "tick 2: event_received nobody (dropped: no rule)",
            "tick 2: event_received a",
            "tick 2: event_received b",
        ]
    );
    let s = status(&world, e);
    assert_eq!(rule(&s, "b").fires, 2, "once per event");
    assert_eq!(rule(&s, "a").fires, 1);
}

#[test]
fn p1_4_the_sixty_fifth_event_drops_the_oldest() {
    let text = rules_text(&format!(
        "(name: \"e\", event: EntityEvent(\"e\"), actions: [{INC_N}]), (name: \"first\", event: EntityEvent(\"first\"))"
    ));
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    step(&mut world);
    emit(&mut world, e, "first");
    for _ in 0..64 {
        emit(&mut world, e, "e");
    }
    step(&mut world);
    // The oldest of 65 was dropped (its rule never fired), the other 64 were delivered, nothing panicked.
    assert_eq!(
        trace_lines(&world, e, "inbox_overflow"),
        ["tick 2: inbox_overflow first"]
    );
    let s = status(&world, e);
    assert_eq!(rule(&s, "first").fires, 0);
    assert_eq!(rule(&s, "e").fires, 64);
    assert_eq!(local(&world, e, "n"), Some(BbValue::Int(64)));
}

#[test]
fn p1_4_a_self_emit_loop_advances_one_hop_per_tick() {
    let emit_action = |name: &str| {
        format!("(task: \"logic::EmitEvent\", set: {{ \"event\": Literal(String(\"{name}\")) }})")
    };
    let text = rules_text(&format!(
        "(name: \"A\", event: EntityEvent(\"X\"), actions: [{}]), (name: \"B\", event: EntityEvent(\"Y\"), actions: [{}])",
        emit_action("Y"),
        emit_action("X")
    ));
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    emit(&mut world, e, "X");
    for tick in 1..=10u64 {
        step(&mut world);
        let s = status(&world, e);
        let (a, b) = (rule(&s, "A").fires, rule(&s, "B").fires);
        assert_eq!(a + b, tick, "one hop per tick");
        assert_eq!(a, tick.div_ceil(2));
        // The next hop waits in the inbox: nothing is lost.
        assert_eq!(world.get::<LogicInbox>(e).expect("inbox").events.len(), 1);
    }
    assert!(trace_lines(&world, e, "inbox_overflow").is_empty());
}

// ---- P1.5 ordering ------------------------------------------------------------------------------------

#[test]
fn p1_5_priority_then_declaration_and_a_failure_does_not_stop_the_next_rule() {
    let text = rules_text(&format!(
        "(name: \"low\", event: EntityEvent(\"go\"), actions: [{INC_N}]),
         (name: \"high\", event: EntityEvent(\"go\"), priority: 10, actions: [(task: \"test::Fail\"), {INC_N}]),
         (name: \"tie\", event: EntityEvent(\"go\"), priority: 10, actions: [{INC_N}])"
    ));
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    emit(&mut world, e, "go");
    step(&mut world);
    let matched = trace_lines(&world, e, "rule_matched");
    assert_eq!(
        matched,
        [
            "tick 1: rule_matched high on go",
            "tick 1: rule_matched tie on go",
            "tick 1: rule_matched low on go",
        ]
    );
    assert_eq!(trace_lines(&world, e, "action_failed").len(), 1);
    assert_eq!(
        local(&world, e, "n"),
        Some(BbValue::Int(2)),
        "tie and low ran; high stopped"
    );
}

// ---- P1.6 conditions ------------------------------------------------------------------------------------

#[test]
fn p1_6_conditions_read_local_and_entity_scopes_without_shadowing() {
    let text = r#"(format_version: 1,
        locals: [(name: "x", default: Int(1)), (name: "mode", default: String("calm")), (name: "src", default: Float(2.5))],
        rules: [
          (name: "local_x", event: EntityEvent("go"), condition: Compare(key: "local/x", op: Eq, value: 1.0)),
          (name: "entity_x", event: EntityEvent("go"), condition: Compare(key: "x", op: Eq, value: 2.0)),
          (name: "entity_prefix", event: EntityEvent("go"), condition: Compare(key: "entity/x", op: Gt, value: 1.5)),
          (name: "text", event: EntityEvent("go"), condition: Text(key: "local/mode", value: "calm")),
          (name: "all", event: EntityEvent("go"), condition: All([Key("armed"), Has("local/x"), Not(Has("missing"))])),
          (name: "any", event: EntityEvent("go"), condition: Any([Key("local/missing"), Key("armed")])),
          (name: "rejected", event: EntityEvent("go"), condition: Any([Key("unarmed"), Text(key: "mode", value: "calm")]),
             actions: [(task: "logic::SetBlackboard", set: { "key": Literal(String("ran")), "value": Literal(Bool(true)) })]),
          (name: "copy", event: EntityEvent("go"),
             actions: [(task: "logic::SetBlackboard", set: { "key": Literal(String("copy")), "value": BbRef("local/src") }),
                       (task: "logic::SetBlackboard", set: { "key": Literal(String("local/x")), "value": Literal(Int(9)) })]),
        ])"#;
    let mut world = World::new();
    let e = spawn(&mut world, text);
    {
        let mut board = bb(&mut world, e);
        board.insert("x", 2);
        board.insert("armed", true);
        board.insert("unarmed", false);
    }
    emit(&mut world, e, "go");
    step(&mut world);
    let s = status(&world, e);
    for name in [
        "local_x",
        "entity_x",
        "entity_prefix",
        "text",
        "all",
        "any",
        "copy",
    ] {
        assert_eq!(rule(&s, name).fires, 1, "{name} matches");
    }
    assert_eq!(rule(&s, "rejected").fires, 0);
    assert_eq!(
        trace_lines(&world, e, "rule_rejected"),
        ["tick 1: rule_rejected rejected on go"]
    );
    let board = world.get::<Blackboard>(e).expect("bb");
    assert!(!board.contains("ran"), "no action of a rejected rule runs");
    assert_eq!(
        board.get("copy"),
        Some(&BbValue::Float(2.5)),
        "BbRef dereferenced"
    );
    assert_eq!(board.get("x"), Some(&BbValue::Int(2)), "local/x is another store");
    assert_eq!(local(&world, e, "x"), Some(BbValue::Int(9)));
}

// ---- P1.7 action chains ------------------------------------------------------------------------------------

#[test]
fn p1_7_a_chain_suspends_on_a_running_task_and_resumes_one_step_per_tick() {
    let text = r#"(format_version: 1, rules: [
        (name: "chain", event: EntityEvent("go"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/a")), "value": Literal(Int(1)) }),
            (task: "test::SucceedAfter", set: { "after": Literal(Int(3)) }),
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/b")), "value": Literal(Int(1)) }),
        ]),
        (name: "other", event: EntityEvent("go"), actions: [(task: "test::Fail")]),
    ])"#;
    let mut world = World::new();
    let e = spawn(&mut world, text);
    emit(&mut world, e, "go");
    step(&mut world);
    assert_eq!(local(&world, e, "a"), Some(BbValue::Int(1)));
    assert_eq!(
        world.get::<Blackboard>(e).expect("bb").get("after"),
        Some(&BbValue::Int(3)),
        "set literal written first"
    );
    assert_eq!(world.get::<Blackboard>(e).expect("bb").get_int("sa_ticks"), 1);
    assert!(rule(&status(&world, e), "chain").chain_busy);
    // A second fire while suspended is dropped.
    emit(&mut world, e, "go");
    step(&mut world);
    assert_eq!(trace_lines(&world, e, "chain_busy"), ["tick 2: chain_busy chain"]);
    assert_eq!(
        world.get::<Blackboard>(e).expect("bb").get_int("sa_ticks"),
        2,
        "one task tick per step"
    );
    assert_eq!(local(&world, e, "b"), None);
    step(&mut world);
    assert_eq!(world.get::<Blackboard>(e).expect("bb").get_int("sa_ticks"), 3);
    assert_eq!(
        local(&world, e, "b"),
        Some(BbValue::Int(1)),
        "resumed and finished on tick 3"
    );
    let s = status(&world, e);
    assert!(!rule(&s, "chain").chain_busy);
    assert_eq!(rule(&s, "chain").fires, 1);
    // The failing rule failed alone, each time it fired.
    assert_eq!(trace_lines(&world, e, "action_failed other").len(), 2);
}

#[test]
fn p1_7_reset_runs_on_re_fire_and_a_failure_stops_only_its_chain() {
    let text = rules_text(
        r#"(name: "latch", event: EntityEvent("go"), actions: [(task: "test::Latch")]),
           (name: "fails", event: EntityEvent("go"), actions: [(task: "test::Fail"), (task: "logic::SetBlackboard", set: { "key": Literal(String("after_fail")), "value": Literal(Bool(true)) })])"#,
    );
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    for _ in 0..3 {
        emit(&mut world, e, "go");
        step(&mut world);
        assert_eq!(
            world.get::<Blackboard>(e).expect("bb").get_int("latch"),
            1,
            "a fresh task each fire"
        );
    }
    assert!(!world.get::<Blackboard>(e).expect("bb").contains("after_fail"));
    assert_eq!(rule(&status(&world, e), "latch").fires, 3);
}

// ---- P1.8 hot reload ------------------------------------------------------------------------------------

fn reload_text(text: &str, world: &mut World, e: Entity) -> LogicReloadOutcome {
    let next = compile(text).expect("compiles");
    let mut graph = world.entity_mut(e).take::<LogicGraph>().expect("graph");
    let outcome = graph.reload(
        next,
        &mut BtContext {
            world,
            entity: e,
            dt: 1.0 / 60.0,
        },
    );
    world.entity_mut(e).insert(graph);
    outcome
}

#[test]
fn p1_8_a_name_preserving_edit_keeps_locals_inbox_and_fire_counts() {
    let v1 = format!(
        "(format_version: 1, locals: [(name: \"n\", default: Int(0)), (name: \"gone\", default: Int(7))], rules: [(name: \"r\", event: EntityEvent(\"go\"), actions: [{INC_N}]), (name: \"s\", event: Start)])"
    );
    let v2 = format!(
        "(format_version: 1, locals: [(name: \"n\", default: Int(0)), (name: \"fresh\", default: Int(3))], rules: [(name: \"s\", event: Start), (name: \"r\", event: EntityEvent(\"go\"), priority: 4, actions: [{INC_N}, {INC_N}])])"
    );
    let mut world = World::new();
    let e = spawn(&mut world, &v1);
    for _ in 0..3 {
        emit(&mut world, e, "go");
        step(&mut world);
    }
    emit(&mut world, e, "go");
    world.get_mut::<LogicGraph>(e).expect("graph").send_event("go");
    assert_eq!(reload_text(&v2, &mut world, e), LogicReloadOutcome::Kept);
    {
        let g = world.get::<LogicGraph>(e).expect("graph");
        assert_eq!(g.locals().get("n"), Some(&BbValue::Int(3)), "kept by name");
        assert_eq!(
            g.locals().get("fresh"),
            Some(&BbValue::Int(3)),
            "new local takes its default"
        );
        assert!(g.locals().get("gone").is_none(), "removed local vanishes");
        assert_eq!(g.ticks(), 3, "tick count kept");
    }
    step(&mut world);
    let s = status(&world, e);
    assert_eq!(
        rule(&s, "r").fires,
        5,
        "3 kept + the inbox event + the pending event"
    );
    assert_eq!(rule(&s, "s").fires, 1, "the Start rule did not run again");
    assert_eq!(local(&world, e, "n"), Some(BbValue::Int(7)));
    assert_eq!(trace_lines(&world, e, "reload_kept").len(), 1);
}

#[test]
fn p1_8_removing_a_rule_resets_its_counters_and_running_state_that_cannot_carry_restarts() {
    let with_r = rules_text(&format!(
        "(name: \"r\", event: EntityEvent(\"go\"), actions: [{INC_N}])"
    ));
    let without_r = rules_text("(name: \"q\", event: EntityEvent(\"go\"))");
    let mut world = World::new();
    let e = spawn(&mut world, &with_r);
    emit(&mut world, e, "go");
    step(&mut world);
    assert_eq!(reload_text(&without_r, &mut world, e), LogicReloadOutcome::Kept);
    assert_eq!(reload_text(&with_r, &mut world, e), LogicReloadOutcome::Kept);
    step(&mut world);
    assert_eq!(
        rule(&status(&world, e), "r").fires,
        0,
        "re-added rule starts from zero"
    );

    // A suspended chain whose rule disappears: restart.
    let busy = r#"(format_version: 1, rules: [(name: "slow", event: EntityEvent("go"), actions: [(task: "test::SucceedAfter", set: { "after": Literal(Int(10)) })])])"#;
    let e2 = spawn(&mut world, busy);
    emit(&mut world, e2, "go");
    step(&mut world);
    assert!(rule(&status(&world, e2), "slow").chain_busy);
    assert_eq!(
        reload_text(&rules_text("(name: \"other\", event: Start)"), &mut world, e2),
        LogicReloadOutcome::Restarted
    );
    step(&mut world);
    let s = status(&world, e2);
    assert_eq!(s.ticks, 1, "started again");
    assert_eq!(rule(&s, "other").fires, 1, "Start ran on the restarted graph");
    assert_eq!(trace_lines(&world, e2, "reload_restarted").len(), 1);

    // A new Start rule must run: restart.
    let e3 = spawn(
        &mut world,
        &rules_text("(name: \"x\", event: EntityEvent(\"go\"))"),
    );
    step(&mut world);
    assert_eq!(
        reload_text(
            &rules_text("(name: \"x\", event: EntityEvent(\"go\")), (name: \"boot\", event: Start)"),
            &mut world,
            e3
        ),
        LogicReloadOutcome::Restarted
    );
}

#[test]
fn p1_8_a_suspended_chain_of_a_kept_rule_carries_its_running_task() {
    let v1 = r#"(format_version: 1, locals: [(name: "done", default: Bool(false))], rules: [
        (name: "slow", event: EntityEvent("go"), actions: [
            (task: "test::SucceedAfter", set: { "after": Literal(Int(3)) }),
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/done")), "value": Literal(Bool(true)) }),
        ])])"#;
    let v2 = r#"(format_version: 1, locals: [(name: "done", default: Bool(false))], rules: [
        (name: "extra", event: EntityEvent("other")),
        (name: "slow", event: EntityEvent("go"), priority: 2, actions: [
            (task: "test::SucceedAfter", set: { "after": Literal(Int(3)) }),
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/done")), "value": Literal(Bool(true)) }),
        ])])"#;
    let mut world = World::new();
    let e = spawn(&mut world, v1);
    emit(&mut world, e, "go");
    step(&mut world);
    assert!(rule(&status(&world, e), "slow").chain_busy);
    assert_eq!(reload_text(v2, &mut world, e), LogicReloadOutcome::Kept);
    step(&mut world);
    assert_eq!(local(&world, e, "done"), Some(BbValue::Bool(false)));
    step(&mut world);
    // The task kept its two earlier ticks: its third tick (tick 3) succeeds and the chain finishes.
    assert_eq!(local(&world, e, "done"), Some(BbValue::Bool(true)));
    assert_eq!(world.get::<Blackboard>(e).expect("bb").get_int("sa_ticks"), 3);
}

#[test]
fn p1_8_a_queued_reload_swaps_in_at_the_start_of_the_next_tick_and_is_logged() {
    let v1 = rules_text(&format!(
        "(name: \"r\", event: EntityEvent(\"go\"), actions: [{INC_N}])"
    ));
    let v2 = rules_text(&format!(
        "(name: \"r\", event: EntityEvent(\"go\"), actions: [{INC_N}, {INC_N}])"
    ));
    let mut world = World::new();
    world.init_resource::<LogicReloads>();
    let e = spawn(&mut world, &v1);
    let gone = spawn(&mut world, &v1);
    step(&mut world);
    let next = compile(&v2).expect("compiles");
    let orphan = compile(&v2).expect("compiles");
    {
        let mut reloads = world.resource_mut::<LogicReloads>();
        reloads.queue(e, "logic/door.logic.ron", next);
        reloads.queue(gone, "logic/door.logic.ron", orphan);
        reloads.reject(e, "logic/bad.logic.ron", "unknown_task");
    }
    world.despawn(gone);
    // Queued, not swapped: the event this tick already runs the new two-action chain because the
    // swap happens at the start of the tick, before the events.
    emit(&mut world, e, "go");
    assert_eq!(world.resource::<LogicReloads>().pending(), 2);
    step(&mut world);
    assert_eq!(local(&world, e, "n"), Some(BbValue::Int(2)));
    let reloads = world.resource::<LogicReloads>();
    assert_eq!(reloads.pending(), 0, "the despawned entity's graph was dropped");
    assert_eq!((reloads.applied, reloads.rejected), (1, 1));
    let log: Vec<_> = reloads
        .log()
        .map(|r| (r.source.as_str(), r.result.clone()))
        .collect();
    assert_eq!(
        log,
        [
            ("logic/bad.logic.ron", Err("unknown_task".to_string())),
            ("logic/door.logic.ron", Ok(LogicReloadOutcome::Kept)),
        ]
    );
    assert_eq!(
        trace_lines(&world, e, "reload_kept"),
        ["tick 1: reload_kept"],
        "swapped before tick 2 counted"
    );
}

#[test]
fn p1_8_a_definition_that_fails_to_compile_never_reaches_reload() {
    let mut world = World::new();
    let e = spawn(&mut world, &timer_text(0, 1, 0));
    step(&mut world);
    let bad = compile(
        "(format_version: 1, rules: [(name: \"t\", event: Start, actions: [(task: \"game::Nope\")])])",
    )
    .err()
    .expect("fails");
    assert_eq!(bad.reason(), "unknown_task");
    step(&mut world);
    assert_eq!(
        rule(&status(&world, e), "t").fires,
        2,
        "the old graph keeps running"
    );
}

// ---- P1.9 determinism ------------------------------------------------------------------------------------

const BUSY: &str = r#"(format_version: 1, seed: 99,
    locals: [(name: "n", default: Int(0)), (name: "log", default: String(""))],
    rules: [
      (name: "boot", event: Start, actions: [(task: "logic::SetBlackboard", set: { "key": Literal(String("hp")), "value": Literal(Int(10)) })]),
      (name: "tick7", event: Timer(every_ticks: 7, jitter_ticks: 5), actions: [(task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })]),
      (name: "tick13", event: Timer(every_ticks: 13, jitter_ticks: 9), cooldown_ticks: 20, actions: [(task: "logic::EmitEvent", set: { "event": Literal(String("pong")) })]),
      (name: "hp", event: BlackboardChanged(key: "hp"), condition: Compare(key: "hp", op: Gt, value: 3.0),
         actions: [(task: "logic::SetBlackboard", set: { "key": Literal(String("local/log")), "op": Literal(String("append")), "value": Literal(String("h")) })]),
      (name: "ping", event: EntityEvent("ping"), priority: 3, actions: [(task: "test::SucceedAfter", set: { "after": Literal(Int(4)) }), (task: "logic::EmitEvent", set: { "event": Literal(String("pong")) })]),
      (name: "pong", event: EntityEvent("pong"), condition: Not(Compare(key: "local/n", op: Gt, value: 200.0)), actions: [(task: "test::Latch")]),
    ])"#;

fn deterministic_run() -> String {
    let mut world = World::new();
    let e = spawn(&mut world, BUSY);
    let mut out = String::new();
    for i in 0..2000u64 {
        if i % 5 == 0 {
            let mut board = bb(&mut world, e);
            let hp = board.get_int("hp");
            board.insert("hp", (hp + 3) % 11);
        }
        if i % 7 == 3 {
            emit(&mut world, e, "ping");
        }
        step(&mut world);
        if i % 250 == 249 {
            out.push_str(&format!("{:?}\n", status(&world, e)));
        }
    }
    out.push_str(&format!("{:?}", status(&world, e)));
    out
}

#[test]
fn p1_9_two_thousand_steps_give_byte_identical_status_and_trace() {
    let a = deterministic_run();
    let b = deterministic_run();
    assert!(a.contains("rule_matched"), "the run did something");
    assert_eq!(a, b);
}

// ---- P1.12 budget ------------------------------------------------------------------------------------

fn budget_step(world: &mut World) {
    crate::tick_behavior_trees(world);
    crate::state_tree::tick_state_trees(world);
    tick_logic_graphs(world);
}

fn cadence_16(world: &mut World) {
    world.insert_resource(AiTickBudget {
        enabled: true,
        max_trees_per_step: 0,
        tiers: vec![AiTickTier {
            max_distance: f32::MAX,
            cadence_steps: 16,
            max_trees_per_step: 0,
        }],
        focus: None,
    });
}

#[test]
fn p1_12_a_cadence_sixteen_graph_fires_its_sixty_step_timer_on_step_sixty() {
    let mut world = World::new();
    cadence_16(&mut world);
    let e = spawn(&mut world, &timer_text(0, 60, 0));
    let mut fired_on = Vec::new();
    for fixed_step in 1..=130u64 {
        budget_step(&mut world);
        if local(&world, e, "n") != Some(BbValue::Int(fired_on.len() as i64)) {
            fired_on.push(fixed_step);
        }
    }
    assert_eq!(fired_on, [60, 120]);
    let s = status(&world, e);
    assert_eq!(rule(&s, "t").last_fired_tick, Some(120));
    assert_eq!(
        s.ticks, 120,
        "graph ticks count fixed steps: the last run was the timer's wake on step 120"
    );
    let stats = world.resource::<crate::AiTickStats>();
    assert!(
        stats.ticks_total < 30,
        "the graph ran on its cadence, not every step ({})",
        stats.ticks_total
    );
}

#[test]
fn p1_12_a_deferred_graph_gets_accumulated_dt_and_coalesced_writes() {
    let text = rules_text(
        "(name: \"w\", event: BlackboardChanged(key: \"x\"), actions: [(task: \"test::RecordDt\")])",
    );
    let mut world = World::new();
    cadence_16(&mut world);
    let e = spawn(&mut world, &text);
    budget_step(&mut world); // step 1: the graph's start tick
    for _ in 0..15 {
        bb(&mut world, e).insert("x", 1);
        budget_step(&mut world); // steps 2..16: deferred
    }
    assert_eq!(rule(&status(&world, e), "w").fires, 0);
    budget_step(&mut world); // step 17: due
    let s = status(&world, e);
    assert_eq!(rule(&s, "w").fires, 1, "fifteen writes, one event");
    let dt = world.get::<Blackboard>(e).expect("bb").get_float("dt");
    assert!((dt - 16.0 / 60.0).abs() < 1e-5, "accumulated dt {dt}");
    assert_eq!(s.evaluations, 1);
    assert!(u64::from(s.evaluations) <= (MAX_PENDING_EVENTS as u64 + 2) * MAX_RULES as u64);
}

// ---- P1.13 cooldown ------------------------------------------------------------------------------------

#[test]
fn p1_13_a_cooldown_matches_but_fires_once_per_period_and_restart_resets_it() {
    let text = rules_text(&format!(
        "(name: \"c\", event: Timer(every_ticks: 1), cooldown_ticks: 120, actions: [{INC_N}])"
    ));
    let mut world = World::new();
    let e = spawn(&mut world, &text);
    for _ in 0..360 {
        step(&mut world);
    }
    let s = status(&world, e);
    assert_eq!(rule(&s, "c").fires, 3, "ticks 1, 121, 241");
    assert_eq!(rule(&s, "c").matches, 360, "matched every tick");
    assert_eq!(rule(&s, "c").last_fired_tick, Some(241));
    assert_eq!(rule(&s, "c").cooldown_remaining, 1);
    let mut graph = world.entity_mut(e).take::<LogicGraph>().expect("graph");
    graph.restart(&mut BtContext {
        world: &mut world,
        entity: e,
        dt: 1.0 / 60.0,
    });
    world.entity_mut(e).insert(graph);
    step(&mut world);
    let s = status(&world, e);
    assert_eq!(rule(&s, "c").fires, 4, "restart reset the cooldown");
    assert_eq!(
        local(&world, e, "n"),
        Some(BbValue::Int(1)),
        "restart reset the locals"
    );
}

// ---- built-in actions ------------------------------------------------------------------------------------

#[test]
fn emit_targets_another_graph_and_disable_enable_toggle_the_entity() {
    let receiver_text = rules_text(&format!(
        "(name: \"heard\", event: EntityEvent(\"alarm\"), actions: [{INC_N}])"
    ));
    let sender_text = rules_text(
        r#"(name: "send", event: EntityEvent("go"), actions: [
              (task: "logic::EmitEvent", set: { "event": Literal(String("alarm")), "target": BbRef("door") }),
              (task: "logic::DisableEntity", set: { "target": BbRef("door") }),
           ]),
           (name: "wake", event: EntityEvent("wake"), actions: [(task: "logic::EnableEntity", set: { "target": BbRef("door") })]),
           (name: "bad", event: EntityEvent("bad"), actions: [(task: "logic::EmitEvent", set: { "event": Literal(String("x")), "target": BbRef("nothing") })])"#,
    );
    let mut world = World::new();
    let receiver = spawn(&mut world, &receiver_text);
    let sender = spawn(&mut world, &sender_text);
    bb(&mut world, sender).insert("door", receiver);
    emit(&mut world, sender, "go");
    emit(&mut world, sender, "bad");
    step(&mut world);
    assert!(
        world
            .get::<regia_ecs::ecs::entity_disabling::Disabled>(receiver)
            .is_some()
    );
    assert_eq!(
        world.get::<LogicInbox>(receiver).expect("inbox").events,
        ["alarm"]
    );
    assert_eq!(trace_lines(&world, sender, "action_failed bad").len(), 1);
    emit(&mut world, sender, "wake");
    step(&mut world);
    assert!(
        world
            .get::<regia_ecs::ecs::entity_disabling::Disabled>(receiver)
            .is_none()
    );
    step(&mut world);
    assert_eq!(
        local(&world, receiver, "n"),
        Some(BbValue::Int(1)),
        "delivered once enabled"
    );
}

#[test]
fn a_peer_without_authority_skips_sim_actions_but_still_ticks() {
    let text = rules_text(&format!("(name: \"s\", event: Start, actions: [{INC_N}])"));
    let mut world = World::new();
    world.insert_resource(LogicAuthority(|_, _| false));
    let e = spawn(&mut world, &text);
    step(&mut world);
    let s = status(&world, e);
    assert_eq!(rule(&s, "s").fires, 1);
    assert_eq!(local(&world, e, "n"), Some(BbValue::Int(0)));
}

#[test]
fn the_status_refresh_reuses_a_steady_trace() {
    let mut world = World::new();
    let e = spawn(&mut world, &rules_text("(name: \"s\", event: Start)"));
    step(&mut world);
    let before = world.get::<LogicGraphStatus>(e).expect("status").trace.as_ptr();
    for _ in 0..10 {
        step(&mut world);
    }
    let after = world.get::<LogicGraphStatus>(e).expect("status").trace.as_ptr();
    assert_eq!(before, after, "no new trace line, no rebuilt list");
}

#[test]
fn the_schema_fingerprint_lists_every_arm() {
    let f = logic_schema_fingerprint();
    for arm in [
        "Start",
        "Timer",
        "BlackboardChanged",
        "EntityEvent",
        "OnSequenceMarker",
        "OnNetMessage",
        "InputAction",
        "BbRef",
    ] {
        assert!(f.contains(arm), "{arm}");
    }
    // v2 (ADR-8900) hashes the declaration lists and their structs, and migrates from v1.
    for field in ["entity_vars", "world_vars", "board"] {
        assert!(f.contains(field), "{field}");
    }
    assert_eq!(logic_migration_sources(), vec![1]);
}
