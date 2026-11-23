// SPDX-License-Identifier: MPL-2.0
//! Game settings (`docs/specs/14-scene-and-framework.md` § B7, settings half — save games are
//! M20): one persisted, live-applied configuration resource.
//!
//! [`GameSettings`] is a reflected resource the game (or an agent, through the `settings.get` /
//! `settings.set` protocol methods) edits at runtime. [`SettingsPlugin`] loads it from
//! `user://settings.ron` at startup, watches it with change detection, **applies** every change
//! live (frame cap → [`FramePacing`]; resolution, fullscreen and vsync → the primary window,
//! ADR-7880) and **persists** the new value back to disk (atomic replace) — so a settings menu
//! never needs a "restart to apply" for the fields the engine owns.
//!
//! Games add their own sections by extending the struct or storing a reflected sidecar; the
//! engine fields here are the ones the engine knows how to apply.

use std::path::PathBuf;

use regia_app::FramePacing;
use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

/// Graphics quality preset (`GameSettings::graphics.preset`).
#[derive(Reflect, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Default)]
pub enum GraphicsPreset {
    /// Lowest cost: for integrated GPUs and handhelds.
    Low,
    /// The balanced default.
    #[default]
    Medium,
    /// Higher-quality shadows and effects.
    High,
    /// Everything on, for showcase hardware.
    Ultra,
    /// DO27's "clean" competitive preset: minimal post effects, maximum clarity. Renders as
    /// `High`; the game strips its post effects itself.
    Clean,
    /// The Steam Deck preset (auto-detected on a Deck).
    Deck,
    /// Below `Low`: Desktop Low drawn at a third of the size and upscaled (ADR-7882), for GPUs that miss 30 fps
    /// at Low (detected on the slowest built-in graphics, or the probe's step from Low).
    VeryLow,
}

impl GraphicsPreset {
    /// The render tier this preset runs on this platform (`android`): on a phone `Low` and
    /// `VeryLow` are Mobile Low and every other preset Mobile High; on desktop `Clean` renders as
    /// `High`.
    #[must_use]
    pub const fn tier(self, android: bool) -> regia_render::QualityTier {
        use regia_render::QualityTier as T;
        let tier = match self {
            Self::Low => T::Low,
            Self::Medium => T::Medium,
            Self::High | Self::Clean => T::High,
            Self::Ultra => T::Ultra,
            Self::Deck => T::Deck,
            Self::VeryLow => T::VeryLow,
        };
        tier.for_platform(android)
    }

    /// The menu preset of a detected tier (Mobile High → `High`, Mobile Low → `Low`).
    #[must_use]
    pub const fn from_tier(tier: regia_render::QualityTier) -> Self {
        use regia_render::QualityTier as T;
        match tier {
            T::Low | T::MobileLow => Self::Low,
            T::Medium => Self::Medium,
            T::High | T::MobileHigh => Self::High,
            T::Ultra => Self::Ultra,
            T::Deck => Self::Deck,
            T::VeryLow => Self::VeryLow,
        }
    }
}

/// Who set `graphics.preset` (`GraphicsSettings::source`).
#[derive(Reflect, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresetSource {
    /// Nobody yet: a first launch. Automatic detection runs (`crate::auto_quality`) and the
    /// engine leaves the game's `RenderSettings` alone until then (the settings plugin inserts
    /// `RenderSettings::default()` only when the game has none).
    Unset,
    /// Automatic detection picked it for this GPU.
    Detected,
    /// The player (or the game's menu) chose it; detection never overrides it.
    Player,
}

/// The last automatic detection (`graphics.detected`), kept for the options menu ("Auto: High —
/// RTX 3080 Ti, 12 GB") and the agent (`quality.get_detected`).
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
#[serde(default)]
pub struct DetectedQuality {
    /// The tier in effect after the probe (`regia_render::QualityTier` name, e.g. `"High"`).
    pub tier: String,
    /// The table's tier before the probe.
    pub detected_tier: String,
    /// One line: "NVIDIA GeForce RTX 3080 Ti, 12 GB → High (nvidia_rtx_3080_3090, measured)".
    pub reason: String,
    /// Table row id, or `fallback`.
    pub rule: String,
    /// `measured`, `by_class`, `guess` or `fallback`.
    pub evidence: String,
    /// Adapter name.
    pub gpu: String,
    /// PCI vendor and device id.
    pub pci_id: [u32; 2],
    /// Device-local memory, MiB (0 = unknown).
    pub vram_mb: u64,
    /// Driver name and version.
    pub driver: String,
    /// The confirmation probe, when it ran.
    pub probe: Option<ProbeRecord>,
}

/// What the first-launch probe measured (`graphics.detected.probe`).
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
#[serde(default)]
pub struct ProbeRecord {
    /// Median frame time, ms.
    pub p50_ms: f32,
    /// 95th percentile frame time, ms.
    pub p95_ms: f32,
    /// The budget (1000 / target fps), ms.
    pub budget_ms: f32,
    /// Timed frames.
    pub frames: u32,
    /// `gpu` (timestamps) or `wall`.
    pub clock: String,
    /// Window size it drew at.
    pub size: [u32; 2],
    /// Whether it stepped the tier down.
    pub stepped_down: bool,
    /// The verdict line.
    pub verdict: String,
    /// v0.5.1: the physical size of the monitor the window was on, when known.
    pub monitor_size: Option<[u32; 2]>,
    /// v0.5.1: the window mode the host asked for when the probe finished: `windowed`,
    /// `borderless_fullscreen`, or empty when unknown.
    pub window_mode: String,
    /// v0.5.1: the host asked for fullscreen but the probe drew at a size smaller than the
    /// monitor (the window had left fullscreen, or was not there yet): its numbers are not the
    /// monitor's, and the probe runs again on the next frame the window fills the monitor.
    pub size_mismatch: bool,
}

/// The lighting option a game shows in its graphics menu (`GameSettings::graphics.lighting`,
/// ADR-133 gi-look), separate from the quality preset.
#[derive(Reflect, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightingMode {
    /// Today's analytic sky and bounce light; every platform.
    Balanced,
    /// GPU-traced bounce light (irradiance probes); desktop only, falls back to `Balanced`
    /// on Android.
    Depth,
}

impl LightingMode {
    /// The renderer's look for this mode.
    #[must_use]
    pub const fn gi_look(self) -> regia_render::GiLook {
        match self {
            Self::Balanced => regia_render::GiLook::Balanced,
            Self::Depth => regia_render::GiLook::Depth,
        }
    }
}

/// The dynamic GI tier a game shows in its graphics menu (`GameSettings::graphics.gi_quality`,
/// `specs/79`, ADR-1679): how large and how fast the GPU irradiance probes are when the
/// lighting option is [`LightingMode::Depth`].
#[derive(Reflect, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GiQuality {
    /// The Deck tier ([`regia_render::DynamicGiTier::Low`]).
    Low,
    /// [`regia_render::DynamicGiTier::Medium`].
    Medium,
    /// The Desktop High tier ([`regia_render::DynamicGiTier::High`]).
    High,
    /// The Desktop Ultra tier ([`regia_render::DynamicGiTier::Ultra`]).
    Ultra,
}

impl GiQuality {
    /// The renderer's tier for this choice.
    #[must_use]
    pub const fn tier(self) -> regia_render::DynamicGiTier {
        match self {
            Self::Low => regia_render::DynamicGiTier::Low,
            Self::Medium => regia_render::DynamicGiTier::Medium,
            Self::High => regia_render::DynamicGiTier::High,
            Self::Ultra => regia_render::DynamicGiTier::Ultra,
        }
    }

    /// The menu choice for a renderer tier.
    #[must_use]
    pub const fn of_tier(tier: regia_render::DynamicGiTier) -> Self {
        match tier {
            regia_render::DynamicGiTier::Low => Self::Low,
            regia_render::DynamicGiTier::Medium => Self::Medium,
            regia_render::DynamicGiTier::High => Self::High,
            regia_render::DynamicGiTier::Ultra => Self::Ultra,
        }
    }
}

/// Graphics section.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
pub struct GraphicsSettings {
    /// Quality preset; individual fields below override it when set.
    pub preset: GraphicsPreset,
    /// Shadow quality multiplier in `0.0…=1.0`; `None` follows the preset. It scales the sun
    /// cascade resolution (to a power of two, at least 256) and the local-light shadow atlas;
    /// `0.0` turns local-light shadows off and drops the sun cascades to 256 (sun shadows stay
    /// on: ADR-2237). Non-finite values are ignored.
    pub shadow_quality: Option<f32>,
    /// Render scale (`0.5` … `1.0`); `None` follows the preset. Clamped to `0.25…=1.0` when
    /// applied; non-finite values are ignored.
    pub render_scale: Option<f32>,
    /// Lighting option; `None` follows the render profile's default. Applied live to
    /// [`regia_render::RenderSettings::gi_look`].
    #[serde(default)]
    pub lighting: Option<LightingMode>,
    /// The player's display calibration; `None` = this platform's out-of-the-box values
    /// ([`regia_render::DisplayCalibration::platform_default`]). Missing in older files.
    #[serde(default)]
    pub calibration: Option<CalibrationSettings>,
    /// Who set `preset`; read it through [`GraphicsSettings::source`]. `None` in files written
    /// before automatic detection: a non-default preset there counts as the player's.
    #[serde(default)]
    pub preset_source: Option<PresetSource>,
    /// The last automatic detection, `None` until one ran.
    #[serde(default)]
    pub detected: Option<DetectedQuality>,
    /// Dynamic GI tier ([`GiQuality`]); `None` follows the render profile's tier. Applied live to
    /// [`regia_render::RenderSettings::dynamic_gi_tier`]. Missing in older files.
    #[serde(default)]
    pub gi_quality: Option<GiQuality>,
}

impl GraphicsSettings {
    /// Who set `preset`. An old file without the field: `Player` when the preset is not the
    /// default (someone chose it), else `Unset`.
    #[must_use]
    pub fn source(&self) -> PresetSource {
        self.preset_source
            .unwrap_or(if self.preset == GraphicsPreset::default() {
                PresetSource::Unset
            } else {
                PresetSource::Player
            })
    }

    /// The player picks `preset` in the options menu: detection never overrides it again (until
    /// they ask for a re-detect).
    pub fn choose_preset(&mut self, preset: GraphicsPreset) {
        self.preset = preset;
        self.preset_source = Some(PresetSource::Player);
    }
}

/// Display calibration the player set on the calibration screen (`graphics.calibration`,
/// book `user-guide/display-calibration.md`). Applied live as the
/// [`regia_render::DisplayCalibration`] resource.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[reflect(Default)]
#[serde(default)]
pub struct CalibrationSettings {
    /// Black level −1..1 (0 neutral): shadow lift or deepen; black, mid grey and white stay.
    pub black_level: f32,
    /// SDR display gamma 1.8–2.6 (2.2 neutral); higher is darker mid-tones.
    pub gamma: f32,
    /// Request HDR10 output (falls back to SDR where the display cannot).
    pub hdr: bool,
    /// HDR paper white (UI and diffuse white), cd/m².
    pub paper_white_nits: f32,
    /// HDR peak, cd/m²; `None` uses what the display reports (else 1000).
    pub peak_nits: Option<f32>,
    /// HDR black, cd/m².
    pub min_nits: f32,
}

impl Default for CalibrationSettings {
    fn default() -> Self {
        Self::from_calibration(&regia_render::DisplayCalibration::NEUTRAL, false)
    }
}

impl CalibrationSettings {
    /// Settings holding `calibration` (its peak kept explicitly).
    #[must_use]
    pub fn from_calibration(calibration: &regia_render::DisplayCalibration, hdr: bool) -> Self {
        Self {
            black_level: calibration.black_level,
            gamma: calibration.gamma,
            hdr,
            paper_white_nits: calibration.paper_white_nits,
            peak_nits: (calibration.peak_nits != regia_render::calibration::DEFAULT_PEAK_NITS)
                .then_some(calibration.peak_nits),
            min_nits: calibration.min_nits,
        }
    }

    /// The renderer's calibration: these values, the display's reported peak where
    /// [`Self::peak_nits`] is `None`, clamped to the player ranges.
    #[must_use]
    pub fn to_calibration(
        &self,
        display: Option<&regia_render::DisplayLuminance>,
    ) -> regia_render::DisplayCalibration {
        let reported = display.and_then(|d| d.peak_nits);
        regia_render::DisplayCalibration {
            black_level: self.black_level,
            gamma: self.gamma,
            paper_white_nits: self.paper_white_nits,
            peak_nits: self
                .peak_nits
                .or(reported)
                .unwrap_or(regia_render::calibration::DEFAULT_PEAK_NITS),
            min_nits: self.min_nits,
        }
        .clamped()
    }
}

/// Display section. Applied live to the primary window (ADR-7880); [`DisplayApplied`] reports
/// what the window and its swapchain took.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[reflect(Default)]
pub struct DisplaySettings {
    /// Windowed resolution in logical pixels; `None` leaves the window at its own size (going back
    /// to `None` restores the size the window had before the setting applied). The platform
    /// window layer clamps it to the monitor; a size it refuses (below
    /// [`regia_platform::MIN_WINDOW_LOGICAL_SIZE`], or on Android, whose window is the screen)
    /// leaves the window as it is, and [`DisplayApplied::refused`] gives the code.
    pub resolution: Option<(u16, u16)>,
    /// Vsync on or off: the primary window's [`regia_platform::PresentMode`]. Off is `Immediate`;
    /// the renderer takes `Immediate`, else `Mailbox`, else FIFO (vsync stays on), whichever the
    /// surface offers, and [`DisplayApplied::present`] reports the result. On is the window's own
    /// vsync mode, and a host's uncapped [`regia_render::PresentPolicy`] becomes `Vsync`. On
    /// Android vsync stays on: frames are released on the display's vsync grid (ADR-5946), so off
    /// is recorded as `android_display_paced` and not applied; lower the rate with `frame_cap`.
    pub vsync: bool,
    /// Frame-rate cap; `None` runs uncapped.
    pub frame_cap: Option<u32>,
    /// Borderless fullscreen on (`true`) or off (`false`); `None` (the default, and in older
    /// files) leaves the window's mode alone, and going back to `None` restores the mode the window
    /// had. Android's window is always fullscreen: `false` is refused (`window_mode_fixed`).
    #[serde(default)]
    pub fullscreen: Option<bool>,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            resolution: None,
            vsync: true,
            frame_cap: None,
            fullscreen: None,
        }
    }
}

/// `DisplayApplied::vsync_note`: vsync off is not applied on Android (ADR-7880).
pub const VSYNC_ANDROID_DISPLAY_PACED: &str = "android_display_paced";
/// `DisplayApplied::vsync_note`: vsync is off but the surface offers no mode without a wait for
/// vertical blank, so it presents with FIFO (ADR-7880).
pub const VSYNC_NO_TEARING_MODE: &str = "no_unsynced_mode_offered";
/// `DisplayApplied::vsync_note`: the host's present policy is variable refresh, which picks its
/// own mode (`Mailbox` when offered) whatever the vsync setting (ADR-7880).
pub const VSYNC_HOST_VRR: &str = "host_vrr_policy";

/// What the display settings did to the primary window and its swapchain (ADR-7880), for an
/// options menu and the agent (`settings.get` answers it as `applied.display`). Updated in `Last`;
/// a change shows up one or two frames after the settings change (the window layer applies it in
/// `PostUpdate`, the renderer reconfigures the swapchain in the next `PreUpdate`).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct DisplayApplied {
    /// The primary window the settings apply to; `None` without one (headless, before the first
    /// resume).
    pub window: Option<Entity>,
    /// The window's requested logical size.
    pub logical_size: Option<(f64, f64)>,
    /// The logical size the window layer passed to the OS (after the monitor clamp).
    pub applied_logical_size: Option<(f64, f64)>,
    /// The window's physical size, as the platform last reported it.
    pub physical_size: Option<(u32, u32)>,
    /// The monitor's logical size, when the OS reports one.
    pub monitor_logical_size: Option<(f64, f64)>,
    /// The window's mode.
    pub mode: Option<regia_platform::WindowMode>,
    /// The last size or mode request the window layer refused: its code
    /// (`regia_platform::WINDOW_*`) and the requested size.
    pub refused: Option<(&'static str, (f64, f64))>,
    /// The window's present mode.
    pub present_mode: Option<regia_platform::PresentMode>,
    /// The swapchain's present modes, once the renderer has a surface for the window.
    pub present: Option<regia_render::swapchain::PresentSummary>,
    /// Why vsync is not what the setting asks for: [`VSYNC_ANDROID_DISPLAY_PACED`],
    /// [`VSYNC_NO_TEARING_MODE`] or [`VSYNC_HOST_VRR`]; `None` when it is.
    pub vsync_note: Option<&'static str>,
}

