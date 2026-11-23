// SPDX-License-Identifier: MPL-2.0
//! The Bake runner panel: runs `regia bake-gi` (`book/agents/baked-gi-bake.md`) on one of the
//! project's `.bake.ron` descriptions or a built-in scene, with progress, cancel and the
//! summary (`docs/specs/19-editor.md` § 2: long operations run in the background with progress
//! and cancel). Agent surface: `editor.bake_gi`, `editor.bake_status`, `editor.bake_gi_cancel`
//! (`book/agents/editor-bake.md`).
//!
//! The bake runs in a child process (ADR-866): the path tracer gets its own GPU device, the
//! editor's frame never waits on it, and a crash or a device loss in the bake cannot take the
//! editor down. The CLI's contract is what the panel reads:
//!
//! - stderr carries progress lines, `"  <stage>: <done>/<total> probes"` ([`parse_progress`]);
//! - stdout carries one JSON summary at exit, `{"ok": true, ...}` or
//!   `{"ok": false, "code", "path", "message"}` ([`parse_outcome`]).
//!
//! Two threads read the child's pipes line by line into a channel; [`BakeJob::poll`] drains it
//! without blocking. The panel and the agent methods share the [`BakePanel`] world resource, so
//! an agent sees a bake a user started and the other way round.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::{Handler, str_param};

/// The CLI's built-in bake scenes. The editor does not link the path tracer, so this repeats
/// `regia_pt::bake_desc::BUILTIN_SCENES`.
pub const BUILTIN_SCENES: [&str; 3] = ["cornell", "furnace", "rolling-hills"];

/// The environment variable naming the `regia` CLI the panel runs.
pub const CLI_ENV: &str = "REGIA_CLI";

/// Lines of the bake's stderr kept for the log and failure messages.
pub const LOG_LINES: usize = 40;

/// Most stdout bytes kept (the summary of a large bake lists every brick).
const MAX_STDOUT_BYTES: usize = 16 * 1024 * 1024;

/// Most directory entries visited when listing a project's bake descriptions.
const MAX_SCAN_ENTRIES: usize = 50_000;

/// The failure code when the bake exits without printing a summary (no GPU, a panic).
pub const NO_SUMMARY: &str = "bake_no_summary";

/// The failure code when the CLI cannot be started.
pub const SPAWN_FAILED: &str = "bake_spawn_failed";

/// Overrides of the description's trace settings (the `regia bake-gi` flags).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BakeOverrides {
    /// `--rays`: rays per probe per sample.
    pub rays: Option<u32>,
    /// `--samples`: samples per probe.
    pub samples: Option<u32>,
    /// `--bounces`: maximum path vertices.
    pub bounces: Option<u32>,
    /// `--seed`.
    pub seed: Option<u32>,
    /// `--probes-per-batch`: probes per GPU batch.
    pub probes_per_batch: Option<u32>,
    /// `--no-filter`: skip the 3×3×3 visibility-weighted filter.
    pub no_filter: bool,
}

/// One bake to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakeRequest {
    /// A built-in scene name or the path of a `.bake.ron`.
    pub scene: String,
    /// The output volume (`None`: the CLI's default, `<stem>.fgiv` in its working directory).
    pub out: Option<PathBuf>,
    /// Trace-setting overrides.
    pub overrides: BakeOverrides,
    /// `--dry-run`: validate and print the layout, no tracing and no GPU.
    pub dry_run: bool,
}

impl BakeRequest {
    /// A request for `scene` with no overrides.
    #[must_use]
    pub fn new(scene: impl Into<String>) -> Self {
        Self {
            scene: scene.into(),
            out: None,
            overrides: BakeOverrides::default(),
            dry_run: false,
        }
    }

    /// The `regia` arguments: `bake-gi <scene> [--out <path>] [--rays <n>] [--samples <n>]
    /// [--bounces <n>] [--seed <n>] [--probes-per-batch <n>] [--no-filter] [--dry-run]`.
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["bake-gi".into(), self.scene.clone().into()];
        if let Some(out) = &self.out {
            args.push("--out".into());
            args.push(out.clone().into_os_string());
        }
        let o = &self.overrides;
        let numbers = [
            ("--rays", o.rays),
            ("--samples", o.samples),
            ("--bounces", o.bounces),
            ("--seed", o.seed),
            ("--probes-per-batch", o.probes_per_batch),
        ];
        for (flag, value) in numbers
            .into_iter()
            .filter_map(|(flag, value)| value.map(|value| (flag, value)))
        {
            args.push(flag.into());
            args.push(value.to_string().into());
        }
        if o.no_filter {
            args.push("--no-filter".into());
        }
        if self.dry_run {
            args.push("--dry-run".into());
        }
        args
    }
}

/// The `regia` CLI to run: `$REGIA_CLI`, else `regia` next to the editor executable,
/// else `regia` on `PATH`.
#[must_use]
pub fn cli_program() -> PathBuf {
    if let Some(path) = regia_core::brand::env_var_os(CLI_ENV).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let name = format!("regia{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(&name);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(name)
}

/// A built-in scene stays a name; a relative description path is resolved against the project.
#[must_use]
pub fn resolve_scene(scene: &str, project: Option<&Path>) -> String {
    if BUILTIN_SCENES.contains(&scene) {
        return scene.to_string();
    }
    let path = Path::new(scene);
    match project {
        Some(root) if path.is_relative() => root.join(path).display().to_string(),
        _ => scene.to_string(),
    }
}

/// Where the panel writes a bake by default: `<dir>/<stem>.fgiv` beside a description
/// (`maps/bore.bake.ron` → `maps/bore.fgiv`), `<project>/.regia/bake/<name>.fgiv` for a
/// built-in. `None` for a built-in without a project (the CLI's default then applies).
#[must_use]
pub fn default_out(scene: &str, project: Option<&Path>) -> Option<PathBuf> {
    if BUILTIN_SCENES.contains(&scene) {
        return project.map(|root| {
            regia_core::brand::project_state_dir(root)
                .join("bake")
                .join(format!("{scene}.fgiv"))
        });
    }
    let path = Path::new(scene);
    let stem = path.file_stem()?.to_str()?.trim_end_matches(".bake");
    if stem.is_empty() {
        return None;
    }
    Some(path.with_file_name(format!("{stem}.fgiv")))
}

/// The project's bake descriptions (`*.bake.ron`), project-relative with `/` separators,
/// sorted. Hidden directories (`.regia`, `.git`) and `target` are skipped; the walk stops
/// after a fixed number of entries so a huge tree cannot stall the panel.
#[must_use]
pub fn find_bake_descriptions(root: &Path) -> Vec<String> {
    find_with_suffix(root, ".bake.ron")
}

/// The project's files named `*<suffix>`, walked as [`find_bake_descriptions`] walks.
#[must_use]
pub fn find_with_suffix(root: &Path, suffix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            visited += 1;
            if visited > MAX_SCAN_ENTRIES {
                out.sort();
                return out;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(entry.path());
                }
                continue;
            }
            if file_type.is_file() && name.ends_with(suffix) {
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(&path);
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

/// One progress line of the bake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakeProgress {
    /// The bake stage (`trace`, `filter`, ...).
    pub stage: String,
    /// Probes done in the stage.
    pub done: u64,
    /// Probes in the stage.
    pub total: u64,
    /// Seconds the rest takes at the rate so far, when the line said (a checkpointed bake's
    /// ` (N left, eta S s)`, whose rate counts the bake's earlier runs; ADR-3095).
    pub eta_seconds: Option<u64>,
}

impl BakeProgress {
    /// `done / total` in `0..=1` (0 for an empty stage).
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done.min(self.total) as f64 / self.total as f64) as f32
    }

    /// Units still to do in the stage.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.total.saturating_sub(self.done)
    }

    /// `{stage, done, total, fraction, remaining, eta_seconds}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "stage": self.stage,
            "done": self.done,
            "total": self.total,
            "fraction": self.fraction(),
            "remaining": self.remaining(),
            "eta_seconds": self.eta_seconds,
        })
    }
}

