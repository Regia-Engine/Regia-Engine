// SPDX-License-Identifier: MPL-2.0
//! Review proof for the w11-crater-edits speed path (ADR-8818).
//!
//! The seeded books compare the tile walk with the reference walk on ordinary maps. These cases
//! are the ones those books do not ask: a sibling that still shares override blocks, a crater at
//! the height clamp, degenerate and coincident roofs, centres at the `i32` extremes, a lattice
//! whose vertices share coordinates, a map near the extent limit, and the segments and snapshot
//! of that ground. A failure is a bit the lockstep query would not share.

#![cfg(feature = "stats")]
#![allow(clippy::unwrap_used)]

use regia_detsim::float::{det_max, det_min};
use regia_world_geom::compat::mesh::{TunnelFaceKind, TunnelTriangle};
use regia_world_geom::cook::{CookInputs, GroundInput, cook, quantize_mesh};
use regia_world_geom::cooked::{CookedTile, CookedTriangle, WorldGeomCooked, encode_canonical};
use regia_world_geom::edit::{HEIGHT_LIMIT_MM, MAX_DEPTH_MM, MAX_RADIUS_MM};
use regia_world_geom::{
    DirtyRect, EditStats, FaceMask, GeomEditError, GeomState, GroundEdit, SegmentHit, WorldGeom, mm_to_m,
};
use std::collections::BTreeMap;

fn cook_section(ground: &GroundInput, mode: &str) -> WorldGeomCooked {
    let doc = format!("(version:1, ground_tile_cells:16, {mode} layers:[])");
    cook(&CookInputs {
        document: doc.as_bytes(),
        ground,
        terrain: None,
        read_file: &|_| Err("no source".into()),
    })
    .unwrap()
    .file
    .canonical
}

fn load(c: &WorldGeomCooked) -> WorldGeom {
    WorldGeom::from_canonical_bytes(&encode_canonical(c).unwrap()).unwrap()
}

fn lattice(n: u32, width_m: f32, offset_m: f32, mode: &str, height: impl Fn(u32, u32) -> u16) -> WorldGeom {
    let counts: Vec<u16> = (0..n * n).map(|v| height(v % n, v / n)).collect();
    let ground = GroundInput {
        lattice_n: n,
        width_m,
        offset_m,
        step_m: 0.001,
        counts,
    };
    load(&cook_section(&ground, mode))
}

fn full(n: u32, width_m: f32, offset_m: f32) -> WorldGeom {
    lattice(n, width_m, offset_m, "", |i, j| {
        1_000u16 + ((i * 3 + j * 5) % 17) as u16
    })
}

fn nested(n: u32) -> WorldGeom {
    lattice(
        n,
        (n - 1) as f32 * 0.3125,
        -20.0,
        "ground: Nested(max_level: 3),",
        |i, j| {
            let slope = 20_000u32 + 40 * i + 25 * j;
            let rough = if i < 48 { (i * 37 + j * 91) % 7 * 120 } else { 0 };
            let di = i64::from(i) - 64;
            let dj = i64::from(j) - 64;
            let bowl = u32::try_from((di * di + dj * dj) / 8).unwrap_or(0);
            (slope + rough + bowl) as u16
        },
    )
}

fn tri(a: [f32; 3], b: [f32; 3], c: [f32; 3], kind: TunnelFaceKind) -> TunnelTriangle {
    TunnelTriangle {
        a,
        b,
        c,
        kind,
        portal: false,
        pilot_only: false,
    }
}

fn quad(y: [f32; 2], lo: [f32; 2], hi: [f32; 2], kind: TunnelFaceKind) -> [TunnelTriangle; 2] {
    [
        tri(
            [lo[0], y[0], lo[1]],
            [lo[0], y[0], hi[1]],
            [hi[0], y[1], lo[1]],
            kind,
        ),
        tri(
            [hi[0], y[1], lo[1]],
            [lo[0], y[0], hi[1]],
            [hi[0], y[1], hi[1]],
            kind,
        ),
    ]
}