/// What [`apply_display_settings`] last wrote onto the primary window, and the host's own values
/// it replaced (restored when a setting goes back to `None`, or vsync back on).
#[derive(Resource, Debug, Default)]
struct AppliedDisplay {
    window: Option<Entity>,
    resolution: Option<(u16, u16)>,
    fullscreen: Option<bool>,
    vsync: bool,
    host_size: (f64, f64),
    host_mode: regia_platform::WindowMode,
    host_present: regia_platform::PresentMode,
}

/// The window present mode for a vsync setting (ADR-7880): `host` is the mode the window had
/// before the setting applied. On is `host` when that is a vsync mode, else `Vsync`; off is
/// `Immediate`. On Android off keeps `host` and returns [`VSYNC_ANDROID_DISPLAY_PACED`].
#[must_use]
pub fn vsync_present_mode(
    vsync: bool,
    host: regia_platform::PresentMode,
    android: bool,
) -> (regia_platform::PresentMode, Option<&'static str>) {
    use regia_platform::PresentMode as Mode;
    match (vsync, android) {
        (true, _) => (
            match host {
                Mode::Vsync | Mode::VsyncRelaxed => host,
                Mode::Immediate => Mode::Vsync,
            },
            None,
        ),
        (false, true) => (host, Some(VSYNC_ANDROID_DISPLAY_PACED)),
        (false, false) => (Mode::Immediate, None),
    }
}

/// `Update`, after the apply: writes `display.resolution`, `display.fullscreen` and
/// `display.vsync` onto the primary window when they differ from what was last written (ADR-7880).
/// A new primary window (the first one, or one re-created on resume) gets the settings that differ
/// from the defaults, so a host's own size, mode and present mode stand while the settings are at
/// their defaults.
fn apply_display_settings(
    settings: Res<GameSettings>,
    mut windows: Query<(Entity, &mut regia_platform::Window), With<regia_platform::PrimaryWindow>>,
    mut applied: ResMut<AppliedDisplay>,
    output: Option<ResMut<regia_render::OutputSettings>>,
) {
    let Some((entity, mut window)) = windows.iter_mut().min_by_key(|(entity, _)| *entity) else {
        return;
    };
    let display = &settings.display;
    if applied.window != Some(entity) {
        let defaults = DisplaySettings::default();
        *applied = AppliedDisplay {
            window: Some(entity),
            resolution: defaults.resolution,
            fullscreen: defaults.fullscreen,
            vsync: defaults.vsync,
            host_size: window.logical_size,
            host_mode: window.mode,
            host_present: window.present_mode,
        };
    }
    if display.resolution != applied.resolution {
        let size = display
            .resolution
            .map_or(applied.host_size, |(w, h)| (f64::from(w), f64::from(h)));
        if window.logical_size != size {
            window.logical_size = size;
        }
        applied.resolution = display.resolution;
    }
    if display.fullscreen != applied.fullscreen {
        let mode = match display.fullscreen {
            Some(true) => regia_platform::WindowMode::BorderlessFullscreen,
            Some(false) => regia_platform::WindowMode::Windowed,
            None => applied.host_mode,
        };
        if window.mode != mode {
            window.mode = mode;
        }
        applied.fullscreen = display.fullscreen;
    }
    if display.vsync != applied.vsync {
        let (mode, _) = vsync_present_mode(display.vsync, applied.host_present, cfg!(target_os = "android"));
        if window.present_mode != mode {
            window.present_mode = mode;
        }
        // The player turned vsync on: a host's uncapped policy would ignore the window's mode.
        if display.vsync
            && let Some(mut output) = output
            && output.present == regia_render::PresentPolicy::Uncapped
        {
            output.present = regia_render::PresentPolicy::Vsync;
        }
        applied.vsync = display.vsync;
    }
}

/// `Last`: refreshes [`DisplayApplied`] from the primary window, its [`regia_platform::WindowStatus`]
/// and its swapchain. Writes only on a difference and allocates nothing.
fn report_display_applied(
    settings: Res<GameSettings>,
    windows: Query<
        (
            Entity,
            &regia_platform::Window,
            Option<&regia_platform::WindowStatus>,
        ),
        With<regia_platform::PrimaryWindow>,
    >,
    surfaces: Option<Res<regia_render::WindowSurfaces>>,
    output: Option<Res<regia_render::OutputSettings>>,
    mut report: ResMut<DisplayApplied>,
) {
    let window = windows.iter().min_by_key(|(entity, ..)| *entity);
    let next = match window {
        None => DisplayApplied::default(),
        Some((entity, window, status)) => {
            let present = surfaces.as_deref().and_then(|s| s.present_summary(entity));
            let vrr = output
                .as_deref()
                .is_some_and(|o| o.present == regia_render::PresentPolicy::Vrr);
            DisplayApplied {
                window: Some(entity),
                logical_size: Some(window.logical_size),
                applied_logical_size: status.and_then(|s| s.applied_logical_size),
                physical_size: Some(window.physical_size),
                monitor_logical_size: status.and_then(|s| s.monitor.map(|m| m.logical_size())),
                mode: Some(window.mode),
                refused: status
                    .and_then(|s| s.refused.as_ref())
                    .map(|r| (r.code, r.requested_size)),
                present_mode: Some(window.present_mode),
                present,
                vsync_note: vsync_note(settings.display.vsync, cfg!(target_os = "android"), vrr, present),
            }
        }
    };
    if *report != next {
        *report = next;
    }
}

/// Why vsync differs from the setting (`DisplayApplied::vsync_note`).
fn vsync_note(
    vsync: bool,
    android: bool,
    vrr: bool,
    present: Option<regia_render::swapchain::PresentSummary>,
) -> Option<&'static str> {
    if vsync {
        return None;
    }
    if android {
        return Some(VSYNC_ANDROID_DISPLAY_PACED);
    }
    if vrr {
        return Some(VSYNC_HOST_VRR);
    }
    present
        .filter(|p| p.waits_for_vblank)
        .map(|_| VSYNC_NO_TEARING_MODE)
}

/// Audio section. Volumes are linear multipliers `0.0…=1.0` (the bus system lands with `specs/16`;
/// the sample reads `master` live through [`GameSettings`]).
/// They default to `1.0` (full volume) and are applied live to the `Master`, `Music`, `Sfx`,
/// `Voice` and `Ambience` buses of [`regia_audio::Audio`] when they change (linear → dB,
/// `20·log10(v)`, silence at `0`), with each bus's mute. `World` and `Ui` stay runtime-only.
/// Older files omit `voice`, `ambience` and the mutes; those load at full volume and unmuted.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[reflect(Default)]
pub struct AudioBusSettings {
    /// Master output volume.
    pub master: f32,
    /// Music bus volume.
    pub music: f32,
    /// SFX bus volume.
    pub sfx: f32,
    /// Voice bus volume. Missing in older files (`1`).
    #[serde(default = "unit_scale")]
    pub voice: f32,
    /// Ambience bus volume. Missing in older files (`1`).
    #[serde(default = "unit_scale")]
    pub ambience: f32,
    /// Mute the Master bus. Missing in older files (off).
    #[serde(default)]
    pub mute_master: bool,
    /// Mute the Music bus. Missing in older files (off).
    #[serde(default)]
    pub mute_music: bool,
    /// Mute the Sfx bus. Missing in older files (off).
    #[serde(default)]
    pub mute_sfx: bool,
    /// Mute the Voice bus. Missing in older files (off).
    #[serde(default)]
    pub mute_voice: bool,
    /// Mute the Ambience bus. Missing in older files (off).
    #[serde(default)]
    pub mute_ambience: bool,
}

impl Default for AudioBusSettings {
    fn default() -> Self {
        Self {
            master: 1.0,
            music: 1.0,
            sfx: 1.0,
            voice: 1.0,
            ambience: 1.0,
            mute_master: false,
            mute_music: false,
            mute_sfx: false,
            mute_voice: false,
            mute_ambience: false,
        }
    }
}

/// A linear bus volume as dB (`-80` dB, the floor, at or below zero or for a non-finite value).
fn linear_to_db(volume: f32) -> f32 {
    if volume.is_finite() && volume > 0.0 {
        (20.0 * volume.log10()).max(-80.0)
    } else {
        -80.0
    }
}

/// Pushes `audio` onto the audio buses when it differs from the last-seen settings (on the
/// baseline frame only when it differs from the defaults, so a host's own bus levels survive).
fn apply_audio(world: &mut World, audio: &AudioBusSettings, before: Option<&AudioBusSettings>) {
    let changed = match before {
        Some(before) => before != audio,
        None => *audio != AudioBusSettings::default(),
    };
    if !changed {
        return;
    }
    let Some(mut out) = world.get_resource_mut::<regia_audio::Audio>() else {
        return;
    };
    for (bus, volume, mute) in [
        ("Master", audio.master, audio.mute_master),
        ("Music", audio.music, audio.mute_music),
        ("Sfx", audio.sfx, audio.mute_sfx),
        ("Voice", audio.voice, audio.mute_voice),
        ("Ambience", audio.ambience, audio.mute_ambience),
    ] {
        // A layout without the bus is the game's choice; nothing to apply.
        let _ = out.set_bus_volume(bus, linear_to_db(volume), std::time::Duration::ZERO);
        let _ = out.set_bus_mute(bus, mute);
    }
}

/// Input section: the cross-game knobs every settings menu exposes.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[reflect(Default)]
pub struct InputSettings {
    /// Look/mouse sensitivity multiplier (`1.0` = default). Applied to actions named in
    /// [`regia_input::InputConfig::look_actions`]. With that list empty the engine does not read
    /// this field; the game reads it (ADR-3464).
    pub sensitivity: f32,
    /// Invert the look Y axis. Applied with [`Self::sensitivity`] when look actions are named;
    /// otherwise the game reads it (ADR-3464).
    pub invert_y: bool,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            sensitivity: 1.0,
            invert_y: false,
        }
    }
}

/// The player's light/dark choice for the UI theme (`docs/specs/ui-default-design.md` § 6.4): it
/// drives `regia_ui::theme_runtime::ThemeMode::follow`.
#[derive(Reflect, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColorSchemeSetting {
    /// Always the dark theme.
    Dark,
    /// Always the light theme.
    Light,
    /// Follow the OS appearance (dark until the platform reports one).
    System,
}

impl ColorSchemeSetting {
    /// The UI preference this setting selects.
    pub fn preference(self) -> regia_ui::theme_runtime::SchemePreference {
        use regia_ui::theme_runtime::SchemePreference;
        match self {
            Self::Dark => SchemePreference::Dark,
            Self::Light => SchemePreference::Light,
            Self::System => SchemePreference::System,
        }
    }
}

/// Accessibility section.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[reflect(Default)]
pub struct AccessibilitySettings {
    /// Show subtitles for spoken lines.
    pub subtitles: bool,
    /// UI text scale (`1.0` = default). The UI limits it to 0.5 through 3.0
    /// ([`regia_ui::UiAccessibility::effective_text_scale`]).
    pub text_scale: f32,
    /// Animations jump to their end value instead of playing. Missing in older files (off).
    #[serde(default)]
    pub reduced_motion: bool,
    /// Themes pick their high-contrast colors. Missing in older files (off).
    #[serde(default)]
    pub high_contrast: bool,
    /// The player's light/dark choice. `None` (the default, and in older files) leaves the theme
    /// to the game; `Some` sets the UI's `ThemeMode::follow`.
    #[serde(default)]
    pub color_scheme: Option<ColorSchemeSetting>,
    /// Which captions show (spec 36 § 9). `None` (the default, and in older files) derives from
    /// `subtitles`: `true` is [`SubtitleMode::Dialogue`], `false` is [`SubtitleMode::Off`]. Set it with
    /// [`AccessibilitySettings::set_subtitle_mode`], which keeps `subtitles` in sync.
    #[serde(default)]
    pub subtitle_mode: Option<SubtitleMode>,
    /// Per-action input assists by action name: hold or toggle, a minimum hold time and
    /// auto-repeat (ADR-2646). Copied into [`regia_input::InputAssists`]. Missing in older files
    /// (none).
    #[serde(default)]
    #[reflect(ignore)]
    pub input_assists: std::collections::BTreeMap<String, regia_input::ActionAssist>,
    /// The whole audio output downmixed to mono (ADR-2646, [`regia_audio::Audio::set_mono_output`]).
    /// Missing in older files (off).
    #[serde(default)]
    pub mono_audio: bool,
    /// A color-vision filter on the rendered image: simulation or compensation of protanopia,
    /// deuteranopia or tritanopia (ADR-2646). Copied into
    /// [`regia_render::color_vision::ColorVision`]. Missing in older files (off).
    #[serde(default)]
    #[reflect(ignore)]
    pub color_vision: regia_render::color_vision::ColorVisionFilter,
    /// Gamepad rumble intensity, `0` (off) to `1` (as the game asks; ADR-2646). Copied into
    /// [`regia_input::Gamepads::set_rumble_scale`]. Missing in older files (`1`).
    #[serde(default = "full_rumble")]
    pub rumble_scale: f32,
    /// Camera shake scale, `0` (off) to `1` (the authored shake). [`Self::reduced_motion`] forces
    /// the effective scale to `0` without writing this field. Missing in older files (`1`).
    /// ADR-3464.
    #[serde(default = "unit_scale")]
    pub camera_shake_scale: f32,
    /// Motion-blur allow bit on the render profile. `None` (the default, and older files) leaves
    /// the profile's bit alone. `Some(false)` forces it off; `Some(true)` allows it.
    /// [`Self::reduced_motion`] forces it off while set. ADR-3464.
    #[serde(default)]
    pub motion_blur: Option<bool>,
    /// Gameplay-camera field of view in degrees. `None` keeps the camera's authored value.
    /// Clamped to [`SceneMotion::FOV_MIN_DEG`]..=[`SceneMotion::FOV_MAX_DEG`] when applied.
    /// Shots and the sequencer keep their own framing. ADR-3464.
    #[serde(default)]
    pub fov_override_deg: Option<f32>,
    /// Phone vibration scale, `0` (off) to `1`. Not tied to [`Self::reduced_motion`]. Copied into
    /// [`regia_input::PhoneVibration`]. Missing in older files (`1`). ADR-3464.
    #[serde(default = "unit_scale")]
    pub vibration_scale: f32,
    /// Per-player input assists, overlaid on [`Self::input_assists`]. Missing in older files.
    #[serde(default)]
    #[reflect(ignore)]
    pub input_assists_by_player:
        std::collections::BTreeMap<u8, std::collections::BTreeMap<String, regia_input::ActionAssist>>,
    /// Per-gamepad input assists, overlaid under the player map and over the default set.
    /// Missing in older files.
    #[serde(default)]
    #[reflect(ignore)]
    pub input_assists_by_gamepad:
        std::collections::BTreeMap<u64, std::collections::BTreeMap<String, regia_input::ActionAssist>>,
}

fn full_rumble() -> f32 {
    1.0
}

fn unit_scale() -> f32 {
    1.0
}

/// The player's caption choice (spec 36 § 9): nothing, spoken lines, or spoken lines and sound captions.
#[derive(Reflect, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SubtitleMode {
    /// No captions.
    Off,
    /// Speech and narration captions.
    #[default]
    Dialogue,
    /// Speech, narration and sound captions (`[door slams]`); speakers are always labeled.
    All,
}

impl SubtitleMode {
    /// `off`, `dialogue` or `all`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Dialogue => "dialogue",
            Self::All => "all",
        }
    }

    /// Parses [`SubtitleMode::as_str`] (ignoring case).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "dialogue" => Some(Self::Dialogue),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

impl AccessibilitySettings {
    /// A first run follows the system captions preference (spec 36 § 9, ADR-1882): a player who
    /// turned captions on in the OS gets [`SubtitleMode::All`]. The OS setting left off says
    /// nothing about subtitles, so the default [`SubtitleMode::Dialogue`] stands.
    pub fn follow_system_captions(&mut self, system: Option<bool>) {
        if system == Some(true) {
            self.set_subtitle_mode(SubtitleMode::All);
        }
    }

    /// The caption mode in effect: `subtitle_mode`, or the one `subtitles` implies.
    #[must_use]
    pub fn effective_subtitle_mode(&self) -> SubtitleMode {
        self.subtitle_mode.unwrap_or(if self.subtitles {
            SubtitleMode::Dialogue
        } else {
            SubtitleMode::Off
        })
    }

    /// Sets the caption mode and keeps `subtitles` in sync (`Off` writes `false`).
    pub fn set_subtitle_mode(&mut self, mode: SubtitleMode) {
        self.subtitle_mode = Some(mode);
        self.subtitles = mode != SubtitleMode::Off;
    }
}

impl Default for AccessibilitySettings {
    fn default() -> Self {
        Self {
            subtitles: true,
            text_scale: 1.0,
            reduced_motion: false,
            high_contrast: false,
            color_scheme: None,
            subtitle_mode: None,
            input_assists: std::collections::BTreeMap::new(),
            mono_audio: false,
            color_vision: regia_render::color_vision::ColorVisionFilter::default(),
            rumble_scale: 1.0,
            camera_shake_scale: 1.0,
            motion_blur: None,
            fov_override_deg: None,
            vibration_scale: 1.0,
            input_assists_by_player: std::collections::BTreeMap::new(),
            input_assists_by_gamepad: std::collections::BTreeMap::new(),
        }
    }
}

