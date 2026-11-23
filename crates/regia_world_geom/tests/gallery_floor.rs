// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/tests/gallery_floor.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-102, mark: as-is (Stage A: unchanged).
// Edits: module paths, doc comments. Re-synced from regia-absorb-1 (d6ed37677) by W-02 (ADR-8167): no
// `sticky` argument and no `previous` floor.

//! Integration: synthetic gallery floor, walls, and ceiling vs heightfield.

use regia_world_geom::compat::ground::{SampleFloorTunnels, sample_floor_with_tunnels};
use regia_world_geom::compat::mesh::{
    clamp_altitude_under_ceiling, separate_disc_from_walls, synthetic_gallery,
};
use regia_world_geom::compat::sampler::{FloorSurface, HEIGHT_METERS_PER_COUNT, HeightGrid, TerrainSampler};

fn flat_terrain(side: usize, world_width: f32, raw_height: u16) -> TerrainSampler {
    let heights = vec![raw_height; side * side];
    TerrainSampler::new(HeightGrid::square(side, heights).expect("grid"), world_width)
}

#[test]
fn tunnels_flag_off_matches_outdoor_heightfield() {
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    let outdoor_y = terrain.height_at_world(0.0, 0.0);
    let mesh = synthetic_gallery(10.0, 10.0, -5.0, 1.0, true);

    let sample = sample_floor_with_tunnels(
        Some(&terrain),
        0.0,
        0.0,
        -1.0,
        SampleFloorTunnels {
            mesh: Some(&mesh),
            enabled: false,
        },
    );

    assert_eq!(sample.surface, FloorSurface::Heightfield);
    assert!((sample.point[1] - outdoor_y).abs() < 1e-5);
}

#[test]
fn tunnels_flag_on_gallery_point_uses_tunnel_floor() {
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    assert!(terrain.height_at_world(0.0, 0.0).abs() < 1e-4);
    let mesh = synthetic_gallery(10.0, 10.0, -5.0, 1.0, true);

    let sample = sample_floor_with_tunnels(
        Some(&terrain),
        0.0,
        0.0,
        -4.0,
        SampleFloorTunnels {
            mesh: Some(&mesh),
            enabled: true,
        },
    );

    assert_eq!(sample.surface, FloorSurface::TunnelFloor);
    assert!(
        (sample.point[1] - (-5.0)).abs() < 1e-4,
        "floor at y=-5, got {}",
        sample.point[1]
    );
}

#[test]
fn disc_inside_wall_is_pushed_out() {
    let mesh = synthetic_gallery(10.0, 10.0, 0.0, 6.0, false);
    let radius = 2.5;
    // 1 m inside the -X wall (x = -10). Inward normal is +X.
    let [x, z] = separate_disc_from_walls(&mesh, -9.0, 0.0, radius);
    assert!((x - (-10.0 + radius)).abs() < 1e-3, "x={x}");
    assert!(z.abs() < 1e-3, "z={z}");
}

#[test]
fn hull_under_ceiling_is_clamped() {
    let mesh = synthetic_gallery(10.0, 10.0, 0.0, 6.0, false);
    let hull_radius = 2.5_f32;
    let clamped = clamp_altitude_under_ceiling(Some(&mesh), 0.0, 10.0, 0.0, hull_radius);
    assert!((clamped - (6.0 - hull_radius)).abs() < 1e-5);
}

#[test]
fn clearance_below_hull_radius_falls_back_to_outdoor() {
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    // Ceiling only 1.0 m above floor — less than HULL_FIT_M (2.5).
    let mesh = synthetic_gallery(10.0, 10.0, -5.0, -4.0, false);
    // T3: the fallback is the outdoor ground.

    let sample = sample_floor_with_tunnels(
        Some(&terrain),
        0.0,
        0.0,
        -4.5,
        SampleFloorTunnels {
            mesh: Some(&mesh),
            enabled: true,
        },
    );

    assert_eq!(sample.surface, FloorSurface::Heightfield);
    assert!(
        (sample.point[1] - 0.0).abs() < 1e-4,
        "outdoor, not history: {}",
        sample.point[1]
    );
}
