// SPDX-License-Identifier: MPL-2.0
//! Install and update prepare step: warm the engine's shipped pipeline units headless,
//! then leave a stamp the launcher can trust.
//!
//! A game's first launch on a new GPU or driver compiles every pipeline in the driver
//! (seconds on a cold cache, much longer on some Windows machines). [`run`] does that
//! work before the player hits Play: a headless device, the same adapter order as the
//! game ([`crate::instance_backends`]; wgpu registers Vulkan before DX12), the
//! boot-tier thread budget ([`crate::boot_threads`]), and the pipeline units the runtime
//! already warms (the splash and the canonical lit frame, plus an optional profile
//! frame). It does not present a frame, so the queue stays in [`crate::WarmMode::Boot`].
//!
//! Two files land in the pipeline-cache directory:
//!
//! * `prepare-stamp.json` — written only after a successful warm and an explicit cache
//!   save. A later run with the same cache key and the same unit list skips the warm.
//! * `prepare-status.json` — rewritten as the warm goes (`starting`, `running`,
//!   `ready`, `failed`). A launcher polls it. A `running` file whose heartbeat is older
//!   than [`STATUS_STALE_AFTER`] reads as [`PrepareState::Stale`].
//!
//! Progress for a launcher UI is [`PrepareProgress::to_json`]: one JSON object, no
//! newline inside it. The `regia prepare` command prints one object per line on
//! stdout.
//!
//! A game's maps (ADR-8453): [`PrepareRequest::maps`] lists each map's render-relevant frames
//! ([`PrepareMap`], what the host's loader extracted: materials, vertex layouts, effects,
//! terrain), and [`PrepareRequest::targets`] the render targets the game draws into besides
//! the window. Each map frame is warmed the way the game-frame warm warms it
//! ([`crate::FrameRenderer::warm_extracted_at`] with a window), at the window formats, the
//! profile's settings and every registered target, on the warm workers. The map list (names
//! and the host's content fingerprints) is part of the stamp's unit list, so `--check` says
//! stale when the list or a map changes. A host reads [`host_status`] before its menu: when
//! [`HostPrepareStatus::needed`] it shows its "Preparing shaders" page and runs [`run_on`] on
//! its own device (or [`run`] on a headless one) instead of compiling in play.
//!
//! Not in this step (parked for their own lanes): carrying a pipeline-cache blob across
//! a driver change, saving the cache concurrently with the last compiles, and checking
//! packaged textures.
//!
//! Specification: the trimmed install prepare (roadmap item 6). The parked decision
//! list lived in `.claude/orchestration/tasks/prepare.md`, which is not in the git tree;
//! what shipped is the behavior named in `docs/roadmap-post-alpha.md` and
//! `docs/research/strategic-review-20261012.md`. See ADR-209.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::pipeline_cache::{self, PipelineCacheKey};
use crate::pipeline_warm::{self, PipelineWarmQueue, WarmTargetKey, WarmTier};
use crate::{RenderDevice, RenderError, RenderProfilePreset};

/// A `running` status whose `updated_unix` is older than this is [`PrepareState::Stale`].
///
/// The warm rewrites the file about five times a second. 45 s of silence means the
/// process stopped updating it (it exited, or it was killed). Portable: it does not
/// ask the OS whether the pid is alive.
pub const STATUS_STALE_AFTER: Duration = Duration::from_secs(45);

/// How long [`run`] waits for the warm, including deferred variants, unless the request
/// says otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8 * 60);

/// Stamp file name inside the pipeline-cache directory.
pub const STAMP_FILE: &str = "prepare-stamp.json";

/// Status file name inside the pipeline-cache directory.
pub const STATUS_FILE: &str = "prepare-status.json";

/// Stamp schema. Unknown versions do not count as current.
pub const STAMP_VERSION: u32 = 1;

/// Status schema.
pub const STATUS_VERSION: u32 = 1;

/// Swapchain formats the prepare step warms. Pipelines depend on the target format;
/// these two are the ones window surfaces actually hand out (BGRA on Windows, either
/// on Linux).
const WINDOW_FORMATS: [wgpu::TextureFormat; 2] = [
    wgpu::TextureFormat::Bgra8UnormSrgb,
    wgpu::TextureFormat::Rgba8UnormSrgb,
];

/// Where the prepare step stands, for the status file and the JSON lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepareState {
    /// The device is coming up. No warm job has started.
    Starting,
    /// Jobs are running.
    Running,
    /// The stamp is current. Either this run finished, or a previous one did and this
    /// run skipped the warm.
    Ready,
    /// A job failed, or the warm did not finish before the timeout. No stamp was written.
    Failed,
    /// The status file says `running` but its heartbeat is older than [`STATUS_STALE_AFTER`].
    Stale,
}

impl PrepareState {
    /// Stable name in the JSON (`starting`, `running`, `ready`, `failed`, `stale`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            "stale" => Self::Stale,
            _ => return None,
        })
    }
}

/// One progress sample. [`Self::to_json`] is the launcher line.
#[derive(Clone, Debug, PartialEq)]
pub struct PrepareProgress {
    /// Lifecycle state.
    pub state: PrepareState,
    /// Current unit label, or `done` / `current` / `failed`.
    pub phase: String,
    /// Warm jobs finished.
    pub done: u32,
    /// Warm jobs queued.
    pub total: u32,
    /// `done / total`, or `1.0` when there is nothing to do.
    pub fraction: f32,
    /// Pipelines created through the engine's tracked sites this process.
    pub pipelines: u64,
    /// Warm workers wanted (the Boot budget, unless the request overrode it).
    pub threads: usize,
    /// Adapter backend (`Vulkan`, `Dx12`, …). Empty before the device exists.
    pub backend: String,
    /// Short human phrase (the job label, or why the run stopped).
    pub message: String,
    /// Set when [`PrepareState::Failed`].
    pub error: Option<String>,
    /// This run found a current stamp and did not warm.
    pub skipped: bool,
}

impl PrepareProgress {
    /// One JSON object and no raw newline. A launcher reads a line at a time.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\"event\":\"prepare\",\"state\":\"{}\",\"phase\":\"{}\",\"done\":{},\"total\":{},\"fraction\":{:.4},\"pipelines\":{},\"threads\":{},\"backend\":\"{}\",\"message\":\"{}\",\"error\":{},\"skipped\":{}}}",
            self.state.name(),
            json_escape(&self.phase),
            self.done,
            self.total,
            self.fraction,
            self.pipelines,
            self.threads,
            json_escape(&self.backend),
            json_escape(&self.message),
            match &self.error {
                Some(err) => format!("\"{}\"", json_escape(err)),
                None => "null".to_string(),
            },
            if self.skipped { "true" } else { "false" },
        )
    }
}

/// What a finished prepare left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareReport {
    /// A current stamp was already on disk. No warm ran.
    pub skipped: bool,
    /// Pipelines created this process (0 when `skipped`, unless something else compiled).
    pub pipelines: u64,
    /// Workers the queue was set to. Boot ([`crate::boot_threads`]) unless overridden.
    /// A prepare run never presents, so this stays at the Boot count.
    pub threads: usize,
    /// Adapter backend.
    pub backend: String,
    /// The cache file, when the device installed one.
    pub cache_path: Option<String>,
    /// The stamp file.
    pub stamp_path: PathBuf,
    /// Wall time of [`run`], including a skip.
    pub elapsed_ms: u64,
    /// The unit list recorded in the stamp (same string [`PrepareStamp::units`] holds).
    pub units: String,
    /// Warm-worker time of the run, every tier summed (its thread time; 0 when `skipped`).
    pub worker_ms: u64,
}

