// SPDX-License-Identifier: MPL-2.0
//! File ▸ Package for platform… and Tools ▸ Clear caches as cancellable editor jobs
//! (`docs/specs/19-editor.md` §§ 2, 7; `docs/specs/21-tooling-and-packaging.md` § 1; ADR-7475).
//!
//! Both run the `regia` command line the CLI user runs, as a child process in the project root,
//! so the editor and the terminal package the same way (spec 21 § 1: the editor calls the same
//! code path): `regia package --platform P --profile R --out D --project <root> --json
//! [--dry-run]` and `regia clean --cache --json --project <root>`. The program is
//! [`regia_agent::cook_package::RegiaCliProgram`] when a host or test set it, else
//! [`crate::bake_runner::cli_program`] (`$REGIA_CLI`, `regia` beside the editor, `regia` on
//! `PATH`).
//!
//! Each run is one job of the agent job registry (`package:<n>`, `clean:<n>`; `agent.job_status`
//! sees them), registered in [`crate::editor_jobs::EditorJobs`] (so `editor.cancel_job { id }`
//! stops it) and published as a [`crate::tasks::BackgroundTasks`] row (`package`,
//! `clear_caches`; so `editor.cancel_task { id }` and the dialog's Cancel stop it). A worker
//! thread owns the child: it streams the output lines into the job and kills the child's whole
//! process group (Unix) or process tree (Windows) on cancel. The UI thread never waits on the
//! child.
//!
//! A package run never writes into the output folder directly (ADR-7496). The editor creates a
//! private staging folder inside it (`<out>/.regia-package-<pid>-<n>`, see [`STAGING_PREFIX`]) and
//! passes that as `--out`; only when the run succeeds are its entries moved into `<out>`, each
//! replacing the entry of the same name (the CLI's own `<slug>/` rebuild and archive replace),
//! except `symbols/`, which merges file by file the way the CLI's `keep_shipping_symbols` does
//! (a file parked there by another run survives). A cancelled, failed or dry run removes the
//! staging folder and the folders the editor created
//! for it, nothing else: files other programs put into `<out>` meanwhile and the previous build
//! stay. `editor.package_start`'s `out` is a request path (ADR-1816) and the method needs
//! `confirm: true` unless it is a dry run, as `agent.package` does.
//!
//! Error codes (`data.reason`): `package_no_project`, `package_bad_target`,
//! `package_bad_profile`, `package_busy`, `package_spawn_failed`, `package_out_unwritable`,
//! `needs_confirmation`, `path_outside_project`; a run that exits non-zero (or whose finished
//! package cannot be moved into place) fails its job with `package_failed` (`clean_failed` for
//! Clear caches); a cancelled run ends its job `cancelled` with result code `package_cancelled`
//! (`clean_cancelled`).

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use regia_agent::jobs::{JobHandle, JobRegistry, JobState};
use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::tasks::{BackgroundTasks, CancelToken, TaskRow};
use crate::viewport_agent::Handler;

/// The background-task id of the package run (`editor.cancel_task { id: "package" }`).
pub const PACKAGE_TASK: &str = "package";
/// The background-task id of the cache clear (`editor.cancel_task { id: "clear_caches" }`).
pub const CLEAN_TASK: &str = "clear_caches";
/// The targets the dialog offers (`regia package --platform`).
pub const TARGETS: &[&str] = &["linux", "windows", "android"];
/// The profiles every project has; `[profiles.<name>]` entries of `Regia.toml` add more.
pub const PROFILES: &[&str] = &["dev", "shipping"];
/// Output lines a job keeps for the dialog and `editor.package_status` (the log file has all).
pub const KEEP_LINES: usize = 400;
/// Polls (100 ms each) the worker waits for the output to close after a kill.
const CANCEL_GRACE_POLLS: u32 = 50;
/// Longest output line kept, in bytes.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// The name prefix of a package run's private staging folder inside its output folder.
pub const STAGING_PREFIX: &str = ".regia-package-";
/// The suffix of a staging folder's owner lock file (`<staging>.lock`, beside the folder): the run
/// holds an exclusive file lock on it while it owns the folder, so a crash releases it and a
/// sweep ([`sweep_stale_staging`]) can tell a dead run's folder from a live one's.
const LOCK_SUFFIX: &str = ".lock";
/// Tries [`OutputGuard::prepare`] makes to hold its owner lock while sweeps race it.
const LOCK_ATTEMPTS: usize = 16;

/// Why a package or clear-caches request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    /// The lowercase protocol reason (`data.reason`).
    pub reason: &'static str,
    /// What to show the user.
    pub message: String,
}

impl JobError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }

    /// The protocol error of this refusal.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let (code, regia) = match self.reason {
            "package_no_project" | "package_not_found" => (NOT_FOUND, "F0003"),
            "package_spawn_failed" | "package_out_unwritable" => (UNSUPPORTED, "F0013"),
            _ => (INVALID_PARAMS, "F0002"),
        };
        let mut error = RpcError::new(
            code,
            regia,
            self.message.clone(),
            "see book/agents/editor-menus.md",
        );
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("reason".into(), Value::String(self.reason.to_string()));
        }
        error
    }
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for JobError {}

/// One File ▸ Package for platform request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRequest {
    /// `linux`, `windows` or `android`.
    pub target: String,
    /// `dev`, `shipping` or a `[profiles.<name>]` entry.
    pub profile: String,
    /// The output directory (`None`: `<project>/dist/<target>`, the CLI's default).
    pub out: Option<PathBuf>,
    /// `--dry-run`: cook if needed, print the size report, build and write nothing.
    pub dry_run: bool,
}

impl Default for PackageRequest {
    fn default() -> Self {
        Self {
            target: host_target().to_string(),
            profile: "dev".to_string(),
            out: None,
            dry_run: false,
        }
    }
}

impl PackageRequest {
    /// Checks the target and the profile name.
    ///
    /// # Errors
    /// `package_bad_target` or `package_bad_profile`.
    pub fn validate(&self) -> Result<(), JobError> {
        if !TARGETS.contains(&self.target.as_str()) {
            return Err(JobError::new(
                "package_bad_target",
                format!("unknown target {:?} (linux, windows or android)", self.target),
            ));
        }
        let name_ok = !self.profile.is_empty()
            && self.profile.len() <= 64
            && !self.profile.starts_with('-')
            && self
                .profile
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !name_ok {
            return Err(JobError::new(
                "package_bad_profile",
                format!(
                    "profile {:?} is not a profile name (dev, shipping or a [profiles.<name>] entry)",
                    self.profile
                ),
            ));
        }
        Ok(())
    }

    /// The output directory the run writes: `out`, else `<root>/dist/<target>`.
    #[must_use]
    pub fn out_dir(&self, root: &Path) -> PathBuf {
        match &self.out {
            Some(out) if out.is_absolute() => out.clone(),
            Some(out) => root.join(out),
            None => root.join("dist").join(&self.target),
        }
    }

    /// The `regia` arguments (without the program), writing into [`Self::out_dir`].
    #[must_use]
    pub fn args(&self, root: &Path) -> Vec<OsString> {
        self.args_into(root, &self.out_dir(root))
    }

    /// The `regia` arguments with `--out out` (the run's private staging folder).
    #[must_use]
    pub fn args_into(&self, root: &Path, out: &Path) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "package".into(),
            "--platform".into(),
            self.target.clone().into(),
            "--profile".into(),
            self.profile.clone().into(),
            "--out".into(),
            out.as_os_str().to_owned(),
            "--project".into(),
            root.as_os_str().to_owned(),
            "--json".into(),
        ];
        if self.dry_run {
            args.push("--dry-run".into());
        }
        args
    }
}

/// The platform tag of the machine the editor runs on (the dialog's default target).
#[must_use]
pub fn host_target() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// The `[profiles.<name>]` names of `Regia.toml` text besides `dev` and `shipping`, in order.
#[must_use]
pub fn extra_profiles(manifest: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some(name) = line
            .strip_prefix("[profiles.")
            .and_then(|rest| rest.strip_suffix(']'))
            .map(|name| name.trim().trim_matches('"'))
        else {
            continue;
        };
        if !name.is_empty()
            && !name.contains('.')
            && !PROFILES.contains(&name)
            && !out.iter().any(|have| have == name)
        {
            out.push(name.to_string());
        }
    }
    out
}

/// What a job's worker has seen so far, shared with the editor.
#[derive(Debug, Default)]
struct JobLog {
    lines: Vec<String>,
    total_lines: u64,
    message: String,
    report: Option<Value>,
    outcome: Option<Value>,
    finished: bool,
    /// The extension after-package hooks ran on this run's output (ADR-7791).
    hooks_ran: bool,
}

#[derive(Debug, Default)]
struct Shared {
    log: Mutex<JobLog>,
    changed: Condvar,
}

impl Shared {
    fn with<R>(&self, f: impl FnOnce(&mut JobLog) -> R) -> R {
        let mut guard = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let out = f(&mut guard);
        self.changed.notify_all();
        out
    }
}

/// Which command line a [`CliJob`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliKind {
    /// `regia package`.
    Package,
    /// `regia clean --cache`.
    ClearCaches,
}

impl CliKind {
    /// The job registry kind (`package`, `clean`).
    #[must_use]
    pub fn job_kind(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::ClearCaches => "clean",
        }
    }

    /// The background-task id ([`PACKAGE_TASK`], [`CLEAN_TASK`]).
    #[must_use]
    pub fn task(self) -> &'static str {
        match self {
            Self::Package => PACKAGE_TASK,
            Self::ClearCaches => CLEAN_TASK,
        }
    }

    fn failed_code(self) -> &'static str {
        match self {
            Self::Package => "package_failed",
            Self::ClearCaches => "clean_failed",
        }
    }

    fn cancelled_code(self) -> &'static str {
        match self {
            Self::Package => "package_cancelled",
            Self::ClearCaches => "clean_cancelled",
        }
    }
}

/// A running or finished `regia` run of the editor. Cheap to clone; every clone sees the same
/// job.
#[derive(Debug, Clone)]
pub struct CliJob {
    /// The job id (`package:<n>`, `clean:<n>`).
    pub id: String,
    /// What it runs.
    pub kind: CliKind,
    /// The output directory a finished run's package is moved into (`None`: the run publishes
    /// nothing: a dry run or Clear caches).
    pub out: Option<PathBuf>,
    /// The private staging folder the run writes (`--out`); removed when the run ends.
    pub staging: Option<PathBuf>,
    /// Where the whole output is written when the run ends.
    pub log_path: PathBuf,
    /// The command line (program first).
    pub command: Vec<String>,
    handle: JobHandle,
    token: CancelToken,
    shared: Arc<Shared>,
}

