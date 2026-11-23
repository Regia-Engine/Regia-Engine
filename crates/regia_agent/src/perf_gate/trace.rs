// SPDX-License-Identifier: MPL-2.0
//! Captured-trace gate: frame, feel and memory files for one run directory.
//!
//! Percentiles here are nearest-rank. The summary gate uses linear interpolation; the two
//! definitions stay separate because each suite pins its own.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::{CommandOutput, TraceRequest, rpc};
use crate::protocol::RpcError;

struct Capture {
    kind: Kind,
    tier: String,
    res: String,
    width: u32,
    text: String,
    /// `csv` or `jsonl`. Feel files are classified by this, not by scanning the text.
    ext: String,
}

enum Kind {
    Frames,
    Feel,
    Mem,
}

struct Group {
    tier: String,
    res: String,
    width: u32,
    frames: Option<String>,
    feel_csv: Option<String>,
    feel_jsonl: Option<String>,
    mem: Option<String>,
}

struct Row {
    tier: String,
    res: String,
    failed: bool,
    checks: Vec<Check>,
    stats: Stats,
}

struct Check {
    name: String,
    status: &'static str,
}

struct Stats {
    frames: usize,
    avg_ms: Option<f64>,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    p99_ms: Option<f64>,
    max_ms: Option<f64>,
    hitch_count: u64,
    gpu_avg: Option<f64>,
    rss_peak_mb: Option<f64>,
    vram_peak_mb: Option<f64>,
    feel_p50_ms: Option<f64>,
    feel_p95_ms: Option<f64>,
    feel_samples: usize,
}

/// Reads one run directory and returns the trace-gate report.
pub(super) fn trace_main(request: &TraceRequest) -> CommandOutput {
    if !request.regression_pct.is_finite()
        || request.regression_pct < 0.0
        || !request.hitch_ms.is_finite()
        || request.hitch_ms < 0.0
    {
        return fail("regression_pct and hitch_ms must be finite and non-negative");
    }
    let files = match read_run(&request.run_dir) {
        Ok(files) => files,
        Err(message) => return fail(&message),
    };
    let baseline = if request.baseline_path.exists() {
        match fs::read_to_string(&request.baseline_path) {
            Ok(text) => match load_entries(&text) {
                Ok(entries) => Some(entries),
                Err(message) => return fail(&message),
            },
            Err(_) => return fail("could not read the baseline"),
        }
    } else {
        Some(Map::new())
    };
    let Some(entries) = baseline else {
        return fail("baseline is not valid JSON");
    };
    let report = match analyze_files(&files, &entries, request.regression_pct, request.hitch_ms) {
        Ok(report) => report,
        Err(message) => return fail(&message),
    };
    if request.update_baseline {
        let document = baseline_document(&entries, &report);
        if let Err(message) = atomic_write(&request.baseline_path, &document) {
            return fail(&message);
        }
        return CommandOutput {
            code: 0,
            stdout: render(request.json, &report, Some(&document)),
            stderr: String::new(),
        };
    }
    CommandOutput {
        code: if report.iter().any(|row| row.failed) { 1 } else { 0 },
        stdout: render(request.json, &report, None),
        stderr: String::new(),
    }
}

/// Trace mode of `logs.perf_gate`. `run` maps each capture file name to its text.
pub(super) fn trace_agent(
    run: &Value,
    params: &Map<String, Value>,
    regression_pct: f64,
    hitch_ms: f64,
) -> Result<Value, RpcError> {
    let files = run.as_object().ok_or_else(|| {
        rpc(
            "logs_perf_gate_input",
            "run must be an object of file name to text",
        )
    })?;
    let mut loaded = Vec::new();
    for (name, text) in files {
        let Some(text) = text.as_str() else {
            return Err(rpc("logs_perf_gate_input", "run file text must be a string"));
        };
        if let Some(capture) = parse_name(name, text) {
            loaded.push(capture);
        }
    }
    let baseline = match params.get("baseline") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::String(text)) => {
            load_entries(text).map_err(|message| rpc("logs_perf_gate_baseline", message))?
        }
        Some(_) => return Err(rpc("logs_perf_gate_baseline", "baseline must be a string")),
    };
    let report = analyze_files(&loaded, &baseline, regression_pct, hitch_ms)
        .map_err(|message| rpc("logs_perf_gate_input", message))?;
    let update = match params.get("update_baseline") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(rpc("logs_perf_gate_input", "update_baseline must be a boolean")),
    };
    let document = baseline_document(&baseline, &report);
    let mut value = report_value(&report, update.then_some(&document));
    if update {
        value["baseline_json"] = document;
    }
    Ok(value)
}

