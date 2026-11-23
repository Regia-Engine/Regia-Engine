// SPDX-License-Identifier: MIT-0
//! A render-out through the agent protocol: 8 frames at 640x360 on lavapipe (or whatever adapter
//! the machine has), twice from fresh apps, with identical frame hashes. Without any GPU adapter
//! the test prints a note and passes, like the other render tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{JobSpec, run};
use regia_render::RenderDevice;

#[test]
fn eight_frames_at_640x360_render_twice_with_identical_hashes() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let spec = JobSpec {
        width: 640,
        height: 360,
        frames: 8,
        warmup_frames: 0,
        plain: true,
        scatter_lod: 2,
        exposure_ev: None,
        repeat: 1,
        // `SHOWCASE_KEEP_FRAMES=<dir>` keeps the first and last frame of run 1 for a look.
        keep: std::env::var_os("SHOWCASE_KEEP_FRAMES").map(Into::into),

        ..JobSpec::default()
    };
    let a = run(1, &spec);
    let b = run(2, &JobSpec { keep: None, ..spec });
    assert!(
        a.distinct_pixel_values > 8,
        "the frame is not flat: {} colours",
        a.distinct_pixel_values
    );
    assert_eq!(
        a.hashes, b.hashes,
        "two runs of the same request give the same frames"
    );
    assert_eq!(a.sequence, b.sequence);
    let first = a.hashes.first().expect("hashes");
    assert!(
        a.hashes.iter().any(|h| h != first),
        "the clock moved the scene between frames"
    );
}

/// The parts of a manifest a repeat render must reproduce.
fn repeatable(manifest: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "frame_hashes": manifest["frame_hashes"],
        "aov_frame_hashes": manifest["aov"]["frame_hashes"],
        "aov_ids": manifest["aov"]["ids"],
        "mix": manifest["audio"]["mix"]["hash"],
        "mix_samples": manifest["audio"]["mix"]["samples"],
    })
}

/// The app clocks a manifest's `clock` block says a job started from. `app_frame_at_start` is left
/// out: it counts app updates, and a harness that free-runs `App::update` while its client connects
/// runs a different number of them on every run (the clock is paused, so they change nothing).
fn start_times(manifest: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "game": manifest["clock"]["app_game_time_at_start_ns"],
        "sim": manifest["clock"]["app_sim_time_at_start_ns"],
    })
}

/// Red-Rouge v0.2.4 smoke (ADR-1656, integ-fixes-v026b): a repeat render of one project with AOVs
/// and audio. Two fresh apps reproduce the frames, every AOV's hashes, the id table and the mix, in
/// the first job and in the second. The second job in one app starts where the first left the
/// director (the showcase's timeline is the scene's state, ADR-1881), so it shows later frames of
/// that timeline: that is printed (run with `--nocapture`), not asserted.
#[test]
fn a_repeat_render_with_aovs_and_audio_reproduces_from_a_fresh_app() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let spec = JobSpec {
        width: 320,
        height: 180,
        frames: 6,
        repeat: 2,
        extra: serde_json::json!({
            "aovs": ["depth", "id", "motion"], "aov_format": "raw",
            "audio": { "sample_rate": 24000 },
        }),
        ..JobSpec::default()
    };
    let a = run(11, &spec);
    let b = run(12, &spec);
    assert_eq!(a.manifests.len(), 2);
    assert_eq!(b.manifests.len(), 2);
    for job in 0..2 {
        let (ma, mb) = (&a.manifests[job], &b.manifests[job]);
        assert!(
            ma["aov"]["frame_hashes"]["id"]
                .as_array()
                .is_some_and(|h| h.len() == 6),
            "{}",
            ma["aov"]
        );
        assert!(
            ma["clock"]["app_frame_at_start"].is_u64() && mb["clock"]["app_frame_at_start"].is_u64(),
            "job {job}: {} / {}",
            ma["clock"],
            mb["clock"]
        );
        assert_eq!(
            start_times(ma),
            start_times(mb),
            "job {job}: fresh apps start the job at the same app times"
        );
        assert_eq!(repeatable(ma), repeatable(mb), "job {job} of two fresh apps");
    }
    let (first, second) = (&a.manifests[0], &a.manifests[1]);
    assert_ne!(
        start_times(first),
        start_times(second),
        "the second job starts later on the app clock"
    );
    let same = |key: &str| repeatable(first)[key] == repeatable(second)[key];
    eprintln!(
        "second job in one app: clock {} -> {}; frames equal {}, aov hashes equal {}, aov ids equal {}, mix equal {}",
        first["clock"],
        second["clock"],
        same("frame_hashes"),
        same("aov_frame_hashes"),
        same("aov_ids"),
        same("mix"),
    );
}

