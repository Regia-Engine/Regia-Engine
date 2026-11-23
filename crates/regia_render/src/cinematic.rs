// SPDX-License-Identifier: MPL-2.0
//! The Cinematic offline profile (ADR-1403): a render profile above Desktop Ultra for offline
//! output (`render.out`, stills, trailers), where frame time does not matter and every frame has
//! to stand alone.
//!
//! What it changes against Desktop Ultra ([`cinematic_profile`]):
//! - Clouds: no reprojection (the whole layer is marched every frame), 300 % view-march steps
//!   (64 → 192, cap 256), the march at full resolution, and [`CloudMarch::samples`] stratified
//!   marches per texel averaged in linear light, so cloud edges and thin gaps carry no start-jitter
//!   dither without any temporal resolve. The result depends only on the frame index, so two runs
//!   of the same sequence hash the same.
//! - Shadows: 8192² sun cascades (Ultra 4096²).
//! - Lights: the uploaded light cap at its maximum ([`crate::MAX_UPLOADED_LIGHTS`]).
//! - Mesh LOD: the colour LOD held to the distance-scale ceiling (bias 2 × scale 8 = 16) and the
//!   shadow LOD chosen like the colour LOD (shadow scale 1).
//! - Terrain anisotropy 16 (as Ultra).
//!
//! The profile is tier-gated ([`gate_preset`]): Android, the Steam Deck and the Low, Medium and
//! mobile tiers get their own preset instead (the cheap fallback); High and Ultra GPUs run it.
//! Hosts read the requested preset from the command line, the environment or a scene directory
//! with [`take_preset_arg`] and [`choose_preset`]; [`take_profile_arg`] and [`choose_profile`] do
//! the same and also accept the quality-ladder tiers that have no preset row (`VeryLow`,
//! `Medium`: [`ProfileName`], ADR-7977).

use std::fmt;
use std::path::{Path, PathBuf};

use crate::quality::{ProfileName, QualityTier};
use crate::{RenderProfile, RenderProfilePreset};

/// How the volumetric clouds' view march runs per frame: marches per texel, the step ceiling
/// and the largest resolution divisor. [`CloudMarch::REALTIME`] on every real-time preset and
/// the lab and editor baselines (frames unchanged); [`CloudMarch::CINEMATIC`] on the Cinematic
/// profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CloudMarch {
    /// Marches averaged per low-resolution texel. March `k` of `n` starts in jitter stratum
    /// `(ign + k) / n` (one per-texel interleaved-gradient offset shared by all strata) and
    /// casts its ray through point `k` of the R2 sequence inside the texel (the centre for
    /// `k = 0`). `1` is the single centred march of the real-time presets. Clamped to
    /// `1..=`[`Self::MAX_SAMPLES`].
    pub samples: u32,
    /// Upper bound of the view-march steps after the profile's step percent. `96` (the v0.1
    /// loop bound) on every real-time preset; clamped to `1..=`[`Self::STEP_LIMIT`].
    pub max_steps: u32,
    /// Largest resolution divisor the march may run at: the layer's
    /// `CloudSettings::resolution_divisor` is clamped to `1..=max_divisor`. `8` (the layer's own
    /// divisor) on every real-time preset, `1` (full resolution) on Cinematic.
    pub max_divisor: u32,
}

impl CloudMarch {
    /// Most marches per texel (the shader's sample loop bound).
    pub const MAX_SAMPLES: u32 = 16;
    /// Most view-march steps (the shader's step loop bound).
    pub const STEP_LIMIT: u32 = 256;
    /// The v0.1 step ceiling every real-time preset keeps.
    pub const REALTIME_MAX_STEPS: u32 = 96;
    /// Largest divisor `CloudSettings::resolution_divisor` may take.
    pub const DIVISOR_LIMIT: u32 = 8;

