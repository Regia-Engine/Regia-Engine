// SPDX-License-Identifier: MPL-2.0
//! `.material.ron` authoring format v7 (`docs/specs/12-assets.md` § 4a, `docs/specs/13-renderer.md` § 7,
//! layered row in `docs/specs/26-terrain-vfx-highend.md` § 4). [`MATERIAL_FORMAT_VERSION`] is 7.
//!
//! v1 files still load: the built-in migration inserts [`HardSurfaceParams::default`]. v2 files load
//! with the v3 defaults (emissive strength 1, packed ORM, IOR 1.5, no transmission, alpha cutoff 0.5).
//! v3 files load with no thin film. v4 files load with the default weather response. v5 files load
//! with no subsurface scattering. v6 files load with no transmission roughness (clear glass). v7
//! stores [`MaterialAssetRon::transmission_roughness`].
//! Cooked payloads are a separate postcard document at [`COOKED_MATERIAL_VERSION`]:
//! texture slots are externally tagged there, because postcard cannot decode an untagged enum.

use std::str::FromStr;

use regia_core::version::{self, VersionStep};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::id::AssetId;

/// Current `.material.ron` format version.
///
/// v1 is the M4 document. v2 appends [`MaterialAssetRon::hard_surface`]. v3 appends the glTF
/// material extensions the hero assets use: [`StandardMaterialParams::emissive_strength`],
/// [`StandardMaterialParams::orm_packed`], [`StandardMaterialParams::ior`],
/// [`MaterialAssetRon::transmission`] and [`MaterialRenderState::alpha_cutoff`]. v4 appends
/// [`MaterialAssetRon::thin_film`] (`KHR_materials_iridescence`). v5 appends
/// [`MaterialAssetRon::weather`] (spec 99 § 7.3). v6 appends [`MaterialAssetRon::subsurface`]. v7
/// appends [`MaterialAssetRon::transmission_roughness`] (frosted glass, ADR-2075). Older files
/// migrate on load; a newer version is rejected.
pub const MATERIAL_FORMAT_VERSION: u32 = 7;

/// Cooked standard-material payload version stored in the postcard blob.
///
/// Version 1 was the implicit layout that postcard-encoded [`MaterialAssetRon`]
/// directly, with an untagged [`TextureSlot`]. Postcard refuses that enum
/// (`WontImplement`), so any cooked material that referenced a texture failed
/// to load. Version 2 is the first integer of the blob and tags each slot.
/// Version 3 appends the v2 hard-surface fields to that body. Version 4 carries the v3
/// fields (emissive strength, packed-ORM flag, IOR, transmission, alpha cutoff). Version 5
/// appends the v4 thin film. Version 6 appends the v5 weather response. Version 7 appends the v6
/// subsurface scattering. Version 8 appends the v7 transmission roughness.
/// [`CookedStandardMaterial::from_postcard`] also reads versions 7, 6, 5 and 4 (every pak shipped so
/// far), which migrate to clear glass, no subsurface scattering, the default weather response (and,
/// from 4, no film). Loaders reject every other value so the cooker rebuilds the cache instead of
/// surfacing a decode error.
pub const COOKED_MATERIAL_VERSION: u32 = 8;

/// An older cooked version: its body is the current one without the trailing film, weather,
/// subsurface and roughness.
const COOKED_MATERIAL_VERSION_NO_FILM: u32 = 4;

/// An older cooked version: its body is the current one without the trailing weather, subsurface
/// and roughness.
const COOKED_MATERIAL_VERSION_NO_WEATHER: u32 = 5;

/// An older cooked version: its body is the current one without the trailing subsurface and
/// roughness.
const COOKED_MATERIAL_VERSION_NO_SUBSURFACE: u32 = 6;

/// The previous cooked version: its body is the current one without the trailing roughness.
const COOKED_MATERIAL_VERSION_NO_ROUGHNESS: u32 = 7;

/// Maximum masked overlay layers (paint, dust, wear, wetness) in the standard shader.
pub const MAX_MATERIAL_LAYERS: usize = 4;

/// Authored material asset on disk.
///
/// [`Serialize`] omits empty optionals only for human-readable formats. Postcard
/// is positional, so the binary form always writes every field.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MaterialAssetRon {
    /// Format version.
    pub format_version: u32,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// `Standard` or a custom shader asset id.
    pub shader: MaterialShaderRef,
    /// Base PBR parameters.
    pub base: StandardMaterialParams,
    /// Optional clearcoat extension.
    #[serde(default)]
    pub clearcoat: Option<ClearcoatParams>,
    /// Optional anisotropy extension.
    #[serde(default)]
    pub anisotropy: Option<AnisotropyParams>,
    /// Up to four masked overlay layers.
    #[serde(default)]
    pub layers: Vec<MaterialLayer>,
    /// Render state overrides.
    #[serde(default)]
    pub render_state: MaterialRenderState,
    /// Optional parent material for instance overrides.
    #[serde(default)]
    pub parent: Option<AssetId>,
    /// Detail maps, clearcoat-adjacent response, parallax, and edge wear (v2).
    ///
    /// Absent on a v1 file until [`migrate_material_v1_to_v2`] runs.
    #[serde(default)]
    pub hard_surface: Option<HardSurfaceParams>,
    /// Thin-dielectric transmission and volume absorption (v3; glTF
    /// `KHR_materials_transmission` and `KHR_materials_volume`). `None` is opaque.
    #[serde(default)]
    pub transmission: Option<TransmissionParams>,
    /// Thin-film interference over the base specular (v4; glTF `KHR_materials_iridescence`).
    /// `None` is no film.
    #[serde(default)]
    pub thin_film: Option<ThinFilmParams>,
    /// How rain and snow change this material (v5; spec 99 § 6.2). `None` is the default
    /// response: porosity from roughness and metallic, full wetness and snow, no puddles.
    #[serde(default)]
    pub weather: Option<MaterialWeatherParams>,
    /// Screen-space subsurface scattering of the diffuse light (v6). `None` is no scattering.
    #[serde(default)]
    pub subsurface: Option<SubsurfaceParams>,
    /// How far a transmissive surface spreads what it shows behind it, perceptual roughness
    /// `0..=1` (v7, ADR-2075: frosted glass, a scuffed visor, ice). `None` and 0 are clear glass.
    /// It takes effect with [`Self::transmission`] above 0. It is separate from
    /// [`StandardMaterialParams::roughness`], which keeps shaping the reflection alone.
    #[serde(default)]
    pub transmission_roughness: Option<f32>,
}

/// Which measured or authored diffusion profile a material's light scatters with, v6.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsurfaceProfileKind {
    /// Human skin: the six Gaussians of d'Eon & Luebke 2007 (red reaches furthest).
    #[default]
    Skin,
    /// Leaves and thin plant tissue: an authored two-Gaussian mixture (green reaches furthest).
    Foliage,
}

/// Light scattering under the surface (skin, leaves), v6: the diffuse lighting spreads over
/// [`Self::radius`] on screen and the albedo is applied after it, so texture detail stays sharp.
///
/// `specs/13` § 7 and `book/agents/subsurface.md` describe the frame side. A material with this
/// block draws without it where the frame cannot scatter (no render profile asks for it, MSAA, a
/// frame without an HDR target).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubsurfaceParams {
    /// The diffusion profile.
    #[serde(default)]
    pub profile: SubsurfaceProfileKind,
    /// Reach of the scattering in millimeters on a world in meters. `0` takes the profile's
    /// measured or authored reach.
    #[serde(default)]
    pub radius: f32,
    /// Share of the light that scatters per channel, `0..=1` (linear RGB). The rest stays where
    /// it landed; black is no scattering.
    #[serde(default = "default_white")]
    pub tint: [f32; 3],
}

impl Default for SubsurfaceParams {
    fn default() -> Self {
        Self {
            profile: SubsurfaceProfileKind::Skin,
            radius: 0.0,
            tint: default_white(),
        }
    }
}

/// How the weather changes a material (spec 99 § 6.2), v5. Every field has the default
/// response's value, so `weather: Some(())` is the same as no block.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
pub struct MaterialWeatherParams {
    /// Porosity `0..=1`: how much a wet surface darkens. `None` derives it from roughness and
    /// metallic (`(1 - metallic) · clamp((roughness - 0.25) / 0.5, 0, 1)`).
    #[serde(default)]
    pub porosity: Option<f32>,
    /// Scales the wetness, `0..=1` (0: rain never wets it).
    #[serde(default = "default_weather_scale")]
    pub wet: f32,
    /// Scales the snow cover, `0..=1` (0: snow never settles on it).
    #[serde(default = "default_weather_scale")]
    pub snow: f32,
    /// Flat, upward parts of this material may hold puddles.
    #[serde(default)]
    pub puddles: bool,
}

impl Default for MaterialWeatherParams {
    fn default() -> Self {
        Self {
            porosity: None,
            wet: default_weather_scale(),
            snow: default_weather_scale(),
            puddles: false,
        }
    }
}

fn default_weather_scale() -> f32 {
    1.0
}

impl MaterialWeatherParams {
    /// Every value finite and in `0..=1`.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let unit = |v: f32| (0.0..=1.0).contains(&v);
        self.porosity.is_none_or(unit) && unit(self.wet) && unit(self.snow)
    }
}

/// A thin dielectric film over the base specular (soap film, oil slick, beetle shell, bird
/// feather), v4: the reflected colour shifts with the view angle and the film's thickness.
///
/// The fields and their defaults are `KHR_materials_iridescence`'s. The film covers
/// `factor × factor_map.r` of the surface and is
/// `mix(thickness_min, thickness_max, thickness_map.g)` nanometres thick;
/// without a thickness map it is [`Self::thickness_max`] thick everywhere.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ThinFilmParams {
    /// Share of the surface the film covers, `0..=1` (`iridescenceFactor`). `0` is no film.
    #[serde(default)]
    pub factor: f32,
    /// Coverage texture, read from R and multiplied into [`Self::factor`] (`iridescenceTexture`).
    #[serde(default)]
    pub factor_map: Option<TextureSlot>,
    /// Index of refraction of the film (`iridescenceIor`).
    #[serde(default = "default_film_ior")]
    pub ior: f32,
    /// Thickness in nanometres where the thickness map reads 0
    /// (`iridescenceThicknessMinimum`).
    #[serde(default = "default_film_thickness_min")]
    pub thickness_min: f32,
    /// Thickness in nanometres where the thickness map reads 1, and everywhere without a map
    /// (`iridescenceThicknessMaximum`).
    #[serde(default = "default_film_thickness_max")]
    pub thickness_max: f32,
    /// Thickness texture, read from G (`iridescenceThicknessTexture`).
    #[serde(default)]
    pub thickness_map: Option<TextureSlot>,
}

