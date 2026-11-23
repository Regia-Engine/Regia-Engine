// SPDX-License-Identifier: MPL-2.0
//! Region re-voxelize (ADR-1934): rebuilding a box of a [`NavTileCache`] from new geometry gives
//! the same polygons as building the whole cache from that geometry.

use regia_core::math::Vec3;
use regia_nav::{NavMeshBuildParams, NavObstacleShape, NavTileCache, NavTileCacheError, NavTileCacheParams};

fn params(tile_size: u32) -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size,
    }
}

/// Two triangles covering `[x0, x1] x [z0, z1]` at height `y`.
fn quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> Vec<[Vec3; 3]> {
    vec![
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

/// A solid block's top and four sides (tall enough to stop an agent).
fn block(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> Vec<[Vec3; 3]> {
    let mut t = quad(x0, x1, z0, z1, y);
    for (a, b) in [
        (Vec3::new(x0, 0.0, z0), Vec3::new(x1, 0.0, z0)),
        (Vec3::new(x1, 0.0, z0), Vec3::new(x1, 0.0, z1)),
        (Vec3::new(x1, 0.0, z1), Vec3::new(x0, 0.0, z1)),
        (Vec3::new(x0, 0.0, z1), Vec3::new(x0, 0.0, z0)),
    ] {
        let (at, bt) = (Vec3::new(a.x, y, a.z), Vec3::new(b.x, y, b.z));
        t.push([a, b, bt]);
        t.push([a, bt, at]);
    }
    t
}

fn floor() -> Vec<[Vec3; 3]> {
    quad(-8.0, 8.0, -8.0, 8.0, 0.0)
}

fn with_wall() -> Vec<[Vec3; 3]> {
    let mut t = floor();
    t.extend(block(-0.5, 0.5, -8.0, 8.0, 3.0));
    t
}

const FAR: Vec3 = Vec3::splat(2.0);

fn assert_same_mesh(a: &NavTileCache, b: &NavTileCache) {
    assert!(a.mesh().polys == b.mesh().polys, "polygons and portals differ");
    assert_eq!(a.mesh().region_count, b.mesh().region_count);
}

#[test]
fn removing_a_wall_matches_a_full_build_of_the_new_geometry() {
    let mut cache = NavTileCache::build(params(32), &with_wall()).expect("builds");
    let (start, end) = (Vec3::new(-5.0, 0.0, 0.0), Vec3::new(5.0, 0.0, 0.0));
    assert!(
        cache.query().find_path(start, end, FAR).is_none(),
        "the wall splits the floor"
    );

    let done = cache
        .rebuild_region(Vec3::new(-0.5, 0.0, -8.0), Vec3::new(0.5, 3.0, 8.0), &floor())
        .expect("rebuilds");
    assert!(!done.queued.is_empty());
    assert!(done.cells_changed > 0);
    cache.update();

    assert_same_mesh(
        &cache,
        &NavTileCache::build(params(32), &floor()).expect("builds"),
    );
    assert!(
        cache.query().find_path(start, end, FAR).is_some(),
        "the gap is walkable"
    );
}

#[test]
fn adding_a_wall_matches_a_full_build_of_the_new_geometry() {
    let mut cache = NavTileCache::build(params(32), &floor()).expect("builds");
    cache
        .rebuild_region(Vec3::new(-0.5, 0.0, -8.0), Vec3::new(0.5, 3.0, 8.0), &with_wall())
        .expect("rebuilds");
    cache.update();
    assert_same_mesh(
        &cache,
        &NavTileCache::build(params(32), &with_wall()).expect("builds"),
    );
}

#[test]
fn a_box_on_a_tile_corner_matches_a_full_build() {
    // The block sits on the corner of four tiles, wherever the grid's border puts that corner.
    let probe = NavTileCache::build(params(32), &floor()).expect("builds");
    let (corner, _) = probe.tile_bounds(1, 1).expect("a tile");
    let (x, z) = (corner.x, corner.z);
    let edited = {
        let mut t = floor();
        t.extend(block(x - 0.25, x + 0.25, z - 0.25, z + 0.25, 3.0));
        t
    };
    let mut cache = NavTileCache::build(params(32), &floor()).expect("builds");
    let done = cache
        .rebuild_region(
            Vec3::new(x - 0.25, 0.0, z - 0.25),
            Vec3::new(x + 0.25, 3.0, z + 0.25),
            &edited,
        )
        .expect("rebuilds");
    assert_eq!(done.queued.len(), 4, "all four tiles change: {:?}", done.queued);
    cache.update();
    assert_same_mesh(&cache, &NavTileCache::build(params(32), &edited).expect("builds"));
}

#[test]
fn a_box_at_the_grid_edge_keeps_the_border_erosion() {
    let edited = {
        let mut t = floor();
        t.extend(block(6.0, 8.0, -8.0, 8.0, 3.0));
        t
    };
    let mut cache = NavTileCache::build(params(32), &floor()).expect("builds");
    cache
        .rebuild_region(Vec3::new(6.0, 0.0, -8.0), Vec3::new(8.0, 3.0, 8.0), &edited)
        .expect("rebuilds");
    cache.update();
    assert_same_mesh(&cache, &NavTileCache::build(params(32), &edited).expect("builds"));
}

#[test]
fn raised_ground_keeps_its_new_height() {
    let raised = {
        let mut t = quad(-8.0, 8.0, -8.0, 0.0, 0.0);
        t.extend(quad(-8.0, 8.0, 0.0, 8.0, 0.2));
        t
    };
    let mut cache = NavTileCache::build(params(32), &floor()).expect("builds");
    cache
        .rebuild_region(Vec3::new(-8.0, 0.0, 0.0), Vec3::new(8.0, 0.2, 8.0), &raised)
        .expect("rebuilds");
    cache.update();
    assert_same_mesh(&cache, &NavTileCache::build(params(32), &raised).expect("builds"));
    let (_, p) = cache
        .query()
        .find_nearest(Vec3::new(4.0, 0.2, 4.0), FAR)
        .expect("on the mesh");
    assert!((p.y - 0.2).abs() < 1e-4, "height {}", p.y);
}

#[test]
fn only_the_tiles_under_the_change_are_rebuilt() {
    let edited = {
        let mut t = floor();
        t.extend(block(-6.0, -5.0, -6.0, -5.0, 3.0));
        t
    };
    let mut cache = NavTileCache::build(params(16), &floor()).expect("builds");
    let done = cache
        .rebuild_region(Vec3::new(-6.0, 0.0, -6.0), Vec3::new(-5.0, 3.0, -5.0), &edited)
        .expect("rebuilds");
    assert!(done.queued.len() < cache.tile_count() / 4, "{:?}", done.queued);
    let update = cache.update();
    assert_eq!(update.rebuilt, done.queued);
    assert_eq!(cache.total_rebuilds(), done.queued.len() as u64);
}

#[test]
fn unchanged_geometry_queues_nothing_and_keeps_the_revision() {
    let mut cache = NavTileCache::build(params(32), &with_wall()).expect("builds");
    let done = cache
        .rebuild_region(Vec3::new(-2.0, 0.0, -2.0), Vec3::new(2.0, 3.0, 2.0), &with_wall())
        .expect("rebuilds");
    assert_eq!(done.cells_changed, 0);
    assert!(done.queued.is_empty());
    assert!(cache.update().rebuilt.is_empty());
    assert_eq!(
        cache.revision(),
        0,
        "polygon indices did not change, crowds need not replan"
    );
}

#[test]
fn cells_outside_the_box_keep_their_old_state() {
    // The new geometry drops the far half too, but only the box is rebuilt.
    let half = quad(-8.0, 0.0, -8.0, 8.0, 0.0);
    let mut cache = NavTileCache::build(params(32), &floor()).expect("builds");
    cache
        .rebuild_region(Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 1.0, 1.0), &half)
        .expect("rebuilds");
    cache.update();
    let probe = Vec3::new(6.0, 0.0, 6.0);
    let (_, p) = cache
        .query()
        .find_nearest(probe, Vec3::splat(0.1))
        .expect("still walkable");
    assert!((p - probe).length() < 0.3);
}

#[test]
fn the_window_covers_the_box_grown_by_the_erosion_reach_twice() {
    let cache = NavTileCache::build(params(32), &floor()).expect("builds");
    let w = cache
        .region_window(Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 1.0, 1.0))
        .expect("inside the grid");
    let radius = cache.params().build.agent.radius;
    assert!(
        w.0.x <= -1.0 - 2.0 * radius && w.1.x >= 1.0 + 2.0 * radius,
        "{w:?}"
    );
    assert!(
        w.0.z <= -1.0 - 2.0 * radius && w.1.z >= 1.0 + 2.0 * radius,
        "{w:?}"
    );
}

#[test]
fn a_box_outside_the_grid_is_a_no_op() {
    let mut cache = NavTileCache::build(params(32), &floor()).expect("builds");
    let (lo, hi) = (Vec3::new(100.0, 0.0, 100.0), Vec3::new(101.0, 1.0, 101.0));
    assert!(cache.region_window(lo, hi).is_none());
    let done = cache.rebuild_region(lo, hi, &floor()).expect("no error");
    assert!(done.outside);
    assert!(done.queued.is_empty());
}

#[test]
fn inverted_corners_are_accepted_and_bad_input_changes_nothing() {
    let mut a = NavTileCache::build(params(32), &with_wall()).expect("builds");
    a.rebuild_region(Vec3::new(0.5, 3.0, 8.0), Vec3::new(-0.5, 0.0, -8.0), &floor())
        .expect("corners in any order");
    a.update();
    assert_same_mesh(&a, &NavTileCache::build(params(32), &floor()).expect("builds"));

    let mut b = NavTileCache::build(params(32), &with_wall()).expect("builds");
    let before = b.mesh().polys.clone();
    let err = b
        .rebuild_region(Vec3::new(f32::NAN, 0.0, 0.0), Vec3::ONE, &floor())
        .expect_err("non-finite box");
    assert!(matches!(err, NavTileCacheError::InvalidRegion(_)));
    assert_eq!(err.code(), "nav_invalid_region");
    let mut bad = floor();
    bad[0][1].y = f32::INFINITY;
    let err = b
        .rebuild_region(Vec3::new(-1.0, 0.0, -1.0), Vec3::ONE, &bad)
        .expect_err("non-finite geometry");
    assert_eq!(err.code(), "nav_invalid_region");
    assert!(b.update().rebuilt.is_empty());
    assert_eq!(b.mesh().polys, before);
}

#[test]
fn triangles_far_outside_the_window_are_ignored() {
    // A triangle wholly on the low side of the window must not wrap the rasterizer's cell range.
    let mut geometry = floor();
    geometry.extend(quad(-8.0, -7.0, -8.0, -7.0, 0.5));
    let mut cache = NavTileCache::build(params(32), &geometry).expect("builds");
    let done = cache
        .rebuild_region(Vec3::new(6.0, 0.0, 6.0), Vec3::new(7.0, 1.0, 7.0), &geometry)
        .expect("rebuilds");
    assert_eq!(done.cells_changed, 0);
}

#[test]
fn obstacles_stay_on_a_rebuilt_region() {
    let mut cache = NavTileCache::build(params(32), &with_wall()).expect("builds");
    cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(-1.0, 0.0, -1.0),
            max: Vec3::new(1.0, 2.0, 1.0),
        })
        .expect("adds");
    cache.update();
    cache
        .rebuild_region(Vec3::new(-0.5, 0.0, -8.0), Vec3::new(0.5, 3.0, 8.0), &floor())
        .expect("rebuilds");
    cache.update();
    assert!(
        cache.query().find_nearest(Vec3::ZERO, Vec3::splat(0.1)).is_none(),
        "under the obstacle"
    );

    let mut fresh = NavTileCache::build(params(32), &floor()).expect("builds");
    fresh
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(-1.0, 0.0, -1.0),
            max: Vec3::new(1.0, 2.0, 1.0),
        })
        .expect("adds");
    fresh.update();
    assert_same_mesh(&cache, &fresh);
}

#[test]
fn update_limited_spreads_a_region_over_steps_and_ends_on_the_same_mesh() {
    let mut cache = NavTileCache::build(params(16), &with_wall()).expect("builds");
    let done = cache
        .rebuild_region(Vec3::new(-0.5, 0.0, -8.0), Vec3::new(0.5, 3.0, 8.0), &floor())
        .expect("rebuilds");
    let mut steps = 0;
    while cache.update_limited(1).remaining > 0 {
        steps += 1;
        assert!(steps <= done.queued.len());
    }
    assert_same_mesh(
        &cache,
        &NavTileCache::build(params(16), &floor()).expect("builds"),
    );
}
