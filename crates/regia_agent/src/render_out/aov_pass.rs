// SPDX-License-Identifier: MPL-2.0
//! The render-out AOV pass (ADR-1521): the frame's meshes and the terrain drawn once more, through
//! the same camera without jitter, at the delivered size, into an id target (`Rg32Uint`:
//! instance, class) and a data target (`Rgba32Float`: motion u, motion v, view depth, coverage),
//! then read back.
//!
//! The pass is its own small pipeline (positions only), so the frame renderer is untouched and
//! the AOVs do not depend on the quality tier, TAA or the upscaler. Motion comes from the pass's
//! own history: last delivered frame's view-projection and each instance's last model matrix (a
//! skinned mesh's last skinned positions), so it is the motion between delivered frames, not
//! between subframes or renderer frames. The sky is a fullscreen triangle drawn first: id 0,
//! no depth, and the motion of a direction at infinity (camera rotation only).
//!
//! Drawn: every extracted opaque or alpha-masked mesh the view's layer mask shows (skinned on the
//! CPU with the frame's palette, morph targets ignored; a masked material's base-colour alpha is
//! tested against its cutoff, so a foliage card has the shape of its leaves) and the terrain
//! heightfield as a grid of at most `aov::MAX_TERRAIN_SIDE` samples per side, built once per job.
//! Not drawn: translucent materials (blend, premultiplied, additive), shadow-only LODs,
//! particles, water, terrain holes and terrain edits during the job (see the agent page's known
//! issues).

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::{Mat4, Vec3};
use regia_render::runtime::CpuMaterial;
use regia_render::runtime::{CpuMesh, ExtractedFrame, RuntimeRenderer};
use regia_render::{ALPHA_MASK, ALPHA_OPAQUE, JointMatrix, RenderDevice, SHADOW_LOD_MESH_TAG, skin_position};

use serde_json::{Value, json};

mod material;
use material::MaterialPass;

use super::aov::{AovPixels, IdTable, InstanceKey, encodings_json, terrain_grid, terrain_stride};
use super::plan::{Request, fnv1a};
use super::writer::AovFile;

/// A job's AOVs: the pass, the files of the frame being made, and the hashes of the delivered
/// ones.
#[derive(Default)]
pub(crate) struct AovState {
    pass: Option<AovPass>,
    /// The AOV files of the frame being made (sampled at its middle subframe).
    pending: Vec<AovFile>,
    /// Per delivered frame, the FNV-1a hash of each AOV's raw bytes, in request order.
    hashes: Vec<Vec<u64>>,
    /// The input hash of the frame being made (see `inputs`).
    pending_input: Option<u64>,
    /// Per delivered frame, the FNV-1a hash of everything the pass uploaded (globals and
    /// instances: matrices, last frame's matrices, ids, held flags), ADR-1965: equal inputs with
    /// unequal AOVs point at the GPU, unequal inputs at the world or the camera.
    inputs: Vec<u64>,
}

impl AovState {
    /// Renders the AOVs of delivered frame `index` now, at the subframe they are sampled at.
    ///
    /// # Errors
    ///
    /// No render device or extracted frame, or the readback failed.
    pub(crate) fn sample(&mut self, world: &World, request: &Request, index: u64) -> Result<(), String> {
        if !request.aovs.is_on() {
            return Ok(());
        }
        if self.pass.is_none() {
            let device = world
                .get_resource::<RenderDevice>()
                .ok_or("no render device for the AOV pass")?;
            self.pass = Some(AovPass::new(device));
        }
        let pass = self.pass.as_mut().ok_or("the AOV pass is missing")?;
        let reset = index == 0 || request.is_cut(index);
        let pixels = pass.render(
            world,
            request.width,
            request.height,
            (reset, request.aovs.wants_material()),
        )?;
        self.pending_input = Some(pass.input_hash);
        self.pending = request
            .aovs
            .list
            .iter()
            .map(|aov| AovFile {
                aov: *aov,
                planes: pixels.planes(*aov, reset),
            })
            .collect();
        Ok(())
    }

    /// Takes the sampled files of the frame being delivered and records their hashes.
    pub(crate) fn take(&mut self) -> Vec<AovFile> {
        let files = std::mem::take(&mut self.pending);
        if let Some(input) = self.pending_input.take() {
            self.inputs.push(input);
        }
        if !files.is_empty() {
            self.hashes.push(
                files
                    .iter()
                    .map(|file| fnv1a(&file.planes.interleaved_le()))
                    .collect(),
            );
        }
        files
    }

    /// Keeps the hashes of the first `delivered` frames (the run on disk after a failure).
    pub(crate) fn truncate(&mut self, delivered: u64) {
        self.hashes.truncate(delivered as usize);
        self.inputs.truncate(delivered as usize);
    }

    /// The manifest's `aov` block (`null` without AOVs).
    pub(crate) fn manifest(&self, request: &Request, delivered: u64) -> Value {
        if !request.aovs.is_on() {
            return Value::Null;
        }
        let mut resets: Vec<u64> = std::iter::once(0)
            .chain(request.cuts.iter().copied())
            .filter(|frame| *frame < delivered)
            .collect();
        resets.dedup();
        let hashes: serde_json::Map<String, Value> = request
            .aovs
            .list
            .iter()
            .enumerate()
            .map(|(n, aov)| {
                let list: Vec<String> = self
                    .hashes
                    .iter()
                    .filter_map(|frame| frame.get(n))
                    .map(|hash| format!("{hash:016x}"))
                    .collect();
                (aov.name().to_string(), Value::from(list))
            })
            .collect();
        json!({
            "encodings": encodings_json(&request.aovs, request.width, request.height),
            "ids": self.pass.as_ref().map_or_else(|| IdTable::default().to_json(), |p| p.ids.to_json()),
            "terrain": self.pass.as_ref().and_then(|p| p.terrain_stride).map(|stride| json!({
                "drawn": true,
                "sample_stride": stride,
            })),
            "motion_reset_frames": resets,
            "frame_hashes": hashes,
            "input_hashes": self.inputs.iter().map(|hash| format!("{hash:016x}")).collect::<Vec<_>>(),
            "files": delivered * request.aovs.list.len() as u64,
        })
    }
}

