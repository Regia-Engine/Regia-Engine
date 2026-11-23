// SPDX-License-Identifier: MPL-2.0
//! Crash reporting (`docs/specs/21-tooling-and-packaging.md` § 5): the panic hook writes a
//! self-contained crash report to `logs://crash-<timestamp>.txt`, and the pending reports are
//! listed/read/deleted by the reporter (`regia_crash`) and the agent protocol
//! (`diagnostics.crash_reports`).
//!
//! A crash report captures the panic message and backtrace, the build identity (version, git hash,
//! profile), OS/GPU/driver facts, the active level and frame count, and the last
//! [`CRASH_LOG_LINES`] log records from the ring buffer (`regia_core::log::ring`).
//!
//! Everything here is best-effort and panic-free: a crash reporter that panics would recurse into
//! itself. The hook chains to any previously installed hook — normally the `tracing` hook installed
//! by [`regia_core::log::init`], which also forwards to logcat on Android.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use regia_app::prelude::{Res, Time};
use regia_core::log::civil_date_time;

use crate::services::OsInfo;

/// Number of recent log lines captured into a crash report.
pub const CRASH_LOG_LINES: usize = 200;

/// File-name prefix of a crash report under `logs://`.
pub const CRASH_PREFIX: &str = "crash-";
/// File-name suffix of a crash report.
pub const CRASH_SUFFIX: &str = ".txt";

/// Build identity captured in every crash report (mirrors `build_info.json`, `specs/21` § 4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    /// The game's version.
    pub version: String,
    /// Short git hash.
    pub git_hash: String,
    /// Cargo profile (`dev`, `release` or `shipping`).
    pub profile: String,
    /// The engine version.
    pub engine: String,
}

impl BuildInfo {
    /// Compile-time build identity. Overridable through `REGIA_BUILD_VERSION`,
    /// `REGIA_GIT_HASH` and `REGIA_BUILD_PROFILE` (set in `.cargo/config.toml` or a build
    /// script, e.g. from the `git rev-parse` value `regia package` already records).
    pub fn detect() -> Self {
        Self {
            version: option_env!("REGIA_BUILD_VERSION")
                .unwrap_or(env!("CARGO_PKG_VERSION"))
                .to_owned(),
            git_hash: option_env!("REGIA_GIT_HASH").unwrap_or("unknown").to_owned(),
            profile: option_env!("REGIA_BUILD_PROFILE")
                .unwrap_or(if cfg!(debug_assertions) { "dev" } else { "shipping" })
                .to_owned(),
            engine: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// A pending crash report on disk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CrashFile {
    /// File name (no directory), e.g. `crash-2026-10-06_12-34-56.txt`.
    pub name: String,
    /// Full host path (for reading/deleting directly).
    pub path: PathBuf,
    /// Size in bytes.
    pub bytes: u64,
    /// Modification time, milliseconds since the Unix epoch (0 when unknown).
    pub modified_unix_ms: u64,
}

/// Whether a file name is a crash-report name (`crash-*.txt`).
pub fn is_crash_file_name(name: &str) -> bool {
    name.starts_with(CRASH_PREFIX) && name.ends_with(CRASH_SUFFIX)
}

/// Rejects names that are not a bare crash-report file name (path traversal guard).
fn valid_name(name: &str) -> bool {
    is_crash_file_name(name) && !name.contains('/') && !name.contains('\\') && !name.contains(':')
}

/// The process-global crash context, filled by [`configure`] and [`set_gpu_info`].
struct Context {
    logs_dir: Option<PathBuf>,
    build_info: BuildInfo,
    os_info: OsInfo,
    gpu_info: String,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            logs_dir: None,
            build_info: BuildInfo::detect(),
            os_info: OsInfo::unknown(),
            gpu_info: String::new(),
        }
    }
}

static CONTEXT: OnceLock<Mutex<Context>> = OnceLock::new();
static FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
static ACTIVE_LEVEL: OnceLock<Mutex<String>> = OnceLock::new();
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

fn context() -> &'static Mutex<Context> {
    CONTEXT.get_or_init(|| Mutex::new(Context::default()))
}

fn active_level() -> &'static Mutex<String> {
    ACTIVE_LEVEL.get_or_init(|| Mutex::new(String::new()))
}

