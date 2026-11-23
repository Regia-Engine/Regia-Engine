// SPDX-License-Identifier: MPL-2.0
//! The `.motiondb.ron` cook (`docs/specs/52-motion-matching.md` § 4, § 8 item 12; ADR-2409): the
//! cook is deterministic, the cooked database answers every query exactly like the runtime-built
//! one, clips with non-finite values are refused (in the database cook and in the glTF clip cook),
//! and a project cook resolves glTF sub-assets into a file the runtime loader reads with its
//! skeleton and clips.

use regia_animation::clip::TrackCurve;
use regia_animation::motion_matching::procedural::{arc_trajectory, procedural_locomotion};
use regia_animation::motion_matching::{
    DEFAULT_TRAJECTORY_TIMES, Exclusion, MOTIONDB_FORMAT_VERSION, MatcherSettings, MotionDatabase,
    MotionMatching, SearchFilter, SearchMethod, load_motion_database,
};
use regia_asset::{AssetServer, AssetServerSettings};
use regia_asset_pipeline::motiondb_cook::{
    CLIP_NON_FINITE, MemoryInputs, cook_motiondb, encode_motiondb_file, first_non_finite, scan_clip_finite,
};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_core::math::Vec3;
use regia_test_utils::unique_temp_dir;
use serde_json::json;

fn source_text() -> String {
    procedural_locomotion().source.to_ron_string().expect("ron")
}

fn cook_procedural() -> regia_asset_pipeline::motiondb_cook::CookedMotionDb {
    let set = procedural_locomotion();
    let mut inputs = MemoryInputs::procedural(&set);
    cook_motiondb(&source_text(), &mut inputs).expect("cook")
}

#[test]
fn the_cook_is_byte_identical_twice() {
    let a = cook_procedural();
    let b = cook_procedural();
    assert_eq!(a.payload, b.payload, "payload bytes");
    assert_eq!(a.dependencies, b.dependencies);
    assert_eq!(
        a.dependencies.len(),
        1 + 6,
        "the skeleton then one clip per entry"
    );
    let id = MemoryInputs::id_of("procedural/locomotion.motiondb.ron");
    let fa = encode_motiondb_file(id, 1, &a).expect("file");
    let fb = encode_motiondb_file(id, 1, &b).expect("file");
    assert_eq!(fa, fb, "whole .fckd files");
    assert_eq!(a.database.content_hash(), b.database.content_hash());
    assert!(a.warnings.is_empty(), "{:?}", a.warnings);
}

/// Fixed queries: every 5th frame's pose with a set of desired paths.
fn queries(database: &MotionDatabase) -> Vec<Vec<f32>> {
    let times = DEFAULT_TRAJECTORY_TIMES;
    let paths = [
        arc_trajectory(&times, 0.0, 0.0),
        arc_trajectory(&times, 1.5, 0.0),
        arc_trajectory(&times, 4.0, 0.0),
        arc_trajectory(&times, 1.5, 1.2),
        arc_trajectory(&times, 1.5, -1.2),
        arc_trajectory(&times, 2.5, 0.4),
    ];
    let mut out = Vec::new();
    for frame in (0..database.len()).step_by(5) {
        for path in &paths {
            let query = database.query_from_frame(frame, path).expect("query");
            out.push(database.normalize(&query).expect("normalize"));
        }
    }
    out
}

