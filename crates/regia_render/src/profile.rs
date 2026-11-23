// SPDX-License-Identifier: MPL-2.0
//! Render profiles and scalability presets (`specs/13` § 3).

use crate::cinematic::CloudMarch;
use crate::texture_stream::TextureStreaming;

/// Anti-aliasing mode for a render profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AntiAliasing {
    /// No AA.
    #[default]
    None,
    /// 4× MSAA (mobile high).
    Msaa4x,
    /// Fast approximate AA (post).
    Fxaa,
    /// Temporal AA (desktop high+).
    Taa,
    /// SMAA 1× (Jimenez et al. 2012): morphological post AA with luma edges, the
    /// non-temporal tiers' AA (Mobile High, the Deck's non-temporal fallback).
    Smaa,
}

/// How a scaled frame without temporal AA reaches the output size (`specs/13` § 3).
///
/// Only consulted when the internal size differs from the output and the frame does
/// not take the temporal upscaler. The default keeps every existing frame unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpatialUpscaler {
    /// Bilinear upsample of the scene-referred HDR image; post runs at the output size.
    #[default]
    Bilinear,
    /// Snapdragon Game Super Resolution 1 (Qualcomm, BSD-3-Clause): post runs at the
    /// internal size and a single edge-adaptive 12-tap Lanczos pass maps the display
    /// image onto the output (`shaders/sgsr.wgsl`). On an HDR target it applies the
    /// same paper-white highlight expansion as the post blit.
    Sgsr,
}

/// Which temporal upscaler resolves a [`AntiAliasing::Taa`] frame (`specs/27` § 2.3).
///
/// Only consulted when the profile's anti-aliasing is TAA. The default keeps every
/// existing frame unchanged (ADR-041: perf or look changes are opt-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TemporalUpscaler {
    /// The original TAAU (`taau.rs`): neighbourhood clamp, bilinear history.
    #[default]
    Taau,
    /// The TSR-grade upscaler (`tsr/`): jitter-aware reconstruction, depth-dilated
    /// motion, disocclusion, variance clipping, thin-feature locks, reactive mask.
    Tsr,
    /// The FSR 3.1 upscaler ported from the AMD FidelityFX SDK v1.1.4 (`fsr3/`), TSR's
    /// comparison baseline: Lanczos upsampling, variance-box rectification, shading-change
    /// and luma-instability detection, locks, RCAS.
    Fsr3,
}

/// HDR colour target format family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrTarget {
    /// `RG11B10` (mobile).
    Rg11B10,
    /// `RGBA16F` (desktop).
    Rgba16f,
}

/// Shadow filtering quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShadowFilter {
    /// Hardware 2×2 PCF.
    Hardware2x2,
    /// 3×3 PCF kernel.
    #[default]
    Pcf3x3,
    /// Standard PCF.
    Pcf,
    /// PCSS-lite (desktop high and ultra).
    PcssLite,
}

/// Diffuse global-illumination tier (`specs/26` § 4–5).
///
/// Mobile and Low presets stay on [`Ambient`](Self::Ambient): the existing
/// ambient / IBL term, with no probe trace and no screen-space bounce.
/// [`ScreenSpace`](Self::ScreenSpace) is the cheaper tier (selectable; not the
/// Low default). [`Probes`](Self::Probes) is the dynamic irradiance grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GiMode {
    /// Baked-or-ambient stand-in. No dynamic GI.
    #[default]
    Ambient,
    /// Screen-space colour bleeding only.
    ScreenSpace,
    /// Dynamic irradiance probes, plus screen-space GI when that flag is set.
    Probes,
}

/// The player-facing lighting option (ADR-133 gi-look): which diffuse indirect light the frame
/// uses, chosen separately from the quality preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GiLook {
    /// The analytic terms: sky irradiance with canyon occlusion, the sunlit-wall bounce and the
    /// mesh ground bounce (ADR-062/065/066). Every platform.
    Balanced,
    /// GPU-traced irradiance probes (`crate::ddgi`): bounce light the probes see, no light
    /// through walls. Desktop only ([`GiLook::depth_available`]).
    Depth,
    /// The map's baked GI volume (`.fgiv`, [`crate::FrameRenderer::set_baked_gi`]): path-traced
    /// bounce, sky and fill light, no probe update. Without an installed volume it shades as
    /// [`GiLook::Balanced`]. A map with a volume uses it under [`GiLook::Depth`] and the preset
    /// default (Low presets included) as well, with DDGI and SSGI off; only a chosen
    /// [`GiLook::Balanced`] ignores it (`RenderSettings::quality`).
    Baked,
}

impl GiLook {
    /// Whether [`GiLook::Depth`] can run on this build's platform (not on Android).
    #[must_use]
    pub const fn depth_available() -> bool {
        !cfg!(target_os = "android")
    }

    /// Whether this look runs the probes here: `Depth` where it is available.
    #[must_use]
    pub const fn runs_probes(self) -> bool {
        matches!(self, Self::Depth) && Self::depth_available()
    }

    /// The look a profile ships with: `Depth` when its `gi_probes` column is set.
    #[must_use]
    pub const fn of_profile(profile: &RenderProfile) -> Self {
        if profile.gi_probes && Self::depth_available() {
            Self::Depth
        } else {
            Self::Balanced
        }
    }

    /// Stable lower-case name (`"balanced"`, `"depth"`, `"baked"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Depth => "depth",
            Self::Baked => "baked",
        }
    }

    /// Whether this look lights a map with its baked GI volume when one is installed (all but
    /// [`GiLook::Balanced`]).
    #[must_use]
    pub const fn uses_bake(self) -> bool {
        !matches!(self, Self::Balanced)
    }
}

/// Screen-space ambient occlusion mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SsaoMode {
    /// Disabled.
    #[default]
    Off,
    /// Half-resolution SSAO.
    HalfRes,
    /// Half-resolution GTAO.
    GtaoHalfRes,
    /// Full-resolution GTAO.
    GtaoFull,
}

/// Named render profile presets from `specs/13` § 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderProfilePreset {
    /// Mobile low quality preset.
    MobileLow,
    /// Mobile high quality preset.
    MobileHigh,
    /// Desktop low quality preset.
    DesktopLow,
    /// Desktop high quality preset. Native render scale with temporal AA; the 67% temporal
    /// upscale (`ADR-041`) is an opt-in budget (`render_scale = 0.67` with a 2048² cascade).
    DesktopHigh,
    /// Desktop ultra quality preset.
    DesktopUltra,
    /// Steam Deck: 1280×800, 40 fps cap, temporal upscale from 70% (`specs/13` § 3, `specs/26` § 4).
    Deck,
    /// Offline output above Desktop Ultra (ADR-1403, [`crate::cinematic`]): every cloud texel
    /// marched every frame with stratified samples, 8192² sun cascades, the light caps at their
    /// maximum, mesh LOD held toward LOD0. Not in [`Self::ALL`] (no game menu or detector picks
    /// it); [`crate::cinematic::gate_preset`] gives Android and tiers below High their own preset.
    Cinematic,
}

impl RenderProfilePreset {
    /// Steam Deck output width (`specs/13` § 3: 1280×800).
    pub const DECK_OUTPUT_WIDTH: u32 = 1280;
    /// Steam Deck output height.
    pub const DECK_OUTPUT_HEIGHT: u32 = 800;
    /// Deck frame cap (inside the 30–40 fps band; 30 and 60 remain user caps).
    pub const DECK_FRAME_CAP_FPS: u32 = 40;

    /// Every real-time preset, in table order: the ones a game menu, the quality detector and
    /// the per-preset tables list. [`Self::WITH_OFFLINE`] adds [`Self::Cinematic`].
    pub const ALL: [Self; 6] = [
        Self::MobileLow,
        Self::MobileHigh,
        Self::DesktopLow,
        Self::DesktopHigh,
        Self::DesktopUltra,
        Self::Deck,
    ];

    /// [`Self::ALL`] followed by the offline [`Self::Cinematic`]: every name
    /// [`Self::from_name`] and `render.set_profile` accept.
    pub const WITH_OFFLINE: [Self; 7] = [
        Self::MobileLow,
        Self::MobileHigh,
        Self::DesktopLow,
        Self::DesktopHigh,
        Self::DesktopUltra,
        Self::Deck,
        Self::Cinematic,
    ];

    /// Whether this is an offline preset ([`Self::Cinematic`]), outside [`Self::ALL`].
    #[must_use]
    pub const fn is_offline(self) -> bool {
        matches!(self, Self::Cinematic)
    }

    /// The preset a platform runs when the game picks none: Mobile High on Android (a phone
    /// cannot carry Desktop High's native-resolution TAA frame), Desktop High elsewhere.
    #[must_use]
    pub const fn platform_default() -> Self {
        if cfg!(target_os = "android") {
            Self::MobileHigh
        } else {
            Self::DesktopHigh
        }
    }

    /// The preset a running game gets when it picks none (`RenderSettings::default()`):
    /// [`Self::Deck`] on a Steam Deck, else [`Self::platform_default`] (ADR-3289). A Deck is
    /// `SteamDeck=1` in the environment (Steam sets it in Game Mode and for games launched from
    /// Steam in Desktop Mode) or, on Linux, a Valve `Jupiter` (LCD) or `Galileo` (OLED) DMI
    /// product name ([`is_steam_deck_model`]). The answer is read once per process.
    ///
    /// Runtime only: cooks and permutation lists keep [`Self::platform_default`], so a cook
    /// never depends on the machine that runs it.
    #[must_use]
    pub fn runtime_default() -> Self {
        static ON_DECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        Self::runtime_default_for(*ON_DECK.get_or_init(detect_steam_deck))
    }

    /// [`Self::runtime_default`] with the Deck answer given (testable): [`Self::Deck`] when
    /// `on_deck` and not on Android, else [`Self::platform_default`].
    #[must_use]
    pub const fn runtime_default_for(on_deck: bool) -> Self {
        if on_deck && !cfg!(target_os = "android") {
            Self::Deck
        } else {
            Self::platform_default()
        }
    }

    /// The name used in `Regia.toml` (`render_profile`) and the agent protocol, e.g. `MobileHigh`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::MobileLow => "MobileLow",
            Self::MobileHigh => "MobileHigh",
            Self::DesktopLow => "DesktopLow",
            Self::DesktopHigh => "DesktopHigh",
            Self::DesktopUltra => "DesktopUltra",
            Self::Deck => "Deck",
            Self::Cinematic => "Cinematic",
        }
    }

    /// Parses a preset name: `MobileHigh`, `mobile_high` and `mobile-high` all match
    /// (case-insensitive, `_`/`-` ignored).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let key: String = name
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        Self::WITH_OFFLINE
            .into_iter()
            .find(|p| p.name().to_lowercase() == key)
    }
}

/// Whether `device_model` (`regia_platform::OsInfo::device_model`: the DMI vendor and product
/// name on Linux) names a Steam Deck: `Valve Jupiter` (LCD) or `Valve Galileo` (OLED).
#[must_use]
pub fn is_steam_deck_model(device_model: &str) -> bool {
    let mut words = device_model.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next()),
        (Some("Valve"), Some("Jupiter" | "Galileo"), None)
    )
}

/// `SteamDeck=1` in the environment, or (Linux) a Deck's DMI product name.
fn detect_steam_deck() -> bool {
    if regia_core::brand::env_var("SteamDeck").is_ok_and(|v| v == "1") {
        return true;
    }
    cfg!(target_os = "linux") && is_steam_deck_model(&regia_platform::OsInfo::detect().device_model)
}