/// Configures crash reporting. Called once by [`crate::PlatformPlugin`] build; later calls update
/// the stored values.
pub fn configure(logs_dir: Option<PathBuf>, build_info: BuildInfo, os_info: OsInfo) {
    let mut ctx = context().lock().unwrap_or_else(PoisonError::into_inner);
    ctx.logs_dir = logs_dir;
    ctx.build_info = build_info;
    ctx.os_info = os_info;
}

/// Sets the GPU/driver description. The renderer (`regia_render`) calls this once it has chosen
/// an adapter, so real reports carry a `gpu:` line instead of the `(not reported)` placeholder.
pub fn set_gpu_info(info: impl Into<String>) {
    let mut ctx = context().lock().unwrap_or_else(PoisonError::into_inner);
    ctx.gpu_info = info.into();
}

/// The GPU/driver description last recorded by [`set_gpu_info`] (empty until the renderer reports
/// the chosen adapter).
pub fn gpu_info() -> String {
    context()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .gpu_info
        .clone()
}

/// Records the active level name. Call from the game when a level loads.
pub fn set_active_level(level: impl Into<String>) {
    *active_level().lock().unwrap_or_else(PoisonError::into_inner) = level.into();
}

/// Records the current frame count. Called once per frame by the platform plugin.
pub fn set_frame_count(frame: u64) {
    FRAME_COUNT.store(frame, Ordering::Relaxed);
}

/// `PostUpdate`: records the current frame count for crash reports.
pub(crate) fn update_frame_count(time: Res<Time>) {
    set_frame_count(time.frame_count());
}

/// The `logs://` directory crash reports are written to (`None` when not configured).
pub fn crash_dir() -> Option<PathBuf> {
    context()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .logs_dir
        .clone()
}

/// Installs the crash-report panic hook, chaining to any previously installed hook. Idempotent.
pub fn install_panic_hook() {
    if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_crash_report(info);
        previous(info);
    }));
}

/// Writes a crash report for `info` to `logs://crash-<timestamp>.txt`, best-effort. Returns the
/// written path. Never panics; on failure (no directory configured, IO error) returns `None`.
pub fn write_crash_report(info: &std::panic::PanicHookInfo<'_>) -> Option<PathBuf> {
    let message = payload_message(info.payload());
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown>".to_owned());
    let backtrace = std::backtrace::Backtrace::force_capture();

    let (logs_dir, build, os, gpu, frame, level) = snapshot();
    let logs_dir = logs_dir?;

    let recent = regia_core::log::ring::recent(&regia_core::log::ring::LogQuery {
        limit: Some(CRASH_LOG_LINES),
        ..Default::default()
    })
    .0;

    let backtrace_text = backtrace.to_string();
    let sections = sections_text();
    let text = format_report(&ReportParts {
        unix_ms: unix_ms_now(),
        headline: "panic",
        message,
        location: &location,
        backtrace: &backtrace_text,
        build: &build,
        os: &os,
        gpu: &gpu,
        frame,
        active_level: &level,
        recent: &recent,
        sections: &sections,
    });
    write_to_dir(&logs_dir, &text)
}

/// Writes a crash report for a lost GPU device or window surface (no panic) to
/// `logs://crash-<timestamp>.txt`, so the next launch's reporter (`regia_crash`) uploads it like a
/// panic report (ADR-2307). `kind` is `device_lost` or `surface_unavailable` and heads the report
/// (`<kind>: <reason>: <message>` where a panic report has `panic: ...`); the registered sections
/// ([`add_report_section`]: the renderer's `gpu breadcrumbs:` block names the passes the GPU was
/// most likely inside) follow the runtime block. `exit_code` is the code the process exits with.
/// Best-effort and panic-free; `None` when no directory is configured or the write fails.
pub fn write_device_lost_report(kind: &str, reason: &str, message: &str, exit_code: u8) -> Option<PathBuf> {
    let (logs_dir, build, os, gpu, frame, level) = snapshot();
    let logs_dir = logs_dir?;
    let recent = regia_core::log::ring::recent(&regia_core::log::ring::LogQuery {
        limit: Some(CRASH_LOG_LINES),
        ..Default::default()
    })
    .0;
    let headline_message = format!("{reason}: {message}");
    let backtrace =
        format!("no backtrace: {kind}; the game stopped rendering and exits with code {exit_code}");
    let sections = sections_text();
    let text = format_report(&ReportParts {
        unix_ms: unix_ms_now(),
        headline: kind,
        message: &headline_message,
        location: "regia_render::gpu_health (device-lost callback)",
        backtrace: &backtrace,
        build: &build,
        os: &os,
        gpu: &gpu,
        frame,
        active_level: &level,
        recent: &recent,
        sections: &sections,
    });
    write_to_dir(&logs_dir, &text)
}

