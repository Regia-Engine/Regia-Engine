// SPDX-License-Identifier: MPL-2.0
//! Parallel evaluation (spec 62 § 5.1, ADR-2724): every schedule gives the serial run's bytes.
//!
//! Each graph runs serially (the reference) and on dedicated pools of 1, 2 and 8 threads; the output,
//! its hash, the report lines (minus wall times) and errors must be equal, and no run may fall back
//! to the serial schedule.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::*;
use parking_lot::Mutex;
use regia_procgen::data::GroundData;
use regia_procgen::exec::{NodeReport, Parallelism, ProcCache, RunLimitsOverride};
use regia_procgen::{
    CancelToken, CompiledGraph, Evaluator, ProcGraph, ProcNodeRegistry, RunError, RunInputs, RunResult,
};

const HALF: [f32; 3] = [100.0, 64.0, 100.0];

/// The thread counts of the brief: explicit pools, so the test never depends on the host's cores.
const THREADS: [usize; 3] = [1, 2, 8];

fn bowl_inputs(seed: u32) -> RunInputs {
    let map = bowl_map(bowl_heights());
    let bounds = box_shape([0.0; 3], HALF, 0.0);
    let ground = GroundData::capture(&map, [-HALF[0], -HALF[2], HALF[0], HALF[2]], 2);
    RunInputs::new(snapshot(bounds, Some(ground)), seed)
}

/// A report line without its wall time.
fn line_key(l: &NodeReport) -> NodeReport {
    NodeReport { ms: 0.0, ..l.clone() }
}

/// What must match between schedules.
#[derive(Debug, PartialEq)]
enum Outcome {
    Ok {
        hash: [u8; 32],
        output: Box<regia_procgen::exec::RunOutput>,
        lines: Vec<NodeReport>,
        cache_hits: usize,
        roads: BTreeMap<String, regia_procgen::exec::RoadReport>,
        warnings: Vec<String>,
    },
    Err(RunError),
}

fn outcome(r: Result<RunResult, RunError>) -> Outcome {
    match r {
        Ok(r) => Outcome::Ok {
            hash: r.output_hash,
            output: Box::new(r.output),
            lines: r.report.nodes.iter().map(line_key).collect(),
            cache_hits: r.report.cache_hits,
            roads: r.report.roads.clone(),
            warnings: r.report.warning_codes(),
        },
        Err(e) => Outcome::Err(e),
    }
}

/// Runs `c` serially and at every [`THREADS`] count; asserts equal outcomes and no fallback.
/// Returns the serial outcome.
fn assert_schedules_agree(what: &str, c: &CompiledGraph, inputs: &RunInputs) -> Outcome {
    let serial = Evaluator::new().with_parallelism(Parallelism::Serial);
    let reference = outcome(serial.run(c, inputs, &CancelToken::new()));
    for t in THREADS {
        let ev = Evaluator::new().with_parallelism(Parallelism::threads(t));
        let got = ev.run(c, inputs, &CancelToken::new());
        if let Ok(r) = &got {
            assert_eq!(r.report.threads, t, "{what}: the report names the pool size");
        }
        let got = outcome(got);
        assert_eq!(got, reference, "{what}: {t} threads differ from the serial run");
        assert_eq!(
            ev.serial_fallbacks(),
            0,
            "{what}: {t} threads fell back to the serial schedule"
        );
    }
    reference
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `.pgraph.ron` of the procgen samples, the starter set and the agent fuzz fixtures.
fn fixture_graphs() -> Vec<PathBuf> {
    let root = repo_root();
    let mut dirs = vec![
        root.join("crates/regia_procgen/starter"),
        root.join("crates/regia_agent/src/fuzz_targets/fixtures"),
    ];
    if let Ok(rd) = std::fs::read_dir(root.join("samples")) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("procgen") {
                dirs.push(e.path().join("assets"));
            }
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.to_string_lossy().ends_with(".pgraph.ron") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_fixture_graph_runs_identically_at_1_2_and_8_threads() {
    let files = fixture_graphs();
    assert!(
        files.len() >= 9,
        "found the sample, starter and fuzz graphs: {files:?}"
    );
    let registry = ProcNodeRegistry::with_builtins();
    let mut ran = 0;
    let mut produced = 0;
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            panic!("read {}", f.display())
        };
        let g = match ProcGraph::from_ron(&text) {
            Ok(g) => g,
            Err(e) => panic!("{} parses: {e}", f.display()),
        };
        // Plant graphs exist only with the `pipeline` feature (spec 68 § 2).
        let Ok(c) = g.compile(&registry) else { continue };
        let dir = f.parent().map(Path::to_path_buf).unwrap_or_default();
        let graphs =
            regia_procgen::graph::collect_subgraphs(&g, &|p| std::fs::read_to_string(dir.join(p)).ok());
        for seed in [7, 1234] {
            let inputs = bowl_inputs(seed).with_graphs(graphs.clone());
            let o = assert_schedules_agree(&f.display().to_string(), &c, &inputs);
            ran += 1;
            if let Outcome::Ok { output, .. } = &o
                && output.hash() != regia_procgen::exec::RunOutput::default().hash()
            {
                produced += 1;
            }
        }
    }
    assert!(ran >= 12, "at least six graphs compiled and ran twice: {ran}");
    assert!(
        produced >= 2,
        "some fixture runs produce output over the bowl: {produced}"
    );
}

