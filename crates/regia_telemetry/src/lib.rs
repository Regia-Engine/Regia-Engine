// SPDX-License-Identifier: MPL-2.0
//! Structured engine telemetry (`book/user-guide/telemetry.md`).
//!
//! [`TelemetryPlugin`] (part of `regia::DefaultPlugins`) turns what the engine already measures
//! into a few `tracing` events with stable targets and fields, cheap enough for a player's machine:
//!
//! | Target | When |
//! |---|---|
//! | `regia::session` | once, when the render device exists and the first frame rendered |
//! | `regia::boot` | when a boot mark is recorded after the session header |
//! | `regia::frame_summary` | every 10 s, and on [`Telemetry::request_summary`] (level end) |
//! | `regia::hitch` | a frame over 2 × the rolling median or more than 50 ms above it, over the pacing floor and not a swapchain wait (rate-limited) |
//! | `regia::gpu_error` / `regia::device_lost` | wgpu's error and device-lost callbacks |
//! | `regia::surface` | surface acquire errors in a summary window |
//! | `regia::problems` | `render.problems` changed |
//! | `regia::stream` | texture budget exceeded, eviction storms, wrong-platform textures, slow asset loads, pak corruption, pipeline creation after warm-up |
//! | `regia::phase` | a transition of the session: the engine's (`boot_done`, `shaders_ready`, `scene_load_start/end`, `quit_start/done`) and the host's, through [`phase()`] |
//! | `regia::exit` | once, at the end of the clean exit: every step's time |
//!
//! Per-frame work aggregates into buffers allocated once (no log line and no allocation per
//! frame; a test counts allocations over 1000 frames). Events carry a `data` field holding the
//! whole record as JSON; [`json_layer`] embeds it as an object. Where events go is the host
//! subscriber's decision.

mod breakdown;
mod cause;
// The counter and mark listing the editor Profiler's Monitors tab reads (P-312, P-318).
pub mod counters;
mod engine_phases;
mod gpu;
mod hist;
mod json;
mod live;
mod phase;
mod probe;
/// Profile captures (`.rprof.json`, ADR-2323).
pub mod profile_capture;
/// Comparing two profile captures (ADR-9740).
pub mod profile_compare;
mod rate;
mod summary;
pub mod trace;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regia_app::RealTime;
use regia_app::prelude::*;
use regia_core::profiling::FrameProfiler;
use regia_core::telemetry::{self as taps, Counter, Gauge, Note};
use regia_ecs::ecs::message::MessageCursor;
use regia_framework::feedback::{FrameSample, FrameSampler};
use regia_render::{CreateRecord, FrameRenderer, GameFrameWarm, GpuTimings, RECENT_CREATES, RuntimeRenderer};
use serde_json::{Value, json};

pub use gpu::GPU_ERROR_MESSAGE_BYTES;

/// Installs the `regia::gpu_error` / `regia::device_lost` hooks on a device the host
/// created itself ([`TelemetryPlugin`] does this for the engine's render device). See
/// [`TelemetryOptions::gpu_error_panics`] for `panic_on_error`.
pub fn install_device_hooks(device: &wgpu::Device, panic_on_error: bool) {
    gpu::install(device, panic_on_error);
}
pub use cause::{OUTSIDE_FRAME_SHARE, SPAN_CAUSE_SHARE, hitch_floor_ms, is_present_wait, pacing_period_ms};
pub use hist::{FPS_BUCKETS, FPS_EDGES_MS, Histogram};
pub use json::{
    JsonLayer, JsonLayerGuard, JsonLayerOptions, JsonLayerStats, json_layer, json_layer_to, jsonl_path,
};
pub use live::{
    CPU_BOUND_SHARE, EVENT_WAIT_SHARE, FrameBound, GPU_BOUND_SHARE, LiveFrame, LiveRunnerState, UnknownBound,
    is_loading_phase, render_scale_permille,
};
pub use phase::phase;
pub use summary::{
    AudioInfo, DisplayInfo, ErrorsInfo, ExitInfo, ExitStepInfo, FrameStats, HitchInfo, MemoryInfo,
    MonitorInfo, PARTIAL_INTERVAL_S, PreviousExit, RegiaInfo, SUMMARY_SCHEMA, SessionInfo, SessionSummary,
    SettingsInfo, ShadersInfo, Stall, SummaryPaths, SystemInfo, detect_previous_exit, write_atomic,
};

/// The filter to give the JSON layer (as a per-layer `EnvFilter`): Regia Engine at `debug`, the
/// chatty GPU and shader crates at `warn`, the font database at `error` (v0.5.1). Without a filter, naga's shader-compiler `log`
/// output alone is ~140 k lines per level load.
pub const RECOMMENDED_FILTER: &str = "info,regia=debug,regia_render=debug,regia_terrain=debug,wgpu_core=warn,wgpu_hal=warn,naga=warn,fontdb=error";

/// The stable event targets.
pub mod target {
    /// Session header (once).
    pub const SESSION: &str = "regia::session";
    /// Boot marks recorded after the header.
    pub const BOOT: &str = "regia::boot";
    /// Periodic and level-end frame summary.
    pub const FRAME_SUMMARY: &str = "regia::frame_summary";
    /// One slow frame with its attribution.
    pub const HITCH: &str = "regia::hitch";
    /// wgpu uncaptured error.
    pub const GPU_ERROR: &str = "regia::gpu_error";
    /// wgpu device lost.
    pub const DEVICE_LOST: &str = "regia::device_lost";
    /// Surface acquire errors.
    pub const SURFACE: &str = "regia::surface";
    /// `render.problems` changed.
    pub const PROBLEMS: &str = "regia::problems";
    /// Streaming and content events.
    pub const STREAM: &str = "regia::stream";
    /// The telemetry machinery itself (JSON writer drops, size cap).
    pub const TELEMETRY: &str = "regia::telemetry";
    /// Session phase transitions, the engine's and the host's (spec 28 § 3).
    pub const PHASE: &str = "regia::phase";
    /// The clean exit's step timings (ADR-188).
    pub const EXIT: &str = "regia::exit";
}

/// How much telemetry to emit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TelemetryLevel {
    /// Header, summaries, warnings and errors only (a struggling machine).
    Normal,
    /// Everything, including every (rate-limited) hitch and boot event. The alpha default.
    #[default]
    Verbose,
    /// v0.5.1 (ADR-8417): what `Normal` emits, made safe to upload from a player build. The
    /// session header is built from the allowlist [`SHIP_HEADER_KEYS`] only (no OS build, CPU
    /// model, monitor names, adapter name or PCI ids, settings dump, host fields or boot marks;
    /// RAM and VRAM as [`memory_tier_gb`] tiers), no `regia::hitch` event is written (hitches are
    /// counted in the summaries), `regia::problems` carries codes without their message text, and
    /// `summary.json` takes its machine sections from that header with no `host` section and no
    /// detected-quality plan. Frame summaries are unchanged (`book/user-guide/telemetry.md`
    /// "Ship level").
    Ship,
}

impl TelemetryLevel {
    /// Parses `normal` / `verbose` / `ship` (case-insensitive).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(Self::Normal),
            "verbose" => Some(Self::Verbose),
            "ship" => Some(Self::Ship),
            _ => None,
        }
    }

    /// The level `REGIA_TELEMETRY` names, `None` when it is unset, `off` or not a level.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        regia_core::brand::env_var("REGIA_TELEMETRY")
            .ok()
            .and_then(|v| Self::parse(&v))
    }

    /// Stable name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Verbose => "verbose",
            Self::Ship => "ship",
        }
    }
}

/// v0.5.1 (ADR-8417): the session header keys written at [`TelemetryLevel::Ship`], the contract a
/// game can show its players (`book/user-guide/telemetry.md` "Ship level"). The Ship header is
/// built from this list alone, so a key added to the full header later stays out of Ship until it
/// is added here (with its projection in `ship_record`).
pub const SHIP_HEADER_KEYS: [&str; 8] = [
    "regia",
    "os",
    "ram_gb",
    "vram_gb",
    "adapter",
    "preset",
    "render_scale",
    "telemetry_level",
];

/// RAM tiers of the Ship header, GB: a machine reports the smallest tier at or above its usable
/// RAM (which reads a little under the installed size), and the last tier for anything above it
/// (`32` means "over 24 GB").
pub const SHIP_RAM_TIERS_GB: [u32; 6] = [4, 8, 12, 16, 24, 32];

/// VRAM tiers of the Ship header, GB, read as [`SHIP_RAM_TIERS_GB`] (`32` means "over 24 GB").
pub const SHIP_VRAM_TIERS_GB: [u32; 8] = [2, 4, 6, 8, 12, 16, 24, 32];

/// The smallest of `tiers` (ascending) at or above `gb`, or the last tier when `gb` is above
/// them all; `None` when `gb` is not a positive finite number or `tiers` is empty.
#[must_use]
pub fn memory_tier_gb(gb: f64, tiers: &[u32]) -> Option<u32> {
    if !(gb.is_finite() && gb > 0.0) {
        return None;
    }
    tiers
        .iter()
        .copied()
        .find(|&tier| gb <= f64::from(tier))
        .or_else(|| tiers.last().copied())
}

/// [`TelemetryPlugin`] settings. `REGIA_TELEMETRY=off|normal|verbose|ship` overrides
/// [`Self::enabled`] and [`Self::level`].
#[derive(Clone, Debug)]
pub struct TelemetryOptions {
    /// Emit anything at all.
    pub enabled: bool,
    /// [`TelemetryLevel`].
    pub level: TelemetryLevel,
    /// Time between `regia::frame_summary` events.
    pub summary_interval: Duration,
    /// A frame longer than this × the rolling median is a hitch…
    pub hitch_ratio: f32,
    /// …and so is a frame more than this above the rolling median, ms. Since v0.1.14 it is an
    /// excess over the median, not an absolute bound: a steady slow frame rate (every frame
    /// 66 ms) is not a hitch, a spike out of it is.
    pub hitch_abs_ms: f32,
    /// A frame shorter than this is never a hitch, ms (keeps 500 fps jitter out).
    pub hitch_min_ms: f32,
    /// v0.5.1 (ADR-7898): when a frame cap or vsync paces the loop, a frame shorter than this
    /// many pacing periods is never a hitch either ([`hitch_floor_ms`]: the floor is
    /// `max(hitch_min_ms, hitch_floor_periods × period)`). Default 1.5: a frame that missed one
    /// vsync (two periods) still counts, a frame that only jittered does not. 0 turns it off.
    pub hitch_floor_periods: f32,
    /// v0.5.1 (ADR-7898): a would-be hitch whose acquire + present wait is at least this share
    /// of its excess over the median (and whose excess stays within [`Self::hitch_abs_ms`]) is a
    /// `present_wait` ([`is_present_wait`]), counted apart and not a hitch. Default 0.5. A value
    /// over 1 effectively turns the classification off.
    pub present_wait_share: f32,
    /// Hitch events at once before the rate limit applies…
    pub hitch_burst: u32,
    /// …then one per this interval. Suppressed hitches are counted in the summary.
    pub hitch_refill: Duration,
    /// The same event key within this window is emitted once, then counted.
    pub repeat_window: Duration,
    /// How often the background probe samples RSS and VRAM.
    pub probe_interval: Duration,
    /// Sample VRAM through wgpu's allocator report (Vulkan, D3D12).
    pub vram_report: bool,
    /// Panic after logging a wgpu uncaptured error (wgpu's default behaviour). Default: debug
    /// builds only, so a player's release build logs and keeps running.
    pub gpu_error_panics: bool,
    /// Frames after the `first_game_frame` boot mark before pipeline creation counts as "after
    /// warm-up"…
    pub warmup_frames: u64,
    /// …or after this many frames when the game never records that mark.
    pub warmup_fallback_frames: u64,
    /// Texture evictions within one second that make an eviction storm.
    pub eviction_storm: u64,
    /// Write `summary.json` / `summary.partial.json` (spec 28 § 4).
    pub summary: bool,
    /// Where the final `summary.json` goes; the partial is beside it. `None` (default): next to
    /// the [`json_layer`]'s file with the same stem (`<stem>.summary.json`), and no summary at all
    /// when the process has no JSON log.
    pub summary_path: Option<std::path::PathBuf>,
}

impl Default for TelemetryOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            level: TelemetryLevel::Verbose,
            summary_interval: Duration::from_secs(10),
            hitch_ratio: 2.0,
            hitch_abs_ms: 50.0,
            hitch_min_ms: 8.0,
            hitch_floor_periods: 1.5,
            present_wait_share: 0.5,
            hitch_burst: 5,
            hitch_refill: Duration::from_secs(2),
            repeat_window: Duration::from_secs(5),
            probe_interval: Duration::from_secs(10),
            vram_report: true,
            gpu_error_panics: cfg!(debug_assertions),
            warmup_frames: 120,
            warmup_fallback_frames: 1800,
            eviction_storm: 32,
            summary: true,
            summary_path: None,
        }
    }
}

impl TelemetryOptions {
    /// Applies `REGIA_TELEMETRY` (`off`, `normal`, `verbose`, `ship`).
    #[must_use]
    pub fn with_env(mut self) -> Self {
        if let Ok(v) = regia_core::brand::env_var("REGIA_TELEMETRY") {
            if v.trim().eq_ignore_ascii_case("off") || v.trim() == "0" {
                self.enabled = false;
            } else if let Some(level) = TelemetryLevel::parse(&v) {
                self.enabled = true;
                self.level = level;
            }
        }
        self
    }
}

/// Host fields copied into the session header (game version, content hashes, …). Set them
/// before the first frame renders.
#[derive(Resource, Default, Debug, Clone)]
pub struct TelemetryHost {
    fields: BTreeMap<String, String>,
    values: BTreeMap<String, Value>,
}

impl TelemetryHost {
    /// Sets one field.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.fields.insert(key.into(), value.into());
    }

    /// The fields.
    #[must_use]
    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }

    /// Sets one structured value for `summary.json`'s free-form `host` section (recommended keys:
    /// `app`, `version`, `git`, `build`, `data_tier`, `network`). Unlike [`Self::set`] it can hold
    /// objects and numbers, and may change during the session: the section is read again at each
    /// partial summary and at the clean exit.
    pub fn set_value(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.values.insert(key.into(), value.into());
    }

    /// The `host` section: [`Self::set`] fields as strings, overridden by [`Self::set_value`].
    #[must_use]
    pub fn summary_section(&self) -> Value {
        let mut map = serde_json::Map::new();
        for (k, v) in &self.fields {
            map.insert(k.clone(), Value::String(v.clone()));
        }
        for (k, v) in &self.values {
            map.insert(k.clone(), v.clone());
        }
        Value::Object(map)
    }
}

/// The frame profiler the hitch event reads its top CPU spans from (`regia::DefaultPlugins`
/// inserts the process profiler; it only has data when the host's subscriber includes it).
#[derive(Resource, Clone)]
pub struct SpanProfiler(pub FrameProfiler);