/// Text of one crash-report section, produced when a report is written. It must be quick,
/// must not block for long and must not panic (it may run inside the panic hook).
pub type ReportSection = fn() -> String;

static SECTIONS: Mutex<Vec<(&'static str, ReportSection)>> = Mutex::new(Vec::new());

/// Registers `section` under `name` (replacing a section of the same name). Every crash report
/// written afterwards (panic or device loss) carries its text, when not empty, after the runtime
/// block. The renderer registers `gpu_breadcrumbs` (ADR-2307).
pub fn add_report_section(name: &'static str, section: ReportSection) {
    let mut sections = SECTIONS.lock().unwrap_or_else(PoisonError::into_inner);
    sections.retain(|(n, _)| *n != name);
    sections.push((name, section));
}

/// The registered section names, in registration order.
#[must_use]
pub fn report_sections() -> Vec<&'static str> {
    SECTIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(n, _)| *n)
        .collect()
}

/// The registered sections' text. The list is read with `try_lock` only (a panic while this
/// thread registers a section must not deadlock the hook).
fn sections_text() -> String {
    let list: Vec<ReportSection> = match SECTIONS.try_lock() {
        Ok(s) => s.iter().map(|(_, f)| *f).collect(),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().iter().map(|(_, f)| *f).collect(),
        Err(std::sync::TryLockError::WouldBlock) => return String::new(),
    };
    let mut out = String::new();
    for section in list {
        let text = section();
        if !text.trim().is_empty() {
            out.push_str(text.trim_end());
            out.push('\n');
        }
    }
    out
}

/// Reads the globals needed for a report (cheap copies; never blocks on anything but the two small
/// mutexes, and recovers from poisoning).
fn snapshot() -> (Option<PathBuf>, BuildInfo, OsInfo, String, u64, String) {
    let ctx = context().lock().unwrap_or_else(PoisonError::into_inner);
    let logs_dir = ctx.logs_dir.clone();
    let build = ctx.build_info.clone();
    let os = ctx.os_info.clone();
    let gpu = ctx.gpu_info.clone();
    drop(ctx);
    let frame = FRAME_COUNT.load(Ordering::Relaxed);
    let level = active_level()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    (logs_dir, build, os, gpu, frame, level)
}

/// The panic payload as text, handling the common `&str` / `String` cases.
fn payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(non-string panic payload)")
}

/// The inputs of a report, split out so the formatting is unit-testable.
struct ReportParts<'a> {
    unix_ms: u64,
    /// `panic` for the panic hook; `device_lost` or `surface_unavailable` for
    /// [`write_device_lost_report`].
    headline: &'a str,
    message: &'a str,
    location: &'a str,
    backtrace: &'a str,
    build: &'a BuildInfo,
    os: &'a OsInfo,
    gpu: &'a str,
    frame: u64,
    active_level: &'a str,
    recent: &'a [regia_core::log::ring::LogRecord],
    /// The registered sections' text ([`add_report_section`]), already formatted.
    sections: &'a str,
}

/// What a crash report keeps of one field: the text through `regia_core::privacy::scrub_report`
/// (home folder to `~`, secrets to `[redacted]`, email addresses to `[email]`). Every field of a
/// report goes through this one function, so a panic message, a source path in the location or a
/// backtrace frame, a GPU string or a registered section can no more carry a user name or a token
/// than a log line can (threat model finding 16, ADR-3285).
pub fn scrub_field(text: &str) -> String {
    regia_core::privacy::scrub_report(text)
}

/// Renders the report body, every field through [`scrub_field`]. Never panics.
fn format_report(parts: &ReportParts<'_>) -> String {
    format_report_with(parts, &scrub_field)
}

