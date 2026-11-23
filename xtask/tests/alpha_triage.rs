// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask alpha-triage` over a synthetic fixture: telemetry JSONL (with a truncated
//! tail), a telemetry-free log whose header comes from crate text lines, and a crash bundle
//! written through the real feedback path. No DO27 data is committed.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use xtask::alpha_triage::{Args, run};

fn fixture_dir() -> PathBuf {
    std::env::temp_dir().join(format!("regia-alpha-triage-{}", std::process::id()))
}

/// One telemetry JSONL line around `fields.data`.
fn line(t_ms: f64, target: &str, data: serde_json::Value) -> String {
    format!(
        "{{\"t_ms\":{t_ms},\"wall\":\"2026-10-17T18:0{:02}:{:04.1}Z\",\"level\":\"INFO\",\
         \"target\":\"{target}\",\"msg\":\"x\",\"fields\":{{\"data\":{data}}},\"span\":[]}}\n",
        (t_ms as usize) % 60,
        t_ms
    )
}

fn summary(seq: u64, t_s: f64, frames: u64, histo: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "seq": seq,
        "reason": "periodic",
        "t_s": t_s,
        "window_s": 10.0,
        "frames": frames,
        "gaps": 0,
        "frame_ms": {
            "p50": 7.0, "p95": 12.2, "p99": 45.0, "max": 120.5, "mean": 7.1
        },
        "fps_histogram": histo,
        "cpu_ms": {"mean": 3.0, "p95": 5.0, "max": 8.0},
        "gpu_ms": {"mean": 6.0, "p95": 9.0, "max": 40.0},
        "bound": "gpu",
        "hitches": 2,
        "hitches_suppressed": 1,
        "warm": true,
        "created": {"pipelines": 9, "bind_groups": 23, "buffers": 9, "textures": 2,
                     "blocking_polls": 0, "inline_bakes": 0},
        "created_after_warmup": {"pipelines": 3, "bind_groups": 0, "buffers": 12, "textures": 0,
                                  "blocking_polls": 2, "inline_bakes": 0},
        "memory": {"rss_bytes": 8000000000u64, "rss_peak_bytes": 9000000000u64,
                    "vram_allocated_bytes": 8000000000u64, "vram_reserved_bytes": 9000000000u64},
        "frame_breakdown_ms": {
            "update": 1.0, "render": 2.0, "sum_ms": 7.0, "frame_ms": 7.1,
            "unaccounted_ms": 0.1,
            "render_split": {"acquire": 6.4, "render_into": 1.0, "tail": 0.4, "present": 0.5, "other": 0.1}
        },
        "top_systems": {
            "timed": 300,
            "all_systems_ms": 9.0,
            "top": [
                {"system": "do27_shell::sim::step", "mean_ms": 4.2, "p95_ms": 6.0, "max_ms": 9.0, "runs_per_frame": 1.0},
                {"system": "regia_render::extract::extract_scene", "mean_ms": 1.1, "p95_ms": 2.0, "max_ms": 3.0, "runs_per_frame": 1.0}
            ]
        },
        "churn": {
            "created_by_label": [
                {"kind": "Buffer", "label": "mesh verts", "count": 5, "bytes": 8192}
            ],
            "labels_not_kept": 0,
            "pipelines_by_label": [{"label": "lab sky", "count": 3}],
            "upload_bytes": {"total": 2400000, "per_frame": 240000.0, "belt": 1000, "texture": 1000, "mesh": 400, "other_buffer": 0},
            "mesh_buffers_created": 4,
            "cache": {"meshes": 0, "materials": 0, "objects": 0, "meshes_growth": 0, "materials_growth": 0, "objects_growth": 0}
        },
        "host_spans": {
            "frames": 600,
            "main": {
                "frame_ms": 7.1,
                "sections": [{"span": "do27.sim", "target": "do27::frame", "mean_ms": 2.1, "max_ms": 4.0, "calls_per_frame": 1.0}],
                "top_level_ms": 2.1,
                "unattributed_ms": 25.5
            },
            "other_threads": {
                "sections": [{"span": "do27.front_frame", "target": "do27::frame", "mean_ms": 0.5, "max_ms": 1.0, "calls_per_frame": 1.0}]
            }
        },
        "counts": {
            "mesh_instances": {"mean": 1500.0, "max": 2000}, "drawn": {"mean": 900.0, "max": 1000},
            "per_object_draws": {"mean": 10.0, "max": 20}, "local_lights": {"mean": 48.0, "max": 48},
            "shadow_casters": {"mean": 30.0, "max": 40}, "culled": {"mean": 500.0, "max": 600},
            "entities": 200, "entity_slots": 256
        },
        "surface_errors": {"outdated": 0, "lost": 0, "timeout": 0, "other": 0},
        "gpu_errors": 0, "asset_loads": 0, "texture_evictions": 0,
        "notes_dropped": 0, "events_suppressed": 0
    })
}

