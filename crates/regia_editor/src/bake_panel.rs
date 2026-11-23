// SPDX-License-Identifier: MPL-2.0
//! The Bake panel: the baked-GI volume the editor bakes (`book/agents/baked-gi-editor.md`,
//! `book/user-guide/bake-panel.md`, ADR-1098).
//!
//! The panel holds one [`BakeVolume`] (a world-space box that becomes a detail brick of probes),
//! the bake settings and the state of the running bake. **Bake** writes a `.bake.ron` v1
//! description ([`desc_ron`]: the volume as a brick, the level's sun and its `GiMobility::Static`
//! meshes as occluders), runs `regia bake-gi` on it as a child process and reads the progress
//! lines the command prints on stderr ([`parse_progress_line`]). The editor never traces in its own
//! process, so a bake cannot stall a frame and **Cancel** is a process kill.
//!
//! The same state serves the agent protocol (`editor.bake_get`, `editor.bake_set`,
//! `editor.bake_start`, `editor.bake_cancel`) and the viewport gizmo ([`bake_gizmo_segments`]), so
//! what an agent sets is what the panel shows.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_render::{DirectionalLight, MeshSource};
use regia_scene::level::LevelSettings;
use regia_scene::prelude::{GiMobility, GlobalTransform};
use serde_json::{Value, json};

use crate::audio_gizmo::Segment;

/// Colour of the volume box in the viewport.
pub const BAKE_VOLUME: egui::Color32 = egui::Color32::from_rgb(120, 230, 120);
/// Colour of the probe lattice on the volume's floor.
pub const BAKE_LATTICE: egui::Color32 = egui::Color32::from_rgb(60, 130, 70);

/// Most probes one editor bake may place (the volume file would pass 100 MB beyond it).
pub const MAX_BAKE_PROBES: u64 = 1 << 22;
/// Smallest probe spacing, metres.
pub const MIN_SPACING: f32 = 0.05;
/// Largest probe spacing, metres.
pub const MAX_SPACING: f32 = 64.0;
/// Largest extent of the volume on one axis, metres.
pub const MAX_EXTENT: f32 = 100_000.0;
/// The lattice is drawn only when it has at most this many probes per floor edge.
const LATTICE_MAX: u32 = 64;
/// Lines of the child's stderr kept for an error message.
const STDERR_TAIL: usize = 12;

/// Stdout kept from a bake (its JSON summary is small); the rest is read and dropped so the child
/// never blocks on a full pipe (audit 11-03 editor L1).
const MAX_STDOUT: usize = 16 * 1024 * 1024;

/// Reads `reader` to its end, keeping the first `cap` bytes (lossy UTF-8).
fn read_capped(reader: impl Read, cap: usize) -> String {
    let mut reader = reader;
    let mut kept = Vec::new();
    let _ = (&mut reader).take(cap as u64).read_to_end(&mut kept);
    let _ = std::io::copy(&mut reader, &mut std::io::sink());
    String::from_utf8_lossy(&kept).into_owned()
}

/// Why a bake request was refused or failed: an agent error code (lowercase ascii, digits and `_`)
/// and a message. The codes are documented in `book/agents/baked-gi-editor.md`; failures of the
/// `regia bake-gi` command keep that command's own code (`bake_desc_invalid`, `bake_layout`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BakeError {
    /// Agent error code.
    pub code: String,
    /// What went wrong.
    pub message: String,
}

impl BakeError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }

    /// The protocol error for this failure: invalid params with the code in `data.bake_error`.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let mut error = RpcError::invalid_params(self.message.clone());
        if let Some(data) = error.data.as_mut() {
            data["bake_error"] = json!(self.code);
        }
        error
    }
}

impl std::fmt::Display for BakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}]", self.message, self.code)
    }
}

impl std::error::Error for BakeError {}

/// The world-space box the bake covers, as one detail brick of probes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BakeVolume {
    /// Box centre, world metres.
    pub center: [f32; 3],
    /// Box extent per axis, metres.
    pub size: [f32; 3],
    /// Probe spacing, metres (the brick places probes at most this far apart).
    pub spacing: f32,
}

impl Default for BakeVolume {
    fn default() -> Self {
        Self {
            center: [0.0, 4.0, 0.0],
            size: [32.0, 8.0, 32.0],
            spacing: 2.0,
        }
    }
}

impl BakeVolume {
    /// Minimum corner.
    #[must_use]
    pub fn min(&self) -> [f32; 3] {
        std::array::from_fn(|a| self.center[a] - self.size[a] * 0.5)
    }

    /// Maximum corner.
    #[must_use]
    pub fn max(&self) -> [f32; 3] {
        std::array::from_fn(|a| self.center[a] + self.size[a] * 0.5)
    }

    /// Probes per axis, as the baker lays a detail brick out: one more than the number of
    /// `spacing` steps that cover the extent, and at least two.
    #[must_use]
    pub fn counts(&self) -> [u32; 3] {
        std::array::from_fn(|a| {
            let len = self.size[a].max(0.0);
            ((len / self.spacing.max(1e-3)).ceil() as u32)
                .saturating_add(1)
                .max(2)
        })
    }

    /// Probes in the brick.
    #[must_use]
    pub fn probe_count(&self) -> u64 {
        self.counts()
            .iter()
            .fold(1u64, |n, &c| n.saturating_mul(u64::from(c)))
    }

    /// Checks the volume: finite numbers, positive extents, a spacing in range, and a probe count
    /// the editor will bake.
    ///
    /// # Errors
    ///
    /// `bake_volume_invalid` with the offending field named.
    pub fn validate(&self) -> Result<(), BakeError> {
        let bad = |what: &str| Err(BakeError::new("bake_volume_invalid", what));
        if self.center.iter().any(|v| !v.is_finite()) {
            return bad("`center` must be finite numbers");
        }
        if self
            .size
            .iter()
            .any(|v| !v.is_finite() || *v <= 0.0 || *v > MAX_EXTENT)
        {
            return bad(&format!(
                "`size` must be between 0 and {MAX_EXTENT} metres on every axis"
            ));
        }
        if !self.spacing.is_finite() || !(MIN_SPACING..=MAX_SPACING).contains(&self.spacing) {
            return bad(&format!(
                "`spacing` must be between {MIN_SPACING} and {MAX_SPACING} metres"
            ));
        }
        let probes = self.probe_count();
        if probes > MAX_BAKE_PROBES {
            return bad(&format!(
                "the volume would hold {probes} probes; the limit is {MAX_BAKE_PROBES} (raise the spacing or shrink the box)"
            ));
        }
        Ok(())
    }
}

