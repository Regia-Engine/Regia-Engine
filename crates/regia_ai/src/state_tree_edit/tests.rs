// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::state_tree::CompareOp;
use crate::task::{BtContext, BtStatus, BtTask};

struct Nop;

impl BtTask for Nop {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Running
    }
}

fn registry() -> BtTaskRegistry {
    let mut r = BtTaskRegistry::default();
    for path in ["game::Patrol", "game::Sense", "game::Chase"] {
        r.register_factory(path, || Box::new(Nop));
    }
    r
}

fn add(name: &str, parent: Option<&str>) -> StOp {
    StOp::AddState {
        name: name.into(),
        parent: parent.map(Into::into),
        index: None,
    }
}

fn transition(state: &str, trigger: Trigger, target: Target) -> StOp {
    StOp::AddTransition {
        state: state.into(),
        trigger,
        condition: None,
        target,
        priority: None,
        hold: None,
        index: None,
    }
}

/// The build-up of the guard fixture, one op of most kinds.
fn guard_ops() -> Vec<StOp> {
    vec![
        add("Root", None),
        add("Idle", Some("Root")),
        add("Alert", Some("Root")),
        add("Chase", Some("Alert")),
        StOp::SetTasks {
            name: "Alert".into(),
            tasks: vec!["game::Patrol".into()],
        },
        StOp::SetParam {
            state: "Idle".into(),
            key: "mood".into(),
            value: Some(Literal::String("calm".into())),
        },
        transition(
            "Idle",
            Trigger::Event("heard".into()),
            Target::State("Alert".into()),
        ),
        StOp::AddTransition {
            state: "Alert".into(),
            trigger: Trigger::Tick,
            condition: Some(Condition::Compare {
                key: "dist".into(),
                op: CompareOp::Lt,
                value: 12.0,
            }),
            target: Target::State("Chase".into()),
            priority: Some(2),
            hold: Some(0.25),
            index: None,
        },
        transition("Chase", Trigger::Succeeded, Target::Parent),
        StOp::SetState {
            name: "Alert".into(),
            selection: None,
            enter: None,
            weight: Some(2.0),
            utility: None,
            clear_utility: false,
            fallback: None,
        },
        StOp::SetTree {
            description: Some("guard".into()),
            seed: None,
            state_key: Some("state".into()),
        },
        StOp::SetAmbient {
            tasks: vec!["game::Sense".into()],
        },
    ]
}

/// The hand-written text the guard fixture must save as.
const GUARD_RON: &str = r#"(
    format_version: 2,
    description: "guard",
    root: "Root",
    state_key: "state",
    ambient: ["game::Sense"],
    states: [
        (
            name: "Root",
            children: ["Idle", "Alert"],
        ),
        (
            name: "Idle",
            enter_set: {"mood": String("calm")},
            transitions: [
                (trigger: Event("heard"), target: State("Alert")),
            ],
        ),
        (
            name: "Alert",
            children: ["Chase"],
            tasks: ["game::Patrol"],
            weight: 2.0,
            transitions: [
                (trigger: Tick, condition: Compare(key:"dist",op:Lt,value:12.0), priority: 2, hold: 0.25, target: State("Chase")),
            ],
        ),
        (
            name: "Chase",
            transitions: [
                (trigger: Succeeded, target: Parent),
            ],
        ),
    ],
)
"#;

fn guard() -> StateTreeDef {
    let mut def = StateTreeDef::new("");
    apply_ops(&mut def, &guard_ops()).expect("the build-up applies");
    def
}

fn round_trip(def: &StateTreeDef) -> StateTreeDef {
    let text = to_canonical_ron(def).expect("writes");
    let back = parse_statetree_ron(&text).expect("reads back");
    assert_eq!(
        &back, def,
        "the canonical text reads back to the same tree:\n{text}"
    );
    assert_eq!(to_canonical_ron(&back).expect("writes"), text, "stable text");
    back
}

fn compiles(def: &StateTreeDef, load: &dyn Fn(&str) -> Option<String>) {
    if let Err(e) = StateTree::compile_with(def, &registry(), load) {
        panic!(
            "does not compile: {e}\n{}",
            to_canonical_ron(def).unwrap_or_default()
        );
    }
}

