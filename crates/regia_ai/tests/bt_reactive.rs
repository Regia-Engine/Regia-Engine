// SPDX-License-Identifier: MPL-2.0
//! Value conditions, observer aborts and the behavior-tree / state-tree bridges (ADR-4543).
//!
//! Steps are counted. Nothing here waits on a clock.

use std::collections::BTreeMap;

use regia_ai::bt_edit::{self, validate};
use regia_ai::bt_value::{self, CompareFault};
use regia_ai::state_tree::{StateDef, StateTree, StateTreeDef, StateTreeError, StateTreeRunStatus};
use regia_ai::{
    AbortMode, BbValue, BehaviorTree, BehaviorTreeDef, Blackboard, BtCompareOp, BtCompileError, BtContext,
    BtLiteral, BtStatus, BtTask, BtTaskRegistration, BtTaskRegistry, BtValueKind, DefKind, DefNode,
    migrate_bt_ron,
};
use regia_app::prelude::*;

fn registry() -> BtTaskRegistry {
    let mut registry = BtTaskRegistry::default();
    registry.register::<Chase>();
    registry.register::<Flee>();
    registry.register::<Touch>();
    registry.register::<Win>();
    registry.register::<Lose>();
    registry.register::<Hold>();
    registry
}

fn bump(ctx: &mut BtContext<'_>, key: &str) {
    if let Some(mut board) = ctx.blackboard_mut() {
        let next = board.get_int(key) + 1;
        board.insert(key, next);
    }
}

struct Chase;
impl BtTask for Chase {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        bump(ctx, "chase_ticks");
        BtStatus::Running
    }
    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        bump(ctx, "chase_aborts");
    }
}
impl BtTaskRegistration for Chase {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Chase)
    }
    fn task_type_path() -> &'static str {
        "reactive::Chase"
    }
}

struct Flee;
impl BtTask for Flee {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        bump(ctx, "flee_ticks");
        BtStatus::Running
    }
    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        bump(ctx, "flee_aborts");
    }
}
impl BtTaskRegistration for Flee {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Flee)
    }
    fn task_type_path() -> &'static str {
        "reactive::Flee"
    }
}

struct Touch;
impl BtTask for Touch {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        bump(ctx, "touched");
        BtStatus::Success
    }
}
impl BtTaskRegistration for Touch {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Touch)
    }
    fn task_type_path() -> &'static str {
        "reactive::Touch"
    }
}

struct Win;
impl BtTask for Win {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Success
    }
}
impl BtTaskRegistration for Win {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Win)
    }
    fn task_type_path() -> &'static str {
        "reactive::Win"
    }
}

struct Lose;
impl BtTask for Lose {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Failure
    }
}
impl BtTaskRegistration for Lose {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Lose)
    }
    fn task_type_path() -> &'static str {
        "reactive::Lose"
    }
}

/// Runs while the blackboard integer `steps` is above zero, then succeeds.
struct Hold;
impl BtTask for Hold {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let left = ctx.blackboard().map(|board| board.get_int("steps")).unwrap_or(0);
        if left <= 0 {
            return BtStatus::Success;
        }
        if let Some(mut board) = ctx.blackboard_mut() {
            board.insert("steps", left - 1);
        }
        BtStatus::Running
    }
}
impl BtTaskRegistration for Hold {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Hold)
    }
    fn task_type_path() -> &'static str {
        "reactive::Hold"
    }
}

fn world(board: Blackboard) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let entity = app.world_mut().spawn(board).id();
    (app, entity)
}

fn tick(app: &mut App, entity: Entity, tree: &mut BehaviorTree) -> BtStatus {
    tree.tick(&mut BtContext {
        world: app.world_mut(),
        entity,
        dt: 0.016,
    })
}

fn count(app: &App, entity: Entity, key: &str) -> i64 {
    app.world()
        .get::<Blackboard>(entity)
        .expect("blackboard")
        .get_int(key)
}

fn set_float(app: &mut App, entity: Entity, key: &str, value: f32) {
    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("blackboard")
        .insert(key, value);
}

