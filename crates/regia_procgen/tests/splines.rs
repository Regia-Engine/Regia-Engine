// SPDX-License-Identifier: MPL-2.0
//! Spec 64 § 9 tests 6-9 (polygons, `polygon.cut`, `spline.intersect`, `spline.modules`) plus
//! `sample.spline`, `spline.clip` and determinism checks. Splines and polygons enter through
//! `graph.input` pins (no world capture needed); no GPU.

mod common;

use common::{box_shape, compile, graph, node, snapshot};
use regia_core::math::Vec3;
use regia_procgen::exec::Frame;
use regia_procgen::nodes::{polygon_data_item, spline_data_item};
use regia_procgen::polygon::{BoolOp, Join, PolygonData};
use regia_procgen::{Bundle, CancelToken, Evaluator, Payload, ProcGraph, RunError, RunInputs, RunResult};
use regia_scene::spline::{Spline, TangentMode};

fn run_inputs(g: &ProcGraph, inputs: Vec<(&str, Bundle)>) -> Result<RunResult, RunError> {
    let c = compile(g);
    let frame = Frame {
        graph_inputs: inputs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        ..Frame::default()
    };
    let ri = RunInputs::new(snapshot(box_shape([0.0; 3], [200.0; 3], 0.0), None), 7);
    Evaluator::new().run_frame(&c, &ri, &CancelToken::new(), &frame)
}

fn out(r: &RunResult, pin: &str) -> Bundle {
    r.output.graph_outputs.get(pin).cloned().unwrap_or_default()
}

fn line(a: [f32; 3], b: [f32; 3]) -> Spline {
    let mut s = Spline::through([Vec3::from(a), Vec3::from(b)]);
    for p in &mut s.points {
        p.mode = TangentMode::Linear;
    }
    s
}

fn polys(b: &Bundle) -> Vec<PolygonData> {
    b.0.iter()
        .filter_map(|i| match &i.payload {
            Payload::Polygon(p) => Some((**p).clone()),
            _ => None,
        })
        .collect()
}

fn splines(b: &Bundle) -> Vec<Spline> {
    b.0.iter()
        .filter_map(|i| match &i.payload {
            Payload::Spline(s) => Some(s.spline.clone()),
            _ => None,
        })
        .collect()
}

fn square(x0: f64, z0: f64, s: f64) -> Vec<[f64; 2]> {
    vec![[x0, z0], [x0 + s, z0], [x0 + s, z0 + s], [x0, z0 + s]]
}

#[test]
fn test_6_polygon_booleans_offset_and_even_odd() {
    let a = PolygonData::new([0.0; 3], &[square(0.0, 0.0, 2.0)]).expect("a");
    let b = PolygonData::new([0.0; 3], &[square(1.0, 1.0, 2.0)]).expect("b");
    let u = a.boolean(&b, BoolOp::Union).expect("union");
    assert!(
        (u.area() - 7.0).abs() < 1e-9,
        "exact on the integer grid: {}",
        u.area()
    );
    let i = a.boolean(&b, BoolOp::Intersection).expect("intersection");
    assert!((i.area() - 1.0).abs() < 1e-9);
    let x = a.boolean(&b, BoolOp::Xor).expect("xor");
    assert!((x.area() - 6.0).abs() < 1e-9);
    // Difference of a smaller square inside a bigger one leaves a hole.
    let big = PolygonData::new([0.0; 3], &[square(0.0, 0.0, 10.0)]).expect("big");
    let small = PolygonData::new([0.0; 3], &[square(4.0, 4.0, 2.0)]).expect("small");
    let holed = big.boolean(&small, BoolOp::Difference).expect("difference");
    assert_eq!(holed.rings.len(), 2);
    assert!((holed.area() - 96.0).abs() < 1e-9);
    assert!(holed.contains(1.0, 1.0) && !holed.contains(5.0, 5.0));
    // Unit square grown by 1 m with round joins: 1 + 4 + π within 0.5 %.
    let unit = PolygonData::new([0.0; 3], &[square(0.0, 0.0, 1.0)]).expect("unit");
    let grown = unit.offset(1.0, Join::Round, 2.0).expect("offset");
    let expected = 5.0 + std::f64::consts::PI;
    assert!(
        ((grown.area() - expected) / expected).abs() < 0.005,
        "{}",
        grown.area()
    );
    // A figure eight resolves by even-odd into two lobes.
    let eight =
        PolygonData::new([0.0; 3], &[vec![[0.0, 0.0], [2.0, 2.0], [2.0, 0.0], [0.0, 2.0]]]).expect("eight");
    assert_eq!(eight.pieces().len(), 2);
    assert!((eight.area() - 2.0).abs() < 1e-9, "{}", eight.area());
}