/// The persisted, live-applied game configuration (§ B7).
///
/// Field paths for the agent protocol use reflection paths, e.g. `input.sensitivity`,
/// `display.frame_cap`, `audio.master`.
///
/// # Example
///
/// ```
/// use regia_framework::settings::GameSettings;
///
/// let mut settings = GameSettings::default();
/// settings.input.sensitivity = 2.5;
/// let text = settings.to_ron().expect("serializes");
/// let back = GameSettings::from_ron(&text).expect("parses");
/// assert_eq!(back.input.sensitivity, 2.5);
/// ```
#[derive(Resource, Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
pub struct GameSettings {
    /// Graphics quality.
    pub graphics: GraphicsSettings,
    /// Window and frame rate.
    pub display: DisplaySettings,
    /// Bus volumes.
    pub audio: AudioBusSettings,
    /// Look sensitivity and inversions.
    pub input: InputSettings,
    /// BCP-47 language tag ("" = platform default).
    pub language: String,
    /// Accessibility basics.
    pub accessibility: AccessibilitySettings,
    /// The player's performance overlay choice ([`crate::perf_overlay`]); `None` follows the
    /// host's default ([`crate::perf_overlay::PerformanceOverlay::default_mode`]). Missing in
    /// older files.
    #[serde(default)]
    pub performance_overlay: Option<crate::perf_overlay::PerformanceOverlayMode>,
    /// BCP-47 tag of the spoken-audio language (spec 36 § 5); `""` (the default, and in older files)
    /// follows the text language. Applied with `Localization::set_audio_locale`.
    #[serde(default)]
    pub audio_language: String,
    /// The player's caption presentation (spec 56 § 2.8, `regia_ui::captions::CaptionOptions`).
    /// `None` (the default, and in older files) keeps the presenter's defaults. The caption mode of
    /// `accessibility` decides `enabled` and `caption_mode` either way.
    #[serde(default)]
    pub captions: Option<regia_ui::captions::CaptionOptions>,
    /// Extension settings (spec 57 § 3.3.5): extension id to the RON text of its declared settings
    /// resource, written when the resource changes and applied over `[extensions.settings.<id>]`
    /// of `Regia.toml` at the next start. Empty (the default, and in older files) keeps the
    /// project table's values; an empty map is not written (ADR-7237).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extensions: std::collections::BTreeMap<String, String>,
}

impl GameSettings {
    /// The caption options these settings give over `base` (the presenter's current options): the
    /// saved `captions` when present, else `base`, with `enabled` and `caption_mode` taken from the
    /// accessibility caption mode.
    #[must_use]
    pub fn caption_options(
        &self,
        base: &regia_ui::captions::CaptionOptions,
    ) -> regia_ui::captions::CaptionOptions {
        let mut options = self.captions.clone().unwrap_or_else(|| base.clone());
        let mode = self.accessibility.effective_subtitle_mode();
        options.enabled = mode != SubtitleMode::Off;
        options.caption_mode = mode == SubtitleMode::All;
        options
    }

    /// Stores caption options the player chose (`captions.set_options` with `persist`): `captions`
    /// gets them and the accessibility caption mode follows `enabled` and `caption_mode`.
    pub fn store_caption_options(&mut self, options: &regia_ui::captions::CaptionOptions) {
        let mode = match (options.enabled, options.caption_mode) {
            (false, _) => SubtitleMode::Off,
            (true, false) => SubtitleMode::Dialogue,
            (true, true) => SubtitleMode::All,
        };
        self.accessibility.set_subtitle_mode(mode);
        self.captions = Some(options.clone());
    }
}

impl GameSettings {
    /// Serialises to canonical RON text.
    pub fn to_ron(&self) -> Result<String, ron::Error> {
        ron::to_string(self)
    }

    /// Parses RON text produced by [`GameSettings::to_ron`]. Unknown fields are **silently
    /// dropped** (a plain `ron::from_str` with no `deny_unknown_fields`): a settings file from a
    /// newer game version loads, but its unknown sections are lost rather than reported.
    pub fn from_ron(text: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(text)
    }

    /// Parses RON produced by [`GameSettings::to_ron`], rejecting unknown **top-level** fields.
    ///
    /// [`GameSettings::from_ron`] silently drops unknown fields, so a settings file written by a
    /// newer game version loads but loses its unknown sections with no signal. This strict variant
    /// errors with [`GameSettingsStrictError::UnknownField`] instead, so a caller can detect and
    /// warn about forward-incompatible files. Only the top level is checked: an unknown field
    /// *inside* a known section (e.g. a new `graphics.*` knob) is still dropped.
    pub fn from_ron_strict(text: &str) -> Result<Self, GameSettingsStrictError> {
        // Reject unknown top-level fields first: parse as a `ron::Value` map and check the keys
        // against the struct's fields (the round-trip test below keeps this list in sync — a new
        // field would otherwise be rejected here).
        let value: ron::Value = ron::from_str(text).map_err(GameSettingsStrictError::Parse)?;
        if let ron::Value::Map(map) = &value {
            for key in map.keys() {
                if let ron::Value::String(field) = key
                    && !TOP_LEVEL_FIELDS.contains(&field.as_str())
                {
                    return Err(GameSettingsStrictError::UnknownField(field.clone()));
                }
            }
        }
        Self::from_ron(text).map_err(GameSettingsStrictError::Parse)
    }
}

/// The top-level fields of [`GameSettings`], in source order. `from_ron_strict` rejects any other
/// top-level key; keep this list in sync with the struct (the round-trip test guards it).
const TOP_LEVEL_FIELDS: [&str; 10] = [
    "graphics",
    "display",
    "audio",
    "input",
    "language",
    "accessibility",
    "performance_overlay",
    "audio_language",
    "captions",
    "extensions",
];

/// Failures of [`GameSettings::from_ron_strict`].
#[derive(thiserror::Error, Debug)]
pub enum GameSettingsStrictError {
    /// The RON could not be parsed (or the parsed value did not match the schema).
    #[error("settings RON: {0}")]
    Parse(#[from] ron::error::SpannedError),
    /// A top-level field that is not part of [`GameSettings`].
    #[error("unknown settings field `{0}`")]
    UnknownField(String),
}

/// Raised (as an ECS message) once per applied settings change, after the new values are live.
/// Menus play a click, re-layout for `text_scale`, etc.
#[derive(Message, Debug, Clone)]
pub struct SettingsChanged {
    /// The settings **before** this change.
    pub before: GameSettings,
    /// The settings now (also readable as the [`GameSettings`] resource).
    pub after: GameSettings,
}

/// Raised (as an ECS message) for each settings write that failed (disk full, permission denied,
/// a read-only directory): the file on disk keeps its previous value. Writes happen only on a
/// change and are rate-limited ([`SettingsPersistence`]), so a failure is not retried every frame
/// (ADR-3102).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct SettingsWriteFailed {
    /// The settings file.
    pub path: PathBuf,
    /// What went wrong.
    pub error: String,
}

/// Where settings persist. Defaults to `user://settings.ron`
/// (`regia_platform::paths::user_dir()`); tests point it at a temp directory, and `None`
/// disables persistence entirely (pure in-memory live settings).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct SettingsPath(pub Option<PathBuf>);

// On Android the body is `Self(None)`, which clippy would derive; desktop needs the manual impl.
#[cfg_attr(target_os = "android", allow(clippy::derivable_impls))]
impl Default for SettingsPath {
    fn default() -> Self {
        // Desktop: `user://settings.ron`. Android has no `user_dir` until the shell has an
        // `AndroidApp` (internal data path); leave persistence off so settings stay live
        // in-memory until the shell inserts a concrete `SettingsPath`.
        #[cfg(not(target_os = "android"))]
        {
            Self(regia_platform::paths::user_dir().map(|dir| dir.join("settings.ron")))
        }
        #[cfg(target_os = "android")]
        {
            Self(None)
        }
    }
}

/// Rate limit of settings writes. A change is written at once when the last write is older than
/// `min_interval`; otherwise it waits, and only the newest value is written once the interval has
/// passed (an options-menu slider that changes [`GameSettings`] every frame costs one write and
/// fsync per interval, not one per frame). A pending value is also written when the app sends
/// [`AppExit`] and when the world is dropped.
#[derive(Resource, Debug)]
pub struct SettingsPersistence {
    /// Shortest time between two writes (default 0.5 s).
    pub min_interval: std::time::Duration,
    last_write: Option<std::time::Instant>,
    pending: Option<(PathBuf, GameSettings)>,
    writes: u64,
    /// Why writes are off this session: the settings file holds the player's data and could not
    /// be read or moved aside, so it must not be written over (ADR-3102).
    blocked: Option<String>,
    /// Failed writes so far.
    failures: u64,
    /// The last write failed (one warn line per streak of failures).
    failing: bool,
    /// Failures not yet sent as [`SettingsWriteFailed`].
    unreported: Vec<SettingsWriteFailed>,
}

impl Default for SettingsPersistence {
    fn default() -> Self {
        Self {
            min_interval: std::time::Duration::from_millis(500),
            last_write: None,
            pending: None,
            writes: 0,
            blocked: None,
            failures: 0,
            failing: false,
            unreported: Vec::new(),
        }
    }
}

impl SettingsPersistence {
    /// Settings writes made so far (attempted, including failed ones).
    #[must_use]
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// Whether a change is waiting for the rate limit.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Settings writes that failed so far (each one sent a [`SettingsWriteFailed`]).
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures
    }

    /// Why settings writes are off this session, if they are: the file could not be read or
    /// moved aside at start-up, and it is never written over while it may hold the player's
    /// data (ADR-3102).
    #[must_use]
    pub fn blocked(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    /// Writes `settings` now when the interval allows, else keeps it as the pending value.
    fn request(&mut self, path: PathBuf, settings: GameSettings) {
        if self.due() {
            self.write(&path, &settings);
        } else {
            self.pending = Some((path, settings));
        }
    }

    fn due(&self) -> bool {
        self.last_write.is_none_or(|at| at.elapsed() >= self.min_interval)
    }

    /// Writes the pending value, if any.
    pub fn flush(&mut self) {
        if let Some((path, settings)) = self.pending.take() {
            self.write(&path, &settings);
        }
    }

    fn write(&mut self, path: &std::path::Path, settings: &GameSettings) {
        self.pending = None;
        if self.blocked.is_some() {
            return;
        }
        self.last_write = Some(std::time::Instant::now());
        self.writes += 1;
        match persist(settings, path) {
            Ok(()) => {
                if self.failing {
                    tracing::info!(target: "regia_framework", "settings saved to {path:?} again");
                }
                self.failing = false;
            }
            Err(err) => {
                self.failures += 1;
                if !self.failing {
                    tracing::warn!(
                        target: "regia_framework",
                        "cannot persist settings to {path:?}: {err}; the file keeps its previous value"
                    );
                }
                self.failing = true;
                self.unreported.push(SettingsWriteFailed {
                    path: path.to_path_buf(),
                    error: err.to_string(),
                });
            }
        }
    }
}

/// `Update` and `Last`: sends one [`SettingsWriteFailed`] per failed write.
fn report_settings_write_failures(world: &mut World) {
    let failed = match world.get_resource_mut::<SettingsPersistence>() {
        Some(mut persistence) if !persistence.unreported.is_empty() => {
            std::mem::take(&mut persistence.unreported)
        }
        _ => return,
    };
    for message in failed {
        world.write_message(message);
    }
}

/// What reading `settings.ron` at start-up found (ADR-3102).
#[derive(Debug, Default)]
struct LoadedSettings {
    /// The settings the file held; `None` when there is no usable file (defaults load).
    settings: Option<GameSettings>,
    /// Why writes must stay off this session (the file may hold the player's data and could not
    /// be moved or copied aside).
    blocked: Option<String>,
}

/// Reads `settings.ron` at start-up. A missing file is a first run. A damaged file (not UTF-8, not
/// RON, empty) is moved aside to `settings.ron.bad-<n>` and defaults load; a file with top-level
/// fields this build does not know (a newer game wrote it) loads the known fields and is copied
/// aside, so the newer sections survive the next write. A file that cannot be read, moved or
/// copied keeps settings writes off for the session. Each case logs one warn line naming the file.
fn load_settings_file(path: &std::path::Path) -> LoadedSettings {
    const NOT_SAVED: &str = "settings changes are not saved this session so the file is not written over";
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return LoadedSettings::default(),
        Err(err) => {
            tracing::warn!(
                target: "regia_framework",
                "cannot read settings {path:?}: {err}; using defaults, and {NOT_SAVED}"
            );
            return LoadedSettings {
                settings: None,
                blocked: Some(format!("cannot read {}: {err}", path.display())),
            };
        }
    };
    let parsed = match std::str::from_utf8(&bytes) {
        Ok(text) => GameSettings::from_ron(text)
            .map(|settings| (settings, text))
            .map_err(|err| err.to_string()),
        Err(err) => Err(format!("not UTF-8 text at byte {}", err.valid_up_to())),
    };
    match parsed {
        Ok((settings, text)) => {
            let mut blocked = None;
            if let Err(GameSettingsStrictError::UnknownField(field)) = GameSettings::from_ron_strict(text) {
                match crate::user_file::copy_aside(path, &bytes) {
                    Ok(copy) => tracing::warn!(
                        target: "regia_framework",
                        "settings file {path:?} has a field this build does not know (`{field}`, from a newer version); \
                         loaded the known settings and kept the whole file as {copy:?}"
                    ),
                    Err(err) => {
                        tracing::warn!(
                            target: "regia_framework",
                            "settings file {path:?} has a field this build does not know (`{field}`, from a newer version) \
                             and cannot be copied aside ({err}); loaded the known settings, and {NOT_SAVED}"
                        );
                        blocked = Some(format!("cannot copy {} aside: {err}", path.display()));
                    }
                }
            }
            LoadedSettings {
                settings: Some(settings),
                blocked,
            }
        }
        Err(why) => match crate::user_file::move_aside(path) {
            Ok(moved) => {
                tracing::warn!(
                    target: "regia_framework",
                    "settings file {path:?} is damaged ({why}); moved it to {moved:?} and using defaults"
                );
                LoadedSettings::default()
            }
            Err(err) => {
                tracing::warn!(
                    target: "regia_framework",
                    "settings file {path:?} is damaged ({why}) and cannot be moved aside ({err}); using defaults, and {NOT_SAVED}"
                );
                LoadedSettings {
                    settings: None,
                    blocked: Some(format!("cannot move {} aside: {err}", path.display())),
                }
            }
        },
    }
}

impl Drop for SettingsPersistence {
    fn drop(&mut self) {
        self.flush();
    }
}

/// `Update`, after the apply: writes a pending settings value once the rate limit allows.
fn persist_pending_settings(mut persistence: ResMut<SettingsPersistence>) {
    if persistence.pending.is_some() && persistence.due() {
        persistence.flush();
    }
}

/// `Last`: writes a pending settings value when the app is exiting.
fn persist_settings_on_exit(mut exit: MessageReader<AppExit>, mut persistence: ResMut<SettingsPersistence>) {
    if exit.read().next().is_some() {
        persistence.flush();
    }
}

/// The settings systems, for ordering (automatic quality detection runs before
/// [`SettingsSystems::Apply`]).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsSystems {
    /// Applies a changed [`GameSettings`] live and persists it.
    Apply,
}

/// Set for one frame by automatic detection when it writes `graphics.preset`, so the change is
/// not taken for the player's choice.
#[derive(Resource, Debug, Default)]
pub(crate) struct DetectorWrote;

/// The graphics preset last applied to `RenderSettings` (re-applied only when it changes, so a
/// volume change never resets the game's profile tweaks), and whether its GPU-dependent levers
/// still wait for the render device ([`decide_gpu_levers`]).
#[derive(Resource, Debug, Default)]
struct AppliedPreset(Option<PresetChoice>, bool);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresetChoice {
    Tier(GraphicsPreset),
    Packaged(regia_render::ProfileName),
}

/// The `accessibility.color_scheme` last copied into `ThemeMode::follow`: it is copied only when
/// it changes, so an unrelated settings change never overrides a theme the game or an agent
/// selected since.
#[derive(Resource, Debug, Default)]
struct AppliedColorScheme(Option<ColorSchemeSetting>);

