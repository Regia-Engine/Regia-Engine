// SPDX-License-Identifier: MPL-2.0
//! Session-summary performance gate (ADR-8429).
//!
//! `logs.perf_gate` and `regia logs perf-gate` judge captured frame windows against a thresholds
//! file. Hidden and loading frames are left out. A baseline summary fails the gate when frame-time
//! p50 or p99 regresses past the allowed percent. Trace captures (`--run`) live in `trace`.
//!
//! The summary mark is `regia::summary`. Telemetry `regia::frame_summary` documents are accepted
//! too. No filesystem path is opened here.

mod trace;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Text result of one perf-gate invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Process status: 0 pass, 1 gate failed, 2 bad input.
    pub code: i32,
    /// Stdout text. Empty when the command only reports an error.
    pub stdout: String,
    /// Stderr text. Empty when the command could read its inputs.
    pub stderr: String,
}

/// Already-loaded session logs and the thresholds document.
#[derive(Debug, Clone)]
pub struct SummaryRequest {
    /// JSONL logs, or one JSON summary each.
    pub logs: Vec<String>,
    /// Thresholds TOML.
    pub thresholds: String,
    /// Older summary used for the regression comparison.
    pub baseline: Option<String>,
    /// Resolution-class override (`1080p`, `1440p`, `ultrawide`).
    pub res: Option<String>,
    /// Tier override (`LOW`, `HIGH`, `ULTRA`).
    pub tier: Option<String>,
    /// Allowed p50 regression, in percent. p99 may move twice this far.
    pub regression_pct: f64,
    /// A visible frame at or above this many milliseconds is a hitch.
    pub hitch_ms: f64,
    /// Print one JSON document instead of the text table.
    pub json: bool,
}

/// A directory of `frames-` / `feel-` / `mem-` captures.
#[derive(Debug, Clone)]
pub struct TraceRequest {
    /// Directory that holds the capture files.
    pub run_dir: PathBuf,
    /// Baseline JSON. A path that does not exist is an empty baseline.
    pub baseline_path: PathBuf,
    /// Replace the baseline with this run and exit 0.
    pub update_baseline: bool,
    /// Percent a stat may rise above its baseline before the row fails.
    pub regression_pct: f64,
    /// Hitch threshold in milliseconds.
    pub hitch_ms: f64,
    /// Print one JSON document instead of the text table.
    pub json: bool,
}

/// Runs the summary gate. Opens no files.
#[tracing::instrument(skip_all)]
pub fn summary_main(request: &SummaryRequest) -> CommandOutput {
    match evaluate_summary(request) {
        Ok(done) => CommandOutput {
            code: if done.failed { 1 } else { 0 },
            stdout: if request.json {
                format!("{}\n", done.value)
            } else {
                format!("{}\n", done.table)
            },
            stderr: String::new(),
        },
        Err(error) => CommandOutput {
            code: 2,
            stdout: String::new(),
            stderr: format!("perf_gate: error: {}\n", error.message),
        },
    }
}

/// Runs the trace gate, reading `request.run_dir` and the baseline path.
#[tracing::instrument(skip_all)]
pub fn trace_main(request: &TraceRequest) -> CommandOutput {
    trace::trace_main(request)
}

