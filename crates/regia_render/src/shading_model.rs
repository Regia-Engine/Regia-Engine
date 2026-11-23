// SPDX-License-Identifier: MPL-2.0
//! Character shading models: skin, eye and fabric sheen (Tincal B3, B4, B6).
//!
//! A [`MaterialShading`] selects how a material lights, on top of its [`crate::LabMaterial`]
//! factors and maps:
//!
//! - [`ShadingModel::Skin`]: pre-integrated diffuse scattering (Penner & Borshukov 2011) with
//!   the d'Eon & Luebke 2007 skin profile, from the surface's own curvature (screen derivatives;
//!   no screen-space pass), per-channel blurred diffuse normals, a second specular lobe and
//!   thin-skin transmission (Jimenez et al. 2010) with the thickness estimated from the same
//!   curvature.
//! - [`ShadingModel::Eye`]: the iris seen through the cornea (Snell refraction onto a recessed
//!   iris plane), the limbal ring, a wet-film specular lobe on the cornea and a socket
//!   occlusion term toward the eyeball's edge.
//! - [`FabricSheen`]: a sheen lobe with the Charlie distribution (Estevez & Kulla 2017) and the
//!   `KHR_materials_sheen` albedo scaling of the layer under it. It combines with the standard
//!   or the skin model.
//!
//! **Where it lives.** The shading model belongs to the material, but [`crate::LabMaterial`]
//! does not change (hosts build it by struct literal). The renderer keeps a side table by
//! material key ([`crate::FrameRenderer::set_material_shading`]) and packs it into five
//! vec4s of the material uniform; a key not in the table packs zeros, which the forward shader
//! reads as the standard model, so other materials draw exactly as before. On the ECS path,
//! put a [`MaterialShading`] component next to the entity's [`crate::MeshRenderer`]:
//! [`extract_material_shading`] keys it by the entity's material `Arc` every frame. Entities
//! that share one material `Arc` share its shading; if they disagree, the first in extraction
//! order wins and [`ExtractedMaterialShading::conflicts`] counts the others.
//!
//! **Pipelines.** No new pipeline: the forward shader branches on the material's model word
//! (uniform per draw), so a warmed renderer draws these materials without building anything.
//!
//! The shader is `shaders/shading_models.wgsl`; the functions below mirror it and the tests
//! check them against the numerical integrals they approximate (`docs/provenance.md`).

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;

use crate::runtime::{ExtractedFrame, ExtractedMesh};

/// Vec4s the shading block adds to the material uniform.
pub(crate) const SHADING_VEC4S: usize = 5;

/// Model word of the standard metallic-roughness response.
pub(crate) const MODEL_STANDARD: f32 = 0.0;
/// Model word of [`ShadingModel::Skin`].
pub(crate) const MODEL_SKIN: f32 = 1.0;
/// Model word of [`ShadingModel::Eye`].
pub(crate) const MODEL_EYE: f32 = 2.0;

/// Index of refraction of the human cornea (Gullstrand's schematic eye).
pub const CORNEA_IOR: f32 = 1.376;

/// The sheen roughness floor (`SHEEN_MIN_ROUGHNESS` in the shader).
pub const SHEEN_MIN_ROUGHNESS: f32 = 0.07;

/// Skin diffusion profile of d'Eon & Luebke 2007 (GPU Gems 3, ch. 14): Gaussian variances in
/// mm² and their RGB weights.
pub const SKIN_PROFILE: [(f32, [f32; 3]); 6] = [
    (0.0064, [0.233, 0.455, 0.649]),
    (0.0484, [0.100, 0.336, 0.344]),
    (0.187, [0.118, 0.198, 0.0]),
    (0.567, [0.113, 0.007, 0.007]),
    (1.99, [0.358, 0.004, 0.0]),
    (7.41, [0.078, 0.0, 0.0]),
];

