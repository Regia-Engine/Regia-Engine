// SPDX-License-Identifier: MPL-2.0
//! Tile-grid growth (ADR-2589): a cache built whole grows by whole tiles when geometry is added
//! past its edge; the existing tiles keep their ids (toward +X/+Z), their bounds, polygons and
//! rebuild counts; toward -X/-Z they move by the reported offset. Deterministic inputs only.

use regia_core::math::Vec3;
use regia_nav::{
    NavAgentParams, NavMeshBuildParams, NavObstacleShape, NavTileCache, NavTileCacheParams, NavTileLayout,
};

/// 4 m tiles (16 cells of 0.25 m).
fn params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 16,
    }
}

fn rect(x0: f32, z0: f32, x1: f32, z1: f32) -> Vec<[Vec3; 3]> {
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

fn extents() -> Vec3 {
    Vec3::new(0.5, 1.0, 0.5)
}

/// Per tile: `(tile, bounds, polygons, rebuilds)`.
type TileRow = ((u32, u32), (Vec3, Vec3), usize, u64);

fn snapshot(cache: &NavTileCache) -> Vec<TileRow> {
    let (tx, tz) = cache.tile_grid();
    let mut out = Vec::new();
    for z in 0..tz {
        for x in 0..tx {
            out.push((
                (x, z),
                cache.tile_bounds(x, z).expect("in range"),
                cache.tile_poly_count(x, z).expect("in range"),
                cache.tile_rebuild_count(x, z).expect("in range"),
            ));
        }
    }
    out
}

#[test]
fn growth_east_keeps_every_tile_id_and_new_ground_gets_navmesh() {
    let floor = rect(0.0, 0.0, 10.0, 8.0);
    let mut cache = NavTileCache::build(params(), &floor).expect("builds");
    let before = snapshot(&cache);
    let (tiles_x, tiles_z) = cache.tile_grid();
    let start = Vec3::new(1.0, 0.0, 4.0);
    let far = Vec3::new(18.0, 0.0, 4.0);
    assert!(cache.tile_at(far).is_none(), "past the edge");
    assert!(cache.query().find_path(start, far, extents()).is_none());
    let revision = cache.revision();
    let growth = cache
        .grow_to(Vec3::new(10.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 8.0))
        .expect("grows");
    assert!(!growth.unchanged);
    assert_eq!(growth.tile_offset, (0, 0));
    assert_eq!(growth.tiles_before, (tiles_x, tiles_z));
    assert!(
        growth.tiles_after.0 > tiles_x && growth.tiles_after.1 == tiles_z,
        "{growth:?}"
    );
    assert_eq!(growth.added[0], 0, "nothing on -X");
    assert!(cache.revision() > revision);
    // Every old tile keeps its id, bounds and polygons, except a partial last column the growth
    // completed (re-polygonised, same polygons over its old cells).
    let after = snapshot(&cache);
    for (tile, bounds, polys, rebuilds) in &before {
        let now = after.iter().find(|r| r.0 == *tile).expect("the id still exists");
        if growth.rebuilt.contains(tile) {
            assert_eq!(
                tile.0,
                tiles_x - 1,
                "only the old last column is completed: {tile:?}"
            );
            assert_eq!(now.1.0, bounds.0, "same min corner");
        } else {
            assert_eq!(
                (now.1, now.2, now.3),
                (*bounds, *polys, *rebuilds),
                "tile {tile:?} untouched"
            );
        }
    }
    for tile in &growth.rebuilt {
        assert!(tile.0 >= tiles_x - 1, "new or completed: {tile:?}");
    }
    assert!(cache.tile_at(far).is_some(), "the grid covers the box");
    // The new cells are empty until geometry is voxelised there.
    assert!(cache.query().find_path(start, far, extents()).is_none());
    let mut level = floor.clone();
    level.extend(rect(10.0, 0.0, 20.0, 8.0));
    let rebuild = cache
        .rebuild_region(Vec3::new(10.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 8.0), &level)
        .expect("rebuilds");
    assert!(!rebuild.outside && rebuild.cells_changed > 0);
    cache.update();
    let path = cache
        .query()
        .find_path(start, far, extents())
        .expect("a path onto the new ground");
    assert!((path.waypoints.last().expect("waypoints").x - far.x).abs() < 0.3);
    // A box the grid already covers changes nothing.
    let revision = cache.revision();
    let same = cache
        .grow_to(Vec3::new(1.0, 0.0, 1.0), Vec3::new(15.0, 0.0, 7.0))
        .expect("no-op");
    assert!(same.unchanged && same.rebuilt.is_empty());
    assert_eq!(cache.revision(), revision);
}

#[test]
fn growth_west_moves_old_tiles_by_the_offset_and_keeps_their_polygons() {
    let floor = rect(0.0, 0.0, 8.0, 8.0);
    let mut cache = NavTileCache::build(params(), &floor).expect("builds");
    let blocker = cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(3.0, -1.0, 3.0),
            max: Vec3::new(4.0, 2.0, 4.0),
        })
        .expect("obstacle");
    cache.update();
    // After the obstacle's rebuild: the growth must keep these.
    let before = snapshot(&cache);
    let touched = cache.touched_tiles(blocker).expect("known");
    let inside = Vec3::new(2.0, 0.0, 2.0);
    let old_tile = cache.tile_at(inside).expect("on grid");
    // The grid starts below the floor's corner (its border padding), so the whole tiles needed
    // to reach the new box come from the grid's own origin and the 4 m tile width.
    let origin = cache.tile_bounds(0, 0).expect("tile 0").0;
    let tiles_to = |edge: f32, from: f32| ((from - edge).max(0.0) / 4.0).ceil() as u32;
    let (want_x, want_z) = (tiles_to(-9.0, origin.x), tiles_to(-6.0, origin.z));
    assert!(
        want_x >= 1 && want_z >= 1,
        "the box reaches past -X and -Z: {origin:?}"
    );
    let (growth, rebuild) = cache
        .rebuild_region_growing(
            Vec3::new(-9.0, 0.0, -6.0),
            Vec3::new(0.0, 0.0, 8.0),
            &[floor.clone(), rect(-9.0, -6.0, 0.0, 8.0)].concat(),
        )
        .expect("grows west and north");
    let (ox, oz) = growth.tile_offset;
    assert_eq!((ox, oz), (want_x, want_z), "{growth:?}");
    assert_eq!(growth.added[0], ox);
    assert_eq!(growth.added[2], oz);
    assert!(rebuild.cells_changed > 0);
    // The same world point lies on the same tile, numbered by the offset.
    assert_eq!(cache.tile_at(inside), Some((old_tile.0 + ox, old_tile.1 + oz)));
    let after = snapshot(&cache);
    for (tile, bounds, polys, _) in &before {
        let moved = (tile.0 + ox, tile.1 + oz);
        let now = after.iter().find(|r| r.0 == moved).expect("moved id exists");
        assert_eq!(now.1, *bounds, "tile {tile:?} stays where it was in the world");
        if !growth.rebuilt.contains(&moved) && !rebuild.queued.contains(&moved) {
            assert_eq!(
                now.2, *polys,
                "tile {tile:?} -> {moved:?}: {growth:?} {rebuild:?}"
            );
        }
    }
    // The obstacle's tiles moved with the numbering, and removing it still restores the floor.
    let moved: Vec<(u32, u32)> = touched.iter().map(|t| (t.0 + ox, t.1 + oz)).collect();
    assert_eq!(cache.touched_tiles(blocker).expect("known"), moved);
    cache.update();
    let through = (Vec3::new(3.5, 0.0, 1.0), Vec3::new(3.5, 0.0, 7.0));
    let around = cache
        .query()
        .find_path(through.0, through.1, extents())
        .expect("around the box");
    cache.remove_obstacle(blocker).expect("known");
    cache.update();
    let straight = cache
        .query()
        .find_path(through.0, through.1, extents())
        .expect("straight");
    assert!(
        straight.length < around.length - 0.1,
        "{} vs {}",
        straight.length,
        around.length
    );
    // The new ground joined the old.
    let west = cache
        .query()
        .find_path(Vec3::new(-8.0, 0.0, 4.0), Vec3::new(7.0, 0.0, 4.0), extents())
        .expect("west to east");
    assert!(west.length > 14.0);
}

