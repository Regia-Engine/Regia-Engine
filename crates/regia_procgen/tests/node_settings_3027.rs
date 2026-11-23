// SPDX-License-Identifier: MPL-2.0
//! Spec 63 node-setting remainders (ADR-3027): `graph.call graph_from`, `points.project` onto
//! splines, polygons and point boxes, `points.align keep_yaw`, `attr.lookup nearest_within`.

mod common;

use std::collections::BTreeMap;

use common::*;
use regia_procgen::graph::{GraphPin, collect_subgraphs};
use regia_procgen::{
    AttrColumn, CancelToken, DataKind, Evaluator, KindSet, ProcGraph, ProcNodeRegistry, RunError, RunInputs,
    RunResult,
};

fn pins(names: &[&str]) -> Vec<GraphPin> {
    names
        .iter()
        .map(|n| GraphPin {
            name: n.to_string(),
            kinds: KindSet::of(&[DataKind::Points]),
        })
        .collect()
}

fn snap() -> std::sync::Arc<regia_procgen::StaticSnapshot> {
    snapshot(box_shape([0.0; 3], [50.0; 3], 0.0), None)
}

fn run_root(root: &ProcGraph, files: &BTreeMap<String, String>) -> Result<RunResult, RunError> {
    let load = |p: &str| files.get(p).cloned();
    let reg = ProcNodeRegistry::with_builtins();
    let c = match root.compile_full(&reg, &BTreeMap::new(), &load) {
        Ok(c) => c,
        Err(e) => panic!("compile: {:?}", e.0),
    };
    let graphs = collect_subgraphs(root, &load);
    let inputs = RunInputs::new(snap(), 4).with_graphs(graphs);
    Evaluator::new().run(&c, &inputs, &CancelToken::new())
}

/// A body that marks each point with `which = mark` (and declares `extra` when asked).
fn body(mark: &str, extra_pin: bool) -> String {
    let mut g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("attr.set", &[("target", "\"which\""), ("value", mark)])),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    g.inputs = pins(if extra_pin { &["in", "extra"] } else { &["in"] });
    g.outputs = pins(&["out"]);
    g.to_ron()
}

fn files() -> BTreeMap<String, String> {
    [
        ("a.pgraph.ron".to_string(), body("1.0", false)),
        ("b.pgraph.ron".to_string(), body("2.0", false)),
        ("c.pgraph.ron".to_string(), body("3.0", true)),
    ]
    .into()
}

