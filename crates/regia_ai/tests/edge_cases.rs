// SPDX-License-Identifier: MPL-2.0
//! Degenerate inputs to the AI layer: malformed behaviour-tree definitions (empty arena, bad
//! indices, cycles), odd blackboard contents, non-finite perception inputs and trees ticking on
//! entities that lack a blackboard.

use regia_ai::{
    BbValue, BehaviorTree, BehaviorTreeBuilder, BehaviorTreeDef, Blackboard, BtCompileError, BtContext,
    BtStatus, BtTask, BtTaskRegistration, BtTaskRegistry, DefKind, DefNode, HearingEvent, ParallelPolicy,
    Perception, can_hear, in_sight_cone, line_of_sight,
};
use regia_app::prelude::*;
use regia_core::math::Vec3;

#[derive(Default)]
struct Succeed;
impl BtTask for Succeed {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Success
    }
}
impl BtTaskRegistration for Succeed {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Succeed)
    }
    fn task_type_path() -> &'static str {
        "edge::Succeed"
    }
}

fn registry() -> BtTaskRegistry {
    let mut r = BtTaskRegistry::default();
    r.register::<Succeed>();
    r
}

fn node(kind: DefKind, children: Vec<usize>) -> DefNode {
    DefNode {
        kind,
        children,
        ..DefNode::default()
    }
}

fn leaf() -> DefNode {
    DefNode {
        kind: DefKind::Leaf,
        task: "edge::Succeed".to_string(),
        ..DefNode::default()
    }
}

fn malformed(def: &BehaviorTreeDef) -> bool {
    matches!(
        BehaviorTree::compile(def, &registry()),
        Err(BtCompileError::Malformed(_))
    )
}

#[test]
fn an_empty_arena_is_rejected_not_ticked() {
    assert!(malformed(&BehaviorTreeDef::default()));
}

#[test]
fn a_root_outside_the_arena_is_rejected() {
    let def = BehaviorTreeDef {
        root: 3,
        nodes: vec![leaf()],
        ..BehaviorTreeDef::default()
    };
    assert!(malformed(&def));
}

#[test]
fn composite_children_outside_the_arena_are_rejected() {
    for kind in [DefKind::Selector, DefKind::Sequence, DefKind::Parallel] {
        let def = BehaviorTreeDef {
            root: 0,
            nodes: vec![node(kind, vec![1, 7]), leaf()],
            ..BehaviorTreeDef::default()
        };
        assert!(malformed(&def), "{kind:?}");
    }
}

#[test]
fn decorators_need_exactly_one_in_range_child() {
    for kind in [
        DefKind::Inverter,
        DefKind::Cooldown,
        DefKind::Repeat,
        DefKind::Timeout,
        DefKind::Service,
    ] {
        for children in [vec![], vec![1, 1], vec![9]] {
            let def = BehaviorTreeDef {
                root: 0,
                nodes: vec![
                    DefNode {
                        task: "edge::Succeed".to_string(),
                        ..node(kind, children.clone())
                    },
                    leaf(),
                ],
                ..BehaviorTreeDef::default()
            };
            assert!(
                BehaviorTree::compile(&def, &registry()).is_err(),
                "{kind:?} with children {children:?}"
            );
        }
    }
}

#[test]
fn cycles_are_rejected_instead_of_overflowing_the_stack() {
    // A node that lists itself.
    let direct = BehaviorTreeDef {
        root: 0,
        nodes: vec![node(DefKind::Selector, vec![0])],
        ..BehaviorTreeDef::default()
    };
    assert!(malformed(&direct));
    // A longer loop: 0 -> 1 -> 2 -> 0.
    let long = BehaviorTreeDef {
        root: 0,
        nodes: vec![
            node(DefKind::Sequence, vec![1]),
            node(DefKind::Inverter, vec![2]),
            node(DefKind::Selector, vec![0, 3]),
            leaf(),
        ],
        ..BehaviorTreeDef::default()
    };
    assert!(malformed(&long));
}

#[test]
fn a_cycle_among_unreachable_nodes_is_rejected() {
    // Nodes 1 and 2 loop but nothing reachable from the root uses them. They are still
    // compiled, so the cycle is refused too (ADR-1315's three-color pass over every node).
    let def = BehaviorTreeDef {
        root: 0,
        nodes: vec![
            leaf(),
            node(DefKind::Selector, vec![2]),
            node(DefKind::Selector, vec![1]),
        ],
        ..BehaviorTreeDef::default()
    };
    assert!(malformed(&def));
}

#[test]
fn shared_children_are_rejected() {
    // Both branches of the selector reach the same leaf: node state is per node, so two parents
    // would share one task instance and its latches (ADR-1315). Not a stack overflow either way.
    let mut b = BehaviorTreeBuilder::new();
    let shared = b.leaf("shared", "edge::Succeed");
    let left = b.inverter("left", shared);
    let right = b.inverter("right", shared);
    let root = b.selector("root", vec![left, right]);
    let def = b.build(root);
    assert!(malformed(&def));
}

