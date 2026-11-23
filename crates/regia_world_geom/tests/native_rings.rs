// SPDX-License-Identifier: MPL-2.0
//! Real cooked rings: ownership, adjacent f32 points and two-sided seam rays.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]
use regia_world_geom::FaceMask;
use regia_world_geom::ground::vertex_m;
pub mod native_mouths;

#[test]
fn support_never_falls_through_a_ring_edge() {
    check_rings(false, true, true);
}
#[test]
fn ring_support_matches_flat_collar_within_1mm() {
    check_rings(false, true, false);
}
#[test]
fn rays_through_mouth_seam_edges_hit() {
    check_rings(true, false, false);
}
#[test]
fn rays_next_to_mouth_seams_hit() {
    check_rings(true, true, false);
}
fn check_rings(rays: bool, neighbors: bool, permit_higher_floor: bool) {
    for map in ["switchbacks", "cinder_steps", "atacama_rail"] {
        let (output, geom, _, _) = native_mouths::world(map);
        let state = geom.new_state();
        let ground_y = native_mouths::acceptance_ground_m(map);
        let l = &geom.cooked().lattice;
        let mut tested = 0;
        let mut missing = 0;
        for ring in &output.report.ground.rings {
            for i in 0..ring.len() {
                let a = ring[i];
                let b = ring[(i + 1) % ring.len()];
                let a = [vertex_m(l, a.0), vertex_m(l, a.1)];
                let b = [vertex_m(l, b.0), vertex_m(l, b.1)];
                for t in [0.0, 0.25, 0.5, 0.75] {
                    let x = a[0] + (b[0] - a[0]) * t;
                    let z = a[1] + (b[1] - a[1]) * t;
                    for [x, z] in [
                        [x, z],
                        [x.next_down(), z],
                        [x.next_up(), z],
                        [x, z.next_down()],
                        [x, z.next_up()],
                    ]
                    .into_iter()
                    .take(if neighbors { 5 } else { 1 })
                    {
                        tested += 1;
                        if rays {
                            for (dy, dz) in [(1.0, 0.0), (-1.0, 0.0), (4.0, 3.0), (-4.0, -3.0)] {
                                if geom
                                    .segment(
                                        &state,
                                        [x, ground_y + dy, z + dz],
                                        [x, ground_y - dy, z - dz],
                                        FaceMask::SOLID,
                                    )
                                    .is_none()
                                {
                                    missing += 1;
                                    if missing == 1 {
                                        let ground =
                                            regia_world_geom::ground::GroundLattice::new(geom.cooked());
                                        eprintln!(
                                            "RING_DEBUG ground={:?} support={:?}",
                                            ground.ground_triangle_at(x, z),
                                            geom.support(&state, x, z, ground_y + 0.9)
                                        );
                                        #[cfg(feature = "stats")]
                                        eprintln!(
                                            "RING_DEBUG no_pyramid={:?}",
                                            geom.segment_without_pyramid(
                                                &state,
                                                [x, ground_y + dy, z + dz],
                                                [x, ground_y - dy, z - dz],
                                                FaceMask::SOLID
                                            )
                                        );
                                        let cell = ground.locate(x, z).unwrap();
                                        eprintln!(
                                            "RING_DEBUG cell={cell:?} triangles={:?}",
                                            ground.cell_triangles(cell.i, cell.j)
                                        );
                                        for tri in geom
                                            .triangles()
                                            .iter()
                                            .filter(|t| {
                                                t.v.iter().any(|v| {
                                                    (v[0] - x).abs() < 0.001 && (v[2] - z).abs() < 0.001
                                                })
                                            })
                                            .take(20)
                                        {
                                            eprintln!("RING_DEBUG tri={tri:?}");
                                        }
                                    }
                                    if missing <= 3 {
                                        eprintln!("RING_RAY_MISS {map} x={x} z={z} dy={dy} dz={dz}");
                                    }
                                }
                            }
                        } else {
                            let y = geom.support(&state, x, z, ground_y + 0.9).floor.y;
                            if y < ground_y - 0.001 || (!permit_higher_floor && y > ground_y + 0.001) {
                                missing += 1;
                                if missing <= 3 {
                                    eprintln!("RING_SUPPORT_MISS {map} x={x} z={z} y={y}");
                                }
                            }
                        }
                    }
                }
            }
        }
        eprintln!("RINGS {map} rays={rays} probes={tested} misses={missing}");
        assert!(tested > 0);
        assert_eq!(missing, 0, "{map}");
    }
}

