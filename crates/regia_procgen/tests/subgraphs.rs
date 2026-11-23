// SPDX-License-Identifier: MPL-2.0
//! Spec 63 § 14 cases 9 and 10: `graph.loop` with a carry pin, recursive `graph.call`, the call
//! depth limit.

mod common;

use std::collections::BTreeMap;

use common::{box_shape, graph, node, snapshot};
use regia_procgen::graph::{GraphPin, ParamDecl, collect_subgraphs, collect_subgraphs_with};
use regia_procgen::{
    AttrType, CancelToken, DataKind, Evaluator, KindSet, ProcGraph, ProcNodeRegistry, RunError, RunInputs,
    RunLimits, RunResult, SettingValue,
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

fn run_root(root: &ProcGraph, files: &BTreeMap<String, String>) -> Result<RunResult, RunError> {
    let load = |p: &str| files.get(p).cloned();
    let reg = ProcNodeRegistry::with_builtins();
    let c = root.compile_full(&reg, &BTreeMap::new(), &load).expect("compile");
    let graphs = collect_subgraphs(root, &load);
    let inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 4).with_graphs(graphs);
    Evaluator::new().run(&c, &inputs, &CancelToken::new())
}

#[test]
fn case_09_loop_runs_once_per_item_and_carries_its_accumulator() {
    // Body: acc' = acc + one point; seen = the iteration's item; idx = the loop index as a point x.
    let mut body = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"acc\"")])),
            (2, node("gen.one", &[])),
            (3, node("points.merge", &[])),
            (4, node("graph.output", &[("pin", "\"acc\"")])),
            (5, node("graph.input", &[("pin", "\"each\"")])),
            (6, node("graph.output", &[("pin", "\"seen\"")])),
        ],
        &[
            (1, "out", 3, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (5, "out", 6, "in"),
        ],
    );
    body.inputs = pins(&["each", "acc"]);
    body.outputs = pins(&["acc", "seen"]);
    let files: BTreeMap<String, String> = [("body.pgraph.ron".to_string(), body.to_ron())].into();

    // Caller: three items on `each` (gathered from three literal sets), five points on `acc`.
    let root = graph(
        vec![
            (10, node("gen.points", &[("points", "[((1.0, 0.0, 0.0))]")])),
            (11, node("gen.points", &[("points", "[((2.0, 0.0, 0.0))]")])),
            (12, node("gen.points", &[("points", "[((3.0, 0.0, 0.0))]")])),
            (13, node("items.gather", &[])),
            (14, node("gen.grid", &[("count", "(5.0, 1.0, 1.0)")])),
            (
                20,
                node(
                    "graph.loop",
                    &[
                        ("graph", "\"body.pgraph.ron\""),
                        ("each", "[\"each\"]"),
                        ("carry", "[\"acc\"]"),
                    ],
                ),
            ),
            (30, node("graph.output", &[("pin", "\"acc\"")])),
            (31, node("graph.output", &[("pin", "\"seen\"")])),
        ],
        &[
            (10, "out", 13, "in"),
            (11, "out", 13, "in"),
            (12, "out", 13, "in"),
            (13, "out", 20, "each"),
            (14, "out", 20, "acc"),
            (20, "acc", 30, "in"),
            (20, "seen", 31, "in"),
        ],
    );
    let r = run_root(&root, &files).expect("run");
    let acc = &r.output.graph_outputs["acc"];
    assert_eq!(
        acc.elements(),
        5 + 3,
        "carry starts with the caller's 5 points and gains one per iteration"
    );
    let seen = &r.output.graph_outputs["seen"];
    assert_eq!(seen.0.len(), 3, "the body ran 3 times");
    let xs: Vec<f32> = seen
        .0
        .iter()
        .map(|i| i.as_points().expect("points").position[0][0])
        .collect();
    assert_eq!(xs, vec![1.0, 2.0, 3.0], "item k on iteration k, in order");
}