/// A bake quality preset: rays per probe, samples and bounces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BakeQuality {
    /// A quick look: 64 rays, 2 samples, 3 bounces.
    Draft,
    /// A working bake: 128 rays, 8 samples, 6 bounces.
    Normal,
    /// A final bake: 256 rays, 16 samples, 8 bounces.
    High,
}

impl BakeQuality {
    /// Every preset, lowest first.
    pub const ALL: [Self; 3] = [Self::Draft, Self::Normal, Self::High];

    /// The lowercase name (`draft`, `normal`, `high`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }

    /// The preset named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.name() == name)
    }

    /// The trace settings of the preset (seed and filter keep the caller's).
    #[must_use]
    pub fn trace(self) -> (u32, u32, u32) {
        match self {
            Self::Draft => (64, 2, 3),
            Self::Normal => (128, 8, 6),
            Self::High => (256, 16, 8),
        }
    }
}

/// The trace settings of the bake (they become the description's `bake: (...)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BakeSettings {
    /// Rays per probe per sample.
    pub rays_per_probe: u32,
    /// Samples per probe.
    pub samples: u32,
    /// Maximum path vertices.
    pub max_bounces: u32,
    /// Seed (the same description and device bake the same volume).
    pub seed: u32,
    /// Run the 3x3x3 visibility-weighted filter.
    pub filter: bool,
}

impl BakeSettings {
    /// The settings of `quality`, seed 1, filter on.
    #[must_use]
    pub fn from_quality(quality: BakeQuality) -> Self {
        let (rays_per_probe, samples, max_bounces) = quality.trace();
        Self {
            rays_per_probe,
            samples,
            max_bounces,
            seed: 1,
            filter: true,
        }
    }

    /// The preset these settings equal (ignoring seed and filter), if any.
    #[must_use]
    pub fn quality(&self) -> Option<BakeQuality> {
        BakeQuality::ALL
            .into_iter()
            .find(|q| q.trace() == (self.rays_per_probe, self.samples, self.max_bounces))
    }

    /// Checks the counts.
    ///
    /// # Errors
    ///
    /// `bake_settings_invalid` when a count is zero or beyond its limit.
    pub fn validate(&self) -> Result<(), BakeError> {
        let check = |name: &str, v: u32, max: u32| {
            if v == 0 || v > max {
                Err(BakeError::new(
                    "bake_settings_invalid",
                    format!("`{name}` must be between 1 and {max}"),
                ))
            } else {
                Ok(())
            }
        };
        check("rays_per_probe", self.rays_per_probe, 4096)?;
        check("samples", self.samples, 1024)?;
        check("max_bounces", self.max_bounces, 32)
    }
}

/// A static mesh the bake traces as an occluder (collected from the level).
#[derive(Clone, Debug, PartialEq)]
pub struct BakeMesh {
    /// Name in reports (`<entity name or index>`).
    pub id: String,
    /// Absolute path of the glTF.
    pub source: String,
    /// Object to world, column-major.
    pub transform: [f32; 16],
}

/// The level's sun for the bake: the direction toward it and its illuminance in lux.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BakeSun {
    /// Vector toward the sun.
    pub toward: [f32; 3],
    /// Illuminance at normal incidence, lux.
    pub illuminance: f32,
}

impl Default for BakeSun {
    fn default() -> Self {
        Self {
            toward: [0.3, 0.8, 0.5],
            illuminance: 50_000.0,
        }
    }
}

/// The command that runs a bake: `program prefix_args... bake-gi <desc> --out <volume> [--dry-run]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BakeCommand {
    /// The `regia` executable.
    pub program: PathBuf,
    /// Arguments placed before `bake-gi` (empty for the real command; tests run a shell).
    pub prefix_args: Vec<String>,
    /// The arguments after the prefix.
    pub args: Vec<String>,
}

impl BakeCommand {
    /// The `bake-gi` command for `desc`, writing `out`.
    #[must_use]
    pub fn bake_gi(program: &Path, prefix_args: &[String], desc: &Path, out: &Path, dry_run: bool) -> Self {
        let mut args = vec![
            "bake-gi".to_string(),
            desc.display().to_string(),
            "--out".to_string(),
            out.display().to_string(),
        ];
        if dry_run {
            args.push("--dry-run".to_string());
        }
        Self {
            program: program.to_path_buf(),
            prefix_args: prefix_args.to_vec(),
            args,
        }
    }
}

/// The `regia` executable next to the running editor, else `regia` on the path.
#[must_use]
pub fn default_program() -> PathBuf {
    let name = if cfg!(windows) { "regia.exe" } else { "regia" };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("regia"))
}

