// SPDX-License-Identifier: MPL-2.0
//! The vendor upscaler integration seam (ADR-1150): everything a DLSS or XeSS binding needs
//! from Regia Engine except the vendor's own calls.
//!
//! - [`JitterSequence`]: the Halton(2, 3) jitter with a phase count tied to the upscale ratio
//!   (8 × ratio², rounded), in NDC for the projection and in render pixels for a runtime. For
//!   frames 0 to the phase count it is bit for bit the jitter the frame uses today
//!   (`taau::halton_jitter`); after that it wraps.
//! - [`VendorRuntimeApi`] and [`VendorEvaluate`]: the calls a vendor binding implements (runtime
//!   state, Vulkan extensions, optimal settings, create, evaluate, release).
//! - [`VendorBackend`]: the feature lifecycle around those calls, implementing
//!   [`Upscaler`] for any target. It checks the frame against the vendor's contract, converts
//!   jitter and motion into the vendor's conventions, creates the feature once per display size,
//!   mode and flag set, keeps the render size inside the runtime's range, resets the history
//!   after a (re)create or a failed evaluate, and disables itself after
//!   [`MAX_CONSECUTIVE_FAILURES`] failures in a row so the frame falls back to a built-in.
//! - [`DlssSdk`] and [`XessSdk`]: the stub bindings this build ships. They report
//!   [`VendorRuntime::NotBundled`] and refuse every call; a vendor lane replaces their bodies
//!   with the SDK calls (ADR-325).
//! - [`RuntimeLibrary`]: where a vendor runtime library is looked for (next to the executable,
//!   then the vendor's environment variable; never the working directory), without loading it.
//! - [`VulkanExtensionRequest`]: the instance and device extensions a runtime asks for, checked
//!   against what device creation enabled ([`features_from_extensions`]).
//!
//! - [`dlss`]: the DLSS binding through NGX (ADR-2002), [`dlss::DlssNgx`], built with the
//!   `dlss` cargo feature and `DLSS_SDK` at build time; the device-creation hook; the frame's
//!   vendor slot with its fallback to the built-in upscalers.
//!
//! - [`PreferredRange`]: the render sizes a runtime asks for, which the frame's size selection
//!   and [`crate::DynResConfig::within_preferred`] stay inside (ADR-2679).
//!
//! No vendor code or binary is in the tree: the NGX SDK is linked from outside it at build time
//! and its runtime library is loaded at run time. Everything here runs on the CPU in tests with
//! a fake binding.

use std::path::{Path, PathBuf};

use super::plugin::{
    GpuTarget, InputContract, InputConventions, UpscaleError, UpscaleFrame, UpscaleTarget, Upscaler,
    VendorRuntime,
};
use super::{UpscaleQuality, UpscalerFeatures, VendorUpscaler, reason};

/// Phases of a [`JitterSequence`] at ratio 1 (native): the base the ratio² scales.
pub const JITTER_BASE_PHASES: u32 = 8;

/// Most phases a [`JitterSequence`] has, whatever the ratio (a custom render scale far below
/// Ultra Performance).
pub const JITTER_MAX_PHASES: u32 = 1024;

/// The Halton(2, 3) projection jitter of one upscaled view, with a phase count tied to the
/// upscale ratio.
///
/// A runtime accumulates one sample per frame per render pixel; at ratio r each render pixel
/// covers r² display pixels, so a cycle of `8 × r²` phases puts about 8 samples in each of them
/// before the pattern repeats (AMD FidelityFX Super Resolution 2 README, "Camera jitter"; the
/// NVIDIA DLSS programming guide's jitter section describes a rule of the same form, to be
/// confirmed against the fetched guide, ADR-1150). The count is rounded to the nearest
/// integer from the exact sizes, on the axis with the larger ratio, and kept within
/// [`JITTER_BASE_PHASES`]..=[`JITTER_MAX_PHASES`]: 8, 18, 23, 32 and 72 for the five
/// [`UpscaleQuality`] modes.
///
/// This sequence wraps. The frame's projection does not: it calls `taau::halton_jitter` on the
/// unbounded index since the last camera cut. The two match for frames 1 through
/// [`Self::phase_count`] only. FSR reads the count to pace lock decay
/// (`fsr3::jitter_phase_count`) and does not wrap the projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterSequence {
    phase_count: u32,
}

impl JitterSequence {
    /// The sequence for a `render` → `display` upscale (zero sizes read as 1).
    #[must_use]
    pub fn for_sizes(render: (u32, u32), display: (u32, u32)) -> Self {
        // round(8 × (display ÷ render)²) in integers; u128 so no axis size overflows.
        let axis = |render: u32, display: u32| -> u128 {
            let r2 = u128::from(render.max(1)).pow(2);
            let d2 = u128::from(display.max(1)).pow(2);
            (u128::from(JITTER_BASE_PHASES) * d2 + r2 / 2) / r2
        };
        let phases = axis(render.0, display.0)
            .max(axis(render.1, display.1))
            .clamp(u128::from(JITTER_BASE_PHASES), u128::from(JITTER_MAX_PHASES));
        Self {
            phase_count: u32::try_from(phases).unwrap_or(JITTER_MAX_PHASES),
        }
    }