fn halving(name: &str, keep: &str) -> String {
    let mut g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("points.reseed", &[])),
            (3, node("points.keep_random", &[("ratio", keep)])),
            (4, node("graph.call", &[("graph", &format!("\"{name}\""))])),
            (5, node("graph.depth", &[])),
            (6, node("graph.output", &[("pin", "\"out\"")])),
            (7, node("out.table", &[("name", "\"depth\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "out", 6, "in"),
            (5, "out", 7, "in"),
        ],
    );
    g.inputs = pins(&["in"]);
    g.outputs = pins(&["out"]);
    g.to_ron()
}

fn caller(path: &str) -> ProcGraph {
    graph(
        vec![
            (1, node("gen.grid", &[("count", "(40.0, 1.0, 40.0)")])),
            (2, node("graph.call", &[("graph", &format!("\"{path}\""))])),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    )
}

#[test]
fn case_10_recursive_call_terminates_and_depth_33_fails() {
    let files: BTreeMap<String, String> = [
        ("halve.pgraph.ron".to_string(), halving("halve.pgraph.ron", "0.5")),
        (
            "forever.pgraph.ron".to_string(),
            halving("forever.pgraph.ron", "1.0"),
        ),
    ]
    .into();
    let r = run_root(&caller("halve.pgraph.ron"), &files).expect("terminates");
    assert_eq!(r.output.graph_outputs["out"].elements(), 0, "halved until empty");
    let lines: Vec<(u32, String, usize)> = r
        .report
        .nodes
        .iter()
        .map(|l| (l.id, l.ty.clone(), l.elements))
        .collect();
    let table = r.output.tables.get("depth").unwrap_or_else(|| {
        panic!(
            "no depth table; tables {:?}; report {lines:?}",
            r.output.tables.keys().collect::<Vec<_>>()
        )
    });
    let deepest = table.column("depth").and_then(|c| c.get(0));
    let depth = match deepest {
        Some(regia_procgen::AttrValue::Int(d)) => d,
        other => panic!("depth table: {other:?}"),
    };
    // Each level's `out.table` (node 7) runs after its nested call (node 4) returns and overwrites the
    // deeper levels' table, so the outermost level's value is published: depth 1.
    assert_eq!(depth, 1, "the outermost call level writes last");

    match run_root(&caller("forever.pgraph.ron"), &files) {
        Err(RunError::Node { code, message, .. }) => {
            assert_eq!(code, "limit_exceeded");
            assert!(message.contains("call_depth"), "{message}");
        }
        other => panic!("expected limit_exceeded, got {other:?}"),
    }
}

#[test]
fn loop_index_outside_a_loop_fails_and_unknown_called_graphs_do_not_compile() {
    let g = graph(vec![(1, node("graph.loop_index", &[]))], &[]);
    let c = g.compile(&ProcNodeRegistry::with_builtins()).expect("compile");
    let r = Evaluator::new().run(
        &c,
        &RunInputs::new(snapshot(box_shape([0.0; 3], [1.0; 3], 0.0), None), 1),
        &CancelToken::new(),
    );
    assert!(
        matches!(r, Err(RunError::Node { ref code, .. }) if code == "not_in_loop"),
        "{r:?}"
    );
    let e = caller("missing.pgraph.ron")
        .compile(&ProcNodeRegistry::with_builtins())
        .expect_err("unknown graph");
    assert!(e.has("procgen_unknown_graph"));
}

fn asset_param(name: &str, default: &str) -> ParamDecl {
    ParamDecl {
        name: name.to_string(),
        ty: AttrType::Asset,
        default: SettingValue::Str(default.to_string()),
        min: None,
        max: None,
        doc: String::new(),
    }
}

/// A subgraph that makes `n` points on its `out` pin.
fn maker(n: u32) -> ProcGraph {
    let mut g = graph(
        vec![
            (1, node("gen.grid", &[("count", &format!("({n}.0, 1.0, 1.0)"))])),
            (2, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in")],
    );
    g.outputs = pins(&["out"]);
    g
}

/// g1 M4: compile resolves `graph: Bind("body")`, so the capture must too, and a `Bind` inside the
/// callee gets the value its caller passes. It used to compile, then fail with `unknown_graph`.
#[test]
fn a_bound_call_path_is_captured_with_the_callees_it_reaches() {
    let mut middle = graph(
        vec![
            (1, node("graph.call", &[("graph", "Bind(\"inner\")")])),
            (2, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in")],
    );
    middle.params = vec![asset_param("inner", "default.pgraph.ron")];
    middle.outputs = pins(&["out"]);
    let files: BTreeMap<String, String> = [
        ("middle.pgraph.ron".to_string(), middle.to_ron()),
        ("maker.pgraph.ron".to_string(), maker(3).to_ron()),
        ("default.pgraph.ron".to_string(), maker(1).to_ron()),
    ]
    .into();
    let mut root = graph(
        vec![
            (
                1,
                node(
                    "graph.call",
                    &[
                        ("graph", "Bind(\"body\")"),
                        ("params", "{\"inner\": \"maker.pgraph.ron\"}"),
                    ],
                ),
            ),
            (2, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in")],
    );
    root.params = vec![asset_param("body", "unused.pgraph.ron")];
    let load = |p: &str| files.get(p).cloned();
    let values = BTreeMap::from([(
        "body".to_string(),
        SettingValue::Str("middle.pgraph.ron".to_string()),
    )]);
    let found = collect_subgraphs_with(&root, &values, &load);
    assert_eq!(
        found.keys().map(String::as_str).collect::<Vec<_>>(),
        ["maker.pgraph.ron", "middle.pgraph.ron"],
        "the bound path, and the callee its caller names, not the callee's default"
    );
    let c = root
        .compile_full(&ProcNodeRegistry::with_builtins(), &values, &load)
        .expect("compile");
    let inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 4).with_graphs(found);
    let r = Evaluator::new()
        .run(&c, &inputs, &CancelToken::new())
        .expect("run");
    assert_eq!(common::out_points(&r, "out").len(), 3);
}

/// g1 H2: every iteration is inside the per-item cap, and a child frame's budget starts at zero, so
/// only a running total stops the gather before the loop has built it.
#[test]
fn a_loop_gather_fails_at_the_iteration_that_passes_the_budget() {
    let mut body = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"each\"")])),
            (2, node("gen.grid", &[("count", "(100.0, 1.0, 1.0)")])),
            (3, node("graph.output", &[("pin", "\"made\"")])),
            (4, node("debug.check", &[])),
        ],
        &[(2, "out", 3, "in"), (1, "out", 4, "in")],
    );
    body.inputs = pins(&["each"]);
    body.outputs = pins(&["made"]);
    let files: BTreeMap<String, String> = [("body.pgraph.ron".to_string(), body.to_ron())].into();
    let root = graph(
        vec![
            (10, node("gen.points", &[("points", "[((1.0, 0.0, 0.0))]")])),
            (11, node("gen.points", &[("points", "[((2.0, 0.0, 0.0))]")])),
            (12, node("gen.points", &[("points", "[((3.0, 0.0, 0.0))]")])),
            (13, node("gen.points", &[("points", "[((4.0, 0.0, 0.0))]")])),
            (14, node("items.gather", &[])),
            (
                20,
                node(
                    "graph.loop",
                    &[("graph", "\"body.pgraph.ron\""), ("each", "[\"each\"]")],
                ),
            ),
            (30, node("graph.output", &[("pin", "\"made\"")])),
        ],
        &[
            (10, "out", 14, "in"),
            (11, "out", 14, "in"),
            (12, "out", 14, "in"),
            (13, "out", 14, "in"),
            (14, "out", 20, "each"),
            (20, "made", 30, "in"),
        ],
    );
    let load = |p: &str| files.get(p).cloned();
    let c = root
        .compile_full(&ProcNodeRegistry::with_builtins(), &BTreeMap::new(), &load)
        .expect("compile");
    let run = |total_elements: usize| {
        let mut inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 4)
            .with_graphs(collect_subgraphs(&root, &load));
        inputs.limits = RunLimits {
            total_elements,
            ..RunLimits::DESKTOP
        };
        Evaluator::new().run(&c, &inputs, &CancelToken::new())
    };
    assert!(run(1 << 20).is_ok());
    // 250 is under one iteration's total in the parent frame's sense, not under four gathered.
    match run(250) {
        Err(RunError::Node { node, code, message }) => {
            assert_eq!(code, "limit_exceeded", "{message}");
            assert_eq!(
                node, 20,
                "the loop node fails, not a later frame check: {message}"
            );
            assert!(
                message.contains("300 > 250"),
                "stopped at the third iteration: {message}"
            );
        }
        other => panic!("expected limit_exceeded, got {other:?}"),
    }
}

