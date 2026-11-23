// SPDX-License-Identifier: MPL-2.0
//! Scene description types for the clustered forward frame (`specs/13` §§ 5–7).
//!
//! The heavy lifting lives in [`crate::frame::FrameRenderer`]; this module keeps
//! the CPU-side description a frame is built from. The golden harness renders
//! [`LabScene`]s; the ECS runtime path (`crate::runtime`) builds the same
//! [`crate::frame::FrameScene`] from extracted components.

use crate::frame::{FrameMesh, FrameRenderer, FrameScene};
use crate::skin::JointMatrix;
use crate::{ForwardResult, RenderDevice, RenderError};

/// Visible-light cap of the lab harness. The GPU buffer holds
/// [`crate::MAX_UPLOADED_LIGHTS`]; [`crate::FrameQuality::LAB`] stays at this
/// cap so a frame that already fits does not reshuffle.
pub const LAB_MAX_LIGHTS: u32 = 64;
/// Maximum lights stored per froxel.
pub const LAB_MAX_PER_CLUSTER: u32 = 32;
/// Physical radiance scale for the display-referred procedural sky (cd/m²).
///
/// The sky maps hold values in 0..1; multiplying by this makes the sky's
/// diffuse irradiance ≈ 15,000 lux — typical ambient skylight — so IBL survives
/// EV100 exposure alongside a ~100 klx sun (`specs/13` § 6).
pub const SKY_IBL_SCALE: f32 = 9000.0;

/// Opaque alpha mode.
pub const ALPHA_OPAQUE: u32 = 0;
/// Alpha-test mode.
pub const ALPHA_MASK: u32 = 1;
/// Alpha blend (drawn in a later pass).
pub const ALPHA_BLEND: u32 = 2;
/// Premultiplied alpha blend.
pub const ALPHA_PREMULTIPLIED: u32 = 3;
/// Additive blend.
pub const ALPHA_ADDITIVE: u32 = 4;

/// GPU vertex for the frame pipelines (position, normal, UV, tangent, colour,
/// skinning influences).
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LabVertex {
    /// Object-space position.
    pub position: [f32; 3],
    /// Object-space normal.
    pub normal: [f32; 3],
    /// Texture coordinates.
    pub uv: [f32; 2],
    /// Tangent xyz and bitangent sign in w.
    pub tangent: [f32; 4],
    /// Linear vertex colour.
    pub color: [f32; 4],
    /// Joint indices.
    pub joints: [u32; 4],
    /// Joint weights.
    pub weights: [f32; 4],
}

impl LabVertex {
    /// Builds a vertex from position/normal/UV with a unit tangent, white
    /// colour and no skinning.
    #[must_use]
    pub fn from_pos_normal_uv(position: [f32; 3], normal: [f32; 3], uv: [f32; 2]) -> Self {
        Self {
            position,
            normal,
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0, 1.0, 1.0, 1.0],
            joints: [0; 4],
            weights: [0.0; 4],
        }
    }

    /// Vertex buffer layout shared by every frame pipeline.
    #[must_use]
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 12,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 24,
                    shader_location: 2,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 32,
                    shader_location: 3,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 48,
                    shader_location: 4,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint32x4,
                    offset: 64,
                    shader_location: 5,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 80,
                    shader_location: 6,
                },
            ],
        }
    }
}

/// RGBA8 image owned by a [`LabMaterial`].
#[derive(Clone, Debug)]
pub struct LabTexture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major RGBA8, as authored: sRGB-encoded for the colour slots
    /// ([`LabMaterial::albedo`], [`LabMaterial::emissive_map`]), linear for the
    /// data slots (normal, occlusion). Do not pre-linearize colour bytes; the
    /// renderer uploads colour slots as `Rgba8UnormSrgb` and the GPU decodes them.
    pub rgba: Vec<u8>,
}

/// Hard-surface factors packed into the material uniform.
///
/// Every strength defaults to off in [`HardSurfaceFactors::OFF`], so a lab material
/// that does not name them shades exactly as before. Authored `.material.ron` files
/// turn multi-scatter and specular occlusion on through [`LabMaterial::from_authored`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HardSurfaceFactors {
    /// Detail albedo blend. `0` skips the detail albedo sample.
    pub detail_albedo_strength: f32,
    /// Detail normal blend (whiteout). `0` skips it.
    pub detail_normal_strength: f32,
    /// Detail roughness blend. `0` skips it.
    pub detail_roughness_strength: f32,
    /// Detail UV scale relative to the parallax-adjusted base UV.
    pub detail_uv_scale: [f32; 2],
    /// Camera distance where the detail weight is still 1.
    pub detail_fade_start: f32,
    /// Camera distance where the detail weight reaches 0. `0` with a start of `0` disables the fade.
    pub detail_fade_end: f32,
    /// Clearcoat lobe weight. `0` skips the second specular lobe.
    pub clearcoat: f32,
    /// Perceptual roughness of the clearcoat lobe.
    pub clearcoat_roughness: f32,
    /// Anisotropy in −1..1. `0` uses the isotropic GGX the rest of the engine uses.
    pub anisotropy: f32,
    /// Rotation of the anisotropy direction, radians, applied to the tangent.
    pub anisotropy_rotation: f32,
    /// Authored specular occlusion. `0` leaves the forward pass's always-on term (the α form).
    /// Above 0.5, the stronger perceptual-roughness form replaces it on the indirect specular
    /// (base, multi-scatter, and clearcoat); the occlusion is applied once either way
    /// ([`crate::hard_surface::indirect_specular_occlusion`]).
    pub specular_occlusion: f32,
    /// Multi-scatter specular. `0` keeps the single-scatter split-sum.
    pub multi_scatter: f32,
    /// Parallax depth in UV units. `0` skips the march. Also requires the profile gate.
    pub pom_scale: f32,
    /// Parallax steps when the view faces the surface.
    pub pom_min_steps: f32,
    /// Parallax steps at grazing angles, before the shader cap of 16.
    pub pom_max_steps: f32,
    /// Edge-wear strength. `0` skips the mask.
    pub edge_intensity: f32,
    /// Added to roughness where the wear mask is 1.
    pub edge_roughness_bias: f32,
    /// Added to metallic where the wear mask is 1.
    pub edge_metallic_bias: f32,
    /// Wear source: `0` off, `1` curvature, `2` ambient occlusion.
    pub edge_source: f32,
    /// Albedo mixed in where the wear mask is 1.
    pub edge_tint: [f32; 3],
    /// Sample value treated as zero wear.
    pub edge_bias: f32,
    /// Contrast applied after [`edge_bias`](Self::edge_bias).
    pub edge_contrast: f32,
}

