// SPDX-License-Identifier: MPL-2.0
//! Exports of a [`TraceFile`] (spec 54 § 4.3): JSON lines for the planned editor profiler, and
//! the public Trace Event Format (`traceEvents`, phases `B`/`E`/`i`/`X`) for any generic viewer.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use serde_json::{Value, json};

use super::events::{EventKind, Lane, TraceFile};

/// Version of the JSON lines layout.
pub const JSONL_FORMAT_VERSION: u32 = 1;

fn io(e: serde_json::Error) -> std::io::Error {
    std::io::Error::other(e)
}

fn kind_value(kind: &EventKind) -> Value {
    match kind {
        EventKind::Mark { name, data, frame } => json!({
            "type": "mark", "name": name, "frame": frame,
            "data": data.as_deref().and_then(|d| serde_json::from_str::<Value>(d).ok()),
        }),
        EventKind::RangeOpen {
            id,
            name,
            frame,
            depth,
            parent,
        } => {
            json!({"type": "range_open", "id": id, "name": name, "frame": frame, "depth": depth, "parent": parent})
        }
        EventKind::RangeClose { id, frame, forced } => {
            json!({"type": "range_close", "id": id, "frame": frame, "forced": forced})
        }
        EventKind::FrameBegin { frame } => json!({"type": "frame_begin", "frame": frame}),
        EventKind::FrameEnd { frame } => json!({"type": "frame_end", "frame": frame}),
        EventKind::PassOpen {
            name,
            frame,
            submission,
            ..
        } => json!({"type": "pass_open", "name": &**name, "frame": frame, "submission": submission}),
        EventKind::PassClose {
            name,
            frame,
            submission,
            ..
        } => json!({"type": "pass_close", "name": &**name, "frame": frame, "submission": submission}),
        EventKind::GpuInterval {
            name,
            begin_ns,
            end_ns,
            frame,
            ..
        } => {
            json!({"type": "gpu_interval", "name": &**name, "begin_ns": begin_ns, "end_ns": end_ns, "frame": frame})
        }
        EventKind::Log { level, target, text } => {
            json!({"type": "log", "level": level, "target": target, "text": text})
        }
        EventKind::Counter { name, value } => json!({"type": "counter", "name": name, "value": value}),
        EventKind::Screenshot {
            name,
            frame,
            width,
            height,
            file,
        } => {
            json!({"type": "screenshot", "name": name, "frame": frame, "width": width, "height": height, "file": file})
        }
    }
}

/// Writes JSON lines: a `header` object, then one object per event with names resolved.
pub fn export_jsonl(trace: &TraceFile, out: &mut impl Write) -> std::io::Result<()> {
    let _span = tracing::trace_span!("trace_export").entered();
    let h = &trace.header;
    let header = json!({
        "format_version": JSONL_FORMAT_VERSION, "type": "header",
        "trace_format_version": h.format_version, "start_unix_ms": h.start_unix_ms,
        "engine_version": h.engine_version, "adapter": h.adapter,
        "build_profile": h.build_profile, "platform": h.platform,
        "gpu_clock_error_ns": h.gpu_clock_error_ns, "truncated": trace.truncated,
    });
    writeln!(out, "{}", serde_json::to_string(&header).map_err(io)?)?;
    for e in &trace.events {
        let mut v = kind_value(&e.kind);
        if let Value::Object(m) = &mut v {
            m.insert("t_ns".into(), json!(e.t_ns));
            m.insert("lane".into(), json!(e.lane.name()));
        }
        writeln!(out, "{}", serde_json::to_string(&v).map_err(io)?)?;
    }
    Ok(())
}