/// Agent method `logs.perf_gate`. A failing gate is a result, not an RPC error.
///
/// Summary mode reads `logs` and `thresholds`. Trace mode reads `run` (file name to text).
/// `run` wins when both are set. Nothing on disk is opened or written; `update_baseline`
/// returns `baseline_json` in the result.
#[tracing::instrument(skip_all)]
pub fn perf_gate(params: &Value) -> Result<Value, RpcError> {
    let obj = params.as_object().ok_or_else(|| {
        rpc(
            "logs_perf_gate_input",
            "params must be an object with logs or run",
        )
    })?;
    let regression_pct = optional_number(obj, "regression_pct")?.unwrap_or(10.0);
    let hitch_ms = optional_number(obj, "hitch_ms")?.unwrap_or(25.0);
    if !regression_pct.is_finite() || regression_pct < 0.0 || !hitch_ms.is_finite() || hitch_ms < 0.0 {
        return Err(rpc(
            "logs_perf_gate_input",
            "regression_pct and hitch_ms must be finite and non-negative",
        ));
    }
    if let Some(run) = obj.get("run").filter(|value| !value.is_null()) {
        return trace::trace_agent(run, obj, regression_pct, hitch_ms);
    }
    let logs = match obj.get("logs") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut logs = Vec::new();
            for item in items {
                let Some(text) = item.as_str() else {
                    return Err(rpc("logs_perf_gate_input", "logs entries must be strings"));
                };
                logs.push(text.to_owned());
            }
            logs
        }
        Some(_) => return Err(rpc("logs_perf_gate_input", "logs must be an array of strings")),
    };
    if logs.is_empty() {
        return Err(rpc("logs_perf_gate_input", "pass logs or a run object"));
    }
    let thresholds = required_string(obj, "thresholds")?;
    let baseline = optional_string(obj, "baseline")?;
    let res = optional_string(obj, "res")?;
    let tier = optional_string(obj, "tier")?;
    let request = SummaryRequest {
        logs,
        thresholds,
        baseline,
        res,
        tier,
        regression_pct,
        hitch_ms,
        json: true,
    };
    evaluate_summary(&request)
        .map(|done| done.value)
        .map_err(|error| rpc(error.code, error.message))
}

struct Done {
    value: Value,
    table: String,
    failed: bool,
}

struct GateError {
    code: &'static str,
    message: String,
}

struct Session {
    maps: BTreeMap<String, Bucket>,
    hidden: u64,
    loading: u64,
    malformed: u64,
    res_hint: Option<String>,
    tier_hint: Option<String>,
}

impl Session {
    fn new() -> Self {
        Self {
            maps: BTreeMap::new(),
            hidden: 0,
            loading: 0,
            malformed: 0,
            res_hint: None,
            tier_hint: None,
        }
    }
}

struct Bucket {
    frames: Vec<f64>,
    update: Vec<f64>,
    render: Vec<f64>,
    outside: Vec<f64>,
    gpu: Vec<f64>,
    telem: Option<Telem>,
    rss_mb: Option<f64>,
    vram_mb: Option<f64>,
}

impl Bucket {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            update: Vec::new(),
            render: Vec::new(),
            outside: Vec::new(),
            gpu: Vec::new(),
            telem: None,
            rss_mb: None,
            vram_mb: None,
        }
    }
}

struct Telem {
    n: u64,
    p50: Option<f64>,
    p95: Option<f64>,
    p99: Option<f64>,
    avg: Option<f64>,
    hitches: Option<f64>,
    seconds: Option<f64>,
}

struct Metrics {
    n: u64,
    fps: Option<f64>,
    p50: Option<f64>,
    p95: Option<f64>,
    p99: Option<f64>,
    update_p50: Option<f64>,
    render_p50: Option<f64>,
    outside_p50: Option<f64>,
    gpu_p50: Option<f64>,
    hitches_per_min: Option<f64>,
    rss_mb: Option<f64>,
    vram_mb: Option<f64>,
}

fn evaluate_summary(request: &SummaryRequest) -> Result<Done, GateError> {
    if !request.regression_pct.is_finite()
        || request.regression_pct < 0.0
        || !request.hitch_ms.is_finite()
        || request.hitch_ms < 0.0
    {
        return Err(GateError {
            code: "logs_perf_gate_input",
            message: "regression_pct and hitch_ms must be finite and non-negative".to_owned(),
        });
    }
    let doc: toml::Value = toml::from_str(&request.thresholds).map_err(|_| GateError {
        code: "logs_perf_gate_thresholds",
        message: "thresholds are not valid TOML".to_owned(),
    })?;
    let mut session = Session::new();
    for log in &request.logs {
        ingest_document(log, &mut session, false)?;
    }
    let mut baseline = Session::new();
    if let Some(text) = &request.baseline {
        ingest_document(text, &mut baseline, true)?;
    }
    Ok(finish(request, &doc, &session, &baseline))
}