#[test]
fn test_7_polygon_cut_by_crossing_streets() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"field\"")])),
            (2, node("graph.input", &[("pin", "\"streets\"")])),
            (3, node("polygon.cut", &[("gap_m", "10.0")])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 3, "in"), (2, "out", 3, "cutters"), (3, "out", 4, "in")],
    );
    let field = PolygonData::rect([0.0, 0.0, 100.0, 100.0], 0.0);
    let streets = Bundle(vec![
        spline_data_item(line([50.0, 0.0, -10.0], [50.0, 0.0, 110.0])),
        spline_data_item(line([-10.0, 0.0, 50.0], [110.0, 0.0, 50.0])),
    ]);
    let r = run_inputs(
        &g,
        vec![
            ("field", Bundle::one(polygon_data_item(field))),
            ("streets", streets),
        ],
    )
    .expect("run");
    let pieces = polys(&out(&r, "out"));
    assert_eq!(pieces.len(), 4);
    for p in &pieces {
        assert!((p.area() - 2025.0).abs() < 1e-6, "{}", p.area());
        let b = p.bounds().expect("bounds");
        assert!(((b[2] - b[0]) - 45.0).abs() < 1e-9 && ((b[3] - b[1]) - 45.0).abs() < 1e-9);
    }
    // Ordered by centroid x, then z.
    let c: Vec<[f64; 2]> = pieces.iter().map(PolygonData::centroid).collect();
    assert!(c[0][0] < 50.0 && c[0][1] < 50.0 && c[1][0] < 50.0 && c[1][1] > 50.0);
    assert!(c[2][0] > 50.0 && c[2][1] < 50.0 && c[3][0] > 50.0 && c[3][1] > 50.0);
}

#[test]
fn test_8_spline_intersect_x() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("spline.intersect", &[])),
            (3, node("graph.output", &[("pin", "\"splines\"")])),
            (4, node("graph.output", &[("pin", "\"points\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out_splines", 3, "in"),
            (2, "out_points", 4, "in"),
        ],
    );
    let input = Bundle(vec![
        spline_data_item(line([-10.0, 0.0, -10.0], [10.0, 0.0, 10.0])),
        spline_data_item(line([-10.0, 0.0, 10.0], [10.0, 0.0, -10.0])),
    ]);
    let r = run_inputs(&g, vec![("in", input)]).expect("run");
    let pts = out(&r, "points");
    let p = pts.0[0].as_points().expect("points");
    assert_eq!(p.len(), 1);
    assert!(
        Vec3::from(p.position[0]).abs_diff_eq(Vec3::ZERO, 1e-3),
        "{:?}",
        p.position[0]
    );
    let sp = splines(&out(&r, "splines"));
    assert_eq!(sp.len(), 2);
    for s in &sp {
        assert_eq!(s.points.len(), 3);
        assert!(s.points[1].position.abs_diff_eq(Vec3::ZERO, 1e-3));
        assert!((s.length() - 800.0f32.sqrt()).abs() < 1e-3);
    }
}

