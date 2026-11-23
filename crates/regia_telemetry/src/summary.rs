// SPDX-License-Identifier: MPL-2.0
//! `summary.json` (spec 28 § 4): one record per session, written at a clean exit and, while the
//! session runs, as `summary.partial.json` (first 5 s after the header, then every 60 s, off the frame thread, replaced atomically)
//! so a session that hard-locks still leaves its numbers. The next session finds a partial without
//! a final and records `previous_exit`.
//!
//! [`SessionSummary`] is the schema: every section is optional, so a record from an older engine,
//! a headless run or a crashed session still parses.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hist::Histogram;
use crate::phase;

/// The summary schema version.
pub const SUMMARY_SCHEMA: u32 = 1;

/// Seconds between partial summaries.
pub const PARTIAL_INTERVAL_S: f64 = 60.0;

/// Seconds after the session header until the first partial (a crash in the first minute leaves one).
pub const FIRST_PARTIAL_S: f64 = 5.0;

/// Frames at least this long (ms) are listed in `stalls`.
pub const STALL_MS: f32 = 1000.0;

/// Most stalls listed.
const MAX_STALLS: usize = 32;

/// Most distinct hitch causes counted (the rest add to `other`).
const MAX_CAUSES: usize = 64;

/// Most maps with their own frame statistics.
const MAX_MAP_FRAMES: usize = 32;