    /// The sequence for a quality mode at a display size (render size by
    /// [`UpscaleQuality::render_size`]).
    #[must_use]
    pub fn for_quality(quality: UpscaleQuality, display: (u32, u32)) -> Self {
        Self::for_sizes(quality.render_size(display), display)
    }

    /// Phases before the pattern repeats.
    #[must_use]
    pub const fn phase_count(self) -> u32 {
        self.phase_count
    }

    /// The 1-based Halton index of a frame in this sequence: frames 0 and 1 use index 1 (as
    /// `taau::halton_jitter` does), frame `phase_count` uses `phase_count`, and the
    /// next frame of the sequence starts again at 1. The frame's projection keeps counting.
    #[must_use]
    pub const fn halton_index(self, frame: u64) -> u32 {
        let from_zero = frame.saturating_sub(1);
        (from_zero % self.phase_count as u64) as u32 + 1
    }

    /// The jitter of `frame` in NDC for a render target of `render`, as added to the projection
    /// (`taau::jitter_projection`): exactly `taau::halton_jitter` at
    /// [`Self::halton_index`].
    #[must_use]
    pub fn ndc(self, frame: u64, render: (u32, u32)) -> [f32; 2] {
        crate::taau::halton_jitter(u64::from(self.halton_index(frame)), render.0, render.1)
    }

    /// The same jitter in render pixels, y down, as the offset the projection was moved by
    /// ([`InputConventions::RENDER_PIXELS`]): each axis in (−½, ½).
    #[must_use]
    pub fn render_pixels(self, frame: u64) -> [f32; 2] {
        let n = self.halton_index(frame);
        [
            crate::taau::halton(n, 2) - 0.5,
            -(crate::taau::halton(n, 3) - 0.5),
        ]
    }
}

/// How the colour, depth and motion inputs are encoded: the creation flags a vendor feature is
/// built with. A change recreates the feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureFlags {
    /// The colour input is scene-referred HDR (not tonemapped).
    pub hdr: bool,
    /// Depth is reverse-Z (1 at the near plane).
    pub depth_inverted: bool,
    /// Motion vectors are at the render size (not dilated to the display size).
    pub motion_vectors_render_size: bool,
    /// Motion vectors include the jitter difference between the two frames.
    pub motion_vectors_jittered: bool,
    /// The runtime computes its own exposure (otherwise [`VendorEvalParams::exposure`] is used).
    pub auto_exposure: bool,
}

impl FeatureFlags {
    /// What Regia Engine's frame hands a backend: HDR colour, standard depth (0 at the near plane,
    /// 1 at the far plane; ADR-2679 corrected the seam's old claim of reverse-Z), render-size motion
    /// vectors without jitter (the velocity pass uses the unjittered matrices of both frames),
    /// and the frame's own exposure.
    pub const REGIA: Self = Self {
        hdr: true,
        depth_inverted: false,
        motion_vectors_render_size: true,
        motion_vectors_jittered: false,
        auto_exposure: false,
    };
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self::REGIA
    }
}

/// What a vendor feature is created for. Any change recreates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureDesc {
    /// Output size.
    pub display_size: (u32, u32),
    /// Quality mode.
    pub quality: UpscaleQuality,
    /// Input encoding.
    pub flags: FeatureFlags,
}

/// A runtime's answer to "what should I render at for this display and mode".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptimalSettings {
    /// The render size the runtime recommends.
    pub render_size: (u32, u32),
    /// The smallest render size the feature accepts (dynamic resolution's floor).
    pub render_min: (u32, u32),
    /// The largest render size the feature accepts.
    pub render_max: (u32, u32),
    /// The runtime's recommended sharpness, 0–1 (0 when it has none).
    pub sharpness: f32,
}

impl OptimalSettings {
    /// ADR-324's rule, for a binding whose runtime has no size query: the mode's render size,
    /// down to the next lower mode's (the dynamic-resolution range of
    /// [`UpscaleQuality::permille_range`]).
    #[must_use]
    pub const fn from_rule(display: (u32, u32), quality: UpscaleQuality) -> Self {
        Self {
            render_size: quality.render_size(display),
            render_min: quality.next_lower().render_size(display),
            render_max: quality.render_size(display),
            sharpness: 0.0,
        }
    }

    /// `render` is within [`Self::render_min`]..=[`Self::render_max`] on both axes.
    #[must_use]
    pub const fn accepts(&self, render: (u32, u32)) -> bool {
        render.0 >= self.render_min.0
            && render.1 >= self.render_min.1
            && render.0 <= self.render_max.0
            && render.1 <= self.render_max.1
    }

