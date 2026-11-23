// SPDX-License-Identifier: MPL-2.0
//! Terrain collision parity (ADR-873): a heightfield collider built with
//! `TerrainMap::collision_heightfield` is the drawn terrain. Rays cast through the real
//! physics world land on `CollisionHeightfield::height_at` (the CPU mirror of the collider's
//! triangles), which equals the finest drawn grid (`terrain_surface_height`) and stays within
//! the twist bound of the simulation's bilinear sampler. Copying the map's row-major heights
//! straight into the collider (the mistake the helper exists to prevent) puts the player on a
//! mirrored hill.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Rng, Vec3};
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsQuery, QueryFilter, RigidBody};
use regia_scene::Transform;
use regia_terrain::{CollisionHeightfield, HoleShape, HolesMask, TerrainMap, terrain_surface_height};

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

/// `w × h` samples over 96 m, rising 40 m toward +Z and 3 m toward +X, with bumps: far from
/// symmetric about the X = Z diagonal.
fn tilted(w: u32, h: u32) -> TerrainMap {
    let mut heights = Vec::with_capacity((w * h) as usize);
    for row in 0..h {
        for col in 0..w {
            let x = col as f32 / (w - 1) as f32;
            let z = row as f32 / (h - 1) as f32;
            heights.push(3.0 * x + 40.0 * z + 2.5 * (x * 11.0).sin() * (z * 6.0).cos());
        }
    }
    TerrainMap::from_heights(w, h, heights, 96.0, 100.0, -10.0).expect("map")
}

fn shape(c: &CollisionHeightfield) -> ColliderShape {
    ColliderShape::HeightField {
        nx: c.nx,
        nz: c.nz,
        heights: c.heights.clone(),
        scale: Vec3::from(c.scale),
    }
}

/// Casts a ray straight down at every `(x, z)` through the physics world holding `collider`,
/// returning the hit heights (`None` for a miss).
fn cast_down(collider: ColliderShape, points: Vec<[f32; 2]>) -> Vec<Option<f32>> {
    cast_down_at(collider, [0.0; 3], points)
}

/// [`cast_down`] with the collider placed at `center` (a `CollisionHeightfield::center`).
fn cast_down_at(collider: ColliderShape, center: [f32; 3], points: Vec<[f32; 2]>) -> Vec<Option<f32>> {
    let slot: Arc<Mutex<Option<Vec<Option<f32>>>>> = Arc::new(Mutex::new(None));
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform {
            translation: Vec3::from(center),
            ..Transform::IDENTITY
        },
        RigidBody::static_body(),
        Collider {
            shape: collider,
            friction: 0.8,
            ..Collider::default()
        },
    ));
    let slot2 = slot.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let hits = points
            .iter()
            .map(|&[x, z]| {
                queries
                    .raycast(Vec3::new(x, 200.0, z), Vec3::NEG_Y, 400.0, QueryFilter::new())
                    .map(|hit| hit.point.y)
            })
            .collect();
        *slot2.lock().expect("slot") = Some(hits);
    });
    app.update();
    app.update();
    let hits = slot.lock().expect("slot").take();
    hits.expect("the query system ran")
}

/// Every hit of a ray straight down at each point (`PhysicsQuery::raycast_all`), heights only.
fn cast_down_all_at(collider: ColliderShape, center: [f32; 3], points: Vec<[f32; 2]>) -> Vec<Vec<f32>> {
    let slot: Arc<Mutex<Option<Vec<Vec<f32>>>>> = Arc::new(Mutex::new(None));
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform {
            translation: Vec3::from(center),
            ..Transform::IDENTITY
        },
        RigidBody::static_body(),
        Collider {
            shape: collider,
            friction: 0.8,
            ..Collider::default()
        },
    ));
    let slot2 = slot.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let hits = points
            .iter()
            .map(|&[x, z]| {
                queries
                    .raycast_all(Vec3::new(x, 200.0, z), Vec3::NEG_Y, 400.0, QueryFilter::new())
                    .iter()
                    .map(|hit| hit.point.y)
                    .collect()
            })
            .collect();
        *slot2.lock().expect("slot") = Some(hits);
    });
    app.update();
    app.update();
    let hits = slot.lock().expect("slot").take();
    hits.expect("the query system ran")
}

fn random_points(map: &TerrainMap, count: usize, seed: u64) -> Vec<[f32; 2]> {
    let half = map.world_width() * 0.5 - 0.01;
    let mut rng = Rng::new(seed);
    (0..count)
        .map(|_| [rng.range(-half..half), rng.range(-half..half)])
        .collect()
}

