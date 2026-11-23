// SPDX-License-Identifier: MPL-2.0
//! Spec 64 § 9 tests 10 (spline-deformed mesh) and 11 (road ribbon), plus `road.exclusion` and the
//! road profile capture path. Inputs enter through `graph.input`; no GPU.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{box_shape, compile, graph, node};
use regia_core::math::Vec3;
use regia_procgen::data::{GroundData, MeshData};
use regia_procgen::exec::{Frame, StaticSnapshot};
use regia_procgen::nodes::spline_data_item;
use regia_procgen::road::RoadProfile;
use regia_procgen::{Bundle, CancelToken, Evaluator, Payload, ProcGraph, RunInputs, RunResult};
use regia_scene::spline::{Spline, SplinePoint, TangentMode};

fn run(g: &ProcGraph, snap: StaticSnapshot, inputs: Vec<(&str, Bundle)>) -> RunResult {
    let frame = Frame {
        graph_inputs: inputs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        ..Frame::default()
    };
    let ri = RunInputs::new(Arc::new(snap), 3);
    match Evaluator::new().run_frame(&compile(g), &ri, &CancelToken::new(), &frame) {
        Ok(r) => r,
        Err(e) => panic!("run failed: {e}"),
    }
}

fn base_snapshot() -> StaticSnapshot {
    StaticSnapshot {
        self_info: Some(regia_procgen::exec::SelfInfo {
            guid: regia_core::id::Guid(1),
            name: "gen".into(),
            bounds: box_shape([0.0; 3], [500.0; 3], 0.0),
        }),
        ..StaticSnapshot::default()
    }
}

/// A unit cube centered on the origin (8 vertices, 12 triangles).
fn unit_box() -> MeshData {
    let p: Vec<[f32; 3]> = (0..8u8)
        .map(|i| {
            [
                f32::from(i & 1) - 0.5,
                f32::from((i >> 1) & 1) - 0.5,
                f32::from((i >> 2) & 1) - 0.5,
            ]
        })
        .collect();
    let quads = [
        [0, 1, 3, 2],
        [4, 6, 7, 5],
        [0, 4, 5, 1],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 5, 7, 3],
    ];
    let mut indices = Vec::new();
    for q in quads {
        indices.extend([q[0], q[1], q[2], q[0], q[2], q[3]]);
    }
    let normals = p.iter().map(|v| Vec3::from(*v).normalize().to_array()).collect();
    MeshData {
        asset: "box.glb".into(),
        positions: p,
        normals,
        indices,
    }
}

/// A quarter circle of radius `r` in XZ from (r, 0, 0) to (0, 0, r).
fn arc(r: f32) -> Spline {
    let k = 1.6569 * r;
    Spline {
        points: vec![
            SplinePoint::free(
                Vec3::new(r, 0.0, 0.0),
                Vec3::new(0.0, 0.0, k),
                Vec3::new(0.0, 0.0, k),
            ),
            SplinePoint::free(
                Vec3::new(0.0, 0.0, r),
                Vec3::new(-k, 0.0, 0.0),
                Vec3::new(-k, 0.0, 0.0),
            ),
        ],
        ..Spline::default()
    }
}

