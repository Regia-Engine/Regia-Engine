// SPDX-License-Identifier: MPL-2.0
//! Event tests: each event is produced by driving [`frame_step`] with injected samples under a
//! capturing subscriber (thread-local, so tests run in parallel).

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

use super::*;
use regia_render::host_checklist::FrameProblem;

#[derive(Clone, Debug)]
struct Captured {
    target: String,
    level: tracing::Level,
    data: Value,
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Captured>>>);

struct DataVisitor(Option<String>);

impl Visit for DataVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "data" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let target = event.metadata().target();
        if !target.starts_with("regia::") {
            return;
        }
        let mut v = DataVisitor(None);
        event.record(&mut v);
        let data =
            v.0.and_then(|d| serde_json::from_str(&d).ok())
                .unwrap_or(Value::Null);
        self.0.lock().expect("lock").push(Captured {
            target: target.to_owned(),
            level: *event.metadata().level(),
            data,
        });
    }
}

impl Capture {
    fn of(&self, target: &str) -> Vec<Captured> {
        self.0
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|c| c.target == target)
            .cloned()
            .collect()
    }
}

/// The note ring and counters are process-wide: tests that drive frames run one at a time.
pub(super) static SERIAL: Mutex<()> = Mutex::new(());

fn with_capture(f: impl FnOnce()) -> Capture {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    tracing::subscriber::with_default(subscriber, f);
    capture
}

fn sample(t_s: f64, ms: f32) -> FrameSample {
    FrameSample {
        t_s,
        frame_ms: ms,
        cpu_ms: ms * 0.4,
        gpu_ms: ms * 0.7,
        ..FrameSample::default()
    }
}

fn opts() -> TelemetryOptions {
    TelemetryOptions {
        summary_interval: Duration::from_secs(10),
        gpu_error_panics: false,
        ..TelemetryOptions::default()
    }
}

/// Runs `frames` frames of `ms` each starting at `*now`.
fn run(t: &mut Telemetry, r: Option<&RuntimeRenderer>, now: &mut f64, frames: usize, ms: f32) {
    for _ in 0..frames {
        *now += f64::from(ms) / 1000.0;
        frame_step(
            t,
            Some(sample(*now, ms)),
            r.map(|x| &x.0),
            None,
            *now,
            FrameInputs::default(),
        );
    }
}

#[test]
fn frame_summary_every_interval_and_on_request() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 700, 16.0); // 11.2 s
        run(&mut t, Some(&renderer), &mut now, 10, 33.0);
        t.request_summary("level_end");
        run(&mut t, Some(&renderer), &mut now, 1, 16.0);
    });
    let summaries = cap.of(target::FRAME_SUMMARY);
    assert_eq!(summaries.len(), 2, "{summaries:?}");
    let first = &summaries[0].data;
    assert_eq!(first["reason"], "periodic");
    assert_eq!(
        first["frames"].as_u64(),
        Some(625),
        "the 10 s window closes on frame 625"
    );
    let p50 = first["frame_ms"]["p50"].as_f64().unwrap_or(0.0);
    assert!((p50 - 16.0).abs() <= 0.13, "p50 {p50}");
    assert!(first["fps_histogram"]["ge_60"].as_u64().is_some_and(|n| n >= 600));
    assert_eq!(first["bound"], "gpu");
    assert!(
        first["cpu_ms"]["mean"]
            .as_f64()
            .is_some_and(|v| (v - 6.4).abs() < 0.1)
    );
    for key in [
        "draws",
        "textures",
        "memory",
        "gpu_pass_ms",
        "surface_errors",
        "created_after_warmup",
        "texture_views",
        "shadow_cpu_ms",
    ] {
        assert!(first.get(key).is_some(), "summary lacks {key}");
    }
    // ADR-385: the frame's reusable lists; nothing drew, so nothing grew.
    let scratch = &first["frame_scratch"];
    assert_eq!(scratch["growths"].as_u64(), Some(0), "{scratch}");
    assert_eq!(scratch["growths_after_warmup"].as_u64(), Some(0), "{scratch}");
    assert_eq!(scratch["total"].as_u64(), Some(0), "{scratch}");
    // The shadow CPU row: every part present; a renderer that drew nothing spent nothing.
    let shadow = &first["shadow_cpu_ms"];
    assert_eq!(shadow["total"]["mean"].as_f64(), Some(0.0), "{shadow}");
    for part in [
        "plan",
        "terrain_prepare",
        "caster_lists",
        "instance_lists",
        "vis_cull",
        "translucent",
    ] {
        assert!(
            shadow[part].as_f64().is_some(),
            "shadow_cpu_ms lacks {part}: {shadow}"
        );
    }
    for kind in ["cascade0", "cascade1", "cascade2", "cascade3", "atlas"] {
        assert!(
            shadow["encode"][kind].as_f64().is_some(),
            "shadow_cpu_ms.encode lacks {kind}"
        );
    }
    let second = &summaries[1].data;
    assert_eq!(second["reason"], "level_end");
    assert_eq!(second["frame_ms"]["max"].as_f64(), Some(33.0));
    assert_eq!(t.counts().summaries, 2);
    assert_eq!(t.last_summary_json().map(|s| s.contains("level_end")), Some(true));
    assert!(last_summary_json().is_some());
}

/// v0.1.18 (ADR-225): the summary carries the emissive fit cache's window counters.
#[test]
fn frame_summary_reports_the_emissive_fit_cache() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 700, 16.0);
    });
    let summaries = cap.of(target::FRAME_SUMMARY);
    let emissive = &summaries[0].data["emissive"];
    for key in [
        "fits_done",
        "fits_deferred",
        "cache_hits",
        "probe_hits",
        "probes_traced",
        "budget_holds",
        "stand_ins",
        "pending",
    ] {
        assert_eq!(emissive[key].as_u64(), Some(0), "emissive.{key}: {emissive}");
    }
    for key in ["frame_ms", "frame_ms_max", "worker_ms"] {
        assert_eq!(emissive[key].as_f64(), Some(0.0), "emissive.{key}: {emissive}");
    }
    assert_eq!(emissive["background"].as_bool(), Some(false), "{emissive}");
}

#[test]
fn hitch_over_twice_the_median_waits_for_the_gpu_lag() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 100, 16.0);
        run(&mut t, Some(&renderer), &mut now, 1, 40.0); // 2.5x the median, under 50 ms
        run(&mut t, Some(&renderer), &mut now, 1, 16.0);
        assert!(t.pending_hitch.is_some(), "waits for its GPU timings");
        run(&mut t, Some(&renderer), &mut now, 1, 16.0);
        run(&mut t, Some(&renderer), &mut now, 50, 16.0);
        run(&mut t, Some(&renderer), &mut now, 1, 60.0); // over 50 ms
        run(&mut t, Some(&renderer), &mut now, 3, 16.0);
        run(&mut t, Some(&renderer), &mut now, 1, 20.0); // 1.25x: not a hitch
        run(&mut t, Some(&renderer), &mut now, 3, 16.0);
    });
    let hitches = cap.of(target::HITCH);
    assert_eq!(hitches.len(), 2, "{hitches:?}");
    let h = &hitches[0].data;
    assert_eq!(h["frame"], 101);
    assert_eq!(h["frame_ms"], 40.0);
    assert_eq!(
        h["median_ms"].as_f64().map(|m| (m - 16.0).abs() < 0.01),
        Some(true)
    );
    assert!(
        h["gpu_ms"].is_null(),
        "the default renderer has never published timings"
    );
    assert!(h.get("top_gpu_passes").is_none());
    assert!(h["created"].get("labels").is_some());
    // The load budget's counters (ADR-7884): zero with the default budget.
    for key in ["deferred_meshes", "deferred_bytes", "deferred_draws"] {
        assert_eq!(h["created"][key].as_u64(), Some(0), "created.{key}: {h}");
    }
    assert!(h["created"]["async_pipelines_pending"].is_u64(), "{h}");
    assert_eq!(hitches[0].level, tracing::Level::INFO);
    assert_eq!(t.counts().hitches, 2);
}

#[test]
fn hitches_are_rate_limited_and_normal_level_keeps_severe_only() {
    let mut t = Telemetry::new(TelemetryOptions {
        hitch_burst: 2,
        hitch_refill: Duration::from_secs(60),
        ..opts()
    });
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 60, 16.0);
        for _ in 0..6 {
            run(&mut t, None, &mut now, 1, 45.0);
            run(&mut t, None, &mut now, 30, 16.0);
        }
    });
    assert_eq!(cap.of(target::HITCH).len(), 2);
    assert_eq!(t.counts().hitches, 6);

    let mut t = Telemetry::new(TelemetryOptions {
        level: TelemetryLevel::Normal,
        ..opts()
    });
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 60, 16.0);
        run(&mut t, None, &mut now, 1, 45.0);
        run(&mut t, None, &mut now, 5, 16.0);
        run(&mut t, None, &mut now, 1, 300.0);
        run(&mut t, None, &mut now, 5, 16.0);
    });
    let hitches = cap.of(target::HITCH);
    assert_eq!(hitches.len(), 1, "normal keeps only the severe hitch");
    assert_eq!(hitches[0].level, tracing::Level::WARN);
    assert_eq!(
        t.counts().hitches,
        2,
        "the 45 ms frame and the severe frame both count"
    );
    assert_eq!(
        t.hitches_suppressed_window, 1,
        "the 45 ms frame was held back by the level"
    );
    assert_eq!(t.agg.hitches_suppressed, 1);
}

/// DO27 10-17 (ask 3): a steady 15 fps logged a hitch every 2 s (every frame was over 50 ms and
/// the rate limit set the pace). A steady slow frame rate is not a hitch.
#[test]
fn steady_slow_frames_are_not_hitches() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| run(&mut t, None, &mut now, 600, 66.0)); // 40 s
    assert_eq!(cap.of(target::HITCH).len(), 0);
    assert_eq!(t.counts().hitches, 0);
    let summaries = cap.of(target::FRAME_SUMMARY);
    assert!(!summaries.is_empty());
    for s in &summaries {
        assert_eq!(s.data["hitches"], 0, "{}", s.data);
        assert_eq!(s.data["hitches_suppressed"], 0);
    }
}

#[test]
fn one_spike_among_fast_frames_is_a_hitch() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 100, 16.0);
        run(&mut t, None, &mut now, 1, 117.0);
        run(&mut t, None, &mut now, 5, 16.0);
    });
    let hitches = cap.of(target::HITCH);
    assert_eq!(hitches.len(), 1, "{hitches:?}");
    let h = &hitches[0].data;
    assert_eq!(h["frame_ms"], 117.0);
    // v0.1.14 fields: bound class, render scale (none drawn here), the GPU waits.
    assert_eq!(h["bound"], "cpu_bound", "{h}");
    assert_eq!(h["render_scale_permille"], Value::Null);
    assert!(h["wait_ms"]["acquire"].is_number() && h["wait_ms"]["readback"].is_number());
}

#[test]
fn a_spike_out_of_a_slow_steady_rate_is_a_hitch() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 100, 16.0);
        // The step down counts until the rolling median catches up (half its 63 frames)…
        run(&mut t, None, &mut now, 200, 66.0);
        let settled = t.counts().hitches;
        // Median (index 31 of 63) is still 16 ms for the first 32 frames of 66 ms.
        assert_eq!(settled, 32, "step down");
        // …then the steady rate is quiet,
        run(&mut t, None, &mut now, 200, 66.0);
        assert_eq!(t.counts().hitches, settled, "steady 66 ms");
        // a frame well above it is a hitch (2 ×, or more than 50 ms above the median),
        run(&mut t, None, &mut now, 1, 140.0);
        run(&mut t, None, &mut now, 10, 66.0);
        assert_eq!(t.counts().hitches, settled + 1, "2.1 × the median");
        run(&mut t, None, &mut now, 1, 120.0);
        run(&mut t, None, &mut now, 10, 66.0);
        assert_eq!(t.counts().hitches, settled + 2, "54 ms above the median");
        // and a frame only a little above it is not.
        run(&mut t, None, &mut now, 1, 100.0);
        run(&mut t, None, &mut now, 10, 66.0);
        assert_eq!(t.counts().hitches, settled + 2, "34 ms above, 1.5 ×");
    });
    // The burst and the rate limit still apply; what they hold back is still counted.
    let events = cap.of(target::HITCH).len() as u64;
    let suppressed: u64 = cap
        .of(target::FRAME_SUMMARY)
        .iter()
        .filter_map(|s| s.data["hitches_suppressed"].as_u64())
        .sum::<u64>()
        + t.hitches_suppressed_window;
    assert_eq!(events + suppressed, t.counts().hitches);
}

