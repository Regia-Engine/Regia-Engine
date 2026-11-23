// SPDX-License-Identifier: MPL-2.0
//! CT-05 robust coincidence acceptance (ADR-8347): neighbor cells, strip/fan expansion,
//! the candidate budget, material UV sets, attribute/deformation equivalence, mesh
//! evidence, primitive order and the source-lint policy surface. The legacy predicate
//! and its tests in `gltf_lint.rs` are unchanged.

#[path = "support/gltf_lint.rs"]
mod fixture;

use fixture::Fixture;
use regia_asset_pipeline::gltf_lint::{
    Agreement, CoincidenceMode, Equivalence, GltfLintError, GltfLintReport, GltfLintSettings, PrimitiveData,
    RobustCoincidence, RobustError, RobustGeometrySettings, expand_topology, lint_gltf,
    robust_mesh_coincidences,
};
use regia_asset_pipeline::source_lint::{self, FindingSeverity, SourceLintPolicy};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

fn empty() -> Fixture {
    Fixture {
        json: json!({
            "asset": {"version": "2.0"}, "scene": 0,
            "scenes": [{"nodes": [0]}], "nodes": [{"mesh": 0}],
            "meshes": [], "accessors": [], "bufferViews": [], "buffers": [{"byteLength": 0}]
        }),
        bin: Vec::new(),
    }
}

fn positions(f: &mut Fixture, points: &[[f32; 3]]) -> usize {
    let flat: Vec<f32> = points.iter().flatten().copied().collect();
    let index = f.accessor(&flat, 3);
    let finite = || points.iter().filter(|p| p.iter().all(|v| v.is_finite()));
    let min: Vec<f32> = (0..3)
        .map(|k| finite().map(|p| p[k]).fold(f32::INFINITY, f32::min))
        .collect();
    let max: Vec<f32> = (0..3)
        .map(|k| finite().map(|p| p[k]).fold(f32::NEG_INFINITY, f32::max))
        .collect();
    f.json["accessors"][index]["min"] = json!(min);
    f.json["accessors"][index]["max"] = json!(max);
    index
}

fn raw(f: &mut Fixture, bytes: &[u8], component: u32, kind: &str, count: usize) -> usize {
    let offset = f.bin.len().next_multiple_of(4);
    f.bin.resize(offset, 0);
    f.bin.extend_from_slice(bytes);
    let views = f.json["bufferViews"].as_array_mut().expect("views");
    let view = views.len();
    views.push(json!({"buffer": 0, "byteOffset": offset, "byteLength": bytes.len()}));
    let accessors = f.json["accessors"].as_array_mut().expect("accessors");
    let index = accessors.len();
    accessors.push(json!({"bufferView": view, "componentType": component, "count": count, "type": kind}));
    f.json["buffers"][0]["byteLength"] = json!(f.bin.len());
    index
}

fn indices(f: &mut Fixture, values: &[u32]) -> usize {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    raw(f, &bytes, 5125, "SCALAR", values.len())
}

fn mesh(f: &mut Fixture, primitives: Value) -> usize {
    let meshes = f.json["meshes"].as_array_mut().expect("meshes");
    meshes.push(json!({"primitives": primitives}));
    meshes.len() - 1
}

fn robust() -> GltfLintSettings {
    GltfLintSettings {
        mode: CoincidenceMode::Robust,
        ..Default::default()
    }
}

