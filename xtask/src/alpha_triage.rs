// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask alpha-triage <dir-or-files...>`: turn DO27 alpha session logs into a triage
//! table (ADR-159/ADR-178 telemetry; `book/user-guide/telemetry.md`).
//!
//! Inputs are Regia Engine telemetry JSONL files (one line per `tracing` event, `fields.data`
//! holding the record: a nested object, or the older JSON string, or `fields.data_json`; spec 28) and feedback/crash bundles (`regia_framework::feedback::bundle.rs`
//! layout: `feedback.json`, `crash.txt`, `host.json`, `minidump.dmp`). Directories are walked
//! for both. Parsing tolerates unknown fields and a truncated last line; a log with no
//! `regia::*` telemetry at all still produces a row (header facts are recovered from
//! `regia_render` / `regia_platform` text lines).
//!
//! A session's `summary.json` (or `summary.partial.json`, when the session never reached a clean
//! exit) is read first (spec 28 § 4): the machine, settings, per-map frame times, stalls, exit and
//! the previous session's exit. The JSONL then supplies the detail, plus the `regia::phase`
//! timeline (boot, shaders, scene loads, quit). A summary with no log beside it is a session row of
//! its own.
//!
//! Session frame-time percentiles come from the summaries' `fps_histogram` buckets merged over
//! the session (bucket edges 240/144/120/90/60/45/30/20/10 fps, linear interpolation inside a
//! bucket), so they are estimates at bucket resolution; the per-window `frame_ms` percentiles
//! in the log are the exact numbers. Dev tool only: nothing here runs the engine.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde::Serialize;
use serde_json::Value;

/// `render_split.acquire` above this (ms) is a suspected engine issue.
const ACQUIRE_MS: f64 = 5.0;
/// `host_spans.main.unattributed_ms` above this (ms) is a suspected engine issue.
const UNATTRIBUTED_MS: f64 = 20.0;
/// "Over 33 ms" reads the fps<30 buckets (edge 33.33 ms); "over 50 ms" the fps<20 ones.
const OVER_A_MS: f64 = 33.333;
const OVER_B_MS: f64 = 50.0;
/// fps histogram buckets, fastest first, with their ms ranges. The slowest is capped at 200 ms.
const BUCKETS_MS: [[f64; 2]; 10] = [
    [0.0, 1000.0 / 240.0],
    [1000.0 / 240.0, 1000.0 / 144.0],
    [1000.0 / 144.0, 1000.0 / 120.0],
    [1000.0 / 120.0, 1000.0 / 90.0],
    [1000.0 / 90.0, 1000.0 / 60.0],
    [1000.0 / 60.0, 1000.0 / 45.0],
    [1000.0 / 45.0, 1000.0 / 30.0],
    [1000.0 / 30.0, 1000.0 / 20.0],
    [1000.0 / 20.0, 100.0],
    [100.0, 200.0],
];
/// The `fps_histogram` key of each bucket, in [`BUCKETS_MS`] order.
const FPS_KEYS: [&str; 10] = [
    "ge_240", "ge_144", "ge_120", "ge_90", "ge_60", "ge_45", "ge_30", "ge_20", "ge_10", "lt_10",
];

/// Command-line arguments of `alpha-triage`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Session log files (`*.jsonl`), feedback bundles (`*.zip`), or directories scanned
    /// recursively for both.
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,
    /// Write the markdown report to this file instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Also write a JSON summary: to `<out>.json` when `--out` is given, else to stdout
    /// (instead of the markdown).
    #[arg(long)]
    pub json: bool,
}

// ---------------------------------------------------------------------------
// Small JSON helpers (every field optional: unknown or absent stays unknown)
// ---------------------------------------------------------------------------

fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}
fn u(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}
fn st<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}
fn arr<'a>(v: &'a Value, key: &str) -> Option<&'a [Value]> {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice)
}
fn sub<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|x| !x.is_null())
}

/// An event's `fields.data` as a value: the nested object (spec 28), else the older JSON string,
/// else `fields.data_json`.
fn event_data(event: &Value) -> Option<Value> {
    let fields = event.get("fields")?;
    for key in ["data", "data_json"] {
        match fields.get(key) {
            Some(v @ (Value::Object(_) | Value::Array(_))) => return Some(v.clone()),
            Some(Value::String(text)) => {
                if let Ok(v @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str::<Value>(text) {
                    return Some(v);
                }
            }
            _ => {}
        }
    }
    None
}

/// A weighted mean accumulator.
#[derive(Default, Clone, Copy)]
struct WMean {
    sum: f64,
    weight: f64,
}

impl WMean {
    fn add(&mut self, value: f64, weight: f64) {
        if weight > 0.0 && value.is_finite() {
            self.sum += value * weight;
            self.weight += weight;
        }
    }
    fn get(&self) -> Option<f64> {
        (self.weight > 0.0).then(|| self.sum / self.weight)
    }
}

/// Name → weighted mean, for top systems and host sections.
#[derive(Default)]
struct WMeans(BTreeMap<String, WMean>);

impl WMeans {
    fn add(&mut self, name: &str, value: f64, weight: f64) {
        self.0.entry(name.to_owned()).or_default().add(value, weight);
    }
    /// Sorted by mean ms, descending.
    fn top(&self, n: usize) -> Vec<NameMs> {
        let mut all: Vec<NameMs> = self
            .0
            .iter()
            .filter_map(|(k, m)| {
                Some(NameMs {
                    name: k.clone(),
                    ms: m.get()?,
                })
            })
            .collect();
        all.sort_by(|a, b| b.ms.total_cmp(&a.ms).then(a.name.cmp(&b.name)));
        all.truncate(n);
        all
    }
}

/// A named value in milliseconds (top systems, host sections).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct NameMs {
    name: String,
    ms: f64,
}

/// A named counter (hitch causes, stream kinds).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct NameCount {
    name: String,
    count: u64,
}

/// A problem code with its event count and last message.
#[derive(Debug, Clone, Serialize)]
struct ProblemCode {
    code: String,
    events: u64,
    message: String,
}

/// A `churn.created_by_label` aggregate.
#[derive(Debug, Clone, Serialize)]
struct CreationCount {
    kind: String,
    label: String,
    count: u64,
    bytes: u64,
}

/// One suspected engine issue found in one session.
#[derive(Debug, Clone, Serialize)]
struct Suspect {
    check: String,
    session: String,
    path: String,
    evidence: String,
}

// ---------------------------------------------------------------------------
// Merged fps histogram
// ---------------------------------------------------------------------------

/// A session's frames merged into the telemetry `fps_histogram` buckets.
#[derive(Default, Clone, Debug, Serialize)]
struct FpsHisto {
    counts: [u64; 10],
}

impl FpsHisto {
    fn from_json(data: &Value) -> Self {
        let mut h = Self::default();
        if let Some(map) = data.get("fps_histogram").and_then(Value::as_object) {
            for (i, key) in FPS_KEYS.iter().enumerate() {
                h.counts[i] = map.get(*key).and_then(Value::as_u64).unwrap_or(0);
            }
        }
        h
    }
    fn merge(&mut self, other: &Self) {
        for (a, b) in self.counts.iter_mut().zip(other.counts) {
            *a = a.saturating_add(b);
        }
    }
    fn total(&self) -> u64 {
        self.counts.iter().sum()
    }
    /// Estimated percentile (q in 0..=1) in ms, interpolated linearly inside its bucket. The
    /// slowest bucket is capped at 200 ms.
    fn percentile_ms(&self, q: f64) -> Option<f64> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let target = q.clamp(0.0, 1.0) * total as f64;
        let mut seen = 0u64;
        for (i, &count) in self.counts.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let cumulative = seen + count;
            if target <= cumulative as f64 {
                let [lo, hi] = BUCKETS_MS[i];
                let frac = (target - seen as f64) / count as f64;
                return Some(lo + frac.clamp(0.0, 1.0) * (hi - lo));
            }
            seen = cumulative;
        }
        Some(BUCKETS_MS[9][1])
    }
    /// Share of frames (percent) slower than `ms`; the bucket containing `ms` is split
    /// linearly.
    fn pct_over_ms(&self, ms: f64) -> Option<f64> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let mut over = 0.0f64;
        for (i, &count) in self.counts.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let [lo, hi] = BUCKETS_MS[i];
            if lo >= ms {
                over += count as f64;
            } else if hi > ms && hi > lo {
                over += count as f64 * ((hi - ms) / (hi - lo));
            }
        }
        Some(over * 100.0 / total as f64)
    }
}

// ---------------------------------------------------------------------------
// GPU naming and settings extraction
// ---------------------------------------------------------------------------

/// The vendor behind a PCI id or adapter name.
fn gpu_vendor(pci: Option<&str>, name: Option<&str>) -> &'static str {
    let by_id = pci.and_then(|p| u32::from_str_radix(p.trim_start_matches("0x"), 16).ok());
    match by_id {
        Some(0x10de) => return "nvidia",
        Some(0x1002) => return "amd",
        Some(0x8086) => return "intel",
        _ => {}
    }
    let name = name.unwrap_or_default().to_ascii_lowercase();
    if name.contains("nvidia") || name.contains("geforce") || name.contains("rtx") {
        "nvidia"
    } else if name.contains("amd") || name.contains("radeon") || name.contains("radv") {
        "amd"
    } else if name.contains("intel") || name.contains("arc") {
        "intel"
    } else if name.is_empty() {
        "unknown"
    } else {
        "other"
    }
}

/// A short family name from the adapter name: `"AMD Radeon RX 7900 XTX (RADV NAVI31)"` →
/// `"RX 7900 XTX"`, `"NVIDIA GeForce RTX 5090"` → `"RTX 5090"`. Parentheses are dropped, then
/// vendor tokens are stripped while what remains still carries a model number.
fn gpu_family(name: &str) -> String {
    let unmarked = name
        .replace("(R)", "")
        .replace("(r)", "")
        .replace("(TM)", "")
        .replace("(tm)", "");
    let bare: String = unmarked
        .split(" (")
        .next()
        .unwrap_or(&unmarked)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned();
    if bare.is_empty() {
        return "unknown".into();
    }
    const STRIP: [&str; 6] = ["NVIDIA", "GeForce", "AMD", "Radeon", "Intel", "Arc"];
    let words: Vec<&str> = bare.split_whitespace().collect();
    let mut best_digit = None;
    let mut rest: Vec<&str> = words.clone();
    while let Some(first) = rest.first() {
        if STRIP.contains(first) {
            rest.remove(0);
            if !rest.is_empty() && rest.iter().any(|w| w.chars().any(|c| c.is_ascii_digit())) {
                best_digit = Some(rest.join(" "));
            }
        } else {
            break;
        }
    }
    if let Some(family) = best_digit {
        return family;
    }
    // No model number survived: keep everything but the bare company names.
    let kept: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| !matches!(*w, "NVIDIA" | "AMD" | "Intel"))
        .collect();
    if kept.is_empty() { bare } else { kept.join(" ") }
}

/// `preset: Ultra` (RON) → `Ultra`. Quotes are stripped; the value ends at a comma, a newline
/// or the closing paren of the enclosing struct.
fn ron_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("{key}:");
    let idx = text.find(&needle)?;
    let rest = &text[idx + needle.len()..];
    let end = rest.find([',', '\n', ')']).unwrap_or(rest.len());
    let value = rest[..end].trim().trim_matches('"').trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// The first candidate key in `settings_ron` that holds a value.
fn ron_setting(text: Option<&str>, keys: &[&str]) -> Option<String> {
    text.and_then(|t| keys.iter().find_map(|k| ron_field(t, k)))
}

// ---------------------------------------------------------------------------
// Session aggregation
// ---------------------------------------------------------------------------

/// Frame-path creation counters.
#[derive(Default, Clone, Copy, Debug, Serialize)]
struct Created {
    pipelines: u64,
    bind_groups: u64,
    buffers: u64,
    textures: u64,
    blocking_polls: u64,
    inline_bakes: u64,
}