fn fail(message: &str) -> CommandOutput {
    CommandOutput {
        code: 2,
        stdout: String::new(),
        stderr: format!("perf_gate: error: {message}\n"),
    }
}

fn read_run(dir: &Path) -> Result<Vec<Capture>, String> {
    let entries = fs::read_dir(dir).map_err(|_| "run directory is missing".to_owned())?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| "could not read the run directory".to_owned())?;
        let file_type = entry
            .file_type()
            .map_err(|_| "could not read the run directory".to_owned())?;
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let text =
            fs::read_to_string(entry.path()).map_err(|_| "could not read a capture file".to_owned())?;
        if let Some(capture) = parse_name(name, &text) {
            files.push(capture);
        }
    }
    Ok(files)
}

fn parse_name(name: &str, text: &str) -> Option<Capture> {
    let (stem, ext) = name.rsplit_once('.')?;
    if ext != "csv" && ext != "jsonl" {
        return None;
    }
    let mut parts = stem.split('-');
    let kind = parts.next()?;
    let tier = parts.next()?;
    let res = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let kind = match kind {
        "frames" => Kind::Frames,
        "feel" => Kind::Feel,
        "mem" => Kind::Mem,
        _ => return None,
    };
    if tier.is_empty() || !tier.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return None;
    }
    let (width_text, height_text) = res.split_once('x')?;
    if width_text.is_empty()
        || height_text.is_empty()
        || !width_text.bytes().all(|byte| byte.is_ascii_digit())
        || !height_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let width = width_text.parse().ok()?;
    let _height: u32 = height_text.parse().ok()?;
    Some(Capture {
        kind,
        tier: tier.to_owned(),
        res: res.to_owned(),
        width,
        text: text.to_owned(),
        ext: ext.to_owned(),
    })
}

fn load_entries(text: &str) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_str(text).map_err(|_| "baseline is not valid JSON".to_owned())?;
    let entries = value
        .get("entries")
        .and_then(Value::as_object)
        .ok_or_else(|| "baseline is not valid JSON".to_owned())?;
    Ok(entries.clone())
}

fn analyze_files(
    files: &[Capture],
    entries: &Map<String, Value>,
    regression_pct: f64,
    hitch_ms: f64,
) -> Result<Vec<Row>, String> {
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();
    for file in files {
        let group = groups
            .entry((file.tier.clone(), file.res.clone()))
            .or_insert_with(|| Group {
                tier: file.tier.clone(),
                res: file.res.clone(),
                width: file.width,
                frames: None,
                feel_csv: None,
                feel_jsonl: None,
                mem: None,
            });
        match &file.kind {
            Kind::Frames => group.frames = Some(file.text.clone()),
            Kind::Feel => {
                if file.ext == "jsonl" {
                    group.feel_jsonl = Some(file.text.clone());
                } else {
                    group.feel_csv = Some(file.text.clone());
                }
            }
            Kind::Mem => group.mem = Some(file.text.clone()),
        }
    }
    if groups.is_empty() {
        return Err("no trace files".to_owned());
    }
    let mut rows = Vec::new();
    for group in groups.into_values() {
        let frame = group.frames.as_deref().map(load_frame_csv).unwrap_or_default();
        let feel = match (group.feel_csv.as_deref(), group.feel_jsonl.as_deref()) {
            (Some(text), _) => feel_latencies(text, false),
            (_, Some(text)) => feel_latencies(text, true),
            _ => Vec::new(),
        };
        let mem = group.mem.as_deref().map(load_mem_csv).unwrap_or_default();
        let has_feel = group.feel_csv.is_some() || group.feel_jsonl.is_some();
        let key = format!("{}@{}", group.tier, group.res);
        let entry = entries.get(&key);
        rows.push(analyze_row(&RowQuery {
            tier: &group.tier,
            res: &group.res,
            width: group.width,
            frames: &frame,
            feel: &feel,
            mem: &mem,
            has_feel,
            entry,
            regression_pct,
            hitch_ms,
        }));
    }
    Ok(rows)
}

