// SPDX-License-Identifier: MPL-2.0
//! FSR 3 frame generation (ADR-514, ADR-2067, `book/agents/frame-gen.md`). Off by default.
//!
//! Frame generation presents one interpolated frame between every two rendered ones. It is a port
//! of the FidelityFX SDK v1.1.4 frame interpolation and optical flow shaders (MIT, the same tag as
//! the FSR 3.1 upscaler of ADR-113) to WGSL, with Regia Engine's own pacing on its wgpu surfaces:
//! the SDK's replacement swap chain cannot sit under wgpu, and no vendor binary is used.
//!
//! With the cargo feature `frame-gen`, `passes` holds the GPU passes and `present` what the window's
//! frame needs beside them (the path itself is `surface::frame_gen_present`). Everything else here
//! is always compiled and can be checked without a GPU:
//!
//! - [`inputs`]: the per-frame dispatch description in Regia Engine's conventions (motion vectors,
//!   depth, camera, colour transfer function, HUD composition) and its validation.
//! - [`optical_flow`]: the flow input stage (luma, field size, scene-change detection) and a CPU
//!   reference of the block-matching estimator, the oracle for the WGSL port.
//! - [`pacing`]: the frame pacer that times the generated and the real present.
//! - [`reference`](mod@reference): a CPU reference of motion-vector interpolation at a time `t`, the oracle that
//!   fixes the motion-vector sign and the half-vector convention.
//!
//! Without the feature nothing runs per frame and [`FrameGenStatus`] reports `not_compiled`. With
//! it and frame generation off, one `Instant` read per tick feeds the pacer, so the agent can see
//! whether the base frame rate would allow it.

pub mod inputs;
pub mod optical_flow;
pub mod pacing;
#[cfg(feature = "frame-gen")]
pub mod passes;
#[cfg(feature = "frame-gen")]
pub mod present;
pub mod reference;
pub(crate) mod wgsl;

use regia_app::prelude::*;

#[cfg(feature = "frame-gen")]
pub use crate::frame::FrameGenInputs;
pub use inputs::{
    DepthConvention, FrameGenCamera, FrameGenDispatch, FrameGenInputError, HudComposition,
    MotionVectorConvention, TransferFunction,
};
pub use pacing::{FramePacer, PacingConfig, PacingDecision, PacingSkip, PacingStats};

/// Whether this build carries the frame-generation hooks (the cargo feature `frame-gen`).
pub const COMPILED: bool = cfg!(feature = "frame-gen");

/// Whether a frame interpolator exists in this build: the WGSL port of the SDK's frame
/// interpolation and optical flow passes (ADR-2067).
pub const INTERPOLATOR_PORTED: bool = true;

/// Lowest accepted [`FrameGenSettings::min_base_fps`].
pub const MIN_BASE_FPS_FLOOR: f32 = 10.0;
/// Highest accepted [`FrameGenSettings::min_base_fps`].
pub const MIN_BASE_FPS_CEIL: f32 = 240.0;

/// Storage textures one shader stage must allow for the interpolation passes.
pub const MIN_STORAGE_TEXTURES: u32 = 4;

/// Storage buffers one shader stage must allow for the interpolation passes: the flow and
/// interpolate bind group layouts each put five on `ShaderStages::COMPUTE`.
pub const MIN_STORAGE_BUFFERS: u32 = 5;

/// Whether a device can run the interpolator: compute with at least [`MIN_STORAGE_TEXTURES`]
/// storage textures and [`MIN_STORAGE_BUFFERS`] storage buffers per stage, and not the GL
/// backend (the FSR 3.1 upscaler's tier). Both limits are asked for because the Vulkan 1.0/1.1
/// minimum is 4 storage buffers per stage — below the layouts' five — and a device that only
/// passes the storage-texture check fails at pipeline creation instead of reporting the
/// `needs_desktop_gpu` blocker (audit-g-render3-platform).
#[must_use]
pub fn desktop_gpu(
    backend: wgpu::Backend,
    max_storage_textures_per_shader_stage: u32,
    max_storage_buffers_per_shader_stage: u32,
) -> bool {
    backend != wgpu::Backend::Gl
        && max_storage_textures_per_shader_stage >= MIN_STORAGE_TEXTURES
        && max_storage_buffers_per_shader_stage >= MIN_STORAGE_BUFFERS
}

