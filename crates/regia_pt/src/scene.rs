// SPDX-License-Identifier: MPL-2.0
//! The path tracer's scene: meshes, instances, materials, lights, sun, sky, an optional terrain
//! heightfield and the camera. Built from the runtime's extracted frame
//! ([`crate::extract`]) or directly by validation scenes ([`crate::scenes`]).

use std::sync::Arc;

use regia_core::glam::{Mat4, Vec3};

/// Triangle mesh in object space.
#[derive(Clone, Debug, Default)]
pub struct PtMesh {
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Vertex normals (same length as `positions`; zero normals fall back to the face normal).
    pub normals: Vec<[f32; 3]>,
    /// Texture coordinates (empty = all zero).
    pub uvs: Vec<[f32; 2]>,
    /// Linear vertex colours (empty = white).
    pub colors: Vec<[f32; 4]>,
    /// Object-space tangents, xyz and the bitangent sign in w (the runtime's `LabVertex::tangent`;
    /// empty or zero = derived from the UVs per triangle). The normal map, detail normal and
    /// anisotropy use this frame like the forward pass.
    pub tangents: Vec<[f32; 4]>,
    /// Triangle list.
    pub indices: Vec<u32>,
}

impl PtMesh {
    /// Number of triangles.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// An RGBA8 texture.
#[derive(Clone, Debug)]
pub struct PtTexture {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Row-major RGBA8, top row first. An `Arc` so a scene extracted from a frame shares a
    /// [`regia_render::SharedImage`]'s allocation (one refcount per extract) instead of
    /// copying the pixels every time; textures built from owned bytes (packs, bakes) hold
    /// their own allocation.
    pub rgba: Arc<[u8]>,
    /// Colour data stored sRGB-encoded (decoded to linear when sampled).
    pub srgb: bool,
}

/// Metallic-roughness material, the runtime's `LabMaterial` subset the tracer shades.
#[derive(Clone, Debug, PartialEq)]
pub struct PtMaterial {
    /// Linear base colour and alpha.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Perceptual roughness.
    pub roughness: f32,
    /// Emitted radiance, cd/m² (the runtime's emissive colour × intensity).
    pub emissive: [f32; 3],
    /// Normal-map scale (0 skips the map).
    pub normal_scale: f32,
    /// 0 opaque, 1 mask, 2+ blend modes (traced as a 0.5 mask).
    pub alpha_mode: u32,
    /// Mask cutoff.
    pub alpha_cutoff: f32,
    /// Clearcoat weight.
    pub clearcoat: f32,
    /// Clearcoat perceptual roughness.
    pub clearcoat_roughness: f32,
    /// Kulla–Conty multi-scatter compensation on the specular.
    pub multi_scatter: bool,
    /// Unlit: the base colour is emitted, nothing is reflected.
    pub unlit: bool,
    /// Validation only: an ideal Lambertian (`base / π`, no specular lobe), the surface the
    /// analytic references (furnace, radiosity, form factors) assume. Not a runtime material.
    pub lambert: bool,
    /// Base-colour texture index.
    pub albedo: Option<usize>,
    /// Tangent-space normal texture index.
    pub normal: Option<usize>,
    /// Emissive texture index.
    pub emissive_map: Option<usize>,
    /// Occlusion / packed ORM texture index (read only when [`Self::orm_packed`]).
    pub orm: Option<usize>,
    /// [`Self::orm`] carries the glTF metallic-roughness channels: G scales the roughness and B
    /// the metallic factor, clamped (the runtime's `LabMaterial::orm_packed`).
    pub orm_packed: bool,
    /// GGX anisotropy, −1..1 (0 = isotropic), along the tangent frame.
    pub anisotropy: f32,
    /// Rotation of the anisotropy direction, radians.
    pub anisotropy_rotation: f32,
    /// Detail maps (`HardSurfaceFactors` detail), `None` = off.
    pub detail: Option<PtDetail>,
    /// Emission leaves the front face only (quad area lights); `false` emits from both faces
    /// like the forward pass's double-sided emissive surfaces.
    pub one_sided_emission: bool,
    /// `KHR_materials_transmission` (`LabMaterial::transmission`): the share of light that
    /// passes straight through a thin dielectric, 0..=1. It replaces the diffuse lobe like
    /// the forward pass; shadow rays pass such surfaces.
    pub transmission: f32,
    /// `KHR_materials_volume` transmittance at normal incidence (`LabMaterial::
    /// transmission_tint`); the path lengthens as `1 / N·V`, clamped like the forward pass.
    pub transmission_tint: [f32; 3],
    /// `KHR_materials_ior`: the dielectric's normal-incidence reflectance is
    /// `((ior − 1) / (ior + 1))²` (1.5 gives glTF's 0.04).
    pub ior: f32,
    /// Thin film over the base specular (`KHR_materials_iridescence`), `None` = no film.
    pub thin_film: Option<PtThinFilm>,
    /// Rough transmission (ADR-2075, `regia_render::rough_transmission`): the perceptual roughness
    /// that spreads the transmitted direction, `0..=1`. 0 goes straight through.
    pub transmission_roughness: f32,
}

/// A thin film as the forward pass shades it (`regia_render::thin_film`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtThinFilm {
    /// Share of the surface the film covers, 0..=1.
    pub factor: f32,
    /// Index of refraction of the film.
    pub ior: f32,
    /// Thickness where the map's G reads 0, nanometres.
    pub thickness_min: f32,
    /// Thickness where the map's G reads 1 and without a map, nanometres.
    pub thickness_max: f32,
    /// Texture: coverage in R, thickness in G (`regia_render::pack_thin_film_map`), linear.
    pub map: Option<usize>,
}

/// Detail maps over a material, as the forward pass applies them (`lab_forward.wgsl`):
/// `base ×= mix(1, 2·detail_rgb, w)`, roughness toward the detail roughness, and the detail
/// normal blended with the normal map (whiteout), with `w = fade · mask · strength` and the
/// fade linear in the distance to the camera between `fade_start` and `fade_end`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtDetail {
    /// Texture: detail albedo RGB (linear, centred on 0.5) and roughness in A
    /// (`hard_surface::pack_detail_image`); `None` = the neutral 128 grey.
    pub image: Option<usize>,
    /// Texture: detail normal RGB and blend mask in A (`hard_surface::pack_detail_normal`);
    /// `None` = flat with a full mask.
    pub normal: Option<usize>,
    /// Albedo, normal and roughness strengths.
    pub strength: [f32; 3],
    /// UV scale of the detail maps.
    pub uv_scale: [f32; 2],
    /// Camera distance where the fade starts.
    pub fade_start: f32,
    /// Camera distance where the detail weight reaches 0 (`<= fade_start` = no fade).
    pub fade_end: f32,
}