#[test]
fn is_hitch_is_relative_to_the_median() {
    let o = TelemetryOptions::default();
    assert!(!is_hitch(66.0, 66.0, &o));
    assert!(!is_hitch(300.0, 300.0, &o), "a steady loading screen");
    assert!(is_hitch(117.0, 16.0, &o));
    assert!(is_hitch(40.0, 16.0, &o), "2.5 ×");
    assert!(!is_hitch(20.0, 16.0, &o));
    assert!(is_hitch(117.0, 66.0, &o), "51 ms above");
    assert!(!is_hitch(115.0, 66.0, &o));
    assert!(!is_hitch(7.0, 3.0, &o), "under hitch_min_ms");
}

#[test]
fn summary_has_the_live_signals() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| run(&mut t, Some(&renderer), &mut now, 700, 16.0));
    let summaries = cap.of(target::FRAME_SUMMARY);
    let s = &summaries[0].data;
    // `sample` gives the GPU 70 % of the frame: the GPU idles, so the frame is CPU-bound.
    assert_eq!(s["frame_bound"]["class"], "cpu_bound", "{s}");
    assert_eq!(s["frame_bound"]["gpu_share"], 0.7);
    assert!(
        s["frame_bound"]["frames"]["cpu_bound"]
            .as_u64()
            .is_some_and(|n| n >= 600)
    );
    // The pre-v0.1.14 field keeps its meaning.
    assert_eq!(s["bound"], "gpu");
    // Nothing drew and no window presented in this process: null, not zero.
    assert_eq!(s["render_scale_permille"], Value::Null);
    for key in [
        "wait_acquire_ms",
        "wait_present_ms",
        "wait_readback_ms",
        "cpu_wait_on_gpu_ms",
    ] {
        assert!(s.get(key).is_some(), "summary lacks {key}");
    }
    assert_eq!(s["live"]["frame_ms"], 16.0);
    assert_eq!(s["live"]["bound"], "cpu_bound");
    let live = t.live();
    assert_eq!(live.frame, 700);
    assert_eq!(live.frame_ms, 16.0);
    assert_eq!(live.bound, FrameBound::CpuBound);
}

#[test]
fn problems_event_on_change_only() {
    let mut t = Telemetry::new(opts());
    let a = [FrameProblem {
        code: "msaa_skips_terrain",
        message: "MSAA skips the terrain".into(),
    }];
    let cap = with_capture(|| {
        problems_changed(&mut t, &[]);
        problems_changed(&mut t, &a);
        problems_changed(&mut t, &a);
        problems_changed(&mut t, &[]);
    });
    let events = cap.of(target::PROBLEMS);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].data["problems"][0]["code"], "msaa_skips_terrain");
    assert_eq!(events[0].level, tracing::Level::WARN);
    assert_eq!(events[1].data["count"], 0);
}

#[test]
fn problems_event_fires_when_the_text_changes_at_equal_length() {
    let mut t = Telemetry::new(opts());
    let problem = |message: &str| {
        [FrameProblem {
            code: "missing_shader",
            message: message.into(),
        }]
    };
    let cap = with_capture(|| {
        problems_changed(&mut t, &problem("missing shader: material_a"));
        problems_changed(&mut t, &problem("missing shader: material_b"));
    });
    assert_eq!(cap.of(target::PROBLEMS).len(), 2);
}

#[test]
fn top_gpu_passes_can_name_post_and_skip_the_totals() {
    let mut values = [0.0_f32; PASS_COUNT];
    values[PASS_NAMES.iter().position(|n| *n == "post").expect("post")] = 9.0;
    values[PASS_COUNT - 2] = 50.0; // scene
    values[PASS_COUNT - 1] = 60.0; // frame
    values[0] = 1.0;
    let top = top_gpu_passes(&values);
    assert_eq!(top.len(), 2, "{top:?}");
    assert_eq!(top[0]["pass"], "post");
    assert_eq!(top[1]["pass"], "shadows");
}

#[test]
fn eviction_rate_is_per_second_not_per_sample() {
    let mut t = Telemetry::new(opts());
    // 40 evictions over a 100 s gap is well under the storm threshold per second. The eviction
    // counter is process-wide: the mark and the add happen under the capture's serial lock, so
    // they never land inside another test's window (ADR-5517).
    let cap = with_capture(|| {
        t.evict_mark = (0.0, taps::counter(Counter::TextureEvictions));
        taps::add(Counter::TextureEvictions, 40);
        eviction_storm(&mut t, 100.0);
    });
    assert!(cap.of(target::STREAM).is_empty());
}

#[test]
fn stream_events_from_notes_evictions_and_pipelines_after_warmup() {
    let mut t = Telemetry::new(TelemetryOptions {
        warmup_fallback_frames: 10,
        ..opts()
    });
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 20, 16.0);
        taps::note(taps::NoteKind::SlowAssetLoad, "mesh 0123abcd-test", 250.0);
        taps::note(taps::NoteKind::SlowAssetLoad, "mesh 0123abcd-test", 260.0); // repeat: held back
        taps::note(
            taps::NoteKind::PakCorrupt,
            "do27.fpak entry feedbeef-test",
            4096.0,
        );
        run(&mut t, None, &mut now, 1, 16.0);
        taps::add(Counter::TextureEvictions, 100);
        run(&mut t, None, &mut now, 70, 16.0);
        let mut s = sample(now, 16.0);
        s.pipelines = 2;
        now += 0.016;
        frame_step(&mut t, Some(s), None, None, now, FrameInputs::default());
    });
    let events = cap.of(target::STREAM);
    let kinds: Vec<&str> = events.iter().filter_map(|e| e.data["kind"].as_str()).collect();
    assert!(kinds.contains(&"pak_corrupt"), "{kinds:?}");
    assert!(kinds.contains(&"eviction_storm"), "{kinds:?}");
    assert!(kinds.contains(&"pipeline_after_warmup"), "{kinds:?}");
    let slow: Vec<_> = events
        .iter()
        .filter(|e| e.data["label"] == "mesh 0123abcd-test")
        .collect();
    assert_eq!(slow.len(), 1, "the repeat is suppressed");
    assert_eq!(slow[0].data["ms"], 250.0);
    let pak = events.iter().find(|e| e.data["kind"] == "pak_corrupt");
    assert_eq!(pak.map(|e| e.data["offset"].clone()), Some(json!(4096.0)));
    assert!(events.iter().all(|e| e.level == tracing::Level::WARN));
}

#[test]
fn surface_errors_summarised_per_window() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 10, 16.0);
        taps::add(Counter::SurfaceOutdated, 3);
        taps::add(Counter::SurfaceTimeout, 1);
        t.request_summary("test");
        run(&mut t, None, &mut now, 1, 16.0);
    });
    let events = cap.of(target::SURFACE);
    assert_eq!(events.len(), 1);
    assert!(events[0].data["outdated"].as_u64().is_some_and(|n| n >= 3));
    assert!(events[0].data["timeout"].as_u64().is_some_and(|n| n >= 1));
}

#[test]
fn gpu_error_is_logged_with_frame_and_pass_and_repeats_held_back() {
    gpu::FRAME.store(77, Ordering::Relaxed);
    let err = wgpu::Error::Validation {
        source: Box::new(std::io::Error::other("test")),
        description: "Buffer usage MAP_READ | STORAGE is not allowed (telemetry test)".into(),
    };
    let cap = with_capture(|| {
        let _pass = tracing::info_span!("forward_pass").entered();
        gpu::on_error(&err, false);
        gpu::on_error(&err, false);
        let other = wgpu::Error::Validation {
            source: Box::new(std::io::Error::other("test")),
            description: "A different validation failure (telemetry test)".into(),
        };
        gpu::on_error(&other, false);
    });
    let events = cap.of(target::GPU_ERROR);
    assert_eq!(
        events.len(),
        2,
        "the repeat is held back, a new description is not: {events:?}"
    );
    assert_eq!(events[0].data["repeated"], 0);
    assert_eq!(events[1].data["pass"], "forward_pass");
    let events = &events[..1];
    let e = &events[0];
    assert_eq!(e.level, tracing::Level::ERROR);
    assert_eq!(e.data["kind"], "validation");
    assert_eq!(e.data["pass"], "forward_pass");
    assert!(e.data["frame"].as_u64().is_some());
}

#[test]
fn gpu_error_messages_are_bounded_utf8_json_and_keep_attribution() {
    let mut cases = vec![
        ("empty", String::new()),
        ("short", "short GPU diagnostic".to_owned()),
        ("below_cap", "b".repeat(gpu::GPU_ERROR_MESSAGE_BYTES - 1)),
        ("at_cap", "x".repeat(gpu::GPU_ERROR_MESSAGE_BYTES)),
        (
            "over_cap",
            format!(
                "{}OVER_CAP_TAIL_SENTINEL",
                "y".repeat(gpu::GPU_ERROR_MESSAGE_BYTES)
            ),
        ),
        (
            "large",
            format!(
                "{}LARGE_TAIL_SENTINEL",
                "z".repeat(gpu::GPU_ERROR_MESSAGE_BYTES * 4)
            ),
        ),
        (
            "multibyte_boundary",
            format!("{}é-tail-marker", "m".repeat(gpu::GPU_ERROR_MESSAGE_BYTES - 1)),
        ),
        (
            "combining_boundary",
            format!(
                "{}e\u{301}combining-tail-marker",
                "n".repeat(gpu::GPU_ERROR_MESSAGE_BYTES - 1)
            ),
        ),
        (
            "json_escaping",
            // The quote, backslash and control characters sit inside the kept prefix, so they
            // must survive the JSON round trip; the filler pushes the tail past the cap.
            format!(
                "{}\"\\\n\r\t\u{0008}\u{000c}{}-tail-marker",
                "q".repeat(64),
                "p".repeat(gpu::GPU_ERROR_MESSAGE_BYTES)
            ),
        ),
    ];
    let cap = with_capture(|| {
        gpu::FRAME.store(8_731, Ordering::Relaxed);
        for (index, (name, message)) in cases.iter().enumerate() {
            let kind = if index.is_multiple_of(2) {
                "validation"
            } else {
                "internal"
            };
            let error = if kind == "validation" {
                wgpu::Error::Validation {
                    source: Box::new(std::io::Error::other("synthetic source")),
                    description: message.clone(),
                }
            } else {
                wgpu::Error::Internal {
                    source: Box::new(std::io::Error::other("synthetic source")),
                    description: message.clone(),
                }
            };
            let span = match index % 3 {
                0 => tracing::info_span!("gpu_payload_pass_a", case = *name),
                1 => tracing::info_span!("gpu_payload_pass_b", case = *name),
                _ => tracing::info_span!("gpu_payload_pass_c", case = *name),
            };
            let _entered = span.enter();
            gpu::on_error(&error, false);
        }
    });

    let events = cap.of(target::GPU_ERROR);
    assert_eq!(events.len(), cases.len());
    for (index, ((name, original), event)) in cases.drain(..).zip(events).enumerate() {
        let kind = if index.is_multiple_of(2) {
            "validation"
        } else {
            "internal"
        };
        let pass = match index % 3 {
            0 => "gpu_payload_pass_a",
            1 => "gpu_payload_pass_b",
            _ => "gpu_payload_pass_c",
        };
        let expected = utf8_prefix(&original, gpu::GPU_ERROR_MESSAGE_BYTES);
        let encoded = event.data.to_string();
        let parsed: Value = serde_json::from_str(&encoded).expect("captured data is JSON");
        let actual = parsed["message"].as_str().expect("message is a string");
        assert_eq!(actual, expected, "case {name}");
        assert!(actual.len() <= gpu::GPU_ERROR_MESSAGE_BYTES, "case {name}");
        assert!(original.starts_with(actual), "case {name} is a prefix");
        assert_eq!(parsed["kind"], kind, "case {name}");
        assert_eq!(parsed["pass"], pass, "case {name}");
        assert_eq!(parsed["frame"], 8_731, "case {name}");
        assert_eq!(parsed["repeated"], 0, "case {name} has an isolated key");
        assert_eq!(event.level, tracing::Level::ERROR);
        if name == "json_escaping" {
            for c in ['"', '\\', '\n', '\r', '\t', '\u{0008}', '\u{000c}'] {
                assert!(
                    actual.contains(c),
                    "case {name} lost {c:?} in the JSON round trip"
                );
            }
        }
        if original.len() <= gpu::GPU_ERROR_MESSAGE_BYTES {
            assert_eq!(actual, original, "short/in-cap message changed: {name}");
        } else {
            let tail_marker = match name {
                "over_cap" => "OVER_CAP_TAIL_SENTINEL",
                "large" => "LARGE_TAIL_SENTINEL",
                "multibyte_boundary" => "é-tail-marker",
                "combining_boundary" => "combining-tail-marker",
                "json_escaping" => "-tail-marker",
                _ => "",
            };
            assert!(!encoded.contains(tail_marker), "tail leaked in {name}");
        }
    }
}

fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[test]
fn gap_frames_keep_their_created_counts_for_the_window() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 10, 16.0);
        now += 6.0;
        let mut stall = sample(now, 6000.0);
        stall.pipelines = 2;
        stall.bind_groups = 3;
        frame_step(
            &mut t,
            Some(stall),
            Some(&renderer.0),
            None,
            now,
            FrameInputs::default(),
        );
        t.request_summary("test");
        run(&mut t, Some(&renderer), &mut now, 1, 16.0);
    });
    let summaries = cap.of(target::FRAME_SUMMARY);
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    // A gap is not a frame, but its device-call counters are per-sample deltas that exist only
    // on this sample: they land in the window that contained the stall, and the next frame does
    // not count them again.
    let created = &summaries[0].data["created"];
    assert_eq!(
        created["pipelines"], 2,
        "the stall frame's pipeline creations count in its window: {created}"
    );
    assert_eq!(created["bind_groups"], 3);
    assert_eq!(summaries[0].data["gaps"], 1);
    assert_eq!(
        summaries[0].data["frames"], 11,
        "the frame carrying the requested summary counts too"
    );
}

#[test]
fn gaps_are_not_frames() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 40, 16.0);
        run(&mut t, None, &mut now, 1, 30_000.0); // also makes the summary due
    });
    let summaries = cap.of(target::FRAME_SUMMARY);
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].data["gaps"], 1);
    assert_eq!(summaries[0].data["frames"], 40);
    assert!(
        summaries[0].data["frame_ms"]["max"]
            .as_f64()
            .is_some_and(|m| m < 100.0)
    );
}

/// v0.1.0 blocker: the probe thread holds a `wgpu::Device` clone, so dropping [`Telemetry`]
/// must wake and join it, not only flag it: a probe still running when `main` returns drops
/// the last device reference inside the driver's exit teardown (SIGSEGV on NVIDIA).
#[test]
fn dropping_telemetry_joins_the_probe_thread() {
    let mut t = Telemetry::new(TelemetryOptions::default());
    let values = t.probe.clone();
    // An hour between samples: only an explicit wake-up ends the thread within the test.
    t.probe_thread = probe::spawn(values.clone(), None, Duration::from_secs(3600), None);
    assert!(t.probe_thread.is_some(), "probe thread did not start");
    let deadline = Instant::now() + Duration::from_secs(10);
    while values.samples.load(std::sync::atomic::Ordering::Relaxed) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        Arc::strong_count(&values),
        3,
        "test + Telemetry + the probe thread hold the probe state"
    );
    let started = Instant::now();
    drop(t);
    let waited = started.elapsed();
    assert_eq!(
        Arc::strong_count(&values),
        1,
        "the probe thread still owns its state (and device clone) after Telemetry dropped: not joined"
    );
    assert!(
        waited < PROBE_JOIN_WAIT,
        "drop waited {waited:?}: the parked probe was not woken"
    );
}

#[test]
fn stop_probe_without_a_thread_is_a_no_op() {
    let mut t = Telemetry::new(TelemetryOptions::default());
    assert!(t.stop_probe());
    assert!(t.stop_probe());
}

/// The probe signals memory pressure when live allocations pass 85 % of the adapter's VRAM
/// (reserved allocator blocks do not count), never when the VRAM size is unknown or under 2 GiB.
#[test]
fn vram_pressure_threshold_uses_allocated_bytes_and_known_vram() {
    let mib = 1024 * 1024;
    let watch = probe::VramWatch {
        adapter_mib: Some(8192),
    };
    assert!(!probe::check_vram_pressure(6900 * mib, watch));
    assert!(probe::check_vram_pressure(7000 * mib, watch));
    assert!(!probe::check_vram_pressure(
        u64::MAX,
        probe::VramWatch { adapter_mib: None }
    ));
    assert!(!probe::check_vram_pressure(
        u64::MAX,
        probe::VramWatch {
            adapter_mib: Some(1024)
        }
    ));
}

// ---------------------------------------------------------------------------------------------
// Log contract (spec 28): phases, summary.json
// ---------------------------------------------------------------------------------------------

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-tele-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn phase_events_carry_the_phase_and_its_data_and_track_the_map() {
    let cap = with_capture(|| {
        phase::reset_for_tests();
        let epoch = phase::epoch();
        phase::phase("menu_ready", Value::Null);
        phase::phase("scene_load_start", json!({ "name": "bore_twelve" }));
        phase::phase(
            "scene_load_end",
            json!({ "name": "bore_twelve", "ms": 1234.5, "phase": "ignored" }),
        );
        phase::phase("match_start", json!({ "name": "bore_twelve" }));
        let now = phase::now();
        assert_eq!(now.map.as_deref(), Some("bore_twelve"));
        assert_eq!(now.maps, vec!["bore_twelve".to_owned()]);
        assert_eq!(now.phase, "match_start");
        phase::phase("match_end", json!({ "name": "bore_twelve", "result": "win" }));
        assert_eq!(phase::now().map, None, "frames after the match belong to no map");
        phase::phase("odd", json!(42));
        assert!(phase::epoch() > epoch);
        phase::reset_for_tests();
    });
    let events = cap.of(target::PHASE);
    let names: Vec<&str> = events
        .iter()
        .map(|e| e.data["phase"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        names,
        [
            "menu_ready",
            "scene_load_start",
            "scene_load_end",
            "match_start",
            "match_end",
            "odd"
        ]
    );
    assert_eq!(events[1].data["name"], "bore_twelve");
    assert_eq!(events[2].data["ms"], 1234.5);
    assert_eq!(
        events[2].data["phase"], "scene_load_end",
        "the host cannot override the phase key"
    );
    assert_eq!(events[4].data["result"], "win");
    assert_eq!(events[5].data["value"], 42, "non-object data goes under `value`");
}

#[test]
fn boot_done_and_shaders_ready_are_emitted_once_with_their_numbers() {
    let mut t = Telemetry::new(opts());
    t.header_done = true;
    t.backend = "Vulkan".into();
    let timeline = |marks: &[(&str, f64)]| -> Vec<(String, f64)> {
        marks.iter().map(|(n, ms)| ((*n).to_owned(), *ms)).collect()
    };
    let cap = with_capture(|| {
        engine_phases::from_marks(&mut t, &timeline(&[("plugin_build", 10.0)]));
        engine_phases::from_marks(
            &mut t,
            &timeline(&[("plugin_build", 10.0), ("first_present", 420.0)]),
        );
        let all = timeline(&[
            ("first_present", 420.0),
            ("boot_tier_ready", 500.0),
            ("menu_tier_ready", 900.0),
        ]);
        engine_phases::from_marks(&mut t, &all);
        engine_phases::from_marks(&mut t, &all);
    });
    let events = cap.of(target::PHASE);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].data["phase"], "boot_done");
    assert_eq!(events[0].data["ms"], 420.0);
    let ready = &events[1].data;
    assert_eq!(ready["phase"], "shaders_ready");
    assert_eq!(ready["ms"], 480.0);
    assert_eq!(ready["boot_tier_ms"], 80.0);
    assert_eq!(ready["backend"], "Vulkan");
    for key in ["pipelines", "ms_each", "cache", "threads"] {
        assert!(ready.get(key).is_some(), "shaders_ready lacks {key}: {ready}");
    }
    assert!(t.agg.shaders.is_some());
}

/// The summary keeps the `shaders_ready` warm-time pipeline count as `ms_each`'s denominator; a
/// process that keeps creating pipelines after the warm must not silently rebase it.
#[test]
fn summary_keeps_the_warm_time_pipeline_count_as_the_ms_each_denominator() {
    let mut t = Telemetry::new(opts());
    t.agg.shaders = Some(ShadersInfo {
        pipelines: Some(146),
        ms: Some(9100.0),
        ms_each: Some(62.3),
        ..ShadersInfo::default()
    });
    let s = t.build_summary(false);
    let shaders = s.shaders.expect("the shaders section");
    assert_eq!(
        shaders.pipelines,
        Some(146),
        "the warm-time count stays next to ms/ms_each"
    );
    assert_eq!(shaders.ms_each, Some(62.3));
    assert_eq!(
        shaders.pipelines_total,
        Some(regia_render::pipeline_cache::pipelines_created()),
        "the lifetime total is its own field"
    );
}

#[test]
fn the_summary_has_every_section_and_splits_frames_by_map() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let _ = with_capture(|| {
        phase::reset_for_tests();
        run(&mut t, Some(&renderer), &mut now, 200, 16.0);
        phase::phase("scene_load_start", json!({ "name": "switchbacks" }));
        run(&mut t, Some(&renderer), &mut now, 100, 16.0);
        run(&mut t, Some(&renderer), &mut now, 5, 60.0);
        run(&mut t, Some(&renderer), &mut now, 1, 2500.0); // a stall
        phase::phase("match_end", Value::Null);
        run(&mut t, Some(&renderer), &mut now, 50, 16.0);
    });
    let mut host = TelemetryHost::default();
    host.set("app", "do27");
    host.set_value("network", json!({ "role": "host", "peers": 2 }));
    t.host_section = Some(host.summary_section());
    let s = t.build_summary(false);
    phase::reset_for_tests();
    assert_eq!(s.schema, SUMMARY_SCHEMA);
    assert_eq!(s.partial, None);
    assert_eq!(
        s.host.as_ref().map(|h| h["network"]["peers"].clone()),
        Some(json!(2))
    );
    assert_eq!(s.host.as_ref().map(|h| h["app"].clone()), Some(json!("do27")));
    assert_eq!(s.session.as_ref().and_then(|x| x.frames), Some(356));
    assert_eq!(
        s.session.as_ref().and_then(|x| x.maps.clone()),
        Some(vec!["switchbacks".to_owned()])
    );
    let frames = s.frames.as_ref().expect("frames");
    assert_eq!(frames.bound.as_deref(), Some("gpu"));
    // Rank ceil(0.99 * 356) = 353 falls in the 60 ms bin (upper edge 60.125).
    assert!(
        frames.p99_ms.is_some_and(|v| (v - 60.125).abs() < 0.01),
        "p99 {:?}",
        frames.p99_ms
    );
    let map = &s.frames_by_map.as_ref().expect("per map")["switchbacks"];
    assert_eq!(map.frames, Some(106), "the map's own frames: 100 + 5 + the stall");
    assert!(map.over_50ms_pct.is_some_and(|p| p > 5.0));
    let stalls = s.stalls.as_ref().expect("stalls");
    assert_eq!(stalls.len(), 1);
    assert_eq!(stalls[0].ms, Some(2500.0));
    assert_eq!(stalls[0].phase.as_deref(), Some("scene_load_start"));
    assert!(
        s.hitches
            .as_ref()
            .is_some_and(|h| h.count.is_some_and(|c| c >= 5))
    );
    for (name, present) in [
        ("memory", s.memory.is_some()),
        ("shaders", s.shaders.is_some()),
        ("audio", s.audio.is_some()),
        ("errors", s.errors.is_some()),
    ] {
        assert!(present, "summary lacks {name}");
    }
    assert!(
        s.shaders
            .as_ref()
            .and_then(|x| x.driver_cache.as_deref())
            .is_some()
    );
    // Serialises and parses back.
    let back: SessionSummary = serde_json::from_str(&s.to_json()).expect("round trip");
    assert_eq!(back, s);
}

