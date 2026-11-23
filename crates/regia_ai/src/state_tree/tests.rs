// SPDX-License-Identifier: MPL-2.0
//! Unit tests for state trees: definition checks, RON, selection, transition priority, events,
//! completion, task lifecycle and the ECS system.

use super::*;
use crate::blackboard::{BbValue, Blackboard};
use crate::task::{BtContext, BtStatus, BtTask, BtTaskRegistration};
use crate::tree::BtTaskRegistry;
use regia_app::prelude::*;

// ---- test tasks -------------------------------------------------------------------------

fn bump(ctx: &mut BtContext<'_>, key: &str) {
    if let Some(mut bb) = ctx.blackboard_mut() {
        let n = bb.get_int(key);
        bb.insert(key.to_string(), n + 1);
    }
}

macro_rules! register_task {
    ($t:ident, $path:literal) => {
        impl BtTaskRegistration for $t {
            fn construct() -> Box<dyn BtTask> {
                Box::new($t::default())
            }
            fn task_type_path() -> &'static str {
                $path
            }
        }
    };
}

/// Runs forever, counting its ticks on the blackboard key `run_ticks`; counts resets in `resets`.
#[derive(Default)]
struct Run;
impl BtTask for Run {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        bump(ctx, "run_ticks");
        BtStatus::Running
    }
    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        bump(ctx, "resets");
    }
}
register_task!(Run, "t::Run");

/// Succeeds on its first tick.
#[derive(Default)]
struct Win;
impl BtTask for Win {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        bump(ctx, "win_ticks");
        BtStatus::Success
    }
}
register_task!(Win, "t::Win");

/// Fails on its first tick.
#[derive(Default)]
struct Lose;
impl BtTask for Lose {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Failure
    }
}
register_task!(Lose, "t::Lose");

/// Succeeds on its third tick since the last reset; counts resets in `resets`.
#[derive(Default)]
struct Three {
    n: u32,
}
impl BtTask for Three {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        self.n += 1;
        if self.n >= 3 {
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }
    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        self.n = 0;
        bump(ctx, "resets");
    }
}
register_task!(Three, "t::Three");

/// Copies the int at `mood` to `seen` on every tick (shows what a state's `enter_set` wrote
/// before its first task tick).
#[derive(Default)]
struct Peek;
impl BtTask for Peek {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if let Some(mut bb) = ctx.blackboard_mut() {
            let v = bb.get_int("mood");
            bb.insert("seen".to_string(), v);
        }
        BtStatus::Running
    }
}
register_task!(Peek, "t::Peek");

/// Sets the boolean `ready` on every tick.
#[derive(Default)]
struct SetReady;
impl BtTask for SetReady {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if let Some(mut bb) = ctx.blackboard_mut() {
            bb.insert("ready".to_string(), true);
        }
        BtStatus::Success
    }
}
register_task!(SetReady, "t::SetReady");

fn registry() -> BtTaskRegistry {
    let mut r = BtTaskRegistry::default();
    r.register::<Run>();
    r.register::<Win>();
    r.register::<Lose>();
    r.register::<Three>();
    r.register::<Peek>();
    r.register::<SetReady>();
    r
}

// ---- definition helpers -----------------------------------------------------------------

fn st(name: &str, children: &[&str]) -> StateDef {
    StateDef {
        children: children.iter().map(|c| c.to_string()).collect(),
        ..StateDef::new(name)
    }
}

fn with_tasks(mut s: StateDef, tasks: &[&str]) -> StateDef {
    s.tasks = tasks.iter().map(|t| t.to_string()).collect();
    s
}

fn tr(trigger: Trigger, target: &str, priority: i32) -> TransitionDef {
    TransitionDef {
        trigger,
        condition: Condition::Always,
        priority,
        target: Target::State(target.to_string()),
        hold: 0.0,
    }
}

fn with_tr(mut s: StateDef, t: TransitionDef) -> StateDef {
    s.transitions.push(t);
    s
}

fn def_of(root: &str, states: Vec<StateDef>) -> StateTreeDef {
    StateTreeDef {
        states,
        ..StateTreeDef::new(root)
    }
}

fn key(k: &str) -> Condition {
    Condition::Key(k.to_string())
}

struct Rig {
    world: World,
    entity: Entity,
    tree: StateTree,
    dt: f32,
}

impl Rig {
    fn new(def: &StateTreeDef) -> Self {
        let tree = StateTree::compile(def, &registry()).expect("compiles");
        let mut world = World::new();
        let entity = world.spawn(Blackboard::default()).id();
        Self {
            world,
            entity,
            tree,
            dt: 0.1,
        }
    }

    fn with_dt(mut self, dt: f32) -> Self {
        self.dt = dt;
        self
    }

    fn reload(&mut self, next: StateTree) -> ReloadOutcome {
        let mut ctx = BtContext {
            world: &mut self.world,
            entity: self.entity,
            dt: self.dt,
        };
        self.tree.reload(next, &mut ctx)
    }

    fn tick(&mut self) -> StateTreeRunStatus {
        let mut ctx = BtContext {
            world: &mut self.world,
            entity: self.entity,
            dt: self.dt,
        };
        self.tree.tick(&mut ctx)
    }

    fn restart(&mut self) {
        let mut ctx = BtContext {
            world: &mut self.world,
            entity: self.entity,
            dt: self.dt,
        };
        self.tree.restart(&mut ctx);
    }

    fn set(&mut self, k: &str, v: impl Into<BbValue>) {
        let mut bb = self.world.get_mut::<Blackboard>(self.entity).expect("blackboard");
        bb.insert(k.to_string(), v);
    }

    fn int(&self, k: &str) -> i64 {
        self.world
            .get::<Blackboard>(self.entity)
            .expect("blackboard")
            .get_int(k)
    }

    fn path(&self) -> String {
        self.tree.active_path_text()
    }

    fn trace_text(&self) -> Vec<String> {
        self.tree.trace().map(ToString::to_string).collect()
    }
}

// ---- RON and validation -----------------------------------------------------------------

#[test]
fn ron_round_trips_a_full_definition() {
    let mut s = st("Root", &["A", "B"]);
    s.selection = Selection::Utility;
    s.enter = Condition::Not(Box::new(key("blocked")));
    let mut a = with_tasks(st("A", &[]), &["t::Run"]);
    a.utility = Some(UtilityDef {
        key: "u".into(),
        scale: 2.0,
        bias: 0.5,
    });
    a.transitions.push(TransitionDef {
        trigger: Trigger::Event("go".into()),
        condition: Condition::Compare {
            key: "hp".into(),
            op: CompareOp::Lt,
            value: 10.0,
        },
        priority: 3,
        target: Target::Next,
        hold: 0.0,
    });
    let mut def = def_of("Root", vec![s, a, st("B", &[])]);
    def.seed = 99;
    def.state_key = "mode".into();
    def.description = "demo".into();
    let text = def.to_ron().expect("writes");
    assert_eq!(StateTreeDef::from_ron(&text).expect("reads"), def);
}

#[test]
fn ron_example_in_the_docs_parses_and_compiles() {
    let text = r#"(
        format_version: 1,
        root: "Root",
        states: [
            (name: "Root", children: ["Idle", "Alert"]),
            (name: "Idle", transitions: [
                (trigger: Event("heard"), target: State("Alert")),
            ]),
            (name: "Alert", tasks: ["t::Win"], transitions: [
                (trigger: Succeeded, condition: Not(Key("hidden")), priority: 2, target: State("Idle")),
            ]),
        ],
    )"#;
    let def = StateTreeDef::from_ron(text).expect("parses");
    assert_eq!(def.states.len(), 3);
    assert_eq!(def.states[2].transitions[0].priority, 2);
    assert!(StateTree::compile(&def, &registry()).is_ok());
}

#[test]
fn a_newer_version_is_rejected_before_the_body_is_read() {
    let err = StateTreeDef::from_ron("(format_version: 3, root: 5, states: \"future shape\")")
        .expect_err("future version");
    assert_eq!(
        err,
        StateTreeError::UnsupportedVersion {
            found: 3,
            supported: 2
        }
    );
}

#[test]
fn malformed_ron_and_a_missing_version_are_parse_errors() {
    assert!(matches!(
        StateTreeDef::from_ron("not ron at all ("),
        Err(StateTreeError::Parse(_))
    ));
    assert!(matches!(
        StateTreeDef::from_ron("(root: \"R\", states: [])"),
        Err(StateTreeError::Parse(_))
    ));
    // Right version, wrong body.
    assert!(matches!(
        StateTreeDef::from_ron("(format_version: 1, root: 7, states: [])"),
        Err(StateTreeError::Parse(_))
    ));
}

#[test]
fn compile_rejects_a_wrong_version_in_code() {
    let mut def = def_of("R", vec![st("R", &[])]);
    def.format_version = 0;
    assert!(matches!(
        StateTree::compile(&def, &registry()),
        Err(StateTreeError::UnsupportedVersion { found: 0, .. })
    ));
}

fn compile_err(def: &StateTreeDef) -> StateTreeError {
    match StateTree::compile(def, &registry()) {
        Ok(_) => panic!("expected a compile error"),
        Err(e) => e,
    }
}

#[test]
fn compile_rejects_structural_mistakes() {
    assert_eq!(compile_err(&def_of("R", vec![])), StateTreeError::Empty);
    assert_eq!(
        compile_err(&def_of("R", vec![st("", &[])])),
        StateTreeError::EmptyName
    );
    assert_eq!(
        compile_err(&def_of("R", vec![st("R", &[]), st("R", &[])])),
        StateTreeError::DuplicateName("R".into())
    );
    assert!(matches!(
        compile_err(&def_of("Nope", vec![st("R", &[])])),
        StateTreeError::UnknownState { .. }
    ));
    assert!(matches!(
        compile_err(&def_of("R", vec![st("R", &["Ghost"])])),
        StateTreeError::UnknownState { .. }
    ));
    // Two parents.
    assert_eq!(
        compile_err(&def_of(
            "R",
            vec![
                st("R", &["A", "B"]),
                st("A", &["C"]),
                st("B", &["C"]),
                st("C", &[])
            ]
        )),
        StateTreeError::MultipleParents("C".into())
    );
    // Listed twice under one parent.
    assert_eq!(
        compile_err(&def_of("R", vec![st("R", &["A", "A"]), st("A", &[])])),
        StateTreeError::MultipleParents("A".into())
    );
    // The root as a child, and a state as its own child.
    assert_eq!(
        compile_err(&def_of("R", vec![st("R", &["R"])])),
        StateTreeError::MultipleParents("R".into())
    );
    assert!(matches!(
        compile_err(&def_of("R", vec![st("R", &[]), st("Loop", &["Loop"])])),
        StateTreeError::Unreachable(_)
    ));
    // A detached cycle.
    assert!(matches!(
        compile_err(&def_of("R", vec![st("R", &[]), st("X", &["Y"]), st("Y", &["X"])])),
        StateTreeError::Unreachable(_)
    ));
}

#[test]
fn compile_rejects_bad_references_and_values() {
    let bad_task = with_tasks(st("R", &[]), &["t::Missing"]);
    assert_eq!(
        compile_err(&def_of("R", vec![bad_task])),
        StateTreeError::UnknownTask {
            state: "R".into(),
            task: "t::Missing".into()
        }
    );
    let bad_target = with_tr(st("R", &[]), tr(Trigger::Tick, "Ghost", 0));
    assert!(matches!(
        compile_err(&def_of("R", vec![bad_target])),
        StateTreeError::UnknownState { .. }
    ));
    let mut next = st("R", &[]);
    next.transitions.push(TransitionDef {
        trigger: Trigger::Tick,
        condition: Condition::Always,
        priority: 0,
        target: Target::Next,
        hold: 0.0,
    });
    assert_eq!(
        compile_err(&def_of("R", vec![next])),
        StateTreeError::NoNextSibling("R".into())
    );
    let mut nan_weight = st("R", &[]);
    nan_weight.weight = f32::NAN;
    assert_eq!(
        compile_err(&def_of("R", vec![nan_weight])),
        StateTreeError::BadValue("R".into())
    );
    let mut inf_scale = st("R", &[]);
    inf_scale.utility = Some(UtilityDef {
        key: "k".into(),
        scale: f32::INFINITY,
        bias: 0.0,
    });
    assert_eq!(
        compile_err(&def_of("R", vec![inf_scale])),
        StateTreeError::BadValue("R".into())
    );
    let mut nan_cmp = st("R", &[]);
    nan_cmp.enter = Condition::Compare {
        key: "k".into(),
        op: CompareOp::Eq,
        value: f32::NAN,
    };
    assert_eq!(
        compile_err(&def_of("R", vec![nan_cmp])),
        StateTreeError::BadValue("R".into())
    );
    let empty_event = with_tr(st("R", &[]), tr(Trigger::Event(String::new()), "R", 0));
    assert_eq!(
        compile_err(&def_of("R", vec![empty_event])),
        StateTreeError::BadValue("R".into())
    );
}