fn try_lint(f: &Fixture, settings: &GltfLintSettings) -> Result<GltfLintReport, GltfLintError> {
    let dir = unique_temp_dir("geometry-robust-lint");
    let path = dir.join("model.glb");
    let bytes = f.glb();
    regia_test_utils::write_atomic(&path, &bytes).expect("fixture");
    let report = lint_gltf(&path, settings);
    assert_eq!(
        std::fs::read(&path).expect("source"),
        bytes,
        "lint never rewrites"
    );
    if let Ok(first) = &report {
        assert_eq!(
            first,
            &lint_gltf(&path, settings).expect("repeat"),
            "deterministic"
        );
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
    report
}

fn lint(f: &Fixture, settings: &GltfLintSettings) -> GltfLintReport {
    try_lint(f, settings).expect("lint")
}

type Canonical = (String, Agreement, Equivalence, Vec<String>, Vec<String>, bool);

fn canonical(rows: &[RobustCoincidence]) -> Vec<Canonical> {
    let mut list: Vec<Canonical> = rows
        .iter()
        .map(|r| {
            (
                r.fingerprint.clone(),
                r.agreement,
                r.equivalence,
                r.differences.clone(),
                r.unavailable.clone(),
                r.safe_delete_candidate,
            )
        })
        .collect();
    list.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    list
}

fn mesh_rows(report: &GltfLintReport, mesh: usize) -> Vec<RobustCoincidence> {
    report
        .robust_coincidences
        .iter()
        .filter(|r| r.mesh == mesh)
        .cloned()
        .collect()
}

/// Two triangles 2 µm apart on either side of a legacy 0.1 mm quantization boundary.
fn straddling() -> Fixture {
    let mut f = empty();
    let p = positions(
        &mut f,
        &[
            [0.0, 0.0, 0.000049],
            [1.0, 0.0, 0.000049],
            [0.0, 1.0, 0.000049],
            [0.0, 0.0, 0.000051],
            [1.0, 0.0, 0.000051],
            [0.0, 1.0, 0.000051],
        ],
    );
    let uv = f.accessor(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0], 2);
    mesh(&mut f, json!([{"attributes": {"POSITION": p, "TEXCOORD_0": uv}}]));
    f
}

#[test]
fn neighbor_cell_pairs_are_found_in_robust_mode_while_legacy_misses_them() {
    let f = straddling();
    let legacy = lint(&f, &GltfLintSettings::default());
    assert_eq!(legacy.mode, CoincidenceMode::Legacy);
    assert!(
        legacy.coincident_triangles.is_empty(),
        "the legacy predicate keeps its boundary behavior"
    );
    assert!(legacy.robust_coincidences.is_empty());
    let report = lint(&f, &robust());
    assert_eq!(report.mode, CoincidenceMode::Robust);
    assert!(
        report.coincident_triangles.is_empty(),
        "robust replaces legacy rows"
    );
    assert_eq!(report.robust_coincidences.len(), 1);
    let row = &report.robust_coincidences[0];
    assert_eq!(row.agreement, Agreement::Tolerance);
    assert!(row.max_separation_meters > 1.9e-6 && row.max_separation_meters < 2.1e-6);
    assert_eq!(row.tolerance_meters, 1e-4);
    assert_eq!(
        row.equivalence,
        Equivalence::Equivalent,
        "no material samples a UV set"
    );
    assert!(
        !row.safe_delete_candidate,
        "tolerance agreement is never a safe-delete candidate"
    );
    assert_eq!(report.robust_work.candidate_pairs, 1);

    // Pure API: pairs straddling a cell edge at many offsets are always candidates.
    let settings = RobustGeometrySettings::default();
    for step in 0..64 {
        let edge = 1e-4 * f64::from(step) - 1e-3;
        let a = (edge - 3e-6) as f32;
        let b = (edge + 3e-6) as f32;
        let data = PrimitiveData {
            mode: 4,
            positions: vec![
                [a, 0.0, 0.0],
                [a, 1.0, 0.0],
                [a, 0.0, 1.0],
                [b, 0.0, 0.0],
                [b, 1.0, 0.0],
                [b, 0.0, 1.0],
            ],
            triangles: vec![[0, 1, 2], [3, 4, 5]],
            ..Default::default()
        };
        let outcome = robust_mesh_coincidences(0, &[data], &settings, &mut 0).expect("valid");
        assert_eq!(outcome.coincidences.len(), 1, "edge {edge}");
    }
}

