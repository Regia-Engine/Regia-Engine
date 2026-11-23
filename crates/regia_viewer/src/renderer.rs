// SPDX-License-Identifier: MPL-2.0
//! The viewer's one forward pass.
//!
//! Everything a pixel needs is bound through uniform buffers and textures. All pipelines are built when
//! the renderer is created; switching a material writes one block, so [`ViewerStats::pipelines_created`]
//! stays constant for the renderer's life. Frames are lazy: state changes mark the renderer dirty, and
//! [`ViewerRenderer::render_if_needed`] draws nothing when nothing changed.

use glam::Affine3A;
use wgpu::util::DeviceExt;

use crate::ViewerError;
use crate::camera::ViewerCamera;
use crate::env::BakedEnv;
use crate::hooks::{ShaderHooks, contact_shadow_source};
use crate::material::{LutHandle, MATERIAL_BLOCK_SIZE, ShaderFamily, ViewerMaterial};
use crate::mesh::{VERTEX_STRIDE, ViewerBounds, ViewerMeshData, ViewerVertex};

/// Width of a lookup row in the atlas.
pub const LUT_WIDTH: u32 = 64;
/// How many lookups the atlas holds.
pub const LUT_ROWS: u32 = 64;
/// Width and height of the split-sum table.
pub const DFG_SIZE: u32 = 32;
/// The split-sum table baked by `regia_viewer_bake_env --dfg`.
pub const DFG_TABLE: &[u8] = include_bytes!("../assets/dfg32.bin");

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;
const INSTANCE_STRIDE: u64 = 48;
const LOOK_TEXTURE_SLOTS: usize = 2;

/// The frame block of `prelude.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FrameBlock {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    sh: [[f32; 4]; 9],
    env: [f32; 4],
    shadow_center: [f32; 4],
    shadow_extent: [f32; 4],
    key_dir: [[f32; 4]; 3],
    key_col: [[f32; 4]; 3],
}

/// How the renderer is set up. Changing any of this means a new renderer.
#[derive(Debug, Clone)]
pub struct ViewerConfig {
    /// The format of the textures [`ViewerRenderer::render`] receives. With a format that has no
    /// hardware sRGB conversion the shader encodes sRGB itself.
    pub target_format: wgpu::TextureFormat,
    /// 1 (off) or 4, usually from [`crate::gpu::sample_count_for`].
    pub sample_count: u32,
    /// The shader hooks.
    pub hooks: ShaderHooks,
    /// Compiled pipelines from an earlier run, or `None` to compile them all. Needs a device with
    /// `PIPELINE_CACHE` (`crate::gpu::request_viewer_device_with_cache`).
    pub pipeline_cache: Option<wgpu::PipelineCache>,
}

impl ViewerConfig {
    /// 4x MSAA and the built-in hooks, for a device known to support them.
    #[must_use]
    pub fn new(target_format: wgpu::TextureFormat) -> Self {
        Self {
            target_format,
            sample_count: 4,
            hooks: ShaderHooks::default(),
            pipeline_cache: None,
        }
    }
}

/// The optional soft contact shadow: a radial falloff over the piece's footprint on a plane behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactShadow {
    /// The center of the footprint on the plane, in world millimeters. The plane faces +Z.
    pub center_mm: [f32; 3],
    /// Half width and half height of the footprint.
    pub half_extent_mm: [f32; 2],
    /// Width of the soft edge.
    pub softness_mm: f32,
    /// How dark the middle is, 0 to 1.
    pub opacity: f32,
}

impl ContactShadow {
    /// A shadow under a piece with these bounds, just behind its back. Pass the bounds of the visible parts:
    /// a hidden post through the surface would push the plane far behind the piece.
    #[must_use]
    pub fn under(bounds: &ViewerBounds, opacity: f32) -> Self {
        let width = bounds.max[0] - bounds.min[0];
        let height = bounds.max[1] - bounds.min[1];
        let depth = (bounds.max[2] - bounds.min[2]).max(0.1);
        let softness = width.max(height).max(0.1) * 0.12;
        Self {
            center_mm: [
                (bounds.min[0] + bounds.max[0]) * 0.5,
                (bounds.min[1] + bounds.max[1]) * 0.5,
                bounds.min[2] - depth * 0.02,
            ],
            half_extent_mm: [width * 0.5, height * 0.5],
            softness_mm: softness,
            opacity,
        }
    }
}

/// A fixed studio light for a hook's glints (`frame.key_dir`, `frame.key_col`). The environment already
/// holds the soft light; these give a stone's facets a sharp source. Lights stay fixed: the camera moves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyLight {
    /// The direction toward the light, any length.
    pub dir: [f32; 3],
    /// Linear radiance.
    pub radiance: [f32; 3],
}

/// The default key lights: the default studio's key, fill and top strip.
pub const DEFAULT_KEY_LIGHTS: [KeyLight; 3] = [
    KeyLight {
        dir: [-0.55, 0.55, 0.65],
        radiance: [6.5, 6.4, 6.2],
    },
    KeyLight {
        dir: [0.82, 0.10, 0.55],
        radiance: [2.6, 2.6, 2.7],
    },
    KeyLight {
        dir: [0.0, 0.93, 0.36],
        radiance: [8.0, 8.0, 8.0],
    },
];

/// How the frame looks around the piece.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewerStyle {
    /// A multiplier on the light before tone mapping.
    pub exposure: f32,
    /// The clear color: linear, premultiplied. Alpha 0 shows the page behind a transparent canvas.
    pub clear_color: [f32; 4],
    /// The contact shadow, if any.
    pub contact_shadow: Option<ContactShadow>,
    /// The fixed studio lights a hook may use for glints.
    pub key_lights: [KeyLight; 3],
}