impl Default for ThinFilmParams {
    fn default() -> Self {
        Self {
            factor: 0.0,
            factor_map: None,
            ior: default_film_ior(),
            thickness_min: default_film_thickness_min(),
            thickness_max: default_film_thickness_max(),
            thickness_map: None,
        }
    }
}

fn default_film_ior() -> f32 {
    1.3
}

fn default_film_thickness_min() -> f32 {
    100.0
}

fn default_film_thickness_max() -> f32 {
    400.0
}

/// Light passing through a dielectric surface (glass, visors, ice), v3.
///
/// The transmitted light is tinted by the base colour (glTF transmission) and, inside the
/// volume, by `attenuation_color` per `attenuation_distance` over `thickness` metres
/// (Beer–Lambert, `KHR_materials_volume`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransmissionParams {
    /// Fraction of light transmitted, `0..=1`.
    #[serde(default)]
    pub factor: f32,
    /// Thickness of the volume behind the surface, metres. `0` is a thin wall (no absorption).
    #[serde(default)]
    pub thickness: f32,
    /// Colour white light has after travelling [`Self::attenuation_distance`] (linear RGB).
    #[serde(default = "default_white")]
    pub attenuation_color: [f32; 3],
    /// Distance at which light reaches [`Self::attenuation_color`], metres. `0` or less means
    /// infinite (no absorption).
    #[serde(default)]
    pub attenuation_distance: f32,
}

impl Default for TransmissionParams {
    fn default() -> Self {
        Self {
            factor: 0.0,
            thickness: 0.0,
            attenuation_color: default_white(),
            attenuation_distance: 0.0,
        }
    }
}

fn default_white() -> [f32; 3] {
    [1.0, 1.0, 1.0]
}

fn default_ior() -> f32 {
    1.5
}

/// Hard-surface response authored on top of the standard material.
///
/// The v1 → v2 migrator inserts [`HardSurfaceParams::default`], which turns on
/// energy-conserving multi-scatter and specular occlusion and leaves detail,
/// parallax, and edge wear off. A partial v2 document that names `hard_surface`
/// but omits the bools gets the same defaults (`true`), because a bare `bool`
/// with `#[serde(default)]` would otherwise deserialise as `false`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HardSurfaceParams {
    /// Tiling detail maps. `None` disables them.
    #[serde(default)]
    pub detail: Option<DetailMapParams>,
    /// Parallax occlusion mapping. `None` disables it (the profile gate is separate).
    #[serde(default)]
    pub parallax: Option<ParallaxParams>,
    /// Curvature or ambient-occlusion edge wear. `None` disables it.
    #[serde(default)]
    pub edge_wear: Option<EdgeWearParams>,
    /// Energy-conserving multi-scatter specular (Fdez-Agüera, Kulla & Conty).
    #[serde(default = "default_true")]
    pub multi_scatter: bool,
    /// Specular occlusion derived from ambient occlusion (Lagarde & de Rousiers).
    #[serde(default = "default_true")]
    pub specular_occlusion: bool,
}

impl Default for HardSurfaceParams {
    fn default() -> Self {
        Self {
            detail: None,
            parallax: None,
            edge_wear: None,
            multi_scatter: true,
            specular_occlusion: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_one() -> f32 {
    1.0
}

fn default_detail_uv_scale() -> [f32; 2] {
    [4.0, 4.0]
}

fn default_pom_scale() -> f32 {
    0.04
}

fn default_min_steps() -> u32 {
    4
}

fn default_max_steps() -> u32 {
    16
}

fn default_edge_tint() -> [f32; 3] {
    [0.75, 0.76, 0.78]
}

fn default_edge_bias() -> f32 {
    0.5
}

fn default_edge_contrast() -> f32 {
    4.0
}

/// Tiling detail albedo, normal, and roughness blended over the base.
///
/// Albedo is a mid-grey (0.5) multiply. `fade_end == 0` disables distance fade.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DetailMapParams {
    /// Detail albedo. RGB is centred on 0.5; the shader multiplies by two.
    #[serde(default)]
    pub albedo: Option<TextureSlot>,
    /// Detail normal, tangent space.
    #[serde(default)]
    pub normal: Option<TextureSlot>,
    /// Detail roughness, linear 0..1.
    #[serde(default)]
    pub roughness: Option<TextureSlot>,
    /// Blend mask. White applies the detail at full strength.
    #[serde(default)]
    pub mask: Option<TextureSlot>,
    /// Detail UV scale relative to the base UV (after parallax).
    #[serde(default = "default_detail_uv_scale")]
    pub uv_scale: [f32; 2],
    /// Albedo blend strength.
    #[serde(default = "default_one")]
    pub albedo_strength: f32,
    /// Normal blend strength (whiteout blend).
    #[serde(default = "default_one")]
    pub normal_strength: f32,
    /// Roughness blend strength.
    #[serde(default = "default_one")]
    pub roughness_strength: f32,
    /// Camera distance where the detail weight is still 1.
    #[serde(default)]
    pub fade_start: f32,
    /// Camera distance where the detail weight reaches 0. `0` disables the fade.
    #[serde(default)]
    pub fade_end: f32,
}

impl Default for DetailMapParams {
    fn default() -> Self {
        Self {
            albedo: None,
            normal: None,
            roughness: None,
            mask: None,
            uv_scale: default_detail_uv_scale(),
            albedo_strength: 1.0,
            normal_strength: 1.0,
            roughness_strength: 1.0,
            fade_start: 0.0,
            fade_end: 0.0,
        }
    }
}

/// Parallax occlusion mapping for panel lines and bolts.
///
/// The shader caps the march at 16 steps. Authored `max_steps` above that is
/// clamped at upload, not rejected here. Height is 1 at the geometric surface
/// (no offset) and 0 at maximum depth. The renderer also requires the desktop
/// High/Ultra profile gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParallaxParams {
    /// Height map. R = 1 is the geometric surface.
    #[serde(default)]
    pub height_map: Option<TextureSlot>,
    /// Depth scale in UV units.
    #[serde(default = "default_pom_scale")]
    pub scale: f32,
    /// Steps when the view faces the surface.
    #[serde(default = "default_min_steps")]
    pub min_steps: u32,
    /// Steps at grazing angles, before the shader's cap of 16.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
}

impl Default for ParallaxParams {
    fn default() -> Self {
        Self {
            height_map: None,
            scale: default_pom_scale(),
            min_steps: default_min_steps(),
            max_steps: default_max_steps(),
        }
    }
}

/// Channel the edge-wear mask is read from. Nothing is baked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeWearSource {
    /// Curvature map: 0.5 is flat, greater than 0.5 is convex.
    #[default]
    Curvature,
    /// Ambient occlusion: high values are exposed edges.
    Ao,
}

/// Edge wear driven by a curvature or ambient-occlusion channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeWearParams {
    /// Which channel supplies the mask.
    #[serde(default)]
    pub source: EdgeWearSource,
    /// Curvature texture when [`source`](Self::source) is [`EdgeWearSource::Curvature`].
    #[serde(default)]
    pub curvature_map: Option<TextureSlot>,
    /// Mask strength. `0` disables the effect.
    #[serde(default = "default_one")]
    pub intensity: f32,
    /// Added to roughness where the mask is 1. Negative values polish the edge.
    #[serde(default)]
    pub roughness_bias: f32,
    /// Added to metallic where the mask is 1 (paint chipping toward bare metal).
    #[serde(default)]
    pub metallic_bias: f32,
    /// Albedo mixed in where the mask is 1.
    #[serde(default = "default_edge_tint")]
    pub tint: [f32; 3],
    /// Sample value treated as zero wear (0.5 for a centred curvature map).
    #[serde(default = "default_edge_bias")]
    pub bias: f32,
    /// Contrast applied after the bias.
    #[serde(default = "default_edge_contrast")]
    pub contrast: f32,
}

impl Default for EdgeWearParams {
    fn default() -> Self {
        Self {
            source: EdgeWearSource::Curvature,
            curvature_map: None,
            intensity: 1.0,
            roughness_bias: 0.0,
            metallic_bias: 0.0,
            tint: default_edge_tint(),
            bias: default_edge_bias(),
            contrast: default_edge_contrast(),
        }
    }
}

/// Shader selection.
///
/// Written as `standard` / `custom(asset_id)` (the v1 wire spelling every saved material uses);
/// the frozen example in `docs/formats/material.example.ron` spells them `Standard` / `Custom`,
/// which is accepted too.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialShaderRef {
    /// Built-in metallic-roughness standard shader.
    #[serde(alias = "Standard")]
    Standard,
    /// Custom WGSL shader asset.
    #[serde(alias = "Custom")]
    Custom(AssetId),
}

