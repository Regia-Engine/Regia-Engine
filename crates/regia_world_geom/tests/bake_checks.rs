// SPDX-License-Identifier: MPL-2.0
//! Plan row W-09 (spec 121 § 10, ADR-8359): each named cook check fails on its own fixture.
//! The numbers come from the plan and from the three ignored map-load cases (requirements only;
//! no map files copied).

#![allow(clippy::unwrap_used)]

use std::cell::Cell;

use regia_world_geom::TerrainBakeReport;
use regia_world_geom::checks::{
    BakeAudit, DrawnSolid, EndpointPair, FACET_SLOPE_LIMIT, HiddenFace, NavSample, SeamEdge, SpawnRing,
    StatedFile, StatedMouth, StatedPortal, column_over_capacity, flood_reaches_border, floor_slope,
    ground_budget_bytes, ground_memory_bytes, ground_over_budget_report, junction_endpoints_match,
    long_cap_over, member_floor_step_m, mouth_cut_equal, only_ramp_facet_failures, seam_gap_over,
    waive_ramp_facets,
};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::{
    Junction, LAYOUT_VERSION, Passage, PassageKind, SAMPLE_STEP_M, TunnelLayout,
};
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::meshlayers::{GroundMode, MeshLayersDocument, TunnelDetail};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::settings::SPAN_CAPACITY;

const GREEN_CODES: [&str; 29] = [
    "drift_triangles",
    "drift_manifest",
    "drift_mouth",
    "drift_portal",
    "spawn_not_flat",
    "drawn_solid_off_wall",
    "column_over_max_floors",
    "seam_gap",
    "ground_pitch_seam",
    "mouth_cut_mismatch",
    "header_mismatch",
    "lattice_mismatch",
    "determinism_table",
    "not_watertight",
    "above_ground_outside_cut",
    "nav_off_floor",
    "zone_mismatch",
    "hidden_face_in_wall",
    "junction_open",
    "shaft_lid_present",
    "ground_inside_span",
    "lane_too_narrow",
    "hull_blocked",
    "ramp_facet_slope_over_limit",
    "portal_blocked",
    "render_off_collision",
    "ground_over_budget",
    "ground_vs_render",
    "input_missing",
];

fn probe_layout() -> TunnelLayout {
    tube(
        "probe",
        "tube",
        22.0,
        12.0,
        vec![[0.0, -40.0, -20.0], [0.0, -40.0, 20.0]],
    )
}

fn tube(map: &str, id: &str, width: f32, ceiling: f32, points: Vec<[f32; 3]>) -> TunnelLayout {
    TunnelLayout {
        map_id: map.into(),
        version: LAYOUT_VERSION,
        sample_step_m: SAMPLE_STEP_M,
        passages: vec![Passage {
            id: id.into(),
            kind: PassageKind::Tube,
            width_m: width,
            ceiling_m: ceiling,
            points,
        }],
        openings: Vec::new(),
        junctions: Vec::new(),
        crawlways: Vec::new(),
    }
}

fn probe_ground() -> GroundInput {
    // -512 + 15744/32 = -20 m, under the probe roof and over its floor.
    GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![15_744; 16],
    }
}

fn canyon_ground() -> GroundInput {
    GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    }
}

fn canyon_terrain() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16_384)
}

fn base_doc() -> MeshLayersDocument {
    MeshLayersDocument::from_ron(
        r#"(version: 1, ground_tile_cells: 16, layers: [(id: "tubes", source: Passages(path: "p.ron"))])"#,
    )
    .unwrap()
}

fn cook_layout(
    layout: &TunnelLayout,
    audit: BakeAudit,
    ground: &GroundInput,
    terrain: Option<&TerrainSampler>,
) -> Result<regia_world_geom::cook::CookOutput, CookError> {
    let passages = PassagesDocument::from_layout(layout.clone()).to_ron().unwrap();
    let mut doc = base_doc();
    doc.audit = audit;
    let ron = doc.to_ron().unwrap();
    let read = |path: &str| {
        if path == "p.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{path}: missing"))
        }
    };
    cook(&CookInputs {
        document: ron.as_bytes(),
        ground,
        terrain,
        read_file: &read,
    })
}

