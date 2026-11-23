// SPDX-License-Identifier: MPL-2.0
//! Boot fast path (`docs/specs/13-rendering.md` § 2c): the boot timeline and the branding screen.
//!
//! The first image a player sees must be on screen in under a second (DO27: aim 0.5 s). The
//! renderer presents a [`BootSplash`] — an image, a line of text and a fade — from the very
//! lifecycle event that creates the window surface, with its own two-pipeline-free draw (one
//! shader module, one pipeline per surface format) and before any game frame runs. While the
//! splash shows, the main renderer is not touched: no scene draw, no UI overlay, so no lazy
//! pipeline creation lands on the boot path. The tiered [`crate::PipelineWarmQueue`] starts right
//! after the first present and compiles the boot and menu pipelines on worker threads;
//! [`BootSplash::finish`] fades the splash out over the first real frames. The 1.5 s cap
//! ([`BootSplash::max_duration`], ADR-173) starts that fade only after the host has called
//! [`BootSplash::finish`]; a splash the host has not finished stays up. What is not done by
//! the cap continues in the background ([`BootBackground`]).
//!
//! [`mark`] records the boot timeline (process start → plugin build → device → surface → first
//! present → …) for `render.boot_timeline`; games add their own marks with [`mark_named`].

use crate::frame_work::counted::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia_app::prelude::*;

use crate::RenderDevice;

// ---------------------------------------------------------------------------------------------
// Boot timeline
// ---------------------------------------------------------------------------------------------

/// Engine boot milestones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BootMark {
    /// `RenderPlugin::build` ran (the app is being assembled).
    PluginBuild,
    /// The first resume (window exists; surfaces are created next).
    Resumed,
    /// The render device exists.
    Device,
    /// The first window surface exists.
    Surface,
    /// The first frame (splash or game) was presented.
    FirstPresent,
    /// The warm queue's boot tier finished.
    BootTierReady,
    /// The warm queue's menu tier finished.
    MenuTierReady,
    /// The splash finished fading out.
    SplashDone,
}

impl BootMark {
    /// Stable snake-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::PluginBuild => "plugin_build",
            Self::Resumed => "resumed",
            Self::Device => "device",
            Self::Surface => "surface",
            Self::FirstPresent => "first_present",
            Self::BootTierReady => "boot_tier_ready",
            Self::MenuTierReady => "menu_tier_ready",
            Self::SplashDone => "splash_done",
        }
    }
}

struct Timeline {
    /// Reference instant and the process age at that instant.
    origin: Option<(Instant, Duration)>,
    marks: Vec<(String, Duration)>,
}

static TIMELINE: Mutex<Timeline> = Mutex::new(Timeline {
    origin: None,
    marks: Vec::new(),
});

fn with_timeline<T>(f: impl FnOnce(&mut Timeline) -> T) -> T {
    let mut t = TIMELINE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if t.origin.is_none() {
        // Linux/Android count from process creation; elsewhere from the first mark.
        t.origin = Some((
            Instant::now(),
            crate::platform::process_age().unwrap_or(Duration::ZERO),
        ));
    }
    f(&mut t)
}

/// Time since the process started (since the first boot mark where the OS does not say).
#[must_use]
pub fn since_process_start() -> Duration {
    with_timeline(|t| {
        let (at, age) = t.origin.unwrap_or((Instant::now(), Duration::ZERO));
        age + at.elapsed()
    })
}

/// Records `mark` once (later calls keep the first time).
pub fn mark(mark: BootMark) {
    mark_named(mark.name());
}

