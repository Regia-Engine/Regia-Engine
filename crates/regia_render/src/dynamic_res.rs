// SPDX-License-Identifier: MPL-2.0
//! Dynamic internal resolution. A pure controller: it returns a render-scale permille and
//! writes nothing.
//!
//! DO27 saw the scale fall from 1000 to 500 permille over a few minutes while the game stayed
//! near 15 fps. The frame was CPU-bound (about 31 ms of GPU time inside a 66 ms wall frame), and
//! a budget check on GPU time alone kept stepping the scale down. This controller steps down
//! only when the rolling median GPU time is over the budget and the current frame is
//! [`FrameBound::GpuBound`]. A CPU-bound or balanced frame does not step down. It steps up when
//! the median is under the budget's lower band, including while CPU-bound, so a scale that was
//! lowered earlier recovers once the GPU work actually drops.
//!
//! # Hold
//!
//! Menu, loading and pause frames are cheap, so fed to the controller they step the scale up to
//! native, and play steps it down again (DO27's oscillation). [`DynResController::hold`] stops
//! that: while held, [`DynResController::update`] returns the current permille and records
//! nothing. [`DynResController::resume`] clears the rolling window, so no stale sample from before
//! the hold moves the scale; the scale moves again once a full window of play frames has been
//! accepted. Hold whenever the frame is not play (`regia_app::PacingState`, ADR-7889).
//!
//! Apply the returned permille to the frame about to be drawn
//! ([`crate::FrameQuality::render_scale_permille`]). Leave `RenderSettings` and the settings
//! file alone on each step: writing `render_scale` every frame fsyncs on the main thread. The
//! Android thermal governor is a separate policy and is not consulted here.
//!
//! # Clocks
//!
//! `gpu_ms` is the GPU's busy time for the frame, [`crate::GpuTimings::frame_ms`] (timestamp
//! queries; the latest sample is two frames late, ADR-027). `frame_ms` is the wall interval
//! between presented frames, the interval the performance overlay graphs.
//! [`crate::FrameCpuTimings::total_ms`] covers `render_into` only, so pass the present interval
//! as `frame_ms`. A non-finite or non-positive sample is ignored, which keeps a missing
//! timestamp (`0`) from moving the scale.
//!
//! # Telemetry
//!
//! Read [`DynResController::permille`], [`DynResController::bound`] and
//! [`DynResController::median_gpu_ms`] once per frame. This module does not emit a telemetry
//! event. A later row can use `dynres_permille` (`u32`), `dynres_bound`
//! ([`FrameBound::label`]) and `dynres_median_gpu_ms` (`f32`, milliseconds).
//!
//! The control law is Regia Engine's (ADR-256). Provenance: `docs/provenance.md`.

use std::collections::VecDeque;

use regia_app::prelude::*;

/// GPU share of the wall frame strictly below which [`classify_bound`] returns
/// [`FrameBound::CpuBound`].
pub const CPU_BOUND_GPU_SHARE: f32 = 0.60;

/// GPU share of the wall frame strictly above which [`classify_bound`] returns
/// [`FrameBound::GpuBound`].
pub const GPU_BOUND_GPU_SHARE: f32 = 0.90;

/// Largest rolling window. A larger `window_frames` is capped so a bad config cannot allocate
/// without a bound.
const MAX_WINDOW_FRAMES: u32 = 4096;

/// Which clock owns the frame, from GPU time divided by wall time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameBound {
    /// GPU time is under [`CPU_BOUND_GPU_SHARE`] of the wall frame. The CPU, or a wait, owns it.
    CpuBound,
    /// GPU time is over [`GPU_BOUND_GPU_SHARE`] of the wall frame.
    GpuBound,
    /// GPU time is between the two shares, or the sample is non-finite or non-positive.
    #[default]
    Balanced,
}

impl FrameBound {
    /// Stable telemetry label: `"cpu"`, `"gpu"` or `"balanced"`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CpuBound => "cpu",
            Self::GpuBound => "gpu",
            Self::Balanced => "balanced",
        }
    }
}

/// Classifies one frame. GPU time under 60% of the wall time is [`FrameBound::CpuBound`], over
/// 90% is [`FrameBound::GpuBound`], and the closed band between them is [`FrameBound::Balanced`].
/// A non-finite or non-positive input is [`FrameBound::Balanced`].
#[must_use]
pub fn classify_bound(gpu_ms: f32, frame_ms: f32) -> FrameBound {
    if !plausible_ms(gpu_ms) || !plausible_ms(frame_ms) {
        return FrameBound::Balanced;
    }
    let share = gpu_ms / frame_ms;
    if share < CPU_BOUND_GPU_SHARE {
        FrameBound::CpuBound
    } else if share > GPU_BOUND_GPU_SHARE {
        FrameBound::GpuBound
    } else {
        FrameBound::Balanced
    }
}

