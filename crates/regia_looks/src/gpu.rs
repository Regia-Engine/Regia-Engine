// SPDX-License-Identifier: MPL-2.0
//! The look renderer (feature `gpu`): one pipeline per look kind, written for the WebGL2 limits on
//! every backend (uniform buffers and textures only, dynamic uniform offsets, MSAA 4, Depth24Plus).
//!
//! A material swap changes only per-draw uniform data: [`LookRenderer::pipelines_created`] stays
//! constant after construction (the viewer's swap budget test reads it).

use std::collections::BTreeMap;

use bytemuck::{Pod, Zeroable};

use crate::anodize::{LUT_ENTRIES, lut_texture_height, lut_upload_is_current, lut_upload_stamp};
pub use crate::board::{LookCamera, LookItem, LookScene};
use crate::env::{Studio, f32_to_f16_bits};
use crate::material::{LookKind, MaterialParams};
use crate::mesh::{LookMesh, LookVertex};

/// The WGSL source of every look shader.
pub const LOOKS_WGSL: &str = crate::LOOKS_SHADER;

/// Per-draw uniform stride (the WebGL2 and WebGPU minimum dynamic offset alignment).
pub const OBJECT_STRIDE: u64 = 256;

/// Errors of the look renderer.
#[derive(Debug, thiserror::Error)]
pub enum LookGpuError {
    /// No adapter could be opened.
    #[error("gpu_no_adapter: {0}")]
    NoAdapter(String),
    /// The device request failed.
    #[error("gpu_no_device: {0}")]
    NoDevice(String),
    /// Waiting for the GPU failed.
    #[error("gpu_poll: {0}")]
    Poll(String),
    /// Reading the image back failed.
    #[error("gpu_readback: {0}")]
    Readback(String),
    /// The scene has more draws than the per-draw buffer holds.
    #[error("too_many_draws: {0} (max {1})")]
    TooManyDraws(usize, usize),
}

impl LookGpuError {
    /// The stable lowercase error code (agent protocol).
    pub fn code(&self) -> &'static str {
        match self {
            LookGpuError::NoAdapter(_) => "gpu_no_adapter",
            LookGpuError::NoDevice(_) => "gpu_no_device",
            LookGpuError::Poll(_) => "gpu_poll",
            LookGpuError::Readback(_) => "gpu_readback",
            LookGpuError::TooManyDraws(..) => "too_many_draws",
        }
    }
}

/// Minimal executor for wgpu's futures (they resolve on poll on native backends).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

/// Opens a headless device with the WebGL2 downlevel limits (lavapipe on CI hosts: set
/// `VK_ICD_FILENAMES`). The limits are the ones the web player gets, so a still proves the shaders
/// fit them.
pub fn headless_device() -> Result<(wgpu::Device, wgpu::Queue, wgpu::AdapterInfo), LookGpuError> {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::from_env()
        .unwrap_or(wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12);
    let instance = wgpu::Instance::new(descriptor);
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
        ..Default::default()
    }))
    .map_err(|e| LookGpuError::NoAdapter(e.to_string()))?;
    let info = adapter.get_info();
    let limits = wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("regia_looks headless"),
        required_limits: limits,
        ..Default::default()
    }))
    .map_err(|e| LookGpuError::NoDevice(e.to_string()))?;
    Ok((device, queue, info))
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct FrameUniform {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    key_dir: [[f32; 4]; 3],
    key_col: [[f32; 4]; 3],
    env: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct ObjectUniform {
    model: [[f32; 4]; 4],
    params: MaterialParams,
}

/// Maximum draws per frame (per-draw uniform buffer of 16 KB at a 256-byte stride... WebGL2's
/// minimum uniform block is 16 KB, so the buffer is bound 128 bytes at a time).
pub const MAX_DRAWS: usize = 256;

/// The look renderer: pipelines, the studio environment and the anodize table texture.
pub struct LookRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    color_format: wgpu::TextureFormat,
    samples: u32,
    pipelines: BTreeMap<LookKind, wgpu::RenderPipeline>,
    background: wgpu::RenderPipeline,
    frame_buf: wgpu::Buffer,
    object_buf: wgpu::Buffer,
    frame_layout: wgpu::BindGroupLayout,
    object_bind: wgpu::BindGroup,
    env_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    lut_rows: Vec<Vec<[f32; 3]>>,
    lut_tex: Option<(wgpu::Texture, wgpu::TextureView, usize)>,
    studio: Studio,
    pipelines_created: u32,
    draws_last_frame: u32,
}