#[test]
fn test_10_spline_mesh_keeps_cross_sections_and_seams() {
    let modules = r#"{ "seg": (mesh: "box.glb", length_m: 1.0) }"#;
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"in\"")])),
            (
                2,
                node(
                    "spline.modules",
                    &[
                        ("modules", modules),
                        ("pattern", "\"repeat(seg)\""),
                        ("fit", "Scale"),
                    ],
                ),
            ),
            (
                3,
                node(
                    "spawn.spline_mesh",
                    &[("segments_per_m", "4.0"), ("material", "\"m.material.ron\"")],
                ),
            ),
            (4, node("graph.output", &[])),
        ],
        &[
            (1, "out", 2, "in"),
            (2, "out", 3, "in"),
            (1, "out", 3, "spline"),
            (3, "out", 4, "in"),
        ],
    );
    let mut snap = base_snapshot();
    snap.meshes.insert("box.glb".into(), Arc::new(unit_box()));
    let r = run(&g, snap, vec![("in", Bundle::one(spline_data_item(arc(10.0))))]);
    assert_eq!(r.output.meshes.len(), 1, "one material, under the vertex cap");
    let m = &r.output.meshes[0];
    assert_eq!(m.material, "m.material.ron");
    assert!(m.indices.len().is_multiple_of(3) && m.indices.iter().all(|i| (*i as usize) < m.positions.len()));
    // Group vertices into rows by their distance from the arc's center axis... the frame is
    // orthonormal, so every cross-section row of a 1 × 1 box keeps area 1: check every set of four
    // vertices that share one source row through their spread in the side/up plane.
    let curve = arc(10.0).curve();
    let mut rows: BTreeMap<u32, Vec<Vec3>> = BTreeMap::new();
    for p in &m.positions {
        let hit = curve.closest(Vec3::from(*p));
        rows.entry((hit.along_m * 1000.0).round() as u32)
            .or_default()
            .push(Vec3::from(*p));
    }
    let mut checked = 0;
    for pts in rows.values().filter(|v| v.len() >= 4) {
        // Extent along up (Y) and along the radial side direction.
        let (lo_y, hi_y) = pts.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), p| {
            (a.min(p.y), b.max(p.y))
        });
        let radial: Vec<f32> = pts.iter().map(|p| Vec3::new(p.x, 0.0, p.z).length()).collect();
        let (lo_r, hi_r) = radial
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), v| {
                (a.min(*v), b.max(*v))
            });
        let area = (hi_y - lo_y) * (hi_r - lo_r);
        assert!((area - 1.0).abs() < 0.01, "row area {area}");
        checked += 1;
    }
    assert!(checked > 10, "rows checked: {checked}");
    // Seams: the end face of each piece and the start face of the next share positions exactly.
    let mut seen: BTreeMap<[u32; 3], usize> = BTreeMap::new();
    for p in &m.positions {
        *seen.entry(p.map(f32::to_bits)).or_default() += 1;
    }
    let shared = seen.values().filter(|n| **n >= 2).count();
    let pieces = 15; // round(15.708 m / 1 m) modules of the Scale fit
    assert!(shared >= 4 * (pieces - 1), "shared seam vertices: {shared}");
}

fn profile_ron() -> &'static str {
    "RoadProfile(format_version: 1, width_m: 6.0, shoulders_m: (0.5, 0.5), surface_lift_m: 0.05, \
     cross_slope_pct: 2.0, material: \"asphalt.material.ron\", uv_m_per_tile: 4.0, samples_per_m: 1.0)"
}

/// A smooth synthetic hill, 2 m per sample.
fn hill() -> GroundData {
    let n = 129u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let (c, r) = ((i % n) as f32 - 64.0, (i / n) as f32 - 64.0);
            12.0 - 0.0015 * (c * c + r * r) + 0.02 * c
        })
        .collect();
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 256.0, 1.0, 0.0).expect("map");
    GroundData::capture(&map, [-120.0, -120.0, 120.0, 120.0], 2)
}