fn session_a_jsonl() -> String {
    let mut out = String::new();
    // A non-regia line first (start timestamp).
    out.push_str(&line(
        0.0,
        "do27::main",
        serde_json::json!({"tick": 0, "extra_unknown": [1, 2, 3]}),
    ));
    // Session header with settings RON.
    out.push_str(&line(
        2954.4,
        "regia::session",
        serde_json::json!({
            "regia": {"version": "0.1.4", "git": "abc1234", "profile": "release"},
            "os": {"family": "linux", "arch": "x86_64"},
            "cpu_threads": 16,
            "adapter": {
                "name": "AMD Radeon RX 7900 XTX (RADV NAVI31)", "vendor": "0x1002", "device": "0x744c",
                "device_type": "DiscreteGpu", "driver": "radv", "driver_info": "Mesa 26.2.4", "backend": "Vulkan"
            },
            "window": {"mode": "BorderlessFullscreen", "physical_size": [3840, 1600], "present_mode": "Immediate", "scale_factor": 1.0},
            "monitors": [{"name": "DP-3", "width": 3840, "height": 1600, "refresh_hz": 144.05, "scale": 1.0, "current": true}],
            "settings_ron": "(graphics: (preset: Ultra, render_scale: 1.0, aa: Taa, gi: Ddgi))",
            "pipeline_cache": {"enabled": true, "load": "miss", "load_ms": 3.5, "pipelines_created": 32},
            "problems": []
        }),
    ));
    // Two summaries: 1200 frames total across buckets.
    let histo1 = serde_json::json!({"ge_240": 0, "ge_144": 500, "ge_120": 0, "ge_90": 80,
                                     "ge_60": 18, "ge_45": 0, "ge_30": 2, "ge_20": 1, "ge_10": 1, "lt_10": 0});
    let histo2 = serde_json::json!({"ge_240": 0, "ge_144": 590, "ge_120": 10, "ge_90": 0,
                                     "ge_60": 0, "ge_45": 0, "ge_30": 0, "ge_20": 0, "ge_10": 0, "lt_10": 0});
    out.push_str(&line(
        10_000.0,
        "regia::frame_summary",
        summary(1, 10.0, 602, histo1),
    ));
    out.push_str(&line(
        20_000.0,
        "regia::frame_summary",
        summary(2, 20.0, 600, histo2),
    ));
    // A hitch with span evidence.
    out.push_str(&line(
        12_000.0,
        "regia::hitch",
        serde_json::json!({
            "frame": 420, "renderer_frame": 400, "t_s": 12.0,
            "frame_ms": 88.2, "median_ms": 7.0, "ratio": 12.6, "gap": false,
            "cpu_ms": 41.0, "gpu_ms": 0.0, "warm": true,
            "top_cpu_spans": [{"span": "extract_scene", "ms": 40.2}, {"span": "present", "ms": 8.0}],
            "top_gpu_passes": [],
            "top_systems": [{"system": "regia_render::extract::extract_scene", "ms": 30.0, "runs": 1}],
            "created": {"pipelines": 0, "bind_groups": 0, "buffers": 0, "textures": 0,
                         "blocking_polls": 0, "inline_bakes": 0, "labels": []},
            "frame_breakdown_ms": {"sum_ms": 88.2, "render_split": {"acquire": 7.1, "render_into": 40.0, "tail": 1.0, "present": 2.0, "other": 0.5}},
            "host_spans": {"top_level_ms": 3.0, "sections": [{"span": "do27.sim", "ms": 3.0}]}
        }),
    ));
    // A gpu_error with repeats, a device loss, surface errors, problems, stream events.
    out.push_str(&line(15_000.0, "regia::gpu_error",
        serde_json::json!({"kind": "validation", "frame": 900, "pass": "forward_pass", "message": "bind group 7 is invalid", "repeated": 2})));
    out.push_str(&line(
        16_000.0,
        "regia::device_lost",
        serde_json::json!({"reason": "unknown", "frame": 950, "message": "device lost"}),
    ));
    out.push_str(&line(17_000.0, "regia::surface",
        serde_json::json!({"outdated": 2, "lost": 0, "timeout": 1, "other": 0, "total_outdated": 5, "frame": 960, "window_s": 10.0})));
    out.push_str(&line(18_000.0, "regia::problems",
        serde_json::json!({"frame": 970, "count": 1, "problems": [{"code": "texture_budget_exceeded", "message": "1248 MiB over the 1024 MiB budget"}]})));
    out.push_str(&line(19_000.0, "regia::stream",
        serde_json::json!({"kind": "pipeline_after_warmup", "frame": 980, "pipelines": 3, "labels": ["lab sky"], "repeated": 0})));
    out.push_str(&line(19_500.0, "regia::stream",
        serde_json::json!({"kind": "slow_asset_load", "frame": 990, "label": "mesh hero", "ms": 250.0, "repeated": 0})));
    // A truncated tail: no trailing newline.
    out.push_str("{\"t_ms\":21000,\"target\":\"regia::frame_s");
    out
}

