// SPDX-License-Identifier: MPL-2.0
//! Automatic quality detection (book `user-guide/auto-quality.md`, ADR-157).
//!
//! On a first launch the game has no saved quality setting, so the engine picks one from the GPU:
//! [`AdapterProfile`] reads the adapter (vendor, device id, name, integrated or discrete, VRAM,
//! limits and features), [`detect`] walks the [`GpuTierTable`] (a RON data table shipped in the
//! engine, `gpu_tiers.ron`) and falls back to VRAM and limits for a GPU the table does not know.
//! The result is a [`QualityTier`] plus a one-line reason ("NVIDIA GeForce RTX 3080 Ti, 12 GB →
//! High"). [`QualityProbe`] is the optional confirmation. It starts after the boot splash is
//! gone and the game-frame warm is back, and draws the first scene offscreen on a warm worker.
//! It never holds the splash. The default run drops 0.75 s and 30 frames, then measures 2.25 s;
//! a p95 GPU frame time over budget steps the tier down once.
//!
//! Persisting the choice and never overriding the player's own is the framework's job
//! (`regia_framework::auto_quality`); this module is pure policy plus the render-side probe.

mod probe;
mod profile_name;

#[cfg(test)]
use probe::ProbeRun;
pub(crate) use probe::probe_start_allowed;
pub use probe::{ProbeConfig, ProbeResult, ProbeSource, QualityProbe, percentile};
pub use profile_name::ProfileName;

use regia_app::prelude::{Resource, bevy_ecs};
use serde::{Deserialize, Serialize};

use crate::{GiLook, RenderDevice, RenderProfile, RenderProfilePreset, RenderSettings};

/// A quality level the detector can pick, cheapest first within each platform family.
///
/// The desktop levels map onto the render presets: `Low` = Desktop Low, `High` = Desktop High,
/// `Ultra` = Desktop Ultra. `Medium` is Desktop High rendered at 0.67 scale (temporal upscale)
/// with the Balanced lighting look: about half of High's GPU cost at the same output size
/// (gpu-farm4, 4K High at 0.67: 3.30 ms against 6.55 ms native on the RTX 5090).
///
/// `VeryLow` (ADR-7882, v0.5.1) is the step below `Low`: Desktop Low drawn at
/// [`QualityTier::VERY_LOW_RENDER_SCALE`] with the cheaper columns of
/// [`QualityTier::profile`], for GPUs that miss 30 fps at Low (the Intel N150 class). It is
/// appended after `Ultra` so the existing variants keep their places; cheapest-first lists
/// ([`QualityTier::EVERY`]) put it just below `Low`. Budgets and baked content follow
/// `Low` ([`QualityTier::content_tier`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QualityTier {
    /// Mobile Low preset.
    MobileLow,
    /// Mobile High preset (the Android default).
    MobileHigh,
    /// The Steam Deck preset (1280×800, 40 fps cap, 70 % temporal upscale).
    Deck,
    /// Desktop Low preset.
    Low,
    /// Desktop High at 0.67 render scale, Balanced lighting.
    Medium,
    /// Desktop High preset.
    High,
    /// Desktop Ultra preset.
    Ultra,
    /// Desktop Low at [`QualityTier::VERY_LOW_RENDER_SCALE`] with cheaper columns (ADR-7882): the
    /// step below `Low` for GPUs that miss 30 fps there.
    VeryLow,
}

impl QualityTier {
    /// The seven tiers through v0.5.0, cheapest first. Lists, indices and bake files keyed on
    /// them (procedural bakes per tier, the procgen quality pins) stay as they were:
    /// [`Self::VeryLow`] is not in it, it uses `Low`'s content ([`Self::content_tier`]).
    /// [`Self::EVERY`] lists every tier.
    pub const ALL: [Self; 7] = [
        Self::MobileLow,
        Self::MobileHigh,
        Self::Deck,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Ultra,
    ];

    /// Every tier, cheapest first: [`Self::ALL`] with [`Self::VeryLow`] just below `Low`.
    pub const EVERY: [Self; 8] = [
        Self::MobileLow,
        Self::MobileHigh,
        Self::Deck,
        Self::VeryLow,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Ultra,
    ];

    /// Render scale of [`QualityTier::Medium`].
    pub const MEDIUM_RENDER_SCALE: f32 = 0.67;

    /// Render scale of [`QualityTier::VeryLow`] (ADR-7882): a third of the output in each axis,
    /// the upscalers' Ultra Performance size (Desktop Low draws at 0.75), upscaled to the output
    /// by SGSR 1. On the Intel N150, 0.5 still missed 30 fps (GPU p50 43.6 ms) and a third held it
    /// (28.9 ms; perf-log, 2026-11-15).
    pub const VERY_LOW_RENDER_SCALE: f32 = 1.0 / 3.0;

    /// The frame rate below which the confirmation probe takes `Low` down to [`Self::VeryLow`]
    /// (ADR-7882): `Low` steps down only when it misses this floor (or the probe's own target,
    /// whichever is lower), so a GPU that holds 30 fps at Low keeps Low whatever its 60 fps
    /// target says.
    pub const VERY_LOW_TRIGGER_FPS: f32 = 30.0;