#[test]
fn a_leaf_with_stray_children_is_rejected() {
    // A child index outside the arena, and a leaf that lists itself, are refused (ADR-1315).
    let def = BehaviorTreeDef {
        root: 0,
        nodes: vec![DefNode {
            children: vec![0, 99],
            ..leaf()
        }],
        ..BehaviorTreeDef::default()
    };
    assert!(malformed(&def));
}

#[test]
fn an_unregistered_leaf_names_the_node_and_the_path() {
    let def = BehaviorTreeDef::leaf("shoot", "nowhere::Missing");
    match BehaviorTree::compile(&def, &registry()) {
        Err(BtCompileError::UnknownTask(name, path)) => {
            assert_eq!(name, "shoot");
            assert_eq!(path, "nowhere::Missing");
        }
        other => panic!("expected UnknownTask, got {:?}", other.err()),
    }
}

fn tick_once(def: &BehaviorTreeDef, blackboard: Option<Blackboard>) -> BtStatus {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let entity = match blackboard {
        Some(bb) => app.world_mut().spawn(bb).id(),
        None => app.world_mut().spawn_empty().id(),
    };
    let mut tree = BehaviorTree::compile(def, &registry()).expect("compiles");
    tree.tick(&mut BtContext {
        world: app.world_mut(),
        entity,
        dt: 0.016,
    })
}

#[test]
fn empty_composites_follow_the_identity_rules() {
    // A sequence of nothing succeeds, a selector of nothing fails, a parallel of nothing
    // satisfies RequireAll vacuously and fails RequireOne.
    assert_eq!(
        tick_once(&BehaviorTreeDef::sequence("s", vec![]), None),
        BtStatus::Success
    );
    assert_eq!(
        tick_once(&BehaviorTreeDef::selector("s", vec![]), None),
        BtStatus::Failure
    );
    let parallel = |policy| BehaviorTreeDef {
        root: 0,
        nodes: vec![DefNode {
            policy,
            ..node(DefKind::Parallel, vec![])
        }],
        ..BehaviorTreeDef::default()
    };
    assert_eq!(
        tick_once(&parallel(ParallelPolicy::RequireAll), None),
        BtStatus::Success
    );
    assert_eq!(
        tick_once(&parallel(ParallelPolicy::RequireOne), None),
        BtStatus::Failure
    );
}

#[test]
fn a_condition_on_an_entity_without_a_blackboard_fails() {
    let def = BehaviorTreeDef::condition("c", "flag");
    assert_eq!(tick_once(&def, None), BtStatus::Failure);
    assert_eq!(tick_once(&def, Some(Blackboard::new())), BtStatus::Failure);
    let mut bb = Blackboard::new();
    bb.insert("flag", 1_i32);
    assert_eq!(
        tick_once(&def, Some(bb)),
        BtStatus::Failure,
        "only Bool(true) holds"
    );
    let mut bb = Blackboard::new();
    bb.insert("flag", true);
    assert_eq!(tick_once(&def, Some(bb)), BtStatus::Success);
}

#[test]
fn blackboard_reads_of_the_wrong_type_use_defaults() {
    let mut bb = Blackboard::new();
    bb.insert("s", "text");
    bb.insert("f", 2.9_f32);
    bb.insert("i", 7_i64);
    assert_eq!(bb.get_int("s"), 0);
    assert_eq!(bb.get_float("s"), 0.0);
    assert!(!bb.get_bool("s"));
    assert_eq!(bb.get_vec3("s"), Vec3::ZERO);
    assert_eq!(bb.get_string("f"), "");
    // Ints and floats convert into each other (truncating).
    assert_eq!(bb.get_int("f"), 2);
    assert_eq!(bb.get_float("i"), 7.0);
    // Out-of-range finite floats saturate rather than panic; NaN and infinity read as the
    // default (ADR-1059).
    bb.insert("nan", f32::NAN);
    bb.insert("inf", f32::INFINITY);
    bb.insert("big", 1.0e30_f32);
    assert_eq!(bb.get_int("nan"), 0);
    assert_eq!(bb.get_int("inf"), 0);
    assert_eq!(bb.get_int("big"), i64::MAX);
    assert!(bb.get_float("nan").is_nan());
}

#[test]
fn entity_values_with_arbitrary_bits_never_panic() {
    let mut bb = Blackboard::new();
    for (i, bits) in [0_u64, 1, u64::MAX, u64::from(u32::MAX), 1 << 63]
        .into_iter()
        .enumerate()
    {
        let key = format!("e{i}");
        bb.values.insert(key.clone(), BbValue::Entity(bits));
        let _ = bb.get_entity(&key);
    }
    assert_eq!(bb.get_entity("missing"), Entity::PLACEHOLDER);
}

#[test]
fn blackboard_empty_and_unusual_keys() {
    let mut bb = Blackboard::new();
    assert!(!bb.contains(""));
    bb.insert("", true);
    assert!(bb.get_bool(""));
    assert_eq!(bb.insert("", false), Some(BbValue::Bool(true)));
    assert!(bb.remove("").is_some());
    assert!(bb.remove("").is_none());
    assert_eq!(bb, Blackboard::default());
}

