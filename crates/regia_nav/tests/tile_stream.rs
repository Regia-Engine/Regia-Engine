// SPDX-License-Identifier: MPL-2.0
//! Nav tile streaming (ADR-1934, spec 78 § 13.2): cooked cells load and unload around an empty
//! cache, and the loaded surface matches the whole cache.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentParams, CrowdAgentState, NavAreaVolume, NavCrowd, NavCrowdConfig, NavMeshBuildParams,
    NavObstacleShape, NavPathOutcome, NavQueryFilter, NavTileCache, NavTileCacheError, NavTileCacheParams,
    OffMeshLink,
};

const FAR: Vec3 = Vec3::splat(2.0);

fn params(tile_size: u32) -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size,
    }
}

fn floor() -> Vec<[Vec3; 3]> {
    let c = 16.0;
    vec![
        [
            Vec3::new(-c, 0.0, -c),
            Vec3::new(c, 0.0, -c),
            Vec3::new(c, 0.0, c),
        ],
        [
            Vec3::new(-c, 0.0, -c),
            Vec3::new(c, 0.0, c),
            Vec3::new(-c, 0.0, c),
        ],
    ]
}

/// Two triangles covering `[x0, x1] x [z0, z1]` at `y = 0`.
fn quad(x0: f32, x1: f32, z0: f32, z1: f32) -> Vec<[Vec3; 3]> {
    vec![
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
        ],
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
            Vec3::new(x0, 0.0, z1),
        ],
    ]
}

/// The floor, with a wall across `x` at `0` except for a gap at `z > 8`.
fn floor_with_wall() -> Vec<[Vec3; 3]> {
    let mut t = floor();
    let (x0, x1, z0, z1, y) = (-0.5, 0.5, -16.0, 8.0, 3.0);
    t.push([Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)]);
    t.push([Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)]);
    t
}

/// A whole cache, an empty streamed twin and one cooked cell per tile, keyed by tile index.
fn world(tile_size: u32, tris: &[[Vec3; 3]]) -> (NavTileCache, NavTileCache, Vec<(u64, Vec<u8>)>) {
    let whole = NavTileCache::build(params(tile_size), tris).expect("builds");
    let streamed = NavTileCache::streamed(params(tile_size), whole.layout()).expect("streamed");
    let (tx, tz) = whole.tile_grid();
    let mut cells = Vec::new();
    for z in 0..tz {
        for x in 0..tx {
            let id = u64::from(z * tx + x);
            cells.push((id, whole.cook_cell(&[(x, z)]).expect("cooks")));
        }
    }
    (whole, streamed, cells)
}

#[test]
fn a_streamed_cache_starts_empty_and_every_cell_rebuilds_the_whole_mesh() {
    let (whole, mut cache, cells) = world(64, &floor());
    assert!(cache.is_streamed());
    assert_eq!(cache.mesh().poly_count(), 0);
    assert!(cache.query().find_nearest(Vec3::ZERO, FAR).is_none());
    for (id, blob) in &cells {
        let load = cache.load_cell(*id, blob).expect("loads");
        assert_eq!(load.added, load.tiles);
    }
    cache.update();
    assert!(
        cache.mesh().polys == whole.mesh().polys,
        "the loaded mesh differs from the whole one"
    );
    assert_eq!(cache.mesh().region_count, whole.mesh().region_count);
    let status = cache.stream_status();
    assert_eq!(status.tiles_loaded, status.tiles_total);
    assert_eq!(status.cells_loaded, cells.len());
}

#[test]
fn cooking_is_deterministic_and_ignores_the_tile_order() {
    let (whole, ..) = world(64, &floor());
    let a = whole.cook_cell(&[(0, 0), (1, 0)]).expect("cooks");
    let b = whole.cook_cell(&[(1, 0), (0, 0), (1, 0)]).expect("cooks");
    assert_eq!(a, b);
}

#[test]
fn a_cell_of_two_tiles_loads_both_and_a_shared_tile_stays_until_its_last_cell_goes() {
    let (whole, mut cache, _) = world(64, &floor());
    let a = whole.cook_cell(&[(0, 0), (1, 0)]).expect("cooks");
    let b = whole.cook_cell(&[(1, 0), (1, 1)]).expect("cooks");
    assert_eq!(cache.load_cell(1, &a).expect("a").added.len(), 2);
    assert_eq!(cache.load_cell(2, &b).expect("b").added, vec![(1, 1)]);
    assert_eq!(cache.unload_cell(1).expect("a goes"), vec![(0, 0)]);
    assert_eq!(cache.tile_loaded(1, 0), Some(true), "still held by cell 2");
    assert_eq!(cache.unload_cell(2).expect("b goes"), vec![(1, 0), (1, 1)]);
    assert_eq!(cache.tile_loaded(1, 0), Some(false));
}