    /// Stable name (`"High"`, `"MobileLow"`, …).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::MobileLow => "MobileLow",
            Self::MobileHigh => "MobileHigh",
            Self::Deck => "Deck",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Ultra => "Ultra",
            Self::VeryLow => "VeryLow",
        }
    }

    /// Parses a name (case-insensitive, `_`/`-` ignored; any of [`Self::EVERY`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let key: String = name
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        Self::EVERY.into_iter().find(|t| t.name().to_lowercase() == key)
    }

    /// Whether this is a desktop level (`VeryLow`, `Low`, `Medium`, `High`, `Ultra`).
    #[must_use]
    pub const fn is_desktop(self) -> bool {
        matches!(
            self,
            Self::VeryLow | Self::Low | Self::Medium | Self::High | Self::Ultra
        )
    }

    /// The tier whose budgets and baked content this tier uses: `Low` for [`Self::VeryLow`]
    /// (it is Low drawn cheaper, ADR-7882), the tier itself otherwise. Systems that size work by
    /// tier (particles, procedural bakes) read this one.
    #[must_use]
    pub const fn content_tier(self) -> Self {
        match self {
            Self::VeryLow => Self::Low,
            other => other,
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::MobileLow => 0,
            Self::MobileHigh => 1,
            Self::Deck => 2,
            Self::VeryLow => 3,
            Self::Low => 4,
            Self::Medium => 5,
            Self::High => 6,
            Self::Ultra => 7,
        }
    }

    /// One level cheaper on the same platform, `None` at the bottom (Very Low, Mobile Low) and for
    /// the Deck (its preset is the Deck's own row; the probe never leaves it). `Low` steps to
    /// [`Self::VeryLow`] (ADR-7882); the probe takes that step only below
    /// [`Self::VERY_LOW_TRIGGER_FPS`] ([`judge_probe`]) and the VRAM floors never do
    /// ([`detect`]).
    #[must_use]
    pub const fn step_down(self) -> Option<Self> {
        match self {
            Self::Ultra => Some(Self::High),
            Self::High => Some(Self::Medium),
            Self::Medium => Some(Self::Low),
            Self::Low => Some(Self::VeryLow),
            Self::MobileHigh => Some(Self::MobileLow),
            Self::VeryLow | Self::MobileLow | Self::Deck => None,
        }
    }

    /// The tier this platform can run: on Android a desktop level becomes Mobile High (Low and
    /// Very Low become Mobile Low) and the Deck Mobile High; elsewhere a mobile level becomes Low.
    #[must_use]
    pub const fn for_platform(self, android: bool) -> Self {
        if android {
            match self {
                Self::Low | Self::VeryLow => Self::MobileLow,
                Self::Medium | Self::High | Self::Ultra | Self::Deck => Self::MobileHigh,
                other => other,
            }
        } else {
            match self {
                Self::MobileLow | Self::MobileHigh => Self::Low,
                other => other,
            }
        }
    }

    /// The render preset this tier starts from.
    #[must_use]
    pub const fn preset(self) -> RenderProfilePreset {
        match self {
            Self::MobileLow => RenderProfilePreset::MobileLow,
            Self::MobileHigh => RenderProfilePreset::MobileHigh,
            Self::Deck => RenderProfilePreset::Deck,
            Self::VeryLow | Self::Low => RenderProfilePreset::DesktopLow,
            Self::Medium | Self::High => RenderProfilePreset::DesktopHigh,
            Self::Ultra => RenderProfilePreset::DesktopUltra,
        }
    }

    /// Writes [`Self::VeryLow`]'s columns over a Desktop Low profile (ADR-7882): the render scale
    /// [`Self::VERY_LOW_RENDER_SCALE`] with SGSR 1 to the output (the non-temporal tiers' spatial
    /// upscaler, ADR-102; Desktop Low upscales its 0.75 bilinearly), and the integrated-GPU texture
    /// streaming cap: a budgeted policy streams at most
    /// [`crate::low_memory::INTEGRATED_STREAM_BUDGET_MIB`] with lazy first use on any RAM size
    /// (ADR-8878). [`RenderSettings::preset`]
    /// still answers Desktop Low for the result, so systems that pick their own tier by preset
    /// (water, foliage) stay on Low's.
    pub fn very_low_columns(profile: &mut RenderProfile) {
        profile.render_scale = Self::VERY_LOW_RENDER_SCALE;
        profile.spatial_upscaler = crate::SpatialUpscaler::Sgsr;
        // The integrated-GPU texture cap as the tier's own column (ADR-7977 build B, ADR-8878):
        // a 16 GB integrated laptop is "large" by the low-memory rule and would stream with
        // Desktop Low's 1 GiB. A discrete card still grows it to its VRAM share every frame.
        let streaming = &mut profile.texture_streaming;
        if streaming.enabled && streaming.budget_mib > 0 {
            streaming.budget_mib = streaming
                .budget_mib
                .min(crate::low_memory::INTEGRATED_STREAM_BUDGET_MIB);
            streaming.eager = false;
        }
    }

    /// The tier a render preset is (Desktop High → `High`).
    #[must_use]
    pub const fn of_preset(preset: RenderProfilePreset) -> Self {
        match preset {
            RenderProfilePreset::MobileLow => Self::MobileLow,
            RenderProfilePreset::MobileHigh => Self::MobileHigh,
            RenderProfilePreset::Deck => Self::Deck,
            RenderProfilePreset::DesktopLow => Self::Low,
            RenderProfilePreset::DesktopHigh => Self::High,
            // The offline profile sits on the Ultra tier (ADR-1403).
            RenderProfilePreset::DesktopUltra | RenderProfilePreset::Cinematic => Self::Ultra,
        }
    }

    /// The full render profile of this tier.
    #[must_use]
    pub fn profile(self) -> RenderProfile {
        let mut profile = RenderProfile::from_preset(self.preset());
        if self == Self::Medium {
            profile.render_scale = Self::MEDIUM_RENDER_SCALE;
            profile.gi_probes = false;
            // A mid tier: bent normals are a High and Ultra cost (ADR-1375).
            profile.bent_normals = false;
        }
        if self == Self::VeryLow {
            Self::very_low_columns(&mut profile);
        }
        profile
    }

    /// Switches `settings` to this tier live ([`RenderSettings::apply_preset`], plus Medium's
    /// render scale and lighting default, and Very Low's [`Self::very_low_columns`]). The player's
    /// lighting option (`gi_look`) is kept.
    pub fn apply(self, settings: &mut RenderSettings) {
        settings.apply_preset(self.preset());
        if self == Self::VeryLow {
            Self::very_low_columns(&mut settings.profile);
        }
        if self == Self::Medium {
            settings.profile.render_scale = Self::MEDIUM_RENDER_SCALE;
            settings.profile.gi_probes = false;
            settings.profile.bent_normals = false;
            // Medium is not a row the dense-scene default was measured on (ADR-3149): drop the
            // path `apply_preset` switched on for Desktop High.
            settings.retract_unmeasured_visibility_default();
        }
    }

    /// [`Self::apply`] on `adapter` (`None` = not known yet), plus the GPU-dependent levers:
    /// the terrain virtual texture on Desktop Low when [`Self::terrain_vt_default`] holds
    /// (ADR-157 addendum v0.1.2).
    pub fn apply_for_gpu(self, settings: &mut RenderSettings, adapter: Option<&AdapterProfile>) {
        self.apply_for_gpu_with(settings, adapter, &TerrainVtPolicy::default());
    }

    /// [`Self::apply_for_gpu`] under a host's [`TerrainVtPolicy`] (ADR-2122): the lever follows
    /// [`Self::terrain_vt_default_with`]. The default policy is [`Self::apply_for_gpu`].
    pub fn apply_for_gpu_with(
        self,
        settings: &mut RenderSettings,
        adapter: Option<&AdapterProfile>,
        policy: &TerrainVtPolicy,
    ) {
        self.apply(settings);
        let on = adapter.is_some_and(|a| self.terrain_vt_default_with(a, std::env::consts::OS, policy));
        policy.set_lever(&mut settings.profile.terrain_budget, on);
    }

    /// Device-local memory from which Desktop Low turns the terrain virtual texture on
    /// (ADR-157 addendum v0.1.2): the atlas is 671 MB (four surface maps and the horizon map)
    /// and DO27's heaviest Low frame peaked at ~1.9 GB of an 8 GB GTX 1080 with it, so 6 GB
    /// cards keep ample headroom.
    pub const TERRAIN_VT_MIN_VRAM_MB: u64 = 6 * 1024;

    /// PCI vendors the terrain virtual texture default skips until it is measured on their
    /// driver (ADR-157 addendum v0.1.2): AMD (0x1002; no RADV measurement yet). ADR-2122 traced
    /// the exclusion: it is that missing measurement, not a fault found in the texture's code
    /// or a driver limitation; [`TerrainVtPolicy::unmeasured_vendors`] lifts it.
    pub const TERRAIN_VT_EXCLUDED_VENDORS: [u32; 1] = [0x1002];

    /// Whether this tier turns the terrain virtual texture on by default on `adapter`: Desktop
    /// Low on a discrete desktop GPU with at least [`Self::TERRAIN_VT_MIN_VRAM_MB`] of known
    /// device-local memory. On the GTX 1080 it took DO27's Switchbacks from 26 to 15 ms GPU p50
    /// at 1080p (v012-pascal-vt), the terrain shading being most of a Low frame on cards of that
    /// class. High and Ultra keep today's frame (ADR-041); integrated GPUs, the Deck and phones
    /// are not measured with it. Linux only, and not on AMD, until the texture is measured on a
    /// Windows driver and on RADV ([`Self::terrain_vt_default_on`]).
    #[must_use]
    pub fn terrain_vt_default(self, adapter: &AdapterProfile) -> bool {
        self.terrain_vt_default_on(adapter, std::env::consts::OS)
    }

    /// [`Self::terrain_vt_default`] for a build whose `std::env::consts::OS` is `os`: only
    /// `"linux"` qualifies (the measured GTX 1080 and Arc BMG G31 ran there; no Windows driver
    /// has run the texture), and a vendor in [`Self::TERRAIN_VT_EXCLUDED_VENDORS`] never does.
    #[must_use]
    pub fn terrain_vt_default_on(self, adapter: &AdapterProfile, os: &str) -> bool {
        self.terrain_vt_default_with(adapter, os, &TerrainVtPolicy::default())
    }

    /// Device-local memory from which Desktop High turns the terrain virtual texture on when
    /// [`TerrainVtPolicy::high`] asks for it (ADR-2122): the Desktop High budget's 10 GiB
    /// (`MemoryBudget::for_tier(BudgetTier::DesktopHigh)`) plus the ~1.5 GiB the desktop,
    /// driver reserves and overlays keep (the reasoning of the Minimum budget), so a 12 GB card
    /// (12 282 MB reported) qualifies and a 10 GB one does not.
    pub const TERRAIN_VT_HIGH_MIN_VRAM_MB: u64 = 11 * 1024 + 512;

    /// GPU memory the terrain virtual texture may take on Desktop High (ADR-2122): 768 MiB,
    /// 7.5 % of the Desktop High budget's 10 GiB. The profile's settings take 671 MB of atlas
    /// (four RGBA16F maps and the horizon map at 32² slots of 128² texels) plus a few MB of page
    /// tables and feedback buffers; `regia_terrain`'s `terrain_vt_tiers` test measures them.
    pub const TERRAIN_VT_HIGH_BUDGET_BYTES: u64 = 768 * 1024 * 1024;

    /// [`Self::terrain_vt_default_on`] under `policy` (ADR-2122). Desktop Low as before; Desktop
    /// High too when [`TerrainVtPolicy::high`] is set and the card has
    /// [`Self::TERRAIN_VT_HIGH_MIN_VRAM_MB`]. `os` must be `"linux"`, or `"windows"` with
    /// [`TerrainVtPolicy::windows`]; a vendor in [`Self::TERRAIN_VT_EXCLUDED_VENDORS`] needs
    /// [`TerrainVtPolicy::unmeasured_vendors`]. Always a discrete, non-Android GPU with known
    /// device-local memory.
    #[must_use]
    pub fn terrain_vt_default_with(
        self,
        adapter: &AdapterProfile,
        os: &str,
        policy: &TerrainVtPolicy,
    ) -> bool {
        let min_vram_mb = match self {
            // Very Low is Low drawn cheaper (ADR-7882): the same lever.
            Self::Low | Self::VeryLow => Self::TERRAIN_VT_MIN_VRAM_MB,
            Self::High if policy.high => Self::TERRAIN_VT_HIGH_MIN_VRAM_MB,
            _ => return false,
        };
        let os_ok = os == "linux" || (policy.windows && os == "windows");
        let vendor_ok =
            policy.unmeasured_vendors || !Self::TERRAIN_VT_EXCLUDED_VENDORS.contains(&adapter.vendor);
        os_ok
            && vendor_ok
            && !adapter.android
            && adapter.class == DeviceClass::Discrete
            && adapter
                .vram_mb
                .or(adapter.dx12_vram_mb)
                .is_some_and(|mb| mb >= min_vram_mb)
    }

    /// The tier `settings` currently runs, `None` for a profile no tier produces. The
    /// GPU-dependent terrain virtual texture lever ([`Self::apply_for_gpu`]) is ignored.
    #[must_use]
    pub fn of_settings(settings: &RenderSettings) -> Option<Self> {
        let mut profile = settings.profile.clone();
        profile.terrain_budget.virtual_texture = false;
        profile.terrain_budget.vt_late_depth = false;
        Self::EVERY.into_iter().find(|t| t.profile() == profile)
    }

    /// The lighting look this tier ships with (Depth on High and Ultra, Balanced elsewhere).
    #[must_use]
    pub fn default_gi_look(self) -> GiLook {
        GiLook::of_profile(&self.profile())
    }

    /// The frame rate the confirmation probe holds this tier to by default: 60 on desktop, 40 on
    /// the Deck (its preset caps there), 30 on mobile and on Very Low (the floor it exists for,
    /// [`Self::VERY_LOW_TRIGGER_FPS`]; the probe never steps below it).
    #[must_use]
    pub const fn default_target_fps(self) -> f32 {
        match self {
            Self::MobileLow | Self::MobileHigh | Self::VeryLow => 30.0,
            Self::Deck => 40.0,
            Self::Low | Self::Medium | Self::High | Self::Ultra => 60.0,
        }
    }
}

