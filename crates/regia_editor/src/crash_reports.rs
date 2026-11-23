// SPDX-License-Identifier: MPL-2.0
//! Editor crash reports (ADR-3931).
//!
//! The editor installs the same panic hook games use (`regia_platform::crash::install_panic_hook`)
//! before the window opens, aimed at the editor's own log folder. An editor-owned hook then runs
//! after that writer: it scrubs home directories out of the whole report
//! (`regia_core::privacy::scrub_home`), caps the file, and retitles a play-in-editor panic from
//! `panic:` to `pie:`. The rest of the text is the game report, so `regia crash list` / `show`
//! (the `parse_report` reader) accepts both. A `pie` headline is that reader's "other" kind, the
//! same slot `device_lost` already uses; a `panic` headline stays a panic.
//!
//! Native crashes (a segfault, a driver fault) are the game's feedback monitor: Linux on, Windows
//! opt-in, `REGIA_NO_NATIVE_CRASH=1` off, `REGIA_NATIVE_CRASH=1` forces Windows on. Those land as
//! `feedback-*.zip` bundles whose `crash.txt` starts with `native crash:`, not as `crash-*.txt`.
//!
//! The next launch's recovery dialog offers the newest report. Agents use `editor.crash_reports`
//! and `editor.crash_report_read` (`book/agents/editor-crash-reports.md`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::{Mutex, PoisonError};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::World;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// Most bytes stored in one editor crash report, counting the truncation note.
pub const MAX_REPORT_BYTES: usize = 256 * 1024;

/// How many bytes of a report `editor.crash_report_read` and View report return.
const READ_CAP: u64 = 1024 * 1024;

const TRUNCATED: &str = "\n[truncated]\n";

static REPORT_BYTES: AtomicUsize = AtomicUsize::new(MAX_REPORT_BYTES);
static REWRITE_INSTALLED: AtomicBool = AtomicBool::new(false);
static PART_SEQ: AtomicU64 = AtomicU64::new(0);
/// How many `with_pie_kind` frames are on the stack. The panic hook runs on the schedule
/// worker that executed the game system, not on the thread that called `catch_unwind`, so a
/// thread-local flag would file the report as `panic`.
static PIE_DEPTH: AtomicU32 = AtomicU32::new(0);

/// Test builds only: every `with_pie_kind` frame holds this shared, and a test that files an
/// editor panic holds it exclusively. `PIE_DEPTH` is process-wide on purpose (see above), so
/// without the gate a PIE test on another libtest thread turned a parallel test's `panic`
/// report into `pie`.
#[cfg(test)]
static PIE_KIND_GATE: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// `panic`, `native` or `pie`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// An editor panic outside play-in-editor.
    Panic,
    /// A native fault recorded as a feedback bundle (`native crash:`).
    Native,
    /// A panic caught in game code during play-in-editor.
    Pie,
}

impl ReportKind {
    /// The agent and dialog token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Panic => "panic",
            Self::Native => "native",
            Self::Pie => "pie",
        }
    }
}

/// One report the next launch or an agent can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedReport {
    /// Milliseconds since the Unix epoch. The report's `unix_ms:` line when it has one, otherwise
    /// the file's modification time (0 when that is unknown).
    pub time_unix_ms: u64,
    /// `panic`, `native` or `pie`.
    pub kind: ReportKind,
    /// The headline line (`panic: …`, `pie: …` or `native crash: …`), not the title banner.
    pub first_line: String,
    /// Full path of the `crash-*.txt` or `feedback-*.zip`.
    pub path: PathBuf,
}

/// The editor log folder: `<user data>/RegiaEngine/<exe>/logs`, or a temp folder when the
/// platform has no user directory.
#[must_use]
pub fn default_log_dir() -> PathBuf {
    user_data_base().join("logs")
}

fn user_data_base() -> PathBuf {
    #[cfg(not(target_os = "android"))]
    {
        regia_platform::paths::user_dir().unwrap_or_else(|| std::env::temp_dir().join("regia-editor"))
    }
    #[cfg(target_os = "android")]
    {
        std::env::temp_dir().join("regia-editor")
    }
}