#[test]
fn relative_tolerance_scales_with_the_mesh() {
    let f = straddling();
    let tight = GltfLintSettings {
        robust: RobustGeometrySettings {
            tolerance_meters: 1e-7,
            ..Default::default()
        },
        ..robust()
    };
    assert!(lint(&f, &tight).robust_coincidences.is_empty());
    let relative = GltfLintSettings {
        robust: RobustGeometrySettings {
            tolerance_meters: 1e-7,
            // Diagonal is about 1.414 m, so the effective tolerance is about 14 µm.
            tolerance_relative: 1e-5,
            ..Default::default()
        },
        ..robust()
    };
    let report = lint(&f, &relative);
    assert_eq!(report.robust_coincidences.len(), 1);
    let effective = report.robust_coincidences[0].tolerance_meters;
    assert!(effective > 1.4e-5 && effective < 1.5e-5, "{effective}");
}

/// A unit quad and an identical copy (vertices 4–7) for topology tests.
fn quad_topologies() -> Fixture {
    let mut f = empty();
    let quad = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]];
    let mut points = quad.to_vec();
    points.extend(quad);
    let p = positions(&mut f, &points);
    for (mode, a, b) in [
        (4, vec![0, 1, 2, 2, 1, 3], vec![4, 5, 6, 6, 5, 7]),
        (5, vec![0, 1, 2, 3], vec![4, 5, 6, 7]),
        (4, vec![0, 1, 3, 0, 3, 2], vec![4, 5, 7, 4, 7, 6]),
        (6, vec![0, 1, 3, 2], vec![4, 5, 7, 6]),
    ] {
        let ia = indices(&mut f, &a);
        let ib = indices(&mut f, &b);
        mesh(
            &mut f,
            json!([
                {"attributes": {"POSITION": p}, "indices": ia, "mode": mode},
                {"attributes": {"POSITION": p}, "indices": ib, "mode": mode},
            ]),
        );
    }
    f.json["nodes"] = json!([{"mesh": 0}, {"mesh": 1}, {"mesh": 2}, {"mesh": 3}]);
    f.json["scenes"][0]["nodes"] = json!([0, 1, 2, 3]);
    f
}

#[test]
fn strip_parity_and_fan_expansion_match_equivalent_lists() {
    // Zigzag strip: every expanded triangle keeps the first triangle's winding.
    let zigzag: Vec<[f64; 2]> = (0..8).map(|i| [f64::from(i / 2), f64::from(i % 2)]).collect();
    let strip: Vec<u32> = (0..8).collect();
    for tri in expand_topology(5, &strip).expect("strip") {
        let [a, b, c] = tri.map(|v| zigzag[v as usize]);
        let z = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
        assert!(z < 0.0, "strip parity flips odd triangles back: {tri:?}");
    }

    let f = quad_topologies();
    let legacy = lint(&f, &GltfLintSettings::default());
    assert_eq!(
        legacy.skipped_primitives.len(),
        4,
        "legacy still skips strips and fans"
    );
    let report = lint(&f, &robust());
    assert!(report.skipped_primitives.is_empty());
    assert_eq!(report.primitives, 8);
    assert_eq!(report.triangles, 16);
    let duplicates: Vec<(usize, usize)> = report
        .duplicate_meshes
        .iter()
        .map(|d| (d.original, d.duplicate))
        .collect();
    assert_eq!(
        duplicates,
        vec![(0, 1), (2, 3)],
        "a strip equals its list, a fan its list"
    );
    for (list, other) in [(0, 1), (2, 3)] {
        let a = mesh_rows(&report, list);
        assert_eq!(a.len(), 2, "two cross-primitive copies per quad");
        assert!(
            a.iter()
                .all(|r| r.agreement == Agreement::Exact && r.safe_delete_candidate)
        );
        assert_eq!(canonical(&a), canonical(&mesh_rows(&report, other)));
    }
    assert_ne!(
        canonical(&mesh_rows(&report, 0)),
        canonical(&mesh_rows(&report, 2)),
        "a different diagonal is different geometry"
    );
}

fn identical(count: usize) -> PrimitiveData {
    PrimitiveData {
        mode: 4,
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        triangles: vec![[0, 1, 2]; count],
        ..Default::default()
    }
}