/// The player's and the agent's frame generation choices. A resource: `register` installs the
/// default (off).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct FrameGenSettings {
    /// Present an interpolated frame between every two rendered frames. It takes effect only
    /// while [`FrameGenStatus::blockers`] is empty; anything else presents plainly.
    pub enabled: bool,
    /// How the HUD is kept out of the interpolation.
    pub hud: HudComposition,
    /// Below this real frame rate frame generation pauses (the pacer presents real frames only).
    /// Interpolation from a low base rate shows its errors for longer and adds more latency.
    pub min_base_fps: f32,
    /// Draw moving bars at the image edges of generated frames (the SDK's tear-line debug view).
    pub debug_tear_lines: bool,
    /// Present only generated frames with the interpolator's internal surfaces drawn on them.
    pub debug_view: bool,
}

impl Default for FrameGenSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            hud: HudComposition::Redraw,
            // AMD's public guidance asks for about 60 fps before interpolation; 55 leaves room
            // for the pacer's moving average to dip under load without toggling every second.
            min_base_fps: 55.0,
            debug_tear_lines: false,
            debug_view: false,
        }
    }
}

/// Why frame generation cannot run. The codes are part of the agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameGenBlocker {
    /// The build lacks the cargo feature `frame-gen`.
    NotCompiled,
    /// The WGSL interpolation passes are not ported yet.
    InterpolatorNotPorted,
    /// The app renders headless: there is no surface to present to.
    Headless,
    /// The device lacks compute with storage textures and buffers (the mobile and GL tiers).
    NeedsDesktopGpu,
}

impl FrameGenBlocker {
    /// The protocol code (lowercase ASCII and `_`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotCompiled => "not_compiled",
            Self::InterpolatorNotPorted => "interpolator_not_ported",
            Self::Headless => "headless",
            Self::NeedsDesktopGpu => "needs_desktop_gpu",
        }
    }
}

/// Where frame generation stands in this process.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameGenStatus {
    /// The build carries the hooks ([`COMPILED`]).
    pub compiled: bool,
    /// Everything that keeps frame generation off, most fundamental first. Empty = it can run.
    pub blockers: Vec<FrameGenBlocker>,
    /// Frame generation is producing frames now: enabled, unblocked, and the pacer's last
    /// decision presented a generated frame.
    pub active: bool,
}

impl FrameGenStatus {
    /// Whether [`FrameGenSettings::enabled`] may be set.
    #[must_use]
    pub fn can_enable(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// What the status depends on, gathered by the caller (pure, so the agent and tests share it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameGenEnvironment {
    /// [`COMPILED`] in a real build; tests pass both values.
    pub compiled: bool,
    /// [`INTERPOLATOR_PORTED`] in a real build.
    pub interpolator: bool,
    /// The app has no window surfaces.
    pub headless: bool,
    /// The device has compute with storage textures and buffers (`None` before a device exists).
    pub desktop_gpu: Option<bool>,
}

impl FrameGenEnvironment {
    /// This build's constants with the given app facts.
    #[must_use]
    pub const fn current(headless: bool, desktop_gpu: Option<bool>) -> Self {
        Self {
            compiled: COMPILED,
            interpolator: INTERPOLATOR_PORTED,
            headless,
            desktop_gpu,
        }
    }
}

/// The status for `settings` in `env`, given the pacer's last decision (if any).
#[must_use]
pub fn frame_gen_status(
    settings: &FrameGenSettings,
    env: FrameGenEnvironment,
    last: Option<&PacingDecision>,
) -> FrameGenStatus {
    let mut blockers = Vec::new();
    if !env.compiled {
        blockers.push(FrameGenBlocker::NotCompiled);
    }
    if !env.interpolator {
        blockers.push(FrameGenBlocker::InterpolatorNotPorted);
    }
    if env.headless {
        blockers.push(FrameGenBlocker::Headless);
    }
    if env.desktop_gpu == Some(false) {
        blockers.push(FrameGenBlocker::NeedsDesktopGpu);
    }
    let presenting = matches!(last, Some(PacingDecision::Generate { .. }));
    let active = settings.enabled && blockers.is_empty() && presenting;
    FrameGenStatus {
        compiled: env.compiled,
        blockers,
        active,
    }
}

/// The FSR 3.1 upscaler's surfaces that frame generation reads (`FrameRenderer::frame_gen_surfaces`).
/// They match what the SDK's frame interpolation takes from its upscaler: dilated depth, dilated
/// motion vectors and the reconstructed previous depth, at the render size.
#[cfg(feature = "frame-gen")]
#[derive(Debug, Clone, Copy)]
pub struct UpscalerSurfaces<'a> {
    /// Size of the two surfaces below (the scene's render size).
    pub render_size: (u32, u32),
    /// The upscaler's output size (the display size frame generation interpolates at).
    pub display_size: (u32, u32),
    /// `Rgba32Float`: `xy` = dilated motion in UV units, previous − current
    /// ([`MotionVectorConvention::REGIA`]); `z` = the nearest device depth of the 3 × 3
    /// neighbourhood (standard depth, 0 near and 1 far, [`DepthConvention::REGIA`]). A nearer
    /// texel stores a smaller `z`.
    pub dilated_motion_depth: &'a wgpu::TextureView,
    /// One `u32` per render texel, row-major: the bits of `1 − device depth` of the previous
    /// frame's surface reprojected to this texel, kept with `atomicMax` (0 = nothing landed).
    pub reconstructed_previous_depth: &'a wgpu::Buffer,
    /// Frames the upscaler has resolved; a value that did not change since the last frame means
    /// the surfaces are stale (the upscaler did not run).
    pub resolves: u64,
}