/// Replaces the cooked tiles. Degenerate triangles are cook-legal: the loader checks extent and
/// order, and a vertical or zero-area plane is refused later by the crossing test.
fn with_triangles(mut c: WorldGeomCooked, tris: &[TunnelTriangle]) -> WorldGeom {
    let mut tiles: BTreeMap<(i32, i32), Vec<CookedTriangle>> = BTreeMap::new();
    for (i, t) in quantize_mesh(tris).unwrap().into_iter().enumerate() {
        let ox = t.v[0][0].div_euclid(1_024_000) * 1_024_000;
        let oz = t.v[0][2].div_euclid(1_024_000) * 1_024_000;
        tiles.entry((oz, ox)).or_default().push(CookedTriangle {
            v: t.v.map(|p| [p[0] - ox, p[1], p[2] - oz]),
            kind: t.kind,
            flags: t.flags,
            layer: 0,
            index: i as u32,
            section: None,
            material: 0,
        });
    }
    c.tiles = tiles
        .into_iter()
        .map(|((oz, ox), triangles)| CookedTile {
            origin_mm: [ox, oz],
            triangles,
        })
        .collect();
    load(&c)
}

fn roofed() -> WorldGeom {
    let n = 65u32;
    let mut tris = Vec::new();
    // A zero-area roof (one point) and a colinear roof over the origin.
    tris.push(tri(
        [0.0, 2.0, 0.0],
        [0.0, 2.0, 0.0],
        [0.0, 2.0, 0.0],
        TunnelFaceKind::Ceiling,
    ));
    tris.push(tri(
        [-1.0, 1.5, 0.0],
        [0.0, 1.5, 0.0],
        [1.0, 1.5, 0.0],
        TunnelFaceKind::Ceiling,
    ));
    // Two coincident quads: the same plane twice, so the strict `>` tie keeps the first.
    tris.extend(quad(
        [0.4, 0.4],
        [-2.0, -2.0],
        [2.0, 2.0],
        TunnelFaceKind::Ceiling,
    ));
    tris.extend(quad(
        [0.4, 0.4],
        [-2.0, -2.0],
        [2.0, 2.0],
        TunnelFaceKind::Ceiling,
    ));
    // A roof whose xz extent is a sliver, over a lattice line.
    tris.push(tri(
        [0.3125, 3.0, -3.0],
        [0.3125, -4.0, 0.0],
        [0.3135, 3.0, 3.0],
        TunnelFaceKind::Ceiling,
    ));
    // Floors under the same xz, coincident with each other.
    tris.extend(quad(
        [-1.0, -1.0],
        [-2.0, -2.0],
        [2.0, 2.0],
        TunnelFaceKind::Floor,
    ));
    tris.extend(quad(
        [-1.0, -1.0],
        [-2.0, -2.0],
        [2.0, 2.0],
        TunnelFaceKind::Floor,
    ));
    let cooked = cook_section(
        &GroundInput {
            lattice_n: n,
            width_m: 20.0,
            offset_m: 1.0,
            step_m: 0.001,
            counts: (0..n * n)
                .map(|v| 1_000u16 + (((v % n) * 3 + (v / n) * 5) % 17) as u16)
                .collect(),
        },
        "",
    );
    with_triangles(cooked, &tris)
}

fn apply_both(
    geom: &WorldGeom,
    edit: &GroundEdit,
    walk: &mut GeomState,
    reference: &mut GeomState,
    what: &str,
) -> Result<DirtyRect, GeomEditError> {
    let (mut walk_stats, mut reference_stats) = (EditStats::default(), EditStats::default());
    let by_walk = walk.apply_ground_edit_with_stats(geom, edit, &mut walk_stats);
    let by_reference = reference.apply_ground_edit_reference(geom, edit, &mut reference_stats);
    assert_eq!(by_walk, by_reference, "{what}: result of {edit:?}");
    assert_eq!(walk_stats, reference_stats, "{what}: counters of {edit:?}");
    assert_eq!(walk.state_hash(), reference.state_hash(), "{what}: hash");
    assert_eq!(walk.generation(), reference.generation(), "{what}: generation");
    assert!(*walk == *reference, "{what}: overrides");
    let (mut a, mut b) = (Vec::new(), Vec::new());
    walk.encode(&mut a);
    reference.encode(&mut b);
    assert_eq!(a, b, "{what}: snapshot bytes");
    if walk_stats.vertices_written > 0 || walk_stats.tiles_promoted > 0 {
        assert!(walk.keeps_ground_ranges(), "{what}: the tile walk keeps ranges");
        assert!(
            !reference.keeps_ground_ranges(),
            "{what}: the reference walk keeps none"
        );
    }
    by_walk
}