/// Skin parameters ([`ShadingModel::Skin`]). [`Default`] is the measured human profile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkinShading {
    /// Multiplies the profile's scattering distances (1 = the measured profile; above 1 reads
    /// as a softer, more translucent skin).
    pub scatter_scale: f32,
    /// Curvature radius in millimetres used instead of the screen-derivative estimate when
    /// above 0 (and always on the visibility-buffer path, which has no derivatives; 0 there
    /// uses 30 mm).
    pub curvature_radius_mm: f32,
    /// How far the red, green and blue diffuse normals lean from the mapped normal back to the
    /// geometric one (Penner's pre-integrated normal blur): red takes this share, green half
    /// of it, blue a quarter. 0 lights all three with the mapped normal.
    pub normal_blur: f32,
    /// Weight of the light transmitted through thin skin (ears, nostrils) from behind.
    pub transmission: f32,
    /// Thickness of the skin per millimetre of curvature radius, for the transmission
    /// (thin parts curve tightly).
    pub thickness_scale: f32,
    /// Perceptual roughness of the second specular lobe.
    pub secondary_roughness: f32,
    /// Share of the second lobe in the specular (0 = one lobe).
    pub secondary_weight: f32,
    /// Millimetres per world unit (1000 for metres), for the curvature radius.
    pub mm_per_unit: f32,
}

impl Default for SkinShading {
    fn default() -> Self {
        Self {
            scatter_scale: 1.0,
            curvature_radius_mm: 0.0,
            normal_blur: 0.5,
            transmission: 1.0,
            thickness_scale: 1.0,
            secondary_roughness: 0.75,
            secondary_weight: 0.15,
            mm_per_unit: 1000.0,
        }
    }
}

/// Eye parameters ([`ShadingModel::Eye`]). Distances are in UV units of the eyeball's base
/// colour map, measured from [`Self::iris_center`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EyeShading {
    /// UV of the pupil centre.
    pub iris_center: [f32; 2],
    /// UV radius of the iris (the limbus).
    pub iris_radius: f32,
    /// Width of the limbal ring in UV.
    pub limbus_width: f32,
    /// How much the limbal ring darkens the base colour (0..1).
    pub limbus_darkening: f32,
    /// Depth of the iris below the cornea at the pupil, in UV units (0 = no refraction).
    pub iris_depth: f32,
    /// Index of refraction of the cornea ([`CORNEA_IOR`]).
    pub cornea_ior: f32,
    /// Perceptual roughness of the wet cornea's specular lobe.
    pub cornea_roughness: f32,
    /// Darkening toward the eyeball's edge, under the lids (0..1).
    pub socket_occlusion: f32,
    /// UV radius where the socket occlusion starts.
    pub occlusion_start: f32,
    /// UV radius where the socket occlusion is full.
    pub occlusion_end: f32,
}

impl Default for EyeShading {
    fn default() -> Self {
        Self {
            iris_center: [0.5, 0.5],
            iris_radius: 0.16,
            limbus_width: 0.025,
            limbus_darkening: 0.6,
            iris_depth: 0.04,
            cornea_ior: CORNEA_IOR,
            cornea_roughness: 0.05,
            socket_occlusion: 0.35,
            occlusion_start: 0.3,
            occlusion_end: 0.5,
        }
    }
}

/// A fabric sheen lobe (`KHR_materials_sheen`: `sheenColorFactor`, `sheenRoughnessFactor`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FabricSheen {
    /// Linear sheen colour (0..1 per channel; black is no sheen).
    pub color: [f32; 3],
    /// Sheen roughness (0..1, floored at [`SHEEN_MIN_ROUGHNESS`]).
    pub roughness: f32,
}

impl Default for FabricSheen {
    fn default() -> Self {
        Self {
            color: [0.5, 0.5, 0.5],
            roughness: 0.5,
        }
    }
}

/// The lighting response of a material.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ShadingModel {
    /// Metallic-roughness GGX with the material's own lobes (clearcoat, anisotropy, …).
    #[default]
    Standard,
    /// Pre-integrated skin ([`SkinShading`]).
    Skin(SkinShading),
    /// Eye with a refracting cornea ([`EyeShading`]).
    Eye(EyeShading),
}

impl ShadingModel {
    /// Lower-case name (`standard`, `skin`, `eye`).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Skin(_) => "skin",
            Self::Eye(_) => "eye",
        }
    }
}

