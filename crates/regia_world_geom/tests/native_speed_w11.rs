// SPDX-License-Identifier: MPL-2.0
//! Plan row W-11 (native query speed, ADR-8815): the segment tables, the any-hit sight line and the
//! near-wall disc push answer bit for bit as the walks they replace, on the cooked Switchbacks map.
//! The times are `benches/mixed_calls.rs`; these tests need no clock.
#![cfg(all(feature = "stage_b", feature = "stats"))]
#![allow(clippy::unwrap_used)]
pub mod mixed_calls;
pub mod native_mouths;
use mixed_calls::MixedTick;
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::SampledLayout;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::{Band, BlockerId, FaceMask, GeomState, Push, SegmentHit, SurfaceKind, WorldGeom};

fn xorshift(seed: &mut u32) -> f32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    (*seed >> 8) as f32 / (1u32 << 24) as f32
}

/// The Switchbacks fixture on the canyon floor with two collapsible runs (a gallery run and a tube
/// run), as `tests/blockers.rs` cooks it, and its sampled layout.
fn plug_world() -> (WorldGeom, SampledLayout) {
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384);
    let ground = GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    };
    let layout = for_map("switchbacks").unwrap();
    let sampled = layout.sample(Some(&terrain));
    let passages = PassagesDocument::from_layout(layout).to_ron().unwrap();
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"), blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0), (passage: \"tube\", from_s_m: 200.0, to_s_m: 230.0)])])";
    let read = |p: &str| {
        if p == "map.passages.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{p}: not found"))
        }
    };
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // ADR-8359: a test cook waives the two known Switchbacks ramp facets by hash and recooks.
    let out = match run(doc) {
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            run(&waive_ramp_facets(doc, &report).unwrap())
        }
        other => other,
    }
    .unwrap();
    (
        WorldGeom::from_cooked(&out.file.encode().unwrap()).unwrap(),
        sampled,
    )
}

/// Every plug of the map set active.
fn all_plugs_active(geom: &WorldGeom) -> GeomState {
    let mut state = geom.new_state();
    for i in 0..geom.blockers().len() {
        state.set_blocker(geom, BlockerId(i as u16), true).unwrap();
    }
    state
}

fn tube_points(sampled: &SampledLayout) -> Vec<([f32; 3], [f32; 2], f32)> {
    sampled
        .passages
        .iter()
        .flat_map(|p| p.samples.iter().map(move |s| (s.p, s.left, p.width_m)))
        .collect()
}

/// ADR-8815: `line_of_sight` stops at the first hit of any kind; its answer is that of the
/// closest-hit reference walk (`segment_without_pyramid(..).is_none()`) on the mixed tick's sight
/// lines and shells and on seeded lines in the tubes, to the surface and along the ground. (The
/// closest-hit `segment` is compared with that walk by W-31's `early_out_segments_equal_the_full_walk`.)
#[test]
fn sight_lines_equal_the_closest_hit_walk() {
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let ground = native_mouths::acceptance_ground_m("switchbacks");
    let tick = MixedTick::new(&sampled, ground, 640.0);
    let tube = tube_points(&sampled);
    let mut rays: Vec<([f32; 3], [f32; 3])> = tick.los.clone();
    rays.extend(tick.segments.iter().copied());
    let mut seed = 0x51ed_270bu32;
    for i in 0..1200 {
        let (a, _, _) = tube[(xorshift(&mut seed) * tube.len() as f32) as usize % tube.len()];
        let from = [a[0], a[1] + 0.2 + xorshift(&mut seed) * 3.0, a[2]];
        let to = match i % 4 {
            0 => {
                let (b, _, _) = tube[(xorshift(&mut seed) * tube.len() as f32) as usize % tube.len()];
                [b[0], b[1] + xorshift(&mut seed) * 3.0, b[2]]
            }
            1 => [
                from[0] + (xorshift(&mut seed) - 0.5) * 1200.0,
                ground + xorshift(&mut seed) * 30.0,
                from[2] + (xorshift(&mut seed) - 0.5) * 1200.0,
            ],
            2 => [
                from[0] + (xorshift(&mut seed) - 0.5) * 30.0,
                from[1] + (xorshift(&mut seed) - 0.5) * 4.0,
                from[2] + (xorshift(&mut seed) - 0.5) * 30.0,
            ],
            _ => [
                (xorshift(&mut seed) - 0.5) * 1200.0,
                ground + 0.5 + xorshift(&mut seed) * 3.0,
                (xorshift(&mut seed) - 0.5) * 1200.0,
            ],
        };
        // Odd rounds start outdoors, above the ground.
        let from = if i % 8 == 7 {
            [
                (xorshift(&mut seed) - 0.5) * 1200.0,
                ground + 0.5 + xorshift(&mut seed) * 3.0,
                (xorshift(&mut seed) - 0.5) * 1200.0,
            ]
        } else {
            from
        };
        rays.push((from, to));
    }
    let state = geom.new_state();
    let (mut blocked, mut clear) = (0usize, 0usize);
    for (k, &(from, to)) in rays.iter().enumerate() {
        let full = geom.segment_without_pyramid(&state, from, to, FaceMask::SOLID);
        assert_eq!(
            geom.line_of_sight(&state, from, to),
            full.is_none(),
            "ray {k} {from:?} -> {to:?}"
        );
        if full.is_some() {
            blocked += 1;
        } else {
            clear += 1;
        }
    }
    eprintln!("W11_LOS rays={} blocked={blocked} clear={clear}", rays.len());
    assert!(
        blocked > rays.len() / 2 && clear > 100,
        "blocked {blocked} clear {clear}"
    );
}