/// Knobs for [`DynResController`]. Permille is thousandths of native resolution: `1000` is
/// native, `500` is half.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynResConfig {
    /// Target GPU time per frame, in milliseconds. The scale steps around this, not around the
    /// wall-clock frame time.
    pub budget_ms: f32,
    /// Lowest permille the controller will return.
    pub min_permille: u32,
    /// Highest permille the controller will return.
    pub max_permille: u32,
    /// Permille removed in one downward step.
    pub step_down_permille: u32,
    /// Permille added in one upward step.
    pub step_up_permille: u32,
    /// Accepted GPU samples kept for the rolling median. The scale does not move until this
    /// many samples have been accepted.
    pub window_frames: u32,
    /// Half-width of the deadband, as a fraction of [`Self::budget_ms`]. The scale steps down
    /// above `budget * (1 + hysteresis)` and up below `budget * (1 - hysteresis)`.
    pub hysteresis: f32,
    /// Accepted samples that must pass after a change before the scale may change again.
    /// `0` allows a change on every accepted sample once the window is full.
    pub cooldown_frames: u32,
}

impl Default for DynResConfig {
    fn default() -> Self {
        Self {
            budget_ms: 16.6,
            min_permille: 500,
            max_permille: 1000,
            step_down_permille: 50,
            step_up_permille: 25,
            window_frames: 30,
            hysteresis: 0.1,
            cooldown_frames: 60,
        }
    }
}

impl DynResConfig {
    /// A copy the controller can run.
    ///
    /// `min_permille` greater than `max_permille` is clamped down to `max_permille`, so the
    /// range collapses to that one value. A non-finite or non-positive `budget_ms` becomes the
    /// default 16.6 ms. `hysteresis` is clamped to `0.0..=0.99` (a non-finite value becomes the
    /// default 0.1), which keeps the lower band positive. `window_frames` of 0 becomes 1, and a
    /// window above 4096 is capped.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let budget_ms = if self.budget_ms.is_finite() && self.budget_ms > 0.0 {
            self.budget_ms
        } else {
            Self::default().budget_ms
        };
        let hysteresis = if self.hysteresis.is_finite() {
            self.hysteresis.clamp(0.0, 0.99)
        } else {
            Self::default().hysteresis
        };
        let max_permille = self.max_permille;
        let min_permille = self.min_permille.min(max_permille);
        Self {
            budget_ms,
            min_permille,
            max_permille,
            step_down_permille: self.step_down_permille,
            step_up_permille: self.step_up_permille,
            window_frames: self.window_frames.clamp(1, MAX_WINDOW_FRAMES),
            hysteresis,
            cooldown_frames: self.cooldown_frames,
        }
    }

    /// This config with its range replaced by an upscale quality mode's
    /// ([`crate::UpscaleQuality::permille_range`], ADR-324): from the mode's own scale down to
    /// the next mode's (Quality 588–667, Performance 333–500; Ultra Performance holds 333).
    /// Under a mode, dynamic resolution moves inside that range and the top is the mode's exact
    /// render size ([`crate::FrameQuality::scaled_extent`]). Budget, steps, window, hysteresis
    /// and cooldown are kept.
    #[must_use]
    pub fn within_quality(self, quality: crate::UpscaleQuality) -> Self {
        let (min_permille, max_permille) = quality.permille_range();
        Self {
            min_permille,
            max_permille,
            ..self
        }
    }

    /// This config with its range replaced by what a vendor runtime accepts
    /// ([`crate::upscaler::vendor::PreferredRange::permille_range`], ADR-2679): under `mode`, the
    /// mode's own permille is the runtime's preferred size and the range is relative to it; with
    /// no mode, permille is a share of `display`. A static range (`min == max`) holds one value.
    /// Budget, steps, window, hysteresis and cooldown are kept.
    #[must_use]
    pub fn within_preferred(
        self,
        range: &crate::upscaler::vendor::PreferredRange,
        display: (u32, u32),
        mode: Option<crate::UpscaleQuality>,
    ) -> Self {
        let (min_permille, max_permille) = range.permille_range(display, mode);
        Self {
            min_permille,
            max_permille,
            ..self
        }
    }
}

/// Rolling-median dynamic resolution. Holds its permille in memory only.
///
/// Construct one, call [`Self::update`] once per presented frame, and use the returned permille
/// for that frame. The same inputs always produce the same permille trace.
#[derive(Clone, Debug)]
#[must_use = "call update once per frame and apply the returned permille"]
pub struct DynResController {
    config: DynResConfig,
    permille: u32,
    /// Accepted `gpu_ms` samples, oldest at the front.
    samples: VecDeque<f32>,
    /// Accepted samples since the last permille change. Starts at `cooldown_frames`, so the
    /// first step waits only for the window to fill.
    since_change: u32,
    bound: FrameBound,
    median_gpu_ms: f32,
    /// [`Self::hold`] is in force: [`Self::update`] records nothing.
    held: bool,
}