impl HardSurfaceFactors {
    /// Every feature off. Detail scale is 1 so a later strength does not collapse the UV.
    pub const OFF: Self = Self {
        detail_albedo_strength: 0.0,
        detail_normal_strength: 0.0,
        detail_roughness_strength: 0.0,
        detail_uv_scale: [1.0, 1.0],
        detail_fade_start: 0.0,
        detail_fade_end: 0.0,
        clearcoat: 0.0,
        clearcoat_roughness: 0.0,
        anisotropy: 0.0,
        anisotropy_rotation: 0.0,
        specular_occlusion: 0.0,
        multi_scatter: 0.0,
        pom_scale: 0.0,
        pom_min_steps: 4.0,
        pom_max_steps: 12.0,
        edge_intensity: 0.0,
        edge_roughness_bias: 0.0,
        edge_metallic_bias: 0.0,
        edge_source: 0.0,
        edge_tint: [0.75, 0.76, 0.78],
        edge_bias: 0.5,
        edge_contrast: 4.0,
    };
}

impl Default for HardSurfaceFactors {
    fn default() -> Self {
        Self::OFF
    }
}

/// One metallic-roughness material.
#[derive(Clone, Debug)]
pub struct LabMaterial {
    /// How the material takes the weather (spec 99 § 6.2).
    pub weather: crate::surface_weather::WeatherResponse,
    /// Linear base colour and alpha.
    pub base_color: [f32; 4],
    /// Metallic factor.
    pub metallic: f32,
    /// Perceptual roughness.
    pub roughness: f32,
    /// Normal-map scale. Zero skips the map.
    pub normal_scale: f32,
    /// Occlusion-map strength.
    pub occlusion_strength: f32,
    /// Linear emissive colour.
    pub emissive: [f32; 3],
    /// Linear emissive scale (glTF emissive strength).
    pub emissive_intensity: f32,
    /// [`ALPHA_OPAQUE`], [`ALPHA_MASK`], [`ALPHA_BLEND`], [`ALPHA_PREMULTIPLIED`] or [`ALPHA_ADDITIVE`].
    pub alpha_mode: u32,
    /// Mask cutoff.
    pub alpha_cutoff: f32,
    /// Skip lighting when true.
    pub unlit: bool,
    /// Base-colour texture, sRGB-encoded bytes (decoded by the sampler).
    pub albedo: Option<LabTexture>,
    /// Tangent-space normal map, linear bytes.
    pub normal_map: Option<LabTexture>,
    /// Occlusion texture (R), linear bytes.
    pub occlusion_map: Option<LabTexture>,
    /// Emissive texture, sRGB-encoded bytes (decoded by the sampler).
    pub emissive_map: Option<LabTexture>,
    /// Detail, clearcoat, anisotropy, occlusion, multi-scatter, parallax, and edge wear.
    pub hard_surface: HardSurfaceFactors,
    /// Detail albedo. RGB is centred on 0.5; packed with roughness at upload.
    pub detail_albedo: Option<LabTexture>,
    /// Detail normal (tangent space). Packed with the mask at upload.
    pub detail_normal: Option<LabTexture>,
    /// Detail roughness in R. Packed into the detail albedo alpha at upload.
    pub detail_roughness: Option<LabTexture>,
    /// Detail blend mask in R. Packed into the detail normal alpha at upload.
    pub detail_mask: Option<LabTexture>,
    /// Parallax height in R. 255 is the geometric surface (no offset).
    pub height_map: Option<LabTexture>,
    /// Curvature in R. 128 is flat; brighter is convex.
    pub curvature_map: Option<LabTexture>,
    /// Material-graph surface function ([`crate::surface_graph`]). When set it replaces the
    /// surface stage and its own textures replace the slots above; the factors that drive
    /// lighting (clearcoat, anisotropy, specular occlusion, multi-scatter, alpha mode, unlit)
    /// still come from this struct.
    pub surface_graph: Option<std::sync::Arc<crate::SurfaceGraph>>,
    /// [`Self::occlusion_map`] is a packed ORM image that also carries the glTF
    /// metallic-roughness channels: G scales [`Self::roughness`] and B scales
    /// [`Self::metallic`] (glTF 2.0 § 3.9.2, `metallicRoughnessTexture`), R stays occlusion.
    /// The products are clamped to `0..=1`. `false` keeps the scalar factors.
    pub orm_packed: bool,
    /// `KHR_materials_transmission`: the fraction of light that passes through the
    /// surface (`0..=1`). Above 0 the material draws as a thin dielectric over what is
    /// behind it: the transmitted share replaces the diffuse lobe and the specular
    /// reflection stays, weighted by Fresnel, so glass reads clear head-on and mirror-like
    /// at grazing angles. Use [`ALPHA_PREMULTIPLIED`] so the frame blends it.
    pub transmission: f32,
    /// `KHR_materials_ior`: index of refraction of a dielectric. Sets the normal-incidence
    /// reflectance `((ior − 1) / (ior + 1))²`; 1.5 gives the glTF default 0.04.
    pub ior: f32,
    /// `KHR_materials_volume`: the fraction of each channel that survives the volume's
    /// thickness at normal incidence (`attenuation_color^(thickness / attenuation_distance)`,
    /// Beer–Lambert; [`volume_transmittance`]). The path lengthens toward grazing views
    /// (`1 / N·V`). White is a thin wall.
    pub transmission_tint: [f32; 3],
    /// `KHR_materials_volume` `thicknessFactor`: the volume's thickness in the mesh's units.
    /// It drives the screen-space refraction offset where that is on
    /// ([`crate::FrameQuality::screen_space_refraction`]); `0` is a thin wall, which bends
    /// nothing.
    pub transmission_thickness: f32,
    /// Draw both faces (glTF `doubleSided`, `.material.ron` `cull_mode: none`): no back-face
    /// culling for this material's draws, shadows and motion vectors, including a material
    /// graph. The forward shader (and a graph's `world_normal`) already lights a visible back
    /// face with the flipped normal. [`LabScene::double_sided`] still turns culling off for the
    /// whole frame. A `.matgraph` does not store this flag: set it on the [`LabMaterial`].
    pub double_sided: bool,
    /// Pre-compressed (BCn) maps that replace the RGBA8 slots above on devices with
    /// `TEXTURE_COMPRESSION_BC` ([`crate::compressed_texture`]). Shared, not copied, by clones.
    pub compressed: Option<std::sync::Arc<crate::compressed_texture::CompressedMaps>>,
    /// Per-slot images keyed by their own identity ([`crate::SharedImage`], [`crate::DynamicTexture`]). A `Some`
    /// slot replaces streaming, [`Self::compressed`] and the RGBA8 map for that slot. `None`
    /// keeps today's upload. A [`Self::surface_graph`] does not read this; put the image on
    /// the graph texture instead.
    pub images: crate::MaterialImages,
    /// Hair card shading (B5, [`crate::hair`]). [`crate::hair::HairFactors::OFF`] by default: the material
    /// shades as it did without the feature.
    pub hair: crate::HairFactors,
    /// Per-vertex ambient occlusion (ADR-1512, [`crate::vertex_ao`]). When true on an opaque
    /// material, [`LabVertex::color`]`[3]` is a 0..1 factor on the indirect light (sky, probes,
    /// baked GI and the reflections that read them), not an opacity; direct sun and local lights
    /// are untouched. False by default: the material shades, bit for bit, as it did without the
    /// feature. Ignored on a masked, blended, additive or unlit material.
    pub vertex_ao: bool,
    /// Thin-film interference over the base specular (`KHR_materials_iridescence`,
    /// [`crate::thin_film`]). [`crate::ThinFilmFactors::OFF`] by default: the material draws
    /// through the standard pipelines, exactly as without the feature.
    pub thin_film: crate::ThinFilmFactors,
    /// The film's coverage (R) and thickness (G), linear bytes
    /// ([`crate::pack_thin_film_map`]). `None` is full coverage at the maximum thickness.
    pub thin_film_map: Option<LabTexture>,
    /// The shared sampler each texture slot reads, on the texture table path (ADR-2071). The
    /// default is the sampler of every earlier release.
    pub samplers: crate::texture_table::MaterialSamplers,
    /// Screen-space subsurface scattering of the diffuse light ([`crate::subsurface`]).
    /// [`crate::SubsurfaceFactors::OFF`] by default: the material draws through the standard
    /// pipelines, exactly as without the feature. It also draws so on a frame whose render profile
    /// does not ask for scattering ([`crate::RenderProfile::subsurface_scattering`]).
    pub subsurface: crate::SubsurfaceFactors,
    /// Rough transmission (ADR-2075, [`crate::rough_transmission`]): the perceptual roughness,
    /// `0..=1`, that spreads what a transmissive surface shows behind it (frosted glass). `0`
    /// (the default) is clear glass, drawn exactly as without the feature. It takes effect where
    /// the frame refracts ([`crate::FrameQuality::screen_space_refraction`]) on a material with
    /// [`Self::transmission`] above 0 and no hair.
    pub transmission_roughness: f32,
}