/// Project graphics settings for one platform (`specs/13` § 3).
///
/// # Example
///
/// ```
/// use regia_render::{RenderProfile, RenderProfilePreset};
///
/// let profile = RenderProfile::from_preset(RenderProfilePreset::Deck);
/// assert_eq!(profile.frame_cap_fps, Some(40));
/// let (w, h) = profile.scaled_extent(1280, 800); // 70% render scale
/// assert_eq!((w, h), (896, 560));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct RenderProfile {
    /// HDR render target format.
    pub hdr_target: HdrTarget,
    /// Internal resolution scale relative to the output. `1.0` is native; values below render
    /// fewer pixels (upscaled), values above `1.0` request supersampling. [`FrameQuality::from_profile`]
    /// clamps this into `[0.05, 1.0]`, so a `render_scale > 1.0` is only honoured by paths that use
    /// the profile directly — the two disagree at the `> 1.0` edge.
    pub render_scale: f32,
    /// Anti-aliasing.
    pub anti_aliasing: AntiAliasing,
    /// Whether a depth prepass runs before the forward pass.
    pub depth_prepass: bool,
    /// Number of directional-light CSM cascades.
    pub csm_cascade_count: u32,
    /// Shadow map resolution per cascade edge (pixels).
    pub csm_resolution: u32,
    /// Local light shadow atlas edge size (0 = off).
    pub local_shadow_atlas_size: u32,
    /// Maximum local lights with shadows in the atlas.
    pub local_shadow_max_lights: u32,
    /// Shadow filter kernel.
    pub shadow_filter: ShadowFilter,
    /// SSAO / GTAO mode.
    pub ssao: SsaoMode,
    /// Screen-space contact shadows multiplied into the sun's visibility
    /// (`crate::contact_shadows`). Needs a depth prepass and a single-sample frame; off on
    /// the mobile presets.
    pub contact_shadows: bool,
    /// Reflection occlusion: the SSAO pass also estimates which share of each surface's
    /// specular lobe leaves the visible enclosure (`crate::ssao`), and the forward pass takes
    /// that part of the sky reflection out (a cockpit or a room behind a window reflects
    /// itself, not the sky). Needs the GTAO rows of [`Self::ssao`]: Desktop High and Ultra.
    /// Elsewhere the specular occlusion is the AO-based one alone.
    pub reflection_occlusion: bool,
    /// Bent normals (ADR-1375): the SSAO pass also writes each texel's bent normal (the mean
    /// unoccluded direction), the forward pass reads the sky, probe and baked GI irradiance along
    /// it instead of the normal, and its specular occlusion becomes the share of the specular cone
    /// inside the visibility cone around it. Creases take less of the light that arrives from the
    /// occluder's side (on baked maps, where SSGI is off, the probe light interpolated from beyond
    /// it). Needs the GTAO rows of [`Self::ssao`]: Desktop High and Ultra; off on Desktop Low, Deck,
    /// mobile and the Medium tier (Desktop High at 0.67 scale).
    pub bent_normals: bool,
    /// Bloom enabled.
    pub bloom: bool,
    /// Number of bloom mips when bloom is on.
    pub bloom_mip_count: u32,
    /// Auto exposure (histogram); when false, fixed EV is used.
    pub auto_exposure: bool,
    /// Maximum reflection probes chosen per frame (one camera-distance list, hard-capped at 4 uploaded).
    pub max_reflection_probes: u32,
    /// Maximum point/spot lights assigned per froxel cluster.
    pub max_lights_per_cluster: u32,
    /// Point and spot lights uploaded for one view. Desktop High is 256.
    /// A larger authored set is ranked by screen importance and the drop is logged.
    pub max_visible_lights: u32,
    /// Optional frame cap. `None` does not change pacing (uncapped, or the user's cap).
    /// The Deck preset is 40 fps.
    pub frame_cap_fps: Option<u32>,
    /// GPU frustum + Hi-Z culling and multi-draw-indirect (Desktop High+).
    pub gpu_driven: bool,
    /// Horizontal-plus FOV for aspects other than 16:9 (desktop and Deck).
    pub horizontal_plus: bool,
    /// Physically based atmosphere lookup tables. Desktop High and Ultra.
    ///
    /// Other presets keep the analytic sky and the existing height fog.
    pub physical_atmosphere: bool,
    /// Froxel volumetric fog (sun, local lights, shadow shafts). Desktop High and Ultra.
    pub volumetric_fog: bool,
    /// Raymarched volumetric clouds and their terrain shadow. Desktop High and Ultra.
    pub volumetric_clouds: bool,
    /// Clouds march one low-resolution texel in 16 per frame (4×4 Bayer order) and
    /// reconstruct the rest from the reprojected previous frame; a camera cut marches
    /// all of them. On for every preset (clouds themselves are Desktop High and Ultra).
    pub cloud_reprojection: bool,
    /// Percent of the cloud layer's view-march steps (`CloudSettings::steps`). Real-time
    /// presets cap the march at 96 steps ([`CloudMarch::REALTIME`]); Desktop Ultra is 150
    /// (64 → 96), paid for by the reprojection, and every other real-time preset is 100.
    /// Cinematic is 300, capped by [`CloudMarch::STEP_LIMIT`] (256), so 64 steps become 192.
    pub cloud_step_percent: u32,
    /// Cloud lighting from a cone of samples toward the sun (with one long tap for distant
    /// cloud), a dark-edge in-scatter probability on the multiple-scattering octaves, and a
    /// curl-noise distortion of the detail erosion. Off keeps the six-step straight light
    /// march and undistorted detail of v0.1. Desktop High and Ultra (the presets with
    /// clouds); off elsewhere.
    pub cloud_cone_light: bool,
    /// How the cloud view march runs: marches per texel, step ceiling, largest resolution
    /// divisor (ADR-1403). [`CloudMarch::REALTIME`] on every real-time preset (frames unchanged),
    /// [`CloudMarch::CINEMATIC`] on [`RenderProfilePreset::Cinematic`].
    pub cloud_march: CloudMarch,
    /// Cached paged sun shadow maps. Off redraws each cascade every frame.
    pub paged_sun_shadows: bool,
    /// Sun cascades that also carry a translucency map (colored shadow map at half the
    /// cascade edge), so alpha-blended and transmissive casters (glass) shadow by their
    /// transmittance. `0` = blended casters cast no sun shadow. Mobile Low 0, Mobile High 1,
    /// Desktop Low and Deck 2, Desktop High and Ultra 4 (clamped to the cascade count).
    pub translucent_shadow_cascades: u32,
    /// Local-light shadows also see translucent casters (a translucency atlas at a quarter of
    /// the local atlas edge). Desktop and Deck presets; off on both Mobile tiers.
    pub translucent_local_shadows: bool,
    /// Edge (texels) of the near sun tile: an orthographic sun view over the first
    /// [`crate::NEAR_SUN_RANGE_M`] of the view, drawn into the local shadow atlas, that mesh
    /// receivers that close sample instead of cascade 0 (ADR-164). `0` = off. Desktop High
    /// 2048, Ultra 4096; off elsewhere.
    pub near_sun_tile: u32,
    /// Diffuse GI tier. Low presets are [`GiMode::Ambient`].
    pub gi: GiMode,
    /// Screen-space colour bleeding (the cheaper GI tier).
    pub screen_space_gi: bool,
    /// GPU-traced irradiance probes (`crate::ddgi`, ADR-132 gi-probes): the default lighting
    /// option of the preset (`GiLook::of_profile`). On for Desktop High and Ultra (the Depth
    /// look), off below and on the Deck and mobile; players and games override it with
    /// `RenderSettings::gi_look`, the renderer with `FrameRenderer::set_ddgi_enabled`. Needs a
    /// linear-HDR frame with the single-sample depth prepass; never runs on Android.
    pub gi_probes: bool,
    /// How large and how fast the probe field is when the probes run
    /// ([`crate::DynamicGiTier`], `specs/79`, ADR-1679): Low on the Deck and Desktop Low
    /// (and mobile, which never runs them), High on Desktop High, Ultra on Ultra.
    /// `RenderSettings::dynamic_gi_tier` overrides it at runtime.
    pub gi_tier: crate::DynamicGiTier,
    /// The world-space radiance cache behind screen-space GI (`specs/79` § 12, ADR-2085). Off in
    /// every preset. `RenderSettings::radiance_cache` overrides it at runtime; it does anything
    /// only on a frame that runs both the probes and screen-space GI.
    pub radiance_cache: bool,
    /// Screen-space reflections on the forward colour.
    pub ssr: bool,
    /// Captured reflection probes, capped by [`Self::max_reflection_probes`].
    pub reflection_probes: bool,
    /// Parallax occlusion mapping. Desktop High and Ultra only; the material scale must also be set.
    pub parallax_occlusion: bool,
    /// Cheaper terrain shading for mid-range GPUs. Off in every preset (it changes pixels
    /// beyond the distances it names; ADR-041 rule); a game opts in.
    pub terrain_budget: TerrainBudget,
    /// Screen-space refraction behind transmissive materials with a volume thickness: the
    /// forward pass copies the opaque frame and bends what each glass surface shows by its
    /// normal × thickness × (IOR − 1). On in Desktop High and Ultra (owner decision for DO27's
    /// exterior glass, 2026-10-08; about +0.1–0.3 ms on frames with glass, nothing without);
    /// off on Low, Mobile and Deck. Needs the HDR target without MSAA.
    pub screen_space_refraction: bool,
    /// Static geometry's TAAU motion comes from the depth buffer (one fullscreen pass) and
    /// only moving meshes draw into the velocity target. Every shipping preset (look-r2
    /// rounds 9–10): per-pixel camera motion is exact where per-vertex motion interpolates
    /// linearly across large triangles, and per-draw work does not shrink with resolution.
    pub depth_velocity: bool,
    /// Temporal upscaler used when [`Self::anti_aliasing`] is TAA. Every preset is
    /// [`TemporalUpscaler::Taau`]; a game opts into [`TemporalUpscaler::Tsr`].
    pub temporal_upscaler: TemporalUpscaler,
    /// Visibility buffer + cluster LOD for rigid opaque meshes that have meshlets
    /// ([`crate::visbuffer`]). Off in every preset (ADR-041: frames change by the LOD cut); a
    /// game or tool opts in for dense scatter.
    pub visibility_buffer: bool,
    /// Which frames take the path and how its resolve shades (ADR-2071). The default in every
    /// preset: every frame the switch allows, one draw per material. [`Self::dense_scenes`] is
    /// the selectable profile for dense scenes.
    pub visibility_path: crate::visbuffer::VisibilityPath,
    /// What the sun's shadows trace (ADR-2035, `book/agents/hwrt.md`). Off in every preset: a
    /// game or tool enables it per profile. [`crate::hwrt::RtShadowRequest::On`] also asks the
    /// device for hardware ray queries, so it must be set before the first window's device;
    /// where the device has none the frame takes the screen-space trace.
    pub sun_shadow_trace: crate::hwrt::RtShadowRequest,
    /// CPU pipelining of the frame (ADR-2074, `book/agents/frame-pipelining.md`). Off in every
    /// preset; the frames are the same either way.
    pub frame_pipelining: crate::FramePipelining,
    /// Screen-space subsurface scattering for materials that ask for it ([`crate::subsurface`],
    /// ADR-2049). Off in every preset: the materials draw without the blur, and a frame costs
    /// nothing for it. On, a frame adds three pooled targets and two blur passes only while a
    /// scattering material is visible.
    pub subsurface_scattering: bool,
    /// Spatiotemporal blue noise (N5, ADR-2075, [`crate::blue_noise`]) for the frame's stochastic
    /// passes: stochastic SSR's ray directions and the spiral rotation of SSGI and SSAO, in place
    /// of interleaved gradient noise. Off in every preset. Off, a frame is unchanged and creates
    /// nothing.
    pub blue_noise: bool,
    /// Weighted blended order-independent transparency for translucent meshes ([`crate::oit`],
    /// ADR-2065). Off in every preset: translucent meshes sort back to front, and a frame costs
    /// nothing for it. On, a frame adds three pooled targets, one accumulation pass and one
    /// composite only while such a mesh is visible, and writes the reactive mask its upscaler reads.
    pub order_independent_transparency: bool,
    /// Two-phase occlusion culling of the forward depth prepass (`specs/98`, ADR-1887): the
    /// GPU-driven prepass skips instances last frame's depth pyramid hides and draws the ones a
    /// pyramid of its own first phase does not, so the depth (and the frame) is the unculled
    /// prepass's. On in Desktop High and Ultra; off on Low, the Deck and both Mobile tiers until
    /// measured there. Runs only on single-sample frames with at least 2048 depth instances.
    /// `RenderSettings::two_phase_occlusion` overrides it at runtime.
    pub two_phase_occlusion: bool,
    /// CPU masked occlusion culling of the forward draw list (ADR-10072,
    /// [`crate::masked_occlusion`]): large opaque rigid meshes in view are rasterized into a
    /// small software depth buffer, and opaque rigid draws whose bounds are hidden behind them
    /// are dropped from the colour and depth passes (shadow casters keep them). Conservative: a
    /// frame is the same with it on or off; only draws the image never shows are skipped. On in
    /// Desktop Low (and the quality ladder's Very Low, drawn from it) and on the Deck, the tiers
    /// without the GPU two-phase prepass cull; off elsewhere. Main view only (no texture views,
    /// no stereo eyes). [`crate::FrameRenderer::set_masked_occlusion`] is the renderer's switch.
    pub masked_occlusion: bool,
    /// Material texture streaming ([`crate::texture_stream`]) and the resident texture budget.
    /// On in every preset ([`TextureStreaming::for_preset`], ADR-130, lane tex-budget): eager
    /// first use, so frames match the unstreamed ones while the tier's budget has room; over it,
    /// least-recently-seen textures drop their top mips instead of the process running out of
    /// memory. [`TextureStreaming::OFF`] restores the unbudgeted upload.
    pub texture_streaming: TextureStreaming,
    /// Upscaler of a scaled frame that does not run temporal AA (both Mobile tiers, and
    /// the Deck when [`Self::non_temporal`] drops its TAA).
    pub spatial_upscaler: SpatialUpscaler,
    /// Allows motion blur ([`crate::MotionBlur`]) on views whose camera asks for it with a
    /// [`crate::PhysicalCamera`]. Desktop High, Ultra and Deck; it needs the TAA velocity target.
    /// A camera without the component renders as before (ADR-041).
    pub motion_blur: bool,
    /// Allows depth of field ([`crate::DepthOfField`]) on views whose camera asks for it with a
    /// [`crate::PhysicalCamera`]. Desktop High, Ultra and Deck. A camera without the component
    /// renders as before (ADR-041).
    pub depth_of_field: bool,
    /// Allows the lens effects pass (ghost flare, lens dirt, chromatic aberration;
    /// [`crate::LensEffects`], ADR-1863) on views that ask for it. Desktop High and Ultra (and
    /// the cinematic profile); a view whose strengths are all 0 records nothing either way.
    pub lens_effects: bool,
    /// Area lights fitted to emissive meshes (`emissive_lights`) uploaded per view, the
    /// brightest on screen first; 0 = off (emission lights its surroundings only through the
    /// GI). Desktop High 96, Ultra 160; Desktop Low, Deck and both Mobile tiers off until
    /// measured there (lane emissive-local; the Mobile plan is the N brightest, diffuse only).
    pub emissive_lights: u32,
    /// Multiplier on projected mesh size for automatic LOD (above 1 keeps detail longer).
    ///
    /// Mobile Low 0.5, Mobile High 0.7, Desktop Low 0.85, Desktop High 1, Desktop Ultra 1.15,
    /// Deck 0.8. Clamped to 0.25..=4 when used. Not copied into [`FrameQuality`] (that type is
    /// `Eq`). Changing it makes [`crate::RenderSettings::preset`] report a custom profile.
    pub mesh_lod_bias: f32,
    /// Tier multiplier on the projected mesh size for the colour LOD, on top of
    /// [`Self::mesh_lod_bias`] (ADR-260, lane v0119d). Switch distances scale linearly with it:
    /// 6 holds each colour LOD to six times the distance of 1.
    ///
    /// Desktop High 2, Desktop Ultra 6, every other preset 1 (1 selects exactly as before). The
    /// product `mesh_lod_bias × scale` is clamped to
    /// [`crate::MESH_LOD_DISTANCE_SCALE_RANGE`] (0.25..=16) when used; a non-finite value
    /// counts as 1.
    pub mesh_lod_distance_scale: f32,
    /// Multiplier from the colour size to the size the shadow LOD is chosen at (below 1 drops
    /// shadow detail earlier than colour). It applies after [`Self::mesh_lod_distance_scale`],
    /// so shadow switch distances scale with `distance_scale × shadow_scale`.
    ///
    /// 0.5 on every preset but Desktop High (0.35) and Desktop Ultra (0.15: its shadows select
    /// about as a bias-1 colour does, measured on the unit-budget bench). Clamped to
    /// [`crate::MESH_LOD_SHADOW_SCALE_RANGE`] (0.05..=1) when used; a non-finite value counts
    /// as 0.5.
    pub mesh_lod_shadow_scale: f32,
    /// Upscale quality mode (ADR-324): `None` (every preset) renders at [`Self::render_scale`]
    /// as before; `Some(mode)` replaces that scale with the mode's
    /// ([`crate::UpscaleQuality::permille`]) and renders at exactly
    /// [`crate::UpscaleQuality::render_size`]. The mode sets the size only: the upscaler is still
    /// [`Self::temporal_upscaler`] on a TAA frame and [`Self::spatial_upscaler`] otherwise.
    /// Dynamic resolution moves inside the mode's range
    /// ([`crate::DynResConfig::within_quality`]).
    pub upscale_quality: Option<crate::UpscaleQuality>,
    /// The lite forward shader (ADR-1874, [`crate::forward_lite`]): both Mobile tiers ask for it,
    /// and it applies on tile GPUs only ([`crate::forward_lite::adapter_takes_lite`]), so a Mobile
    /// frame on a desktop adapter keeps its pixels. Off in every other preset.
    pub forward_lite: crate::forward_lite::ForwardLite,
    /// The lightmap lookup this tier allows (ADR-2107): it caps [`crate::lightmap::LightmapSettings`]
    /// on lightmap receivers and touches nothing else. Bilinear on Mobile Low, bilinear with
    /// directions on Mobile High, whatever the settings ask elsewhere.
    pub lightmap_tier: crate::lightmap::LightmapTier,
    /// Where the subsurface blur lays its kernel ([`crate::subsurface::SubsurfaceKernel`],
    /// ADR-3170): [`crate::subsurface::SubsurfaceKernel::Screen`] in every preset, so frames with
    /// [`Self::subsurface_scattering`] on do not move unless a game asks for the tangent-plane
    /// kernel. Does nothing while scattering is off.
    pub subsurface_kernel: crate::subsurface::SubsurfaceKernel,
    /// Two-phase occlusion culling of the shadow casters (ADR-2072, `specs/98` § 10a): a redrawn
    /// sun cascade or atlas with at least [`crate::SHADOW_OCCLUSION_MIN_CASTERS`] GPU-driven
    /// caster instances skips the ones its last redraw's depth pyramid hides; the maps are the
    /// unculled maps. Off in every preset until measured on a card (ADR-2482, card-costs flag 3).
    /// `RenderSettings::shadow_occlusion` overrides it at runtime, and `RenderSettings::preset`
    /// ignores it, so a shipped preset with it on still reads as that preset.
    pub shadow_occlusion: bool,
    /// Stochastic many-light sampling (M15d, ADR-2470 and ADR-2652, [`crate::restir_di`]): the
    /// opaque forward pass shades one resampled point or spot light per pixel (spatiotemporal
    /// reservoirs over the pixel's cluster list) instead of looping over every one. Fitted
    /// emitters, the sun, GI and the blended, OIT and glass passes keep their loops. Off in every
    /// preset until the owner approves Desktop Ultra plates with it on; a host turns it on at
    /// runtime (`render.set_many_light_sampling {"mode":"production"}`), which
    /// [`crate::RenderSettings::preset`] ignores.
    pub many_light_sampling: bool,
    /// The ground's aerial perspective and the weather's extinction from the atmosphere tables
    /// (ADR-3294, [`crate::TableAir`]): on a frame that draws the LUT sky, the outdoor composite
    /// adds the precipitation's extinction to the sky and the ground as one term, with the
    /// horizon sky from the same tables as its in-scatter, and carries the aerial perspective
    /// past the tables' last slice to the sky's last row. The forward pass, the lite pass and the
    /// terrain then leave their analytic weather in-scatter (`haze_inscatter`) off. Frames without
    /// the LUT sky are unchanged. Off in every preset until the owner has looked at the plates; a
    /// host turns it on at runtime, which [`crate::RenderSettings::preset`] ignores.
    pub aerial_from_tables: bool,
    /// Horizon-based ambient occlusion (ADR-3202, `book/agents/contact-calibration.md`): the GTAO
    /// rows of [`Self::ssao`] run the horizon-slice kernel (Jimenez et al. 2016) with a falloff
    /// on each depth sample's column, instead of the point obscurance kernel. A box's foot then
    /// darkens as the path tracer's does (spec 27 § 2.2 `ao_contact`). Off in every preset until
    /// the owner approves the moved frames; a host turns it on at runtime
    /// (`render.set_contact_calibration`), which [`crate::RenderSettings::preset`] ignores.
    pub horizon_ao: bool,
    /// Sun penumbra by the sun's angular size below the PCSS filter's texel footprint (ADR-3202):
    /// with [`Self::shadow_filter`] = PCSS-lite, a near-contact sun shadow edge narrower than the
    /// filter's one-texel floor and bilinear taps is steepened about its midpoint to the width
    /// the sun's disc gives (spec 27 § 2.2 `sun_penumbra_1m`), down to a 0.4-texel ramp. Off in every
    /// preset; set at runtime (`render.set_contact_calibration`), ignored by
    /// [`crate::RenderSettings::preset`].
    pub angular_sun_penumbra: bool,
    /// Per-pixel indirect diffuse path resampling through hardware ray queries
    /// ([`crate::restir_gi`], spec 27 stage 2b, ADR-3349). Off in every preset. The probe field
    /// stays active as the hit-radiance source and as the fallback when the device lacks ray
    /// queries or hit-vertex return. [`crate::RenderSettings::preset`] ignores this opt-in, so
    /// enabling it at run time does not move unrelated quality tiers.
    pub restir_gi: bool,

    /// Cluster residency of the visibility buffer (ADR-3118, [`crate::visbuffer::ClusterResidency`]):
    /// a byte budget for the cluster data of meshes on the path, finer DAG bands loading as the
    /// view asks for them. Off in every preset ([`crate::visbuffer::ClusterResidency::for_preset`]
    /// gives each tier's numbers); a host turns it on at runtime, which
    /// [`crate::RenderSettings::preset`] ignores.
    pub cluster_residency: crate::visbuffer::ClusterResidency,
    /// The direct BRDF's energy model (ADR-2632, [`crate::BrdfEnergy`]). [`crate::BrdfEnergy::Legacy`]
    /// in every preset (the shipped look); `Compensated` keeps a white surface's reflected energy at
    /// or under what it receives at grazing angles, and moves every lit pixel. Opting in does not
    /// make [`crate::RenderSettings::preset`] report a custom profile. Read once at startup by
    /// [`crate::RenderPlugin`], which sets [`crate::set_process_brdf_energy`]; a later change takes
    /// that call or `render.set_brdf_energy`.
    pub brdf_energy: crate::BrdfEnergy,
    /// Forward decals (ADR-3255, [`crate::forward_decals`]): projected decals that write albedo,
    /// normal, roughness or metallic change the surface in the forward pass before it is lit, so
    /// the decal's normal and roughness are lit; the rest keep the projected post pass. Off in every
    /// preset until the owner approves plates with it on. Opting in does not make
    /// [`crate::RenderSettings::preset`] report a custom profile. Read once at startup by
    /// [`crate::RenderPlugin`], which sets [`crate::set_process_forward_decals`] before the first
    /// frame composes a forward module (the switch is latched per process).
    pub forward_decals: bool,
    /// The sun's specular as the disc it is (ADR-3190): when set, the runtime lights each
    /// isotropic GGX lobe from the sun's 0.27° disc (its mean over the disc,
    /// [`crate::DirectionalLightUniform::with_specular_disc`]) instead of a point, as the path
    /// tracer does. Smooth highlights (roughness ≤ 0.3) get wider and dimmer at the core; rough
    /// lobes do not move. Off in every preset (the shipped look and every golden); a host turns it
    /// on at runtime (`render.set_sun_specular_disc`), which [`crate::RenderSettings::preset`]
    /// ignores.
    pub sun_specular_disc: bool,
    /// Rect and tube lights on a tier with no emitter budget (`emissive_lights` 0: `DesktopLow`,
    /// `MobileLow`, `MobileHigh`) as representative-point stand-ins (ADR-3247,
    /// [`crate::light_profiles::area_light_fallback`]): a cosine spot per emitting face of a rect,
    /// three radial cosine spots for a tube, each with the equal-area source size, shaded by the clustered loop every tier
    /// runs. Off in every preset (those tiers' rect and tube lights then emit nothing, the shipped
    /// look); a host turns it on at runtime (`render.set_area_light_fallback`), which
    /// [`crate::RenderSettings::preset`] ignores. A tier with an emitter budget never uses it.
    pub area_light_fallback: bool,
    /// Every point and spot light with a `LightSourceSize` shades its sphere through the LTC tables, as
    /// if it carried a `LightSourceLtc` marker (ADR-3247): the resolved highlight of a glossy surface
    /// takes the sphere's flat-topped shape instead of the representative point's lobe, and a sphere
    /// that dips below a surface's horizon still lights it. Off in every preset (sized lights keep
    /// the representative point, ADR-1367); a host turns it on at runtime
    /// (`render.set_sized_light_ltc`), which [`crate::RenderSettings::preset`] ignores. A light
    /// without a size, and the Mobile lite and terrain passes, are unchanged.
    pub sized_light_ltc: bool,
    /// The cap on the CPU copies of mesh and texture data a [`crate::host_memory::HostMeshStore`]
    /// keeps (ADR-7891): [`crate::host_memory::HostMemoryBudget::for_preset`] in each row. It
    /// changes no pixel (a dropped copy draws from its GPU buffers), and
    /// [`crate::RenderSettings::preset`] ignores it, so a host may set its own value and keep its
    /// preset. A process-wide override (`render.set_host_budget`) wins over it.
    pub host_memory: crate::host_memory::HostMemoryBudget,
}