const SHADER: &str = r"
struct Globals {
    view_proj: mat4x4<f32>,
    prev_view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    size: vec4<f32>,
};
struct Instance { model: mat4x4<f32>, prev_model: mat4x4<f32>, ids: vec4<u32> };
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;

struct MeshOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) ids: vec2<u32>,
    @location(1) prev: vec4<f32>,
    @location(2) view_z: f32,
    @location(3) @interpolate(flat) held: u32,
};

struct AovOut { @location(0) ids: vec2<u32>, @location(1) data: vec4<f32> };

fn pixel_of(clip: vec4<f32>) -> vec2<f32> {
    let ndc = clip.xy / clip.w;
    return vec2<f32>((ndc.x * 0.5 + 0.5) * globals.size.x, (0.5 - ndc.y * 0.5) * globals.size.y);
}

// Backward motion in pixels: where the surface was (`prev`, last frame's clip position) minus
// where it is now (`here`, this fragment's pixel center). Behind last frame's camera: 0. A held
// surface (`held`: its instance and the view are bit-identical to the last delivered frame's) has
// motion exactly 0, not the rounding of reconstructing one position twice (ADR-1965).
fn motion_to(prev: vec4<f32>, here: vec2<f32>, held: u32) -> vec2<f32> {
    if prev.w <= 1.0e-6 || held != 0u {
        return vec2<f32>(0.0, 0.0);
    }
    return pixel_of(prev) - here;
}

// The view is bit-identical to the last delivered frame's (`globals.size.z`).
fn view_held() -> u32 {
    return select(0u, 1u, globals.size.z > 0.5);
}

@vertex fn vs_mesh(
    @location(0) position: vec3<f32>,
    @location(1) prev_position: vec3<f32>,
    @builtin(instance_index) instance: u32,
) -> MeshOut {
    let inst = instances[instance];
    let world = inst.model * vec4<f32>(position, 1.0);
    // Constructors, not struct `var`s (ADR-026).
    return MeshOut(
        globals.view_proj * world,
        inst.ids.xy,
        globals.prev_view_proj * (inst.prev_model * vec4<f32>(prev_position, 1.0)),
        -(globals.view * world).z,
        inst.ids.z & view_held(),
    );
}

@fragment fn fs_mesh(in: MeshOut) -> AovOut {
    return AovOut(in.ids, vec4<f32>(motion_to(in.prev, in.clip.xy, in.held), in.view_z, 1.0));
}

// Alpha-masked materials: the base-colour alpha (times the factor) against the cutoff.
@group(1) @binding(0) var albedo: texture_2d<f32>;
@group(1) @binding(1) var albedo_sampler: sampler;
@group(1) @binding(2) var<uniform> mask_factors: vec4<f32>;

struct MaskedOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) ids: vec2<u32>,
    @location(1) prev: vec4<f32>,
    @location(2) view_z: f32,
    @location(3) uv: vec2<f32>,
    @location(4) @interpolate(flat) held: u32,
};

@vertex fn vs_masked(
    @location(0) position: vec3<f32>,
    @location(1) prev_position: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @builtin(instance_index) instance: u32,
) -> MaskedOut {
    let inst = instances[instance];
    let world = inst.model * vec4<f32>(position, 1.0);
    return MaskedOut(
        globals.view_proj * world,
        inst.ids.xy,
        globals.prev_view_proj * (inst.prev_model * vec4<f32>(prev_position, 1.0)),
        -(globals.view * world).z,
        uv,
        inst.ids.z & view_held(),
    );
}

@fragment fn fs_masked(in: MaskedOut) -> AovOut {
    let alpha = textureSample(albedo, albedo_sampler, in.uv).a * mask_factors.y;
    if alpha < mask_factors.x {
        discard;
    }
    return AovOut(in.ids, vec4<f32>(motion_to(in.prev, in.clip.xy, in.held), in.view_z, 1.0));
}