impl CliJob {
    /// The job's state now.
    #[must_use]
    pub fn state(&self) -> JobState {
        self.handle.state()
    }

    /// Whether the worker still runs (the job state can end a moment before the worker's last
    /// write; this reads the worker's own flag).
    #[must_use]
    pub fn running(&self) -> bool {
        !self.shared.with(|log| log.finished)
    }

    /// Asks the run to stop (the same flag the Cancel button and `editor.cancel_task` set).
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// The last [`KEEP_LINES`] output lines.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.shared.with(|log| log.lines.clone())
    }

    /// The progress line (the last output line).
    #[must_use]
    pub fn message(&self) -> String {
        self.shared.with(|log| log.message.clone())
    }

    /// The run's outcome once it ended: `{ ok, code?, exit_code, artifact?, report?, removed?,
    /// log, ... }`.
    #[must_use]
    pub fn outcome(&self) -> Option<Value> {
        self.shared.with(|log| log.outcome.clone())
    }

    /// Blocks until the worker has ended (tests, host shutdown). No sleep: a condition variable
    /// the worker signals.
    pub fn wait(&self) -> Option<Value> {
        let mut guard = self.shared.log.lock().unwrap_or_else(PoisonError::into_inner);
        while !guard.finished {
            guard = self
                .shared
                .changed
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        guard.outcome.clone()
    }

    /// Blocks until an output line contains `needle` (true) or the run ended without one
    /// (false). Tests use it to cancel at a known point.
    pub fn wait_for_line(&self, needle: &str) -> bool {
        let mut seen = 0usize;
        let mut guard = self.shared.log.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let start = seen.min(guard.lines.len());
            if guard.lines[start..].iter().any(|line| line.contains(needle)) {
                return true;
            }
            seen = guard.lines.len();
            if guard.finished {
                return false;
            }
            guard = self
                .shared
                .changed
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The status object `editor.package_status` answers.
    #[must_use]
    pub fn status_json(&self) -> Value {
        let (lines, total, message, outcome) = self.shared.with(|log| {
            let tail = log.lines.len().saturating_sub(40);
            (
                log.lines[tail..].to_vec(),
                log.total_lines,
                log.message.clone(),
                log.outcome.clone(),
            )
        });
        let state = self.handle.state();
        // The job reads ended a moment before the worker's last write: report `running` until
        // the outcome is in place, so a caller never sees `done` without its result.
        let state_name = if outcome.is_none() {
            "running"
        } else {
            state.name()
        };
        json!({
            "job": self.id,
            "task": self.kind.task(),
            "state": state_name,
            "message": message,
            "lines": lines,
            "line_count": total,
            "out": self.out.as_ref().map(|p| p.display().to_string()),
            "staging": self.staging.as_ref().map(|p| p.display().to_string()),
            "log": self.log_path.display().to_string(),
            "command": self.command,
            "result": outcome,
        })
    }
}

/// The editor's package and clear-caches runs (the newest of each kind).
#[derive(Resource, Debug, Clone, Default)]
pub struct CliJobs {
    /// The newest package run.
    pub package: Option<CliJob>,
    /// The newest clear-caches run.
    pub clean: Option<CliJob>,
}

impl CliJobs {
    /// Whether either run is still going.
    #[must_use]
    pub fn busy(&self) -> Option<&CliJob> {
        [self.package.as_ref(), self.clean.as_ref()]
            .into_iter()
            .flatten()
            .find(|job| job.running())
    }
}

/// The `regia` program the jobs run.
#[must_use]
pub fn program(world: &World) -> PathBuf {
    world
        .get_resource::<regia_agent::cook_package::RegiaCliProgram>()
        .map(|p| p.0.clone())
        .unwrap_or_else(crate::bake_runner::cli_program)
}

/// The project root of the editor, when it is a project (has `Regia.toml`).
fn project(world: &World) -> Result<PathBuf, JobError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| JobError::new("package_no_project", "open a project first (the editor has none)"))?;
    if !regia_core::brand::is_project_root(&root) {
        return Err(JobError::new(
            "package_no_project",
            format!(
                "{} has no Regia.toml, so there is nothing to package",
                root.display()
            ),
        ));
    }
    Ok(root)
}

/// What runs now that shares the cooked cache, the asset database and the paks: an editor
/// package or Clear caches run, or an agent cook, package or clean job. `None` when idle. Tools ▸
/// Rebuild asset database is disabled and refused (`tools_busy`) while this is `Some`.
#[must_use]
pub fn busy_reason(world: &World) -> Option<String> {
    if let Some(job) = world.get_resource::<CliJobs>().and_then(CliJobs::busy) {
        return Some(format!("{} is still running; cancel it or wait for it", job.id));
    }
    let registry = world.get_resource::<JobRegistry>()?;
    ["cook", "package", "clean"].into_iter().find_map(|kind| {
        registry
            .running(kind)
            .map(|running| format!("{running} is still running; poll it with agent.job_status"))
    })
}

/// How old a staging folder with no owner lock file must be, besides its owner process being gone,
/// before the sweep removes it (ADR-7744, review finding S1). A folder's modified time moves only
/// when its top-level entries change, so a long live run can look old; the owner check is what
/// keeps it, this age only guards a process id the check cannot see (another machine sharing the
/// folder).
const LOCKLESS_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Whether process `pid` runs on this machine: `None` when this platform cannot tell (the sweep
/// then keeps what it would otherwise remove).
fn process_alive(pid: u32) -> Option<bool> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // `/proc/self` missing means no procfs to ask.
        if !Path::new("/proc/self").exists() {
            return None;
        }
        Some(Path::new(&format!("/proc/{pid}")).exists())
    }
    #[cfg(windows)]
    {
        Some(regia_platform::native_crash::process_alive(pid))
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    {
        // No procfs (macOS): `kill -0` answers whether the process exists; any answer but "no
        // such process" counts as unknown.
        let probe = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .ok()?;
        if probe.status.success() {
            return Some(true);
        }
        String::from_utf8_lossy(&probe.stderr)
            .contains("No such process")
            .then_some(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        None
    }
}

/// The owner process id a staging folder's name carries (`.regia-package-<pid>-<n>`).
fn owner_pid(name: &str) -> Option<u32> {
    let (pid, n) = name.strip_prefix(STAGING_PREFIX)?.split_once('-')?;
    n.parse::<u64>().ok()?;
    pid.parse().ok()
}

/// The packaging process recorded in an owner lock file ([`OutputGuard::record_writer`]): the
/// `regia package` child, which outlives a crashed editor and still writes the folder.
fn recorded_writer(lock: &mut std::fs::File) -> Option<u32> {
    use std::io::Seek as _;
    let mut text = String::new();
    lock.seek(std::io::SeekFrom::Start(0)).ok()?;
    lock.take(32).read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// Opens (creating it when `create`) and exclusively locks the owner lock file at `path`; answers
/// the handle only when the lock is ours and `path` still names the locked file. A sweep may
/// unlink an unlocked lock file between another process's open and lock (review finding S3): a
/// lock on an unlinked file protects nothing.
fn lock_owner_file(path: &Path, create: bool) -> Option<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
        .ok()?;
    #[cfg(test)]
    tests::between_open_and_lock(path);
    file.try_lock().ok()?;
    // A second handle on the path cannot take the lock this one holds (file locks are per open
    // file on every platform): if it can, or the path is gone, the path names another file.
    let again = std::fs::OpenOptions::new().read(true).write(true).open(path);
    match again.map(|other| other.try_lock()) {
        Ok(Err(std::fs::TryLockError::WouldBlock)) => Some(file),
        _ => None,
    }
}

/// Removes an owner lock file while still holding its lock, then lets the lock go. In the other
/// order a run that took the same name in between would lose its fresh lock file (review finding
/// S3).
fn unlink_then_unlock(lock: std::fs::File, path: &Path) {
    let _ = std::fs::remove_file(path);
    #[cfg(test)]
    tests::between_unlink_and_unlock(path);
    drop(lock);
}

/// Removes the stale package staging folders (`.regia-package-*`) a crashed run left in `out`;
/// answers what it removed (ADR-7744).
///
/// A folder with an owner lock file (`<folder>.lock`) is stale when the lock can be taken (a run
/// holds it for as long as it owns the folder, and the operating system releases it when the run
/// dies) and the `regia package` process the file names is gone too (it outlives a crashed
/// editor and still writes the folder). A folder with no lock file (an older editor's) is removed
/// only when the editor process its name carries is gone on this machine and the folder is older
/// than `LOCKLESS_MIN_AGE` at `now`; when the platform cannot tell whether that process runs, or
/// the name carries none, it stays. So a live job's folder (this process's or another editor's)
/// is never removed. A lock file with no folder goes too. Entries that are not staging folders
/// are never touched.
pub fn sweep_stale_staging(out: &Path, now: std::time::SystemTime) -> Vec<PathBuf> {
    let _span = tracing::info_span!(target: "regia_editor", "package_staging_sweep").entered();
    let Ok(entries) = std::fs::read_dir(out) else {
        return Vec::new();
    };
    let lock_of = |name: &str| out.join(format!("{name}{LOCK_SUFFIX}"));
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(STAGING_PREFIX) {
            continue;
        }
        let path = entry.path();
        let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        if let Some(folder) = name.strip_suffix(LOCK_SUFFIX) {
            // A lock file whose folder is gone: orphaned, unless a run holds it.
            if !out.join(folder).exists()
                && let Some(lock) = lock_owner_file(&path, false)
            {
                unlink_then_unlock(lock, &path);
            }
            continue;
        }
        if !is_dir {
            continue;
        }
        let lock_path = lock_of(&name);
        let lock = match std::fs::symlink_metadata(&lock_path) {
            // A lock file: stale only when nobody holds the lock and its writer is gone.
            Ok(_) => {
                let Some(mut lock) = lock_owner_file(&lock_path, false) else {
                    continue;
                };
                if let Some(writer) = recorded_writer(&mut lock)
                    && process_alive(writer) != Some(false)
                {
                    tracing::info!(
                        target: "regia_editor",
                        "keeping package staging folder {}: its packaging process {writer} still runs",
                        path.display()
                    );
                    continue;
                }
                Some(lock)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let owner_gone = owner_pid(&name).and_then(process_alive) == Some(false);
                let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|modified| {
                    now.duration_since(modified)
                        .is_ok_and(|age| age >= LOCKLESS_MIN_AGE)
                });
                if !(owner_gone && old) {
                    tracing::debug!(
                        target: "regia_editor",
                        "keeping package staging folder {} (no owner lock; owner gone: {owner_gone}, old: {old})",
                        path.display()
                    );
                    continue;
                }
                None
            }
            Err(_) => continue,
        };
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed.push(path),
            Err(error) => tracing::warn!(
                target: "regia_editor",
                "cannot remove the stale package staging folder {}: {error}",
                path.display()
            ),
        }
        if let Some(lock) = lock {
            unlink_then_unlock(lock, &lock_path);
        }
    }
    removed
}