/// Where the quality detector may turn the terrain virtual texture on beyond its measured default
/// (ADR-2122). Every field is off by default, which is the ADR-157 addendum v0.1.2 rule: Desktop
/// Low, Linux, not AMD. A host inserts it as a resource before the settings plugin applies a
/// preset (or sets it through the agent's `quality.set_terrain_vt_policy`) to opt in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Resource)]
#[serde(default)]
pub struct TerrainVtPolicy {
    /// Desktop High also turns it on, on discrete cards with
    /// [`QualityTier::TERRAIN_VT_HIGH_MIN_VRAM_MB`] (the texture's memory stays within
    /// [`QualityTier::TERRAIN_VT_HIGH_BUDGET_BYTES`]). Medium and Ultra never do.
    pub high: bool,
    /// Windows builds qualify as Linux ones do. With Vulkan (which Windows enumerates first) the
    /// texture runs with forced early depth tests; with DX12 it needs the late-depth path
    /// (`VirtualTextureSettings::late_depth_fallback`), which the lever then asks for
    /// ([`Self::late_depth`]), and DX12 VRAM comes from DXGI. Not
    /// measured on any Windows driver yet.
    pub windows: bool,
    /// Vendors in [`QualityTier::TERRAIN_VT_EXCLUDED_VENDORS`] (AMD) qualify. The exclusion is
    /// a missing RADV measurement, not a known fault: ADR-2122 found no AMD-specific hazard in
    /// the texture's code, and lavapipe with the Khronos validation layer reports nothing.
    pub unmeasured_vendors: bool,
}