fn chase_flee(abort: AbortMode) -> BehaviorTreeDef {
    let compare = DefNode {
        name: "threat".into(),
        kind: DefKind::Compare,
        key: "threat".into(),
        abort,
        op: BtCompareOp::Ge,
        value_kind: BtValueKind::Float,
        literal: Some(BtLiteral::Float(0.8)),
        ..DefNode::default()
    };
    let flee = DefNode {
        name: "flee".into(),
        kind: DefKind::Leaf,
        task: "reactive::Flee".into(),
        ..DefNode::default()
    };
    let sequence = DefNode {
        name: "flee_branch".into(),
        kind: DefKind::Sequence,
        children: vec![2, 4],
        ..DefNode::default()
    };
    let chase = DefNode {
        name: "chase".into(),
        kind: DefKind::Leaf,
        task: "reactive::Chase".into(),
        ..DefNode::default()
    };
    let root = DefNode {
        name: "root".into(),
        kind: DefKind::Selector,
        children: vec![1, 3],
        ..DefNode::default()
    };
    // 0 selector [1 sequence, 3 chase]; 1 sequence [2 compare, 4 flee].
    BehaviorTreeDef {
        format_version: 2,
        root: 0,
        nodes: vec![root, sequence, compare, chase, flee],
    }
}

#[test]
fn a_chase_flips_to_flee_on_the_tick_the_threat_changes_and_aborts_once() {
    let (mut app, entity) = world(Blackboard::new());
    set_float(&mut app, entity, "threat", 0.0);
    let mut tree =
        BehaviorTree::compile(&chase_flee(AbortMode::LowerPriority), &registry()).expect("compiles");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(
        tree.observer_evaluations(),
        0,
        "the first tick only stores a baseline"
    );
    assert_eq!(count(&app, entity, "chase_ticks"), 1);
    assert_eq!(count(&app, entity, "chase_aborts"), 0);
    assert_eq!(count(&app, entity, "flee_ticks"), 0);

    set_float(&mut app, entity, "threat", 0.9);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(tree.observer_evaluations(), 1);
    assert_eq!(
        count(&app, entity, "chase_ticks"),
        1,
        "chase is not ticked on the abort tick"
    );
    assert_eq!(count(&app, entity, "chase_aborts"), 1);
    assert_eq!(count(&app, entity, "flee_ticks"), 1);

    set_float(&mut app, entity, "threat", 0.9);
    set_float(&mut app, entity, "noise", 1.0);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(
        tree.observer_evaluations(),
        1,
        "an equal write and an unwatched key do not re-check"
    );
    assert_eq!(count(&app, entity, "chase_aborts"), 1);
    assert_eq!(count(&app, entity, "flee_ticks"), 2);

    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("blackboard")
        .values
        .insert("threat".into(), BbValue::Float(0.1));
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(
        tree.observer_evaluations(),
        2,
        "a write that bypasses insert still counts"
    );
    assert_eq!(
        count(&app, entity, "chase_aborts"),
        1,
        "a failed re-check does not abort again"
    );
    assert_eq!(count(&app, entity, "flee_ticks"), 3);
}

#[test]
fn without_an_observer_the_running_branch_is_resumed_and_the_blackboard_is_untouched() {
    let (mut app, entity) = world(Blackboard::new());
    set_float(&mut app, entity, "threat", 0.0);
    let mut tree = BehaviorTree::compile(&chase_flee(AbortMode::None), &registry()).expect("compiles");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    set_float(&mut app, entity, "threat", 0.9);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(tree.observer_evaluations(), 0);
    assert_eq!(count(&app, entity, "chase_ticks"), 2);
    assert_eq!(count(&app, entity, "chase_aborts"), 0);
    assert_eq!(count(&app, entity, "flee_ticks"), 0);

    let leaf = BehaviorTreeDef {
        format_version: 1,
        root: 0,
        nodes: vec![DefNode {
            name: "touch".into(),
            kind: DefKind::Leaf,
            task: "reactive::Touch".into(),
            ..DefNode::default()
        }],
    };
    let (mut app, entity) = world(Blackboard::new());
    let mut tree = BehaviorTree::compile(&leaf, &registry()).expect("compiles");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Success);
    let pending = app
        .world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("blackboard")
        .drain_changes();
    assert!(
        pending.contains("touched"),
        "a tree with no observers does not drain changes"
    );
    assert_eq!(tree.observer_evaluations(), 0);
}

