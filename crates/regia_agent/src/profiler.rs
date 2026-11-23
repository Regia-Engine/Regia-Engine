// SPDX-License-Identifier: MPL-2.0
//! The frame profiler through the agent protocol (`docs/specs/83-prefab-profiler-volumes.md`
//! § 4.3, ADR-2323, `book/agents/profiler.md`): `profiler.set_capture`, `profiler.frame`,
//! `profiler.capture`, `profiler.compare` (ADR-9740) and `profiler.timeline` (ADR-7325).
//!
//! The data is the process frame profiler (`regia_telemetry::SpanProfiler`, fed by the logging
//! layer): its flame graph (span call tree, the per-frame mean over a window of frames; flame
//! capture is opt-in) and its per-span averages, plus the main view's GPU pass times when the
//! device has timestamp queries and GPU timing is on.
//!
//! Error reasons (`data.reason`): `no_profiler`, `bad_flame`, `bad_frames`, `bad_depth`, `bad_max_nodes`,
//! `bad_path`, `no_project_root`, `path_outside_project`, `golden_path_denied`, `write_failed`, `bad_top`, `bad_capture`, `bad_timeline`, `timeline_off`, `bad_frame`, `bad_threads`, `bad_max_spans`, `bad_since_frame`, `bad_entities`.

use std::path::PathBuf;

use regia_app::prelude::*;
use regia_core::profiling::FrameProfiler;
use regia_telemetry::profile_capture::{
    CaptureTimelineFrame, DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES, DEFAULT_MAX_TIMELINE_SPANS,
    PROFILE_CAPTURE_SUFFIX, ProfileCapture,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Default window of `profiler.frame` and `profiler.capture`, frames.
pub const DEFAULT_FRAMES: u64 = 60;
/// Most frames a call may average (the profiler's history).
pub const MAX_FRAMES: u64 = regia_core::profiling::HISTORY_FRAMES as u64;
/// Deepest tree a call may ask for.
pub const MAX_DEPTH: u64 = 32;
/// Most nodes a call may ask for.
pub const MAX_NODES: u64 = 5_000;
/// Spans in the `top` list.
const TOP_SPANS: usize = 10;

fn error(code: i64, regia_code: &str, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": "see book/agents/profiler.md" })),
    }
}

fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    error(protocol::INVALID_PARAMS, "F0002", reason, message)
}

fn missing(reason: &str, message: impl Into<String>) -> RpcError {
    error(protocol::NOT_FOUND, "F0003", reason, message)
}

fn profiler(world: &World) -> Result<FrameProfiler, RpcError> {
    world
        .get_resource::<regia_telemetry::SpanProfiler>()
        .map(|p| p.0.clone())
        .ok_or_else(|| {
            missing(
                "no_profiler",
                "no frame profiler is installed (DefaultPlugins installs one; a custom subscriber adds \
                 regia::frame_profiler() as a layer)",
            )
        })
}

/// An optional integer in `1..=max` (default `default`), or a `reason` error.
fn bounded(params: &Value, key: &str, default: u64, max: u64, reason: &str) -> Result<usize, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default as usize),
        Some(value) => match value.as_u64() {
            Some(n) if (1..=max).contains(&n) => Ok(n as usize),
            _ => Err(invalid(
                reason,
                format!("`{key}` must be an integer in 1..={max}"),
            )),
        },
    }
}

/// The main view's GPU pass times (`(pass, ms)`), empty without timestamps.
fn gpu_passes(world: &World) -> Vec<(&'static str, f32)> {
    world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map(|r| r.0.gpu_timings())
        .filter(|g| g.frame_ms > 0.0)
        .map(|g| g.passes().to_vec())
        .unwrap_or_default()
}

fn capture_of(world: &World, params: &Value) -> Result<(FrameProfiler, ProfileCapture), RpcError> {
    let profiler = profiler(world)?;
    let frames = bounded(params, "frames", DEFAULT_FRAMES, MAX_FRAMES, "bad_frames")?;
    let depth = bounded(params, "depth", DEFAULT_MAX_DEPTH as u64, MAX_DEPTH, "bad_depth")?;
    let nodes = bounded(
        params,
        "max_nodes",
        DEFAULT_MAX_NODES as u64,
        MAX_NODES,
        "bad_max_nodes",
    )?;
    let graph = profiler.flame_graph(frames);
    let capture = ProfileCapture::from_flame(&graph, &gpu_passes(world), depth, nodes);
    Ok((profiler, capture))
}

/// An optional boolean `key`, or a `reason` error when it is present and not a boolean.
fn optional_bool(params: &Value, key: &str, reason: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid(reason, format!("`{key}` must be true or false"))),
    }
}

/// `profiler.set_capture { flame?, timeline? }`: turns flame (call tree) capture and the
/// per-thread timeline (ADR-7325) on or off; at least one is required.
pub fn set_capture(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let profiler = profiler(world)?;
    let flame = optional_bool(params, "flame", "bad_flame")?;
    let timeline = optional_bool(params, "timeline", "bad_timeline")?;
    if flame.is_none() && timeline.is_none() {
        return Err(invalid(
            "bad_flame",
            "`flame` or `timeline` (true or false) is required",
        ));
    }
    if let Some(flame) = flame {
        profiler.set_flame_enabled(flame);
    }
    if let Some(timeline) = timeline {
        profiler.set_timeline_enabled(timeline);
    }
    Ok(json!({
        "flame": profiler.flame_enabled(),
        "timeline": profiler.timeline_enabled(),
        "frames_recorded": profiler.frame_count(),
    }))
}

