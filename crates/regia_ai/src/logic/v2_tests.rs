// SPDX-License-Identifier: MPL-2.0
//! `.logic.ron` v2 (ADR-8900, P-177 / L9): typed entity and world variables. A v1 file reads as v2
//! (the number only, idempotent), a newer one is refused; declared defaults are seeded when the
//! graph starts, never over a value already there and without raising `BlackboardChanged`; the
//! compiler refuses malformed declarations and a `logic::SetBlackboard` that would change a
//! declared variable's type. Fixed steps and counters only.

use super::*;
use crate::blackboard::Blackboard;
use crate::state_tree::Literal;
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

fn fires(world: &World, e: Entity, rule: &str) -> u64 {
    world
        .get::<LogicGraphStatus>(e)
        .and_then(|s| s.rules.iter().find(|r| r.name == rule).map(|r| r.fires))
        .expect("rule status")
}

/// The v1 corpus fixture's shape (`format-corpus/logic/v1.ron` is the fuzz registry's example).
const V1: &str = r#"(format_version: 1, description: "door", locals: [(name: "n", default: Int(0))], world: ["gate"], rules: [
    (name: "go", event: EntityEvent("go"), actions: [
        (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })]),
])"#;

#[test]
fn a_v1_file_reads_as_v2_and_writes_back_unchanged_but_for_the_number() {
    assert_eq!(LOGIC_FORMAT_VERSION, 2);
    assert_eq!(logic_migration_sources(), vec![1]);
    let def = LogicGraphDef::from_ron(V1).expect("v1 reads");
    assert_eq!(def.format_version, LOGIC_FORMAT_VERSION);
    assert!(def.entity_vars.is_empty() && def.world_vars.is_empty());
    let once = def.to_ron().expect("writes");
    assert!(once.contains("format_version: 2"), "{once}");
    assert!(
        !once.contains("entity_vars") && !once.contains("world_vars"),
        "empty lists are not written: {once}"
    );
    let twice = LogicGraphDef::from_ron(&once)
        .expect("v2 reads")
        .to_ron()
        .expect("writes");
    assert_eq!(once, twice, "the migration is idempotent");
    // A v1 definition built in code still compiles.
    let mut v1 = def.clone();
    v1.format_version = 1;
    LogicGraph::compile(&v1, &BtTaskRegistry::default()).expect("v1 in memory compiles");
}

#[test]
fn a_newer_or_zero_version_is_refused_with_the_version_error() {
    for found in [0, 3, 99] {
        let text = format!("(format_version: {found}, rules: [])");
        assert_eq!(
            LogicGraphDef::from_ron(&text),
            Err(LogicGraphError::UnsupportedVersion {
                found,
                supported: LOGIC_FORMAT_VERSION,
            })
        );
    }
    // The v1 build's error for a v2 file, as `docs/formats/logic-ron.md` documents it.
    let err = LogicGraphError::UnsupportedVersion {
        found: 2,
        supported: 1,
    };
    assert_eq!(
        err.to_string(),
        "logic graph format_version 2 is not supported (this build reads version 1)"
    );
}

const TYPED: &str = r#"(format_version: 2, world: ["score"],
    entity_vars: [(name: "hp", default: Int(100)), (name: "tag", default: String("guard"))],
    world_vars: [(board: "score", name: "red", default: Int(0)), (board: "score", name: "ratio", default: Float(0.5))],
    rules: [
        (name: "watch_hp", event: BlackboardChanged(key: "hp")),
        (name: "watch_red", event: BlackboardChanged(key: "world/score/red")),
        (name: "hit", event: EntityEvent("hit"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("hp")), "value": Literal(Int(90)) })]),
    ])"#;