impl Default for PtMaterial {
    fn default() -> Self {
        Self {
            base_color: [0.8, 0.8, 0.8, 1.0],
            metallic: 0.0,
            roughness: 0.5,
            emissive: [0.0; 3],
            normal_scale: 0.0,
            alpha_mode: 0,
            alpha_cutoff: 0.5,
            clearcoat: 0.0,
            clearcoat_roughness: 0.0,
            multi_scatter: false,
            unlit: false,
            lambert: false,
            albedo: None,
            normal: None,
            emissive_map: None,
            orm: None,
            orm_packed: false,
            anisotropy: 0.0,
            anisotropy_rotation: 0.0,
            detail: None,
            one_sided_emission: false,
            transmission: 0.0,
            transmission_tint: [1.0; 3],
            ior: 1.5,
            thin_film: None,
            transmission_roughness: 0.0,
        }
    }
}

impl PtMaterial {
    /// A diffuse material of linear colour `rgb`.
    #[must_use]
    pub fn diffuse(rgb: [f32; 3]) -> Self {
        Self {
            base_color: [rgb[0], rgb[1], rgb[2], 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..Self::default()
        }
    }

    /// An ideal Lambertian of reflectance `rgb` (validation scenes only).
    #[must_use]
    pub fn lambert(rgb: [f32; 3]) -> Self {
        Self {
            lambert: true,
            ..Self::diffuse(rgb)
        }
    }

    /// A black emitter of radiance `rgb` (cd/m²).
    #[must_use]
    pub fn emitter(rgb: [f32; 3]) -> Self {
        Self {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: rgb,
            roughness: 1.0,
            ..Self::default()
        }
    }

    /// True when the surface emits.
    #[must_use]
    pub fn is_emissive(&self) -> bool {
        self.emissive.iter().any(|&e| e > 0.0)
    }

    /// Dielectric reflectance at normal incidence from [`Self::ior`] (an invalid IOR is
    /// glTF's 1.5).
    #[must_use]
    pub fn dielectric_f0(&self) -> f32 {
        let ior = if self.ior.is_finite() && self.ior >= 1.0 {
            self.ior
        } else {
            1.5
        };
        let r = (ior - 1.0) / (ior + 1.0);
        r * r
    }
}

/// One placed mesh.
#[derive(Clone, Copy, Debug)]
pub struct PtInstance {
    /// Index into [`PtScene::meshes`].
    pub mesh: usize,
    /// Index into [`PtScene::materials`].
    pub material: usize,
    /// Object → world.
    pub transform: Mat4,
}

/// Point, spot and sphere lights in physical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PtLight {
    /// Isotropic point light.
    Point {
        /// World position.
        position: [f32; 3],
        /// Luminous intensity × linear colour (cd).
        intensity: [f32; 3],
    },
    /// Spot light with the runtime's smooth cone (`spot_attenuation`).
    Spot {
        /// World position.
        position: [f32; 3],
        /// Direction from the light into the scene (unit).
        direction: [f32; 3],
        /// Luminous intensity × linear colour (cd).
        intensity: [f32; 3],
        /// Cosine of the inner cone angle.
        inner_cos: f32,
        /// Cosine of the outer cone angle.
        outer_cos: f32,
    },
    /// Spherical area light.
    Sphere {
        /// Centre.
        position: [f32; 3],
        /// Radius, metres.
        radius: f32,
        /// Emitted radiance × linear colour (cd/m²).
        radiance: [f32; 3],
    },
    /// Rectangular (parallelogram) area light `corner + s·edge_u + t·edge_v`, `s, t ∈ [0, 1]`.
    /// It is traced as two emissive triangles: visible to camera and BSDF rays and sampled by
    /// next-event estimation with MIS like any emissive surface.
    Quad {
        /// One corner.
        corner: [f32; 3],
        /// First edge from the corner.
        edge_u: [f32; 3],
        /// Second edge from the corner.
        edge_v: [f32; 3],
        /// Emitted radiance × linear colour (cd/m²).
        radiance: [f32; 3],
        /// Emit from both faces; otherwise only toward `edge_u × edge_v`.
        two_sided: bool,
    },
    /// A spot light with a sphere source (`LightSourceSize` on a spot, ADR-2592): a sphere of `radius` whose
    /// radiance toward a receiver takes the runtime's smooth cone of the direction from its centre.
    SphereSpot {
        /// Centre.
        position: [f32; 3],
        /// Radius, metres.
        radius: f32,
        /// Emitted radiance × linear colour on the axis (cd/m²).
        radiance: [f32; 3],
        /// Direction from the light into the scene (unit).
        direction: [f32; 3],
        /// Cosine of the inner cone angle.
        inner_cos: f32,
        /// Cosine of the outer cone angle.
        outer_cos: f32,
    },
}

impl PtLight {
    /// Approximate emitted power used for power-weighted selection.
    #[must_use]
    pub fn power(&self) -> f32 {
        let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        match *self {
            Self::Point { intensity, .. } => 4.0 * std::f32::consts::PI * lum(intensity),
            Self::Spot {
                intensity,
                inner_cos,
                outer_cos,
                ..
            } => 2.0 * std::f32::consts::PI * (1.0 - 0.5 * (inner_cos + outer_cos)) * lum(intensity),
            Self::Sphere { radius, radiance, .. } => {
                4.0 * std::f32::consts::PI * std::f32::consts::PI * radius * radius * lum(radiance)
            }
            Self::SphereSpot {
                radius,
                radiance,
                inner_cos,
                outer_cos,
                ..
            } => {
                let cd = std::f32::consts::PI * radius * radius * lum(radiance);
                2.0 * std::f32::consts::PI * (1.0 - 0.5 * (inner_cos + outer_cos)) * cd
            }
            Self::Quad {
                edge_u,
                edge_v,
                radiance,
                two_sided,
                ..
            } => {
                let area = Vec3::from_array(edge_u).cross(Vec3::from_array(edge_v)).length();
                let sides = if two_sided { 2.0 } else { 1.0 };
                sides * std::f32::consts::PI * area * lum(radiance)
            }
        }
    }
}

