// SPDX-License-Identifier: MPL-2.0
//! The contracts the book and format pages state about surface outputs and the compile cache.

use std::collections::BTreeMap;

use regia_matgraph::{MaterialGraph, NormalSpace, graph_hash, permutation_hash};

const NORMAL_SPACE_IN_OUTPUT: &str = r#"(
    format_version: 2,
    name: "n",
    nodes: { "n": Vec3((0.0, 0.0, 1.0)) },
    output: (normal: Some("n"), normal_space: World),
)"#;

const NORMAL_SPACE_IN_SHADING: &str = r#"(
    format_version: 2,
    name: "n",
    nodes: { "n": Vec3((0.0, 0.0, 1.0)) },
    output: (normal: Some("n")),
    shading: (normal_space: World),
)"#;

const FLOAT_NORMAL: &str = r#"(
    format_version: 2,
    name: "n",
    nodes: { "n": Float(1.0) },
    output: (normal: Some("n")),
)"#;

#[test]
fn normal_space_is_an_output_field_not_a_shading_field() {
    let g = MaterialGraph::from_ron(NORMAL_SPACE_IN_OUTPUT).expect("output.normal_space parses");
    assert_eq!(g.output.normal_space, NormalSpace::World);
    assert!(MaterialGraph::from_ron(NORMAL_SPACE_IN_SHADING).is_err());
}

#[test]
fn normal_output_takes_a_vec3_not_a_float() {
    let g = MaterialGraph::from_ron(FLOAT_NORMAL).expect("parses");
    assert!(regia_matgraph::compile_unvalidated(&g).is_err());
}

#[test]
fn graph_hash_is_the_default_switch_permutation() {
    let g = MaterialGraph::from_ron(NORMAL_SPACE_IN_OUTPUT).expect("parses");
    assert_eq!(graph_hash(&g), permutation_hash(&g, &BTreeMap::new()));
}