/// Sweeps `dirs` ([`sweep_stale_staging`]) on a worker thread: a multi-gigabyte leftover must
/// not stall a frame (review finding S4). The sweep is safe beside live runs, so nothing waits.
fn sweep_in_background(dirs: Vec<PathBuf>) {
    let spawned = std::thread::Builder::new()
        .name("regia-editor-staging-sweep".into())
        .spawn(move || {
            let now = std::time::SystemTime::now();
            for dir in dirs {
                let removed = sweep_stale_staging(&dir, now);
                if !removed.is_empty() {
                    tracing::info!(
                        target: "regia_editor",
                        "removed {} stale package staging folder(s) in {}",
                        removed.len(),
                        dir.display()
                    );
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(target: "regia_editor", "cannot start the staging sweep: {error}");
    }
}

/// The project whose `dist/` the editor already swept at start.
#[derive(Resource, Debug, Default)]
struct SweptProject(Option<PathBuf>);

/// Sweeps the stale staging folders of every `<project>/dist/<target>` once per project (the
/// editor's first frames); [`start_package`] sweeps the run's own output folder every time.
fn sweep_project_once(world: &mut World) {
    let Some(root) = crate::mesh_source::project_root(world) else {
        return;
    };
    let mut swept = world.remove_resource::<SweptProject>().unwrap_or_default();
    if swept.0.as_deref() != Some(root.as_path()) {
        swept.0 = Some(root.clone());
        if let Ok(targets) = std::fs::read_dir(root.join("dist")) {
            sweep_in_background(targets.flatten().map(|target| target.path()).collect());
        }
    }
    world.insert_resource(swept);
}

/// Refuses a new run while a package, a clear or an agent cook/package runs (they share the
/// cooked cache and the paks).
fn check_idle(world: &World) -> Result<(), JobError> {
    match busy_reason(world) {
        Some(message) => Err(JobError::new("package_busy", message)),
        None => Ok(()),
    }
}

/// Starts File ▸ Package for platform: validates `request`, then runs `regia package` as job
/// `package:<n>`.
///
/// # Errors
/// `package_no_project`, `package_bad_target`, `package_bad_profile`, `package_busy` or
/// `package_spawn_failed` (the `regia` program could not start; the message names it and
/// `REGIA_CLI`).
pub fn start_package(world: &mut World, request: &PackageRequest) -> Result<CliJob, JobError> {
    let root = project(world)?;
    request.validate()?;
    check_idle(world)?;
    // Extension before-cook hooks (ADR-7791): an abort refuses the run; their lines open its log.
    let hooks = crate::extension::hooks::run_stage(
        world,
        &crate::extension::hooks::HookInput {
            stage: crate::extension::hooks::HookStage::BeforeCook,
            project: root.clone(),
            target: request.target.clone(),
            profile: request.profile.clone(),
            out: None,
        },
    );
    if let Some(message) = hooks.aborted {
        return Err(JobError::new("package_hook_aborted", message));
    }
    let out = request.out_dir(&root);
    let out = std::path::absolute(&out).unwrap_or(out);
    // A crashed run's staging folder goes (ADR-7744), off the UI thread.
    sweep_in_background(vec![out.clone()]);
    static NEXT_STAGING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT_STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let guard = OutputGuard::prepare(&out, &format!("{}-{n}", std::process::id()), request.dry_run)?;
    let args = request.args_into(&root, &guard.staging);
    let label = format!("{} package for {}", request.profile, request.target);
    let job = start(world, CliKind::Package, &root, args, Some(guard), label)?;
    if !hooks.log.is_empty() {
        job.shared
            .with(|log| log.lines.splice(0..0, hooks.log).for_each(drop));
    }
    world.get_resource_or_insert_with(CliJobs::default).package = Some(job.clone());
    Ok(job)
}

/// Starts Tools ▸ Clear caches: `regia clean --cache` (the cook cache, the asset database, the
/// shader and pipeline caches; never sources or `assets/`) as job `clean:<n>`.
///
/// # Errors
/// `package_no_project`, `package_busy` or `package_spawn_failed`.
pub fn start_clear_caches(world: &mut World) -> Result<CliJob, JobError> {
    let root = project(world)?;
    check_idle(world)?;
    let args: Vec<OsString> = vec![
        "clean".into(),
        "--cache".into(),
        "--json".into(),
        "--project".into(),
        root.as_os_str().to_owned(),
    ];
    let job = start(
        world,
        CliKind::ClearCaches,
        &root,
        args,
        None,
        "clear caches".into(),
    )?;
    world.get_resource_or_insert_with(CliJobs::default).clean = Some(job.clone());
    Ok(job)
}

/// A package run's private staging folder inside its output folder (ADR-7496). The run writes
/// only there; [`Self::publish`] moves a finished package into the output folder and
/// [`Self::discard`] removes a cancelled, failed or dry run's writes, so the editor never deletes
/// an entry of the output folder that the run did not write.
#[derive(Debug, Clone)]
struct OutputGuard {
    /// The output folder (absolute).
    out: PathBuf,
    /// `<out>/.regia-package-<pid>-<n>`, the run's `--out`.
    staging: PathBuf,
    /// The outermost folder of `out`'s chain the editor created for this run (removed again,
    /// when empty, if the run publishes nothing).
    created: Option<PathBuf>,
    /// A dry run publishes nothing.
    dry_run: bool,
    /// The run's owner lock file, held (exclusively locked) until the staging folder is gone.
    owner: Arc<Mutex<Option<std::fs::File>>>,
    /// `<out>/.regia-package-<tag>.lock`.
    lock_path: PathBuf,
}

impl OutputGuard {
    /// Creates `out` (as needed) and a fresh staging folder in it.
    fn prepare(out: &Path, tag: &str, dry_run: bool) -> Result<Self, JobError> {
        let unwritable = |path: &Path, error: std::io::Error| {
            JobError::new(
                "package_out_unwritable",
                format!("cannot create {}: {error}", path.display()),
            )
        };
        let created = out
            .ancestors()
            .take_while(|dir| std::fs::symlink_metadata(dir).is_err())
            .last()
            .map(Path::to_path_buf);
        std::fs::create_dir_all(out).map_err(|e| unwritable(out, e))?;
        let staging = out.join(format!("{STAGING_PREFIX}{tag}"));
        // The owner lock first, then the folder: a sweep that sees the folder always sees a held
        // lock (ADR-7744).
        let lock_path = out.join(format!("{STAGING_PREFIX}{tag}{LOCK_SUFFIX}"));
        // A sweep may unlink the new lock file before this run locks it (it looks orphaned while
        // the folder does not exist yet): lock again until the file at the path is the locked one
        // (review finding S3). A sweep holds such a lock only to unlink it, so a few tries do.
        let mut owner = None;
        for _ in 0..LOCK_ATTEMPTS {
            owner = lock_owner_file(&lock_path, true);
            if owner.is_some() {
                break;
            }
            std::thread::yield_now();
        }
        let Some(owner) = owner else {
            return Err(unwritable(
                &lock_path,
                std::io::Error::other("cannot hold the owner lock"),
            ));
        };
        // A reused name's file may still name an old run's packaging process.
        let _ = owner.set_len(0);
        // `create_dir`, not `create_dir_all`: the folder is the run's own, never an existing one.
        if let Err(error) = std::fs::create_dir(&staging) {
            unlink_then_unlock(owner, &lock_path);
            return Err(unwritable(&staging, error));
        }
        Ok(Self {
            out: out.to_path_buf(),
            staging,
            created,
            dry_run,
            owner: Arc::new(Mutex::new(Some(owner))),
            lock_path,
        })
    }

    /// Removes the owner lock file and then lets go of its lock (the staging folder is gone or
    /// about to be).
    fn release(&self) {
        if let Some(lock) = self.owner.lock().unwrap_or_else(PoisonError::into_inner).take() {
            unlink_then_unlock(lock, &self.lock_path);
        }
    }

    /// Writes the packaging process's id into the owner lock file: that process outlives a
    /// crashed editor (and its lock) and still writes the staging folder, so the sweep keeps the
    /// folder while it runs (review finding S4).
    fn record_writer(&self, pid: u32) {
        use std::io::{Seek as _, Write as _};
        let mut owner = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(file) = owner.as_mut() else {
            return;
        };
        let written = file
            .set_len(0)
            .and_then(|()| file.seek(std::io::SeekFrom::Start(0)))
            .and_then(|_| file.write_all(pid.to_string().as_bytes()))
            .and_then(|()| file.flush());
        if let Err(error) = written {
            tracing::warn!(
                target: "regia_editor",
                "cannot record the packaging process in {}: {error}",
                self.lock_path.display()
            );
        }
    }

    /// Removes the staging folder and the empty folders the editor created for the run; answers
    /// the removed paths. Nothing else in the output folder is touched.
    fn discard(&self) -> Vec<String> {
        let mut removed = Vec::new();
        if std::fs::symlink_metadata(&self.staging).is_ok() {
            match std::fs::remove_dir_all(&self.staging) {
                Ok(()) => removed.push(self.staging.display().to_string()),
                Err(error) => tracing::warn!(
                    target: "regia_editor",
                    "cannot remove the package staging folder {}: {error}",
                    self.staging.display()
                ),
            }
        }
        self.release();
        if let Some(created) = &self.created {
            // `remove_dir` only removes an empty folder, so a file another program put there
            // keeps its folder.
            for dir in self.out.ancestors() {
                if std::fs::remove_dir(dir).is_err() {
                    break;
                }
                removed.push(dir.display().to_string());
                if dir == created {
                    break;
                }
            }
        }
        removed
    }

    /// Moves every entry of the finished run's staging folder into the output folder, each
    /// replacing the entry of the same name (as the CLI's own rebuild of `<out>/<slug>` and its
    /// archive rename do), then removes the staging folder. Answers the published paths.
    ///
    /// The `symbols/` directory is the exception the CLI also treats specially (rev-claude-63
    /// finding D): `regia package` merges into `<out>/symbols/` file by file and never deletes
    /// what else is parked there, so publish merges it too instead of replacing the whole
    /// directory.
    ///
    /// The moves happen one entry at a time, so a mid-publish failure is partial: the error names
    /// the entry that failed and lists what was already moved (finding F of rev-claude-63); the
    /// staging folder keeps the entries not moved yet.
    fn publish(&self) -> Result<Vec<String>, String> {
        let entries = std::fs::read_dir(&self.staging)
            .map_err(|e| format!("cannot read {}: {e}", self.staging.display()))?;
        // A sorted, deterministic order, so a partial failure's "already moved" list is stable.
        let mut entries: Vec<std::fs::DirEntry> = entries
            .map(|entry| entry.map_err(|e| format!("cannot read {}: {e}", self.staging.display())))
            .collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        let mut published = Vec::new();
        for entry in entries {
            let dest = self.out.join(entry.file_name());
            if entry.file_name() == "symbols" && entry.path().is_dir() && dest.is_dir() {
                Self::merge_dir(&entry.path(), &dest, &mut published)?;
                continue;
            }
            if let Ok(meta) = std::fs::symlink_metadata(&dest) {
                let removed = if meta.is_dir() {
                    std::fs::remove_dir_all(&dest)
                } else {
                    std::fs::remove_file(&dest)
                };
                removed.map_err(|e| publish_step_error("replace", &dest, &e, &published))?;
            }
            std::fs::rename(entry.path(), &dest)
                .map_err(|e| publish_step_error("move the package into", &dest, &e, &published))?;
            published.push(dest.display().to_string());
        }
        std::fs::remove_dir(&self.staging)
            .map_err(|e| format!("cannot remove {}: {e}", self.staging.display()))?;
        self.release();
        Ok(published)
    }

    /// Merges the staging `symbols/` tree into the output `symbols/` tree file by file (a parked
    /// file the run did not write survives, per ADR-7496). Answers through `published`.
    fn merge_dir(from: &Path, to: &Path, published: &mut Vec<String>) -> Result<(), String> {
        std::fs::create_dir_all(to).map_err(|e| publish_step_error("merge into", to, &e, published))?;
        let entries = std::fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))?;
        let mut entries: Vec<std::fs::DirEntry> = entries
            .map(|entry| entry.map_err(|e| format!("cannot read {}: {e}", from.display())))
            .collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let dest = to.join(entry.file_name());
            if dest.is_dir() && entry.path().is_dir() {
                Self::merge_dir(&entry.path(), &dest, published)?;
                continue;
            }
            if let Ok(meta) = std::fs::symlink_metadata(&dest) {
                let removed = if meta.is_dir() {
                    std::fs::remove_dir_all(&dest)
                } else {
                    std::fs::remove_file(&dest)
                };
                removed.map_err(|e| publish_step_error("replace", &dest, &e, published))?;
            }
            std::fs::rename(entry.path(), &dest)
                .map_err(|e| publish_step_error("move the package into", &dest, &e, published))?;
            published.push(dest.display().to_string());
        }
        // Every entry moved out: the emptied staging copy goes too, or the staging folder would
        // not be empty and its removal (the end of publish) would fail.
        std::fs::remove_dir(from).map_err(|e| publish_step_error("remove", from, &e, published))
    }

    /// `value` with every string that names a path in the staging folder renamed to the same
    /// path in the output folder (the `--json` report's `artifact`, `symbols`, `build_info`).
    fn rewrite(&self, value: &Value) -> Value {
        let from = self.staging.display().to_string();
        let to = self.out.display().to_string();
        match value {
            Value::String(text) => match text.strip_prefix(&from) {
                Some(rest) => Value::String(format!("{to}{rest}")),
                None => value.clone(),
            },
            Value::Array(items) => Value::Array(items.iter().map(|v| self.rewrite(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, v)| (key.clone(), self.rewrite(v)))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }
}

/// One publish step's error: names the failed path and, once something was moved, lists the
/// entries already moved into place (the staging folder keeps only the rest; rev-claude-63
/// finding F).
fn publish_step_error(action: &str, path: &Path, error: &std::io::Error, published: &[String]) -> String {
    let mut message = format!("cannot {action} {}: {error}", path.display());
    if !published.is_empty() {
        message.push_str("; already moved into place: ");
        message.push_str(&published.join(", "));
    }
    message
}

fn start(
    world: &mut World,
    kind: CliKind,
    root: &Path,
    args: Vec<OsString>,
    guard: Option<OutputGuard>,
    label: String,
) -> Result<CliJob, JobError> {
    let program = program(world);
    let mut command_line = vec![program.display().to_string()];
    command_line.extend(args.iter().map(|a| a.to_string_lossy().into_owned()));
    let (id, handle) = world
        .get_resource_or_insert_with::<JobRegistry>(Default::default)
        .start(kind.job_kind(), label.clone(), true);
    world
        .get_resource_or_insert_with::<crate::editor_jobs::EditorJobs>(Default::default)
        .register(&id, handle.clone());
    let log_path = regia_core::brand::project_state_dir(root)
        .join("logs")
        .join(format!("{}.log", id.replace(':', "-")));
    let token = CancelToken::new();
    let shared = Arc::new(Shared::default());
    let job = CliJob {
        id: id.clone(),
        kind,
        out: guard.as_ref().filter(|g| !g.dry_run).map(|g| g.out.clone()),
        staging: guard.as_ref().map(|g| g.staging.clone()),
        log_path: log_path.clone(),
        command: command_line,
        handle: handle.clone(),
        token: token.clone(),
        shared: Arc::clone(&shared),
    };
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a cancel stops cargo and the compilers it started too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = match command.spawn() {
        Ok(child) => {
            if let Some(guard) = &guard {
                guard.record_writer(child.id());
            }
            child
        }
        Err(error) => {
            let message = format!(
                "cannot run {}: {error} (build the CLI with `cargo build -p regia_cli`, or set {})",
                program.display(),
                crate::bake_runner::CLI_ENV
            );
            if let Some(guard) = &guard {
                guard.discard();
            }
            handle.fail("package_spawn_failed", message.clone());
            shared.with(|log| {
                log.outcome = Some(json!({ "ok": false, "code": "package_spawn_failed", "error": message }));
                log.finished = true;
            });
            return Err(JobError::new("package_spawn_failed", message));
        }
    };
    handle.progress(None, label.clone());
    let worker = Worker {
        kind,
        handle,
        token: token.clone(),
        shared,
        guard,
        log_path,
    };
    if let Err(error) = worker.spawn(child) {
        // The worker ended the job (and removed the staging folder); no task row was published.
        return Err(JobError::new(
            "package_spawn_failed",
            format!("cannot start the job's worker thread: {error}"),
        ));
    }
    // Published once the worker owns the run, so a run that never started leaves no Cancel row;
    // a run that already ended has its row removed by the next `sync_tasks`.
    world
        .get_resource_or_insert_with(BackgroundTasks::default)
        .publish(TaskRow {
            id: kind.task().to_string(),
            label,
            progress: None,
            cancel: token,
        });
    Ok(job)
}

struct Worker {
    kind: CliKind,
    handle: JobHandle,
    token: CancelToken,
    shared: Arc<Shared>,
    guard: Option<OutputGuard>,
    log_path: PathBuf,
}

impl Worker {
    fn spawn(self, mut child: Child) -> std::io::Result<()> {
        #[cfg(test)]
        if tests::FAIL_WORKER_SPAWN.with(std::cell::Cell::get) {
            kill_tree(&mut child);
            let _ = child.wait();
            self.end(Err("injected worker failure".into()), false, None);
            return Err(std::io::Error::other("injected worker failure"));
        }
        let (tx, rx) = mpsc::channel::<String>();
        let streams: [Option<Box<dyn Read + Send>>; 2] = [
            child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
            child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        ];
        for stream in streams.into_iter().flatten() {
            let tx = tx.clone();
            let spawned = std::thread::Builder::new()
                .name("regia-editor-cli-io".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stream);
                    while let Some(line) = read_line_lossy(&mut reader) {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                });
            if let Err(error) = spawned {
                kill_tree(&mut child);
                let _ = child.wait();
                self.end(Err(format!("cannot read the run's output: {error}")), false, None);
                return Err(error);
            }
        }
        drop(tx);
        let failed = self.handle.clone();
        let shared = Arc::clone(&self.shared);
        let guard = self.guard.clone();
        let spawned = std::thread::Builder::new()
            .name("regia-editor-cli".into())
            .spawn(move || self.supervise(child, &rx));
        if let Err(error) = &spawned {
            if let Some(guard) = &guard {
                guard.discard();
            }
            failed.fail(
                "package_spawn_failed",
                format!("cannot start the worker: {error}"),
            );
            shared.with(|log| log.finished = true);
        }
        spawned.map(|_| ())
    }

    fn cancel_requested(&self) -> bool {
        self.token.is_cancelled() || self.handle.cancel_requested()
    }

    fn supervise(self, mut child: Child, lines: &mpsc::Receiver<String>) {
        let mut killed = false;
        let mut grace = CANCEL_GRACE_POLLS;
        let mut all_lines: Vec<String> = Vec::new();
        loop {
            if !killed && self.cancel_requested() {
                kill_tree(&mut child);
                killed = true;
            }
            if killed {
                if grace == 0 {
                    break;
                }
                grace -= 1;
            }
            match lines.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    regia_core::log::ring::push("INFO", "regia_editor::package", line.clone());
                    if let Some(report) = parse_report(self.kind, &line) {
                        self.shared.with(|log| log.report = Some(report));
                    }
                    self.handle.progress(None, line.clone());
                    self.handle.event(json!({ "kind": "line", "line": line }));
                    self.shared.with(|log| {
                        log.message.clone_from(&line);
                        log.total_lines += 1;
                        if log.lines.len() == KEEP_LINES {
                            log.lines.remove(0);
                        }
                        log.lines.push(line.clone());
                    });
                    all_lines.push(line);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if !killed && self.cancel_requested() {
            // The cancel came as the output closed: the run may not have finished its writes.
            kill_tree(&mut child);
            killed = true;
        }
        let status = child.wait();
        self.write_log(&all_lines);
        let exit_code = status.as_ref().ok().and_then(std::process::ExitStatus::code);
        if let Err(error) = status {
            self.end(
                Err(format!("waiting for the run failed: {error}")),
                killed,
                exit_code,
            );
            return;
        }
        let ok = !killed && exit_code == Some(0);
        if ok {
            self.end(Ok(()), false, exit_code);
        } else if killed {
            self.end(Ok(()), true, exit_code);
        } else {
            let last = all_lines
                .iter()
                .rev()
                .find(|line| !line.trim().is_empty())
                .cloned()
                .unwrap_or_default();
            self.end(
                Err(format!(
                    "regia exited with {} ({last})",
                    exit_code.map_or_else(|| "a signal".to_string(), |c| c.to_string())
                )),
                false,
                exit_code,
            );
        }
    }

    fn write_log(&self, lines: &[String]) {
        let Some(dir) = self.log_path.parent() else {
            return;
        };
        let mut text = lines.join("\n");
        text.push('\n');
        let written = std::fs::create_dir_all(dir)
            .map_err(|e| e.to_string())
            .and_then(|()| crate::atomic_write::write_atomic(&self.log_path, text.as_bytes()));
        if let Err(error) = written {
            tracing::warn!(target: "regia_editor", "cannot write {}: {error}", self.log_path.display());
        }
    }

    /// Ends the job: `Ok` with `cancelled` false publishes the package and is done (a dry run
    /// discards its staging folder), `Ok` with `cancelled` true discards the staging folder and
    /// ends `cancelled`, `Err` discards it and fails.
    fn end(&self, result: Result<(), String>, cancelled: bool, exit_code: Option<i32>) {
        let log = self.log_path.display().to_string();
        let raw = self.shared.with(|log| log.report.clone());
        let report = match (&self.guard, raw) {
            (Some(guard), Some(report)) => Some(guard.rewrite(&report)),
            (_, report) => report,
        };
        let discard = || self.guard.as_ref().map(OutputGuard::discard).unwrap_or_default();
        let result = match result {
            Ok(()) if !cancelled => match self.guard.as_ref().filter(|g| !g.dry_run) {
                Some(guard) => guard.publish().map(Some).map_err(|error| {
                    format!(
                        "the run finished but its package could not be moved completely into \
                         place ({error}); the entries not moved yet stay in {}",
                        guard.staging.display()
                    )
                }),
                None => {
                    discard();
                    Ok(None)
                }
            },
            Ok(()) => Ok(None),
            Err(message) => {
                discard();
                Err(message)
            }
        };
        let (outcome, error) = match result {
            Ok(_) if cancelled => {
                let removed = discard();
                regia_core::log::ring::push(
                    "INFO",
                    "regia_editor::package",
                    format!("{} cancelled", self.kind.job_kind()),
                );
                (
                    json!({
                        "ok": false,
                        "code": self.kind.cancelled_code(),
                        "cancelled": true,
                        "exit_code": exit_code,
                        "removed": removed,
                        "log": log,
                    }),
                    None,
                )
            }
            Ok(published) => {
                let artifact = report
                    .as_ref()
                    .and_then(|r| r.get("artifact"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                (
                    json!({
                        "ok": true,
                        "exit_code": exit_code,
                        "artifact": artifact,
                        "out": self.guard.as_ref().filter(|g| !g.dry_run).map(|g| g.out.display().to_string()),
                        "published": published,
                        "report": report,
                        "log": log,
                    }),
                    None,
                )
            }
            Err(message) => (
                json!({
                    "ok": false,
                    "code": self.kind.failed_code(),
                    "error": message,
                    "exit_code": exit_code,
                    "report": report,
                    "log": log,
                }),
                Some(message),
            ),
        };
        // The outcome is in place before the job reads as ended.
        self.shared.with(|log| log.outcome = Some(outcome.clone()));
        match error {
            Some(message) => self.handle.fail(self.kind.failed_code(), message),
            None => self.handle.finish_as(outcome, cancelled),
        }
        self.shared.with(|log| log.finished = true);
    }
}

/// The `--json` result object of a `regia` run (spec 21 § 1): a whole line that parses as a JSON
/// object with an `ok` key (`regia package`, `json_result`) or, for `regia clean`, with a `paths`
/// key (its `render_json` object `{ dry_run, level, removed, paths, bytes }` has no `ok`).
fn parse_report(kind: CliKind, line: &str) -> Option<Value> {
    let trimmed = line.trim();
    if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
        return None;
    }
    let key = match kind {
        CliKind::Package => "ok",
        CliKind::ClearCaches => "paths",
    };
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .filter(|v| v.get(key).is_some() || v.get("ok").is_some())
}

/// Reads one output line: invalid UTF-8 is replaced, a line longer than `MAX_LINE_BYTES` keeps
/// its start. `None` at the end of the stream or on a read error.
fn read_line_lossy(reader: &mut impl BufRead) -> Option<String> {
    let mut line = Vec::new();
    loop {
        let (consumed, done) = {
            let chunk = match reader.fill_buf() {
                Ok(chunk) => chunk,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            };
            if chunk.is_empty() {
                if line.is_empty() {
                    return None;
                }
                break;
            }
            let (part, done) = match chunk.iter().position(|&b| b == b'\n') {
                Some(end) => (&chunk[..end], true),
                None => (chunk, false),
            };
            let room = MAX_LINE_BYTES.saturating_sub(line.len());
            line.extend_from_slice(&part[..part.len().min(room)]);
            (part.len() + usize::from(done), done)
        };
        reader.consume(consumed);
        if done {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(String::from_utf8_lossy(&line).into_owned())
}

/// Stops the run: its process group on Unix (it leads one), its process tree on Windows, then
/// the child itself.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let group = format!("-{}", child.id());
        let _ = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Removes finished runs' rows from [`BackgroundTasks`]. The shell calls it every frame and the
/// status method on every call; idempotent.
pub fn sync_tasks(world: &mut World) {
    let Some(jobs) = world.get_resource::<CliJobs>().cloned() else {
        return;
    };
    for job in [jobs.package, jobs.clean].into_iter().flatten() {
        if job.running() {
            continue;
        }
        if job.kind == CliKind::Package {
            after_package_hooks(world, &job);
        }
        // One run per task at a time (`check_idle`), so a finished newest run owns the row.
        if let Some(mut tasks) = world.get_resource_mut::<BackgroundTasks>()
            && tasks.rows().iter().any(|row| row.id == job.kind.task())
        {
            tasks.finish(job.kind.task());
        }
    }
}

/// Runs the extension after-package hooks once on a finished, successful package run's output
/// (ADR-7791): their lines join the job's log, and an abort turns the result into `ok: false,
/// code: package_hook_aborted`, which the dialog and `editor.package_status` show.
fn after_package_hooks(world: &World, job: &CliJob) {
    let ready = job.shared.with(|log| {
        let ok = log
            .outcome
            .as_ref()
            .and_then(|o| o.get("ok"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let run = ok && !log.hooks_ran;
        log.hooks_ran = true;
        run
    });
    let (Some(out), true) = (job.out.clone(), ready) else {
        return;
    };
    let Some(project) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
    else {
        return;
    };
    let target = job
        .command
        .iter()
        .position(|a| a == "--platform")
        .and_then(|i| job.command.get(i + 1))
        .cloned()
        .unwrap_or_default();
    let profile = job
        .command
        .iter()
        .position(|a| a == "--profile")
        .and_then(|i| job.command.get(i + 1))
        .cloned()
        .unwrap_or_default();
    let report = crate::extension::hooks::run_stage(
        world,
        &crate::extension::hooks::HookInput {
            stage: crate::extension::hooks::HookStage::AfterPackage,
            project,
            target,
            profile,
            out: Some(out),
        },
    );
    job.shared.with(|log| {
        log.lines.extend(report.log);
        if let Some(message) = report.aborted {
            log.lines.push(message.clone());
            if let Some(Value::Object(outcome)) = log.outcome.as_mut() {
                outcome.insert("ok".into(), Value::Bool(false));
                outcome.insert("code".into(), Value::String("package_hook_aborted".into()));
                outcome.insert("error".into(), Value::String(message));
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------
// The dialog

/// The File ▸ Package for platform window.
#[derive(Resource, Debug, Clone, Default)]
pub struct PackageDialog {
    /// Whether the window is open.
    pub open: bool,
    /// The request the fields edit.
    pub request: PackageRequest,
    /// The output folder field (empty: the default `<project>/dist/<target>`).
    pub out_text: String,
    /// `[profiles.<name>]` names read from `Regia.toml` when the dialog opened.
    pub extra_profiles: Vec<String>,
    /// The last refusal or result line.
    pub status: String,
}

impl PackageDialog {
    /// Opens the window and reads the project's profiles.
    pub fn open_for(&mut self, root: Option<&Path>) {
        self.open = true;
        self.extra_profiles = root
            .and_then(|root| std::fs::read_to_string(root.join("Regia.toml")).ok())
            .map(|text| extra_profiles(&text))
            .unwrap_or_default();
    }
}

/// Draws the Package for platform window (when open) and keeps the task rows in step. Called
/// every frame by the shell.
pub fn show(ctx: &egui::Context, world: &mut World) {
    sync_tasks(world);
    sweep_project_once(world);
    let Some(mut dialog) = world.get_resource::<PackageDialog>().cloned() else {
        return;
    };
    if !dialog.open {
        return;
    }
    let job = world
        .get_resource::<CliJobs>()
        .and_then(|jobs| jobs.package.clone());
    let root = crate::mesh_source::project_root(world);
    let mut open = dialog.open;
    let mut start = false;
    egui::Window::new("Package for platform")
        .open(&mut open)
        .resizable(true)
        .default_width(520.0)
        .show(ctx, |ui| {
            egui::Grid::new("package-dialog-grid")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Target");
                    ui.horizontal(|ui| {
                        for target in TARGETS {
                            // Choosing a field, not a state change of the editor.
                            ui.radio_value(&mut dialog.request.target, (*target).to_string(), *target);
                        }
                    });
                    ui.end_row();
                    ui.label("Profile");
                    egui::ComboBox::from_id_salt("package-profile")
                        .selected_text(dialog.request.profile.clone())
                        .show_ui(ui, |ui| {
                            let names: Vec<String> = PROFILES
                                .iter()
                                .map(|p| (*p).to_string())
                                .chain(dialog.extra_profiles.iter().cloned())
                                .collect();
                            for name in names {
                                ui.selectable_value(&mut dialog.request.profile, name.clone(), name);
                            }
                        });
                    ui.end_row();
                    ui.label("Output folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.out_text).hint_text("dist/<target> (default)"),
                    );
                    ui.end_row();
                    ui.label("Dry run");
                    ui.checkbox(&mut dialog.request.dry_run, "cook and report sizes only");
                    ui.end_row();
                });
            dialog.request.out = Some(dialog.out_text.trim())
                .filter(|text| !text.is_empty())
                .map(PathBuf::from);
            ui.separator();
            let running = job.as_ref().is_some_and(CliJob::running);
            ui.horizontal(|ui| {
                if crate::agent_parity::action_enabled(
                    ui,
                    !running && root.is_some(),
                    &crate::agent_parity::PACKAGE_START,
                    None,
                )
                .clicked()
                {
                    start = true;
                }
                if running && crate::tasks::cancel_button(ui, world, PACKAGE_TASK) {
                    dialog.status = "cancelling the package".into();
                }
            });
            if let Some(job) = &job {
                job_view(ui, job);
            }
            if !dialog.status.is_empty() {
                ui.label(&dialog.status);
            }
        });
    dialog.open = open;
    if start {
        dialog.status = match start_package(world, &dialog.request) {
            Ok(job) => format!("started {}", job.id),
            Err(error) => error.to_string(),
        };
    }
    world.insert_resource(dialog);
}

/// The job's progress line, result, log link and output tail.
fn job_view(ui: &mut egui::Ui, job: &CliJob) {
    let status = job.status_json();
    let state = status["state"].as_str().unwrap_or_default().to_string();
    ui.horizontal(|ui| {
        if state == "running" {
            ui.spinner();
        }
        ui.strong(format!("{} · {state}", job.id));
    });
    if let Some(result) = status.get("result").filter(|r| !r.is_null()) {
        let line = if result["ok"] == json!(true) {
            match result["artifact"].as_str() {
                Some(artifact) => format!("Packaged {artifact}"),
                None => "Done".to_string(),
            }
        } else {
            let code = result["code"].as_str().unwrap_or_default();
            let error = result["error"].as_str().unwrap_or_default();
            format!("{code} {error}")
        };
        ui.label(line);
        ui.horizontal(|ui| {
            if crate::agent_parity::action_button(ui, &crate::agent_parity::PACKAGE_OPEN_LOG).clicked()
                && let Err(error) = open_path(&job.log_path)
            {
                tracing::warn!(target: "regia_editor", "{error}");
            }
            if let Some(artifact) = result["artifact"].as_str()
                && crate::agent_parity::action_button(ui, &crate::agent_parity::PACKAGE_SHOW_OUTPUT).clicked()
            {
                let path = Path::new(artifact);
                let folder = if path.is_dir() {
                    path
                } else {
                    path.parent().unwrap_or(path)
                };
                if let Err(error) = open_path(folder) {
                    tracing::warn!(target: "regia_editor", "{error}");
                }
            }
        });
    } else {
        ui.weak(status["message"].as_str().unwrap_or_default());
    }
    egui::CollapsingHeader::new("Output")
        .id_salt("package-output")
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in status["lines"].as_array().into_iter().flatten() {
                        ui.monospace(line.as_str().unwrap_or_default());
                    }
                });
        });
}

/// Opens a file or folder in the OS (the log link, the output folder, the book).
///
/// # Errors
/// The path is missing or the OS opener could not start.
pub fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(not(target_os = "android"))]
    {
        regia_platform::open_path(path).map_err(|e| format!("cannot open {}: {e}", path.display()))
    }
    #[cfg(target_os = "android")]
    {
        Err(format!("cannot open {} on Android", path.display()))
    }
}

// ---------------------------------------------------------------------------------------------
// Agent methods

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(RpcError::wrong_type(key, "string")),
    }
}

/// `editor.package_start`.
fn package_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut request = PackageRequest::default();
    if let Some(target) = opt_str(params, "target")? {
        request.target = target.to_string();
    }
    if let Some(profile) = opt_str(params, "profile")? {
        request.profile = profile.to_string();
    }
    request.dry_run = match params.get("dry_run") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(on)) => *on,
        Some(_) => return Err(RpcError::wrong_type("dry_run", "boolean")),
    };
    // The project and the fields first, so a caller learns what is wrong before confirming.
    project(world).map_err(|e| e.rpc())?;
    request.validate().map_err(|e| e.rpc())?;
    // `out` is a request path (ADR-1816): under the project, the temp dir, the working directory
    // or an `output_roots` entry, without `..` or a symlink leading out.
    if let Some(out) = opt_str(params, "out")?.filter(|out| !out.is_empty()) {
        request.out = Some(crate::agent_ext::allowed_request_path(world, "out", out)?);
    }
    // Packaging writes a shippable game: like `agent.package` (spec 23 § 3.7), it needs the
    // caller's confirmation; a dry run writes nothing and needs none.
    if !request.dry_run && params.get("confirm") != Some(&Value::Bool(true)) {
        return Err(JobError::new(
            "needs_confirmation",
            "packaging builds and writes a shippable game into the output folder: repeat with \
             `confirm: true` once the owner agreed (a `dry_run` needs no confirmation)",
        )
        .rpc());
    }
    let job = start_package(world, &request).map_err(|e| e.rpc())?;
    Ok(json!({
        "job": job.id,
        "task": PACKAGE_TASK,
        "command": job.command,
        "out": job.out.as_ref().map(|p| p.display().to_string()),
        "staging": job.staging.as_ref().map(|p| p.display().to_string()),
        "log": job.log_path.display().to_string(),
    }))
}