impl TerrainVtPolicy {
    /// Whether a lever this policy turns on may take the late-depth path
    /// ([`crate::TerrainBudget::vt_late_depth`]): only with [`Self::windows`], since the only
    /// backend it admits without forced early depth tests is DX12. Off by default, so a
    /// profile that asks for the texture on a DX12 or Metal device keeps it off as before.
    #[must_use]
    pub const fn late_depth(&self) -> bool {
        self.windows
    }

    /// Writes the detector's decision `on` into `budget`: the lever, and with it
    /// [`Self::late_depth`] (off together with the lever).
    pub fn set_lever(&self, budget: &mut crate::TerrainBudget, on: bool) {
        budget.virtual_texture = on;
        budget.vt_late_depth = on && self.late_depth();
    }
}

impl std::fmt::Display for QualityTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Integrated, discrete, … (wgpu's `DeviceType`, serialisable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceClass {
    /// A discrete GPU with its own memory.
    Discrete,
    /// An integrated GPU sharing system memory.
    Integrated,
    /// A virtual GPU (VM passthrough layer).
    Virtual,
    /// A software rasteriser (lavapipe, llvmpipe, WARP).
    Cpu,
    /// The driver did not say.
    Other,
}

impl DeviceClass {
    /// From wgpu's device type.
    #[must_use]
    pub const fn from_wgpu(device_type: wgpu::DeviceType) -> Self {
        match device_type {
            wgpu::DeviceType::DiscreteGpu => Self::Discrete,
            wgpu::DeviceType::IntegratedGpu => Self::Integrated,
            wgpu::DeviceType::VirtualGpu => Self::Virtual,
            wgpu::DeviceType::Cpu => Self::Cpu,
            wgpu::DeviceType::Other => Self::Other,
        }
    }

    /// Lower-case name (`"discrete"`, …).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Discrete => "discrete",
            Self::Integrated => "integrated",
            Self::Virtual => "virtual",
            Self::Cpu => "cpu",
            Self::Other => "other",
        }
    }
}

