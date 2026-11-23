// SPDX-License-Identifier: MPL-2.0
//! Observer-abort semantics across running, succeeded and failed children (ADR-4543, ADR-7559).
//!
//! A decorator that watches a key is a gate: it is checked when the decorator is entered, and
//! re-checked only when the key changes while it runs. `self` stops the decorator's own running
//! branch, `lower_priority` stops a running lower sibling of a selector when the gate opens, `both`
//! does both. Steps are counted; nothing here waits on a clock.

use regia_ai::bt_edit::validate;
use regia_ai::{
    AbortMode, BehaviorTree, BehaviorTreeDef, Blackboard, BtCompileError, BtContext, BtStatus, BtTask,
    BtTaskRegistration, BtTaskRegistry, DefKind, DefNode, MAX_BT_DEPTH,
};
use regia_app::prelude::*;

fn bump(ctx: &mut BtContext<'_>, key: &str) {
    if let Some(mut board) = ctx.blackboard_mut() {
        let next = board.get_int(key) + 1;
        board.insert(key, next);
    }
}

macro_rules! counting_task {
    ($name:ident, $path:literal, $ticks:literal, $resets:literal, $status:expr) => {
        struct $name;
        impl BtTask for $name {
            fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
                bump(ctx, $ticks);
                $status
            }
            fn reset(&mut self, ctx: &mut BtContext<'_>) {
                bump(ctx, $resets);
            }
        }
        impl BtTaskRegistration for $name {
            fn construct() -> Box<dyn BtTask> {
                Box::new($name)
            }
            fn task_type_path() -> &'static str {
                $path
            }
        }
    };
}

counting_task!(
    Chase,
    "abort::Chase",
    "chase_ticks",
    "chase_aborts",
    BtStatus::Running
);
counting_task!(
    Flee,
    "abort::Flee",
    "flee_ticks",
    "flee_aborts",
    BtStatus::Running
);
counting_task!(
    Touch,
    "abort::Touch",
    "touched",
    "touch_resets",
    BtStatus::Success
);
counting_task!(Lose, "abort::Lose", "lost", "lose_resets", BtStatus::Failure);
counting_task!(
    Watch,
    "abort::Watch",
    "service_ticks",
    "service_resets",
    BtStatus::Success
);

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
        "abort::Hold"
    }
}