/// A stamp file ([`STAMP_FILE`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareStamp {
    /// [`STAMP_VERSION`].
    pub version: u32,
    /// Written only after the warm and the cache save both succeeded.
    pub ok: bool,
    /// [`PipelineCacheKey::canonical`], or the same fields for a backend without a
    /// pipeline cache. Newlines included.
    pub key: String,
    /// Units this stamp covers, comma-separated, in warm order.
    pub units: String,
    /// Pipelines created during the warm that wrote the stamp.
    pub pipelines: u64,
    /// Warm workers used.
    pub threads: usize,
    /// Adapter backend name.
    pub backend: String,
    /// Seconds since the Unix epoch when the stamp was written.
    pub finished_unix: u64,
}

impl PrepareStamp {
    /// The stamp is for this key and this unit list, and it recorded a success.
    #[must_use]
    pub fn is_current(&self, key: &str, units: &str) -> bool {
        self.version == STAMP_VERSION && self.ok && self.key == key && self.units == units
    }
}

/// A status file ([`STATUS_FILE`]), after the stale-heartbeat rule.
#[derive(Clone, Debug, PartialEq)]
pub struct PrepareStatus {
    /// [`STATUS_VERSION`].
    pub version: u32,
    /// Interpreted state (`running` older than [`STATUS_STALE_AFTER`] is [`PrepareState::Stale`]).
    pub state: PrepareState,
    /// Pid that wrote the file.
    pub pid: u32,
    /// Seconds since the Unix epoch when the run started.
    pub started_unix: u64,
    /// Seconds since the Unix epoch of the last rewrite.
    pub updated_unix: u64,
    /// Last phase string.
    pub phase: String,
    /// Jobs finished at the last rewrite.
    pub done: u32,
    /// Jobs queued at the last rewrite.
    pub total: u32,
    /// Fraction at the last rewrite.
    pub fraction: f32,
    /// Pipelines at the last rewrite.
    pub pipelines: u64,
    /// Workers.
    pub threads: usize,
    /// Backend, possibly empty.
    pub backend: String,
    /// Failure text.
    pub error: Option<String>,
}

/// What [`run`] could not finish. The status file is `failed` when the failure happened
/// after the cache directory existed.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    /// `REGIA_PIPELINE_CACHE=0` (or no directory resolved).
    #[error("pipeline cache is disabled (REGIA_PIPELINE_CACHE=0)")]
    CacheDisabled,
    /// No adapter, or the device request failed.
    #[error("prepare device: {0}")]
    Device(#[from] RenderError),
    /// A warm job returned an error.
    #[error("prepare warm failed: {0}")]
    Warm(String),
    /// The timeout elapsed with jobs still queued.
    #[error("prepare timed out after {seconds}s ({pending})")]
    Timeout {
        /// The request's timeout, in seconds.
        seconds: u64,
        /// [`PipelineWarmQueue::describe`] at the deadline.
        pending: String,
    },
    /// The stamp or the status file could not be written.
    #[error("prepare could not write {}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// One map of the game, as its loader extracted it (ADR-8453).
///
/// `frames` are the map's render-relevant frames: what [`crate::extract_frame`] produced after
/// the host spawned the map and resolved its meshes, materials, effects and terrain (one frame
/// is usually enough; a map with areas of different settings passes one per area). Geometry
/// does not matter to a pipeline, only the vertex layout, the materials, the effects and the
/// frame's settings do, so a stand-in mesh per material is as good as the map's own.
///
/// `fingerprint` is the host's content hash of the map (its files, its cooked pack entry): it
/// goes into the stamp with `name`, so a changed map makes the stamp stale. A `--check` (or
/// [`host_status`]) needs only `name` and `fingerprint`; `frames` may be empty there.
#[derive(Clone, Default)]
pub struct PrepareMap {
    /// The map's name in the unit list and the progress lines (`map:<name>:...`). A `,` is
    /// written as `_`.
    pub name: String,
    /// Host content hash of the map; any change makes the stamp stale.
    pub fingerprint: String,
    /// Frames to warm. Empty: nothing of this map is warmed (a check-only request).
    pub frames: Vec<crate::ExtractedFrame>,
    /// Host warm steps for what a frame does not carry ([`Self::with_job`]): a renderer that
    /// draws without the ECS extraction, terrain, effects. Each runs once per window format on
    /// a warm worker, before the cache is saved.
    pub jobs: Vec<(String, PrepareJob)>,
}

/// A host warm step of a [`PrepareMap`]: builds what the map draws into a target of the given
/// format on the given device (on a warm worker; it may block). An `Err` fails the prepare.
pub type PrepareJob =
    std::sync::Arc<dyn Fn(&RenderDevice, wgpu::TextureFormat) -> Result<(), String> + Send + Sync>;

impl std::fmt::Debug for PrepareMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrepareMap")
            .field("name", &self.name)
            .field("fingerprint", &self.fingerprint)
            .field("frames", &self.frames.len())
            .field(
                "jobs",
                &self.jobs.iter().map(|(label, _)| label).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PrepareMap {
    /// A map with no frames yet.
    #[must_use]
    pub fn new(name: impl Into<String>, fingerprint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            fingerprint: fingerprint.into(),
            frames: Vec::new(),
            jobs: Vec::new(),
        }
    }

    /// Adds a host warm step (`label` names it in the progress, after `map:<name>:`).
    #[must_use]
    pub fn with_job(
        mut self,
        label: impl Into<String>,
        job: impl Fn(&RenderDevice, wgpu::TextureFormat) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.jobs.push((label.into(), std::sync::Arc::new(job)));
        self
    }

    /// Adds one frame to warm.
    #[must_use]
    pub fn with_frame(mut self, frame: crate::ExtractedFrame) -> Self {
        self.frames.push(frame);
        self
    }
}

/// Arguments of [`run`].
#[derive(Clone, Debug)]
pub struct PrepareRequest {
    /// Directory the pipeline cache (and the stamp and status) live in.
    ///
    /// `REGIA_PIPELINE_CACHE_DIR`, when set and non-empty, wins, the same way the
    /// windowed runtime resolves its cache. The stamp is written next to the cache
    /// that was actually used.
    pub cache_dir: PathBuf,
    /// Shipped `.fpc` directory (`content/pipelines` in a package). `None` keeps the
    /// process default ([`crate::pipeline_cache::set_shipped_directory`] is left alone).
    pub shipped_dir: Option<PathBuf>,
    /// Folded into the cache key before the device is created ([`crate::pipeline_cache::set_content_salt`]).
    /// A game update with new material graphs passes a new salt so the stamp misses.
    pub content_salt: String,
    /// Extra map-tier frame using this preset's post stack, depth prepass and upscaler.
    /// `None` warms the splash and the canonical lit frame only.
    pub profile: Option<RenderProfilePreset>,
    /// Warm workers. `None` is [`crate::WarmMode::Boot`] ([`crate::boot_threads`]).
    ///
    /// An explicit count is passed to [`PipelineWarmQueue::new`] and is not replaced:
    /// [`PipelineWarmQueue::set_mode`] would overwrite it with [`crate::boot_threads`].
    pub threads: Option<usize>,
    /// Lower this thread and the warm workers to background priority.
    pub nice: bool,
    /// When the stamp matches the key and the unit list, return without warming.
    pub skip_if_current: bool,
    /// Bound on the warm, including deferred variants.
    pub timeout: Duration,
    /// The game's maps (ADR-8453). Empty: the splash, canonical and profile frames only, with
    /// the unit list (and stamp) of a request without maps.
    pub maps: Vec<PrepareMap>,
    /// Render targets the game draws into besides the window (render-to-texture cameras, a
    /// second window, an MSAA count it can switch to). They are registered
    /// ([`crate::pipeline_warm::register_warm_target`]) before the warm, so every frame warm of
    /// this process renders into them too. Targets the host registered itself count as well.
    pub targets: Vec<WarmTargetKey>,
}

impl PrepareRequest {
    /// `cache_dir` set, nice on, skip when current, the default timeout, no profile.
    #[must_use]
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            shipped_dir: None,
            content_salt: String::new(),
            profile: None,
            threads: None,
            nice: true,
            skip_if_current: true,
            timeout: DEFAULT_TIMEOUT,
            maps: Vec::new(),
            targets: Vec::new(),
        }
    }

    /// The unit list this request warms and stamps (what [`PrepareStamp::units`] must equal).
    /// Includes the targets registered in this process at the time of the call.
    #[must_use]
    pub fn units(&self) -> String {
        unit_list(self.profile, &self.maps, &self.warm_targets())
    }

    /// [`Self::targets`] plus the process's registered targets, sorted and without repeats.
    fn warm_targets(&self) -> Vec<WarmTargetKey> {
        let mut targets: Vec<WarmTargetKey> = self
            .targets
            .iter()
            .copied()
            .chain(pipeline_warm::registered_warm_targets())
            .map(|key| WarmTargetKey {
                format: key.format,
                samples: key.samples.max(1),
            })
            .collect();
        targets.sort_by_key(|key| (target_label(key.format), key.samples));
        targets.dedup();
        targets
    }
}