impl Created {
    fn from_json(v: &Value) -> Self {
        Self {
            pipelines: u(v, "pipelines").unwrap_or(0),
            bind_groups: u(v, "bind_groups").unwrap_or(0),
            buffers: u(v, "buffers").unwrap_or(0),
            textures: u(v, "textures").unwrap_or(0),
            blocking_polls: u(v, "blocking_polls").unwrap_or(0),
            inline_bakes: u(v, "inline_bakes").unwrap_or(0),
        }
    }
    fn total(&self) -> u64 {
        self.pipelines + self.bind_groups + self.buffers + self.textures
    }
}

/// The last summary's scene counts.
#[derive(Debug, Clone, Serialize)]
struct SceneCounts {
    mesh_instances_mean: f64,
    drawn_mean: f64,
    local_lights_mean: f64,
    entities: u64,
}

/// Frame times of one map (`summary.json` `frames_by_map`).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct MapFrames {
    map: String,
    frames: Option<u64>,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    p99_ms: Option<f64>,
    over_33ms_pct: Option<f64>,
    over_50ms_pct: Option<f64>,
}

/// A frame of a second or more (`summary.json` `stalls`).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct StallRow {
    t_s: Option<f64>,
    ms: Option<f64>,
    phase: String,
}

/// How the previous session ended (`summary.json` `previous_exit`, or the
/// `previous_session_unclean` phase event).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct PreviousExitRow {
    clean: Option<bool>,
    last_partial_seconds: Option<f64>,
    last_partial_frames: Option<u64>,
}

/// One `regia::phase` event.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct PhaseRow {
    phase: String,
    t_s: Option<f64>,
    name: Option<String>,
    ms: Option<f64>,
}

/// Most phase events kept per session.
const MAX_PHASES: usize = 400;

/// Everything aggregated from one JSONL session log. This is also the JSON summary's
/// per-session record.
#[derive(Debug, Default, Serialize)]
struct Session {
    path: String,
    name: String,
    started_utc: Option<String>,
    duration_s: Option<f64>,

    gpu_vendor: String,
    gpu_family: String,
    gpu_name: String,
    gpu_driver: String,
    gpu_backend: String,
    gpu_device_type: String,
    os: String,
    cpu_threads: Option<u64>,
    regia_version: String,
    regia_git: String,
    regia_profile: String,
    pipeline_cache: String,
    preset: String,
    render_scale: String,
    aa: String,
    gi: String,
    resolution: Option<[u32; 2]>,
    refresh_hz: Option<f64>,

    frames: u64,
    histo: FpsHisto,
    frame_p50_ms: Option<f64>,
    frame_p95_ms: Option<f64>,
    frame_p99_ms: Option<f64>,
    frame_mean_ms: Option<f64>,
    frame_max_ms: Option<f64>,
    pct_over_33: Option<f64>,
    pct_over_50: Option<f64>,
    bound: String,

    hitches: u64,
    hitches_suppressed: u64,
    hitch_events: u64,
    worst_hitch_ms: Option<f64>,
    hitch_causes: Vec<NameCount>,
    hitch_top_spans: Vec<NameCount>,

    gpu_errors: u64,
    gpu_error_first: Vec<String>,
    device_lost: u64,
    device_lost_reasons: Vec<String>,
    surface: BTreeMap<String, u64>,
    surface_totals: BTreeMap<String, u64>,
    problems: Vec<ProblemCode>,
    streams: BTreeMap<String, u64>,
    slow_load_worst_ms: Option<f64>,
    slow_load_labels: Vec<String>,

    vram_peak_bytes: Option<u64>,
    vram_reserved_peak_bytes: Option<u64>,
    rss_peak_bytes: Option<u64>,

    after_warmup: Created,
    warm_windows: u64,
    mesh_buffers_created: u64,
    upload_per_frame_max_bytes: Option<f64>,
    creations: Vec<CreationCount>,

    top_systems: Vec<NameMs>,
    top_host_spans: Vec<NameMs>,
    top_host_other: Vec<NameMs>,
    unattributed_mean_ms: Option<f64>,
    unattributed_max_ms: Option<f64>,
    acquire_mean_ms: Option<f64>,
    acquire_max_ms: Option<f64>,

    counts_last: Option<SceneCounts>,

    /// `final` or `partial`, when a `summary.json` was read.
    summary_kind: Option<String>,
    summary_path: Option<String>,
    cpu_model: String,
    ram_gb: Option<f64>,
    vram_gb: Option<f64>,
    os_version: String,
    upscaler: String,
    auto_plan: String,
    maps: Vec<MapFrames>,
    stalls: Vec<StallRow>,
    previous_exit: Option<PreviousExitRow>,
    exit_clean: Option<bool>,
    exit_ms: Option<f64>,
    phases: Vec<PhaseRow>,
    boot_done_ms: Option<f64>,
    shaders_ready: Option<Value>,
    scene_loads: Vec<Value>,
    host: Option<Value>,
    shaders_backend: String,
    driver_cache: String,

    telemetry_events: u64,
    crate_lines: u64,
    bad_lines: u64,
    truncated_tail: bool,
    dropped_lines: u64,
    notes: Vec<String>,

    /// Worst-first rank among all sessions (filled by the report).
    rank: usize,
}

/// Mutable accumulators that fold into [`Session`].
#[derive(Default)]
struct Fold {
    mean: WMean,
    max_ms: Option<f64>,
    systems: WMeans,
    host_main: WMeans,
    host_other: WMeans,
    unattributed: WMean,
    unattributed_max: Option<f64>,
    acquire: WMean,
    acquire_max: Option<f64>,
    causes: BTreeMap<String, u64>,
    top_spans: BTreeMap<String, u64>,
    creations: BTreeMap<(String, String), CreationCount>,
    last_t_ms: Option<f64>,
}

/// Parses one JSONL session log. Never fails on content: bad lines are counted and a missing
/// telemetry header still yields a session row from crate text lines.
fn parse_session(path: &Path) -> Result<Session> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let ends_with_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let mut s = Session {
        path: path.display().to_string(),
        name: file_name(path),
        ..Session::default()
    };
    let mut fold = Fold::default();
    // Whether the `regia::session` header was seen (crate-line fallback stops then).
    let mut got_header = false;

    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            if i + 1 == lines.len() && !ends_with_newline {
                s.truncated_tail = true;
            } else {
                s.bad_lines += 1;
            }
            continue;
        };
        if let Some(t) = f(&event, "t_ms") {
            fold.last_t_ms = Some(fold.last_t_ms.map_or(t, |m| m.max(t)));
        }
        if s.started_utc.is_none() {
            s.started_utc = st(&event, "wall").map(str::to_owned);
        }
        // The v0.1.x alpha wrote the targets from before the rename (`<old name>::session`,
        // ADR-1429).
        let raw_target = st(&event, "target").unwrap_or_default();
        let current_target = regia_core::brand::current_log_target(raw_target);
        let target = current_target.as_deref().unwrap_or(raw_target);
        let data_owned = event_data(&event);
        let data = data_owned.as_ref();
        match target {
            "regia::session" => {
                // The telemetry header always wins over the crate-line fallback (its lines
                // can arrive earlier in the file).
                if !got_header {
                    session_header(&mut s, data);
                    got_header = true;
                }
                s.telemetry_events += 1;
            }
            "regia::frame_summary" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    fold_summary(&mut s, &mut fold, d);
                }
            }
            "regia::hitch" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    fold_hitch(&mut s, &mut fold, d);
                }
            }
            "regia::gpu_error" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    fold_gpu_error(&mut s, d);
                }
            }
            "regia::device_lost" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    s.device_lost += 1;
                    let reason = st(d, "reason").unwrap_or("unknown").to_owned();
                    if !s.device_lost_reasons.contains(&reason) {
                        s.device_lost_reasons.push(reason);
                    }
                }
            }
            "regia::surface" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    fold_surface(&mut s, d);
                }
            }
            "regia::problems" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    fold_problems(&mut s, d);
                }
            }
            "regia::stream" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    fold_stream(&mut s, d);
                }
            }
            "regia::phase" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    fold_phase(&mut s, f(&event, "t_ms"), d);
                }
            }
            "regia::exit" => {
                s.telemetry_events += 1;
                if let Some(d) = data {
                    s.exit_ms = s.exit_ms.or_else(|| f(d, "total_ms"));
                }
            }
            t if t.starts_with("regia::") => s.telemetry_events += 1,
            _ => {
                s.crate_lines += 1;
                crate_line_fallback(&mut s, st(&event, "msg").unwrap_or_default(), got_header);
            }
        }
    }

    if let Some(last) = fold.last_t_ms {
        s.duration_s = Some(last / 1000.0);
    }
    s.frames = s.histo.total();
    s.frame_p50_ms = s.histo.percentile_ms(0.50);
    s.frame_p95_ms = s.histo.percentile_ms(0.95);
    s.frame_p99_ms = s.histo.percentile_ms(0.99);
    s.pct_over_33 = s.histo.pct_over_ms(OVER_A_MS);
    s.pct_over_50 = s.histo.pct_over_ms(OVER_B_MS);
    s.frame_mean_ms = fold.mean.get();
    s.frame_max_ms = fold.max_ms;
    s.top_systems = fold.systems.top(8);
    s.top_host_spans = fold.host_main.top(6);
    s.top_host_other = fold.host_other.top(4);
    s.unattributed_mean_ms = fold.unattributed.get();
    s.unattributed_max_ms = fold.unattributed_max;
    s.acquire_mean_ms = fold.acquire.get();
    s.acquire_max_ms = fold.acquire_max;
    s.hitch_causes = top_counts(&fold.causes, 6);
    s.hitch_top_spans = top_counts(&fold.top_spans, 6);
    let mut creations: Vec<CreationCount> = fold.creations.into_values().collect();
    creations.sort_by(|a, b| b.count.cmp(&a.count).then(b.bytes.cmp(&a.bytes)));
    creations.truncate(10);
    s.creations = creations;
    if s.gpu_name.is_empty() {
        s.gpu_vendor = "unknown".into();
        s.gpu_family = "unknown".into();
    } else if s.gpu_family.is_empty() {
        s.gpu_family = "unknown".into();
    }
    if s.telemetry_events == 0 {
        s.notes.push(
            "no regia::* telemetry events in this log (header facts recovered from crate text lines)".into(),
        );
    }
    Ok(s)
}

/// The file name, or the whole path when there is none.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn top_counts(map: &BTreeMap<String, u64>, n: usize) -> Vec<NameCount> {
    let mut all: Vec<NameCount> = map
        .iter()
        .map(|(k, v)| NameCount {
            name: k.clone(),
            count: *v,
        })
        .collect();
    all.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
    all.truncate(n);
    all
}