fn kind_tag(kind: regia_world_geom::SurfaceKind) -> u8 {
    match kind {
        regia_world_geom::SurfaceKind::Ground => 1,
        regia_world_geom::SurfaceKind::Floor => 2,
        regia_world_geom::SurfaceKind::Roof => 3,
        regia_world_geom::SurfaceKind::Wall => 4,
        regia_world_geom::SurfaceKind::Blocker => 5,
    }
}

fn mix(h: &mut u64, v: u64) {
    *h = h.wrapping_mul(0x9e37_79b1_85eb_ca87).wrapping_add(v);
}

fn mix_hit(h: &mut u64, hit: Option<SegmentHit>) {
    match hit {
        None => mix(h, 0),
        Some(hit) => {
            mix(h, 1);
            mix(h, u64::from(hit.t.to_bits()));
            mix(h, u64::from(hit.point[0].to_bits()));
            mix(h, u64::from(hit.point[1].to_bits()));
            mix(h, u64::from(hit.point[2].to_bits()));
            mix(h, u64::from(kind_tag(hit.kind)));
            match hit.tri {
                None => mix(h, 0),
                Some(tri) => {
                    mix(h, 1);
                    mix(h, u64::from(tri.layer));
                    mix(h, u64::from(tri.triangle));
                }
            }
        }
    }
}

fn assert_same_hit(a: Option<SegmentHit>, b: Option<SegmentHit>, what: &str) {
    match (a, b) {
        (None, None) => {}
        (Some(a), Some(b)) => {
            assert_eq!(
                (a.t.to_bits(), a.point.map(|v| v.to_bits()), a.kind, a.tri),
                (b.t.to_bits(), b.point.map(|v| v.to_bits()), b.kind, b.tri),
                "{what}: fast {a:?} full {b:?}"
            );
        }
        (Some(hit), None) => panic!("{what}: only the pyramid walk hit {hit:?}"),
        (None, Some(hit)) => panic!("{what}: only the full walk hit {hit:?}"),
    }
}

fn height_span(geom: &WorldGeom, state: &GeomState) -> (f32, f32) {
    let n = geom.cooked().lattice.lattice_n;
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for v in 0..n * n {
        if let Some(mm) = state.vertex_height_mm(geom, v) {
            let y = mm_to_m(0, mm);
            lo = det_min(lo, y);
            hi = det_max(hi, y);
        }
    }
    (lo, hi)
}

fn vertex_m(geom: &WorldGeom, i: u32) -> f32 {
    mm_to_m(0, geom.cooked().lattice.vertex_mm(i) as i32)
}

/// Horizontal, diagonal and vertical segments around the edited heights, plus a dip through the
/// middle of the map. The pyramid skip is allowed to differ from the full walk only by doing
/// more work, so every hit bit has to match.
fn rays_of(geom: &WorldGeom, state: &GeomState) -> Vec<([f32; 3], [f32; 3])> {
    let n = geom.cooked().lattice.lattice_n;
    let x0 = vertex_m(geom, 0);
    let x1 = vertex_m(geom, n - 1);
    let mid = vertex_m(geom, n / 2);
    let (lo, hi) = height_span(geom, state);
    let mut rays = Vec::new();
    let columns = [0, 1, n / 2, n.saturating_sub(2), n - 1];
    for &col in &columns {
        let x = vertex_m(geom, col);
        let z = vertex_m(geom, if col + 3 < n { col + 3 } else { 0 });
        rays.push(([x, hi + 50.0, z], [x, lo - 50.0, z]));
        rays.push(([x, lo - 50.0, z], [x, hi + 50.0, z]));
        // Just inside one cell, so a bad owner-tile range cannot hide behind a long clip.
        let x2 = vertex_m(geom, col.saturating_add(1).min(n - 1));
        rays.push(([x, hi + 1.0, mid], [x2, lo - 1.0, mid]));
    }
    for extra in [-1.0_f32, -0.002, 0.0, 0.0005, 0.002, 0.05, 1.0] {
        let y = if extra >= 0.0 { hi + extra } else { lo + extra };
        rays.push(([x0, y, x0], [x1, y, x1]));
        rays.push(([x0, y, x1], [x1, y, x0]));
        rays.push(([x0, y + 2.0, mid], [x1, y - 2.0, mid]));
    }
    rays.push(([mid, hi + 5.0, x0], [mid, lo + 0.01, x1]));
    rays.push(([x0 - 1.0, hi, x0 - 1.0], [x1 + 1.0, lo, x1 + 1.0]));
    rays
}