fn ingest_document(text: &str, session: &mut Session, strict: bool) -> Result<(), GateError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
    {
        ingest_value(&value, session);
        return Ok(());
    }
    let mut parsed = 0u64;
    let mut bad = 0u64;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => {
                parsed += 1;
                ingest_value(&value, session);
            }
            Err(_) => bad += 1,
        }
    }
    session.malformed += bad;
    if strict && parsed == 0 && bad > 0 {
        return Err(GateError {
            code: "logs_perf_gate_baseline",
            message: "baseline is not valid JSON".to_owned(),
        });
    }
    Ok(())
}

fn ingest_value(value: &Value, session: &mut Session) {
    match value {
        Value::Array(items) => {
            for item in items {
                ingest_value(item, session);
            }
        }
        Value::Object(obj) => ingest_object(obj, session),
        _ => session.malformed += 1,
    }
}

fn ingest_object(obj: &Map<String, Value>, session: &mut Session) {
    let body = nested_body(obj);
    if !is_play(obj, body) {
        return;
    }
    if is_sample(obj, body) {
        ingest_samples(obj, body, session);
        return;
    }
    if is_telemetry(obj, body) {
        ingest_telemetry(obj, body, session);
    }
}

fn nested_body(obj: &Map<String, Value>) -> &Map<String, Value> {
    for key in ["fields", "data"] {
        if let Some(Value::Object(inner)) = obj.get(key)
            && (inner.contains_key("frames")
                || inner.contains_key("frames_by_map")
                || inner.contains_key("map")
                || inner.contains_key("map_id"))
        {
            return inner;
        }
    }
    obj
}

fn is_play(obj: &Map<String, Value>, body: &Map<String, Value>) -> bool {
    phase_ok(obj) && phase_ok(body)
}

fn phase_ok(obj: &Map<String, Value>) -> bool {
    match text_of(obj, &["phase"]) {
        Some(phase) => phase.eq_ignore_ascii_case("play"),
        None => true,
    }
}

fn mark_is(obj: &Map<String, Value>, marks: &[&str]) -> bool {
    ["target", "record", "type", "kind", "msg"].iter().any(|key| {
        obj.get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| marks.contains(&value))
    })
}

fn is_sample(obj: &Map<String, Value>, body: &Map<String, Value>) -> bool {
    matches!(body.get("frames"), Some(Value::Array(_)))
        && (mark_is(obj, &["regia::summary", "summary"]) || mark_is(body, &["regia::summary", "summary"]))
}

fn is_telemetry(obj: &Map<String, Value>, body: &Map<String, Value>) -> bool {
    let marked = mark_is(obj, &["regia::frame_summary"]) || mark_is(body, &["regia::frame_summary"]);
    let shaped = body.contains_key("frames_by_map")
        || matches!(body.get("frames"), Some(Value::Object(frames)) if frames.contains_key("p50_ms") || frames.contains_key("p99_ms"));
    marked || shaped
}