/// Counts since start (`telemetry.get_last_summary` reports them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TelemetryCounts {
    /// Frames observed.
    pub frames: u64,
    /// `regia::frame_summary` events.
    pub summaries: u64,
    /// Hitches detected.
    pub hitches: u64,
    /// Of which emitted as `regia::hitch`.
    pub hitch_events: u64,
    /// v0.5.1: frames that would have been hitches but waited on the swapchain or display
    /// (acquire + present) for most of their excess ([`is_present_wait`]); not in `hitches`.
    pub present_waits: u64,
    /// `regia::problems` events.
    pub problems_events: u64,
    /// `regia::stream` events.
    pub stream_events: u64,
    /// `regia::surface` events.
    pub surface_events: u64,
    /// Events held back by the repeat limiter.
    pub suppressed: u64,
    /// Frames that showed the paused-scene hold's kept image (ADR-7890): no scene pass ran. They
    /// are counted here and in the summary's `held` section, never in the frame-time histograms,
    /// the hitch median or the live bound classification.
    pub held_frames: u64,
}

/// System set of the telemetry systems (in `Last`).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TelemetrySystems;

static LAST_SUMMARY: Mutex<Option<Arc<str>>> = Mutex::new(None);

/// The newest `regia::frame_summary` record as JSON, from anywhere (a crash or stall
/// handler adds it to its report).
#[must_use]
pub fn last_summary_json() -> Option<Arc<str>> {
    LAST_SUMMARY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Asks for a `regia::frame_summary` at the end of this frame with `reason` (e.g.
/// `"level_end"`); the window restarts after it.
pub fn request_summary(world: &mut World, reason: &'static str) {
    if let Some(mut t) = world.get_resource_mut::<Telemetry>() {
        t.request_summary(reason);
    }
}

/// Pass keys of `gpu_pass_ms` (`forward_glass` since v0.1.17).
const PASS_COUNT: usize = 19;

const PASS_NAMES: [&str; PASS_COUNT] = [
    "shadows",
    "clusters",
    "gi_probes",
    "prepass",
    "cull",
    "ssao",
    "contact_shadows",
    "forward",
    "terrain_forward",
    "forward_glass",
    "ssgi",
    "ssr",
    "atmosphere",
    "clouds",
    "fog",
    "upscale",
    "post",
    "scene",
    "frame",
];

fn pass_values(t: &GpuTimings) -> [f32; PASS_COUNT] {
    [
        t.shadows_ms,
        t.clusters_ms,
        t.gi_ms,
        t.prepass_ms,
        t.cull_ms,
        t.ssao_ms,
        t.contact_shadows_ms,
        t.forward_ms,
        t.shared_forward_ms,
        t.forward_glass_ms,
        t.ssgi_ms,
        t.ssr_ms,
        t.atmosphere_ms,
        t.clouds_ms,
        t.fog_ms,
        t.taau_ms,
        t.post_ms,
        t.scene_ms,
        t.frame_ms,
    ]
}

/// Frames in the rolling median.
const MEDIAN_FRAMES: usize = 63;
/// Frames before hitch detection starts (the median needs a history).
const MEDIAN_MIN: usize = 30;
/// A frame interval longer than this is a gap (suspend, debugger, minimised), not a frame.
const GAP_MS: f32 = 5000.0;
/// Frames a hitch waits for its GPU timings (they are read back two frames late).
const GPU_LAG_FRAMES: u32 = 2;

#[derive(Default, Clone, Copy)]
struct Creates {
    pipelines: u64,
    bind_groups: u64,
    buffers: u64,
    textures: u64,
    blocking_polls: u64,
    inline_bakes: u64,
}

impl Creates {
    fn add(&mut self, s: &FrameSample) {
        self.pipelines += u64::from(s.pipelines);
        self.bind_groups += u64::from(s.bind_groups);
        self.buffers += u64::from(s.buffers);
        self.textures += u64::from(s.textures);
        self.blocking_polls += u64::from(s.blocking_polls);
        self.inline_bakes += u64::from(s.inline_bakes);
    }

    fn json(&self) -> Value {
        json!({
            "pipelines": self.pipelines, "bind_groups": self.bind_groups, "buffers": self.buffers,
            "textures": self.textures, "blocking_polls": self.blocking_polls, "inline_bakes": self.inline_bakes,
        })
    }
}

struct PendingHitch {
    record: Value,
    frames_left: u32,
}

/// Telemetry state and results. Insert by [`TelemetryPlugin`].
#[derive(Resource)]
pub struct Telemetry {
    opts: TelemetryOptions,
    sampler: FrameSampler,
    frame: u64,
    counts: TelemetryCounts,
    // Window aggregation (preallocated).
    window_start_s: f64,
    window_frames: u64,
    gaps: u64,
    frame_hist: Histogram,
    cpu_hist: Histogram,
    gpu_hist: Histogram,
    pass_sum: [f64; PASS_COUNT],
    pass_max: [f32; PASS_COUNT],
    pass_frames: u64,
    /// Window sums: drawn, instanced, per-object, culled, skipped (missing render entry), skinned
    /// culled by their joint-palette bound.
    draws_sum: [u64; 6],
    draws_max: u64,
    zero_key_moving_max: u64,
    draw_frames: u64,
    /// The renderer's `skipped_draws_total` at the last sample: the window's skipped count is
    /// the growth of the total, so a sample taken without a new frame adds nothing.
    skipped_total_seen: u64,
    /// ADR-385: the renderer's `frame_scratch_stats().growths` at the last sample, and the
    /// window's growth of it (all, and after warm-up).
    scratch_growths_seen: u64,
    scratch_growths_window: u64,
    scratch_growths_after_warm: u64,
    /// Window sums of [`shadow_cpu_values`] (ms) and the largest frame total.
    shadow_cpu_sum: [f64; SHADOW_CPU_PARTS],
    shadow_cpu_max: f32,
    /// v0.1.18: the renderer's emissive fit counters at the window start, and the window's
    /// largest emissive collection (frame-thread ms).
    emissive_at_window: regia_render::emissive_lights::EmissiveFitStats,
    emissive_ms_max: f32,
    creates: Creates,
    creates_after_warm: Creates,
    /// What the creations after warm-up were, per site (ADR-7885): kept for every warm frame,
    /// also while the `pipeline_after_warmup` event is rate-limited.
    late_sites: LateSites,
    hitches_window: u64,
    hitches_suppressed_window: u64,
    /// v0.5.1: present-wait frames in the window ([`TelemetryCounts::present_waits`]).
    present_waits_window: u64,
    /// The pacing period of the frame cap or vsync refresh, ms (0: unpaced).
    pacing_period_ms: f32,
    /// The current monitor's refresh rate from the session header.
    refresh_hz: Option<f32>,
    /// This frame's acquire + present wait when a window frame was presented since the last
    /// telemetry frame, else 0.
    frame_wait_ms: f32,
    frame_wait_seq: u64,
    /// Hitches waiting one frame for their update's phases (cause and `interval_ms`).
    pending_causes: [Option<cause::PendingCause>; cause::PENDING_CAUSES],
    /// This frame's and the previous frame's window events.
    window_events: cause::WindowEvents,
    window_events_prev: cause::WindowEvents,
    window_mode: Option<&'static str>,
    window_size: Option<(u32, u32)>,
    window_focused: Option<bool>,
    /// The phase epoch last read, and whether its newest marker is a load start.
    phase_epoch_seen: u64,
    loading: bool,
    counters_at_window: [u64; Counter::COUNT],
    /// Held frames of the window (ADR-7890), and the sum and count of their timed GPU times.
    held_window: u64,
    held_gpu_sum: f64,
    held_gpu_n: u64,
    /// The window path's hold report sequence seen last (`regia_render::scene_hold::last_window_frame`).
    hold_seq_seen: u64,
    // Hitches.
    median: [f32; MEDIAN_FRAMES],
    median_len: usize,
    median_next: usize,
    hitch_bucket: rate::TokenBucket,
    pending_hitch: Option<PendingHitch>,
    // Events.
    repeats: rate::RateLimiter,
    slow_repeats: rate::RateLimiter,
    problems_key: Option<(usize, u64)>,
    notes: Box<[Note; taps::NOTE_CAPACITY]>,
    created: Box<[CreateRecord; RECENT_CREATES]>,
    created_len: usize,
    last_create_seq: u64,
    evict_mark: (f64, u64),
    wrong_platform_seen: usize,
    // Session.
    boot_marks_seen: usize,
    first_game_frame_at: Option<u64>,
    header_done: bool,
    hooks_installed: bool,
    probe: Arc<probe::ProbeValues>,
    probe_started: bool,
    /// The probe thread (it holds a device clone): stopped and joined on drop.
    probe_thread: Option<std::thread::JoinHandle<()>>,
    requested: Option<&'static str>,
    summary_seq: u64,
    last_summary: Option<Arc<str>>,
    session: Option<Arc<str>>,
    /// Frame breakdown, top systems, churn and counts (`breakdown.rs`).
    breakdown: breakdown::Breakdown,
    /// The newest frame's live signals and their window aggregates (`live.rs`).
    live: LiveFrame,
    live_window: live::LiveWindow,
    // `summary.json` (spec 28 § 4).
    agg: summary::SummaryAgg,
    /// The sections known at the session header (machine, display, settings).
    static_info: Option<SessionSummary>,
    sink: Option<summary::SummarySink>,
    host_section: Option<Value>,
    last_now_s: f64,
    /// Session seconds as the sum of the real (unclamped) frame intervals.
    wall_s: f64,
    next_partial_s: f64,
    upscaler: Option<&'static str>,
    backend: String,
    // `regia::phase` bookkeeping (`engine_phases.rs`).
    boot_done_sent: bool,
    shaders_ready_sent: bool,
    warms_started_seen: u64,
    warm_report_seen: u64,
    /// The last scene prepare whose `scene_ready` phase went out (ADR-7883).
    scene_ready_seen: u64,
    span_layer_checked: bool,
}

/// Per-frame inputs from the ECS that [`telemetry_frame`] hands to the frame step (empty for
/// [`Telemetry::observe_frame`]).
#[derive(Default, Clone, Copy)]
struct FrameInputs<'a> {
    phases: Option<&'a regia_app::FramePhases>,
    runner: Option<&'a regia_app::RunnerState>,
    counts: breakdown::FrameCounts,
    entities: Option<&'a regia_ecs::ecs::entity::Entities>,
    /// The window frame showed the scene hold's kept image (ADR-7890).
    held: Option<regia_render::SceneHoldFrame>,
    /// The frame's GPU waits; `None` reads the renderer's (`regia_render::frame_waits`).
    waits: Option<regia_render::FrameWaits>,
    /// The frame's window events.
    window: cause::WindowEvents,
    /// A game-frame warm has the runtime renderer ([`GameFrameWarm::is_warming`]).
    warming: bool,
    /// Extraction ran but produced no camera view.
    no_scene: bool,
}

impl Telemetry {
    /// State for `opts` (all aggregation buffers allocated here, once).
    #[must_use]
    pub fn new(opts: TelemetryOptions) -> Self {
        let hitch_bucket = rate::TokenBucket::new(opts.hitch_burst, opts.hitch_refill);
        let repeats = rate::RateLimiter::new(opts.repeat_window);
        Self {
            sampler: FrameSampler::default(),
            frame: 0,
            counts: TelemetryCounts::default(),
            window_start_s: 0.0,
            window_frames: 0,
            gaps: 0,
            frame_hist: Histogram::new(),
            cpu_hist: Histogram::new(),
            gpu_hist: Histogram::new(),
            pass_sum: [0.0; PASS_COUNT],
            pass_max: [0.0; PASS_COUNT],
            pass_frames: 0,
            draws_sum: [0; 6],
            draws_max: 0,
            zero_key_moving_max: 0,
            draw_frames: 0,
            skipped_total_seen: 0,
            scratch_growths_seen: 0,
            scratch_growths_window: 0,
            scratch_growths_after_warm: 0,
            shadow_cpu_sum: [0.0; SHADOW_CPU_PARTS],
            shadow_cpu_max: 0.0,
            emissive_at_window: regia_render::emissive_lights::EmissiveFitStats::default(),
            emissive_ms_max: 0.0,
            creates: Creates::default(),
            creates_after_warm: Creates::default(),
            late_sites: LateSites::new(),
            hitches_window: 0,
            hitches_suppressed_window: 0,
            present_waits_window: 0,
            pacing_period_ms: 0.0,
            refresh_hz: None,
            frame_wait_ms: 0.0,
            frame_wait_seq: 0,
            pending_causes: [None; cause::PENDING_CAUSES],
            window_events: cause::WindowEvents::default(),
            window_events_prev: cause::WindowEvents::default(),
            window_mode: None,
            window_size: None,
            window_focused: None,
            phase_epoch_seen: u64::MAX,
            loading: false,
            counters_at_window: counters_now(),
            held_window: 0,
            held_gpu_sum: 0.0,
            held_gpu_n: 0,
            hold_seq_seen: 0,
            median: [0.0; MEDIAN_FRAMES],
            median_len: 0,
            median_next: 0,
            hitch_bucket,
            pending_hitch: None,
            repeats,
            slow_repeats: rate::RateLimiter::new(Duration::from_secs(60)),
            problems_key: None,
            notes: Box::new(taps::note_buffer()),
            created: Box::new(regia_render::create_buffer()),
            created_len: 0,
            last_create_seq: regia_render::create_seq(),
            evict_mark: (0.0, taps::counter(Counter::TextureEvictions)),
            wrong_platform_seen: 0,
            boot_marks_seen: 0,
            first_game_frame_at: None,
            header_done: false,
            hooks_installed: false,
            probe: probe::ProbeValues::new(),
            probe_started: false,
            probe_thread: None,
            requested: None,
            summary_seq: 0,
            last_summary: None,
            session: None,
            breakdown: breakdown::Breakdown::new(),
            live: LiveFrame::default(),
            live_window: live::LiveWindow::default(),
            agg: summary::SummaryAgg::new(),
            static_info: None,
            sink: None,
            host_section: None,
            last_now_s: 0.0,
            wall_s: 0.0,
            next_partial_s: f64::MAX,
            upscaler: None,
            backend: String::new(),
            boot_done_sent: false,
            shaders_ready_sent: false,
            warms_started_seen: 0,
            warm_report_seen: 0,
            scene_ready_seen: 0,
            span_layer_checked: false,
            opts,
        }
    }

    /// The options in effect.
    #[must_use]
    pub fn options(&self) -> &TelemetryOptions {
        &self.opts
    }

    /// Asks for a summary at the end of this frame (level end, match end) with `reason`.
    pub fn request_summary(&mut self, reason: &'static str) {
        self.requested = Some(reason);
    }

    /// The newest `regia::frame_summary` record (JSON), if one was emitted.
    #[must_use]
    pub fn last_summary_json(&self) -> Option<&str> {
        self.last_summary.as_deref()
    }

    /// The newest frame's render scale, GPU time, bound class and GPU waits (`telemetry.live`).
    /// All zero before the first frame.
    #[must_use]
    pub fn live(&self) -> LiveFrame {
        self.live
    }

    /// The `regia::session` record (JSON), once emitted.
    #[must_use]
    pub fn session_json(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// Counts since start.
    #[must_use]
    pub fn counts(&self) -> TelemetryCounts {
        let mut c = self.counts;
        c.suppressed = self.repeats.suppressed_total() + self.slow_repeats.suppressed_total();
        c
    }

    fn verbose(&self) -> bool {
        self.opts.level == TelemetryLevel::Verbose
    }

    fn ship(&self) -> bool {
        self.opts.level == TelemetryLevel::Ship
    }

    /// The pacing period the hitch floor follows, ms (0 when no frame cap or vsync paces the
    /// loop). [`telemetry_frame`] sets it every frame from `FramePacing` and the primary window's
    /// present mode and monitor refresh; a harness driving [`Self::observe_frame`] sets it here.
    #[must_use]
    pub fn pacing_period_ms(&self) -> f32 {
        self.pacing_period_ms
    }

    /// Sets the pacing period, ms ([`pacing_period_ms`] computes one); `None` or a non-positive
    /// value means unpaced.
    pub fn set_pacing_period_ms(&mut self, period_ms: Option<f32>) {
        self.pacing_period_ms = period_ms.filter(|p| p.is_finite() && *p > 0.0).unwrap_or(0.0);
    }

    /// The shortest frame that can be a hitch now, ms ([`hitch_floor_ms`]).
    #[must_use]
    pub fn hitch_floor_ms(&self) -> f32 {
        cause::hitch_floor_ms(&self.opts, self.pacing_period_ms)
    }

    fn warm(&self) -> bool {
        match self.first_game_frame_at {
            Some(at) => self.frame >= at + self.opts.warmup_frames,
            None => self.frame >= self.opts.warmup_fallback_frames,
        }
    }

    fn median_ms(&self) -> Option<f32> {
        if self.median_len < MEDIAN_MIN {
            return None;
        }
        let mut copy = self.median;
        let window = &mut copy[..self.median_len];
        let mid = window.len() / 2;
        let (_, m, _) = window.select_nth_unstable_by(mid, f32::total_cmp);
        Some(*m)
    }

    fn push_median(&mut self, ms: f32) {
        self.median[self.median_next] = ms;
        self.median_next = (self.median_next + 1) % MEDIAN_FRAMES;
        self.median_len = (self.median_len + 1).min(MEDIAN_FRAMES);
    }
}

/// Longest a dropping [`Telemetry`] waits for its probe thread (one sample in a stuck driver
/// call must not hang quit).
pub const PROBE_JOIN_WAIT: Duration = Duration::from_secs(1);

impl Telemetry {
    /// Stops the background probe and joins it (at most [`PROBE_JOIN_WAIT`]), so its
    /// `wgpu::Device` clone is dropped on the probe thread before this returns. Runs on drop;
    /// idempotent. True when no probe thread is left running.
    pub fn stop_probe(&mut self) -> bool {
        match self.probe_thread.take() {
            Some(handle) => probe::stop_and_join(&self.probe, handle, PROBE_JOIN_WAIT),
            None => {
                self.probe.stop();
                true
            }
        }
    }
}

impl Telemetry {
    /// Asks the probe thread to stop without waiting (ADR-287): the clean exit's first step, so
    /// no allocator report starts once the exit has begun. The join stays with
    /// [`Telemetry::stop_probe`] (on drop) and the end of the process's wait for GPU workers.
    pub fn signal_probe_stop(&self) {
        self.probe.stop();
        if let Some(handle) = &self.probe_thread {
            handle.thread().unpark();
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        // Joined, not just flagged: a probe that outlives the app drops the last device
        // reference during process exit, which segfaults in NVIDIA's driver (v0.1.0).
        self.stop_probe();
    }
}

/// Adds the telemetry systems. Part of `regia::DefaultPlugins`.
#[derive(Clone, Debug, Default)]
pub struct TelemetryPlugin {
    /// Settings (`REGIA_TELEMETRY` overrides them).
    pub options: TelemetryOptions,
}

impl Plugin for TelemetryPlugin {
    fn build(&self, app: &mut App) {
        let opts = self.options.clone().with_env();
        if !opts.enabled {
            return;
        }
        // Crash bundles (a lost device's included) carry the newest frame summary.
        regia_framework::feedback::set_summary_source(last_summary_json);
        // Before the other plugins' exit hooks (the render plugin's warm shutdown can wait for
        // a compile): the probe takes no new sample once the exit has begun (ADR-287).
        {
            let mut hooks = app
                .world_mut()
                .get_resource_or_init::<regia_platform::ExitHooks>();
            hooks.add(
                regia_platform::ExitStep::StopBackgroundWork,
                "telemetry probe",
                |world| {
                    if let Some(t) = world.get_resource::<Telemetry>() {
                        t.signal_probe_stop();
                    }
                },
            );
        }
        if opts.summary {
            engine_phases::register_exit(app);
            let _ = regia_core::log::set_panic_callback(engine_phases::write_panic_summary);
        }
        app.insert_resource(Telemetry::new(opts))
            .init_resource::<TelemetryHost>()
            .add_systems(
                Last,
                (telemetry_session.run_if(session_pending), telemetry_frame)
                    .chain()
                    .in_set(TelemetrySystems),
            );
    }
}

fn session_pending(t: Res<Telemetry>) -> bool {
    !t.header_done
}

fn counters_now() -> [u64; Counter::COUNT] {
    [
        Counter::SurfaceOutdated,
        Counter::SurfaceLost,
        Counter::SurfaceTimeout,
        Counter::SurfaceOther,
        Counter::TextureEvictions,
        Counter::AssetLoads,
        Counter::GpuErrors,
        Counter::Panics,
        Counter::DeviceLost,
        Counter::AudioSuspends,
        Counter::AudioThrottleSuspends,
    ]
    .map(taps::counter)
}

/// `Last` (until the header is out): installs the device hooks once the render device exists
/// and emits `regia::session` once the first frame rendered (or after 300 frames without a
/// device: headless).
pub fn telemetry_session(world: &mut World) {
    let device = world.get_resource::<regia_render::RenderDevice>().cloned();
    let rendered = world
        .get_resource::<RuntimeRenderer>()
        .is_some_and(|r| r.0.frame_index() > 0);
    let Some(mut t) = world.get_resource_mut::<Telemetry>() else {
        return;
    };
    if let Some(device) = device.as_ref()
        && !t.hooks_installed
    {
        gpu::install(&device.device, t.opts.gpu_error_panics);
        // Spec 54 § 4.2: the `.regiatrace` header names the adapter (it stayed empty before).
        trace::recorder().set_adapter(&trace::adapter_label(&device.adapter.get_info()));
        t.hooks_installed = true;
    }
    if !(device.is_some() && rendered) && t.frame < 300 {
        return;
    }
    if !t.probe_started {
        let vram = t.opts.vram_report.then(|| probe::VramWatch {
            adapter_mib: device
                .as_ref()
                .and_then(|d| regia_render::quality::AdapterProfile::from_device(d).vram_mb),
        });
        t.probe_thread = probe::spawn(
            t.probe.clone(),
            device.as_ref().map(|d| d.device.clone()),
            t.opts.probe_interval,
            vram,
        );
        t.probe_started = true;
    }
    t.header_done = true;
    let ship = t.ship();
    let full = session_record(world, device.as_ref());
    // The pacing period follows the monitor at every level; only what is written is filtered.
    let refresh_hz = current_refresh_hz(&full);
    let record = if ship { ship_header(world, &full) } else { full };
    let text: Arc<str> = Arc::from(record.to_string());
    let adapter = adapter_label(&record["adapter"]);
    let backend = record["adapter"]["backend"].as_str().unwrap_or("none").to_owned();
    tracing::info!(target: target::SESSION, adapter = %adapter, backend = %backend, data = %text, "regia session: {adapter} ({backend})");
    let marks = regia_render::boot::mark_count();
    let skeleton = summary_skeleton(world, &record, ship);
    let host = world
        .get_resource::<TelemetryHost>()
        .filter(|_| !ship)
        .map(TelemetryHost::summary_section);
    if let Some(mut t) = world.get_resource_mut::<Telemetry>() {
        t.session = Some(text);
        t.refresh_hz = refresh_hz;
        t.boot_marks_seen = marks;
        t.backend = backend;
        t.static_info = Some(skeleton);
        t.host_section = host;
        start_summary_sink(&mut t);
        engine_phases::from_marks(&mut t, &regia_render::boot::timeline());
    }
}

/// Starts the `summary.json` writer once the session header is out.
fn start_summary_sink(t: &mut Telemetry) {
    if !t.opts.summary || t.sink.is_some() {
        return;
    }
    let paths = match (&t.opts.summary_path, json::jsonl_path()) {
        (Some(path), _) => summary::SummaryPaths::from_final(path),
        (None, Some(log)) => summary::SummaryPaths::beside_log(&log),
        (None, None) => return,
    };
    t.sink = summary::SummarySink::start(paths);
    if t.sink.is_none() {
        tracing::warn!(target: target::TELEMETRY, "cannot start the summary writer thread: no partial or exit summaries");
    }
    // A panic before the first partial (five seconds in) still writes a `summary.json`.
    if let Some((paths, record)) = panic_seed(t) {
        engine_phases::remember_for_panic(&paths, record);
    }
    t.next_partial_s = t.last_now_s + summary::FIRST_PARTIAL_S;
}

/// What the panic hook can write before the first partial summary exists: the header-time record
/// and where its final goes.
fn panic_seed(t: &Telemetry) -> Option<(summary::SummaryPaths, SessionSummary)> {
    let sink = t.sink.as_ref()?;
    Some((sink.paths.clone(), t.build_summary(true)))
}

/// The summary sections that are known once at the session header: engine build, machine,
/// display and settings.
fn summary_skeleton(world: &World, record: &Value, ship: bool) -> SessionSummary {
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let adapter = &record["adapter"];
    let os_label = text(&record["os_version"]).or_else(|| {
        text(&record["os"]["family"]).map(|f| {
            format!("{f} {}", record["os"]["arch"].as_str().unwrap_or(""))
                .trim()
                .to_owned()
        })
    });
    let system = SystemInfo {
        os: os_label,
        cpu: text(&record["cpu"]),
        cpu_threads: record["cpu_threads"].as_u64().filter(|&n| n > 0),
        ram_gb: record["ram_gb"].as_f64(),
        gpu: text(&adapter["name"]),
        vendor: text(&adapter["vendor"]),
        driver: text(&adapter["driver_info"]).or_else(|| text(&adapter["driver"])),
        backend: text(&adapter["backend"]),
        vram_gb: record["vram_gb"].as_f64(),
    };
    let monitors: Vec<MonitorInfo> = record["monitors"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|m| MonitorInfo {
                    name: text(&m["name"]),
                    width: m["width"].as_u64().and_then(|w| u32::try_from(w).ok()),
                    height: m["height"].as_u64().and_then(|h| u32::try_from(h).ok()),
                    refresh_hz: m["refresh_hz"].as_f64(),
                    scale: m["scale"].as_f64(),
                    hdr: None,
                    current: m["current"].as_bool(),
                })
                .collect()
        })
        .unwrap_or_default();
    let hdr = world
        .get_resource::<regia_render::OutputSettings>()
        .map(|o| o.hdr != regia_render::HdrOutput::Sdr);
    let mut monitors = monitors;
    let current_refresh = monitors
        .iter()
        .find(|m| m.current == Some(true))
        .and_then(|m| m.refresh_hz);
    for m in monitors.iter_mut().filter(|m| m.current == Some(true)) {
        m.hdr = hdr;
    }
    let window = &record["window"];
    let size = window["physical_size"].as_array();
    let display = DisplayInfo {
        width: size
            .and_then(|s| s.first())
            .and_then(Value::as_u64)
            .and_then(|w| u32::try_from(w).ok()),
        height: size
            .and_then(|s| s.get(1))
            .and_then(Value::as_u64)
            .and_then(|h| u32::try_from(h).ok()),
        refresh_hz: current_refresh,
        mode: text(&window["mode"]),
        present: text(&window["present_mode"]),
        hdr,
        monitors: (!monitors.is_empty()).then_some(monitors),
    };
    let settings = world
        .get_resource::<regia_framework::settings::GameSettings>()
        .map(|g| SettingsInfo {
            preset: Some(format!("{:?}", g.graphics.preset)),
            render_scale: g.graphics.render_scale.map(|s| round2(f64::from(s))),
            aa: None,
            gi: g.graphics.lighting.map(|l| format!("{l:?}")),
            upscaler: None,
            // The detected plan names the adapter: not at Ship (ADR-8417).
            auto_plan: g
                .graphics
                .detected
                .as_ref()
                .filter(|_| !ship)
                .and_then(|d| serde_json::to_value(d).ok()),
        });
    SessionSummary {
        schema: SUMMARY_SCHEMA,
        regia: Some(RegiaInfo {
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            git: Some(option_env!("REGIA_GIT_HASH").unwrap_or("unknown").to_owned()),
            profile: Some(
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                }
                .to_owned(),
            ),
        }),
        system: Some(system),
        display: Some(display),
        settings,
        ..SessionSummary::default()
    }
}

