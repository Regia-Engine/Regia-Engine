// SPDX-License-Identifier: MPL-2.0
//! The anim graph editor authors the v4 IK nodes (ADR-8475): `chain_ik` and `body_aim` are node
//! kinds of `editor.anim_add_node`, their settings edit through `editor.anim_set_node`, they
//! compile into the saved `.animgraph.ron`, and every step undoes. No window.

use regia_animation::chain_ik::{ChainSolver, JointLimit};
use regia_animation::graph::{AnimGraph, AnimNode, ChainLimits};
use regia_app::prelude::*;
use regia_editor::anim::AnimEditor;
use regia_editor::anim::model::NodeKind;
use regia_editor::command::UndoStack;
use serde_json::{Value, json};

fn handler_world() -> World {
    let mut world = World::new();
    world.init_resource::<AnimEditor>();
    world.init_resource::<UndoStack>();
    world
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    let handler = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .map(|(_, h)| h)
        .unwrap_or_else(|| panic!("no {method}"));
    handler(world, &params).map_err(|e| format!("{e:?}"))
}

fn h(world: &mut World, method: &str, params: Value) -> Value {
    call(world, method, params).unwrap_or_else(|e| panic!("{method}: {e}"))
}

fn undo(world: &mut World) -> bool {
    world.resource_scope(|world, mut stack: Mut<UndoStack>| stack.undo(world))
}

fn compiled(world: &World) -> AnimGraph {
    world
        .resource::<AnimEditor>()
        .docs
        .graph
        .compile_validated()
        .expect("the graph compiles and validates")
        .graph
}

#[test]
fn editor_creates_and_undoes_a_chain_ik_node() {
    let mut world = handler_world();
    h(&mut world, "editor.anim_open", json!({ "create": true }));
    h(&mut world, "editor.anim_rig", json!({ "source": "demo" }));
    h(
        &mut world,
        "editor.anim_param",
        json!({ "name": "Reach", "kind": "vec3" }),
    );
    let clip = h(
        &mut world,
        "editor.anim_add_node",
        json!({ "kind": "clip", "data": { "clip": "Idle" } }),
    )["node"]
        .as_u64()
        .expect("clip id");
    let nodes_before = world.resource::<AnimEditor>().docs.graph.nodes.len();
    let chain = h(
        &mut world,
        "editor.anim_add_node",
        json!({ "kind": "chain_ik", "data": {
            "root": "spine", "tip": "head", "solver": "Ccd",
            "target": { "Param": "Reach" },
            "limits": { "Uniform": { "Cone": { "max_swing": 0.4, "min_twist": -0.1, "max_twist": 0.1 } } },
            "iterations": 48
        } }),
    )["node"]
        .as_u64()
        .expect("chain id");
    h(
        &mut world,
        "editor.anim_connect",
        json!({ "child": clip, "parent": chain, "slot": "inner" }),
    );
    h(&mut world, "editor.anim_connect", json!({ "child": chain }));

    let graph = compiled(&world);
    let AnimNode::ChainIK(node) = &graph.root else {
        panic!("the output is not a chain IK node: {:?}", graph.root);
    };
    assert_eq!((node.root.as_str(), node.tip.as_str()), ("spine", "head"));
    assert_eq!(node.solver, ChainSolver::Ccd);
    assert_eq!(node.iterations, 48);
    assert!(matches!(
        node.limits,
        Some(ChainLimits::Uniform(JointLimit::Cone { .. }))
    ));
    assert!(matches!(node.inner.as_ref(), AnimNode::Clip(_)));
    let ron = h(&mut world, "editor.anim_graph", json!({ "ron": true }))["ron"]
        .as_str()
        .map(str::to_owned)
        .expect("ron");
    assert!(ron.contains("format_version: 4"), "{ron}");
    assert_eq!(
        AnimGraph::from_ron_str(&ron).expect("the saved graph loads"),
        graph
    );

    // The node reports its kind and settings to the agent.
    let listed = h(&mut world, "editor.anim_graph", json!({}));
    let entry = listed["nodes"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|n| n["id"].as_u64() == Some(chain)))
        .cloned()
        .expect("chain node listed");
    assert_eq!(entry["kind"], "chain_ik");
    assert_eq!(entry["data"]["tip"], "head");

    // An edit through anim_set_node, then undo it.
    h(
        &mut world,
        "editor.anim_set_node",
        json!({ "node": chain, "data": { "solver": "Fabrik", "pole": "Reach" } }),
    );
    let AnimNode::ChainIK(edited) = compiled(&world).root else {
        panic!("chain IK root");
    };
    assert_eq!(edited.solver, ChainSolver::Fabrik);
    assert_eq!(edited.pole.as_deref(), Some("Reach"));
    assert!(undo(&mut world), "the edit undoes");
    let AnimNode::ChainIK(restored) = compiled(&world).root else {
        panic!("chain IK root");
    };
    assert_eq!(restored.solver, ChainSolver::Ccd);
    assert_eq!(restored.pole, None);

    // Undo both connections and the creation: the node is gone again.
    for _ in 0..3 {
        assert!(undo(&mut world), "a step undoes");
    }
    let editor = world.resource::<AnimEditor>();
    assert!(
        !editor.docs.graph.nodes.contains_key(&(chain as u32)),
        "undo removed the chain IK node"
    );
    assert_eq!(editor.docs.graph.nodes.len(), nodes_before);
}

#[test]
fn editor_creates_a_body_aim_node_and_refuses_bad_data() {
    let mut world = handler_world();
    h(&mut world, "editor.anim_open", json!({ "create": true }));
    h(&mut world, "editor.anim_rig", json!({ "source": "demo" }));
    h(
        &mut world,
        "editor.anim_param",
        json!({ "name": "Look", "kind": "vec3" }),
    );
    let clip = h(
        &mut world,
        "editor.anim_add_node",
        json!({ "kind": "clip", "data": { "clip": "Idle" } }),
    )["node"]
        .clone();
    let aim = h(
        &mut world,
        "editor.anim_add_node",
        json!({ "kind": "body_aim", "data": {
            "joints": [["spine", 1.0], ["chest", 1.0], ["neck", 2.0]],
            "aim_joint": "head", "target": { "Param": "Look" }, "passes": 3
        } }),
    )["node"]
        .as_u64()
        .expect("aim id");
    h(
        &mut world,
        "editor.anim_connect",
        json!({ "child": clip, "parent": aim, "slot": "inner" }),
    );
    h(&mut world, "editor.anim_connect", json!({ "child": aim }));
    let AnimNode::BodyAim(node) = compiled(&world).root else {
        panic!("body aim root");
    };
    assert_eq!(node.joints.len(), 3);
    assert_eq!(node.aim_joint, "head");
    assert_eq!(node.passes, 3);
    assert_eq!(
        node.axis,
        regia_core::math::Vec3::NEG_Z,
        "the default axis is forward"
    );

    // A solver name the format does not know is refused with the node unchanged.
    assert!(
        call(
            &mut world,
            "editor.anim_add_node",
            json!({ "kind": "chain_ik", "data": { "solver": "Jacobian" } }),
        )
        .is_err()
    );
    assert!(NodeKind::LABELS.contains(&"chain_ik") && NodeKind::LABELS.contains(&"body_aim"));
    for label in ["chain_ik", "body_aim"] {
        let kind = NodeKind::default_of(label).expect("default node");
        assert_eq!(kind.label(), label);
        assert_eq!(kind.input_names(), ["inner"]);
    }
}