#[test]
fn partial_summaries_are_written_off_thread_every_interval() {
    let dir = temp_dir("partials");
    // An earlier session hung: only its partial is there.
    std::fs::write(
        dir.join("hung.summary.partial.json"),
        r#"{"schema":1,"partial":true,"session":{"seconds":99.0}}"#,
    )
    .expect("write");
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(TelemetryOptions {
        summary_path: Some(dir.join("run.summary.json")),
        ..opts()
    });
    let mut now = 0.0;
    let cap = with_capture(|| {
        t.header_done = true;
        start_summary_sink(&mut t);
        assert_eq!(
            t.summary_paths().map(|p| p.partial_path.clone()),
            Some(dir.join("run.summary.partial.json"))
        );
        run(&mut t, Some(&renderer), &mut now, 6 * 62, 16.0); // 5.9 s: past the first partial
        let deadline = Instant::now() + Duration::from_secs(5);
        while t.partials_written() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    assert!(t.partials_written() >= 1);
    let text = std::fs::read_to_string(dir.join("run.summary.partial.json")).expect("partial exists");
    let partial: SessionSummary = serde_json::from_str(&text).expect("parses");
    assert_eq!(partial.partial, Some(true));
    assert!(partial.frames.is_some());
    // The hung session is noticed, in the log and in the next summary.
    let deadline = Instant::now() + Duration::from_secs(5);
    while t.build_summary(false).previous_exit.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let prev = t.build_summary(false).previous_exit.expect("previous exit");
    assert_eq!(prev.clean, Some(false));
    assert_eq!(
        prev.last_partial
            .as_ref()
            .and_then(|p| p["session"]["seconds"].as_f64()),
        Some(99.0)
    );
    let _ = cap;
    drop(t);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_span_layer_warns_once() {
    let profiler = FrameProfiler::new();
    for _ in 0..70 {
        profiler.end_frame();
    }
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        // The check retries until frame 600, in case the first frames were idle.
        for _ in 0..700 {
            now += 0.016;
            frame_step(
                &mut t,
                Some(sample(now, 16.0)),
                None,
                Some(&profiler),
                now,
                FrameInputs::default(),
            );
        }
    });
    let warnings: Vec<_> = cap
        .of(target::TELEMETRY)
        .into_iter()
        .filter(|c| c.level == tracing::Level::WARN)
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

/// Review holes-life #11: the summary's skipped-draw window total grows by the renderer's
/// running total, so samples taken without a new main frame (the last frame's count repeated)
/// add nothing, and a rebuilt renderer counts from zero.
#[test]
fn skipped_draws_count_each_skip_once() {
    let mut seen = 0;
    assert_eq!(skipped_since(3, &mut seen), 3);
    // The same frame sampled again: the per-frame count is still 3, the total did not move.
    assert_eq!(skipped_since(3, &mut seen), 0);
    assert_eq!(skipped_since(3, &mut seen), 0);
    assert_eq!(skipped_since(5, &mut seen), 2);
    // A renderer rebuilt (shader hot swap, device restart) starts its total again.
    assert_eq!(skipped_since(1, &mut seen), 1);
    assert_eq!(seen, 1);
}

#[test]
fn the_panic_snapshot_exists_from_the_session_header() {
    let dir = temp_dir("panic_seed");
    let mut t = Telemetry::new(TelemetryOptions {
        summary_path: Some(dir.join("run.summary.json")),
        ..opts()
    });
    assert!(panic_seed(&t).is_none(), "no sink yet");
    start_summary_sink(&mut t);
    let (paths, record) = panic_seed(&t).expect("seeded before the first partial");
    assert_eq!(paths.final_path, dir.join("run.summary.json"));
    assert_eq!(record.partial, Some(true));
    drop(t);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn errors_after_the_summary_was_built_reach_the_clean_exit_file() {
    let mut record = SessionSummary {
        errors: Some(summary::ErrorsInfo {
            device_lost: Some(0),
            ..summary::ErrorsInfo::default()
        }),
        ..SessionSummary::default()
    };
    regia_core::telemetry::add(Counter::DeviceLost, 1);
    engine_phases::refresh_error_counts(&mut record);
    let lost = record.errors.as_ref().and_then(|e| e.device_lost).unwrap_or(0);
    assert!(lost >= 1, "{lost}");
}

#[test]
fn a_non_finite_stall_is_not_listed() {
    let mut agg = summary::SummaryAgg::new();
    agg.stall(1.0, f32::INFINITY);
    agg.stall(f64::NAN, 500.0);
    assert!(agg.stalls.is_empty());
    agg.stall(2.0, 500.0);
    assert_eq!(agg.stalls.len(), 1);
}

#[test]
fn first_window_starts_at_the_first_frame_not_at_zero() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 12.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 700, 16.0);
    });
    let summaries = cap.of(target::FRAME_SUMMARY);
    let first = &summaries[0].data;
    let window_s = first["window_s"].as_f64().unwrap_or(0.0);
    assert!((10.0..11.0).contains(&window_s), "window_s {window_s}");
}

#[test]
fn session_clock_follows_the_unclamped_frame_interval() {
    let mut real = RealTime::default();
    let mut wall = 0.0;
    real.advance_by(Duration::from_millis(16));
    assert!((advance_wall(&mut wall, Some(&real)) - 0.016).abs() < 1e-9);
    real.advance_by(Duration::from_secs(4));
    let now = advance_wall(&mut wall, Some(&real));
    assert!(
        (now - 4.016).abs() < 1e-9,
        "a 4 s stall advances the clock by 4 s, got {now}"
    );
    assert_eq!(advance_wall(&mut wall, None), now);
}

#[test]
fn eviction_storm_uses_the_per_second_rate_of_a_long_window() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        // 63 evictions over a 2 s window is 31.5/s: below the default 32/s threshold.
        t.evict_mark = (0.0, taps::counter(Counter::TextureEvictions));
        taps::add(Counter::TextureEvictions, 63);
        run(&mut t, None, &mut now, 1, 2000.0);
    });
    let kinds: Vec<String> = cap
        .of(target::STREAM)
        .iter()
        .filter_map(|e| e.data["kind"].as_str().map(str::to_owned))
        .collect();
    assert!(!kinds.iter().any(|k| k == "eviction_storm"), "{kinds:?}");
}

#[test]
fn memory_subsystems_lists_heap_and_gpu_bytes_per_subsystem() {
    use regia_core::memory::{MemorySnapshot, Subsystem, SubsystemMemory};
    // Untracked heap, no GPU sample yet: every subsystem listed, every number null.
    let v = memory_subsystems_value(&MemorySnapshot::default(), None);
    assert_eq!(v["heap_tracked"], false);
    assert!(v["heap_live_bytes"].is_null());
    let by = v["by_subsystem"].as_object().expect("object");
    assert_eq!(by.len(), Subsystem::COUNT);
    assert!(by["scene"]["heap_bytes"].is_null());
    assert!(by["render"]["gpu_bytes"].is_null());

    let mut heap = MemorySnapshot {
        tracking: true,
        live_bytes: 1000,
        peak_bytes: 4000,
        ..MemorySnapshot::default()
    };
    heap.subsystems = std::array::from_fn(|i| (Subsystem::ALL[i], SubsystemMemory::default()));
    heap.subsystems[Subsystem::Scene as usize].1.live_bytes = 600;
    let mut gpu = [(Subsystem::Unscoped, 0u64); Subsystem::COUNT];
    for (i, slot) in gpu.iter_mut().enumerate() {
        *slot = (Subsystem::ALL[i], 0);
    }
    gpu[Subsystem::Terrain as usize].1 = 1 << 20;
    let v = memory_subsystems_value(&heap, Some(&gpu));
    assert_eq!(v["heap_tracked"], true);
    assert_eq!(v["heap_live_bytes"], 1000);
    assert_eq!(v["heap_peak_bytes"], 4000);
    assert_eq!(v["by_subsystem"]["scene"]["heap_bytes"], 600);
    assert_eq!(v["by_subsystem"]["terrain"]["gpu_bytes"], 1 << 20);
    assert_eq!(v["by_subsystem"]["terrain"]["heap_bytes"], 0);
}

#[test]
fn a_hitch_flushed_early_by_a_second_hitch_carries_no_gpu_timings() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 100, 16.0);
        run(&mut t, Some(&renderer), &mut now, 1, 80.0);
        run(&mut t, Some(&renderer), &mut now, 1, 80.0); // flushes the first, one frame early
        run(&mut t, Some(&renderer), &mut now, 4, 16.0);
    });
    let hitches = cap.of(target::HITCH);
    assert_eq!(hitches.len(), 2, "{hitches:?}");
    assert!(hitches[0].data.get("gpu_ms").is_none(), "{:?}", hitches[0].data);
    assert!(hitches[0].data.get("top_gpu_passes").is_none());
    assert!(
        hitches[1].data["gpu_ms"].is_null(),
        "an unpublished renderer has no GPU timing even after the lag"
    );
}

#[test]
fn a_map_past_the_cap_does_not_become_current() {
    let cap = with_capture(|| {
        phase::reset_for_tests();
        for i in 0..40 {
            phase::phase("match_start", json!({ "name": format!("map{i}") }));
        }
        let now = phase::now();
        assert_eq!(now.maps.len(), 32);
        assert_eq!(
            now.map, None,
            "the 40th map cannot be listed, so it is not current"
        );
        phase::phase("match_start", json!({ "name": "map3" }));
        assert_eq!(
            phase::now().map.as_deref(),
            Some("map3"),
            "a listed map still works"
        );
        phase::reset_for_tests();
    });
    drop(cap);
}

#[test]
fn a_nan_window_turns_the_recorder_window_off() {
    let rec = trace::Recorder::new(trace::TraceConfig::default());
    rec.set_window_seconds(5.0);
    assert_eq!(rec.state(), trace::RecorderState::Windowed);
    rec.set_window_seconds(f32::NAN);
    assert_eq!(rec.state(), trace::RecorderState::Idle);
}

#[test]
fn negative_frames_stay_out_of_the_median() {
    let mut t = Telemetry::new(opts());
    for _ in 0..40 {
        t.push_median(16.0);
    }
    // detect_hitch only pushes finite, non-negative frames.
    let mut now = 0.0;
    let cap = with_capture(|| {
        for _ in 0..3 {
            now += 0.016;
            frame_step(
                &mut t,
                Some(sample(now, -1.0)),
                None,
                None,
                now,
                FrameInputs::default(),
            );
        }
    });
    drop(cap);
    assert_eq!(t.median_ms(), Some(16.0));
}

