// SPDX-License-Identifier: MPL-2.0
//! The material half of the AOV pass (ADR-3256): the same draw list as the id/motion pass drawn
//! once more with a fragment stage that reads the draw's material, into
//! - target A (`Rgba32Float`): world normal xyz and roughness,
//! - target B (`Rgba32Float`): linear base color rgb and metalness,
//! - then, in a second render pass over the same vertex stage, target C (`R32Uint`): material id.
//!
//! One shader module serves both passes (one vertex stage, one material-evaluation function), so
//! the three outputs cannot disagree about which surface is visible. A single pass cannot hold
//! all three targets: a color attachment budget of 32 bytes per sample fits A and B or A and C,
//! not all of them.
//!
//! The material is evaluated from the engine's `CpuMaterial` the way the forward renderer reads
//! its scalar and base-color slots: base color is the factor times the sRGB-decoded base-color
//! texture, roughness and metalness are the factors (times the green and blue channel of a packed
//! ORM map, `orm_packed`, clamped to 0..1), and an alpha-masked material discards below its
//! cutoff exactly like the id pass. The normal is the vertex normal carried through the draw's
//! normal matrix (inverse transpose of the model's upper 3×3), interpolated, renormalized and
//! flipped toward the camera on a back face. Not read: normal maps, material graphs, shared or
//! dynamic image slots, vertex colour, skinned deformation of the normal (the rest-pose normal is
//! used) and morph targets.

use std::collections::HashMap;
use std::sync::Arc;

use regia_core::glam::{Mat3, Mat4};
use regia_render::ALPHA_MASK;
use regia_render::RenderDevice;
use regia_render::runtime::{CpuMaterial, CpuMesh};

use super::{DEPTH_FORMAT, Draw, EVICT_AFTER, Pipes, VertexSource, buffer, copy_out, map_rows, mat_bytes};
use crate::render_out::aov::MaterialPixels;

const FLOAT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;
const ID_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Uint;

/// Bytes of one material-pass instance: the model, the normal matrix as three padded columns and
/// the ids.
const INSTANCE_BYTES: usize = 64 + 3 * 16 + 16;
/// Bytes of the globals uniform: the view-projection matrix.
const GLOBALS_BYTES: usize = 64;

pub(super) const SHADER: &str = r"
struct Globals { view_proj: mat4x4<f32> };
struct Instance {
    model: mat4x4<f32>,
    n0: vec4<f32>,
    n1: vec4<f32>,
    n2: vec4<f32>,
    ids: vec4<u32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;

// flags.x: alpha-masked material; flags.y: the ORM map carries roughness (G) and metalness (B).
struct Params { color: vec4<f32>, rough_metal_cutoff: vec4<f32>, flags: vec4<u32> };
@group(1) @binding(0) var albedo: texture_2d<f32>;
@group(1) @binding(1) var orm: texture_2d<f32>;
@group(1) @binding(2) var tex_sampler: sampler;
@group(1) @binding(3) var<uniform> params: Params;

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) @interpolate(flat) material: u32,
};

@vertex fn vs_material(
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @builtin(instance_index) instance: u32,
) -> VertexOut {
    let inst = instances[instance];
    let world = inst.model * vec4<f32>(position, 1.0);
    let n = inst.n0.xyz * normal.x + inst.n1.xyz * normal.y + inst.n2.xyz * normal.z;
    return VertexOut(globals.view_proj * world, n, uv, inst.ids.x);
}

fn masked_out(uv: vec2<f32>) -> bool {
    let alpha = textureSampleLevel(albedo, tex_sampler, uv, 0.0).a * params.color.a;
    return params.flags.x != 0u && alpha < params.rough_metal_cutoff.z;
}

struct SurfaceOut { @location(0) normal_roughness: vec4<f32>, @location(1) color_metalness: vec4<f32> };