/// Beer–Lambert transmittance of a `KHR_materials_volume` at normal incidence:
/// `attenuation_color^(thickness / attenuation_distance)` per channel. A zero thickness or a
/// non-positive (infinite) distance absorbs nothing.
#[must_use]
pub fn volume_transmittance(
    thickness: f32,
    attenuation_color: [f32; 3],
    attenuation_distance: f32,
) -> [f32; 3] {
    if !(thickness > 0.0 && attenuation_distance > 0.0 && attenuation_distance.is_finite()) {
        return [1.0; 3];
    }
    let k = thickness / attenuation_distance;
    attenuation_color.map(|c| c.clamp(0.0, 1.0).powf(k))
}

impl LabMaterial {
    /// The default material as an associated const (usable in `static`s).
    pub const EMPTY: Self = Self {
        weather: crate::surface_weather::WeatherResponse::DEFAULT,
        base_color: [0.8, 0.8, 0.8, 1.0],
        metallic: 0.0,
        roughness: 0.5,
        normal_scale: 0.0,
        occlusion_strength: 1.0,
        emissive: [0.0; 3],
        emissive_intensity: 1.0,
        alpha_mode: ALPHA_OPAQUE,
        alpha_cutoff: 0.5,
        unlit: false,
        albedo: None,
        normal_map: None,
        occlusion_map: None,
        emissive_map: None,
        hard_surface: HardSurfaceFactors::OFF,
        detail_albedo: None,
        detail_normal: None,
        detail_roughness: None,
        detail_mask: None,
        height_map: None,
        curvature_map: None,
        surface_graph: None,
        orm_packed: false,
        transmission: 0.0,
        ior: 1.5,
        transmission_tint: [1.0; 3],
        transmission_thickness: 0.0,
        double_sided: false,
        compressed: None,
        images: crate::texture_residency::MaterialImages::EMPTY,
        hair: crate::HairFactors::OFF,
        vertex_ao: false,
        thin_film: crate::ThinFilmFactors::OFF,
        thin_film_map: None,
        samplers: crate::texture_table::MaterialSamplers::DEFAULT,
        subsurface: crate::SubsurfaceFactors::OFF,
        transmission_roughness: 0.0,
    };
}

