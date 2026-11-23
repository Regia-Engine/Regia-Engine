// SPDX-License-Identifier: MPL-2.0
//! Sparse rate statistics and engine-general session summaries.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    Settings, Stitched, error,
    time::{number, timestamp},
};
use crate::protocol::RpcError;

fn data(record: &Value) -> Value {
    for value in [
        &record["fields"]["data"],
        &record["fields"]["data_json"],
        &record["data"],
        &record["fields"],
    ] {
        if value.is_object() {
            return value.clone();
        }
        if let Some(text) = value.as_str()
            && let Ok(value) = serde_json::from_str::<Value>(text)
            && value.is_object()
        {
            return value;
        }
    }
    json!({})
}

/// Frame timings from the producer schema, then legacy flat fields, then tracing fields
/// that sit beside a JSON `data` string (that string replaces the selected object).
fn frame_millis(record: &Value, data: &Value, key: &str, nested: &str) -> Option<f64> {
    number(&data["frame_ms"][nested])
        .or_else(|| number(&data[key]))
        .or_else(|| number(&record["fields"][key]))
        .or_else(|| number(&record[key]))
        .filter(|n| *n >= 0.0)
}

fn frame_sample(record: &Value, data: &Value) -> Value {
    let mut sample = if data.is_object() { data.clone() } else { json!({}) };
    if let Some(obj) = sample.as_object_mut() {
        if let Some(p50) = frame_millis(record, data, "p50_ms", "p50") {
            obj.insert("p50_ms".to_owned(), json!(p50));
        }
        if let Some(p99) = frame_millis(record, data, "p99_ms", "p99") {
            obj.insert("p99_ms".to_owned(), json!(p99));
        }
    }
    sample
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn final_summary(path: &str) -> bool {
    if partial_summary(path) {
        return false;
    }
    let name = file_name(path);
    name == "summary.json" || name.ends_with(".summary.json") || name.ends_with("-summary.json")
}

fn partial_summary(path: &str) -> bool {
    let name = file_name(path);
    name == "summary.partial.json" || name.ends_with(".summary.partial.json")
}

fn text<'a>(value: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|key| value[key].as_str()).unwrap_or("")
}

fn percentile(values: &mut [f64], percentile: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let index = (values.len() - 1) as f64 * percentile;
    let lo = index.floor() as usize;
    let hi = index.ceil() as usize;
    Some(values[lo] + (values[hi] - values[lo]) * index.fract())
}

fn metric(values: &[Value], key: &str, percentile_value: f64) -> Option<f64> {
    percentile(
        &mut values
            .iter()
            .filter_map(|v| number(&v[key]).filter(|n| *n >= 0.0))
            .collect::<Vec<_>>(),
        percentile_value,
    )
}