#[test]
fn dense_identical_triangles_exhaust_an_explicit_candidate_budget() {
    let settings = RobustGeometrySettings {
        max_candidate_pairs: 1000,
        ..Default::default()
    };
    let mut used = 0;
    // 200 copies would be 19 900 pairs; the work stops right after the budget.
    assert_eq!(
        robust_mesh_coincidences(0, &[identical(200)], &settings, &mut used),
        Err(RobustError::CandidateBudget {
            examined: 1001,
            limit: 1000
        })
    );
    assert_eq!(used, 1001);
    let mut used = 0;
    let outcome = robust_mesh_coincidences(0, &[identical(20)], &settings, &mut used).expect("in budget");
    assert_eq!(outcome.work.candidate_pairs, 190);
    assert_eq!(outcome.coincidences.len(), 190);
    assert_eq!(used, 190);
    // The budget is per operation: a second mesh continues the same counter.
    assert!(matches!(
        robust_mesh_coincidences(1, &[identical(50)], &settings, &mut used),
        Err(RobustError::CandidateBudget { examined: 1001, .. })
    ));
    // Separated triangles are never candidates: work is linear, not quadratic.
    let mut far = PrimitiveData {
        mode: 4,
        ..Default::default()
    };
    for i in 0..500_u32 {
        let x = i as f32;
        far.positions
            .extend([[x, 0.0, 0.0], [x + 0.5, 0.0, 0.0], [x, 0.5, 0.0]]);
        far.triangles.push([3 * i, 3 * i + 1, 3 * i + 2]);
    }
    let outcome = robust_mesh_coincidences(0, &[far], &settings, &mut 0).expect("sparse");
    assert_eq!(outcome.work.candidate_pairs, 0);
    assert_eq!(outcome.work.cells, 500);

    // Through the file path the exhausted budget is gltf_lint_limit, never a clean report.
    let mut f = empty();
    let p = positions(&mut f, &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
    let i = indices(&mut f, &[0, 1, 2].repeat(10));
    mesh(&mut f, json!([{"attributes": {"POSITION": p}, "indices": i}]));
    let small = GltfLintSettings {
        robust: RobustGeometrySettings {
            max_candidate_pairs: 44,
            ..Default::default()
        },
        ..robust()
    };
    assert_eq!(
        try_lint(&f, &small).expect_err("budget").code(),
        "gltf_lint_limit"
    );
    let enough = GltfLintSettings {
        robust: RobustGeometrySettings {
            max_candidate_pairs: 45,
            ..Default::default()
        },
        ..robust()
    };
    assert_eq!(lint(&f, &enough).robust_work.candidate_pairs, 45);
}

/// Two exact copies with equal UV0; the material samples `texCoord` 1.
fn uv1_material(uv1: Option<[f32; 12]>) -> Fixture {
    let mut f = empty();
    let tri = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let p = positions(&mut f, &[tri, tri].concat());
    let uv0 = f.accessor(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0], 2);
    let mut attributes = json!({"POSITION": p, "TEXCOORD_0": uv0});
    if let Some(uv1) = uv1 {
        attributes["TEXCOORD_1"] = json!(f.accessor(&uv1, 2));
    }
    let image = raw(&mut f, &[0x89, b'P', b'N', b'G'], 5121, "SCALAR", 4);
    let view = f.json["accessors"][image]["bufferView"].clone();
    f.json["accessors"].as_array_mut().expect("accessors").pop();
    f.json["images"] = json!([{"bufferView": view, "mimeType": "image/png"}]);
    f.json["textures"] = json!([{"source": 0}]);
    f.json["materials"] =
        json!([{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0, "texCoord": 1}}}]);
    mesh(&mut f, json!([{"attributes": attributes, "material": 0}]));
    f
}

