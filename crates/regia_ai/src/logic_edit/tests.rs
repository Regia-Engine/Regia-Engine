// SPDX-License-Identifier: MPL-2.0
//! Phase-2 headless tests of spec 102 § 13 (P2.1 round trip, P2.2 wiring, P2.4 validation codes).
//! The panel and agent halves (P2.3, P2.5 and the save/reload/run acceptance) are in
//! `regia_editor::logic_graph_editor`.

use super::*;
use crate::blackboard::{BbValue, Blackboard};
use crate::logic::{LogicGraphStatus, LogicInbox};
use regia_ecs::prelude::*;

const EXAMPLE: &str = r#"LogicGraphDef(
  format_version: 1,
  description: "Gatehouse door logic",
  seed: 7,
  locals: [ (name: "opened_count", default: Int(0)) ],
  rules: [
    ( name: "open_door",
      event: EntityEvent("trigger_enter"),
      priority: 10,
      condition: All([ Has("door_ref"), Not(Key("locked")) ]),
      actions: [
        ( task: "logic::SetBlackboard", set: { "key": Literal(String("local/opened_count")),
                                               "op": Literal(String("increment")) } ),
        ( task: "logic::EmitEvent", set: { "event": Literal(String("opened")) } ),
      ]),
    ( name: "raise_alarm",
      event: EntityEvent("trigger_enter"),
      priority: 20,
      cooldown_ticks: 5,
      condition: Key("locked"),
      actions: [ ( task: "logic::EmitEvent", set: { "event": Literal(String("alarm")) } ) ]),
    ( name: "single_all", event: Start, condition: All([ Has("x") ]) ),
    ( name: "any_rule", event: Timer(every_ticks: 3, jitter_ticks: 1), condition: Any([ Has("a"), Has("b") ]) ),
    ( name: "always", event: BlackboardChanged(key: "local/opened_count") ),
  ],
)"#;

fn ev(name: &str) -> LogicEvent {
    LogicEvent::EntityEvent(name.into())
}

fn set_n(op: &str) -> BTreeMap<String, Binding> {
    BTreeMap::from([
        (
            "key".to_string(),
            Binding::Literal(Literal::String("local/n".into())),
        ),
        ("op".to_string(), Binding::Literal(Literal::String(op.into()))),
    ])
}

fn ops(doc: &mut LogicGraphDoc, list: &[LogicOp]) -> Vec<NodeId> {
    apply_ops(doc, list).expect("ops apply")
}

fn op_err(doc: &mut LogicGraphDoc, op: LogicOp) -> &'static str {
    apply_ops(doc, &[op]).expect_err("op refused").error.code
}

fn codes(doc: &LogicGraphDoc) -> Vec<&'static str> {
    validate(doc, None).iter().map(|i| i.code).collect()
}

/// P2.1: `def → doc → def` is exact (so re-saving an unchanged asset is byte-identical) and the
/// layout sidecar round-trips every position by rule and role.
#[test]
fn p2_1_a_definition_round_trips_exactly_and_resaves_byte_identical() {
    let def = LogicGraphDef::from_ron(EXAMPLE).expect("example parses");
    let doc = LogicGraphDoc::from_def(&def, &BTreeMap::new());
    assert_eq!(doc.to_def(), def, "def -> doc -> def is exact");
    // The canonical text: write once, then open and re-save without edits.
    let canonical = def.to_ron().expect("ron");
    let reopened = LogicGraphDoc::from_ron(&canonical, &BTreeMap::new()).expect("reopen");
    assert_eq!(
        reopened.to_ron().expect("ron"),
        canonical,
        "re-save is byte-identical"
    );
    // Five rules: open_door has two condition nodes (All of two), single_all, any_rule one each.
    let chains = doc.chains();
    assert_eq!(
        chains
            .iter()
            .map(|c| (c.conditions.len(), c.actions.len()))
            .collect::<Vec<_>>(),
        vec![(2, 2), (1, 1), (1, 0), (1, 0), (0, 0)]
    );
    // Moved nodes keep their place through the sidecar.
    let mut moved = doc.clone();
    let last_action = chains[0].actions[1];
    ops(
        &mut moved,
        &[LogicOp::MoveNode {
            node: last_action,
            at: [900.5, -40.0],
        }],
    );
    let sidecar = LogicLayoutFile::of(&moved).to_ron().expect("layout ron");
    let positions = LogicLayoutFile::positions_from_ron(&sidecar);
    assert_eq!(positions.get("open_door#action1"), Some(&[900.5, -40.0]));
    let back = LogicGraphDoc::from_ron(&canonical, &positions).expect("reopen with layout");
    assert_eq!(back, moved, "graph and layout survive save and reopen");
    // A broken sidecar is no error: the default layout is used.
    assert!(LogicLayoutFile::positions_from_ron("not ron").is_empty());
    assert_eq!(
        layout_path(std::path::Path::new("a/gate.logic.ron")),
        std::path::PathBuf::from("a/gate.logic.layout.ron")
    );
}