/// Loads, applies and persists [`GameSettings`] (see the module docs).
pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<GameSettings>()
            .register_type::<GraphicsSettings>()
            .register_type::<CalibrationSettings>()
            .register_type::<DisplaySettings>()
            .register_type::<AudioBusSettings>()
            .register_type::<InputSettings>()
            .register_type::<AccessibilitySettings>()
            .register_type::<ColorSchemeSetting>()
            .register_type::<Option<ColorSchemeSetting>>()
            .register_type::<SubtitleMode>()
            .register_type::<Option<SubtitleMode>>()
            .register_type::<regia_ui::captions::CaptionOptions>()
            .register_type::<Option<regia_ui::captions::CaptionOptions>>()
            .register_type::<std::collections::BTreeMap<String, String>>()
            .register_type::<GraphicsPreset>()
            .register_type::<LightingMode>()
            .register_type::<Option<LightingMode>>()
            .register_type::<PresetSource>()
            .register_type::<Option<PresetSource>>()
            .register_type::<DetectedQuality>()
            .register_type::<Option<DetectedQuality>>()
            .register_type::<ProbeRecord>()
            .register_type::<Option<ProbeRecord>>()
            .init_resource::<AppliedPreset>()
            .init_resource::<AppliedOverrides>()
            .init_resource::<AppliedColorScheme>()
            .init_resource::<AppliedDisplay>()
            .init_resource::<DisplayApplied>()
            .register_type::<Option<bool>>()
            .register_type::<crate::perf_overlay::PerformanceOverlayMode>()
            .register_type::<Option<crate::perf_overlay::PerformanceOverlayMode>>()
            .add_message::<SettingsChanged>()
            .add_message::<SettingsWriteFailed>()
            .init_resource::<SettingsPersistence>()
            .init_resource::<SettingsPath>();

        // Load before the first frame; a missing file → defaults (a fresh install must boot), a
        // damaged file → moved aside plus defaults and a warning, never written over (ADR-3102).
        let path = app.world().resource::<SettingsPath>().0.clone();
        let mut settings = GameSettings::default();
        let mut first_run = true;
        if let Some(path) = &path {
            let loaded = load_settings_file(path);
            if let Some(loaded) = loaded.settings {
                settings = loaded;
                first_run = false;
            }
            if loaded.blocked.is_some() {
                app.world_mut().resource_mut::<SettingsPersistence>().blocked = loaded.blocked;
            }
        }
        if first_run {
            settings
                .accessibility
                .follow_system_captions(regia_platform::system_captions());
        }
        // The saved language is the third start-up locale source (ADR-1618); `regia_ui` reads the
        // hint in its `Startup` system, after every plugin has been built.
        if !settings.language.trim().is_empty() {
            app.world_mut()
                .get_resource_or_insert_with(regia_platform::LocaleHints::default)
                .settings = Some(settings.language.clone());
        }
        app.add_message::<regia_ui::LocaleWriteBack>();
        app.add_systems(
            Update,
            (
                write_back_locale
                    .after(regia_ui::locale_startup::apply_player_locale)
                    .before(SettingsSystems::Apply),
                follow_language_setting.after(SettingsSystems::Apply),
                apply_display_settings.after(SettingsSystems::Apply),
                follow_audio_language_setting.after(SettingsSystems::Apply),
                apply_caption_settings.after(SettingsSystems::Apply),
            ),
        );
        app.insert_resource(settings.clone());
        // `RenderSettings` always exists from plugin build on, so a host `Startup` system may take
        // `ResMut<RenderSettings>` on a first launch too (no settings file, preset `Unset`). The
        // game's own settings stand: one inserted before this plugin is kept, one inserted later
        // replaces this default. Detection (or the player) applies its preset over it later.
        if !app.world().contains_resource::<regia_render::RenderSettings>() {
            app.insert_resource(regia_render::RenderSettings::default());
        }
        // A saved preset goes to the renderer now, before any host `Startup` system: a host that
        // tweaks `RenderSettings.profile` at startup (render scale, upscaler) keeps its tweaks,
        // because the `Update` apply re-applies the preset only when it changes.
        crate::packaged_config::ensure_loaded(app.world_mut());
        apply_preset(app.world_mut(), &settings);
        // Baseline captured at build: a change made in `Startup` (or by the next system before
        // the first `Update`) is a real change, not the baseline.
        app.insert_resource(SettingsLastSeen(settings));
        app.add_systems(
            Update,
            (
                apply_and_persist_settings,
                decide_gpu_levers,
                persist_pending_settings,
                report_settings_write_failures,
            )
                .chain()
                .in_set(SettingsSystems::Apply),
        )
        .add_systems(
            Last,
            (
                (persist_settings_on_exit, report_settings_write_failures).chain(),
                report_display_applied,
            ),
        );
    }
}

/// `Update`: stores a player's language choice ([`regia_ui::LocaleWriteBack`]) in
/// `GameSettings::language`, so the next start-up picks it (the saved language outranks the Steam
/// and OS languages). Agent-driven switches send no such message.
fn write_back_locale(
    mut messages: MessageReader<regia_ui::LocaleWriteBack>,
    mut settings: ResMut<GameSettings>,
) {
    for message in messages.read() {
        if settings.language != message.locale {
            settings.language.clone_from(&message.locale);
        }
    }
}

/// `Update`, after the apply: a language set through `settings.set` or an options menu selects
/// that locale live. The first frame only records the baseline (start-up already used it).
fn follow_language_setting(
    settings: Res<GameSettings>,
    localization: Option<ResMut<regia_ui::Localization>>,
    mut seen: Local<Option<String>>,
) {
    let Some(previous) = seen.as_deref() else {
        *seen = Some(settings.language.clone());
        return;
    };
    if previous == settings.language {
        return;
    }
    *seen = Some(settings.language.clone());
    let Some(mut localization) = localization else {
        return;
    };
    if let Some(matched) = regia_ui::locale_startup::match_loaded(&localization, &settings.language) {
        localization.set_locale(&matched);
    }
}

/// `Update`, after the apply: the audio language setting sets (or, empty, clears) the audio locale
/// override. Unlike the text language there is no start-up path for it, so the first frame applies it.
fn follow_audio_language_setting(
    settings: Res<GameSettings>,
    localization: Option<ResMut<regia_ui::Localization>>,
    mut seen: Local<Option<String>>,
) {
    let Some(mut localization) = localization else {
        return;
    };
    if seen.as_deref() == Some(settings.audio_language.as_str()) {
        return;
    }
    *seen = Some(settings.audio_language.clone());
    let tag = Some(settings.audio_language.as_str()).filter(|t| !t.trim().is_empty());
    if localization.audio_override() == tag {
        return;
    }
    if let Err(err) = localization.set_audio_locale(tag) {
        tracing::warn!(target: "regia_framework", "audio_language {:?}: {err}", settings.audio_language);
    }
}

/// What [`apply_caption_settings`] last applied: the saved options and the caption mode.
type CaptionSettingsKey = (Option<regia_ui::captions::CaptionOptions>, SubtitleMode);

/// `Update`, after the apply: the saved caption options and caption mode reach the presenter's
/// `CaptionOptions` on the first frame and whenever they change in the settings. An unrelated settings
/// change never overrides options a game or an agent set since. A game without the presenter is left
/// alone.
fn apply_caption_settings(
    settings: Res<GameSettings>,
    options: Option<ResMut<regia_ui::captions::CaptionOptions>>,
    mut seen: Local<Option<CaptionSettingsKey>>,
) {
    let Some(mut options) = options else {
        return;
    };
    let key = (
        settings.captions.clone(),
        settings.accessibility.effective_subtitle_mode(),
    );
    if seen.as_ref() == Some(&key) {
        return;
    }
    *seen = Some(key);
    let wanted = settings.caption_options(&options);
    if *options != wanted {
        *options = wanted;
    }
}

/// `Update`: when [`GameSettings`] changed, applies the engine-owned fields live, persists to
/// [`SettingsPath`], and sends [`SettingsChanged`]. The first frame applies values loaded from
/// disk but raises no event (it is the baseline, captured at plugin build).
#[tracing::instrument(skip_all, target = "regia_framework", name = "settings_apply")]
fn apply_and_persist_settings(world: &mut World) {
    let Some(settings_ref) = world.get_resource_ref::<GameSettings>() else {
        return;
    };
    if !settings_ref.is_changed() {
        return;
    }
    let detector_wrote = world.remove_resource::<DetectorWrote>().is_some();
    // The baseline (plugin build) or the previously applied value; `None` on first sight.
    let before = world.remove_resource::<SettingsLastSeen>().map(|last| last.0);
    // A preset change nobody marked (an options menu, `settings.set`) is the player's choice.
    if !detector_wrote
        && let Some(before) = &before
        && before.graphics.preset != world.resource::<GameSettings>().graphics.preset
        && world.resource::<GameSettings>().graphics.preset_source != Some(PresetSource::Player)
    {
        world.resource_mut::<GameSettings>().graphics.preset_source = Some(PresetSource::Player);
    }
    let after = world.resource::<GameSettings>().clone();
    world.insert_resource(SettingsLastSeen(after.clone()));
    let is_change = before.as_ref().is_some_and(|before| *before != after);
    // The appliers below push a value when it differs from what they saw last. On the first
    // apply they compare against the defaults instead of the build baseline: the baseline holds
    // the values just loaded from disk, so comparing with it would skip every saved volume,
    // mute, look and assist value at launch (ADR-5573).
    let first_apply = !world.contains_resource::<SettingsFirstApplied>();
    if first_apply {
        world.insert_resource(SettingsFirstApplied);
    }
    let applied_before = if first_apply { None } else { before.as_ref() };

    // Apply live (also on the baseline frame, so file-loaded values take effect): the frame cap.
    // This writes only the player's cap (`FramePacing::target_fps`); the per-state cap
    // (`regia_app::PacingCaps`) lives apart and the runner paces at the tighter of the two, so an
    // apply never removes a menu, loading or pause cap (ADR-7889).
    if let Some(frame_cap) = after.display.frame_cap.filter(|fps| *fps > 0) {
        if let Some(mut pacing) = world.get_resource_mut::<FramePacing>() {
            pacing.target_fps = Some(frame_cap as f32);
        }
    } else if world.get_resource::<FramePacing>().is_some() {
        world.resource_mut::<FramePacing>().target_fps = None;
    }
    // The quality preset goes to the renderer once someone set it (detection or the player); a
    // first launch before detection leaves the game's `RenderSettings` alone.
    let rebuilt = apply_preset(world, &after);
    apply_motion_comfort(world, &after, rebuilt);
    // The lighting option (Balanced / Depth) goes to the renderer's settings.
    if let Some(mut render) = world.get_resource_mut::<regia_render::RenderSettings>() {
        // A chosen option wins; going back to `None` clears it. While the option stays `None` a
        // host value (`RenderSettings::gi_look` set in `Startup`) is left alone (ADR-133).
        let look = after.graphics.lighting.map(LightingMode::gi_look);
        let cleared = look.is_none()
            && before
                .as_ref()
                .is_some_and(|before| before.graphics.lighting.is_some());
        if (look.is_some() || cleared) && render.gi_look != look {
            render.gi_look = look;
        }
        // The dynamic GI tier, with the same rule: a host value stays while the setting is unset.
        let tier = after.graphics.gi_quality.map(GiQuality::tier);
        let cleared = tier.is_none()
            && before
                .as_ref()
                .is_some_and(|before| before.graphics.gi_quality.is_some());
        if (tier.is_some() || cleared) && render.dynamic_gi_tier != tier {
            render.dynamic_gi_tier = tier;
        }
    }
    apply_audio(world, &after.audio, applied_before.map(|b| &b.audio));
    apply_look(world, &after.input, applied_before.map(|b| &b.input));
    apply_calibration(world, &after);
    apply_accessibility(world, &after);
    apply_accessibility_assists(
        world,
        &after.accessibility,
        applied_before.map(|b| &b.accessibility),
    );
    // Resolution, fullscreen and vsync reach the primary window in `apply_display_settings`
    // (it also runs while the settings are unchanged, for a window created later; ADR-7880).

    if !is_change {
        return;
    }

    // Persist (atomic and durable: temp file, fsync, rename), rate-limited so a stream of
    // changes costs at most one write per `SettingsPersistence::min_interval` on this thread.
    let path = world.resource::<SettingsPath>().0.clone();
    if let Some(path) = path {
        world
            .resource_mut::<SettingsPersistence>()
            .request(path, after.clone());
    }

    world.write_message(SettingsChanged {
        before: before.expect("is_change guarantees Some"), // invariant: is_change returned true above
        after,
    });
}

/// Copies `accessibility` into the UI's [`regia_ui::UiAccessibility`] resource (when the app has
/// a UI), which re-lays out text for a new `text_scale` and which tween and theme code read, and a
/// chosen `color_scheme` into `ThemeMode::follow`.
fn apply_accessibility(world: &mut World, settings: &GameSettings) {
    let scheme = settings.accessibility.color_scheme;
    let applied = world.get_resource::<AppliedColorScheme>().and_then(|a| a.0);
    if let Some(scheme) = scheme
        && applied != Some(scheme)
        && let Some(mut mode) = world.get_resource_mut::<regia_ui::theme_runtime::ThemeMode>()
    {
        let follow = Some(scheme.preference());
        if mode.follow != follow {
            mode.follow = follow;
        }
    }
    // Recorded only once a UI is there to take it, so an app that adds its UI later still gets it.
    let has_ui = world.contains_resource::<regia_ui::theme_runtime::ThemeMode>();
    if applied != scheme && has_ui {
        world.insert_resource(AppliedColorScheme(scheme));
    }
    let wanted = regia_ui::UiAccessibility {
        text_scale: settings.accessibility.text_scale,
        reduced_motion: settings.accessibility.reduced_motion,
        high_contrast: settings.accessibility.high_contrast,
    };
    if let Some(mut live) = world.get_resource_mut::<regia_ui::UiAccessibility>()
        && *live != wanted
    {
        *live = wanted;
    }
    publish_scene_motion(world, &settings.accessibility);
}

/// Copies the input assists, mono audio, the color-vision filter and the rumble scale (ADR-2646)
/// into [`regia_input::InputAssists`], [`regia_audio::Audio`],
/// [`regia_render::color_vision::ColorVision`] and [`regia_input::Gamepads`] when they differ from the last-seen settings (on the
/// baseline frame only when they differ from the defaults, so a host's own values survive).
fn apply_accessibility_assists(
    world: &mut World,
    now: &AccessibilitySettings,
    before: Option<&AccessibilitySettings>,
) {
    let default = AccessibilitySettings::default();
    let base = before.unwrap_or(&default);
    if (now.input_assists != base.input_assists
        || now.input_assists_by_player != base.input_assists_by_player
        || now.input_assists_by_gamepad != base.input_assists_by_gamepad)
        && let Some(mut live) = world.get_resource_mut::<regia_input::InputAssists>()
    {
        let mut wanted = regia_input::InputAssists::default();
        wanted.replace(&now.input_assists);
        wanted.replace_players(&now.input_assists_by_player);
        wanted.replace_gamepads(&now.input_assists_by_gamepad);
        if *live != wanted {
            *live = wanted;
        }
    }
    if now.mono_audio != base.mono_audio
        && let Some(mut audio) = world.get_resource_mut::<regia_audio::Audio>()
        && audio.mono_output() != now.mono_audio
    {
        audio.set_mono_output(now.mono_audio);
    }
    if now.color_vision != base.color_vision
        && let Some(mut vision) = world.get_resource_mut::<regia_render::color_vision::ColorVision>()
        && vision.filter != now.color_vision
    {
        vision.filter = now.color_vision;
    }
    if now.rumble_scale.to_bits() != base.rumble_scale.to_bits()
        && let Some(mut pads) = world.get_resource_mut::<regia_input::Gamepads>()
        && pads.rumble_scale() != now.rumble_scale
    {
        pads.set_rumble_scale(now.rumble_scale);
    }
    if now.vibration_scale.to_bits() != base.vibration_scale.to_bits() {
        apply_vibration_scale(world, now.vibration_scale);
    }
}

/// Copies the phone vibration scale when it differs from the baseline. A missing resource is
/// inserted only for a non-default scale, so a host's own [`regia_input::PhoneVibration`] survives
/// a settings file that never set one. NaN reads as full strength, matching rumble.
fn apply_vibration_scale(world: &mut World, scale: f32) {
    let stored = if scale.is_nan() {
        1.0
    } else {
        scale.clamp(0.0, 1.0)
    };
    if let Some(mut phone) = world.get_resource_mut::<regia_input::PhoneVibration>() {
        if phone.scale() != stored {
            phone.set_scale(stored);
        }
    } else if stored != 1.0 {
        let mut phone = regia_input::PhoneVibration::default();
        phone.set_scale(stored);
        world.insert_resource(phone);
    }
}

/// Copies look sensitivity and Y invert into [`regia_input::LookSettings`] when they differ from
/// the baseline. A missing resource is inserted only when the value is not the default, so a
/// host's own look scale survives a settings file that never changed it. The input crate reads
/// the resource only for actions named in [`regia_input::InputConfig::look_actions`].
fn apply_look(world: &mut World, now: &InputSettings, before: Option<&InputSettings>) {
    let (base_sensitivity, base_invert) = before
        .map(|before| (before.sensitivity, before.invert_y))
        .unwrap_or((1.0, false));
    if now.sensitivity.to_bits() == base_sensitivity.to_bits() && now.invert_y == base_invert {
        return;
    }
    let wanted = regia_input::LookSettings {
        sensitivity: now.sensitivity,
        invert_y: now.invert_y,
    };
    if let Some(mut live) = world.get_resource_mut::<regia_input::LookSettings>() {
        if *live != wanted {
            *live = wanted;
        }
    } else if wanted != regia_input::LookSettings::default() {
        world.insert_resource(wanted);
    }
}

