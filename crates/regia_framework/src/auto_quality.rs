// SPDX-License-Identifier: MPL-2.0
//! Automatic quality detection on first launch (book `user-guide/auto-quality.md`, ADR-157).
//!
//! [`AutoQualityPlugin`] is part of `regia::DefaultPlugins`. It works on the persisted
//! [`GameSettings`] (add [`crate::settings::SettingsPlugin`]): when `graphics.preset` has never been
//! set ([`PresetSource::Unset`], a first launch), it reads the GPU, picks a tier from the engine's
//! table ([`regia_render::quality::detect`]), writes `graphics.preset` with
//! `preset_source: Detected` and the reason into `graphics.detected`, and the settings plugin
//! applies it to `RenderSettings` and saves it. Optionally ([`AutoQuality::confirm`], off by
//! default since v0.1.0, ADR-173) a probe renderer then draws the host's first scene (DO27's lobby) offscreen on a
//! warm worker for about three seconds ([`regia_render::QualityProbe`]), after the boot splash
//! is gone and the game-frame warm is back, never behind the splash (ADR-173). A sustained GPU
//! frame time over budget steps the tier down once, with a `SettingsChanged`. A level load
//! (`GameFrameWarm::request`) before the probe has finished aborts it: the table's pick stays.
//!
//! It never overrides the player: a preset the player chose ([`PresetSource::Player`], or a
//! non-default preset in an older file) is left alone, and detection runs once — later launches
//! read the saved value. [`AutoQuality::request_redetect`] (the options menu's "Auto-detect"
//! button, agent `quality.detect`) runs it again and replaces the preset. An app without
//! `GameSettings` is not touched.

use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_render::quality::{
    AdapterProfile, Detection, DeviceClass, GpuTierTable, ProbeConfig, ProbeResult, ProbeVerdict,
    QualityTier, detect, judge_probe,
};

use crate::settings::{
    DetectedQuality, DetectorWrote, GameSettings, GraphicsPreset, PresetSource, ProbeRecord, SettingsSystems,
};

/// Automatic quality detection's configuration and state (a resource; insert your own before
/// `DefaultPlugins` to change the defaults).
#[derive(Resource, Clone, Debug)]
pub struct AutoQuality {
    /// Detect on a first launch (default `true`).
    pub enabled: bool,
    /// Run the confirmation probe after detecting. Default **off** (ADR-173): the reworked
    /// offscreen probe's GPU timings were not trustworthy on the RTX 5080 at v0.1.0, so the
    /// first launch uses the table's pick only. Leave it off on Android, where settings
    /// persistence is up to the shell.
    pub confirm: bool,
    /// The probe's frame-rate target; `None` = the tier's default (60 desktop, 40 Deck, 30 mobile).
    pub target_fps: Option<f32>,
    /// Probe length, seconds (a quarter is warm-up). Default 3.
    pub probe_seconds: f32,
    /// The GPU table ([`GpuTierTable::builtin`]); prepend rows to extend it.
    pub table: GpuTierTable,
    redetect: bool,
    phase: Phase,
    last: Option<Detection>,
    verdict: Option<ProbeVerdict>,
    runs: u32,
    /// The last probe drew smaller than the monitor while fullscreen was asked for: probe again
    /// on the next frame the window fills it (v0.5.1), from this tier (the one that probe ran
    /// on: its verdict was not applied).
    reprobe: Option<QualityTier>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Idle,
    Probing {
        tier: QualityTier,
        since: Instant,
        /// [`regia_render::GameFrameWarm::explicit_requests`] when the probe was requested: a
        /// later request is a level load, which aborts the probe (ADR-173).
        warm_requests: u64,
    },
    Done,
}

impl Default for AutoQuality {
    fn default() -> Self {
        Self {
            enabled: true,
            confirm: false,
            target_fps: None,
            probe_seconds: 3.0,
            table: GpuTierTable::builtin(),
            redetect: false,
            phase: Phase::Idle,
            last: None,
            verdict: None,
            runs: 0,
            reprobe: None,
        }
    }
}

impl AutoQuality {
    /// Extra time the probe may take beyond its length before detection stops waiting for it
    /// (a headless app never draws a window frame).
    pub const PROBE_GRACE: Duration = Duration::from_secs(20);

    /// With the confirmation probe on or off.
    #[must_use]
    pub fn with_confirm(mut self, confirm: bool) -> Self {
        self.confirm = confirm;
        self
    }

    /// With a probe frame-rate target (`None` = the tier's default).
    #[must_use]
    pub fn with_target_fps(mut self, target_fps: Option<f32>) -> Self {
        self.target_fps = target_fps;
        self
    }