/// Two jobs in one app (Red-Rouge v0.2.6: 0 of 48 depth hashes matched, ADR-1881). With a `time_map`
/// of scale 0 the fixed loop never steps, so the director and the track stay where they are and the
/// world is the same at the start of both jobs. Everything the render clock moves still runs, and
/// nothing the first job left behind may show in the second: the frames, the AOVs and the mix repeat
/// exactly, in the plain look and in the film look (TAA, bloom, auto exposure). The warm-up is part
/// of the case: on a fresh host frame 0's depth differs from the rest of this held shot without it,
/// a world the second job never sees.
fn two_jobs_from_one_world_state(plain: bool) {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let spec = JobSpec {
        width: 320,
        height: 180,
        frames: 6,
        warmup_frames: 2,
        plain,
        repeat: 2,
        extra: serde_json::json!({
            "aovs": ["depth", "id", "motion"], "aov_format": "raw",
            "audio": { "sample_rate": 24000 },
            "time_map": [{ "frame": 0, "scale": 0 }],
            "cuts": [3],
        }),
        ..JobSpec::default()
    };
    let out = run(13, &spec);
    assert_eq!(out.manifests.len(), 2);
    let (first, second) = (&out.manifests[0], &out.manifests[1]);
    assert_ne!(start_times(first), start_times(second), "the app clock moved on");
    assert_eq!(repeatable(first), repeatable(second));
}

#[test]
fn two_jobs_from_one_world_state_repeat_in_one_app() {
    two_jobs_from_one_world_state(true);
}

#[test]
fn two_jobs_from_one_world_state_repeat_in_the_film_look() {
    two_jobs_from_one_world_state(false);
}

/// Red-Rouge v0.2.8 (ADR-1965): with the world held (`time_map` scale 0), two 24-frame jobs with 16
/// warm-up frames in one host repeated the frames, depth and id but not every motion frame (22 and 23
/// of 24 in the default profile, 5 to 10 in the cinematic one). The motion AOV repeats with the rest.
fn held_world_motion_repeats(profile: Option<regia_render::RenderProfilePreset>, plain: bool) {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let spec = JobSpec {
        width: 320,
        height: 180,
        frames: 24,
        warmup_frames: 16,
        fps: 24,
        plain,
        profile,
        repeat: 2,
        extra: serde_json::json!({
            "aovs": ["depth", "id", "motion"], "aov_format": "raw",
            "time_map": [{ "frame": 0, "scale": 0, "ease": "hold" }],
        }),
        ..JobSpec::default()
    };
    let out = run(14, &spec);
    assert_eq!(out.manifests.len(), 2);
    let (first, second) = (&out.manifests[0], &out.manifests[1]);
    for aov in ["depth", "id", "motion"] {
        let a = first["aov"]["frame_hashes"][aov]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let b = second["aov"]["frame_hashes"][aov]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(a.len(), 24, "{aov}");
        let differ: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
        assert!(differ.is_empty(), "{aov} differs on frames {differ:?}");
    }
    assert_eq!(repeatable(first), repeatable(second));
}

#[test]
fn held_world_motion_repeats_in_the_default_profile() {
    held_world_motion_repeats(None, false);
}

#[test]
fn held_world_motion_repeats_in_the_cinematic_profile() {
    held_world_motion_repeats(Some(regia_render::RenderProfilePreset::Cinematic), false);
}

