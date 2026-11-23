// SPDX-License-Identifier: MPL-2.0
//! CPU lightmap baker (`regia bake-lightmaps`, ADR-860): fills a
//! `regia_asset::lightmap::Lightmap` for the placed meshes of a [`PtScene`], one entry per
//! receiving instance, with the UV2 layout from `regia_asset_pipeline::lightmap_uv`.
//!
//! Per entry, every unwrapped triangle is rasterised in the entry's rectangle; a texel whose
//! centre lies on a triangle gets that surface point. Its irradiance is estimated by Monte Carlo
//! path tracing (Kajiya 1986; PBRT 4th ed. ch. 13) over Lambertian surfaces:
//!
//! - **Hemisphere**: cosine-weighted directions (Malley's method), so a ray that escapes adds
//!   `π · L_sky` and a ray that hits a surface adds its albedo times the irradiance there.
//! - **Next-event estimation** at every path vertex: the sun (a cone of its angular radius),
//!   point, spot (the runtime's linear `spot_attenuation` ramp) and sphere lights (as a point of
//!   the same intensity, nothing inside the sphere), and emissive triangles (picked by power, a
//!   uniform point on the triangle; PBRT 4th ed. § 12.6). Quad lights become two emissive
//!   triangles first, like the path tracer's `pack`, so they cast shadows and block the sky.
//!   Emission found by a hemisphere ray is not added again, so each light is counted once.
//! - **Contents**: the sun seen directly at the texel is only baked with
//!   [`LightmapBakeSettings::direct_sun`]; by default the runtime's sun and its shadows stay live
//!   and the lightmap holds the sky, the sun's bounces, emissive surfaces and local lights
//!   (`regia_asset::lightmap::LightmapContents`).
//! - **Leaks**: a texel whose hemisphere rays mostly hit back faces sits inside geometry (a
//!   floor running under a wall); it is dropped and refilled from its neighbours. A texel whose
//!   center lies on a chart's border (the UV atlas generator puts borders on texel centers) traces
//!   from just inside its triangle: on the border itself it would sit in the plane of the wall it
//!   joins, and rays would leave through the joint (ADR-2024).
//! - **Filter** (optional): a 3×3 edge-aware blur over baked texels, weighted by normal
//!   similarity (a single cross-bilateral step, Tomasi & Manduchi 1998).
//! - **Denoise** ([`LightmapBakeSettings::denoise`], ADR-2314; `regia bake-lightmaps` turns it on
//!   for new bakes): an edge-avoiding à-trous filter guided by normal, position and chart, with
//!   its luminance weight scaled by each texel's measured noise (Dammertz et al. 2010, Schied et
//!   al. 2017; `denoise.rs`). Across a chart border it reads only the surface that continues the
//!   chart, in sight of the texel (ADR-4108, ADR-7590), and it replaces the 3×3 filter.
//! - **Gutters**: empty texels inside an entry's rectangle next to filled ones take the mean of
//!   their filled neighbours, `padding + 1` rounds, so bilinear filtering never reads black.
//! - **Directions** ([`LightmapBakeSettings::directional`], ADR-1556): every light contribution
//!   at the texel (each next-event sample, and the light a hemisphere path brings back along its
//!   first direction) adds its direction weighted by its luminance. The mean is the dominant
//!   direction `d` (`|d| <= 1`) the `.flmap` v2 direction section stores with `n · d`. It uses
//!   no extra random numbers, so the irradiance is the same with or without it.
//! - **Noise** (validation without a denoiser): each texel keeps the sum and the sum of squares
//!   of its per-sample luminance, so the bake reports the relative standard error of the mean
//!   ([`LightmapNoise`]) before any filter runs. With [`LightmapBakeSettings::noise_target`] the
//!   texel keeps sampling, `samples` at a time, until it is below the target or reaches
//!   [`LightmapBakeSettings::max_samples`] (at most [`MAX_LIGHTMAP_SAMPLES`]); sample `k` always
//!   uses the same random stream, so the result stays deterministic.
//!
//! Traversal uses the binned SAH [`Bvh`] over world triangles and Möller–Trumbore ray/triangle
//! tests (Möller & Trumbore 1997). Ray origins leave the surface by a few float steps along the
//! face normal (Wächter & Binder 2019, *Ray Tracing Gems* ch. 6), so the offset follows the
//! position's precision rather than the scene's size and thin walls still occlude. A hemisphere
//! direction below the geometric surface (a shading normal tilted away from the face) is
//! occluded by the surface itself. Every texel's random sequence is a PCG hash of the seed, the
//! entry's name, the texel's position inside its entry and the sample (Jarzynski & Olano 2020),
//! so the result depends neither on the thread count nor on where the atlas packer put the
//! entry.
//!
//! [`bake_mesh_lightmap`] bakes one instance on its own with its authored UV2
//! (lightmaps phase 2, ADR-1101), and [`quad_box_scene`] is the reference scene its tests use.
//!
//! Not traced: the terrain heightfield, sky bodies, media, alpha masks, transmission and
//! specular lobes (a metallic surface bounces `base × (1 − metallic)`). A `PtSky::Table` is
//! baked as its mean radiance; a `PtSky::Equirect` as its solid-angle weighted mean.

use std::sync::atomic::{AtomicUsize, Ordering};

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapDenoiseInfo, LightmapLayout,
    MAX_LIGHTMAP_SIZE,
};
use regia_core::glam::{Mat3, Vec3};

use crate::bvh::{Aabb, Bvh};
use crate::{PtError, PtLight, PtScene, PtSky};

pub mod cells;
mod compare;
mod crease;
mod denoise;
pub mod density;
#[cfg(feature = "lightmap-gpu")]
mod gpu;
mod resume;
#[cfg(feature = "lightmap-gpu")]
pub(crate) use gpu::contract_rows as gpu_contract_rows;
mod single;
mod stitch;

pub use compare::{LIGHTMAP_GPU_PSNR_FLOOR_DB, LightmapComparison, compare_lightmaps};
#[cfg(feature = "lightmap-gpu")]
pub use gpu::bake_lightmap_gpu;
#[cfg(feature = "lightmap-gpu")]
pub use gpu::bake_lightmap_gpu_checkpointed;
#[cfg(feature = "lightmap-gpu")]
pub use gpu::{
    DEFAULT_GPU_TILE_RAYS, LightmapGpuControl, LightmapGpuOptions, LightmapGpuProgress, LightmapGpuStats,
    LightmapGpuTile, MAX_GPU_TILE_RAYS, MAX_GPU_YIELD, MIN_GPU_TILE_RAYS, bake_lightmap_gpu_with,
    lightmap_gpu_error_code, lightmap_gpu_schedule, lightmap_gpu_schedule_hash, plan_gpu_chunk,
};
pub use resume::{bake_lightmap_checkpointed, lightmap_input_hash};
pub use single::{MeshLightmapRequest, QuadBoxScene, bake_mesh_lightmap, mesh_layout, quad_box_scene};

/// Normal-similarity exponent of the filter.
const FILTER_NORMAL_POWER: i32 = 8;

/// Most hemisphere samples a texel may take ([`LightmapBakeSettings::samples`] and
/// [`LightmapBakeSettings::max_samples`]).
pub const MAX_LIGHTMAP_SAMPLES: u32 = 1 << 16;
/// Most path vertices after the texel ([`LightmapBakeSettings::bounces`]).
pub const MAX_LIGHTMAP_BOUNCES: u32 = 64;

/// Settings of a lightmap bake.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightmapBakeSettings {
    /// Hemisphere samples per texel.
    pub samples: u32,
    /// Path vertices after the texel (1 = light reaching the texel directly).
    pub bounces: u32,
    /// Random seed.
    pub seed: u32,
    /// Bake the sun seen directly at the texel (mobile, fully static lighting).
    pub direct_sun: bool,
    /// Run the edge-aware 3×3 filter.
    pub filter: bool,
    /// Worker threads (0 = the machine's parallelism).
    pub threads: usize,
    /// Share of hemisphere rays hitting back faces above which a texel is inside geometry.
    pub inside_threshold: f32,
    /// Bake the dominant light direction per texel (the `.flmap` v2 direction section), so
    /// normal maps bend the baked light at runtime.
    pub directional: bool,
    /// Adaptive sampling: the most samples a texel may take (0 = exactly `samples`). Only used
    /// with a [`Self::noise_target`].
    pub max_samples: u32,
    /// Adaptive sampling: a texel stops once the relative standard error of its luminance is at
    /// or below this (0 = off; 0.02 is 2 %).
    pub noise_target: f32,
    /// Run the à-trous denoiser with these settings (ADR-2314) instead of the 3×3 filter; the
    /// lightmap records them. `None` (the default here; `regia bake-lightmaps` passes
    /// `LightmapDenoiseInfo::default()` with its own pass count, 4 since ADR-4108, unless
    /// `--denoise off`) keeps the bake as before.
    pub denoise: Option<LightmapDenoiseInfo>,
    /// Crease-aware texel placement (ADR-2404): a texel whose centre lies inside geometry is
    /// sampled at the nearest point of its own chart outside it (`crease.rs`) instead of being
    /// dropped and refilled. Off by default here (every existing caller and golden bakes as
    /// before); `regia bake-lightmaps` turns it on for new bakes (`--crease off` keeps the old
    /// placement).
    pub crease_placement: bool,
    /// Seam stitching (ADR-2404, `docs/specs/lightmaps.md` § 4.5): after the gutters are filled,
    /// a least-squares solve makes the bilinear lookups on the two sides of every smooth chart
    /// seam agree, weighted toward the baked values (`stitch.rs`). Off by default here;
    /// `regia bake-lightmaps` turns it on for new bakes (`--stitch off`).
    pub stitch_seams: bool,
}

impl Default for LightmapBakeSettings {
    fn default() -> Self {
        Self {
            samples: 256,
            bounces: 3,
            seed: 1,
            direct_sun: false,
            filter: true,
            threads: 0,
            inside_threshold: 0.5,
            directional: true,
            max_samples: 0,
            noise_target: 0.0,
            denoise: None,
            crease_placement: false,
            stitch_seams: false,
        }
    }
}

impl LightmapBakeSettings {
    /// Checks the bounds: `samples` in `1..=MAX_LIGHTMAP_SAMPLES`, `max_samples` 0 or in
    /// `samples..=MAX_LIGHTMAP_SAMPLES`, `bounces` in `1..=`[`MAX_LIGHTMAP_BOUNCES`], and a finite,
    /// non-negative `noise_target` and `inside_threshold`.
    ///
    /// # Errors
    ///
    /// A message naming the first setting out of range.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_LIGHTMAP_SAMPLES).contains(&self.samples) {
            return Err(format!(
                "samples {} is outside 1..={MAX_LIGHTMAP_SAMPLES}",
                self.samples
            ));
        }
        if self.max_samples != 0 && !(self.samples..=MAX_LIGHTMAP_SAMPLES).contains(&self.max_samples) {
            return Err(format!(
                "max_samples {} is outside {}..={MAX_LIGHTMAP_SAMPLES} (or 0)",
                self.max_samples, self.samples
            ));
        }
        if self.bounces == 0 {
            return Err("bounces 0 is below 1 (1 is light reaching the texel directly)".into());
        }
        if self.bounces > MAX_LIGHTMAP_BOUNCES {
            return Err(format!(
                "bounces {} is above {MAX_LIGHTMAP_BOUNCES}",
                self.bounces
            ));
        }
        if !(self.noise_target.is_finite() && self.noise_target >= 0.0) {
            return Err(format!(
                "noise_target {} is not a non-negative number",
                self.noise_target
            ));
        }
        if !(self.inside_threshold.is_finite() && self.inside_threshold >= 0.0) {
            return Err(format!(
                "inside_threshold {} is not a non-negative number",
                self.inside_threshold
            ));
        }
        if let Some(d) = &self.denoise {
            d.validate().map_err(|m| format!("denoise {m}"))?;
        }
        Ok(())
    }

    /// The most samples one texel takes: `max_samples` when adaptive sampling is on, else
    /// `samples`.
    #[must_use]
    pub fn sample_cap(&self) -> u32 {
        if self.noise_target > 0.0 && self.max_samples > self.samples {
            self.max_samples
        } else {
            self.samples
        }
    }
}

/// The noise of a bake before any filter (relative standard error of each baked texel's mean
/// luminance; texels with no light count as exact).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct LightmapNoise {
    /// Mean relative standard error over the baked texels.
    pub mean: f32,
    /// 95th percentile.
    pub p95: f32,
    /// Largest.
    pub max: f32,
    /// Mean samples a baked texel took.
    pub mean_samples: f32,
    /// Texels that reached the sample cap above [`LightmapBakeSettings::noise_target`] (0
    /// without a target).
    pub texels_over_target: usize,
}

/// What a bake did.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct LightmapBakeReport {
    /// Entries baked.
    pub entries: usize,
    /// Texels whose centre lies on a surface.
    pub texels_baked: usize,
    /// Texels dropped as inside geometry (then refilled where a neighbour allowed).
    pub texels_inside: usize,
    /// Gutter texels filled from neighbours.
    pub texels_dilated: usize,
    /// Rays traced (hemisphere and shadow rays).
    pub rays: u64,
    /// Triangles in the scene.
    pub triangles: usize,
    /// Emissive triangles sampled as lights.
    pub emissive_triangles: usize,
    /// Lights of the source scene (point, spot, sphere and quad).
    pub lights: usize,
    /// Names of the baked light components (`LightmapContents`).
    pub contents: Vec<&'static str>,
    /// Noise before the filter; `None` when texels took fewer than 2 samples.
    pub noise: Option<LightmapNoise>,
    /// The lightmap carries a direction section.
    pub directional: bool,
    /// The denoiser ran ([`LightmapBakeSettings::denoise`]).
    pub denoised: bool,
    /// Mean relative standard error of the baked texels after the denoiser, as it carries each
    /// texel's variance through its passes (`None` without the denoiser or a measurable noise):
    /// compare with [`LightmapNoise::mean`].
    pub denoised_noise: Option<f32>,
    /// Texels the crease placement found inside geometry ([`LightmapBakeSettings::crease_placement`];
    /// 0 when it is off).
    pub texels_crease_inside: usize,
    /// Of those, texels sampled at a point of their chart outside the geometry instead.
    pub texels_relocated: usize,
    /// Gutter and inside texels next to a chart that took its light's gradient across the edge
    /// instead of its neighbours' mean (crease placement; counted in `texels_dilated` too).
    pub texels_extrapolated: usize,
    /// Smooth chart-seam edges the stitch solved over ([`LightmapBakeSettings::stitch_seams`]; 0
    /// when it is off or no entry has a seam).
    pub seam_edges_stitched: usize,
    /// Texels the stitch changed (baked and gutter texels the seams' lookups read).
    pub texels_stitched: usize,
}

/// What one texel's estimate came to.
#[derive(Clone, Copy, Debug, Default)]
struct TexelEstimate {
    /// Mean irradiance.
    e: Vec3,
    /// First hemisphere rays that hit a back face.
    back: u32,
    /// Samples taken.
    taken: u32,
    /// Luminance-weighted mean light direction (`|d| <= 1`).
    dir: Vec3,
    /// Relative standard error of the mean luminance (0 for a black texel, NaN below 2
    /// samples).
    rel_err: f32,
}