#[test]
fn an_unchanged_vector_with_nan_bits_does_not_dispatch_an_is_set_observer() {
    let mut board = Blackboard::new();
    board.insert("target", regia_core::math::Vec3::new(f32::NAN, 0.0, 1.0));
    let (mut app, entity) = world(board);
    let def = one(DefNode {
        kind: DefKind::Compare,
        key: "target".into(),
        op: BtCompareOp::IsSet,
        abort: AbortMode::Both,
        ..DefNode::default()
    });
    let mut tree = BehaviorTree::compile(&def, &registry()).expect("is_set accepts every stored kind");
    for _ in 0..10 {
        assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Success);
    }
    assert_eq!(tree.observer_evaluations(), 0, "the watched bits never changed");
}

fn watched_decorator(kind: DefKind, with_condition: bool) -> BehaviorTreeDef {
    let mut nodes = vec![
        DefNode {
            name: "watch".into(),
            kind,
            key: "allow".into(),
            abort: AbortMode::AbortSelf,
            children: vec![1],
            task: "reactive::Flee".into(),
            seconds: 0.032,
            ..DefNode::default()
        },
        DefNode {
            name: "chase".into(),
            kind: DefKind::Leaf,
            task: "reactive::Chase".into(),
            ..DefNode::default()
        },
    ];
    let root = if with_condition {
        nodes.push(DefNode {
            name: "allow".into(),
            kind: DefKind::Condition,
            key: "allow".into(),
            abort: AbortMode::AbortSelf,
            ..DefNode::default()
        });
        nodes.push(DefNode {
            name: "root".into(),
            kind: DefKind::Sequence,
            children: vec![2, 0],
            ..DefNode::default()
        });
        3
    } else {
        0
    };
    BehaviorTreeDef {
        format_version: 2,
        root,
        nodes,
    }
}

#[test]
fn overlapping_self_observers_abort_each_task_once() {
    let mut board = Blackboard::new();
    board.insert("allow", true);
    let (mut app, entity) = world(board);
    let mut tree = BehaviorTree::compile(&watched_decorator(DefKind::Service, true), &registry())
        .expect("nested observers");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("board")
        .insert("allow", false);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Failure);
    assert_eq!(count(&app, entity, "chase_aborts"), 1);
    assert_eq!(count(&app, entity, "flee_aborts"), 1, "service callback");
}

#[test]
fn nested_decorators_reset_once_when_the_child_is_stored_before_the_parent() {
    let mut builder = regia_ai::BehaviorTreeBuilder::new();
    let chase = builder.leaf("chase", "reactive::Chase");
    let inner = builder.service("inner", "reactive::Flee", chase);
    let outer = builder.service("outer", "reactive::Flee", inner);
    for index in [inner, outer] {
        builder.nodes[index].key = "allow".into();
        builder.nodes[index].abort = AbortMode::AbortSelf;
    }
    let mut board = Blackboard::new();
    board.insert("allow", true);
    let (mut app, entity) = world(board);
    let mut tree = BehaviorTree::compile(&builder.build(outer), &registry()).expect("nested services");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("board")
        .insert("allow", false);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Failure);
    assert_eq!(count(&app, entity, "chase_aborts"), 1);
    assert_eq!(
        count(&app, entity, "flee_aborts"),
        2,
        "one reset for each service"
    );
}

#[test]
fn a_service_observer_aborts_its_own_task_and_child() {
    let mut board = Blackboard::new();
    board.insert("allow", true);
    let (mut app, entity) = world(board);
    let mut tree = BehaviorTree::compile(&watched_decorator(DefKind::Service, false), &registry())
        .expect("service observer");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("board")
        .insert("allow", false);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Failure);
    assert_eq!(count(&app, entity, "chase_aborts"), 1);
    assert_eq!(count(&app, entity, "flee_aborts"), 1, "service callback");
}

#[test]
fn a_timeout_observer_restarts_with_its_full_budget_after_abort() {
    let mut board = Blackboard::new();
    board.insert("allow", true);
    let (mut app, entity) = world(board);
    let mut tree = BehaviorTree::compile(&watched_decorator(DefKind::Timeout, false), &registry())
        .expect("timeout observer");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("board")
        .insert("allow", false);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Failure);
    assert_eq!(count(&app, entity, "chase_aborts"), 1);
    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("board")
        .insert("allow", true);
    assert_eq!(
        tick(&mut app, entity, &mut tree),
        BtStatus::Running,
        "fresh first step"
    );
    assert_eq!(
        tick(&mut app, entity, &mut tree),
        BtStatus::Failure,
        "fresh second step"
    );
    assert_eq!(count(&app, entity, "chase_aborts"), 2);
}