macro_rules! optional_fields {
    ($(#[$meta:meta])* pub struct $name:ident { $($(#[$fmeta:meta])* pub $field:ident : $ty:ty,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        pub struct $name {
            $(
                $(#[$fmeta])*
                #[serde(skip_serializing_if = "Option::is_none")]
                pub $field: Option<$ty>,
            )*
        }
    };
}

optional_fields! {
    /// The engine build.
    pub struct RegiaInfo {
        /// Engine version.
        pub version: String,
        /// Git hash of the build.
        pub git: String,
        /// `debug` or `release`.
        pub profile: String,
    }
}

optional_fields! {
    /// The machine.
    pub struct SystemInfo {
        /// OS name and version (`Windows 11 Pro 25H2 (build 26200)`).
        pub os: String,
        /// Processor model.
        pub cpu: String,
        /// Logical CPUs.
        pub cpu_threads: u64,
        /// Physical RAM, GB.
        pub ram_gb: f64,
        /// Adapter name.
        pub gpu: String,
        /// PCI vendor id (`0x10de`).
        pub vendor: String,
        /// Driver version.
        pub driver: String,
        /// Graphics backend (`Vulkan`).
        pub backend: String,
        /// Device-local memory, GB.
        pub vram_gb: f64,
    }
}

optional_fields! {
    /// One monitor.
    pub struct MonitorInfo {
        /// Name.
        pub name: String,
        /// Native width, px.
        pub width: u32,
        /// Native height, px.
        pub height: u32,
        /// Refresh rate, Hz.
        pub refresh_hz: f64,
        /// Scale factor.
        pub scale: f64,
        /// HDR output is on (known for the monitor in use only).
        pub hdr: bool,
        /// The window is on this monitor.
        pub current: bool,
    }
}

optional_fields! {
    /// The display in use.
    pub struct DisplayInfo {
        /// Window width, px.
        pub width: u32,
        /// Window height, px.
        pub height: u32,
        /// Refresh rate of the monitor in use, Hz.
        pub refresh_hz: f64,
        /// Window mode.
        pub mode: String,
        /// Present mode.
        pub present: String,
        /// HDR output.
        pub hdr: bool,
        /// Every monitor.
        pub monitors: Vec<MonitorInfo>,
    }
}

optional_fields! {
    /// Graphics settings in effect.
    pub struct SettingsInfo {
        /// Quality preset.
        pub preset: String,
        /// Render scale.
        pub render_scale: f64,
        /// Anti-aliasing.
        pub aa: String,
        /// Global illumination mode.
        pub gi: String,
        /// The upscaler actually used (`native`, `TAAU`, `FSR 3`, …).
        pub upscaler: String,
        /// The first-launch auto-quality choice (ADR-157).
        pub auto_plan: Value,
    }
}

optional_fields! {
    /// Session totals.
    pub struct SessionInfo {
        /// Seconds since start.
        pub seconds: f64,
        /// Frames observed.
        pub frames: u64,
        /// Maps named by the host's markers.
        pub maps: Vec<String>,
    }
}

optional_fields! {
    /// Frame-time statistics (session-wide, or one map's).
    pub struct FrameStats {
        /// Frames counted (per-map entries).
        pub frames: u64,
        /// Median, ms.
        pub p50_ms: f64,
        /// 95th percentile, ms.
        pub p95_ms: f64,
        /// 99th percentile, ms.
        pub p99_ms: f64,
        /// Share of frames over 33.3 ms, percent.
        pub over_33ms_pct: f64,
        /// Share of frames over 50 ms, percent.
        pub over_50ms_pct: f64,
        /// `cpu`, `gpu` or `unknown`.
        pub bound: String,
    }
}

optional_fields! {
    /// Hitches.
    pub struct HitchInfo {
        /// Hitches detected.
        pub count: u64,
        /// Not logged because of the rate limit.
        pub suppressed: u64,
        /// `[cause, count]`, most frequent first.
        pub top_causes: Vec<(String, u64)>,
    }
}

optional_fields! {
    /// One frame of a second or more.
    pub struct Stall {
        /// Seconds since start when it ended.
        pub t_s: f64,
        /// Its length, ms.
        pub ms: f64,
        /// The host's or engine's phase marker in effect.
        pub phase: String,
    }
}

optional_fields! {
    /// Memory.
    pub struct MemoryInfo {
        /// Peak GPU allocations, GB.
        pub vram_peak_gb: f64,
        /// Peak resident set, GB.
        pub rss_peak_gb: f64,
        /// The texture budget was exceeded at some point.
        pub texture_budget_exceeded: bool,
    }
}

optional_fields! {
    /// Shaders and pipelines.
    pub struct ShadersInfo {
        /// Pipelines created by the startup warm: the denominator `ms` is divided by for `ms_each`.
        pub pipelines: u64,
        /// Pipelines created over the whole process at the time of the summary (the lifetime
        /// total, grown by pipelines compiled after the warm).
        pub pipelines_total: u64,
        /// Startup warm time, ms.
        pub ms: f64,
        /// Warm time per pipeline, ms.
        pub ms_each: f64,
        /// Pipeline cache at startup (`hit`, `miss`, …).
        pub cache: String,
        /// Warm worker threads.
        pub threads: u64,
        /// Graphics backend.
        pub backend: String,
        /// The driver's own shader cache: `on`, `off` or `unknown`.
        pub driver_cache: String,
    }
}

optional_fields! {
    /// Audio output.
    pub struct AudioInfo {
        /// Output suspends.
        pub suspends: u64,
        /// Of which caused by slow frames.
        pub throttle_suspends: u64,
    }
}

optional_fields! {
    /// Error counts.
    pub struct ErrorsInfo {
        /// wgpu uncaptured errors.
        pub gpu_errors: u64,
        /// Device losses.
        pub device_lost: u64,
        /// Surface acquire errors.
        pub surface_errors: u64,
        /// Panics.
        pub panics: u64,
    }
}

optional_fields! {
    /// One clean-exit step.
    pub struct ExitStepInfo {
        /// Step name.
        pub step: String,
        /// Its duration, ms.
        pub ms: f64,
    }
}

optional_fields! {
    /// How the session ended.
    pub struct ExitInfo {
        /// Left through the clean exit.
        pub clean: bool,
        /// The clean exit's duration, ms.
        pub ms: f64,
        /// A last-resort exit watchdog was armed.
        pub watchdog: bool,
        /// A caught panic ended the session.
        pub panic: bool,
        /// Each step's time.
        pub steps: Vec<ExitStepInfo>,
    }
}

optional_fields! {
    /// How the previous session ended, when this one could tell.
    pub struct PreviousExit {
        /// It reached its clean exit.
        pub clean: bool,
        /// Its last partial summary (an unclean exit only).
        pub last_partial: Value,
    }
}

/// The `summary.json` record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSummary {
    /// [`SUMMARY_SCHEMA`].
    pub schema: u32,
    /// `true` in `summary.partial.json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial: Option<bool>,
    /// The engine build. A summary written before the rename to Regia Engine (the previous
    /// session of an upgraded game, an alpha log) has it under the legacy key (ADR-1429).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "ferrite")] // rename-keep: the key v0.1.x summaries were written with
    pub regia: Option<RegiaInfo>,
    /// The host's free-form section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<Value>,
    /// The machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfo>,
    /// The display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayInfo>,
    /// Graphics settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<SettingsInfo>,
    /// Session totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    /// Frame times over the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<FrameStats>,
    /// Frame times per map.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames_by_map: Option<BTreeMap<String, FrameStats>>,
    /// Hitches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hitches: Option<HitchInfo>,
    /// Frames of a second or more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalls: Option<Vec<Stall>>,
    /// Memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryInfo>,
    /// Shaders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shaders: Option<ShadersInfo>,
    /// Audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioInfo>,
    /// Errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<ErrorsInfo>,
    /// How this session ended (absent in a partial).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitInfo>,
    /// How the previous session ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_exit: Option<PreviousExit>,
}