/// One world-space triangle of the flattened scene.
#[derive(Clone, Copy, Debug)]
struct Tri {
    p: [Vec3; 3],
    n: [Vec3; 3],
    face: Vec3,
    albedo: Vec3,
}

/// An emissive triangle sampled as a light.
#[derive(Clone, Copy, Debug)]
struct Emitter {
    tri: usize,
    radiance: Vec3,
    two_sided: bool,
    area: f32,
    /// Made from a [`PtLight::Quad`] (a local light, not an emissive material).
    quad: bool,
}

/// A [`PtScene`] flattened to world triangles for the CPU tracer.
#[derive(Clone, Debug)]
pub struct LightmapScene {
    tris: Vec<Tri>,
    bvh: Bvh,
    /// Per instance: world positions, world normals, indices (as the instance's mesh), first
    /// triangle and whether it may receive a lightmap.
    instances: Vec<InstanceGeometry>,
    emitters: Vec<Emitter>,
    /// Running sum of emitter power for selection.
    emitter_cdf: Vec<f32>,
    /// Point, spot and sphere lights (quads are emitter triangles).
    lights: Vec<PtLight>,
    /// Quad lights turned into emitter instances.
    quad_lights: usize,
    sun: Option<crate::PtSun>,
    /// The frame's extra directional lights (ADR-7254), lit like the sun.
    extra_suns: Vec<crate::PtSun>,
    sky: Vec3,
    has_sky: bool,
}

/// One instance's world-space geometry.
#[derive(Clone, Debug)]
pub struct InstanceGeometry {
    /// World positions, metres.
    pub positions: Vec<[f32; 3]>,
    /// World unit normals (zero where the source had none).
    pub normals: Vec<[f32; 3]>,
    /// Triangle list (the mesh's own).
    pub indices: Vec<u32>,
    /// Index of the instance's first triangle in the flattened scene.
    first_tri: usize,
    /// Neither emissive, unlit nor a quad light: it can hold a lightmap.
    pub receiver: bool,
}

fn lum(c: Vec3) -> f32 {
    0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z
}

impl LightmapScene {
    /// Flattens `scene`: every instance's triangles in world space with their material's
    /// diffuse albedo, the emissive triangles, the analytic lights, the sun and the sky. Each
    /// [`PtLight::Quad`] becomes an emitter instance (two triangles, not a receiver) appended
    /// after the scene's own instances, so the scene's instance indices stay valid.
    ///
    /// # Errors
    ///
    /// [`PtError::Scene`] when an instance names a missing mesh or material, or a mesh index is
    /// out of range.
    pub fn from_pt_scene(scene: &PtScene) -> Result<Self, PtError> {
        let _span =
            tracing::info_span!("lightmap_scene_flatten", instances = scene.instances.len()).entered();
        let own_instances = scene.instances.len();
        let quad_lights = scene
            .lights
            .iter()
            .filter(|l| matches!(l, PtLight::Quad { .. }))
            .count();
        let with_quads;
        let scene = if quad_lights > 0 {
            with_quads = crate::pack::quads_as_meshes(scene);
            &with_quads
        } else {
            scene
        };
        let mut tris = Vec::new();
        let mut instances = Vec::with_capacity(scene.instances.len());
        let mut emitters = Vec::new();
        for (k, inst) in scene.instances.iter().enumerate() {
            let mesh = scene
                .meshes
                .get(inst.mesh)
                .ok_or_else(|| PtError::Scene(format!("instance {k}: no mesh {}", inst.mesh)))?;
            let mat = scene
                .materials
                .get(inst.material)
                .ok_or_else(|| PtError::Scene(format!("instance {k}: no material {}", inst.material)))?;
            if !mesh.indices.len().is_multiple_of(3) {
                return Err(PtError::Scene(format!(
                    "instance {k}: index count is not a multiple of 3"
                )));
            }
            if let Some(i) = mesh.indices.iter().find(|&&i| i as usize >= mesh.positions.len()) {
                return Err(PtError::Scene(format!(
                    "instance {k}: index {i} past the vertices"
                )));
            }
            let m = inst.transform;
            let normal_m = Mat3::from_mat4(m).inverse().transpose();
            let positions: Vec<[f32; 3]> = mesh
                .positions
                .iter()
                .map(|p| m.transform_point3(Vec3::from_array(*p)).to_array())
                .collect();
            // A singular transform has no inverse: its non-finite normals become zero and the
            // triangles fall back to their face normal.
            let normals: Vec<[f32; 3]> = (0..mesh.positions.len())
                .map(|i| {
                    let n = normal_m * Vec3::from_array(mesh.normals.get(i).copied().unwrap_or([0.0; 3]));
                    if n.is_finite() {
                        n.normalize_or_zero().to_array()
                    } else {
                        [0.0; 3]
                    }
                })
                .collect();
            let base = Vec3::new(mat.base_color[0], mat.base_color[1], mat.base_color[2]);
            let albedo = if mat.unlit {
                Vec3::ZERO
            } else {
                (base * (1.0 - mat.metallic.clamp(0.0, 1.0))).clamp(Vec3::ZERO, Vec3::splat(0.98))
            };
            let emission = if mat.unlit {
                base
            } else {
                Vec3::from_array(mat.emissive)
            };
            let first_tri = tris.len();
            for t in mesh.indices.as_chunks::<3>().0 {
                let p = t.map(|i| Vec3::from_array(positions[i as usize]));
                let face_raw = (p[1] - p[0]).cross(p[2] - p[0]);
                let face = face_raw.normalize_or_zero();
                let n = t.map(|i| {
                    let v = Vec3::from_array(normals[i as usize]);
                    if v == Vec3::ZERO { face } else { v }
                });
                if emission.max_element() > 0.0 && face != Vec3::ZERO {
                    emitters.push(Emitter {
                        tri: tris.len(),
                        radiance: emission,
                        two_sided: !mat.one_sided_emission,
                        area: 0.5 * face_raw.length(),
                        quad: k >= own_instances,
                    });
                }
                tris.push(Tri { p, n, face, albedo });
            }
            instances.push(InstanceGeometry {
                positions,
                normals,
                indices: mesh.indices.clone(),
                first_tri,
                // Quad lights (appended past the scene's own instances) are never receivers.
                receiver: k < own_instances && !mat.unlit && !mat.is_emissive(),
            });
        }
        let bounds: Vec<Aabb> = tris
            .iter()
            .map(|t| Aabb::from_points(&t.p.map(|v| v.to_array())))
            .collect();
        let bvh = Bvh::build(&bounds);
        let mut acc = 0.0f32;
        let emitter_cdf = emitters
            .iter()
            .map(|e| {
                acc += e.area * lum(e.radiance) * if e.two_sided { 2.0 } else { 1.0 };
                acc
            })
            .collect();
        let (sky, has_sky) = match &scene.sky {
            PtSky::None => (Vec3::ZERO, false),
            PtSky::Uniform(c) => (Vec3::from_array(*c), true),
            PtSky::Equirect {
                width,
                height,
                texels,
                scale,
                ..
            } => (
                equirect_mean(*width, *height, texels) * *scale,
                !texels.is_empty(),
            ),
            PtSky::Table { texels, scale, .. } => {
                let n = texels.len().max(1) as f32;
                let sum = texels
                    .iter()
                    .fold(Vec3::ZERO, |a, t| a + Vec3::new(t[0], t[1], t[2]));
                (sum / n * *scale, !texels.is_empty())
            }
        };
        Ok(Self {
            tris,
            bvh,
            instances,
            emitters,
            emitter_cdf,
            lights: scene.lights.clone(),
            quad_lights,
            sun: scene.sun,
            extra_suns: scene
                .extra_suns
                .iter()
                .copied()
                .filter(|s| s.illuminance > 0.0)
                .collect(),
            sky,
            has_sky: has_sky && sky.max_element() > 0.0,
        })
    }

    /// The instances' world geometry, in scene order.
    #[must_use]
    pub fn instances(&self) -> &[InstanceGeometry] {
        &self.instances
    }

    /// Triangles in the scene.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.tris.len()
    }

    /// The light components a bake with `settings` holds (`LightmapContents` bits).
    #[must_use]
    pub fn contents(&self, settings: &LightmapBakeSettings) -> u32 {
        let mut c = 0;
        if self.has_sky {
            c |= LightmapContents::SKY;
        }
        if self.sun.is_some_and(|s| s.illuminance > 0.0) || !self.extra_suns.is_empty() {
            if settings.bounces >= 2 {
                c |= LightmapContents::SUN_BOUNCE;
            }
            if settings.direct_sun {
                c |= LightmapContents::SUN_DIRECT;
            }
        }
        if self.emitters.iter().any(|e| !e.quad) {
            c |= LightmapContents::EMISSIVE;
        }
        if !self.lights.is_empty() || self.quad_lights > 0 {
            c |= LightmapContents::LOCAL_LIGHTS;
        }
        c
    }

    /// Lights of the source scene: point, spot, sphere and quad.
    #[must_use]
    pub fn light_count(&self) -> usize {
        self.lights.len() + self.quad_lights
    }

    /// Closest hit along `o + t·d`, `t` in `(0, t_max)`: (triangle, t, barycentric u, v).
    fn intersect(&self, o: Vec3, d: Vec3, t_max: f32, any: bool) -> Option<(usize, f32, f32, f32)> {
        // An empty scene's BVH is one zeroed node that is not a leaf: nothing to hit.
        if self.tris.is_empty() {
            return None;
        }
        let inv = Vec3::ONE / d;
        let mut best: Option<(usize, f32, f32, f32)> = None;
        let mut limit = t_max;
        let mut stack = NodeStack::default();
        stack.push(0);
        while let Some(ni) = stack.pop() {
            let node = &self.bvh.nodes[ni as usize];
            if !slab(o, inv, node.min, node.max, limit) {
                continue;
            }
            if node.is_leaf() {
                for j in node.a..node.a + node.count {
                    let ti = self.bvh.order[j as usize] as usize;
                    if let Some((t, u, v)) = ray_triangle(o, d, &self.tris[ti].p)
                        && t < limit
                    {
                        limit = t;
                        best = Some((ti, t, u, v));
                        if any {
                            return best;
                        }
                    }
                }
            } else {
                stack.push(node.a + 1);
                stack.push(node.a);
            }
        }
        best
    }

    /// No occluder between the (already offset) origin `o` and the point `dist` along `d`; the
    /// far end stops short by a relative 1e-4 so the light's own surface does not occlude it.
    fn visible(&self, o: Vec3, d: Vec3, dist: f32, rays: &mut u64) -> bool {
        *rays += 1;
        self.intersect(o, d, dist * (1.0 - 1e-4), true).is_none()
    }

    /// Irradiance at `p` (normal `n`, ray origin `o` already offset) from the lights picked
    /// by next-event estimation. Each contribution also adds `luminance · direction` and its
    /// luminance to `dir_acc`.
    fn direct(
        &self,
        o: Vec3,
        n: Vec3,
        sun: bool,
        rng: &mut Rng,
        rays: &mut u64,
        dir_acc: &mut (Vec3, f32),
    ) -> Vec3 {
        let mut e = Vec3::ZERO;
        let mut add = |e: &mut Vec3, c: Vec3, dir: Vec3| {
            *e += c;
            let l = lum(c);
            dir_acc.0 += dir * l;
            dir_acc.1 += l;
        };
        if sun && let Some(s) = self.sun {
            let toward = Vec3::from_array(s.toward).normalize_or_zero();
            let dir = if s.angular_radius > 0.0 {
                sample_cone(toward, s.angular_radius.cos(), rng.uniform(), rng.uniform())
            } else {
                toward
            };
            let cos = n.dot(dir);
            if cos > 0.0 && s.illuminance > 0.0 && self.visible(o, dir, f32::INFINITY, rays) {
                add(&mut e, Vec3::from_array(s.color) * s.illuminance * cos, dir);
            }
        }
        // The frame's extra directional lights (ADR-7254), as the sun; none in a one-sun scene,
        // so its random stream and bytes are unchanged.
        if sun {
            for s in &self.extra_suns {
                let toward = Vec3::from_array(s.toward).normalize_or_zero();
                let dir = if s.angular_radius > 0.0 {
                    sample_cone(toward, s.angular_radius.cos(), rng.uniform(), rng.uniform())
                } else {
                    toward
                };
                let cos = n.dot(dir);
                if cos > 0.0 && self.visible(o, dir, f32::INFINITY, rays) {
                    add(&mut e, Vec3::from_array(s.color) * s.illuminance * cos, dir);
                }
            }
        }
        for light in &self.lights {
            let (c, dir) = self.local_light(light, o, n, rays);
            add(&mut e, c, dir);
        }
        if let Some(&total) = self.emitter_cdf.last()
            && total > 0.0
        {
            let pick = rng.uniform() * total;
            let k = self
                .emitter_cdf
                .partition_point(|&c| c <= pick)
                .min(self.emitters.len() - 1);
            let em = self.emitters[k];
            let weight = em.area * lum(em.radiance) * if em.two_sided { 2.0 } else { 1.0 };
            let p_pick = weight / total;
            let t = &self.tris[em.tri];
            let (r1, r2) = (rng.uniform().sqrt(), rng.uniform());
            let q = t.p[0] * (1.0 - r1) + t.p[1] * (r1 * (1.0 - r2)) + t.p[2] * (r1 * r2);
            let to = q - o;
            let d2 = to.length_squared();
            if d2 > 1e-12 && p_pick > 0.0 {
                let dist = d2.sqrt();
                let dir = to / dist;
                let cos_r = n.dot(dir);
                let facing = -t.face.dot(dir);
                let cos_l = if em.two_sided { facing.abs() } else { facing };
                if cos_r > 0.0 && cos_l > 0.0 && self.visible(o, dir, dist, rays) {
                    add(&mut e, em.radiance * (cos_r * cos_l / d2 * em.area / p_pick), dir);
                }
            }
        }
        e
    }

    /// Irradiance from one analytic light and the unit direction toward it.
    fn local_light(&self, light: &PtLight, o: Vec3, n: Vec3, rays: &mut u64) -> (Vec3, Vec3) {
        let point = |pos: [f32; 3], intensity: Vec3, rays: &mut u64| -> (Vec3, Vec3) {
            let to = Vec3::from_array(pos) - o;
            let d2 = to.length_squared().max(1e-8);
            let dist = d2.sqrt();
            let dir = to / dist;
            let cos = n.dot(dir);
            if cos <= 0.0 || !self.visible(o, dir, dist, rays) {
                return (Vec3::ZERO, dir);
            }
            (intensity * cos / d2, dir)
        };
        match *light {
            PtLight::Point { position, intensity } => point(position, Vec3::from_array(intensity), rays),
            PtLight::Spot {
                position,
                direction,
                intensity,
                inner_cos,
                outer_cos,
            } => {
                let (e, dir) = point(position, Vec3::from_array(intensity), rays);
                (
                    e * spot_falloff(dir, Vec3::from_array(direction), inner_cos, outer_cos),
                    dir,
                )
            }
            PtLight::Sphere {
                position,
                radius,
                radiance,
            } => {
                // Inside the sphere nothing is lit (the path tracer's `sample_light`); outside,
                // `π r² L / d²` never exceeds the `π L` of a sphere filling the hemisphere.
                if (Vec3::from_array(position) - o).length_squared() <= radius * radius {
                    return (Vec3::ZERO, Vec3::ZERO);
                }
                let intensity = Vec3::from_array(radiance) * (std::f32::consts::PI * radius * radius);
                point(position, intensity, rays)
            }
            // A sized spot bakes as the delta spot of the same intensity (ADR-2592).
            PtLight::SphereSpot {
                position,
                radius,
                radiance,
                direction,
                inner_cos,
                outer_cos,
            } => {
                if (Vec3::from_array(position) - o).length_squared() <= radius * radius {
                    return (Vec3::ZERO, Vec3::ZERO);
                }
                let intensity = Vec3::from_array(radiance) * (std::f32::consts::PI * radius * radius);
                let (e, dir) = point(position, intensity, rays);
                (
                    e * spot_falloff(dir, Vec3::from_array(direction), inner_cos, outer_cos),
                    dir,
                )
            }
            // `from_pt_scene` turned every quad into emitter triangles: they are sampled with
            // the emissive surfaces and occlude like any surface.
            PtLight::Quad { .. } => (Vec3::ZERO, Vec3::ZERO),
        }
    }

    /// Irradiance estimate at a surface point, with its direction and noise. Takes `samples`
    /// samples, then (adaptive sampling) more `samples` at a time while the relative standard
    /// error is above the target and the cap allows.
    fn texel_irradiance(
        &self,
        p: Vec3,
        n: Vec3,
        face: Vec3,
        s: &LightmapBakeSettings,
        key: u32,
        rays: &mut u64,
    ) -> TexelEstimate {
        let mut sum = Vec3::ZERO;
        let mut back = 0u32;
        let (mut lum_sum, mut lum_sq) = (0.0f64, 0.0f64);
        let mut dir_acc = (Vec3::ZERO, 0.0f32);
        let o0 = offset_origin(p, face);
        let cap = s.sample_cap();
        let batch = s.samples.max(1);
        let shift = sobol_shift(s.seed, key);
        let mut taken = 0u32;
        while taken < cap {
            let end = taken.saturating_add(batch).min(cap);
            for sample in taken..end {
                let mut rng = Rng::new(s.seed, key, sample);
                let mut e = self.direct(o0, n, s.direct_sun, &mut rng, rays, &mut dir_acc);
                // Light brought back by the hemisphere path arrives along its first direction.
                let mut first_dir = Vec3::ZERO;
                let mut carried = Vec3::ZERO;
                let mut scratch = (Vec3::ZERO, 0.0f32);
                let mut throughput = Vec3::ONE;
                let (mut o, mut nn, mut nf) = (o0, n, face);
                for depth in 0..s.bounces.max(1) {
                    let (u1, u2) = (rng.uniform(), rng.uniform());
                    // The texel's own hemisphere takes the stratified point (ADR-4108); the
                    // stream's two numbers are still drawn, so every later dimension is unchanged.
                    let (u1, u2) = if depth == 0 {
                        sobol_02(sample, shift)
                    } else {
                        (u1, u2)
                    };
                    let d = cosine_sample(nn, u1, u2);
                    if depth == 0 {
                        first_dir = d;
                    }
                    // Below the geometric surface: the surface itself is in the way.
                    if d.dot(nf) <= 0.0 {
                        break;
                    }
                    *rays += 1;
                    match self.intersect(o, d, f32::INFINITY, false) {
                        None => {
                            if self.has_sky {
                                let c = throughput * self.sky * std::f32::consts::PI;
                                e += c;
                                carried += c;
                            }
                            break;
                        }
                        Some((ti, _, u, v)) => {
                            let tri = &self.tris[ti];
                            if tri.face.dot(d) > 0.0 {
                                if depth == 0 {
                                    back += 1;
                                }
                                break;
                            }
                            if depth + 1 >= s.bounces {
                                break;
                            }
                            throughput *= tri.albedo;
                            if throughput.max_element() <= 0.0 {
                                break;
                            }
                            // The hit point from its barycentrics (exact on the triangle's plane,
                            // unlike `o + d·t`), kept off the triangle's edges, then offset like
                            // the texel's origin (`bounce_origin`).
                            let sn = (tri.n[0] * (1.0 - u - v) + tri.n[1] * u + tri.n[2] * v)
                                .normalize_or(tri.face);
                            let sn = if sn.dot(tri.face) < 0.0 { tri.face } else { sn };
                            o = bounce_origin(&tri.p, u, v, tri.face);
                            nn = sn;
                            nf = tri.face;
                            let c = throughput * self.direct(o, nn, true, &mut rng, rays, &mut scratch);
                            e += c;
                            carried += c;
                        }
                    }
                }
                let l = lum(carried);
                dir_acc.0 += first_dir * l;
                dir_acc.1 += l;
                sum += e;
                let le = f64::from(lum(e));
                lum_sum += le;
                lum_sq += le * le;
            }
            taken = end;
            if s.noise_target <= 0.0 || relative_error(taken, lum_sum, lum_sq) <= s.noise_target {
                break;
            }
        }
        let dir = if dir_acc.1 > 0.0 {
            dir_acc.0 / dir_acc.1
        } else {
            Vec3::ZERO
        };
        TexelEstimate {
            e: sum / taken.max(1) as f32,
            back,
            taken,
            dir,
            rel_err: relative_error(taken, lum_sum, lum_sq),
        }
    }
}