/// What the detector knows about a GPU. Built from a live adapter ([`AdapterProfile::from_device`])
/// or by hand (tests, the agent's dry run).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdapterProfile {
    /// Adapter name as the driver reports it ("NVIDIA GeForce RTX 3080 Ti").
    pub name: String,
    /// PCI vendor id (0x10de NVIDIA, 0x1002 AMD, 0x8086 Intel, 0x5143 Qualcomm, 0x13b5 Arm).
    #[serde(default)]
    pub vendor: u32,
    /// PCI device id.
    #[serde(default)]
    pub device: u32,
    /// Integrated, discrete, …
    pub class: DeviceClass,
    /// Largest device-local memory heap in MiB, `None` when the backend does not say (DX12).
    #[serde(default)]
    pub vram_mb: Option<u64>,
    /// A DX12 adapter's dedicated video memory in MiB (DXGI), `None` elsewhere (ADR-2122). Only
    /// the terrain virtual texture's rule reads it ([`QualityTier::terrain_vt_default_with`]);
    /// tier detection keeps treating DX12 VRAM as unknown, as before.
    #[serde(default)]
    pub dx12_vram_mb: Option<u64>,
    /// Backend name (`"Vulkan"`, `"Dx12"`).
    #[serde(default)]
    pub backend: String,
    /// Driver name and version.
    #[serde(default)]
    pub driver: String,
    /// `max_texture_dimension_2d`.
    #[serde(default = "default_texture_2d")]
    pub max_texture_dimension_2d: u32,
    /// `max_storage_buffers_per_shader_stage`.
    #[serde(default = "default_storage_buffers")]
    pub max_storage_buffers_per_shader_stage: u32,
    /// `max_bind_groups`.
    #[serde(default = "default_bind_groups")]
    pub max_bind_groups: u32,
    /// BC texture compression (every desktop GPU; desktop content is BC).
    #[serde(default = "default_true")]
    pub block_compression: bool,
    /// GPU timestamp queries (the confirmation probe's clock).
    #[serde(default = "default_true")]
    pub timestamps: bool,
    /// Running on Android.
    #[serde(default)]
    pub android: bool,
}

const fn default_texture_2d() -> u32 {
    16384
}
const fn default_storage_buffers() -> u32 {
    16
}
const fn default_bind_groups() -> u32 {
    8
}
const fn default_true() -> bool {
    true
}

impl AdapterProfile {
    /// A discrete desktop GPU called `name` with default limits (tests, dry runs).
    #[must_use]
    pub fn named(name: &str, vendor: u32, vram_mb: Option<u64>) -> Self {
        Self {
            name: name.to_string(),
            vendor,
            device: 0,
            class: DeviceClass::Discrete,
            vram_mb,
            dx12_vram_mb: None,
            backend: String::new(),
            driver: String::new(),
            max_texture_dimension_2d: default_texture_2d(),
            max_storage_buffers_per_shader_stage: default_storage_buffers(),
            max_bind_groups: default_bind_groups(),
            block_compression: true,
            timestamps: true,
            android: false,
        }
    }

    /// The adapter of `device`, with its VRAM.
    #[must_use]
    pub fn from_device(device: &RenderDevice) -> Self {
        Self::from_adapter(&device.adapter)
    }

    /// Reads `adapter` (info, limits, features, VRAM).
    #[must_use]
    pub fn from_adapter(adapter: &wgpu::Adapter) -> Self {
        let info = adapter.get_info();
        let limits = adapter.limits();
        let features = adapter.features();
        Self {
            name: info.name.clone(),
            vendor: info.vendor,
            device: info.device,
            class: DeviceClass::from_wgpu(info.device_type),
            vram_mb: device_local_mib(adapter),
            dx12_vram_mb: dx12_dedicated_mib(adapter),
            backend: format!("{:?}", info.backend),
            driver: format!("{} {}", info.driver, info.driver_info).trim().to_string(),
            max_texture_dimension_2d: limits.max_texture_dimension_2d,
            max_storage_buffers_per_shader_stage: limits.max_storage_buffers_per_shader_stage,
            max_bind_groups: limits.max_bind_groups,
            block_compression: features.contains(wgpu::Features::TEXTURE_COMPRESSION_BC),
            timestamps: features.contains(wgpu::Features::TIMESTAMP_QUERY),
            android: cfg!(target_os = "android"),
        }
    }

    /// "12 GB" (rounded GiB), "512 MB", or `None` when unknown.
    #[must_use]
    pub fn vram_label(&self) -> Option<String> {
        self.vram_mb.map(|mb| {
            if mb >= 1024 {
                format!("{} GB", (mb + 512) / 1024)
            } else {
                format!("{mb} MB")
            }
        })
    }
}

/// `DXGI_ADAPTER_DESC2::DedicatedVideoMemory` of a DX12 adapter, in MiB (`None` on other
/// backends or when DXGI reports none; [`AdapterProfile::dx12_vram_mb`], ADR-2122).
#[cfg(target_os = "windows")]
fn dx12_dedicated_mib(adapter: &wgpu::Adapter) -> Option<u64> {
    // SAFETY: the hal adapter guard is only read: `GetDesc2` fills a plain struct from the
    // adapter wgpu keeps alive (no object is created or released), and the guard is dropped
    // before this function returns.
    #[allow(unsafe_code)]
    unsafe {
        let hal = adapter.as_hal::<wgpu::hal::api::Dx12>()?;
        let desc = hal.raw_adapter().GetDesc2().ok()?;
        let mib = u64::try_from(desc.DedicatedVideoMemory).ok()? / (1024 * 1024);
        (mib > 0).then_some(mib)
    }
}

#[cfg(not(target_os = "windows"))]
fn dx12_dedicated_mib(_adapter: &wgpu::Adapter) -> Option<u64> {
    None
}

