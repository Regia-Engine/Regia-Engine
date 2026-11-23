// SPDX-License-Identifier: MPL-2.0
//! Dispatcher e2e for `terrain.surface_at`, `terrain.mesh_layers`, `terrain.bake_report` and
//! `terrain.blockers` on the cooked Switchbacks fixture (spec 121 § 12, ADR-8386).
//!
//! On the base each call answered `F0008`. These tests require `no_world_geom` (from every method
//! but `terrain.mesh_layers`, which answers `loaded: false` without geometry, ADR-8576 item 6 and
//! ADR-8627) and the rest of the contract.

use std::sync::OnceLock;

use regia_agent::protocol::RpcError;
use regia_agent::terrain_mesh_layers::{TerrainMeshLayers, TerrainMeshLayersLoading};
use regia_agent::{LocalReply, call_local, mentions_token};
use regia_app::prelude::*;
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, CookReport, GroundInput, cook};
use regia_world_geom::cooked::TriKind;
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::{BlockerId, GeomState, SurfaceHit, SurfaceKind, TerrainBakeReport, WorldGeom};
use serde_json::{Value, json};

const MAP: &str = "switchbacks";

/// Spec 121 § 10, in table order. The book page names each one.
const W09: &[&str] = &[
    "render_off_collision",
    "nav_off_floor",
    "mouth_cut_mismatch",
    "junction_open",
    "lane_too_narrow",
    "hull_blocked",
    "drawn_solid_off_wall",
    "hidden_face_in_wall",
    "not_watertight",
    "above_ground_outside_cut",
    "ground_inside_span",
    "seam_gap",
    "portal_blocked",
    "zone_mismatch",
    "ground_vs_render",
    "ground_pitch_seam",
    "ground_over_budget",
    "column_over_max_floors",
    "header_mismatch",
    "lattice_mismatch",
    "drift_triangles",
    "drift_manifest",
    "drift_mouth",
    "drift_portal",
    "spawn_not_flat",
    "determinism_table",
];

struct Cooked {
    geom: WorldGeom,
    report: CookReport,
    bake: TerrainBakeReport,
}

fn switchbacks() -> &'static Cooked {
    static COOKED: OnceLock<Cooked> = OnceLock::new();
    COOKED.get_or_init(|| {
        let passages = PassagesDocument::from_layout(for_map(MAP).expect("switchbacks layout"))
            .to_ron()
            .expect("passages ron");
        let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"), blockers: [(passage: \"gallery\", from_s_m: 40.0, to_s_m: 70.0)])])";
        let read = |path: &str| {
            if path == "map.passages.ron" {
                Ok(passages.as_bytes().to_vec())
            } else {
                Err(format!("{path}: not found"))
            }
        };
        let grid = HeightGrid::square(4, vec![8192; 16]).expect("canyon grid");
        let terrain = TerrainSampler::new(grid, 1280.0).with_zero_counts(16384);
        let ground = GroundInput {
            lattice_n: 4,
            width_m: 1280.0,
            offset_m: -512.0,
            step_m: 1.0 / 32.0,
            counts: vec![8192; 16],
        };
        let run = |document: &str| {
            cook(&CookInputs {
                document: document.as_bytes(),
                ground: &ground,
                terrain: Some(&terrain),
                read_file: &read,
            })
        };
        // The Switchbacks test cook authors ADR-8359's ramp-facet waiver, as the other test cooks
        // do (ADR-8436): the bake checks refuse the two K1 facets otherwise.
        let out = match run(doc) {
            Ok(out) => out,
            Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
                let waived = waive_ramp_facets(doc, &report).expect("ramp-facet waiver");
                run(&waived).expect("switchbacks cook with the waiver")
            }
            Err(err) => panic!("switchbacks cook: {err}"),
        };
        let bytes = out.file.encode().expect("cooked bytes");
        Cooked {
            geom: WorldGeom::from_cooked(&bytes).expect("load cooked geometry"),
            report: out.report,
            bake: out.bake_report,
        }
    })
}

fn loaded(sim_owned: bool, with_report: bool) -> (World, WorldGeom, GeomState) {
    let cooked = switchbacks();
    let state = cooked.geom.new_state();
    let mut world = World::new();
    // A successful `edit_scene` call stamps `agent.changes`, which reads `Time`.
    world.insert_resource(Time::default());
    world.insert_resource(TerrainMeshLayers {
        geom: cooked.geom.clone(),
        state: state.clone(),
        report: with_report.then(|| cooked.report.clone()),
        bake: with_report.then(|| cooked.bake.clone()),
        map: MAP.to_string(),
        sim_owned,
    });
    (world, cooked.geom.clone(), state)
}