fn assert_segments(geom: &WorldGeom, state: &GeomState, what: &str) -> u64 {
    let mut fp = 0x6a09_e667_f3bc_c909_u64;
    let mut t_only = 0u32;
    let mut point = 0u32;
    let mut miss = 0u32;
    for mask in [FaceMask::GROUND, FaceMask::SOLID] {
        for (k, (from, to)) in rays_of(geom, state).into_iter().enumerate() {
            let fast = geom.segment(state, from, to, mask);
            let full = geom.segment_without_pyramid(state, from, to, mask);
            match (fast, full) {
                (None, None) => {}
                (Some(a), Some(b)) => {
                    let points = a.point.map(|v| v.to_bits()) == b.point.map(|v| v.to_bits());
                    let rest = a.kind == b.kind && a.tri == b.tri;
                    if !points || !rest {
                        point += 1;
                        if point <= 3 {
                            println!(
                                "{what} mask {} ray {k} from {from:?} to {to:?}: pyramid {fast:?} full {full:?}",
                                mask.0
                            );
                        }
                    } else if a.t.to_bits() != b.t.to_bits() {
                        t_only += 1;
                    }
                }
                _ => {
                    miss += 1;
                    println!(
                        "{what} MISS mask {} ray {k} from {from:?} to {to:?}: pyramid {fast:?} full {full:?}",
                        mask.0
                    );
                }
            }
            mix_hit(&mut fp, fast);
        }
    }
    println!("{what}: t-only {t_only}, point {point}, miss {miss}");
    let n = geom.cooked().lattice.lattice_n;
    let step = (n / 8).max(1);
    for j in (0..n).step_by(step as usize) {
        for i in (0..n).step_by(step as usize) {
            let (x, z) = (vertex_m(geom, i), vertex_m(geom, j));
            let y = mm_to_m(0, state.vertex_height_mm(geom, j * n + i).unwrap_or(0)) + 2.0;
            let span = geom.support(state, x, z, y);
            mix(&mut fp, u64::from(span.floor.y.to_bits()));
            mix(&mut fp, u64::from(kind_tag(span.floor.kind)));
            if let Some(lid) = span.ceiling {
                mix(&mut fp, u64::from(lid.y.to_bits()));
            }
        }
    }
    fp
}

fn assert_supports_match(geom: &WorldGeom, a: &GeomState, b: &GeomState, what: &str) {
    let n = geom.cooked().lattice.lattice_n;
    let step = (n / 8).max(1);
    for j in (0..n.saturating_sub(1)).step_by(step as usize) {
        for i in (0..n.saturating_sub(1)).step_by(step as usize) {
            // A point in the cell, not only on the vertex, so a bad promotion plane shows up.
            let x = (vertex_m(geom, i) + vertex_m(geom, i + 1)) * 0.5;
            let z = (vertex_m(geom, j) + vertex_m(geom, j + 1)) * 0.5;
            let y = 10_000.0;
            let (sa, sb) = (geom.support(a, x, z, y), geom.support(b, x, z, y));
            assert_eq!(
                sa.floor.y.to_bits(),
                sb.floor.y.to_bits(),
                "{what} floor at ({i},{j})"
            );
            assert_eq!(sa.floor.kind, sb.floor.kind, "{what} floor kind");
            match (sa.ceiling, sb.ceiling) {
                (None, None) => {}
                (Some(pa), Some(pb)) => {
                    assert_eq!(pa.y.to_bits(), pb.y.to_bits(), "{what} lid at ({i},{j})");
                    assert_eq!(pa.kind, pb.kind, "{what} lid kind");
                }
                (Some(pa), None) => panic!("{what}: only the first state has a lid {pa:?}"),
                (None, Some(pb)) => panic!("{what}: only the second state has a lid {pb:?}"),
            }
        }
    }
}

