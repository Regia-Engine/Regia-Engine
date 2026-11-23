// SPDX-License-Identifier: MPL-2.0
//! Agent methods of the Diagnostics window (ADR-2771, `book/agents/editor-diagnostics.md`):
//! `editor.diagnostics` (the GPU tab), `editor.trace_timeline` (open a `.regiatrace`, zoom, pan,
//! read the layout) and `editor.trace_record` (the record button). They drive the same state and
//! functions the window paints, and call the registered `render.breadcrumbs` and
//! `telemetry.trace` methods rather than reading the recorder a second way.

use std::path::Path;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::diagnostics_panel::{DiagnosticsPanel, DiagnosticsTab, load_last_crash, state_json};
use crate::trace_timeline::{TraceTimeline, record, trace_lanes, trace_layout};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// Most layout items one answer lists.
pub const MAX_ITEMS: usize = 500;

fn bad(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn number(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .map(|n| Some(n as f32))
            .ok_or_else(|| {
                bad(
                    "bad_view",
                    format!("`{key}` is a number"),
                    "zoom 1-10000, pan 0-1",
                )
            }),
    }
}

/// An optional boolean parameter. A value of another type is `bad_param`: it used to be ignored
/// and the call answered ok as if the flag had been left out.
fn flag(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(bad(
            "bad_param",
            format!("`{key}` is a boolean"),
            "see book/agents/editor-diagnostics.md",
        )),
    }
}

/// `editor.diagnostics { open?, tab?, load_crash? }`.
fn diagnostics(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world
        .get_resource::<DiagnosticsPanel>()
        .cloned()
        .unwrap_or_default();
    let open = flag(params, "open")?;
    let load_crash = flag(params, "load_crash")?;
    if let Some(open) = open {
        panel.open = open;
    }
    if let Some(tab) = params.get("tab").filter(|v| !v.is_null()) {
        panel.tab = tab.as_str().and_then(DiagnosticsTab::parse).ok_or_else(|| {
            bad(
                "bad_tab",
                "`tab` is \"gpu\" or \"timeline\"",
                "see book/agents/editor-diagnostics.md",
            )
        })?;
    }
    let mut crash_error = None;
    if load_crash == Some(true) {
        match load_last_crash(world) {
            Ok(view) => {
                panel.status = format!("loaded {}", view.source);
                panel.crash = Some(view);
            }
            Err(e) => {
                panel.crash = None;
                panel.status = e.clone();
                crash_error = Some(e);
            }
        }
    }
    world.insert_resource(panel);
    let mut out = state_json(world);
    if let Some(e) = crash_error {
        out["crash_error"] = json!(e);
    }
    Ok(out)
}

fn timeline_json(timeline: &TraceTimeline, with_layout: bool) -> Value {
    let Some(loaded) = &timeline.loaded else {
        return json!({ "file": Value::Null, "loaded": false, "status": timeline.status });
    };
    let lanes: Vec<Value> = trace_lanes(&loaded.trace)
        .iter()
        .map(|l| json!({ "name": l.name, "first_row": l.first_row, "rows": l.rows }))
        .collect();
    let mut out = json!({
        "file": timeline.path.as_ref().map(|p| p.display().to_string()),
        "loaded": true,
        "events": loaded.trace.events.len(),
        "truncated": loaded.trace.truncated,
        "warning": loaded.warning,
        "adapter": loaded.trace.header.adapter,
        "lanes": lanes,
        "zoom": timeline.view.zoom,
        "pan": timeline.view.pan,
        "width": timeline.view.width,
        "status": timeline.status,
    });
    if with_layout {
        let rects = trace_layout(&loaded.trace, &timeline.view);
        let total = rects.len();
        out["items"] = Value::Array(
            rects
                .iter()
                .take(MAX_ITEMS)
                .map(|r| {
                    json!({
                        "kind": r.kind.name(), "name": r.name, "lane": r.lane, "row": r.row,
                        "frame": r.frame, "x": r.x, "width": r.width, "file": r.file,
                        "count": r.count,
                    })
                })
                .collect(),
        );
        out["items_total"] = json!(total);
    }
    out
}