/// Parses a progress line of `regia bake-gi` (`  trace: 128/4096 probes`) into
/// `(stage, done, total)`.
#[must_use]
pub fn parse_progress_line(line: &str) -> Option<(String, usize, usize)> {
    let (stage, rest) = line.trim().split_once(':')?;
    let fraction = rest.trim().strip_suffix("probes")?.trim();
    let (done, total) = fraction.split_once('/')?;
    let stage = stage.trim();
    if stage.is_empty() {
        return None;
    }
    Some((
        stage.to_string(),
        done.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
}

/// Where a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobPhase {
    /// The child process runs.
    Running,
    /// It exited with success and reported `ok`.
    Succeeded,
    /// It failed to start, exited with an error or reported `ok: false`.
    Failed,
    /// The user cancelled it.
    Cancelled,
}

impl JobPhase {
    /// The lowercase name (`running`, `succeeded`, `failed`, `cancelled`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// What the reader thread has seen of a job.
#[derive(Clone, Debug)]
struct JobShared {
    phase: JobPhase,
    stage: String,
    done: usize,
    total: usize,
    stdout: String,
    stderr_tail: Vec<String>,
    exit_code: Option<i32>,
    spawn_error: Option<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means a reader thread panicked; its data is still the best we have.
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A running (or finished) `regia bake-gi` child process.
pub struct BakeJob {
    shared: Arc<Mutex<JobShared>>,
    child: Arc<Mutex<Option<Child>>>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The volume file the job writes.
    pub out: PathBuf,
    /// Whether this is a `--dry-run` (no trace, no file).
    pub dry_run: bool,
}

impl std::fmt::Debug for BakeJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BakeJob")
            .field("out", &self.out)
            .field("dry_run", &self.dry_run)
            .field("phase", &self.phase())
            .finish()
    }
}

impl BakeJob {
    /// Starts `command`.
    ///
    /// # Errors
    ///
    /// `bake_spawn_failed` when the program cannot be started.
    pub fn spawn(command: &BakeCommand, out: PathBuf, dry_run: bool) -> Result<Self, BakeError> {
        let mut child = Command::new(&command.program)
            .args(&command.prefix_args)
            .args(&command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                BakeError::new(
                    "bake_spawn_failed",
                    format!("cannot start `{}`: {e}", command.program.display()),
                )
            })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let shared = Arc::new(Mutex::new(JobShared {
            phase: JobPhase::Running,
            stage: String::new(),
            done: 0,
            total: 0,
            stdout: String::new(),
            stderr_tail: Vec::new(),
            exit_code: None,
            spawn_error: None,
        }));
        let child = Arc::new(Mutex::new(Some(child)));
        let cancel = Arc::new(AtomicBool::new(false));
        let thread = {
            let (shared, child, cancel) = (Arc::clone(&shared), Arc::clone(&child), Arc::clone(&cancel));
            std::thread::Builder::new()
                .name("bake-job".to_string())
                .spawn(move || run_reader(&shared, &child, &cancel, stdout, stderr))
                .map_err(|e| {
                    BakeError::new(
                        "bake_spawn_failed",
                        format!("cannot start the reader thread: {e}"),
                    )
                })
        };
        let thread = match thread {
            Ok(handle) => Some(handle),
            Err(err) => {
                if let Some(c) = lock(&child).as_mut() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                return Err(err);
            }
        };
        Ok(Self {
            shared,
            child,
            cancel,
            thread,
            out,
            dry_run,
        })
    }

    /// The job's phase now.
    #[must_use]
    pub fn phase(&self) -> JobPhase {
        let mut shared = lock(&self.shared);
        if shared.phase == JobPhase::Running && self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            // The reader sets the phase as its last step; a finished reader still `Running`
            // died on the way (a panic, a refused thread). Without this the job stayed
            // `Running` forever (audit 11-03 editor M7).
            shared.phase = JobPhase::Failed;
            if shared.spawn_error.is_none() {
                shared.spawn_error = Some("the bake reader thread stopped".to_string());
            }
        }
        shared.phase
    }

    /// `(stage, done, total)` of the last progress line.
    #[must_use]
    pub fn progress(&self) -> (String, usize, usize) {
        let s = lock(&self.shared);
        (s.stage.clone(), s.done, s.total)
    }

    /// Kills the child; the job ends `Cancelled`. Does nothing once it has finished.
    pub fn cancel(&self) {
        if let Some(child) = lock(&self.child).as_mut() {
            // Only a child that is still running counts as cancelled: one that just finished
            // successfully keeps its result.
            if matches!(child.try_wait(), Ok(None)) {
                self.cancel.store(true, Ordering::SeqCst);
                let _ = child.kill();
            }
        }
    }

    /// Blocks until the child has exited and its output was read.
    pub fn wait(&mut self) {
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }

    fn snapshot(&self) -> JobShared {
        lock(&self.shared).clone()
    }
}

impl Drop for BakeJob {
    fn drop(&mut self) {
        // A job dropped while running (the editor closes) must not leave the tracer behind.
        if self.phase() == JobPhase::Running {
            self.cancel();
        }
        self.wait();
    }
}