impl Default for ViewerStyle {
    fn default() -> Self {
        Self {
            exposure: 1.0,
            clear_color: [0.86, 0.86, 0.87, 1.0],
            contact_shadow: None,
            key_lights: DEFAULT_KEY_LIGHTS,
        }
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// Replaces channels that are not finite. Finite numbers, including negatives, stay so a host can
/// still pass a dark clear color or a zero shadow. Exposure and opacity are then clamped.
fn sanitize_style(style: &ViewerStyle) -> ViewerStyle {
    let defaults = ViewerStyle::default();
    ViewerStyle {
        exposure: finite_or(style.exposure, defaults.exposure).max(0.0),
        clear_color: [
            finite_or(style.clear_color[0], defaults.clear_color[0]),
            finite_or(style.clear_color[1], defaults.clear_color[1]),
            finite_or(style.clear_color[2], defaults.clear_color[2]),
            finite_or(style.clear_color[3], defaults.clear_color[3]),
        ],
        contact_shadow: style.contact_shadow.map(|shadow| ContactShadow {
            center_mm: [
                finite_or(shadow.center_mm[0], 0.0),
                finite_or(shadow.center_mm[1], 0.0),
                finite_or(shadow.center_mm[2], 0.0),
            ],
            half_extent_mm: [
                finite_or(shadow.half_extent_mm[0], 0.0),
                finite_or(shadow.half_extent_mm[1], 0.0),
            ],
            softness_mm: finite_or(shadow.softness_mm, 0.1).max(0.0),
            opacity: finite_or(shadow.opacity, 0.0).clamp(0.0, 1.0),
        }),
        key_lights: style.key_lights.map(|light| KeyLight {
            dir: if light.dir.iter().all(|c| c.is_finite()) {
                light.dir
            } else {
                [0.0, 0.0, 1.0]
            },
            radiance: [
                finite_or(light.radiance[0], 0.0),
                finite_or(light.radiance[1], 0.0),
                finite_or(light.radiance[2], 0.0),
            ],
        }),
    }
}

/// Monotonic counters, for tests and for the agent surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewerStats {
    /// Render pipelines built since the renderer was created.
    pub pipelines_created: u64,
    /// Frames drawn.
    pub frames_drawn: u64,
    /// Draw calls issued.
    pub draw_calls: u64,
    /// Instances drawn.
    pub instances_drawn: u64,
    /// Triangles drawn.
    pub triangles_drawn: u64,
    /// Pipeline binds issued.
    pub pipeline_binds: u64,
    /// Material blocks written to the GPU.
    pub material_writes: u64,
    /// Buffers created (meshes, growth).
    pub buffers_created: u64,
    /// Bind groups created.
    pub bind_groups_created: u64,
}

/// A mesh on the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MeshHandle {
    slot: u32,
    generation: u32,
}

/// A material slot. Every draw that uses it follows [`ViewerRenderer::set_material`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaterialHandle {
    slot: u32,
    generation: u32,
}

/// One mesh under one material at one or more transforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DrawHandle {
    slot: u32,
    generation: u32,
}

/// A vector of slots with reuse and generations, so a stale handle is an error and not another object.
#[derive(Debug)]
struct Slots<T> {
    items: Vec<(u32, Option<T>)>,
    free: Vec<u32>,
}

impl<T> Slots<T> {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            free: Vec::new(),
        }
    }

    fn insert(&mut self, value: T) -> (u32, u32) {
        if let Some(slot) = self.free.pop() {
            let entry = &mut self.items[slot as usize];
            entry.1 = Some(value);
            (slot, entry.0)
        } else {
            self.items.push((0, Some(value)));
            ((self.items.len() - 1) as u32, 0)
        }
    }

    fn get(&self, slot: u32, generation: u32) -> Option<&T> {
        let entry = self.items.get(slot as usize)?;
        if entry.0 == generation {
            entry.1.as_ref()
        } else {
            None
        }
    }

    fn get_mut(&mut self, slot: u32, generation: u32) -> Option<&mut T> {
        let entry = self.items.get_mut(slot as usize)?;
        if entry.0 == generation {
            entry.1.as_mut()
        } else {
            None
        }
    }

    fn remove(&mut self, slot: u32, generation: u32) -> Option<T> {
        let entry = self.items.get_mut(slot as usize)?;
        if entry.0 != generation {
            return None;
        }
        let value = entry.1.take()?;
        entry.0 = entry.0.wrapping_add(1);
        self.free.push(slot);
        Some(value)
    }

    /// The value in a slot, whatever its generation (for internal lists rebuilt on every change).
    fn at(&self, slot: u32) -> Option<&T> {
        self.items.get(slot as usize)?.1.as_ref()
    }

    fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.1.as_ref().map(|v| (i as u32, v)))
    }
}

#[derive(Debug)]
struct GpuMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
    bounds: ViewerBounds,
}

#[derive(Debug)]
struct MaterialSlot {
    value: ViewerMaterial,
}

#[derive(Debug)]
struct DrawItem {
    mesh: MeshHandle,
    material: MaterialHandle,
    /// Transforms that keep their winding, then the mirrored ones.
    transforms: Vec<Affine3A>,
    mirrored_from: usize,
}

/// One contiguous instanced draw, in draw order.
#[derive(Debug, Clone, Copy)]
struct Batch {
    pipeline: usize,
    material_offset: u32,
    mesh_slot: u32,
    first_instance: u32,
    instances: u32,
    index_count: u32,
}

#[derive(Debug)]
struct Attachments {
    size: (u32, u32),
    depth: wgpu::TextureView,
    color: Option<wgpu::TextureView>,
}

/// The product viewer's renderer. The host owns the device's surface; on Android the surface can die and
/// come back while this stays alive, because the renderer only ever sees the texture it is handed.
#[derive(Debug)]
pub struct ViewerRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    target_format: wgpu::TextureFormat,
    sample_count: u32,
    encode_srgb: bool,

    frame_layout: wgpu::BindGroupLayout,
    material_layout: wgpu::BindGroupLayout,
    look_layout: wgpu::BindGroupLayout,
    /// family index * 2 + mirrored, then the contact shadow.
    pipelines: Vec<wgpu::RenderPipeline>,

    frame_buffer: wgpu::Buffer,
    env_sampler: wgpu::Sampler,
    clamp_sampler: wgpu::Sampler,
    look_sampler: wgpu::Sampler,
    env_view: wgpu::TextureView,
    env_last_mip: f32,
    env_sh: [[f32; 4]; 9],
    dfg_view: wgpu::TextureView,
    lut_texture: wgpu::Texture,
    lut_view: wgpu::TextureView,
    lut_count: u32,
    look_views: [wgpu::TextureView; LOOK_TEXTURE_SLOTS],

    frame_group: wgpu::BindGroup,
    material_group: wgpu::BindGroup,
    look_group: wgpu::BindGroup,

    material_buffer: wgpu::Buffer,
    material_capacity: u32,
    material_stride: u32,

    instance_buffer: wgpu::Buffer,
    instance_capacity: u64,

    meshes: Slots<GpuMesh>,
    materials: Slots<MaterialSlot>,
    draws: Slots<DrawItem>,

    camera: ViewerCamera,
    style: ViewerStyle,
    attachments: Option<Attachments>,

    batches: Vec<Batch>,
    batches_dirty: bool,
    dirty: bool,
    stats: ViewerStats,
}