/// [`format_report`] with the field scrubber injected (tests plant a home folder of their own).
fn format_report_with(parts: &ReportParts<'_>, scrub: &dyn Fn(&str) -> String) -> String {
    let message = scrub(parts.message);
    let location = scrub(parts.location);
    let version = scrub(&parts.build.version);
    let git_hash = scrub(&parts.build.git_hash);
    let profile = scrub(&parts.build.profile);
    let engine = scrub(&parts.build.engine);
    let os = scrub(&parts.os.os);
    let os_version = scrub(&parts.os.version);
    let device_model = scrub(&parts.os.device_model);
    let gpu = scrub(parts.gpu);
    let active_level = scrub(parts.active_level);
    let sections = scrub(parts.sections);
    let backtrace = scrub(parts.backtrace);
    let headline = scrub(parts.headline);
    let mut text = String::with_capacity(8192);
    let _ = writeln!(text, "Regia Engine crash report");
    let _ = writeln!(text, "====================");
    let _ = writeln!(text, "unix_ms: {}", parts.unix_ms);
    let _ = writeln!(text);
    let _ = writeln!(text, "{headline}: {message}");
    let _ = writeln!(text, "location: {location}");
    let _ = writeln!(text);
    let _ = writeln!(text, "build:");
    let _ = writeln!(text, "  version: {version}");
    let _ = writeln!(text, "  git: {git_hash}");
    let _ = writeln!(text, "  profile: {profile}");
    let _ = writeln!(text, "  engine: {engine}");
    let _ = writeln!(text);
    let _ = writeln!(text, "system:");
    let _ = writeln!(text, "  os: {os} ({os_version})");
    let _ = writeln!(text, "  device: {device_model}");
    let _ = writeln!(text, "  ram_bytes: {}", parts.os.total_ram_bytes);
    let _ = writeln!(text, "  cpus: {}", parts.os.cpu_count);
    let _ = writeln!(
        text,
        "  gpu: {}",
        if gpu.is_empty() {
            "(not reported)"
        } else {
            gpu.as_str()
        }
    );
    let _ = writeln!(text);
    let _ = writeln!(text, "runtime:");
    let _ = writeln!(text, "  frame: {}", parts.frame);
    let _ = writeln!(
        text,
        "  active_level: {}",
        if active_level.is_empty() {
            "(none)"
        } else {
            active_level.as_str()
        }
    );
    let _ = writeln!(text);
    if !sections.is_empty() {
        let _ = writeln!(text, "{}", sections.trim_end());
        let _ = writeln!(text);
    }
    let _ = writeln!(text, "backtrace:\n{backtrace}");
    let _ = writeln!(text);
    let _ = writeln!(text, "recent logs (last {}):", CRASH_LOG_LINES);
    if parts.recent.is_empty() {
        let _ = writeln!(text, "  (log ring empty — logging was not initialized)");
    }
    for record in parts.recent {
        let _ = writeln!(
            text,
            "  {} {} @{}ms {}",
            scrub(&record.level),
            scrub(&record.target),
            record.unix_ms,
            scrub(&record.message)
        );
    }
    text
}

/// Writes `text` to `dir/crash-<timestamp>.txt`, uniquifying the name on collision. Never panics.
fn write_to_dir(dir: &Path, text: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_date_time(secs);
    let stem = format!("{CRASH_PREFIX}{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}");
    let mut path = dir.join(format!("{stem}{CRASH_SUFFIX}"));
    let mut n = 1u32;
    // `create_new` claims the name atomically, so two panics in the same second cannot both win it.
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                if file.write_all(text.as_bytes()).is_err() {
                    // A partial report (a full disk mid-backtrace) would be listed and uploaded
                    // as a whole one next launch: remove it (audit 11-03 crash L4, ADR-2396).
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
                prune_dir(dir, &path, MAX_CRASH_FILES, MAX_CRASH_BYTES);
                return Some(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                path = dir.join(format!("{stem}_{n}{CRASH_SUFFIX}"));
                n = n.checked_add(1)?;
            }
            Err(_) => return None,
        }
    }
}

/// Most crash-report files (pending and `.rejected`) kept in the crash directory.
const MAX_CRASH_FILES: usize = 32;
/// Most total bytes of crash-report files kept in the crash directory.
const MAX_CRASH_BYTES: u64 = 64 * 1024 * 1024;

