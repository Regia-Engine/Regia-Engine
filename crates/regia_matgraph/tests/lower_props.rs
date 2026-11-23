// SPDX-License-Identifier: MPL-2.0
//! Deterministic lower/compile edge cases built with the crate's graph builder.
//!
//! `MaterialGraph` stores node IDs as names in a `BTreeMap`; IDs are not content-hashed and
//! appear in generated WGSL. This checks equivalent construction orders with stable IDs.

use regia_matgraph::{MATGRAPH_FORMAT_VERSION, MaterialGraph, Node, compile_unvalidated};

/// Builds the same small graph with a fixed-seed permutation of insertion order.
fn graph(seed: u64) -> MaterialGraph {
    let mut graph = MaterialGraph::new("deterministic");
    graph.format_version = MATGRAPH_FORMAT_VERSION;
    let nodes = [
        ("source", Node::Vec3([0.25, 0.5, 0.75])),
        ("scale", Node::Float(0.5)),
        ("scaled", Node::Mul("source".into(), "scale".into())),
    ];
    let mut order = [0, 1, 2];
    let mut state = seed;
    for i in (1..order.len()).rev() {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        order.swap(i, (state as usize) % (i + 1));
    }
    for index in order {
        let (name, node) = &nodes[index];
        graph.nodes.insert((*name).into(), node.clone());
    }
    graph.output.emissive = Some("scaled".into());
    graph
}

#[test]
fn construction_orders_compile_to_identical_bytes() {
    let first = compile_unvalidated(&graph(0x5eed)).expect("first compilation");
    let second = compile_unvalidated(&graph(0xcafe)).expect("second compilation");
    assert_eq!(first.wgsl.as_bytes(), second.wgsl.as_bytes());
    assert_eq!(first.vertex_wgsl, second.vertex_wgsl);
    assert_eq!(first.key, second.key);
}

#[test]
fn a_node_dependency_cycle_is_an_error() {
    let mut graph = MaterialGraph::new("cycle");
    graph.nodes.insert("a".into(), Node::Add("b".into(), "b".into()));
    graph.nodes.insert("b".into(), Node::Mul("a".into(), "a".into()));
    graph.output.roughness = Some("a".into());
    let result = compile_unvalidated(&graph);
    assert!(result.is_err(), "cyclic dependency must not compile");
}

#[test]
fn an_unconnected_required_input_is_an_error() {
    let mut graph = MaterialGraph::new("unconnected");
    graph
        .nodes
        .insert("required".into(), Node::Add(String::new(), "also_missing".into()));
    graph.output.roughness = Some("required".into());
    let errors = compile_unvalidated(&graph).expect_err("unconnected input").0;
    assert!(
        errors.iter().any(|error| {
            error.node.as_deref() == Some("required") && error.message.contains("not connected")
        }),
        "{errors:?}"
    );
}