#[test]
fn compile_rejects_a_condition_nested_too_deep() {
    let mut deep = key("x");
    for _ in 0..MAX_CONDITION_DEPTH {
        deep = Condition::Not(Box::new(deep));
    }
    assert_eq!(deep.depth(), MAX_CONDITION_DEPTH + 1);
    let mut s = st("R", &[]);
    s.enter = deep;
    assert_eq!(
        compile_err(&def_of("R", vec![s])),
        StateTreeError::ConditionTooDeep("R".into())
    );
    // Exactly at the limit is fine.
    let mut ok = key("x");
    for _ in 0..MAX_CONDITION_DEPTH - 1 {
        ok = Condition::Not(Box::new(ok));
    }
    let mut s = st("R", &[]);
    s.enter = ok;
    assert!(StateTree::compile(&def_of("R", vec![s]), &registry()).is_ok());
}

#[test]
fn compile_rejects_too_many_states() {
    let states: Vec<StateDef> = (0..=MAX_STATES).map(|i| st(&format!("s{i}"), &[])).collect();
    assert_eq!(
        compile_err(&def_of("s0", states)),
        StateTreeError::TooManyStates(MAX_STATES + 1)
    );
}

// ---- conditions -------------------------------------------------------------------------

#[test]
fn conditions_read_the_blackboard() {
    let mut bb = Blackboard::default();
    bb.insert("flag", true);
    bb.insert("off", false);
    bb.insert("hp", 7i32);
    bb.insert("name", "guard");
    assert!(Condition::Always.eval(&bb));
    assert!(key("flag").eval(&bb));
    assert!(!key("off").eval(&bb));
    assert!(!key("missing").eval(&bb));
    assert!(Condition::Has("off".into()).eval(&bb));
    assert!(!Condition::Has("missing".into()).eval(&bb));
    let cmp = |op, value| Condition::Compare {
        key: "hp".into(),
        op,
        value,
    };
    assert!(cmp(CompareOp::Lt, 8.0).eval(&bb));
    assert!(cmp(CompareOp::Le, 7.0).eval(&bb));
    assert!(cmp(CompareOp::Eq, 7.0).eval(&bb));
    assert!(cmp(CompareOp::Ne, 6.0).eval(&bb));
    assert!(cmp(CompareOp::Ge, 7.0).eval(&bb));
    assert!(cmp(CompareOp::Gt, 6.0).eval(&bb));
    assert!(!cmp(CompareOp::Gt, 7.0).eval(&bb));
    // A missing or non-numeric key fails every comparison, `Ne` included.
    let missing = Condition::Compare {
        key: "nope".into(),
        op: CompareOp::Ne,
        value: 0.0,
    };
    assert!(!missing.eval(&bb));
    let text_as_number = Condition::Compare {
        key: "name".into(),
        op: CompareOp::Ne,
        value: 0.0,
    };
    assert!(!text_as_number.eval(&bb));
    assert!(
        Condition::Text {
            key: "name".into(),
            value: "guard".into()
        }
        .eval(&bb)
    );
    assert!(
        !Condition::Text {
            key: "hp".into(),
            value: "7".into()
        }
        .eval(&bb)
    );
    assert!(Condition::Not(Box::new(key("off"))).eval(&bb));
    assert!(Condition::All(vec![]).eval(&bb));
    assert!(!Condition::Any(vec![]).eval(&bb));
    assert!(Condition::All(vec![key("flag"), cmp(CompareOp::Eq, 7.0)]).eval(&bb));
    assert!(!Condition::All(vec![key("flag"), key("off")]).eval(&bb));
    assert!(Condition::Any(vec![key("off"), key("flag")]).eval(&bb));
}

// ---- starting and the debug trace -------------------------------------------------------

#[test]
fn the_first_tick_enters_the_root_and_descends_to_a_leaf() {
    let def = def_of(
        "Root",
        vec![
            st("Root", &["Mid"]),
            st("Mid", &["Leaf", "Other"]),
            st("Leaf", &[]),
            st("Other", &[]),
        ],
    );
    let mut rig = Rig::new(&def);
    assert_eq!(rig.tree.status(), StateTreeRunStatus::NotStarted);
    assert_eq!(rig.path(), "");
    assert_eq!(rig.tick(), StateTreeRunStatus::Running);
    assert_eq!(rig.path(), "Root/Mid/Leaf");
    assert_eq!(rig.tree.active_path(), vec!["Root", "Mid", "Leaf"]);
    assert_eq!(rig.tree.active_leaf(), Some("Leaf"));
    assert!(rig.tree.is_active("Mid"));
    assert!(!rig.tree.is_active("Other"));
    assert_eq!(
        rig.trace_text(),
        vec!["tick 1: enter Root", "tick 1: enter Mid", "tick 1: enter Leaf"]
    );
}

#[test]
fn an_unenterable_root_fails_the_tree_at_start() {
    let mut root = st("Root", &[]);
    root.enter = key("ready");
    let mut rig = Rig::new(&def_of("Root", vec![root]));
    assert_eq!(rig.tick(), StateTreeRunStatus::Failed);
    assert_eq!(rig.path(), "");
    assert_eq!(
        rig.tree.trace().last().map(|t| t.kind.clone()),
        Some(TraceKind::StartFailed)
    );
    // It stays failed.
    rig.set("ready", true);
    assert_eq!(rig.tick(), StateTreeRunStatus::Failed);
}

#[test]
fn enter_conditions_gate_the_first_selection() {
    let mut a = st("A", &[]);
    a.enter = key("a_ok");
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    let mut rig = Rig::new(&def);
    rig.set("a_ok", true);
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
}

#[test]
fn a_parent_with_no_enterable_child_cannot_be_entered() {
    let mut a = st("A", &[]);
    a.enter = key("never");
    let def = def_of("Root", vec![st("Root", &["A"]), a]);
    let mut rig = Rig::new(&def);
    assert_eq!(rig.tick(), StateTreeRunStatus::Failed);
}

#[test]
fn the_trace_is_bounded_and_drops_the_oldest() {
    // Two states bouncing every tick produce more than MAX_TRACE_ENTRIES entries.
    let def = def_of(
        "Root",
        vec![
            st("Root", &["A", "B"]),
            with_tr(st("A", &[]), tr(Trigger::Tick, "B", 0)),
            with_tr(st("B", &[]), tr(Trigger::Tick, "A", 0)),
        ],
    );
    let mut rig = Rig::new(&def);
    for _ in 0..60 {
        rig.tick();
    }
    assert_eq!(rig.tree.trace().count(), MAX_TRACE_ENTRIES);
    let last = rig.tree.trace().last().expect("entries").tick;
    assert_eq!(last, 60);
    let first = rig.tree.trace().next().expect("entries").tick;
    assert!(
        first > 1,
        "the oldest entries were dropped, first is tick {first}"
    );
    let taken = rig.tree.take_trace();
    assert_eq!(taken.len(), MAX_TRACE_ENTRIES);
    assert_eq!(rig.tree.trace().count(), 0);
}

// ---- transitions on tick: priority ------------------------------------------------------

fn three_leaves(a: StateDef) -> StateTreeDef {
    def_of(
        "Root",
        vec![st("Root", &["A", "B", "C"]), a, st("B", &[]), st("C", &[])],
    )
}

#[test]
fn the_highest_priority_tick_transition_wins() {
    let a = with_tr(
        with_tr(st("A", &[]), tr(Trigger::Tick, "B", 1)),
        tr(Trigger::Tick, "C", 5),
    );
    let mut rig = Rig::new(&three_leaves(a));
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
    assert!(
        rig.trace_text()
            .contains(&"tick 1: transition A -> C on tick (priority 5)".to_string())
    );
}

#[test]
fn a_negative_priority_loses_to_zero() {
    let a = with_tr(
        with_tr(st("A", &[]), tr(Trigger::Tick, "B", -4)),
        tr(Trigger::Tick, "C", 0),
    );
    let mut rig = Rig::new(&three_leaves(a));
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
}

#[test]
fn equal_priority_goes_to_declaration_order() {
    let a = with_tr(
        with_tr(st("A", &[]), tr(Trigger::Tick, "C", 2)),
        tr(Trigger::Tick, "B", 2),
    );
    let mut rig = Rig::new(&three_leaves(a));
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
}