/// P2.2: event → action compiles to `Always`, a condition chain to `All`, a mode switch to `Not`
/// or `Any` changes the compiled tree, and a dangling node is an issue, not a panic.
#[test]
fn p2_2_wiring_lowers_to_the_expected_condition_trees() {
    let mut doc = LogicGraphDoc::new();
    let ids = ops(
        &mut doc,
        &[
            LogicOp::AddEvent {
                rule: "go".into(),
                event: ev("go"),
                priority: 0,
                cooldown_ticks: 0,
                id: None,
                at: None,
            },
            LogicOp::AddAction {
                task: "logic::SetBlackboard".into(),
                set: set_n("increment"),
                id: None,
                at: None,
            },
        ],
    );
    let (event, action) = (ids[0], ids[1]);
    ops(
        &mut doc,
        &[LogicOp::Connect {
            from: event,
            to: action,
        }],
    );
    assert_eq!(doc.to_def().rules[0].condition, Condition::Always);

    // Two condition nodes between event and action: All of both, in wire order.
    let conds = ops(
        &mut doc,
        &[
            LogicOp::AddCondition {
                condition: Condition::Has("a".into()),
                id: None,
                at: None,
            },
            LogicOp::AddCondition {
                condition: Condition::Key("b".into()),
                id: None,
                at: None,
            },
        ],
    );
    ops(
        &mut doc,
        &[
            LogicOp::Disconnect {
                from: event,
                to: action,
            },
            LogicOp::Connect {
                from: event,
                to: conds[0],
            },
            LogicOp::Connect {
                from: conds[0],
                to: conds[1],
            },
            LogicOp::Connect {
                from: conds[1],
                to: action,
            },
        ],
    );
    assert_eq!(
        doc.to_def().rules[0].condition,
        Condition::All(vec![Condition::Has("a".into()), Condition::Key("b".into())])
    );
    ops(
        &mut doc,
        &[LogicOp::SetConditionMode {
            node: conds[1],
            mode: CondMode::Not,
        }],
    );
    assert_eq!(
        doc.to_def().rules[0].condition,
        Condition::All(vec![
            Condition::Has("a".into()),
            Condition::Not(Box::new(Condition::Key("b".into())))
        ])
    );
    // One node with two terms switched to Any.
    ops(
        &mut doc,
        &[
            LogicOp::SetCondition {
                node: conds[0],
                condition: Condition::All(vec![Condition::Has("a".into()), Condition::Has("c".into())]),
            },
            LogicOp::SetConditionMode {
                node: conds[0],
                mode: CondMode::Any,
            },
            LogicOp::SetConditionMode {
                node: conds[1],
                mode: CondMode::Leaf,
            },
        ],
    );
    assert_eq!(
        doc.to_def().rules[0].condition,
        Condition::All(vec![
            Condition::Any(vec![Condition::Has("a".into()), Condition::Has("c".into())]),
            Condition::Key("b".into()),
        ])
    );
    assert!(is_valid(&validate(&doc, None)), "{:?}", validate(&doc, None));

    // Unwiring the action leaves it dangling: an issue naming the node, and the lowering skips it.
    ops(
        &mut doc,
        &[LogicOp::Disconnect {
            from: conds[1],
            to: action,
        }],
    );
    let issues = validate(&doc, None);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!((issues[0].code, issues[0].node), ("dangling_wire", Some(action)));
    assert!(doc.to_def().rules[0].actions.is_empty());
}