fn corner_mm(geom: &WorldGeom, i: u32, j: u32) -> (i32, i32) {
    let l = &geom.cooked().lattice;
    (l.vertex_mm(i) as i32, l.vertex_mm(j) as i32)
}

/// Tile corners, the last partial tile, one-vertex rectangles and a clamp-depth crater.
#[test]
fn fast_walk_matches_reference_on_adversarial_edits() {
    for (what, geom) in [
        ("full", full(65, 20.0, 0.5)),
        ("nested-131", nested(131)),
        ("nested-129", nested(129)),
    ] {
        let (mut walk, mut reference) = (geom.new_state(), geom.new_state());
        let l = geom.cooked().lattice;
        let g = u32::from(l.ground_tile_cells);
        let per = l.tiles_per_side();
        let n = l.lattice_n;
        for b in 0..per {
            for a in 0..per {
                let col = (a * g).min(n - 1);
                let row = (b * g).min(n - 1);
                let edit = GroundEdit::Rect {
                    col,
                    row,
                    width: 1,
                    height: 1,
                    heights_mm: vec![if (a + b).is_multiple_of(2) {
                        HEIGHT_LIMIT_MM
                    } else {
                        -HEIGHT_LIMIT_MM
                    }],
                };
                let _ = apply_both(
                    &geom,
                    &edit,
                    &mut walk,
                    &mut reference,
                    &format!("{what} corner ({a},{b})"),
                );
            }
        }
        // The vertex the last tile shares with its west and south neighbors.
        let edge = (per - 1) * g;
        let (x_mm, z_mm) = corner_mm(&geom, edge.min(n - 1), edge.min(n - 1));
        let _ = apply_both(
            &geom,
            &GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm: 2_500,
                depth_mm: MAX_DEPTH_MM,
            },
            &mut walk,
            &mut reference,
            &format!("{what} clamp crater"),
        );
        let _ = apply_both(
            &geom,
            &GroundEdit::Berm {
                x_mm,
                z_mm,
                radius_mm: 2_500,
                height_mm: MAX_DEPTH_MM,
            },
            &mut walk,
            &mut reference,
            &format!("{what} clamp berm"),
        );
        assert_segments(&geom, &walk, what);
        assert_supports_match(&geom, &walk, &reference, what);
    }
}

/// A clone keeps the blocks and the ranges it had. Editing one state must not move the other,
/// including a query that uses the ranges.
#[test]
fn shared_blocks_stay_put_when_a_sibling_is_edited() {
    let geom = nested(131);
    let mut original = geom.new_state();
    let (x_mm, z_mm) = corner_mm(&geom, 64, 64);
    original
        .apply_ground_edit(
            &geom,
            &GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm: 4_000,
                depth_mm: 8_000,
            },
        )
        .unwrap();
    let mut before = Vec::new();
    original.encode(&mut before);
    let hits: Vec<_> = rays_of(&geom, &original)
        .into_iter()
        .map(|(from, to)| geom.segment(&original, from, to, FaceMask::GROUND))
        .collect();
    let sibling = original.clone();
    let (far_x, far_z) = corner_mm(&geom, 8, 120);
    original
        .apply_ground_edit(
            &geom,
            &GroundEdit::Crater {
                x_mm: far_x,
                z_mm: far_z,
                radius_mm: 3_000,
                depth_mm: 12_000,
            },
        )
        .unwrap();
    let mut sibling_bytes = Vec::new();
    sibling.encode(&mut sibling_bytes);
    assert_eq!(
        sibling_bytes, before,
        "editing the original rewrote the sibling's blocks"
    );
    let sibling_hits: Vec<_> = rays_of(&geom, &sibling)
        .into_iter()
        .map(|(from, to)| geom.segment(&sibling, from, to, FaceMask::GROUND))
        .collect();
    for (k, (a, b)) in hits.iter().zip(&sibling_hits).enumerate() {
        assert_same_hit(
            *a,
            *b,
            &format!("sibling ray {k} after the original was edited again"),
        );
    }
    // The other direction: a write on the sibling leaves the original's snapshot alone.
    let mut original_bytes = Vec::new();
    original.encode(&mut original_bytes);
    let mut sibling = sibling;
    sibling
        .apply_ground_edit(
            &geom,
            &GroundEdit::Berm {
                x_mm,
                z_mm,
                radius_mm: 1_500,
                height_mm: 2_000,
            },
        )
        .unwrap();
    let mut after = Vec::new();
    original.encode(&mut after);
    assert_eq!(
        after, original_bytes,
        "editing the sibling rewrote the original's blocks"
    );
    assert_segments(&geom, &original, "original after the sibling's berm");
    assert_segments(&geom, &sibling, "sibling after its own berm");
}

