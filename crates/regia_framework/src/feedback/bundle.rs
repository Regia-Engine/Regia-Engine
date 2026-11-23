// SPDX-License-Identifier: MPL-2.0
//! The feedback bundle: what goes in it, the frame-time ring it draws on, and the files on disk.
//!
//! A bundle is one `.zip` (stored entries, [`super::zip`]) under the feedback folder:
//!
//! | Entry | What |
//! |---|---|
//! | `feedback.json` | category, note, time, map/position, which parts are present, build summary, wgpu errors since start |
//! | `note.txt` | the player's note |
//! | `screenshot.png` | the frame when the player opened the prompt (absent in crash bundles) |
//! | `frame_times.csv` | the last [`FeedbackConfig::window_s`](super::FeedbackConfig) seconds: frame, CPU, GPU ms and device creates |
//! | `hitch_report.txt` | the `specs/27` § 2.4 hitch report over those frames |
//! | `system.txt` | OS, device, CPU, RAM, GPU and driver |
//! | `settings.ron` | the active `GameSettings` (graphics preset included) |
//! | `render_problems.txt` | `render.problems`: the renderer's last-frame problems |
//! | `log_tail.txt` | the most recent log records |
//! | `build_info.json` | version, git hash, profile, engine; the packaged `build_info.json` (glibc floor) |
//! | `host.json` | the fields the game added ([`super::FeedbackHost`]): match id, map, player count, position |
//! | `crash.txt` | crash bundles only: panic message, location and backtrace; for a native crash the signal, fault address, thread and last frame |
//! | `minidump.dmp` | native crash bundles only: the minidump (`cargo xtask minidump` reads it) |
//! | `pipelines/*.fpc` | optional last saved cache, validated for the capture's GPU/driver key |

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use regia_core::log::civil_date_time;
use regia_core::log::ring::LogRecord;
use regia_platform::OsInfo;
use regia_platform::crash::BuildInfo;
use regia_render::{CapturedImage, FramePathCounters, HitchFrame, HitchReport, HitchTargets};
use serde_json::{Value, json};

use super::zip::{ZipWriter, entry_names, read_entries};

/// File-name prefix of a bundle.
pub const BUNDLE_PREFIX: &str = "feedback-";
/// File-name suffix of a bundle.
pub const BUNDLE_SUFFIX: &str = ".zip";
/// Bundle format version written into `feedback.json`.
pub const BUNDLE_FORMAT: u32 = 1;
/// Largest bundle file a reader takes. Real bundles are a few MiB.
pub const MAX_BUNDLE_BYTES: u64 = 256 * 1024 * 1024;

/// What the feedback is about. The prompt offers the first four; [`Self::Crash`] is written by
/// the panic hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FeedbackCategory {
    /// Something is broken.
    #[default]
    Bug,
    /// Slow, stutters, hitches.
    Performance,
    /// Looks wrong.
    Visual,
    /// Anything else.
    Other,
    /// Written automatically after a panic.
    Crash,
}

impl FeedbackCategory {
    /// The categories the prompt offers, in order.
    pub const PLAYER: [Self; 4] = [Self::Bug, Self::Performance, Self::Visual, Self::Other];

    /// Lower-case name (`bug`, `performance`, `visual`, `other`, `crash`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Performance => "performance",
            Self::Visual => "visual",
            Self::Other => "other",
            Self::Crash => "crash",
        }
    }

    /// The prompt's button label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Bug => "Bug",
            Self::Performance => "Performance",
            Self::Visual => "Visual",
            Self::Other => "Other",
            Self::Crash => "Crash",
        }
    }

    /// Parses [`Self::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "bug" => Self::Bug,
            "performance" => Self::Performance,
            "visual" => Self::Visual,
            "other" => Self::Other,
            "crash" => Self::Crash,
            _ => return None,
        })
    }
}

/// Why a renderer timing value is absent on an update tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderTimingAbsence {
    /// The runner skipped rendering because the window is hidden or its surface was refused.
    Hidden,
    /// No scene is available (including a renderer temporarily owned by scene warming).
    NoScene,
    /// The renderer has published nothing new since the last observation.
    RendererIdle,
    /// No runtime renderer is installed.
    NoRenderer,
    /// GPU timestamps are disabled or unsupported.
    NoGpuTiming,
    /// A GPU sample has not completed readback yet.
    Pending,
}

/// Source render frames and validity of the numeric values in a feedback sample.
/// GPU frame IDs may lag CPU frame IDs; each publication is consumed once per sampler.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct RenderTimingSample {
    /// Source frame of the cached CPU sample, even when stale.
    pub cpu_frame_id: Option<u64>,
    /// Source frame of the cached GPU sample, even when stale.
    pub gpu_frame_id: Option<u64>,
    /// The CPU publication has not been observed by this sampler before.
    pub cpu_fresh: bool,
    /// The GPU publication has not been observed by this sampler before.
    pub gpu_fresh: bool,
    /// Why no CPU value was recorded for this tick.
    pub cpu_absent: Option<RenderTimingAbsence>,
    /// Why no GPU value was recorded for this tick.
    pub gpu_absent: Option<RenderTimingAbsence>,
}

impl RenderTimingSample {
    /// Refines absent values with the runner's hidden state or a scene-warm state.
    /// Fresh values retain their source frame and are never relabeled as absent.
    pub fn inactive(&mut self, hidden: bool, no_scene: bool) {
        let reason = if hidden {
            Some(RenderTimingAbsence::Hidden)
        } else if no_scene {
            Some(RenderTimingAbsence::NoScene)
        } else {
            None
        };
        if let Some(reason) = reason {
            if !self.cpu_fresh {
                self.cpu_absent = Some(reason);
            }
            if !self.gpu_fresh {
                self.gpu_absent = Some(reason);
            }
        }
    }
}

/// One recorded frame in the feedback ring.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameSample {
    /// Optional source identities, freshness and absence reasons (v0.5.1).
    /// Legacy/native crash records may omit this metadata; numeric zero means absent there.
    pub timing: Option<RenderTimingSample>,
    /// Real time since startup at the end of the frame, seconds.
    pub t_s: f64,
    /// Frame interval as presented, ms.
    pub frame_ms: f32,
    /// Fresh CPU time inside the renderer, ms (0 when absent; see `timing`).
    pub cpu_ms: f32,
    /// Fresh GPU frame time, ms (0 when absent; see `timing`).
    pub gpu_ms: f32,
    /// Pipelines created during the frame.
    pub pipelines: u32,
    /// Bind groups created.
    pub bind_groups: u32,
    /// Buffers allocated.
    pub buffers: u32,
    /// Textures allocated.
    pub textures: u32,
    /// Blocking device polls.
    pub blocking_polls: u16,
    /// Inline CPU bakes.
    pub inline_bakes: u16,
}

