// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (tools/tunnels_v2/test_tunnel_bake.py, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-281, mark: as-is (Stage A: unchanged).
// Edits: pytest cases re-expressed as Rust tests, assertions kept (fixture description = DO27 data, P3); `monkeypatch` of the cut builder becomes `bake_unchecked` + a shifted cut + `finish`; the absent handoff fixture becomes a one-passage `do27-tunnels/1` export of the Switchbacks tube; the tampered vertex is the tenth on the tube floor (the mesher's vertex order differs); sha256 becomes BLAKE3; DO27 file and engine names dropped; plan row W-07, ADR-8175.

//! The T1 baker's cases (plan row W-07): one description bakes render, collision, mouth cut and nav, and the bake
//! fails when any two disagree.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::TunnelLayout;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::tunnel_bake::{
    self as tb, BakeMesh, DescriptionError, FindingStatus, TunnelBakeError, bake, bake_to_dir,
    bake_unchecked, canonical_bytes, check_consistency, finish, validate_description,
};
use serde_json::{Value, json};

/// A fresh temporary directory (pid + counter), never a fixed name.
fn unique_temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One curved tube, one straight gallery, one junction, one ramp mouth.
fn fixture_desc() -> Value {
    json!({
        "passages": [
            {"id": "main", "kind": "tube", "width_m": 22.0, "ceiling_m": 12.0,
             "level": "tube_1",
             "points": [[-60.0, -40.0, 0.0], [0.0, -40.0, 40.0], [60.0, -40.0, 0.0]]},
            {"id": "spur", "kind": "gallery", "width_m": 18.0, "ceiling_m": 10.0,
             "points": [[0.0, -40.0, 40.0], [0.0, -40.0, 110.0]]},
            {"id": "ramp_w", "kind": "ramp", "width_m": 20.0, "ceiling_m": 12.0,
             "points": [[-60.0, -40.0, 0.0], [-95.0, -40.0, 25.0], [-110.0, -5.0, 45.0]]},
        ],
        "openings": [
            {"id": "west", "kind": "fracture_ramp", "ramp": "ramp_w", "top": [-110.0, -5.0, 45.0]},
        ],
        "junctions": [
            {"id": "core", "at": [0.0, -40.0, 40.0], "passages": ["main", "spur"]},
        ],
        "sample_step_m": 5.0,
        "looks": {"audio_preset": "tube", "floor_roughness": 0.8},
        "gameplay": {"ai_cost_scale": 0.75},
    })
}

fn bake_ok(desc: &Value) -> tb::Baked {
    bake(desc, None, true).unwrap()
}

#[test]
fn crawlway_is_pilot_only_and_absent_from_the_stock_bake() {
    let stock = bake_ok(&fixture_desc());
    assert!(stock.description.crawlways.is_empty());
    let stock_json: Value = serde_json::from_slice(&canonical_bytes(&stock)).unwrap();
    assert!(stock_json["description"].get("crawlways").is_none());
    assert!(stock.nav.nodes.iter().all(|n| !n.pilot_only));
    assert!(stock.nav.edges.iter().all(|e| !e.pilot_only));
    assert!(
        stock_json["nav"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n.get("pilot_only").is_none())
    );

    let mut desc = fixture_desc();
    desc["crawlways"] = json!([{
        "kind": "crawlway",
        "id": "flank",
        "points": [[0.0, 10.0, 40.0], [20.0, 10.0, 40.0]],
        "width_m": 1.4,
        "height_m": 2.2,
        "seed": 3,
    }]);
    let baked = bake_ok(&desc);
    assert!(baked.report.pass);
    assert_eq!(baked.description.crawlways[0].kind, "crawlway");
    let nodes: Vec<&str> = baked
        .nav
        .nodes
        .iter()
        .filter(|n| n.pilot_only)
        .map(|n| n.id.as_str())
        .collect();
    let edges: Vec<_> = baked.nav.edges.iter().filter(|e| e.pilot_only).collect();
    assert_eq!(nodes, ["flank:0", "flank:1"]);
    assert!(edges.len() == 1 && edges[0].cost_mm > 0);
    assert_eq!(baked.collision_mesh.faces.len(), stock.collision_mesh.faces.len());
    assert_eq!(canonical_bytes(&bake_ok(&desc)), canonical_bytes(&baked));

    let mut wide = fixture_desc();
    wide["crawlways"] = json!([{
        "kind": "crawlway",
        "id": "wide",
        "points": [[0.0, 10.0, 0.0], [10.0, 10.0, 0.0]],
        "width_m": 8.0,
        "height_m": 2.2,
    }]);
    assert!(validate_description(&wide).is_err());
}