#[test]
fn test_11_road_ribbon_on_a_hill() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (
                2,
                node("in.ground", &[("rect", "[-120.0, -120.0, 120.0, 120.0]")]),
            ),
            (
                3,
                node("road.ribbon", &[("profile", "\"roads/two_lane.road.ron\"")]),
            ),
            (4, node("graph.output", &[])),
            (5, node("graph.output", &[("pin", "\"edges\"")])),
        ],
        &[
            (1, "out", 3, "splines"),
            (2, "out", 3, "ground"),
            (3, "out", 4, "in"),
            (3, "out_edges", 5, "in"),
        ],
    );
    let ground = hill();
    let mut snap = base_snapshot();
    snap.ground = Some(Arc::new(ground.clone()));
    snap.road_profiles.insert(
        "roads/two_lane.road.ron".into(),
        RoadProfile::from_ron(profile_ron()).expect("profile"),
    );
    let mut road = Spline::through([
        Vec3::new(-80.0, 0.0, -30.0),
        Vec3::new(-20.0, 0.0, 10.0),
        Vec3::new(40.0, 0.0, -10.0),
        Vec3::new(90.0, 0.0, 30.0),
    ]);
    road.up = regia_scene::spline::SplineUp::WorldY;
    let r = run(&g, snap, vec![("road", Bundle::one(spline_data_item(road)))]);
    let m = &r.output.meshes[0];
    assert_eq!(m.material, "asphalt.material.ron");
    // Five columns per row: shoulder, edge, center, edge, shoulder.
    let cols = 5;
    assert!(m.positions.len().is_multiple_of(cols));
    let rows = m.positions.len() / cols;
    assert!(rows > 150, "{rows} rows for ~190 m at 1 per m");
    for (i, p) in m.positions.iter().enumerate() {
        let g = ground.height(p[0], p[2]);
        assert!(
            p[1] >= g + 0.05 - 1e-4,
            "vertex {i} at {} under ground {g} + lift",
            p[1]
        );
    }
    for row in m.positions.chunks(cols) {
        let (edge, center) = (Vec3::from(row[1]), Vec3::from(row[2]));
        let run_m = Vec3::new(edge.x - center.x, 0.0, edge.z - center.z).length();
        let slope = (center.y - edge.y) / run_m;
        assert!((slope - 0.02).abs() < 0.02 * 0.001 + 1e-5, "cross slope {slope}");
    }
    for k in 1..rows {
        assert!(
            m.uvs[k * cols][1] > m.uvs[(k - 1) * cols][1],
            "V increases along the road"
        );
    }
    let edges = r
        .output
        .graph_outputs
        .get("edges")
        .and_then(|b| b.0.first())
        .and_then(|i| i.as_points().cloned());
    assert_eq!(edges.map(|e| e.len()), Some(rows * 2));
}

#[test]
fn road_exclusion_covers_the_road_and_missing_profiles_fail() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (2, node("road.exclusion", &[("profile", profile_ron())])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "splines"), (2, "out", 3, "in")],
    );
    let mut s = Spline::through([Vec3::ZERO, Vec3::new(50.0, 0.0, 0.0)]);
    s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let r = run(
        &g,
        base_snapshot(),
        vec![("road", Bundle::one(spline_data_item(s.clone())))],
    );
    let b = r.output.graph_outputs.get("out").cloned().unwrap_or_default();
    let Some(Payload::Polygon(p)) = b.0.first().map(|i| &i.payload) else {
        panic!("no polygon");
    };
    // Half width 3 + shoulder 0.5 + flatten_extra 1 = 4.5 m each side, round caps not added.
    assert!(p.contains(25.0, 4.4) && !p.contains(25.0, 4.6));
    assert!(((p.area() - 50.0 * 9.0) / 450.0).abs() < 0.01, "{}", p.area());

    let missing = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (
                2,
                node("road.exclusion", &[("profile", "\"roads/none.road.ron\"")]),
            ),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "splines"), (2, "out", 3, "in")],
    );
    let frame = Frame {
        graph_inputs: [("road".to_string(), Bundle::one(spline_data_item(s)))]
            .into_iter()
            .collect(),
        ..Frame::default()
    };
    let err = Evaluator::new()
        .run_frame(
            &compile(&missing),
            &RunInputs::new(Arc::new(base_snapshot()), 3),
            &CancelToken::new(),
            &frame,
        )
        .expect_err("missing profile");
    assert_eq!(err.code(), "road_profile_missing");
}