/// Pin types, busy pins and cycles are refused at the op, with the document unchanged.
#[test]
fn p2_2_pin_type_checks_refuse_bad_wires() {
    let mut doc = LogicGraphDoc::new();
    let add_event = |rule: &str| LogicOp::AddEvent {
        rule: rule.into(),
        event: ev(rule),
        priority: 0,
        cooldown_ticks: 0,
        id: None,
        at: None,
    };
    let ids = ops(
        &mut doc,
        &[
            add_event("a"),
            add_event("b"),
            LogicOp::AddCondition {
                condition: Condition::Always,
                id: None,
                at: None,
            },
            LogicOp::AddAction {
                task: "logic::EmitEvent".into(),
                set: BTreeMap::new(),
                id: None,
                at: None,
            },
            LogicOp::AddAction {
                task: "logic::EmitEvent".into(),
                set: BTreeMap::new(),
                id: None,
                at: None,
            },
        ],
    );
    let (ea, eb, c, a1, a2) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
    let before = doc.clone();
    assert_eq!(op_err(&mut doc, LogicOp::Connect { from: a1, to: c }), "pin_type");
    assert_eq!(op_err(&mut doc, LogicOp::Connect { from: c, to: ea }), "pin_type");
    assert_eq!(
        op_err(&mut doc, LogicOp::Connect { from: ea, to: eb }),
        "pin_type"
    );
    assert_eq!(op_err(&mut doc, LogicOp::Connect { from: a1, to: a1 }), "cycle");
    assert_eq!(
        op_err(&mut doc, LogicOp::Connect { from: ea, to: 99 }),
        "unknown_node"
    );
    assert_eq!(doc, before);
    ops(
        &mut doc,
        &[
            LogicOp::Connect { from: ea, to: a1 },
            LogicOp::Connect { from: a1, to: a2 },
        ],
    );
    assert_eq!(
        op_err(&mut doc, LogicOp::Connect { from: eb, to: a1 }),
        "pin_busy"
    );
    assert_eq!(op_err(&mut doc, LogicOp::Connect { from: ea, to: c }), "pin_busy");
    assert_eq!(
        op_err(&mut doc, LogicOp::Connect { from: a2, to: a1 }),
        "pin_busy"
    );
    assert_eq!(
        op_err(&mut doc, LogicOp::Disconnect { from: eb, to: a2 }),
        "no_wire"
    );
    assert_eq!(
        op_err(
            &mut doc,
            LogicOp::SetTask {
                node: c,
                task: "x".into()
            }
        ),
        "wrong_kind"
    );
    assert_eq!(
        op_err(
            &mut doc,
            LogicOp::AddCondition {
                condition: Condition::Always,
                id: Some(ea),
                at: None
            }
        ),
        "duplicate_id"
    );
    assert_eq!(
        op_err(
            &mut doc,
            LogicOp::MoveNode {
                node: c,
                at: [f32::NAN, 0.0]
            }
        ),
        "bad_value"
    );
    // A batch is all or nothing: the good first op is not kept when the second fails.
    let before = doc.clone();
    let err = apply_ops(
        &mut doc,
        &[
            LogicOp::Disconnect { from: a1, to: a2 },
            LogicOp::RemoveNode { node: 1234 },
        ],
    )
    .expect_err("second op fails");
    assert_eq!((err.op_index, err.error.code), (1, "unknown_node"));
    assert_eq!(doc, before);
    // Removing a node drops its wires and its rule.
    ops(&mut doc, &[LogicOp::RemoveNode { node: ea }]);
    assert!(!doc.wires.contains_key(&ea) && !doc.rules.contains(&ea));
    assert_eq!(doc.dangling(), vec![c, a1, a2]);
}