impl Telemetry {
    /// The `summary.json` record as of now: the header's sections, the host's, and everything
    /// the session accumulated. `partial` marks a mid-session record.
    #[must_use]
    pub fn build_summary(&self, partial: bool) -> SessionSummary {
        let mut s = self.static_info.clone().unwrap_or_else(|| SessionSummary {
            schema: SUMMARY_SCHEMA,
            ..SessionSummary::default()
        });
        s.partial = partial.then_some(true);
        s.host.clone_from(&self.host_section);
        if let Some(settings) = s.settings.as_mut() {
            settings.upscaler = self.upscaler.map(str::to_owned);
        } else if let Some(u) = self.upscaler {
            s.settings = Some(SettingsInfo {
                upscaler: Some(u.to_owned()),
                ..SettingsInfo::default()
            });
        }
        s.session = Some(SessionInfo {
            seconds: Some(round2(self.last_now_s)),
            frames: Some(self.counts.frames),
            maps: Some(phase::now().maps),
        });
        self.agg.fill(&mut s, self.counts.hitches);
        let gb = |bytes: u64| round2(bytes as f64 / f64::from(1u32 << 30));
        let vram_peak = self
            .agg
            .vram_peak
            .max(probe::ProbeValues::get(&self.probe.vram_allocated).unwrap_or(0));
        s.memory = Some(MemoryInfo {
            vram_peak_gb: (vram_peak > 0).then(|| gb(vram_peak)),
            rss_peak_gb: probe::ProbeValues::get(&self.probe.rss_peak).map(gb),
            texture_budget_exceeded: Some(self.agg.budget_exceeded),
        });
        let mut shaders = self.agg.shaders.clone().unwrap_or_default();
        // Keep the warm-time `pipelines` next to `ms`/`ms_each` (it is the denominator): the
        // lifetime total is reported separately.
        shaders.pipelines_total = Some(regia_render::pipeline_cache::pipelines_created());
        if shaders.backend.is_none() && !self.backend.is_empty() {
            shaders.backend = Some(self.backend.clone());
        }
        shaders.driver_cache = Some(driver_cache_state().to_owned());
        s.shaders = Some(shaders);
        s.audio = Some(AudioInfo {
            suspends: Some(taps::counter(Counter::AudioSuspends)),
            throttle_suspends: Some(taps::counter(Counter::AudioThrottleSuspends)),
        });
        let surface = [
            Counter::SurfaceOutdated,
            Counter::SurfaceLost,
            Counter::SurfaceTimeout,
            Counter::SurfaceOther,
        ]
        .map(taps::counter)
        .iter()
        .sum();
        s.errors = Some(ErrorsInfo {
            gpu_errors: Some(taps::counter(Counter::GpuErrors)),
            device_lost: Some(taps::counter(Counter::DeviceLost)),
            surface_errors: Some(surface),
            panics: Some(taps::counter(Counter::Panics)),
        });
        s.previous_exit = self.sink.as_ref().and_then(summary::SummarySink::previous_exit);
        s
    }

