// SPDX-License-Identifier: MPL-2.0
//! Native Stage B acceptance (spec 121, ADR-8246).
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]

use regia_world_geom::cook::{CookInputs, GroundInput, cook};
use regia_world_geom::cooked::{CookedTile, CookedTriangle, TriKind, encode_canonical};
use regia_world_geom::{SpanBuf, SurfaceKind, WorldGeom};

fn flat(triangles: Vec<CookedTriangle>) -> WorldGeom {
    let input = GroundInput {
        lattice_n: 17,
        width_m: 32.0,
        offset_m: 0.0,
        step_m: 0.03125,
        counts: vec![0; 17 * 17],
    };
    let mut c = cook(&CookInputs {
        document: b"(version: 1, ground_tile_cells: 16, layers: [])",
        ground: &input,
        terrain: None,
        read_file: &|_| Err("no sources".into()),
    })
    .unwrap()
    .file
    .canonical;
    if !triangles.is_empty() {
        c.tiles = vec![CookedTile {
            origin_mm: [0, 0],
            triangles,
        }];
    }
    WorldGeom::from_canonical_bytes(&encode_canonical(&c).unwrap()).unwrap()
}
fn floor(index: u32, y: i32) -> CookedTriangle {
    CookedTriangle {
        v: [[-10000, y, -10000], [0, y, 10000], [10000, y, -10000]],
        kind: TriKind::Floor,
        flags: 0,
        layer: 0,
        index,
        section: None,
        material: 0,
    }
}
#[test]
fn native_support_uses_the_canonical_floor() {
    let geom = flat(vec![floor(0, 3000)]);
    let state = geom.new_state();
    assert_eq!(geom.support(&state, 1.0, 0.0, 4.0).floor.y, 3.0);
    assert_eq!(
        geom.support(&state, 1.0, 0.0, 1.0).floor.kind,
        SurfaceKind::Ground
    );
}
#[test]
fn column_overflow_is_reported_not_dropped() {
    for count in [13, 17] {
        let geom = flat((0..count).map(|i| floor(i, (i as i32 + 1) * 3000)).collect());
        let mut spans = SpanBuf::default();
        geom.floors_at(&geom.new_state(), 1.0, 0.0, &mut spans);
        assert_eq!(spans.overflowed, count == 17);
        assert_eq!(spans.spans().len(), (count as usize + 1).min(16));
    }
}

