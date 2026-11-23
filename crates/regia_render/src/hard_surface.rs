// SPDX-License-Identifier: MPL-2.0
//! Hard-surface material response: detail maps, clearcoat, anisotropy, specular
//! occlusion, multi-scatter, parallax occlusion, and edge wear.
//!
//! The formulas are re-derived in [`crate`] tests from the public sources listed in
//! `docs/provenance.md`. The GPU path is `shaders/lab_forward.wgsl`. Features stay
//! off on [`LabMaterial::EMPTY`] so existing goldens do not move; a `.material.ron`
//! load turns multi-scatter and specular occlusion on via [`LabMaterial::from_authored`].

use regia_asset::{
    AlphaMode as AssetAlpha, AnisotropyParams, ClearcoatParams, EdgeWearSource, MaterialAssetRon,
};
use regia_core::glam::{Mat4, Vec3};

use crate::lab_frame::{
    ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, ALPHA_PREMULTIPLIED, LabMaterial, LabMesh, LabScene, LabTexture,
    LabVertex,
};
use crate::primitives::uv_sphere;
use crate::profile::FrameQuality;

/// One hard-surface feature the golden harness can toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardSurfaceFeature {
    /// Tiling detail albedo, normal, and roughness.
    Detail,
    /// Second specular lobe.
    Clearcoat,
    /// Brushed-metal anisotropic GGX.
    Anisotropy,
    /// Specular occlusion derived from ambient occlusion.
    SpecularOcclusion,
    /// Energy-conserving multi-scatter specular.
    MultiScatter,
    /// Parallax occlusion mapping.
    Parallax,
    /// Curvature-driven edge wear.
    EdgeWear,
}

impl HardSurfaceFeature {
    /// Every feature the golden harness renders.
    pub const ALL: [Self; 7] = [
        Self::Detail,
        Self::Clearcoat,
        Self::Anisotropy,
        Self::SpecularOcclusion,
        Self::MultiScatter,
        Self::Parallax,
        Self::EdgeWear,
    ];

    /// File stem for the lavapipe golden.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Detail => "detail",
            Self::Clearcoat => "clearcoat",
            Self::Anisotropy => "anisotropy",
            Self::SpecularOcclusion => "specular_occlusion",
            Self::MultiScatter => "multi_scatter",
            Self::Parallax => "parallax",
            Self::EdgeWear => "edge_wear",
        }
    }
}

/// Distance fade for a detail map. `fade_end <= fade_start` leaves the weight at 1.
/// Near the camera the result is 1; at `fade_end` and beyond it is 0.
#[must_use]
pub fn detail_fade(distance: f32, fade_start: f32, fade_end: f32) -> f32 {
    if fade_end <= fade_start {
        1.0
    } else {
        ((fade_end - distance) / (fade_end - fade_start)).clamp(0.0, 1.0)
    }
}

/// Detail blend weight: distance fade × mask × strength.
#[must_use]
pub fn detail_weight(distance: f32, fade_start: f32, fade_end: f32, mask: f32, strength: f32) -> f32 {
    detail_fade(distance, fade_start, fade_end) * mask.clamp(0.0, 1.0) * strength
}

/// Mid-grey (0.5) detail multiply: `base * mix(1, detail * 2, weight)`.
#[must_use]
pub fn apply_detail_albedo(base: [f32; 3], detail: [f32; 3], weight: f32) -> [f32; 3] {
    let mut out = [0.0; 3];
    for i in 0..3 {
        let scale = (1.0 - weight) + detail[i] * 2.0 * weight;
        out[i] = base[i] * scale;
    }
    out
}

/// Whiteout normal blend. `n2.xy` is scaled by `strength` before the add.
#[must_use]
pub fn blend_normals_whiteout(n1: [f32; 3], n2: [f32; 3], strength: f32) -> [f32; 3] {
    let x = n1[0] + n2[0] * strength;
    let y = n1[1] + n2[1] * strength;
    let z = n1[2] * n2[2];
    let len = (x * x + y * y + z * z).sqrt().max(1.0e-8);
    [x / len, y / len, z / len]
}

/// Linear roughness mix by `weight`.
#[must_use]
pub fn apply_detail_roughness(base: f32, detail: f32, weight: f32) -> f32 {
    base * (1.0 - weight) + detail * weight
}

/// Specular occlusion from ambient occlusion.
///
/// `ao == 1` returns 1. Roughness 1 returns `ao`. At a facing smooth surface a
/// partial `ao` returns a value above `ao` (the highlight is not fully crushed).
#[must_use]
pub fn specular_occlusion(no_v: f32, ao: f32, perceptual_roughness: f32) -> f32 {
    let exponent = (-16.0 * perceptual_roughness - 1.0).exp2();
    ((no_v + ao).powf(exponent) - 1.0 + ao).clamp(0.0, 1.0)
}

/// The smallest GGX α the shading uses (`pbr_lighting.wgsl` `MIN_ALPHA`): perceptual roughness
/// 0.045, squared (Filament's minimum; Guy & Agopian 2018 § 4.8.3.3).
pub const MIN_GGX_ALPHA: f32 = 0.002_025;

/// The specular occlusion the forward pass applies to the indirect specular: once, in the
/// always-on α form (`α = perceptual²`, floored at [`MIN_GGX_ALPHA`] as the shader floors it) or, for a
/// material that authors specular occlusion, the perceptual-roughness form in its place.
/// Mirrors `lab_forward.wgsl`.
#[must_use]
pub fn indirect_specular_occlusion(no_v: f32, ao: f32, perceptual_roughness: f32, authored: bool) -> f32 {
    if authored {
        specular_occlusion(no_v, ao, perceptual_roughness)
    } else {
        specular_occlusion(
            no_v,
            ao,
            (perceptual_roughness * perceptual_roughness).max(MIN_GGX_ALPHA),
        )
    }
}

/// Anisotropic GGX roughness along tangent (`[0]`) and bitangent (`[1]`).
///
/// `alpha` is the isotropic α (perceptual roughness squared). Positive
/// `anisotropy` widens the lobe along the tangent.
#[must_use]
pub fn anisotropic_alphas(alpha: f32, anisotropy: f32) -> [f32; 2] {
    [
        (alpha * (1.0 + anisotropy)).max(1.0e-4),
        (alpha * (1.0 - anisotropy)).max(1.0e-4),
    ]
}

/// Isotropic GGX NDF. `alpha` is α, matching `d_ggx` in the lighting shader.
#[must_use]
pub fn d_ggx_isotropic(no_h: f32, alpha: f32) -> f32 {
    let a2 = alpha * alpha;
    let f = (no_h * a2 - no_h) * no_h + 1.0;
    a2 / (std::f32::consts::PI * f * f)
}

/// Anisotropic GGX NDF. Equal `at` and `ab` match [`d_ggx_isotropic`].
#[must_use]
pub fn d_ggx_anisotropic(dot_th: f32, dot_bh: f32, dot_nh: f32, at: f32, ab: f32) -> f32 {
    let a2 = at * ab;
    let fx = ab * dot_th;
    let fy = at * dot_bh;
    let fz = a2 * dot_nh;
    let denom = (fx * fx + fy * fy + fz * fz).max(1.0e-8);
    let w2 = a2 / denom;
    a2 * w2 * w2 / std::f32::consts::PI
}

fn schlick_f32(vo_h: f32, f0: f32) -> f32 {
    let one_minus = (1.0 - vo_h).clamp(0.0, 1.0);
    let mut f = one_minus * one_minus;
    f *= f;
    f *= one_minus;
    f0 + (1.0 - f0) * f
}

/// Base-layer attenuation under a dielectric clearcoat, `1 - factor * F(NoV, 0.04)`.
#[must_use]
pub fn clearcoat_attenuation(no_v: f32, factor: f32) -> f32 {
    1.0 - factor * schlick_f32(no_v, 0.04)
}

/// Average Fresnel of a Schlick lobe, `F0 + (1 - F0) / 21`.
#[must_use]
pub fn fresnel_average(f0: [f32; 3]) -> [f32; 3] {
    let mut out = [0.0; 3];
    for i in 0..3 {
        out[i] = f0[i] + (1.0 - f0[i]) / 21.0;
    }
    out
}