#[test]
fn equal_priority_prefers_the_deeper_state() {
    // Root and its active leaf A both match with priority 0: the leaf's transition wins.
    let root = with_tr(st("Root", &["A", "B", "C"]), tr(Trigger::Tick, "B", 0));
    let a = with_tr(st("A", &[]), tr(Trigger::Tick, "C", 0));
    let def = def_of("Root", vec![root, a, st("B", &[]), st("C", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
}

#[test]
fn an_ancestor_with_higher_priority_beats_the_leaf() {
    let root = with_tr(st("Root", &["A", "B", "C"]), tr(Trigger::Tick, "B", 9));
    let a = with_tr(st("A", &[]), tr(Trigger::Tick, "C", 1));
    let def = def_of("Root", vec![root, a, st("B", &[]), st("C", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
}

#[test]
fn a_false_guard_removes_a_transition_from_the_pool() {
    let mut guarded = tr(Trigger::Tick, "C", 9);
    guarded.condition = key("go");
    let a = with_tr(with_tr(st("A", &[]), guarded), tr(Trigger::Tick, "B", 1));
    let mut rig = Rig::new(&three_leaves(a.clone()));
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    let mut rig = Rig::new(&three_leaves(a));
    rig.set("go", true);
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
}

#[test]
fn a_blocked_best_transition_falls_back_to_the_next_best() {
    let mut c = st("C", &[]);
    c.enter = key("c_open");
    let a = with_tr(
        with_tr(st("A", &[]), tr(Trigger::Tick, "C", 9)),
        tr(Trigger::Tick, "B", 1),
    );
    let def = def_of("Root", vec![st("Root", &["A", "B", "C"]), a, st("B", &[]), c]);
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    assert!(
        rig.trace_text()
            .contains(&"tick 1: rejected A -> C (cannot enter)".to_string())
    );
}

#[test]
fn a_tick_transition_to_a_sibling_keeps_the_shared_ancestors_and_their_tasks() {
    let root = with_tasks(st("Root", &["A", "B"]), &["t::Run"]);
    let a = with_tr(st("A", &[]), tr(Trigger::Tick, "B", 0));
    let mut rig = Rig::new(&def_of("Root", vec![root, a, st("B", &[])]));
    rig.tick();
    rig.tick();
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    // Root's task ticked on all three ticks and was never reset by the A -> B hop.
    assert_eq!(rig.int("run_ticks"), 3);
    assert_eq!(rig.int("resets"), 0);
    let text = rig.trace_text();
    assert!(!text.iter().any(|l| l.contains("exit Root")));
}

#[test]
fn a_transition_into_a_subtree_enters_the_missing_ancestors_and_descends() {
    let a = with_tr(st("A", &[]), tr(Trigger::Tick, "Group", 0));
    let def = def_of(
        "Root",
        vec![
            st("Root", &["A", "Group"]),
            a,
            st("Group", &["G1", "G2"]),
            st("G1", &[]),
            st("G2", &[]),
        ],
    );
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/Group/G1");
}

#[test]
fn a_transition_to_an_ancestor_re_enters_it_and_reselects_below() {
    let mut g1 = st("G1", &[]);
    g1.transitions.push(tr(Trigger::Event("home".into()), "Group", 0));
    let mut g2 = st("G2", &[]);
    g2.enter = key("g2");
    let def = def_of(
        "Root",
        vec![st("Root", &["Group"]), st("Group", &["G2", "G1"]), g1, g2],
    );
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/Group/G1");
    rig.set("g2", true);
    assert!(rig.tree.send_event("home"));
    rig.tick();
    assert_eq!(rig.path(), "Root/Group/G2");
}

#[test]
fn next_moves_to_the_next_sibling() {
    let mut a = st("A", &[]);
    a.transitions.push(TransitionDef {
        trigger: Trigger::Tick,
        condition: Condition::Always,
        priority: 0,
        target: Target::Next,
        hold: 0.0,
    });
    let mut b = st("B", &[]);
    b.transitions.push(TransitionDef {
        trigger: Trigger::Tick,
        condition: Condition::Always,
        priority: 0,
        target: Target::Next,
        hold: 0.0,
    });
    let def = def_of("Root", vec![st("Root", &["A", "B", "C"]), a, b, st("C", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
}

#[test]
fn at_most_one_tick_transition_is_taken_per_tick() {
    let def = def_of(
        "Root",
        vec![
            st("Root", &["A", "B"]),
            with_tr(st("A", &[]), tr(Trigger::Tick, "B", 0)),
            with_tr(st("B", &[]), tr(Trigger::Tick, "A", 0)),
        ],
    );
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
}

// ---- events -----------------------------------------------------------------------------

fn event_def() -> StateTreeDef {
    def_of(
        "Root",
        vec![
            st("Root", &["A", "B", "C"]),
            with_tr(st("A", &[]), tr(Trigger::Event("go".into()), "B", 0)),
            with_tr(st("B", &[]), tr(Trigger::Event("go".into()), "C", 0)),
            st("C", &[]),
        ],
    )
}

#[test]
fn an_event_triggers_its_transition() {
    let mut rig = Rig::new(&event_def());
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
    rig.tick();
    assert_eq!(rig.path(), "Root/A", "no event, no move");
    assert!(rig.tree.send_event("go"));
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
}

#[test]
fn an_unmatched_event_is_ignored_and_not_remembered() {
    let mut rig = Rig::new(&event_def());
    rig.tick();
    rig.tree.send_event("other");
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
    assert!(
        rig.trace_text()
            .contains(&"tick 2: event other ignored".to_string())
    );
    // The ignored event is gone: a later tick does not replay it.
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
}

#[test]
fn an_event_arriving_before_its_state_is_active_is_not_kept() {
    let mut rig = Rig::new(&event_def());
    rig.tick();
    rig.tree.send_event("go"); // A -> B
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    // "go" was consumed with the tick; B needs a fresh one to reach C.
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
}

#[test]
fn queued_events_are_handled_in_order_within_one_tick() {
    let mut rig = Rig::new(&event_def());
    rig.tick();
    rig.tree.send_event("go");
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(
        rig.path(),
        "Root/C",
        "the second event saw the state the first one reached"
    );
}

#[test]
fn event_transitions_obey_priority_and_guards() {
    let mut low = tr(Trigger::Event("go".into()), "B", 1);
    low.condition = Condition::Always;
    let mut high = tr(Trigger::Event("go".into()), "C", 5);
    high.condition = key("allowed");
    let a = with_tr(with_tr(st("A", &[]), low), high);
    let def = three_leaves(a);
    let mut rig = Rig::new(&def);
    rig.tick();
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(
        rig.path(),
        "Root/B",
        "the guarded high-priority transition does not match"
    );
    let mut rig = Rig::new(&def);
    rig.set("allowed", true);
    rig.tick();
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
}

#[test]
fn an_ancestor_can_handle_an_event_for_all_its_descendants() {
    let root = with_tr(
        st("Root", &["A", "B"]),
        tr(Trigger::Event("panic".into()), "B", 0),
    );
    let mut rig = Rig::new(&def_of("Root", vec![root, st("A", &[]), st("B", &[])]));
    rig.tick();
    rig.tree.send_event("panic");
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
}

#[test]
fn event_transitions_do_not_fire_without_an_event_and_tick_transitions_ignore_events() {
    let a = with_tr(st("A", &[]), tr(Trigger::Event("go".into()), "B", 0));
    let mut rig = Rig::new(&def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]));
    for _ in 0..5 {
        rig.tick();
    }
    assert_eq!(rig.path(), "Root/A");
}

#[test]
fn the_event_queue_is_bounded() {
    let mut rig = Rig::new(&event_def());
    rig.tick();
    let accepted = (0..MAX_PENDING_EVENTS + 6)
        .filter(|_| rig.tree.send_event("x"))
        .count();
    assert_eq!(accepted, MAX_PENDING_EVENTS);
    assert!(
        rig.tree
            .trace()
            .any(|t| matches!(&t.kind, TraceKind::EventDropped(e) if e == "x"))
    );
    // Delivering the queue clears it, so sending works again.
    rig.tick();
    assert!(rig.tree.send_event("go"));
}

// ---- completion -------------------------------------------------------------------------

#[test]
fn a_succeeded_task_fires_the_succeeded_transition() {
    let a = with_tr(
        with_tasks(st("A", &[]), &["t::Win"]),
        tr(Trigger::Succeeded, "B", 0),
    );
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
}

#[test]
fn a_failed_task_fires_the_failed_transition_not_the_succeeded_one() {
    let a = with_tr(
        with_tr(
            with_tasks(st("A", &[]), &["t::Lose"]),
            tr(Trigger::Succeeded, "B", 9),
        ),
        tr(Trigger::Failed, "C", 0),
    );
    let mut rig = Rig::new(&three_leaves(a));
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
}

#[test]
fn completed_fires_for_both_outcomes() {
    for (task, expect) in [("t::Win", "Root/B"), ("t::Lose", "Root/B")] {
        let a = with_tr(with_tasks(st("A", &[]), &[task]), tr(Trigger::Completed, "B", 0));
        let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
        let mut rig = Rig::new(&def);
        rig.tick();
        assert_eq!(rig.path(), expect, "task {task}");
    }
}

#[test]
fn a_running_task_does_not_complete_the_state() {
    let a = with_tr(
        with_tasks(st("A", &[]), &["t::Run"]),
        tr(Trigger::Completed, "B", 0),
    );
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def);
    for _ in 0..5 {
        rig.tick();
    }
    assert_eq!(rig.path(), "Root/A");
    assert_eq!(rig.int("run_ticks"), 5);
}

#[test]
fn all_tasks_of_a_state_must_succeed_before_it_completes() {
    let a = with_tr(
        with_tasks(st("A", &[]), &["t::Win", "t::Three"]),
        tr(Trigger::Succeeded, "B", 0),
    );
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    rig.tick();
    assert_eq!(rig.path(), "Root/A", "Three needs a third tick");
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    assert_eq!(rig.int("win_ticks"), 1, "a finished task is not ticked again");
}

#[test]
fn a_failed_task_stops_the_later_tasks_of_its_state() {
    let a = with_tr(
        with_tasks(st("A", &[]), &["t::Lose", "t::Run"]),
        tr(Trigger::Failed, "B", 0),
    );
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    assert_eq!(rig.int("run_ticks"), 0, "the task after the failure never ran");
}

#[test]
fn a_leaf_completion_bubbles_through_task_less_parents() {
    let group = with_tr(st("Group", &["Work"]), tr(Trigger::Succeeded, "Done", 0));
    let work = with_tasks(st("Work", &[]), &["t::Win"]);
    let def = def_of(
        "Root",
        vec![st("Root", &["Group", "Done"]), group, work, st("Done", &[])],
    );
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/Done");
}

#[test]
fn a_parents_own_tasks_complete_it_while_a_child_keeps_running() {
    let root = with_tr(
        with_tasks(st("Root", &["A", "Done"]), &["t::Win"]),
        tr(Trigger::Succeeded, "Done", 0),
    );
    let a = with_tasks(st("A", &[]), &["t::Run"]);
    let mut rig = Rig::new(&def_of("Root", vec![root, a, st("Done", &[])]));
    rig.tick();
    assert_eq!(rig.path(), "Root/Done");
}

#[test]
fn an_unhandled_completion_finishes_the_tree() {
    let a = with_tasks(st("A", &[]), &["t::Win"]);
    let mut rig = Rig::new(&def_of("Root", vec![st("Root", &["A"]), a]));
    assert_eq!(rig.tick(), StateTreeRunStatus::Succeeded);
    assert_eq!(rig.path(), "");
    assert_eq!(
        rig.tree.trace().last().map(|t| t.kind.clone()),
        Some(TraceKind::Finished(StateTreeRunStatus::Succeeded))
    );

    let a = with_tasks(st("A", &[]), &["t::Lose"]);
    let mut rig = Rig::new(&def_of("Root", vec![st("Root", &["A"]), a]));
    assert_eq!(rig.tick(), StateTreeRunStatus::Failed);
}

#[test]
fn a_leaf_without_tasks_never_completes_by_itself() {
    let mut rig = Rig::new(&def_of("Root", vec![st("Root", &["A"]), st("A", &[])]));
    for _ in 0..10 {
        assert_eq!(rig.tick(), StateTreeRunStatus::Running);
    }
}

#[test]
fn succeed_and_fail_targets_finish_the_tree_and_stop_ticking() {
    for (target, expect) in [
        (Target::Succeed, StateTreeRunStatus::Succeeded),
        (Target::Fail, StateTreeRunStatus::Failed),
    ] {
        let mut a = with_tasks(st("A", &[]), &["t::Run"]);
        a.transitions.push(TransitionDef {
            trigger: Trigger::Event("end".into()),
            condition: Condition::Always,
            priority: 0,
            target,
            hold: 0.0,
        });
        let mut rig = Rig::new(&def_of("Root", vec![st("Root", &["A"]), a]));
        rig.tick();
        rig.tree.send_event("end");
        assert_eq!(rig.tick(), expect);
        assert_eq!(rig.path(), "");
        // Exiting reset the running task once, and later ticks do nothing.
        assert_eq!(rig.int("resets"), 1);
        let ticks = rig.int("run_ticks");
        assert_eq!(rig.tick(), expect);
        assert_eq!(rig.int("run_ticks"), ticks);
        rig.tree.send_event("end");
        assert_eq!(rig.tick(), expect);
        assert_eq!(rig.path(), "");
    }
}

#[test]
fn restart_returns_to_the_start_state() {
    let a = with_tr(st("A", &[]), tr(Trigger::Event("go".into()), "B", 0));
    let mut rig = Rig::new(&def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]));
    rig.tick();
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    rig.restart();
    assert_eq!(rig.tree.status(), StateTreeRunStatus::NotStarted);
    assert_eq!(rig.path(), "");
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
}

// ---- task lifecycle ---------------------------------------------------------------------

#[test]
fn leaving_a_state_resets_the_tasks_that_ran_and_re_entering_starts_fresh() {
    let a = with_tr(
        with_tasks(st("A", &[]), &["t::Three"]),
        tr(Trigger::Event("away".into()), "B", 0),
    );
    let b = with_tr(st("B", &[]), tr(Trigger::Event("back".into()), "A", 0));
    let mut rig = Rig::new(&def_of("Root", vec![st("Root", &["A", "B"]), a, b]));
    rig.tick();
    rig.tick();
    rig.tree.send_event("away");
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    assert_eq!(rig.int("resets"), 1);
    rig.tree.send_event("back");
    rig.tick(); // enters A; its task ticks this tick: n = 1, not 3
    rig.tick();
    assert_eq!(
        rig.path(),
        "Root/A",
        "the task restarted from zero and has not finished"
    );
    rig.tick();
    // Third tick since re-entry completes it; with no Succeeded transition the tree finishes.
    assert_eq!(rig.tree.status(), StateTreeRunStatus::Succeeded);
}

#[test]
fn a_task_that_never_ticked_is_not_reset() {
    // B is entered by an event and left by another event before any task tick.
    let a = with_tr(st("A", &[]), tr(Trigger::Event("one".into()), "B", 0));
    let b = with_tr(
        with_tasks(st("B", &[]), &["t::Run"]),
        tr(Trigger::Event("two".into()), "C", 0),
    );
    let def = def_of("Root", vec![st("Root", &["A", "B", "C"]), a, b, st("C", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    rig.tree.send_event("one");
    rig.tree.send_event("two");
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
    assert_eq!(rig.int("run_ticks"), 0);
    assert_eq!(rig.int("resets"), 0);
}

#[test]
fn a_self_transition_exits_and_re_enters_the_state() {
    let a = with_tr(
        with_tasks(st("A", &[]), &["t::Three"]),
        tr(Trigger::Event("again".into()), "A", 0),
    );
    let mut rig = Rig::new(&def_of("Root", vec![st("Root", &["A"]), a]));
    rig.tick();
    rig.tick();
    rig.tree.send_event("again");
    rig.tick();
    assert_eq!(rig.int("resets"), 1);
    assert_eq!(rig.path(), "Root/A");
    let text = rig.trace_text();
    assert!(text.contains(&"tick 3: exit A".to_string()));
    assert!(text.contains(&"tick 3: enter A".to_string()));
}

#[test]
fn tasks_receive_the_agent_entity_and_blackboard() {
    #[derive(Default)]
    struct Mark;
    impl BtTask for Mark {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let dt = ctx.dt;
            if let Some(mut bb) = ctx.blackboard_mut() {
                bb.insert("seen_dt", dt);
            }
            BtStatus::Running
        }
    }
    register_task!(Mark, "t::Mark");
    let mut reg = registry();
    reg.register::<Mark>();
    let a = with_tasks(st("A", &[]), &["t::Mark"]);
    let def = def_of("Root", vec![st("Root", &["A"]), a]);
    let mut tree = StateTree::compile(&def, &reg).expect("compiles");
    let mut world = World::new();
    let entity = world.spawn(Blackboard::default()).id();
    let mut ctx = BtContext {
        world: &mut world,
        entity,
        dt: 0.25,
    };
    tree.tick(&mut ctx);
    assert_eq!(
        world.get::<Blackboard>(entity).expect("bb").get_float("seen_dt"),
        0.25
    );
}

// ---- blackboard integration -------------------------------------------------------------

#[test]
fn state_key_mirrors_the_active_leaf_on_the_blackboard() {
    let a = with_tr(st("A", &[]), tr(Trigger::Event("go".into()), "B", 0));
    let mut def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    def.state_key = "mode".into();
    let mut rig = Rig::new(&def);
    rig.tick();
    let bb = |rig: &Rig| {
        rig.world
            .get::<Blackboard>(rig.entity)
            .expect("bb")
            .get_string("mode")
            .to_string()
    };
    assert_eq!(bb(&rig), "A");
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(bb(&rig), "B");
}

#[test]
fn state_key_is_cleared_when_the_tree_finishes_or_restarts() {
    // The key used to keep naming the last leaf of a finished tree (ADR-1660).
    let a = with_tr(st("A", &[]), tr(Trigger::Event("go".into()), "B", 0));
    let mut b = st("B", &[]);
    b.transitions.push(TransitionDef {
        trigger: Trigger::Event("end".into()),
        condition: Condition::Always,
        priority: 0,
        target: Target::Succeed,
        hold: 0.0,
    });
    let mut def = def_of("Root", vec![st("Root", &["A", "B"]), a, b]);
    def.state_key = "mode".into();
    let mut rig = Rig::new(&def);
    let bb = |rig: &Rig| {
        rig.world
            .get::<Blackboard>(rig.entity)
            .expect("bb")
            .get_string("mode")
            .to_string()
    };
    rig.tick();
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(bb(&rig), "B");
    rig.tree.send_event("end");
    assert_eq!(rig.tick(), StateTreeRunStatus::Succeeded);
    assert_eq!(rig.path(), "");
    assert_eq!(bb(&rig), "", "a finished tree has no active leaf");
    rig.restart();
    rig.tick();
    assert_eq!(bb(&rig), "A");
    rig.restart();
    assert_eq!(bb(&rig), "", "a restarted tree has no active leaf until it ticks");
}

#[test]
fn state_key_is_cleared_when_the_start_fails() {
    // A stale key (a loaded blackboard, a reloaded definition) must not outlive a failed start
    // (audit-gameplay H1, ADR-1809).
    let mut root = st("Root", &[]);
    root.enter = key("ready");
    let mut def = def_of("Root", vec![root]);
    def.state_key = "mode".into();
    let mut rig = Rig::new(&def);
    rig.world
        .get_mut::<Blackboard>(rig.entity)
        .expect("bb")
        .insert("mode", "Attack");
    assert_eq!(rig.tick(), StateTreeRunStatus::Failed);
    assert_eq!(
        rig.world
            .get::<Blackboard>(rig.entity)
            .expect("bb")
            .get_string("mode"),
        ""
    );
}

#[test]
fn a_tree_on_an_entity_without_a_blackboard_still_runs() {
    let a = with_tr(st("A", &[]), tr(Trigger::Tick, "B", 0));
    let mut def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    def.state_key = "mode".into();
    let mut tree = StateTree::compile(&def, &registry()).expect("compiles");
    let mut world = World::new();
    let entity = world.spawn_empty().id();
    let mut ctx = BtContext {
        world: &mut world,
        entity,
        dt: 0.1,
    };
    tree.tick(&mut ctx);
    assert_eq!(tree.active_path_text(), "Root/B");
}

#[test]
fn blackboard_changes_drive_guards_between_ticks() {
    let mut flee = tr(Trigger::Tick, "Flee", 5);
    flee.condition = Condition::Compare {
        key: "hp".into(),
        op: CompareOp::Lt,
        value: 25.0,
    };
    let fight = with_tr(st("Fight", &[]), flee);
    let def = def_of(
        "Root",
        vec![st("Root", &["Fight", "Flee"]), fight, st("Flee", &[])],
    );
    let mut rig = Rig::new(&def);
    rig.set("hp", 80.0f32);
    rig.tick();
    assert_eq!(rig.path(), "Root/Fight");
    rig.set("hp", 10.0f32);
    rig.tick();
    assert_eq!(rig.path(), "Root/Flee");
}

// ---- selection --------------------------------------------------------------------------

fn utility_def() -> StateTreeDef {
    let mut root = st("Root", &["A", "B", "C"]);
    root.selection = Selection::Utility;
    let mut a = st("A", &[]);
    a.utility = Some(UtilityDef {
        key: "a".into(),
        scale: 1.0,
        bias: 0.0,
    });
    let mut b = st("B", &[]);
    b.utility = Some(UtilityDef {
        key: "b".into(),
        scale: 2.0,
        bias: 1.0,
    });
    def_of("Root", vec![root, a, b, st("C", &[])])
}

#[test]
fn utility_selection_takes_the_highest_score() {
    let mut rig = Rig::new(&utility_def());
    rig.set("a", 3.0f32);
    rig.set("b", 2.0f32); // 1 + 2 * 2 = 5
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    let mut rig = Rig::new(&utility_def());
    rig.set("a", 9.0f32);
    rig.set("b", 2.0f32);
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
}

#[test]
fn utility_ties_go_to_the_earlier_child_and_a_missing_key_scores_zero() {
    // a = 0 (missing) ties with C (no utility, 0): A comes first.
    let mut rig = Rig::new(&utility_def());
    rig.set("b", -5.0f32); // B scores -9
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
}

#[test]
fn utility_skips_non_finite_scores_and_unenterable_children() {
    let mut rig = Rig::new(&utility_def());
    rig.set("a", f32::NAN); // try_get_float rejects NaN, so A reads as 0 and ties with C
    rig.set("b", f32::INFINITY);
    rig.tick();
    // B reads as 0 too (non-finite input is rejected): bias 1 beats 0.
    assert_eq!(rig.path(), "Root/B");

    let mut def = utility_def();
    def.states[2].enter = key("b_ok");
    let mut rig = Rig::new(&def);
    rig.set("b", 100.0f32);
    rig.tick();
    assert_eq!(rig.path(), "Root/A", "B scores highest but cannot be entered");
}

#[test]
fn utility_is_re_evaluated_each_time_the_parent_is_entered() {
    let mut def = utility_def();
    def.states[0]
        .transitions
        .push(tr(Trigger::Event("rethink".into()), "Root", 0));
    let mut rig = Rig::new(&def);
    rig.set("a", 1.0f32); // A scores 1, B scores 1 + 2 * 0 = 1: the tie goes to A
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
    rig.set("b", 5.0f32); // B now scores 11
    rig.tick();
    assert_eq!(
        rig.path(),
        "Root/A",
        "no re-evaluation while the state stays active"
    );
    rig.tree.send_event("rethink");
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
}

#[test]
fn utility_scores_an_inexact_integer_input_instead_of_reading_zero() {
    // 16_777_217 is not exact in f32: the lossy value still beats a sibling's bias of 1.
    let mut rig = Rig::new(&utility_def());
    rig.set("a", 16_777_217_i64);
    rig.tick();
    assert_eq!(rig.path(), "Root/A");
}

#[test]
fn a_hold_never_completes_while_the_step_does_not_advance_time() {
    for dt in [0.0, -1.0, f32::NAN] {
        let mut rig = Rig::new(&hold_def()).with_dt(dt);
        rig.set("go", true);
        for _ in 0..30 {
            rig.tick();
            assert_eq!(rig.path(), "Root/A", "dt {dt}");
        }
    }
}

#[test]
fn weighted_selection_survives_weights_whose_f32_sum_overflows() {
    let def = weighted_def(3, [2.0e38, 2.0e38, 0.0]);
    let picks = weighted_sequence(&def, 40);
    assert!(picks.iter().all(|p| p == "A" || p == "B"), "{picks:?}");
}

fn weighted_def(seed: u64, weights: [f32; 3]) -> StateTreeDef {
    let mut root = st("Root", &["A", "B", "C"]);
    root.selection = Selection::Weighted;
    let mut states = vec![root, st("A", &[]), st("B", &[]), st("C", &[])];
    for (s, w) in states[1..].iter_mut().zip(weights) {
        s.weight = w;
    }
    StateTreeDef {
        seed,
        ..def_of("Root", states)
    }
}

fn weighted_sequence(def: &StateTreeDef, draws: usize) -> Vec<String> {
    let mut rig = Rig::new(def);
    let mut out = Vec::new();
    for _ in 0..draws {
        rig.tick();
        out.push(rig.tree.active_leaf().unwrap_or("").to_string());
        rig.restart();
    }
    out
}

#[test]
fn weighted_selection_is_deterministic_for_a_seed() {
    let def = weighted_def(7, [1.0, 1.0, 1.0]);
    assert_eq!(weighted_sequence(&def, 50), weighted_sequence(&def, 50));
}

#[test]
fn weighted_selection_follows_the_weights_and_never_picks_zero() {
    let def = weighted_def(1234, [3.0, 1.0, 0.0]);
    let picks = weighted_sequence(&def, 400);
    let count = |n: &str| picks.iter().filter(|p| p.as_str() == n).count();
    assert_eq!(count("C"), 0, "a zero weight is never picked");
    assert!(count("A") > count("B"), "A {} vs B {}", count("A"), count("B"));
    assert!(count("B") > 0);
    assert_eq!(count("A") + count("B"), 400);
}

#[test]
fn weighted_selection_with_no_positive_weight_cannot_enter() {
    let def = weighted_def(1, [0.0, -2.0, 0.0]);
    let mut rig = Rig::new(&def);
    assert_eq!(rig.tick(), StateTreeRunStatus::Failed);
}

#[test]
fn weighted_selection_ignores_unenterable_children() {
    let mut def = weighted_def(5, [1.0, 1.0, 1.0]);
    def.states[1].enter = key("never");
    def.states[2].enter = key("never");
    let picks = weighted_sequence(&def, 20);
    assert!(picks.iter().all(|p| p == "C"), "{picks:?}");
}

// ---- the ECS system ---------------------------------------------------------------------

#[test]
fn the_system_delivers_the_inbox_and_writes_a_reflected_status() {
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(FixedTime::default());
    let tree = StateTree::compile(&event_def(), &registry()).expect("compiles");
    let e = world
        .spawn((Blackboard::default(), tree, StateTreeInbox::default()))
        .id();
    tick_state_trees(&mut world);
    let status = world.get::<StateTreeStatus>(e).expect("status written");
    assert_eq!(status.status, StateTreeRunStatus::Running);
    assert_eq!(status.active_path, vec!["Root", "A"]);
    assert_eq!(status.ticks, 1);
    assert!(status.trace.contains(&"tick 1: enter A".to_string()));

    world
        .get_mut::<StateTreeInbox>(e)
        .expect("inbox")
        .events
        .push("go".to_string());
    tick_state_trees(&mut world);
    let status = world.get::<StateTreeStatus>(e).expect("status");
    assert_eq!(status.active_path, vec!["Root", "B"]);
    assert!(world.get::<StateTreeInbox>(e).expect("inbox").events.is_empty());
}

/// Spec 38 § 5: a paused tree advances nothing and its queued events keep waiting. The system
/// used to drain the inbox into a paused tree, so a burst of more than `MAX_PENDING_EVENTS`
/// events sent during a pause wrote `dropped` trace lines and lost events. The waiting inbox keeps
/// the newest `MAX_PENDING_EVENTS` (ADR-5905), so the oldest of this burst are counted in
/// `dropped` and nothing reaches the trace.
#[test]
fn the_system_leaves_the_inbox_waiting_while_a_tree_is_paused() {
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(FixedTime::default());
    let tree = StateTree::compile(&event_def(), &registry()).expect("compiles");
    let e = world
        .spawn((Blackboard::default(), tree, StateTreeInbox::default()))
        .id();
    tick_state_trees(&mut world);
    let before = world.get::<StateTreeStatus>(e).expect("status").clone();
    assert_eq!(before.active_path, vec!["Root", "A"]);

    world.get_mut::<StateTree>(e).expect("tree").pause();
    let burst: Vec<String> = (0..MAX_PENDING_EVENTS + 6)
        .map(|i| format!("noise{i}"))
        .chain(std::iter::once("go".to_string()))
        .collect();
    world.get_mut::<StateTreeInbox>(e).expect("inbox").events = burst.clone();
    for _ in 0..3 {
        tick_state_trees(&mut world);
    }
    let paused = world.get::<StateTreeStatus>(e).expect("status").clone();
    assert!(paused.paused);
    assert_eq!(paused.ticks, before.ticks, "no tick counted");
    assert_eq!(paused.trace, before.trace, "no trace line, no dropped event");
    assert_eq!(paused.active_path, before.active_path);
    let inbox = world.get::<StateTreeInbox>(e).expect("inbox");
    assert_eq!(
        inbox.events,
        burst[burst.len() - MAX_PENDING_EVENTS..].to_vec(),
        "the newest events wait in the inbox, in order"
    );
    assert_eq!(inbox.dropped, (burst.len() - MAX_PENDING_EVENTS) as u64);

    // Resumed: the inbox is delivered; it fits the tree's queue, so no event drops there and the
    // last one, `go`, takes its transition.
    world.get_mut::<StateTree>(e).expect("tree").resume();
    tick_state_trees(&mut world);
    assert!(world.get::<StateTreeInbox>(e).expect("inbox").events.is_empty());
    let resumed = world.get::<StateTreeStatus>(e).expect("status");
    assert!(!resumed.paused);
    assert_eq!(resumed.ticks, before.ticks + 1);
    assert_eq!(resumed.active_path, vec!["Root", "B"]);
    assert!(
        !world
            .get::<StateTree>(e)
            .expect("tree")
            .trace()
            .any(|t| matches!(t.kind, TraceKind::EventDropped(_)))
    );
}

/// Review finding (fixrev4): a host that sends an event every step to a tree paused for a long
/// cutscene must not grow the inbox without bound. The inbox keeps the newest
/// `MAX_PENDING_EVENTS` names in order and counts the rest in `dropped`, so the resume step
/// delivers what fits the tree's queue and writes no `dropped` trace line.
#[test]
fn a_paused_trees_inbox_keeps_only_the_newest_events_up_to_the_cap() {
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(FixedTime::default());
    let tree = StateTree::compile(&event_def(), &registry()).expect("compiles");
    let e = world
        .spawn((Blackboard::default(), tree, StateTreeInbox::default()))
        .id();
    tick_state_trees(&mut world);
    let before = world.get::<StateTreeStatus>(e).expect("status").clone();
    world.get_mut::<StateTree>(e).expect("tree").pause();

    // One event per fixed step for 600 steps (ten seconds at 60 Hz), then the one that matters.
    const SENT: usize = 600;
    for i in 0..SENT {
        let name = if i + 1 == SENT {
            "go".to_string()
        } else {
            format!("heard{i}")
        };
        world
            .get_mut::<StateTreeInbox>(e)
            .expect("inbox")
            .events
            .push(name);
        tick_state_trees(&mut world);
        let inbox = world.get::<StateTreeInbox>(e).expect("inbox");
        assert!(
            inbox.events.len() <= MAX_PENDING_EVENTS,
            "step {i}: the paused inbox holds {} events (cap {MAX_PENDING_EVENTS})",
            inbox.events.len()
        );
    }
    let inbox = world.get::<StateTreeInbox>(e).expect("inbox").clone();
    let newest: Vec<String> = (SENT - MAX_PENDING_EVENTS..SENT - 1)
        .map(|i| format!("heard{i}"))
        .chain(std::iter::once("go".to_string()))
        .collect();
    assert_eq!(inbox.events, newest, "the newest events wait, in order");
    assert_eq!(inbox.dropped, (SENT - MAX_PENDING_EVENTS) as u64);
    let paused = world.get::<StateTreeStatus>(e).expect("status").clone();
    assert_eq!(paused.trace, before.trace, "a pause writes no trace line");

    world.get_mut::<StateTree>(e).expect("tree").resume();
    tick_state_trees(&mut world);
    assert!(world.get::<StateTreeInbox>(e).expect("inbox").events.is_empty());
    let tree = world.get::<StateTree>(e).expect("tree");
    assert!(
        !tree.trace().any(|t| matches!(t.kind, TraceKind::EventDropped(_))),
        "everything the inbox kept fits the tree's queue"
    );
    // `go` was the newest event, so it is delivered (the `heard*` names match no transition).
    let resumed = world.get::<StateTreeStatus>(e).expect("status");
    assert_eq!(resumed.active_path, vec!["Root", "B"]);
}

/// The same cap holds for an agent a layer has parked: the tick system skips it, so nothing else
/// would drain its inbox.
#[test]
fn a_parked_agents_inbox_is_capped_too() {
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(FixedTime::default());
    let tree = StateTree::compile(&event_def(), &registry()).expect("compiles");
    let e = world
        .spawn((
            Blackboard::default(),
            tree,
            StateTreeInbox::default(),
            LayerParked {
                restore: regia_scene::prelude::Visibility::Inherited,
                inherited: false,
            },
        ))
        .id();
    let burst: Vec<String> = (0..MAX_PENDING_EVENTS * 3).map(|i| format!("e{i}")).collect();
    world.get_mut::<StateTreeInbox>(e).expect("inbox").events = burst.clone();
    tick_state_trees(&mut world);
    let inbox = world.get::<StateTreeInbox>(e).expect("inbox");
    assert_eq!(inbox.events, burst[MAX_PENDING_EVENTS * 2..].to_vec());
    assert_eq!(inbox.dropped, (MAX_PENDING_EVENTS * 2) as u64);
    assert!(
        world.get::<StateTreeStatus>(e).is_none(),
        "a parked tree does not tick"
    );
}

/// An inbox under the cap is left alone while paused: no event is dropped and the component is
/// not written (so change detection stays quiet on a steady paused agent).
#[test]
fn a_paused_inbox_under_the_cap_is_not_touched() {
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(FixedTime::default());
    let tree = StateTree::compile(&event_def(), &registry()).expect("compiles");
    let e = world
        .spawn((Blackboard::default(), tree, StateTreeInbox::default()))
        .id();
    tick_state_trees(&mut world);
    world.get_mut::<StateTree>(e).expect("tree").pause();
    let events: Vec<String> = (0..MAX_PENDING_EVENTS).map(|i| format!("e{i}")).collect();
    world.get_mut::<StateTreeInbox>(e).expect("inbox").events = events.clone();
    world.clear_trackers();
    let tick_before = world.change_tick();
    tick_state_trees(&mut world);
    let inbox = world.entity(e).get_ref::<StateTreeInbox>().expect("inbox");
    assert_eq!(inbox.events, events);
    assert_eq!(inbox.dropped, 0);
    assert!(
        !inbox
            .last_changed()
            .is_newer_than(tick_before, world.change_tick()),
        "a paused inbox under the cap is not written"
    );
}

#[test]
fn the_system_uses_the_fixed_step_for_task_dt() {
    #[derive(Default)]
    struct Dt;
    impl BtTask for Dt {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let dt = ctx.dt;
            if let Some(mut bb) = ctx.blackboard_mut() {
                bb.insert("dt", dt);
            }
            BtStatus::Running
        }
    }
    register_task!(Dt, "t::Dt");
    let mut reg = registry();
    reg.register::<Dt>();
    let a = with_tasks(st("A", &[]), &["t::Dt"]);
    let def = def_of("Root", vec![st("Root", &["A"]), a]);
    let mut world = World::new();
    world.insert_resource(Time::default());
    let mut fixed = FixedTime::default();
    fixed.hz = 50.0;
    world.insert_resource(fixed);
    let e = world
        .spawn((
            Blackboard::default(),
            StateTree::compile(&def, &reg).expect("compiles"),
        ))
        .id();
    tick_state_trees(&mut world);
    let dt = world.get::<Blackboard>(e).expect("bb").get_float("dt");
    assert!((dt - 0.02).abs() < 1e-6, "dt was {dt}");
}

#[test]
fn a_task_that_despawns_its_agent_does_not_abort_the_system() {
    #[derive(Default)]
    struct Die;
    impl BtTask for Die {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let e = ctx.entity;
            ctx.world.despawn(e);
            BtStatus::Success
        }
    }
    register_task!(Die, "t::Die");
    let mut reg = registry();
    reg.register::<Die>();
    let def = def_of(
        "Root",
        vec![st("Root", &["A"]), with_tasks(st("A", &[]), &["t::Die"])],
    );
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(FixedTime::default());
    let a = world
        .spawn(StateTree::compile(&def, &reg).expect("compiles"))
        .id();
    let b = world
        .spawn(StateTree::compile(&def, &reg).expect("compiles"))
        .id();
    tick_state_trees(&mut world);
    assert!(world.get_entity(a).is_err());
    assert!(world.get_entity(b).is_err());
}

#[test]
fn agents_tick_in_entity_order_whatever_their_archetype() {
    // `a` gets one more component, so the query walks `b` and `c` first; the tick order must
    // still be spawn order (ADR-1660).
    #[derive(Resource, Default)]
    struct TickLog(Vec<Entity>);
    #[derive(Default)]
    struct Log;
    impl BtTask for Log {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let e = ctx.entity;
            ctx.world.resource_mut::<TickLog>().0.push(e);
            BtStatus::Running
        }
    }
    register_task!(Log, "t::Log");
    let mut reg = registry();
    reg.register::<Log>();
    let def = def_of(
        "Root",
        vec![st("Root", &["A"]), with_tasks(st("A", &[]), &["t::Log"])],
    );
    let mut world = World::new();
    world.insert_resource(FixedTime::default());
    world.init_resource::<TickLog>();
    let spawn = |world: &mut World| {
        world
            .spawn(StateTree::compile(&def, &reg).expect("compiles"))
            .id()
    };
    let (a, b, c) = (spawn(&mut world), spawn(&mut world), spawn(&mut world));
    world.entity_mut(a).insert(Blackboard::default());
    let mut query = world.query::<(Entity, &StateTree)>();
    let walked: Vec<Entity> = query.iter(&world).map(|(e, _)| e).collect();
    assert_ne!(walked, [a, b, c], "the archetypes do reorder the query");
    tick_state_trees(&mut world);
    tick_state_trees(&mut world);
    let log = &world.resource::<TickLog>().0;
    assert!(!log.is_empty() && log.len().is_multiple_of(3), "{log:?}");
    assert!(log.chunks(3).all(|tick| tick == [a, b, c]), "{log:?}");
}

#[test]
fn ai_plugin_registers_and_ticks_state_trees() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, crate::AiPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    let tree = StateTree::compile(&event_def(), &registry()).expect("compiles");
    let e = app.world_mut().spawn((Blackboard::default(), tree)).id();
    for _ in 0..3 {
        app.update();
    }
    let status = app
        .world()
        .get::<StateTreeStatus>(e)
        .expect("a reflected status was written");
    assert_eq!(status.status, StateTreeRunStatus::Running);
    assert_eq!(status.active_path.first().map(String::as_str), Some("Root"));
}

// ---- v2 (spec 38, ADR-1639) -------------------------------------------------------------

fn v1_text() -> &'static str {
    r#"// a guard
(
    format_version: 1,
    root: "Root",
    states: [
        (name: "Root", children: ["Idle", "Alert"]),
        (name: "Idle", transitions: [(trigger: Event("heard"), target: State("Alert"))]),
        (name: "Alert"),
    ],
)"#
}

#[test]
fn v1_files_load_and_migrate_to_v2_keeping_every_other_byte() {
    let migrated = migrate_ron_v1_to_v2(v1_text()).expect("migrates");
    assert_eq!(
        migrated,
        v1_text().replace("format_version: 1", "format_version: 2")
    );
    // The migrated text and the v1 text mean the same thing.
    assert_eq!(
        StateTreeDef::from_ron(&migrated).expect("v2"),
        StateTreeDef::from_ron(v1_text()).expect("v1")
    );
    assert_eq!(StateTreeDef::from_ron(v1_text()).expect("v1").format_version, 2);
    // Already v2: unchanged. A newer file is refused.
    assert_eq!(migrate_ron_v1_to_v2(&migrated).expect("same"), migrated);
    assert_eq!(
        migrate_ron_v1_to_v2("(format_version: 3, root: \"R\", states: [])"),
        Err(StateTreeError::UnsupportedVersion {
            found: 3,
            supported: 2
        })
    );
}

#[test]
fn a_migrated_v1_tree_behaves_like_the_v1_tree() {
    let run = |text: &str| {
        let def = StateTreeDef::from_ron(text).expect("parses");
        let mut rig = Rig::new(&def);
        let mut log = Vec::new();
        rig.tick();
        log.push(rig.path());
        rig.tree.send_event("heard");
        rig.tick();
        log.push(rig.path());
        (log, rig.trace_text())
    };
    let migrated = migrate_ron_v1_to_v2(v1_text()).expect("migrates");
    assert_eq!(run(v1_text()), run(&migrated));
}

#[test]
fn the_version_number_is_found_only_at_the_top_level() {
    let text = "(description: \"format_version: 1\", /* format_version: 1 */ format_version: 1, root: \"R\", states: [(name: \"R\")])";
    let out = migrate_ron_v1_to_v2(text).expect("migrates");
    assert!(out.contains("description: \"format_version: 1\""));
    assert!(out.contains("/* format_version: 1 */"));
    assert!(out.contains("*/ format_version: 2,"));
}

#[test]
fn enter_set_lands_before_the_first_task_tick_and_stays_after_leaving() {
    let mut a = with_tasks(st("A", &[]), &["t::Peek"]);
    a.enter_set.insert("mood".into(), Literal::Int(7));
    a.enter_set.insert("calm".into(), Literal::Bool(true));
    a.enter_set.insert("label".into(), Literal::String("hi".into()));
    a.enter_set.insert("gain".into(), Literal::Float(0.5));
    a = with_tr(a, tr(Trigger::Event("go".into()), "B", 0));
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.int("seen"), 7, "the task saw the literal on its first tick");
    {
        let bb = rig.world.get::<Blackboard>(rig.entity).expect("blackboard");
        assert!(bb.get_bool("calm"));
        assert_eq!(bb.get("label"), Some(&BbValue::String("hi".into())));
        assert_eq!(bb.get("gain"), Some(&BbValue::Float(0.5)));
    }
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    assert_eq!(rig.int("mood"), 7, "leaving does not remove it");
}