impl DynResController {
    /// A controller at `start_permille`, clamped into the sanitized config's range.
    pub fn new(config: DynResConfig, start_permille: u32) -> Self {
        let config = config.sanitized();
        let permille = clamp_permille(start_permille, config.min_permille, config.max_permille);
        let capacity = window_len(config.window_frames);
        Self {
            config,
            permille,
            samples: VecDeque::with_capacity(capacity),
            since_change: config.cooldown_frames,
            bound: FrameBound::Balanced,
            median_gpu_ms: 0.0,
            held: false,
        }
    }

    /// Freezes the scale: until [`Self::resume`], [`Self::update`] returns the current permille
    /// and records nothing (no sample, no cooldown progress, no bound). Call it when the frames
    /// stop being play (a menu, a loading page, a paused scene). Holding twice is harmless.
    pub fn hold(&mut self) {
        self.held = true;
    }

    /// Ends a [`Self::hold`] and clears the rolling window, so the scale moves again only after
    /// [`DynResConfig::window_frames`] new samples. The permille and the cooldown count are kept.
    /// Resuming a controller that is not held does nothing.
    pub fn resume(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        self.samples.clear();
        self.median_gpu_ms = 0.0;
    }

    /// [`Self::hold`] when `held`, else [`Self::resume`]: for a caller that derives the hold from
    /// a state each frame.
    pub fn set_held(&mut self, held: bool) {
        if held {
            self.hold();
        } else {
            self.resume();
        }
    }

    /// True while [`Self::hold`] is in force.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        self.held
    }

    /// The sanitized config this controller is running.
    #[must_use]
    pub const fn config(&self) -> DynResConfig {
        self.config
    }

    /// Permille to draw with right now.
    #[must_use]
    pub const fn permille(&self) -> u32 {
        self.permille
    }

    /// Classification of the latest accepted sample. [`FrameBound::Balanced`] until one arrives.
    /// An ignored sample leaves this unchanged.
    #[must_use]
    pub const fn bound(&self) -> FrameBound {
        self.bound
    }

    /// Rolling median of accepted GPU times, in milliseconds. `0` until the first accepted
    /// sample (a real sample is positive, so `0` means "no sample yet"). An even count averages
    /// the two central values.
    #[must_use]
    pub const fn median_gpu_ms(&self) -> f32 {
        self.median_gpu_ms
    }

    /// Accepts one frame and returns the permille to use for the frame about to be drawn.
    ///
    /// A non-finite or non-positive `gpu_ms` or `frame_ms` is ignored: the window, the cooldown,
    /// the bound and the permille all stay as they were. Otherwise the sample joins the rolling
    /// median. The scale steps only once [`DynResConfig::window_frames`] samples have been
    /// accepted and [`DynResConfig::cooldown_frames`] accepted samples have passed since the
    /// last change. Down requires a [`FrameBound::GpuBound`] frame whose median is strictly over
    /// `budget_ms * (1 + hysteresis)`. Up requires a median strictly under
    /// `budget_ms * (1 - hysteresis)` and does not look at the bound. The result is clamped to
    /// `[min_permille, max_permille]`.
    ///
    /// While [`Self::hold`] is in force every sample is ignored the same way.
    #[must_use]
    pub fn update(&mut self, gpu_ms: f32, frame_ms: f32) -> u32 {
        if self.held || !plausible_ms(gpu_ms) || !plausible_ms(frame_ms) {
            return self.permille;
        }
        self.bound = classify_bound(gpu_ms, frame_ms);
        self.push_sample(gpu_ms);
        self.median_gpu_ms = median_ms(&self.samples);
        let full = self.samples.len() >= window_len(self.config.window_frames);
        if full && self.since_change >= self.config.cooldown_frames {
            let next = self.target_permille();
            if next != self.permille {
                self.permille = next;
                self.since_change = 0;
                return self.permille;
            }
        }
        self.since_change = self.since_change.saturating_add(1);
        self.permille
    }

    fn push_sample(&mut self, gpu_ms: f32) {
        let window = window_len(self.config.window_frames);
        if self.samples.len() >= window {
            self.samples.pop_front();
        }
        self.samples.push_back(gpu_ms);
    }

    fn target_permille(&self) -> u32 {
        let budget = self.config.budget_ms;
        let upper = budget * (1.0 + self.config.hysteresis);
        let lower = budget * (1.0 - self.config.hysteresis);
        let current = self.permille;
        let stepped = if self.median_gpu_ms > upper && self.bound == FrameBound::GpuBound {
            current.saturating_sub(self.config.step_down_permille)
        } else if self.median_gpu_ms < lower {
            current.saturating_add(self.config.step_up_permille)
        } else {
            current
        };
        clamp_permille(stepped, self.config.min_permille, self.config.max_permille)
    }
}