/// The relative standard error of a texel's mean luminance from its sums: 0 for a black texel, NaN
/// below 2 samples.
fn relative_error(taken: u32, lum_sum: f64, lum_sq: f64) -> f32 {
    if taken < 2 {
        return f32::NAN;
    }
    let k = f64::from(taken);
    let mean = lum_sum / k;
    if mean <= 0.0 {
        return 0.0;
    }
    let var = ((lum_sq - k * mean * mean) / (k - 1.0)).max(0.0);
    ((var / k).sqrt() / mean) as f32
}

/// The traversal stack: 64 entries inline (no allocation per ray), spilling to the heap for a
/// deeper hierarchy.
struct NodeStack {
    fixed: [u32; 64],
    len: usize,
    spill: Vec<u32>,
}

impl Default for NodeStack {
    fn default() -> Self {
        Self {
            fixed: [0; 64],
            len: 0,
            spill: Vec::new(),
        }
    }
}

impl NodeStack {
    fn push(&mut self, node: u32) {
        if self.len < self.fixed.len() {
            self.fixed[self.len] = node;
            self.len += 1;
        } else {
            self.spill.push(node);
        }
    }

    fn pop(&mut self) -> Option<u32> {
        if let Some(node) = self.spill.pop() {
            return Some(node);
        }
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.fixed[self.len])
    }
}

/// Ray/box slab test against `t_max`.
fn slab(o: Vec3, inv: Vec3, min: [f32; 3], max: [f32; 3], t_max: f32) -> bool {
    let t0 = (Vec3::from_array(min) - o) * inv;
    let t1 = (Vec3::from_array(max) - o) * inv;
    let near = t0.min(t1).max_element().max(0.0);
    let far = t0.max(t1).min_element().min(t_max);
    near <= far
}

/// Barycentric slack of a hit. Möller–Trumbore is not watertight: a ray through the edge two
/// triangles share can round a hair outside both and pass between them, through a closed wall
/// (sky light inside the CLI's sealed room on aarch64, ADR-3287). With the slack such a ray is
/// taken by at least one of them; `bounce_origin` pulls a hit just outside back inside.
const EDGE_SLACK: f32 = 1e-5;

/// Möller–Trumbore, both faces, with [`EDGE_SLACK`]: `(t, u, v)` for `t > 0`.
fn ray_triangle(o: Vec3, d: Vec3, p: &[Vec3; 3]) -> Option<(f32, f32, f32)> {
    let e1 = p[1] - p[0];
    let e2 = p[2] - p[0];
    let pv = d.cross(e2);
    let det = e1.dot(pv);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let tv = o - p[0];
    let u = tv.dot(pv) * inv;
    if !(-EDGE_SLACK..=1.0 + EDGE_SLACK).contains(&u) {
        return None;
    }
    let qv = tv.cross(e1);
    let v = d.dot(qv) * inv;
    if v < -EDGE_SLACK || u + v > 1.0 + EDGE_SLACK {
        return None;
    }
    let t = e2.dot(qv) * inv;
    (t > 0.0).then_some((t, u, v))
}

/// A ray origin just off the surface at `p` on the side of `n` (Wächter & Binder 2019, *Ray
/// Tracing Gems* ch. 6): each coordinate moves by `256 · n` units in the last place, so the step
/// grows with the coordinate's own precision; near the origin, where units in the last place
/// vanish, it moves by `n / 65536` m instead.
fn offset_origin(p: Vec3, n: Vec3) -> Vec3 {
    const ORIGIN: f32 = 1.0 / 32.0;
    const FLOAT_SCALE: f32 = 1.0 / 65_536.0;
    const INT_SCALE: f32 = 256.0;
    let (p, n) = (p.to_array(), n.to_array());
    Vec3::from_array(std::array::from_fn(|k| {
        if p[k].abs() < ORIGIN {
            return p[k] + FLOAT_SCALE * n[k];
        }
        // Units in the last place grow away from zero: step the bits up on the side of `n`.
        let ulps = (INT_SCALE * n[k]) as i32;
        let step = if p[k] < 0.0 { -ulps } else { ulps };
        f32::from_bits((p[k].to_bits() as i32).wrapping_add(step) as u32)
    }))
}

/// The runtime's spot cone (`spot_attenuation` in `pbr_lighting.wgsl`, the path tracer's
/// `sample_light`): a linear ramp from 0 at the outer cosine to 1 at the inner one. `to_light` is
/// the unit direction from the surface to the light; `spot_dir` points from the light into the
/// scene.
fn spot_falloff(to_light: Vec3, spot_dir: Vec3, inner_cos: f32, outer_cos: f32) -> f32 {
    let cos = -to_light.dot(spot_dir.normalize_or_zero());
    ((cos - outer_cos) / (inner_cos - outer_cos).max(1e-4)).clamp(0.0, 1.0)
}

