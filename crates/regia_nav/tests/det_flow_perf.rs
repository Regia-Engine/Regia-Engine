// SPDX-License-Identifier: MPL-2.0
//! Perf evidence for `regia_nav::det::flow` (ADR-8294). Not a pass/fail test: timings are printed for the lane
//! report and never asserted. Run in release:
//! `cargo test --release -p regia_nav --test det_flow_perf -- --ignored --nocapture`.

use std::sync::Arc;
use std::time::Instant;

use regia_nav::det::flow::{ClusterFlow, FlowField, FlowSettings};
use regia_nav::det::grid::{GridSettings, HeightCells, NavGrid, NavStatic, Profile, Stamp};

fn flat(w: usize, h: usize) -> NavGrid {
    let settings = Arc::new(GridSettings {
        canvas_cells: [w, h],
        ..GridSettings::hover_ground_jet()
    });
    NavGrid::new(Arc::new(NavStatic::build(None, settings).expect("canvas"))).expect("grid")
}

/// The walled 256 x 256 map of the unit tests: a cliff wall with a gap and two stamps.
fn walled() -> NavGrid {
    let mut heights = Vec::with_capacity(256 * 256);
    for z in 0..256usize {
        for x in 0..256usize {
            let wall = (126..=129).contains(&x) && !(140..=146).contains(&z);
            heights.push(if wall { 60.0 } else { 0.0 });
        }
    }
    let cells = HeightCells::centered(256, 256, 5.0, heights);
    let settings = Arc::new(GridSettings::hover_ground_jet());
    let mut g = NavGrid::new(Arc::new(
        NavStatic::build(Some(&cells), settings).expect("ground"),
    ))
    .expect("grid");
    g.sync_stamps(vec![
        (60, Stamp::from_world(-60.0, 30.0, 9.0)),
        (61, Stamp::from_world(40.0, 70.0, 12.0)),
    ]);
    g
}

/// SplitMix64 (the det golden input stream), local so this file needs only the public API.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn signed(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / 16_777_216.0 * 2.0 - 1.0
    }
}

/// Perf evidence (not an assertion; run with `--ignored --nocapture` in release): a 256 x 256 full
/// field, a clustered flow on a 4096 x 4096 canvas (route plus the local fields of one long walk),
/// and 900 movers sampling a field per tick.
#[test]
#[ignore = "perf evidence: run in release with --ignored --nocapture"]
fn flow_perf_evidence() {
    let grid = walled();
    let p = Profile::HOVER;
    let goal = grid.cell_at(400.0, 300.0);
    let t = Instant::now();
    let reps = 20;
    let mut field = None;
    for _ in 0..reps {
        field = Some(FlowField::build(&grid, p, &[goal], FlowSettings::default()));
    }
    let field = field.expect("built");
    println!(
        "full 256x256: {:.2} ms per build, {} bytes, {} reached",
        t.elapsed().as_secs_f64() * 1000.0 / reps as f64,
        field.memory_bytes(),
        field.stats().reached
    );
    let mut rng = Rng(3);
    let movers: Vec<[f32; 2]> = (0..900)
        .map(|_| [rng.signed() * 600.0, rng.signed() * 600.0])
        .collect();
    for blend in [false, true] {
        let f = FlowField::build(
            &grid,
            p,
            &[goal],
            FlowSettings {
                blend,
                ..FlowSettings::default()
            },
        );
        let t = Instant::now();
        let ticks = 1000;
        let mut sum = 0.0f32;
        for _ in 0..ticks {
            for m in &movers {
                if let Some(d) = f.direction_at(*m) {
                    sum += d[0];
                }
            }
        }
        println!(
            "900 movers, blend {blend}: {:.1} us per tick ({sum})",
            t.elapsed().as_secs_f64() * 1e6 / ticks as f64
        );
    }
    let t = Instant::now();
    let big = flat(4096, 4096);
    println!(
        "4096x4096 grid build: {:.0} ms",
        t.elapsed().as_secs_f64() * 1000.0
    );
    let goal = big.cell_at(9000.0, 8000.0);
    let t = Instant::now();
    let mut flow = ClusterFlow::build(&big, p, &[goal], 1 << 18);
    println!(
        "4096x4096 cluster route: {:.1} ms, regions/edges/expanded {:?}",
        t.elapsed().as_secs_f64() * 1000.0,
        flow.route_size()
    );
    let t = Instant::now();
    let start = big.cell_at(-9000.0, -7000.0);
    let mut at = start;
    let mut steps = 0;
    while let Some(n) = flow.next_cell(&big, at) {
        at = n;
        steps += 1;
    }
    println!(
        "4096x4096 cluster walk: {steps} moves, {} local fields, {:.1} ms, {} bytes",
        flow.stats().local_builds,
        t.elapsed().as_secs_f64() * 1000.0,
        flow.memory_bytes()
    );
    let t = Instant::now();
    let mut sum = 0.0f32;
    for m in &movers {
        let pos = [m[0] * 14.0, m[1] * 14.0];
        if let Some(d) = flow.direction_at(&big, pos) {
            sum += d[1];
        }
    }
    println!(
        "4096x4096 cluster, 900 movers first tick (builds on demand): {:.1} ms ({sum}), {} local fields",
        t.elapsed().as_secs_f64() * 1000.0,
        flow.stats().local_builds
    );
    let t = Instant::now();
    let ticks = 100;
    for _ in 0..ticks {
        for m in &movers {
            let pos = [m[0] * 14.0, m[1] * 14.0];
            if let Some(d) = flow.direction_at(&big, pos) {
                sum += d[1];
            }
        }
    }
    println!(
        "4096x4096 cluster, 900 movers cached tick: {:.1} us per tick ({sum}), {} bytes",
        t.elapsed().as_secs_f64() * 1e6 / ticks as f64,
        flow.memory_bytes()
    );
    // A group order: 900 movers in a 300 m square far from the goal (the usual RTS case).
    let mut group = ClusterFlow::build(&big, p, &[goal], 4096);
    let t = Instant::now();
    for m in &movers {
        let pos = [-8000.0 + m[0] * 0.25, -6000.0 + m[1] * 0.25];
        if let Some(d) = group.direction_at(&big, pos) {
            sum += d[0];
        }
    }
    println!(
        "4096x4096 cluster, 900 movers in one group, first tick: {:.1} ms, {} local fields, {} bytes",
        t.elapsed().as_secs_f64() * 1000.0,
        group.stats().local_builds,
        group.memory_bytes()
    );
    let t = Instant::now();
    for _ in 0..ticks {
        for m in &movers {
            let pos = [-8000.0 + m[0] * 0.25, -6000.0 + m[1] * 0.25];
            if let Some(d) = group.direction_at(&big, pos) {
                sum += d[0];
            }
        }
    }
    println!(
        "4096x4096 cluster, 900 movers in one group, cached tick: {:.1} us per tick ({sum})",
        t.elapsed().as_secs_f64() * 1e6 / ticks as f64
    );
    let t = Instant::now();
    let fullbig = FlowField::build(&big, p, &[goal], FlowSettings::default());
    println!(
        "4096x4096 full field for comparison: {:.0} ms, {} bytes",
        t.elapsed().as_secs_f64() * 1000.0,
        fullbig.memory_bytes()
    );
}