@vertex fn vs_sky(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

@fragment fn fs_sky(@builtin(position) frag: vec4<f32>) -> AovOut {
    let ndc = vec2<f32>(frag.x / globals.size.x * 2.0 - 1.0, 1.0 - frag.y / globals.size.y * 2.0);
    let on_near = globals.inv_view_proj * vec4<f32>(ndc, 0.0, 1.0);
    let dir = on_near.xyz / on_near.w - globals.eye.xyz;
    return AovOut(
        vec2<u32>(0u, 0u),
        vec4<f32>(motion_to(globals.prev_view_proj * vec4<f32>(dir, 0.0), frag.xy, view_held()), 0.0, 0.0),
    );
}
";

/// Bytes of one `Instance` in the storage buffer: two matrices and four ids.
const INSTANCE_BYTES: usize = 64 + 64 + 16;
/// Bytes of the `Globals` uniform.
const GLOBALS_BYTES: usize = 4 * 64 + 16 + 16;
/// Frames a cached mesh may go undrawn before its buffers are dropped.
const EVICT_AFTER: u64 = 600;

const ID_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rg32Uint;
const DATA_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Positions (object space) and texture coordinates of a mesh on the GPU, kept while the mesh is
/// drawn.
struct GpuPositions {
    buffer: wgpu::Buffer,
    uvs: wgpu::Buffer,
    /// Keeps the `Arc` alive so its address is not reused while cached.
    _owner: Arc<CpuMesh>,
    last_used: u64,
}

/// An index list on the GPU (a mesh's own, or a LOD's `draw_indices`).
struct GpuIndices {
    buffer: wgpu::Buffer,
    count: u32,
    /// Keep the list's owner alive so its address is not reused while cached.
    _mesh: Arc<CpuMesh>,
    _lod: Option<Arc<Vec<u32>>>,
    last_used: u64,
}

struct Targets {
    width: u32,
    height: u32,
    ids: wgpu::Texture,
    data: wgpu::Texture,
    depth: wgpu::TextureView,
}

/// A masked material's base-colour texture and cutoff (group 1 of the masked pipeline).
struct GpuMaterial {
    bind: wgpu::BindGroup,
    _owner: Arc<CpuMaterial>,
    last_used: u64,
}

/// The terrain grid, built once per job.
struct TerrainDraw {
    positions: wgpu::Buffer,
    indices: wgpu::Buffer,
    count: u32,
}

/// What the previous delivered frame's AOVs saw, for motion.
#[derive(Default)]
struct History {
    view_proj: Option<Mat4>,
    models: HashMap<u64, [[f32; 4]; 4]>,
    skinned: HashMap<u64, Vec<[f32; 3]>>,
}

/// The per-job AOV pass. Created on the job's first AOV frame.
pub(crate) struct AovPass {
    mesh_pipe: wgpu::RenderPipeline,
    masked_pipe: wgpu::RenderPipeline,
    sky_pipe: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    material_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    materials: HashMap<usize, GpuMaterial>,
    positions: HashMap<usize, GpuPositions>,
    indices: HashMap<usize, GpuIndices>,
    terrain: Option<Option<TerrainDraw>>,
    targets: Option<Targets>,
    /// The material half (ADR-3256), created with the first frame that asks for a material AOV.
    material: Option<MaterialPass>,
    history: History,
    frame: u64,
    /// FNV-1a of the last frame's uploaded globals and instances (`AovState::inputs`).
    input_hash: u64,
    /// The job's instance and class ids.
    pub ids: IdTable,
    /// Terrain sample stride the grid was built with (`None`: no terrain).
    pub terrain_stride: Option<u32>,
}

/// One instance to draw.
struct Draw {
    instance: u32,
    class: u32,
    model: [[f32; 4]; 4],
    prev_model: [[f32; 4]; 4],
    /// The instance is where it was in the last delivered frame, bit for bit (model, and for a
    /// skinned mesh its skinned positions): with a held view its motion is exactly 0.
    held: bool,
    vertex: VertexSource,
    index: usize,
    /// The mesh's cached positions and texture coordinates (`positions` key; unused by the terrain).
    mesh: usize,
    /// A masked material (`materials` key); `None` draws the mesh whole.
    mask: Option<usize>,
    /// The mesh asset and its material, for the material pass (`None` for the terrain).
    mesh_arc: Option<Arc<CpuMesh>>,
    material: Option<Arc<CpuMaterial>>,
    /// The material id (0 unless the request has a material AOV).
    material_id: u32,
}

enum VertexSource {
    /// A cached rigid mesh (positions buffer key): the previous positions are the same buffer.
    Rigid(usize),
    /// CPU-skinned positions of this frame and the last.
    Skinned(wgpu::Buffer, wgpu::Buffer),
    /// The terrain grid.
    Terrain,
}

/// Texture coordinates (vertex buffer 2 of the masked pipeline).
static UV: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
    format: wgpu::VertexFormat::Float32x2,
    offset: 0,
    shader_location: 2,
}];
/// This frame's object-space position (vertex buffer 0).
static POSITION_NOW: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
    format: wgpu::VertexFormat::Float32x3,
    offset: 0,
    shader_location: 0,
}];
/// Last frame's object-space position (vertex buffer 1; the same buffer for a rigid mesh).
static POSITION_PREV: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
    format: wgpu::VertexFormat::Float32x3,
    offset: 0,
    shader_location: 1,
}];

/// What both AOV pipelines share.
struct Pipes<'a> {
    gpu: &'a wgpu::Device,
    layout: &'a wgpu::PipelineLayout,
    module: &'a wgpu::ShaderModule,
    targets: &'a [Option<wgpu::ColorTargetState>],
}