fn linear_sampler(device: &wgpu::Device, label: &str, mipmaps: bool) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: if mipmaps {
            wgpu::MipmapFilterMode::Linear
        } else {
            wgpu::MipmapFilterMode::Nearest
        },
        ..Default::default()
    })
}

fn float_texture_entry(binding: u32, dimension: wgpu::TextureViewDimension) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: dimension,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn white_texture(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::TextureView {
    let texture = device.create_texture_with_data(
        queue,
        &wgpu::TextureDescriptor {
            label: Some("viewer look default"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::LayerMajor,
        &[255, 255, 255, 255],
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn env_texture(device: &wgpu::Device, queue: &wgpu::Queue, env: &BakedEnv) -> wgpu::TextureView {
    let size = env.face_size();
    let mips = env.mip_count();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewer environment"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 6,
        },
        mip_level_count: mips,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgb9e5Ufloat,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for mip in 0..mips {
        let level = (size >> mip).max(1);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: mip,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(env.mip_texels(mip)),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(level * 4),
                rows_per_image: Some(level),
            },
            wgpu::Extent3d {
                width: level,
                height: level,
                depth_or_array_layers: 6,
            },
        );
    }
    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("viewer environment"),
        dimension: Some(wgpu::TextureViewDimension::Cube),
        ..Default::default()
    })
}

fn sh_block(env: &BakedEnv) -> [[f32; 4]; 9] {
    let mut out = [[0.0; 4]; 9];
    for (block, c) in out.iter_mut().zip(env.sh()) {
        *block = [c[0], c[1], c[2], 0.0];
    }
    out
}

fn material_stride(device: &wgpu::Device) -> u32 {
    let align = device.limits().min_uniform_buffer_offset_alignment.max(1);
    (MATERIAL_BLOCK_SIZE as u32).div_ceil(align) * align
}

/// The clear color as the target stores it: a target without hardware sRGB gets encoded values, so
/// the picture is the same on either. The color is premultiplied; in encode mode the alpha is
/// encoded with the same curve, otherwise `enc(rgb) > alpha` on a translucent clear and the data
/// is not valid premultiplied for a compositing page.
pub(crate) fn clear_value(clear_color: [f32; 4], encode_srgb: bool) -> [f32; 4] {
    let [r, g, b, a] = clear_color;
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    if encode_srgb {
        let encode = |v: f32| {
            let v = finite(v).clamp(0.0, 1.0);
            if v <= 0.003_130_8 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            }
        };
        [encode(r), encode(g), encode(b), encode(a)]
    } else {
        [finite(r), finite(g), finite(b), finite(a)]
    }
}

impl ViewerRenderer {
    /// Creates a renderer on a device and builds every pipeline. The default studio is the environment
    /// until [`ViewerRenderer::set_environment`].
    ///
    /// # Errors
    /// [`ViewerError::Config`] when the sample count is neither 1 nor 4.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, config: ViewerConfig) -> Result<Self, ViewerError> {
        let _span = tracing::info_span!("viewer_renderer_new").entered();
        if !matches!(config.sample_count, 1 | 4) {
            return Err(ViewerError::Config(format!(
                "sample count {} (use 1 or 4)",
                config.sample_count
            )));
        }
        let mut stats = ViewerStats::default();

