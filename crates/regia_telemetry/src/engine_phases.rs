// SPDX-License-Identifier: MPL-2.0
//! The engine's own `regia::phase` events (spec 28 § 3) and its clean-exit hooks (ADR-188):
//! `boot_done`, `shaders_ready`, `scene_load_start` / `scene_load_end` (the scene warm, ADR-190),
//! `scene_ready` (the host's scene prepare presented, ADR-7883),
//! `quit_start` / `quit_done`, plus the final `summary.json` written inside the exit sequence,
//! before the world drops.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use regia_app::prelude::*;
use regia_platform::{CleanExitReport, CleanExitSettings, ExitHooks, ExitStep};
use regia_render::{GameFrameWarm, GameWarmReport, SceneReadyState, SceneReadyStatus};
use serde_json::{Value, json};

use crate::summary::{ExitInfo, ExitStepInfo, ShadersInfo, SummaryPaths, write_atomic};
use crate::{SessionSummary, Telemetry, TelemetryHost, phase};

fn ms(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Emits `boot_done` and `shaders_ready` once the boot timeline has their marks. Called when the
/// session header is out and whenever a mark is recorded after it.
pub(crate) fn from_marks(t: &mut Telemetry, timeline: &[(String, f64)]) {
    if !t.header_done {
        return;
    }
    let find = |name: &str| timeline.iter().find(|(n, _)| n == name).map(|(_, ms)| *ms);
    if !t.boot_done_sent
        && let Some(first_present) = find("first_present")
    {
        t.boot_done_sent = true;
        phase::phase("boot_done", json!({ "ms": ms(first_present) }));
    }
    if !t.shaders_ready_sent
        && let Some(menu_ready) = find("menu_tier_ready")
    {
        t.shaders_ready_sent = true;
        // The warm queue starts right after the first present (ADR-173).
        let started = find("first_present").unwrap_or(0.0);
        let warm_ms = (menu_ready - started).max(0.0);
        let pipelines = regia_render::pipeline_cache::pipelines_created();
        let ms_each = if pipelines > 0 {
            warm_ms / pipelines as f64
        } else {
            0.0
        };
        let cache = cache_label();
        let threads = regia_render::pipeline_warm::default_threads() as u64;
        let mut data = json!({
            "pipelines": pipelines,
            "ms": ms(warm_ms),
            "ms_each": ms(ms_each),
            "cache": cache,
            "threads": threads,
            "backend": t.backend,
        });
        if let Some(boot_tier) = find("boot_tier_ready") {
            data["boot_tier_ms"] = json!(ms((boot_tier - started).max(0.0)));
        }
        phase::phase("shaders_ready", data);
        t.agg.shaders = Some(ShadersInfo {
            pipelines: Some(pipelines),
            pipelines_total: None,
            ms: Some(ms(warm_ms)),
            ms_each: Some(ms(ms_each)),
            cache: Some(cache.to_owned()),
            threads: Some(threads),
            backend: (!t.backend.is_empty()).then(|| t.backend.clone()),
            driver_cache: None,
        });
    }
}

/// `hit`, `miss` or the cache's other states, in one word.
fn cache_label() -> &'static str {
    use regia_render::pipeline_cache::CacheLoad;
    match regia_render::pipeline_cache::status().load {
        Some(CacheLoad::Loaded(_) | CacheLoad::Seeded(_)) => "hit",
        Some(CacheLoad::Missing) => "miss",
        Some(CacheLoad::Discarded(_)) => "discarded",
        None => "off",
    }
}

/// `scene_load_start` when a game-frame warm starts, `scene_load_end` when one finishes
/// (ADR-190) and its coverage is known ([`GameWarmReport::complete`]: the first scene frame after
/// it, ADR-7883), `scene_ready` when the host's scene prepare reached its presented edge
/// (ADR-7883). Nothing happens on a frame without a change.
pub(crate) fn scene_warm(t: &mut Telemetry, warm: &GameFrameWarm) {
    let started = warm.warms_started();
    if started > t.warms_started_seen {
        t.warms_started_seen = started;
        phase::phase("scene_load_start", json!({ "warm": started }));
    }
    if let Some(report) = warm.last_report()
        && report.seq > t.warm_report_seen
        && report.complete.is_some()
    {
        t.warm_report_seen = report.seq;
        let (level, data) = scene_load_end_data(report);
        phase::emit(level, "scene_load_end", data);
    }
    if let Some(status) = warm.prepare_status()
        && status.state == SceneReadyState::Presented
        && status.prepare > t.scene_ready_seen
    {
        t.scene_ready_seen = status.prepare;
        let (level, data) = scene_ready_data(&status);
        phase::emit(level, "scene_ready", data);
    }
}