/// ADR-8815: `push_disc` applies only the walls near the disc at their turn; the result (center
/// bits, contact count, last wall) is that of applying every wall of the neighborhood in canonical
/// order (`push_disc_in_full_order`: a scan of every triangle, no index), along every tube at
/// offsets from the centerline to past the walls, for every hull radius from the 5 cm floor to the
/// 8 m pad, with narrow and wide bands.
#[test]
fn near_wall_push_equals_the_full_order() {
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let tube = tube_points(&sampled);
    let bits = |p: Push| (p.xz.map(f32::to_bits), p.contacts, p.last);
    let mut seed = 0x0bad_5eedu32;
    let (mut calls, mut moved, mut multi) = (0usize, 0usize, 0usize);
    {
        let (name, state) = ("clear", geom.new_state());
        // Every third sample keeps the reference scan (all triangles per call) short.
        for (k, &(p, left, width)) in tube.iter().enumerate().filter(|(k, _)| k % 3 == 0) {
            for radius in [0.05f32, 0.6, 2.5, 3.0, 4.5, 8.0, 20.0] {
                let off = (xorshift(&mut seed) - 0.5) * width * 1.3;
                let (x, z) = (p[0] + left[0] * off, p[2] + left[1] * off);
                let band = match k % 4 {
                    0 => Band {
                        lo: p[1] + 0.5,
                        hi: p[1] + 3.0,
                    },
                    1 => Band {
                        lo: p[1] + 1.0,
                        hi: p[1] + 1.0,
                    },
                    2 => Band {
                        lo: p[1] - 50.0,
                        hi: p[1] + 50.0,
                    },
                    _ => Band {
                        lo: p[1] + 0.2,
                        hi: p[1] + 8.0,
                    },
                };
                let fast = geom.push_disc(&state, x, z, radius, band);
                let full = geom.push_disc_in_full_order(&state, x, z, radius, band);
                assert_eq!(
                    bits(fast),
                    bits(full),
                    "{name}: sample {k} ({x}, {z}) radius {radius} band {band:?}"
                );
                calls += 1;
                moved += usize::from(full.contacts > 0);
                multi += usize::from(full.contacts > 1);
            }
        }
    }
    eprintln!("W11_PUSH calls={calls} moved={moved} multi={multi}");
    assert!(
        moved > calls / 8 && multi > 20,
        "calls {calls} moved {moved} multi {multi}"
    );
}

