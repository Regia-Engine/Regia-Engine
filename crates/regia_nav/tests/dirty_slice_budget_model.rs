// SPDX-License-Identifier: MPL-2.0
//! Fixed-step oracle for the existing region queue contract. All geometry is in memory.
//!
//! The oracle scans a fixed lattice and closed tile rectangles, independently of `tiles_in`,
//! `tile_bounds`, `region_window`, and `rebuild_region`. Coalescing expectations are explicit
//! arrival histories, rather than a second implementation of the queue's insertion algorithm.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{
    NavAgentParams, NavDirtySource, NavMeshBuildParams, NavRegionQueue, NavRegionStats, NavTileCache,
    NavTileCacheParams, apply_nav_regions,
};

const CELLS: usize = 16;
const TILE_CELLS: usize = 4;
const CELL: f32 = 0.5;
const ORIGIN: f32 = -4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Box3 {
    lo: Vec3,
    hi: Vec3,
}

impl Box3 {
    fn new(x0: f32, z0: f32, x1: f32, z1: f32) -> Self {
        Self {
            lo: Vec3::new(x0, -1.0, z0),
            hi: Vec3::new(x1, 1.0, z1),
        }
    }

    fn normalized(self) -> Self {
        Self {
            lo: self.lo.min(self.hi),
            hi: self.lo.max(self.hi),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Work {
    bounds: Box3,
    source: NavDirtySource,
    sliced: bool,
}

fn request(bounds: Box3, source: NavDirtySource) -> Work {
    Work {
        bounds: bounds.normalized(),
        source,
        sliced: source == NavDirtySource::Replay,
    }
}

/// Closed rectangles share their edge: an exact tile-edge box reaches both neighbors.
fn slices(work: Work) -> Vec<Work> {
    let mut result = Vec::new();
    for z in 0..CELLS / TILE_CELLS {
        for x in 0..CELLS / TILE_CELLS {
            let tile_lo = Vec3::new(ORIGIN + x as f32 * 2.0, -1.0, ORIGIN + z as f32 * 2.0);
            let tile_hi = tile_lo + Vec3::new(2.0, 2.0, 2.0);
            let lo = work.bounds.lo.max(tile_lo);
            let hi = work.bounds.hi.min(tile_hi);
            if lo.x <= hi.x && lo.z <= hi.z {
                result.push(Work {
                    bounds: Box3 {
                        lo: Vec3::new(lo.x, work.bounds.lo.y, lo.z),
                        hi: Vec3::new(hi.x, work.bounds.hi.y, hi.z),
                    },
                    sliced: true,
                    ..work
                });
            }
        }
    }
    result
}

/// The fixture has zero erosion. The apply area is the centers in the box plus one cell;
/// the geometry window includes one more row of cells on each side (region.rs contract).
fn cells_and_window(bounds: Box3) -> (Vec<(usize, usize)>, Box3) {
    let mut cells = Vec::new();
    for z in 0..CELLS {
        for x in 0..CELLS {
            let cx = ORIGIN + (x as f32 + 0.5) * CELL;
            let cz = ORIGIN + (z as f32 + 0.5) * CELL;
            if cx >= bounds.lo.x - CELL
                && cx <= bounds.hi.x + CELL
                && cz >= bounds.lo.z - CELL
                && cz <= bounds.hi.z + CELL
            {
                cells.push((x, z));
            }
        }
    }
    assert!(!cells.is_empty(), "fixture box must reach a cell");
    let min_x = cells.iter().map(|&(x, _)| x).min().expect("cells");
    let max_x = cells.iter().map(|&(x, _)| x).max().expect("cells");
    let min_z = cells.iter().map(|&(_, z)| z).min().expect("cells");
    let max_z = cells.iter().map(|&(_, z)| z).max().expect("cells");
    let edge = |n: usize| ORIGIN + n as f32 * CELL;
    let window = Box3 {
        lo: Vec3::new(
            edge(min_x.saturating_sub(1)),
            f32::NEG_INFINITY,
            edge(min_z.saturating_sub(1)),
        ),
        hi: Vec3::new(
            edge((max_x + 2).min(CELLS)),
            f32::INFINITY,
            edge((max_z + 2).min(CELLS)),
        ),
    };
    (cells, window)
}

#[derive(Default)]
struct Probe {
    calls: Vec<Box3>,
    call_limit: usize,
    blocked: Option<Box3>,
    invalid: Option<Box3>,
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    boxes: Vec<(Vec3, Vec3, &'static str)>,
    stats: NavRegionStats,
    calls: Vec<Box3>,
    pending: Vec<(u32, u32)>,
    rebuilds: Vec<u64>,
    revision: u64,
}

struct Harness {
    world: World,
    probe: Arc<Mutex<Probe>>,
    waiting: Vec<Work>,
    stats: NavRegionStats,
    // z-major indices also model the cache's polygonization order.
    walkable: BTreeSet<usize>,
    pending: BTreeSet<usize>,
    rebuilds: Vec<u64>,
    revision: u64,
    installed: bool,
}

impl Harness {
    fn new() -> Self {
        let params = NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: CELL,
                border_size: 0.0,
                agent: NavAgentParams {
                    radius: 0.0,
                    ..Default::default()
                },
                ..Default::default()
            },
            tile_size: TILE_CELLS as u32,
        };
        // Build adds a final cell beyond the geometry. Centers in the first 15 rows/columns
        // lie on this flat quad; the final row and column have no geometry.
        let a = Vec3::new(-4.0, 0.0, -4.0);
        let b = Vec3::new(3.5, 0.0, -4.0);
        let c = Vec3::new(3.5, 0.0, 3.5);
        let d = Vec3::new(-4.0, 0.0, 3.5);
        let cache = NavTileCache::build(params, &[[a, b, c], [a, c, d]]).expect("flat cache");
        assert_eq!(cache.layout().origin, a);
        assert_eq!((cache.layout().width, cache.layout().height), (16, 16));
        assert_eq!(cache.tile_grid(), (4, 4));
        let mut world = World::new();
        world.insert_resource(cache);
        let mut queue = NavRegionQueue::default();
        queue.config.max_slices_per_step = 2;
        queue.config.max_tiles_per_step = 1;
        queue.config.max_queued = 32;
        world.insert_resource(queue);
        Self {
            world,
            probe: Arc::new(Mutex::new(Probe::default())),
            waiting: Vec::new(),
            stats: NavRegionStats::default(),
            walkable: (0..15)
                .flat_map(|z| (0..15).map(move |x| z * CELLS + x))
                .collect(),
            pending: BTreeSet::new(),
            rebuilds: vec![0; 16],
            revision: 0,
            installed: false,
        }
    }

    fn install(&mut self) {
        let probe = self.probe.clone();
        self.world
            .resource_mut::<NavRegionQueue>()
            .set_source(Arc::new(move |_, lo, hi, out| {
                let mut probe = probe.lock().expect("test probe");
                let window = Box3 { lo, hi };
                assert!(
                    probe.calls.len() < probe.call_limit,
                    "source retried beyond this step's attempts"
                );
                probe.calls.push(window);
                if probe.blocked == Some(window) {
                    // A refusing source may append partial geometry; the next call must clear it.
                    out.push([Vec3::splat(f32::NAN); 3]);
                    return false;
                }
                if probe.invalid == Some(window) {
                    out.push([Vec3::splat(f32::NAN); 3]);
                } else {
                    assert!(out.is_empty(), "triangles leaked from an earlier source call");
                }
                // Empty geometry removes the floor, giving an independent changed-cell oracle.
                true
            }));
        self.installed = true;
    }

    fn push(&mut self, bounds: Box3, source: NavDirtySource) {
        self.world
            .resource_mut::<NavRegionQueue>()
            .push(bounds.lo, bounds.hi, source);
        self.stats.received += 1;
    }

    fn budgets(&mut self, slices: usize, tiles: usize) {
        let mut queue = self.world.resource_mut::<NavRegionQueue>();
        queue.config.max_slices_per_step = slices;
        queue.config.max_tiles_per_step = tiles;
    }

    fn snapshot(&self) -> Snapshot {
        let q = self.world.resource::<NavRegionQueue>();
        let cache = self.world.resource::<NavTileCache>();
        Snapshot {
            boxes: q.boxes().collect(),
            stats: q.stats(),
            calls: self.probe.lock().expect("test probe").calls.clone(),
            pending: cache.pending_tiles(),
            rebuilds: (0..4)
                .flat_map(|z| (0..4).map(move |x| cache.tile_rebuild_count(x, z).expect("tile")))
                .collect(),
            revision: cache.revision(),
        }
    }

    fn check(&self) {
        let actual = self.snapshot();
        assert_eq!(
            actual.boxes,
            self.waiting
                .iter()
                .map(|w| (w.bounds.lo, w.bounds.hi, w.source.name()))
                .collect::<Vec<_>>()
        );
        assert_eq!(actual.stats, self.stats);
        assert_eq!(
            actual.pending,
            self.pending
                .iter()
                .map(|&i| ((i % 4) as u32, (i / 4) as u32))
                .collect::<Vec<_>>()
        );
        assert_eq!(actual.rebuilds, self.rebuilds);
        assert_eq!(actual.revision, self.revision);
        let cache = self.world.resource::<NavTileCache>();
        assert_eq!(cache.pending_tile_count(), self.pending.len());
        assert_eq!(cache.total_rebuilds(), self.rebuilds.iter().sum::<u64>());
        let queue = self.world.resource::<NavRegionQueue>();
        assert_eq!(queue.len(), self.waiting.len());
        assert_eq!(queue.is_empty(), self.waiting.is_empty());
        assert_eq!(queue.has_source(), self.installed);
    }

    fn step(&mut self) -> Snapshot {
        self.check();
        let config = self.world.resource::<NavRegionQueue>().config;
        let mut expected_calls = Vec::new();
        if !self.waiting.is_empty() && !self.installed {
            self.stats.source_waits += 1;
        } else {
            for _ in 0..config.max_slices_per_step {
                while self.waiting.first().is_some_and(|w| !w.sliced) {
                    let front = self.waiting.remove(0);
                    let expanded = slices(front);
                    if expanded.is_empty() {
                        self.stats.rejected += 1;
                    }
                    self.waiting.splice(0..0, expanded);
                }
                if self.waiting.is_empty() {
                    break;
                }
                let front = self.waiting[0];
                let (cells, window) = cells_and_window(front.bounds);
                expected_calls.push(window);
                let probe = self.probe.lock().expect("test probe");
                if probe.blocked == Some(window) {
                    self.stats.source_waits += 1;
                    break;
                }
                self.waiting.remove(0);
                if probe.invalid == Some(window) {
                    self.stats.failed += 1;
                    continue;
                }
                self.stats.slices += 1;
                for (x, z) in cells {
                    if self.walkable.remove(&(z * CELLS + x)) {
                        self.stats.cells_changed += 1;
                        self.pending.insert((z / TILE_CELLS) * 4 + x / TILE_CELLS);
                    }
                }
            }
        }
        let rebuilt = config.max_tiles_per_step.min(self.pending.len());
        for _ in 0..rebuilt {
            let tile = self.pending.pop_first().expect("pending tile");
            self.rebuilds[tile] += 1;
        }
        if rebuilt > 0 {
            self.revision += 1;
        }
        {
            let mut probe = self.probe.lock().expect("test probe");
            probe.calls.clear();
            // Fail inside a misbehaving retry loop, without a timeout or unbounded call log.
            probe.call_limit = expected_calls.len();
        }
        apply_nav_regions(&mut self.world);
        let actual = self.snapshot();
        assert_eq!(actual.calls, expected_calls, "source windows and retry order");
        assert!(actual.calls.len() <= config.max_slices_per_step, "attempt budget");
        self.check();
        actual
    }

    fn drain(&mut self) {
        for _ in 0..128 {
            self.step();
            if self.waiting.is_empty() && self.pending.is_empty() {
                return;
            }
        }
        panic!("bounded finite history did not drain");
    }
}

#[test]
fn closed_tile_intersections_cover_reversed_edges_negative_and_clipped_boxes() {
    // Enumerate every pairing of negative coordinates, tile/grid edges and sub-cell offsets.
    let edges = [-4.5, -4.0, -3.875, -2.0, -0.125, 0.0, 2.0, 4.0, 4.5];
    for &x0 in &edges {
        for &x1 in &edges {
            for &(z0, z1) in &[(-3.0, -3.0), (-2.0, 0.0), (2.0, -2.0), (-4.5, 4.5)] {
                let mut h = Harness::new();
                h.install();
                let bounds = Box3::new(x0, z0, x1, z1);
                let w = request(bounds, NavDirtySource::Terrain);
                h.push(bounds, w.source);
                h.waiting.push(w);
                let expected = slices(w);
                h.budgets(1, 0);
                h.step();
                assert_eq!(h.stats.slices as usize, usize::from(!expected.is_empty()));
                h.budgets(64, 1);
                h.drain();
                assert_eq!(h.stats.slices as usize, expected.len());
                assert_eq!(h.stats.rejected, u64::from(expected.is_empty()));
                assert_eq!(h.stats.failed, 0);
            }
        }
    }
}

#[test]
fn overlapping_sources_wait_at_the_selected_slice_with_exact_step_accounting() {
    let mut h = Harness::new();
    let a = request(Box3::new(-3.0, -3.0, 1.0, -1.0), NavDirtySource::Terrain);
    let b = request(Box3::new(-1.0, -2.5, 3.0, 1.0), NavDirtySource::Destruction);
    h.push(a.bounds, a.source);
    h.push(b.bounds, b.source);
    h.waiting = vec![a, b];
    // Missing source preserves unsliced requests even with zero work budgets.
    h.budgets(0, 0);
    h.step();
    h.step();
    assert_eq!(h.stats.source_waits, 2);
    h.install();
    h.step();
    assert_eq!(h.stats.slices, 0);
    let selected = slices(a)[1];
    h.probe.lock().expect("test probe").blocked = Some(cells_and_window(selected.bounds).1);
    h.budgets(3, 0);
    let stalled = h.step();
    assert_eq!(stalled.calls.len(), 2, "first succeeds, second waits");
    assert_eq!(h.stats.slices, 1, "slicing itself is not a rebuild");
    assert!(!h.pending.is_empty());
    h.budgets(64, 1);
    assert_eq!(h.step().calls.len(), 1, "no retry loop inside a step");
    // A later arrival remains behind the refused slice and the other source.
    let c = request(Box3::new(-3.0, 1.0, -1.0, 3.0), NavDirtySource::Game);
    h.push(c.bounds, c.source);
    h.waiting.push(c);
    h.step();
    h.probe.lock().expect("test probe").blocked = None;
    h.budgets(2, 1);
    h.drain();
    assert_eq!(h.stats.source_waits, 5);
    assert_eq!(
        h.stats.slices as usize,
        slices(a).len() + slices(b).len() + slices(c).len()
    );
    let touched: BTreeSet<_> = h
        .rebuilds
        .iter()
        .enumerate()
        .filter_map(|(i, &n)| (n > 0).then_some(i))
        .collect();
    assert_eq!(
        touched,
        BTreeSet::from([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13])
    );
}

#[test]
fn equivalent_coalesced_histories_keep_replays_whole_and_in_arrival_order() {
    let run = |replace_small: bool| {
        let mut h = Harness::new();
        h.install();
        h.world.resource_mut::<NavRegionQueue>().config.max_queued = 1;
        let big = request(Box3::new(-3.0, -3.0, 1.0, 1.0), NavDirtySource::Terrain);
        let small = request(Box3::new(-2.5, -2.5, -1.5, -1.5), NavDirtySource::Destruction);
        let replay = request(big.bounds, NavDirtySource::Replay);
        let (first, second) = if replace_small { (small, big) } else { (big, small) };
        h.push(first.bounds, first.source);
        h.push(replay.bounds, replay.source);
        h.push(second.bounds, second.source);
        h.stats.covered = 1;
        h.waiting = vec![big, replay];
        h.check();
        // Replays bypass both covered-box coalescing and queue overflow in either direction.
        h.push(replay.bounds, replay.source);
        h.waiting.push(replay);
        let extended = request(Box3::new(-3.0, -3.0, 3.0, 1.0), NavDirtySource::Terrain);
        let extra = request(Box3::new(1.5, -2.5, 3.0, 0.5), NavDirtySource::Game);
        h.push(extra.bounds, extra.source);
        h.stats.overflowed = 1;
        h.waiting[0] = extended;
        h.check();
        let mut trace = Vec::new();
        for _ in 0..16 {
            trace.push(h.step());
        }
        assert!(h.waiting.is_empty());
        assert!(h.pending.is_empty());
        assert_eq!(h.stats.received, 5);
        assert_eq!(h.stats.covered, 1);
        assert_eq!(h.stats.overflowed, 1);
        assert_eq!(h.stats.slices as usize, slices(extended).len() + 2);
        trace
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn refused_rebuild_consumes_budget_and_zero_tile_budget_preserves_pending_work() {
    let mut h = Harness::new();
    h.install();
    let w = request(Box3::new(-3.0, -3.0, 1.0, -1.0), NavDirtySource::Game);
    h.push(w.bounds, w.source);
    h.waiting.push(w);
    h.probe.lock().expect("test probe").invalid = Some(cells_and_window(slices(w)[0].bounds).1);
    h.budgets(1, 0);
    h.step();
    assert_eq!(h.stats.failed, 1);
    assert_eq!(h.stats.slices, 0);
    // Refuse the next slice too, followed by a valid slice in this same step. The invalid
    // triangles must be cleared between attempts, and both attempts spend the budget.
    h.probe.lock().expect("test probe").invalid = Some(cells_and_window(slices(w)[1].bounds).1);
    h.budgets(2, 0);
    h.step();
    assert_eq!(h.stats.failed, 2);
    h.probe.lock().expect("test probe").invalid = None;
    assert_eq!(h.stats.slices, 1);
    assert!(!h.pending.is_empty());
    h.budgets(0, 1);
    h.step();
    assert_eq!(h.stats.slices, 1, "tile draining spends no slice budget");
    h.budgets(2, 1);
    h.drain();
    assert_eq!(h.stats.slices as usize + h.stats.failed as usize, slices(w).len());
}

#[test]
fn arrivals_after_slicing_do_not_treat_pending_slices_as_covering_requests() {
    let mut h = Harness::new();
    h.install();
    let original = request(Box3::new(-3.0, -3.0, 1.0, 1.0), NavDirtySource::Terrain);
    h.push(original.bounds, original.source);
    h.waiting.push(original);
    h.budgets(1, 0);
    h.step();
    let tiny = request(Box3::new(-1.75, -2.75, -1.25, -2.25), NavDirtySource::Destruction);
    // This lies inside the first pending slice, but a slice cannot suppress a new request.
    h.push(tiny.bounds, tiny.source);
    h.waiting.push(tiny);
    h.check();
    assert_eq!(h.stats.covered, 0);
    // Conversely, a new covering box replaces the first slice in place. Other pending slices
    // remain; the queue does not promise to remove every redundant item.
    h.push(original.bounds, original.source);
    h.waiting[0] = original;
    h.stats.covered = 1;
    h.check();
    h.budgets(2, 1);
    h.drain();
    assert_eq!(h.stats.slices as usize, 2 * slices(original).len());
    assert_eq!(h.stats.received, 3);
}