/// Depth at the clamp, a map near the extent limit, and a lattice whose vertices coincide.
#[test]
fn steep_wide_and_plateau_segments_match_the_full_walk() {
    let mut fp = 0u64;
    let plain = full(65, 20.0, 0.25);
    // The same rays on cooked ground, before any edit: a mismatch here is the pyramid walk
    // itself, not the edited ranges.
    assert_segments(&plain, &plain.new_state(), "fresh 20 m");
    let (x_mm, z_mm) = corner_mm(&plain, 32, 32);
    let mut state = plain.new_state();
    state
        .apply_ground_edit(
            &plain,
            &GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm: 6_000,
                depth_mm: MAX_DEPTH_MM,
            },
        )
        .unwrap();
    mix(&mut fp, assert_segments(&plain, &state, "steep 6 m"));
    let mut bytes = Vec::new();
    state.encode(&mut bytes);
    let restored = GeomState::decode(&plain, &bytes).unwrap();
    assert_eq!(restored.state_hash(), state.state_hash());
    assert!(restored.keeps_ground_ranges());
    mix(&mut fp, assert_segments(&plain, &restored, "steep restored"));

    // Half-width 8 km: under the 2^24 mm extent, and coarse enough that one cell is 500 m.
    let wide = full(33, 16_000.0, 100.0);
    let mut state = wide.new_state();
    state
        .apply_ground_edit(
            &wide,
            &GroundEdit::Crater {
                x_mm: 0,
                z_mm: 0,
                radius_mm: 600_000,
                depth_mm: 400_000,
            },
        )
        .unwrap();
    let (cx, cz) = corner_mm(&wide, 0, 32);
    state
        .apply_ground_edit(
            &wide,
            &GroundEdit::Berm {
                x_mm: cx,
                z_mm: cz,
                radius_mm: 700_000,
                height_mm: 250_000,
            },
        )
        .unwrap();
    mix(&mut fp, assert_segments(&wide, &state, "wide"));
    let mut bytes = Vec::new();
    state.encode(&mut bytes);
    let restored = GeomState::decode(&wide, &bytes).unwrap();
    mix(&mut fp, assert_segments(&wide, &restored, "wide restored"));

    // Two millimetres across 64 cells: many vertices share one coordinate, so ground triangles
    // are degenerate and the disc's squared offsets plateau.
    let plateau = full(65, 0.002, 0.0);
    let mut state = plateau.new_state();
    state
        .apply_ground_edit(
            &plateau,
            &GroundEdit::Crater {
                x_mm: 0,
                z_mm: 0,
                radius_mm: 5,
                depth_mm: 20_000,
            },
        )
        .unwrap();
    let reference = {
        let mut reference = plateau.new_state();
        reference
            .apply_ground_edit_reference(
                &plateau,
                &GroundEdit::Crater {
                    x_mm: 0,
                    z_mm: 0,
                    radius_mm: 5,
                    depth_mm: 20_000,
                },
                &mut EditStats::default(),
            )
            .unwrap();
        reference
    };
    assert_eq!(state.state_hash(), reference.state_hash(), "plateau hash");
    assert_supports_match(&plateau, &state, &reference, "plateau");
    mix(&mut fp, assert_segments(&plateau, &state, "plateau"));
    println!("ADVERSARIAL-FINGERPRINT {fp:016x}");
}