    /// With a probe length in seconds.
    #[must_use]
    pub fn with_probe_seconds(mut self, seconds: f32) -> Self {
        self.probe_seconds = if seconds.is_finite() {
            seconds.clamp(0.0, 3600.0)
        } else {
            3.0
        };
        self
    }

    /// With detection on or off.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Detects again on the next frame and replaces `graphics.preset`, even one the player chose
    /// (the options menu's "Auto-detect" button).
    pub fn request_redetect(&mut self) {
        self.redetect = true;
        self.phase = Phase::Idle;
    }

    /// Whether a re-detect is queued for the next frame.
    #[must_use]
    pub fn redetect_pending(&self) -> bool {
        self.redetect
    }

    /// The detection made in this process, if any.
    #[must_use]
    pub fn last_detection(&self) -> Option<&Detection> {
        self.last.as_ref()
    }

    /// The probe's verdict in this process, if it ran.
    #[must_use]
    pub fn last_verdict(&self) -> Option<&ProbeVerdict> {
        self.verdict.as_ref()
    }

    /// Detections run in this process (0 on a launch that read a saved preset).
    #[must_use]
    pub fn detections_run(&self) -> u32 {
        self.runs
    }

    /// v0.5.1: the last probe's size did not match the monitor while fullscreen was asked for
    /// ([`ProbeRecord::size_mismatch`]); it runs again on the next frame the window fills the
    /// monitor.
    #[must_use]
    pub fn reprobe_pending(&self) -> bool {
        self.reprobe.is_some()
    }

    /// Whether the confirmation probe is running.
    #[must_use]
    pub fn is_probing(&self) -> bool {
        matches!(self.phase, Phase::Probing { .. })
    }

    /// The frame rate the probe holds `tier` to.
    #[must_use]
    pub fn target_fps_for(&self, tier: QualityTier) -> f32 {
        self.target_fps.unwrap_or_else(|| tier.default_target_fps())
    }

    /// What the table picks for `adapter` (no side effects).
    #[must_use]
    pub fn detect(&self, adapter: &AdapterProfile) -> Detection {
        detect(adapter, &self.table)
    }
}

/// Pins the adapter detection reads (tests, CI, a shell reporting a GPU the engine cannot query).
#[derive(Resource, Clone, Debug)]
pub struct AdapterOverride(pub AdapterProfile);

/// The display a probe's frames were drawn for (v0.5.1): the primary window's requested mode
/// and physical size, and the physical size of the monitor it is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProbeDisplay {
    /// The host asked for fullscreen (`WindowMode::BorderlessFullscreen`).
    pub fullscreen: bool,
    /// The window's physical size now.
    pub window_size: Option<[u32; 2]>,
    /// The monitor's physical size.
    pub monitor_size: Option<[u32; 2]>,
}

impl ProbeDisplay {
    /// `windowed` / `borderless_fullscreen`, or empty without a window.
    #[must_use]
    pub fn mode_name(&self) -> &'static str {
        match (self.window_size, self.fullscreen) {
            (None, false) => "",
            (_, true) => "borderless_fullscreen",
            (Some(_), false) => "windowed",
        }
    }

    /// Whether a probe that drew at `size` measured less than the monitor while fullscreen was
    /// asked for.
    #[must_use]
    pub fn size_mismatch(&self, size: [u32; 2]) -> bool {
        self.fullscreen
            && self
                .monitor_size
                .is_some_and(|m| size[0] < m[0] || size[1] < m[1])
    }

    /// Whether the window fills the monitor in fullscreen now (a re-probe may start).
    #[must_use]
    pub fn fills_monitor(&self) -> bool {
        self.fullscreen && self.window_size.is_some() && self.window_size == self.monitor_size
    }
}

/// Pins [`ProbeDisplay`] instead of reading the primary window and its monitor (tests, hosts
/// that draw elsewhere).
#[derive(Resource, Clone, Copy, Debug)]
pub struct DisplayOverride(pub ProbeDisplay);

/// The primary window's [`ProbeDisplay`] ([`DisplayOverride`] first).
pub fn current_display(world: &mut World) -> ProbeDisplay {
    if let Some(o) = world.get_resource::<DisplayOverride>() {
        return o.0;
    }
    let window = {
        let mut q = world.query_filtered::<&regia_platform::Window, With<regia_platform::PrimaryWindow>>();
        q.iter(world).next().map(|w| {
            (
                w.mode == regia_platform::WindowMode::BorderlessFullscreen,
                w.physical_size,
            )
        })
    };
    let monitor_size = world
        .get_resource::<regia_platform::WindowHandles>()
        .and_then(|handles| {
            let (_, os) = handles.iter().next()?;
            let size = os.current_monitor()?.size();
            Some([size.width, size.height])
        });
    ProbeDisplay {
        fullscreen: window.is_some_and(|(f, _)| f),
        window_size: window.map(|(_, (w, h))| [w, h]),
        monitor_size,
    }
}

