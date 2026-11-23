// SPDX-License-Identifier: MPL-2.0
//! Plan row W-21 (spec 121 § 5.6, spec 25 § 7.7, ADR-8371): the cook writes `<map>.viscells.ron`
//! from the sections. Cell 0 is outdoors; each section is the next cell. Mouth rings open onto
//! cell 0, and each junction cut is one portal. A 0.5 m wall gap is `viscell_leak`.

#![allow(clippy::unwrap_used)]

use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::{Sample, SampledLayout, TunnelLayout};
use regia_world_geom::cook::{CookError, CookOutput, reject_viscell_gap};
use regia_world_geom::cooked::{CookedZoneShape, TriKind};
use regia_world_geom::m_to_mm;
use regia_world_geom::sections::open_skylight_shafts;
use regia_world_geom::viscells::{GapSection, GapTri, emit, sealed_flood, wall_gap_of};

/// The canyon cook from `passage_sections` (flat ground at -256 m, skylight shafts opened).
fn cook_doc(map: &str) -> CookOutput {
    let passages = regia_world_geom::passages::PassagesDocument::from_layout(for_map(map).unwrap())
        .to_ron()
        .unwrap();
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"))])";
    let read = |p: &str| {
        if p == "map.passages.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{p}: not found"))
        }
    };
    let terrain = regia_world_geom::compat::sampler::TerrainSampler::new(
        regia_world_geom::compat::sampler::HeightGrid::square(4, vec![8192; 16]).unwrap(),
        1280.0,
    )
    .with_zero_counts(16384);
    let ground = regia_world_geom::cook::GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    };
    let run = |document: &str| {
        regia_world_geom::cook::cook(&regia_world_geom::cook::CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // The Switchbacks test cook authors ADR-8359's ramp-facet waiver, as the other test cooks do (ADR-8436).
    match run(doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if regia_world_geom::checks::only_ramp_facet_failures(&report) => {
            run(&regia_world_geom::checks::waive_ramp_facets(doc, &report).unwrap()).unwrap()
        }
        Err(e) => panic!("{e} ({})", e.code()),
    }
}

fn sampled(layout: &TunnelLayout) -> SampledLayout {
    let terrain = regia_world_geom::compat::sampler::TerrainSampler::new(
        regia_world_geom::compat::sampler::HeightGrid::square(4, vec![8192; 16]).unwrap(),
        1280.0,
    )
    .with_zero_counts(16384);
    let mut s = layout.sample(Some(&terrain));
    open_skylight_shafts(&mut s, &layout.openings);
    s
}

/// The sample nearest `at` (3D, ties to the lower index), matching the section cut.
fn nearest_to(samples: &[Sample], at: [f32; 3]) -> usize {
    let mut best = (f64::INFINITY, 0usize);
    for (i, q) in samples.iter().enumerate() {
        let d = [
            f64::from(q.p[0]) - f64::from(at[0]),
            f64::from(q.p[1]) - f64::from(at[1]),
            f64::from(q.p[2]) - f64::from(at[2]),
        ];
        let d2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
        if d2 < best.0 {
            best = (d2, i);
        }
    }
    best.1
}

/// With every portal closed, a flood across shared triangle edges stays inside one section.
#[test]
fn sealed_sections_separate_by_flood_fill() {
    let out = cook_doc("switchbacks");
    let emitted = emit(&out.file.canonical).unwrap();
    let seal: Vec<(u16, u16, [[f32; 3]; 4])> = emitted.portals.iter().map(|p| (p.a, p.b, p.quad_m)).collect();
    let crossings = sealed_flood(&out.file.canonical, &seal).unwrap();
    assert!(
        crossings > 0,
        "the sealed flood had no shared edge between sections to separate"
    );
    assert_eq!(crossings, emitted.sealed_crossings);
    assert_eq!(emitted.cells.len(), out.file.canonical.sections.len() + 1);
    assert!(emitted.cells[0].outdoor && emitted.cells[0].id == 0);
}

/// Every surface opening is a cooked mouth portal, and every quad of it joins that section to cell 0.
#[test]
fn every_mouth_is_a_portal_to_cell_0() {
    let layout = for_map("switchbacks").unwrap();
    assert!(!layout.openings.is_empty(), "Switchbacks has surface openings");
    let out = cook_doc("switchbacks");
    let canonical = &out.file.canonical;
    let emitted = emit(canonical).unwrap();
    for opening in &layout.openings {
        let mouths: Vec<_> = canonical
            .portals
            .iter()
            .filter(|p| p.mouth.as_deref() == Some(opening.id.as_str()))
            .collect();
        assert!(
            !mouths.is_empty(),
            "opening {} has no cooked mouth portal",
            opening.id
        );
        for portal in mouths {
            assert!(
                portal.sides.contains(&None),
                "mouth portal {} of {} does not open outdoors",
                portal.id,
                opening.id
            );
            let section = portal.sides.into_iter().flatten().next().unwrap();
            let cell = section + 1;
            let quads: Vec<_> = emitted.portals.iter().filter(|q| q.cooked == portal.id).collect();
            assert!(!quads.is_empty(), "mouth portal {} emitted no quad", portal.id);
            for quad in quads {
                let joins = (quad.a == 0 && quad.b == cell) || (quad.b == 0 && quad.a == cell);
                assert!(
                    joins,
                    "mouth {} quad joins cells {} and {}, not 0 and {cell}",
                    opening.id, quad.a, quad.b
                );
            }
        }
    }
}

/// Each interior junction cut is one cooked portal and one emitted quad, between two sections of that passage.
#[test]
fn every_junction_cut_is_one_portal() {
    let layout = for_map("switchbacks").unwrap();
    let sampled = sampled(&layout);
    let out = cook_doc("switchbacks");
    let canonical = &out.file.canonical;
    let emitted = emit(canonical).unwrap();
    let mut cuts = Vec::new();
    for passage in &sampled.passages {
        let n = passage.samples.len();
        for junction in layout
            .junctions
            .iter()
            .filter(|j| j.passages.iter().any(|id| id == &passage.id))
        {
            let i = nearest_to(&passage.samples, junction.at);
            if i > 0 && i + 1 < n {
                let nav = passage.samples[i].p.map(m_to_mm);
                cuts.push((passage.id.clone(), nav));
            }
        }
    }
    assert!(!cuts.is_empty(), "Switchbacks has an interior junction cut");
    for (passage, nav) in &cuts {
        let source = format!("tubes/{passage}");
        let same_passage = |side: Option<u16>| {
            side.is_some_and(|id| {
                canonical
                    .sections
                    .iter()
                    .any(|section| section.id == id && section.source == source)
            })
        };
        let matching: Vec<_> = canonical
            .portals
            .iter()
            .filter(|p| p.nav_mm == *nav && p.sides.into_iter().all(same_passage))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "junction cut of {source} at {nav:?} cooked {} portals",
            matching.len()
        );
        let quads: Vec<_> = emitted
            .portals
            .iter()
            .filter(|q| q.cooked == matching[0].id)
            .collect();
        assert_eq!(
            quads.len(),
            1,
            "junction cut of {source} emitted {} quads",
            quads.len()
        );
    }
    for portal in &canonical.portals {
        assert!(
            emitted.portals.iter().any(|q| q.cooked == portal.id),
            "cooked portal {} emitted no quad",
            portal.id
        );
    }
}