#[test]
fn road_markings_follow_the_dash_pattern_over_the_surface() {
    let profile = "RoadProfile(width_m: 6.0, shoulders_m: (0.5, 0.5), surface_lift_m: 0.05, cross_slope_pct: 2.0, \
                   samples_per_m: 1.0, markings: [(pattern: \"dash(3, 9)\", offset_m: 0.0, width_m: 0.2)])";
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (
                2,
                node(
                    "road.markings",
                    &[("profile", profile), ("material", "\"paint.material.ron\"")],
                ),
            ),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "splines"), (2, "out", 3, "in")],
    );
    let mut s = Spline::through([Vec3::ZERO, Vec3::new(50.0, 0.0, 0.0)]);
    s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let r = run(
        &g,
        base_snapshot(),
        vec![("road", Bundle::one(spline_data_item(s)))],
    );
    let m = &r.output.meshes[0];
    assert_eq!(m.material, "paint.material.ron");
    // No ground: the center sits at lift + the crown drop of the outer shoulder edge (3.5 m × 2 %).
    let expected = 0.05 + 0.07 + 0.005;
    assert!(
        m.positions
            .iter()
            .all(|p| (p[1] - expected).abs() < 1e-4 + 0.1 * 0.02),
        "{:?}",
        m.positions[0]
    );
    // Dashes cover [0, 3], [12, 15], [24, 27], [36, 39], [48, 50]: no vertex in a gap.
    for p in &m.positions {
        let x = f64::from(p[0]);
        let in_dash = [(0.0, 3.0), (12.0, 15.0), (24.0, 27.0), (36.0, 39.0), (48.0, 50.0)]
            .iter()
            .any(|(a, b)| x >= a - 1e-4 && x <= b + 1e-4);
        assert!(in_dash, "vertex at x = {x}");
        assert!((p[2].abs() - 0.1).abs() < 1e-5);
    }
    assert!(m.indices.len().is_multiple_of(6));
}

fn flatten_graph() -> ProcGraph {
    graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (
                2,
                node("in.ground", &[("rect", "[-100.0, -100.0, 100.0, 100.0]")]),
            ),
            (
                3,
                node(
                    "terrain.flatten_road",
                    &[(
                        "profile",
                        "RoadProfile(width_m: 6.0, shoulders_m: (0.5, 0.5), surface_lift_m: 0.05, cross_slope_pct: 2.0, \
                         samples_per_m: 1.0, terrain: (flatten: true, flatten_extra_m: 1.0, falloff_m: 4.0, carve_max_m: 3.0, fill_max_m: 3.0))",
                    )],
                ),
            ),
            (4, node("graph.output", &[])),
        ],
        &[
            (1, "out", 3, "splines"),
            (2, "out", 3, "ground"),
            (3, "out", 4, "in"),
        ],
    )
}

fn try_flatten(
    terrain: &regia_terrain::Terrain,
    gate: regia_procgen::exec::TerrainEditGate,
) -> Result<RunResult, regia_procgen::RunError> {
    let mut snap = base_snapshot();
    snap.ground = Some(Arc::new(GroundData::capture(
        &terrain.map,
        [-100.0, -100.0, 100.0, 100.0],
        2,
    )));
    let mut road = Spline::through([Vec3::new(-60.0, 0.0, 0.0), Vec3::new(60.0, 0.0, 0.0)]);
    road.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let frame = Frame {
        graph_inputs: [("road".to_string(), Bundle::one(spline_data_item(road)))]
            .into_iter()
            .collect(),
        ..Frame::default()
    };
    let mut ri = RunInputs::new(Arc::new(snap), 3);
    ri.terrain_edits = gate;
    Evaluator::new().run_frame(&compile(&flatten_graph()), &ri, &CancelToken::new(), &frame)
}