#[test]
fn enter_set_is_checked_at_compile_time() {
    let mut a = st("R", &[]);
    a.enter_set.insert(String::new(), Literal::Int(1));
    assert_eq!(
        compile_err(&def_of("R", vec![a])),
        StateTreeError::BadValue("R".into())
    );
    let mut b = st("R", &[]);
    b.enter_set.insert("k".into(), Literal::Float(f32::NAN));
    assert_eq!(
        compile_err(&def_of("R", vec![b])),
        StateTreeError::BadValue("R".into())
    );
}

fn hold_def() -> StateTreeDef {
    let go = TransitionDef {
        condition: key("go"),
        hold: 0.25,
        ..tr(Trigger::Tick, "B", 0)
    };
    let a = with_tr(
        with_tr(st("A", &[]), go),
        tr(Trigger::Event("reset".into()), "A", 0),
    );
    def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])])
}

/// The step (1-based) on which the tree reaches `Root/B`, driving `script(step, rig)` before each tick.
fn step_of_b(script: impl Fn(u32, &mut Rig)) -> u32 {
    let mut rig = Rig::new(&hold_def()).with_dt(1.0 / 60.0);
    for step in 1..=60 {
        script(step, &mut rig);
        rig.tick();
        if rig.path() == "Root/B" {
            return step;
        }
    }
    0
}