    /// Partial summaries written to disk so far.
    #[must_use]
    pub fn partials_written(&self) -> u64 {
        self.sink
            .as_ref()
            .map_or(0, |s| s.written.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Where `summary.json` is written (`None` until the session header is out, or without a
    /// JSON log and without [`TelemetryOptions::summary_path`]).
    #[must_use]
    pub fn summary_paths(&self) -> Option<&SummaryPaths> {
        self.sink.as_ref().map(|s| &s.paths)
    }
}

/// The summary's `memory.categories`: bytes per [`regia_render::memory::SUMMARY_BUCKETS`] at
/// the last probe sample (`null` before one, or without an allocator report), plus
/// `pipeline_cache` (RAM).
fn memory_categories_json() -> Value {
    let Some(buckets) = regia_render::memory::last_summary_buckets() else {
        return Value::Null;
    };
    let mut map: serde_json::Map<String, Value> = buckets
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), json!(bytes)))
        .collect();
    map.insert(
        "pipeline_cache".to_owned(),
        json!(regia_render::memory::pipeline_cache_bytes()),
    );
    Value::Object(map)
}

/// The window record's `memory.host` (ADR-7891): the CPU mesh and texture copies every
/// `HostMeshStore` holds, what the host memory budget dropped and re-read, and its over-budget
/// frames (process totals).
fn memory_host_json() -> Value {
    let c = regia_render::host_memory::host_memory_counters();
    json!({
        "cpu_mesh_bytes": c.cpu_mesh_bytes,
        "cpu_texture_bytes": c.cpu_texture_bytes,
        "dropped_bytes": c.dropped_bytes,
        "reread_bytes": c.reread_bytes,
        "rereads": c.rereads,
        "over_budget_ticks": c.over_budget_ticks,
    })
}

/// The window record's `memory.subsystems` (ADR-539): per engine subsystem, live CPU heap bytes
/// (`regia_core::memory`; `null` unless the host installed its tagging allocator) and GPU bytes
/// at the last probe sample (`null` before one, or without an allocator report).
fn memory_subsystems_json() -> Value {
    let heap = regia_core::memory::snapshot();
    let gpu = regia_render::memory::last_subsystem_bytes();
    memory_subsystems_value(&heap, gpu.as_ref())
}

/// `memory_subsystems_json` from its inputs (the testable part).
fn memory_subsystems_value(
    heap: &regia_core::memory::MemorySnapshot,
    gpu: Option<&[(regia_core::memory::Subsystem, u64); regia_core::memory::Subsystem::COUNT]>,
) -> Value {
    let tracked = heap.tracking;
    let by_subsystem: serde_json::Map<String, Value> = heap
        .subsystems
        .iter()
        .enumerate()
        .map(|(i, (subsystem, m))| {
            (
                subsystem.name().to_owned(),
                json!({
                    "heap_bytes": tracked.then_some(m.live_bytes),
                    "gpu_bytes": gpu.map(|g| g[i].1),
                }),
            )
        })
        .collect();
    json!({
        "heap_tracked": tracked,
        "heap_live_bytes": tracked.then_some(heap.live_bytes),
        "heap_peak_bytes": tracked.then_some(heap.peak_bytes),
        "by_subsystem": by_subsystem,
    })
}

/// The summary's `emissive`: what the emissive fit cache did over the window (deltas of
/// [`FrameRenderer::emissive_fit_stats`] since `at_window`) and the largest frame-thread
/// collection (`ms_max`); `null` without a renderer.
fn emissive_json(
    renderer: Option<&FrameRenderer>,
    at_window: &regia_render::emissive_lights::EmissiveFitStats,
    ms_max: f32,
) -> Value {
    let Some(r) = renderer else {
        return Value::Null;
    };
    let w = r.emissive_fit_stats().since(at_window);
    json!({
        "fits_done": w.fits_done,
        "fits_deferred": w.fits_deferred,
        "cache_hits": w.cache_hits,
        "probe_hits": w.probe_hits,
        "probes_traced": w.probes_traced,
        "budget_holds": w.budget_holds,
        "stand_ins": w.stand_ins,
        "frame_ms": round2(w.frame_ms),
        "frame_ms_max": r2(ms_max),
        "worker_ms": round2(w.worker_ms),
        "pending": w.pending,
        "background": w.background,
    })
}

/// The summary's `memory.pressure`: the out-of-memory handling state ([`regia_render::gpu_health`])
/// and the renderer's streaming state.
fn memory_pressure_json(renderer: Option<&FrameRenderer>) -> Value {
    use regia_render::gpu_health as health;
    json!({
        "signals": health::memory_pressure_count(),
        "texture_budget_steps": health::texture_budget_steps(),
        "oom_uncaught": health::gpu_oom_count(),
        "oom_caught": health::captured_oom_count(),
        "mips_held": health::texture_mips_held(),
        "mip_drops": health::texture_mip_drops(),
        "stream_backing_off": renderer.map(FrameRenderer::texture_streaming_backing_off),
        "integrated_limits": renderer.map(FrameRenderer::integrated_gpu_limits),
    })
}

/// Whether the GPU driver's own shader cache is on: `off` when an environment switch turns it
/// off, `on` when one turns it on, else `unknown` (the driver does not say).
fn driver_cache_state() -> &'static str {
    let var = |name: &str| regia_core::brand::env_var(name).ok();
    if var("__GL_SHADER_DISK_CACHE").is_some_and(|v| v == "0")
        || var("MESA_SHADER_CACHE_DISABLE").is_some_and(|v| matches!(v.as_str(), "1" | "true"))
        || var("RADV_PERFTEST").is_some_and(|v| v.contains("nocache"))
    {
        "off"
    } else if var("__GL_SHADER_DISK_CACHE").is_some_and(|v| v == "1")
        || var("MESA_SHADER_CACHE_DIR").is_some()
    {
        "on"
    } else {
        "unknown"
    }
}

fn session_record(world: &mut World, device: Option<&regia_render::RenderDevice>) -> Value {
    let adapter = device.map_or(Value::Null, |d| {
        let info = d.adapter.get_info();
        json!({
            "name": info.name,
            "vendor": format!("0x{:04x}", info.vendor),
            "device": format!("0x{:04x}", info.device),
            "device_type": format!("{:?}", info.device_type),
            "driver": info.driver,
            "driver_info": info.driver_info,
            "backend": format!("{:?}", info.backend),
        })
    });
    let (features, limits, fallbacks) = device.map_or((Value::Null, Value::Null, Vec::new()), |d| {
        let features = d.device.features();
        let limits = d.device.limits();
        let info = d.adapter.get_info();
        let caps = &d.capabilities;
        let mut fallbacks: Vec<String> = Vec::new();
        if info.device_type == wgpu::DeviceType::Cpu {
            fallbacks.push("software_adapter".into());
        }
        match d.adapter_selection() {
            regia_render::AdapterSelection::Fallback(why) => {
                fallbacks.push(format!("adapter_selection: {why}"))
            }
            regia_render::AdapterSelection::Refused(why) => fallbacks.push(format!("adapter_refused: {why}")),
            _ => {}
        }
        if !caps.timestamp_query {
            fallbacks.push("no_timestamp_query (GPU pass timings are 0)".into());
        }
        if !caps.multi_draw_indirect_count {
            fallbacks.push("no_multi_draw_indirect_count (CPU draw counts)".into());
        }
        if !caps.texture_compression_bc && !cfg!(target_os = "android") {
            fallbacks.push("no_bc_compression (BCn decoded on the CPU)".into());
        }
        if !features.contains(wgpu::Features::PIPELINE_CACHE) {
            fallbacks.push("no_pipeline_cache".into());
        }
        let limits = json!({
            "max_texture_dimension_2d": limits.max_texture_dimension_2d,
            "max_texture_array_layers": limits.max_texture_array_layers,
            "max_bind_groups": limits.max_bind_groups,
            "max_storage_buffer_binding_size": limits.max_storage_buffer_binding_size,
            "max_buffer_size": limits.max_buffer_size,
            "max_storage_buffers_per_shader_stage": limits.max_storage_buffers_per_shader_stage,
            "max_sampled_textures_per_shader_stage": limits.max_sampled_textures_per_shader_stage,
            "max_compute_workgroup_storage_size": limits.max_compute_workgroup_storage_size,
            "max_compute_invocations_per_workgroup": limits.max_compute_invocations_per_workgroup,
            "max_color_attachments": limits.max_color_attachments,
        });
        (Value::String(format!("{features:?}")), limits, fallbacks)
    });
    let monitors = monitors(world);
    let window = window_info(world);
    let settings = world
        .get_resource::<regia_framework::settings::GameSettings>()
        .and_then(|s| s.to_ron().ok());
    let host = world
        .get_resource::<TelemetryHost>()
        .map(|h| json!(h.fields()))
        .unwrap_or(Value::Null);
    let problems: Vec<Value> = world
        .get_resource::<RuntimeRenderer>()
        .map_or_else(Vec::new, |r| {
            r.0.frame_problems()
                .iter()
                .map(|p| json!({ "code": p.code, "message": p.message }))
                .collect()
        });
    let cache = regia_render::pipeline_cache::status();
    let cache_load = match &cache.load {
        None => Value::Null,
        Some(regia_render::pipeline_cache::CacheLoad::Missing) => json!("miss"),
        Some(regia_render::pipeline_cache::CacheLoad::Loaded(bytes)) => {
            json!(format!("hit ({bytes} bytes)"))
        }
        Some(regia_render::pipeline_cache::CacheLoad::Discarded(why)) => {
            json!(format!("discarded ({why:?})"))
        }
        Some(regia_render::pipeline_cache::CacheLoad::Seeded(bytes)) => {
            json!(format!("seeded from shipped ({bytes} bytes)"))
        }
    };
    let boot: Vec<Value> = regia_render::boot::timeline()
        .into_iter()
        .map(|(name, ms)| json!({ "mark": name, "ms": round2(ms) }))
        .collect();
    let level = world
        .get_resource::<Telemetry>()
        .map_or("verbose", |t| t.opts.level.name());
    let os_info = world.get_resource::<regia_platform::OsInfo>();
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    let os_version = os_info.and_then(|o| non_empty(&o.version));
    let cpu = os_info.and_then(|o| non_empty(&o.cpu_model));
    let ram_gb = os_info
        .filter(|o| o.total_ram_bytes > 0)
        .map(|o| round2(o.total_ram_bytes as f64 / f64::from(1u32 << 30)));
    let vram_gb = device
        .and_then(|d| regia_render::quality::AdapterProfile::from_device(d).vram_mb)
        .map(|mb| round2(mb as f64 / 1024.0));
    json!({
        "regia": {
            "version": env!("CARGO_PKG_VERSION"),
            "git": option_env!("REGIA_GIT_HASH").unwrap_or("unknown"),
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        },
        "os": { "family": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "os_version": os_version,
        "cpu": cpu,
        "ram_gb": ram_gb,
        "vram_gb": vram_gb,
        "cpu_threads": std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        "adapter": adapter,
        "features": features,
        "limits": limits,
        "fallbacks": fallbacks,
        "monitors": monitors,
        "window": window,
        "settings_ron": settings,
        "host": host,
        "boot_marks": boot,
        "pipeline_cache": {
            "enabled": cache.enabled,
            "load": cache_load,
            "load_ms": round2(f64::from(cache.load_ms)),
            "pipelines_created": regia_render::pipeline_cache::pipelines_created(),
        },
        "problems": problems,
        "telemetry_level": level,
    })
}

/// The Ship session header (ADR-8417): [`SHIP_HEADER_KEYS`] projected from the full header, with
/// the quality preset and render scale from `GameSettings`.
fn ship_header(world: &World, full: &Value) -> Value {
    let graphics = world
        .get_resource::<regia_framework::settings::GameSettings>()
        .map(|g| &g.graphics);
    let preset = graphics.map(|g| format!("{:?}", g.preset));
    let render_scale = graphics
        .and_then(|g| g.render_scale)
        .map(|s| round2(f64::from(s)));
    ship_record(full, preset.as_deref(), render_scale)
}

/// [`SHIP_HEADER_KEYS`] projected from a full session header. Only keys in the list are written;
/// each has its own projection, so nothing of the full header passes through unfiltered.
fn ship_record(full: &Value, preset: Option<&str>, render_scale: Option<f64>) -> Value {
    let tier = |key: &str, tiers: &[u32]| json!(full[key].as_f64().and_then(|gb| memory_tier_gb(gb, tiers)));
    let mut out = serde_json::Map::new();
    for key in SHIP_HEADER_KEYS {
        let value = match key {
            "regia" => json!({ "version": full["regia"]["version"], "profile": full["regia"]["profile"] }),
            "os" => json!({ "family": full["os"]["family"], "arch": full["os"]["arch"] }),
            "ram_gb" => tier(key, &SHIP_RAM_TIERS_GB),
            "vram_gb" => tier(key, &SHIP_VRAM_TIERS_GB),
            "adapter" => ship_adapter(&full["adapter"]),
            "preset" => json!(preset),
            "render_scale" => json!(render_scale),
            "telemetry_level" => json!(TelemetryLevel::Ship.name()),
            // A key without a projection is written as unknown, never copied.
            _ => Value::Null,
        };
        out.insert(key.to_owned(), value);
    }
    Value::Object(out)
}

/// The Ship header's adapter: class, vendor family and backend; no name, ids or driver.
fn ship_adapter(adapter: &Value) -> Value {
    if adapter.is_null() {
        return Value::Null;
    }
    let class = match adapter["device_type"].as_str() {
        Some("DiscreteGpu") => "discrete",
        Some("IntegratedGpu") => "integrated",
        Some("VirtualGpu") => "virtual",
        Some("Cpu") => "cpu",
        _ => "other",
    };
    let vendor = adapter["vendor"]
        .as_str()
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .map_or("unknown", vendor_family);
    json!({ "class": class, "vendor": vendor, "backend": adapter["backend"] })
}

/// A PCI (or Khronos) vendor id's family name.
fn vendor_family(id: u32) -> &'static str {
    match id {
        0x10de => "nvidia",
        0x1002 | 0x1022 => "amd",
        0x8086 => "intel",
        0x13b5 => "arm",
        0x5143 => "qualcomm",
        0x106b => "apple",
        0x1010 => "imagination",
        0x14e4 => "broadcom",
        0x10005 => "mesa",
        0 => "unknown",
        _ => "other",
    }
}

