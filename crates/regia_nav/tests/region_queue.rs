// SPDX-License-Identifier: MPL-2.0
//! The region queue (ADR-1934): budgets, ordering, merging, a source that is not ready, and replays.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, NavCrowd, NavCrowdConfig, NavCrowdPlugin, NavDirtySource, NavMeshBuildParams,
    NavObstacleShape, NavRegionDirty, NavRegionPlugin, NavRegionQueue, NavTileCache, NavTileCacheParams,
    apply_nav_regions,
};

fn params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 16,
    }
}

fn quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> Vec<[Vec3; 3]> {
    vec![
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

fn floor() -> Vec<[Vec3; 3]> {
    quad(-8.0, 8.0, -8.0, 8.0, 0.0)
}

/// A floor with a raised strip along `x` across the whole map.
fn edited() -> Vec<[Vec3; 3]> {
    let mut t = quad(-8.0, 8.0, -8.0, 0.0, 0.0);
    t.extend(quad(-8.0, 8.0, 0.0, 1.0, 2.0));
    t.extend(quad(-8.0, 8.0, 1.0, 8.0, 0.0));
    t
}

/// A world with the cache, the queue and a source serving `geometry`.
fn world(geometry: Vec<[Vec3; 3]>) -> World {
    let mut world = World::new();
    world.insert_resource(NavTileCache::build(params(), &floor()).expect("builds"));
    let mut queue = NavRegionQueue::default();
    queue.set_source(Arc::new(move |_, _, _, out| {
        out.extend_from_slice(&geometry);
        true
    }));
    world.insert_resource(queue);
    world
}

fn queue(world: &mut World) -> Mut<'_, NavRegionQueue> {
    world.resource_mut::<NavRegionQueue>()
}

const STRIP_LO: Vec3 = Vec3::new(-8.0, 0.0, 0.0);
const STRIP_HI: Vec3 = Vec3::new(8.0, 2.0, 1.0);

/// Steps until the queue is empty and no tile waits; returns how many steps it took.
fn run_dry(world: &mut World) -> usize {
    let mut steps = 0;
    loop {
        apply_nav_regions(world);
        steps += 1;
        let cache = world.resource::<NavTileCache>();
        if world.resource::<NavRegionQueue>().is_empty() && cache.pending_tiles().is_empty() {
            return steps;
        }
        assert!(steps < 1000, "the queue never drained");
    }
}

#[test]
fn a_box_across_the_map_ends_on_the_mesh_of_a_full_build() {
    let mut w = world(edited());
    queue(&mut w).push(STRIP_LO, STRIP_HI, NavDirtySource::Destruction);
    run_dry(&mut w);
    let want = NavTileCache::build(params(), &edited()).expect("builds");
    assert!(
        w.resource::<NavTileCache>().mesh().polys == want.mesh().polys,
        "mesh differs"
    );
    assert!(w.resource::<NavRegionQueue>().stats().cells_changed > 0);
}

#[test]
fn the_budget_spreads_one_box_over_steps_with_the_same_result() {
    let mut fast = world(edited());
    queue(&mut fast).config.max_slices_per_step = 1000;
    queue(&mut fast).push(STRIP_LO, STRIP_HI, NavDirtySource::Terrain);
    let fast_steps = run_dry(&mut fast);

    let mut slow = world(edited());
    queue(&mut slow).config.max_slices_per_step = 1;
    queue(&mut slow).config.max_tiles_per_step = 1;
    queue(&mut slow).push(STRIP_LO, STRIP_HI, NavDirtySource::Terrain);
    let slow_steps = run_dry(&mut slow);

    assert!(slow_steps > fast_steps, "{slow_steps} vs {fast_steps}");
    let (a, b) = (slow.resource::<NavTileCache>(), fast.resource::<NavTileCache>());
    assert!(a.mesh().polys == b.mesh().polys, "the budget changed the mesh");
    assert_eq!(
        slow.resource::<NavRegionQueue>().stats().slices,
        fast.resource::<NavRegionQueue>().stats().slices
    );
}

#[test]
fn identical_runs_rebuild_the_same_tiles_on_the_same_steps() {
    let trace = || {
        let mut w = world(edited());
        queue(&mut w).config.max_slices_per_step = 2;
        queue(&mut w).push(STRIP_LO, STRIP_HI, NavDirtySource::Destruction);
        let mut log = Vec::new();
        for _ in 0..40 {
            apply_nav_regions(&mut w);
            let cache = w.resource::<NavTileCache>();
            log.push((
                cache.total_rebuilds(),
                cache.revision(),
                cache.mesh().poly_count(),
            ));
        }
        log
    };
    assert_eq!(trace(), trace());
}

#[test]
fn a_covered_box_is_dropped_and_a_covering_box_replaces_the_small_one() {
    let mut q = NavRegionQueue::default();
    let big = (Vec3::new(-2.0, 0.0, -2.0), Vec3::new(2.0, 1.0, 2.0));
    let small = (Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 1.0, 1.0));
    q.push(big.0, big.1, NavDirtySource::Terrain);
    q.push(small.0, small.1, NavDirtySource::Destruction);
    assert_eq!(q.len(), 1);
    assert_eq!(q.stats().covered, 1);

    let mut q = NavRegionQueue::default();
    q.push(small.0, small.1, NavDirtySource::Destruction);
    q.push(big.0, big.1, NavDirtySource::Terrain);
    assert_eq!(q.len(), 1);
    assert_eq!(q.boxes().next(), Some((big.0, big.1, "terrain")));
}