/// Records a named mark once (games: `"menu_interactive"`, `"in_control"`).
pub fn mark_named(name: &str) {
    let now = since_process_start();
    with_timeline(|t| {
        if !t.marks.iter().any(|(n, _)| n == name) {
            tracing::info!(target: "regia_render", "boot: {name} at {:.1} ms", now.as_secs_f64() * 1000.0);
            t.marks.push((name.to_string(), now));
            MARK_COUNT.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    });
}

static MARK_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Marks recorded so far (one atomic load; telemetry polls it to notice a new mark without
/// copying the timeline).
#[must_use]
pub fn mark_count() -> usize {
    MARK_COUNT.load(std::sync::atomic::Ordering::Acquire)
}

/// The boot timeline: `(mark, milliseconds since process start)` in the order recorded.
#[must_use]
pub fn timeline() -> Vec<(String, f64)> {
    with_timeline(|t| {
        t.marks
            .iter()
            .map(|(n, d)| (n.clone(), d.as_secs_f64() * 1000.0))
            .collect()
    })
}

/// Milliseconds from process start to `mark`, if recorded.
#[must_use]
pub fn mark_ms(name: &str) -> Option<f64> {
    timeline().into_iter().find(|(n, _)| n == name).map(|(_, ms)| ms)
}

// ---------------------------------------------------------------------------------------------
// Background boot work (ADR-173)
// ---------------------------------------------------------------------------------------------

/// What the engine is still doing in the background after the boot splash (ADR-173). Once
/// the host has called [`BootSplash::finish`], the splash is gone after at most
/// [`BootSplash::max_duration`] (1.5 s). A splash the host has not finished stays up.
/// Whatever was not done by the cap (the game-frame warm, the deferred pipeline variants,
/// the first-launch quality probe) carries on behind the host's first scene (DO27's lobby).
///
/// A render-plugin resource, refreshed in `PreUpdate`, so `Update` systems read this frame's
/// state; it only changes (change detection) when a field does. Hosts check it before entering
/// gameplay:
///
/// - [`Self::warm_done`]: no game-frame warm is requested or running. Entering a map while it
///   runs is safe anyway (the window draws no scene until the renderer is back: the loading
///   path), but a host that wants to keep its lobby up until then waits for this. It is not
///   "the map is resident and on screen": for that, call [`crate::SceneReadiness::prepare`]
///   and wait for the [`crate::SceneReady`] message (ADR-7883).
/// - [`Self::probe_done`]: the quality probe has finished and its preset, if it changed, was
///   applied (a `SettingsChanged` was sent). A host never has to wait for it: a level load
///   (`GameFrameWarm::request`) aborts a probe that has not finished and keeps the table pick.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct BootBackground {
    /// The splash is gone, or there never was one.
    pub splash_done: bool,
    /// The cap ended the splash while a game-frame warm or its minimum time still held it.
    pub splash_capped: bool,
    /// A game-frame warm is requested or running ([`crate::GameFrameWarm::is_pending`]).
    pub game_warm_pending: bool,
    /// The runtime renderer is on a warm worker now; the window draws no scene.
    pub game_warm_running: bool,
    /// Deferred pipeline variants still waiting for a background build
    /// ([`crate::pipeline_warm::deferred_pending`]).
    pub deferred_pending: usize,
    /// The quality probe is requested, timing frames, or has a result not yet applied.
    pub probe_active: bool,
}

impl BootBackground {
    /// No game-frame warm is requested or running. Not a scene-ready signal: see
    /// [`crate::SceneReady`].
    #[must_use]
    pub fn warm_done(&self) -> bool {
        !self.game_warm_pending
    }

    /// The quality probe is not running (finished, aborted, or never requested).
    #[must_use]
    pub fn probe_done(&self) -> bool {
        !self.probe_active
    }

    /// Splash gone, no game-frame warm, no deferred variant pending, no probe.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.splash_done && self.warm_done() && self.deferred_pending == 0 && self.probe_done()
    }
}

/// `PreUpdate`: refreshes [`BootBackground`] and marks `boot_background_idle` once.
pub(crate) fn track_boot_background(
    mut state: ResMut<BootBackground>,
    splash: Option<Res<BootSplash>>,
    warm: Option<Res<crate::GameFrameWarm>>,
    probe: Option<Res<crate::QualityProbe>>,
) {
    let next = BootBackground {
        splash_done: splash.as_deref().is_none_or(BootSplash::is_done),
        splash_capped: splash.as_deref().is_some_and(BootSplash::was_capped),
        game_warm_pending: warm.as_deref().is_some_and(crate::GameFrameWarm::is_pending),
        game_warm_running: warm.as_deref().is_some_and(crate::GameFrameWarm::is_warming),
        deferred_pending: crate::pipeline_warm::deferred_pending(),
        probe_active: probe.as_deref().is_some_and(crate::QualityProbe::is_active),
    };
    if *state != next {
        if next.is_idle() && !state.is_idle() {
            mark_named("boot_background_idle");
        }
        *state = next;
    }
}

// ---------------------------------------------------------------------------------------------
// Branding screen
// ---------------------------------------------------------------------------------------------

/// An RGBA8 (sRGB) image for the splash.
#[derive(Clone)]
pub struct SplashImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Tightly packed RGBA8 rows, top row first.
    pub rgba: Arc<[u8]>,
}

/// Where the splash is in its life.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SplashPhase {
    /// Covering the window; the game frame is not drawn. `opacity` of the logo and text (fade in).
    Showing {
        /// 0..=1.
        opacity: f32,
    },
    /// Drawn over the game frame while it fades out.
    FadingOut {
        /// 0..=1 of the whole splash, background included.
        opacity: f32,
    },
    /// Gone.
    Done,
}

/// The boot branding screen. Insert it before the app runs; the renderer presents it from the
/// first resume until [`Self::finish`] and its fade-out. A running [`crate::GameFrameWarm`]
/// holds the fade-out, but never past [`Self::max_duration`] once the host has called
/// [`Self::finish`] (1.5 s after process start by default, ADR-173). The cap does not fade
/// a splash the host has not finished.
#[derive(Resource)]
pub struct BootSplash {
    /// Logo / key art, centred.
    pub image: Option<SplashImage>,
    /// A line of text under the image, drawn with the built-in 5×7 pixel font (A–Z, 0–9 and
    /// common punctuation; lower case is drawn as upper case).
    pub text: String,
    /// Background colour (linear RGBA).
    pub background: [f32; 4],
    /// Text colour (linear RGBA).
    pub text_color: [f32; 4],
    /// Largest image size as a fraction of the window's smaller side.
    pub image_scale: f32,
    /// Text cap height as a fraction of the window height.
    pub text_scale: f32,
    /// Fade-in of the image and text.
    pub fade_in: Duration,
    /// Fade-out over the game frame after [`Self::finish`].
    pub fade_out: Duration,
    /// Shortest time on screen (a finish earlier than this waits).
    pub min_duration: Duration,
    /// Latest time after process start at which the splash is gone, fade-out included
    /// (ADR-173; default [`SPLASH_MAX_DURATION`], 1.5 s; `REGIA_SPLASH_MAX_MS` overrides it,
    /// `0` = no cap). Once the host has called [`Self::finish`], the fade-out starts at the
    /// latest at this time minus [`Self::fade_out`], whatever still holds it (a running
    /// [`crate::GameFrameWarm`], [`Self::min_duration`]). That work carries on in the
    /// background behind the host's first scene. The cap never fades a splash the host has
    /// not finished: [`Self::finish`] is the host's "my first screen can draw" signal.
    /// `None`: no cap (the splash waits for every hold, as before ADR-173).
    pub max_duration: Option<Duration>,
    shown_at: Option<Instant>,
    /// When [`Self::finish`] was called.
    finish_called: Option<Instant>,
    finish_at: Option<Instant>,
    /// When the cap starts the fade-out (from [`Self::max_duration`] at the first present).
    cap_at: Option<Instant>,
    /// The cap started the fade-out while a hold or the minimum time still ran.
    capped: bool,
    /// The fade-out waits (a [`crate::GameFrameWarm`] is running).
    held: bool,
    done: bool,
    gpu: Option<SplashGpu>,
    /// The warm queue that builds the splash pipeline ([`Self::set_warm_queue`]).
    warm: SplashWarm,
}

