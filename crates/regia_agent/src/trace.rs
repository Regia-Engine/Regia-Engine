// SPDX-License-Identifier: MPL-2.0
//! The trace recorder on the agent protocol (`book/agents/gpu-breadcrumbs-and-trace.md`,
//! `docs/specs/54-gpu-breadcrumbs-and-trace-markers.md`): `telemetry.mark`,
//! `telemetry.range_open`, `telemetry.range_close`, `telemetry.trace` and the deferred
//! `telemetry.screenshot_mark`. `AgentPlugin` installs them ([`install_trace_methods`], through
//! `crate::install_builtin_extensions`, ADR-2307).
//!
//! Error reasons (`data.reason`): `name_too_long`, `data_too_large`, `invalid_lane`,
//! `unknown_range`, `trace_busy`, `trace_not_writing`, `bad_file_name`, `trace_io`,
//! `no_renderer`, `screenshot_failed`, `invalid_value`, `unknown_field`.

use regia_app::prelude::*;
use regia_telemetry::trace::{self, Lane, RangeId, TraceError, TraceSummary};
use serde_json::{Value, json};

use crate::breadcrumbs::{error, invalid, not_found};
use crate::extension::{AgentExtension, DeferredReply};
use crate::protocol::{self, RpcError};
use crate::schema::{MethodSchema, ParamSchema, Permission};

/// Longest mark or range name, characters.
pub const MAX_NAME_CHARS: usize = 64;