#[test]
fn a_full_queue_merges_and_loses_no_area() {
    let mut q = NavRegionQueue::default();
    q.config.max_queued = 2;
    for i in 0..5 {
        let x = i as f32 * 10.0;
        q.push(
            Vec3::new(x, 0.0, 0.0),
            Vec3::new(x + 1.0, 1.0, 1.0),
            NavDirtySource::Game,
        );
    }
    assert_eq!(q.len(), 2);
    assert_eq!(q.stats().overflowed, 3);
    let (_, hi, _) = q.boxes().last().expect("a box");
    assert_eq!(hi.x, 41.0, "the last box reaches the last edit");
}

#[test]
fn non_finite_and_off_grid_boxes_are_counted_not_applied() {
    let mut w = world(edited());
    queue(&mut w).push(Vec3::splat(f32::NAN), Vec3::ONE, NavDirtySource::Game);
    queue(&mut w).push(Vec3::splat(500.0), Vec3::splat(501.0), NavDirtySource::Game);
    run_dry(&mut w);
    let stats = w.resource::<NavRegionQueue>().stats();
    assert_eq!(stats.received, 2);
    assert_eq!(stats.rejected, 2);
    assert_eq!(w.resource::<NavTileCache>().revision(), 0);
}

#[test]
fn no_source_keeps_the_box_until_one_is_set() {
    let mut w = world(edited());
    w.insert_resource(NavRegionQueue::default());
    queue(&mut w).push(STRIP_LO, STRIP_HI, NavDirtySource::Terrain);
    apply_nav_regions(&mut w);
    assert_eq!(w.resource::<NavRegionQueue>().len(), 1);
    assert_eq!(w.resource::<NavTileCache>().revision(), 0);
    let geometry = edited();
    queue(&mut w).set_source(Arc::new(move |_, _, _, out| {
        out.extend_from_slice(&geometry);
        true
    }));
    run_dry(&mut w);
    assert!(w.resource::<NavRegionQueue>().stats().cells_changed > 0);
}