/// Image-based multi-scatter lobe `FmsEms` from a split-sum `(scale, bias)` pair.
///
/// `scale + bias` is the F=1 directional albedo. A smooth surface (`Ess ≈ 1`)
/// returns approximately 0. A rough metal returns a positive lobe.
#[must_use]
pub fn multi_scatter_lobe(f0: [f32; 3], scale: f32, bias: f32) -> [f32; 3] {
    let ess = (scale + bias).clamp(0.0, 1.0);
    let favg = fresnel_average(f0);
    let ems = 1.0 - ess;
    let mut out = [0.0; 3];
    for i in 0..3 {
        let fss = f0[i] * scale + bias;
        let denom = (1.0 - favg[i] * ems).max(1.0e-4);
        out[i] = fss * favg[i] * ems / denom;
    }
    out
}

/// Punctual multi-scatter energy compensation (Turquin 2019): the factor on the single-scatter
/// GGX lobe, `1 + Fms (1 − E(μo)) / E(μo)`, with `e_o` the lobe's directional albedo at n·v for
/// F = 1 (the DFG table's B channel) and `Fms = Favg E_avg / (1 − Favg (1 − E_avg))` the share of
/// the missing energy that survives the further bounces, each attenuated by the average Fresnel
/// (Kulla & Conty 2017), with `e_avg` the lobe's hemispherical average (the table's A channel).
/// A white conductor then reflects everything: `E · (1 + (1 − E) / E) = 1`. Mirrors
/// `pbr_lighting.wgsl::energy_compensation`.
#[must_use]
pub fn energy_compensation(f0: [f32; 3], e_o: f32, e_avg: f32) -> [f32; 3] {
    let e = e_o.clamp(0.05, 1.0);
    let ea = e_avg.clamp(0.05, 1.0);
    fresnel_average(f0).map(|fa| 1.0 + fa * ea / (1.0 - fa * (1.0 - ea)) * (1.0 - e) / e)
}

/// The diffuse weight under the specular layer with multi-scatter on: `1 − (F0 · scale + bias)`
/// at n·v, the share of the light the specular layer does not reflect toward the eye (albedo
/// scaling). Mirrors `pbr_brdf.wgsl::with_multi_scatter`.
#[must_use]
pub fn diffuse_under_specular(f0: [f32; 3], scale: f32, bias: f32) -> [f32; 3] {
    f0.map(|f| 1.0 - (f * scale + bias).clamp(0.0, 1.0))
}

/// Edge-wear mask. `sample` is curvature (0.5 flat) or ambient occlusion.
#[must_use]
pub fn edge_wear_mask(sample: f32, bias: f32, contrast: f32, intensity: f32) -> f32 {
    ((sample - bias) * contrast).clamp(0.0, 1.0) * intensity
}

/// Apply an edge-wear mask to albedo, roughness, and metallic.
#[must_use]
pub fn apply_edge_wear(
    albedo: [f32; 3],
    roughness: f32,
    metallic: f32,
    mask: f32,
    tint: [f32; 3],
    roughness_bias: f32,
    metallic_bias: f32,
) -> ([f32; 3], f32, f32) {
    let mut out = [0.0; 3];
    for i in 0..3 {
        out[i] = albedo[i] * (1.0 - mask) + tint[i] * mask;
    }
    let rough = (roughness + roughness_bias * mask).clamp(0.0, 1.0);
    let metal = (metallic + metallic_bias * mask).clamp(0.0, 1.0);
    (out, rough, metal)
}

/// Parallax step count. Grazing views (`|view_z|` near 0) use `max_steps`; facing views use `min_steps`.
///
/// `|view_z|` is clamped to 1, so the caller must pass a unit view. An unnormalized
/// view whose Z is the camera distance saturates here and sticks at `min_steps`.
#[must_use]
pub fn parallax_step_count(view_ts_z: f32, min_steps: f32, max_steps: f32) -> u32 {
    let max_steps = max_steps.clamp(1.0, 16.0);
    let min_steps = min_steps.clamp(1.0, max_steps);
    let facing = view_ts_z.abs().clamp(0.0, 1.0);
    let steps_f = (max_steps * (1.0 - facing) + min_steps * facing).clamp(1.0, 16.0);
    (steps_f + 0.5).floor().clamp(1.0, 16.0) as u32
}

/// Unit tangent-space view. The step count tracks the angle only after this.
#[must_use]
pub fn unit_tangent_view(view_ts: [f32; 3]) -> [f32; 3] {
    let len = (view_ts[0] * view_ts[0] + view_ts[1] * view_ts[1] + view_ts[2] * view_ts[2])
        .sqrt()
        .max(1.0e-8);
    [view_ts[0] / len, view_ts[1] / len, view_ts[2] / len]
}

/// Parallax occlusion UV. `height_at` returns 1 on the geometric surface and 0 at maximum depth.
///
/// A height of 1 returns `uv`. The march is capped at 16 steps.
#[must_use]
pub fn parallax_uv(
    uv: [f32; 2],
    view_ts: [f32; 3],
    scale: f32,
    min_steps: f32,
    max_steps: f32,
    height_at: impl Fn([f32; 2]) -> f32,
) -> [f32; 2] {
    if scale <= 1.0e-5 {
        return uv;
    }
    let vz = view_ts[2].abs().max(0.15);
    let max_steps = max_steps.clamp(1.0, 16.0);
    let min_steps = min_steps.clamp(1.0, max_steps);
    let facing = view_ts[2].abs().clamp(0.0, 1.0);
    let steps_f = (max_steps * (1.0 - facing) + min_steps * facing).clamp(1.0, 16.0);
    let steps = (steps_f + 0.5).floor().clamp(1.0, 16.0) as u32;
    // The layer depth and the UV delta divide by the rounded step count: with
    // the unrounded count the march ends at steps/steps_f, which is 1 only when
    // steps_f is integral, and a flat deep height then overshoots (or never
    // reaches depth 1 and returns the end UV short of the surface).
    let count = steps as f32;
    let layer = 1.0 / count;
    let delta = [view_ts[0] / vz * scale / count, view_ts[1] / vz * scale / count];
    let mut current_uv = uv;
    let mut current_depth = 0.0;
    let mut prev_uv = uv;
    let mut prev_depth = 0.0;
    let mut hit = false;
    for i in 0..16 {
        if i >= steps || hit {
            continue;
        }
        let h = height_at(current_uv);
        // Height 1 is the geometric surface. The ray starts at depth 0, so a
        // flat map hits before it moves; a deeper sample waits until the ray
        // catches it.
        if h >= 1.0 - current_depth {
            hit = true;
        } else {
            prev_uv = current_uv;
            prev_depth = current_depth;
            current_uv = [current_uv[0] - delta[0], current_uv[1] - delta[1]];
            current_depth += layer;
        }
    }
    if !hit {
        return current_uv;
    }
    let h_after = height_at(current_uv);
    let h_before = height_at(prev_uv);
    let da = prev_depth - (1.0 - h_before);
    let db = current_depth - (1.0 - h_after);
    let denom = da - db;
    let t = if denom.abs() > 1.0e-5 {
        (da / denom).clamp(0.0, 1.0)
    } else {
        0.0
    };
    [
        prev_uv[0] * (1.0 - t) + current_uv[0] * t,
        prev_uv[1] * (1.0 - t) + current_uv[1] * t,
    ]
}

fn channel(tex: &LabTexture, u: f32, v: f32, channel: usize) -> u8 {
    let x = ((u.clamp(0.0, 0.999) * tex.width as f32) as u32).min(tex.width.saturating_sub(1));
    let y = ((v.clamp(0.0, 0.999) * tex.height as f32) as u32).min(tex.height.saturating_sub(1));
    let i = ((y * tex.width + x) * 4 + channel as u32) as usize;
    tex.rgba.get(i).copied().unwrap_or(0)
}

fn image_from(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> LabTexture {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&pixel(x, y));
        }
    }
    LabTexture { width, height, rgba }
}