/// `editor.trace_timeline { path?, zoom?, pan?, width?, close?, layout? }`.
fn trace_timeline(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut timeline = world.get_resource::<TraceTimeline>().cloned().unwrap_or_default();
    if params.get("close").and_then(Value::as_bool) == Some(true) {
        timeline = TraceTimeline::default();
    }
    if params.get("path").is_some_and(|v| !v.is_null()) {
        let path = crate::baseline_agent::agent_path(world, params)?;
        timeline
            .open(&path)
            .map_err(|e| bad("trace_unreadable", e, "a .regiatrace written by telemetry.trace"))?;
    }
    let mut view = timeline.view;
    if let Some(zoom) = number(params, "zoom")? {
        view.zoom = zoom;
    }
    if let Some(pan) = number(params, "pan")? {
        view.pan = pan;
    }
    if let Some(width) = number(params, "width")? {
        view.width = width;
    }
    timeline.view = view.clamped();
    let with_layout = params.get("layout").and_then(Value::as_bool).unwrap_or(true);
    let out = timeline_json(&timeline, with_layout);
    world.insert_resource(timeline);
    if params.get("path").is_some_and(|v| !v.is_null()) {
        let mut panel = world
            .get_resource::<DiagnosticsPanel>()
            .cloned()
            .unwrap_or_default();
        panel.open = true;
        panel.tab = DiagnosticsTab::Timeline;
        world.insert_resource(panel);
    }
    Ok(out)
}

/// `editor.trace_record { action, file? }`.
fn trace_record(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or_default();
    if !matches!(action, "start" | "stop" | "status") {
        return Err(bad(
            "bad_action",
            "`action` is \"start\", \"stop\" or \"status\"",
            "the record button's two states",
        ));
    }
    let file = match params.get("file") {
        None | Some(Value::Null) => None,
        Some(Value::String(file)) => Some(file.as_str()),
        // A number or an object used to be dropped, and the recording started under the default
        // name.
        Some(_) => {
            return Err(bad(
                "bad_param",
                "`file` is a path string",
                "see book/agents/gpu-breadcrumbs-and-trace.md",
            ));
        }
    };
    let mut answer = record(world, action, file).map_err(|e| {
        let (reason, message) = e.split_once(": ").unwrap_or(("trace_error", e.as_str()));
        bad(
            reason,
            message.to_owned(),
            "see book/agents/gpu-breadcrumbs-and-trace.md",
        )
    })?;
    if action == "stop"
        && let Some(file) = answer["file"].as_str().map(std::path::PathBuf::from)
    {
        let mut timeline = world.get_resource::<TraceTimeline>().cloned().unwrap_or_default();
        let opened = timeline.open(Path::new(&file));
        answer["opened_in_timeline"] = json!(opened.is_ok());
        world.insert_resource(timeline);
    }
    Ok(answer)
}