/// Red-Rouge v0.2.8 (ADR-1965): a headless host's first job compiled the pipelines on the app thread
/// before frame 0 (30 s on the showcase scene). `render.pipeline_warm { offscreen }` warms the
/// renderer on a thread of its own before the first job (a stand-in holds its place meanwhile);
/// with the world held, the first job and the second still repeat everything (counters and hashes,
/// not times).
#[test]
fn an_offscreen_warm_before_the_first_job_keeps_two_jobs_equal() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let spec = JobSpec {
        width: 320,
        height: 180,
        frames: 4,
        warmup_frames: 2,
        plain: false,
        repeat: 2,
        offscreen_warm: true,
        extra: serde_json::json!({
            "aovs": ["depth", "id", "motion"], "aov_format": "raw",
            "time_map": [{ "frame": 0, "scale": 0 }],
        }),
        ..JobSpec::default()
    };
    let out = run(16, &spec);
    let (warm, longest) = out.offscreen_warm.clone().expect("the warm ran");
    let last = &warm["offscreen_warm"]["last"];
    assert_eq!(last["renderer_back"], serde_json::json!(true), "{last}");
    assert_eq!(last["error"], serde_json::Value::Null, "{last}");
    assert!(last["pipelines"].as_u64().is_some_and(|n| n > 0), "{last}");
    assert_eq!(last["size"], serde_json::json!([320, 180]), "{last}");
    println!(
        "offscreen warm {last}; longest answer {longest} ms; jobs {:?}",
        out.poll_gaps_ms
    );
    assert_eq!(repeatable(&out.manifests[0]), repeatable(&out.manifests[1]));
}

/// Red-Rouge v0.2.8 (item 3 of ADR-1965): a host's first job was silent for 30 s before frame 0 after the
/// pipeline warm was done. Prints what the first job of a fresh host creates outside the warm list and the
/// longest wait for a poll, against a second job (measurement only; `SHOWCASE_TEST_SCENE` picks the scene).
#[test]
#[ignore = "measurement: run by hand with --nocapture"]
fn first_job_after_the_warm_measured() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let spec = JobSpec {
        width: 320,
        height: 180,
        frames: 6,
        warmup_frames: 0,
        fps: 6,
        plain: false,
        repeat: 2,
        // `SHOWCASE_OFFSCREEN_WARM=1`: ask for the offscreen warm first, as the fix's client does.
        offscreen_warm: std::env::var_os("SHOWCASE_OFFSCREEN_WARM").is_some(),
        // A large scene's first frames can hold a loaded host past the default 30 s answer wait.
        calls: vec![(
            "render.pipeline_warm".into(),
            serde_json::json!({ "timeout_ms": 600_000 }),
        )],
        after: vec![(
            "render.pipeline_warm".into(),
            serde_json::json!({ "timeout_ms": 600_000 }),
        )],
        extra: serde_json::json!({
            "audio": { "sample_rate": 96000, "format": "pcm24",
                       "stems": ["Master", "Music", "Sfx", "World", "Ui", "Voice", "Ambience", "Reverb"] },
        }),
        ..JobSpec::default()
    };
    let out = run(15, &spec);
    let (before, after) = (&out.before[0], &out.reports[0]);
    println!(
        "job ms {:?} poll gaps (longest call, longest gap) ms {:?}",
        out.all_job_ms, out.poll_gaps_ms
    );
    println!(
        "pipelines outside the warm list: before {} after {}; created before {} after {}",
        before["pipelines_outside"],
        after["pipelines_outside"],
        before["pipelines_created"],
        after["pipelines_created"]
    );
    println!("outside sites before {}", before["outside_sites"]);
    println!("outside sites after  {}", after["outside_sites"]);
    if let Some((warm, longest)) = &out.offscreen_warm {
        println!(
            "offscreen warm {} (longest answer {longest} ms)",
            warm["offscreen_warm"]
        );
    }
}

/// One way Red-Rouge ran two identical jobs in one host (v0.2.13 retest, ADR-2582).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    /// A held camera, nothing armed.
    Plain,
    /// A camera dolly set again before each job, the game clock paused until the job takes it.
    PathArmed,
    /// The world held by a `time_map` of scale 0.
    Held,
    /// Held, with the dolly set again before each job.
    HeldPathArmed,
}

impl Variant {
    fn held(self) -> bool {
        matches!(self, Self::Held | Self::HeldPathArmed)
    }

    fn path(self) -> bool {
        matches!(self, Self::PathArmed | Self::HeldPathArmed)
    }
}