/// Build identity recorded in editor reports. Same fields games store; the version strings are
/// this crate's, which the workspace keeps equal to the engine version.
fn editor_build_info() -> regia_platform::crash::BuildInfo {
    regia_platform::crash::BuildInfo {
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

/// Points crash reports at `logs_dir` and installs the game panic hook, then the editor rewrite
/// hook. Idempotent. Call after logging is initialized and before the window opens. Best-effort:
/// a directory that cannot be created still leaves the hook installed, and the writer skips a
/// missing folder.
pub fn install(logs_dir: impl Into<PathBuf>) {
    let logs_dir = logs_dir.into();
    let _ = std::fs::create_dir_all(&logs_dir);
    regia_platform::crash::configure(
        Some(logs_dir),
        editor_build_info(),
        regia_platform::OsInfo::detect(),
    );
    // The platform's own take + set runs under the editor's panic-hook lock, so it cannot
    // interleave with another editor install (ADR-8847).
    crate::panic_trace::with_hook_lock(regia_platform::crash::install_panic_hook);
    install_rewrite_hook();
}

fn install_rewrite_hook() {
    if REWRITE_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::panic_trace::chain_hook(|previous| {
        Box::new(move |info| {
            let before = regia_platform::crash::crash_dir().map(|dir| crash_names(&dir));
            previous(info);
            if let Some(dir) = regia_platform::crash::crash_dir() {
                rewrite_new(&dir, before.as_ref());
            }
        })
    });
}

/// Runs `body` so a panic inside it is filed as `pie` instead of `panic`. The flag is restored
/// when `body` returns, including when a panic unwinds out of it. It is process-wide: the
/// schedule may run the panicking system on another thread.
pub(crate) fn with_pie_kind<R>(body: impl FnOnce() -> R) -> R {
    // Declared before the depth guard, so the depth is back down before the gate opens.
    #[cfg(test)]
    let _gate = PIE_KIND_GATE.read().unwrap_or_else(PoisonError::into_inner);
    PIE_DEPTH.fetch_add(1, Ordering::SeqCst);
    let _guard = PieDepthGuard;
    body()
}

struct PieDepthGuard;

impl Drop for PieDepthGuard {
    fn drop(&mut self) {
        PIE_DEPTH.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: a panic on this thread is an editor panic whatever PIE frames other tests run
    /// meanwhile (`PIE_DEPTH` is process-wide and the test binary runs PIE tests in parallel; the
    /// several-client PIE worlds go through `with_pie_kind` since ADR-7340). Set by the crash
    /// tests' `catch_panic` around their own deliberate panics only.
    static EDITOR_PANIC_HERE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn current_kind() -> ReportKind {
    #[cfg(test)]
    if EDITOR_PANIC_HERE.with(std::cell::Cell::get) {
        return ReportKind::Panic;
    }
    if PIE_DEPTH.load(Ordering::SeqCst) > 0 {
        ReportKind::Pie
    } else {
        ReportKind::Panic
    }
}

/// Feedback settings for the editor: the game defaults (native capture on, Windows opt-in off)
/// and the crash-report directory as the bundle folder.
#[must_use]
pub fn editor_feedback_config() -> regia_framework::feedback::FeedbackConfig {
    regia_framework::feedback::FeedbackConfig {
        dir: regia_platform::crash::crash_dir(),
        ..Default::default()
    }
}

fn crash_names(dir: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return names;
    };
    for entry in read.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if regia_platform::crash::is_crash_file_name(name) {
            names.insert(name.to_owned());
        }
    }
    names
}

fn rewrite_new(dir: &Path, before: Option<&HashSet<String>>) {
    let pie = current_kind() == ReportKind::Pie;
    let limit = report_byte_limit();
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let fresh: Vec<(String, PathBuf)> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            if !regia_platform::crash::is_crash_file_name(&name) {
                return None;
            }
            if before.is_some_and(|known| known.contains(&name)) {
                return None;
            }
            Some((name, entry.path()))
        })
        .collect();
    for (name, path) in fresh {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let finished = finish_report(&text, pie, limit);
        atomic_replace(&path, &name, finished.as_bytes());
    }
}

fn report_byte_limit() -> usize {
    let limit = REPORT_BYTES.load(Ordering::Relaxed);
    if limit == 0 { MAX_REPORT_BYTES } else { limit }
}

#[cfg(test)]
fn set_report_byte_limit(limit: usize) {
    REPORT_BYTES.store(limit, Ordering::Relaxed);
}

/// Scrubs home paths, optionally retitles `panic:` to `pie:`, then caps the bytes.
fn finish_report(text: &str, pie: bool, limit: usize) -> String {
    let titled = if pie { retitle_pie(text) } else { text.to_owned() };
    let scrubbed = regia_core::privacy::scrub_home(&titled);
    cap_text(&scrubbed, limit)
}

fn retitle_pie(text: &str) -> String {
    let mut done = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if !done && let Some(rest) = line.strip_prefix("panic: ") {
            out.push_str("pie: ");
            out.push_str(rest);
            done = true;
        } else {
            out.push_str(line);
        }
    }
    out
}

fn cap_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    if limit <= TRUNCATED.len() {
        return TRUNCATED.chars().take(limit).collect();
    }
    let mut end = limit - TRUNCATED.len();
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + TRUNCATED.len());
    out.push_str(&text[..end]);
    out.push_str(TRUNCATED);
    out
}