fn modules_graph(pattern: &str) -> ProcGraph {
    let modules = r#"{
        "post": (mesh: "fence/post.glb", length_m: 0.2),
        "panel": (mesh: "fence/panel.glb", length_m: 2.0, stretch: (0.85, 1.15)),
        "gate": (prefab: "fence/gate.prefab.ron", length_m: 3.0),
        "end": (mesh: "fence/end_post.glb", length_m: 0.25),
    }"#;
    graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (
                2,
                node(
                    "spline.modules",
                    &[("modules", modules), ("pattern", &format!("{pattern:?}"))],
                ),
            ),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    )
}

fn module_names(r: &RunResult) -> Vec<String> {
    let b = out(r, "out");
    match b.0[0].as_points().and_then(|p| p.attrs.get("module")) {
        Some(regia_procgen::AttrColumn::Name(v)) => v.to_vec(),
        _ => Vec::new(),
    }
}

#[test]
fn test_9_module_pattern_fits() {
    let fence = Bundle::one(spline_data_item(line([0.0; 3], [21.0, 0.0, 0.0])));
    let r = run_inputs(&modules_graph("post repeat(panel post) end"), vec![("in", fence)]).expect("run");
    let names = module_names(&r);
    assert_eq!(names.iter().filter(|n| *n == "panel").count(), 9);
    assert_eq!(names.first().map(String::as_str), Some("post"));
    assert_eq!(names.last().map(String::as_str), Some("end"));
    let b = out(&r, "out");
    let p = b.0[0].as_points().expect("points");
    let Some(regia_procgen::AttrColumn::Float(lens)) = p.attrs.get("length_m") else {
        panic!("length_m missing");
    };
    let total: f64 = lens.iter().sum();
    assert!((total - 21.0).abs() < 1e-3, "{total}");
    // Panels stretched equally, within range; posts not stretched.
    for (n, l) in names.iter().zip(lens) {
        match n.as_str() {
            "panel" => assert!((l - (2.0 + 0.75 / 9.0)).abs() < 1e-6, "{l}"),
            "post" => assert!((l - 0.2).abs() < 1e-9),
            _ => {}
        }
    }
    // Each module starts where the previous ended.
    let Some(regia_procgen::AttrColumn::Float(starts)) = p.attrs.get("distance_m") else {
        panic!("distance_m missing");
    };
    for k in 1..starts.len() {
        assert!((starts[k] - starts[k - 1] - lens[k - 1]).abs() < 1e-9);
    }
    assert!((p.position[1][0] - 0.2).abs() < 1e-4);

    let short = Bundle::one(spline_data_item(line([0.0; 3], [0.3, 0.0, 0.0])));
    let r = run_inputs(&modules_graph("post repeat(panel post) end"), vec![("in", short)]).expect("run");
    assert_eq!(module_names(&r), vec!["post".to_string()]);
    assert!(r.report.warning_codes().contains(&"pattern_too_long".to_string()));

    let five = Bundle::one(spline_data_item(line([0.0; 3], [5.0, 0.0, 0.0])));
    let r = run_inputs(&modules_graph("first(gate, panel)"), vec![("in", five)]).expect("run");
    assert_eq!(module_names(&r), vec!["gate".to_string()]);
    let two = Bundle::one(spline_data_item(line([0.0; 3], [2.5, 0.0, 0.0])));
    let r = run_inputs(&modules_graph("first(gate, panel)"), vec![("in", two)]).expect("run");
    assert_eq!(module_names(&r), vec!["panel".to_string()]);

    // Syntax errors fail compile validation with the column.
    let bad = modules_graph("post repeat(panel");
    let err = bad
        .compile(&regia_procgen::ProcNodeRegistry::with_builtins())
        .expect_err("syntax error");
    let text = format!("{err:?}");
    assert!(
        text.contains("pattern_syntax") && text.contains("column 18"),
        "{text}"
    );
    let e = regia_procgen::nodes::parse_pattern("a b ( c").expect_err("bad");
    assert_eq!((e.code, e.column), ("pattern_syntax", 5));
    let unknown = modules_graph("post wall");
    let err = unknown
        .compile(&regia_procgen::ProcNodeRegistry::with_builtins())
        .expect_err("unknown");
    assert!(format!("{err:?}").contains("pattern_unknown_module"));
}