/// The stamp path for `cache_dir`.
#[must_use]
pub fn stamp_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(STAMP_FILE)
}

/// The status path for `cache_dir`.
#[must_use]
pub fn status_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(STATUS_FILE)
}

/// Reads a stamp. `Ok(None)` when the file is absent. A file that is not our schema
/// is [`PrepareError::Io`] with an invalid-data source, so a torn write is not a match.
///
/// # Errors
///
/// The file could not be read, or it is not a version-1 stamp.
pub fn read_stamp(path: &Path) -> Result<Option<PrepareStamp>, PrepareError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_err(path, err)),
    };
    parse_stamp(&text).map(Some).ok_or_else(|| {
        io_err(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, "not a prepare stamp"),
        )
    })
}

/// Reads a status file and applies the stale-heartbeat rule. `Ok(None)` when absent.
///
/// # Errors
///
/// The file could not be read, or it is not a version-1 status.
pub fn read_status(path: &Path) -> Result<Option<PrepareStatus>, PrepareError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_err(path, err)),
    };
    let mut status = parse_status(&text).ok_or_else(|| {
        io_err(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, "not a prepare status"),
        )
    })?;
    if status.state == PrepareState::Running && heartbeat_stale(status.updated_unix) {
        status.state = PrepareState::Stale;
    }
    Ok(Some(status))
}

/// Whether the stamp in the resolved cache directory matches this machine and this
/// request's unit list. Opens a headless device to read the adapter key and does not warm.
///
/// # Errors
///
/// See [`PrepareError::CacheDisabled`], [`PrepareError::Device`] and [`PrepareError::Io`].
pub fn stamp_is_current(request: &PrepareRequest) -> Result<bool, PrepareError> {
    let opened = open_device(request)?;
    let stamp = read_stamp(&stamp_path(&opened.cache_dir))?;
    Ok(stamp.is_some_and(|stamp| stamp.is_current(&opened.key, &opened.units)) && cache_file_present())
}

/// What a host reads before it shows its menu (ADR-8453): whether this GPU, driver, unit list
/// and map list still need a prepare, and how far a running one is.
///
/// ```
/// use regia_render::prepare::HostPrepareStatus;
/// let status = HostPrepareStatus { needed: true, state: None, done: 0, total: 0, eta: None };
/// assert!(status.needed && status.fraction() == 0.0);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct HostPrepareStatus {
    /// The stamp does not match this GPU, driver, unit list or map list (or the cache file is
    /// gone): show the "Preparing shaders" page and run [`run_on`] (or `regia prepare`).
    pub needed: bool,
    /// The status file's state (`None`: no prepare has written one in this cache directory).
    pub state: Option<PrepareState>,
    /// Warm jobs finished, from the status file.
    pub done: u32,
    /// Warm jobs queued, from the status file.
    pub total: u32,
    /// Time left at the rate so far, while a prepare runs and has finished a job; `None`
    /// otherwise.
    pub eta: Option<Duration>,
}

impl HostPrepareStatus {
    /// `done / total`; `1.0` when nothing is needed, `0.0` before the first job.
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if !self.needed {
            1.0
        } else if self.total == 0 {
            0.0
        } else {
            self.done as f32 / self.total as f32
        }
    }
}

/// [`HostPrepareStatus`] for `request` on the adapter `adapter` (the host's device:
/// `device.adapter.get_info()`). Reads the stamp and the status file in the resolved cache
/// directory; opens no device and warms nothing. A running prepare in this or another process
/// shows its `done`/`total` and an `eta`.
///
/// # Errors
///
/// [`PrepareError::CacheDisabled`] (`REGIA_PIPELINE_CACHE=0`), or a stamp or status file that
/// could not be read.
pub fn host_status(
    request: &PrepareRequest,
    adapter: &wgpu::AdapterInfo,
) -> Result<HostPrepareStatus, PrepareError> {
    let dir = resolved_cache_dir(request).ok_or(PrepareError::CacheDisabled)?;
    let key = cache_key_text(adapter);
    let units = request.units();
    let current = read_stamp(&stamp_path(&dir))?.is_some_and(|stamp| stamp.is_current(&key, &units))
        && cache_file_present();
    // A torn or foreign status file reads as no status: the stamp decides `needed`.
    let status = read_status(&status_path(&dir)).ok().flatten();
    let state = status.as_ref().map(|status| status.state);
    let (done, total) = status
        .as_ref()
        .map_or((0, 0), |status| (status.done, status.total));
    let eta = status
        .as_ref()
        .filter(|status| !current && status.state == PrepareState::Running && status.done > 0)
        .map(|status| {
            eta_of(
                status.done,
                status.total,
                status.updated_unix.saturating_sub(status.started_unix),
            )
        });
    Ok(HostPrepareStatus {
        needed: !current,
        state,
        done,
        total,
        eta,
    })
}

/// Time left for `total - done` jobs when `done` took `elapsed_secs`.
fn eta_of(done: u32, total: u32, elapsed_secs: u64) -> Duration {
    let left = f64::from(total.saturating_sub(done));
    Duration::from_secs_f64(elapsed_secs as f64 * left / f64::from(done.max(1)))
}

/// Whether the installed pipeline cache has a non-empty file on disk. A stamp without
/// one is stale: the player (or a cleaner) removed the blob and Play would compile cold.
/// A backend with no pipeline cache has no file to lose, so it counts as present.
fn cache_file_present() -> bool {
    match pipeline_cache::status().path {
        Some(path) => std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0),
        None => true,
    }
}

/// Warms the shipped units and writes the stamp.
///
/// `on_progress` is called at the start, about every 200 ms during the warm, and once
/// at the end. It must not build pipelines of its own.
///
/// # Errors
///
/// See [`PrepareError`]. A failure after the status file exists leaves `state: failed`
/// and does not write a successful stamp. [`PrepareRequest::skip_if_current`] is the
/// exception: a matching stamp returns [`PrepareReport::skipped`] and does not warm.
pub fn run(
    request: &PrepareRequest,
    on_progress: impl FnMut(&PrepareProgress),
) -> Result<PrepareReport, PrepareError> {
    let _span = tracing::info_span!("prepare_step").entered();
    let started = Instant::now();
    check_maps(request)?;
    let opened = open_device(request)?;
    run_opened(request, opened, started, on_progress)
}