/// The `scene_load_end` event of a finished warm (spec 28 § 3: `ms`, `mb_uploaded`, `pipelines`,
/// `meshes`; ADR-7883: `map` when the warm was a round of the host's scene prepare, `complete`:
/// false when the first scene frame after it still uploaded meshes or compiled pipelines) and its
/// level: a failed or timed-out load is a warning, so it survives the log's size cap.
pub(crate) fn scene_load_end_data(report: &GameWarmReport) -> (tracing::Level, Value) {
    let mut data = json!({
        "ms": ms(f64::from(report.ms)),
        "mb_uploaded": ms(f64::from(report.mb_uploaded)),
        "pipelines": report.pipelines,
        "meshes": report.meshes,
    });
    if let Some(map) = &report.map_id {
        data["map"] = json!(map);
    }
    if let Some(complete) = report.complete {
        data["complete"] = json!(complete);
    }
    if report.timed_out {
        data["timed_out"] = json!(true);
    }
    if let Some(error) = &report.error {
        data["error"] = json!(error);
    }
    let level = if report.timed_out || report.error.is_some() {
        tracing::Level::WARN
    } else {
        tracing::Level::INFO
    };
    (level, data)
}

/// The `scene_ready` event of a presented scene prepare (ADR-7883): `map`, `first_present_ms`
/// (from the prepare call to the present of its first scene frame), `frame`, the warm totals
/// (`warms`, `ms`, `mb_uploaded`, `meshes`, `pipelines`), `complete`, `presented` (false: headless
/// or hidden window) and `timed_out`/`error` when it ended early (then a warning).
pub(crate) fn scene_ready_data(status: &SceneReadyStatus) -> (tracing::Level, Value) {
    let mut data = json!({
        "map": status.map_id.clone().unwrap_or_default(),
        "first_present_ms": ms(f64::from(status.first_present_ms.unwrap_or(0.0))),
        "frame": status.first_present_frame.unwrap_or(0),
        "warms": status.warms,
        "ms": ms(f64::from(status.ms)),
        "mb_uploaded": ms(f64::from(status.uploaded_mb)),
        "meshes": status.meshes,
        "pipelines": status.pipelines,
        "presented": status.presented_to_window,
    });
    if let Some(complete) = status.complete {
        data["complete"] = json!(complete);
    }
    if status.timed_out {
        data["timed_out"] = json!(true);
    }
    if let Some(error) = &status.error {
        data["error"] = json!(error);
    }
    let level = if status.timed_out {
        tracing::Level::WARN
    } else {
        tracing::Level::INFO
    };
    (level, data)
}

/// What the exit hooks share: where the final summary went and what it said, so the closing
/// callback can add the sequence's own timings.
#[derive(Default)]
struct ExitShared {
    last: Mutex<Option<(SummaryPaths, SessionSummary)>>,
    started: Mutex<Option<Instant>>,
}