impl Default for LabMaterial {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Point light in lumens with a finite radius.
#[derive(Clone, Copy, Debug)]
pub struct LabPointLight {
    /// World position.
    pub position: [f32; 3],
    /// Linear RGB.
    pub color: [f32; 3],
    /// Luminous power in lumens (converted to candela for the shader: lm / 4π,
    /// `KHR_lights_punctual` § 5.3).
    pub intensity: f32,
    /// Influence radius.
    pub radius: f32,
    /// Render this light into the shadow atlas.
    pub cast_shadows: bool,
    /// Emitter radius in metres from a [`crate::LightSourceSize`]. `None` keeps the defaults:
    /// a 45 cm shadow source ([`crate::shadow::LOCAL_SHADOW_SOURCE_RADIUS_M`]) and a punctual
    /// highlight (see [`crate::light_source`]).
    pub source_radius: Option<f32>,
}

/// Spot light in candela.
#[derive(Clone, Copy, Debug)]
pub struct LabSpotLight {
    /// World position.
    pub position: [f32; 3],
    /// Direction from the light into the scene (normalized).
    pub direction: [f32; 3],
    /// Linear RGB.
    pub color: [f32; 3],
    /// Luminous intensity.
    pub intensity: f32,
    /// Range.
    pub range: f32,
    /// Inner cone half-angle in radians.
    pub inner_angle: f32,
    /// Outer cone half-angle in radians.
    pub outer_angle: f32,
    /// Render this light into the shadow atlas.
    pub cast_shadows: bool,
    /// Emitter radius in metres from a [`crate::LightSourceSize`]. `None` keeps the defaults,
    /// as for [`LabPointLight::source_radius`].
    pub source_radius: Option<f32>,
}

/// Local light stored in the cluster buffer.
#[derive(Clone, Copy, Debug)]
pub enum LabLocalLight {
    /// Point light.
    Point(LabPointLight),
    /// Spot light.
    Spot(LabSpotLight),
}

/// Indexed mesh plus a material index into [`LabScene::materials`].
#[derive(Clone, Debug)]
pub struct LabMesh {
    /// Vertices.
    pub vertices: Vec<LabVertex>,
    /// Triangle indices.
    pub indices: Vec<u32>,
    /// Index into the scene material list.
    pub material: usize,
    /// Column-major object-to-world matrix.
    ///
    /// Skinned meshes whose palette is already in world space use identity here.
    pub model: [[f32; 4]; 4],
    /// Joint palette. Empty means the mesh is rigid; vertex weights must then be zero.
    pub joints: Vec<JointMatrix>,
}

impl LabMesh {
    /// Empty rigid mesh with an identity model and no joints.
    #[must_use]
    pub fn unskinned() -> Self {
        Self {
            vertices: Vec::new(),
            indices: Vec::new(),
            material: 0,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: Vec::new(),
        }
    }
}

/// One golden / lab frame.
#[derive(Clone, Debug)]
pub struct LabScene {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Camera eye.
    pub eye: [f32; 3],
    /// Camera look-at point.
    pub target: [f32; 3],
    /// Vertical field of view in radians.
    pub fov_y: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
    /// Run the depth prepass before the colour pass.
    pub depth_prepass: bool,
    /// Cascaded shadow map for the directional light.
    pub directional_shadows: bool,
    /// Image-based lighting from the procedural sky. Off leaves the flat ambient term.
    pub ibl: bool,
    /// Fixed exposure in EV100 applied once before the tonemap when `post` is off
    /// (`specs/13` § 6; the post stack applies its own `PostSettings::exposure_ev`).
    pub exposure_ev: f32,
    /// Flat ambient illuminance in lux (used only when `ibl` is off; the IBL path
    /// derives its ambient from the sky). Outdoor daylight ≈ 10,000–15,000.
    pub ambient_lux: f32,
    /// Render both triangle faces (glTF models from mixed-winding assets): no
    /// back-face culling, and the fragment shader flips the normal on back
    /// faces so both sides shade correctly (the glTF doubleSided recipe,
    /// applied per scene because some converted assets — measured on the
    /// Khronos Sponza glTF — mix winding conventions between meshes).
    pub double_sided: bool,
    /// Post stack. `None` tonemaps in the material shader.
    pub post: Option<crate::PostSettings>,
    /// Directional light.
    pub directional: crate::DirectionalLightUniform,
    /// Point and spot lights.
    pub local_lights: Vec<LabLocalLight>,
    /// Materials.
    pub materials: Vec<LabMaterial>,
    /// Meshes.
    pub meshes: Vec<LabMesh>,
}

/// FNV-1a over `bytes` into `hash` (Fowler–Noll–Vo, public-domain parameters; the same
/// construction as the VFX editor's simulation fingerprint, `regia_editor::vfx_editor::sim`).
/// Shared by the lab slot keys ([`material_texture_key`]) and the emissive fit signature
/// ([`crate::emissive_lights::material_signature`]).
pub(crate) fn mix_bytes(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

/// Payloads at or under this many bytes stay on [`mix_bytes`]. Larger ones are word-hashed
/// once ([`crate::key_hash::KeyHasher`]) and reused while the buffer's address, length and
/// sample stay put.
const PAYLOAD_MEMO_MIN: usize = 4096;

/// Cap on the per-thread lab key memos. Past it the memo drops and the next call hashes again.
const PAYLOAD_MEMO_CAP: usize = 4096;

/// Samples spread over a byte slice, first and last included. An edit of a byte the sample
/// misses keeps a memoized key until the buffer moves or changes length: the same class of
/// hole as the emissive content fingerprint.
fn sample_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    const SAMPLES: usize = 32;
    let mut hasher = crate::key_hash::KeyHasher::default();
    let Some(last) = bytes.len().checked_sub(1) else {
        return 0;
    };
    for s in 0..SAMPLES {
        hasher.write_u8(bytes[s * last / (SAMPLES - 1)]);
    }
    hasher.finish()
}

fn word_hash(bytes: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = crate::key_hash::KeyHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

struct PayloadMemo {
    ptr: usize,
    len: usize,
    sample: u64,
    hash: u64,
}

struct MeshMemo {
    slot: usize,
    vert_ptr: usize,
    vert_len: usize,
    index_ptr: usize,
    index_len: usize,
    sample: u64,
    key: u64,
}

thread_local! {
    static TEXTURE_MEMOS: std::cell::RefCell<Vec<PayloadMemo>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static MESH_MEMOS: std::cell::RefCell<Vec<MeshMemo>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
thread_local! {
    static TEXTURE_PAYLOAD_WALKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times this thread has walked a lab texture's pixels end to end.
#[cfg(test)]
pub(crate) fn texture_payload_walks() -> u64 {
    TEXTURE_PAYLOAD_WALKS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn note_texture_payload_walk(bytes: usize) {
    if bytes > 4096 {
        TEXTURE_PAYLOAD_WALKS.with(|walks| walks.set(walks.get().saturating_add(1)));
    }
}

/// wyhash's folded multiply (Wang Yi, public domain): the 128-bit product's halves xored.
#[inline]
fn fold(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    (r as u64) ^ ((r >> 64) as u64)
}

/// Odd 64-bit lane seeds. The first three are SplitMix64's (Steele, Lea & Flood 2014): its
/// increment `0x9e3779b97f4a7c15` (2^64 divided by the golden ratio) and its two finalizer
/// multipliers. The fourth, `0xd6e8feb86659fd93`, is an odd constant with no cited source:
/// any odd constant with well-mixed bits serves, and `mix_bytes_wide_matches_its_known_answers`
/// pins the four values.
const WIDE_LANES: [u64; 4] = [
    0x9e37_79b9_7f4a_7c15,
    0xbf58_476d_1ce4_e5b9,
    0x94d0_49bb_1331_11eb,
    0xd6e8_feb8_6659_fd93,
];

/// Four-lane folded-multiply mix of `bytes` into `hash`: `u64::from_le_bytes` chunks (endian
/// stable), chunk `i` feeding lane `i mod 4` so the multiplies overlap, then the lanes and the
/// tail folded together. For buffers mixed per frame (an emissive map's pixels): the serial
/// per-byte chain of [`mix_bytes`] costs a dependent multiply per byte, this about one
/// multiply per eight bytes.
pub(crate) fn mix_bytes_wide(hash: &mut u64, bytes: &[u8]) {
    let mut lanes = [*hash, WIDE_LANES[1], WIDE_LANES[2], WIDE_LANES[3]];
    let (chunks, rest) = bytes.as_chunks::<8>();
    for (i, c) in chunks.iter().enumerate() {
        let l = &mut lanes[i & 3];
        *l = fold(*l ^ u64::from_le_bytes(*c), WIDE_LANES[(i + 1) & 3]);
    }
    let mut tail = [0u8; 8];
    tail[..rest.len()].copy_from_slice(rest);
    lanes[0] = fold(
        lanes[0] ^ u64::from_le_bytes(tail),
        WIDE_LANES[0] ^ (bytes.len() as u64),
    );
    *hash = fold(lanes[0] ^ WIDE_LANES[2], lanes[1] ^ WIDE_LANES[3])
        ^ fold(lanes[2] ^ WIDE_LANES[0], lanes[3] ^ WIDE_LANES[1]);
}

fn mix_texture(hash: &mut u64, texture: &Option<LabTexture>) {
    match texture {
        Some(texture) => {
            mix_bytes(hash, &texture.width.to_le_bytes());
            mix_bytes(hash, &texture.height.to_le_bytes());
            if texture.rgba.len() > PAYLOAD_MEMO_MIN {
                let payload = memoized_texture_hash(texture);
                mix_bytes(hash, &payload.to_le_bytes());
            } else {
                mix_bytes(hash, &texture.rgba);
            }
        }
        None => mix_bytes(hash, &[0]),
    }
}

/// Word hash of a large texture's pixels, cached on `(address, length, sample)`.
///
/// The sample includes the first byte, so an edit there misses. An edit of a byte the sample
/// skips keeps this hash until the allocation moves: same hole as the emissive fingerprint.
fn memoized_texture_hash(texture: &LabTexture) -> u64 {
    let ptr = texture.rgba.as_ptr() as usize;
    let len = texture.rgba.len();
    let sample = sample_bytes(&texture.rgba);
    let cached = TEXTURE_MEMOS.with(|memos| {
        memos
            .borrow()
            .iter()
            .find(|memo| memo.ptr == ptr && memo.len == len && memo.sample == sample)
            .map(|memo| memo.hash)
    });
    if let Some(hash) = cached {
        return hash;
    }
    #[cfg(test)]
    note_texture_payload_walk(len);
    let hash = word_hash(&texture.rgba);
    TEXTURE_MEMOS.with(|memos| {
        let mut memos = memos.borrow_mut();
        if memos.len() >= PAYLOAD_MEMO_CAP {
            memos.clear();
        }
        memos.push(PayloadMemo {
            ptr,
            len,
            sample,
            hash,
        });
    });
    hash
}

/// Murmur3's 64-bit finaliser: every input bit reaches every output bit, so a content
/// key built from a word hash can be packed into fewer bits without clustering.
fn fmix64(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// Bits of a lab key that hold the content hash; the slot (index in the scene, plus one)
/// sits above them.
const LAB_KEY_HASH_BITS: u32 = 40;

/// Tag mixed into material keys so a material and a mesh at the same slot with colliding
/// hashes still get different keys.
const LAB_MATERIAL_TAG: u64 = 0x6d61_7465_7269_616c;

/// Packs a slot and a content hash into a lab cache key.
///
/// The slot keeps two entries of one scene apart even when their contents are equal (two
/// copies of a mesh are two objects, and two materials must not share a uniform buffer).
/// The top byte stays far below [`crate::EPHEMERAL_KEY`]'s and the key is never 0.
fn lab_key(slot: usize, hash: u64) -> u64 {
    let slot = (slot as u64 + 1) & ((1 << (64 - LAB_KEY_HASH_BITS - 1)) - 1);
    (slot << LAB_KEY_HASH_BITS) | (fmix64(hash) & ((1 << LAB_KEY_HASH_BITS) - 1))
}

/// Cache identity of the lab mesh at `slot` in [`LabScene::meshes`]: the slot and a hash of
/// its vertex and index bytes.
///
/// Rebuilding the same scene (a caller that makes a new [`LabScene`] every frame) gives the
/// same keys, so the renderer's caches keyed by object (the paged shadow cache, previous
/// model matrices for motion vectors) see the same object; they used to see a new one
/// whenever the allocator moved the mesh. Changed geometry at a slot gets a new key and a
/// fresh upload, where an address key reused at the same byte size kept stale buffers.
/// The model matrix and the joint palette are object state and stay out of the key.
///
/// A mesh whose vertex and index bytes together pass [`PAYLOAD_MEMO_MIN`] reuses that key
/// while the slices' addresses, lengths and a 32-byte sample stay put. The stored key is the
/// same [`crate::key_hash::KeyHasher`] result as a full walk. A smaller mesh is always walked,
/// so a one-triangle edit cannot hide in the sample. A large mesh can: a byte the sample
/// misses keeps the old key until the allocation moves.
fn lab_mesh_key(slot: usize, mesh: &LabMesh) -> u64 {
    let vertices: &[u8] = bytemuck::cast_slice(&mesh.vertices);
    let indices: &[u8] = bytemuck::cast_slice(&mesh.indices);
    if vertices.len().saturating_add(indices.len()) > PAYLOAD_MEMO_MIN {
        let vert_ptr = mesh.vertices.as_ptr() as usize;
        let index_ptr = mesh.indices.as_ptr() as usize;
        let sample = sample_bytes(vertices) ^ sample_bytes(indices).rotate_left(1);
        let cached = MESH_MEMOS.with(|memos| {
            memos
                .borrow()
                .iter()
                .find(|memo| {
                    memo.slot == slot
                        && memo.vert_ptr == vert_ptr
                        && memo.vert_len == mesh.vertices.len()
                        && memo.index_ptr == index_ptr
                        && memo.index_len == mesh.indices.len()
                        && memo.sample == sample
                })
                .map(|memo| memo.key)
        });
        if let Some(key) = cached {
            return key;
        }
        let key = mesh_bytes_key(slot, mesh);
        MESH_MEMOS.with(|memos| {
            let mut memos = memos.borrow_mut();
            if memos.len() >= PAYLOAD_MEMO_CAP {
                memos.clear();
            }
            memos.push(MeshMemo {
                slot,
                vert_ptr,
                vert_len: mesh.vertices.len(),
                index_ptr,
                index_len: mesh.indices.len(),
                sample,
                key,
            });
        });
        return key;
    }
    mesh_bytes_key(slot, mesh)
}

fn mesh_bytes_key(slot: usize, mesh: &LabMesh) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = crate::key_hash::KeyHasher::default();
    hasher.write_u64(mesh.vertices.len() as u64);
    hasher.write(bytemuck::cast_slice(&mesh.vertices));
    hasher.write_u64(mesh.indices.len() as u64);
    hasher.write(bytemuck::cast_slice(&mesh.indices));
    lab_key(slot, hasher.finish())
}

/// Cache identity of the lab material at `slot` in [`LabScene::materials`].
///
/// The slot keeps two materials in one submit on separate uniform buffers. A queue write
/// is visible to every draw, so sharing a buffer would let the last writer repaint the
/// earlier meshes. Scalar edits do not change the key: [`crate::FrameRenderer`] rewrites
/// that uniform in place.
///
/// Texture bytes are mixed in as well: a slot whose pixels change must miss the bind
/// group. A map larger than [`PAYLOAD_MEMO_MIN`] bytes contributes a word hash
/// ([`crate::key_hash::KeyHasher`]) cached on the buffer's address, so a still scene does
/// not walk those pixels again. That changes the numeric key of a large map compared with
/// the serial mix; the pixels drawn do not change. A byte the sample misses keeps the key
/// until the allocation moves. The runtime `Arc<CpuMaterial>` path does not use this function.
fn material_texture_key(slot: usize, material: &LabMaterial) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    mix_bytes(&mut hash, &LAB_MATERIAL_TAG.to_le_bytes());
    mix_texture(&mut hash, &material.albedo);
    mix_texture(&mut hash, &material.normal_map);
    mix_texture(&mut hash, &material.occlusion_map);
    mix_texture(&mut hash, &material.emissive_map);
    mix_texture(&mut hash, &material.detail_albedo);
    mix_texture(&mut hash, &material.detail_normal);
    mix_texture(&mut hash, &material.detail_roughness);
    mix_texture(&mut hash, &material.detail_mask);
    mix_texture(&mut hash, &material.height_map);
    mix_texture(&mut hash, &material.curvature_map);
    // Only a set map changes the key, so every material without one keeps its bind group.
    if material.thin_film_map.is_some() {
        mix_texture(&mut hash, &material.thin_film_map);
    }
    if let Some(maps) = &material.compressed {
        mix_bytes(
            &mut hash,
            &(std::sync::Arc::as_ptr(maps) as usize as u64).to_le_bytes(),
        );
    }
    // Only a set image changes the key. Mixing a constant for an empty slot would move every
    // existing material onto a new bind group. The pointer is stable across dynamic writes, so
    // those writes hit the in-place path instead of a new group.
    mix_material_image(&mut hash, &material.images.albedo);
    mix_material_image(&mut hash, &material.images.normal);
    mix_material_image(&mut hash, &material.images.occlusion);
    mix_material_image(&mut hash, &material.images.emissive);
    mix_material_image(&mut hash, &material.images.detail);
    mix_material_image(&mut hash, &material.images.detail_normal);
    mix_material_image(&mut hash, &material.images.aux);
    if let Some(graph) = &material.surface_graph {
        for slot in &graph.textures {
            let Some(tex) = slot else { continue };
            if let Some(shared) = &tex.shared {
                mix_image_identity(&mut hash, shared.identity(), shared.generation());
            }
            if let Some(dynamic) = &tex.dynamic {
                mix_image_identity(&mut hash, dynamic.identity(), dynamic.generation());
            }
            // A counted texture: its identity and upload serial, as in a standard slot (ADR-8610).
            if let Some(asset) = &tex.asset {
                mix_image_identity(&mut hash, asset.identity(), asset.serial().unwrap_or(0));
            }
            if let Some(compressed) = &tex.compressed {
                mix_bytes(
                    &mut hash,
                    &(std::sync::Arc::as_ptr(compressed) as usize as u64).to_le_bytes(),
                );
            }
            // `bind_graph_texture` uploads `image` when no shared or dynamic image wins the
            // slot, and it uses `srgb` for that upload and for a compressed fallback. A pixel
            // or encoding change must miss the bind group. Shared and dynamic slots keep an
            // empty `image`, so mixing those bytes stays stable across in-place writes.
            mix_bytes(&mut hash, &tex.image.width.to_le_bytes());
            mix_bytes(&mut hash, &tex.image.height.to_le_bytes());
            mix_bytes(&mut hash, &tex.image.rgba);
            mix_bytes(&mut hash, &[u8::from(tex.srgb)]);
        }
    }
    lab_key(slot, hash)
}

/// Address plus creation generation. The generation is what separates an image from a later
/// one that the allocator placed at the same address (ADR-179). The material entry also holds
/// the `Arc`, so the address is not free while the entry exists.
fn mix_image_identity(hash: &mut u64, identity: usize, generation: u64) {
    mix_bytes(hash, &(identity as u64).to_le_bytes());
    mix_bytes(hash, &generation.to_le_bytes());
}

fn mix_material_image(hash: &mut u64, image: &Option<crate::texture_residency::MaterialImage>) {
    match image {
        Some(crate::texture_residency::MaterialImage::Shared(image)) => {
            mix_image_identity(hash, image.identity(), image.generation());
        }
        Some(crate::texture_residency::MaterialImage::Dynamic(image)) => {
            mix_image_identity(hash, image.identity(), image.generation());
        }
        // The upload serial: a reload (or the first upload after a load) binds the new texture.
        Some(crate::texture_residency::MaterialImage::Asset(image)) => {
            mix_image_identity(hash, image.identity(), image.serial().unwrap_or(0));
        }
        None => {}
    }
}

impl LabScene {
    /// The [`FrameMesh::object_key`] [`Self::as_frame_scene`] gives the mesh at `slot` (`None`
    /// past the end): what per-object renderer state such as
    /// [`crate::FrameRenderer::set_glass`] matches draws by.
    #[must_use]
    pub fn object_key(&self, slot: usize) -> Option<u64> {
        self.meshes.get(slot).map(|mesh| lab_mesh_key(slot, mesh))
    }

    /// Borrows this scene into the renderer's frame input.
    ///
    /// Keys come from each entry's slot and content, never its address: the
    /// same scene rebuilt every frame keeps its keys, so the renderer's caches
    /// see the same objects. The mesh and object key is the mesh's slot plus a
    /// hash of its vertex and index bytes. The material key is the material's
    /// slot plus its texture bytes. Colour and the other scalars stay out of the
    /// key so an in-place edit rewrites one uniform. Two materials still cannot
    /// share the buffer, and a slot with new pixels or new geometry does not keep
    /// the old bind group or buffers.
    ///
    /// Meshes whose material index is out of range are skipped (warned once);
    /// the one-shot path used to hard-error, which a runtime frame cannot do.
    #[must_use]
    pub fn as_frame_scene(&self) -> FrameScene<'_> {
        let _span = tracing::info_span!("as_frame_scene", meshes = self.meshes.len()).entered();
        let meshes = self
            .meshes
            .iter()
            .enumerate()
            .filter_map(|(slot, mesh)| {
                let material = self.materials.get(mesh.material).or_else(|| {
                    tracing::warn!(
                        target: "regia_render",
                        "mesh references material {} of {}; skipping",
                        mesh.material,
                        self.materials.len()
                    );
                    None
                })?;
                let key = lab_mesh_key(slot, mesh);
                Some(FrameMesh {
                    is_static: false,
                    mesh_key: key,
                    material_key: material_texture_key(mesh.material, material),
                    object_key: key,
                    vertices: &mesh.vertices,
                    indices: &mesh.indices,
                    material,
                    model: mesh.model,
                    joints: &mesh.joints,
                    receiver_layer: 0,
                    shadow: crate::ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                })
            })
            .collect::<Vec<_>>();
        FrameScene {
            width: self.width,
            height: self.height,
            eye: self.eye,
            target: self.target,
            fov_y: self.fov_y,
            near: self.near,
            far: self.far,
            depth_prepass: self.depth_prepass,
            directional_shadows: self.directional_shadows,
            ibl: self.ibl,
            exposure_ev: self.exposure_ev,
            ambient_lux: self.ambient_lux,
            double_sided: self.double_sided,
            post: self.post.as_ref(),
            fog: crate::FogSettings::default(),
            directional: self.directional,
            local_lights: &self.local_lights,
            quality: crate::profile::FrameQuality::LAB,
            meshes,
        }
    }

    /// Same as [`Self::as_frame_scene`] with an explicit [`crate::FrameQuality`].
    ///
    /// Golden tests use [`crate::FrameQuality::LAB`]. The GPU-driven comparison
    /// passes a copy with `gpu_driven` set.
    #[must_use]
    pub fn as_frame_scene_with_quality(&self, quality: crate::profile::FrameQuality) -> FrameScene<'_> {
        let mut scene = self.as_frame_scene();
        scene.quality = quality;
        scene
    }
}

/// Renders `scene` offscreen and returns tightly packed RGBA8 pixels.
///
/// Thin wrapper over [`FrameRenderer`]: a fresh renderer per call, so every
/// upload and pipeline is (re)built exactly once. Callers that render many
/// frames should hold a [`FrameRenderer`] and call
/// [`FrameRenderer::render_offscreen`] with [`LabScene::as_frame_scene`].
#[tracing::instrument(skip_all)]
pub fn render_lab_scene(device: &RenderDevice, scene: &LabScene) -> Result<ForwardResult, RenderError> {
    let mut renderer = FrameRenderer::new();
    renderer.render_offscreen(device, &scene.as_frame_scene())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_cluster_cap_matches_the_golden_baseline() {
        assert_eq!(
            LAB_MAX_PER_CLUSTER,
            crate::profile::FrameQuality::LAB.max_lights_per_cluster
        );
    }

    #[test]
    fn lab_vertex_layout_matches_gpu_struct() {
        let layout = LabVertex::layout();
        assert_eq!(layout.array_stride as usize, size_of::<LabVertex>());
        assert_eq!(layout.attributes.len(), 7);
    }

    fn one_mesh_scene() -> LabScene {
        LabScene {
            width: 8,
            height: 8,
            eye: [0.0, 0.0, 5.0],
            target: [0.0; 3],
            fov_y: 1.0,
            near: 0.1,
            far: 50.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 15.0,
            ambient_lux: 10_000.0,
            double_sided: false,
            post: None,
            directional: crate::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 100_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial::default()],
            meshes: vec![LabMesh::unskinned()],
        }
    }

    fn triangle(x: f32) -> LabMesh {
        let n = [0.0, 0.0, 1.0];
        LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([x, 0.0, 0.0], n, [0.0, 0.0]),
                LabVertex::from_pos_normal_uv([x + 1.0, 0.0, 0.0], n, [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([x, 1.0, 0.0], n, [0.0, 1.0]),
            ],
            indices: vec![0, 1, 2],
            ..LabMesh::unskinned()
        }
    }

    #[test]
    fn frame_scene_borrows_all_meshes() {
        let scene = one_mesh_scene();
        let frame = scene.as_frame_scene();
        assert_eq!(frame.meshes.len(), 1);
        // Distinct meshes get distinct keys even when they share a material.
        let a = frame.meshes[0].mesh_key;
        let b = frame.meshes[0].object_key;
        assert_eq!(a, b, "one object per LabScene mesh");
        assert_ne!(frame.meshes[0].material_key, 0);
    }

    /// A caller that builds a new scene every frame must keep its keys: the paged shadow
    /// cache and the motion-vector history are keyed by object, and an address key changed
    /// whenever the allocator moved a mesh (`scatter_lod_pops` passed or failed with it).
    #[test]
    fn rebuilt_scene_keeps_its_keys_wherever_the_allocator_puts_it() {
        let build = || {
            let mut scene = one_mesh_scene();
            scene.meshes = vec![triangle(0.0), triangle(5.0)];
            scene.materials.push(LabMaterial {
                base_color: [0.5, 0.4, 0.3, 1.0],
                ..LabMaterial::default()
            });
            scene.meshes[1].material = 1;
            scene
        };
        let keys = |scene: &LabScene| {
            scene
                .as_frame_scene()
                .meshes
                .iter()
                .map(|m| (m.mesh_key, m.material_key, m.object_key))
                .collect::<Vec<_>>()
        };
        let first = build();
        let expected = keys(&first);
        // Hold the first scene so the second cannot reuse its allocations.
        let mut second = build();
        assert_eq!(keys(&second), expected, "same content, same keys");
        drop(first);
        // A moved model or a new joint palette is object state, not identity.
        second.meshes[0].model[3][0] = 4.0;
        second.materials[1].roughness = 0.2;
        assert_eq!(keys(&second), expected, "transforms and scalars keep the keys");
        for (mesh, material, object) in &expected {
            assert_ne!(*mesh, 0);
            assert_ne!(*material, 0);
            assert_ne!(*object, 0);
            assert_ne!(mesh & 0xFF00_0000_0000_0000, crate::EPHEMERAL_KEY);
        }
    }

    /// Two `as_frame_scene` calls on one scene that owns a multi-megabyte texture keep the
    /// material key and walk those pixels once. A later call hashes again only when the
    /// bytes change.
    #[test]
    fn one_scene_walks_a_large_texture_once() {
        let pixels = 512 * 512 * 4;
        let mut rgba = vec![0u8; pixels];
        rgba[0] = 7;
        rgba[pixels - 1] = 9;
        let mut scene = one_mesh_scene();
        scene.materials[0].albedo = Some(LabTexture {
            width: 512,
            height: 512,
            rgba,
        });
        let before = texture_payload_walks();
        let first = scene.as_frame_scene().meshes[0].material_key;
        let second = scene.as_frame_scene().meshes[0].material_key;
        let walks = texture_payload_walks() - before;
        assert_eq!(first, second, "the same scene must keep its material key");
        assert_eq!(
            walks, 1,
            "two calls walked the texture {walks} times (one full walk)"
        );
        scene.materials[0].albedo.as_mut().expect("albedo").rgba[0] = 8;
        let changed = scene.as_frame_scene().meshes[0].material_key;
        assert_ne!(changed, first, "a changed pixel must miss the bind group");
    }

    #[test]
    fn equal_meshes_stay_apart_and_new_geometry_gets_a_new_key() {
        let mut scene = one_mesh_scene();
        scene.meshes = vec![triangle(0.0), triangle(0.0)];
        let frame = scene.as_frame_scene();
        assert_ne!(
            frame.meshes[0].object_key, frame.meshes[1].object_key,
            "two copies are two objects"
        );
        let before = frame.meshes[0].mesh_key;
        drop(frame);
        scene.meshes[0] = triangle(2.0);
        assert_ne!(
            scene.as_frame_scene().meshes[0].mesh_key,
            before,
            "same byte size, new vertices: the buffers must be re-uploaded"
        );
        scene.meshes[0].indices = vec![0, 2, 1];
        let swapped = scene.as_frame_scene().meshes[0].mesh_key;
        scene.meshes[0].indices = vec![0, 1, 2];
        assert_ne!(
            scene.as_frame_scene().meshes[0].mesh_key,
            swapped,
            "indices are content"
        );
    }

    #[test]
    fn material_key_stays_put_when_scalars_change_and_splits_allocations() {
        let mut red = LabMaterial {
            base_color: [0.9, 0.12, 0.08, 1.0],
            ..LabMaterial::default()
        };
        let before = material_texture_key(0, &red);
        red.base_color = [0.1, 0.2, 0.3, 1.0];
        red.roughness = 0.15;
        red.hard_surface.clearcoat = 1.0;
        assert_eq!(
            before,
            material_texture_key(0, &red),
            "scalars rewrite the uniform in place"
        );
        let blend = LabMaterial {
            alpha_mode: ALPHA_BLEND,
            ..red.clone()
        };
        assert_eq!(
            material_texture_key(0, &red),
            material_texture_key(0, &blend),
            "the alpha mode is a scalar too"
        );
        assert_ne!(
            material_texture_key(0, &red),
            material_texture_key(1, &red),
            "two materials of one scene never share a uniform buffer"
        );
        red.albedo = Some(LabTexture {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        });
        assert_ne!(
            before,
            material_texture_key(0, &red),
            "new pixels must miss the bind group"
        );
    }

    #[test]
    fn graph_slot_pixels_and_srgb_change_the_material_key() {
        let rgba = |bytes: [u8; 4], srgb: bool| {
            let mut graph = crate::SurfaceGraph::new("fn material_surface() {}");
            graph.textures[0] = Some(crate::surface_graph::GraphTexture::rgba(
                LabTexture {
                    width: 1,
                    height: 1,
                    rgba: bytes.to_vec(),
                },
                srgb,
            ));
            LabMaterial {
                surface_graph: Some(std::sync::Arc::new(graph)),
                ..LabMaterial::default()
            }
        };
        let red = rgba([255, 0, 0, 255], true);
        let blue = rgba([0, 0, 255, 255], true);
        let linear = rgba([0, 0, 255, 255], false);
        assert_ne!(
            material_texture_key(0, &red),
            material_texture_key(0, &blue),
            "graph slot pixels must miss the bind group"
        );
        assert_ne!(
            material_texture_key(0, &blue),
            material_texture_key(0, &linear),
            "graph slot srgb must miss the bind group"
        );
        assert_eq!(
            material_texture_key(0, &red),
            material_texture_key(0, &rgba([255, 0, 0, 255], true)),
            "the same graph pixels keep the bind group"
        );
    }

    #[test]
    fn an_empty_image_slot_does_not_move_the_key_and_a_shared_image_does() {
        let plain = LabMaterial::default();
        let explicit = LabMaterial {
            images: crate::texture_residency::MaterialImages::EMPTY,
            ..LabMaterial::default()
        };
        assert_eq!(
            material_texture_key(0, &plain),
            material_texture_key(0, &explicit)
        );
        let image = crate::SharedImage::rgba(1, 1, vec![9, 8, 7, 255], true).expect("image");
        let shared = LabMaterial {
            images: crate::texture_residency::MaterialImages {
                albedo: Some(crate::MaterialImage::Shared(image.clone())),
                ..crate::texture_residency::MaterialImages::EMPTY
            },
            ..LabMaterial::default()
        };
        assert_ne!(material_texture_key(0, &plain), material_texture_key(0, &shared));
        let again = LabMaterial {
            images: crate::texture_residency::MaterialImages {
                albedo: Some(crate::MaterialImage::Shared(image)),
                ..crate::texture_residency::MaterialImages::EMPTY
            },
            ..LabMaterial::default()
        };
        assert_eq!(
            material_texture_key(0, &shared),
            material_texture_key(0, &again),
            "clones of one image keep the bind group"
        );
    }

    /// Deterministic test bytes: `(31 i + 7) mod 256`.
    fn wide_pattern(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| i.wrapping_mul(31).wrapping_add(7) as u8)
            .collect()
    }