#[test]
fn fixture_bakes_deterministically() {
    let (a, b) = (unique_temp_dir("tunnel-bake-a"), unique_temp_dir("tunnel-bake-b"));
    let first = bake_to_dir(&fixture_desc(), &a, None).unwrap();
    let second = bake_to_dir(&fixture_desc(), &b, None).unwrap();
    let names = |paths: &[PathBuf]| -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(names(&first), names(&second));
    assert_eq!(names(&first), tb::FILES);
    for (pa, pb) in first.iter().zip(&second) {
        assert_eq!(
            blake3::hash(&std::fs::read(pa).unwrap()),
            blake3::hash(&std::fs::read(pb).unwrap()),
            "{}",
            pa.display()
        );
    }
    let baked = bake_ok(&fixture_desc());
    assert_eq!(
        canonical_bytes(&bake_ok(&fixture_desc())),
        canonical_bytes(&baked)
    );
    let _ = std::fs::remove_dir_all(a);
    let _ = std::fs::remove_dir_all(b);
}

#[test]
fn render_collision_nav_agree() {
    let baked = bake_ok(&fixture_desc());
    assert!(baked.report.pass);
    let mut rules: Vec<&str> = baked.report.findings.iter().map(|f| f.rule.as_str()).collect();
    rules.sort_unstable();
    assert_eq!(
        rules,
        [
            "collision_inside_render",
            "junctions_join_members",
            "mouth_cut_matches_collision",
            "nav_on_collision_floor",
            "render_covers_floor",
        ]
    );
    assert!(!baked.render_mesh.vertices.is_empty() && !baked.collision_mesh.vertices.is_empty());
    assert!(!baked.render_mesh.faces.is_empty() && !baked.collision_mesh.faces.is_empty());
    assert!(baked.terrain_cut.cuts.len() == 1 && baked.terrain_cut.portals.len() == 1);
    assert!(baked.nav.nodes.len() > 3 && !baked.nav.edges.is_empty());
    assert!(baked.nav.edges.iter().all(|e| e.cost_mm >= 0));
    let passage_edges: Vec<_> = baked
        .nav
        .edges
        .iter()
        .filter(|e| e.a.contains(':') && e.b.contains(':'))
        .collect();
    assert!(!passage_edges.is_empty() && passage_edges.iter().all(|e| e.cost_mm > 0));
}

#[test]
fn fixture_covers_straight_curved_junction_mouth() {
    let baked = bake_ok(&fixture_desc());
    let by_id = |id: &str| baked.sampled.passage(id).unwrap();
    assert!(
        by_id("main").samples.len() >= 3,
        "curved tube sampled along its arc"
    );
    assert!(by_id("spur").samples.len() >= 3, "straight gallery");
    // The junction opens walls on both members where they meet.
    let main = by_id("main");
    assert!(!main.wall_left.iter().all(|w| *w) || !main.wall_right.iter().all(|w| *w));
    let spur = by_id("spur");
    assert!(!spur.wall_left.iter().chain(&spur.wall_right).all(|w| *w));
    // The ramp mouth stays open and ends at the opening top.
    let ramp = by_id("ramp_w");
    assert!(!ramp.cap_end);
    let end = ramp.samples.last().unwrap().p;
    for (got, want) in end.iter().zip([-110.0f32, -5.0, 45.0]) {
        assert!((got - want).abs() <= 1.0, "{end:?}");
    }
    // The ramp climbs from the tube floor to the surface.
    assert!((ramp.samples[0].p[1] - -40.0).abs() < 1e-6);
    let portal = &baked.terrain_cut.portals[0];
    assert!(portal.opening == "west" && portal.width_mm == 20_000);
}

/// The handoff fixture: DO27's `do27-tunnels/1` export of one passage (the Switchbacks tube alone; the fixture the
/// baker's test reads is not at the tag, so it is generated here from the armed layout, never forked).
fn handoff_fixture() -> Value {
    let mut layout: TunnelLayout = for_map("switchbacks").unwrap();
    layout.passages.truncate(1);
    layout.openings.clear();
    layout.junctions.clear();
    serde_json::from_str(&layout.export_json(None)).unwrap()
}

