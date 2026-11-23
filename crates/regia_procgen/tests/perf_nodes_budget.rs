// SPDX-License-Identifier: MPL-2.0
//! Spec 63 § 15 per-node budgets at 1 M points (desktop reference, one core): `points.jitter`
//! ≤ 30 ms, `filter.range` ≤ 10 ms, `density.noise` (3 octaves) ≤ 60 ms. Hand-run timing
//! (`docs/06` § 9), never part of a normal test run:
//! `cargo test -p regia_procgen --release --test perf_nodes_budget -- --ignored --nocapture`.
//! Prints `perf-node copy_floor: ...` (one deep copy of the input, the floor of an in-place node), then
//! `perf-node <name>: serial p50= ... (run on-cpu p50= ...) 8 threads p50= ...` and asserts the
//! one-core budgets on the serial schedule (wall time; the on-CPU figure shows how much of it was run-queue wait). Numbers from shared hosts (the build server) are indicative only.
//!
//! Recorded on an idle i7-8700K (Windows, release, one core, p50), `integ/v035` → ADR-2724:
//! `points.jitter` 145 → 92 ms, `filter.range` 75 → 20 ms, `density.noise` 112 → 35 ms; so this test
//! still fails there on jitter and the range filter (spec 63 addendum). Numbers from the shared lane
//! host (the build server) are indicative only.

mod common;

use std::sync::Arc;

use common::{compile, graph, node};
use regia_procgen::data::Payload;
use regia_procgen::exec::{Frame, Parallelism, StaticSnapshot};
use regia_procgen::{Bundle, CancelToken, CompiledGraph, Evaluator, RunInputs};

const RUNS: usize = 9;

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

fn single(ty: &str, settings: &[(&str, &str)]) -> CompiledGraph {
    let first_out = if ty.starts_with("filter.") {
        "inside"
    } else {
        "out"
    };
    compile(&graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (2, node(ty, settings)),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "in"), (2, first_out, 3, "in")],
    ))
}

/// On-CPU time of the calling thread in nanoseconds (`/proc/thread-self/schedstat`, Linux), so a
/// shared host's run-queue wait can be told apart from the work; `None` elsewhere.
fn thread_cpu_ns() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    s.split_whitespace().next()?.parse().ok()
}

/// `points` in a fixed random order.
fn shuffled(points: &Bundle) -> Bundle {
    let mut out = points.clone();
    for item in &mut out.0 {
        if let Payload::Points(p) = &mut item.payload {
            let mut order: Vec<usize> = (0..p.len()).collect();
            let mut x = 0x9E37_79B9u32;
            for i in (1..order.len()).rev() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                order.swap(i, x as usize % (i + 1));
            }
            *p = p.select(&order);
        }
    }
    out
}

/// p50 of the node's own report time over [`RUNS`] runs, milliseconds, and the p50 of the calling
/// thread's on-CPU time per whole run (meaningful on the serial schedule only; `NaN` off Linux).
fn node_p50(ev: &Evaluator, c: &CompiledGraph, points: &Bundle) -> (f64, f64) {
    let frame = Frame {
        graph_inputs: [("in".to_string(), points.clone())].into_iter().collect(),
        ..Frame::default()
    };
    let inputs = RunInputs::new(Arc::new(StaticSnapshot::default()), 3);
    let mut ms = Vec::with_capacity(RUNS);
    let mut cpu = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let before = thread_cpu_ns();
        let r = ev
            .run_frame(c, &inputs, &CancelToken::new(), &frame)
            .unwrap_or_else(|e| panic!("run: {e}"));
        let after = thread_cpu_ns();
        assert!(
            r.report.node(2).is_some_and(|l| l.elements > 0),
            "the node ran on the points"
        );
        ms.push(r.report.node(2).map_or(0.0, |l| l.ms));
        cpu.push(match (before, after) {
            (Some(b), Some(a)) => a.saturating_sub(b) as f64 / 1.0e6,
            _ => f64::NAN,
        });
    }
    ms.sort_by(f64::total_cmp);
    cpu.sort_by(f64::total_cmp);
    (ms[RUNS / 2], cpu[RUNS / 2])
}

#[test]
#[ignore = "hand-run timing (docs/06 § 9)"]
fn perf_node_budgets_at_one_million_points() {
    let points = million_points();
    // The floor every in-place node pays: one deep copy of the 1 M-point input (its output set).
    let mut copy = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = std::time::Instant::now();
        let c = std::hint::black_box(points.clone());
        copy.push(started.elapsed().as_secs_f64() * 1.0e3);
        drop(c);
    }
    copy.sort_by(f64::total_cmp);
    println!(
        "perf-node copy_floor: one deep copy of the 1 M-point input p50={:.2}ms",
        copy[RUNS / 2]
    );
    let serial = Evaluator::new().with_parallelism(Parallelism::Serial);
    let pool8 = Evaluator::new().with_parallelism(Parallelism::threads(8));
    let cases: [(&str, CompiledGraph, f64); 3] = [
        (
            "points_jitter",
            single(
                "points.jitter",
                &[
                    ("offset", "[(-0.2, 0.0, -0.2), (0.2, 0.5, 0.2)]"),
                    ("rotate_deg", "[(0.0, 0.0, 0.0), (0.0, 360.0, 0.0)]"),
                    ("scale", "[(0.6, 0.6, 0.6), (1.4, 1.4, 1.4)]"),
                ],
            ),
            30.0,
        ),
        (
            "filter_range",
            single(
                "filter.range",
                &[("attr", "\"position:x\""), ("min", "-100.0"), ("max", "100.0")],
            ),
            10.0,
        ),
        (
            "density_noise_3oct",
            single("density.noise", &[("scale_m", "32.0"), ("octaves", "3")]),
            60.0,
        ),
    ];
    // The same nodes on the points in random order, for scale: no budget, but noise then misses its
    // per-cell cache and the range filter its run copies, so this is their worst case.
    let shuffled = shuffled(&points);
    for (name, c, _) in &cases {
        let (one, _) = node_p50(&serial, c, &shuffled);
        println!("perf-node {name}_shuffled: serial p50={one:.2}ms");
    }
    // Jitter that only offsets (three draws per point, no rotation), for scale: `cases` draws seven
    // values and composes a rotation per point.
    let offset_only = single(
        "points.jitter",
        &[("offset", "[(-0.2, 0.0, -0.2), (0.2, 0.5, 0.2)]")],
    );
    let (one, _) = node_p50(&serial, &offset_only, &points);
    println!("perf-node points_jitter_offset_only: serial p50={one:.2}ms");
    let mut over = Vec::new();
    for (name, c, budget) in &cases {
        let (one, one_cpu) = node_p50(&serial, c, &points);
        let (eight, _) = node_p50(&pool8, c, &points);
        println!(
            "perf-node {name}: serial p50={one:.2}ms (run on-cpu p50={one_cpu:.2}ms) 8 threads p50={eight:.2}ms budget={budget}ms"
        );
        if one > *budget {
            over.push(format!("{name} {one:.2} ms > {budget} ms"));
        }
    }
    assert!(
        over.is_empty(),
        "one-core budgets missed (spec 63 § 15): {over:?}"
    );
}