#[test]
fn a_source_that_is_not_ready_holds_the_order_and_is_asked_again() {
    let mut w = world(edited());
    let ready = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let (r, a, geometry) = (ready.clone(), asked.clone(), edited());
    queue(&mut w).set_source(Arc::new(move |_, _, _, out| {
        a.fetch_add(1, Ordering::SeqCst);
        if !r.load(Ordering::SeqCst) {
            return false;
        }
        out.extend_from_slice(&geometry);
        true
    }));
    queue(&mut w).push(STRIP_LO, STRIP_HI, NavDirtySource::Terrain);
    apply_nav_regions(&mut w);
    apply_nav_regions(&mut w);
    assert_eq!(asked.load(Ordering::SeqCst), 2, "one try per step");
    assert_eq!(w.resource::<NavRegionQueue>().stats().source_waits, 2);
    assert_eq!(w.resource::<NavTileCache>().revision(), 0);
    ready.store(true, Ordering::SeqCst);
    run_dry(&mut w);
    let want = NavTileCache::build(params(), &edited()).expect("builds");
    assert!(
        w.resource::<NavTileCache>().mesh().polys == want.mesh().polys,
        "mesh differs"
    );
}

#[test]
fn bad_geometry_from_the_source_is_counted_and_the_mesh_stays() {
    let mut w = world(edited());
    queue(&mut w).set_source(Arc::new(|_, _, _, out| {
        out.push([Vec3::splat(f32::NAN), Vec3::ZERO, Vec3::ONE]);
        true
    }));
    queue(&mut w).push(STRIP_LO, STRIP_HI, NavDirtySource::Game);
    run_dry(&mut w);
    assert!(w.resource::<NavRegionQueue>().stats().failed > 0);
    assert_eq!(w.resource::<NavTileCache>().revision(), 0);
}

#[test]
fn a_streamed_edit_comes_back_through_the_queue_and_opens_the_gate() {
    let whole = NavTileCache::build(params(), &floor()).expect("builds");
    let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    let (tx, tz) = whole.tile_grid();
    let mut cells = Vec::new();
    for z in 0..tz {
        for x in 0..tx {
            let id = u64::from(z * tx + x);
            let blob = whole.cook_cell(&[(x, z)]).expect("cooks");
            cache.load_cell(id, &blob).expect("loads");
            cells.push((id, blob));
        }
    }
    let mut w = world(edited());
    w.insert_resource(cache);
    run_dry(&mut w);
    queue(&mut w).push(STRIP_LO, STRIP_HI, NavDirtySource::Terrain);
    run_dry(&mut w);
    let want = w.resource::<NavTileCache>().mesh().polys.clone();

    // Every tile under the strip leaves and returns as cooked.
    let tiles = w.resource::<NavTileCache>().tiles_in(STRIP_LO, STRIP_HI);
    for &(x, z) in &tiles {
        w.resource_mut::<NavTileCache>()
            .unload_cell(u64::from(z * tx + x))
            .expect("unloads");
    }
    run_dry(&mut w);
    for &(x, z) in &tiles {
        let id = u64::from(z * tx + x);
        let blob = cells
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, b)| b.as_slice())
            .expect("a blob");
        w.resource_mut::<NavTileCache>()
            .load_cell(id, blob)
            .expect("reloads");
    }
    let first = u64::from(tiles[0].1 * tx + tiles[0].0);
    assert!(!w.resource::<NavTileCache>().cell_ready(first));
    run_dry(&mut w);
    let cache = w.resource::<NavTileCache>();
    assert!(cache.cell_ready(first), "the replay ran");
    assert_eq!(cache.stream_status().replay_pending, 0);
    assert!(cache.mesh().polys == want, "the edit came back");
}

