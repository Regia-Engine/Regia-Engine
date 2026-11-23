// SPDX-License-Identifier: MPL-2.0
//! The first-launch confirmation probe (ADR-157, reworked by ADR-173): after the boot splash is
//! gone and the game-frame warm is back, a probe renderer on a warm worker draws the host's
//! first scene (DO27's lobby) offscreen at the window's size, frame after frame, with GPU
//! timestamps on, so the GPU is busy enough to reach its clocks. It never holds the splash and
//! never touches the runtime renderer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::RenderDevice;

/// How long the probe runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeConfig {
    /// Frames dropped first (GPU clocks ramp up, temporal history fills, first-frame
    /// transients such as the GI probes' first update pass). At least this long...
    pub warmup: Duration,
    /// ...and at least this many probe frames.
    pub warmup_frames: u32,
    /// Frames timed after the warm-up.
    pub measure: Duration,
    /// Gives up (a result with no frames) when it cannot start or finish for this long.
    pub timeout: Duration,
}

impl Default for ProbeConfig {
    /// 0.75 s and at least 30 frames of warm-up, then 2.25 s measured; 15 s timeout.
    fn default() -> Self {
        Self {
            warmup: Duration::from_millis(750),
            warmup_frames: ProbeConfig::MIN_WARMUP_FRAMES,
            measure: Duration::from_millis(2250),
            timeout: Duration::from_secs(15),
        }
    }
}

impl ProbeConfig {
    /// The fewest warm-up frames any probe drops.
    pub const MIN_WARMUP_FRAMES: u32 = 30;

    /// A probe of `seconds` in all, a quarter of it warm-up (and at least
    /// [`Self::MIN_WARMUP_FRAMES`] warm-up frames).
    #[must_use]
    pub fn with_seconds(seconds: f32) -> Self {
        let total = Duration::from_secs_f32(seconds.clamp(0.5, 30.0));
        Self {
            warmup: total / 4,
            measure: total - total / 4,
            ..Self::default()
        }
    }
}

/// Which clock the samples came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeSource {
    /// GPU timestamps (`GpuTimings::frame_ms`): the GPU's busy time per frame.
    Gpu,
    /// The wall interval between frames (no timestamp queries; includes CPU and waits).
    Wall,
}

/// What the probe measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Timed frames.
    pub frames: u32,
    /// Median frame time, ms.
    pub p50_ms: f32,
    /// 95th percentile frame time, ms.
    pub p95_ms: f32,
    /// The clock.
    pub source: ProbeSource,
    /// Window size it drew at.
    pub width: u32,
    /// Window height.
    pub height: u32,
    /// It gave up before measuring (no frames).
    pub timed_out: bool,
    /// It was cut short ([`QualityProbe::abort`]: a level load started first); its frames
    /// are not judged.
    #[serde(default)]
    pub aborted: bool,
    /// Median share of the wall interval between probe frames that the GPU was busy (GPU
    /// timestamp time / wall interval, 0..=1). Under about a half the GPU was waiting
    /// (present- or CPU-bound) and likely downclocked, so its times say little about the tier.
    /// `None` when unknown (no timestamps, or a result measured elsewhere).
    #[serde(default)]
    pub gpu_busy: Option<f32>,
}

/// Nearest-rank percentile `q` (0..=1) of `samples` (unsorted), 0 for none.
#[must_use]
pub fn percentile(samples: &[f32], q: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f32::total_cmp);
    let rank = (q.clamp(0.0, 1.0) * sorted.len() as f32).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// The probe loop's bookkeeping, one call per probe frame (testable without a GPU).
#[derive(Debug)]
pub(crate) struct ProbeRun {
    config: ProbeConfig,
    started: Instant,
    frames: u32,
    /// When the measured window began (warm-up over).
    measuring_since: Option<Instant>,
    gpu: Vec<f32>,
    wall: Vec<f32>,
    busy: Vec<f32>,
}