/// The Diagnostics window's agent methods, installed by `agent_ext::all_methods`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.diagnostics",
                tool: "diagnostics",
                description: "The Diagnostics window's GPU tab: breadcrumb evidence level, submitted/completed/in-flight counters, the newest frame's pass list. Opens the window or picks the tab, and `load_crash` reads the newest crash report (or bundle) and returns its `gpu breadcrumbs:` block.",
                params: &[
                    ParamSchema {
                        name: "open",
                        kind: "boolean",
                        required: false,
                        description: "Show or hide the window.",
                    },
                    ParamSchema {
                        name: "tab",
                        kind: "string",
                        required: false,
                        description: "\"gpu\" or \"timeline\".",
                    },
                    ParamSchema {
                        name: "load_crash",
                        kind: "boolean",
                        required: false,
                        description: "Load the last crash report's gpu breadcrumbs block.",
                    },
                ],
                result: "{ open, tab, gpu: { level, level_number, adapter, submitted, completed, in_flight, acknowledged_frame, last_frame, last_frame_passes, suspects } | { unavailable }, crash: { source, headline, block } | null, crash_error?, status }",
                mutating: true,
                permission: Permission::EditScene,
            },
            diagnostics as Handler,
        ),
        (
            MethodSchema {
                name: "editor.trace_timeline",
                tool: "trace_timeline",
                description: "The Diagnostics window's .regiatrace timeline: open a trace, set zoom and pan, read the lanes and the laid-out marks (vertical lines), ranges (bars per lane), GPU passes and screenshot thumbnails, ordered by frame.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "A .regiatrace file: absolute, or relative to the project. Opens it and shows the window's timeline tab.",
                    },
                    ParamSchema {
                        name: "zoom",
                        kind: "number",
                        required: false,
                        description: "1 shows the whole trace; up to 10000.",
                    },
                    ParamSchema {
                        name: "pan",
                        kind: "number",
                        required: false,
                        description: "0 (start) to 1 (end) when zoomed in.",
                    },
                    ParamSchema {
                        name: "width",
                        kind: "number",
                        required: false,
                        description: "View width in points (default the window's).",
                    },
                    ParamSchema {
                        name: "layout",
                        kind: "boolean",
                        required: false,
                        description: "List the laid-out items (default true; at most 500).",
                    },
                    ParamSchema {
                        name: "close",
                        kind: "boolean",
                        required: false,
                        description: "Close the open trace first.",
                    },
                ],
                result: "{ file, loaded, events, truncated, warning, adapter, lanes: [{ name, first_row, rows }], zoom, pan, width, items?: [{ kind, name, lane, row, frame, x, width, file, count }], items_total?, status }",
                mutating: true,
                permission: Permission::EditScene,
            },
            trace_timeline as Handler,
        ),
        (
            MethodSchema {
                name: "editor.trace_record",
                tool: "trace_record",
                description: "The timeline's record button: start or stop the process trace recorder (the telemetry.trace handler). Stopping opens the written file in the timeline.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "\"start\", \"stop\" or \"status\".",
                    },
                    ParamSchema {
                        name: "file",
                        kind: "string",
                        required: false,
                        description: "For start: a bare file name in the profiling folder.",
                    },
                ],
                result: "the telemetry.trace summary plus opened_in_timeline on stop",
                mutating: true,
                permission: Permission::Play,
            },
            trace_record as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_methods_are_named_and_unique() {
        let names: Vec<_> = methods().into_iter().map(|(m, _)| m.name).collect();
        assert_eq!(
            names,
            [
                "editor.diagnostics",
                "editor.trace_timeline",
                "editor.trace_record"
            ]
        );
    }

    #[test]
    fn bad_params_give_codes() {
        let mut world = World::new();
        let e = diagnostics(&mut world, &json!({ "tab": "cpu" })).expect_err("bad tab");
        assert_eq!(e.data.as_ref().map(|d| d["code"].clone()), Some(json!("bad_tab")));
        let e = trace_timeline(&mut world, &json!({ "zoom": "big" })).expect_err("bad zoom");
        assert_eq!(
            e.data.as_ref().map(|d| d["code"].clone()),
            Some(json!("bad_view"))
        );
        let e = trace_record(&mut world, &json!({ "action": "pause" })).expect_err("bad action");
        assert_eq!(
            e.data.as_ref().map(|d| d["code"].clone()),
            Some(json!("bad_action"))
        );
    }

    /// Corr editor-a finding L4: a flag or a file of the wrong type is `bad_param`, and nothing
    /// changes. It used to be ignored and the call answered ok.
    #[test]
    fn wrong_typed_params_are_refused() {
        let mut world = World::new();
        for params in [json!({ "open": "true" }), json!({ "load_crash": 1 })] {
            let e = diagnostics(&mut world, &params).expect_err("wrong type");
            assert_eq!(
                e.data.as_ref().map(|d| d["code"].clone()),
                Some(json!("bad_param")),
                "{params}"
            );
        }
        assert!(
            world.get_resource::<DiagnosticsPanel>().is_none(),
            "a refused call changes nothing"
        );
        let e = trace_record(&mut world, &json!({ "action": "start", "file": 7 })).expect_err("file");
        assert_eq!(
            e.data.as_ref().map(|d| d["code"].clone()),
            Some(json!("bad_param"))
        );
    }
}