fn run_reader(
    shared: &Arc<Mutex<JobShared>>,
    child: &Arc<Mutex<Option<Child>>>,
    cancel: &Arc<AtomicBool>,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
) {
    let out_thread = stdout.and_then(|pipe| {
        let shared_out = Arc::clone(shared);
        let spawned = std::thread::Builder::new()
            .name("bake-job-stdout".to_string())
            .spawn(move || {
                lock(&shared_out).stdout = read_capped(pipe, MAX_STDOUT);
            });
        match spawned {
            Ok(handle) => Some(handle),
            Err(err) => {
                // The pipe is dropped: the child fails on its next stdout write instead of
                // blocking, and the job ends `Failed` below (audit 11-03 editor M7).
                lock(shared).spawn_error = Some(format!("cannot read the bake's output: {err}"));
                None
            }
        }
    });
    if let Some(pipe) = stderr {
        let mut reader = BufReader::new(pipe);
        let mut raw = Vec::new();
        // Read bytes, not `lines()`: a non-UTF-8 byte must not end the read and break the pipe.
        while matches!(reader.read_until(b'\n', &mut raw), Ok(n) if n > 0) {
            let line = String::from_utf8_lossy(&raw).trim_end().to_string();
            raw.clear();
            let mut s = lock(shared);
            if let Some((stage, done, total)) = parse_progress_line(&line) {
                s.stage = stage;
                s.done = done;
                s.total = total;
            } else if !line.trim().is_empty() {
                if s.stderr_tail.len() == STDERR_TAIL {
                    s.stderr_tail.remove(0);
                }
                s.stderr_tail.push(line);
            }
        }
    }
    if let Some(handle) = out_thread {
        let _ = handle.join();
    }
    // Poll rather than `wait()` so `cancel` can still take the child lock between checks.
    let status = loop {
        let polled = match lock(child).as_mut() {
            Some(c) => c.try_wait(),
            None => Ok(None),
        };
        match polled {
            Ok(Some(status)) => break Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    let mut s = lock(shared);
    s.exit_code = status.and_then(|st| st.code());
    let exited_ok = status.is_some_and(|st| st.success());
    let reported_ok = serde_json::from_str::<Value>(s.stdout.trim())
        .ok()
        .and_then(|v| v.get("ok").and_then(Value::as_bool));
    s.phase = if cancel.load(Ordering::SeqCst) {
        JobPhase::Cancelled
    } else if exited_ok && reported_ok != Some(false) && s.spawn_error.is_none() {
        JobPhase::Succeeded
    } else {
        JobPhase::Failed
    };
    if status.is_none() {
        s.spawn_error = Some("the bake process could not be waited for".to_string());
    }
}

/// What a finished bake left behind (kept until the next one starts).
#[derive(Clone, Debug, PartialEq)]
pub struct BakeOutcome {
    /// `succeeded`, `failed` or `cancelled`.
    pub phase: JobPhase,
    /// Whether it was a dry run.
    pub dry_run: bool,
    /// The volume file (`None` for a dry run or a failure).
    pub volume: Option<PathBuf>,
    /// Failure code (`bake_failed`, or the command's own code such as `bake_desc_invalid`).
    pub code: Option<String>,
    /// Failure message.
    pub message: Option<String>,
    /// The command's JSON summary, when it printed one.
    pub summary: Option<Value>,
    /// Whether the level now names the volume (`assign`).
    pub assigned: bool,
}

/// The Bake panel's state (a world resource; the panel, the gizmo and the agent share it).
#[derive(Resource, Debug)]
pub struct BakePanelState {
    /// The volume to bake.
    pub volume: BakeVolume,
    /// The trace settings.
    pub settings: BakeSettings,
    /// The sun to bake under when the level has no directional light.
    pub sun: BakeSun,
    /// Volume file; `None` = `<project>/assets/maps/<level>.fgiv`.
    pub out: Option<PathBuf>,
    /// Draw the volume in the viewport.
    pub show_gizmo: bool,
    /// Name the volume in the level's settings when a bake succeeds.
    pub assign: bool,
    /// The `regia` executable.
    pub program: PathBuf,
    /// Arguments before `bake-gi` (tests only).
    pub prefix_args: Vec<String>,
    /// The running job.
    pub job: Option<BakeJob>,
    /// The last finished job.
    pub last: Option<BakeOutcome>,
    /// A request from the UI or the agent to start next frame (`dry_run`).
    pub request: Option<bool>,
}

impl Default for BakePanelState {
    fn default() -> Self {
        Self {
            volume: BakeVolume::default(),
            settings: BakeSettings::from_quality(BakeQuality::Normal),
            sun: BakeSun::default(),
            out: None,
            show_gizmo: true,
            assign: false,
            program: default_program(),
            prefix_args: Vec::new(),
            job: None,
            last: None,
            request: None,
        }
    }
}

impl BakePanelState {
    /// Whether a bake is running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.phase() == JobPhase::Running)
    }

    /// Moves a finished job into [`Self::last`]; with `assign`, names the volume in the level.
    pub fn poll(&mut self, world: &mut World) {
        let Some(job) = self.job.as_ref() else {
            return;
        };
        if job.phase() == JobPhase::Running {
            return;
        }
        let Some(mut job) = self.job.take() else {
            return;
        };
        job.wait();
        let snap = job.snapshot();
        let summary = serde_json::from_str::<Value>(snap.stdout.trim()).ok();
        let (code, message) = match snap.phase {
            JobPhase::Failed => failure_of(&snap, summary.as_ref()),
            _ => (None, None),
        };
        let succeeded = snap.phase == JobPhase::Succeeded;
        let volume = (succeeded && !job.dry_run).then(|| job.out.clone());
        let mut assigned = false;
        if let (true, Some(path)) = (self.assign, volume.as_deref()) {
            assigned = assign_to_level(world, path);
            if assigned && let Some(mut undo) = world.get_resource_mut::<crate::command::UndoStack>() {
                undo.mark_dirty();
            }
        }
        self.last = Some(BakeOutcome {
            phase: snap.phase,
            dry_run: job.dry_run,
            volume,
            code,
            message,
            summary,
            assigned,
        });
    }

    /// Writes the description, starts the job and returns.
    ///
    /// # Errors
    ///
    /// A bake already running, no open project, an invalid volume or settings, an unwritable
    /// description or a program that cannot start.
    pub fn start(&mut self, world: &mut World, dry_run: bool) -> Result<(), BakeError> {
        self.poll(world);
        if self.running() {
            return Err(BakeError::new(
                "bake_busy",
                "a bake is already running; cancel it first",
            ));
        }
        self.volume.validate()?;
        self.settings.validate()?;
        let paths = bake_paths(world, self.out.as_deref())?;
        let (meshes, sun) = level_inputs(world, self.sun);
        let text = desc_ron(&self.volume, &self.settings, &sun, &meshes);
        write_atomically(&paths.desc, text.as_bytes()).map_err(|e| {
            BakeError::new(
                "bake_write_failed",
                format!("cannot write {}: {e}", paths.desc.display()),
            )
        })?;
        if !dry_run && let Some(dir) = paths.out.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                BakeError::new(
                    "bake_write_failed",
                    format!("cannot create {}: {e}", dir.display()),
                )
            })?;
        }
        let command =
            BakeCommand::bake_gi(&self.program, &self.prefix_args, &paths.desc, &paths.out, dry_run);
        self.job = Some(BakeJob::spawn(&command, paths.out, dry_run)?);
        self.last = None;
        Ok(())
    }

    /// Cancels the running job, if any.
    ///
    /// # Errors
    ///
    /// `bake_not_running` when no job runs.
    pub fn cancel(&mut self, world: &mut World) -> Result<(), BakeError> {
        if !self.running() {
            return Err(BakeError::new("bake_not_running", "no bake is running"));
        }
        if let Some(job) = self.job.as_mut() {
            job.cancel();
            job.wait();
        }
        self.poll(world);
        Ok(())
    }
}

fn failure_of(snap: &JobShared, summary: Option<&Value>) -> (Option<String>, Option<String>) {
    if let Some(error) = &snap.spawn_error {
        return (Some("bake_failed".to_string()), Some(error.clone()));
    }
    let code = summary
        .and_then(|v| v.get("code"))
        .and_then(Value::as_str)
        .map_or_else(|| "bake_failed".to_string(), str::to_string);
    let message = summary
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| (!snap.stderr_tail.is_empty()).then(|| snap.stderr_tail.join("\n")))
        .unwrap_or_else(|| match snap.exit_code {
            Some(c) => format!("`regia bake-gi` exited with code {c}"),
            None => "`regia bake-gi` was stopped by a signal".to_string(),
        });
    (Some(code), Some(message))
}