    /// One centred march per texel, at most 96 steps, at the layer's divisor: the real-time
    /// presets' march, unchanged.
    pub const REALTIME: Self = Self {
        samples: 1,
        max_steps: Self::REALTIME_MAX_STEPS,
        max_divisor: Self::DIVISOR_LIMIT,
    };

    /// The Cinematic march: four stratified marches per texel, up to 256 steps, full resolution.
    /// Four strata cut the start-jitter error of one march to a quarter of a step; with 192
    /// steps at full resolution that is below one 8-bit code value on a 4K cloud base (measured
    /// in the lane report, ADR-1403).
    pub const CINEMATIC: Self = Self {
        samples: 4,
        max_steps: Self::STEP_LIMIT,
        max_divisor: 1,
    };

    /// [`Self::samples`] clamped to `1..=`[`Self::MAX_SAMPLES`].
    #[must_use]
    pub const fn sample_count(self) -> u32 {
        if self.samples < 1 {
            1
        } else if self.samples > Self::MAX_SAMPLES {
            Self::MAX_SAMPLES
        } else {
            self.samples
        }
    }

    /// View-march steps for a layer of `layer_steps` at `percent` of them:
    /// `layer_steps × percent / 100`, clamped to `1..=max_steps` (itself at most
    /// [`Self::STEP_LIMIT`]).
    #[must_use]
    pub fn steps(self, layer_steps: u32, percent: u32) -> u32 {
        let cap = self.max_steps.clamp(1, Self::STEP_LIMIT);
        let scaled = u64::from(layer_steps) * u64::from(percent.max(1)) / 100;
        scaled.clamp(1, u64::from(cap)) as u32
    }

    /// The divisor the march runs at for a layer asking for `layer_divisor`.
    #[must_use]
    pub fn divisor(self, layer_divisor: u32) -> u32 {
        let cap = self.max_divisor.clamp(1, Self::DIVISOR_LIMIT);
        layer_divisor.clamp(1, cap)
    }

    /// Start jitter of march `k` of `n` for a texel whose interleaved-gradient value is `base`
    /// (`0..1`): `(base + k) / n`, one sample in each of `n` equal strata. The CPU mirror of the
    /// shader's `march_texel`.
    #[must_use]
    pub fn stratum_jitter(base: f32, k: u32, n: u32) -> f32 {
        let n = n.max(1);
        let base = base.clamp(0.0, 1.0 - f32::EPSILON);
        (base + k.min(n - 1) as f32) / n as f32
    }

    /// Sub-texel ray position of march `k`: point `k` of the R2 sequence (Roberts, "The
    /// Unreasonable Effectiveness of Quasirandom Sequences", 2018) started at the texel centre,
    /// in `0..1` texel units. The CPU mirror of the shader's `r2_offset`.
    #[must_use]
    pub fn sub_texel(k: u32) -> [f32; 2] {
        const A1: f32 = 0.754_877_7;
        const A2: f32 = 0.569_840_3;
        let k = k as f32;
        [(0.5 + k * A1).fract(), (0.5 + k * A2).fract()]
    }
}

impl Default for CloudMarch {
    fn default() -> Self {
        Self::REALTIME
    }
}

/// Sun cascade edge of the Cinematic profile: twice Desktop Ultra's, at the 8192 texel edge every
/// desktop device can create (wgpu's default `max_texture_dimension_2d`).
pub const CINEMATIC_CSM_RESOLUTION: u32 = 8192;

/// Cloud view-march step percent of the Cinematic profile (64 → 192 steps).
pub const CINEMATIC_CLOUD_STEP_PERCENT: u32 = 300;