impl SessionSummary {
    /// Pretty JSON text.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }
}

/// Rounds to two decimals.
fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn pct(n: u64, of: u64) -> f64 {
    if of == 0 {
        0.0
    } else {
        r2(n as f64 * 100.0 / of as f64)
    }
}

/// The 30 fps frame time (the `ge_30` edge): a frame of exactly this length is not over it.
const OVER_30FPS_MS: f32 = 1000.0 / 30.0;

/// Frame-time statistics of one set of frames.
struct FrameAgg {
    hist: Histogram,
    frames: u64,
    over_33: u64,
    over_50: u64,
}

impl FrameAgg {
    fn new() -> Self {
        Self {
            hist: Histogram::new(),
            frames: 0,
            over_33: 0,
            over_50: 0,
        }
    }

    fn add(&mut self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        self.hist.add(ms);
        self.frames += 1;
        self.over_33 += u64::from(ms > OVER_30FPS_MS);
        self.over_50 += u64::from(ms > 50.0);
    }

    fn stats(&self, bound: Option<&str>, with_count: bool) -> FrameStats {
        // An empty aggregate has no percentiles: report none rather than 0 ms.
        let some = |v: f64| (self.frames > 0).then_some(v);
        FrameStats {
            frames: with_count.then_some(self.frames),
            p50_ms: some(r2(f64::from(self.hist.percentile(0.5)))),
            p95_ms: some(r2(f64::from(self.hist.percentile(0.95)))),
            p99_ms: some(r2(f64::from(self.hist.percentile(0.99)))),
            over_33ms_pct: some(pct(self.over_33, self.frames)),
            over_50ms_pct: some(pct(self.over_50, self.frames)),
            bound: bound.map(str::to_owned),
        }
    }
}

/// What the frame thread accumulates over the session (fixed buffers; nothing here allocates per
/// frame except when a new map or stall appears).
pub(crate) struct SummaryAgg {
    all: FrameAgg,
    maps: Vec<(String, FrameAgg)>,
    active: Option<usize>,
    seen_epoch: u64,
    phase: String,
    gpu_sum: f64,
    gpu_n: u64,
    cpu_sum: f64,
    cpu_n: u64,
    causes: BTreeMap<String, u64>,
    pub(crate) stalls: Vec<Stall>,
    /// Hitches the rate limiter held back, over the session.
    pub hitches_suppressed: u64,
    /// Peak GPU allocations seen by a summary, bytes.
    pub vram_peak: u64,
    /// The texture budget was exceeded in some window.
    pub budget_exceeded: bool,
    /// Startup shader numbers (set when `shaders_ready` is emitted).
    pub shaders: Option<ShadersInfo>,
}

impl SummaryAgg {
    pub(crate) fn new() -> Self {
        Self {
            all: FrameAgg::new(),
            maps: Vec::new(),
            active: None,
            seen_epoch: phase::epoch(),
            phase: String::new(),
            gpu_sum: 0.0,
            gpu_n: 0,
            cpu_sum: 0.0,
            cpu_n: 0,
            causes: BTreeMap::new(),
            stalls: Vec::new(),
            hitches_suppressed: 0,
            vram_peak: 0,
            budget_exceeded: false,
            shaders: None,
        }
    }

    /// Adds one frame (not a gap). Allocation-free unless the host's markers changed.
    pub(crate) fn add_frame(&mut self, frame_ms: f32, cpu_ms: f32, gpu_ms: f32) {
        if !(frame_ms.is_finite() && cpu_ms.is_finite() && gpu_ms.is_finite()) {
            return;
        }
        if phase::epoch() != self.seen_epoch {
            self.sync_phase();
        }
        self.all.add(frame_ms);
        if let Some(i) = self.active {
            self.maps[i].1.add(frame_ms);
        }
        if cpu_ms > 0.0 {
            self.cpu_sum += f64::from(cpu_ms);
            self.cpu_n += 1;
        }
        if gpu_ms > 0.0 {
            self.gpu_sum += f64::from(gpu_ms);
            self.gpu_n += 1;
        }
    }

    fn sync_phase(&mut self) {
        self.seen_epoch = phase::epoch();
        let now = phase::now();
        self.active = now.map.as_deref().and_then(|name| {
            if let Some(i) = self.maps.iter().position(|(n, _)| n == name) {
                Some(i)
            } else if self.maps.len() < MAX_MAP_FRAMES {
                self.maps.push((name.to_owned(), FrameAgg::new()));
                Some(self.maps.len() - 1)
            } else {
                None
            }
        });
        self.phase = now.phase;
    }