#[test]
fn uv1_dependent_materials_are_not_judged_solely_by_uv0() {
    let different = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.5, 0.5, 0.9, 0.5, 0.5, 0.9];
    let f = uv1_material(Some(different));
    assert_eq!(
        lint(&f, &GltfLintSettings::default()).coincident_triangles.len(),
        1,
        "the legacy predicate only sees UV0"
    );
    let report = lint(&f, &robust());
    let row = &report.robust_coincidences[0];
    assert_eq!(row.agreement, Agreement::Exact);
    assert_eq!(row.uv_sets_compared, vec![1]);
    assert_eq!(row.differences, vec!["texcoord_1".to_owned()]);
    assert_eq!(row.equivalence, Equivalence::Differs);
    assert!(!row.safe_delete_candidate);

    let missing = lint(&uv1_material(None), &robust());
    let row = &missing.robust_coincidences[0];
    assert_eq!(row.unavailable, vec!["texcoord_1_missing".to_owned()]);
    assert_eq!(
        row.equivalence,
        Equivalence::Unknown,
        "unavailable data is labeled, not assumed"
    );
    assert!(!row.safe_delete_candidate);

    let same = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0];
    let row = lint(&uv1_material(Some(same)), &robust()).robust_coincidences[0].clone();
    assert_eq!(row.equivalence, Equivalence::Equivalent);
    assert!(row.safe_delete_candidate);

    let mut transformed = uv1_material(Some(same));
    transformed.json["extensionsUsed"] = json!(["KHR_texture_transform"]);
    let row = lint(&transformed, &robust()).robust_coincidences[0].clone();
    assert_eq!(row.unavailable, vec!["khr_texture_transform_unread".to_owned()]);
    assert!(!row.safe_delete_candidate);
}

fn advisory_only(report: &GltfLintReport, difference: &str) {
    assert_eq!(report.robust_coincidences.len(), 1, "{report:?}");
    let row = &report.robust_coincidences[0];
    assert_eq!(row.agreement, Agreement::Exact, "positions are equal");
    assert_eq!(row.differences, vec![difference.to_owned()]);
    assert_eq!(row.equivalence, Equivalence::Differs);
    assert!(
        !row.safe_delete_candidate,
        "{difference} data is never a safe-delete candidate"
    );
    let findings = source_lint::findings_from_gltf_report("m.glb", report);
    let robust: Vec<_> = findings
        .iter()
        .filter(|f| f.rule_id.starts_with("gltf_coincident_"))
        .collect();
    assert_eq!(robust.len(), 1);
    assert_eq!(robust[0].rule_id, "gltf_coincident_advisory");
    assert_eq!(robust[0].severity, FindingSeverity::Info);
    assert!(robust[0].message.contains("safe_delete_candidate false"));
}

fn copies() -> (Fixture, usize) {
    let mut f = empty();
    let tri = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let p = positions(&mut f, &[tri, tri].concat());
    (f, p)
}

#[test]
fn equal_positions_with_different_skin_morph_or_material_stay_advisory() {
    // Material: the same triangle in two primitives with different (untextured) materials.
    let mut f = empty();
    let tri = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let p = positions(&mut f, &tri);
    f.json["materials"] = json!([{"name": "a"}, {"name": "b"}]);
    mesh(
        &mut f,
        json!([
            {"attributes": {"POSITION": p}, "material": 0},
            {"attributes": {"POSITION": p}, "material": 1},
        ]),
    );
    advisory_only(&lint(&f, &robust()), "material");

    // Skin: vertex influences differ between the copies.
    let skin = |second: [u16; 4]| {
        let (mut f, p) = copies();
        let mut joints = Vec::new();
        for v in 0..6 {
            let j = if v < 3 { [0, 0, 0, 0] } else { second };
            joints.extend(j.iter().flat_map(|x| x.to_le_bytes()));
        }
        let j = raw(&mut f, &joints, 5123, "VEC4", 6);
        let w = f.accessor(&[1.0, 0.0, 0.0, 0.0].repeat(6), 4);
        mesh(
            &mut f,
            json!([{"attributes": {"POSITION": p, "JOINTS_0": j, "WEIGHTS_0": w}}]),
        );
        f
    };
    advisory_only(&lint(&skin([1, 0, 0, 0]), &robust()), "skin");
    let equal = lint(&skin([0, 3, 0, 0]), &robust());
    assert!(
        equal.robust_coincidences[0].safe_delete_candidate,
        "zero-weight slots do not change influences"
    );

    // Morph: the second copy moves under the target.
    let (mut f, p) = copies();
    let deltas = f.accessor(
        &[
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.1, 0.0, 0.0, 0.1, 0.0, 0.0, 0.1,
        ],
        3,
    );
    f.json["accessors"][deltas]["min"] = json!([0, 0, 0]);
    f.json["accessors"][deltas]["max"] = json!([0, 0, 0.1]);
    mesh(
        &mut f,
        json!([{"attributes": {"POSITION": p}, "targets": [{"POSITION": deltas}]}]),
    );
    advisory_only(&lint(&f, &robust()), "morph");

    // Exactly equivalent copies are the only warning-level, safe-delete rows.
    let (mut f, p) = copies();
    mesh(&mut f, json!([{"attributes": {"POSITION": p}}]));
    let report = lint(&f, &robust());
    assert!(report.robust_coincidences[0].safe_delete_candidate);
    let findings = source_lint::findings_from_gltf_report("m.glb", &report);
    let row = findings
        .iter()
        .find(|f| f.rule_id == "gltf_coincident_equivalent")
        .expect("equivalent row");
    assert_eq!(row.severity, FindingSeverity::Warning);
    assert_eq!(row.location, "/meshes/0/primitives/0/triangles/1/keeper/0/0");
}