fn registry() -> BtTaskRegistry {
    let mut registry = BtTaskRegistry::default();
    registry.register::<Chase>();
    registry.register::<Flee>();
    registry.register::<Touch>();
    registry.register::<Lose>();
    registry.register::<Watch>();
    registry.register::<Hold>();
    registry
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

fn set(app: &mut App, entity: Entity, key: &str, value: bool) {
    app.world_mut()
        .get_mut::<Blackboard>(entity)
        .expect("blackboard")
        .insert(key, value);
}

fn leaf(name: &str, task: &str) -> DefNode {
    DefNode {
        name: name.into(),
        kind: DefKind::Leaf,
        task: task.into(),
        ..DefNode::default()
    }
}

/// A decorator of `kind` gated on the blackboard bool `allow` with `abort`, over node `child`.
fn gate(kind: DefKind, abort: AbortMode, child: usize) -> DefNode {
    DefNode {
        name: "gate".into(),
        kind,
        key: "allow".into(),
        abort,
        children: vec![child],
        // A service's task; ignored by the other decorators.
        task: "abort::Watch".into(),
        // Long enough that a timeout never expires in these tests.
        seconds: 100.0,
        ..DefNode::default()
    }
}

/// `0 selector [1 gate(kind, abort) -> 2 first, 3 fallback]`.
fn gated_selector(kind: DefKind, abort: AbortMode, first: &str, fallback: &str) -> BehaviorTreeDef {
    BehaviorTreeDef {
        format_version: 2,
        root: 0,
        nodes: vec![
            DefNode {
                name: "root".into(),
                kind: DefKind::Selector,
                children: vec![1, 3],
                ..DefNode::default()
            },
            gate(kind, abort, 2),
            leaf("first", first),
            leaf("fallback", fallback),
        ],
    }
}

const DECORATORS: [DefKind; 5] = [
    DefKind::Inverter,
    DefKind::Cooldown,
    DefKind::Repeat,
    DefKind::Timeout,
    DefKind::Service,
];

/// Finding 2: after a `self` abort the gate stays shut while its key is false. Before the fix the
/// branch re-entered on the very next tick and ran with the key still false.
#[test]
fn a_self_aborted_decorator_stays_shut_while_its_key_is_false() {
    for kind in DECORATORS {
        for abort in [AbortMode::AbortSelf, AbortMode::Both] {
            let label = format!("{kind:?} abort {abort:?}");
            let mut board = Blackboard::new();
            board.insert("allow", true);
            let (mut app, entity) = world(board);
            let def = gated_selector(kind, abort, "abort::Chase", "abort::Touch");
            let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
            assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
            assert_eq!(count(&app, entity, "chase_ticks"), 1, "{label}");

            set(&mut app, entity, "allow", false);
            assert_eq!(
                tick(&mut app, entity, &mut tree),
                BtStatus::Success,
                "{label}: aborted, the fallback ran"
            );
            assert_eq!(count(&app, entity, "chase_aborts"), 1, "{label}");
            assert_eq!(count(&app, entity, "touched"), 1, "{label}");

            for step in 0..3 {
                assert_eq!(
                    tick(&mut app, entity, &mut tree),
                    BtStatus::Success,
                    "{label}: step {step} after the abort"
                );
            }
            assert_eq!(
                count(&app, entity, "chase_ticks"),
                1,
                "{label}: the gated branch does not run while the key is false"
            );
            assert_eq!(count(&app, entity, "touched"), 4, "{label}");
            if kind == DefKind::Service {
                assert_eq!(
                    count(&app, entity, "service_ticks"),
                    1,
                    "{label}: a shut service does not tick its task"
                );
            }

            set(&mut app, entity, "allow", true);
            assert_eq!(
                tick(&mut app, entity, &mut tree),
                BtStatus::Running,
                "{label}: the gate opens again"
            );
            assert_eq!(count(&app, entity, "chase_ticks"), 2, "{label}");
            assert_eq!(count(&app, entity, "chase_aborts"), 1, "{label}");
        }
    }
}

/// Finding 2: a gate is checked on entry in every abort mode, so a branch whose key is false on the
/// first tick does not run. `lower_priority` and `both` then pre-empt the running fallback when the
/// key turns true; `self` alone does not.
#[test]
fn a_gated_decorator_is_checked_on_entry_and_lower_priority_preempts_when_it_opens() {
    for kind in DECORATORS {
        for abort in [AbortMode::AbortSelf, AbortMode::LowerPriority, AbortMode::Both] {
            let label = format!("{kind:?} abort {abort:?}");
            let mut board = Blackboard::new();
            board.insert("allow", false);
            let (mut app, entity) = world(board);
            let def = gated_selector(kind, abort, "abort::Chase", "abort::Flee");
            let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
            for _ in 0..2 {
                assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
            }
            assert_eq!(count(&app, entity, "chase_ticks"), 0, "{label}: shut on entry");
            assert_eq!(count(&app, entity, "flee_ticks"), 2, "{label}");
            assert_eq!(count(&app, entity, "service_ticks"), 0, "{label}");

            set(&mut app, entity, "allow", true);
            assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
            let preempts = matches!(abort, AbortMode::LowerPriority | AbortMode::Both);
            assert_eq!(
                count(&app, entity, "flee_aborts"),
                i64::from(preempts),
                "{label}: lower-priority pre-emption"
            );
            assert_eq!(count(&app, entity, "chase_ticks"), i64::from(preempts), "{label}");
            assert_eq!(
                count(&app, entity, "flee_ticks"),
                3 - i64::from(preempts),
                "{label}"
            );
        }
    }
}

/// A `lower_priority`-only gate does not stop its own running branch when its key turns false:
/// the gate is re-checked on the next entry, not while it runs.
#[test]
fn a_lower_priority_only_gate_keeps_its_running_branch() {
    for kind in DECORATORS {
        let label = format!("{kind:?}");
        let mut board = Blackboard::new();
        board.insert("allow", true);
        let (mut app, entity) = world(board);
        let def = gated_selector(kind, AbortMode::LowerPriority, "abort::Chase", "abort::Flee");
        let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
        assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
        set(&mut app, entity, "allow", false);
        for _ in 0..3 {
            assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
        }
        assert_eq!(count(&app, entity, "chase_ticks"), 4, "{label}");
        assert_eq!(count(&app, entity, "chase_aborts"), 0, "{label}");
        assert_eq!(count(&app, entity, "flee_ticks"), 0, "{label}");
    }
}

/// `0 sequence [1 gate(timeout, abort) -> 2 first, 3 then]`.
fn gated_sequence(abort: AbortMode, first: &str, then: &str) -> BehaviorTreeDef {
    BehaviorTreeDef {
        format_version: 2,
        root: 0,
        nodes: vec![
            DefNode {
                name: "root".into(),
                kind: DefKind::Sequence,
                children: vec![1, 3],
                ..DefNode::default()
            },
            gate(DefKind::Timeout, abort, 2),
            leaf("first", first),
            leaf("then", then),
        ],
    }
}

/// A gate whose child already succeeded has nothing to abort: the later sibling runs on. The next
/// time the sequence starts, the gate is checked on entry and stays shut.
#[test]
fn a_gate_over_a_succeeded_child_does_not_abort_and_is_checked_on_the_next_entry() {
    for abort in [AbortMode::AbortSelf, AbortMode::Both] {
        let label = format!("{abort:?}");
        let mut board = Blackboard::new();
        board.insert("allow", true);
        board.insert("steps", 2i64);
        let (mut app, entity) = world(board);
        let def = gated_sequence(abort, "abort::Touch", "abort::Hold");
        let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
        assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
        assert_eq!(count(&app, entity, "touched"), 1, "{label}");

        set(&mut app, entity, "allow", false);
        assert_eq!(
            tick(&mut app, entity, &mut tree),
            BtStatus::Running,
            "{label}: the later sibling runs on"
        );
        assert_eq!(count(&app, entity, "touch_resets"), 0, "{label}: nothing aborted");
        assert_eq!(
            tick(&mut app, entity, &mut tree),
            BtStatus::Success,
            "{label}: the sequence finishes"
        );
        assert_eq!(
            tick(&mut app, entity, &mut tree),
            BtStatus::Failure,
            "{label}: the restarted sequence meets a shut gate"
        );
        assert_eq!(count(&app, entity, "touched"), 1, "{label}");
    }
}

/// A gate whose child already failed is not running, so a key change aborts nothing below it. With
/// `lower_priority` (`both`) an opening gate still pre-empts the running lower sibling, which then
/// starts over because the gated branch fails again.
#[test]
fn a_gate_over_a_failed_child_aborts_only_the_lower_sibling_it_preempts() {
    for abort in [AbortMode::AbortSelf, AbortMode::Both] {
        let label = format!("{abort:?}");
        let mut board = Blackboard::new();
        board.insert("allow", true);
        let (mut app, entity) = world(board);
        let def = gated_selector(DefKind::Timeout, abort, "abort::Lose", "abort::Chase");
        let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
        assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
        assert_eq!(count(&app, entity, "lost"), 1, "{label}");

        set(&mut app, entity, "allow", false);
        assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
        assert_eq!(
            count(&app, entity, "chase_aborts"),
            0,
            "{label}: no running branch to stop"
        );
        assert_eq!(
            count(&app, entity, "lost"),
            1,
            "{label}: the failed gate is not re-entered"
        );

        set(&mut app, entity, "allow", true);
        assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running, "{label}");
        let preempts = abort == AbortMode::Both;
        assert_eq!(
            count(&app, entity, "chase_aborts"),
            i64::from(preempts),
            "{label}"
        );
        assert_eq!(count(&app, entity, "lost"), 1 + i64::from(preempts), "{label}");
        assert_eq!(count(&app, entity, "chase_ticks"), 3, "{label}");
    }
}