#[derive(Default)]
struct FrameCsv {
    ms: Vec<f64>,
    gpu: Vec<f64>,
}

#[derive(Default)]
struct MemCsv {
    rss_peak_mb: Option<f64>,
    vram_peak_mb: Option<f64>,
}

fn load_frame_csv(text: &str) -> FrameCsv {
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        rows.push(split_csv(line));
    }
    if rows.is_empty() {
        return FrameCsv::default();
    }
    let header = rows[0].get(1).is_none_or(|cell| cell.parse::<f64>().is_err());
    let (ms_col, gpu_col, start) = if header {
        let ms_col = column(&rows[0], &["ms", "frame_ms"]).unwrap_or(1);
        let gpu_col = column(&rows[0], &["gpu", "gpu_ms"]);
        (ms_col, gpu_col, 1)
    } else {
        (1, Some(4), 0)
    };
    let mut loaded = FrameCsv::default();
    for row in rows.iter().skip(start) {
        let Some(ms) = row.get(ms_col).and_then(|cell| parse_cell(cell)) else {
            continue;
        };
        if ms < 0.0 {
            continue;
        }
        loaded.ms.push(ms);
        if let Some(index) = gpu_col
            && let Some(gpu) = row.get(index).and_then(|cell| parse_cell(cell))
        {
            loaded.gpu.push(gpu);
        }
    }
    loaded
}

fn feel_latencies(text: &str, jsonl: bool) -> Vec<f64> {
    if jsonl {
        let mut latencies = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(obj) = value.as_object() else {
                continue;
            };
            let input = super::num_of(obj, &["input_t_ms", "input_ms", "input"]);
            let present = super::num_of(obj, &["present_t_ms", "present_ms", "present"]);
            if let (Some(input), Some(present)) = (input, present) {
                latencies.push(present - input);
            }
        }
        return latencies;
    }
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        rows.push(split_csv(line));
    }
    if rows.is_empty() {
        return Vec::new();
    }
    let header = rows[0].iter().any(|cell| cell.parse::<f64>().is_err());
    let (input_col, present_col, start) = if header {
        (
            column(&rows[0], &["input_t_ms", "input_ms", "input"]).unwrap_or(0),
            column(&rows[0], &["present_t_ms", "present_ms", "present"]).unwrap_or(1),
            1,
        )
    } else {
        (0, 1, 0)
    };
    let mut latencies = Vec::new();
    for row in rows.iter().skip(start) {
        let input = row.get(input_col).and_then(|cell| parse_cell(cell));
        let present = row.get(present_col).and_then(|cell| parse_cell(cell));
        if let (Some(input), Some(present)) = (input, present) {
            latencies.push(present - input);
        }
    }
    latencies
}

fn load_mem_csv(text: &str) -> MemCsv {
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        rows.push(split_csv(line));
    }
    if rows.is_empty() {
        return MemCsv::default();
    }
    let header = rows[0].iter().any(|cell| cell.parse::<f64>().is_err());
    let (rss_col, vram_col, start) = if header {
        (
            column(&rows[0], &["rss_mb", "rss"]),
            column(&rows[0], &["vram_mb", "vram"]),
            1,
        )
    } else {
        (Some(1), Some(2), 0)
    };
    let mut loaded = MemCsv::default();
    for row in rows.iter().skip(start) {
        if let Some(index) = rss_col {
            consider(
                &mut loaded.rss_peak_mb,
                row.get(index).map(String::as_str).unwrap_or(""),
            );
        }
        if let Some(index) = vram_col {
            consider(
                &mut loaded.vram_peak_mb,
                row.get(index).map(String::as_str).unwrap_or(""),
            );
        }
    }
    loaded
}

#[derive(Copy, Clone)]
struct RowQuery<'a> {
    tier: &'a str,
    res: &'a str,
    width: u32,
    frames: &'a FrameCsv,
    feel: &'a [f64],
    mem: &'a MemCsv,
    has_feel: bool,
    entry: Option<&'a Value>,
    regression_pct: f64,
    hitch_ms: f64,
}

