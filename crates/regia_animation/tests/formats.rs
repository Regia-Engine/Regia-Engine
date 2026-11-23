// SPDX-License-Identifier: MPL-2.0
//! Frozen-format fixture for `.animgraph.ron`: the checked-in `locomotion.animgraph.ron` from
//! `docs/formats/` (copied into `fixtures/`) must keep loading, and load → save → load is equal
//! (`docs/specs/12-assets.md` § 4a).

use regia_animation::{AnimGraph, AnimGraphError, animgraph_schema_fingerprint};

/// The checked-in `.animgraph.ron` v1 fixture still loads.
#[test]
fn animgraph_v1_fixture_still_loads() {
    let graph =
        AnimGraph::from_ron_str(include_str!("fixtures/locomotion.animgraph.ron")).expect("v1 animgraph");
    assert_eq!(graph.parameters.len(), 9);
}

/// `.animgraph.ron` round trips: load → save → load equal (the graph re-triangulates 2D blend
/// spaces, so compare the parsed graph against the reparsed one).
#[test]
fn animgraph_round_trips() {
    let graph = AnimGraph::from_ron_str(include_str!("fixtures/locomotion.animgraph.ron")).expect("load");
    let text = graph.to_ron_string().expect("save");
    let back = AnimGraph::from_ron_str(&text).expect("reload");
    assert_eq!(back, graph);
}

/// A newer `.animgraph.ron` version is rejected with a clear error.
#[test]
fn animgraph_rejects_newer_version() {
    let newer = include_str!("fixtures/locomotion.animgraph.ron").replacen(
        "format_version: 1",
        "format_version: 99",
        1,
    );
    let err = AnimGraph::from_ron_str(&newer).expect_err("v99 must be rejected");
    assert!(err.to_string().contains("99"), "{err}");
}

/// A newer `.animgraph.ron` version whose body no longer deserialises (the required `root` field is
/// absent) is still a *version* error, not a RON "missing field" error.
#[test]
fn animgraph_rejects_newer_version_even_when_body_changed() {
    let err =
        AnimGraph::from_ron_str("(format_version: 99, parameters: [])").expect_err("v99 must be rejected");
    assert!(
        matches!(err, AnimGraphError::UnknownVersion(99)),
        "expected a version error, got: {err}"
    );
}

/// The animgraph schema fingerprint hashes *nested* field names (not just the root struct's).
#[test]
fn animgraph_schema_fingerprint_hashes_nested_fields() {
    let fp = animgraph_schema_fingerprint();
    assert!(fp.contains("format_version"), "{fp}");
    assert!(fp.contains("root"), "{fp}");
    // `clip` lives on the nested `ClipNode`; `weight_param` on `LayerEntry`.
    assert!(fp.contains("clip"), "nested ClipNode not hashed: {fp}");
    assert!(fp.contains("weight_param"), "nested LayerEntry not hashed: {fp}");
}