/// Header facts from `regia::session`.
fn session_header(s: &mut Session, data: Option<&Value>) {
    let Some(d) = data else { return };
    if let Some(adapter) = sub(d, "adapter") {
        let name = st(adapter, "name").unwrap_or_default();
        s.gpu_name = name.to_owned();
        s.gpu_vendor = gpu_vendor(st(adapter, "vendor"), Some(name)).to_owned();
        s.gpu_family = gpu_family(name);
        s.gpu_driver = format!(
            "{} {}",
            st(adapter, "driver").unwrap_or("?"),
            st(adapter, "driver_info").unwrap_or("")
        )
        .trim()
        .to_owned();
        s.gpu_backend = st(adapter, "backend").unwrap_or_default().to_owned();
        s.gpu_device_type = st(adapter, "device_type").unwrap_or_default().to_owned();
    }
    if let Some(os) = sub(d, "os") {
        s.os = format!(
            "{} {}",
            st(os, "family").unwrap_or_default(),
            st(os, "arch").unwrap_or_default()
        )
        .trim()
        .to_owned();
    }
    s.cpu_threads = u(d, "cpu_threads");
    s.cpu_model = st(d, "cpu").unwrap_or_default().to_owned();
    s.ram_gb = f(d, "ram_gb");
    s.vram_gb = f(d, "vram_gb");
    s.os_version = st(d, "os_version").unwrap_or_default().to_owned();
    if let Some(regia) = sub(d, "regia").or_else(|| sub(d, regia_core::brand::LEGACY_MAIN_CRATE)) {
        s.regia_version = st(regia, "version").unwrap_or_default().to_owned();
        s.regia_git = st(regia, "git").unwrap_or_default().to_owned();
        s.regia_profile = st(regia, "profile").unwrap_or_default().to_owned();
    }
    if let Some(window) = sub(d, "window")
        && let Some(size) = window.get("physical_size").and_then(Value::as_array)
        && size.len() >= 2
        && let (Some(w), Some(h)) = (size[0].as_u64(), size[1].as_u64())
    {
        let (w, h) = (w as u32, h as u32);
        if w > 0 && h > 0 {
            s.resolution = Some([w, h]);
        }
    }
    if let Some(monitors) = arr(d, "monitors") {
        s.refresh_hz = monitors
            .iter()
            .find(|m| m.get("current").and_then(Value::as_bool).unwrap_or(false))
            .and_then(|m| m.get("refresh_hz").and_then(Value::as_f64));
    }
    if let Some(cache) = sub(d, "pipeline_cache") {
        let load = st(cache, "load").unwrap_or_default().to_owned();
        s.pipeline_cache = match f(cache, "load_ms") {
            Some(ms) => format!("{load} ({ms:.1} ms)"),
            None => load,
        };
    }
    let settings = st(d, "settings_ron");
    s.preset = ron_setting(settings, &["preset"]).unwrap_or_default();
    s.render_scale = ron_setting(settings, &["render_scale", "scale"]).unwrap_or_default();
    s.aa = ron_setting(settings, &["aa", "anti_aliasing", "anti_alias"]).unwrap_or_default();
    s.gi = ron_setting(settings, &["gi", "gi_mode", "gi_method"]).unwrap_or_default();
}

/// Header facts recovered from crate text lines in logs without telemetry events, plus two
/// text-line counters (`pipeline created outside the warm queue`, `dropped lines`) that stay
/// useful even beside telemetry. Header facts stop once `header_done` (a real
/// `regia::session` event arrived and overwrote them).
fn crate_line_fallback(s: &mut Session, msg: &str, header_done: bool) {
    if !header_done {
        if let Some(rest) = msg.strip_prefix("GPU: ") {
            // "GPU: AMD Radeon RX 7900 XTX (RADV NAVI31) (Vulkan, DiscreteGpu)"
            let name = rest.split(" (").next().unwrap_or(rest).trim();
            if s.gpu_name.is_empty() && !name.is_empty() {
                s.gpu_name = name.to_owned();
                s.gpu_vendor = gpu_vendor(None, Some(name)).to_owned();
                s.gpu_family = gpu_family(name);
            }
            if s.gpu_backend.is_empty()
                && let Some(open) = rest.rfind('(')
            {
                let tail = rest[open + 1..].trim_end_matches(')');
                let mut parts = tail.split(',').map(str::trim);
                if let Some(backend) = parts.next() {
                    s.gpu_backend = backend.to_owned();
                }
                if let Some(device_type) = parts.next() {
                    s.gpu_device_type = device_type.to_owned();
                }
            }
        } else if msg.starts_with("os: ") && s.os.is_empty() {
            s.os = msg[4..].split(" |").next().unwrap_or(&msg[4..]).trim().to_owned();
        } else if msg.starts_with("configure ") {
            if let Some(dim) = msg.strip_prefix("configure ")
                && let Some((w, h)) = dim.trim().split_once('x')
                && let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>())
            {
                s.resolution = Some([w, h]);
            }
        } else if msg.starts_with("window created: ")
            && s.resolution.is_none()
            && let Some(dim) = msg["window created: ".len()..].split_whitespace().next()
            && let Some((w, h)) = dim.split_once('x')
            && let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>())
        {
            s.resolution = Some([w, h]);
        }
    }
    if msg.starts_with("pipeline created outside the warm queue") {
        // The un-rate-limited twin of the telemetry `pipeline_after_warmup` event.
        *s.streams
            .entry("pipeline_after_warmup_log_lines".to_owned())
            .or_default() += 1;
    } else if msg.starts_with("dropped lines") {
        s.dropped_lines += 1;
    }
}

/// Folds one `regia::phase` event.
fn fold_phase(s: &mut Session, t_ms: Option<f64>, d: &Value) {
    let phase = st(d, "phase").unwrap_or("unknown").to_owned();
    match phase.as_str() {
        "boot_done" => s.boot_done_ms = f(d, "ms"),
        "shaders_ready" => s.shaders_ready = Some(d.clone()),
        "scene_load_end" if s.scene_loads.len() < 64 => s.scene_loads.push(d.clone()),
        "previous_session_unclean" if s.previous_exit.is_none() => {
            s.previous_exit = Some(PreviousExitRow {
                clean: Some(false),
                last_partial_seconds: f(d, "last_partial_session_seconds"),
                last_partial_frames: None,
            });
        }
        _ => {}
    }
    if s.phases.len() < MAX_PHASES {
        s.phases.push(PhaseRow {
            phase,
            t_s: t_ms.map(|t| t / 1000.0),
            name: st(d, "name").map(str::to_owned),
            ms: f(d, "ms"),
        });
    }
}

/// Folds one `summary.json` / `summary.partial.json` record into `s`. Header facts the JSONL gave
/// win; the summary fills what is empty, and brings the sections only it has.
fn apply_summary(s: &mut Session, v: &Value, partial_file: bool, path: &Path) {
    s.summary_kind = Some(
        if partial_file || v.get("partial").and_then(Value::as_bool) == Some(true) {
            "partial"
        } else {
            "final"
        }
        .to_owned(),
    );
    s.summary_path = Some(path.display().to_string());
    let fill = |dst: &mut String, src: Option<&str>| {
        if dst.is_empty()
            && let Some(x) = src
        {
            x.clone_into(dst);
        }
    };
    if let Some(regia) = sub(v, "regia").or_else(|| sub(v, regia_core::brand::LEGACY_MAIN_CRATE)) {
        fill(&mut s.regia_version, st(regia, "version"));
        fill(&mut s.regia_git, st(regia, "git"));
        fill(&mut s.regia_profile, st(regia, "profile"));
    }
    if let Some(sys) = sub(v, "system") {
        let had_gpu = !s.gpu_name.is_empty();
        fill(&mut s.gpu_name, st(sys, "gpu"));
        if !had_gpu && let Some(name) = st(sys, "gpu") {
            s.gpu_vendor = gpu_vendor(st(sys, "vendor"), Some(name)).to_owned();
            s.gpu_family = gpu_family(name);
        }
        fill(&mut s.gpu_driver, st(sys, "driver"));
        fill(&mut s.gpu_backend, st(sys, "backend"));
        fill(&mut s.os, st(sys, "os"));
        fill(&mut s.os_version, st(sys, "os"));
        fill(&mut s.cpu_model, st(sys, "cpu"));
        s.cpu_threads = s.cpu_threads.or_else(|| u(sys, "cpu_threads"));
        s.ram_gb = s.ram_gb.or_else(|| f(sys, "ram_gb"));
        s.vram_gb = s.vram_gb.or_else(|| f(sys, "vram_gb"));
    }
    if let Some(display) = sub(v, "display") {
        if s.resolution.is_none()
            && let (Some(w), Some(h)) = (u(display, "width"), u(display, "height"))
            && w > 0
            && h > 0
        {
            s.resolution = Some([w as u32, h as u32]);
        }
        s.refresh_hz = s.refresh_hz.or_else(|| f(display, "refresh_hz"));
    }
    if let Some(settings) = sub(v, "settings") {
        fill(&mut s.preset, st(settings, "preset"));
        if s.render_scale.is_empty()
            && let Some(x) = f(settings, "render_scale")
        {
            s.render_scale = format!("{x}");
        }
        fill(&mut s.aa, st(settings, "aa"));
        fill(&mut s.gi, st(settings, "gi"));
        fill(&mut s.upscaler, st(settings, "upscaler"));
        if let Some(plan) = sub(settings, "auto_plan") {
            s.auto_plan = st(plan, "reason")
                .or_else(|| st(plan, "tier"))
                .map_or_else(|| plan.to_string(), str::to_owned);
        }
    }
    if let Some(session) = sub(v, "session") {
        s.duration_s = s.duration_s.or_else(|| f(session, "seconds"));
    }
    // Frame times from the summary when the log has no frame summaries (a hung session's log may
    // be cut off; the partial still has the numbers).
    if s.histo.total() == 0
        && let Some(frames) = sub(v, "frames")
    {
        s.frames = sub(v, "session").and_then(|x| u(x, "frames")).unwrap_or(0);
        s.frame_p50_ms = f(frames, "p50_ms");
        s.frame_p95_ms = f(frames, "p95_ms");
        s.frame_p99_ms = f(frames, "p99_ms");
        s.pct_over_33 = f(frames, "over_33ms_pct");
        s.pct_over_50 = f(frames, "over_50ms_pct");
        if s.bound.is_empty() {
            st(frames, "bound").unwrap_or_default().clone_into(&mut s.bound);
        }
    }
    if let Some(by_map) = v.get("frames_by_map").and_then(Value::as_object) {
        s.maps = by_map
            .iter()
            .map(|(map, x)| MapFrames {
                map: map.clone(),
                frames: u(x, "frames"),
                p50_ms: f(x, "p50_ms"),
                p95_ms: f(x, "p95_ms"),
                p99_ms: f(x, "p99_ms"),
                over_33ms_pct: f(x, "over_33ms_pct"),
                over_50ms_pct: f(x, "over_50ms_pct"),
            })
            .collect();
    }
    if let Some(list) = arr(v, "stalls") {
        s.stalls = list
            .iter()
            .map(|x| StallRow {
                t_s: f(x, "t_s"),
                ms: f(x, "ms"),
                phase: st(x, "phase").unwrap_or_default().to_owned(),
            })
            .collect();
    }
    if let Some(prev) = sub(v, "previous_exit") {
        let last = sub(prev, "last_partial");
        s.previous_exit = Some(PreviousExitRow {
            clean: prev.get("clean").and_then(Value::as_bool),
            last_partial_seconds: last.and_then(|l| sub(l, "session")).and_then(|x| f(x, "seconds")),
            last_partial_frames: last.and_then(|l| sub(l, "session")).and_then(|x| u(x, "frames")),
        });
    }
    if let Some(exit) = sub(v, "exit") {
        s.exit_clean = exit.get("clean").and_then(Value::as_bool);
        s.exit_ms = f(exit, "ms").or(s.exit_ms);
    }
    if s.hitch_causes.is_empty()
        && let Some(list) = sub(v, "hitches").and_then(|h| arr(h, "top_causes"))
    {
        s.hitch_causes = list
            .iter()
            .filter_map(|pair| match pair.as_array()?.as_slice() {
                [name, count, ..] => Some((name.as_str()?.to_owned(), count.as_u64()?)),
                _ => None,
            })
            .map(|(name, count)| NameCount { name, count })
            .collect();
    }
    if let Some(mem) = sub(v, "memory") {
        let bytes = |gb: f64| (gb * f64::from(1u32 << 30)) as u64;
        s.vram_peak_bytes = s.vram_peak_bytes.or_else(|| f(mem, "vram_peak_gb").map(bytes));
        s.rss_peak_bytes = s.rss_peak_bytes.or_else(|| f(mem, "rss_peak_gb").map(bytes));
    }
    if let Some(shaders) = sub(v, "shaders") {
        fill(&mut s.shaders_backend, st(shaders, "backend"));
        fill(&mut s.driver_cache, st(shaders, "driver_cache"));
        fill(&mut s.pipeline_cache, st(shaders, "cache"));
    }
    if s.gpu_errors == 0
        && let Some(errors) = sub(v, "errors")
    {
        s.gpu_errors = u(errors, "gpu_errors").unwrap_or(0);
        s.device_lost = s.device_lost.max(u(errors, "device_lost").unwrap_or(0));
    }
    s.host = sub(v, "host").cloned();
}