    /// The answer is usable for `display`: a non-zero render size, `render_min <= render_size <=
    /// render_max` on both axes, and nothing larger than the display (an upscaler never renders
    /// above its output). A runtime answer that fails this (a corrupt or mismatched runtime) is
    /// refused before a feature is created at its sizes (audit wave 6, upscalers M4).
    #[must_use]
    pub const fn is_sane_for(&self, display: (u32, u32)) -> bool {
        self.render_size.0 > 0
            && self.render_size.1 > 0
            && self.accepts(self.render_size)
            && self.render_max.0 <= display.0
            && self.render_max.1 <= display.1
    }
}

/// The render sizes a vendor runtime asks for at a display size and mode (ADR-2679): the answer
/// of [`VendorRuntimeApi::optimal_settings`] as the frame and dynamic resolution use it. A runtime
/// with no dynamic range answers `min == max`, a static range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreferredRange {
    /// The size the runtime recommends.
    pub preferred: (u32, u32),
    /// The smallest size it accepts.
    pub min: (u32, u32),
    /// The largest size it accepts.
    pub max: (u32, u32),
}

impl PreferredRange {
    /// The range in a runtime's answer.
    #[must_use]
    pub const fn from_settings(settings: &OptimalSettings) -> Self {
        Self {
            preferred: settings.render_size,
            min: settings.render_min,
            max: settings.render_max,
        }
    }

    /// The runtime accepts exactly one size per axis: dynamic resolution has nothing to move.
    #[must_use]
    pub fn is_static(&self) -> bool {
        self.min == self.max
    }

    /// `size` moved inside the range per axis, at least 1.
    #[must_use]
    pub fn clamp(&self, size: (u32, u32)) -> (u32, u32) {
        let axis = |value: u32, min: u32, max: u32| value.min(max).max(min).max(1);
        (
            axis(size.0, self.min.0, self.max.0),
            axis(size.1, self.min.1, self.max.1),
        )
    }

    /// The frame's render size under this range. Under a `mode`, its own permille renders exactly
    /// [`Self::preferred`] and another permille scales it by `permille ÷ mode permille` (rounded,
    /// halves up), so a dynamic-resolution step never jumps. With no mode, the size the frame
    /// `requested` from its own scale. Always inside the range.
    #[must_use]
    pub fn frame_extent(
        &self,
        requested: (u32, u32),
        permille: u32,
        mode: Option<UpscaleQuality>,
    ) -> (u32, u32) {
        let Some(mode) = mode else {
            return self.clamp(requested);
        };
        let anchor = u64::from(mode.permille().max(1));
        let axis = |preferred: u32| -> u32 {
            let scaled = (u64::from(preferred) * u64::from(permille) * 2 + anchor) / (2 * anchor);
            u32::try_from(scaled).unwrap_or(u32::MAX)
        };
        self.clamp((axis(self.preferred.0), axis(self.preferred.1)))
    }

    /// The permille range dynamic resolution may move in so that [`Self::frame_extent`] covers
    /// the sizes this range accepts, `(min, max)`, never above native. Under a `mode` it is
    /// relative to the mode's permille, which is always inside it; with no mode, a share of
    /// `display`. Rounded inward on the axis that binds, so no value leaves the range on any
    /// axis. An aspect ratio that leaves no such value collapses to the top.
    #[must_use]
    pub fn permille_range(&self, display: (u32, u32), mode: Option<UpscaleQuality>) -> (u32, u32) {
        let (anchor, base) = match mode {
            Some(mode) => (u64::from(mode.permille()), self.preferred),
            None => (1000, display),
        };
        let bound = |size: u32, base: u32, up: bool| -> u64 {
            let base = u64::from(base.max(1));
            let scaled = u64::from(size) * anchor;
            if up { scaled.div_ceil(base) } else { scaled / base }
        };
        let low = bound(self.min.0, base.0, true).max(bound(self.min.1, base.1, true));
        let high = bound(self.max.0, base.0, false).min(bound(self.max.1, base.1, false));
        let high = u32::try_from(high.min(1000)).unwrap_or(1000);
        let low = u32::try_from(low).unwrap_or(high).min(high);
        (low, high)
    }
}

/// A live feature inside a vendor runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeatureHandle(pub u64);

/// Per-frame values a vendor evaluate receives, already in the vendor's conventions (its
/// [`InputContract::conventions`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VendorEvalParams {
    /// This frame's jitter in the vendor's units and sign.
    pub jitter: [f32; 2],
    /// Factors the runtime multiplies Regia Engine's motion texture by to get its own units and
    /// direction ([`InputConventions::motion_scale_from_regia`]); the texture is not rewritten.
    pub motion_scale: [f32; 2],
    /// The render size of this frame (within the feature's range).
    pub render_size: (u32, u32),
    /// Drop the history: the frame asked for it, or the feature is new, or the last evaluate
    /// failed.
    pub reset: bool,
    /// Exposure multiplier (linear HDR → display-referred).
    pub exposure: f32,
    /// The history's pre-exposure relative to this frame's (1 = unchanged).
    pub history_scale: f32,
    /// Frame time in milliseconds.
    pub frame_time_ms: f32,
    /// Post-upscale sharpening, 0–1.
    pub sharpness: f32,
}