/// Default and most spans `profiler.timeline` returns.
pub const DEFAULT_TIMELINE_SPANS: u64 = 5_000;
/// Most spans `profiler.timeline` may return.
pub const MAX_TIMELINE_SPANS: u64 = 100_000;
/// Most frames a `profiler.timeline` strip may cover (ADR-7409).
pub const MAX_TIMELINE_FRAMES: u64 = 8;

/// `profiler.timeline { frame?, frames?, threads?, max_spans? }`: the per-thread timeline of
/// one retained frame (default the newest): per thread its spans with start and end (ns from
/// the frame start) and nesting depth. `frames` (1-8, default 1) lays that many consecutive
/// frames (ending at `frame`, oldest first) on one time axis: every span keeps its place on the
/// axis and `frames` lists each frame with its start offset. `timeline_off` while the
/// profiler's timeline is off.
pub fn timeline(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let profiler = profiler(world)?;
    let max_spans = bounded(
        params,
        "max_spans",
        DEFAULT_TIMELINE_SPANS,
        MAX_TIMELINE_SPANS,
        "bad_max_spans",
    )?;
    let strip_frames = bounded(params, "frames", 1, MAX_TIMELINE_FRAMES, "bad_frames")?;
    let number = match params.get("frame") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| {
            invalid(
                "bad_frame",
                "`frame` must be a frame number (an integer from a previous answer)",
            )
        })?),
    };
    let threads: Option<Vec<String>> = match params.get("threads") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| invalid("bad_threads", "`threads` must be an array of thread names"))?,
        ),
        Some(_) => {
            return Err(invalid(
                "bad_threads",
                "`threads` must be an array of thread names",
            ));
        }
    };
    if !profiler.timeline_enabled() {
        return Err(missing(
            "timeline_off",
            "the per-thread timeline is off: call profiler.set_capture { timeline: true } and let a frame run",
        ));
    }
    let numbers = profiler.frame_numbers();
    // The strip ends at the asked frame (or the newest) and covers the `strip_frames` newest
    // retained frames up to it, oldest first.
    let end = match number {
        Some(n) => numbers.iter().position(|&k| k == n).ok_or_else(|| {
            invalid(
                "bad_frame",
                format!(
                    "frame {n} is not in the history (frames {} to {})",
                    numbers.first().copied().unwrap_or(0),
                    numbers.last().copied().unwrap_or(0)
                ),
            )
        })?,
        // An empty history (the timeline is on and no frame has ended yet) has no newest frame.
        None => numbers
            .len()
            .checked_sub(1)
            .ok_or_else(|| missing("timeline_off", "no frame has been recorded yet"))?,
    };
    let first = end.saturating_sub(strip_frames.saturating_sub(1));
    let per_frame: Vec<regia_core::profiling::TimelineFrame> = numbers[first..=end]
        .iter()
        .filter_map(|n| profiler.timeline_frame(Some(*n)))
        .collect();
    let captured_last = per_frame
        .last()
        .cloned()
        .ok_or_else(|| missing("timeline_off", "no frame has been recorded yet"))?;
    // One frame answers as before (its own wall time); more lay out as a strip on one axis.
    let strip = (strip_frames > 1).then(|| {
        let pieces: Vec<CaptureTimelineFrame> =
            per_frame.iter().map(CaptureTimelineFrame::from_frame).collect();
        regia_telemetry::profile_capture::timeline_strip(&pieces)
    });
    let (mut captured, strip_frames_meta) = match strip {
        Some(strip) => (strip.combined, strip.frames),
        None => (CaptureTimelineFrame::from_frame(&captured_last), Vec::new()),
    };
    if let Some(names) = &threads {
        captured.threads.retain(|t| names.contains(&t.name));
    }
    let total = captured.span_count();
    let mut budget = max_spans;
    for thread in &mut captured.threads {
        thread.spans.truncate(budget);
        budget -= thread.spans.len();
    }
    let frames: Vec<Value> = if strip_frames_meta.is_empty() {
        vec![json!({
            "number": captured.number,
            "frame_ns": captured.frame_ns,
            "start_ns": 0,
            "span_ns": captured.span_ns(),
            "dropped": captured.dropped,
        })]
    } else {
        strip_frames_meta
            .iter()
            .map(|f| {
                json!({
                    "number": f.number,
                    "frame_ns": f.frame_ns,
                    "start_ns": f.start_ns,
                    "span_ns": f.span_ns,
                    "dropped": f.dropped,
                })
            })
            .collect()
    };
    Ok(json!({
        "frame": captured.number,
        "first_frame": numbers.first(),
        "newest_frame": numbers.last(),
        "frame_ns": captured.frame_ns,
        "frame_ms": (captured.frame_ns as f64 / 1e3).round() / 1e3,
        "dropped": captured.dropped,
        "spans": total.min(max_spans),
        "truncated": total.saturating_sub(max_spans),
        "frames": frames,
        "threads": captured.threads,
    }))
}