/// Folds one `regia::frame_summary`.
fn fold_summary(s: &mut Session, fold: &mut Fold, d: &Value) {
    let frames = u(d, "frames").unwrap_or(0);
    let weight = frames.max(1) as f64;
    s.histo.merge(&FpsHisto::from_json(d));
    if let Some(fm) = sub(d, "frame_ms") {
        fold.mean.add(f(fm, "mean").unwrap_or(0.0), weight);
        let max = f(fm, "max").unwrap_or(0.0);
        fold.max_ms = Some(fold.max_ms.map_or(max, |m| m.max(max)));
    }
    s.hitches += u(d, "hitches").unwrap_or(0);
    s.hitches_suppressed += u(d, "hitches_suppressed").unwrap_or(0);
    s.dropped_lines += u(d, "notes_dropped").unwrap_or(0);
    s.bound = st(d, "bound").unwrap_or_default().to_owned();
    if let Some(mem) = sub(d, "memory") {
        s.vram_peak_bytes = peak(s.vram_peak_bytes, u(mem, "vram_allocated_bytes"));
        s.vram_reserved_peak_bytes = peak(s.vram_reserved_peak_bytes, u(mem, "vram_reserved_bytes"));
        s.rss_peak_bytes = peak(s.rss_peak_bytes, u(mem, "rss_peak_bytes"));
    }
    let warm = d.get("warm").and_then(Value::as_bool).unwrap_or(false);
    if warm {
        s.warm_windows += 1;
        if let Some(after) = sub(d, "created_after_warmup") {
            let created = Created::from_json(after);
            s.after_warmup.pipelines += created.pipelines;
            s.after_warmup.bind_groups += created.bind_groups;
            s.after_warmup.buffers += created.buffers;
            s.after_warmup.textures += created.textures;
            s.after_warmup.blocking_polls += created.blocking_polls;
            s.after_warmup.inline_bakes += created.inline_bakes;
        }
    }
    if let Some(churn) = sub(d, "churn") {
        s.mesh_buffers_created += u(churn, "mesh_buffers_created").unwrap_or(0);
        s.upload_per_frame_max_bytes = max_opt(
            s.upload_per_frame_max_bytes,
            churn.get("upload_bytes").and_then(|x| f(x, "per_frame")),
        );
        if let Some(list) = arr(churn, "created_by_label") {
            for entry in list {
                let kind = st(entry, "kind").unwrap_or("?").to_owned();
                let label = st(entry, "label").unwrap_or("?").to_owned();
                let count = u(entry, "count").unwrap_or(0);
                let bytes = u(entry, "bytes").unwrap_or(0);
                let slot = fold
                    .creations
                    .entry((kind.clone(), label.clone()))
                    .or_insert(CreationCount {
                        kind,
                        label,
                        count: 0,
                        bytes: 0,
                    });
                slot.count += count;
                slot.bytes += bytes;
            }
        }
    }
    if let Some(breakdown) = sub(d, "frame_breakdown_ms")
        && let Some(split) = breakdown.get("render_split").filter(|v| !v.is_null())
        && let Some(acquire) = f(split, "acquire")
    {
        fold.acquire.add(acquire, weight);
        fold.acquire_max = Some(fold.acquire_max.map_or(acquire, |m| m.max(acquire)));
    }
    if let Some(systems) = sub(d, "top_systems")
        && let Some(top) = arr(systems, "top")
    {
        for entry in top {
            if let (Some(name), Some(mean)) = (st(entry, "system"), f(entry, "mean_ms")) {
                fold.systems.add(name, mean, weight);
            }
        }
    }
    if let Some(host) = sub(d, "host_spans").filter(|v| !v.is_null()) {
        if let Some(main) = host.get("main").filter(|v| !v.is_null()) {
            if let Some(unattributed) = f(main, "unattributed_ms") {
                fold.unattributed.add(unattributed, weight);
                fold.unattributed_max = Some(
                    fold.unattributed_max
                        .map_or(unattributed, |m| m.max(unattributed)),
                );
            }
            if let Some(sections) = arr(main, "sections") {
                for sec in sections {
                    if let (Some(name), Some(mean)) = (st(sec, "span"), f(sec, "mean_ms")) {
                        fold.host_main.add(name, mean, weight);
                    }
                }
            }
        }
        if let Some(other) = host.get("other_threads").filter(|v| !v.is_null())
            && let Some(sections) = arr(other, "sections")
        {
            for sec in sections {
                if let (Some(name), Some(mean)) = (st(sec, "span"), f(sec, "mean_ms")) {
                    fold.host_other.add(name, mean, weight);
                }
            }
        }
    }
    if let Some(counts) = sub(d, "counts") {
        let pick = |key: &str| counts.get(key).and_then(|x| f(x, "mean")).unwrap_or(0.0);
        s.counts_last = Some(SceneCounts {
            mesh_instances_mean: pick("mesh_instances"),
            drawn_mean: pick("drawn"),
            local_lights_mean: pick("local_lights"),
            entities: u(counts, "entities").unwrap_or(0),
        });
    }
}

/// Span names that only wrap the whole frame: useless as a cause, skipped as the primary.
const GENERIC_SPANS: [&str; 2] = ["frame", "boot_splash"];

/// The dominant cause label of one hitch event, from its evidence lists.
fn hitch_cause(d: &Value) -> String {
    if let Some(first) = arr(d, "top_cpu_spans").and_then(|a| {
        a.iter()
            .filter_map(|x| st(x, "span"))
            .find(|name| !GENERIC_SPANS.contains(name))
    }) {
        return format!("span {first}");
    }
    if let Some(first) = arr(d, "top_systems")
        .and_then(|a| a.first())
        .and_then(|x| st(x, "system"))
    {
        return format!("system {first}");
    }
    if let Some(first) = arr(d, "top_gpu_passes")
        .and_then(|a| a.first())
        .and_then(|x| st(x, "pass"))
    {
        return format!("pass {first}");
    }
    let created = d.get("created").map(Created::from_json).unwrap_or_default();
    if created.total() > 0 {
        let label = d
            .get("created")
            .and_then(|c| arr(c, "labels"))
            .and_then(|list| list.first())
            .map(|l| {
                format!(
                    "{} {}",
                    st(l, "kind").unwrap_or("?"),
                    st(l, "label").unwrap_or("?")
                )
            });
        return match label {
            Some(l) => format!("create {l}"),
            None => format!("create {} device objects", created.total()),
        };
    }
    "unknown".to_owned()
}

/// Folds one `regia::hitch` event.
fn fold_hitch(s: &mut Session, fold: &mut Fold, d: &Value) {
    s.hitch_events += 1;
    let frame_ms = f(d, "frame_ms").unwrap_or(0.0);
    s.worst_hitch_ms = Some(s.worst_hitch_ms.map_or(frame_ms, |m| m.max(frame_ms)));
    *fold.causes.entry(hitch_cause(d)).or_default() += 1;
    if let Some(spans) = arr(d, "top_cpu_spans") {
        for span in spans {
            if let Some(name) = st(span, "span") {
                *fold.top_spans.entry(name.to_owned()).or_default() += 1;
            }
        }
    }
    if let Some(breakdown) = sub(d, "frame_breakdown_ms")
        && let Some(split) = breakdown.get("render_split").filter(|v| !v.is_null())
        && let Some(acquire) = f(split, "acquire")
    {
        fold.acquire_max = Some(fold.acquire_max.map_or(acquire, |m| m.max(acquire)));
    }
}

/// Folds one `regia::gpu_error`.
fn fold_gpu_error(s: &mut Session, d: &Value) {
    let repeated = u(d, "repeated").unwrap_or(0);
    s.gpu_errors += 1 + repeated;
    if s.gpu_error_first.len() < 5 {
        let kind = st(d, "kind").unwrap_or("?");
        let pass = st(d, "pass").unwrap_or("?");
        let msg: String = st(d, "message").unwrap_or("").chars().take(90).collect();
        let rep = if repeated > 0 {
            format!(" (+{repeated})")
        } else {
            String::new()
        };
        s.gpu_error_first.push(format!("{kind} in {pass}{rep}: {msg}"));
    }
}

/// Folds one `regia::surface` event.
fn fold_surface(s: &mut Session, d: &Value) {
    for key in ["outdated", "lost", "timeout", "other"] {
        if let Some(n) = u(d, key)
            && n > 0
        {
            *s.surface.entry(key.to_owned()).or_default() += n;
        }
    }
    if let Some(map) = d.as_object() {
        for (key, value) in map {
            if let Some(n) = value.as_u64()
                && key.starts_with("total_")
                && n > 0
            {
                *s.surface_totals.entry(key.clone()).or_default() += n;
            }
        }
    }
}

/// Folds one `regia::problems` event.
fn fold_problems(s: &mut Session, d: &Value) {
    let Some(list) = arr(d, "problems") else { return };
    for p in list {
        let code = st(p, "code").unwrap_or("?").to_owned();
        let message: String = st(p, "message").unwrap_or("").chars().take(140).collect();
        match s.problems.iter_mut().find(|x| x.code == code) {
            Some(entry) => {
                entry.events += 1;
                entry.message = message;
            }
            None => s.problems.push(ProblemCode {
                code,
                events: 1,
                message,
            }),
        }
    }
}

/// Folds one `regia::stream` event.
fn fold_stream(s: &mut Session, d: &Value) {
    let kind = st(d, "kind").unwrap_or("?").to_owned();
    let repeated = u(d, "repeated").unwrap_or(0);
    *s.streams.entry(kind.clone()).or_default() += 1 + repeated;
    match kind.as_str() {
        "slow_asset_load" => {
            let ms = f(d, "ms").unwrap_or(0.0);
            s.slow_load_worst_ms = Some(s.slow_load_worst_ms.map_or(ms, |m| m.max(ms)));
            if let Some(label) = st(d, "label")
                && !s.slow_load_labels.iter().any(|l| l == label)
                && s.slow_load_labels.len() < 8
            {
                s.slow_load_labels.push(label.to_owned());
            }
        }
        "eviction_storm" => {
            let rate = f(d, "evictions_per_s").unwrap_or(0.0);
            s.notes.push(format!("eviction storm: {rate:.0} evictions/s"));
        }
        _ => {}
    }
}

fn peak(current: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (current, value) {
        (Some(c), Some(v)) => Some(c.max(v)),
        (None, Some(v)) => Some(v),
        (c, None) => c,
    }
}

/// The larger of two optional values (`None` only when both are).
fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

// ---------------------------------------------------------------------------
// Crash bundles
// ---------------------------------------------------------------------------

/// One feedback/crash bundle with its bucket signature.
#[derive(Debug, Clone, Serialize)]
struct CrashBundle {
    path: String,
    name: String,
    category: String,
    time_utc: String,
    signature: String,
    top_frames: Vec<String>,
    map: String,
}

/// Reads one bundle and derives its signature. `None` when the zip is not a feedback bundle.
fn parse_bundle(path: &Path) -> Option<CrashBundle> {
    let entries = regia_framework::feedback::read_bundle(path).ok()?;
    let find = |name: &str| -> Option<String> {
        entries
            .iter()
            .find(|e| e.name == name)
            .and_then(|e| String::from_utf8(e.data.clone()).ok())
    };
    let meta_text = find("feedback.json")?;
    let meta: Value = serde_json::from_str(&meta_text).ok()?;
    let crash_txt = find("crash.txt").unwrap_or_default();
    let mut top_frames = backtrace_symbols(&crash_txt, 3);
    let crash = meta.get("crash").filter(|c| !c.is_null());
    let signature = match crash {
        Some(c) if c.get("native").and_then(Value::as_bool).unwrap_or(false) => format!(
            "{} {}",
            st(c, "signal").unwrap_or("?"),
            st(c, "reason").unwrap_or("")
        ),
        Some(c) => {
            let message = st(c, "message").unwrap_or("?").lines().next().unwrap_or("?");
            match top_frames.first() {
                Some(frame) => format!("panic: {message} — {frame}"),
                None => format!("panic: {message}"),
            }
        }
        None => "feedback (no crash)".to_owned(),
    };
    if top_frames.is_empty()
        && let Some(location) = crash.and_then(|c| st(c, "location"))
    {
        top_frames.push(location.to_owned());
    }
    Some(CrashBundle {
        path: path.display().to_string(),
        name: file_name(path),
        category: st(&meta, "category").unwrap_or("?").to_owned(),
        time_utc: st(&meta, "time_utc").unwrap_or_default().to_owned(),
        signature: signature.trim().to_owned(),
        top_frames,
        map: find("host.json")
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|h| st(&h, "map").map(str::to_owned))
            .unwrap_or_default(),
    })
}

