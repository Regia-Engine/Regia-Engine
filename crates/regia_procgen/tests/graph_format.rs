// SPDX-License-Identifier: MPL-2.0
//! Spec 62 § 10 tests 1-3: graph format, compile and parameters.

mod common;

use std::collections::BTreeMap;

use common::{graph, node};
use regia_procgen::graph::{ParamDecl, ParamSource, resolve_graph};
use regia_procgen::{AttrType, ProcGraph, ProcGraphVariant, ProcNodeRegistry, SettingValue};

const EXAMPLE: &str = r#"ProcGraph(
    format_version: 1,
    description: "Rocks along the slope bands",
    params: [
        (name: "spacing", ty: Float, default: 4.0, min: Some(0.25), max: Some(64.0), doc: "Meters between candidates"),
        (name: "rock_mesh", ty: Asset, default: "meshes/rock_a.glb", doc: ""),
    ],
    inputs: [ (name: "in", kinds: ["Points", "Spline", "Shape"]) ],
    outputs: [ (name: "out", kinds: ["Points"]) ],
    nodes: {
        1: (ty: "in.self", settings: {}, at: (0.0, 0.0)),
        2: (ty: "sample.ground", settings: { "spacing_m": Bind("spacing"), "jitter": 0.8 }, at: (220.0, 0.0)),
        3: (ty: "spawn.mesh", settings: { "pick": Weighted, "entries": [ (mesh: Bind("rock_mesh"), weight: 1.0) ] }, at: (440.0, 0.0)),
        4: (ty: "graph.output", settings: { "pin": "out" }, at: (660.0, 0.0)),
    },
    edges: [ (from: (1, "out"), to: (2, "region")), (from: (2, "out"), to: (3, "in")), (from: (3, "out"), to: (4, "in")) ],
    flags: (starter: false, tool: None, domain: World),
)"#;

fn registry() -> ProcNodeRegistry {
    ProcNodeRegistry::with_builtins()
}

#[test]
fn spec_example_loads_compiles_and_round_trips_byte_identically() {
    let g = ProcGraph::from_ron(EXAMPLE).expect("load");
    assert_eq!(g.nodes.len(), 4);
    assert_eq!(g.params[0].ty, AttrType::Float);
    let c = g.compile(&registry()).expect("compile");
    assert_eq!(c.nodes.iter().map(|n| n.id).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(
        c.nodes[1].settings.f("spacing_m"),
        4.0,
        "Bind resolved to the default"
    );
    let once = g.to_ron();
    let again = ProcGraph::from_ron(&once).expect("reload").to_ron();
    assert_eq!(once, again, "write -> read -> write is byte-identical");
    assert_eq!(ProcGraph::from_ron(&once).expect("reload"), g);
}

#[test]
fn unknown_setting_names_node_and_setting_and_newer_version_is_refused() {
    let text = EXAMPLE.replace("\"jitter\": 0.8", "\"jiter\": 0.8");
    let g = ProcGraph::from_ron(&text).expect("still loads for the editor");
    let e = g.compile(&registry()).expect_err("unknown setting");
    let err =
        e.0.iter()
            .find(|e| e.code == "unknown_setting")
            .expect("unknown_setting");
    assert_eq!(err.node, Some(2));
    assert!(err.message.contains("jiter"), "{}", err.message);

    let newer = EXAMPLE.replace("format_version: 1", "format_version: 2");
    let e = ProcGraph::from_ron(&newer).expect_err("newer");
    assert_eq!(e.code, "pgraph_version_newer");

    let unknown_ty = EXAMPLE.replace("\"in.self\"", "\"in.nothing\"");
    let e = ProcGraph::from_ron(&unknown_ty)
        .expect("loads")
        .compile(&registry())
        .expect_err("unknown type");
    assert!(
        e.0.iter()
            .any(|x| x.code == "unknown_node_type" && x.node == Some(1))
    );
}

#[test]
fn compile_reports_every_fault_in_one_pass() {
    // Three faults: a cycle 2 <-> 3, a kind mismatch into 5, a setting out of range on 6.
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("points.jitter", &[])),
            (3, node("points.jitter", &[])),
            (4, node("items.count", &[])),
            (5, node("points.jitter", &[])),
            (6, node("points.keep_random", &[("ratio", "1.5")])),
        ],
        &[
            (1, "out", 6, "in"),
            (2, "out", 3, "in"),
            (3, "out", 2, "in"),
            (4, "out", 5, "in"),
        ],
    );
    let e = g.compile(&registry()).expect_err("faults");
    let codes: Vec<&str> = e.0.iter().map(|x| x.code.as_str()).collect();
    assert!(codes.contains(&"graph_cycle"), "{codes:?}");
    assert!(codes.contains(&"pin_kind_mismatch"), "{codes:?}");
    assert!(codes.contains(&"setting_out_of_range"), "{codes:?}");
    assert_eq!(e.0.len(), 3, "exactly the three faults: {:?}", e.0);
    let cycle = e.0.iter().find(|x| x.code == "graph_cycle").expect("cycle");
    assert!(
        matches!(cycle.node, Some(2) | Some(3)),
        "names a node on the cycle"
    );
    let kind = e.0.iter().find(|x| x.code == "pin_kind_mismatch").expect("kind");
    assert!(
        kind.message.contains("4.out") && kind.message.contains("5.in"),
        "{}",
        kind.message
    );
}