impl Pipes<'_> {
    /// A pipeline with the `(vertex, fragment)` entry points and `(depth write, depth test)`.
    fn build(
        &self,
        label: &str,
        entries: (&str, &str),
        buffers: &[Option<wgpu::VertexBufferLayout<'_>>],
        depth: (bool, wgpu::CompareFunction),
    ) -> wgpu::RenderPipeline {
        self.gpu.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(self.layout),
            vertex: wgpu::VertexState {
                module: self.module,
                entry_point: Some(entries.0),
                buffers,
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: self.module,
                entry_point: Some(entries.1),
                targets: self.targets,
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // Double-sided: a plane or a shell seen from behind still has an id.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(depth.0),
                depth_compare: Some(depth.1),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
    }
}

fn mat_bytes(out: &mut Vec<u8>, m: &[[f32; 4]; 4]) {
    for column in m {
        for v in column {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

fn position_bytes(positions: impl Iterator<Item = [f32; 3]>) -> Vec<u8> {
    let mut out = Vec::new();
    for p in positions {
        for c in p {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    out
}

fn buffer(device: &RenderDevice, label: &str, bytes: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: (bytes.len() as u64).max(16).next_multiple_of(4),
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    device.queue.write_buffer(&buffer, 0, bytes);
    buffer
}

fn index_bytes(indices: &[u32]) -> Vec<u8> {
    indices.iter().flat_map(|i| i.to_le_bytes()).collect()
}

/// A mesh skinned by `palette` (object space, before the model matrix), as the frame's skinning
/// shaders do. An influence naming a joint past the palette counts as the identity.
pub(crate) fn skin_positions(mesh: &CpuMesh, palette: &[JointMatrix]) -> Vec<[f32; 3]> {
    let joint = |j: u32| {
        palette
            .get(j as usize)
            .copied()
            .unwrap_or_else(JointMatrix::identity)
    };
    mesh.vertices
        .iter()
        .map(|v| {
            let joints = [
                joint(v.joints[0]),
                joint(v.joints[1]),
                joint(v.joints[2]),
                joint(v.joints[3]),
            ];
            skin_position(v.position, &joints, v.weights)
        })
        .collect()
}

/// The camera the job's frame renders with: view, projection and eye, unjittered.
pub(crate) fn camera_matrices(frame: &ExtractedFrame, width: u32, height: u32) -> (Mat4, Mat4, Vec3) {
    let index = frame.window_views().first().map_or(0, |(index, _)| *index);
    let aspect = width.max(1) as f32 / height.max(1) as f32;
    let (eye, target, fov_y, near, far) = frame.views.get(index).map_or(
        (
            [0.0; 3],
            [0.0, 0.0, -1.0],
            std::f32::consts::FRAC_PI_4,
            0.1,
            1000.0,
        ),
        |v| {
            (
                v.camera.eye,
                v.camera.target,
                v.camera.fov_y,
                v.camera.near,
                v.camera.far,
            )
        },
    );
    let (fov_y, near, far) = regia_render::view_math::usable_projection(fov_y, near, far);
    let fov_y = if frame.quality.horizontal_plus {
        regia_render::output::horizontal_plus_fov_y(fov_y, aspect)
    } else {
        fov_y
    };
    let eye_v = Vec3::from_array(eye);
    let eye_v = if eye_v.is_finite() { eye_v } else { Vec3::ZERO };
    let view = regia_render::view_math::look_at_view(eye_v, Vec3::from_array(target), Vec3::Y);
    (view, perspective(fov_y, aspect, near, far), eye_v)
}

fn layer_visible(layer_mask: u32, layer: u32) -> bool {
    layer < 32 && layer_mask & (1 << layer) != 0
}

/// True for a material the AOVs draw: opaque, or alpha-masked (tested per pixel). Translucent
/// materials (blend, premultiplied, additive) show what is behind them.
fn drawn_material(alpha_mode: u32) -> bool {
    matches!(alpha_mode, ALPHA_OPAQUE | ALPHA_MASK)
}

/// True for a draw that only goes into shadow maps (a shadow LOD, ADR-1384).
fn shadow_only(lod_key: u64) -> bool {
    (lod_key & SHADOW_LOD_MESH_TAG) == SHADOW_LOD_MESH_TAG
}

impl AovPass {
    pub(crate) fn new(device: &RenderDevice) -> Self {
        let gpu = &device.device;
        let module = gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("render-out aov"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
        });
        let layout = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("render-out aov"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let material_layout = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("render-out aov mask"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = gpu.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("render-out aov"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let masked_layout = gpu.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("render-out aov masked"),
            bind_group_layouts: &[Some(&layout), Some(&material_layout)],
            immediate_size: 0,
        });
        let targets = [
            Some(wgpu::ColorTargetState {
                format: ID_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            }),
            Some(wgpu::ColorTargetState {
                format: DATA_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            }),
        ];
        let pipes = Pipes {
            gpu,
            layout: &pipeline_layout,
            module: &module,
            targets: &targets,
        };
        let mesh_pipe = pipes.build(
            "render-out aov meshes",
            ("vs_mesh", "fs_mesh"),
            &[
                Some(wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &POSITION_NOW,
                }),
                Some(wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &POSITION_PREV,
                }),
            ],
            (true, wgpu::CompareFunction::Less),
        );
        let masked_pipe = Pipes {
            layout: &masked_layout,
            ..pipes
        }
        .build(
            "render-out aov masked",
            ("vs_masked", "fs_masked"),
            &[
                Some(wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &POSITION_NOW,
                }),
                Some(wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &POSITION_PREV,
                }),
                Some(wgpu::VertexBufferLayout {
                    array_stride: 8,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &UV,
                }),
            ],
            (true, wgpu::CompareFunction::Less),
        );
        let sampler = gpu.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("render-out aov mask"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        // The sky goes first and leaves the depth at the far plane.
        let sky_pipe = pipes.build(
            "render-out aov sky",
            ("vs_sky", "fs_sky"),
            &[],
            (false, wgpu::CompareFunction::Always),
        );
        Self {
            mesh_pipe,
            masked_pipe,
            sky_pipe,
            layout,
            material_layout,
            sampler,
            materials: HashMap::new(),
            positions: HashMap::new(),
            indices: HashMap::new(),
            terrain: None,
            targets: None,
            material: None,
            history: History::default(),
            frame: 0,
            input_hash: 0,
            ids: IdTable::default(),
            terrain_stride: None,
        }
    }

    fn ensure_targets(&mut self, device: &RenderDevice, width: u32, height: u32) {
        if self
            .targets
            .as_ref()
            .is_none_or(|t| t.width != width || t.height != height)
        {
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
            let depth = texture("render-out aov depth", DEPTH_FORMAT)
                .create_view(&wgpu::TextureViewDescriptor::default());
            self.targets = Some(Targets {
                width,
                height,
                ids: texture("render-out aov ids", ID_FORMAT),
                data: texture("render-out aov data", DATA_FORMAT),
                depth,
            });
        }
    }

    /// The terrain's grid (positions and indices), if the renderer has a heightfield.
    fn terrain_geometry(renderer: Option<&RuntimeRenderer>) -> Option<(u32, Vec<[f32; 3]>, Vec<u32>)> {
        let f = renderer.and_then(|r| r.0.reference_heightfield())?;
        let stride = terrain_stride(f.width, f.depth);
        let (positions, indices) = terrain_grid(f.width, f.depth, f.origin, f.spacing, &f.heights, stride)?;
        Some((stride, positions, indices))
    }

    /// The terrain grid, built the first time (the renderer's terrain heightfield, if any).
    fn ensure_terrain(&mut self, device: &RenderDevice, renderer: Option<&RuntimeRenderer>) {
        if self.terrain.is_some() {
            return;
        }
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_aov_terrain").entered();
        let built = Self::terrain_geometry(renderer).map(|(stride, positions, indices)| {
            self.terrain_stride = Some(stride);
            TerrainDraw {
                positions: buffer(
                    device,
                    "render-out aov terrain",
                    &position_bytes(positions.into_iter()),
                    wgpu::BufferUsages::VERTEX,
                ),
                indices: buffer(
                    device,
                    "render-out aov terrain indices",
                    &index_bytes(&indices),
                    wgpu::BufferUsages::INDEX,
                ),
                count: indices.len() as u32,
            }
        });
        self.terrain = Some(built);
    }

    fn rigid(&mut self, device: &RenderDevice, mesh: &Arc<CpuMesh>) -> usize {
        let key = Arc::as_ptr(mesh) as usize;
        let frame = self.frame;
        self.positions
            .entry(key)
            .and_modify(|cached| cached.last_used = frame)
            .or_insert_with(|| GpuPositions {
                buffer: buffer(
                    device,
                    "render-out aov positions",
                    &position_bytes(mesh.vertices.iter().map(|v| v.position)),
                    wgpu::BufferUsages::VERTEX,
                ),
                uvs: buffer(
                    device,
                    "render-out aov uvs",
                    &mesh
                        .vertices
                        .iter()
                        .flat_map(|v| v.uv)
                        .flat_map(f32::to_le_bytes)
                        .collect::<Vec<u8>>(),
                    wgpu::BufferUsages::VERTEX,
                ),
                _owner: Arc::clone(mesh),
                last_used: frame,
            });
        key
    }

    /// The mask bind group of an alpha-masked material with a base-colour texture (`None` for
    /// any other material: it is drawn whole).
    fn material(&mut self, device: &RenderDevice, material: &Arc<CpuMaterial>) -> Option<usize> {
        if material.alpha_mode != ALPHA_MASK {
            return None;
        }
        let albedo = material.albedo.as_ref().filter(|t| {
            t.width > 0 && t.height > 0 && t.rgba.len() >= t.width as usize * t.height as usize * 4
        })?;
        let key = Arc::as_ptr(material) as usize;
        let frame = self.frame;
        if let Some(cached) = self.materials.get_mut(&key) {
            cached.last_used = frame;
            return Some(key);
        }
        let size = wgpu::Extent3d {
            width: albedo.width,
            height: albedo.height,
            depth_or_array_layers: 1,
        };
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("render-out aov mask"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let bytes = albedo.width as usize * albedo.height as usize * 4;
        device.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &albedo.rgba[..bytes],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(albedo.width * 4),
                rows_per_image: Some(albedo.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut factors = Vec::with_capacity(16);
        for v in [material.alpha_cutoff, material.base_color[3], 0.0, 0.0] {
            factors.extend_from_slice(&v.to_le_bytes());
        }
        let factors = buffer(
            device,
            "render-out aov mask cutoff",
            &factors,
            wgpu::BufferUsages::UNIFORM,
        );
        let bind = device.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("render-out aov mask"),
            layout: &self.material_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: factors.as_entire_binding(),
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
        Some(key)
    }

    fn index_list(
        &mut self,
        device: &RenderDevice,
        mesh: &Arc<CpuMesh>,
        lod: Option<&Arc<Vec<u32>>>,
    ) -> usize {
        let (key, list, lod_owner) = match lod {
            Some(lod) => (Arc::as_ptr(lod) as usize, lod.as_slice(), Some(Arc::clone(lod))),
            None => (mesh.indices.as_ptr() as usize, mesh.indices.as_slice(), None),
        };
        let frame = self.frame;
        self.indices
            .entry(key)
            .and_modify(|cached| cached.last_used = frame)
            .or_insert_with(|| GpuIndices {
                buffer: buffer(
                    device,
                    "render-out aov indices",
                    &index_bytes(list),
                    wgpu::BufferUsages::INDEX,
                ),
                count: list.len() as u32,
                _mesh: Arc::clone(mesh),
                _lod: lod_owner,
                last_used: frame,
            });
        key
    }

    /// Draws this frame's AOVs at `width`×`height` and reads them back. `reset` (frame 0, a cut)
    /// forgets the history, so this frame's motion is 0 and the next one moves from here.
    /// `flags` is `(reset, material)`: `material` also draws the material pass (ADR-3256).
    ///
    /// # Errors
    ///
    /// No extracted frame, or the GPU readback failed.
    pub(crate) fn render(
        &mut self,
        world: &World,
        width: u32,
        height: u32,
        flags: (bool, bool),
    ) -> Result<AovPixels, String> {
        let (reset, want_material) = flags;
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_aov").entered();
        let device = world
            .get_resource::<RenderDevice>()
            .ok_or("no render device for the AOV pass")?;
        let frame = world
            .get_resource::<ExtractedFrame>()
            .ok_or("no extracted frame for the AOV pass")?;
        let (width, height) = (width.max(1), height.max(1));
        self.frame += 1;
        if reset {
            self.history = History::default();
        }
        self.ensure_terrain(device, world.get_resource::<RuntimeRenderer>());
        let (view, proj, eye) = camera_matrices(frame, width, height);
        let view_proj = proj * view;
        let prev_view_proj = self.history.view_proj.unwrap_or(view_proj);

        // The meshes the view shows, in a fixed order (entity, fade-pair key, mesh).
        let layer_mask = frame
            .views
            .get(frame.window_views().first().map_or(0, |(index, _)| *index))
            .map_or(u32::MAX, |v| v.layer_mask);
        let mut meshes: Vec<_> = frame
            .meshes
            .iter()
            .filter(|m| {
                layer_visible(layer_mask, m.layer)
                    && !m.mesh.indices.is_empty()
                    && !shadow_only(m.lod_key)
                    && drawn_material(m.material.alpha_mode)
            })
            .collect();
        meshes.sort_by_key(|m| {
            (
                m.entity.to_bits(),
                m.object_key_xor,
                Arc::as_ptr(&m.mesh) as usize,
            )
        });
        let has_terrain = matches!(self.terrain, Some(Some(_)));
        // Only instances seen for the first time need a label.
        let mut keys: Vec<(InstanceKey, String)> = meshes
            .iter()
            .filter(|m| self.ids.instance(InstanceKey::Entity(m.entity.to_bits())) == 0)
            .map(|m| (InstanceKey::Entity(m.entity.to_bits()), format!("{:?}", m.entity)))
            .collect();
        if has_terrain {
            keys.push((InstanceKey::Terrain, "terrain".into()));
        }
        self.ids.assign_instances(&keys);

        let mut draws = Vec::with_capacity(meshes.len() + 1);
        let mut models = HashMap::with_capacity(meshes.len());
        let mut skinned_now = HashMap::new();
        for m in &meshes {
            let key = m.entity.to_bits() ^ m.object_key_xor;
            let prev_model = self.history.models.get(&key).copied().unwrap_or(m.model);
            models.insert(key, m.model);
            let mut held = prev_model == m.model;
            let mesh = self.rigid(device, &m.mesh);
            let vertex = if m.joints.is_empty() {
                VertexSource::Rigid(mesh)
            } else {
                let now = skin_positions(&m.mesh, &m.joints);
                let before = self
                    .history
                    .skinned
                    .get(&key)
                    .filter(|p| p.len() == now.len())
                    .unwrap_or(&now);
                held &= *before == now;
                let source = VertexSource::Skinned(
                    buffer(
                        device,
                        "render-out aov skinned",
                        &position_bytes(now.iter().copied()),
                        wgpu::BufferUsages::VERTEX,
                    ),
                    buffer(
                        device,
                        "render-out aov skinned prev",
                        &position_bytes(before.iter().copied()),
                        wgpu::BufferUsages::VERTEX,
                    ),
                );
                skinned_now.insert(key, now);
                source
            };
            let index = self.index_list(device, &m.mesh, m.draw_indices.as_ref());
            let mask = self.material(device, &m.material);
            let material_id = if want_material {
                self.ids.material_of(&m.material)
            } else {
                0
            };
            draws.push(Draw {
                instance: self.ids.instance(InstanceKey::Entity(m.entity.to_bits())),
                class: self.ids.class_of(&m.mesh),
                model: m.model,
                prev_model,
                held,
                vertex,
                index,
                mesh,
                mask,
                mesh_arc: Some(Arc::clone(&m.mesh)),
                material: Some(Arc::clone(&m.material)),
                material_id,
            });
        }
        if has_terrain {
            let identity = Mat4::IDENTITY.to_cols_array_2d();
            draws.push(Draw {
                instance: self.ids.instance(InstanceKey::Terrain),
                class: self.ids.terrain_class(),
                model: identity,
                prev_model: identity,
                held: true,
                vertex: VertexSource::Terrain,
                index: 0,
                mesh: 0,
                mask: None,
                mesh_arc: None,
                material: None,
                material_id: if want_material {
                    self.ids.terrain_material()
                } else {
                    0
                },
            });
        }

        let mut globals = Vec::with_capacity(GLOBALS_BYTES);
        for m in [view_proj, prev_view_proj, view, view_proj.inverse()] {
            mat_bytes(&mut globals, &m.to_cols_array_2d());
        }
        // `size.z`: the view is bit-identical to the last delivered frame's (ADR-1965).
        let view_held = if prev_view_proj == view_proj { 1.0 } else { 0.0 };
        for v in [
            eye.x,
            eye.y,
            eye.z,
            1.0,
            width as f32,
            height as f32,
            view_held,
            0.0,
        ] {
            globals.extend_from_slice(&v.to_le_bytes());
        }
        let mut instances = Vec::with_capacity(draws.len().max(1) * INSTANCE_BYTES);
        for draw in &draws {
            mat_bytes(&mut instances, &draw.model);
            mat_bytes(&mut instances, &draw.prev_model);
            for v in [draw.instance, draw.class, u32::from(draw.held), 0] {
                instances.extend_from_slice(&v.to_le_bytes());
            }
        }
        if instances.is_empty() {
            instances.resize(INSTANCE_BYTES, 0);
        }
        self.input_hash = fnv1a(&[globals.as_slice(), instances.as_slice()].concat());
        let globals = buffer(
            device,
            "render-out aov globals",
            &globals,
            wgpu::BufferUsages::UNIFORM,
        );
        let instances = buffer(
            device,
            "render-out aov instances",
            &instances,
            wgpu::BufferUsages::STORAGE,
        );
        let bind = device.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("render-out aov"),
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

        self.ensure_targets(device, width, height);
        let targets = self.targets.as_ref().ok_or("the AOV targets are missing")?;
        let ids_view = targets.ids.create_view(&wgpu::TextureViewDescriptor::default());
        let data_view = targets.data.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render-out aov"),
            });
        {
            let clear = |view| {
                Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render-out aov"),
                color_attachments: &[clear(&ids_view), clear(&data_view)],
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
            pass.set_bind_group(0, &bind, &[]);
            pass.set_pipeline(&self.sky_pipe);
            pass.draw(0..3, 0..1);
            // Whole meshes first, then the masked ones (their pipeline binds the mask too).
            let order = draws
                .iter()
                .enumerate()
                .filter(|(_, d)| d.mask.is_none())
                .chain(draws.iter().enumerate().filter(|(_, d)| d.mask.is_some()));
            let mut masked = false;
            pass.set_pipeline(&self.mesh_pipe);
            for (n, draw) in order {
                if let Some(key) = draw.mask {
                    let Some(material) = self.materials.get(&key) else {
                        continue;
                    };
                    let Some(uvs) = self.positions.get(&draw.mesh) else {
                        continue;
                    };
                    if !masked {
                        pass.set_pipeline(&self.masked_pipe);
                        masked = true;
                    }
                    pass.set_bind_group(1, &material.bind, &[]);
                    pass.set_vertex_buffer(2, uvs.uvs.slice(..));
                }
                let (index_buffer, count) = match &draw.vertex {
                    VertexSource::Terrain => match &self.terrain {
                        Some(Some(t)) => {
                            pass.set_vertex_buffer(0, t.positions.slice(..));
                            pass.set_vertex_buffer(1, t.positions.slice(..));
                            (&t.indices, t.count)
                        }
                        _ => continue,
                    },
                    VertexSource::Rigid(key) => {
                        let Some(p) = self.positions.get(key) else {
                            continue;
                        };
                        pass.set_vertex_buffer(0, p.buffer.slice(..));
                        pass.set_vertex_buffer(1, p.buffer.slice(..));
                        let Some(i) = self.indices.get(&draw.index) else {
                            continue;
                        };
                        (&i.buffer, i.count)
                    }
                    VertexSource::Skinned(now, before) => {
                        pass.set_vertex_buffer(0, now.slice(..));
                        pass.set_vertex_buffer(1, before.slice(..));
                        let Some(i) = self.indices.get(&draw.index) else {
                            continue;
                        };
                        (&i.buffer, i.count)
                    }
                };
                if count == 0 {
                    continue;
                }
                pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                let instance = n as u32;
                pass.draw_indexed(0..count, 0, instance..instance + 1);
            }
        }
        let ids_rb = copy_out(device, &mut encoder, &targets.ids, width, height, 8);
        let data_rb = copy_out(device, &mut encoder, &targets.data, width, height, 16);
        device.queue.submit([encoder.finish()]);
        let id_bytes = map_rows(device, &ids_rb, width, height, 8)?;
        let data_bytes = map_rows(device, &data_rb, width, height, 16)?;
        let ids = id_bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|b| {
                [
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                ]
            })
            .collect();
        let data = data_bytes
            .as_chunks::<16>()
            .0
            .iter()
            .map(|b| {
                let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
                [f(0), f(4), f(8), f(12)]
            })
            .collect();

        let material = if want_material {
            let mut pass = match self.material.take() {
                Some(pass) => pass,
                None => MaterialPass::new(device),
            };
            if has_terrain
                && !pass.has_terrain()
                && let Some((_, positions, indices)) =
                    Self::terrain_geometry(world.get_resource::<RuntimeRenderer>())
            {
                pass.ensure_terrain(device, &positions, &indices);
            }
            let rendered = pass.render(
                self,
                device,
                &draws,
                (&view, &view_proj),
                (width, height),
                self.frame,
            );
            self.material = Some(pass);
            Some(rendered?)
        } else {
            None
        };

        // History for the next delivered frame; evict what was not drawn for a while.
        self.history = History {
            view_proj: Some(view_proj),
            models,
            skinned: skinned_now,
        };
        let now = self.frame;
        self.positions
            .retain(|_, p| now.saturating_sub(p.last_used) < EVICT_AFTER);
        self.indices
            .retain(|_, i| now.saturating_sub(i.last_used) < EVICT_AFTER);
        self.materials
            .retain(|_, m| now.saturating_sub(m.last_used) < EVICT_AFTER);
        Ok(AovPixels { ids, data, material })
    }
}

/// Bytes per readback row, padded to the copy alignment.
fn padded_row(width: u32, texel: u32) -> u32 {
    (width * texel).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
}

fn copy_out(
    device: &RenderDevice,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    texel: u32,
) -> wgpu::Buffer {
    let padded = padded_row(width, texel);
    let readback = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("render-out aov readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    readback
}

/// Maps a padded readback and returns its rows tightly packed.
fn map_rows(
    device: &RenderDevice,
    readback: &wgpu::Buffer,
    width: u32,
    height: u32,
    texel: u32,
) -> Result<Vec<u8>, String> {
    let padded = padded_row(width, texel) as usize;
    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("GPU poll failed: {e}"))?;
    rx.recv()
        .map_err(|_| "AOV map callback dropped".to_string())?
        .map_err(|e| format!("AOV readback failed: {e}"))?;
    let mapped = readback
        .slice(..)
        .get_mapped_range()
        .map_err(|e| format!("AOV readback map failed: {e:?}"))?;
    let row = (width * texel) as usize;
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        out.extend_from_slice(&mapped[y * padded..y * padded + row]);
    }
    drop(mapped);
    readback.unmap();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::LabVertex;

    /// Audit v034 WGSL M2: the AOV pass puts no struct or array in a function-local `var`
    /// (ADR-026), which Mesa's SPIR-V validation refuses; it is outside `cargo xtask check-shaders`.
    #[test]
    fn aov_shader_has_no_function_layout_errors() {
        let found = regia_render::wgsl_function_layout_errors("aov_pass.wgsl", SHADER).expect("emits");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn skinning_matches_the_renderer_and_tolerates_short_palettes() {
        let mut v = LabVertex::from_pos_normal_uv([1.0, 2.0, 3.0], [0.0, 1.0, 0.0], [0.0; 2]);
        v.joints = [0, 5, 0, 0];
        v.weights = [0.5, 0.5, 0.0, 0.0];
        let mesh = CpuMesh {
            vertices: vec![v],
            indices: vec![0, 0, 0],
        };
        let palette = [JointMatrix::translation(2.0, 0.0, 0.0)];
        // Joint 5 is past the palette: it counts as the identity.
        let skinned = skin_positions(&mesh, &palette);
        assert_eq!(skinned, [[2.0, 2.0, 3.0]]);
        let reference = skin_position(
            [1.0, 2.0, 3.0],
            &[palette[0], JointMatrix::identity(), palette[0], palette[0]],
            [0.5, 0.5, 0.0, 0.0],
        );
        assert_eq!(skinned[0], reference);
    }

    #[test]
    fn the_aov_camera_projects_like_the_frame_camera() {
        let mut frame = ExtractedFrame::default();
        let (view, proj, eye) = camera_matrices(&frame, 200, 100);
        assert_eq!(eye, Vec3::ZERO);
        assert!(view.is_finite() && proj.is_finite());
        // A point straight ahead lands at the image center with standard Z in (0, 1).
        let clip = proj * view * regia_core::glam::Vec4::new(0.0, 0.0, -10.0, 1.0);
        let ndc = clip.truncate() / clip.w;
        assert!(ndc.x.abs() < 1e-6 && ndc.y.abs() < 1e-6, "{ndc}");
        assert!(ndc.z > 0.0 && ndc.z < 1.0);
        // View depth is the distance along the view axis: 10 m ahead.
        assert!((-(view * regia_core::glam::Vec4::new(0.0, 0.0, -10.0, 1.0)).z - 10.0).abs() < 1e-5);
        frame.quality.horizontal_plus = true;
        let (_, wide, _) = camera_matrices(&frame, 200, 100);
        assert!(wide.is_finite());
    }

    #[test]
    fn delivered_aovs_are_hashed_in_request_order_and_trimmed_with_the_frames() {
        use super::super::aov::{Aov, AovPlanes};
        let request = Request::parse(&json!({
            "out": "/o", "frames": 10, "width": 1, "height": 1, "aovs": ["id", "depth"], "cuts": [2, 9]
        }))
        .expect("request");
        let mut state = AovState::default();
        for frame in 0..3u32 {
            state.pending = vec![
                AovFile {
                    aov: Aov::Depth,
                    planes: AovPlanes::F32(vec![vec![frame as f32]]),
                },
                AovFile {
                    aov: Aov::Id,
                    planes: AovPlanes::U32(vec![vec![7], vec![1]]),
                },
            ];
            assert_eq!(state.take().len(), 2);
            assert!(state.pending.is_empty(), "taken once");
        }
        assert!(state.take().is_empty(), "nothing pending, nothing hashed");
        let m = state.manifest(&request, 3);
        let depth = m["frame_hashes"]["depth"].as_array().expect("depth hashes");
        let ids = m["frame_hashes"]["id"].as_array().expect("id hashes");
        assert_eq!((depth.len(), ids.len()), (3, 3));
        assert_ne!(depth[0], depth[1], "the depth changed");
        assert_eq!(ids[0], ids[2], "the ids did not");
        assert_eq!(
            depth[0],
            json!(format!("{:016x}", fnv1a(&0.0f32.to_le_bytes()))),
            "the hash is FNV-1a of the raw file bytes"
        );
        assert_eq!(
            m["motion_reset_frames"],
            json!([0, 2]),
            "cuts past the delivered run are left out"
        );
        assert_eq!(m["files"], json!(6));
        state.truncate(1);
        assert_eq!(
            state.manifest(&request, 1)["frame_hashes"]["id"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        let none = Request::parse(&json!({ "out": "/o", "frames": 1 })).expect("no aovs");
        assert_eq!(AovState::default().manifest(&none, 1), Value::Null);
    }

    #[test]
    fn readback_rows_are_padded_to_the_copy_alignment() {
        assert_eq!(padded_row(1, 8), 256);
        assert_eq!(padded_row(32, 8), 256);
        assert_eq!(padded_row(33, 8), 512);
        assert_eq!(padded_row(16, 16), 256);
        assert_eq!(padded_row(17, 16), 512);
    }

    #[test]
    fn layer_masks_hide_meshes_like_the_frame_does() {
        assert!(layer_visible(1, 0));
        assert!(!layer_visible(1, 1));
        assert!(!layer_visible(u32::MAX, 32), "layers past 31 never show");
        assert!(shadow_only(regia_render::mesh_lod_cache_key(0, true)));
        assert!(!shadow_only(regia_render::mesh_lod_cache_key(2, false)));
        assert!(!shadow_only(0));
        assert!(drawn_material(ALPHA_OPAQUE) && drawn_material(ALPHA_MASK));
        for translucent in [
            regia_render::ALPHA_BLEND,
            regia_render::ALPHA_PREMULTIPLIED,
            regia_render::ALPHA_ADDITIVE,
        ] {
            assert!(!drawn_material(translucent));
        }
    }
}