impl FrameSample {
    /// The frame's device calls in the hitch report's terms.
    #[must_use]
    pub fn counters(&self) -> FramePathCounters {
        FramePathCounters {
            pipelines: u64::from(self.pipelines),
            bind_groups: u64::from(self.bind_groups),
            buffers: u64::from(self.buffers),
            textures: u64::from(self.textures),
            blocking_polls: u64::from(self.blocking_polls),
            inline_bakes: u64::from(self.inline_bakes),
            ..FramePathCounters::default()
        }
    }
}

/// Samples in the ring: 30 s at up to 270 fps. Allocated once when the plugin is
/// added; recording a frame never allocates.
pub const FRAME_RING_CAPACITY: usize = 8192;

/// A fixed-capacity ring of [`FrameSample`]s.
#[derive(Debug)]
pub struct FrameRing {
    samples: Box<[FrameSample]>,
    next: usize,
    len: usize,
}

impl FrameRing {
    /// A ring of `capacity` samples (at least one), allocated now.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: vec![FrameSample::default(); capacity.max(1)].into_boxed_slice(),
            next: 0,
            len: 0,
        }
    }

    /// Records one frame, overwriting the oldest when full. Never allocates.
    pub fn push(&mut self, sample: FrameSample) {
        let cap = self.samples.len();
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % cap;
        self.len = (self.len + 1).min(cap);
    }

    /// Samples held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Visits every held sample, oldest first, without allocating (the native crash handler
    /// copies the ring from a signal handler).
    pub fn for_each(&self, mut f: impl FnMut(&FrameSample)) {
        let cap = self.samples.len();
        let start = (self.next + cap - self.len) % cap;
        for i in 0..self.len {
            f(&self.samples[(start + i) % cap]);
        }
    }

    /// The samples of the last `window_s` seconds (relative to the newest), oldest first.
    #[must_use]
    pub fn last_window(&self, window_s: f64) -> Vec<FrameSample> {
        let cap = self.samples.len();
        let start = (self.next + cap - self.len) % cap;
        let all = (0..self.len).map(|i| self.samples[(start + i) % cap]);
        let Some(newest) = self.len.checked_sub(1).map(|i| self.samples[(start + i) % cap]) else {
            return Vec::new();
        };
        all.filter(|s| newest.t_s - s.t_s <= window_s).collect()
    }
}

/// Panic details for a crash bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrashDetails {
    /// The panic message.
    pub message: String,
    /// `file:line:column`.
    pub location: String,
    /// The captured backtrace.
    pub backtrace: String,
    /// The thread that panicked.
    pub thread: String,
    /// What `crash.txt` calls it: empty for a panic, else the reason (`device_lost`,
    /// `surface_unavailable`); then [`Self::backtrace`] holds the details instead.
    pub label: String,
}

/// A native crash (a signal such as SIGSEGV, not a panic) for a crash bundle. The crash monitor
/// fills it after the game process died (`feedback::native`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeCrash {
    /// The signal number (11 = SIGSEGV, 7 = SIGBUS, 4 = SIGILL, 6 = SIGABRT, 8 = SIGFPE, 5 = SIGTRAP).
    /// On Windows it is the exception code (an `NTSTATUS` such as `0xC0000005`, access violation);
    /// every such code is above 255, so it cannot be mistaken for a signal number.
    pub signal: u32,
    /// `si_code` (for SIGSEGV: 1 = address not mapped, 2 = invalid permissions). On Windows, for
    /// an access violation or in-page error: 0 = read, 1 = write, 8 = instruction fetch (DEP).
    pub code: i32,
    /// The faulting address (`si_addr`; on Windows the address the access violation touched).
    pub address: u64,
    /// The crashed process.
    pub pid: u32,
    /// The crashing thread.
    pub tid: u32,
    /// The crashing thread's name (empty when unknown).
    pub thread: String,
    /// The minidump (`minidump.dmp` in the bundle).
    pub minidump: Option<Vec<u8>>,
    /// Why there is no minidump, when there is none.
    pub minidump_error: Option<String>,
    /// The OS the crash happened on (`linux`, `windows`, `android`). Empty: this build's OS.
    pub platform: String,
    /// The stack from an in-process record (Android, ADR-328): `pc`, `lr`, then the frame-pointer
    /// frames, each `address module+offset`. Empty with a minidump, which holds the stack.
    pub stack: Vec<String>,
}

impl NativeCrash {
    /// The signal's name (`SIGSEGV`…), or `signal N`.
    #[must_use]
    pub fn signal_name(&self) -> String {
        if self.signal > 255 {
            return match self.signal {
                0xC000_0005 => "EXCEPTION_ACCESS_VIOLATION".into(),
                0xC000_0006 => "EXCEPTION_IN_PAGE_ERROR".into(),
                0xC000_001D => "EXCEPTION_ILLEGAL_INSTRUCTION".into(),
                0xC000_0025 => "EXCEPTION_NONCONTINUABLE_EXCEPTION".into(),
                0xC000_008C => "EXCEPTION_ARRAY_BOUNDS_EXCEEDED".into(),
                0xC000_008E => "EXCEPTION_FLT_DIVIDE_BY_ZERO".into(),
                0xC000_0094 => "EXCEPTION_INT_DIVIDE_BY_ZERO".into(),
                0xC000_0095 => "EXCEPTION_INT_OVERFLOW".into(),
                0xC000_0096 => "EXCEPTION_PRIV_INSTRUCTION".into(),
                0xC000_00FD => "EXCEPTION_STACK_OVERFLOW".into(),
                0xC000_0374 => "STATUS_HEAP_CORRUPTION".into(),
                0xC000_0409 => "STATUS_STACK_BUFFER_OVERRUN (fast fail)".into(),
                0xC000_000D => "STATUS_INVALID_PARAMETER".into(),
                0x8000_0003 => "EXCEPTION_BREAKPOINT".into(),
                n => format!("exception {n:#010x}"),
            };
        }
        match self.signal {
            4 => "SIGILL".into(),
            5 => "SIGTRAP".into(),
            6 => "SIGABRT".into(),
            7 => "SIGBUS".into(),
            8 => "SIGFPE".into(),
            11 => "SIGSEGV".into(),
            n => format!("signal {n}"),
        }
    }