/// Names `volume` in the open level's settings (project-relative under `assets/`). Returns
/// whether the level was changed. The strip compares case-folded on Windows: the bake wrote
/// the volume through filesystem APIs that fold, so a case-variant spelling still names it
/// (audit G finding 5, ADR-4824).
fn assign_to_level(world: &mut World, volume: &Path) -> bool {
    let Some(root) = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
    else {
        return false;
    };
    let Some(relative) = crate::path_norm::strip_prefix_normalized(&root.join("assets"), volume) else {
        return false;
    };
    let name = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    let Some(mut level) = world.get_resource_mut::<LevelSettings>() else {
        return false;
    };
    let entry = level.look.baked_gi.get_or_insert_with(Default::default);
    entry.volume = name;
    true
}

/// The files of a bake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BakePaths {
    /// The `.bake.ron` the panel writes (`<project>/bake/<level>.editor.bake.ron`, outside
    /// `assets/` so it never ships).
    pub desc: PathBuf,
    /// The `.fgiv` the bake writes.
    pub out: PathBuf,
}

/// The description and volume paths for the open project and level.
///
/// # Errors
///
/// `bake_no_project` when no project is open.
pub fn bake_paths(world: &World, out: Option<&Path>) -> Result<BakePaths, BakeError> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| BakeError::new("bake_no_project", "open a project first (editor.open_project)"))?;
    let stem = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|l| l.path.as_deref())
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .map(|n| n.split('.').next().unwrap_or("level").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "level".to_string());
    let out = match out {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => root.join(p),
        None => root.join("assets").join("maps").join(format!("{stem}.fgiv")),
    };
    Ok(BakePaths {
        desc: root.join("bake").join(format!("{stem}.editor.bake.ron")),
        out,
    })
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::atomic_write::write_atomic(path, bytes).map_err(std::io::Error::other)
}

/// The bake's sun and static meshes from the edited level: the sun the live frame uses (the
/// lowest-entity `DirectionalLight` that is drawn, so a light hidden itself or under a hidden group
/// is skipped; else `fallback`), and every `GiMobility::Static` entity with a glTF `MeshSource`.
#[must_use]
pub fn level_inputs(world: &World, fallback: BakeSun) -> (Vec<BakeMesh>, BakeSun) {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    let sun = world
        .try_query::<(
            Entity,
            &DirectionalLight,
            regia_render::visibility::DrawVisibility,
        )>()
        .and_then(|mut q| {
            q.iter(world)
                .filter(|(.., inherited)| regia_render::visibility::is_drawn(*inherited))
                .min_by_key(|(entity, ..)| entity.to_bits())
                .map(|(_, light, _)| light.clone())
        })
        .map_or(fallback, |light| {
            let d = Vec3::from(light.direction);
            let toward = if d.length_squared() > 1e-12 {
                -d.normalize()
            } else {
                Vec3::Y
            };
            BakeSun {
                toward: toward.to_array(),
                illuminance: light.illuminance,
            }
        });
    let mut found: Vec<(Entity, String, [f32; 16])> = Vec::new();
    if let Some(mut query) = world.try_query::<(Entity, &MeshSource, &GiMobility, &GlobalTransform)>() {
        for (entity, source, mobility, global) in query.iter(world) {
            if *mobility != GiMobility::Static
                || source.mesh.starts_with("builtin:")
                || source.mesh.is_empty()
            {
                continue;
            }
            let path = crate::mesh_source::resolve_asset_path(root.as_deref(), &source.mesh);
            let matrix = regia_core::math::Mat4::from(global.0).to_cols_array();
            found.push((entity, path.to_string_lossy().replace('\\', "/"), matrix));
        }
    }
    found.sort_by_key(|(entity, _, _)| entity.to_bits());
    let meshes = found
        .into_iter()
        .map(|(entity, source, transform)| BakeMesh {
            id: format!(
                "{}#{}",
                crate::outliner::display_name(world, entity),
                entity.to_bits()
            ),
            source,
            transform,
        })
        .collect();
    (meshes, sun)
}

fn ron_string(s: &str) -> String {
    format!("{s:?}")
}

fn ron_f32(v: f32) -> String {
    format!("{v:?}")
}

fn ron_array(values: &[f32]) -> String {
    let parts: Vec<String> = values.iter().map(|v| ron_f32(*v)).collect();
    format!("({})", parts.join(", "))
}

/// The `.bake.ron` v1 text for a volume: the sun, the static meshes (as occluders and bounce
/// surfaces, layer `setpiece`) and one detail brick over the volume. Deterministic: the same
/// inputs give the same text.
#[must_use]
pub fn desc_ron(volume: &BakeVolume, settings: &BakeSettings, sun: &BakeSun, meshes: &[BakeMesh]) -> String {
    let mut out = String::new();
    out.push_str(
        "// Written by the Regia Engine editor's Bake panel. Edits are overwritten on the next bake.\n(\n",
    );
    out.push_str("    format_version: 1,\n");
    out.push_str(&format!(
        "    sun: Some((toward: {}, illuminance: {}, angular_radius: 0.00465)),\n",
        ron_array(&sun.toward),
        ron_f32(sun.illuminance)
    ));
    if meshes.is_empty() {
        out.push_str("    meshes: [],\n");
    } else {
        out.push_str("    meshes: [\n");
        for mesh in meshes {
            out.push_str(&format!(
                "        (id: Some({}), source: {}, transform: {}, layer: Some(\"setpiece\")),\n",
                ron_string(&mesh.id),
                ron_string(&mesh.source),
                ron_array(&mesh.transform)
            ));
        }
        out.push_str("    ],\n");
    }
    out.push_str("    layout: (\n");
    out.push_str(&format!(
        "        bricks: [(min: {}, max: {}, spacing: {})],\n",
        ron_array(&volume.min()),
        ron_array(&volume.max()),
        ron_f32(volume.spacing)
    ));
    out.push_str("    ),\n");
    out.push_str(&format!(
        "    bake: (rays_per_probe: {}, samples: {}, max_bounces: {}, seed: {}, filter: {}),\n",
        settings.rays_per_probe, settings.samples, settings.max_bounces, settings.seed, settings.filter
    ));
    out.push_str(")\n");
    out
}

/// The 12 edges of the volume box plus, when it is small enough, the probe lattice on its floor,
/// as world-space segments (the live viewport and the captures draw the same lines).
#[must_use]
pub fn bake_gizmo_segments(world: &World) -> Vec<Segment> {
    let Some(state) = world.get_resource::<BakePanelState>() else {
        return Vec::new();
    };
    if !state.show_gizmo || state.volume.validate().is_err() {
        return Vec::new();
    }
    volume_segments(&state.volume)
}