/// Which queue builds the splash pipeline ([`BootSplash::set_warm_queue`]).
#[derive(Default)]
enum SplashWarm {
    /// Never set: the renderer attaches the app's queue.
    #[default]
    Unset,
    /// Set to `None`: built where [`BootSplash::record`] runs, even when the app has a queue.
    Off,
    /// This queue (set by the host, or attached by the renderer).
    Queue(crate::PipelineWarmQueue),
}

impl SplashWarm {
    fn queue(&self) -> Option<&crate::PipelineWarmQueue> {
        match self {
            Self::Queue(queue) => Some(queue),
            Self::Unset | Self::Off => None,
        }
    }
}

impl Default for BootSplash {
    fn default() -> Self {
        Self {
            image: None,
            text: String::new(),
            background: [0.0, 0.0, 0.0, 1.0],
            text_color: [0.8, 0.8, 0.8, 1.0],
            image_scale: 0.5,
            text_scale: 0.03,
            fade_in: Duration::from_millis(250),
            fade_out: Duration::from_millis(250),
            min_duration: Duration::ZERO,
            max_duration: splash_max_from_env(),
            shown_at: None,
            finish_called: None,
            finish_at: None,
            cap_at: None,
            capped: false,
            held: false,
            done: false,
            gpu: None,
            warm: SplashWarm::Unset,
        }
    }
}

/// Default [`BootSplash::max_duration`]: once the host has called [`BootSplash::finish`], the
/// splash is gone 1.5 s after process start at the latest (owner, v0.1.0: the splash leads
/// into DO27's lobby; get there fast). The cap does not fade a splash the host has not finished.
pub const SPLASH_MAX_DURATION: Duration = Duration::from_millis(1500);

/// [`SPLASH_MAX_DURATION`], or `REGIA_SPLASH_MAX_MS` (milliseconds; `0` = no cap).
fn splash_max_from_env() -> Option<Duration> {
    match regia_core::brand::env_var("REGIA_SPLASH_MAX_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(SPLASH_MAX_DURATION),
    }
}

impl BootSplash {
    /// A black splash with no image or text.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// With `image`.
    #[must_use]
    pub fn with_image(mut self, image: SplashImage) -> Self {
        self.image = Some(image);
        self
    }

    /// With a line of text.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// With a cap ([`Self::max_duration`]; `None` = no cap).
    #[must_use]
    pub fn with_max_duration(mut self, max: Option<Duration>) -> Self {
        self.max_duration = max;
        self
    }

    /// Builds the splash pipeline on `queue`'s boot tier instead of on the presenting thread
    /// (ADR-5131): the first [`Self::record`] for a surface format queues a `"boot splash"` job
    /// (the first boot job when the queue has not started yet) and clears the target to
    /// [`Self::background`] until that job is done; the image and the text then fade in as
    /// before. With `None`, or a queue that is shutting down, the pipeline is built where
    /// [`Self::record`] runs. A splash this was never called on gets the app's queue: the
    /// renderer attaches it by itself; `None` opts out of that.
    pub fn set_warm_queue(&mut self, queue: Option<crate::PipelineWarmQueue>) {
        self.warm = match queue {
            Some(queue) => SplashWarm::Queue(queue),
            None => SplashWarm::Off,
        };
    }

    /// The renderer's half of [`Self::set_warm_queue`]: attaches `queue` to a splash the host
    /// did not set (one set to `None` stays without).
    pub(crate) fn attach_warm_queue(&mut self, queue: Option<&crate::PipelineWarmQueue>) {
        if let (SplashWarm::Unset, Some(queue)) = (&self.warm, queue) {
            self.warm = SplashWarm::Queue(queue.clone());
        }
    }

    /// Starts the fade-out (after [`Self::min_duration`]). The game calls this when its menu (or
    /// whatever follows) can draw.
    pub fn finish(&mut self) {
        // One instant for both, so a finish after the cap is never mistaken for a capped one.
        let now = Instant::now();
        if self.finish_called.is_none() {
            self.finish_called = Some(now);
        }
        if self.finish_at.is_none() {
            let earliest = self.shown_at.map(|t| t + self.min_duration);
            self.finish_at = Some(earliest.map_or(now, |e| e.max(now)));
        }
    }

    /// Holds the fade-out while `held` (the runtime renderer warms the first game frame on a
    /// worker, [`crate::GameFrameWarm`]); a finish that came due meanwhile fades from the
    /// release.
    pub(crate) fn set_held(&mut self, held: bool) {
        if self.held
            && !held
            && let Some(at) = self.finish_at
        {
            self.finish_at = Some(at.max(Instant::now()));
        }
        self.held = held;
    }

