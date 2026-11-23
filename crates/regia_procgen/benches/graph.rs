// SPDX-License-Identifier: MPL-2.0
//! Spec 62 § 12 graph budgets (desktop reference): compile a 200-node graph ≤ 2 ms; sample a
//! 1 km × 1 km ground at 2 m spacing (250 000 candidates), filter by slope and altitude, jitter,
//! prune and spawn ≤ 400 ms on one core, ≤ 120 ms with 8 workers.
//!
//! `compile_200_nodes` times `ProcGraph::compile` alone (no cache: the compile cache lives in the
//! world host). `ground_1km_*` times a whole `Evaluator::run` over a captured 501 × 501 heightfield
//! on the serial schedule (one core) and on an 8-thread pool. Wall time per run; the snapshot is
//! built once. `cargo bench -p regia_procgen --bench graph`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use regia_core::id::Guid;
use regia_procgen::data::{GroundData, ShapeData, ShapeForm};
use regia_procgen::exec::{Parallelism, SelfInfo, StaticSnapshot};
use regia_procgen::{
    CancelToken, CompiledGraph, Edge, Evaluator, NodeEntry, ProcGraph, ProcNodeRegistry, RunInputs,
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

fn compile(g: &ProcGraph, reg: &ProcNodeRegistry) -> CompiledGraph {
    match g.compile(reg) {
        Ok(c) => c,
        Err(e) => panic!("compile: {:?}", e.0),
    }
}

/// 200 nodes: a grid, 198 alternating point and filter nodes with settings, and an output.
fn two_hundred_nodes() -> ProcGraph {
    let mut nodes = vec![(1, node("gen.grid", &[("count", "(8.0, 1.0, 8.0)")]))];
    let mut edges: Vec<(u32, String, u32, String)> = Vec::new();
    let mut prev = (1u32, "out".to_string());
    for id in 2..200u32 {
        let (entry, out) = match id % 3 {
            0 => (
                node(
                    "points.jitter",
                    &[("offset", "[(-0.1, 0.0, -0.1), (0.1, 0.0, 0.1)]")],
                ),
                "out",
            ),
            1 => (
                node(
                    "filter.range",
                    &[("attr", "\"position:y\""), ("min", "-10.0"), ("max", "10.0")],
                ),
                "inside",
            ),
            _ => (node("density.remap", &[("to", "[0.2, 1.0]")]), "out"),
        };
        nodes.push((id, entry));
        edges.push((prev.0, prev.1.clone(), id, "in".to_string()));
        prev = (id, out.to_string());
    }
    nodes.push((200, node("graph.output", &[("pin", "\"out\"")])));
    edges.push((prev.0, prev.1, 200, "in".to_string()));
    let refs: Vec<(u32, &str, u32, &str)> = edges
        .iter()
        .map(|(a, ap, b, bp)| (*a, ap.as_str(), *b, bp.as_str()))
        .collect();
    graph(nodes, &refs)
}

/// The § 12 pipeline: ground at 2 m → slope and altitude filters → jitter → prune → spawn.
fn ground_pipeline() -> ProcGraph {
    graph(
        vec![
            (1, node("in.self", &[])),
            (
                2,
                node(
                    "sample.ground",
                    &[("spacing_m", "2.0"), ("jitter", "0.8"), ("align", "normal")],
                ),
            ),
            (
                3,
                node("density.facing", &[("lo_deg", "10.0"), ("hi_deg", "30.0")]),
            ),
            (
                4,
                node(
                    "filter.range",
                    &[("attr", "\"density\""), ("min", "0.5"), ("max", "1.0")],
                ),
            ),
            (
                5,
                node(
                    "filter.range",
                    &[("attr", "\"position:y\""), ("min", "-20.0"), ("max", "60.0")],
                ),
            ),
            (
                6,
                node(
                    "points.jitter",
                    &[
                        ("rotate_deg", "[(0.0, 0.0, 0.0), (0.0, 360.0, 0.0)]"),
                        ("scale", "[(0.6, 0.6, 0.6), (1.4, 1.4, 1.4)]"),
                    ],
                ),
            ),
            (7, node("points.prune", &[])),
            (
                8,
                node(
                    "spawn.mesh",
                    &[(
                        "entries",
                        "[(mesh: \"meshes/rock_a.glb\", weight: 3.0), (mesh: \"meshes/rock_b.glb\", weight: 1.0)]",
                    )],
                ),
            ),
        ],
        &[
            (1, "out", 2, "region"),
            (2, "out", 3, "in"),
            (3, "out", 4, "in"),
            (4, "inside", 5, "in"),
            (5, "inside", 6, "in"),
            (6, "out", 7, "in"),
            (7, "out", 8, "in"),
        ],
    )
}

/// A 1 km × 1 km rolling heightfield, 501 × 501 samples at 2 m (polynomial and a lattice ripple in
/// f32, so every platform builds the same terrain).
fn ground_inputs() -> RunInputs {
    let n = 501u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let (c, r) = ((i % n) as f32 - 250.0, (i / n) as f32 - 250.0);
            let ripple = ((i % 7) as f32 - 3.0) * 0.4 + ((i / n % 5) as f32 - 2.0) * 0.6;
            0.0004 * c * c - 0.0003 * r * r + 0.05 * c + ripple
        })
        .collect();
    let map = match regia_terrain::TerrainMap::from_heights(n, n, heights, 1000.0, 1.0, 0.0) {
        Ok(m) => m,
        Err(e) => panic!("terrain: {e:?}"),
    };
    let bounds = ShapeData {
        form: ShapeForm::Box {
            half_extents: [500.0, 200.0, 500.0],
        },
        position: [0.0; 3],
        rotation: [0.0, 0.0, 0.0, 1.0],
        edge_m: 0.0,
        clip_xz: None,
    };
    let ground = GroundData::capture(&map, [-500.0, -500.0, 500.0, 500.0], 2);
    let snapshot = StaticSnapshot {
        self_info: Some(SelfInfo {
            guid: Guid(0x5eed),
            name: "bench".to_string(),
            bounds,
        }),
        ground: Some(Arc::new(ground)),
        ..StaticSnapshot::default()
    };
    RunInputs::new(Arc::new(snapshot), 11)
}

fn bench_graph(c: &mut Criterion) {
    let reg = ProcNodeRegistry::with_builtins();
    let big = two_hundred_nodes();
    assert_eq!(big.nodes.len(), 200);
    let mut group = c.benchmark_group("graph");
    group.bench_function("compile_200_nodes", |b| {
        b.iter(|| black_box(compile(black_box(&big), &reg)));
    });

    let pipeline = compile(&ground_pipeline(), &reg);
    let inputs = ground_inputs();
    let probe = Evaluator::new()
        .with_parallelism(Parallelism::Serial)
        .run(&pipeline, &inputs, &CancelToken::new())
        .unwrap_or_else(|e| panic!("pipeline: {e}"));
    let candidates = probe
        .report
        .nodes
        .iter()
        .find(|n| n.id == 2)
        .map_or(0, |n| n.elements);
    assert!(
        (240_000..=260_000).contains(&candidates),
        "about 250 000 candidates: {candidates}"
    );
    assert!(probe.output.instance_count() > 0, "the pipeline spawns");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));
    for (name, par) in [
        ("ground_1km_serial", Parallelism::Serial),
        ("ground_1km_8_threads", Parallelism::threads(8)),
    ] {
        let eval = Evaluator::new().with_parallelism(par);
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(
                    eval.run(&pipeline, &inputs, &CancelToken::new())
                        .unwrap_or_else(|e| panic!("{name}: {e}")),
                )
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_graph);
criterion_main!(benches);