/// The segments of `volume` (see [`bake_gizmo_segments`]).
#[must_use]
pub fn volume_segments(volume: &BakeVolume) -> Vec<Segment> {
    let (lo, hi) = (volume.min(), volume.max());
    let corner = |i: usize| {
        Vec3::new(
            if i & 1 == 0 { lo[0] } else { hi[0] },
            if i & 2 == 0 { lo[1] } else { hi[1] },
            if i & 4 == 0 { lo[2] } else { hi[2] },
        )
    };
    let mut out = Vec::new();
    for i in 0..8usize {
        for bit in [1usize, 2, 4] {
            if i & bit == 0 {
                out.push((corner(i), corner(i | bit), BAKE_VOLUME));
            }
        }
    }
    let counts = volume.counts();
    if counts[0] <= LATTICE_MAX && counts[2] <= LATTICE_MAX {
        let at = |n: u32, count: u32, a: usize| lo[a] + (hi[a] - lo[a]) * n as f32 / (count - 1) as f32;
        for ix in 0..counts[0] {
            let x = at(ix, counts[0], 0);
            out.push((
                Vec3::new(x, lo[1], lo[2]),
                Vec3::new(x, lo[1], hi[2]),
                BAKE_LATTICE,
            ));
        }
        for iz in 0..counts[2] {
            let z = at(iz, counts[2], 2);
            out.push((
                Vec3::new(lo[0], lo[1], z),
                Vec3::new(hi[0], lo[1], z),
                BAKE_LATTICE,
            ));
        }
    }
    out
}

/// Fits `volume` around the world positions `points` plus `margin` metres on every side.
/// Returns `None` for no points.
#[must_use]
pub fn fit_to_points(points: &[Vec3], margin: f32, spacing: f32) -> Option<BakeVolume> {
    let first = *points.first()?;
    let (lo, hi) = points
        .iter()
        .fold((first, first), |(lo, hi), p| (lo.min(*p), hi.max(*p)));
    let size = (hi - lo) + Vec3::splat(2.0 * margin.max(0.0));
    let center = (lo + hi) * 0.5;
    Some(BakeVolume {
        center: center.to_array(),
        size: size.max(Vec3::splat(spacing.max(MIN_SPACING))).to_array(),
        spacing,
    })
}