/// Perf evidence for the agent surface (ADR-8839; printed, never asserted): `DetFlowView::query`
/// (what `sim.grid_flow` runs) on a 256 x 256 grid (first call builds the full field, the next
/// answers from the kept one), on 1024 x 1024 (the largest full field), and on 4096 x 4096 (the
/// clustered route plus one 64 x 64 window); and the debug arrows of a 64 x 64 window.
#[test]
#[ignore = "perf evidence: run in release with --ignored --nocapture"]
fn flow_view_and_arrows_perf_evidence() {
    use regia_detsim::debug::DebugRecorder;
    use regia_nav::det::flow::CellRect;
    use regia_nav::det_view::DetFlowView;

    let p = Profile::HOVER;
    for (side, label) in [(256usize, "256x256"), (1024, "1024x1024"), (4096, "4096x4096")] {
        let grid = if side == 256 { walled() } else { flat(side, side) };
        let goal = grid.cell_at(400.0, 300.0);
        let (gx, gz) = grid.xz(goal);
        let rect = CellRect {
            x0: gx.saturating_sub(32) as u32,
            z0: gz.saturating_sub(32) as u32,
            x1: (gx + 32) as u32,
            z1: (gz + 32) as u32,
        };
        let mut view = DetFlowView::default();
        let t = Instant::now();
        view.publish(1, &grid);
        let publish_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let first = view.query(p, &[goal], rect, 4096).expect("query");
        let first_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let reps = 20;
        for _ in 0..reps {
            let again = view.query(p, &[goal], rect, 4096).expect("query");
            assert!(again.cached);
        }
        println!(
            "DetFlowView {label}: publish (grid copy) {publish_ms:.1} ms, first query ({:?}) {first_ms:.1} ms, \
             cached window {:.1} us, {} bytes",
            first.kind,
            t.elapsed().as_secs_f64() * 1e6 / reps as f64,
            first.memory_bytes
        );
    }
    let grid = walled();
    let goal = grid.cell_at(400.0, 300.0);
    let field = FlowField::build(&grid, p, &[goal], FlowSettings::default());
    let (gx, gz) = grid.xz(goal);
    let rect = CellRect {
        x0: gx as u32 - 32,
        z0: gz as u32 - 32,
        x1: gx as u32 + 32,
        z1: gz as u32 + 32,
    };
    let mut rec = DebugRecorder::default();
    let reps = 200;
    let t = Instant::now();
    for _ in 0..reps {
        rec.clear();
        field.debug_emit(&mut rec, rect, 4096, 0.0);
    }
    println!(
        "debug arrows 64x64: {:.1} us per frame, {} primitives",
        t.elapsed().as_secs_f64() * 1e6 / reps as f64,
        rec.len()
    );
}