/// Spec 28 § 4: the first partial summary is due five seconds after the header, then every 60 s.
#[test]
fn partials_are_due_five_seconds_after_the_header_then_every_minute() {
    let dir = regia_test_utils::unique_temp_dir("tele-partial-cadence");
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(TelemetryOptions {
        summary_path: Some(dir.join("run.summary.json")),
        ..opts()
    });
    assert_eq!(t.next_partial_s, f64::MAX, "no partial is due before the header");
    let mut now = 0.0;
    let cap = with_capture(|| {
        // The header lands one second into the session.
        run(&mut t, Some(&renderer), &mut now, 62, 16.0);
        t.header_done = true;
        start_summary_sink(&mut t);
        let header_s = t.last_now_s;
        assert_eq!(t.next_partial_s, header_s + summary::FIRST_PARTIAL_S);
        // Just short of five seconds after the header: not yet.
        while now + 0.016 < header_s + summary::FIRST_PARTIAL_S {
            run(&mut t, Some(&renderer), &mut now, 1, 16.0);
        }
        assert_eq!(t.next_partial_s, header_s + summary::FIRST_PARTIAL_S);
        // The frame that crosses it sends the first partial and schedules the next a minute on.
        run(&mut t, Some(&renderer), &mut now, 1, 16.0);
        let first_s = now;
        assert_eq!(t.next_partial_s, first_s + summary::PARTIAL_INTERVAL_S);
        while now + 0.016 < first_s + summary::PARTIAL_INTERVAL_S {
            run(&mut t, Some(&renderer), &mut now, 1, 16.0);
        }
        assert_eq!(
            t.next_partial_s,
            first_s + summary::PARTIAL_INTERVAL_S,
            "nothing sent inside the minute"
        );
        run(&mut t, Some(&renderer), &mut now, 1, 16.0);
        assert_eq!(t.next_partial_s, now + summary::PARTIAL_INTERVAL_S);
    });
    drop(cap);
    assert_eq!(summary::FIRST_PARTIAL_S, 5.0);
    assert_eq!(summary::PARTIAL_INTERVAL_S, 60.0);
    drop(t);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 28 § 5 item 2: the session header carries the CPU model, RAM, device-local VRAM and the OS
/// version, each `null` when unknown.
#[test]
fn the_session_header_carries_cpu_ram_vram_and_os_version() {
    let mut world = World::new();
    world.insert_resource(regia_platform::OsInfo {
        os: "linux".into(),
        version: "Arch Linux".into(),
        device_model: String::new(),
        total_ram_bytes: 32 << 30,
        cpu_count: 16,
        cpu_model: "AMD Ryzen 7 7800X3D 8-Core Processor".into(),
    });
    let record = session_record(&mut world, None);
    assert_eq!(record["os_version"], json!("Arch Linux"));
    assert_eq!(record["cpu"], json!("AMD Ryzen 7 7800X3D 8-Core Processor"));
    assert_eq!(record["ram_gb"].as_f64(), Some(32.0));
    // VRAM comes from the device; without one it is unknown, not zero.
    assert!(record["vram_gb"].is_null());
    assert!(record["cpu_threads"].as_u64().is_some_and(|n| n > 0));

    // Unknown facts are `null`, never empty strings or zero.
    let mut bare = World::new();
    bare.insert_resource(regia_platform::OsInfo {
        os: "linux".into(),
        version: String::new(),
        device_model: String::new(),
        total_ram_bytes: 0,
        cpu_count: 1,
        cpu_model: String::new(),
    });
    let record = session_record(&mut bare, None);
    for key in ["os_version", "cpu", "ram_gb", "vram_gb"] {
        assert!(
            record[key].is_null(),
            "{key} is null when unknown: {}",
            record[key]
        );
    }
    let record = session_record(&mut World::new(), None);
    assert!(record["os_version"].is_null() && record["cpu"].is_null());
}

/// Spec 28 § 3: the engine's `scene_load_end` carries `ms`, `mb_uploaded`, `pipelines` and
/// `meshes`; a timed-out or failed load is a warning that says so.
#[test]
fn scene_load_end_carries_the_warm_numbers_and_warns_on_a_failed_load() {
    let report = regia_render::GameWarmReport {
        ms: 1234.567,
        pipelines: 146,
        error: None,
        timed_out: false,
        mb_uploaded: 412.375,
        meshes: 87,
        seq: 1,
        map_id: None,
        complete: None,
    };
    let (level, data) = engine_phases::scene_load_end_data(&report);
    assert_eq!(level, tracing::Level::INFO);
    assert_eq!(data["ms"].as_f64(), Some(1234.57));
    assert_eq!(data["mb_uploaded"].as_f64(), Some(412.38));
    assert_eq!(data["pipelines"], json!(146));
    assert_eq!(data["meshes"], json!(87));
    assert!(data.get("timed_out").is_none() && data.get("error").is_none());

    let timed_out = regia_render::GameWarmReport {
        timed_out: true,
        ..report.clone()
    };
    let (level, data) = engine_phases::scene_load_end_data(&timed_out);
    assert_eq!(level, tracing::Level::WARN);
    assert_eq!(data["timed_out"], json!(true));

    let failed = regia_render::GameWarmReport {
        error: Some("device lost".into()),
        ..report
    };
    let (level, data) = engine_phases::scene_load_end_data(&failed);
    assert_eq!(level, tracing::Level::WARN);
    assert_eq!(data["error"], json!("device lost"));
}

/// Spec 28 § 4: `system` comes from the session header, `display.monitors[]` lists every monitor
/// and marks the one in use (whose refresh is the display's), and `settings.upscaler` is the one
/// the frame actually used.
#[test]
fn the_summary_skeleton_maps_the_header_and_marks_the_monitor_in_use() {
    let record = json!({
        "os": { "family": "windows", "arch": "x86_64" },
        "os_version": "Windows 11 Pro 25H2 (build 26200)",
        "cpu": "AMD Ryzen 7 7800X3D 8-Core Processor",
        "ram_gb": 32.0,
        "vram_gb": 16.0,
        "cpu_threads": 16,
        "adapter": { "name": "NVIDIA GeForce RTX 4080", "vendor": "0x10de", "driver_info": "617.42", "backend": "Vulkan" },
        "monitors": [
            { "name": "left", "width": 1920, "height": 1080, "refresh_hz": 60.0, "scale": 1.0, "current": false },
            { "name": "main", "width": 3440, "height": 1440, "refresh_hz": 144.0, "scale": 1.25, "current": true },
        ],
        "window": { "physical_size": [3440, 1440], "mode": "fullscreen", "present_mode": "Immediate" },
    });
    let s = summary_skeleton(&World::new(), &record, false);
    let system = s.system.as_ref().expect("system");
    assert_eq!(system.os.as_deref(), Some("Windows 11 Pro 25H2 (build 26200)"));
    assert_eq!(
        system.cpu.as_deref(),
        Some("AMD Ryzen 7 7800X3D 8-Core Processor")
    );
    assert_eq!(system.cpu_threads, Some(16));
    assert_eq!(system.ram_gb, Some(32.0));
    assert_eq!(system.vram_gb, Some(16.0));
    assert_eq!(system.gpu.as_deref(), Some("NVIDIA GeForce RTX 4080"));
    assert_eq!(system.vendor.as_deref(), Some("0x10de"));
    assert_eq!(system.driver.as_deref(), Some("617.42"));
    assert_eq!(system.backend.as_deref(), Some("Vulkan"));
    let display = s.display.as_ref().expect("display");
    assert_eq!((display.width, display.height), (Some(3440), Some(1440)));
    assert_eq!(
        display.refresh_hz,
        Some(144.0),
        "the refresh of the monitor in use"
    );
    assert_eq!(display.mode.as_deref(), Some("fullscreen"));
    assert_eq!(display.present.as_deref(), Some("Immediate"));
    let monitors = display.monitors.as_ref().expect("every monitor is listed");
    assert_eq!(monitors.len(), 2);
    let in_use: Vec<_> = monitors.iter().filter(|m| m.current == Some(true)).collect();
    assert_eq!(in_use.len(), 1);
    assert_eq!(in_use[0].name.as_deref(), Some("main"));
    assert_eq!(in_use[0].scale, Some(1.25));
    assert_eq!(monitors[0].current, Some(false));

    // An OS without a version string falls back to family and arch.
    let bare = summary_skeleton(
        &World::new(),
        &json!({ "os": { "family": "linux", "arch": "aarch64" } }),
        false,
    );
    assert_eq!(
        bare.system.as_ref().and_then(|x| x.os.as_deref()),
        Some("linux aarch64")
    );
    assert!(bare.display.as_ref().is_some_and(|d| d.monitors.is_none()));

    // The upscaler is the one the frame used, added at build time.
    let mut t = Telemetry::new(opts());
    t.static_info = Some(s);
    t.upscaler = Some("TAAU");
    let built = t.build_summary(false);
    assert_eq!(
        built.settings.as_ref().and_then(|x| x.upscaler.as_deref()),
        Some("TAAU")
    );
}

/// ADR-7883: `scene_load_end` names the host's map and says whether the warm covered the scene
/// (`complete`); `scene_ready` carries the prepare's first present and is a warning when the
/// prepare timed out.
#[test]
fn scene_load_end_has_the_map_and_complete_and_scene_ready_the_first_present() {
    let report = regia_render::GameWarmReport {
        ms: 10.0,
        pipelines: 4,
        seq: 1,
        map_id: Some("switchbacks".into()),
        complete: Some(false),
        ..regia_render::GameWarmReport::default()
    };
    let (_, data) = engine_phases::scene_load_end_data(&report);
    assert_eq!(data["map"], json!("switchbacks"));
    assert_eq!(data["complete"], json!(false));

    let status = regia_render::SceneReadyStatus {
        state: regia_render::SceneReadyState::Presented,
        map_id: Some("switchbacks".into()),
        prepare: 1,
        uploaded_mb: 340.25,
        meshes: 183,
        pipelines: 85,
        warms: 2,
        ms: 41000.0,
        complete: Some(true),
        first_present_frame: Some(912),
        first_present_ms: Some(41234.567),
        presented_to_window: true,
        ..regia_render::SceneReadyStatus::default()
    };
    let (level, data) = engine_phases::scene_ready_data(&status);
    assert_eq!(level, tracing::Level::INFO);
    assert_eq!(data["map"], json!("switchbacks"));
    assert_eq!(data["first_present_ms"].as_f64(), Some(41234.57));
    assert_eq!(data["frame"], json!(912));
    assert_eq!(data["warms"], json!(2));
    assert_eq!(data["complete"], json!(true));
    assert_eq!(data["presented"], json!(true));
    assert!(data.get("timed_out").is_none());
    let late = regia_render::SceneReadyStatus {
        timed_out: true,
        error: Some("abandoned".into()),
        ..status
    };
    let (level, data) = engine_phases::scene_ready_data(&late);
    assert_eq!(level, tracing::Level::WARN);
    assert_eq!(data["timed_out"], json!(true));
}

/// A held frame report (ADR-7890): the composite and the UI took 0.4 ms on the GPU.
fn held_report() -> regia_render::SceneHoldFrame {
    regia_render::SceneHoldFrame {
        step: regia_render::scene_hold::HoldStep::Held,
        held: true,
        scene_views: 0,
        composite_ms: 0.1,
        ui_ms: 0.3,
        gpu_ms: 0.4,
        timed: true,
    }
}

/// Runs `frames` held frames of `ms` each starting at `*now`.
fn run_held(t: &mut Telemetry, now: &mut f64, frames: usize, ms: f32) {
    for _ in 0..frames {
        *now += f64::from(ms) / 1000.0;
        let inputs = FrameInputs {
            held: Some(held_report()),
            ..FrameInputs::default()
        };
        frame_step(t, Some(sample(*now, ms)), None, None, *now, inputs);
    }
}

#[test]
fn held_frames_are_counted_apart_and_feed_no_median_or_bound() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 100, 16.0);
        // A long pause of cheap held frames: they must not pull the median down...
        run_held(&mut t, &mut now, 200, 2.0);
        assert!(t.live().held, "the live frame says held");
        assert_eq!(
            t.live().gpu_ms,
            0.4,
            "a held frame's GPU time is the composite and the UI"
        );
        // ...so the first played frame at the old rate is not a hitch.
        run(&mut t, None, &mut now, 1, 16.0);
        assert!(!t.live().held);
        t.request_summary("test");
        run(&mut t, None, &mut now, 1, 16.0);
    });
    assert_eq!(t.counts().hitches, 0, "{:?}", cap.of(target::HITCH));
    assert_eq!(t.counts().held_frames, 200);
    let summaries = cap.of(target::FRAME_SUMMARY);
    let last = summaries.last().map(|s| &s.data).expect("a summary");
    let held_total: u64 = summaries
        .iter()
        .map(|s| s.data["held"]["frames"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(held_total, 200, "every held frame is in a summary's held count");
    let frames_total: u64 = summaries
        .iter()
        .map(|s| s.data["frames"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(frames_total, 102, "held frames are not frames of the window");
    assert!(
        last["frame_ms"]["p50"].as_f64().is_some_and(|p| p > 10.0),
        "held frames stay out of the frame-time histogram: {last}"
    );
}

#[test]
fn a_held_hitch_says_held_with_no_scene_pass_and_the_held_gpu_time() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 100, 16.0);
        run_held(&mut t, &mut now, 1, 400.0); // severe: emitted at the normal level
        run(&mut t, Some(&renderer), &mut now, 3, 16.0);
    });
    let hitches = cap.of(target::HITCH);
    assert_eq!(hitches.len(), 1, "{hitches:?}");
    let h = &hitches[0].data;
    assert_eq!(h["held"], true);
    assert_eq!(h["scene_passes"], 0);
    assert_eq!(h["gpu_ms"], 0.4);
    let passes: Vec<&str> = h["top_gpu_passes"]
        .as_array()
        .map(|a| a.iter().filter_map(|p| p["pass"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(passes, ["scene hold composite", "ui"]);
}

/// A render pipeline into `format` at `samples`, created through the counted device on this
/// thread (it lands in the recent-creation ring with its target).
fn late_pipeline(device: &regia_render::RenderDevice, format: wgpu::TextureFormat, samples: u32) {
    let d = regia_render::TrackedDevice::observe(&device.device);
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("late target test"),
        source: wgpu::ShaderSource::Wgsl(
            "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> { \
             return vec4<f32>(f32(i), 0.0, 0.0, 1.0); } \
             @fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }"
                .into(),
        ),
    });
    let _pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("late target test"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            targets: &[Some(format.into())],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: samples,
            ..Default::default()
        },
        multiview_mask: None,
        cache: None,
    });
}

/// ADR-7885 (DO27 pipeline-cache-cold): a pipeline created after warm-up is reported with its
/// target format, sample count, thread and creation site, and the window summary's
/// `created_after_warmup.sites` counts every one of them per site, also the frames whose
/// `pipeline_after_warmup` event the rate limit held back (tester A2: 210 created, 4 named).
#[test]
fn late_pipelines_name_their_target_and_count_per_site_when_rate_limited() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let late = 5;
    let cap = with_capture(|| {
        // `Telemetry::new` starts reading the process-wide create ring at its current sequence:
        // build it under the capture's serial lock. Built before the lock, it named the pipelines
        // a serialized GPU test (stall_tests' lab pipelines) created while this test waited for
        // the lock as this run's first late creation ("lab blend" at index 0).
        let mut t = Telemetry::new(TelemetryOptions {
            warmup_fallback_frames: 10,
            ..opts()
        });
        let mut now = 0.0;
        run(&mut t, None, &mut now, 20, 16.0);
        for _ in 0..late {
            late_pipeline(&device, wgpu::TextureFormat::Rgba8UnormSrgb, 4);
            let mut s = sample(now, 16.0);
            s.pipelines = 1;
            now += 0.016;
            frame_step(&mut t, Some(s), None, None, now, FrameInputs::default());
        }
        t.request_summary("level_end");
        run(&mut t, None, &mut now, 1, 16.0);
    });
    let events: Vec<_> = cap
        .of(target::STREAM)
        .into_iter()
        .filter(|e| e.data["kind"] == "pipeline_after_warmup")
        .collect();
    assert!(
        !events.is_empty() && events.len() < late,
        "the rate limit holds back repeats: {} events for {late} frames",
        events.len()
    );
    let create = &events[0].data["creates"][0];
    assert_eq!(create["label"], "late target test", "{}", events[0].data);
    assert_eq!(create["format"], "Rgba8UnormSrgb", "{create}");
    assert_eq!(create["samples"], 4, "{create}");
    assert_eq!(create["thread"], "frame", "{create}");
    assert!(
        create["site"].as_str().is_some_and(|s| s.contains("tests.rs")),
        "the creation site is the calling line: {create}"
    );
    let summary = cap
        .of(target::FRAME_SUMMARY)
        .into_iter()
        .find(|s| s.data["reason"] == "level_end")
        .map(|s| s.data["created_after_warmup"].clone())
        .unwrap_or(Value::Null);
    let site = summary["sites"]
        .as_array()
        .and_then(|sites| sites.iter().find(|s| s["label"] == "late target test"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        site["count"].as_u64(),
        Some(late as u64),
        "every late creation is counted per site, rate-limited or not: {summary}"
    );
    assert_eq!(site["format"], "Rgba8UnormSrgb", "{summary}");
    assert_eq!(site["samples"], 4, "{summary}");
    assert_eq!(site["thread"], "frame", "{summary}");
    // Other test threads may create pipelines meanwhile (the ring is process-wide): at least ours.
    assert!(
        summary["frame_thread"].as_u64().is_some_and(|n| n >= late as u64),
        "{summary}"
    );
}