/// The app's dynamic-resolution governor (ADR-7889): a [`DynResController`] kept as a resource,
/// so the agent protocol can read it (`render.dynres`), that holds itself outside play.
///
/// The host inserts one and calls [`Self::feed`] once per presented frame with the current
/// `regia_app::PacingState`; with [`Self::hold_outside_play`] (the default) a menu, loading or
/// paused frame holds the controller and the first play frame resumes it. The engine does not
/// feed it yet: wiring it into the renderer is ADR-256's remaining step.
#[derive(Resource, Clone, Debug)]
pub struct DynResGovernor {
    /// The controller; hold it by hand (with [`Self::hold_outside_play`] off) for other stretches.
    pub controller: DynResController,
    /// Hold while the pacing state is not [`regia_app::PacingState::Play`]. Default `true`.
    pub hold_outside_play: bool,
}

impl DynResGovernor {
    /// A governor around `controller` that holds outside play.
    pub fn new(controller: DynResController) -> Self {
        Self {
            controller,
            hold_outside_play: true,
        }
    }

    /// One presented frame in `state`: holds or resumes the controller as
    /// [`Self::hold_outside_play`] says, then [`DynResController::update`]. Returns the permille
    /// for the frame about to be drawn.
    #[must_use]
    pub fn feed(&mut self, state: regia_app::PacingState, gpu_ms: f32, frame_ms: f32) -> u32 {
        if self.hold_outside_play {
            self.controller.set_held(state != regia_app::PacingState::Play);
        }
        self.controller.update(gpu_ms, frame_ms)
    }
}

/// Finite and strictly positive. Zero is a missing timestamp, not a fast frame.
fn plausible_ms(ms: f32) -> bool {
    ms.is_finite() && ms > 0.0
}

fn window_len(window_frames: u32) -> usize {
    usize::try_from(window_frames).unwrap_or(usize::MAX).max(1)
}

fn clamp_permille(value: u32, min_permille: u32, max_permille: u32) -> u32 {
    let (lo, hi) = if min_permille <= max_permille {
        (min_permille, max_permille)
    } else {
        (max_permille, min_permille)
    };
    value.clamp(lo, hi)
}