/// Parses a CLI progress line, `"  <stage>: <done>/<total> probes"` (`regia bake-gi`) or
/// `"  <stage>: <done>/<total> texels"` (`regia bake-lightmaps`), with a checkpointed bake's
/// ` (<remaining> left, eta <s> s)` after it (ADR-3095).
#[must_use]
pub fn parse_progress(line: &str) -> Option<BakeProgress> {
    let line = line.trim();
    let (line, eta_seconds) = match line.strip_suffix(" s)").and_then(|l| l.rsplit_once(", eta ")) {
        Some((head, eta)) => {
            let head = head.rsplit_once(" (").map_or(head, |(h, _)| h);
            (head, eta.trim().parse::<u64>().ok())
        }
        None => (line, None),
    };
    let rest = line
        .strip_suffix(" probes")
        .or_else(|| line.strip_suffix(" texels"))?;
    let (stage, counts) = rest.rsplit_once(": ")?;
    let (done, total) = counts.split_once('/')?;
    let stage = stage.trim();
    if stage.is_empty() {
        return None;
    }
    Some(BakeProgress {
        stage: stage.to_string(),
        done: done.trim().parse().ok()?,
        total: total.trim().parse().ok()?,
        eta_seconds,
    })
}

/// What a finished bake (or dry run) reported.
#[derive(Debug, Clone, PartialEq)]
pub struct BakeSummary {
    /// A `--dry-run` (layout only, nothing written).
    pub dry_run: bool,
    /// The volume written.
    pub out: Option<String>,
    /// Its size in bytes.
    pub bytes: Option<u64>,
    /// Probes in the layout.
    pub probes: u64,
    /// Valid probes (`None` for a dry run).
    pub valid: Option<u64>,
    /// Invalid probes (`None` for a dry run).
    pub invalid: Option<u64>,
    /// Detail bricks in the layout.
    pub bricks: usize,
    /// Wall time the CLI reported, seconds.
    pub seconds: f64,
    /// The GPU that traced it.
    pub device: Option<String>,
    /// The CLI's whole summary.
    pub json: Value,
}

impl BakeSummary {
    /// The fields above as JSON, with the CLI's summary under `cli`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "dry_run": self.dry_run,
            "out": self.out,
            "bytes": self.bytes,
            "probes": self.probes,
            "valid": self.valid,
            "invalid": self.invalid,
            "bricks": self.bricks,
            "seconds": self.seconds,
            "device": self.device,
            "cli": self.json,
        })
    }
}

/// Why a bake failed: the CLI's description error code (`bake_desc_invalid`,
/// `bake_source_missing`, ...) or one of the panel's own ([`NO_SUMMARY`], [`SPAWN_FAILED`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakeFailure {
    /// Lowercase error code.
    pub code: String,
    /// The description key path at fault, when the CLI named one.
    pub path: Option<String>,
    /// A sentence for the user.
    pub message: String,
}

impl BakeFailure {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            path: None,
            message: message.into(),
        }
    }

    /// `{code, path, message}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "code": self.code, "path": self.path, "message": self.message })
    }
}

/// Reads the CLI's stdout summary. Without one, the failure quotes the last stderr line.
///
/// # Errors
///
/// The CLI's own error (`ok: false`), or [`NO_SUMMARY`] when stdout holds no JSON object.
pub fn parse_outcome(stdout: &str, stderr_tail: &[String]) -> Result<BakeSummary, BakeFailure> {
    let value = stdout
        .find('{')
        .and_then(|start| stdout.get(start..))
        .and_then(|text| serde_json::from_str::<Value>(text.trim()).ok())
        .filter(Value::is_object);
    let Some(value) = value else {
        let last = stderr_tail
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
            .unwrap_or("no output");
        return Err(BakeFailure::new(
            NO_SUMMARY,
            format!("the bake exited without a summary: {last}"),
        ));
    };
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(BakeFailure {
            code: value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("bake_failed")
                .to_string(),
            path: value.get("path").and_then(Value::as_str).map(str::to_string),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the bake failed")
                .to_string(),
        });
    }
    let layers = &value["layers"];
    Ok(BakeSummary {
        dry_run: value.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
        out: value.get("out").and_then(Value::as_str).map(str::to_string),
        bytes: value.get("bytes").and_then(Value::as_u64),
        probes: layers.get("probes").and_then(Value::as_u64).unwrap_or(0),
        valid: layers.get("valid").and_then(Value::as_u64),
        invalid: layers.get("invalid").and_then(Value::as_u64),
        bricks: layers.get("bricks").and_then(Value::as_array).map_or(0, Vec::len),
        seconds: value.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
        device: value.get("device").and_then(Value::as_str).map(str::to_string),
        json: value,
    })
}

/// Where a bake is.
#[derive(Debug, Clone, PartialEq)]
pub enum BakeState {
    /// Nothing started yet.
    Idle,
    /// Running; the last progress line, when one arrived.
    Running(Option<BakeProgress>),
    /// Finished with a summary.
    Succeeded(BakeSummary),
    /// Finished with an error.
    Failed(BakeFailure),
    /// Cancelled by the user or an agent.
    Cancelled,
}

impl BakeState {
    /// `idle`, `running`, `succeeded`, `failed` or `cancelled`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running(_) => "running",
            Self::Succeeded(_) => "succeeded",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the bake is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }
}

enum StreamEvent {
    Out(String),
    Err(String),
    Closed,
}

fn pump(reader: impl Read, tx: &Sender<StreamEvent>, wrap: fn(String) -> StreamEvent) {
    // Lossy byte lines: a non-UTF-8 byte must not end the read (audit 11-03 editor M6).
    let mut open = true;
    crate::tools::host::for_each_line_lossy(reader, crate::tools::host::MAX_LOG_LINE, |line| {
        open = tx.send(wrap(line)).is_ok();
        open
    });
    if open {
        let _ = tx.send(StreamEvent::Closed);
    }
}

/// One running (or finished) bake: the child process and what its pipes said.
pub struct BakeJob {
    request: BakeRequest,
    child: Option<Child>,
    events: Mutex<Receiver<StreamEvent>>,
    open_streams: usize,
    stdout: String,
    log: VecDeque<String>,
    progress: Option<BakeProgress>,
    state: BakeState,
}

impl BakeJob {
    /// Starts `program bake-gi ...` for `request` in `cwd`.
    ///
    /// # Errors
    ///
    /// [`SPAWN_FAILED`]: the program does not exist or cannot be run.
    pub fn spawn(program: &Path, request: &BakeRequest, cwd: Option<&Path>) -> Result<Self, BakeFailure> {
        Self::spawn_args(program, request.clone(), &request.args(), cwd)
    }