// ---------------------------------------------------------------------------------------------
// v0.5.1 hitch truth (DO27 tester items telemetry-hitch-floor, focus-guard-fullscreen; ADR-7898)
// ---------------------------------------------------------------------------------------------

/// A window frame's waits, published as publication `seq`.
#[allow(clippy::field_reassign_with_default)] // `FrameWaits` is `#[non_exhaustive]`
fn waits(seq: u64, acquire_ms: f32, present_ms: f32) -> regia_render::FrameWaits {
    let mut w = regia_render::FrameWaits::default();
    w.acquire_ms = acquire_ms;
    w.present_ms = present_ms;
    w.seq = seq;
    w
}

/// Drives one frame of `ms` with this frame's waits and the previous update's phase record.
fn step_with(
    t: &mut Telemetry,
    now: &mut f64,
    ms: f32,
    frame_waits: Option<regia_render::FrameWaits>,
    previous_update: Option<&regia_app::FramePhases>,
    spans: Option<&FrameProfiler>,
) {
    *now += f64::from(ms) / 1000.0;
    let inputs = FrameInputs {
        phases: previous_update,
        waits: frame_waits,
        ..FrameInputs::default()
    };
    frame_step(t, Some(sample(*now, ms)), None, spans, *now, inputs);
}

/// Tester A1's sequence: 7 ms frames, and every tenth a 14.2 ms frame whose excess is the
/// acquire wait (6324 such "hitches" in one session at v0.5.0). With `acquire_ms` 7.1 none is a
/// hitch; with 0.1 (the same frame as a CPU spike) every one still is.
fn acquire_sequence(acquire_ms: f32) -> (Telemetry, Capture) {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let mut seq = 0;
    let cap = with_capture(|| {
        for _ in 0..100 {
            seq += 1;
            step_with(&mut t, &mut now, 7.0, Some(waits(seq, 0.2, 0.1)), None, None);
        }
        for _ in 0..20 {
            seq += 1;
            step_with(
                &mut t,
                &mut now,
                14.2,
                Some(waits(seq, acquire_ms, 0.1)),
                None,
                None,
            );
            for _ in 0..9 {
                seq += 1;
                step_with(&mut t, &mut now, 7.0, Some(waits(seq, 0.2, 0.1)), None, None);
            }
        }
    });
    (t, cap)
}

#[test]
fn a_frame_spent_waiting_on_acquire_is_a_present_wait_not_a_hitch() {
    let (t, cap) = acquire_sequence(7.1);
    assert_eq!(t.counts().hitches, 0, "{:?}", t.counts());
    assert_eq!(t.counts().present_waits, 20);
    assert!(cap.of(target::HITCH).is_empty());
    let (t, cap) = acquire_sequence(0.1);
    assert_eq!(t.counts().hitches, 20, "a CPU spike is still a hitch");
    assert_eq!(t.counts().present_waits, 0);
    let hitches = cap.of(target::HITCH);
    assert!(!hitches.is_empty());
    assert_eq!(hitches[0].data["frame_ms"].as_f64(), Some(14.2));
}

#[test]
fn a_stale_wait_publication_does_not_make_a_present_wait() {
    // No window frame presented since the last telemetry frame: the old acquire is not this one's.
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let stale = waits(7, 7.1, 0.1);
    with_capture(|| {
        for _ in 0..100 {
            step_with(&mut t, &mut now, 7.0, Some(stale), None, None);
        }
        step_with(&mut t, &mut now, 14.2, Some(stale), None, None);
    });
    assert_eq!((t.counts().hitches, t.counts().present_waits), (1, 0));
}

#[test]
fn a_long_wait_stays_a_hitch() {
    // 70 ms over the median, all of it in acquire: a stall, not swapchain pacing.
    let o = TelemetryOptions::default();
    assert!(is_present_wait(14.2, 7.0, 7.1, &o));
    assert!(!is_present_wait(14.2, 7.0, 3.0, &o), "under half the excess");
    assert!(!is_present_wait(77.0, 7.0, 70.0, &o), "over hitch_abs_ms");
    assert!(!is_present_wait(7.0, 7.0, 7.0, &o), "no excess");
}

#[test]
fn the_hitch_floor_follows_the_pacing_period() {
    let o = TelemetryOptions::default();
    assert_eq!(hitch_floor_ms(&o, 0.0), 8.0, "unpaced: hitch_min_ms");
    let vsync60 = pacing_period_ms(None, true, Some(60.0));
    assert!((vsync60 - 16.667).abs() < 0.01);
    assert!((hitch_floor_ms(&o, vsync60) - 25.0).abs() < 0.01, "1.5 periods");
    assert_eq!(
        pacing_period_ms(Some(30.0), true, Some(144.0)),
        1000.0 / 30.0,
        "the slower pace"
    );
    assert_eq!(
        pacing_period_ms(None, false, Some(60.0)),
        0.0,
        "vsync off: no display pace"
    );
    assert_eq!(pacing_period_ms(Some(0.0), false, None), 0.0);
    // A 20 ms frame against an 8 ms median under 60 Hz vsync: under the floor, not a hitch; a
    // frame that missed a vsync (34 ms) still is.
    let mut t = Telemetry::new(opts());
    t.set_pacing_period_ms(Some(vsync60));
    let mut now = 0.0;
    with_capture(|| {
        run(&mut t, None, &mut now, 100, 8.0);
        run(&mut t, None, &mut now, 1, 20.0);
        run(&mut t, None, &mut now, 10, 8.0);
        assert_eq!(t.counts().hitches, 0, "under the floor");
        run(&mut t, None, &mut now, 1, 34.0);
        run(&mut t, None, &mut now, 10, 8.0);
    });
    assert_eq!(t.counts().hitches, 1);
    assert!((t.hitch_floor_ms() - 25.0).abs() < 0.01);
    t.set_pacing_period_ms(None);
    assert_eq!(t.hitch_floor_ms(), 8.0);
}

/// A phase record of update `frame` with `outside` ms in the event loop and `update` in `Update`.
fn update_phases(frame: u64, outside: f32, update: f32) -> regia_app::FramePhases {
    let mut p = regia_app::FramePhases {
        frame,
        ..regia_app::FramePhases::default()
    };
    p.ms[regia_app::Phase::OutsideUpdate as usize] = outside;
    p.ms[regia_app::Phase::Update as usize] = update;
    p
}

/// Frames of 16 ms with phase records (1 ms event loop, 15 ms `Update`), and frame `spike` of
/// `spike_ms` whose interval holds `update_ms` of `Update` in the previous update; returns the
/// hitch records.
fn interval_run(
    spike: u64,
    spike_ms: f32,
    spike_update_ms: f32,
    spans: Option<&FrameProfiler>,
) -> (Telemetry, Vec<Value>) {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        for i in 1..=spike + 4 {
            let ms = if i == spike { spike_ms } else { 16.0 };
            // Frame i's interval holds update i - 1's schedules after `First`.
            let update = if i == spike + 1 {
                1.0
            } else if i == spike {
                spike_update_ms
            } else {
                15.0
            };
            let previous = update_phases(i - 1, 1.0, if i - 1 == spike { 1.0 } else { update });
            let previous = (i > 1).then_some(&previous);
            step_with(
                &mut t,
                &mut now,
                ms,
                None,
                previous,
                if i == spike { spans } else { None },
            );
        }
    });
    let records = cap.of(target::HITCH).into_iter().map(|c| c.data).collect();
    (t, records)
}

#[test]
fn a_700_ms_frame_with_15_ms_of_phases_is_outside_frame() {
    // Tester A2 frame 51: 736 ms, 15 ms inside the engine's phases, ~720 ms nowhere.
    let (t, hitches) = interval_run(100, 700.0, 14.0, None);
    assert_eq!(hitches.len(), 1, "{hitches:?}");
    let h = &hitches[0];
    assert_eq!(h["unaccounted_ms"].as_f64(), Some(685.0), "{h}");
    assert_eq!(h["cause"], "outside_frame", "{h}");
    assert_eq!(h["interval_ms"]["measured_ms"].as_f64(), Some(15.0), "{h}");
    assert_eq!(h["event_loop_ms"].as_f64(), Some(1.0), "{h}");
    assert_eq!(
        h["interval_ms"]["previous_update"]["update"].as_f64(),
        Some(14.0),
        "{h}"
    );
    let causes = t
        .build_summary(true)
        .hitches
        .and_then(|h| h.top_causes)
        .unwrap_or_default();
    assert_eq!(causes, vec![("outside_frame".to_owned(), 1)]);
}

#[test]
fn a_slow_update_in_the_interval_names_its_phase() {
    // The same 700 ms, all of it in the previous update's `Update`: measured, not outside.
    let (_, hitches) = interval_run(100, 700.0, 684.0, None);
    assert_eq!(hitches.len(), 1, "{hitches:?}");
    let h = &hitches[0];
    assert_eq!(h["unaccounted_ms"].as_f64(), Some(15.0), "{h}");
    assert_eq!(h["cause"], "phase_update", "{h}");
    assert_eq!(h["interval_ms"]["largest"]["update"], "previous", "{h}");
}

#[test]
fn a_worker_span_never_causes_a_hitch() {
    // A warm worker's long span beside a frame whose main thread did 2 ms: the worker span is
    // background, not the cause (v0.5.0 named the launch hitch `pipeline_warm_job`).
    let profiler = FrameProfiler::new();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
    let d = dispatch.clone();
    std::thread::spawn(move || {
        tracing::dispatcher::with_default(&d, || {
            regia_core::profiling::mark_main_thread();
            let _main = tracing::info_span!("v051_main_tick").entered();
        });
    })
    .join()
    .expect("main thread");
    let d = dispatch.clone();
    std::thread::spawn(move || {
        tracing::dispatcher::with_default(&d, || {
            let _job = tracing::info_span!("v051_worker_job").entered();
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_millis(3) {
                std::hint::spin_loop();
            }
        });
    })
    .join()
    .expect("worker thread");
    profiler.end_frame();
    let (t, hitches) = interval_run(100, 520.0, 300.0, Some(&profiler));
    assert_eq!(hitches.len(), 1, "{hitches:?}");
    let h = &hitches[0];
    assert_ne!(h["cause"], "v051_worker_job", "{h}");
    assert_eq!(h["cause"], "phase_update", "{h}");
    let spans = |key: &str| -> Vec<String> {
        h[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s["span"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(
        spans("background_spans").contains(&"v051_worker_job".to_owned()),
        "{h}"
    );
    assert!(
        !spans("top_cpu_spans").contains(&"v051_worker_job".to_owned()),
        "{h}"
    );
    assert!(
        spans("top_cpu_spans").contains(&"v051_main_tick".to_owned()),
        "{h}"
    );
    let causes = t
        .build_summary(true)
        .hitches
        .and_then(|h| h.top_causes)
        .unwrap_or_default();
    assert!(!causes.iter().any(|(c, _)| c == "v051_worker_job"), "{causes:?}");
}

#[test]
fn the_hitch_record_lists_the_window_events_of_its_interval() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, None, &mut now, 100, 16.0);
        // The frame before the hitch: the window lost focus and changed mode.
        now += 0.016;
        let before = FrameInputs {
            window: cause::WindowEvents {
                focus_lost: 1,
                mode_changes: 1,
                ..cause::WindowEvents::default()
            },
            ..FrameInputs::default()
        };
        frame_step(&mut t, Some(sample(now, 16.0)), None, None, now, before);
        now += 0.7;
        let hitch = FrameInputs {
            window: cause::WindowEvents {
                resized: 2,
                ..cause::WindowEvents::default()
            },
            ..FrameInputs::default()
        };
        frame_step(&mut t, Some(sample(now, 700.0)), None, None, now, hitch);
        run(&mut t, None, &mut now, 4, 16.0);
    });
    let hitches = cap.of(target::HITCH);
    assert_eq!(hitches.len(), 1);
    let w = &hitches[0].data["window_events"];
    assert_eq!(w["resized"], 2, "{w}");
    assert_eq!(w["focus_lost"], 1, "{w}");
    assert_eq!(w["mode_changes"], 1, "{w}");
    assert_eq!(w["any"], true, "{w}");
    // Without phase records the interval is unknown, not zero.
    assert_eq!(hitches[0].data["unaccounted_ms"], Value::Null);
}