#[test]
fn growth_refuses_streamed_caches_bad_boxes_and_huge_grids() {
    let floor = rect(0.0, 0.0, 8.0, 8.0);
    let whole = NavTileCache::build(params(), &floor).expect("builds");
    let mut streamed = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    let e = streamed
        .grow_to(Vec3::new(-50.0, 0.0, 0.0), Vec3::ZERO)
        .expect_err("a cooked layout");
    assert_eq!(e.code(), "nav_grid_growth");
    let layout: NavTileLayout = streamed.layout();
    assert_eq!(layout, whole.layout(), "nothing changed");
    let mut cache = whole.clone();
    for (lo, hi) in [
        (Vec3::new(f32::NAN, 0.0, 0.0), Vec3::ZERO),
        (Vec3::ZERO, Vec3::new(1e9, 0.0, 1e9)),
        (Vec3::new(-1e12, 0.0, 0.0), Vec3::ZERO),
    ] {
        let e = cache.grow_to(lo, hi).expect_err("refused");
        assert_eq!(e.code(), "nav_grid_growth");
        assert_eq!(cache.tile_grid(), whole.tile_grid(), "nothing changed");
        assert_eq!(cache.revision(), whole.revision());
    }
    let e = cache
        .rebuild_region_growing(Vec3::ZERO, Vec3::ONE, &[[Vec3::NAN, Vec3::ZERO, Vec3::ONE]])
        .expect_err("bad geometry");
    assert_eq!(e.code(), "nav_invalid_region");
    assert_eq!(cache.tile_grid(), whole.tile_grid());
}