/// The largest device-local heap of a Vulkan adapter, in MiB. `None` on other backends.
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
fn device_local_mib(adapter: &wgpu::Adapter) -> Option<u64> {
    // SAFETY: the hal adapter guard is only read: its physical device and instance are used for
    // one `vkGetPhysicalDeviceMemoryProperties` query (no object is created or destroyed), and the
    // guard is dropped before this function returns, while `adapter` keeps both alive.
    #[allow(unsafe_code)]
    unsafe {
        let hal = adapter.as_hal::<wgpu::hal::api::Vulkan>()?;
        let instance = hal.shared_instance().raw_instance();
        let props = instance.get_physical_device_memory_properties(hal.raw_physical_device());
        let count = (props.memory_heap_count as usize).min(props.memory_heaps.len());
        // VK_MEMORY_HEAP_DEVICE_LOCAL_BIT = 0x1.
        props.memory_heaps[..count]
            .iter()
            .filter(|heap| heap.flags.as_raw() & 0x1 != 0)
            .map(|heap| heap.size / (1024 * 1024))
            .max()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
fn device_local_mib(_adapter: &wgpu::Adapter) -> Option<u64> {
    None
}

/// Where a table row's tier comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Evidence {
    /// Measured on that GPU class with `cargo xtask bench-do27` (the text names the run).
    Measured(String),
    /// Table-by-class, not measured: the tier of the nearest measured row, scaled by public
    /// relative performance (the text cites the Steam survey share where it applies).
    ByClass(String),
    /// Not measured and no measured neighbour (the Deck, phones). The probe corrects a by-class or
    /// guessed tier that is too high.
    Guess(String),
}

impl Evidence {
    /// `"measured"`, `"by_class"` or `"guess"`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Measured(_) => "measured",
            Self::ByClass(_) => "by_class",
            Self::Guess(_) => "guess",
        }
    }

    /// The note.
    #[must_use]
    pub fn note(&self) -> &str {
        match self {
            Self::Measured(n) | Self::ByClass(n) | Self::Guess(n) => n,
        }
    }
}

/// One row of the table: which GPUs it matches and the tier they get.
///
/// A row matches when the vendor matches (if set), the class matches (if set), the device id is
/// in `device_ids` **or** the name contains one of `any` (when either list is set), the name
/// contains every string of `all`, and none of `none`. Names compare case-insensitively.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuRule {
    /// Stable id, shown in the reason.
    pub id: String,
    /// PCI vendor id.
    #[serde(default)]
    pub vendor: Option<u32>,
    /// PCI device ids.
    #[serde(default)]
    pub device_ids: Vec<u32>,
    /// Name substrings, any of which identifies the GPU.
    #[serde(default)]
    pub any: Vec<String>,
    /// Name substrings that must all be present ("Laptop").
    #[serde(default)]
    pub all: Vec<String>,
    /// Name substrings that exclude the row.
    #[serde(default)]
    pub none: Vec<String>,
    /// Device class the row is limited to.
    #[serde(default)]
    pub class: Option<DeviceClass>,
    /// The tier.
    pub tier: QualityTier,
    /// Why this tier.
    pub evidence: Evidence,
}

impl GpuRule {
    /// Whether this row matches `adapter`.
    #[must_use]
    pub fn matches(&self, adapter: &AdapterProfile) -> bool {
        if self.vendor.is_some_and(|v| v != adapter.vendor) {
            return false;
        }
        if self.class.is_some_and(|c| c != adapter.class) {
            return false;
        }
        let name = adapter.name.to_lowercase();
        let has = |s: &String| name.contains(&s.to_lowercase());
        if !(self.device_ids.is_empty() && self.any.is_empty())
            && !self.device_ids.contains(&adapter.device)
            && !self.any.iter().any(has)
        {
            return false;
        }
        self.all.iter().all(has) && !self.none.iter().any(has)
    }
}

/// A VRAM threshold of the fallback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VramStep {
    /// Smallest device-local heap, MiB.
    pub min_mb: u64,
    /// The tier from there up.
    pub tier: QualityTier,
}

/// The least a GPU needs to run above [`LimitFloor::cap`] without engine fallbacks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimitFloor {
    /// `max_texture_dimension_2d` (terrain and VT pages).
    pub max_texture_dimension_2d: u32,
    /// `max_storage_buffers_per_shader_stage` (the vis-buffer resolve needs 10).
    pub max_storage_buffers_per_shader_stage: u32,
    /// `max_bind_groups` (the shared terrain draw needs 5).
    pub max_bind_groups: u32,
    /// BC textures (desktop content is BC; without it every map decodes to RGBA8).
    pub block_compression: bool,
    /// The highest tier a GPU below the floor gets.
    pub cap: QualityTier,
}

impl LimitFloor {
    /// What `adapter` lacks, `None` when it meets the floor.
    #[must_use]
    pub fn missing(&self, adapter: &AdapterProfile) -> Option<String> {
        let mut missing = Vec::new();
        if adapter.max_texture_dimension_2d < self.max_texture_dimension_2d {
            missing.push(format!("2D textures {}", adapter.max_texture_dimension_2d));
        }
        if adapter.max_storage_buffers_per_shader_stage < self.max_storage_buffers_per_shader_stage {
            missing.push(format!(
                "{} storage buffers per stage",
                adapter.max_storage_buffers_per_shader_stage
            ));
        }
        if adapter.max_bind_groups < self.max_bind_groups {
            missing.push(format!("{} bind groups", adapter.max_bind_groups));
        }
        if self.block_compression && !adapter.block_compression && !adapter.android {
            missing.push("no BC textures".to_string());
        }
        (!missing.is_empty()).then(|| missing.join(", "))
    }
}

/// What an unknown GPU gets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FallbackRules {
    /// Software rasterisers.
    pub cpu: QualityTier,
    /// Virtual GPUs.
    pub virtual_gpu: QualityTier,
    /// Integrated GPUs.
    pub integrated: QualityTier,
    /// Any Android GPU no row matched.
    pub android: QualityTier,
    /// Discrete (or unclassified) GPUs by VRAM, highest threshold first.
    pub discrete_by_vram: Vec<VramStep>,
    /// Discrete GPUs whose VRAM is unknown.
    pub unknown_vram: QualityTier,
    /// Limits below which a GPU is capped.
    pub limits: LimitFloor,
}