#[test]
fn the_summary_says_why_the_bound_is_unknown() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        for _ in 0..700 {
            now += 0.016;
            let s = FrameSample {
                gpu_ms: 0.0,
                ..sample(now, 16.0)
            };
            frame_step(
                &mut t,
                Some(s),
                Some(&renderer.0),
                None,
                now,
                FrameInputs::default(),
            );
        }
    });
    let s = &cap.of(target::FRAME_SUMMARY)[0].data;
    assert_eq!(s["bound"], "unknown");
    // A renderer without GPU timing turned on.
    assert_eq!(s["bound_reason"], "no_gpu_timing", "{s}");
    assert!(
        s["frame_bound"]["unknown_reasons"]["no_gpu_timing"]
            .as_u64()
            .is_some_and(|n| n >= 600),
        "{s}"
    );
    assert_eq!(t.live().bound_reason, Some(UnknownBound::NoGpuTiming));
    // A known bound has no reason.
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| run(&mut t, Some(&renderer), &mut now, 700, 16.0));
    assert_eq!(cap.of(target::FRAME_SUMMARY)[0].data["bound_reason"], Value::Null);
    assert_eq!(t.live().bound_reason, None);
}

/// The brief's measurement, deterministic: tester A1's 14 ms / 7 ms acquire sequence at the size
/// of the session (6324 such frames) through the telemetry step. Printed for the report.
#[test]
fn replay_of_the_tester_acquire_sequence() {
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let mut seq = 0;
    with_capture(|| {
        for i in 0..(6324 * 10 + 100u32) {
            seq += 1;
            let spike = i >= 100 && (i - 100).is_multiple_of(10);
            let (ms, acquire) = if spike { (14.2, 7.1) } else { (7.0, 0.2) };
            step_with(&mut t, &mut now, ms, Some(waits(seq, acquire, 0.1)), None, None);
        }
    });
    eprintln!(
        "replay tester A1 acquire sequence: hitches {}, present_waits {}",
        t.counts().hitches,
        t.counts().present_waits
    );
    assert_eq!((t.counts().hitches, t.counts().present_waits), (0, 6324));
}