/// Red-Rouge v0.2.13 on an RTX 5090 (ADR-2582): two identical jobs in one showcase host gave 0 of 24
/// equal color frames in every variant, while depth, id and motion repeated 24 of 24 and two fresh hosts
/// repeated the color too: the color path carried state from one job into the next. The host here runs
/// as the showcase binary does by default (the GPU-driven draw path, the film look). The world and the
/// camera are the same at every frame of both jobs, so the AOVs and the color frames must repeat.
/// `REGIA_GPU_UUID` runs it on a real card.
fn two_jobs_repeat_their_color(variant: Variant) {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let mut extra = serde_json::json!({ "aovs": ["depth", "id", "motion"], "aov_format": "raw" });
    if variant.held() {
        extra["time_map"] = serde_json::json!([{ "frame": 0, "scale": 0, "ease": "hold" }]);
    }
    let spec = JobSpec {
        width: 320,
        height: 180,
        frames: 8,
        warmup_frames: 4,
        fps: 24,
        plain: false,
        gpu_driven: true,
        repeat: 2,
        camera: variant
            .path()
            .then_some(([60.0, 46.0, -170.0], [0.0, 36.0, -100.0])),
        camera_end: variant.path().then_some([40.0, 43.0, -150.0]),
        rearm_path: variant.path(),
        extra,
        ..JobSpec::default()
    };
    let out = run(17, &spec);
    assert_eq!(out.manifests.len(), 2);
    let (first, second) = (&out.manifests[0], &out.manifests[1]);
    let frames = |m: &serde_json::Value| m["frame_hashes"].as_array().cloned().unwrap_or_default();
    let (a, b) = (frames(first), frames(second));
    assert_eq!(a.len(), 8, "{variant:?}");
    let equal = (0..a.len()).filter(|&i| a[i] == b[i]).count();
    let aov_equal = first["aov"]["frame_hashes"] == second["aov"]["frame_hashes"];
    println!(
        "{variant:?}: color frames equal {equal} of {}, AOVs equal {aov_equal}",
        a.len()
    );
    for aov in ["depth", "id", "motion"] {
        let hashes = |m: &serde_json::Value| {
            m["aov"]["frame_hashes"][aov]
                .as_array()
                .cloned()
                .unwrap_or_default()
        };
        let (x, y) = (hashes(first), hashes(second));
        let differ: Vec<usize> = (0..x.len().min(y.len())).filter(|&i| x[i] != y[i]).collect();
        println!("{variant:?}: {aov} differs on frames {differ:?}");
    }
    println!(
        "{variant:?}: clocks {} / {}; inputs equal {}",
        first["clock"],
        second["clock"],
        first["aov"]["input_hashes"] == second["aov"]["input_hashes"]
    );
    // With the dolly re-armed the id AOV and the color used to differ while depth and motion repeated:
    // the first job's dolly left the streamed scatter's LODs and cross-fades for the second (ADR-2672).
    assert!(aov_equal, "{variant:?}: the same job repeats its AOVs");
    let differ: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
    assert!(
        differ.is_empty(),
        "{variant:?}: the AOVs repeat but color frames {differ:?} differ: the color path kept state from the first job"
    );
}

/// The calls of Red-Rouge's 2c host that leave live effects behind (their v0.2.13 run: calls 14 and 15
/// of `calls-main-host.jsonl`), placed in front of this test's camera.
fn live_crash_calls() -> Vec<(String, serde_json::Value)> {
    vec![
        (
            "vfx.crash".to_string(),
            serde_json::json!({ "ground": "snow", "impact_in": 0.05, "position": [0.0, 36.0, -100.0],
                                "wind": [3.0, 0.0, 1.0], "yield_t": 250.0 }),
        ),
        (
            "vfx.spawn_crash_fires".to_string(),
            serde_json::json!({ "count": 0, "position": [0.0, 36.0, -100.0], "replace": true, "seed": 2048,
                                "start_frame": 2 }),
        ),
    ]
}