#[test]
fn centreline_matches_handoff_fixture() {
    let shipped = handoff_fixture();
    let layout = &shipped["layout"];
    let passages: Vec<Value> = layout["passages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            json!({"id": p["id"], "kind": p["kind"], "width_m": p["width_m"],
                   "ceiling_m": p["ceiling_m"], "points": p["points"]})
        })
        .collect();
    let desc = json!({
        "passages": passages,
        "openings": [],
        "sample_step_m": layout["sample_step_m"],
    });
    let sampled = tb::sample(&validate_description(&desc).unwrap(), None).unwrap();
    let got = &sampled.passages[0];
    let want = shipped["sampled"]["passages"][0]["samples"].as_array().unwrap();
    assert_eq!(got.samples.len(), want.len());
    let close = |g: &[f32], w: &Value| {
        let w: Vec<f64> = w
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        g.len() == w.len() && g.iter().zip(&w).all(|(a, b)| (f64::from(*a) - b).abs() <= 0.02)
    };
    for (g, w) in got.samples.iter().zip(want) {
        assert!(close(&g.p, &w["p"]), "{g:?} vs {w}");
        assert!(close(&g.left, &w["left"]), "{g:?} vs {w}");
        assert!((f64::from(g.s) - w["s"].as_f64().unwrap()).abs() <= 0.02);
    }
    // Single passage with no neighbour: all walls present, both ends capped.
    assert!(got.wall_left.iter().all(|w| *w) && got.wall_right.iter().all(|w| *w));
    assert!(got.cap_start && got.cap_end);
}

#[test]
fn cover_rule_needs_ground_over_the_ceiling() {
    let desc = validate_description(&fixture_desc()).unwrap();
    // Flat ground at 0 m, as the sampler reads it.
    let ground = TerrainSampler::new(HeightGrid::square(4, vec![16_384; 16]).unwrap(), 1280.0)
        .with_zero_counts(16_384);
    let covered = tb::sample(&desc, Some(&ground)).unwrap();
    assert!(
        covered.passage("main").unwrap().covered.iter().all(|c| *c),
        "40 m of rock over a -40 m tube"
    );
    let open_cut = tb::sample(&desc, None).unwrap();
    assert!(
        !open_cut.passage("main").unwrap().covered.iter().any(|c| *c),
        "unknown overburden bakes open"
    );
}

fn bad_descriptions() -> Vec<(Value, &'static str)> {
    let mut cases: Vec<(Value, &'static str)> = vec![
        (json!({}), "passages must be a list of 1..64 passages"),
        (
            json!({"passages": []}),
            "passages must be a list of 1..64 passages",
        ),
        (json!("just a string"), "must be a JSON object"),
    ];
    let mut with = |mutate: &dyn Fn(&mut Value), fragment: &'static str| {
        let mut d = fixture_desc();
        mutate(&mut d);
        cases.push((d, fragment));
    };
    with(&|d| d["passages"] = json!([]), "passages must be a list");
    with(&|d| d["passages"][0]["width_m"] = json!(2.0), "width_m");
    with(&|d| d["passages"][0]["kind"] = json!("cavern"), "kind");
    with(
        &|d| d["passages"][0]["points"] = json!([[0.0, 0.0, 0.0]]),
        "2..512",
    );
    with(&|d| d["passages"][1]["id"] = json!("main"), "unique");
    with(
        &|d| d["openings"][0]["ramp"] = json!("main"),
        "does not name a ramp passage",
    );
    with(
        &|d| d["openings"][0]["top"] = json!([500.0, 0.0, 500.0]),
        "must sit on the ramp end",
    );
    with(
        &|d| d["junctions"][0]["passages"] = json!(["main", "nope"]),
        "unknown passage",
    );
    with(
        &|d| d["junctions"][0]["passages"] = json!(["main"]),
        "at least 2 passages",
    );
    with(
        &|d| d["junctions"][0]["at"] = json!([400.0, -40.0, 400.0]),
        "must sit where its passages meet",
    );
    with(&|d| d["sample_step_m"] = json!(0.1), "sample_step_m");
    with(&|d| d["passages"][0]["id"] = json!("Main Tube!"), "lore slug");
    with(&|d| d["passages"][0]["bogus"] = json!(1), "unknown keys");
    with(
        &|d| d["gameplay"] = json!({"reclaimer_fits": true}),
        "reclaimer_fits must be false",
    );
    cases
}

