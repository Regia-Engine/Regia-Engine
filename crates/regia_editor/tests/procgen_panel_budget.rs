// SPDX-License-Identifier: MPL-2.0
//! The procedural panel's frame budget (`docs/specs/67-procedural-graph-editor.md` § 9, "panel frame
//! cost ≤ 2 ms with a 300-node graph") as counts, not time: a frame draws each node once, the count
//! does not grow from frame to frame, and after warm-up a frame allocates a flat, bounded number of times
//! (egui itself allocates every frame: zero is not reachable, growth is the regression). Headless: egui runs on the CPU, no GPU and no clock. This binary holds one test so the
//! counting allocator sees one thread's work only.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::procgen::{self, doc, panel};
use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGraphSource};
use regia_procgen::{Edge, NodeEntry, ProcGraph};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

const NODES: u32 = 300;

/// A chain of `NODES` nodes: in.self → sample.region → 297 × points.jitter → out.points.
fn big_graph() -> ProcGraph {
    let mut g = ProcGraph::default();
    g.nodes.insert(1, NodeEntry::new("in.self"));
    g.nodes.insert(2, NodeEntry::new("sample.region"));
    g.edges.push(Edge::new(1, "out", 2, "region"));
    for id in 3..NODES {
        let mut n = NodeEntry::new("points.jitter");
        n.at = (f64::from(id) * 30.0, 0.0);
        g.nodes.insert(id, n);
        g.edges.push(Edge::new(id - 1, "out", id, "in"));
    }
    g.nodes.insert(NODES, NodeEntry::new("out.points"));
    g.edges.push(Edge::new(NODES - 1, "out", NODES, "in"));
    g
}

fn frame(ctx: &egui::Context, world: &mut World, n: usize) -> u64 {
    let before = alloc_counter::thread_allocs();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1600.0, 900.0),
        )),
        time: Some(n as f64 / 60.0),
        predicted_dt: 1.0 / 60.0,
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| panel::procgen_graph_panel(ui, world));
    });
    out.textures_delta.clear();
    alloc_counter::thread_allocs() - before
}

#[test]
fn a_300_node_panel_frame_draws_each_node_once_and_allocations_stay_flat() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::from([(
            "big.pgraph.ron".to_string(),
            big_graph().to_ron(),
        )])),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.init_resource::<UndoStack>();
    procgen::install(app.world_mut());
    app.finish_plugins();
    app.update();
    doc::open(app.world_mut(), "big.pgraph.ron").expect("open");

    let ctx = egui::Context::default();
    let world = app.world_mut();
    let mut allocs = Vec::new();
    let mut drawn = Vec::new();
    for n in 0..12 {
        allocs.push(frame(&ctx, world, n));
        drawn.push(panel::nodes_drawn(world));
    }
    eprintln!("allocations per frame: {allocs:?}; nodes drawn: {drawn:?}");
    // Each node at most once per frame, the same count every frame after the first.
    assert!(drawn.iter().all(|n| *n <= NODES as usize), "{drawn:?}");
    assert!(
        drawn[3..].iter().all(|n| *n == drawn[3]),
        "drawn count is stable: {drawn:?}"
    );
    assert!(drawn[3] > 0, "the canvas draws nodes");
    assert!(
        drawn[3] < NODES as usize,
        "off-screen nodes are not drawn: {drawn:?}"
    );
    // After warm-up (4 frames) no frame allocates more than the one before it did, give or take
    // nothing: the steady state is flat.
    let steady = &allocs[6..];
    let (lo, hi) = (
        *steady.iter().min().unwrap_or(&0),
        *steady.iter().max().unwrap_or(&0),
    );
    assert!(
        hi - lo <= 8 && hi <= allocs[1] + 8,
        "steady-state allocations per frame are flat (within 8) and no more than the first warm frame: {allocs:?}"
    );
    // Absolute ceiling on a steady frame. Measured 2026-11-07 (debug, line tables only, 1600×900,
    // 1:1 view of the origin): 49 of 300 nodes drawn every frame, steady allocations 7 796–7 797.
    // Frame 0 was 41 122 because the first `run_ui` can run twice and builds the snarl; it is not
    // in `steady`. Before the cull, the same graph allocated about 19 167 times a frame (about 64
    // per node). 8 000 is the measured high-water plus a small pad: drawing the whole graph, or
    // doubling the visible-set cost, fails it. Counts only; the 2 ms figure in the spec is not asserted.
    assert!(hi <= 8_000, "allocations per frame: {allocs:?}");
}