/// How a material shades (see the module docs). [`Default`] is the standard model without
/// sheen, which draws exactly like no shading at all.
///
/// # Example
///
/// ```
/// use regia_render::shading_model::{FabricSheen, MaterialShading, ShadingModel, SkinShading};
///
/// let skin = MaterialShading::skin(SkinShading::default());
/// assert_eq!(skin.model.name(), "skin");
/// let velvet = MaterialShading::sheen(FabricSheen { color: [0.9, 0.2, 0.3], roughness: 0.4 });
/// assert!(matches!(velvet.model, ShadingModel::Standard));
/// assert!(MaterialShading::default().is_standard());
/// ```
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct MaterialShading {
    /// The base response.
    pub model: ShadingModel,
    /// Optional sheen lobe on top (ignored by the eye model).
    pub sheen: Option<FabricSheen>,
}

impl MaterialShading {
    /// Skin without sheen.
    #[must_use]
    pub fn skin(skin: SkinShading) -> Self {
        Self {
            model: ShadingModel::Skin(skin),
            sheen: None,
        }
    }

    /// Eye.
    #[must_use]
    pub fn eye(eye: EyeShading) -> Self {
        Self {
            model: ShadingModel::Eye(eye),
            sheen: None,
        }
    }

    /// The standard model with a sheen lobe (fabric).
    #[must_use]
    pub fn sheen(sheen: FabricSheen) -> Self {
        Self {
            model: ShadingModel::Standard,
            sheen: Some(sheen),
        }
    }

    /// Whether this draws exactly like the standard model (no table entry is needed).
    #[must_use]
    pub fn is_standard(&self) -> bool {
        self.pack() == [[0.0; 4]; SHADING_VEC4S]
    }

