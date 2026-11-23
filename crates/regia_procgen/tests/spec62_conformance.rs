// SPDX-License-Identifier: MPL-2.0
//! Spec 62 conformance (lane spec-25-62, 2026-11-10): codes and rules the spec names that had no
//! test of their own. Each test states the spec section it proves.

mod common;

use std::collections::BTreeMap;

use common::{box_shape, compile, graph, node, run_with, snapshot};
use regia_procgen::graph::resolve_graph;
use regia_procgen::{DataItem, DataKind, KindSet, Point, PointSet, ProcNodeRegistry, RunError, RunInputs};

fn registry() -> ProcNodeRegistry {
    ProcNodeRegistry::with_builtins()
}

fn run_code(g: &regia_procgen::ProcGraph) -> Result<regia_procgen::RunResult, RunError> {
    let c = compile(g);
    run_with(
        &c,
        &RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 1),
    )
}

fn node_error(r: &Result<regia_procgen::RunResult, RunError>) -> Option<(u32, String)> {
    match r {
        Err(RunError::Node { node, code, .. }) => Some((*node, code.clone())),
        _ => None,
    }
}

/// § 3.2: creating a custom attribute with a reserved name (`index`, `count`, a leading `_`) is the
/// node error `attr_reserved_name`; an ordinary name is accepted.
#[test]
fn reserved_attribute_names_fail_with_attr_reserved_name() {
    for name in ["index", "count", "_hidden"] {
        let g = graph(
            vec![
                (1, node("gen.grid", &[])),
                (2, node("attr.random", &[("target", &format!("\"{name}\""))])),
                (3, node("graph.output", &[])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        );
        let r = run_code(&g);
        assert_eq!(
            node_error(&r),
            Some((2, "attr_reserved_name".to_string())),
            "`{name}`: {r:?}"
        );
    }
    let ok = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("attr.random", &[("target", "\"wetness\"")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let r = run_code(&ok).expect("an ordinary name is accepted");
    let p = common::out_points(&r, "out");
    assert!(p.attrs.contains_key("wetness"));
}

/// § 3.5: a vector read as a scalar is `attr_type_mismatch` unless a component suffix picks one
/// part (`position` fails, `position:y` passes).
#[test]
fn a_vector_read_as_a_scalar_needs_a_component_suffix() {
    let filter = |attr: &str| {
        graph(
            vec![
                (1, node("gen.grid", &[])),
                (
                    2,
                    node(
                        "filter.range",
                        &[("attr", &format!("\"{attr}\"")), ("min", "-1.0"), ("max", "1.0")],
                    ),
                ),
                (3, node("graph.output", &[])),
            ],
            &[(1, "out", 2, "in"), (2, "inside", 3, "in")],
        )
    };
    let r = run_code(&filter("position"));
    assert_eq!(
        node_error(&r),
        Some((2, "attr_type_mismatch".to_string())),
        "{r:?}"
    );
    let r = run_code(&filter("position:y")).expect("a suffix picks one part");
    // gen.grid's default (4, 1, 4) grid sits at y = 0, inside [-1, 1].
    assert_eq!(common::out_points(&r, "out").len(), 16);

    let cmp = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("attr.compare", &[("a", "\"scale\""), ("b", "1.0")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    assert_eq!(
        node_error(&run_code(&cmp)),
        Some((2, "attr_type_mismatch".to_string()))
    );
}

/// § 4.2 rule 5: a required input pin with no edge fails compile with `pin_unconnected` naming the
/// node.
#[test]
fn a_required_pin_without_an_edge_is_pin_unconnected() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("attr.reduce", &[("attr", "\"density\"")])),
        ],
        &[],
    );
    let e = g.compile(&registry()).expect_err("unconnected");
    let hit =
        e.0.iter()
            .find(|x| x.code == "pin_unconnected")
            .unwrap_or_else(|| panic!("no pin_unconnected in {:?}", e.0));
    assert_eq!(hit.node, Some(2));
    assert!(hit.message.contains("`in`"), "{}", hit.message);
    // Connected, the same graph compiles.
    let ok = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("attr.reduce", &[("attr", "\"density\"")])),
        ],
        &[(1, "out", 2, "in")],
    );
    ok.compile(&registry()).expect("connected compiles");
}