impl LookRenderer {
    /// Builds the pipelines and uploads the studio. `encode_srgb_in_shader` is for targets that are
    /// not `*Srgb` formats (a WebGL2 canvas).
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        color_format: wgpu::TextureFormat,
        samples: u32,
        studio: Studio,
    ) -> LookRenderer {
        let _span = tracing::debug_span!("look_renderer_new").entered();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("regia_looks"),
            source: wgpu::ShaderSource::Wgsl(LOOKS_WGSL.into()),
        });
        let frame_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("looks frame"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<FrameUniform>() as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::Cube,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let object_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("looks object"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<ObjectUniform>() as u64),
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("looks"),
            bind_group_layouts: &[Some(&frame_layout), Some(&object_layout)],
            immediate_size: 0,
        });
        let depth = || wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth24Plus,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        };
        let multisample = wgpu::MultisampleState {
            count: samples,
            ..Default::default()
        };
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<LookVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32],
        };
        let mut pipelines_created = 0;
        let mut pipelines = BTreeMap::new();
        for kind in LookKind::ALL {
            let p = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(kind.fragment_entry()),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[Some(vertex_layout.clone())],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(kind.fragment_entry()),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: color_format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(wgpu::Face::Back),
                    ..Default::default()
                },
                depth_stencil: Some(depth()),
                multisample,
                multiview_mask: None,
                cache: None,
            });
            pipelines_created += 1;
            pipelines.insert(kind, p);
        }
        let background = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("looks background"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_background"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_background"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                ..depth()
            }),
            multisample,
            multiview_mask: None,
            cache: None,
        });
        pipelines_created += 1;
        let frame_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("looks frame"),
            size: std::mem::size_of::<FrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let object_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("looks objects"),
            size: OBJECT_STRIDE * MAX_DRAWS as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let object_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("looks object"),
            layout: &object_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &object_buf,
                    offset: 0,
                    size: wgpu::BufferSize::new(std::mem::size_of::<ObjectUniform>() as u64),
                }),
            }],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("looks env"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let env_view = upload_studio(&device, &queue, &studio);
        LookRenderer {
            device,
            queue,
            color_format,
            samples,
            pipelines,
            background,
            frame_buf,
            object_buf,
            frame_layout,
            object_bind,
            env_view,
            sampler,
            lut_rows: Vec::new(),
            lut_tex: None,
            studio,
            pipelines_created,
            draws_last_frame: 0,
        }
    }

    /// Pipelines created since construction (constant: swaps never create one).
    pub fn pipelines_created(&self) -> u32 {
        self.pipelines_created
    }

    /// Draw calls in the last [`LookRenderer::render`] (background included).
    pub fn draws_last_frame(&self) -> u32 {
        self.draws_last_frame
    }

    /// The device.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The anodize table row of a color table, uploading a new row when it is not resident.
    fn lut_row(&mut self, lut: &[[f32; 3]]) -> u32 {
        if let Some(i) = self.lut_rows.iter().position(|r| r.as_slice() == lut) {
            return i as u32;
        }
        self.lut_rows.push(lut.to_vec());
        (self.lut_rows.len() - 1) as u32
    }

    fn ensure_lut_texture(&mut self) {
        let resident = self.lut_rows.len();
        let uploaded = self.lut_tex.as_ref().map(|(_, _, n)| *n);
        if lut_upload_is_current(uploaded, resident) {
            return;
        }
        let rows = lut_texture_height(resident);
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("looks anodize"),
            size: wgpu::Extent3d {
                width: LUT_ENTRIES as u32,
                height: rows as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut texels: Vec<u16> = Vec::with_capacity(LUT_ENTRIES * rows * 4);
        for r in 0..rows {
            for i in 0..LUT_ENTRIES {
                let c = self.lut_rows.get(r).map_or([0.0; 3], |row| resample(row, i));
                texels.extend_from_slice(&[
                    f32_to_f16_bits(c[0]),
                    f32_to_f16_bits(c[1]),
                    f32_to_f16_bits(c[2]),
                    0x3c00,
                ]);
            }
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(LUT_ENTRIES as u32 * 8),
                rows_per_image: Some(rows as u32),
            },
            wgpu::Extent3d {
                width: LUT_ENTRIES as u32,
                height: rows as u32,
                depth_or_array_layers: 1,
            },
        );
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        self.lut_tex = Some((tex, view, lut_upload_stamp(resident)));
    }

    /// Renders a scene into `target` (a view of `color_format`, `samples` samples; `resolve` is the
    /// single-sample view when `samples > 1`).
    pub fn render(
        &mut self,
        scene: &LookScene,
        size: (u32, u32),
        target: &wgpu::TextureView,
        resolve: Option<&wgpu::TextureView>,
        encode_srgb_in_shader: bool,
    ) -> Result<(), LookGpuError> {
        let _span = tracing::debug_span!("looks_render", items = scene.items.len()).entered();
        if scene.items.len() > MAX_DRAWS {
            return Err(LookGpuError::TooManyDraws(scene.items.len(), MAX_DRAWS));
        }
        // Material data first: a new anodize color may add a table row.
        let rows: Vec<Option<u32>> = scene
            .items
            .iter()
            .map(|it| it.material.anodize_lut().map(|lut| self.lut_row(lut)))
            .collect();
        self.ensure_lut_texture();
        let aspect = size.0 as f32 / size.1.max(1) as f32;
        let keys = self.studio.keys;
        let frame = FrameUniform {
            view_proj: scene.camera.view_proj(aspect).to_cols_array_2d(),
            eye: [
                scene.camera.eye.x,
                scene.camera.eye.y,
                scene.camera.eye.z,
                scene.exposure,
            ],
            key_dir: keys.map(|k| [k.dir.x, k.dir.y, k.dir.z, 0.0]),
            key_col: keys.map(|k| [k.color.x, k.color.y, k.color.z, 0.0]),
            env: [
                1.0,
                (self.studio.mips - 1) as f32,
                self.lut_rows.len().max(1) as f32,
                if encode_srgb_in_shader { 1.0 } else { 0.0 },
            ],
        };
        self.queue
            .write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(&frame));
        let mut objects = vec![0u8; OBJECT_STRIDE as usize * scene.items.len().max(1)];
        for (i, (item, row)) in scene.items.iter().zip(&rows).enumerate() {
            let u = ObjectUniform {
                model: item.model.to_cols_array_2d(),
                params: item.material.params(item.stone, *row),
            };
            let at = i * OBJECT_STRIDE as usize;
            objects[at..at + std::mem::size_of::<ObjectUniform>()].copy_from_slice(bytemuck::bytes_of(&u));
        }
        self.queue.write_buffer(&self.object_buf, 0, &objects);
        let Some((_, lut_view, _)) = self.lut_tex.as_ref() else {
            return Ok(());
        };
        let frame_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("looks frame"),
            layout: &self.frame_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.frame_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.env_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(lut_view),
                },
            ],
        });
        let buffers: Vec<(wgpu::Buffer, wgpu::Buffer, u32)> = scene
            .items
            .iter()
            .map(|it| upload_mesh(&self.device, &self.queue, &it.mesh))
            .collect();
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("looks depth"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: self.samples,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth24Plus,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("looks") });
        let mut draws = 0;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("looks"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: resolve,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
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
            pass.set_bind_group(0, &frame_bind, &[]);
            pass.set_bind_group(1, &self.object_bind, &[0]);
            pass.set_pipeline(&self.background);
            pass.draw(0..3, 0..1);
            draws += 1;
            // Opaque kinds first, then stones (each stone is a closed convex solid; no sorting).
            let mut order: Vec<usize> = (0..scene.items.len()).collect();
            order.sort_by_key(|&i| scene.items[i].material.kind() != LookKind::Metal);
            for i in order {
                let (vb, ib, count) = &buffers[i];
                let Some(pipeline) = self.pipelines.get(&scene.items[i].material.kind()) else {
                    continue;
                };
                pass.set_pipeline(pipeline);
                pass.set_bind_group(1, &self.object_bind, &[(i as u64 * OBJECT_STRIDE) as u32]);
                pass.set_vertex_buffer(0, vb.slice(..));
                pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..*count, 0, 0..1);
                draws += 1;
            }
        }
        self.queue.submit([encoder.finish()]);
        self.draws_last_frame = draws;
        Ok(())
    }

    /// Renders a scene offscreen and reads it back as tightly packed RGBA8 (sRGB) rows.
    pub fn render_still(
        &mut self,
        scene: &LookScene,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, LookGpuError> {
        let _span = tracing::debug_span!("looks_render_still", width, height).entered();
        let make = |samples: u32, usage: wgpu::TextureUsages| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("looks still"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format: self.color_format,
                usage,
                view_formats: &[],
            })
        };
        let resolved = make(
            1,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        let resolved_view = resolved.create_view(&wgpu::TextureViewDescriptor::default());
        let encode_in_shader = !self.color_format.is_srgb();
        if self.samples > 1 {
            let ms = make(self.samples, wgpu::TextureUsages::RENDER_ATTACHMENT);
            let ms_view = ms.create_view(&wgpu::TextureViewDescriptor::default());
            self.render(
                scene,
                (width, height),
                &ms_view,
                Some(&resolved_view),
                encode_in_shader,
            )?;
        } else {
            self.render(scene, (width, height), &resolved_view, None, encode_in_shader)?;
        }
        read_rgba8(&self.device, &self.queue, &resolved, width, height)
    }
}