#[test]
fn a_sequence_aborts_its_running_child_when_its_condition_fails() {
    let condition = DefNode {
        name: "go".into(),
        kind: DefKind::Condition,
        key: "go".into(),
        abort: AbortMode::AbortSelf,
        ..DefNode::default()
    };
    let chase = DefNode {
        name: "chase".into(),
        kind: DefKind::Leaf,
        task: "reactive::Chase".into(),
        ..DefNode::default()
    };
    let root = DefNode {
        name: "root".into(),
        kind: DefKind::Sequence,
        children: vec![1, 2],
        ..DefNode::default()
    };
    let def = BehaviorTreeDef {
        format_version: 2,
        root: 0,
        nodes: vec![root, condition, chase],
    };
    let mut board = Blackboard::new();
    board.insert("go", true);
    let (mut app, entity) = world(board);
    let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(count(&app, entity, "chase_ticks"), 1);
    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("blackboard")
        .insert("go", false);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Failure);
    assert_eq!(count(&app, entity, "chase_ticks"), 1);
    assert_eq!(count(&app, entity, "chase_aborts"), 1);
    assert_eq!(tree.observer_evaluations(), 1);
}

#[test]
fn each_operator_is_a_validator_error_on_a_type_mismatch_and_does_not_panic() {
    let ordering = [
        BtCompareOp::Eq,
        BtCompareOp::Ne,
        BtCompareOp::Lt,
        BtCompareOp::Le,
        BtCompareOp::Gt,
        BtCompareOp::Ge,
    ];
    let kinds = [
        BtValueKind::Bool,
        BtValueKind::Int,
        BtValueKind::Float,
        BtValueKind::Tag,
        BtValueKind::Entity,
    ];
    for op in ordering {
        for kind in kinds {
            let wrong_kind = if kind == BtValueKind::Int {
                BtValueKind::Float
            } else {
                BtValueKind::Int
            };
            let node = DefNode {
                name: "cmp".into(),
                kind: DefKind::Compare,
                key: "k".into(),
                op,
                value_kind: kind,
                literal: Some(sample(wrong_kind, 1)),
                ..DefNode::default()
            };
            let def = BehaviorTreeDef {
                format_version: 2,
                root: 0,
                nodes: vec![node],
            };
            let issues = validate(&def, None);
            assert!(
                issues.iter().any(|issue| issue.code == "type_mismatch"),
                "{op:?} {kind:?} -> {issues:?}"
            );
            match BehaviorTree::compile(&def, &registry()) {
                Err(BtCompileError::Malformed(message)) => {
                    assert!(message.contains("type_mismatch"), "{message}");
                }
                Err(other) => panic!("expected a validator error, got {other:?}"),
                Ok(_) => panic!("expected a validator error, the tree compiled"),
            }
        }
    }
    let set = DefNode {
        name: "set".into(),
        kind: DefKind::Compare,
        key: "k".into(),
        op: BtCompareOp::IsSet,
        value_kind: BtValueKind::Int,
        literal: Some(BtLiteral::Int(1)),
        ..DefNode::default()
    };
    assert!(
        validate(&one(set), None)
            .iter()
            .any(|issue| issue.code == "bad_compare")
    );
    let range = DefNode {
        name: "range".into(),
        kind: DefKind::Compare,
        key: "k".into(),
        op: BtCompareOp::InRange,
        value_kind: BtValueKind::Tag,
        literal: Some(BtLiteral::Tag("a".into())),
        literal_hi: Some(BtLiteral::Tag("z".into())),
        ..DefNode::default()
    };
    assert!(
        validate(&one(range), None)
            .iter()
            .any(|issue| issue.code == "type_mismatch")
    );
    let mut board = Blackboard::new();
    board.insert("k", BbValue::Vec3(regia_core::math::Vec3::ZERO));
    let literal = Some(BtLiteral::Int(1));
    assert!(!bt_value::holds(
        &board,
        bt_value::CompareView {
            op: BtCompareOp::Eq,
            kind: BtValueKind::Int,
            key: "k",
            rhs_key: "",
            literal: &literal,
            literal_hi: &None,
        }
    ));
    assert!(
        bt_value::compare_faults(bt_value::CompareView {
            op: BtCompareOp::Eq,
            kind: BtValueKind::Int,
            key: "k",
            rhs_key: "",
            literal: &Some(BtLiteral::Float(1.0)),
            literal_hi: &None,
        })
        .contains(&CompareFault::TypeMismatch)
    );
}