/// The target-independent calls of a vendor runtime binding.
pub trait VendorRuntimeApi {
    /// Which vendor upscaler this binds.
    fn vendor(&self) -> VendorUpscaler;

    /// The runtime's state in this process ([`VendorRuntime::Ready`] once it loaded and
    /// reports the feature usable).
    fn runtime(&self) -> VendorRuntime;

    /// The vendor's input contract. [`InputContract::VENDOR`] unless the SDK says otherwise
    /// (a different jitter sign or motion direction is one value here, ADR-510 (3)).
    fn contract(&self) -> InputContract {
        InputContract::VENDOR
    }

    /// The Vulkan instance and device extensions the runtime needs at device creation.
    ///
    /// # Errors
    ///
    /// The runtime is not loaded or cannot answer.
    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError>;

    /// The render size and range the runtime wants for a display size and mode.
    ///
    /// # Errors
    ///
    /// The runtime is not loaded, or does not support the mode at that size.
    fn optimal_settings(
        &self,
        display: (u32, u32),
        quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError>;

    /// Frees a feature. Called once per created feature: on recreation, on
    /// [`VendorBackend::release`], after the last of [`MAX_CONSECUTIVE_FAILURES`] and on drop.
    fn release_feature(&mut self, feature: FeatureHandle);
}

/// The calls of a vendor runtime binding that record work on a target.
pub trait VendorEvaluate<T: UpscaleTarget>: VendorRuntimeApi {
    /// Creates a feature (the runtimes record its creation on a command buffer).
    ///
    /// # Errors
    ///
    /// The runtime refused or failed.
    fn create_feature(
        &mut self,
        ctx: &mut T::Context<'_>,
        desc: &FeatureDesc,
        settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError>;

    /// Records one upscale of `frame` (Regia Engine's images, already checked against
    /// [`VendorRuntimeApi::contract`]) into the context's output, with `eval` in the vendor's
    /// conventions.
    ///
    /// # Errors
    ///
    /// The runtime failed.
    fn evaluate(
        &mut self,
        feature: FeatureHandle,
        ctx: &mut T::Context<'_>,
        frame: &UpscaleFrame<'_, T::Image>,
        eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError>;
}

/// Failures in a row (create or evaluate) after which a [`VendorBackend`] disables itself and
/// the frame stays on its fallback until [`VendorBackend::reenable`].
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Slack on the half-pixel jitter bound (render pixels), for float rounding.
const JITTER_TOLERANCE_PX: f32 = 1.0e-3;

/// Counters of a [`VendorBackend`], for tests and leak checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VendorStats {
    /// Features created.
    pub features_created: u64,
    /// Features released.
    pub features_released: u64,
    /// Successful evaluates.
    pub evaluations: u64,
    /// Failed creates and evaluates (all time).
    pub failures: u64,
}

impl VendorStats {
    /// Features alive now (0 or 1).
    #[must_use]
    pub const fn live_features(&self) -> u64 {
        self.features_created.saturating_sub(self.features_released)
    }
}

#[derive(Debug, Clone, Copy)]
struct LiveFeature {
    handle: FeatureHandle,
    desc: FeatureDesc,
    settings: OptimalSettings,
    needs_reset: bool,
}

/// A vendor runtime binding behind the [`Upscaler`] boundary: the feature lifecycle Regia Engine
/// owns, around the calls the binding `S` makes.
#[derive(Debug)]
pub struct VendorBackend<S: VendorRuntimeApi> {
    sdk: S,
    quality: UpscaleQuality,
    flags: FeatureFlags,
    live: Option<LiveFeature>,
    stats: VendorStats,
    consecutive_failures: u32,
    disabled: bool,
}

impl<S: VendorRuntimeApi> VendorBackend<S> {
    /// A backend over `sdk` at `quality`, with Regia Engine's input flags. No feature exists until
    /// the first frame.
    #[must_use]
    pub fn new(sdk: S, quality: UpscaleQuality) -> Self {
        Self {
            sdk,
            quality,
            flags: FeatureFlags::REGIA,
            live: None,
            stats: VendorStats::default(),
            consecutive_failures: 0,
            disabled: false,
        }
    }

    /// The binding.
    #[must_use]
    pub const fn sdk(&self) -> &S {
        &self.sdk
    }

    /// The binding, mutably (a host reloading the runtime, a test injecting failures).
    pub fn sdk_mut(&mut self) -> &mut S {
        &mut self.sdk
    }