    /// What `si_code` means for this signal, when it is a common one.
    #[must_use]
    pub fn code_meaning(&self) -> &'static str {
        if self.signal > 255 {
            return match (self.signal, self.code) {
                (0xC000_0005 | 0xC000_0006, 0) => "read of an inaccessible address",
                (0xC000_0005 | 0xC000_0006, 1) => "write to an inaccessible address",
                (0xC000_0005 | 0xC000_0006, 8) => "instruction fetch from a non-executable address",
                _ => "",
            };
        }
        match (self.signal, self.code) {
            (11, 1) => "address not mapped",
            (11, 2) => "invalid permissions for the mapped address",
            (7, 1) => "invalid address alignment",
            (7, 2) => "nonexistent physical address",
            (7, 3) => "object-specific hardware error",
            (4, 1) => "illegal opcode",
            (4, 2) => "illegal operand",
            (4, 4) => "illegal addressing mode",
            (4, 5) => "illegal trap",
            (4, 6) => "privileged opcode",
            (8, 1) => "integer divide by zero",
            (8, 2) => "integer overflow",
            (_, 0) => "sent by a process (kill/raise)",
            (_, -6) => "sent by tgkill (abort)",
            _ => "",
        }
    }

    /// `SIGSEGV (address not mapped)`.
    #[must_use]
    pub fn reason(&self) -> String {
        match self.code_meaning() {
            "" if self.signal > 255 => self.signal_name(),
            "" => format!("{} (code {})", self.signal_name(), self.code),
            m => format!("{} ({m})", self.signal_name()),
        }
    }
}

/// Machine facts for `system.txt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInfo {
    /// OS, version, device model, RAM, logical CPUs.
    pub os: OsInfo,
    /// CPU model name (empty when unknown).
    pub cpu_model: String,
    /// GPU, backend and driver as the renderer reported the chosen adapter (empty before).
    pub gpu: String,
}

impl SystemInfo {
    /// The running machine.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            os: OsInfo::detect(),
            cpu_model: cpu_model(),
            gpu: regia_platform::crash::gpu_info(),
        }
    }
}

/// Everything one bundle holds. Built on the main thread at the moment of capture; encoded and
/// written on a worker thread ([`write_bundle`]).
#[derive(Debug, Clone)]
pub struct BundleInput {
    /// What the feedback is about.
    pub category: FeedbackCategory,
    /// The player's note (may be empty).
    pub note: String,
    /// Capture time, ms since the Unix epoch.
    pub unix_ms: u64,
    /// The frame on screen (RGBA8, as displayed).
    pub screenshot: Option<CapturedImage>,
    /// Why there is no screenshot, when there is none.
    pub screenshot_missing: Option<String>,
    /// The recent frames, oldest first.
    pub frames: Vec<FrameSample>,
    /// Seconds of frames requested.
    pub window_s: f64,
    /// Machine facts.
    pub system: SystemInfo,
    /// The active settings as RON (`None` without the settings resource).
    pub settings_ron: Option<String>,
    /// `render.problems`: `(code, message)` (`None` without a runtime renderer).
    pub render_problems: Option<Vec<(String, String)>>,
    /// The log tail, oldest first.
    pub log_tail: Vec<LogRecord>,
    /// Compile-time build identity.
    pub build: BuildInfo,
    /// The packaged `build_info.json` beside the executable, verbatim, when present.
    pub package_build_info: Option<String>,
    /// Game-provided fields (match id, map, player count, position…).
    pub host: Vec<(String, String)>,
    /// Panic details (crash bundles only).
    pub crash: Option<CrashDetails>,
    /// Native crash details and the minidump (native crash bundles only).
    pub native_crash: Option<NativeCrash>,
    /// Uncaptured wgpu errors since the game started (`feedback.json` `gpu_errors`;
    /// [`regia_render::gpu_health::gpu_error_count`]). `None` when unknown (the native crash
    /// monitor is another process).
    pub gpu_errors: Option<u64>,
    /// GPU breadcrumbs (spec 54): `gpu_breadcrumbs.json` and the `gpu breadcrumbs:` block of
    /// `crash.txt` (a device-lost bundle, or a validation/internal error bundle).
    pub gpu_breadcrumbs: Option<GpuBreadcrumbs>,
    /// Optional saved cache source, read and validated when the worker builds the archive.
    pub pipeline_cache: Option<super::PipelineCacheAttachment>,
    /// Attachment budget in bytes; 0 disables. Defaults to 32 MiB, capped at half
    /// `MAX_BUNDLE_BYTES` to leave room for the rest of the report.
    pub pipeline_cache_max_bytes: u64,
}

/// The breadcrumb parts of a bundle ([`regia_render::breadcrumbs::BreadcrumbReport`] rendered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuBreadcrumbs {
    /// The `gpu_breadcrumbs.json` text.
    pub json: String,
    /// The `gpu breadcrumbs:` block appended to `crash.txt`.
    pub text: String,
}

impl BundleInput {
    /// An input with the machine and build facts filled in and everything else empty.
    #[must_use]
    pub fn new(category: FeedbackCategory, note: impl Into<String>) -> Self {
        Self {
            category,
            note: note.into(),
            unix_ms: unix_ms_now(),
            screenshot: None,
            screenshot_missing: None,
            frames: Vec::new(),
            window_s: 30.0,
            system: SystemInfo::detect(),
            settings_ron: None,
            render_problems: None,
            log_tail: Vec::new(),
            build: BuildInfo::detect(),
            package_build_info: package_build_info(),
            host: Vec::new(),
            crash: None,
            gpu_breadcrumbs: None,
            native_crash: None,
            gpu_errors: Some(regia_render::gpu_health::gpu_error_count()),
            pipeline_cache: None,
            pipeline_cache_max_bytes: 32 * 1024 * 1024,
        }
    }

    /// A host field by key.
    #[must_use]
    pub fn host_field(&self, key: &str) -> Option<&str> {
        self.host.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// A bundle written to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleInfo {
    /// File name (no directory).
    pub name: String,
    /// Full path: what the player sends.
    pub path: PathBuf,
    /// Size in bytes.
    pub bytes: u64,
    /// The category.
    pub category: FeedbackCategory,
    /// The archive's entry names.
    pub entries: Vec<String>,
}

/// A bundle found in the feedback folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFile {
    /// File name.
    pub name: String,
    /// Full path.
    pub path: PathBuf,
    /// Size in bytes.
    pub bytes: u64,
    /// Modification time, ms since the Unix epoch (0 when unknown).
    pub modified_unix_ms: u64,
    /// The category from the file name (`None` for an unknown suffix).
    pub category: Option<FeedbackCategory>,
}

/// Encodes `input` as a bundle archive. Returns the bytes and the entry names.
pub fn build_bundle(input: &BundleInput) -> (Vec<u8>, Vec<String>) {
    build_bundle_limited(input, MAX_BUNDLE_BYTES)
}

fn build_bundle_limited(input: &BundleInput, limit: u64) -> (Vec<u8>, Vec<String>) {
    let cache = input
        .pipeline_cache
        .as_ref()
        .map(|source| source.read(input.pipeline_cache_max_bytes));
    let encoded = encode_bundle(input, &cache);
    if encoded.0.len() as u64 > limit && matches!(cache, Some(Ok(_))) {
        // Preserve the diagnostic report when the screenshot/logs leave too little room.
        // The cache is optional; never create a report its own reader cannot open because
        // of the attachment. The uncommon retry does no file or driver IO.
        encode_bundle(
            input,
            &Some(Err("pipeline cache omitted: bundle byte limit".into())),
        )
    } else {
        encoded
    }
}

