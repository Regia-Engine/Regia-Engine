// SPDX-License-Identifier: MPL-2.0
//! The automatic crash bundle: a panic hook that writes the same bundle as the prompt (no
//! screenshot: the renderer may be mid-frame or the panic may be on its thread), and the
//! next-launch notice that tells the player where it is.
//!
//! The hook cannot reach the ECS world, so the parts it needs live in process globals: the
//! frame-time ring (one sample pushed per frame, no allocation) and a context the plugin
//! refreshes only when something changes (settings, render problems, host fields). The hook
//! chains to the previous one, so `regia_platform`'s `crash-*.txt` report is still written.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use super::bundle::{
    BundleFile, BundleInfo, BundleInput, CrashDetails, FRAME_RING_CAPACITY, FeedbackCategory, FrameRing,
    list_bundles, unix_ms_now, write_bundle,
};

/// The file in the feedback folder listing crash bundles the player was already told about.
pub const ANNOUNCED_FILE: &str = "announced.txt";

/// Two panics closer than this write one bundle (a caught panic in a loop must not fill the disk).
const MIN_CRASH_INTERVAL_MS: u64 = 5_000;

/// Uncaptured validation or internal errors closer than this write one bundle (spec 54 § 4.1).
const MIN_VALIDATION_INTERVAL_MS: u64 = 60_000;

/// What the crash hook writes besides the frames.
#[derive(Debug, Clone)]
pub(crate) struct CrashContext {
    pub dir: Option<PathBuf>,
    pub settings_ron: Option<String>,
    pub render_problems: Option<Vec<(String, String)>>,
    pub host: Vec<(String, String)>,
    pub window_s: f64,
    pub log_lines: usize,
    pub keep: usize,
    pub enabled: bool,
}

impl Default for CrashContext {
    fn default() -> Self {
        Self {
            dir: None,
            settings_ron: None,
            render_problems: None,
            host: Vec::new(),
            window_s: 30.0,
            log_lines: 400,
            keep: 50,
            enabled: true,
        }
    }
}

pub(super) static FRAMES: OnceLock<Mutex<FrameRing>> = OnceLock::new();
static CONTEXT: OnceLock<Mutex<CrashContext>> = OnceLock::new();
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
/// When the last panic bundle was written (ms since the epoch; 0 = never). The native handler
/// reads it to skip the abort that follows a panic under `panic = "abort"`.
pub(super) static LAST_CRASH_MS: AtomicU64 = AtomicU64::new(0);

/// When the last uncaptured validation or internal breadcrumb bundle was written.
static LAST_VALIDATION_MS: AtomicU64 = AtomicU64::new(0);

/// The process-wide frame ring (allocated on first use).
pub(crate) fn frames() -> &'static Mutex<FrameRing> {
    FRAMES.get_or_init(|| Mutex::new(FrameRing::new(FRAME_RING_CAPACITY)))
}

pub(crate) fn context() -> &'static Mutex<CrashContext> {
    CONTEXT.get_or_init(|| Mutex::new(CrashContext::default()))
}

/// Runs `f` on the crash context (poison-tolerant).
pub(crate) fn with_context<R>(f: impl FnOnce(&mut CrashContext) -> R) -> R {
    f(&mut context().lock().unwrap_or_else(PoisonError::into_inner))
}

/// Changes the crash context and hands the new one to the native crash monitor (when running).
/// Call only when something changed: the monitor gets a copy each time.
pub(crate) fn update_context(f: impl FnOnce(&mut CrashContext)) {
    with_context(f);
    super::native::publish_context();
}

/// Points crash bundles at `dir` (the plugin does this with the feedback folder).
pub fn set_crash_dir(dir: Option<PathBuf>) {
    update_context(|c| c.dir = dir);
}

/// The folder crash bundles go to (`None` until configured).
#[must_use]
pub fn crash_dir() -> Option<PathBuf> {
    with_context(|c| c.dir.clone())
}