/// Pack detail albedo (RGB) and roughness (A). `None` when both inputs are absent.
///
/// The primary image sets the size. The other is nearest-resampled. A missing
/// channel is 128, so an absent albedo does not tint (`128/255 * 2 ≈ 1`).
#[must_use]
pub fn pack_detail_image(albedo: Option<&LabTexture>, roughness: Option<&LabTexture>) -> Option<LabTexture> {
    let primary = albedo.or(roughness)?;
    let width = primary.width.max(1);
    let height = primary.height.max(1);
    Some(image_from(width, height, |x, y| {
        let u = (x as f32 + 0.5) / width as f32;
        let v = (y as f32 + 0.5) / height as f32;
        let rgb = if let Some(alb) = albedo {
            [
                channel(alb, u, v, 0),
                channel(alb, u, v, 1),
                channel(alb, u, v, 2),
            ]
        } else {
            [128, 128, 128]
        };
        let rough = roughness.map(|tex| channel(tex, u, v, 0)).unwrap_or(128);
        [rgb[0], rgb[1], rgb[2], rough]
    }))
}

/// Pack a detail normal (RGB) and mask (A). `None` when both inputs are absent.
///
/// A missing normal is flat `(128, 128, 255)`. A missing mask is white.
#[must_use]
pub fn pack_detail_normal(normal: Option<&LabTexture>, mask: Option<&LabTexture>) -> Option<LabTexture> {
    let primary = normal.or(mask)?;
    let width = primary.width.max(1);
    let height = primary.height.max(1);
    Some(image_from(width, height, |x, y| {
        let u = (x as f32 + 0.5) / width as f32;
        let v = (y as f32 + 0.5) / height as f32;
        let rgb = if let Some(n) = normal {
            [channel(n, u, v, 0), channel(n, u, v, 1), channel(n, u, v, 2)]
        } else {
            [128, 128, 255]
        };
        let mask_r = mask.map(|tex| channel(tex, u, v, 0)).unwrap_or(255);
        [rgb[0], rgb[1], rgb[2], mask_r]
    }))
}

/// Pack height (R, 255 = geometric surface) and curvature (G, 128 = flat).
#[must_use]
pub fn pack_aux(height: Option<&LabTexture>, curvature: Option<&LabTexture>) -> Option<LabTexture> {
    let primary = height.or(curvature)?;
    let width = primary.width.max(1);
    let height_px = primary.height.max(1);
    Some(image_from(width, height_px, |x, y| {
        let u = (x as f32 + 0.5) / width as f32;
        let v = (y as f32 + 0.5) / height_px as f32;
        let h = height.map(|tex| channel(tex, u, v, 0)).unwrap_or(255);
        let c = curvature.map(|tex| channel(tex, u, v, 0)).unwrap_or(128);
        [h, c, 0, 255]
    }))
}

/// Pixels for the hard-surface slots. `.material.ron` stores paths, not images.
///
/// The mesh bind group packs these into the detail, detail-normal, and height
/// textures. An empty slot keeps that texture's neutral fallback.
#[derive(Clone, Debug, Default)]
pub struct AuthoredSurfaceMaps {
    /// Detail albedo, centred on mid-grey.
    pub detail_albedo: Option<LabTexture>,
    /// Detail normal, tangent space.
    pub detail_normal: Option<LabTexture>,
    /// Detail roughness in R.
    pub detail_roughness: Option<LabTexture>,
    /// Detail blend mask in R.
    pub detail_mask: Option<LabTexture>,
    /// Parallax height in R. 255 is the geometric surface.
    pub height_map: Option<LabTexture>,
    /// Curvature in R. 128 is flat.
    pub curvature_map: Option<LabTexture>,
    /// Thin film coverage in R.
    pub thin_film_coverage: Option<LabTexture>,
    /// Thin film thickness in G: 0 is `thickness_min`, 255 `thickness_max`.
    pub thin_film_thickness: Option<LabTexture>,
}

impl LabMaterial {
    /// Copy authored scalars onto a lab material. Texture slots stay empty.
    ///
    /// A path in `.material.ron` is not pixels. [`Self::from_authored_with_maps`]
    /// copies images the caller has already resolved, and the mesh bind group
    /// uploads those three packed textures. `max_steps` above 16 is clamped.
    #[must_use]
    pub fn from_authored(asset: &MaterialAssetRon) -> Self {
        let mut mat = Self::EMPTY;
        mat.base_color = asset.base.base_color;
        mat.metallic = asset.base.metallic;
        mat.roughness = asset.base.roughness;
        mat.emissive = asset.base.emissive;
        mat.emissive_intensity = asset.base.emissive_strength.max(0.0);
        mat.ior = asset.base.ior;
        mat.alpha_mode = match asset.render_state.alpha_mode {
            AssetAlpha::Opaque => ALPHA_OPAQUE,
            AssetAlpha::Mask => ALPHA_MASK,
            AssetAlpha::Blend => ALPHA_BLEND,
        };
        mat.alpha_cutoff = asset.render_state.alpha_cutoff;
        mat.double_sided = asset.render_state.cull_mode == Some(regia_asset::CullMode::None);
        if let Some(t) = &asset.transmission
            && t.factor > 0.0
        {
            mat.transmission = t.factor.clamp(0.0, 1.0);
            mat.transmission_tint =
                crate::volume_transmittance(t.thickness, t.attenuation_color, t.attenuation_distance);
            mat.transmission_thickness = t.thickness.max(0.0);
            mat.transmission_roughness = asset.transmission_roughness.unwrap_or(0.0);
            mat.alpha_mode = ALPHA_PREMULTIPLIED;
        }
        if let Some(film) = &asset.thin_film {
            mat.thin_film = crate::ThinFilmFactors {
                factor: film.factor,
                ior: film.ior,
                thickness_min: film.thickness_min,
                thickness_max: film.thickness_max,
            };
        }
        if let Some(weather) = &asset.weather {
            // `.material.ron` v5 (spec 99 § 7.3); `None` keeps `WeatherResponse::DEFAULT`.
            mat.weather = crate::WeatherResponse {
                porosity: weather.porosity,
                wet: weather.wet,
                snow: weather.snow,
                puddles: weather.puddles,
            };
        }
        if let Some(block) = &asset.subsurface {
            mat.subsurface = crate::SubsurfaceFactors {
                profile: match block.profile {
                    regia_asset::SubsurfaceProfileKind::Skin => crate::SubsurfaceProfile::Skin,
                    regia_asset::SubsurfaceProfileKind::Foliage => crate::SubsurfaceProfile::Foliage,
                },
                radius_mm: block.radius,
                tint: block.tint,
            };
        }
        if let Some(ClearcoatParams {
            factor, roughness, ..
        }) = asset.clearcoat
        {
            mat.hard_surface.clearcoat = factor;
            mat.hard_surface.clearcoat_roughness = roughness;
        }
        if let Some(AnisotropyParams { strength, direction }) = asset.anisotropy {
            mat.hard_surface.anisotropy = strength;
            mat.hard_surface.anisotropy_rotation = direction[1].atan2(direction[0]);
        }
        if let Some(hs) = &asset.hard_surface {
            mat.hard_surface.multi_scatter = if hs.multi_scatter { 1.0 } else { 0.0 };
            mat.hard_surface.specular_occlusion = if hs.specular_occlusion { 1.0 } else { 0.0 };
            if let Some(detail) = &hs.detail {
                mat.hard_surface.detail_albedo_strength = detail.albedo_strength;
                mat.hard_surface.detail_normal_strength = detail.normal_strength;
                mat.hard_surface.detail_roughness_strength = detail.roughness_strength;
                mat.hard_surface.detail_uv_scale = detail.uv_scale;
                mat.hard_surface.detail_fade_start = detail.fade_start;
                mat.hard_surface.detail_fade_end = detail.fade_end;
            }
            if let Some(pom) = &hs.parallax {
                mat.hard_surface.pom_scale = pom.scale;
                let max_steps = (pom.max_steps as f32).clamp(1.0, 16.0);
                let min_steps = (pom.min_steps as f32).clamp(1.0, max_steps);
                mat.hard_surface.pom_min_steps = min_steps;
                mat.hard_surface.pom_max_steps = max_steps;
            }
            if let Some(wear) = &hs.edge_wear {
                mat.hard_surface.edge_intensity = wear.intensity;
                mat.hard_surface.edge_roughness_bias = wear.roughness_bias;
                mat.hard_surface.edge_metallic_bias = wear.metallic_bias;
                mat.hard_surface.edge_source = match wear.source {
                    EdgeWearSource::Curvature => 1.0,
                    EdgeWearSource::Ao => 2.0,
                };
                mat.hard_surface.edge_tint = wear.tint;
                mat.hard_surface.edge_bias = wear.bias;
                mat.hard_surface.edge_contrast = wear.contrast;
            }
        }
        mat
    }