fn atomic_replace(path: &Path, name: &str, bytes: &[u8]) {
    let n = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    let Some(parent) = path.parent() else {
        return;
    };
    let tmp = parent.join(format!("{name}.{n}.part"));
    if std::fs::write(&tmp, bytes).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Pending text reports and native bundles in the configured directory, oldest first.
#[must_use]
pub fn list() -> Vec<ListedReport> {
    list_result().unwrap_or_default()
}

fn list_result() -> Result<Vec<ListedReport>, AccessError> {
    let Some(dir) = regia_platform::crash::crash_dir() else {
        return Err(AccessError::Unavailable);
    };
    let mut reports = Vec::new();
    if let Ok(read) = std::fs::read_dir(&dir) {
        let mut files: Vec<(String, PathBuf, u64)> = read
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                if !regia_platform::crash::is_crash_file_name(&name) {
                    return None;
                }
                if !entry.file_type().ok()?.is_file() {
                    return None;
                }
                let meta = entry.metadata().ok()?;
                Some((name, entry.path(), mtime_ms(&meta)))
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        for (_name, path, mtime) in files {
            let Some(text) = read_prefix(&path, 64 * 1024) else {
                continue;
            };
            let first = headline_line(&text);
            reports.push(ListedReport {
                time_unix_ms: unix_ms_of(&text, mtime),
                kind: kind_of_line(&first),
                first_line: first,
                path,
            });
        }
    }
    for bundle in regia_framework::feedback::list_bundles(&dir) {
        if resolve_report(&bundle.path).is_err() {
            continue;
        }
        let Ok(entries) = regia_framework::feedback::read_bundle(&bundle.path) else {
            continue;
        };
        let Some(crash) = entries.iter().find(|entry| entry.name == "crash.txt") else {
            continue;
        };
        let text = String::from_utf8_lossy(&crash.data);
        let first = headline_line(&text);
        if kind_of_line(&first) != ReportKind::Native {
            continue;
        }
        reports.push(ListedReport {
            time_unix_ms: unix_ms_of(&text, bundle.modified_unix_ms),
            kind: ReportKind::Native,
            first_line: first,
            path: bundle.path,
        });
    }
    reports.sort_by(|a, b| {
        a.time_unix_ms
            .cmp(&b.time_unix_ms)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(reports)
}

fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn read_prefix(path: &Path, max: u64) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some(bytes_to_string(bytes, false))
}

fn bytes_to_string(mut bytes: Vec<u8>, truncated: bool) -> String {
    while std::str::from_utf8(&bytes).is_err() && !bytes.is_empty() {
        bytes.pop();
    }
    let mut text = String::from_utf8(bytes).unwrap_or_default();
    if truncated {
        text.push_str(TRUNCATED);
    }
    text
}

fn headline_line(text: &str) -> String {
    for line in text.lines() {
        if is_headline(line) {
            return line.to_owned();
        }
    }
    text.lines().next().unwrap_or("").to_owned()
}

fn is_headline(line: &str) -> bool {
    line.starts_with("panic: ")
        || line.starts_with("pie: ")
        || line.starts_with("native crash:")
        || line.starts_with("device_lost:")
        || line.starts_with("surface_unavailable:")
}

fn kind_of_line(line: &str) -> ReportKind {
    if line.starts_with("pie:") {
        ReportKind::Pie
    } else if line.starts_with("native crash:") || line.starts_with("native:") {
        ReportKind::Native
    } else {
        ReportKind::Panic
    }
}

fn unix_ms_of(text: &str, fallback: u64) -> u64 {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("unix_ms:")
            && let Ok(value) = rest.trim().parse::<u64>()
        {
            return value;
        }
        if is_headline(line) {
            break;
        }
    }
    fallback
}

/// The text of one report. A bundle yields its `crash.txt`.
///
/// # Errors
/// The path is not a report in the configured directory, or the file cannot be read.
pub fn read_text(path: &Path) -> Result<String, String> {
    match read_resolved(path) {
        Ok(text) => Ok(text),
        Err(AccessError::Unavailable) => Err("crash reports are not configured".to_owned()),
        Err(AccessError::Invalid) => Err("not a crash report in the report directory".to_owned()),
        Err(AccessError::NotFound) => Err("crash report not found".to_owned()),
        Err(AccessError::Io(message)) => Err(message),
    }
}

enum AccessError {
    Unavailable,
    Invalid,
    NotFound,
    Io(String),
}

fn read_resolved(path: &Path) -> Result<String, AccessError> {
    let full = resolve_report(path)?;
    if regia_framework::feedback::bundle::is_bundle_name(file_name(&full).unwrap_or("")) {
        let entries = regia_framework::feedback::read_bundle(&full).map_err(AccessError::Io)?;
        let crash = entries
            .into_iter()
            .find(|entry| entry.name == "crash.txt")
            .ok_or(AccessError::NotFound)?;
        let mut bytes = crash.data;
        let truncated = bytes.len() as u64 > READ_CAP;
        if truncated {
            bytes.truncate(READ_CAP as usize);
        }
        return Ok(bytes_to_string(bytes, truncated));
    }
    read_capped(&full)
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}

fn read_capped(path: &Path) -> Result<String, AccessError> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| AccessError::NotFound)?;
    let mut bytes = Vec::new();
    file.take(READ_CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| AccessError::Io(err.to_string()))?;
    let truncated = bytes.len() as u64 > READ_CAP;
    if truncated {
        bytes.truncate(READ_CAP as usize);
    }
    Ok(bytes_to_string(bytes, truncated))
}

fn resolve_report(raw: &Path) -> Result<PathBuf, AccessError> {
    let text = raw.to_str().ok_or(AccessError::Invalid)?;
    resolve_report_str(text)
}

fn resolve_report_str(raw: &str) -> Result<PathBuf, AccessError> {
    if raw.is_empty() || raw.contains('\0') || raw.contains("..") {
        return Err(AccessError::Invalid);
    }
    let Some(dir) = regia_platform::crash::crash_dir() else {
        return Err(AccessError::Unavailable);
    };
    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else if raw.contains('/') || raw.contains('\\') {
        return Err(AccessError::Invalid);
    } else {
        dir.join(raw)
    };
    let Some(name) = file_name(&joined) else {
        return Err(AccessError::Invalid);
    };
    let named = regia_platform::crash::is_crash_file_name(name)
        || regia_framework::feedback::bundle::is_bundle_name(name);
    if !named || name.contains(':') {
        return Err(AccessError::Invalid);
    }
    if !joined.is_file() {
        return Err(AccessError::NotFound);
    }
    let canon = joined.canonicalize().map_err(|_| AccessError::NotFound)?;
    let dir_canon = dir.canonicalize().map_err(|_| AccessError::Unavailable)?;
    if !canon.starts_with(&dir_canon) {
        return Err(AccessError::Invalid);
    }
    Ok(canon)
}

/// Copies `text` to the host clipboard (the recovery dialog's Copy button).
pub fn copy_report(ctx: &egui::Context, text: String) {
    ctx.copy_text(text);
}

