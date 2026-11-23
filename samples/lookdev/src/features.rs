// SPDX-License-Identifier: MIT-0
//! What the linked engine can actually shade, and what the in-flight lanes would add.
//!
//! Desktop High and Ultra request every photoreal knob [`regia_render::RenderProfile`]
//! already has (CSM count, PCSS-lite, GTAO, reflection-probe budget, TAA, GPU-driven
//! culling). The heightfield itself is drawn by `regia_terrain`, which on main applies
//! the sun cascade, exponential height fog, CDLOD, splat layers, scatter and holes.
//! Volumetric atmosphere, clouds, screen-space reflections, probe-grid GI, macro-variation
//! sampling and heightfield streaming are compile-time probes: they flip when those types
//! land on main, and [`EngineFeatures`] records them so `--compare` shows the lane.

use regia_render::{AntiAliasing, RenderProfile, RenderProfilePreset, ShadowFilter, SsaoMode};
use serde::{Deserialize, Serialize};

use crate::shots::ProfileKind;

/// Compile-time detection. Each flag is set by `build.rs` when one of the named items
/// exists under `crates/`. Presence is not application: the terrain hook still draws
/// one sun cascade, and this harness does not submit the forward SSR or GI passes.
pub const SSR_LANE_PRESENT: bool = cfg!(lookdev_detect_ssr);
/// `GiField` / `GpuGiGrid` (M15b) are in the linked render crate.
pub const GI_LANE_PRESENT: bool = cfg!(lookdev_detect_gi);
/// Volumetric atmosphere (M15c).
pub const ATMOSPHERE_LANE_PRESENT: bool = cfg!(lookdev_detect_atmosphere);
/// Volumetric clouds (M15c).
pub const CLOUDS_LANE_PRESENT: bool = cfg!(lookdev_detect_clouds);
/// Heightfield streaming (terrain-data lane).
pub const TERRAIN_STREAM_LANE_PRESENT: bool = cfg!(lookdev_detect_terrain_stream);
/// GPU sampling of macro variation / layer height maps (terrain-photoreal lane).
pub const MACRO_LANE_PRESENT: bool = cfg!(lookdev_detect_macro);

/// One profile's requested quality, next to what the terrain hook actually runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileFeatures {
    /// `ultra`, `high`, or `deck-medium`.
    pub profile: String,
    /// Preset name (`DesktopUltra`, `DesktopHigh`, `Deck`).
    pub preset: String,
    /// SSAO / GTAO mode the preset requests.
    pub ssao: String,
    /// Shadow filter the preset requests.
    pub shadow_filter: String,
    /// CSM cascade count the preset requests. The terrain hook runs one cascade.
    pub csm_cascades: u32,
    /// Shadow-map edge the preset requests, pixels.
    pub csm_resolution: u32,
    /// Reflection-probe budget (the M15b GI cap on the forward path).
    pub reflection_probes: u32,
    /// GPU-driven culling requested.
    pub gpu_driven: bool,
    /// Temporal AA requested.
    pub taa: bool,
    /// Internal render scale.
    pub render_scale: f32,
    /// Cascades the terrain shader samples. One, until the hook grows.
    pub applied_terrain_cascades: u32,
    /// Height fog is mixed in the terrain shader for every look-dev profile.
    pub height_fog: bool,
    /// The single terrain sun cascade is encoded for every look-dev profile.
    pub sun_shadow: bool,
}

/// Features recorded at the top of `shots.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineFeatures {
    /// CDLOD node selection is on (the terrain crate's LOD).
    pub cdlod: bool,
    /// Layer height maps are generated with the scene.
    pub authored_layer_height_maps: bool,
    /// The linked terrain GPU samples those height maps.
    pub gpu_samples_layer_height_maps: bool,
    /// The macro-variation map is generated with the scene.
    pub authored_macro_variation: bool,
    /// The linked terrain GPU samples the macro map.
    pub gpu_samples_macro_variation: bool,
    /// An SSR pass type exists in the linked render crate.
    pub ssr_in_linked_crates: bool,
    /// A probe-grid GI type exists in the linked render crate.
    pub gi_probes_in_linked_crates: bool,
    /// A volumetric atmosphere type exists.
    pub volumetric_atmosphere_in_linked_crates: bool,
    /// A volumetric cloud type exists.
    pub volumetric_clouds_in_linked_crates: bool,
    /// A heightfield streamer type exists.
    pub terrain_streaming_in_linked_crates: bool,
    /// The harness wrote each layer height map to an `R32Float` texture.
    /// The terrain shader samples those maps only when [`MACRO_LANE_PRESENT`] is set.
    #[serde(default)]
    pub uploaded_layer_height_maps: bool,
    /// The harness wrote the macro-variation map to an `R32Float` texture.
    #[serde(default)]
    pub uploaded_macro_variation: bool,
    /// Payload bytes of the uploaded layer height maps (`resolution² × 4` each).
    #[serde(default)]
    pub uploaded_layer_height_bytes: u64,
    /// Payload bytes of the uploaded macro map.
    #[serde(default)]
    pub uploaded_macro_bytes: u64,
    /// The device exposes timestamp queries, so per-pass GPU times are real.
    pub timestamp_query: bool,
    /// Per-profile requested and applied knobs.
    pub profiles: Vec<ProfileFeatures>,
}