/// The detector's data: rows tried in order (first match wins), then the fallback; every result
/// is then capped by the limit floor and by VRAM.
///
/// The built-in table is `crates/regia_render/src/quality/gpu_tiers.ron`. A game extends it by
/// inserting rows at the front ([`GpuTierTable::prepend`]) or replaces it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuTierTable {
    /// Format version (1).
    pub version: u32,
    /// Known GPUs.
    pub rules: Vec<GpuRule>,
    /// Smallest VRAM (MiB) each tier needs; a GPU with less steps down until it fits.
    pub vram_floors: Vec<VramStep>,
    /// Unknown GPUs.
    pub fallback: FallbackRules,
}

/// The engine's built-in table.
pub const BUILTIN_GPU_TIERS: &str = include_str!("gpu_tiers.ron");

impl GpuTierTable {
    /// Parses a table.
    pub fn from_ron(text: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(text)
    }

    /// The built-in table. A parse failure (guarded by a test) logs and returns the fallback-only
    /// [`GpuTierTable::minimal`].
    #[must_use]
    pub fn builtin() -> Self {
        Self::from_ron(BUILTIN_GPU_TIERS).unwrap_or_else(|err| {
            tracing::error!(target: "regia_render", "built-in gpu_tiers.ron does not parse: {err}");
            Self::minimal()
        })
    }

    /// No rows: integrated/CPU → Low, discrete by VRAM.
    #[must_use]
    pub fn minimal() -> Self {
        Self {
            version: 1,
            rules: Vec::new(),
            vram_floors: Vec::new(),
            fallback: FallbackRules {
                cpu: QualityTier::Low,
                virtual_gpu: QualityTier::Low,
                integrated: QualityTier::Low,
                android: QualityTier::MobileHigh,
                discrete_by_vram: vec![
                    VramStep {
                        min_mb: 15_000,
                        tier: QualityTier::High,
                    },
                    VramStep {
                        min_mb: 7_600,
                        tier: QualityTier::Medium,
                    },
                    VramStep {
                        min_mb: 0,
                        tier: QualityTier::Low,
                    },
                ],
                unknown_vram: QualityTier::Medium,
                limits: LimitFloor {
                    max_texture_dimension_2d: 16384,
                    max_storage_buffers_per_shader_stage: 10,
                    max_bind_groups: 5,
                    block_compression: true,
                    cap: QualityTier::Low,
                },
            },
        }
    }

    /// Puts `rules` in front of the table's rows (they win over the built-in ones).
    pub fn prepend(&mut self, rules: impl IntoIterator<Item = GpuRule>) {
        let mut front: Vec<GpuRule> = rules.into_iter().collect();
        front.append(&mut self.rules);
        self.rules = front;
    }

    fn vram_floor(&self, tier: QualityTier) -> Option<u64> {
        self.vram_floors.iter().find(|f| f.tier == tier).map(|f| f.min_mb)
    }

    fn fallback_tier(&self, adapter: &AdapterProfile) -> (QualityTier, String) {
        let fb = &self.fallback;
        if adapter.android {
            return (fb.android, "unknown Android GPU".to_string());
        }
        match adapter.class {
            DeviceClass::Cpu => (fb.cpu, "software rasterizer".to_string()),
            DeviceClass::Virtual => (fb.virtual_gpu, "virtual GPU".to_string()),
            DeviceClass::Integrated => (fb.integrated, "integrated GPU".to_string()),
            DeviceClass::Discrete | DeviceClass::Other => match adapter.vram_mb {
                Some(vram) => {
                    let step = fb.discrete_by_vram.iter().find(|s| vram >= s.min_mb);
                    let tier = step.map_or(QualityTier::Low, |s| s.tier);
                    (
                        tier,
                        format!("unknown GPU by VRAM (>= {} MB)", step.map_or(0, |s| s.min_mb)),
                    )
                }
                None => (fb.unknown_vram, "unknown GPU, VRAM unknown".to_string()),
            },
        }
    }
}

impl Default for GpuTierTable {
    fn default() -> Self {
        Self::builtin()
    }
}

/// What [`detect`] decided and why.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    /// The tier.
    pub tier: QualityTier,
    /// One line for menus, logs and the agent: "NVIDIA GeForce RTX 3080 Ti, 12 GB → High".
    pub reason: String,
    /// The matching row's id, or `fallback`.
    pub rule: String,
    /// `measured`, `by_class`, `guess` or `fallback`.
    pub evidence: String,
    /// The row's evidence note (or the fallback's reasoning).
    pub note: String,
    /// Caps applied after the row (limits, VRAM, platform).
    pub caps: Vec<String>,
    /// The adapter it looked at.
    pub adapter: AdapterProfile,
}

/// Picks a tier for `adapter` from `table` (see [`GpuTierTable`]).
#[must_use]
pub fn detect(adapter: &AdapterProfile, table: &GpuTierTable) -> Detection {
    let (mut tier, rule, evidence, note) = match table.rules.iter().find(|r| r.matches(adapter)) {
        Some(row) => (
            row.tier,
            row.id.clone(),
            row.evidence.kind().to_string(),
            row.evidence.note().to_string(),
        ),
        None => {
            let (tier, why) = table.fallback_tier(adapter);
            (tier, "fallback".to_string(), "fallback".to_string(), why)
        }
    };
    let mut caps = Vec::new();
    let cap = table.fallback.limits.cap;
    if tier.is_desktop()
        && tier.rank() > cap.rank()
        && let Some(missing) = table.fallback.limits.missing(adapter)
    {
        caps.push(format!("{tier} → {cap}: {missing}"));
        tier = cap;
    }
    if let Some(vram) = adapter.vram_mb {
        while let Some(floor) = table.vram_floor(tier)
            && vram < floor
        {
            // Very Low draws cheaper, not smaller: the VRAM floors stop at Low (ADR-7882).
            let Some(lower) = tier.step_down().filter(|t| *t != QualityTier::VeryLow) else {
                break;
            };
            caps.push(format!("{tier} → {lower}: {vram} MB VRAM < {floor} MB"));
            tier = lower;
        }
    }
    let platform = tier.for_platform(adapter.android);
    if platform != tier {
        caps.push(format!("{tier} → {platform}: platform"));
        tier = platform;
    }
    let mut what = adapter.name.clone();
    match (adapter.class, adapter.vram_label()) {
        (DeviceClass::Integrated | DeviceClass::Cpu | DeviceClass::Virtual, _) => {
            what.push_str(", ");
            what.push_str(adapter.class.name());
        }
        (_, Some(vram)) => {
            what.push_str(", ");
            what.push_str(&vram);
        }
        (_, None) => {}
    }
    let source = if rule == "fallback" {
        format!("fallback: {note}")
    } else {
        format!("{rule}, {evidence}")
    };
    let mut reason = format!("{what} → {tier} ({source})");
    if !caps.is_empty() {
        reason.push_str("; ");
        reason.push_str(&caps.join("; "));
    }
    Detection {
        tier,
        reason,
        rule,
        evidence,
        note,
        caps,
        adapter: adapter.clone(),
    }
}