/// [`run`] on the host's own device (ADR-8453): a game's "Preparing shaders" page at first
/// launch or after a driver change. The pipelines land in the driver cache the device
/// installed (saved before the stamp is written) and in the device's shared pipelines, so the
/// game's own warm of the same map then builds from cache.
///
/// The device keeps the cache directory and content salt it was created with
/// ([`crate::pipeline_cache::set_directory`] and [`crate::pipeline_cache::set_content_salt`]
/// before the device); `request.cache_dir` (after `REGIA_PIPELINE_CACHE_DIR`) is where the
/// stamp and status go, so pass the same directory. `request.shipped_dir` and
/// `request.content_salt` are not applied here. Call it off the frame thread: it waits for the
/// warm.
///
/// # Errors
///
/// As [`run`], except that no device is opened.
pub fn run_on(
    device: &RenderDevice,
    request: &PrepareRequest,
    on_progress: impl FnMut(&PrepareProgress),
) -> Result<PrepareReport, PrepareError> {
    let _span = tracing::info_span!("prepare_step_on_device").entered();
    let started = Instant::now();
    check_maps(request)?;
    if request.nice {
        let _ = regia_platform::background::lower_current_thread_priority();
    }
    let info = device.adapter.get_info();
    let opened = Opened {
        backend: format!("{:?}", info.backend),
        key: cache_key_text(&info),
        units: request.units(),
        threads: request.threads.unwrap_or_else(crate::boot_threads),
        cache_dir: resolved_cache_dir(request).ok_or(PrepareError::CacheDisabled)?,
        device: device.clone(),
    };
    std::fs::create_dir_all(&opened.cache_dir).map_err(|err| io_err(&opened.cache_dir, err))?;
    run_opened(request, opened, started, on_progress)
}

/// A map frame with no window view cannot be warmed: refuse before any device work.
fn check_maps(request: &PrepareRequest) -> Result<(), PrepareError> {
    for map in &request.maps {
        if let Some(index) = map
            .frames
            .iter()
            .position(|frame| frame.window_views().is_empty())
        {
            return Err(PrepareError::Warm(format!(
                "map {}: frame {index} has no window view",
                map.name
            )));
        }
    }
    Ok(())
}

fn run_opened(
    request: &PrepareRequest,
    opened: Opened,
    started: Instant,
    on_progress: impl FnMut(&PrepareProgress),
) -> Result<PrepareReport, PrepareError> {
    // Every sample also goes to `last_progress` (the agent's `render.pipeline_warm`).
    let mut user = on_progress;
    let mut on_progress = move |progress: &PrepareProgress| {
        record_progress(progress, started);
        user(progress);
    };
    let cache_dir = opened.cache_dir.clone();
    let key = opened.key.clone();
    let units = opened.units.clone();
    let device = opened.device;
    let stamp_file = stamp_path(&cache_dir);
    let status_file = status_path(&cache_dir);
    let mut progress = PrepareProgress {
        state: PrepareState::Starting,
        phase: "device".to_string(),
        done: 0,
        total: 0,
        fraction: 0.0,
        pipelines: pipeline_cache::pipelines_created(),
        threads: opened.threads,
        backend: opened.backend.clone(),
        message: "headless device ready".to_string(),
        error: None,
        skipped: false,
    };
    write_status(&status_file, &progress)?;
    on_progress(&progress);

    if request.skip_if_current
        && read_stamp(&stamp_file)?.is_some_and(|stamp| stamp.is_current(&key, &units))
        && cache_file_present()
    {
        progress.state = PrepareState::Ready;
        progress.phase = "current".to_string();
        progress.fraction = 1.0;
        progress.skipped = true;
        progress.message = "stamp matches this GPU, driver and unit list".to_string();
        write_status(&status_file, &progress)?;
        on_progress(&progress);
        tracing::info!(
            target: "regia_render",
            "prepare: stamp current, warm skipped ({})",
            stamp_file.display()
        );
        return Ok(finish_report(
            &progress,
            &stamp_file,
            (started, 0),
            true,
            &units,
            &device,
        ));
    }
    if !request.skip_if_current {
        // A forced run must not leave the previous success in place if this process dies
        // mid-warm: the next launch would skip onto a half-written cache.
        match std::fs::remove_file(&stamp_file) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(fail(
                    &status_file,
                    &mut progress,
                    io_err(&stamp_file, err),
                    &mut on_progress,
                ));
            }
        }
    }

    // Before the queue starts: every frame warm of this process renders into them as well.
    for key in &request.targets {
        pipeline_warm::register_warm_target(key.format, key.samples);
    }
    let threads = progress.threads;
    let queue = PipelineWarmQueue::new(threads);
    // Stay on this count. `set_mode(Boot)` would replace an explicit override with
    // `boot_threads()`, and `note_scene_frame` would drop the queue to Play. Prepare
    // does not present, so nothing else calls it.
    queue.set_background_priority(request.nice);
    let splash_format = WINDOW_FORMATS[0];
    queue.push(WarmTier::Boot, "splash", splash_job(splash_format));
    for (index, format) in WINDOW_FORMATS.into_iter().enumerate() {
        let tier = if index == 0 {
            WarmTier::Boot
        } else {
            WarmTier::Menu
        };
        let label = format!("canonical:{}", format_label(format));
        queue.push(tier, label, pipeline_warm::frame_job(canonical_for(None), format));
    }
    if let Some(preset) = request.profile {
        for format in WINDOW_FORMATS {
            let label = format!("profile:{}:{}", preset.name(), format_label(format));
            queue.push(
                WarmTier::Map,
                label,
                pipeline_warm::frame_job(canonical_for(Some(preset)), format),
            );
        }
    }
    push_map_jobs(&queue, request);
    queue.start(&device);
    let threads = queue.threads();
    progress.threads = threads;
    progress.total = queue.progress().total;
    progress.state = PrepareState::Running;
    progress.phase = "warm".to_string();
    progress.message = format!("boot workers {threads}");
    if let Err(err) = write_status(&status_file, &progress) {
        queue.shutdown();
        return Err(fail(&status_file, &mut progress, err, &mut on_progress));
    }
    on_progress(&progress);

    let warmed = wait_until(
        &queue,
        request.timeout,
        &status_file,
        &mut progress,
        &mut on_progress,
    );
    if let Err(err) = warmed {
        queue.shutdown();
        return Err(err);
    }
    // Deferred variants (blend and glass the canonical frame did not draw) build on the
    // same workers once the queued jobs are done. `take_next` runs first, so they never
    // jump ahead of the splash and the frames.
    pipeline_warm::release_deferred();
    if let Err(err) = wait_deferred(
        &queue,
        // The deferred phase keeps a floor so a warm that used the whole budget does not
        // fail the run for want of a few blend and glass variants.
        request
            .timeout
            .saturating_sub(started.elapsed())
            .max(DEFERRED_MIN_WAIT),
        &status_file,
        &mut progress,
        &mut on_progress,
    ) {
        queue.shutdown();
        return Err(err);
    }

    let failed = queue.progress().tiers.iter().map(|t| t.jobs_failed).sum::<u32>();
    if failed > 0 {
        let message = queue
            .progress()
            .last_error
            .unwrap_or_else(|| format!("{failed} warm jobs failed"));
        queue.shutdown();
        return Err(fail(
            &status_file,
            &mut progress,
            PrepareError::Warm(message),
            &mut on_progress,
        ));
    }

    if let Some(Err(err)) = pipeline_cache::save_installed() {
        queue.shutdown();
        return Err(fail(
            &status_file,
            &mut progress,
            io_err(Path::new("pipeline cache"), err),
            &mut on_progress,
        ));
    }
    // After the warm: a Play switch (which this step must not do) would show up here.
    let threads = queue.threads();
    progress.threads = threads;
    let pipelines = pipeline_cache::pipelines_created();
    let stamp = PrepareStamp {
        version: STAMP_VERSION,
        ok: true,
        key,
        units: units.clone(),
        pipelines,
        threads,
        backend: progress.backend.clone(),
        finished_unix: unix_now(),
    };
    if let Err(err) = write_stamp(&stamp_file, &stamp) {
        queue.shutdown();
        return Err(fail(&status_file, &mut progress, err, &mut on_progress));
    }
    progress.state = PrepareState::Ready;
    progress.phase = "done".to_string();
    progress.done = progress.total;
    progress.fraction = 1.0;
    progress.pipelines = pipelines;
    progress.message = format!("{pipelines} pipelines, stamp {}", stamp_file.display());
    progress.error = None;
    progress.skipped = false;
    // The stamp is already valid: a failed status write must not turn success into failure.
    if let Err(err) = write_status(&status_file, &progress) {
        tracing::warn!(target: "regia_render", "prepare: final status write failed: {err}");
    }
    on_progress(&progress);
    tracing::info!(
        target: "regia_render",
        "prepare: ready, {pipelines} pipelines, {} workers, {}",
        threads,
        stamp_file.display()
    );
    let worker_ms = queue
        .progress()
        .tiers
        .iter()
        .map(|tier| f64::from(tier.busy_ms))
        .sum::<f64>()
        .round() as u64;
    let report = finish_report(
        &progress,
        &stamp_file,
        (started, worker_ms),
        false,
        &units,
        &device,
    );
    queue.shutdown();
    Ok(report)
}