/// FNV-1a of an entry name: the per-entry part of the texel random streams.
fn name_hash(name: &str) -> u32 {
    name.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// Orthonormal basis around unit `n` (Duff et al. 2017).
fn basis(n: Vec3) -> (Vec3, Vec3) {
    let sign = 1.0f32.copysign(n.z);
    let a = -1.0 / (sign + n.z);
    let b = n.x * n.y * a;
    (
        Vec3::new(1.0 + sign * n.x * n.x * a, sign * b, -sign * n.x),
        Vec3::new(b, sign + n.y * n.y * a, -n.y),
    )
}

/// Cosine-weighted direction around `n` (Malley's method).
fn cosine_sample(n: Vec3, u1: f32, u2: f32) -> Vec3 {
    let (t, b) = basis(n);
    let r = u1.sqrt();
    let phi = std::f32::consts::TAU * u2;
    (t * (r * phi.cos()) + b * (r * phi.sin()) + n * (1.0 - u1).max(0.0).sqrt()).normalize_or(n)
}

/// Uniform direction in the cone of `cos_max` around `axis`.
fn sample_cone(axis: Vec3, cos_max: f32, u1: f32, u2: f32) -> Vec3 {
    let (t, b) = basis(axis);
    let cos = 1.0 - u1 * (1.0 - cos_max);
    let sin = (1.0 - cos * cos).max(0.0).sqrt();
    let phi = std::f32::consts::TAU * u2;
    (t * (sin * phi.cos()) + b * (sin * phi.sin()) + axis * cos).normalize_or(axis)
}

/// PCG hash (Jarzynski & Olano 2020).
fn pcg(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// Point `i` of the first two dimensions of the Sobol sequence, a (0,2)-sequence in base 2 (the
/// radical inverse and its companion; Kollig & Keller 2002, *Efficient Multidimensional
/// Sampling*), XOR-shifted by `shift` (a random digital shift, which keeps every elementary
/// interval of every power-of-two prefix holding exactly one point). Used for a texel's
/// hemisphere directions (ADR-4108): its `samples` cosine-weighted rays cover the projected disk
/// in strata instead of independently, so the count that finds a small bright region (a sunlit
/// floor through a skylight) varies less from texel to texel. Mirrored by `lightmap_bake.wgsl`.
fn sobol_02(i: u32, shift: [u32; 2]) -> (f32, f32) {
    let x = i.reverse_bits();
    let (mut y, mut v, mut k) = (0u32, 1u32 << 31, i);
    while k != 0 {
        if k & 1 != 0 {
            y ^= v;
        }
        k >>= 1;
        v ^= v >> 1;
    }
    let unit = |b: u32| (b >> 8) as f32 * (1.0 / 16_777_216.0);
    (unit(x ^ shift[0]), unit(y ^ shift[1]))
}

/// The digital shift of a texel's [`sobol_02`] points: a hash of the seed and the texel's key, so
/// neighbouring texels' strata do not line up.
fn sobol_shift(seed: u32, key: u32) -> [u32; 2] {
    let a = pcg(key ^ pcg(seed ^ 0x5bd1_e995));
    [a, pcg(a ^ 0x68e3_1da4)]
}

/// A per-(texel, sample) random stream.
struct Rng(u32);

impl Rng {
    fn new(seed: u32, texel: u32, sample: u32) -> Self {
        Self(pcg(seed ^ pcg(texel ^ pcg(sample.wrapping_add(0x9e37_79b9)))))
    }

    /// Uniform in `[0, 1)`.
    fn uniform(&mut self) -> f32 {
        self.0 = pcg(self.0);
        (self.0 >> 8) as f32 * (1.0 / 16_777_216.0)
    }
}

/// A texel to bake.
#[derive(Clone, Copy, Debug)]
struct Task {
    /// Atlas texel index.
    texel: usize,
    /// Random-stream key: the entry's name and the texel's place inside the entry.
    key: u32,
    p: Vec3,
    n: Vec3,
    face: Vec3,
}

/// Bakes `layout` over `scene`: entry `e` is instance `receivers[e]` (its unwrap's `xref` names
/// that instance's vertices). `progress(done, total)` is called from the workers as texels finish.
///
/// # Errors
///
/// [`PtError::Scene`] when `receivers` does not match the entries, an instance is missing, an
/// unwrap's vertices do not match its instance, the atlas is outside `1..=MAX_LIGHTMAP_SIZE`, a
/// rectangle leaves the atlas or an unwrap index or UV2 is out of range (all checked before any
/// texel is touched); a lightmap that fails to encode.
pub fn bake_lightmap(
    scene: &LightmapScene,
    layout: LightmapLayout,
    receivers: &[usize],
    settings: &LightmapBakeSettings,
    progress: impl Fn(usize, usize) + Sync,
) -> Result<(Lightmap, LightmapBakeReport), PtError> {
    let _span = tracing::info_span!("lightmap_bake", entries = layout.entries.len()).entered();
    let prepared = prepare_tasks(scene, &layout, receivers, settings)?;
    let total = prepared.tasks.len();
    let done = AtomicUsize::new(0);
    let (estimates, rays) = estimate_tasks(scene, &prepared.tasks, settings, &done, total, &progress);
    finish_bake(scene, layout, settings, &prepared, &estimates, rays)
}

/// Estimates `tasks` on the bake's worker threads, one consecutive share each, in task order;
/// `done` counts texels for `progress(done, total)`. Each estimate depends only on its task and
/// the settings (its random stream is keyed by the task), so how the tasks are split never changes
/// a bit (a checkpointed bake, `resume.rs`, calls this per batch).
fn estimate_tasks(
    scene: &LightmapScene,
    tasks: &[Task],
    settings: &LightmapBakeSettings,
    done: &AtomicUsize,
    total: usize,
    progress: &(impl Fn(usize, usize) + Sync),
) -> (Vec<TexelEstimate>, u64) {
    let threads = worker_threads(settings);
    let chunk = tasks.len().div_ceil(threads).max(1);
    let results: Vec<(Vec<TexelEstimate>, u64)> = std::thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    let _span = tracing::info_span!("lightmap_bake_worker", texels = part.len()).entered();
                    let mut rays = 0u64;
                    let mut out = Vec::with_capacity(part.len());
                    for (i, task) in part.iter().enumerate() {
                        out.push(
                            scene.texel_irradiance(task.p, task.n, task.face, settings, task.key, &mut rays),
                        );
                        if (i + 1).is_multiple_of(256) || i + 1 == part.len() {
                            let k = (i + 1) % 256;
                            let step = if k == 0 { 256 } else { k };
                            let now = done.fetch_add(step, Ordering::Relaxed) + step;
                            progress(now, total);
                        }
                    }
                    (out, rays)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    let estimates: Vec<TexelEstimate> = results.iter().flat_map(|(r, _)| r.iter().copied()).collect();
    let rays = results.iter().map(|(_, r)| r).sum();
    (estimates, rays)
}

/// The worker threads of a bake: `settings.threads`, or the machine's parallelism for 0.
fn worker_threads(settings: &LightmapBakeSettings) -> usize {
    if settings.threads == 0 {
        std::thread::available_parallelism().map_or(1, usize::from)
    } else {
        settings.threads
    }
    .clamp(1, 256)
}

/// What the raster found: every texel to bake, and the per-texel tables the filter and the gutters
/// need.
struct Prepared {
    tasks: Vec<Task>,
    normals: Vec<Vec3>,
    entry_of: Vec<u32>,
    /// The chart (unique across entries) of each texel a triangle claimed; `u32::MAX` elsewhere.
    chart_of: Vec<u32>,
    /// World size of a texel of each texel's entry, metres.
    texel_size: Vec<f32>,
    /// World offset of one atlas texel step in x and y on each claimed texel's triangle (the
    /// denoiser's taps across a chart border; zero elsewhere).
    axes: Vec<[Vec3; 2]>,
    /// What the crease placement did.
    crease: crease::CreaseStats,
    /// The scene instance of each entry.
    receivers: Vec<usize>,
    w: usize,
    h: usize,
}

/// Checks `layout` against `scene` and rasterises every entry's triangles into texel tasks.
fn prepare_tasks(
    scene: &LightmapScene,
    layout: &LightmapLayout,
    receivers: &[usize],
    settings: &LightmapBakeSettings,
) -> Result<Prepared, PtError> {
    if receivers.len() != layout.entries.len() || layout.unwraps.len() != layout.entries.len() {
        return Err(PtError::Scene(format!(
            "{} receivers, {} entries and {} unwraps",
            receivers.len(),
            layout.entries.len(),
            layout.unwraps.len()
        )));
    }
    settings
        .validate()
        .map_err(|m| PtError::Scene(format!("lightmap settings: {m}")))?;
    check_layout(layout)?;
    // A paged atlas (ADR-3300) is rasterised on its stacked grid: page `p`'s rows follow page
    // `p - 1`'s, the order of `Lightmap::texels`. Filter and gutters stay inside an entry's
    // rectangle or chart; the denoiser's off-chart taps find the texel continuing the surface by
    // world position (any entry, `denoise` module docs). Stitch samples with `page_rows` = this
    // layout's height, saved before the stack, and each entry's own page, so a seam on a page's
    // first or last row does not read the page next to it.
    let stacked = layout.stacked();
    let layout = &stacked;
    let (w, h) = (layout.width as usize, layout.height as usize);
    let n_texels = w * h;
    let mut tasks: Vec<Task> = Vec::new();
    let mut claimed = vec![false; n_texels];
    let mut normals = vec![Vec3::ZERO; n_texels];
    let mut entry_of = vec![u32::MAX; n_texels];
    let mut chart_of = vec![u32::MAX; n_texels];
    let mut texel_size = vec![0.0f32; n_texels];
    let mut axes = vec![[Vec3::ZERO; 2]; n_texels];
    let mut sources: Vec<crease::TaskSource> = Vec::new();
    let mut entry_charts: Vec<crease::InstanceCharts> = Vec::with_capacity(layout.entries.len());
    let mut chart_base = 0u32;
    for (e, (entry, unwrap)) in layout.entries.iter().zip(&layout.unwraps).enumerate() {
        let inst = scene
            .instances
            .get(receivers[e])
            .ok_or_else(|| PtError::Scene(format!("entry `{}`: no instance {}", entry.name, receivers[e])))?;
        if entry.source_vertices as usize != inst.positions.len()
            || unwrap.indices.len() != inst.indices.len()
        {
            return Err(PtError::Scene(format!(
                "entry `{}`: the unwrap does not match instance {} ({} vertices, {} indices)",
                entry.name,
                receivers[e],
                inst.positions.len(),
                inst.indices.len()
            )));
        }
        let [rx, ry, rw, rh] = entry.rect.map(|v| v as usize);
        let entry_key = name_hash(&entry.name);
        let borders = chart_border_edges(&unwrap.indices);
        let (tri_chart, charts) = denoise::triangle_charts(&unwrap.indices, unwrap.xref.len());
        let size = if entry.texels_per_metre > 0.0 {
            1.0 / entry.texels_per_metre
        } else {
            1.0
        };
        for (y, x) in (ry..ry + rh).flat_map(|y| (rx..rx + rw).map(move |x| (y, x))) {
            entry_of[y * w + x] = e as u32;
            texel_size[y * w + x] = size;
        }
        for (k, t) in unwrap.indices.as_chunks::<3>().0.iter().enumerate() {
            let uv = t.map(|i| {
                let q = unwrap.uv[i as usize];
                [q[0] * rw as f32, q[1] * rh as f32]
            });
            let area = edge(uv[0], uv[1], uv[2]);
            if area.abs() < 1e-12 {
                continue;
            }
            let tri = &scene.tris[inst.first_tri + k];
            // World offset per atlas texel in x and y: P · E⁻¹, E the triangle's texel-space
            // edges and P its world edges.
            let (e1, e2) = (
                [uv[1][0] - uv[0][0], uv[1][1] - uv[0][1]],
                [uv[2][0] - uv[0][0], uv[2][1] - uv[0][1]],
            );
            let det = e1[0] * e2[1] - e2[0] * e1[1];
            let (p1, p2) = (tri.p[1] - tri.p[0], tri.p[2] - tri.p[0]);
            let tri_axes = [(p1 * e2[1] - p2 * e1[1]) / det, (p2 * e1[0] - p1 * e2[0]) / det];
            let tri_axes = if tri_axes.iter().all(|a| a.is_finite()) {
                tri_axes
            } else {
                [Vec3::ZERO; 2]
            };
            let lo = |c: usize| {
                uv.iter()
                    .map(|q| q[c])
                    .fold(f32::INFINITY, f32::min)
                    .floor()
                    .max(0.0) as usize
            };
            let hi = |c: usize, n: usize| {
                (uv.iter()
                    .map(|q| q[c])
                    .fold(f32::NEG_INFINITY, f32::max)
                    .ceil()
                    .max(0.0) as usize)
                    .min(n)
            };
            for y in lo(1)..hi(1, rh) {
                for x in lo(0)..hi(0, rw) {
                    let c = [x as f32 + 0.5, y as f32 + 0.5];
                    let b0 = edge(uv[1], uv[2], c) / area;
                    let b1 = edge(uv[2], uv[0], c) / area;
                    let b2 = 1.0 - b0 - b1;
                    let tol = -1e-5;
                    if b0 < tol || b1 < tol || b2 < tol {
                        continue;
                    }
                    let [b0, b1, b2] = inset_from_border([b0, b1, b2], &tri.p, borders[k]);
                    let texel = (ry + y) * w + rx + x;
                    if claimed[texel] {
                        continue;
                    }
                    claimed[texel] = true;
                    axes[texel] = tri_axes;
                    chart_of[texel] = chart_base.saturating_add(tri_chart[k]);
                    let p = tri.p[0] * b0 + tri.p[1] * b1 + tri.p[2] * b2;
                    let sn = (tri.n[0] * b0 + tri.n[1] * b1 + tri.n[2] * b2).normalize_or(tri.face);
                    let n = if tri.face == Vec3::ZERO {
                        sn
                    } else if sn.dot(tri.face) < 0.0 {
                        tri.face
                    } else {
                        sn
                    };
                    let face = if tri.face == Vec3::ZERO { n } else { tri.face };
                    normals[texel] = n;
                    tasks.push(Task {
                        texel,
                        key: pcg(entry_key ^ pcg((y * rw + x) as u32)),
                        p,
                        n,
                        face,
                    });
                    sources.push(crease::TaskSource {
                        entry: e,
                        instance: receivers[e],
                        tri: k as u32,
                        texel_size: size,
                    });
                }
            }
        }
        chart_base = chart_base.saturating_add(charts);
        if settings.crease_placement {
            entry_charts.push(crease::InstanceCharts::new(tri_chart));
        }
    }
    let crease = if settings.crease_placement {
        let stats = crease::relocate(
            scene,
            &mut tasks,
            &sources,
            &entry_charts,
            settings.inside_threshold,
            worker_threads(settings),
        );
        for t in &tasks {
            normals[t.texel] = t.n;
        }
        stats
    } else {
        crease::CreaseStats::default()
    };
    Ok(Prepared {
        tasks,
        normals,
        entry_of,
        chart_of,
        texel_size,
        axes,
        crease,
        receivers: receivers.to_vec(),
        w,
        h,
    })
}

/// The bake's result from one estimate per task: the filter, the gutters, the encoding and the
/// report. The CPU and the GPU baker share it.
fn finish_bake(
    scene: &LightmapScene,
    layout: LightmapLayout,
    settings: &LightmapBakeSettings,
    prepared: &Prepared,
    estimates: &[TexelEstimate],
    rays: u64,
) -> Result<(Lightmap, LightmapBakeReport), PtError> {
    let Prepared {
        tasks,
        normals,
        entry_of,
        chart_of,
        texel_size,
        axes,
        crease,
        receivers,
        w,
        h,
    } = prepared;
    let (w, h) = (*w, *h);
    let n_texels = w * h;
    let mut irradiance = vec![[0.0f32; 3]; n_texels];
    let mut directions = vec![[0.0f32; 4]; n_texels];
    let mut coverage = vec![Coverage::EMPTY; n_texels];
    let mut report = LightmapBakeReport {
        entries: layout.entries.len(),
        triangles: scene.tris.len(),
        emissive_triangles: scene.emitters.len(),
        lights: scene.light_count(),
        directional: settings.directional,
        texels_crease_inside: crease.inside,
        texels_relocated: crease.relocated,
        ..LightmapBakeReport::default()
    };
    let mut errors: Vec<f32> = Vec::new();
    let mut positions = vec![Vec3::ZERO; n_texels];
    let mut rel_err = vec![f32::NAN; n_texels];
    let (mut samples_taken, mut over_target) = (0u64, 0usize);
    for (task, est) in tasks.iter().zip(estimates) {
        if est.back as f32 > settings.inside_threshold * est.taken as f32 {
            report.texels_inside += 1;
            continue;
        }
        irradiance[task.texel] = est.e.to_array();
        let d = est.dir;
        directions[task.texel] = [d.x, d.y, d.z, task.n.dot(d).clamp(0.0, 1.0)];
        coverage[task.texel] = Coverage::BAKED;
        positions[task.texel] = task.p;
        rel_err[task.texel] = est.rel_err;
        report.texels_baked += 1;
        samples_taken += u64::from(est.taken);
        if !est.rel_err.is_nan() {
            errors.push(est.rel_err);
            if settings.noise_target > 0.0 && est.rel_err > settings.noise_target {
                over_target += 1;
            }
        }
    }
    report.rays = rays;
    report.noise = noise_summary(errors, samples_taken, report.texels_baked, over_target);
    // The denoiser's taps across a chart border read only texels in sight (no wall between).
    let clear = |a: Vec3, b: Vec3| {
        let d = b - a;
        let len = d.length();
        // A zero (or non-finite) segment has nothing on it.
        !len.is_normal() || scene.intersect(a, d / len, len, true).is_none()
    };
    let guides = denoise::DenoiseGuides {
        coverage: &coverage,
        normals,
        positions: &positions,
        chart_of,
        texel_size,
        rel_err: &rel_err,
        axes,
        clear: Some(&clear),
        threads: worker_threads(settings),
        w,
        h,
    };
    let filter_3x3 = settings.filter && settings.denoise.is_none();
    let mut denoised_var: Option<Vec<f32>> = None;
    if settings.directional {
        // Filter and dilate irradiance and direction together, with the same weights.
        let mut packed: Vec<[f32; 7]> = irradiance
            .iter()
            .zip(&directions)
            .map(|(e, d)| [e[0], e[1], e[2], d[0], d[1], d[2], d[3]])
            .collect();
        if let Some(d) = &settings.denoise {
            denoised_var = Some(denoise::denoise(&mut packed, &guides, d));
        } else if filter_3x3 {
            filter(&mut packed, &coverage, normals, entry_of, w, h);
        }
        if settings.crease_placement {
            report.texels_extrapolated =
                crease::extrapolate_first_ring(&mut packed, &mut coverage, chart_of, w, h);
        }
        report.texels_dilated = report.texels_extrapolated
            + dilate(&mut packed, &mut coverage, entry_of, w, h, layout.padding + 1);
        if settings.stitch_seams {
            let page_rows = layout.height as usize;
            let s = stitch::stitch(
                &mut packed,
                &coverage,
                &layout.stacked(),
                &entry_positions(scene, receivers),
                w,
                h,
                page_rows,
            );
            report.seam_edges_stitched = s.edges;
            report.texels_stitched = s.texels;
        }
        for ((e, d), p) in irradiance.iter_mut().zip(directions.iter_mut()).zip(&packed) {
            *e = [p[0], p[1], p[2]];
            *d = [p[3], p[4], p[5], p[6]];
        }
    } else {
        if let Some(d) = &settings.denoise {
            denoised_var = Some(denoise::denoise(&mut irradiance, &guides, d));
        } else if filter_3x3 {
            filter(&mut irradiance, &coverage, normals, entry_of, w, h);
        }
        if settings.crease_placement {
            report.texels_extrapolated =
                crease::extrapolate_first_ring(&mut irradiance, &mut coverage, chart_of, w, h);
        }
        report.texels_dilated = report.texels_extrapolated
            + dilate(&mut irradiance, &mut coverage, entry_of, w, h, layout.padding + 1);
        if settings.stitch_seams {
            let page_rows = layout.height as usize;
            let s = stitch::stitch(
                &mut irradiance,
                &coverage,
                &layout.stacked(),
                &entry_positions(scene, receivers),
                w,
                h,
                page_rows,
            );
            report.seam_edges_stitched = s.edges;
            report.texels_stitched = s.texels;
        }
    }
    report.denoised = settings.denoise.is_some();
    report.denoised_noise = denoised_var.and_then(|var| {
        // Over the texels whose noise was measured, as `LightmapNoise::mean` is.
        let rel: Vec<f64> = tasks
            .iter()
            .filter(|t| rel_err[t.texel].is_finite())
            .filter_map(|t| {
                let l = f64::from(lum(Vec3::from_array(irradiance[t.texel])));
                (l > 0.0).then(|| f64::from(var[t.texel]).max(0.0).sqrt() / l)
            })
            .collect();
        (!rel.is_empty()).then(|| (rel.iter().sum::<f64>() / rel.len() as f64) as f32)
    });
    let contents = scene.contents(settings);
    report.contents = LightmapContents::names(contents);
    let sun = scene.sun.filter(|s| s.illuminance > 0.0);
    let bake = LightmapBakeInfo {
        contents,
        samples: settings.samples,
        bounces: settings.bounces,
        seed: settings.seed,
        sun_toward: sun.map_or([0.0; 3], |s| s.toward),
        sun_illuminance: sun.map_or(0.0, |s| s.illuminance * lum(Vec3::from_array(s.color))),
        filtered: filter_3x3,
    };
    let mut map = Lightmap::from_irradiance(layout, &irradiance, coverage, bake)
        .map_err(|e| PtError::Scene(format!("lightmap: {e}")))?;
    if settings.directional {
        map.set_directions(&directions)
            .map_err(|e| PtError::Scene(format!("lightmap: {e}")))?;
    }
    map.denoise = settings.denoise;
    Ok((map, report))
}

/// The world positions of each entry's instance (empty for a missing one), for the stitch.
fn entry_positions<'a>(scene: &'a LightmapScene, receivers: &[usize]) -> Vec<&'a [[f32; 3]]> {
    receivers
        .iter()
        .map(|&k| scene.instances.get(k).map_or(&[][..], |i| i.positions.as_slice()))
        .collect()
}