#[test]
fn test_13_flatten_records_a_bit_exact_patch_and_respects_the_gates() {
    use regia_procgen::exec::TerrainEditGate;
    // A 256 m terrain, 2 m per sample, rising 5 cm per meter along x and bumpy across z.
    let n = 129u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let (c, r) = ((i % n) as f32, (i / n) as f32);
            0.1 * (c - 64.0) + 0.3 * ((r - 64.0) * 0.4).sin()
        })
        .collect();
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 256.0, 100.0, -50.0).expect("map");
    let mut terrain = regia_terrain::Terrain::new(
        &map.to_heightfield(),
        256.0,
        100.0,
        -50.0,
        regia_terrain::CdlodConfig::default(),
        4,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain");

    for (gate, code) in [
        (TerrainEditGate::NotAllowed, "terrain_edits_not_allowed"),
        (TerrainEditGate::NotBaked, "flatten_runtime_forbidden"),
    ] {
        let err = try_flatten(&terrain, gate).expect_err("gated");
        assert_eq!(err.code(), code);
    }

    let r = try_flatten(&terrain, TerrainEditGate::Allowed).expect("flatten");
    assert_eq!(r.output.terrain_edits.len(), 1);
    let rec = &r.output.terrain_edits[0];
    let [c0, r0, c1, r1] = rec.rect;
    assert_eq!(rec.samples.len(), ((c1 - c0) * (r1 - r0)) as usize);
    // Reach: 3 + 0.5 + 1 + 4 m falloff = 8.5 m across (about four samples each side of z = 0).
    assert!(r1 - r0 <= 12, "rows {r0}..{r1}");
    // Under the center line the ground sits at the road surface minus the lift.
    let w = (c1 - c0) as usize;
    let mid_row = (64 - r0) as usize;
    for c in 10..w - 10 {
        let (old, new) = (rec.original[mid_row * w + c], rec.samples[mid_row * w + c]);
        assert!((new - old).abs() < 0.5, "a gentle flatten: {old} -> {new}");
    }
    // Same inputs, same record (baking twice equals baking once).
    let again = try_flatten(&terrain, TerrainEditGate::Allowed).expect("again");
    assert_eq!(&again.output.terrain_edits[0], rec);
    assert_eq!(again.output_hash, r.output_hash);
    // The patch applies and restores bit for bit.
    let before = terrain.map.heights().to_vec();
    let mut edits = regia_terrain::TerrainEdits::default();
    let rect = regia_terrain::DirtyRect {
        col_start: c0,
        col_end: c1,
        row_start: r0,
        row_end: r1,
    };
    let undo =
        regia_terrain::apply_height_patch(&mut terrain, &mut edits, rect, &rec.samples).expect("patch");
    assert_eq!(
        undo.before, rec.original,
        "the record's originals are the terrain's samples"
    );
    undo.restore(&mut terrain, &mut edits, true);
    assert!(
        terrain
            .map
            .heights()
            .iter()
            .zip(&before)
            .all(|(a, b)| a.to_bits() == b.to_bits())
    );
}

/// `road.exclusion` reads its `.road.ron` from the world snapshot, so the world content hash is in
/// its cache key (ADR-1875 (11)): with one shared cache, an edited profile never answers with the
/// polygon of the old one, and an unchanged profile still hits the cache.
#[test]
fn an_edited_road_profile_never_answers_from_the_cache() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (2, node("road.exclusion", &[("profile", "\"roads/a.road.ron\"")])),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "splines"), (2, "out", 3, "in")],
    );
    let c = compile(&g);
    let mut s = Spline::through([Vec3::ZERO, Vec3::new(50.0, 0.0, 0.0)]);
    s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let frame = Frame {
        graph_inputs: [("road".to_string(), Bundle::one(spline_data_item(s)))]
            .into_iter()
            .collect(),
        ..Frame::default()
    };
    let snap = |width: &str| {
        let text = profile_ron().replace("width_m: 6.0", &format!("width_m: {width}"));
        let profile = RoadProfile::from_ron(&text).expect("profile");
        let mut snap = base_snapshot();
        snap.road_profiles = BTreeMap::from([("roads/a.road.ron".to_string(), profile)]);
        RunInputs::new(Arc::new(snap), 3)
    };
    let area = |r: &RunResult| {
        let b = r.output.graph_outputs.get("out").cloned().unwrap_or_default();
        let Some(Payload::Polygon(p)) = b.0.first().map(|i| &i.payload) else {
            panic!("no polygon");
        };
        p.area()
    };
    let cache = Arc::new(parking_lot::Mutex::new(regia_procgen::exec::ProcCache::new(
        64 << 20,
    )));
    let cached = |inputs: &RunInputs| {
        Evaluator::with_cache(cache.clone())
            .run_frame(&c, inputs, &CancelToken::new(), &frame)
            .expect("run")
    };
    let first = cached(&snap("6.0"));
    let edited = cached(&snap("10.0"));
    // Half widths 3 and 5, + shoulder 0.5 + flatten_extra 1, over 50 m.
    assert!(
        ((area(&first) - 50.0 * 9.0) / 450.0).abs() < 0.01,
        "{}",
        area(&first)
    );
    assert!(
        ((area(&edited) - 50.0 * 13.0) / 650.0).abs() < 0.01,
        "stale cache: {}",
        area(&edited)
    );
    let again = cached(&snap("6.0"));
    assert_eq!(again.output_hash, first.output_hash);
    assert!(again.report.cache_hits >= 1, "{:?}", again.report.cache_hits);
}