/// The newest progress sample of a prepare run in this process, with when that run started.
static LAST_PROGRESS: std::sync::Mutex<Option<(PrepareProgress, Instant)>> = std::sync::Mutex::new(None);

fn record_progress(progress: &PrepareProgress, started: Instant) {
    if let Ok(mut last) = LAST_PROGRESS.lock() {
        *last = Some((progress.clone(), started));
    }
}

/// The newest progress of a [`run`] or [`run_on`] in this process (ADR-8453), with the time
/// since that run started; `None` when none ran. The agent's `render.pipeline_warm` reports it
/// as `prepare`, so a host's "Preparing shaders" page can be watched like the warm tiers.
#[must_use]
pub fn last_progress() -> Option<(PrepareProgress, Duration)> {
    LAST_PROGRESS.lock().ok().and_then(|last| {
        last.as_ref()
            .map(|(progress, started)| (progress.clone(), started.elapsed()))
    })
}

/// Time left at the rate of `progress` after `elapsed`: `None` before the first job, after the
/// run ended, or when it failed.
#[must_use]
pub fn eta(progress: &PrepareProgress, elapsed: Duration) -> Option<Duration> {
    if progress.state != PrepareState::Running || progress.done == 0 {
        return None;
    }
    let left = f64::from(progress.total.saturating_sub(progress.done));
    Some(Duration::from_secs_f64(
        elapsed.as_secs_f64() * left / f64::from(progress.done),
    ))
}

/// The first running map job's map name in a [`PipelineWarmQueue::describe`] line
/// (`running [map] map:<name>:<format> (#3) 1.2 s`).
fn current_map(describe: &str) -> Option<&str> {
    describe.split("; running [").skip(1).find_map(|job| {
        let label = job.split_once("] ")?.1;
        let label = label.split_once(" (#").map_or(label, |(label, _)| label);
        let rest = label.strip_prefix("map:")?;
        Some(rest.split(':').next().unwrap_or(rest))
    })
}

/// Least time the deferred-variant wait gets, whatever the main warm used.
const DEFERRED_MIN_WAIT: Duration = Duration::from_secs(60);

/// Window size a map warm allocates its window-size frame at. Pipelines do not depend on the
/// size; the window path is what builds the game-frame warm's extras (TSR, the depth match,
/// the deferred blend and transmission variants, ADR-7885), which a game builds for its map.
const MAP_WINDOW: (u32, u32) = (128, 72);

/// One job per map frame, window format and (with a profile) profiled copy, in map order.
fn push_map_jobs(queue: &PipelineWarmQueue, request: &PrepareRequest) {
    for map in &request.maps {
        let name = unit_name(&map.name);
        for (index, frame) in map.frames.iter().enumerate() {
            let tag = if map.frames.len() > 1 {
                format!("{name}#{index}")
            } else {
                name.clone()
            };
            for format in WINDOW_FORMATS {
                queue.push(
                    WarmTier::Map,
                    format!("map:{tag}:{}", format_label(format)),
                    map_job(frame.clone(), format),
                );
                if let Some(preset) = request.profile {
                    queue.push(
                        WarmTier::Map,
                        format!("map:{tag}:{}:{}", preset.name(), format_label(format)),
                        map_job(with_profile(frame.clone(), preset), format),
                    );
                }
            }
        }
        for (label, job) in &map.jobs {
            for format in WINDOW_FORMATS {
                let job = std::sync::Arc::clone(job);
                queue.push(
                    WarmTier::Map,
                    format!("map:{name}:{}:{}", unit_name(label), format_label(format)),
                    move |device: &RenderDevice| {
                        let _span = tracing::info_span!("prepare_map_job").entered();
                        job(device, format)
                    },
                );
            }
        }
    }
}

/// Warms one map frame the way the game-frame warm does, then waits for its material graphs:
/// a prepare saves the cache once, after every job, so a graph left building would be compiled
/// in play.
fn map_job(
    frame: crate::ExtractedFrame,
    format: wgpu::TextureFormat,
) -> impl FnOnce(&RenderDevice) -> Result<(), String> + Send + 'static {
    move |device: &RenderDevice| {
        let _span = tracing::info_span!("prepare_map").entered();
        let mut renderer = crate::FrameRenderer::new();
        renderer
            .warm_extracted_at(device, &frame, format, false, Some(MAP_WINDOW))
            .map_err(|e| e.to_string())?;
        renderer.settle_material_graphs();
        Ok(())
    }
}

/// `REGIA_PIPELINE_CACHE_DIR` (non-empty) or the request's directory; `None` when
/// `REGIA_PIPELINE_CACHE=0`. What [`run`] ends up with after it sets the directory.
fn resolved_cache_dir(request: &PrepareRequest) -> Option<PathBuf> {
    if regia_core::brand::env_var("REGIA_PIPELINE_CACHE").ok().as_deref() == Some("0") {
        return None;
    }
    let env = regia_core::brand::env_var_os("REGIA_PIPELINE_CACHE_DIR").filter(|dir| !dir.is_empty());
    Some(env.map_or_else(|| request.cache_dir.clone(), PathBuf::from))
}

struct Opened {
    device: RenderDevice,
    cache_dir: PathBuf,
    key: String,
    units: String,
    backend: String,
    threads: usize,
}

fn open_device(request: &PrepareRequest) -> Result<Opened, PrepareError> {
    if request.nice {
        let _ = regia_platform::background::lower_current_thread_priority();
    }
    pipeline_cache::set_content_salt(&request.content_salt);
    if let Some(dir) = &request.shipped_dir {
        pipeline_cache::set_shipped_directory(Some(dir.clone()));
    }
    pipeline_cache::set_directory(Some(request.cache_dir.clone()));
    let Some(cache_dir) = pipeline_cache::directory() else {
        return Err(PrepareError::CacheDisabled);
    };
    if cache_dir != request.cache_dir {
        tracing::warn!(
            target: "regia_render",
            "prepare cache dir {} overridden by REGIA_PIPELINE_CACHE_DIR ({})",
            request.cache_dir.display(),
            cache_dir.display()
        );
    }
    let device = RenderDevice::headless()?;
    let info = device.adapter.get_info();
    Ok(Opened {
        backend: format!("{:?}", info.backend),
        key: cache_key_text(&info),
        units: request.units(),
        threads: request.threads.unwrap_or_else(crate::boot_threads),
        cache_dir,
        device,
    })
}