#[test]
fn degenerate_and_coincident_roofs_match_and_do_not_panic() {
    let geom = roofed();
    let (mut walk, mut reference) = (geom.new_state(), geom.new_state());
    let edits = [
        GroundEdit::Crater {
            x_mm: 0,
            z_mm: 0,
            radius_mm: 3_000,
            depth_mm: 50_000,
        },
        GroundEdit::Berm {
            x_mm: 400,
            z_mm: -200,
            radius_mm: 2_000,
            height_mm: 30_000,
        },
        GroundEdit::Crater {
            x_mm: 313,
            z_mm: 0,
            radius_mm: 1_500,
            depth_mm: MAX_DEPTH_MM,
        },
        GroundEdit::Rect {
            col: 30,
            row: 30,
            width: 8,
            height: 8,
            heights_mm: vec![250; 64],
        },
    ];
    for (k, edit) in edits.iter().enumerate() {
        let _ = apply_both(&geom, edit, &mut walk, &mut reference, &format!("roofed {k}"));
    }
    assert_segments(&geom, &walk, "roofed");
    assert_supports_match(&geom, &walk, &reference, "roofed");
    let mut bytes = Vec::new();
    walk.encode(&mut bytes);
    let restored = GeomState::decode(&geom, &bytes).unwrap();
    assert_segments(&geom, &restored, "roofed restored");
    for (k, (from, to)) in rays_of(&geom, &walk).into_iter().enumerate() {
        assert_same_hit(
            geom.segment(&walk, from, to, FaceMask::SOLID),
            geom.segment(&reference, from, to, FaceMask::SOLID),
            &format!("roofed states ray {k}"),
        );
    }
}

#[test]
fn extreme_centres_are_refused_or_match() {
    let geom = full(65, 20.0, 0.0);
    let (first, last) = corner_mm(&geom, 0, 64);
    let centres = [
        (i32::MAX, 0, MAX_RADIUS_MM, MAX_DEPTH_MM),
        (i32::MIN, i32::MIN, 1, 0),
        (i32::MAX, i32::MIN, MAX_RADIUS_MM, 1),
        (0, 0, 0, 100),
        (0, 0, -8, 100),
        (0, 0, 1, -1),
        (0, 0, MAX_RADIUS_MM, MAX_DEPTH_MM.saturating_add(1)),
        (first, last, 1, MAX_DEPTH_MM),
        (1_000_000, -1_000_000, MAX_RADIUS_MM, 10_000),
        (-1_000_000, 1_000_000, MAX_RADIUS_MM, 10_000),
        (last, first, MAX_RADIUS_MM, 1),
    ];
    for (k, (x_mm, z_mm, radius_mm, depth_mm)) in centres.into_iter().enumerate() {
        let edit = GroundEdit::Crater {
            x_mm,
            z_mm,
            radius_mm,
            depth_mm,
        };
        let (mut walk, mut reference) = (geom.new_state(), geom.new_state());
        let before = walk.state_hash();
        let result = apply_both(&geom, &edit, &mut walk, &mut reference, &format!("centre {k}"));
        if let Err(err) = result {
            assert!(
                matches!(
                    err,
                    GeomEditError::OutOfMap | GeomEditError::OutOfRange | GeomEditError::TooLarge
                ),
                "centre {k}: unexpected {err:?}"
            );
            assert_eq!(walk.state_hash(), before, "centre {k} wrote on error");
            assert_eq!(
                reference.state_hash(),
                before,
                "centre {k} reference wrote on error"
            );
        }
    }
}

/// Without a roof, the same integer profile added back restores every vertex millimetre. The
/// override block may still hold that height; the two walks have to agree on which.
#[test]
fn unstopped_berm_restores_vertex_millimetres() {
    let geom = full(65, 20.0, 2.0);
    let fresh = geom.new_state();
    let (mut walk, mut reference) = (geom.new_state(), geom.new_state());
    let (x_mm, z_mm) = corner_mm(&geom, 40, 20);
    let crater = GroundEdit::Crater {
        x_mm,
        z_mm,
        radius_mm: 4_000,
        depth_mm: 800,
    };
    let berm = GroundEdit::Berm {
        x_mm,
        z_mm,
        radius_mm: 4_000,
        height_mm: 800,
    };
    let _ = apply_both(&geom, &crater, &mut walk, &mut reference, "undo crater");
    let _ = apply_both(&geom, &berm, &mut walk, &mut reference, "undo berm");
    let n = geom.cooked().lattice.lattice_n;
    for v in 0..n * n {
        assert_eq!(
            walk.vertex_height_mm(&geom, v),
            fresh.vertex_height_mm(&geom, v),
            "vertex {v} after crater then berm"
        );
    }
    assert_segments(&geom, &walk, "restored profile");
    assert_supports_match(&geom, &walk, &reference, "restored profile");
}