/// Opens the folder that contains `path`. Tests record the folder instead of starting a program.
pub fn open_report_folder(path: &Path) {
    let folder = path
        .parent()
        .map_or_else(|| path.to_path_buf(), Path::to_path_buf);
    #[cfg(test)]
    {
        *OPENED_FOLDER.lock().unwrap_or_else(PoisonError::into_inner) = Some(folder);
    }
    #[cfg(not(test))]
    {
        spawn_file_manager(&folder);
    }
}

#[cfg(not(test))]
fn spawn_file_manager(folder: &Path) {
    let shown = folder.display().to_string();
    #[cfg(windows)]
    let result = std::process::Command::new("explorer").arg(folder).spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(folder).spawn();
    if result.is_err() {
        tracing::warn!(target: "regia_editor", "cannot open a file manager at {shown}");
    }
}

#[cfg(test)]
static OPENED_FOLDER: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
fn take_opened_folder() -> Option<PathBuf> {
    OPENED_FOLDER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

/// What the recovery dialog is offering.
pub struct RecoveryOffer<'a> {
    /// Autosave path, when a level was left unsaved.
    pub autosave: Option<&'a str>,
    /// `for <level>` or `(untitled level)`.
    pub original: Option<&'a str>,
    /// Headline of the newest report.
    pub report_line: Option<&'a str>,
    /// Report the buttons act on.
    pub report_path: Option<&'a Path>,
    /// Text already loaded by View report.
    pub shown_text: Option<&'a str>,
}

/// Buttons the recovery dialog collected this frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryChoice {
    /// Restore the autosave.
    pub restore: bool,
    /// Delete the autosave.
    pub discard: bool,
    /// Load the report into [`RecoveryOffer::shown_text`] on the next frame.
    pub view: bool,
    /// Dismiss the report offer for this session. The report stays on disk.
    pub dismiss: bool,
}

/// The body of the Crash recovery window: the unexpected-close line, the autosave offer when
/// there is one, and View report / Copy to clipboard / Open folder when a report is pending.
pub fn recovery_offer_ui(ui: &mut egui::Ui, offer: &RecoveryOffer<'_>) -> RecoveryChoice {
    let mut choice = RecoveryChoice::default();
    ui.label("The editor closed unexpectedly.");
    if let Some(autosave) = offer.autosave {
        ui.label("A previous editor session left an unsaved level behind:");
        ui.weak(autosave);
        if let Some(original) = offer.original {
            ui.weak(original);
        }
        ui.horizontal(|ui| {
            if crate::agent_parity::action_button(ui, &crate::agent_parity::RECOVERY_RESTORE).clicked() {
                choice.restore = true;
            }
            if crate::agent_parity::action_button(ui, &crate::agent_parity::RECOVERY_DISCARD).clicked() {
                choice.discard = true;
            }
        });
    }
    if offer.report_path.is_some() {
        if let Some(line) = offer.report_line {
            ui.weak(line);
        }
        ui.horizontal(|ui| {
            if ui.button("View report").clicked() {
                choice.view = true;
            }
            if ui.button("Copy to clipboard").clicked()
                && let Some(path) = offer.report_path
            {
                match read_text(path) {
                    Ok(text) => copy_report(ui.ctx(), text),
                    Err(err) => tracing::warn!(target: "regia_editor", "crash report copy: {err}"),
                }
            }
            if ui.button("Open folder").clicked()
                && let Some(path) = offer.report_path
            {
                open_report_folder(path);
            }
        });
    }
    if offer.report_path.is_some() && ui.button("Dismiss report").clicked() {
        choice.dismiss = true;
    }
    if let Some(text) = offer.shown_text {
        ui.separator();
        egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
            ui.monospace(text);
        });
    }
    choice
}

/// Draws the Crash recovery window and returns the buttons pressed this frame.
pub fn recovery_window(ctx: &egui::Context, offer: &RecoveryOffer<'_>) -> RecoveryChoice {
    let mut choice = RecoveryChoice::default();
    egui::Window::new("Crash recovery")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            choice = recovery_offer_ui(ui, offer);
        });
    choice
}

/// `editor.crash_reports` and `editor.crash_report_read`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.crash_reports",
                tool: "crash_reports",
                description: "Crash reports saved for the editor: time, kind (panic, native or pie), the headline line and the path.",
                params: &[],
                result: "{ reports: [{ time, kind, first_line, path }] }",
                mutating: false,
                permission: Permission::Read,
            },
            crash_reports_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.crash_report_read",
                tool: "crash_report_read",
                description: "The text of one editor crash report. `path` is a path from editor.crash_reports. A native bundle returns its crash.txt.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Full path or file name of a crash-*.txt or feedback-*.zip in the report directory.",
                }],
                result: "{ text }",
                mutating: false,
                permission: Permission::Read,
            },
            crash_report_read_method as Handler,
        ),
    ]
}

fn crash_reports_method(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let reports = list_result().map_err(access_error)?;
    let rows: Vec<Value> = reports
        .iter()
        .map(|report| {
            json!({
                "time": report.time_unix_ms,
                "kind": report.kind.as_str(),
                "first_line": report.first_line,
                "path": report.path.display().to_string(),
            })
        })
        .collect();
    Ok(json!({ "reports": rows }))
}

fn crash_report_read_method(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params.get("path").and_then(Value::as_str).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "invalid_report_path",
            "`path` is required",
            "pass a path from editor.crash_reports",
        )
    })?;
    let text = read_resolved(Path::new(path)).map_err(access_error)?;
    Ok(json!({ "text": text }))
}