/// Scene-motion scales the camera systems read (ADR-3464).
///
/// Published on every settings apply from [`AccessibilitySettings`]. Absent means scale `1` and no
/// field-of-view override, which is what a host that never loaded settings gets.
///
/// Framework camera effects that animate on their own, and whether reduced motion stops them:
///
/// - Camera shake ([`crate::camera::sensor::ShakeSensor`]) honors it: [`Self::shake_scale`] is `0`.
/// - Handheld sway, breathing and walk bob ([`crate::camera::handheld::Handheld`]) honor it:
///   [`Self::bob_scale`] is `0`.
/// - The first-person walk bob ([`crate::camera::walk_bob`]) honors the same [`Self::bob_scale`].
/// - Motion blur honors it by being forced off while reduced motion is on.
/// - Field-of-view kicks (rig fov nodes, a shot camera's fov, the sequencer's `camera.fov`) stay
///   authored. The gameplay camera driver replaces its fov only when [`Self::fov_override_deg`] is
///   `Some`. Reduced motion does not strip those kicks, and shots and the sequencer are not
///   overridden.
/// - Phone vibration and gamepad rumble are not tied to reduced motion.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct SceneMotion {
    /// Multiplier for authored camera shake. `0` when reduced motion is on.
    pub shake_scale: f32,
    /// Multiplier for handheld sway, breathing and walk bob. `0` when reduced motion is on.
    pub bob_scale: f32,
    /// Gameplay field of view in degrees, already clamped, or `None` to keep the authored value.
    pub fov_override_deg: Option<f32>,
}

impl SceneMotion {
    /// Lowest gameplay field of view the override applies, degrees. Matches the rig fov node.
    pub const FOV_MIN_DEG: f32 = 1.0;
    /// Highest gameplay field of view the override applies, degrees. Matches the rig fov node.
    pub const FOV_MAX_DEG: f32 = 170.0;

    /// The scales [`AccessibilitySettings`] asks the cameras to use.
    #[must_use]
    pub fn from_accessibility(settings: &AccessibilitySettings) -> Self {
        let shake = if settings.camera_shake_scale.is_finite() {
            settings.camera_shake_scale.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            shake_scale: if settings.reduced_motion { 0.0 } else { shake },
            bob_scale: if settings.reduced_motion { 0.0 } else { 1.0 },
            fov_override_deg: settings.fov_override_deg.and_then(|degrees| {
                if degrees.is_finite() {
                    Some(degrees.clamp(Self::FOV_MIN_DEG, Self::FOV_MAX_DEG))
                } else {
                    None
                }
            }),
        }
    }

    /// `authored` degrees, or the player's override when one is set.
    #[must_use]
    pub fn fov_deg(self, authored: f32) -> f32 {
        self.fov_override_deg.unwrap_or(authored)
    }
}

/// Writes [`SceneMotion`] from `settings` on every apply, including the defaults, so a camera
/// system sees reduced motion even when the other accessibility fields are unchanged.
fn publish_scene_motion(world: &mut World, settings: &AccessibilitySettings) {
    let wanted = SceneMotion::from_accessibility(settings);
    if world
        .get_resource::<SceneMotion>()
        .is_none_or(|live| *live != wanted)
    {
        world.insert_resource(wanted);
    }
}

/// The profile motion-blur bit captured before a player force, and the force currently written.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
struct MotionBlurMemory {
    /// The bit to restore when the player stops forcing one.
    tier: bool,
    /// Whether `tier` was captured.
    have_tier: bool,
    /// The force written onto the profile, if one is in effect.
    forced: Option<bool>,
}

/// The motion-blur bit the player forces, if any. Reduced motion is off, and it wins over
/// [`AccessibilitySettings::motion_blur`].
fn forced_motion_blur(accessibility: &AccessibilitySettings) -> Option<bool> {
    if accessibility.reduced_motion {
        Some(false)
    } else {
        accessibility.motion_blur
    }
}

/// Applies the player's motion-blur choice when a preset rebuild did not just do it.
///
/// `None` leaves the profile bit alone until a previous force has one to restore. The first force
/// captures the bit it replaces, so clearing the choice puts that bit back.
fn apply_motion_comfort(world: &mut World, settings: &GameSettings, rebuilt: bool) {
    if rebuilt {
        return;
    }
    let forced = forced_motion_blur(&settings.accessibility);
    let mut memory = world
        .get_resource::<MotionBlurMemory>()
        .copied()
        .unwrap_or_default();
    if memory.forced == forced {
        return;
    }
    {
        let Some(mut render) = world.get_resource_mut::<regia_render::RenderSettings>() else {
            return;
        };
        match forced {
            Some(bit) => {
                if memory.forced.is_none() && !memory.have_tier {
                    memory.tier = render.profile.motion_blur;
                    memory.have_tier = true;
                }
                render.profile.motion_blur = bit;
            }
            None => {
                if memory.have_tier {
                    render.profile.motion_blur = memory.tier;
                }
            }
        }
        memory.forced = forced;
    }
    world.insert_resource(memory);
}

/// The `render_scale` and `shadow_quality` overrides (as bits) last applied on top of the preset.
#[derive(Resource, Debug, Default, PartialEq)]
struct AppliedOverrides(Option<u32>, Option<u32>);

/// The finite override value, or `None`.
fn finite(v: Option<f32>) -> Option<f32> {
    v.filter(|v| v.is_finite())
}

/// Writes the `render_scale` and `shadow_quality` overrides of `graphics` onto `profile`
/// (already holding the preset's values). `motion_blur` forces the profile's allow bit when
/// `Some`; `None` leaves the bit the preset or the host wrote.
fn apply_graphics_overrides(
    profile: &mut regia_render::RenderProfile,
    graphics: &GraphicsSettings,
    motion_blur: Option<bool>,
) {
    if let Some(scale) = finite(graphics.render_scale) {
        profile.render_scale = scale.clamp(0.25, 1.0);
    }
    if let Some(quality) = finite(graphics.shadow_quality) {
        let quality = quality.clamp(0.0, 1.0);
        let scaled = (profile.csm_resolution as f32 * quality) as u32;
        profile.csm_resolution = if scaled < 256 {
            256.min(profile.csm_resolution)
        } else {
            1 << scaled.ilog2()
        };
        if quality == 0.0 {
            profile.local_shadow_atlas_size = 0;
            profile.local_shadow_max_lights = 0;
        } else {
            let atlas = (profile.local_shadow_atlas_size as f32 * quality) as u32;
            profile.local_shadow_atlas_size = if atlas < 256 { 0 } else { 1 << atlas.ilog2() };
        }
    }
    if let Some(enabled) = motion_blur {
        profile.motion_blur = enabled;
    }
}

/// Applies a package before Startup even in apps without SettingsPlugin; saved/player state,
/// when present, uses the same precedence as the regular settings applier.
pub(crate) fn apply_packaged_startup(world: &mut World) {
    crate::packaged_config::ensure_loaded(world);
    if crate::packaged_config::active_profile(world).is_some() {
        let settings = world.get_resource::<GameSettings>().cloned().unwrap_or_default();
        apply_preset(world, &settings);
    }
}

/// Applies the packaged profile or `graphics.preset` when it (or a player override) changed.
/// Without a package, an unset source leaves the host's render settings alone.
///
/// Returns whether the profile was rebuilt. A rebuild records the tier's motion-blur bit before
/// the player's force overwrites it, so [`apply_motion_comfort`] can restore it later.
fn apply_preset(world: &mut World, settings: &GameSettings) -> bool {
    let preset = if let Some(packaged) = crate::packaged_config::active_profile(world) {
        PresetChoice::Packaged(packaged)
    } else if settings.graphics.source() != PresetSource::Unset {
        PresetChoice::Tier(settings.graphics.preset)
    } else {
        return false;
    };
    let overrides = AppliedOverrides(
        finite(settings.graphics.render_scale).map(f32::to_bits),
        finite(settings.graphics.shadow_quality).map(f32::to_bits),
    );
    if world.get_resource::<AppliedPreset>().and_then(|a| a.0) == Some(preset)
        && world.get_resource::<AppliedOverrides>() == Some(&overrides)
    {
        return false;
    }
    if !world.contains_resource::<regia_render::RenderSettings>() {
        world.insert_resource(regia_render::RenderSettings::default());
    }
    // The GPU-dependent levers (ADR-157 addendum v0.1.2) need the adapter; at plugin build there is no render
    // device yet, so `decide_gpu_levers` finishes the job on the first frame that has one.
    let adapter = crate::auto_quality::current_adapter(world);
    let policy = terrain_vt_policy(world);
    match preset {
        PresetChoice::Tier(preset) => preset.tier(cfg!(target_os = "android")).apply_for_gpu_with(
            &mut world.resource_mut::<regia_render::RenderSettings>(),
            adapter.as_ref(),
            &policy,
        ),
        // A preset row applies exactly; a tier name (`VeryLow`, ADR-8844) applies as the tier,
        // with the GPU levers when the adapter is known.
        PresetChoice::Packaged(name) => name.apply_for_gpu_with(
            &mut world.resource_mut::<regia_render::RenderSettings>(),
            adapter.as_ref(),
            &policy,
        ),
    }
    let tier_blur = world
        .resource::<regia_render::RenderSettings>()
        .profile
        .motion_blur;
    let forced = forced_motion_blur(&settings.accessibility);
    apply_graphics_overrides(
        &mut world.resource_mut::<regia_render::RenderSettings>().profile,
        &settings.graphics,
        forced,
    );
    world.insert_resource(overrides);
    world.insert_resource(AppliedPreset(
        Some(preset),
        matches!(
            preset,
            PresetChoice::Tier(_) | PresetChoice::Packaged(regia_render::ProfileName::Tier(_))
        ) && adapter.is_none(),
    ));
    world.insert_resource(MotionBlurMemory {
        tier: tier_blur,
        have_tier: true,
        forced,
    });
    match preset {
        PresetChoice::Tier(preset) => {
            let tier = preset.tier(cfg!(target_os = "android"));
            tracing::info!(target: "regia_framework", "graphics preset {preset:?} → render tier {tier}");
        }
        PresetChoice::Packaged(preset) => {
            tracing::info!(target: "regia_framework", "packaged render profile {}", preset.name());
        }
    }
    true
}

/// `Update`: once the render device exists, applies the GPU-dependent levers of a preset that was
/// applied without it (at plugin build): the terrain virtual texture on Desktop Low on discrete
/// GPUs with ≥ 6 GB, on Linux and not on AMD ([`regia_render::quality::QualityTier::terrain_vt_default`],
/// ADR-157 addendum v0.1.2; beyond that only where the host's
/// [`regia_render::quality::TerrainVtPolicy`] resource allows, ADR-2122). Only
/// turns the lever on; a host that wants it off calls `TerrainSharedDraw::set_virtual_texture`.
fn decide_gpu_levers(world: &mut World) {
    let Some(AppliedPreset(Some(choice), true)) = world.get_resource::<AppliedPreset>() else {
        return;
    };
    let choice = *choice;
    let tier = match choice {
        PresetChoice::Tier(preset) => preset.tier(cfg!(target_os = "android")),
        // A packaged tier name (ADR-8844) is applied exactly, on every platform.
        PresetChoice::Packaged(regia_render::ProfileName::Tier(tier)) => tier,
        PresetChoice::Packaged(regia_render::ProfileName::Preset(_)) => return,
    };
    let Some(adapter) = crate::auto_quality::current_adapter(world) else {
        return;
    };
    world.insert_resource(AppliedPreset(Some(choice), false));
    let policy = terrain_vt_policy(world);
    if tier.terrain_vt_default_with(&adapter, std::env::consts::OS, &policy)
        && let Some(mut render) = world.get_resource_mut::<regia_render::RenderSettings>()
    {
        policy.set_lever(&mut render.profile.terrain_budget, true);
        tracing::info!(target: "regia_framework", "render tier {tier} on {}: terrain virtual texture on (ADR-157 addendum v0.1.2)", adapter.name);
    }
}

/// The host's terrain virtual texture policy (ADR-2122), the measured default without one.
fn terrain_vt_policy(world: &World) -> regia_render::quality::TerrainVtPolicy {
    world
        .get_resource::<regia_render::quality::TerrainVtPolicy>()
        .copied()
        .unwrap_or_default()
}

/// Applies `graphics.calibration` to the renderer: the [`regia_render::DisplayCalibration`]
/// resource (the player's values, or the platform default when unset) and, when the player chose,
/// the HDR request in [`regia_render::OutputSettings`].
fn apply_calibration(world: &mut World, settings: &GameSettings) {
    let display = world.get_resource::<regia_render::DisplayLuminance>().copied();
    let calibration = match &settings.graphics.calibration {
        Some(chosen) => chosen.to_calibration(display.as_ref()),
        None => {
            let platform = world
                .get_resource::<regia_render::DisplayPlatform>()
                .copied()
                .unwrap_or_else(regia_render::DisplayPlatform::current);
            let default = regia_render::DisplayCalibration::platform_default(platform);
            display.map_or(default, |d| default.with_display(&d))
        }
    };
    if world.get_resource::<regia_render::DisplayCalibration>() != Some(&calibration) {
        world.insert_resource(calibration);
    }
    if let Some(chosen) = &settings.graphics.calibration
        && let Some(mut output) = world.get_resource_mut::<regia_render::OutputSettings>()
    {
        let want = if chosen.hdr {
            regia_render::HdrOutput::Hdr10
        } else {
            regia_render::HdrOutput::Sdr
        };
        if output.hdr != want {
            output.hdr = want;
        }
    }
}

/// The last settings value this plugin applied; the `before` of the next [`SettingsChanged`].
#[derive(Resource, Debug, Clone)]
struct SettingsLastSeen(GameSettings);

/// Present once `apply_and_persist_settings` has applied the settings at least once.
#[derive(Resource, Debug, Clone, Copy)]
struct SettingsFirstApplied;

/// Writes `settings` to `path` atomically and durably (temp file, `sync_all`, rename, directory
/// fsync), creating parent directories.
fn persist(settings: &GameSettings, path: &std::path::Path) -> Result<(), SettingsError> {
    let text = settings.to_ron().map_err(SettingsError::Serialize)?;
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return Err(SettingsError::Io(err));
    }
    // This call's own temp file (`<file>.tmp.<pid>.<n>`, created new): two writers never share
    // one, and nothing already at a temp name is opened or followed (audit v034 cook M2).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let temp = path.with_file_name(name);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| {
            use std::io::Write as _;
            file.write_all(text.as_bytes())?;
            // Durable before the rename (as `save_game::write_atomic`): after a power loss the file
            // is either the old settings or the new ones, never an empty `settings.ron`.
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temp, path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(SettingsError::Io(err));
    }
    // Best effort: persist the rename itself. Opening a directory fails on Windows; NTFS journals
    // the rename there anyway.
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Failures of loading/persisting settings; never fatal (the game boots on defaults).
#[derive(thiserror::Error, Debug)]
pub enum SettingsError {
    /// RON serialisation failed.
    #[error("settings serialization failed: {0}")]
    Serialize(#[from] ron::Error),
    /// Disk IO failed.
    #[error("settings IO: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_app(path: Option<PathBuf>) -> App {
        let mut app = App::new();
        app.insert_resource(SettingsPath(path))
            .add_plugins((MinimalPlugins, SettingsPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("regia-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir.join("settings.ron")
    }

    #[test]
    fn audio_bus_settings_default_to_full_volume_and_apply_live() {
        assert_eq!(AudioBusSettings::default().master, 1.0);
        assert_eq!(linear_to_db(1.0), 0.0);
        assert!((linear_to_db(0.5) + 6.0206).abs() < 1e-3);
        assert_eq!(linear_to_db(0.0), -80.0);
        assert_eq!(linear_to_db(f32::NAN), -80.0);
        let mut app = make_app(None);
        app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..regia_audio::AudioSettings::default()
        }));
        app.update();
        let db = |app: &App, bus: &str| {
            app.world()
                .resource::<regia_audio::Audio>()
                .bus_volume(bus)
                .expect("bus")
        };
        // The baseline (defaults) leaves the host's levels alone.
        assert_eq!(db(&app, "Master"), 0.0);
        app.world_mut().resource_mut::<GameSettings>().audio.master = 0.5;
        app.world_mut().resource_mut::<GameSettings>().audio.music = 0.1;
        app.update();
        assert!((db(&app, "Master") + 6.0206).abs() < 1e-3);
        assert!((db(&app, "Music") + 20.0).abs() < 1e-3);
        assert_eq!(db(&app, "Sfx"), 0.0);
    }