#[test]
fn modules_pick_is_deterministic_and_gap_fit_spreads() {
    let g = |pattern: &str| {
        let mut g = modules_graph(pattern);
        if let Some(n) = g.nodes.get_mut(&2) {
            *n = n
                .clone()
                .with("fit", regia_procgen::SettingValue::Ident("Gap".into()));
        }
        g
    };
    let input = || Bundle::one(spline_data_item(line([0.0; 3], [30.0, 0.0, 5.0])));
    let a = run_inputs(&g("repeat(pick(post: 1, end: 1) gap(1))"), vec![("in", input())]).expect("a");
    let b = run_inputs(&g("repeat(pick(post: 1, end: 1) gap(1))"), vec![("in", input())]).expect("b");
    assert_eq!(a.output_hash, b.output_hash);
    let names = module_names(&a);
    assert!(
        names.contains(&"post".to_string()) && names.contains(&"end".to_string()),
        "{names:?}"
    );
    let r = run_inputs(&g("post post"), vec![("in", input())]).expect("gap");
    let b = out(&r, "out");
    let p = b.0[0].as_points().expect("points");
    let Some(regia_procgen::AttrColumn::Float(starts)) = p.attrs.get("distance_m") else {
        panic!("distance_m missing");
    };
    let len = 30.0f64.hypot(5.0);
    assert!((starts[1] - (len - 0.2)).abs() < 1e-3, "{starts:?}");
}

#[test]
fn sample_spline_fits_ends_and_writes_attributes() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("sample.spline", &[("step_m", "3.0")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let r = run_inputs(
        &g,
        vec![(
            "in",
            Bundle::one(spline_data_item(line([0.0; 3], [10.0, 0.0, 0.0]))),
        )],
    )
    .expect("run");
    let b = out(&r, "out");
    let p = b.0[0].as_points().expect("points");
    // round(10 / 3) = 3 steps of 3.333 m: four points, both ends included.
    assert_eq!(p.len(), 4);
    assert!((p.position[3][0] - 10.0).abs() < 1e-4);
    for name in ["along", "distance_m", "key", "tangent", "curvature", "turn_deg"] {
        assert!(p.attrs.contains_key(name), "{name}");
    }
    // Rotation: local X along the tangent (+X here).
    let q = regia_procgen::rot::Rot(p.rotation[1]).to_mat3();
    assert!(q.x_axis.abs_diff_eq(Vec3::X, 1e-5));

    let open_inside = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("sample.spline", &[("place", "inside")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let err = run_inputs(
        &open_inside,
        vec![(
            "in",
            Bundle::one(spline_data_item(line([0.0; 3], [10.0, 0.0, 0.0]))),
        )],
    )
    .expect_err("open spline");
    assert_eq!(err.code(), "spline_not_closed");
    let mut ring = Spline::through([
        Vec3::ZERO,
        Vec3::new(10.0, 0.0, 0.0),
        Vec3::new(10.0, 0.0, 10.0),
        Vec3::new(0.0, 0.0, 10.0),
    ])
    .closed();
    ring.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let r = run_inputs(&open_inside, vec![("in", Bundle::one(spline_data_item(ring)))]).expect("inside");
    assert_eq!(out(&r, "out").0[0].as_points().map(|p| p.len()), Some(25));
}

#[test]
fn clip_routes_pieces_and_offset_reverse_roundtrip() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("graph.input", &[("pin", "\"poly\"")])),
            (3, node("spline.clip", &[])),
            (4, node("graph.output", &[("pin", "\"inside\"")])),
            (5, node("graph.output", &[("pin", "\"outside\"")])),
        ],
        &[
            (1, "out", 3, "in"),
            (2, "out", 3, "polygons"),
            (3, "inside", 4, "in"),
            (3, "outside", 5, "in"),
        ],
    );
    let r = run_inputs(
        &g,
        vec![
            (
                "in",
                Bundle::one(spline_data_item(line([-20.0, 0.0, 0.0], [20.0, 0.0, 0.0]))),
            ),
            (
                "poly",
                Bundle::one(polygon_data_item(PolygonData::rect([-5.0, -5.0, 5.0, 5.0], 0.0))),
            ),
        ],
    )
    .expect("run");
    let inside = splines(&out(&r, "inside"));
    let outside = splines(&out(&r, "outside"));
    assert_eq!((inside.len(), outside.len()), (1, 2));
    assert!((inside[0].length() - 10.0).abs() < 1e-3);

    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("spline.offset", &[("distance_m", "2.0")])),
            (3, node("spline.reverse", &[])),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in"), (3, "out", 4, "in")],
    );
    let r = run_inputs(
        &g,
        vec![(
            "in",
            Bundle::one(spline_data_item(line([0.0; 3], [10.0, 0.0, 0.0]))),
        )],
    )
    .expect("run");
    let s = &splines(&out(&r, "out"))[0];
    // Right of +X with up +Y is +Z; reversed, the curve runs from x = 10 back to 0.
    let a = s.at_param(0.0).position;
    let b = s.at_param(1.0).position;
    assert!(a.abs_diff_eq(Vec3::new(10.0, 0.0, 2.0), 1e-3), "{a:?}");
    assert!(b.abs_diff_eq(Vec3::new(0.0, 0.0, 2.0), 1e-3), "{b:?}");
}