/// Three items: x = 1 (no scope value), x = 2 and x = 3 (both naming `target`).
fn caller(target: &str, graphs: &str) -> ProcGraph {
    let name = format!("\"{target}\"");
    graph(
        vec![
            (10, node("gen.points", &[("points", "[((1.0, 0.0, 0.0))]")])),
            (11, node("gen.points", &[("points", "[((2.0, 0.0, 0.0))]")])),
            (12, node("gen.points", &[("points", "[((3.0, 0.0, 0.0))]")])),
            (
                13,
                node("attr.set", &[("target", "\"item.graph\""), ("value", &name)]),
            ),
            (
                14,
                node("attr.set", &[("target", "\"item.graph\""), ("value", &name)]),
            ),
            (15, node("items.gather", &[])),
            (
                20,
                node(
                    "graph.call",
                    &[
                        ("graph", "\"a.pgraph.ron\""),
                        ("graph_from", "\"item.graph\""),
                        ("graphs", graphs),
                    ],
                ),
            ),
            (30, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[
            (11, "out", 13, "in"),
            (12, "out", 14, "in"),
            (10, "out", 15, "in"),
            (13, "out", 15, "in"),
            (14, "out", 15, "in"),
            (15, "out", 20, "in"),
            (20, "out", 30, "in"),
        ],
    )
}

fn which_by_x(r: &RunResult) -> Vec<(f32, f64)> {
    let mut out = Vec::new();
    for item in r
        .output
        .graph_outputs
        .get("out")
        .map(|b| b.0.clone())
        .unwrap_or_default()
    {
        let Some(p) = item.as_points() else {
            continue;
        };
        let which = match p.attrs.get("which") {
            Some(AttrColumn::Float(v)) => v.to_vec(),
            _ => Vec::new(),
        };
        for (i, w) in which.iter().enumerate() {
            out.push((p.position[i][0], *w));
        }
    }
    out
}

#[test]
fn graph_from_runs_the_graph_each_item_names_once_per_group() {
    let root = caller("b.pgraph.ron", "[\"b.pgraph.ron\"]");
    let captured = collect_subgraphs(&root, &|p: &str| files().get(p).cloned());
    assert!(captured.contains_key("b.pgraph.ron"), "the candidate is captured");
    assert!(!captured.contains_key("c.pgraph.ron"));
    let r = match run_root(&root, &files()) {
        Ok(r) => r,
        Err(e) => panic!("run: {e}"),
    };
    // Groups in first-appearance order: the item without a value runs `a`, then both `b` items.
    assert_eq!(which_by_x(&r), vec![(1.0, 1.0), (2.0, 2.0), (3.0, 2.0)]);
}

#[test]
fn graph_from_refuses_an_uncaptured_graph_and_a_pin_mismatch() {
    // `b` named by the items but not listed in `graphs`: not captured.
    let root = caller("b.pgraph.ron", "[]");
    match run_root(&root, &files()) {
        Err(RunError::Node { code, .. }) => assert_eq!(code, "unknown_graph"),
        other => panic!("expected unknown_graph, got {:?}", other.map(|_| ())),
    }
    // `c` declares another input pin than `a`.
    let root = caller("c.pgraph.ron", "[\"c.pgraph.ron\"]");
    match run_root(&root, &files()) {
        Err(RunError::Node { code, .. }) => assert_eq!(code, "call_pins_mismatch"),
        other => panic!("expected call_pins_mismatch, got {:?}", other.map(|_| ())),
    }
}

/// The `graphs` list is the node's own allow-list: a path captured for another node must not
/// run here (the setting doc: "any other path fails with unknown_graph").
#[test]
fn graph_from_refuses_a_graph_only_another_node_captured() {
    let root = graph(
        vec![
            (10, node("gen.points", &[("points", "[((1.0, 0.0, 0.0))]")])),
            (
                13,
                node(
                    "attr.set",
                    &[("target", "\"item.graph\""), ("value", "\"b.pgraph.ron\"")],
                ),
            ),
            (15, node("items.gather", &[])),
            (
                20,
                node(
                    "graph.call",
                    &[
                        ("graph", "\"a.pgraph.ron\""),
                        ("graph_from", "\"item.graph\""),
                        ("graphs", "[\"b.pgraph.ron\"]"),
                    ],
                ),
            ),
            (
                21,
                node(
                    "graph.call",
                    &[
                        ("graph", "\"a.pgraph.ron\""),
                        ("graph_from", "\"item.graph\""),
                        ("graphs", "[]"),
                    ],
                ),
            ),
            (30, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[
            (10, "out", 13, "in"),
            (13, "out", 15, "in"),
            (15, "out", 20, "in"),
            (15, "out", 21, "in"),
            (20, "out", 30, "in"),
        ],
    );
    // Node 20's `graphs` captures `b` for the run; node 21's items name it without listing it.
    let captured = collect_subgraphs(&root, &|p: &str| files().get(p).cloned());
    assert!(captured.contains_key("b.pgraph.ron"), "node 20 captured b");
    match run_root(&root, &files()) {
        Err(RunError::Node { node, code, .. }) => {
            assert_eq!(code, "unknown_graph", "node 21 must not run b");
            assert_eq!(node, 21);
        }
        other => panic!("expected unknown_graph from node 21, got {:?}", other.map(|_| ())),
    }
}

fn project_graph(
    onto: Vec<(u32, regia_procgen::NodeEntry)>,
    onto_out: u32,
    settings: &[(&str, &str)],
) -> ProcGraph {
    let mut nodes = vec![
        (
            1,
            node(
                "gen.points",
                &[(
                    "points",
                    "[((2.0, 9.0, 3.0)), ((-30.0, 9.0, -30.0)), ((5.0, 7.0, 0.5))]",
                )],
            ),
        ),
        (2, node("points.project", settings)),
        (3, node("out.points", &[("name", "\"out\"")])),
    ];
    nodes.extend(onto);
    graph(
        nodes,
        &[
            (1, "out", 2, "in"),
            (onto_out, "out", 2, "onto"),
            (2, "out", 3, "in"),
        ],
    )
}

#[test]
fn project_onto_a_polygon_takes_its_plane_and_drops_points_outside() {
    let square = vec![
        (
            40,
            node(
                "gen.points",
                &[(
                    "points",
                    "[((-10.0, 5.0, -10.0)), ((10.0, 5.0, -10.0)), ((10.0, 5.0, 10.0)), ((-10.0, 5.0, 10.0))]",
                )],
            ),
        ),
        (41, node("polygon.from", &[("mode", "hull")])),
    ];
    let mut g = project_graph(square.clone(), 41, &[]);
    g.edges.push(regia_procgen::Edge::new(40, "out", 41, "in"));
    let p = exported(&run(&g, snap(), 1), "out");
    let pos: Vec<[f32; 3]> = p.position.to_vec();
    assert_eq!(
        pos,
        vec![[2.0, 5.0, 3.0], [5.0, 5.0, 0.5]],
        "inside points on the plane, outside dropped"
    );

    let mut g = project_graph(square, 41, &[("keep_outside", "true")]);
    g.edges.push(regia_procgen::Edge::new(40, "out", 41, "in"));
    let p = exported(&run(&g, snap(), 1), "out");
    assert_eq!(p.position[1], [-30.0, 9.0, -30.0], "kept outside, unmoved");
}

#[test]
fn project_onto_a_spline_moves_to_the_nearest_curve_point() {
    let line = vec![
        (
            40,
            node(
                "gen.points",
                &[("points", "[((-20.0, 2.0, 0.0)), ((20.0, 2.0, 0.0))]")],
            ),
        ),
        (41, node("spline.from_points", &[])),
    ];
    let mut g = project_graph(line, 41, &[]);
    g.edges.push(regia_procgen::Edge::new(40, "out", 41, "in"));
    let p = exported(&run(&g, snap(), 1), "out");
    assert_eq!(p.len(), 3, "a spline keeps every point");
    let near = |a: [f32; 3], b: [f32; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3);
    for (i, want) in [[2.0, 2.0, 0.0], [-20.0, 2.0, 0.0], [5.0, 2.0, 0.0]]
        .into_iter()
        .enumerate()
    {
        assert!(
            near(p.position[i], want),
            "point {i}: {:?} vs {want:?}",
            p.position[i]
        );
    }
}

#[test]
fn project_onto_point_boxes_lands_on_the_highest_box_top() {
    // Two unit boxes under (2, _, 3): tops at 0.5 and 3.5; nothing under the other two points.
    let boxes = vec![(
        40,
        node(
            "gen.points",
            &[(
                "points",
                "[((2.0, 0.0, 3.0)), ((2.2, 3.0, 3.1)), ((40.0, 0.0, 40.0))]",
            )],
        ),
    )];
    let p = exported(&run(&project_graph(boxes, 40, &[]), snap(), 1), "out");
    assert_eq!(p.position.to_vec(), vec![[2.0, 3.5, 3.0]]);
}

fn align_graph(keep_yaw: &str) -> ProcGraph {
    graph(
        vec![
            (
                1,
                node(
                    "gen.points",
                    &[(
                        "points",
                        "[(position: (0.0, 0.0, 0.0), rotation_deg: (0.0, 90.0, 0.0))]",
                    )],
                ),
            ),
            (2, node("points.align", &[("to", "up"), ("keep_yaw", keep_yaw)])),
            (3, node("out.points", &[("name", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    )
}

#[test]
fn align_keep_yaw_keeps_or_drops_the_heading() {
    let yawed = exported(&run(&align_graph("true"), snap(), 1), "out").rotation[0];
    let plain = exported(&run(&align_graph("false"), snap(), 1), "out").rotation[0];
    // Already upright: keeping the yaw leaves the 90 degree turn, dropping it gives no rotation.
    assert!(yawed[1].abs() > 0.5, "the yaw is kept: {yawed:?}");
    let identity = [0.0, 0.0, 0.0, 1.0];
    assert!(
        plain.iter().zip(identity).all(|(a, b)| (a - b).abs() < 1e-5),
        "the yaw is dropped: {plain:?}"
    );
}

fn lookup_graph(within: &str) -> ProcGraph {
    graph(
        vec![
            (
                1,
                node(
                    "gen.points",
                    &[(
                        "points",
                        "[((9.0, 0.0, 0.0)), ((15.0, 0.0, 0.0)), ((20.0, 0.0, 0.0))]",
                    )],
                ),
            ),
            (
                2,
                node("attr.set", &[("target", "\"h\""), ("from", "\"position:x\"")]),
            ),
            (
                3,
                node(
                    "table.make",
                    &[(
                        "columns",
                        "[(\"h\", Float, [0.0, 10.0, 20.0]), (\"v\", Float, [1.0, 2.0, 3.0])]",
                    )],
                ),
            ),
            (
                4,
                node(
                    "attr.lookup",
                    &[
                        ("keys", "[(\"h\", \"h\")]"),
                        ("columns", "[\"v\"]"),
                        ("on_miss", "default"),
                        ("nearest_within", within),
                    ],
                ),
            ),
            (5, node("out.points", &[("name", "\"out\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 4, "in"),
            (3, "out", 4, "table"),
            (4, "out", 5, "in"),
        ],
    )
}

fn v_column(within: &str) -> Vec<f64> {
    match exported(&run(&lookup_graph(within), snap(), 1), "out")
        .attrs
        .get("v")
    {
        Some(AttrColumn::Float(v)) => v.to_vec(),
        other => panic!("no v column: {other:?}"),
    }
}

#[test]
fn lookup_nearest_within_matches_the_closest_row_in_range() {
    // Exact: only h = 20 matches.
    assert_eq!(v_column("-1.0"), vec![0.0, 0.0, 3.0]);
    // Within 2: 9 → row 10, 15 is 5 from both neighbors (miss), 20 exact.
    assert_eq!(v_column("2.0"), vec![2.0, 0.0, 3.0]);
    // Within 5: 15 is equally close to 10 and 20; the lowest row wins (`multi: first`).
    assert_eq!(v_column("5.0"), vec![2.0, 2.0, 3.0]);
}

#[test]
fn project_onto_points_copies_attributes_color_and_tags_from_the_target() {
    let settings = [
        ("attributes", "all"),
        ("color", "mul"),
        ("tags", "both"),
        ("keep_outside", "true"),
    ];
    let mut nodes = vec![
        (
            1,
            node(
                "gen.points",
                &[("points", "[((2.0, 9.0, 3.0)), ((40.0, 9.0, -40.0))]")],
            ),
        ),
        (5, node("items.tag", &[("add", "[\"src\"]")])),
        (2, node("points.project", &settings)),
        (3, node("out.points", &[("name", "\"out\"")])),
        (4, node("graph.output", &[("pin", "\"items\"")])),
        (40, node("gen.points", &[("points", "[((2.0, 0.0, 3.0))]")])),
        (
            41,
            node("attr.set", &[("target", "\"surface\""), ("value", "7.0")]),
        ),
        (
            42,
            node(
                "attr.set",
                &[("target", "\"color\""), ("value", "(0.25, 0.5, 1.0, 1.0)")],
            ),
        ),
        (43, node("items.tag", &[("add", "[\"ground\"]")])),
    ];
    nodes.sort_by_key(|n| n.0);
    let g = graph(
        nodes,
        &[
            (1, "out", 5, "in"),
            (5, "out", 2, "in"),
            (40, "out", 41, "in"),
            (41, "out", 42, "in"),
            (42, "out", 43, "in"),
            (43, "out", 2, "onto"),
            (2, "out", 3, "in"),
            (2, "out", 4, "in"),
        ],
    );
    let r = run(&g, snap(), 1);
    let item = r.output.points.get("out").cloned().unwrap_or_default();
    assert_eq!(item.len(), 2);
    assert_eq!(item.position[0], [2.0, 0.5, 3.0], "onto the box top");
    assert_eq!(item.position[1], [40.0, 9.0, -40.0], "kept outside, unmoved");
    match item.attrs.get("surface") {
        Some(AttrColumn::Float(v)) => {
            assert_eq!(v.to_vec(), vec![7.0, 0.0], "target value, else the default")
        }
        other => panic!("no surface column: {other:?}"),
    }
    assert_eq!(
        item.color[0],
        [0.25, 0.5, 1.0, 1.0],
        "white times the target color"
    );
    assert_eq!(item.color[1], [1.0; 4], "no target: unchanged");
    let tags: Vec<String> = r
        .output
        .graph_outputs
        .get("items")
        .and_then(|b| b.0.first())
        .map(|i| i.tags.iter().cloned().collect())
        .unwrap_or_default();
    assert_eq!(tags, ["ground", "src"], "the source's tags plus the target's");
}

/// The points `out.points { name }` exported.
fn exported(r: &regia_procgen::RunResult, name: &str) -> regia_procgen::PointSet {
    r.output.points.get(name).cloned().unwrap_or_default()
}