/// The confirmation probe's verdict for a detected `tier` (pure; the framework applies it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeVerdict {
    /// The tier to run.
    pub tier: QualityTier,
    /// Whether the probe stepped down.
    pub stepped_down: bool,
    /// The budget the probe was judged against, ms: 1000 / target fps, or for `Low` (whose step
    /// is [`QualityTier::VeryLow`]) 1000 / the lower of the target and
    /// [`QualityTier::VERY_LOW_TRIGGER_FPS`] (ADR-7882).
    pub budget_ms: f32,
    /// One line ("p95 21.9 ms > 16.7 ms budget: High → Medium").
    pub reason: String,
}

/// Below this median GPU busy share ([`ProbeResult::gpu_busy`]) the probe never steps down.
pub const MIN_GPU_BUSY: f32 = 0.5;

/// A step-down also needs the median frame over this share of the budget (with the p95 over
/// the budget), so first-frame transients alone never step down.
pub const P50_SHARE: f32 = 0.8;

/// Steps `tier` down once when the probe's p95 is over `1000 / target_fps` and its median over
/// [`P50_SHARE`] of that (ADR-173: sustained cost, not transients). It never steps up. `Low`'s
/// step goes to [`QualityTier::VeryLow`] only when `Low` misses the lower of the target and
/// [`QualityTier::VERY_LOW_TRIGGER_FPS`] (ADR-7882), so a GPU that holds 30 fps at Low keeps it
/// under the desktop's 60 fps target. A probe
/// without frames, or an aborted one, keeps the tier; so does one that only had the wall clock
/// ([`ProbeSource::Wall`]), which includes waits (vsync, CPU) and cannot tell a slow GPU from a
/// synced one; and so does one whose GPU was busy for under [`MIN_GPU_BUSY`] of the frame
/// interval (present- or CPU-bound, likely downclocked: its times overstate the tier's cost).
#[must_use]
pub fn judge_probe(tier: QualityTier, result: &ProbeResult, target_fps: f32) -> ProbeVerdict {
    let target_fps = match tier.step_down() {
        // Below Low the look drops a whole step: only for a GPU that cannot hold the floor.
        Some(QualityTier::VeryLow) => target_fps.min(QualityTier::VERY_LOW_TRIGGER_FPS),
        _ => target_fps,
    };
    let budget_ms = 1000.0 / target_fps.max(1.0);
    if result.aborted {
        return ProbeVerdict {
            tier,
            stepped_down: false,
            budget_ms,
            reason: "probe aborted (a level load started before it finished); tier kept".to_string(),
        };
    }
    if result.frames == 0 {
        return ProbeVerdict {
            tier,
            stepped_down: false,
            budget_ms,
            reason: "probe measured no frames; tier kept".to_string(),
        };
    }
    if result.source == ProbeSource::Wall {
        return ProbeVerdict {
            tier,
            stepped_down: false,
            budget_ms,
            reason: format!(
                "wall clock only (no GPU timestamps; includes the vsync wait): p95 {:.1} ms, tier kept",
                result.p95_ms
            ),
        };
    }
    // The GPU mostly waited (present- or CPU-bound, so likely downclocked): its times say
    // little about what the tier costs at full clocks (ADR-173).
    if let Some(busy) = result.gpu_busy
        && busy < MIN_GPU_BUSY
    {
        return ProbeVerdict {
            tier,
            stepped_down: false,
            budget_ms,
            reason: format!(
                "GPU busy only {:.0} % of the frame interval (present- or CPU-bound, likely downclocked): \
                 p50 {:.1} ms, p95 {:.1} ms, tier kept",
                busy * 100.0,
                result.p50_ms,
                result.p95_ms
            ),
        };
    }
    // Sustained cost, not transients: the p95 over budget and the median close to it.
    let over = result.p95_ms > budget_ms && result.p50_ms > budget_ms * P50_SHARE;
    match (over, tier.step_down()) {
        (true, Some(lower)) => ProbeVerdict {
            tier: lower,
            stepped_down: true,
            budget_ms,
            reason: if lower == QualityTier::VeryLow {
                format!(
                    "p95 {:.1} ms > {budget_ms:.1} ms budget: {tier} misses {:.0} fps, {tier} → {lower} \
                     (Desktop Low at {:.0} % render scale, SGSR upscale)",
                    result.p95_ms,
                    1000.0 / budget_ms,
                    QualityTier::VERY_LOW_RENDER_SCALE * 100.0
                )
            } else {
                format!(
                    "p95 {:.1} ms > {budget_ms:.1} ms budget: {tier} → {lower}",
                    result.p95_ms
                )
            },
        },
        (true, None) => ProbeVerdict {
            tier,
            stepped_down: false,
            budget_ms,
            reason: format!(
                "p95 {:.1} ms > {budget_ms:.1} ms budget, but {tier} is the lowest tier",
                result.p95_ms
            ),
        },
        (false, _) => ProbeVerdict {
            tier,
            stepped_down: false,
            budget_ms,
            reason: format!("p95 {:.1} ms within {budget_ms:.1} ms budget", result.p95_ms),
        },
    }
}

#[cfg(test)]
mod tests;