fn finish_report(
    progress: &PrepareProgress,
    stamp_file: &Path,
    (started, worker_ms): (Instant, u64),
    skipped: bool,
    units: &str,
    device: &RenderDevice,
) -> PrepareReport {
    // The cache is installed on this device; read its path before the caller drops it.
    let _ = device.device_id();
    PrepareReport {
        skipped,
        pipelines: progress.pipelines,
        threads: progress.threads,
        backend: progress.backend.clone(),
        cache_path: pipeline_cache::status().path,
        stamp_path: stamp_file.to_path_buf(),
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        units: units.to_string(),
        worker_ms,
    }
}

fn fail(
    status_file: &Path,
    progress: &mut PrepareProgress,
    err: PrepareError,
    on_progress: &mut impl FnMut(&PrepareProgress),
) -> PrepareError {
    progress.state = PrepareState::Failed;
    progress.phase = "failed".to_string();
    progress.message = err.to_string();
    progress.error = Some(err.to_string());
    if let Err(write_err) = write_status(status_file, progress) {
        tracing::warn!(target: "regia_render", "prepare status write failed: {write_err}");
    }
    on_progress(progress);
    err
}

fn wait_until(
    queue: &PipelineWarmQueue,
    timeout: Duration,
    status_file: &Path,
    progress: &mut PrepareProgress,
    on_progress: &mut impl FnMut(&PrepareProgress),
) -> Result<(), PrepareError> {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = queue.progress();
        progress.state = PrepareState::Running;
        progress.done = snap.done;
        progress.total = snap.total;
        progress.fraction = snap.fraction();
        progress.pipelines = pipeline_cache::pipelines_created();
        let describe = queue.describe();
        // The map being warmed names the phase (`map:<name>`), else the unfinished tier.
        progress.phase = current_map(&describe).map_or_else(
            || {
                snap.tiers
                    .iter()
                    .find(|t| t.jobs_done < t.jobs_total)
                    .map(|t| t.tier.to_string())
                    .unwrap_or_else(|| "warm".to_string())
            },
            |map| format!("map:{map}"),
        );
        progress.message = describe;
        if let Err(err) = write_status(status_file, progress) {
            return Err(fail(status_file, progress, err, on_progress));
        }
        on_progress(progress);
        if queue.is_tier_ready(WarmTier::Map) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(fail(
                status_file,
                progress,
                PrepareError::Timeout {
                    seconds: timeout.as_secs(),
                    pending: queue.describe(),
                },
                on_progress,
            ));
        }
        let slice = Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now()));
        let _ = queue.wait_tier(WarmTier::Map, slice);
    }
}