/// Eight independent ground samplers with their own filters and jitters, each spawning, merged into
/// exports: many ready nodes per wave, several writers in compiled order.
fn fan_out_graph() -> ProcGraph {
    let mut nodes = vec![(1, node("in.self", &[]))];
    let mut edges: Vec<(u32, String, u32, String)> = Vec::new();
    for b in 0..8u32 {
        let base = 10 + b * 10;
        let spacing = format!("{}.0", 3 + b);
        nodes.push((
            base,
            node(
                "sample.ground",
                &[("spacing_m", spacing.as_str()), ("jitter", "0.7")],
            ),
        ));
        nodes.push((
            base + 1,
            node("density.noise", &[("scale_m", "24.0"), ("octaves", "3")]),
        ));
        nodes.push((
            base + 2,
            node(
                "filter.range",
                &[("attr", "\"density\""), ("min", "0.4"), ("max", "1.0")],
            ),
        ));
        nodes.push((
            base + 3,
            node(
                "points.jitter",
                &[
                    ("offset", "[(-0.5, 0.0, -0.5), (0.5, 0.0, 0.5)]"),
                    ("rotate_deg", "[(0.0, 0.0, 0.0), (0.0, 360.0, 0.0)]"),
                    ("scale", "[(0.7, 0.7, 0.7), (1.3, 1.3, 1.3)]"),
                ],
            ),
        ));
        let mesh =
            format!("[(mesh: \"meshes/m{b}.glb\", weight: 1.0), (mesh: \"meshes/shared.glb\", weight: 1.0)]");
        nodes.push((base + 4, node("spawn.mesh", &[("entries", mesh.as_str())])));
        edges.push((1, "out".into(), base, "region".into()));
        edges.push((base, "out".into(), base + 1, "in".into()));
        edges.push((base + 1, "out".into(), base + 2, "in".into()));
        edges.push((base + 2, "inside".into(), base + 3, "in".into()));
        edges.push((base + 3, "out".into(), base + 4, "in".into()));
        edges.push((base + 4, "out".into(), 200, "in".into()));
        let name = format!("\"branch{b}\"");
        nodes.push((base + 5, node("out.points", &[("name", name.as_str())])));
        edges.push((base + 2, "outside".into(), base + 5, "in".into()));
    }
    nodes.push((200, node("points.merge", &[])));
    nodes.push((201, node("graph.output", &[("pin", "\"out\"")])));
    edges.push((200, "out".into(), 201, "in".into()));
    let e: Vec<(u32, &str, u32, &str)> = edges
        .iter()
        .map(|(a, ap, b, bp)| (*a, ap.as_str(), *b, bp.as_str()))
        .collect();
    graph(nodes, &e)
}