#[test]
fn spline_graph_outputs_are_deterministic() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("sample.spline", &[("step_m", "0.7"), ("jitter", "0.5")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let curvy = || {
        Bundle::one(spline_data_item(Spline::through([
            Vec3::ZERO,
            Vec3::new(10.0, 1.0, 5.0),
            Vec3::new(20.0, 0.0, -5.0),
            Vec3::new(30.0, 2.0, 0.0),
        ])))
    };
    let a = run_inputs(&g, vec![("in", curvy())]).expect("a");
    let b = run_inputs(&g, vec![("in", curvy())]).expect("b");
    assert_eq!(a.output_hash, b.output_hash);
}

fn run_one(node_ty: &str, settings: &[(&str, &str)], spline: Spline) -> RunResult {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node(node_ty, settings)),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    run_inputs(&g, vec![("in", Bundle::one(spline_data_item(spline)))]).expect("run")
}

fn first_points(r: &RunResult) -> regia_procgen::PointSet {
    out(r, "out").0[0].as_points().expect("points").clone()
}

#[test]
fn sample_spline_zero_range_emits_one_point() {
    // start_m + end_m covers the whole curve: one station, not two coincident ones.
    for fit in ["true", "false"] {
        let r = run_one(
            "sample.spline",
            &[("start_m", "6.0"), ("end_m", "6.0"), ("fit_ends", fit)],
            line([0.0; 3], [10.0, 0.0, 0.0]),
        );
        assert_eq!(first_points(&r).len(), 1, "fit_ends {fit}");
    }
}

#[test]
fn sample_spline_inside_uses_the_world_lattice() {
    // Box (1,1)-(10,10), spacing 4: world cell centers are 2, 6, 10 and the half-open range keeps
    // cells 0..=2; only those inside the polygon stay (10 is on the max edge: excluded by contains).
    let mut ring = Spline::through([
        Vec3::new(1.0, 0.0, 1.0),
        Vec3::new(10.0, 0.0, 1.0),
        Vec3::new(10.0, 0.0, 10.0),
        Vec3::new(1.0, 0.0, 10.0),
    ])
    .closed();
    ring.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let r = run_one(
        "sample.spline",
        &[("place", "inside"), ("spacing_m", "4.0")],
        ring,
    );
    let p = first_points(&r);
    for pos in &p.position {
        let cell = |v: f32| (f64::from(v) / 4.0 - 0.5).fract().abs();
        assert!(cell(pos[0]) < 1e-5 && cell(pos[2]) < 1e-5, "off lattice: {pos:?}");
    }
    assert!(p.position.iter().any(|q| (q[0] - 2.0).abs() < 1e-5));
    assert!(!p.position.iter().any(|q| (q[0] - 3.0).abs() < 1e-5));
}