/// Shared PBR parameters for base and layers.
///
/// Texture slots are omitted from `.material.ron` when absent. The postcard form
/// always writes them.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct StandardMaterialParams {
    /// Base colour factor (linear RGBA).
    #[serde(default = "default_base_color")]
    pub base_color: [f32; 4],
    /// Metallic factor.
    #[serde(default)]
    pub metallic: f32,
    /// Perceptual roughness.
    #[serde(default = "default_roughness")]
    pub roughness: f32,
    /// Normal map texture path or asset id string (resolved at cook).
    #[serde(default)]
    pub normal_map: Option<TextureSlot>,
    /// ORM packed map (occlusion R, roughness G, metallic B).
    #[serde(default)]
    pub orm_map: Option<TextureSlot>,
    /// Base colour texture.
    #[serde(default)]
    pub base_color_map: Option<TextureSlot>,
    /// Emissive colour factor.
    #[serde(default)]
    pub emissive: [f32; 3],
    /// Emissive texture.
    #[serde(default)]
    pub emissive_map: Option<TextureSlot>,
    /// Emissive scale (`KHR_materials_emissive_strength`), v3: luminance is
    /// `emissive × emissive_strength × texture` in nits.
    #[serde(default = "default_one")]
    pub emissive_strength: f32,
    /// [`Self::orm_map`] also carries the glTF metallic-roughness channels: G scales
    /// [`Self::roughness`] and B scales [`Self::metallic`] (glTF 2.0 § 3.9.2), v3. `false` reads
    /// occlusion only.
    #[serde(default = "default_true")]
    pub orm_packed: bool,
    /// Index of refraction of the dielectric (`KHR_materials_ior`), v3. Sets the normal-incidence
    /// reflectance `((ior − 1) / (ior + 1))²`; 1.5 is the glTF default (0.04).
    #[serde(default = "default_ior")]
    pub ior: f32,
}

fn default_base_color() -> [f32; 4] {
    [1.0, 1.0, 1.0, 1.0]
}

fn default_roughness() -> f32 {
    1.0
}

/// One masked overlay layer (terrain paint / wear / wetness).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MaterialLayer {
    /// Layer label (paint, dust, wear, wetness, ...).
    pub name: String,
    /// Blend mask texture (single channel or alpha).
    pub mask: TextureSlot,
    /// Layer albedo tint.
    #[serde(default = "default_base_color")]
    pub base_color: [f32; 4],
    /// Layer roughness multiplier.
    #[serde(default = "default_roughness")]
    pub roughness: f32,
    /// Layer metallic multiplier.
    #[serde(default)]
    pub metallic: f32,
    /// Optional triplanar mapping for this layer.
    #[serde(default)]
    pub triplanar: bool,
    /// Optional normal map for the layer.
    #[serde(default)]
    pub normal_map: Option<TextureSlot>,
}

/// Texture reference by project-relative path or cooked id.
///
/// In `.material.ron` (and any other human-readable format) this is a bare
/// string, matching the frozen v1 example: a string that parses as a GUID is
/// [`TextureSlot::Asset`], anything else is [`TextureSlot::Path`]. A project
/// path must not be a bare GUID.
///
/// In postcard the slot is an externally tagged enum. Variant order is the
/// discriminant (`Path` = 0, `Asset` = 1) and must not change.
#[derive(Clone, Debug, PartialEq)]
pub enum TextureSlot {
    /// Path under `assets/`.
    Path(String),
    /// Already-imported texture asset.
    Asset(AssetId),
}

/// Postcard form of [`TextureSlot`]. Discriminant order is load-bearing.
#[derive(Serialize, Deserialize)]
enum TextureSlotBin {
    Path(String),
    Asset(AssetId),
}

impl Serialize for TextureSlot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            match self {
                TextureSlot::Path(path) => serializer.serialize_str(path),
                TextureSlot::Asset(id) => serializer.collect_str(id),
            }
        } else {
            let bin = match self {
                TextureSlot::Path(path) => TextureSlotBin::Path(path.clone()),
                TextureSlot::Asset(id) => TextureSlotBin::Asset(*id),
            };
            bin.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for TextureSlot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let text = String::deserialize(deserializer)?;
            Ok(texture_slot_from_text(&text))
        } else {
            Ok(match TextureSlotBin::deserialize(deserializer)? {
                TextureSlotBin::Path(path) => TextureSlot::Path(path),
                TextureSlotBin::Asset(id) => TextureSlot::Asset(id),
            })
        }
    }
}

/// A GUID string is an asset id. Every other string is a project-relative path.
fn texture_slot_from_text(text: &str) -> TextureSlot {
    match AssetId::from_str(text) {
        Ok(id) => TextureSlot::Asset(id),
        Err(_) => TextureSlot::Path(text.to_owned()),
    }
}

/// Clearcoat BRDF extension parameters.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ClearcoatParams {
    /// Clearcoat strength 0..1.
    pub factor: f32,
    /// Clearcoat roughness.
    pub roughness: f32,
    /// Optional clearcoat normal map.
    #[serde(default)]
    pub normal_map: Option<TextureSlot>,
}

/// Anisotropic specular extension.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnisotropyParams {
    /// Anisotropy strength -1..1.
    pub strength: f32,
    /// Direction in tangent space (xy).
    pub direction: [f32; 2],
}

/// Raster state overrides carried into the cooked material.
///
/// `cull_mode: Some(None)` is glTF's `doubleSided`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MaterialRenderState {
    /// Alpha mode.
    #[serde(default)]
    pub alpha_mode: AlphaMode,
    /// Cull mode override.
    #[serde(default)]
    pub cull_mode: Option<CullMode>,
    /// Depth write override.
    #[serde(default)]
    pub depth_write: Option<bool>,
    /// Render queue offset.
    #[serde(default)]
    pub queue_offset: i32,
    /// Coverage below which [`AlphaMode::Mask`] discards (glTF `alphaCutoff`), v3.
    #[serde(default = "default_alpha_cutoff")]
    pub alpha_cutoff: f32,
}

impl Default for MaterialRenderState {
    fn default() -> Self {
        Self {
            alpha_mode: AlphaMode::default(),
            cull_mode: None,
            depth_write: None,
            queue_offset: 0,
            alpha_cutoff: default_alpha_cutoff(),
        }
    }
}

fn default_alpha_cutoff() -> f32 {
    0.5
}

/// Alpha blending mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlphaMode {
    /// Opaque surface.
    #[default]
    Opaque,
    /// Alpha cutout.
    Mask,
    /// Alpha blend.
    Blend,
}

/// Face culling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CullMode {
    /// Cull back faces.
    Back,
    /// Cull front faces.
    Front,
    /// No culling.
    None,
}

/// `.material.ron` drops `None` so files stay small. Postcard cannot skip a field.
fn serialize_optional_field<S, T>(
    state: &mut S,
    human_readable: bool,
    name: &'static str,
    value: &Option<T>,
) -> Result<(), S::Error>
where
    S: SerializeStruct,
    T: Serialize,
{
    if human_readable && value.is_none() {
        state.skip_field(name)
    } else {
        state.serialize_field(name, value)
    }
}

/// `.material.ron` drops an empty layer list. Postcard always writes it.
fn serialize_vec_field<S, T>(
    state: &mut S,
    human_readable: bool,
    name: &'static str,
    value: &[T],
) -> Result<(), S::Error>
where
    S: SerializeStruct,
    T: Serialize,
{
    if human_readable && value.is_empty() {
        state.skip_field(name)
    } else {
        state.serialize_field(name, value)
    }
}

impl Serialize for MaterialAssetRon {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut state = serializer.serialize_struct("MaterialAssetRon", 13)?;
        state.serialize_field("format_version", &self.format_version)?;
        serialize_optional_field(&mut state, human_readable, "name", &self.name)?;
        state.serialize_field("shader", &self.shader)?;
        state.serialize_field("base", &self.base)?;
        serialize_optional_field(&mut state, human_readable, "clearcoat", &self.clearcoat)?;
        serialize_optional_field(&mut state, human_readable, "anisotropy", &self.anisotropy)?;
        serialize_vec_field(&mut state, human_readable, "layers", &self.layers)?;
        state.serialize_field("render_state", &self.render_state)?;
        serialize_optional_field(&mut state, human_readable, "parent", &self.parent)?;
        serialize_optional_field(&mut state, human_readable, "hard_surface", &self.hard_surface)?;
        serialize_optional_field(&mut state, human_readable, "transmission", &self.transmission)?;
        serialize_optional_field(&mut state, human_readable, "thin_film", &self.thin_film)?;
        serialize_optional_field(&mut state, human_readable, "weather", &self.weather)?;
        serialize_optional_field(&mut state, human_readable, "subsurface", &self.subsurface)?;
        serialize_optional_field(
            &mut state,
            human_readable,
            "transmission_roughness",
            &self.transmission_roughness,
        )?;
        state.end()
    }
}

impl Serialize for MaterialWeatherParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut state = serializer.serialize_struct("MaterialWeatherParams", 4)?;
        serialize_optional_field(&mut state, human_readable, "porosity", &self.porosity)?;
        state.serialize_field("wet", &self.wet)?;
        state.serialize_field("snow", &self.snow)?;
        state.serialize_field("puddles", &self.puddles)?;
        state.end()
    }
}

impl Serialize for ThinFilmParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut state = serializer.serialize_struct("ThinFilmParams", 6)?;
        state.serialize_field("factor", &self.factor)?;
        serialize_optional_field(&mut state, human_readable, "factor_map", &self.factor_map)?;
        state.serialize_field("ior", &self.ior)?;
        state.serialize_field("thickness_min", &self.thickness_min)?;
        state.serialize_field("thickness_max", &self.thickness_max)?;
        serialize_optional_field(&mut state, human_readable, "thickness_map", &self.thickness_map)?;
        state.end()
    }
}

impl Serialize for StandardMaterialParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut state = serializer.serialize_struct("StandardMaterialParams", 11)?;
        state.serialize_field("base_color", &self.base_color)?;
        state.serialize_field("metallic", &self.metallic)?;
        state.serialize_field("roughness", &self.roughness)?;
        serialize_optional_field(&mut state, human_readable, "normal_map", &self.normal_map)?;
        serialize_optional_field(&mut state, human_readable, "orm_map", &self.orm_map)?;
        serialize_optional_field(&mut state, human_readable, "base_color_map", &self.base_color_map)?;
        state.serialize_field("emissive", &self.emissive)?;
        serialize_optional_field(&mut state, human_readable, "emissive_map", &self.emissive_map)?;
        state.serialize_field("emissive_strength", &self.emissive_strength)?;
        state.serialize_field("orm_packed", &self.orm_packed)?;
        state.serialize_field("ior", &self.ior)?;
        state.end()
    }
}