    /// The material uniform's shading block: `shading` (x = model, y = sheen on), three model
    /// parameter vec4s and the sheen colour + roughness. Out-of-range values are clamped.
    #[must_use]
    pub(crate) fn pack(&self) -> [[f32; 4]; SHADING_VEC4S] {
        let mut out = [[0.0; 4]; SHADING_VEC4S];
        match self.model {
            ShadingModel::Standard => out[0][0] = MODEL_STANDARD,
            ShadingModel::Skin(s) => {
                out[0][0] = MODEL_SKIN;
                out[1] = [
                    finite_or(s.scatter_scale, 1.0).max(1e-3),
                    finite_or(s.curvature_radius_mm, 0.0).max(0.0),
                    finite_or(s.normal_blur, 0.0).clamp(0.0, 1.0),
                    finite_or(s.transmission, 0.0).max(0.0),
                ];
                out[2] = [
                    finite_or(s.secondary_roughness, 0.75).clamp(0.0, 1.0),
                    finite_or(s.secondary_weight, 0.0).clamp(0.0, 1.0),
                    finite_or(s.thickness_scale, 1.0).max(0.0),
                    finite_or(s.mm_per_unit, 1000.0).max(1e-6),
                ];
            }
            ShadingModel::Eye(e) => {
                out[0][0] = MODEL_EYE;
                out[1] = [
                    finite_or(e.iris_radius, 0.16).max(1e-4),
                    finite_or(e.limbus_width, 0.0).max(0.0),
                    finite_or(e.limbus_darkening, 0.0).clamp(0.0, 1.0),
                    finite_or(e.iris_depth, 0.0).max(0.0),
                ];
                out[2] = [
                    finite_or(e.iris_center[0], 0.5),
                    finite_or(e.iris_center[1], 0.5),
                    finite_or(e.cornea_ior, CORNEA_IOR).max(1.0),
                    finite_or(e.cornea_roughness, 0.05).clamp(0.0, 1.0),
                ];
                out[3] = [
                    finite_or(e.socket_occlusion, 0.0).clamp(0.0, 1.0),
                    finite_or(e.occlusion_start, 0.3),
                    finite_or(e.occlusion_end, 0.5),
                    0.0,
                ];
            }
        }
        if let Some(sheen) = self.sheen
            && !matches!(self.model, ShadingModel::Eye(_))
        {
            let color = sheen.color.map(|c| finite_or(c, 0.0).clamp(0.0, 1.0));
            if color.iter().any(|&c| c > 0.0) {
                out[0][1] = 1.0;
                out[4] = [
                    color[0],
                    color[1],
                    color[2],
                    finite_or(sheen.roughness, 0.5).clamp(SHEEN_MIN_ROUGHNESS, 1.0),
                ];
            }
        }
        out
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

// ---------------------------------------------------------------------------
// CPU mirrors of `shading_models.wgsl` and the references they approximate
// ---------------------------------------------------------------------------

/// The clamped cosine's Fourier coefficients for the harmonics the shader keeps
/// (`k = 1, 2, 4, …, 12`): `1/2` for k = 1, `(2/π)(−1)^(k/2+1) / (k² − 1)` for even k.
fn clamped_cosine_coefficient(k: u32) -> f64 {
    if k == 1 {
        return 0.5;
    }
    let sign = if (k / 2) % 2 == 1 { 1.0 } else { -1.0 };
    sign * 2.0 / std::f64::consts::PI / f64::from(k * k - 1)
}

const HARMONICS: [u32; 7] = [1, 2, 4, 6, 8, 10, 12];

/// Penner's pre-integrated skin falloff, evaluated as the shader does: the clamped cosine
/// `cos_theta` (to the light) plus the profile's ring-convolution correction at a curvature
/// radius of `radius_mm` (already divided by the scatter scale). RGB, never below 0.
#[must_use]
pub fn skin_falloff(cos_theta: f32, radius_mm: f32) -> [f32; 3] {
    let kernel = skin_kernel(radius_mm);
    let c = f64::from(cos_theta.clamp(-1.0, 1.0));
    let theta = c.acos();
    let mut out = [0.0f32; 3];
    for (ch, value) in out.iter_mut().enumerate() {
        let mut d = c.max(0.0);
        for (i, &k) in HARMONICS.iter().enumerate() {
            d += clamped_cosine_coefficient(k) * f64::from(kernel[i][ch]) * (f64::from(k) * theta).cos();
        }
        *value = d.max(0.0) as f32;
    }
    out
}

/// `κ_k − 1` per harmonic and channel, as `shading_models.wgsl::skin_kernel` computes them.
fn skin_kernel(radius_mm: f32) -> [[f32; 3]; 7] {
    let r2 = radius_mm * radius_mm;
    let mut sum = [0.0f32; 3];
    let mut s = [[0.0f32; 3]; 7];
    for &(v, w) in &SKIN_PROFILE {
        let a = r2 / v;
        let rho = a / (0.5 + (a * a + 2.25).sqrt());
        let scale = 1.0 / (v * (1.0 + std::f32::consts::TAU * a).sqrt());
        for ch in 0..3 {
            let wc = w[ch] * scale;
            sum[ch] += wc;
            for (i, &k) in HARMONICS.iter().enumerate() {
                s[i][ch] += wc * rho.powi((k * k) as i32);
            }
        }
    }
    for row in &mut s {
        for ch in 0..3 {
            row[ch] = row[ch] / sum[ch].max(1e-12) - 1.0;
        }
    }
    s
}

/// The integral [`skin_falloff`] approximates (Penner & Borshukov 2011): the clamped cosine
/// convolved around a ring of radius `radius_mm` with the skin profile [`SKIN_PROFILE`],
/// `∫ cos⁺(θ + x) R(2r sin(x/2)) dx / ∫ R(2r sin(x/2)) dx`, by the midpoint rule.
#[must_use]
pub fn skin_falloff_reference(theta: f32, radius_mm: f32) -> [f32; 3] {
    let steps = 4096;
    let r = f64::from(radius_mm);
    let mut num = [0.0f64; 3];
    let mut den = [0.0f64; 3];
    for i in 0..steps {
        let x = (f64::from(i) + 0.5) / f64::from(steps) * std::f64::consts::TAU - std::f64::consts::PI;
        let d = 2.0 * r * (x / 2.0).sin().abs();
        let lit = (f64::from(theta) + x).cos().max(0.0);
        for &(v, w) in &SKIN_PROFILE {
            let v = f64::from(v);
            let g = (-d * d / (2.0 * v)).exp() / (std::f64::consts::TAU * v);
            for ch in 0..3 {
                let p = f64::from(w[ch]) * g;
                num[ch] += lit * p;
                den[ch] += p;
            }
        }
    }
    [0, 1, 2].map(|ch| (num[ch] / den[ch].max(1e-300)) as f32)
}

/// Transmittance of the skin profile through `thickness_mm` (Jimenez et al. 2010):
/// `Σ w_i exp(−s² / v_i)` per channel.
#[must_use]
pub fn skin_transmittance(thickness_mm: f32) -> [f32; 3] {
    let s2 = thickness_mm * thickness_mm;
    let mut t = [0.0f32; 3];
    for &(v, w) in &SKIN_PROFILE {
        let e = (-s2 / v).exp();
        for ch in 0..3 {
            t[ch] += w[ch] * e;
        }
    }
    t
}

/// Charlie sheen distribution `(2 + 1/α) sin(θh)^(1/α) / 2π`, `α` = sheen roughness².
#[must_use]
pub fn d_charlie(no_h: f32, alpha: f32) -> f32 {
    let inv = 1.0 / alpha;
    let sin2 = (1.0 - no_h * no_h).max(1e-7);
    (2.0 + inv) * sin2.powf(0.5 * inv) / std::f32::consts::TAU
}

/// Ashikhmin's sheen visibility `1 / (4 (n·l + n·v − n·l n·v))`.
#[must_use]
pub fn v_sheen(no_v: f32, no_l: f32) -> f32 {
    1.0 / (4.0 * (no_l + no_v - no_l * no_v)).max(1e-4)
}

/// The shader's fitted directional albedo of the white sheen lobe at `cos θ = mu`, sheen
/// roughness `roughness` (`shading_models.wgsl::sheen_albedo`).
#[must_use]
pub fn sheen_albedo(mu: f32, roughness: f32) -> f32 {
    let r = roughness;
    let m = mu.max(0.02);
    let a = 0.042 + r * (1.0505 + r * (0.3251 - 0.984 * r));
    let b = (-0.044 + r * (0.3808 + r * (2.0542 - 1.7916 * r))).abs();
    let c = -0.4447 + r * (0.371 + r * (-0.5375 + 0.4956 * r));
    (a / (m + b) + c).clamp(0.0, 1.0)
}

/// The integral [`sheen_albedo`] fits: `∫ D_charlie V_sheen (n·l) dω_l` over the hemisphere
/// for a view at `cos θ = mu`, clamped to 1 (the lobe is not energy-bounded at grazing views).
#[must_use]
pub fn sheen_albedo_reference(mu: f32, roughness: f32) -> f32 {
    let alpha = f64::from(roughness * roughness);
    let mu = f64::from(mu);
    let view = [(1.0 - mu * mu).max(0.0).sqrt(), 0.0, mu];
    let (nt, np) = (256u32, 512u32);
    let dt = std::f64::consts::FRAC_PI_2 / f64::from(nt);
    let dp = std::f64::consts::TAU / f64::from(np);
    let mut sum = 0.0;
    for i in 0..nt {
        let theta = (f64::from(i) + 0.5) * dt;
        let (st, ct) = theta.sin_cos();
        for j in 0..np {
            let phi = (f64::from(j) + 0.5) * dp;
            let l = [st * phi.cos(), st * phi.sin(), ct];
            let h = [l[0] + view[0], l[1] + view[1], l[2] + view[2]];
            let len = (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt();
            let no_h = h[2] / len;
            let sin2 = (1.0 - no_h * no_h).max(0.0);
            let d = (2.0 + 1.0 / alpha) * sin2.powf(0.5 / alpha) / std::f64::consts::TAU;
            let v = 1.0 / (4.0 * (ct + mu - ct * mu));
            sum += d * v * ct * st * dt * dp;
        }
    }
    sum.min(1.0) as f32
}

/// The UV the iris shows at through the cornea (`shading_models.wgsl::eye_iris_uv`): `view_ts`
/// is the unit view direction in the tangent frame (z along the normal).
#[must_use]
pub fn eye_iris_uv(uv: [f32; 2], eye: &EyeShading, view_ts: [f32; 3]) -> [f32; 2] {
    let d = [uv[0] - eye.iris_center[0], uv[1] - eye.iris_center[1]];
    let rho = (d[0] * d[0] + d[1] * d[1]).sqrt() / eye.iris_radius.max(1e-4);
    let h = eye.iris_depth * (1.0 - rho * rho).max(0.0);
    let vz = view_ts[2].max(1e-3);
    let len = (view_ts[0] * view_ts[0] + view_ts[1] * view_ts[1] + vz * vz).sqrt();
    let incident = [-view_ts[0] / len, -view_ts[1] / len, -vz / len];
    // Snell (the WGSL `refract` builtin) at the normal (0, 0, 1).
    let eta = 1.0 / eye.cornea_ior.max(1.0);
    let cos_i = -incident[2];
    let k = 1.0 - eta * eta * (1.0 - cos_i * cos_i);
    if k < 0.0 {
        return uv;
    }
    let t = eta * cos_i - k.sqrt();
    let r = [eta * incident[0], eta * incident[1], eta * incident[2] + t];
    let rz = (-r[2]).max(0.15);
    [uv[0] + r[0] / rz * h, uv[1] + r[1] / rz * h]
}

// ---------------------------------------------------------------------------
// ECS extraction
// ---------------------------------------------------------------------------

/// The frame's material shading table, by material key ([`material_key`] of the
/// [`crate::CpuMaterial`] `Arc`, the key [`ExtractedFrame::frame_scene`] gives each draw).
///
/// [`extract_material_shading`] fills it after [`crate::extract_frame`]. A host that renders an
/// [`ExtractedFrame`] itself passes [`Self::entries`] to
/// [`crate::FrameRenderer::set_material_shading`] every frame.
#[derive(Resource, Debug, Clone, Default)]
pub struct ExtractedMaterialShading {
    /// `(material key, shading)`, one per material, standard entries left out.
    pub entries: Vec<(u64, MaterialShading)>,
    /// Extracted mesh draws whose entity carries a non-standard [`MaterialShading`].
    pub shaded_draws: usize,
    /// Draws whose entity's shading disagrees with an earlier entity's for the same material
    /// (the earlier one is used).
    pub conflicts: usize,
}

impl ExtractedMaterialShading {
    /// Builds the table from the frame's meshes and each entity's shading.
    #[must_use]
    pub fn collect(meshes: &[ExtractedMesh], shading: impl Fn(Entity) -> Option<MaterialShading>) -> Self {
        let mut out = Self::default();
        let mut by_key: HashMap<u64, usize> = HashMap::new();
        for mesh in meshes {
            let Some(value) = shading(mesh.entity).filter(|s| !s.is_standard()) else {
                continue;
            };
            out.shaded_draws += 1;
            let key = material_key(&mesh.material);
            match by_key.get(&key) {
                Some(&index) => {
                    if out.entries[index].1 != value {
                        out.conflicts += 1;
                    }
                }
                None => {
                    by_key.insert(key, out.entries.len());
                    out.entries.push((key, value));
                }
            }
        }
        out
    }

    /// [`Self::collect`] with each entity's [`MaterialShading`] component.
    #[must_use]
    pub fn from_world(world: &World, meshes: &[ExtractedMesh]) -> Self {
        Self::collect(meshes, |entity| world.get::<MaterialShading>(entity).copied())
    }

    /// The entries, for [`crate::FrameRenderer::set_material_shading`].
    pub fn entries(&self) -> impl Iterator<Item = (u64, MaterialShading)> + '_ {
        self.entries.iter().copied()
    }
}

/// The material key a [`crate::MeshRenderer`]'s material draws under on the ECS path: a
/// generation id per `Arc` (the same for its clones, never reused after the last one drops), not
/// the `Arc`'s address, which a later material could be allocated at.
#[must_use]
pub fn material_key(material: &Arc<crate::CpuMaterial>) -> u64 {
    crate::runtime::material_key(material)
}

/// `Render` system: fills [`ExtractedMaterialShading`] from the extracted meshes.
pub fn extract_material_shading(
    extracted: Option<Res<ExtractedFrame>>,
    shading: Query<&MaterialShading>,
    mut out: ResMut<ExtractedMaterialShading>,
) {
    // No entity carries a shading: the table is empty without a lookup per draw (ADR-385).
    let Some(extracted) = extracted.filter(|_| !shading.is_empty()) else {
        *out = ExtractedMaterialShading::default();
        return;
    };
    *out = ExtractedMaterialShading::collect(&extracted.meshes, |entity| shading.get(entity).ok().copied());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_packs_zeros_and_sheen_alone_sets_its_flag() {
        assert_eq!(MaterialShading::default().pack(), [[0.0; 4]; SHADING_VEC4S]);
        assert!(MaterialShading::default().is_standard());
        // A black sheen is no sheen.
        assert!(
            MaterialShading::sheen(FabricSheen {
                color: [0.0; 3],
                roughness: 0.3
            })
            .is_standard()
        );
        let velvet = MaterialShading::sheen(FabricSheen {
            color: [0.9, 0.2, 0.3],
            roughness: 0.01,
        })
        .pack();
        assert_eq!(velvet[0], [MODEL_STANDARD, 1.0, 0.0, 0.0]);
        assert_eq!(velvet[4], [0.9, 0.2, 0.3, SHEEN_MIN_ROUGHNESS]);
        let skin = MaterialShading::skin(SkinShading::default()).pack();
        assert_eq!(skin[0][0], MODEL_SKIN);
        let eye = MaterialShading {
            model: ShadingModel::Eye(EyeShading::default()),
            sheen: Some(FabricSheen::default()),
        }
        .pack();
        assert_eq!(eye[0], [MODEL_EYE, 0.0, 0.0, 0.0], "the eye ignores sheen");
        assert!((eye[2][2] - CORNEA_IOR).abs() < 1e-6);
    }

    #[test]
    fn pack_clamps_and_replaces_non_finite_values() {
        let skin = MaterialShading::skin(SkinShading {
            scatter_scale: f32::NAN,
            normal_blur: 4.0,
            secondary_weight: -1.0,
            ..SkinShading::default()
        })
        .pack();
        assert_eq!(skin[1][0], 1.0);
        assert_eq!(skin[1][2], 1.0);
        assert_eq!(skin[2][1], 0.0);
    }

    #[test]
    fn the_profile_weights_sum_to_one_per_channel() {
        for ch in 0..3 {
            let sum: f32 = SKIN_PROFILE.iter().map(|(_, w)| w[ch]).sum();
            assert!((sum - 1.0).abs() < 2e-3, "channel {ch}: {sum}");
        }
    }

    #[test]
    fn skin_falloff_matches_the_pre_integration_integral() {
        // The worst error over the angle grid per radius; the bound grows toward 1 mm, where
        // the truncated harmonics and the ρ = I₁/I₀ approximation matter most.
        for (radius, bound) in [
            (1.0f32, 0.03f32),
            (2.0, 0.02),
            (5.0, 0.012),
            (12.0, 0.008),
            (40.0, 0.004),
        ] {
            let mut worst = 0.0f32;
            for step in 0..=36 {
                let theta = step as f32 / 36.0 * std::f32::consts::PI;
                let reference = skin_falloff_reference(theta, radius);
                let fitted = skin_falloff(theta.cos(), radius);
                for ch in 0..3 {
                    worst = worst.max((reference[ch] - fitted[ch]).abs());
                }
            }
            assert!(worst < bound, "radius {radius} mm: worst error {worst} ≥ {bound}");
        }
    }

    #[test]
    fn skin_falloff_wraps_red_past_the_terminator_on_tight_curves() {
        let tight = skin_falloff(0.0, 2.0);
        assert!(tight[0] > tight[1] && tight[1] >= tight[2], "{tight:?}");
        assert!(tight[0] > 0.015, "{tight:?}");
        // A flat surface is the clamped cosine.
        let flat = skin_falloff(0.5, 500.0);
        for value in flat {
            assert!((value - 0.5).abs() < 1e-3, "{flat:?}");
        }
        assert!(skin_falloff(-0.6, 500.0).iter().all(|&v| v < 1e-3));
    }

    #[test]
    fn transmittance_is_red_through_thin_skin_and_zero_through_thick() {
        let ear = skin_transmittance(2.0);
        assert!(ear[0] > 0.05 && ear[0] > 10.0 * ear[1].max(1e-6), "{ear:?}");
        assert_eq!(skin_transmittance(0.0).map(|t| (t * 100.0).round()), [100.0; 3]);
        assert!(skin_transmittance(20.0).iter().all(|&t| t < 1e-6));
    }

    #[test]
    fn charlie_is_normalised_over_half_vectors() {
        // ∫ D(h) (n·h) dω_h = 1 over the hemisphere of half vectors.
        for roughness in [0.3f32, 0.6, 1.0] {
            let alpha = roughness * roughness;
            let n = 2048;
            let mut sum = 0.0f64;
            for i in 0..n {
                let theta = (i as f64 + 0.5) / n as f64 * std::f64::consts::FRAC_PI_2;
                let d = f64::from(d_charlie(theta.cos() as f32, alpha));
                sum += d * theta.cos() * theta.sin() * std::f64::consts::FRAC_PI_2 / n as f64
                    * std::f64::consts::TAU;
            }
            assert!((sum - 1.0).abs() < 0.01, "roughness {roughness}: {sum}");
        }
        assert!((v_sheen(1.0, 1.0) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn sheen_albedo_fit_matches_the_integral() {
        let mut worst = 0.0f32;
        let mut total = 0.0f32;
        let mut count = 0;
        for roughness in [0.1f32, 0.25, 0.4, 0.55, 0.7, 0.85, 1.0] {
            for mu in [0.05f32, 0.15, 0.3, 0.5, 0.7, 0.9, 1.0] {
                let error = (sheen_albedo_reference(mu, roughness) - sheen_albedo(mu, roughness)).abs();
                worst = worst.max(error);
                total += error;
                count += 1;
            }
        }
        let mean = total / count as f32;
        assert!(worst < 0.06 && mean < 0.015, "worst {worst}, mean {mean}");
    }

    #[test]
    fn the_iris_refracts_toward_the_view_and_stays_put_head_on() {
        let eye = EyeShading::default();
        let center = eye.iris_center;
        assert_eq!(eye_iris_uv(center, &eye, [0.0, 0.0, 1.0]), center);
        // Seen from +u, the refracted ray runs on away from the viewer: the recessed iris
        // under the pupil shows the texel toward −u.
        let side = eye_iris_uv(center, &eye, [0.6, 0.0, 0.8]);
        assert!(
            side[0] < center[0] && (side[1] - center[1]).abs() < 1e-6,
            "{side:?}"
        );
        // Refraction bends the ray toward the normal: a smaller shift than an air gap's.
        let air = EyeShading {
            cornea_ior: 1.0,
            ..eye
        };
        let unbent = eye_iris_uv(center, &air, [0.6, 0.0, 0.8]);
        assert!(center[0] - side[0] < center[0] - unbent[0], "{side:?} {unbent:?}");
        // No shift on the sclera.
        let sclera = [center[0] + 0.3, center[1]];
        assert_eq!(eye_iris_uv(sclera, &eye, [0.6, 0.0, 0.8]), sclera);
    }

    #[test]
    fn extraction_keys_by_material_and_counts_conflicts() {
        use crate::{CpuMaterial, CpuMesh};
        let mesh = Arc::new(CpuMesh {
            vertices: vec![],
            indices: vec![],
        });
        let skin_mat = Arc::new(CpuMaterial::default());
        let cloth_mat = Arc::new(CpuMaterial::default());
        let draw = |entity: Entity, material: &Arc<CpuMaterial>| ExtractedMesh {
            is_static: false,
            entity,
            mesh: mesh.clone(),
            material: material.clone(),
            layer: 0,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: Vec::new(),
            decal_layer: 0,
            uv1: None,
            shadow: crate::ShadowCast::All,
            draw_indices: None,
            lod_key: 0,
            object_key_xor: 0,
            gi_proxy: true,
            lod_dither: 0.0,
        };
        let mut world = World::new();
        let (a, b, c, d) = (
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
        );
        let meshes = vec![
            draw(a, &skin_mat),
            draw(b, &skin_mat),
            draw(c, &cloth_mat),
            draw(d, &skin_mat),
        ];
        let skin = MaterialShading::skin(SkinShading::default());
        let velvet = MaterialShading::sheen(FabricSheen::default());
        let table = ExtractedMaterialShading::collect(&meshes, |entity| {
            if entity == a || entity == b {
                Some(skin)
            } else if entity == c || entity == d {
                Some(velvet)
            } else {
                None
            }
        });
        assert_eq!(
            table.entries,
            vec![
                (material_key(&skin_mat), skin),
                (material_key(&cloth_mat), velvet)
            ]
        );
        assert_eq!(table.shaded_draws, 4);
        assert_eq!(
            table.conflicts, 1,
            "d disagrees with a on the shared skin material"
        );
    }
}