fn sample(kind: BtValueKind, n: i64) -> BtLiteral {
    match kind {
        BtValueKind::Bool => BtLiteral::Bool(n != 0),
        BtValueKind::Int => BtLiteral::Int(n),
        BtValueKind::Float => BtLiteral::Float(n as f32),
        BtValueKind::Tag => BtLiteral::Tag(if n <= 0 { "m" } else { "z" }.into()),
        BtValueKind::Entity => BtLiteral::Entity(n as u64),
    }
}

fn one(node: DefNode) -> BehaviorTreeDef {
    BehaviorTreeDef {
        format_version: 2,
        root: 0,
        nodes: vec![node],
    }
}

#[test]
fn v1_text_migrates_by_inserting_the_version_and_a_second_pass_is_identical() {
    let v1 = "(\n    root: 0,\n    nodes: [\n        (name: \"root\", kind: selector, children: [], key: \"\", seconds: 0.0, times: None, task: \"\", policy: require_all),\n    ],\n)\n";
    let migrated = migrate_bt_ron(v1).expect("migrates");
    let expected = "(\n    format_version: 2,\n    root: 0,\n    nodes: [\n        (name: \"root\", kind: selector, children: [], key: \"\", seconds: 0.0, times: None, task: \"\", policy: require_all),\n    ],\n)\n";
    assert_eq!(migrated, expected);
    assert_eq!(migrate_bt_ron(&migrated).expect("again"), migrated);
    let loaded = bt_edit::parse_bt_ron(v1).expect("v1 loads");
    let upgraded = bt_edit::parse_bt_ron(&migrated).expect("v2 loads");
    assert_eq!(loaded.nodes, upgraded.nodes);
    assert_eq!(loaded.root, upgraded.root);
    assert_eq!(loaded.format_version, 1);
    assert_eq!(upgraded.format_version, 2);
    let mut left = BehaviorTree::compile(&loaded, &registry()).expect("v1 compiles");
    let mut right = BehaviorTree::compile(&upgraded, &registry()).expect("v2 compiles");
    let (mut app, entity) = world(Blackboard::new());
    assert_eq!(
        tick(&mut app, entity, &mut left),
        tick(&mut app, entity, &mut right)
    );

    let explicit = "(\n    format_version: 1,\n    root: 0,\n    nodes: [],\n)\n";
    let rewritten = migrate_bt_ron(explicit).expect("rewrites");
    assert_eq!(
        rewritten,
        "(\n    format_version: 2,\n    root: 0,\n    nodes: [],\n)\n"
    );
    assert_eq!(migrate_bt_ron(&rewritten).expect("stable"), rewritten);

    let commented = "// keep\n(\n    root: 0,\n    nodes: [],\n)\n";
    let with_note = migrate_bt_ron(commented).expect("comment");
    assert!(with_note.starts_with("// keep\n"));
    assert!(with_note.contains("format_version: 2"));
    assert_eq!(migrate_bt_ron(&with_note).expect("stable"), with_note);

    let err = migrate_bt_ron("").expect_err("empty");
    assert!(!err.starts_with("no migrator registered"), "{err}");
    let err = migrate_bt_ron("(format_version: 9, root: 0, nodes: [])").expect_err("future");
    assert!(err.contains("newer"), "{err}");
}

#[test]
fn v1_migration_preserves_nested_comments_and_ron_extension_attributes() {
    let body = "(root: 0, nodes: [])";
    for prefix in [
        "/* outer /* inner */ (still a comment) */\n",
        "#![enable(implicit_some)]\n",
        "#![schema = \"schemas/(guard].ron\"]\n",
        "/* outer /* inner */ (note) */\n#![enable(implicit_some)]\n",
    ] {
        let v1 = format!("{prefix}{body}");
        let old = bt_edit::parse_bt_ron(&v1).expect("valid v1 RON");
        let migrated = migrate_bt_ron(&v1).expect("migrates valid v1");
        assert_eq!(
            migrated,
            format!("{prefix}(format_version: 2, root: 0, nodes: [])")
        );
        let mut expected = old;
        expected.format_version = 2;
        assert_eq!(bt_edit::parse_bt_ron(&migrated).expect("valid v2 RON"), expected);
        assert_eq!(migrate_bt_ron(&migrated).expect("stable"), migrated);
    }
}