/// Why a tick presented plainly although frame generation is enabled. The codes are part of the
/// agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameGenFallback {
    /// A [`FrameGenBlocker`] applies.
    Blocked,
    /// More than one window is open: frame generation runs on one.
    MultipleWindows,
    /// The window shows more than one view, or one that does not fill it.
    MultipleViews,
    /// The boot splash is up.
    Splash,
    /// There is no scene to interpolate.
    NoScene,
    /// An error stopped frame generation; turning it off and on again retries.
    Failed,
}

impl FrameGenFallback {
    /// The protocol code (lowercase ASCII and `_`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::MultipleWindows => "multiple_windows",
            Self::MultipleViews => "multiple_views",
            Self::Splash => "splash",
            Self::NoScene => "no_scene",
            Self::Failed => "failed",
        }
    }
}

/// How evenly the present path's images went out: the time between consecutive presents, since
/// frame generation last started. Gaps over 250 ms (a hitch or a pause) do not count.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PresentGaps {
    /// Gaps measured.
    pub count: u64,
    /// Their mean (milliseconds).
    pub mean_ms: f64,
    /// Their standard deviation (milliseconds).
    pub stddev_ms: f64,
    /// The longest (milliseconds).
    pub max_ms: f64,
}

impl PresentGaps {
    /// Adds one gap (Welford's running variance).
    pub fn push(&mut self, gap_ms: f64) {
        self.count += 1;
        let previous_mean = self.mean_ms;
        self.mean_ms += (gap_ms - previous_mean) / self.count as f64;
        // `stddev_ms` holds the sum of squares until `finished` turns it into a deviation.
        self.stddev_ms += (gap_ms - previous_mean) * (gap_ms - self.mean_ms);
        self.max_ms = self.max_ms.max(gap_ms);
    }

    /// The published form: [`Self::stddev_ms`] as a deviation.
    #[must_use]
    pub fn finished(self) -> Self {
        Self {
            stddev_ms: if self.count > 0 {
                (self.stddev_ms / self.count as f64).sqrt()
            } else {
                0.0
            },
            ..self
        }
    }
}

/// What the present path did since start.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameGenPresentStats {
    /// Generated images presented.
    pub generated_presents: u64,
    /// Rendered images presented through the frame generation path.
    pub real_presents: u64,
    /// Frames that started a new history (camera cuts, resizes, first frames).
    pub resets: u64,
    /// Swap-chain images that could not be acquired on this path.
    pub acquire_failures: u64,
    /// Why this tick presented plainly, with frame generation enabled.
    pub fallback: Option<FrameGenFallback>,
    /// The error behind [`FrameGenFallback::Failed`].
    pub last_error: Option<String>,
    /// The spacing of the presents.
    pub gaps: PresentGaps,
}

/// The pacer's state: fed with real frame times, and its last decision. `register` installs it
/// when the feature is on and the app has surfaces.
#[derive(Resource, Debug, Default)]
pub struct FrameGenPacing {
    /// The pacer. It runs while frame generation is off too, so the agent can report whether
    /// the base frame rate would allow it.
    pub pacer: FramePacer,
    /// The last decision ([`FramePacer::on_real_frame`]).
    pub last: Option<PacingDecision>,
    /// What the present path did.
    pub present: FrameGenPresentStats,
    /// The present path already accounted for this tick: it fed the frame it presented, or the
    /// tick showed no image. Set only once a present was queued (or nothing was shown), so a
    /// tick that failed and presented plainly is fed by the plain observer (ADR-8930).
    pub fed_this_tick: bool,
    /// The clock's time origin.
    epoch: Option<std::time::Instant>,
}