/// The Cinematic table row: Desktop Ultra with the offline changes listed in the module docs.
/// [`RenderProfile::from_preset`] returns this for [`RenderProfilePreset::Cinematic`].
#[must_use]
pub fn cinematic_profile() -> RenderProfile {
    let ultra = RenderProfile::from_preset(RenderProfilePreset::DesktopUltra);
    RenderProfile {
        render_scale: 1.0,
        csm_resolution: CINEMATIC_CSM_RESOLUTION,
        // The near sun tile stays at Ultra's 4096: it spans 4 m (`NEAR_SUN_RANGE_M`, ~1 mm
        // texels already), and an 8192 tile would take the whole 8192² local atlas from the
        // local lights' shadows (`shadow::layout_shadow_tiles_reserved`).
        max_lights_per_cluster: crate::profile::MAX_LIGHTS_PER_CLUSTER,
        max_visible_lights: crate::light_budget::MAX_UPLOADED_LIGHTS,
        frame_cap_fps: None,
        cloud_reprojection: false,
        cloud_step_percent: CINEMATIC_CLOUD_STEP_PERCENT,
        cloud_march: CloudMarch::CINEMATIC,
        terrain_budget: crate::TerrainBudget::with_anisotropy(crate::TerrainBudget::MAX_ANISOTROPY),
        upscale_quality: None,
        mesh_lod_bias: 2.0,
        mesh_lod_distance_scale: 8.0,
        mesh_lod_shadow_scale: 1.0,
        texture_streaming: crate::TextureStreaming::for_preset(RenderProfilePreset::Cinematic),
        ..ultra
    }
}

/// The lowest detected tier that runs the Cinematic profile.
pub const CINEMATIC_MIN_TIER: QualityTier = QualityTier::High;

/// The outcome of [`gate_preset`]: what was asked, what runs, and why they differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetGate {
    /// The preset the host or agent asked for.
    pub requested: RenderProfilePreset,
    /// The preset to apply.
    pub applied: RenderProfilePreset,
    /// Stable lowercase code when [`Self::applied`] differs from [`Self::requested`]:
    /// `cinematic_needs_desktop` (Android) or `cinematic_tier_too_low` (a tier below High).
    pub reason: Option<&'static str>,
}

impl PresetGate {
    /// Whether the requested preset runs as asked.
    #[must_use]
    pub fn honoured(&self) -> bool {
        self.requested == self.applied
    }
}

/// Gates a requested preset by platform and detected tier. Only
/// [`RenderProfilePreset::Cinematic`] is gated; every other preset applies as asked.
///
/// - On Android Cinematic becomes Mobile High (`cinematic_needs_desktop`), even with `force`.
/// - With a detected `tier` below [`CINEMATIC_MIN_TIER`] (Deck, Low, Medium, the mobile tiers)
///   Cinematic becomes that tier's own preset (`cinematic_tier_too_low`) unless `force`.
/// - Without a detected tier (a headless tool, a test world) Cinematic runs.
#[must_use]
pub fn gate_preset(
    requested: RenderProfilePreset,
    tier: Option<QualityTier>,
    android: bool,
    force: bool,
) -> PresetGate {
    let keep = PresetGate {
        requested,
        applied: requested,
        reason: None,
    };
    if requested != RenderProfilePreset::Cinematic {
        return keep;
    }
    if android {
        return PresetGate {
            requested,
            applied: RenderProfilePreset::MobileHigh,
            reason: Some("cinematic_needs_desktop"),
        };
    }
    match tier {
        Some(tier) if !force && !matches!(tier, QualityTier::High | QualityTier::Ultra) => PresetGate {
            requested,
            applied: tier.for_platform(false).preset(),
            reason: Some("cinematic_tier_too_low"),
        },
        _ => keep,
    }
}

/// The command-line flag a host reads its preset from: `--render-profile Cinematic` or
/// `--render-profile=cinematic`.
pub const RENDER_PROFILE_FLAG: &str = "--render-profile";

/// The environment variable a host reads its preset from when the command line names none.
pub const RENDER_PROFILE_ENV: &str = "REGIA_RENDER_PROFILE";

/// The file in a scene directory that names its preset: one preset name; blank lines and `#`
/// comments are ignored.
pub const RENDER_PROFILE_FILE: &str = "render_profile.txt";