/// ADR-8815: the segment tables are counted in the resident bytes (the query-index part), and the
/// whole stays within spec 121 § 11's sim-geometry budget.
#[test]
fn segment_tables_are_counted_and_within_budget() {
    let (_, geom, _, _) = native_mouths::world("switchbacks");
    let [lattice, triangles, _, index, pyramid, _] = geom.resident_parts();
    let resident = geom.resident_bytes();
    eprintln!(
        "W11_RESIDENT resident={resident} lattice={lattice} triangles={triangles} index={index} pyramid={pyramid}"
    );
    eprintln!("W11_TABLES switchbacks {:?}", geom.segment_table_parts());
    for map in ["cinder_steps", "atacama_rail"] {
        let (output, other, _, _) = native_mouths::world(map);
        eprintln!(
            "W11_TABLES {map} {:?} resident={} estimate={} tris={}",
            other.segment_table_parts(),
            other.resident_bytes(),
            output.report.ground.sim_bytes,
            other.triangles().len()
        );
    }
    // 32 bytes of bounds, 4 of id and 1 of kind per triangle at least.
    assert!(
        index >= geom.triangles().len() as u64 * 37,
        "the segment tables are not counted"
    );
    assert!(
        resident <= regia_world_geom::checks::GROUND_BUDGET_SMALL,
        "{resident} B"
    );
}

/// ADR-8815 with active blocker plugs (spec 121 § 5.3 rule 6): on a Switchbacks cook with two
/// collapsible runs, every plug active, the closest hit, the sight line and the disc push are
/// those of the reference walks, and plug faces are among the hits and the contacts. With every
/// plug clear no plug face is hit.
#[test]
fn plug_faces_answer_as_the_reference_walks() {
    let (geom, sampled) = plug_world();
    assert_eq!(geom.blockers().len(), 2, "the cook has two plugs");
    let tube = tube_points(&sampled);
    let all = geom.triangles();
    let bits = |h: Option<SegmentHit>| h.map(|h| (h.t.to_bits(), h.point.map(f32::to_bits), h.kind, h.tri));
    let push_bits = |p: Push| (p.xz.map(f32::to_bits), p.contacts, p.last);
    let mut seed = 0x7f4a_7c15u32;
    for (name, state, want_plugs) in [
        ("clear", geom.new_state(), false),
        ("plugs", all_plugs_active(&geom), true),
    ] {
        let (mut plug_hits, mut plug_pushes, mut calls) = (0usize, 0usize, 0usize);
        for (k, &(p, left, width)) in tube.iter().enumerate() {
            // A line from this sample to one a few samples on, at chest height: through a plug where one stands.
            let (q, _, _) = tube[(k + 1 + (xorshift(&mut seed) * 12.0) as usize).min(tube.len() - 1)];
            let from = [p[0], p[1] + 0.5 + xorshift(&mut seed) * 2.0, p[2]];
            let to = [q[0], q[1] + 0.5 + xorshift(&mut seed) * 2.0, q[2]];
            for mask in [FaceMask::SOLID, FaceMask::NO_FLOORS, FaceMask::BLOCKER] {
                let fast = geom.segment(&state, from, to, mask);
                let full = geom.segment_without_pyramid(&state, from, to, mask);
                assert_eq!(
                    bits(fast),
                    bits(full),
                    "{name}: sample {k} {from:?} -> {to:?} mask {mask:?}"
                );
                plug_hits += usize::from(full.is_some_and(|h| h.kind == SurfaceKind::Blocker));
            }
            let full = geom.segment_without_pyramid(&state, from, to, FaceMask::SOLID);
            assert_eq!(
                geom.line_of_sight(&state, from, to),
                full.is_none(),
                "{name}: sample {k}"
            );
            if k % 2 == 0 {
                let off = (xorshift(&mut seed) - 0.5) * width;
                let (x, z) = (p[0] + left[0] * off, p[2] + left[1] * off);
                let band = Band {
                    lo: p[1] + 0.5,
                    hi: p[1] + 3.0,
                };
                for radius in [0.6f32, 3.0, 8.0] {
                    let fast = geom.push_disc(&state, x, z, radius, band);
                    let full = geom.push_disc_in_full_order(&state, x, z, radius, band);
                    assert_eq!(
                        push_bits(fast),
                        push_bits(full),
                        "{name}: sample {k} ({x}, {z}) radius {radius}"
                    );
                    calls += 1;
                    if let Some(last) = full.last {
                        let kind = all.iter().find(|t| t.tri == last).map(|t| t.kind);
                        plug_pushes += usize::from(kind == Some(regia_world_geom::cooked::TriKind::Blocker));
                    }
                }
            }
        }
        eprintln!("W11_PLUGS {name} plug_hits={plug_hits} plug_pushes={plug_pushes} push_calls={calls}");
        assert_eq!(plug_hits > 0, want_plugs, "{name}: plug hits {plug_hits}");
        assert!(want_plugs || plug_pushes == 0, "{name}: a clear plug pushed");
    }
}