/// Spans listed per row of a `profiler.frame { since_frame }` answer (the costliest first).
pub const FRAME_TIME_SPANS: usize = 5;
/// Most components `profiler.frame { entities }` ranks.
pub const MAX_ENTITY_COMPONENTS: u64 = 200;
/// Attempts at a consistent read of the frame history while frames keep ending on other threads.
const FRAME_TIMES_ATTEMPTS: usize = 4;

/// A duration in ms, rounded to the microsecond.
fn round_ms(time: std::time::Duration) -> f64 {
    (time.as_secs_f64() * 1e6).round() / 1e3
}

/// Every retained frame numbered `since` or later, oldest first: `{ number, ms, spans }` with
/// the frame's wall time and its [`FRAME_TIME_SPANS`] costliest spans by name. The rows are read
/// index by index, so the read is redone when a frame ended meanwhile (the newest number moved);
/// after [`FRAME_TIMES_ATTEMPTS`] moving reads the answer is empty and the caller asks again
/// with the same cursor.
#[must_use]
pub fn frame_times_since(profiler: &FrameProfiler, since: u64) -> Vec<Value> {
    let _span = tracing::trace_span!("profiler_frame_times").entered();
    for _ in 0..FRAME_TIMES_ATTEMPTS {
        let numbers = profiler.frame_numbers();
        let times = profiler.frame_times();
        let first = numbers.partition_point(|&n| n < since);
        let rows: Vec<Value> = numbers
            .iter()
            .zip(&times)
            .enumerate()
            .skip(first)
            .map(|(index, (number, time))| {
                let spans: Vec<Value> = profiler
                    .frame_spans(index)
                    .into_iter()
                    .take(FRAME_TIME_SPANS)
                    .map(|(name, total, _)| json!({ "name": name, "ms": round_ms(total) }))
                    .collect();
                json!({ "number": number, "ms": round_ms(*time), "spans": spans })
            })
            .collect();
        let steady = numbers.len() == times.len()
            && profiler.newest_frame_number() == numbers.last().copied()
            && profiler.frame_count() == numbers.len();
        if steady {
            return rows;
        }
    }
    Vec::new()
}

/// ECS counts of `world`: spawned entities, archetypes, and the `top` component types by the
/// entities that hold them, most first (ties by name). Walks the archetype table, not the
/// entities: each non-empty archetype adds its length to each of its component types. The
/// editor Profiler's in-process Entities view and `profiler.frame { entities }` both count
/// with this, so a process source shows the same rows as an in-process one.
#[must_use]
pub fn entity_counts(world: &World, top: usize) -> (u32, usize, Vec<(String, u64)>) {
    let mut by_id: std::collections::BTreeMap<regia_ecs::ecs::component::ComponentId, u64> =
        std::collections::BTreeMap::new();
    for archetype in world.archetypes().iter() {
        let n = u64::from(archetype.len());
        if n == 0 {
            continue;
        }
        for &id in archetype.components() {
            *by_id.entry(id).or_default() += n;
        }
    }
    let mut components: Vec<(String, u64)> = by_id
        .into_iter()
        .map(|(id, n)| {
            let name = world
                .components()
                .get_info(id)
                .map_or_else(|| format!("{id:?}"), |info| info.name().to_string());
            (name, n)
        })
        .collect();
    components.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    components.truncate(top);
    (
        world.entities().count_spawned(),
        world.archetypes().len(),
        components,
    )
}

/// `profiler.frame { frames?, depth?, max_nodes?, since_frame?, entities? }`: the CPU call tree
/// over the newest frames, the costliest spans, and the GPU passes. `since_frame` adds
/// `frame_times`: every retained frame from that number on (a poller's cursor, so it sees every
/// frame, not one sample per poll) and `newest_frame`. `entities` (1-200) adds the ECS counts with
/// that many component types ranked by entity count.
pub fn frame(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (profiler, capture) = capture_of(world, params)?;
    let frames = bounded(params, "frames", DEFAULT_FRAMES, MAX_FRAMES, "bad_frames")?;
    let since = match params.get("since_frame") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| {
            invalid(
                "bad_since_frame",
                "`since_frame` must be a frame number (0 or the last answer's newest_frame + 1)",
            )
        })?),
    };
    let entities = match params.get("entities") {
        None | Some(Value::Null) => None,
        Some(_) => Some(bounded(
            params,
            "entities",
            1,
            MAX_ENTITY_COMPONENTS,
            "bad_entities",
        )?),
    };
    let top: Vec<Value> = profiler
        .averages(frames)
        .into_iter()
        .take(TOP_SPANS)
        .map(|(name, time)| json!({ "name": name, "ms": round_ms(time) }))
        .collect();
    let mut answer = json!({
        "flame_enabled": profiler.flame_enabled(),
        "timeline_enabled": profiler.timeline_enabled(),
        "frames_recorded": profiler.frame_count(),
        "frames": capture.frames,
        "frame_ms": capture.frame_ms,
        "nodes": capture.node_count(),
        "truncated": capture.truncated,
        "systems": regia_core::profiling::system_count(),
        "systems_over_cap": regia_core::profiling::systems_over_cap(),
        "cpu": capture.cpu,
        "top": top,
        "gpu": capture.gpu,
    });
    if let Some(map) = answer.as_object_mut() {
        if let Some(since) = since {
            let rows = frame_times_since(&profiler, since);
            let newest = rows.last().and_then(|r| r["number"].as_u64());
            map.insert("frame_times".into(), Value::Array(rows));
            map.insert(
                "newest_frame".into(),
                json!(newest.or_else(|| profiler.newest_frame_number())),
            );
        }
        if let Some(top) = entities {
            let (entities, archetypes, components) = entity_counts(world, top);
            map.insert(
                "entities".into(),
                json!({
                    "entities": entities,
                    "archetypes": archetypes,
                    "components": components
                        .iter()
                        .map(|(name, n)| json!({ "name": name, "entities": n }))
                        .collect::<Vec<Value>>(),
                }),
            );
        }
    }
    Ok(answer)
}

