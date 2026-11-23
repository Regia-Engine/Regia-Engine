// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/tunnels.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1324-1648 (the module's tests), mark: as-is (Stage A: unchanged).
// Edits: module paths (the tests cover `compat::mesh` and `compat::ground` together, so they live
// beside both), lint attributes, one comment naming the host constant. Re-synced from regia-absorb-1
// (d6ed37677, lines 1430-1785) by W-02 (ADR-8167): no `sticky` or `previous` arguments, `step_ok` at
// the 50 mm bound.

//! DO27's `tunnels.rs` tests: the collision mesh, the wall push, the ceiling clamp, the nav overlay and
//! the floor query.

#![allow(clippy::unwrap_used)]

use super::ground::*;
use super::mesh::*;
use crate::compat::sampler::{FloorSurface, HEIGHT_METERS_PER_COUNT, HeightGrid, TerrainSampler};

fn flat_terrain(side: usize, world_width: f32, raw_height: u16) -> TerrainSampler {
    let heights = vec![raw_height; side * side];
    TerrainSampler::new(
        HeightGrid::square(side, heights).expect("square grid"),
        world_width,
    )
}

/// Square ramp grid, raw heights = sample index.
fn ramp_grid(side: usize) -> HeightGrid {
    HeightGrid::square(side, (0..(side * side) as u16).collect()).expect("grid")
}

#[test]
fn outdoor_only_unchanged_when_tunnels_flag_off() {
    let grid = ramp_grid(4);
    let terrain = TerrainSampler::new(grid, 40.0);
    // Gallery below outdoor; flag off must ignore it.
    let mesh = synthetic_gallery(10.0, 10.0, -5.0, 1.0, true);
    // Sticky TunnelFloor must still return heightfield and never claim tunnel.
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
    let outdoor_y = terrain.height_at_world(0.0, 0.0);
    assert!((sample.point[1] - outdoor_y).abs() < 1e-5);
}

#[test]
fn a_point_in_the_gallery_follows_its_floor_mesh() {
    // Flat outdoor at y=0 (center-zeroed uniform grid).
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    assert!(terrain.height_at_world(0.0, 0.0).abs() < 1e-4);
    // Gallery floor at -5, ceiling at 1 → 6 m clearance.
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
fn hull_that_cannot_fit_falls_back_to_outdoor() {
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    // Ceiling only 1.0 m above floor — less than HULL_FIT_M (2.5).
    let mesh = synthetic_gallery(10.0, 10.0, -5.0, -4.0, false);
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
        "outdoor: {}",
        sample.point[1]
    );
}

#[test]
fn a_portal_apron_at_ground_height_is_the_open_ground() {
    // A mesh floor at the height of the terrain is one surface with it: the open ground.
    let terrain = flat_terrain(4, 40.0, 0); // outdoor y ≈ 0 after center zero
    let outdoor_y = terrain.height_at_world(0.0, -11.0);
    let mesh = synthetic_gallery(10.0, 10.0, 0.0, 6.0, true);
    // Stand on portal apron (z ≈ -11).
    let (x, z) = (0.0, -11.0);
    assert!(mesh.is_portal_at(x, z));
    for hint in [outdoor_y - 0.2, outdoor_y, outdoor_y + 0.9] {
        let base = sample_floor_with_tunnels(
            Some(&terrain),
            x,
            z,
            hint,
            SampleFloorTunnels {
                mesh: Some(&mesh),
                enabled: true,
            },
        );
        assert_eq!(base.surface, FloorSurface::Heightfield, "hint {hint}");
        assert!(
            (base.point[1] - outdoor_y).abs() < 1e-4,
            "hint {hint}: {}",
            base.point[1]
        );
    }
}

#[test]
fn query_ground_pairs_gallery_floor_with_its_lid() {
    // T3: one query returns the surface under the body AND the ceiling
    // over it. A body in the gallery stands on the gallery floor with
    // the lid paired; a body on the surface stands on the heightfield
    // with no ceiling, though the gallery is right under it.
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    // Lid a metre under the surface: the gallery is fully buried.
    let mesh = synthetic_gallery(10.0, 10.0, -5.0, -1.0, true);
    let inside = query_ground(Some(&terrain), Some(&mesh), 0.0, 0.0, -4.0);
    assert_eq!(inside.floor.surface, FloorSurface::TunnelFloor);
    assert!((inside.floor.point[1] - (-5.0)).abs() < 1e-4);
    assert!(
        inside.ceiling.is_some_and(|lid| (lid - (-1.0)).abs() < 1e-4),
        "lid paired: {:?}",
        inside.ceiling
    );
    let surface = query_ground(Some(&terrain), Some(&mesh), 0.0, 0.0, 0.0);
    assert_eq!(surface.floor.surface, FloorSurface::Heightfield);
    assert!(surface.floor.point[1].abs() < 1e-4);
    assert_eq!(surface.ceiling, None);
}