/// The sun: a disc of angular radius `angular_radius` delivering `illuminance × color` at
/// normal incidence (radius 0 = a delta direction, like the forward pass's sun).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtSun {
    /// Unit vector toward the sun.
    pub toward: [f32; 3],
    /// Linear colour.
    pub color: [f32; 3],
    /// Illuminance at normal incidence, lux.
    pub illuminance: f32,
    /// Angular radius of the disc, radians (0 = delta).
    pub angular_radius: f32,
}

/// The infinitely distant environment.
#[derive(Clone, Debug, Default)]
pub enum PtSky {
    /// Black.
    #[default]
    None,
    /// The same radiance from every direction (cd/m²).
    Uniform([f32; 3]),
    /// The runtime's sky-view table (cd/m², Hillaire 2020 mapping relative to the sun azimuth).
    Table {
        /// Table width.
        width: u32,
        /// Table height.
        height: u32,
        /// Row-major RGBA texels (RGB used).
        texels: Vec<[f32; 4]>,
        /// Sun direction the table was built for (its azimuth origin).
        sun_direction: [f32; 3],
        /// Radiance multiplier.
        scale: f32,
    },
    /// An equirectangular (latitude-longitude) map, the usual HDRI layout
    /// ([`crate::env_map`]): row 0 is the zenith (+Y), the last row the nadir; the image's
    /// horizontal centre looks along −Z and `u` grows toward +X. Bilinear, U wrapped, V clamped,
    /// importance-sampled like the table. Build one with [`crate::env_map::EnvMap::into_sky`].
    Equirect {
        /// Map width (360° of azimuth).
        width: u32,
        /// Map height (180° of elevation).
        height: u32,
        /// Row-major RGBA texels, top row first (RGB used), at most
        /// [`crate::env_map::MAX_EQUIRECT_TEXELS`].
        texels: Vec<[f32; 4]>,
        /// Turn of the map about +Y, radians (positive turns +Z toward +X).
        rotation: f32,
        /// Radiance multiplier (map values × `scale` = cd/m²).
        scale: f32,
    },
}

impl PtSky {
    /// The sky-view table of the runtime's atmosphere (`build_atmosphere_luts_for`).
    #[must_use]
    pub fn from_atmosphere(luts: &regia_render::AtmosphereLuts) -> Self {
        Self::Table {
            width: luts.sky_view.width,
            height: luts.sky_view.height,
            texels: luts.sky_view.texels.clone(),
            sun_direction: luts.sun_direction,
            scale: 1.0,
        }
    }
}

/// A body in the sky (Jupiter, Io from Europa): a disc of angular radius `angular_radius`
/// toward `direction` whose radiance is tabulated over the disc. Seen directly it adds to the
/// sky behind it and hides the sun disc; it lights the scene as an area light (next-event
/// estimation over its cone, MIS with BSDF rays) with radiance × `light_scale`.
#[derive(Clone, Debug, PartialEq)]
pub struct PtSkyBody {
    /// Unit vector toward the centre of the disc.
    pub direction: [f32; 3],
    /// Angular radius, radians.
    pub angular_radius: f32,
    /// Square table of linear radiance (cd/m²) over the disc, `size × size`, row-major: texel
    /// (i, j) is the direction `normalize(c + tan(r)·(x·e1 + y·e2))` with `x, y ∈ [−1, 1]` at
    /// texel centres (`y` up), `e1`/`e2` the frame of [`Self::frame`] ([`Self::tabulate`]).
    pub radiance: Vec<[f32; 3]>,
    /// Table side.
    pub size: u32,
    /// Multiplier on the radiance when the body lights the scene (camera rays see it
    /// unscaled): the runtime's authored `fill_lux` over the disc's own illuminance, 1 =
    /// physical.
    pub light_scale: f32,
}

impl PtSkyBody {
    /// A disc of uniform radiance (validation: irradiance `π·L·sin²r` at normal incidence).
    #[must_use]
    pub fn uniform(direction: [f32; 3], angular_radius: f32, radiance: [f32; 3]) -> Self {
        let size = 64u32;
        let mut body = Self {
            direction,
            angular_radius,
            radiance: Vec::new(),
            size,
            light_scale: 1.0,
        };
        body.radiance = body.tabulate(|_| radiance);
        body
    }

    /// The disc's tangent frame `(c, e1, e2)`: `c` toward the centre, `e2` toward world +Y
    /// projected onto the disc (its north, `lit_sphere`'s pole), `e1 = e2 × c`. The tracer reads
    /// the table in this frame (`pack.rs` uploads it).
    #[must_use]
    pub fn frame(&self) -> (Vec3, Vec3, Vec3) {
        let c = Vec3::from_array(self.direction).normalize_or(Vec3::Y);
        let mut pole = Vec3::Y - c * Vec3::Y.dot(c);
        if pole.length() < 1e-3 {
            pole = Vec3::X - c * Vec3::X.dot(c);
        }
        let e2 = pole.normalize();
        let e1 = e2.cross(c).normalize();
        (c, e1, e2)
    }