impl ProbeRun {
    pub(crate) fn new(config: ProbeConfig, started: Instant) -> Self {
        Self {
            config,
            started,
            frames: 0,
            measuring_since: None,
            gpu: Vec::new(),
            wall: Vec::new(),
            busy: Vec::new(),
        }
    }

    /// One finished probe frame at `now`: `gpu_ms` its GPU time (0 when the timer has
    /// nothing), `wall_ms` the interval since the previous frame finished. Returns `true` once
    /// the measured window is over.
    pub(crate) fn frame(&mut self, now: Instant, gpu_ms: f32, wall_ms: f32) -> bool {
        self.frames += 1;
        if self.measuring_since.is_none() {
            let warm = self.frames > self.config.warmup_frames
                && now.saturating_duration_since(self.started) >= self.config.warmup;
            if !warm {
                return false;
            }
            self.measuring_since = Some(now);
        }
        if gpu_ms > 0.0 {
            self.gpu.push(gpu_ms);
            if wall_ms > 0.0 {
                self.busy.push((gpu_ms / wall_ms).min(1.0));
            }
        }
        self.wall.push(wall_ms);
        self.measuring_since
            .is_some_and(|at| now.saturating_duration_since(at) >= self.config.measure)
    }

    /// The result at `size` (empty and `timed_out` without measured frames).
    pub(crate) fn result(&self, size: (u32, u32)) -> ProbeResult {
        // Most of the measured frames must carry a GPU time to trust that clock.
        let (samples, source, timed_out) = if !self.gpu.is_empty() && self.gpu.len() * 2 >= self.wall.len() {
            (&self.gpu, ProbeSource::Gpu, false)
        } else if !self.wall.is_empty() {
            (&self.wall, ProbeSource::Wall, false)
        } else {
            (&self.wall, ProbeSource::Wall, true)
        };
        ProbeResult {
            frames: samples.len() as u32,
            p50_ms: percentile(samples, 0.5),
            p95_ms: percentile(samples, 0.95),
            source,
            width: size.0,
            height: size.1,
            timed_out,
            aborted: false,
            gpu_busy: (source == ProbeSource::Gpu && !self.busy.is_empty())
                .then(|| percentile(&self.busy, 0.5)),
        }
    }
}

/// What the worker hands back.
#[derive(Default)]
struct Shared {
    result: Option<ProbeResult>,
    error: Option<String>,
    done: bool,
}

/// A probe running on a warm worker.
struct Running {
    started: Instant,
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    size: (u32, u32),
}

/// The confirmation probe (a resource of `RenderPlugin`).
///
/// [`QualityProbe::request`] arms it. Once the splash is gone, no game-frame warm is pending
/// and the window draws a renderable scene, the window path starts it on a
/// [`crate::PipelineWarmQueue`] worker (tier `Optional`): a fresh renderer with GPU timing on
/// (with copies of the runtime renderer's installs: VFX layers, GI field, reflection probes,
/// interior scene; not the terrain shared draw) warms its pipelines on that frame,
/// mostly from the pipeline cache, then draws it offscreen at the window's size back to back,
/// one frame in flight at a time. The GPU stays busy enough to ramp its clocks, and the host's
/// frames (the lobby) wait at most one probe frame on the GPU queue. After the warm-up (at
/// least [`ProbeConfig::warmup`] and [`ProbeConfig::warmup_frames`]) it times
/// [`ProbeConfig::measure`] of frames by their GPU timestamps and publishes a [`ProbeResult`]
/// that [`QualityProbe::take_result`] hands to the caller. The runtime renderer and the
/// visible frames are untouched, and the splash is never held. A window that draws no scene
/// for [`ProbeConfig::timeout`] ends it with no frames; [`QualityProbe::abort`] ends it early (a
/// level load starts); the warm queue shutting down stops it too.
#[derive(Resource, Default)]
pub struct QualityProbe {
    config: Option<ProbeConfig>,
    requested_at: Option<Instant>,
    running: Option<Running>,
    result: Option<ProbeResult>,
    runs: u32,
}