#[test]
fn step_ok_matches_the_t3_acceptance() {
    // T3: the surface under a body moves at most 0.05 m between two ticks.
    assert!(step_ok(0, 49));
    assert!(step_ok(0, -50));
    assert!(!step_ok(0, 51));
    assert!(!step_ok(-30_000, -29_000));
    // The old sill hop (0.30 m) and the old mouth hops (0.55-0.91 m) fail.
    assert!(!step_ok(0, 300));
    assert!(!step_ok(0, 550));
    assert!(step_ok(0, 40));
}

#[test]
fn hole_mask_cuts_the_heightfield_at_the_mouth_only() {
    let mask = HoleMask {
        discs: vec![HoleDisc {
            x_mm: 0,
            z_mm: 0,
            r_mm: 6000,
        }],
    };
    assert!(mask.cut_at(0, 0));
    assert!(mask.cut_at(6000, 0), "the rim is cut");
    assert!(mask.cut_at(4000, 4000), "the diagonal inside the disc is cut");
    assert!(!mask.cut_at(6001, 0));
    assert!(
        !mask.cut_at(4300, 4300),
        "the diagonal outside the disc is not cut"
    );
    assert!(!mask.cut_at(0, 60_000));
    assert!(!HoleMask::default().cut_at(0, 0));
}

#[test]
fn hole_mask_cut_at_survives_huge_coordinates_without_overflow() {
    // 4e9 mm apart: i32 subtraction would wrap to ~+295e6 and wrongly
    // cut; the i128 dx/dz must keep the point outside the disc.
    let mask = HoleMask {
        discs: vec![HoleDisc {
            x_mm: 2_000_000_000,
            z_mm: 0,
            r_mm: 2_147_483_000,
        }],
    };
    assert!(mask.cut_at(2_000_000_000, 0), "the disc centre always cuts");
    assert!(!mask.cut_at(-2_000_000_000, 0), "i32 wrap would land inside");
}

#[test]
fn query_ground_reports_a_wall_ahead_as_a_rising_floor() {
    // T3: no floor is under the point, so the lowest floor over it
    // answers — the hover wall rule and the ledge rule fire on exactly
    // this.
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    let mesh = synthetic_gallery(10.0, 10.0, -5.0, 1.0, false);
    // A body under the gallery floor (under the world, off the mesh):
    // the gallery floor over it is the rising answer, not the surface.
    let support = query_ground(Some(&terrain), Some(&mesh), 0.0, 0.0, -20.0);
    assert_eq!(support.floor.surface, FloorSurface::TunnelFloor);
    assert!((support.floor.point[1] - (-5.0)).abs() < 1e-4);
}

#[test]
fn query_ground_walks_stacked_decks_without_history() {
    // T3: four stacked decks (Route A shape). The body stands on the
    // deck it is over — no last floor needed to pick the right storey.
    let mesh = synthetic_four_decks();
    for (body_y, deck_y) in [(0.5, 0.0), (-3.5, -4.0), (-7.5, -8.0), (-11.5, -12.0)] {
        let support = query_ground(None, Some(&mesh), 0.0, 0.0, body_y);
        assert_eq!(support.floor.surface, FloorSurface::TunnelFloor, "body {body_y}");
        assert!(
            (support.floor.point[1] - deck_y).abs() < 1e-4,
            "body {body_y}: floor {}, not deck {deck_y}",
            support.floor.point[1]
        );
    }
    // Falling between decks: the point is in the span of the deck below it,
    // so the ledge rule (not the query) owns the fall.
    let support = query_ground(None, Some(&mesh), 0.0, 0.0, -6.0);
    assert!((support.floor.point[1] - (-8.0)).abs() < 1e-4);
}

#[test]
fn pathing_with_no_portal_returns_none() {
    let mesh = synthetic_gallery(10.0, 10.0, 0.0, 6.0, false);
    let overlay = TunnelNavOverlay::bake(&mesh);
    assert!(overlay.portal_indices.is_empty());
    let next = overlay.next_waypoint([0.0, 0.0, 0.0], [100.0, 0.0, 100.0]);
    assert!(next.is_none(), "no portal → unit holds");
}