        let frame_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer frame"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<FrameBlock>() as u64),
                    },
                    count: None,
                },
                float_texture_entry(1, wgpu::TextureViewDimension::Cube),
                sampler_entry(2),
                float_texture_entry(3, wgpu::TextureViewDimension::D2),
                float_texture_entry(4, wgpu::TextureViewDimension::D2),
                sampler_entry(5),
            ],
        });
        let material_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer material"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(MATERIAL_BLOCK_SIZE),
                },
                count: None,
            }],
        });
        let look_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer look"),
            entries: &[
                float_texture_entry(0, wgpu::TextureViewDimension::D2),
                float_texture_entry(1, wgpu::TextureViewDimension::D2),
                sampler_entry(2),
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewer"),
            bind_group_layouts: &[Some(&frame_layout), Some(&material_layout), Some(&look_layout)],
            immediate_size: 0,
        });
        let pipelines = build_pipelines(&device, &layout, &config, &mut stats);

        let frame_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer frame"),
            size: std::mem::size_of::<FrameBlock>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        stats.buffers_created += 1;

        let env_sampler = linear_sampler(&device, "viewer environment", true);
        let clamp_sampler = linear_sampler(&device, "viewer table", false);
        let look_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viewer look"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let env = BakedEnv::studio_default();
        let env_view = env_texture(&device, &queue, &env);

        let dfg = device.create_texture_with_data(
            &queue,
            &wgpu::TextureDescriptor {
                label: Some("viewer dfg"),
                size: wgpu::Extent3d {
                    width: DFG_SIZE,
                    height: DFG_SIZE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            DFG_TABLE,
        );
        let dfg_view = dfg.create_view(&wgpu::TextureViewDescriptor::default());

        let lut_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer lookups"),
            size: wgpu::Extent3d {
                width: LUT_WIDTH,
                height: LUT_ROWS,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let lut_view = lut_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let look_views = [white_texture(&device, &queue), white_texture(&device, &queue)];

        let material_capacity = 16;
        let stride = material_stride(&device);
        let material_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer materials"),
            size: u64::from(material_capacity) * u64::from(stride),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        stats.buffers_created += 1;
        let instance_capacity = 64;
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer instances"),
            size: instance_capacity * INSTANCE_STRIDE,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        stats.buffers_created += 1;

        let frame_group = create_frame_group(
            &device,
            &frame_layout,
            &frame_buffer,
            &env_view,
            &env_sampler,
            &dfg_view,
            &lut_view,
            &clamp_sampler,
        );
        let material_group = create_material_group(&device, &material_layout, &material_buffer);
        let look_group = create_look_group(&device, &look_layout, &look_views, &look_sampler);
        stats.bind_groups_created += 3;

        let encode_srgb = !config.target_format.is_srgb();
        let env_last_mip = (env.mip_count() - 1) as f32;
        let env_sh = sh_block(&env);

        Ok(Self {
            device,
            queue,
            target_format: config.target_format,
            sample_count: config.sample_count,
            encode_srgb,
            frame_layout,
            material_layout,
            look_layout,
            pipelines,
            frame_buffer,
            env_sampler,
            clamp_sampler,
            look_sampler,
            env_view,
            env_last_mip,
            env_sh,
            dfg_view,
            lut_texture,
            lut_view,
            lut_count: 0,
            look_views,
            frame_group,
            material_group,
            look_group,
            material_buffer,
            material_capacity,
            material_stride: stride,
            instance_buffer,
            instance_capacity,
            meshes: Slots::new(),
            materials: Slots::new(),
            draws: Slots::new(),
            camera: ViewerCamera::default(),
            style: ViewerStyle::default(),
            attachments: None,
            batches: Vec::new(),
            batches_dirty: true,
            dirty: true,
            stats,
        })
    }

    /// The device.
    #[must_use]
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The queue.
    #[must_use]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The target format the pipelines were built for.
    #[must_use]
    pub fn target_format(&self) -> wgpu::TextureFormat {
        self.target_format
    }

    /// The sample count the pipelines were built for.
    #[must_use]
    pub fn sample_count(&self) -> u32 {
        self.sample_count
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> ViewerStats {
        self.stats
    }

    /// The fixed number of render pipelines: two windings per family, plus the contact shadow.
    #[must_use]
    pub fn pipeline_count(&self) -> usize {
        self.pipelines.len()
    }

    /// True when the next [`ViewerRenderer::render_if_needed`] will draw.
    #[must_use]
    pub fn needs_redraw(&self) -> bool {
        self.dirty
    }

    /// Asks for a frame even though no state changed, for example after the surface came back.
    pub fn request_redraw(&mut self) {
        self.dirty = true;
    }

    /// Replaces the environment. No pipeline is touched.
    pub fn set_environment(&mut self, env: &BakedEnv) {
        let _span = tracing::info_span!("viewer_set_environment").entered();
        self.env_view = env_texture(&self.device, &self.queue, env);
        self.env_last_mip = (env.mip_count() - 1) as f32;
        self.env_sh = sh_block(env);
        self.refresh_frame_group();
        self.dirty = true;
    }

    /// Uploads a mesh.
    ///
    /// # Errors
    /// [`ViewerError::BadMesh`] when the data fails [`ViewerMeshData::validate`].
    pub fn upload_mesh(&mut self, data: &ViewerMeshData) -> Result<MeshHandle, ViewerError> {
        let _span = tracing::info_span!("viewer_upload_mesh").entered();
        data.validate()?;
        let vertices = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewer mesh vertices"),
            contents: bytemuck::cast_slice::<ViewerVertex, u8>(&data.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let indices = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewer mesh indices"),
            contents: bytemuck::cast_slice::<u32, u8>(&data.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        self.stats.buffers_created += 2;
        let (slot, generation) = self.meshes.insert(GpuMesh {
            vertices,
            indices,
            index_count: data.indices.len() as u32,
            bounds: data.bounds(),
        });
        Ok(MeshHandle { slot, generation })
    }

    /// Frees a mesh. A draw still using it is an error at the next frame's preparation, so remove those
    /// draws first.
    ///
    /// # Errors
    /// [`ViewerError::StaleHandle`] when it is already gone, [`ViewerError::InUse`] when a draw uses it.
    pub fn remove_mesh(&mut self, mesh: MeshHandle) -> Result<(), ViewerError> {
        if self.draws.iter().any(|(_, d)| d.mesh == mesh) {
            return Err(ViewerError::InUse("a draw uses this mesh"));
        }
        self.meshes
            .remove(mesh.slot, mesh.generation)
            .map(|_| ())
            .ok_or(ViewerError::StaleHandle("mesh"))
    }

    /// Creates a material slot.
    pub fn create_material(&mut self, material: &ViewerMaterial) -> MaterialHandle {
        let (slot, generation) = self.materials.insert(MaterialSlot { value: *material });
        self.ensure_material_capacity(slot + 1);
        self.write_material(slot, material);
        self.batches_dirty = true;
        self.dirty = true;
        MaterialHandle { slot, generation }
    }

    /// Switches a material: one block write, and at most another prebuilt pipeline. An identical value
    /// does nothing and asks for no frame.
    ///
    /// # Errors
    /// [`ViewerError::StaleHandle`] when the handle is gone.
    pub fn set_material(
        &mut self,
        handle: MaterialHandle,
        material: &ViewerMaterial,
    ) -> Result<(), ViewerError> {
        let entry = self
            .materials
            .get_mut(handle.slot, handle.generation)
            .ok_or(ViewerError::StaleHandle("material"))?;
        if entry.value == *material {
            return Ok(());
        }
        let family_changed = entry.value.family() != material.family();
        entry.value = *material;
        self.write_material(handle.slot, material);
        if family_changed {
            self.batches_dirty = true;
        }
        self.dirty = true;
        Ok(())
    }

    /// Frees a material slot.
    ///
    /// # Errors
    /// [`ViewerError::StaleHandle`] when it is already gone, [`ViewerError::InUse`] when a draw uses it.
    pub fn remove_material(&mut self, handle: MaterialHandle) -> Result<(), ViewerError> {
        if self.draws.iter().any(|(_, d)| d.material == handle) {
            return Err(ViewerError::InUse("a draw uses this material"));
        }
        self.materials
            .remove(handle.slot, handle.generation)
            .map(|_| ())
            .ok_or(ViewerError::StaleHandle("material"))
    }

    /// Adds a baked lookup to the atlas: 2 or more linear colors from one end of the coordinate to the
    /// other, resampled to one row. Used for an anodize film's cos(theta) table.
    ///
    /// # Errors
    /// [`ViewerError::LutFull`] when all rows are taken, [`ViewerError::BadLut`] for too few entries.
    pub fn add_lut(&mut self, entries: &[[f32; 3]]) -> Result<LutHandle, ViewerError> {
        if entries.len() < 2 {
            return Err(ViewerError::BadLut(entries.len()));
        }
        if self.lut_count >= LUT_ROWS {
            return Err(ViewerError::LutFull(LUT_ROWS));
        }
        let row = self.lut_count;
        let mut bytes = Vec::with_capacity(LUT_WIDTH as usize * 4);
        for x in 0..LUT_WIDTH {
            let t = x as f32 / (LUT_WIDTH - 1) as f32 * (entries.len() - 1) as f32;
            let low = (t as usize).min(entries.len() - 2);
            let f = t - low as f32;
            for (a, b) in entries[low].iter().zip(&entries[low + 1]) {
                bytes.push(linear_to_srgb8(a * (1.0 - f) + b * f));
            }
            bytes.push(255);
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.lut_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: row, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(LUT_WIDTH * 4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: LUT_WIDTH,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        self.lut_count += 1;
        self.dirty = true;
        Ok(LutHandle(row))
    }

    /// Rows of the lookup atlas in use ([`ViewerRenderer::add_lut`]), at most [`LUT_ROWS`].
    #[must_use]
    pub fn lut_rows(&self) -> u32 {
        self.lut_count
    }

    /// Frees every row of the lookup atlas, so the next [`ViewerRenderer::add_lut`] starts at row 0 again.
    /// A host that shows one product after another calls this when it removes the old product's
    /// materials: without it the atlas fills after [`LUT_ROWS`] lookups and every later one is
    /// [`ViewerError::LutFull`]. A material still drawn with an old row shows whatever the row holds
    /// next, so remove or reset those materials first.
    pub fn clear_luts(&mut self) {
        self.lut_count = 0;
    }

    /// Sets a look's texture (`look_tex0` or `look_tex1`): RGBA8, not sRGB-decoded. No pipeline is touched.
    ///
    /// # Errors
    /// [`ViewerError::Config`] for a slot other than 0 or 1, a size this device cannot store, or
    /// pixel data of the wrong length.
    pub fn set_look_texture(
        &mut self,
        slot: usize,
        width: u32,
        height: u32,
        rgba8: &[u8],
    ) -> Result<(), ViewerError> {
        if slot >= LOOK_TEXTURE_SLOTS {
            return Err(ViewerError::Config(format!(
                "look texture slot {slot} (use 0 or 1)"
            )));
        }
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4));
        let largest = self.device.limits().max_texture_dimension_2d;
        if width == 0
            || height == 0
            || width > largest
            || height > largest
            || pixels.is_none_or(|n| rgba8.len() != n)
        {
            return Err(ViewerError::Config(format!(
                "look texture of {width}x{height} with {} bytes",
                rgba8.len()
            )));
        }
        let texture = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: Some("viewer look"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            rgba8,
        );
        self.look_views[slot] = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.look_group = create_look_group(
            &self.device,
            &self.look_layout,
            &self.look_views,
            &self.look_sampler,
        );
        self.stats.bind_groups_created += 1;
        self.dirty = true;
        Ok(())
    }

    /// Adds a draw: one mesh, one material, one or more transforms.
    ///
    /// # Errors
    /// [`ViewerError::StaleHandle`] for a mesh or material that is gone, [`ViewerError::Config`] for no
    /// transforms.
    pub fn add_draw(
        &mut self,
        mesh: MeshHandle,
        material: MaterialHandle,
        transforms: &[Affine3A],
    ) -> Result<DrawHandle, ViewerError> {
        self.meshes
            .get(mesh.slot, mesh.generation)
            .ok_or(ViewerError::StaleHandle("mesh"))?;
        self.materials
            .get(material.slot, material.generation)
            .ok_or(ViewerError::StaleHandle("material"))?;
        let (ordered, mirrored_from) = split_mirrored(transforms)?;
        let (slot, generation) = self.draws.insert(DrawItem {
            mesh,
            material,
            transforms: ordered,
            mirrored_from,
        });
        self.batches_dirty = true;
        self.dirty = true;
        Ok(DrawHandle { slot, generation })
    }

    /// Points a draw at another material slot (a slot change, not a value change).
    ///
    /// # Errors
    /// [`ViewerError::StaleHandle`] when a handle is gone.
    pub fn set_draw_material(
        &mut self,
        draw: DrawHandle,
        material: MaterialHandle,
    ) -> Result<(), ViewerError> {
        self.materials
            .get(material.slot, material.generation)
            .ok_or(ViewerError::StaleHandle("material"))?;
        let item = self
            .draws
            .get_mut(draw.slot, draw.generation)
            .ok_or(ViewerError::StaleHandle("draw"))?;
        if item.material != material {
            item.material = material;
            self.batches_dirty = true;
            self.dirty = true;
        }
        Ok(())
    }

    /// Swaps a draw's mesh (a part swap).
    ///
    /// # Errors
    /// [`ViewerError::StaleHandle`] when a handle is gone.
    pub fn set_draw_mesh(&mut self, draw: DrawHandle, mesh: MeshHandle) -> Result<(), ViewerError> {
        self.meshes
            .get(mesh.slot, mesh.generation)
            .ok_or(ViewerError::StaleHandle("mesh"))?;
        let item = self
            .draws
            .get_mut(draw.slot, draw.generation)
            .ok_or(ViewerError::StaleHandle("draw"))?;
        if item.mesh != mesh {
            item.mesh = mesh;
            self.batches_dirty = true;
            self.dirty = true;
        }
        Ok(())
    }

    /// Moves a draw.
    ///
    /// # Errors
    /// [`ViewerError::StaleHandle`] when the handle is gone, [`ViewerError::Config`] for no transforms.
    pub fn set_draw_transforms(
        &mut self,
        draw: DrawHandle,
        transforms: &[Affine3A],
    ) -> Result<(), ViewerError> {
        let (ordered, mirrored_from) = split_mirrored(transforms)?;
        let item = self
            .draws
            .get_mut(draw.slot, draw.generation)
            .ok_or(ViewerError::StaleHandle("draw"))?;
        if item.transforms == ordered && item.mirrored_from == mirrored_from {
            return Ok(());
        }
        item.transforms = ordered;
        item.mirrored_from = mirrored_from;
        self.batches_dirty = true;
        self.dirty = true;
        Ok(())
    }

    /// Removes a draw.
    ///
    /// # Errors
    /// [`ViewerError::StaleHandle`] when it is already gone.
    pub fn remove_draw(&mut self, draw: DrawHandle) -> Result<(), ViewerError> {
        self.draws
            .remove(draw.slot, draw.generation)
            .ok_or(ViewerError::StaleHandle("draw"))?;
        self.batches_dirty = true;
        self.dirty = true;
        Ok(())
    }

    /// The box around every draw, if there are any.
    #[must_use]
    pub fn bounds(&self) -> Option<ViewerBounds> {
        let mut out: Option<ViewerBounds> = None;
        for (_, draw) in self.draws.iter() {
            let Some(mesh) = self.meshes.get(draw.mesh.slot, draw.mesh.generation) else {
                continue;
            };
            for transform in &draw.transforms {
                let b = mesh.bounds.transformed(transform);
                out = Some(out.map_or(b, |o| o.union(&b)));
            }
        }
        out
    }

    /// Moves the camera. An identical camera asks for no frame.
    ///
    /// # Errors
    /// [`ViewerError::BadCamera`] when a value is not finite, the eye sits on the target, or the view
    /// direction is parallel to up.
    pub fn set_camera(&mut self, camera: &ViewerCamera) -> Result<(), ViewerError> {
        let lens = [camera.fov_deg, camera.near_mm, camera.far_mm];
        let mut values = camera
            .eye
            .iter()
            .chain(&camera.target)
            .chain(&camera.up)
            .chain(&lens);
        if values.any(|v| !v.is_finite()) {
            return Err(ViewerError::BadCamera("a value is not finite"));
        }
        let forward = glam::Vec3::from(camera.target) - glam::Vec3::from(camera.eye);
        if forward.length_squared() < 1e-10 {
            return Err(ViewerError::BadCamera("the eye sits on the target"));
        }
        if forward
            .normalize()
            .cross(glam::Vec3::from(camera.up))
            .length_squared()
            < 1e-10
        {
            return Err(ViewerError::BadCamera("the view direction is parallel to up"));
        }
        if self.camera != *camera {
            self.camera = *camera;
            self.dirty = true;
        }
        Ok(())
    }

    /// The camera.
    #[must_use]
    pub fn camera(&self) -> &ViewerCamera {
        &self.camera
    }

    /// Sets exposure, clear color and contact shadow. An identical style asks for no frame.
    /// A channel that is not finite is replaced before it is stored, so it cannot reach the frame
    /// uniform. Exposure is stored at least 0. Other finite values are kept, including a clear color
    /// outside 0 to 1.
    pub fn set_style(&mut self, style: &ViewerStyle) {
        let style = sanitize_style(style);
        if self.style != style {
            self.style = style;
            self.dirty = true;
        }
    }

    /// The style.
    #[must_use]
    pub fn style(&self) -> &ViewerStyle {
        &self.style
    }

    /// Draws when something changed since the last frame, or the target's size differs from the last frame's;
    /// returns whether it drew.
    ///
    /// # Errors
    /// As [`ViewerRenderer::render`].
    pub fn render_if_needed(&mut self, target: &wgpu::Texture) -> Result<bool, ViewerError> {
        // A target of another size (a resized canvas or surface) has never seen this picture.
        let resized = self
            .attachments
            .as_ref()
            .is_some_and(|a| a.size != (target.width(), target.height()));
        if !self.dirty && !resized {
            return Ok(false);
        }
        self.render(target)?;
        Ok(true)
    }

    /// Draws one frame to `target` and submits it. The target's format must be
    /// [`ViewerRenderer::target_format`].
    ///
    /// # Errors
    /// [`ViewerError::TargetFormat`] for another format.
    pub fn render(&mut self, target: &wgpu::Texture) -> Result<(), ViewerError> {
        let _span = tracing::info_span!("viewer_render").entered();
        if target.format() != self.target_format {
            return Err(ViewerError::TargetFormat {
                expected: self.target_format,
                got: target.format(),
            });
        }
        let (width, height) = (target.width().max(1), target.height().max(1));
        self.ensure_attachments(width, height);
        self.write_frame(width, height);
        if self.batches_dirty {
            self.rebuild_batches();
        }

        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let Some(attachments) = self.attachments.as_ref() else {
            return Ok(());
        };
        let (color_view, resolve) = match attachments.color.as_ref() {
            Some(msaa) => (msaa, Some(&target_view)),
            None => (&target_view, None),
        };
        let store = if resolve.is_some() {
            wgpu::StoreOp::Discard
        } else {
            wgpu::StoreOp::Store
        };
        let clear = self.clear_for_target();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewer forward"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color_view,
                    depth_slice: None,
                    resolve_target: resolve,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(clear[0]),
                            g: f64::from(clear[1]),
                            b: f64::from(clear[2]),
                            a: f64::from(clear[3]),
                        }),
                        store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &attachments.depth,
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
            pass.set_bind_group(0, &self.frame_group, &[]);
            pass.set_bind_group(2, &self.look_group, &[]);
            let mut bound_pipeline = usize::MAX;
            let mut bound_material = u32::MAX;
            let mut bound_mesh = u32::MAX;
            let instance_bytes = self.instance_capacity * INSTANCE_STRIDE;
            for batch in &self.batches {
                if batch.pipeline != bound_pipeline {
                    pass.set_pipeline(&self.pipelines[batch.pipeline]);
                    bound_pipeline = batch.pipeline;
                    self.stats.pipeline_binds += 1;
                }
                if batch.material_offset != bound_material {
                    pass.set_bind_group(1, &self.material_group, &[batch.material_offset]);
                    bound_material = batch.material_offset;
                }
                if batch.mesh_slot != bound_mesh {
                    let Some(mesh) = self.meshes.at(batch.mesh_slot) else {
                        continue;
                    };
                    pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                    pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                    bound_mesh = batch.mesh_slot;
                }
                // WebGL2 has no first-instance draw: the instance range is a buffer offset.
                let start = u64::from(batch.first_instance) * INSTANCE_STRIDE;
                let end = (start + u64::from(batch.instances) * INSTANCE_STRIDE).min(instance_bytes);
                pass.set_vertex_buffer(1, self.instance_buffer.slice(start..end));
                pass.draw_indexed(0..batch.index_count, 0, 0..batch.instances);
                self.stats.draw_calls += 1;
                self.stats.instances_drawn += u64::from(batch.instances);
                self.stats.triangles_drawn += u64::from(batch.index_count / 3) * u64::from(batch.instances);
            }
            if self.style.contact_shadow.is_some()
                && let Some(pipeline) = self.pipelines.last()
            {
                pass.set_pipeline(pipeline);
                self.stats.pipeline_binds += 1;
                if bound_material == u32::MAX {
                    pass.set_bind_group(1, &self.material_group, &[0]);
                }
                pass.draw(0..4, 0..1);
                self.stats.draw_calls += 1;
            }
        }
        self.queue.submit([encoder.finish()]);
        self.stats.frames_drawn += 1;
        self.dirty = false;
        Ok(())
    }

    /// The clear color as the target stores it: a target without hardware sRGB gets encoded values, so
    /// the picture is the same on either.
    fn clear_for_target(&self) -> [f32; 4] {
        clear_value(self.style.clear_color, self.encode_srgb)
    }

    /// Draws into a texture that is not the host's picture: the host's next lazy frame still happens.
    pub(crate) fn render_offscreen(&mut self, target: &wgpu::Texture) -> Result<(), ViewerError> {
        let dirty = self.dirty;
        let result = self.render(target);
        self.dirty = dirty;
        result
    }

    fn ensure_attachments(&mut self, width: u32, height: u32) {
        if self
            .attachments
            .as_ref()
            .is_some_and(|a| a.size == (width, height))
        {
            return;
        }
        // Transient attachments live in tile memory on phones. The browser's WebGPU may not know the flag.
        let usage = if cfg!(target_arch = "wasm32") {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TRANSIENT_ATTACHMENT
        };
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer depth"),
            size,
            mip_level_count: 1,
            sample_count: self.sample_count,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage,
            view_formats: &[],
        });
        let color = (self.sample_count > 1).then(|| {
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("viewer msaa color"),
                    size,
                    mip_level_count: 1,
                    sample_count: self.sample_count,
                    dimension: wgpu::TextureDimension::D2,
                    format: self.target_format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        });
        self.attachments = Some(Attachments {
            size: (width, height),
            depth: depth.create_view(&wgpu::TextureViewDescriptor::default()),
            color,
        });
    }

    fn write_frame(&mut self, width: u32, height: u32) {
        let aspect = width as f32 / height as f32;
        let (shadow_center, shadow_extent, shadow_opacity) = match self.style.contact_shadow {
            Some(s) => (
                [
                    finite_or(s.center_mm[0], 0.0),
                    finite_or(s.center_mm[1], 0.0),
                    finite_or(s.center_mm[2], 0.0),
                    finite_or(s.softness_mm, 0.1).max(0.001),
                ],
                [
                    finite_or(s.half_extent_mm[0], 0.0).max(0.0),
                    finite_or(s.half_extent_mm[1], 0.0).max(0.0),
                    0.0,
                    0.0,
                ],
                finite_or(s.opacity, 0.0).clamp(0.0, 1.0),
            ),
            None => ([0.0; 4], [0.0; 4], 0.0),
        };
        let block = FrameBlock {
            view_proj: self.camera.view_proj(aspect).to_cols_array_2d(),
            eye: [
                self.camera.eye[0],
                self.camera.eye[1],
                self.camera.eye[2],
                finite_or(self.style.exposure, 1.0).max(0.0),
            ],
            sh: self.env_sh,
            env: [
                self.env_last_mip,
                if self.encode_srgb { 1.0 } else { 0.0 },
                shadow_opacity,
                0.0,
            ],
            shadow_center,
            shadow_extent,
            key_dir: self.style.key_lights.map(|k| {
                let d = glam::Vec3::from(k.dir).normalize_or(glam::Vec3::Z);
                [d.x, d.y, d.z, 0.0]
            }),
            key_col: self.style.key_lights.map(|k| {
                [
                    finite_or(k.radiance[0], 0.0),
                    finite_or(k.radiance[1], 0.0),
                    finite_or(k.radiance[2], 0.0),
                    0.0,
                ]
            }),
        };
        self.queue
            .write_buffer(&self.frame_buffer, 0, bytemuck::bytes_of(&block));
    }

    fn rebuild_batches(&mut self) {
        let _span = tracing::info_span!("viewer_rebuild_batches").entered();
        let mut order: Vec<(usize, u32, u32, u32, bool)> = Vec::new();
        for (slot, draw) in self.draws.iter() {
            let Some(material) = self.materials.get(draw.material.slot, draw.material.generation) else {
                continue;
            };
            let family = material.value.family().index();
            if draw.mirrored_from > 0 {
                order.push((family, draw.material.slot, draw.mesh.slot, slot, false));
            }
            if draw.mirrored_from < draw.transforms.len() {
                order.push((family, draw.material.slot, draw.mesh.slot, slot, true));
            }
        }
        // Family first (fewest pipeline binds), then material and mesh, with a deterministic tie break.
        order.sort_unstable_by_key(|(family, material, mesh, slot, mirrored)| {
            (*family, *mirrored, *material, *mesh, *slot)
        });

        let mut rows: Vec<[[f32; 4]; 3]> = Vec::new();
        let mut batches = Vec::with_capacity(order.len());
        for (family, material, mesh_slot, slot, mirrored) in order {
            let Some(draw) = self.draws.at(slot) else {
                continue;
            };
            let Some(mesh) = self.meshes.get(draw.mesh.slot, draw.mesh.generation) else {
                continue;
            };
            let range = if mirrored {
                draw.mirrored_from..draw.transforms.len()
            } else {
                0..draw.mirrored_from
            };
            let first_instance = rows.len() as u32;
            rows.extend(draw.transforms[range.clone()].iter().map(transform_rows));
            batches.push(Batch {
                pipeline: family * 2 + usize::from(mirrored),
                material_offset: material * self.material_stride,
                mesh_slot,
                first_instance,
                instances: range.len() as u32,
                index_count: mesh.index_count,
            });
        }
        self.ensure_instance_capacity(rows.len() as u64);
        if !rows.is_empty() {
            self.queue
                .write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&rows));
        }
        self.batches = batches;
        self.batches_dirty = false;
    }

    fn ensure_instance_capacity(&mut self, needed: u64) {
        if needed <= self.instance_capacity {
            return;
        }
        let capacity = needed.next_power_of_two();
        self.instance_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer instances"),
            size: capacity * INSTANCE_STRIDE,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.instance_capacity = capacity;
        self.stats.buffers_created += 1;
    }

    fn ensure_material_capacity(&mut self, needed: u32) {
        if needed <= self.material_capacity {
            return;
        }
        let capacity = needed.next_power_of_two();
        self.material_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer materials"),
            size: u64::from(capacity) * u64::from(self.material_stride),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.stats.buffers_created += 1;
        self.material_capacity = capacity;
        self.material_group =
            create_material_group(&self.device, &self.material_layout, &self.material_buffer);
        self.stats.bind_groups_created += 1;
        let existing: Vec<(u32, ViewerMaterial)> =
            self.materials.iter().map(|(slot, m)| (slot, m.value)).collect();
        for (slot, value) in existing {
            self.write_material(slot, &value);
        }
    }

    fn write_material(&mut self, slot: u32, material: &ViewerMaterial) {
        let block = material.to_block();
        let offset = u64::from(slot) * u64::from(self.material_stride);
        self.queue
            .write_buffer(&self.material_buffer, offset, bytemuck::bytes_of(&block));
        self.stats.material_writes += 1;
    }

    fn refresh_frame_group(&mut self) {
        self.frame_group = create_frame_group(
            &self.device,
            &self.frame_layout,
            &self.frame_buffer,
            &self.env_view,
            &self.env_sampler,
            &self.dfg_view,
            &self.lut_view,
            &self.clamp_sampler,
        );
        self.stats.bind_groups_created += 1;
    }
}