#[test]
fn sample_spline_per_segment_closed_has_no_seam_duplicate() {
    let mut ring = Spline::through([
        Vec3::ZERO,
        Vec3::new(10.0, 0.0, 0.0),
        Vec3::new(10.0, 0.0, 10.0),
        Vec3::new(0.0, 0.0, 10.0),
    ])
    .closed();
    ring.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let r = run_one(
        "sample.spline",
        &[("by", "\"per_segment\""), ("per_segment", "2")],
        ring,
    );
    // 4 segments x 2 points, the seam station not repeated.
    assert_eq!(first_points(&r).len(), 8);
}

// ---------------------------------------------------------------------------------------------
// Spec conformance additions (lane spec-26-64-splines-polygons-road, 2026-11-10).
// ---------------------------------------------------------------------------------------------

/// Spec 64 § 4 `spline.simplify`: points closer than `merge_m` to the previous kept one are
/// dropped, then Ramer-Douglas-Peucker removes points within `tolerance_m`; the ends and
/// `Free`/`Aligned` points are always kept.
#[test]
fn spline_simplify_merges_then_drops_within_tolerance_keeping_pinned_points() {
    let mut s = Spline::through([
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(0.01, 0.0, 0.0), // merged (closer than 0.05 m)
        Vec3::new(5.0, 0.0, 0.02), // within 0.1 m of the chord: dropped
        Vec3::new(10.0, 0.0, 0.0), // collinear but Free: kept
        Vec3::new(15.0, 0.0, 0.03),
        Vec3::new(20.0, 0.0, 0.0),
        Vec3::new(25.0, 0.0, 3.0), // 3 m off the chord: kept
        Vec3::new(30.0, 0.0, 0.0),
    ]);
    for p in &mut s.points {
        p.mode = TangentMode::Linear;
    }
    s.points[3].mode = TangentMode::Free;
    let r = run_one("spline.simplify", &[], s);
    let got = &splines(&out(&r, "out"))[0];
    let xs: Vec<f32> = got.points.iter().map(|p| p.position.x).collect();
    assert_eq!(xs, vec![0.0, 10.0, 20.0, 25.0, 30.0], "{xs:?}");
    assert_eq!(got.points[1].mode, TangentMode::Free);
    // A loose tolerance still keeps the ends and the Free point.
    let mut s2 = got.clone();
    s2.points[3].position.z = 0.5;
    let r = run_one("spline.simplify", &[("tolerance_m", "1.0")], s2);
    let xs: Vec<f32> = splines(&out(&r, "out"))[0]
        .points
        .iter()
        .map(|p| p.position.x)
        .collect();
    assert_eq!(xs, vec![0.0, 10.0, 30.0]);
}

/// Spec 64 § 2.1: a spline with fewer than 2 points is treated as empty with warning
/// `spline_degenerate`.
#[test]
fn a_one_point_spline_warns_spline_degenerate() {
    let r = run_one("sample.spline", &[], Spline::through([Vec3::new(1.0, 2.0, 3.0)]));
    assert!(
        r.report.warning_codes().iter().any(|c| c == "spline_degenerate"),
        "{:?}",
        r.report.warning_codes()
    );
}