#[test]
fn a_held_transition_fires_on_exactly_the_step_its_counter_is_reached() {
    assert_eq!(
        step_of_b(|step, rig| if step == 1 {
            rig.set("go", true)
        }),
        15
    );
}

#[test]
fn a_held_transition_rejected_on_its_due_step_fires_once_the_target_can_be_entered() {
    let go = TransitionDef {
        condition: key("go"),
        hold: 0.25,
        ..tr(Trigger::Tick, "B", 0)
    };
    let mut b = st("B", &[]);
    b.enter = key("b_ok");
    let def = def_of(
        "Root",
        vec![st("Root", &["A", "B"]), with_tr(st("A", &[]), go), b],
    );
    let mut rig = Rig::new(&def).with_dt(1.0 / 60.0);
    let mut fired = 0;
    for step in 1..=60 {
        match step {
            1 => rig.set("go", true),
            // The hold completes on step 15 while B cannot be entered; B opens on step 20.
            20 => rig.set("b_ok", true),
            _ => {}
        }
        rig.tick();
        if rig.path() == "Root/B" {
            fired = step;
            break;
        }
    }
    assert_eq!(fired, 20);
}

/// ADR-1809 (spec 38 § 4): a held transition counts as matching from the tick its counter is
/// reached for as long as trigger and condition keep holding, so one *outranked* on its due tick
/// (here by a higher-priority `Stay`, which takes the tick pool) fires on the next tick.
#[test]
fn a_held_transition_outranked_on_its_due_step_fires_on_the_next() {
    let go = TransitionDef {
        condition: key("go"),
        hold: 0.25,
        ..tr(Trigger::Tick, "B", 0)
    };
    let block = TransitionDef {
        condition: key("block"),
        priority: 5,
        ..to_target(Trigger::Tick, Target::Stay)
    };
    let a = with_tr(with_tr(st("A", &[]), go), block);
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def).with_dt(1.0 / 60.0);
    let mut fired = 0;
    for step in 1..=60 {
        match step {
            1 => rig.set("go", true),
            // The hold completes on step 15, where the higher-priority Stay wins the pool.
            15 => rig.set("block", true),
            16 => rig.set("block", false),
            _ => {}
        }
        rig.tick();
        if rig.path() == "Root/B" {
            fired = step;
            break;
        }
    }
    assert_eq!(fired, 16);
}