/// A `lower_priority` condition whose selector already finished has nothing to abort; the key
/// change takes effect when the selector starts over.
#[test]
fn a_lower_priority_condition_over_a_finished_selector_aborts_nothing() {
    let nodes = vec![
        DefNode {
            name: "root".into(),
            kind: DefKind::Selector,
            children: vec![1, 4],
            ..DefNode::default()
        },
        DefNode {
            name: "branch".into(),
            kind: DefKind::Sequence,
            children: vec![2, 3],
            ..DefNode::default()
        },
        DefNode {
            name: "go".into(),
            kind: DefKind::Condition,
            key: "go".into(),
            abort: AbortMode::LowerPriority,
            ..DefNode::default()
        },
        leaf("flee", "abort::Flee"),
        leaf("touch", "abort::Touch"),
    ];
    let def = BehaviorTreeDef {
        format_version: 2,
        root: 0,
        nodes,
    };
    let mut board = Blackboard::new();
    board.insert("go", false);
    let (mut app, entity) = world(board);
    let mut tree = BehaviorTree::compile(&def, &registry()).expect("compiles");
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Success);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Success);
    set(&mut app, entity, "go", true);
    assert_eq!(tick(&mut app, entity, &mut tree), BtStatus::Running);
    assert_eq!(
        count(&app, entity, "touch_resets"),
        0,
        "the finished branch is not reset"
    );
    assert_eq!(count(&app, entity, "touched"), 2);
    assert_eq!(count(&app, entity, "flee_ticks"), 1);
}