fn analyze_row(query: &RowQuery<'_>) -> Row {
    let RowQuery {
        tier,
        res,
        width,
        frames,
        feel,
        mem,
        has_feel,
        entry,
        regression_pct,
        hitch_ms,
    } = *query;
    let stats = stats_of(frames, feel, mem, hitch_ms);
    let mut checks = Vec::new();
    if !has_feel {
        checks.push(Check {
            name: "feel".to_owned(),
            status: "NO DATA",
        });
    }
    let class = width_class(width);
    if let (Some(avg), Some(p99)) = (stats.avg_ms, stats.p99_ms) {
        if let Some((avg_max, p99_max)) = budget(tier, class) {
            checks.push(verdict("budget.avg", avg <= avg_max));
            checks.push(verdict("budget.p99", p99 <= p99_max));
        } else {
            checks.push(Check {
                name: "budget".to_owned(),
                status: "NO DATA",
            });
        }
    } else {
        checks.push(Check {
            name: "budget".to_owned(),
            status: "NO DATA",
        });
    }
    if let Some(entry) = entry {
        push_stat(
            &mut checks,
            "regression.avg",
            stats.avg_ms,
            entry_f64(entry, "avg_ms"),
            regression_pct,
        );
        push_stat(
            &mut checks,
            "regression.p50",
            stats.p50_ms,
            entry_f64(entry, "p50_ms"),
            regression_pct,
        );
        push_stat(
            &mut checks,
            "regression.p95",
            stats.p95_ms,
            entry_f64(entry, "p95_ms"),
            regression_pct,
        );
        push_stat(
            &mut checks,
            "regression.p99",
            stats.p99_ms,
            entry_f64(entry, "p99_ms"),
            regression_pct,
        );
        push_stat(
            &mut checks,
            "regression.rss_peak",
            stats.rss_peak_mb,
            entry_f64(entry, "rss_peak_mb"),
            regression_pct,
        );
        push_stat(
            &mut checks,
            "regression.vram_peak",
            stats.vram_peak_mb,
            entry_f64(entry, "vram_peak_mb"),
            regression_pct,
        );
        push_stat(
            &mut checks,
            "regression.feel_p50",
            stats.feel_p50_ms,
            entry_f64(entry, "feel_p50_ms"),
            regression_pct,
        );
        push_stat(
            &mut checks,
            "regression.feel_p95",
            stats.feel_p95_ms,
            entry_f64(entry, "feel_p95_ms"),
            regression_pct,
        );
        if let Some(base) = entry_f64(entry, "hitch_count") {
            let delta = stats.hitch_count as f64 - base;
            let fail =
                delta >= 5.0 && (base < 10.0 || regressed(stats.hitch_count as f64, base, regression_pct));
            checks.push(Check {
                name: "regression.hitches".to_owned(),
                status: if fail { "FAIL" } else { "PASS" },
            });
        }
    } else if stats.frames > 0 {
        checks.push(Check {
            name: "baseline".to_owned(),
            status: "NEW",
        });
    }
    let failed = checks.iter().any(|check| check.status == "FAIL");
    Row {
        tier: tier.to_owned(),
        res: res.to_owned(),
        failed,
        checks,
        stats,
    }
}

fn stats_of(frames: &FrameCsv, feel: &[f64], mem: &MemCsv, hitch_ms: f64) -> Stats {
    let sorted_ms = sorted(&frames.ms);
    let sorted_feel = sorted(feel);
    Stats {
        frames: frames.ms.len(),
        avg_ms: mean(&frames.ms),
        p50_ms: nearest(&sorted_ms, 50.0),
        p95_ms: nearest(&sorted_ms, 95.0),
        p99_ms: nearest(&sorted_ms, 99.0),
        max_ms: sorted_ms.last().copied(),
        hitch_count: frames.ms.iter().filter(|ms| **ms >= hitch_ms).count() as u64,
        gpu_avg: mean(&frames.gpu),
        rss_peak_mb: mem.rss_peak_mb,
        vram_peak_mb: mem.vram_peak_mb,
        feel_p50_ms: nearest(&sorted_feel, 50.0),
        feel_p95_ms: nearest(&sorted_feel, 95.0),
        feel_samples: feel.len(),
    }
}

fn verdict(name: &str, passed: bool) -> Check {
    Check {
        name: name.to_owned(),
        status: if passed { "PASS" } else { "FAIL" },
    }
}

fn push_stat(checks: &mut Vec<Check>, name: &str, current: Option<f64>, base: Option<f64>, pct: f64) {
    let (Some(current), Some(base)) = (current, base) else {
        return;
    };
    checks.push(verdict(name, !regressed(current, base, pct)));
}

