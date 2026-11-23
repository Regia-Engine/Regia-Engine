// SPDX-License-Identifier: MPL-2.0
//! Culling of off-screen nodes in the procedural panel: a before/after of nodes drawn and
//! allocations per frame on a long graph (counts, not time). The original budget doc follows.
//!
//! The procedural panel's frame budget (`docs/specs/67-procedural-graph-editor.md` § 9, "panel frame
//! cost ≤ 2 ms with a 300-node graph") as counts, not time: a frame draws each node once, the count
//! does not grow from frame to frame, and after warm-up a frame allocates a flat, bounded number of times
//! (egui itself allocates every frame: zero is not reachable, growth is the regression). Headless: egui runs on the CPU, no GPU and no clock. This binary holds one test so the
//! counting allocator sees one thread's work only.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::procgen::{self, doc, panel};
use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGraphSource};
use regia_procgen::{Edge, NodeEntry, ProcGraph};

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards to the system allocator unchanged; the counter is a thread-local `Cell`.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const NODES: u32 = 1000;

/// A chain of `NODES` nodes spread 30 units apart: far more than one screen holds.
fn long_graph() -> ProcGraph {
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
    let before = ALLOCS.with(Cell::get);
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
    ALLOCS.with(Cell::get) - before
}

/// Runs 12 frames and returns (nodes drawn, nodes culled, allocations) of the last one.
fn run(app: &mut App, ctx: &egui::Context) -> (usize, usize, u64) {
    let world = app.world_mut();
    let mut last = 0;
    for n in 0..12 {
        last = frame(ctx, world, n);
    }
    (panel::nodes_drawn(world), panel::nodes_culled(world), last)
}

#[test]
fn off_screen_nodes_are_culled_and_the_frame_allocates_less() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::from([(
            "long.pgraph.ron".to_string(),
            long_graph().to_ron(),
        )])),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.init_resource::<UndoStack>();
    procgen::install(app.world_mut());
    app.finish_plugins();
    app.update();
    doc::open(app.world_mut(), "long.pgraph.ron").expect("open");

    // Before: culling off draws every node.
    panel::set_culling(app.world_mut(), false);
    let ctx = egui::Context::default();
    let (drawn_off, culled_off, allocs_off) = run(&mut app, &ctx);
    // After: culling on.
    panel::set_culling(app.world_mut(), true);
    let ctx = egui::Context::default();
    let (drawn_on, culled_on, allocs_on) = run(&mut app, &ctx);
    eprintln!(
        "CULL before: drawn {drawn_off} culled {culled_off} allocs {allocs_off}; \
         after: drawn {drawn_on} culled {culled_on} allocs {allocs_on}"
    );
    assert_eq!(culled_off, 0, "culling off skips nothing");
    assert_eq!(drawn_off, NODES as usize, "culling off draws every node once");
    assert!(
        culled_on > 0,
        "a 1000-node chain 30 000 units wide has off-screen nodes"
    );
    assert_eq!(
        drawn_on + culled_on,
        NODES as usize,
        "every node is either drawn or culled, once"
    );
    assert!(drawn_on < drawn_off, "fewer nodes drawn with culling");
    assert!(
        allocs_on < allocs_off,
        "fewer allocations with culling: {allocs_on} vs {allocs_off}"
    );
}