#[test]
fn bad_description_is_refused_with_a_clear_error() {
    let cases = bad_descriptions();
    assert_eq!(cases.len(), 17, "every parametrized case of the baker's test");
    for (desc, fragment) in cases {
        match bake(&desc, None, true) {
            Err(TunnelBakeError::Description(DescriptionError(msg))) => {
                assert!(msg.contains(fragment), "{fragment:?} not in {msg:?}");
            }
            other => panic!("{fragment:?}: expected a description error, got {other:?}"),
        }
    }
    assert_eq!(
        bake(&json!({}), None, true).map_err(|e| e.code()).err(),
        Some("tunnel_description_invalid")
    );
}

#[test]
fn disagreement_fails_the_check() {
    let mut tampered = bake_ok(&fixture_desc());
    // Under the floor: nothing there. (The baker's own vertex 10 is a floor vertex of its builder's order; here the
    // pinned mesher's order differs, so the tenth vertex on the -40 m tube floor is moved.)
    let i = (0..tampered.collision_mesh.vertices.len())
        .filter(|&i| tampered.collision_mesh.vertices[i][1] == -40_000)
        .nth(10)
        .unwrap();
    tampered.collision_mesh.vertices[i][1] -= 5_000;
    let fails: Vec<_> = check_consistency(&tampered)
        .into_iter()
        .filter(|f| f.status == FindingStatus::Fail)
        .collect();
    assert!(fails.iter().any(|f| f.rule == "collision_inside_render"));
    let mut shifted = bake_ok(&fixture_desc());
    for c in &mut shifted.terrain_cut.cuts[0].corners {
        c[0] += 50_000;
        c[2] += 50_000;
    }
    assert!(
        check_consistency(&shifted)
            .iter()
            .any(|f| f.rule == "mouth_cut_matches_collision" && f.status == FindingStatus::Fail)
    );
}

#[test]
fn strict_bake_raises_when_outputs_disagree() {
    // The cut builder's output shifted 50 m before the checks run (the baker's test patched the builder).
    let mut baked = bake_unchecked(&fixture_desc(), None).unwrap();
    for c in &mut baked.terrain_cut.cuts[0].corners {
        c[0] += 50_000;
        c[2] += 50_000;
    }
    let err = finish(baked, true).unwrap_err();
    assert_eq!(err.code(), "tunnel_bake_disagrees");
    assert!(err.to_string().contains("mouth_cut_matches_collision"), "{err}");
}

/// A `do27-tunnels-v2/1` description imports as a `Passages` source whose cook meshes the same collision triangles
/// the baker's collision mesh holds (one mesher, ADR-8175).
#[test]
fn a_description_imports_as_a_passages_source_with_the_same_collision() {
    use regia_world_geom::passages::PassagesDocument;
    let text = fixture_desc().to_string();
    assert!(PassagesDocument::is_description(&text));
    let doc = PassagesDocument::import_description(&text, "fixture").unwrap();
    assert_eq!(doc.layout.map_id, "fixture");
    assert_eq!(doc.layout.passages.len(), 3);
    assert_eq!(PassagesDocument::from_ron(&doc.to_ron().unwrap()).unwrap(), doc);
    // On the same terrain (the cook always has one; without ground the baker bakes unknown overburden open).
    let ground = TerrainSampler::new(HeightGrid::square(4, vec![16_384; 16]).unwrap(), 1280.0)
        .with_zero_counts(16_384);
    let baked = bake(&fixture_desc(), Some(&ground), true).unwrap();
    let mesh = doc.layout.sample(Some(&ground));
    let tris =
        regia_world_geom::cook::quantize_mesh(&mesh.triangles(&doc.layout.openings, Some(&ground))).unwrap();
    assert_eq!(BakeMesh::from_world(&tris), baked.collision_mesh);
    assert_eq!(
        PassagesDocument::import_description("{\"passages\": []}", "x").map_err(|e| e.code()),
        Err("passages_import")
    );
}