/// World positions of the selected entities.
fn selection_points(world: &World) -> Vec<Vec3> {
    let selection = world
        .get_resource::<crate::selection::Selection>()
        .cloned()
        .unwrap_or_default();
    selection
        .entities()
        .iter()
        .filter_map(|e| crate::viewport::world_position(world, *e))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The panel.

/// Draws the Bake panel.
pub fn bake_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut state = world.remove_resource::<BakePanelState>().unwrap_or_default();
    state.poll(world);
    if let Some(dry_run) = state.request.take()
        && let Err(err) = state.start(world, dry_run)
    {
        state.last = Some(BakeOutcome {
            phase: JobPhase::Failed,
            dry_run,
            volume: None,
            code: Some(err.code),
            message: Some(err.message),
            summary: None,
            assigned: false,
        });
    }
    draw(ui, &mut state, world);
    if state.running() {
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
    world.insert_resource(state);
}

fn drag3(ui: &mut egui::Ui, label: &str, values: &mut [f32; 3], speed: f32, enabled: bool) {
    ui.horizontal(|ui| {
        ui.add_enabled_ui(enabled, |ui| {
            ui.label(label);
            for v in values.iter_mut() {
                ui.add(
                    egui::DragValue::new(v)
                        .speed(speed)
                        .range(-MAX_EXTENT..=MAX_EXTENT)
                        .suffix(" m"),
                );
            }
        });
    });
}

fn draw(ui: &mut egui::Ui, state: &mut BakePanelState, world: &mut World) {
    let editable = !state.running();
    ui.heading("Baked light");
    ui.checkbox(&mut state.show_gizmo, "Show the volume in the viewport");
    ui.separator();
    ui.label("Volume");
    drag3(ui, "Center", &mut state.volume.center, 0.25, editable);
    drag3(ui, "Size  ", &mut state.volume.size, 0.25, editable);
    ui.horizontal(|ui| {
        ui.add_enabled_ui(editable, |ui| {
            ui.label("Probe spacing");
            ui.add(
                egui::DragValue::new(&mut state.volume.spacing)
                    .speed(0.05)
                    .range(MIN_SPACING..=MAX_SPACING)
                    .suffix(" m"),
            );
            if ui.button("Fit to selection").clicked()
                && let Some(fit) = fit_to_points(&selection_points(world), 2.0, state.volume.spacing)
            {
                state.volume = fit;
            }
        });
    });
    let probes = state.volume.probe_count();
    let counts = state.volume.counts();
    match state.volume.validate() {
        Ok(()) => {
            ui.weak(format!(
                "{} x {} x {} = {probes} probes",
                counts[0], counts[1], counts[2]
            ));
        }
        Err(err) => {
            ui.colored_label(egui::Color32::from_rgb(230, 110, 90), err.message);
        }
    }
    ui.separator();
    ui.label("Quality");
    ui.add_enabled_ui(editable, |ui| {
        ui.horizontal(|ui| {
            for quality in BakeQuality::ALL {
                let on = state.settings.quality() == Some(quality);
                if ui.selectable_label(on, quality.name()).clicked() {
                    let (seed, filter) = (state.settings.seed, state.settings.filter);
                    state.settings = BakeSettings {
                        seed,
                        filter,
                        ..BakeSettings::from_quality(quality)
                    };
                }
            }
        });
        egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Rays");
                ui.add(egui::DragValue::new(&mut state.settings.rays_per_probe).range(1..=4096));
                ui.label("Samples");
                ui.add(egui::DragValue::new(&mut state.settings.samples).range(1..=1024));
                ui.label("Bounces");
                ui.add(egui::DragValue::new(&mut state.settings.max_bounces).range(1..=32));
            });
            ui.horizontal(|ui| {
                ui.label("Seed");
                ui.add(egui::DragValue::new(&mut state.settings.seed));
                ui.checkbox(&mut state.settings.filter, "Filter");
            });
            ui.checkbox(&mut state.assign, "Name the volume in the level when done");
        });
    });
    ui.separator();
    ui.horizontal(|ui| {
        if state.running() {
            if ui.button("Cancel").clicked() {
                let _ = state.cancel(world);
            }
        } else {
            let ready = state.volume.validate().is_ok() && state.settings.validate().is_ok();
            if ui.add_enabled(ready, egui::Button::new("Bake")).clicked() {
                state.request = Some(false);
            }
            if ui.add_enabled(ready, egui::Button::new("Dry run")).clicked() {
                state.request = Some(true);
            }
        }
    });
    if let Some(job) = &state.job {
        let (stage, done, total) = job.progress();
        let fraction = if total == 0 {
            0.0
        } else {
            done as f32 / total as f32
        };
        let text = if total == 0 {
            "starting".to_string()
        } else {
            format!("{stage}: {done}/{total} probes")
        };
        ui.add(egui::ProgressBar::new(fraction).text(text));
    }
    if let Some(last) = &state.last {
        match last.phase {
            JobPhase::Succeeded if last.dry_run => {
                ui.label("Dry run passed: the description is valid.");
            }
            JobPhase::Succeeded => {
                if let Some(volume) = &last.volume {
                    ui.label(format!("Baked {}", volume.display()));
                }
                if last.assigned {
                    ui.weak("The level now names this volume. Save the level.");
                }
            }
            JobPhase::Cancelled => {
                ui.label("Cancelled.");
            }
            _ => {
                let code = last.code.as_deref().unwrap_or("bake_failed");
                let message = last.message.as_deref().unwrap_or("");
                ui.colored_label(
                    egui::Color32::from_rgb(230, 110, 90),
                    format!("{code}: {message}"),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The agent surface.

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn num3(params: &Value, name: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    let list = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(list) {
        *slot = item
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` holds a non-number")))?
            as f32;
    }
    Ok(Some(out))
}

fn num1(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|n| Some(n as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is a number"))),
    }
}

fn count1(params: &Value, name: &str) -> Result<Option<u32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is a non-negative integer"))),
    }
}

fn state_json(world: &World, state: &BakePanelState) -> Value {
    let v = &state.volume;
    let s = &state.settings;
    let counts = v.counts();
    let job = state.job.as_ref().map(|j| {
        let (stage, done, total) = j.progress();
        json!({ "phase": j.phase().name(), "dry_run": j.dry_run, "stage": stage, "done": done, "total": total })
    });
    let last = state.last.as_ref().map(|o| {
        json!({
            "phase": o.phase.name(),
            "dry_run": o.dry_run,
            "volume": o.volume.as_ref().map(|p| p.display().to_string()),
            "code": o.code,
            "message": o.message,
            "summary": o.summary,
            "assigned": o.assigned,
        })
    });
    let paths = bake_paths(world, state.out.as_deref()).ok();
    json!({
        "volume": {
            "center": v.center,
            "size": v.size,
            "spacing": v.spacing,
            "min": v.min(),
            "max": v.max(),
            "counts": counts,
            "probes": v.probe_count(),
            "valid": v.validate().is_ok(),
            "error": v.validate().err().map(|e| e.code),
        },
        "quality": s.quality().map(BakeQuality::name),
        "settings": {
            "rays_per_probe": s.rays_per_probe,
            "samples": s.samples,
            "max_bounces": s.max_bounces,
            "seed": s.seed,
            "filter": s.filter,
        },
        "sun": { "toward": state.sun.toward, "illuminance": state.sun.illuminance },
        "show_gizmo": state.show_gizmo,
        "assign": state.assign,
        "running": state.running(),
        "job": job,
        "last": last,
        "desc": paths.as_ref().map(|p| p.desc.display().to_string()),
        "out": paths.as_ref().map(|p| p.out.display().to_string()),
    })
}

fn with_state<T>(
    world: &mut World,
    f: impl FnOnce(&mut World, &mut BakePanelState) -> Result<T, RpcError>,
) -> Result<T, RpcError> {
    let mut state = world.remove_resource::<BakePanelState>().unwrap_or_default();
    let result = f(world, &mut state);
    world.insert_resource(state);
    result
}

fn bake_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    with_state(world, |world, state| {
        state.poll(world);
        Ok(state_json(world, state))
    })
}

fn bake_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_state(world, |world, state| {
        state.poll(world);
        if state.running() {
            return Err(BakeError::new(
                "bake_busy",
                "a bake is running; cancel it before changing the settings",
            )
            .rpc());
        }
        let mut volume = state.volume;
        let mut settings = state.settings;
        if let Some(name) = params.get("quality") {
            let quality = name
                .as_str()
                .and_then(BakeQuality::from_name)
                .ok_or_else(|| RpcError::invalid_params("`quality` is draft, normal or high"))?;
            settings = BakeSettings {
                seed: settings.seed,
                filter: settings.filter,
                ..BakeSettings::from_quality(quality)
            };
        }
        if params.get("fit_selection").and_then(Value::as_bool) == Some(true) {
            let margin = num1(params, "margin")?.unwrap_or(2.0);
            let spacing = num1(params, "spacing")?.unwrap_or(volume.spacing);
            volume = fit_to_points(&selection_points(world), margin, spacing).ok_or_else(|| {
                BakeError::new(
                    "bake_selection_empty",
                    "select at least one entity to fit the volume to",
                )
                .rpc()
            })?;
        }
        if let Some(c) = num3(params, "center")? {
            volume.center = c;
        }
        if let Some(s) = num3(params, "size")? {
            volume.size = s;
        }
        if let Some(s) = num1(params, "spacing")? {
            volume.spacing = s;
        }
        if let Some(n) = count1(params, "rays_per_probe")? {
            settings.rays_per_probe = n;
        }
        if let Some(n) = count1(params, "samples")? {
            settings.samples = n;
        }
        if let Some(n) = count1(params, "max_bounces")? {
            settings.max_bounces = n;
        }
        if let Some(n) = count1(params, "seed")? {
            settings.seed = n;
        }
        if let Some(b) = params.get("filter").and_then(Value::as_bool) {
            settings.filter = b;
        }
        let mut sun_toward = state.sun.toward;
        let mut sun_illuminance = state.sun.illuminance;
        if let Some(toward) = num3(params, "sun_toward")? {
            sun_toward = toward;
        }
        if let Some(lux) = num1(params, "sun_illuminance")? {
            sun_illuminance = lux;
        }
        volume.validate().map_err(|e| e.rpc())?;
        settings.validate().map_err(|e| e.rpc())?;
        // Parsed before any write: a later refusal (a bad `out`) keeps the earlier fields.
        let show_gizmo = params
            .get("show_gizmo")
            .and_then(Value::as_bool)
            .unwrap_or(state.show_gizmo);
        let assign = params
            .get("assign")
            .and_then(Value::as_bool)
            .unwrap_or(state.assign);
        let out = if let Some(out) = params.get("out") {
            match out {
                Value::Null => None,
                // Under the agent's allowed roots only (ADR-1816).
                Value::String(s) if !s.is_empty() => {
                    Some(crate::agent_ext::allowed_request_path(world, "out", s)?)
                }
                _ => return Err(RpcError::invalid_params("`out` is a path or null")),
            }
        } else {
            state.out.clone()
        };
        state.volume = volume;
        state.settings = settings;
        state.sun.toward = sun_toward;
        state.sun.illuminance = sun_illuminance;
        state.show_gizmo = show_gizmo;
        state.assign = assign;
        state.out = out;
        Ok(state_json(world, state))
    })
}

fn bake_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_state(world, |world, state| {
        let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
        let wait = params.get("wait").and_then(Value::as_bool).unwrap_or(false);
        if let Some(b) = params.get("assign").and_then(Value::as_bool) {
            state.assign = b;
        }
        state.start(world, dry_run).map_err(|e| e.rpc())?;
        if wait {
            if let Some(job) = state.job.as_mut() {
                job.wait();
            }
            state.poll(world);
        }
        Ok(state_json(world, state))
    })
}

fn bake_cancel(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    with_state(world, |world, state| {
        state.cancel(world).map_err(|e| e.rpc())?;
        Ok(state_json(world, state))
    })
}

/// The Bake panel's agent methods: `editor.bake_get`, `editor.bake_set`, `editor.bake_start`,
/// `editor.bake_cancel`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    const SET_PARAMS: &[ParamSchema] = &[
        ParamSchema {
            name: "center",
            kind: "array",
            required: false,
            description: "Volume centre [x, y, z], world metres.",
        },
        ParamSchema {
            name: "size",
            kind: "array",
            required: false,
            description: "Volume extent [x, y, z], metres.",
        },
        ParamSchema {
            name: "spacing",
            kind: "number",
            required: false,
            description: "Probe spacing, metres (0.05 to 64).",
        },
        ParamSchema {
            name: "fit_selection",
            kind: "boolean",
            required: false,
            description: "Fit the volume around the selected entities' positions first; explicit center/size then override it.",
        },
        ParamSchema {
            name: "margin",
            kind: "number",
            required: false,
            description: "Margin of `fit_selection`, metres (default 2).",
        },
        ParamSchema {
            name: "quality",
            kind: "string",
            required: false,
            description: "draft | normal | high: sets rays, samples and bounces.",
        },
        ParamSchema {
            name: "rays_per_probe",
            kind: "number",
            required: false,
            description: "Rays per probe per sample (1 to 4096).",
        },
        ParamSchema {
            name: "samples",
            kind: "number",
            required: false,
            description: "Samples per probe (1 to 1024).",
        },
        ParamSchema {
            name: "max_bounces",
            kind: "number",
            required: false,
            description: "Path vertices (1 to 32).",
        },
        ParamSchema {
            name: "seed",
            kind: "number",
            required: false,
            description: "Bake seed.",
        },
        ParamSchema {
            name: "filter",
            kind: "boolean",
            required: false,
            description: "Run the 3x3x3 visibility-weighted filter.",
        },
        ParamSchema {
            name: "sun_toward",
            kind: "array",
            required: false,
            description: "Vector toward the sun, used when the level has no directional light.",
        },
        ParamSchema {
            name: "sun_illuminance",
            kind: "number",
            required: false,
            description: "Sun illuminance in lux, used when the level has no directional light.",
        },
        ParamSchema {
            name: "show_gizmo",
            kind: "boolean",
            required: false,
            description: "Draw the volume in the viewport.",
        },
        ParamSchema {
            name: "assign",
            kind: "boolean",
            required: false,
            description: "Name the volume in the level's settings when a bake succeeds.",
        },
        ParamSchema {
            name: "out",
            kind: "string",
            required: false,
            description: "Volume file (project-relative or absolute); null restores `assets/maps/<level>.fgiv`.",
        },
    ];
    vec![
        (
            MethodSchema {
                name: "editor.bake_get",
                tool: "bake_get",
                description: "The Bake panel's state: volume, settings, probe count, the running or last bake.",
                params: &[],
                result: "{ volume: {center, size, spacing, min, max, counts, probes, valid, error}, quality, settings, sun, running, job, last, desc, out }",
                mutating: false,
                permission: Permission::Read,
            },
            bake_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bake_set",
                tool: "bake_set",
                description: "Change the Bake panel's volume, quality and options. Refused while a bake runs; \
                    an invalid volume or setting is refused whole with `data.bake_error`.",
                params: SET_PARAMS,
                result: "the state of editor.bake_get",
                mutating: true,
                permission: Permission::EditScene,
            },
            bake_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bake_start",
                tool: "bake_start",
                description: "Write the bake description and run `regia bake-gi` on it. Returns at once \
                    (poll editor.bake_get) unless `wait`. `dry_run` validates without tracing.",
                params: &[
                    ParamSchema {
                        name: "dry_run",
                        kind: "boolean",
                        required: false,
                        description: "Validate the description and lay out the probes; no GPU, no file.",
                    },
                    ParamSchema {
                        name: "wait",
                        kind: "boolean",
                        required: false,
                        description: "Block until the bake ends (for dry runs and small bakes).",
                    },
                    ParamSchema {
                        name: "assign",
                        kind: "boolean",
                        required: false,
                        description: "Name the volume in the level's settings on success (not undoable; save the level).",
                    },
                ],
                result: "the state of editor.bake_get",
                mutating: true,
                permission: Permission::EditScene,
            },
            bake_start as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bake_cancel",
                tool: "bake_cancel",
                description: "Stop the running bake (the child process is killed; no volume is written).",
                params: &[],
                result: "the state of editor.bake_get",
                mutating: true,
                permission: Permission::EditScene,
            },
            bake_cancel as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