/// § 4.2 rule 5: a declared graph output that nothing feeds is a warning `output_unreachable`, not
/// an error; the graph still compiles and runs, and the warning reaches the run report.
#[test]
fn an_unfed_graph_output_warns_output_unreachable() {
    let mut g = graph(
        vec![(1, node("gen.grid", &[])), (2, node("points.jitter", &[]))],
        &[(1, "out", 2, "in")],
    );
    g.outputs = vec![regia_procgen::graph::GraphPin {
        name: "out".to_string(),
        kinds: KindSet::of(&[DataKind::Points]),
    }];
    let c = g.compile(&registry()).expect("a warning does not stop compile");
    assert!(
        c.warnings.iter().any(|w| w.code == "output_unreachable"),
        "{:?}",
        c.warnings
    );
    let r = run_with(
        &c,
        &RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 1),
    )
    .expect("runs");
    assert!(
        r.report
            .compile_warnings
            .iter()
            .any(|w| w.code == "output_unreachable")
    );

    // Fed by a `graph.output` node, no warning.
    let mut fed = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in")],
    );
    fed.outputs = g.outputs.clone();
    let c = fed.compile(&registry()).expect("compiles");
    assert!(c.warnings.iter().all(|w| w.code != "output_unreachable"));
}

/// § 7.2 / spec 63 § 3: `in.generator_output` naming an export the snapshot does not hold warns
/// `export_missing` and outputs an empty bundle (the run still succeeds); a held export is
/// forwarded.
#[test]
fn a_missing_generator_export_warns_and_a_present_one_is_forwarded() {
    let g = graph(
        vec![
            (
                1,
                node(
                    "in.generator_output",
                    &[("generator", "\"quarry\""), ("table", "\"rocks\"")],
                ),
            ),
            (2, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in")],
    );
    let c = compile(&g);
    let r = run_with(
        &c,
        &RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 1),
    )
    .expect("a missing export is a warning, not an error");
    let line = r.report.nodes.iter().find(|n| n.id == 1).expect("node 1 report");
    assert!(
        line.warnings.iter().any(|(code, _)| code == "export_missing"),
        "{:?}",
        line.warnings
    );
    assert!(common::out_points(&r, "out").is_empty());

    let mut snap = (*snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None)).clone();
    let mut pts = PointSet::default();
    pts.push(Point {
        position: [1.0, 2.0, 3.0],
        ..Point::default()
    });
    snap.exports.insert(
        ("quarry".to_string(), "rocks".to_string()),
        DataItem::new(regia_procgen::Payload::Points(pts)),
    );
    let r = run_with(&c, &RunInputs::new(std::sync::Arc::new(snap), 1)).expect("runs");
    let line = r.report.nodes.iter().find(|n| n.id == 1).expect("node 1 report");
    assert!(line.warnings.iter().all(|(code, _)| code != "export_missing"));
    let p = common::out_points(&r, "out");
    assert_eq!(p.len(), 1);
    assert_eq!(p.position[0], [1.0, 2.0, 3.0]);
}

/// § 4: a variant's parent chain may hold up to 8 variants; a ninth is refused with
/// `variant_chain_too_long`.
#[test]
fn a_variant_chain_of_eight_resolves_and_nine_is_refused() {
    let chain = |variants: usize| {
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        files.insert(
            "base.pgraph.ron".to_string(),
            "ProcGraph(format_version: 1, nodes: {}, edges: [])".to_string(),
        );
        for k in 0..variants {
            let parent = if k + 1 == variants {
                "base.pgraph.ron".to_string()
            } else {
                format!("v{}.pgvar.ron", k + 1)
            };
            files.insert(
                format!("v{k}.pgvar.ron"),
                format!("ProcGraphVariant(format_version: 1, parent: \"{parent}\", overrides: {{}})"),
            );
        }
        files
    };
    let eight = chain(8);
    let ok = resolve_graph("v0.pgvar.ron", &|p| eight.get(p).cloned()).expect("eight variants resolve");
    assert_eq!(ok.files.len(), 9, "eight variants plus the graph");
    let nine = chain(9);
    let e = resolve_graph("v0.pgvar.ron", &|p| nine.get(p).cloned()).expect_err("nine variants");
    assert_eq!(e.code, "variant_chain_too_long");
}