#[test]
fn multi_input_edges_concatenate_in_from_node_then_pin_order() {
    // Edges listed out of order: (3, out), (1, out), (2, out) into a multi pin.
    let g = graph(
        vec![
            (1, node("gen.points", &[("points", "[((1.0, 0.0, 0.0))]")])),
            (2, node("gen.points", &[("points", "[((2.0, 0.0, 0.0))]")])),
            (3, node("gen.points", &[("points", "[((3.0, 0.0, 0.0))]")])),
            (4, node("points.merge", &[])),
            (5, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[
            (3, "out", 4, "in"),
            (1, "out", 4, "in"),
            (2, "out", 4, "in"),
            (4, "out", 5, "in"),
        ],
    );
    let snap = common::snapshot(common::box_shape([0.0; 3], [10.0; 3], 0.0), None);
    let r = common::run(&g, snap, 1);
    let p = common::out_points(&r, "out");
    let xs: Vec<f32> = p.position.iter().map(|x| x[0]).collect();
    assert_eq!(xs, vec![1.0, 2.0, 3.0]);
}

#[test]
fn single_input_pin_refuses_two_edges() {
    let g = graph(
        vec![
            (1, node("gen.one", &[])),
            (2, node("gen.one", &[])),
            (3, node("points.jitter", &[])),
        ],
        &[(1, "out", 3, "in"), (2, "out", 3, "in")],
    );
    let e = g.compile(&registry()).expect_err("two edges");
    assert!(e.has("pin_too_many_edges"));
}

fn param_graph() -> ProcGraph {
    let mut g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (
                2,
                node("points.keep_random", &[])
                    .with("ratio", SettingValue::parse("Bind(\"keep\")").expect("bind")),
            ),
        ],
        &[(1, "out", 2, "in")],
    );
    g.params.push(ParamDecl {
        name: "keep".into(),
        ty: AttrType::Float,
        default: SettingValue::Float(0.5),
        min: Some(0.0),
        max: Some(1.0),
        doc: String::new(),
    });
    g
}