fn finish(method: &str, reply: LocalReply) -> Result<Value, RpcError> {
    match reply {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("{method} answers in the call"),
    }
}

fn must_ok(world: &mut World, method: &str, params: Value) -> Value {
    match finish(method, call_local(world, method, params, None)) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn must_code(world: &mut World, method: &str, params: Value) -> String {
    match finish(method, call_local(world, method, params, None)) {
        Ok(value) => panic!("{method} succeeded: {value}"),
        Err(err) => err
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

fn assert_f32(got: &Value, expected: f32, what: &str) {
    let number = got
        .as_f64()
        .unwrap_or_else(|| panic!("{what} is not a number: {got}"));
    assert_eq!(
        number.to_bits(),
        f64::from(expected).to_bits(),
        "{what}: {number} vs {expected}"
    );
}

fn assert_f32_3(got: &Value, expected: [f32; 3], what: &str) {
    let items = got
        .as_array()
        .unwrap_or_else(|| panic!("{what} is not an array: {got}"));
    assert_eq!(items.len(), 3, "{what}");
    for (item, expected) in items.iter().zip(expected) {
        assert_f32(item, expected, what);
    }
}

fn kind_name(kind: SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Ground => "ground",
        SurfaceKind::Floor => "floor",
        SurfaceKind::Roof => "roof",
        SurfaceKind::Wall => "wall",
        SurfaceKind::Blocker => "blocker",
    }
}

fn assert_hit(got: &Value, hit: &SurfaceHit) {
    assert_f32(&got["y"], hit.y, "y");
    assert_f32_3(&got["normal"], hit.normal, "normal");
    assert_eq!(got["kind"], kind_name(hit.kind));
    assert_eq!(got["material"].as_u64(), Some(u64::from(hit.material)));
    match hit.section {
        Some(id) => assert_eq!(got["section"].as_u64(), Some(u64::from(id.0))),
        None => assert!(got["section"].is_null(), "{got}"),
    }
    match hit.tri {
        Some(tri) => {
            assert_eq!(got["tri"]["layer"].as_u64(), Some(u64::from(tri.layer)));
            assert_eq!(got["tri"]["triangle"].as_u64(), Some(u64::from(tri.triangle)));
        }
        None => assert!(got["tri"].is_null(), "{got}"),
    }
}

fn assert_opt_hit(got: &Value, hit: Option<SurfaceHit>) {
    match hit {
        None => assert!(got.is_null(), "{got}"),
        Some(hit) => assert_hit(got, &hit),
    }
}

/// Same key walk as `contract::assert_result_named` (that helper is crate-private).
fn assert_result_named(method: &str, answer: &Value) {
    fn named(text: &str, key: &str) -> bool {
        let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
        text.match_indices(key).any(|(at, _)| {
            !text[..at].chars().next_back().is_some_and(ident)
                && !text[at + key.len()..].chars().next().is_some_and(ident)
        })
    }
    fn walk(text: &str, value: &Value, path: &str, missing: &mut Vec<String>) {
        let Value::Object(map) = value else {
            return;
        };
        for (key, inner) in map {
            if !named(text, key) {
                missing.push(format!("{path}{key}"));
            }
            if text.contains(&format!("{key}: {{")) {
                walk(text, inner, &format!("{path}{key}."), missing);
            }
            if text.contains(&format!("{key}: [{{"))
                && let Some(first) = inner.as_array().and_then(|items| items.first())
            {
                walk(text, first, &format!("{path}{key}[0]."), missing);
            }
        }
    }
    let text = regia_agent::schema::method(method).expect(method).result;
    let mut missing = Vec::new();
    walk(text, answer, "", &mut missing);
    assert!(
        missing.is_empty(),
        "{method} keys {missing:?} are absent from {text}"
    );
}

fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// Floor-triangle grade. The literal 1.5 is the K1 cut; this does not read the handler's constant.
fn grade(v: [[f32; 3]; 3]) -> Option<f32> {
    let ax = v[1][0] - v[0][0];
    let ay = v[1][1] - v[0][1];
    let az = v[1][2] - v[0][2];
    let bx = v[2][0] - v[0][0];
    let by = v[2][1] - v[0][1];
    let bz = v[2][2] - v[0][2];
    let nx = ay * bz - az * by;
    let ny = az * bx - ax * bz;
    let nz = ax * by - ay * bx;
    let horiz2 = nx * nx + nz * nz;
    if !horiz2.is_finite() || !ny.is_finite() {
        return None;
    }
    let vert = ny.abs();
    if vert < 1.0e-6 {
        return None;
    }
    let slope = horiz2.sqrt() / vert;
    slope.is_finite().then_some(slope)
}

fn k1_facets(geom: &WorldGeom) -> Vec<Value> {
    const MIN_SLOPE: f32 = 1.5;
    let mut rows = Vec::new();
    for tri in geom.triangles() {
        if tri.kind != TriKind::Floor {
            continue;
        }
        let Some(slope) = grade(tri.v) else {
            continue;
        };
        if slope < MIN_SLOPE {
            continue;
        }
        let x = (tri.v[0][0] + tri.v[1][0] + tri.v[2][0]) / 3.0;
        let y = (tri.v[0][1] + tri.v[1][1] + tri.v[2][1]) / 3.0;
        let z = (tri.v[0][2] + tri.v[1][2] + tri.v[2][2]) / 3.0;
        if x.is_finite() && y.is_finite() && z.is_finite() {
            rows.push(json!({
                "x": x,
                "y": y,
                "z": z,
                "slope": slope,
                "section": tri.section,
            }));
        }
    }
    rows
}

fn facet_brief(facets: &[Value]) -> String {
    let mut parts = Vec::new();
    for row in facets.iter().take(12) {
        parts.push(format!("({}, {}, slope {})", row["x"], row["z"], row["slope"]));
    }
    if facets.len() > 12 {
        parts.push(format!("and {} more", facets.len() - 12));
    }
    if parts.is_empty() {
        "(none)".to_string()
    } else {
        parts.join("; ")
    }
}

fn steepest_floors(geom: &WorldGeom) -> String {
    let mut rows = Vec::new();
    for tri in geom.triangles() {
        if tri.kind != TriKind::Floor {
            continue;
        }
        let Some(slope) = grade(tri.v) else {
            continue;
        };
        let x = (tri.v[0][0] + tri.v[1][0] + tri.v[2][0]) / 3.0;
        let z = (tri.v[0][2] + tri.v[1][2] + tri.v[2][2]) / 3.0;
        rows.push((slope, x, z));
    }
    rows.sort_by(|left, right| right.0.partial_cmp(&left.0).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(8);
    if rows.is_empty() {
        return "(no finite floor grade)".to_string();
    }
    rows.iter()
        .map(|(slope, x, z)| format!("({x}, {z}, slope {slope})"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn near_xz(facets: &[Value], x: f64, z: f64) -> bool {
    facets.iter().any(|row| {
        let Some(fx) = row.get("x").and_then(Value::as_f64) else {
            return false;
        };
        let Some(fz) = row.get("z").and_then(Value::as_f64) else {
            return false;
        };
        let dx = fx - x;
        let dz = fz - z;
        dx * dx + dz * dz <= 900.0
    })
}

fn floor_probe(geom: &WorldGeom) -> (f32, f32, f32) {
    let tri = geom
        .triangles()
        .into_iter()
        .find(|tri| tri.kind == TriKind::Floor && tri.section.is_some())
        .expect("a sectioned floor triangle");
    let x = (tri.v[0][0] + tri.v[1][0] + tri.v[2][0]) / 3.0;
    let y = (tri.v[0][1] + tri.v[1][1] + tri.v[2][1]) / 3.0;
    let z = (tri.v[0][2] + tri.v[1][2] + tri.v[2][2]) / 3.0;
    (x, z, y)
}

#[test]
fn terrain_surface_at_answers_no_world_geom_without_geometry() {
    let mut world = World::new();
    let code = must_code(
        &mut world,
        "terrain.surface_at",
        json!({ "points": [[0.0, 0.0, 1.0]] }),
    );
    assert_eq!(code, "no_world_geom");
}

/// ADR-8576 item 6 (ADR-8627): one `terrain.mesh_layers`; without geometry it answers
/// `loaded: false` (level-cook's contract), not `no_world_geom`. A map or section id cannot be
/// checked against nothing, so neither is refused.
#[test]
fn terrain_mesh_layers_answers_loaded_false_without_geometry() {
    let mut world = World::new();
    for params in [json!({}), json!({ "map": "other" }), json!({ "section": 3 })] {
        let reply = must_ok(&mut world, "terrain.mesh_layers", params.clone());
        assert_result_named("terrain.mesh_layers", &reply);
        assert_eq!(reply["loaded"], false, "{params}");
        assert!(reply["map"].is_null(), "{reply}");
        assert!(reply["geom_generation"].is_null(), "{reply}");
        assert!(reply["content_hash"].is_null(), "{reply}");
        assert_eq!(reply["chunks"], 0);
        assert_eq!(reply["sections"], 0);
        for key in [
            "layers",
            "mouths",
            "layer_reports",
            "mouth_reports",
            "section_rows",
            "portal_rows",
            "blocker_rows",
        ] {
            assert_eq!(reply[key], json!([]), "{key}");
        }
    }
}

#[test]
fn terrain_mesh_layers_answers_layers_loading_before_loaded_false() {
    let mut world = World::new();
    world.insert_resource(TerrainMeshLayersLoading);
    assert_eq!(
        must_code(&mut world, "terrain.mesh_layers", json!({})),
        "layers_loading"
    );
}

#[test]
fn terrain_bake_report_answers_no_world_geom_without_geometry() {
    let mut world = World::new();
    assert_eq!(
        must_code(&mut world, "terrain.bake_report", json!({})),
        "no_world_geom"
    );
}

#[test]
fn terrain_blockers_answers_no_world_geom_without_geometry() {
    let mut world = World::new();
    assert_eq!(
        must_code(&mut world, "terrain.blockers", json!({})),
        "no_world_geom"
    );
}

#[test]
fn terrain_surface_at_answers_layers_loading_before_no_world_geom() {
    let mut world = World::new();
    world.insert_resource(TerrainMeshLayersLoading);
    let code = must_code(
        &mut world,
        "terrain.surface_at",
        json!({ "points": [[0.0, 0.0, 1.0]] }),
    );
    assert_eq!(code, "layers_loading");
}

#[test]
fn terrain_surface_at_answers_bad_point() {
    let (mut world, _, _) = loaded(false, true);
    assert_eq!(
        must_code(&mut world, "terrain.surface_at", json!({})),
        "bad_point"
    );
    assert_eq!(
        must_code(&mut world, "terrain.surface_at", json!({ "points": [null] })),
        "bad_point"
    );
    // A wrong-typed `points` is refused by the schema type check (F0002) before the reader runs.
    assert_eq!(
        must_code(&mut world, "terrain.surface_at", json!({ "points": "0,0,1" })),
        "F0002"
    );
    assert_eq!(
        must_code(
            &mut world,
            "terrain.surface_at",
            json!({ "points": [["nope", 0.0, 1.0]] })
        ),
        "bad_point"
    );
}

#[test]
fn terrain_surface_at_answers_non_finite() {
    let (mut world, _, _) = loaded(false, true);
    let code = must_code(
        &mut world,
        "terrain.surface_at",
        json!({ "points": [["NaN", 0.0, 1.0]] }),
    );
    assert_eq!(code, "non_finite");
}

#[test]
fn terrain_surface_at_answers_too_many_points() {
    let (mut world, _, _) = loaded(false, true);
    let points = vec![[0.0, 0.0, 1.0]; 65];
    let code = must_code(&mut world, "terrain.surface_at", json!({ "points": points }));
    assert_eq!(code, "too_many_points");
}

#[test]
fn terrain_surface_at_answers_out_of_map() {
    let (mut world, _, _) = loaded(false, true);
    let code = must_code(
        &mut world,
        "terrain.surface_at",
        json!({ "points": [[1.0e6, 0.0, 1.0]] }),
    );
    assert_eq!(code, "out_of_map");
}

#[test]
fn terrain_surface_at_answers_unknown_section() {
    let (mut world, _, _) = loaded(false, true);
    let code = must_code(
        &mut world,
        "terrain.surface_at",
        json!({ "points": [[0.0, 0.0, 1.0]], "section": 65_000 }),
    );
    assert_eq!(code, "unknown_section");
}

#[test]
fn terrain_bake_report_answers_no_bake_report() {
    let (mut world, _, _) = loaded(false, false);
    assert_eq!(
        must_code(&mut world, "terrain.bake_report", json!({})),
        "no_bake_report"
    );
}

#[test]
fn terrain_mesh_layers_answers_f0003_for_another_map() {
    let (mut world, _, _) = loaded(false, true);
    assert_eq!(
        must_code(&mut world, "terrain.mesh_layers", json!({ "map": "other" })),
        "F0003"
    );
}

#[test]
fn terrain_blockers_answers_blocker_unknown() {
    let (mut world, _, _) = loaded(false, true);
    let code = must_code(
        &mut world,
        "terrain.blockers",
        json!({ "set": 40_000, "active": false }),
    );
    assert_eq!(code, "blocker_unknown");
}

#[test]
fn terrain_blockers_answers_blocker_sim_owned_without_a_flip() {
    let (mut world, _, _) = loaded(true, true);
    let before = must_ok(&mut world, "terrain.blockers", json!({}));
    assert_eq!(before["applied"], false);
    let code = must_code(
        &mut world,
        "terrain.blockers",
        json!({ "set": 40_000, "active": true }),
    );
    assert_eq!(code, "blocker_sim_owned");
    let after = must_ok(&mut world, "terrain.blockers", json!({}));
    assert_eq!(after["generation"], before["generation"]);
    assert_eq!(after["blockers"], before["blockers"]);
}

#[test]
fn terrain_surface_at_returns_the_callers_support_span() {
    let (mut world, geom, state) = loaded(false, true);
    let (x, z, y) = floor_probe(&geom);
    let reply = must_ok(
        &mut world,
        "terrain.surface_at",
        json!({ "points": [[x, z, y]], "tolerance_m": 0.0, "map": MAP }),
    );
    assert_result_named("terrain.surface_at", &reply);
    assert_eq!(reply["generation"].as_u64(), Some(state.generation().0));
    let row = &reply["results"][0];
    assert_f32_3(&row["at"], [x, z, y], "at");
    let support = geom.support(&state, x, z, y);
    let surface = geom.surface_at(&state, x, z, y, 0.0);
    assert_hit(&row["support"]["floor"], &support.floor);
    assert_opt_hit(&row["support"]["ceiling"], support.ceiling);
    assert_opt_hit(&row["under"], surface.under);
    assert_opt_hit(&row["over"], surface.over);
    match support.floor.section {
        Some(id) => assert_eq!(row["section"].as_u64(), Some(u64::from(id.0))),
        None => assert!(row["section"].is_null(), "{row}"),
    }
    let higher = y + 0.9;
    let raised = must_ok(
        &mut world,
        "terrain.surface_at",
        json!({ "points": [[x, z, higher]] }),
    );
    assert_f32(&raised["results"][0]["at"][2], higher, "caller height");
    let section = geom.sections().first().expect("a cooked section").id;
    let checked = must_ok(
        &mut world,
        "terrain.surface_at",
        json!({ "points": [[x, z, y]], "section": section }),
    );
    assert_eq!(
        checked["results"], reply["results"],
        "section validates and does not filter"
    );
}

#[test]
fn terrain_mesh_layers_lists_layers_sections_generation_and_hash() {
    let (mut world, geom, state) = loaded(false, true);
    let reply = must_ok(&mut world, "terrain.mesh_layers", json!({}));
    assert_result_named("terrain.mesh_layers", &reply);
    assert_eq!(reply["loaded"], true);
    assert_eq!(reply["map"], MAP);
    // ADR-8627: the resource's blocker generation is `geom_generation`; `generation` is the level
    // component's (0 here: no level component is spawned).
    assert_eq!(reply["geom_generation"].as_u64(), Some(state.generation().0));
    assert_eq!(reply["generation"], 0);
    assert_eq!(reply["content_hash"], hex32(&geom.content_hash()));
    assert_eq!(reply["resident_bytes"].as_u64(), Some(geom.resident_bytes()));
    // ADR-8627: `chunks` is the level's chunk count (no level component: 0), not an array.
    assert_eq!(reply["chunks"], 0);
    assert_eq!(
        reply["layers"],
        json!([]),
        "no level component: no presentation layers"
    );
    assert_eq!(reply["sections"].as_u64(), Some(geom.sections().len() as u64));
    assert_eq!(reply["portals"].as_u64(), Some(geom.portals().len() as u64));
    assert_eq!(
        reply["blockers"].as_u64(),
        Some(state.blockers(&geom).count() as u64)
    );
    let report = &switchbacks().report;
    let layers = reply["layer_reports"].as_array().expect("layer_reports");
    assert_eq!(layers.len(), report.layers.len());
    for (row, layer) in layers.iter().zip(&report.layers) {
        assert_eq!(row["id"], layer.id);
        assert_eq!(
            row["welded_vertices"].as_u64(),
            Some(u64::from(layer.welded_vertices))
        );
        assert_eq!(
            row["trimmed_triangles"].as_u64(),
            Some(u64::from(layer.trimmed_triangles))
        );
    }
    let sections = reply["section_rows"].as_array().expect("section_rows");
    assert_eq!(sections.len(), geom.sections().len());
    for (row, section) in sections.iter().zip(geom.sections()) {
        assert_eq!(row["id"].as_u64(), Some(u64::from(section.id)));
        assert_eq!(row["source"], section.source);
    }
    let blockers = reply["blocker_rows"].as_array().expect("blocker_rows");
    let expect: Vec<_> = state.blockers(&geom).collect();
    assert_eq!(blockers.len(), expect.len());
    assert!(!expect.is_empty(), "the gallery plug is cooked");
    for (row, status) in blockers.iter().zip(&expect) {
        assert_eq!(row["id"].as_u64(), Some(u64::from(status.id.0)));
        assert_eq!(row["section"].as_u64(), Some(u64::from(status.section.0)));
        assert_eq!(row["active"], status.active);
    }
}

#[test]
fn terrain_bake_report_lists_checks_conditioning_and_k1_facets() {
    let (mut world, geom, _) = loaded(false, true);
    let reply = must_ok(&mut world, "terrain.bake_report", json!({ "map": MAP }));
    assert_result_named("terrain.bake_report", &reply);
    // ADR-8576 item 6 (ADR-8627): the rows are the cook's `TerrainBakeReport`, as
    // `regia terrain check --json` prints them; they no longer stand in failed at `input_missing`.
    let bake = &switchbacks().bake;
    assert_eq!(reply["checks"], serde_json::to_value(&bake.checks).expect("rows"));
    assert_eq!(reply["passed"], bake.passed);
    assert_eq!(
        reply["passed"], true,
        "the cook succeeded, so no unwaived failure"
    );
    assert!(reply["failure"].is_null(), "{}", reply["failure"]);
    assert_eq!(reply["waived"].as_u64(), Some(u64::from(bake.waived)));
    assert_eq!(reply["ground_mode"], bake.ground_mode);
    assert_eq!(reply["memory_bytes"].as_u64(), Some(bake.memory_bytes));
    assert_eq!(reply["report_content_hash"], hex32(&geom.content_hash()));
    let checks = reply["checks"].as_array().expect("checks");
    for code in W09 {
        assert!(
            checks.iter().any(|row| row["code"] == *code),
            "spec 121 § 10 check {code} has a row"
        );
    }
    for row in checks {
        assert!(
            row["passed"] == true || row["waived"] == true,
            "unwaived failure in a cook that succeeded: {row}"
        );
    }
    // The waiver this fixture authors is the K1 ramp facets' (ADR-8359): every waived row is one.
    let waived: Vec<_> = checks.iter().filter(|row| row["waived"] == true).collect();
    assert_eq!(waived.len() as u64, u64::from(bake.waived));
    for row in &waived {
        assert_eq!(row["code"], "ramp_facet_slope_over_limit", "{row}");
    }
    let report = &switchbacks().report;
    let welded: u64 = report
        .layers
        .iter()
        .map(|layer| u64::from(layer.welded_vertices))
        .sum();
    let trimmed: u64 = report
        .layers
        .iter()
        .map(|layer| u64::from(layer.trimmed_triangles))
        .sum();
    assert_eq!(reply["welded_vertices"].as_u64(), Some(welded));
    assert_eq!(reply["trimmed_triangles"].as_u64(), Some(trimmed));
    let facets = reply["facets"].as_array().expect("facets");
    assert_eq!(facets, &k1_facets(&geom));
    let east = near_xz(facets, 175.0, 92.0);
    let west = near_xz(facets, -173.0, 82.0);
    assert!(
        east && west,
        "K1 near (175, 92): {east}; near (-173, 82): {west}; facets: {}; steepest: {}",
        facet_brief(facets),
        steepest_floors(&geom),
    );
}

#[test]
fn terrain_blockers_lists_plugs_and_a_flip_bumps_one_generation() {
    let (mut world, geom, state) = loaded(false, true);
    let before = must_ok(&mut world, "terrain.blockers", json!({}));
    assert_result_named("terrain.blockers", &before);
    assert_eq!(before["applied"], false);
    assert!(before.get("set").is_none(), "a read does not echo set");
    let rows = before["blockers"].as_array().expect("blockers");
    assert!(!rows.is_empty(), "the gallery plug is cooked");
    let id = rows[0]["id"].as_u64().expect("plug id");
    let active = rows[0]["active"].as_bool().expect("active");
    let reply = must_ok(
        &mut world,
        "terrain.blockers",
        json!({ "set": id, "active": !active }),
    );
    assert_eq!(reply["applied"], true);
    assert_eq!(reply["set"].as_u64(), Some(id));
    assert_eq!(reply["active"], !active);
    let plug = BlockerId(u16::try_from(id).expect("plug id fits u16"));
    let mut local = state.clone();
    let generation = local.set_blocker(&geom, plug, !active).expect("flip");
    assert_eq!(reply["generation"].as_u64(), Some(generation.0));
    let changes = world
        .get_resource::<regia_agent::ChangeLog>()
        .expect("change log");
    assert_eq!(changes.records.len(), 1, "the flip is one authored change");
    assert_eq!(changes.records[0].method, "terrain.blockers");
    let flipped = reply["blockers"]
        .as_array()
        .expect("blockers")
        .iter()
        .find(|row| row["id"].as_u64() == Some(id))
        .expect("the flipped plug is still listed");
    assert_eq!(flipped["active"], !active);
    let again = must_ok(
        &mut world,
        "terrain.blockers",
        json!({ "set": id, "active": !active }),
    );
    assert_eq!(again["applied"], false);
    assert_eq!(again["generation"].as_u64(), Some(generation.0));
    assert_eq!(
        world
            .get_resource::<regia_agent::ChangeLog>()
            .expect("change log")
            .records
            .len(),
        1,
        "a no-op flip is not a second change"
    );
    assert_eq!(
        must_code(&mut world, "terrain.blockers", json!({ "set": id })),
        "F0002"
    );
}

/// ADR-8627: with both the level's component (chunks, stream) and the agent resource (cook rows,
/// blocker state), one answer carries both: presentation keys from the component, geometry keys
/// and rows from the resource, `geom_generation` follows a blocker flip, `section` filters
/// `blocker_rows`, and an unknown section or another map is refused.
#[test]
fn terrain_mesh_layers_joins_the_level_component_and_the_agent_resource() {
    use regia_terrain::{
        CdlodConfig, CookedChunkLod, CookedLayer, CookedLayerChunk, LayerChunkGrid,
        TerrainMeshLayers as LevelMeshLayers, TerrainMeshLayersCooked,
    };
    let (mut world, geom, state) = loaded(false, true);
    let c = CdlodConfig::default();
    let presentation = TerrainMeshLayersCooked {
        version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
        chunk_importer: 0,
        grid: LayerChunkGrid::new(1024.0, 6, c.grid_size, c.range_factor, [0.0, 0.0], 64.0),
        layers: vec![CookedLayer {
            id: "tubes".into(),
            map_layer: 1,
            cast_shadows: true,
            collider: false,
            material: String::new(),
            material_id: None,
            chunks: (0..2)
                .map(|x| CookedLayerChunk {
                    x,
                    z: 0,
                    bounds: [[0.0; 3], [1.0; 3]],
                    lods: vec![CookedChunkLod {
                        mesh: regia_asset::AssetId::sub_asset(
                            regia_asset::AssetId::default(),
                            &format!("{x}/0"),
                        ),
                        triangles: 7,
                        bytes: 64,
                        error_m: 0.0,
                    }],
                })
                .collect(),
        }],
    };
    world.spawn(
        LevelMeshLayers::with_geom(std::sync::Arc::new(geom.clone())).with_presentation(presentation, None),
    );
    let reply = must_ok(&mut world, "terrain.mesh_layers", json!({ "map": MAP }));
    assert_result_named("terrain.mesh_layers", &reply);
    assert_eq!(reply["loaded"], true);
    assert_eq!(reply["map"], MAP);
    assert_eq!(reply["chunks"], 2);
    assert_eq!(reply["layers"][0]["id"], "tubes");
    assert_eq!(reply["layers"][0]["triangles"], 14);
    assert_eq!(reply["layers"][0]["cast_shadows"], true);
    assert_eq!(
        reply["layers"][0]["collider"], false,
        "the cooked collider flag (ADR-8872)"
    );
    assert!(reply["grid"].is_object(), "{}", reply["grid"]);
    assert_eq!(
        reply["generation"], 1,
        "with_geom starts the level generation at 1"
    );
    assert_eq!(reply["content_hash"], hex32(&geom.content_hash()));
    assert_eq!(reply["sections"].as_u64(), Some(geom.sections().len() as u64));
    let rows = reply["blocker_rows"].as_array().expect("blocker_rows").clone();
    assert!(!rows.is_empty(), "the gallery plug is cooked");
    assert_eq!(reply["geom_generation"].as_u64(), Some(state.generation().0));

    // `section` filters the blocker rows to that section's plugs.
    let plug_section = rows[0]["section"].as_u64().expect("plug section");
    let filtered = must_ok(
        &mut world,
        "terrain.mesh_layers",
        json!({ "section": plug_section }),
    );
    let only = filtered["blocker_rows"].as_array().expect("blocker_rows");
    assert!(!only.is_empty());
    assert!(
        only.iter()
            .all(|row| row["section"].as_u64() == Some(plug_section))
    );
    let other = geom
        .sections()
        .iter()
        .map(|s| u64::from(s.id))
        .find(|id| rows.iter().all(|row| row["section"].as_u64() != Some(*id)))
        .expect("a section without a plug");
    let none = must_ok(&mut world, "terrain.mesh_layers", json!({ "section": other }));
    assert_eq!(none["blocker_rows"], json!([]));
    assert_eq!(
        none["section_rows"], reply["section_rows"],
        "section filters only blockers"
    );

    // A blocker flip moves `geom_generation`, not the level's `generation`.
    let id = rows[0]["id"].as_u64().expect("plug id");
    let active = rows[0]["active"].as_bool().expect("active");
    let flip = must_ok(
        &mut world,
        "terrain.blockers",
        json!({ "set": id, "active": !active }),
    );
    let after = must_ok(&mut world, "terrain.mesh_layers", json!({}));
    assert_eq!(after["geom_generation"], flip["generation"]);
    assert_ne!(after["geom_generation"], reply["geom_generation"]);
    assert_eq!(after["generation"], reply["generation"]);
    assert_eq!(after["blocker_rows"][0]["active"], !active);

    assert_eq!(
        must_code(&mut world, "terrain.mesh_layers", json!({ "section": 40_000 })),
        "unknown_section"
    );
    assert_eq!(
        must_code(&mut world, "terrain.mesh_layers", json!({ "map": "other" })),
        "F0003"
    );
}

/// The level component alone (a level loaded its `<map>.meshlayers`, no agent resource): loaded,
/// with the counts and hash from the component's geometry and empty resource rows.
#[test]
fn terrain_mesh_layers_reads_the_level_component_without_the_agent_resource() {
    let geom = switchbacks().geom.clone();
    let mut world = World::new();
    world.spawn(regia_terrain::TerrainMeshLayers::with_geom(std::sync::Arc::new(
        geom.clone(),
    )));
    let reply = must_ok(&mut world, "terrain.mesh_layers", json!({}));
    assert_result_named("terrain.mesh_layers", &reply);
    assert_eq!(reply["loaded"], true);
    assert!(reply["map"].is_null());
    assert!(reply["geom_generation"].is_null());
    assert_eq!(reply["content_hash"], hex32(&geom.content_hash()));
    assert_eq!(reply["resident_bytes"].as_u64(), Some(geom.resident_bytes()));
    assert_eq!(reply["sections"].as_u64(), Some(geom.sections().len() as u64));
    assert_eq!(reply["section_rows"], json!([]));
    assert_eq!(reply["blocker_rows"], json!([]));
    let first = geom.sections().first().expect("a section").id;
    must_ok(&mut world, "terrain.mesh_layers", json!({ "section": first }));
    assert_eq!(
        must_code(&mut world, "terrain.mesh_layers", json!({ "section": 40_000 })),
        "unknown_section"
    );
}

#[test]
fn the_book_page_names_every_method_parameter_and_error() {
    let page = include_str!("../../../book/agents/terrain-mesh-layers.md");
    for method in [
        "terrain.surface_at",
        "terrain.mesh_layers",
        "terrain.bake_report",
        "terrain.blockers",
    ] {
        assert!(mentions_token(page, method), "{method}");
        let schema = regia_agent::schema::method(method).expect(method);
        for param in schema.params {
            assert!(mentions_token(page, param.name), "{method} param {}", param.name);
        }
        let contract = regia_agent::contract::contract(method).expect(method);
        for code in contract.errors {
            if code.starts_with('F') {
                continue;
            }
            assert!(mentions_token(page, code), "{method} error {code}");
        }
    }
    for code in W09 {
        assert!(mentions_token(page, code), "check {code}");
    }
    assert!(page.contains("0.9"), "the waist reach is the caller's");
    assert!(mentions_token(page, "input_missing"));
    assert!(mentions_token(page, "chunks"));
}