fn encode_bundle(input: &BundleInput, cache: &Option<Result<Vec<u8>, String>>) -> (Vec<u8>, Vec<String>) {
    let mut zip = ZipWriter::new(input.unix_ms / 1000);
    let mut names = Vec::new();
    let mut add = |zip: &mut ZipWriter, name: &str, data: &[u8]| {
        zip.add(name, data);
        names.push(name.to_owned());
    };

    let mut screenshot_missing = input.screenshot_missing.clone();
    let png = input.screenshot.as_ref().and_then(|img| match encode_png(img) {
        Ok(png) => Some(png),
        Err(e) => {
            screenshot_missing = Some(format!("PNG encoding failed: {e}"));
            None
        }
    });
    if input.screenshot.is_none() && screenshot_missing.is_none() {
        screenshot_missing = Some("no screenshot requested".into());
    }

    let hitch = hitch_report(&input.frames);
    let cache_name = input
        .pipeline_cache
        .as_ref()
        .filter(|_| matches!(cache, Some(Ok(_))))
        .map(|source| format!("pipelines/{}", source.key.shipped_file_name()));
    let cache_missing = cache.as_ref().and_then(|result| result.as_ref().err());
    let mut present = vec![
        "note.txt",
        "frame_times.csv",
        "hitch_report.txt",
        "system.txt",
        "settings.ron",
        "render_problems.txt",
        "log_tail.txt",
        "build_info.json",
        "host.json",
    ];
    if png.is_some() {
        present.insert(1, "screenshot.png");
    }
    if input.crash.is_some() || input.native_crash.is_some() {
        present.push("crash.txt");
    }
    let minidump = input.native_crash.as_ref().and_then(|n| n.minidump.as_deref());
    if minidump.is_some() {
        present.push("minidump.dmp");
    }
    if input.gpu_breadcrumbs.is_some() {
        present.push("gpu_breadcrumbs.json");
    }
    if let Some(name) = &cache_name {
        present.push(name);
    }
    let crash_meta = match (&input.crash, &input.native_crash) {
        (Some(c), _) => json!({
            "kind": if c.label.is_empty() { "panic" } else { c.label.as_str() },
            "message": c.message, "location": c.location, "thread": c.thread,
        }),
        (None, Some(n)) => json!({
            "native": true,
            "signal": n.signal_name(),
            "reason": n.reason(),
            "address": format!("{:#x}", n.address),
            "thread": n.thread,
            "tid": n.tid,
            "minidump": minidump.map(|_| "minidump.dmp"),
            "minidump_error": n.minidump_error,
            "platform": if n.platform.is_empty() { std::env::consts::OS } else { n.platform.as_str() },
            "stack": n.stack,
        }),
        (None, None) => Value::Null,
    };
    // Optional metadata keeps the frozen CSV and native-crash wire records unchanged.
    let render_timing = input.frames.iter().any(|f| f.timing.is_some()).then(|| {
        input
            .frames
            .iter()
            .map(|f| {
                json!({
                    "t_s": f.t_s,
                    "timing": f.timing,
                    "cpu_sample_ms": f.timing.filter(|t| t.cpu_fresh).map(|_| f.cpu_ms),
                    "gpu_sample_ms": f.timing.filter(|t| t.gpu_fresh).map(|_| f.gpu_ms),
                })
            })
            .collect::<Vec<_>>()
    });
    let meta = json!({
        "format": BUNDLE_FORMAT,
        "render_timing": render_timing,
        "category": input.category.as_str(),
        "note": input.note,
        "unix_ms": input.unix_ms,
        "time_utc": utc_stamp(input.unix_ms / 1000, " "),
        "crash": crash_meta,
        "map": input.host_field("map"),
        "position": input.host_field("position"),
        "screenshot": png.as_ref().map(|_| "screenshot.png"),
        "screenshot_missing": if png.is_some() { None } else { screenshot_missing.clone() },
        "frames": input.frames.len(),
        "window_s": input.window_s,
        "hitch_verdict": if input.frames.is_empty() { Value::Null } else { json!(hitch.passes()) },
        "gpu_errors": input.gpu_errors,
        "pipeline_cache": cache_name,
        "pipeline_cache_missing": cache_missing,
        "build": { "version": input.build.version, "git": input.build.git_hash, "profile": input.build.profile },
        "files": present,
        "privacy": "Written on this machine only. The engine uploads nothing; the game decides whether to send it.",
    });
    add(
        &mut zip,
        "feedback.json",
        serde_json::to_string_pretty(&meta).unwrap_or_default().as_bytes(),
    );
    add(&mut zip, "note.txt", input.note.as_bytes());
    if let Some(png) = &png {
        add(&mut zip, "screenshot.png", png);
    }
    add(&mut zip, "frame_times.csv", frame_csv(&input.frames).as_bytes());
    let hitch_text = hitch_text(&input.frames, &hitch, input.window_s);
    add(&mut zip, "hitch_report.txt", hitch_text.as_bytes());
    add(&mut zip, "system.txt", system_text(&input.system).as_bytes());
    let settings = input
        .settings_ron
        .clone()
        .unwrap_or_else(|| "// no GameSettings resource in this app\n".into());
    add(&mut zip, "settings.ron", settings.as_bytes());
    add(
        &mut zip,
        "render_problems.txt",
        problems_text(input.render_problems.as_deref()).as_bytes(),
    );
    add(&mut zip, "log_tail.txt", log_text(&input.log_tail).as_bytes());
    let build = json!({
        "version": input.build.version,
        "git": input.build.git_hash,
        "profile": input.build.profile,
        "engine": input.build.engine,
        "package": input
            .package_build_info
            .as_deref()
            .and_then(|t| serde_json::from_str::<Value>(t).ok()),
    });
    add(
        &mut zip,
        "build_info.json",
        serde_json::to_string_pretty(&build)
            .unwrap_or_default()
            .as_bytes(),
    );
    let host: serde_json::Map<String, Value> = input
        .host
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    add(
        &mut zip,
        "host.json",
        serde_json::to_string_pretty(&Value::Object(host))
            .unwrap_or_default()
            .as_bytes(),
    );
    if let Some(crash) = &input.crash {
        let (label, section) = if crash.label.is_empty() {
            ("panic", "backtrace")
        } else {
            (crash.label.as_str(), "details")
        };
        let mut text = format!(
            "{label}: {}\nlocation: {}\nthread: {}\n\n{section}:\n{}\n",
            crash.message, crash.location, crash.thread, crash.backtrace
        );
        if let Some(crumbs) = &input.gpu_breadcrumbs {
            text.push('\n');
            text.push_str(&crumbs.text);
        }
        add(&mut zip, "crash.txt", text.as_bytes());
    }
    if let Some(crumbs) = &input.gpu_breadcrumbs {
        add(&mut zip, "gpu_breadcrumbs.json", crumbs.json.as_bytes());
    }
    if let (Some(name), Some(Ok(bytes))) = (&cache_name, cache) {
        add(&mut zip, name, bytes);
    }
    if let Some(native) = &input.native_crash {
        add(
            &mut zip,
            "crash.txt",
            native_crash_text(native, &input.frames).as_bytes(),
        );
        if let Some(dmp) = minidump {
            add(&mut zip, "minidump.dmp", dmp);
        }
    }
    (zip.finish(), names)
}