fn live_crash_spec(held: bool) -> JobSpec {
    let mut extra = serde_json::json!({ "aovs": ["depth", "id", "motion"], "aov_format": "raw" });
    if held {
        extra["time_map"] = serde_json::json!([{ "frame": 0, "scale": 0, "ease": "hold" }]);
    }
    JobSpec {
        width: 320,
        height: 180,
        frames: 8,
        warmup_frames: 4,
        fps: 24,
        plain: false,
        gpu_driven: true,
        repeat: 2,
        extra,
        calls: live_crash_calls(),
        after: vec![("vfx.fire_lights".to_string(), serde_json::json!({}))],
        ..JobSpec::default()
    }
}

/// Red-Rouge's v0.2.13 failure, reproduced on that binary on an RTX 5090 (ADR-2633): a crash and its
/// fires left running in the host, then two identical held jobs: 0 of 24 color frames equal, while depth,
/// id and motion repeat (the AOVs draw no particles). A held job now leaves the live effects where it found
/// them, so the second job plays the same frames. The particles still move inside each job.
#[test]
fn two_held_jobs_repeat_their_color_with_a_live_crash() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let out = run(17, &live_crash_spec(true));
    let (first, second) = (&out.manifests[0], &out.manifests[1]);
    let frames = |m: &serde_json::Value| m["frame_hashes"].as_array().cloned().unwrap_or_default();
    let (a, b) = (frames(first), frames(second));
    assert_eq!(a.len(), 8);
    assert!(
        a.iter().any(|h| *h != a[0]),
        "the live effect moves inside a held job: {a:?}"
    );
    let differ: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
    assert!(
        differ.is_empty(),
        "color frames {differ:?} of the second held job differ; clocks {} / {}",
        first["clock"],
        second["clock"]
    );
    assert_eq!(first["clock"]["live_effects"], "restored");
    assert_eq!(first["aov"]["frame_hashes"], second["aov"]["frame_hashes"]);
    let listed = &out.reports[0];
    assert_eq!(
        listed["count"], out.before[1]["total"],
        "the fires the crash lit during the job went back with it: {listed}"
    );
}

/// The other cause on v0.2.13 (RTX 5090, ADR-2633): a weather transition under way. It runs on the render
/// clock too, so without the fix a held job moved it on for the next: 0 of 24 color frames equal, the AOVs
/// 24 of 24.
#[test]
fn two_held_jobs_repeat_their_color_during_a_weather_transition() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let spec = JobSpec {
        calls: vec![(
            "render.set_weather".to_string(),
            serde_json::json!({ "preset": "overcast", "transition_s": 30 }),
        )],
        after: Vec::new(),
        ..live_crash_spec(true)
    };
    let out = run(17, &spec);
    let (first, second) = (&out.manifests[0], &out.manifests[1]);
    assert_eq!(first["clock"]["live_effects"], "restored");
    assert_eq!(first["frame_hashes"], second["frame_hashes"]);
}

/// The control of [`two_held_jobs_repeat_their_color_with_a_live_crash`]: without the hold a job moves the
/// world on, the effect's particles with it, so the crash shows in the frames and the second job's differ.
/// If the crash were not in the picture the test above could not fail.
#[test]
fn a_job_without_a_hold_moves_a_live_crash_on() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase render test: {err}");
        return;
    }
    let out = run(17, &live_crash_spec(false));
    let (first, second) = (&out.manifests[0], &out.manifests[1]);
    assert_eq!(first["clock"]["live_effects"], "moved_on");
    assert_ne!(
        first["frame_hashes"], second["frame_hashes"],
        "the second job continues the effect"
    );
    assert!(
        out.reports[0]["count"].as_u64().unwrap_or(0) > out.before[1]["total"].as_u64().unwrap_or(u64::MAX),
        "the detonation lit fires and they stay: {}",
        out.reports[0]
    );
}

#[test]
fn two_jobs_repeat_their_color_plain() {
    two_jobs_repeat_their_color(Variant::Plain);
}

#[test]
fn two_jobs_repeat_their_color_path_armed() {
    two_jobs_repeat_their_color(Variant::PathArmed);
}

#[test]
fn two_jobs_repeat_their_color_held() {
    two_jobs_repeat_their_color(Variant::Held);
}

#[test]
fn two_jobs_repeat_their_color_held_path_armed() {
    two_jobs_repeat_their_color(Variant::HeldPathArmed);
}