/// Where a host's preset came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetSource {
    /// [`RENDER_PROFILE_FLAG`] on the command line.
    CommandLine,
    /// [`RENDER_PROFILE_ENV`].
    Environment,
    /// [`RENDER_PROFILE_FILE`] in the scene directory.
    SceneDir,
    /// The host's default.
    Default,
}

impl PresetSource {
    /// Stable lowercase name (`command_line`, `environment`, `scene_dir`, `default`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CommandLine => "command_line",
            Self::Environment => "environment",
            Self::SceneDir => "scene_dir",
            Self::Default => "default",
        }
    }
}

/// A host's chosen preset and where it came from ([`choose_preset`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetChoice {
    /// The preset.
    pub preset: RenderProfilePreset,
    /// Where it came from.
    pub source: PresetSource,
}

/// Why a preset could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetChoiceError {
    /// [`RENDER_PROFILE_FLAG`] without a value.
    MissingValue,
    /// A name that is no preset.
    Unknown {
        /// The name as given.
        name: String,
        /// Where it was given.
        source: PresetSource,
    },
    /// A name that is neither a preset nor a tier of [`ProfileName::TIERS`] (the profile readers:
    /// [`choose_profile`], [`take_profile_arg`]; ADR-8844). Same code as [`Self::Unknown`].
    UnknownProfile {
        /// The name as given.
        name: String,
        /// Where it was given.
        source: PresetSource,
    },
    /// The scene directory's [`RENDER_PROFILE_FILE`] exists but could not be read.
    Unreadable {
        /// The file.
        path: PathBuf,
        /// The IO error.
        reason: String,
    },
}

impl PresetChoiceError {
    /// Stable lowercase code: `render_profile_missing_value`, `render_profile_unknown` or
    /// `render_profile_unreadable`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MissingValue => "render_profile_missing_value",
            Self::Unknown { .. } | Self::UnknownProfile { .. } => "render_profile_unknown",
            Self::Unreadable { .. } => "render_profile_unreadable",
        }
    }
}

impl fmt::Display for PresetChoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue => write!(f, "{RENDER_PROFILE_FLAG} needs a preset name"),
            Self::Unknown { name, source } => write!(
                f,
                "unknown render profile `{name}` ({}); one of {}",
                source.name(),
                preset_names().join(", ")
            ),
            Self::UnknownProfile { name, source } => write!(
                f,
                "unknown render profile `{name}` ({}); one of {}",
                source.name(),
                ProfileName::names().join(", ")
            ),
            Self::Unreadable { path, reason } => write!(f, "{}: {reason}", path.display()),
        }
    }
}

impl std::error::Error for PresetChoiceError {}

/// Every preset name a host or agent may ask for, the real-time ones first.
#[must_use]
pub fn preset_names() -> Vec<&'static str> {
    RenderProfilePreset::WITH_OFFLINE
        .iter()
        .map(|p| p.name())
        .collect()
}

fn parse_named(name: &str, source: PresetSource) -> Result<RenderProfilePreset, PresetChoiceError> {
    RenderProfilePreset::from_name(name.trim()).ok_or_else(|| PresetChoiceError::Unknown {
        name: name.trim().to_string(),
        source,
    })
}