/// A target directly above the eye has no horizontal bearing: an unrestricted cone still sees it
/// (as `can_hear` would count it), a narrow one does not.
#[test]
fn sight_cone_sees_straight_above_only_when_unrestricted() {
    let eye = Vec3::ZERO;
    let above = Vec3::new(0.0, 2.0, 0.0);
    assert!(in_sight_cone(eye, Vec3::X, above, std::f32::consts::PI, 40.0));
    assert!(in_sight_cone(eye, Vec3::ZERO, above, 0.5, 40.0));
    assert!(!in_sight_cone(eye, Vec3::X, above, 0.5, 40.0));
    assert!(
        !in_sight_cone(eye, Vec3::X, above, std::f32::consts::PI, 1.0),
        "out of range in 3D"
    );
    assert!(!in_sight_cone(eye, Vec3::ZERO, eye, 0.5, 40.0), "on the eye");
}

#[test]
fn sight_cone_rejects_non_finite_and_degenerate_inputs() {
    let eye = Vec3::ZERO;
    let fwd = Vec3::X;
    let half = 45_f32.to_radians();
    assert!(in_sight_cone(eye, fwd, Vec3::new(5.0, 0.0, 0.0), half, 10.0));
    // Outside the range, behind, or standing on the eye.
    assert!(!in_sight_cone(eye, fwd, Vec3::new(50.0, 0.0, 0.0), half, 10.0));
    assert!(!in_sight_cone(eye, fwd, Vec3::new(-5.0, 0.0, 0.0), half, 10.0));
    assert!(!in_sight_cone(eye, fwd, Vec3::ZERO, half, 10.0));
    // Non-finite targets or eyes are never in view.
    assert!(!in_sight_cone(eye, fwd, Vec3::NAN, half, 10.0));
    assert!(!in_sight_cone(
        eye,
        fwd,
        Vec3::new(f32::INFINITY, 0.0, 0.0),
        half,
        f32::INFINITY
    ));
    assert!(!in_sight_cone(Vec3::NAN, fwd, Vec3::X, half, 10.0));
    // A zero facing direction sees all around; a zero range sees nothing.
    assert!(in_sight_cone(
        eye,
        Vec3::ZERO,
        Vec3::new(0.0, 0.0, 5.0),
        half,
        10.0
    ));
    assert!(!in_sight_cone(eye, fwd, Vec3::new(5.0, 0.0, 0.0), half, 0.0));
    // The vertical offset is ignored: a target straight above the eye is on the eye's spot.
    assert!(!in_sight_cone(eye, fwd, Vec3::new(0.0, 5.0, 0.0), half, 10.0));
    // A NaN cone angle admits nothing.
    assert!(!in_sight_cone(eye, fwd, Vec3::new(5.0, 0.0, 0.0), f32::NAN, 10.0));
}

#[test]
fn hearing_edge_cases() {
    let listener = Perception::default();
    let at = |x: f32, radius: f32, team: u8| HearingEvent {
        position: Vec3::new(x, 0.0, 0.0),
        radius,
        team,
        source: Entity::PLACEHOLDER,
    };
    assert!(can_hear(&listener, Vec3::ZERO, &at(5.0, 10.0, 0)));
    // The range is inclusive at the boundary of both radii.
    assert!(can_hear(&listener, Vec3::ZERO, &at(10.0, 10.0, 0)));
    assert!(can_hear(&listener, Vec3::ZERO, &at(15.0, 20.0, 0)));
    assert!(!can_hear(&listener, Vec3::ZERO, &at(15.5, 20.0, 0)));
    // Either radius can be the limit. Any team is audible (the attitude table decides who counts).
    assert!(!can_hear(&listener, Vec3::ZERO, &at(12.0, 10.0, 0)));
    assert!(can_hear(&listener, Vec3::ZERO, &at(5.0, 10.0, 1)));
    assert!(!can_hear(&listener, Vec3::ZERO, &at(12.0, 10.0, 1)));
    // A zero radius hears only a sound at the same point.
    assert!(can_hear(&listener, Vec3::ZERO, &at(0.0, 0.0, 0)));
    assert!(!can_hear(&listener, Vec3::ZERO, &at(0.1, 0.0, 0)));
    // NaN anywhere: nothing is heard.
    assert!(!can_hear(&listener, Vec3::NAN, &at(1.0, 10.0, 0)));
    assert!(!can_hear(&listener, Vec3::ZERO, &at(f32::NAN, 10.0, 0)));
    assert!(!can_hear(&listener, Vec3::ZERO, &at(1.0, f32::NAN, 0)));
    let deaf = Perception {
        hearing_radius: f32::NAN,
        ..Perception::default()
    };
    assert!(!can_hear(&deaf, Vec3::ZERO, &at(1.0, 10.0, 0)));
}

#[test]
fn line_of_sight_without_physics_is_clear() {
    let world = World::new();
    assert!(line_of_sight(&world, Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0), &[]).is_none());
    // Coincident and non-finite endpoints do not panic either.
    assert!(line_of_sight(&world, Vec3::ONE, Vec3::ONE, &[]).is_none());
    assert!(line_of_sight(&world, Vec3::NAN, Vec3::ONE, &[]).is_none());
}