/// Adds [`AutoQuality`] and its system (in `regia::DefaultPlugins`).
#[derive(Default)]
pub struct AutoQualityPlugin;

impl Plugin for AutoQualityPlugin {
    fn build(&self, app: &mut App) {
        crate::settings::apply_packaged_startup(app.world_mut());
        app.init_resource::<AutoQuality>()
            .add_systems(Update, auto_detect_quality.before(SettingsSystems::Apply));
    }
}

/// The adapter detection reads: the override, else the render device's.
#[must_use]
pub fn current_adapter(world: &World) -> Option<AdapterProfile> {
    world
        .get_resource::<AdapterOverride>()
        .map(|o| o.0.clone())
        .or_else(|| {
            world
                .get_resource::<regia_render::RenderDevice>()
                .map(AdapterProfile::from_device)
        })
}

/// `Update`, before the settings apply: detect on a first launch or a re-detect, then judge the
/// probe when its result arrives.
#[tracing::instrument(skip_all, target = "regia_framework", name = "auto_quality")]
fn auto_detect_quality(world: &mut World) {
    let Some(auto) = world.get_resource::<AutoQuality>() else {
        return;
    };
    if !auto.enabled || !world.contains_resource::<GameSettings>() {
        return;
    }
    let (phase, redetect) = (auto.phase, auto.redetect);
    if !redetect && crate::packaged_config::active_profile(world).is_some() {
        return;
    }
    let length = Duration::try_from_secs_f32(auto.probe_seconds.max(0.0)).unwrap_or(Duration::from_secs(3));
    if let Phase::Probing {
        tier,
        since,
        warm_requests,
    } = phase
    {
        // A level load before the probe finished: keep the table's pick (ADR-173).
        if world
            .get_resource::<regia_render::GameFrameWarm>()
            .is_some_and(|w| w.explicit_requests() != warm_requests)
            && let Some(mut probe) = world.get_resource_mut::<regia_render::QualityProbe>()
            && probe.abort()
        {
            tracing::info!(target: "regia_framework", "quality probe aborted: a level load started; keeping {tier}");
        }
        let result = world
            .get_resource_mut::<regia_render::QualityProbe>()
            .and_then(|mut p| p.take_result());
        if let Some(result) = result {
            finish_probe(world, tier, &result);
        } else if since.elapsed() > length + AutoQuality::PROBE_GRACE {
            tracing::warn!(target: "regia_framework", "quality probe never ran; keeping {tier}");
            world.resource_mut::<AutoQuality>().phase = Phase::Done;
        }
        return;
    }
    if phase == Phase::Done && world.resource::<AutoQuality>().reprobe_pending() && !redetect {
        reprobe_at_monitor_size(world);
        return;
    }
    let source = world.resource::<GameSettings>().graphics.source();
    if !(redetect || (source == PresetSource::Unset && phase == Phase::Idle)) {
        return;
    }
    // Wait for the device (it appears on the first resume).
    let Some(adapter) = current_adapter(world) else {
        return;
    };
    let detection = world.resource::<AutoQuality>().detect(&adapter);
    tracing::info!(target: "regia_framework", "quality detected: {}", detection.reason);
    {
        let mut settings = world.resource_mut::<GameSettings>();
        let graphics = &mut settings.graphics;
        graphics.preset = GraphicsPreset::from_tier(detection.tier);
        graphics.preset_source = Some(PresetSource::Detected);
        graphics.detected = Some(DetectedQuality {
            tier: detection.tier.name().to_string(),
            detected_tier: detection.tier.name().to_string(),
            reason: detection.reason.clone(),
            rule: detection.rule.clone(),
            evidence: detection.evidence.clone(),
            gpu: adapter.name.clone(),
            pci_id: [adapter.vendor, adapter.device],
            vram_mb: adapter.vram_mb.unwrap_or(0),
            driver: adapter.driver.clone(),
            probe: None,
        });
    }
    world.insert_resource(DetectorWrote);
    rewarm(world);
    // A probe that can change nothing is skipped (ADR-173): a tier with no step down (Very Low,
    // the Deck, mobile low), or a software rasteriser, whose back-to-back probe frames would only
    // stall the lobby. Low is probed since ADR-7882 (its step is Very Low, below 30 fps only).
    let probe = {
        let auto = world.resource::<AutoQuality>();
        (auto.confirm && detection.tier.step_down().is_some() && adapter.class != DeviceClass::Cpu)
            .then(|| ProbeConfig::with_seconds(auto.probe_seconds))
    };
    let probing = match (probe, world.get_resource_mut::<regia_render::QualityProbe>()) {
        (Some(config), Some(mut p)) => {
            p.request(config);
            true
        }
        _ => false,
    };
    // After `rewarm`: the detection's own re-warm is not a level load.
    let warm_requests = world
        .get_resource::<regia_render::GameFrameWarm>()
        .map_or(0, regia_render::GameFrameWarm::explicit_requests);
    let mut auto = world.resource_mut::<AutoQuality>();
    auto.redetect = false;
    auto.runs += 1;
    auto.verdict = None;
    auto.phase = if probing {
        Phase::Probing {
            tier: detection.tier,
            since: Instant::now(),
            warm_requests,
        }
    } else {
        Phase::Done
    };
    auto.last = Some(detection);
}