fn junction_graph(profile: &str) -> ProcGraph {
    graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (2, node("road.junctions", &[("profile", profile)])),
            (3, node("road.ribbon", &[("profile", profile)])),
            (4, node("graph.output", &[])),
        ],
        &[
            (1, "out", 2, "splines"),
            (2, "out_splines", 3, "splines"),
            (3, "out", 4, "in"),
        ],
    )
}

const JUNCTION_PROFILE: &str = "RoadProfile(width_m: 6.0, shoulders_m: (0.5, 0.5), surface_lift_m: 0.05, \
    cross_slope_pct: 2.0, samples_per_m: 1.0, material: \"asphalt\", \
    junction: (margin_m: 1.0, material: Some(\"patch\")))";

/// A straight road item; its entity scope names it by its first point, like `in.entities` would.
fn line(from: Vec3, to: Vec3) -> regia_procgen::DataItem {
    let mut s = Spline::through([from, to]);
    s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
    let mut item = spline_data_item(s);
    item.scope.insert(
        "entity".to_string(),
        regia_procgen::AttrValue::Name(format!("{from}")),
    );
    item
}

/// Every ribbon vertex inside the square of half side `reach` around the junction center (the end
/// rows) has a patch vertex within 1 mm.
fn assert_patch_meets_ribbons(r: &RunResult, center: Vec3, reach: f32) {
    let patch = r
        .output
        .meshes
        .iter()
        .find(|m| m.material == "patch")
        .expect("a patch mesh");
    let ribbon = r
        .output
        .meshes
        .iter()
        .find(|m| m.material == "asphalt")
        .expect("a ribbon mesh");
    let mut met = 0;
    for p in ribbon.positions.iter().map(|p| Vec3::from(*p)) {
        if (p.x - center.x).abs().max((p.z - center.z).abs()) > reach {
            continue;
        }
        let best = patch
            .positions
            .iter()
            .map(|q| (Vec3::from(*q) - p).length())
            .fold(f32::INFINITY, f32::min);
        assert!(best < 0.001, "ribbon vertex {p} is {best} m from the patch");
        met += 1;
    }
    assert!(met >= 5, "the ribbons reach the patch ({met} end vertices)");
    for t in patch.indices.as_chunks::<3>().0 {
        let [a, b, c] = t.map(|i| Vec3::from(patch.positions[i as usize]));
        assert!((b - a).cross(c - a).y >= 0.0, "patch triangles face up");
    }
    assert_eq!(patch.positions.len(), patch.normals.len());
    assert_eq!(patch.positions.len(), patch.uvs.len());
}

