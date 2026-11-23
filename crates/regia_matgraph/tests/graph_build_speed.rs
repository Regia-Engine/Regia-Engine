// SPDX-License-Identifier: MPL-2.0
//! Material-graph pipeline builds off the frame path are fast enough for a level's worth of
//! graphs (lane warm-speed, 2026-10-10): `N` distinct graphs prewarmed in async mode and
//! settled take no longer than building them inline on one thread (generous bound: the async
//! workers must not serialise or stall), and print the per-set costs.
//!
//! One test in its own binary so its timings see no other test's GPU work.

use std::collections::BTreeMap;
use std::time::Instant;

use regia_matgraph::{MaterialGraph, compile, default_params, lab_material};
use regia_render::{FrameRenderer, GraphTarget, RenderDevice, SurfaceGraph};

/// A graph whose code differs per `i` (the constant is part of the generated WGSL).
fn graph(i: usize) -> SurfaceGraph {
    let text = format!(
        r#"(
    format_version: 2,
    params: [(name: "tint", value: Color((0.1, 0.35, 0.8)))],
    nodes: {{
        "tint": Param("tint"),
        "wp": WorldPosition,
        "z": Swizzle(input: "wp", channels: "z"),
        "k": Float({k}),
        "zk": Mul("z", "k"),
        "f": Frac("zk"),
        "base": Mul("tint", "f"),
    }},
    output: (base_color: Some("base")),
)"#,
        k = 1.0 + i as f32 * 0.25
    );
    let graph = MaterialGraph::from_ron(&text).expect("parse graph");
    let compiled = compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    let material =
        lab_material(&graph, &compiled, default_params(&graph, 0.0), &BTreeMap::new()).expect("material");
    material
        .surface_graph
        .as_deref()
        .cloned()
        .expect("graph material carries its surface graph")
}

#[test]
fn async_graph_builds_are_not_slower_than_inline() {
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let count: usize = regia_core::brand::env_var("GRAPH_SPEED_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    let graphs: Vec<SurfaceGraph> = (0..count).map(graph).collect();
    let refs: Vec<&SurfaceGraph> = graphs.iter().collect();
    let targets = [GraphTarget::HDR];

    // Inline: every set built on this thread, one after another.
    let mut inline = FrameRenderer::new();
    let start = Instant::now();
    let built = inline
        .prewarm_material_graphs(&device, &refs[..count / 2], &targets)
        .expect("inline prewarm");
    let inline_ms = start.elapsed().as_secs_f64() * 1e3;
    assert_eq!(built, count / 2);
    let per_set_inline = inline_ms / built.max(1) as f64;

    // Async: the other half queued on the renderer's material-graph workers, then settled.
    let mut background = FrameRenderer::new();
    background.set_async_material_graphs(true);
    let start = Instant::now();
    let queued = background
        .prewarm_material_graphs(&device, &refs[count / 2..], &targets)
        .expect("async prewarm");
    background.settle_material_graphs();
    let async_ms = start.elapsed().as_secs_f64() * 1e3;
    let stats = background.material_graph_stats();
    assert_eq!(stats.ready, queued, "{stats:?}");
    assert_eq!(stats.failed, 0, "{stats:?}");
    let per_set_async = async_ms / queued.max(1) as f64;
    println!(
        "graph sets: inline {built} in {inline_ms:.1} ms ({per_set_inline:.1} ms/set), \
         async {queued} in {async_ms:.1} ms ({per_set_async:.1} ms/set wall), {} threads available",
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    );
    // A budgeted settle (the game-frame warm's) returns at its deadline with builds still
    // pending; they keep building and a later settle installs every one.
    let mut budgeted = FrameRenderer::new();
    budgeted.set_async_material_graphs(true);
    let fresh: Vec<SurfaceGraph> = (count..count + 4).map(graph).collect();
    let fresh_refs: Vec<&SurfaceGraph> = fresh.iter().collect();
    let queued_fresh = budgeted
        .prewarm_material_graphs(&device, &fresh_refs, &targets)
        .expect("budgeted prewarm");
    let start = Instant::now();
    let all = budgeted.settle_material_graphs_for(std::time::Duration::from_millis(1));
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "the budgeted settle overran its budget"
    );
    // `all` is exactly "nothing pending": a fast machine may finish four small sets in 1 ms.
    assert_eq!(
        all,
        budgeted.material_graph_stats().pending == 0,
        "settle_material_graphs_for must report whether every build landed"
    );
    budgeted.settle_material_graphs();
    let stats = budgeted.material_graph_stats();
    assert_eq!((stats.ready, stats.pending), (queued_fresh, 0), "{stats:?}");

    // Generous: the workers run in parallel, so the async wall time per set must not exceed
    // the inline time per set by more than 1.5x (scheduling noise on a loaded CI box).
    assert!(
        per_set_async <= per_set_inline * 1.5 + 5.0,
        "async graph builds stalled: {per_set_async:.1} ms/set against {per_set_inline:.1} inline"
    );
}