fn no_includes(_: &str) -> Option<String> {
    None
}

#[test]
fn the_guard_build_up_saves_as_the_hand_written_text_and_compiles() {
    let def = guard();
    assert_eq!(to_canonical_ron(&def).expect("writes"), GUARD_RON);
    let back = round_trip(&def);
    compiles(&back, &no_includes);
    let issues = validate(
        &back,
        ValidateOptions {
            registry: Some(&registry()),
            ..ValidateOptions::default()
        },
    );
    assert!(issues.is_empty(), "{issues:?}");
    // The hand-written text parses to the same tree the ops built.
    assert_eq!(parse_statetree_ron(GUARD_RON).expect("parses"), def);
}

fn include_files() -> impl Fn(&str) -> Option<String> {
    |path: &str| match path {
        "ai/combat.statetree.ron" => Some(
            r#"(format_version: 2, root: "Fight", states: [
                (name: "Fight", children: ["Strike"]),
                (name: "Strike", transitions: [(trigger: Succeeded, target: State("Fight"))]),
            ])"#
            .to_string(),
        ),
        "ai/flee.statetree.ron" => {
            Some(r#"(format_version: 2, root: "Run", states: [(name: "Run")])"#.to_string())
        }
        _ => None,
    }
}

/// Every operation, applied to the guard: the result round-trips through the canonical text and
/// still compiles (includes resolved through a loader).
#[test]
fn every_operation_round_trips_and_compiles() {
    let load = include_files();
    let cases: Vec<(&str, Vec<StOp>)> = vec![
        (
            "add_state at an index",
            vec![StOp::AddState {
                name: "Rest".into(),
                parent: Some("Root".into()),
                index: Some(0),
            }],
        ),
        (
            "remove_state splices children",
            vec![StOp::RemoveState {
                name: "Alert".into(),
                subtree: false,
            }],
        ),
        (
            "remove_state subtree",
            vec![StOp::RemoveState {
                name: "Alert".into(),
                subtree: true,
            }],
        ),
        (
            "rename_state",
            vec![StOp::RenameState {
                name: "Alert".into(),
                to: "Alarm".into(),
            }],
        ),
        (
            "rename the root",
            vec![StOp::RenameState {
                name: "Root".into(),
                to: "Top".into(),
            }],
        ),
        (
            "reparent_state",
            vec![StOp::ReparentState {
                name: "Chase".into(),
                parent: "Root".into(),
                index: Some(1),
            }],
        ),
        (
            "reorder through reparent",
            vec![StOp::ReparentState {
                name: "Alert".into(),
                parent: "Root".into(),
                index: Some(0),
            }],
        ),
        (
            "set_root on a new top",
            vec![
                add("Top", None),
                StOp::SetRoot { name: "Top".into() },
                StOp::ReparentState {
                    name: "Root".into(),
                    parent: "Top".into(),
                    index: None,
                },
            ],
        ),
        (
            "set_state",
            vec![StOp::SetState {
                name: "Root".into(),
                selection: Some(Selection::Utility),
                enter: Some(Condition::Not(Box::new(Condition::Key("dead".into())))),
                weight: Some(0.5),
                utility: Some(UtilityDef {
                    key: "u".into(),
                    scale: 2.0,
                    bias: -1.0,
                }),
                clear_utility: false,
                fallback: Some(Fallback::NextEnterable),
            }],
        ),
        (
            "set_state clears the utility",
            vec![
                StOp::SetState {
                    name: "Idle".into(),
                    selection: None,
                    enter: None,
                    weight: None,
                    utility: Some(UtilityDef {
                        key: "u".into(),
                        scale: 1.0,
                        bias: 0.0,
                    }),
                    clear_utility: false,
                    fallback: None,
                },
                StOp::SetState {
                    name: "Idle".into(),
                    selection: None,
                    enter: None,
                    weight: None,
                    utility: None,
                    clear_utility: true,
                    fallback: None,
                },
            ],
        ),
        (
            "set_tasks",
            vec![StOp::SetTasks {
                name: "Idle".into(),
                tasks: vec!["game::Patrol".into(), "game::Chase".into()],
            }],
        ),
        ("set_ambient empty", vec![StOp::SetAmbient { tasks: vec![] }]),
        (
            "set_param each literal",
            vec![
                StOp::SetParam {
                    state: "Alert".into(),
                    key: "b".into(),
                    value: Some(Literal::Bool(true)),
                },
                StOp::SetParam {
                    state: "Alert".into(),
                    key: "i".into(),
                    value: Some(Literal::Int(-3)),
                },
                StOp::SetParam {
                    state: "Alert".into(),
                    key: "f".into(),
                    value: Some(Literal::Float(0.125)),
                },
            ],
        ),
        (
            "set_param removes",
            vec![StOp::SetParam {
                state: "Idle".into(),
                key: "mood".into(),
                value: None,
            }],
        ),
        (
            "add_transition at an index",
            vec![StOp::AddTransition {
                state: "Idle".into(),
                trigger: Trigger::Tick,
                condition: Some(Condition::All(vec![
                    Condition::Has("x".into()),
                    Condition::Text {
                        key: "mood".into(),
                        value: "angry".into(),
                    },
                ])),
                target: Target::Next,
                priority: Some(-1),
                hold: Some(1.5),
                index: Some(0),
            }],
        ),
        (
            "remove_transition",
            vec![StOp::RemoveTransition {
                state: "Idle".into(),
                index: 0,
            }],
        ),
        (
            "set_transition",
            vec![StOp::SetTransition {
                state: "Alert".into(),
                index: 0,
                trigger: Some(Trigger::Completed),
                condition: Some(Condition::Always),
                target: Some(Target::Succeed),
                priority: Some(0),
                hold: Some(0.0),
            }],
        ),
        (
            "move_transition",
            vec![
                transition("Idle", Trigger::Failed, Target::Fail),
                StOp::MoveTransition {
                    state: "Idle".into(),
                    from: 1,
                    to: 0,
                },
            ],
        ),
        (
            "add_include and target into it",
            vec![
                StOp::AddInclude {
                    prefix: "Combat".into(),
                    path: "ai/combat.statetree.ron".into(),
                    index: None,
                },
                StOp::ReparentState {
                    name: "Combat/Fight".into(),
                    parent: "Root".into(),
                    index: None,
                },
                transition("Idle", Trigger::Tick, Target::State("Combat/Strike".into())),
            ],
        ),
        (
            "set_include carries the prefix",
            vec![
                StOp::AddInclude {
                    prefix: "Combat".into(),
                    path: "ai/combat.statetree.ron".into(),
                    index: None,
                },
                StOp::ReparentState {
                    name: "Combat/Fight".into(),
                    parent: "Root".into(),
                    index: None,
                },
                transition("Idle", Trigger::Tick, Target::State("Combat/Strike".into())),
                StOp::SetInclude {
                    prefix: "Combat".into(),
                    to_prefix: Some("Melee".into()),
                    path: Some("ai/combat.statetree.ron".into()),
                },
            ],
        ),
        (
            "remove_include drops its references",
            vec![
                StOp::AddInclude {
                    prefix: "Combat".into(),
                    path: "ai/combat.statetree.ron".into(),
                    index: None,
                },
                StOp::ReparentState {
                    name: "Combat/Fight".into(),
                    parent: "Root".into(),
                    index: None,
                },
                transition("Idle", Trigger::Tick, Target::State("Combat/Strike".into())),
                StOp::RemoveInclude {
                    prefix: "Combat".into(),
                },
            ],
        ),
        (
            "set_tree",
            vec![StOp::SetTree {
                description: Some(String::new()),
                seed: Some(42),
                state_key: Some(String::new()),
            }],
        ),
    ];
    for (label, ops) in cases {
        let mut def = guard();
        if let Err(e) = apply_ops(&mut def, &ops) {
            panic!("{label}: {e:?}");
        }
        let back = round_trip(&def);
        compiles(&back, &load);
        let issues = validate(
            &back,
            ValidateOptions {
                registry: Some(&registry()),
                load: Some(&load),
                self_path: Some("ai/guard.statetree.ron"),
            },
        );
        assert!(is_valid(&issues), "{label}: {issues:?}");
    }
}