impl Serialize for MaterialLayer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut state = serializer.serialize_struct("MaterialLayer", 7)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("mask", &self.mask)?;
        state.serialize_field("base_color", &self.base_color)?;
        state.serialize_field("roughness", &self.roughness)?;
        state.serialize_field("metallic", &self.metallic)?;
        state.serialize_field("triplanar", &self.triplanar)?;
        serialize_optional_field(&mut state, human_readable, "normal_map", &self.normal_map)?;
        state.end()
    }
}

impl Serialize for ClearcoatParams {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut state = serializer.serialize_struct("ClearcoatParams", 3)?;
        state.serialize_field("factor", &self.factor)?;
        state.serialize_field("roughness", &self.roughness)?;
        serialize_optional_field(&mut state, human_readable, "normal_map", &self.normal_map)?;
        state.end()
    }
}

impl Serialize for MaterialRenderState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut state = serializer.serialize_struct("MaterialRenderState", 5)?;
        state.serialize_field("alpha_mode", &self.alpha_mode)?;
        serialize_optional_field(&mut state, human_readable, "cull_mode", &self.cull_mode)?;
        serialize_optional_field(&mut state, human_readable, "depth_write", &self.depth_write)?;
        state.serialize_field("queue_offset", &self.queue_offset)?;
        state.serialize_field("alpha_cutoff", &self.alpha_cutoff)?;
        state.end()
    }
}

/// Cooked standard material payload (postcard in the cooked file).
///
/// The first postcard integer is [`version`](Self::version). It is not part of
/// `.material.ron`. Version 1 caches (whose first integer is
/// [`MaterialAssetRon::format_version`], always 1) are rejected by
/// [`CookedStandardMaterial::from_postcard`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedStandardMaterial {
    /// [`COOKED_MATERIAL_VERSION`].
    pub version: u32,
    /// Mirrored parameters ready for GPU upload (texture ids resolved).
    pub authored: MaterialAssetRon,
}

impl CookedStandardMaterial {
    /// Cook `authored` at [`COOKED_MATERIAL_VERSION`].
    pub fn new(authored: MaterialAssetRon) -> Self {
        Self {
            version: COOKED_MATERIAL_VERSION,
            authored,
        }
    }

    /// Serialize to postcard bytes (version integer, then the authored material).
    ///
    /// # Errors
    ///
    /// Returns a postcard error if the value exceeds postcard's size limits.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Deserialize a cooked payload at [`COOKED_MATERIAL_VERSION`], or at version 7 (migrated to
    /// clear glass), 6 (also no subsurface scattering), 5 (also the default weather response) or 4
    /// (also no film), and reject any other version.
    ///
    /// The version integer is checked before the rest of the blob, so a version-1
    /// cache (untagged texture slots) returns [`MaterialLoadError::UnsupportedVersion`]
    /// instead of postcard's `WontImplement`.
    ///
    /// # Errors
    ///
    /// * [`MaterialLoadError::UnsupportedVersion`] — the leading version is not current.
    /// * [`MaterialLoadError::Postcard`] — the bytes are not a current cooked payload.
    /// * [`MaterialLoadError::Invalid`] — the authored material fails validation.
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, MaterialLoadError> {
        let (version, _) = postcard::take_from_bytes::<u32>(bytes)?;
        // Postcard is positional and the fields a version lacks are the last ones: the body is a
        // current one short of their `None` bytes.
        let missing_fields = match version {
            COOKED_MATERIAL_VERSION_NO_FILM => 4,
            COOKED_MATERIAL_VERSION_NO_WEATHER => 3,
            COOKED_MATERIAL_VERSION_NO_SUBSURFACE => 2,
            COOKED_MATERIAL_VERSION_NO_ROUGHNESS => 1,
            _ => 0,
        };
        if missing_fields > 0 {
            let mut padded = Vec::with_capacity(bytes.len() + missing_fields);
            padded.extend_from_slice(bytes);
            padded.resize(bytes.len() + missing_fields, 0);
            // An old body that does not decode is no cook this build reads: a truncated one, or a
            // bare `.material.ron` postcard (its leading format version may read the same). Both
            // get the recook advice, as every other old version does.
            let Ok(mut cooked) = postcard::from_bytes::<Self>(&padded) else {
                return Err(MaterialLoadError::UnsupportedVersion {
                    found: version,
                    supported: COOKED_MATERIAL_VERSION,
                });
            };
            cooked.version = COOKED_MATERIAL_VERSION;
            // The authored document inside carries its own older format version, or a current one
            // with empty new fields: either way the chain brings it to the latest.
            cooked.authored.migrate_to_latest()?;
            cooked.authored.validate()?;
            return Ok(cooked);
        }
        if version != COOKED_MATERIAL_VERSION {
            return Err(MaterialLoadError::UnsupportedVersion {
                found: version,
                supported: COOKED_MATERIAL_VERSION,
            });
        }
        let cooked: Self = postcard::from_bytes(bytes)?;
        cooked.authored.validate()?;
        Ok(cooked)
    }
}

impl MaterialAssetRon {
    /// Parse from RON bytes using the built-in migration chain.
    ///
    /// The `format_version` field is peeked before the full deserialise, so a *newer* version is
    /// rejected even when its body no longer matches the current struct. RON's `IMPLICIT_SOME`
    /// extension is enabled so the frozen v1 example's bare optionals (`name: "x"` rather than
    /// `name: Some("x")`) load.
    pub fn from_ron(bytes: &[u8]) -> Result<Self, MaterialError> {
        Self::from_ron_with_steps(bytes, builtin_migrations())
    }

    /// Parse from RON bytes with a caller-supplied migration chain.
    ///
    /// This is the full loader path — version peek, deserialise, migration, validation — with
    /// `steps` used instead of the built-in chain. Tests inject a step here to prove the loader
    /// actually applies it (not a side-channel call to [`regia_core::version::migrate`]).
    pub fn from_ron_with_steps(bytes: &[u8], steps: &[VersionStep<Self>]) -> Result<Self, MaterialError> {
        let s = std::str::from_utf8(bytes).map_err(|e| MaterialError::Ron(e.to_string()))?;
        let version =
            regia_core::version::peek_ron_version(s).map_err(|e| MaterialError::Ron(e.to_string()))?;
        if version > MATERIAL_FORMAT_VERSION {
            return Err(MaterialError::UnsupportedVersion {
                found: version,
                supported: MATERIAL_FORMAT_VERSION,
            });
        }
        let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
        let mut mat: Self = opts.from_str(s).map_err(|e| MaterialError::Ron(e.to_string()))?;
        mat.migrate_to_latest_with(steps)?;
        mat.validate()?;
        Ok(mat)
    }

    /// Serialize to pretty RON.
    pub fn to_ron_string(&self) -> Result<String, MaterialError> {
        self.validate()?;
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| MaterialError::Ron(e.to_string()))
    }

    /// Validate version and layer count.
    pub fn validate(&self) -> Result<(), MaterialError> {
        if self.format_version != MATERIAL_FORMAT_VERSION {
            return Err(MaterialError::UnsupportedVersion {
                found: self.format_version,
                supported: MATERIAL_FORMAT_VERSION,
            });
        }
        if self.layers.len() > MAX_MATERIAL_LAYERS {
            return Err(MaterialError::TooManyLayers {
                count: self.layers.len(),
                max: MAX_MATERIAL_LAYERS,
            });
        }
        if let Some(weather) = &self.weather
            && !weather.is_valid()
        {
            return Err(MaterialError::InvalidWeather(*weather));
        }
        if let Some(r) = self.transmission_roughness
            && !(0.0..=1.0).contains(&r)
        {
            return Err(MaterialError::TransmissionRoughness(r));
        }
        let b = &self.base;
        let unit = |name: &'static str, v: f32| {
            if (0.0..=1.0).contains(&v) {
                Ok(())
            } else {
                Err(MaterialError::InvalidParam { name, value: v })
            }
        };
        unit("roughness", b.roughness)?;
        unit("metallic", b.metallic)?;
        if !(b.ior.is_finite() && b.ior >= 1.0) {
            return Err(MaterialError::InvalidParam {
                name: "ior",
                value: b.ior,
            });
        }
        if !(b.emissive_strength.is_finite() && b.emissive_strength >= 0.0) {
            return Err(MaterialError::InvalidParam {
                name: "emissive_strength",
                value: b.emissive_strength,
            });
        }
        Ok(())
    }

    /// Reject a newer version, or run the built-in migrator chain to bring an older file up to
    /// [`MATERIAL_FORMAT_VERSION`] (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, MaterialError> {
        self.migrate_to_latest_with(builtin_migrations())
    }

    /// [`migrate_to_latest`](Self::migrate_to_latest) against a caller-supplied step chain.
    ///
    /// The version is still rejected when it exceeds [`MATERIAL_FORMAT_VERSION`]; an older version
    /// is stepped up through `steps` (a missing step is a [`MaterialError::Migration`]). This is the
    /// hook the migration-chain unit tests and a future project upgrader use to inject a real step.
    pub fn migrate_to_latest_with(&mut self, steps: &[VersionStep<Self>]) -> Result<u32, MaterialError> {
        if self.format_version > MATERIAL_FORMAT_VERSION {
            return Err(MaterialError::UnsupportedVersion {
                found: self.format_version,
                supported: MATERIAL_FORMAT_VERSION,
            });
        }
        if self.format_version < MATERIAL_FORMAT_VERSION {
            let to = version::migrate(self, self.format_version, steps, MATERIAL_FORMAT_VERSION)
                .map_err(|e| MaterialError::Migration(e.to_string()))?;
            self.format_version = to;
        }
        Ok(self.format_version)
    }

    /// Default grey standard material.
    pub fn default_standard() -> Self {
        Self {
            format_version: MATERIAL_FORMAT_VERSION,
            name: Some("Default".to_string()),
            shader: MaterialShaderRef::Standard,
            base: StandardMaterialParams {
                base_color: default_base_color(),
                metallic: 0.0,
                roughness: default_roughness(),
                normal_map: None,
                orm_map: None,
                base_color_map: None,
                emissive: [0.0, 0.0, 0.0],
                emissive_map: None,
                emissive_strength: 1.0,
                orm_packed: true,
                ior: default_ior(),
            },
            clearcoat: None,
            anisotropy: None,
            layers: Vec::new(),
            render_state: MaterialRenderState::default(),
            parent: None,
            hard_surface: Some(HardSurfaceParams::default()),
            transmission: None,
            thin_film: None,
            weather: None,
            subsurface: None,
            transmission_roughness: None,
        }
    }
}