    /// Fills the table with `f(direction)`. Texels outside the disc take the value at the rim
    /// in their direction, so bilinear lookups just inside the rim are not darkened (the
    /// tracer returns 0 outside the disc itself).
    #[must_use]
    pub fn tabulate(&self, f: impl Fn(Vec3) -> [f32; 3]) -> Vec<[f32; 3]> {
        let (c, e1, e2) = self.frame();
        let n = self.size.max(1);
        let r = self.angular_radius.clamp(0.0, 1.5);
        let tan_r = r.tan();
        let mut out = Vec::with_capacity((n * n) as usize);
        for j in 0..n {
            for i in 0..n {
                let x = (i as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let y = 1.0 - (j as f32 + 0.5) / n as f32 * 2.0;
                let offset = e1 * x + e2 * y;
                let max_offset = (r * (1.0 - 1e-4)).tan();
                let len = offset.length() * tan_r;
                let d = if len > max_offset {
                    (c + offset.normalize() * max_offset).normalize()
                } else {
                    (c + offset * tan_r).normalize()
                };
                out.push(f(d));
            }
        }
        out
    }

    /// Irradiance (lux) the disc delivers to a surface facing its centre, from the table
    /// (`∫ L cos θ dω` over the disc).
    #[must_use]
    pub fn normal_irradiance(&self) -> f32 {
        let (c, e1, e2) = self.frame();
        let n = self.size.max(1);
        let tan_r = self.angular_radius.clamp(0.0, 1.5).tan();
        let texel = 2.0 * tan_r / n as f32;
        let mut sum = 0.0f64;
        for j in 0..n {
            for i in 0..n {
                let x = (i as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let y = 1.0 - (j as f32 + 0.5) / n as f32 * 2.0;
                let p = c + (e1 * x + e2 * y) * tan_r;
                let len = p.length();
                if 1.0 / len < self.angular_radius.cos() {
                    continue;
                }
                // Solid angle of a texel on the tangent plane at distance 1: area · cos / r².
                let d_omega = texel * texel / (len * len * len);
                let cos = 1.0 / len;
                let Some(&l) = self.radiance.get(j as usize * n as usize + i as usize) else {
                    continue;
                };
                let lum = 0.2126 * l[0] + 0.7152 * l[1] + 0.0722 * l[2];
                sum += f64::from(lum * cos * d_omega);
            }
        }
        sum as f32
    }
}

/// One layer of participating medium: extinction and scattering coefficients (1/m) at
/// `reference_height`, decaying as `exp(−falloff · (y − reference_height))` and zero outside
/// `[min_height, max_height]` (world Y). A falloff of 0 between two heights is a homogeneous
/// slab. The planet's air (Rayleigh, Mie) and height fog are layers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtMediumLayer {
    /// Scattering coefficient per channel at the reference height, 1/m.
    pub scattering: [f32; 3],
    /// Absorption coefficient per channel at the reference height, 1/m.
    pub absorption: [f32; 3],
    /// World Y of the reference density.
    pub reference_height: f32,
    /// Exponential falloff with height, 1/m (0 = constant).
    pub falloff: f32,
    /// Lower bound of the layer, world Y.
    pub min_height: f32,
    /// Upper bound of the layer, world Y.
    pub max_height: f32,
    /// Phase function.
    pub phase: PtPhase,
    /// The planet's air: the sky table and the sun's illuminance already integrate it, so it
    /// only applies to segments that end at a surface or at a local light (aerial
    /// perspective), never to rays that escape to the sky or toward the sun or a sky body.
    pub aerial: bool,
}

/// Phase function of a [`PtMediumLayer`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PtPhase {
    /// Rayleigh, `3/(16π)·(1 + cos²θ)`.
    Rayleigh,
    /// Henyey–Greenstein with a per-channel asymmetry.
    HenyeyGreenstein([f32; 3]),
}

/// Participating media along every path segment ([`PtMediumLayer`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PtMedia {
    /// Layers (at most 8).
    pub layers: Vec<PtMediumLayer>,
    /// Rays that escape to the sky see the non-aerial layers out to this distance, metres
    /// (the real-time froxel fog's range; 0 = unbounded).
    pub escape_distance: f32,
}

impl PtMedia {
    /// True when any layer scatters or absorbs.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.layers.iter().any(|l| {
            l.scattering.iter().chain(l.absorption.iter()).any(|v| *v > 0.0) && l.max_height > l.min_height
        })
    }
}

/// What fills a [`PtCloudField`]'s shell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PtCloudDensity {
    /// The raster sky's density: the weather map, the noise volumes and the height profile of
    /// [`regia_render::CloudSettings`], through `regia::cloud_density` (the one definition).
    Noise,
    /// A constant normalised density `0..=1` everywhere between the base and the top: a
    /// horizontally uniform layer with a known answer, for checking the tracking itself.
    Uniform(f32),
}

/// Phase function of a [`PtCloudField`]: a mix of two Henyey–Greenstein lobes,
/// `forward_weight · HG(forward_g) + (1 − forward_weight) · HG(back_g)`. Sampled exactly (a lobe
/// by its weight, then HG by inversion), so the sampling weight is 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtCloudPhase {
    /// Asymmetry of the first lobe, `-0.99..=0.99`.
    pub forward_g: f32,
    /// Asymmetry of the second lobe, `-0.99..=0.99`.
    pub back_g: f32,
    /// Weight of the first lobe, `0..=1`.
    pub forward_weight: f32,
}

impl PtCloudPhase {
    /// Water droplets in the visible, one lobe of asymmetry 0.85 (Mie scattering by droplets of
    /// about 10 µm; [`regia_render::CLOUD_DROPLET_ASYMMETRY`]).
    pub const DROPLET: Self = Self {
        forward_g: regia_render::CLOUD_DROPLET_ASYMMETRY,
        back_g: 0.0,
        forward_weight: 1.0,
    };

    /// The raster march's first octave, `mix(HG(0.8), HG(−0.3), 0.3)` (`dual_hg` in
    /// `clouds.wgsl`).
    pub const RASTER_DUAL_LOBE: Self = Self {
        forward_g: 0.8,
        back_g: -0.3,
        forward_weight: 0.7,
    };
}

/// A volumetric cloud layer as a participating medium (ADR-2127): the raster sky's own density
/// between two curved shells of the planet, traced with delta tracking for the free flights and
/// ratio tracking for the shadow rays, so any number of scattering orders is unbiased.
///
/// `None` on [`PtScene::clouds`] (the default) traces the scene exactly as before.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtCloudField {
    /// The layer: heights, extinction, coverage, type, wind and time, tiles, look, seed.
    pub settings: regia_render::CloudSettings,
    /// Radius of the planet the shells curve with, metres.
    pub planet_radius_m: f32,
    /// Single-scattering albedo, `0..=1` (`1` for water in the visible; the raster march uses
    /// 0.99, `regia_render::CLOUD_MARCH_ALBEDO`).
    pub albedo: f32,
    /// Phase function.
    pub phase: PtCloudPhase,
    /// The detail erosion swirls with the curl field, as the raster does with its cone light
    /// (`cloud_cone_light`, Desktop High and up).
    pub curl: bool,
    /// What fills the shell.
    pub density: PtCloudDensity,
}

impl PtCloudField {
    /// The physical reference of `settings`: albedo 1, droplet phase, curl on (the look of the
    /// quality presets that draw clouds), the raster sky's density.
    #[must_use]
    pub fn from_settings(settings: regia_render::CloudSettings, planet_radius_m: f32) -> Self {
        Self {
            settings,
            planet_radius_m,
            albedo: 1.0,
            phase: PtCloudPhase::DROPLET,
            curl: true,
            density: PtCloudDensity::Noise,
        }
    }
}