/// A node type at settings version 3 whose version-1 files named `radius_m` `radius` and whose
/// version-2 files stored it in centimeters (spec 62 § 4: a settings migrator chain per node type).
struct Versioned;

impl regia_procgen::NodeKind for Versioned {
    fn id(&self) -> &str {
        "test.versioned"
    }
    fn category(&self) -> &str {
        "test"
    }
    fn doc(&self) -> &str {
        "Settings version 3 with a migrator chain from 1"
    }
    fn settings(&self) -> &[regia_procgen::nodes::SettingDesc] {
        const S: &[regia_procgen::nodes::SettingDesc] = &[regia_procgen::nodes::SettingDesc {
            name: "radius_m",
            ty: regia_procgen::nodes::SettingType::Float,
            default: "1.0",
            min: Some(0.01),
            max: Some(100.0),
            doc: "Radius",
        }];
        S
    }
    fn pins(&self, _: &regia_procgen::nodes::Settings) -> regia_procgen::nodes::PinSpec {
        regia_procgen::nodes::PinSpec::new(
            vec![],
            vec![regia_procgen::nodes::PinDesc {
                name: "out".into(),
                kinds: KindSet::ANY,
                multi: false,
                required: false,
            }],
        )
    }
    fn eval(
        &self,
        _: &mut regia_procgen::exec::NodeCtx<'_>,
        _: &[regia_procgen::Bundle],
        _: &regia_procgen::nodes::Settings,
    ) -> Result<Vec<regia_procgen::Bundle>, regia_procgen::nodes::NodeError> {
        Ok(vec![regia_procgen::Bundle::empty()])
    }
    fn settings_version(&self) -> u32 {
        3
    }
    fn migrate_settings(
        &self,
        from: u32,
        settings: &mut BTreeMap<String, regia_procgen::SettingValue>,
    ) -> Result<(), String> {
        let mut v = from;
        if v == 1 {
            if let Some(r) = settings.remove("radius") {
                let cm = r.as_f64().ok_or("radius is a number")? * 100.0;
                settings.insert("radius_m".into(), regia_procgen::SettingValue::Float(cm));
            }
            v = 2;
        }
        if v == 2 {
            if let Some(cm) = settings
                .get("radius_m")
                .and_then(regia_procgen::SettingValue::as_f64)
            {
                settings.insert("radius_m".into(), regia_procgen::SettingValue::Float(cm / 100.0));
            }
            v = 3;
        }
        if v == 3 {
            Ok(())
        } else {
            Err(format!("no settings migrator from version {from}"))
        }
    }
}

/// § 4: a node entry's `v: N` runs the node type's migrator chain up to the current settings
/// version; a version newer than the build is `setting_version_newer`; an older version without a
/// migrator is `setting_migration`. The file's `format_version` stays 1 throughout.
#[test]
fn node_settings_versions_migrate_and_newer_versions_are_refused() {
    let mut reg = registry();
    reg.register(std::sync::Arc::new(Versioned));
    let radius = |text: &str| {
        let g = regia_procgen::ProcGraph::from_ron(text).expect("loads");
        assert_eq!(g.format_version, 1);
        g.compile(&reg).map(|c| c.nodes[0].settings.f("radius_m"))
    };
    let v1 = r#"ProcGraph(format_version: 1, nodes: { 1: (ty: "test.versioned", v: 1, settings: { "radius": 2.5 }) }, edges: [])"#;
    let v2 = r#"ProcGraph(format_version: 1, nodes: { 1: (ty: "test.versioned", v: 2, settings: { "radius_m": 250.0 }) }, edges: [])"#;
    let v3 = r#"ProcGraph(format_version: 1, nodes: { 1: (ty: "test.versioned", settings: { "radius_m": 2.5 }) }, edges: [])"#;
    for (name, text) in [("v1", v1), ("v2", v2), ("current", v3)] {
        let r = radius(text).unwrap_or_else(|e| panic!("{name}: {:?}", e.0));
        assert!((r - 2.5).abs() < 1e-12, "{name}: {r}");
    }
    let newer = r#"ProcGraph(format_version: 1, nodes: { 1: (ty: "test.versioned", v: 4, settings: {}) }, edges: [])"#;
    assert!(radius(newer).expect_err("newer").has("setting_version_newer"));
    // A built-in node type is at version 1: v 2 is newer, v 0 has no migrator.
    let jitter = |v: u32| {
        regia_procgen::ProcGraph::from_ron(&format!(
            r#"ProcGraph(format_version: 1, nodes: {{ 1: (ty: "gen.grid", v: {v}, settings: {{}}) }}, edges: [])"#
        ))
        .expect("loads")
        .compile(&registry())
    };
    assert!(jitter(2).expect_err("newer").has("setting_version_newer"));
    assert!(jitter(0).expect_err("no migrator").has("setting_migration"));
    assert!(jitter(1).is_ok(), "the current version compiles");
}