#[test]
fn native_resident_geometry_fits_the_sim_budget() {
    for map in ["switchbacks", "cinder_steps", "atacama_rail"] {
        let (output, geom, _, _) = native_mouths::world(map);
        let resident = geom.resident_bytes();
        let budget = output.report.ground.budget_bytes;
        let counts: usize = geom.cooked().ground_counts.iter().map(|b| b.len()).sum();
        let parts = geom.resident_parts();
        eprintln!(
            "NATIVE_RESIDENT {map} actual={resident} cook_estimate={} budget={budget} count_u16={counts} tris={} parts={parts:?}",
            output.report.ground.sim_bytes,
            geom.triangles().len()
        );
        assert!(
            resident <= budget,
            "{map}: native runtime residency exceeds the authored Full-mode budget"
        );
        let estimate = output.report.ground.sim_bytes;
        let delta = (estimate as i64 - resident as i64).unsigned_abs();
        let scale = resident.max(estimate);
        assert!(
            delta.saturating_mul(100) <= scale,
            "{map}: cook estimate {estimate} and resident {resident} differ by more than 1%"
        );
    }
}

/// Open Switchbacks: `surface_at` on the band. Closed: the same point is the lattice ground.
#[test]
fn presentation_surface_at_answers_the_band() {
    use regia_world_geom::SurfaceKind;
    let map = "switchbacks";
    let (output, open, _, _) = native_mouths::world(map);
    let open_state = open.new_state();
    let ground_y = native_mouths::acceptance_ground_m(map);
    let lattice = &open.cooked().lattice;
    let mut on_band = None;
    'rings: for ring in &output.report.ground.rings {
        for i in 0..ring.len() {
            let a = ring[i];
            let b = ring[(i + 1) % ring.len()];
            let a = [vertex_m(lattice, a.0), vertex_m(lattice, a.1)];
            let b = [vertex_m(lattice, b.0), vertex_m(lattice, b.1)];
            for t in [0.0_f32, 0.25, 0.5, 0.75] {
                let x = a[0] + (b[0] - a[0]) * t;
                let z = a[1] + (b[1] - a[1]) * t;
                for [x, z] in [
                    [x, z],
                    [x.next_down(), z],
                    [x.next_up(), z],
                    [x, z.next_down()],
                    [x, z.next_up()],
                ] {
                    let Some(hit) = open.surface_at(&open_state, x, z, ground_y, 0.0).under else {
                        continue;
                    };
                    if hit.kind == SurfaceKind::Floor
                        && hit.tri.is_some()
                        && (hit.y - ground_y).abs() <= 0.001
                    {
                        on_band = Some((x, z));
                        break 'rings;
                    }
                }
            }
        }
    }
    let (x, z) = on_band.expect("open Switchbacks mouth answers a pin-height band");
    let (_, closed, _, _) = native_mouths::world_closed(map);
    let closed_state = closed.new_state();
    let hit = closed
        .surface_at(&closed_state, x, z, ground_y, 0.0)
        .under
        .expect("closed mouth answers a surface");
    assert_eq!(hit.kind, SurfaceKind::Ground, "closed mouth answers the ground");
    assert!(hit.tri.is_none());
    assert!(
        (hit.y - ground_y).abs() <= 0.001,
        "closed ground {hit:?} is not the lattice at {ground_y}"
    );
}