fn ingest_samples(obj: &Map<String, Value>, body: &Map<String, Value>, session: &mut Session) {
    let Some(Value::Array(frames)) = body.get("frames") else {
        return;
    };
    note_hint(obj, body, session);
    let map = map_name(obj, body);
    let bucket = session.maps.entry(map).or_insert_with(Bucket::new);
    take_memory(obj, body, bucket);
    for frame in frames {
        let Some(frame) = frame.as_object() else {
            continue;
        };
        if flag(frame, "loading") {
            session.loading += 1;
            continue;
        }
        if hidden_frame(frame) {
            session.hidden += 1;
            continue;
        }
        let Some(ms) = num_of(frame, &["frame_ms", "frame", "dt_ms", "dt"]) else {
            continue;
        };
        if ms <= 0.0 {
            continue;
        }
        bucket.frames.push(ms);
        if let Some(value) = num_of(frame, &["update_ms", "update"]) {
            bucket.update.push(value);
        }
        if let Some(value) = num_of(frame, &["render_ms", "render"]) {
            bucket.render.push(value);
        }
        if let Some(value) = num_of(
            frame,
            &["outside_update_ms", "outside_update", "outside_ms", "outside"],
        ) {
            bucket.outside.push(value);
        }
        if let Some(value) = num_of(frame, &["gpu_ms", "gpu"]) {
            bucket.gpu.push(value);
        }
    }
}

fn hidden_frame(frame: &Map<String, Value>) -> bool {
    if flag(frame, "hidden") {
        return true;
    }
    if frame.get("visible").and_then(Value::as_bool) == Some(false) {
        return true;
    }
    let outside = num_of(
        frame,
        &["outside_update_ms", "outside_update", "outside_ms", "outside"],
    )
    .unwrap_or(0.0);
    let render = num_of(frame, &["render_ms", "render"]);
    outside >= 80.0 && render.is_none_or(|value| value <= 0.0)
}

fn ingest_telemetry(obj: &Map<String, Value>, body: &Map<String, Value>, session: &mut Session) {
    note_hint(obj, body, session);
    if let Some(Value::Object(by_map)) = body.get("frames_by_map") {
        for (map, stats) in by_map {
            let Some(stats) = stats.as_object() else {
                continue;
            };
            let bucket = session.maps.entry(map.clone()).or_insert_with(Bucket::new);
            take_memory(obj, body, bucket);
            bucket.telem = Some(telem_of(stats, body));
        }
        return;
    }
    if let Some(Value::Object(stats)) = body.get("frames") {
        let map = map_name(obj, body);
        let bucket = session.maps.entry(map).or_insert_with(Bucket::new);
        take_memory(obj, body, bucket);
        bucket.telem = Some(telem_of(stats, body));
    }
}

fn telem_of(stats: &Map<String, Value>, body: &Map<String, Value>) -> Telem {
    let n = num_of(stats, &["n", "count", "frames"])
        .map(|value| value.max(0.0) as u64)
        .unwrap_or(0);
    let seconds = body
        .get("session")
        .and_then(Value::as_object)
        .and_then(|session| num_of(session, &["seconds", "duration_s"]))
        .or_else(|| num_of(body, &["seconds", "session_seconds"]));
    let hitches = body
        .get("hitches")
        .and_then(Value::as_object)
        .and_then(|hitches| num_of(hitches, &["count"]))
        .or_else(|| num_of(body, &["hitch_count"]));
    Telem {
        n,
        p50: num_of(stats, &["p50_ms", "p50"]),
        p95: num_of(stats, &["p95_ms", "p95"]),
        p99: num_of(stats, &["p99_ms", "p99"]),
        avg: num_of(stats, &["avg_ms", "mean_ms", "avg"]),
        hitches,
        seconds,
    }
}

fn note_hint(obj: &Map<String, Value>, body: &Map<String, Value>, session: &mut Session) {
    if session.res_hint.is_none() {
        session.res_hint = res_hint(obj).or_else(|| res_hint(body));
    }
    if session.tier_hint.is_none() {
        session.tier_hint = tier_hint(obj).or_else(|| tier_hint(body));
    }
}

fn res_hint(obj: &Map<String, Value>) -> Option<String> {
    if let Some(name) = text_of(obj, &["res_class", "resolution_class", "res", "resolution"]) {
        return Some(normalize_res(name));
    }
    let width = num_of(obj, &["width"]).or_else(|| {
        obj.get("display")
            .and_then(Value::as_object)
            .and_then(|display| num_of(display, &["width"]))
    })?;
    let height = num_of(obj, &["height"]).or_else(|| {
        obj.get("display")
            .and_then(Value::as_object)
            .and_then(|display| num_of(display, &["height"]))
    })?;
    Some(class_from_pixels(width, height))
}

