// SPDX-License-Identifier: MPL-2.0
//! Black-box tests of `regia bench` through the real binary: what it prints and exits with, and
//! that a measured frame actually drew something (a GPU or lavapipe is needed for those tests; they
//! say so and return when there is no device, since a CPU-only host cannot run them).

use std::process::{Command, Output};

use regia_test_utils::unique_temp_dir;
use serde_json::Value;

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .output()
        .expect("spawn the regia binary")
}

fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON object ({err}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// The report, or `None` on a host with no device.
fn measured(args: &[&str]) -> Option<Value> {
    let out = regia(args);
    let value = json_of(&out);
    if value["code"] == "bench_no_gpu" {
        eprintln!("skip: no device on this host: {}", value["message"]);
        return None;
    }
    assert!(out.status.success(), "{value}");
    Some(value)
}

#[test]
fn list_names_the_set_and_its_licences() {
    let out = regia(&["bench", "--list", "--json"]);
    assert!(out.status.success());
    let value = json_of(&out);
    let scenes = value["scenes"].as_array().expect("scenes");
    let find = |id: &str| scenes.iter().find(|s| s["id"] == id).expect(id);
    assert_eq!(find("flight_helmet")["licence"], "CC0-1.0");
    assert_eq!(find("flight_helmet")["publishable"], true);
    assert_eq!(find("sponza_khronos")["publishable"], false);
    assert_eq!(find("synthetic_atrium")["synthetic"], true);
    assert_eq!(find("synthetic_atrium")["on_disk"], true);
}

#[test]
fn a_refused_request_exits_2_with_its_code() {
    let dir = unique_temp_dir("bench_refused");
    let assets = dir.to_str().expect("utf-8");
    for (args, code) in [
        (vec!["bench", "no_such_scene"], "bench_unknown_scene"),
        (vec!["bench", "flight_helmet"], "bench_scene_missing"),
        (vec!["bench", "sponza_khronos"], "bench_scene_restricted"),
        (
            vec!["bench", "synthetic_cubes", "--frames", "0"],
            "bench_bad_args",
        ),
    ] {
        let mut args = args;
        args.extend(["--assets", assets, "--json"]);
        let out = regia(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let value = json_of(&out);
        assert_eq!(value["ok"], false);
        assert_eq!(value["code"], code);
    }
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn the_human_error_names_its_code_on_stderr() {
    let out = regia(&["bench", "no_such_scene"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bench_unknown_scene"), "{stderr}");
}

#[test]
fn the_fetch_script_runs_the_command_it_documents() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fetch-bench-scenes.sh");
    let text = std::fs::read_to_string(script).expect("the fetch script");
    assert!(text.contains("-- bench --fetch \"$@\""), "{text}");
}

/// The whole path on a synthetic scene: requested frame count, stable scene counts, real draws, a
/// frame with something in it, and the PNG.
#[test]
fn a_synthetic_scene_measures_real_frames() {
    let dir = unique_temp_dir("bench_synthetic");
    let png = dir.join("frames");
    let Some(report) = measured(&[
        "bench",
        "synthetic_cubes",
        "--frames",
        "3",
        "--warmup",
        "1",
        "--size",
        "64x36",
        "--png-dir",
        png.to_str().expect("utf-8"),
        "--json",
    ]) else {
        return;
    };
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["size"], serde_json::json!([64, 36]));
    let scene = &report["scenes"][0];
    assert_eq!(scene["id"], "synthetic_cubes");
    assert_eq!(scene["frames"], 3);
    assert_eq!(scene["triangles"], 120_000);
    assert_eq!(scene["meshes"], 10_000);
    assert!(scene["wall_ms"]["p50"].as_f64().unwrap_or(0.0) > 0.0, "{scene}");
    assert!(scene["draws"]["drawn"].as_u64().unwrap_or(0) > 0, "{scene}");
    assert!(
        scene["last_frame"]["lit_pct"].as_f64().unwrap_or(0.0) > 1.0,
        "{scene}"
    );
    let frame = image::open(png.join("synthetic_cubes.png")).expect("the frame");
    assert_eq!((frame.width(), frame.height()), (64, 36));
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// The interior scene keeps its camera inside and culls: it is there to stress occlusion and lights.
#[test]
fn the_atrium_draws_from_inside_and_culls() {
    let Some(report) = measured(&[
        "bench",
        "synthetic_atrium",
        "--frames",
        "2",
        "--warmup",
        "0",
        "--size",
        "64x36",
        "--json",
    ]) else {
        return;
    };
    let scene = &report["scenes"][0];
    assert_eq!(scene["materials"], 500);
    assert!(scene["meshes"].as_u64().unwrap_or(0) > 1_500, "{scene}");
    assert!(scene["draws"]["drawn"].as_u64().unwrap_or(0) > 0, "{scene}");
    assert!(scene["draws"]["culled"].as_u64().unwrap_or(0) > 0, "{scene}");
    assert!(
        scene["last_frame"]["lit_pct"].as_f64().unwrap_or(0.0) > 1.0,
        "{scene}"
    );
}

/// A file through the real glTF path is measured and never reads as publishable.
#[test]
fn a_gltf_file_is_measured_and_marked_local() {
    let dir = unique_temp_dir("bench_gltf_file");
    let positions: [[f32; 3]; 3] = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [0.0, 1.0, 0.0]];
    let bytes: Vec<u8> = positions.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(dir.join("tri.bin"), &bytes).expect("write");
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],
        "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"material":0}}]}}],
        "materials":[{{"doubleSided":true}}],
        "buffers":[{{"uri":"tri.bin","byteLength":{len}}}],"bufferViews":[{{"buffer":0,"byteLength":{len}}}],
        "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[-1,-1,0],"max":[1,1,0]}}]}}"#,
        len = bytes.len()
    );
    let path = dir.join("tri.gltf");
    std::fs::write(&path, json).expect("write");
    let Some(report) = measured(&[
        "bench",
        "--gltf",
        path.to_str().expect("utf-8"),
        "--frames",
        "2",
        "--warmup",
        "0",
        "--size",
        "32x32",
        "--json",
    ]) else {
        std::fs::remove_dir_all(&dir).expect("cleanup");
        return;
    };
    let scenes = report["scenes"].as_array().expect("scenes");
    assert_eq!(scenes.len(), 1, "a file alone measures just that file");
    assert_eq!(scenes[0]["id"], "file:tri");
    assert_eq!(scenes[0]["publishable"], false);
    assert_eq!(scenes[0]["triangles"], 1);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// The generated scenes with counters are in the list, as the project's own code (ADR-2695).
#[test]
fn list_names_the_generated_scenes_with_counters() {
    let out = regia(&["bench", "--list", "--json"]);
    assert!(out.status.success());
    let value = json_of(&out);
    let scenes = value["scenes"].as_array().expect("scenes");
    for id in ["synthetic_grass_1m", "template_npc_200"] {
        let scene = scenes.iter().find(|s| s["id"] == id).expect(id);
        assert_eq!(scene["synthetic"], true, "{id}");
        assert_eq!(scene["publishable"], true, "{id}");
        assert_eq!(scene["on_disk"], true, "{id}");
        assert_eq!(scene["licence"], "Regia Engine's own code", "{id}");
    }
}

/// One small-size run of a generated scene: two measured frames after one warm-up.
fn small(id: &str) -> Option<Value> {
    measured(&[
        "bench", id, "--small", "--frames", "2", "--warmup", "1", "--size", "96x54", "--json",
    ])
}

/// The renderer's own count of the grass field is the plan's product, and the wind clock ran.
#[test]
fn the_grass_scene_reports_its_blade_count_and_sways() {
    let Some(report) = small("synthetic_grass_1m") else {
        return;
    };
    let scene = &report["scenes"][0];
    assert_eq!(scene["size"], "small");
    let counters = &scene["counters"];
    assert_eq!(counters["kind"], "grass");
    // 2 x 2 cells x 250 clumps x 10 blades.
    assert_eq!(counters["field_blades"], 10_000, "{counters}");
    assert_eq!(counters["census"]["blades"], 10_000, "{counters}");
    assert_eq!(counters["census"]["candidates"], 1_000, "{counters}");
    assert_eq!(counters["census"]["patches"], 4, "{counters}");
    assert_eq!(counters["last_frame"]["drawing"], true, "{counters}");
    assert_eq!(counters["last_frame"]["walked_blades"], 10_000, "{counters}");
    assert_eq!(counters["last_frame"]["quality"], "ultra", "{counters}");
    assert!(
        counters["wind"]["strength"].as_f64().unwrap_or(0.0) > 0.0,
        "{counters}"
    );
    assert!(
        counters["wind"]["clock_s"].as_f64().unwrap_or(0.0) > 0.0,
        "{counters}"
    );
    assert_eq!(counters["wind"]["frames_prepared"], 3, "{counters}");
    assert!(
        scene["last_frame"]["lit_pct"].as_f64().unwrap_or(0.0) > 1.0,
        "{scene}"
    );
}

/// Two runs give the same counters: the field and the clock are functions of the frame index.
#[test]
fn the_grass_counters_repeat_between_runs() {
    let (Some(first), Some(second)) = (small("synthetic_grass_1m"), small("synthetic_grass_1m")) else {
        return;
    };
    assert_eq!(first["scenes"][0]["counters"], second["scenes"][0]["counters"]);
}

/// Every NPC is drawn skinned and animated, and the totals follow the fixed steps.
#[test]
fn the_npc_scene_draws_every_agent_skinned_and_counts_animation_updates() {
    let Some(report) = small("template_npc_200") else {
        return;
    };
    let scene = &report["scenes"][0];
    assert_eq!(scene["size"], "small");
    let counters = &scene["counters"];
    assert_eq!(counters["kind"], "npc");
    assert_eq!(counters["agents"], 20, "{counters}");
    assert_eq!(counters["skinned_draws"], 20, "{counters}");
    // One fixed step per frame: one warm-up and two measured.
    assert_eq!(counters["animation"]["steps"], 3, "{counters}");
    let updates = counters["animation"]["updates"].as_u64().unwrap_or(0);
    let skipped = counters["animation"]["skipped"].as_u64().unwrap_or(0);
    assert_eq!(updates + skipped, 3 * 20, "{counters}");
    assert!(updates > 0, "{counters}");
    assert!(
        counters["animation"]["poses_evaluated"].as_u64().unwrap_or(0) > 0,
        "{counters}"
    );
    assert!(
        scene["last_frame"]["lit_pct"].as_f64().unwrap_or(0.0) > 1.0,
        "{scene}"
    );
}

/// Two runs give the same counters and the same crowd digests: a fixed path, seed and step count.
#[test]
fn the_npc_counters_and_digests_repeat_between_runs() {
    let (Some(first), Some(second)) = (small("template_npc_200"), small("template_npc_200")) else {
        return;
    };
    assert_eq!(first["scenes"][0]["counters"], second["scenes"][0]["counters"]);
    assert_ne!(
        first["scenes"][0]["counters"]["path"]["crowd_digest"],
        Value::Null
    );
}