#[test]
fn a_rename_updates_the_root_children_and_every_target() {
    let mut def = guard();
    apply_ops(
        &mut def,
        &[
            transition(
                "Root",
                Trigger::Event("reset".into()),
                Target::State("Alert".into()),
            ),
            StOp::RenameState {
                name: "Alert".into(),
                to: "Alarm".into(),
            },
        ],
    )
    .expect("renames");
    let text = to_canonical_ron(&def).expect("writes");
    assert!(
        !text.contains("\"Alert\""),
        "no reference to the old name is left:\n{text}"
    );
    assert_eq!(def.states[0].children, ["Idle", "Alarm"]);
    assert_eq!(def.states[1].transitions[0].target, Target::State("Alarm".into()));
    assert_eq!(def.states[0].transitions[0].target, Target::State("Alarm".into()));
    compiles(&def, &no_includes);
    // The expected text is the fixture's with the name replaced.
    let mut guard_only = guard();
    apply_op(
        &mut guard_only,
        &StOp::RenameState {
            name: "Alert".into(),
            to: "Alarm".into(),
        },
    )
    .expect("renames");
    assert_eq!(
        to_canonical_ron(&guard_only).expect("writes"),
        GUARD_RON.replace("\"Alert\"", "\"Alarm\"")
    );
}