/// Puts the transforms that keep their winding first; the second value is where the mirrored ones start.
fn split_mirrored(transforms: &[Affine3A]) -> Result<(Vec<Affine3A>, usize), ViewerError> {
    if transforms.is_empty() {
        return Err(ViewerError::Config(
            "a draw needs at least one transform".to_string(),
        ));
    }
    if transforms.iter().any(|t| !t.is_finite()) {
        return Err(ViewerError::Config(
            "a transform holds a value that is not finite".to_string(),
        ));
    }
    let (keep, mirrored): (Vec<Affine3A>, Vec<Affine3A>) =
        transforms.iter().partition(|t| t.matrix3.determinant() >= 0.0);
    let start = keep.len();
    let mut ordered = keep;
    ordered.extend(mirrored);
    Ok((ordered, start))
}

fn transform_rows(t: &Affine3A) -> [[f32; 4]; 3] {
    let m = t.matrix3;
    let p = t.translation;
    [
        [m.x_axis.x, m.y_axis.x, m.z_axis.x, p.x],
        [m.x_axis.y, m.y_axis.y, m.z_axis.y, p.y],
        [m.x_axis.z, m.y_axis.z, m.z_axis.z, p.z],
    ]
}

fn linear_to_srgb8(v: f32) -> u8 {
    let v = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
    let encoded = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5) as u8
}