/// The session event's adapter label: the adapter name, or at Ship its vendor family and class.
fn adapter_label(adapter: &Value) -> String {
    if let Some(name) = adapter["name"].as_str() {
        return name.to_owned();
    }
    match (adapter["vendor"].as_str(), adapter["class"].as_str()) {
        (Some(vendor), Some(class)) => format!("{vendor} {class}"),
        _ => "none".to_owned(),
    }
}

/// The refresh rate of the monitor the session header marked current, Hz.
fn current_refresh_hz(record: &Value) -> Option<f32> {
    record["monitors"]
        .as_array()?
        .iter()
        .find(|m| m["current"].as_bool() == Some(true))
        .and_then(|m| m["refresh_hz"].as_f64())
        .map(|hz| hz as f32)
        .filter(|hz| hz.is_finite() && *hz > 0.0)
}

fn monitors(world: &World) -> Value {
    let Some(handles) = world.get_resource::<regia_platform::WindowHandles>() else {
        return Value::Null;
    };
    let Some((_, window)) = handles.iter().next() else {
        return Value::Null;
    };
    let current = window.current_monitor().and_then(|m| m.name());
    let list: Vec<Value> = window
        .available_monitors()
        .map(|m| {
            let size = m.size();
            json!({
                "name": m.name(),
                "width": size.width,
                "height": size.height,
                "refresh_hz": m.refresh_rate_millihertz().map(|mhz| f64::from(mhz) / 1000.0),
                "scale": m.scale_factor(),
                "current": m.name().is_some() && m.name() == current,
            })
        })
        .collect();
    Value::Array(list)
}