fn fail_layout(
    layout: &TunnelLayout,
    audit: BakeAudit,
    ground: &GroundInput,
    terrain: Option<&TerrainSampler>,
) -> TerrainBakeReport {
    match cook_layout(layout, audit, ground, terrain) {
        Err(CookError::Checks(report)) => report,
        Ok(out) => panic!("cook passed: {}", out.bake_report.failure_code()),
        Err(e) => panic!("{e} ({})", e.code()),
    }
}

fn fail_probe(audit: BakeAudit) -> TerrainBakeReport {
    let ground = probe_ground();
    fail_layout(&probe_layout(), audit, &ground, None)
}

/// Fills one audit. The default lives here so callers do not assign fields onto `BakeAudit::default()`.
fn planted(set: impl FnOnce(&mut BakeAudit)) -> BakeAudit {
    let mut audit = BakeAudit::default();
    set(&mut audit);
    audit
}

fn finding<'a>(report: &'a TerrainBakeReport, code: &str) -> &'a regia_world_geom::checks::CheckFinding {
    report
        .checks
        .iter()
        .find(|check| check.code == code && !check.passed && !check.waived)
        .unwrap_or_else(|| panic!("{code} did not fail in {report}"))
}

fn near(location: &str, target: [f64; 2]) -> bool {
    let mut parts = location.split(',');
    let x: f64 = parts.next().unwrap().trim().parse().unwrap();
    let z: f64 = parts.next().unwrap().trim().parse().unwrap();
    let dx = x - target[0];
    let dz = z - target[1];
    dx * dx + dz * dz <= 400.0
}

#[test]
fn an_empty_audit_passes() {
    let ground = probe_ground();
    let out = cook_layout(&probe_layout(), BakeAudit::default(), &ground, None)
        .unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    assert!(out.bake_report.passed, "{}", out.bake_report);
    assert_eq!(out.bake_report.map, "probe");
    let codes: Vec<_> = out.bake_report.checks.iter().map(|check| check.code).collect();
    assert_eq!(codes, GREEN_CODES);
    assert!(out.bake_report.checks.iter().all(|check| check.passed));
    let again = cook_layout(&probe_layout(), BakeAudit::default(), &ground, None).unwrap();
    assert_eq!(again.bake_report.content_hash, out.bake_report.content_hash);
}

#[test]
fn triangle_count_drift_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.triangle_file = Some(3526);
        audit.triangle_layout = Some(3378);
    }));
    let found = finding(&report, "drift_triangles");
    assert_eq!(report.failure_code(), "drift_triangles");
    assert_eq!(found.measured, "3526 vs 3378");
}

#[test]
fn stale_manifest_row_fails_verify() {
    let report = fail_probe(planted(|audit| {
        audit.manifest = vec![StatedFile {
            path: "layer.bin".into(),
            bytes: 10,
        }];
        audit.files = vec![StatedFile {
            path: "layer.bin".into(),
            bytes: 11,
        }];
    }));
    let found = finding(&report, "drift_manifest");
    assert_eq!(report.failure_code(), "drift_manifest");
    assert_eq!(found.location, "layer.bin");
    assert_eq!(found.measured, "layer.bin 10 vs 11");
}

#[test]
fn mouth_drifts_from_layout_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.stated_mouths = vec![StatedMouth {
            id: "mid".into(),
            layout: [-40.0, -256.0, -90.0],
            file: [-48.0, -256.0, -78.0],
        }];
    }));
    let found = finding(&report, "drift_mouth");
    assert_eq!(report.failure_code(), "drift_mouth");
    assert!(found.measured.contains("-40"), "{}", found.measured);
    assert!(found.measured.contains("-48"), "{}", found.measured);
    assert!(found.measured.contains("-256"), "{}", found.measured);
    assert!(found.measured.contains("-90"), "{}", found.measured);
    assert!(found.measured.contains("-78"), "{}", found.measured);
}