#[test]
fn unloading_removes_polygons_and_reloading_restores_them() {
    let (whole, mut cache, cells) = world(64, &floor());
    for (id, blob) in &cells {
        cache.load_cell(*id, blob).expect("loads");
    }
    cache.update();
    let revision = cache.revision();
    assert_eq!(cache.unload_cell(0).expect("unloads"), vec![(0, 0)]);
    cache.update();
    assert!(cache.revision() > revision, "crowds replan through the revision");
    assert!(cache.mesh().poly_count() < whole.mesh().poly_count());
    assert!(
        cache
            .query()
            .find_nearest(Vec3::new(-8.0, 0.0, -8.0), Vec3::splat(0.5))
            .is_none(),
        "tile (0, 0) is gone"
    );
    cache.load_cell(0, &cells[0].1).expect("reloads");
    cache.update();
    assert!(
        cache.mesh().polys == whole.mesh().polys,
        "the reloaded mesh differs"
    );
}

#[test]
fn loading_twice_is_a_no_op_and_unloading_an_unknown_cell_is_an_error() {
    let (_, mut cache, cells) = world(64, &floor());
    cache.load_cell(0, &cells[0].1).expect("loads");
    let again = cache.load_cell(0, &cells[0].1).expect("loads again");
    assert!(again.already_loaded);
    assert!(again.added.is_empty());
    cache.unload_cell(0).expect("one unload is enough");
    assert_eq!(cache.tile_loaded(0, 0), Some(false));
    let err = cache.unload_cell(0).expect_err("gone");
    assert_eq!(err, NavTileCacheError::UnknownCell(0));
    assert_eq!(err.code(), "nav_unknown_cell");
}

#[test]
fn the_gate_opens_after_the_update_that_adds_the_tiles() {
    let (_, mut cache, cells) = world(64, &floor());
    assert!(!cache.cell_ready(0), "not loaded");
    cache.load_cell(0, &cells[0].1).expect("loads");
    assert!(!cache.cell_ready(0), "queued");
    cache.update();
    assert!(cache.cell_ready(0));
    assert_eq!(cache.loaded_cells(), vec![0]);
}

#[test]
fn damaged_and_foreign_blobs_are_typed_errors_and_change_nothing() {
    let (_, mut cache, cells) = world(64, &floor());
    let blob = &cells[0].1;

    for (name, bytes, code) in [
        ("empty", Vec::new(), "nav_cell_corrupt"),
        ("not a cell", vec![7; 64], "nav_cell_corrupt"),
        ("truncated", blob[..blob.len() / 2].to_vec(), "nav_cell_corrupt"),
        ("header only", blob[..10].to_vec(), "nav_cell_corrupt"),
        ("flipped byte", flipped(blob, 40), "nav_cell_corrupt"),
    ] {
        let err = cache.load_cell(9, &bytes).expect_err(name);
        assert_eq!(err.code(), code, "{name}");
    }

    let mut newer = blob.clone();
    newer[4..8].copy_from_slice(&99u32.to_le_bytes());
    assert_eq!(
        cache.load_cell(9, &newer).expect_err("newer").code(),
        "nav_cell_version"
    );

    let other = NavTileCache::build(params(32), &floor()).expect("builds");
    let foreign = other.cook_cell(&[(0, 0)]).expect("cooks");
    assert_eq!(
        cache.load_cell(9, &foreign).expect_err("foreign").code(),
        "nav_cell_layout_mismatch"
    );

    assert_eq!(cache.stream_status().cells_loaded, 0);
    assert_eq!(cache.stream_status().tiles_loaded, 0);
    assert!(cache.update().rebuilt.is_empty());
}

fn flipped(blob: &[u8], at: usize) -> Vec<u8> {
    let mut out = blob.to_vec();
    out[at] ^= 0x40;
    out
}

#[test]
fn streaming_calls_on_a_whole_cache_and_bad_layouts_are_typed_errors() {
    let (mut whole, _, cells) = world(64, &floor());
    assert_eq!(
        whole.load_cell(0, &cells[0].1).expect_err("whole").code(),
        "nav_not_streamed"
    );
    assert_eq!(
        whole.unload_cell(0).expect_err("whole").code(),
        "nav_not_streamed"
    );
    assert_eq!(
        whole.cook_cell(&[(99, 0)]).expect_err("range").code(),
        "nav_tile_out_of_range"
    );

    let mut layout = whole.layout();
    layout.width = 0;
    assert_eq!(
        NavTileCache::streamed(params(64), layout)
            .expect_err("empty")
            .code(),
        "nav_invalid_layout"
    );
    layout = whole.layout();
    layout.origin.x = f32::NAN;
    assert_eq!(
        NavTileCache::streamed(params(64), layout)
            .expect_err("nan")
            .code(),
        "nav_invalid_layout"
    );
    assert_eq!(
        NavTileCache::streamed(params(0), whole.layout())
            .expect_err("tile")
            .code(),
        "nav_invalid_tile_size"
    );
}