    #[test]
    fn file_loaded_values_apply_on_the_first_frame() {
        // Saved volumes, mutes, look and vibration must take effect at launch, not only after the
        // player changes some other setting.
        let path = temp_path("first-frame");
        let mut saved = GameSettings::default();
        saved.audio.master = 0.5;
        saved.audio.mute_voice = true;
        saved.input.sensitivity = 2.0;
        saved.input.invert_y = true;
        saved.accessibility.vibration_scale = 0.5;
        std::fs::write(&path, saved.to_ron().expect("ron")).expect("write settings");
        let mut app = make_app(Some(path.clone()));
        app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..regia_audio::AudioSettings::default()
        }));
        app.update();
        let audio = app.world().resource::<regia_audio::Audio>();
        assert!((audio.bus_volume("Master").expect("bus") + 6.0206).abs() < 1e-3);
        assert_eq!(audio.bus_muted("Voice"), Some(true));
        let look = app
            .world()
            .get_resource::<regia_input::LookSettings>()
            .expect("look settings inserted");
        assert_eq!(look.sensitivity, 2.0);
        assert!(look.invert_y);
        let phone = app
            .world()
            .get_resource::<regia_input::PhoneVibration>()
            .expect("vibration inserted");
        assert_eq!(phone.scale(), 0.5);
        // The launch apply is not a change: nothing is re-persisted and no event is raised.
        let messages = app.world().resource::<Messages<SettingsChanged>>();
        let mut cursor = regia_ecs::ecs::message::MessageCursor::default();
        assert_eq!(
            cursor.read(messages).count(),
            0,
            "the launch apply raises no change event"
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn graphics_overrides_write_render_scale_and_shadow_quality() {
        let base = regia_render::RenderProfile::from_preset(regia_render::RenderProfilePreset::Deck);
        let mut profile = base.clone();
        apply_graphics_overrides(&mut profile, &GraphicsSettings::default(), None);
        assert_eq!(profile, base, "no override changes nothing");
        let graphics = GraphicsSettings {
            render_scale: Some(0.5),
            shadow_quality: Some(0.0),
            ..GraphicsSettings::default()
        };
        apply_graphics_overrides(&mut profile, &graphics, None);
        assert_eq!(profile.render_scale, 0.5);
        assert_eq!(profile.local_shadow_atlas_size, 0);
        assert_eq!(profile.local_shadow_max_lights, 0);
        assert!(profile.csm_resolution <= base.csm_resolution);
        let mut profile = base.clone();
        let graphics = GraphicsSettings {
            render_scale: Some(f32::NAN),
            shadow_quality: Some(f32::INFINITY),
            ..GraphicsSettings::default()
        };
        apply_graphics_overrides(&mut profile, &graphics, None);
        assert_eq!(profile, base, "non-finite overrides are ignored");
    }

    #[test]
    fn render_scale_override_reaches_render_settings_and_clears() {
        let mut app = make_app(None);
        app.update();
        {
            let mut settings = app.world_mut().resource_mut::<GameSettings>();
            settings.graphics.preset = GraphicsPreset::High;
            settings.graphics.render_scale = Some(0.5);
        }
        app.update();
        let scale = |app: &App| {
            app.world()
                .resource::<regia_render::RenderSettings>()
                .profile
                .render_scale
        };
        assert_eq!(scale(&app), 0.5);
        app.world_mut()
            .resource_mut::<GameSettings>()
            .graphics
            .render_scale = None;
        app.update();
        assert_ne!(
            scale(&app),
            0.5,
            "clearing the override restores the preset's scale"
        );
    }

    #[test]
    fn unset_lighting_keeps_a_host_gi_look() {
        let mut app = make_app(None);
        app.update();
        app.world_mut()
            .resource_mut::<regia_render::RenderSettings>()
            .gi_look = Some(regia_render::GiLook::Depth);
        // An unrelated settings change must not wipe the host's value.
        app.world_mut().resource_mut::<GameSettings>().audio.master = 0.4;
        app.update();
        assert_eq!(
            app.world().resource::<regia_render::RenderSettings>().gi_look,
            Some(regia_render::GiLook::Depth)
        );
        // Choosing an option applies it; clearing it goes back to the profile.
        app.world_mut().resource_mut::<GameSettings>().graphics.lighting = Some(LightingMode::Balanced);
        app.update();
        assert_eq!(
            app.world().resource::<regia_render::RenderSettings>().gi_look,
            Some(regia_render::GiLook::Balanced)
        );
        app.world_mut().resource_mut::<GameSettings>().graphics.lighting = None;
        app.update();
        assert_eq!(
            app.world().resource::<regia_render::RenderSettings>().gi_look,
            None
        );
    }

    #[test]
    fn gi_quality_applies_live_and_unset_keeps_a_host_tier() {
        let mut app = make_app(None);
        app.update();
        app.world_mut()
            .resource_mut::<regia_render::RenderSettings>()
            .dynamic_gi_tier = Some(regia_render::DynamicGiTier::Ultra);
        app.world_mut().resource_mut::<GameSettings>().audio.master = 0.4;
        app.update();
        assert_eq!(
            app.world()
                .resource::<regia_render::RenderSettings>()
                .dynamic_gi_tier,
            Some(regia_render::DynamicGiTier::Ultra)
        );
        app.world_mut().resource_mut::<GameSettings>().graphics.gi_quality = Some(GiQuality::Low);
        app.update();
        assert_eq!(
            app.world()
                .resource::<regia_render::RenderSettings>()
                .dynamic_gi_tier,
            Some(regia_render::DynamicGiTier::Low)
        );
        app.world_mut().resource_mut::<GameSettings>().graphics.gi_quality = None;
        app.update();
        assert_eq!(
            app.world()
                .resource::<regia_render::RenderSettings>()
                .dynamic_gi_tier,
            None
        );
        for t in regia_render::DynamicGiTier::ALL {
            assert_eq!(GiQuality::of_tier(t).tier(), t);
        }
    }

    #[test]
    fn a_file_without_gi_quality_loads_and_the_choice_round_trips() {
        let mut settings = GameSettings::default();
        assert_eq!(settings.graphics.gi_quality, None);
        settings.graphics.gi_quality = Some(GiQuality::Medium);
        let text = settings.to_ron().expect("serializes");
        assert!(text.contains("gi_quality"), "{text}");
        let back = GameSettings::from_ron(&text).expect("parses");
        assert_eq!(back.graphics.gi_quality, Some(GiQuality::Medium));
        // A file written before the field existed: the field is simply absent.
        // Compact RON: the field is written as `,gi_quality:Some(Medium)` (pretty RON spaces it).
        let old = text
            .replace(",gi_quality:Some(Medium)", "")
            .replace("gi_quality: Some(Medium),", "");
        assert!(!old.contains("gi_quality"), "{old}");
        let back = GameSettings::from_ron(&old).expect("an older file parses");
        assert_eq!(back.graphics.gi_quality, None);
    }

    #[test]
    fn ron_round_trip() {
        let mut settings = GameSettings::default();
        settings.input.sensitivity = 2.5;
        settings.display.frame_cap = Some(120);
        settings.audio.master = 0.4;
        let text = settings.to_ron().expect("serializes");
        let back = GameSettings::from_ron(&text).expect("parses");
        assert_eq!(settings, back);
    }

    #[test]
    fn accessibility_fields_round_trip_and_default_in_older_files() {
        let mut settings = GameSettings::default();
        settings.accessibility.text_scale = 1.5;
        settings.accessibility.reduced_motion = true;
        settings.accessibility.high_contrast = true;
        let text = settings.to_ron().expect("serializes");
        assert_eq!(GameSettings::from_ron(&text).expect("parses"), settings);
        // A file from before the two flags existed loads with both off and keeps its text scale.
        let old = "(graphics:(preset:Medium,shadow_quality:None,render_scale:None),display:(resolution:None,vsync:true,frame_cap:None),audio:(master:1.0,music:1.0,sfx:1.0),input:(sensitivity:1.0,invert_y:false),language:\"\",accessibility:(subtitles:false,text_scale:1.25))";
        let loaded = GameSettings::from_ron(old).expect("older file loads");
        assert_eq!(loaded.accessibility.text_scale, 1.25);
        assert!(!loaded.accessibility.subtitles);
        assert!(!loaded.accessibility.reduced_motion);
        assert!(!loaded.accessibility.high_contrast);
    }

    #[test]
    fn accessibility_settings_reach_the_ui_resource_live() {
        let mut app = make_app(None);
        app.insert_resource(regia_ui::UiAccessibility::default());
        app.update();
        {
            let mut settings = app.world_mut().resource_mut::<GameSettings>();
            settings.accessibility.text_scale = 1.5;
            settings.accessibility.reduced_motion = true;
            settings.accessibility.high_contrast = true;
        }
        app.update();
        let live = *app.world().resource::<regia_ui::UiAccessibility>();
        assert_eq!(live.text_scale, 1.5);
        assert!(live.reduced_motion);
        assert!(live.high_contrast);
    }

    #[test]
    fn accessibility_assists_round_trip_and_default_in_older_files() {
        use regia_render::color_vision::{ColorVisionDeficiency, ColorVisionFilter, ColorVisionMode};
        let mut settings = GameSettings::default();
        settings.accessibility.input_assists.insert(
            "Crouch".into(),
            regia_input::ActionAssist {
                mode: regia_input::PressMode::Toggle,
                min_hold_secs: 0.25,
                repeat: Some(regia_input::RepeatRate::DEFAULT),
                cooldown_steps: 0,
            },
        );
        settings.accessibility.mono_audio = true;
        settings.accessibility.rumble_scale = 0.25;
        settings.accessibility.color_vision =
            ColorVisionFilter::new(ColorVisionMode::Compensate, ColorVisionDeficiency::Tritanopia);
        let text = settings.to_ron().expect("serializes");
        assert_eq!(GameSettings::from_ron(&text).expect("parses"), settings);
        assert_eq!(
            GameSettings::from_ron_strict(&text).expect("strict parses"),
            settings
        );
        // A file from before these fields loads with all of them off.
        let old = "(graphics:(preset:Medium,shadow_quality:None,render_scale:None),display:(resolution:None,vsync:true,frame_cap:None),audio:(master:1.0,music:1.0,sfx:1.0),input:(sensitivity:1.0,invert_y:false),language:\"\",accessibility:(subtitles:true,text_scale:1.0,reduced_motion:true))";
        let loaded = GameSettings::from_ron(old).expect("older file loads");
        assert!(loaded.accessibility.reduced_motion);
        assert!(loaded.accessibility.input_assists.is_empty());
        assert!(!loaded.accessibility.mono_audio);
        assert!(loaded.accessibility.color_vision.is_off());
        assert_eq!(loaded.accessibility.rumble_scale, 1.0);
        assert_eq!(loaded.accessibility.camera_shake_scale, 1.0);
        assert_eq!(loaded.accessibility.motion_blur, None);
        assert_eq!(loaded.accessibility.fov_override_deg, None);
        assert_eq!(loaded.accessibility.vibration_scale, 1.0);
        assert!(loaded.accessibility.input_assists_by_player.is_empty());
        assert!(loaded.accessibility.input_assists_by_gamepad.is_empty());
        assert_eq!(loaded.audio.voice, 1.0);
        assert_eq!(loaded.audio.ambience, 1.0);
        assert!(!loaded.audio.mute_master);
        assert!(!loaded.audio.mute_voice);
        assert!(!loaded.audio.mute_ambience);
    }

    #[test]
    fn accessibility_assists_reach_input_audio_and_render_live() {
        use regia_render::color_vision::{
            ColorVision, ColorVisionDeficiency, ColorVisionFilter, ColorVisionMode,
        };
        let mut app = make_app(None);
        app.init_resource::<regia_input::InputAssists>();
        app.init_resource::<ColorVision>();
        app.init_resource::<regia_input::Gamepads>();
        app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..Default::default()
        }));
        // A host's own assist survives the defaults on the baseline frame.
        app.world_mut().resource_mut::<regia_input::InputAssists>().set(
            "Sprint",
            regia_input::ActionAssist {
                mode: regia_input::PressMode::Toggle,
                ..Default::default()
            },
        );
        app.update();
        assert!(
            app.world()
                .resource::<regia_input::InputAssists>()
                .get("Sprint")
                .is_some()
        );
        let filter = ColorVisionFilter::new(ColorVisionMode::Simulate, ColorVisionDeficiency::Protanopia);
        {
            let mut settings = app.world_mut().resource_mut::<GameSettings>();
            let a = &mut settings.accessibility;
            a.input_assists.insert(
                "Aim".into(),
                regia_input::ActionAssist {
                    mode: regia_input::PressMode::Toggle,
                    ..Default::default()
                },
            );
            a.mono_audio = true;
            a.rumble_scale = 0.5;
            a.color_vision = filter;
        }
        app.update();
        let assists = app.world().resource::<regia_input::InputAssists>();
        assert_eq!(
            assists.get("Aim").map(|a| a.mode),
            Some(regia_input::PressMode::Toggle)
        );
        assert!(
            assists.get("Sprint").is_none(),
            "the player's settings replace the set"
        );
        assert!(app.world().resource::<regia_audio::Audio>().mono_output());
        assert_eq!(
            app.world().resource::<regia_input::Gamepads>().rumble_scale(),
            0.5
        );
        assert_eq!(app.world().resource::<ColorVision>().filter, filter);
        // Turning them off again reaches the live resources too.
        {
            let mut settings = app.world_mut().resource_mut::<GameSettings>();
            settings.accessibility.mono_audio = false;
            settings.accessibility.color_vision = ColorVisionFilter::default();
        }
        app.update();
        assert!(!app.world().resource::<regia_audio::Audio>().mono_output());
        assert!(app.world().resource::<ColorVision>().filter.is_off());
    }

    #[test]
    fn the_color_scheme_choice_drives_theme_mode_and_loads_from_older_files() {
        let mut settings = GameSettings::default();
        assert_eq!(
            settings.accessibility.color_scheme, None,
            "the game decides by default"
        );
        settings.accessibility.color_scheme = Some(ColorSchemeSetting::System);
        let text = settings.to_ron().expect("serializes");
        assert_eq!(GameSettings::from_ron(&text).expect("parses"), settings);
        let old = "(graphics:(preset:Medium,shadow_quality:None,render_scale:None),display:(resolution:None,vsync:true,frame_cap:None),audio:(master:1.0,music:1.0,sfx:1.0),input:(sensitivity:1.0,invert_y:false),language:\"\",accessibility:(subtitles:true,text_scale:1.0,high_contrast:true))";
        let loaded = GameSettings::from_ron(old).expect("older file loads");
        assert_eq!(loaded.accessibility.color_scheme, None);
        assert!(loaded.accessibility.high_contrast);

        let mut app = make_app(None);
        let host = regia_ui::theme_runtime::ThemeMode {
            follow: Some(regia_ui::theme_runtime::SchemePreference::Dark),
            ..Default::default()
        };
        app.insert_resource(host.clone());
        app.update();
        assert_eq!(
            *app.world().resource::<regia_ui::theme_runtime::ThemeMode>(),
            host,
            "no choice: the host's preference stays"
        );
        app.world_mut()
            .resource_mut::<GameSettings>()
            .accessibility
            .color_scheme = Some(ColorSchemeSetting::Light);
        app.update();
        assert_eq!(
            app.world()
                .resource::<regia_ui::theme_runtime::ThemeMode>()
                .follow,
            Some(regia_ui::theme_runtime::SchemePreference::Light)
        );
        // The game turns following off; an unrelated settings change does not turn it back on.
        app.world_mut()
            .resource_mut::<regia_ui::theme_runtime::ThemeMode>()
            .follow = None;
        app.world_mut().resource_mut::<GameSettings>().input.sensitivity = 2.0;
        app.update();
        assert_eq!(
            app.world()
                .resource::<regia_ui::theme_runtime::ThemeMode>()
                .follow,
            None
        );
    }

    #[test]
    fn audio_language_and_caption_fields_round_trip_and_default_in_older_files() {
        let mut settings = GameSettings {
            audio_language: "ja".into(),
            captions: Some(regia_ui::captions::CaptionOptions {
                text_scale: 1.5,
                ..Default::default()
            }),
            ..Default::default()
        };
        settings.accessibility.set_subtitle_mode(SubtitleMode::All);
        let text = settings.to_ron().expect("serializes");
        assert_eq!(GameSettings::from_ron(&text).expect("parses"), settings);
        assert_eq!(GameSettings::from_ron_strict(&text).expect("strict"), settings);
        let old = "(graphics:(preset:Medium,shadow_quality:None,render_scale:None),display:(resolution:None,vsync:true,frame_cap:None),audio:(master:1.0,music:1.0,sfx:1.0),input:(sensitivity:1.0,invert_y:false),language:\"fr\",accessibility:(subtitles:false,text_scale:1.0))";
        let loaded = GameSettings::from_ron(old).expect("older file loads");
        assert_eq!(loaded.audio_language, "", "audio follows the text language");
        assert_eq!(loaded.captions, None);
        assert_eq!(loaded.accessibility.subtitle_mode, None);
        assert_eq!(loaded.accessibility.effective_subtitle_mode(), SubtitleMode::Off);
    }

    #[test]
    fn a_first_run_follows_the_system_captions_preference() {
        let mut a = AccessibilitySettings::default();
        a.follow_system_captions(None);
        assert_eq!(
            a.effective_subtitle_mode(),
            SubtitleMode::Dialogue,
            "not reported"
        );
        a.follow_system_captions(Some(false));
        assert_eq!(
            a.effective_subtitle_mode(),
            SubtitleMode::Dialogue,
            "left off says nothing"
        );
        assert_eq!(a.subtitle_mode, None);
        a.follow_system_captions(Some(true));
        assert_eq!(a.effective_subtitle_mode(), SubtitleMode::All);
        assert!(a.subtitles);
    }

    #[test]
    fn subtitle_mode_derives_from_subtitles_and_keeps_it_in_sync() {
        let mut a = AccessibilitySettings::default();
        assert_eq!(a.effective_subtitle_mode(), SubtitleMode::Dialogue);
        a.subtitles = false;
        assert_eq!(a.effective_subtitle_mode(), SubtitleMode::Off);
        a.set_subtitle_mode(SubtitleMode::All);
        assert!(a.subtitles);
        assert_eq!(a.effective_subtitle_mode(), SubtitleMode::All);
        a.set_subtitle_mode(SubtitleMode::Off);
        assert!(!a.subtitles, "Off writes false");
        for mode in [SubtitleMode::Off, SubtitleMode::Dialogue, SubtitleMode::All] {
            assert_eq!(SubtitleMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(SubtitleMode::parse("ALL"), Some(SubtitleMode::All));
        assert_eq!(SubtitleMode::parse("some"), None);
    }

    #[test]
    fn stored_caption_options_set_the_mode_and_reach_the_presenter() {
        let mut settings = GameSettings::default();
        let options = regia_ui::captions::CaptionOptions {
            caption_mode: true,
            text_scale: 2.0,
            ..Default::default()
        };
        settings.store_caption_options(&options);
        assert_eq!(settings.accessibility.subtitle_mode, Some(SubtitleMode::All));
        let base = regia_ui::captions::CaptionOptions::default();
        assert_eq!(settings.caption_options(&base), options);
        // The mode wins over the stored flags.
        settings.accessibility.set_subtitle_mode(SubtitleMode::Off);
        let applied = settings.caption_options(&base);
        assert!(!applied.enabled && !applied.caption_mode);
        assert_eq!(applied.text_scale, 2.0);

        let mut app = make_app(None);
        let host = regia_ui::captions::CaptionOptions {
            text_scale: 1.25,
            ..Default::default()
        };
        app.insert_resource(host.clone());
        app.update();
        assert_eq!(
            *app.world().resource::<regia_ui::captions::CaptionOptions>(),
            host,
            "nothing saved: the game's options stay"
        );
        app.world_mut()
            .resource_mut::<GameSettings>()
            .accessibility
            .set_subtitle_mode(SubtitleMode::All);
        app.update();
        let live = app
            .world()
            .resource::<regia_ui::captions::CaptionOptions>()
            .clone();
        assert!(live.caption_mode && live.enabled);
        assert_eq!(live.text_scale, 1.25, "only the mode moved");
        // An agent tweak survives an unrelated settings change.
        app.world_mut()
            .resource_mut::<regia_ui::captions::CaptionOptions>()
            .text_scale = 0.75;
        app.world_mut().resource_mut::<GameSettings>().input.sensitivity = 2.0;
        app.update();
        assert_eq!(
            app.world()
                .resource::<regia_ui::captions::CaptionOptions>()
                .text_scale,
            0.75
        );
    }

    #[test]
    fn the_audio_language_setting_drives_the_audio_locale() {
        let mut app = make_app(None);
        let l = regia_ui::Localization::new(
            vec![
                ("en".to_owned(), "a = A\n".to_owned()),
                ("de".to_owned(), "a = B\n".to_owned()),
            ],
            Some("en".to_owned()),
        )
        .expect("locales");
        app.insert_resource(l);
        app.world_mut().resource_mut::<GameSettings>().audio_language = "ja".into();
        app.update();
        assert_eq!(
            app.world().resource::<regia_ui::Localization>().audio_locale(),
            "ja"
        );
        app.world_mut()
            .resource_mut::<GameSettings>()
            .audio_language
            .clear();
        app.update();
        let l = app.world().resource::<regia_ui::Localization>();
        assert_eq!(l.audio_override(), None);
        assert_eq!(l.audio_locale(), "en", "cleared: follows the text locale");
    }

    #[test]
    fn from_ron_strict_accepts_round_trip_output() {
        let mut settings = GameSettings::default();
        settings.input.sensitivity = 2.5;
        let text = settings.to_ron().expect("serializes");
        let back = GameSettings::from_ron_strict(&text).expect("strict parse");
        assert_eq!(back, settings);
    }

    #[test]
    fn from_ron_strict_rejects_unknown_top_level_fields() {
        let text = r#"GameSettings(
            graphics: (),
            display: (),
            audio: (),
            input: (),
            language: "",
            accessibility: (),
            future_section: (),
        )"#;
        let err = GameSettings::from_ron_strict(text).expect_err("unknown section rejected");
        assert!(
            matches!(err, GameSettingsStrictError::UnknownField(ref f) if f == "future_section"),
            "{err:?}"
        );
    }

    #[test]
    fn calibration_persists_and_applies_live() {
        let path = temp_path("calibration");
        let mut app = make_app(Some(path.clone()));
        app.update();
        assert!(
            app.world()
                .resource::<regia_render::DisplayCalibration>()
                .is_sdr_neutral()
                || cfg!(target_os = "android"),
            "an unset calibration is the platform default"
        );
        app.world_mut()
            .resource_mut::<GameSettings>()
            .graphics
            .calibration = Some(CalibrationSettings {
            black_level: 0.4,
            gamma: 2.4,
            ..CalibrationSettings::default()
        });
        app.update();
        let live = *app.world().resource::<regia_render::DisplayCalibration>();
        assert_eq!(live.black_level, 0.4);
        assert_eq!(live.gamma, 2.4);
        let text = std::fs::read_to_string(&path).expect("persisted");
        let back = GameSettings::from_ron(&text).expect("parses");
        assert_eq!(back.graphics.calibration.map(|c| c.black_level), Some(0.4));
        // A file from before calibration existed still loads.
        let old = "(graphics:(preset:Medium,shadow_quality:None,render_scale:None),display:(resolution:None,vsync:true,frame_cap:None),audio:(master:1.0,music:1.0,sfx:1.0),input:(sensitivity:1.0,invert_y:false),language:\"\",accessibility:(subtitles:true,text_scale:1.0))";
        assert_eq!(
            GameSettings::from_ron(old)
                .expect("old file")
                .graphics
                .calibration,
            None
        );
        let _ = std::fs::remove_file(&path);
    }

    fn language_app(path: &std::path::Path) -> App {
        let mut app = make_app(Some(path.to_path_buf()));
        let sources = ["en", "fr", "ja"]
            .iter()
            .map(|t| ((*t).to_owned(), format!("hello = Hello {t}\n")))
            .collect();
        app.insert_resource(regia_ui::Localization::new(sources, Some("en".to_owned())).expect("loads"))
            .add_message::<regia_ui::PlayerSetLocale>()
            .add_systems(Update, regia_ui::locale_startup::apply_player_locale);
        app
    }

    /// A player-driven switch is saved, and the next start-up picks it from the saved settings.
    #[test]
    fn a_player_language_choice_is_written_back_and_read_at_the_next_start() {
        let path = temp_path("language-write-back");
        {
            let mut app = language_app(&path);
            app.update();
            app.world_mut()
                .write_message(regia_ui::PlayerSetLocale("ja".to_owned()));
            app.update();
            assert_eq!(app.world().resource::<regia_ui::Localization>().locale(), "ja");
            assert_eq!(app.world().resource::<GameSettings>().language, "ja");
            app.update();
        }
        let saved = GameSettings::from_ron(&std::fs::read_to_string(&path).expect("file")).expect("parses");
        assert_eq!(saved.language, "ja");
        // A fresh start: the plugin offers the saved language as a hint and the start-up order
        // (everything else absent) selects it.
        let app = language_app(&path);
        let mut hints = app.world().resource::<regia_platform::LocaleHints>().clone();
        assert_eq!(hints.settings.as_deref(), Some("ja"));
        hints.isolated = true;
        hints.os = Some("fr".to_owned());
        let mut fresh = regia_ui::Localization::new(
            ["en", "fr", "ja"]
                .iter()
                .map(|t| ((*t).to_owned(), format!("hello = Hello {t}\n")))
                .collect(),
            Some("en".to_owned()),
        )
        .expect("loads");
        let start = regia_ui::locale_startup::choose_startup_locale(&mut fresh, &hints);
        assert_eq!(start.source, regia_ui::LocaleSource::Settings);
        assert_eq!(
            fresh.locale(),
            "ja",
            "the saved language outranks the OS language"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A switch that does not come from the player (an agent, game code) saves nothing.
    #[test]
    fn a_switch_without_a_player_choice_is_not_written_back() {
        let path = temp_path("language-no-write-back");
        let mut app = language_app(&path);
        app.update();
        app.world_mut()
            .resource_mut::<regia_ui::Localization>()
            .set_locale("fr");
        app.update();
        app.update();
        assert_eq!(app.world().resource::<GameSettings>().language, "");
        assert!(!path.exists(), "nothing changed, nothing written");
    }

    /// `settings.set language` (or an options menu writing the field) selects the locale live.
    #[test]
    fn a_changed_language_setting_selects_the_locale_live() {
        let path = temp_path("language-follow");
        let mut app = language_app(&path);
        app.update();
        app.world_mut().resource_mut::<GameSettings>().language = "fr-CA".to_owned();
        app.update();
        app.update();
        assert_eq!(app.world().resource::<regia_ui::Localization>().locale(), "fr");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn defaults_persist_and_reload() {
        let path = temp_path("roundtrip");
        {
            let mut app = make_app(Some(path.clone()));
            app.world_mut().resource_mut::<GameSettings>().audio.master = 0.25;
            app.update(); // applies + persists
            assert!(path.exists(), "settings file written");
        }
        // A fresh app loads the persisted value.
        let app = make_app(Some(path.clone()));
        assert_eq!(app.world().resource::<GameSettings>().audio.master, 0.25);
        let _ = std::fs::remove_file(&path);
    }
    /// A slider dragged over many frames writes the file once, then the newest value once the
    /// interval has passed; dropping the app writes a value that is still pending.
    #[test]
    fn rapid_changes_coalesce_into_rate_limited_writes() {
        let path = temp_path("coalesce");
        let mut app = make_app(Some(path.clone()));
        app.world_mut().resource_mut::<SettingsPersistence>().min_interval = Duration::from_millis(300);
        app.update(); // baseline
        let read_master = |path: &PathBuf| {
            GameSettings::from_ron(&std::fs::read_to_string(path).expect("file"))
                .expect("parses")
                .audio
                .master
        };
        let started = std::time::Instant::now();
        for i in 1..=20 {
            app.world_mut().resource_mut::<GameSettings>().audio.master = i as f32 / 100.0;
            app.update();
        }
        let persistence = app.world().resource::<SettingsPersistence>();
        if started.elapsed() < Duration::from_millis(300) {
            assert_eq!(persistence.writes(), 1, "the first change is written at once");
            assert!(persistence.has_pending(), "the rest wait");
            assert_eq!(read_master(&path), 0.01);
        }
        std::thread::sleep(Duration::from_millis(350));
        app.update(); // no change this frame: the pending value is written
        let persistence = app.world().resource::<SettingsPersistence>();
        assert!(!persistence.has_pending());
        assert!(persistence.writes() <= 3, "{} writes", persistence.writes());
        assert_eq!(read_master(&path), 0.20, "the newest value wins");

        // A change still pending at exit is written when the app (world) is dropped.
        app.world_mut().resource_mut::<GameSettings>().audio.master = 0.33;
        app.update();
        drop(app);
        assert_eq!(read_master(&path), 0.33);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_replaces_the_file_and_leaves_no_temp() {
        let path = temp_path("persist-durable");
        std::fs::write(&path, "stale").expect("seed old file");
        let mut settings = GameSettings::default();
        settings.audio.master = 0.5;
        persist(&settings, &path).expect("persist");
        let text = std::fs::read_to_string(&path).expect("read back");
        let back = GameSettings::from_ron(&text).expect("parses");
        assert_eq!(back.audio.master, 0.5);
        assert!(!path.with_extension("ron.tmp").exists(), "temp file renamed away");
        let _ = std::fs::remove_file(&path);
    }

    /// Audit v034 cook M2: the temp file is this writer's own, so a stale entry at the old fixed
    /// name (here a directory) does not stop the write.
    #[test]
    fn persist_does_not_depend_on_a_fixed_temp_name() {
        let path = temp_path("persist-stale-temp");
        let stale = path.with_extension("ron.tmp");
        std::fs::create_dir_all(&stale).expect("stale entry");
        let mut settings = GameSettings::default();
        settings.audio.master = 0.25;
        persist(&settings, &path).expect("persist uses its own temp file");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(GameSettings::from_ron(&text).expect("parses").audio.master, 0.25);
        let _ = std::fs::remove_dir(&stale);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn change_is_applied_live_to_frame_pacing() {
        let mut app = make_app(None);
        app.world_mut().insert_resource(FramePacing::default());
        app.update(); // first frame: applied as baseline

        app.world_mut().resource_mut::<GameSettings>().display.frame_cap = Some(30);
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(30.0));

        app.world_mut().resource_mut::<GameSettings>().display.frame_cap = None;
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().target_fps, None);
    }

    #[test]
    fn a_settings_apply_keeps_the_state_cap() {
        let mut app = make_app(None);
        app.insert_resource(regia_app::PacingCaps {
            menu: Some(60.0),
            ..Default::default()
        });
        app.insert_resource(regia_app::PacingState::Menu);
        app.update();
        let menu = Some(std::time::Duration::from_secs_f64(1.0 / 60.0));
        assert_eq!(app.world().resource::<FramePacing>().period(), menu);
        // The player picks "uncapped" (0) and then a cap above the menu cap.
        app.world_mut().resource_mut::<GameSettings>().display.frame_cap = Some(0);
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().target_fps, None);
        assert_eq!(
            app.world().resource::<FramePacing>().period(),
            menu,
            "the menu cap holds"
        );
        app.world_mut().resource_mut::<GameSettings>().display.frame_cap = Some(144);
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().period(), menu);
        // A player cap below the menu cap wins; back in play the player cap paces alone.
        app.world_mut().resource_mut::<GameSettings>().display.frame_cap = Some(40);
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().effective_fps(), Some(40.0));
        app.world_mut().resource_mut::<GameSettings>().display.frame_cap = Some(144);
        app.insert_resource(regia_app::PacingState::Play);
        app.update();
        assert_eq!(app.world().resource::<FramePacing>().effective_fps(), Some(144.0));
    }

    #[test]
    fn change_sends_settings_changed_with_before_and_after() {
        use regia_ecs::ecs::message::MessageCursor;
        let mut app = make_app(None);
        app.update();

        app.world_mut().resource_mut::<GameSettings>().input.sensitivity = 3.0;
        app.update();

        let messages = app.world().resource::<Messages<SettingsChanged>>();
        let mut cursor = MessageCursor::default();
        let events: Vec<_> = cursor.read(messages).cloned().collect();
        assert_eq!(events.len(), 1, "one change event: {events:?}");
        assert_eq!(events[0].before.input.sensitivity, 1.0);
        assert_eq!(events[0].after.input.sensitivity, 3.0);
    }

    #[test]
    fn corrupt_file_boots_on_defaults() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "}}}not ron{{{").expect("write junk");
        let mut app = make_app(Some(path.clone()));
        app.update(); // must not panic
        assert_eq!(app.world().resource::<GameSettings>().input.sensitivity, 1.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_boots_on_defaults() {
        let path = temp_path("missing").join("does-not-exist.ron");
        let mut app = make_app(Some(path));
        app.update();
        assert!(app.world().get_resource::<GameSettings>().is_some());
    }

    #[test]
    fn scene_motion_scale_defaults_and_reduced_motion_zeroes_shake_and_bob() {
        let motion = SceneMotion::from_accessibility(&AccessibilitySettings::default());
        assert_eq!(motion.shake_scale, 1.0);
        assert_eq!(motion.bob_scale, 1.0);
        assert_eq!(motion.fov_deg(60.0), 60.0);

        let mut settings = AccessibilitySettings {
            camera_shake_scale: 0.25,
            fov_override_deg: Some(90.0),
            ..Default::default()
        };
        let motion = SceneMotion::from_accessibility(&settings);
        assert_eq!(motion.shake_scale, 0.25);
        assert_eq!(motion.bob_scale, 1.0);
        assert_eq!(motion.fov_deg(60.0), 90.0);
        assert_eq!(
            settings.camera_shake_scale, 0.25,
            "the stored scale is not rewritten"
        );

        settings.reduced_motion = true;
        let motion = SceneMotion::from_accessibility(&settings);
        assert_eq!(motion.shake_scale, 0.0);
        assert_eq!(motion.bob_scale, 0.0);
        assert_eq!(settings.camera_shake_scale, 0.25);

        settings.camera_shake_scale = f32::NAN;
        settings.reduced_motion = false;
        assert_eq!(SceneMotion::from_accessibility(&settings).shake_scale, 0.0);
        settings.camera_shake_scale = 4.0;
        assert_eq!(SceneMotion::from_accessibility(&settings).shake_scale, 1.0);

        let mut app = make_app(None);
        app.update();
        assert_eq!(
            app.world().resource::<SceneMotion>().shake_scale,
            1.0,
            "the default apply still publishes a scale"
        );
        {
            let mut live = app.world_mut().resource_mut::<GameSettings>();
            live.accessibility.camera_shake_scale = 0.5;
            live.accessibility.reduced_motion = true;
        }
        app.update();
        let motion = *app.world().resource::<SceneMotion>();
        assert_eq!(motion.shake_scale, 0.0);
        assert_eq!(motion.bob_scale, 0.0);
        assert_eq!(
            app.world()
                .resource::<GameSettings>()
                .accessibility
                .camera_shake_scale,
            0.5
        );
    }

    #[test]
    fn player_fov_override_clamps_and_none_keeps_the_authored_value() {
        let mut settings = AccessibilitySettings::default();
        assert_eq!(SceneMotion::from_accessibility(&settings).fov_deg(60.0), 60.0);
        settings.fov_override_deg = Some(0.0);
        assert_eq!(
            SceneMotion::from_accessibility(&settings).fov_deg(60.0),
            SceneMotion::FOV_MIN_DEG
        );
        settings.fov_override_deg = Some(500.0);
        assert_eq!(
            SceneMotion::from_accessibility(&settings).fov_deg(60.0),
            SceneMotion::FOV_MAX_DEG
        );
        settings.fov_override_deg = Some(f32::NAN);
        assert_eq!(SceneMotion::from_accessibility(&settings).fov_deg(60.0), 60.0);
    }

    #[test]
    fn voice_ambience_and_mutes_round_trip_and_apply_live() {
        let mut settings = GameSettings::default();
        settings.audio.voice = 0.5;
        settings.audio.ambience = 0.25;
        settings.audio.mute_voice = true;
        settings.audio.mute_ambience = true;
        settings.audio.mute_master = false;
        let text = settings.to_ron().expect("serializes");
        assert_eq!(GameSettings::from_ron(&text).expect("parses"), settings);
        assert!(text.contains("voice"), "{text}");
        assert!(text.contains("mute_voice"), "{text}");

        let mut app = make_app(None);
        app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..regia_audio::AudioSettings::default()
        }));
        app.update();
        let audio = app.world().resource::<regia_audio::Audio>();
        assert_eq!(audio.bus_volume("Voice").expect("voice"), 0.0);
        assert_eq!(audio.bus_muted("Voice"), Some(false));
        {
            let mut live = app.world_mut().resource_mut::<GameSettings>();
            live.audio.voice = 0.5;
            live.audio.ambience = 0.25;
            live.audio.mute_voice = true;
        }
        app.update();
        let audio = app.world().resource::<regia_audio::Audio>();
        assert!((audio.bus_volume("Voice").expect("voice") + 6.0206).abs() < 1e-3);
        assert!((audio.bus_volume("Ambience").expect("ambience") + 12.041).abs() < 1e-2);
        assert_eq!(audio.bus_muted("Voice"), Some(true));
        assert_eq!(audio.bus_muted("Ambience"), Some(false));
        assert_eq!(audio.bus_muted("Master"), Some(false));
        // World stays at the runtime default: the settings file does not write it.
        assert_eq!(audio.bus_volume("World").expect("world"), 0.0);
        assert_eq!(audio.bus_muted("World"), Some(false));
    }

    #[test]
    fn motion_blur_follows_the_player_and_reduced_motion_forces_it_off() {
        let mut app = make_app(None);
        app.update();
        app.world_mut()
            .resource_mut::<regia_render::RenderSettings>()
            .profile
            .motion_blur = true;
        // An unrelated change must not touch a host bit while the player has no choice.
        app.world_mut().resource_mut::<GameSettings>().audio.master = 0.4;
        app.update();
        assert!(
            app.world()
                .resource::<regia_render::RenderSettings>()
                .profile
                .motion_blur,
            "None leaves the host bit"
        );
        app.world_mut()
            .resource_mut::<GameSettings>()
            .accessibility
            .motion_blur = Some(false);
        app.update();
        assert!(
            !app.world()
                .resource::<regia_render::RenderSettings>()
                .profile
                .motion_blur
        );
        app.world_mut()
            .resource_mut::<GameSettings>()
            .accessibility
            .motion_blur = None;
        app.update();
        assert!(
            app.world()
                .resource::<regia_render::RenderSettings>()
                .profile
                .motion_blur,
            "clearing the choice restores the captured bit"
        );
        {
            let mut settings = app.world_mut().resource_mut::<GameSettings>();
            settings.accessibility.motion_blur = Some(true);
            settings.accessibility.reduced_motion = true;
        }
        app.update();
        assert!(
            !app.world()
                .resource::<regia_render::RenderSettings>()
                .profile
                .motion_blur,
            "reduced motion forces blur off"
        );
        app.world_mut()
            .resource_mut::<GameSettings>()
            .accessibility
            .reduced_motion = false;
        app.update();
        assert!(
            app.world()
                .resource::<regia_render::RenderSettings>()
                .profile
                .motion_blur,
            "clearing reduced motion restores the player's allow bit"
        );
    }

    #[test]
    fn look_and_vibration_scales_copy_when_they_change_and_a_host_value_survives_defaults() {
        let mut app = make_app(None);
        app.insert_resource(regia_input::LookSettings {
            sensitivity: 3.0,
            invert_y: true,
        });
        let mut phone = regia_input::PhoneVibration::default();
        phone.set_scale(0.2);
        app.insert_resource(phone);
        app.update();
        assert_eq!(
            app.world().resource::<regia_input::LookSettings>().sensitivity,
            3.0
        );
        assert_eq!(app.world().resource::<regia_input::PhoneVibration>().scale(), 0.2);
        {
            let mut settings = app.world_mut().resource_mut::<GameSettings>();
            settings.input.sensitivity = 2.0;
            settings.input.invert_y = true;
            settings.accessibility.vibration_scale = 0.5;
        }
        app.update();
        let look = *app.world().resource::<regia_input::LookSettings>();
        assert_eq!(look.sensitivity, 2.0);
        assert!(look.invert_y);
        assert_eq!(app.world().resource::<regia_input::PhoneVibration>().scale(), 0.5);
    }

    // ---------------------------------------------------------------------------------------------
    // ADR-7880: display.resolution, display.fullscreen and display.vsync reach the primary window.
    // ---------------------------------------------------------------------------------------------

    fn spawn_primary(app: &mut App, window: regia_platform::Window) -> Entity {
        app.world_mut()
            .spawn((window, regia_platform::PrimaryWindow))
            .id()
    }

    fn primary(app: &App, entity: Entity) -> regia_platform::Window {
        app.world()
            .get::<regia_platform::Window>(entity)
            .expect("primary window")
            .clone()
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn display_vsync_switches_the_window_present_mode_live() {
        use regia_platform::PresentMode;
        let mut app = make_app(None);
        let window = spawn_primary(&mut app, regia_platform::Window::default());
        app.update();
        assert_eq!(primary(&app, window).present_mode, PresentMode::Vsync);
        app.world_mut().resource_mut::<GameSettings>().display.vsync = false;
        app.update();
        assert_eq!(primary(&app, window).present_mode, PresentMode::Immediate);
        let applied = app.world().resource::<DisplayApplied>().clone();
        assert_eq!(applied.window, Some(window));
        assert_eq!(applied.present_mode, Some(PresentMode::Immediate));
        assert_eq!(applied.present, None, "no renderer surface in this app");
        assert_eq!(applied.vsync_note, None);
        app.world_mut().resource_mut::<GameSettings>().display.vsync = true;
        app.update();
        assert_eq!(primary(&app, window).present_mode, PresentMode::Vsync);
        assert_eq!(
            app.world().resource::<DisplayApplied>().present_mode,
            Some(PresentMode::Vsync)
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn a_host_present_mode_stands_until_vsync_changes_and_comes_back_with_it() {
        use regia_platform::PresentMode;
        let mut app = make_app(None);
        let window = spawn_primary(
            &mut app,
            regia_platform::Window {
                present_mode: PresentMode::VsyncRelaxed,
                ..regia_platform::Window::default()
            },
        );
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(
            primary(&app, window).present_mode,
            PresentMode::VsyncRelaxed,
            "the default vsync setting leaves the host's mode alone"
        );
        app.world_mut().resource_mut::<GameSettings>().display.vsync = false;
        app.update();
        assert_eq!(primary(&app, window).present_mode, PresentMode::Immediate);
        app.world_mut().resource_mut::<GameSettings>().display.vsync = true;
        app.update();
        assert_eq!(primary(&app, window).present_mode, PresentMode::VsyncRelaxed);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn vsync_on_lifts_a_host_uncapped_policy_and_leaves_vrr() {
        let mut app = make_app(None);
        spawn_primary(&mut app, regia_platform::Window::default());
        app.insert_resource(regia_render::OutputSettings {
            present: regia_render::PresentPolicy::Uncapped,
            ..Default::default()
        });
        app.update();
        assert_eq!(
            app.world().resource::<regia_render::OutputSettings>().present,
            regia_render::PresentPolicy::Uncapped,
            "the baseline leaves the host's policy"
        );
        app.world_mut().resource_mut::<GameSettings>().display.vsync = false;
        app.update();
        app.world_mut().resource_mut::<GameSettings>().display.vsync = true;
        app.update();
        assert_eq!(
            app.world().resource::<regia_render::OutputSettings>().present,
            regia_render::PresentPolicy::Vsync
        );
        // Variable refresh picks its own mode: the policy stays, and vsync off says why.
        app.world_mut()
            .resource_mut::<regia_render::OutputSettings>()
            .present = regia_render::PresentPolicy::Vrr;
        app.world_mut().resource_mut::<GameSettings>().display.vsync = false;
        app.update();
        app.world_mut().resource_mut::<GameSettings>().display.vsync = true;
        app.update();
        assert_eq!(
            app.world().resource::<regia_render::OutputSettings>().present,
            regia_render::PresentPolicy::Vrr
        );
        app.world_mut().resource_mut::<GameSettings>().display.vsync = false;
        app.update();
        assert_eq!(
            app.world().resource::<DisplayApplied>().vsync_note,
            Some(VSYNC_HOST_VRR)
        );
    }

    #[test]
    fn vsync_off_on_android_keeps_the_display_paced_mode() {
        use regia_platform::PresentMode;
        assert_eq!(
            vsync_present_mode(false, PresentMode::Vsync, true),
            (PresentMode::Vsync, Some(VSYNC_ANDROID_DISPLAY_PACED))
        );
        assert_eq!(
            vsync_present_mode(true, PresentMode::Vsync, true),
            (PresentMode::Vsync, None)
        );
        assert_eq!(
            vsync_present_mode(false, PresentMode::Vsync, false),
            (PresentMode::Immediate, None)
        );
        // A host that started without vsync gets plain vsync when the player turns it on.
        assert_eq!(
            vsync_present_mode(true, PresentMode::Immediate, false),
            (PresentMode::Vsync, None)
        );
        assert_eq!(
            vsync_note(false, true, false, None),
            Some(VSYNC_ANDROID_DISPLAY_PACED)
        );
        assert_eq!(vsync_note(true, true, false, None), None);
    }

    #[test]
    fn vsync_off_on_a_fifo_only_surface_reports_the_fallback() {
        let summary = |effective, waits| regia_render::swapchain::PresentSummary {
            planned: "AutoNoVsync",
            effective,
            waits_for_vblank: waits,
            offers_immediate: false,
            offers_mailbox: false,
        };
        assert_eq!(
            vsync_note(false, false, false, Some(summary("Fifo", true))),
            Some(VSYNC_NO_TEARING_MODE)
        );
        assert_eq!(
            vsync_note(false, false, false, Some(summary("Mailbox", false))),
            None
        );
        assert_eq!(
            vsync_note(false, false, false, None),
            None,
            "no surface yet: nothing to say"
        );
        for code in [VSYNC_ANDROID_DISPLAY_PACED, VSYNC_NO_TEARING_MODE, VSYNC_HOST_VRR] {
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn display_resolution_and_fullscreen_reach_the_window_and_restore_the_host_values() {
        use regia_platform::WindowMode;
        let mut app = make_app(None);
        let window = spawn_primary(
            &mut app,
            regia_platform::Window {
                logical_size: (1024.0, 600.0),
                ..regia_platform::Window::default()
            },
        );
        app.update();
        assert_eq!(primary(&app, window).logical_size, (1024.0, 600.0));
        {
            let mut settings = app.world_mut().resource_mut::<GameSettings>();
            settings.display.resolution = Some((1600, 900));
            settings.display.fullscreen = Some(true);
        }
        app.update();
        let live = primary(&app, window);
        assert_eq!(live.logical_size, (1600.0, 900.0));
        assert_eq!(live.mode, WindowMode::BorderlessFullscreen);
        let applied = app.world().resource::<DisplayApplied>().clone();
        assert_eq!(applied.logical_size, Some((1600.0, 900.0)));
        assert_eq!(applied.mode, Some(WindowMode::BorderlessFullscreen));
        app.world_mut().resource_mut::<GameSettings>().display.fullscreen = Some(false);
        app.update();
        assert_eq!(primary(&app, window).mode, WindowMode::Windowed);
        {
            let mut settings = app.world_mut().resource_mut::<GameSettings>();
            settings.display.resolution = None;
            settings.display.fullscreen = None;
        }
        app.update();
        let live = primary(&app, window);
        assert_eq!(
            live.logical_size,
            (1024.0, 600.0),
            "None restores the window's own size"
        );
        assert_eq!(live.mode, WindowMode::Windowed);
    }

    #[test]
    fn saved_display_settings_reach_a_window_created_later_and_a_recreated_one() {
        use regia_platform::WindowMode;
        let path = regia_test_utils::unique_temp_dir("display-late-window").join("settings.ron");
        let mut saved = GameSettings::default();
        saved.display.resolution = Some((1920, 1080));
        saved.display.fullscreen = Some(true);
        std::fs::write(&path, saved.to_ron().expect("ron")).expect("write settings");
        let mut app = make_app(Some(path));
        // No window yet (a window appears on the first resume): nothing to apply, nothing lost.
        app.update();
        app.update();
        assert_eq!(app.world().resource::<DisplayApplied>().window, None);
        let first = spawn_primary(&mut app, regia_platform::Window::default());
        app.update();
        assert_eq!(primary(&app, first).logical_size, (1920.0, 1080.0));
        assert_eq!(primary(&app, first).mode, WindowMode::BorderlessFullscreen);
        // Android re-creates the window on resume: the new one gets the settings again.
        app.world_mut().despawn(first);
        let second = spawn_primary(&mut app, regia_platform::Window::default());
        app.update();
        assert_eq!(primary(&app, second).logical_size, (1920.0, 1080.0));
        assert_eq!(primary(&app, second).mode, WindowMode::BorderlessFullscreen);
        assert_eq!(app.world().resource::<DisplayApplied>().window, Some(second));
    }

    #[test]
    fn display_applied_reports_the_monitor_clamp_and_a_refusal() {
        let mut app = make_app(None);
        let window = spawn_primary(&mut app, regia_platform::Window::default());
        app.world_mut()
            .entity_mut(window)
            .insert(regia_platform::WindowStatus {
                monitor: Some(regia_platform::MonitorExtent {
                    physical_size: (2560, 1440),
                    scale_factor: 2.0,
                }),
                applied_logical_size: Some((1280.0, 720.0)),
                refused: Some(regia_platform::WindowRefusal {
                    code: regia_platform::WINDOW_SIZE_TOO_SMALL,
                    requested_size: (200.0, 100.0),
                    requested_mode: regia_platform::WindowMode::Windowed,
                }),
                ..Default::default()
            });
        app.update();
        let applied = app.world().resource::<DisplayApplied>().clone();
        assert_eq!(applied.monitor_logical_size, Some((1280.0, 720.0)));
        assert_eq!(applied.applied_logical_size, Some((1280.0, 720.0)));
        assert_eq!(
            applied.refused,
            Some((regia_platform::WINDOW_SIZE_TOO_SMALL, (200.0, 100.0)))
        );
    }

    #[test]
    fn display_settings_from_older_files_load_without_fullscreen() {
        let text = "(resolution: Some((1280, 720)), vsync: false, frame_cap: None)";
        let display: DisplaySettings = ron::from_str(text).expect("older display section parses");
        assert_eq!(display.fullscreen, None);
        assert_eq!(display.resolution, Some((1280, 720)));
        assert!(!display.vsync);
    }
}