/// A warm already ran (or is queued) on the old tier: warm again so the new tier's first frame
/// compiles nothing.
fn rewarm(world: &mut World) {
    if let Some(mut warm) = world.get_resource_mut::<regia_render::GameFrameWarm>()
        && (warm.last_report().is_some() || warm.is_pending())
    {
        warm.request();
    }
}

/// v0.5.1: a probe that drew smaller than the monitor while fullscreen was asked for runs again
/// on the first frame the window fills the monitor.
fn reprobe_at_monitor_size(world: &mut World) {
    if !current_display(world).fills_monitor() {
        return;
    }
    let (seconds, tier) = {
        let auto = world.resource::<AutoQuality>();
        (auto.probe_seconds, auto.reprobe)
    };
    let Some(tier) = tier else {
        return;
    };
    let started = match world.get_resource_mut::<regia_render::QualityProbe>() {
        Some(mut p) => {
            p.request(ProbeConfig::with_seconds(seconds));
            true
        }
        None => false,
    };
    let warm_requests = world
        .get_resource::<regia_render::GameFrameWarm>()
        .map_or(0, regia_render::GameFrameWarm::explicit_requests);
    let mut auto = world.resource_mut::<AutoQuality>();
    auto.reprobe = None;
    if started {
        tracing::info!(target: "regia_framework", "quality probe: again at the monitor size ({tier})");
        auto.phase = Phase::Probing {
            tier,
            since: Instant::now(),
            warm_requests,
        };
    }
}

fn finish_probe(world: &mut World, tier: QualityTier, result: &ProbeResult) {
    let screen = current_display(world);
    let mismatch = screen.size_mismatch([result.width, result.height]);
    if mismatch {
        tracing::info!(
            target: "regia_framework",
            "quality probe drew {}x{} on a {:?} monitor with fullscreen asked for: probing again when the window fills it",
            result.width,
            result.height,
            screen.monitor_size
        );
    }
    world.resource_mut::<AutoQuality>().reprobe = mismatch.then_some(tier);
    let target = world.resource::<AutoQuality>().target_fps_for(tier);
    let verdict = judge_probe(tier, result, target);
    tracing::info!(target: "regia_framework", "quality probe: {}", verdict.reason);
    // A probe that drew smaller than the monitor is not the monitor's number: it is recorded, and
    // nothing steps down on it (no preset change, no re-warm) until the probe at the monitor size.
    let applied = !mismatch && verdict.stepped_down;
    let ours = world.resource::<GameSettings>().graphics.source() == PresetSource::Detected;
    if ours {
        let mut settings = world.resource_mut::<GameSettings>();
        let graphics = &mut settings.graphics;
        if applied {
            graphics.preset = GraphicsPreset::from_tier(verdict.tier);
        }
        if let Some(detected) = graphics.detected.as_mut() {
            detected.tier = if mismatch { tier } else { verdict.tier }.name().to_string();
            if applied {
                detected.reason = format!("{}; {}", detected.reason, verdict.reason);
            }
            detected.probe = Some(ProbeRecord {
                p50_ms: result.p50_ms,
                p95_ms: result.p95_ms,
                budget_ms: verdict.budget_ms,
                frames: result.frames,
                clock: match result.source {
                    regia_render::quality::ProbeSource::Gpu => "gpu".to_string(),
                    regia_render::quality::ProbeSource::Wall => "wall".to_string(),
                },
                size: [result.width, result.height],
                stepped_down: applied,
                verdict: verdict.reason.clone(),
                monitor_size: screen.monitor_size,
                window_mode: screen.mode_name().to_string(),
                size_mismatch: mismatch,
            });
        }
        world.insert_resource(DetectorWrote);
        if applied {
            rewarm(world);
        }
    }
    let mut auto = world.resource_mut::<AutoQuality>();
    auto.phase = Phase::Done;
    auto.verdict = Some(verdict);
}

#[cfg(test)]
mod tests;