/// Terrain heightfield: `heights[j * width + i]` at `(origin.x + i·spacing.x, origin.z +
/// j·spacing.z)`; cells split along the (i, j+1)–(i+1, j) diagonal like the CDLOD grid.
///
/// A NaN height is a cut sample (a hole: a tunnel mouth, [`Self::cut_holes`]): every cell
/// touching one is dropped, as the physics heightfield drops its cells, so rays pass through
/// and probes there are not under the terrain. The raster's cut follows the exact footprint;
/// here the hole is the cells around its samples (up to one cell wider).
#[derive(Clone, Debug)]
pub struct PtHeightfield {
    /// Samples along X.
    pub width: u32,
    /// Samples along Z.
    pub depth: u32,
    /// World X, Z of sample (0, 0).
    pub origin: [f32; 2],
    /// Sample spacing along X and Z, metres.
    pub spacing: [f32; 2],
    /// Heights, metres.
    pub heights: Vec<f32>,
    /// Material; its textures are mapped over the whole map (UV 0..1 across X and Z).
    pub material: usize,
}

impl PtHeightfield {
    /// The runtime terrain map's heights at full resolution.
    #[must_use]
    pub fn from_terrain_map(map: &regia_terrain::TerrainMap, material: usize) -> Self {
        let w = map.width();
        let h = map.height();
        let o = map.sample_to_world(0, 0);
        let far = map.sample_to_world(w - 1, h - 1);
        Self {
            width: w,
            depth: h,
            origin: o,
            spacing: [
                (far[0] - o[0]) / (w - 1).max(1) as f32,
                (far[1] - o[1]) / (h - 1).max(1) as f32,
            ],
            heights: map.heights().to_vec(),
            material,
        }
    }

    /// Cuts the samples inside `holes` (sets them to NaN, as
    /// [`regia_terrain::HolesMask::cut_heights`] does), so the tracer drops every cell
    /// touching the hole. Returns the samples cut. A shared draw's
    /// [`regia_render::ReferenceHeightfield`] arrives cut already.
    pub fn cut_holes(&mut self, holes: &regia_terrain::HolesMask) -> usize {
        let (w, origin, spacing) = (self.width.max(1) as usize, self.origin, self.spacing);
        holes.cut_heights(&mut self.heights, 0.0, |i| {
            [
                origin[0] + (i % w) as f32 * spacing[0],
                origin[1] + (i / w) as f32 * spacing[1],
            ]
        })
    }

    /// Height at sample (i, j), clamped to the grid.
    #[must_use]
    pub fn height(&self, i: u32, j: u32) -> f32 {
        let i = i.min(self.width.saturating_sub(1));
        let j = j.min(self.depth.saturating_sub(1));
        // An empty or short grid has no sample: NaN, like a cut cell.
        self.heights
            .get(j as usize * self.width as usize + i as usize)
            .copied()
            .unwrap_or(f32::NAN)
    }

    /// Surface height at world `(x, z)` on the traced triangles (cells split along the
    /// (i, j+1)–(i+1, j) diagonal); `None` outside the grid and in a cut cell (a hole: no
    /// ground).
    #[must_use]
    pub fn height_at(&self, x: f32, z: f32) -> Option<f32> {
        if self.width < 2
            || self.depth < 2
            || (self.heights.len() as u64) < u64::from(self.width) * u64::from(self.depth)
        {
            return None;
        }
        let gx = (x - self.origin[0]) / self.spacing[0];
        let gz = (z - self.origin[1]) / self.spacing[1];
        let (max_x, max_z) = ((self.width - 1) as f32, (self.depth - 1) as f32);
        if !(gx >= 0.0 && gz >= 0.0 && gx <= max_x && gz <= max_z) {
            return None;
        }
        let i = (gx.floor() as u32).min(self.width - 2);
        let j = (gz.floor() as u32).min(self.depth - 2);
        let (fx, fz) = (gx - i as f32, gz - j as f32);
        let h00 = self.height(i, j);
        let h10 = self.height(i + 1, j);
        let h01 = self.height(i, j + 1);
        let h11 = self.height(i + 1, j + 1);
        let h = if fx + fz <= 1.0 {
            h00 + fx * (h10 - h00) + fz * (h01 - h00)
        } else {
            h11 + (1.0 - fx) * (h01 - h11) + (1.0 - fz) * (h10 - h11)
        };
        // A cut corner makes the whole cell NaN: the tracer drops it.
        if [h00, h10, h01, h11].iter().all(|v| v.is_finite()) {
            Some(h)
        } else {
            None
        }
    }
}

/// Camera (`FrameScene` convention: +Y up, vertical field of view): a pinhole, or a thin lens
/// when [`Self::aperture_radius`] is above 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtCamera {
    /// Eye.
    pub eye: [f32; 3],
    /// Look-at point.
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Image width.
    pub width: u32,
    /// Image height.
    pub height: u32,
    /// Thin-lens aperture radius, metres (0 = pinhole: everything in focus).
    pub aperture_radius: f32,
    /// Distance along the view axis that is in focus, metres (used when the aperture is open).
    pub focus_distance: f32,
}

impl PtCamera {
    /// A pinhole camera.
    #[must_use]
    pub fn pinhole(eye: [f32; 3], target: [f32; 3], fov_y: f32, width: u32, height: u32) -> Self {
        Self {
            eye,
            target,
            fov_y,
            width,
            height,
            aperture_radius: 0.0,
            focus_distance: 1.0,
        }
    }

    /// A thin lens of focal length `focal_mm` at f-number `f_number` focused at `focus_m`:
    /// aperture radius `f / (2N)`.
    #[must_use]
    pub fn with_lens(mut self, focal_mm: f32, f_number: f32, focus_m: f32) -> Self {
        self.aperture_radius = (focal_mm * 1e-3 / (2.0 * f_number.max(0.1))).max(0.0);
        self.focus_distance = focus_m.max(1e-3);
        self
    }

    /// Orthonormal (forward, right, up) of the camera.
    #[must_use]
    pub fn basis(&self) -> (Vec3, Vec3, Vec3) {
        let eye = Vec3::from_array(self.eye);
        let fwd = (Vec3::from_array(self.target) - eye).normalize_or(Vec3::NEG_Z);
        let mut right = fwd.cross(Vec3::Y);
        if right.length_squared() < 1e-10 {
            right = fwd.cross(Vec3::Z);
        }
        let right = right.normalize();
        let up = right.cross(fwd).normalize();
        (fwd, right, up)
    }
}