/// `editor.package_status`.
fn package_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    sync_tasks(world);
    let jobs = world.get_resource::<CliJobs>().cloned().unwrap_or_default();
    let job = match opt_str(params, "job")? {
        Some(id) => [jobs.package, jobs.clean]
            .into_iter()
            .flatten()
            .find(|job| job.id == id)
            .ok_or_else(|| {
                JobError::new(
                    "package_not_found",
                    format!("the editor ran no package or clear-caches job {id}"),
                )
                .rpc()
            })?,
        None => match jobs.package {
            Some(job) => job,
            None => return Ok(json!({ "job": null, "state": "idle" })),
        },
    };
    Ok(job.status_json())
}

/// `editor.clear_caches`.
fn clear_caches(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if params.get("confirm") != Some(&Value::Bool(true)) {
        return Err(JobError::new(
            "needs_confirmation",
            "Clear caches deletes the cook cache, the asset database and the shader and pipeline caches \
             (the next cook and launch rebuild them): repeat with `confirm: true`",
        )
        .rpc());
    }
    let job = start_clear_caches(world).map_err(|e| e.rpc())?;
    Ok(json!({ "job": job.id, "task": CLEAN_TASK, "command": job.command }))
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.package_start",
                tool: "package_start",
                description: "File ▸ Package for platform: run `regia package` for the open project as a \
                    background editor job (package:<n>) and answer its id. Poll editor.package_status (or \
                    agent.job_status); stop it with editor.cancel_task { id: \"package\" } or \
                    editor.cancel_job { id }. The run writes into a private staging folder inside the \
                    output directory; only a finished run's package is moved into the output directory \
                    (replacing same-named entries), and a cancelled, failed or dry run removes the \
                    staging folder and nothing else. Writes a shippable game, so it needs confirm: \
                    true unless dry_run. Errors: package_no_project, package_bad_target, \
                    package_bad_profile, path_outside_project, needs_confirmation, package_busy, \
                    package_out_unwritable, package_spawn_failed.",
                params: &[
                    ParamSchema {
                        name: "target",
                        kind: "string",
                        required: false,
                        description: "linux, windows or android (default: the host's).",
                    },
                    ParamSchema {
                        name: "profile",
                        kind: "string",
                        required: false,
                        description: "dev (default), shipping or a [profiles.<name>] entry of Regia.toml.",
                    },
                    ParamSchema {
                        name: "out",
                        kind: "string",
                        required: false,
                        description: "Output directory: project-relative, or absolute under the project, the temp \
                            directory, the working directory or an output_roots entry (ADR-1816; default \
                            dist/<target>).",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "regia package --dry-run: cook if needed and report sizes; no build, no output.",
                    },
                    ParamSchema {
                        name: "confirm",
                        kind: "boolean",
                        required: false,
                        description: "Must be true unless dry_run (the dialog's Package button).",
                    },
                ],
                result: "{ job, task, command, out, staging, log }",
                mutating: true,
                permission: Permission::Build,
            },
            package_start as Handler,
        ),
        (
            MethodSchema {
                name: "editor.package_status",
                tool: "package_status",
                description: "The editor's newest package job (or the package or clear-caches job `job`): \
                    state (running, done, failed, cancelled), the last output line, the output tail, \
                    the log file and, once it ended, the result { ok, code?, artifact?, out?, \
                    published?, report?, removed?, exit_code, log }. A cancelled run's result code is package_cancelled; a \
                    failed run's package_failed. `state: idle` when none ran.",
                params: &[ParamSchema {
                    name: "job",
                    kind: "string",
                    required: false,
                    description: "A job id (package:<n>, clean:<n>); default the newest package job.",
                }],
                result: "{ job, task, state, message, lines, line_count, out, staging, log, command, result } | { job: null, state: idle }",
                mutating: false,
                permission: Permission::Read,
            },
            package_status as Handler,
        ),
        (
            MethodSchema {
                name: "editor.clear_caches",
                tool: "clear_caches",
                description: "Tools ▸ Clear caches: run `regia clean --cache` for the open project (the cook \
                    cache, the asset database, the shader and pipeline caches; never sources or assets/) \
                    as job clean:<n>. Destructive: needs confirm: true (needs_confirmation otherwise). Poll \
                    editor.package_status { job }.",
                params: &[ParamSchema {
                    name: "confirm",
                    kind: "boolean",
                    required: true,
                    description: "Must be true (the confirm dialog's Clear).",
                }],
                result: "{ job, task, command }",
                mutating: true,
                permission: Permission::Build,
            },
            clear_caches as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Makes `Worker::spawn` fail as if the OS refused a thread.
        pub(super) static FAIL_WORKER_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// How many owner lock files a "concurrent sweep" unlinks right after they are opened and
        /// before they are locked (review finding S3).
        static UNLINK_BEFORE_LOCK: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        /// A tag a "new run" prepares in the output folder of the released lock, between its
        /// release's two steps (review finding S3); the run's guard lands in `RACED_RUN`.
        static PREPARE_ON_RELEASE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
        static RACED_RUN: std::cell::RefCell<Option<OutputGuard>> = const { std::cell::RefCell::new(None) };
    }

    /// Test hook of `lock_owner_file`: a sweep that unlinks the file in the open-to-lock gap.
    pub(super) fn between_open_and_lock(path: &Path) {
        UNLINK_BEFORE_LOCK.with(|left| {
            if left.get() > 0 {
                left.set(left.get() - 1);
                std::fs::remove_file(path).expect("the racing sweep unlinks the lock file");
            }
        });
    }

    /// Test hook of `unlink_then_unlock`: a new run takes the same name between the two steps.
    pub(super) fn between_unlink_and_unlock(path: &Path) {
        let Some(tag) = PREPARE_ON_RELEASE.with(|tag| tag.borrow_mut().take()) else {
            return;
        };
        let out = path.parent().expect("an output folder");
        // The old run's folder is already gone; the new run reuses its name.
        let guard = OutputGuard::prepare(out, &tag, false).expect("the new run prepares");
        RACED_RUN.with(|run| *run.borrow_mut() = Some(guard));
    }

    /// Review w8 finding 9: when the job's worker cannot start, the `package` task row (the
    /// Cancel button) is not left behind for a run that never happens.
    #[cfg(unix)]
    #[test]
    fn a_worker_that_cannot_start_leaves_no_task_row() {
        let root = regia_test_utils::unique_temp_dir("package_worker_fail");
        std::fs::write(root.join("Regia.toml"), "[project]\nname = \"w\"\n").expect("manifest");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(PathBuf::from(
            "/bin/true",
        )));
        let request = PackageRequest {
            target: "linux".into(),
            dry_run: true,
            ..PackageRequest::default()
        };
        FAIL_WORKER_SPAWN.with(|fail| fail.set(true));
        let result = start_package(&mut world, &request);
        FAIL_WORKER_SPAWN.with(|fail| fail.set(false));
        assert_eq!(
            result.map(|job| job.id).map_err(|e| e.reason),
            Err("package_spawn_failed")
        );
        sync_tasks(&mut world);
        assert!(
            world
                .get_resource::<BackgroundTasks>()
                .is_none_or(|tasks| tasks.rows().iter().all(|row| row.id != PACKAGE_TASK)),
            "no Cancel row for a run that never started"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn package_args_match_the_cli_flags() {
        let root = Path::new("/p");
        let request = PackageRequest {
            target: "windows".into(),
            profile: "demo".into(),
            out: Some(PathBuf::from("out/win")),
            dry_run: true,
        };
        let args: Vec<String> = request
            .args(root)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "package",
                "--platform",
                "windows",
                "--profile",
                "demo",
                "--out",
                "/p/out/win",
                "--project",
                "/p",
                "--json",
                "--dry-run"
            ]
        );
        let default = PackageRequest {
            target: "android".into(),
            ..PackageRequest::default()
        };
        assert_eq!(default.out_dir(root), Path::new("/p/dist/android"));
    }

    #[test]
    fn bad_targets_and_profiles_are_refused() {
        let bad = PackageRequest {
            target: "macos".into(),
            ..PackageRequest::default()
        };
        assert_eq!(bad.validate().map_err(|e| e.reason), Err("package_bad_target"));
        for profile in ["", "--release", "Demo", "a b"] {
            let bad = PackageRequest {
                profile: profile.into(),
                ..PackageRequest::default()
            };
            assert_eq!(
                bad.validate().map_err(|e| e.reason),
                Err("package_bad_profile"),
                "{profile:?}"
            );
        }
    }

    #[test]
    fn extra_profiles_are_read_from_the_manifest() {
        let manifest = "[project]\nname = \"x\"\n[profiles.demo]\nlevels = []\n[profiles.dev]\n[profiles.\"qa\"] # x\n[profiles.demo]\n";
        assert_eq!(extra_profiles(manifest), ["demo", "qa"]);
    }

    /// rev-claude-63 finding D: `regia package` merges into `<out>/symbols/` (it writes one file
    /// and never deletes what else is parked there, e.g. an earlier build's renamed `.debug`), so
    /// the editor's publish must merge the staging `symbols/` file by file, not replace the whole
    /// directory. Other same-named entries keep the CLI's whole-entry replacement.
    #[test]
    fn publish_merges_the_symbols_directory_file_by_file() {
        let dir = regia_test_utils::unique_temp_dir("package_symbols_merge");
        std::fs::create_dir_all(dir.join("symbols")).expect("symbols");
        std::fs::write(dir.join("symbols/Old.debug"), b"parked").expect("parked");
        let guard = OutputGuard::prepare(&dir, "t1", false).expect("prepare");
        std::fs::create_dir_all(guard.staging.join("symbols")).expect("staging symbols");
        std::fs::write(guard.staging.join("symbols/Game.debug"), b"new").expect("new");
        std::fs::write(guard.staging.join("symbols/notes.txt"), b"n").expect("notes");
        guard.publish().expect("publish");
        assert_eq!(
            std::fs::read(dir.join("symbols/Old.debug")).expect("parked survives"),
            b"parked",
            "a file parked in <out>/symbols/ by another run is not deleted"
        );
        assert_eq!(
            std::fs::read(dir.join("symbols/Game.debug")).expect("merged"),
            b"new"
        );
        assert_eq!(std::fs::read(dir.join("symbols/notes.txt")).expect("added"), b"n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review rev63-followups-verify (finding D): merging `symbols/` empties the staging copy but
    /// leaves its directory, so publish must remove it too: otherwise the staging folder is not
    /// empty, publish fails after moving everything, and every re-package into an output folder
    /// that already has `symbols/` reports a failure and leaves `.regia-package-*` behind.
    #[test]
    fn publish_over_an_existing_symbols_directory_succeeds_and_leaves_no_staging() {
        let dir = regia_test_utils::unique_temp_dir("package_symbols_republish");
        std::fs::create_dir_all(dir.join("symbols/nested")).expect("symbols");
        std::fs::write(dir.join("symbols/Old.debug"), b"parked").expect("parked");
        let guard = OutputGuard::prepare(&dir, "t-republish", false).expect("prepare");
        std::fs::create_dir_all(guard.staging.join("symbols/nested")).expect("staging symbols");
        std::fs::write(guard.staging.join("symbols/Game.debug"), b"new").expect("new");
        std::fs::write(guard.staging.join("symbols/nested/Game.map"), b"map").expect("nested");
        std::fs::write(guard.staging.join("Game.tar.zst"), b"archive").expect("archive");
        let published = guard.publish().expect("publish over an existing symbols/");
        assert!(!guard.staging.exists(), "the staging folder is removed");
        assert_eq!(
            std::fs::read(dir.join("symbols/Old.debug")).expect("parked"),
            b"parked"
        );
        assert_eq!(
            std::fs::read(dir.join("symbols/nested/Game.map")).expect("nested"),
            b"map"
        );
        assert_eq!(
            std::fs::read(dir.join("Game.tar.zst")).expect("archive"),
            b"archive"
        );
        assert!(
            published.iter().any(|p| p.ends_with("Game.debug")),
            "{published:?}"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("out")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
            .map(|e| e.file_name())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-7744: a crash mid-package leaves its `.regia-package-*` folder; the sweep removes the
    /// stale ones and never the folder a live run owns.
    #[test]
    fn the_sweep_removes_a_stale_staging_folder_and_keeps_a_live_one() {
        let dir = regia_test_utils::unique_temp_dir("package_sweep");
        // A crashed run: its folder and its (unheld) owner lock file.
        let stale = dir.join(format!("{STAGING_PREFIX}9999-0"));
        std::fs::create_dir_all(stale.join("game")).expect("stale");
        std::fs::write(stale.join("game/file"), b"partial").expect("partial");
        std::fs::write(dir.join(format!("{STAGING_PREFIX}9999-0{LOCK_SUFFIX}")), b"").expect("stale lock");
        // A live run: prepared by this process, so its lock is held.
        let live = OutputGuard::prepare(&dir, "live-1", false).expect("prepare");
        std::fs::write(live.staging.join("partial"), b"in progress").expect("live file");
        // Not staging folders: a package, a plain file, a folder that merely resembles one.
        std::fs::create_dir_all(dir.join("Game")).expect("game");
        std::fs::write(dir.join("notes.txt"), b"keep").expect("notes");
        std::fs::create_dir_all(dir.join("regia-package-x")).expect("lookalike");
        // A lock file whose run and folder are gone.
        std::fs::write(dir.join(format!("{STAGING_PREFIX}gone{LOCK_SUFFIX}")), b"").expect("orphan");

        // A folder with no lock file, named for a process that cannot exist (no pid is that
        // large on Linux; Windows pids are multiples of four). At the epoch it is not old yet.
        let legacy = dir.join(format!("{STAGING_PREFIX}{DEAD_PID}-0"));
        std::fs::create_dir_all(&legacy).expect("legacy");
        let removed = sweep_stale_staging(&dir, std::time::SystemTime::UNIX_EPOCH);
        assert_eq!(
            removed,
            vec![stale.clone()],
            "only the crashed run's folder goes: {removed:?}"
        );
        assert!(!stale.exists());
        assert!(
            !dir.join(format!("{STAGING_PREFIX}9999-0{LOCK_SUFFIX}")).exists(),
            "its lock file too"
        );
        assert!(
            !dir.join(format!("{STAGING_PREFIX}gone{LOCK_SUFFIX}")).exists(),
            "the orphan lock too"
        );
        assert!(
            live.staging.join("partial").is_file(),
            "the live run's folder stays"
        );
        assert!(
            legacy.is_dir(),
            "a folder without a lock file, not yet old, stays"
        );
        assert!(
            dir.join("Game").is_dir()
                && dir.join("notes.txt").is_file()
                && dir.join("regia-package-x").is_dir()
        );

        // Two days on: the lock-less folder of a gone process goes (where the platform can tell
        // that it is gone), the live one still stays.
        let later = std::time::SystemTime::now() + 2 * LOCKLESS_MIN_AGE;
        let removed = sweep_stale_staging(&dir, later);
        if process_alive(DEAD_PID) == Some(false) {
            assert_eq!(removed, vec![legacy.clone()], "{removed:?}");
        } else {
            assert!(removed.is_empty() && legacy.is_dir(), "{removed:?}");
            std::fs::remove_dir_all(&legacy).expect("legacy");
        }
        assert!(live.staging.is_dir());

        // The run ends: its folder and lock file go, and nothing is left to sweep.
        live.discard();
        assert!(!live.staging.exists() && !live.lock_path.exists());
        assert!(sweep_stale_staging(&dir, later).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A published run leaves neither its staging folder nor its lock file behind.
    #[test]
    fn publish_releases_the_owner_lock() {
        let dir = regia_test_utils::unique_temp_dir("package_sweep_publish");
        let guard = OutputGuard::prepare(&dir, "pub-1", false).expect("prepare");
        std::fs::create_dir_all(guard.staging.join("Game")).expect("game");
        guard.publish().expect("publish");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("out")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["Game".to_owned()], "{names:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A process id no process can have: above Linux's `pid_max` limit (2^22) and not a multiple
    /// of four (every Windows pid is).
    const DEAD_PID: u32 = 999_999_999;

    /// Review finding S1 (ADR-7744): a staging folder with no lock file is never removed while
    /// the editor its name carries runs, however old it looks (an older editor that writes no lock
    /// file, or a run whose lock file was lost). A name with no process id stays too.
    #[test]
    fn a_lockless_folder_of_a_running_editor_survives_the_sweep() {
        let dir = regia_test_utils::unique_temp_dir("package_sweep_lockless");
        let mine = dir.join(format!("{STAGING_PREFIX}{}-0", std::process::id()));
        std::fs::create_dir_all(mine.join("Game")).expect("live folder");
        let nameless = dir.join(format!("{STAGING_PREFIX}legacy"));
        std::fs::create_dir_all(&nameless).expect("nameless");
        let far = std::time::SystemTime::now() + 2 * LOCKLESS_MIN_AGE;
        let removed = sweep_stale_staging(&dir, far);
        assert!(removed.is_empty(), "{removed:?}");
        assert!(mine.join("Game").is_dir(), "the running editor's folder stays");
        assert!(nameless.is_dir(), "a folder whose owner is unknown stays");

        // A gone owner: kept while young, removed once old (where the platform can tell).
        let gone = dir.join(format!("{STAGING_PREFIX}{DEAD_PID}-3"));
        std::fs::create_dir_all(&gone).expect("gone");
        assert!(sweep_stale_staging(&dir, std::time::SystemTime::now()).is_empty());
        assert!(
            gone.is_dir(),
            "a gone owner's folder younger than the minimum age stays"
        );
        let removed = sweep_stale_staging(&dir, far);
        if process_alive(DEAD_PID) == Some(false) {
            assert_eq!(removed, vec![gone.clone()]);
        } else {
            assert!(removed.is_empty());
        }
        assert!(mine.is_dir() && nameless.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review finding S4: after an editor crash its `regia package` child still writes the
    /// staging folder but holds no lock. The lock file names that process, and the sweep keeps
    /// the folder while it runs.
    #[test]
    fn a_folder_whose_packaging_process_still_runs_survives_the_sweep() {
        let dir = regia_test_utils::unique_temp_dir("package_sweep_writer");
        let orphaned = dir.join(format!("{STAGING_PREFIX}{DEAD_PID}-1"));
        std::fs::create_dir_all(orphaned.join("Game")).expect("folder");
        // The crashed editor's lock file (nobody holds it) names a running process.
        std::fs::write(
            dir.join(format!("{STAGING_PREFIX}{DEAD_PID}-1{LOCK_SUFFIX}")),
            std::process::id().to_string(),
        )
        .expect("lock file");
        let removed = sweep_stale_staging(&dir, std::time::SystemTime::now());
        assert!(removed.is_empty(), "{removed:?}");
        assert!(orphaned.join("Game").is_dir(), "the writer's folder stays");

        // A prepared run records its writer the same way.
        let guard = OutputGuard::prepare(&dir, "rec-2", false).expect("prepare");
        guard.record_writer(std::process::id());
        let mut lock = std::fs::File::open(&guard.lock_path).expect("lock file");
        drop(guard.owner.lock().expect("owner").take());
        assert_eq!(recorded_writer(&mut lock), Some(std::process::id()));
        // Its lock is gone (as after a crash), but the writer runs: the folder stays.
        assert!(sweep_stale_staging(&dir, std::time::SystemTime::now()).is_empty());
        assert!(guard.staging.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review finding S3 (1): a concurrent sweep unlinks the new lock file between `prepare`'s
    /// open and lock (it looks orphaned: the folder does not exist yet). The run must not end up
    /// holding a lock on an unlinked file, which would leave its folder unprotected.
    #[test]
    fn prepare_holds_the_lock_file_at_its_path_after_a_racing_unlink() {
        let dir = regia_test_utils::unique_temp_dir("package_lock_race_open");
        UNLINK_BEFORE_LOCK.with(|left| left.set(1));
        let guard = OutputGuard::prepare(&dir, "race-1", false).expect("prepare");
        UNLINK_BEFORE_LOCK.with(|left| left.set(0));
        assert!(
            guard.lock_path.is_file(),
            "the run's lock file is on disk: {}",
            guard.lock_path.display()
        );
        // And it is the locked one: no one else can take it, so a sweep keeps the folder.
        assert!(lock_owner_file(&guard.lock_path, false).is_none());
        assert!(sweep_stale_staging(&dir, std::time::SystemTime::now()).is_empty());
        assert!(guard.staging.is_dir());
        guard.discard();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review finding S3 (2): a run that takes a released run's name between the release's two
    /// steps keeps its own lock file (the release unlinks before it unlocks).
    #[test]
    fn a_release_never_removes_the_next_runs_lock_file() {
        let dir = regia_test_utils::unique_temp_dir("package_lock_race_release");
        let old = OutputGuard::prepare(&dir, "same-1", false).expect("prepare");
        std::fs::remove_dir(&old.staging).expect("the old run's folder goes first");
        PREPARE_ON_RELEASE.with(|tag| *tag.borrow_mut() = Some("same-1".into()));
        old.release();
        let new = RACED_RUN
            .with(|run| run.borrow_mut().take())
            .expect("the new run prepared during the release");
        assert!(
            new.lock_path.is_file(),
            "the new run's lock file survives the old run's release"
        );
        assert!(lock_owner_file(&new.lock_path, false).is_none(), "and is held");
        assert!(sweep_stale_staging(&dir, std::time::SystemTime::now()).is_empty());
        assert!(new.staging.is_dir());
        new.discard();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review rev63-followups-verify (finding F, through `publish` itself): an entry that cannot
    /// be replaced stops publish after the entries before it moved; the error lists those, and
    /// the staging folder keeps the entry that did not move.
    #[cfg(unix)]
    #[test]
    fn a_publish_that_fails_midway_names_what_it_already_moved() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = regia_test_utils::unique_temp_dir("package_publish_partial");
        // `<out>/b` cannot be removed: its subfolder is read-only, so its file cannot be deleted.
        std::fs::create_dir_all(dir.join("b/locked")).expect("b");
        std::fs::write(dir.join("b/locked/f"), b"old").expect("old b");
        let locked = dir.join("b/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        if std::fs::write(locked.join("probe"), b"").is_ok() {
            // Running as root: permissions do not stop the removal, so the scenario cannot be set up.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        let guard = OutputGuard::prepare(&dir, "t-partial", false).expect("prepare");
        std::fs::write(guard.staging.join("a"), b"new a").expect("a");
        std::fs::create_dir_all(guard.staging.join("b")).expect("staging b");
        std::fs::write(guard.staging.join("b/x"), b"new b").expect("b/x");
        let result = guard.publish();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
        let error = result.expect_err("b cannot be replaced");
        let moved_a = dir.join("a").display().to_string();
        assert!(
            error.contains(&format!("already moved into place: {moved_a}")),
            "the error lists the entry already moved: {error}"
        );
        assert_eq!(std::fs::read(dir.join("a")).expect("a moved"), b"new a");
        assert!(
            guard.staging.join("b/x").is_file(),
            "the entry not moved stays in staging"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// rev-claude-63 finding F: `publish` moves entry after entry, so a mid-publish failure must
    /// say what was already moved: the error names the failed path and lists the entries that
    /// were already published (the staging folder keeps only the rest).
    #[test]
    fn a_publish_step_error_lists_what_was_already_moved() {
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let moved = ["/out/Game".to_string()];
        let message = publish_step_error("replace", Path::new("/out/symbols"), &error, &moved);
        assert!(message.contains("/out/symbols"), "{message}");
        assert!(message.contains("denied"), "{message}");
        assert!(
            message.contains("/out/Game"),
            "the already-moved entries are listed: {message}"
        );
        let none_moved = publish_step_error("move the package into", Path::new("/out/b"), &error, &[]);
        assert!(!none_moved.contains("already moved"), "{none_moved}");
    }

    #[test]
    fn the_report_line_is_the_json_object_with_ok() {
        assert!(parse_report(CliKind::Package, r#"{"ok":true,"artifact":"/x"}"#).is_some());
        assert!(parse_report(CliKind::Package, r#"{"progress":1}"#).is_none());
        assert!(parse_report(CliKind::Package, "cooking 3/9").is_none());
        let clean = r#"{"dry_run":false,"level":"cache","removed":true,"paths":[],"bytes":0}"#;
        assert!(parse_report(CliKind::ClearCaches, clean).is_some());
        assert!(parse_report(CliKind::Package, clean).is_none());
    }

    #[test]
    fn the_output_guard_publishes_into_and_discards_only_its_staging_folder() {
        let dir = regia_test_utils::unique_temp_dir("package_guard");
        std::fs::write(dir.join("old.txt"), b"keep").expect("old");
        std::fs::create_dir_all(dir.join("Game")).expect("previous");
        std::fs::write(dir.join("Game/stale"), b"previous").expect("stale");
        let guard = OutputGuard::prepare(&dir, "t1", false).expect("prepare");
        assert!(guard.created.is_none(), "the output folder existed");
        std::fs::create_dir_all(guard.staging.join("Game/content")).expect("staging");
        std::fs::write(guard.staging.join("Game-linux.tar.gz"), b"new").expect("archive");
        let report = json!({ "artifact": guard.staging.join("Game-linux.tar.gz").display().to_string() });
        assert_eq!(
            guard.rewrite(&report)["artifact"],
            json!(dir.join("Game-linux.tar.gz").display().to_string())
        );
        let published = guard.publish().expect("publish");
        assert_eq!(published.len(), 2, "{published:?}");
        assert!(dir.join("Game/content").is_dir() && !dir.join("Game/stale").exists());
        assert!(dir.join("old.txt").is_file() && !guard.staging.exists());

        // A discarded run removes its staging folder and nothing else.
        let guard = OutputGuard::prepare(&dir, "t2", false).expect("prepare");
        std::fs::write(guard.staging.join("partial"), b"x").expect("partial");
        std::fs::write(dir.join("foreign"), b"another program").expect("foreign");
        assert_eq!(guard.discard(), [guard.staging.display().to_string()]);
        assert!(dir.join("foreign").is_file() && dir.join("Game/content").is_dir());

        // Folders the editor created go again when empty, and stay when not.
        let fresh = dir.join("a/b");
        let guard = OutputGuard::prepare(&fresh, "t3", true).expect("prepare");
        assert_eq!(guard.created.as_deref(), Some(dir.join("a").as_path()));
        guard.discard();
        assert!(!dir.join("a").exists(), "the created chain is gone");
        let guard = OutputGuard::prepare(&fresh, "t4", true).expect("prepare");
        std::fs::write(dir.join("a/other"), b"x").expect("other");
        guard.discard();
        assert!(!fresh.exists() && dir.join("a/other").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }
}
