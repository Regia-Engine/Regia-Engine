// SPDX-License-Identifier: MPL-2.0
//! Conservative rasterisation (nav cell v3, ADR-8899): a cell takes every triangle whose XZ
//! projection overlaps its square, so a wall top or a plank thinner than `cell_size` is seen
//! wherever it falls on the grid. Before v3 the heightfield sampled cell centres only, and such
//! geometry could fall between two centres and vanish (ADR-7558).

use regia_core::math::Vec3;
use regia_nav::tile_cache::nav_cell_migration_sources;
use regia_nav::{
    NAV_CELL_FORMAT_VERSION, NavAgentParams, NavMeshBuildParams, NavMeshBuilder, NavQuery, NavTileCache,
    NavTileCacheParams,
};

/// Two triangles covering `[x0, x1] x [z0, z1]` at height `y`.
fn quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> Vec<[Vec3; 3]> {
    vec![
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

/// A solid block's top and its four (vertical) sides, from the floor up to `y`.
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

/// The wall thickness: well under the default 0.25 m cell.
const THIN: f32 = 0.1;

/// Offsets of the thin geometry inside one cell (0.25 m), so at least one falls between two cell
/// centres and at least one covers a centre.
fn offsets() -> impl Iterator<Item = f32> {
    (0..10).map(|i| 4.0 + i as f32 * 0.025)
}

/// An 8 m x 4 m floor cut in two by a 2 m high, `THIN` thick wall from `x0`, wall to wall in z.
fn floor_with_thin_wall(x0: f32) -> Vec<[Vec3; 3]> {
    let mut t = quad(0.0, 8.0, 0.0, 4.0, 0.0);
    t.extend(block(x0, x0 + THIN, 0.0, 4.0, 2.0));
    t
}

#[test]
fn a_thin_wall_blocks_wherever_it_falls_on_the_grid() {
    let builder = NavMeshBuilder::default();
    let (from, to) = (Vec3::new(1.0, 0.0, 2.0), Vec3::new(7.0, 0.0, 2.0));
    let mut leaks = Vec::new();
    for x0 in offsets() {
        let mesh = builder.build(&floor_with_thin_wall(x0)).expect("builds");
        let query = NavQuery {
            mesh: &mesh,
            obstacles: None,
        };
        // Both sides are on the mesh.
        assert!(
            query.find_nearest(from, Vec3::splat(0.5)).is_some(),
            "x0 {x0}: start side"
        );
        assert!(
            query.find_nearest(to, Vec3::splat(0.5)).is_some(),
            "x0 {x0}: end side"
        );
        let reached = query
            .find_path(from, to, Vec3::splat(0.5))
            .is_some_and(|p| p.waypoints.last().is_some_and(|w| (*w - to).length() < 0.5));
        if reached {
            leaks.push(x0);
        }
    }
    assert!(leaks.is_empty(), "the wall leaks at offsets {leaks:?}");
}

#[test]
fn a_sub_cell_plank_is_walkable_wherever_it_falls_on_the_grid() {
    // A radius-0 agent (no erosion), so a one-cell strip survives. The plank is 1 m up, 4 m long
    // in z and `THIN` wide in x, over nothing.
    let builder = NavMeshBuilder::new(NavMeshBuildParams {
        agent: NavAgentParams {
            radius: 0.0,
            ..NavAgentParams::default()
        },
        ..NavMeshBuildParams::default()
    });
    let mut missing = Vec::new();
    for x0 in offsets() {
        let plank = quad(x0, x0 + THIN, 0.0, 4.0, 1.0);
        let probe = Vec3::new(x0 + THIN * 0.5, 1.0, 2.0);
        let found = builder.build(&plank).ok().is_some_and(|mesh| {
            let query = NavQuery {
                mesh: &mesh,
                obstacles: None,
            };
            query
                .find_nearest(probe, Vec3::new(0.2, 0.3, 0.2))
                .is_some_and(|(_, at)| (at.y - 1.0).abs() < 1e-3)
        });
        if !found {
            missing.push(x0);
        }
    }
    assert!(
        missing.is_empty(),
        "the plank is not on the mesh at offsets {missing:?}"
    );
}

fn cache_params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams::default(),
        tile_size: 16,
    }
}

#[test]
fn a_region_rebuild_over_a_thin_wall_equals_a_full_build() {
    for x0 in offsets() {
        let mut cache = NavTileCache::build(cache_params(), &quad(0.0, 8.0, 0.0, 4.0, 0.0)).expect("builds");
        let geometry = floor_with_thin_wall(x0);
        cache
            .rebuild_region(
                Vec3::new(x0 - 0.5, -1.0, -0.5),
                Vec3::new(x0 + 0.6, 3.0, 4.5),
                &geometry,
            )
            .expect("rebuilds");
        cache.update();
        let whole = NavTileCache::build(cache_params(), &geometry).expect("builds");
        assert!(
            cache.mesh().polys == whole.mesh().polys,
            "x0 {x0}: polygons differ"
        );
        assert_eq!(cache.mesh().region_count, whole.mesh().region_count, "x0 {x0}");
    }
}

/// FNV-1a 64 over `bytes` (the nav cell checksum, `docs/formats/nav-cell.md`).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `blob` with its version set to `version` and the checksum written again.
fn with_version(mut blob: Vec<u8>, version: u32) -> Vec<u8> {
    blob[4..8].copy_from_slice(&version.to_le_bytes());
    let body = blob.len() - 8;
    let sum = fnv1a(&blob[..body]);
    blob[body..].copy_from_slice(&sum.to_le_bytes());
    blob
}

#[test]
fn the_cook_writes_v3_and_a_v2_cell_still_loads() {
    assert_eq!(NAV_CELL_FORMAT_VERSION, 3);
    assert_eq!(nav_cell_migration_sources(), vec![1, 2]);
    let whole = NavTileCache::build(cache_params(), &floor_with_thin_wall(4.13)).expect("builds");
    let (tx, tz) = whole.tile_grid();
    let tiles: Vec<(u32, u32)> = (0..tz).flat_map(|z| (0..tx).map(move |x| (x, z))).collect();
    let blob = whole.cook_cell(&tiles).expect("cooks");
    // The version moved, so every cooked blob differs from a v2 cook: the cook rewrites it.
    assert_eq!(&blob[4..8], &3u32.to_le_bytes());

    let mut v3 = NavTileCache::streamed(cache_params(), whole.layout()).expect("streamed");
    v3.load_cell(0, &blob).expect("v3 loads");
    v3.update();
    assert!(
        v3.mesh().polys == whole.mesh().polys,
        "a v3 cell rebuilds the cooked mesh"
    );

    // A v2 cell (same layout, centre-sampled heights when a v2 build cooked it) still loads as it is.
    let mut v2 = NavTileCache::streamed(cache_params(), whole.layout()).expect("streamed");
    v2.load_cell(0, &with_version(blob.clone(), 2)).expect("v2 loads");
    v2.update();
    assert!(
        v2.mesh().polys == whole.mesh().polys,
        "a v2 cell's heights are read unchanged"
    );

    // A newer cell is refused.
    let mut newer = NavTileCache::streamed(cache_params(), whole.layout()).expect("streamed");
    let e = newer
        .load_cell(0, &with_version(blob, NAV_CELL_FORMAT_VERSION + 1))
        .expect_err("newer");
    assert_eq!(e.code(), "nav_cell_version");
}