fn roof(index: u32, y: i32) -> CookedTriangle {
    CookedTriangle {
        kind: TriKind::Roof,
        ..floor(index, y)
    }
}
#[test]
fn clamp_under_ceiling_reports_lid_contact() {
    let geom = flat(vec![floor(0, 3000), roof(1, 7000)]);
    let state = geom.new_state();
    let hit = geom.clamp_under_ceiling_contact(&state, 1.0, 6.0, 0.0, 3.0, 2.5);
    assert_eq!(hit.y, 4.5);
    assert_eq!(hit.contact.unwrap().y, 7.0);
    assert_eq!(geom.clamp_under_ceiling(&state, 1.0, 6.0, 0.0, 3.0, 2.5), hit.y);
    for y in [3.0, 4.5, 7.001, 12.0] {
        let result = geom.clamp_under_ceiling_contact(&state, 1.0, y, 0.0, 3.0, 2.5);
        assert_eq!(result.y, y);
        assert!(result.contact.is_none());
    }
}
#[test]
fn falling_into_a_skylight_shaft_never_clamps() {
    // The cook removed the shaft lid. The clamp never invents one while the body falls.
    let geom = flat(vec![floor(0, -12000)]);
    let state = geom.new_state();
    for step in 0..200 {
        let y = 15.0 - step as f32 * 0.15;
        assert_eq!(geom.clamp_under_ceiling(&state, 1.0, y, 0.0, -12.0, 2.5), y);
    }
}
#[test]
fn surface_at_inside_a_free_span_equals_support() {
    let geom = flat(vec![
        floor(0, 3000),
        roof(1, 7000),
        floor(2, 9000),
        roof(3, 13000),
    ]);
    let state = geom.new_state();
    for y in [1.0, 3.0, 3.001, 4.0, 6.999, 9.0, 12.99] {
        let s = geom.support(&state, 1.0, 0.0, y);
        let p = geom.surface_at(&state, 1.0, 0.0, y, 0.0);
        assert_eq!(p.under, Some(s.floor), "y={y}");
        assert_eq!(p.over, s.ceiling, "y={y}");
    }
}
#[test]
fn place_floor_obeys_fit_and_accepts_open_sky() {
    let geom = flat(vec![
        floor(0, 3000),
        roof(1, 7000),
        floor(2, 9000),
        roof(3, 11000),
    ]);
    assert_eq!(geom.place_floor(&geom.new_state(), 1.0, 0.0).y, 3.0);
    let geom = flat(vec![floor(0, 3000), roof(1, 7000), floor(2, 9000)]);
    assert_eq!(geom.place_floor(&geom.new_state(), 1.0, 0.0).y, 9.0);
}
#[test]
fn segment_finds_ground_and_layers_and_respects_masks() {
    use regia_world_geom::FaceMask;
    let geom = flat(vec![floor(0, 3000), roof(1, 7000)]);
    let state = geom.new_state();
    for (mask, y) in [
        (FaceMask::SOLID, 7.0),
        (FaceMask::FLOOR, 3.0),
        (FaceMask::GROUND, 0.0),
    ] {
        let hit = geom
            .segment(&state, [1.0, 10.0, 0.0], [1.0, -10.0, 0.0], mask)
            .unwrap();
        // C2 intentionally replaces the former Möller–Trumbore fraction (floor
        // 0x3eb33334) with the sheared-edge fraction (0x3eb33333), ADR-8246.
        let t = (10.0_f32 - y) / 20.0;
        assert_eq!(hit.t.to_bits(), t.to_bits());
        assert_eq!(hit.point[1].to_bits(), (10.0 - 20.0 * t).to_bits());
    }
    assert!(!geom.line_of_sight(&state, [1.0, 10.0, 0.0], [1.0, -10.0, 0.0]));
    assert!(geom.line_of_sight(&state, [1.0, 10.0, 0.0], [1.0, 11.0, 0.0]));
    assert_eq!(
        geom.segment(&state, [f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], FaceMask::SOLID),
        None
    );
}
#[test]
fn disc_push_is_one_pass_and_uses_the_vertical_band() {
    use regia_world_geom::Band;
    let wall = CookedTriangle {
        v: [[0, 0, -10000], [0, 10000, -10000], [0, 0, 10000]],
        kind: TriKind::Wall,
        ..floor(0, 0)
    };
    let geom = flat(vec![wall]);
    let state = geom.new_state();
    let p = geom.push_disc(&state, 0.5, 0.0, 2.0, Band { lo: 1.0, hi: 2.0 });
    assert_eq!(p.xz, [2.0, 0.0]);
    assert_eq!(p.contacts, 1);
    let p = geom.push_disc(&state, 0.5, 0.0, 2.0, Band { lo: 8.0, hi: 9.0 });
    assert_eq!(p.xz, [0.5, 0.0]);
    assert_eq!(p.contacts, 0);
}
#[test]
fn shared_edge_bits_do_not_depend_on_triangle_order() {
    // Noncoplanar adjacent facets: ownership chooses one normal, independently of source order.
    let a = CookedTriangle {
        v: [[-1000, 1000, -1000], [-1000, 1000, 1000], [1000, 2000, -1000]],
        ..floor(0, 0)
    };
    let b = CookedTriangle {
        v: [[1000, 2000, -1000], [-1000, 1000, 1000], [1000, 4000, 1000]],
        ..floor(1, 0)
    };
    let ga = flat(vec![a, b]);
    let gb = flat(vec![
        CookedTriangle { index: 0, ..b },
        CookedTriangle { index: 1, ..a },
    ]);
    for i in 1..100 {
        let x = -1.0 + i as f32 * 0.02;
        let z = -x;
        let a = ga.support(&ga.new_state(), x, z, 5.0).floor;
        let b = gb.support(&gb.new_state(), x, z, 5.0).floor;
        assert_eq!(a.y.to_bits(), b.y.to_bits());
        assert_eq!(a.normal.map(f32::to_bits), b.normal.map(f32::to_bits));
        assert_eq!(a.kind, SurfaceKind::Floor);
    }
}
#[test]
fn compat_shared_edge_order_has_a_committed_witness() {
    use regia_world_geom::compat::mesh::{TunnelCollisionMesh, TunnelFaceKind, TunnelTriangle};
    use regia_world_geom::compat::surface::column;
    let a = TunnelTriangle {
        a: [-1.0, 1.0, -1.0],
        b: [-1.0, 1.0, 1.0],
        c: [1.0, 2.0, -1.0],
        kind: TunnelFaceKind::Floor,
        portal: false,
        pilot_only: false,
    };
    let b = TunnelTriangle {
        a: a.c,
        b: a.b,
        c: [1.0, 4.0, 1.0],
        ..a
    };
    let first = TunnelCollisionMesh::new(vec![a, b]);
    let second = TunnelCollisionMesh::new(vec![b, a]);
    let a = column(None, Some(&first), 0.0, 0.0).span_at(5.0).floor;
    let b = column(None, Some(&second), 0.0, 0.0).span_at(5.0).floor;
    assert_ne!(a.normal.map(f32::to_bits), b.normal.map(f32::to_bits));
}

#[test]
fn surface_at_lid_threshold_does_not_round_away_near_y() {
    let geom = flat(vec![roof(0, 0)]);
    let state = geom.new_state();
    assert_eq!(
        geom.surface_at(&state, 1.0, 0.0, -1e-10, 0.0).over.map(|h| h.y),
        Some(0.0)
    );
    assert!(geom.surface_at(&state, 1.0, 0.0, 0.0, 0.0).over.is_none());
}