#[test]
fn a_wide_graph_with_many_writers_matches_the_serial_run() {
    let c = compile(&fan_out_graph());
    for seed in [1, 7, 99] {
        let o = assert_schedules_agree("fan-out", &c, &bowl_inputs(seed));
        let Outcome::Ok { output, .. } = o else {
            panic!("fan-out runs: {o:?}")
        };
        assert!(output.instance_count() > 1000, "every branch spawns");
        assert_eq!(output.points.len(), 8, "every branch exports its outside points");
    }
}

#[test]
fn the_first_failing_node_in_compiled_order_is_reported_at_every_thread_count() {
    // Two branches fail (a NaN on the later-compiled one, a failed check on the earlier); the serial
    // run reports the earlier, so every schedule must.
    let g = graph(
        vec![
            (1, node("gen.grid", &[])),
            (2, node("attr.set", &[("target", "\"w\""), ("value", "-1.0")])),
            (3, node("attr.math", &[("op", "sqrt"), ("a", "\"w\"")])),
            (4, node("gen.grid", &[])),
            (5, node("debug.check", &[("min_count", "100000")])),
            (6, node("graph.output", &[])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (4, "out", 5, "in"),
            (3, "out", 6, "in"),
        ],
    );
    let c = compile(&g);
    let o = assert_schedules_agree("two failures", &c, &bowl_inputs(3));
    let Outcome::Err(RunError::Node { node, .. }) = o else {
        panic!("the run fails: {o:?}")
    };
    // Compiled order decides which failure is first; the serial run named it and every pool agrees.
    assert!(node == 3 || node == 5, "one of the two failing nodes: {node}");
}

#[test]
fn a_cumulative_limit_fails_at_the_same_node_on_every_schedule() {
    let c = compile(&fan_out_graph());
    for cap in [3_000usize, 15_000, 40_000] {
        let mut inputs = bowl_inputs(5);
        inputs.limits = inputs.limits.with_override(&RunLimitsOverride {
            total_elements: Some(cap),
            ..RunLimitsOverride::default()
        });
        let o = assert_schedules_agree(&format!("total_elements {cap}"), &c, &inputs);
        let Outcome::Err(e) = o else {
            panic!("cap {cap} is crossed: {o:?}")
        };
        assert_eq!(e.code(), "limit_exceeded");
    }
}

#[test]
fn a_shared_cache_serves_parallel_runs_the_serial_bytes() {
    let c = compile(&fan_out_graph());
    let inputs = bowl_inputs(11);
    let reference = Evaluator::new()
        .with_parallelism(Parallelism::Serial)
        .run(&c, &inputs, &CancelToken::new())
        .map(|r| r.output_hash);
    let cache = Arc::new(Mutex::new(ProcCache::new(256 << 20)));
    for t in THREADS {
        let ev = Evaluator::with_cache(cache.clone()).with_parallelism(Parallelism::threads(t));
        for _ in 0..2 {
            let r = ev.run(&c, &inputs, &CancelToken::new()).map(|r| r.output_hash);
            assert_eq!(r, reference, "{t} threads with a shared cache");
        }
    }
    assert!(cache.lock().stats().0 > 0, "later runs hit the cache");
}

#[test]
fn a_tripped_token_cancels_a_parallel_run() {
    let c = compile(&fan_out_graph());
    let token = CancelToken::new();
    token.cancel();
    for t in THREADS {
        let ev = Evaluator::new().with_parallelism(Parallelism::threads(t));
        let r = ev.run(&c, &bowl_inputs(1), &token);
        assert!(
            matches!(r, Err(RunError::Cancelled)),
            "{t} threads: {:?}",
            r.map(|r| r.output_hash)
        );
    }
}

#[test]
fn the_default_schedule_is_the_compute_pool_and_zero_threads_is_serial() {
    // A run of the suite on a forced schedule (`REGIA_PROCGEN_THREADS`) has another default.
    match std::env::var("REGIA_PROCGEN_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        None => assert!(matches!(Evaluator::new().parallelism(), Parallelism::Compute)),
        Some(n) => assert_eq!(Evaluator::new().parallelism().thread_count(), n.max(1)),
    }
    assert!(matches!(Parallelism::threads(0), Parallelism::Serial));
    assert_eq!(Parallelism::Serial.thread_count(), 1);
    assert_eq!(Parallelism::threads(3).thread_count(), 3);
}

/// The child half of `the_env_thread_pool_is_shared_between_evaluators`: a no-op unless the
/// parent set `REGIA_PROCGEN_THREADS`.
#[test]
fn env_pool_child() {
    if std::env::var("REGIA_PROCGEN_THREADS").is_err() {
        return;
    }
    let (first, second) = (Evaluator::new(), Evaluator::new());
    let (Parallelism::Pool(a), Parallelism::Pool(b)) = (first.parallelism(), second.parallelism()) else {
        panic!("REGIA_PROCGEN_THREADS set: expected the pool schedule");
    };
    assert!(
        Arc::ptr_eq(a, b),
        "every Evaluator::new() built a new thread pool; the env schedule must be shared"
    );
}

/// `REGIA_PROCGEN_THREADS` forces every `Evaluator::new()` onto a dedicated pool; the pool is
/// built once per process, not once per evaluator (the knob is a test-only schedule switch,
/// and pool churn costs threads per evaluator).
#[test]
fn the_env_thread_pool_is_shared_between_evaluators() {
    if std::env::var("REGIA_PROCGEN_THREADS").is_ok() {
        return; // inside a child: only `env_pool_child` runs there
    }
    let exe = std::env::current_exe().expect("test executable");
    let output = std::process::Command::new(&exe)
        .args(["--exact", "env_pool_child", "--nocapture", "--test-threads=1"])
        .env("REGIA_PROCGEN_THREADS", "2")
        .output()
        .expect("run the child");
    assert!(
        output.status.success(),
        "env_pool_child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_million_point_chain_splits_into_chunks_and_concatenates_them_in_order() {
    // 1 M points cross 245 chunks of 4096: jitter, noise and the range filter run chunked.
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
                    "points.jitter",
                    &[
                        ("offset", "[(-0.2, 0.0, -0.2), (0.2, 0.5, 0.2)]"),
                        ("rotate_deg", "[(0.0, 0.0, 0.0), (10.0, 360.0, 10.0)]"),
                        ("scale", "[(0.5, 0.5, 0.5), (2.0, 2.0, 2.0)]"),
                        ("scale_axes", "each"),
                    ],
                ),
            ),
            (3, node("density.noise", &[("scale_m", "16.0"), ("octaves", "3")])),
            (
                4,
                node(
                    "filter.range",
                    &[("attr", "\"density\""), ("min", "0.3"), ("max", "0.8")],
                ),
            ),
            (5, node("graph.output", &[("pin", "\"in\"")])),
            (6, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "inside", 5, "in"),
            (4, "outside", 6, "in"),
        ],
    );
    let c = compile(&g);
    let o = assert_schedules_agree("1 M chain", &c, &bowl_inputs(2));
    let Outcome::Ok { lines, .. } = o else {
        panic!("the chain runs: {o:?}")
    };
    let jitter = lines.iter().find(|l| l.id == 2).map_or(0, |l| l.elements);
    assert_eq!(jitter, 1_000_000, "one million points through the chain");
    let split: usize = lines
        .iter()
        .filter(|l| l.id == 5 || l.id == 6)
        .map(|l| l.elements)
        .sum();
    assert_eq!(
        split, 0,
        "graph.output has no outputs; the filter's lines carry the counts"
    );
    let filter = lines.iter().find(|l| l.id == 4).map_or(0, |l| l.elements);
    assert_eq!(filter, 1_000_000, "inside plus outside keep every point");
}