/// Resolves a capture path: relative to the project root, no `..`, ending in `.rprof.json`.
fn capture_path(world: &World, path: &str) -> Result<PathBuf, RpcError> {
    if regia_core::project_path::check_relative(path).is_err() {
        return Err(invalid(
            "bad_path",
            "`path` must be relative to the project root, without `..`",
        ));
    }
    if !path.to_ascii_lowercase().ends_with(PROFILE_CAPTURE_SUFFIX) {
        return Err(invalid(
            "bad_path",
            format!("`path` must end in {PROFILE_CAPTURE_SUFFIX}"),
        ));
    }
    let root = world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .ok_or_else(|| {
            missing(
                "no_project_root",
                "no AssetServer resource: no project root to write under",
            )
        })?;
    // Lexical `bad_path` is already returned. This refuses a symlink whose target leaves the
    // project (ADR-1816) before anything is written.
    crate::req_path::project_path_in(&root, "path", path)
}

/// The per-thread timeline of the newest `frames` retained frames, oldest first (ADR-7325).
#[must_use]
pub fn window_timeline(profiler: &FrameProfiler, frames: usize) -> Vec<regia_core::profiling::TimelineFrame> {
    let numbers = profiler.frame_numbers();
    let skip = numbers.len().saturating_sub(frames);
    numbers[skip..]
        .iter()
        .filter_map(|n| profiler.timeline_frame(Some(*n)))
        .collect()
}

/// `profiler.capture { path, frames?, depth?, max_nodes?, timeline? }`: writes a `.rprof.json`
/// capture, with the window's per-thread timeline when `timeline` (default: the profiler's
/// timeline is on).
pub fn capture(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("bad_path", "`path` (project-relative, .rprof.json) is required"))?;
    let with_timeline = optional_bool(params, "timeline", "bad_timeline")?;
    let file = capture_path(world, path)?;
    let (profiler, mut capture) = capture_of(world, params)?;
    if with_timeline.unwrap_or_else(|| profiler.timeline_enabled()) {
        let frames = bounded(params, "frames", DEFAULT_FRAMES, MAX_FRAMES, "bad_frames")?;
        capture = capture.with_timeline(&window_timeline(&profiler, frames), DEFAULT_MAX_TIMELINE_SPANS);
    }
    capture
        .write(&file)
        .map_err(|e| error(protocol::INTERNAL_ERROR, "F0001", "write_failed", e.to_string()))?;
    let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    Ok(json!({
        "path": path,
        "frames": capture.frames,
        "nodes": capture.node_count(),
        "truncated": capture.truncated,
        "gpu_passes": capture.gpu.len(),
        "timeline_spans": capture.timeline_span_count(),
        "timeline_truncated": capture.timeline_truncated,
        "bytes": bytes,
    }))
}

/// Default and most span rows of `profiler.compare`.
const DEFAULT_COMPARE_ROWS: u64 = 50;

/// Reads the capture named by parameter `key` of `profiler.compare`.
fn read_capture(world: &World, params: &Value, key: &str) -> Result<ProfileCapture, RpcError> {
    let rel = params.get(key).and_then(Value::as_str).ok_or_else(|| {
        invalid(
            "bad_path",
            format!("`{key}` (project-relative, .rprof.json) is required"),
        )
    })?;
    if regia_core::project_path::check_relative(rel).is_err()
        || !rel.to_ascii_lowercase().ends_with(PROFILE_CAPTURE_SUFFIX)
    {
        return Err(invalid(
            "bad_path",
            format!(
                "`{key}` must be relative to the project root, without `..`, ending in {PROFILE_CAPTURE_SUFFIX}"
            ),
        ));
    }
    let root = world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .ok_or_else(|| {
            missing(
                "no_project_root",
                "no AssetServer resource: no project root to read under",
            )
        })?;
    let file = crate::req_path::project_file_in(&root, key, rel)?;
    let text =
        std::fs::read_to_string(&file).map_err(|e| missing("bad_capture", format!("`{key}` {rel}: {e}")))?;
    ProfileCapture::from_json(&text).map_err(|e| invalid("bad_capture", format!("`{key}` {rel}: {e}")))
}