#[test]
fn removing_a_state_drops_transitions_into_it_and_splices_its_children() {
    let mut def = guard();
    apply_op(
        &mut def,
        &StOp::RemoveState {
            name: "Alert".into(),
            subtree: false,
        },
    )
    .expect("removes");
    assert_eq!(def.states[0].children, ["Idle", "Chase"]);
    assert!(
        def.states[1].transitions.is_empty(),
        "Idle's transition to Alert is gone"
    );
    compiles(&def, &no_includes);
    // The root with one child hands the root over.
    let mut small = StateTreeDef::new("");
    apply_ops(
        &mut small,
        &[add("R", None), add("A", Some("R")), add("B", Some("A"))],
    )
    .expect("builds");
    apply_op(
        &mut small,
        &StOp::RemoveState {
            name: "R".into(),
            subtree: false,
        },
    )
    .expect("hands over");
    assert_eq!(small.root, "A");
    compiles(&small, &no_includes);
}

fn code_of(def: &mut StateTreeDef, op: StOp) -> &'static str {
    let before = def.clone();
    let err = apply_op(def, &op).expect_err("refused");
    assert_eq!(*def, before, "a refused op changes nothing");
    err.code
}

#[test]
fn refused_operations_name_their_code_and_change_nothing() {
    let mut def = guard();
    assert_eq!(code_of(&mut def, add("Idle", Some("Root"))), "duplicate_name");
    assert_eq!(code_of(&mut def, add("", Some("Root"))), "empty_name");
    assert_eq!(code_of(&mut def, add("X", Some("Nope"))), "unknown_state");
    assert_eq!(
        code_of(
            &mut def,
            StOp::AddState {
                name: "X".into(),
                parent: Some("Root".into()),
                index: Some(9),
            }
        ),
        "bad_index"
    );
    assert_eq!(
        code_of(
            &mut def,
            StOp::RenameState {
                name: "Idle".into(),
                to: "Alert".into(),
            }
        ),
        "duplicate_name"
    );
    assert_eq!(
        code_of(
            &mut def,
            StOp::ReparentState {
                name: "Alert".into(),
                parent: "Chase".into(),
                index: None,
            }
        ),
        "would_cycle"
    );
    assert_eq!(
        code_of(
            &mut def,
            StOp::ReparentState {
                name: "Root".into(),
                parent: "Idle".into(),
                index: None,
            }
        ),
        "is_root"
    );
    assert_eq!(
        code_of(
            &mut def,
            StOp::RemoveState {
                name: "Root".into(),
                subtree: false,
            }
        ),
        "cannot_remove_root"
    );
    assert_eq!(
        code_of(&mut def, StOp::SetRoot { name: "Idle".into() }),
        "has_parent"
    );
    assert_eq!(
        code_of(
            &mut def,
            StOp::RemoveTransition {
                state: "Idle".into(),
                index: 5,
            }
        ),
        "bad_index"
    );
    assert_eq!(
        code_of(
            &mut def,
            StOp::AddTransition {
                state: "Idle".into(),
                trigger: Trigger::Tick,
                condition: None,
                target: Target::Stay,
                priority: None,
                hold: Some(f32::NAN),
                index: None,
            }
        ),
        "bad_value"
    );
    assert_eq!(
        code_of(
            &mut def,
            StOp::SetParam {
                state: "Idle".into(),
                key: String::new(),
                value: Some(Literal::Int(1)),
            }
        ),
        "bad_value"
    );
    assert_eq!(
        code_of(
            &mut def,
            StOp::RemoveInclude {
                prefix: "Nope".into()
            }
        ),
        "unknown_include"
    );
    apply_op(
        &mut def,
        &StOp::AddInclude {
            prefix: "C".into(),
            path: "c.statetree.ron".into(),
            index: None,
        },
    )
    .expect("adds");
    assert_eq!(
        code_of(
            &mut def,
            StOp::AddInclude {
                prefix: "C".into(),
                path: "d.statetree.ron".into(),
                index: None,
            }
        ),
        "duplicate_prefix"
    );
    // A batch is all or nothing and names the failing op.
    let before = def.clone();
    let err =
        apply_ops(&mut def, &[add("New", Some("Root")), add("New", Some("Root"))]).expect_err("second fails");
    assert_eq!((err.op_index, err.error.code), (1, "duplicate_name"));
    assert_eq!(def, before);
}