/// v0.5.1 (deep-zai-procgen-runtime finding 6): a called graph's road decals reach the caller's
/// output, as its instances and meshes do (they used to be dropped at the call).
#[test]
fn a_called_graph_keeps_its_road_decals() {
    use regia_procgen::exec::Frame;
    use regia_procgen::nodes::spline_data_item;
    use regia_scene::spline::{Spline, TangentMode};
    let profile = "RoadProfile(width_m: 6.0, shoulders_m: (0.5, 0.5), surface_lift_m: 0.05, \
        cross_slope_pct: 2.0, samples_per_m: 1.0, material: \"asphalt\", \
        junction: (margin_m: 1.0, material: Some(\"patch\")))";
    let mut body = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (2, node("road.decals", &[("profile", profile)])),
        ],
        &[(1, "out", 2, "splines")],
    );
    body.inputs = vec![GraphPin {
        name: "road".to_string(),
        kinds: KindSet::of(&[DataKind::Spline]),
    }];
    let files: BTreeMap<String, String> = [("decals.pgraph.ron".to_string(), body.to_ron())].into();
    let root = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (2, node("graph.call", &[("graph", "\"decals.pgraph.ron\"")])),
        ],
        &[(1, "out", 2, "road")],
    );
    let load = |p: &str| files.get(p).cloned();
    let reg = ProcNodeRegistry::with_builtins();
    let c = root.compile_full(&reg, &BTreeMap::new(), &load).expect("compile");
    let inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [100.0; 3], 0.0), None), 4)
        .with_graphs(collect_subgraphs(&root, &load));
    let mut s = Spline::through([
        regia_core::math::Vec3::ZERO,
        regia_core::math::Vec3::new(30.0, 0.0, 40.0),
    ]);
    s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let frame = Frame {
        graph_inputs: [(
            "road".to_string(),
            regia_procgen::Bundle::one(spline_data_item(s)),
        )]
        .into(),
        ..Frame::default()
    };
    let r = Evaluator::new()
        .run_frame(&c, &inputs, &CancelToken::new(), &frame)
        .expect("run");
    assert_eq!(
        r.output.road_decals.len(),
        1,
        "the called graph's decal segment is in the run's output"
    );
    assert_eq!(r.output.road_decals[0].b, [30.0, 40.0]);
}