/// `crash.txt` for a native crash: signal, address, thread, the minidump and the last frame.
fn native_crash_text(n: &NativeCrash, frames: &[FrameSample]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "native crash: {}", n.reason());
    let _ = writeln!(s, "fault address: {:#018x}", n.address);
    let thread = if n.thread.is_empty() {
        "<unknown>"
    } else {
        n.thread.as_str()
    };
    let _ = writeln!(s, "thread: {thread} (tid {})", n.tid);
    let _ = writeln!(s, "process: {}", n.pid);
    match (&n.minidump, &n.minidump_error) {
        (Some(d), _) => {
            let _ = writeln!(s, "minidump: minidump.dmp ({} bytes)", d.len());
        }
        (None, Some(e)) => {
            let _ = writeln!(s, "minidump: not written ({e})");
        }
        (None, None) => {
            let _ = writeln!(s, "minidump: not written");
        }
    }
    match frames.last() {
        Some(f) => {
            // A tick with no fresh publication has no renderer time (0.0 means absent there).
            let ms = |fresh: bool, ms: f32| {
                if fresh {
                    format!("{ms:.2} ms")
                } else {
                    "no fresh sample".to_owned()
                }
            };
            let _ = writeln!(
                s,
                "last frame: {} recorded, the newest at {:.3} s took {:.2} ms (renderer CPU {}, GPU {})",
                frames.len(),
                f.t_s,
                f.frame_ms,
                ms(f.timing.is_none_or(|t| t.cpu_fresh), f.cpu_ms),
                ms(f.timing.is_none_or(|t| t.gpu_fresh), f.gpu_ms)
            );
        }
        None => {
            let _ = writeln!(s, "last frame: none recorded");
        }
    }
    if n.stack.is_empty() {
        let _ = writeln!(
            s,
            "\nThe stack is in the minidump. Read it with `cargo xtask minidump <this bundle> --symbols \
             <package>/symbols` (needs the release's `symbols/<binary>.debug`)."
        );
    } else {
        let _ = writeln!(
            s,
            "\nstack (pc, lr, then the frame-pointer walk; module+file offset):"
        );
        for line in &n.stack {
            let _ = writeln!(s, "  {line}");
        }
        let _ = writeln!(
            s,
            "\nAn in-process record (no minidump on {}). Resolve an offset in the game's library with \
             `llvm-addr2line -fC -e <unstripped libmain.so> <offset>` (see book agents/crash-reports.md).",
            if n.platform.is_empty() {
                std::env::consts::OS
            } else {
                n.platform.as_str()
            }
        );
    }
    s
}

/// Writes `input` as a new bundle in `dir` (created if missing), atomically (temp file, then
/// rename), and keeps at most `keep` bundles (oldest removed first; `0` keeps all). A crash
/// bundle the player has not been told about yet ([`super::crash::unannounced_crashes`]) is
/// never pruned, so a burst of feedback captures cannot delete it before the next launch.
pub fn write_bundle(dir: &Path, input: &BundleInput, keep: usize) -> std::io::Result<BundleInfo> {
    std::fs::create_dir_all(dir)?;
    let (bytes, entries) = build_bundle(input);
    let stem = format!(
        "{BUNDLE_PREFIX}{}",
        utc_stamp(input.unix_ms / 1000, "_").replace(':', "-")
    );
    let category = input.category.as_str();
    // The temp name is unique per call (process id + counter), so concurrent writers (a feedback
    // worker, the device-lost path, the panic hook) never share a temp file. The final name is
    // claimed with a hard link, which fails atomically if another writer took it first.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{stem}-{category}.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    let mut name = format!("{stem}-{category}{BUNDLE_SUFFIX}");
    let mut n = 1u32;
    let path = loop {
        let candidate = dir.join(&name);
        match std::fs::hard_link(&tmp, &candidate) {
            Ok(()) => {
                let _ = std::fs::remove_file(&tmp);
                break candidate;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) if !candidate.exists() => {
                // The file system has no hard links: fall back to a rename.
                if let Err(e) = std::fs::rename(&tmp, &candidate) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                break candidate;
            }
            Err(_) => {}
        }
        name = format!("{stem}_{n}-{category}{BUNDLE_SUFFIX}");
        n += 1;
    };
    if keep > 0 {
        let all = list_bundles(dir);
        let excess = all.len().saturating_sub(keep);
        if excess > 0 {
            let unannounced: Vec<String> = super::crash::unannounced_crashes(dir)
                .into_iter()
                .map(|b| b.name)
                .collect();
            for old in all
                .iter()
                .filter(|b| b.path != path && !unannounced.contains(&b.name))
                .take(excess)
            {
                let _ = std::fs::remove_file(&old.path);
            }
        }
    }
    Ok(BundleInfo {
        name,
        bytes: bytes.len() as u64,
        path,
        category: input.category,
        entries,
    })
}

/// Whether `name` is a bundle file name.
#[must_use]
pub fn is_bundle_name(name: &str) -> bool {
    name.starts_with(BUNDLE_PREFIX)
        && name.ends_with(BUNDLE_SUFFIX)
        && !name.contains(['/', '\\', ':'])
        && !name.contains("..")
}

/// The category encoded in a bundle file name.
#[must_use]
pub fn category_of(name: &str) -> Option<FeedbackCategory> {
    let stem = name.strip_suffix(BUNDLE_SUFFIX)?;
    FeedbackCategory::parse(stem.rsplit('-').next()?)
}

/// The bundles in `dir`, oldest first (by name, which starts with the UTC time).
#[must_use]
pub fn list_bundles(dir: &Path) -> Vec<BundleFile> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<BundleFile> = read
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            if !is_bundle_name(&name) {
                return None;
            }
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| BundleFile {
                category: category_of(&name),
                modified_unix_ms: meta
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as u64),
                bytes: meta.len(),
                path: e.path(),
                name,
            })
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files
}