#[test]
fn generator_override_beats_variant_beats_default_and_clamping_warns_once() {
    let base = param_graph();
    let files: BTreeMap<String, String> = [
        ("g.pgraph.ron".to_string(), base.to_ron()),
        (
            "v1.pgvar.ron".to_string(),
            ProcGraphVariant {
                format_version: 1,
                parent: "g.pgraph.ron".into(),
                overrides: [("keep".to_string(), SettingValue::Float(0.25))].into(),
            }
            .to_ron(),
        ),
        (
            "v2.pgvar.ron".to_string(),
            ProcGraphVariant {
                format_version: 1,
                parent: "v1.pgvar.ron".into(),
                overrides: [
                    ("keep".to_string(), SettingValue::Float(0.75)),
                    ("renamed_away".to_string(), SettingValue::Int(3)),
                ]
                .into(),
            }
            .to_ron(),
        ),
    ]
    .into();
    let load = |p: &str| files.get(p).cloned();
    let reg = registry();

    let r = resolve_graph("g.pgraph.ron", &load).expect("graph");
    let c = r
        .graph
        .compile_with(&reg, &r.merged_with(&BTreeMap::new()))
        .expect("compile");
    assert_eq!(c.params["keep"], SettingValue::Float(0.5), "default");

    let r = resolve_graph("v2.pgvar.ron", &load).expect("variant chain");
    assert_eq!(
        r.overrides["keep"].1,
        ParamSource::Variant("v2.pgvar.ron".into()),
        "nearest variant wins"
    );
    let c = r
        .graph
        .compile_with(&reg, &r.merged_with(&BTreeMap::new()))
        .expect("compile");
    assert_eq!(c.params["keep"], SettingValue::Float(0.75));
    assert!(
        c.warnings
            .iter()
            .any(|w| w.code == "variant_unknown_param" && w.message.contains("renamed_away")),
        "unknown override kept and warned: {:?}",
        c.warnings
    );

    let generator: BTreeMap<String, SettingValue> = [("keep".to_string(), SettingValue::Float(2.0))].into();
    let c = r
        .graph
        .compile_with(&reg, &r.merged_with(&generator))
        .expect("compile");
    assert_eq!(
        c.params["keep"],
        SettingValue::Float(1.0),
        "generator wins, clamped to max"
    );
    let clamped: Vec<_> = c.warnings.iter().filter(|w| w.code == "param_clamped").collect();
    assert_eq!(clamped.len(), 1, "warned once");
}

#[test]
fn variant_cycle_and_wrong_type_are_refused() {
    let files: BTreeMap<String, String> = [
        (
            "a.pgvar.ron".to_string(),
            "ProcGraphVariant(format_version: 1, parent: \"b.pgvar.ron\", overrides: {})".to_string(),
        ),
        (
            "b.pgvar.ron".to_string(),
            "ProcGraphVariant(format_version: 1, parent: \"a.pgvar.ron\", overrides: {})".to_string(),
        ),
    ]
    .into();
    let e = resolve_graph("a.pgvar.ron", &|p| files.get(p).cloned()).expect_err("cycle");
    assert_eq!(e.code, "variant_cycle");

    let g = param_graph();
    let bad: BTreeMap<String, SettingValue> = [("keep".to_string(), SettingValue::Str("x".into()))].into();
    let e = g.compile_with(&registry(), &bad).expect_err("type");
    assert!(e.has("param_type_mismatch"));
}

#[test]
fn bind_to_an_undeclared_parameter_fails() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("points.keep_random", &[("ratio", "Bind(\"nope\")")])),
        ],
        &[(1, "out", 2, "in")],
    );
    assert!(
        g.compile(&registry())
            .expect_err("unknown param")
            .has("unknown_param")
    );
}

#[test]
fn graph_hash_ignores_canvas_positions_and_reroutes() {
    let a = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("points.jitter", &[])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let mut b = a.clone();
    b.nodes.get_mut(&2).expect("node").at = (500.0, -20.0);
    b.nodes.insert(9, node("reroute", &[]));
    b.edges = vec![
        regia_procgen::Edge::new(1, "out", 9, "in"),
        regia_procgen::Edge::new(9, "out", 2, "in"),
        regia_procgen::Edge::new(2, "out", 3, "in"),
    ];
    b.nodes.insert(10, node("note", &[]));
    let reg = registry();
    assert_eq!(a.graph_hash(&reg), b.graph_hash(&reg));
    let mut c = a.clone();
    c.nodes.get_mut(&2).expect("node").enabled = false;
    assert_ne!(a.graph_hash(&reg), c.graph_hash(&reg));
    assert!(b.compile(&reg).is_ok(), "reroutes collapse before validation");
}

#[test]
fn variant_file_round_trips() {
    let v = ProcGraphVariant {
        format_version: 1,
        parent: "procgen/rocks.pgraph.ron".into(),
        overrides: [("spacing".to_string(), SettingValue::Float(2.0))].into(),
    };
    let text = v.to_ron();
    assert_eq!(ProcGraphVariant::from_ron(&text).expect("load"), v);
    assert_eq!(ProcGraphVariant::from_ron(&text).expect("load").to_ron(), text);
    let e = ProcGraphVariant::from_ron(&text.replace("format_version: 1", "format_version: 9"))
        .expect_err("newer");
    assert_eq!(e.code, "pgvar_version_newer");
}