#[test]
fn portal_report_drift_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.stated_portals = vec![StatedPortal {
            id: "cut".into(),
            report_s: 46.88,
            layout_s: 57.7,
        }];
    }));
    let found = finding(&report, "drift_portal");
    assert_eq!(report.failure_code(), "drift_portal");
    assert!(found.measured.contains("46.88"), "{}", found.measured);
    assert!(found.measured.contains("57.7"), "{}", found.measured);
}

#[test]
fn spawn_ring_rise_over_limit_fails() {
    let report = fail_probe(planted(|audit| {
        audit.spawn_rings = vec![SpawnRing {
            radius_m: 9.0,
            rise_m: 3.95,
            limit_m: 2.41,
            height_window_m: 4.51,
        }];
    }));
    let found = finding(&report, "spawn_not_flat");
    assert_eq!(report.failure_code(), "spawn_not_flat");
    assert!(found.measured.contains("9"), "{}", found.measured);
    assert!(found.measured.contains("3.95"), "{}", found.measured);
    assert!(found.measured.contains("2.41"), "{}", found.measured);
    assert!(found.measured.contains("4.51"), "{}", found.measured);
}

#[test]
fn drawn_solid_within_half_meter_of_wall() {
    let report = fail_probe(planted(|audit| {
        audit.drawn_solids = vec![DrawnSolid {
            extent_m: 5.0,
            lane_m: 20.0,
            wall_distance_m: 1.2,
        }];
    }));
    let found = finding(&report, "drawn_solid_off_wall");
    assert_eq!(report.failure_code(), "drawn_solid_off_wall");
    assert!(found.measured.contains('5'), "{}", found.measured);
    assert!(found.measured.contains("20"), "{}", found.measured);
    assert!(found.measured.contains("0.5"), "{}", found.measured);
}

#[test]
fn displacement_over_limit_is_render_off_collision() {
    let layout = probe_layout();
    let passages = PassagesDocument::from_layout(layout).to_ron().unwrap();
    let mut doc = base_doc();
    doc.layers[0].look.detail = Some(TunnelDetail {
        normal_displace: 0.05,
        noise_amplitude: 0.12,
        noise_scale: 0.9,
        seed: 7,
    });
    let ron = doc.to_ron().unwrap();
    let ground = probe_ground();
    let read = |path: &str| {
        if path == "p.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{path}: missing"))
        }
    };
    let err = cook(&CookInputs {
        document: ron.as_bytes(),
        ground: &ground,
        terrain: None,
        read_file: &read,
    })
    .expect_err("displaced");
    let CookError::Checks(report) = err else {
        panic!("expected render_off_collision, got {err} ({})", err.code());
    };
    let found = finding(&report, "render_off_collision");
    assert_eq!(report.failure_code(), "render_off_collision");
    assert!(found.measured.contains("0.05"), "{}", found.measured);
    assert!(found.measured.contains("0.12"), "{}", found.measured);
    assert!(found.measured.contains("versus 0"), "{}", found.measured);
}