/// P2.4 (headless half): every § 6 issue code is reported, with the node it is about.
#[test]
fn p2_4_each_issue_code_is_reported_with_its_node() {
    let base = |text: &str| {
        LogicGraphDoc::from_ron(
            &format!("(format_version: 1, locals: [(name: \"n\", default: Int(0))], rules: [{text}])"),
            &BTreeMap::new(),
        )
        .expect("parses")
    };
    let ok = base(
        "(name: \"r\", event: Start, actions: [(task: \"logic::EmitEvent\", set: { \"event\": Literal(String(\"x\")) })])",
    );
    assert!(codes(&ok).is_empty(), "{:?}", validate(&ok, None));

    let dup = base("(name: \"r\", event: Start), (name: \"r\", event: Start)");
    assert_eq!(codes(&dup), vec!["duplicate_name"]);
    assert_eq!(validate(&dup, None)[0].node, Some(dup.rules[1]));

    let mut many = String::new();
    for i in 0..=MAX_RULES {
        many.push_str(&format!("(name: \"r{i}\", event: Start),"));
    }
    assert_eq!(codes(&base(&many)), vec!["too_many_rules"]);

    let actions =
        vec!["(task: \"logic::EmitEvent\", set: { \"event\": Literal(String(\"x\")) })"; 17].join(",");
    assert_eq!(
        codes(&base(&format!(
            "(name: \"r\", event: Start, actions: [{actions}])"
        ))),
        vec!["too_many_actions"]
    );

    let mut deep = "Has(\"a\")".to_string();
    for _ in 0..17 {
        deep = format!("Not({deep})");
    }
    let too_deep = base(&format!("(name: \"r\", event: Start, condition: {deep})"));
    assert_eq!(codes(&too_deep), vec!["condition_too_deep"]);
    assert_eq!(
        validate(&too_deep, None)[0].node,
        Some(too_deep.chain(too_deep.rules[0]).conditions[0])
    );

    let task = base(
        "(name: \"r\", event: Start, actions: [(task: \"logic::EmitEvent\", set: { \"event\": Literal(String(\"x\")) }), (task: \"game::Nope\")])",
    );
    let issues = validate(&task, None);
    assert_eq!(
        issues.iter().map(|i| i.code).collect::<Vec<_>>(),
        vec!["unknown_task"]
    );
    assert_eq!(
        issues[0].severity,
        LogicSeverity::Warning,
        "no registry: not checked"
    );
    assert_eq!(issues[0].node, Some(task.chain(task.rules[0]).actions[1]));
    let scope = base("(name: \"r\", event: Start, condition: All([Has(\"a\"), Key(\"zone/x\")]))");
    let issues = validate(&scope, None);
    assert_eq!(
        issues.iter().map(|i| i.code).collect::<Vec<_>>(),
        vec!["unknown_scope"]
    );
    assert_eq!(issues[0].node, Some(scope.chain(scope.rules[0]).conditions[1]));

    let bad = base("(name: \"r\", event: Timer(every_ticks: 0))");
    assert_eq!(codes(&bad), vec!["bad_value"]);
    let mut empty_name = ok.clone();
    let head = empty_name.rules[0];
    apply_ops(
        &mut empty_name,
        &[LogicOp::SetEvent {
            node: head,
            rule: Some(String::new()),
            event: None,
            priority: None,
            cooldown_ticks: None,
        }],
    )
    .expect("rename");
    assert_eq!(codes(&empty_name), vec!["bad_value"]);
    let compare = base("(name: \"r\", event: Start, condition: Compare(key: \"hp\", op: Lt, value: 3.0))");
    assert!(codes(&compare).is_empty());

    // Since phase 3 a world list compiles (ADR-7268); a bad board name is a header issue, and a rule
    // that names an undeclared board is `unknown_scope`.
    let world =
        LogicGraphDoc::from_ron("(format_version: 1, world: [\"gate\"])", &BTreeMap::new()).expect("parses");
    assert!(codes(&world).is_empty());
    let bad_board =
        LogicGraphDoc::from_ron("(format_version: 1, world: [\"a/b\"])", &BTreeMap::new()).expect("parses");
    assert_eq!(codes(&bad_board), vec!["bad_value"]);
    let undeclared = base("(name: \"r\", event: Start, condition: Key(\"world/keep/x\"))");
    assert_eq!(codes(&undeclared), vec!["unknown_scope"]);

    let mut orphan = ok.clone();
    ops(
        &mut orphan,
        &[LogicOp::AddCondition {
            condition: Condition::Always,
            id: None,
            at: None,
        }],
    );
    assert_eq!(codes(&orphan), vec!["dangling_wire"]);
    // Two rules with problems both show up (one bad rule does not hide another).
    let two = base(
        "(name: \"a\", event: Timer(every_ticks: 0)), (name: \"b\", event: Start, condition: Key(\"zone/x\"))",
    );
    assert_eq!(codes(&two), vec!["bad_value", "unknown_scope"]);
}