/// The layout checks the raster needs before it indexes anything (`Lightmap::validate` repeats
/// them, with overlap and name checks, when the result is encoded).
fn check_layout(layout: &LightmapLayout) -> Result<(), PtError> {
    let bad = |m: String| Err(PtError::Scene(format!("lightmap layout: {m}")));
    let pages = layout.pages();
    if pages > regia_asset::lightmap::MAX_LIGHTMAP_PAGES {
        return bad(format!(
            "{pages} pages, over {}",
            regia_asset::lightmap::MAX_LIGHTMAP_PAGES
        ));
    }
    if layout.width == 0
        || layout.height == 0
        || layout.width > MAX_LIGHTMAP_SIZE
        || layout.height > MAX_LIGHTMAP_SIZE
    {
        return bad(format!(
            "atlas {}×{} is outside 1..={MAX_LIGHTMAP_SIZE}",
            layout.width, layout.height
        ));
    }
    for (entry, unwrap) in layout.entries.iter().zip(&layout.unwraps) {
        let [x, y, w, h] = entry.rect;
        if w == 0
            || h == 0
            || x.checked_add(w).is_none_or(|r| r > layout.width)
            || y.checked_add(h).is_none_or(|b| b > layout.height)
        {
            return bad(format!(
                "entry `{}`: rect {:?} outside the atlas",
                entry.name, entry.rect
            ));
        }
        if unwrap.indices.iter().any(|&i| i as usize >= unwrap.uv.len()) {
            return bad(format!(
                "entry `{}`: an index is past the {} unwrapped vertices",
                entry.name,
                unwrap.uv.len()
            ));
        }
        if unwrap.uv.iter().flatten().any(|v| !v.is_finite()) {
            return bad(format!("entry `{}`: a UV2 is not finite", entry.name));
        }
    }
    Ok(())
}

/// Twice the signed area of `(a, b, c)`.
/// For each triangle of an unwrap, whether the edge opposite each corner lies on its chart's border:
/// no other triangle shares it over the unwrapped vertices, which both unwrappers split at a border.
fn chart_border_edges(indices: &[u32]) -> Vec<[bool; 3]> {
    let tris = indices.as_chunks::<3>().0;
    let key = |t: &[u32; 3], corner: usize| {
        let (a, b) = (t[(corner + 1) % 3], t[(corner + 2) % 3]);
        (a.min(b), a.max(b))
    };
    let mut uses: std::collections::HashMap<(u32, u32), u32> = std::collections::HashMap::new();
    for t in tris {
        for corner in 0..3 {
            *uses.entry(key(t, corner)).or_insert(0) += 1;
        }
    }
    tris.iter()
        .map(|t| std::array::from_fn(|corner| uses.get(&key(t, corner)) == Some(&1)))
        .collect()
}

/// Moves barycentrics `b` of the triangle `p` off the border edges in `border` (`border[i]` is the
/// edge opposite corner `i`) by a little more than the ray origin's own offset (`offset_origin`),
/// at most to the triangle's center. A point already that far inside comes back unchanged, to the
/// bit.
fn inset_from_border(b: [f32; 3], p: &[Vec3; 3], border: [bool; 3]) -> [f32; 3] {
    /// The inset, metres, where the origin's offset is smaller.
    const LEAST_INSET: f32 = 1e-3;
    if border == [false; 3] {
        return b;
    }
    let at = p[0] * b[0] + p[1] * b[1] + p[2] * b[2];
    let inset = (512.0 * f32::EPSILON * at.abs().max_element().max(1.0)).max(LEAST_INSET);
    let doubled_area = (p[1] - p[0]).cross(p[2] - p[0]).length();
    let mut raised = [false; 3];
    let mut out = b;
    for i in 0..3 {
        let length = (p[(i + 1) % 3] - p[(i + 2) % 3]).length();
        if !border[i] || length <= 0.0 || doubled_area <= 0.0 {
            continue;
        }
        let least = (inset * length / doubled_area).min(1.0 / 3.0);
        if b[i] < least {
            out[i] = least;
            raised[i] = true;
        }
    }
    if raised == [false; 3] {
        return b;
    }
    let held: f32 = (0..3).filter(|&i| raised[i]).map(|i| out[i]).sum();
    let rest: f32 = (0..3).filter(|&i| !raised[i]).map(|i| b[i].max(0.0)).sum();
    let free = (0..3).filter(|&i| !raised[i]).count();
    if free == 0 {
        return out.map(|v| v / held);
    }
    for i in (0..3).filter(|&i| !raised[i]) {
        out[i] = if rest > 0.0 {
            b[i].max(0.0) * (1.0 - held) / rest
        } else {
            (1.0 - held) / free as f32
        };
    }
    out
}

/// The next origin of a path that hit the triangle `p` at barycentrics `(u, v)`: the hit point
/// moved off the triangle's edges as a texel is moved off its chart's border
/// (`inset_from_border` with every edge a border), then offset along `face` (`offset_origin`).
///
/// On an edge the interpolated point can lie on, or a rounding step behind, the plane of the
/// triangle across it. Where that is a room's corner, the next ray started outside the other
/// wall and, the wall being one-sided, left the room (ADR-3287). A hit farther inside than the
/// inset keeps its point to the bit. `lightmap_bake.wgsl` mirrors it (`bounce_origin`).
fn bounce_origin(p: &[Vec3; 3], u: f32, v: f32, face: Vec3) -> Vec3 {
    let b = inset_from_border([1.0 - u - v, u, v], p, [true; 3]);
    offset_origin(p[0] * b[0] + p[1] * b[1] + p[2] * b[2], face)
}

fn edge(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])
}

/// The noise summary of the baked texels' relative errors; `None` when none was measurable.
fn noise_summary(
    mut errors: Vec<f32>,
    samples_taken: u64,
    texels_baked: usize,
    texels_over_target: usize,
) -> Option<LightmapNoise> {
    if errors.is_empty() {
        return None;
    }
    errors.sort_by(f32::total_cmp);
    let n = errors.len();
    let p95 = errors[((n as f64 * 0.95).ceil() as usize).clamp(1, n) - 1];
    Some(LightmapNoise {
        mean: (errors.iter().map(|&e| f64::from(e)).sum::<f64>() / n as f64) as f32,
        p95,
        max: errors[n - 1],
        mean_samples: (samples_taken as f64 / texels_baked.max(1) as f64) as f32,
        texels_over_target,
    })
}

/// The 3×3 edge-aware filter over baked texels of the same entry (every channel of a texel
/// with the same weights).
fn filter<const N: usize>(
    irr: &mut [[f32; N]],
    cov: &[u8],
    normals: &[Vec3],
    entry_of: &[u32],
    w: usize,
    h: usize,
) {
    let src = irr.to_vec();
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if cov[i] != Coverage::BAKED {
                continue;
            }
            let mut sum = [0.0f32; N];
            let mut wsum = 0.0f32;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if cov[j] != Coverage::BAKED || entry_of[j] != entry_of[i] {
                        continue;
                    }
                    let spatial = [1.0, 2.0, 1.0][(dx + 1) as usize] * [1.0, 2.0, 1.0][(dy + 1) as usize];
                    let wgt = spatial * normals[i].dot(normals[j]).max(0.0).powi(FILTER_NORMAL_POWER);
                    for (s, v) in sum.iter_mut().zip(src[j]) {
                        *s += wgt * v;
                    }
                    wsum += wgt;
                }
            }
            if wsum > 0.0 {
                irr[i] = sum.map(|s| s / wsum);
            }
        }
    }
}

/// Fills empty texels next to filled ones (same entry) with their neighbours' mean, `rounds`
/// times. Returns the number of texels filled.
fn dilate<const N: usize>(
    irr: &mut [[f32; N]],
    cov: &mut [u8],
    entry_of: &[u32],
    w: usize,
    h: usize,
    rounds: u32,
) -> usize {
    let mut filled = 0;
    for _ in 0..rounds {
        let mut next: Vec<(usize, [f32; N])> = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if cov[i] != Coverage::EMPTY || entry_of[i] == u32::MAX {
                    continue;
                }
                let mut sum = [0.0f32; N];
                let mut n = 0u32;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                        if (dx, dy) == (0, 0) || nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                            continue;
                        }
                        let j = ny as usize * w + nx as usize;
                        if cov[j] == Coverage::EMPTY || entry_of[j] != entry_of[i] {
                            continue;
                        }
                        for (s, v) in sum.iter_mut().zip(irr[j]) {
                            *s += v;
                        }
                        n += 1;
                    }
                }
                if n > 0 {
                    next.push((i, sum.map(|s| s / n as f32)));
                }
            }
        }
        if next.is_empty() {
            break;
        }
        for (i, v) in next {
            irr[i] = v;
            cov[i] = Coverage::DILATED;
            filled += 1;
        }
    }
    filled
}