#[test]
fn ramp_facet_slope_over_limit_fails_cook() {
    let slope = floor_slope([[0, 0, 0], [1000, 0, 0], [0, 20_000, 1000]]);
    assert!((slope - 20.0).abs() < 1.0e-6, "{slope}");
    assert!(slope > FACET_SLOPE_LIMIT);

    let steep = tube(
        "steep",
        "bend",
        22.0,
        12.0,
        vec![[0.0, -40.0, 0.0], [6.0, -40.0, 0.0], [6.0, -28.0, 6.0]],
    );
    let ground = probe_ground();
    let steep_report = fail_layout(&steep, BakeAudit::default(), &ground, None);
    assert!(
        steep_report
            .checks
            .iter()
            .any(|check| check.code == "ramp_facet_slope_over_limit" && !check.passed),
        "{steep_report}"
    );

    let layout = for_map("switchbacks").unwrap();
    let passages = PassagesDocument::from_layout(layout).to_ron().unwrap();
    let doc = base_doc().to_ron().unwrap();
    let canyon = canyon_ground();
    let terrain = canyon_terrain();
    let read = |path: &str| {
        if path == "p.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{path}: missing"))
        }
    };
    let cooked = cook(&CookInputs {
        document: doc.as_bytes(),
        ground: &canyon,
        terrain: Some(&terrain),
        read_file: &read,
    });
    let Err(CookError::Checks(report)) = cooked else {
        panic!("switchbacks cook did not fail the facet check: {cooked:?}");
    };
    let facets: Vec<_> = report
        .checks
        .iter()
        .filter(|check| !check.passed && !check.waived)
        .collect();
    let described: Vec<_> = facets
        .iter()
        .map(|check| format!("{} {} {}", check.code, check.location, check.measured))
        .collect();
    assert!(
        facets
            .iter()
            .all(|check| check.code == "ramp_facet_slope_over_limit"),
        "{described:?}"
    );
    assert_eq!(facets.len(), 2, "{described:?}");
    assert!(
        facets.iter().any(|check| near(&check.location, [175.0, 92.0])),
        "{described:?}"
    );
    assert!(
        facets.iter().any(|check| near(&check.location, [-173.0, 82.0])),
        "{described:?}"
    );
    assert!(only_ramp_facet_failures(&report));
    let waived = waive_ramp_facets(&doc, &report).unwrap();
    let again = cook(&CookInputs {
        document: waived.as_bytes(),
        ground: &canyon,
        terrain: Some(&terrain),
        read_file: &read,
    })
    .unwrap_or_else(|e| panic!("waived cook: {e} ({})", e.code()));
    assert!(again.bake_report.passed, "{}", again.bake_report);
    assert_eq!(again.bake_report.content_hash, report.content_hash);
    let lid = again
        .bake_report
        .checks
        .iter()
        .find(|check| check.code == "shaft_lid_present")
        .unwrap();
    assert!(lid.passed, "{lid:?}");
    let hash = &facets[0].triangle_hash;
    let flipped = format!("{}{}", if hash.starts_with('a') { "b" } else { "a" }, &hash[1..]);
    let bad = waived.replace(hash, &flipped);
    let lapsed = cook(&CookInputs {
        document: bad.as_bytes(),
        ground: &canyon,
        terrain: Some(&terrain),
        read_file: &read,
    });
    let Err(CookError::Checks(lapsed)) = lapsed else {
        panic!("a mutated waiver hash must fail the cook");
    };
    assert_eq!(lapsed.failure_code(), "ramp_facet_slope_over_limit");
}

#[test]
fn column_capacity_exceeded_fails_cook() {
    assert!(column_over_capacity(13, 8));
    assert!(column_over_capacity(17, SPAN_CAPACITY));
    assert!(!column_over_capacity(u32::from(SPAN_CAPACITY), SPAN_CAPACITY));
    let report = fail_probe(planted(|audit| {
        audit.column_floors = Some(13);
    }));
    let found = finding(&report, "column_over_max_floors");
    assert_eq!(report.failure_code(), "column_over_max_floors");
    assert!(found.measured.contains("13"), "{}", found.measured);
    assert!(found.measured.contains("16"), "{}", found.measured);
}

#[test]
fn open_floor_edge_seam_report_flags_a_long_cap_on_uneven_ground() {
    assert!(seam_gap_over(0.001));
    assert!(!seam_gap_over(1.0e-4));
    assert!(long_cap_over(9.0, 0.03, true));
    assert!(!long_cap_over(20.0, 1.0, false), "a deep floor is not a cap");
    let report = fail_probe(planted(|audit| {
        audit.seam_edges = vec![SeamEdge {
            length_m: 9.0,
            deviation_m: 0.03,
            gap_m: 0.001,
            near_ground: true,
        }];
    }));
    let found = finding(&report, "seam_gap");
    assert_eq!(report.failure_code(), "seam_gap");
    assert_eq!(found.location, "edge 0");
    assert!(found.measured.contains("9"), "{}", found.measured);
    assert!(found.measured.contains("0.001"), "{}", found.measured);
}