/// Deletes the oldest crash-report files (`crash-*.txt` and quarantined `crash-*.txt.rejected`)
/// until at most `max_files` files and `max_bytes` bytes remain. `keep` (the report just written)
/// is never deleted. Never panics.
fn prune_dir(dir: &Path, keep: &Path, max_files: usize, max_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(String, PathBuf, u64)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            let stem = name.strip_suffix(".rejected").unwrap_or(&name);
            if !is_crash_file_name(stem) {
                return None;
            }
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (name, e.path(), meta.len()))
        })
        .collect();
    // The timestamped names sort oldest first.
    files.sort_by_cached_key(|f| order_key(&f.0));
    let mut count = files.len();
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    for (_, path, len) in &files {
        if count <= max_files && total <= max_bytes {
            break;
        }
        if path == keep {
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            count -= 1;
            total = total.saturating_sub(*len);
        }
    }
}

/// Lists pending crash reports in the crash directory, oldest first.
pub fn pending() -> Vec<CrashFile> {
    let Some(dir) = crash_dir() else {
        return Vec::new();
    };
    list_dir(&dir)
}

/// Reads one crash report by its file name (see [`CrashFile::name`]).
pub fn read(name: &str) -> Option<String> {
    if !valid_name(name) {
        return None;
    }
    read_from(&crash_dir()?, name)
}

/// Deletes one crash report by its file name.
pub fn remove(name: &str) -> std::io::Result<()> {
    if !valid_name(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a crash-report file name",
        ));
    }
    let Some(dir) = crash_dir() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "crash directory not configured",
        ));
    };
    remove_from(&dir, name)
}

fn list_dir(dir: &Path) -> Vec<CrashFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<CrashFile> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(valid_name))
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            let meta = std::fs::metadata(&path).ok()?;
            Some(CrashFile {
                name,
                modified_unix_ms: meta.modified().ok().and_then(modified_unix_ms).unwrap_or(0),
                bytes: meta.len(),
                path,
            })
        })
        .collect();
    files.sort_by_cached_key(|f| order_key(&f.name));
    files
}

/// Oldest-first sort key of a crash file name: the timestamp stem, then the numeric collision suffix
/// (`_2` before `_10`; the unsuffixed name first).
fn order_key(name: &str) -> (String, u32) {
    let base = name.strip_suffix(CRASH_SUFFIX).unwrap_or(name);
    match base.rsplit_once('_') {
        Some((stem, n)) if stem.contains('_') => match n.parse::<u32>() {
            Ok(n) => (stem.to_owned(), n),
            Err(_) => (base.to_owned(), 0),
        },
        _ => (base.to_owned(), 0),
    }
}

fn read_from(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name)).ok()
}