#[test]
fn migration_replaces_the_parsed_version_token_and_preserves_its_surroundings() {
    for token in ["1", "0x1", "0o1", "0b1"] {
        let v1 = format!(
            "#![enable(implicit_some)]\nBehaviorTreeDef /* ( /* nested */ ) */ (format_version: /* before */ {token} /* after */, root: 0, nodes: [])"
        );
        let mut expected = bt_edit::parse_bt_ron(&v1).expect("valid v1 spelling");
        expected.format_version = 2;
        let migrated = migrate_bt_ron(&v1).expect("migrates parsed token");
        assert_eq!(
            migrated,
            v1.replace(&format!("{token} /* after */"), "2 /* after */")
        );
        assert_eq!(bt_edit::parse_bt_ron(&migrated).expect("valid v2"), expected);
        assert_eq!(migrate_bt_ron(&migrated).expect("stable"), migrated);
    }
    let named = "BehaviorTreeDef /* ( /* nested */ ) */ (root: 0, nodes: [])";
    assert!(bt_edit::parse_bt_ron(named).is_ok());
    assert_eq!(
        migrate_bt_ron(named).expect("named implicit v1"),
        "BehaviorTreeDef /* ( /* nested */ ) */ (format_version: 2, root: 0, nodes: [])"
    );
}

fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(path, text)| ((*path).to_string(), (*text).to_string()))
        .collect()
}

fn load<'a>(files: &'a BTreeMap<String, String>) -> impl Fn(&str) -> Option<String> + 'a {
    move |path| files.get(path).cloned()
}

fn leaf_bt(task: &str) -> String {
    format!(
        "(\n    format_version: 2,\n    root: 0,\n    nodes: [\n        (name: \"leaf\", kind: leaf, children: [], key: \"\", seconds: 0.0, times: None, task: \"{task}\", policy: require_all),\n    ],\n)\n"
    )
}

fn state_text(task: &str) -> String {
    format!(
        "(\n    format_version: 2,\n    root: \"Root\",\n    states: [\n        (name: \"Root\", tasks: [\"{task}\"]),\n    ],\n)\n"
    )
}

fn state_def(task: &str) -> StateTreeDef {
    let mut def = StateTreeDef::new("Root");
    let mut state = StateDef::new("Root");
    state.tasks.push(task.to_string());
    def.states.push(state);
    def
}

fn tick_state(app: &mut App, entity: Entity, tree: &mut StateTree) -> StateTreeRunStatus {
    tree.tick(&mut BtContext {
        world: app.world_mut(),
        entity,
        dt: 0.016,
    })
}

#[test]
fn bridges_finish_with_the_tasks_status_in_both_directions() {
    let registry = registry();
    let bt_files = files(&[
        ("win.bt.ron", &leaf_bt("reactive::Win")),
        ("lose.bt.ron", &leaf_bt("reactive::Lose")),
    ]);
    for (path, want) in [
        ("bt:win.bt.ron", StateTreeRunStatus::Succeeded),
        ("bt:lose.bt.ron", StateTreeRunStatus::Failed),
    ] {
        let def = state_def(path);
        let mut tree = StateTree::compile_with(&def, &registry, &load(&bt_files)).expect("compiles");
        let (mut app, entity) = world(Blackboard::new());
        assert_eq!(tick_state(&mut app, entity, &mut tree), want, "{path}");
        assert_eq!(
            tick_state(&mut app, entity, &mut tree),
            want,
            "a finished tree stays finished"
        );
    }

    let state_files = files(&[
        ("win.statetree.ron", &state_text("reactive::Win")),
        ("lose.statetree.ron", &state_text("reactive::Lose")),
    ]);
    for (path, want) in [
        ("win.statetree.ron", BtStatus::Success),
        ("lose.statetree.ron", BtStatus::Failure),
    ] {
        let def = one(DefNode {
            name: "bridge".into(),
            kind: DefKind::StateTree,
            task: path.into(),
            ..DefNode::default()
        });
        let mut tree = BehaviorTree::compile_with(&def, &registry, &load(&state_files)).expect("compiles");
        let (mut app, entity) = world(Blackboard::new());
        assert_eq!(tick(&mut app, entity, &mut tree), want, "{path}");
        assert_eq!(
            tick(&mut app, entity, &mut tree),
            want,
            "a finished bridge stays finished"
        );
    }
}