impl FrameGenPacing {
    /// Feeds one real frame that finished at `ready_ns` (nanoseconds on the caller's clock).
    pub fn observe(
        &mut self,
        ready_ns: u64,
        reset: bool,
        enabled: bool,
        min_base_fps: f32,
    ) -> PacingDecision {
        self.pacer.config.min_base_fps = min_base_fps;
        let decision = self.pacer.on_real_frame(ready_ns, reset, enabled);
        self.last = Some(decision);
        decision
    }

    /// The clock's time origin.
    pub fn epoch(&mut self) -> std::time::Instant {
        *self.epoch.get_or_insert_with(std::time::Instant::now)
    }

    /// Nanoseconds since the first call (the clock of the pacer and the present worker).
    pub fn now_ns(&mut self) -> u64 {
        let epoch = self.epoch();
        u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// Installs [`FrameGenSettings`] and, with the feature `frame-gen` on a windowed app, the pacer,
/// the present resources and the observer of plain ticks (after the window present, in `Render`).
pub(crate) fn register(app: &mut App, headless: bool) {
    app.init_resource::<FrameGenSettings>();
    if COMPILED && !headless {
        app.init_resource::<FrameGenPacing>();
        #[cfg(feature = "frame-gen")]
        app.init_resource::<present::FrameGenPresenter>()
            .add_systems(Render, observe_presents.after(crate::surface::render_windows));
    }
}

/// `Render`, after the present: a tick the plain path presented is one real frame the pacer has not
/// seen. It generated nothing, so it counts as disabled whatever the settings say.
#[cfg(feature = "frame-gen")]
fn observe_presents(settings: Res<FrameGenSettings>, mut pacing: ResMut<FrameGenPacing>) {
    if std::mem::take(&mut pacing.fed_this_tick) {
        return;
    }
    let now = pacing.now_ns();
    pacing.observe(now, false, false, settings.min_base_fps);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(compiled: bool, interpolator: bool) -> FrameGenEnvironment {
        FrameGenEnvironment {
            compiled,
            interpolator,
            headless: false,
            desktop_gpu: Some(true),
        }
    }

    #[test]
    fn this_build_can_enable_exactly_when_the_feature_is_compiled() {
        let status = frame_gen_status(
            &FrameGenSettings::default(),
            FrameGenEnvironment::current(false, Some(true)),
            None,
        );
        assert_eq!(status.compiled, COMPILED);
        assert!(!status.blockers.contains(&FrameGenBlocker::InterpolatorNotPorted));
        assert_eq!(status.can_enable(), COMPILED);
        assert!(!status.active);
    }

    #[test]
    fn present_gaps_keep_a_running_mean_and_deviation() {
        let mut gaps = PresentGaps::default();
        for gap in [10.0, 10.0, 16.0, 12.0] {
            gaps.push(gap);
        }
        let done = gaps.finished();
        assert_eq!(done.count, 4);
        assert!((done.mean_ms - 12.0).abs() < 1.0e-9);
        // Variance of 10, 10, 16, 12 around 12: (4 + 4 + 16 + 0) / 4 = 6.
        assert!((done.stddev_ms - 6f64.sqrt()).abs() < 1.0e-9);
        assert_eq!(done.max_ms, 16.0);
        assert_eq!(PresentGaps::default().finished().stddev_ms, 0.0);
    }

    #[test]
    fn fallback_codes_are_protocol_safe() {
        for fallback in [
            FrameGenFallback::Blocked,
            FrameGenFallback::MultipleWindows,
            FrameGenFallback::MultipleViews,
            FrameGenFallback::Splash,
            FrameGenFallback::NoScene,
            FrameGenFallback::Failed,
        ] {
            assert!(
                fallback
                    .code()
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c == b'_'),
                "{}",
                fallback.code()
            );
        }
    }

    #[test]
    fn blockers_are_listed_most_fundamental_first() {
        let status = frame_gen_status(
            &FrameGenSettings::default(),
            FrameGenEnvironment {
                compiled: false,
                interpolator: false,
                headless: true,
                desktop_gpu: Some(false),
            },
            None,
        );
        let codes: Vec<_> = status.blockers.iter().map(|b| b.code()).collect();
        assert_eq!(
            codes,
            [
                "not_compiled",
                "interpolator_not_ported",
                "headless",
                "needs_desktop_gpu"
            ]
        );
        for code in codes {
            assert!(
                code.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'),
                "{code}"
            );
        }
        // No device yet is not a blocker: the device decides later.
        let status = frame_gen_status(
            &FrameGenSettings::default(),
            FrameGenEnvironment {
                desktop_gpu: None,
                ..env(true, true)
            },
            None,
        );
        assert!(status.can_enable());
    }

    #[test]
    fn active_needs_enabled_unblocked_and_a_generated_present() {
        let generate = PacingDecision::Generate {
            generated_at_ns: 10,
            real_at_ns: 20,
            interval_ns: 10,
        };
        let real_only = PacingDecision::RealOnly {
            present_at_ns: 10,
            reason: PacingSkip::Disabled,
        };
        let on = FrameGenSettings {
            enabled: true,
            ..FrameGenSettings::default()
        };
        assert!(frame_gen_status(&on, env(true, true), Some(&generate)).active);
        assert!(!frame_gen_status(&on, env(true, true), Some(&real_only)).active);
        assert!(!frame_gen_status(&on, env(true, true), None).active);
        assert!(!frame_gen_status(&on, env(true, false), Some(&generate)).active);
        assert!(!frame_gen_status(&FrameGenSettings::default(), env(true, true), Some(&generate)).active);
    }

    #[test]
    fn gl_and_storage_starved_devices_are_not_desktop_gpus() {
        assert!(desktop_gpu(wgpu::Backend::Vulkan, 8, 8));
        assert!(desktop_gpu(wgpu::Backend::Dx12, 64, 64));
        assert!(desktop_gpu(wgpu::Backend::Metal, 8, 8));
        assert!(!desktop_gpu(wgpu::Backend::Gl, 8, 8));
        assert!(!desktop_gpu(wgpu::Backend::Vulkan, 2, 8));
    }

    /// audit-g-render3-platform: the flow and interpolate bind group layouts each put five
    /// storage buffers on `ShaderStages::COMPUTE`. A Vulkan device at the 1.0/1.1 minimum
    /// (`max_storage_buffers_per_shader_stage == 4`, reachable through the Mobile min-spec
    /// clamp) reports enough storage textures for the old check, then fails the pipeline
    /// build and turns frame generation off with a shader error. The capability check must
    /// ask for the buffers too, so such a device keeps the `needs_desktop_gpu` blocker.
    #[test]
    fn five_storage_buffers_per_stage_are_part_of_desktop_too() {
        // 4 storage textures, 4 storage buffers (the Vulkan minimum): not desktop-class.
        assert!(!desktop_gpu(wgpu::Backend::Vulkan, 4, 4));
        // 4 storage textures but 5 storage buffers: the layouts fit.
        assert!(desktop_gpu(wgpu::Backend::Vulkan, 4, 5));
        assert!(desktop_gpu(wgpu::Backend::Vulkan, 8, 8));
        // GL stays out whatever the limits say.
        assert!(!desktop_gpu(wgpu::Backend::Gl, 8, 8));
    }

    #[test]
    fn defaults_are_off_and_in_range() {
        let settings = FrameGenSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.hud, HudComposition::Redraw);
        assert!((MIN_BASE_FPS_FLOOR..=MIN_BASE_FPS_CEIL).contains(&settings.min_base_fps));
    }

    #[test]
    fn the_observer_feeds_the_pacer_with_the_settings_floor() {
        let mut pacing = FrameGenPacing::default();
        // 40 fps frames against a 30 fps floor: generated after the warm-up.
        let step = 25_000_000;
        let mut last = None;
        for i in 0..6 {
            last = Some(pacing.observe(i * step, false, true, 30.0));
        }
        assert!(matches!(last, Some(PacingDecision::Generate { .. })), "{last:?}");
        // The same frames against a 55 fps floor pause generation.
        let decision = pacing.observe(6 * step, false, true, 55.0);
        assert_eq!(
            decision,
            PacingDecision::RealOnly {
                present_at_ns: 6 * step,
                reason: PacingSkip::BaseRateTooLow
            }
        );
        assert_eq!(pacing.last, Some(decision));
    }
}