    /// Notes a hitch of kind `cause`.
    pub(crate) fn hitch_cause(&mut self, cause: &str) {
        if let Some(n) = self.causes.get_mut(cause) {
            *n += 1;
        } else if self.causes.len() < MAX_CAUSES {
            self.causes.insert(cause.to_owned(), 1);
        } else {
            *self.causes.entry("other".into()).or_insert(0) += 1;
        }
    }

    /// Notes a frame of [`STALL_MS`] or more that ended at `t_s`.
    pub(crate) fn stall(&mut self, t_s: f64, ms: f32) {
        // JSON has no infinity: one such sample would make the whole summary unserializable.
        if !ms.is_finite() || !t_s.is_finite() {
            return;
        }
        if phase::epoch() != self.seen_epoch {
            self.sync_phase();
        }
        if self.stalls.len() < MAX_STALLS {
            self.stalls.push(Stall {
                t_s: Some(r2(t_s)),
                ms: Some(r2(f64::from(ms))),
                phase: Some(self.phase.clone()),
            });
        }
    }

    fn bound(&self) -> &'static str {
        if self.gpu_n == 0 || self.cpu_n == 0 {
            "unknown"
        } else if self.gpu_sum / self.gpu_n as f64 > self.cpu_sum / self.cpu_n as f64 {
            "gpu"
        } else {
            "cpu"
        }
    }

    /// The frame-derived sections of the summary.
    pub(crate) fn fill(&self, out: &mut SessionSummary, hitch_count: u64) {
        out.frames = Some(self.all.stats(Some(self.bound()), false));
        if !self.maps.is_empty() {
            out.frames_by_map = Some(
                self.maps
                    .iter()
                    .map(|(name, agg)| (name.clone(), agg.stats(None, true)))
                    .collect(),
            );
        }
        let mut causes: Vec<(String, u64)> = self.causes.iter().map(|(k, v)| (k.clone(), *v)).collect();
        causes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        causes.truncate(8);
        out.hitches = Some(HitchInfo {
            count: Some(hitch_count),
            suppressed: Some(self.hitches_suppressed),
            top_causes: Some(causes),
        });
        out.stalls = Some(self.stalls.clone());
    }
}

// ---------------------------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------------------------

/// Where a session's summary files go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryPaths {
    /// The final `summary.json`.
    pub final_path: PathBuf,
    /// The periodic `summary.partial.json`.
    pub partial_path: PathBuf,
}

impl SummaryPaths {
    /// The summary files for a session whose final summary is `final_path`; the partial is
    /// `final_path` with `.partial` before `.json`.
    #[must_use]
    pub fn from_final(final_path: impl Into<PathBuf>) -> Self {
        let final_path = final_path.into();
        let partial_path = final_path.with_extension("partial.json");
        Self {
            final_path,
            partial_path,
        }
    }

    /// The default for a session log at `jsonl`: `<stem>.summary.json` next to it.
    #[must_use]
    pub fn beside_log(jsonl: &Path) -> Self {
        let stem = jsonl.file_stem().and_then(|s| s.to_str()).unwrap_or("regia");
        Self::from_final(jsonl.with_file_name(format!("{stem}.summary.json")))
    }
}

/// Writes `text` to `path` through a temporary file in the same folder and an atomic rename, so a
/// reader (or a crash) never sees half a file. Each call has its own temp name (process id and a
/// counter), so two writers of one path (the exit-time write and the panic hook's) never write
/// into the same temp file; the last rename wins with a whole file (ADR-2396).
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_data()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// How the newest earlier session in `paths`' folder (or, for a folder-per-session layout with
/// files named `summary.json`, in a sibling folder) ended: `None` when there is none.
/// Unclean = a partial without a final, a partial newer than the final, or a final whose
/// `exit.clean` is false.
#[must_use]
pub fn detect_previous_exit(paths: &SummaryPaths) -> Option<PreviousExit> {
    let dir = paths.final_path.parent()?;
    let final_name = paths.final_path.file_name()?.to_str()?;
    let mut groups: Vec<Group> = Vec::new();
    if final_name == "summary.json" {
        // One folder per session: look at the sibling folders.
        for entry in fs::read_dir(dir.parent()?).ok()?.flatten() {
            if entry.path().is_dir() {
                groups.push(Group::read(
                    &entry.path().join("summary.json"),
                    &entry.path().join("summary.partial.json"),
                ));
            }
        }
    } else {
        // `beside_log` names are `<stem>.summary.json`; a custom path (`SummaryPaths::from_final`)
        // is `<stem>.json` with its partial `<stem>.partial.json`.
        let suffix = if final_name.ends_with(".summary.json") {
            ".summary.json"
        } else {
            ".json"
        };
        let partial_suffix = suffix.replace(".json", ".partial.json");
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(stem) = name.strip_suffix(partial_suffix.as_str()) {
                let final_file = dir.join(format!("{stem}{suffix}"));
                if !final_file.exists() {
                    groups.push(Group::read(&final_file, &dir.join(name)));
                }
            } else if let Some(stem) = name.strip_suffix(suffix) {
                groups.push(Group::read(
                    &dir.join(name),
                    &dir.join(format!("{stem}{partial_suffix}")),
                ));
            }
        }
    }
    let newest = groups
        .into_iter()
        .filter(|g| g.newest().is_some())
        .max_by_key(Group::newest)?;
    if newest.unclean() || newest.final_says_unclean() {
        let last_partial = newest
            .partial_path
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|text| serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)));
        Some(PreviousExit {
            clean: Some(false),
            last_partial,
        })
    } else {
        Some(PreviousExit {
            clean: Some(true),
            last_partial: None,
        })
    }
}