/// Whether the window path may start a requested probe this tick (ADR-173).
///
/// The splash must be finished (or absent), no game-frame warm may be pending, and the
/// window must not already be warming a scene. A request does not start the probe, and
/// the probe never holds the splash.
#[must_use]
pub(crate) fn probe_start_allowed(splash_done: bool, game_warm_pending: bool, scene_warming: bool) -> bool {
    splash_done && !game_warm_pending && !scene_warming
}

impl QualityProbe {
    /// Arms a probe (replacing one that has not started).
    pub fn request(&mut self, config: ProbeConfig) {
        if self.running.is_none() {
            self.config = Some(config);
            self.requested_at = Some(Instant::now());
            self.result = None;
        }
    }

    /// Whether a probe is requested, running, or has an untaken result.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.config.is_some() || self.result.is_some()
    }

    /// Whether probe frames are being drawn and timed now.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Probes finished in this process.
    #[must_use]
    pub fn runs(&self) -> u32 {
        self.runs
    }

    /// Finishes with a result measured elsewhere: a shell that drives `FrameRenderer` itself and
    /// times its own frames, or a test.
    pub fn complete_with(&mut self, result: ProbeResult) {
        self.stop_worker();
        self.config = None;
        self.requested_at = None;
        self.result = Some(result);
        self.runs += 1;
    }

    /// The finished probe's result (once).
    pub fn take_result(&mut self) -> Option<ProbeResult> {
        self.result.take()
    }

    /// Ends a requested or running probe now with an aborted result (no frames judged): a
    /// level load started before it finished, so the table's pick stays. The worker stops
    /// after its current frame. Returns whether a probe was cut.
    pub fn abort(&mut self) -> bool {
        if self.config.is_none() {
            return false;
        }
        let size = self.running.as_ref().map_or((0, 0), |r| r.size);
        self.stop_worker();
        self.config = None;
        self.requested_at = None;
        self.result = Some(ProbeResult {
            frames: 0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            source: ProbeSource::Wall,
            width: size.0,
            height: size.1,
            timed_out: false,
            aborted: true,
            gpu_busy: None,
        });
        self.runs += 1;
        true
    }

    fn stop_worker(&mut self) {
        if let Some(run) = self.running.take() {
            run.cancel.store(true, Ordering::Release);
        }
    }

    /// Requested and not started: the window path may start it this tick.
    pub(crate) fn wants_start(&self) -> bool {
        self.config.is_some() && self.running.is_none()
    }

    /// `Render` (window path): starts the probe on a warm worker with `frame` (renderable) at
    /// `size` in `format`. `translucent_from` lends its installs (VFX layers, GI field, reflection probes) to the probe
    /// renderer.
    pub(crate) fn start(
        &mut self,
        queue: &crate::PipelineWarmQueue,
        translucent_from: &crate::FrameRenderer,
        frame: &crate::ExtractedFrame,
        format: wgpu::TextureFormat,
        size: (u32, u32),
    ) {
        let Some(config) = self.config else { return };
        if self.running.is_some() || size.0 == 0 || size.1 == 0 {
            return;
        }
        let shared = Arc::new(Mutex::new(Shared::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut renderer = crate::FrameRenderer::new();
        translucent_from.share_installs_with(&mut renderer);
        let frame = frame.clone();
        let (job_shared, job_cancel, job_queue) = (shared.clone(), cancel.clone(), queue.clone());
        crate::boot::mark_named("quality_probe_start");
        queue.push(
            crate::WarmTier::Optional,
            "quality probe",
            move |device: &RenderDevice| {
                let stop = || job_cancel.load(Ordering::Acquire) || job_queue.is_shutting_down();
                let outcome = run_probe(device, renderer, &frame, format, size, config, &stop);
                let mut s = job_shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match outcome {
                    Ok(result) => s.result = result,
                    Err(err) => s.error = Some(err),
                }
                s.done = true;
                Ok(())
            },
        );
        self.running = Some(Running {
            started: Instant::now(),
            shared,
            cancel,
            size,
        });
    }

    /// `Render`: publishes a finished worker's result, and ends a probe that could not start or
    /// finish within [`ProbeConfig::timeout`] (no frames).
    pub(crate) fn poll(&mut self, now: Instant) {
        let Some(config) = self.config else { return };
        if let Some(run) = &self.running {
            let (done, result, error) = {
                let mut s = run
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (s.done, s.result.take(), s.error.take())
            };
            if done {
                if let Some(err) = error {
                    tracing::warn!(target: "regia_render", "quality probe failed: {err}");
                }
                let size = run.size;
                self.running = None;
                self.finish(result.unwrap_or_else(|| ProbeRun::new(config, now).result(size)));
                return;
            }
            // Started work gets the timeout again from its start (its warm can take a while).
            if now.saturating_duration_since(run.started) > config.timeout + config.warmup + config.measure {
                tracing::warn!(target: "regia_render", "quality probe did not finish in time; stopped");
                let size = run.size;
                self.stop_worker();
                self.finish(ProbeRun::new(config, now).result(size));
            }
            return;
        }
        if self
            .requested_at
            .is_some_and(|at| now.saturating_duration_since(at) > config.timeout)
        {
            // Nothing renderable to probe (a UI-only first screen, a hidden window).
            self.finish(ProbeRun::new(config, now).result((0, 0)));
        }
    }

    fn finish(&mut self, result: ProbeResult) {
        crate::boot::mark_named("quality_probe_done");
        tracing::info!(
            target: "regia_render",
            "quality probe: {} frames, p50 {:.2} ms, p95 {:.2} ms ({:?}, GPU busy {:?})",
            result.frames,
            result.p50_ms,
            result.p95_ms,
            result.source,
            result.gpu_busy
        );
        self.config = None;
        self.requested_at = None;
        self.result = Some(result);
        self.runs += 1;
    }
}

/// The worker side: warms `renderer` on `frame`, then draws it offscreen at `size` until the
/// run is measured or `stop` says so. `Ok(None)`: stopped early.
fn run_probe(
    device: &RenderDevice,
    mut renderer: crate::FrameRenderer,
    frame: &crate::ExtractedFrame,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    config: ProbeConfig,
    stop: &dyn Fn() -> bool,
) -> Result<Option<ProbeResult>, String> {
    let _span = tracing::info_span!("quality_probe").entered();
    renderer.set_gpu_timing(device, true);
    // Pipelines (from the cache after the game-frame warm) and the window-size targets.
    renderer
        .warm_extracted_at(device, frame, format, false, Some(size))
        .map_err(|e| e.to_string())?;
    let Some(view_index) = frame.window_views().first().map(|(index, _)| *index) else {
        return Err("the frame has no window view".into());
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("quality probe target"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
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
    let scene = frame.frame_scene(view_index, size.0, size.1);
    let mut run = ProbeRun::new(config, Instant::now());
    let mut last = Instant::now();
    loop {
        if stop() {
            return Ok(None);
        }
        let target = crate::FrameTarget::rect(&view, format, size.0, size.1, [0, 0, size.0, size.1], false);
        renderer
            .render_into(device, &scene, target)
            .map_err(|e| e.to_string())?;
        // One probe frame in flight: the host's frames wait at most one probe frame.
        device.wait_idle().map_err(|e| e.to_string())?;
        let now = Instant::now();
        let wall_ms = now.saturating_duration_since(last).as_secs_f32() * 1000.0;
        last = now;
        // The timer lags a frame or two behind `render_into`; in steady state that is the same.
        if run.frame(now, renderer.gpu_timings().frame_ms, wall_ms) {
            return Ok(Some(run.result(size)));
        }
    }
}