    /// Whether the fade-out waits for a game-frame warm ([`crate::GameFrameWarm`]).
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Whether [`Self::finish`] was called.
    #[must_use]
    pub fn is_finishing(&self) -> bool {
        self.finish_at.is_some()
    }

    /// Whether the splash is gone.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Whether the cap ([`Self::max_duration`]) started the fade-out while a hold (a running
    /// [`crate::GameFrameWarm`]) or [`Self::min_duration`] would still have kept the splash.
    #[must_use]
    pub fn was_capped(&self) -> bool {
        self.capped
    }

    /// When the fade-out starts, if it has started by `now`: at the finish once nothing holds
    /// it, or at the cap ([`Self::max_duration`]) once the host has finished, whichever is
    /// first.
    fn fade_start(&self, now: Instant) -> (Option<Instant>, bool) {
        let normal = self.finish_at.filter(|at| now >= *at && !self.held);
        let capped = match (self.cap_at, self.finish_called) {
            (Some(cap), Some(called)) => Some(cap.max(called)).filter(|at| now >= *at),
            _ => None,
        };
        match (normal, capped) {
            (Some(a), Some(b)) if b < a => (Some(b), true),
            (Some(a), _) => (Some(a), false),
            (None, Some(b)) => (Some(b), true),
            (None, None) => (None, false),
        }
    }

    /// The phase at `now`.
    #[must_use]
    pub fn phase_at(&self, now: Instant) -> SplashPhase {
        if self.done {
            return SplashPhase::Done;
        }
        let shown = self.shown_at.unwrap_or(now);
        let fade_in = ramp(now.saturating_duration_since(shown), self.fade_in);
        match self.fade_start(now).0 {
            Some(at) => {
                let out = ramp(now.saturating_duration_since(at), self.fade_out);
                if out >= 1.0 {
                    SplashPhase::Done
                } else {
                    SplashPhase::FadingOut { opacity: 1.0 - out }
                }
            }
            None => SplashPhase::Showing { opacity: fade_in },
        }
    }

    /// Advances the state to `now` and returns the phase (marks the splash shown on first call).
    pub fn advance(&mut self, now: Instant) -> SplashPhase {
        if self.shown_at.is_none() {
            self.shown_at = Some(now);
            // The cap counts from process start and includes the fade-out.
            self.cap_at = self.max_duration.map(|max| {
                now + max
                    .saturating_sub(self.fade_out)
                    .saturating_sub(since_process_start())
            });
        }
        if !self.capped && self.fade_start(now).1 {
            self.capped = true;
            mark_named("splash_capped");
            tracing::info!(
                target: "regia_render",
                "boot splash: cap reached (held by a game-frame warm: {}); the rest continues in the background",
                self.held
            );
        }
        let phase = self.phase_at(now);
        if phase == SplashPhase::Done && !self.done {
            self.done = true;
            self.gpu = None;
            mark(BootMark::SplashDone);
        }
        phase
    }

    /// Records the splash into `view` (a `width`×`height` target in `format`). `Showing` clears
    /// to the background first; `FadingOut` blends over what the target already holds.
    pub fn record(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        (width, height): (u32, u32),
        phase: SplashPhase,
    ) {
        let _span = tracing::info_span!("boot_splash").entered();
        let (clear, opacity, background_alpha) = match phase {
            SplashPhase::Showing { opacity } => (true, opacity, 0.0),
            SplashPhase::FadingOut { opacity } => (false, opacity, opacity),
            SplashPhase::Done => return,
        };
        if self.gpu.is_none() {
            self.gpu = Some(SplashGpu::new(device, self.image.as_ref(), &self.text));
        }
        let warm = self.warm.queue().cloned();
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        let pipeline = gpu.pipeline(device, format, warm.as_ref());
        let bg = self.background;
        let Some(pipeline) = pipeline else {
            // The pipeline is still on the warm queue: the background alone (a clear needs no
            // pipeline). A fade-out has nothing to blend yet.
            if clear {
                let _pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("boot splash"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(splash_clear(bg)),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
            }
            return;
        };
        let aspect = width.max(1) as f32 / height.max(1) as f32;
        let mut quads: Vec<(usize, [f32; 4], [f32; 4])> = Vec::new();
        quads.push((0, [-1.0, -1.0, 1.0, 1.0], [bg[0], bg[1], bg[2], background_alpha]));
        if let Some((w, h)) = gpu.image_size {
            // Fit the image in `image_scale` of the smaller window side, keep its aspect.
            let side = self.image_scale.clamp(0.05, 1.0);
            let (iw, ih) = (w as f32, h as f32);
            let (mut half_w, mut half_h) = if aspect >= 1.0 {
                (side * (iw / ih) / aspect, side)
            } else {
                (side, side * aspect * (ih / iw))
            };
            let over = half_w.max(half_h);
            if over > 1.0 {
                half_w /= over;
                half_h /= over;
            }
            let cy = if gpu.text_size.is_some() { 0.08 } else { 0.0 };
            quads.push((
                1,
                [-half_w, cy - half_h, half_w, cy + half_h],
                [1.0, 1.0, 1.0, opacity],
            ));
        }
        if let Some((tw, th)) = gpu.text_size {
            let half_h = self.text_scale.clamp(0.005, 0.2) * (th as f32 / 7.0);
            let half_w = half_h * (tw as f32 / th as f32) / aspect;
            let (half_w, half_h) = if half_w > 0.95 {
                (0.95, half_h * 0.95 / half_w)
            } else {
                (half_w, half_h)
            };
            let cy = if gpu.image_size.is_some() { -0.62 } else { 0.0 };
            let c = self.text_color;
            quads.push((
                2,
                [-half_w, cy - half_h, half_w, cy + half_h],
                [c[0], c[1], c[2], c[3] * opacity],
            ));
        }
        for (slot, rect, tint) in &quads {
            let uniform = QuadUniform {
                rect: *rect,
                tint: *tint,
            };
            device
                .queue
                .write_buffer(&gpu.uniforms[*slot], 0, bytemuck::bytes_of(&uniform));
        }
        let load = if clear {
            wgpu::LoadOp::Clear(splash_clear(bg))
        } else {
            wgpu::LoadOp::Load
        };
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("boot splash"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.counted_set_pipeline(&pipeline);
        for (slot, _, tint) in &quads {
            if tint[3] <= 0.0 {
                continue;
            }
            pass.counted_set_bind_group(0, &gpu.bind_groups[*slot], &[]);
            pass.counted_draw(0..6, 0..1);
        }
    }
}

/// The clear colour of a splash background.
fn splash_clear(bg: [f32; 4]) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(bg[0]),
        g: f64::from(bg[1]),
        b: f64::from(bg[2]),
        a: f64::from(bg[3]),
    }
}