/// Spec 64 § 4 `polygon.bool` (each pin unioned first) and `polygon.to_spline` (closed `Linear`
/// splines, one per ring).
#[test]
fn polygon_bool_unions_each_pin_and_to_spline_closes_rings() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"a\"")])),
            (2, node("graph.input", &[("pin", "\"b\"")])),
            (3, node("polygon.bool", &[("op", "\"difference\"")])),
            (4, node("graph.output", &[])),
            (5, node("polygon.to_spline", &[])),
            (6, node("graph.output", &[("pin", "\"rings\"")])),
        ],
        &[
            (1, "out", 3, "a"),
            (2, "out", 3, "b"),
            (3, "out", 4, "in"),
            (3, "out", 5, "in"),
            (5, "out", 6, "in"),
        ],
    );
    // Two disjoint 10 m squares on `a`; `b` covers x 5..25 across both.
    let a = Bundle(vec![
        polygon_data_item(PolygonData::rect([0.0, 0.0, 10.0, 10.0], 0.0)),
        polygon_data_item(PolygonData::rect([20.0, 0.0, 30.0, 10.0], 0.0)),
    ]);
    let b = Bundle::one(polygon_data_item(PolygonData::rect([5.0, -5.0, 25.0, 15.0], 0.0)));
    let r = run_inputs(&g, vec![("a", a), ("b", b)]).expect("run");
    let area: f64 = polys(&out(&r, "out")).iter().map(PolygonData::area).sum();
    assert!((area - 100.0).abs() < 1e-6, "two 5 x 10 m pieces remain: {area}");
    let rings = splines(&out(&r, "rings"));
    assert_eq!(rings.len(), 2, "one closed spline per ring");
    for s in &rings {
        assert!(s.closed);
        assert_eq!(s.points.len(), 4);
        assert!(s.points.iter().all(|p| p.mode == TangentMode::Linear));
        assert!(
            (s.length() - 30.0).abs() < 1e-3,
            "a 5 x 10 m ring: {}",
            s.length()
        );
    }
}

/// Spec 64 § 5: `repeat(seq, n)` places exactly `n` copies; with `Stretch`, a leftover the
/// stretch range cannot absorb becomes gaps, so every module still fits on the spline.
#[test]
fn repeat_n_places_exactly_n_copies() {
    let fence = Bundle::one(spline_data_item(line([0.0; 3], [21.0, 0.0, 0.0])));
    let r = run_inputs(
        &modules_graph("post repeat(panel post, 3) end"),
        vec![("in", fence)],
    )
    .expect("run");
    let names = module_names(&r);
    assert_eq!(names.iter().filter(|n| *n == "panel").count(), 3, "{names:?}");
    assert_eq!(names.len(), 8, "{names:?}");
    assert_eq!(names.last().map(String::as_str), Some("end"));
    let b = out(&r, "out");
    let p = b.0[0].as_points().expect("points");
    let (Some(regia_procgen::AttrColumn::Float(lens)), Some(regia_procgen::AttrColumn::Float(starts))) =
        (p.attrs.get("length_m"), p.attrs.get("distance_m"))
    else {
        panic!("length_m / distance_m missing");
    };
    for (n, l) in names.iter().zip(lens.iter()) {
        if n == "panel" {
            assert!(
                *l <= 2.0 * 1.15 + 1e-9,
                "panels stay within their stretch range: {l}"
            );
        }
    }
    let k = starts.len() - 1;
    assert!(
        starts[k] + lens[k] <= 21.0 + 1e-3,
        "the last module ends on the spline"
    );
    for k in 1..starts.len() {
        assert!(
            starts[k] >= starts[k - 1] + lens[k - 1] - 1e-9,
            "modules do not overlap"
        );
    }
}

/// Spec 64 § 4 `spline.from_points` with `order: nearest_chain`: greedy nearest neighbor from
/// the first point; `mode: Linear` sets every point's mode.
#[test]
fn from_points_nearest_chain_orders_greedily_from_the_first_point() {
    let mut ps = regia_procgen::PointSet::new();
    for x in [0.0f32, 30.0, 10.0, 20.0] {
        ps.push(regia_procgen::data::Point {
            position: [x, 0.0, 0.0],
            ..regia_procgen::data::Point::default()
        });
    }
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (
                2,
                node(
                    "spline.from_points",
                    &[("order", "\"nearest_chain\""), ("mode", "\"Linear\"")],
                ),
            ),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let item = regia_procgen::DataItem::points(ps);
    let r = run_inputs(&g, vec![("in", Bundle::one(item.clone()))]).expect("run");
    let s = &splines(&out(&r, "out"))[0];
    let xs: Vec<f32> = s.points.iter().map(|p| p.position.x).collect();
    assert_eq!(xs, vec![0.0, 10.0, 20.0, 30.0]);
    assert!(s.points.iter().all(|p| p.mode == TangentMode::Linear));
    assert!((s.length() - 30.0).abs() < 1e-4);
    // `order: index` keeps the input order.
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node("spline.from_points", &[])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let r = run_inputs(&g, vec![("in", Bundle::one(item))]).expect("run");
    let xs: Vec<f32> = splines(&out(&r, "out"))[0]
        .points
        .iter()
        .map(|p| p.position.x)
        .collect();
    assert_eq!(xs, vec![0.0, 30.0, 10.0, 20.0]);
}