#[allow(clippy::too_many_arguments)] // One bind group's bindings, all required (ADR-2539).
fn create_frame_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    frame: &wgpu::Buffer,
    env: &wgpu::TextureView,
    env_sampler: &wgpu::Sampler,
    dfg: &wgpu::TextureView,
    lut: &wgpu::TextureView,
    clamp_sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewer frame"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: frame.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(env),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(env_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(dfg),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(lut),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::Sampler(clamp_sampler),
            },
        ],
    })
}

fn create_material_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewer material"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer,
                offset: 0,
                size: wgpu::BufferSize::new(MATERIAL_BLOCK_SIZE),
            }),
        }],
    })
}

fn create_look_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    views: &[wgpu::TextureView; LOOK_TEXTURE_SLOTS],
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewer look"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&views[0]),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&views[1]),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

fn build_pipelines(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    config: &ViewerConfig,
    stats: &mut ViewerStats,
) -> Vec<wgpu::RenderPipeline> {
    let _span = tracing::info_span!("viewer_build_pipelines").entered();
    let attributes_mesh = [
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
            format: wgpu::VertexFormat::Float32,
            offset: 32,
            shader_location: 3,
        },
    ];
    let attributes_instance = [
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x4,
            offset: 0,
            shader_location: 4,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x4,
            offset: 16,
            shader_location: 5,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x4,
            offset: 32,
            shader_location: 6,
        },
    ];
    let buffers = [
        Some(wgpu::VertexBufferLayout {
            array_stride: VERTEX_STRIDE,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attributes_mesh,
        }),
        Some(wgpu::VertexBufferLayout {
            array_stride: INSTANCE_STRIDE,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &attributes_instance,
        }),
    ];
    let multisample = wgpu::MultisampleState {
        count: config.sample_count,
        mask: !0,
        alpha_to_coverage_enabled: false,
    };
    let opaque_target = [Some(wgpu::ColorTargetState {
        format: config.target_format,
        blend: None,
        write_mask: wgpu::ColorWrites::ALL,
    })];
    let mut pipelines = Vec::with_capacity(ShaderFamily::ALL.len() * 2 + 1);
    for family in ShaderFamily::ALL {
        let source = config.hooks.family_source(family);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(family.name()),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        // A mirrored transform flips the winding: same shader, the other front face.
        for front_face in [wgpu::FrontFace::Ccw, wgpu::FrontFace::Cw] {
            pipelines.push(device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(family.name()),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &buffers,
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face,
                    cull_mode: Some(wgpu::Face::Back),
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample,
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &opaque_target,
                }),
                multiview_mask: None,
                cache: config.pipeline_cache.as_ref(),
            }));
            stats.pipelines_created += 1;
        }
    }
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("contact shadow"),
        source: wgpu::ShaderSource::Wgsl(contact_shadow_source().into()),
    });
    let blend_target = [Some(wgpu::ColorTargetState {
        format: config.target_format,
        blend: Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        }),
        write_mask: wgpu::ColorWrites::ALL,
    })];
    pipelines.push(device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("contact shadow"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_shadow"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample,
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_shadow"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &blend_target,
        }),
        multiview_mask: None,
        cache: config.pipeline_cache.as_ref(),
    }));
    stats.pipelines_created += 1;
    pipelines
}