/// Opt-in terrain shading cuts, each beyond a distance from the eye (lane terrain-perf;
/// measured on DO27's maps at 1440p on an RX 7900 in `docs/evidence/perf/do27-frame-20261007.md`).
/// [`TerrainBudget::OFF`] is the default look. A shared terrain draw reads it from the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TerrainBudget {
    /// Read the height pyramid for the per-pixel normal with hardware trilinear filtering
    /// (one fetch per tap instead of eight loads) where the device can filter 32-bit
    /// floats. Normals change by the filter's weight precision, everywhere.
    pub hw_filtered_normals: bool,
    /// Beyond this distance (metres) each pixel blends only its two strongest layers,
    /// without the height-texture blend; `0` = off. A 20 m band blends into it.
    pub far_layers_m: u32,
    /// Beyond this distance (metres) the terrain shades with its vertex normal instead of
    /// the per-pixel heightfield normal; `0` = off. A 20 m band blends into it.
    pub vertex_normal_m: u32,
    /// Cache the photoreal terrain surface in the terrain virtual texture (ADR-117, `Surface`
    /// cache, both LODs per pixel so frames without TAA stay stable) where the host has not
    /// chosen one itself (`TerrainSharedDraw::set_virtual_texture` wins). Distant ground is
    /// reconstructed from texels: slightly softer, with fine cracks lost; 671 MB of atlas (a ~1.9
    /// GB card peak on DO27's heaviest Low frame). The quality detector turns it on for Desktop
    /// Low on discrete GPUs with at least [`crate::quality::QualityTier::TERRAIN_VT_MIN_VRAM_MB`],
    /// on Linux and not on AMD for now ([`crate::quality::QualityTier::terrain_vt_default`],
    /// ADR-157 addendum v0.1.2); off in every preset.
    pub virtual_texture: bool,
    /// With [`Self::virtual_texture`]: on a device without forced early depth tests (DX12,
    /// Metal) the texture runs its late-depth path (`VirtualTextureSettings::late_depth_fallback`
    /// in `regia_terrain`) instead of staying off. Alone it does nothing. The quality detector
    /// sets it with the lever only where its `TerrainVtPolicy::windows` opt-in admits Windows
    /// ([`crate::quality::TerrainVtPolicy::late_depth`], ADR-2122); off in every preset.
    pub vt_late_depth: bool,
    /// Anisotropic filtering level of the terrain's layer, triplanar and macro textures, and
    /// the number of heightfield normals averaged along a grazing footprint (half the level,
    /// at most 4); `0` or `1` = off (the isotropic v0.1.7 look). Clamped to 16. Grazing ground
    /// keeps the detail of its pixel's short axis instead of the long axis' mean (ADR-192).
    /// [`Self::OFF`] has 16; the presets: Mobile Low 0, Mobile High 2, Desktop Low and Deck 4,
    /// Desktop High 8, Desktop Ultra 16.
    pub anisotropy: u8,
    /// The terrain's grass takes the foliage profile's transmission for light through a blade
    /// ([`crate::subsurface::slab_transmission`], ADR-3170) instead of its own back-lit term.
    /// [`FrameQuality::from_profile`] sets it from [`RenderProfile::subsurface_scattering`] (off in
    /// every preset); a host building its [`FrameQuality`] itself sets both. Not a cut:
    /// [`Self::is_off`] does not look at it.
    pub foliage_subsurface: bool,
}

impl TerrainBudget {
    /// Highest [`Self::anisotropy`] level (the sampler's clamp).
    pub const MAX_ANISOTROPY: u8 = 16;

    /// No cuts: the default look (full anisotropic filtering).
    pub const OFF: Self = Self {
        hw_filtered_normals: false,
        far_layers_m: 0,
        vertex_normal_m: 0,
        virtual_texture: false,
        vt_late_depth: false,
        anisotropy: Self::MAX_ANISOTROPY,
        foliage_subsurface: false,
    };

    /// The measured mid-range tier: hardware-filtered normals, two layers past 150 m, the
    /// vertex normal past 150 m, 8× anisotropic filtering.
    pub const MID_RANGE: Self = Self {
        hw_filtered_normals: true,
        far_layers_m: 150,
        vertex_normal_m: 150,
        virtual_texture: false,
        vt_late_depth: false,
        anisotropy: 8,
        foliage_subsurface: false,
    };

    /// No cuts, with anisotropic filtering at `level` (0 or 1 = off).
    #[must_use]
    pub const fn with_anisotropy(level: u8) -> Self {
        Self {
            anisotropy: level,
            ..Self::OFF
        }
    }

    /// The anisotropic filtering level the terrain applies: [`Self::anisotropy`] clamped to
    /// [`Self::MAX_ANISOTROPY`], `0` when off (0 or 1).
    #[must_use]
    pub fn anisotropy_level(&self) -> u8 {
        if self.anisotropy <= 1 {
            0
        } else {
            self.anisotropy.min(Self::MAX_ANISOTROPY)
        }
    }

    /// `true` when no cut is on and the virtual texture is not requested. The anisotropy
    /// level is a filtering setting, not a cut, and is not looked at.
    #[must_use]
    pub fn is_off(&self) -> bool {
        !self.hw_filtered_normals
            && self.far_layers_m == 0
            && self.vertex_normal_m == 0
            && !self.virtual_texture
    }
}

impl Default for TerrainBudget {
    fn default() -> Self {
        Self::OFF
    }
}

impl RenderProfile {
    /// The HDR colour format a frame with this profile renders into on a device with `caps`:
    /// `Rg11b10Ufloat` when the profile asks for it and the device renders it, `Rgba16Float`
    /// otherwise (the renderer's own choice). Warm jobs build effect and terrain pipelines for it.
    #[must_use]
    pub fn hdr_color_format(&self, caps: &crate::RenderCapabilities) -> wgpu::TextureFormat {
        match self.hdr_target {
            HdrTarget::Rg11B10 if caps.rg11b10_renderable => wgpu::TextureFormat::Rg11b10Ufloat,
            _ => wgpu::TextureFormat::Rgba16Float,
        }
    }