fn wait_deferred(
    queue: &PipelineWarmQueue,
    timeout: Duration,
    status_file: &Path,
    progress: &mut PrepareProgress,
    on_progress: &mut impl FnMut(&PrepareProgress),
) -> Result<(), PrepareError> {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = queue.progress();
        progress.phase = "deferred".to_string();
        progress.pipelines = pipeline_cache::pipelines_created();
        progress.message = format!("{} deferred left", snap.deferred_pending);
        if let Err(err) = write_status(status_file, progress) {
            return Err(fail(status_file, progress, err, on_progress));
        }
        on_progress(progress);
        if snap.deferred_pending == 0 && snap.deferred_released {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(fail(
                status_file,
                progress,
                PrepareError::Timeout {
                    seconds: timeout.as_secs(),
                    pending: queue.describe(),
                },
                on_progress,
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn splash_job(
    format: wgpu::TextureFormat,
) -> impl FnOnce(&RenderDevice) -> Result<(), String> + Send + 'static {
    move |device: &RenderDevice| {
        let _span = tracing::info_span!("prepare_splash").entered();
        let size = pipeline_warm::WARM_FRAME_SIZE;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("prepare splash"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut splash = crate::BootSplash::new().with_text("REGIA");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("prepare splash"),
            });
        splash.record(
            device,
            &mut encoder,
            &view,
            format,
            (size, size),
            crate::SplashPhase::Showing { opacity: 1.0 },
        );
        device.queue.submit([encoder.finish()]);
        device.poll_nonblocking().map_err(|e| e.to_string())
    }
}

/// The canonical lit frame, with `preset`'s post stack when set.
fn canonical_for(preset: Option<RenderProfilePreset>) -> crate::ExtractedFrame {
    let frame = pipeline_warm::canonical_frame();
    match preset {
        Some(preset) => with_profile(frame, preset),
        None => frame,
    }
}

/// `frame` with `preset`'s post stack (every window view), depth prepass, fog, quality and
/// temporal upscaler.
fn with_profile(mut frame: crate::ExtractedFrame, preset: RenderProfilePreset) -> crate::ExtractedFrame {
    let settings = crate::RenderSettings::from_profile(preset);
    let post = settings.resolve_post();
    for view in &mut frame.views {
        view.post = post.clone();
    }
    frame.depth_prepass = settings.profile.depth_prepass;
    frame.fog = settings.fog;
    frame.quality = settings.quality();
    frame.temporal_upscaler = settings.profile.temporal_upscaler;
    frame
}

fn unit_list(profile: Option<RenderProfilePreset>, maps: &[PrepareMap], targets: &[WarmTargetKey]) -> String {
    let mut units = vec![format!("splash:{}", format_label(WINDOW_FORMATS[0]))];
    for format in WINDOW_FORMATS {
        units.push(format!("canonical:{}", format_label(format)));
    }
    if let Some(preset) = profile {
        for format in WINDOW_FORMATS {
            units.push(format!("profile:{}:{}", preset.name(), format_label(format)));
        }
    }
    for map in maps {
        let name = unit_name(&map.name);
        for format in WINDOW_FORMATS {
            units.push(format!("map:{name}:{}", format_label(format)));
            if let Some(preset) = profile {
                units.push(format!("map:{name}:{}:{}", preset.name(), format_label(format)));
            }
        }
    }
    for key in targets {
        units.push(format!("target:{}x{}", target_label(key.format), key.samples));
    }
    if !maps.is_empty() {
        units.push(format!("maps:{}", map_list_hash(maps)));
    }
    units.join(",")
}

/// A map name as it appears in the unit list: the list is comma-separated.
fn unit_name(name: &str) -> String {
    name.replace(',', "_")
}

/// Hash of the map list in order: each map's name and the host's fingerprint (blake3, the
/// first 16 hex digits). A renamed, added, removed, reordered or changed map gives another
/// hash, so the stamp's unit list no longer matches.
fn map_list_hash(maps: &[PrepareMap]) -> String {
    let mut hasher = blake3::Hasher::new();
    for map in maps {
        for part in [map.name.as_str(), map.fingerprint.as_str()] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
    }
    let hex = hasher.finalize().to_hex();
    hex.as_str()[..16].to_string()
}

/// A target format's name in the unit list (`format_label` for the two window formats).
fn target_label(format: wgpu::TextureFormat) -> String {
    match format_label(format) {
        "other" => format!("{format:?}").to_ascii_lowercase(),
        label => label.to_string(),
    }
}

fn format_label(format: wgpu::TextureFormat) -> &'static str {
    match format {
        wgpu::TextureFormat::Bgra8UnormSrgb => "bgra8-srgb",
        wgpu::TextureFormat::Rgba8UnormSrgb => "rgba8-srgb",
        _ => "other",
    }
}

fn cache_key_text(info: &wgpu::AdapterInfo) -> String {
    if let Some(key) = PipelineCacheKey::for_adapter(info) {
        return key.canonical();
    }
    format!(
        "backend={:?}\nvendor={:#x}\ndevice={:#x}\ndriver={}\ndriver_info={}\nwgpu={}\nshaders={}\n",
        info.backend,
        info.vendor,
        info.device,
        info.driver,
        info.driver_info,
        pipeline_cache::WGPU_VERSION,
        pipeline_cache::salted_shader_hash()
    )
}

fn write_stamp(path: &Path, stamp: &PrepareStamp) -> Result<(), PrepareError> {
    let body = format!(
        "{{\n  \"version\": {},\n  \"ok\": {},\n  \"key\": \"{}\",\n  \"units\": \"{}\",\n  \"pipelines\": {},\n  \"threads\": {},\n  \"backend\": \"{}\",\n  \"finished_unix\": {}\n}}\n",
        stamp.version,
        if stamp.ok { "true" } else { "false" },
        json_escape(&stamp.key),
        json_escape(&stamp.units),
        stamp.pipelines,
        stamp.threads,
        json_escape(&stamp.backend),
        stamp.finished_unix,
    );
    pipeline_cache::write_atomic(path, body.as_bytes()).map_err(|err| io_err(path, err))
}

fn write_status(path: &Path, progress: &PrepareProgress) -> Result<(), PrepareError> {
    let now = unix_now();
    let started = json_u64(&read_if_any(path), "started_unix").unwrap_or(now);
    let body = format!(
        "{{\n  \"version\": {STATUS_VERSION},\n  \"state\": \"{}\",\n  \"pid\": {},\n  \"started_unix\": {started},\n  \"updated_unix\": {now},\n  \"phase\": \"{}\",\n  \"done\": {},\n  \"total\": {},\n  \"fraction\": {:.4},\n  \"pipelines\": {},\n  \"threads\": {},\n  \"backend\": \"{}\",\n  \"error\": {},\n  \"skipped\": {}\n}}\n",
        progress.state.name(),
        std::process::id(),
        json_escape(&progress.phase),
        progress.done,
        progress.total,
        progress.fraction,
        progress.pipelines,
        progress.threads,
        json_escape(&progress.backend),
        match &progress.error {
            Some(err) => format!("\"{}\"", json_escape(err)),
            None => "null".to_string(),
        },
        if progress.skipped { "true" } else { "false" },
    );
    pipeline_cache::write_atomic(path, body.as_bytes()).map_err(|err| io_err(path, err))
}

fn read_if_any(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn parse_stamp(text: &str) -> Option<PrepareStamp> {
    let version = u32::try_from(json_u64(text, "version")?).ok()?;
    if version != STAMP_VERSION {
        return None;
    }
    Some(PrepareStamp {
        version,
        ok: json_bool(text, "ok")?,
        key: json_str(text, "key")?,
        units: json_str(text, "units")?,
        pipelines: json_u64(text, "pipelines")?,
        threads: usize::try_from(json_u64(text, "threads")?).ok()?,
        backend: json_str(text, "backend")?,
        finished_unix: json_u64(text, "finished_unix")?,
    })
}

fn parse_status(text: &str) -> Option<PrepareStatus> {
    let version = u32::try_from(json_u64(text, "version")?).ok()?;
    if version != STATUS_VERSION {
        return None;
    }
    Some(PrepareStatus {
        version,
        state: PrepareState::parse(&json_str(text, "state")?)?,
        pid: u32::try_from(json_u64(text, "pid")?).ok()?,
        started_unix: json_u64(text, "started_unix")?,
        updated_unix: json_u64(text, "updated_unix")?,
        phase: json_str(text, "phase")?,
        done: u32::try_from(json_u64(text, "done")?).ok()?,
        total: u32::try_from(json_u64(text, "total")?).ok()?,
        fraction: json_f32(text, "fraction")?,
        pipelines: json_u64(text, "pipelines")?,
        threads: usize::try_from(json_u64(text, "threads")?).ok()?,
        backend: json_str(text, "backend")?,
        error: json_str_or_null(text, "error"),
    })
}

fn heartbeat_stale(updated_unix: u64) -> bool {
    unix_now().saturating_sub(updated_unix) > STATUS_STALE_AFTER.as_secs()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn io_err(path: &Path, source: std::io::Error) -> PrepareError {
    PrepareError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

/// The value after `"name":`, ignoring that same spelling inside a string.
///
/// A phase of `done` is the string `"done"`, which is not the `"done"` field.
fn field_raw<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("\"{name}\"");
    let mut in_string = false;
    let mut escape = false;
    for (index, ch) in text.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch != '"' {
            continue;
        }
        let rest = &text[index..];
        if let Some(after) = rest.strip_prefix(&needle) {
            let after = after.trim_start();
            if let Some(value) = after.strip_prefix(':') {
                return Some(value.trim_start());
            }
        }
        in_string = true;
    }
    None
}

fn json_str(text: &str, name: &str) -> Option<String> {
    let rest = field_raw(text, name)?.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code)?);
                }
                other => out.push(other),
            }
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

fn json_str_or_null(text: &str, name: &str) -> Option<String> {
    let after = field_raw(text, name)?;
    if after.starts_with("null") {
        None
    } else {
        json_str(text, name)
    }
}