/// Median of a non-empty window. Even counts average the two central order statistics.
/// Empty is `0`.
fn median_ms(samples: &VecDeque<f32>) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let mut ordered: Vec<f32> = samples.iter().copied().collect();
    ordered.sort_by(f32::total_cmp);
    let mid = n / 2;
    if n % 2 == 1 {
        ordered.get(mid).copied().unwrap_or(0.0)
    } else {
        let left = ordered.get(mid.saturating_sub(1)).copied().unwrap_or(0.0);
        let right = ordered.get(mid).copied().unwrap_or(0.0);
        // Halve first: the sum of two large finite samples would overflow to infinity.
        left * 0.5 + right * 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_even_median_of_huge_samples_stays_finite() {
        let samples: VecDeque<f32> = [2.0e38, 2.0e38, 3.0e38, 3.0e38].into_iter().collect();
        let median = median_ms(&samples);
        assert!(median.is_finite() && (median - 2.5e38).abs() < 1.0e32, "{median}");
    }

    /// GPU-bound overload: 40/42 is about 95% GPU, and 40 ms is over the 16.6 ms budget.
    const GPU_OVER_MS: f32 = 40.0;
    const FRAME_OVER_MS: f32 = 42.0;
    /// The owner's CPU-bound scene: 31/66 is about 47% GPU, over the budget, under 60%.
    const GPU_CPU_MS: f32 = 31.0;
    const FRAME_CPU_MS: f32 = 66.0;

    fn changes_of(samples: &[u32]) -> Vec<u32> {
        let mut out = Vec::new();
        for value in samples {
            if out.last().is_none_or(|previous| previous != value) {
                out.push(*value);
            }
        }
        out
    }

    #[test]
    fn classifies_the_gpu_share_with_a_closed_middle_band() {
        assert_eq!(classify_bound(5.0, 10.0), FrameBound::CpuBound);
        let frame = 10.0;
        let at_cpu = CPU_BOUND_GPU_SHARE * frame;
        assert_eq!(classify_bound(at_cpu, frame), FrameBound::Balanced);
        assert_eq!(classify_bound(at_cpu * 0.99, frame), FrameBound::CpuBound);
        assert_eq!(classify_bound(7.5, 10.0), FrameBound::Balanced);
        let at_gpu = GPU_BOUND_GPU_SHARE * frame;
        assert_eq!(classify_bound(at_gpu, frame), FrameBound::Balanced);
        assert_eq!(classify_bound(at_gpu * 1.01, frame), FrameBound::GpuBound);
        assert_eq!(classify_bound(GPU_CPU_MS, FRAME_CPU_MS), FrameBound::CpuBound);
        assert_eq!(classify_bound(GPU_OVER_MS, FRAME_OVER_MS), FrameBound::GpuBound);
    }

    #[test]
    fn bad_inputs_classify_as_balanced() {
        for gpu in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0, -0.0] {
            assert_eq!(classify_bound(gpu, 16.0), FrameBound::Balanced);
        }
        for frame in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -4.0] {
            assert_eq!(classify_bound(8.0, frame), FrameBound::Balanced);
        }
    }

    #[test]
    fn bound_labels_are_stable() {
        assert_eq!(FrameBound::CpuBound.label(), "cpu");
        assert_eq!(FrameBound::GpuBound.label(), "gpu");
        assert_eq!(FrameBound::Balanced.label(), "balanced");
    }

    #[test]
    fn cpu_bound_scene_keeps_native_permille_for_three_minutes() {
        // DO27: the scale fell 1000 → 500 while fps stayed ~15. GPU 31 ms, wall 66 ms
        // (~15 fps), budget 16.6 ms. Three minutes of those frames must stay at 1000.
        let mut controller = DynResController::new(DynResConfig::default(), 1000);
        let frames = 3 * 60 * 1000 / 66;
        assert!(frames >= 3 * 60 * 15, "the run covers three minutes at 15 fps");
        for _ in 0..frames {
            assert_eq!(controller.update(GPU_CPU_MS, FRAME_CPU_MS), 1000);
        }
        assert_eq!(controller.permille(), 1000);
        assert_eq!(controller.bound(), FrameBound::CpuBound);
        let upper = controller.config().budget_ms * (1.0 + controller.config().hysteresis);
        assert!(controller.median_gpu_ms() > upper);
    }

    #[test]
    fn cpu_bound_above_budget_does_not_step_down_from_a_reduced_scale() {
        let mut controller = DynResController::new(DynResConfig::default(), 700);
        for _ in 0..(3 * 60 * 15) {
            assert_eq!(controller.update(GPU_CPU_MS, FRAME_CPU_MS), 700);
        }
        assert_eq!(controller.bound(), FrameBound::CpuBound);
    }

    #[test]
    fn balanced_over_budget_does_not_step_down() {
        // 20/25 = 0.80, inside the band, and 20 ms is over the 16.6 ms budget.
        let mut controller = DynResController::new(DynResConfig::default(), 1000);
        for _ in 0..500 {
            assert_eq!(controller.update(20.0, 25.0), 1000);
        }
        assert_eq!(controller.bound(), FrameBound::Balanced);
    }

    #[test]
    fn gpu_bound_overload_steps_down_by_50_to_500_and_no_lower() {
        let mut controller = DynResController::new(DynResConfig::default(), 1000);
        let mut trace = Vec::new();
        for _ in 0..5_000 {
            let permille = controller.update(GPU_OVER_MS, FRAME_OVER_MS);
            assert!(permille >= 500);
            assert!(permille <= 1000);
            trace.push(permille);
        }
        assert_eq!(
            changes_of(&trace),
            vec![1000, 950, 900, 850, 800, 750, 700, 650, 600, 550, 500]
        );
        assert_eq!(controller.permille(), 500);
        assert_eq!(controller.bound(), FrameBound::GpuBound);
        for _ in 0..500 {
            assert_eq!(controller.update(80.0, 82.0), 500);
        }
    }

    #[test]
    fn a_dropped_scale_recovers_when_the_load_drops_even_if_cpu_bound() {
        let mut controller = DynResController::new(DynResConfig::default(), 1000);
        for _ in 0..5_000 {
            assert!(controller.update(GPU_OVER_MS, FRAME_OVER_MS) >= 500);
        }
        assert_eq!(controller.permille(), 500);
        // 8/66 is CPU-bound, and 8 ms is under the lower hysteresis band.
        let mut trace = Vec::new();
        for _ in 0..8_000 {
            trace.push(controller.update(8.0, FRAME_CPU_MS));
        }
        let steps: Vec<u32> = changes_of(&trace).into_iter().skip(1).collect();
        assert_eq!(steps.len(), 20);
        assert_eq!(steps.first().copied(), Some(525));
        assert_eq!(steps.last().copied(), Some(1000));
        assert!(steps.windows(2).all(|pair| pair[1] == pair[0] + 25));
        assert_eq!(controller.permille(), 1000);
        assert_eq!(controller.bound(), FrameBound::CpuBound);
        for _ in 0..200 {
            assert_eq!(controller.update(8.0, FRAME_CPU_MS), 1000);
        }
    }

    #[test]
    fn the_scale_holds_until_the_window_is_full() {
        let config = DynResConfig {
            window_frames: 8,
            cooldown_frames: 0,
            ..DynResConfig::default()
        };
        let mut controller = DynResController::new(config, 1000);
        for _ in 0..7 {
            assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 1000);
        }
        assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 950);
    }

    #[test]
    fn cooldown_holds_for_the_configured_samples_then_allows_one_step() {
        let config = DynResConfig {
            window_frames: 1,
            cooldown_frames: 60,
            ..DynResConfig::default()
        };
        let mut controller = DynResController::new(config, 1000);
        assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 950);
        for _ in 0..60 {
            assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 950);
        }
        assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 900);
    }

    #[test]
    fn gpu_bound_under_budget_steps_up() {
        let config = DynResConfig {
            window_frames: 1,
            cooldown_frames: 0,
            ..DynResConfig::default()
        };
        let mut controller = DynResController::new(config, 800);
        // 10/11 is over 90% GPU, and 10 ms is under 16.6 * 0.9.
        assert_eq!(controller.update(10.0, 11.0), 825);
        assert_eq!(controller.bound(), FrameBound::GpuBound);
    }

    #[test]
    fn the_deadband_edges_hold() {
        // budget 16 and hysteresis 0.25 put the edges on 20 and 12, both exact in f32.
        let config = DynResConfig {
            budget_ms: 16.0,
            hysteresis: 0.25,
            window_frames: 1,
            cooldown_frames: 0,
            ..DynResConfig::default()
        };
        let mut controller = DynResController::new(config, 800);
        assert_eq!(controller.update(20.0, 21.0), 800);
        assert_eq!(controller.update(12.0, 13.0), 800);
        assert_eq!(controller.permille(), 800);
    }

    #[test]
    fn hysteresis_holds_a_noisy_budget_and_a_zero_band_does_not() {
        // 16 and 0.25 are exact in f32, so the band edges are 20 and 12.
        let mut held_config = DynResConfig {
            budget_ms: 16.0,
            hysteresis: 0.25,
            window_frames: 4,
            cooldown_frames: 0,
            ..DynResConfig::default()
        };
        let mut held = DynResController::new(held_config, 1000);
        held_config.hysteresis = 0.0;
        let mut open = DynResController::new(held_config, 1000);
        let mut held_trace = Vec::new();
        let mut open_trace = Vec::new();
        for i in 0..80 {
            let gpu = if i % 8 < 4 { 18.0 } else { 14.0 };
            let frame = gpu + 1.0;
            held_trace.push(held.update(gpu, frame));
            open_trace.push(open.update(gpu, frame));
        }
        assert_eq!(changes_of(&held_trace), vec![1000]);
        assert!(changes_of(&open_trace).len() >= 3);
        assert_eq!(held.bound(), FrameBound::GpuBound);
    }

    #[test]
    fn non_finite_and_non_positive_samples_are_ignored() {
        let config = DynResConfig {
            window_frames: 1,
            cooldown_frames: 60,
            ..DynResConfig::default()
        };
        let mut controller = DynResController::new(config, 1000);
        assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 950);
        let median_bits = controller.median_gpu_ms().to_bits();
        let bound = controller.bound();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0, -0.0] {
            assert_eq!(controller.update(bad, FRAME_OVER_MS), 950);
            assert_eq!(controller.update(GPU_OVER_MS, bad), 950);
            assert_eq!(controller.bound(), bound);
            assert_eq!(controller.median_gpu_ms().to_bits(), median_bits);
        }
        // Ignored samples do not count toward the cooldown: 60 accepted frames still have to pass.
        for _ in 0..30 {
            assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 950);
        }
        assert_eq!(controller.update(f32::NAN, FRAME_OVER_MS), 950);
        for _ in 0..30 {
            assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 950);
        }
        assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 900);
    }

    #[test]
    fn min_greater_than_max_is_clamped_to_a_point() {
        let config = DynResConfig {
            min_permille: 900,
            max_permille: 600,
            budget_ms: f32::NAN,
            window_frames: 0,
            hysteresis: f32::INFINITY,
            ..DynResConfig::default()
        };
        let mut controller = DynResController::new(config, 1000);
        assert_eq!(controller.config().min_permille, 600);
        assert_eq!(controller.config().max_permille, 600);
        assert_eq!(controller.config().budget_ms.to_bits(), 16.6f32.to_bits());
        assert_eq!(controller.config().window_frames, 1);
        assert_eq!(controller.config().hysteresis.to_bits(), 0.1f32.to_bits());
        assert_eq!(controller.permille(), 600);
        for _ in 0..50 {
            assert_eq!(controller.update(4.0, 40.0), 600);
            assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 600);
        }
    }

    #[test]
    fn a_large_step_stops_on_the_floor_and_the_ceiling() {
        let config = DynResConfig {
            step_down_permille: 10_000,
            step_up_permille: 10_000,
            window_frames: 1,
            cooldown_frames: 0,
            ..DynResConfig::default()
        };
        let mut down = DynResController::new(config, 1000);
        assert_eq!(down.update(GPU_OVER_MS, FRAME_OVER_MS), 500);
        assert_eq!(down.update(GPU_OVER_MS, FRAME_OVER_MS), 500);
        let mut up = DynResController::new(config, 500);
        assert_eq!(up.update(4.0, 40.0), 1000);
        assert_eq!(up.update(4.0, 40.0), 1000);
    }

    #[test]
    fn median_is_the_central_order_statistic() {
        let config = DynResConfig {
            budget_ms: 1.0e6,
            window_frames: 4,
            ..DynResConfig::default()
        };
        let mut even = DynResController::new(config, 1000);
        for value in [10.0, 30.0, 20.0, 40.0] {
            assert_eq!(even.update(value, value + 1.0), 1000);
        }
        assert_eq!(even.median_gpu_ms().to_bits(), 25.0f32.to_bits());
        assert_eq!(even.permille(), 1000);

        let mut odd = DynResController::new(
            DynResConfig {
                window_frames: 5,
                ..config
            },
            1000,
        );
        for value in [10.0, 100.0, 10.0, 10.0, 10.0] {
            assert_eq!(odd.update(value, value + 1.0), 1000);
        }
        // One spike in five samples stays out of the middle.
        assert_eq!(odd.median_gpu_ms().to_bits(), 10.0f32.to_bits());
    }

    #[test]
    fn the_same_inputs_produce_the_same_trace() {
        let run = |seed_bad: bool| {
            let mut controller = DynResController::new(DynResConfig::default(), 800);
            let mut trace = Vec::new();
            for i in 0..200 {
                let (gpu, frame) = if seed_bad && i == 50 {
                    (f32::NAN, 10.0)
                } else if i < 100 {
                    (GPU_OVER_MS, FRAME_OVER_MS)
                } else {
                    (8.0, FRAME_CPU_MS)
                };
                trace.push(controller.update(gpu, frame));
            }
            trace
        };
        assert_eq!(run(false), run(false));
        let with_gap = run(true);
        let clean = run(false);
        assert_ne!(with_gap, clean);
        assert_eq!(run(true), run(true));
    }

    #[test]
    fn a_quality_mode_replaces_the_range_and_keeps_the_rest() {
        use crate::UpscaleQuality as Q;
        let base = DynResConfig::default();
        let ranges: Vec<(u32, u32)> = Q::ALL
            .iter()
            .map(|mode| {
                let config = base.within_quality(*mode);
                assert_eq!(config.budget_ms.to_bits(), base.budget_ms.to_bits());
                assert_eq!(config.step_down_permille, base.step_down_permille);
                assert_eq!(config.window_frames, base.window_frames);
                (config.min_permille, config.max_permille)
            })
            .collect();
        assert_eq!(
            ranges,
            vec![(667, 1000), (588, 667), (500, 588), (333, 500), (333, 333)]
        );
        // Ultra Performance cannot move: an overload holds 333.
        let config = DynResConfig {
            window_frames: 1,
            cooldown_frames: 0,
            ..base
        }
        .within_quality(Q::UltraPerformance);
        let mut controller = DynResController::new(config, 1000);
        for _ in 0..10 {
            assert_eq!(controller.update(GPU_OVER_MS, FRAME_OVER_MS), 333);
        }
    }

    #[test]
    fn default_config_matches_the_published_knobs() {
        let config = DynResConfig::default();
        assert_eq!(config.budget_ms.to_bits(), 16.6f32.to_bits());
        assert_eq!(config.min_permille, 500);
        assert_eq!(config.max_permille, 1000);
        assert_eq!(config.step_down_permille, 50);
        assert_eq!(config.step_up_permille, 25);
        assert_eq!(config.window_frames, 30);
        assert_eq!(config.hysteresis.to_bits(), 0.1f32.to_bits());
        assert_eq!(config.cooldown_frames, 60);
        assert_eq!(
            DynResController::new(config, 1000).median_gpu_ms().to_bits(),
            0.0f32.to_bits()
        );
        assert_eq!(DynResController::new(config, 2500).permille(), 1000);
        assert_eq!(DynResController::new(config, 100).permille(), 500);
    }

    /// GPU time of DO27-like play frames at `permille`: 20 ms at native, scaling with the pixel
    /// count. GPU-bound (the frame is the GPU time).
    fn play_gpu_ms(permille: u32) -> f32 {
        let scale = permille as f32 / 1000.0;
        20.0 * scale * scale
    }

    /// `frames` play frames from `controller`'s current scale; returns the permille trace.
    fn run_play(controller: &mut DynResController, frames: usize) -> Vec<u32> {
        let mut trace = Vec::with_capacity(frames);
        let mut permille = controller.permille();
        for _ in 0..frames {
            let gpu = play_gpu_ms(permille);
            permille = controller.update(gpu, gpu);
            trace.push(permille);
        }
        trace
    }

    /// `frames` cheap menu frames: 3 ms of GPU inside a 60 fps cap.
    fn run_menu(controller: &mut DynResController, frames: usize) -> Vec<u32> {
        (0..frames).map(|_| controller.update(3.0, 16.7)).collect()
    }

    #[test]
    fn a_held_menu_stretch_returns_play_to_the_settled_scale() {
        let mut controller = DynResController::new(DynResConfig::default(), 1000);
        let play = run_play(&mut controller, 600);
        let settled = controller.permille();
        assert_eq!(
            changes_of(&play),
            vec![1000, 950],
            "play settles at 950: {:?}",
            changes_of(&play)
        );
        controller.hold();
        assert!(controller.is_held());
        let menu = run_menu(&mut controller, 600);
        assert!(
            menu.iter().all(|p| *p == settled),
            "held: {:?}",
            changes_of(&menu)
        );
        controller.resume();
        assert!(!controller.is_held());
        let again = run_play(&mut controller, 600);
        assert_eq!(
            again[0], settled,
            "the first play frame after resume keeps the settled scale"
        );
        assert!(
            again.iter().all(|p| *p == settled),
            "no oscillation after the menu: {:?}",
            changes_of(&again)
        );
    }

    #[test]
    fn without_a_hold_menu_frames_step_the_scale_up_to_native() {
        // The unchanged no-hold behavior (and the v0.5.0 oscillation the hold removes).
        let mut controller = DynResController::new(DynResConfig::default(), 1000);
        let _ = run_play(&mut controller, 600);
        assert_eq!(controller.permille(), 950);
        let _ = run_menu(&mut controller, 600);
        assert_eq!(controller.permille(), 1000, "cheap menu frames climb to native");
        let again = run_play(&mut controller, 600);
        assert_eq!(again[0], 1000);
        assert_eq!(*again.last().unwrap_or(&0), 950, "and play steps it down again");
    }

    #[test]
    fn resume_clears_the_window_so_stale_cheap_samples_never_step_up() {
        let config = DynResConfig {
            cooldown_frames: 0,
            ..DynResConfig::default()
        };
        let mut controller = DynResController::new(config, 800);
        // 29 cheap samples: one short of a full window.
        for _ in 0..29 {
            assert_eq!(controller.update(3.0, 16.7), 800);
        }
        controller.hold();
        assert_eq!(controller.update(3.0, 16.7), 800, "held");
        controller.resume();
        assert_eq!(
            controller.median_gpu_ms(),
            0.0,
            "the window is empty after resume"
        );
        // Without the clear, the next cheap sample would fill the window and step up.
        for _ in 0..29 {
            assert_eq!(controller.update(3.0, 16.7), 800);
        }
        assert_eq!(controller.update(3.0, 16.7), 825, "a full new window steps");
    }

    #[test]
    fn the_governor_holds_outside_play_and_returns_to_the_settled_scale() {
        use regia_app::PacingState;
        let mut governor = DynResGovernor::new(DynResController::new(DynResConfig::default(), 1000));
        let mut permille = 1000;
        for _ in 0..600 {
            let gpu = play_gpu_ms(permille);
            permille = governor.feed(PacingState::Play, gpu, gpu);
        }
        assert_eq!(permille, 950);
        for state in [PacingState::Paused, PacingState::Menu, PacingState::Loading] {
            for _ in 0..600 {
                assert_eq!(governor.feed(state, 3.0, 16.7), 950, "{state:?}");
            }
            assert!(governor.controller.is_held());
        }
        let gpu = play_gpu_ms(permille);
        assert_eq!(governor.feed(PacingState::Play, gpu, gpu), 950);
        assert!(!governor.controller.is_held());
        // Opted out: menu frames move the scale as before.
        governor.hold_outside_play = false;
        for _ in 0..600 {
            permille = governor.feed(PacingState::Menu, 3.0, 16.7);
        }
        assert_eq!(permille, 1000);
    }

    #[test]
    fn hold_freezes_bound_and_median_and_resume_without_hold_is_a_no_op() {
        let mut controller = DynResController::new(DynResConfig::default(), 1000);
        for _ in 0..10 {
            let _ = controller.update(GPU_OVER_MS, FRAME_OVER_MS);
        }
        let (bound, median) = (controller.bound(), controller.median_gpu_ms());
        controller.resume();
        assert_eq!(
            controller.median_gpu_ms(),
            median,
            "resume while running keeps the window"
        );
        controller.set_held(true);
        controller.set_held(true);
        let _ = controller.update(GPU_CPU_MS, FRAME_CPU_MS);
        assert_eq!(controller.bound(), bound);
        assert_eq!(controller.median_gpu_ms(), median);
        controller.set_held(false);
        assert!(!controller.is_held());
    }
}