impl EngineFeatures {
    /// Probes the three look-dev profiles and the compile-time lane flags.
    #[must_use]
    pub fn detect(timestamp_query: bool) -> Self {
        Self {
            cdlod: true,
            authored_layer_height_maps: true,
            gpu_samples_layer_height_maps: MACRO_LANE_PRESENT,
            authored_macro_variation: true,
            gpu_samples_macro_variation: MACRO_LANE_PRESENT,
            ssr_in_linked_crates: SSR_LANE_PRESENT,
            gi_probes_in_linked_crates: GI_LANE_PRESENT,
            volumetric_atmosphere_in_linked_crates: ATMOSPHERE_LANE_PRESENT,
            volumetric_clouds_in_linked_crates: CLOUDS_LANE_PRESENT,
            terrain_streaming_in_linked_crates: TERRAIN_STREAM_LANE_PRESENT,
            uploaded_layer_height_maps: false,
            uploaded_macro_variation: false,
            uploaded_layer_height_bytes: 0,
            uploaded_macro_bytes: 0,
            timestamp_query,
            profiles: [ProfileKind::Ultra, ProfileKind::High, ProfileKind::DeckMedium]
                .into_iter()
                .map(profile_features)
                .collect(),
        }
    }
}

fn profile_features(kind: ProfileKind) -> ProfileFeatures {
    let preset = kind.preset();
    let profile = RenderProfile::from_preset(preset);
    ProfileFeatures {
        profile: kind.name().to_string(),
        preset: preset_name(preset).to_string(),
        ssao: ssao_name(profile.ssao).to_string(),
        shadow_filter: shadow_name(profile.shadow_filter).to_string(),
        csm_cascades: profile.csm_cascade_count,
        csm_resolution: profile.csm_resolution,
        reflection_probes: profile.max_reflection_probes,
        gpu_driven: profile.gpu_driven,
        taa: matches!(profile.anti_aliasing, AntiAliasing::Taa),
        render_scale: profile.render_scale,
        applied_terrain_cascades: 1,
        height_fog: true,
        sun_shadow: true,
    }
}

fn preset_name(preset: RenderProfilePreset) -> &'static str {
    match preset {
        RenderProfilePreset::MobileLow => "MobileLow",
        RenderProfilePreset::MobileHigh => "MobileHigh",
        RenderProfilePreset::DesktopLow => "DesktopLow",
        RenderProfilePreset::DesktopHigh => "DesktopHigh",
        RenderProfilePreset::DesktopUltra => "DesktopUltra",
        RenderProfilePreset::Deck => "Deck",
        RenderProfilePreset::Cinematic => "Cinematic",
    }
}

fn ssao_name(mode: SsaoMode) -> &'static str {
    match mode {
        SsaoMode::Off => "off",
        SsaoMode::HalfRes => "half_res",
        SsaoMode::GtaoHalfRes => "gtao_half",
        SsaoMode::GtaoFull => "gtao_full",
    }
}

fn shadow_name(filter: ShadowFilter) -> &'static str {
    match filter {
        ShadowFilter::Hardware2x2 => "hardware_2x2",
        ShadowFilter::Pcf3x3 => "pcf_3x3",
        ShadowFilter::Pcf => "pcf",
        ShadowFilter::PcssLite => "pcss_lite",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ultra_requests_the_high_end_knobs_and_applies_fog_and_sun() {
        let features = EngineFeatures::detect(false);
        let ultra = features
            .profiles
            .iter()
            .find(|p| p.profile == "ultra")
            .expect("ultra profile");
        assert_eq!(ultra.preset, "DesktopUltra");
        assert_eq!(ultra.ssao, "gtao_full");
        assert_eq!(ultra.shadow_filter, "pcss_lite");
        assert_eq!(ultra.csm_cascades, 4);
        assert!(ultra.gpu_driven);
        assert!(ultra.taa);
        assert!(ultra.height_fog);
        assert!(ultra.sun_shadow);
        assert_eq!(ultra.applied_terrain_cascades, 1);
        assert!(
            features
                .profiles
                .iter()
                .all(|profile| profile.applied_terrain_cascades == 1)
        );
        assert!(features.authored_layer_height_maps);
        assert!(features.authored_macro_variation);
        assert!(features.cdlod);
        // M15b is on this tree. The flags record the types; they do not mean the
        // terrain draw ran SSR or the probe grid.
        assert!(
            features.ssr_in_linked_crates,
            "fn ssr_roughness_weight / ReflectionProbe"
        );
        assert!(features.gi_probes_in_linked_crates, "GiField / GpuGiGrid");
    }

    #[test]
    fn deck_medium_is_the_deck_preset() {
        let features = EngineFeatures::detect(true);
        let deck = features
            .profiles
            .iter()
            .find(|p| p.profile == "deck-medium")
            .expect("deck profile");
        assert_eq!(deck.preset, "Deck");
        assert!(deck.render_scale < 0.99);
        assert!(features.timestamp_query);
    }
}