fn session_b_jsonl() -> String {
    // No telemetry at all: header facts must come from crate text lines.
    let gpu = "{\"t_ms\":500,\"wall\":\"2026-10-17T19:00:00.000Z\",\"level\":\"INFO\",\"target\":\"regia_render\",\"msg\":\"GPU: NVIDIA GeForce RTX 5090 (Vulkan, DiscreteGpu)\",\"fields\":{},\"span\":[]}\n";
    let os = "{\"t_ms\":510,\"wall\":\"2026-10-17T19:00:00.010Z\",\"level\":\"INFO\",\"target\":\"regia_platform\",\"msg\":\"os: linux TestBox (kernel 7.2.8)\",\"fields\":{},\"span\":[]}\n";
    let configure = "{\"t_ms\":520,\"wall\":\"2026-10-17T19:00:00.020Z\",\"level\":\"INFO\",\"target\":\"regia_render\",\"msg\":\"configure 2560x1440\",\"fields\":{},\"span\":[]}\n";
    let warm = "{\"t_ms\":600,\"wall\":\"2026-10-17T19:00:00.100Z\",\"level\":\"INFO\",\"target\":\"regia_render\",\"msg\":\"pipeline created outside the warm queue at /cargo/checkout/lab.rs:120\",\"fields\":{},\"span\":[]}\n";
    format!("{gpu}{os}{configure}{warm}")
}