@fragment fn fs_surface(in: VertexOut, @builtin(front_facing) front: bool) -> SurfaceOut {
    if masked_out(in.uv) {
        discard;
    }
    let tex = textureSampleLevel(albedo, tex_sampler, in.uv, 0.0);
    let packed = textureSampleLevel(orm, tex_sampler, in.uv, 0.0);
    var roughness = params.rough_metal_cutoff.x;
    var metalness = params.rough_metal_cutoff.y;
    if params.flags.y != 0u {
        roughness = clamp(roughness * packed.g, 0.0, 1.0);
        metalness = clamp(metalness * packed.b, 0.0, 1.0);
    }
    let n = normalize(select(-in.normal, in.normal, front));
    return SurfaceOut(
        vec4<f32>(n, roughness),
        vec4<f32>(params.color.rgb * tex.rgb, metalness),
    );
}

@fragment fn fs_material_id(in: VertexOut) -> @location(0) u32 {
    if masked_out(in.uv) {
        discard;
    }
    return in.material;
}
";

/// One material's bind group (group 1).
struct GpuMaterial {
    bind: wgpu::BindGroup,
    _owner: Arc<CpuMaterial>,
    last_used: u64,
}

/// A mesh's normals and the zero texture coordinates a terrain draw binds.
struct GpuNormals {
    buffer: wgpu::Buffer,
    _owner: Arc<CpuMesh>,
    last_used: u64,
}

struct Targets {
    width: u32,
    height: u32,
    normal_roughness: wgpu::Texture,
    color_metalness: wgpu::Texture,
    material: wgpu::Texture,
    depth: wgpu::TextureView,
}

/// The terrain's normals (per grid vertex) and zero texture coordinates, built with the grid.
pub(super) struct TerrainNormals {
    pub(super) normals: wgpu::Buffer,
    pub(super) uvs: wgpu::Buffer,
}

/// The material pass of a job's AOVs. Created with the first frame that asks for a material AOV.
pub(super) struct MaterialPass {
    surface_pipe: wgpu::RenderPipeline,
    id_pipe: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    material_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    white_srgb: wgpu::TextureView,
    white_linear: wgpu::TextureView,
    materials: HashMap<usize, GpuMaterial>,
    normals: HashMap<usize, GpuNormals>,
    terrain: Option<TerrainNormals>,
    targets: Option<Targets>,
    /// The terrain's material: neutral grey, roughness 1, metalness 0 (it has no `CpuMaterial`).
    neutral: Arc<CpuMaterial>,
}

/// Smooth normals of a terrain grid (area-weighted face normals, `+y` up for a standard grid).
pub(super) fn grid_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut sums = vec![[0.0f32; 3]; positions.len()];
    for tri in indices.as_chunks::<3>().0 {
        let [a, b, c] = tri.map(|i| i as usize);
        let (Some(pa), Some(pb), Some(pc)) = (positions.get(a), positions.get(b), positions.get(c)) else {
            continue;
        };
        let u = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
        let v = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        for i in [a, b, c] {
            for (sum, add) in sums[i].iter_mut().zip(n) {
                *sum += add;
            }
        }
    }
    sums.into_iter()
        .map(|n| {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len > 1.0e-12 {
                [n[0] / len, n[1] / len, n[2] / len]
            } else {
                [0.0, 1.0, 0.0]
            }
        })
        .collect()
}

/// The matrix that carries a normal with `model`: the inverse transpose of its upper 3×3 (the
/// identity when the matrix is singular).
pub(super) fn normal_matrix(model: &[[f32; 4]; 4]) -> Mat3 {
    let upper = Mat3::from_mat4(Mat4::from_cols_array_2d(model));
    if upper.determinant().abs() > 1.0e-20 && upper.is_finite() {
        upper.inverse().transpose()
    } else {
        Mat3::IDENTITY
    }
}

fn vec3_bytes(out: &mut Vec<u8>, v: [f32; 3]) {
    for c in v {
        out.extend_from_slice(&c.to_le_bytes());
    }
}

static POSITION: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
    format: wgpu::VertexFormat::Float32x3,
    offset: 0,
    shader_location: 0,
}];
static NORMAL: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
    format: wgpu::VertexFormat::Float32x3,
    offset: 0,
    shader_location: 1,
}];
static UV: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
    format: wgpu::VertexFormat::Float32x2,
    offset: 0,
    shader_location: 2,
}];

fn vertex_layouts() -> [Option<wgpu::VertexBufferLayout<'static>>; 3] {
    let layout = |stride, attributes: &'static [wgpu::VertexAttribute]| {
        Some(wgpu::VertexBufferLayout {
            array_stride: stride,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes,
        })
    };
    [layout(12, &POSITION), layout(12, &NORMAL), layout(8, &UV)]
}