fn three_primitives(order: [usize; 3]) -> Fixture {
    let mut f = empty();
    let base = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let nudged = [[0.0, 0.0, 0.000001], [1.0, 0.0, 0.000001], [0.0, 1.0, 0.000001]];
    let other = [[5.0, 5.0, 0.0], [6.0, 5.0, 0.0], [5.0, 6.0, 0.0]];
    let a = positions(&mut f, &[base, other].concat());
    let b = positions(&mut f, &[other, base].concat());
    let c = positions(&mut f, &nudged);
    f.json["materials"] = json!([{"name": "a"}, {"name": "b"}]);
    let all = [
        json!({"attributes": {"POSITION": a}, "material": 0}),
        json!({"attributes": {"POSITION": b}, "material": 0}),
        json!({"attributes": {"POSITION": c}, "material": 1}),
    ];
    mesh(
        &mut f,
        Value::Array(order.iter().map(|&i| all[i].clone()).collect()),
    );
    f
}

#[test]
fn reordering_primitives_does_not_change_canonical_findings() {
    let reference = lint(&three_primitives([0, 1, 2]), &robust());
    assert_eq!(reference.robust_coincidences.len(), 4, "{reference:?}");
    let expected = canonical(&reference.robust_coincidences);
    for order in [[0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        let report = lint(&three_primitives(order), &robust());
        assert_eq!(canonical(&report.robust_coincidences), expected, "{order:?}");
        assert_eq!(report.robust_work, reference.robust_work, "{order:?}");
    }
    let kinds: Vec<(Agreement, Equivalence)> = expected.iter().map(|c| (c.1, c.2)).collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == (Agreement::Exact, Equivalence::Equivalent))
            .count(),
        2
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == (Agreement::Tolerance, Equivalence::Differs))
            .count(),
        2
    );
}