#[test]
fn junction_member_floors_within_step_bound() {
    for map in ["cinder_steps", "switchbacks", "atacama_rail"] {
        let layout = for_map(map).unwrap();
        for junction in &layout.junctions {
            let step = member_floor_step_m(&layout, junction);
            assert!(
                step <= 0.05,
                "{map} {} step {step} m (the shared control point, not the next segment)",
                junction.id
            );
        }
    }
    let cinder = for_map("cinder_steps").unwrap();
    let south = cinder
        .junctions
        .iter()
        .find(|junction| junction.id == "south")
        .unwrap();
    assert!(member_floor_step_m(&cinder, south) <= 0.05);
    assert!(junction_endpoints_match([1, 2, 3], [1, 2, 3]));
    assert!(!junction_endpoints_match([1, 2, 3], [1, 2, 4]));

    let mut stepped = tube("step", "a", 22.0, 12.0, vec![[0.0, 0.0, 0.0], [0.0, 0.0, 20.0]]);
    stepped.passages.push(Passage {
        id: "b".into(),
        kind: PassageKind::Tube,
        width_m: 22.0,
        ceiling_m: 12.0,
        points: vec![[0.0, 0.5, 0.0], [0.0, 0.5, 20.0]],
    });
    stepped.junctions.push(Junction {
        id: "meet".into(),
        at: [0.0, 0.0, 0.0],
        passages: vec!["a".into(), "b".into()],
    });
    let ground = probe_ground();
    let report = fail_layout(&stepped, BakeAudit::default(), &ground, None);
    let found = finding(&report, "junction_open");
    assert_eq!(report.failure_code(), "junction_open");
    assert!(found.measured.contains("0.500"), "{}", found.measured);
}

#[test]
fn sweep_without_patches_or_onfoot_grid_is_an_error() {
    let mut doc = base_doc();
    doc.audit.require_sweep = true;
    let ron = doc.to_ron().unwrap();
    let ground = probe_ground();
    let hit = Cell::new(false);
    let read = |path: &str| {
        hit.set(true);
        Err(format!("{path}: should not be read"))
    };
    let err = cook(&CookInputs {
        document: ron.as_bytes(),
        ground: &ground,
        terrain: None,
        read_file: &read,
    })
    .expect_err("sweep");
    assert!(
        !hit.get(),
        "a missing sweep returns before any named file is read"
    );
    let CookError::Checks(report) = err else {
        panic!("expected input_missing, got {err} ({})", err.code());
    };
    let found = finding(&report, "input_missing");
    assert_eq!(report.failure_code(), "input_missing");
    assert_eq!(found.location, "sweep");
}

/// A `Full` ground over the spec 121 § 11 budget is refused by the bake check (`CookError::Checks`,
/// code `ground_over_budget`) before any named file is read, keeps the authored mode, and the finding
/// names the `Nested` mode that fits (ADR-8436: one refusal carries both the check and the advice).
#[test]
fn ground_over_budget_names_nested_and_does_not_switch() {
    assert!(
        ground_over_budget_report("", GroundMode::Full, 32, ground_budget_bytes(1280.0), None, &[]).is_none()
    );
    assert_eq!(ground_memory_bytes(4), 32);
    let n = 4223;
    let memory = ground_memory_bytes(n);
    let budget = ground_budget_bytes(1280.0);
    assert!(memory > budget);
    let doc = MeshLayersDocument::new().to_ron().unwrap();
    // Flat ground: every tile meets `pitch_error_m` at any level, so the smallest Nested mode fits.
    let ground = GroundInput {
        lattice_n: n,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![16_384; (n * n) as usize],
    };
    let hit = Cell::new(false);
    let read = |path: &str| {
        hit.set(true);
        Err(path.to_string())
    };
    let err = cook(&CookInputs {
        document: doc.as_bytes(),
        ground: &ground,
        terrain: None,
        read_file: &read,
    })
    .expect_err("budget");
    assert!(!hit.get());
    let CookError::Checks(report) = err else {
        panic!("expected ground_over_budget, got {err} ({})", err.code());
    };
    assert_eq!(report.failure_code(), "ground_over_budget");
    assert!(!report.passed);
    assert_eq!(report.ground_mode, "Full");
    assert!(report.memory_bytes >= memory && report.memory_bytes > budget);
    let found = finding(&report, "ground_over_budget");
    assert!(
        found.measured.contains(&report.memory_bytes.to_string())
            && found.measured.contains(&budget.to_string()),
        "{}",
        found.measured
    );
    assert_eq!(found.fits, Some(GroundMode::Nested { max_level: 1 }));
    assert!(
        found.measured.contains("Nested { max_level: 1 }"),
        "{}",
        found.measured
    );
    assert!(found.tiles.is_empty());
}