#[test]
fn a_running_bridge_stays_running_for_a_fixed_number_of_steps() {
    let registry = registry();
    let bt = files(&[("hold.bt.ron", &leaf_bt("reactive::Hold"))]);
    let mut state =
        StateTree::compile_with(&state_def("bt:hold.bt.ron"), &registry, &load(&bt)).expect("compiles");
    let mut board = Blackboard::new();
    board.insert("steps", 2_i64);
    let (mut app, entity) = world(board);
    assert_eq!(
        tick_state(&mut app, entity, &mut state),
        StateTreeRunStatus::Running
    );
    assert_eq!(
        tick_state(&mut app, entity, &mut state),
        StateTreeRunStatus::Running
    );
    assert_eq!(
        tick_state(&mut app, entity, &mut state),
        StateTreeRunStatus::Succeeded
    );

    let states = files(&[("hold.statetree.ron", &state_text("reactive::Hold"))]);
    let def = one(DefNode {
        name: "bridge".into(),
        kind: DefKind::StateTree,
        task: "hold.statetree.ron".into(),
        ..DefNode::default()
    });
    let mut tree = BehaviorTree::compile_with(&def, &registry, &load(&states)).expect("compiles");
    let mut board = Blackboard::new();
    board.insert("steps", 2_i64);
    let (mut app, entity) = world(board);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Success);
}

#[test]
fn a_bridge_cycle_is_a_compile_error_and_a_bad_path_is_a_validator_error() {
    let registry = registry();
    let text = files(&[
        (
            "loop.bt.ron",
            "(\n    format_version: 2,\n    root: 0,\n    nodes: [\n        (name: \"loop\", kind: state_tree, children: [], key: \"\", seconds: 0.0, times: None, task: \"loop.statetree.ron\", policy: require_all),\n    ],\n)\n",
        ),
        (
            "loop.statetree.ron",
            "(\n    format_version: 2,\n    root: \"Root\",\n    states: [\n        (name: \"Root\", tasks: [\"bt:loop.bt.ron\"]),\n    ],\n)\n",
        ),
    ]);
    let err = match StateTree::compile_with(&state_def("bt:loop.bt.ron"), &registry, &load(&text)) {
        Err(err) => err,
        Ok(_) => panic!("expected a cycle"),
    };
    assert_eq!(err.reason(), "bad_behavior_tree");
    match err {
        StateTreeError::BadBehaviorTree { detail, .. } => {
            assert!(detail.contains("bridge_cycle"), "{detail}")
        }
        other => panic!("expected bad_behavior_tree, got {other:?}"),
    }

    let empty = one(DefNode {
        name: "bridge".into(),
        kind: DefKind::StateTree,
        ..DefNode::default()
    });
    assert!(
        validate(&empty, None)
            .iter()
            .any(|issue| issue.code == "empty_state_tree")
    );
    let bad = one(DefNode {
        name: "bridge".into(),
        kind: DefKind::StateTree,
        task: "notes.txt".into(),
        ..DefNode::default()
    });
    assert!(
        validate(&bad, None)
            .iter()
            .any(|issue| issue.code == "bad_state_tree")
    );
    let missing = one(DefNode {
        name: "bridge".into(),
        kind: DefKind::StateTree,
        task: "missing.statetree.ron".into(),
        ..DefNode::default()
    });
    assert!(
        validate(&missing, None)
            .iter()
            .all(|issue| issue.code != "bad_state_tree")
    );
    match BehaviorTree::compile_with(&missing, &registry, &|_| None) {
        Err(BtCompileError::BadStateTree { path, .. }) => assert_eq!(path, "missing.statetree.ron"),
        Err(other) => panic!("expected a missing asset, got {other:?}"),
        Ok(_) => panic!("expected a missing asset, the tree compiled"),
    }
}