/// Smoothstep from 0 to 1 over `length` (1 at once for a zero length).
fn ramp(elapsed: Duration, length: Duration) -> f32 {
    if length.is_zero() {
        return 1.0;
    }
    let t = (elapsed.as_secs_f32() / length.as_secs_f32()).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadUniform {
    rect: [f32; 4],
    tint: [f32; 4],
}

/// GPU objects of the splash: one shader, one pipeline per surface format, three quads
/// (background, image, text).
struct SplashGpu {
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    /// Per surface format; shared with the warm job that builds one ([`BootSplash::set_warm_queue`]).
    pipelines: Arc<Mutex<HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>>>,
    /// Formats whose pipeline was queued on the warm queue.
    queued: Vec<wgpu::TextureFormat>,
    uniforms: [wgpu::Buffer; 3],
    bind_groups: [wgpu::BindGroup; 3],
    image_size: Option<(u32, u32)>,
    text_size: Option<(u32, u32)>,
}

impl SplashGpu {
    fn new(device: &RenderDevice, image: Option<&SplashImage>, text: &str) -> Self {
        let d = &device.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("boot splash"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/boot-splash.wgsl").into()),
        });
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("boot splash"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("boot splash"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let linear = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("boot splash linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let nearest = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("boot splash nearest"),
            ..Default::default()
        });
        let upload = |label: &str, w: u32, h: u32, rgba: &[u8]| {
            let texture = d.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            device.queue.write_texture(
                texture.as_image_copy(),
                rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * w),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            texture.create_view(&wgpu::TextureViewDescriptor::default())
        };
        let white = upload("boot splash white", 1, 1, &[255; 4]);
        let image_ok = image.filter(|i| {
            i.width > 0 && i.height > 0 && i.rgba.len() as u64 == u64::from(i.width) * u64::from(i.height) * 4
        });
        if image.is_some() && image_ok.is_none() {
            tracing::warn!(target: "regia_render", "boot splash image has the wrong size; drawing without it");
        }
        let image_view = image_ok.map(|i| upload("boot splash image", i.width, i.height, &i.rgba));
        let text_pixels = rasterize_text(text);
        let text_view = text_pixels
            .as_ref()
            .map(|(w, h, rgba)| upload("boot splash text", *w, *h, rgba));
        let uniforms = std::array::from_fn(|i| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(["boot splash background", "boot splash image", "boot splash text"][i]),
                size: size_of::<QuadUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        let bind = |i: usize, view: &wgpu::TextureView, sampler: &wgpu::Sampler| {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("boot splash"),
                layout: &bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniforms[i].as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        };
        let bind_groups = [
            bind(0, &white, &nearest),
            bind(1, image_view.as_ref().unwrap_or(&white), &linear),
            bind(2, text_view.as_ref().unwrap_or(&white), &nearest),
        ];
        Self {
            module,
            layout,
            pipelines: Arc::new(Mutex::new(HashMap::new())),
            queued: Vec::new(),
            uniforms,
            bind_groups,
            image_size: image_ok.map(|i| (i.width, i.height)),
            text_size: text_pixels.map(|(w, h, _)| (w, h)),
        }
    }

    /// The pipeline for `format`, or `None` while a warm job builds it (`warm`: a queue that is
    /// not shutting down; the first call for a format queues the job). Without such a queue the
    /// pipeline is built here.
    fn pipeline(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
        warm: Option<&crate::PipelineWarmQueue>,
    ) -> Option<wgpu::RenderPipeline> {
        let built = self
            .pipelines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&format)
            .cloned();
        if built.is_some() {
            return built;
        }
        if let Some(queue) = warm.filter(|q| !q.is_shutting_down()) {
            if !self.queued.contains(&format) {
                self.queued.push(format);
                let (module, layout) = (self.module.clone(), self.layout.clone());
                let slot = Arc::clone(&self.pipelines);
                queue.push(
                    crate::WarmTier::Boot,
                    "boot splash",
                    move |device: &RenderDevice| {
                        if crate::pipeline_warm::warm_cancelled() {
                            return Err(crate::pipeline_warm::CANCELLED_AT_SHUTDOWN.to_owned());
                        }
                        let pipeline = build_splash_pipeline(device, &module, &layout, format);
                        slot.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(format, pipeline);
                        Ok(())
                    },
                );
            }
            return None;
        }
        let pipeline = build_splash_pipeline(device, &self.module, &self.layout, format);
        self.pipelines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(format, pipeline.clone());
        Some(pipeline)
    }
}

/// The splash pipeline for `format` (alpha-blended quads, no depth).
fn build_splash_pipeline(
    device: &RenderDevice,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let d = &device.device;
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("boot splash"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(d).as_deref(),
    })
}