/// Spec 38 § 4: `hold` applies to completion triggers too. A state whose task succeeds on its
/// first tick leaves through a `Succeeded` transition held 0.25 s exactly on step 15 (the root
/// runs its own task, so the leaf's completion does not finish the tree first).
#[test]
fn a_hold_on_a_succeeded_trigger_counts_from_the_completion() {
    let after = TransitionDef {
        hold: 0.25,
        ..tr(Trigger::Succeeded, "B", 0)
    };
    let a = with_tr(with_tasks(st("A", &[]), &["t::Win"]), after);
    let root = with_tasks(st("Root", &["A", "B"]), &["t::Run"]);
    let def = def_of("Root", vec![root, a, st("B", &[])]);
    let mut rig = Rig::new(&def).with_dt(1.0 / 60.0);
    let mut fired = 0;
    for step in 1..=60 {
        rig.tick();
        if rig.path() == "Root/B" {
            fired = step;
            break;
        }
    }
    assert_eq!(fired, 15);
    assert_eq!(rig.int("win_ticks"), 1, "a finished task is not ticked again");
    assert_eq!(rig.tree.status(), StateTreeRunStatus::Running);
}

/// The book's note on held completion transitions: a root without tasks takes its child's
/// completion, so the tree finishes on the tick the leaf completes, before a hold can end.
#[test]
fn a_root_without_tasks_finishes_before_a_held_completion_transition() {
    let after = TransitionDef {
        hold: 0.25,
        ..tr(Trigger::Succeeded, "B", 0)
    };
    let a = with_tr(with_tasks(st("A", &[]), &["t::Win"]), after);
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def).with_dt(1.0 / 60.0);
    assert_eq!(rig.tick(), StateTreeRunStatus::Succeeded);
}

#[test]
fn dropping_the_condition_resets_the_hold_counter() {
    let fired = step_of_b(|step, rig| match step {
        1 => rig.set("go", true),
        10 => rig.set("go", false),
        11 => rig.set("go", true),
        _ => {}
    });
    assert_eq!(fired, 25, "counter restarts at step 11 and reaches 15 on step 25");
}

#[test]
fn leaving_and_re_entering_the_state_resets_the_hold_counter() {
    let fired = step_of_b(|step, rig| match step {
        1 => rig.set("go", true),
        10 => {
            rig.tree.send_event("reset");
        }
        _ => {}
    });
    assert_eq!(
        fired, 24,
        "re-entered on step 10, counter is 1 on step 10 and 15 on step 24"
    );
}

#[test]
fn the_status_lists_running_hold_counters() {
    let mut rig = Rig::new(&hold_def()).with_dt(1.0 / 60.0);
    rig.set("go", true);
    assert!(rig.tree.held().is_empty());
    rig.tick();
    assert_eq!(rig.tree.held(), vec!["A -> B".to_string()]);
    assert_eq!(StateTreeStatus::of(&rig.tree).held, vec!["A -> B".to_string()]);
    rig.set("go", false);
    rig.tick();
    assert!(rig.tree.held().is_empty());
}

#[test]
fn hold_is_rejected_on_events_and_bad_numbers() {
    let on_event = TransitionDef {
        hold: 1.0,
        ..tr(Trigger::Event("e".into()), "R", 0)
    };
    let s = with_tr(st("R", &[]), on_event);
    assert_eq!(
        compile_err(&def_of("R", vec![s])),
        StateTreeError::HoldOnEvent("R".into())
    );
    for bad in [-1.0, f32::NAN, f32::INFINITY] {
        let t = TransitionDef {
            hold: bad,
            ..tr(Trigger::Tick, "R", 0)
        };
        let s = with_tr(st("R", &[]), t);
        assert_eq!(
            compile_err(&def_of("R", vec![s])),
            StateTreeError::BadValue("R".into())
        );
    }
}

fn to_target(trigger: Trigger, target: Target) -> TransitionDef {
    TransitionDef {
        target,
        ..tr(trigger, "unused", 0)
    }
}

#[test]
fn parent_re_enters_the_parent_and_descends_by_its_selection() {
    let mut m1 = st("M1", &[]);
    m1.enter = Condition::Not(Box::new(key("skip")));
    m1 = with_tr(m1, tr(Trigger::Event("go2".into()), "M2", 0));
    let m2 = with_tr(
        st("M2", &[]),
        to_target(Trigger::Event("up".into()), Target::Parent),
    );
    let def = def_of(
        "Root",
        vec![
            st("Root", &["Menu", "Play"]),
            st("Menu", &["M1", "M2"]),
            m1,
            m2,
            st("Play", &[]),
        ],
    );
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(rig.path(), "Root/Menu/M1");
    rig.tree.send_event("go2");
    rig.tick();
    assert_eq!(rig.path(), "Root/Menu/M2");
    rig.set("skip", true);
    rig.tree.send_event("up");
    rig.tick();
    assert_eq!(
        rig.path(),
        "Root/Menu/M2",
        "Menu was re-entered and M1 is blocked"
    );
    let trace = rig.trace_text();
    assert!(
        trace
            .iter()
            .any(|l| l.contains("transition M2 -> Menu on event:up"))
    );
    assert!(trace.iter().filter(|l| l.contains("enter Menu")).count() == 2);
}

#[test]
fn parent_of_the_root_is_a_compile_error() {
    let root = with_tr(st("Root", &[]), to_target(Trigger::Tick, Target::Parent));
    let err = compile_err(&def_of("Root", vec![root]));
    assert_eq!(err, StateTreeError::ParentOfRoot("Root".into()));
    assert_eq!(err.reason(), "parent_of_root");
}