    /// [`from_authored`](Self::from_authored) plus the resolved detail, height, and curvature images.
    ///
    /// The frame renderer packs and binds these on the mesh material group. Passing
    /// `AuthoredSurfaceMaps::default()` is the same as [`from_authored`](Self::from_authored).
    #[must_use]
    pub fn from_authored_with_maps(asset: &MaterialAssetRon, maps: AuthoredSurfaceMaps) -> Self {
        let mut mat = Self::from_authored(asset);
        mat.detail_albedo = maps.detail_albedo;
        mat.detail_normal = maps.detail_normal;
        mat.detail_roughness = maps.detail_roughness;
        mat.detail_mask = maps.detail_mask;
        mat.height_map = maps.height_map;
        mat.curvature_map = maps.curvature_map;
        mat.thin_film_map = crate::pack_thin_film_map(
            maps.thin_film_coverage.as_ref(),
            maps.thin_film_thickness.as_ref(),
        );
        mat
    }
}

/// Base images a host resolved for a material's texture slots (`base_color_map`,
/// `normal_map`, `orm_map`, `emissive_map`). `None` means the slot is empty or did not
/// resolve.
#[derive(Clone, Debug, Default)]
pub struct ResolvedBaseMaps {
    /// Base colour (sRGB).
    pub albedo: Option<LabTexture>,
    /// Tangent-space normal.
    pub normal: Option<LabTexture>,
    /// The packed ORM image (occlusion R, roughness G, metallic B).
    pub occlusion: Option<LabTexture>,
    /// Emissive colour (sRGB).
    pub emissive: Option<LabTexture>,
}

impl ResolvedBaseMaps {
    /// Each image box-filtered down to the active tier's resident cap
    /// ([`crate::texture_budget::resident_cap`]): pixels above it are never uploaded, so they are
    /// not kept in RAM either.
    #[must_use]
    pub fn fit_to_resident_cap(self) -> Self {
        let cap = crate::texture_budget::resident_cap();
        if cap == 0 {
            return self;
        }
        let fit =
            |t: Option<LabTexture>, srgb: bool| t.map(|t| crate::texture_budget::fit_texture(t, cap, srgb));
        Self {
            albedo: fit(self.albedo, true),
            normal: fit(self.normal, false),
            occlusion: fit(self.occlusion, false),
            emissive: fit(self.emissive, true),
        }
    }
}

/// Asset names already warned about (one warning per asset per process).
fn warn_missing_once(asset: &str, slot: &str) {
    static WARNED: std::sync::Mutex<Option<std::collections::BTreeSet<String>>> = std::sync::Mutex::new(None);
    let Ok(mut warned) = WARNED.lock() else {
        return;
    };
    let key = format!("{asset}/{slot}");
    if warned.get_or_insert_with(Default::default).insert(key) {
        tracing::warn!(
            target: "regia_render",
            asset,
            slot,
            "material texture did not resolve; drawing the neutral fallback material \
             (grey 0.5, roughness 0.7, no metal, flat normal)"
        );
    }
}

impl LabMaterial {
    /// The material a mesh draws with when its base colour texture is missing or did not
    /// resolve: mid grey (0.5), roughness 0.7, no metal, flat normal, no emission. Neutral
    /// on purpose: the authored colour factor alone (often a tint meant to multiply the
    /// texture) is not what the artist meant to see.
    #[must_use]
    pub fn neutral_fallback() -> Self {
        Self {
            base_color: [0.5, 0.5, 0.5, 1.0],
            metallic: 0.0,
            roughness: 0.7,
            ..Self::EMPTY
        }
    }

    /// [`from_authored_with_maps`](Self::from_authored_with_maps) plus the base images the
    /// host resolved. A slot the asset names that did not resolve is handled here:
    /// a missing base colour map makes the whole material [`Self::neutral_fallback`]; a
    /// missing normal map leaves the normal flat. Each case warns once, naming `asset_name`.
    #[must_use]
    pub fn from_authored_resolved(
        asset_name: &str,
        asset: &MaterialAssetRon,
        base: ResolvedBaseMaps,
        maps: AuthoredSurfaceMaps,
    ) -> Self {
        if asset.base.base_color_map.is_some() && base.albedo.is_none() {
            warn_missing_once(asset_name, "base_color_map");
            return Self::neutral_fallback();
        }
        let base = base.fit_to_resident_cap();
        let mut mat = Self::from_authored_with_maps(asset, maps);
        mat.albedo = base.albedo;
        if asset.base.normal_map.is_some() && base.normal.is_none() {
            warn_missing_once(asset_name, "normal_map");
        }
        // A resolved normal map applies at full strength (glTF `normalTexture.scale` 1).
        mat.normal_scale = if base.normal.is_some() { 1.0 } else { 0.0 };
        mat.normal_map = base.normal;
        // `orm_map` is the packed ORM (occlusion R, roughness G, metallic B) unless the material
        // says it carries occlusion only (`orm_packed: false`).
        mat.orm_packed = asset.base.orm_packed && asset.base.orm_map.is_some() && base.occlusion.is_some();
        mat.occlusion_map = base.occlusion;
        mat.emissive_map = base.emissive;
        crate::texture_budget::name_material_textures(&mat, asset_name);
        mat
    }

    /// The material a cooked pak draws: [`from_authored_resolved`](Self::from_authored_resolved)
    /// where each slot may resolve to an RGBA8 image (`base`, from a `CookedImage`) or to a
    /// cooked block-compressed chain (`compressed`, from a `CookedTexture`; uploaded as it is, or
    /// decoded on a device that cannot sample it). A slot counts as resolved when either is
    /// present; the missing-slot rules are the same.
    #[must_use]
    pub fn from_cooked(
        asset_name: &str,
        asset: &MaterialAssetRon,
        base: ResolvedBaseMaps,
        compressed: crate::CompressedMaps,
    ) -> Self {
        let has = [
            base.albedo.is_some() || compressed.albedo.is_some(),
            base.normal.is_some() || compressed.normal.is_some(),
            base.occlusion.is_some() || compressed.occlusion.is_some(),
        ];
        if asset.base.base_color_map.is_some() && !has[0] {
            warn_missing_once(asset_name, "base_color_map");
            return Self::neutral_fallback();
        }
        let base = base.fit_to_resident_cap();
        let compressed = compressed.fit_to_resident_cap();
        let mut mat = Self::from_authored(asset);
        mat.albedo = base.albedo;
        if asset.base.normal_map.is_some() && !has[1] {
            warn_missing_once(asset_name, "normal_map");
        }
        mat.normal_scale = if has[1] { 1.0 } else { 0.0 };
        mat.normal_map = base.normal;
        mat.orm_packed = asset.base.orm_packed && asset.base.orm_map.is_some() && has[2];
        mat.occlusion_map = base.occlusion;
        mat.emissive_map = base.emissive;
        if compressed != crate::CompressedMaps::default() {
            mat.compressed = Some(std::sync::Arc::new(compressed));
        }
        crate::texture_budget::name_material_textures(&mat, asset_name);
        mat
    }
}

fn rgba_image(size: u32, pixel: impl Fn(f32, f32) -> [u8; 4]) -> LabTexture {
    image_from(size, size, |x, y| {
        let u = (x as f32 + 0.5) / size as f32;
        let v = (y as f32 + 0.5) / size as f32;
        pixel(u, v)
    })
}