fn window_info(world: &mut World) -> Value {
    let mut q = world.query_filtered::<&regia_platform::Window, With<regia_platform::PrimaryWindow>>();
    q.iter(world).next().map_or(Value::Null, |w| {
        json!({
            "physical_size": [w.physical_size.0, w.physical_size.1],
            "scale_factor": w.scale_factor,
            "mode": format!("{:?}", w.mode),
            "present_mode": format!("{:?}", w.present_mode),
        })
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn r2(v: f32) -> f64 {
    round2(f64::from(v))
}

/// Message cursors of [`telemetry_frame`]'s window watch (v0.5.1).
#[derive(Default)]
pub struct WindowWatch {
    resized: MessageCursor<regia_platform::WindowResized>,
    focused: MessageCursor<regia_platform::WindowFocused>,
    scale: MessageCursor<regia_platform::WindowScaleFactorChanged>,
}

/// The window messages [`telemetry_frame`] reads (each optional: a headless app has none).
type WindowMessages<'w> = (
    Option<Res<'w, Messages<regia_platform::WindowResized>>>,
    Option<Res<'w, Messages<regia_platform::WindowFocused>>>,
    Option<Res<'w, Messages<regia_platform::WindowScaleFactorChanged>>>,
);

/// [`telemetry_frame`]'s pacing and window inputs: the frame cap, the primary window, its
/// messages and the message cursors.
type FrameWindowParams<'w, 's> = (
    Option<Res<'w, regia_app::FramePacing>>,
    Query<'w, 's, &'static regia_platform::Window, With<regia_platform::PrimaryWindow>>,
    WindowMessages<'w>,
    Local<'s, WindowWatch>,
);

/// This frame's window events and the primary window's mode and size (a mode or size that
/// differs from the frame before counts as a change even without a message).
fn watch_windows(
    t: &mut Telemetry,
    watch: &mut WindowWatch,
    messages: &WindowMessages<'_>,
    window: Option<&regia_platform::Window>,
) -> cause::WindowEvents {
    let mut events = cause::WindowEvents::default();
    if let Some(m) = messages.0.as_deref() {
        events.resized = watch.resized.read(m).count() as u32;
    }
    if let Some(m) = messages.1.as_deref() {
        for e in watch.focused.read(m) {
            if e.focused {
                events.focus_gained += 1;
            } else {
                events.focus_lost += 1;
            }
            t.window_focused = Some(e.focused);
        }
    }
    if let Some(m) = messages.2.as_deref() {
        events.scale_changes = watch.scale.read(m).count() as u32;
    }
    if let Some(w) = window {
        let mode = match w.mode {
            regia_platform::WindowMode::Windowed => "windowed",
            regia_platform::WindowMode::BorderlessFullscreen => "borderless_fullscreen",
        };
        if t.window_mode.is_some_and(|m| m != mode) {
            events.mode_changes += 1;
        }
        t.window_mode = Some(mode);
        t.window_size = Some(w.physical_size);
    }
    events
}

/// `Last`: the per-frame system. Aggregates the frame into the window, detects hitches and
/// changes, and emits events. Allocates only when it emits one.
#[allow(clippy::too_many_arguments)]
pub fn telemetry_frame(
    real: Option<Res<RealTime>>,
    renderer: Option<Res<RuntimeRenderer>>,
    spans: Option<Res<SpanProfiler>>,
    phases: Option<Res<regia_app::FramePhases>>,
    runner: Option<Res<regia_app::RunnerState>>,
    extracted: Option<Res<regia_render::ExtractedFrame>>,
    warm: Option<Res<GameFrameWarm>>,
    host: Option<Res<TelemetryHost>>,
    entities: &regia_ecs::ecs::entity::Entities,
    (pacing, primary, messages, mut watch): FrameWindowParams<'_, '_>,
    mut t: ResMut<Telemetry>,
) {
    let renderer = renderer.as_deref();
    let t = &mut *t;
    let window = primary.iter().next();
    let vsync = window.is_some_and(|w| {
        matches!(
            w.present_mode,
            regia_platform::PresentMode::Vsync | regia_platform::PresentMode::VsyncRelaxed
        )
    });
    t.pacing_period_ms =
        cause::pacing_period_ms(pacing.as_deref().and_then(|p| p.target_fps), vsync, t.refresh_hz);
    let window_events = watch_windows(t, &mut watch, &messages, window);
    let now_s = advance_wall(&mut t.wall_s, real.as_deref());
    // Ship writes no host section (ADR-8417): the game adds its own fields to its upload.
    if let Some(host) = host.as_ref().filter(|h| h.is_changed() && !t.ship()) {
        t.host_section = Some(host.summary_section());
    }
    if let Some(warm) = warm.as_deref() {
        engine_phases::scene_warm(t, warm);
    }
    let sample = t.sampler.sample(real.as_deref(), renderer);
    // The window path's newest frame report: a held frame rendered no scene (ADR-7890).
    let (hold_seq, hold) = regia_render::scene_hold::last_window_frame();
    let held = (hold_seq != t.hold_seq_seen && hold.held).then_some(hold);
    t.hold_seq_seen = hold_seq;
    let inputs = FrameInputs {
        held,
        phases: phases.as_deref(),
        runner: runner.as_deref(),
        counts: extracted
            .as_deref()
            .map_or_else(Default::default, |x| breakdown::FrameCounts {
                mesh_instances: x.meshes.len() as u64,
                local_lights: x.local_lights.len() as u64,
            }),
        entities: Some(entities),
        waits: None,
        window: window_events,
        warming: warm.as_deref().is_some_and(GameFrameWarm::is_warming),
        no_scene: extracted.as_deref().is_some_and(|x| x.views.is_empty()),
    };
    frame_step(
        t,
        sample,
        renderer.map(|r| &r.0),
        spans.as_deref().map(|p| &p.0),
        now_s,
        inputs,
    );
}

/// Advances the session clock by the real frame interval and returns it. `RealTime::elapsed`
/// adds only the clamped gameplay delta, so a multi-second stall would advance it by 250 ms and
/// skew `t_s`, `window_s` and the summary period; the unclamped `raw_delta` keeps them true.
fn advance_wall(wall_s: &mut f64, real: Option<&RealTime>) -> f64 {
    if let Some(r) = real {
        *wall_s += r.raw_delta().as_secs_f64();
    }
    *wall_s
}

impl Telemetry {
    /// Drives one frame by hand, for harnesses that render without the ECS app (benches, tools):
    /// `frame_ms` is the frame interval, `now_s` the time since start; `spans` is the frame
    /// profiler whose newest closed frame is this one. Same work as [`telemetry_frame`].
    pub fn observe_frame(
        &mut self,
        frame_ms: f32,
        now_s: f64,
        renderer: Option<&FrameRenderer>,
        spans: Option<&FrameProfiler>,
    ) {
        let sample = self.sampler.sample_at(now_s, frame_ms, renderer);
        frame_step(self, Some(sample), renderer, spans, now_s, FrameInputs::default());
    }

    /// [`Self::observe_frame`] for a frame that showed the scene hold's kept image (ADR-7890):
    /// counted as held, kept out of the frame-time histograms, the hitch median and the live
    /// bound classification; its GPU time is `held.gpu_ms`.
    pub fn observe_held_frame(
        &mut self,
        frame_ms: f32,
        now_s: f64,
        held: regia_render::SceneHoldFrame,
        renderer: Option<&FrameRenderer>,
        spans: Option<&FrameProfiler>,
    ) {
        let sample = self.sampler.sample_at(now_s, frame_ms, renderer);
        let inputs = FrameInputs {
            held: held.held.then_some(held),
            ..FrameInputs::default()
        };
        frame_step(self, Some(sample), renderer, spans, now_s, inputs);
    }
}

/// One frame of [`telemetry_frame`] with the sample already taken (tests inject samples).
fn frame_step(
    t: &mut Telemetry,
    sample: Option<FrameSample>,
    renderer: Option<&FrameRenderer>,
    spans: Option<&FrameProfiler>,
    now_s: f64,
    inputs: FrameInputs<'_>,
) {
    t.frame += 1;
    t.counts.frames += 1;
    t.last_now_s = now_s;
    if t.frame == 1 {
        // A harness that starts at t > 0 must not report that offset as the first window.
        let first_ms = sample.as_ref().map_or(0.0, |s| f64::from(s.frame_ms.max(0.0)));
        t.window_start_s = (now_s - first_ms / 1000.0).max(0.0);
    }
    gpu::FRAME.store(t.frame, Ordering::Relaxed);
    if let Some(s) = sample.as_ref()
        && s.frame_ms >= summary::STALL_MS
    {
        t.agg.stall(now_s, s.frame_ms);
    }
    if t.frame >= SPAN_LAYER_CHECK_FRAME && !t.span_layer_checked && t.frame.is_multiple_of(30) {
        check_span_layer(t, spans);
    }

    // Name what this frame created (only when it created something).
    t.created_len = 0;
    if let Some(s) = sample.as_ref()
        && s.pipelines + s.bind_groups + s.buffers + s.textures > 0
    {
        t.created_len = regia_render::recent_creates(t.last_create_seq, &mut t.created);
        if t.created_len > 0 {
            t.last_create_seq = t.created[t.created_len - 1].seq;
        }
    }

    // A hitch from an earlier frame goes out once its GPU timings are published.
    if let Some(p) = t.pending_hitch.as_mut() {
        p.frames_left = p.frames_left.saturating_sub(1);
        if p.frames_left == 0 {
            emit_pending_hitch(t, renderer);
        }
    }
    if let Some(s) = sample.as_ref() {
        t.breakdown.frame(
            t.frame,
            s.frame_ms > GAP_MS,
            inputs.phases,
            renderer,
            inputs.counts,
            &t.created[..t.created_len],
        );
    }
    let mut sample = sample;
    if let (Some(h), Some(s)) = (inputs.held.as_ref(), sample.as_mut()) {
        // A held frame's GPU work is the composite and the UI; the renderer's timings are stale.
        s.gpu_ms = h.gpu_ms;
    }
    // The previous frame's hitches have their update's phases now.
    resolve_causes(t);
    t.window_events_prev = t.window_events;
    t.window_events = inputs.window;
    if let Some(s) = sample {
        let waits = inputs.waits.unwrap_or_else(regia_render::frame_waits);
        live_frame(t, &s, renderer, &waits, inputs);
        match inputs.held.as_ref() {
            Some(h) => held_frame(t, &s, h),
            None => aggregate(t, &s, renderer),
        }
        detect_hitch(t, &s, renderer, spans, inputs.held.as_ref());
        if s.pipelines > 0 && t.warm() {
            pipelines_after_warmup(t, &s);
        }
    }
    if let Some(r) = renderer {
        problems_changed(t, r.frame_problems());
    }
    if taps::notes_pending() {
        drain_notes(t);
    }
    eviction_storm(t, now_s);
    let marks = regia_render::boot::mark_count();
    if marks != t.boot_marks_seen {
        boot_marks(t, marks);
    }
    let due = now_s - t.window_start_s >= t.opts.summary_interval.as_secs_f64();
    if (due && t.window_frames > 0) || t.requested.is_some() {
        let reason = t.requested.take().unwrap_or("periodic");
        let entities = inputs.entities.map(|e| (e.count_spawned(), e.len()));
        summary(t, renderer, now_s, reason, entities);
    }
    if now_s >= t.next_partial_s {
        send_partial_summary(t, now_s);
    }
}

/// The frame at which the frame profiler is checked for data.
const SPAN_LAYER_CHECK_FRAME: u64 = 120;
/// The frame after which a profiler that has still seen no span gets its warning.
const SPAN_LAYER_GIVE_UP_FRAME: u64 = 600;

/// A host that installs its own `tracing` subscriber must add the frame profiler as a layer
/// (`regia::frame_profiler()`); without it hitch events name no spans and the frame breakdown
/// has no host sections. Warns once, after the profiler has had frames to see.
fn check_span_layer(t: &mut Telemetry, spans: Option<&FrameProfiler>) {
    let Some(profiler) = spans else { return };
    if t.span_layer_checked || !profiler.is_enabled() || profiler.frame_count() < 60 {
        return;
    }
    // Spans seen in any retained frame count: a single idle frame (menu, headless) has none.
    if profiler.history_span_count() > 0 {
        t.span_layer_checked = true;
        return;
    }
    if t.frame < SPAN_LAYER_GIVE_UP_FRAME {
        return;
    }
    t.span_layer_checked = true;
    tracing::warn!(
        target: target::TELEMETRY,
        "the frame profiler has recorded no spans: this process's tracing subscriber lacks its layer, so hitch \
         events carry no top spans and the frame breakdown no host sections; add \
         `registry().with(regia::frame_profiler())` to the subscriber"
    );
}

/// Queues the partial summary (the file is written off the frame thread) and schedules the next.
fn send_partial_summary(t: &mut Telemetry, now_s: f64) {
    t.next_partial_s = now_s + summary::PARTIAL_INTERVAL_S;
    let record = t.build_summary(true);
    if let Some(sink) = t.sink.as_ref() {
        sink.send_partial(record.clone());
    }
    if let Some(sink) = t.sink.as_ref() {
        engine_phases::remember_for_panic(&sink.paths, record);
    }
}

/// Updates [`Telemetry::live`] and its window aggregates with the frame that just ended and the
/// last window frame's GPU waits.
fn live_frame(
    t: &mut Telemetry,
    s: &FrameSample,
    renderer: Option<&FrameRenderer>,
    waits: &regia_render::FrameWaits,
    inputs: FrameInputs<'_>,
) {
    let held = inputs.held.is_some();
    let gap = s.frame_ms > GAP_MS;
    // A wait counts for this frame only when a window frame was presented since the last one.
    let fresh_waits = waits.seq != 0 && waits.seq != t.frame_wait_seq;
    t.frame_wait_ms = if fresh_waits {
        waits.acquire_ms + waits.present_ms
    } else {
        0.0
    };
    t.frame_wait_seq = waits.seq;
    let scale = renderer
        .and_then(FrameRenderer::last_extent)
        .map(|e| render_scale_permille(&e));
    let hidden = inputs
        .runner
        .is_some_and(|r| r.occluded || r.stalled || r.surface_refused);
    let event_wait_ms = inputs
        .phases
        .filter(|p| p.frame > 0)
        .map(|p| p.get(regia_app::Phase::EventWait));
    let mut timing = s.timing;
    if let Some(timing) = timing.as_mut() {
        timing.inactive(hidden, inputs.warming || inputs.no_scene);
    }
    let current = timing.is_none_or(|t| t.cpu_fresh && t.gpu_fresh);
    let waited = event_wait_ms.is_some_and(|ms| ms > 0.0 && ms >= EVENT_WAIT_SHARE * s.frame_ms);
    let bound = if current && !waited && !hidden {
        FrameBound::classify(s.frame_ms, s.gpu_ms)
    } else {
        FrameBound::Unknown
    };
    let bound_reason = (bound == FrameBound::Unknown).then(|| {
        if hidden {
            return UnknownBound::Hidden;
        }
        if let Some(timing) = timing {
            if timing.cpu_absent == Some(regia_framework::feedback::RenderTimingAbsence::NoScene)
                || inputs.warming
            {
                return UnknownBound::NoScene;
            }
            if !timing.cpu_fresh && timing.cpu_frame_id.is_some() {
                return UnknownBound::RendererIdle;
            }
        }
        if waited {
            return UnknownBound::EventWait;
        }
        let epoch = phase::epoch();
        if epoch != t.phase_epoch_seen {
            t.phase_epoch_seen = epoch;
            t.loading = is_loading_phase(&phase::now().phase);
        }
        let drew = renderer.is_some_and(|r| {
            let d = r.draw_stats();
            d.drawn + d.instanced > 0
        });
        UnknownBound::classify(
            renderer.is_some_and(FrameRenderer::gpu_timing_enabled),
            t.loading,
            inputs.warming,
            drew,
        )
    });
    t.live = LiveFrame {
        timing,
        cpu_sample_ms: timing.filter(|t| t.cpu_fresh).map(|_| s.cpu_ms),
        gpu_sample_ms: timing.filter(|t| t.gpu_fresh).map(|_| s.gpu_ms),
        phases_frame: inputs.phases.filter(|p| p.frame > 0).map(|p| p.frame),
        event_wait_ms,
        runner: inputs.runner.copied().map(LiveRunnerState::from),
        frame: t.frame,
        frame_ms: s.frame_ms,
        cpu_ms: s.cpu_ms,
        gpu_ms: s.gpu_ms,
        bound,
        bound_reason,
        render_scale_permille: scale,
        wait_acquire_ms: waits.acquire_ms,
        wait_present_ms: waits.present_ms,
        wait_readback_ms: waits.readback_ms,
        held,
    };
    t.live_window.waits(waits, gap);
    // A held frame says nothing about the scene's cost: no bound or render-scale sample.
    if !gap && !held {
        t.live_window.frame(scale, bound, bound_reason);
        if bound != FrameBound::Unknown {
            t.live_window.pair(s.frame_ms, s.gpu_ms);
        }
    }
}

/// A frame that showed the scene hold's kept image (ADR-7890): counted apart. Its device-call
/// counters still count (a held frame that creates something is worth seeing); the frame-time
/// histograms, pass means and draw counts are left alone.
fn held_frame(t: &mut Telemetry, s: &FrameSample, held: &regia_render::SceneHoldFrame) {
    t.counts.held_frames += 1;
    t.held_window += 1;
    if held.timed {
        t.held_gpu_sum += f64::from(held.gpu_ms);
        t.held_gpu_n += 1;
    }
    t.creates.add(s);
    if t.warm() {
        t.creates_after_warm.add(s);
    }
}

/// Draws skipped since the last sample: the growth of the renderer's running total `total`
/// (`seen` is the total at the last sample). A renderer rebuilt with a fresh total counts from
/// zero.
fn skipped_since(total: u64, seen: &mut u64) -> u64 {
    let delta = if total >= *seen { total - *seen } else { total };
    *seen = total;
    delta
}

fn aggregate(t: &mut Telemetry, s: &FrameSample, renderer: Option<&FrameRenderer>) {
    if s.frame_ms > GAP_MS {
        t.gaps += 1;
        // The device-call counters are per-sample deltas that exist only on this sample: account
        // for them before the early return so a stall frame's creations are not discarded (they
        // would otherwise be in no window at all). The frame-time histograms, pass means and
        // draw counts stay skipped — a gap is not a frame.
        t.creates.add(s);
        if t.warm() {
            t.creates_after_warm.add(s);
            t.late_sites.add(&t.created[..t.created_len], s);
        }
        if let Some(r) = renderer {
            let integrity = r.draw_integrity();
            // Index 4 is the skipped-draw count in the draws_sum layout below.
            t.draws_sum[4] += skipped_since(integrity.skipped_draws_total, &mut t.skipped_total_seen);
            let scratch = skipped_since(r.frame_scratch_stats().growths, &mut t.scratch_growths_seen);
            t.scratch_growths_window += scratch;
            if t.warm() {
                t.scratch_growths_after_warm += scratch;
            }
        }
        return;
    }
    t.window_frames += 1;
    t.agg.add_frame(s.frame_ms, s.cpu_ms, s.gpu_ms);
    t.frame_hist.add(s.frame_ms);
    if s.cpu_ms > 0.0 {
        t.cpu_hist.add(s.cpu_ms);
    }
    if s.gpu_ms > 0.0 {
        t.gpu_hist.add(s.gpu_ms);
    }
    t.creates.add(s);
    if t.warm() {
        t.creates_after_warm.add(s);
        t.late_sites.add(&t.created[..t.created_len], s);
    }
    let Some(r) = renderer else {
        return;
    };
    let timings = r.gpu_timings();
    if s.gpu_ms > 0.0 {
        for (i, v) in pass_values(&timings).into_iter().enumerate() {
            t.pass_sum[i] += f64::from(v);
            t.pass_max[i] = t.pass_max[i].max(v);
        }
        t.pass_frames += 1;
    }
    if s.timing.is_some_and(|t| !t.cpu_fresh) {
        return;
    }
    let d = r.draw_stats();
    let integrity = r.draw_integrity();
    for (i, v) in [
        d.drawn as u64,
        d.instanced as u64,
        d.per_object as u64,
        d.culled as u64,
        skipped_since(integrity.skipped_draws_total, &mut t.skipped_total_seen),
        integrity.skinned_culled as u64,
    ]
    .into_iter()
    .enumerate()
    {
        t.draws_sum[i] += v;
    }
    t.draws_max = t.draws_max.max(d.drawn as u64);
    t.zero_key_moving_max = t.zero_key_moving_max.max(d.zero_key_moving_meshes as u64);
    t.draw_frames += 1;
    let scratch = skipped_since(r.frame_scratch_stats().growths, &mut t.scratch_growths_seen);
    t.scratch_growths_window += scratch;
    if t.warm() {
        t.scratch_growths_after_warm += scratch;
    }
    let shadow = r.shadow_cpu_timings();
    for (sum, v) in t.shadow_cpu_sum.iter_mut().zip(shadow_cpu_values(&shadow)) {
        *sum += f64::from(v);
    }
    t.shadow_cpu_max = t.shadow_cpu_max.max(shadow.total_ms());
    t.emissive_ms_max = t.emissive_ms_max.max(r.emissive_lights().cpu_ms);
}

/// Parts of the summary's `shadow_cpu_ms` ([`shadow_cpu_values`]).
const SHADOW_CPU_PARTS: usize = 12;

/// A frame's shadow CPU times in summary order: total, plan, terrain prepare, caster lists,
/// instance lists, visibility-buffer cull, cascade 0..3 and atlas encode, translucent maps.
fn shadow_cpu_values(s: &regia_render::ShadowCpuTimings) -> [f32; SHADOW_CPU_PARTS] {
    [
        s.total_ms(),
        s.plan_ms,
        s.shared_prepare_ms,
        s.caster_lists_ms,
        s.instance_lists_ms,
        s.vis_cull_ms,
        s.encode_ms[0],
        s.encode_ms[1],
        s.encode_ms[2],
        s.encode_ms[3],
        s.encode_ms[4],
        s.translucent_ms,
    ]
}

/// Whether a frame of `frame_ms` is a hitch against the rolling median `median_ms` (v0.1.14): at
/// least [`TelemetryOptions::hitch_min_ms`], and standing out from the median by
/// [`TelemetryOptions::hitch_ratio`] × or by more than [`TelemetryOptions::hitch_abs_ms`]. Both
/// tests are relative, so a steady slow frame rate is never a hitch (before v0.1.14 every frame
/// over 50 ms was one: a steady 66 ms game logged one every 2 s, the rate limit's pace).
#[cfg(test)]
fn is_hitch(frame_ms: f32, median_ms: f32, opts: &TelemetryOptions) -> bool {
    is_hitch_over(frame_ms, median_ms, opts.hitch_min_ms, opts)
}

fn detect_hitch(
    t: &mut Telemetry,
    s: &FrameSample,
    renderer: Option<&FrameRenderer>,
    spans: Option<&FrameProfiler>,
    held: Option<&regia_render::SceneHoldFrame>,
) {
    let median = t.median_ms();
    // Held frames (ADR-7890) never feed the median: a cheap paused frame would lower it and turn
    // the first played frame into a hitch.
    if held.is_none() && (0.0..=GAP_MS).contains(&s.frame_ms) {
        t.push_median(s.frame_ms);
    }
    let Some(median) = median else {
        return;
    };
    let gap = s.frame_ms > GAP_MS;
    let floor = cause::hitch_floor_ms(&t.opts, t.pacing_period_ms);
    if !(gap || is_hitch_over(s.frame_ms, median, floor, &t.opts)) {
        return;
    }
    if !gap && cause::is_present_wait(s.frame_ms, median, t.frame_wait_ms, &t.opts) {
        t.counts.present_waits += 1;
        t.present_waits_window += 1;
        return;
    }
    t.counts.hitches += 1;
    let (frame_spans, background_spans) =
        spans.map_or_else(Default::default, FrameProfiler::last_frame_split);
    if !gap {
        t.hitches_window += 1;
        // The cause waits one frame for this update's phases (`resolve_causes`).
        let pending = cause::PendingCause {
            frame: t.frame,
            sample: *s,
            median_ms: median,
            frame_top: cause::frame_top_span(&frame_spans),
        };
        if let Some(free) = t.pending_causes.iter().position(Option::is_none) {
            t.pending_causes[free] = Some(pending);
        } else {
            // Full (a burst): the oldest resolves now with what is known.
            resolve_cause_now(t, 0);
            t.pending_causes[0] = Some(pending);
        }
    }
    let severe = s.frame_ms >= 250.0;
    if t.ship() || !(t.verbose() || severe) {
        // Normal emits only a severe hitch, Ship none (its record names resources and host
        // spans, ADR-8417). A window hitch held back by the level still counts as suppressed
        // (book/user-guide/telemetry.md). A gap is not a window hitch.
        if !gap {
            t.hitches_suppressed_window += 1;
            t.agg.hitches_suppressed += 1;
        }
        return;
    }
    if !t.hitch_bucket.take(Instant::now()) {
        t.hitches_suppressed_window += 1;
        t.agg.hitches_suppressed += 1;
        return;
    }
    // A pending hitch is emitted now with the timings available, so the new one can wait.
    if t.pending_hitch.is_some() {
        emit_pending_hitch(t, renderer);
    }
    let span_json = |list: &[(&'static str, Duration)]| -> Vec<Value> {
        list.iter()
            .take(5)
            .map(|(name, d)| json!({ "span": name, "ms": round2(d.as_secs_f64() * 1000.0) }))
            .collect()
    };
    let top_spans = span_json(&frame_spans);
    let background = span_json(&background_spans);
    let created: Vec<Value> = t.created[..t.created_len]
        .iter()
        .rev()
        .take(12)
        .map(|c| json!({ "kind": format!("{:?}", c.kind), "label": c.label(), "bytes": c.bytes }))
        .collect();
    // What the frame load budget left for later frames (ADR-7884; zero without a budget).
    let budget = renderer
        .map(FrameRenderer::load_budget_report)
        .unwrap_or_default();
    let mut record = json!({
        "frame": t.frame,
        "renderer_frame": renderer.map(|r| r.frame_index()),
        "t_s": round2(s.t_s),
        "frame_ms": r2(s.frame_ms),
        "median_ms": r2(median),
        "ratio": r2(s.frame_ms / median.max(1e-3)),
        "gap": gap,
        "cpu_ms": r2(s.cpu_ms),
        "bound": t.live.bound.name(),
        "render_scale_permille": t.live.render_scale_permille,
        "wait_ms": {
            "acquire": r2(t.live.wait_acquire_ms),
            "present": r2(t.live.wait_present_ms),
            "readback": r2(t.live.wait_readback_ms),
        },
        "top_cpu_spans": top_spans,
        "top_systems": t.breakdown.frame_top_systems(5),
        "host_spans": t.breakdown.frame_host_spans(8),
        "created": {
            "pipelines": s.pipelines, "bind_groups": s.bind_groups, "buffers": s.buffers,
            "textures": s.textures, "blocking_polls": s.blocking_polls, "inline_bakes": s.inline_bakes,
            "labels": created,
            "deferred_meshes": budget.deferred_meshes, "deferred_bytes": budget.deferred_bytes,
            "deferred_draws": budget.deferred_draws, "async_pipelines_pending": budget.async_pipelines_pending,
        },
        "warm": t.warm(),
        "held": held.is_some(),
    });
    if let Some(h) = held {
        // A held frame ran no scene pass: its GPU time is the composite and the UI (ADR-7890).
        record["scene_passes"] = json!(h.scene_views);
        record["gpu_ms"] = json!(r2(h.gpu_ms));
        record["top_gpu_passes"] = Value::Array(
            h.gpu_passes()
                .into_iter()
                .map(|(pass, ms)| json!({ "pass": pass, "ms": r2(ms) }))
                .collect(),
        );
    }
    // v0.5.1 (ADR-7898), outside the `json!` above to keep it short.
    record["background_spans"] = Value::Array(background);
    record["cause"] = if gap { json!("gap") } else { Value::Null };
    record["hitch_floor_ms"] = json!(r2(floor));
    record["bound_reason"] = json!(t.live.bound_reason.map(UnknownBound::name));
    record["render_timing"] = json!(t.live.timing);
    record["window_events"] =
        t.window_events
            .merged(&t.window_events_prev)
            .json(t.window_mode, t.window_size, t.window_focused);
    t.pending_hitch = Some(PendingHitch {
        record,
        frames_left: GPU_LAG_FRAMES,
    });
}

/// [`is_hitch`] against an explicit floor (the pacing floor, [`hitch_floor_ms`]) instead of
/// [`TelemetryOptions::hitch_min_ms`].
fn is_hitch_over(frame_ms: f32, median_ms: f32, floor_ms: f32, opts: &TelemetryOptions) -> bool {
    frame_ms >= floor_ms
        && (frame_ms > median_ms * opts.hitch_ratio || frame_ms > median_ms + opts.hitch_abs_ms)
}

/// The interval of telemetry frame `frame` from the breakdown's phase records (the previous
/// update's and this one's), `None` until both arrived.
fn interval_of(t: &Telemetry, frame: u64, frame_ms: f32) -> Option<cause::Interval> {
    let previous = t.breakdown.phases_of(frame.checked_sub(1)?)?;
    let this = t.breakdown.phases_of(frame)?;
    Some(cause::Interval::of(frame_ms, &previous, &this))
}

/// Resolves the pending causes of frames before this one (their update's phases arrived with
/// this frame): `summary.json`'s cause count, and the pending record's `cause`.
fn resolve_causes(t: &mut Telemetry) {
    for i in 0..cause::PENDING_CAUSES {
        if t.pending_causes[i].is_some_and(|p| p.frame < t.frame) {
            resolve_cause_now(t, i);
        }
    }
}

fn resolve_cause_now(t: &mut Telemetry, slot: usize) {
    let Some(p) = t.pending_causes[slot].take() else {
        return;
    };
    let interval = interval_of(t, p.frame, p.sample.frame_ms);
    let cause = cause::hitch_cause(&p.sample, p.median_ms, p.frame_top, interval.as_ref());
    t.agg.hitch_cause(&cause);
    if let Some(pending) = t.pending_hitch.as_mut()
        && pending.record["frame"].as_u64() == Some(p.frame)
    {
        pending.record["cause"] = json!(cause);
    }
}

/// The five slowest passes of `values` (`PASS_NAMES` order), skipping the last two entries
/// (`scene`, `frame`), which are totals.
fn top_gpu_passes(values: &[f32; PASS_COUNT]) -> Vec<Value> {
    let mut order: [usize; PASS_COUNT - 2] = std::array::from_fn(|i| i);
    order.sort_unstable_by(|&a, &b| values[b].total_cmp(&values[a]));
    order
        .iter()
        .take(5)
        .filter(|&&i| values[i] > 0.0)
        .map(|&i| json!({ "pass": PASS_NAMES[i], "ms": r2(values[i]) }))
        .collect()
}

fn emit_pending_hitch(t: &mut Telemetry, renderer: Option<&FrameRenderer>) {
    let Some(mut p) = t.pending_hitch.take() else {
        return;
    };
    // Flushed early (a second hitch arrived before the GPU lag elapsed): the published timings
    // belong to an earlier frame, so the record carries none rather than wrong ones.
    let held = p.record["held"].as_bool().unwrap_or(false);
    if let Some(r) = renderer.filter(|_| p.frames_left == 0 && !held) {
        attach_hitch_gpu(&mut p.record, r.gpu_timings());
    }
    let frame_ms = p.record["frame_ms"].as_f64().unwrap_or(0.0);
    let frame = p.record["frame"].as_u64().unwrap_or(0);
    if let Some(split) = t.breakdown.hitch_json(frame) {
        p.record["frame_breakdown_ms"] = split;
    }
    if let Some((frame_wall, update_wall)) = t.breakdown.hitch_wall_ms(frame) {
        p.record["frame_ms_wall"] = json!(round2(frame_wall));
        p.record["update_ms_wall"] = json!(round2(update_wall));
    }
    // v0.5.1: the interval's measured phases and what ran outside all of them.
    let interval = interval_of(t, frame, frame_ms as f32);
    p.record["unaccounted_ms"] = interval.map_or(Value::Null, |iv| json!(r2(iv.unaccounted_ms)));
    p.record["event_loop_ms"] = interval.map_or(Value::Null, |iv| json!(r2(iv.event_loop_ms())));
    p.record["interval_ms"] = interval.map_or(Value::Null, |iv| iv.json());
    // A cause still pending (flushed early by a second hitch) resolves with what is known.
    if p.record["cause"].is_null()
        && let Some(slot) = t
            .pending_causes
            .iter()
            .position(|c| c.is_some_and(|c| c.frame == frame))
    {
        t.pending_hitch = Some(p);
        resolve_cause_now(t, slot);
        let Some(back) = t.pending_hitch.take() else {
            return;
        };
        p = back;
    }
    let text = p.record.to_string();
    t.counts.hitch_events += 1;
    if frame_ms >= 250.0 {
        tracing::warn!(target: target::HITCH, frame, frame_ms, data = %text, "hitch: frame {frame} took {frame_ms:.1} ms");
    } else {
        tracing::info!(target: target::HITCH, frame, frame_ms, data = %text, "hitch: frame {frame} took {frame_ms:.1} ms");
    }
}

/// Distinct after-warm-up creation sites a summary window keeps ([`LateSites`]); later ones
/// count as `unnamed`.
const LATE_SITES: usize = 48;

/// Pipelines and bind groups created after warm-up in the current summary window, per site
/// (kind, label, target format, sample count, thread; ADR-7885). Preallocated: a warm frame
/// that creates something scans at most [`LATE_SITES`] entries and never allocates.
struct LateSites {
    sites: Vec<(CreateRecord, u64)>,
    /// Creations the recent-creation ring had already overwritten, or past [`LATE_SITES`].
    unnamed: u64,
    /// Named creations on a thread that records frames (the frame path).
    frame_thread: u64,
    /// Named creations on a warm worker or background builder (off the frame path).
    warm_worker: u64,
}

impl LateSites {
    fn new() -> Self {
        Self {
            sites: Vec::with_capacity(LATE_SITES),
            unnamed: 0,
            frame_thread: 0,
            warm_worker: 0,
        }
    }

    fn same(a: &CreateRecord, b: &CreateRecord) -> bool {
        a.kind == b.kind
            && a.detail.format == b.detail.format
            && a.detail.samples == b.detail.samples
            && a.detail.warm_worker == b.detail.warm_worker
            && a.label() == b.label()
    }

    /// Counts the pipelines and bind groups in `created` (this frame's remembered creations) and
    /// the rest of `s`'s counts as unnamed.
    fn add(&mut self, created: &[CreateRecord], s: &FrameSample) {
        let mut named = 0u64;
        for record in created.iter().filter(|r| {
            matches!(
                r.kind,
                regia_render::FramePathKind::Pipeline | regia_render::FramePathKind::BindGroup
            )
        }) {
            named += 1;
            if record.detail.warm_worker {
                self.warm_worker += 1;
            } else {
                self.frame_thread += 1;
            }
            if let Some(entry) = self.sites.iter_mut().find(|(known, _)| Self::same(known, record)) {
                entry.1 += 1;
            } else if self.sites.len() < LATE_SITES {
                self.sites.push((*record, 1));
            } else {
                self.unnamed += 1;
            }
        }
        let total = u64::from(s.pipelines) + u64::from(s.bind_groups);
        self.unnamed += total.saturating_sub(named);
    }

    fn clear(&mut self) {
        self.sites.clear();
        self.unnamed = 0;
        self.frame_thread = 0;
        self.warm_worker = 0;
    }

    /// `counts` (the window's `created_after_warmup` totals) with `sites`, `unnamed`,
    /// `frame_thread` and `warm_worker` added.
    fn json(&self, mut counts: Value) -> Value {
        let sites: Vec<Value> = self
            .sites
            .iter()
            .map(|(record, count)| {
                let mut site = create_json(record);
                site["count"] = json!(count);
                site
            })
            .collect();
        counts["sites"] = Value::Array(sites);
        counts["unnamed"] = json!(self.unnamed);
        counts["frame_thread"] = json!(self.frame_thread);
        counts["warm_worker"] = json!(self.warm_worker);
        counts
    }
}

/// One remembered creation as JSON: kind, label and, for pipelines, the target format, depth
/// format, sample count, thread (`frame` or `warm_worker`), creation site and the forward-set key
/// axes the renderer had no set for (ADR-7885).
fn create_json(record: &CreateRecord) -> Value {
    let kind = match record.kind {
        regia_render::FramePathKind::Pipeline => "pipeline",
        regia_render::FramePathKind::BindGroup => "bind_group",
        regia_render::FramePathKind::Buffer => "buffer",
        regia_render::FramePathKind::Texture => "texture",
        regia_render::FramePathKind::BlockingPoll => "blocking_poll",
    };
    let detail = &record.detail;
    let mut out = json!({
        "kind": kind,
        "label": record.label(),
        "thread": if detail.warm_worker { "warm_worker" } else { "frame" },
    });
    if record.kind == regia_render::FramePathKind::Pipeline {
        out["format"] = detail.format.map_or(Value::Null, |f| json!(format!("{f:?}")));
        out["depth_format"] = detail
            .depth_format
            .map_or(Value::Null, |f| json!(format!("{f:?}")));
        out["samples"] = json!(detail.samples);
        out["site"] = detail.site.map_or(Value::Null, |site| {
            json!(format!("{}:{}", site.file(), site.line()))
        });
        out["new_axes"] = json!(detail.new_axes.names().collect::<Vec<_>>());
    }
    out
}

fn attach_hitch_gpu(record: &mut Value, timings: GpuTimings) {
    // The two-frame readback delay is measured in renders, not telemetry ticks. Update-only
    // ticks must not attach an old GPU sample to a new hitch, even after that delay elapsed.
    record["gpu_frame_id"] = json!(timings.frame_id);
    let source = record["render_timing"]["cpu_frame_id"].as_u64();
    if record["render_timing"]["cpu_fresh"].as_bool() != Some(true)
        || source.is_none()
        || source != timings.frame_id
        || timings.sample_id == 0
    {
        return;
    }
    record["gpu_ms"] = json!(r2(timings.frame_ms));
    record["top_gpu_passes"] = Value::Array(top_gpu_passes(&pass_values(&timings)));
}

fn pipelines_after_warmup(t: &mut Telemetry, s: &FrameSample) {
    let Some(repeated) = t
        .repeats
        .check(rate::key(&[b"pipeline_after_warmup"]), Instant::now())
    else {
        return;
    };
    let labels: Vec<&str> = t.created[..t.created_len]
        .iter()
        .filter(|c| c.kind == regia_render::FramePathKind::Pipeline)
        .map(CreateRecord::label)
        .collect();
    // What each pipeline drew into, on which thread and where (ADR-7885): the labels alone
    // could not tell a second renderer or target from a missing variant.
    let creates: Vec<Value> = t.created[..t.created_len]
        .iter()
        .filter(|c| c.kind == regia_render::FramePathKind::Pipeline)
        .map(create_json)
        .collect();
    let data = json!({
        "kind": "pipeline_after_warmup", "frame": t.frame, "pipelines": s.pipelines,
        "labels": labels, "creates": creates, "repeated": repeated,
    })
    .to_string();
    stream_event(
        t,
        "pipeline_after_warmup",
        &data,
        &format!("{} pipeline(s) created after warm-up", s.pipelines),
    );
}

fn stream_event(t: &mut Telemetry, kind: &str, data: &str, message: &str) {
    t.counts.stream_events += 1;
    tracing::warn!(target: target::STREAM, kind, data = %data, "{message}");
}

fn problems_changed(t: &mut Telemetry, problems: &[regia_render::host_checklist::FrameProblem]) {
    let key = (problems.len(), {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        for p in problems {
            p.code.hash(&mut hasher);
            p.message.hash(&mut hasher);
        }
        hasher.finish()
    });
    if t.problems_key == Some(key) {
        return;
    }
    let first = t.problems_key.is_none();
    t.problems_key = Some(key);
    if first && problems.is_empty() {
        return;
    }
    // Ship keeps the codes and drops the message text (ADR-8417).
    let ship = t.ship();
    let list: Vec<Value> = problems
        .iter()
        .map(|p| {
            if ship {
                json!({ "code": p.code })
            } else {
                json!({ "code": p.code, "message": p.message })
            }
        })
        .collect();
    let codes: Vec<&str> = problems.iter().map(|p| p.code).collect();
    let data = json!({ "frame": t.frame, "count": problems.len(), "problems": list }).to_string();
    let codes = codes.join(",");
    t.counts.problems_events += 1;
    if problems.is_empty() {
        tracing::info!(target: target::PROBLEMS, count = 0u64, data = %data, "render problems cleared");
    } else {
        tracing::warn!(target: target::PROBLEMS, count = problems.len() as u64, codes = %codes, data = %data, "render problems: {codes}");
    }
}

fn drain_notes(t: &mut Telemetry) {
    let n = taps::drain_notes(&mut t.notes[..]);
    for i in 0..n {
        let note = t.notes[i];
        let kind = note.kind.name();
        let Some(repeated) = t.repeats.check(
            rate::key(&[kind.as_bytes(), note.label().as_bytes()]),
            Instant::now(),
        ) else {
            continue;
        };
        let (value_name, message) = match note.kind {
            taps::NoteKind::SlowAssetLoad => (
                "ms",
                format!("slow asset load: {} took {:.0} ms", note.label(), note.value),
            ),
            taps::NoteKind::PakCorrupt => ("offset", format!("pak corruption: {}", note.label())),
        };
        let data = json!({
            "kind": kind, "frame": t.frame, "label": note.label(), (value_name): note.value, "repeated": repeated,
        })
        .to_string();
        stream_event(t, kind, &data, &message);
    }
}

fn eviction_storm(t: &mut Telemetry, now_s: f64) {
    let elapsed = now_s - t.evict_mark.0;
    if elapsed < 1.0 {
        return;
    }
    let total = taps::counter(Counter::TextureEvictions);
    let in_window = total.saturating_sub(t.evict_mark.1);
    t.evict_mark = (now_s, total);
    // Per-second rate over the real window: a slow frame or a long gap between samples stretches
    // it past 1 s and must not read as one second of evictions.
    if in_window as f64 >= t.opts.eviction_storm as f64 * elapsed
        && let Some(repeated) = t
            .slow_repeats
            .check(rate::key(&[b"eviction_storm"]), Instant::now())
    {
        let data = json!({ "kind": "eviction_storm", "frame": t.frame, "evictions_per_s": (in_window as f64 / elapsed).round() as u64, "repeated": repeated })
            .to_string();
        stream_event(
            t,
            "eviction_storm",
            &data,
            &format!("texture eviction storm: {in_window} evictions in {elapsed:.1} s"),
        );
    }
}

fn boot_marks(t: &mut Telemetry, marks: usize) {
    t.boot_marks_seen = marks;
    let timeline = regia_render::boot::timeline();
    if t.first_game_frame_at.is_none() && timeline.iter().any(|(n, _)| n == "first_game_frame") {
        t.first_game_frame_at = Some(t.frame);
    }
    engine_phases::from_marks(t, &timeline);
    if !t.header_done || !t.verbose() {
        return;
    }
    let list: Vec<Value> = timeline
        .iter()
        .map(|(name, ms)| json!({ "mark": name, "ms": round2(*ms) }))
        .collect();
    let newest = timeline.last().map_or("", |(n, _)| n.as_str());
    let data = json!({ "frame": t.frame, "boot_marks": list }).to_string();
    tracing::info!(target: target::BOOT, mark = %newest, data = %data, "boot mark: {newest}");
}

fn summary(
    t: &mut Telemetry,
    renderer: Option<&FrameRenderer>,
    now_s: f64,
    reason: &str,
    entities: Option<(u32, u32)>,
) {
    let counters = counters_now();
    let delta = |c: Counter| counters[c as usize] - t.counters_at_window[c as usize];
    let pass_frames = t.pass_frames.max(1) as f64;
    let mut passes = serde_json::Map::new();
    for (i, name) in PASS_NAMES.iter().enumerate() {
        if t.pass_frames > 0 {
            passes.insert(
                (*name).to_owned(),
                json!({ "mean": round2(t.pass_sum[i] / pass_frames), "max": r2(t.pass_max[i]) }),
            );
        }
    }
    let draw_frames = t.draw_frames.max(1) as f64;
    let mean = |i: usize| round2(t.draws_sum[i] as f64 / draw_frames);
    let mean_shadow = |i: usize| round2(t.shadow_cpu_sum[i] / draw_frames);
    let fps = t.frame_hist.fps_buckets();
    let mut fps_hist = serde_json::Map::new();
    for (name, n) in FPS_BUCKETS.iter().zip(fps) {
        fps_hist.insert((*name).to_owned(), json!(n));
    }
    let (textures, over_budget) = renderer.map_or((Value::Null, false), |r| {
        let stream = r.stream_report();
        let budget = r.texture_budget_bytes();
        let over = budget != u64::MAX && stream.resident_bytes > budget;
        (
            json!({
                "resident_bytes": stream.resident_bytes,
                "budget_bytes": (budget != u64::MAX).then_some(budget),
                "over_budget": over,
                "streamed_materials": stream.materials,
                "queue_depth": stream.pending,
                "fading": stream.fading,
                "levels_below_cap": stream.levels_below_cap,
                "cpu_bytes": stream.cpu_bytes,
            }),
            over,
        )
    });
    t.agg.budget_exceeded |= over_budget;
    if let Some(vram) = probe::ProbeValues::get(&t.probe.vram_allocated) {
        t.agg.vram_peak = t.agg.vram_peak.max(vram);
    }
    if let Some(extent) = renderer.and_then(FrameRenderer::last_extent)
        && t.upscaler != Some(extent.upscaler)
    {
        t.upscaler = Some(extent.upscaler);
    }
    let gpu_mean = t.gpu_hist.mean();
    let cpu_mean = t.cpu_hist.mean();
    let bound = if t.gpu_hist.count() == 0 {
        "unknown"
    } else if gpu_mean > cpu_mean {
        "gpu"
    } else {
        "cpu"
    };
    let sections = t.breakdown.summary(
        f64::from(t.frame_hist.mean()),
        t.window_frames,
        renderer,
        entities,
    );
    let probe_mem = t.probe.snapshot();
    t.summary_seq += 1;
    let mut record = json!({
        "seq": t.summary_seq,
        "reason": reason,
        "frame": t.frame,
        "t_s": round2(now_s),
        "window_s": round2(now_s - t.window_start_s),
        "frames": t.window_frames,
        "gaps": t.gaps,
        // Frames that showed the paused-scene hold's kept image (ADR-7890): not in `frames` or
        // any frame-time figure; `gpu_ms_mean` is the composite and the UI.
        "held": {
            "frames": t.held_window,
            "gpu_ms_mean": (t.held_gpu_n > 0).then(|| round2(t.held_gpu_sum / t.held_gpu_n as f64)),
        },
        "frame_ms": {
            "p50": r2(t.frame_hist.percentile(0.5)),
            "p95": r2(t.frame_hist.percentile(0.95)),
            "p99": r2(t.frame_hist.percentile(0.99)),
            "max": r2(t.frame_hist.max()),
            "mean": r2(t.frame_hist.mean()),
        },
        "fps_histogram": fps_hist,
        "cpu_ms": { "samples": t.cpu_hist.count(), "mean": r2(cpu_mean), "p95": r2(t.cpu_hist.percentile(0.95)), "max": r2(t.cpu_hist.max()) },
        "gpu_ms": { "samples": t.gpu_hist.count(), "mean": r2(gpu_mean), "p95": r2(t.gpu_hist.percentile(0.95)), "max": r2(t.gpu_hist.max()) },
        "bound": bound,
        "gpu_pass_ms": passes,
        "probe_update_ms": passes_get(t, 2),
        "draws": {
            "drawn_mean": mean(0), "drawn_max": t.draws_max, "instanced_mean": mean(1),
            "per_object_mean": mean(2), "culled_mean": mean(3),
            // Draws skipped because a mesh, object or material entry was gone (W2.2): 0 in a
            // healthy run. Window total, not a mean: the growth of the renderer's running total
            // (texture views included), each skip counted once.
            "skipped": t.draws_sum[4], "skinned_culled_mean": mean(5),
            "zero_key_moving_meshes": t.zero_key_moving_max,
        },
        // Window means (ms) of the frame thread's shadow work (`FrameRenderer::shadow_cpu_timings`):
        // what a CPU-bound frame spends on its sun cascades, by part.
        "shadow_cpu_ms": {
            "total": { "mean": mean_shadow(0), "max": r2(t.shadow_cpu_max) },
            "plan": mean_shadow(1),
            "terrain_prepare": mean_shadow(2),
            "caster_lists": mean_shadow(3),
            "instance_lists": mean_shadow(4),
            "vis_cull": mean_shadow(5),
            "encode": {
                "cascade0": mean_shadow(6), "cascade1": mean_shadow(7), "cascade2": mean_shadow(8),
                "cascade3": mean_shadow(9), "atlas": mean_shadow(10),
            },
            "translucent": mean_shadow(11),
        },
        // v0.1.18 (ADR-225): the emissive fit cache over the window (`emissive_lights`).
        "emissive": emissive_json(renderer, &t.emissive_at_window, t.emissive_ms_max),
        "scatter_emitted": taps::gauge(Gauge::ScatterEmitted),
        "particles_live": taps::gauge(Gauge::ParticlesLive),
        "textures": textures,
        "memory": {
            "rss_bytes": probe_mem.rss,
            "rss_peak_bytes": probe_mem.rss_peak,
            "vram_allocated_bytes": probe_mem.vram_allocated,
            "vram_reserved_bytes": probe_mem.vram_reserved,
            // v0.1.16 (ADR-225): what filled the GPU, and the out-of-memory state.
            "categories": memory_categories_json(),
            "pressure": memory_pressure_json(renderer),
            // v0.1.28 (ADR-539): CPU heap and GPU bytes per engine subsystem.
            "subsystems": memory_subsystems_json(),
            // v0.5.1 (ADR-7891): the engine's CPU copies of mesh and texture data under the host
            // memory budget.
            "host": memory_host_json(),
        },
        "hitches": t.hitches_window,
        "hitches_suppressed": t.hitches_suppressed_window,
        "created": t.creates.json(),
        "created_after_warmup": t.late_sites.json(t.creates_after_warm.json()),
        "warm": t.warm(),
        "surface_errors": {
            "outdated": delta(Counter::SurfaceOutdated), "lost": delta(Counter::SurfaceLost),
            "timeout": delta(Counter::SurfaceTimeout), "other": delta(Counter::SurfaceOther),
        },
        "gpu_errors": delta(Counter::GpuErrors),
        "asset_loads": delta(Counter::AssetLoads),
        "texture_evictions": delta(Counter::TextureEvictions),
        "notes_dropped": taps::notes_dropped(),
        "events_suppressed": t.repeats.suppressed_total() + t.slow_repeats.suppressed_total(),
    });
    // v0.5.1: why the window's bound is unknown (its unknown frames' most frequent reason).
    let bound_reason = if bound == "unknown" {
        t.live_window.unknown_reason().map(UnknownBound::name)
    } else {
        None
    };
    let live = t.live_window.take_json(&t.live);
    if let Value::Object(map) = &mut record {
        for (name, value) in sections.into_iter().chain(live) {
            map.insert(name.to_owned(), value);
        }
        // ADR-385: times the renderer's per-frame lists had to grow in the window (0 after
        // warm-up in a settled scene), and its running total. Outside the `json!` above, which
        // is near the macro's recursion limit.
        map.insert("bound_reason".to_owned(), json!(bound_reason));
        map.insert("present_waits".to_owned(), json!(t.present_waits_window));
        map.insert(
            "frame_scratch".to_owned(),
            json!({
                "growths": t.scratch_growths_window,
                "growths_after_warmup": t.scratch_growths_after_warm,
                "total": renderer.map_or(0, |r| r.frame_scratch_stats().growths),
            }),
        );
    }
    let text: Arc<str> = Arc::from(record.to_string());
    let p50 = r2(t.frame_hist.percentile(0.5));
    let p99 = r2(t.frame_hist.percentile(0.99));
    tracing::info!(
        target: target::FRAME_SUMMARY,
        reason,
        frames = t.window_frames,
        p50_ms = p50,
        p99_ms = p99,
        hitches = t.hitches_window,
        data = %text,
        "frame summary ({reason}): {} frames, p50 {p50} ms, p99 {p99} ms, {} hitches",
        t.window_frames,
        t.hitches_window
    );
    *LAST_SUMMARY.lock().unwrap_or_else(PoisonError::into_inner) = Some(text.clone());
    t.last_summary = Some(text);
    t.counts.summaries += 1;

    // Window-level events.
    let surface = [
        delta(Counter::SurfaceOutdated),
        delta(Counter::SurfaceLost),
        delta(Counter::SurfaceTimeout),
        delta(Counter::SurfaceOther),
    ];
    if surface.iter().any(|&n| n > 0) {
        let data = json!({
            "frame": t.frame, "window_s": round2(now_s - t.window_start_s),
            "outdated": surface[0], "lost": surface[1], "timeout": surface[2], "other": surface[3],
            "total_outdated": counters[Counter::SurfaceOutdated as usize], "total_lost": counters[Counter::SurfaceLost as usize],
            "total_timeout": counters[Counter::SurfaceTimeout as usize], "total_other": counters[Counter::SurfaceOther as usize],
        })
        .to_string();
        t.counts.surface_events += 1;
        tracing::warn!(target: target::SURFACE, outdated = surface[0], lost = surface[1], timeout = surface[2], other = surface[3], data = %data, "surface errors in the last window");
    }
    if over_budget
        && let Some(repeated) = t
            .slow_repeats
            .check(rate::key(&[b"budget_exceeded"]), Instant::now())
    {
        let data = json!({ "kind": "budget_exceeded", "frame": t.frame, "textures": record["textures"], "repeated": repeated })
            .to_string();
        stream_event(t, "budget_exceeded", &data, "texture budget exceeded");
    }
    let wrong = regia_render::wrong_platform_textures();
    let wrong_n = wrong.decoded + wrong.unbound;
    if wrong_n > t.wrong_platform_seen {
        t.wrong_platform_seen = wrong_n;
        let data = json!({
            "kind": "wrong_platform_textures", "frame": t.frame, "decoded": wrong.decoded,
            "decoded_bytes": wrong.decoded_bytes, "unbound": wrong.unbound, "formats": wrong.formats, "labels": wrong.labels,
        })
        .to_string();
        stream_event(
            t,
            "wrong_platform_textures",
            &data,
            &format!("{wrong_n} texture(s) cooked for another platform"),
        );
    }

    // Restart the window.
    t.window_start_s = now_s;
    t.window_frames = 0;
    t.gaps = 0;
    t.held_window = 0;
    t.held_gpu_sum = 0.0;
    t.held_gpu_n = 0;
    t.frame_hist.clear();
    t.cpu_hist.clear();
    t.gpu_hist.clear();
    t.pass_sum = [0.0; PASS_COUNT];
    t.pass_max = [0.0; PASS_COUNT];
    t.pass_frames = 0;
    t.draws_sum = [0; 6];
    t.draws_max = 0;
    t.zero_key_moving_max = 0;
    t.draw_frames = 0;
    t.shadow_cpu_sum = [0.0; SHADOW_CPU_PARTS];
    t.shadow_cpu_max = 0.0;
    if let Some(r) = renderer {
        t.emissive_at_window = r.emissive_fit_stats();
    }
    t.emissive_ms_max = 0.0;
    t.creates = Creates::default();
    t.creates_after_warm = Creates::default();
    t.late_sites.clear();
    t.scratch_growths_window = 0;
    t.scratch_growths_after_warm = 0;
    t.hitches_window = 0;
    t.hitches_suppressed_window = 0;
    t.present_waits_window = 0;
    t.counters_at_window = counters;
}

fn passes_get(t: &Telemetry, i: usize) -> Value {
    if t.pass_frames == 0 {
        return Value::Null;
    }
    json!({ "mean": round2(t.pass_sum[i] / t.pass_frames as f64), "max": r2(t.pass_max[i]) })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod stall_tests;