#[test]
fn an_obstacle_outlives_its_tile() {
    let (_, mut cache, cells) = world(64, &floor());
    for (id, blob) in &cells {
        cache.load_cell(*id, blob).expect("loads");
    }
    let spot = Vec3::new(-8.0, 0.0, -8.0);
    cache
        .add_obstacle(NavObstacleShape::Cylinder {
            base: spot,
            radius: 1.0,
            height: 2.0,
        })
        .expect("adds");
    cache.update();
    cache.unload_cell(0).expect("unloads");
    cache.update();
    assert_eq!(cache.obstacle_count(), 1);
    cache.load_cell(0, &cells[0].1).expect("reloads");
    cache.update();
    assert!(
        cache.query().find_nearest(spot, Vec3::splat(0.1)).is_none(),
        "still carved"
    );
}

#[test]
fn a_path_into_an_unloaded_tile_stops_at_its_border() {
    let (_, mut cache, cells) = world(64, &floor());
    for (id, blob) in &cells {
        cache.load_cell(*id, blob).expect("loads");
    }
    cache.update();
    let (start, end) = (Vec3::new(-8.0, 0.0, -8.0), Vec3::new(8.0, 0.0, -8.0));
    let full = cache.find_path_streamed(start, end, FAR);
    assert_eq!(full.outcome, NavPathOutcome::Complete);

    let (corner, _) = cache.tile_bounds(1, 1).expect("a tile");
    cache.unload_cell(1).expect("unloads the east tile");
    cache.update();
    let partial = cache.find_path_streamed(start, end, FAR);
    assert_eq!(partial.outcome, NavPathOutcome::Partial);
    assert_eq!(partial.outcome.name(), "nav_tile_unloaded");
    assert_eq!(partial.unloaded_tile, Some((1, 0)));
    // The end sits in tile (1, 0): the closest reachable surface is straight across its border
    // with the loaded tile (1, 1), not the nearer-looking west border of the tile.
    let last = *partial
        .path
        .expect("a path")
        .waypoints
        .last()
        .expect("a waypoint");
    assert!((last.x - end.x).abs() < 1e-3, "above the end: {last}");
    assert!((last.z - corner.z).abs() < 0.3, "on the tile border: {last}");
}

#[test]
fn a_disconnected_goal_with_everything_loaded_is_no_path() {
    let mut tris = floor();
    let (x0, x1, y) = (-0.5, 0.5, 3.0);
    tris.push([
        Vec3::new(x0, y, -16.0),
        Vec3::new(x1, y, -16.0),
        Vec3::new(x1, y, 16.0),
    ]);
    tris.push([
        Vec3::new(x0, y, -16.0),
        Vec3::new(x1, y, 16.0),
        Vec3::new(x0, y, 16.0),
    ]);
    let (_, mut cache, cells) = world(64, &tris);
    for (id, blob) in &cells {
        cache.load_cell(*id, blob).expect("loads");
    }
    cache.update();
    let r = cache.find_path_streamed(Vec3::new(-8.0, 0.0, 0.0), Vec3::new(8.0, 0.0, 0.0), FAR);
    assert_eq!(r.outcome, NavPathOutcome::NoPath);
    assert_eq!(r.outcome.name(), "no_path");
    assert!(r.path.is_none());
}