/// Everything the tracer renders.
#[derive(Clone, Debug)]
pub struct PtScene {
    /// Meshes (shared by instances).
    pub meshes: Vec<PtMesh>,
    /// Placed meshes.
    pub instances: Vec<PtInstance>,
    /// Materials.
    pub materials: Vec<PtMaterial>,
    /// Textures.
    pub textures: Vec<PtTexture>,
    /// Analytic local lights (emissive surfaces become lights automatically).
    pub lights: Vec<PtLight>,
    /// The sun.
    pub sun: Option<PtSun>,
    /// The frame's directional lights besides the sun (ADR-7211, ADR-7254), at most
    /// `regia_render::MAX_EXTRA_DIRECTIONAL` from an extracted frame. The CPU probe and lightmap
    /// bakes light with each like the sun (its direct light in the sun component), unshadowed
    /// only in the forward; empty (the default) keeps every scene's hash and bake bytes.
    pub extra_suns: Vec<PtSun>,
    /// The sky.
    pub sky: PtSky,
    /// The terrain.
    pub terrain: Option<PtHeightfield>,
    /// The terrain albedo a raster frame shaded each pixel with, for camera-ray hits on
    /// [`Self::terrain`] (see [`PtViewAlbedo`]); `None` = the terrain material everywhere.
    pub terrain_view_albedo: Option<PtViewAlbedo>,
    /// Bodies in the sky (at most 4).
    pub sky_bodies: Vec<PtSkyBody>,
    /// Peak radiance (cd/m²) of the star field seen against the sky: the raster's airless
    /// `day_star_radiance`; 0 = none. Stars are background only (they light nothing).
    pub stars: f32,
    /// Participating media.
    pub media: PtMedia,
    /// A volumetric cloud layer over the scene (ADR-2127). `None` (the default) traces none.
    pub clouds: Option<PtCloudField>,
    /// The camera.
    pub camera: PtCamera,
    /// The BRDF's energy model (ADR-2632): [`regia_render::BrdfEnergy::Legacy`] (the forward
    /// pass's shipped BRDF) or `Compensated`, the forward pass's `BRDF_ENERGY_COMPENSATED`
    /// variant. [`Self::new`] takes [`regia_render::process_brdf_energy`], legacy by default.
    pub brdf_energy: regia_render::BrdfEnergy,
    /// IES profiles on point, spot and sphere lights (ADR-2592): each names a light of [`Self::lights`] by index.
    pub light_profiles: Vec<PtLightProfile>,
    /// Material graphs (ADR-2725): each names a material of [`Self::materials`] by index, whose
    /// surface stage the graph then replaces, as in the forward pass. Empty for none.
    pub surface_graphs: Vec<PtSurfaceGraph>,
    /// Subsurface scattering (ADR-2725): each names a material of [`Self::materials`] by index,
    /// whose diffuse light then spreads under the surface by Burley's normalized diffusion.
    pub subsurface: Vec<PtSubsurface>,
    /// Projected decals (ADR-2951, [`crate::PtDecal`]): the frame's decal projection, weights and
    /// textures applied to the hit's albedo, normal, roughness and emission. Empty (the default)
    /// traces the base kernels, bit for bit as before.
    pub decals: Vec<crate::PtDecal>,
}

/// A material graph's surface stage on one of a scene's materials (ADR-2725): the forward pass's
/// generated `material_surface` text, its parameter block and its textures. The tracer composes
/// the text into its kernels (`crate::graph`); what it cannot evaluate it refuses by name in the
/// image's metadata, and that material traces with its standard factors.
///
/// Emission: on a graph that composes, BSDF hits show the graph's emission, but the light list
/// is built from the material's standard `emissive` factors. `crate::extract` zeroes them for
/// such a material, as the forward pass ignores them; a caller that builds a [`PtScene`]
/// directly must do the same, or the standard emission is sampled as triangle lights while hits
/// show another emitter.
#[derive(Clone, Debug, PartialEq)]
pub struct PtSurfaceGraph {
    /// Index of the material in [`PtScene::materials`].
    pub material: usize,
    /// The surface text as the forward pass splices it
    /// (`regia_render::surface_graph::surface_source`).
    pub surface: std::sync::Arc<str>,
    /// The parameter block (slot 0: time, previous time, wind; the floating origin in the graph's
    /// origin slot), as the frame uploads it.
    pub params: [[f32; 4]; regia_render::surface_graph::GRAPH_PARAM_SLOTS],
    /// Textures by graph slot, as indices into [`PtScene::textures`]; `None` samples white.
    pub textures: [Option<usize>; regia_render::surface_graph::GRAPH_TEXTURE_SLOTS],
    /// The graph also offsets vertex positions (refused: the tracer traces the undisplaced mesh).
    pub vertex_offset: bool,
    /// The graph reads the second UV set (refused: the tracer's meshes carry one).
    pub uses_uv1: bool,
    /// The graph reads the material clock (slot 0): `crate::extract::apply_material_clock` writes it.
    pub uses_time: bool,
}

/// Subsurface scattering on one of a scene's materials (ADR-2725): Burley's normalized diffusion
/// profile `R(r) = (e^(-r/d) + e^(-r/(3d))) / (8π d r)` per channel, sampled by disc projection.
/// A share [`Self::tint`] of the diffuse light enters at a point the profile picks and leaves at
/// the shaded one; the rest stays where it landed, as in the frame's screen-space pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtSubsurface {
    /// Index of the material in [`PtScene::materials`].
    pub material: usize,
    /// The profile's shape `d` per channel, meters (the mean free path over Burley's scaling).
    pub d: [f32; 3],
    /// Share of the diffuse light that scatters per channel, `0..=1`.
    pub tint: [f32; 3],
}

/// An IES photometric profile on one of a scene's analytic lights (ADR-2592): the light's emission toward a
/// direction is scaled by the profile's relative intensity there (`regia_render::light_profiles::relative_gpu`).
#[derive(Clone, Debug, PartialEq)]
pub struct PtLightProfile {
    /// Index of the light in [`PtScene::lights`] (a point, spot, sphere or sized spot).
    pub light: usize,
    /// The cooked profile.
    pub profile: std::sync::Arc<regia_asset::ies::CookedIesProfile>,
    /// The light's nadir (a spot's direction; a point light's rotated `-Y`).
    pub nadir: [f32; 3],
    /// Horizontal turn in `regia_render::light_profiles::PROFILE_TURN_STEPS` steps.
    pub turn: u8,
}