fn codes(text: &str, files: &[(&str, &str)]) -> Vec<&'static str> {
    let def = parse_statetree_ron(text).unwrap_or_else(|e| panic!("fixture parses: {e}\n{text}"));
    let files: Vec<(String, String)> = files.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect();
    let load = move |p: &str| files.iter().find(|(f, _)| f == p).map(|(_, t)| t.clone());
    let issues = validate(
        &def,
        ValidateOptions {
            registry: Some(&registry()),
            load: Some(&load),
            self_path: Some("self.statetree.ron"),
        },
    );
    // The compiler agrees that the fixture is bad.
    assert!(
        StateTree::compile_with(&def, &registry(), &load).is_err()
            || issues.iter().all(|i| i.severity == StSeverity::Warning),
        "the compiler accepts a fixture the validator refuses: {issues:?}"
    );
    issues.iter().map(|i| i.code).collect()
}

/// A validator fixture: the code it must raise, the tree text and the files it includes.
type Fixture = (&'static str, String, Vec<(&'static str, &'static str)>);

/// Each validator code fires on a minimal bad definition, with a path to the field.
#[test]
fn each_validator_code_fires_on_a_minimal_bad_fixture() {
    let one = |body: &str| format!("(format_version: 2, root: \"R\", states: [{body}])");
    let cases: Vec<Fixture> = vec![
        ("empty_tree", "(format_version: 2, root: \"R\", states: [])".into(), vec![]),
        ("empty_name", one("(name: \"R\", children: [\"\"]), (name: \"\")"), vec![]),
        ("duplicate_name", one("(name: \"R\", children: [\"A\"]), (name: \"A\"), (name: \"A\")"), vec![]),
        ("missing_root", "(format_version: 2, root: \"Nope\", states: [(name: \"R\")])".into(), vec![]),
        ("missing_child", one("(name: \"R\", children: [\"Ghost\"])"), vec![]),
        (
            "missing_target",
            one("(name: \"R\", transitions: [(trigger: Tick, target: State(\"Ghost\"))])"),
            vec![],
        ),
        (
            "multiple_parents",
            one("(name: \"R\", children: [\"A\", \"B\"]), (name: \"A\", children: [\"B\"]), (name: \"B\")"),
            vec![],
        ),
        ("unreachable_state", one("(name: \"R\"), (name: \"Lost\")"), vec![]),
        ("empty_task", one("(name: \"R\", tasks: [\"\"])"), vec![]),
        ("unknown_task", one("(name: \"R\", tasks: [\"game::Nope\"])"), vec![]),
        (
            "no_next_sibling",
            one("(name: \"R\", children: [\"A\"]), (name: \"A\", transitions: [(trigger: Tick, target: Next)])"),
            vec![],
        ),
        (
            "parent_of_root",
            one("(name: \"R\", transitions: [(trigger: Tick, target: Parent)])"),
            vec![],
        ),
        (
            "hold_on_event",
            one("(name: \"R\", transitions: [(trigger: Event(\"e\"), hold: 1.0, target: Stay)])"),
            vec![],
        ),
        (
            "bad_value",
            one("(name: \"R\", transitions: [(trigger: Tick, hold: -1.0, target: Stay)])"),
            vec![],
        ),
        (
            "condition_too_deep",
            one(&format!(
                "(name: \"R\", enter: {}Always{})",
                "Not(".repeat(17),
                ")".repeat(17)
            )),
            vec![],
        ),
        (
            "bad_include",
            "(format_version: 2, root: \"R\", includes: [(prefix: \"\", path: \"a.statetree.ron\")], states: [(name: \"R\")])"
                .into(),
            vec![],
        ),
        (
            "include_missing",
            "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"gone.statetree.ron\")], states: [(name: \"R\")])"
                .into(),
            vec![],
        ),
        (
            "include_invalid",
            "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"bad.statetree.ron\")], states: [(name: \"R\")])"
                .into(),
            vec![("bad.statetree.ron", "(not a tree")],
        ),
        (
            "include_cycle",
            "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"a.statetree.ron\")], states: [(name: \"R\")])"
                .into(),
            vec![(
                "a.statetree.ron",
                "(format_version: 2, root: \"A\", includes: [(prefix: \"q\", path: \"self.statetree.ron\")], states: [(name: \"A\")])",
            )],
        ),
        (
            "include_depth",
            "(format_version: 2, root: \"R\", includes: [(prefix: \"p\", path: \"d1.statetree.ron\")], states: [(name: \"R\")])"
                .into(),
            vec![
                ("d1.statetree.ron", "(format_version: 2, root: \"A\", includes: [(prefix: \"p\", path: \"d2.statetree.ron\")], states: [(name: \"A\")])"),
                ("d2.statetree.ron", "(format_version: 2, root: \"A\", includes: [(prefix: \"p\", path: \"d3.statetree.ron\")], states: [(name: \"A\")])"),
                ("d3.statetree.ron", "(format_version: 2, root: \"A\", includes: [(prefix: \"p\", path: \"d4.statetree.ron\")], states: [(name: \"A\")])"),
                ("d4.statetree.ron", "(format_version: 2, root: \"A\", includes: [(prefix: \"p\", path: \"d5.statetree.ron\")], states: [(name: \"A\")])"),
                ("d5.statetree.ron", "(format_version: 2, root: \"A\", states: [(name: \"A\")])"),
            ],
        ),
    ];
    for (code, text, files) in &cases {
        let found = codes(text, files);
        assert!(found.contains(code), "{code}: found {found:?}");
    }
    // Every error code the type documents has a fixture here (besides the version and size limits,
    // tested on their own below).
    let documented = [
        "empty_tree",
        "empty_name",
        "duplicate_name",
        "missing_root",
        "missing_child",
        "missing_target",
        "multiple_parents",
        "unreachable_state",
        "empty_task",
        "unknown_task",
        "no_next_sibling",
        "parent_of_root",
        "hold_on_event",
        "bad_value",
        "condition_too_deep",
        "bad_include",
        "include_missing",
        "include_invalid",
        "include_cycle",
        "include_depth",
    ];
    for code in documented {
        assert!(cases.iter().any(|(c, _, _)| *c == code), "no fixture for {code}");
    }
}

#[test]
fn version_and_size_limits_are_reported() {
    let mut def = guard();
    def.format_version = 9;
    let issues = validate(&def, ValidateOptions::default());
    assert_eq!(issues[0].code, "unsupported_version");
    assert_eq!(issues[0].path, "format_version");
    let mut big = StateTreeDef::new("S0");
    big.states = (0..=MAX_STATES).map(|i| StateDef::new(format!("S{i}"))).collect();
    big.states[0].children = (1..=MAX_STATES).map(|i| format!("S{i}")).collect();
    let issues = validate(&big, ValidateOptions::default());
    assert!(
        issues.iter().any(|i| i.code == "too_many_states"),
        "{:?}",
        issues.first()
    );
}

#[test]
fn issues_carry_paths_and_every_problem_is_listed_at_once() {
    let text = r#"(format_version: 2, root: "R", states: [
        (name: "R", children: ["A", "Ghost"], transitions: [(trigger: Tick, target: State("Nope"))]),
        (name: "A", tasks: ["game::Nope"]),
        (name: "Lost"),
    ])"#;
    let def = parse_statetree_ron(text).expect("parses");
    let issues = validate(
        &def,
        ValidateOptions {
            registry: Some(&registry()),
            ..ValidateOptions::default()
        },
    );
    let pairs: Vec<(&str, &str)> = issues.iter().map(|i| (i.code, i.path.as_str())).collect();
    assert!(
        pairs.contains(&("missing_child", "states/R/children/1")),
        "{pairs:?}"
    );
    assert!(
        pairs.contains(&("missing_target", "states/R/transitions/0/target")),
        "{pairs:?}"
    );
    assert!(pairs.contains(&("unknown_task", "states/A/tasks/0")), "{pairs:?}");
    assert!(pairs.contains(&("unreachable_state", "states/Lost")), "{pairs:?}");
    assert_eq!(
        issues
            .iter()
            .find(|i| i.code == "unknown_task")
            .and_then(StIssue::state),
        Some("A")
    );
    // Without a registry, tasks are not checked.
    let issues = validate(&def, ValidateOptions::default());
    assert!(!issues.iter().any(|i| i.code == "unknown_task"));
}