    /// Returns the table row for `preset`.
    #[must_use]
    pub fn from_preset(preset: RenderProfilePreset) -> Self {
        match preset {
            RenderProfilePreset::MobileLow => Self {
                hdr_target: HdrTarget::Rg11B10,
                render_scale: 0.7,
                anti_aliasing: AntiAliasing::None,
                depth_prepass: false,
                csm_cascade_count: 1,
                csm_resolution: 1024,
                local_shadow_atlas_size: 0,
                local_shadow_max_lights: 0,
                shadow_filter: ShadowFilter::Hardware2x2,
                ssao: SsaoMode::Off,
                contact_shadows: false,
                reflection_occlusion: false,
                bent_normals: false,
                bloom: false,
                bloom_mip_count: 0,
                auto_exposure: false,
                max_reflection_probes: 1,
                max_lights_per_cluster: 16,
                max_visible_lights: 32,
                frame_cap_fps: None,
                gpu_driven: false,
                horizontal_plus: false,
                physical_atmosphere: false,
                volumetric_fog: false,
                volumetric_clouds: false,
                cloud_reprojection: true,
                cloud_step_percent: 100,
                cloud_cone_light: false,
                cloud_march: CloudMarch::REALTIME,
                paged_sun_shadows: false,
                translucent_shadow_cascades: 0,
                translucent_local_shadows: false,
                near_sun_tile: 0,
                gi: GiMode::Ambient,
                screen_space_gi: false,
                gi_probes: false,
                gi_tier: crate::DynamicGiTier::of_preset(RenderProfilePreset::MobileLow),
                radiance_cache: false,
                ssr: false,
                reflection_probes: false,
                parallax_occlusion: false,
                terrain_budget: TerrainBudget::with_anisotropy(0),
                screen_space_refraction: false,
                depth_velocity: true,
                temporal_upscaler: TemporalUpscaler::Taau,
                visibility_buffer: false,
                visibility_path: crate::visbuffer::VisibilityPath::DEFAULT,
                sun_shadow_trace: crate::hwrt::RtShadowRequest::Off,
                frame_pipelining: crate::FramePipelining::OFF,
                subsurface_scattering: false,
                blue_noise: false,
                order_independent_transparency: false,
                two_phase_occlusion: false,
                masked_occlusion: false,
                texture_streaming: TextureStreaming::for_preset(preset),
                spatial_upscaler: SpatialUpscaler::Sgsr,
                motion_blur: false,
                depth_of_field: false,
                lens_effects: false,
                emissive_lights: 0,
                mesh_lod_bias: 0.5,
                mesh_lod_distance_scale: 1.0,
                mesh_lod_shadow_scale: 0.5,
                upscale_quality: None,
                forward_lite: crate::forward_lite::ForwardLite::Low,
                lightmap_tier: crate::lightmap::LightmapTier::Bilinear,
                subsurface_kernel: crate::subsurface::SubsurfaceKernel::Screen,
                shadow_occlusion: false,
                many_light_sampling: false,
                aerial_from_tables: false,
                horizon_ao: false,
                angular_sun_penumbra: false,
                restir_gi: false,

                cluster_residency: crate::visbuffer::ClusterResidency::OFF,
                brdf_energy: crate::BrdfEnergy::Legacy,
                forward_decals: false,
                sun_specular_disc: false,
                area_light_fallback: false,
                sized_light_ltc: false,
                host_memory: crate::host_memory::HostMemoryBudget::for_preset(preset),
            },
            RenderProfilePreset::MobileHigh => Self {
                hdr_target: HdrTarget::Rg11B10,
                // 85 % with SMAA at the internal size and SGSR to the output (ADR-102; was 85 %
                // bilinear + FXAA). 75 % with SGSR lost to the old preset on every DO27 plate
                // (SSIM vs a 4× supersampled frame), so the scale stays. Not MSAA 4x: the native
                // terrain draw and the planet sky (sky bodies, airless/Mars skies) run on
                // single-sample frames only, so an MSAA frame drew no terrain and no sky and
                // metered black (look-r2 round 8, ADR).
                render_scale: 0.85,
                anti_aliasing: AntiAliasing::Smaa,
                depth_prepass: false,
                csm_cascade_count: 2,
                csm_resolution: 2048,
                local_shadow_atlas_size: 4096,
                local_shadow_max_lights: 4,
                shadow_filter: ShadowFilter::Pcf3x3,
                ssao: SsaoMode::Off,
                contact_shadows: false,
                reflection_occlusion: false,
                bent_normals: false,
                bloom: true,
                bloom_mip_count: 4,
                auto_exposure: true,
                max_reflection_probes: 2,
                max_lights_per_cluster: 32,
                max_visible_lights: 64,
                frame_cap_fps: None,
                gpu_driven: false,
                horizontal_plus: false,
                physical_atmosphere: false,
                volumetric_fog: false,
                volumetric_clouds: false,
                cloud_reprojection: true,
                cloud_step_percent: 100,
                cloud_cone_light: false,
                cloud_march: CloudMarch::REALTIME,
                paged_sun_shadows: false,
                translucent_shadow_cascades: 1,
                translucent_local_shadows: false,
                near_sun_tile: 0,
                gi: GiMode::Ambient,
                screen_space_gi: false,
                gi_probes: false,
                gi_tier: crate::DynamicGiTier::of_preset(RenderProfilePreset::MobileHigh),
                radiance_cache: false,
                ssr: false,
                reflection_probes: false,
                parallax_occlusion: false,
                terrain_budget: TerrainBudget::with_anisotropy(2),
                screen_space_refraction: false,
                depth_velocity: true,
                temporal_upscaler: TemporalUpscaler::Taau,
                visibility_buffer: false,
                visibility_path: crate::visbuffer::VisibilityPath::DEFAULT,
                sun_shadow_trace: crate::hwrt::RtShadowRequest::Off,
                frame_pipelining: crate::FramePipelining::OFF,
                subsurface_scattering: false,
                blue_noise: false,
                order_independent_transparency: false,
                two_phase_occlusion: false,
                masked_occlusion: false,
                texture_streaming: TextureStreaming::for_preset(preset),
                spatial_upscaler: SpatialUpscaler::Sgsr,
                motion_blur: false,
                depth_of_field: false,
                lens_effects: false,
                emissive_lights: 0,
                mesh_lod_bias: 0.7,
                mesh_lod_distance_scale: 1.0,
                mesh_lod_shadow_scale: 0.5,
                upscale_quality: None,
                forward_lite: crate::forward_lite::ForwardLite::High,
                lightmap_tier: crate::lightmap::LightmapTier::Directional,
                subsurface_kernel: crate::subsurface::SubsurfaceKernel::Screen,
                shadow_occlusion: false,
                many_light_sampling: false,
                aerial_from_tables: false,
                horizon_ao: false,
                angular_sun_penumbra: false,
                restir_gi: false,

                cluster_residency: crate::visbuffer::ClusterResidency::OFF,
                brdf_energy: crate::BrdfEnergy::Legacy,
                forward_decals: false,
                sun_specular_disc: false,
                area_light_fallback: false,
                sized_light_ltc: false,
                host_memory: crate::host_memory::HostMemoryBudget::for_preset(preset),
            },
            RenderProfilePreset::DesktopLow => Self {
                hdr_target: HdrTarget::Rgba16f,
                render_scale: 0.75,
                anti_aliasing: AntiAliasing::Fxaa,
                depth_prepass: true,
                csm_cascade_count: 2,
                csm_resolution: 2048,
                local_shadow_atlas_size: 4096,
                local_shadow_max_lights: 8,
                shadow_filter: ShadowFilter::Pcf,
                ssao: SsaoMode::HalfRes,
                contact_shadows: true,
                reflection_occlusion: false,
                bent_normals: false,
                bloom: true,
                bloom_mip_count: 4,
                auto_exposure: true,
                max_reflection_probes: 2,
                max_lights_per_cluster: 64,
                max_visible_lights: 128,
                frame_cap_fps: None,
                // Indirect draws and the culled object list (the adapter decides the
                // strategy; CPU draws where it lacks indirect support) and retained,
                // paged sun cascades: on DO27's content at 1280×800 the per-draw and
                // per-cascade work did not shrink with resolution (look-r2 round 9).
                gpu_driven: true,
                horizontal_plus: true,
                physical_atmosphere: false,
                volumetric_fog: false,
                volumetric_clouds: false,
                cloud_reprojection: true,
                cloud_step_percent: 100,
                cloud_cone_light: false,
                cloud_march: CloudMarch::REALTIME,
                paged_sun_shadows: true,
                translucent_shadow_cascades: 2,
                translucent_local_shadows: true,
                near_sun_tile: 0,
                gi: GiMode::Ambient,
                screen_space_gi: false,
                gi_probes: false,
                gi_tier: crate::DynamicGiTier::of_preset(RenderProfilePreset::DesktopLow),
                radiance_cache: false,
                ssr: false,
                reflection_probes: false,
                parallax_occlusion: false,
                terrain_budget: TerrainBudget::with_anisotropy(4),
                screen_space_refraction: false,
                depth_velocity: true,
                temporal_upscaler: TemporalUpscaler::Taau,
                visibility_buffer: false,
                visibility_path: crate::visbuffer::VisibilityPath::DEFAULT,
                sun_shadow_trace: crate::hwrt::RtShadowRequest::Off,
                frame_pipelining: crate::FramePipelining::OFF,
                subsurface_scattering: false,
                blue_noise: false,
                order_independent_transparency: false,
                two_phase_occlusion: false,
                masked_occlusion: true,
                texture_streaming: TextureStreaming::for_preset(preset),
                spatial_upscaler: SpatialUpscaler::Bilinear,
                motion_blur: false,
                depth_of_field: false,
                lens_effects: false,
                emissive_lights: 0,
                mesh_lod_bias: 0.85,
                mesh_lod_distance_scale: 1.0,
                mesh_lod_shadow_scale: 0.5,
                upscale_quality: None,
                forward_lite: crate::forward_lite::ForwardLite::Off,
                lightmap_tier: crate::lightmap::LightmapTier::Full,
                subsurface_kernel: crate::subsurface::SubsurfaceKernel::Screen,
                shadow_occlusion: false,
                many_light_sampling: false,
                aerial_from_tables: false,
                horizon_ao: false,
                angular_sun_penumbra: false,
                restir_gi: false,

                cluster_residency: crate::visbuffer::ClusterResidency::OFF,
                brdf_energy: crate::BrdfEnergy::Legacy,
                forward_decals: false,
                sun_specular_disc: false,
                area_light_fallback: false,
                sized_light_ltc: false,
                host_memory: crate::host_memory::HostMemoryBudget::for_preset(preset),
            },
            RenderProfilePreset::DesktopHigh => Self {
                hdr_target: HdrTarget::Rgba16f,
                // Native scale (specs/13). A game may opt into 0.67 for performance: at
                // exactly 670 permille with a 2048² cascade it also gets half-res SSGI,
                // 6+8 PCSS taps and no full-res SSR (`desktop_high_budget`). Kept off by
                // default: at 0.67 the current TAAU loses thin geometry and the floor
                // reflections (lead, 2026-10-07, against the owner's look bar).
                render_scale: 1.0,
                anti_aliasing: AntiAliasing::Taa,
                depth_prepass: true,
                csm_cascade_count: 4,
                csm_resolution: 2048,
                local_shadow_atlas_size: 8192,
                local_shadow_max_lights: 32,
                shadow_filter: ShadowFilter::PcssLite,
                ssao: SsaoMode::GtaoHalfRes,
                contact_shadows: true,
                reflection_occlusion: true,
                bent_normals: true,
                bloom: true,
                bloom_mip_count: 5,
                auto_exposure: true,
                max_reflection_probes: 4,
                max_lights_per_cluster: 128,
                max_visible_lights: 256,
                frame_cap_fps: None,
                gpu_driven: true,
                horizontal_plus: true,
                physical_atmosphere: true,
                volumetric_fog: true,
                volumetric_clouds: true,
                cloud_reprojection: true,
                cloud_step_percent: 100,
                cloud_cone_light: true,
                cloud_march: CloudMarch::REALTIME,
                paged_sun_shadows: true,
                translucent_shadow_cascades: 4,
                translucent_local_shadows: true,
                near_sun_tile: 2048,
                gi: GiMode::Probes,
                screen_space_gi: true,
                gi_probes: true,
                gi_tier: crate::DynamicGiTier::of_preset(RenderProfilePreset::DesktopHigh),
                radiance_cache: false,
                ssr: true,
                reflection_probes: true,
                parallax_occlusion: true,
                terrain_budget: TerrainBudget::with_anisotropy(8),
                screen_space_refraction: true,
                depth_velocity: true,
                temporal_upscaler: TemporalUpscaler::Taau,
                visibility_buffer: false,
                visibility_path: crate::visbuffer::VisibilityPath::DEFAULT,
                sun_shadow_trace: crate::hwrt::RtShadowRequest::Off,
                frame_pipelining: crate::FramePipelining::OFF,
                subsurface_scattering: false,
                blue_noise: false,
                order_independent_transparency: false,
                two_phase_occlusion: true,
                masked_occlusion: false,
                texture_streaming: TextureStreaming::for_preset(preset),
                spatial_upscaler: SpatialUpscaler::Bilinear,
                motion_blur: true,
                depth_of_field: true,
                lens_effects: true,
                emissive_lights: 96,
                mesh_lod_bias: 1.0,
                mesh_lod_distance_scale: 2.0,
                mesh_lod_shadow_scale: 0.35,
                upscale_quality: None,
                forward_lite: crate::forward_lite::ForwardLite::Off,
                lightmap_tier: crate::lightmap::LightmapTier::Full,
                subsurface_kernel: crate::subsurface::SubsurfaceKernel::Screen,
                shadow_occlusion: false,
                many_light_sampling: false,
                aerial_from_tables: false,
                horizon_ao: false,
                angular_sun_penumbra: false,
                restir_gi: false,

                cluster_residency: crate::visbuffer::ClusterResidency::OFF,
                brdf_energy: crate::BrdfEnergy::Legacy,
                forward_decals: false,
                sun_specular_disc: false,
                area_light_fallback: false,
                sized_light_ltc: false,
                host_memory: crate::host_memory::HostMemoryBudget::for_preset(preset),
            },
            RenderProfilePreset::DesktopUltra => Self {
                hdr_target: HdrTarget::Rgba16f,
                render_scale: 1.0,
                anti_aliasing: AntiAliasing::Taa,
                depth_prepass: true,
                csm_cascade_count: 4,
                csm_resolution: 4096,
                local_shadow_atlas_size: 8192,
                local_shadow_max_lights: 64,
                shadow_filter: ShadowFilter::PcssLite,
                ssao: SsaoMode::GtaoFull,
                contact_shadows: true,
                reflection_occlusion: true,
                bent_normals: true,
                bloom: true,
                bloom_mip_count: 6,
                auto_exposure: true,
                max_reflection_probes: 4,
                max_lights_per_cluster: 256,
                max_visible_lights: 512,
                frame_cap_fps: None,
                gpu_driven: true,
                horizontal_plus: true,
                physical_atmosphere: true,
                volumetric_fog: true,
                volumetric_clouds: true,
                cloud_reprojection: true,
                cloud_step_percent: 150,
                cloud_cone_light: true,
                cloud_march: CloudMarch::REALTIME,
                paged_sun_shadows: true,
                translucent_shadow_cascades: 4,
                translucent_local_shadows: true,
                near_sun_tile: 4096,
                gi: GiMode::Probes,
                screen_space_gi: true,
                gi_probes: true,
                gi_tier: crate::DynamicGiTier::of_preset(RenderProfilePreset::DesktopUltra),
                radiance_cache: false,
                ssr: true,
                reflection_probes: true,
                parallax_occlusion: true,
                terrain_budget: TerrainBudget::with_anisotropy(16),
                screen_space_refraction: true,
                depth_velocity: true,
                temporal_upscaler: TemporalUpscaler::Taau,
                visibility_buffer: false,
                visibility_path: crate::visbuffer::VisibilityPath::DEFAULT,
                sun_shadow_trace: crate::hwrt::RtShadowRequest::Off,
                frame_pipelining: crate::FramePipelining::OFF,
                subsurface_scattering: false,
                blue_noise: false,
                order_independent_transparency: false,
                two_phase_occlusion: true,
                masked_occlusion: false,
                texture_streaming: TextureStreaming::for_preset(preset),
                spatial_upscaler: SpatialUpscaler::Bilinear,
                motion_blur: true,
                depth_of_field: true,
                lens_effects: true,
                emissive_lights: 160,
                mesh_lod_bias: 1.15,
                mesh_lod_distance_scale: 6.0,
                mesh_lod_shadow_scale: 0.15,
                upscale_quality: None,
                forward_lite: crate::forward_lite::ForwardLite::Off,
                lightmap_tier: crate::lightmap::LightmapTier::Full,
                subsurface_kernel: crate::subsurface::SubsurfaceKernel::Screen,
                shadow_occlusion: false,
                many_light_sampling: false,
                aerial_from_tables: false,
                horizon_ao: false,
                angular_sun_penumbra: false,
                restir_gi: false,

                cluster_residency: crate::visbuffer::ClusterResidency::OFF,
                brdf_energy: crate::BrdfEnergy::Legacy,
                forward_decals: false,
                sun_specular_disc: false,
                area_light_fallback: false,
                sized_light_ltc: false,
                host_memory: crate::host_memory::HostMemoryBudget::for_preset(preset),
            },
            RenderProfilePreset::Deck => Self {
                hdr_target: HdrTarget::Rgba16f,
                render_scale: 0.7,
                anti_aliasing: AntiAliasing::Taa,
                depth_prepass: true,
                csm_cascade_count: 2,
                csm_resolution: 2048,
                local_shadow_atlas_size: 4096,
                local_shadow_max_lights: 8,
                shadow_filter: ShadowFilter::Pcf,
                ssao: SsaoMode::HalfRes,
                contact_shadows: true,
                reflection_occlusion: false,
                bent_normals: false,
                bloom: true,
                bloom_mip_count: 4,
                auto_exposure: true,
                max_reflection_probes: 2,
                max_lights_per_cluster: 64,
                max_visible_lights: 96,
                frame_cap_fps: Some(RenderProfilePreset::DECK_FRAME_CAP_FPS),
                // Indirect draws and the culled object list (the adapter decides the
                // strategy; CPU draws where it lacks indirect support) and retained,
                // paged sun cascades: on DO27's content at 1280×800 the per-draw and
                // per-cascade work did not shrink with resolution (look-r2 round 9).
                gpu_driven: true,
                horizontal_plus: true,
                physical_atmosphere: false,
                volumetric_fog: false,
                volumetric_clouds: false,
                cloud_reprojection: true,
                cloud_step_percent: 100,
                cloud_cone_light: false,
                cloud_march: CloudMarch::REALTIME,
                paged_sun_shadows: true,
                translucent_shadow_cascades: 2,
                translucent_local_shadows: true,
                near_sun_tile: 0,
                gi: GiMode::Ambient,
                screen_space_gi: false,
                gi_probes: false,
                gi_tier: crate::DynamicGiTier::of_preset(RenderProfilePreset::Deck),
                radiance_cache: false,
                ssr: false,
                reflection_probes: false,
                parallax_occlusion: false,
                terrain_budget: TerrainBudget::with_anisotropy(4),
                screen_space_refraction: false,
                depth_velocity: true,
                temporal_upscaler: TemporalUpscaler::Taau,
                visibility_buffer: false,
                visibility_path: crate::visbuffer::VisibilityPath::DEFAULT,
                sun_shadow_trace: crate::hwrt::RtShadowRequest::Off,
                frame_pipelining: crate::FramePipelining::OFF,
                subsurface_scattering: false,
                blue_noise: false,
                order_independent_transparency: false,
                two_phase_occlusion: false,
                masked_occlusion: true,
                texture_streaming: TextureStreaming::for_preset(preset),
                spatial_upscaler: SpatialUpscaler::Sgsr,
                motion_blur: true,
                depth_of_field: true,
                lens_effects: false,
                emissive_lights: 0,
                mesh_lod_bias: 0.8,
                mesh_lod_distance_scale: 1.0,
                mesh_lod_shadow_scale: 0.5,
                upscale_quality: None,
                forward_lite: crate::forward_lite::ForwardLite::Off,
                lightmap_tier: crate::lightmap::LightmapTier::Full,
                subsurface_kernel: crate::subsurface::SubsurfaceKernel::Screen,
                shadow_occlusion: false,
                many_light_sampling: false,
                aerial_from_tables: false,
                horizon_ao: false,
                angular_sun_penumbra: false,
                restir_gi: false,

                cluster_residency: crate::visbuffer::ClusterResidency::OFF,
                brdf_energy: crate::BrdfEnergy::Legacy,
                forward_decals: false,
                sun_specular_disc: false,
                area_light_fallback: false,
                sized_light_ltc: false,
                host_memory: crate::host_memory::HostMemoryBudget::for_preset(preset),
            },
            RenderProfilePreset::Cinematic => crate::cinematic::cinematic_profile(),
        }
    }

    /// This profile without temporal AA: TAA becomes SMAA (a scaled frame then reaches the
    /// output through [`Self::spatial_upscaler`]), any other AA is kept. The Deck's
    /// fallback when history cannot be used (a game that turns jitter off, cut scenes
    /// that must not ghost); the presets themselves are unchanged.
    #[must_use]
    pub fn non_temporal(&self) -> Self {
        let mut profile = self.clone();
        if profile.anti_aliasing == AntiAliasing::Taa {
            profile.anti_aliasing = AntiAliasing::Smaa;
        }
        profile
    }

