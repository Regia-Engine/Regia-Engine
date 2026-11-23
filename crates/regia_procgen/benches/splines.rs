// SPDX-License-Identifier: MPL-2.0
//! Spec 64 § 10 budgets (desktop reference, one core): sampling a 10 km spline at 1 m ≤ 5 ms;
//! `closest` ≤ 20 µs per query on a 200-point spline; a polygon union of two 10 000-vertex polygons
//! ≤ 30 ms; a road ribbon for 10 km of road ≤ 60 ms; `spawn.spline_mesh` 1 M output vertices ≤ 80 ms.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use regia_core::math::Vec3;
use regia_procgen::data::MeshData;
use regia_procgen::exec::{Frame, StaticSnapshot};
use regia_procgen::nodes::spline_data_item;
use regia_procgen::polygon::{BoolOp, PolygonData};
use regia_procgen::{
    Bundle, CancelToken, CompiledGraph, DataItem, Edge, Evaluator, NodeEntry, ProcGraph, ProcNodeRegistry,
    RunInputs, SettingValue,
};
use regia_scene::spline::{Spline, TangentMode};

/// A gently winding spline of `points` control points over `length_m`.
fn winding(points: usize, length_m: f32) -> Spline {
    let step = length_m / (points - 1) as f32;
    Spline::through((0..points).map(|i| {
        let x = i as f32 * step;
        // A triangle wave: no libm, and the same curve everywhere.
        let k = i % 8;
        let z = if k < 4 { k as f32 } else { (8 - k) as f32 } * step * 0.2;
        Vec3::new(x, 0.0, z)
    }))
}

fn node(ty: &str, settings: &[(&str, &str)]) -> NodeEntry {
    let mut n = NodeEntry::new(ty);
    for (k, v) in settings {
        match SettingValue::parse(v) {
            Ok(value) => n = n.with(k, value),
            Err(e) => panic!("setting {k}: {e}"),
        }
    }
    n
}

fn compile(g: &ProcGraph) -> CompiledGraph {
    match g.compile(&ProcNodeRegistry::with_builtins()) {
        Ok(c) => c,
        Err(e) => panic!("compile: {:?}", e.0),
    }
}

/// Runs a compiled graph with bundles on its `graph.input` pins.
fn run(graph: &CompiledGraph, snap: StaticSnapshot, pins: &[(&str, &Bundle)]) {
    let frame = Frame {
        graph_inputs: pins
            .iter()
            .map(|(k, b)| ((*k).to_string(), (*b).clone()))
            .collect(),
        ..Frame::default()
    };
    let inputs = RunInputs::new(Arc::new(snap), 3);
    if let Err(e) = Evaluator::new().run_frame(graph, &inputs, &CancelToken::new(), &frame) {
        panic!("run: {e}");
    }
}

fn pipeline(nodes: Vec<(u32, NodeEntry)>, edges: &[(u32, &str, u32, &str)]) -> CompiledGraph {
    compile(&ProcGraph {
        nodes: nodes.into_iter().collect(),
        edges: edges.iter().map(|(a, p, b, q)| Edge::new(*a, p, *b, q)).collect(),
        ..ProcGraph::default()
    })
}

fn sampling(c: &mut Criterion) {
    let curve = winding(1000, 10_000.0).curve();
    let len = curve.length();
    c.bench_function("sample_10km_spline_at_1m", |b| {
        b.iter(|| {
            let mut sum = 0.0;
            let mut d = 0.0;
            while d < len {
                sum += curve.at_distance(d).position.x;
                d += 1.0;
            }
            black_box(sum)
        });
    });
}

fn closest(c: &mut Criterion) {
    let curve = winding(200, 2_000.0).curve();
    c.bench_function("closest_on_200_points", |b| {
        let mut i = 0u32;
        b.iter(|| {
            i = i.wrapping_add(1);
            let x = (i % 2000) as f32;
            black_box(curve.closest(Vec3::new(x, 3.0, 40.0)))
        });
    });
}