/// A screen-space terrain albedo: what a raster frame's terrain shader computed as the albedo
/// of every pixel it drew (layer weights, texture mips at the pixel footprint, distance-faded
/// near-field terms), seen through [`Self::view_proj`]. Where a camera ray's first hit is the
/// heightfield and that point projects inside the image onto a covered texel (alpha > 0), the
/// tracer shades it with this albedo instead of the heightfield material's texture (and, with
/// [`Self::normal`], with the raster's shading normal instead of the heightfield's smooth one),
/// so the reference and the raster light the same material at that pixel (`specs/27` § 1.1
/// "same inputs"). With [`Self::all_bounces`], bounce-ray hits on terrain the camera sees take
/// them too; terrain the camera does not see keeps the material's map-wide texture and smooth
/// normals.
#[derive(Clone, Debug, PartialEq)]
pub struct PtViewAlbedo {
    /// World → clip of the raster camera (column-major, 0..1 depth, glam's `*_rh` directx
    /// convention): clip x/y over w map to the texture as u = (x + 1)/2, v = (1 − y)/2.
    pub view_proj: [[f32; 4]; 4],
    /// Index into [`PtScene::textures`]: sRGB RGBA8, top row first, alpha 0 where the raster
    /// drew no terrain.
    pub texture: usize,
    /// The raster's world shading normal per pixel, same layout: index into
    /// [`PtScene::textures`], RGB = 0.5 + 0.5·n (decoded through the texture's sRGB flag),
    /// alpha 0 where no terrain. `None` = the heightfield's interpolated normal.
    pub normal: Option<usize>,
    /// The raster camera's eye (world): with [`Self::all_bounces`], the visibility test's end.
    pub eye: [f32; 3],
    /// `true`: every heightfield hit the camera sees (in the image and unoccluded by the
    /// heightfield along the segment to [`Self::eye`]) takes the view inputs, so light bounced
    /// off visible terrain carries the albedo the frame shows there; `false`: camera rays only.
    pub all_bounces: bool,
}

impl PtScene {
    /// Installs `albedo` (sRGB RGBA8, top row first, alpha 0 = no terrain) as the terrain's
    /// albedo where the camera at `eye` sees it through `view_proj` ([`PtViewAlbedo`], every
    /// bounce), and `normal` (same layout, 0.5 + 0.5·n) as its shading normal when given. Set
    /// [`PtViewAlbedo::all_bounces`] to `false` afterwards for camera rays only.
    pub fn set_terrain_view_albedo(
        &mut self,
        view_proj: [[f32; 4]; 4],
        eye: [f32; 3],
        albedo: PtTexture,
        normal: Option<PtTexture>,
    ) {
        self.textures.push(albedo);
        let texture = self.textures.len() - 1;
        let normal = normal.map(|n| {
            self.textures.push(n);
            self.textures.len() - 1
        });
        self.terrain_view_albedo = Some(PtViewAlbedo {
            view_proj,
            texture,
            normal,
            eye,
            all_bounces: true,
        });
    }

    /// An empty scene seen by `camera`.
    #[must_use]
    pub fn new(camera: PtCamera) -> Self {
        Self {
            meshes: Vec::new(),
            instances: Vec::new(),
            materials: Vec::new(),
            textures: Vec::new(),
            lights: Vec::new(),
            sun: None,
            extra_suns: Vec::new(),
            sky: PtSky::None,
            terrain: None,
            terrain_view_albedo: None,
            sky_bodies: Vec::new(),
            stars: 0.0,
            media: PtMedia::default(),
            clouds: None,
            camera,
            // The process-wide model the forward modules compose with (legacy unless a host or
            // `REGIA_BRDF_ENERGY` chose otherwise), so a reference renders what the frame shades.
            brdf_energy: regia_render::process_brdf_energy(),
            light_profiles: Vec::new(),
            surface_graphs: Vec::new(),
            subsurface: Vec::new(),
            decals: Vec::new(),
        }
    }

    /// Adds a material, returning its index.
    pub fn add_material(&mut self, material: PtMaterial) -> usize {
        self.materials.push(material);
        self.materials.len() - 1
    }

    /// Adds a mesh, returning its index.
    pub fn add_mesh(&mut self, mesh: PtMesh) -> usize {
        self.meshes.push(mesh);
        self.meshes.len() - 1
    }

    /// Places `mesh` with `material` at `transform`.
    pub fn add_instance(&mut self, mesh: usize, material: usize, transform: Mat4) {
        self.instances.push(PtInstance {
            mesh,
            material,
            transform,
        });
    }

