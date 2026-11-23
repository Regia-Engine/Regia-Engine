// SPDX-License-Identifier: MIT-0
//! The tunnels scene's frame job (ADR-8825, plan D8): each view of a view pair (`tube_<pair>` inside a sealed
//! tube, `outdoor_<pair>` on the plain right above) is picked with `tunnels.view`, filmed with a headless
//! `render.out_start` job, and timed with `play.hitch_report` during the job (its `gpu_passes` are the last
//! frame's GPU pass times; `render_ms` of the job is wall time and is not a GPU number). The test also reads the
//! renderer's GPU timings every frame of the job and reports the per-pass medians.
//!
//! Lavapipe by default (a small size, a few frames): the test checks the job runs end to end and both views
//! draw; it asserts no time. The D8 bars are measured on a device with the same test:
//!
//! ```text
//! REGIA_TUNNELS_PROFILE=Deck REGIA_TUNNELS_SIZE=1280x800 REGIA_TUNNELS_FRAMES=240 \
//!   REGIA_TUNNELS_REPORT=/tmp/tunnels-deck.json cargo test --release -p showcase --test tunnels_frames -- --nocapture
//! ```
//!
//! `REGIA_TUNNELS_PAIR` picks the pair (`west` by default), `REGIA_TUNNELS_HITCH_S` the hitch window (seconds).
//! `REGIA_TUNNELS_PROFILE` takes a preset row or a tier by name (`VeryLow`, ADR-8844), and `REGIA_TUNNELS_SCENE` a
//! copy of `tunnels_scene/` for a test binary run away from the source tree (the Deck, the N150, a phone).
//! `REGIA_TUNNELS_MASKED=0|1` switches the CPU masked occlusion cull off or on (ADR-10072; unset: the profile's
//! column), so one binary measures both; each view's row reports `masked_occlusion` (`render.get_occlusion` `masked`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{JobSpec, run_with};
use serde_json::{Value, json};

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The tunnels scene: `REGIA_TUNNELS_SCENE` (a copy of `tunnels_scene/` next to a test binary run on a device),
/// else the sample's own directory.
fn scene_dir() -> std::path::PathBuf {
    env("REGIA_TUNNELS_SCENE").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("tunnels_scene"),
        std::path::PathBuf::from,
    )
}

fn size() -> (u32, u32) {
    env("REGIA_TUNNELS_SIZE")
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((320, 180))
}

/// One frame's GPU timings: the frame time, the node passes and the terrain nodes drawn.
struct Frame {
    frame_ms: f32,
    passes: Vec<(&'static str, f32)>,
    terrain_nodes: u32,
}

/// Per-frame GPU timings of the job, deduplicated by the renderer's frame id.
#[derive(Default)]
struct Samples {
    seen: Option<u64>,
    frames: Vec<Frame>,
}

fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    Some(v[v.len() / 2])
}