#[test]
fn push_disc_chain_is_independent_of_index_cell_size() {
    use regia_world_geom::Band;
    let wall = |index, x| CookedTriangle {
        v: [[x, -10000, -10000], [x, 10000, -10000], [x, -10000, 10000]],
        kind: TriKind::Wall,
        flags: 0,
        layer: 0,
        index,
        section: None,
        material: 0,
    };
    let sixteen = flat(vec![wall(0, 16000), wall(1, 1000)]);
    let mut cooked = sixteen.cooked().clone();
    cooked.settings.index_cell_m = 4.0;
    let four = WorldGeom::from_canonical_bytes(&encode_canonical(&cooked).unwrap()).unwrap();
    let band = Band { lo: -1.0, hi: 1.0 };
    let at_four = four.push_disc(&four.new_state(), 15.0, 0.0, 8.0, band);
    let at_sixteen = sixteen.push_disc(&sixteen.new_state(), 15.0, 0.0, 8.0, band);
    assert_eq!(
        at_four, at_sixteen,
        "single input-cell T3 candidates must also satisfy spec 121 §5.3 cell-size independence"
    );
}

#[test]
fn segment_finds_a_roof_the_6m_sampler_misses() {
    use regia_world_geom::FaceMask;
    use regia_world_geom::compat::mesh::{TunnelCollisionMesh, TunnelFaceKind, TunnelTriangle};
    let filler = CookedTriangle {
        v: [[0, -20000, 0], [32000, -20000, 0], [0, -20000, 32000]],
        ..floor(0, -20000)
    };
    let small = CookedTriangle {
        v: [[7300, 0, 7300], [7700, 0, 7300], [7500, 0, 7700]],
        ..roof(1, 0)
    };
    let triangles = [filler, small];
    let reference = TunnelCollisionMesh::new(
        triangles
            .iter()
            .map(|t| {
                let v = t.v.map(|p| p.map(|v| v as f32 / 1000.0));
                TunnelTriangle {
                    a: v[0],
                    b: v[1],
                    c: v[2],
                    kind: if t.kind == TriKind::Floor {
                        TunnelFaceKind::Floor
                    } else {
                        TunnelFaceKind::Ceiling
                    },
                    portal: false,
                    pilot_only: false,
                }
            })
            .collect(),
    );
    let from = [3.0, 0.9, 12.0];
    let to = [14.0, -1.3, 1.0];
    assert_eq!(
        reference.segment_blocker(from, to),
        None,
        "sampler skips the corner bucket holding this roof"
    );
    let native = flat(triangles.to_vec());
    let hit = native
        .segment(&native.new_state(), from, to, FaceMask::ROOF)
        .unwrap();
    assert_eq!(
        hit.tri,
        Some(regia_world_geom::TriRef {
            layer: 0,
            triangle: 1
        })
    );
    assert!(hit.point[1].abs() < 0.000001);
}

#[cfg(feature = "stats")]
#[test]
fn pyramid_preserves_sloping_ground_and_override_hits() {
    use regia_world_geom::FaceMask;
    let mut c = flat(Vec::new()).cooked().clone();
    for counts in &mut c.ground_counts {
        for (i, h) in counts.iter_mut().enumerate() {
            *h = ((i * 71) % 2048) as u16;
        }
    }
    c.ring_pins.push(regia_world_geom::cooked::VertexHeightMm {
        vertex: 8 * 17 + 8,
        height_mm: 95000,
    });
    let geom = WorldGeom::from_canonical_bytes(&encode_canonical(&c).unwrap()).unwrap();
    let state = geom.new_state();
    let mut seed = 1_u32;
    let mut coordinate = || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 8) as f32 / 16777216.0
    };
    for _ in 0..2048 {
        let from = [
            coordinate() * 48.0 - 24.0,
            coordinate() * 128.0 - 16.0,
            coordinate() * 48.0 - 24.0,
        ];
        let to = [
            coordinate() * 48.0 - 24.0,
            coordinate() * 128.0 - 16.0,
            coordinate() * 48.0 - 24.0,
        ];
        assert_eq!(
            geom.segment(&state, from, to, FaceMask::GROUND),
            geom.segment_without_pyramid(&state, from, to, FaceMask::GROUND),
            "from={from:?} to={to:?}"
        );
    }
}

#[test]
fn pilot_only_flag_does_not_remove_a_geometric_support_floor() {
    let mut f = floor(0, 3000);
    f.flags = regia_world_geom::cooked::TRI_FLAG_PILOT_ONLY;
    let geom = flat(vec![f]);
    assert_eq!(geom.support(&geom.new_state(), 1.0, 0.0, 4.0).floor.y, 3.0);
}