#[test]
fn test_12_a_crossing_makes_one_patch_that_meets_both_cut_ribbons() {
    let g = junction_graph(JUNCTION_PROFILE);
    let roads = Bundle(vec![
        line(Vec3::new(-50.0, 0.0, 0.0), Vec3::new(50.0, 0.0, 0.0)),
        line(Vec3::new(0.0, 0.0, -50.0), Vec3::new(0.0, 0.0, 50.0)),
    ]);
    let r = run(&g, base_snapshot(), vec![("road", roads)]);
    assert_eq!(
        r.report.node(2).and_then(|n| n.counters.get("junctions")),
        Some(&1)
    );
    // Half the other road's width (3.5) plus the margin (1): the end rows sit 4.5 m out.
    assert_patch_meets_ribbons(&r, Vec3::ZERO, 4.6);
    let ribbon = r
        .output
        .meshes
        .iter()
        .find(|m| m.material == "asphalt")
        .expect("ribbon");
    let nearest = ribbon
        .positions
        .iter()
        .map(|p| p[0].abs().max(p[2].abs()))
        .fold(f32::INFINITY, f32::min);
    assert!((nearest - 4.5).abs() < 1e-4, "cut back to 4.5 m: {nearest}");
    assert_eq!(r.report.roads.len(), 2);
    assert!(
        r.report
            .roads
            .values()
            .all(|l| l.junctions == 1 && l.length_m == 100.0)
    );
}

#[test]
fn test_12_a_t_end_is_detected_and_keeps_the_through_road_whole_past_the_patch() {
    let g = junction_graph(JUNCTION_PROFILE);
    let roads = Bundle(vec![
        line(Vec3::new(-50.0, 0.0, 0.0), Vec3::new(50.0, 0.0, 0.0)),
        // Stops 2 m short of the through road's center line.
        line(Vec3::new(0.0, 0.0, 40.0), Vec3::new(0.0, 0.0, 2.0)),
    ]);
    let r = run(&g, base_snapshot(), vec![("road", roads)]);
    assert_eq!(
        r.report.node(2).and_then(|n| n.counters.get("junctions")),
        Some(&1)
    );
    assert_patch_meets_ribbons(&r, Vec3::ZERO, 4.6);
    let ribbon = r
        .output
        .meshes
        .iter()
        .find(|m| m.material == "asphalt")
        .expect("ribbon");
    let (lo, hi) = ribbon
        .positions
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), p| (a.min(p[0]), b.max(p[0])));
    assert!(
        lo <= -50.0 + 1e-3 && hi >= 50.0 - 1e-3,
        "the through road keeps its ends"
    );
    assert!(
        !ribbon
            .positions
            .iter()
            .any(|p| p[0].abs() < 3.0 && p[2].abs() < 3.0),
        "nothing of a ribbon inside the junction"
    );
}

/// Two cross roads 8.5 m apart: each cut takes 4.5 m out of the through road, so they overlap and no
/// piece stays between them, though the crossings are farther apart than the widths and margin add up
/// to. Two patches would leave a hole across the road.
#[test]
fn crossings_whose_cuts_overlap_fill_as_one_junction() {
    let g = junction_graph(JUNCTION_PROFILE);
    let roads = Bundle(vec![
        line(Vec3::new(-50.0, 0.0, 0.0), Vec3::new(50.0, 0.0, 0.0)),
        line(Vec3::new(0.0, 0.0, -50.0), Vec3::new(0.0, 0.0, 50.0)),
        line(Vec3::new(8.5, 0.0, -50.0), Vec3::new(8.5, 0.0, 50.0)),
    ]);
    let r = run(&g, base_snapshot(), vec![("road", roads)]);
    assert_eq!(
        r.report.node(2).and_then(|n| n.counters.get("junctions")),
        Some(&1)
    );
    let patches = r.output.meshes.iter().filter(|m| m.material == "patch").count();
    assert_eq!(patches, 1);
}