fn bind_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BindingType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty,
        count: None,
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    bind_entry(
        binding,
        wgpu::ShaderStages::FRAGMENT,
        wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
    )
}

fn uniform_ty() -> wgpu::BindingType {
    wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: false,
        min_binding_size: None,
    }
}

/// A texture of `rgba` bytes (`srgb`: decoded by the sampler), or a 1×1 white one when `None`.
fn upload(
    device: &RenderDevice,
    label: &str,
    size: (u32, u32),
    rgba: &[u8],
    srgb: bool,
) -> wgpu::TextureView {
    let extent = wgpu::Extent3d {
        width: size.0,
        height: size.1,
        depth_or_array_layers: 1,
    };
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: if srgb {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        },
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    device.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.0 * 4),
            rows_per_image: Some(size.1),
        },
        extent,
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// A usable map of the material: its pixels cover its size.
fn usable(t: &regia_render::LabTexture) -> bool {
    t.width > 0 && t.height > 0 && t.rgba.len() >= t.width as usize * t.height as usize * 4
}

impl MaterialPass {
    pub(super) fn new(device: &RenderDevice) -> Self {
        let gpu = &device.device;
        let module = gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("render-out aov material"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
        });
        let layout = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("render-out aov material"),
            entries: &[
                bind_entry(0, wgpu::ShaderStages::VERTEX, uniform_ty()),
                bind_entry(
                    1,
                    wgpu::ShaderStages::VERTEX,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
            ],
        });
        let material_layout = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("render-out aov material params"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                bind_entry(
                    2,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
                bind_entry(3, wgpu::ShaderStages::FRAGMENT, uniform_ty()),
            ],
        });
        let pipeline_layout = gpu.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("render-out aov material"),
            bind_group_layouts: &[Some(&layout), Some(&material_layout)],
            immediate_size: 0,
        });
        let float_target = Some(wgpu::ColorTargetState {
            format: FLOAT_FORMAT,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        });
        let surface_targets = [float_target.clone(), float_target];
        let id_targets = [Some(wgpu::ColorTargetState {
            format: ID_FORMAT,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let buffers = vertex_layouts();
        let surface_pipe = Pipes {
            gpu,
            layout: &pipeline_layout,
            module: &module,
            targets: &surface_targets,
        }
        .build(
            "render-out aov surface",
            ("vs_material", "fs_surface"),
            &buffers,
            (true, wgpu::CompareFunction::Less),
        );
        let id_pipe = Pipes {
            gpu,
            layout: &pipeline_layout,
            module: &module,
            targets: &id_targets,
        }
        .build(
            "render-out aov material id",
            ("vs_material", "fs_material_id"),
            &buffers,
            (true, wgpu::CompareFunction::Less),
        );
        let sampler = gpu.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("render-out aov material"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let white = [255u8; 4];
        Self {
            surface_pipe,
            id_pipe,
            layout,
            material_layout,
            sampler,
            white_srgb: upload(device, "render-out aov white", (1, 1), &white, true),
            white_linear: upload(device, "render-out aov white", (1, 1), &white, false),
            materials: HashMap::new(),
            normals: HashMap::new(),
            terrain: None,
            targets: None,
            neutral: Arc::new(CpuMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                roughness: 1.0,
                metallic: 0.0,
                ..CpuMaterial::default()
            }),
        }
    }

    /// True once the terrain's normals exist (or the pass was told there is no terrain).
    pub(super) fn has_terrain(&self) -> bool {
        self.terrain.is_some()
    }

    /// Builds the terrain's normals and zero texture coordinates from the grid, once.
    pub(super) fn ensure_terrain(&mut self, device: &RenderDevice, positions: &[[f32; 3]], indices: &[u32]) {
        if self.terrain.is_some() {
            return;
        }
        let mut normals = Vec::with_capacity(positions.len() * 12);
        for n in grid_normals(positions, indices) {
            vec3_bytes(&mut normals, n);
        }
        self.terrain = Some(TerrainNormals {
            normals: buffer(
                device,
                "render-out aov terrain normals",
                &normals,
                wgpu::BufferUsages::VERTEX,
            ),
            uvs: buffer(
                device,
                "render-out aov terrain uvs",
                &vec![0u8; positions.len() * 8],
                wgpu::BufferUsages::VERTEX,
            ),
        });
    }

    /// The normals buffer of a mesh, cached by the mesh asset.
    fn mesh_normals(&mut self, device: &RenderDevice, mesh: &Arc<CpuMesh>, frame: u64) -> usize {
        let key = Arc::as_ptr(mesh) as usize;
        self.normals
            .entry(key)
            .and_modify(|cached| cached.last_used = frame)
            .or_insert_with(|| {
                let mut bytes = Vec::with_capacity(mesh.vertices.len() * 12);
                for v in &mesh.vertices {
                    vec3_bytes(&mut bytes, v.normal);
                }
                GpuNormals {
                    buffer: buffer(
                        device,
                        "render-out aov normals",
                        &bytes,
                        wgpu::BufferUsages::VERTEX,
                    ),
                    _owner: Arc::clone(mesh),
                    last_used: frame,
                }
            });
        key
    }

    /// The bind group of a material, cached by the material asset.
    fn material(&mut self, device: &RenderDevice, material: &Arc<CpuMaterial>, frame: u64) -> usize {
        let key = Arc::as_ptr(material) as usize;
        if let Some(cached) = self.materials.get_mut(&key) {
            cached.last_used = frame;
            return key;
        }
        let albedo = material.albedo.as_ref().filter(|t| usable(t)).map(|t| {
            let bytes = t.width as usize * t.height as usize * 4;
            upload(
                device,
                "render-out aov albedo",
                (t.width, t.height),
                &t.rgba[..bytes],
                true,
            )
        });
        let orm = material
            .orm_packed
            .then_some(material.occlusion_map.as_ref())
            .flatten()
            .filter(|t| usable(t))
            .map(|t| {
                let bytes = t.width as usize * t.height as usize * 4;
                upload(
                    device,
                    "render-out aov orm",
                    (t.width, t.height),
                    &t.rgba[..bytes],
                    false,
                )
            });
        let masked = material.alpha_mode == ALPHA_MASK && albedo.is_some();
        let mut params = Vec::with_capacity(48);
        for v in material.base_color {
            params.extend_from_slice(&v.to_le_bytes());
        }
        for v in [material.roughness, material.metallic, material.alpha_cutoff, 0.0] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        for v in [u32::from(masked), u32::from(orm.is_some()), 0, 0] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        let params = buffer(
            device,
            "render-out aov material params",
            &params,
            wgpu::BufferUsages::UNIFORM,
        );
        let bind = device.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("render-out aov material"),
            layout: &self.material_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(albedo.as_ref().unwrap_or(&self.white_srgb)),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(orm.as_ref().unwrap_or(&self.white_linear)),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: params.as_entire_binding(),
                },
            ],
        });
        self.materials.insert(
            key,
            GpuMaterial {
                bind,
                _owner: Arc::clone(material),
                last_used: frame,
            },
        );
        key
    }

    fn ensure_targets(&mut self, device: &RenderDevice, width: u32, height: u32) {
        if self
            .targets
            .as_ref()
            .is_some_and(|t| t.width == width && t.height == height)
        {
            return;
        }
        let texture = |label, format| {
            device.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        self.targets = Some(Targets {
            width,
            height,
            normal_roughness: texture("render-out aov normal", FLOAT_FORMAT),
            color_metalness: texture("render-out aov color", FLOAT_FORMAT),
            material: texture("render-out aov material id", ID_FORMAT),
            depth: texture("render-out aov material depth", DEPTH_FORMAT)
                .create_view(&wgpu::TextureViewDescriptor::default()),
        });
    }

    /// Draws `draws` (the id pass's list) through `camera` (view, view-projection) and reads
    /// the three targets back.
    ///
    /// # Errors
    ///
    /// The GPU readback failed.
    pub(super) fn render(
        &mut self,
        pass: &super::AovPass,
        device: &RenderDevice,
        draws: &[Draw],
        camera: (&Mat4, &Mat4),
        size: (u32, u32),
        frame: u64,
    ) -> Result<MaterialPixels, String> {
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_aov_material").entered();
        let (width, height) = size;
        let (view, view_proj) = camera;
        let view = view.to_cols_array_2d();
        let mut instances = Vec::with_capacity(draws.len().max(1) * INSTANCE_BYTES);
        for draw in draws {
            mat_bytes(&mut instances, &draw.model);
            let n = normal_matrix(&draw.model);
            for column in [n.x_axis, n.y_axis, n.z_axis] {
                for c in [column.x, column.y, column.z, 0.0] {
                    instances.extend_from_slice(&c.to_le_bytes());
                }
            }
            for v in [draw.material_id, 0, 0, 0] {
                instances.extend_from_slice(&v.to_le_bytes());
            }
        }
        if instances.is_empty() {
            instances.resize(INSTANCE_BYTES, 0);
        }
        let mut globals = Vec::with_capacity(GLOBALS_BYTES);
        mat_bytes(&mut globals, &view_proj.to_cols_array_2d());
        let globals = buffer(
            device,
            "render-out aov material globals",
            &globals,
            wgpu::BufferUsages::UNIFORM,
        );
        let instances = buffer(
            device,
            "render-out aov material instances",
            &instances,
            wgpu::BufferUsages::STORAGE,
        );
        let bind = device.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("render-out aov material"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: instances.as_entire_binding(),
                },
            ],
        });
        // Per draw: its material bind group key and its normals buffer key (`None`: skip).
        let mut keys: Vec<Option<(usize, Option<usize>)>> = Vec::with_capacity(draws.len());
        for draw in draws {
            keys.push(match (&draw.material, &draw.mesh_arc) {
                (Some(material), Some(mesh)) => {
                    let m = self.material(device, material, frame);
                    let n = self.mesh_normals(device, mesh, frame);
                    Some((m, Some(n)))
                }
                (Some(material), None) => Some((self.material(device, material, frame), None)),
                _ => None,
            });
        }
        // The terrain's material: neutral grey, roughness 1, metalness 0.
        let neutral = {
            let shared = Arc::clone(&self.neutral);
            self.material(device, &shared, frame)
        };
        self.ensure_targets(device, width, height);
        let targets = self
            .targets
            .as_ref()
            .ok_or("the material AOV targets are missing")?;
        let views = [
            targets
                .normal_roughness
                .create_view(&wgpu::TextureViewDescriptor::default()),
            targets
                .color_metalness
                .create_view(&wgpu::TextureViewDescriptor::default()),
            targets
                .material
                .create_view(&wgpu::TextureViewDescriptor::default()),
        ];
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render-out aov material"),
            });
        for (pipe, attachments) in [(&self.surface_pipe, &views[0..2]), (&self.id_pipe, &views[2..3])] {
            let colors: Vec<_> = attachments
                .iter()
                .map(|view| {
                    Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })
                })
                .collect();
            let mut render = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render-out aov material"),
                color_attachments: &colors,
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &targets.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            render.set_pipeline(pipe);
            render.set_bind_group(0, &bind, &[]);
            for (n, draw) in draws.iter().enumerate() {
                let (material_key, normals_key) = match keys[n] {
                    Some(k) => k,
                    None if matches!(draw.vertex, VertexSource::Terrain) => (neutral, None),
                    None => continue,
                };
                let Some(material) = self.materials.get(&material_key) else {
                    continue;
                };
                render.set_bind_group(1, &material.bind, &[]);
                let (positions, normals, uvs, indices, count) = match &draw.vertex {
                    VertexSource::Terrain => match (&pass.terrain, &self.terrain) {
                        (Some(Some(t)), Some(extra)) => {
                            (&t.positions, &extra.normals, &extra.uvs, &t.indices, t.count)
                        }
                        _ => continue,
                    },
                    VertexSource::Rigid(key) => {
                        let (Some(p), Some(nk), Some(i)) = (
                            pass.positions.get(key),
                            normals_key,
                            pass.indices.get(&draw.index),
                        ) else {
                            continue;
                        };
                        let Some(nbuf) = self.normals.get(&nk) else {
                            continue;
                        };
                        (&p.buffer, &nbuf.buffer, &p.uvs, &i.buffer, i.count)
                    }
                    VertexSource::Skinned(now, _) => {
                        let (Some(p), Some(nk), Some(i)) = (
                            pass.positions.get(&draw.mesh),
                            normals_key,
                            pass.indices.get(&draw.index),
                        ) else {
                            continue;
                        };
                        let Some(nbuf) = self.normals.get(&nk) else {
                            continue;
                        };
                        (now, &nbuf.buffer, &p.uvs, &i.buffer, i.count)
                    }
                };
                if count == 0 {
                    continue;
                }
                render.set_vertex_buffer(0, positions.slice(..));
                render.set_vertex_buffer(1, normals.slice(..));
                render.set_vertex_buffer(2, uvs.slice(..));
                render.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                let instance = n as u32;
                render.draw_indexed(0..count, 0, instance..instance + 1);
            }
        }
        let nr_rb = copy_out(device, &mut encoder, &targets.normal_roughness, width, height, 16);
        let cm_rb = copy_out(device, &mut encoder, &targets.color_metalness, width, height, 16);
        let id_rb = copy_out(device, &mut encoder, &targets.material, width, height, 4);
        device.queue.submit([encoder.finish()]);
        let floats = |bytes: Vec<u8>| -> Vec<[f32; 4]> {
            bytes
                .as_chunks::<16>()
                .0
                .iter()
                .map(|b| {
                    let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
                    [f(0), f(4), f(8), f(12)]
                })
                .collect()
        };
        let normal_roughness = floats(map_rows(device, &nr_rb, width, height, 16)?);
        let color_metalness = floats(map_rows(device, &cm_rb, width, height, 16)?);
        let material = map_rows(device, &id_rb, width, height, 4)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        self.materials
            .retain(|_, m| frame.saturating_sub(m.last_used) < EVICT_AFTER);
        self.normals
            .retain(|_, n| frame.saturating_sub(n.last_used) < EVICT_AFTER);
        Ok(MaterialPixels {
            normal_roughness,
            color_metalness,
            material,
            view,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Like the id pass's shader: no struct or array in a function-local `var` (ADR-026).
    #[test]
    fn material_shader_has_no_function_layout_errors() {
        let found = regia_render::wgsl_function_layout_errors("aov_material.wgsl", SHADER).expect("emits");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn grid_normals_point_up_on_a_flat_grid_and_follow_a_slope() {
        // Two triangles of a unit quad in the xz plane, counter-clockwise seen from +y.
        let flat = [[0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [1.0, 0.0, 0.0]];
        let indices = [0, 1, 2, 0, 2, 3];
        for n in grid_normals(&flat, &indices) {
            assert_eq!(n, [0.0, 1.0, 0.0]);
        }
        // Raise the x = 1 edge by 1: the slope leans toward -x, and the normals stay unit length.
        let slope = [[0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0, 1.0], [1.0, 1.0, 0.0]];
        for n in grid_normals(&slope, &indices) {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-6 && n[0] < 0.0 && n[1] > 0.0, "{n:?}");
        }
        // A vertex no triangle touches, and an index past the list, fall back to up.
        let lone = grid_normals(&[[0.0; 3]], &[0, 5, 6]);
        assert_eq!(lone, [[0.0, 1.0, 0.0]]);
    }

    #[test]
    fn the_normal_matrix_keeps_normals_perpendicular_under_non_uniform_scale() {
        // Scale x by 4: a surface with tangent (1, 1, 0) has normal (1, -1, 0) before the scale.
        let model = Mat4::from_scale(regia_core::glam::Vec3::new(4.0, 1.0, 1.0)).to_cols_array_2d();
        let n = normal_matrix(&model) * regia_core::glam::Vec3::new(1.0, -1.0, 0.0);
        let tangent =
            Mat3::from_mat4(Mat4::from_cols_array_2d(&model)) * regia_core::glam::Vec3::new(1.0, 1.0, 0.0);
        assert!(n.dot(tangent).abs() < 1e-6, "{n} vs {tangent}");
        // A singular matrix does not poison the pass.
        let flat = Mat4::from_scale(regia_core::glam::Vec3::new(1.0, 0.0, 1.0)).to_cols_array_2d();
        assert_eq!(normal_matrix(&flat), Mat3::IDENTITY);
    }
}