/// The first `n` backtrace symbols of `crash.txt`, preferring Regia Engine frames.
fn backtrace_symbols(crash_txt: &str, n: usize) -> Vec<String> {
    let mut regia = Vec::new();
    let mut other = Vec::new();
    for line in crash_txt.lines() {
        let trimmed = line.trim_start();
        let Some((num, symbol)) = trimmed.split_once(": ") else {
            continue;
        };
        if !num.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let symbol = symbol.trim().to_owned();
        if symbol.is_empty() {
            continue;
        }
        if symbol.contains("regia") || symbol.contains(regia_core::brand::LEGACY_MAIN_CRATE) {
            if !regia.contains(&symbol) {
                regia.push(symbol);
            }
        } else if !other.contains(&symbol) && !symbol.starts_with("std::") {
            other.push(symbol);
        }
    }
    let mut all = regia;
    all.extend(other);
    all.truncate(n);
    all
}

// ---------------------------------------------------------------------------
// Input discovery
// ---------------------------------------------------------------------------

/// What a scan of the inputs found.
#[derive(Default)]
struct Scan {
    jsonls: Vec<PathBuf>,
    summaries: Vec<PathBuf>,
    bundles: Vec<PathBuf>,
    notes: Vec<String>,
}

fn scan(inputs: &[PathBuf]) -> Result<Scan> {
    let mut scan = Scan::default();
    for input in inputs {
        let meta = fs::metadata(input).with_context(|| format!("stat {}", input.display()))?;
        if meta.is_file() {
            classify_file(input, &mut scan);
        } else if meta.is_dir() {
            walk(input, 0, &mut scan);
        } else {
            scan.notes
                .push(format!("{}: not a file or directory", input.display()));
        }
    }
    scan.jsonls.sort();
    scan.summaries.sort();
    scan.bundles.sort();
    Ok(scan)
}

fn walk(dir: &Path, depth: usize, scan: &mut Scan) {
    if depth > 8 {
        scan.notes
            .push(format!("{}: directory depth limit", dir.display()));
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        scan.notes
            .push(format!("{}: unreadable directory", dir.display()));
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir_with_name = |n: &str| entry.file_type().is_ok_and(|t| t.is_dir()) && name == n;
        if name.starts_with('.') || is_dir_with_name("target") || is_dir_with_name("node_modules") {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&path, depth + 1, scan);
        } else {
            classify_file(&path, scan);
        }
    }
}

fn classify_file(path: &Path, scan: &mut Scan) {
    let name = file_name(path);
    if name.ends_with(".jsonl") {
        scan.jsonls.push(path.to_path_buf());
    } else if name.ends_with(".jsonl.zst") || name.ends_with(".jsonl.xz") {
        scan.notes.push(format!(
            "{}: compressed session log (decompress first, e.g. `zstd -d`)",
            path.display()
        ));
    } else if name.ends_with(".zip") {
        scan.bundles.push(path.to_path_buf());
    } else if name.ends_with("summary.json") || name.ends_with("summary.partial.json") {
        scan.summaries.push(path.to_path_buf());
    }
}

/// The summary file of `jsonl`: `<stem>.summary.json` (else its `.partial`) beside it, or a
/// `summary.json` / `summary.partial.json` in a folder that holds only this log.
fn summary_for(jsonl: &Path, scan: &Scan) -> Option<PathBuf> {
    let dir = jsonl.parent()?;
    let stem = jsonl.file_stem()?.to_str()?;
    let alone = scan.jsonls.iter().filter(|p| p.parent() == Some(dir)).count() == 1;
    let mut names = vec![
        format!("{stem}.summary.json"),
        format!("{stem}.summary.partial.json"),
    ];
    if alone {
        names.push("summary.json".into());
        names.push("summary.partial.json".into());
    }
    names
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| scan.summaries.contains(p))
}

/// Reads and parses a summary file.
fn read_summary(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text).ok().filter(Value::is_object)
}

// ---------------------------------------------------------------------------
// Cross-session report
// ---------------------------------------------------------------------------

/// A GPU/preset group's merged frame-time distribution.
#[derive(Debug, Clone, Serialize)]
struct Group {
    vendor: String,
    family: String,
    preset: String,
    sessions: usize,
    frames: u64,
    median_ms: Option<f64>,
    p95_ms: Option<f64>,
    pct_over_33: Option<f64>,
}

/// A crash bucket.
#[derive(Debug, Clone, Serialize)]
struct CrashSignature {
    signature: String,
    count: usize,
    bundles: Vec<String>,
    maps: Vec<String>,
}

/// Everything the report shows about one session with errors.
#[derive(Debug, Clone, Serialize)]
struct ErrorSession {
    session: String,
    path: String,
    gpu_errors: u64,
    device_lost: u64,
    surface: String,
    problems: String,
    serious_streams: String,
}

/// The whole triage output; serialized as the JSON summary.
#[derive(Debug, Serialize)]
struct Report {
    generated_utc: String,
    input_files: usize,
    sessions: Vec<Session>,
    bundles: Vec<CrashBundle>,
    skipped: Vec<String>,
    groups: Vec<Group>,
    hitch_causes: Vec<NameCount>,
    crash_signatures: Vec<CrashSignature>,
    error_sessions: Vec<ErrorSession>,
    suspected: Vec<Suspect>,
    thresholds: BTreeMap<String, f64>,
}

fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_days(secs / 86400, secs % 86400);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

/// Civil date from days since the Unix epoch (Howard Hinnant's `civil_from_days`, public
/// domain; documented in *chrono*'s algorithms page and cited in `docs/provenance.md`).
fn civil_from_days(z: u64, secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let z = z as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

/// Ranks sessions worst-first (p95, then %>33 ms, then hitch count; frame-less rows sink).
fn build_report(
    mut sessions: Vec<Session>,
    bundles: Vec<CrashBundle>,
    skipped: Vec<String>,
    input_files: usize,
) -> Report {
    sessions.sort_by(|a, b| {
        let key = |s: &Session| {
            (
                s.frame_p95_ms.unwrap_or(-1.0),
                s.pct_over_33.unwrap_or(-1.0),
                s.hitches as f64,
            )
        };
        let ord = key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal);
        ord.then_with(|| a.started_utc.cmp(&b.started_utc))
            .then_with(|| a.path.cmp(&b.path))
    });
    for (i, s) in sessions.iter_mut().enumerate() {
        s.rank = i + 1;
    }
    let mut thresholds = BTreeMap::new();
    thresholds.insert("acquire_ms".into(), ACQUIRE_MS);
    thresholds.insert("unattributed_ms".into(), UNATTRIBUTED_MS);
    thresholds.insert("over_ms_a".into(), OVER_A_MS);
    thresholds.insert("over_ms_b".into(), OVER_B_MS);
    Report {
        generated_utc: utc_now(),
        input_files,
        sessions,
        bundles,
        skipped,
        groups: Vec::new(),
        hitch_causes: Vec::new(),
        crash_signatures: Vec::new(),
        error_sessions: Vec::new(),
        suspected: Vec::new(),
        thresholds,
    }
}

/// Groups, cross-session hitch causes, crash buckets and the error-session list.
fn cross_session(report: &mut Report) {
    let mut groups: BTreeMap<(String, String, String), (Group, FpsHisto)> = BTreeMap::new();
    for s in &report.sessions {
        let family = if s.gpu_family.is_empty() {
            "unknown".to_owned()
        } else {
            s.gpu_family.clone()
        };
        let preset = if s.preset.is_empty() {
            "unknown".to_owned()
        } else {
            s.preset.clone()
        };
        let key = (s.gpu_vendor.clone(), family.clone(), preset.clone());
        let (group, histo) = groups.entry(key).or_insert_with(|| {
            (
                Group {
                    vendor: s.gpu_vendor.clone(),
                    family,
                    preset,
                    sessions: 0,
                    frames: 0,
                    median_ms: None,
                    p95_ms: None,
                    pct_over_33: None,
                },
                FpsHisto::default(),
            )
        });
        group.sessions += 1;
        group.frames += s.frames;
        histo.merge(&s.histo);
    }
    let mut group_list: Vec<Group> = groups
        .into_values()
        .map(|(mut g, h)| {
            g.median_ms = h.percentile_ms(0.50);
            g.p95_ms = h.percentile_ms(0.95);
            g.pct_over_33 = h.pct_over_ms(OVER_A_MS);
            g
        })
        .collect();
    group_list.sort_by(|a, b| {
        b.p95_ms
            .unwrap_or(-1.0)
            .partial_cmp(&a.p95_ms.unwrap_or(-1.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.vendor.cmp(&b.vendor).then_with(|| a.family.cmp(&b.family)))
    });
    report.groups = group_list;

    // Top hitch causes across sessions (each session's cause counts already aggregated).
    let mut causes: BTreeMap<String, u64> = BTreeMap::new();
    for s in &report.sessions {
        for cause in &s.hitch_causes {
            *causes.entry(cause.name.clone()).or_default() += cause.count;
        }
    }
    report.hitch_causes = top_counts(&causes, 12);

    // Crash signatures, bucketed by reason + top frames.
    let mut signatures: BTreeMap<String, CrashSignature> = BTreeMap::new();
    for b in &report.bundles {
        let slot = signatures
            .entry(b.signature.clone())
            .or_insert_with(|| CrashSignature {
                signature: b.signature.clone(),
                count: 0,
                bundles: Vec::new(),
                maps: Vec::new(),
            });
        slot.count += 1;
        slot.bundles.push(b.name.clone());
        if !b.map.is_empty() && !slot.maps.contains(&b.map) {
            slot.maps.push(b.map.clone());
        }
    }
    let mut signature_list: Vec<CrashSignature> = signatures.into_values().collect();
    signature_list.sort_by(|a, b| b.count.cmp(&a.count).then(a.signature.cmp(&b.signature)));
    report.crash_signatures = signature_list;

    // Sessions with errors.
    let mut errors = Vec::new();
    for s in &report.sessions {
        let surface: String = if s.surface.is_empty() && s.surface_totals.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = s
                .surface
                .iter()
                .filter(|(_, v)| **v > 0)
                .map(|(k, v)| format!("{k} {v}"))
                .collect();
            if parts.is_empty() {
                "totals only".into()
            } else {
                parts.join(", ")
            }
        };
        let problems: Vec<String> = s
            .problems
            .iter()
            .map(|p| format!("{} ×{}", p.code, p.events))
            .collect();
        const SERIOUS: [&str; 4] = [
            "budget_exceeded",
            "eviction_storm",
            "pak_corrupt",
            "wrong_platform_textures",
        ];
        let serious: Vec<String> = s
            .streams
            .iter()
            .filter(|(k, _)| SERIOUS.contains(&k.as_str()))
            .map(|(k, v)| format!("{k} ×{v}"))
            .collect();
        if s.gpu_errors > 0 || s.device_lost > 0 || !surface.is_empty() || !problems.is_empty() {
            errors.push(ErrorSession {
                session: s.name.clone(),
                path: s.path.clone(),
                gpu_errors: s.gpu_errors,
                device_lost: s.device_lost,
                surface,
                problems: problems.join(", "),
                serious_streams: serious.join(", "),
            });
        }
    }
    report.error_sessions = errors;
}