/// A graph built only through ops lowers to a definition the phase-1 runtime compiles and runs.
#[test]
fn a_graph_built_with_ops_runs_in_the_runtime() {
    let mut doc = LogicGraphDoc::new();
    ops(
        &mut doc,
        &[
            LogicOp::SetLocal {
                name: "n".into(),
                default: Some(Literal::Int(0)),
            },
            LogicOp::AddEvent {
                rule: "count".into(),
                event: ev("go"),
                priority: 0,
                cooldown_ticks: 0,
                id: Some(10),
                at: None,
            },
            LogicOp::AddCondition {
                condition: Condition::Not(Box::new(Condition::Key("stop".into()))),
                id: Some(11),
                at: None,
            },
            LogicOp::AddAction {
                task: "logic::SetBlackboard".into(),
                set: set_n("increment"),
                id: Some(12),
                at: None,
            },
            LogicOp::Connect { from: 10, to: 11 },
            LogicOp::Connect { from: 11, to: 12 },
        ],
    );
    assert_eq!(doc.next_id, 13);
    let def = doc.to_def();
    let graph = LogicGraph::compile(&def, &BtTaskRegistry::default()).expect("compiles");
    let mut world = World::new();
    let e = world
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id();
    world.get_mut::<LogicInbox>(e).expect("inbox").push("go");
    crate::logic::tick_logic_graphs(&mut world);
    world.get_mut::<LogicInbox>(e).expect("inbox").push("go");
    crate::logic::tick_logic_graphs(&mut world);
    let status = world.get::<LogicGraphStatus>(e).cloned().expect("status");
    assert_eq!(status.rules[0].fires, 2);
    assert_eq!(
        world
            .get::<LogicGraph>(e)
            .and_then(|g| g.locals().get("n").cloned()),
        Some(BbValue::Int(2))
    );
}

/// Phase 3: `set_graph` declares world blackboards (spec 102 § 5); a bad board name is an issue.
#[test]
fn p3_set_graph_declares_world_boards() {
    let mut doc = LogicGraphDoc::from_ron("(format_version: 1)", &BTreeMap::new()).expect("parses");
    let set = |world: &str| LogicOp::SetGraph {
        description: None,
        seed: None,
        world: Some(vec![world.to_string()]),
    };
    apply_ops(&mut doc, &[set("gatehouse")]).expect("applies");
    assert_eq!(doc.to_def().world, ["gatehouse"]);
    assert!(codes(&doc).is_empty());
    apply_ops(&mut doc, &[set("a/b")]).expect("applies");
    assert_eq!(codes(&doc), vec!["bad_value"]);
    apply_ops(&mut doc, &[set("keep")]).expect("applies");
    assert_eq!(doc.to_def().world, ["keep"], "the list is replaced");
}