/// Takes [`RENDER_PROFILE_FLAG`] (`--render-profile NAME` or `--render-profile=NAME`; the last
/// one wins) out of `args` and returns the preset it names plus the other arguments in order,
/// so a host's own parser never sees the flag.
///
/// # Errors
/// [`PresetChoiceError::MissingValue`] for a trailing flag, [`PresetChoiceError::Unknown`] for a
/// name that is no preset.
pub fn take_preset_arg<I, S>(args: I) -> Result<(Option<RenderProfilePreset>, Vec<String>), PresetChoiceError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut preset = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter().map(Into::into);
    while let Some(arg) = args.next() {
        if arg == RENDER_PROFILE_FLAG {
            let value = args.next().ok_or(PresetChoiceError::MissingValue)?;
            preset = Some(parse_named(&value, PresetSource::CommandLine)?);
        } else if let Some(value) = arg
            .strip_prefix(RENDER_PROFILE_FLAG)
            .and_then(|tail| tail.strip_prefix('='))
        {
            if value.trim().is_empty() {
                return Err(PresetChoiceError::MissingValue);
            }
            preset = Some(parse_named(value, PresetSource::CommandLine)?);
        } else {
            rest.push(arg);
        }
    }
    Ok((preset, rest))
}

/// Parses the text of a [`RENDER_PROFILE_FILE`]: the first line that is neither blank nor a `#`
/// comment names the preset (`Ok(None)` when there is none).
///
/// # Errors
/// [`PresetChoiceError::Unknown`] for a name that is no preset.
pub fn parse_preset_file(text: &str) -> Result<Option<RenderProfilePreset>, PresetChoiceError> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .find(|line| !line.is_empty())
        .map(|name| parse_named(name, PresetSource::SceneDir))
        .transpose()
}

/// Reads `dir/`[`RENDER_PROFILE_FILE`]; `Ok(None)` when the file does not exist.
///
/// # Errors
/// [`PresetChoiceError::Unreadable`] when the file exists but cannot be read,
/// [`PresetChoiceError::Unknown`] for a name that is no preset.
pub fn preset_from_scene_dir(dir: &Path) -> Result<Option<RenderProfilePreset>, PresetChoiceError> {
    let path = dir.join(RENDER_PROFILE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_preset_file(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(PresetChoiceError::Unreadable {
            path,
            reason: err.to_string(),
        }),
    }
}

/// The preset a host runs: the command line's (from [`take_preset_arg`]) first, then `env` (the
/// value of [`RENDER_PROFILE_ENV`]; blank counts as unset), then the scene directory's
/// [`RENDER_PROFILE_FILE`], then `default`. Pass the result through [`gate_preset`] before
/// applying it.
///
/// # Errors
/// [`PresetChoiceError::Unknown`] for a name that is no preset, [`PresetChoiceError::Unreadable`]
/// for a scene file that cannot be read.
pub fn choose_preset(
    command_line: Option<RenderProfilePreset>,
    env: Option<&str>,
    scene_dir: Option<&Path>,
    default: RenderProfilePreset,
) -> Result<PresetChoice, PresetChoiceError> {
    if let Some(preset) = command_line {
        return Ok(PresetChoice {
            preset,
            source: PresetSource::CommandLine,
        });
    }
    if let Some(value) = env.filter(|v| !v.trim().is_empty()) {
        return Ok(PresetChoice {
            preset: parse_named(value, PresetSource::Environment)?,
            source: PresetSource::Environment,
        });
    }
    if let Some(dir) = scene_dir
        && let Some(preset) = preset_from_scene_dir(dir)?
    {
        return Ok(PresetChoice {
            preset,
            source: PresetSource::SceneDir,
        });
    }
    Ok(PresetChoice {
        preset: default,
        source: PresetSource::Default,
    })
}

/// A host's chosen profile name and where it came from ([`choose_profile`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileChoice {
    /// The name.
    pub profile: ProfileName,
    /// Where it came from.
    pub source: PresetSource,
}

fn parse_profile_named(name: &str, source: PresetSource) -> Result<ProfileName, PresetChoiceError> {
    ProfileName::parse(name).ok_or_else(|| PresetChoiceError::UnknownProfile {
        name: name.trim().to_string(),
        source,
    })
}

