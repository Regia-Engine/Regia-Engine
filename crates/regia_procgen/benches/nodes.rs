// SPDX-License-Identifier: MPL-2.0
//! Spec 63 § 15 per-node budgets (desktop reference, one core, 1 M input points): `points.jitter`
//! ≤ 30 ms, `filter.range` ≤ 10 ms, `density.noise` (3 octaves) ≤ 60 ms, `points.prune` ≤ 250 ms,
//! `spawn.mesh` ≤ 40 ms, region tests against 10 000 boxes ≤ 150 ms.
//!
//! Each case feeds a prepared 1 M-point bundle through `graph.input` → node → `graph.output` and
//! times only the node: criterion's `iter_custom` sums the node's own report time
//! (`NodeReport::ms`), so the input copy and the output pin are not counted. Cases run on the serial
//! schedule (the budgets are one core) and, for the chunked nodes, on an 8-thread pool.
//! `cargo bench -p regia_procgen --bench nodes`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use regia_procgen::exec::{Frame, Parallelism, StaticSnapshot};
use regia_procgen::{
    Bundle, CancelToken, CompiledGraph, Edge, Evaluator, NodeEntry, ProcGraph, ProcNodeRegistry, RunInputs,
    SettingValue,
};

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

fn graph(nodes: Vec<(u32, NodeEntry)>, edges: &[(u32, &str, u32, &str)]) -> ProcGraph {
    ProcGraph {
        nodes: nodes.into_iter().collect(),
        edges: edges
            .iter()
            .map(|(a, ap, b, bp)| Edge::new(*a, ap, *b, bp))
            .collect(),
        ..ProcGraph::default()
    }
}

/// A 1000 × 1000 grid at 0.5 m (1 M points) with a 0.4 m box per point.
fn million_points() -> Bundle {
    let g = graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(1000.0, 1.0, 1000.0)"), ("step_m", "(0.5, 1.0, 0.5)")],
                ),
            ),
            (
                2,
                node(
                    "points.bounds",
                    &[("min", "(-0.2, -0.2, -0.2)"), ("max", "(0.2, 0.2, 0.2)")],
                ),
            ),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let r = Evaluator::new()
        .run(
            &compile(&g),
            &RunInputs::new(Arc::new(StaticSnapshot::default()), 3),
            &CancelToken::new(),
        )
        .unwrap_or_else(|e| panic!("grid: {e}"));
    r.output.graph_outputs.get("out").cloned().unwrap_or_default()
}

/// `graph.input` → `ty` → `graph.output`, plus extra pins wired from other graph inputs.
fn single(ty: &str, settings: &[(&str, &str)], extra_pins: &[&str]) -> CompiledGraph {
    let mut nodes = vec![
        (1, node("graph.input", &[("pin", "\"in\"")])),
        (2, node(ty, settings)),
        (3, node("graph.output", &[("pin", "\"out\"")])),
    ];
    // Filters name their first output `inside`.
    let first_out = if ty.starts_with("filter.") {
        "inside"
    } else {
        "out"
    };
    let mut edges = vec![(1, "out", 2, "in"), (2, first_out, 3, "in")];
    for (k, pin) in extra_pins.iter().enumerate() {
        let id = 10 + k as u32;
        nodes.push((id, node("graph.input", &[("pin", &format!("\"{pin}\""))])));
        edges.push((id, "out", 2, *pin));
    }
    graph(nodes, &edges)
        .compile(&ProcNodeRegistry::with_builtins())
        .unwrap_or_else(|e| panic!("{ty}: {:?}", e.0))
}

/// The node's own time over `iters` runs.
fn node_time(ev: &Evaluator, c: &CompiledGraph, pins: &[(&str, &Bundle)], iters: u64) -> Duration {
    let frame = Frame {
        graph_inputs: pins
            .iter()
            .map(|(k, b)| ((*k).to_string(), (*b).clone()))
            .collect(),
        ..Frame::default()
    };
    let inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), 3);
    let mut total = Duration::ZERO;
    for _ in 0..iters {
        let r = ev
            .run_frame(c, &inputs, &CancelToken::new(), &frame)
            .unwrap_or_else(|e| panic!("run: {e}"));
        let ms = r.report.node(2).map_or(0.0, |l| l.ms);
        total += Duration::from_secs_f64(ms / 1000.0);
        black_box(r.output_hash);
    }
    total
}