/// A streamed world holding every tile, with one edit applied and then `tile`, which must lie under
/// the strip, reloaded as cooked.
fn reloaded_after_an_edit(tile: (u32, u32)) -> (World, u64) {
    let whole = NavTileCache::build(params(), &floor()).expect("builds");
    let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    let (tx, tz) = whole.tile_grid();
    for z in 0..tz {
        for x in 0..tx {
            let blob = whole.cook_cell(&[(x, z)]).expect("cooks");
            cache.load_cell(u64::from(z * tx + x), &blob).expect("loads");
        }
    }
    cache.update();
    // Applied whole, not through the queue: the edit crosses tile borders, so the replay box is
    // wider than any tile slice and only an unsliced rebuild can cover it.
    cache
        .rebuild_region(STRIP_LO, STRIP_HI, &edited())
        .expect("rebuilds");
    cache.update();
    let mut w = world(edited());
    w.insert_resource(cache);
    let id = u64::from(tile.1 * tx + tile.0);
    let blob = whole.cook_cell(&[tile]).expect("cooks");
    let mut cache = w.resource_mut::<NavTileCache>();
    cache.unload_cell(id).expect("unloads");
    cache.load_cell(id, &blob).expect("reloads");
    assert!(
        cache.stream_status().replay_pending > 0,
        "tile {tile:?} is not under the edit"
    );
    assert!(!cache.cell_ready(id), "the gate waits for the replay");
    (w, id)
}

#[test]
fn a_replay_a_covering_box_arrives_on_still_opens_the_gate() {
    // A big box lands on the queue first: dropping the replay under it, then slicing it along tile
    // borders, would leave no rebuild that covers the replay's box.
    let (mut w, cell) = reloaded_after_an_edit((1, 2));
    queue(&mut w).push(Vec3::splat(-8.0), Vec3::splat(8.0), NavDirtySource::Game);
    run_dry(&mut w);
    let cache = w.resource::<NavTileCache>();
    assert_eq!(cache.stream_status().replay_pending, 0);
    assert!(cache.cell_ready(cell), "the gate stayed shut");
}

#[test]
fn a_covering_box_does_not_replace_a_queued_replay() {
    let (mut w, cell) = reloaded_after_an_edit((1, 2));
    let boxes = w.resource_mut::<NavTileCache>().take_replay_regions();
    assert_eq!(boxes.len(), 1);
    queue(&mut w).push(boxes[0].0, boxes[0].1, NavDirtySource::Replay);
    queue(&mut w).push(Vec3::splat(-8.0), Vec3::splat(8.0), NavDirtySource::Game);
    assert_eq!(w.resource::<NavRegionQueue>().len(), 2);
    run_dry(&mut w);
    assert!(
        w.resource::<NavTileCache>().cell_ready(cell),
        "the gate stayed shut"
    );
}