fn union(c: &mut Criterion) {
    // Two 10 000-vertex combs sharing half their area.
    let comb = |shift: f64| -> Vec<[f64; 2]> {
        let mut ring: Vec<[f64; 2]> = (0..5000)
            .map(|i| [i as f64 * 0.1 + shift, if i % 2 == 0 { 0.0 } else { 0.05 }])
            .collect();
        ring.extend((0..5000).rev().map(|i| [i as f64 * 0.1 + shift, 100.0]));
        ring
    };
    let (a, b_poly) = match (
        PolygonData::new([0.0; 3], &[comb(0.0)]),
        PolygonData::new([0.0; 3], &[comb(250.0)]),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        _ => panic!("the combs are valid polygons"),
    };
    c.bench_function("polygon_union_10k_vertices", |b| {
        b.iter(|| black_box(a.boolean(&b_poly, BoolOp::Union).map(|p| p.area())));
    });
}

fn ribbon(c: &mut Criterion) {
    const PROFILE: &str = "RoadProfile(width_m: 7.0, shoulders_m: (0.5, 0.5), samples_per_m: 0.5)";
    let g = pipeline(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (2, node("road.ribbon", &[("profile", PROFILE)])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "splines"), (2, "out", 3, "in")],
    );
    let road = Bundle::one(spline_data_item(winding(1000, 10_000.0)));
    c.bench_function("road_ribbon_10km", |b| {
        b.iter(|| run(&g, StaticSnapshot::default(), &[("road", &road)]));
    });
}

fn spline_mesh(c: &mut Criterion) {
    // 10 000 pieces of a 100-vertex strip: 1 M output vertices.
    let strip = 50;
    let mut positions = Vec::with_capacity(2 * strip);
    for i in 0..strip {
        let x = i as f32 / (strip - 1) as f32;
        positions.extend([[x, 0.0, -0.5], [x, 0.0, 0.5]]);
    }
    let normals = vec![[0.0, 1.0, 0.0]; positions.len()];
    let mut indices = Vec::new();
    for i in 0..(strip as u32 - 1) {
        let v = i * 2;
        indices.extend([v, v + 1, v + 2, v + 1, v + 3, v + 2]);
    }
    let mesh = MeshData {
        asset: "strip".to_string(),
        positions,
        normals,
        indices,
    };
    let meshes: BTreeMap<String, Arc<MeshData>> = [("strip".to_string(), Arc::new(mesh))].into();
    let g = pipeline(
        vec![
            (1, node("graph.input", &[("pin", "\"points\"")])),
            (2, node("graph.input", &[("pin", "\"road\"")])),
            (
                3,
                node(
                    "spawn.spline_mesh",
                    &[("segments_per_m", "0.0"), ("smooth_normals", "false")],
                ),
            ),
            (4, node("graph.output", &[])),
        ],
        &[(1, "out", 3, "in"), (2, "out", 3, "spline"), (3, "out", 4, "in")],
    );
    let mut road = winding(1000, 10_000.0);
    road.points.iter_mut().for_each(|p| p.mode = TangentMode::Auto);
    let spline = Bundle::one(spline_data_item(road));
    let mut points = regia_procgen::PointSet::new();
    let n = 10_000;
    for _ in 0..n {
        points.push(regia_procgen::Point::default());
    }
    use regia_procgen::AttrColumn;
    points.attrs.insert(
        "mesh".to_string(),
        AttrColumn::Asset(vec!["strip".to_string(); n].into()),
    );
    points.attrs.insert(
        "distance_m".to_string(),
        AttrColumn::Float((0..n).map(|i| i as f64).collect()),
    );
    points
        .attrs
        .insert("length_m".to_string(), AttrColumn::Float(vec![1.0; n].into()));
    let points = Bundle::one(DataItem::points(points));
    let snap = StaticSnapshot {
        meshes,
        ..StaticSnapshot::default()
    };
    c.bench_function("spline_mesh_1m_vertices", |b| {
        b.iter(|| run(&g, snap.clone(), &[("points", &points), ("road", &spline)]));
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = sampling, closest, union, ribbon, spline_mesh
}
criterion_main!(benches);