/// `profiler.compare { a, b, top? }`: per span the inclusive and self time of capture `a` and
/// capture `b` and `b - a`, largest change first (`regia_telemetry::profile_compare`).
pub fn compare(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let top = bounded(params, "top", DEFAULT_COMPARE_ROWS, MAX_NODES, "bad_top")?;
    let a = read_capture(world, params, "a")?;
    let b = read_capture(world, params, "b")?;
    let diff = regia_telemetry::profile_compare::compare(&a, &b);
    let total = diff.spans.len();
    let spans: Vec<_> = diff.spans.into_iter().take(top).collect();
    Ok(json!({
        "frame_ms_a": diff.frame_ms_a,
        "frame_ms_b": diff.frame_ms_b,
        "frame_ms_delta": diff.frame_ms_delta,
        "total_spans": total,
        "spans": spans,
        "gpu": diff.gpu,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn capture_path_refuses_a_symlink_that_leaves_the_project() {
        let base = regia_test_utils::unique_temp_dir("agent-profiler-symlink");
        let root = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("project");
        std::fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, root.join("out")).expect("directory symlink");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_asset::AssetPlugin {
            settings: regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
        let world = app.world_mut();
        world.insert_resource(regia_telemetry::SpanProfiler(FrameProfiler::new()));
        for path in ["out/a.rprof.json", "out/new/deep/a.rprof.json"] {
            let err = capture(world, &json!({ "path": path })).expect_err("outside project");
            assert_eq!(reason(&err), "path_outside_project");
        }
        assert!(!outside.join("a.rprof.json").exists());
        assert!(!outside.join("new").exists());
        let target = outside.join("existing.rprof.json");
        std::fs::write(&target, b"untouched").expect("existing target");
        std::os::unix::fs::symlink(&target, root.join("file.rprof.json")).expect("file symlink");
        let err = capture(world, &json!({ "path": "file.rprof.json" })).expect_err("outside file");
        assert_eq!(reason(&err), "path_outside_project");
        assert_eq!(std::fs::read(&target).expect("unchanged file"), b"untouched");
        std::fs::create_dir(root.join("captures")).expect("inside directory");
        std::os::unix::fs::symlink(root.join("captures"), root.join("inside")).expect("inside symlink");
        capture(world, &json!({ "path": "inside/ok.rprof.json" })).expect("inside project");
        assert!(root.join("captures/ok.rprof.json").is_file());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn world_with_profiler() -> (World, FrameProfiler) {
        let mut world = World::new();
        let profiler = FrameProfiler::new();
        world.insert_resource(regia_telemetry::SpanProfiler(profiler.clone()));
        (world, profiler)
    }

    #[test]
    fn capture_toggles_and_frame_reports_the_tree() {
        let (mut world, profiler) = world_with_profiler();
        assert!(!profiler.flame_enabled());
        let on = set_capture(&mut world, &json!({ "flame": true })).expect("on");
        assert_eq!(on["flame"], true);
        assert!(profiler.flame_enabled(), "the shared profiler changed");
        // Two frames with one span tree each, recorded through the profiler's layer.
        let subscriber =
            tracing_subscriber::layer::SubscriberExt::with(tracing_subscriber::registry(), profiler.clone());
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..2 {
                let outer = tracing::info_span!("agent_outer").entered();
                {
                    let _inner = tracing::info_span!("agent_inner").entered();
                }
                drop(outer);
                profiler.end_frame();
            }
        });
        let report = frame(&mut world, &json!({ "frames": 2 })).expect("frame");
        assert_eq!(report["flame_enabled"], true);
        assert_eq!(report["frames"], 2);
        let cpu = report["cpu"].as_array().cloned().unwrap_or_default();
        let outer = cpu
            .iter()
            .find(|n| n["name"] == "agent_outer")
            .expect("outer root");
        assert_eq!(outer["children"][0]["name"], "agent_inner");
        assert_eq!(outer["calls"], 1.0);
        assert!(report["top"].as_array().is_some_and(|t| !t.is_empty()));
        assert_eq!(report["gpu"], json!([]), "no renderer, no GPU passes");
        // Depth 1 cuts the children and counts them.
        let shallow = frame(&mut world, &json!({ "frames": 2, "depth": 1 })).expect("shallow");
        assert!(shallow["truncated"].as_u64().is_some_and(|n| n >= 1));
        let off = set_capture(&mut world, &json!({ "flame": false })).expect("off");
        assert_eq!(off["flame"], false);
    }

    /// `profiler.frame { since_frame }` lists every retained frame from the cursor on (a poller
    /// sees all 100 frames that ended between two polls, not one sample), each with its wall time
    /// and costliest spans; `newest_frame` is the next cursor minus one (w11-profiler-finish).
    #[test]
    fn since_frame_lists_every_frame_from_the_cursor() {
        let (mut world, profiler) = world_with_profiler();
        let subscriber =
            tracing_subscriber::layer::SubscriberExt::with(tracing_subscriber::registry(), profiler.clone());
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..100 {
                {
                    let _work = tracing::info_span!("since_frame_work").entered();
                }
                profiler.end_frame();
            }
        });
        let all = frame(&mut world, &json!({ "frames": 1, "since_frame": 0 })).expect("all");
        let rows = all["frame_times"].as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 100, "{all:#}");
        assert_eq!(rows[0]["number"], 0);
        assert_eq!(rows[99]["number"], 99);
        assert_eq!(all["newest_frame"], 99);
        assert!(rows.iter().all(|r| r["ms"].as_f64().is_some()));
        assert!(
            rows.iter().all(|r| r["spans"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["name"] == "since_frame_work"))),
            "each row carries the frame's own spans"
        );
        let rest = frame(&mut world, &json!({ "since_frame": 40 })).expect("rest");
        let numbers: Vec<u64> = rest["frame_times"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["number"].as_u64())
            .collect();
        assert_eq!(numbers, (40..100).collect::<Vec<u64>>());
        let none = frame(&mut world, &json!({ "since_frame": 100 })).expect("caught up");
        assert_eq!(none["frame_times"], json!([]));
        assert_eq!(
            none["newest_frame"], 99,
            "a caught-up cursor still learns the newest"
        );
        let plain = frame(&mut world, &json!({})).expect("plain");
        assert!(
            plain.get("frame_times").is_none(),
            "no cursor, no rows: {plain:#}"
        );
        for (params, why) in [
            (json!({ "since_frame": "x" }), "bad_since_frame"),
            (json!({ "since_frame": -1 }), "bad_since_frame"),
            (json!({ "entities": 0 }), "bad_entities"),
            (json!({ "entities": MAX_ENTITY_COMPONENTS + 1 }), "bad_entities"),
        ] {
            assert_eq!(
                reason(&frame(&mut world, &params).expect_err("bad")),
                why,
                "{params}"
            );
        }
    }

    /// `profiler.frame { entities }` ranks the component types by the entities that hold them,
    /// the same counts the editor's in-process Entities view shows (w11-profiler-finish).
    #[test]
    fn entities_rank_component_types_by_entity_count() {
        #[derive(Component)]
        struct Wide;
        #[derive(Component)]
        struct Narrow;
        let (mut world, _) = world_with_profiler();
        for i in 0..7 {
            if i < 2 {
                world.spawn((Wide, Narrow));
            } else {
                world.spawn(Wide);
            }
        }
        let answer = frame(&mut world, &json!({ "entities": 200 })).expect("entities");
        let counts = &answer["entities"];
        assert_eq!(
            counts["entities"].as_u64(),
            Some(u64::from(world.entities().count_spawned()))
        );
        assert_eq!(
            counts["archetypes"].as_u64(),
            Some(world.archetypes().len() as u64)
        );
        let rows: Vec<(String, u64)> = counts["components"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| Some((r["name"].as_str()?.to_owned(), r["entities"].as_u64()?)))
            .collect();
        let of = |suffix: &str| rows.iter().find(|(n, _)| n.ends_with(suffix)).map(|(_, k)| *k);
        assert_eq!(of("::Wide"), Some(7), "{rows:?}");
        assert_eq!(of("::Narrow"), Some(2), "{rows:?}");
        let wide = rows.iter().position(|(n, _)| n.ends_with("::Wide"));
        let narrow = rows.iter().position(|(n, _)| n.ends_with("::Narrow"));
        assert!(wide < narrow, "most entities first: {rows:?}");
        let one = frame(&mut world, &json!({ "entities": 1 })).expect("top 1");
        assert_eq!(one["entities"]["components"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn capture_writes_a_file_under_the_project() {
        let root = regia_test_utils::unique_temp_dir("agent-profiler");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_asset::AssetPlugin {
            settings: regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
        let world = app.world_mut();
        world.insert_resource(regia_telemetry::SpanProfiler(FrameProfiler::new()));
        let written =
            capture(world, &json!({ "path": "captures/one.rprof.json", "frames": 1 })).expect("capture");
        assert_eq!(written["path"], "captures/one.rprof.json");
        let text = std::fs::read_to_string(root.join("captures/one.rprof.json")).expect("written");
        let parsed = ProfileCapture::from_json(&text).expect("a capture");
        assert_eq!(parsed.frames, 0, "no frames recorded yet");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn errors_carry_reasons() {
        let mut bare = World::new();
        let err = frame(&mut bare, &json!({})).expect_err("no profiler");
        assert_eq!(reason(&err), "no_profiler");
        let (mut world, _) = world_with_profiler();
        for (params, why) in [
            (json!({}), "bad_flame"),
            (json!({ "flame": "yes" }), "bad_flame"),
            (json!({ "timeline": 1 }), "bad_timeline"),
        ] {
            assert_eq!(
                reason(&set_capture(&mut world, &params).expect_err("bad")),
                why,
                "{params}"
            );
        }
        for (params, why) in [
            (json!({ "frames": 0 }), "bad_frames"),
            (json!({ "frames": MAX_FRAMES + 1 }), "bad_frames"),
            (json!({ "depth": 0 }), "bad_depth"),
            (json!({ "max_nodes": "many" }), "bad_max_nodes"),
        ] {
            assert_eq!(
                reason(&frame(&mut world, &params).expect_err("bad")),
                why,
                "{params}"
            );
        }
        for (params, why) in [
            (json!({}), "bad_path"),
            (json!({ "path": "../out.rprof.json" }), "bad_path"),
            (json!({ "path": "/tmp/out.rprof.json" }), "bad_path"),
            (json!({ "path": "out.json" }), "bad_path"),
            (json!({ "path": "out.rprof.json" }), "no_project_root"),
        ] {
            assert_eq!(
                reason(&capture(&mut world, &params).expect_err("bad")),
                why,
                "{params}"
            );
        }
    }

    /// A directory symlink that leaves the project is refused before the capture is written.
    #[cfg(unix)]
    #[test]
    fn capture_refuses_a_symlink_out_of_the_project() {
        use std::os::unix::fs::symlink;

        let root = regia_test_utils::unique_temp_dir("agent-profiler-link");
        let outside = regia_test_utils::unique_temp_dir("agent-profiler-out");
        symlink(&outside, root.join("captures")).expect("symlink");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_asset::AssetPlugin {
            settings: regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
        let world = app.world_mut();
        world.insert_resource(regia_telemetry::SpanProfiler(FrameProfiler::new()));
        let err = capture(world, &json!({ "path": "captures/one.rprof.json", "frames": 1 }))
            .expect_err("symlink out");
        assert_eq!(reason(&err), "path_outside_project");
        assert!(
            !outside.join("one.rprof.json").exists(),
            "the capture was not written outside the project"
        );
        let written = capture(world, &json!({ "path": "ok.rprof.json", "frames": 1 })).expect("inside");
        assert_eq!(written["path"], "ok.rprof.json");
        assert!(root.join("ok.rprof.json").is_file());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// ADR-9740: two fixture captures under the project compare to the expected rows, twice to
    /// the same bytes; bad inputs carry their reasons.
    #[test]
    fn compare_reads_two_captures_and_reports_the_deltas() {
        use regia_telemetry::profile_capture::CaptureNode;
        let root = regia_test_utils::unique_temp_dir("agent-profiler-compare");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_asset::AssetPlugin {
            settings: regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
        let node = |name: &str, ms: f64| CaptureNode {
            name: name.into(),
            inclusive_ms: ms,
            self_ms: ms,
            calls: 1.0,
            children: Vec::new(),
        };
        let capture = |frame_ms: f64, cpu: Vec<CaptureNode>| ProfileCapture {
            format_version: 1,
            frames: 60,
            frame_ms,
            cpu,
            truncated: 0,
            gpu: Vec::new(),
            folded: String::new(),
            timeline: Vec::new(),
            timeline_truncated: 0,
        };
        capture(4.0, vec![node("update", 1.0), node("render", 3.0)])
            .write(&root.join("captures/a.rprof.json"))
            .expect("a");
        capture(5.5, vec![node("render", 4.5), node("audio", 0.25)])
            .write(&root.join("captures/b.rprof.json"))
            .expect("b");
        let world = app.world_mut();
        let params = json!({ "a": "captures/a.rprof.json", "b": "captures/b.rprof.json" });
        let diff = compare(world, &params).expect("compare");
        assert_eq!(diff["frame_ms_delta"], 1.5);
        assert_eq!(diff["total_spans"], 3);
        let rows: Vec<(String, f64)> = diff["spans"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        (
                            r["path"].to_string(),
                            r["inclusive_delta"].as_f64().unwrap_or(f64::NAN),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            rows,
            vec![
                (r#"["render"]"#.to_owned(), 1.5),
                (r#"["update"]"#.to_owned(), -1.0),
                (r#"["audio"]"#.to_owned(), 0.25),
            ]
        );
        assert_eq!(diff["spans"][1]["presence"], "only_a");
        let again = compare(world, &params).expect("again");
        assert_eq!(again.to_string(), diff.to_string(), "byte-identical");
        let one = compare(
            world,
            &json!({ "a": "captures/a.rprof.json", "b": "captures/b.rprof.json", "top": 1 }),
        )
        .expect("top 1");
        assert_eq!(one["spans"].as_array().map(Vec::len), Some(1));
        assert_eq!(one["total_spans"], 3);
        std::fs::write(root.join("captures/bad.rprof.json"), "not json").expect("bad file");
        for (params, why) in [
            (json!({ "b": "captures/b.rprof.json" }), "bad_path"),
            (
                json!({ "a": "../a.rprof.json", "b": "captures/b.rprof.json" }),
                "bad_path",
            ),
            (
                json!({ "a": "captures/a.json", "b": "captures/b.rprof.json" }),
                "bad_path",
            ),
            (
                json!({ "a": "captures/bad.rprof.json", "b": "captures/b.rprof.json" }),
                "bad_capture",
            ),
            (
                json!({ "a": "captures/a.rprof.json", "b": "captures/b.rprof.json", "top": 0 }),
                "bad_top",
            ),
        ] {
            assert_eq!(
                reason(&compare(world, &params).expect_err("bad")),
                why,
                "{params}"
            );
        }
        let mut bare = World::new();
        assert_eq!(
            reason(&compare(&mut bare, &params).expect_err("no root")),
            "no_project_root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-7325: `profiler.timeline` reads one row per thread with nested depths (a span on a
    /// spawned worker and two on this thread), filters threads, cuts at `max_spans`, refuses a
    /// frame that left the history, and answers `timeline_off` while the timeline is off; a capture
    /// then carries the window's timeline.
    #[test]
    fn timeline_reads_threads_and_depths_and_refuses_when_off() {
        let (mut world, profiler) = world_with_profiler();
        let off = timeline(&mut world, &json!({})).expect_err("off");
        assert_eq!(reason(&off), "timeline_off");
        let on = set_capture(&mut world, &json!({ "timeline": true })).expect("on");
        assert_eq!(
            (on["timeline"].clone(), on["flame"].clone()),
            (json!(true), json!(false))
        );
        let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::registry(),
            profiler.clone(),
        ));
        tracing::dispatcher::with_default(&dispatch, || {
            let outer = tracing::info_span!("agent_tl_outer").entered();
            {
                let _inner = tracing::info_span!("agent_tl_inner").entered();
            }
            let d = dispatch.clone();
            std::thread::Builder::new()
                .name("agent-tl-worker".into())
                .spawn(move || {
                    tracing::dispatcher::with_default(&d, || {
                        let _w = tracing::info_span!("agent_tl_worker").entered();
                    });
                })
                .and_then(|h| h.join().map_err(|_| std::io::Error::other("panicked")))
                .expect("worker");
            drop(outer);
            profiler.end_frame();
        });
        let answer = timeline(&mut world, &json!({})).expect("timeline");
        let threads = answer["threads"].as_array().cloned().unwrap_or_default();
        assert_eq!(threads.len(), 2, "{answer:#}");
        let worker = threads
            .iter()
            .find(|t| t["name"] == "agent-tl-worker")
            .expect("worker row");
        assert_eq!(worker["spans"][0]["name"], "agent_tl_worker");
        let main = threads
            .iter()
            .find(|t| t["name"] != "agent-tl-worker")
            .expect("main row");
        assert_eq!(main["spans"][0]["name"], "agent_tl_outer");
        assert_eq!(main["spans"][1]["depth"], 1);
        assert_eq!(answer["frame"], 0);
        let only = timeline(&mut world, &json!({ "threads": ["agent-tl-worker"] })).expect("filter");
        assert_eq!(only["threads"].as_array().map(Vec::len), Some(1));
        let cut = timeline(&mut world, &json!({ "max_spans": 1 })).expect("cut");
        assert_eq!(
            (cut["spans"].clone(), cut["truncated"].clone()),
            (json!(1), json!(2))
        );
        for (params, why) in [
            (json!({ "frame": 9 }), "bad_frame"),
            (json!({ "frame": "newest" }), "bad_frame"),
            (json!({ "threads": "main" }), "bad_threads"),
            (json!({ "max_spans": 0 }), "bad_max_spans"),
        ] {
            assert_eq!(
                reason(&timeline(&mut world, &params).expect_err("bad")),
                why,
                "{params}"
            );
        }
        let window = window_timeline(&profiler, 60);
        assert_eq!(window.len(), 1);
        let capture = ProfileCapture::from_flame(&profiler.flame_graph(1), &[], 8, 100)
            .with_timeline(&window, DEFAULT_MAX_TIMELINE_SPANS);
        assert_eq!(capture.timeline_span_count(), 3);
    }

    /// Review g16 finding 1: with the timeline on and no frame ended yet, `profiler.timeline`
    /// answers `timeline_off` ("no frame has been recorded yet") instead of slicing an empty
    /// history; an explicit `frame` on the empty history still answers `bad_frame`.
    #[test]
    fn timeline_on_with_no_frame_answers_timeline_off() {
        let (mut world, profiler) = world_with_profiler();
        set_capture(&mut world, &json!({ "timeline": true })).expect("on");
        assert!(profiler.timeline_enabled());
        assert!(profiler.frame_numbers().is_empty(), "no frame has ended");
        for params in [json!({}), json!({ "frames": 4 }), json!({ "frame": null })] {
            let err = timeline(&mut world, &params).expect_err("no frame yet");
            assert_eq!(reason(&err), "timeline_off", "{params}");
        }
        let err = timeline(&mut world, &json!({ "frame": 3 })).expect_err("bad frame");
        assert_eq!(reason(&err), "bad_frame");
    }

    /// Spec 83 § 6: the agent page names every `profiler.*` method and every error reason this
    /// module can return (the list in the module docs).
    #[test]
    fn the_book_page_names_every_method_and_reason() {
        let book = include_str!("../../../book/agents/profiler.md");
        let module = include_str!("profiler.rs");
        let reasons = module
            .lines()
            .find_map(|line| line.strip_prefix("//! Error reasons (`data.reason`): "))
            .expect("the module docs list the reasons");
        let mut reasons: Vec<&str> = reasons
            .split(',')
            .map(|r| r.trim().trim_end_matches('.').trim_matches('`'))
            .filter(|r| !r.is_empty())
            .collect();
        // The list wraps onto the next doc line.
        let tail = module
            .lines()
            .skip_while(|line| !line.starts_with("//! Error reasons"))
            .nth(1)
            .and_then(|line| line.strip_prefix("//! "))
            .unwrap_or_default();
        reasons.extend(
            tail.split(',')
                .map(|r| r.trim().trim_end_matches('.').trim_matches('`'))
                .filter(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_lowercase() || c == '_')),
        );
        for reason in [
            "no_profiler",
            "no_project_root",
            "write_failed",
            "path_outside_project",
        ] {
            assert!(
                reasons.contains(&reason),
                "{reason} is in the module's list: {reasons:?}"
            );
        }
        for name in reasons.iter().copied().chain([
            "profiler.set_capture",
            "profiler.frame",
            "profiler.capture",
            "profiler.compare",
            "profiler.timeline",
        ]) {
            assert!(
                book.contains(&format!("`{name}`")),
                "book/agents/profiler.md lacks `{name}`"
            );
        }
    }
}