    /// Internal render width/height from the output size and
    /// [`render_scale`](Self::render_scale), or the exact size of
    /// [`upscale_quality`](Self::upscale_quality) when one is set.
    #[must_use]
    pub fn scaled_extent(&self, output_width: u32, output_height: u32) -> (u32, u32) {
        if let Some(mode) = self.upscale_quality {
            return mode.render_size((output_width, output_height));
        }
        let w = ((output_width as f32) * self.render_scale).round().max(1.0) as u32;
        let h = ((output_height as f32) * self.render_scale).round().max(1.0) as u32;
        (w, h)
    }
}

impl RenderProfile {
    /// `preset` with the visibility buffer selectable for dense scenes (ADR-2071): frames take the
    /// path while the scene is dense ([`crate::visbuffer::DenseSceneSelect::DEFAULT`]), and the
    /// resolve runs in one draw where the device has the texture table. `None` on every preset but
    /// Desktop High and Ultra: the others have not been measured. No preset uses this by itself;
    /// a game picks it, and the owner decides by eye before a preset does.
    #[must_use]
    pub fn dense_scenes(preset: RenderProfilePreset) -> Option<Self> {
        if !matches!(
            preset,
            RenderProfilePreset::DesktopHigh | RenderProfilePreset::DesktopUltra
        ) {
            return None;
        }
        let mut profile = Self::from_preset(preset);
        profile.visibility_buffer = true;
        profile.visibility_path = crate::visbuffer::VisibilityPath {
            select: crate::visbuffer::VisibilitySelect::Dense(crate::visbuffer::DenseSceneSelect::DEFAULT),
            single_pass_resolve: true,
        };
        Some(profile)
    }
}

impl Default for RenderProfile {
    fn default() -> Self {
        Self::from_preset(RenderProfilePreset::DesktopHigh)
    }
}

/// Per-frame quality knobs the renderer consumes, derived from a [`RenderProfile`]
/// (`specs/13` § 3) or the golden-harness baseline.
///
/// The runtime path builds this from the active profile every frame
/// ([`FrameQuality::from_profile`]); the lab/golden harness pins
/// [`FrameQuality::LAB`] so blessed images stay reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameQuality {
    /// Directional-light CSM cascades (1..=[`crate::forward::CASCADE_COUNT`]).
    pub csm_cascade_count: u32,
    /// Shadow map resolution per cascade edge (pixels).
    pub csm_resolution: u32,
    /// Local-light shadow atlas edge size (0 = local shadows off).
    pub local_shadow_atlas_size: u32,
    /// Maximum local lights assigned atlas tiles.
    pub local_shadow_max_lights: u32,
    /// MSAA sample count for the colour/depth passes (1 or 4).
    pub msaa_samples: u32,
    /// Internal resolution in thousandths of the output (`1000` = scale 1).
    pub render_scale_permille: u16,
    /// HDR colour target. `Rg11b10` is used only when the device can render it.
    pub hdr_target: HdrTarget,
    /// Shadow filter kernel.
    pub shadow_filter: ShadowFilter,
    /// Screen-space ambient occlusion. Off leaves indirect lighting unchanged.
    pub ssao: SsaoMode,
    /// Screen-space contact shadows on the sun (see [`RenderProfile::contact_shadows`]).
    /// Off on [`Self::LAB`] and [`Self::EDITOR`], so their goldens stay put.
    pub contact_shadows: bool,
    /// Reflection occlusion from the SSAO pass (see [`RenderProfile::reflection_occlusion`]):
    /// only with a GTAO [`Self::ssao`] row. Off on [`Self::LAB`] and [`Self::EDITOR`].
    pub reflection_occlusion: bool,
    /// Bent normals from the SSAO pass for the indirect lookups and the cone specular occlusion
    /// (see [`RenderProfile::bent_normals`]): only with a GTAO [`Self::ssao`] row. Off on
    /// [`Self::LAB`] and [`Self::EDITOR`], so their goldens stay put.
    pub bent_normals: bool,
    /// Point/spot lights stored per froxel. The cluster buffer is sized for the
    /// largest preset (256); this is the live cap the compute pass honours.
    pub max_lights_per_cluster: u32,
    /// Reflection-probe cap per cluster.
    ///
    /// Reflection probes blended per view. [`Self::probe_budget`] truncates the
    /// list, and the same cap is uploaded in the frame uniform. The value changes
    /// pixels only when [`Self::reflection_probes`] is on and probes are bound.
    pub max_reflection_probes: u32,
    /// GPU-driven frustum + Hi-Z + indirect draws.
    pub gpu_driven: bool,
    /// Temporal anti-aliasing: jitter plus history accumulation, upscaling from
    /// [`Self::render_scale`] to the output (1:1 at native scale).
    pub taau: bool,
    /// Horizontal-plus FOV (`output::horizontal_plus_fov_y`).
    pub horizontal_plus: bool,
    /// LUT atmosphere. Off on [`Self::LAB`] and [`Self::EDITOR`].
    pub physical_atmosphere: bool,
    /// Froxel volumetric fog.
    pub volumetric_fog: bool,
    /// Raymarched volumetric clouds.
    pub volumetric_clouds: bool,
    /// See [`RenderProfile::cloud_reprojection`]. Off on [`Self::LAB`] and
    /// [`Self::EDITOR`], so a multi-frame golden marches every texel.
    pub cloud_reprojection: bool,
    /// See [`RenderProfile::cloud_step_percent`].
    pub cloud_step_percent: u32,
    /// See [`RenderProfile::cloud_cone_light`]. Off on [`Self::LAB`] and [`Self::EDITOR`],
    /// so the lab goldens keep the v0.1 light march unless a test turns it on.
    pub cloud_cone_light: bool,
    /// See [`RenderProfile::cloud_march`]. [`CloudMarch::REALTIME`] on [`Self::LAB`] and
    /// [`Self::EDITOR`].
    pub cloud_march: CloudMarch,
    /// Cached paged sun shadows. The lab baseline redraws every cascade.
    pub paged_sun_shadows: bool,
    /// Keep every sun cascade on a world-fixed texel lattice: its center is snapped to its own
    /// texel grid in light space ([`crate::sun_cascades`], ADR-1236), so a turning or moving
    /// camera does not slide the shadow texels under static casters (shimmer). Paged sun
    /// shadows always snap. On in every preset and the editor; off on [`Self::LAB`], whose
    /// goldens were blessed with the free-sliding fit.
    pub stable_sun_cascades: bool,
    /// See [`RenderProfile::translucent_shadow_cascades`] (clamped to
    /// [`Self::csm_cascade_count`] by the frame).
    pub translucent_shadow_cascades: u32,
    /// See [`RenderProfile::translucent_local_shadows`].
    pub translucent_local_shadows: bool,
    /// See [`RenderProfile::near_sun_tile`] (`0` = off; the lab baseline is off).
    pub near_sun_tile: u32,
    /// Diffuse GI tier. [`GiMode::Ambient`] leaves the forward equation unchanged.
    pub gi: GiMode,
    /// Screen-space colour bleeding. Requires a depth prepass to run.
    pub screen_space_gi: bool,
    /// GPU-traced irradiance probes ([`RenderProfile::gi_probes`]).
    pub gi_probes: bool,
    /// Light the frame with the baked GI volume when one is installed
    /// ([`crate::FrameRenderer::set_baked_gi`]); DDGI and SSGI are then off and full-resolution
    /// GTAO runs at half resolution. On in every preset (a map without a volume is unchanged),
    /// off on [`Self::LAB`] and [`Self::EDITOR`]; [`crate::RenderSettings::quality`] turns it
    /// off when the player chose the [`GiLook::Balanced`] look.
    pub baked_gi: bool,
    /// Screen-space reflections. Requires a depth prepass to run.
    pub ssr: bool,
    /// Sample bound reflection probes. Off during probe capture so it cannot recurse.
    pub reflection_probes: bool,
    /// Point and spot lights uploaded this view. The storage buffer holds
    /// [`crate::MAX_UPLOADED_LIGHTS`]. The lab harness stays at 64 so frames
    /// that already fit do not reshuffle shadow tiles.
    pub max_visible_lights: u32,
    /// Neutral interior ambient: the shader skips sky IBL and tints the flat
    /// illuminance white. Off for outdoor frames and the golden harness.
    pub interior_ambient: bool,
    /// Local-shadow diagnostic. `0` is the lit frame. `1` draws the average
    /// local-light shadow factor (1 = lit) and skips tonemap and post. `2`
    /// colours each atlas tile and the tile UV, so a sample that crosses into
    /// the next tile is visible. `3` draws the sun on meshes: R the shadow-map visibility,
    /// G the contact shadow, B n·l of the shading normal. `4` the
    /// diffuse albedo (base colour × vertex colour × (1 − metallic)), `5` the shading normal,
    /// `6` the sun cascades' receiver-plane normal (both as `n * 0.5 + 0.5`). Probe capture
    /// forces this back to `0`.
    ///
    /// Packed in `LabFrame::pcss.w`. `pcss.z` is the parallax gate.
    pub shadow_debug: u32,
    /// Parallax occlusion. Off on the lab and editor baselines so goldens stay put.
    ///
    /// Packed in `LabFrame::pcss.z`. `quality.w` is the PCSS noise index.
    pub parallax_occlusion: bool,
    /// Opt-in terrain shading cuts handed to the shared draw ([`TerrainBudget`]).
    pub terrain_budget: TerrainBudget,
    /// Screen-space refraction for transmissive materials
    /// ([`RenderProfile::screen_space_refraction`]; on in Desktop High and Ultra). Runs only on an
    /// HDR frame without MSAA.
    pub screen_space_refraction: bool,
    /// See [`RenderProfile::depth_velocity`].
    pub depth_velocity: bool,
    /// Draw rigid opaque meshes with meshlets through the visibility buffer
    /// ([`RenderProfile::visibility_buffer`]). Off on every baseline.
    pub visibility_buffer: bool,
    /// See [`RenderProfile::visibility_path`].
    pub visibility_path: crate::visbuffer::VisibilityPath,
    /// See [`RenderProfile::sun_shadow_trace`]. Off on every baseline.
    pub sun_shadow_trace: crate::hwrt::RtShadowRequest,
    /// See [`RenderProfile::frame_pipelining`]. Off on every baseline.
    pub frame_pipelining: crate::FramePipelining,
    /// See [`RenderProfile::subsurface_scattering`]. Off on every baseline.
    pub subsurface_scattering: bool,
    /// See [`RenderProfile::blue_noise`]. Off on every baseline.
    pub blue_noise: bool,
    /// See [`RenderProfile::order_independent_transparency`]. Off on every baseline.
    pub order_independent_transparency: bool,
    /// Material texture streaming and the resident texture budget ([`crate::texture_stream`]).
    /// On in every preset ([`RenderProfile::texture_streaming`]); off on the lab and editor
    /// baselines (fixed-content tools and goldens).
    pub texture_streaming: TextureStreaming,
    /// Geometric specular anti-aliasing on meshes ([`crate::specular_aa`]): the GGX lobe
    /// widens by the screen-space variance of the shading normal, so minified normal detail
    /// and tight curvature stop sparkling under motion. About free; on in every preset.
    pub specular_aa: bool,
    /// SMAA 1× in the post stack after the tonemap (instead of FXAA).
    pub smaa: bool,
    /// See [`RenderProfile::spatial_upscaler`]. Consulted only when the frame is scaled and
    /// [`Self::taau`] is off.
    pub spatial_upscaler: SpatialUpscaler,
    /// See [`RenderProfile::emissive_lights`]. 0 on the lab and editor baselines.
    pub emissive_lights: u32,
    /// See [`RenderProfile::upscale_quality`]: `None` on every baseline and preset. With a mode,
    /// [`Self::scaled_extent`] returns the mode's exact render size while
    /// [`Self::render_scale_permille`] is the mode's own scale; any other permille (dynamic
    /// resolution, a texture view or probe capture forcing 1000) scales as before.
    pub upscale_quality: Option<crate::UpscaleQuality>,
    /// The tier of the lite forward shader this frame asks for ([`RenderProfile::forward_lite`]).
    pub forward_lite: crate::forward_lite::ForwardLite,
    /// The lightmap lookup this frame allows ([`RenderProfile::lightmap_tier`]).
    pub lightmap_tier: crate::lightmap::LightmapTier,
    /// See [`RenderProfile::many_light_sampling`]. Off on every baseline.
    pub many_light_sampling: bool,
    /// See [`RenderProfile::aerial_from_tables`]. Off on every baseline.
    pub aerial_from_tables: bool,
    /// See [`RenderProfile::horizon_ao`]. Off on every baseline.
    pub horizon_ao: bool,
    /// See [`RenderProfile::angular_sun_penumbra`]. Off on every baseline.
    pub angular_sun_penumbra: bool,
    /// See [`RenderProfile::restir_gi`]. Off on every baseline.
    pub restir_gi: bool,
    /// See [`RenderProfile::subsurface_kernel`]: the screen kernel on every baseline.
    pub subsurface_kernel: crate::subsurface::SubsurfaceKernel,

    /// See [`RenderProfile::cluster_residency`]. Off on every baseline.
    pub cluster_residency: crate::visbuffer::ClusterResidency,
}

impl FrameQuality {
    /// The golden-harness/lab baseline: 2 cascades at 512², a 1024² atlas,
    /// no MSAA (matches the blessed images).
    pub const LAB: Self = Self {
        csm_cascade_count: 2,
        csm_resolution: 512,
        local_shadow_atlas_size: 1024,
        local_shadow_max_lights: 16,
        msaa_samples: 1,
        render_scale_permille: 1000,
        hdr_target: HdrTarget::Rgba16f,
        shadow_filter: ShadowFilter::Pcf3x3,
        ssao: SsaoMode::Off,
        contact_shadows: false,
        reflection_occlusion: false,
        bent_normals: false,
        max_lights_per_cluster: crate::lab_frame::LAB_MAX_PER_CLUSTER,
        max_reflection_probes: 1,
        gpu_driven: false,
        taau: false,
        horizontal_plus: false,
        physical_atmosphere: false,
        volumetric_fog: false,
        volumetric_clouds: false,
        cloud_reprojection: false,
        cloud_step_percent: 100,
        cloud_cone_light: false,
        cloud_march: CloudMarch::REALTIME,
        paged_sun_shadows: false,
        stable_sun_cascades: false,
        translucent_shadow_cascades: 4,
        translucent_local_shadows: true,
        near_sun_tile: 0,
        gi: GiMode::Ambient,
        screen_space_gi: false,
        gi_probes: false,
        baked_gi: false,
        ssr: false,
        reflection_probes: false,
        max_visible_lights: crate::lab_frame::LAB_MAX_LIGHTS,
        interior_ambient: false,
        shadow_debug: 0,
        parallax_occlusion: false,
        terrain_budget: TerrainBudget::OFF,
        screen_space_refraction: false,
        depth_velocity: false,
        visibility_buffer: false,
        visibility_path: crate::visbuffer::VisibilityPath::DEFAULT,
        sun_shadow_trace: crate::hwrt::RtShadowRequest::Off,
        frame_pipelining: crate::FramePipelining::OFF,
        subsurface_scattering: false,
        blue_noise: false,
        order_independent_transparency: false,
        texture_streaming: TextureStreaming::OFF,
        specular_aa: true,
        smaa: false,
        spatial_upscaler: SpatialUpscaler::Bilinear,
        emissive_lights: 0,
        upscale_quality: None,
        forward_lite: crate::forward_lite::ForwardLite::Off,
        lightmap_tier: crate::lightmap::LightmapTier::Full,
        many_light_sampling: false,
        aerial_from_tables: false,
        horizon_ao: false,
        angular_sun_penumbra: false,
        restir_gi: false,
        subsurface_kernel: crate::subsurface::SubsurfaceKernel::Screen,

        cluster_residency: crate::visbuffer::ClusterResidency::OFF,
    };