    /// Starts `program` with `args` in `cwd`, with the same pipe contract as a `bake-gi` run
    /// (`regia bake-lightmaps`, the Lightmaps panel); `request` is what the job reports.
    ///
    /// # Errors
    ///
    /// [`SPAWN_FAILED`]: the program does not exist or cannot be run.
    pub fn spawn_args(
        program: &Path,
        request: BakeRequest,
        args: &[OsString],
        cwd: Option<&Path>,
    ) -> Result<Self, BakeFailure> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().map_err(|err| {
            BakeFailure::new(
                SPAWN_FAILED,
                format!(
                    "cannot run {}: {err} (build the CLI with `cargo build -p regia_cli`, or set {CLI_ENV})",
                    program.display()
                ),
            )
        })?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BakeFailure::new(SPAWN_FAILED, "the bake's pipes did not open"));
        };
        let mut job = Self::from_readers(request, stdout, stderr);
        job.child = Some(child);
        Ok(job)
    }

    /// A job reading `stdout` and `stderr` streams with no child process (tests, replays).
    #[must_use]
    pub fn from_readers(
        request: BakeRequest,
        stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let tx_err = tx.clone();
        let out_thread = std::thread::Builder::new()
            .name("bake-stdout".to_string())
            .spawn(move || pump(stdout, &tx, StreamEvent::Out));
        let err_thread = std::thread::Builder::new()
            .name("bake-stderr".to_string())
            .spawn(move || pump(stderr, &tx_err, StreamEvent::Err));
        Self {
            request,
            child: None,
            events: Mutex::new(rx),
            open_streams: usize::from(out_thread.is_ok()) + usize::from(err_thread.is_ok()),
            stdout: String::new(),
            log: VecDeque::new(),
            progress: None,
            state: BakeState::Running(None),
        }
    }

    /// The request this job runs.
    #[must_use]
    pub fn request(&self) -> &BakeRequest {
        &self.request
    }

    /// The state as of the last [`Self::poll`] / [`Self::wait`].
    #[must_use]
    pub fn state(&self) -> &BakeState {
        &self.state
    }

    /// The last progress line (kept after the bake finishes).
    #[must_use]
    pub fn progress(&self) -> Option<&BakeProgress> {
        self.progress.as_ref()
    }

    /// The last [`LOG_LINES`] lines of the bake's stderr.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log.iter().cloned().collect()
    }

    fn absorb(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Out(line) => {
                if self.stdout.len() + line.len() < MAX_STDOUT_BYTES {
                    self.stdout.push_str(&line);
                    self.stdout.push('\n');
                }
            }
            StreamEvent::Err(line) => {
                if let Some(progress) = parse_progress(&line) {
                    self.state = BakeState::Running(Some(progress.clone()));
                    self.progress = Some(progress);
                }
                if self.log.len() == LOG_LINES {
                    self.log.pop_front();
                }
                self.log.push_back(line);
            }
            StreamEvent::Closed => self.open_streams = self.open_streams.saturating_sub(1),
        }
    }

    /// Drains what the pipes said so far without blocking and returns the state.
    pub fn poll(&mut self) -> &BakeState {
        while self.state.is_running() && self.open_streams > 0 {
            let event = match self.events.get_mut() {
                Ok(rx) => rx.try_recv(),
                Err(_) => Err(TryRecvError::Disconnected),
            };
            match event {
                Ok(event) => self.absorb(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.open_streams = 0,
            }
        }
        self.finish(false);
        &self.state
    }

    /// Blocks until the bake finishes (or was cancelled) and returns the state. The editor
    /// calls this only for dry runs, which do not trace.
    pub fn wait(&mut self) -> &BakeState {
        while self.state.is_running() && self.open_streams > 0 {
            let event = match self.events.get_mut() {
                Ok(rx) => rx.recv().ok(),
                Err(_) => None,
            };
            match event {
                Some(event) => self.absorb(event),
                None => self.open_streams = 0,
            }
        }
        self.finish(true);
        &self.state
    }

    /// Once both pipes closed: reaps the child and reads the summary.
    fn finish(&mut self, block: bool) {
        if !self.state.is_running() || self.open_streams > 0 {
            return;
        }
        let status = match self.child.as_mut() {
            None => None,
            Some(child) if block => child.wait().ok(),
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status),
                // The pipes closed but the process has not exited yet: next poll.
                Ok(None) => return,
                Err(_) => None,
            },
        };
        self.child = None;
        let log: Vec<String> = self.log.iter().cloned().collect();
        self.state = match parse_outcome(&self.stdout, &log) {
            Ok(summary) => BakeState::Succeeded(summary),
            Err(mut failure) => {
                if let Some(status) = status
                    && !status.success()
                    && failure.code == NO_SUMMARY
                {
                    failure.message = format!("{} ({status})", failure.message);
                }
                BakeState::Failed(failure)
            }
        };
    }

    /// Kills the bake. Returns `false` when it had already finished.
    pub fn cancel(&mut self) -> bool {
        if !self.state.is_running() {
            return false;
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.state = BakeState::Cancelled;
        true
    }
}

impl Drop for BakeJob {
    /// A bake still running when the editor drops it (a new bake, the editor closing) is
    /// killed rather than left behind.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Starts a bake: [`BakeJob::spawn`] in the editor, a fake in tests.
pub type Launcher = fn(&Path, &BakeRequest, Option<&Path>) -> Result<BakeJob, BakeFailure>;

/// The Bake runner panel's state (a world resource shared by the panel and the agent methods).
#[derive(Resource)]
pub struct BakePanel {
    /// The selected scene: a built-in name or a project-relative `.bake.ron`.
    pub scene: String,
    /// The output path typed in the panel (`""`: [`default_out`]).
    pub out: String,
    /// The overrides set in the panel.
    pub overrides: BakeOverrides,
    /// The project's descriptions, once listed (`None`: list on the next draw).
    pub descriptions: Option<Vec<String>>,
    /// The CLI to run (`None`: [`cli_program`]).
    pub program: Option<PathBuf>,
    /// How bakes start.
    pub launcher: Launcher,
    /// Bakes started in this session (the current one's number).
    pub started: u64,
    job: Option<BakeJob>,
    start_error: Option<BakeFailure>,
    /// The person picked a scene in the panel: it no longer follows the open level (ADR-8586).
    scene_picked: bool,
}

impl Default for BakePanel {
    fn default() -> Self {
        Self {
            scene: BUILTIN_SCENES[0].to_string(),
            out: String::new(),
            overrides: BakeOverrides::default(),
            descriptions: None,
            program: None,
            launcher: BakeJob::spawn,
            started: 0,
            job: None,
            start_error: None,
            scene_picked: false,
        }
    }
}

/// The open level's bake description among `descriptions` (project-relative, as the panel lists
/// them; bug 23, ADR-8586): `bake/<stem>.bake.ron` (`regia bake-gi --from-scene`), then
/// `bake/<stem>.editor.bake.ron` (the Bake panel's), then `<stem>.bake.ron` beside the level.
#[must_use]
pub fn level_scene(level: &Path, project: Option<&Path>, descriptions: &[String]) -> Option<String> {
    let name = level.file_name()?.to_string_lossy().into_owned();
    let stem = name.split('.').next().filter(|s| !s.is_empty())?;
    let beside = project
        .and_then(|root| level.strip_prefix(root).ok())
        .and_then(Path::parent)
        .map(|dir| crate::content::slash_path(&dir.join(format!("{stem}.bake.ron"))));
    [
        Some(format!("bake/{stem}.bake.ron")),
        Some(format!("bake/{stem}.editor.bake.ron")),
        beside,
    ]
    .into_iter()
    .flatten()
    .find(|candidate| descriptions.iter().any(|d| d == candidate))
}

/// Points the panel's scene at the open level's bake description while the person has not picked
/// one (bug 23, ADR-8586): the default is the level being edited, not a built-in test scene. Without
/// a level or a description for it, the scene stays as it is.
pub fn follow_open_level(panel: &mut BakePanel, world: &World, project: Option<&Path>) {
    if panel.scene_picked || panel.is_running() {
        return;
    }
    let Some(level) = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone())
    else {
        return;
    };
    if panel.descriptions.is_none() {
        panel.descriptions = Some(project.map(find_bake_descriptions).unwrap_or_default());
    }
    let descriptions = panel.descriptions.as_deref().unwrap_or_default();
    if let Some(scene) = level_scene(&level, project, descriptions) {
        panel.scene = scene;
    }
}