/// v1 → v2: insert [`HardSurfaceParams::default`] when the block is absent.
///
/// Detail, parallax, and edge wear stay off. Multi-scatter and specular occlusion
/// turn on. `regia upgrade-project` rewrites the file through
/// [`migrate_material_ron_bytes`].
pub fn migrate_material_v1_to_v2(mat: &mut MaterialAssetRon) -> Result<(), String> {
    if mat.hard_surface.is_none() {
        mat.hard_surface = Some(HardSurfaceParams::default());
    }
    Ok(())
}

/// v2 → v3: the v3 fields take their defaults (serde fills them while reading a v2 body):
/// emissive strength 1, packed ORM on (the renderer scales roughness and metallic by the
/// ORM's G and B since ADR-069, which a v2 `orm_map` already meant), IOR 1.5 (F0 0.04, as
/// before), no transmission, alpha cutoff 0.5. Nothing a v2 file says changes meaning.
pub fn migrate_material_v2_to_v3(mat: &mut MaterialAssetRon) -> Result<(), String> {
    if !mat.base.ior.is_finite() || mat.base.ior < 1.0 {
        mat.base.ior = default_ior();
    }
    Ok(())
}

/// v3 → v4: [`MaterialAssetRon::thin_film`] is absent (no film), which is what a v3 file drew.
/// Nothing a v3 file says changes meaning.
pub fn migrate_material_v3_to_v4(mat: &mut MaterialAssetRon) -> Result<(), String> {
    // A v3 file cannot hold the v4 block: one that does is mislabelled (a hand edit or
    // a downgrade tool), and clearing it would lose data silently (audit-v035-formats 13).
    if mat.thin_film.is_some() {
        return Err(
            "a v3 material has a `thin_film` block, which format version 4 added; \
             fix its `format_version`"
                .to_owned(),
        );
    }
    Ok(())
}

/// v4 → v5: [`MaterialAssetRon::weather`] is absent (the default response), which is what a v4
/// file shaded with since the weather landed. Nothing a v4 file says changes meaning.
pub fn migrate_material_v4_to_v5(mat: &mut MaterialAssetRon) -> Result<(), String> {
    // A v4 file cannot hold the v5 block: one that does is mislabelled (a hand edit or
    // a downgrade tool), and clearing it would lose data silently (audit-v035-formats 13).
    if mat.weather.is_some() {
        return Err(
            "a v4 material has a `weather` block, which format version 5 added; \
             fix its `format_version`"
                .to_owned(),
        );
    }
    Ok(())
}

/// v5 → v6: [`MaterialAssetRon::subsurface`] is absent (no scattering), which is what a v5 file
/// drew. Nothing a v5 file says changes meaning.
pub fn migrate_material_v5_to_v6(mat: &mut MaterialAssetRon) -> Result<(), String> {
    // A v5 file cannot hold the v6 block: one that does is mislabelled (a hand edit or
    // a downgrade tool), and clearing it would lose data silently (audit-v035-formats 13).
    if mat.subsurface.is_some() {
        return Err(
            "a v5 material has a `subsurface` block, which format version 6 added; \
             fix its `format_version`"
                .to_owned(),
        );
    }
    Ok(())
}

/// v6 → v7: [`MaterialAssetRon::transmission_roughness`] is absent (clear glass), which is what a
/// v6 file drew. Nothing a v6 file says changes meaning.
pub fn migrate_material_v6_to_v7(mat: &mut MaterialAssetRon) -> Result<(), String> {
    // A v6 file cannot hold the v7 block: one that does is mislabelled (a hand edit or
    // a downgrade tool), and clearing it would lose data silently (audit-v035-formats 13).
    if mat.transmission_roughness.is_some() {
        return Err(
            "a v6 material has a `transmission_roughness` block, which format version 7 added; \
             fix its `format_version`"
                .to_owned(),
        );
    }
    Ok(())
}

/// Load `.material.ron` bytes and write them back at [`MATERIAL_FORMAT_VERSION`].
///
/// # Errors
///
/// Returns the loader error when the bytes are not a material this engine can migrate.
pub fn migrate_material_ron_bytes(bytes: &[u8]) -> Result<Vec<u8>, MaterialError> {
    let mat = MaterialAssetRon::from_ron(bytes)?;
    Ok(mat.to_ron_string()?.into_bytes())
}

/// The built-in migration chain for `.material.ron`.
fn builtin_migrations() -> &'static [VersionStep<MaterialAssetRon>] {
    &[
        VersionStep {
            from: 1,
            to: 2,
            migrate: migrate_material_v1_to_v2,
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: migrate_material_v2_to_v3,
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: migrate_material_v3_to_v4,
        },
        VersionStep {
            from: 4,
            to: 5,
            migrate: migrate_material_v4_to_v5,
        },
        VersionStep {
            from: 5,
            to: 6,
            migrate: migrate_material_v5_to_v6,
        },
        VersionStep {
            from: 6,
            to: 7,
            migrate: migrate_material_v6_to_v7,
        },
    ]
}

/// The versions a built-in migration step leaves (`from` side), for `cargo xtask check-formats`
/// to verify a version bump ships with a migrator from the previous version.
pub fn material_migration_sources() -> Vec<u32> {
    builtin_migrations().iter().map(|s| s.from).collect()
}

/// The serde field-list fingerprint of the `.material.ron` format (hashed by
/// `cargo xtask check-formats`).
///
/// The root struct *and* every nested struct are listed, so a shape change anywhere in the tree —
/// e.g. adding a field to [`StandardMaterialParams`] — changes the hash. Every serde enum's
/// variant names are listed too (canonical + alias spellings), so a `rename`/`rename_all`/`alias`
/// edit changes the hash without a field moving ([`TextureSlot`] has a hand-written string form
/// and no variants on the wire).
pub fn material_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<MaterialAssetRon>(),
        regia_core::schema::serde_field_names::<StandardMaterialParams>(),
        regia_core::schema::serde_field_names::<MaterialLayer>(),
        regia_core::schema::serde_field_names::<ClearcoatParams>(),
        regia_core::schema::serde_field_names::<AnisotropyParams>(),
        regia_core::schema::serde_field_names::<MaterialRenderState>(),
        regia_core::schema::serde_field_names::<HardSurfaceParams>(),
        regia_core::schema::serde_field_names::<DetailMapParams>(),
        regia_core::schema::serde_field_names::<ParallaxParams>(),
        regia_core::schema::serde_field_names::<EdgeWearParams>(),
        regia_core::schema::serde_field_names::<TransmissionParams>(),
        regia_core::schema::serde_field_names::<ThinFilmParams>(),
        regia_core::schema::serde_field_names::<MaterialWeatherParams>(),
        regia_core::schema::serde_field_names::<SubsurfaceParams>(),
        regia_core::schema::serde_variant_names::<MaterialShaderRef>(),
        regia_core::schema::serde_variant_names::<AlphaMode>(),
        regia_core::schema::serde_variant_names::<CullMode>(),
        regia_core::schema::serde_variant_names::<EdgeWearSource>(),
        regia_core::schema::serde_variant_names::<SubsurfaceProfileKind>(),
    ])
}

/// Errors parsing material assets.
#[derive(Debug, Error)]
pub enum MaterialError {
    /// RON failure.
    #[error("material ron error: {0}")]
    Ron(String),

    /// Bad format version.
    #[error("unsupported material format version {found} (supported: {supported})")]
    UnsupportedVersion {
        /// Found version.
        found: u32,
        /// Supported version.
        supported: u32,
    },

    /// A weather response value outside `0..=1` or not finite.
    #[error("material weather response out of range (porosity, wet and snow are 0..=1): {0:?}")]
    InvalidWeather(MaterialWeatherParams),

    /// Too many overlay layers.
    #[error("material has {count} layers (max {max})")]
    TooManyLayers {
        /// Layer count.
        count: usize,
        /// Maximum allowed.
        max: usize,
    },

    /// [`MaterialAssetRon::transmission_roughness`] is not a number in `0..=1`.
    #[error("material transmission_roughness {0} is outside 0..=1")]
    TransmissionRoughness(f32),

    /// A base scalar is out of range or not finite (roughness and metallic `0..=1`, IOR
    /// finite and at least 1, emissive strength finite and non-negative).
    #[error("material parameter `{name}` is invalid: {value}")]
    InvalidParam {
        /// The field name.
        name: &'static str,
        /// The rejected value.
        value: f32,
    },
    /// A migration step failed or is missing.
    #[error("material migration failed: {0}")]
    Migration(String),
}

/// Errors loading a cooked standard material.
#[derive(Debug, Error)]
pub enum MaterialLoadError {
    /// The leading version integer is not [`COOKED_MATERIAL_VERSION`].
    ///
    /// The cooker treats this as a stale cache and rebuilds from source.
    #[error("cooked material version {found} is not supported (supported: {supported}); recook the asset")]
    UnsupportedVersion {
        /// Version in the blob.
        found: u32,
        /// Version this runtime writes and reads.
        supported: u32,
    },

    /// Postcard rejected the blob.
    #[error("material postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    /// The authored body failed [`MaterialAssetRon::validate`].
    #[error(transparent)]
    Invalid(#[from] MaterialError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cooked_header::{
        COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version,
    };
    use crate::loader::{AssetTypeTag, load_material_bytes};
    use std::str::FromStr;

    const ASSET_GUID: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

    fn asset_id() -> AssetId {
        AssetId::from_str(ASSET_GUID).expect("guid")
    }