const JITTER: &[(&str, &str)] = &[
    ("offset", "[(-0.2, 0.0, -0.2), (0.2, 0.5, 0.2)]"),
    ("rotate_deg", "[(0.0, 0.0, 0.0), (0.0, 360.0, 0.0)]"),
    ("scale", "[(0.6, 0.6, 0.6), (1.4, 1.4, 1.4)]"),
];
const RANGE: &[(&str, &str)] = &[("attr", "\"position:x\""), ("min", "-100.0"), ("max", "100.0")];
const NOISE: &[(&str, &str)] = &[("scale_m", "32.0"), ("octaves", "3")];

/// One bench case: name, graph, graph-input pins, whether it also runs on 8 threads.
type Case<'a> = (&'static str, CompiledGraph, Vec<(&'static str, &'a Bundle)>, bool);

fn bench_nodes(c: &mut Criterion) {
    let points = million_points();
    // 10 000 region boxes: a 100 × 100 grid at 5 m with 1 m boxes.
    let boxes = {
        let g = graph(
            vec![
                (
                    1,
                    node(
                        "gen.grid",
                        &[("count", "(100.0, 1.0, 100.0)"), ("step_m", "(5.0, 1.0, 5.0)")],
                    ),
                ),
                (
                    2,
                    node(
                        "points.bounds",
                        &[("min", "(-0.5, -5.0, -0.5)"), ("max", "(0.5, 5.0, 0.5)")],
                    ),
                ),
                (3, node("graph.output", &[("pin", "\"out\"")])),
            ],
            &[(1, "out", 2, "in"), (2, "out", 3, "in")],
        );
        Evaluator::new()
            .run(
                &compile(&g),
                &RunInputs::new(Arc::new(StaticSnapshot::default()), 3),
                &CancelToken::new(),
            )
            .map(|r| r.output.graph_outputs.get("out").cloned().unwrap_or_default())
            .unwrap_or_else(|e| panic!("boxes: {e}"))
    };
    let serial = Evaluator::new().with_parallelism(Parallelism::Serial);
    let pool8 = Evaluator::new().with_parallelism(Parallelism::threads(8));
    let cases: Vec<Case<'_>> = vec![
        (
            "points_jitter_1m",
            single("points.jitter", JITTER, &[]),
            vec![("in", &points)],
            true,
        ),
        (
            "filter_range_1m",
            single("filter.range", RANGE, &[]),
            vec![("in", &points)],
            true,
        ),
        (
            "density_noise_3oct_1m",
            single("density.noise", NOISE, &[]),
            vec![("in", &points)],
            true,
        ),
        (
            "points_prune_1m",
            single("points.prune", &[], &[]),
            vec![("in", &points)],
            false,
        ),
        (
            "spawn_mesh_1m",
            single(
                "spawn.mesh",
                &[(
                    "entries",
                    "[(mesh: \"meshes/a.glb\", weight: 1.0), (mesh: \"meshes/b.glb\", weight: 1.0)]",
                )],
                &[],
            ),
            vec![("in", &points)],
            false,
        ),
        (
            // Without the `mesh` attribute, whose per-point `String` is most of the default's cost.
            "spawn_mesh_no_attr_1m",
            single(
                "spawn.mesh",
                &[
                    (
                        "entries",
                        "[(mesh: \"meshes/a.glb\", weight: 1.0), (mesh: \"meshes/b.glb\", weight: 1.0)]",
                    ),
                    ("write_attr", "\"\""),
                ],
                &[],
            ),
            vec![("in", &points)],
            false,
        ),
        (
            "region_subtract_10k_boxes_1m",
            single("region.subtract", &[("mode", "cut")], &["minus"]),
            vec![("in", &points), ("minus", &boxes)],
            false,
        ),
    ];
    let mut group = c.benchmark_group("procgen_nodes");
    group.sample_size(10);
    for (name, graph, pins, chunked) in &cases {
        group.bench_function(format!("{name}_serial"), |b| {
            b.iter_custom(|n| node_time(&serial, graph, pins, n))
        });
        if *chunked {
            group.bench_function(format!("{name}_8_threads"), |b| {
                b.iter_custom(|n| node_time(&pool8, graph, pins, n))
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_nodes);
criterion_main!(benches);