#[test]
fn nonfinite_degenerate_and_zero_area_triangles_are_evidence() {
    let mut f = empty();
    let p = positions(
        &mut f,
        &[
            [f32::NAN, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [2.0, 0.0, 0.0],
            [3.0, 0.0, 0.0],
            [4.0, 0.0, 0.0],
        ],
    );
    // NaN corner, repeated index, collinear, then a valid triangle.
    let i = indices(&mut f, &[0, 1, 2, 1, 1, 2, 3, 4, 5, 1, 2, 3]);
    mesh(&mut f, json!([{"attributes": {"POSITION": p}, "indices": i}]));
    assert_eq!(
        try_lint(&f, &GltfLintSettings::default())
            .expect_err("legacy refuses non-finite input")
            .code(),
        "gltf_lint_geometry"
    );
    let report = lint(&f, &robust());
    assert_eq!(report.mesh_evidence.len(), 1);
    let row = &report.mesh_evidence[0];
    assert_eq!((row.non_finite, row.repeated_index, row.zero_area), (1, 1, 1));
    assert_eq!(
        (
            row.first_non_finite,
            row.first_repeated_index,
            row.first_zero_area
        ),
        (Some(0), Some(1), Some(2))
    );
    assert_eq!(report.robust_work.eligible_triangles, 1);
    let rules: Vec<String> = source_lint::findings_from_gltf_report("m.glb", &report)
        .into_iter()
        .map(|f| f.rule_id)
        .collect();
    for rule in [
        "gltf_mesh_nonfinite",
        "gltf_mesh_degenerate",
        "gltf_mesh_zero_area",
    ] {
        assert!(rules.iter().any(|r| r == rule), "{rule} in {rules:?}");
        assert!(source_lint::rules().iter().any(|r| r.id == rule));
    }
    // Out-of-range indices stay a hard error in both modes.
    let mut f = empty();
    let p = positions(&mut f, &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
    let i = indices(&mut f, &[0, 1, 9]);
    mesh(
        &mut f,
        json!([{"attributes": {"POSITION": p}, "indices": i, "mode": 5}]),
    );
    assert_eq!(
        try_lint(&f, &robust()).expect_err("index").code(),
        "gltf_lint_geometry"
    );
}

#[test]
fn policy_controls_mode_units_and_candidate_limit() {
    let text = "(format_version: 1, defaults: (geometry: Some((coincidence_mode: Some(robust), \
                tolerance_meters: Some(0.0005), tolerance_relative: Some(0.000001), \
                uv_tolerance: Some(0.01), max_candidate_pairs: Some(50)))), \
                profiles: {\"strict\": (geometry: Some((coincidence_mode: Some(legacy))))})";
    let policy = SourceLintPolicy::from_ron(text).expect("policy");
    let resolved = policy.resolve("default", None).expect("resolve");
    assert_eq!(resolved.geometry.mode, CoincidenceMode::Robust);
    assert_eq!(resolved.geometry.robust.tolerance_meters, 0.0005);
    assert_eq!(resolved.geometry.robust.tolerance_relative, 0.000001);
    assert_eq!(resolved.geometry.robust.uv_tolerance, 0.01);
    assert_eq!(resolved.geometry.robust.max_candidate_pairs, 50);
    assert_eq!(
        policy.resolve("strict", None).expect("strict").geometry.mode,
        CoincidenceMode::Legacy
    );
    let bad = "(format_version: 1, defaults: (geometry: Some((tolerance_meters: Some(-1.0)))))";
    let error = SourceLintPolicy::from_ron(bad)
        .expect("parses")
        .resolve("default", None)
        .expect_err("negative tolerance");
    assert_eq!(error.code(), "content_lint_policy");
    let zero = "(format_version: 1, defaults: (geometry: Some((max_candidate_pairs: Some(0)))))";
    assert!(
        SourceLintPolicy::from_ron(zero)
            .expect("parses")
            .resolve("default", None)
            .is_err()
    );
    // Engine defaults keep the legacy predicate.
    let defaults = SourceLintPolicy::engine_defaults()
        .resolve("default", None)
        .expect("defaults");
    assert_eq!(defaults.geometry.mode, CoincidenceMode::Legacy);
    // Agent settings JSON uses the same names.
    let settings: GltfLintSettings = serde_json::from_value(json!({
        "mode": "robust", "robust": {"tolerance_meters": 0.001, "max_candidate_pairs": 7}
    }))
    .expect("settings json");
    assert_eq!(settings.robust.max_candidate_pairs, 7);
    assert!(serde_json::from_value::<GltfLintSettings>(json!({"robust": {"tolerance": 1}})).is_err());
}

#[test]
fn default_settings_keep_the_legacy_report() {
    let f = Fixture::stretched();
    let legacy = lint(&f, &GltfLintSettings::default());
    assert_eq!(legacy.coincident_triangles.len(), 1);
    assert!(legacy.robust_coincidences.is_empty() && legacy.mesh_evidence.is_empty());
    assert_eq!(legacy.robust_work, Default::default());
    // Robust mode: UV0 is not sampled by the default material, so the 20 µm copy is a
    // tolerance match with equivalent attributes and no safe-delete label.
    let report = lint(&f, &robust());
    let row = &report.robust_coincidences[0];
    assert_eq!(
        (row.agreement, row.equivalence),
        (Agreement::Tolerance, Equivalence::Equivalent)
    );
    assert!(row.uv_sets_compared.is_empty());
    assert!(!row.safe_delete_candidate);
}