fn mark_schema() -> MethodSchema {
    MethodSchema {
        name: "telemetry.mark",
        tool: "telemetry_mark",
        description: "Add a named instant to the trace recorder (and a regia::mark log event).",
        params: &[
            ParamSchema {
                name: "name",
                kind: "string",
                required: true,
                description: "1-64 characters",
            },
            ParamSchema {
                name: "data",
                kind: "object",
                required: false,
                description: "extra JSON, at most 1 KiB",
            },
        ],
        result: "{ t_ns, frame, recorded }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn range_open_schema() -> MethodSchema {
    MethodSchema {
        name: "telemetry.range_open",
        tool: "telemetry_range_open",
        description: "Open a named range on a CPU lane of the trace recorder; close it with telemetry.range_close.",
        params: &[
            ParamSchema {
                name: "name",
                kind: "string",
                required: true,
                description: "1-64 characters",
            },
            ParamSchema {
                name: "lane",
                kind: "string",
                required: false,
                description: "`main` (default), `render` or `worker:<n>`",
            },
        ],
        result: "{ id, depth, recorded }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn range_close_schema() -> MethodSchema {
    MethodSchema {
        name: "telemetry.range_close",
        tool: "telemetry_range_close",
        description: "Close a range; inner ranges still open close first and are reported as forced.",
        params: &[ParamSchema {
            name: "id",
            kind: "integer",
            required: true,
            description: "the id telemetry.range_open returned",
        }],
        result: "{ duration_ns, forced_close: [id] }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn trace_schema() -> MethodSchema {
    MethodSchema {
        name: "telemetry.trace",
        tool: "telemetry_trace",
        description: "Run the trace recorder: start or stop a .regiatrace file, save the rolling window, or read the status.",
        params: &[
            ParamSchema {
                name: "action",
                kind: "string",
                required: true,
                description: "`start`, `stop`, `save_window` or `status`",
            },
            ParamSchema {
                name: "streams",
                kind: "array",
                required: false,
                description: "for start: any of `frames`, `passes`, `counters`, `logs` (`marks` is always on); default marks, frames, passes",
            },
            ParamSchema {
                name: "window_seconds",
                kind: "number",
                required: false,
                description: "for start: resize the rolling window (0 turns it off)",
            },
            ParamSchema {
                name: "file",
                kind: "string",
                required: false,
                description: "for start and save_window: a bare file name in the profiling folder",
            },
        ],
        result: "{ state, file?, bytes, events, dropped, ranges_open, degraded }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn screenshot_mark_schema() -> MethodSchema {
    MethodSchema {
        name: "telemetry.screenshot_mark",
        tool: "telemetry_screenshot_mark",
        description: "While a trace file is being written: capture the window, write the PNG next to the trace and record a screenshot mark that references it.",
        params: &[
            ParamSchema {
                name: "name",
                kind: "string",
                required: false,
                description: "1-64 characters",
            },
            ParamSchema {
                name: "size",
                kind: "array",
                required: false,
                description: "[width, height] to scale the shot to (1-4096 each; default the window size)",
            },
        ],
        result: "{ file, frame, t_ns, recorded, width, height, mime, png_base64 }",
        mutating: true,
        permission: Permission::Play,
    }
}

/// Registers the `telemetry.mark`, `telemetry.range_open`, `telemetry.range_close`,
/// `telemetry.trace` and `telemetry.screenshot_mark` methods on the world's
/// [`AgentExtension`]. `AgentPlugin` calls it (ADR-2307); calling it again replaces the rows with
/// the same ones.
pub fn install_trace_methods(world: &mut World) {
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    extension.register(mark_schema(), telemetry_mark);
    extension.register(range_open_schema(), range_open);
    extension.register(range_close_schema(), range_close);
    extension.register(trace_schema(), telemetry_trace);
    extension.register_deferred(screenshot_mark_schema(), screenshot_mark);
    world.insert_resource(extension);
}

/// Most pixels per side of a screenshot mark.
pub const MAX_SHOT_SIDE: u32 = 4096;

fn size_param(params: &Value) -> Result<Option<(u32, u32)>, RpcError> {
    let bad = || invalid("invalid_value", "`size` must be [width, height], each 1-4096");
    match params.get("size") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) if a.len() == 2 => {
            let side = |v: &Value| {
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| (1..=MAX_SHOT_SIDE).contains(n))
                    .ok_or_else(bad)
            };
            Ok(Some((side(&a[0])?, side(&a[1])?)))
        }
        Some(_) => Err(bad()),
    }
}

/// `telemetry.screenshot_mark`: checks the call, then asks the renderer for the next window
/// image; the answer comes from [`write_screenshot_mark`] when the capture is done.
fn screenshot_mark(world: &mut World, params: &Value, reply: DeferredReply) -> Result<(), RpcError> {
    check_fields(params, &["name", "size"])?;
    let name = match params.get("name") {
        None | Some(Value::Null) => None,
        Some(_) => Some(name_param(params)?.to_owned()),
    };
    let size = size_param(params)?;
    if trace::recorder().state() != trace::RecorderState::Writing {
        return Err(trace_error(&TraceError::NotWriting));
    }
    let window = {
        let mut q = world.query_filtered::<Entity, With<regia_platform::PrimaryWindow>>();
        q.iter(world).next()
    };
    let Some(mut requests) = world.get_resource_mut::<regia_render::ScreenshotRequests>() else {
        return Err(not_found(
            "no_renderer",
            "this app has no renderer to capture (add regia_render::RenderPlugin)",
        ));
    };
    requests.request_with(
        window,
        regia_render::ScreenshotOptions { neutral: false },
        Box::new(move |result| {
            let _ = reply.send(write_screenshot_mark(result, name.as_deref(), size));
        }),
    );
    Ok(())
}

/// Nearest-neighbour scale of `image` to `w` x `h`.
fn scaled(image: &regia_render::CapturedImage, w: u32, h: u32) -> regia_render::CapturedImage {
    if (image.width, image.height) == (w, h) || image.width == 0 || image.height == 0 {
        return regia_render::CapturedImage {
            width: image.width,
            height: image.height,
            rgba: image.rgba.clone(),
        };
    }
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        let sy = (u64::from(y) * u64::from(image.height) / u64::from(h)) as usize;
        for x in 0..w {
            let sx = (u64::from(x) * u64::from(image.width) / u64::from(w)) as usize;
            let at = (sy * image.width as usize + sx) * 4;
            rgba.extend_from_slice(image.rgba.get(at..at + 4).unwrap_or(&[0, 0, 0, 0]));
        }
    }
    regia_render::CapturedImage {
        width: w,
        height: h,
        rgba,
    }
}