/// A 2 mm lattice cooks near-vertical ground. After a crater at the origin the pyramid walk and
/// the single walk disagree on whether these rays hit, and on the height when both hit.
#[test]
fn degenerate_plateau_segment_matches_the_full_walk() {
    let geom = full(65, 0.002, 0.0);
    let rays = [
        // Horizontal, at the crater's highest vertex: the single walk hits the far corner and
        // the pyramid walk misses it. Unedited ground is below this ray, so both walks miss.
        ([-0.001_f32, -17.384, -0.001], [0.001, -17.384, 0.001]),
        // Straight down, 1 mm off the origin: only the pyramid walk hits.
        ([0.001, 32.615997, -0.001], [0.001, -69.0, -0.001]),
        // Straight down the origin: both hit, about 9 mm apart.
        ([0.0, 32.615997, 0.0], [0.0, -69.0, 0.0]),
        ([0.001, -16.384, 0.0], [0.001, -20.0, 0.0]),
    ];
    let fresh = geom.new_state();
    for (i, (from, to)) in rays.iter().enumerate() {
        let pyramid = geom.segment(&fresh, *from, *to, FaceMask::GROUND);
        let full = geom.segment_without_pyramid(&fresh, *from, *to, FaceMask::GROUND);
        println!("fresh {i}: pyramid {pyramid:?} full {full:?}");
    }
    let mut state = geom.new_state();
    state
        .apply_ground_edit(
            &geom,
            &GroundEdit::Crater {
                x_mm: 0,
                z_mm: 0,
                radius_mm: 5,
                depth_mm: 20_000,
            },
        )
        .unwrap();
    assert!(state.keeps_ground_ranges());
    for (i, (from, to)) in rays.iter().enumerate() {
        let pyramid = geom.segment(&state, *from, *to, FaceMask::GROUND);
        let full = geom.segment_without_pyramid(&state, *from, *to, FaceMask::GROUND);
        println!("edited {i}: pyramid {pyramid:?} full {full:?}");
        assert_eq!(
            pyramid.is_some(),
            full.is_some(),
            "ray {i} from {from:?} to {to:?}: pyramid {pyramid:?} full {full:?}"
        );
        if let (Some(pyramid), Some(full)) = (pyramid, full) {
            assert_eq!(
                pyramid.point.map(|v| v.to_bits()),
                full.point.map(|v| v.to_bits()),
                "ray {i} point"
            );
            assert_eq!(pyramid.t.to_bits(), full.t.to_bits(), "ray {i} t");
        }
    }
}

/// A crater in the far corner turns the height pyramid on for the whole state. This ray stays on
/// cooked ground at the west edge, clear of that crater. Before the edited-range pyramid, an
/// edited state took the single ground walk, so `segment` matched `segment_without_pyramid` here
/// up to pruning, and pruning cannot return a smaller `t`. The pyramid walk does.
#[test]
fn edited_ground_segment_t_matches_the_full_walk() {
    let geom = full(65, 20.0, 0.25);
    let mut state = geom.new_state();
    let (x_mm, z_mm) = corner_mm(&geom, 64, 64);
    state
        .apply_ground_edit(
            &geom,
            &GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm: 400,
                depth_mm: 500,
            },
        )
        .unwrap();
    assert!(state.keeps_ground_ranges());
    let from = [-9.687_f32, 2.266, 0.0];
    let to = [-9.375, 0.25, 0.0];
    let fast = geom.segment(&state, from, to, FaceMask::GROUND).unwrap();
    let full = geom
        .segment_without_pyramid(&state, from, to, FaceMask::GROUND)
        .unwrap();
    assert_eq!(fast.kind, full.kind);
    assert_eq!(fast.tri, full.tri);
    assert_eq!(
        fast.point.map(|v| v.to_bits()),
        full.point.map(|v| v.to_bits()),
        "hit position"
    );
    assert_eq!(
        fast.t.to_bits(),
        full.t.to_bits(),
        "edited ground segment t, from {from:?} to {to:?}: pyramid {fast:?} full {full:?}"
    );
}