    /// Editor viewport, material preview and prefab edit (`specs/13` § 12).
    ///
    /// Cheap and stable on purpose: native scale, no TAAU jitter, no GPU-driven
    /// culling, no SSAO, no MSAA. Editing and paused play both stay here, so the
    /// picture does not shimmer and a play-in-editor frame matches the edit
    /// frame. This is not [`Self::from_profile`] of Desktop High or Deck — those
    /// presets turn on GPU-driven draws and, on Deck, temporal upscale.
    pub const EDITOR: Self = Self {
        csm_cascade_count: 1,
        csm_resolution: 512,
        local_shadow_atlas_size: 0,
        local_shadow_max_lights: 0,
        msaa_samples: 1,
        render_scale_permille: 1000,
        hdr_target: HdrTarget::Rgba16f,
        shadow_filter: ShadowFilter::Pcf3x3,
        ssao: SsaoMode::Off,
        contact_shadows: false,
        reflection_occlusion: false,
        bent_normals: false,
        max_lights_per_cluster: crate::lab_frame::LAB_MAX_PER_CLUSTER,
        max_reflection_probes: 1,
        gpu_driven: false,
        taau: false,
        horizontal_plus: false,
        physical_atmosphere: false,
        volumetric_fog: false,
        volumetric_clouds: false,
        cloud_reprojection: false,
        cloud_step_percent: 100,
        cloud_cone_light: false,
        cloud_march: CloudMarch::REALTIME,
        // Cheap and stable: no paged shadows, GI, or screen-space passes.
        paged_sun_shadows: false,
        // An orbiting viewport camera must not shimmer the shadows (ADR-1236).
        stable_sun_cascades: true,
        translucent_shadow_cascades: 1,
        translucent_local_shadows: false,
        near_sun_tile: 0,
        gi: GiMode::Ambient,
        screen_space_gi: false,
        gi_probes: false,
        baked_gi: false,
        ssr: false,
        reflection_probes: false,
        max_visible_lights: crate::lab_frame::LAB_MAX_LIGHTS,
        interior_ambient: false,
        shadow_debug: 0,
        parallax_occlusion: false,
        terrain_budget: TerrainBudget::OFF,
        screen_space_refraction: false,
        depth_velocity: false,
        visibility_buffer: false,
        visibility_path: crate::visbuffer::VisibilityPath::DEFAULT,
        sun_shadow_trace: crate::hwrt::RtShadowRequest::Off,
        frame_pipelining: crate::FramePipelining::OFF,
        subsurface_scattering: false,
        blue_noise: false,
        order_independent_transparency: false,
        texture_streaming: TextureStreaming::OFF,
        specular_aa: true,
        smaa: false,
        spatial_upscaler: SpatialUpscaler::Bilinear,
        emissive_lights: 0,
        upscale_quality: None,
        forward_lite: crate::forward_lite::ForwardLite::Off,
        lightmap_tier: crate::lightmap::LightmapTier::Full,
        many_light_sampling: false,
        aerial_from_tables: false,
        horizon_ao: false,
        angular_sun_penumbra: false,
        restir_gi: false,
        subsurface_kernel: crate::subsurface::SubsurfaceKernel::Screen,

        cluster_residency: crate::visbuffer::ClusterResidency::OFF,
    };

    /// The quality the render profile prescribes.
    #[must_use]
    pub fn from_profile(profile: &RenderProfile) -> Self {
        Self {
            csm_cascade_count: profile.csm_cascade_count.clamp(1, crate::forward::CASCADE_COUNT),
            csm_resolution: profile.csm_resolution,
            local_shadow_atlas_size: profile.local_shadow_atlas_size,
            local_shadow_max_lights: profile.local_shadow_max_lights,
            msaa_samples: match profile.anti_aliasing {
                crate::profile::AntiAliasing::Msaa4x => 4,
                _ => 1,
            },
            render_scale_permille: match profile.upscale_quality {
                Some(mode) => mode.permille() as u16,
                None => (profile.render_scale.clamp(0.05, 1.0) * 1000.0).round() as u16,
            },
            hdr_target: profile.hdr_target,
            shadow_filter: profile.shadow_filter,
            ssao: profile.ssao,
            contact_shadows: profile.contact_shadows,
            reflection_occlusion: profile.reflection_occlusion,
            bent_normals: profile.bent_normals,
            max_lights_per_cluster: profile.max_lights_per_cluster.clamp(1, MAX_LIGHTS_PER_CLUSTER),
            max_reflection_probes: profile.max_reflection_probes.max(1),
            gpu_driven: profile.gpu_driven,
            // Temporal AA at the profile scale. Below 1.0 this is TAAU (the opt-in
            // Desktop High 0.67 budget, ADR-041). At 1.0 (Desktop High and Ultra) it resolves 1:1 and still accumulates
            // the per-pixel-rotated PCSS / SSAO / SSGI kernels.
            taau: matches!(profile.anti_aliasing, AntiAliasing::Taa),
            horizontal_plus: profile.horizontal_plus,
            physical_atmosphere: profile.physical_atmosphere,
            volumetric_fog: profile.volumetric_fog,
            volumetric_clouds: profile.volumetric_clouds,
            cloud_reprojection: profile.cloud_reprojection,
            cloud_step_percent: profile.cloud_step_percent,
            cloud_cone_light: profile.cloud_cone_light,
            cloud_march: profile.cloud_march,
            paged_sun_shadows: profile.paged_sun_shadows,
            stable_sun_cascades: true,
            translucent_shadow_cascades: profile.translucent_shadow_cascades,
            translucent_local_shadows: profile.translucent_local_shadows,
            near_sun_tile: profile.near_sun_tile,
            gi: profile.gi,
            screen_space_gi: profile.screen_space_gi,
            gi_probes: profile.gi_probes,
            baked_gi: true,
            ssr: profile.ssr,
            reflection_probes: profile.reflection_probes,
            max_visible_lights: profile
                .max_visible_lights
                .clamp(1, crate::light_budget::MAX_UPLOADED_LIGHTS),
            interior_ambient: false,
            shadow_debug: 0,
            parallax_occlusion: profile.parallax_occlusion,
            terrain_budget: TerrainBudget {
                foliage_subsurface: profile.terrain_budget.foliage_subsurface
                    || profile.subsurface_scattering,
                ..profile.terrain_budget
            },
            screen_space_refraction: profile.screen_space_refraction,
            depth_velocity: profile.depth_velocity,
            visibility_buffer: profile.visibility_buffer,
            visibility_path: profile.visibility_path,
            sun_shadow_trace: profile.sun_shadow_trace,
            frame_pipelining: profile.frame_pipelining,
            subsurface_scattering: profile.subsurface_scattering,
            blue_noise: profile.blue_noise,
            order_independent_transparency: profile.order_independent_transparency,
            texture_streaming: profile.texture_streaming,
            specular_aa: true,
            smaa: matches!(profile.anti_aliasing, AntiAliasing::Smaa),
            spatial_upscaler: profile.spatial_upscaler,
            emissive_lights: profile
                .emissive_lights
                .min(crate::light_budget::MAX_UPLOADED_LIGHTS),
            upscale_quality: profile.upscale_quality,
            forward_lite: profile.forward_lite,
            lightmap_tier: profile.lightmap_tier,
            many_light_sampling: profile.many_light_sampling,
            aerial_from_tables: profile.aerial_from_tables,
            horizon_ao: profile.horizon_ao,
            angular_sun_penumbra: profile.angular_sun_penumbra,
            restir_gi: profile.restir_gi,
            subsurface_kernel: profile.subsurface_kernel,

            cluster_residency: profile.cluster_residency,
        }
    }

    /// Internal render scale (`1.0` = native).
    #[must_use]
    pub fn render_scale(self) -> f32 {
        f32::from(self.render_scale_permille) / 1000.0
    }

    /// Output size scaled by [`Self::render_scale`], at least 1×1. With an
    /// [`Self::upscale_quality`] whose own permille is [`Self::render_scale_permille`], the
    /// mode's exact [`crate::UpscaleQuality::render_size`] instead.
    #[must_use]
    pub fn scaled_extent(self, output_width: u32, output_height: u32) -> (u32, u32) {
        if let Some(mode) = self.upscale_quality
            && mode.permille() == u32::from(self.render_scale_permille)
        {
            return mode.render_size((output_width, output_height));
        }
        let scale = self.render_scale();
        let w = ((output_width as f32) * scale).round().max(1.0) as u32;
        let h = ((output_height as f32) * scale).round().max(1.0) as u32;
        (w, h)
    }

    /// Shader selector for [`Self::shadow_filter`]: 0 hardware 2×2, 1 PCF 3×3, 2 PCF, 3 PCSS-lite,
    /// 4 PCSS-lite with the sun penumbra by the sun's angular size ([`Self::angular_sun_penumbra`],
    /// ADR-3202; the shaders treat it as 3 everywhere else).
    #[must_use]
    pub fn shadow_filter_mode(self) -> u32 {
        match self.shadow_filter {
            ShadowFilter::Hardware2x2 => 0,
            ShadowFilter::Pcf3x3 => 1,
            ShadowFilter::Pcf => 2,
            ShadowFilter::PcssLite if self.angular_sun_penumbra => 4,
            ShadowFilter::PcssLite => 3,
        }
    }

    /// Probe list length after the profile cap (M15b consumes this; see the field docs).
    #[must_use]
    pub fn probe_budget(self, available: u32) -> u32 {
        available.min(self.max_reflection_probes)
    }

    /// The opt-in Desktop High cost cuts: render scale 0.67 and a 2048² cascade.
    ///
    /// Ultra is native with a 4096² map. A lab or m15b [`FrameQuality`] keeps
    /// scale 1 and a 512² map even when it turns SSGI or PCSS-lite on, so it
    /// does not take this path.
    fn desktop_high_budget(self) -> bool {
        self.render_scale_permille == 670 && self.csm_resolution == 2048
    }

    /// Half-resolution SSGI gather plus a joint-bilateral upsample.
    ///
    /// Only the opt-in Desktop High budget (render scale 0.67 and a 2048² cascade)
    /// with SSGI on. Stock Desktop High (native scale) and Ultra stay full resolution. Lab and m15b frames that enable SSGI keep
    /// the full gather their goldens were blessed with.
    #[must_use]
    pub fn ssgi_half_res(self) -> bool {
        self.screen_space_gi && self.desktop_high_budget()
    }

    /// PCSS blocker-search and filter tap counts.
    ///
    /// `(0, 0)` when the filter is not PCSS-lite; the shader then keeps the
    /// historical 12 search and 16 filter taps. Only the opt-in 0.67 Desktop High
    /// budget uses 6 and 8. Stock Desktop High (native scale), Ultra and any other
    /// PCSS-lite quality (including a lab frame) use 12 and 16.
    #[must_use]
    pub fn pcss_taps(self) -> (u32, u32) {
        if self.shadow_filter != ShadowFilter::PcssLite {
            return (0, 0);
        }
        if self.desktop_high_budget() {
            (6, 8)
        } else {
            (12, 16)
        }
    }

    /// Full-resolution linear SSR march and probe specular. When stochastic SSR is capable
    /// (`stochastic_ssr`, Desktop High and Ultra) that pass runs instead, with cutoff 0.55 and no
    /// `normal.y` probe gate; the 0.35/0.7 weights and the 0.72 gate belong to the linear march.
    ///
    /// The opt-in 0.67 Desktop High budget skips that pass (stock native-scale Desktop High keeps it). On a moving 160×90 lavapipe frame it was
    /// the largest screen-space cost (~12 ms) and it does not take the
    /// half-resolution SSGI path. Ultra and every lab or m15b quality keep it,
    /// including ones that turn SSR on. The profile's `ssr` flag stays set.
    #[must_use]
    pub fn full_res_reflections(self) -> bool {
        !self.desktop_high_budget()
    }

    /// Stochastic SSR (the FidelityFX SSSR port in `sssr.rs`) instead of the linear march of
    /// `ssr.wgsl`: Desktop High and Ultra, the presets with SSR and GPU-driven draws. Lab, editor
    /// and probe-capture qualities keep the older pass, so their goldens do not move.
    #[must_use]
    pub fn stochastic_ssr(self) -> bool {
        self.ssr && self.gpu_driven
    }

    /// Reuse the previous SSGI, SSR, and AO results when the unjittered camera,
    /// sun, and draw list are unchanged.
    ///
    /// Only the probe-GI profiles (SSGI together with paged sun shadows) opt in.
    /// Lab and editor frames keep running every pass, so their goldens stay put.
    #[must_use]
    pub fn reuse_static_screen(self) -> bool {
        self.screen_space_gi && self.paged_sun_shadows
    }

    /// A stable name for this quality, used in logs and the agent protocol's
    /// viewport-capture report.
    ///
    /// The editor viewport reports `"editor"`. A quality that matches no named
    /// baseline (for example one built by [`Self::from_profile`] from a changed
    /// preset) reports `"custom"`, so a renderer profile change is visible in
    /// the capture report instead of silently reporting a hardcoded label.
    #[must_use]
    pub fn profile_name(self) -> &'static str {
        if self == Self::EDITOR {
            "editor"
        } else if self == Self::LAB {
            "lab"
        } else {
            "custom"
        }
    }
}

/// Largest `max_lights_per_cluster` in the profile table. The cluster index buffer
/// is allocated for this so a preset change does not rebuild it.
pub const MAX_LIGHTS_PER_CLUSTER: u32 = 256;