#[test]
fn synthetic_fixture_reports_sessions_groups_crashes_and_suspects() {
    let dir = fixture_dir();
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("fixture dir");
    fs::write(
        dir.join("do27-regia-20261017-180000-111.jsonl"),
        session_a_jsonl(),
    )
    .expect("write a");
    fs::write(
        dir.join("do27-regia-20261017-190000-222.jsonl"),
        session_b_jsonl(),
    )
    .expect("write b");

    // A crash bundle through the real feedback writer.
    let input =
        regia_framework::feedback::BundleInput::new(regia_framework::feedback::FeedbackCategory::Crash, "");
    let input = regia_framework::feedback::BundleInput {
        crash: Some(regia_framework::feedback::CrashDetails {
            message: "cannot access a Thread Local Storage value during or after destruction".into(),
            location: "library/std/src/thread/local.rs:429:25".into(),
            backtrace: "   0: regia_framework::feedback::crash::install_crash_hook::{closure#0}\n             at crates/regia_framework/src/feedback/crash.rs:113:24\n   1: alloc::boxed::Box::call\n".into(),
            thread: "main".into(),
            label: String::new(),
        }),
        ..input
    };
    let bundle = regia_framework::feedback::bundle::write_bundle(&dir, &input, 0).expect("bundle");

    let out = dir.join("report.md");
    let code = run(&Args {
        inputs: vec![dir.clone()],
        out: Some(out.clone()),
        json: true,
    })
    .expect("run");
    assert_eq!(code, ExitCode::SUCCESS);

    let md = fs::read_to_string(&out).expect("md");
    let json = fs::read_to_string(dir.join("report.json")).expect("json");
    let summary: serde_json::Value = serde_json::from_str(&json).expect("json parses");

    // Both sessions, worst first (a has frames, b sinks).
    assert_eq!(summary["sessions"].as_array().map(Vec::len), Some(2), "{json}");
    assert!(md.contains("do27-regia-20261017-180000-111.jsonl"), "{md}");
    assert!(md.contains("do27-regia-20261017-190000-222.jsonl"), "{md}");

    // Header facts.
    assert!(md.contains("RX 7900 XTX"), "{md}");
    assert!(md.contains("Ultra"), "{md}");
    assert!(md.contains("3840×1600"), "{md}");
    assert!(md.contains("RTX 5090"), "{md}");
    assert!(md.contains("2560×1440"), "{md}");

    // Frame stats from merged histograms: p50 ~5.7, p95 ~9.7, p99 ~13.6, >33ms 0.2%, >50ms 0.1%.
    let a = &summary["sessions"][0];
    let close = |got: f64, want: f64, tol: f64| (got - want).abs() < tol;
    assert!(
        close(a["frame_p50_ms"].as_f64().unwrap_or(0.0), 5.7, 0.5),
        "{}",
        a["frame_p50_ms"]
    );
    assert!(
        close(a["frame_p95_ms"].as_f64().unwrap_or(0.0), 9.7, 0.5),
        "{}",
        a["frame_p95_ms"]
    );
    assert!(
        close(a["frame_p99_ms"].as_f64().unwrap_or(0.0), 14.2, 0.5),
        "{}",
        a["frame_p99_ms"]
    );
    assert!(
        close(a["pct_over_33"].as_f64().unwrap_or(0.0), 0.167, 0.05),
        "{}",
        a["pct_over_33"]
    );
    assert!(
        close(a["pct_over_50"].as_f64().unwrap_or(0.0), 0.083, 0.05),
        "{}",
        a["pct_over_50"]
    );
    assert_eq!(a["frames"].as_u64(), Some(1202));

    // Hitches and causes.
    assert!(md.contains("span extract_scene"), "{md}");
    assert!(md.contains("88.2 ms"), "{md}");

    // Groups.
    let groups = summary["groups"].as_array().expect("groups");
    assert!(
        groups
            .iter()
            .any(|g| g["vendor"] == "amd" && g["family"] == "RX 7900 XTX" && g["preset"] == "Ultra"),
        "{json}"
    );
    assert!(
        groups
            .iter()
            .any(|g| g["vendor"] == "nvidia" && g["family"] == "RTX 5090"),
        "{json}"
    );

    // Errors table.
    assert!(md.contains("| gpu errors |"), "{md}");
    assert!(md.contains("texture_budget_exceeded"), "{md}");
    assert!(
        summary["error_sessions"].as_array().map(Vec::len) == Some(1),
        "{json}"
    );

    // Crash signature bucketed by panic message + regia frame.
    let sigs = summary["crash_signatures"].as_array().expect("signatures");
    assert_eq!(sigs.len(), 1, "{json}");
    assert!(
        sigs[0]["signature"]
            .as_str()
            .unwrap_or("")
            .contains("cannot access a Thread Local Storage"),
        "{json}"
    );
    assert!(md.contains(bundle.name.as_str()), "{md}");

    // Suspected engine issues.
    let checks: Vec<&str> = summary["suspected"]
        .as_array()
        .map(|list| list.iter().filter_map(|x| x["check"].as_str()).collect())
        .unwrap_or_default();
    for expected in [
        "no-telemetry",
        "pipelines-after-warmup",
        "steady-state-creations",
        "acquire-over-5ms",
        "unattributed-over-20ms",
        "gpu-errors",
        "device-lost",
        "surface-errors",
        "render-problems",
        "slow-asset-loads",
        "blocking-polls-after-warmup",
    ] {
        assert!(checks.contains(&expected), "missing {expected} in {checks:?}");
    }
    assert!(md.contains("render_split.acquire mean 6.4 ms"), "{md}");
    assert!(md.contains("unattributed_ms mean 25.5 ms"), "{md}");

    // Tolerances: the truncated tail was not a bad line.
    assert_eq!(a["bad_lines"].as_u64(), Some(0));
    assert_eq!(a["truncated_tail"].as_bool(), Some(true));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn empty_input_fails() {
    let dir = std::env::temp_dir().join(format!("regia-alpha-triage-empty-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("dir");
    let err = run(&Args {
        inputs: vec![dir.clone()],
        out: None,
        json: false,
    })
    .expect_err("no inputs");
    assert!(err.to_string().contains("found no session logs"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}

/// A JSONL line whose `fields.data` is the older JSON *string* (and, optionally, `data_json`).
fn string_line(t_ms: f64, target: &str, data: &serde_json::Value, key: &str) -> String {
    format!(
        "{{\"t_ms\":{t_ms},\"wall\":\"2026-10-18T10:00:{:04.1}Z\",\"level\":\"INFO\",\"target\":\"{target}\",\
         \"msg\":\"x\",\"fields\":{{{}:{}}},\"span\":[]}}\n",
        t_ms / 1000.0,
        serde_json::to_string(key).expect("key"),
        serde_json::to_string(&data.to_string()).expect("string")
    )
}

#[test]
fn summary_json_phases_and_both_data_shapes_are_read() {
    let dir = std::env::temp_dir().join(format!("regia-alpha-triage-contract-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("dir");

    // Session with nested data, phases, and a final summary beside it.
    let mut log = String::new();
    log.push_str(&line(
        100.0,
        "regia::session",
        serde_json::json!({
            "regia": {"version": "0.1.9", "git": "abc", "profile": "release"},
            "os": {"family": "windows", "arch": "x86_64"}, "os_version": "Windows 11 Pro 25H2",
            "cpu": "AMD Ryzen 7 7800X3D", "ram_gb": 31.2, "vram_gb": 16.0, "cpu_threads": 16,
            "adapter": {"name": "NVIDIA GeForce RTX 4080", "vendor": "0x10de", "driver": "617.42", "backend": "Vulkan", "device_type": "DiscreteGpu"}
        }),
    ));
    log.push_str(&line(
        900.0,
        "regia::phase",
        serde_json::json!({"phase": "boot_done", "ms": 880.0}),
    ));
    log.push_str(&line(
        9000.0,
        "regia::phase",
        serde_json::json!({"phase": "shaders_ready", "pipelines": 146, "ms": 9100.0, "ms_each": 62.3, "cache": "miss", "threads": 4, "backend": "Vulkan"}),
    ));
    log.push_str(&line(
        20_000.0,
        "regia::phase",
        serde_json::json!({"phase": "scene_load_end", "name": "bore_twelve", "ms": 3100.0, "mb_uploaded": 1010.0, "pipelines": 72, "meshes": 640}),
    ));
    // Old shapes: the data as a JSON string, and only as `data_json`.
    log.push_str(&string_line(
        21_000.0,
        "regia::frame_summary",
        &summary(
            1,
            21.0,
            600,
            serde_json::json!({"ge_144": 600, "ge_240": 0, "ge_120": 0, "ge_90": 0, "ge_60": 0, "ge_45": 0, "ge_30": 0, "ge_20": 0, "ge_10": 0, "lt_10": 0}),
        ),
        "data",
    ));
    log.push_str(&string_line(
        22_000.0,
        "regia::gpu_error",
        &serde_json::json!({"kind": "validation", "frame": 5, "message": "oops", "repeated": 0}),
        "data_json",
    ));
    fs::write(dir.join("do27-20261018-100000-7.jsonl"), log).expect("write log");
    fs::write(
        dir.join("do27-20261018-100000-7.summary.json"),
        serde_json::json!({
            "schema": 1,
            "settings": {"preset": "High", "upscaler": "TAAU", "auto_plan": {"tier": "High", "reason": "RTX 4080 -> High"}},
            "session": {"seconds": 1240.0, "frames": 51234, "maps": ["bore_twelve", "switchbacks"]},
            "frames": {"p50_ms": 18.2, "p95_ms": 31.0, "p99_ms": 54.0, "bound": "cpu"},
            "frames_by_map": {
                "bore_twelve": {"frames": 30000, "p50_ms": 15.0, "p95_ms": 22.0, "p99_ms": 40.0, "over_33ms_pct": 1.0, "over_50ms_pct": 0.2},
                "switchbacks": {"frames": 21234, "p50_ms": 21.0, "p95_ms": 38.0, "p99_ms": 61.0, "over_33ms_pct": 9.5, "over_50ms_pct": 3.1}
            },
            "stalls": [{"t_s": 349.7, "ms": 3120.0, "phase": "scene_load_start"}],
            "shaders": {"backend": "Vulkan", "driver_cache": "off"},
            "host": {"app": "do27"},
            "exit": {"clean": true, "ms": 255.0, "watchdog": false},
            "previous_exit": {"clean": false, "last_partial": {"session": {"seconds": 412.0, "frames": 18000}}}
        })
        .to_string(),
    )
    .expect("write summary");

    // A hung session: only its partial, no log.
    fs::write(
        dir.join("hung.summary.partial.json"),
        serde_json::json!({
            "schema": 1, "partial": true,
            "system": {"gpu": "AMD Radeon RX 7900 XTX", "vendor": "0x1002"},
            "session": {"seconds": 412.0, "frames": 18000},
            "frames": {"p50_ms": 9.0, "p95_ms": 14.0, "p99_ms": 20.0}
        })
        .to_string(),
    )
    .expect("write partial");

    let out = dir.join("report.md");
    let code = run(&Args {
        inputs: vec![dir.clone()],
        out: Some(out.clone()),
        json: true,
    })
    .expect("run");
    assert_eq!(code, ExitCode::SUCCESS);
    let md = fs::read_to_string(&out).expect("md");
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("report.json")).expect("json")).expect("parses");
    assert_eq!(json["sessions"].as_array().map(Vec::len), Some(2), "{json}");

    let logged = json["sessions"]
        .as_array()
        .and_then(|l| l.iter().find(|s| s["summary_kind"] == "final"))
        .expect("the logged session");
    // Both data shapes parsed: the string summary gave frames, the data_json error was counted.
    assert_eq!(logged["frames"].as_u64(), Some(600), "{logged}");
    assert_eq!(logged["telemetry_events"].as_u64(), Some(6), "{logged}");
    assert!(
        md.contains("gpu error: oops") || logged["gpu_errors"].as_u64() == Some(1),
        "{md}"
    );
    // Summary-first facts and phases.
    assert_eq!(logged["cpu_model"], "AMD Ryzen 7 7800X3D");
    assert_eq!(logged["upscaler"], "TAAU");
    assert_eq!(logged["auto_plan"], "RTX 4080 -> High");
    assert_eq!(logged["boot_done_ms"].as_f64(), Some(880.0));
    assert_eq!(logged["maps"].as_array().map(Vec::len), Some(2));
    assert_eq!(logged["driver_cache"], "off");
    assert_eq!(logged["previous_exit"]["clean"], false);
    assert!(md.contains("| switchbacks | 21234 |"), "{md}");
    assert!(md.contains("shaders_ready 146 pipelines in 9100.0 ms"), "{md}");
    assert!(
        md.contains("scene_load bore_twelve 3100.0 ms, 1010.0 MB uploaded, 72 pipelines, 640 meshes"),
        "{md}"
    );
    assert!(md.contains("previous session: **ended uncleanly**"), "{md}");
    assert!(md.contains("clean 255 ms"), "{md}");

    let checks: Vec<(&str, &str)> = json["suspected"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|x| Some((x["check"].as_str()?, x["session"].as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        checks.iter().any(|(c, _)| *c == "previous-session-unclean"),
        "{checks:?}"
    );
    assert!(checks.iter().any(|(c, _)| *c == "stalls"), "{checks:?}");
    assert!(
        checks.contains(&("hard-lock-suspected", "hung.summary.partial.json")),
        "{checks:?}"
    );
    // The partial-only session still has a row with its numbers.
    let hung = json["sessions"]
        .as_array()
        .and_then(|l| l.iter().find(|s| s["summary_kind"] == "partial"))
        .expect("the hung session");
    assert_eq!(hung["frame_p95_ms"].as_f64(), Some(14.0));
    assert_eq!(hung["gpu_vendor"], "amd");
    let _ = fs::remove_dir_all(&dir);
}