#[test]
fn declared_defaults_seed_on_start_without_overwriting_or_firing_watches() {
    let mut world = World::new();
    let graph = || compile(TYPED).expect("compiles");
    // `tag` is already on the entity: it keeps its value. The second entity has no blackboard.
    let mut bb = Blackboard::new();
    bb.insert("tag", BbValue::from("captain"));
    let a = world.spawn((graph(), bb, LogicInbox::default())).id();
    let b = world.spawn((graph(), LogicInbox::default())).id();
    tick_logic_graphs(&mut world);
    let a_bb = world.get::<Blackboard>(a).expect("blackboard");
    assert_eq!(a_bb.get("hp"), Some(&Literal::Int(100).to_value()));
    assert_eq!(
        a_bb.get("tag"),
        Some(&BbValue::from("captain")),
        "an existing value is kept"
    );
    let b_bb = world.get::<Blackboard>(b).expect("the seed adds a blackboard");
    assert_eq!(b_bb.get("tag"), Some(&Literal::String("guard".into()).to_value()));
    let boards = world.resource::<LogicWorldBlackboards>();
    let score = boards.boards.get("score").expect("board");
    assert_eq!(score.get("red"), Some(&Literal::Int(0).to_value()));
    assert_eq!(score.get("ratio"), Some(&Literal::Float(0.5).to_value()));
    // A host value on the board survives a later start (a restart re-seeds missing keys only).
    world
        .resource_mut::<LogicWorldBlackboards>()
        .boards
        .get_mut("score")
        .expect("board")
        .insert("red", BbValue::from(7_i64));
    for _ in 0..3 {
        tick_logic_graphs(&mut world);
    }
    assert_eq!(
        fires(&world, a, "watch_hp"),
        0,
        "the seed raised no BlackboardChanged"
    );
    // The host's write to `red` after the start is a real change: both graphs see it once.
    assert_eq!(fires(&world, a, "watch_red"), 1);
    assert_eq!(fires(&world, b, "watch_red"), 1);
    let score = &world.resource::<LogicWorldBlackboards>().boards["score"];
    assert_eq!(score.get("red"), Some(&BbValue::from(7_i64)));
    // A write of the declared type compiles and runs.
    world.get_mut::<LogicInbox>(a).expect("inbox").push("hit");
    tick_logic_graphs(&mut world);
    assert_eq!(
        world.get::<Blackboard>(a).expect("bb").get("hp"),
        Some(&Literal::Int(90).to_value())
    );
}

#[test]
fn malformed_declarations_and_type_changing_writes_fail_compile() {
    let with =
        |decl: &str, rules: &str| format!("(format_version: 2, world: [\"score\"], {decl} rules: [{rules}])");
    let set = |key: &str, value: &str| {
        format!(
            "(name: \"s\", event: Start, actions: [(task: \"logic::SetBlackboard\", set: {{ \"key\": Literal(String(\"{key}\")), \"value\": Literal({value}) }})])"
        )
    };
    let op = |key: &str, op: &str| {
        format!(
            "(name: \"s\", event: Start, actions: [(task: \"logic::SetBlackboard\", set: {{ \"key\": Literal(String(\"{key}\")), \"op\": Literal(String(\"{op}\")), \"value\": Literal(String(\"x\")) }})])"
        )
    };
    let hp = "entity_vars: [(name: \"hp\", default: Int(1))],";
    let red = "world_vars: [(board: \"score\", name: \"red\", default: Int(0))],";
    for (text, code) in [
        (with(hp, ""), "ok"),
        (with(red, ""), "ok"),
        (
            with("entity_vars: [(name: \"\", default: Int(1))],", ""),
            "bad_value",
        ),
        (
            with("entity_vars: [(name: \"a/b\", default: Int(1))],", ""),
            "bad_value",
        ),
        (
            with(
                "entity_vars: [(name: \"hp\", default: Int(1)), (name: \"hp\", default: Int(2))],",
                "",
            ),
            "duplicate_name",
        ),
        (
            with(
                "world_vars: [(board: \"keep\", name: \"x\", default: Int(0))],",
                "",
            ),
            "unknown_scope",
        ),
        (
            with(
                "world_vars: [(board: \"score\", name: \"\", default: Int(0))],",
                "",
            ),
            "bad_value",
        ),
        (
            with(
                "world_vars: [(board: \"score\", name: \"x\", default: Int(0)), (board: \"score\", name: \"x\", default: Bool(true))],",
                "",
            ),
            "duplicate_name",
        ),
        (with(hp, &set("hp", "Int(5)")), "ok"),
        (with(hp, &set("entity/hp", "Int(5)")), "ok"),
        (with(hp, &set("hp", "String(\"x\")")), "bad_value"),
        (with(hp, &set("entity/hp", "Float(1.0)")), "bad_value"),
        (with(red, &set("world/score/red", "Bool(true)")), "bad_value"),
        (with(red, &set("world/score/blue", "Bool(true)")), "ok"),
        (with(hp, &set("other", "Bool(true)")), "ok"),
        (with(hp, &op("hp", "append")), "bad_value"),
        (
            with(
                "entity_vars: [(name: \"log\", default: String(\"\"))],",
                &op("log", "append"),
            ),
            "ok",
        ),
    ] {
        assert_eq!(reason(&text), code, "{text}");
    }
    let mut def = LogicGraphDef::new();
    def.entity_vars.push(EntityVarDef {
        name: "f".into(),
        default: Literal::Float(f32::INFINITY),
    });
    assert_eq!(
        LogicGraph::compile(&def, &BtTaskRegistry::default())
            .map(|_| ())
            .map_err(|e| e.reason()),
        Err("bad_value")
    );
    let too_many: Vec<String> = (0..=MAX_LOCALS)
        .map(|i| format!("(name: \"v{i}\", default: Int(0))"))
        .collect();
    assert_eq!(
        reason(&with(&format!("entity_vars: [{}],", too_many.join(", ")), "")),
        "bad_value"
    );
}