/// § 4.1: parameter names follow `[a-z][a-z0-9_]{0,63}`; any other name fails compile with
/// `param_name`.
#[test]
fn parameter_names_follow_the_documented_pattern() {
    let with_param = |name: &str| {
        let mut g = graph(vec![(1, node("gen.grid", &[]))], &[]);
        g.params.push(regia_procgen::graph::ParamDecl {
            name: name.to_string(),
            ty: regia_procgen::AttrType::Float,
            default: regia_procgen::SettingValue::Float(1.0),
            min: None,
            max: None,
            doc: String::new(),
        });
        g.compile(&registry())
    };
    let longest = format!("a{}", "b".repeat(63));
    for ok in ["a", "spacing_m", "x9", longest.as_str()] {
        assert!(with_param(ok).is_ok(), "`{ok}` is valid");
    }
    let too_long = format!("a{}", "b".repeat(64));
    for bad in ["", "Spacing", "9x", "_x", "a-b", "a b", too_long.as_str()] {
        assert!(
            with_param(bad).expect_err("invalid name").has("param_name"),
            "`{bad}` is refused"
        );
    }
}

/// § 4 bypass rules beyond `points.jitter`: a bypassed node with no inputs outputs empty bundles;
/// a bypassed node with two outputs passes its first input to its first output and leaves the
/// second empty. § 4 reroute nodes forward their bundle unchanged.
#[test]
fn bypass_without_inputs_and_with_two_outputs_and_reroute_forwarding() {
    let mut grid = node("gen.grid", &[]);
    grid.enabled = false;
    let g = graph(
        vec![(1, grid), (2, node("graph.output", &[]))],
        &[(1, "out", 2, "in")],
    );
    let r = run_code(&g).expect("runs");
    assert!(
        common::out_points(&r, "out").is_empty(),
        "no inputs: empty output"
    );
    let line = r.report.nodes.iter().find(|n| n.id == 1).expect("report");
    assert!(line.bypassed, "the report marks the node bypassed");

    let mut filter = node(
        "filter.range",
        &[("attr", "\"position:y\""), ("min", "100.0"), ("max", "200.0")],
    );
    filter.enabled = false;
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, filter),
            (3, node("graph.output", &[("pin", "\"inside\"")])),
            (4, node("graph.output", &[("pin", "\"outside\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "inside", 3, "in"),
            (2, "outside", 4, "in"),
        ],
    );
    let r = run_code(&g).expect("runs");
    assert_eq!(
        common::out_points(&r, "inside").len(),
        16,
        "first input to first output"
    );
    assert!(
        common::out_points(&r, "outside").is_empty(),
        "other outputs empty"
    );

    let direct = graph(
        vec![(1, node("gen.grid", &[])), (2, node("graph.output", &[]))],
        &[(1, "out", 2, "in")],
    );
    let rerouted = graph(
        vec![
            (1, node("gen.grid", &[])),
            (5, node("reroute", &[])),
            (6, node("reroute", &[])),
            (2, node("graph.output", &[])),
        ],
        &[(1, "out", 5, "in"), (5, "out", 6, "in"), (6, "out", 2, "in")],
    );
    let a = run_code(&direct).expect("direct");
    let b = run_code(&rerouted).expect("rerouted");
    assert_eq!(
        a.output_hash, b.output_hash,
        "reroutes forward the bundle unchanged"
    );
    assert_eq!(common::out_points(&b, "out").len(), 16);
}