#[test]
fn the_cooked_database_gives_the_runtime_ones_best_candidates() {
    let runtime = procedural_locomotion().build().expect("runtime database");
    let cooked = MotionDatabase::from_cooked_payload(&cook_procedural().payload).expect("load");
    assert_eq!(cooked.len(), runtime.len());
    assert_eq!(cooked.content_hash(), runtime.content_hash());
    let runtime_queries = queries(&runtime);
    let cooked_queries = queries(&cooked);
    assert_eq!(runtime_queries.len(), cooked_queries.len());
    assert!(runtime_queries.len() > 100, "{} queries", runtime_queries.len());
    for (i, (q, c)) in runtime_queries.iter().zip(&cooked_queries).enumerate() {
        assert!(
            q.iter().zip(c).all(|(a, b)| a.to_bits() == b.to_bits()),
            "query {i}: the normalized query differs"
        );
        for method in [SearchMethod::BruteForce, SearchMethod::KdTree, SearchMethod::Auto] {
            let a = runtime.search(q, Exclusion::NONE, method).expect("hit");
            let b = cooked.search(c, Exclusion::NONE, method).expect("hit");
            assert_eq!(a.frame, b.frame, "query {i} {method:?}: best frame");
            assert_eq!(
                a.cost.to_bits(),
                b.cost.to_bits(),
                "query {i} {method:?}: cost bits"
            );
        }
        let ka = runtime.search_top_k(q, SearchFilter::default(), Exclusion::NONE, 8);
        let kb = cooked.search_top_k(c, SearchFilter::default(), Exclusion::NONE, 8);
        assert_eq!(
            ka.iter().map(|h| (h.frame, h.cost.to_bits())).collect::<Vec<_>>(),
            kb.iter().map(|h| (h.frame, h.cost.to_bits())).collect::<Vec<_>>(),
            "query {i}: top 8"
        );
    }
}

#[test]
fn a_matcher_on_the_cooked_database_plays_the_same_jumps() {
    let runtime = std::sync::Arc::new(procedural_locomotion().build().expect("runtime"));
    let cooked =
        std::sync::Arc::new(MotionDatabase::from_cooked_payload(&cook_procedural().payload).expect("load"));
    let mut a = MotionMatching::new(runtime, MatcherSettings::default());
    let mut b = MotionMatching::new(cooked, MatcherSettings::default());
    let times = DEFAULT_TRAJECTORY_TIMES;
    for step in 0..600u32 {
        // Stand, walk, turn right, run, turn left, stop: one phase every 100 steps.
        let (speed, turn) = [
            (0.0, 0.0),
            (1.5, 0.0),
            (1.5, 1.2),
            (4.0, 0.0),
            (1.5, -1.2),
            (0.0, 0.0),
        ][(step / 100) as usize];
        let path = arc_trajectory(&times, speed, turn);
        a.trajectory = path.clone();
        b.trajectory = path;
        let sa = a.step(1.0 / 30.0).expect("step");
        let sb = b.step(1.0 / 30.0).expect("step");
        assert_eq!(sa.playback.frame, sb.playback.frame, "step {step}");
        assert_eq!(
            sa.playback.time.to_bits(),
            sb.playback.time.to_bits(),
            "step {step}"
        );
        assert_eq!(sa.jump.is_some(), sb.jump.is_some(), "step {step}");
    }
    assert!(
        a.matcher.jumps() > 3,
        "the path changes make it jump ({})",
        a.matcher.jumps()
    );
    assert_eq!(a.matcher.jumps(), b.matcher.jumps());
}

#[test]
fn a_clip_with_a_nan_or_an_infinity_is_refused() {
    let set = procedural_locomotion();
    let clean = set.clips[1].1.clone();
    assert_eq!(first_non_finite(&clean), None);
    assert!(scan_clip_finite(&clean).is_ok());

    let mut nan = clean.clone();
    if let TrackCurve::Animated { values, .. } = &mut nan.tracks[1].translations {
        values[7] = Vec3::new(0.0, f32::NAN, 0.0);
    }
    let error = scan_clip_finite(&nan).expect_err("NaN refused");
    assert_eq!(error.code, CLIP_NON_FINITE);
    assert!(
        error.message.contains("joint 1 translation key 7"),
        "{}",
        error.message
    );

    let mut inf = clean.clone();
    inf.duration = f32::INFINITY;
    assert_eq!(first_non_finite(&inf).as_deref(), Some("duration"));

    // Through the database cook: the entry and the clip are named.
    let walk_path = set.clips[1].0.clone();
    let mut inputs = MemoryInputs::procedural(&set).with_clip(walk_path, nan);
    let error = cook_motiondb(&source_text(), &mut inputs).expect_err("cook refuses");
    assert_eq!(error.code, CLIP_NON_FINITE);
    assert!(error.message.starts_with("entry 1:"), "{}", error.message);
    assert!(error.to_string().starts_with("clip_non_finite: "), "{error}");
}