fn resample(row: &[[f32; 3]], i: usize) -> [f32; 3] {
    if row.len() == LUT_ENTRIES || row.len() < 2 {
        return row.get(i).copied().unwrap_or([0.0; 3]);
    }
    let x = i as f32 / (LUT_ENTRIES - 1) as f32 * (row.len() - 1) as f32;
    let a = x.floor() as usize;
    let b = (a + 1).min(row.len() - 1);
    let t = x - a as f32;
    std::array::from_fn(|c| row[a][c] + (row[b][c] - row[a][c]) * t)
}

fn upload_mesh(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    mesh: &LookMesh,
) -> (wgpu::Buffer, wgpu::Buffer, u32) {
    let verts = mesh.vertices();
    let vb = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("looks vertices"),
        size: (std::mem::size_of_val(verts.as_slice()) as u64)
            .max(16)
            .next_multiple_of(4),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&vb, 0, bytemuck::cast_slice(&verts));
    let ib = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("looks indices"),
        size: (std::mem::size_of_val(mesh.indices.as_slice()) as u64)
            .max(16)
            .next_multiple_of(4),
        usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&ib, 0, bytemuck::cast_slice(&mesh.indices));
    (vb, ib, mesh.indices.len() as u32)
}

fn upload_studio(device: &wgpu::Device, queue: &wgpu::Queue, studio: &Studio) -> wgpu::TextureView {
    let levels = studio.build();
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("looks studio"),
        size: wgpu::Extent3d {
            width: studio.base_size,
            height: studio.base_size,
            depth_or_array_layers: 6,
        },
        mip_level_count: studio.mips,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (mip, faces) in levels.iter().enumerate() {
        let size = studio.mip_size(mip as u32);
        for (face, texels) in faces.iter().enumerate() {
            let half: Vec<u16> = texels.iter().flat_map(|t| t.map(f32_to_f16_bits)).collect();
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: mip as u32,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: face as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&half),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size * 8),
                    rows_per_image: Some(size),
                },
                wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: 1,
                },
            );
        }
    }
    tex.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::Cube),
        ..Default::default()
    })
}

fn read_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, LookGpuError> {
    let row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("looks readback"),
        size: u64::from(row) * u64::from(height),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("looks readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| LookGpuError::Poll(e.to_string()))?;
    rx.recv()
        .map_err(|e| LookGpuError::Readback(e.to_string()))?
        .map_err(|e| LookGpuError::Readback(e.to_string()))?;
    let data = buf
        .slice(..)
        .get_mapped_range()
        .map_err(|e| LookGpuError::Readback(e.to_string()))?;
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as usize {
        let start = y * row as usize;
        out.extend_from_slice(&data[start..start + width as usize * 4]);
    }
    drop(data);
    buf.unmap();
    Ok(out)
}