#[test]
fn pathing_with_portal_returns_a_waypoint() {
    let mesh = synthetic_gallery(10.0, 10.0, 0.0, 6.0, true);
    let overlay = TunnelNavOverlay::bake(&mesh);
    assert!(!overlay.portal_indices.is_empty());
    let next = overlay.next_waypoint([0.0, 0.0, 0.0], [0.0, 0.0, -50.0]);
    assert!(next.is_some());
}

#[test]
fn disc_is_pushed_out_of_a_gallery_wall() {
    let mesh = synthetic_gallery(10.0, 10.0, 0.0, 6.0, false);
    let radius = 2.5;
    // 1 m inside the -X wall (x = -10). Inward normal is +X.
    let [x, z] = separate_disc_from_walls(&mesh, -9.0, 0.0, radius);
    assert!((x - (-10.0 + radius)).abs() < 1e-3, "x={x}");
    assert!(z.abs() < 1e-3, "z={z}");
    let [cx, cz] = separate_disc_from_walls(&mesh, 0.0, 0.0, radius);
    assert!(cx.abs() < 1e-3 && cz.abs() < 1e-3, "center moved to {cx},{cz}");
}

#[test]
fn altitude_clamp_keeps_hull_under_ceiling() {
    let mesh = synthetic_gallery(10.0, 10.0, 0.0, 6.0, false);
    // 2.5 = the host's unit collision radius
    let hull_radius = 2.5_f32;
    let clamped = clamp_altitude_under_ceiling(Some(&mesh), 0.0, 10.0, 0.0, hull_radius);
    assert!((clamped - (6.0 - hull_radius)).abs() < 1e-5);
}

#[test]
fn four_decks_are_all_pathable() {
    let mesh = synthetic_four_decks();
    let overlay = TunnelNavOverlay::bake(&mesh);
    let depths: Vec<u8> = overlay.levels.clone();
    for level in 0..MAX_NAV_LEVELS {
        assert!(depths.contains(&level), "missing deck {level} in {depths:?}");
    }
    let deep = overlay
        .waypoints
        .iter()
        .zip(overlay.levels.iter())
        .find(|(_, level)| **level == MAX_NAV_LEVELS - 1)
        .map(|(p, _)| *p)
        .expect("deepest deck");
    let mut pos = [0.0, 0.0, 0.0];
    let mut saw_deep = false;
    for _ in 0..48 {
        if overlay.depth_at(pos) == Some(MAX_NAV_LEVELS - 1) {
            saw_deep = true;
            break;
        }
        let next = overlay.next_waypoint(pos, deep).expect("path across four decks");
        if dist_sq(next, pos) < 1e-8 {
            break;
        }
        pos = next;
    }
    assert!(saw_deep, "path never reached the fourth deck, stopped at {pos:?}");
}

#[test]
fn deep_gallery_does_not_swallow_the_surface() {
    let terrain = flat_terrain(4, 40.0, (8.0 / HEIGHT_METERS_PER_COUNT) as u16);
    let mesh = synthetic_four_decks();
    let surface = sample_floor_with_tunnels(
        Some(&terrain),
        0.0,
        0.0,
        10.0,
        SampleFloorTunnels {
            mesh: Some(&mesh),
            enabled: true,
        },
    );
    assert_eq!(surface.surface, FloorSurface::Heightfield);
    let deep = sample_floor_with_tunnels(
        Some(&terrain),
        0.0,
        0.0,
        -12.0,
        SampleFloorTunnels {
            mesh: Some(&mesh),
            enabled: true,
        },
    );
    assert_eq!(deep.surface, FloorSurface::TunnelFloor);
    assert!(
        (deep.point[1] - (-12.0)).abs() < 0.2,
        "deep deck y={}, want -12",
        deep.point[1]
    );
}

#[test]
fn building_decks_are_for_pilots() {
    let mesh = synthetic_building_decks();
    let overlay = TunnelNavOverlay::bake(&mesh);
    let top = overlay
        .waypoints
        .iter()
        .zip(overlay.levels.iter())
        .find(|(_, level)| **level == 0)
        .map(|(p, _)| *p)
        .expect("top deck");
    let ground = [0.0, 0.0, 0.0];
    assert!(overlay.next_waypoint_for(ground, top, true).is_some());
    assert!(overlay.next_waypoint_for(ground, top, false).is_none());
}