#[test]
fn ground_pitch_seam_fails_on_authored_neighbors() {
    let mut nested = base_doc();
    nested.ground = GroundMode::Nested { max_level: 1 };
    let ground = probe_ground();
    let passages = PassagesDocument::from_layout(probe_layout()).to_ron().unwrap();
    let read = |path: &str| {
        if path == "p.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{path}: missing"))
        }
    };
    // `Nested` cooks since W-10 (ADR-8214): the check passes on the cook's own seam measure, and
    // only an unmeasured or badly levelled ground fails it (ADR-8436).
    let out = cook(&CookInputs {
        document: nested.to_ron().unwrap().as_bytes(),
        ground: &ground,
        terrain: None,
        read_file: &read,
    })
    .unwrap_or_else(|e| panic!("{e} ({})", e.code()));
    assert!(out.report.ground.pitch_seam.is_some());
    let seam = out
        .bake_report
        .checks
        .iter()
        .find(|check| check.code == "ground_pitch_seam")
        .unwrap();
    assert!(seam.passed, "{}", seam.measured);
    assert!(seam.measured.contains("pitch borders"), "{}", seam.measured);
    assert_eq!(out.bake_report.ground_mode, "Nested { max_level: 1 }");

    let report = fail_probe(planted(|audit| {
        audit.pitch_neighbors = vec![(0, 2)];
    }));
    let found = finding(&report, "ground_pitch_seam");
    assert_eq!(report.failure_code(), "ground_pitch_seam");
    assert!(
        found.measured.contains("0") && found.measured.contains('2'),
        "{}",
        found.measured
    );
}

#[test]
fn ground_inside_span_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.span_intrusions = vec![regia_world_geom::checks::SpanPoint {
            y_m: 1.0,
            floor_m: 0.0,
            lid_m: 2.0,
        }];
    }));
    let found = finding(&report, "ground_inside_span");
    assert_eq!(report.failure_code(), "ground_inside_span");
    assert!(found.measured.contains('1'), "{}", found.measured);
}

#[test]
fn header_mismatch_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.claimed_max_floors = Some(3);
    }));
    let found = finding(&report, "header_mismatch");
    assert_eq!(report.failure_code(), "header_mismatch");
    assert!(found.measured.contains('3'), "{}", found.measured);
    assert!(found.measured.contains('8'), "{}", found.measured);
}

#[test]
fn lattice_mismatch_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.claimed_lattice_n = Some(2);
    }));
    let found = finding(&report, "lattice_mismatch");
    assert_eq!(report.failure_code(), "lattice_mismatch");
    assert!(found.measured.contains('2'), "{}", found.measured);
    assert!(found.measured.contains('4'), "{}", found.measured);
}

#[test]
fn mouth_cut_mismatch_is_exact() {
    assert!(!mouth_cut_equal(1, 2));
    assert!(mouth_cut_equal(3, 3));
    let report = fail_probe(planted(|audit| {
        audit.mouth_cut_mismatch = true;
    }));
    let found = finding(&report, "mouth_cut_mismatch");
    assert_eq!(report.failure_code(), "mouth_cut_mismatch");
    assert!(found.measured.contains("differ"), "{}", found.measured);
}