/// Installs the crash-bundle panic hook, chaining to the previous hook. Idempotent.
pub fn install_crash_hook() {
    if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(non-string panic payload)".to_owned());
        let location = info.location().map_or_else(
            || "<unknown>".to_owned(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        let thread = std::thread::current().name().unwrap_or("<unnamed>").to_owned();
        let details = CrashDetails {
            message,
            location,
            backtrace: std::backtrace::Backtrace::force_capture().to_string(),
            thread,
            label: String::new(),
        };
        let _ = write_crash_bundle(details);
        previous(info);
    }));
}

/// Writes a crash bundle now from the global state, best-effort and panic-free. Returns `None`
/// when no folder is configured, crash bundles are off, another was written in the last 5 s, or
/// the write failed.
pub fn write_crash_bundle(details: CrashDetails) -> Option<BundleInfo> {
    write_crash_bundle_with(details, Vec::new(), None)
}

/// Where crash bundles get the newest telemetry frame summary (the telemetry plugin sets
/// `regia_telemetry::last_summary_json`; this crate cannot depend on it).
static SUMMARY_SOURCE: OnceLock<fn() -> Option<std::sync::Arc<str>>> = OnceLock::new();

/// Sets where a device-lost crash bundle gets the newest frame summary (first call wins).
pub fn set_summary_source(source: fn() -> Option<std::sync::Arc<str>>) {
    let _ = SUMMARY_SOURCE.set(source);
}

/// The crash bundle of a lost GPU device or an unavailable window surface
/// ([`regia_render::DeviceLostHooks`], added by the feedback plugin): `crash.txt` starts with
/// the reason ([`regia_render::DeviceLostInfo::kind`]: `device_lost` or
/// `surface_unavailable`) and holds wgpu's message, the uncaptured error count, the GPU memory
/// by bucket at the last probe sample with the out-of-memory state
/// ([`regia_render::memory::memory_summary_line`]) and the newest telemetry frame summary;
/// `host.json` gets `crash_reason` = that reason, plus `gpu_memory`, `gpu_oom_errors` and
/// `texture_budget_steps`. The next launch announces it like any crash bundle. `None` when crash
/// bundles are off, no folder is configured, or another crash bundle was written in the last 5 s.
pub fn write_device_lost_bundle(info: &regia_render::DeviceLostInfo) -> Option<BundleInfo> {
    let summary = SUMMARY_SOURCE.get().and_then(|source| source()).map_or_else(
        || "none (telemetry off or no summary yet)".to_owned(),
        |s| s.to_string(),
    );
    let kind = info.kind();
    let what = if kind == "device_lost" {
        "the GPU device was lost"
    } else {
        "no window surface could be created or recovered"
    };
    let details = CrashDetails {
        message: format!("{what} ({}): {}", info.reason, info.message),
        location: "regia_render::gpu_health (device-lost callback or surface creation)".to_owned(),
        backtrace: format!(
            "no backtrace: {what}; the game stopped rendering and exited with code {}\n\
             wgpu errors since start: {}\n\
             {}\n\
             last telemetry frame summary: {summary}\n",
            info.exit_code(),
            regia_render::gpu_health::gpu_error_count(),
            regia_render::memory::memory_summary_line(),
        ),
        thread: std::thread::current().name().unwrap_or("<unnamed>").to_owned(),
        label: kind.to_owned(),
    };
    let crumbs = device_lost_breadcrumbs(info);
    write_crash_bundle_with(
        details,
        vec![
            ("crash_reason".to_owned(), kind.to_owned()),
            ("device_lost_reason".to_owned(), info.reason.to_owned()),
            // v0.1.16 (ADR-223): what filled the GPU and the out-of-memory state.
            (
                "gpu_memory".to_owned(),
                regia_render::memory::memory_summary_line(),
            ),
            (
                "gpu_oom_errors".to_owned(),
                format!(
                    "{} uncaught, {} caught",
                    regia_render::gpu_health::gpu_oom_count(),
                    regia_render::gpu_health::captured_oom_count()
                ),
            ),
            (
                "texture_budget_steps".to_owned(),
                regia_render::gpu_health::texture_budget_steps().to_string(),
            ),
        ],
        crumbs,
    )
}