/// Writes a captured image as the next `<trace>.shots/<n>.png` (atomically: a temporary file,
/// then a rename) and records the screenshot mark that references it. The frame is the recorder's
/// at the capture.
pub(crate) fn write_screenshot_mark(
    result: Result<regia_render::CapturedImage, String>,
    name: Option<&str>,
    size: Option<(u32, u32)>,
) -> Result<Value, RpcError> {
    let image = result.map_err(|e| {
        error(
            protocol::INTERNAL_ERROR,
            "F0001",
            "screenshot_failed",
            format!("the screenshot failed: {e}"),
        )
    })?;
    let image = match size {
        Some((w, h)) => scaled(&image, w, h),
        None => image,
    };
    let recorder = trace::recorder();
    let dir = recorder
        .shots_dir()
        .ok_or_else(|| trace_error(&TraceError::NotWriting))?;
    let io = |e: std::io::Error| {
        error(
            protocol::INTERNAL_ERROR,
            "F0001",
            "trace_io",
            format!("screenshot file: {e}"),
        )
    };
    let taken = std::fs::read_dir(&dir)
        .map_err(io)?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "png"));
    let n = taken.count() + 1;
    let path = dir.join(format!("{n}.png"));
    let png = crate::image::png_bytes(&image).map_err(|e| {
        error(
            protocol::INTERNAL_ERROR,
            "F0001",
            "screenshot_failed",
            format!("png: {e}"),
        )
    })?;
    let tmp = dir.join(format!(".{n}.png.tmp"));
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    crate::req_path::write_new_temp(&tmp, &png).map_err(io)?;
    std::fs::rename(&tmp, &path).map_err(io)?;
    let frame = recorder.frame();
    let m = recorder.screenshot_mark(name, frame, (image.width, image.height), &path);
    let file = dir
        .file_name()
        .map(|d| format!("{}/{n}.png", d.to_string_lossy()))
        .unwrap_or_else(|| format!("{n}.png"));
    Ok(json!({
        "file": file,
        "frame": m.frame,
        "t_ns": m.t_ns,
        "recorded": m.recorded,
        "width": image.width,
        "height": image.height,
        "mime": "image/png",
        "png_base64": crate::image::base64(&png),
    }))
}

fn check_fields(params: &Value, allowed: &[&str]) -> Result<(), RpcError> {
    match params {
        Value::Null => Ok(()),
        Value::Object(map) => match map.keys().find(|k| !allowed.contains(&k.as_str())) {
            Some(key) => Err(invalid(
                "unknown_field",
                format!("unknown field `{key}`; one of {}", allowed.join(", ")),
            )),
            None => Ok(()),
        },
        _ => Err(invalid("invalid_value", "params must be an object")),
    }
}

fn name_param(params: &Value) -> Result<&str, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("invalid_value", "`name` is required and must be a string"))?;
    let chars = name.chars().count();
    if chars == 0 {
        return Err(invalid("invalid_value", "`name` must not be empty"));
    }
    if chars > MAX_NAME_CHARS {
        return Err(invalid(
            "name_too_long",
            format!("`name` is {chars} characters; the limit is {MAX_NAME_CHARS}"),
        ));
    }
    Ok(name)
}

fn telemetry_mark(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_fields(params, &["name", "data"])?;
    let name = name_param(params)?;
    let data = match params.get("data") {
        None | Some(Value::Null) => None,
        Some(v @ Value::Object(_)) => {
            let size = serde_json::to_string(v).map_or(usize::MAX, |s| s.len());
            if size > trace::MAX_MARK_DATA_BYTES {
                return Err(invalid(
                    "data_too_large",
                    format!(
                        "`data` is {size} bytes; the limit is {}",
                        trace::MAX_MARK_DATA_BYTES
                    ),
                ));
            }
            Some(v)
        }
        Some(_) => return Err(invalid("invalid_value", "`data` must be an object")),
    };
    let m = trace::recorder().mark(name, data);
    Ok(json!({ "t_ns": m.t_ns, "frame": m.frame, "recorded": m.recorded }))
}

fn range_open(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_fields(params, &["name", "lane"])?;
    let name = name_param(params)?;
    let lane = match params.get("lane") {
        None | Some(Value::Null) => Lane::Main,
        Some(Value::String(s)) => match Lane::parse(s) {
            Some(Lane::Gpu) | None => {
                return Err(invalid(
                    "invalid_lane",
                    format!("lane `{s}` is not a CPU lane; use main, render or worker:<n>"),
                ));
            }
            Some(lane) => lane,
        },
        Some(_) => return Err(invalid("invalid_value", "`lane` must be a string")),
    };
    let o = trace::recorder().open_range(name, lane);
    Ok(json!({ "id": o.id.0, "depth": o.depth, "recorded": o.recorded }))
}