    /// Review of fixrev-21-k, finding 5: `mix_bytes_wide` had no test of its own (the only
    /// caller test used a 16-byte map, which has no tail). Known answers for the empty input,
    /// a tail-only input, one exact chunk, four chunks plus a tail, and a 1 MiB buffer, from
    /// FNV-1a's offset basis. The values were computed by an independent model of the
    /// documented construction (little-endian 8-byte chunks round-robin over four lanes,
    /// tail zero-padded, length folded in), so they also pin endian stability: a big-endian
    /// read changes every non-empty row. CPU only, so the guard does not depend on host speed.
    #[test]
    fn mix_bytes_wide_matches_its_known_answers() {
        const SEED: u64 = 0xcbf2_9ce4_8422_2325;
        let rows: [(usize, u64); 5] = [
            (0, 0x410b_43b9_1e41_9cee),
            (7, 0x687f_c262_7c28_abff),
            (8, 0xb6da_c3ab_7d2c_a233),
            (33, 0xb9f1_9d81_e26c_955d),
            (1 << 20, 0x404a_63fe_f95d_c35b),
        ];
        for (len, want) in rows {
            let mut hash = SEED;
            mix_bytes_wide(&mut hash, &wide_pattern(len));
            assert_eq!(hash, want, "length {len}: {hash:#018x}");
        }
    }