fn tier_hint(obj: &Map<String, Value>) -> Option<String> {
    if let Some(tier) = text_of(obj, &["tier", "preset", "quality"]) {
        return Some(normalize_tier(tier));
    }
    obj.get("settings")
        .and_then(Value::as_object)
        .and_then(|settings| text_of(settings, &["preset", "tier", "quality"]))
        .map(normalize_tier)
}

fn map_name(obj: &Map<String, Value>, body: &Map<String, Value>) -> String {
    text_of(body, &["map", "map_id", "level"])
        .or_else(|| text_of(obj, &["map", "map_id", "level"]))
        .unwrap_or("unknown")
        .to_owned()
}

fn take_memory(obj: &Map<String, Value>, body: &Map<String, Value>, bucket: &mut Bucket) {
    for source in [body, obj] {
        if let Some(mb) = num_of(source, &["rss_peak_mb", "rss_mb"]) {
            peak(&mut bucket.rss_mb, mb);
        }
        if let Some(mb) = num_of(source, &["vram_peak_mb", "vram_mb"]) {
            peak(&mut bucket.vram_mb, mb);
        }
        if let Some(memory) = source.get("memory").and_then(Value::as_object) {
            if let Some(gb) = num_of(memory, &["rss_peak_gb", "rss_gb"]) {
                peak(&mut bucket.rss_mb, gb * 1000.0);
            }
            if let Some(gb) = num_of(memory, &["vram_peak_gb", "vram_gb"]) {
                peak(&mut bucket.vram_mb, gb * 1000.0);
            }
        }
    }
}

fn peak(slot: &mut Option<f64>, value: f64) {
    if value.is_finite() {
        *slot = Some(slot.map_or(value, |had| had.max(value)));
    }
}