#[test]
fn abort_on_a_leaf_and_an_abort_without_a_key_are_validator_errors() {
    let leaf = DefNode {
        name: "leaf".into(),
        kind: DefKind::Leaf,
        task: "reactive::Win".into(),
        abort: AbortMode::Both,
        key: "go".into(),
        ..DefNode::default()
    };
    assert!(
        validate(&one(leaf), Some(&registry()))
            .iter()
            .any(|issue| issue.code == "abort_not_allowed")
    );
    let bare = DefNode {
        name: "gate".into(),
        kind: DefKind::Inverter,
        children: vec![1],
        abort: AbortMode::AbortSelf,
        ..DefNode::default()
    };
    let child = DefNode {
        name: "leaf".into(),
        kind: DefKind::Leaf,
        task: "reactive::Win".into(),
        ..DefNode::default()
    };
    let def = BehaviorTreeDef {
        format_version: 2,
        root: 0,
        nodes: vec![bare, child],
    };
    assert!(
        validate(&def, Some(&registry()))
            .iter()
            .any(|issue| issue.code == "abort_without_key")
    );
}

#[test]
fn an_abort_mode_round_trips_through_canonical_text() {
    let def = one(DefNode {
        name: "go".into(),
        kind: DefKind::Condition,
        key: "go".into(),
        abort: AbortMode::AbortSelf,
        ..DefNode::default()
    });
    let text = bt_edit::to_canonical_ron(&def).expect("writes");
    assert!(text.contains("abort:"), "{text}");
    assert_eq!(bt_edit::parse_bt_ron(&text).expect("reads"), def);
}

/// A `.bt.ron` text of `depth` inverters over one `reactive::Win` leaf.
fn inverter_bt(depth: usize) -> String {
    let mut nodes = String::new();
    for index in 0..depth {
        nodes.push_str(&format!(
            "        (name: \"not{index}\", kind: inverter, children: [{}], key: \"\", seconds: 0.0, times: None, task: \"\", policy: require_all),\n",
            index + 1
        ));
    }
    nodes.push_str(
        "        (name: \"leaf\", kind: leaf, children: [], key: \"\", seconds: 0.0, times: None, task: \"reactive::Win\", policy: require_all),\n",
    );
    format!("(\n    format_version: 2,\n    root: 0,\n    nodes: [\n{nodes}    ],\n)\n")
}

/// rev-62 note 1 (finding 5 follow-up): the depth cap counts a whole bridge chain, not one
/// tree. `tick_node` recurses one frame per level, and the bridge task's tick runs the inner
/// tree on top of the outer tree's frames, so two trees that each fit under `MAX_BT_DEPTH`
/// must not compile into a chain deeper than it. A chain that fits still runs.
#[test]
fn a_bridge_chain_deeper_than_the_cap_is_refused_and_one_that_fits_runs() {
    let registry = registry();
    // The outer tree: `outer` inverters over one `state_tree` bridge node (height `outer + 1`).
    let bridged = |outer: usize, inner: usize| {
        let mut nodes: Vec<DefNode> = (0..outer)
            .map(|index| DefNode {
                name: format!("not{index}"),
                kind: DefKind::Inverter,
                children: vec![index + 1],
                ..DefNode::default()
            })
            .collect();
        nodes.push(DefNode {
            name: "bridge".into(),
            kind: DefKind::StateTree,
            task: "mid.statetree.ron".into(),
            ..DefNode::default()
        });
        let def = BehaviorTreeDef {
            format_version: 2,
            root: 0,
            nodes,
        };
        let files = files(&[
            (
                "mid.statetree.ron",
                "(format_version: 2, root: \"Root\", states: [\n        (name: \"Root\", tasks: [\"bt:inner.bt.ron\"]),\n    ],\n)\n",
            ),
            ("inner.bt.ron", &inverter_bt(inner)),
        ]);
        (def, files)
    };

    // 151 levels above the bridge, 151 below: 302 across the chain, over the 256 cap.
    let (def, files) = bridged(150, 150);
    let detail = match BehaviorTree::compile_with(&def, &registry, &load(&files)) {
        Err(BtCompileError::BadStateTree { detail, .. }) => detail,
        Ok(_) => panic!("a 302-level bridge chain compiled; its first tick would recurse 302 levels"),
        Err(other) => panic!("expected a chain depth error, got {other:?}"),
    };
    assert!(detail.contains("deep"), "{detail}");

    // 101 + 101 = 202 levels across the chain: it compiles and runs.
    let (def, files) = bridged(100, 100);
    let mut tree =
        BehaviorTree::compile_with(&def, &registry, &load(&files)).expect("202 levels fit the cap");
    let (mut app, entity) = world(Blackboard::new());
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Success);
}