/// [`take_preset_arg`] with tier names accepted ([`ProfileName::parse`], ADR-7977):
/// `--render-profile VeryLow` gives `Tier(VeryLow)`.
///
/// # Errors
/// [`PresetChoiceError::MissingValue`] for a trailing flag, [`PresetChoiceError::UnknownProfile`]
/// for a name that is neither a preset nor a tier of [`ProfileName::TIERS`].
pub fn take_profile_arg<I, S>(args: I) -> Result<(Option<ProfileName>, Vec<String>), PresetChoiceError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut profile = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter().map(Into::into);
    while let Some(arg) = args.next() {
        if arg == RENDER_PROFILE_FLAG {
            let value = args.next().ok_or(PresetChoiceError::MissingValue)?;
            profile = Some(parse_profile_named(&value, PresetSource::CommandLine)?);
        } else if let Some(value) = arg
            .strip_prefix(RENDER_PROFILE_FLAG)
            .and_then(|tail| tail.strip_prefix('='))
        {
            if value.trim().is_empty() {
                return Err(PresetChoiceError::MissingValue);
            }
            profile = Some(parse_profile_named(value, PresetSource::CommandLine)?);
        } else {
            rest.push(arg);
        }
    }
    Ok((profile, rest))
}

/// [`parse_preset_file`] with tier names accepted.
///
/// # Errors
/// [`PresetChoiceError::UnknownProfile`] for a name that is neither a preset nor a tier of
/// [`ProfileName::TIERS`].
pub fn parse_profile_file(text: &str) -> Result<Option<ProfileName>, PresetChoiceError> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .find(|line| !line.is_empty())
        .map(|name| parse_profile_named(name, PresetSource::SceneDir))
        .transpose()
}

/// [`preset_from_scene_dir`] with tier names accepted; `Ok(None)` when the file does not exist.
///
/// # Errors
/// [`PresetChoiceError::Unreadable`] when the file exists but cannot be read,
/// [`PresetChoiceError::UnknownProfile`] for a name that is neither a preset nor a tier.
pub fn profile_from_scene_dir(dir: &Path) -> Result<Option<ProfileName>, PresetChoiceError> {
    let path = dir.join(RENDER_PROFILE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_profile_file(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(PresetChoiceError::Unreadable {
            path,
            reason: err.to_string(),
        }),
    }
}

/// [`choose_preset`] with tier names accepted (ADR-7977): the command line's (from
/// [`take_profile_arg`]) first, then `env` (the value of [`RENDER_PROFILE_ENV`]; blank counts as
/// unset), then the scene directory's [`RENDER_PROFILE_FILE`], then `default`.
/// [`choose_preset`] keeps refusing tier names with `render_profile_unknown`, so existing hosts
/// see no change. Gate the result with [`gate_preset`] on [`ProfileName::preset`] (Very Low gates
/// as Desktop Low) and apply it with [`ProfileName::apply_for_gpu`].
///
/// # Errors
/// [`PresetChoiceError::UnknownProfile`] for a name that is neither a preset nor a tier,
/// [`PresetChoiceError::Unreadable`] for a scene file that cannot be read.
pub fn choose_profile(
    command_line: Option<ProfileName>,
    env: Option<&str>,
    scene_dir: Option<&Path>,
    default: RenderProfilePreset,
) -> Result<ProfileChoice, PresetChoiceError> {
    if let Some(profile) = command_line {
        return Ok(ProfileChoice {
            profile,
            source: PresetSource::CommandLine,
        });
    }
    if let Some(value) = env.filter(|v| !v.trim().is_empty()) {
        return Ok(ProfileChoice {
            profile: parse_profile_named(value, PresetSource::Environment)?,
            source: PresetSource::Environment,
        });
    }
    if let Some(dir) = scene_dir
        && let Some(profile) = profile_from_scene_dir(dir)?
    {
        return Ok(ProfileChoice {
            profile,
            source: PresetSource::SceneDir,
        });
    }
    Ok(ProfileChoice {
        profile: ProfileName::Preset(default),
        source: PresetSource::Default,
    })
}

#[cfg(test)]
mod tests;