#[test]
fn an_unresolved_include_does_not_make_its_targets_missing() {
    let text = r#"(format_version: 2, root: "R", includes: [(prefix: "Combat", path: "c.statetree.ron")], states: [
        (name: "R", children: ["Combat/Fight"], transitions: [(trigger: Tick, target: State("Combat/Strike"))]),
    ])"#;
    let def = parse_statetree_ron(text).expect("parses");
    let issues = validate(&def, ValidateOptions::default());
    assert!(
        issues.is_empty(),
        "no loader: includes are not followed and their names not judged: {issues:?}"
    );
    let load = include_files();
    let fixed = text.replace("c.statetree.ron", "ai/combat.statetree.ron");
    let def = parse_statetree_ron(&fixed).expect("parses");
    let issues = validate(
        &def,
        ValidateOptions {
            registry: Some(&registry()),
            load: Some(&load),
            self_path: None,
        },
    );
    assert!(issues.is_empty(), "the merged tree is checked: {issues:?}");
}

#[test]
fn the_hierarchy_lists_the_tree_depth_first_then_the_strays() {
    let mut def = guard();
    apply_op(&mut def, &add("Stray", None)).expect("adds");
    let names: Vec<(&str, usize)> = hierarchy(&def)
        .into_iter()
        .map(|(i, d)| (def.states[i].name.as_str(), d))
        .collect();
    assert_eq!(
        names,
        [("Root", 0), ("Idle", 1), ("Alert", 1), ("Chase", 2), ("Stray", 0)]
    );
    let parents = parents(&def);
    assert_eq!(parents["Chase"].as_deref(), Some("Alert"));
    assert_eq!(parents["Root"], None);
}

#[test]
fn non_finite_numbers_cannot_be_written() {
    let mut def = guard();
    def.states[2].weight = f32::INFINITY;
    assert_eq!(to_canonical_ron(&def).expect_err("refused").code, "bad_value");
    let issues = validate(&def, ValidateOptions::default());
    assert!(
        issues
            .iter()
            .any(|i| i.code == "bad_value" && i.path == "states/Alert/weight")
    );
}