/// Writes the Trace Event Format: marks become global instants, ranges `B`/`E` pairs on their
/// lane, GPU intervals `X` events on the `gpu` lane, screenshot marks instants with the PNG path
/// in `args`. A close whose open fell outside the file (a window edge) is left out.
pub fn export_trace_event(trace: &TraceFile, out: &mut impl Write) -> std::io::Result<()> {
    let _span = tracing::trace_span!("trace_export").entered();
    let mut tids: HashMap<String, u32> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut tid = |name: &str| -> u32 {
        if let Some(&t) = tids.get(name) {
            return t;
        }
        let t = u32::try_from(order.len()).unwrap_or(u32::MAX) + 1;
        tids.insert(name.to_owned(), t);
        order.push(name.to_owned());
        t
    };
    let us = |ns: u64| ns as f64 / 1000.0;
    let mut events: Vec<Value> = Vec::new();
    let mut open_ranges: HashSet<u64> = HashSet::new();
    let mut open_frames: HashSet<u64> = HashSet::new();
    let mut open_pass: HashMap<u16, u32> = HashMap::new();
    for e in &trace.events {
        let lane = e.lane.name();
        match &e.kind {
            EventKind::Mark { name, data, frame } => events.push(json!({
                "ph": "i", "s": "g", "name": name, "ts": us(e.t_ns), "pid": 1, "tid": tid(&lane),
                "args": {"frame": frame, "data": data.as_deref().and_then(|d| serde_json::from_str::<Value>(d).ok())},
            })),
            EventKind::RangeOpen { id, name, frame, .. } => {
                open_ranges.insert(*id);
                events.push(json!({
                    "ph": "B", "name": name, "ts": us(e.t_ns), "pid": 1, "tid": tid(&lane), "args": {"frame": frame},
                }));
            }
            EventKind::RangeClose { id, forced, .. } => {
                if open_ranges.remove(id) {
                    events.push(json!({
                        "ph": "E", "ts": us(e.t_ns), "pid": 1, "tid": tid(&lane), "args": {"forced": forced},
                    }));
                }
            }
            EventKind::FrameBegin { frame } => {
                open_frames.insert(*frame);
                events.push(json!({
                    "ph": "B", "name": format!("frame {frame}"), "ts": us(e.t_ns), "pid": 1, "tid": tid("frames"),
                }));
            }
            EventKind::FrameEnd { frame } => {
                if open_frames.remove(frame) {
                    events.push(json!({"ph": "E", "ts": us(e.t_ns), "pid": 1, "tid": tid("frames")}));
                }
            }
            EventKind::PassOpen { tag, name, frame, .. } => {
                *open_pass.entry(*tag).or_default() += 1;
                events.push(json!({
                    "ph": "B", "name": &**name, "ts": us(e.t_ns), "pid": 1, "tid": tid(&Lane::Gpu.name()),
                    "args": {"frame": frame},
                }));
            }
            EventKind::PassClose { tag, .. } => {
                if let Some(n) = open_pass.get_mut(tag).filter(|n| **n > 0) {
                    *n -= 1;
                    events.push(json!({"ph": "E", "ts": us(e.t_ns), "pid": 1, "tid": tid(&Lane::Gpu.name())}));
                }
            }
            EventKind::GpuInterval {
                name,
                begin_ns,
                end_ns,
                frame,
                ..
            } => events.push(json!({
                "ph": "X", "name": &**name, "ts": us(*begin_ns), "dur": us(end_ns.saturating_sub(*begin_ns)),
                "pid": 1, "tid": tid(&Lane::Gpu.name()), "args": {"frame": frame},
            })),
            EventKind::Log { level, target, text } => events.push(json!({
                "ph": "i", "s": "t", "name": text, "ts": us(e.t_ns), "pid": 1, "tid": tid(&lane),
                "args": {"level": level, "target": target},
            })),
            EventKind::Counter { name, value } => events.push(json!({
                "ph": "C", "name": name, "ts": us(e.t_ns), "pid": 1, "args": {"value": value},
            })),
            EventKind::Screenshot {
                name,
                frame,
                width,
                height,
                file,
            } => events.push(json!({
                "ph": "i", "s": "g", "name": name.clone().unwrap_or_else(|| "screenshot".to_owned()),
                "ts": us(e.t_ns), "pid": 1, "tid": tid(&lane),
                "args": {"file": file, "frame": frame, "width": width, "height": height},
            })),
        }
    }
    for name in &order {
        events.push(json!({
            "ph": "M", "name": "thread_name", "pid": 1, "tid": tids[name], "args": {"name": name},
        }));
    }
    let doc = json!({
        "traceEvents": events,
        "displayTimeUnit": "ms",
        "otherData": {"format_version": JSONL_FORMAT_VERSION, "engine_version": trace.header.engine_version},
    });
    out.write_all(serde_json::to_string(&doc).map_err(io)?.as_bytes())
}