    /// A stable hash of everything that changes the image (for metadata and caches).
    #[must_use]
    pub fn content_hash(&self) -> u64 {
        // FNV-1a over the packed bytes; enough to tell two scenes apart in a report.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for m in &self.meshes {
            eat(bytemuck::cast_slice(&m.positions));
            eat(bytemuck::cast_slice(&m.indices));
            eat(bytemuck::cast_slice(&m.normals));
            eat(bytemuck::cast_slice(&m.uvs));
            eat(bytemuck::cast_slice(&m.colors));
            eat(bytemuck::cast_slice(&m.tangents));
        }
        for i in &self.instances {
            eat(bytemuck::cast_slice(&i.transform.to_cols_array()));
            eat(&(i.mesh as u64).to_le_bytes());
            eat(&(i.material as u64).to_le_bytes());
        }
        for m in &self.materials {
            eat(format!("{m:?}").as_bytes());
        }
        for t in &self.textures {
            eat(&t.rgba);
            eat(format!("{}x{}{}", t.width, t.height, t.srgb).as_bytes());
        }
        eat(format!("{:?}{:?}{:?}", self.lights, self.sun, self.camera).as_bytes());
        // Only extra lights feed the hash, so every one-sun scene keeps its hash (ADR-7254).
        if !self.extra_suns.is_empty() {
            eat(format!("extra_suns:{:?}", self.extra_suns).as_bytes());
        }
        // Only a non-default model feeds the hash, so every legacy scene keeps its hash.
        if self.brdf_energy != regia_render::BrdfEnergy::Legacy {
            eat(format!("brdf_energy:{}", self.brdf_energy.name()).as_bytes());
        }
        if let PtSky::Table {
            texels,
            width,
            height,
            sun_direction,
            scale,
        } = &self.sky
        {
            eat(bytemuck::cast_slice(texels));
            eat(format!("{width}x{height}{sun_direction:?}{scale}").as_bytes());
        } else if let PtSky::Equirect {
            width,
            height,
            texels,
            rotation,
            scale,
        } = &self.sky
        {
            eat(bytemuck::cast_slice(texels));
            eat(format!("equirect{width}x{height}r{rotation}s{scale}").as_bytes());
        } else {
            eat(format!("{:?}", self.sky).as_bytes());
        }
        if let Some(t) = &self.terrain {
            eat(bytemuck::cast_slice(&t.heights));
            eat(format!(
                "{}x{}{:?}{:?}{}",
                t.width, t.depth, t.origin, t.spacing, t.material
            )
            .as_bytes());
        }
        eat(format!("{:?}", self.terrain_view_albedo).as_bytes());
        for b in &self.sky_bodies {
            eat(bytemuck::cast_slice(&b.radiance));
            eat(format!("{:?}{}{}", b.direction, b.angular_radius, b.light_scale).as_bytes());
        }
        eat(format!("{:?}{}", self.media, self.stars).as_bytes());
        // Only when present, so the hash of a scene without clouds stays what it was.
        if let Some(c) = &self.clouds {
            eat(format!("{c:?}").as_bytes());
        }
        // Graphs and subsurface only when present (ADR-2725): every other scene keeps its hash.
        for g in &self.surface_graphs {
            eat(g.surface.as_bytes());
            eat(format!(
                "{}{:?}{:?}{}{}",
                g.material, g.params, g.textures, g.vertex_offset, g.uses_uv1
            )
            .as_bytes());
        }
        for sss in &self.subsurface {
            eat(format!("sss{sss:?}").as_bytes());
        }
        // Decals only when present (ADR-2951): every other scene keeps its hash.
        for d in &self.decals {
            eat(format!("decal{d:?}").as_bytes());
        }
        // Likewise only when present (ADR-3292): an IES profile changes every direction a light emits toward, so
        // a cache keyed on this hash must not serve the image of the scene without it.
        if !self.light_profiles.is_empty() {
            eat(format!("{:?}", self.light_profiles).as_bytes());
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> PtScene {
        PtScene::new(PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.8, 8, 8))
    }

    #[test]
    fn content_hash_sees_an_extra_sun_and_keeps_one_sun_scenes() {
        let base = scene();
        let mut extra = scene();
        extra.extra_suns.push(PtSun {
            toward: [0.0, 1.0, 0.0],
            color: [1.0; 3],
            illuminance: 10.0,
            angular_radius: 0.0,
        });
        assert_ne!(base.content_hash(), extra.content_hash());
        extra.extra_suns.clear();
        assert_eq!(
            base.content_hash(),
            extra.content_hash(),
            "no extra suns: the hash it always had"
        );
    }

    #[test]
    fn content_hash_sees_every_image_input() {
        let base = {
            let mut s = scene();
            s.add_mesh(PtMesh {
                positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                uvs: vec![[0.0; 2]; 3],
                colors: Vec::new(),
                tangents: Vec::new(),
                indices: vec![0, 1, 2],
            });
            s.sky = PtSky::Table {
                width: 1,
                height: 1,
                texels: vec![[1.0; 4]],
                sun_direction: [0.0, 1.0, 0.0],
                scale: 1.0,
            };
            s.terrain = Some(PtHeightfield {
                width: 2,
                depth: 2,
                origin: [0.0; 2],
                spacing: [1.0; 2],
                heights: vec![0.0; 4],
                material: 0,
            });
            s
        };
        let h0 = base.content_hash();
        assert_eq!(h0, base.clone().content_hash());

        let mut s = base.clone();
        s.meshes[0].normals[0] = [0.0, 1.0, 0.0];
        assert_ne!(s.content_hash(), h0, "normals");
        let mut s = base.clone();
        s.meshes[0].uvs[1] = [1.0, 0.0];
        assert_ne!(s.content_hash(), h0, "uvs");
        let mut s = base.clone();
        if let PtSky::Table { scale, .. } = &mut s.sky {
            *scale = 2.0;
        }
        assert_ne!(s.content_hash(), h0, "sky scale");
        let mut s = base.clone();
        if let Some(t) = &mut s.terrain {
            t.origin = [5.0, 0.0];
        }
        assert_ne!(s.content_hash(), h0, "terrain origin");
        let mut s = base.clone();
        s.set_terrain_view_albedo(
            [[0.0; 4]; 4],
            [0.0; 3],
            PtTexture {
                width: 1,
                height: 1,
                rgba: std::sync::Arc::from(vec![255; 4]),
                srgb: true,
            },
            None,
        );
        let with_albedo = s.content_hash();
        assert_ne!(with_albedo, h0, "view albedo");
        if let Some(a) = &mut s.terrain_view_albedo {
            a.all_bounces = false;
        }
        assert_ne!(s.content_hash(), with_albedo, "all_bounces");
        s.textures[0].srgb = false;
        let h = s.content_hash();
        s.textures[0].srgb = true;
        assert_ne!(s.content_hash(), h, "texture srgb");
        let mut s = base.clone();
        s.brdf_energy = regia_render::BrdfEnergy::Compensated;
        assert_ne!(s.content_hash(), h0, "brdf energy");
        s.brdf_energy = regia_render::BrdfEnergy::Legacy;
        assert_eq!(
            s.content_hash(),
            h0,
            "the default model leaves the hash as it was"
        );
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    #[test]
    fn empty_heightfield_height_is_nan_not_a_panic() {
        let h = PtHeightfield {
            width: 0,
            depth: 0,
            origin: [0.0; 2],
            spacing: [1.0; 2],
            heights: Vec::new(),
            material: 0,
        };
        assert!(h.height(3, 4).is_nan());
        assert_eq!(h.height_at(0.0, 0.0), None);
        let short = PtHeightfield {
            width: 4,
            depth: 4,
            heights: vec![1.0; 3],
            ..h
        };
        assert!(short.height(3, 3).is_nan());
        assert_eq!(short.height(1, 0), 1.0);
    }

    #[test]
    fn short_sky_body_table_does_not_panic() {
        let mut body = PtSkyBody::uniform([0.0, 1.0, 0.0], 0.1, [1.0; 3]);
        let full = body.normal_irradiance();
        assert!(full > 0.0);
        body.radiance.truncate(1);
        assert!(body.normal_irradiance() <= full);
    }
}