#[test]
fn stay_swallows_an_event_so_a_lower_priority_transition_does_not_take_it() {
    let swallow = TransitionDef {
        priority: 5,
        ..to_target(Trigger::Event("n".into()), Target::Stay)
    };
    let s = with_tr(st("S", &[]), swallow);
    let root = with_tr(
        st("Root", &["S", "Other"]),
        tr(Trigger::Event("n".into()), "Other", 0),
    );
    let def = def_of("Root", vec![root.clone(), s, st("Other", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    rig.tree.send_event("n");
    rig.tick();
    assert_eq!(rig.path(), "Root/S");
    // Without the Stay, the ancestor's transition fires.
    let def = def_of("Root", vec![root, st("S", &[]), st("Other", &[])]);
    let mut rig = Rig::new(&def);
    rig.tick();
    rig.tree.send_event("n");
    rig.tick();
    assert_eq!(rig.path(), "Root/Other");
}

#[test]
fn next_enterable_skips_a_sibling_whose_enter_fails_and_next_does_not() {
    let build = |target: Target| {
        let a = with_tr(st("A", &[]), to_target(Trigger::Tick, target));
        let mut b = st("B", &[]);
        b.enter = key("b_ok");
        def_of("Root", vec![st("Root", &["A", "B", "C"]), a, b, st("C", &[])])
    };
    let mut rig = Rig::new(&build(Target::NextEnterable));
    rig.tick();
    assert_eq!(rig.path(), "Root/C");
    let mut rig = Rig::new(&build(Target::Next));
    rig.tick();
    assert_eq!(rig.path(), "Root/A", "Next targets B, which cannot be entered");
    assert!(rig.trace_text().iter().any(|l| l.contains("rejected A -> B")));
}

#[test]
fn next_enterable_needs_a_next_sibling() {
    let a = with_tr(st("A", &[]), to_target(Trigger::Tick, Target::NextEnterable));
    let err = compile_err(&def_of("Root", vec![st("Root", &["A"]), a]));
    assert_eq!(err, StateTreeError::NoNextSibling("A".into()));
}

#[test]
fn fallback_next_enterable_tries_the_following_siblings_when_nothing_is_below() {
    let build = |fallback: Fallback| {
        let w = with_tr(st("W", &[]), tr(Trigger::Event("go".into()), "X", 0));
        let mut x = st("X", &["Xc"]);
        x.fallback = fallback;
        let mut xc = st("Xc", &[]);
        xc.enter = key("never");
        def_of("Root", vec![st("Root", &["W", "X", "Y"]), w, x, xc, st("Y", &[])])
    };
    let mut rig = Rig::new(&build(Fallback::NextEnterable));
    rig.tick();
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(rig.path(), "Root/Y");
    let mut rig = Rig::new(&build(Fallback::Stay));
    rig.tick();
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(rig.path(), "Root/W");
    assert!(rig.trace_text().iter().any(|l| l.contains("rejected W -> X")));
}

#[test]
fn ambient_tasks_run_before_the_first_selection_and_every_tick() {
    let mut a = st("A", &[]);
    a.enter = key("ready");
    let mut def = def_of("Root", vec![st("Root", &["A"]), a]);
    def.ambient = vec!["t::SetReady".into(), "t::Run".into()];
    let mut rig = Rig::new(&def);
    rig.tick();
    assert_eq!(
        rig.path(),
        "Root/A",
        "the ambient task's write was visible to `enter`"
    );
    rig.tick();
    rig.tick();
    assert_eq!(rig.int("run_ticks"), 3);
    assert_eq!(rig.int("resets"), 0);
    rig.restart();
    assert_eq!(rig.int("resets"), 1, "ambient tasks reset on restart");
    rig.tick();
    assert_eq!(rig.int("run_ticks"), 4);
}

/// Spec 38 § 4/§ 6: ambient tasks tick in list order (a later one sees an earlier one's write
/// on the same tick) and before events are delivered (an event guard on the first tick sees the
/// ambient write).
#[test]
fn ambient_tasks_tick_in_list_order_and_before_events() {
    /// Copies the boolean `ready` into the int `ready_seen` (1 or 0).
    #[derive(Default)]
    struct CopyReady;
    impl BtTask for CopyReady {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            if let Some(mut bb) = ctx.blackboard_mut() {
                let seen = i64::from(bb.get_bool("ready"));
                bb.insert("ready_seen".to_string(), seen);
            }
            BtStatus::Success
        }
    }
    register_task!(CopyReady, "t::CopyReady");
    let mut reg = registry();
    reg.register::<CopyReady>();
    let seen_on_first_tick = |ambient: &[&str]| {
        let mut def = def_of("R", vec![st("R", &[])]);
        def.ambient = ambient.iter().map(|s| (*s).to_string()).collect();
        let tree = StateTree::compile(&def, &reg).expect("compiles");
        let mut rig = Rig {
            tree,
            ..Rig::new(&def_of("R", vec![st("R", &[])]))
        };
        rig.tick();
        rig.int("ready_seen")
    };
    assert_eq!(seen_on_first_tick(&["t::SetReady", "t::CopyReady"]), 1);
    assert_eq!(seen_on_first_tick(&["t::CopyReady", "t::SetReady"]), 0);

    // An event queued before the first tick is delivered after the ambient tasks ran.
    let go = TransitionDef {
        condition: key("ready"),
        ..tr(Trigger::Event("go".into()), "B", 0)
    };
    let mut def = def_of(
        "Root",
        vec![st("Root", &["A", "B"]), with_tr(st("A", &[]), go), st("B", &[])],
    );
    def.ambient = vec!["t::SetReady".into()];
    let mut rig = Rig::new(&def);
    rig.tree.send_event("go");
    rig.tick();
    assert_eq!(rig.path(), "Root/B", "the event guard saw the ambient write");
}

#[test]
fn an_unregistered_ambient_task_is_an_unknown_task() {
    let mut def = def_of("R", vec![st("R", &[])]);
    def.ambient = vec!["nope::Missing".into()];
    assert!(matches!(compile_err(&def), StateTreeError::UnknownTask { .. }));
}

#[test]
fn ambient_tasks_reset_when_the_tree_finishes() {
    let root = with_tr(st("Root", &[]), to_target(Trigger::Tick, Target::Succeed));
    let mut def = def_of("Root", vec![root]);
    def.ambient = vec!["t::Run".into()];
    let mut rig = Rig::new(&def);
    assert_eq!(rig.tick(), StateTreeRunStatus::Succeeded);
    assert_eq!(rig.int("resets"), 1);
    rig.tick();
    assert_eq!(
        rig.int("run_ticks"),
        1,
        "a finished tree does not tick ambient tasks"
    );
}

#[test]
fn a_finished_tree_does_not_count_ticks() {
    let root = with_tr(st("Root", &[]), to_target(Trigger::Tick, Target::Succeed));
    let mut rig = Rig::new(&def_of("Root", vec![root]));
    assert_eq!(rig.tick(), StateTreeRunStatus::Succeeded);
    let finished_at = rig.tree.ticks();
    for _ in 0..5 {
        assert_eq!(rig.tick(), StateTreeRunStatus::Succeeded);
    }
    assert_eq!(rig.tree.ticks(), finished_at, "ticks freeze once finished");
}

fn loader<'a>(files: &'a [(&'a str, String)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |path| files.iter().find(|(p, _)| *p == path).map(|(_, t)| t.clone())
}

#[test]
fn includes_inline_prefixed_states_and_their_targets_resolve() {
    let combat = r#"(
        format_version: 1,
        root: "Top",
        seed: 99,
        state_key: "ignored",
        ambient: ["nope::Ignored"],
        states: [
            (name: "Top", children: ["A", "B"]),
            (name: "A", transitions: [(trigger: Event("x"), target: State("B"))]),
            (name: "B"),
        ],
    )"#;
    let files = [("ai/combat.statetree.ron", combat.to_string())];
    let mut def = def_of("Root", vec![st("Root", &["Combat/Top"])]);
    def.includes = vec![IncludeDef {
        prefix: "Combat".into(),
        path: "ai/combat.statetree.ron".into(),
    }];
    let load = loader(&files);
    let tree = StateTree::compile_with(&def, &registry(), &load).expect("compiles");
    assert!(tree.has_state("Combat/Top") && tree.has_state("Combat/A") && tree.has_state("Combat/B"));
    assert_eq!(
        tree.def().seed,
        0,
        "the included seed and ambient list are ignored"
    );
    assert!(tree.def().includes.is_empty());
    let text = tree.def().to_ron().expect("writes");
    assert_eq!(&StateTreeDef::from_ron(&text).expect("reads"), tree.def());

    let mut rig = Rig {
        tree,
        ..Rig::new(&def_of("R", vec![st("R", &[])]))
    };
    rig.tick();
    assert_eq!(rig.path(), "Root/Combat/Top/Combat/A");
    rig.tree.send_event("x");
    rig.tick();
    assert_eq!(rig.path(), "Root/Combat/Top/Combat/B");
}

/// Spec 38 § 4: an included tree's `seed`, `state_key` and `ambient` are ignored (its ambient
/// list names an unregistered task, which would be `unknown_task` if it were kept).
#[test]
fn an_included_trees_state_key_and_ambient_are_ignored() {
    let sub = r#"(
        format_version: 2,
        root: "S",
        seed: 7,
        state_key: "sub_mode",
        ambient: ["nope::Ignored"],
        states: [(name: "S")],
    )"#;
    let files = [("sub.ron", sub.to_string())];
    let mut def = def_of("Root", vec![st("Root", &["Sub/S"])]);
    def.state_key = "mode".into();
    def.includes = vec![IncludeDef {
        prefix: "Sub".into(),
        path: "sub.ron".into(),
    }];
    let tree = StateTree::compile_with(&def, &registry(), &loader(&files)).expect("compiles");
    assert_eq!(tree.def().state_key, "mode");
    assert!(tree.def().ambient.is_empty());
    assert_eq!(tree.def().seed, def.seed);
    let mut rig = Rig {
        tree,
        ..Rig::new(&def_of("R", vec![st("R", &[])]))
    };
    rig.tick();
    let bb = rig.world.get::<Blackboard>(rig.entity).expect("bb");
    assert!(
        bb.get("sub_mode").is_none(),
        "the included state_key is not written"
    );
    assert!(bb.get("mode").is_some(), "the host's state_key is");
}

#[test]
fn plain_compile_does_not_resolve_includes() {
    let mut def = def_of("R", vec![st("R", &[])]);
    def.includes = vec![IncludeDef {
        prefix: "p".into(),
        path: "x.ron".into(),
    }];
    let err = compile_err(&def);
    assert_eq!(err, StateTreeError::IncludeMissing("x.ron".into()));
    assert_eq!(err.reason(), "include_missing");
}

fn include_file(next: Option<&str>) -> String {
    let includes = next.map_or(String::new(), |n| {
        format!("includes: [(prefix: \"p\", path: \"{n}\")],")
    });
    format!("(format_version: 2, root: \"R\", {includes} states: [(name: \"R\")])")
}

fn include_main(first: &str) -> StateTreeDef {
    let mut def = def_of("R", vec![st("R", &[])]);
    def.includes = vec![IncludeDef {
        prefix: "p".into(),
        path: first.into(),
    }];
    def
}

#[test]
fn an_include_cycle_is_an_error() {
    let files = [
        ("a.ron", include_file(Some("b.ron"))),
        ("b.ron", include_file(Some("a.ron"))),
    ];
    let err = include_main("a.ron")
        .expand_includes(&loader(&files))
        .expect_err("cycle");
    assert!(matches!(err, StateTreeError::IncludeCycle(_)));
    assert_eq!(err.reason(), "include_cycle");
    // A file that includes itself.
    let files = [("a.ron", include_file(Some("a.ron")))];
    assert!(matches!(
        include_main("a.ron").expand_includes(&loader(&files)),
        Err(StateTreeError::IncludeCycle(_))
    ));
}

#[test]
fn includes_nest_four_deep_but_not_five() {
    let chain = |depth: usize| -> Vec<(String, String)> {
        (1..=depth)
            .map(|n| {
                let next = (n < depth).then(|| format!("f{}.ron", n + 1));
                (format!("f{n}.ron"), include_file(next.as_deref()))
            })
            .collect()
    };
    let run = |depth: usize| {
        let owned = chain(depth);
        let load = |p: &str| owned.iter().find(|(name, _)| name == p).map(|(_, t)| t.clone());
        include_main("f1.ron").expand_includes(&load)
    };
    let four = run(4).expect("four deep");
    assert_eq!(four.states.len(), 5);
    let err = run(5).expect_err("five deep");
    assert_eq!(err, StateTreeError::IncludeDepth);
    assert_eq!(err.reason(), "include_depth");
}

#[test]
fn an_include_with_a_newer_version_is_refused() {
    let files = [(
        "a.ron",
        "(format_version: 3, root: \"R\", states: [])".to_string(),
    )];
    assert!(matches!(
        include_main("a.ron").expand_includes(&loader(&files)),
        Err(StateTreeError::UnsupportedVersion { found: 3, .. })
    ));
}

fn reload_def(extra_transition: bool) -> StateTreeDef {
    let mut a = with_tasks(st("A", &[]), &["t::Run"]);
    if extra_transition {
        a = with_tr(a, tr(Trigger::Event("x".into()), "B", 0));
    }
    def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])])
}

fn compiled(def: &StateTreeDef) -> StateTree {
    StateTree::compile(def, &registry()).expect("compiles")
}

#[test]
fn reload_keeps_the_active_path_ticks_and_queue_when_every_state_survives() {
    let mut rig = Rig::new(&reload_def(false));
    for _ in 0..3 {
        rig.tick();
    }
    assert_eq!(rig.tree.ticks(), 3);
    rig.tree.send_event("x");
    let outcome = rig.reload(compiled(&reload_def(true)));
    assert_eq!(outcome, ReloadOutcome::Kept);
    assert_eq!(rig.path(), "Root/A");
    assert_eq!(rig.tree.ticks(), 3);
    rig.tick();
    assert_eq!(rig.tree.ticks(), 4);
    assert_eq!(
        rig.path(),
        "Root/B",
        "the queued event reached the new transition"
    );
}

#[test]
fn reload_restarts_when_an_active_state_is_gone() {
    let mut rig = Rig::new(&reload_def(false));
    rig.tick();
    let mut renamed = reload_def(false);
    renamed.states[1].name = "A2".into();
    renamed.states[0].children[0] = "A2".into();
    let outcome = rig.reload(compiled(&renamed));
    assert_eq!(outcome, ReloadOutcome::Restarted);
    assert_eq!(rig.tree.status(), StateTreeRunStatus::NotStarted);
    assert!(rig.tree.active_path().is_empty());
    rig.tick();
    assert_eq!(rig.path(), "Root/A2");
    assert!(rig.int("resets") >= 1, "the old task was reset");
}