/// The breadcrumb parts of a device-lost bundle (spec 54 § 4.1): the report of the global ring
/// at the loss, as `gpu_breadcrumbs.json` and as the `gpu breadcrumbs:` block of `crash.txt`.
/// `None` when the writer is off, so the bundle is today's baseline (evidence level `none`).
#[must_use]
pub fn device_lost_breadcrumbs(info: &regia_render::DeviceLostInfo) -> Option<super::bundle::GpuBreadcrumbs> {
    let report = regia_render::breadcrumbs::BreadcrumbReport::capture(
        &regia_render::breadcrumbs::snapshot(),
        Some(info),
    );
    if report.level == regia_render::breadcrumbs::EvidenceLevel::None {
        return None;
    }
    Some(super::bundle::GpuBreadcrumbs {
        json: serde_json::to_string(&report).ok()?,
        text: report.crash_text(),
    })
}

/// A copy of the crash context. Never blocks for good: a panic may have happened while this
/// thread held the lock, so it only retries `try_lock` for a short while. The retry matters: the
/// native monitor's publisher thread takes the lock for a moment right after every
/// `update_context`, and the device-lost hook calls `update_context` just before it writes its
/// bundle; a single `try_lock` lost that race now and then and no bundle was written.
fn clone_context() -> Option<CrashContext> {
    let context = CONTEXT.get()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        match context.try_lock() {
            Ok(ctx) => return Some(ctx.clone()),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner().clone()),
            Err(std::sync::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::yield_now();
            }
        }
    }
}

/// Publishes `now` into `last` when `min_interval_ms` has passed since the previous claim
/// (or there was none). One atomic compare-exchange, so two threads cannot both pass the gate.
fn claim_slot(last: &AtomicU64, now: u64, min_interval_ms: u64) -> bool {
    let mut seen = last.load(Ordering::Relaxed);
    loop {
        if seen != 0 && now.saturating_sub(seen) < min_interval_ms {
            return false;
        }
        match last.compare_exchange_weak(seen, now, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => seen = actual,
        }
    }
}

/// Publishes `now` into `last` when `MIN_CRASH_INTERVAL_MS` has passed since the previous crash
/// (or there was none).
fn claim_crash_slot(last: &AtomicU64, now: u64) -> bool {
    claim_slot(last, now, MIN_CRASH_INTERVAL_MS)
}

/// A feedback bundle for an uncaptured validation or internal GPU error (spec 54 § 4.1):
/// `gpu_breadcrumbs.json` with `lost` null, category `bug`, at most once a minute. This does
/// not take the crash-bundle slot. `None` when no folder is configured, bundles are off, one
/// was written in the last minute, or the write failed. The JSON is included even when the
/// breadcrumb evidence level is `none`.
pub fn write_validation_breadcrumb_bundle(kind: &str) -> Option<BundleInfo> {
    let ctx = clone_context()?;
    let dir = ctx.dir.clone().filter(|_| ctx.enabled)?;
    let report =
        regia_render::breadcrumbs::BreadcrumbReport::capture(&regia_render::breadcrumbs::snapshot(), None);
    let json = serde_json::to_string(&report).ok()?;
    let text = report.crash_text();
    let now = unix_ms_now();
    if !claim_slot(&LAST_VALIDATION_MS, now, MIN_VALIDATION_INTERVAL_MS) {
        return None;
    }
    let mut input = BundleInput::new(FeedbackCategory::Bug, format!("uncaptured {kind} error"));
    input.unix_ms = now;
    input.window_s = ctx.window_s;
    input.frames = FRAMES
        .get()
        .and_then(|f| f.try_lock().ok())
        .map(|ring| ring.last_window(ctx.window_s))
        .unwrap_or_default();
    input.settings_ron = ctx.settings_ron;
    input.render_problems = ctx.render_problems;
    input.host = ctx.host;
    input.log_tail = log_tail(ctx.log_lines);
    input.gpu_breadcrumbs = Some(super::bundle::GpuBreadcrumbs { json, text });
    write_bundle(&dir, &input, ctx.keep).ok()
}