    /// The quality mode the next feature is created for.
    #[must_use]
    pub const fn quality(&self) -> UpscaleQuality {
        self.quality
    }

    /// Sets the quality mode; the next frame recreates the feature if it changed.
    pub fn set_quality(&mut self, quality: UpscaleQuality) {
        self.quality = quality;
    }

    /// The input flags the next feature is created with.
    #[must_use]
    pub const fn flags(&self) -> FeatureFlags {
        self.flags
    }

    /// Sets the input flags; the next frame recreates the feature if they changed.
    pub fn set_flags(&mut self, flags: FeatureFlags) {
        self.flags = flags;
    }

    /// The counters.
    #[must_use]
    pub const fn stats(&self) -> VendorStats {
        self.stats
    }

    /// The runtime's state ([`VendorRuntimeApi::runtime`]).
    #[must_use]
    pub fn runtime(&self) -> VendorRuntime {
        self.sdk.runtime()
    }

    /// The live feature's description, if there is one.
    #[must_use]
    pub fn live_feature(&self) -> Option<FeatureDesc> {
        self.live.map(|live| live.desc)
    }

    /// The sizes the runtime asks for at a display size and mode, `None` while the backend cannot
    /// run or the runtime's answer is not usable ([`OptimalSettings::is_sane_for`], ADR-2679).
    #[must_use]
    pub fn preferred_range(&self, display: (u32, u32), quality: UpscaleQuality) -> Option<PreferredRange> {
        if self.disabled || self.sdk.runtime().blocker().is_some() {
            return None;
        }
        self.sdk
            .optimal_settings(display, quality)
            .ok()
            .filter(|settings| settings.is_sane_for(display))
            .map(|settings| PreferredRange::from_settings(&settings))
    }

    /// The live feature's settings, if there is one.
    #[must_use]
    pub fn live_settings(&self) -> Option<OptimalSettings> {
        self.live.map(|live| live.settings)
    }

    /// The backend gave up after [`MAX_CONSECUTIVE_FAILURES`] failures in a row.
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Clears the failure count and the disabled state (after a device or runtime reload).
    pub fn reenable(&mut self) {
        self.disabled = false;
        self.consecutive_failures = 0;
    }

    /// Frees the live feature, if any (device loss, suspend, renderer teardown). The next frame
    /// creates a new one.
    pub fn release(&mut self) {
        if let Some(live) = self.live.take() {
            self.sdk.release_feature(live.handle);
            self.stats.features_released += 1;
        }
    }

    /// Counts a create or evaluate failure and disables the backend at the limit.
    fn fail(&mut self, error: UpscaleError) -> UpscaleError {
        self.consecutive_failures += 1;
        self.stats.failures += 1;
        if let Some(live) = self.live.as_mut() {
            live.needs_reset = true;
        }
        if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            tracing::warn!(
                vendor = self.sdk.vendor().name(),
                failures = self.consecutive_failures,
                %error,
                "vendor upscaler disabled; the frame stays on its fallback"
            );
            self.disabled = true;
            self.release();
        }
        error
    }
}

impl<S: VendorRuntimeApi> Drop for VendorBackend<S> {
    fn drop(&mut self) {
        self.release();
    }
}

impl<T: UpscaleTarget, S: VendorEvaluate<T>> Upscaler<T> for VendorBackend<S> {
    fn name(&self) -> &'static str {
        self.sdk.vendor().name()
    }

    fn contract(&self) -> InputContract {
        self.sdk.contract()
    }

    fn render_size(&self, display: (u32, u32), quality: UpscaleQuality) -> (u32, u32) {
        self.sdk
            .optimal_settings(display, quality)
            .ok()
            .filter(|settings| settings.is_sane_for(display))
            .map_or_else(|| quality.render_size(display), |settings| settings.render_size)
    }

    fn preferred_range(&self, display: (u32, u32), quality: UpscaleQuality) -> Option<PreferredRange> {
        VendorBackend::preferred_range(self, display, quality)
    }