/// The summary files of one earlier session.
struct Group {
    final_path: PathBuf,
    final_mtime: Option<SystemTime>,
    partial_mtime: Option<SystemTime>,
    partial_path: Option<PathBuf>,
}

impl Group {
    fn read(final_path: &Path, partial_path: &Path) -> Self {
        let mtime = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
        let partial_mtime = mtime(partial_path);
        Self {
            final_path: final_path.to_path_buf(),
            final_mtime: mtime(final_path),
            partial_mtime,
            partial_path: partial_mtime.map(|_| partial_path.to_path_buf()),
        }
    }

    fn newest(&self) -> Option<SystemTime> {
        self.final_mtime.max(self.partial_mtime)
    }

    /// A final written at the start of teardown says `exit.clean: false` until the sequence
    /// finishes, so a crash or hang during teardown leaves a final that admits it.
    fn final_says_unclean(&self) -> bool {
        self.final_mtime.is_some()
            && fs::read_to_string(&self.final_path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .is_some_and(|v| v["exit"]["clean"] == Value::Bool(false))
    }

    fn unclean(&self) -> bool {
        match (self.final_mtime, self.partial_mtime) {
            (None, Some(_)) => true,
            (Some(f), Some(p)) => p > f,
            _ => false,
        }
    }
}

enum Msg {
    Partial(Box<SessionSummary>),
}

/// The background writer of the partial summaries. Detects the previous session's exit first.
pub(crate) struct SummarySink {
    pub paths: SummaryPaths,
    tx: Option<SyncSender<Msg>>,
    pub previous: Arc<Mutex<Option<PreviousExit>>>,
    thread: Option<JoinHandle<()>>,
    /// Partials written, for tests and the report.
    pub written: Arc<std::sync::atomic::AtomicU64>,
}

impl SummarySink {
    /// Starts the writer thread; it checks the previous session before its first write.
    pub(crate) fn start(paths: SummaryPaths) -> Option<Self> {
        let (tx, rx): (SyncSender<Msg>, Receiver<Msg>) = std::sync::mpsc::sync_channel(2);
        let previous = Arc::new(Mutex::new(None));
        let written = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let thread = {
            let paths = paths.clone();
            let previous = previous.clone();
            let written = written.clone();
            std::thread::Builder::new()
                .name("regia-telemetry-summary".into())
                .spawn(move || {
                    run_writer(&paths, &rx, &previous, &written);
                })
                .ok()?
        };
        Some(Self {
            paths,
            tx: Some(tx),
            previous,
            thread: Some(thread),
            written,
        })
    }

    /// Queues a partial summary (dropped when the writer is still busy with the last one).
    pub(crate) fn send_partial(&self, summary: SessionSummary) {
        if let Some(tx) = &self.tx
            && let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
                tx.try_send(Msg::Partial(Box::new(summary)))
        {
            tracing::debug!(target: crate::target::TELEMETRY, "partial summary skipped: the writer is busy");
        }
    }