fn remove_from(dir: &Path, name: &str) -> std::io::Result<()> {
    std::fs::remove_file(dir.join(name))
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn modified_unix_ms(modified: SystemTime) -> Option<u64> {
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Saves a nonfatal hang report with already captured thread stacks.
/// The whole report passes through the standard privacy scrubber and appears in
/// [`pending`], so `diagnostics.crash_reports` can list/read it. No capture runs in
/// the panic hook. `None` means no configured logs directory or a write failure.
pub fn write_hang_report(reason: &str, stacks: &str) -> Option<PathBuf> {
    let (logs_dir, build, os, gpu, frame, level) = snapshot();
    let logs_dir = logs_dir?;
    let recent = regia_core::log::ring::recent(&regia_core::log::ring::LogQuery {
        limit: Some(CRASH_LOG_LINES),
        ..Default::default()
    })
    .0;
    let text = format_report(&ReportParts {
        unix_ms: unix_ms_now(),
        headline: "hang",
        message: reason,
        location: "watchdog thread snapshot",
        backtrace: stacks,
        build: &build,
        os: &os,
        gpu: &gpu,
        frame,
        active_level: &level,
        recent: &recent,
        sections: "",
    });
    write_hang_to_dir(&logs_dir, &text)
}

/// Publish a complete hang report atomically without replacing an existing name.
/// A hard link from a hidden, fully written staging file is an atomic no-replace
/// publication on the same filesystem. Readers never see a partial report.
fn write_hang_to_dir(dir: &Path, text: &str) -> Option<PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir).ok()?;
    let mut staging;
    let mut file = loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        staging = dir.join(format!(".hang-{}-{n}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
        {
            Ok(file) => break file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    };
    let written = file.write_all(text.as_bytes()).is_ok();
    drop(file);
    let result = if written {
        let secs = unix_ms_now() / 1000;
        let (y, mo, d, h, mi, s) = civil_date_time(secs);
        let stem = format!("{CRASH_PREFIX}{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}");
        let mut suffix = 0u64;
        loop {
            let name = if suffix == 0 {
                format!("{stem}{CRASH_SUFFIX}")
            } else {
                format!("{stem}_{suffix}{CRASH_SUFFIX}")
            };
            let path = dir.join(name);
            match std::fs::hard_link(&staging, &path) {
                Ok(()) => break Some(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let Some(next) = suffix.checked_add(1) else {
                        break None;
                    };
                    suffix = next;
                }
                Err(_) => break None,
            }
        }
    } else {
        None
    };
    let _ = std::fs::remove_file(&staging);
    if let Some(path) = &result {
        prune_dir(dir, path, MAX_CRASH_FILES, MAX_CRASH_BYTES);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::log::ring::LogRecord;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("regia-crash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn build_info_detects_without_env() {
        let info = BuildInfo::detect();
        assert!(!info.version.is_empty());
        assert!(!info.engine.is_empty());
        assert!(["dev", "shipping", "release"].contains(&info.profile.as_str()));
        assert_eq!(info.git_hash, "unknown");
    }

    #[test]
    fn collision_suffixes_sort_numerically() {
        let mut names = [
            "crash-2026-10-06_12-34-56_10.txt",
            "crash-2026-10-06_12-34-56_2.txt",
            "crash-2026-10-06_12-34-56.txt",
            "crash-2026-10-06_12-34-57.txt",
        ];
        names.sort_by_cached_key(|n| order_key(n));
        assert_eq!(
            names,
            [
                "crash-2026-10-06_12-34-56.txt",
                "crash-2026-10-06_12-34-56_2.txt",
                "crash-2026-10-06_12-34-56_10.txt",
                "crash-2026-10-06_12-34-57.txt",
            ]
        );
    }

    #[test]
    fn crash_file_name_predicate() {
        assert!(is_crash_file_name("crash-2026-10-06_12-34-56.txt"));
        assert!(is_crash_file_name("crash-2026-10-06_12-34-56_1.txt"));
        assert!(!is_crash_file_name("2026-10-06_12-34-56.log"));
        assert!(!is_crash_file_name("crash-2026-10-06_12-34-56")); // no suffix
        assert!(!is_crash_file_name("session.log"));
        // Path traversal is never a valid name for read/remove.
        assert!(!valid_name("../crash-x.txt"));
        assert!(!valid_name("crash-x.txt/.."));
        assert!(!valid_name("a/crash-x.txt"));
        assert!(!valid_name("crash-..\\x.txt"));
        assert!(!valid_name("crash-2026-10-06_12-34-56.txt/"));
        assert!(valid_name("crash-2026-10-06_12-34-56.txt"));
    }

    #[test]
    fn report_body_has_every_section() {
        let build = BuildInfo {
            version: "1.2.3".into(),
            git_hash: "abc1234".into(),
            profile: "shipping".into(),
            engine: "0.1.0".into(),
        };
        let os = OsInfo {
            os: "linux".into(),
            version: "Arch Linux".into(),
            device_model: "box".into(),
            total_ram_bytes: 1024,
            cpu_count: 8,
            cpu_model: "test cpu".into(),
        };
        let recent = vec![LogRecord {
            seq: 0,
            unix_ms: 42,
            level: "ERROR".into(),
            target: "test".into(),
            message: "something broke".into(),
        }];
        let text = format_report(&ReportParts {
            headline: "panic",
            sections: "",
            unix_ms: 1,
            message: "panicked at boom",
            location: "src/lib.rs:1:1",
            backtrace: " 0: frame",
            build: &build,
            os: &os,
            gpu: "NVIDIA RTX 5080",
            frame: 99,
            active_level: "levels/main",
            recent: &recent,
        });
        for needle in [
            "Regia Engine crash report",
            "panic: panicked at boom",
            "location: src/lib.rs:1:1",
            "version: 1.2.3",
            "git: abc1234",
            "profile: shipping",
            "os: linux (Arch Linux)",
            "gpu: NVIDIA RTX 5080",
            "frame: 99",
            "active_level: levels/main",
            "backtrace:",
            "recent logs (last 200):",
            "ERROR test @42ms something broke",
        ] {
            assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
        }
    }

    /// Threat model finding 16 (ADR-3285): only the log lines went through the scrubber; the panic
    /// message, the location, the backtrace and every other field reached the file (and the
    /// uploader) as they were. A home folder, a token and an email are planted in every field.
    #[test]
    fn every_report_field_is_scrubbed_of_a_home_a_token_and_an_email() {
        let homes = regia_core::privacy::home_spellings("/home/ann");
        let scrub = |text: &str| regia_core::privacy::scrub_report_with(text, &homes);
        let plant = |field: &str| format!("{field} /home/ann/x token=tok-{field} ann.{field}@example.com");
        let build = BuildInfo {
            version: plant("version"),
            git_hash: plant("git"),
            profile: plant("profile"),
            engine: plant("engine"),
        };
        let os = OsInfo {
            os: plant("os"),
            version: plant("osversion"),
            device_model: plant("device"),
            total_ram_bytes: 1,
            cpu_count: 1,
            cpu_model: plant("cpu"),
        };
        let recent = vec![LogRecord {
            seq: 0,
            unix_ms: 1,
            level: plant("level"),
            target: plant("target"),
            message: plant("logline"),
        }];
        let (headline, message, location, backtrace, gpu, level, sections) = (
            plant("headline"),
            plant("message"),
            format!("/home/ann/src/main.rs:1:1 {}", plant("location")),
            format!(
                "  0: game::main\n      at /home/ann/.cargo/registry/x.rs:9\n{}",
                plant("backtrace")
            ),
            plant("gpu"),
            plant("activelevel"),
            plant("section"),
        );
        let text = format_report_with(
            &ReportParts {
                unix_ms: 1,
                headline: &headline,
                message: &message,
                location: &location,
                backtrace: &backtrace,
                build: &build,
                os: &os,
                gpu: &gpu,
                frame: 1,
                active_level: &level,
                recent: &recent,
                sections: &sections,
            },
            &scrub,
        );
        assert!(!text.contains("/home/ann"), "{text}");
        assert!(!text.contains("tok-"), "{text}");
        assert!(!text.contains("@example.com"), "{text}");
        // Every field is still there, scrubbed.
        assert!(text.contains("location: ~/src/main.rs:1:1"), "{text}");
        assert!(text.contains("at ~/.cargo/registry/x.rs:9"), "{text}");
        assert!(text.contains("gpu: gpu ~/x token=[redacted] [email]"), "{text}");
    }

    #[test]
    fn gpu_info_round_trips_and_is_not_a_placeholder() {
        assert_eq!(gpu_info(), "", "empty until the renderer reports the adapter");

        set_gpu_info("NVIDIA RTX 5080 (Vulkan, DiscreteGpu, driver 555.0 555.42.06)");
        assert_eq!(
            gpu_info(),
            "NVIDIA RTX 5080 (Vulkan, DiscreteGpu, driver 555.0 555.42.06)"
        );

        // A recorded GPU string renders as a real `gpu:` line, never the `(not reported)` placeholder.
        let build = BuildInfo::detect();
        let os = OsInfo::unknown();
        let text = format_report(&ReportParts {
            headline: "panic",
            sections: "",
            unix_ms: 1,
            message: "boom",
            location: "src/lib.rs:1:1",
            backtrace: " 0: frame",
            build: &build,
            os: &os,
            gpu: &gpu_info(),
            frame: 0,
            active_level: "",
            recent: &[],
        });
        assert!(text.contains("gpu: NVIDIA RTX 5080"), "{text}");
        assert!(!text.contains("gpu: (not reported)"), "{text}");

        // Reset so later tests in this process still see an unset GPU.
        set_gpu_info("");
    }

    #[test]
    fn list_read_remove_round_trip() {
        let dir = tmp("roundtrip");
        // Two crash files and one unrelated file.
        std::fs::write(dir.join("crash-2026-10-06_00-00-01.txt"), "one").expect("write");
        std::fs::write(dir.join("crash-2026-10-06_00-00-02.txt"), "two").expect("write");
        std::fs::write(dir.join("session.log"), "nope").expect("write");

        let files = list_dir(&dir);
        assert_eq!(files.len(), 2, "{files:?}");
        assert_eq!(files[0].name, "crash-2026-10-06_00-00-01.txt");
        assert_eq!(files[0].bytes, 3);
        assert_eq!(files[1].name, "crash-2026-10-06_00-00-02.txt");

        // The public read/remove gate on `valid_name`, which only accepts a bare crash-report name.
        assert!(valid_name("crash-2026-10-06_00-00-01.txt"));
        assert!(!valid_name("session.log"));
        assert!(!valid_name("../etc/passwd"));

        assert_eq!(
            read_from(&dir, "crash-2026-10-06_00-00-01.txt").as_deref(),
            Some("one")
        );
        remove_from(&dir, "crash-2026-10-06_00-00-01.txt").expect("remove");
        assert_eq!(list_dir(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_dir_hides_names_remove_would_refuse() {
        let dir = tmp("unremovable");
        std::fs::write(dir.join("crash-ok.txt"), "x").expect("write");
        // `:` is a legal file name on Unix but `remove` rejects it, so it must not be listed.
        #[cfg(unix)]
        std::fs::write(dir.join("crash-12:34:56.txt"), "x").expect("write");
        let names: Vec<String> = list_dir(&dir).into_iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["crash-ok.txt".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_dir_drops_the_oldest_by_count_and_bytes() {
        let dir = tmp("prune");
        for i in 1..=5 {
            std::fs::write(dir.join(format!("crash-2026-10-06_00-00-0{i}.txt")), "xxxx").expect("seed");
        }
        std::fs::write(dir.join("crash-2026-10-06_00-00-00.txt.rejected"), "xxxx").expect("seed");
        std::fs::write(dir.join("notes.txt"), "keep me").expect("seed");
        let newest = dir.join("crash-2026-10-06_00-00-05.txt");

        // Count cap: 6 crash files -> 3 remain; the oldest (the .rejected one) go first.
        prune_dir(&dir, &newest, 3, u64::MAX);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "crash-2026-10-06_00-00-03.txt",
                "crash-2026-10-06_00-00-04.txt",
                "crash-2026-10-06_00-00-05.txt",
                "notes.txt"
            ]
        );

        // Byte cap: 4 bytes each; a 5-byte cap leaves only the protected newest file.
        prune_dir(&dir, &newest, usize::MAX, 5);
        assert!(newest.exists());
        assert!(!dir.join("crash-2026-10-06_00-00-03.txt").exists());
        assert!(dir.join("notes.txt").exists(), "non-crash files are never pruned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_dir_orders_collision_suffixes_numerically() {
        let dir = tmp("prune_numeric");
        let stem = "crash-2026-10-06_00-00-00";
        for suffix in ["", "_2", "_10"] {
            std::fs::write(dir.join(format!("{stem}{suffix}.txt")), "x").expect("seed");
        }
        let newest = dir.join(format!("{stem}_10.txt"));
        prune_dir(&dir, &newest, 2, u64::MAX);
        assert!(!dir.join(format!("{stem}.txt")).exists(), "the oldest goes first");
        assert!(
            dir.join(format!("{stem}_2.txt")).exists(),
            "_2 is older than _10 and survives"
        );
        assert!(newest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_to_dir_uniquifies_names() {
        let dir = tmp("uniquify");
        let first = write_to_dir(&dir, "a").expect("first");
        let second = write_to_dir(&dir, "b").expect("second");
        assert_ne!(first, second, "two writes in the same second do not collide");
        assert_eq!(list_dir(&dir).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_to_dir_never_overwrites_an_existing_report() {
        let dir = tmp("no-overwrite");
        let paths: Vec<_> = (0..5)
            .map(|i| write_to_dir(&dir, &format!("report {i}")).expect("write"))
            .collect();
        for (i, path) in paths.iter().enumerate() {
            assert_eq!(
                std::fs::read_to_string(path).expect("read"),
                format!("report {i}")
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod hang_tests {
    use super::*;
    #[test]
    fn concurrent_hang_writers_publish_complete_distinct_reports() {
        let dir = regia_test_utils::TempDir::new("hang-publish");
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|n| {
                    let path = dir.path();
                    scope.spawn(move || {
                        let text = format!("{n}\n{}", "frame\n".repeat(4096));
                        let report = write_hang_to_dir(path, &text).expect("test fixture");
                        assert_eq!(std::fs::read_to_string(&report).expect("test fixture"), text);
                        report
                    })
                })
                .collect();
            let mut paths: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().expect("test fixture"))
                .collect();
            paths.sort();
            paths.dedup();
            assert_eq!(paths.len(), 8);
        });
        assert_eq!(std::fs::read_dir(dir.path()).expect("test fixture").count(), 8);
        assert_eq!(list_dir(dir.path()).len(), 8);
    }
}