#[test]
fn collider_rays_land_on_the_drawn_surface() {
    let map = tilted(97, 97);
    let c = map.collision_heightfield(None);
    let points = random_points(&map, 600, 11);
    let hits = cast_down(shape(&c), points.clone());
    let bound = c.max_bilinear_delta();
    let mut worst_drawn = 0.0f32;
    let mut worst_sim = 0.0f32;
    for (&[x, z], hit) in points.iter().zip(&hits) {
        let y = hit.unwrap_or_else(|| panic!("ray at ({x}, {z}) missed the terrain"));
        let mirror = c.height_at(x, z).expect("inside");
        assert!(
            (y - mirror).abs() < 2e-3,
            "({x}, {z}): physics {y} vs collider mirror {mirror}"
        );
        let drawn = terrain_surface_height(&map, map.sample_spacing(), x, z);
        worst_drawn = worst_drawn.max((y - drawn).abs());
        worst_sim = worst_sim.max((y - map.sample_bilinear(x, z)).abs());
    }
    eprintln!("collider vs drawn {worst_drawn:.5} m, vs bilinear {worst_sim:.5} m (bound {bound:.5} m)");
    assert!(worst_drawn < 2e-3, "collider vs finest drawn grid: {worst_drawn}");
    assert!(
        worst_sim <= bound + 2e-3,
        "collider vs bilinear sampler: {worst_sim} > {bound}"
    );
}

/// A map matched to a cell-spaced simulation grid (ADR-8179: 64 samples over 320 m, grid
/// origin -2.5 m): the physics collider placed at `CollisionHeightfield::center` is the drawn
/// surface, and a ray down at a simulation sample lands on that sample's height.
#[test]
fn a_collider_at_the_grid_origin_is_the_drawn_surface() {
    let (n, sim) = (64u32, 320.0f32);
    let heights: Vec<f32> = (0..n)
        .flat_map(|r| (0..n).map(move |c| ((c * 7 + r * 13) % 23) as f32 * 0.5))
        .collect();
    let map = TerrainMap::from_heights(n, n, heights, sim * (n - 1) as f32 / n as f32, 100.0, -10.0)
        .and_then(|m| m.with_grid_origin([-0.5 * sim / n as f32; 2]))
        .expect("map");
    let c = map.collision_heightfield(None);
    assert_eq!(c.center, [-2.5, 0.0, -2.5]);
    let sim_x = |i: u32| (i as f32 - (n / 2) as f32) * (sim / n as f32);
    let samples = [(0u32, 0u32), (32, 32), (17, 40), (63, 63), (5, 60), (40, 3)];
    let mut points: Vec<[f32; 2]> = samples.iter().map(|&(i, k)| [sim_x(i), sim_x(k)]).collect();
    let mut rng = Rng::new(8179);
    points.extend((0..300).map(|_| [rng.range(-159.9..154.9), rng.range(-159.9..154.9)]));
    let hits = cast_down_at(shape(&c), c.center, points.clone());
    for (k, (&[x, z], hit)) in points.iter().zip(&hits).enumerate() {
        let y = hit.unwrap_or_else(|| panic!("ray at ({x}, {z}) missed the terrain"));
        let drawn = terrain_surface_height(&map, map.sample_spacing(), x, z);
        assert!(
            (y - drawn).abs() < 2e-3,
            "({x}, {z}): physics {y} vs drawn {drawn}"
        );
        if let Some(&(i, j)) = samples.get(k) {
            let stored = map.height_at_sample(i64::from(i), i64::from(j));
            assert!(
                (y - stored).abs() < 2e-3,
                "sample ({i}, {j}): physics {y} vs stored {stored}"
            );
        }
    }
    // The collider left at the world origin (the base's placement) is half a sample off.
    let wrong = cast_down(shape(&c), points[..samples.len()].to_vec());
    let off = points[..samples.len()]
        .iter()
        .zip(&wrong)
        .filter(|(point, hit)| {
            let [x, z] = **point;
            hit.is_none_or(|y| (y - terrain_surface_height(&map, 5.0, x, z)).abs() > 2e-3)
        })
        .count();
    assert!(
        off > 0,
        "an unmoved collider must miss the drawn heights somewhere"
    );
}

#[test]
fn a_non_square_map_keeps_its_rows_along_z() {
    // 97 columns, 49 rows: the Z spacing is twice the X spacing.
    let map = tilted(97, 49);
    let c = map.collision_heightfield(None);
    assert_eq!((c.nx, c.nz), (49, 97));
    let points = random_points(&map, 300, 4);
    let hits = cast_down(shape(&c), points.clone());
    for (&[x, z], hit) in points.iter().zip(&hits) {
        let y = hit.unwrap_or_else(|| panic!("ray at ({x}, {z}) missed the terrain"));
        let mirror = c.height_at(x, z).expect("inside");
        assert!(
            (y - mirror).abs() < 2e-3,
            "({x}, {z}): physics {y} vs mirror {mirror}"
        );
        // Within one cell twist of the sampler: the rows are not swapped for columns.
        assert!(
            (y - map.sample_bilinear(x, z)).abs() <= c.max_bilinear_delta() + 2e-3,
            "({x}, {z}): physics {y} vs sampler {}",
            map.sample_bilinear(x, z)
        );
    }
}