    fn upscale(
        &mut self,
        ctx: &mut T::Context<'_>,
        frame: &UpscaleFrame<'_, T::Image>,
    ) -> Result<(), UpscaleError> {
        let _span = tracing::info_span!("vendor_upscale", vendor = self.sdk.vendor().name()).entered();
        if self.disabled {
            return Err(UpscaleError::Disabled {
                failures: self.consecutive_failures,
            });
        }
        if let Some(reason) = self.sdk.runtime().blocker() {
            return Err(UpscaleError::RuntimeUnavailable { reason });
        }
        let contract = self.sdk.contract();
        frame.check(&contract)?;
        let p = frame.params;
        // Regia Engine's jitter is NDC; more than half a render pixel means something passed pixels
        // (or garbage) as NDC, which a runtime would turn into ghosting, not an error.
        let px = InputConventions::RENDER_PIXELS.jitter_from_regia(p.jitter, p.render_size);
        if !px
            .iter()
            .all(|axis| axis.is_finite() && axis.abs() <= 0.5 + JITTER_TOLERANCE_PX)
        {
            return Err(UpscaleError::InvalidJitter {
                render: p.render_size,
            });
        }
        let desc = FeatureDesc {
            display_size: p.display_size,
            quality: self.quality,
            flags: self.flags,
        };
        let mut live = match self.live.take() {
            Some(live) if live.desc == desc => live,
            stale => {
                if let Some(old) = stale {
                    self.sdk.release_feature(old.handle);
                    self.stats.features_released += 1;
                }
                let settings = match self.sdk.optimal_settings(p.display_size, self.quality) {
                    Ok(settings) if settings.is_sane_for(p.display_size) => settings,
                    Ok(settings) => {
                        return Err(self.fail(UpscaleError::Backend(format!(
                            "the runtime's sizes for display {:?} are not usable: render {:?}, range {:?}..={:?}",
                            p.display_size, settings.render_size, settings.render_min, settings.render_max
                        ))));
                    }
                    Err(error) => return Err(self.fail(error)),
                };
                let handle = match self.sdk.create_feature(ctx, &desc, &settings) {
                    Ok(handle) => handle,
                    Err(error) => return Err(self.fail(error)),
                };
                self.stats.features_created += 1;
                LiveFeature {
                    handle,
                    desc,
                    settings,
                    needs_reset: true,
                }
            }
        };
        if !live.settings.accepts(p.render_size) {
            let error = UpscaleError::RenderSizeOutOfRange {
                render: p.render_size,
                min: live.settings.render_min,
                max: live.settings.render_max,
            };
            self.live = Some(live);
            return Err(error);
        }
        let conventions = contract.conventions;
        let eval = VendorEvalParams {
            jitter: conventions.jitter_from_regia(p.jitter, p.render_size),
            motion_scale: conventions.motion_scale_from_regia(p.render_size),
            render_size: p.render_size,
            reset: p.reset || live.needs_reset,
            exposure: p.exposure,
            history_scale: if p.history_scale > 0.0 {
                p.history_scale
            } else {
                1.0
            },
            frame_time_ms: p.frame_time_ms,
            sharpness: p.sharpness,
        };
        let result = self.sdk.evaluate(live.handle, ctx, frame, &eval);
        if result.is_ok() {
            live.needs_reset = false;
        }
        self.live = Some(live);
        match result {
            Ok(()) => {
                self.consecutive_failures = 0;
                self.stats.evaluations += 1;
                Ok(())
            }
            Err(error) => Err(self.fail(error)),
        }
    }
}

/// The error every call of a stub binding returns.
const fn not_bundled() -> UpscaleError {
    UpscaleError::RuntimeUnavailable {
        reason: reason::SDK_NOT_BUNDLED,
    }
}

/// The DLSS Super Resolution binding of this build: a stub. No NVIDIA SDK code or library is in
/// the tree, so it reports [`VendorRuntime::NotBundled`] and refuses every call.
///
/// A vendor lane replaces the bodies with the NGX calls ADR-325 lists (init on the raw
/// Vulkan or DX12 device through wgpu-hal, the optimal-settings query, feature creation and
/// evaluation on the upscale interval's command buffer, release), after confirming the jitter
/// sign and the motion-vector direction against the fetched SDK.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DlssSdk;

impl VendorRuntimeApi for DlssSdk {
    fn vendor(&self) -> VendorUpscaler {
        VendorUpscaler::Dlss
    }

    fn runtime(&self) -> VendorRuntime {
        VendorRuntime::NotBundled
    }

    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        Err(not_bundled())
    }

    fn optimal_settings(
        &self,
        _display: (u32, u32),
        _quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        Err(not_bundled())
    }

    fn release_feature(&mut self, _feature: FeatureHandle) {}
}

impl VendorEvaluate<GpuTarget> for DlssSdk {
    fn create_feature(
        &mut self,
        _ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        _desc: &FeatureDesc,
        _settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        Err(not_bundled())
    }

    fn evaluate(
        &mut self,
        _feature: FeatureHandle,
        _ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        _frame: &UpscaleFrame<'_, wgpu::TextureView>,
        _eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        Err(not_bundled())
    }
}

/// The XeSS Super Resolution binding of this build: a stub, like [`DlssSdk`]. A vendor lane
/// replaces the bodies with the XeSS calls ADR-325 lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XessSdk;

impl VendorRuntimeApi for XessSdk {
    fn vendor(&self) -> VendorUpscaler {
        VendorUpscaler::Xess
    }

    fn runtime(&self) -> VendorRuntime {
        VendorRuntime::NotBundled
    }

    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        Err(not_bundled())
    }

    fn optimal_settings(
        &self,
        _display: (u32, u32),
        _quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        Err(not_bundled())
    }

    fn release_feature(&mut self, _feature: FeatureHandle) {}
}