/// ADR-8815 on the cooked map with D1 edits: after craters and a berm on the tube's ground and
/// outdoors, the closest hit and the sight line are those of the reference walk, on the mixed
/// tick's shells and sight lines and on lines aimed at the edits.
#[test]
fn edited_map_answers_as_the_reference_walk() {
    use regia_world_geom::{GroundEdit, m_to_mm};
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let ground = native_mouths::acceptance_ground_m("switchbacks");
    let tick = MixedTick::new(&sampled, ground, 640.0);
    let mut state = geom.new_state();
    let mut centers = Vec::new();
    for (k, &[x, _, z]) in tick.floors.iter().enumerate().filter(|(k, _)| k % 23 == 0) {
        let edit = if k % 2 == 0 {
            GroundEdit::Crater {
                x_mm: m_to_mm(x),
                z_mm: m_to_mm(z),
                radius_mm: 6_000,
                depth_mm: 450,
            }
        } else {
            GroundEdit::Berm {
                x_mm: m_to_mm(x),
                z_mm: m_to_mm(z),
                radius_mm: 4_000,
                height_mm: 1_500,
            }
        };
        if state.apply_ground_edit(&geom, &edit).is_ok() {
            centers.push([x, z]);
        }
    }
    assert!(centers.len() >= 6, "edits applied: {}", centers.len());
    let mut rays: Vec<([f32; 3], [f32; 3])> = tick.segments.clone();
    rays.extend(tick.los.iter().copied());
    let mut seed = 0x1f12_3bb5u32;
    for i in 0..600 {
        let c = centers[i % centers.len()];
        let to = [
            c[0] + (xorshift(&mut seed) - 0.5) * 10.0,
            ground - 3.0,
            c[1] + (xorshift(&mut seed) - 0.5) * 10.0,
        ];
        let from = if i % 2 == 0 {
            [
                to[0] + (xorshift(&mut seed) - 0.5) * 40.0,
                ground + 30.0 * xorshift(&mut seed),
                to[2] + (xorshift(&mut seed) - 0.5) * 40.0,
            ]
        } else {
            [
                c[0] - 30.0,
                ground + 0.3 + xorshift(&mut seed) * 2.0,
                c[1] + (xorshift(&mut seed) - 0.5) * 8.0,
            ]
        };
        let to = if i % 2 == 0 {
            to
        } else {
            [c[0] + 30.0, ground + 0.3 + xorshift(&mut seed) * 2.0, to[2]]
        };
        rays.push((from, to));
    }
    let bits = |h: Option<SegmentHit>| h.map(|h| (h.t.to_bits(), h.point.map(f32::to_bits), h.kind, h.tri));
    let mut ground_hits = 0usize;
    for (k, &(from, to)) in rays.iter().enumerate() {
        for mask in [FaceMask::SOLID, FaceMask::GROUND] {
            let full = geom.segment_without_pyramid(&state, from, to, mask);
            assert_eq!(
                bits(geom.segment(&state, from, to, mask)),
                bits(full),
                "ray {k} {from:?} -> {to:?} mask {mask:?}"
            );
            ground_hits += usize::from(full.is_some_and(|h| h.kind == SurfaceKind::Ground));
        }
        let full = geom.segment_without_pyramid(&state, from, to, FaceMask::SOLID);
        assert_eq!(
            geom.line_of_sight(&state, from, to),
            full.is_none(),
            "ray {k} {from:?} -> {to:?}"
        );
    }
    eprintln!(
        "W11_EDITED rays={} edits={} ground_hits={ground_hits}",
        rays.len(),
        centers.len()
    );
    assert!(ground_hits > 300, "ground hits {ground_hits}");
}