/// v0.5.1 (deep-zai-procgen-runtime finding 7, decided as documented replace): a `graph.loop`
/// body that publishes `out.points "trees"` keeps one value per run, the last iteration's, while
/// its instances accumulate; the replacement is reported as `output_name_replaced` (spec 62 § 7.3).
#[test]
fn a_loop_body_publishing_a_name_keeps_the_last_write_and_warns() {
    let mut body = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"each\"")])),
            (2, node("out.points", &[("name", "\"trees\"")])),
            (3, node("spawn.mesh", &[("entries", "[(mesh: \"tree.glb\")]")])),
        ],
        &[(1, "out", 2, "in"), (1, "out", 3, "in")],
    );
    body.inputs = pins(&["each"]);
    let files: BTreeMap<String, String> = [("body.pgraph.ron".to_string(), body.to_ron())].into();
    let root = graph(
        vec![
            (10, node("gen.points", &[("points", "[((1.0, 0.0, 0.0))]")])),
            (11, node("gen.points", &[("points", "[((2.0, 0.0, 0.0))]")])),
            (12, node("gen.points", &[("points", "[((3.0, 0.0, 0.0))]")])),
            (13, node("items.gather", &[])),
            (
                20,
                node(
                    "graph.loop",
                    &[("graph", "\"body.pgraph.ron\""), ("each", "[\"each\"]")],
                ),
            ),
        ],
        &[
            (10, "out", 13, "in"),
            (11, "out", 13, "in"),
            (12, "out", 13, "in"),
            (13, "out", 20, "each"),
        ],
    );
    let r = run_root(&root, &files).expect("run");
    assert_eq!(
        r.output.instance_count(),
        3,
        "instances accumulate over iterations"
    );
    let trees = &r.output.points["trees"];
    assert_eq!(trees.len(), 1, "a named output holds the last write");
    assert_eq!(trees.position[0][0], 3.0, "the last iteration's point");
    assert!(
        r.report
            .warning_codes()
            .contains(&"output_name_replaced".to_string()),
        "{:?}",
        r.report.warning_codes()
    );
    // One call publishing a name the caller does not have is not a replacement.
    let once = graph(
        vec![
            (10, node("gen.points", &[("points", "[((1.0, 0.0, 0.0))]")])),
            (20, node("graph.call", &[("graph", "\"body.pgraph.ron\"")])),
        ],
        &[(10, "out", 20, "each")],
    );
    let r = run_root(&once, &files).expect("run");
    assert_eq!(r.output.points["trees"].len(), 1);
    assert!(
        !r.report
            .warning_codes()
            .contains(&"output_name_replaced".to_string())
    );
}