// ---------------------------------------------------------------------------------------------
// Built-in 5×7 pixel font
// ---------------------------------------------------------------------------------------------

/// Glyph rows, five bits each (bit 4 = left column), top row first.
const FONT: &[(char, [u8; 7])] = &[
    (' ', [0, 0, 0, 0, 0, 0, 0]),
    (
        'A',
        [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
    ),
    (
        'B',
        [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
    ),
    (
        'C',
        [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
    ),
    (
        'D',
        [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
    ),
    (
        'E',
        [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
    ),
    (
        'F',
        [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
    ),
    (
        'G',
        [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
    ),
    (
        'H',
        [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
    ),
    (
        'I',
        [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    ),
    (
        'J',
        [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
    ),
    (
        'K',
        [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
    ),
    (
        'L',
        [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
    ),
    (
        'M',
        [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
    ),
    (
        'N',
        [0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001],
    ),
    (
        'O',
        [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
    ),
    (
        'P',
        [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
    ),
    (
        'Q',
        [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
    ),
    (
        'R',
        [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
    ),
    (
        'S',
        [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
    ),
    (
        'T',
        [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
    ),
    (
        'U',
        [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
    ),
    (
        'V',
        [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
    ),
    (
        'W',
        [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
    ),
    (
        'X',
        [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
    ),
    (
        'Y',
        [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
    ),
    (
        'Z',
        [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
    ),
    (
        '0',
        [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    ),
    (
        '1',
        [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    ),
    (
        '2',
        [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    ),
    (
        '3',
        [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
    ),
    (
        '4',
        [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    ),
    (
        '5',
        [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    ),
    (
        '6',
        [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    ),
    (
        '7',
        [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    ),
    (
        '8',
        [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    ),
    (
        '9',
        [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
    ),
    ('.', [0, 0, 0, 0, 0, 0b01100, 0b01100]),
    (',', [0, 0, 0, 0, 0b01100, 0b00100, 0b01000]),
    (':', [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0]),
    ('-', [0, 0, 0, 0b11111, 0, 0, 0]),
    ('!', [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0b00100]),
    ('?', [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0, 0b00100]),
    ('\'', [0b00100, 0b00100, 0b01000, 0, 0, 0, 0]),
    ('/', [0, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0]),
    (
        '(',
        [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
    ),
    (
        ')',
        [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
    ),
    ('+', [0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0]),
    ('_', [0, 0, 0, 0, 0, 0, 0b11111]),
];

/// The glyph rows of `c` (`?` for characters the font lacks).
fn glyph(c: char) -> [u8; 7] {
    let upper = c.to_ascii_uppercase();
    FONT.iter()
        .find(|(g, _)| *g == upper)
        .or_else(|| FONT.iter().find(|(g, _)| *g == '?'))
        .map_or([0; 7], |(_, rows)| *rows)
}

/// Rasterises `text` (one line) to white-on-transparent RGBA8: 6 columns per glyph (5 + gap),
/// 7 rows, with a one-pixel transparent border so linear edges stay clean. `None` for empty text.
fn rasterize_text(text: &str) -> Option<(u32, u32, Vec<u8>)> {
    let chars: Vec<char> = text.chars().take(120).collect();
    if chars.iter().all(|c| c.is_whitespace()) {
        return None;
    }
    let w = chars.len() * 6 + 1;
    let h = 7 + 2;
    let mut rgba = vec![0u8; w * h * 4];
    for (i, c) in chars.iter().enumerate() {
        for (row, bits) in glyph(*c).iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) != 0 {
                    let x = 1 + i * 6 + col;
                    let y = 1 + row;
                    rgba[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[255; 4]);
                }
            }
        }
    }
    Some((w as u32, h as u32, rgba))
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "boot-splash.wgsl",
        "Quad",
        QuadUniform { rect, tint }
    )]
}

#[cfg(test)]
impl BootSplash {
    /// The queue [`Self::record`] builds the pipeline on.
    fn warm_queue(&self) -> Option<&crate::PipelineWarmQueue> {
        self.warm.queue()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fixrev-demo-warm-k (review finding 3): `set_warm_queue(None)` is documented to keep the
    /// splash pipeline inline, but the renderer's `attach_warm_queue` (every frame) put the app's
    /// queue back. A splash never set gets the app's queue; one set to `None` keeps none; one set
    /// to its own queue keeps that one.
    /// Whether `splash` builds on `queue`: a probe job pushed on the splash's queue lands there.
    fn builds_on(splash: &BootSplash, queue: &crate::PipelineWarmQueue) -> bool {
        let jobs =
            |q: &crate::PipelineWarmQueue| q.progress().tiers[crate::WarmTier::Boot as usize].jobs_total;
        let before = jobs(queue);
        if let Some(own) = splash.warm_queue() {
            own.push(crate::WarmTier::Boot, "probe", |_: &RenderDevice| Ok(()));
        }
        jobs(queue) > before
    }

    #[test]
    fn set_warm_queue_none_opts_the_splash_out_of_the_apps_queue() {
        let app_queue = crate::PipelineWarmQueue::new(1);
        let mut unset = BootSplash::new();
        unset.attach_warm_queue(Some(&app_queue));
        assert!(
            builds_on(&unset, &app_queue),
            "the renderer attaches the app's queue"
        );

        let mut off = BootSplash::new();
        off.set_warm_queue(None);
        off.attach_warm_queue(Some(&app_queue));
        off.attach_warm_queue(Some(&app_queue));
        assert!(off.warm_queue().is_none(), "None opts out, on every frame");

        let own = crate::PipelineWarmQueue::new(1);
        let mut set = BootSplash::new();
        set.set_warm_queue(Some(own.clone()));
        set.attach_warm_queue(Some(&app_queue));
        assert!(
            builds_on(&set, &own) && !builds_on(&set, &app_queue),
            "its own queue stays"
        );
        set.set_warm_queue(None);
        set.attach_warm_queue(Some(&app_queue));
        assert!(set.warm_queue().is_none(), "set to None later: off");
    }

    #[test]
    fn phases_fade_in_hold_and_fade_out() {
        let mut splash = BootSplash {
            fade_in: Duration::from_millis(100),
            fade_out: Duration::from_millis(100),
            min_duration: Duration::from_millis(300),
            // This test is about the minimum time, not the process-start cap (ADR-173).
            max_duration: None,
            ..BootSplash::new()
        };
        let t0 = Instant::now();
        assert_eq!(splash.advance(t0), SplashPhase::Showing { opacity: 0.0 });
        assert_eq!(
            splash.phase_at(t0 + Duration::from_millis(200)),
            SplashPhase::Showing { opacity: 1.0 }
        );
        // A finish before the minimum time waits for it.
        splash.finish();
        let early = t0 + Duration::from_millis(250);
        assert!(matches!(splash.phase_at(early), SplashPhase::Showing { .. }));
        let mid = splash.phase_at(t0 + Duration::from_millis(350));
        assert!(matches!(mid, SplashPhase::FadingOut { opacity } if opacity > 0.0 && opacity < 1.0));
        assert_eq!(splash.advance(t0 + Duration::from_millis(500)), SplashPhase::Done);
        assert!(splash.is_done());
    }

    #[test]
    fn a_held_splash_fades_from_the_release() {
        let mut splash = BootSplash {
            fade_in: Duration::ZERO,
            fade_out: Duration::from_secs(60),
            // This test is about the hold, not the process-start cap (ADR-173).
            max_duration: None,
            ..BootSplash::new()
        };
        let t0 = Instant::now();
        splash.advance(t0);
        splash.finish();
        // A game-frame warm holds the fade even after the finish came due.
        splash.set_held(true);
        assert!(splash.is_held());
        assert!(matches!(
            splash.phase_at(Instant::now() + Duration::from_secs(1)),
            SplashPhase::Showing { .. }
        ));
        // Released: the fade starts now, not at the (earlier) finish.
        std::thread::sleep(Duration::from_millis(20));
        splash.set_held(false);
        let at = Instant::now();
        match splash.phase_at(at) {
            SplashPhase::FadingOut { opacity } => assert!(opacity > 0.99, "fade restarted: {opacity}"),
            other => panic!("expected the fade-out, got {other:?}"),
        }
    }

    /// A splash whose cap falls `cap_in` after its first present.
    fn capped_splash(cap_in: Duration) -> BootSplash {
        BootSplash {
            fade_in: Duration::ZERO,
            fade_out: Duration::from_millis(100),
            // The process is already older than this, so the cap is set relative to it.
            max_duration: Some(since_process_start() + cap_in + Duration::from_millis(100)),
            ..BootSplash::new()
        }
    }

    #[test]
    fn the_cap_ends_a_held_splash_and_the_fade_is_over_by_then() {
        let mut splash = capped_splash(Duration::from_millis(400));
        let t0 = Instant::now();
        splash.advance(t0);
        splash.finish();
        // A game-frame warm holds the fade-out for longer than the cap.
        splash.set_held(true);
        let before = t0 + Duration::from_millis(300);
        assert!(matches!(splash.phase_at(before), SplashPhase::Showing { .. }));
        assert!(!splash.was_capped());
        // The fade starts at the cap minus the fade-out, held or not...
        let fading = t0 + Duration::from_millis(460);
        assert!(matches!(splash.advance(fading), SplashPhase::FadingOut { .. }));
        assert!(splash.was_capped());
        assert!(splash.is_held(), "the warm still runs behind the fading splash");
        // ...and the splash is gone by the cap (plus timer slack).
        assert_eq!(splash.advance(t0 + Duration::from_millis(800)), SplashPhase::Done);
        assert!(splash.is_done());
    }

    #[test]
    fn the_cap_never_fades_a_splash_the_host_has_not_finished() {
        let mut splash = capped_splash(Duration::from_millis(50));
        let t0 = Instant::now();
        splash.advance(t0);
        let late = t0 + Duration::from_secs(5);
        assert!(matches!(splash.phase_at(late), SplashPhase::Showing { .. }));
        // A finish after the cap fades at once, past the minimum time.
        splash.min_duration = Duration::from_secs(60);
        splash.finish();
        let now = Instant::now() + Duration::from_millis(50);
        assert!(matches!(splash.advance(now), SplashPhase::FadingOut { .. }));
        assert!(splash.was_capped(), "the cap cut the minimum time");
    }

    #[test]
    fn a_splash_that_finishes_early_is_not_capped() {
        let mut splash = capped_splash(Duration::from_secs(60));
        let t0 = Instant::now();
        splash.advance(t0);
        splash.finish();
        assert!(matches!(
            splash.advance(t0 + Duration::from_millis(50)),
            SplashPhase::FadingOut { .. }
        ));
        assert_eq!(splash.advance(t0 + Duration::from_millis(200)), SplashPhase::Done);
        assert!(!splash.was_capped());
        // No cap: a held splash waits.
        let mut uncapped = BootSplash::new().with_max_duration(None);
        uncapped.fade_in = Duration::ZERO;
        uncapped.advance(t0);
        uncapped.finish();
        uncapped.set_held(true);
        assert!(matches!(
            uncapped.phase_at(t0 + Duration::from_secs(3600)),
            SplashPhase::Showing { .. }
        ));
    }

    /// `BootBackground` follows the splash, the game-frame warm and the probe (ADR-173), and
    /// only changes when a field does.
    #[test]
    fn boot_background_tracks_the_splash_warm_and_probe() {
        use regia_ecs::ecs::system::RunSystemOnce;
        let mut world = World::new();
        world.init_resource::<BootBackground>();
        let mut splash = BootSplash::new().with_max_duration(None);
        splash.fade_in = Duration::ZERO;
        world.insert_resource(splash);
        let mut warm = crate::GameFrameWarm::default();
        warm.request();
        world.insert_resource(warm);
        let mut probe = crate::QualityProbe::default();
        probe.request(crate::quality::ProbeConfig::default());
        world.insert_resource(probe);
        let run = |world: &mut World| {
            if let Err(err) = world.run_system_once(track_boot_background) {
                panic!("tracker: {err}");
            }
        };
        run(&mut world);
        let bg = world.resource::<BootBackground>().clone();
        assert!(!bg.splash_done && bg.game_warm_pending && !bg.game_warm_running && bg.probe_active);
        assert!(!bg.warm_done() && !bg.probe_done() && !bg.is_idle());
        // Splash gone, warm request dropped, probe aborted and its result taken.
        world.remove_resource::<BootSplash>();
        world.insert_resource(crate::GameFrameWarm::default());
        {
            let mut probe = world.resource_mut::<crate::QualityProbe>();
            assert!(probe.abort());
            assert!(probe.take_result().is_some());
        }
        run(&mut world);
        let bg = world.resource::<BootBackground>();
        assert!(bg.splash_done && bg.warm_done() && bg.probe_done());
        assert_eq!(bg.is_idle(), bg.deferred_pending == 0);
    }

    #[test]
    fn text_rasterises_known_glyphs() {
        let (w, h, rgba) = rasterize_text("Hi 27").expect("text");
        assert_eq!((w, h), (31, 9));
        // 'H' top-left pixel is set, the gap column after it is not.
        assert_eq!(rgba[(9 + 1) * 4 + 3], 0, "border stays clear");
        assert_eq!(rgba[(w as usize + 1) * 4 + 3], 255);
        assert_eq!(rgba[(w as usize + 6) * 4 + 3], 0);
        assert!(rasterize_text("   ").is_none());
        // Every glyph is 5 columns wide.
        assert!(FONT.iter().all(|(_, rows)| rows.iter().all(|r| *r < 32)));
        assert_eq!(glyph('~'), glyph('?'));
    }

    #[test]
    fn timeline_keeps_the_first_mark() {
        mark_named("test_mark_once");
        let first = mark_ms("test_mark_once").expect("recorded");
        std::thread::sleep(Duration::from_millis(5));
        mark_named("test_mark_once");
        assert_eq!(mark_ms("test_mark_once"), Some(first));
        assert!(since_process_start().as_secs_f64() * 1000.0 >= first);
    }

    #[test]
    fn splash_draws_image_text_and_fade_on_lavapipe() {
        let device = crate::device::test_device();
        let (w, h) = (64u32, 48u32);
        let image = SplashImage {
            width: 2,
            height: 2,
            rgba: vec![255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255].into(),
        };
        let mut splash = BootSplash {
            background: [0.0, 0.0, 1.0, 1.0],
            fade_in: Duration::ZERO,
            text_scale: 0.1,
            ..BootSplash::new().with_image(image).with_text("REGIA")
        };
        let target = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("splash test target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let phase = splash.advance(Instant::now());
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        splash.record(
            &device,
            &mut encoder,
            &view,
            wgpu::TextureFormat::Rgba8Unorm,
            (w, h),
            phase,
        );
        let readback =
            crate::screenshot::encode_copy(&device, &mut encoder, &target, wgpu::TextureFormat::Rgba8Unorm);
        device.queue.submit([encoder.finish()]);
        let image = crate::screenshot::finish(&device, readback).expect("readback");
        let px = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [image.rgba[i], image.rgba[i + 1], image.rgba[i + 2]]
        };
        assert_eq!(px(1, 1), [0, 0, 255], "background");
        let centre = px(w / 2, h / 2 - 3);
        assert!(
            centre[0] > 200 && centre[2] < 50,
            "image in the center: {centre:?}"
        );
        let text_row = (0..w).any(|x| {
            let p = px(x, (h as f32 * 0.81) as u32);
            p[0] > 100 && p[1] > 100
        });
        assert!(text_row, "text drawn under the image");
    }
}