impl BakePanel {
    /// The current (or last) job's state; a failed start is a `Failed` state.
    #[must_use]
    pub fn state(&self) -> BakeState {
        match (&self.job, &self.start_error) {
            (_, Some(failure)) => BakeState::Failed(failure.clone()),
            (Some(job), None) => job.state().clone(),
            (None, None) => BakeState::Idle,
        }
    }

    /// The current (or last) job.
    #[must_use]
    pub fn job(&self) -> Option<&BakeJob> {
        self.job.as_ref()
    }

    /// Whether a bake is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job.state().is_running())
    }

    /// The request the panel's fields describe, resolved against `project`.
    #[must_use]
    pub fn request(&self, project: Option<&Path>, dry_run: bool) -> BakeRequest {
        let scene = resolve_scene(&self.scene, project);
        let out = if self.out.trim().is_empty() {
            default_out(&scene, project)
        } else {
            let out = Path::new(self.out.trim());
            Some(match project {
                Some(root) if out.is_relative() => root.join(out),
                _ => out.to_path_buf(),
            })
        };
        BakeRequest {
            scene,
            out: if dry_run { None } else { out },
            overrides: self.overrides.clone(),
            dry_run,
        }
    }

    /// Starts `request` with the CLI in `project` (its working directory). Returns the job
    /// number.
    ///
    /// # Errors
    ///
    /// `bake_busy` while a bake runs, `bake_out_dir` when the output folder cannot be made,
    /// [`SPAWN_FAILED`] when the CLI does not start. A failed start also shows as the state.
    pub fn start(&mut self, request: &BakeRequest, project: Option<&Path>) -> Result<u64, BakeFailure> {
        self.poll();
        if self.is_running() {
            return Err(BakeFailure::new(
                "bake_busy",
                "a bake is running: wait for it or cancel it first",
            ));
        }
        self.job = None;
        self.start_error = None;
        self.started += 1;
        if let Some(dir) = request.out.as_deref().and_then(Path::parent)
            && !dir.as_os_str().is_empty()
            && let Err(err) = std::fs::create_dir_all(dir)
        {
            let failure = BakeFailure::new("bake_out_dir", format!("cannot create {}: {err}", dir.display()));
            self.start_error = Some(failure.clone());
            return Err(failure);
        }
        let program = self.program.clone().unwrap_or_else(cli_program);
        tracing::info!(target: "regia_editor", "bake {}: {} {:?}", self.started, program.display(), request.args());
        match (self.launcher)(&program, request, project) {
            Ok(job) => {
                self.job = Some(job);
                Ok(self.started)
            }
            Err(failure) => {
                self.start_error = Some(failure.clone());
                Err(failure)
            }
        }
    }

    /// Drains the running job's output (no blocking) and returns the state.
    pub fn poll(&mut self) -> BakeState {
        if let Some(job) = self.job.as_mut() {
            job.poll();
        }
        self.state()
    }

    /// Blocks until the running job finishes and returns the state.
    pub fn wait(&mut self) -> BakeState {
        if let Some(job) = self.job.as_mut() {
            job.wait();
        }
        self.state()
    }

    /// Cancels the running bake. Returns whether one was running.
    pub fn cancel(&mut self) -> bool {
        self.job.as_mut().is_some_and(BakeJob::cancel)
    }

    /// The status `editor.bake_status` returns.
    #[must_use]
    pub fn status_json(&self) -> Value {
        let state = self.state();
        let (summary, error) = match &state {
            BakeState::Succeeded(summary) => (summary.to_json(), Value::Null),
            BakeState::Failed(failure) => (Value::Null, failure.to_json()),
            _ => (Value::Null, Value::Null),
        };
        let job = self.job.as_ref();
        json!({
            "job": self.started,
            "state": state.name(),
            "running": state.is_running(),
            "scene": job.map(|job| job.request().scene.clone()),
            "dry_run": job.map(|job| job.request().dry_run),
            "out": job.and_then(|job| job.request().out.as_ref().map(|out| out.display().to_string())),
            "args": job.map(|job| {
                job.request()
                    .args()
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            }),
            "progress": job.and_then(BakeJob::progress).map(BakeProgress::to_json),
            "summary": summary,
            "error": error,
            "log": job.map(BakeJob::log).unwrap_or_default(),
            // A resumable checkpoint of the job's output (ADR-3095).
            "checkpoint": job
                .and_then(|job| job.request().out.as_deref())
                .map(regia_asset::bake_checkpoint::checkpoint_status),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------------------------

const GOOD: egui::Color32 = egui::Color32::from_rgb(110, 200, 120);
const BAD: egui::Color32 = egui::Color32::from_rgb(230, 100, 90);

/// Draws the Bake runner panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "bake_runner_panel")]
pub fn bake_runner_panel(ui: &mut egui::Ui, world: &mut World) {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<BakePanel>().unwrap_or_default();
    panel.poll();
    follow_open_level(&mut panel, world, project.as_deref());
    draw(ui, &mut panel, project.as_deref());
    if panel.is_running() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
    }
    world.insert_resource(panel);
}

fn override_row(ui: &mut egui::Ui, label: &str, value: &mut Option<u32>, default: u32) {
    ui.horizontal(|ui| {
        let mut on = value.is_some();
        ui.checkbox(&mut on, label);
        match (on, value.as_mut()) {
            (true, Some(v)) => {
                ui.add(egui::DragValue::new(v).range(0..=1_000_000));
            }
            (true, None) => *value = Some(default),
            (false, _) => *value = None,
        }
    });
}

fn draw(ui: &mut egui::Ui, panel: &mut BakePanel, project: Option<&Path>) {
    if panel.descriptions.is_none() {
        panel.descriptions = Some(project.map(find_bake_descriptions).unwrap_or_default());
    }
    let running = panel.is_running();
    ui.horizontal(|ui| {
        ui.strong("Bake GI");
        ui.weak("probe volume · regia bake-gi");
        if ui.small_button("Refresh").clicked() {
            panel.descriptions = None;
        }
    });
    ui.separator();
    ui.add_enabled_ui(!running, |ui| {
        egui::Grid::new("bake-settings").num_columns(2).show(ui, |ui| {
            ui.label("Scene");
            let before = panel.scene.clone();
            egui::ComboBox::from_id_salt("bake-scene")
                .selected_text(panel.scene.clone())
                .width(260.0)
                .show_ui(ui, |ui| {
                    for name in BUILTIN_SCENES {
                        ui.selectable_value(&mut panel.scene, name.to_string(), format!("{name} (built-in)"));
                    }
                    for rel in panel.descriptions.clone().unwrap_or_default() {
                        ui.selectable_value(&mut panel.scene, rel.clone(), rel);
                    }
                });
            if panel.scene != before {
                panel.scene_picked = true;
            }
            ui.end_row();
            ui.label("Output");
            let hint = default_out(&resolve_scene(&panel.scene, project), project)
                .map_or_else(|| "<scene>.fgiv".to_string(), |out| out.display().to_string());
            ui.add(
                egui::TextEdit::singleline(&mut panel.out)
                    .hint_text(hint)
                    .desired_width(260.0),
            );
            ui.end_row();
        });
        egui::CollapsingHeader::new("Overrides")
            .id_salt("bake-overrides")
            .show(ui, |ui| {
                let o = &mut panel.overrides;
                override_row(ui, "Rays per probe", &mut o.rays, 256);
                override_row(ui, "Samples", &mut o.samples, 1);
                override_row(ui, "Bounces", &mut o.bounces, 3);
                override_row(ui, "Seed", &mut o.seed, 1);
                override_row(ui, "Probes per batch", &mut o.probes_per_batch, 4096);
                ui.checkbox(&mut o.no_filter, "Skip the visibility filter");
            });
    });
    if panel.descriptions.as_ref().is_some_and(Vec::is_empty) && project.is_some() {
        ui.weak("No .bake.ron in the project yet; the built-in scenes still bake.");
    }

    ui.horizontal(|ui| {
        let validate = ui
            .add_enabled(!running, egui::Button::new("Validate"))
            .on_hover_text("Dry run: check the description and show the layout, no GPU");
        let bake = ui.add_enabled(!running, egui::Button::new("Bake"));
        let cancel = ui.add_enabled(running, egui::Button::new("Cancel"));
        if validate.clicked() || bake.clicked() {
            let request = panel.request(project, validate.clicked());
            let _ = panel.start(&request, project);
        }
        if cancel.clicked() {
            panel.cancel();
        }
    });
    ui.separator();
    draw_state(ui, panel);
}

fn draw_state(ui: &mut egui::Ui, panel: &BakePanel) {
    let state = panel.state();
    match &state {
        BakeState::Idle => {
            ui.weak("Pick a scene, then Validate or Bake.");
        }
        BakeState::Running(progress) => match progress {
            Some(progress) => {
                ui.add(egui::ProgressBar::new(progress.fraction()).text(format!(
                    "{}: {}/{} probes",
                    progress.stage, progress.done, progress.total
                )));
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Starting the bake…");
                });
            }
        },
        BakeState::Succeeded(summary) => {
            ui.colored_label(
                GOOD,
                if summary.dry_run {
                    "Description is valid"
                } else {
                    "Bake finished"
                },
            );
            egui::Grid::new("bake-summary")
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    let mut row = |key: &str, value: String| {
                        ui.label(key);
                        ui.label(value);
                        ui.end_row();
                    };
                    row("probes", summary.probes.to_string());
                    row("bricks", summary.bricks.to_string());
                    if let (Some(valid), Some(invalid)) = (summary.valid, summary.invalid) {
                        row("valid / invalid", format!("{valid} / {invalid}"));
                    }
                    if let Some(out) = &summary.out {
                        row("written", out.clone());
                    }
                    if let Some(bytes) = summary.bytes {
                        row("size", format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0)));
                    }
                    if let Some(device) = &summary.device {
                        row("device", device.clone());
                    }
                    row("time", format!("{:.1} s", summary.seconds));
                });
        }
        BakeState::Failed(failure) => {
            ui.colored_label(BAD, format!("Bake failed ({})", failure.code));
            if let Some(path) = &failure.path {
                ui.label(format!("at {path}"));
            }
            ui.label(&failure.message);
        }
        BakeState::Cancelled => {
            ui.weak("Cancelled.");
        }
    }
    if let Some(job) = panel.job() {
        let log = job.log();
        if !log.is_empty() {
            egui::CollapsingHeader::new(format!("Log ({} lines)", log.len()))
                .id_salt("bake-log")
                .show(ui, |ui| {
                    for line in &log {
                        ui.monospace(line);
                    }
                });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

pub(crate) fn bake_error(code: i64, failure: &BakeFailure, hint: &str) -> RpcError {
    let mut error = RpcError::new(code, "F0002", failure.message.clone(), hint);
    if let Some(data) = error.data.as_mut() {
        data["bake_error"] = json!(failure.code);
    }
    error
}

pub(crate) fn u32_param(params: &Value, name: &str) -> Result<Option<u32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a whole number"))),
    }
}