fn json_u64(text: &str, name: &str) -> Option<u64> {
    let after = field_raw(text, name)?;
    let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

fn json_bool(text: &str, name: &str) -> Option<bool> {
    let after = field_raw(text, name)?;
    if after.starts_with("true") {
        Some(true)
    } else if after.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn json_f32(text: &str, name: &str) -> Option<f32> {
    let after = field_raw(text, name)?;
    let num: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_stamp() -> PrepareStamp {
        PrepareStamp {
            version: STAMP_VERSION,
            ok: true,
            key: "backend=Vulkan\nvendor=0x10de\ndevice=0x2c02\ndriver=NVIDIA\ndriver_info=615.71\nwgpu=30\nshaders=abc\n"
                .to_string(),
            units: "splash:bgra8-srgb,canonical:bgra8-srgb,canonical:rgba8-srgb".to_string(),
            pipelines: 40,
            threads: 12,
            backend: "Vulkan".to_string(),
            finished_unix: 1_700_000_000,
        }
    }

    #[test]
    fn stamp_roundtrip_matches_only_the_same_key_and_units() {
        let dir = std::env::temp_dir().join(format!("regia-prepare-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = stamp_path(&dir);
        let stamp = sample_stamp();
        write_stamp(&path, &stamp).expect("write");
        let read = read_stamp(&path).expect("read").expect("present");
        assert!(read.is_current(&stamp.key, &stamp.units));
        assert!(!read.is_current("backend=Vulkan\n", &stamp.units));
        assert!(!read.is_current(&stamp.key, "other"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_stamp_is_absent_and_garbage_is_not_a_match() {
        let dir = std::env::temp_dir().join(format!("regia-prepare-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = stamp_path(&dir);
        assert!(read_stamp(&path).expect("read").is_none());
        std::fs::write(&path, "{not a stamp").expect("write");
        assert!(read_stamp(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_stamp_is_not_current() {
        let mut stamp = sample_stamp();
        stamp.ok = false;
        assert!(!stamp.is_current(&stamp.key, &stamp.units));
    }

    /// The fallback stamp key (every backend without pipeline caches: Metal, DX12, GL) used
    /// to carry no content salt, so a game update with new material graphs compared equal
    /// and the warm was skipped.
    #[test]
    fn non_vulkan_stamp_key_includes_content_salt() {
        let info = || wgpu::AdapterInfo {
            name: "test".into(),
            vendor: 0x106b,
            device: 0,
            device_type: wgpu::DeviceType::DiscreteGpu,
            device_pci_bus_id: String::new(),
            driver: "Metal".into(),
            driver_info: "1.0".into(),
            backend: wgpu::Backend::Metal,
            subgroup_min_size: 32,
            subgroup_max_size: 32,
            transient_saves_memory: None,
            limit_bucket: None,
        };
        crate::pipeline_cache::set_content_salt("game-v1");
        let key_one = cache_key_text(&info());
        crate::pipeline_cache::set_content_salt("game-v2");
        let key_two = cache_key_text(&info());
        crate::pipeline_cache::set_content_salt("game-v1");
        let key_again = cache_key_text(&info());
        crate::pipeline_cache::set_content_salt("");
        assert_ne!(
            key_one, key_two,
            "a new salt must miss the stamp on every backend"
        );
        assert_eq!(key_one, key_again, "the same salt keeps the key");
        let stamp = PrepareStamp {
            key: key_one,
            ..sample_stamp()
        };
        assert!(!stamp.is_current(&key_two, &stamp.units));
    }

    #[test]
    fn a_ready_status_with_phase_done_roundtrips() {
        let dir = std::env::temp_dir().join(format!("regia-prepare-done-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = status_path(&dir);
        let progress = PrepareProgress {
            state: PrepareState::Ready,
            phase: "done".to_string(),
            done: 3,
            total: 3,
            fraction: 1.0,
            pipelines: 37,
            threads: 12,
            backend: "Vulkan".to_string(),
            message: "stamp written".to_string(),
            error: None,
            skipped: false,
        };
        write_status(&path, &progress).expect("write");
        let status = read_status(&path).expect("read").expect("present");
        assert_eq!(status.state, PrepareState::Ready);
        assert_eq!(status.phase, "done");
        assert_eq!(status.done, 3);
        assert_eq!(status.total, 3);
        assert_eq!(status.threads, 12);
        assert_eq!(status.pipelines, 37);
        assert!(status.error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_quiet_running_status_is_stale() {
        let dir = std::env::temp_dir().join(format!("regia-prepare-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = status_path(&dir);
        let old = unix_now().saturating_sub(STATUS_STALE_AFTER.as_secs() + 5);
        let body = format!(
            "{{\"version\":1,\"state\":\"running\",\"pid\":1,\"started_unix\":{old},\"updated_unix\":{old},\"phase\":\"warm\",\"done\":0,\"total\":3,\"fraction\":0.0,\"pipelines\":0,\"threads\":12,\"backend\":\"Vulkan\",\"error\":null,\"skipped\":false}}\n"
        );
        std::fs::write(&path, body).expect("write");
        let status = read_status(&path).expect("read").expect("present");
        assert_eq!(status.state, PrepareState::Stale);
        assert_eq!(status.phase, "warm");
        assert!(status.error.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_json_is_one_object_and_escapes() {
        let line = PrepareProgress {
            state: PrepareState::Running,
            phase: "canonical:bgra8-srgb".to_string(),
            done: 1,
            total: 3,
            fraction: 1.0 / 3.0,
            pipelines: 12,
            threads: 12,
            backend: "Vulkan".to_string(),
            message: "line\n\"quoted\"".to_string(),
            error: None,
            skipped: false,
        }
        .to_json();
        assert!(line.starts_with('{') && line.ends_with('}'));
        assert!(!line.contains('\n'));
        assert!(line.contains("\"state\":\"running\""));
        assert!(line.contains("line\\n\\\"quoted\\\""));
        assert!(line.contains("\"error\":null"));
        assert_eq!(json_str(&line, "message").as_deref(), Some("line\n\"quoted\""));
    }

    /// ADR-8453: the stamp's unit list covers the map list (names and fingerprints, in order)
    /// and the targets, and a request without either keeps the unit list older stamps hold.
    #[test]
    fn the_unit_list_covers_the_map_list_and_targets() {
        let bare = unit_list(None, &[], &[]);
        assert_eq!(
            bare, "splash:bgra8-srgb,canonical:bgra8-srgb,canonical:rgba8-srgb",
            "a request without maps keeps the unit list stamps were written with"
        );
        let two = [PrepareMap::new("a", "1"), PrepareMap::new("b", "1")];
        let units = unit_list(None, &two, &[]);
        assert!(units.starts_with(&bare));
        assert!(units.contains("map:a:bgra8-srgb") && units.contains("map:b:rgba8-srgb"));
        let stamp = PrepareStamp {
            units: units.clone(),
            ..sample_stamp()
        };
        assert!(stamp.is_current(&stamp.key, &units));
        let changed = [PrepareMap::new("a", "1"), PrepareMap::new("b", "2")];
        let reordered = [PrepareMap::new("b", "1"), PrepareMap::new("a", "1")];
        let shorter = [PrepareMap::new("a", "1")];
        // The fingerprint is not in the readable units; only the hash carries it.
        let glued = [PrepareMap::new("a", "1b"), PrepareMap::new("", "1")];
        for other in [&changed[..], &reordered[..], &shorter[..], &glued[..]] {
            let other_units = unit_list(None, other, &[]);
            assert!(
                !stamp.is_current(&stamp.key, &other_units),
                "{other:?} must not match a stamp for {two:?}"
            );
        }
        let profiled = unit_list(Some(RenderProfilePreset::DesktopHigh), &two, &[]);
        assert!(profiled.contains("map:a:DesktopHigh:bgra8-srgb"));
        let target = WarmTargetKey {
            format: wgpu::TextureFormat::Rgba16Float,
            samples: 4,
        };
        let with_target = unit_list(None, &[], &[target]);
        assert_eq!(with_target, format!("{bare},target:rgba16floatx4"));
        let comma = [PrepareMap::new("x,y", "1")];
        assert!(unit_list(None, &comma, &[]).contains("map:x_y:"));
    }

    #[test]
    fn the_running_map_job_names_the_phase() {
        let line = "done 3/8, 4 workers; running [map] map:levels/b:rgba8-srgb (#5) 1.2 s; running [map] map:levels/c:bgra8-srgb (#6) 0.1 s; pending [map] map:levels/d:bgra8-srgb (#7) 0.0 s";
        assert_eq!(current_map(line), Some("levels/b"));
        assert_eq!(
            current_map("done 1/3, 4 workers; running [boot] splash (#1) 0.2 s"),
            None
        );
        assert_eq!(current_map("done 0/0, 4 workers, not started"), None);
    }

    #[test]
    fn eta_scales_the_elapsed_time_by_the_jobs_left() {
        assert_eq!(eta_of(2, 6, 10), Duration::from_secs(20));
        assert_eq!(eta_of(6, 6, 10), Duration::ZERO);
        let status = HostPrepareStatus {
            needed: true,
            state: Some(PrepareState::Running),
            done: 1,
            total: 4,
            eta: None,
        };
        assert_eq!(status.fraction(), 0.25);
    }

    #[test]
    fn unit_list_grows_with_the_profile() {
        let bare = unit_list(None, &[], &[]);
        let high = unit_list(Some(RenderProfilePreset::DesktopHigh), &[], &[]);
        assert!(bare.contains("splash:bgra8-srgb"));
        assert!(bare.contains("canonical:rgba8-srgb"));
        assert!(!bare.contains("profile:"));
        assert!(high.starts_with(&bare));
        assert!(high.contains("profile:DesktopHigh:bgra8-srgb"));
    }
}