#[test]
fn unknown_dependencies_and_bad_sources_have_codes() {
    let set = procedural_locomotion();
    let mut empty = MemoryInputs::new();
    assert_eq!(
        cook_motiondb(&source_text(), &mut empty)
            .expect_err("no skeleton")
            .code,
        "motiondb_unknown_skeleton"
    );
    let mut no_clips = MemoryInputs::new().with_skeleton(set.source.skeleton.clone(), set.skeleton.clone());
    let error = cook_motiondb(&source_text(), &mut no_clips).expect_err("no clips");
    assert_eq!(error.code, "motiondb_unknown_clip");
    assert!(error.message.starts_with("entry 0:"), "{}", error.message);
    let mut inputs = MemoryInputs::procedural(&set);
    assert_eq!(
        cook_motiondb("(", &mut inputs).expect_err("parse").code,
        "motiondb_parse"
    );
    // One past the version this build reads (the source text carries the current version).
    let current = format!("format_version: {MOTIONDB_FORMAT_VERSION}");
    assert!(
        source_text().contains(&current),
        "the source text names the current version"
    );
    let newer = source_text().replacen(
        &current,
        &format!("format_version: {}", MOTIONDB_FORMAT_VERSION + 1),
        1,
    );
    assert_eq!(
        cook_motiondb(&newer, &mut inputs).expect_err("version").code,
        "motiondb_version"
    );
    let bad_joint = source_text().replacen("\"foot_l\"", "\"no_such_joint\"", 1);
    assert_eq!(
        cook_motiondb(&bad_joint, &mut inputs).expect_err("joint").code,
        "motiondb_build"
    );
}

// ---- project cook --------------------------------------------------------------------------------

fn le_f32(values: impl IntoIterator<Item = f32>) -> Vec<u8> {
    values.into_iter().flat_map(f32::to_le_bytes).collect()
}

fn pack_glb(json: &serde_json::Value, bin: &[u8]) -> Vec<u8> {
    let mut json = json.to_string().into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut glb = b"glTF".to_vec();
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(bin);
    glb
}

/// A walking rig: `Root` moves 1.5 m along +Z per second (31 keys over 1 s), `FootL` and
/// `FootR` swing opposite each other along Z. `poison` puts a NaN into one foot key.
fn rig_glb(poison: bool) -> Vec<u8> {
    let keys = 31usize;
    let times: Vec<f32> = (0..keys).map(|k| k as f32 / 30.0).collect();
    let root: Vec<f32> = times.iter().flat_map(|&t| [0.0, 0.0, 1.5 * t]).collect();
    let foot = |side: f32| -> Vec<f32> {
        times
            .iter()
            .flat_map(|&t| {
                let s = side * (std::f32::consts::TAU * t).sin();
                [-0.1 * side, 0.05 * s.max(0.0), 0.3 * s]
            })
            .collect()
    };
    let mut left = foot(1.0);
    if poison {
        left[3 * 9 + 2] = f32::NAN;
    }
    let right = foot(-1.0);
    // An unskinned triangle first (positions 36 bytes, indices 12), as a real file has a mesh.
    let mut bin = le_f32([0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    for i in 0u32..3 {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    bin.extend(le_f32(times.iter().copied()));
    bin.extend(le_f32(root));
    bin.extend(le_f32(left));
    bin.extend(le_f32(right));
    let t_len = keys * 4;
    let v_len = keys * 12;
    let doc = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0, 3]}],
        "nodes": [
            {"name": "Root", "children": [1, 2]},
            {"name": "FootL", "translation": [-0.1, 0.0, 0.0]},
            {"name": "FootR", "translation": [0.1, 0.0, 0.0]},
            {"name": "Tri", "mesh": 0}
        ],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1}]}],
        "skins": [{"joints": [0, 1, 2]}],
        "animations": [{
            "name": "walk",
            "samplers": [
                {"input": 2, "output": 3},
                {"input": 2, "output": 4},
                {"input": 2, "output": 5}
            ],
            "channels": [
                {"sampler": 0, "target": {"node": 0, "path": "translation"}},
                {"sampler": 1, "target": {"node": 1, "path": "translation"}},
                {"sampler": 2, "target": {"node": 2, "path": "translation"}}
            ]
        }],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
             "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]},
            {"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"},
            {"bufferView": 2, "componentType": 5126, "count": keys, "type": "SCALAR",
             "min": [0.0], "max": [times[keys - 1]]},
            {"bufferView": 3, "componentType": 5126, "count": keys, "type": "VEC3"},
            {"bufferView": 4, "componentType": 5126, "count": keys, "type": "VEC3"},
            {"bufferView": 5, "componentType": 5126, "count": keys, "type": "VEC3"}
        ],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 36},
            {"buffer": 0, "byteOffset": 36, "byteLength": 12},
            {"buffer": 0, "byteOffset": 48, "byteLength": t_len},
            {"buffer": 0, "byteOffset": 48 + t_len, "byteLength": v_len},
            {"buffer": 0, "byteOffset": 48 + t_len + v_len, "byteLength": v_len},
            {"buffer": 0, "byteOffset": 48 + t_len + 2 * v_len, "byteLength": v_len}
        ],
        "buffers": [{"byteLength": bin.len()}]
    });
    pack_glb(&doc, &bin)
}