fn finish(request: &SummaryRequest, doc: &toml::Value, session: &Session, baseline: &Session) -> Done {
    let mut warnings = Vec::new();
    let res = match request.res.clone() {
        Some(value) => normalize_res(&value),
        None => session.res_hint.clone().unwrap_or_else(|| {
            warnings.push("resolution class missing; assuming 1080p".to_owned());
            "1080p".to_owned()
        }),
    };
    let tier = match request.tier.clone() {
        Some(value) => normalize_tier(&value),
        None => session.tier_hint.clone().unwrap_or_else(|| {
            warnings.push("tier missing; assuming HIGH".to_owned());
            "HIGH".to_owned()
        }),
    };
    let mut stats = Map::new();
    let mut results = Vec::new();
    let mut regressions = Vec::new();
    let mut table_rows: Vec<String> = Vec::new();
    let mut any_fail = false;
    let mut usable = 0u32;
    for (map, bucket) in &session.maps {
        if bucket.frames.is_empty() && bucket.telem.is_none() {
            continue;
        }
        usable += 1;
        let metrics = metrics_of(bucket, request.hitch_ms);
        if metrics.n > 0 && metrics.n < 10 {
            warnings.push(format!("{map} has {n} visible frames", n = metrics.n));
        }
        stats.insert(map.clone(), stats_value(&metrics, bucket));
        let (limits, used_default) = lookup_limits(doc, &res, &tier, map);
        if used_default {
            warnings.push(format!("no threshold row for {map}; using default"));
        }
        let Some(limits) = limits else {
            warnings.push(format!("no thresholds for {res} {tier}"));
            continue;
        };
        for check in checks_for(&metrics, limits) {
            if !check.passed {
                any_fail = true;
            }
            table_rows.push(format!(
                "{:<16} {:<22} {:>10.3} {:>10.3} {}",
                map,
                check.metric,
                check.value,
                check.limit,
                pass_word(check.passed)
            ));
            results.push(json!({
                "map": map,
                "metric": check.metric,
                "value": check.value,
                "limit": check.limit,
                "pass": check.passed,
            }));
        }
        match baseline.maps.get(map) {
            None => {
                if !baseline.maps.is_empty() || request.baseline.is_some() {
                    warnings.push(format!("{map} is missing from the baseline"));
                }
            }
            Some(base_bucket) => {
                let base = metrics_of(base_bucket, request.hitch_ms);
                let mut sinks = GateSinks {
                    regressions: &mut regressions,
                    table_rows: &mut table_rows,
                    any_fail: &mut any_fail,
                };
                push_regression(
                    map,
                    "frame p50 vs baseline",
                    metrics.p50,
                    base.p50,
                    request.regression_pct,
                    &mut sinks,
                );
                push_regression(
                    map,
                    "frame p99 vs baseline",
                    metrics.p99,
                    base.p99,
                    request.regression_pct * 2.0,
                    &mut sinks,
                );
            }
        }
    }
    if usable == 0 {
        warnings.push("no usable play frames found".to_owned());
        any_fail = true;
    }
    let mut header = format!("perf gate {res} {tier} ({} hidden frames skipped", session.hidden);
    if session.loading > 0 {
        header.push_str(&format!(", {} loading frames skipped", session.loading));
    }
    header.push(')');
    let mut lines = vec![header];
    for warning in &warnings {
        lines.push(format!("warning: {warning}"));
    }
    lines.extend(table_rows);
    lines.push(format!("overall: {}", pass_word(!any_fail)));
    let info = vec![format!("{} hidden frames skipped", session.hidden)];
    let value = json!({
        "mode": "summary",
        "overall": pass_word(!any_fail),
        "res_class": res,
        "tier": tier,
        "hidden_frames": session.hidden,
        "loading_frames": session.loading,
        "malformed_lines": session.malformed,
        "stats": Value::Object(stats),
        "results": results,
        "regressions": regressions,
        "warnings": warnings,
        "info": info,
        "rows": [],
        "failed": any_fail,
        "baseline_json": Value::Null,
    });
    Done {
        value,
        table: lines.join("\n"),
        failed: any_fail,
    }
}

struct Check {
    metric: &'static str,
    value: f64,
    limit: f64,
    passed: bool,
}

fn checks_for(metrics: &Metrics, limits: &toml::Value) -> Vec<Check> {
    let specs = [
        ("fps_min", "fps", true, metrics.fps),
        ("frame_ms_p50_max", "frame p50", false, metrics.p50),
        ("frame_ms_p95_max", "frame p95", false, metrics.p95),
        ("frame_ms_p99_max", "frame p99", false, metrics.p99),
        ("update_ms_p50_max", "update p50", false, metrics.update_p50),
        ("render_ms_p50_max", "render p50", false, metrics.render_p50),
        (
            "outside_update_ms_p50_max",
            "outside p50",
            false,
            metrics.outside_p50,
        ),
        ("gpu_ms_p50_max", "gpu p50", false, metrics.gpu_p50),
        (
            "hitches_per_minute_max",
            "hitches per minute",
            false,
            metrics.hitches_per_min,
        ),
        ("rss_peak_mb_max", "peak RSS", false, metrics.rss_mb),
        ("vram_peak_mb_max", "peak VRAM", false, metrics.vram_mb),
    ];
    let mut checks = Vec::new();
    for (key, metric, lower_bound, value) in specs {
        let Some(value) = value else {
            continue;
        };
        let Some(limit) = toml_key(limits, key).and_then(toml_f64) else {
            continue;
        };
        let passed = if lower_bound {
            value >= limit
        } else {
            value <= limit
        };
        checks.push(Check {
            metric,
            value,
            limit,
            passed,
        });
    }
    checks
}

struct GateSinks<'a> {
    regressions: &'a mut Vec<Value>,
    table_rows: &'a mut Vec<String>,
    any_fail: &'a mut bool,
}