#[test]
fn a_region_edit_survives_an_unload_through_a_replay() {
    let (_, mut cache, cells) = world(64, &floor());
    for (id, blob) in &cells {
        cache.load_cell(*id, blob).expect("loads");
    }
    cache.update();
    let (lo, hi) = (Vec3::new(-0.5, 0.0, -16.0), Vec3::new(0.5, 3.0, 8.0));
    let edited = floor_with_wall();
    let done = cache.rebuild_region(lo, hi, &edited).expect("rebuilds");
    assert!(done.cells_changed > 0);
    cache.update();
    let want = cache.mesh().polys.clone();
    assert!(cache.take_replay_regions().is_empty(), "nothing was unloaded");

    // Tile (1, 0) holds the wall and comes back as cooked, without it.
    cache.unload_cell(1).expect("unloads");
    cache.update();
    cache.load_cell(1, &cells[1].1).expect("reloads");
    cache.update();
    assert!(!cache.cell_ready(1), "the edit is not back yet");
    assert!(cache.mesh().polys != want, "the reloaded tile lost the wall");

    let boxes = cache.take_replay_regions();
    assert_eq!(boxes.len(), 1);
    assert!(cache.take_replay_regions().is_empty(), "handed out once");
    assert_eq!(cache.stream_status().replay_pending, 1);
    for (a, b) in boxes {
        cache.rebuild_region(a, b, &edited).expect("replays");
    }
    cache.update();
    assert!(cache.cell_ready(1));
    assert_eq!(cache.stream_status().replay_pending, 0);
    assert!(cache.mesh().polys == want, "the replay did not restore the edit");
}

#[test]
fn an_edit_over_an_unloaded_tile_waits_for_it_and_does_not_erode_its_neighbors() {
    let (_, mut cache, cells) = world(64, &floor());
    for (id, blob) in &cells {
        if *id != 1 {
            cache.load_cell(*id, blob).expect("loads");
        }
    }
    cache.update();
    let want = cache.mesh().polys.clone();

    // The geometry of tile (1, 0) is gone with it: the source returns everything else.
    let (t_min, t_max) = cache.tile_bounds(1, 0).expect("a tile");
    let c = 16.0;
    let mut rest = quad(-c, t_min.x, -c, c);
    rest.extend(quad(t_min.x, c, t_max.z, c));
    let (lo, hi) = (
        Vec3::new(t_min.x - 1.0, 0.0, t_max.z - 2.0),
        Vec3::new(t_min.x + 1.0, 1.0, t_max.z - 1.0),
    );
    let done = cache.rebuild_region(lo, hi, &rest).expect("rebuilds");
    assert!(done.deferred.contains(&(1, 0)), "{:?}", done.deferred);
    cache.update();
    assert!(
        cache.mesh().polys == want,
        "the loaded cells beside the missing tile eroded"
    );

    cache.load_cell(1, &cells[1].1).expect("loads");
    assert_eq!(cache.take_replay_regions().len(), 1, "the box waits for the tile");
}

#[test]
fn shifting_the_origin_moves_tiles_obstacles_and_later_cells() {
    let (whole, mut cache, cells) = world(64, &floor());
    for (id, blob) in cells.iter().filter(|(id, _)| *id != 3) {
        cache.load_cell(*id, blob).expect("loads");
    }
    let spot = Vec3::new(-8.0, 0.0, -8.0);
    cache
        .add_obstacle(NavObstacleShape::Cylinder {
            base: spot,
            radius: 1.0,
            height: 2.0,
        })
        .expect("adds");
    cache.update();
    let revision = cache.revision();
    let delta = Vec3::new(128.0, 4.0, -64.0);
    cache.shift_origin(delta);
    assert!(cache.revision() > revision);
    assert_eq!(cache.layout().origin, whole.layout().origin + delta);
    assert!(
        cache
            .query()
            .find_nearest(spot + delta, Vec3::splat(0.1))
            .is_none(),
        "obstacle moved"
    );
    let on_floor = Vec3::new(-4.0, 0.0, -4.0) + delta;
    let (_, p) = cache
        .query()
        .find_nearest(on_floor, Vec3::splat(0.2))
        .expect("floor moved");
    assert!((p.y - delta.y).abs() < 1e-3, "height {}", p.y);

    // A cell cooked in the old frame lands in the new one.
    cache.load_cell(3, &cells[3].1).expect("loads");
    cache.update();
    let east = Vec3::new(8.0, 0.0, 8.0) + delta;
    let (_, p) = cache
        .query()
        .find_nearest(east, Vec3::splat(0.2))
        .expect("new cell in the new frame");
    assert!((p.y - delta.y).abs() < 1e-3, "height {}", p.y);
}

/// A floor cut into two 16-tile-wide cells of 8 m tiles, for the crowd tests.
fn two_cells() -> (NavTileCache, Vec<u8>, Vec<u8>) {
    let whole = NavTileCache::build(params(32), &floor()).expect("builds");
    let (tx, tz) = whole.tile_grid();
    let west: Vec<(u32, u32)> = (0..tz).flat_map(|z| (0..tx / 2).map(move |x| (x, z))).collect();
    let east: Vec<(u32, u32)> = (0..tz).flat_map(|z| (tx / 2..tx).map(move |x| (x, z))).collect();
    let (a, b) = (
        whole.cook_cell(&west).expect("cooks"),
        whole.cook_cell(&east).expect("cooks"),
    );
    let mut cache = NavTileCache::streamed(params(32), whole.layout()).expect("streamed");
    cache.load_cell(0, &a).expect("loads");
    cache.load_cell(1, &b).expect("loads");
    cache.update();
    (cache, a, b)
}