/// Registers the exit hooks: `quit_start` and the final summary at the first step (the world is
/// intact; the renderer's warm queue stops after it), `quit_done` and the summary's exit timings
/// when the sequence is over.
pub(crate) fn register_exit(app: &mut App) {
    let shared = Arc::new(ExitShared::default());
    let mut hooks = app.world_mut().get_resource_or_init::<ExitHooks>();
    let first = shared.clone();
    hooks.add(ExitStep::StopBackgroundWork, "telemetry summary", move |world| {
        *first.started.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        phase::phase("quit_start", Value::Null);
        let watchdog = world
            .get_resource::<CleanExitSettings>()
            .is_some_and(|s| s.watchdog.is_some());
        let host = world.get_resource::<TelemetryHost>().map(TelemetryHost::summary_section);
        let Some(t) = world.get_resource::<Telemetry>() else {
            return;
        };
        let Some(paths) = t.summary_paths().cloned() else {
            return;
        };
        let mut record = t.build_summary(false);
        if host.is_some() {
            record.host = host;
        }
        // Not clean until the sequence has finished (`on_done`): a hang or crash during teardown
        // leaves this file saying so.
        record.exit = Some(ExitInfo {
            clean: Some(false),
            watchdog: Some(watchdog),
            ..ExitInfo::default()
        });
        if let Err(e) = write_atomic(&paths.final_path, &record.to_json()) {
            tracing::warn!(target: crate::target::TELEMETRY, "cannot write {}: {e}", paths.final_path.display());
        }
        *first.last.lock().unwrap_or_else(PoisonError::into_inner) = Some((paths, record));
    });
    hooks.on_done(move |report: &CleanExitReport| {
        let total = report.total.as_secs_f64() * 1e3;
        let steps: Vec<(&'static str, f64)> = report
            .steps
            .iter()
            .map(|t| (t.step.name(), t.elapsed.as_secs_f64() * 1e3))
            .collect();
        let step_list: Vec<Value> = steps.iter().map(|(n, m)| json!({ "step": n, "ms": ms(*m) })).collect();
        phase::phase("quit_done", json!({ "ms": ms(total), "steps": step_list }));
        let Some((paths, mut record)) = shared.last.lock().unwrap_or_else(PoisonError::into_inner).take() else {
            return;
        };
        refresh_error_counts(&mut record);
        if let Some(exit) = record.exit.as_mut() {
            exit.clean = Some(true);
            exit.ms = Some(ms(total));
            exit.steps = Some(
                steps
                    .iter()
                    .map(|(n, m)| ExitStepInfo {
                        step: Some((*n).to_owned()),
                        ms: Some(ms(*m)),
                    })
                    .collect(),
            );
        }
        if let Err(e) = write_atomic(&paths.final_path, &record.to_json()) {
            tracing::warn!(target: crate::target::TELEMETRY, "cannot write {}: {e}", paths.final_path.display());
        }
    });
}

/// Re-reads the error counters into `record`: errors raised after the summary was built (a device
/// loss during teardown) must not be missing from the file that says the exit was clean.
pub(crate) fn refresh_error_counts(record: &mut SessionSummary) {
    use regia_core::telemetry::{Counter, counter};
    if let Some(errors) = record.errors.as_mut() {
        errors.gpu_errors = Some(counter(Counter::GpuErrors));
        errors.device_lost = Some(counter(Counter::DeviceLost));
        errors.panics = Some(counter(Counter::Panics));
    }
}

/// The newest partial summary and where its final goes, for the panic hook.
static PANIC_SNAPSHOT: Mutex<Option<(SummaryPaths, SessionSummary)>> = Mutex::new(None);

pub(crate) fn remember_for_panic(paths: &SummaryPaths, record: SessionSummary) {
    let next = Some((paths.clone(), record));
    // Swap under the lock but drop the previous record after it is released: a panic hook that
    // runs meanwhile (`try_lock`) must not find the slot busy for the duration of a drop.
    let old = std::mem::replace(
        &mut *PANIC_SNAPSHOT.lock().unwrap_or_else(PoisonError::into_inner),
        next,
    );
    drop(old);
}

/// The panic hook's callback: writes `summary.json` from the newest partial, marked as ending in
/// a panic (up to one partial interval old). A panic while the snapshot is being replaced writes
/// nothing; the partial file on disk is still there.
///
/// Shipping builds abort on panic, so every panic there ends the process. With unwinding a worker
/// thread's panic is often caught and the session carries on: only the main thread's panic writes
/// the file then, so a caught worker panic cannot mark a healthy session as crashed (the
/// `errors.panics` counter still records it).
pub(crate) fn write_panic_summary() {
    if !cfg!(panic = "abort") && std::thread::current().name() != Some("main") {
        return;
    }
    let Ok(guard) = PANIC_SNAPSHOT.try_lock() else {
        return;
    };
    let Some((paths, mut record)) = guard.clone() else {
        return;
    };
    drop(guard);
    record.partial = None;
    record.exit = Some(ExitInfo {
        clean: Some(false),
        panic: Some(true),
        ..ExitInfo::default()
    });
    if let Some(errors) = record.errors.as_mut() {
        errors.panics = Some(regia_core::telemetry::counter(
            regia_core::telemetry::Counter::Panics,
        ));
    }
    // No `tracing` from a panic hook: stderr is all that is safe here.
    if let Err(e) = write_atomic(&paths.final_path, &record.to_json()) {
        eprintln!(
            "regia: cannot write {} after a panic: {e}",
            paths.final_path.display()
        );
    }
}