fn range_close(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_fields(params, &["id"])?;
    let id = params
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("invalid_value", "`id` is required and must be an integer"))?;
    match trace::recorder().close_range(RangeId(id)) {
        Ok(c) => Ok(json!({
            "duration_ns": c.duration_ns,
            "forced_close": c.forced_close.iter().map(|r| r.0).collect::<Vec<_>>(),
        })),
        Err(trace::UnknownRange) => Err(not_found(
            "unknown_range",
            format!("no open range with id {id} (already closed, or never opened)"),
        )),
    }
}

fn summary_json(s: &TraceSummary) -> Value {
    json!({
        "state": s.state.name(),
        "file": s.file.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "bytes": s.bytes,
        "events": s.events,
        "dropped": s.dropped,
        "ranges_open": s.ranges_open,
        "degraded": s.degraded,
    })
}

fn trace_error(e: &TraceError) -> RpcError {
    match e {
        TraceError::Busy | TraceError::NotWriting | TraceError::NoWindow | TraceError::BadFileName => {
            invalid(e.reason(), e.to_string())
        }
        TraceError::Io(_) => error(protocol::INTERNAL_ERROR, "F0001", e.reason(), e.to_string()),
    }
}

fn telemetry_trace(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_fields(params, &["action", "streams", "window_seconds", "file"])?;
    let action = params.get("action").and_then(Value::as_str).ok_or_else(|| {
        invalid(
            "invalid_value",
            "`action` is required: start, stop, save_window or status",
        )
    })?;
    let file = match params.get("file") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err(invalid("invalid_value", "`file` must be a string")),
    };
    let recorder = trace::recorder();
    let summary = match action {
        "status" => recorder.status(),
        "start" => {
            let streams = match params.get("streams") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => {
                    let names: Option<Vec<&str>> = a.iter().map(Value::as_str).collect();
                    let names = names.ok_or_else(|| invalid("invalid_value", "`streams` must be strings"))?;
                    Some(trace::Streams::from_names(names.iter().copied()).ok_or_else(|| {
                        invalid(
                            "invalid_value",
                            "unknown stream; one of marks, frames, passes, counters, logs",
                        )
                    })?)
                }
                Some(_) => return Err(invalid("invalid_value", "`streams` must be an array")),
            };
            if let Some(v) = params.get("window_seconds").filter(|v| !v.is_null()) {
                let secs = v.as_f64().filter(|s| (0.0..=600.0).contains(s)).ok_or_else(|| {
                    invalid("invalid_value", "`window_seconds` must be a number from 0 to 600")
                })?;
                if recorder.state() == trace::RecorderState::Writing {
                    return Err(trace_error(&TraceError::Busy));
                }
                recorder.set_window_seconds(secs as f32);
            }
            recorder.start(file, streams).map_err(|e| trace_error(&e))?
        }
        "stop" => recorder.stop().map_err(|e| trace_error(&e))?,
        "save_window" => recorder.save_window(file).map_err(|e| trace_error(&e))?,
        other => {
            return Err(invalid(
                "invalid_value",
                format!("unknown action `{other}`; one of start, stop, save_window, status"),
            ));
        }
    };
    Ok(summary_json(&summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_telemetry::trace::{RecorderState, TraceConfig};

    /// The methods drive the process recorder, so these tests take turns.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
        let handler = world
            .resource::<AgentExtension>()
            .method(name)
            .map(|(_, h)| h)
            .expect("registered");
        handler(world, &params)
    }

    fn reason(e: &RpcError) -> (String, String) {
        let d = e.data.as_ref().expect("data");
        (
            d["code"].as_str().unwrap_or("").to_owned(),
            d["reason"].as_str().unwrap_or("").to_owned(),
        )
    }

    fn fresh() -> (World, std::path::PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = regia_test_utils::unique_temp_dir("agent-trace");
        if trace::recorder().state() == RecorderState::Writing {
            let _ = trace::recorder().stop();
        }
        trace::configure(TraceConfig {
            window_seconds: 5.0,
            dir: dir.clone(),
            ..TraceConfig::default()
        });
        let mut w = World::new();
        install_trace_methods(&mut w);
        (w, dir, guard)
    }

    #[test]
    fn all_four_methods_are_registered_with_play_permission() {
        let (w, _dir, _g) = fresh();
        let ext = w.resource::<AgentExtension>();
        for name in [
            "telemetry.mark",
            "telemetry.range_open",
            "telemetry.range_close",
            "telemetry.trace",
        ] {
            let (schema, _) = ext.method(name).expect(name);
            assert_eq!(schema.permission.name(), "play", "{name}");
            assert!(schema.mutating);
        }
    }

    #[test]
    fn mark_run_record_read_back() {
        let (mut w, dir, _g) = fresh();
        let started = call(
            &mut w,
            "telemetry.trace",
            json!({ "action": "start", "file": "recipe" }),
        )
        .expect("start");
        assert_eq!(started["state"], "writing");
        let m = call(
            &mut w,
            "telemetry.mark",
            json!({ "name": "boss door", "data": { "room": 4 } }),
        )
        .expect("mark");
        assert_eq!(m["recorded"], true);
        let outer = call(&mut w, "telemetry.range_open", json!({ "name": "stress" })).expect("open");
        let inner = call(
            &mut w,
            "telemetry.range_open",
            json!({ "name": "inner", "lane": "worker:2" }),
        )
        .expect("open");
        assert_eq!(
            (outer["depth"].clone(), inner["depth"].clone()),
            (json!(0), json!(0))
        );
        let closed = call(&mut w, "telemetry.range_close", json!({ "id": outer["id"] })).expect("close");
        assert_eq!(closed["forced_close"], json!([]));
        let status = call(&mut w, "telemetry.trace", json!({ "action": "status" })).expect("status");
        assert_eq!(status["ranges_open"], 1);
        let stopped = call(&mut w, "telemetry.trace", json!({ "action": "stop" })).expect("stop");
        assert_eq!(stopped["ranges_open"], 0);
        assert!(stopped["events"].as_u64().unwrap_or(0) >= 5);
        let file = dir.join("recipe.regiatrace");
        let read = trace::read_trace(&file).expect("reads");
        assert!(
            read.events
                .iter()
                .any(|e| matches!(&e.kind, trace::EventKind::Mark { name, .. } if name == "boss door"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn range_open_while_idle_is_not_recorded() {
        let (mut w, dir, _g) = fresh();
        trace::recorder().set_window_seconds(0.0);
        assert_eq!(trace::recorder().state(), RecorderState::Idle);
        let opened = call(&mut w, "telemetry.range_open", json!({ "name": "load" })).expect("open");
        assert_eq!(opened["id"], 0);
        assert_eq!(opened["depth"], 0);
        assert_eq!(opened["recorded"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_error_reason() {
        let (mut w, dir, _g) = fresh();
        let long = "x".repeat(65);
        let cases = [
            (
                "telemetry.mark",
                json!({ "name": long }),
                "F0002",
                "name_too_long",
            ),
            ("telemetry.mark", json!({ "name": "" }), "F0002", "invalid_value"),
            ("telemetry.mark", json!({}), "F0002", "invalid_value"),
            (
                "telemetry.mark",
                json!({ "name": "a", "data": "s" }),
                "F0002",
                "invalid_value",
            ),
            (
                "telemetry.mark",
                json!({ "name": "a", "data": { "k": "y".repeat(2000) } }),
                "F0002",
                "data_too_large",
            ),
            (
                "telemetry.mark",
                json!({ "name": "a", "extra": 1 }),
                "F0002",
                "unknown_field",
            ),
            (
                "telemetry.range_open",
                json!({ "name": "a", "lane": "gpu" }),
                "F0002",
                "invalid_lane",
            ),
            (
                "telemetry.range_open",
                json!({ "name": "a", "lane": "nope" }),
                "F0002",
                "invalid_lane",
            ),
            (
                "telemetry.range_close",
                json!({ "id": 987_654_321 }),
                "F0003",
                "unknown_range",
            ),
            ("telemetry.range_close", json!({}), "F0002", "invalid_value"),
            (
                "telemetry.trace",
                json!({ "action": "stop" }),
                "F0002",
                "trace_not_writing",
            ),
            (
                "telemetry.trace",
                json!({ "action": "start", "file": "../x" }),
                "F0002",
                "bad_file_name",
            ),
            (
                "telemetry.trace",
                json!({ "action": "start", "streams": ["bogus"] }),
                "F0002",
                "invalid_value",
            ),
            (
                "telemetry.trace",
                json!({ "action": "dance" }),
                "F0002",
                "invalid_value",
            ),
            ("telemetry.trace", json!({}), "F0002", "invalid_value"),
        ];
        for (method, params, code, why) in cases {
            let err = call(&mut w, method, params.clone()).expect_err("rejected");
            assert_eq!(
                reason(&err),
                (code.to_owned(), why.to_owned()),
                "{method} {params}"
            );
        }
        call(
            &mut w,
            "telemetry.trace",
            json!({ "action": "start", "file": "busy" }),
        )
        .expect("start");
        let err = call(&mut w, "telemetry.trace", json!({ "action": "start" })).expect_err("busy");
        assert_eq!(reason(&err).1, "trace_busy");
        call(&mut w, "telemetry.trace", json!({ "action": "stop" })).expect("stop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_window_leaves_the_state_alone() {
        let (mut w, dir, _g) = fresh();
        call(&mut w, "telemetry.mark", json!({ "name": "in the window" })).expect("mark");
        let saved = call(
            &mut w,
            "telemetry.trace",
            json!({ "action": "save_window", "file": "win" }),
        )
        .expect("save");
        assert_eq!(saved["state"], "windowed");
        assert!(dir.join("win.regiatrace").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn deferred(
        world: &mut World,
        name: &str,
        params: Value,
    ) -> Result<crossbeam_channel::Receiver<crate::protocol::Response>, RpcError> {
        let handler = world
            .resource::<AgentExtension>()
            .deferred_method(name)
            .expect("deferred method registered");
        let (tx, rx) = crossbeam_channel::unbounded();
        handler(world, &params, DeferredReply::new(json!(1), tx))?;
        Ok(rx)
    }

    fn image(w: u32, h: u32) -> regia_render::CapturedImage {
        let mut rgba = Vec::new();
        for i in 0..w * h {
            rgba.extend_from_slice(&[(i % 256) as u8, 7, 9, 255]);
        }
        regia_render::CapturedImage {
            width: w,
            height: h,
            rgba,
        }
    }

    /// Spec 54 test 16 (headless part): the PNG sits next to the trace in `<trace>.shots/`, the
    /// mark carries the frame and the relative path, and a second shot gets the next number.
    #[test]
    fn screenshot_mark_writes_the_png_next_to_the_trace_and_references_it() {
        let (mut w, dir, _g) = fresh();
        let (schema, deferred_is_registered) = {
            let ext = w.resource::<AgentExtension>();
            (
                ext.method_schema("telemetry.screenshot_mark")
                    .map(|s| (s.permission.name(), s.mutating)),
                ext.deferred_method("telemetry.screenshot_mark").is_some(),
            )
        };
        assert_eq!(schema, Some(("play", true)));
        assert!(deferred_is_registered);
        call(
            &mut w,
            "telemetry.trace",
            json!({ "action": "start", "file": "shots" }),
        )
        .expect("start");
        trace::recorder().set_frame(42);
        let first = write_screenshot_mark(Ok(image(8, 4)), Some("door"), None).expect("first");
        assert_eq!(first["file"], "shots.regiatrace.shots/1.png");
        assert_eq!(first["frame"], 42);
        assert_eq!(
            (first["width"].clone(), first["height"].clone()),
            (json!(8), json!(4))
        );
        assert_eq!(first["recorded"], true);
        assert!(first["png_base64"].as_str().is_some_and(|b| !b.is_empty()));
        let second = write_screenshot_mark(Ok(image(8, 4)), None, Some((4, 2))).expect("second");
        assert_eq!(second["file"], "shots.regiatrace.shots/2.png");
        assert_eq!(
            (second["width"].clone(), second["height"].clone()),
            (json!(4), json!(2))
        );
        let png = dir.join("shots.regiatrace.shots/1.png");
        assert!(
            std::fs::read(&png).is_ok_and(|b| b.starts_with(b"\x89PNG")),
            "{}",
            png.display()
        );
        call(&mut w, "telemetry.trace", json!({ "action": "stop" })).expect("stop");
        let read = trace::read_trace(&dir.join("shots.regiatrace")).expect("reads");
        let shots: Vec<(Option<String>, u64, String)> = read
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                trace::EventKind::Screenshot {
                    name, frame, file, ..
                } => Some((name.clone(), *frame, file.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            shots,
            vec![
                (
                    Some("door".to_owned()),
                    42,
                    "shots.regiatrace.shots/1.png".to_owned()
                ),
                (None, 42, "shots.regiatrace.shots/2.png".to_owned()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn screenshot_mark_reasons() {
        let (mut w, dir, _g) = fresh();
        let rejected = |w: &mut World, params: Value| match deferred(w, "telemetry.screenshot_mark", params) {
            Ok(_) => panic!("accepted"),
            Err(e) => reason(&e),
        };
        // Not writing a file: nothing to put the shot next to.
        assert_eq!(
            rejected(&mut w, json!({})),
            ("F0002".to_owned(), "trace_not_writing".to_owned())
        );
        assert_eq!(
            rejected(&mut w, json!({ "size": [0, 4] })),
            ("F0002".to_owned(), "invalid_value".to_owned())
        );
        assert_eq!(
            rejected(&mut w, json!({ "size": [4] })),
            ("F0002".to_owned(), "invalid_value".to_owned())
        );
        assert_eq!(
            rejected(&mut w, json!({ "name": "x".repeat(65) })),
            ("F0002".to_owned(), "name_too_long".to_owned())
        );
        assert_eq!(
            rejected(&mut w, json!({ "zoom": 2 })),
            ("F0002".to_owned(), "unknown_field".to_owned())
        );
        call(
            &mut w,
            "telemetry.trace",
            json!({ "action": "start", "file": "noshot" }),
        )
        .expect("start");
        // Writing, but this world has no renderer to capture.
        assert_eq!(
            rejected(&mut w, json!({})),
            ("F0003".to_owned(), "no_renderer".to_owned())
        );
        let failed = write_screenshot_mark(Err("lost".to_owned()), None, None).expect_err("failed");
        assert_eq!(
            reason(&failed),
            ("F0001".to_owned(), "screenshot_failed".to_owned())
        );
        call(&mut w, "telemetry.trace", json!({ "action": "stop" })).expect("stop");
        let late = write_screenshot_mark(Ok(image(2, 2)), None, None).expect_err("stopped");
        assert_eq!(reason(&late).1, "trace_not_writing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The writer thread (ADR-2307): a file being written is flushed through the writer, and a
    /// file that the recorder still writes reads back up to its last complete chunk.
    #[test]
    fn a_running_file_reads_back_through_the_writer_thread() {
        let (mut w, dir, _g) = fresh();
        call(
            &mut w,
            "telemetry.trace",
            json!({ "action": "start", "file": "live" }),
        )
        .expect("start");
        for n in 0..50 {
            call(
                &mut w,
                "telemetry.mark",
                json!({ "name": format!("writer-thread-{n}") }),
            )
            .expect("mark");
        }
        trace::recorder().flush().expect("flush");
        let stats = trace::recorder().writer_stats().expect("writing");
        assert!(stats.batches >= 1, "{stats:?}");
        let stopped = call(&mut w, "telemetry.trace", json!({ "action": "stop" })).expect("stop");
        let file = dir.join("live.regiatrace");
        let on_disk = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            stopped["bytes"].as_u64(),
            Some(on_disk),
            "stop waits for the writer to drain"
        );
        let read = trace::read_trace(&file).expect("reads");
        // Only this test's marks: the recorder is the process's, and other tests in this binary
        // (the method-coverage sweep) call `telemetry.mark` without taking `TURN`, so a parallel
        // run could add theirs to this file (seen: 52 marks).
        let marks = read
            .events
            .iter()
            .filter(|e| matches!(&e.kind, trace::EventKind::Mark { name, .. } if name.starts_with("writer-thread-")))
            .count();
        assert_eq!(marks, 50);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
