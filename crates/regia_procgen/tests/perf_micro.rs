// SPDX-License-Identifier: MPL-2.0
//! Hand-run timing for the procedural evaluator (spec 62 § 12, spec 63 § 15; `docs/06` § 9).
//! `#[ignore]`: run with
//! `cargo test -p regia_procgen --release --test perf_micro -- --ignored --nocapture`.
//! Prints `perf-micro <name>: n= p50= p99= max=` and asserts only that the work happened.

mod common;

use std::time::Instant;

use common::{box_shape, compile, graph, node, snapshot};
use regia_procgen::data::GroundData;
use regia_procgen::{CancelToken, Evaluator, ProcNodeRegistry, RunInputs};

fn report(name: &str, mut ms: Vec<f64>) {
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    println!(
        "perf-micro {name}: n={} p50={:.2}ms p99={:.2}ms max={:.2}ms",
        ms.len(),
        at(0.5),
        at(0.99),
        ms.last().copied().unwrap_or(0.0)
    );
}

/// 1 km × 1 km ground at 2 m spacing (250 000 candidates), slope and altitude filters, jitter, prune,
/// spawn: the spec 62 § 12 single-core pipeline.
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_square_km_pipeline() {
    let n = 501u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let (c, r) = ((i % n) as f32 - 250.0, (i / n) as f32 - 250.0);
            0.0004 * c * c + 0.0003 * r * r + 0.01 * c
        })
        .collect();
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 1000.0, 1.0, 0.0).expect("map");
    let ground = GroundData::capture(&map, [-500.0, -500.0, 500.0, 500.0], 2);
    let snap = snapshot(box_shape([0.0; 3], [500.0, 200.0, 500.0], 0.0), Some(ground));
    let g = graph(
        vec![
            (1, node("in.self", &[])),
            (
                2,
                node("sample.ground", &[("spacing_m", "2.0"), ("align", "normal")]),
            ),
            (
                3,
                node("density.facing", &[("lo_deg", "5.0"), ("hi_deg", "20.0")]),
            ),
            (
                4,
                node(
                    "filter.range",
                    &[("attr", "\"position:y\""), ("min", "1.0"), ("max", "200.0")],
                ),
            ),
            (
                5,
                node(
                    "points.jitter",
                    &[
                        ("rotate_deg", "[(0.0, 0.0, 0.0), (0.0, 360.0, 0.0)]"),
                        ("scale", "[(0.5, 0.5, 0.5), (1.5, 1.5, 1.5)]"),
                    ],
                ),
            ),
            (6, node("points.prune", &[])),
            (
                7,
                node(
                    "spawn.mesh",
                    &[("entries", "[(mesh: \"a\"), (mesh: \"b\", weight: 2.0)]")],
                ),
            ),
        ],
        &[
            (1, "out", 2, "region"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "inside", 5, "in"),
            (5, "out", 6, "in"),
            (6, "out", 7, "in"),
        ],
    );
    let c = compile(&g);
    let inputs = RunInputs::new(snap, 3);
    let mut ms = Vec::new();
    let mut per_node: Vec<Vec<f64>> = Vec::new();
    let mut instances = 0;
    for _ in 0..5 {
        let t = Instant::now();
        let r = Evaluator::new()
            .run(&c, &inputs, &CancelToken::new())
            .expect("run");
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        instances = r.output.instance_count();
        for (k, line) in r.report.nodes.iter().enumerate() {
            if per_node.len() <= k {
                per_node.push(Vec::new());
            }
            per_node[k].push(line.ms);
        }
        assert_eq!(r.report.nodes[1].elements, 250_000, "250 000 candidates");
    }
    assert!(instances > 1000, "spawned {instances}");
    report("square_km_pipeline", ms);
    for (k, v) in per_node.into_iter().enumerate() {
        report(&format!("square_km_pipeline.node{}", k + 1), v);
    }
}

/// Compile a 200-node chain graph (spec 62 § 12: ≤ 2 ms desktop).
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_compile_200_nodes() {
    let mut nodes = vec![(1, node("gen.grid", &[]))];
    let mut edges = Vec::new();
    for k in 2..=200u32 {
        nodes.push((
            k,
            node(
                "points.jitter",
                &[("offset", "[(-1.0, 0.0, -1.0), (1.0, 0.0, 1.0)]")],
            ),
        ));
        edges.push((k - 1, "out", k, "in"));
    }
    let g = graph(nodes, &edges);
    let reg = ProcNodeRegistry::with_builtins();
    let mut ms = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        let c = g.compile(&reg).expect("compile");
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(c.nodes.len(), 200);
    }
    report("compile_200_nodes", ms);
}

/// One million points through `points.jitter` and `filter.range` (spec 63 § 15).
#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_micro_million_points_jitter_filter() {
    let g = graph(
        vec![
            (1, node("gen.grid", &[("count", "(1000.0, 1.0, 1000.0)")])),
            (
                2,
                node(
                    "points.jitter",
                    &[("offset", "[(-0.4, 0.0, -0.4), (0.4, 0.0, 0.4)]")],
                ),
            ),
            (
                3,
                node(
                    "filter.range",
                    &[("attr", "\"position:x\""), ("min", "-100.0"), ("max", "100.0")],
                ),
            ),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    );
    let c = compile(&g);
    let inputs = RunInputs::new(snapshot(box_shape([0.0; 3], [10.0; 3], 0.0), None), 1);
    let (mut jitter, mut filter) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        let r = Evaluator::new()
            .run(&c, &inputs, &CancelToken::new())
            .expect("run");
        jitter.push(r.report.nodes[1].ms);
        filter.push(r.report.nodes[2].ms);
        assert_eq!(r.report.nodes[1].elements, 1_000_000);
    }
    report("jitter_1m", jitter);
    report("filter_range_1m", filter);
}