    fn slotted_material() -> MaterialAssetRon {
        let mut mat = MaterialAssetRon::default_standard();
        mat.name = Some("Slotted".to_string());
        mat.base.base_color_map = Some(TextureSlot::Path("textures/albedo.png".to_string()));
        mat.base.normal_map = Some(TextureSlot::Asset(asset_id()));
        mat.base.orm_map = Some(TextureSlot::Path("textures/orm.png".to_string()));
        mat.base.emissive_map = Some(TextureSlot::Asset(asset_id()));
        mat.clearcoat = Some(ClearcoatParams {
            factor: 0.2,
            roughness: 0.1,
            normal_map: Some(TextureSlot::Path("textures/clearcoat_n.png".to_string())),
        });
        mat.layers.push(MaterialLayer {
            name: "dust".to_string(),
            mask: TextureSlot::Path("textures/dust.png".to_string()),
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 1.0,
            metallic: 0.0,
            triplanar: false,
            normal_map: Some(TextureSlot::Asset(asset_id())),
        });
        mat
    }

    #[test]
    fn texture_slot_path_round_trips_ron_as_a_bare_string() {
        let slot = TextureSlot::Path("textures/concrete_albedo.png".to_string());
        let text = ron::to_string(&slot).expect("ron");
        assert_eq!(text, "\"textures/concrete_albedo.png\"");
        assert!(!text.contains("Path"));
        let back: TextureSlot = ron::from_str(&text).expect("parse");
        assert_eq!(back, slot);
    }

    #[test]
    fn texture_slot_asset_round_trips_ron_as_a_guid_string() {
        let slot = TextureSlot::Asset(asset_id());
        let text = ron::to_string(&slot).expect("ron");
        assert_eq!(text, format!("\"{ASSET_GUID}\""));
        assert!(!text.contains("Asset"));
        let back: TextureSlot = ron::from_str(&text).expect("parse");
        assert_eq!(back, slot);

        let upper = format!("\"{}\"", ASSET_GUID.to_ascii_uppercase());
        let from_upper: TextureSlot = ron::from_str(&upper).expect("upper guid");
        assert_eq!(from_upper, slot);
    }

    #[test]
    fn texture_slot_variants_round_trip_postcard() {
        for slot in [
            TextureSlot::Path("textures/albedo.png".to_string()),
            TextureSlot::Asset(asset_id()),
        ] {
            let bytes = postcard::to_allocvec(&slot).expect("encode");
            let back: TextureSlot = postcard::from_bytes(&bytes).expect("decode");
            assert_eq!(back, slot);
        }
    }

    #[test]
    fn material_with_every_slot_round_trips_ron_and_postcard() {
        let mat = slotted_material();
        let text = mat.to_ron_string().expect("ron");
        // Editor output keeps `Some(...)` around the frozen bare-string slot.
        assert!(text.contains("Some(\"textures/albedo.png\")"), "{text}");
        assert!(text.contains(&format!("Some(\"{ASSET_GUID}\")")), "{text}");
        assert!(!text.contains("Path(") && !text.contains("Asset("), "{text}");
        let from_text = MaterialAssetRon::from_ron(text.as_bytes()).expect("parse ron");
        assert_eq!(from_text, mat);

        let cooked = CookedStandardMaterial::new(mat);
        let bytes = cooked.to_postcard().expect("postcard");
        let (version, _rest): (u32, &[u8]) = postcard::take_from_bytes(&bytes).expect("version");
        assert_eq!(version, COOKED_MATERIAL_VERSION);
        let back = CookedStandardMaterial::from_postcard(&bytes).expect("decode");
        assert_eq!(back, cooked);
    }

    #[test]
    fn a_cooked_v4_body_that_does_not_decode_asks_for_a_recook() {
        let mut cooked = CookedStandardMaterial::new(MaterialAssetRon::default_standard());
        cooked.version = COOKED_MATERIAL_VERSION_NO_FILM;
        let mut bytes = postcard::to_allocvec(&cooked).expect("encode");
        // Drop the film's, the weather's, the subsurface's and the roughness's `None` bytes: a body
        // as a version 4 cook wrote it, which loads.
        bytes.truncate(bytes.len() - 4);
        assert!(CookedStandardMaterial::from_postcard(&bytes).is_ok());
        bytes.truncate(bytes.len() / 2);
        let err = CookedStandardMaterial::from_postcard(&bytes).expect_err("truncated");
        assert!(
            matches!(err, MaterialLoadError::UnsupportedVersion { found: 4, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("recook"), "{err}");
    }

    #[test]
    fn a_cooked_v6_body_loads_with_no_subsurface_and_keeps_its_film() {
        let mut authored = MaterialAssetRon::default_standard();
        authored.thin_film = Some(ThinFilmParams {
            factor: 0.5,
            ..ThinFilmParams::default()
        });
        let mut cooked = CookedStandardMaterial::new(authored.clone());
        cooked.version = COOKED_MATERIAL_VERSION_NO_SUBSURFACE;
        let mut bytes = postcard::to_allocvec(&cooked).expect("encode");
        // Drop the subsurface's and the roughness's `None` bytes: a body as a version 6 cook wrote it.
        bytes.truncate(bytes.len() - 2);
        let loaded = CookedStandardMaterial::from_postcard(&bytes).expect("version 6");
        assert_eq!(loaded.version, COOKED_MATERIAL_VERSION);
        assert_eq!(loaded.authored, authored);
    }

    #[test]
    fn a_cooked_body_keeps_its_subsurface_block() {
        let mut authored = MaterialAssetRon::default_standard();
        authored.subsurface = Some(SubsurfaceParams {
            profile: SubsurfaceProfileKind::Foliage,
            radius: 3.5,
            tint: [0.2, 0.9, 0.3],
        });
        let cooked = CookedStandardMaterial::new(authored);
        let bytes = cooked.to_postcard().expect("postcard");
        assert_eq!(
            CookedStandardMaterial::from_postcard(&bytes).expect("decode"),
            cooked
        );
    }

    /// A material with every versioned block set away from its default (`slotted_material`'s
    /// slots, clearcoat and layer, plus each block a later version added).
    fn every_factor_material() -> MaterialAssetRon {
        let mut mat = slotted_material();
        mat.base.base_color = [0.6, 0.5, 0.4, 1.0];
        mat.base.metallic = 0.3;
        mat.base.roughness = 0.6;
        mat.base.emissive = [0.1, 0.2, 0.3];
        mat.anisotropy = Some(AnisotropyParams {
            strength: 0.4,
            direction: [0.0, 1.0],
        });
        mat.hard_surface = Some(HardSurfaceParams {
            multi_scatter: false,
            specular_occlusion: false,
            ..HardSurfaceParams::default()
        });
        mat.transmission = Some(TransmissionParams {
            factor: 0.5,
            thickness: 0.01,
            attenuation_color: [0.8, 0.9, 1.0],
            attenuation_distance: 2.0,
        });
        mat.thin_film = Some(ThinFilmParams {
            factor: 0.4,
            ior: 1.4,
            thickness_min: 150.0,
            thickness_max: 350.0,
            ..ThinFilmParams::default()
        });
        mat.weather = Some(MaterialWeatherParams {
            porosity: Some(0.3),
            wet: 0.5,
            snow: 0.25,
            puddles: true,
        });
        mat.subsurface = Some(SubsurfaceParams {
            profile: SubsurfaceProfileKind::Foliage,
            radius: 4.0,
            tint: [0.2, 0.9, 0.3],
        });
        mat.transmission_roughness = Some(0.35);
        mat
    }

    /// `full` as a document of `version` holds it: the blocks later versions added are absent,
    /// and before v3 the base's v3 fields take the defaults serde fills.
    fn at_version(full: &MaterialAssetRon, version: u32) -> MaterialAssetRon {
        let mut mat = full.clone();
        mat.format_version = version;
        if version < 7 {
            mat.transmission_roughness = None;
        }
        if version < 6 {
            mat.subsurface = None;
        }
        if version < 5 {
            mat.weather = None;
        }
        if version < 4 {
            mat.thin_film = None;
        }
        if version < 3 {
            let defaults = MaterialAssetRon::default_standard();
            mat.transmission = None;
            mat.base.emissive_strength = defaults.base.emissive_strength;
            mat.base.orm_packed = defaults.base.orm_packed;
            mat.base.ior = defaults.base.ior;
            mat.render_state.alpha_cutoff = defaults.render_state.alpha_cutoff;
        } else {
            mat.base.emissive_strength = 2.0;
            mat.base.ior = 1.6;
            mat.render_state.alpha_cutoff = 0.4;
        }
        if version < 2 {
            mat.hard_surface = None;
        }
        mat
    }

    /// What loading `authored` (at its own version) must give: the same factors at the current
    /// version, and a v1 file's hard-surface block from its migrator.
    fn migrated(authored: &MaterialAssetRon) -> MaterialAssetRon {
        let mut expected = authored.clone();
        if authored.format_version < 2 {
            expected.hard_surface = Some(HardSurfaceParams::default());
        }
        expected.format_version = MATERIAL_FORMAT_VERSION;
        expected
    }

    /// Every factor of every `.material.ron` version survives the loader and its migration chain,
    /// the editor's save, and the cook, and so does every body an older cook wrote (ADR-2106 (1):
    /// weather v5, subsurface v6 and transmission roughness v7 joined in a row; a fixture left at a
    /// lane's number is caught by the same check).
    #[test]
    fn every_material_version_keeps_every_factor_through_load_save_and_cook() {
        let full = every_factor_material();
        let latest = at_version(&full, MATERIAL_FORMAT_VERSION);
        assert!(
            latest.subsurface.is_some()
                && latest.transmission_roughness.is_some()
                && latest.weather.is_some()
                && latest.thin_film.is_some()
                && latest.transmission.is_some()
                && latest.hard_surface.is_some(),
            "the fixture sets every block"
        );
        for version in 1..=MATERIAL_FORMAT_VERSION {
            let authored = at_version(&full, version);
            let text = ron::ser::to_string_pretty(&authored, regia_core::ron_pretty()).expect("ron");
            let loaded = MaterialAssetRon::from_ron(text.as_bytes())
                .unwrap_or_else(|err| panic!("v{version}: {err}\n{text}"));
            assert_eq!(loaded, migrated(&authored), "v{version}: load\n{text}");
            let saved = loaded.to_ron_string().expect("save");
            let reloaded = MaterialAssetRon::from_ron(saved.as_bytes()).expect("reload");
            assert_eq!(reloaded, loaded, "v{version}: save and reload\n{saved}");
            let cooked = CookedStandardMaterial::new(loaded.clone());
            let bytes = cooked.to_postcard().expect("cook");
            let back = CookedStandardMaterial::from_postcard(&bytes).expect("decode");
            assert_eq!(back.authored, loaded, "v{version}: cook");
        }
        // Bodies older cooks wrote: the authored document of their day, short of the trailing
        // fields' `None` bytes.
        for (cooked_version, authored_version, missing) in [
            (COOKED_MATERIAL_VERSION_NO_FILM, 3, 4),
            (COOKED_MATERIAL_VERSION_NO_WEATHER, 4, 3),
            (COOKED_MATERIAL_VERSION_NO_SUBSURFACE, 5, 2),
            (COOKED_MATERIAL_VERSION_NO_ROUGHNESS, 6, 1),
        ] {
            let authored = at_version(&full, authored_version);
            let mut cooked = CookedStandardMaterial::new(authored.clone());
            cooked.version = cooked_version;
            let mut bytes = postcard::to_allocvec(&cooked).expect("encode");
            bytes.truncate(bytes.len() - missing);
            let loaded = CookedStandardMaterial::from_postcard(&bytes)
                .unwrap_or_else(|err| panic!("cooked v{cooked_version}: {err}"));
            assert_eq!(loaded.version, COOKED_MATERIAL_VERSION);
            assert_eq!(loaded.authored, migrated(&authored), "cooked v{cooked_version}");
        }
    }

    #[test]
    fn version_one_payload_is_rejected_before_untagged_decode() {
        let err = CookedStandardMaterial::from_postcard(&[1, 0xff, 0xff]).expect_err("stale");
        assert!(
            matches!(
                err,
                MaterialLoadError::UnsupportedVersion {
                    found: 1,
                    supported: COOKED_MATERIAL_VERSION
                }
            ),
            "{err}"
        );
        let message = err.to_string();
        assert!(message.contains("recook"), "{message}");
        assert!(
            !message.to_ascii_lowercase().contains("wontimplement"),
            "{message}"
        );

        let legacy = postcard::to_allocvec(&MaterialAssetRon::default_standard()).expect("legacy");
        let err = CookedStandardMaterial::from_postcard(&legacy).expect_err("legacy blob");
        assert!(
            matches!(
                err,
                MaterialLoadError::UnsupportedVersion { found, .. } if found == MATERIAL_FORMAT_VERSION
            ),
            "{err}"
        );
    }

    #[test]
    fn v1_document_gains_hard_surface_defaults() {
        let v1 = br#"(
            format_version: 1,
            shader: standard,
            base: (),
        )"#;
        let mat = MaterialAssetRon::from_ron(v1).expect("v1");
        assert_eq!(mat.format_version, MATERIAL_FORMAT_VERSION);
        let hs = mat.hard_surface.expect("migrated");
        assert!(hs.multi_scatter);
        assert!(hs.specular_occlusion);
        assert!(hs.detail.is_none());
        assert!(hs.parallax.is_none());
        assert!(hs.edge_wear.is_none());
    }

    #[test]
    fn partial_hard_surface_bools_default_on() {
        let v2 = br#"(
            format_version: 2,
            shader: standard,
            base: (),
            hard_surface: (),
        )"#;
        let mat = MaterialAssetRon::from_ron(v2).expect("v2");
        let hs = mat.hard_surface.expect("block");
        assert!(hs.multi_scatter);
        assert!(hs.specular_occlusion);
        assert!(hs.detail.is_none());
    }