pub(crate) fn bool_param(params: &Value, name: &str) -> bool {
    params.get(name).and_then(Value::as_bool).unwrap_or(false)
}

/// `editor.bake_gi`.
fn bake_gi_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let scene = str_param(params, "scene")
        .filter(|scene| !scene.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params("`scene` (a built-in name or a .bake.ron path) is required")
        })?;
    // The scene is handed to the CLI as an argument: never as a flag (ADR-1816). Its file is only
    // read and parsed as a bake description, so it may lie anywhere (like `editor.import` sources).
    if scene.starts_with('-') {
        return Err(RpcError::invalid_params("`scene` must not start with `-`"));
    }
    let dry_run = bool_param(params, "dry_run");
    let wait = bool_param(params, "wait");
    if wait && !dry_run {
        return Err(bake_error(
            regia_agent::protocol::INVALID_PARAMS,
            &BakeFailure::new(
                "bake_wait_needs_dry_run",
                "`wait` blocks the editor, so only a dry run may wait",
            ),
            "start the bake without `wait` and poll editor.bake_status until running is false",
        ));
    }
    let overrides = BakeOverrides {
        rays: u32_param(params, "rays")?,
        samples: u32_param(params, "samples")?,
        bounces: u32_param(params, "bounces")?,
        seed: u32_param(params, "seed")?,
        probes_per_batch: u32_param(params, "probes_per_batch")?,
        no_filter: bool_param(params, "no_filter"),
    };
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<BakePanel>().unwrap_or_default();
    let resolved = resolve_scene(scene, project.as_deref());
    let out = if dry_run {
        None
    } else {
        match str_param(params, "out").filter(|out| !out.is_empty()) {
            // Under the agent's allowed roots only (ADR-1816).
            Some(out) => Some(crate::agent_ext::allowed_request_path(world, "out", out)?),
            None => default_out(&resolved, project.as_deref()),
        }
    };
    let request = BakeRequest {
        scene: resolved,
        out,
        overrides,
        dry_run,
    };
    let started = panel.start(&request, project.as_deref());
    let reply = match started {
        Err(failure) => {
            let (code, hint) = match failure.code.as_str() {
                "bake_busy" => (
                    regia_agent::protocol::INVALID_REQUEST,
                    "poll editor.bake_status until running is false, or call editor.bake_gi_cancel",
                ),
                SPAWN_FAILED => (
                    regia_agent::protocol::UNSUPPORTED,
                    "build the CLI (cargo build -p regia_cli) or set REGIA_CLI to its path",
                ),
                _ => (
                    regia_agent::protocol::INTERNAL_ERROR,
                    "check the output path and the editor log",
                ),
            };
            Err(bake_error(code, &failure, hint))
        }
        Ok(_) => {
            if wait {
                panel.wait();
            } else {
                panel.poll();
            }
            Ok(panel.status_json())
        }
    };
    world.insert_resource(panel);
    reply
}

/// `editor.bake_status`.
fn bake_status_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.get_resource_or_insert_with(BakePanel::default);
    panel.poll();
    let mut status = panel.status_json();
    if bool_param(params, "list") {
        status["builtins"] = json!(BUILTIN_SCENES);
        status["descriptions"] = json!(project.as_deref().map(find_bake_descriptions).unwrap_or_default());
    }
    Ok(status)
}

/// `editor.bake_gi_cancel`.
fn bake_cancel_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.get_resource_or_insert_with(BakePanel::default);
    panel.poll();
    let cancelled = panel.cancel();
    let mut status = panel.status_json();
    status["cancelled"] = json!(cancelled);
    Ok(status)
}