/// The solid-angle weighted mean RGB of an equirectangular map (row 0 at the zenith): each row
/// weighs `cos θ0 − cos θ1`, so the polar rows count less than the equator rows. Falls back to
/// the plain mean when the size does not match `texels`.
fn equirect_mean(width: u32, height: u32, texels: &[[f32; 4]]) -> Vec3 {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || texels.len() != w * h {
        let n = texels.len().max(1) as f32;
        let sum = texels
            .iter()
            .fold(Vec3::ZERO, |a, t| a + Vec3::new(t[0], t[1], t[2]));
        return sum / n;
    }
    let mut total = [0.0f64; 3];
    let mut weight = 0.0f64;
    for (j, row) in texels.chunks_exact(w).enumerate() {
        let t0 = std::f64::consts::PI * j as f64 / h as f64;
        let t1 = std::f64::consts::PI * (j + 1) as f64 / h as f64;
        let wr = (t0.cos() - t1.cos()) * w as f64;
        for t in row {
            for c in 0..3 {
                total[c] += f64::from(t[c]) * (wr / w as f64);
            }
        }
        weight += wr / w as f64 * w as f64;
    }
    if weight <= 0.0 {
        return Vec3::ZERO;
    }
    Vec3::new(
        (total[0] / weight) as f32,
        (total[1] / weight) as f32,
        (total[2] / weight) as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PtCamera, PtMaterial, PtMesh, PtSun};
    use regia_asset::lightmap::{LightmapEntry, MeshUnwrap};
    use regia_core::glam::Mat4;

    /// A `size × size` floor at y = 0 facing +Y, one quad, unwrapped onto a `res × res` rect
    /// with a 1-texel gutter.
    fn floor(size: f32) -> PtMesh {
        let h = size * 0.5;
        PtMesh {
            positions: vec![[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2],
            ..PtMesh::default()
        }
    }

    fn floor_layout(res: u32) -> LightmapLayout {
        let g = 1.0 / res as f32;
        let (lo, hi) = (g, 1.0 - g);
        LightmapLayout {
            width: res,
            height: res,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "floor".into(),
                source_vertices: 4,
                rect: [0, 0, res, res],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 1.0,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[lo, lo], [hi, lo], [hi, hi], [lo, hi]],
                indices: vec![0, 2, 1, 0, 3, 2],
            }],
        }
    }

    fn scene_with(sky: PtSky) -> PtScene {
        let mut s = PtScene::new(PtCamera::pinhole([0.0, 5.0, 5.0], [0.0; 3], 1.0, 4, 4));
        let m = s.add_mesh(floor(4.0));
        let mat = s.add_material(PtMaterial::lambert([0.5; 3]));
        s.add_instance(m, mat, Mat4::IDENTITY);
        s.sky = sky;
        s
    }

    fn settings(samples: u32) -> LightmapBakeSettings {
        LightmapBakeSettings {
            samples,
            bounces: 3,
            seed: 3,
            filter: false,
            threads: 2,
            ..LightmapBakeSettings::default()
        }
    }

    /// An open floor under a uniform sky of radiance L receives E = π·L everywhere (the
    /// furnace-style analytic check): cosine sampling makes every sample exact.
    #[test]
    fn an_open_floor_under_a_uniform_sky_gets_pi_l() {
        let scene =
            LightmapScene::from_pt_scene(&scene_with(PtSky::Uniform([1.0, 2.0, 0.5]))).expect("scene");
        let (map, report) =
            bake_lightmap(&scene, floor_layout(8), &[0], &settings(16), |_, _| {}).expect("bake");
        assert_eq!(report.texels_baked, 36, "the 6×6 interior");
        assert_eq!(report.texels_inside, 0);
        assert!(report.texels_dilated > 0);
        assert_eq!(report.contents, ["sky"]);
        let pi = std::f32::consts::PI;
        for y in 0..8 {
            for x in 0..8 {
                let e = map.irradiance_at(x, y);
                let want = [pi, 2.0 * pi, 0.5 * pi];
                for (g, w) in e.iter().zip(want) {
                    assert!((g - w).abs() < w * 0.01, "texel ({x}, {y}): {e:?}");
                }
            }
        }
        assert_eq!(map.coverage_counts()[0], 0, "the gutter is fully dilated");
    }

    /// A sun at 60° from the zenith with `direct_sun` gives `E = illuminance · cos 60°` on the
    /// open floor (no sky, one vertex); without `direct_sun` the floor gets nothing (the sun
    /// stays live and there is nothing to bounce off).
    #[test]
    fn the_direct_sun_is_baked_only_on_request() {
        let mut pt = scene_with(PtSky::None);
        let toward = Vec3::new(60f32.to_radians().sin(), 60f32.to_radians().cos(), 0.0);
        pt.sun = Some(PtSun {
            toward: toward.to_array(),
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 0.0,
        });
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let s = LightmapBakeSettings {
            direct_sun: true,
            bounces: 1,
            ..settings(4)
        };
        let (map, report) = bake_lightmap(&scene, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");
        assert_eq!(report.contents, ["sun_direct"]);
        let e = map.irradiance_at(4, 4);
        assert!((e[0] - 500.0).abs() < 5.0, "{e:?}");
        let (live, report) =
            bake_lightmap(&scene, floor_layout(8), &[0], &settings(4), |_, _| {}).expect("bake");
        assert_eq!(report.contents, ["sun_bounce"]);
        assert_eq!(live.irradiance_at(4, 4), [0.0; 3]);
        assert!((map.bake.sun_illuminance - 1000.0).abs() < 0.1);
    }

    /// A floor lit only by one of the frame's extra directional lights (ADR-7254) bakes its
    /// direct light like the sun's: `E = illuminance · cos 60°` (the forward's Lambert input,
    /// within the bake tolerance) and the same irradiance as with that light as the sun.
    #[test]
    fn an_extra_directional_light_bakes_like_the_sun() {
        let toward = Vec3::new(60f32.to_radians().sin(), 60f32.to_radians().cos(), 0.0);
        let light = PtSun {
            toward: toward.to_array(),
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 0.0,
        };
        let s = LightmapBakeSettings {
            direct_sun: true,
            bounces: 1,
            ..settings(4)
        };
        let mut extra = scene_with(PtSky::None);
        extra.extra_suns = vec![light];
        let scene = LightmapScene::from_pt_scene(&extra).expect("scene");
        let (map, report) = bake_lightmap(&scene, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");
        assert_eq!(report.contents, ["sun_direct"]);
        let e = map.irradiance_at(4, 4);
        assert!(e[0] > 0.0, "the extra light reaches the texel");
        assert!((e[0] - 500.0).abs() < 5.0, "{e:?}");
        let mut as_sun = scene_with(PtSky::None);
        as_sun.sun = Some(light);
        let sun_scene = LightmapScene::from_pt_scene(&as_sun).expect("scene");
        let (sun_map, _) = bake_lightmap(&sun_scene, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(
                    map.irradiance_at(x, y),
                    sun_map.irradiance_at(x, y),
                    "texel ({x}, {y})"
                );
            }
        }
    }

    /// A wall standing on the floor shadows the sky: texels next to its foot get less light
    /// than texels far away, and the wall's bounce adds its albedo's share.
    #[test]
    fn a_wall_occludes_the_sky_near_its_foot() {
        let mut pt = scene_with(PtSky::Uniform([1.0; 3]));
        let wall = pt.add_mesh(PtMesh {
            positions: vec![
                [-2.0, 0.0, 0.0],
                [2.0, 0.0, 0.0],
                [2.0, 4.0, 0.0],
                [-2.0, 4.0, 0.0],
            ],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..PtMesh::default()
        });
        let black = pt.add_material(PtMaterial::lambert([0.0; 3]));
        pt.add_instance(wall, black, Mat4::IDENTITY);
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        assert_eq!(scene.instances().len(), 2);
        let (map, _) =
            bake_lightmap(&scene, floor_layout(16), &[0], &settings(256), |_, _| {}).expect("bake");
        // The floor's v axis runs along +z: texel row 8 is next to the wall (z ≈ 0+), row 14 is
        // 1.5 m away.
        let near = map.irradiance_at(8, 8)[0];
        let far = map.irradiance_at(8, 14)[0];
        assert!(near < 0.75 * far, "near {near} far {far}");
        assert!(far < std::f32::consts::PI * 1.01);
    }

    /// The result does not depend on the thread count, and a seed change changes it.
    #[test]
    fn the_bake_is_deterministic_across_thread_counts() {
        let mut pt = scene_with(PtSky::Uniform([1.0; 3]));
        pt.lights.push(PtLight::Point {
            position: [0.5, 1.0, 0.3],
            intensity: [10.0; 3],
        });
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let one = LightmapBakeSettings {
            threads: 1,
            ..settings(8)
        };
        let five = LightmapBakeSettings {
            threads: 5,
            ..settings(8)
        };
        let (a, ra) = bake_lightmap(&scene, floor_layout(8), &[0], &one, |_, _| {}).expect("a");
        let (b, rb) = bake_lightmap(&scene, floor_layout(8), &[0], &five, |_, _| {}).expect("b");
        assert_eq!(a, b);
        assert_eq!(ra, rb);
        assert_eq!(ra.contents, ["sky", "local_lights"]);
    }

    /// A point light straight above: `E = I / d²` at the texel under it.
    #[test]
    fn a_point_light_follows_the_inverse_square_law() {
        let mut pt = scene_with(PtSky::None);
        pt.lights.push(PtLight::Point {
            position: [0.25, 2.0, 0.25],
            intensity: [100.0; 3],
        });
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let s = LightmapBakeSettings {
            bounces: 1,
            ..settings(1)
        };
        let (map, _) = bake_lightmap(&scene, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");
        // Texel (4, 4) centre: uv 4.5/8 → x = -2 + 4·(4.5/8 − 1/8)/(6/8)... the floor spans the
        // interior 1..7: x = -2 + 4·(4.5 − 1)/6 = 0.333.
        let x = -2.0 + 4.0 * (4.5 - 1.0) / 6.0;
        let d2 = (x - 0.25f32).powi(2) * 2.0 + 4.0;
        let cos = 2.0 / d2.sqrt();
        let want = 100.0 * cos / d2;
        let got = map.irradiance_at(4, 4)[0];
        assert!((got - want).abs() < want * 0.01, "got {got} want {want}");
    }

    /// An enclosed box (normals inward) lit by an emissive ceiling: every texel of the floor is
    /// lit, and a texel whose rays hit back faces (a receiver outside a closed box looking at
    /// its outside) is not marked inside.
    #[test]
    fn an_emissive_triangle_lights_the_floor_by_next_event_estimation() {
        let mut pt = scene_with(PtSky::None);
        let lamp = pt.add_mesh(PtMesh {
            positions: vec![
                [-0.5, 2.0, -0.5],
                [0.5, 2.0, -0.5],
                [0.5, 2.0, 0.5],
                [-0.5, 2.0, 0.5],
            ],
            normals: vec![[0.0, -1.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..PtMesh::default()
        });
        let glow = pt.add_material(PtMaterial::emitter([50.0; 3]));
        pt.add_instance(lamp, glow, Mat4::IDENTITY);
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        assert!(!scene.instances()[1].receiver);
        let s = LightmapBakeSettings {
            bounces: 1,
            ..settings(256)
        };
        let (map, report) = bake_lightmap(&scene, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");
        assert_eq!(report.emissive_triangles, 2);
        assert_eq!(report.contents, ["emissive"]);
        // Under the 1 m² lamp 2 m up: E ≈ L · A · cos² / d² = 50 · 1 / 4 = 12.5 (small-source
        // approximation, a few % high for a 1 m lamp).
        let e = map.irradiance_at(4, 4)[0];
        assert!(e > 10.0 && e < 13.5, "{e}");
        let edge = map.irradiance_at(1, 1)[0];
        assert!(edge < e, "the corner is farther from the lamp: {edge} vs {e}");
    }

    /// Over the unwrapped vertices: a quad's outline is its chart's border, its diagonal is not.
    #[test]
    fn a_chart_border_is_an_edge_no_other_triangle_shares() {
        let borders = chart_border_edges(&[0, 1, 2, 0, 2, 3]);
        // Triangle 0's edge opposite corner 1 is the diagonal 0-2; so is triangle 1's opposite 2.
        assert_eq!(borders, [[true, false, true], [true, true, false]]);
        // Split vertices make the same mesh edge a border on both sides.
        assert_eq!(chart_border_edges(&[0, 1, 2, 3, 4, 5]), [[true; 3], [true; 3]]);
    }

    /// A point on a border moves just inside it; a point well inside, and one on an edge that is no
    /// border, stay to the bit; and the point stays on the triangle.
    #[test]
    fn a_point_on_a_chart_border_moves_just_inside_it() {
        let p = [Vec3::ZERO, Vec3::new(4.0, 0.0, 0.0), Vec3::new(0.0, 4.0, 0.0)];
        let position = |b: [f32; 3]| p[0] * b[0] + p[1] * b[1] + p[2] * b[2];
        // On the edge y = 0, opposite corner 2.
        let on = [0.5, 0.5, 0.0];
        let moved = inset_from_border(on, &p, [false, false, true]);
        let q = position(moved);
        assert!((1e-3 - 1e-6..1e-2).contains(&q.y), "{q}");
        assert!((q.x - 2.0).abs() < 1e-2, "{q}");
        assert!((moved.iter().sum::<f32>() - 1.0).abs() < 1e-6, "{moved:?}");

        assert_eq!(inset_from_border(on, &p, [false; 3]), on, "no border");
        assert_eq!(
            inset_from_border(on, &p, [true, true, false]),
            on,
            "it is on the other edge, which is no border, and far from these"
        );
        let inside = [0.25, 0.25, 0.5];
        assert_eq!(inset_from_border(inside, &p, [true; 3]), inside);

        // In a corner of two borders, away from both.
        let q = position(inset_from_border([1.0, 0.0, 0.0], &p, [false, true, true]));
        assert!(q.x >= 1e-3 - 1e-6 && q.y >= 1e-3 - 1e-6, "{q}");

        // A triangle smaller than the inset ends at its center, not outside it.
        let tiny = p.map(|v| v * 1e-5);
        let b = inset_from_border([1.0, 0.0, 0.0], &tiny, [true; 3]);
        for v in b {
            assert!((v - 1.0 / 3.0).abs() < 1e-5, "{b:?}");
        }

        // Far from the origin the origin's offset is larger, and so is the inset.
        let far = p.map(|v| v + Vec3::new(1000.0, 0.0, 0.0));
        let b = inset_from_border(on, &far, [false, false, true]);
        let q = far[0] * b[0] + far[1] * b[1] + far[2] * b[2];
        assert!(q.y > 0.03, "{q}");
    }

    /// A bounce off a wall at the edge it shares with another wall starts inside the room, past
    /// the rounding of its barycentric interpolation: on the edge itself the next origin lay on (or
    /// one rounding step behind) the neighbour's plane, so a one-sided wall no longer stopped the
    /// next ray (one texel lit at 49 lux in the CLI's sealed-room test on aarch64, ADR-3287).
    #[test]
    fn a_bounce_at_a_room_corner_starts_inside_the_room() {
        // The x = -2 wall of a 4 m room facing +x, corners on the y and z edges.
        let p = [
            Vec3::new(-2.0, -2.0, -2.0),
            Vec3::new(-2.0, 2.0, -2.0),
            Vec3::new(-2.0, 2.0, 2.0),
        ];
        let face = Vec3::X;
        // Every corner, every edge's midpoint and points near the edges.
        let hits: [(f32, f32); 9] = [
            (0.0, 0.0),
            (1.0, 0.0),
            (0.0, 1.0),
            (0.5, 0.0),
            (0.5, 0.5),
            (0.0, 0.5),
            (0.3, 0.7),
            (1e-7, 0.999_999_9),
            (0.999_999_9, 0.0),
        ];
        for (u, v) in hits {
            let o = bounce_origin(&p, u, v, face);
            assert!(o.x > -2.0, "({u}, {v}): {o} is on the wall's front");
            for (k, c) in [o.y, o.z].into_iter().enumerate() {
                assert!(
                    c > -2.0 + 5e-4 && c < 2.0 - 5e-4,
                    "({u}, {v}): {o} axis {k} not inside the neighbouring walls"
                );
            }
        }
        // A hit well inside keeps its point (only the face offset moves it).
        let mid = bounce_origin(&p, 0.25, 0.25, face);
        let want = offset_origin(p[0] * 0.5 + p[1] * 0.25 + p[2] * 0.25, face);
        assert_eq!(mid, want);
    }

    /// A ray through the diagonal two triangles of a quad share hits one of them, from any
    /// direction (ADR-3287: rounding let such a ray pass between both).
    #[test]
    fn a_ray_through_a_shared_edge_hits_one_of_its_triangles() {
        let quad = [
            Vec3::new(-2.0, -2.0, -2.0),
            Vec3::new(-2.0, 2.0, -2.0),
            Vec3::new(-2.0, 2.0, 2.0),
            Vec3::new(-2.0, -2.0, 2.0),
        ];
        let a = [quad[0], quad[1], quad[2]];
        let b = [quad[0], quad[2], quad[3]];
        let mut missed = Vec::new();
        for i in 1..200 {
            let s = i as f32 / 200.0;
            // A point on the diagonal quad[0]..quad[2].
            let at = quad[0] + (quad[2] - quad[0]) * s;
            for j in 0..16 {
                let angle = j as f32 * 0.39;
                let d = Vec3::new(-1.0, 0.7 * angle.cos(), 0.7 * angle.sin()).normalize();
                let o = at - d * 3.3;
                if ray_triangle(o, d, &a).is_none() && ray_triangle(o, d, &b).is_none() {
                    missed.push((s, j));
                }
            }
        }
        assert!(
            missed.is_empty(),
            "rays through the shared edge missed both: {missed:?}"
        );
    }

    /// A floor texel under a closed box (the box's bottom face lies on the floor) sees only the
    /// box's back faces: it is dropped as inside geometry and refilled by dilation.
    #[test]
    fn texels_inside_geometry_are_dropped_and_refilled() {
        let mut pt = scene_with(PtSky::Uniform([1.0; 3]));
        // A thin closed slab hovering 1 cm above the floor's centre, facing outward.
        let (a, b) = (0.6f32, 0.01f32);
        let p = |x: f32, y: f32, z: f32| [x, y, z];
        let positions = vec![
            p(-a, b, -a),
            p(a, b, -a),
            p(a, b, a),
            p(-a, b, a),
            p(-a, 2.0 * b, -a),
            p(a, 2.0 * b, -a),
            p(a, 2.0 * b, a),
            p(-a, 2.0 * b, a),
        ];
        // Bottom face points down (toward the floor), top up, sides out.
        let indices = vec![
            0, 1, 2, 0, 2, 3, // bottom (-y)
            4, 6, 5, 4, 7, 6, // top (+y)
            0, 4, 5, 0, 5, 1, // -z
            2, 6, 7, 2, 7, 3, // +z
            1, 5, 6, 1, 6, 2, // +x
            3, 7, 4, 3, 4, 0, // -x
        ];
        let slab = pt.add_mesh(PtMesh {
            positions,
            normals: Vec::new(),
            indices,
            ..PtMesh::default()
        });
        let grey = pt.add_material(PtMaterial::lambert([0.5; 3]));
        pt.add_instance(slab, grey, Mat4::IDENTITY);
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let (open, _) = bake_lightmap(&scene, floor_layout(8), &[0], &settings(64), |_, _| {}).expect("bake");
        // The bottom face points down at the floor: rays from under it hit its front face, so the
        // texels there are dark but baked (not inside).
        assert!(open.irradiance_at(4, 4)[0] < 0.5 * open.irradiance_at(1, 1)[0]);

        // Flip the slab inside out: now the floor under it sees back faces.
        let mut flipped = pt.clone();
        let m = &mut flipped.meshes[1];
        for t in m.indices.as_chunks_mut::<3>().0 {
            t.swap(1, 2);
        }
        let scene = LightmapScene::from_pt_scene(&flipped).expect("scene");
        let (map, report) =
            bake_lightmap(&scene, floor_layout(8), &[0], &settings(64), |_, _| {}).expect("bake");
        assert!(report.texels_inside > 0, "{report:?}");
        assert_eq!(map.coverage_at(4, 4), Coverage::DILATED);
    }

    #[test]
    fn mismatched_receivers_are_errors() {
        let scene = LightmapScene::from_pt_scene(&scene_with(PtSky::Uniform([1.0; 3]))).expect("scene");
        assert!(bake_lightmap(&scene, floor_layout(8), &[], &settings(1), |_, _| {}).is_err());
        assert!(bake_lightmap(&scene, floor_layout(8), &[3], &settings(1), |_, _| {}).is_err());
        let mut bad = floor_layout(8);
        bad.entries[0].source_vertices = 5;
        assert!(bake_lightmap(&scene, bad, &[0], &settings(1), |_, _| {}).is_err());
        let zero = LightmapBakeSettings {
            samples: 0,
            ..settings(1)
        };
        assert!(bake_lightmap(&scene, floor_layout(8), &[0], &zero, |_, _| {}).is_err());
    }

    #[test]
    fn progress_reaches_the_texel_count() {
        let scene = LightmapScene::from_pt_scene(&scene_with(PtSky::Uniform([1.0; 3]))).expect("scene");
        let last = AtomicUsize::new(0);
        let (_, report) = bake_lightmap(&scene, floor_layout(32), &[0], &settings(1), |done, total| {
            assert!(done <= total);
            last.fetch_max(done, Ordering::Relaxed);
        })
        .expect("bake");
        assert_eq!(
            last.load(Ordering::Relaxed),
            report.texels_baked + report.texels_inside
        );
    }

    #[test]
    fn the_node_stack_is_lifo_across_the_spill() {
        let mut s = NodeStack::default();
        for n in 0..200 {
            s.push(n);
        }
        for n in (0..200).rev() {
            assert_eq!(s.pop(), Some(n));
        }
        assert_eq!(s.pop(), None);
        // A scene with nothing in it traces to the sky.
        let empty = LightmapScene::from_pt_scene(&PtScene::new(PtCamera::pinhole(
            [0.0; 3],
            [0.0, 0.0, -1.0],
            1.0,
            4,
            4,
        )))
        .expect("empty scene");
        assert!(
            empty
                .intersect(Vec3::ZERO, Vec3::Y, f32::INFINITY, false)
                .is_none()
        );
    }

    #[test]
    fn helpers_behave() {
        for n in [Vec3::X, Vec3::Y, -Vec3::Z, Vec3::new(0.3, -0.8, 0.52).normalize()] {
            let (t, b) = basis(n);
            assert!(t.dot(n).abs() < 1e-5 && b.dot(n).abs() < 1e-5 && t.dot(b).abs() < 1e-5);
            let mut rng = Rng::new(1, 2, 3);
            for _ in 0..64 {
                let d = cosine_sample(n, rng.uniform(), rng.uniform());
                assert!(d.dot(n) >= -1e-5 && (d.length() - 1.0).abs() < 1e-4);
                let c = sample_cone(n, 0.99, rng.uniform(), rng.uniform());
                assert!(c.dot(n) >= 0.99 - 1e-4);
            }
        }
        let tri = [Vec3::ZERO, Vec3::X, Vec3::Y];
        let hit = ray_triangle(Vec3::new(0.25, 0.25, 1.0), -Vec3::Z, &tri).expect("hit");
        assert!((hit.0 - 1.0).abs() < 1e-6);
        assert!(ray_triangle(Vec3::new(0.75, 0.75, 1.0), -Vec3::Z, &tri).is_none());
        let mut r = Rng::new(0, 0, 0);
        assert!((0..1000).map(|_| r.uniform()).all(|v| (0.0..1.0).contains(&v)));
    }

    /// World x (or z) of texel `i`'s centre in `floor_layout(8)` over `floor(4.0)`.
    fn centre(i: u32) -> f32 {
        -2.0 + 4.0 * (i as f32 - 0.5) / 6.0
    }

    fn direct_only() -> LightmapBakeSettings {
        LightmapBakeSettings {
            bounces: 1,
            ..settings(1)
        }
    }

    /// A spot straight down: a penumbra texel gets `I cos / d²` times the runtime's linear ramp
    /// (`spot_attenuation`), not its square; inside the inner cone the ramp is 1, past the outer
    /// cone 0.
    #[test]
    fn a_spot_light_uses_the_runtime_linear_cone_ramp() {
        let mut pt = scene_with(PtSky::None);
        let (inner, outer) = (20f32.to_radians().cos(), 35f32.to_radians().cos());
        pt.lights.push(PtLight::Spot {
            position: [0.0, 2.0, 0.0],
            direction: [0.0, -1.0, 0.0],
            intensity: [100.0; 3],
            inner_cos: inner,
            outer_cos: outer,
        });
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let (map, report) =
            bake_lightmap(&scene, floor_layout(8), &[0], &direct_only(), |_, _| {}).expect("bake");
        assert_eq!(report.contents, ["local_lights"]);
        let expect = |x: u32, z: u32| {
            let (px, pz) = (centre(x), centre(z));
            let d2 = px * px + pz * pz + 4.0;
            let cos = 2.0 / d2.sqrt();
            let ramp = ((cos - outer) / (inner - outer)).clamp(0.0, 1.0);
            (100.0 * cos / d2 * ramp, ramp)
        };
        let (want, ramp) = expect(5, 4);
        assert!(
            ramp > 0.2 && ramp < 0.8,
            "texel (5, 4) sits in the penumbra: {ramp}"
        );
        let got = map.irradiance_at(5, 4)[0];
        assert!(
            (got - want).abs() < want * 0.01,
            "penumbra: got {got} want {want}"
        );
        let (want, ramp) = expect(3, 3);
        assert_eq!(ramp, 1.0);
        let got = map.irradiance_at(3, 3)[0];
        assert!(
            (got - want).abs() < want * 0.01,
            "inner cone: got {got} want {want}"
        );
        assert_eq!(expect(6, 6).1, 0.0);
        assert_eq!(map.irradiance_at(6, 6), [0.0; 3]);
    }

    /// A sphere light sunk into the floor: texels inside it get nothing (the path tracer's
    /// `sample_light`), and no texel outside gets more than the `π L` of a sphere filling the
    /// hemisphere.
    #[test]
    fn a_sphere_light_lights_nothing_inside_itself_and_stays_under_pi_l() {
        let mut pt = scene_with(PtSky::None);
        let radiance = 2.0;
        pt.lights.push(PtLight::Sphere {
            position: [0.0, 0.2, 0.0],
            radius: 0.6,
            radiance: [radiance; 3],
        });
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let (map, _) = bake_lightmap(&scene, floor_layout(8), &[0], &direct_only(), |_, _| {}).expect("bake");
        // (±1/3, ±1/3) is 0.51 m from the centre, inside the 0.6 m sphere.
        for (x, z) in [(3, 3), (3, 4), (4, 3), (4, 4)] {
            assert_eq!(
                map.irradiance_at(x, z),
                [0.0; 3],
                "texel ({x}, {z}) is inside the light"
            );
        }
        assert!(
            map.irradiance_at(5, 3)[0] > 0.0,
            "outside the sphere the floor is lit"
        );
        let bound = std::f32::consts::PI * radiance * 1.01;
        for y in 0..8 {
            for x in 0..8 {
                let e = map.irradiance_at(x, y)[0];
                assert!(e.is_finite() && e <= bound, "texel ({x}, {y}): {e} > π L");
            }
        }
    }

    /// The black underside of a 2 m panel 0.5 m above the floor, facing down.
    fn panel() -> PtMesh {
        PtMesh {
            positions: vec![
                [-1.0, 0.5, -1.0],
                [1.0, 0.5, -1.0],
                [1.0, 0.5, 1.0],
                [-1.0, 0.5, 1.0],
            ],
            normals: vec![[0.0, -1.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..PtMesh::default()
        }
    }

    /// A quad light between the floor and the sky blocks the sky like any surface and lights
    /// the floor from its front face: the floor under it is darker than the open sky gives, and
    /// brighter than under a black panel of the same shape.
    #[test]
    fn a_quad_light_blocks_the_sky_and_lights_from_its_front_face() {
        let base = scene_with(PtSky::Uniform([1.0; 3]));
        let mut lit = base.clone();
        lit.lights.push(PtLight::Quad {
            corner: [-1.0, 0.5, -1.0],
            edge_u: [2.0, 0.0, 0.0],
            edge_v: [0.0, 0.0, 2.0],
            radiance: [0.25; 3],
            two_sided: false,
        });
        let scene = LightmapScene::from_pt_scene(&lit).expect("scene");
        assert_eq!(
            scene.instances().len(),
            2,
            "the quad became an instance after the floor"
        );
        assert!(!scene.instances()[1].receiver);
        assert_eq!(scene.light_count(), 1);
        let s = settings(256);
        let (map, report) = bake_lightmap(&scene, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");
        assert_eq!(report.contents, ["sky", "local_lights"]);
        assert_eq!((report.lights, report.emissive_triangles), (1, 2));

        let mut shaded = base.clone();
        let m = shaded.add_mesh(panel());
        let black = shaded.add_material(PtMaterial::lambert([0.0; 3]));
        shaded.add_instance(m, black, Mat4::IDENTITY);
        let shaded = LightmapScene::from_pt_scene(&shaded).expect("scene");
        let (dark, _) = bake_lightmap(&shaded, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");

        let pi = std::f32::consts::PI;
        let under = map.irradiance_at(4, 4)[0];
        let under_black = dark.irradiance_at(4, 4)[0];
        assert!(under < 0.8 * pi, "the quad blocks the sky: {under}");
        assert!(
            under > under_black + 0.1,
            "the quad's front face lights the floor: {under} vs {under_black} under black"
        );
    }

    /// The ray offset follows the position, not the scene's size: with a speck 4 km away a wall
    /// 2 cm in front of a point light still shadows the floor behind it.
    #[test]
    fn thin_walls_near_a_light_still_shadow_in_a_large_scene() {
        let mut pt = scene_with(PtSky::None);
        let wall = pt.add_mesh(PtMesh {
            positions: vec![
                [-2.0, 0.0, 0.0],
                [2.0, 0.0, 0.0],
                [2.0, 2.0, 0.0],
                [-2.0, 2.0, 0.0],
            ],
            normals: vec![[0.0, 0.0, -1.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2],
            ..PtMesh::default()
        });
        let speck = pt.add_mesh(PtMesh {
            positions: vec![[4000.0, 0.0, 0.0], [4000.1, 0.0, 0.0], [4000.0, 0.1, 0.0]],
            indices: vec![0, 1, 2],
            ..PtMesh::default()
        });
        let black = pt.add_material(PtMaterial::lambert([0.0; 3]));
        pt.add_instance(wall, black, Mat4::IDENTITY);
        pt.add_instance(speck, black, Mat4::IDENTITY);
        pt.lights.push(PtLight::Point {
            position: [0.0, 1.0, 0.02],
            intensity: [10.0; 3],
        });
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        // The wall's back face is toward the light's side: keep every texel (no inside drop), so
        // both texels below are traced, not refilled.
        let s = LightmapBakeSettings {
            inside_threshold: 2.0,
            ..direct_only()
        };
        let (map, _) = bake_lightmap(&scene, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");
        assert_eq!(map.coverage_at(3, 1), Coverage::BAKED);
        assert_eq!(map.coverage_at(3, 5), Coverage::BAKED);
        // Texel (3, 1) is at z = -1.67: the wall stands between it and the light.
        assert_eq!(map.irradiance_at(3, 1), [0.0; 3], "light leaked past the wall");
        // Texel (3, 5) is at z = +1, on the light's side.
        assert!(map.irradiance_at(3, 5)[0] > 0.0);
    }

    /// A transform with a zero scale has no normal matrix: the normals fall back to the face
    /// normal and the open floor still gets `π L`. (glam's `normalize_or_zero` already zeroes a
    /// non-finite vector; the explicit check keeps this independent of that.)
    #[test]
    fn a_singular_transform_falls_back_to_face_normals() {
        let mut pt = scene_with(PtSky::Uniform([1.0; 3]));
        pt.instances[0].transform = Mat4::from_scale(Vec3::new(1.0, 0.0, 1.0));
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        assert!(
            scene.instances()[0]
                .normals
                .iter()
                .flatten()
                .all(|v| v.is_finite())
        );
        let (map, _) = bake_lightmap(&scene, floor_layout(8), &[0], &settings(4), |_, _| {}).expect("bake");
        let e = map.irradiance_at(4, 4)[0];
        assert!(
            (e - std::f32::consts::PI).abs() < 0.01 * std::f32::consts::PI,
            "{e}"
        );
    }

    /// A rectangle past the atlas, an index past the unwrap, a non-finite UV2 or an atlas size
    /// out of range is an error before the raster indexes anything (it used to abort).
    #[test]
    fn bad_layouts_are_errors_before_the_raster() {
        let scene = LightmapScene::from_pt_scene(&scene_with(PtSky::Uniform([1.0; 3]))).expect("scene");
        let mut cases: Vec<(&str, LightmapLayout)> = Vec::new();
        let mut l = floor_layout(8);
        l.entries[0].rect = [4, 4, 8, 8];
        cases.push(("rect past the atlas", l));
        let mut l = floor_layout(8);
        l.entries[0].rect = [u32::MAX, 0, 8, 8];
        cases.push(("rect overflow", l));
        let mut l = floor_layout(8);
        l.unwraps[0].indices[0] = 9;
        cases.push(("index past the unwrap", l));
        let mut l = floor_layout(8);
        l.unwraps[0].uv[1] = [f32::NAN, 0.0];
        cases.push(("NaN UV2", l));
        let mut l = floor_layout(8);
        l.width = 0;
        cases.push(("empty atlas", l));
        let mut l = floor_layout(8);
        l.width = MAX_LIGHTMAP_SIZE + 1;
        cases.push(("atlas too wide", l));
        for (what, layout) in cases {
            let err = bake_lightmap(&scene, layout, &[0], &settings(1), |_, _| {}).expect_err(what);
            assert!(err.to_string().contains("lightmap layout"), "{what}: {err}");
        }
    }

    /// An entry's texels do not depend on where the packer put it: the random streams are keyed
    /// by the entry's name and the texel's place inside it. A different name is a different
    /// stream.
    #[test]
    fn an_entry_bakes_the_same_wherever_the_atlas_puts_it() {
        let mut pt = scene_with(PtSky::Uniform([1.0; 3]));
        let wall = pt.add_mesh(PtMesh {
            positions: vec![
                [-2.0, 0.0, 0.0],
                [2.0, 0.0, 0.0],
                [2.0, 1.0, 0.0],
                [-2.0, 1.0, 0.0],
            ],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..PtMesh::default()
        });
        let grey = pt.add_material(PtMaterial::lambert([0.5; 3]));
        pt.add_instance(wall, grey, Mat4::IDENTITY);
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let (here, _) = bake_lightmap(&scene, floor_layout(8), &[0], &settings(8), |_, _| {}).expect("bake");
        let mut moved = floor_layout(8);
        moved.width = 20;
        moved.height = 12;
        moved.entries[0].rect = [9, 3, 8, 8];
        moved.entries[0].scale_offset = [8.0 / 20.0, 8.0 / 12.0, 9.0 / 20.0, 3.0 / 12.0];
        let (there, _) = bake_lightmap(&scene, moved, &[0], &settings(8), |_, _| {}).expect("bake");
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(
                    here.irradiance_at(x, y),
                    there.irradiance_at(x + 9, y + 3),
                    "({x}, {y})"
                );
                assert_eq!(here.coverage_at(x, y), there.coverage_at(x + 9, y + 3));
            }
        }
        let mut renamed = floor_layout(8);
        renamed.entries[0].name = "floor_2".into();
        let (other, _) = bake_lightmap(&scene, renamed, &[0], &settings(8), |_, _| {}).expect("bake");
        assert_ne!(here.texels, other.texels, "the name keys the random streams");
    }

    #[test]
    fn the_ray_offset_leaves_the_surface_on_the_normal_side() {
        for p in [
            Vec3::ZERO,
            Vec3::new(0.01, -0.02, 0.0),
            Vec3::new(3.0, -7.5, 1.0),
            Vec3::new(-4096.0, 12.0, 65_536.0),
        ] {
            for n in [Vec3::X, -Vec3::Y, Vec3::new(0.6, 0.0, -0.8)] {
                let o = offset_origin(p, n);
                let along = (o - p).dot(n);
                assert!(along > 0.0, "p {p} n {n}: moved {along} along the normal");
                // At most 256 units in the last place of the largest coordinate per axis.
                let bound = 256.0 * 3f32.sqrt() * p.abs().max_element() * f32::EPSILON + 1e-4;
                assert!(
                    (o - p).length() <= bound,
                    "p {p} n {n}: moved {}",
                    (o - p).length()
                );
            }
        }
        assert_eq!(spot_falloff(-Vec3::Y, Vec3::NEG_Y, 0.9, 0.8), 0.0);
        assert_eq!(spot_falloff(Vec3::Y, Vec3::NEG_Y, 0.9, 0.8), 1.0);
        assert_ne!(name_hash("a"), name_hash("b"));
    }

    /// Directions (ADR-1556): a sun alone gives its own direction with full strength (`|d| = 1`,
    /// `w = n · d = cos 60°`); a uniform sky gives the cosine-weighted mean, straight up with
    /// length 2/3. The irradiance is the same with directions on or off.
    #[test]
    fn directions_point_at_the_light_and_leave_the_irradiance_alone() {
        let mut pt = scene_with(PtSky::None);
        let toward = Vec3::new(60f32.to_radians().sin(), 60f32.to_radians().cos(), 0.0);
        pt.sun = Some(PtSun {
            toward: toward.to_array(),
            color: [1.0; 3],
            illuminance: 1000.0,
            angular_radius: 0.0,
        });
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let s = LightmapBakeSettings {
            direct_sun: true,
            bounces: 1,
            ..settings(4)
        };
        let (map, report) = bake_lightmap(&scene, floor_layout(8), &[0], &s, |_, _| {}).expect("bake");
        assert!(report.directional && map.is_directional());
        let step = 2.0 / 255.0;
        for y in 1..7 {
            for x in 1..7 {
                let d = map.direction_at(x, y).expect("directional");
                for (g, w) in d.iter().zip([toward.x, toward.y, toward.z, 0.5]) {
                    assert!((g - w).abs() <= step, "texel ({x}, {y}): {d:?}");
                }
            }
        }
        let plain = LightmapBakeSettings {
            directional: false,
            ..s
        };
        let (flat, report) = bake_lightmap(&scene, floor_layout(8), &[0], &plain, |_, _| {}).expect("bake");
        assert!(!report.directional && !flat.is_directional());
        assert_eq!(flat.texels, map.texels, "directions do not change the irradiance");

        let sky = LightmapScene::from_pt_scene(&scene_with(PtSky::Uniform([1.0; 3]))).expect("scene");
        let (map, _) = bake_lightmap(&sky, floor_layout(8), &[0], &settings(64), |_, _| {}).expect("bake");
        for y in 1..7 {
            for x in 1..7 {
                let d = map.direction_at(x, y).expect("directional");
                assert!(d[0].abs() < 0.25 && d[2].abs() < 0.25, "texel ({x}, {y}): {d:?}");
                assert!((0.55..=0.78).contains(&d[1]), "texel ({x}, {y}): {d:?}");
                assert!((d[3] - d[1]).abs() <= step, "w = n · d for an up normal: {d:?}");
            }
        }
    }

    /// The noise report: an open floor under a uniform sky is exact in every sample (cosine
    /// sampling), so its error is 0 and adaptive sampling stops at the first batch; a sun with an
    /// angular radius over a wall's shadow edge is noisy, reports a positive error, and adaptive
    /// sampling takes more samples there without passing the cap.
    #[test]
    fn the_noise_report_and_adaptive_sampling_stay_within_their_bounds() {
        let sky = LightmapScene::from_pt_scene(&scene_with(PtSky::Uniform([1.0; 3]))).expect("scene");
        let adaptive = LightmapBakeSettings {
            max_samples: 64,
            noise_target: 0.001,
            ..settings(8)
        };
        let (_, report) = bake_lightmap(&sky, floor_layout(8), &[0], &adaptive, |_, _| {}).expect("bake");
        let noise = report.noise.expect("8 samples measure the noise");
        assert_eq!(noise.max, 0.0, "{noise:?}");
        assert_eq!(noise.mean_samples, 8.0, "exact texels stop at the first batch");
        assert_eq!(noise.texels_over_target, 0);

        // A cosine-weighted sky behind a wall: texels near its foot see the wall or the sky.
        let mut pt = scene_with(PtSky::Uniform([1.0; 3]));
        let wall = pt.add_mesh(PtMesh {
            positions: vec![
                [-2.0, 0.0, 0.0],
                [2.0, 0.0, 0.0],
                [2.0, 4.0, 0.0],
                [-2.0, 4.0, 0.0],
            ],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..PtMesh::default()
        });
        let grey = pt.add_material(PtMaterial::lambert([0.5; 3]));
        pt.add_instance(wall, grey, Mat4::IDENTITY);
        let walled = LightmapScene::from_pt_scene(&pt).expect("scene");
        let (_, fixed) =
            bake_lightmap(&walled, floor_layout(8), &[0], &settings(8), |_, _| {}).expect("bake");
        let fixed_noise = fixed.noise.expect("noise");
        assert!(
            fixed_noise.max > 0.0 && fixed_noise.p95 <= fixed_noise.max,
            "{fixed_noise:?}"
        );
        assert_eq!(fixed_noise.mean_samples, 8.0);
        let adaptive = LightmapBakeSettings {
            max_samples: 64,
            noise_target: 0.01,
            ..settings(8)
        };
        let (_, more) = bake_lightmap(&walled, floor_layout(8), &[0], &adaptive, |_, _| {}).expect("bake");
        let more_noise = more.noise.expect("noise");
        assert!(
            more_noise.mean_samples > 8.0 && more_noise.mean_samples <= 64.0,
            "{more_noise:?}"
        );
        assert!(
            more_noise.mean <= fixed_noise.mean,
            "{more_noise:?} vs {fixed_noise:?}"
        );
        assert!(more.rays > fixed.rays);
        // One sample cannot measure noise.
        let (_, one) = bake_lightmap(&walled, floor_layout(8), &[0], &settings(1), |_, _| {}).expect("bake");
        assert_eq!(one.noise, None);
    }

    #[test]
    fn settings_out_of_range_are_refused_before_the_bake() {
        let sky = LightmapScene::from_pt_scene(&scene_with(PtSky::Uniform([1.0; 3]))).expect("scene");
        for bad in [
            settings(MAX_LIGHTMAP_SAMPLES + 1),
            LightmapBakeSettings {
                max_samples: 4,
                ..settings(8)
            },
            LightmapBakeSettings {
                bounces: MAX_LIGHTMAP_BOUNCES + 1,
                ..settings(8)
            },
            LightmapBakeSettings {
                bounces: 0,
                ..settings(8)
            },
            LightmapBakeSettings {
                noise_target: f32::INFINITY,
                ..settings(8)
            },
        ] {
            let e = bake_lightmap(&sky, floor_layout(8), &[0], &bad, |_, _| {}).expect_err("refused");
            assert!(e.to_string().contains("lightmap settings"), "{e}");
        }
        assert_eq!(settings(8).sample_cap(), 8);
        let adaptive = LightmapBakeSettings {
            max_samples: 32,
            noise_target: 0.02,
            ..settings(8)
        };
        assert_eq!(adaptive.sample_cap(), 32);
        assert_eq!(
            LightmapBakeSettings {
                noise_target: 0.0,
                ..adaptive
            }
            .sample_cap(),
            8,
            "no target, no adaptive sampling"
        );
    }

    /// ADR-3300: a paged layout bakes each page as the same entries on one page would: two floors
    /// side by side on one page, and the same two floors with the second on page 1 at the first's
    /// rectangle, give that floor the same texels, coverage and directions, bit for bit (the
    /// filter, the gutters, the denoiser and the stitch never reach into another page).
    #[test]
    fn a_page_bakes_as_its_entries_would_on_one_page() {
        let (scene, side, paged, s) = page_case();
        let (flat, _) = bake_lightmap(&scene, side, &[0, 1], &s, |_, _| {}).expect("one page");
        let (pages, report) = bake_lightmap(&scene, paged, &[0, 1], &s, |_, _| {}).expect("two pages");
        assert_eq!(report.entries, 2);
        assert_page_matches_side(&pages, &flat);
    }

    /// [`a_page_bakes_as_its_entries_would_on_one_page`] on the GPU baker (ADR-3300): it shares the
    /// paged task list and the paged finish, so a page bakes the same texels as its entries beside
    /// each other on one page.
    #[cfg(feature = "lightmap-gpu")]
    #[test]
    fn a_page_bakes_on_the_gpu_as_its_entries_would_on_one_page() {
        let device = regia_render::RenderDevice::headless().expect("a headless device (lavapipe)");
        let (scene, side, paged, s) = page_case();
        let (flat, _) =
            super::gpu::bake_lightmap_gpu(&device, &scene, side, &[0, 1], &s, |_, _| {}).expect("one page");
        let (pages, report) =
            super::gpu::bake_lightmap_gpu(&device, &scene, paged, &[0, 1], &s, |_, _| {}).expect("two pages");
        assert_eq!(report.entries, 2);
        assert_page_matches_side(&pages, &flat);
    }

    /// Two floors (a roof over part of the second): the layout with both side by side on one 16×8
    /// page, the layout with the second on page 1 of two 8×8 pages, and the settings.
    fn page_case() -> (
        LightmapScene,
        LightmapLayout,
        LightmapLayout,
        LightmapBakeSettings,
    ) {
        let mut pt = scene_with(PtSky::Uniform([0.3, 0.4, 0.5]));
        let m = pt.add_mesh(floor(4.0));
        let mat = pt.add_material(PtMaterial::lambert([0.7, 0.3, 0.2]));
        pt.add_instance(m, mat, Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0)));
        // A small roof over part of the second floor makes its light vary across the page.
        let roof = pt.add_mesh(floor(2.0));
        pt.add_instance(roof, mat, Mat4::from_translation(Vec3::new(11.0, 0.8, 1.0)));
        pt.sun = Some(PtSun {
            toward: Vec3::new(0.3, 0.9, 0.2).normalize().to_array(),
            color: [1.0; 3],
            illuminance: 5000.0,
            angular_radius: 0.0,
        });
        let scene = LightmapScene::from_pt_scene(&pt).expect("scene");
        let one = floor_layout(8);
        let mut side = one.clone();
        side.width = 16;
        let mut a = one.entries[0].clone();
        a.scale_offset = [0.5, 1.0, 0.0, 0.0];
        let mut b = a.clone();
        b.name = "floor_b".into();
        b.rect = [8, 0, 8, 8];
        b.scale_offset = [0.5, 1.0, 0.5, 0.0];
        side.entries = vec![a, b];
        side.unwraps = vec![one.unwraps[0].clone(), one.unwraps[0].clone()];
        let mut paged = one.clone();
        let mut pb = one.entries[0].clone();
        pb.name = "floor_b".into();
        pb.page = 1;
        paged.entries.push(pb);
        paged.unwraps.push(one.unwraps[0].clone());
        let s = LightmapBakeSettings {
            direct_sun: true,
            filter: true,
            directional: true,
            stitch_seams: true,
            crease_placement: true,
            ..settings(8)
        };
        (scene, side, paged, s)
    }

    /// `pages` (two 8×8 pages) holds, texel for texel, what `flat` (one 16×8 page) holds side by
    /// side, and the second page is its own entry's light.
    fn assert_page_matches_side(pages: &Lightmap, flat: &Lightmap) {
        assert_eq!(pages.pages(), 2);
        assert_eq!(pages.exposure, flat.exposure);
        for (page, x0) in [(0u32, 0u32), (1, 8)] {
            for y in 0..8 {
                for x in 0..8 {
                    let (p, f) = (page * 8 + y, y);
                    assert_eq!(
                        pages.texels[(p * 8 + x) as usize],
                        flat.texels[(f * 16 + x0 + x) as usize],
                        "page {page} texel ({x}, {y})"
                    );
                    assert_eq!(pages.coverage_at(x, p), flat.coverage_at(x0 + x, f));
                    assert_eq!(pages.direction_at(x, p), flat.direction_at(x0 + x, f));
                }
            }
        }
        // The second floor's light is not the first's: the page holds its own entry.
        assert_ne!(
            pages.sample_entry(1, [0.5, 0.5]),
            pages.sample_entry(0, [0.5, 0.5])
        );
    }

    /// An equirect sky is baked as its solid-angle weighted mean, not the texel average: a lit
    /// polar row on a height-4 map weighs about 0.146, not 0.25.
    #[test]
    fn equirect_sky_mean_is_solid_angle_weighted() {
        let (w, h) = (2u32, 4u32);
        let mut texels = vec![[0.0f32; 4]; (w * h) as usize];
        for t in &mut texels[..w as usize] {
            *t = [1.0, 1.0, 1.0, 1.0];
        }
        let m = equirect_mean(w, h, &texels);
        let expect = ((1.0f64 - (std::f64::consts::FRAC_PI_4).cos()) / 2.0) as f32;
        assert!((m.x - expect).abs() < 1e-5, "{} vs {expect}", m.x);
        assert!(m.x < 0.2);
        let uniform = equirect_mean(w, h, &vec![[0.5, 0.5, 0.5, 1.0]; (w * h) as usize]);
        assert!((uniform.y - 0.5).abs() < 1e-6);
    }
}