    #[test]
    fn a_v3_document_migrates_to_no_film_and_keeps_every_other_field() {
        let v3 = br#"(
            format_version: 3,
            shader: standard,
            base: (roughness: 0.25, ior: 1.6),
            transmission: (factor: 0.5),
        )"#;
        let mat = MaterialAssetRon::from_ron(v3).expect("v3");
        assert_eq!(mat.format_version, MATERIAL_FORMAT_VERSION);
        assert_eq!(mat.thin_film, None);
        assert_eq!(mat.base.roughness, 0.25);
        assert_eq!(mat.base.ior, 1.6);
        assert_eq!(mat.transmission.expect("kept").factor, 0.5);
        // `regia upgrade-project` rewrites through the same chain, and omits the empty block.
        let text = String::from_utf8(migrate_material_ron_bytes(v3).expect("rewrite")).expect("utf8");
        assert!(text.contains("format_version: 7"), "{text}");
        assert!(!text.contains("thin_film"), "{text}");
        assert!(!text.contains("weather"), "{text}");
        assert!(!text.contains("transmission_roughness"), "{text}");
    }

    #[test]
    fn material_migration_chain_starts_at_every_older_version() {
        assert_eq!(material_migration_sources(), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_v5_document_migrates_to_no_subsurface_and_keeps_its_film() {
        let v5 = br#"(
            format_version: 5,
            shader: standard,
            base: (roughness: 0.25),
            thin_film: (factor: 0.5),
        )"#;
        let mat = MaterialAssetRon::from_ron(v5).expect("v5");
        assert_eq!(mat.format_version, MATERIAL_FORMAT_VERSION);
        assert_eq!(mat.subsurface, None);
        assert_eq!(mat.thin_film.expect("kept").factor, 0.5);
        let text = String::from_utf8(migrate_material_ron_bytes(v5).expect("rewrite")).expect("utf8");
        assert!(text.contains("format_version: 7"), "{text}");
        assert!(!text.contains("subsurface"), "{text}");
    }

    #[test]
    fn a_bare_subsurface_block_takes_the_skin_defaults_and_round_trips() {
        let v6 = br#"(
            format_version: 6,
            shader: standard,
            base: (),
            subsurface: (),
        )"#;
        let block = MaterialAssetRon::from_ron(v6)
            .expect("v6")
            .subsurface
            .expect("block");
        assert_eq!(block, SubsurfaceParams::default());
        assert_eq!(block.profile, SubsurfaceProfileKind::Skin);
        assert_eq!(block.tint, [1.0; 3]);
        let v6 = br#"(
            format_version: 6,
            shader: standard,
            base: (),
            subsurface: (profile: foliage, radius: 4.0, tint: (0.2, 0.9, 0.3)),
        )"#;
        let mat = MaterialAssetRon::from_ron(v6).expect("v6");
        let text = mat.to_ron_string().expect("ron");
        assert!(text.contains("foliage"), "{text}");
        assert_eq!(MaterialAssetRon::from_ron(text.as_bytes()).expect("reload"), mat);
    }

    #[test]
    fn a_v4_file_loads_as_clear_glass() {
        let v4 = br#"(
            format_version: 4,
            shader: standard,
            base: (roughness: 0.3),
            transmission: (factor: 0.9),
            thin_film: (factor: 0.5),
        )"#;
        let mat = MaterialAssetRon::from_ron(v4).expect("v4");
        assert_eq!(mat.format_version, MATERIAL_FORMAT_VERSION);
        assert_eq!(mat.transmission_roughness, None);
        assert_eq!(mat.transmission.expect("kept").factor, 0.9);
        assert_eq!(mat.thin_film.expect("kept").factor, 0.5);
        assert_eq!(mat.base.roughness, 0.3);
    }

    #[test]
    fn transmission_roughness_round_trips_and_out_of_range_is_rejected() {
        let v7 = br#"(
            format_version: 7,
            shader: standard,
            base: (),
            transmission: (factor: 1.0),
            transmission_roughness: 0.4,
        )"#;
        let mat = MaterialAssetRon::from_ron(v7).expect("v7");
        assert_eq!(mat.transmission_roughness, Some(0.4));
        let text = mat.to_ron_string().expect("ron");
        assert_eq!(MaterialAssetRon::from_ron(text.as_bytes()).expect("reload"), mat);
        let cooked = CookedStandardMaterial::new(mat.clone());
        let bytes = cooked.to_postcard().expect("postcard");
        assert_eq!(
            CookedStandardMaterial::from_postcard(&bytes)
                .expect("decode")
                .authored,
            mat
        );
        for bad in [1.5, -0.1, f32::NAN, f32::INFINITY] {
            let mut m = MaterialAssetRon::default_standard();
            m.transmission_roughness = Some(bad);
            assert!(matches!(
                m.validate(),
                Err(MaterialError::TransmissionRoughness(_))
            ));
        }
    }

    /// A cooked body of version 4 to 7 (shipped paks) keeps loading: a current body without its
    /// trailing fields decodes to its older meaning, and a broken one gets the recook advice.
    #[test]
    fn older_cooked_bodies_load_without_the_trailing_fields() {
        let mut mat = MaterialAssetRon::default_standard();
        mat.base.roughness = 0.6;
        let cooked = CookedStandardMaterial::new(mat.clone());
        let current = cooked.to_postcard().expect("postcard");
        for (version, trailing) in [(7u8, 1usize), (6u8, 2usize), (5u8, 3usize), (4u8, 4usize)] {
            // The `None` bytes are at the very end of a body without either block.
            let mut old = current[..current.len() - trailing].to_vec();
            old[0] = version;
            let loaded = CookedStandardMaterial::from_postcard(&old).expect("older body");
            assert_eq!(loaded.version, COOKED_MATERIAL_VERSION);
            assert_eq!(loaded.authored, mat);
            old.truncate(old.len() - 1);
            assert!(matches!(
                CookedStandardMaterial::from_postcard(&old),
                Err(MaterialLoadError::UnsupportedVersion { found, .. }) if found == u32::from(version)
            ));
        }
    }

    #[test]
    fn a_bare_film_block_takes_the_extension_defaults() {
        let v4 = br#"(
            format_version: 4,
            shader: standard,
            base: (),
            thin_film: (),
        )"#;
        let film = MaterialAssetRon::from_ron(v4)
            .expect("v4")
            .thin_film
            .expect("block");
        assert_eq!(film, ThinFilmParams::default());
        assert_eq!(film.factor, 0.0);
        assert_eq!(film.ior, 1.3);
        assert_eq!((film.thickness_min, film.thickness_max), (100.0, 400.0));
        assert!(film.factor_map.is_none() && film.thickness_map.is_none());
    }

    #[test]
    fn a_film_with_maps_round_trips_ron_and_postcard() {
        let mut mat = MaterialAssetRon::default_standard();
        mat.thin_film = Some(ThinFilmParams {
            factor: 0.75,
            factor_map: Some(TextureSlot::Path("textures/film_mask.png".to_string())),
            ior: 1.45,
            thickness_min: 150.0,
            thickness_max: 620.0,
            thickness_map: Some(TextureSlot::Asset(asset_id())),
        });
        let text = mat.to_ron_string().expect("ron");
        assert!(text.contains("thickness_max: 620.0"), "{text}");
        assert_eq!(MaterialAssetRon::from_ron(text.as_bytes()).expect("parse"), mat);
        let cooked = CookedStandardMaterial::new(mat);
        let bytes = cooked.to_postcard().expect("postcard");
        assert_eq!(
            CookedStandardMaterial::from_postcard(&bytes).expect("decode"),
            cooked
        );
    }

    /// Every pak shipped before the film holds version 4 materials: they load, as no film, with
    /// every other field intact.
    #[test]
    fn a_version_4_payload_loads_with_no_film() {
        let mut older = slotted_material();
        older.format_version = 3;
        let mut bytes = CookedStandardMaterial {
            version: 4,
            authored: older,
        }
        .to_postcard()
        .expect("postcard");
        // A version 4 body ends where the film's `None` byte starts (the weather's, the
        // subsurface's and the roughness's follow it).
        assert_eq!(bytes.pop(), Some(0));
        assert_eq!(bytes.pop(), Some(0));
        assert_eq!(bytes.pop(), Some(0));
        assert_eq!(bytes.pop(), Some(0));
        let loaded = CookedStandardMaterial::from_postcard(&bytes).expect("version 4 loads");
        assert_eq!(loaded, CookedStandardMaterial::new(slotted_material()));
    }

    /// Every pak cooked with the film and before the weather holds version 5 materials: they load
    /// with the default weather response and their film intact (spec 99 § 7.3).
    #[test]
    fn a_version_5_payload_loads_with_the_default_weather() {
        let mut current = slotted_material();
        current.thin_film = Some(ThinFilmParams {
            factor: 0.5,
            ..ThinFilmParams::default()
        });
        let mut older = current.clone();
        older.format_version = 4;
        let mut bytes = CookedStandardMaterial {
            version: 5,
            authored: older,
        }
        .to_postcard()
        .expect("postcard");
        // A version 5 body ends where the weather's, the subsurface's and the roughness's `None`
        // bytes start.
        assert_eq!(bytes.pop(), Some(0));
        assert_eq!(bytes.pop(), Some(0));
        assert_eq!(bytes.pop(), Some(0));
        let loaded = CookedStandardMaterial::from_postcard(&bytes).expect("version 5 loads");
        assert_eq!(loaded, CookedStandardMaterial::new(current));
        assert_eq!(loaded.authored.weather, None);
        bytes.truncate(bytes.len() / 2);
        let err = CookedStandardMaterial::from_postcard(&bytes).expect_err("truncated");
        assert!(
            matches!(err, MaterialLoadError::UnsupportedVersion { found: 5, .. }),
            "{err}"
        );
    }

    #[test]
    fn a_v4_document_migrates_to_the_default_weather() {
        let v4 = br#"(
            format_version: 4,
            shader: standard,
            base: (roughness: 0.7),
            thin_film: (factor: 0.25),
        )"#;
        let mat = MaterialAssetRon::from_ron(v4).expect("v4");
        assert_eq!(mat.format_version, MATERIAL_FORMAT_VERSION);
        assert_eq!(mat.weather, None);
        assert_eq!(mat.thin_film.expect("kept").factor, 0.25);
        let text = String::from_utf8(migrate_material_ron_bytes(v4).expect("rewrite")).expect("utf8");
        assert!(text.contains("format_version: 7"), "{text}");
        assert!(!text.contains("weather"), "{text}");
    }

    #[test]
    fn a_weather_block_round_trips_and_a_bare_one_is_the_default() {
        let bare = br#"(
            format_version: 5,
            shader: standard,
            base: (),
            weather: (),
        )"#;
        let weather = MaterialAssetRon::from_ron(bare)
            .expect("v5")
            .weather
            .expect("block");
        assert_eq!(weather, MaterialWeatherParams::default());
        assert_eq!(
            (weather.porosity, weather.wet, weather.snow, weather.puddles),
            (None, 1.0, 1.0, false)
        );
        let mut mat = MaterialAssetRon::default_standard();
        mat.weather = Some(MaterialWeatherParams {
            porosity: Some(0.6),
            wet: 0.8,
            snow: 0.0,
            puddles: true,
        });
        let text = mat.to_ron_string().expect("ron");
        assert!(text.contains("porosity: Some(0.6)"), "{text}");
        assert_eq!(MaterialAssetRon::from_ron(text.as_bytes()).expect("parse"), mat);
        let derived = MaterialAssetRon {
            weather: Some(MaterialWeatherParams::default()),
            ..MaterialAssetRon::default_standard()
        };
        assert!(!derived.to_ron_string().expect("ron").contains("porosity"));
        let cooked = CookedStandardMaterial::new(mat);
        let bytes = cooked.to_postcard().expect("postcard");
        assert_eq!(
            CookedStandardMaterial::from_postcard(&bytes).expect("decode"),
            cooked
        );
    }

    #[test]
    fn a_weather_response_out_of_range_is_refused() {
        for bad in [
            MaterialWeatherParams {
                porosity: Some(1.5),
                ..MaterialWeatherParams::default()
            },
            MaterialWeatherParams {
                wet: -0.1,
                ..MaterialWeatherParams::default()
            },
            MaterialWeatherParams {
                snow: f32::NAN,
                ..MaterialWeatherParams::default()
            },
        ] {
            let mat = MaterialAssetRon {
                weather: Some(bad),
                ..MaterialAssetRon::default_standard()
            };
            assert!(
                matches!(mat.validate(), Err(MaterialError::InvalidWeather(_))),
                "{bad:?}"
            );
        }
        let text = br#"(format_version: 5, shader: standard, base: (), weather: (wet: 2.0))"#;
        assert!(MaterialAssetRon::from_ron(text).is_err());
    }

    #[test]
    fn non_finite_or_out_of_range_base_scalars_fail_validation() {
        let edit: [fn(&mut StandardMaterialParams); 6] = [
            |b| b.ior = -1.0,
            |b| b.ior = 0.5,
            |b| b.roughness = f32::NAN,
            |b| b.metallic = 2.0,
            |b| b.emissive_strength = f32::INFINITY,
            |b| b.emissive_strength = -1.0,
        ];
        for e in edit {
            let mut mat = MaterialAssetRon::default_standard();
            e(&mut mat.base);
            assert!(matches!(mat.validate(), Err(MaterialError::InvalidParam { .. })));
        }
        assert!(MaterialAssetRon::default_standard().validate().is_ok());
    }

    #[test]
    fn a_version_older_than_4_is_rejected_for_a_recook() {
        let mut bytes = CookedStandardMaterial::new(slotted_material())
            .to_postcard()
            .expect("postcard");
        bytes[0] = 3;
        let err = CookedStandardMaterial::from_postcard(&bytes).expect_err("stale");
        assert!(
            matches!(err, MaterialLoadError::UnsupportedVersion { found: 3, .. }),
            "{err}"
        );
    }

    #[test]
    fn cooked_file_round_trip_keeps_texture_slots() {
        let cooked = CookedStandardMaterial::new(slotted_material());
        let payload = cooked.to_postcard().expect("payload");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::standard_material().0,
            asset_id: asset_id(),
            importer_version: pack_importer_version(1, 1, 0),
            dependencies: vec![asset_id()],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("file");
        let (header_back, loaded) = load_material_bytes(&file).expect("load");
        assert_eq!(header_back.asset_id, asset_id());
        assert_eq!(loaded, cooked);
        assert!(matches!(
            loaded.authored.base.base_color_map,
            Some(TextureSlot::Path(ref path)) if path == "textures/albedo.png"
        ));
        assert!(matches!(
            loaded.authored.base.normal_map,
            Some(TextureSlot::Asset(id)) if id == asset_id()
        ));
    }
}