#[cfg(test)]
mod frame_block_tests {
    use super::FrameBlock;

    /// Spec 42 § 4.1: the per-frame uniform block (camera, SH9 irradiance, the fixed key lights)
    /// stays within 1 KB and keeps the 16-byte uniform layout of `prelude.wgsl`.
    #[test]
    fn the_frame_block_fits_one_kilobyte_and_keeps_uniform_alignment() {
        let size = std::mem::size_of::<FrameBlock>();
        assert!(size <= 1024, "FrameBlock is {size} bytes");
        assert!(size.is_multiple_of(16), "FrameBlock is {size} bytes");
    }
}

#[cfg(test)]
mod tests {
    use super::clear_value;

    #[test]
    fn an_encoded_clear_keeps_rgb_at_or_below_alpha() {
        // corr-web-viewer listed: in encode mode the rgb channels were encoded but the alpha was
        // left linear, so a premultiplied translucent clear (rgb <= alpha) stored enc(rgb) > alpha
        // — not valid premultiplied data for a compositing page.
        for (rgb, a) in [(0.5, 0.5), (0.25, 0.6), (0.0, 0.25), (1.0, 1.0)] {
            let clear = clear_value([rgb, rgb, rgb, a], true);
            for channel in &clear[0..3] {
                assert!(
                    *channel <= clear[3] + 1e-6,
                    "({rgb}, alpha {a}) encoded to {clear:?}: rgb exceeds alpha"
                );
            }
        }
        // A linear target stores the premultiplied color untouched.
        assert_eq!(clear_value([0.3, 0.2, 0.1, 0.4], false), [0.3, 0.2, 0.1, 0.4]);
        // Non-finite components are zeroed, never NaN in the encoder.
        let clear = clear_value([f32::NAN, 0.0, 0.0, 1.0], true);
        assert!(clear[0].is_finite() && clear.iter().all(|v| *v >= 0.0));
    }
}