/// Reads a bundle's entries (CRC-checked).
pub fn read_bundle(path: &Path) -> Result<Vec<super::zip::ZipEntry>, String> {
    read_entries(&read_capped(path)?)
}

/// A bundle's entry names.
pub fn bundle_entry_names(path: &Path) -> Result<Vec<String>, String> {
    entry_names(&read_capped(path)?)
}

/// A bundle file's bytes, refusing one past [`MAX_BUNDLE_BYTES`]: anything that can write the
/// feedback folder (a synced account) could otherwise force an allocation of any size.
fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_BUNDLE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(format!(
            "{}: larger than the {MAX_BUNDLE_BYTES} bytes a bundle may be",
            path.display()
        ));
    }
    Ok(bytes)
}

/// The § 2.4 hitch report over `frames`.
#[must_use]
pub fn hitch_report(frames: &[FrameSample]) -> HitchReport {
    let hitch_frames: Vec<HitchFrame> = frames
        .iter()
        .map(|s| HitchFrame {
            frame_ms: s.frame_ms,
            cpu_ms: s.cpu_ms,
            gpu_ms: s.gpu_ms,
            counters: s.counters(),
        })
        .collect();
    let mut report = HitchReport::new(&hitch_frames, HitchTargets::SPEC);
    // A tick with no fresh renderer publication records 0.0 for "absent" (ADR-7965): the CPU and
    // GPU percentiles are over the fresh samples only. A frame without timing metadata (an older
    // record) keeps the old rule and counts its values.
    let fresh = |pick: fn(&FrameSample) -> Option<f32>| {
        let mut values: Vec<f32> = frames.iter().filter_map(pick).collect();
        values.sort_by(f32::total_cmp);
        values
    };
    let cpu = fresh(|s| s.timing.is_none_or(|t| t.cpu_fresh).then_some(s.cpu_ms));
    let gpu = fresh(|s| s.timing.is_none_or(|t| t.gpu_fresh).then_some(s.gpu_ms));
    report.cpu_p50_ms = regia_render::hitch::percentile_sorted(&cpu, 0.5);
    report.cpu_p99_ms = regia_render::hitch::percentile_sorted(&cpu, 0.99);
    report.gpu_p50_ms = regia_render::hitch::percentile_sorted(&gpu, 0.5);
    report.gpu_p99_ms = regia_render::hitch::percentile_sorted(&gpu, 0.99);
    report
}

/// How many of `frames` carry a fresh renderer CPU and GPU sample (a frame without timing
/// metadata counts as fresh, as in [`hitch_report`]).
fn fresh_sample_counts(frames: &[FrameSample]) -> (usize, usize) {
    let cpu = frames
        .iter()
        .filter(|s| s.timing.is_none_or(|t| t.cpu_fresh))
        .count();
    let gpu = frames
        .iter()
        .filter(|s| s.timing.is_none_or(|t| t.gpu_fresh))
        .count();
    (cpu, gpu)
}

/// `hitch_report.txt`: the report over `frames` (recorded over `window_s` seconds) and how many
/// fresh renderer samples its CPU and GPU percentiles read.
fn hitch_text(frames: &[FrameSample], hitch: &HitchReport, window_s: f64) -> String {
    if frames.is_empty() {
        return "no frames recorded\n".to_owned();
    }
    let (cpu, gpu) = fresh_sample_counts(frames);
    format!(
        "Hitch report over the last {:.1} s ({} frames, specs/27 § 2.4)\n\
         renderer samples: CPU {cpu} fresh, GPU {gpu} fresh of {} frames (CPU and GPU percentiles \
         read fresh samples only)\n{}",
        window_s,
        frames.len(),
        frames.len(),
        hitch.summary()
    )
}

fn encode_png(image: &CapturedImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&image.rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn frame_csv(frames: &[FrameSample]) -> String {
    let mut s = String::with_capacity(48 * frames.len() + 128);
    s.push_str(
        "t_s,frame_ms,cpu_ms,gpu_ms,pipelines,bind_groups,buffers,textures,blocking_polls,inline_bakes\n",
    );
    for f in frames {
        let _ = writeln!(
            s,
            "{:.4},{:.3},{:.3},{:.3},{},{},{},{},{},{}",
            f.t_s,
            f.frame_ms,
            f.cpu_ms,
            f.gpu_ms,
            f.pipelines,
            f.bind_groups,
            f.buffers,
            f.textures,
            f.blocking_polls,
            f.inline_bakes
        );
    }
    s
}

fn system_text(sys: &SystemInfo) -> String {
    let or = |s: &str, d: &'static str| if s.is_empty() { d.to_owned() } else { s.to_owned() };
    format!(
        "os: {} ({})\ndevice: {}\ncpu: {}\ncpus: {}\nram_mib: {}\ngpu: {}\n",
        sys.os.os,
        or(&sys.os.version, "unknown version"),
        or(&sys.os.device_model, "unknown"),
        or(&sys.cpu_model, "unknown"),
        sys.os.cpu_count,
        sys.os.total_ram_bytes / (1024 * 1024),
        or(&sys.gpu, "(not reported)"),
    )
}

fn problems_text(problems: Option<&[(String, String)]>) -> String {
    match problems {
        None => "no runtime renderer in this app\n".into(),
        Some([]) => "ok: no problems\n".into(),
        Some(list) => {
            let mut s = String::new();
            for (code, message) in list {
                let _ = writeln!(s, "{code}: {message}");
            }
            s
        }
    }
}

fn log_text(records: &[LogRecord]) -> String {
    if records.is_empty() {
        return "(log ring empty: logging was not initialized)\n".into();
    }
    let mut s = String::with_capacity(records.len() * 96);
    for r in records {
        let _ = writeln!(
            s,
            "{} {:5} {}: {}",
            utc_stamp_ms(r.unix_ms),
            r.level,
            r.target,
            regia_core::privacy::scrub_report(&r.message)
        );
    }
    s
}

/// `YYYY-MM-DD<sep>HH:MM:SS` in UTC.
fn utc_stamp(secs: u64, sep: &str) -> String {
    let (y, mo, d, h, mi, s) = civil_date_time(secs);
    format!("{y:04}-{mo:02}-{d:02}{sep}{h:02}:{mi:02}:{s:02}")
}

fn utc_stamp_ms(ms: u64) -> String {
    let (_, _, _, h, mi, s) = civil_date_time(ms / 1000);
    format!("{h:02}:{mi:02}:{s:02}.{:03}", ms % 1000)
}

pub(crate) fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The packaged `build_info.json` beside the executable (`regia package` writes it), verbatim.
#[must_use]
pub fn package_build_info() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let text = std::fs::read_to_string(exe.parent()?.join("build_info.json")).ok()?;
    (text.len() < 64 * 1024).then_some(text)
}