const LOCO_DB: &str = r#"(
  format_version: 1,
  skeleton: "rig.glb#skeleton/Root",
  schema: ( root_joint: "Root", feet: ["FootL", "FootR"] ),
  tags: ["walk"],
  entries: [ ( clip: "rig.glb#clip/walk", looping: true, tags: ["walk"] ) ],
)"#;

#[test]
fn a_project_cook_resolves_gltf_sub_assets_and_the_runtime_loads_the_database() {
    let root = unique_temp_dir("motiondb-project");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    std::fs::write(assets.join("rig.glb"), rig_glb(false)).expect("glb");
    // `a_` sorts before `rig.glb`: the cook must still see the glTF's clips first.
    std::fs::write(assets.join("a_loco.motiondb.ron"), LOCO_DB).expect("motiondb");

    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: regia_asset::current_platform(),
        content_dir: None,
    })
    .expect("server");
    let loaded = load_motion_database(&mut server, "a_loco.motiondb.ron").expect("load");
    assert_eq!(loaded.database.clips().len(), 1);
    assert!(loaded.database.len() >= 30, "{} frames", loaded.database.len());
    assert_eq!(
        loaded.clips.as_ref().map(|c| c.len()),
        Some(1),
        "the walk clip came along"
    );
    assert_eq!(loaded.skeleton.as_ref().map(|s| s.len()), Some(3));
    assert!(loaded.player().is_some(), "enough to play it");

    // A second cook is incremental and gives the same file.
    let id = loaded.id.expect("id");
    let first = server.read_cooked(id).expect("cooked").0;
    let again = cook_project(CookOptions::for_host(root.clone())).expect("cook again");
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    let second = server.read_cooked(id).expect("cooked").0;
    assert_eq!(first, second, "the cooked database is stable across cooks");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_project_cook_fails_a_gltf_clip_with_a_nan_with_its_code() {
    let root = unique_temp_dir("motiondb-nan");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets dir");
    std::fs::write(assets.join("rig.glb"), rig_glb(true)).expect("glb");
    std::fs::write(assets.join("a_loco.motiondb.ron"), LOCO_DB).expect("motiondb");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    let glb = report
        .errors
        .iter()
        .find(|(path, _)| path.ends_with("rig.glb"))
        .unwrap_or_else(|| panic!("the glTF fails: {:?}", report.errors));
    assert!(glb.1.contains(CLIP_NON_FINITE), "{}", glb.1);
    assert!(glb.1.contains("walk"), "names the clip: {}", glb.1);
    let db = report
        .errors
        .iter()
        .find(|(path, _)| path.ends_with("a_loco.motiondb.ron"))
        .unwrap_or_else(|| panic!("the database fails too: {:?}", report.errors));
    assert!(db.1.contains("motiondb_unknown"), "{}", db.1);
    let _ = std::fs::remove_dir_all(&root);
}