fn render_view(id: u32, view: &str, frames: u64) -> Value {
    let (width, height) = size();
    // A preset row or a quality-ladder tier by name (`VeryLow`, `Medium`; ADR-8844), as `--render-profile`.
    let profile = env("REGIA_TUNNELS_PROFILE").map(|name| {
        regia_render::ProfileName::parse(&name).unwrap_or_else(|| panic!("unknown profile {name}"))
    });
    let hitch_s: f64 = env("REGIA_TUNNELS_HITCH_S")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);
    // `REGIA_TUNNELS_MASKED=0|1` sets the CPU masked occlusion column for an A/B on one binary (ADR-10072); unset,
    // the profile's own column.
    let mut calls = vec![
        ("tunnels.state".into(), json!({})),
        ("tunnels.view".into(), json!({ "name": view })),
    ];
    if let Some(masked) = env("REGIA_TUNNELS_MASKED") {
        calls.push(("render.set_occlusion".into(), json!({ "masked": masked == "1" })));
    }
    let spec = JobSpec {
        width,
        height,
        frames,
        warmup_frames: 4,
        // The film look (auto exposure): the plain look's fixed EV 14.5 is a daylight exposure.
        plain: false,
        profile: profile.map(regia_render::ProfileName::preset),
        tier: profile.and_then(regia_render::ProfileName::tier),
        scene: Some(scene_dir()),
        place_camera: false,
        calls,
        during: vec![
            (
                "play.hitch_report".into(),
                json!({ "seconds": hitch_s, "warmup_s": 0.5, "timeout_ms": 1_800_000 }),
            ),
            // What the masked cull did on the last frame (ADR-10072).
            ("render.get_occlusion".into(), json!({})),
        ],
        after: vec![("tunnels.state".into(), json!({}))],
        ..JobSpec::default()
    };
    let samples = Arc::new(Mutex::new(Samples::default()));
    let sink = Arc::clone(&samples);
    let outcome = run_with(id, &spec, move |app| {
        let world = app.world();
        let Some(renderer) = world.get_resource::<regia_render::RuntimeRenderer>() else {
            return;
        };
        let timings = renderer.0.gpu_timings();
        let mut s = sink.lock().expect("samples");
        if timings.frame_id.is_none() || timings.frame_id == s.seen {
            return;
        }
        s.seen = timings.frame_id;
        let nodes = timings.node_passes().iter().map(|p| (p.pass, p.ms)).collect();
        let selected = world
            .get_resource::<showcase::app::TerrainStatsHandle>()
            .map_or(0, |h| h.0.selected_nodes());
        s.frames.push(Frame {
            frame_ms: timings.frame_ms,
            passes: nodes,
            terrain_nodes: selected,
        });
    });

    let stats = outcome.stats.clone().expect("stats");
    assert!(stats.tunnels, "the tunnels are armed in the running app");
    assert!(stats.tunnel_meshes > 0 && stats.tunnel_lights > 0, "{stats:?}");
    let state = &outcome.before[0];
    assert_eq!(state["armed"], json!(true), "{state}");
    assert_eq!(state["lattice_cut"], json!(true), "{state}");
    assert_eq!(outcome.before[1]["view"], json!(view), "{}", outcome.before[1]);
    assert_eq!(outcome.reports[0]["playing"], json!(view));
    assert!(
        outcome.distinct_pixel_values > 16,
        "{view}: the first frame draws a scene ({} pixel values)",
        outcome.distinct_pixel_values
    );
    let hitch = &outcome.during[0];
    assert!(hitch.get("gpu").is_some(), "{view}: hitch report {hitch}");

    // The job's own frames: the last `frames` samples.
    let s = samples.lock().expect("samples");
    let job: Vec<_> = s.frames.iter().rev().take(frames as usize).collect();
    let mut passes: BTreeMap<&str, Vec<f32>> = BTreeMap::new();
    for frame in &job {
        for (pass, ms) in &frame.passes {
            passes.entry(pass).or_default().push(*ms);
        }
    }
    let pass_p50: BTreeMap<&str, f32> = passes
        .into_iter()
        .filter_map(|(k, v)| median(v).map(|m| (k, m)))
        .collect();
    let frame_p50 = median(job.iter().map(|f| f.frame_ms).filter(|ms| *ms > 0.0).collect());
    let nodes_p50 = median(job.iter().map(|f| f.terrain_nodes as f32).collect());
    let render_ms = outcome.manifests.last().and_then(|m| m.get("render_ms").cloned());
    let row = json!({
        "view": view,
        "size": [width, height],
        "profile": env("REGIA_TUNNELS_PROFILE"),
        "frames": frames,
        "gpu_frames_timed": job.len(),
        "gpu_frame_ms_p50": frame_p50,
        "gpu_pass_ms_p50": pass_p50,
        "terrain_nodes_p50": nodes_p50,
        "hitch_report": { "gpu": hitch["gpu"], "gpu_passes": hitch["gpu_passes"], "bound": hitch["bound"] },
        "scatter_instances": stats.scatter_instances,
        "tunnel_meshes": stats.tunnel_meshes,
        "tunnel_lights": stats.tunnel_lights,
        "render_ms_wall": render_ms,
        // The process's peak resident memory (the N150 bar's RAM) and the renderer's GPU memory estimate (VRAM).
        "process_peak_rss_bytes": regia_render::process_peak_rss_bytes(),
        "memory_budget": outcome.memory,
        // The CPU masked occlusion column and its last frame (occluders, tested, culled, CPU microseconds).
        "masked_occlusion": outcome.during.get(1).map(|occlusion| occlusion["masked"].clone()),
    });
    eprintln!("tunnels_frames {view}: {row}");
    // The perf-budget rows `tunnels_{tube,outdoor}_frame_gpu.*` read these (ADR-8873); no line without GPU timings.
    if let (Some(kind), Some(ms)) = (view.split('_').next(), frame_p50) {
        println!("perf-frame tunnels_{kind}_frame_gpu: {ms:.4} ms");
    }
    row
}

#[test]
fn tube_and_outdoor_views_render_with_pass_times() {
    if regia_render::RenderDevice::headless().is_err() {
        eprintln!("skipped: no headless device");
        return;
    }
    let pair = env("REGIA_TUNNELS_PAIR").unwrap_or_else(|| "west".into());
    let frames: u64 = env("REGIA_TUNNELS_FRAMES")
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let tube = render_view(1, &format!("tube_{pair}"), frames);
    let outdoor = render_view(2, &format!("outdoor_{pair}"), frames);
    let ratio = match (
        tube["gpu_frame_ms_p50"].as_f64(),
        outdoor["gpu_frame_ms_p50"].as_f64(),
    ) {
        (Some(t), Some(o)) if o > 0.0 => Some(t / o),
        _ => None,
    };
    let report = json!({
        "pair": pair,
        "tube": tube,
        "outdoor": outdoor,
        "interior_over_outdoor": ratio,
        "bar": "D8: interior frame <= outdoors (ratio <= 1.0) on the Deck preset",
    });
    eprintln!("tunnels_frames report: {report}");
    if let Some(path) = env("REGIA_TUNNELS_REPORT") {
        let text = serde_json::to_string_pretty(&report).expect("json");
        showcase::write_atomic(Path::new(&path), text.as_bytes()).expect("report written");
    }
}