fn encode_normal(x: f32, y: f32, z: f32) -> [u8; 4] {
    let enc = |n: f32| -> u8 { ((n.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0).round() as u8 };
    [enc(x), enc(y), enc(z), 255]
}

fn checker(size: u32, lo: u8, hi: u8) -> LabTexture {
    rgba_image(size, |u, v| {
        let on = ((u * 8.0) as u32 + (v * 8.0) as u32).is_multiple_of(2);
        let g = if on { hi } else { lo };
        [g, g, g, 255]
    })
}

fn ridge_normal(size: u32) -> LabTexture {
    rgba_image(size, |u, _v| {
        let s = (u * std::f32::consts::TAU * 8.0).sin() * 0.65;
        let z = (1.0 - s * s).sqrt();
        encode_normal(s, 0.0, z)
    })
}

/// A soft cavity on the camera-facing part of a UV sphere (`u ≈ 0.25` is +Z).
fn facing_cavity(size: u32) -> LabTexture {
    rgba_image(size, |u, v| {
        let d = ((u - 0.25).powi(2) + (v - 0.5).powi(2)).sqrt();
        let t = ((d - 0.035) / 0.07).clamp(0.0, 1.0);
        let ao = (78.0 + t * 177.0).round() as u8;
        [ao, ao, ao, 255]
    })
}

fn panel_height(size: u32) -> LabTexture {
    rgba_image(size, |u, v| {
        let groove = |t: f32| {
            let f = (t * 3.0).fract();
            f < 0.16 || f > 0.84
        };
        let bolt = (u - 0.5).abs() < 0.04 && (v - 0.5).abs() < 0.04;
        let h = if bolt {
            20
        } else if groove(u) || groove(v) {
            40
        } else {
            255
        };
        [h, h, h, 255]
    })
}

fn border_curvature(size: u32) -> LabTexture {
    curvature_rim(size, 0.12)
}

/// Convex rim. `width` is the UV distance from the face border that counts as an edge.
fn curvature_rim(size: u32, width: f32) -> LabTexture {
    rgba_image(size, |u, v| {
        let edge = u.min(1.0 - u).min(v.min(1.0 - v));
        let c = if edge < width { 235 } else { 40 };
        [c, c, c, 255]
    })
}

fn stripe_albedo(size: u32) -> LabTexture {
    rgba_image(size, |u, v| {
        let on = ((u * 10.0) as u32 + (v * 10.0) as u32).is_multiple_of(2);
        if on {
            [240, 240, 240, 255]
        } else {
            [15, 15, 15, 255]
        }
    })
}

/// Panel paint: white body, dark seams, two bolt heads. The base colour tints the body.
fn panel_albedo(size: u32) -> LabTexture {
    rgba_image(size, |u, v| {
        let groove = |t: f32| {
            let f = (t * 4.0).fract();
            f < 0.035 || f > 0.965
        };
        let bolt = (u - 0.22).hypot(v - 0.28) < 0.045 || (u - 0.78).hypot(v - 0.72) < 0.045;
        if bolt {
            [28, 30, 34, 255]
        } else if groove(u) || groove(v) {
            [12, 14, 18, 255]
        } else {
            [255, 255, 255, 255]
        }
    })
}

/// Fine flake around mid-grey. A harsh checker reads as a debug texture, not paint.
fn paint_grain(size: u32) -> LabTexture {
    rgba_image(size, |u, v| {
        let n = ((u * 47.0).sin() * (v * 31.0).cos()).mul_add(0.5, 0.5);
        let g = (112.0 + n * 32.0).round() as u8;
        [g, g, g, 255]
    })
}

fn flake_normal(size: u32) -> LabTexture {
    rgba_image(size, |u, v| {
        let s = (u * std::f32::consts::TAU * 11.0).sin() * 0.22;
        let t = (v * std::f32::consts::TAU * 7.0).cos() * 0.14;
        let z = (1.0 - s * s - t * t).max(0.2).sqrt();
        encode_normal(s, t, z)
    })
}

/// Daylight automotive blue. Dielectric, mid roughness, sharp clearcoat when `coated`.
fn car_paint(coated: bool) -> LabMaterial {
    let mut mat = LabMaterial::EMPTY;
    mat.base_color = [0.07, 0.16, 0.52, 1.0];
    mat.metallic = 0.0;
    mat.roughness = 0.42;
    mat.hard_surface.multi_scatter = 1.0;
    if coated {
        mat.hard_surface.clearcoat = 1.0;
        mat.hard_surface.clearcoat_roughness = 0.04;
    }
    mat
}

fn feature_material(feature: HardSurfaceFeature, enabled: bool) -> LabMaterial {
    let mut mat = LabMaterial::EMPTY;
    match feature {
        HardSurfaceFeature::Detail => {
            mat.base_color = [0.55, 0.56, 0.58, 1.0];
            mat.roughness = 0.45;
            mat.detail_albedo = Some(checker(64, 64, 191));
            mat.detail_normal = Some(ridge_normal(64));
            mat.detail_roughness = Some(checker(64, 40, 220));
            mat.detail_mask = Some(checker(64, 255, 255));
            if enabled {
                mat.hard_surface.detail_albedo_strength = 1.0;
                mat.hard_surface.detail_normal_strength = 1.0;
                mat.hard_surface.detail_roughness_strength = 1.0;
                mat.hard_surface.detail_uv_scale = [8.0, 8.0];
            }
        }
        HardSurfaceFeature::Clearcoat => {
            mat = car_paint(enabled);
        }
        HardSurfaceFeature::Anisotropy => {
            mat.base_color = [0.78, 0.78, 0.80, 1.0];
            mat.roughness = 0.38;
            mat.metallic = 1.0;
            if enabled {
                mat.hard_surface.anisotropy = 0.92;
            }
        }
        HardSurfaceFeature::SpecularOcclusion => {
            mat.base_color = [0.86, 0.87, 0.90, 1.0];
            mat.roughness = 0.25;
            mat.metallic = 1.0;
            mat.occlusion_strength = 1.0;
            mat.occlusion_map = Some(facing_cavity(128));
            if enabled {
                mat.hard_surface.specular_occlusion = 1.0;
            }
        }
        HardSurfaceFeature::MultiScatter => {
            mat.base_color = [0.76, 0.40, 0.14, 1.0];
            mat.roughness = 0.72;
            mat.metallic = 1.0;
            if enabled {
                mat.hard_surface.multi_scatter = 1.0;
            }
        }
        HardSurfaceFeature::Parallax => {
            mat.base_color = [0.62, 0.64, 0.68, 1.0];
            mat.roughness = 0.5;
            mat.albedo = Some(stripe_albedo(64));
            mat.height_map = Some(panel_height(64));
            if enabled {
                mat.hard_surface.pom_scale = 0.16;
                mat.hard_surface.pom_min_steps = 4.0;
                mat.hard_surface.pom_max_steps = 16.0;
            }
        }
        HardSurfaceFeature::EdgeWear => {
            mat.base_color = [0.10, 0.22, 0.55, 1.0];
            mat.roughness = 0.45;
            mat.curvature_map = Some(border_curvature(64));
            if enabled {
                mat.hard_surface.edge_intensity = 1.0;
                mat.hard_surface.edge_source = 1.0;
                mat.hard_surface.edge_metallic_bias = 1.0;
                mat.hard_surface.edge_roughness_bias = -0.3;
                mat.hard_surface.edge_tint = [0.82, 0.83, 0.86];
                mat.hard_surface.edge_bias = 0.5;
                mat.hard_surface.edge_contrast = 6.0;
            }
        }
    }
    mat
}

fn push_face(vertices: &mut Vec<LabVertex>, indices: &mut Vec<u32>, corners: [([f32; 3], [f32; 2]); 4]) {
    let p0 = Vec3::from_array(corners[0].0);
    let p1 = Vec3::from_array(corners[1].0);
    let p3 = Vec3::from_array(corners[3].0);
    let edge_u = p1 - p0;
    let edge_v = p3 - p0;
    let normal = edge_u.cross(edge_v).normalize_or_zero();
    let tangent = edge_u.normalize_or_zero();
    let w = normal.cross(tangent).dot(edge_v.normalize_or_zero()).signum();
    let tangent = [tangent.x, tangent.y, tangent.z, if w == 0.0 { 1.0 } else { w }];
    let base = vertices.len() as u32;
    for (position, uv) in corners {
        vertices.push(LabVertex {
            position,
            normal: normal.to_array(),
            uv,
            tangent,
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        });
    }
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// A box with a real tangent frame on each face (panel lines and brushed metal).
#[must_use]
pub fn panel_hull(half: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    let (hx, hy, hz) = (half[0].abs(), half[1].abs(), half[2].abs());
    let faces = [
        [
            ([hx, -hy, hz], [0.0, 1.0]),
            ([hx, -hy, -hz], [1.0, 1.0]),
            ([hx, hy, -hz], [1.0, 0.0]),
            ([hx, hy, hz], [0.0, 0.0]),
        ],
        [
            ([-hx, -hy, -hz], [0.0, 1.0]),
            ([-hx, -hy, hz], [1.0, 1.0]),
            ([-hx, hy, hz], [1.0, 0.0]),
            ([-hx, hy, -hz], [0.0, 0.0]),
        ],
        [
            ([-hx, hy, hz], [0.0, 1.0]),
            ([hx, hy, hz], [1.0, 1.0]),
            ([hx, hy, -hz], [1.0, 0.0]),
            ([-hx, hy, -hz], [0.0, 0.0]),
        ],
        [
            ([-hx, -hy, -hz], [0.0, 1.0]),
            ([hx, -hy, -hz], [1.0, 1.0]),
            ([hx, -hy, hz], [1.0, 0.0]),
            ([-hx, -hy, hz], [0.0, 0.0]),
        ],
        [
            ([-hx, -hy, hz], [0.0, 1.0]),
            ([hx, -hy, hz], [1.0, 1.0]),
            ([hx, hy, hz], [1.0, 0.0]),
            ([-hx, hy, hz], [0.0, 0.0]),
        ],
        [
            ([hx, -hy, -hz], [0.0, 1.0]),
            ([-hx, -hy, -hz], [1.0, 1.0]),
            ([-hx, hy, -hz], [1.0, 0.0]),
            ([hx, hy, -hz], [0.0, 0.0]),
        ],
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for face in faces {
        push_face(&mut vertices, &mut indices, face);
    }
    (vertices, indices)
}

fn placed(vertices: Vec<LabVertex>, indices: Vec<u32>, material: usize, offset: [f32; 3]) -> LabMesh {
    let mut mesh = LabMesh::unskinned();
    mesh.vertices = vertices;
    mesh.indices = indices;
    mesh.material = material;
    mesh.model = Mat4::from_translation(Vec3::from_array(offset)).to_cols_array_2d();
    mesh
}

/// Width of the hard-surface goldens.
pub const DEMO_WIDTH: u32 = 960;
/// Height of the hard-surface goldens.
pub const DEMO_HEIGHT: u32 = 540;
/// Daylight EV100 for the goldens. A 100 klx sun sits near the middle of AgX here.
pub const DEMO_EV: f32 = 14.5;

fn sunny_scene(width: u32, height: u32, eye: [f32; 3], target: [f32; 3]) -> LabScene {
    LabScene {
        width,
        height,
        eye,
        target,
        fov_y: 0.62,
        near: 0.05,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: DEMO_EV,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: None,
        directional: crate::DirectionalLightUniform::new([-0.35, -0.75, -0.55], [1.0, 0.98, 0.95], 100_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    }
}

/// One camera for every shot: slightly off axis, so a coat glint, a brushed streak,
/// and a parallax shift all land on screen, and the three spheres stay inside the frame.
fn demo_view() -> ([f32; 3], [f32; 3]) {
    ([0.72, 0.52, 1.85], [0.0, 0.30, 0.05])
}

fn push_sphere_row(scene: &mut LabScene, material: usize) {
    let (sphere_v, sphere_i) = uv_sphere(0.20, 32, 20);
    for x in [-0.62_f32, 0.0, 0.62] {
        scene.meshes.push(placed(
            sphere_v.clone(),
            sphere_i.clone(),
            material,
            [x, 0.22, 0.42],
        ));
    }
}

fn push_hull(scene: &mut LabScene, material: usize, half: [f32; 3], offset: [f32; 3]) {
    let (hull_v, hull_i) = panel_hull(half);
    scene.meshes.push(placed(hull_v, hull_i, material, offset));
}

/// Sphere row in front of a panelled hull. 960×540 at [`DEMO_EV`].
#[must_use]
pub fn feature_demo_scene(feature: HardSurfaceFeature, enabled: bool) -> LabScene {
    let (eye, target) = demo_view();
    let mut scene = sunny_scene(DEMO_WIDTH, DEMO_HEIGHT, eye, target);
    scene.materials.push(feature_material(feature, enabled));
    push_hull(&mut scene, 0, [0.78, 0.24, 0.36], [0.0, 0.26, -0.22]);
    push_sphere_row(&mut scene, 0);
    scene
}

/// Quality for [`feature_demo_scene`]. Parallax turns the profile gate on; the lab baseline stays off.
#[must_use]
pub fn feature_demo_quality(feature: HardSurfaceFeature) -> FrameQuality {
    let mut quality = FrameQuality::LAB;
    quality.parallax_occlusion = feature == HardSurfaceFeature::Parallax;
    quality
}

fn hull_materials(coated: bool) -> [LabMaterial; 2] {
    let mut hull = car_paint(coated);
    hull.albedo = Some(panel_albedo(128));
    hull.detail_albedo = Some(paint_grain(64));
    hull.detail_normal = Some(flake_normal(64));
    hull.detail_mask = Some(checker(64, 255, 255));
    hull.height_map = Some(panel_height(64));
    hull.curvature_map = Some(curvature_rim(128, 0.06));
    hull.hard_surface.detail_albedo_strength = 0.28;
    hull.hard_surface.detail_normal_strength = 0.22;
    hull.hard_surface.detail_uv_scale = [8.0, 8.0];
    hull.hard_surface.detail_fade_start = 6.0;
    hull.hard_surface.detail_fade_end = 24.0;
    hull.hard_surface.specular_occlusion = 1.0;
    hull.hard_surface.pom_scale = 0.08;
    hull.hard_surface.pom_min_steps = 4.0;
    hull.hard_surface.pom_max_steps = 16.0;
    hull.hard_surface.edge_intensity = 0.9;
    hull.hard_surface.edge_source = 1.0;
    hull.hard_surface.edge_metallic_bias = 0.85;
    hull.hard_surface.edge_roughness_bias = -0.22;
    hull.hard_surface.edge_tint = [0.80, 0.81, 0.84];
    hull.hard_surface.edge_bias = 0.55;
    hull.hard_surface.edge_contrast = 5.0;
    [hull, car_paint(coated)]
}

fn hull_scene(coated: bool) -> LabScene {
    // Lower than the feature camera so the front of the hull, not the roof, fills the shot.
    let mut scene = sunny_scene(DEMO_WIDTH, DEMO_HEIGHT, [0.95, 0.34, 1.70], [0.0, 0.26, 0.10]);
    let [hull, sphere] = hull_materials(coated);
    scene.materials.push(hull);
    scene.materials.push(sphere);
    push_hull(&mut scene, 0, [0.78, 0.24, 0.36], [0.0, 0.26, -0.22]);
    push_sphere_row(&mut scene, 1);
    scene
}

/// Close view of a painted, panelled hull and a row of clearcoat spheres.
#[must_use]
pub fn hull_closeup_scene() -> LabScene {
    hull_scene(true)
}

/// The same close-up with the clearcoat lobe removed.
#[must_use]
pub fn hull_closeup_uncoated() -> LabScene {
    hull_scene(false)
}

/// [`FrameQuality::LAB`] plus the parallax profile gate, for the hull close-up and cost pairs.
#[must_use]
pub fn hull_closeup_quality() -> FrameQuality {
    let mut quality = FrameQuality::LAB;
    quality.parallax_occlusion = true;
    quality
}

/// A grid of hulls used to time one feature on the GPU.  The off scene is the same grid.
#[must_use]
pub fn feature_cost_scene(feature: HardSurfaceFeature, enabled: bool, width: u32, height: u32) -> LabScene {
    let mut scene = sunny_scene(width, height, [0.0, 2.2, 6.2], [0.0, 0.3, 0.0]);
    if feature == HardSurfaceFeature::Parallax {
        scene.eye = [2.4, 1.1, 3.2];
        scene.target = [0.0, 0.25, 0.0];
    }
    scene.materials.push(feature_material(feature, enabled));
    let (hull_v, hull_i) = panel_hull([0.55, 0.2, 0.32]);
    for row in 0..3 {
        for col in 0..4 {
            let x = (col as f32 - 1.5) * 1.45;
            let z = (row as f32 - 1.0) * 1.05;
            scene
                .meshes
                .push(placed(hull_v.clone(), hull_i.clone(), 0, [x, 0.2, z]));
        }
    }
    scene
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1.0e-4
    }

    /// `.material.ron` v5's weather block reaches the lab material (spec 99 § 7.3); no block keeps
    /// the default response, which packs to no bits.
    #[test]
    fn the_authored_weather_response_reaches_the_lab_material() {
        let plain = regia_asset::MaterialAssetRon::default_standard();
        let mat = LabMaterial::from_authored(&plain);
        assert_eq!(mat.weather, crate::WeatherResponse::DEFAULT);
        assert_eq!(mat.weather.packed_bits(), 0);
        let authored = regia_asset::MaterialAssetRon {
            weather: Some(regia_asset::MaterialWeatherParams {
                porosity: Some(0.6),
                wet: 0.5,
                snow: 0.0,
                puddles: true,
            }),
            ..plain
        };
        let mat = LabMaterial::from_authored(&authored);
        assert_eq!(
            mat.weather,
            crate::WeatherResponse {
                porosity: Some(0.6),
                wet: 0.5,
                snow: 0.0,
                puddles: true,
            }
        );
        assert_ne!(mat.weather.packed_bits(), 0);
    }

    #[test]
    fn detail_fades_out_with_distance_and_multiplies_around_grey() {
        assert!(near(detail_fade(1.0, 0.0, 0.0), 1.0));
        assert!(near(detail_fade(0.0, 2.0, 10.0), 1.0));
        assert!(near(detail_fade(10.0, 2.0, 10.0), 0.0));
        assert!(near(detail_fade(6.0, 2.0, 10.0), 0.5));
        let weight = detail_weight(0.0, 0.0, 0.0, 1.0, 1.0);
        let grey = apply_detail_albedo([0.4, 0.4, 0.4], [0.5, 0.5, 0.5], weight);
        assert!(near(grey[0], 0.4));
        let dark = apply_detail_albedo([0.4, 0.4, 0.4], [0.25, 0.25, 0.25], 1.0);
        assert!(dark[0] < 0.4);
        assert!(near(apply_detail_roughness(0.2, 0.8, 0.5), 0.5));
        let flat = blend_normals_whiteout([0.0, 0.0, 1.0], [0.0, 0.0, 1.0], 1.0);
        assert!(near(flat[2], 1.0));
    }

    #[test]
    fn specular_occlusion_keeps_a_facing_highlight() {
        assert!(near(specular_occlusion(0.3, 1.0, 0.2), 1.0));
        assert!(near(specular_occlusion(1.0, 0.4, 1.0), 0.4));
        let partial = specular_occlusion(1.0, 0.25, 0.0);
        assert!(partial > 0.25, "smooth facing SO {partial} should exceed AO");
    }

    #[test]
    fn authored_specular_occlusion_replaces_the_default_once() {
        // AO 0.5 on a facing surface of perceptual roughness 0.3.
        let default = indirect_specular_occlusion(1.0, 0.5, 0.3, false);
        let authored = indirect_specular_occlusion(1.0, 0.5, 0.3, true);
        assert!(near(default, specular_occlusion(1.0, 0.5, 0.09)));
        assert!(near(authored, specular_occlusion(1.0, 0.5, 0.3)));
        // One application: the authored form is not the product of both forms.
        assert!(authored < default && authored > default * specular_occlusion(1.0, 0.5, 0.3) + 0.1);
        assert!(near(indirect_specular_occlusion(0.5, 1.0, 0.6, true), 1.0));
    }

    #[test]
    fn anisotropic_ndf_matches_isotropic_when_anisotropy_is_zero() {
        let alpha = 0.36;
        let [at, ab] = anisotropic_alphas(alpha, 0.0);
        assert!(near(at, alpha) && near(ab, alpha));
        let h = Vec3::new(0.25, -0.1, 1.0).normalize();
        let iso = d_ggx_isotropic(h.z, alpha);
        let an = d_ggx_anisotropic(h.x, h.y, h.z, at, ab);
        assert!((iso - an).abs() < 1.0e-5, "iso {iso} an {an}");
        let [wide, narrow] = anisotropic_alphas(alpha, 0.8);
        assert!(wide > alpha && narrow < alpha);
    }

    #[test]
    fn clearcoat_attenuates_only_when_the_factor_is_set() {
        assert!(near(clearcoat_attenuation(1.0, 0.0), 1.0));
        let facing = clearcoat_attenuation(1.0, 1.0);
        assert!(near(facing, 0.96));
        assert!(clearcoat_attenuation(0.2, 1.0) < facing);
    }

    #[test]
    fn multi_scatter_is_zero_when_smooth_and_positive_when_rough() {
        let metal = [1.0, 1.0, 1.0];
        let smooth = multi_scatter_lobe(metal, 0.92, 0.08);
        assert!(smooth[0].abs() < 1.0e-4, "{smooth:?}");
        let rough = multi_scatter_lobe(metal, 0.15, 0.05);
        assert!(rough[0] > 0.0, "{rough:?}");
        let avg = fresnel_average([0.04, 0.04, 0.04]);
        assert!(near(avg[0], 0.04 + 0.96 / 21.0));
    }

    #[test]
    fn energy_compensation_restores_a_white_conductor_and_keeps_dielectrics_close() {
        // A white conductor reflects everything the single-scatter lobe loses: E · comp = 1.
        for e in [0.4_f32, 0.6, 0.9, 1.0] {
            for e_avg in [0.5_f32, 0.8, 1.0] {
                assert!(
                    near(e * energy_compensation([1.0; 3], e, e_avg)[0], 1.0),
                    "{e} {e_avg}"
                );
            }
        }
        // A dielectric (F0 0.04, Favg 0.086) gains only about 7 % at E = 0.4, E_avg = 0.5.
        let d = energy_compensation([0.04; 3], 0.4, 0.5)[0];
        assert!((d - 1.067).abs() < 0.002, "{d}");
        // An 0.8 conductor at E = 0.5, E_avg = 0.5: Fms = 0.81 · 0.5 / (1 − 0.81 · 0.5) = 0.68,
        // not F0 = 0.8 (Turquin's single-Fresnel form overshot Cycles' Principled BSDF on rough
        // metal spheres by 8 %; the single-scatter lobe alone matches Cycles' GGX to 1 %).
        let m = energy_compensation([0.8; 3], 0.5, 0.5)[0];
        assert!((m - 1.68).abs() < 0.002, "{m}");
        // The table's B channel is the height-correlated albedo: 1 when smooth, lower when rough.
        let lut = crate::build_brdf_lut();
        let b = |x: usize, y: usize| f32::from(lut.rgba[(y * 32 + x) * 4 + 2]) / 255.0;
        assert!(
            b(31, 0) > 0.95 && b(31, 31) < 0.5 && b(31, 31) > 0.3,
            "{} {}",
            b(31, 0),
            b(31, 31)
        );
        // The A channel is the row's hemispherical average: 1 when smooth, about 0.5 at r 0.9,
        // the same along a row.
        let a = |x: usize, y: usize| f32::from(lut.rgba[(y * 32 + x) * 4 + 3]) / 255.0;
        assert!(
            a(0, 0) > 0.95 && a(5, 28) > 0.4 && a(5, 28) < 0.6,
            "{} {}",
            a(0, 0),
            a(5, 28)
        );
        assert_eq!(a(0, 28), a(31, 28));
    }

    #[test]
    fn diffuse_under_specular_leaves_the_unreflected_share() {
        // F0 0.04 at normal incidence on a smooth surface: about 96 % goes to the diffuse.
        let w = diffuse_under_specular([0.04; 3], 0.95, 0.01)[0];
        assert!((w - (1.0 - 0.04 * 0.95 - 0.01)).abs() < 1e-6, "{w}");
        // A metal's specular layer takes nearly all of it.
        assert!(diffuse_under_specular([1.0; 3], 0.9, 0.08)[0] < 0.05);
    }

    #[test]
    fn edge_wear_opens_on_convex_samples_only() {
        assert!(near(edge_wear_mask(0.4, 0.5, 4.0, 1.0), 0.0));
        assert!(near(edge_wear_mask(0.75, 0.5, 4.0, 1.0), 1.0));
        let (albedo, rough, metal) =
            apply_edge_wear([0.1, 0.2, 0.6], 0.45, 0.0, 1.0, [0.8, 0.8, 0.8], -0.2, 1.0);
        assert!(near(albedo[0], 0.8));
        assert!(near(rough, 0.25));
        assert!(near(metal, 1.0));
    }

    #[test]
    fn parallax_is_a_no_op_on_a_flat_height_and_shifts_a_deep_one() {
        let uv = [0.4, 0.6];
        let flat = parallax_uv(uv, [0.8, 0.0, 0.2], 0.1, 4.0, 12.0, |_| 1.0);
        assert!(near(flat[0], uv[0]) && near(flat[1], uv[1]));
        let shifted = parallax_uv(uv, [1.0, 0.0, 0.2], 0.1, 4.0, 12.0, |_| 0.0);
        assert!((shifted[0] - uv[0]).abs() > 1.0e-3, "{shifted:?}");
        let grazing = parallax_step_count(0.05, 4.0, 12.0);
        let facing = parallax_step_count(1.0, 4.0, 12.0);
        assert!(grazing > facing, "grazing {grazing} facing {facing}");
        assert_eq!(parallax_step_count(0.0, 4.0, 40.0), 16);
    }

    #[test]
    fn a_flat_deep_height_reaches_the_full_depth_offset() {
        // A constant height of 0 is at maximum depth: the returned UV must be
        // the full-depth offset, whatever the view angle. The march rounds the
        // step count but used to divide the layer depth and the UV delta by
        // the unrounded count, so the ray overshot or stopped short of depth 1
        // and returned an offset up to a third too large (e.g. steps_f = 1.5).
        for view_ts in [[0.2, 0.0, 0.6], [0.3, 0.0, 0.7], [0.4, 0.0, 0.8]] {
            let uv = [0.5, 0.5];
            let out = parallax_uv(uv, view_ts, 0.2, 4.0, 8.0, |_| 0.0);
            let want = [
                uv[0] - view_ts[0] / view_ts[2].abs() * 0.2,
                uv[1] - view_ts[1] / view_ts[2].abs() * 0.2,
            ];
            assert!(
                (out[0] - want[0]).abs() < 1.0e-4 && (out[1] - want[1]).abs() < 1.0e-4,
                "view {view_ts:?}: {out:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn unit_tangent_view_keeps_grazing_steps() {
        // Camera distance in Z. `|z| > 1` saturates the facing term, so the march
        // sticks at `min_steps` even though the angle is not facing.
        let raw = [3.0_f32, 0.0, 1.2];
        assert!(raw[2].abs() > 1.0);
        assert_eq!(
            parallax_step_count(raw[2], 4.0, 16.0),
            parallax_step_count(1.0, 4.0, 16.0)
        );
        let unit = unit_tangent_view(raw);
        assert!(unit[2].abs() < 0.5, "{unit:?}");
        assert!(
            parallax_step_count(unit[2], 4.0, 16.0) > parallax_step_count(1.0, 4.0, 16.0),
            "unit z {} should add steps",
            unit[2]
        );
    }

    #[test]
    fn packers_skip_empty_inputs_and_keep_missing_channels_neutral() {
        assert!(pack_detail_image(None, None).is_none());
        assert!(pack_detail_normal(None, None).is_none());
        assert!(pack_aux(None, None).is_none());
        let albedo = LabTexture {
            width: 2,
            height: 2,
            rgba: vec![10, 20, 30, 255, 10, 20, 30, 255, 10, 20, 30, 255, 10, 20, 30, 255],
        };
        let packed = pack_detail_image(Some(&albedo), None).expect("albedo");
        assert_eq!(packed.rgba[0], 10);
        assert_eq!(packed.rgba[3], 128);
        let height = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
        };
        let aux = pack_aux(Some(&height), None).expect("height");
        assert_eq!(aux.rgba[0], 255);
        assert_eq!(aux.rgba[1], 128);
    }

    #[test]
    fn from_authored_copies_clearcoat_and_defaults_response_on() {
        let mut asset = MaterialAssetRon::default_standard();
        asset.clearcoat = Some(ClearcoatParams {
            factor: 0.35,
            roughness: 0.08,
            normal_map: None,
        });
        asset.anisotropy = Some(AnisotropyParams {
            strength: 0.5,
            direction: [0.0, 1.0],
        });
        let mat = LabMaterial::from_authored(&asset);
        assert!(near(mat.hard_surface.clearcoat, 0.35));
        assert!(near(
            mat.hard_surface.anisotropy_rotation,
            std::f32::consts::FRAC_PI_2
        ));
        assert!(near(mat.hard_surface.multi_scatter, 1.0));
        assert!(near(mat.hard_surface.specular_occlusion, 1.0));
        assert!(mat.albedo.is_none());
        asset.hard_surface = None;
        assert!(near(
            LabMaterial::from_authored(&asset).hard_surface.multi_scatter,
            0.0
        ));
    }

    #[test]
    fn from_authored_with_maps_copies_images_scalars_do_not() {
        use regia_asset::{DetailMapParams, EdgeWearParams, HardSurfaceParams, ParallaxParams};
        let mut asset = MaterialAssetRon::default_standard();
        asset.hard_surface = Some(HardSurfaceParams {
            detail: Some(DetailMapParams {
                albedo_strength: 0.8,
                ..DetailMapParams::default()
            }),
            parallax: Some(ParallaxParams {
                scale: 0.12,
                ..ParallaxParams::default()
            }),
            edge_wear: Some(EdgeWearParams::default()),
            ..HardSurfaceParams::default()
        });
        let pixels = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![10, 20, 30, 40],
        };
        let scalars = LabMaterial::from_authored(&asset);
        assert!(scalars.detail_albedo.is_none());
        assert!(scalars.height_map.is_none());
        assert!(scalars.curvature_map.is_none());
        let mapped = LabMaterial::from_authored_with_maps(
            &asset,
            AuthoredSurfaceMaps {
                detail_albedo: Some(pixels.clone()),
                height_map: Some(pixels.clone()),
                curvature_map: Some(pixels),
                ..AuthoredSurfaceMaps::default()
            },
        );
        assert_eq!(mapped.detail_albedo.expect("detail").rgba[0], 10);
        assert!(mapped.height_map.is_some());
        assert!(mapped.curvature_map.is_some());
        assert!(near(mapped.hard_surface.pom_scale, 0.12));
        assert!(near(mapped.hard_surface.detail_albedo_strength, 0.8));
    }

    #[test]
    fn every_demo_frames_three_spheres_at_daylight() {
        for feature in HardSurfaceFeature::ALL {
            let scene = feature_demo_scene(feature, true);
            assert_eq!(scene.width, DEMO_WIDTH, "{}", feature.name());
            assert_eq!(scene.height, DEMO_HEIGHT);
            assert!(near(scene.exposure_ev, DEMO_EV));
            assert!(scene.meshes.len() >= 4, "{}", feature.name());
        }
        let hull = hull_closeup_scene();
        assert_eq!(hull.meshes.len(), 4);
        assert!(near(hull.exposure_ev, DEMO_EV));
        assert!(hull.materials[0].hard_surface.clearcoat > 0.5);
        assert!(hull_closeup_uncoated().materials[0].hard_surface.clearcoat < 1.0e-5);
        assert!(hull.materials[1].albedo.is_none());
        assert!(hull.materials[0].height_map.is_some());
    }

    #[test]
    fn panel_hull_tangents_follow_the_face() {
        let (vertices, indices) = panel_hull([1.0, 0.4, 0.6]);
        assert_eq!(indices.len(), 36);
        let tangent = Vec3::from_slice(&vertices[0].tangent);
        assert!(tangent.length() > 0.9);
        assert!(
            vertices
                .iter()
                .any(|v| (v.tangent[0] - 1.0).abs() > 0.1 || (v.tangent[1]).abs() > 0.1)
        );
    }
}