#[cfg(test)]
mod shader_spelling_tests {
    use super::{MaterialShaderRef, material_schema_fingerprint};

    #[test]
    fn both_v1_shader_spellings_load() {
        // Documented form and the snake_case form older v1 files were saved with.
        for text in ["Standard", "standard"] {
            let shader: MaterialShaderRef = ron::from_str(text).expect(text);
            assert_eq!(shader, MaterialShaderRef::Standard, "{text}");
        }
        // Saving keeps the pre-existing v1 spelling.
        assert_eq!(
            ron::to_string(&MaterialShaderRef::Standard).expect("ser"),
            "standard"
        );
    }

    /// The 2026-10-06 regression: dropping `rename_all` (or the UpperCamel aliases) from
    /// [`MaterialShaderRef`] changed the wire spelling without tripping `check-formats`, because
    /// the fingerprint did not hash variant names. Both spellings must stay in the fingerprint.
    #[test]
    fn fingerprint_hashes_the_wire_spellings() {
        let fingerprint = material_schema_fingerprint();
        for spelling in [
            "standard", "Standard", "custom", "Custom", "opaque", "mask", "blend",
        ] {
            assert!(
                fingerprint
                    .split(';')
                    .any(|list| list.split(',').any(|n| n == spelling)),
                "{spelling} missing from the fingerprint: {fingerprint}"
            );
        }
    }
}