impl Default for FrameQuality {
    fn default() -> Self {
        Self::LAB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_anisotropy_per_preset() {
        let level = |p| {
            FrameQuality::from_profile(&RenderProfile::from_preset(p))
                .terrain_budget
                .anisotropy_level()
        };
        assert_eq!(level(RenderProfilePreset::MobileLow), 0);
        assert_eq!(level(RenderProfilePreset::MobileHigh), 2);
        assert_eq!(level(RenderProfilePreset::DesktopLow), 4);
        assert_eq!(level(RenderProfilePreset::Deck), 4);
        assert_eq!(level(RenderProfilePreset::DesktopHigh), 8);
        assert_eq!(level(RenderProfilePreset::DesktopUltra), 16);
        // Off at 0 and 1, clamped to the sampler's 16; the default look has no cuts.
        assert_eq!(TerrainBudget::with_anisotropy(1).anisotropy_level(), 0);
        assert_eq!(TerrainBudget::with_anisotropy(64).anisotropy_level(), 16);
        assert_eq!(TerrainBudget::default().anisotropy_level(), 16);
        assert!(TerrainBudget::with_anisotropy(4).is_off());
    }

    /// The book pages (renderer, high-end-lighting, fog, rendering) state these per-preset facts.
    #[test]
    fn documented_preset_facts_hold() {
        use RenderProfilePreset as P;
        for p in [P::DesktopLow, P::DesktopHigh, P::DesktopUltra, P::Deck] {
            let r = RenderProfile::from_preset(p);
            assert!(r.paged_sun_shadows, "{p:?} pages sun shadows");
            assert!(r.gpu_driven, "{p:?} is GPU driven");
        }
        for p in [P::MobileLow, P::MobileHigh] {
            assert!(!RenderProfile::from_preset(p).paged_sun_shadows, "{p:?}");
        }
        for p in [P::DesktopHigh, P::DesktopUltra] {
            assert!(RenderProfile::from_preset(p).physical_atmosphere, "{p:?} LUT sky");
        }
        let high = RenderProfile::from_preset(P::DesktopHigh);
        assert_eq!(high.texture_streaming, TextureStreaming::DESKTOP_HIGH);
        assert!(high.texture_streaming.eager);
        assert_ne!(high.texture_streaming, TextureStreaming::DESKTOP);
    }

    #[test]
    fn desktop_high_matches_spec_table() {
        let p = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
        assert_eq!(p.csm_cascade_count, 4);
        assert_eq!(p.max_lights_per_cluster, 128);
        assert_eq!(p.max_visible_lights, 256);
        assert!((p.render_scale - 1.0).abs() < f32::EPSILON);
        assert!(p.gpu_driven);
        assert!(p.physical_atmosphere && p.volumetric_fog && p.volumetric_clouds);
        assert_eq!(p.ssao, SsaoMode::GtaoHalfRes);
        assert_eq!(p.shadow_filter, ShadowFilter::PcssLite);
        let q = FrameQuality::from_profile(&p);
        assert_eq!(q.render_scale_permille, 1000, "Desktop High is native (specs/13)");
        assert!(!q.ssgi_half_res());
        assert_eq!(q.pcss_taps(), (12, 16));
        assert!(q.physical_atmosphere && q.volumetric_fog && q.volumetric_clouds);
        assert!(
            q.full_res_reflections(),
            "native Desktop High keeps full-resolution SSR"
        );
        // The opt-in 0.67 performance budget.
        let mut budget = p.clone();
        budget.render_scale = 0.67;
        let b = FrameQuality::from_profile(&budget);
        assert_eq!(b.render_scale_permille, 670);
        assert!(b.taau, "the 0.67 budget is a temporal upscale");
        assert!(b.ssgi_half_res());
        assert_eq!(b.pcss_taps(), (6, 8));
        assert!(
            !b.full_res_reflections(),
            "the 0.67 budget skips full-resolution SSR"
        );
        assert!(q.reuse_static_screen());
        assert!(!FrameQuality::LAB.ssgi_half_res());
        assert_eq!(FrameQuality::LAB.pcss_taps(), (0, 0));
        let lab_features = FrameQuality {
            screen_space_gi: true,
            gi_probes: false,
            shadow_filter: ShadowFilter::PcssLite,
            paged_sun_shadows: true,
            gi: GiMode::Probes,
            ..FrameQuality::LAB
        };
        assert!(
            !lab_features.ssgi_half_res(),
            "a lab frame with SSGI stays full resolution"
        );
        assert_eq!(
            lab_features.pcss_taps(),
            (12, 16),
            "a lab frame with PCSS-lite keeps 12+16 taps"
        );
        assert!(
            lab_features.full_res_reflections(),
            "a lab frame keeps full-resolution SSR"
        );
        assert!(!FrameQuality::LAB.reuse_static_screen());
        assert!(!FrameQuality::EDITOR.reuse_static_screen());
        assert_eq!(q.shadow_filter_mode(), 3);
        assert_eq!(q.max_lights_per_cluster, 128);
        assert_eq!(q.max_visible_lights, 256);
        assert!(!q.interior_ambient);
        assert_eq!(q.shadow_debug, 0);
        assert_eq!(FrameQuality::LAB.shadow_debug, 0);
        assert_eq!(q.probe_budget(8), 4);
        assert!(p.paged_sun_shadows && p.ssr && p.screen_space_gi && p.reflection_probes);
        assert!(p.parallax_occlusion && q.parallax_occlusion);
        const {
            assert!(!FrameQuality::LAB.parallax_occlusion);
        };
        assert_eq!(p.gi, GiMode::Probes);
        assert_eq!(q.gi, GiMode::Probes);
        let ultra = RenderProfile::from_preset(RenderProfilePreset::DesktopUltra);
        assert!(ultra.paged_sun_shadows && ultra.ssr);
        assert_eq!(ultra.gi, GiMode::Probes);
        assert!((ultra.render_scale - 1.0).abs() < f32::EPSILON);
        let ultra_q = FrameQuality::from_profile(&ultra);
        assert!(!ultra_q.ssgi_half_res());
        assert_eq!(ultra_q.pcss_taps(), (12, 16));
        assert!(ultra_q.full_res_reflections());
        assert!(ultra_q.reuse_static_screen());
    }

    #[test]
    fn mobile_tiers_are_spatial_and_the_deck_falls_back_to_smaa_sgsr() {
        let high = RenderProfile::from_preset(RenderProfilePreset::MobileHigh);
        assert_eq!(high.anti_aliasing, AntiAliasing::Smaa);
        assert_eq!(high.spatial_upscaler, SpatialUpscaler::Sgsr);
        let q = FrameQuality::from_profile(&high);
        assert!(q.smaa && !q.taau && q.msaa_samples == 1);
        assert_eq!(q.render_scale_permille, 850);
        assert_eq!(q.spatial_upscaler, SpatialUpscaler::Sgsr);
        let low = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileLow));
        assert!(!low.smaa && low.spatial_upscaler == SpatialUpscaler::Sgsr);
        // Desktop tiers keep their frames: bilinear (Desktop Low) or temporal (High, Ultra).
        for preset in [
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::DesktopHigh,
            RenderProfilePreset::DesktopUltra,
        ] {
            let p = RenderProfile::from_preset(preset);
            assert_eq!(p.spatial_upscaler, SpatialUpscaler::Bilinear, "{preset:?}");
            assert!(!FrameQuality::from_profile(&p).smaa);
        }
        const {
            assert!(!FrameQuality::LAB.smaa && !FrameQuality::EDITOR.smaa);
        }
        // The Deck keeps TAAU; its non-temporal fallback is SMAA upscaled by SGSR.
        let deck = RenderProfile::from_preset(RenderProfilePreset::Deck);
        assert_eq!(deck.anti_aliasing, AntiAliasing::Taa);
        let fallback = FrameQuality::from_profile(&deck.non_temporal());
        assert!(fallback.smaa && !fallback.taau);
        assert_eq!(fallback.spatial_upscaler, SpatialUpscaler::Sgsr);
        assert_eq!(fallback.render_scale_permille, 700);
        // Non-TAA profiles are unchanged by the fallback.
        assert_eq!(high.non_temporal(), high);
    }

    #[test]
    fn deck_profile_is_800p_40fps_with_temporal_upscale() {
        let p = RenderProfile::from_preset(RenderProfilePreset::Deck);
        assert_eq!(p.frame_cap_fps, Some(RenderProfilePreset::DECK_FRAME_CAP_FPS));
        assert!((p.render_scale - 0.7).abs() < 1.0e-4);
        assert_eq!(p.anti_aliasing, AntiAliasing::Taa);
        assert!(p.horizontal_plus);
        // Fixed per-draw / per-cascade work dominates at 1280×800 (look-r2 round 9).
        assert!(
            p.gpu_driven && p.paged_sun_shadows,
            "Deck draws indirect with paged cascades"
        );
        assert!(!p.physical_atmosphere && !p.volumetric_fog && !p.volumetric_clouds);
        let (w, h) = FrameQuality::from_profile(&p).scaled_extent(
            RenderProfilePreset::DECK_OUTPUT_WIDTH,
            RenderProfilePreset::DECK_OUTPUT_HEIGHT,
        );
        assert_eq!((w, h), (896, 560), "70% of 1280×800");
        assert!(FrameQuality::from_profile(&p).taau);
        let mobile = RenderProfile::from_preset(RenderProfilePreset::MobileLow);
        assert!(!mobile.physical_atmosphere && !mobile.volumetric_clouds);
        assert_eq!(mobile.hdr_target, HdrTarget::Rg11B10);
        assert_eq!(mobile.shadow_filter, ShadowFilter::Hardware2x2);
        assert_eq!(FrameQuality::from_profile(&mobile).max_lights_per_cluster, 16);
        assert_eq!(mobile.max_visible_lights, 32);
        assert_eq!(p.max_visible_lights, 96);
        assert!(
            p.max_visible_lights
                < RenderProfile::from_preset(RenderProfilePreset::DesktopHigh).max_visible_lights
        );
        assert_eq!(mobile.gi, GiMode::Ambient);
        assert!(!mobile.ssr && !mobile.screen_space_gi && !mobile.paged_sun_shadows);
        assert!(!p.ssr && !p.screen_space_gi && p.gi == GiMode::Ambient);
        let high = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
        assert!(high.parallax_occlusion);
        assert!(FrameQuality::from_profile(&high).parallax_occlusion);
        const {
            assert!(!FrameQuality::EDITOR.parallax_occlusion);
        };
        let low = RenderProfile::from_preset(RenderProfilePreset::DesktopLow);
        assert_eq!(low.gi, GiMode::Ambient);
        assert!(!low.ssr && !low.reflection_probes);
        assert!(low.gpu_driven && low.paged_sun_shadows);
        assert!(!p.parallax_occlusion && !mobile.parallax_occlusion && !low.parallax_occlusion);
    }

    #[test]
    fn editor_viewport_profile_is_cheap_and_stable() {
        let q = FrameQuality::EDITOR;
        assert!(!q.taau, "no TAAU jitter while editing or paused");
        assert!(!q.gpu_driven, "the editor stays on the CPU draw path");
        assert_eq!(q.render_scale_permille, 1000);
        assert_eq!(q.msaa_samples, 1);
        assert_eq!(q.ssao, SsaoMode::Off);
        assert!(!q.physical_atmosphere && !q.volumetric_fog && !q.volumetric_clouds);
        assert!(
            !q.horizontal_plus,
            "the editor camera's vertical FOV is the gizmo's"
        );
        // Deck is the preset that jitters; the editor profile must not inherit it.
        assert!(FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::Deck)).taau);
    }

    #[test]
    fn profile_name_tracks_the_quality_not_a_hardcoded_label() {
        // The editor capture reports these two fields; they must be derived from
        // the quality the viewport renders with, so a renderer change shows up.
        let editor = FrameQuality::EDITOR;
        assert_eq!(editor.profile_name(), "editor");
        assert!(!editor.taau, "editing must not jitter");
        // The "profile changed" failure mode: a Deck-like quality jitters and is
        // no longer reported as "editor" — the capture check must then fail.
        let deck = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::Deck));
        assert!(deck.taau, "Deck turns on temporal upscale");
        assert_ne!(deck.profile_name(), "editor");
    }

    #[test]
    fn preset_names_round_trip() {
        for preset in RenderProfilePreset::ALL {
            assert_eq!(RenderProfilePreset::from_name(preset.name()), Some(preset));
        }
        assert_eq!(
            RenderProfilePreset::from_name("mobile_low"),
            Some(RenderProfilePreset::MobileLow)
        );
        assert_eq!(
            RenderProfilePreset::from_name("Desktop-High"),
            Some(RenderProfilePreset::DesktopHigh)
        );
        assert_eq!(RenderProfilePreset::from_name("phone"), None);
        // Desktop test hosts keep Desktop High; Android builds get Mobile High.
        let expected = if cfg!(target_os = "android") {
            RenderProfilePreset::MobileHigh
        } else {
            RenderProfilePreset::DesktopHigh
        };
        assert_eq!(RenderProfilePreset::platform_default(), expected);
    }

    #[test]
    fn runtime_default_is_deck_on_a_deck_and_the_platform_default_elsewhere() {
        // ADR-3289: off a Deck the runtime default is the platform default; on one it is Deck
        // (an Android build never reports a Deck).
        assert_eq!(
            RenderProfilePreset::runtime_default_for(false),
            RenderProfilePreset::platform_default()
        );
        let on_deck = if cfg!(target_os = "android") {
            RenderProfilePreset::MobileHigh
        } else {
            RenderProfilePreset::Deck
        };
        assert_eq!(RenderProfilePreset::runtime_default_for(true), on_deck);
        assert!(is_steam_deck_model("Valve Jupiter"));
        assert!(is_steam_deck_model("Valve Galileo"));
        assert!(is_steam_deck_model("  Valve   Galileo\n"));
        for other in [
            "",
            "Valve",
            "Jupiter",
            "Galileo",
            "valve galileo",
            "Valve Galileo 2",
            "Valve Fremont",
            "ASUS ROG Ally",
            "LENOVO 83E1",
        ] {
            assert!(!is_steam_deck_model(other), "{other:?}");
        }
    }

    #[test]
    fn mesh_lod_tier_scales_per_preset() {
        let scales = |preset| {
            let profile = RenderProfile::from_preset(preset);
            (profile.mesh_lod_distance_scale, profile.mesh_lod_shadow_scale)
        };
        // The lower tiers select exactly as before the tier scales existed (1, and the old 0.5
        // shadow factor); High and Ultra keep colour detail longer and drop shadow detail earlier.
        for preset in [
            RenderProfilePreset::MobileLow,
            RenderProfilePreset::MobileHigh,
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::Deck,
        ] {
            assert_eq!(scales(preset), (1.0, 0.5), "{preset:?}");
        }
        assert_eq!(scales(RenderProfilePreset::DesktopHigh), (2.0, 0.35));
        assert_eq!(scales(RenderProfilePreset::DesktopUltra), (6.0, 0.15));
        for preset in RenderProfilePreset::ALL {
            let (distance, shadow) = scales(preset);
            let bias = RenderProfile::from_preset(preset).mesh_lod_bias;
            let (lo, hi) = crate::MESH_LOD_DISTANCE_SCALE_RANGE;
            assert!((lo..=hi).contains(&(bias * distance)), "{preset:?}");
            let (lo, hi) = crate::MESH_LOD_SHADOW_SCALE_RANGE;
            assert!((lo..=hi).contains(&shadow), "{preset:?}");
        }
    }

    /// The customer rule (ADR-2035): no preset and no baseline traces the sun; a profile opts in.
    #[test]
    fn subsurface_scattering_is_off_on_every_preset_and_baseline() {
        for preset in RenderProfilePreset::ALL {
            let profile = RenderProfile::from_preset(preset);
            assert!(!profile.subsurface_scattering, "{preset:?}");
            assert!(
                !FrameQuality::from_profile(&profile).subsurface_scattering,
                "{preset:?}"
            );
        }
        assert!(!RenderProfile::default().subsurface_scattering);
        for baseline in [FrameQuality::LAB, FrameQuality::EDITOR] {
            assert!(!baseline.subsurface_scattering);
        }
        let opted = RenderProfile {
            subsurface_scattering: true,
            ..RenderProfile::default()
        };
        assert!(FrameQuality::from_profile(&opted).subsurface_scattering);
    }

    /// No preset and no baseline turns on order-independent transparency; a profile opts in.
    #[test]
    fn order_independent_transparency_is_off_on_every_preset_and_baseline() {
        for preset in RenderProfilePreset::ALL {
            let profile = RenderProfile::from_preset(preset);
            assert!(!profile.order_independent_transparency, "{preset:?}");
            assert!(
                !FrameQuality::from_profile(&profile).order_independent_transparency,
                "{preset:?}"
            );
        }
        assert!(!RenderProfile::default().order_independent_transparency);
        for baseline in [FrameQuality::LAB, FrameQuality::EDITOR] {
            assert!(!baseline.order_independent_transparency);
        }
        let opted = RenderProfile {
            order_independent_transparency: true,
            ..RenderProfile::default()
        };
        assert!(FrameQuality::from_profile(&opted).order_independent_transparency);
    }

    #[test]
    fn blue_noise_is_off_on_every_preset_and_baseline() {
        for preset in RenderProfilePreset::ALL {
            let profile = RenderProfile::from_preset(preset);
            assert!(!profile.blue_noise, "{preset:?}");
            assert!(!FrameQuality::from_profile(&profile).blue_noise, "{preset:?}");
        }
        const { assert!(!FrameQuality::LAB.blue_noise && !FrameQuality::EDITOR.blue_noise) };
        let opted = RenderProfile {
            blue_noise: true,
            ..RenderProfile::default()
        };
        assert!(FrameQuality::from_profile(&opted).blue_noise);
    }

    #[test]
    fn many_light_sampling_is_off_on_every_preset_and_baseline() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let profile = RenderProfile::from_preset(preset);
            assert!(!profile.many_light_sampling, "{preset:?}");
            assert!(
                !FrameQuality::from_profile(&profile).many_light_sampling,
                "{preset:?}"
            );
        }
        const { assert!(!FrameQuality::LAB.many_light_sampling && !FrameQuality::EDITOR.many_light_sampling) };
        let opted = RenderProfile {
            many_light_sampling: true,
            ..RenderProfile::from_preset(RenderProfilePreset::DesktopUltra)
        };
        assert!(FrameQuality::from_profile(&opted).many_light_sampling);
    }

    #[test]
    fn restir_gi_is_off_on_every_preset_and_runtime_opt_in_keeps_preset_identity() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let profile = RenderProfile::from_preset(preset);
            assert!(!profile.restir_gi, "{preset:?}");
            assert!(!FrameQuality::from_profile(&profile).restir_gi, "{preset:?}");
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile.restir_gi = true;
            assert_eq!(settings.preset(), Some(preset), "{preset:?}");
        }
        const { assert!(!FrameQuality::LAB.restir_gi && !FrameQuality::EDITOR.restir_gi) };
        let opted = RenderProfile {
            restir_gi: true,
            ..RenderProfile::from_preset(RenderProfilePreset::DesktopUltra)
        };
        assert!(FrameQuality::from_profile(&opted).restir_gi);
    }

    #[test]
    fn sun_shadow_trace_is_off_on_every_preset_and_baseline() {
        use crate::hwrt::RtShadowRequest;
        for preset in RenderProfilePreset::ALL {
            let profile = RenderProfile::from_preset(preset);
            assert_eq!(profile.sun_shadow_trace, RtShadowRequest::Off, "{preset:?}");
            assert_eq!(
                FrameQuality::from_profile(&profile).sun_shadow_trace,
                RtShadowRequest::Off,
                "{preset:?}"
            );
        }
        assert_eq!(RenderProfile::default().sun_shadow_trace, RtShadowRequest::Off);
        assert_eq!(FrameQuality::LAB.sun_shadow_trace, RtShadowRequest::Off);
        assert_eq!(FrameQuality::EDITOR.sun_shadow_trace, RtShadowRequest::Off);
        let opted = RenderProfile {
            sun_shadow_trace: RtShadowRequest::On,
            ..RenderProfile::default()
        };
        assert_eq!(
            FrameQuality::from_profile(&opted).sun_shadow_trace,
            RtShadowRequest::On
        );
    }

    #[test]
    fn contact_shadows_on_every_desktop_tier_and_off_on_mobile_and_baselines() {
        use RenderProfilePreset as P;
        for preset in [P::DesktopLow, P::DesktopHigh, P::DesktopUltra, P::Deck] {
            let p = RenderProfile::from_preset(preset);
            assert!(p.contact_shadows && p.depth_prepass, "{preset:?}");
            assert!(FrameQuality::from_profile(&p).contact_shadows, "{preset:?}");
        }
        for preset in [P::MobileLow, P::MobileHigh] {
            assert!(!RenderProfile::from_preset(preset).contact_shadows, "{preset:?}");
        }
        const { assert!(!FrameQuality::LAB.contact_shadows && !FrameQuality::EDITOR.contact_shadows) };
    }

    #[test]
    fn reflection_occlusion_only_with_the_gtao_rows() {
        use RenderProfilePreset as P;
        for preset in [P::DesktopHigh, P::DesktopUltra] {
            let p = RenderProfile::from_preset(preset);
            assert!(p.reflection_occlusion, "{preset:?}");
            assert!(
                matches!(p.ssao, SsaoMode::GtaoHalfRes | SsaoMode::GtaoFull),
                "{preset:?}"
            );
            assert!(FrameQuality::from_profile(&p).reflection_occlusion, "{preset:?}");
        }
        // Mobile and the half-res AO row keep the AO-based specular occlusion alone.
        for preset in [P::MobileLow, P::MobileHigh, P::DesktopLow, P::Deck] {
            assert!(
                !RenderProfile::from_preset(preset).reflection_occlusion,
                "{preset:?}"
            );
        }
        const { assert!(!FrameQuality::LAB.reflection_occlusion && !FrameQuality::EDITOR.reflection_occlusion) };
    }

    #[test]
    fn bent_normals_only_with_the_gtao_rows() {
        use RenderProfilePreset as P;
        for preset in [P::DesktopHigh, P::DesktopUltra] {
            let p = RenderProfile::from_preset(preset);
            assert!(p.bent_normals, "{preset:?}");
            assert!(
                matches!(p.ssao, SsaoMode::GtaoHalfRes | SsaoMode::GtaoFull),
                "{preset:?}"
            );
            assert!(FrameQuality::from_profile(&p).bent_normals, "{preset:?}");
        }
        // Low and mid tiers keep the normal and the AO-based specular occlusion.
        for preset in [P::MobileLow, P::MobileHigh, P::DesktopLow, P::Deck] {
            assert!(!RenderProfile::from_preset(preset).bent_normals, "{preset:?}");
        }
        assert!(!crate::quality::QualityTier::Medium.profile().bent_normals);
        const { assert!(!FrameQuality::LAB.bent_normals && !FrameQuality::EDITOR.bent_normals) };
    }

    #[test]
    fn no_preset_sets_a_quality_mode_so_render_sizes_are_unchanged() {
        for preset in RenderProfilePreset::ALL {
            let profile = RenderProfile::from_preset(preset);
            assert_eq!(profile.upscale_quality, None, "{preset:?}");
            let quality = FrameQuality::from_profile(&profile);
            assert_eq!(quality.upscale_quality, None, "{preset:?}");
            let permille = (profile.render_scale.clamp(0.05, 1.0) * 1000.0).round() as u16;
            assert_eq!(quality.render_scale_permille, permille, "{preset:?}");
            for display in [(1280, 800), (1920, 1080), (2560, 1440), (3840, 2160)] {
                let scale = f32::from(permille) / 1000.0;
                let old = (
                    ((display.0 as f32) * scale).round().max(1.0) as u32,
                    ((display.1 as f32) * scale).round().max(1.0) as u32,
                );
                assert_eq!(quality.scaled_extent(display.0, display.1), old, "{preset:?}");
            }
        }
        assert_eq!(FrameQuality::LAB.upscale_quality, None);
        assert_eq!(FrameQuality::EDITOR.upscale_quality, None);
    }

    #[test]
    fn a_quality_mode_renders_at_its_exact_size() {
        use crate::UpscaleQuality as Q;
        let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
        profile.upscale_quality = Some(Q::Quality);
        assert_eq!(profile.scaled_extent(2560, 1440), (1707, 960));
        let quality = FrameQuality::from_profile(&profile);
        assert_eq!(quality.render_scale_permille, 667);
        assert_eq!(quality.upscale_quality, Some(Q::Quality));
        // Through 667 permille alone 4K would be 2561×1441.
        assert_eq!(quality.scaled_extent(3840, 2160), (2560, 1440));
        assert_eq!(quality.scaled_extent(2560, 1440), (1707, 960));
        for mode in Q::ALL {
            profile.upscale_quality = Some(mode);
            let quality = FrameQuality::from_profile(&profile);
            assert_eq!(
                quality.scaled_extent(3840, 2160),
                mode.render_size((3840, 2160)),
                "{mode:?}"
            );
        }
        // Another permille (dynamic resolution) scales as before; 1000 (a texture view or a
        // probe capture) is native.
        profile.upscale_quality = Some(Q::Quality);
        let mut stepped = FrameQuality::from_profile(&profile);
        stepped.render_scale_permille = 617;
        assert_eq!(stepped.scaled_extent(2560, 1440), (1580, 888));
        stepped.render_scale_permille = 1000;
        assert_eq!(stepped.scaled_extent(2560, 1440), (2560, 1440));
        // The mode makes the profile custom.
        assert_ne!(
            profile,
            RenderProfile::from_preset(RenderProfilePreset::DesktopHigh)
        );
    }

    #[test]
    fn dynamic_resolution_under_a_mode_stays_in_its_range() {
        use crate::{DynResConfig, DynResController, UpscaleQuality as Q};
        let config = DynResConfig {
            window_frames: 1,
            cooldown_frames: 0,
            ..DynResConfig::default()
        }
        .within_quality(Q::Quality);
        assert_eq!((config.min_permille, config.max_permille), (588, 667));
        let mut controller = DynResController::new(config, 1000);
        assert_eq!(controller.permille(), 667, "the start is clamped to the mode");
        let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
        profile.upscale_quality = Some(Q::Quality);
        let mut quality = FrameQuality::from_profile(&profile);
        let mut sizes = vec![quality.scaled_extent(2560, 1440)];
        // GPU-bound overload (40 of 42 ms): 667 → 617 → 588, then the floor holds.
        for _ in 0..4 {
            quality.render_scale_permille = controller.update(40.0, 42.0) as u16;
            sizes.push(quality.scaled_extent(2560, 1440));
        }
        assert_eq!(
            sizes,
            vec![(1707, 960), (1580, 888), (1505, 847), (1505, 847), (1505, 847)]
        );
        // Light load climbs back to the mode's exact size and no further.
        for _ in 0..8 {
            quality.render_scale_permille = controller.update(4.0, 40.0) as u16;
        }
        assert_eq!(quality.render_scale_permille, 667);
        assert_eq!(quality.scaled_extent(2560, 1440), (1707, 960));
    }

    /// ADR-3202: the contact calibration switches are off on every preset and baseline, the
    /// angular penumbra rides on the PCSS filter mode (4, only with PCSS-lite), and turning
    /// either on at runtime leaves the preset recognised.
    #[test]
    fn contact_calibration_is_opt_in_and_keeps_the_preset() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let profile = RenderProfile::from_preset(preset);
            assert!(!profile.horizon_ao && !profile.angular_sun_penumbra, "{preset:?}");
            let q = FrameQuality::from_profile(&profile);
            assert!(!q.horizon_ao && !q.angular_sun_penumbra, "{preset:?}");
            assert!(q.shadow_filter_mode() <= 3, "{preset:?}");
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile.horizon_ao = true;
            settings.profile.angular_sun_penumbra = true;
            assert_eq!(settings.preset(), Some(preset), "{preset:?}");
            let opted = FrameQuality::from_profile(&settings.profile);
            assert!(opted.horizon_ao && opted.angular_sun_penumbra);
            let want = if opted.shadow_filter == ShadowFilter::PcssLite {
                4
            } else {
                q.shadow_filter_mode()
            };
            assert_eq!(opted.shadow_filter_mode(), want, "{preset:?}");
        }
        const { assert!(!FrameQuality::LAB.horizon_ao && !FrameQuality::EDITOR.horizon_ao) };
        const { assert!(!FrameQuality::LAB.angular_sun_penumbra && !FrameQuality::EDITOR.angular_sun_penumbra) };
    }

    /// ADR-3170: the screen kernel and no grass transmission in every preset and baseline; opting
    /// in to either keeps the preset other systems take their tier from.
    #[test]
    fn subsurface_kernel_and_foliage_transmission_are_opt_in() {
        use crate::subsurface::SubsurfaceKernel;
        for preset in RenderProfilePreset::ALL {
            let profile = RenderProfile::from_preset(preset);
            assert_eq!(profile.subsurface_kernel, SubsurfaceKernel::Screen, "{preset:?}");
            let quality = FrameQuality::from_profile(&profile);
            assert_eq!(quality.subsurface_kernel, SubsurfaceKernel::Screen, "{preset:?}");
            assert!(!quality.terrain_budget.foliage_subsurface, "{preset:?}");
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile.subsurface_kernel = SubsurfaceKernel::Tangent;
            settings.profile.subsurface_scattering = true;
            settings.profile.terrain_budget.foliage_subsurface = true;
            assert_eq!(settings.preset(), Some(preset), "{preset:?} keeps its preset");
            let opted = settings.quality();
            assert_eq!(opted.subsurface_kernel, SubsurfaceKernel::Tangent);
            assert!(opted.terrain_budget.foliage_subsurface);
        }
        for baseline in [FrameQuality::LAB, FrameQuality::EDITOR] {
            assert_eq!(baseline.subsurface_kernel, SubsurfaceKernel::Screen);
            assert!(!baseline.terrain_budget.foliage_subsurface);
        }
        // The grass follows the scattering switch.
        let scatter = RenderProfile {
            subsurface_scattering: true,
            ..RenderProfile::default()
        };
        assert!(
            FrameQuality::from_profile(&scatter)
                .terrain_budget
                .foliage_subsurface
        );
        assert!(
            TerrainBudget {
                foliage_subsurface: true,
                ..TerrainBudget::OFF
            }
            .is_off()
        );
    }

    /// ADR-3170 (MSAA stays a fallback of the subsurface pass): no preset renders multisampled, so
    /// the `msaa` reason is reached only by a host's own choice, and the reason is a stable code.
    #[test]
    fn no_preset_is_multisampled_so_subsurface_never_falls_back_to_msaa_on_one() {
        for preset in RenderProfilePreset::ALL {
            let quality = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
            assert_eq!(quality.msaa_samples, 1, "{preset:?}");
        }
        assert!(crate::subsurface::SUBSURFACE_FRAME_REASONS.contains(&"msaa"));
        let msaa = RenderProfile {
            anti_aliasing: AntiAliasing::Msaa4x,
            ..RenderProfile::default()
        };
        assert_eq!(FrameQuality::from_profile(&msaa).msaa_samples, 4);
    }

    /// ADR-3118: residency is off on every preset and baseline; a profile opts in and the frame
    /// quality carries it.
    #[test]
    fn cluster_residency_is_off_on_every_preset_and_baseline() {
        use crate::visbuffer::ClusterResidency;
        for preset in RenderProfilePreset::ALL {
            let profile = RenderProfile::from_preset(preset);
            assert_eq!(profile.cluster_residency, ClusterResidency::OFF, "{preset:?}");
            assert_eq!(
                FrameQuality::from_profile(&profile).cluster_residency,
                ClusterResidency::OFF
            );
        }
        assert_eq!(FrameQuality::LAB.cluster_residency, ClusterResidency::OFF);
        assert_eq!(FrameQuality::EDITOR.cluster_residency, ClusterResidency::OFF);
        let opted = RenderProfile {
            cluster_residency: ClusterResidency::for_preset(RenderProfilePreset::Deck),
            ..RenderProfile::from_preset(RenderProfilePreset::Deck)
        };
        assert!(FrameQuality::from_profile(&opted).cluster_residency.enabled);
    }

    /// ADR-2632: every preset ships the legacy BRDF energy model, and opting in to the compensated
    /// one keeps the preset other systems take their tier from.
    #[test]
    fn brdf_energy_is_legacy_on_every_preset_and_opting_in_keeps_the_preset() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            assert_eq!(
                RenderProfile::from_preset(preset).brdf_energy,
                crate::BrdfEnergy::Legacy,
                "{preset:?}"
            );
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile.brdf_energy = crate::BrdfEnergy::Compensated;
            assert_eq!(settings.preset(), Some(preset), "{preset:?}");
        }
        assert_eq!(RenderProfile::default().brdf_energy, crate::BrdfEnergy::Legacy);
    }

    #[test]
    fn forward_decals_are_off_on_every_preset_and_opting_in_keeps_the_preset() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            assert!(!RenderProfile::from_preset(preset).forward_decals, "{preset:?}");
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile.forward_decals = true;
            assert_eq!(settings.preset(), Some(preset), "{preset:?}");
        }
        assert!(!RenderProfile::default().forward_decals);
    }

    /// ADR-3190: no preset lights the sun's specular as a disc, and opting in keeps the preset
    /// other systems take their tier from.
    #[test]
    fn sun_specular_disc_is_off_on_every_preset_and_opting_in_keeps_the_preset() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            assert!(
                !RenderProfile::from_preset(preset).sun_specular_disc,
                "{preset:?}"
            );
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile.sun_specular_disc = true;
            assert_eq!(settings.preset(), Some(preset), "{preset:?}");
        }
        assert!(!RenderProfile::default().sun_specular_disc);
    }

    /// ADR-3247: no preset turns the rect and tube fallback on, and opting in keeps the preset.
    #[test]
    fn area_light_fallback_is_off_on_every_preset_and_opting_in_keeps_the_preset() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            assert!(
                !RenderProfile::from_preset(preset).area_light_fallback,
                "{preset:?}"
            );
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile.area_light_fallback = true;
            assert_eq!(settings.preset(), Some(preset), "{preset:?}");
        }
        assert!(!RenderProfile::default().area_light_fallback);
    }

    /// ADR-3247: no preset shades every sized light through the LTC tables, and opting in keeps the preset.
    #[test]
    fn sized_light_ltc_is_off_on_every_preset_and_opting_in_keeps_the_preset() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            assert!(!RenderProfile::from_preset(preset).sized_light_ltc, "{preset:?}");
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile.sized_light_ltc = true;
            assert_eq!(settings.preset(), Some(preset), "{preset:?}");
        }
        assert!(!RenderProfile::default().sized_light_ltc);
    }

    /// ADR-7891: every row carries its tier's host memory budget, and a host that sets its own
    /// (a tighter cap, or none) keeps its preset (`profile-field-breaks-preset`).
    #[test]
    fn a_host_memory_budget_keeps_the_preset() {
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let row = RenderProfile::from_preset(preset);
            assert_eq!(
                row.host_memory,
                crate::host_memory::HostMemoryBudget::for_preset(preset),
                "{preset:?}"
            );
            for budget in [
                crate::host_memory::HostMemoryBudget::UNLIMITED,
                crate::host_memory::HostMemoryBudget {
                    cpu_mesh_bytes: 64 << 20,
                    cpu_texture_bytes: 1,
                },
            ] {
                let mut settings = crate::RenderSettings::from_profile(preset);
                settings.profile.host_memory = budget;
                assert_eq!(settings.preset(), Some(preset), "{preset:?} {budget:?}");
            }
        }
        assert_eq!(
            RenderProfile::default().host_memory,
            crate::host_memory::HostMemoryBudget::for_preset(RenderProfilePreset::DesktopHigh)
        );
    }
}
