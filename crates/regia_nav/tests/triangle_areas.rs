// SPDX-License-Identifier: MPL-2.0
//! Per-triangle area paint at cook (ADR-2420): an area id per input triangle reaches the walkable
//! cells, the polygons and the cooked tiles; area volumes turned into triangle areas give the mesh
//! volume paint gives; painted areas change the path; a cook with nothing painted is byte for byte
//! the old one; a region rebuild keeps the paint.

use regia_core::math::Vec3;
use regia_nav::{
    NavAreaCosts, NavAreaVolume, NavMesh, NavMeshBuildParams, NavMeshBuilder, NavObstacleShape, NavPath,
    NavQuery, NavTileCache, NavTileCacheParams, paint_triangle_areas,
};

const REACH: Vec3 = Vec3::splat(2.0);
const MUD: u8 = 3;

fn params() -> NavMeshBuildParams {
    let mut p = NavMeshBuildParams {
        cell_size: 0.5,
        ..Default::default()
    };
    p.agent.radius = 0.0;
    p
}

fn cache_params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: params(),
        tile_size: 16,
    }
}

/// A 32 x 32 m floor of 2 m quads (two triangles each), row by row.
fn floor() -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    for zi in 0..16 {
        for xi in 0..16 {
            let (x0, z0) = (-16.0 + 2.0 * xi as f32, -16.0 + 2.0 * zi as f32);
            let (x1, z1) = (x0 + 2.0, z0 + 2.0);
            tris.push([
                Vec3::new(x0, 0.0, z0),
                Vec3::new(x1, 0.0, z0),
                Vec3::new(x1, 0.0, z1),
            ]);
            tris.push([
                Vec3::new(x0, 0.0, z0),
                Vec3::new(x1, 0.0, z1),
                Vec3::new(x0, 0.0, z1),
            ]);
        }
    }
    tris
}

/// A mud pit across the straight line from `(-12, 0)` to `(12, 0)`, on quad borders.
fn pit() -> NavAreaVolume {
    NavAreaVolume::new(
        NavObstacleShape::Box {
            min: Vec3::new(-6.0, -1.0, -8.0),
            max: Vec3::new(6.0, 1.0, 8.0),
        },
        MUD,
    )
}

fn pit_areas() -> Vec<u8> {
    paint_triangle_areas(&floor(), &[], &[pit()], params().agent.max_climb)
}

fn area_at(mesh: &NavMesh, x: f32, z: f32) -> u8 {
    let q = NavQuery {
        mesh,
        obstacles: None,
    };
    let (p, _) = q.find_nearest(Vec3::new(x, 0.0, z), REACH).expect("on the mesh");
    mesh.polys[p as usize].area
}

fn plan(mesh: &NavMesh) -> NavPath {
    NavQuery {
        mesh,
        obstacles: None,
    }
    .find_path(Vec3::new(-12.0, 0.0, 0.0), Vec3::new(12.0, 0.0, 0.0), REACH)
    .expect("a path")
}

fn crosses(mesh: &NavMesh, path: &NavPath, area: u8) -> bool {
    path.polys.iter().any(|&p| mesh.polys[p as usize].area == area)
}

#[test]
fn volumes_become_triangle_areas() {
    let areas = pit_areas();
    assert_eq!(areas.len(), floor().len());
    // 6 x 8 quads inside the pit, two triangles each.
    assert_eq!(areas.iter().filter(|&&a| a == MUD).count(), 2 * 6 * 8);
    assert!(areas.iter().all(|&a| a == 0 || a == MUD));
    // A base list is kept where no volume paints; a later volume wins.
    let base = vec![7; floor().len()];
    let over = NavAreaVolume::new(
        NavObstacleShape::Box {
            min: Vec3::new(-2.0, -1.0, -2.0),
            max: Vec3::new(2.0, 1.0, 2.0),
        },
        9,
    );
    let painted = paint_triangle_areas(&floor(), &base, &[pit(), over], 0.4);
    assert_eq!(painted.iter().filter(|&&a| a == 9).count(), 2 * 2 * 2);
    assert_eq!(painted.iter().filter(|&&a| a == MUD).count(), 2 * 6 * 8 - 8);
    assert_eq!(
        painted.iter().filter(|&&a| a == 7).count(),
        floor().len() - 2 * 6 * 8
    );
}

