// SPDX-License-Identifier: MPL-2.0
//! The anim graph editor authors the v4 locomotion warp nodes (ADR-8540): `stride_warp` and
//! `orientation_warp` are node kinds of `editor.anim_add_node`, their settings edit through
//! `editor.anim_set_node`, they compile into the saved `.animgraph.ron`, and every step undoes.
//! No window.

use regia_animation::graph::{AnimGraph, AnimNode};
use regia_animation::motion_matching::warp::StrideMode;
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
fn editor_creates_and_undoes_warp_nodes() {
    let mut world = handler_world();
    h(&mut world, "editor.anim_open", json!({ "create": true }));
    h(&mut world, "editor.anim_rig", json!({ "source": "demo" }));
    for name in ["GroundSpeed", "Heading"] {
        h(
            &mut world,
            "editor.anim_param",
            json!({ "name": name, "kind": "float" }),
        );
    }
    let clip = h(
        &mut world,
        "editor.anim_add_node",
        json!({ "kind": "clip", "data": { "clip": "Idle" } }),
    )["node"]
        .as_u64()
        .expect("clip id");
    let nodes_before = world.resource::<AnimEditor>().docs.graph.nodes.len();
    let stride = h(
        &mut world,
        "editor.anim_add_node",
        json!({ "kind": "stride_warp", "data": {
            "ground_speed_param": "GroundSpeed",
            "settings": { "strength": 1.0, "min_speed": 0.1, "ratio_min": 0.8, "ratio_max": 1.4,
                          "response": 12.0, "mode": "Pose" },
            "legs": [["left_thigh", "left_calf", "left_foot"], ["right_thigh", "right_calf", "right_foot"]],
            "pelvis": "hips"
        } }),
    )["node"]
        .as_u64()
        .expect("stride id");
    let orient = h(
        &mut world,
        "editor.anim_add_node",
        json!({ "kind": "orientation_warp", "data": {
            "heading_param": "Heading", "pelvis": "hips",
            "spine": [["spine", 0.5], ["chest", 0.5]], "max_delta": 1.0
        } }),
    )["node"]
        .as_u64()
        .expect("orientation id");
    h(
        &mut world,
        "editor.anim_connect",
        json!({ "child": clip, "parent": stride, "slot": "inner" }),
    );
    h(
        &mut world,
        "editor.anim_connect",
        json!({ "child": stride, "parent": orient, "slot": "inner" }),
    );
    h(&mut world, "editor.anim_connect", json!({ "child": orient }));

    let graph = compiled(&world);
    let AnimNode::OrientationWarp(o) = &graph.root else {
        panic!("the output is not an orientation warp: {:?}", graph.root);
    };
    assert_eq!(o.heading_param, "Heading");
    assert_eq!(o.spine.len(), 2);
    assert_eq!((o.strength, o.max_delta), (1.0, 1.0));
    let AnimNode::StrideWarp(s) = o.inner.as_ref() else {
        panic!("stride warp inner");
    };
    assert_eq!(s.settings.mode, StrideMode::Pose);
    assert_eq!(s.settings.ratio_max, 1.4);
    assert_eq!(s.legs.len(), 2);
    assert_eq!(s.pelvis, "hips");
    let ron = h(&mut world, "editor.anim_graph", json!({ "ron": true }))["ron"]
        .as_str()
        .map(str::to_owned)
        .expect("ron");
    assert!(ron.contains("format_version: 4"), "{ron}");
    assert_eq!(
        AnimGraph::from_ron_str(&ron).expect("the saved graph loads"),
        graph
    );

    let listed = h(&mut world, "editor.anim_graph", json!({}));
    let kind_of = |id: u64| {
        listed["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|n| n["id"].as_u64() == Some(id)))
            .map(|n| n["kind"].clone())
    };
    assert_eq!(kind_of(stride), Some(json!("stride_warp")));
    assert_eq!(kind_of(orient), Some(json!("orientation_warp")));

    // Switch the stride warp to Rate through anim_set_node, then undo it.
    h(
        &mut world,
        "editor.anim_set_node",
        json!({ "node": stride, "data": { "settings": { "strength": 0.5, "min_speed": 0.1,
            "ratio_min": 0.8, "ratio_max": 1.4, "response": 12.0, "mode": "Rate" } } }),
    );
    let stride_of = |g: &AnimGraph| match &g.root {
        AnimNode::OrientationWarp(o) => match o.inner.as_ref() {
            AnimNode::StrideWarp(s) => s.settings,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let edited = stride_of(&compiled(&world));
    assert_eq!((edited.mode, edited.strength), (StrideMode::Rate, 0.5));
    assert!(undo(&mut world), "the edit undoes");
    assert_eq!(stride_of(&compiled(&world)).mode, StrideMode::Pose);

    // Undo the three connections and both creations: the nodes are gone again.
    for _ in 0..5 {
        assert!(undo(&mut world), "a step undoes");
    }
    let editor = world.resource::<AnimEditor>();
    assert!(!editor.docs.graph.nodes.contains_key(&(stride as u32)));
    assert!(!editor.docs.graph.nodes.contains_key(&(orient as u32)));
    assert_eq!(editor.docs.graph.nodes.len(), nodes_before);
}

#[test]
fn warp_kinds_are_listed_and_bad_data_is_refused() {
    let mut world = handler_world();
    h(&mut world, "editor.anim_open", json!({ "create": true }));
    assert!(NodeKind::LABELS.contains(&"stride_warp") && NodeKind::LABELS.contains(&"orientation_warp"));
    for label in ["stride_warp", "orientation_warp"] {
        let kind = NodeKind::default_of(label).expect("default node");
        assert_eq!(kind.label(), label);
        assert_eq!(kind.input_names(), ["inner"]);
    }
    // A stride mode the format does not know is refused.
    assert!(
        call(
            &mut world,
            "editor.anim_add_node",
            json!({ "kind": "stride_warp", "data": { "settings": { "strength": 1.0, "min_speed": 0.1,
                "ratio_min": 0.8, "ratio_max": 1.4, "response": 12.0, "mode": "Teleport" } } }),
        )
        .is_err()
    );
}