// Numeric scanning is UTF-8 preserving; UUIDs and numeric runs become one placeholder each.
fn template(message: &str) -> String {
    let message = regia_core::privacy::scrub_report(message);
    let bytes = message.as_bytes();
    let mut out = String::new();
    let mut at = 0;
    while at < bytes.len() {
        let remaining = &bytes[at..];
        let uuid = remaining.len() >= 36
            && remaining[..36].iter().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    *b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            });
        if uuid {
            out.push('#');
            at += 36;
        } else if bytes[at].is_ascii_digit()
            || (matches!(bytes[at], b'+' | b'-' | b'.') && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            let start = at;
            if matches!(bytes[at], b'+' | b'-') {
                at += 1;
            }
            if bytes.get(at..at + 2).is_some_and(|s| s == b"0x" || s == b"0X") {
                at += 2;
                while bytes.get(at).is_some_and(u8::is_ascii_hexdigit) {
                    at += 1;
                }
            } else {
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                if bytes.get(at) == Some(&b'.') {
                    at += 1;
                    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                        at += 1;
                    }
                }
                if bytes.get(at).is_some_and(|b| matches!(b, b'e' | b'E')) {
                    let exponent = at;
                    at += 1;
                    if bytes.get(at).is_some_and(|b| matches!(b, b'+' | b'-')) {
                        at += 1;
                    }
                    let digits = at;
                    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                        at += 1;
                    }
                    if at == digits {
                        at = exponent;
                    }
                }
            }
            debug_assert!(at > start);
            out.push('#');
        } else {
            let Some(ch) = message[at..].chars().next() else {
                break;
            };
            out.push(ch);
            at += ch.len_utf8();
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn summarize(session: &str, files: &[&Stitched], settings: &Settings) -> Result<Value, RpcError> {
    let mut records = Vec::new();
    let mut malformed = 0;
    for file in files.iter().filter(|f| f.path.ends_with(".jsonl")) {
        for line in file.text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<Value>(line) {
                Ok(value) if value.is_object() => records.push(value),
                _ => malformed += 1,
            }
        }
    }
    // Stable sorting preserves source order for tied timestamps and for records without a clock.
    records.sort_by(|a, b| match (timestamp(a), timestamp(b)) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    let mut warnings = 0;
    let mut errors = 0;
    let mut panics = 0;
    let mut device_lost = 0;
    let mut matches = 0;
    let mut in_match = false;
    let mut frames = Vec::new();
    let mut match_frames = Vec::new();
    let mut network = Vec::new();
    let mut desyncs = 0;
    let mut late_inputs = 0;
    let mut hitches = Vec::new();
    let mut stalls = Vec::new();
    let mut phases = BTreeMap::<String, (u64, f64)>::new();
    let mut templates = BTreeMap::<String, u64>::new();
    let mut levels = BTreeMap::<String, u64>::new();
    let mut exit = None;
    let mut exit_max_ms: Option<f64> = None;
    for record in &records {
        let target = text(record, &["target"]);
        let suffix = target.rsplit("::").next().unwrap_or(target);
        let level = text(record, &["level"]).to_ascii_uppercase();
        *levels.entry(level.clone()).or_default() += 1;
        if level == "WARN" || level == "WARNING" {
            warnings += 1;
        }
        if level == "ERROR" {
            errors += 1;
        }
        let message = text(record, &["msg", "message", "text"]);
        let message = if message.is_empty() {
            text(&record["fields"], &["message"])
        } else {
            message
        };
        *templates.entry(template(message)).or_default() += 1;
        let d = data(record);
        match suffix {
            "match" => match text(&d, &["event"]) {
                "start" | "begin" | "open" => {
                    matches += 1;
                    in_match = true;
                }
                "end" | "finish" | "over" | "close" => in_match = false,
                _ => {}
            },
            "phase" => {
                let phase = text(&d, &["phase"]);
                if phase == "match_start" {
                    matches += 1;
                    in_match = true;
                }
                if phase == "match_end" {
                    in_match = false;
                }
                if let Some(ms) = number(&d["ms"]).filter(|n| *n >= 0.0) {
                    let entry = phases.entry(phase.to_owned()).or_default();
                    entry.0 += 1;
                    entry.1 = entry.1.max(ms);
                }
            }
            "frame_summary" | "summary" => {
                let sample = frame_sample(record, &d);
                if in_match
                    && number(&sample["tick"])
                        .or_else(|| number(&record["tick"]))
                        .is_none_or(|n| n > 0.0)
                {
                    match_frames.push(sample.clone());
                }
                frames.push(sample);
            }
            "hitch" => {
                if let Some(ms) = number(&d["frame_ms"])
                    .or_else(|| number(&d["ms"]))
                    .filter(|n| *n >= 0.0)
                {
                    hitches.push(json!({"t_ms": timestamp(record), "ms": ms,
                        "cause": text(&d, &["cause", "span"])}));
                }
            }
            "stall" => {
                if let Some(ms) = number(&d["silent_ms"])
                    .or_else(|| number(&d["ms"]))
                    .filter(|n| *n >= 0.0)
                {
                    stalls.push(
                        json!({"t_ms": timestamp(record), "ms": ms, "where": text(&d, &["where", "phase"])}),
                    );
                }
            }
            "net" | "network" | "lockstep" => {
                let event = text(&d, &["event"]).to_ascii_lowercase();
                let count = d["count"].as_u64().or_else(|| d["inputs"].as_u64()).unwrap_or(1);
                if event.contains("desync") || event.contains("mismatch") {
                    desyncs = u64::saturating_add(desyncs, count);
                }
                if event.contains("late") {
                    late_inputs = u64::saturating_add(late_inputs, count);
                }
                network.push(d);
            }
            "panic" => panics += 1,
            "device_lost" => device_lost += 1,
            "exit" => {
                if let Some(clean) = d["clean"].as_bool() {
                    exit = Some(clean);
                }
                let steps = d["steps"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(std::slice::from_ref(&d));
                for step in steps {
                    if let Some(ms) = number(&step["ms"]).filter(|n| *n >= 0.0) {
                        exit_max_ms = Some(exit_max_ms.unwrap_or(0.0).max(ms));
                    }
                }
            }
            _ => {}
        }
    }
    let finals: Vec<_> = files.iter().filter(|f| final_summary(&f.path)).collect();
    let partial = files.iter().any(|f| partial_summary(&f.path));
    let mut summary_errors = 0;
    if !finals.is_empty() {
        let mut states = Vec::new();
        for file in finals {
            if let Ok(v) = serde_json::from_str::<Value>(&file.text)
                && let Some(clean) = v["exit"]["clean"].as_bool()
            {
                states.push(clean);
            } else {
                summary_errors += 1;
            }
        }
        // A corrupt final must never be mistaken for proof of a clean exit.
        exit = if states.contains(&false) {
            Some(false)
        } else if summary_errors == 0 && !states.is_empty() {
            Some(true)
        } else {
            None
        };
    } else if partial {
        exit = Some(false);
    }
    let rates = rates(&records, settings)?;
    let rtt = metric(&network, "rtt_ms", 0.95);
    let loss = metric(&network, "loss_pct", 0.95);
    let match_p50 = metric(&match_frames, "p50_ms", 0.5);
    let verdict = if panics > 0 || device_lost > 0 || exit == Some(false) {
        "crash_or_unclean"
    } else if desyncs > 0
        || rtt.is_some_and(|n| n > settings.slow_rtt_ms)
        || loss.is_some_and(|n| n > settings.loss_percent)
    {
        "network"
    } else if match_p50.is_some_and(|n| n > settings.slow_frame_ms) {
        "frame_rate"
    } else if records.is_empty() {
        "insufficient_data"
    } else {
        "no_issue_detected"
    };
    let mut templates: Vec<_> = templates.into_iter().collect();
    templates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    templates.truncate(settings.top);
    Ok(json!({
        "session":session,"records":records.len(),"malformed_lines":malformed,"summary_errors":summary_errors,
        "files":files.len(),"source_gaps":files.iter().map(|f| f.gaps.len()).sum::<usize>(),
        "duplicates":files.iter().map(|f| f.duplicates).sum::<usize>(),
        "exit":match exit {Some(true)=>"clean",Some(false)=>"unclean",None=>"unknown"},
        "exit_max_ms":exit_max_ms,"warnings":warnings,"errors":errors,"levels":levels,
        "panics":panics,"device_lost":device_lost,"matches":matches,"verdict":verdict,
        "frame_summary_p50_median_ms":metric(&frames,"p50_ms",0.5),
        "frame_summary_p99_max_ms":metric(&frames,"p99_ms",1.0),
        "match_frame_summary_p50_median_ms":match_p50,
        "hitches":hitches.len(),"hitch_max_ms":metric(&hitches,"ms",1.0),
        "hitch_details":hitches,"stalls":stalls,
        "phases":phases.into_iter().map(|(name,(count,max_ms))|json!({"phase":name,"count":count,"max_ms":max_ms})).collect::<Vec<_>>(),
        "rtt_p95_ms":rtt,"loss_p95_percent":loss,"desyncs":desyncs,"late_inputs":late_inputs,
        "templates":templates.into_iter().map(|(template,count)|json!({"template":template,"count":count})).collect::<Vec<_>>(),
        "rates":rates
    }))
}

fn rates(records: &[Value], settings: &Settings) -> Result<Value, RpcError> {
    let timed: Vec<_> = records
        .iter()
        .filter_map(|v| timestamp(v).map(|t| (t, text(v, &["target"]))))
        .collect();
    let start = timed.first().map(|v| v.0);
    let end = timed.last().map(|v| v.0);
    let origin = start.unwrap_or(0.0);
    let bucket_ms = settings.bucket_seconds * 1000.0;
    if !bucket_ms.is_finite() {
        return Err(error(
            "logs_triage_settings",
            "bucket width overflows milliseconds",
        ));
    }
    let last = ((end.unwrap_or(origin) - origin) / bucket_ms).floor() as u64;
    let span = last.saturating_add(1);
    let mut buckets: BTreeMap<&str, BTreeMap<u64, u64>> = BTreeMap::new();
    for (t, target) in &timed {
        *buckets
            .entry(target)
            .or_default()
            .entry(((t - origin) / bucket_ms).floor() as u64)
            .or_default() += 1;
    }
    let mut rows = Vec::new();
    let mut storms = Vec::new();
    let mut totals = BTreeMap::new();
    for (target, counts) in buckets {
        let mut nonzero: Vec<_> = counts.values().copied().collect();
        nonzero.sort_unstable();
        let zeros = span - nonzero.len() as u64;
        let rank = |i: u64| {
            if i < zeros {
                0.0
            } else {
                nonzero[(i - zeros) as usize] as f64
            }
        };
        let median = (rank((span - 1) / 2) + rank(span / 2)) / 2.0;
        totals.insert(target, counts.values().sum::<u64>());
        for (index, count) in counts {
            let row = json!({"target":target,"bucket":index,"start_ms":origin+index as f64*bucket_ms,
                "count":count,"per_second":count as f64/settings.bucket_seconds});
            rows.push(row.clone());
            if (median == 0.0 && count >= settings.storm_min_count)
                || (median > 0.0 && count as f64 > median * settings.storm_factor)
            {
                let mut row = row;
                row["median_count"] = json!(median);
                row["ratio"] = if median == 0.0 {
                    Value::Null
                } else {
                    json!(count as f64 / median)
                };
                storms.push(row);
            }
        }
    }
    let gaps: Vec<_> = timed
        .windows(2)
        .filter_map(|w| {
            let ms = w[1].0 - w[0].0;
            (ms > settings.gap_seconds * 1000.0)
                .then(|| json!({"start_ms":w[0].0,"end_ms":w[1].0,"duration_ms":ms}))
        })
        .collect();
    Ok(
        json!({"timed_records":timed.len(),"untimed_records":records.len()-timed.len(),
        "start_ms":start,"end_ms":end,"duration_ms":end.zip(start).map(|(e,s)|e-s),
        "bucket_seconds":settings.bucket_seconds,"bucket_span":if timed.is_empty(){0}else{span},
        "targets":totals,"buckets":rows,"storms":storms,"gaps":gaps}),
    )
}