/// Fills `suspected` with the pattern checks, worst evidence first.
fn suspects(report: &mut Report) {
    let mut out = Vec::new();
    let sessions = std::mem::take(&mut report.sessions);
    for s in &sessions {
        let mut push = |check: &str, evidence: String| {
            out.push(Suspect {
                check: check.to_owned(),
                session: s.name.clone(),
                path: s.path.clone(),
                evidence,
            });
        };
        if let Some(prev) = &s.previous_exit
            && prev.clean == Some(false)
        {
            let last = match (prev.last_partial_seconds, prev.last_partial_frames) {
                (Some(secs), Some(frames)) => {
                    format!("its last partial summary was at {secs:.0} s, {frames} frames")
                }
                (Some(secs), None) => format!("its last partial summary was at {secs:.0} s"),
                _ => "no partial details".to_owned(),
            };
            push(
                "previous-session-unclean",
                format!(
                    "the session before this one never reached a clean exit (hard lock, crash or kill): {last}"
                ),
            );
        }
        if s.summary_kind.as_deref() == Some("partial") {
            let at = s.duration_s.map_or_else(|| "?".into(), |d| format!("{d:.0} s"));
            push(
                "hard-lock-suspected",
                format!(
                    "only summary.partial.json exists (last written at {at}): this session never reached its clean exit"
                ),
            );
        }
        if s.exit_clean == Some(false) && s.summary_kind.as_deref() == Some("final") {
            push(
                "unclean-exit",
                "summary.json says the session ended without the clean exit (a caught panic)".into(),
            );
        }
        if !s.stalls.is_empty() {
            let worst = s.stalls.iter().filter_map(|x| x.ms).fold(0.0, f64::max);
            let phases: Vec<String> = s
                .stalls
                .iter()
                .take(3)
                .map(|x| format!("{:.1} s in {}", x.ms.unwrap_or(0.0) / 1000.0, or_dash(&x.phase)))
                .collect();
            push(
                "stalls",
                format!(
                    "{} frame(s) of 1 s or more, worst {worst:.0} ms: {}",
                    s.stalls.len(),
                    phases.join("; ")
                ),
            );
        }
        if s.telemetry_events == 0 {
            push(
                "no-telemetry",
                "0 regia::* events: this build predates telemetry or runs with it off; frame stats unavailable".into(),
            );
        }
        let pipeline_streams = s.streams.get("pipeline_after_warmup").copied().unwrap_or(0).max(
            s.streams
                .get("pipeline_after_warmup_log_lines")
                .copied()
                .unwrap_or(0),
        );
        if s.after_warmup.pipelines > 0 || pipeline_streams > 0 {
            let labels: Vec<String> = s
                .creations
                .iter()
                .filter(|c| c.kind == "Pipeline")
                .take(4)
                .map(|c| format!("{} ×{}", c.label, c.count))
                .collect();
            let label_note = if labels.is_empty() {
                String::new()
            } else {
                format!(" ({})", labels.join(", "))
            };
            push(
                "pipelines-after-warmup",
                format!(
                    "{} pipeline(s) after warm-up across {} warm window(s){}",
                    s.after_warmup.pipelines.max(pipeline_streams),
                    s.warm_windows,
                    label_note
                ),
            );
        }
        if s.mesh_buffers_created > 0 || s.after_warmup.buffers > 0 {
            let per_frame = if s.frames > 0 {
                s.after_warmup.buffers as f64 / s.frames as f64
            } else {
                0.0
            };
            push(
                "steady-state-creations",
                format!(
                    "mesh buffers {} total; {} buffer(s) after warm-up (~{:.3}/frame) in {} warm window(s)",
                    s.mesh_buffers_created, s.after_warmup.buffers, per_frame, s.warm_windows
                ),
            );
        }
        if let Some(mean) = s.acquire_mean_ms
            && (mean > ACQUIRE_MS || s.acquire_max_ms.is_some_and(|m| m > ACQUIRE_MS))
        {
            push(
                "acquire-over-5ms",
                format!(
                    "render_split.acquire mean {mean:.1} ms, max {:.1} ms",
                    s.acquire_max_ms.unwrap_or(mean)
                ),
            );
        }
        if let Some(mean) = s.unattributed_mean_ms
            && (mean > UNATTRIBUTED_MS || s.unattributed_max_ms.is_some_and(|m| m > UNATTRIBUTED_MS))
        {
            push(
                "unattributed-over-20ms",
                format!(
                    "host_spans.main.unattributed_ms mean {mean:.1} ms, max {:.1} ms",
                    s.unattributed_max_ms.unwrap_or(mean)
                ),
            );
        }
        if s.gpu_errors > 0 {
            let first = s.gpu_error_first.first().cloned().unwrap_or_default();
            push("gpu-errors", format!("{} event(s): {first}", s.gpu_errors));
        }
        if s.device_lost > 0 {
            push(
                "device-lost",
                format!(
                    "{} event(s) ({})",
                    s.device_lost,
                    s.device_lost_reasons.join(", ")
                ),
            );
        }
        if s.surface.values().any(|v| *v > 0) {
            let parts: Vec<String> = s
                .surface
                .iter()
                .filter(|(_, v)| **v > 0)
                .map(|(k, v)| format!("{k} {v}"))
                .collect();
            push("surface-errors", parts.join(", "));
        }
        for problem in &s.problems {
            push(
                "render-problems",
                format!("{} ×{}: {}", problem.code, problem.events, problem.message),
            );
        }
        if let Some(worst) = s.slow_load_worst_ms
            && worst > 0.0
        {
            let labels = if s.slow_load_labels.is_empty() {
                String::new()
            } else {
                format!(" ({})", s.slow_load_labels.join(", "))
            }
            .chars()
            .take(120)
            .collect::<String>();
            push("slow-asset-loads", format!("worst {worst:.0} ms{labels}"));
        }
        if s.after_warmup.blocking_polls > 0 {
            push(
                "blocking-polls-after-warmup",
                format!("{} blocking poll(s) after warm-up", s.after_warmup.blocking_polls),
            );
        }
        if s.dropped_lines > 0 {
            push(
                "log-loss",
                format!("{} dropped telemetry note(s)/line(s)", s.dropped_lines),
            );
        }
    }
    report.sessions = sessions;
    report.suspected = out;
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// `12.3`; `—` when unknown.
fn ms1(v: Option<f64>) -> String {
    v.map_or_else(|| "—".into(), |x| format!("{x:.1}"))
}

fn pct1(v: Option<f64>) -> String {
    v.map_or_else(|| "—".into(), |x| format!("{x:.1}%"))
}

fn fmt_bytes(v: Option<u64>) -> String {
    let Some(v) = v else { return "—".into() };
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * MIB;
    let v = v as f64;
    if v >= GIB {
        format!("{:.2} GiB", v / GIB)
    } else if v >= MIB {
        format!("{:.0} MiB", v / MIB)
    } else {
        format!("{:.0} KiB", v / 1024.0)
    }
}

fn fmt_duration(v: Option<f64>) -> String {
    let Some(secs) = v else { return "—".into() };
    let secs = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Escapes a cell for a markdown table row.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn or_dash(text: &str) -> String {
    if text.is_empty() {
        "—".into()
    } else {
        text.to_owned()
    }
}

/// The markdown report.
/// The table's `exit` cell: how the session ended, as far as the files say.
fn exit_label(sess: &Session) -> String {
    match (sess.summary_kind.as_deref(), sess.exit_clean) {
        (Some("final"), Some(true)) => sess
            .exit_ms
            .map_or_else(|| "clean".into(), |ms| format!("clean {ms:.0} ms")),
        (Some("final"), Some(false)) => "panic".into(),
        (Some("partial"), _) => "no clean exit".into(),
        (Some(_), None) => "—".into(),
        _ => "—".into(),
    }
}

/// The spec-28 sections of a session's details: summary file, previous session, machine, phase
/// timeline, per-map frames and stalls.
fn write_contract_details(s: &mut String, sess: &Session) {
    if let (Some(kind), Some(path)) = (&sess.summary_kind, &sess.summary_path) {
        let _ = writeln!(
            s,
            "- summary.json: **{kind}** (`{path}`); exit {}",
            exit_label(sess)
        );
    }
    if let Some(prev) = &sess.previous_exit {
        match prev.clean {
            Some(false) => {
                let _ = writeln!(
                    s,
                    "- previous session: **ended uncleanly** (hard lock, crash or kill); last partial summary at {} s, {} frames",
                    prev.last_partial_seconds
                        .map_or_else(|| "?".into(), |x| format!("{x:.0}")),
                    prev.last_partial_frames
                        .map_or_else(|| "?".into(), |x| x.to_string())
                );
            }
            Some(true) => {
                let _ = writeln!(s, "- previous session: ended cleanly");
            }
            None => {}
        }
    }
    if !sess.cpu_model.is_empty() || sess.ram_gb.is_some() || sess.vram_gb.is_some() {
        let _ = writeln!(
            s,
            "- machine: {}, {} GB RAM, {} GB VRAM; {}",
            or_dash(&sess.cpu_model),
            sess.ram_gb.map_or_else(|| "—".into(), |x| format!("{x:.1}")),
            sess.vram_gb.map_or_else(|| "—".into(), |x| format!("{x:.1}")),
            or_dash(&sess.os_version)
        );
    }
    if !sess.upscaler.is_empty() || !sess.auto_plan.is_empty() {
        let _ = writeln!(
            s,
            "- upscaler {}; first-launch plan: {}",
            or_dash(&sess.upscaler),
            or_dash(&sess.auto_plan)
        );
    }
    if !sess.shaders_backend.is_empty() || !sess.driver_cache.is_empty() {
        let _ = writeln!(
            s,
            "- shaders: backend {}, driver shader cache {}",
            or_dash(&sess.shaders_backend),
            or_dash(&sess.driver_cache)
        );
    }
    let mut timeline: Vec<String> = Vec::new();
    if let Some(ms) = sess.boot_done_ms {
        timeline.push(format!("boot_done {ms:.0} ms"));
    }
    if let Some(ready) = &sess.shaders_ready {
        timeline.push(format!(
            "shaders_ready {} pipelines in {} ms ({} ms each, cache {}, {} threads)",
            ready.get("pipelines").and_then(Value::as_u64).unwrap_or(0),
            ms1(f(ready, "ms")),
            ms1(f(ready, "ms_each")),
            st(ready, "cache").unwrap_or("?"),
            ready.get("threads").and_then(Value::as_u64).unwrap_or(0)
        ));
    }
    for load in &sess.scene_loads {
        timeline.push(format!(
            "scene_load {}{} ms, {} MB uploaded, {} pipelines, {} meshes",
            st(load, "name").map(|n| format!("{n} ")).unwrap_or_default(),
            ms1(f(load, "ms")),
            ms1(f(load, "mb_uploaded")),
            load.get("pipelines").and_then(Value::as_u64).unwrap_or(0),
            load.get("meshes").and_then(Value::as_u64).unwrap_or(0)
        ));
    }
    if !timeline.is_empty() {
        let _ = writeln!(s, "- phases: {}", timeline.join("; "));
    }
    let other: Vec<String> = sess
        .phases
        .iter()
        .filter(|p| !matches!(p.phase.as_str(), "boot_done" | "shaders_ready"))
        .map(|p| match (&p.name, p.t_s) {
            (Some(name), Some(t)) => format!("{} {name} @{t:.0}s", p.phase),
            (None, Some(t)) => format!("{} @{t:.0}s", p.phase),
            _ => p.phase.clone(),
        })
        .take(40)
        .collect();
    if !other.is_empty() {
        let _ = writeln!(s, "  - timeline: {}", other.join(", "));
    }
    if !sess.maps.is_empty() {
        let _ = writeln!(
            s,
            "\n| map | frames | p50 | p95 | p99 | >33ms | >50ms |\n|---|---:|---:|---:|---:|---:|---:|"
        );
        for m in &sess.maps {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {} |",
                cell(&m.map),
                m.frames.map_or_else(|| "—".into(), |n| n.to_string()),
                ms1(m.p50_ms),
                ms1(m.p95_ms),
                ms1(m.p99_ms),
                pct1(m.over_33ms_pct),
                pct1(m.over_50ms_pct)
            );
        }
        s.push('\n');
    }
    if !sess.stalls.is_empty() {
        let _ = writeln!(s, "- stalls (frames of 1 s or more):");
        for st in sess.stalls.iter().take(10) {
            let _ = writeln!(
                s,
                "  - {} s: {} ms in {}",
                st.t_s.map_or_else(|| "?".into(), |t| format!("{t:.0}")),
                ms1(st.ms),
                or_dash(&st.phase)
            );
        }
    }
}

fn render_markdown(report: &Report) -> String {
    let mut s = String::with_capacity(16 * 1024);
    let _ = writeln!(
        s,
        "# DO27 alpha triage\n\nGenerated {} from {} input path(s): **{} session log(s)**, **{} feedback bundle(s)**.\n",
        report.generated_utc,
        report.input_files,
        report.sessions.len(),
        report.bundles.len()
    );
    if !report.skipped.is_empty() {
        let _ = writeln!(s, "Skipped / notes:\n");
        for note in &report.skipped {
            let _ = writeln!(s, "- {note}");
        }
        s.push('\n');
    }

    // Sessions table, worst first.
    let _ = writeln!(s, "## Sessions (worst first)\n");
    let _ = writeln!(
        s,
        "| # | session | GPU | preset | res | dur | frames | p50 | p95 | p99 | >33ms | >50ms | hitches | errors | exit |"
    );
    let _ = writeln!(
        s,
        "|---:|---|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|"
    );
    for sess in &report.sessions {
        let errors = sess.gpu_errors
            + sess.device_lost
            + sess.surface.values().sum::<u64>()
            + sess.problems.iter().map(|p| p.events).sum::<u64>();
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            sess.rank,
            cell(&sess.name),
            cell(if sess.gpu_family.is_empty() {
                &sess.gpu_vendor
            } else {
                &sess.gpu_family
            }),
            or_dash(&sess.preset),
            sess.resolution
                .map_or_else(|| "—".into(), |[w, h]| format!("{w}×{h}")),
            fmt_duration(sess.duration_s),
            sess.frames,
            ms1(sess.frame_p50_ms),
            ms1(sess.frame_p95_ms),
            ms1(sess.frame_p99_ms),
            pct1(sess.pct_over_33),
            pct1(sess.pct_over_50),
            sess.hitches.max(sess.hitch_events),
            errors,
            exit_label(sess)
        );
    }
    s.push('\n');
    let _ = writeln!(
        s,
        "Frame percentiles are estimated from the merged `fps_histogram` buckets (the 33.3/50 ms bucket edges are exact); `>33ms` reads the fps<30 buckets.\n"
    );

    // Worst sessions with paths.
    let _ = writeln!(s, "### Worst sessions\n");
    for sess in report.sessions.iter().take(5) {
        let _ = writeln!(
            s,
            "{}. **{}** — p95 {} ms, {} hitches — `{}`",
            sess.rank,
            cell(&sess.name),
            ms1(sess.frame_p95_ms),
            sess.hitches.max(sess.hitch_events),
            sess.path
        );
    }
    s.push('\n');

    // Per-session details.
    let _ = writeln!(s, "## Session details\n");
    for sess in &report.sessions {
        let _ = writeln!(s, "### {}. `{}`\n", sess.rank, cell(&sess.name));
        let _ = writeln!(s, "- path: `{}`", sess.path);
        let _ = writeln!(
            s,
            "- GPU: {} (`{}` {}, {}) — vendor **{}**",
            or_dash(&sess.gpu_name),
            or_dash(&sess.gpu_driver),
            or_dash(&sess.gpu_backend),
            or_dash(&sess.gpu_device_type),
            sess.gpu_vendor
        );
        let _ = writeln!(
            s,
            "- OS: {}, {} threads; regia {} (git {}, {})",
            or_dash(&sess.os),
            sess.cpu_threads.map_or_else(|| "—".into(), |t| t.to_string()),
            or_dash(&sess.regia_version),
            or_dash(&sess.regia_git),
            or_dash(&sess.regia_profile)
        );
        if !sess.pipeline_cache.is_empty() {
            let _ = writeln!(s, "- pipeline cache: {}", sess.pipeline_cache);
        }
        write_contract_details(&mut s, sess);
        let _ = writeln!(
            s,
            "- preset **{}**, scale {}, AA {}, GI {}; resolution {} ({} Hz), started {}",
            or_dash(&sess.preset),
            or_dash(&sess.render_scale),
            or_dash(&sess.aa),
            or_dash(&sess.gi),
            sess.resolution
                .map_or_else(|| "—".into(), |[w, h]| format!("{w}×{h}")),
            sess.refresh_hz
                .map_or_else(|| "—".into(), |hz| format!("{hz:.0}")),
            sess.started_utc.as_deref().unwrap_or("—")
        );
        let _ = writeln!(
            s,
            "- duration {}, {} frames ({} telemetry events, {} crate lines, {} bad lines{})",
            fmt_duration(sess.duration_s),
            sess.frames,
            sess.telemetry_events,
            sess.crate_lines,
            sess.bad_lines,
            if sess.truncated_tail {
                ", truncated tail"
            } else {
                ""
            }
        );
        if sess.frames > 0 {
            let _ = writeln!(
                s,
                "- frame ms: p50 {}, p95 {}, p99 {}, mean {}, max {}; >33 ms: {}, >50 ms: {}; bound {}",
                ms1(sess.frame_p50_ms),
                ms1(sess.frame_p95_ms),
                ms1(sess.frame_p99_ms),
                ms1(sess.frame_mean_ms),
                ms1(sess.frame_max_ms),
                pct1(sess.pct_over_33),
                pct1(sess.pct_over_50),
                or_dash(&sess.bound)
            );
        }
        let _ = writeln!(
            s,
            "- hitches: {} emitted (+{} suppressed), {} in summaries, worst {} ms",
            sess.hitch_events,
            sess.hitches_suppressed,
            sess.hitches,
            ms1(sess.worst_hitch_ms)
        );
        if !sess.hitch_causes.is_empty() {
            let causes: Vec<String> = sess
                .hitch_causes
                .iter()
                .map(|c| format!("{} ({})", c.name, c.count))
                .collect();
            let _ = writeln!(s, "  - top causes: {}", causes.join(", "));
        }
        if !sess.hitch_top_spans.is_empty() {
            let spans: Vec<String> = sess
                .hitch_top_spans
                .iter()
                .map(|c| format!("{} ({})", c.name, c.count))
                .collect();
            let _ = writeln!(s, "  - spans in hitch frames: {}", spans.join(", "));
        }
        let lost_reasons = if sess.device_lost_reasons.is_empty() {
            "—".to_owned()
        } else {
            sess.device_lost_reasons.join(", ")
        };
        let _ = writeln!(
            s,
            "- errors: gpu {}, device lost {} ({lost_reasons}), surface [{}]",
            sess.gpu_errors,
            sess.device_lost,
            if sess.surface.is_empty() {
                "—".to_owned()
            } else {
                sess.surface
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        for error in &sess.gpu_error_first {
            let _ = writeln!(s, "  - gpu error: {error}");
        }
        for problem in &sess.problems {
            let _ = writeln!(
                s,
                "  - problem {} ×{}: {}",
                problem.code, problem.events, problem.message
            );
        }
        if !sess.streams.is_empty() {
            let streams: Vec<String> = sess.streams.iter().map(|(k, v)| format!("{k} ×{v}")).collect();
            let _ = writeln!(s, "- stream events: {}", streams.join(", "));
        }
        let _ = writeln!(
            s,
            "- memory: VRAM {} (reserved {}), RSS peak {}",
            fmt_bytes(sess.vram_peak_bytes),
            fmt_bytes(sess.vram_reserved_peak_bytes),
            fmt_bytes(sess.rss_peak_bytes)
        );
        let _ = writeln!(
            s,
            "- churn after warm-up ({} warm windows): pipelines {}, buffers {}, mesh buffers {}, blocking polls {}, inline bakes {}; uploads ≤ {:.0} bytes/frame",
            sess.warm_windows,
            sess.after_warmup.pipelines,
            sess.after_warmup.buffers,
            sess.mesh_buffers_created,
            sess.after_warmup.blocking_polls,
            sess.after_warmup.inline_bakes,
            sess.upload_per_frame_max_bytes.unwrap_or(0.0)
        );
        if !sess.creations.is_empty() {
            let creations: Vec<String> = sess
                .creations
                .iter()
                .map(|c| {
                    format!(
                        "{} {} ×{} ({})",
                        c.kind,
                        c.label,
                        c.count,
                        fmt_bytes(Some(c.bytes))
                    )
                })
                .collect();
            let _ = writeln!(s, "  - top creations: {}", creations.join(", "));
        }
        if !sess.top_systems.is_empty() {
            let systems: Vec<String> = sess
                .top_systems
                .iter()
                .map(|x| format!("{} ({:.2} ms)", x.name, x.ms))
                .collect();
            let _ = writeln!(s, "- top systems: {}", systems.join(", "));
        }
        if !sess.top_host_spans.is_empty() || !sess.top_host_other.is_empty() {
            let main: Vec<String> = sess
                .top_host_spans
                .iter()
                .map(|x| format!("{} ({:.2} ms)", x.name, x.ms))
                .collect();
            let other: Vec<String> = sess
                .top_host_other
                .iter()
                .map(|x| format!("{} ({:.2} ms)", x.name, x.ms))
                .collect();
            let _ = writeln!(
                s,
                "- host spans (main): {}; other threads: {}; unattributed {} ms (max {})",
                if main.is_empty() {
                    "—".to_owned()
                } else {
                    main.join(", ")
                },
                if other.is_empty() {
                    "—".to_owned()
                } else {
                    other.join(", ")
                },
                ms1(sess.unattributed_mean_ms),
                ms1(sess.unattributed_max_ms)
            );
        }
        if let Some(counts) = &sess.counts_last {
            let _ = writeln!(
                s,
                "- scene: {:.0} mesh instances, {:.0} draws, {:.0} lights, {} entities",
                counts.mesh_instances_mean, counts.drawn_mean, counts.local_lights_mean, counts.entities
            );
        }
        for note in &sess.notes {
            let _ = writeln!(s, "- note: {note}");
        }
        s.push('\n');
    }

    // Across sessions.
    let _ = writeln!(s, "## Across sessions\n");
    let _ = writeln!(s, "### FPS by GPU and preset\n");
    let _ = writeln!(
        s,
        "| vendor | family | preset | sessions | frames | median ms | p95 ms | >33ms |"
    );
    let _ = writeln!(s, "|---|---|---|---:|---:|---:|---:|---:|");
    for g in &report.groups {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            g.vendor,
            cell(&g.family),
            g.preset,
            g.sessions,
            g.frames,
            ms1(g.median_ms),
            ms1(g.p95_ms),
            pct1(g.pct_over_33)
        );
    }
    s.push('\n');
    let _ = writeln!(s, "### Top hitch causes\n");
    if report.hitch_causes.is_empty() {
        let _ = writeln!(s, "_none (no hitch events with evidence)_\n");
    } else {
        let _ = writeln!(s, "| cause | hitches |");
        let _ = writeln!(s, "|---|---:|");
        for cause in &report.hitch_causes {
            let _ = writeln!(s, "| {} | {} |", cell(&cause.name), cause.count);
        }
        s.push('\n');
    }
    let _ = writeln!(s, "### Crash signatures\n");
    if report.crash_signatures.is_empty() {
        let _ = writeln!(s, "_no crash bundles found_\n");
    } else {
        let _ = writeln!(s, "| signature | count | bundles |");
        let _ = writeln!(s, "|---|---:|---|");
        for sig in &report.crash_signatures {
            let maps = if sig.maps.is_empty() {
                String::new()
            } else {
                format!(" [{}]", sig.maps.join(", "))
            };
            let _ = writeln!(
                s,
                "| {} | {} | {}{} |",
                cell(&sig.signature),
                sig.count,
                sig.bundles.iter().map(|b| cell(b)).collect::<Vec<_>>().join(", "),
                maps
            );
        }
        let _ = writeln!(
            s,
            "\nNative crash stacks: `cargo xtask minidump <bundle> --symbols <package>/symbols`.\n"
        );
    }
    let _ = writeln!(s, "### Sessions with errors\n");
    if report.error_sessions.is_empty() {
        let _ = writeln!(s, "_none_\n");
    } else {
        let _ = writeln!(
            s,
            "| session | gpu errors | device lost | surface | problems | serious streams |"
        );
        let _ = writeln!(s, "|---|---:|---:|---|---|---|");
        for e in &report.error_sessions {
            let _ = writeln!(
                s,
                "| `{}` | {} | {} | {} | {} | {} |",
                cell(&e.session),
                e.gpu_errors,
                e.device_lost,
                or_dash(&e.surface),
                or_dash(&e.problems),
                or_dash(&e.serious_streams)
            );
        }
        s.push('\n');
    }

    // Suspected engine issues.
    let _ = writeln!(
        s,
        "## Suspected engine issues\n\nChecks: `no-telemetry`; `pipelines-after-warmup` (pipelines > 0 after warm); `steady-state-creations` (mesh/buffer creations > 0 in warm windows); `acquire-over-5ms` (`render_split.acquire` > {ACQUIRE_MS} ms); `unattributed-over-20ms` (`host_spans.main.unattributed_ms` > {UNATTRIBUTED_MS} ms); plus errors, problems and log loss.\n"
    );
    if report.suspected.is_empty() {
        let _ = writeln!(s, "_none flagged_\n");
    } else {
        let _ = writeln!(s, "| check | session | evidence |");
        let _ = writeln!(s, "|---|---|---|");
        for suspect in &report.suspected {
            let _ = writeln!(
                s,
                "| {} | `{}` | {} |",
                suspect.check,
                cell(&suspect.session),
                cell(&suspect.evidence)
            );
        }
        s.push('\n');
    }
    s
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Runs `alpha-triage`: scan, parse, aggregate, report.
///
/// # Errors
///
/// Returns an error when an input cannot be read, or when nothing usable was found.
pub fn run(args: &Args) -> Result<ExitCode> {
    let mut scan = scan(&args.inputs)?;
    if scan.jsonls.is_empty() && scan.bundles.is_empty() && scan.summaries.is_empty() {
        bail!(
            "alpha-triage found no session logs (*.jsonl) and no feedback bundles (*.zip) in: {}",
            args.inputs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let mut sessions = Vec::new();
    let mut used: Vec<PathBuf> = Vec::new();
    for path in &scan.jsonls {
        match parse_session(path) {
            Ok(mut session) => {
                if let Some(found) = summary_for(path, &scan) {
                    match read_summary(&found) {
                        Some(v) => {
                            let partial = file_name(&found).ends_with("partial.json");
                            apply_summary(&mut session, &v, partial, &found);
                        }
                        None => scan
                            .notes
                            .push(format!("{}: unreadable summary file", found.display())),
                    }
                    used.push(found);
                }
                sessions.push(session);
            }
            Err(e) => scan.notes.push(format!("{e:#}")),
        }
    }
    // Summaries with no log beside them (a zip without the jsonl, a hung session's folder).
    for path in scan.summaries.iter().filter(|p| !used.contains(p)) {
        let Some(v) = read_summary(path) else {
            scan.notes
                .push(format!("{}: unreadable summary file", path.display()));
            continue;
        };
        let mut session = Session {
            path: path.display().to_string(),
            name: file_name(path),
            ..Session::default()
        };
        session.gpu_vendor = "unknown".into();
        session.gpu_family = "unknown".into();
        let partial = file_name(path).ends_with("partial.json");
        apply_summary(&mut session, &v, partial, path);
        session
            .notes
            .push("session known from summary.json only (no JSONL log)".into());
        sessions.push(session);
    }
    let mut bundles = Vec::new();
    for path in &scan.bundles {
        match parse_bundle(path) {
            Some(bundle) => bundles.push(bundle),
            None => scan.notes.push(format!(
                "{}: zip without a readable feedback.json",
                path.display()
            )),
        }
    }

    let mut report = build_report(sessions, bundles, scan.notes, args.inputs.len());
    cross_session(&mut report);
    suspects(&mut report);

    let md = render_markdown(&report);
    let json = serde_json::to_string_pretty(&report).context("serializing the JSON summary")?;

    match (&args.out, args.json) {
        (Some(out), true) => {
            write_file(out, &md)?;
            let mut json_path = out.clone();
            json_path.set_extension("json");
            write_file(&json_path, &format!("{json}\n"))?;
        }
        (Some(out), false) => write_file(out, &md)?,
        (None, true) => println!("{json}"),
        (None, false) => print!("{md}"),
    }
    println!(
        "alpha-triage: {} session(s), {} bundle(s), {} suspected issue(s), {} group(s)",
        report.sessions.len(),
        report.bundles.len(),
        report.suspected.len(),
        report.groups.len()
    );
    Ok(ExitCode::SUCCESS)
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_percentiles_and_shares() {
        let mut h = FpsHisto::default();
        // 100 frames at 5 ms (ge_144), 100 at 10 ms (ge_90), 100 at 40 ms (ge_20), 100 at 150 ms (lt_10).
        h.counts[1] = 100;
        h.counts[4] = 100;
        h.counts[7] = 100;
        h.counts[9] = 100;
        assert_eq!(h.total(), 400);
        // p50: the 200th frame is the last of the ge_90 bucket [11.11, 16.67).
        let p50 = h.percentile_ms(0.5).expect("p50");
        assert!((11.11..16.67).contains(&p50), "{p50}");
        // p95: the 380th frame is 80 into the lt_10 bucket [100, 200).
        let p95 = h.percentile_ms(0.95).expect("p95");
        assert!((150.0..200.0).contains(&p95), "{p95}");
        // p100 clamps to the cap.
        assert_eq!(h.percentile_ms(1.0), Some(200.0));
        // Over 33.33 ms: all of ge_20 + lt_10 = 50%.
        let pct = h.pct_over_ms(OVER_A_MS).expect("pct");
        assert!((pct - 50.0).abs() < 0.01, "{pct}");
        // Over 50 ms: lt_10 only = 25%.
        let pct = h.pct_over_ms(OVER_B_MS).expect("pct");
        assert!((pct - 25.0).abs() < 0.01, "{pct}");
        // A threshold inside a bucket splits it: 40 ms cuts ge_20's [33.33, 50) at 60%.
        let pct = h.pct_over_ms(40.0).expect("pct");
        let expected = (100.0 + 100.0 * (50.0 - 40.0) / (50.0 - 33.333)) * 100.0 / 400.0;
        assert!((pct - expected).abs() < 0.1, "{pct} vs {expected}");
        assert_eq!(FpsHisto::default().percentile_ms(0.5), None);
    }

    #[test]
    fn gpu_families_and_vendors() {
        assert_eq!(gpu_vendor(Some("0x1002"), None), "amd");
        assert_eq!(gpu_vendor(Some("0x10de"), None), "nvidia");
        assert_eq!(gpu_vendor(Some("0x8086"), None), "intel");
        assert_eq!(gpu_vendor(None, Some("NVIDIA GeForce RTX 5090")), "nvidia");
        assert_eq!(gpu_family("AMD Radeon RX 7900 XTX (RADV NAVI31)"), "RX 7900 XTX");
        assert_eq!(gpu_family("NVIDIA GeForce RTX 5090"), "RTX 5090");
        assert_eq!(gpu_family("Intel(R) Arc(TM) A770 Graphics"), "A770 Graphics");
        // No model number: keep the marketing name, drop the company.
        assert_eq!(
            gpu_family("AMD Radeon Graphics (radv RADV PHOENIX)"),
            "Radeon Graphics"
        );
        assert_eq!(gpu_family(""), "unknown");
    }

    #[test]
    fn ron_settings_extraction() {
        let ron =
            "(graphics: (preset: \"Ultra\", render_scale: 0.8, aa: Taa, gi: Ddgi), audio: (master: 80))";
        assert_eq!(ron_field(ron, "preset").as_deref(), Some("Ultra"));
        assert_eq!(ron_field(ron, "render_scale").as_deref(), Some("0.8"));
        assert_eq!(ron_field(ron, "aa").as_deref(), Some("Taa"));
        assert_eq!(ron_field(ron, "gi").as_deref(), Some("Ddgi"));
        assert_eq!(ron_field(ron, "missing"), None);
    }

    #[test]
    fn crash_backtrace_symbols_prefer_regia() {
        let txt = "panic: boom\nlocation: a.rs:1:1\nthread: main\n\nbacktrace:\n   0: std::panicking::begin_panic\n             at lib.rs:9:9\n   1: regia_render::window_phases::present\n             at crates/regia_render/src/window_phases.rs:120:5\n   2: do27_shell::frame::step\n             at shell/frame.rs:7:1\n";
        let frames = backtrace_symbols(txt, 3);
        assert_eq!(
            frames,
            vec![
                "regia_render::window_phases::present".to_owned(),
                "do27_shell::frame::step".to_owned(),
            ]
        );
    }

    #[test]
    fn a_log_written_before_the_rename_still_triages() {
        let legacy = regia_core::brand::LEGACY_MAIN_CRATE;
        let dir = regia_test_utils::unique_temp_dir("alpha-triage-legacy");
        let path = dir.join("session.jsonl");
        let mut header = serde_json::Map::new();
        header.insert(
            legacy.to_owned(),
            serde_json::json!({ "version": "0.1.29", "git": "55dc2d625", "profile": "release" }),
        );
        let line = |t_ms: f64, target: String, data: Value| {
            format!(
                "{{\"t_ms\":{t_ms},\"wall\":\"2026-10-17T18:00:01.0Z\",\"level\":\"INFO\",\
                 \"target\":\"{target}\",\"msg\":\"x\",\"fields\":{{\"data\":{data}}},\"span\":[]}}\n"
            )
        };
        let text = line(1.0, format!("{legacy}::session"), Value::Object(header))
            + &line(
                9.0,
                format!("{legacy}::exit"),
                serde_json::json!({ "total_ms": 12.5 }),
            );
        fs::write(&path, text).expect("write log");
        let s = parse_session(&path).expect("parse");
        assert_eq!(s.regia_version, "0.1.29");
        assert_eq!(s.regia_git, "55dc2d625");
        assert_eq!(s.telemetry_events, 2, "both legacy targets are telemetry events");
        assert_eq!(s.exit_ms, Some(12.5));
        let crash = format!(
            "backtrace:\n   0: {legacy}_render::window_phases::present\n   1: do27_shell::frame::step\n"
        );
        assert_eq!(
            backtrace_symbols(&crash, 1),
            vec![format!("{legacy}_render::window_phases::present")],
            "engine frames from before the rename are engine frames"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_histo_percentile_is_none() {
        assert_eq!(FpsHisto::default().pct_over_ms(OVER_A_MS), None);
    }

    #[test]
    fn hitch_cause_skips_the_frame_wrapper_span() {
        let d = serde_json::json!({
            "top_cpu_spans": [{"span": "frame", "ms": 80.0}, {"span": "render_into", "ms": 40.0}]
        });
        assert_eq!(hitch_cause(&d), "span render_into");
        let d = serde_json::json!({"top_cpu_spans": [{"span": "frame", "ms": 80.0}]});
        assert_eq!(hitch_cause(&d), "unknown");
        let d = serde_json::json!({"top_systems": [{"system": "a::b", "ms": 1.0, "runs": 1}]});
        assert_eq!(hitch_cause(&d), "system a::b");
    }

    #[test]
    fn utc_now_is_shaped() {
        let now = utc_now();
        assert_eq!(now.len(), "YYYY-MM-DD HH:MM:SS UTC".len(), "{now}");
        assert!(now.ends_with(" UTC"));
    }

    #[test]
    fn civil_date_matches_known_day() {
        // 2026-10-12 is day 20738 since the epoch.
        assert_eq!(civil_from_days(20_738, 3661), (2026, 10, 12, 1, 1, 1));
    }
}