/// ADR-8815: `clamp_under_ceiling_contact` skips its column query when the segment grid shows no
/// roof within reach of the body. Its result (height bits and contact) is that of the clamp
/// computed from `support` directly, along every tube from under the floor to above the roof, for
/// small and large half heights (a negative one too), floors below, at and above the body, with
/// plugs clear and active.
#[test]
fn lid_clamp_equals_the_clamp_from_support() {
    use regia_detsim::float::{det_max, det_min};
    let (_, geom, _, sampled) = native_mouths::world("switchbacks");
    let (plug_geom, plug_sampled) = plug_world();
    let mut seed = 0x243f_6a88u32;
    for (name, geom, state, sampled) in [
        ("clear", &geom, geom.new_state(), &sampled),
        ("plugs", &plug_geom, all_plugs_active(&plug_geom), &plug_sampled),
        ("plugs clear", &plug_geom, plug_geom.new_state(), &plug_sampled),
    ] {
        let tube = tube_points(sampled);
        let (mut calls, mut moved, mut skipped_possible) = (0usize, 0usize, 0usize);
        for (k, &(p, left, width)) in tube.iter().enumerate() {
            let off = (xorshift(&mut seed) - 0.5) * width * 0.9;
            let (x, z) = (p[0] + left[0] * off, p[2] + left[1] * off);
            for step in 0..14 {
                // From half a metre under the floor to about 8 m over it, past the roof.
                let y = p[1] - 0.5 + step as f32 * 0.65 + xorshift(&mut seed) * 0.3;
                for half_h in [0.3f32, 1.0, 2.5, 6.0, -1.0] {
                    let floor_y = match (k + step) % 4 {
                        0 => p[1],
                        1 => y,
                        2 => y + 0.4,
                        _ => p[1] - 3.0,
                    };
                    let got = geom.clamp_under_ceiling_contact(&state, x, y, z, floor_y, half_h);
                    // The clamp as spec 121 § 5.4 states it, from the column query.
                    let mut want_y = y;
                    let mut want_contact = None;
                    if let Some(lid) = geom.support(&state, x, z, y).ceiling
                        && y <= lid.y
                    {
                        want_y = det_max(det_min(y, lid.y - half_h), floor_y);
                        if want_y != y {
                            want_contact = Some(lid);
                        }
                    }
                    assert_eq!(
                        (got.y.to_bits(), got.contact),
                        (want_y.to_bits(), want_contact),
                        "{name}: sample {k} ({x}, {z}) y {y} floor {floor_y} half {half_h}"
                    );
                    assert_eq!(
                        geom.clamp_under_ceiling(&state, x, y, z, floor_y, half_h)
                            .to_bits(),
                        want_y.to_bits()
                    );
                    calls += 1;
                    moved += usize::from(want_contact.is_some());
                    skipped_possible += usize::from(want_contact.is_none() && y > floor_y);
                }
            }
        }
        eprintln!("W11_CLAMP {name} calls={calls} moved={moved} unmoved_above_floor={skipped_possible}");
        assert!(
            moved > calls / 20 && skipped_possible > calls / 4,
            "{name}: calls {calls} moved {moved}"
        );
    }
}