/// The CPU model name: `/proc/cpuinfo` on Linux and Android, `PROCESSOR_IDENTIFIER` on Windows.
#[must_use]
pub fn cpu_model() -> String {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
            for key in ["model name", "Hardware", "Processor"] {
                if let Some(value) = text.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    (k.trim() == key).then(|| v.trim().to_owned())
                }) && !value.is_empty()
                {
                    return value;
                }
            }
        }
        String::new()
    }
    #[cfg(target_os = "windows")]
    {
        regia_core::brand::env_var("PROCESSOR_IDENTIFIER").unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "windows")))]
    {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_pipeline_cache_yields_to_the_total_bundle_limit() {
        use regia_render::pipeline_cache::{PipelineCacheKey, encode};
        let root = regia_test_utils::unique_temp_dir("feedback-cache-total-limit");
        let key = PipelineCacheKey {
            backend: "Vulkan".into(),
            vendor: 1,
            device: 2,
            driver: "test".into(),
            driver_info: "1".into(),
            wgpu_version: "30".into(),
            shader_hash: "test".into(),
        };
        let path = root.join("saved.fpc");
        regia_test_utils::write_atomic(&path, &encode(&key, &[7; 4096])).expect("saved cache");
        let mut input = BundleInput::new(FeedbackCategory::Performance, "keep this report");
        let baseline = build_bundle(&input).0.len();
        input.pipeline_cache = Some(super::super::PipelineCacheAttachment { path, key });
        let limit = baseline as u64 + 1024;
        let (bytes, names) = build_bundle_limited(&input, limit);
        assert!(bytes.len() as u64 <= limit);
        assert!(!names.iter().any(|name| name.ends_with(".fpc")));
        let entries = read_entries(&bytes).expect("readable report");
        let meta: Value = serde_json::from_slice(&entries[0].data).expect("metadata");
        assert_eq!(meta["note"], "keep this report");
        assert_eq!(
            meta["pipeline_cache_missing"],
            "pipeline cache omitted: bundle byte limit"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    fn sample(t: f64, ms: f32) -> FrameSample {
        FrameSample {
            t_s: t,
            frame_ms: ms,
            ..FrameSample::default()
        }
    }

    #[test]
    fn optional_timing_metadata_preserves_csv_and_explains_absent_samples() {
        let timing = RenderTimingSample {
            cpu_frame_id: Some(4),
            gpu_frame_id: Some(2),
            cpu_absent: Some(RenderTimingAbsence::Hidden),
            gpu_absent: Some(RenderTimingAbsence::Hidden),
            ..Default::default()
        };
        let frame = FrameSample {
            timing: Some(timing),
            ..sample(1.0, 100.0)
        };
        let mut input = BundleInput::new(FeedbackCategory::Performance, "hidden interval");
        input.frames.push(frame);
        let (bytes, _) = build_bundle(&input);
        let entries = read_entries(&bytes).expect("bundle entries");
        let metadata = entries
            .iter()
            .find(|e| e.name == "feedback.json")
            .expect("metadata");
        let json: Value = serde_json::from_slice(&metadata.data).expect("JSON metadata");
        assert_eq!(json["format"], BUNDLE_FORMAT);
        assert_eq!(json["render_timing"][0]["timing"]["cpu_frame_id"], 4);
        assert_eq!(json["render_timing"][0]["timing"]["cpu_absent"], "hidden");
        assert_eq!(json["render_timing"][0]["cpu_sample_ms"], Value::Null);
        assert_eq!(
            frame_csv(&[frame]),
            "t_s,frame_ms,cpu_ms,gpu_ms,pipelines,bind_groups,buffers,textures,blocking_polls,inline_bakes\n1.0000,100.000,0.000,0.000,0,0,0,0,0,0\n"
        );
    }

    #[test]
    fn ring_overwrites_and_windows_by_time() {
        let mut ring = FrameRing::new(4);
        assert!(ring.is_empty());
        assert!(ring.last_window(30.0).is_empty());
        for i in 0..6 {
            ring.push(sample(f64::from(i), 10.0 + i as f32));
        }
        assert_eq!(ring.len(), 4);
        let all = ring.last_window(100.0);
        assert_eq!(
            all.iter().map(|s| s.t_s).collect::<Vec<_>>(),
            vec![2.0, 3.0, 4.0, 5.0]
        );
        let recent = ring.last_window(1.5);
        assert_eq!(recent.iter().map(|s| s.t_s).collect::<Vec<_>>(), vec![4.0, 5.0]);
    }

    #[test]
    fn category_names_round_trip() {
        for c in FeedbackCategory::PLAYER
            .into_iter()
            .chain([FeedbackCategory::Crash])
        {
            assert_eq!(FeedbackCategory::parse(c.as_str()), Some(c));
        }
        assert_eq!(
            category_of("feedback-2026-10-09_12-00-00-visual.zip"),
            Some(FeedbackCategory::Visual)
        );
        assert_eq!(
            category_of("feedback-2026-10-09_12-00-00_1-crash.zip"),
            Some(FeedbackCategory::Crash)
        );
        assert!(is_bundle_name("feedback-2026-10-09_12-00-00-bug.zip"));
        assert!(!is_bundle_name("../feedback-x.zip"));
        assert!(!is_bundle_name("feedback-x.zip.tmp"));
    }

    /// Pruning removes the oldest ordinary bundles but never a crash bundle that was not
    /// announced yet.
    #[test]
    fn prune_keeps_unannounced_crash_bundles() {
        let dir = std::env::temp_dir().join(format!("regia-feedback-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let at = |category, unix_ms| BundleInput {
            unix_ms,
            ..BundleInput::new(category, "")
        };
        let base = 1_791_000_000_000u64;
        let crash = write_bundle(&dir, &at(FeedbackCategory::Crash, base), 2).expect("crash");
        for i in 1..=4u64 {
            write_bundle(&dir, &at(FeedbackCategory::Bug, base + i * 1000), 2).expect("bug");
        }
        let names: Vec<String> = list_bundles(&dir).into_iter().map(|b| b.name).collect();
        assert!(names.contains(&crash.name), "unannounced crash kept: {names:?}");
        // The protected crash still counts toward `keep`: it and the newest report remain.
        assert_eq!(names.len(), 2, "the crash plus the newest report: {names:?}");
        // Once announced, it is pruned like any other bundle.
        let unannounced = super::super::crash::unannounced_crashes(&dir);
        super::super::crash::mark_announced(&dir, &unannounced).expect("announce");
        write_bundle(&dir, &at(FeedbackCategory::Bug, base + 9000), 2).expect("bug");
        let names: Vec<String> = list_bundles(&dir).into_iter().map(|b| b.name).collect();
        assert!(!names.contains(&crash.name), "{names:?}");
        assert_eq!(names.len(), 2, "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Concurrent writers with the same second and category each get their own bundle: no shared
    /// temp file, no two callers returned the same path.
    #[test]
    fn concurrent_writes_get_distinct_paths() {
        let dir = regia_test_utils::unique_temp_dir("feedback-concurrent");
        let input = BundleInput {
            unix_ms: 1_800_000_000_000,
            ..BundleInput::new(FeedbackCategory::Bug, "")
        };
        let paths: Vec<PathBuf> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| write_bundle(&dir, &input, 0).expect("write").path))
                .collect();
            workers.into_iter().map(|w| w.join().expect("join")).collect()
        });
        let mut unique = paths.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 8, "{paths:?}");
        assert_eq!(list_bundles(&dir).len(), 8);
        let leftovers = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_parts_say_why() {
        let input = BundleInput::new(FeedbackCategory::Other, "");
        let (bytes, names) = build_bundle(&input);
        assert!(!names.contains(&"screenshot.png".to_owned()));
        let entries = read_entries(&bytes).expect("readable");
        let get = |n: &str| {
            String::from_utf8(entries.iter().find(|e| e.name == n).expect("entry").data.clone())
                .expect("utf8")
        };
        assert!(get("render_problems.txt").contains("no runtime renderer"));
        assert!(get("settings.ron").contains("no GameSettings"));
        assert!(get("hitch_report.txt").contains("no frames"));
        let meta: Value = serde_json::from_str(&get("feedback.json")).expect("json");
        assert_eq!(meta["screenshot"], Value::Null);
        assert_eq!(meta["screenshot_missing"], "no screenshot requested");
    }

    /// A Windows native crash (an exception code, not a signal) writes the same bundle layout as
    /// a Linux one: crash.txt, host.json, feedback.json and minidump.dmp.
    #[test]
    fn windows_native_crash_bundle_has_the_linux_layout() {
        let mut input = BundleInput::new(FeedbackCategory::Crash, "");
        input.host = vec![("map".to_owned(), "harbor".to_owned())];
        input.native_crash = Some(NativeCrash {
            signal: 0xC000_0005,
            code: 1,
            address: 0xdead_beef,
            pid: 4242,
            tid: 7,
            thread: "render".into(),
            minidump: Some(b"MDMP-test".to_vec()),
            minidump_error: None,
            platform: "windows".into(),
            stack: Vec::new(),
        });
        let (bytes, names) = build_bundle(&input);
        for n in ["crash.txt", "host.json", "feedback.json", "minidump.dmp"] {
            assert!(names.contains(&n.to_owned()), "{n} in {names:?}");
        }
        let entries = read_entries(&bytes).expect("readable");
        let get = |n: &str| entries.iter().find(|e| e.name == n).expect("entry").data.clone();
        assert_eq!(get("minidump.dmp"), b"MDMP-test");
        let crash = String::from_utf8(get("crash.txt")).expect("utf8");
        assert!(
            crash.contains("EXCEPTION_ACCESS_VIOLATION (write to an inaccessible address)"),
            "{crash}"
        );
        assert!(
            crash.contains("0x00000000deadbeef") && crash.contains("render (tid 7)"),
            "{crash}"
        );
        let meta: Value = serde_json::from_slice(&get("feedback.json")).expect("json");
        assert_eq!(meta["crash"]["native"], true);
        assert_eq!(meta["crash"]["signal"], "EXCEPTION_ACCESS_VIOLATION");
        assert_eq!(meta["crash"]["minidump"], "minidump.dmp");
    }

    #[test]
    fn exception_codes_never_read_as_signals() {
        let unknown = NativeCrash {
            signal: 0xE06D_7363,
            ..NativeCrash::default()
        };
        assert_eq!(unknown.reason(), "exception 0xe06d7363");
        let segv = NativeCrash {
            signal: 11,
            code: 1,
            ..NativeCrash::default()
        };
        assert_eq!(segv.reason(), "SIGSEGV (address not mapped)");
    }

    /// rev-g S1: one slow render followed by eight update-only ticks (loading, the DO27 case): the
    /// hitch report's CPU and GPU percentiles are the one real sample, not pulled to 0 ms by the
    /// absent ticks; the report text counts the fresh samples, and crash.txt says the newest tick
    /// has no fresh sample instead of printing 0.00 ms. Frames without timing metadata (older
    /// records) keep the old rule: their values count as measured.
    #[test]
    fn hitch_percentiles_use_fresh_renderer_samples_only() {
        let fresh = RenderTimingSample {
            cpu_frame_id: Some(10),
            gpu_frame_id: Some(10),
            cpu_fresh: true,
            gpu_fresh: true,
            ..RenderTimingSample::default()
        };
        let stale = RenderTimingSample {
            cpu_fresh: false,
            gpu_fresh: false,
            cpu_absent: Some(RenderTimingAbsence::NoScene),
            gpu_absent: Some(RenderTimingAbsence::NoScene),
            ..fresh
        };
        let mut frames = vec![FrameSample {
            timing: Some(fresh),
            t_s: 1.0,
            frame_ms: 120.0,
            cpu_ms: 40.0,
            gpu_ms: 90.0,
            ..FrameSample::default()
        }];
        for k in 0..8u32 {
            frames.push(FrameSample {
                timing: Some(stale),
                t_s: 1.1 + f64::from(k) * 0.016,
                frame_ms: 16.0,
                ..FrameSample::default()
            });
        }
        let report = hitch_report(&frames);
        assert_eq!(report.frames, 9);
        assert_eq!(
            (report.cpu_p50_ms, report.cpu_p99_ms),
            (40.0, 40.0),
            "absent CPU ticks counted as 0 ms"
        );
        assert_eq!(
            (report.gpu_p50_ms, report.gpu_p99_ms),
            (90.0, 90.0),
            "absent GPU ticks counted as 0 ms"
        );
        assert_eq!(fresh_sample_counts(&frames), (1, 1));
        let text = hitch_text(&frames, &report, 2.0);
        assert!(
            text.contains("renderer samples: CPU 1 fresh, GPU 1 fresh of 9 frames"),
            "{text}"
        );
        // Legacy frames (no metadata): every value counts, zeros included.
        let legacy: Vec<FrameSample> = frames
            .iter()
            .map(|f| FrameSample { timing: None, ..*f })
            .collect();
        let report = hitch_report(&legacy);
        assert_eq!(report.cpu_p50_ms, 0.0);
        assert_eq!(report.cpu_p99_ms, 40.0);
        // crash.txt: the newest tick had no fresh sample.
        let crash = native_crash_text(&NativeCrash::default(), &frames);
        assert!(
            crash.contains("renderer CPU no fresh sample, GPU no fresh sample"),
            "{crash}"
        );
        let crash = native_crash_text(&NativeCrash::default(), &frames[..1]);
        assert!(crash.contains("renderer CPU 40.00 ms, GPU 90.00 ms"), "{crash}");
    }
}