fn push_regression(
    map: &str,
    metric: &str,
    current: Option<f64>,
    baseline: Option<f64>,
    limit_pct: f64,
    sinks: &mut GateSinks<'_>,
) {
    let (Some(current), Some(baseline)) = (current, baseline) else {
        return;
    };
    if baseline <= 0.0 || !current.is_finite() {
        return;
    }
    let delta = (current - baseline) / baseline * 100.0;
    let passed = delta <= limit_pct;
    if !passed {
        *sinks.any_fail = true;
    }
    sinks.table_rows.push(format!(
        "{:<16} {:<22} {:>10.3} {:>10.3} {}",
        map,
        metric,
        current,
        limit_pct,
        pass_word(passed)
    ));
    sinks.regressions.push(json!({
        "map": map,
        "metric": metric,
        "current": current,
        "baseline": baseline,
        "delta_pct": delta,
        "pass": passed,
    }));
}

fn metrics_of(bucket: &Bucket, hitch_ms: f64) -> Metrics {
    if !bucket.frames.is_empty() {
        let frames = sorted(&bucket.frames);
        let sum: f64 = bucket.frames.iter().sum();
        let hitches = bucket.frames.iter().filter(|ms| **ms >= hitch_ms).count() as f64;
        return Metrics {
            n: bucket.frames.len() as u64,
            fps: (sum > 0.0).then_some(1000.0 / (sum / bucket.frames.len() as f64)),
            p50: percentile_linear(&frames, 0.50),
            p95: percentile_linear(&frames, 0.95),
            p99: percentile_linear(&frames, 0.99),
            update_p50: median(&bucket.update),
            render_p50: median(&bucket.render),
            outside_p50: median(&bucket.outside),
            gpu_p50: median(&bucket.gpu),
            hitches_per_min: (sum > 0.0).then_some(hitches * 60_000.0 / sum),
            rss_mb: bucket.rss_mb,
            vram_mb: bucket.vram_mb,
        };
    }
    let telem = bucket.telem.as_ref();
    let p50 = telem.and_then(|telem| telem.p50);
    let avg = telem.and_then(|telem| telem.avg).or(p50);
    let hitches_per_min = telem.and_then(|telem| match (telem.hitches, telem.seconds) {
        (Some(hitches), Some(seconds)) if seconds > 0.0 => Some(hitches / seconds * 60.0),
        _ => None,
    });
    Metrics {
        n: telem.map(|telem| telem.n).unwrap_or(0),
        fps: avg.filter(|value| *value > 0.0).map(|value| 1000.0 / value),
        p50,
        p95: telem.and_then(|telem| telem.p95),
        p99: telem.and_then(|telem| telem.p99),
        update_p50: None,
        render_p50: None,
        outside_p50: None,
        gpu_p50: None,
        hitches_per_min,
        rss_mb: bucket.rss_mb,
        vram_mb: bucket.vram_mb,
    }
}

fn stats_value(metrics: &Metrics, bucket: &Bucket) -> Value {
    let frame = if bucket.frames.is_empty() {
        json!({
            "n": metrics.n,
            "fps_avg": metrics.fps,
            "p50": metrics.p50,
            "p95": metrics.p95,
            "p99": metrics.p99,
            "median": metrics.p50,
        })
    } else {
        series_json(&bucket.frames, metrics.fps)
    };
    json!({
        "frame_ms": frame,
        "update_ms": series_json(&bucket.update, None),
        "render_ms": series_json(&bucket.render, None),
        "outside_update_ms": series_json(&bucket.outside, None),
        "gpu_ms": series_json(&bucket.gpu, None),
    })
}

fn series_json(values: &[f64], fps: Option<f64>) -> Value {
    let sorted = sorted(values);
    json!({
        "n": values.len(),
        "fps_avg": fps,
        "p50": percentile_linear(&sorted, 0.50),
        "p95": percentile_linear(&sorted, 0.95),
        "p99": percentile_linear(&sorted, 0.99),
        "median": percentile_linear(&sorted, 0.50),
    })
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        percentile_linear(&sorted(values), 0.50)
    }
}