impl VendorEvaluate<GpuTarget> for XessSdk {
    fn create_feature(
        &mut self,
        _ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        _desc: &FeatureDesc,
        _settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        Err(not_bundled())
    }

    fn evaluate(
        &mut self,
        _feature: FeatureHandle,
        _ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        _frame: &UpscaleFrame<'_, wgpu::TextureView>,
        _eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        Err(not_bundled())
    }
}

/// DLSS behind the boundary with this build's stub binding.
pub type DlssUpscaler = VendorBackend<DlssSdk>;

/// XeSS behind the boundary with this build's stub binding.
pub type XessUpscaler = VendorBackend<XessSdk>;

/// The operating system a runtime library is named for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryOs {
    /// Windows (`.dll`).
    Windows,
    /// Linux (`.so`, often with a version suffix).
    Linux,
    /// Android, macOS and the rest: no vendor runtime exists there.
    Other,
}

impl LibraryOs {
    /// The OS this build runs on.
    pub const CURRENT: Self = if cfg!(target_os = "windows") {
        Self::Windows
    } else if cfg!(target_os = "linux") {
        Self::Linux
    } else {
        Self::Other
    };
}

/// One file name a runtime library may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryName {
    /// Exactly this name.
    Exact(&'static str),
    /// This name, or this name followed by a numeric version (`name.1`, `name.310.4.0`); the
    /// highest version wins.
    Versioned(&'static str),
}

impl LibraryName {
    /// The version key of `file_name` if it is this library (empty for the bare name), or
    /// `None`.
    #[must_use]
    pub fn version_of(self, file_name: &str) -> Option<Vec<u32>> {
        match self {
            Self::Exact(name) => (file_name == name).then(Vec::new),
            Self::Versioned(stem) => {
                let rest = file_name.strip_prefix(stem)?;
                if rest.is_empty() {
                    return Some(Vec::new());
                }
                let version = rest.strip_prefix('.')?;
                version
                    .split('.')
                    .map(|part| {
                        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                            None
                        } else {
                            part.parse::<u32>().ok()
                        }
                    })
                    .collect()
            }
        }
    }
}

/// Where a vendor runtime library is looked for (ADR-325 (7)): next to the executable, then in
/// the directory the vendor's environment variable names (absolute paths only). Never the
/// working directory, so a file dropped there is not loaded.
///
/// The file names are the redistributables the public integration guides name; a vendor lane
/// confirms them against the fetched SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeLibrary {
    /// The vendor.
    pub vendor: VendorUpscaler,
    /// The environment variable naming an extra directory.
    pub env_var: &'static str,
    /// The names the library may have on this OS (empty: no runtime for this OS).
    pub names: &'static [LibraryName],
}

/// What [`RuntimeLibrary::locate`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeSearch {
    /// The library, if one was found.
    pub found: Option<PathBuf>,
    /// The directories looked in, in order.
    pub searched: Vec<PathBuf>,
    /// The environment variable's directory, when it was refused for being relative (it would
    /// resolve against the working directory).
    pub refused_relative: Option<PathBuf>,
}

impl RuntimeSearch {
    /// The runtime state the search alone decides: [`VendorRuntime::NotBundled`] when the
    /// build has no binding for the vendor, [`VendorRuntime::NotFound`] when nothing was found,
    /// `None` when a library was found and the binding must load it to know more.
    #[must_use]
    pub const fn state(&self, binding_built: bool) -> Option<VendorRuntime> {
        if !binding_built {
            Some(VendorRuntime::NotBundled)
        } else if self.found.is_none() {
            Some(VendorRuntime::NotFound)
        } else {
            None
        }
    }
}

impl RuntimeLibrary {
    /// The library of `vendor` on `os`.
    #[must_use]
    pub const fn of(vendor: VendorUpscaler, os: LibraryOs) -> Self {
        const DLSS_WINDOWS: &[LibraryName] = &[LibraryName::Exact("nvngx_dlss.dll")];
        const DLSS_LINUX: &[LibraryName] = &[LibraryName::Versioned("libnvidia-ngx-dlss.so")];
        const XESS_WINDOWS: &[LibraryName] = &[LibraryName::Exact("libxess.dll")];
        const NONE: &[LibraryName] = &[];
        let (env_var, names) = match (vendor, os) {
            (VendorUpscaler::Dlss, LibraryOs::Windows) => ("REGIA_DLSS_PATH", DLSS_WINDOWS),
            (VendorUpscaler::Dlss, LibraryOs::Linux) => ("REGIA_DLSS_PATH", DLSS_LINUX),
            (VendorUpscaler::Dlss, LibraryOs::Other) => ("REGIA_DLSS_PATH", NONE),
            (VendorUpscaler::Xess, LibraryOs::Windows) => ("REGIA_XESS_PATH", XESS_WINDOWS),
            // No Linux XeSS runtime is known (ADR-325 (8)); a vendor lane adds it if one exists.
            (VendorUpscaler::Xess, LibraryOs::Linux | LibraryOs::Other) => ("REGIA_XESS_PATH", NONE),
        };
        Self {
            vendor,
            env_var,
            names,
        }
    }