fn agent_params() -> CrowdAgentParams {
    CrowdAgentParams::default()
}

fn crowd_step(crowd: &mut NavCrowd, cache: &mut NavTileCache) {
    cache.update();
    crowd.sync_mesh_revision(cache.revision());
    crowd.sync_streaming(cache);
    crowd.step(cache.mesh());
}

#[test]
fn an_agent_on_a_removed_tile_sleeps_keeps_its_target_and_finishes_when_it_returns() {
    let (mut cache, _, east) = two_cells();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(3.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    let target = Vec3::new(10.0, 0.0, 0.0);
    crowd.set_target(cache.mesh(), id, target).expect("on the mesh");
    for _ in 0..30 {
        crowd_step(&mut crowd, &mut cache);
    }
    let held = crowd.agent(id).expect("agent").position();
    assert!(held.x > 3.0, "it walked: {held:?}");

    cache.unload_cell(1).expect("loaded");
    for _ in 0..60 {
        crowd_step(&mut crowd, &mut cache);
        let a = crowd.agent(id).expect("agent");
        assert_eq!(a.state(), CrowdAgentState::Dormant);
        assert_eq!(a.position(), held, "a dormant agent does not move");
        assert_eq!(a.velocity(), Vec3::ZERO);
        assert_eq!(a.target(), Some(target));
    }
    assert_eq!(crowd.dormant_count(), 1);
    assert_eq!(crowd.stats().dormant_entries, 1, "one entry, not one per step");

    cache.load_cell(1, &east).expect("loads");
    for _ in 0..600 {
        crowd_step(&mut crowd, &mut cache);
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Arrived, "ended at {:?}", a.position());
    assert_eq!(crowd.dormant_count(), 0);
}

#[test]
fn a_dormant_agent_neither_pushes_nor_is_pushed() {
    let (mut cache, _, _) = two_cells();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let sleeper = crowd
        .add_agent(cache.mesh(), Vec3::new(2.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    let walker = crowd
        .add_agent(cache.mesh(), Vec3::new(-6.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    cache.unload_cell(1).expect("loaded");
    let start = crowd.agent(sleeper).expect("agent").position();
    crowd
        .set_target(cache.mesh(), walker, Vec3::new(-0.25, 0.0, 0.0))
        .expect("on the mesh");
    for _ in 0..400 {
        crowd_step(&mut crowd, &mut cache);
        assert_eq!(crowd.agent(sleeper).expect("agent").position(), start);
    }
    assert_eq!(
        crowd.agent(walker).expect("agent").state(),
        CrowdAgentState::Arrived,
        "the sleeper is a tile away and does not affect the walker"
    );
}

#[test]
fn a_tile_back_but_not_yet_in_the_mesh_does_not_wake_an_agent() {
    let (mut cache, _, east) = two_cells();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(3.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    cache.unload_cell(1).expect("loaded");
    crowd_step(&mut crowd, &mut cache);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Dormant);
    cache.load_cell(1, &east).expect("loads");
    crowd.sync_streaming(&cache);
    assert_eq!(
        crowd.agent(id).expect("agent").state(),
        CrowdAgentState::Dormant,
        "queued, not in the mesh"
    );
    cache.update();
    crowd.sync_streaming(&cache);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Idle);
}

#[test]
fn targets_set_and_cleared_while_dormant_apply_on_waking() {
    let (mut cache, _, east) = two_cells();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let cleared = crowd
        .add_agent(cache.mesh(), Vec3::new(3.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    let ordered = crowd
        .add_agent(cache.mesh(), Vec3::new(3.0, 0.0, 6.0), agent_params())
        .expect("on the mesh");
    crowd
        .set_target(cache.mesh(), cleared, Vec3::new(10.0, 0.0, 0.0))
        .expect("on the mesh");
    cache.unload_cell(1).expect("loaded");
    crowd_step(&mut crowd, &mut cache);
    crowd.clear_target(cleared).expect("known agent");
    assert_eq!(
        crowd.agent(cleared).expect("agent").state(),
        CrowdAgentState::Dormant,
        "clearing does not wake it"
    );
    // A target on the west side is on the mesh now; an order to the east would be off it.
    crowd
        .set_target(cache.mesh(), ordered, Vec3::new(-3.0, 0.0, 6.0))
        .expect("on the mesh");
    assert_eq!(
        crowd.agent(ordered).expect("agent").state(),
        CrowdAgentState::Dormant
    );
    cache.load_cell(1, &east).expect("loads");
    for _ in 0..600 {
        crowd_step(&mut crowd, &mut cache);
    }
    assert_eq!(
        crowd.agent(cleared).expect("agent").state(),
        CrowdAgentState::Idle
    );
    assert_eq!(
        crowd.agent(ordered).expect("agent").state(),
        CrowdAgentState::Arrived
    );
}

#[test]
fn a_replay_keeps_agents_asleep_until_it_ran() {
    let (mut cache, _, east) = two_cells();
    let (lo, hi) = (Vec3::new(2.0, -1.0, -2.0), Vec3::new(4.0, 1.0, 2.0));
    cache.rebuild_region(lo, hi, &floor()).expect("rebuilds");
    cache.update();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(3.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    cache.unload_cell(1).expect("loaded");
    crowd_step(&mut crowd, &mut cache);
    cache.load_cell(1, &east).expect("loads");
    cache.update();
    crowd.sync_streaming(&cache);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Dormant);
    for (min, max) in cache.take_replay_regions() {
        cache.rebuild_region(min, max, &floor()).expect("rebuilds");
    }
    cache.update();
    crowd.sync_streaming(&cache);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Idle);
}

#[test]
fn a_cache_built_whole_never_puts_anyone_to_sleep() {
    let mut cache = NavTileCache::build(params(32), &floor()).expect("builds");
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(3.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(-1.0, -1.0, -1.0),
            max: Vec3::new(1.0, 1.0, 1.0),
        })
        .expect("valid obstacle");
    crowd.sync_streaming(&cache);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Idle);
}

#[test]
fn a_dormant_walk_replays_to_the_same_digest() {
    let run = || {
        let (mut cache, _, east) = two_cells();
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        for i in 0..6 {
            let id = crowd
                .add_agent(
                    cache.mesh(),
                    Vec3::new(-5.0 + i as f32 * 2.0, 0.0, -5.0),
                    agent_params(),
                )
                .expect("on the mesh");
            crowd
                .set_target(cache.mesh(), id, Vec3::new(10.0, 0.0, 5.0))
                .expect("on the mesh");
        }
        for step in 0..500 {
            if step == 60 {
                cache.unload_cell(1).expect("loaded");
            }
            if step == 200 {
                cache.load_cell(1, &east).expect("loads");
            }
            crowd_step(&mut crowd, &mut cache);
        }
        crowd.digest()
    };
    assert_eq!(run(), run());
}

#[test]
fn shifting_the_crowd_with_the_cache_finishes_the_same_walk() {
    let walk = |shift: Vec3| {
        let (mut cache, _, _) = two_cells();
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        let id = crowd
            .add_agent(cache.mesh(), Vec3::new(-8.0, 0.0, 0.0), agent_params())
            .expect("on the mesh");
        crowd
            .set_target(cache.mesh(), id, Vec3::new(8.0, 0.0, 0.0))
            .expect("on the mesh");
        for step in 0..900 {
            if step == 60 {
                cache.shift_origin(shift);
                crowd.shift_origin(shift);
            }
            crowd_step(&mut crowd, &mut cache);
        }
        let a = crowd.agent(id).expect("agent");
        (a.state(), a.position() - shift, a.target())
    };
    let shift = Vec3::new(1000.0, 4.0, -2000.0);
    let (state, at, target) = walk(shift);
    assert_eq!(state, CrowdAgentState::Arrived);
    assert_eq!(target, Some(Vec3::new(8.0, 0.0, 0.0) + shift));
    assert!((at - Vec3::new(8.0, 0.0, 0.0)).length() < 0.2, "ended at {at:?}");
}

#[test]
fn painted_areas_come_back_with_a_cooked_cell_and_a_volume_outlives_a_tile() {
    let mut whole = NavTileCache::build(params(32), &floor()).expect("builds");
    whole
        .add_area_volume(NavAreaVolume::new(
            NavObstacleShape::Box {
                min: Vec3::new(-6.0, -1.0, -6.0),
                max: Vec3::new(6.0, 1.0, 6.0),
            },
            3,
        ))
        .expect("valid volume");
    whole.update();
    let (tx, tz) = whole.tile_grid();
    let tiles: Vec<(u32, u32)> = (0..tz).flat_map(|z| (0..tx).map(move |x| (x, z))).collect();
    let blob = whole.cook_cell(&tiles).expect("cooks");
    let mut cache = NavTileCache::streamed(params(32), whole.layout()).expect("streamed");
    let live = cache
        .add_area_volume(NavAreaVolume::new(
            NavObstacleShape::Box {
                min: Vec3::new(8.0, -1.0, 8.0),
                max: Vec3::new(10.0, 1.0, 10.0),
            },
            5,
        ))
        .expect("valid volume");
    cache.load_cell(7, &blob).expect("loads");
    cache.update();
    let area_at = |c: &NavTileCache, x: f32, z: f32| {
        let (p, _) = c
            .query()
            .find_nearest(Vec3::new(x, 0.0, z), FAR)
            .expect("on the mesh");
        c.mesh().polys[p as usize].area
    };
    assert_eq!(area_at(&cache, 0.0, 0.0), 3, "cooked in");
    assert_eq!(area_at(&cache, 9.0, 9.0), 5, "painted over the loaded cell");
    assert_eq!(area_at(&cache, -12.0, 0.0), 0);
    cache.unload_cell(7).expect("loaded");
    cache.update();
    cache.load_cell(7, &blob).expect("loads");
    cache.update();
    assert_eq!(area_at(&cache, 0.0, 0.0), 3);
    assert_eq!(area_at(&cache, 9.0, 9.0), 5, "the volume stayed");
    cache.remove_area_volume(live).expect("registered");
    cache.update();
    assert_eq!(area_at(&cache, 9.0, 9.0), 0);
    assert_eq!(whole.cook_cell(&tiles).expect("cooks"), blob, "deterministic");
}

#[test]
fn an_area_id_past_the_table_in_a_cell_is_corrupt() {
    let whole = NavTileCache::build(params(32), &floor()).expect("builds");
    let mut cache = NavTileCache::streamed(params(32), whole.layout()).expect("streamed");
    let mut blob = whole.cook_cell(&[(0, 0)]).expect("cooks");
    let body = blob.len() - 8;
    // The last area byte of the tile, then the checksum made right again.
    blob[body - 1] = 64;
    let mut sum: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in &blob[..body] {
        sum ^= u64::from(b);
        sum = sum.wrapping_mul(0x0000_0100_0000_01b3);
    }
    blob[body..].copy_from_slice(&sum.to_le_bytes());
    assert_eq!(
        cache.load_cell(0, &blob).map(|_| ()).map_err(|e| e.code()),
        Err("nav_cell_corrupt")
    );
    assert_eq!(cache.loaded_cells(), Vec::<u64>::new());
}

#[test]
fn an_agent_whose_goal_left_reports_no_path_and_goes_on_when_it_returns() {
    let (mut cache, _, east) = two_cells();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-6.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    crowd
        .set_target(cache.mesh(), id, Vec3::new(10.0, 0.0, 0.0))
        .expect("on the mesh");
    cache.unload_cell(1).expect("loaded");
    for _ in 0..30 {
        crowd_step(&mut crowd, &mut cache);
    }
    assert_eq!(
        crowd.agent(id).expect("agent").state(),
        CrowdAgentState::NoPath,
        "it stands on a loaded tile, so it is not dormant"
    );
    assert_eq!(crowd.dormant_count(), 0);
    cache.load_cell(1, &east).expect("loads");
    for _ in 0..900 {
        crowd_step(&mut crowd, &mut cache);
    }
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
}

#[test]
fn the_border_search_crosses_links_and_keeps_to_the_filter() {
    // West and east islands joined by a link. The east island's tile column under the landing is
    // loaded and the one holding the end is not, so only the link reaches that border.
    let mut tris = quad(-16.0, -2.0, -16.0, 16.0);
    tris.extend(quad(2.0, 16.0, -16.0, 16.0));
    let landing = Vec3::new(5.0, 0.0, 0.0);
    let (start, end) = (Vec3::new(-10.0, 0.0, 0.0), Vec3::new(15.5, 0.0, 15.5));
    let whole = NavTileCache::build(params(32), &tris).expect("builds");
    let landing_column = whole.tile_at(landing).expect("on the grid").0;
    assert_ne!(whole.tile_at(end).expect("on the grid").0, landing_column);
    let mut cache = NavTileCache::streamed(params(32), whole.layout()).expect("streamed");
    let (tx, tz) = whole.tile_grid();
    let tiles: Vec<(u32, u32)> = (0..tz)
        .flat_map(|z| (0..tx).map(move |x| (x, z)))
        .filter(|&(x, _)| x < tx / 2 || x == landing_column)
        .collect();
    cache
        .load_cell(0, &whole.cook_cell(&tiles).expect("cooks"))
        .expect("loads");
    cache.update();
    let mut link = OffMeshLink::new(Vec3::new(-3.0, 0.0, 0.0), landing);
    link.flags = 4;
    cache.set_off_mesh_links(vec![link]);
    let result = cache.find_path_streamed(start, end, FAR);
    assert_eq!(result.outcome, NavPathOutcome::Partial, "{result:?}");
    let reached = result.path.expect("a partial path");
    let last = *reached.waypoints.last().expect("waypoints");
    assert!(last.x > 2.0, "it went over the link: {last:?}");

    let no_links = NavQueryFilter {
        exclude_flags: 4,
        ..Default::default()
    };
    let result = cache.find_path_streamed_filtered(start, end, FAR, &no_links);
    let held = result.path.as_ref().and_then(|p| p.waypoints.last()).copied();
    assert!(
        held.is_none_or(|p| p.x < 0.0),
        "the filter keeps it west: {held:?}"
    );
}

#[test]
fn a_queued_rebuild_on_a_loaded_tile_does_not_put_agents_to_sleep() {
    let (mut cache, _, _) = two_cells();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(3.0, 0.0, 0.0), agent_params())
        .expect("on the mesh");
    cache
        .add_area_volume(NavAreaVolume::new(
            NavObstacleShape::Box {
                min: Vec3::new(2.0, -1.0, -1.0),
                max: Vec3::new(4.0, 1.0, 1.0),
            },
            3,
        ))
        .expect("valid volume");
    assert!(cache.pending_tile_count() > 0);
    crowd.sync_streaming(&cache);
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Idle);
    assert!(cache.cell_ready(1), "the gate stays open too");
}

#[test]
fn the_tile_named_is_the_one_toward_the_end() {
    let whole = NavTileCache::build(params(32), &floor()).expect("builds");
    let (tx, tz) = whole.tile_grid();
    let mut cache = NavTileCache::streamed(params(32), whole.layout()).expect("streamed");
    let middle: Vec<(u32, u32)> = (0..tz).map(|z| (tx / 2, z)).collect();
    cache
        .load_cell(0, &whole.cook_cell(&middle).expect("cooks"))
        .expect("loads");
    cache.update();
    let (bx0, bx1) = (
        cache.tile_bounds(tx / 2, 0).expect("in range").0.x,
        cache.tile_bounds(tx / 2, 0).expect("in range").1.x,
    );
    let start = Vec3::new((bx0 + bx1) * 0.5, 0.0, 0.0);
    let end = Vec3::new(bx1 + 6.0, 0.0, 0.0);
    let result = cache.find_path_streamed(start, end, FAR);
    assert_eq!(result.outcome, NavPathOutcome::Partial);
    let (x, _) = result.unloaded_tile.expect("a tile");
    assert_eq!(
        x,
        tx / 2 + 1,
        "the tile on the way to the end, not the first one found"
    );
}

#[test]
fn an_end_snapped_out_of_an_unloaded_tile_is_not_complete() {
    let whole = NavTileCache::build(params(32), &floor()).expect("builds");
    let (tx, tz) = whole.tile_grid();
    let mut cache = NavTileCache::streamed(params(32), whole.layout()).expect("streamed");
    let west: Vec<(u32, u32)> = (0..tz).flat_map(|z| (0..tx / 2).map(move |x| (x, z))).collect();
    cache
        .load_cell(0, &whole.cook_cell(&west).expect("cooks"))
        .expect("loads");
    cache.update();
    let border = cache.tile_bounds(tx / 2, 0).expect("in range").0.x;
    let end = Vec3::new(border + 1.0, 0.0, 0.0);
    let result = cache.find_path_streamed(Vec3::new(-10.0, 0.0, 0.0), end, FAR);
    assert_eq!(result.outcome, NavPathOutcome::Partial, "{result:?}");
    assert!(result.path.is_some());
    assert!(result.unloaded_tile.is_some());
}

#[test]
fn a_box_handed_out_before_a_shift_comes_back_in_the_new_frame() {
    let (mut cache, _, east) = two_cells();
    let (lo, hi) = (Vec3::new(2.0, -1.0, -2.0), Vec3::new(4.0, 1.0, 2.0));
    cache.rebuild_region(lo, hi, &floor()).expect("rebuilds");
    cache.update();
    cache.unload_cell(1).expect("loaded");
    cache.update();
    cache.load_cell(1, &east).expect("loads");
    cache.update();
    let first = cache.take_replay_regions();
    assert!(!first.is_empty());
    assert!(cache.take_replay_regions().is_empty(), "handed out once");
    let delta = Vec3::new(1000.0, 0.0, 0.0);
    cache.shift_origin(delta);
    let again = cache.take_replay_regions();
    assert_eq!(again.len(), first.len());
    assert_eq!(again[0].0, first[0].0 + delta);
}