#[test]
fn a_sharp_junction_warns_and_a_far_road_makes_none() {
    let g = junction_graph(JUNCTION_PROFILE);
    let sharp = Bundle(vec![
        line(Vec3::new(-50.0, 0.0, 0.0), Vec3::new(50.0, 0.0, 0.0)),
        line(Vec3::new(-50.0, 0.0, -9.0), Vec3::new(50.0, 0.0, 9.0)),
    ]);
    let r = run(&g, base_snapshot(), vec![("road", sharp)]);
    assert!(r.report.warning_codes().contains(&"junction_sharp".to_string()));
    let apart = Bundle(vec![
        line(Vec3::new(-50.0, 0.0, 0.0), Vec3::new(50.0, 0.0, 0.0)),
        line(Vec3::new(-50.0, 0.0, 30.0), Vec3::new(50.0, 0.0, 30.0)),
    ]);
    let r = run(&g, base_snapshot(), vec![("road", apart)]);
    assert_eq!(
        r.report.node(2).and_then(|n| n.counters.get("junctions")),
        Some(&0)
    );
    assert_eq!(r.output.meshes.len(), 1, "two whole ribbons, no patch");
}

#[test]
fn road_decals_follow_the_polyline_at_half_the_carriageway() {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (
                2,
                node(
                    "road.decals",
                    &[("profile", JUNCTION_PROFILE), ("color", "(0.1, 0.2, 0.3)")],
                ),
            ),
            (3, node("graph.output", &[])),
        ],
        &[(1, "out", 2, "splines"), (2, "out", 3, "in")],
    );
    let r = run(
        &g,
        base_snapshot(),
        vec![(
            "road",
            Bundle::one(line(Vec3::new(0.0, 0.0, 0.0), Vec3::new(30.0, 0.0, 40.0))),
        )],
    );
    assert_eq!(r.output.road_decals.len(), 1, "a straight road is one segment");
    let d = r.output.road_decals[0];
    assert_eq!((d.a, d.b, d.half_width), ([0.0, 0.0], [30.0, 40.0], 3.0));
    assert_eq!(d.color, [0.1, 0.2, 0.3]);
    let mut other = r.output.clone();
    other.road_decals.clear();
    assert_ne!(
        other.hash(),
        r.output.hash(),
        "decals are part of the output hash"
    );
}

#[test]
fn the_starter_road_network_compiles() {
    let g = ProcGraph::from_ron(regia_procgen::road::STARTER_ROAD_NETWORK_GRAPH).expect("parses");
    compile(&g);
}

/// Spec 64 § 7.3: where the road would need more than `carve_max_m`/`fill_max_m`, the flatten
/// stops at the limit, warns `flatten_limit` and the run report names the distance along the road.
#[test]
fn a_ridge_beyond_the_carve_limit_warns_flatten_limit() {
    use regia_procgen::exec::TerrainEditGate;
    let terrain_of = |ridge: f32| {
        // Flat at 0, with a 10 m (or `ridge`) wall along the road's shoulder band at z = 4 m.
        let n = 129u32;
        let heights: Vec<f32> = (0..n * n)
            .map(|i| if i / n == 66 { ridge } else { 0.0 })
            .collect();
        let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 256.0, 100.0, -50.0).expect("map");
        regia_terrain::Terrain::new(
            &map.to_heightfield(),
            256.0,
            100.0,
            -50.0,
            regia_terrain::CdlodConfig::default(),
            4,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain")
    };
    let r = try_flatten(&terrain_of(10.0), TerrainEditGate::Allowed).expect("flatten");
    assert!(
        r.report.warning_codes().iter().any(|c| c == "flatten_limit"),
        "{:?}",
        r.report.warning_codes()
    );
    let line = r.report.roads.values().next().expect("one road line");
    let at = line.flatten_limit_at_m.expect("the limit distance is reported");
    assert!((0.0..=120.0).contains(&at), "{at}");
    // The record never moves a sample by more than the limits (3 m down, 3 m up).
    let rec = &r.output.terrain_edits[0];
    for (o, s) in rec.original.iter().zip(&rec.samples) {
        assert!((s - o).abs() <= 3.0 + 1e-3, "{o} -> {s}");
    }
    // A 1 m ridge stays within the limits: no warning.
    let ok = try_flatten(&terrain_of(1.0), TerrainEditGate::Allowed).expect("flatten");
    assert!(!ok.report.warning_codes().iter().any(|c| c == "flatten_limit"));
    assert!(ok.report.roads.values().all(|l| l.flatten_limit_at_m.is_none()));
}