/// The Bake runner panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.bake_gi",
                tool: "bake_gi",
                description: "Start a GI probe-volume bake in the Bake runner panel: runs `regia bake-gi` on a \
                    built-in scene or a .bake.ron in a child process and returns the status \
                    (editor.bake_status's shape). `dry_run: true` validates and lays out without \
                    tracing; only a dry run may `wait`. One bake at a time (data.bake_error \
                    `bake_busy`).",
                params: &[
                    ParamSchema {
                        name: "scene",
                        kind: "string",
                        required: true,
                        description: "`cornell`, `furnace`, `rolling-hills`, or a .bake.ron path (absolute or project-relative).",
                    },
                    ParamSchema {
                        name: "out",
                        kind: "string",
                        required: false,
                        description: "Output .fgiv. Default: beside the description (`<stem>.fgiv`), or .regia/bake/<name>.fgiv for a built-in.",
                    },
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "Validate and print the layout only (no GPU, nothing written). Default false.",
                    },
                    ParamSchema {
                        name: "wait",
                        kind: "boolean",
                        required: false,
                        description: "Block until the dry run finishes. Dry runs only. Default false.",
                    },
                    ParamSchema {
                        name: "rays",
                        kind: "number",
                        required: false,
                        description: "Rays per probe per sample (overrides the description).",
                    },
                    ParamSchema {
                        name: "samples",
                        kind: "number",
                        required: false,
                        description: "Samples per probe.",
                    },
                    ParamSchema {
                        name: "bounces",
                        kind: "number",
                        required: false,
                        description: "Maximum path vertices.",
                    },
                    ParamSchema {
                        name: "seed",
                        kind: "number",
                        required: false,
                        description: "Trace seed.",
                    },
                    ParamSchema {
                        name: "probes_per_batch",
                        kind: "number",
                        required: false,
                        description: "Probes per GPU batch.",
                    },
                    ParamSchema {
                        name: "no_filter",
                        kind: "boolean",
                        required: false,
                        description: "Skip the visibility-weighted filter. Default false.",
                    },
                ],
                result: "{ job, state, running, scene, dry_run, out, args, progress, summary, error, log, checkpoint }",
                mutating: true,
                permission: Permission::Assets,
            },
            bake_gi_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bake_status",
                tool: "bake_status",
                description: "The Bake runner panel's current or last bake: state (`idle`, `running`, \
                    `succeeded`, `failed`, `cancelled`), progress {stage, done, total, fraction, remaining, eta_seconds}, \
                    the summary (probes, valid, invalid, bricks, out, bytes, seconds) or the error \
                    {code, path, message}, and the last stderr lines. `list: true` adds the built-in \
                    scenes and the project's .bake.ron files.",
                params: &[ParamSchema {
                    name: "list",
                    kind: "boolean",
                    required: false,
                    description: "Also list `builtins` and the project's `descriptions` (project-relative .bake.ron paths). Default false.",
                }],
                result: "{ job, state, running, scene, dry_run, out, args, progress, summary, error, log, checkpoint, builtins?, descriptions? }",
                mutating: false,
                permission: Permission::Read,
            },
            bake_status_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bake_gi_cancel",
                tool: "bake_gi_cancel",
                description: "Kill the running bake (nothing is written). `cancelled` is false when no \
                    bake was running.",
                params: &[],
                result: "{ cancelled, job, state, ... }",
                mutating: true,
                permission: Permission::Assets,
            },
            bake_cancel_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Audit 11-03 editor M6 (bake runner copy): a non-UTF-8 line ended the reader early.
    #[test]
    fn a_non_utf8_bake_line_does_not_end_the_reader() {
        let (tx, rx) = mpsc::channel();
        let bytes: &[u8] = b"first\n\xff bad\nafter\n";
        pump(bytes, &tx, StreamEvent::Out);
        drop(tx);
        let lines: Vec<String> = rx
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Out(line) => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 3, "the reader stopped at the bad line: {lines:?}");
        assert_eq!(lines[2], "after");
    }

    const SUMMARY: &str = r#"{
  "ok": true,
  "out": "/p/maps/bore.fgiv",
  "bytes": 2048,
  "scene": "/p/maps/bore.bake.ron",
  "device": "lavapipe (Vulkan)",
  "seconds": 3.5,
  "layers": { "terrain": null, "bricks": [{"index": 0}, {"index": 1}], "probes": 54, "valid": 50, "invalid": 4 }
}"#;

    const DRY_RUN: &str = r#"{
  "ok": true,
  "dry_run": true,
  "scene": "furnace",
  "layers": { "terrain": null, "bricks": [{"index": 0}], "probes": 27, "valid": null, "invalid": null },
  "seconds": 0.01
}"#;

    const DESC_ERROR: &str = r#"{"ok": false, "code": "bake_desc_invalid", "path": "layout.auto_bricks.spacing", "message": "spacing must be > 0"}"#;

    fn readers(stdout: &str, stderr: &str) -> (Cursor<Vec<u8>>, Cursor<Vec<u8>>) {
        (
            Cursor::new(stdout.as_bytes().to_vec()),
            Cursor::new(stderr.as_bytes().to_vec()),
        )
    }

    fn fake_launcher(
        _program: &Path,
        request: &BakeRequest,
        _cwd: Option<&Path>,
    ) -> Result<BakeJob, BakeFailure> {
        let (stdout, stderr) = if request.scene.ends_with("bad.bake.ron") {
            readers(DESC_ERROR, "")
        } else if request.dry_run {
            readers(DRY_RUN, "")
        } else {
            readers(SUMMARY, "  trace: 27/54 probes\n  trace: 54/54 probes\n")
        };
        Ok(BakeJob::from_readers(request.clone(), stdout, stderr))
    }

    /// A reader that blocks until its sender drops: a bake that never ends on its own.
    struct Blocking(Receiver<()>);

    impl Read for Blocking {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    #[test]
    fn request_args_are_the_cli_flags() {
        let mut request = BakeRequest::new("maps/bore.bake.ron");
        assert_eq!(
            request.args(),
            ["bake-gi", "maps/bore.bake.ron"].map(OsString::from)
        );
        request.out = Some(PathBuf::from("out/bore.fgiv"));
        request.overrides = BakeOverrides {
            rays: Some(64),
            samples: None,
            bounces: Some(2),
            seed: Some(7),
            probes_per_batch: Some(512),
            no_filter: true,
        };
        request.dry_run = true;
        let args: Vec<String> = request
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "bake-gi",
                "maps/bore.bake.ron",
                "--out",
                "out/bore.fgiv",
                "--rays",
                "64",
                "--bounces",
                "2",
                "--seed",
                "7",
                "--probes-per-batch",
                "512",
                "--no-filter",
                "--dry-run",
            ]
        );
    }

    #[test]
    fn progress_lines_parse_and_other_lines_do_not() {
        let progress = parse_progress("  trace: 12/48 probes").expect("progress");
        assert_eq!(progress.stage, "trace");
        assert_eq!((progress.done, progress.total), (12, 48));
        assert!((progress.fraction() - 0.25).abs() < 1e-6);
        assert_eq!(
            parse_progress("relocate: invalid probes: 3/9 probes").map(|p| p.stage),
            Some("relocate: invalid probes".to_string()),
            "the counts are the last `: ` field"
        );
        assert!(parse_progress("warning: something").is_none());
        assert!(parse_progress("trace: x/48 probes").is_none());
        assert!(parse_progress(": 1/2 probes").is_none());
        let empty = BakeProgress {
            stage: "trace".into(),
            done: 0,
            total: 0,
            eta_seconds: None,
        };
        // A checkpointed bake's line carries the work left and the estimate (ADR-3095).
        let eta = parse_progress("  bake: 120/480 probes (360 left, eta 35 s)").expect("eta line");
        assert_eq!((eta.done, eta.total, eta.eta_seconds), (120, 480, Some(35)));
        assert_eq!(eta.remaining(), 360);
        assert_eq!(eta.to_json()["eta_seconds"], 35);
        let texels = parse_progress("  lightmap: 5/10 texels (5 left, eta 7 s)").expect("texels");
        assert_eq!((texels.stage.as_str(), texels.eta_seconds), ("lightmap", Some(7)));
        assert_eq!(
            parse_progress("  trace: 12/48 probes").and_then(|p| p.eta_seconds),
            None
        );
        assert!(
            empty.fraction().abs() < f32::EPSILON,
            "an empty stage is 0, not NaN"
        );
    }

    #[test]
    fn outcomes_parse_summary_dry_run_and_errors() {
        let summary = parse_outcome(SUMMARY, &[]).expect("summary");
        assert!(!summary.dry_run);
        assert_eq!(summary.out.as_deref(), Some("/p/maps/bore.fgiv"));
        assert_eq!(summary.bytes, Some(2048));
        assert_eq!(
            (summary.probes, summary.valid, summary.invalid),
            (54, Some(50), Some(4))
        );
        assert_eq!(summary.bricks, 2);
        assert_eq!(summary.device.as_deref(), Some("lavapipe (Vulkan)"));

        let dry = parse_outcome(DRY_RUN, &[]).expect("dry run");
        assert!(dry.dry_run);
        assert_eq!((dry.probes, dry.valid, dry.bricks), (27, None, 1));

        let err = parse_outcome(DESC_ERROR, &[]).expect_err("description error");
        assert_eq!(err.code, "bake_desc_invalid");
        assert_eq!(err.path.as_deref(), Some("layout.auto_bricks.spacing"));

        let tail = vec!["Error: no GPU adapter for the bake".to_string(), String::new()];
        let none = parse_outcome("", &tail).expect_err("no summary");
        assert_eq!(none.code, NO_SUMMARY);
        assert!(none.message.contains("no GPU adapter"), "{}", none.message);
        let garbage = parse_outcome("not json {", &[]).expect_err("garbage");
        assert_eq!(garbage.code, NO_SUMMARY);
    }

    #[test]
    fn a_job_reads_progress_then_the_summary() {
        let (stdout, stderr) = readers(SUMMARY, "  trace: 27/54 probes\nnoise\n  trace: 54/54 probes\n");
        let mut job = BakeJob::from_readers(BakeRequest::new("x.bake.ron"), stdout, stderr);
        let state = job.wait().clone();
        let summary = match &state {
            BakeState::Succeeded(summary) => summary.clone(),
            other => panic!("expected success, got {other:?}"),
        };
        assert_eq!(summary.probes, 54);
        let progress = job.progress().expect("progress kept after the end");
        assert_eq!((progress.done, progress.total), (54, 54));
        assert_eq!(job.log().len(), 3, "every stderr line is in the log");
        assert!(!job.cancel(), "a finished bake cannot be cancelled");
        assert_eq!(
            job.poll().name(),
            "succeeded",
            "polling a finished job changes nothing"
        );
    }

    #[test]
    fn the_log_keeps_the_last_lines_only() {
        let stderr: String = (0..LOG_LINES + 5).map(|i| format!("line {i}\n")).collect();
        let (stdout, stderr) = readers(DRY_RUN, &stderr);
        let mut job = BakeJob::from_readers(BakeRequest::new("furnace"), stdout, stderr);
        job.wait();
        let log = job.log();
        assert_eq!(log.len(), LOG_LINES);
        assert_eq!(log.first().map(String::as_str), Some("line 5"));
    }

    #[test]
    fn cancel_stops_a_running_job() {
        let (out_tx, out_rx) = mpsc::channel::<()>();
        let (err_tx, err_rx) = mpsc::channel::<()>();
        let mut job = BakeJob::from_readers(BakeRequest::new("furnace"), Blocking(out_rx), Blocking(err_rx));
        assert!(job.poll().is_running(), "nothing arrived: still running");
        assert!(job.cancel());
        assert_eq!(job.state(), &BakeState::Cancelled);
        assert_eq!(
            job.poll(),
            &BakeState::Cancelled,
            "late output does not revive it"
        );
        drop((out_tx, err_tx));
    }

    #[test]
    fn the_panel_runs_one_bake_at_a_time() {
        fn blocking_launcher(
            _program: &Path,
            request: &BakeRequest,
            _cwd: Option<&Path>,
        ) -> Result<BakeJob, BakeFailure> {
            // Dropped senders would end the readers; forgetting them keeps this bake running
            // until it is cancelled (the two reader threads end with the test process).
            let (out_tx, out_rx) = mpsc::channel::<()>();
            let (err_tx, err_rx) = mpsc::channel::<()>();
            std::mem::forget((out_tx, err_tx));
            Ok(BakeJob::from_readers(
                request.clone(),
                Blocking(out_rx),
                Blocking(err_rx),
            ))
        }
        let mut panel = BakePanel {
            launcher: blocking_launcher,
            ..Default::default()
        };
        let request = BakeRequest::new("furnace");
        assert_eq!(panel.start(&request, None), Ok(1));
        assert!(panel.is_running());
        let busy = panel.start(&request, None).expect_err("busy");
        assert_eq!(busy.code, "bake_busy");
        assert!(
            panel.is_running(),
            "the refused start left the running bake alone"
        );
        assert!(panel.cancel());
        assert_eq!(panel.state(), BakeState::Cancelled);
        assert!(!panel.cancel(), "nothing left to cancel");
        panel.launcher = fake_launcher;
        assert_eq!(
            panel.start(&request, None),
            Ok(2),
            "a new bake starts after a cancel"
        );
        assert_eq!(panel.wait().name(), "succeeded");
    }

    #[test]
    fn a_missing_cli_is_a_spawn_failure() {
        let dir = regia_test_utils::unique_temp_dir("bake_panel_no_cli");
        let mut panel = BakePanel {
            program: Some(dir.join("no-such-regia")),
            ..Default::default()
        };
        let failure = panel
            .start(&BakeRequest::new("furnace"), None)
            .expect_err("no such program");
        assert_eq!(failure.code, SPAWN_FAILED);
        assert_eq!(panel.state().name(), "failed");
        assert_eq!(panel.status_json()["error"]["code"], SPAWN_FAILED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scenes_resolve_and_outputs_default_beside_the_description() {
        let root = Path::new("/proj");
        assert_eq!(resolve_scene("furnace", Some(root)), "furnace");
        assert_eq!(
            resolve_scene("maps/bore.bake.ron", Some(root)),
            Path::new("/proj")
                .join("maps/bore.bake.ron")
                .display()
                .to_string()
        );
        assert_eq!(resolve_scene("maps/bore.bake.ron", None), "maps/bore.bake.ron");
        assert_eq!(
            default_out("maps/bore.bake.ron", None),
            Some(PathBuf::from("maps/bore.fgiv"))
        );
        assert_eq!(
            default_out("cornell", Some(root)),
            Some(root.join(".regia").join("bake").join("cornell.fgiv"))
        );
        assert_eq!(default_out("cornell", None), None);
    }

    /// Bug 23 (ADR-8586): with a level open the scene defaults to its bake description, not the
    /// built-in test scene; a pick in the panel sticks; without a description the scene stays.
    #[test]
    fn the_scene_defaults_to_the_open_level() {
        let dir = regia_test_utils::unique_temp_dir("bake_runner_level_default");
        std::fs::create_dir_all(dir.join("bake")).expect("dirs");
        std::fs::create_dir_all(dir.join("maps")).expect("dirs");
        regia_test_utils::write_atomic(&dir.join("bake/bore.bake.ron"), b"(format_version: 1)")
            .expect("write");
        regia_test_utils::write_atomic(&dir.join("maps/hall.bake.ron"), b"(format_version: 1)")
            .expect("write");
        let mut world = World::new();
        let mut panel = BakePanel::default();
        follow_open_level(&mut panel, &world, Some(dir.as_path()));
        assert_eq!(
            panel.scene, "cornell",
            "no level open: the built-in default stays"
        );
        world.insert_resource(crate::level_io::OpenLevel {
            path: Some(dir.join("maps/bore.level.ron")),
            id: regia_scene::level::LevelId::default(),
        });
        follow_open_level(&mut panel, &world, Some(dir.as_path()));
        assert_eq!(panel.scene, "bake/bore.bake.ron");
        world.insert_resource(crate::level_io::OpenLevel {
            path: Some(dir.join("maps/hall.level.ron")),
            id: regia_scene::level::LevelId::default(),
        });
        follow_open_level(&mut panel, &world, Some(dir.as_path()));
        assert_eq!(
            panel.scene, "maps/hall.bake.ron",
            "a description beside the level"
        );
        world.insert_resource(crate::level_io::OpenLevel {
            path: Some(dir.join("maps/none.level.ron")),
            id: regia_scene::level::LevelId::default(),
        });
        follow_open_level(&mut panel, &world, Some(dir.as_path()));
        assert_eq!(
            panel.scene, "maps/hall.bake.ron",
            "no description: the scene stays"
        );
        panel.scene_picked = true;
        panel.scene = "furnace".into();
        world.insert_resource(crate::level_io::OpenLevel {
            path: Some(dir.join("maps/bore.level.ron")),
            id: regia_scene::level::LevelId::default(),
        });
        follow_open_level(&mut panel, &world, Some(dir.as_path()));
        assert_eq!(panel.scene, "furnace", "a picked scene sticks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn descriptions_are_listed_relative_and_sorted() {
        let dir = regia_test_utils::unique_temp_dir("bake_panel_list");
        for rel in [
            "maps/b.bake.ron",
            "maps/a.bake.ron",
            "notes.ron",
            ".regia/bake/old.bake.ron",
            "target/x.bake.ron",
            "z.bake.ron",
        ] {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            regia_test_utils::write_atomic(&path, b"(format_version: 1)").expect("write");
        }
        assert_eq!(
            find_bake_descriptions(&dir),
            ["maps/a.bake.ron", "maps/b.bake.ron", "z.bake.ron"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn world_with_fake_bakes() -> World {
        let mut world = World::new();
        world.insert_resource(BakePanel {
            launcher: fake_launcher,
            ..Default::default()
        });
        world
    }

    #[test]
    fn agent_dry_run_waits_and_reports_the_layout() {
        let mut world = world_with_fake_bakes();
        let status = bake_gi_method(
            &mut world,
            &json!({ "scene": "furnace", "dry_run": true, "wait": true, "rays": 16 }),
        )
        .expect("editor.bake_gi");
        assert_eq!(status["state"], "succeeded");
        assert_eq!(status["dry_run"], true);
        assert_eq!(status["summary"]["probes"], 27);
        assert_eq!(status["out"], Value::Null, "a dry run writes nothing");
        assert_eq!(
            status["args"],
            json!(["bake-gi", "furnace", "--rays", "16", "--dry-run"])
        );
        let again = bake_status_method(&mut world, &json!({})).expect("editor.bake_status");
        assert_eq!(again["state"], "succeeded");
        assert_eq!(again["job"], 1);
        assert!(again.get("builtins").is_none(), "listing is opt-in");
        let listed = bake_status_method(&mut world, &json!({ "list": true })).expect("list");
        assert_eq!(listed["builtins"], json!(BUILTIN_SCENES));
        assert_eq!(
            listed["descriptions"],
            json!([]),
            "no project open: nothing to list"
        );
        let cancel = bake_cancel_method(&mut world, &json!({})).expect("editor.bake_gi_cancel");
        assert_eq!(cancel["cancelled"], false, "nothing was running");
    }

    #[test]
    fn agent_description_errors_come_back_in_the_status() {
        let mut world = world_with_fake_bakes();
        let status = bake_gi_method(
            &mut world,
            &json!({ "scene": "/p/bad.bake.ron", "dry_run": true, "wait": true }),
        )
        .expect("the call itself succeeds");
        assert_eq!(status["state"], "failed");
        assert_eq!(status["error"]["code"], "bake_desc_invalid");
        assert_eq!(status["error"]["path"], "layout.auto_bricks.spacing");
    }

    #[test]
    fn agent_full_bake_starts_then_status_shows_the_result() {
        let mut world = world_with_fake_bakes();
        let dir = regia_test_utils::unique_temp_dir("bake_panel_agent_out");
        let out = dir.join("out").join("bore.fgiv");
        let started = bake_gi_method(
            &mut world,
            &json!({ "scene": "/p/maps/bore.bake.ron", "out": out.display().to_string() }),
        )
        .expect("editor.bake_gi");
        assert_eq!(started["job"], 1);
        assert_eq!(started["out"], out.display().to_string());
        assert!(
            dir.join("out").is_dir(),
            "the output folder is made before the bake"
        );
        // Not waited: finish deterministically, then read the status the agent would poll.
        world.resource_mut::<BakePanel>().wait();
        let status = bake_status_method(&mut world, &json!({})).expect("editor.bake_status");
        assert_eq!(status["state"], "succeeded");
        assert_eq!(status["summary"]["valid"], 50);
        assert_eq!(status["progress"]["done"], 54);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_parameter_errors_have_codes() {
        let mut world = world_with_fake_bakes();
        let err = bake_gi_method(&mut world, &json!({})).expect_err("no scene");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
        let err = bake_gi_method(&mut world, &json!({ "scene": "furnace", "wait": true }))
            .expect_err("wait without dry run");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("bake_error")),
            Some(&json!("bake_wait_needs_dry_run"))
        );
        let err = bake_gi_method(&mut world, &json!({ "scene": "furnace", "rays": -1 }))
            .expect_err("negative rays");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
        assert_eq!(
            bake_status_method(&mut world, &json!({})).expect("status")["state"],
            "idle",
            "refused calls start nothing"
        );
    }

    #[test]
    fn methods_are_named_and_registered() {
        let names: Vec<&str> = methods().iter().map(|(schema, _)| schema.name).collect();
        assert_eq!(
            names,
            ["editor.bake_gi", "editor.bake_status", "editor.bake_gi_cancel"]
        );
        let all: Vec<&str> = crate::agent_ext::all_methods()
            .iter()
            .map(|(schema, _)| schema.name)
            .collect();
        for name in names {
            assert!(all.contains(&name), "{name} is installed by agent_ext");
        }
    }

    /// The real CLI: `REGIA_CLI=target/debug/regia cargo test -p regia_editor -- --ignored real_cli`.
    #[test]
    #[ignore = "needs the regia CLI built and REGIA_CLI set"]
    fn real_cli_dry_run_of_the_furnace() {
        let mut panel = BakePanel::default();
        let mut request = BakeRequest::new("furnace");
        request.dry_run = true;
        panel.start(&request, None).expect("the CLI starts");
        let summary = match panel.wait() {
            BakeState::Succeeded(summary) => summary,
            other => panic!("dry run failed: {other:?} {}", panel.status_json()),
        };
        assert!(summary.dry_run);
        assert_eq!(summary.probes, 27);
    }
}