/// A chain of `n` inverters over one succeeding leaf.
fn inverter_chain(n: usize) -> BehaviorTreeDef {
    let mut nodes: Vec<DefNode> = (0..n)
        .map(|index| DefNode {
            name: format!("not{index}"),
            kind: DefKind::Inverter,
            children: vec![index + 1],
            ..DefNode::default()
        })
        .collect();
    nodes.push(leaf("touch", "abort::Touch"));
    BehaviorTreeDef {
        format_version: 1,
        root: 0,
        nodes,
    }
}

/// Finding 5: a tree deeper than [`MAX_BT_DEPTH`] is refused by the compiler and the validator
/// (`too_deep`) instead of overflowing the stack on the first tick. A tree at the cap runs.
#[test]
fn a_tree_deeper_than_the_cap_is_refused_and_one_at_the_cap_runs() {
    let deep = inverter_chain(40_000);
    match BehaviorTree::compile(&deep, &registry()) {
        Err(BtCompileError::Malformed(message)) => assert!(message.contains("deep"), "{message}"),
        Err(other) => panic!("expected a depth error, got {other:?}"),
        Ok(_) => panic!("a 40,001-level tree compiled; its first tick would overflow the stack"),
    }
    let issues = validate(&deep, Some(&registry()));
    assert!(
        issues.iter().any(|issue| issue.code == "too_deep"),
        "{:?}",
        issues.iter().map(|issue| issue.code).collect::<Vec<_>>()
    );

    let over = inverter_chain(MAX_BT_DEPTH);
    assert!(
        BehaviorTree::compile(&over, &registry()).is_err(),
        "one past the cap"
    );

    // MAX_BT_DEPTH nodes on the path: MAX_BT_DEPTH - 1 inverters and the leaf.
    let at_cap = inverter_chain(MAX_BT_DEPTH - 1);
    assert!(
        validate(&at_cap, Some(&registry()))
            .iter()
            .all(|issue| issue.code != "too_deep")
    );
    let mut tree = BehaviorTree::compile(&at_cap, &registry()).expect("a tree at the cap compiles");
    let (mut app, entity) = world(Blackboard::new());
    let want = if (MAX_BT_DEPTH - 1).is_multiple_of(2) {
        BtStatus::Success
    } else {
        BtStatus::Failure
    };
    assert_eq!(tick(&mut app, entity, &mut tree), want);
    assert_eq!(count(&app, entity, "touched"), 1);
}