fn access_error(err: AccessError) -> RpcError {
    match err {
        AccessError::Unavailable => RpcError::new(
            UNSUPPORTED,
            "crash_reports_unavailable",
            "crash reports are not configured",
            "start the editor so it can install its crash hook",
        ),
        AccessError::Invalid => RpcError::new(
            INVALID_PARAMS,
            "invalid_report_path",
            "path is not a crash report in the report directory",
            "pass a path from editor.crash_reports; paths outside that directory are refused",
        ),
        AccessError::NotFound => RpcError::new(
            NOT_FOUND,
            "report_not_found",
            "crash report not found",
            "list editor.crash_reports and pass one of those paths",
        ),
        AccessError::Io(message) => RpcError::new(
            NOT_FOUND,
            "report_not_found",
            message,
            "list editor.crash_reports and pass one of those paths",
        ),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::MutexGuard;

    use regia_test_utils::unique_temp_dir;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Serializes the tests that touch the process-wide crash state (report folder, PIE flag);
    /// other modules' PIE panic tests take it too.
    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a [`with_pie_kind`] body runs now (a panic now would be filed as `pie`); for the
    /// panic hooks of other modules' PIE tests (ADR-7340).
    pub(crate) fn in_pie() -> bool {
        PIE_DEPTH.load(Ordering::SeqCst) > 0
    }

    struct Restore {
        dir: Option<PathBuf>,
        gpu: String,
        limit: usize,
    }

    impl Restore {
        fn capture() -> Self {
            Self {
                dir: regia_platform::crash::crash_dir(),
                gpu: regia_platform::crash::gpu_info(),
                limit: REPORT_BYTES.load(Ordering::Relaxed),
            }
        }
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            regia_platform::crash::configure(
                self.dir.clone(),
                editor_build_info(),
                regia_platform::OsInfo::detect(),
            );
            regia_platform::crash::set_gpu_info(self.gpu.clone());
            REPORT_BYTES.store(self.limit, Ordering::Relaxed);
        }
    }

    fn with_dir(test: impl FnOnce(&Path)) {
        let _guard = lock();
        let restore = Restore::capture();
        let dir = unique_temp_dir("editor-crash");
        install(&dir);
        regia_platform::crash::set_gpu_info("Test Adapter 9000");
        set_report_byte_limit(MAX_REPORT_BYTES);
        test(&dir);
        drop(restore);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn home_path() -> String {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default()
    }

    fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(message) = payload.downcast_ref::<&str>() {
            (*message).to_owned()
        } else if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else {
            "unknown panic".to_owned()
        }
    }

    /// Runs `body` (an editor-side panic fixture) on this thread, its report filed as `panic`
    /// even while another test's PIE frame runs: the thread is marked (`EDITOR_PANIC_HERE`) and
    /// `PIE_KIND_GATE` is held exclusively, so no PIE frame of a parallel test is open while the
    /// hook files the report.
    fn catch_panic(body: impl FnOnce()) -> String {
        let _outside_pie = PIE_KIND_GATE.write().unwrap_or_else(PoisonError::into_inner);
        EDITOR_PANIC_HERE.with(|here| here.set(true));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        EDITOR_PANIC_HERE.with(|here| here.set(false));
        match result {
            Ok(()) => panic!("the fixture was expected to panic"),
            // `&*payload`: `&payload` would hand over the Box itself as the `Any`.
            Err(payload) => panic_message(&*payload),
        }
    }

    fn report_with(needle: &str) -> ListedReport {
        list()
            .into_iter()
            .find(|report| report.first_line.contains(needle))
            .unwrap_or_else(|| panic!("no report containing {needle}: {:?}", list()))
    }

    /// The headline token `regia crash` reads: the first `key: value` whose key is not `unix_ms`
    /// and contains no space. `parse_report` on lane cli-polish-crash uses this rule.
    fn headline_token(text: &str) -> Option<&str> {
        for line in text.lines() {
            if line.starts_with("Regia Engine ") || line.chars().all(|c| c == '=') || line.is_empty() {
                continue;
            }
            let Some((key, _)) = line.split_once(": ") else {
                continue;
            };
            if key == "unix_ms" {
                continue;
            }
            if !key.contains(' ') {
                return Some(key);
            }
        }
        None
    }

    fn log_section_len(text: &str) -> usize {
        let mut inside = false;
        let mut count = 0;
        for line in text.lines() {
            if line.starts_with("recent logs") {
                inside = true;
                continue;
            }
            if inside && line.starts_with("  ") {
                count += 1;
            }
        }
        count
    }

    #[test]
    fn editor_panic_report_matches_the_game_layout() {
        with_dir(|dir| {
            for index in 0..250 {
                regia_core::log::ring::push(
                    "INFO",
                    "editor_crash",
                    format!("EDITOR_CRASH_MARKER-{index:04}"),
                );
            }
            let home = home_path();
            let marker = if home.is_empty() {
                "editor crash marker alpha".to_owned()
            } else {
                format!("editor crash marker alpha at {home}/secret-file")
            };
            let caught = catch_panic(|| panic!("{marker}"));
            assert!(caught.contains("editor crash marker alpha"));
            let report = report_with("editor crash marker alpha");
            assert_eq!(report.kind, ReportKind::Panic);
            assert_eq!(report.path.parent().expect("a report has a parent folder"), dir);
            let text = read_text(&report.path).expect("report text");
            assert_eq!(headline_token(&text), Some("panic"));
            for needle in [
                "Regia Engine crash report",
                "unix_ms: ",
                "panic: ",
                "location: ",
                "build:",
                "version: ",
                "git: ",
                "profile: ",
                "engine: ",
                "system:",
                "os: ",
                "device: ",
                "ram_bytes: ",
                "cpus: ",
                "gpu: Test Adapter 9000",
                "runtime:",
                "frame: ",
                "active_level: ",
                "backtrace:",
                "recent logs (last 200):",
                "editor crash marker alpha",
            ] {
                assert!(text.contains(needle), "missing `{needle}`");
            }
            let ours = text.matches("EDITOR_CRASH_MARKER").count();
            assert!(ours <= 200, "log tail kept {ours} markers");
            let newest = "EDITOR_CRASH_MARKER-0249";
            if !text.contains(newest) {
                assert_eq!(
                    log_section_len(&text),
                    200,
                    "the newest marker is missing and the log section is not full"
                );
            }
            if !home.is_empty() {
                assert!(!text.contains(&home), "home path survived in the report");
                assert!(text.contains('~'), "scrubbed home should leave a tilde");
            }
            let stamped: u64 = text
                .lines()
                .find_map(|line| line.strip_prefix("unix_ms: ")?.trim().parse().ok())
                .expect("unix_ms");
            assert_eq!(report.time_unix_ms, stamped);
            let file_len = std::fs::metadata(&report.path).expect("meta").len();
            assert!(file_len <= MAX_REPORT_BYTES as u64);
        });
    }

    #[test]
    fn report_is_capped_on_a_utf8_boundary() {
        with_dir(|_dir| {
            set_report_byte_limit(800);
            let long = "x".repeat(4000);
            let _ = catch_panic(|| panic!("editor crash marker cap {long}"));
            let report = report_with("editor crash marker cap");
            let text = read_text(&report.path).expect("text");
            assert!(text.starts_with("Regia Engine crash report"));
            assert!(text.contains("[truncated]"));
            assert!(text.len() <= 800, "len {}", text.len());
            assert!(std::str::from_utf8(text.as_bytes()).is_ok());
        });
    }

    #[test]
    fn scrub_and_cap_do_not_need_a_panic() {
        let home = home_path();
        let raw = format!("panic: boom at {home}/secret\nlocation: {home}/src/lib.rs:1:1\n");
        let out = finish_report(&raw, true, 10_000);
        assert!(out.starts_with("pie: boom"));
        assert_eq!(headline_token(&out), Some("pie"));
        if !home.is_empty() {
            assert!(!out.contains(&home));
        }
        // The truncation note is longer than 8 bytes, so a tiny cap keeps a UTF-8 prefix of it.
        let capped = cap_text(&"é".repeat(50), 8);
        assert!(capped.len() <= 8);
        assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
        let noted = cap_text(&"é".repeat(50), 40);
        assert!(noted.contains("[truncated]"));
        assert!(noted.len() <= 40);
        assert!(std::str::from_utf8(noted.as_bytes()).is_ok());
    }

    #[test]
    fn feedback_config_keeps_the_game_defaults() {
        with_dir(|dir| {
            let config = editor_feedback_config();
            let base = regia_framework::feedback::FeedbackConfig::default();
            assert_eq!(config.native_crashes, base.native_crashes);
            assert!(config.native_crashes);
            assert_eq!(config.native_crashes_windows, base.native_crashes_windows);
            assert!(!config.native_crashes_windows);
            assert_eq!(config.crash_bundles, base.crash_bundles);
            assert!(config.crash_bundles);
            assert_eq!(config.dir.as_deref(), Some(dir));
        });
    }

    #[test]
    fn feedback_plugin_updates_without_the_game_ui() {
        let _guard = lock();
        let dir = unique_temp_dir("editor-crash-ui");
        let mut app = crate::engine::app::App::new();
        app.add_plugins(crate::engine::app::MinimalPlugins);
        app.add_plugins(regia_framework::feedback::FeedbackPlugin {
            config: regia_framework::feedback::FeedbackConfig {
                dir: Some(dir.clone()),
                native_crashes: false,
                crash_bundles: false,
                ..regia_framework::feedback::FeedbackConfig::default()
            },
        });
        app.finish_plugins();
        app.update();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_sort_by_timestamp_across_text_and_native_bundles() {
        with_dir(|dir| {
            std::fs::write(dir.join("crash-a.txt"), "unix_ms: 30\npanic: newest\n").expect("newest");
            std::fs::write(dir.join("crash-z.txt"), "unix_ms: 10\npie: oldest\n").expect("oldest");
            let mut zip = regia_framework::feedback::zip::ZipWriter::new(0);
            zip.add("crash.txt", b"unix_ms: 20\nnative crash: middle\n");
            std::fs::write(dir.join("feedback-native-sort.zip"), zip.finish()).expect("bundle");
            let reports = list();
            let times: Vec<_> = reports.iter().map(|report| report.time_unix_ms).collect();
            assert_eq!(times, [10, 20, 30]);
            assert_eq!(reports.last().expect("newest report").first_line, "panic: newest");
        });
    }

    #[cfg(unix)]
    #[test]
    fn reports_do_not_follow_links_outside_the_report_folder() {
        with_dir(|dir| {
            let outside = unique_temp_dir("editor-crash-outside");
            std::fs::create_dir_all(&outside).expect("outside directory");
            let secret = outside.join("private.txt");
            std::fs::write(&secret, "panic: private data\n").expect("outside file");
            let linked = dir.join("crash-link.txt");
            std::os::unix::fs::symlink(&secret, &linked).expect("link");
            assert!(list().is_empty());
            assert!(matches!(read_resolved(&linked), Err(AccessError::Invalid)));
            std::fs::remove_dir_all(&outside).expect("cleanup");
        });
    }

    #[test]
    fn native_bundle_is_listed_as_native() {
        with_dir(|dir| {
            let mut zip = regia_framework::feedback::zip::ZipWriter::new(0);
            zip.add("crash.txt", b"native crash: SIGSEGV\nfault address: 0x10\n");
            let bytes = zip.finish();
            let path = dir.join("feedback-native-test.zip");
            std::fs::write(&path, bytes).expect("zip");
            let report = list()
                .into_iter()
                .find(|report| report.kind == ReportKind::Native)
                .expect("native report");
            assert_eq!(report.first_line, "native crash: SIGSEGV");
            assert_eq!(report.path, path);
            let text = read_text(&path).expect("crash.txt");
            assert!(text.starts_with("native crash:"));
        });
    }

    #[test]
    fn native_report_read_obeys_the_text_read_cap() {
        with_dir(|dir| {
            let mut zip = regia_framework::feedback::zip::ZipWriter::new(0);
            let text = format!("native crash: fixture\n{}", "é".repeat(READ_CAP as usize));
            zip.add("crash.txt", text.as_bytes());
            let path = dir.join("feedback-native-large.zip");
            std::fs::write(&path, zip.finish()).expect("bundle");
            let read = read_text(&path).expect("report text");
            assert!(read.starts_with("native crash: fixture"));
            assert!(read.ends_with(TRUNCATED));
            assert!(read.len() <= READ_CAP as usize + TRUNCATED.len());
        });
    }

    #[test]
    fn agent_lists_and_reads_and_refuses_other_paths() {
        with_dir(|dir| {
            let _ = catch_panic(|| panic!("editor crash marker agent"));
            let mut world = World::new();
            let listed = crash_reports_method(&mut world, &json!({})).expect("list");
            let rows = listed["reports"].as_array().expect("rows");
            let row = rows
                .iter()
                .find(|row| {
                    row["first_line"]
                        .as_str()
                        .unwrap_or("")
                        .contains("editor crash marker agent")
                })
                .expect("row");
            assert_eq!(row["kind"], "panic");
            assert!(row["time"].as_u64().is_some());
            let path = row["path"].as_str().expect("path");
            let read = crash_report_read_method(&mut world, &json!({ "path": path })).expect("read");
            assert!(
                read["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("editor crash marker agent")
            );
            let outside =
                crash_report_read_method(&mut world, &json!({ "path": "../crash-x.txt" })).expect_err("..");
            assert_eq!(error_code(&outside), "invalid_report_path");
            let missing = dir.join("crash-missing.txt");
            let absent =
                crash_report_read_method(&mut world, &json!({ "path": missing.display().to_string() }))
                    .expect_err("missing");
            assert_eq!(error_code(&absent), "report_not_found");
            let sneaky = dir.join("notes.txt");
            std::fs::write(&sneaky, b"secret").expect("notes");
            let refused =
                crash_report_read_method(&mut world, &json!({ "path": sneaky.display().to_string() }))
                    .expect_err("not a report");
            assert_eq!(error_code(&refused), "invalid_report_path");
            assert_eq!(methods()[0].0.name, "editor.crash_reports");
            assert_eq!(methods()[1].0.name, "editor.crash_report_read");
        });
    }

    #[test]
    fn crash_reports_are_accessible_through_the_agent_dispatcher() {
        with_dir(|dir| {
            let path = dir.join("crash-agent-dispatch.txt");
            std::fs::write(&path, "unix_ms: 42\npie: dispatcher fixture\nbacktrace:\n")
                .expect("fixture report");
            let mut app = crate::engine::app::App::new();
            app.add_plugins(crate::engine::app::MinimalPlugins);
            app.insert_resource(regia_agent::AgentSettings {
                project_dir: Some(dir.to_path_buf()),
            });
            app.add_plugins(regia_agent::AgentPlugin);
            crate::agent_ext::install(app.world_mut());
            app.finish_plugins();
            let (sender, inbox) = crossbeam_channel::unbounded();
            app.insert_resource(regia_agent::AgentInbox(inbox));
            let mut call = |method: &str, params: Value| {
                let (reply, answer) = crossbeam_channel::bounded(1);
                sender
                    .send(regia_agent::server::Inbound {
                        request: regia_agent::protocol::Request {
                            jsonrpc: "2.0".into(),
                            id: Some(json!(1)),
                            method: method.into(),
                            params,
                        },
                        reply,
                    })
                    .expect("agent inbox");
                app.update();
                let response = answer.try_recv().expect("dispatch answers in one frame");
                assert!(response.error.is_none(), "{method}: {:?}", response.error);
                response.result.expect("agent result")
            };
            let reports = call("editor.crash_reports", json!({}));
            assert_eq!(reports["reports"][0]["time"], 42);
            assert_eq!(reports["reports"][0]["kind"], "pie");
            let read = call(
                "editor.crash_report_read",
                json!({ "path": reports["reports"][0]["path"] }),
            );
            assert!(
                read["text"]
                    .as_str()
                    .expect("text")
                    .contains("pie: dispatcher fixture")
            );
        });
    }

    #[test]
    fn agent_reports_when_the_directory_is_not_configured() {
        let _guard = lock();
        let restore = Restore::capture();
        regia_platform::crash::configure(None, editor_build_info(), regia_platform::OsInfo::detect());
        let mut world = World::new();
        let err = crash_reports_method(&mut world, &json!({})).expect_err("unconfigured");
        assert_eq!(error_code(&err), "crash_reports_unavailable");
        drop(restore);
    }

    fn error_code(err: &RpcError) -> &str {
        err.data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    fn frame_text(ctx: &egui::Context, mut draw: impl FnMut(&egui::Context)) -> String {
        fn walk(shape: &egui::Shape, out: &mut String) {
            match shape {
                egui::Shape::Text(text) => {
                    out.push_str(text.galley.text());
                    out.push('\n');
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, out)),
                _ => {}
            }
        }
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        // A new window's first pass only measures it and paints nothing: keep the second.
        let mut first = ctx.run_ui(raw.clone(), |ui| draw(ui.ctx()));
        first.textures_delta.clear();
        let mut output = ctx.run_ui(raw, |ui| draw(ui.ctx()));
        output.textures_delta.clear();
        let mut text = String::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut text);
        }
        text
    }

    #[test]
    fn recovery_dialog_offers_the_report_actions() {
        let ctx = egui::Context::default();
        let path = Path::new("/tmp/regia-editor-crash/crash-test.txt");
        let both = frame_text(&ctx, |ctx| {
            let _ = recovery_window(
                ctx,
                &RecoveryOffer {
                    autosave: Some("/tmp/level.autosave"),
                    original: Some("(untitled level)"),
                    report_line: Some("panic: boom"),
                    report_path: Some(path),
                    shown_text: None,
                },
            );
        });
        for label in [
            "Crash recovery",
            "The editor closed unexpectedly.",
            "A previous editor session left an unsaved level behind:",
            "Restore",
            "Discard autosave",
            "View report",
            "Copy to clipboard",
            "Open folder",
            "Dismiss report",
            "panic: boom",
        ] {
            assert!(both.contains(label), "missing `{label}` in:\n{both}");
        }
        let report_only = frame_text(&ctx, |ctx| {
            let _ = recovery_window(
                ctx,
                &RecoveryOffer {
                    autosave: None,
                    original: None,
                    report_line: Some("pie: game"),
                    report_path: Some(path),
                    shown_text: Some("pie: game\nbacktrace:\n"),
                },
            );
        });
        assert!(report_only.contains("The editor closed unexpectedly."));
        assert!(report_only.contains("View report"));
        assert!(!report_only.contains("Restore"));
        assert!(report_only.contains("pie: game"));
        let autosave_only = frame_text(&ctx, |ctx| {
            let _ = recovery_window(
                ctx,
                &RecoveryOffer {
                    autosave: Some("/tmp/level.autosave"),
                    original: Some("(untitled level)"),
                    report_line: None,
                    report_path: None,
                    shown_text: None,
                },
            );
        });
        assert!(autosave_only.contains("The editor closed unexpectedly."));
        assert!(autosave_only.contains("Restore"));
        assert!(!autosave_only.contains("View report"));
    }

    #[test]
    fn copy_and_open_folder_use_the_button_functions() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| copy_report(ui.ctx(), "panic: boom".to_owned()));
        output.textures_delta.clear();
        assert!(
            output.platform_output.commands.iter().any(
                |command| matches!(command, egui::OutputCommand::CopyText(text) if text == "panic: boom")
            )
        );
        let path = Path::new("/tmp/regia-editor-crash/crash-test.txt");
        open_report_folder(path);
        assert_eq!(
            take_opened_folder().as_deref(),
            Some(Path::new("/tmp/regia-editor-crash"))
        );
    }

    #[test]
    fn an_editor_panic_is_not_filed_as_pie_while_another_thread_plays() {
        // Another libtest thread inside play-in-editor (as every PIE test is) for almost all of
        // this test's run: without the gate each panic below read `PIE_DEPTH > 0` and was filed
        // as `pie`.
        with_dir(|_dir| {
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
            let player = {
                let stop = std::sync::Arc::clone(&stop);
                let entered = std::sync::Arc::clone(&entered);
                std::thread::spawn(move || {
                    let mut first = true;
                    while !stop.load(Ordering::SeqCst) {
                        with_pie_kind(|| {
                            if std::mem::take(&mut first) {
                                entered.wait();
                            }
                            for _ in 0..10_000 {
                                std::hint::spin_loop();
                            }
                        });
                    }
                })
            };
            entered.wait();
            for round in 0..20 {
                let needle = format!("editor crash marker beside pie {round:02}");
                let _ = catch_panic(|| panic!("{needle}"));
                assert_eq!(report_with(&needle).kind, ReportKind::Panic, "round {round}");
            }
            stop.store(true, Ordering::SeqCst);
            player.join().expect("the PIE thread ends");
        });
    }

    #[inline(never)]
    fn pie_fixture_panics() {
        panic!("pie fixture boom");
    }

    #[test]
    fn pie_panic_writes_a_pie_report_and_the_editor_continues() {
        with_dir(|_dir| {
            let mut app = crate::engine::app::App::new();
            app.add_plugins(crate::engine::app::MinimalPlugins);
            app.add_systems(crate::engine::app::prelude::Update, pie_fixture_panics);
            app.finish_plugins();
            let mut session = crate::pie::PieSession {
                app,
                mode: crate::pie::PlayMode::Playing,
                frames: 0,
                started: std::time::Instant::now(),
                start_elapsed_ms: 0,
                entities: 0,
                game: None,
                panic: None,
                panic_backtrace: None,
                options: crate::pie::PlayOptions::default(),
            };
            let message = session.frame().expect("the fixture panics");
            assert!(message.contains("pie fixture boom"));
            let report = report_with("pie fixture boom");
            assert_eq!(report.kind, ReportKind::Pie);
            let text = read_text(&report.path).expect("text");
            assert_eq!(headline_token(&text), Some("pie"));
            assert!(text.contains("backtrace:"));
            assert!(
                text.contains("pie_fixture_panics"),
                "game backtrace should name the fixture:\n{text}"
            );
            crate::pie::stop_play(session);
            assert!(list().iter().any(|report| report.kind == ReportKind::Pie));
        });
    }
}