/// Spec 64 § 4 `polygon.offset` at node level: a 10 m square grown by 1 m with round joins has
/// area 100 + 40 + π within 0.5 % (as spec test 6 for the library), and shrinking by 6 m
/// collapses it.
#[test]
fn polygon_offset_node_grows_and_collapses() {
    let run = |d: &str| {
        let g = graph(
            vec![
                (1, node("graph.input", &[("pin", "\"in\"")])),
                (
                    2,
                    node("polygon.offset", &[("distance_m", d), ("join", "\"round\"")]),
                ),
                (3, node("graph.output", &[])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        );
        let sq = Bundle::one(polygon_data_item(PolygonData::rect([0.0, 0.0, 10.0, 10.0], 0.0)));
        let r = run_inputs(&g, vec![("in", sq)]).expect("run");
        polys(&out(&r, "out")).iter().map(PolygonData::area).sum::<f64>()
    };
    let grown = run("1.0");
    let want = 140.0 + std::f64::consts::PI;
    assert!((grown - want).abs() / want < 0.005, "{grown} vs {want}");
    assert_eq!(run("-6.0"), 0.0, "a shrink past the half width leaves nothing");
}

/// v0.5.1 (deep-zai-procgen-runtime finding 1): counted repeats are sized before phase A builds
/// anything. A literal pattern that expands past the cap fails compile validation with
/// `pattern_too_large`; the same pattern read from an item-scope value (`pattern_from`, unknown at
/// compile time) fails the run with the same code instead of building every copy first.
#[test]
fn a_pattern_that_expands_too_far_is_refused_before_it_is_built() {
    // A literal pattern: compile validation names the code.
    let err = modules_graph("repeat(repeat(post, 1000000), 1000000)")
        .compile(&regia_procgen::ProcNodeRegistry::with_builtins())
        .expect_err("a pattern past the cap does not compile");
    assert!(format!("{err:?}").contains("pattern_too_large"), "{err:?}");

    // An item-scope pattern: the run fails with the code (500 000 copies would otherwise be built
    // and then found too long for the spline).
    let mut g = modules_graph("post");
    if let Some(n) = g.nodes.get_mut(&2) {
        *n = n.clone().with(
            "pattern_from",
            regia_procgen::SettingValue::Str("fence_pattern".into()),
        );
    }
    let mut item = spline_data_item(line([0.0; 3], [21.0, 0.0, 0.0]));
    item.scope.insert(
        "fence_pattern".into(),
        regia_procgen::data::AttrValue::Name("post repeat(panel, 500000) end".into()),
    );
    match run_inputs(&g, vec![("in", Bundle::one(item))]) {
        Err(RunError::Node { code, .. }) => assert_eq!(code, "pattern_too_large"),
        other => panic!("expected pattern_too_large, got {other:?}"),
    }
    // A runtime pattern within the cap still runs.
    let mut item = spline_data_item(line([0.0; 3], [21.0, 0.0, 0.0]));
    item.scope.insert(
        "fence_pattern".into(),
        regia_procgen::data::AttrValue::Name("post repeat(panel post) end".into()),
    );
    let r = run_inputs(&g, vec![("in", Bundle::one(item))]).expect("a sane runtime pattern runs");
    assert_eq!(module_names(&r).iter().filter(|n| *n == "panel").count(), 9);
}