#[test]
fn row_major_heights_copied_straight_in_mirror_the_ground() {
    // What the helper prevents: the map's own (X-fastest) heights handed to the collider.
    let map = tilted(97, 97);
    let naive = ColliderShape::HeightField {
        nx: map.height() as usize,
        nz: map.width() as usize,
        heights: map.heights().to_vec(),
        scale: Vec3::new(map.world_width(), 1.0, map.world_width()),
    };
    // Low X, high Z: the drawn ground is near the top of the 40 m Z rise; the naive collider
    // reads it at (Z, X) instead.
    let points = vec![[-40.0, 40.0], [40.0, -40.0]];
    let hits = cast_down(naive, points.clone());
    for (&[x, z], hit) in points.iter().zip(&hits) {
        let y = hit.expect("the naive collider still has ground");
        let drawn = map.sample_bilinear(x, z);
        assert!(
            (y - drawn).abs() > 10.0,
            "({x}, {z}): the transposed collider ({y}) should be far from the drawn ground ({drawn})"
        );
    }
}

#[test]
fn holes_cut_by_the_helper_match_the_drawn_cut() {
    let map = tilted(97, 97);
    let mask =
        HolesMask::from_shapes(vec![HoleShape::segment([-10.0, 5.0], [10.0, 5.0], 6.0)]).expect("mask");
    let c = map.collision_heightfield(Some(&mask));
    assert!(c.cut_samples > 0);
    let points = vec![[0.0, 5.0], [8.0, 6.5], [0.0, 30.0], [-30.0, -30.0]];
    let hits = cast_down(shape(&c), points.clone());
    for (&[x, z], hit) in points.iter().zip(&hits) {
        assert_eq!(
            hit.is_some(),
            c.height_at(x, z).is_some(),
            "({x}, {z}): physics {hit:?} vs mirror {:?}",
            c.height_at(x, z)
        );
    }
    assert!(
        hits[0].is_none() && hits[1].is_none(),
        "rays down the trench: {hits:?}"
    );
    assert!(hits[2].is_some() && hits[3].is_some(), "rays beside it: {hits:?}");
}

/// ADR-8715: a ray straight down exactly on a heightfield cell line (a sample's row or column;
/// every simulation sample of a grid-origin map is on two) lands on the drawn surface, for a
/// moved and an unmoved collider and for `raycast` and `raycast_all`. Before the fix parry's
/// single-cell walk missed 18 of 63 such rays on the moved map and 24 of 95 on the unmoved one.
#[test]
fn rays_down_on_cell_lines_land_on_the_drawn_surface() {
    let (n, sim) = (64u32, 320.0f32);
    let heights: Vec<f32> = (0..n)
        .flat_map(|r| (0..n).map(move |c| ((c * 7 + r * 13) % 23) as f32 * 0.5))
        .collect();
    let moved = TerrainMap::from_heights(n, n, heights, sim * (n - 1) as f32 / n as f32, 100.0, -10.0)
        .and_then(|m| m.with_grid_origin([-0.5 * sim / n as f32; 2]))
        .expect("map");
    let plain = tilted(97, 97);
    for (name, map) in [("moved", &moved), ("unmoved", &plain)] {
        let c = map.collision_heightfield(None);
        let [min_x, min_z] = map.world_min();
        let (sx, sz) = (map.sample_spacing(), map.sample_spacing_z());
        let (w, h) = (map.width(), map.height());
        let mut points = Vec::new();
        for k in 1..w - 1 {
            // A column line at an off-line Z, and the sample itself (both lines).
            points.push([min_x + sx * k as f32, min_z + sz * (0.37 + (k % (h - 1)) as f32)]);
            points.push(map.sample_to_world(k, k % h));
        }
        for k in 1..h - 1 {
            points.push([min_x + sx * (0.61 + (k % (w - 1)) as f32), min_z + sz * k as f32]);
        }
        let hits = cast_down_at(shape(&c), c.center, points.clone());
        let all = cast_down_all_at(shape(&c), c.center, points.clone());
        for ((&[x, z], hit), every) in points.iter().zip(&hits).zip(&all) {
            let drawn = terrain_surface_height(map, map.sample_spacing(), x, z);
            let y = hit.unwrap_or_else(|| panic!("{name}: ray at ({x}, {z}) missed the terrain"));
            assert!(
                (y - drawn).abs() < 2e-3,
                "{name} ({x}, {z}): physics {y} vs drawn {drawn}"
            );
            assert_eq!(every.len(), 1, "{name} ({x}, {z}): raycast_all {every:?}");
            assert!(
                (every[0] - drawn).abs() < 2e-3,
                "{name} ({x}, {z}): raycast_all {every:?} vs {drawn}"
            );
        }
    }
}