#[test]
fn triangle_paint_gives_the_mesh_volume_paint_gives() {
    let builder = NavMeshBuilder::new(params());
    let by_triangle = builder
        .build_with_triangle_areas(&floor(), &pit_areas(), &[])
        .expect("builds");
    let by_volume = builder.build_with_areas(&floor(), &[pit()]).expect("builds");
    assert!(by_triangle.poly_count() > 1, "the pit has its own polygons");
    assert_eq!(by_triangle.poly_count(), by_volume.poly_count());
    for (a, b) in by_triangle.polys.iter().zip(&by_volume.polys) {
        assert_eq!(a.area, b.area);
    }
    assert_eq!(area_at(&by_triangle, 0.0, 0.0), MUD);
    assert_eq!(area_at(&by_triangle, 5.75, 7.75), MUD);
    assert_eq!(area_at(&by_triangle, 6.25, 0.0), 0);
    assert_eq!(area_at(&by_triangle, 0.0, 8.25), 0);
    // Nothing painted is the plain build.
    let plain = builder.build(&floor()).expect("builds");
    let zeros = builder
        .build_with_triangle_areas(&floor(), &vec![0; floor().len()], &[])
        .expect("builds");
    assert_eq!(plain.poly_count(), zeros.poly_count());
    assert!(zeros.polys.iter().all(|p| p.area == 0));
}

#[test]
fn painted_triangles_change_the_path_cost() {
    let mut mesh = NavMeshBuilder::new(params())
        .build_with_triangle_areas(&floor(), &pit_areas(), &[])
        .expect("builds");
    let straight = plan(&mesh);
    assert!(crosses(&mesh, &straight, MUD), "at cost 1 the pit is on the way");
    let mut costs = NavAreaCosts::default();
    costs.set(MUD, 10.0).expect("cost");
    mesh.set_area_costs(costs);
    let around = plan(&mesh);
    assert!(!crosses(&mesh, &around, MUD), "at cost 10 the path goes round");
    let widest = around.waypoints.iter().map(|w| w.z.abs()).fold(0.0f32, f32::max);
    assert!(widest >= 8.0, "it leaves the pit's strip: {:?}", around.waypoints);
}

#[test]
fn an_unpainted_cook_is_byte_identical() {
    let plain = NavTileCache::build(cache_params(), &floor()).expect("builds");
    let empty = NavTileCache::build_with_triangle_areas(cache_params(), &floor(), &[]).expect("builds");
    let zeros = NavTileCache::build_with_triangle_areas(cache_params(), &floor(), &vec![0; floor().len()])
        .expect("builds");
    let tiles = plain.tiles_in(Vec3::splat(-16.0), Vec3::splat(16.0));
    assert!(tiles.len() > 1);
    let blob = plain.cook_cell(&tiles).expect("cooks");
    assert_eq!(empty.cook_cell(&tiles).expect("cooks"), blob);
    assert_eq!(zeros.cook_cell(&tiles).expect("cooks"), blob);
    // A painted cook differs, and its tiles come back painted with no volume registered.
    let painted =
        NavTileCache::build_with_triangle_areas(cache_params(), &floor(), &pit_areas()).expect("builds");
    let painted_blob = painted.cook_cell(&tiles).expect("cooks");
    assert_ne!(painted_blob, blob);
    assert_eq!(painted_blob.len(), blob.len(), "same layout, other area bytes");
    let mut streamed = NavTileCache::streamed(cache_params(), painted.layout()).expect("streamed");
    streamed.load_cell(1, &painted_blob).expect("loads");
    streamed.update();
    assert_eq!(streamed.area_volumes().count(), 0);
    assert_eq!(area_at(streamed.mesh(), 0.0, 0.0), MUD);
    assert_eq!(area_at(streamed.mesh(), 10.0, 0.0), 0);
}

#[test]
fn a_region_rebuild_keeps_the_triangle_paint() {
    let mut cache =
        NavTileCache::build_with_triangle_areas(cache_params(), &floor(), &pit_areas()).expect("builds");
    assert_eq!(area_at(cache.mesh(), 0.0, 0.0), MUD);
    // A box dropped in the pit and taken away again: heights change twice, the paint stays.
    let mut with_box = floor();
    let b = [
        [
            Vec3::new(-1.0, 2.0, -1.0),
            Vec3::new(1.0, 2.0, -1.0),
            Vec3::new(1.0, 2.0, 1.0),
        ],
        [
            Vec3::new(-1.0, 2.0, -1.0),
            Vec3::new(1.0, 2.0, 1.0),
            Vec3::new(-1.0, 2.0, 1.0),
        ],
    ];
    with_box.extend(b);
    let (lo, hi) = (Vec3::new(-2.0, -1.0, -2.0), Vec3::new(2.0, 3.0, 2.0));
    cache.rebuild_region(lo, hi, &with_box).expect("rebuilds");
    cache.update();
    cache.rebuild_region(lo, hi, &floor()).expect("rebuilds");
    cache.update();
    assert_eq!(area_at(cache.mesh(), 0.0, 0.0), MUD);
    assert_eq!(area_at(cache.mesh(), 10.0, 0.0), 0);
}

#[test]
fn bad_triangle_area_lists_are_typed_errors() {
    let short = NavTileCache::build_with_triangle_areas(cache_params(), &floor(), &[1, 2]);
    assert_eq!(short.err().map(|e| e.code()), Some("nav_invalid_build_input"));
    let mut past = vec![0; floor().len()];
    past[3] = 64;
    let past = NavMeshBuilder::new(params()).build_with_triangle_areas(&floor(), &past, &[]);
    assert!(past.is_err());
}