/// ADR-1639 (6): the pause flag and the debug lists survive a reload, kept or restarted.
#[test]
fn a_reload_keeps_the_pause_flag_and_debug_lists() {
    for (next, expected) in [
        (reload_def(true), ReloadOutcome::Kept),
        (
            {
                let mut renamed = reload_def(false);
                renamed.states[1].name = "A2".into();
                renamed.states[0].children[0] = "A2".into();
                renamed
            },
            ReloadOutcome::Restarted,
        ),
    ] {
        let mut rig = Rig::new(&reload_def(false));
        rig.tick();
        rig.tree.set_debug(&["B".to_string()], &["B".to_string()]);
        rig.tree.pause();
        assert_eq!(rig.reload(compiled(&next)), expected);
        assert!(rig.tree.is_paused(), "{expected:?}: still paused");
        assert_eq!(rig.tree.debug_lists().0, &["B".to_string()], "{expected:?}");
        assert_eq!(rig.tree.debug_lists().1, &["B".to_string()], "{expected:?}");
        let ticks = rig.tree.ticks();
        rig.tick();
        assert_eq!(
            rig.tree.ticks(),
            ticks,
            "{expected:?}: a paused tree does not tick"
        );
    }
}

#[test]
fn reload_restarts_when_the_chain_changed_shape() {
    let mut rig = Rig::new(&reload_def(false));
    rig.tick();
    // A moves under a new parent.
    let moved = def_of(
        "Root",
        vec![
            st("Root", &["Group", "B"]),
            st("Group", &["A"]),
            with_tasks(st("A", &[]), &["t::Run"]),
            st("B", &[]),
        ],
    );
    assert_eq!(rig.reload(compiled(&moved)), ReloadOutcome::Restarted);
}

#[test]
fn a_paused_tree_advances_nothing_and_then_matches_an_unpaused_control() {
    let def = def_of(
        "Root",
        vec![
            st("Root", &["A", "B"]),
            with_tr(
                with_tasks(st("A", &[]), &["t::Run"]),
                tr(Trigger::Event("go".into()), "B", 0),
            ),
            with_tasks(st("B", &[]), &["t::Run"]),
        ],
    );
    let mut paused = Rig::new(&def);
    let mut control = Rig::new(&def);
    paused.tree.pause();
    assert!(paused.tree.is_paused());
    paused.tree.send_event("go");
    for _ in 0..100 {
        paused.tick();
    }
    assert_eq!(paused.tree.ticks(), 0);
    assert_eq!(paused.tree.status(), StateTreeRunStatus::NotStarted);
    assert!(paused.trace_text().is_empty());
    assert!(StateTreeStatus::of(&paused.tree).paused);
    paused.tree.resume();
    // The queued event waited; the control gets it on its first tick too.
    control.tree.send_event("go");
    for _ in 0..10 {
        paused.tick();
        control.tick();
    }
    assert_eq!(paused.trace_text(), control.trace_text());
    assert_eq!(paused.path(), control.path());
    assert_eq!(paused.int("run_ticks"), control.int("run_ticks"));
}

#[test]
fn disabled_states_are_skipped_and_pause_on_enter_pauses_after_the_tick() {
    let a = with_tr(st("A", &[]), tr(Trigger::Tick, "B", 0));
    let def = def_of("Root", vec![st("Root", &["A", "B"]), a, st("B", &[])]);
    let mut rig = Rig::new(&def);
    rig.tree.set_debug(&["A".to_string()], &[]);
    rig.tick();
    assert_eq!(rig.path(), "Root/B", "A was skipped by the start descent");

    let mut rig = Rig::new(&def);
    rig.tree.set_debug(&[], &["B".to_string()]);
    assert_eq!(rig.tree.debug_lists().1, &["B".to_string()]);
    rig.tick();
    assert_eq!(rig.path(), "Root/B");
    assert!(
        rig.tree.is_paused(),
        "paused at the end of the tick that entered B"
    );
    let ticks = rig.tree.ticks();
    rig.tick();
    assert_eq!(rig.tree.ticks(), ticks);
    assert!(rig.tree.has_state("B") && !rig.tree.has_state("Nope"));
}

#[test]
fn the_same_inputs_give_identical_status_and_trace_over_2000_ticks() {
    let go = |target: &str| tr(Trigger::Event("e".into()), target, 0);
    let mut root = st("Root", &["A", "B", "C"]);
    root.selection = Selection::Weighted;
    let held = TransitionDef {
        hold: 0.3,
        condition: key("hot"),
        ..tr(Trigger::Tick, "B", 0)
    };
    let def = StateTreeDef {
        seed: 12345,
        ambient: vec!["t::Run".into()],
        ..def_of(
            "Root",
            vec![
                root,
                with_tr(with_tr(st("A", &[]), go("Root")), held),
                with_tr(st("B", &[]), go("Root")),
                with_tr(st("C", &[]), go("Root")),
            ],
        )
    };
    let run = || {
        let mut rig = Rig::new(&def);
        let mut log = Vec::new();
        for n in 0..2000u32 {
            rig.set("hot", n % 7 < 4);
            if n % 3 == 0 {
                rig.tree.send_event("e");
            }
            rig.tick();
            log.push((rig.path(), StateTreeStatus::of(&rig.tree)));
        }
        log
    };
    assert_eq!(run(), run());
}

#[test]
fn compile_errors_have_snake_case_reasons() {
    let cases: Vec<(StateTreeError, &str)> = vec![
        (StateTreeError::Parse("x".into()), "parse"),
        (StateTreeError::Empty, "empty"),
        (StateTreeError::TooManyStates(5000), "too_many_states"),
        (StateTreeError::EmptyName, "empty_name"),
        (StateTreeError::DuplicateName("a".into()), "duplicate_name"),
        (StateTreeError::MultipleParents("a".into()), "multiple_parents"),
        (StateTreeError::Unreachable("a".into()), "unreachable"),
        (StateTreeError::NoNextSibling("a".into()), "no_next_sibling"),
        (StateTreeError::ConditionTooDeep("a".into()), "condition_too_deep"),
        (StateTreeError::BadValue("a".into()), "bad_value"),
        (StateTreeError::IncludeDepth, "include_depth"),
        (StateTreeError::HoldOnEvent("a".into()), "hold_on_event"),
        // Spec 38 § 4 lists 18 reasons; these six were proved only by other tests.
        (
            StateTreeError::UnsupportedVersion {
                found: 3,
                supported: STATE_TREE_FORMAT_VERSION,
            },
            "unsupported_version",
        ),
        (
            StateTreeError::UnknownState {
                name: "a".into(),
                from: "b".into(),
            },
            "unknown_state",
        ),
        (
            StateTreeError::UnknownTask {
                state: "a".into(),
                task: "t::Nope".into(),
            },
            "unknown_task",
        ),
        (StateTreeError::IncludeMissing("x.ron".into()), "include_missing"),
        (StateTreeError::IncludeCycle("x.ron".into()), "include_cycle"),
        (StateTreeError::ParentOfRoot("a".into()), "parent_of_root"),
        // The writer's error is not a compile reason of its own.
        (StateTreeError::Write("x".into()), "parse"),
    ];
    for (err, reason) in cases {
        assert_eq!(err.reason(), reason);
    }
    assert_eq!(StateTreeError::BadValue("a".into()).state(), Some("a"));
    assert_eq!(StateTreeError::Empty.state(), None);
}

#[test]
fn damaged_definition_text_never_panics_the_loader_or_the_compiler() {
    let sample = r#"(
        format_version: 2,
        root: "Root",
        ambient: ["t::Run"],
        includes: [(prefix: "p", path: "x.ron")],
        states: [
            (name: "Root", children: ["A", "B"], selection: Weighted),
            (name: "A", enter_set: {"k": Int(1)}, fallback: NextEnterable, transitions: [
                (trigger: Tick, condition: Compare(key: "hp", op: Lt, value: 3.0), hold: 0.5, target: Parent),
            ]),
            (name: "B", tasks: ["t::Win"]),
        ],
    )"#;
    let load = |_: &str| None;
    let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let exercise = |text: &str| {
        if let Ok(def) = StateTreeDef::from_ron(text) {
            let _ = StateTree::compile_with(&def, &registry(), &load);
        }
    };
    // Every prefix of the text.
    for end in 0..=sample.len() {
        if sample.is_char_boundary(end) {
            exercise(&sample[..end]);
        }
    }
    // Seeded single-byte damage (ASCII replacements keep the text valid UTF-8).
    const REPLACEMENTS: &[u8] = b" ()[]{}:,\"-0123456789xA";
    let bytes = sample.as_bytes();
    for _ in 0..500 {
        let mut damaged = bytes.to_vec();
        let at = (next() % damaged.len() as u64) as usize;
        damaged[at] = REPLACEMENTS[(next() % REPLACEMENTS.len() as u64) as usize];
        if let Ok(text) = String::from_utf8(damaged) {
            exercise(&text);
        }
    }
    // The undamaged text is only missing its include.
    let def = StateTreeDef::from_ron(sample).expect("parses");
    assert_eq!(
        StateTree::compile_with(&def, &registry(), &load).err(),
        Some(StateTreeError::IncludeMissing("x.ron".into()))
    );
}

// ---- event traces on the error paths (re-landed lost work, 2026-11-09) ----

fn ends_on(event: &str) -> StateTreeDef {
    let mut a = with_tasks(st("A", &[]), &["t::Run"]);
    a.transitions.push(TransitionDef {
        trigger: Trigger::Event(event.into()),
        condition: Condition::Always,
        priority: 0,
        target: Target::Succeed,
        hold: 0.0,
    });
    def_of("Root", vec![st("Root", &["A"]), a])
}

fn ignored(rig: &Rig, name: &str) -> usize {
    rig.tree
        .trace()
        .filter(|t| matches!(&t.kind, TraceKind::EventIgnored(e) if e == name))
        .count()
}

#[test]
fn events_sent_to_a_finished_tree_are_traced_as_ignored() {
    let mut rig = Rig::new(&ends_on("end"));
    rig.tick();
    rig.tree.send_event("end");
    assert_eq!(rig.tick(), StateTreeRunStatus::Succeeded);
    rig.tree.send_event("loot");
    rig.tick();
    assert_eq!(ignored(&rig, "loot"), 1);
}

#[test]
fn events_behind_the_one_that_finishes_the_tree_are_traced_as_ignored() {
    let mut rig = Rig::new(&ends_on("end"));
    rig.tick();
    rig.tree.send_event("end");
    rig.tree.send_event("late");
    assert_eq!(rig.tick(), StateTreeRunStatus::Succeeded);
    assert_eq!(ignored(&rig, "late"), 1);
    assert_eq!(ignored(&rig, "end"), 0);
}

#[test]
fn a_restarting_reload_traces_the_events_it_drops() {
    let mut rig = Rig::new(&reload_def(false));
    rig.tick();
    rig.tree.send_event("alert");
    let mut renamed = reload_def(false);
    renamed.states[1].name = "A2".into();
    renamed.states[0].children[0] = "A2".into();
    assert_eq!(rig.reload(compiled(&renamed)), ReloadOutcome::Restarted);
    assert!(
        rig.tree
            .trace()
            .any(|t| matches!(&t.kind, TraceKind::EventDropped(e) if e == "alert"))
    );
}

/// A task that submits a spatial query and installs a replacement state tree on the same entity.
#[derive(Resource)]
struct AudfixSubmitted(crate::spatial_query::RequestId);

#[derive(Default)]
struct ReplaceState;
impl BtTask for ReplaceState {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let entity = ctx.entity;
        let id = ctx.world.resource_mut::<crate::SpatialQueryManager>().submit(
            crate::spatial_query::SpatialQueryRequest::new("no-such-template", entity),
        );
        ctx.world.insert_resource(AudfixSubmitted(id));
        let replacement =
            StateTree::compile(&def_of("Root", vec![st("Root", &[])]), &BtTaskRegistry::default())
                .expect("the replacement tree compiles");
        ctx.world.entity_mut(entity).insert(replacement);
        BtStatus::Success
    }
}
register_task!(ReplaceState, "t::ReplaceState");

#[test]
fn audfix_replacing_a_state_tree_forgets_its_spatial_queries() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, crate::SpatialQueryPlugin));
    let mut tasks = registry();
    tasks.register::<ReplaceState>();
    let def = def_of("Root", vec![with_tasks(st("Root", &[]), &["t::ReplaceState"])]);
    let tree = StateTree::compile(&def, &tasks).expect("compiles");
    let entity = app.world_mut().spawn((Blackboard::default(), tree)).id();
    super::tick_state_trees(app.world_mut());
    assert!(
        app.world().get::<StateTree>(entity).is_some(),
        "the replacement tree stays on the agent"
    );
    let id = app.world().resource::<AudfixSubmitted>().0;
    let manager = app.world().resource::<crate::SpatialQueryManager>();
    assert!(
        !manager.is_active(id) && !manager.is_finished(id),
        "active={} finished={} (replacing the tree must drop the old owner's queries)",
        manager.is_active(id),
        manager.is_finished(id)
    );
}