    /// The library of `vendor` on the OS this build runs on.
    #[must_use]
    pub const fn current(vendor: VendorUpscaler) -> Self {
        Self::of(vendor, LibraryOs::CURRENT)
    }

    /// Looks for the library in `exe_dir`, then in `env_dir` (refused when relative). Reads
    /// directory entries only; loads nothing.
    #[must_use]
    pub fn locate(&self, exe_dir: Option<&Path>, env_dir: Option<&Path>) -> RuntimeSearch {
        let mut search = RuntimeSearch::default();
        let mut dirs: Vec<&Path> = exe_dir.into_iter().collect();
        if let Some(dir) = env_dir {
            if dir.is_absolute() {
                dirs.push(dir);
            } else {
                search.refused_relative = Some(dir.to_path_buf());
            }
        }
        for dir in dirs {
            search.searched.push(dir.to_path_buf());
            if let Some(path) = self.find_in(dir) {
                search.found = Some(path);
                break;
            }
        }
        search
    }

    /// [`Self::locate`] for this process: the running executable's directory and the
    /// environment variable.
    #[must_use]
    pub fn locate_for_this_process(&self) -> RuntimeSearch {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let env_dir = regia_core::brand::env_var_os(self.env_var).map(PathBuf::from);
        self.locate(exe_dir.as_deref(), env_dir.as_deref())
    }

    fn find_in(&self, dir: &Path) -> Option<PathBuf> {
        for name in self.names {
            match name {
                LibraryName::Exact(file) => {
                    let path = dir.join(file);
                    if path.is_file() {
                        return Some(path);
                    }
                }
                LibraryName::Versioned(_) => {
                    let Ok(entries) = std::fs::read_dir(dir) else {
                        continue;
                    };
                    let best = entries
                        .filter_map(Result::ok)
                        .filter_map(|entry| {
                            let file_name = entry.file_name().into_string().ok()?;
                            let version = name.version_of(&file_name)?;
                            let path = entry.path();
                            path.is_file().then_some((version, path))
                        })
                        .max();
                    if let Some((_, path)) = best {
                        return Some(path);
                    }
                }
            }
        }
        None
    }
}

/// The Vulkan extensions a vendor runtime asks for at device creation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VulkanExtensionRequest {
    /// Instance extensions.
    pub instance: Vec<String>,
    /// Device extensions.
    pub device: Vec<String>,
}

impl VulkanExtensionRequest {
    /// The requested extensions that are not enabled, instance ones first, in request order.
    #[must_use]
    pub fn missing(&self, enabled_instance: &[&str], enabled_device: &[&str]) -> Vec<String> {
        let instance = self
            .instance
            .iter()
            .filter(|name| !enabled_instance.contains(&name.as_str()));
        let device = self
            .device
            .iter()
            .filter(|name| !enabled_device.contains(&name.as_str()));
        instance.chain(device).cloned().collect()
    }

    /// Every requested extension is enabled.
    #[must_use]
    pub fn is_satisfied(&self, enabled_instance: &[&str], enabled_device: &[&str]) -> bool {
        self.missing(enabled_instance, enabled_device).is_empty()
    }

    /// Both requests together, each name once, in first-seen order: what a device-creation hook
    /// serving several runtimes asks for.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        fn merge(a: &[String], b: &[String]) -> Vec<String> {
            let mut out: Vec<String> = Vec::with_capacity(a.len() + b.len());
            for name in a.iter().chain(b) {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
            out
        }
        Self {
            instance: merge(&self.instance, &other.instance),
            device: merge(&self.device, &other.device),
        }
    }
}

/// [`UpscalerFeatures`] from what each runtime asked for (`None`: it did not answer, so its
/// extensions count as missing) and what device creation enabled.
#[must_use]
pub fn features_from_extensions(
    dlss: Option<&VulkanExtensionRequest>,
    xess: Option<&VulkanExtensionRequest>,
    enabled_instance: &[&str],
    enabled_device: &[&str],
) -> UpscalerFeatures {
    let satisfied = |request: Option<&VulkanExtensionRequest>| {
        request.is_some_and(|request| request.is_satisfied(enabled_instance, enabled_device))
    };
    UpscalerFeatures {
        dlss_vulkan_extensions: satisfied(dlss),
        xess_vulkan_extensions: satisfied(xess),
    }
}

pub mod dlss;
#[doc(hidden)]
pub mod scripted;

#[cfg(test)]
mod tests;

/// Intel XeSS Super Resolution binding (ADR-4656).
pub mod xess;