    /// The previous session's exit, once the writer thread has looked.
    pub(crate) fn previous_exit(&self) -> Option<PreviousExit> {
        self.previous
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for SummarySink {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            // The writer ends as soon as its queue drains; never wait long on a stuck disk.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

fn run_writer(
    paths: &SummaryPaths,
    rx: &Receiver<Msg>,
    previous: &Mutex<Option<PreviousExit>>,
    written: &std::sync::atomic::AtomicU64,
) {
    if let Some(parent) = paths.partial_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let found = detect_previous_exit(paths);
    if let Some(prev) = &found
        && prev.clean == Some(false)
    {
        let seconds = prev
            .last_partial
            .as_ref()
            .and_then(|p| p.get("session"))
            .and_then(|s| s.get("seconds"))
            .cloned();
        phase::emit(
            tracing::Level::WARN,
            "previous_session_unclean",
            serde_json::json!({ "last_partial_session_seconds": seconds }),
        );
    }
    *previous.lock().unwrap_or_else(PoisonError::into_inner) = found;
    while let Ok(Msg::Partial(summary)) = rx.recv() {
        match write_atomic(&paths.partial_path, &summary.to_json()) {
            Ok(()) => {
                written.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => tracing::warn!(
                target: crate::target::TELEMETRY,
                "cannot write {}: {e}",
                paths.partial_path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn a_steady_30_fps_frame_is_not_over_33ms() {
        let mut agg = FrameAgg::new();
        agg.add(1000.0 / 30.0);
        agg.add(33.4);
        assert_eq!(agg.over_33, 1);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("regia-summary-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn full() -> SessionSummary {
        SessionSummary {
            schema: SUMMARY_SCHEMA,
            partial: Some(true),
            regia: Some(RegiaInfo {
                version: Some("0.1.9".into()),
                git: Some("abc".into()),
                profile: Some("release".into()),
            }),
            host: Some(json!({ "app": "do27", "network": { "role": "host", "peers": 2 } })),
            system: Some(SystemInfo {
                os: Some("Windows 11".into()),
                cpu: Some("AMD Ryzen 7 7800X3D".into()),
                cpu_threads: Some(16),
                ram_gb: Some(31.2),
                vram_gb: Some(16.0),
                ..SystemInfo::default()
            }),
            display: Some(DisplayInfo {
                width: Some(3440),
                monitors: Some(vec![MonitorInfo {
                    name: Some("DP-1".into()),
                    current: Some(true),
                    ..MonitorInfo::default()
                }]),
                ..DisplayInfo::default()
            }),
            settings: Some(SettingsInfo {
                upscaler: Some("TAAU".into()),
                auto_plan: Some(json!({ "tier": "High" })),
                ..SettingsInfo::default()
            }),
            session: Some(SessionInfo {
                seconds: Some(12.5),
                frames: Some(700),
                maps: Some(vec!["bore_twelve".into()]),
            }),
            frames: Some(FrameStats {
                p50_ms: Some(18.2),
                bound: Some("cpu".into()),
                ..FrameStats::default()
            }),
            frames_by_map: Some(BTreeMap::from([(
                "bore_twelve".to_owned(),
                FrameStats {
                    frames: Some(600),
                    p99_ms: Some(54.0),
                    ..FrameStats::default()
                },
            )])),
            hitches: Some(HitchInfo {
                count: Some(284),
                suppressed: Some(12),
                top_causes: Some(vec![("terrain_upload".into(), 41)]),
            }),
            stalls: Some(vec![Stall {
                t_s: Some(349.7),
                ms: Some(3120.0),
                phase: Some("scene_load".into()),
            }]),
            memory: Some(MemoryInfo {
                vram_peak_gb: Some(7.6),
                rss_peak_gb: None,
                texture_budget_exceeded: Some(true),
            }),
            shaders: Some(ShadersInfo {
                pipelines: Some(146),
                cache: Some("miss".into()),
                driver_cache: Some("unknown".into()),
                ..ShadersInfo::default()
            }),
            audio: Some(AudioInfo {
                suspends: Some(0),
                throttle_suspends: Some(0),
            }),
            errors: Some(ErrorsInfo::default()),
            exit: Some(ExitInfo {
                clean: Some(true),
                ms: Some(255.0),
                steps: Some(vec![ExitStepInfo {
                    step: Some("drop_device".into()),
                    ms: Some(80.0),
                }]),
                ..ExitInfo::default()
            }),
            previous_exit: Some(PreviousExit {
                clean: Some(false),
                last_partial: Some(json!({ "session": { "seconds": 3 } })),
            }),
        }
    }

    #[test]
    fn a_full_summary_survives_a_json_round_trip() {
        let summary = full();
        let text = summary.to_json();
        let back: SessionSummary = serde_json::from_str(&text).expect("parses");
        assert_eq!(back, summary);
    }

    #[test]
    fn a_summary_written_before_the_rename_keeps_its_engine_section() {
        let legacy = regia_core::brand::LEGACY_MAIN_CRATE;
        let text = format!(r#"{{"schema":1,"{legacy}":{{"version":"0.1.29","profile":"release"}}}}"#);
        let parsed: SessionSummary = serde_json::from_str(&text).expect("a v0.1.x summary parses");
        let engine = parsed
            .regia
            .expect("the engine section is read from the legacy key");
        assert_eq!(engine.version.as_deref(), Some("0.1.29"));
        assert_eq!(engine.profile.as_deref(), Some("release"));
    }

    #[test]
    fn every_section_is_optional() {
        let empty: SessionSummary = serde_json::from_str("{}").expect("an empty object is a summary");
        assert_eq!(empty, SessionSummary::default());
        assert_eq!(empty.to_json().replace([' ', '\n'], ""), r#"{"schema":0}"#);
        // Unknown sections and fields from a newer engine are ignored, known ones kept.
        let newer: SessionSummary =
            serde_json::from_str(r#"{"schema":2,"future":{"x":1},"frames":{"p50_ms":9.5,"extra":1}}"#)
                .expect("newer records parse");
        assert_eq!(newer.frames.and_then(|f| f.p50_ms), Some(9.5));
        // The spec's example, with its tuples and nested host section.
        let spec = r#"{"schema":1,"hitches":{"count":284,"suppressed":12,"top_causes":[["terrain_upload",41]]},
            "stalls":[{"t_s":349.7,"ms":3120,"phase":"scene_load"}],"exit":{"clean":true,"ms":255,"watchdog":false}}"#;
        let parsed: SessionSummary = serde_json::from_str(spec).expect("spec example parses");
        assert_eq!(
            parsed.hitches.and_then(|h| h.top_causes).map(|c| c[0].1),
            Some(41)
        );
    }

    #[test]
    fn the_partial_is_replaced_atomically_and_leaves_no_temp_file() {
        let dir = temp_dir("atomic");
        let path = dir.join("a.summary.partial.json");
        write_atomic(&path, "{\"schema\":1,\"n\":1}").expect("first write");
        write_atomic(&path, "{\"schema\":1,\"n\":2}").expect("second write replaces");
        assert_eq!(fs::read_to_string(&path).expect("read"), "{\"schema\":1,\"n\":2}");
        let names: Vec<String> = fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["a.summary.partial.json".to_owned()],
            "no .tmp is left"
        );
        // A write that fails keeps the old file and leaves no temp file behind.
        let missing = dir.join("no-such-folder").join("x.json");
        assert!(write_atomic(&missing, "x").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_without_a_final_is_an_unclean_previous_exit() {
        let dir = temp_dir("prev-same-dir");
        // Explicit modification times, seconds apart: sleeps of 30 ms tied on filesystems with
        // coarse timestamps, and `read_dir` order then picked the group (audit-v035-flaky M4).
        let now = std::time::SystemTime::now();
        let stamp = |path: &std::path::Path, secs_ago: u64| {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| f.set_modified(now - Duration::from_secs(secs_ago)))
                .expect("set mtime");
        };
        // An older clean session, then a newer one that only has its partial.
        fs::write(dir.join("old-1.summary.json"), "{\"schema\":1}").expect("write");
        stamp(&dir.join("old-1.summary.json"), 30);
        fs::write(
            dir.join("hung-2.summary.partial.json"),
            r#"{"schema":1,"partial":true,"session":{"seconds":412.0}}"#,
        )
        .expect("write");
        stamp(&dir.join("hung-2.summary.partial.json"), 20);
        let paths = SummaryPaths::beside_log(&dir.join("new-3.jsonl"));
        assert_eq!(paths.final_path, dir.join("new-3.summary.json"));
        assert_eq!(paths.partial_path, dir.join("new-3.summary.partial.json"));
        let prev = detect_previous_exit(&paths).expect("a previous session");
        assert_eq!(prev.clean, Some(false));
        assert_eq!(
            prev.last_partial
                .as_ref()
                .and_then(|p| p["session"]["seconds"].as_f64()),
            Some(412.0)
        );

        // The hung session's final appears (a later run reports it): now it is clean.
        fs::write(dir.join("hung-2.summary.json"), "{\"schema\":1}").expect("write");
        stamp(&dir.join("hung-2.summary.json"), 10);
        let prev = detect_previous_exit(&paths).expect("a previous session");
        assert_eq!(prev.clean, Some(true));
        assert!(prev.last_partial.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_final_written_at_quit_start_that_says_unclean_is_an_unclean_previous_exit() {
        let dir = temp_dir("prev-final-unclean");
        fs::write(
            dir.join("a-1.summary.partial.json"),
            r#"{"schema":1,"partial":true}"#,
        )
        .expect("write");
        // The final is newer than the partial but was never finished (teardown crashed).
        fs::write(
            dir.join("a-1.summary.json"),
            r#"{"schema":1,"exit":{"clean":false}}"#,
        )
        .expect("write");
        let paths = SummaryPaths::beside_log(&dir.join("b-2.jsonl"));
        let prev = detect_previous_exit(&paths).expect("a previous session");
        assert_eq!(prev.clean, Some(false));
        fs::write(
            dir.join("a-1.summary.json"),
            r#"{"schema":1,"exit":{"clean":true}}"#,
        )
        .expect("write");
        let prev = detect_previous_exit(&paths).expect("a previous session");
        assert_eq!(prev.clean, Some(true));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_per_session_layouts_are_read_through_the_sibling_folders() {
        let root = temp_dir("prev-folders");
        let old = root.join("session-1");
        fs::create_dir_all(&old).expect("mkdir");
        fs::write(old.join("summary.partial.json"), r#"{"schema":1,"partial":true}"#).expect("write");
        let me = root.join("session-2");
        fs::create_dir_all(&me).expect("mkdir");
        let paths = SummaryPaths::from_final(me.join("summary.json"));
        assert_eq!(paths.partial_path, me.join("summary.partial.json"));
        let prev = detect_previous_exit(&paths).expect("previous");
        assert_eq!(prev.clean, Some(false));
        // Nothing earlier at all: nothing to say.
        let alone = temp_dir("prev-none");
        assert!(detect_previous_exit(&SummaryPaths::beside_log(&alone.join("x.jsonl"))).is_none());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&alone);
    }

    #[test]
    fn the_sink_writes_a_partial_and_reports_the_previous_exit() {
        let dir = temp_dir("sink");
        fs::write(
            dir.join("crashed.summary.partial.json"),
            r#"{"schema":1,"partial":true}"#,
        )
        .expect("write");
        let paths = SummaryPaths::beside_log(&dir.join("now.jsonl"));
        let sink = SummarySink::start(paths.clone()).expect("thread");
        sink.send_partial(full());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sink.written.load(std::sync::atomic::Ordering::Relaxed) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sink.previous_exit().map(|p| p.clean), Some(Some(false)));
        let text = fs::read_to_string(&paths.partial_path).expect("the partial exists");
        let written: SessionSummary = serde_json::from_str(&text).expect("the partial parses");
        assert_eq!(
            written,
            full(),
            "the partial carries exactly the summary that was sent"
        );
        drop(sink);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_finite_frame_times_move_no_counter() {
        let mut agg = SummaryAgg::new();
        agg.add_frame(16.0, 6.0, 8.0);
        agg.add_frame(16.0, 6.0, f32::INFINITY);
        agg.add_frame(f32::INFINITY, 6.0, 8.0);
        agg.add_frame(16.0, f32::NAN, 8.0);
        assert_eq!(agg.all.frames, 1);
        assert_eq!(agg.all.over_50, 0);
        assert_eq!(agg.gpu_n, 1);
        assert_eq!(agg.cpu_n, 1);
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    #[test]
    fn an_empty_aggregate_reports_no_percentiles() {
        let s = FrameAgg::new().stats(None, true);
        assert_eq!(s.frames, Some(0));
        assert_eq!(s.p50_ms, None);
        assert_eq!(s.p99_ms, None);
        assert_eq!(s.over_33ms_pct, None);
    }

    #[test]
    fn a_negative_frame_is_not_counted() {
        let mut agg = FrameAgg::new();
        agg.add(-1.0);
        agg.add(f32::NEG_INFINITY);
        assert_eq!(agg.frames, 0);
        agg.add(10.0);
        assert_eq!(agg.frames, 1);
        assert!(agg.stats(None, true).p50_ms.is_some());
    }
}

#[cfg(test)]
mod custom_path_tests {
    use super::*;

    #[test]
    fn a_custom_final_name_finds_its_own_partial() {
        let dir = std::env::temp_dir().join(format!("regia-summary-custom-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        fs::write(
            dir.join("session.partial.json"),
            r#"{"schema":1,"partial":true,"session":{"seconds":7.0}}"#,
        )
        .expect("write");
        let paths = SummaryPaths::from_final(dir.join("next.json"));
        assert_eq!(paths.partial_path, dir.join("next.partial.json"));
        let prev = detect_previous_exit(&paths).expect("the crashed session is found");
        assert_eq!(prev.clean, Some(false));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Audit 11-03 crash L5 (ADR-2396): every writer used the one temp name `<path>.tmp`, so the
    /// exit-time write and the panic hook's write of the same summary truncated each other's temp
    /// file (a torn rename, or a failed one). A writer holding that name must not stop another.
    #[test]
    fn two_writers_of_one_path_do_not_share_a_temp_file() {
        let dir = regia_test_utils::unique_temp_dir("summary-temp-name");
        let path = dir.join("a.summary.json");
        // Another writer is mid-write on the old fixed temp name: stand in for it with a
        // folder there, which a shared name can neither truncate nor rename over the target.
        let mut shared = path.as_os_str().to_owned();
        shared.push(".tmp");
        fs::create_dir_all(PathBuf::from(&shared)).expect("the other writer's temp");
        write_atomic(&path, "{\"schema\":1}").expect("a write with its own temp name");
        assert_eq!(fs::read_to_string(&path).expect("read"), "{\"schema\":1}");
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "a.summary.json" && n != "a.summary.json.tmp")
            .collect();
        assert!(leftovers.is_empty(), "no temp file is left: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