#[test]
fn not_watertight_flood_reaches_a_border() {
    let mut sealed = vec![true; 9];
    sealed[4] = false;
    assert!(!flood_reaches_border(3, 3, &sealed, (1, 1)));
    let mut leak = vec![true; 25];
    leak[2 * 5 + 2] = false;
    leak[2 * 5 + 1] = false;
    leak[2 * 5] = false;
    assert!(flood_reaches_border(5, 5, &leak, (2, 2)));
    let report = fail_probe(planted(|audit| {
        audit.leaky_mesh = true;
    }));
    assert_eq!(report.failure_code(), "not_watertight");
}

#[test]
fn above_ground_outside_cut_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.above_ground_m = Some(1.0);
    }));
    let found = finding(&report, "above_ground_outside_cut");
    assert_eq!(report.failure_code(), "above_ground_outside_cut");
    assert!(found.measured.contains('1'), "{}", found.measured);
}

#[test]
fn nav_off_floor_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.nav_samples = vec![NavSample {
            y_m: 1.0,
            floor_y_m: 0.0,
        }];
    }));
    let found = finding(&report, "nav_off_floor");
    assert_eq!(report.failure_code(), "nav_off_floor");
    assert!(found.measured.contains("0.5"), "{}", found.measured);
}

#[test]
fn portal_blocked_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.blocked_portal = true;
    }));
    assert_eq!(report.failure_code(), "portal_blocked");
}

#[test]
fn zone_mismatch_fails_when_a_sample_is_outside() {
    let report = fail_probe(planted(|audit| {
        audit.zone_samples = vec![[1000.0, 1000.0, 1000.0]];
    }));
    let found = finding(&report, "zone_mismatch");
    assert_eq!(report.failure_code(), "zone_mismatch");
    assert!(found.measured.contains("1000"), "{}", found.measured);
}

#[test]
fn determinism_table_hash_fails_on_a_stated_mismatch() {
    let report = fail_probe(planted(|audit| {
        audit.expected_content_hash = "00".into();
    }));
    let found = finding(&report, "determinism_table");
    assert_eq!(report.failure_code(), "determinism_table");
    assert!(found.measured.contains("00"), "{}", found.measured);
    assert!(found.measured.contains("!="), "{}", found.measured);
}

#[test]
fn shaft_lid_present_fails_when_authored() {
    let report = fail_probe(planted(|audit| {
        audit.shaft_lid = true;
    }));
    assert_eq!(report.failure_code(), "shaft_lid_present");
}

#[test]
fn lane_too_narrow_fails_cook() {
    let narrow = tube(
        "narrow",
        "slot",
        1.0,
        12.0,
        vec![[0.0, -40.0, -20.0], [0.0, -40.0, 20.0]],
    );
    let ground = probe_ground();
    let report = fail_layout(&narrow, BakeAudit::default(), &ground, None);
    assert_eq!(report.failure_code(), "lane_too_narrow");
    assert!(finding(&report, "hull_blocked").code == "hull_blocked");
}

#[test]
fn hull_blocked_fails_cook() {
    let tight = tube(
        "hull",
        "slot",
        8.0,
        12.0,
        vec![[0.0, -40.0, -20.0], [0.0, -40.0, 20.0]],
    );
    let ground = probe_ground();
    let report = fail_layout(&tight, BakeAudit::default(), &ground, None);
    assert_eq!(report.failure_code(), "hull_blocked");
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.code == "lane_too_narrow" && check.passed)
    );
}

#[test]
fn hidden_face_in_wall_fails_cook() {
    let report = fail_probe(planted(|audit| {
        audit.hidden_faces = vec![HiddenFace {
            distance_m: 0.2,
            below_floor_m: 0.4,
        }];
    }));
    assert_eq!(report.failure_code(), "hidden_face_in_wall");
}

#[test]
fn endpoint_pair_opens_a_junction() {
    let report = fail_probe(planted(|audit| {
        audit.endpoint_pairs = vec![EndpointPair {
            a: [0, 0, 0],
            b: [1, 0, 0],
        }];
    }));
    assert_eq!(report.failure_code(), "junction_open");
}