/// ADR-8815 on the other two cooked fixtures (The Cinder Steps: stacked decks and a junction;
/// Atacama Rail): the closest hit, the sight line, the disc push and the lid clamp are those of
/// the reference walks there too, on lines between tube samples, to the surface and along the
/// ground, and on hulls across every passage.
#[test]
fn other_maps_answer_as_the_reference_walks() {
    use regia_detsim::float::{det_max, det_min};
    let bits = |h: Option<SegmentHit>| h.map(|h| (h.t.to_bits(), h.point.map(f32::to_bits), h.kind, h.tri));
    let push_bits = |p: Push| (p.xz.map(f32::to_bits), p.contacts, p.last);
    for map in ["cinder_steps", "atacama_rail"] {
        let (_, geom, _, sampled) = native_mouths::world(map);
        let ground = native_mouths::acceptance_ground_m(map);
        let state = geom.new_state();
        let tube = tube_points(&sampled);
        let mut seed = 0x85eb_ca6bu32 ^ map.len() as u32;
        let (mut hits, mut clear, mut moved, mut clamped) = (0usize, 0usize, 0usize, 0usize);
        for i in 0..520 {
            let (a, left, width) = tube[(xorshift(&mut seed) * tube.len() as f32) as usize % tube.len()];
            let from = [a[0], a[1] + 0.2 + xorshift(&mut seed) * 3.0, a[2]];
            let to = match i % 4 {
                0 => {
                    let (b, _, _) = tube[(xorshift(&mut seed) * tube.len() as f32) as usize % tube.len()];
                    [b[0], b[1] + xorshift(&mut seed) * 3.0, b[2]]
                }
                1 => [
                    from[0] + (xorshift(&mut seed) - 0.5) * 900.0,
                    ground + xorshift(&mut seed) * 30.0,
                    from[2] + (xorshift(&mut seed) - 0.5) * 900.0,
                ],
                2 => [
                    from[0] + (xorshift(&mut seed) - 0.5) * 30.0,
                    from[1] + (xorshift(&mut seed) - 0.5) * 12.0,
                    from[2] + (xorshift(&mut seed) - 0.5) * 30.0,
                ],
                _ => [
                    (xorshift(&mut seed) - 0.5) * 1000.0,
                    ground + 0.5 + xorshift(&mut seed) * 3.0,
                    (xorshift(&mut seed) - 0.5) * 1000.0,
                ],
            };
            for mask in [FaceMask::SOLID, FaceMask::NO_FLOORS] {
                assert_eq!(
                    bits(geom.segment(&state, from, to, mask)),
                    bits(geom.segment_without_pyramid(&state, from, to, mask)),
                    "{map}: ray {i} {from:?} -> {to:?} mask {mask:?}"
                );
            }
            let full = geom.segment_without_pyramid(&state, from, to, FaceMask::SOLID);
            assert_eq!(
                geom.line_of_sight(&state, from, to),
                full.is_none(),
                "{map}: ray {i}"
            );
            if full.is_some() {
                hits += 1;
            } else {
                clear += 1;
            }
            // A hull at this sample, off the centerline up to past the walls.
            let off = (xorshift(&mut seed) - 0.5) * width * 1.2;
            let (x, z) = (a[0] + left[0] * off, a[2] + left[1] * off);
            let band = Band {
                lo: a[1] + 0.5,
                hi: a[1] + 3.0,
            };
            for radius in [0.6f32, 3.0, 4.5] {
                let fast = geom.push_disc(&state, x, z, radius, band);
                let reference = geom.push_disc_in_full_order(&state, x, z, radius, band);
                assert_eq!(
                    push_bits(fast),
                    push_bits(reference),
                    "{map}: hull {i} ({x}, {z}) radius {radius}"
                );
                moved += usize::from(reference.contacts > 0);
            }
            for (y, half_h) in [
                (a[1] + 1.0, 1.0f32),
                // The lids of these maps are 10 m and more over the floor: this body reaches them.
                (a[1] + 2.0 + xorshift(&mut seed) * 14.0, 2.5),
            ] {
                let got = geom.clamp_under_ceiling_contact(&state, x, y, z, a[1], half_h);
                let mut want = (y, None);
                if let Some(lid) = geom.support(&state, x, z, y).ceiling
                    && y <= lid.y
                {
                    let clamped_y = det_max(det_min(y, lid.y - half_h), a[1]);
                    want = (clamped_y, (clamped_y != y).then_some(lid));
                }
                assert_eq!(
                    (got.y.to_bits(), got.contact),
                    (want.0.to_bits(), want.1),
                    "{map}: hull {i} y {y}"
                );
                clamped += usize::from(want.1.is_some());
            }
        }
        // The clamp count is printed, not required: these maps' lids sit high over their floors
        // (the contacts are pinned on Switchbacks by `lid_clamp_equals_the_clamp_from_support`).
        eprintln!("W11_OTHER {map} hits={hits} clear={clear} pushes_moved={moved} clamps_moved={clamped}");
        assert!(
            hits > 200 && clear > 20 && moved > 50,
            "{map}: hits {hits} clear {clear} moved {moved}"
        );
    }
}