/// A sealed wall is not a leak. The same wall with a 0.5 m hole is `viscell_leak` and names the section.
#[test]
fn leak_check_fails_a_gap() {
    let (sealed, gapped) = wall_pair();
    assert!(wall_gap_of(&sealed).is_none(), "a closed wall is not a leak");
    assert!(reject_viscell_gap(&sealed).is_ok());
    let gap = wall_gap_of(&gapped).expect("the 0.5 m hole is a leak");
    assert!(gap.span_mm >= 500, "span {} mm", gap.span_mm);
    let err = reject_viscell_gap(&gapped).expect_err("cook refuses the gap");
    assert_eq!(err.code(), "viscell_leak");
    let text = err.to_string();
    assert!(text.contains("viscell_leak"), "{text}");
    assert!(text.contains("tubes/gapped"), "{text}");
    assert!(matches!(err, CookError::VisCellLeak { .. }));
}

/// Two cooks of the same fixture write the same `<map>.viscells.ron` bytes.
#[test]
fn viscells_cook_twice_identical_bytes() {
    let a = cook_doc("switchbacks");
    let b = cook_doc("switchbacks");
    assert_eq!(a.viscells_ron, b.viscells_ron);
    assert!(a.viscells_ron.ends_with('\n'));
    assert!(a.viscells_ron.contains("version: 1"));
    assert_eq!(a.report.vis_cells.len(), a.file.canonical.sections.len() + 1);
    assert_eq!(a.report.vis_cells, b.report.vis_cells);
    assert_eq!(a.report.vis_portals, b.report.vis_portals);
    assert!(!a.report.vis_portals.is_empty());
}

/// A wall at x = 4 m, and the same wall with the centre 0.5 m square left out.
fn wall_pair() -> (GapSection, GapSection) {
    let ys = [0, 1250, 1750, 3000];
    let zs = [0, 3750, 4250, 8000];
    let mut sealed = Vec::new();
    let mut gapped = Vec::new();
    for yi in 0..ys.len() - 1 {
        for zi in 0..zs.len() - 1 {
            let (y0, y1) = (ys[yi], ys[yi + 1]);
            let (z0, z1) = (zs[zi], zs[zi + 1]);
            let hole = y0 == 1250 && z0 == 3750;
            let cell = [
                GapTri {
                    v: [[4000, y0, z0], [4000, y1, z0], [4000, y1, z1]],
                    kind: TriKind::Wall,
                },
                GapTri {
                    v: [[4000, y0, z0], [4000, y1, z1], [4000, y0, z1]],
                    kind: TriKind::Wall,
                },
            ];
            sealed.extend(cell);
            if !hole {
                gapped.extend(cell);
            }
        }
    }
    let portals_mm = vec![
        vec![[4000, 0, 0], [3990, 0, 0], [3990, 0, 8000], [4000, 0, 8000]],
        vec![
            [4000, 3000, 0],
            [3990, 3000, 0],
            [3990, 3000, 8000],
            [4000, 3000, 8000],
        ],
        vec![[4000, 0, 0], [3990, 0, 0], [3990, 3000, 0], [4000, 3000, 0]],
        vec![
            [4000, 0, 8000],
            [3990, 0, 8000],
            [3990, 3000, 8000],
            [4000, 3000, 8000],
        ],
    ];
    let zone = Some(CookedZoneShape::OrientedBox {
        center_mm: [2000, 1500, 4000],
        half_mm: [2500, 2000, 4500],
        yaw: 0.0,
    });
    let section = |source: &str, triangles: Vec<GapTri>| GapSection {
        source: source.to_string(),
        triangles,
        portals_mm: portals_mm.clone(),
        zone,
        others: Vec::new(),
    };
    (section("tubes/sealed", sealed), section("tubes/gapped", gapped))
}