#[test]
fn stale_render_reasons_and_runner_waits_reach_live_and_summary() {
    use regia_framework::feedback::{RenderTimingAbsence as A, RenderTimingSample};
    let mut t = Telemetry::new(opts());
    let first = FrameSample {
        frame_ms: 4218.53,
        cpu_ms: 4218.53,
        gpu_ms: 0.52,
        timing: Some(RenderTimingSample {
            cpu_frame_id: Some(1),
            gpu_frame_id: Some(1),
            cpu_fresh: true,
            gpu_fresh: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let runner = regia_app::RunnerState {
        stalled: true,
        throttle_ms: 100.0,
        ..Default::default()
    };
    let cap = with_capture(|| {
        frame_step(&mut t, Some(first), None, None, 4.21853, FrameInputs::default());
        for tick in 2..=9 {
            let mut phases = regia_app::FramePhases {
                frame: tick - 1,
                runner: Some(runner),
                ..Default::default()
            };
            phases.ms[regia_app::Phase::EventWait as usize] = 75.0;
            phases.ms[regia_app::Phase::Update as usize] = 25.0;
            let s = FrameSample {
                frame_ms: 100.0,
                timing: Some(RenderTimingSample {
                    cpu_frame_id: Some(1),
                    gpu_frame_id: Some(1),
                    cpu_absent: Some(A::RendererIdle),
                    gpu_absent: Some(A::RendererIdle),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if tick == 9 {
                t.request_summary("stale_regression");
            }
            frame_step(
                &mut t,
                Some(s),
                None,
                None,
                4.21853 + (tick - 1) as f64 * 0.1,
                FrameInputs {
                    phases: Some(&phases),
                    runner: Some(&runner),
                    ..Default::default()
                },
            );
            assert_eq!(t.live().bound, FrameBound::Unknown);
            assert_eq!(t.live().bound_reason, Some(UnknownBound::Hidden));
            assert_eq!(t.live().cpu_sample_ms, None);
            assert_eq!(t.live().timing.expect("timing").cpu_absent, Some(A::Hidden));
        }
    });
    let s = &cap.of(target::FRAME_SUMMARY)[0].data;
    assert_eq!(s["cpu_ms"]["samples"], 1);
    assert_eq!(s["gpu_ms"]["samples"], 1);
    assert_eq!(s["cpu_ms"]["mean"], 4218.53);
    assert_eq!(s["frame_bound"]["frames"]["unknown"], 8);
    assert_eq!(s["frame_bound"]["unknown_reasons"]["hidden"], 8);
    assert_eq!(s["live"]["timing"]["cpu_frame_id"], 1);
    assert_eq!(s["live"]["cpu_sample_ms"], Value::Null);
    assert_eq!(s["live"]["event_wait_ms"], 75.0);
    assert_eq!(s["live"]["phases_frame"], 8);
    assert_eq!(s["live"]["runner"]["focused"], false);
    assert_eq!(s["live"]["runner"]["stalled"], true);
    assert_eq!(s["live"]["runner"]["throttle_ms"], 100.0);
    assert_eq!(s["frame_breakdown_ms"]["event_wait"], 75.0);
    assert_eq!(s["frame_breakdown_ms"]["outside_update"], 0.0);
    assert_eq!(s["frame_breakdown_ms"]["update"], 25.0);
    assert_eq!(s["frame_breakdown_ms"]["runner"]["stalled"], true);
}

#[test]
fn event_wait_belongs_to_this_interval_and_is_not_update_work() {
    use regia_app::{FramePhases, Phase};
    let mut previous = FramePhases {
        frame: 1,
        ..Default::default()
    };
    previous.ms[Phase::Update as usize] = 25.0;
    previous.ms[Phase::EventWait as usize] = 900.0;
    let mut this = FramePhases {
        frame: 2,
        ..Default::default()
    };
    this.ms[Phase::EventWait as usize] = 75.0;
    let interval = cause::Interval::of(100.0, &previous, &this);
    assert_eq!(interval.measured_ms, 100.0);
    assert_eq!(interval.unaccounted_ms, 0.0);
    assert_eq!(interval.json()["event_wait_ms"], 75.0);
    assert_eq!(interval.json()["largest"]["phase"], "event_wait");
    assert_eq!(interval.json()["largest"]["update"], "this");
    assert!(interval.json()["previous_update"].get("event_wait").is_none());
    assert_eq!(
        cause::hitch_cause(&sample(1.0, 100.0), 16.0, None, Some(&interval)),
        "event_wait"
    );
    let mut breakdown = breakdown::Breakdown::new();
    breakdown.frame(3, false, Some(&this), None, Default::default(), &[]);
    assert_eq!(breakdown.hitch_wall_ms(2), Some((75.0, 0.0)));
}

#[test]
fn stale_gpu_and_deliberate_wait_never_classify_as_cpu_bound() {
    use regia_framework::feedback::{RenderTimingAbsence as A, RenderTimingSample};
    let mut t = Telemetry::new(opts());
    let stale = FrameSample {
        frame_ms: 100.0,
        cpu_ms: 25.0,
        timing: Some(RenderTimingSample {
            cpu_frame_id: Some(10),
            gpu_frame_id: Some(7),
            cpu_fresh: true,
            gpu_absent: Some(A::Pending),
            ..Default::default()
        }),
        ..Default::default()
    };
    let cap = with_capture(|| {
        frame_step(&mut t, Some(stale), None, None, 0.1, FrameInputs::default());
        assert_eq!(t.live().bound, FrameBound::Unknown);
        let mut phases = regia_app::FramePhases {
            frame: 1,
            ..Default::default()
        };
        phases.ms[regia_app::Phase::EventWait as usize] = 75.0;
        let fresh = FrameSample {
            gpu_ms: 1.0,
            timing: Some(RenderTimingSample {
                gpu_fresh: true,
                gpu_absent: None,
                ..stale.timing.expect("timing")
            }),
            ..stale
        };
        t.request_summary("event_wait_regression");
        frame_step(
            &mut t,
            Some(fresh),
            None,
            None,
            0.2,
            FrameInputs {
                phases: Some(&phases),
                ..Default::default()
            },
        );
        assert_eq!(t.live().bound_reason, Some(UnknownBound::EventWait));
    });
    let s = &cap.of(target::FRAME_SUMMARY)[0].data;
    assert_eq!(s["frame_bound"]["class"], "unknown");
    assert_eq!(s["frame_bound"]["frames"]["cpu_bound"], 0);
}

#[test]
fn small_redraw_waits_keep_fresh_frames_classifiable() {
    use regia_framework::feedback::RenderTimingSample;
    let mut t = Telemetry::new(opts());
    let s = FrameSample {
        frame_ms: 100.0,
        cpu_ms: 25.0,
        gpu_ms: 95.0,
        timing: Some(RenderTimingSample {
            cpu_fresh: true,
            gpu_fresh: true,
            cpu_frame_id: Some(3),
            gpu_frame_id: Some(1),
            ..Default::default()
        }),
        ..Default::default()
    };
    with_capture(|| {
        for (frame, wait, bound) in [
            (1, 0.1, FrameBound::GpuBound),
            (2, 49.9, FrameBound::GpuBound),
            (3, 50.0, FrameBound::Unknown),
        ] {
            let mut phases = regia_app::FramePhases {
                frame,
                ..Default::default()
            };
            phases.ms[regia_app::Phase::EventWait as usize] = wait;
            frame_step(
                &mut t,
                Some(s),
                None,
                None,
                frame as f64 * 0.1,
                FrameInputs {
                    phases: Some(&phases),
                    ..Default::default()
                },
            );
            assert_eq!(t.live().bound, bound, "event wait {wait} ms");
        }
        assert_eq!(t.live().bound_reason, Some(UnknownBound::EventWait));
    });
}

#[test]
fn hitch_gpu_requires_a_fresh_render_and_its_matching_source_frame() {
    for (fresh, source) in [(false, Some(4)), (true, Some(3)), (true, None)] {
        let mut record = json!({
            "render_timing": { "cpu_frame_id": 4, "cpu_fresh": fresh }
        });
        attach_hitch_gpu(
            &mut record,
            GpuTimings {
                frame_id: source,
                sample_id: 10,
                frame_ms: 4218.53,
                ..Default::default()
            },
        );
        assert!(
            record["gpu_ms"].is_null(),
            "cached or mismatched GPU timing: {record}"
        );
        assert!(
            record.get("gpu_ms").is_none(),
            "legacy optional numeric field stays omitted"
        );
        assert!(record.get("top_gpu_passes").is_none());
    }
    let mut record = json!({
        "render_timing": { "cpu_frame_id": 4, "cpu_fresh": true }
    });
    attach_hitch_gpu(
        &mut record,
        GpuTimings {
            frame_id: Some(4),
            sample_id: 11,
            frame_ms: 3.0,
            shadows_ms: 1.25,
            ..Default::default()
        },
    );
    assert_eq!(record["gpu_ms"], 3.0);
    assert_eq!(record["gpu_frame_id"], 4);
    assert_eq!(record["top_gpu_passes"][0]["ms"], 1.25);
}

#[test]
fn a_hitch_without_a_renderer_publication_has_no_gpu_sample() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(opts());
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 100, 16.0);
        run(&mut t, Some(&renderer), &mut now, 1, 80.0);
        run(&mut t, Some(&renderer), &mut now, 4, 16.0);
    });
    let hitches = cap.of(target::HITCH);
    assert_eq!(hitches.len(), 1);
    assert!(
        hitches[0].data["gpu_ms"].is_null(),
        "waiting two update ticks does not create a renderer timing publication: {:?}",
        hitches[0].data
    );
}

/// ADR-8417: `ship` is a level name, parsed like the others.
#[test]
fn ship_level_parses_and_names_itself() {
    assert_eq!(TelemetryLevel::parse("ship"), Some(TelemetryLevel::Ship));
    assert_eq!(TelemetryLevel::parse(" SHIP "), Some(TelemetryLevel::Ship));
    assert_eq!(TelemetryLevel::Ship.name(), "ship");
    assert_eq!(TelemetryLevel::parse("normal"), Some(TelemetryLevel::Normal));
    assert_eq!(TelemetryLevel::parse("verbose"), Some(TelemetryLevel::Verbose));
    assert_eq!(TelemetryLevel::parse("player"), None);
    assert_eq!(
        TelemetryLevel::default(),
        TelemetryLevel::Verbose,
        "the default is unchanged"
    );
}

/// ADR-8417: RAM and VRAM round up to the next tier (usable RAM reads under the installed size)
/// and cap at the last tier.
#[test]
fn ship_memory_tiers_round_up_and_cap() {
    let ram = |gb| memory_tier_gb(gb, &SHIP_RAM_TIERS_GB);
    assert_eq!(ram(3.0), Some(4));
    assert_eq!(ram(7.6), Some(8));
    assert_eq!(ram(15.4), Some(16));
    assert_eq!(ram(16.0), Some(16));
    assert_eq!(ram(16.01), Some(24));
    assert_eq!(ram(31.2), Some(32));
    assert_eq!(ram(128.0), Some(32), "the last tier means over 24 GB");
    assert_eq!(ram(0.0), None);
    assert_eq!(ram(f64::NAN), None);
    assert_eq!(ram(-1.0), None);
    let vram = |gb| memory_tier_gb(gb, &SHIP_VRAM_TIERS_GB);
    assert_eq!(vram(0.5), Some(2));
    assert_eq!(vram(5.98), Some(6));
    assert_eq!(vram(7.98), Some(8));
    assert_eq!(vram(11.99), Some(12));
    assert_eq!(vram(31.5), Some(32));
    assert_eq!(vram(48.0), Some(32));
    assert_eq!(memory_tier_gb(8.0, &[]), None);
}

/// A machine whose header carries every identifying field the Ship level must drop.
fn identifying_world() -> (World, Value) {
    let mut world = World::new();
    world.insert_resource(regia_platform::OsInfo {
        os: "linux".into(),
        version: "Arch Linux 7.3.4-arch1-1".into(),
        device_model: String::new(),
        total_ram_bytes: (15.4 * f64::from(1u32 << 30)) as u64,
        cpu_count: 16,
        cpu_model: "AMD Ryzen 7 7800X3D 8-Core Processor".into(),
    });
    let mut settings = regia_framework::settings::GameSettings::default();
    settings.graphics.render_scale = Some(0.75);
    world.insert_resource(settings);
    let mut host = TelemetryHost::default();
    host.set("player_build", "host-field-value-17");
    world.insert_resource(host);
    let mut full = session_record(&mut world, None);
    // No device in a unit test: the adapter and monitor sections a real session writes.
    full["adapter"] = json!({
        "name": "NVIDIA GeForce RTX 5090",
        "vendor": "0x10de",
        "device": "0x2b85",
        "device_type": "DiscreteGpu",
        "driver": "NVIDIA",
        "driver_info": "620.70.06",
        "backend": "Vulkan",
    });
    full["vram_gb"] = json!(31.5);
    full["features"] = json!("Features(TIMESTAMP_QUERY | PIPELINE_CACHE)");
    full["monitors"] = json!([{ "name": "DELL U2723QE", "width": 3840, "height": 2160,
        "refresh_hz": 60.0, "scale": 1.5, "current": true }]);
    (world, full)
}

/// ADR-8417 item 1: the Ship header has exactly the allowlisted keys, each projected (no OS
/// build, CPU model, monitors, settings dump, adapter name, PCI ids, driver or host fields).
#[test]
fn ship_header_is_exactly_the_allowlist() {
    let (world, full) = identifying_world();
    // The full header (Normal and Verbose) still carries them.
    for key in [
        "os_version",
        "cpu",
        "cpu_threads",
        "monitors",
        "settings_ron",
        "host",
        "boot_marks",
    ] {
        assert!(full.get(key).is_some(), "the full header keeps {key}");
    }
    let ship = ship_header(&world, &full);
    let keys: std::collections::BTreeSet<&str> = ship
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    let allowed: std::collections::BTreeSet<&str> = SHIP_HEADER_KEYS.into_iter().collect();
    assert_eq!(keys, allowed, "{ship}");
    for key in [
        "os_version",
        "cpu",
        "cpu_threads",
        "monitors",
        "window",
        "settings_ron",
        "host",
        "features",
        "limits",
        "fallbacks",
        "boot_marks",
        "pipeline_cache",
        "problems",
    ] {
        assert!(ship.get(key).is_none(), "ship header has {key}: {ship}");
    }
    let text = ship.to_string();
    for secret in [
        "Arch Linux",
        "7.3.4",
        "Ryzen",
        "RTX 5090",
        "0x10de",
        "0x2b85",
        "620.70",
        "DELL",
        "host-field-value-17",
        "Features(",
    ] {
        assert!(!text.contains(secret), "ship header leaks {secret:?}: {text}");
    }
    assert_eq!(
        ship["adapter"],
        json!({ "class": "discrete", "vendor": "nvidia", "backend": "Vulkan" })
    );
    assert_eq!(
        ship["os"],
        json!({ "family": std::env::consts::OS, "arch": std::env::consts::ARCH })
    );
    let regia: std::collections::BTreeSet<&str> = ship["regia"]
        .as_object()
        .expect("regia section")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        regia,
        ["profile", "version"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(ship["ram_gb"], json!(16));
    assert_eq!(ship["vram_gb"], json!(32));
    let preset = format!(
        "{:?}",
        regia_framework::settings::GameSettings::default().graphics.preset
    );
    assert_eq!(ship["preset"], json!(preset));
    assert_eq!(ship["render_scale"], json!(0.75));
    assert_eq!(ship["telemetry_level"], "ship");
}

/// ADR-8417: a key added to the full header later is not in the Ship header until someone adds
/// it to `SHIP_HEADER_KEYS`.
#[test]
fn a_new_full_header_key_stays_out_of_ship() {
    let (world, mut full) = identifying_world();
    full["machine_serial"] = json!("SN-0001");
    full["os"]["kernel"] = json!("7.3.4-arch1-1");
    full["adapter"]["uuid"] = json!("GPU-88c2d3f5");
    let ship = ship_header(&world, &full);
    let text = ship.to_string();
    for secret in ["machine_serial", "SN-0001", "kernel", "GPU-88c2d3f5"] {
        assert!(!text.contains(secret), "{secret} reached the ship header: {text}");
    }
}

/// ADR-8417: adapter classes and vendor families, with unknowns named as such.
#[test]
fn ship_adapter_names_class_and_vendor_family_only() {
    let adapter = |device_type: &str, vendor: &str| {
        ship_adapter(&json!({ "name": "x", "vendor": vendor, "device_type": device_type, "backend": "Dx12" }))
    };
    assert_eq!(adapter("IntegratedGpu", "0x8086")["class"], "integrated");
    assert_eq!(adapter("IntegratedGpu", "0x8086")["vendor"], "intel");
    assert_eq!(adapter("Cpu", "0x10005")["class"], "cpu");
    assert_eq!(adapter("Cpu", "0x10005")["vendor"], "mesa");
    assert_eq!(adapter("VirtualGpu", "0x1af4")["vendor"], "other");
    assert_eq!(adapter("Other", "0x0000")["class"], "other");
    assert_eq!(adapter("Other", "0x0000")["vendor"], "unknown");
    assert_eq!(adapter("DiscreteGpu", "0x1002")["vendor"], "amd");
    assert_eq!(adapter("DiscreteGpu", "0x1002")["backend"], "Dx12");
    assert_eq!(ship_adapter(&Value::Null), Value::Null, "no device: unknown");
    assert_eq!(
        adapter_label(&adapter("DiscreteGpu", "0x10de")),
        "nvidia discrete"
    );
    assert_eq!(
        adapter_label(&json!({ "name": "RTX" })),
        "RTX",
        "full headers keep the name"
    );
    assert_eq!(adapter_label(&Value::Null), "none");
}

/// Runs the session header writer once with no device (300 frames in, as headless does).
fn write_header(world: &mut World) -> Capture {
    world.resource_mut::<Telemetry>().frame = 300;
    let cap = with_capture(|| telemetry_session(world));
    if let Some(t) = world.get_resource::<Telemetry>() {
        t.signal_probe_stop();
    }
    cap
}

/// ADR-8417: the `regia::session` event, `session_json` and the `summary.json` record at Ship
/// carry the allowlisted header only; Verbose writes the full header as before.
#[test]
fn the_ship_session_event_and_summary_carry_the_allowlist_only() {
    let (mut world, _) = identifying_world();
    world.insert_resource(Telemetry::new(TelemetryOptions {
        level: TelemetryLevel::Ship,
        summary: false,
        ..opts()
    }));
    let cap = write_header(&mut world);
    let events = cap.of(target::SESSION);
    assert_eq!(events.len(), 1, "{events:?}");
    let header = &events[0].data;
    let keys: std::collections::BTreeSet<&str> = header
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        SHIP_HEADER_KEYS
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        "{header}"
    );
    assert_eq!(header["ram_gb"], json!(16));
    assert_eq!(header["telemetry_level"], "ship");
    let t = world.resource::<Telemetry>();
    let stored: Value = serde_json::from_str(t.session_json().expect("session json")).expect("json");
    assert_eq!(&stored, header);
    let summary = serde_json::to_value(t.build_summary(true)).expect("summary json");
    let system = &summary["system"];
    for key in ["cpu", "cpu_threads", "gpu", "driver"] {
        assert!(system[key].is_null(), "summary system.{key} at ship: {system}");
    }
    assert_eq!(system["ram_gb"], json!(16.0), "{system}");
    assert_eq!(
        system["os"],
        json!(format!("{} {}", std::env::consts::OS, std::env::consts::ARCH))
    );
    assert!(summary["host"].is_null(), "no host section at ship: {summary}");
    assert!(summary["display"]["monitors"].is_null(), "{summary}");
    assert!(summary["settings"]["auto_plan"].is_null(), "{summary}");
    let text = summary.to_string();
    for secret in ["Arch Linux", "Ryzen", "host-field-value-17"] {
        assert!(
            !text.contains(secret),
            "summary.json leaks {secret:?} at ship: {text}"
        );
    }
    drop(world);

    // The same session at Verbose writes the full header, as before.
    let (mut world, _) = identifying_world();
    world.insert_resource(Telemetry::new(TelemetryOptions {
        summary: false,
        ..opts()
    }));
    let cap = write_header(&mut world);
    let header = &cap.of(target::SESSION)[0].data;
    assert_eq!(header["os_version"], "Arch Linux 7.3.4-arch1-1");
    assert_eq!(header["cpu"], "AMD Ryzen 7 7800X3D 8-Core Processor");
    assert_eq!(header["host"]["player_build"], "host-field-value-17");
    assert_eq!(header["telemetry_level"], "verbose");
    let summary = serde_json::to_value(world.resource::<Telemetry>().build_summary(true)).expect("json");
    assert_eq!(summary["system"]["cpu"], "AMD Ryzen 7 7800X3D 8-Core Processor");
    assert_eq!(summary["host"]["player_build"], "host-field-value-17");
}

/// ADR-8417 item 2: a Ship frame summary carries the frame-time percentiles, GPU time, bound,
/// pass times, render scale and hitch counts; Ship writes no `regia::hitch` event, even a severe
/// one, and still counts it.
#[test]
fn ship_summaries_carry_frame_percentiles_and_hitches_are_counted_only() {
    let renderer = RuntimeRenderer::default();
    let mut t = Telemetry::new(TelemetryOptions {
        level: TelemetryLevel::Ship,
        ..opts()
    });
    let mut now = 0.0;
    let cap = with_capture(|| {
        run(&mut t, Some(&renderer), &mut now, 60, 16.0);
        run(&mut t, Some(&renderer), &mut now, 1, 300.0);
        run(&mut t, Some(&renderer), &mut now, 1, 45.0);
        run(&mut t, Some(&renderer), &mut now, 700, 16.0);
    });
    assert!(cap.of(target::HITCH).is_empty(), "ship writes no hitch event");
    assert_eq!(t.counts().hitches, 2, "both hitches count");
    assert_eq!(t.counts().hitch_events, 0);
    let summaries = cap.of(target::FRAME_SUMMARY);
    let s = &summaries.first().expect("a frame summary at ship").data;
    for p in ["p50", "p95", "p99", "max", "mean"] {
        assert!(s["frame_ms"][p].as_f64().is_some(), "frame_ms.{p}: {s}");
    }
    assert!(s["gpu_ms"]["mean"].as_f64().is_some(), "{s}");
    assert_eq!(s["bound"], "gpu");
    assert!(s["frame_bound"]["class"].is_string(), "{s}");
    assert!(s.get("gpu_pass_ms").is_some_and(Value::is_object), "{s}");
    assert!(s.get("render_scale_permille").is_some(), "{s}");
    assert_eq!(s["hitches"], json!(2), "{s}");
    assert_eq!(s["hitches_suppressed"], json!(2), "{s}");
}

/// ADR-8417: Ship's `regia::problems` keeps the codes and drops the message text.
#[test]
fn ship_problems_carry_codes_without_messages() {
    let mut t = Telemetry::new(TelemetryOptions {
        level: TelemetryLevel::Ship,
        ..opts()
    });
    let a = [FrameProblem {
        code: "texture_missing",
        message: "assets/levels/secret_room/wall.ktx2 is missing".into(),
    }];
    let cap = with_capture(|| {
        problems_changed(&mut t, &[]);
        problems_changed(&mut t, &a);
    });
    let events = cap.of(target::PROBLEMS);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].data["problems"], json!([{ "code": "texture_missing" }]));
}