fn write_crash_bundle_with(
    details: CrashDetails,
    extra_host: Vec<(String, String)>,
    gpu_breadcrumbs: Option<super::bundle::GpuBreadcrumbs>,
) -> Option<BundleInfo> {
    let ctx = clone_context()?;
    let dir = ctx.dir.clone().filter(|_| ctx.enabled)?;
    let now = unix_ms_now();
    if !claim_crash_slot(&LAST_CRASH_MS, now) {
        return None;
    }
    let mut input = BundleInput::new(FeedbackCategory::Crash, "");
    input.unix_ms = now;
    input.screenshot_missing =
        Some("crash bundles carry no screenshot (the GPU state is unknown after a panic)".into());
    input.window_s = ctx.window_s;
    input.frames = FRAMES
        .get()
        .and_then(|f| f.try_lock().ok())
        .map(|ring| ring.last_window(ctx.window_s))
        .unwrap_or_default();
    input.settings_ron = ctx.settings_ron;
    input.render_problems = ctx.render_problems;
    input.host = ctx.host;
    input.host.extend(extra_host);
    input.log_tail = log_tail(ctx.log_lines);
    input.crash = Some(details);
    input.gpu_breadcrumbs = gpu_breadcrumbs;
    write_bundle(&dir, &input, ctx.keep).ok()
}

/// The newest `lines` log records.
pub(crate) fn log_tail(lines: usize) -> Vec<regia_core::log::ring::LogRecord> {
    regia_core::log::ring::recent(&regia_core::log::ring::LogQuery {
        limit: Some(lines),
        ..Default::default()
    })
    .0
}

/// Crash bundles in `dir` the player has not been told about yet, oldest first.
#[must_use]
pub fn unannounced_crashes(dir: &Path) -> Vec<BundleFile> {
    let announced = std::fs::read_to_string(dir.join(ANNOUNCED_FILE)).unwrap_or_default();
    list_bundles(dir)
        .into_iter()
        .filter(|b| b.category == Some(FeedbackCategory::Crash))
        .filter(|b| !announced.lines().any(|l| l == b.name))
        .collect()
}

/// Records that the player was told about `bundles`.
pub fn mark_announced(dir: &Path, bundles: &[BundleFile]) -> std::io::Result<()> {
    if bundles.is_empty() {
        return Ok(());
    }
    let mut text = std::fs::read_to_string(dir.join(ANNOUNCED_FILE)).unwrap_or_default();
    // Forget names whose bundle is gone, so the file stays small.
    let present: Vec<String> = list_bundles(dir).into_iter().map(|b| b.name).collect();
    text = text
        .lines()
        .filter(|l| present.iter().any(|p| p == l))
        .map(|l| format!("{l}\n"))
        .collect();
    for b in bundles {
        text.push_str(&b.name);
        text.push('\n');
    }
    std::fs::write(dir.join(ANNOUNCED_FILE), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crash-interval gate admits one crash per interval, even from racing threads.
    #[test]
    fn crash_gate_admits_one_per_interval() {
        let last = AtomicU64::new(0);
        assert!(claim_crash_slot(&last, 100_000));
        assert!(!claim_crash_slot(&last, 100_000 + MIN_CRASH_INTERVAL_MS - 1));
        assert!(claim_crash_slot(&last, 100_000 + MIN_CRASH_INTERVAL_MS));

        let shared = AtomicU64::new(0);
        let admitted = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| claim_crash_slot(&shared, 200_000)))
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("join"))
                .filter(|admitted| *admitted)
                .count()
        });
        assert_eq!(admitted, 1);
    }
}