fn regressed(current: f64, base: f64, pct: f64) -> bool {
    base > 0.0 && (current - base) / base * 100.0 > pct
}

fn budget(tier: &str, class: &str) -> Option<(f64, f64)> {
    match (tier, class) {
        ("low" | "medium", "1080") => Some((6.0, 8.0)),
        ("high", "1080") => Some((5.7, 7.5)),
        ("high", "1440") => Some((11.5, 14.5)),
        ("ultra", "4k") => Some((16.7, 20.0)),
        ("max", "4k") => Some((33.3, 40.0)),
        _ => None,
    }
}

fn width_class(width: u32) -> &'static str {
    if width <= 1920 {
        "1080"
    } else if width <= 2560 {
        "1440"
    } else {
        "4k"
    }
}

fn nearest(sorted: &[f64], pct: f64) -> Option<f64> {
    let len = sorted.len();
    if len == 0 {
        return None;
    }
    let product = pct / 100.0 * len as f64;
    let nearest = product.round();
    let product = if (product - nearest).abs() < 1e-8 {
        nearest
    } else {
        product
    };
    let rank = product.ceil() as isize;
    let index = (rank - 1).clamp(0, len as isize - 1) as usize;
    Some(sorted[index])
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

fn sorted(values: &[f64]) -> Vec<f64> {
    let mut copy = values.to_vec();
    copy.sort_by(f64::total_cmp);
    copy
}

fn entry_f64(entry: &Value, key: &str) -> Option<f64> {
    let value = entry.get(key)?;
    // `as_f64` already accepts JSON integers that fit. The integer arms cover a number
    // whose float view is absent.
    let number = value
        .as_f64()
        .or_else(|| value.as_i64().map(|item| item as f64))
        .or_else(|| value.as_u64().map(|item| item as f64))?;
    number.is_finite().then_some(number)
}

fn baseline_document(existing: &Map<String, Value>, rows: &[Row]) -> Value {
    let mut entries = existing.clone();
    for row in rows {
        let key = format!("{}@{}", row.tier, row.res);
        entries.insert(key, stored_stats(&row.stats));
    }
    json!({ "version": 1, "entries": entries })
}

fn stored_stats(stats: &Stats) -> Value {
    let mut obj = Map::new();
    insert_rounded(&mut obj, "avg_ms", stats.avg_ms);
    insert_rounded(&mut obj, "p50_ms", stats.p50_ms);
    insert_rounded(&mut obj, "p95_ms", stats.p95_ms);
    insert_rounded(&mut obj, "p99_ms", stats.p99_ms);
    insert_rounded(&mut obj, "max_ms", stats.max_ms);
    obj.insert("hitch_count".to_owned(), json!(stats.hitch_count));
    obj.insert("frames".to_owned(), json!(stats.frames));
    insert_rounded(&mut obj, "rss_peak_mb", stats.rss_peak_mb);
    insert_rounded(&mut obj, "vram_peak_mb", stats.vram_peak_mb);
    insert_rounded(&mut obj, "feel_p50_ms", stats.feel_p50_ms);
    insert_rounded(&mut obj, "feel_p95_ms", stats.feel_p95_ms);
    obj.insert("feel_samples".to_owned(), json!(stats.feel_samples));
    Value::Object(obj)
}

fn insert_rounded(obj: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(value) = value {
        obj.insert(key.to_owned(), json!(round3(value)));
    }
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn report_value(rows: &[Row], baseline: Option<&Value>) -> Value {
    json!({
        "mode": "trace",
        "overall": if rows.iter().any(|row| row.failed) { "FAIL" } else { "PASS" },
        "failed": rows.iter().any(|row| row.failed),
        "rows": rows.iter().map(row_value).collect::<Vec<_>>(),
        "stats": {},
        "results": [],
        "regressions": [],
        "warnings": [],
        "info": [],
        "res_class": Value::Null,
        "tier": Value::Null,
        "baseline_json": baseline.cloned().unwrap_or(Value::Null),
    })
}

fn row_value(row: &Row) -> Value {
    json!({
        "tier": row.tier,
        "res": row.res,
        "failed": row.failed,
        "checks": row.checks.iter().map(|check| json!({"name": check.name, "status": check.status})).collect::<Vec<_>>(),
        "avg_ms": row.stats.avg_ms,
        "p50_ms": row.stats.p50_ms,
        "p99_ms": row.stats.p99_ms,
        "gpu_avg_ms": row.stats.gpu_avg,
        "hitch_count": row.stats.hitch_count,
        "frames": row.stats.frames,
    })
}

fn render(json_out: bool, rows: &[Row], baseline: Option<&Value>) -> String {
    if json_out {
        return format!("{}\n", report_value(rows, baseline));
    }
    let mut lines = Vec::new();
    for row in rows {
        lines.push(format!(
            "{} {} {}",
            row.tier,
            row.res,
            if row.failed { "FAIL" } else { "PASS" }
        ));
        for check in &row.checks {
            lines.push(format!("  {} {}", check.name, check.status));
        }
    }
    lines.push(format!(
        "overall: {}",
        if rows.iter().any(|row| row.failed) {
            "FAIL"
        } else {
            "PASS"
        }
    ));
    format!("{}\n", lines.join("\n"))
}

fn atomic_write(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string(value).map_err(|_| "could not encode the baseline".to_owned())?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "baseline.json".into());
    tmp_name.push(".tmp");
    let tmp = parent.join(tmp_name);
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    crate::req_path::write_new_temp(&tmp, text.as_bytes())
        .map_err(|_| "could not write the baseline".to_owned())?;
    fs::rename(&tmp, path).map_err(|_| "could not replace the baseline".to_owned())?;
    Ok(())
}

fn split_csv(line: &str) -> Vec<String> {
    line.split(',').map(|cell| cell.trim().to_owned()).collect()
}

fn column(header: &[String], names: &[&str]) -> Option<usize> {
    header
        .iter()
        .position(|cell| names.iter().any(|name| cell.eq_ignore_ascii_case(name)))
}

fn parse_cell(cell: &str) -> Option<f64> {
    if cell.is_empty() {
        return None;
    }
    cell.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn consider(peak: &mut Option<f64>, cell: &str) {
    if let Some(value) = parse_cell(cell) {
        *peak = Some(peak.map_or(value, |had| had.max(value)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn almost(actual: f64, expected: f64, places: i32) {
        let scale = 10f64.powi(places);
        assert!(
            (actual - expected).abs() * scale < 0.5,
            "{actual} != {expected} at {places} places"
        );
    }

    fn status<'a>(row: &'a Row, name: &str) -> &'a str {
        row.checks
            .iter()
            .find(|check| check.name == name)
            .map(|check| check.status)
            .unwrap_or("missing")
    }

    fn frame_text(samples: &[f64]) -> String {
        let mut text = String::from("frame,ms,game,render,gpu\n");
        for (index, ms) in samples.iter().enumerate() {
            text.push_str(&format!("{index},{ms:.4},0,0,{gpu:.2}\n", gpu = ms * 0.6));
        }
        text
    }

    fn series(scale: f64) -> Vec<f64> {
        (0..200)
            .map(|index| (5.4 + (index % 10) as f64 * 0.05) * scale)
            .collect()
    }

    fn baseline_entry() -> Value {
        json!({
            "avg_ms": 5.7,
            "p50_ms": 5.5,
            "p95_ms": 6.8,
            "p99_ms": 7.5,
            "max_ms": 9.0,
            "hitch_count": 0,
            "frames": 900,
            "rss_peak_mb": 1800.0,
            "vram_peak_mb": 2500.0,
            "feel_p50_ms": 12.0,
            "feel_p95_ms": 20.0,
            "feel_samples": 300
        })
    }

    fn row_for(tier: &str, width: u32, res: &str, samples: &[f64], entry: Option<&Value>, feel: bool) -> Row {
        let frames = load_frame_csv(&frame_text(samples));
        let feel_values = if feel { vec![12.0, 12.0] } else { Vec::new() };
        let mem = if feel {
            MemCsv {
                rss_peak_mb: Some(1800.0),
                vram_peak_mb: Some(2500.0),
            }
        } else {
            MemCsv::default()
        };
        analyze_row(&RowQuery {
            tier,
            res,
            width,
            frames: &frames,
            feel: &feel_values,
            mem: &mem,
            has_feel: feel,
            entry,
            regression_pct: 10.0,
            hitch_ms: 25.0,
        })
    }

    #[test]
    fn test_frame_percentiles_and_hitches() {
        let mut samples: Vec<f64> = (0..98).map(|index| 5.0 + index as f64 * 0.01).collect();
        samples.push(30.0);
        samples.push(45.0);
        let loaded = load_frame_csv(&frame_text(&samples));
        let stats = stats_of(&loaded, &[], &MemCsv::default(), 25.0);
        assert_eq!(stats.frames, 100);
        assert_eq!(stats.hitch_count, 2);
        almost(stats.p50_ms.expect("p50"), 5.49, 6);
        almost(stats.p99_ms.expect("p99"), 30.0, 6);
        let mean_ms = samples.iter().sum::<f64>() / samples.len() as f64;
        almost(stats.gpu_avg.expect("gpu"), mean_ms * 0.6, 1);
    }

    #[test]
    fn test_headerless_frame_csv() {
        let loaded = load_frame_csv("0,6.1,2.0,3.0,4.0\n1,6.3,2.0,3.0,4.0\n");
        assert_eq!(loaded.ms.len(), 2);
        almost(loaded.ms[0], 6.1, 6);
        almost(loaded.ms[1], 6.3, 6);
    }

    #[test]
    fn test_feel_csv_and_jsonl_latencies() {
        let csv = "input_t_ms,present_t_ms\n0,12.5\n10,23.3\n";
        let jsonl =
            "{\"input_t_ms\":0,\"present_t_ms\":12.5}\nnot json\n{\"input_t_ms\":10,\"present_t_ms\":23.3}\n";
        for (text, jsonl_mode) in [(csv, false), (jsonl, true)] {
            let latencies = feel_latencies(text, jsonl_mode);
            assert_eq!(latencies.len(), 2);
            let stats = stats_of(&FrameCsv::default(), &latencies, &MemCsv::default(), 25.0);
            assert_eq!(stats.feel_samples, 2);
            almost(stats.feel_p50_ms.expect("p50"), 12.5, 6);
            almost(stats.feel_p95_ms.expect("p95"), 13.3, 6);
        }
    }

    #[test]
    fn test_mem_csv_peaks() {
        let loaded = load_mem_csv("t_ms,rss_mb,vram_mb\n0,1000,2000\n1,1800.5,2500\n1,,\n");
        almost(loaded.rss_peak_mb.expect("rss"), 1800.5, 6);
        almost(loaded.vram_peak_mb.expect("vram"), 2500.0, 6);
        let empty = load_mem_csv("");
        assert!(empty.rss_peak_mb.is_none());
    }

    #[test]
    fn test_pass_row_clears_budget_and_regression() {
        let entry = baseline_entry();
        let row = row_for("high", 1920, "1920x1080", &series(1.0), Some(&entry), true);
        assert!(!row.failed);
        assert_eq!(status(&row, "budget.avg"), "PASS");
        assert_eq!(status(&row, "budget.p99"), "PASS");
        assert_eq!(status(&row, "regression.avg"), "PASS");
    }

    #[test]
    fn test_budget_breach_fails() {
        let samples: Vec<f64> = (0..200).map(|index| 9.0 + (index % 5) as f64 * 0.1).collect();
        let row = row_for("high", 1920, "1920x1080", &samples, None, false);
        assert!(row.failed);
        assert_eq!(status(&row, "budget.avg"), "FAIL");
        assert_eq!(status(&row, "baseline"), "NEW");
        assert_eq!(status(&row, "feel"), "NO DATA");
    }

    #[test]
    fn test_regression_over_ten_percent_fails() {
        let entry = baseline_entry();
        let row = row_for("high", 1920, "1920x1080", &series(1.15), Some(&entry), true);
        assert!(row.failed);
        assert_eq!(status(&row, "regression.avg"), "FAIL");
    }

    #[test]
    fn test_regression_within_band_passes() {
        let entry = baseline_entry();
        let row = row_for("high", 1920, "1920x1080", &series(1.05), Some(&entry), true);
        assert_eq!(status(&row, "regression.avg"), "PASS");
    }

    #[test]
    fn test_hitch_spike_fails() {
        let mut samples = vec![5.5; 200];
        samples.extend(std::iter::repeat_n(40.0, 8));
        let entry = baseline_entry();
        let row = row_for("high", 1920, "1920x1080", &samples, Some(&entry), true);
        assert_eq!(status(&row, "regression.hitches"), "FAIL");
        assert!(row.failed);
    }

    #[test]
    fn test_row_without_spec_budget_has_no_budget_verdict() {
        let samples = vec![5.5; 100];
        let row = row_for("low", 2560, "2560x1440", &samples, None, false);
        assert_eq!(status(&row, "budget"), "NO DATA");
    }

    fn write_run(dir: &Path, name: &str, samples: &[f64]) {
        let path = dir.join(name);
        let tmp = dir.join(format!("{name}.tmp"));
        fs::write(&tmp, frame_text(samples)).expect("write capture");
        fs::rename(tmp, path).expect("rename capture");
    }

    fn baseline_file(entries: Value) -> String {
        serde_json::to_string(&json!({"version": 1, "entries": entries})).expect("baseline json")
    }

    #[test]
    fn test_cli_exit_codes_and_update_baseline() {
        let dir = regia_test_utils::unique_temp_dir("perf_gate_exit");
        let run = dir.join("run");
        fs::create_dir(&run).expect("run dir");
        write_run(&run, "frames-high-1920x1080.csv", &series(1.0));
        let baseline = dir.join("baseline.json");
        let mut entries = Map::new();
        entries.insert("high@1920x1080".to_owned(), baseline_entry());
        fs::write(&baseline, baseline_file(Value::Object(entries))).expect("baseline");
        let first = trace_main(&TraceRequest {
            run_dir: run.clone(),
            baseline_path: baseline.clone(),
            update_baseline: false,
            regression_pct: 10.0,
            hitch_ms: 25.0,
            json: false,
        });
        assert_eq!(first.code, 0, "{}", first.stderr);
        write_run(
            &run,
            "frames-high-1920x1080.csv",
            &(0..200)
                .map(|index| 6.48 + (index % 10) as f64 * 0.05)
                .collect::<Vec<_>>(),
        );
        let second = trace_main(&TraceRequest {
            run_dir: run,
            baseline_path: baseline,
            update_baseline: false,
            regression_pct: 10.0,
            hitch_ms: 25.0,
            json: false,
        });
        assert_eq!(second.code, 1, "{}", second.stdout);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_update_baseline_then_regression() {
        let dir = regia_test_utils::unique_temp_dir("perf_gate_update");
        let run = dir.join("run");
        fs::create_dir(&run).expect("run dir");
        write_run(&run, "frames-low-2560x1440.csv", &vec![11.0; 200]);
        let baseline = dir.join("baseline.json");
        fs::write(&baseline, baseline_file(json!({}))).expect("empty baseline");
        let request = |update| TraceRequest {
            run_dir: run.clone(),
            baseline_path: baseline.clone(),
            update_baseline: update,
            regression_pct: 10.0,
            hitch_ms: 25.0,
            json: false,
        };
        assert_eq!(trace_main(&request(false)).code, 0);
        assert_eq!(trace_main(&request(true)).code, 0);
        let stored: Value =
            serde_json::from_str(&fs::read_to_string(&baseline).expect("read")).expect("json");
        almost(
            stored["entries"]["low@2560x1440"]["avg_ms"]
                .as_f64()
                .expect("avg"),
            11.0,
            3,
        );
        write_run(&run, "frames-low-2560x1440.csv", &vec![12.5; 200]);
        assert_eq!(trace_main(&request(false)).code, 1);
        assert_eq!(trace_main(&request(true)).code, 0);
        assert_eq!(trace_main(&request(false)).code, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cli_corrupt_baseline_is_exit_two() {
        let dir = regia_test_utils::unique_temp_dir("perf_gate_corrupt");
        let run = dir.join("run");
        fs::create_dir(&run).expect("run dir");
        write_run(&run, "frames-high-1920x1080.csv", &series(1.0));
        let baseline = dir.join("baseline.json");
        fs::write(&baseline, "{ not json").expect("corrupt");
        let output = trace_main(&TraceRequest {
            run_dir: run,
            baseline_path: baseline,
            update_baseline: false,
            regression_pct: 10.0,
            hitch_ms: 25.0,
            json: false,
        });
        assert_eq!(output.code, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cli_missing_run_is_exit_two() {
        let dir = regia_test_utils::unique_temp_dir("perf_gate_missing");
        let baseline = dir.join("baseline.json");
        fs::write(&baseline, baseline_file(json!({}))).expect("baseline");
        let output = trace_main(&TraceRequest {
            run_dir: dir.join("missing"),
            baseline_path: baseline,
            update_baseline: false,
            regression_pct: 10.0,
            hitch_ms: 25.0,
            json: false,
        });
        assert_eq!(output.code, 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