    /// A one-byte edit anywhere (here in the tail of a 1 MiB buffer, and in its first chunk)
    /// changes the hash, and so does a length change with the same leading bytes.
    #[test]
    fn mix_bytes_wide_changes_with_a_one_byte_edit() {
        const SEED: u64 = 0xcbf2_9ce4_8422_2325;
        let hash_of = |bytes: &[u8]| {
            let mut hash = SEED;
            mix_bytes_wide(&mut hash, bytes);
            hash
        };
        let base = wide_pattern(1 << 20);
        let whole = hash_of(&base);
        let mut edited = base.clone();
        edited[(1 << 20) - 3] ^= 1;
        assert_eq!(hash_of(&edited), 0x946e_b82a_0345_b199);
        assert_ne!(hash_of(&edited), whole);
        let mut front = base.clone();
        front[0] ^= 0x80;
        assert_ne!(hash_of(&front), whole);
        for len in [7, 8, 33] {
            let short = wide_pattern(len);
            let mut one = short.clone();
            one[len - 1] ^= 1;
            assert_ne!(hash_of(&one), hash_of(&short), "length {len}");
        }
        // Trailing zero bytes are not absorbed by the zero-padded tail: the length is mixed.
        assert_ne!(hash_of(&[1, 2, 3]), hash_of(&[1, 2, 3, 0]));
    }
}