#[test]
fn a_full_queue_never_merges_a_replay() {
    let mut q = NavRegionQueue::default();
    q.config.max_queued = 1;
    let replay = (Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
    q.push(replay.0, replay.1, NavDirtySource::Replay);
    q.push(
        Vec3::new(5.0, 0.0, 5.0),
        Vec3::new(6.0, 1.0, 6.0),
        NavDirtySource::Game,
    );
    assert_eq!(
        q.boxes().next(),
        Some((replay.0, replay.1, "replay")),
        "the replay box kept its size"
    );
    assert_eq!(q.len(), 2);
    q.push(
        Vec3::new(9.0, 0.0, 9.0),
        Vec3::new(10.0, 1.0, 10.0),
        NavDirtySource::Game,
    );
    assert_eq!(q.len(), 2, "later boxes merge into the box that is not a replay");
    assert_eq!(q.stats().overflowed, 1);
}

#[test]
fn a_box_thinner_than_a_cell_still_rebuilds() {
    // No cell center lies inside this box, but the edit under it is real.
    let tall = {
        let mut t = floor();
        t.extend(quad(0.05, 0.2, -8.0, 8.0, 2.0));
        t
    };
    let mut w = world(tall);
    queue(&mut w).push(
        Vec3::new(0.06, 0.0, -8.0),
        Vec3::new(0.07, 2.0, 8.0),
        NavDirtySource::Destruction,
    );
    run_dry(&mut w);
    let stats = w.resource::<NavRegionQueue>().stats();
    assert_eq!(stats.rejected, 0, "dropped as if it missed the grid");
    assert!(stats.slices > 0);
}

#[test]
fn a_zero_size_box_on_the_grid_is_not_rejected() {
    let mut w = world(edited());
    queue(&mut w).push(
        Vec3::new(1.0, 0.0, 0.5),
        Vec3::new(1.0, 0.0, 0.5),
        NavDirtySource::Game,
    );
    run_dry(&mut w);
    let stats = w.resource::<NavRegionQueue>().stats();
    assert_eq!(stats.rejected, 0);
    assert!(stats.cells_changed > 0, "the point sits on the raised strip");
}

#[test]
fn the_plugin_reads_messages_and_runs_in_the_fixed_step() {
    let mut app = App::new();
    app.add_plugins(NavRegionPlugin);
    app.insert_resource(NavTileCache::build(params(), &floor()).expect("builds"));
    let geometry = edited();
    app.world_mut()
        .resource_mut::<NavRegionQueue>()
        .set_source(Arc::new(move |_, _, _, out| {
            out.extend_from_slice(&geometry);
            true
        }));
    app.world_mut().write_message(NavRegionDirty {
        min: STRIP_LO,
        max: STRIP_HI,
        source: NavDirtySource::Destruction,
    });
    for _ in 0..40 {
        app.world_mut().run_schedule(FixedUpdate);
    }
    let stats = app.world().resource::<NavRegionQueue>().stats();
    assert_eq!(stats.received, 1);
    assert!(stats.cells_changed > 0);
    assert!(app.world().resource::<NavRegionQueue>().is_empty());
}

#[test]
fn a_crowd_app_keeps_the_tile_budget() {
    let mut app = App::new();
    app.add_plugins((NavRegionPlugin, NavCrowdPlugin));
    app.insert_resource(NavTileCache::build(params(), &floor()).expect("builds"));
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let at = Vec3::new(4.0, 0.0, 4.0);
    {
        let cache = app.world().resource::<NavTileCache>();
        crowd
            .add_agent(cache.mesh(), at, CrowdAgentParams::default())
            .expect("on the mesh");
    }
    app.insert_resource(crowd);
    app.world_mut()
        .resource_mut::<NavRegionQueue>()
        .config
        .max_tiles_per_step = 1;
    let shape = NavObstacleShape::Box {
        min: Vec3::new(-7.0, -1.0, -7.0),
        max: Vec3::new(7.0, 1.0, 7.0),
    };
    let queued = {
        let mut cache = app.world_mut().resource_mut::<NavTileCache>();
        cache.add_obstacle(shape).expect("valid obstacle");
        cache.pending_tile_count()
    };
    assert!(queued > 1);
    app.world_mut().run_schedule(FixedUpdate);
    let cache = app.world().resource::<NavTileCache>();
    assert_eq!(
        cache.pending_tile_count(),
        queued - 1,
        "one tile per step, crowd or not"
    );
}

#[test]
fn a_replay_the_cache_refuses_opens_the_gate_as_cooked() {
    let (mut w, cell) = reloaded_after_an_edit((1, 2));
    queue(&mut w).set_source(Arc::new(|_, _, _, out| {
        out.push([Vec3::splat(f32::NAN), Vec3::ZERO, Vec3::ONE]);
        true
    }));
    assert!(
        !w.resource::<NavTileCache>().cell_ready(cell),
        "a replay is waiting"
    );
    run_dry(&mut w);
    assert!(w.resource::<NavRegionQueue>().stats().failed > 0);
    assert!(
        w.resource::<NavTileCache>().cell_ready(cell),
        "a shut gate would stall the level for good"
    );
}

#[test]
fn waiting_boxes_move_with_the_origin() {
    let mut w = world(edited());
    queue(&mut w).push(Vec3::ZERO, Vec3::ONE, NavDirtySource::Game);
    let delta = Vec3::new(100.0, 2.0, -50.0);
    queue(&mut w).shift_origin(delta);
    let (lo, hi, _) = w.resource::<NavRegionQueue>().boxes().next().expect("a box");
    assert_eq!((lo, hi), (delta, Vec3::ONE + delta));
}