fn sorted(values: &[f64]) -> Vec<f64> {
    let mut copy = values.to_vec();
    copy.sort_by(f64::total_cmp);
    copy
}

fn percentile_linear(sorted: &[f64], frac: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let rank = (sorted.len() - 1) as f64 * frac;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let weight = rank - lo as f64;
    Some(sorted[lo] * (1.0 - weight) + sorted[hi] * weight)
}

fn lookup_limits<'a>(
    doc: &'a toml::Value,
    res: &str,
    tier: &str,
    map: &str,
) -> (Option<&'a toml::Value>, bool) {
    let Some(classes) = doc.get("classes").and_then(toml::Value::as_table) else {
        return (None, false);
    };
    let Some(class) = table_get(classes, res).and_then(toml::Value::as_table) else {
        return (None, false);
    };
    let Some(tier_table) = table_get(class, tier).and_then(toml::Value::as_table) else {
        return (None, false);
    };
    if let Some(row) = table_get(tier_table, map) {
        return (Some(row), false);
    }
    (table_get(tier_table, "default"), true)
}

fn table_get<'a>(table: &'a toml::map::Map<String, toml::Value>, key: &str) -> Option<&'a toml::Value> {
    table.get(key).or_else(|| {
        table
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    })
}

fn toml_key<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    value.get(key)
}

fn toml_f64(value: &toml::Value) -> Option<f64> {
    value
        .as_float()
        .or_else(|| value.as_integer().map(|item| item as f64))
        .filter(|item| item.is_finite())
}

fn class_from_pixels(width: f64, height: f64) -> String {
    let width = width.round() as i64;
    let height = height.round() as i64;
    if (width, height) == (1920, 1080) {
        return "1080p".to_owned();
    }
    if (width, height) == (2560, 1440) {
        return "1440p".to_owned();
    }
    if (width, height) == (3840, 1600) || (width, height) == (3440, 1440) {
        return "ultrawide".to_owned();
    }
    if width >= 3440 && height > 0 && width as f64 / height as f64 > 1.9 {
        return "ultrawide".to_owned();
    }
    if height >= 1400 || width >= 2400 {
        "1440p".to_owned()
    } else {
        "1080p".to_owned()
    }
}

fn normalize_res(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1080" | "1080p" | "fhd" => "1080p".to_owned(),
        "1440" | "1440p" | "qhd" => "1440p".to_owned(),
        "uw" | "ultrawide" | "ultra-wide" | "ultrawidep" => "ultrawide".to_owned(),
        other => other.to_owned(),
    }
}

fn normalize_tier(raw: &str) -> String {
    raw.trim().to_ascii_uppercase()
}

fn pass_word(passed: bool) -> &'static str {
    if passed { "PASS" } else { "FAIL" }
}

fn flag(obj: &Map<String, Value>, key: &str) -> bool {
    obj.get(key).and_then(Value::as_bool) == Some(true)
}

fn text_of<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| obj.get(*key).and_then(Value::as_str))
}

fn num_of(obj: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        obj.get(*key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
    })
}

fn rpc(code: &'static str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "see book/agents/perf-gate.md")
}

fn optional_string(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, RpcError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(rpc("logs_perf_gate_input", format!("{key} must be a string"))),
    }
}

fn required_string(obj: &Map<String, Value>, key: &str) -> Result<String, RpcError> {
    optional_string(obj, key)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| rpc("logs_perf_gate_thresholds", "thresholds text is required"))
}

fn optional_number(obj: &Map<String, Value>, key: &str) -> Result<Option<f64>, RpcError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|number| number.is_finite())
            .map(Some)
            .ok_or_else(|| rpc("logs_perf_gate_input", format!("{key} must be a finite number"))),
    }
}