#[test]
fn growth_is_deterministic() {
    let run = || {
        let floor = rect(0.0, 0.0, 8.0, 8.0);
        let mut cache = NavTileCache::build(params(), &floor).expect("builds");
        let level = [floor.clone(), rect(8.0, -6.0, 16.0, 8.0)].concat();
        cache
            .rebuild_region_growing(Vec3::new(8.0, 0.0, -6.0), Vec3::new(16.0, 0.0, 8.0), &level)
            .expect("grows");
        cache.update();
        let mesh = cache.mesh();
        let polys: Vec<_> = mesh
            .polys
            .iter()
            .map(|p| (p.min.to_array(), p.max.to_array()))
            .collect();
        (cache.tile_grid(), polys)
    };
    assert_eq!(run(), run());
}

/// `cell_size` 1, no border, agent radius 0, tile width `tile_size`. The floor is x in `[0, 2]`,
/// z in `[0, 1]`: the builder's one-cell pad makes a 3 by 2 grid. A `[0, 3]²` floor is 4 by 4,
/// and `4 * (16_777_221 + 4)` is past 64 Mi cells, so that setup never reaches the f32 cast.
fn coarse_floor(tile_size: u32) -> (NavTileCacheParams, Vec<[Vec3; 3]>) {
    let params = NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 1.0,
            border_size: 0.0,
            agent: NavAgentParams {
                radius: 0.0,
                ..NavAgentParams::default()
            },
            ..NavMeshBuildParams::default()
        },
        tile_size,
    };
    (params, rect(0.0, 0.0, 2.0, 1.0))
}

/// A west grow whose cell shift is not an f32 must be refused, and the old floor must still
/// lose its polygon to an obstacle.
fn assert_inexact_west_grow_refused(tile_size: u32, west_edge: f32) {
    let (params, floor) = coarse_floor(tile_size);
    let mut cache = NavTileCache::build(params, &floor).expect("builds");
    let on_floor = Vec3::new(0.5, 0.0, 0.5);
    let old_tile = cache.tile_at(on_floor).expect("the floor is on the grid");
    let polys = cache.tile_poly_count(old_tile.0, old_tile.1).expect("tile");
    assert!(polys > 0, "erosion kept the floor");
    let revision = cache.revision();
    let grid = cache.tile_grid();
    let err = cache
        .grow_to(Vec3::new(west_edge, 0.0, 0.25), Vec3::new(1.0, 0.0, 0.5))
        .expect_err("the west cell shift is not an f32");
    assert_eq!(err.code(), "nav_grid_growth");
    assert_eq!(cache.tile_at(on_floor), Some(old_tile), "the cache did not move");
    assert_eq!(cache.tile_grid(), grid);
    assert_eq!(cache.revision(), revision);
    assert_eq!(cache.tile_poly_count(old_tile.0, old_tile.1), Some(polys));
    let before = cache
        .query()
        .find_nearest(on_floor, Vec3::new(0.2, 2.0, 0.2))
        .expect("the point is on the mesh");
    cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(0.25, -1.0, 0.25),
            max: Vec3::new(0.75, 2.0, 0.75),
        })
        .expect("obstacle");
    cache.update();
    assert!(
        cache
            .query()
            .find_nearest(on_floor, Vec3::new(0.2, 2.0, 0.2))
            .is_none(),
        "the obstacle removes the polygon under the point; nearest was {before:?}"
    );
}

#[test]
fn growth_west_refuses_a_cell_shift_f32_cannot_represent() {
    // `-16_781_312` is an f32. `ceil(16_781_312 / 4095) = 4099` tiles and `dx = 16_785_405`
    // cells, which rounds to `16_785_404`. Tile size 4095 is the same hole as the audit's
    // tile size 3, with a few thousand tiles instead of 5_592_407.
    assert_ne!(16_785_405usize as f32 as usize, 16_785_405);
    assert_inexact_west_grow_refused(4095, -16_781_312.0);
}

#[test]
fn growth_west_refuses_the_audit_sixteen_million_cell_shift() {
    // Audit numbers: min x `-16_777_220` (an f32), tile size 3,
    // `west = ceil(16_777_220 / 3) = 5_592_407`, `dx = 16_777_221`, which is not an f32.
    // `16_777_221 as f32` is `16_777_220` (ulp 2 above 2^24, ties round to even).
    assert_ne!(16_777_221usize as f32 as usize, 16_777_221);
    assert_inexact_west_grow_refused(3, -16_777_220.0);
}
