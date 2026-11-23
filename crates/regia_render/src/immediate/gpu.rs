// SPDX-License-Identifier: MPL-2.0
//! GPU side of [`super::ImmediateBatch`]: pipelines per target format, sample count, blend and
//! texture kind; registered images; the multisampled layer and its composite.
//!
//! With `samples > 1` the alpha and additive draws render into a cleared multisampled
//! `Rgba16Float` layer that is resolved and composited "over" the frame (anti-aliased edges
//! without touching the frame's own attachments). Multiply and replace draws need the frame
//! underneath, so they draw straight onto it, between layer segments, in submission order.

use crate::frame_work::counted::*;
use std::collections::HashMap;
use std::sync::Arc;

use regia_core::math::Mat4;

use super::{ImmediateBatch, ImmediateBlend, ImmediateDraw, ImmediateTexture, ImmediateVertex};
use crate::{RenderDevice, RenderError, TrackedDevice};

const SHADER: &str = include_str!("../../shaders/immediate.wgsl");

/// `immediate.wgsl`, for `check-shaders`.
#[must_use]
pub fn immediate_shader_source() -> &'static str {
    SHADER
}

/// Format of the multisampled layer.
const LAYER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Pixel layout of an [`ImmediateImage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImmediateImageFormat {
    /// sRGB-encoded RGBA8 with **premultiplied** alpha (sampled as linear, premultiplied).
    Rgba8SrgbPremultiplied,
    /// One byte of coverage per pixel ([`ImmediateTexture::Mask`]).
    R8,
}

impl ImmediateImageFormat {
    fn wgpu(self) -> wgpu::TextureFormat {
        match self {
            Self::Rgba8SrgbPremultiplied => wgpu::TextureFormat::Rgba8UnormSrgb,
            Self::R8 => wgpu::TextureFormat::R8Unorm,
        }
    }

    fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgba8SrgbPremultiplied => 4,
            Self::R8 => 1,
        }
    }
}

/// A CPU image the renderer uploads on demand.
#[derive(Debug, Clone)]
pub struct ImmediateImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel layout.
    pub format: ImmediateImageFormat,
    /// Tightly packed rows, top row first.
    pub data: Arc<[u8]>,
}

/// Where a batch is drawn.
pub struct ImmediateTarget<'a> {
    /// The frame's colour view (loaded, not cleared).
    pub view: &'a wgpu::TextureView,
    /// Its format.
    pub format: wgpu::TextureFormat,
    /// Its size in pixels.
    pub width: u32,
    /// Its size in pixels.
    pub height: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PipelineKey {
    format: wgpu::TextureFormat,
    samples: u32,
    blend: ImmediateBlend,
    mask: bool,
    /// The draw goes into the multisampled layer (additive keeps the layer's alpha).
    layer: bool,
}

struct Layouts {
    view: wgpu::BindGroupLayout,
    image: wgpu::BindGroupLayout,
    pipeline: wgpu::PipelineLayout,
}

struct GpuImage {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    /// Whether the texture holds the bytes of the image last requested for this
    /// id. `false` after a rejected (short or empty) upload: draws skip the
    /// texture rather than sample uninitialized or stale pixels.
    ready: bool,
    size: (u32, u32),
    format: ImmediateImageFormat,
}

struct StoredImage {
    image: ImmediateImage,
    dirty: bool,
    /// The too-short-buffer warning was logged for the current image.
    warned: bool,
}

struct Layer {
    size: (u32, u32),
    samples: u32,
    msaa: wgpu::TextureView,
    resolve_bind: wgpu::BindGroup,
    resolve: wgpu::TextureView,
}

struct DeviceState {
    device_id: u64,
    layouts: Layouts,
    module: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    white: wgpu::BindGroup,
    /// `[layer view, direct view]` uniforms (the direct one carries the target's encoding).
    views: [(wgpu::Buffer, wgpu::BindGroup); 2],
    pipelines: HashMap<PipelineKey, wgpu::RenderPipeline>,
    composites: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    vertex: Option<(wgpu::Buffer, u64)>,
    index: Option<(wgpu::Buffer, u64)>,
    images: HashMap<u64, GpuImage>,
    layer: Option<Layer>,
}

/// Draws [`ImmediateBatch`]es. Device state is created lazily on the first non-empty
/// [`Self::record`] and rebuilt when the device changes; registered images survive that (the
/// renderer keeps their pixels and re-uploads).
#[derive(Default)]
pub struct ImmediateRenderer {
    state: Option<DeviceState>,
    images: HashMap<u64, StoredImage>,
    /// Last vertex/index upload, when one has run.
    uploaded: Option<UploadKey>,
    /// Set by [`Self::arm_upload`] and consumed by the next [`Self::record`].
    pending_upload: Option<UploadKey>,
    /// Vertex and index uploads performed after [`Self::arm_upload`].
    geometry_uploads: u64,
}

/// Identity of one vertex and index upload.
///
/// A later record with the same generation and the same buffer lengths already has those bytes
/// on the GPU. Generation 0 never matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadKey {
    /// Bumped by the producer when it rebuilds the batch. Zero opts out of reuse.
    pub generation: u64,
    /// Vertex count of that upload.
    pub vertices: usize,
    /// Index count of that upload.
    pub indices: usize,
}

/// Whether a record must copy vertex and index bytes to the GPU.
///
/// Returns `true` when `next` is not a repeat of `previous`. The draw pass still runs either way.
#[must_use]
pub fn uploads_vertex_buffers(previous: Option<UploadKey>, next: UploadKey) -> bool {
    !matches!(previous, Some(prev) if next.generation != 0 && prev == next)
}

/// Bytes of the view uniform (`ImmediateView`).
const VIEW_BYTES: u64 = 80;

fn view_bytes(clip_from_local: Mat4, encode_srgb: bool) -> [u8; VIEW_BYTES as usize] {
    let mut out = [0u8; VIEW_BYTES as usize];
    out[..64].copy_from_slice(bytemuck::cast_slice(&clip_from_local.to_cols_array()));
    out[64..68].copy_from_slice(&u32::from(encode_srgb).to_le_bytes());
    out
}

/// Whether the shader must sRGB-encode for `format` (8-bit UNORM, non-sRGB targets).
fn shader_encodes(format: wgpu::TextureFormat) -> bool {
    !format.is_srgb()
        && matches!(
            format,
            wgpu::TextureFormat::Rgba8Unorm
                | wgpu::TextureFormat::Bgra8Unorm
                | wgpu::TextureFormat::Rgb10a2Unorm
        )
}

fn blend_state(blend: ImmediateBlend) -> Option<wgpu::BlendState> {
    use wgpu::{BlendComponent, BlendFactor as F, BlendOperation as Op};
    let over = BlendComponent {
        src_factor: F::One,
        dst_factor: F::OneMinusSrcAlpha,
        operation: Op::Add,
    };
    match blend {
        ImmediateBlend::Alpha => Some(wgpu::BlendState {
            color: over,
            alpha: over,
        }),
        ImmediateBlend::Additive => Some(wgpu::BlendState {
            color: BlendComponent {
                src_factor: F::One,
                dst_factor: F::One,
                operation: Op::Add,
            },
            // Alpha is kept: in the layer, additive light must not occlude the frame when the
            // layer is composited "over" it.
            alpha: BlendComponent {
                src_factor: F::Zero,
                dst_factor: F::One,
                operation: Op::Add,
            },
        }),
        ImmediateBlend::Multiply => Some(wgpu::BlendState {
            // dst · (src + (1 − a)): premultiplied multiply that fades with alpha.
            color: BlendComponent {
                src_factor: F::Dst,
                dst_factor: F::OneMinusSrcAlpha,
                operation: Op::Add,
            },
            alpha: BlendComponent {
                src_factor: F::Zero,
                dst_factor: F::One,
                operation: Op::Add,
            },
        }),
        ImmediateBlend::Replace => None,
    }
}

/// Whether a draw can go into the multisampled layer.
fn layerable(blend: ImmediateBlend) -> bool {
    matches!(blend, ImmediateBlend::Alpha | ImmediateBlend::Additive)
}

impl DeviceState {
    fn new(device: &RenderDevice) -> Result<Self, RenderError> {
        let tracked = TrackedDevice::observe(&device.device);
        let view = device
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("immediate view"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(VIEW_BYTES),
                    },
                    count: None,
                }],
            });
        let image = device
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("immediate image"),
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
                ],
            });
        let pipeline = device
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("immediate"),
                bind_group_layouts: &[Some(&view), Some(&image)],
                immediate_size: 0,
            });
        let naga_module = crate::shader_lib::compose_source(SHADER, "immediate.wgsl", HashMap::new())?;
        let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("immediate.wgsl"),
            source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(naga_module)),
        });
        let sampler = device.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("immediate"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let layouts = Layouts {
            view,
            image,
            pipeline,
        };
        let white_image = ImmediateImage {
            width: 1,
            height: 1,
            format: ImmediateImageFormat::Rgba8SrgbPremultiplied,
            data: Arc::from([255u8; 4].as_slice()),
        };
        let white = upload_image(device, &tracked, &layouts, &sampler, &white_image).bind_group;
        let make_view = |label| {
            let buffer = tracked.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: VIEW_BYTES,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind = tracked.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &layouts.view,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            (buffer, bind)
        };
        let views = [
            make_view("immediate layer view"),
            make_view("immediate direct view"),
        ];
        Ok(Self {
            device_id: device.device_id(),
            layouts,
            module,
            sampler,
            white,
            views,
            pipelines: HashMap::new(),
            composites: HashMap::new(),
            vertex: None,
            index: None,
            images: HashMap::new(),
            layer: None,
        })
    }

    fn pipeline(&mut self, device: &TrackedDevice, key: PipelineKey) -> wgpu::RenderPipeline {
        if let Some(pipeline) = self.pipelines.get(&key) {
            return pipeline.clone();
        }
        let attributes = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2, 2 => Float32x4];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("immediate"),
            layout: Some(&self.layouts.pipeline),
            vertex: wgpu::VertexState {
                module: &self.module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ImmediateVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attributes,
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: key.samples,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.module,
                entry_point: Some(if key.mask { "fs_mask" } else { "fs_image" }),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: key.format,
                    blend: blend_state(key.blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        self.pipelines.insert(key, pipeline.clone());
        pipeline
    }

    fn composite(&mut self, device: &TrackedDevice, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        if let Some(pipeline) = self.composites.get(&format) {
            return pipeline.clone();
        }
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("immediate composite"),
            layout: Some(&self.layouts.pipeline),
            vertex: wgpu::VertexState {
                module: &self.module,
                entry_point: Some("vs_composite"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &self.module,
                entry_point: Some("fs_composite"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: blend_state(ImmediateBlend::Alpha),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        self.composites.insert(format, pipeline.clone());
        pipeline
    }

    fn layer(&mut self, device: &TrackedDevice, size: (u32, u32), samples: u32) -> &Layer {
        let stale = self
            .layer
            .as_ref()
            .is_none_or(|layer| layer.size != size || layer.samples != samples);
        if stale {
            let extent = wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            };
            let msaa = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("immediate layer msaa"),
                size: extent,
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format: LAYER_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let resolve = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("immediate layer"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: LAYER_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let resolve_view = resolve.create_view(&wgpu::TextureViewDescriptor::default());
            let resolve_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("immediate layer"),
                layout: &self.layouts.image,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&resolve_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.layer = Some(Layer {
                size,
                samples,
                msaa: msaa.create_view(&wgpu::TextureViewDescriptor::default()),
                resolve_bind,
                resolve: resolve_view,
            });
        }
        // Just (re)built or already current.
        self.layer.as_ref().expect("layer was created above")
    }

    /// Grows `slot` to hold `bytes` and writes them.
    fn upload(
        device: &RenderDevice,
        tracked: &TrackedDevice,
        slot: &mut Option<(wgpu::Buffer, u64)>,
        bytes: &[u8],
        usage: wgpu::BufferUsages,
        label: &'static str,
    ) -> wgpu::Buffer {
        let needed = (bytes.len() as u64).max(4).next_power_of_two();
        if slot.as_ref().is_none_or(|(_, capacity)| *capacity < needed) {
            *slot = Some((
                tracked.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: needed,
                    usage: usage | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                needed,
            ));
        }
        let buffer = slot
            .as_ref()
            .map(|(b, _)| b.clone())
            .expect("buffer was created above");
        device.queue.write_buffer(&buffer, 0, bytes);
        buffer
    }
}

fn upload_image(
    device: &RenderDevice,
    tracked: &TrackedDevice,
    layouts: &Layouts,
    sampler: &wgpu::Sampler,
    image: &ImmediateImage,
) -> GpuImage {
    let (width, height) = (image.width.max(1), image.height.max(1));
    let texture = tracked.create_texture(&wgpu::TextureDescriptor {
        label: Some("immediate image"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: image.format.wgpu(),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let ready = write_image(device, &texture, image);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = tracked.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("immediate image"),
        layout: &layouts.image,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    });
    GpuImage {
        _texture: texture,
        bind_group,
        ready,
        size: (width, height),
        format: image.format,
    }
}

/// Writes `image`'s pixels into `texture`. `false` when the buffer is shorter
/// than the declared size needs or a dimension is 0 (nothing was written).
fn write_image(device: &RenderDevice, texture: &wgpu::Texture, image: &ImmediateImage) -> bool {
    let row = image.width * image.format.bytes_per_pixel();
    let expected = row as usize * image.height as usize;
    if image.data.len() < expected || image.width == 0 || image.height == 0 {
        return false;
    }
    device.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &image.data[..expected],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(row),
            rows_per_image: Some(image.height),
        },
        wgpu::Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        },
    );
    true
}

/// A run of draws that either all go into the multisampled layer or all draw directly.
struct Segment<'a> {
    layer: bool,
    draws: &'a [ImmediateDraw],
}

fn segments(draws: &[ImmediateDraw], use_layer: bool) -> Vec<Segment<'_>> {
    let mut out: Vec<Segment<'_>> = Vec::new();
    let mut start = 0;
    for i in 1..=draws.len() {
        let split = i == draws.len()
            || (use_layer && layerable(draws[i].state.blend) != layerable(draws[start].state.blend));
        if split {
            out.push(Segment {
                layer: use_layer && layerable(draws[start].state.blend),
                draws: &draws[start..i],
            });
            start = i;
        }
    }
    out
}

impl ImmediateRenderer {
    /// A renderer with no device state yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the next [`Self::record`] to skip the vertex and index copy when `generation`
    /// and these buffer lengths match the previous upload.
    ///
    /// Call it immediately before [`Self::record`]. `generation == 0` never matches, so a caller
    /// that does not track a generation uploads every time. A new generation, a change in vertex
    /// or index length, or a new GPU device uploads again. The arm applies to one record.
    pub fn arm_upload(&mut self, generation: u64, vertices: usize, indices: usize) {
        self.pending_upload = Some(UploadKey {
            generation,
            vertices,
            indices,
        });
    }

    /// Vertex and index uploads performed after [`Self::arm_upload`].
    #[must_use]
    pub fn geometry_upload_count(&self) -> u64 {
        self.geometry_uploads
    }

    /// Registers (or replaces) image `id`; it uploads on the next [`Self::record`].
    pub fn set_image(&mut self, id: u64, image: ImmediateImage) {
        self.images.insert(
            id,
            StoredImage {
                image,
                dirty: true,
                warned: false,
            },
        );
    }

    /// Removes image `id`; draws that still name it are skipped.
    pub fn remove_image(&mut self, id: u64) {
        self.images.remove(&id);
        if let Some(state) = &mut self.state {
            state.images.remove(&id);
        }
    }

    /// True when image `id` is registered.
    #[must_use]
    pub fn has_image(&self, id: u64) -> bool {
        self.images.contains_key(&id)
    }

    /// Records `batch` over `target`. `clip_from_local` maps the batch's positions to clip space;
    /// `samples` is the anti-aliasing sample count of the layer (1 draws straight onto the
    /// target, unsmoothed). An empty batch records nothing.
    pub fn record(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        target: &ImmediateTarget<'_>,
        batch: &ImmediateBatch,
        clip_from_local: Mat4,
        samples: u32,
    ) -> Result<(), RenderError> {
        let pending = self.pending_upload.take();
        if batch.is_empty() || target.width == 0 || target.height == 0 {
            return Ok(());
        }
        let _span = tracing::info_span!("immediate_draw").entered();
        let _crumb = crate::breadcrumbs::frame_pass("immediate_draw");
        let tracked = TrackedDevice::observe(&device.device);
        let mut skip_geometry_upload =
            pending.is_some_and(|next| !uploads_vertex_buffers(self.uploaded, next));
        if self
            .state
            .as_ref()
            .is_none_or(|s| s.device_id != device.device_id())
        {
            self.state = Some(DeviceState::new(device)?);
            // The new buffers do not hold the previous upload.
            self.uploaded = None;
            skip_geometry_upload = false;
            for stored in self.images.values_mut() {
                stored.dirty = true;
            }
        }
        let state = self.state.as_mut().expect("device state was created above");

        // Images: (re)upload what changed. A rejected (short or empty) buffer
        // leaves the image dirty, so a later corrected buffer still uploads;
        // the texture keeps its previous pixels and draws skip it meanwhile.
        for (id, stored) in &mut self.images {
            if !stored.dirty {
                continue;
            }
            let reuse = state.images.get_mut(id).filter(|gpu| {
                gpu.size == (stored.image.width.max(1), stored.image.height.max(1))
                    && gpu.format == stored.image.format
            });
            let copied = match reuse {
                Some(gpu) => {
                    let copied = write_image(device, &gpu._texture, &stored.image);
                    gpu.ready = copied;
                    copied
                }
                None => {
                    let gpu = upload_image(device, &tracked, &state.layouts, &state.sampler, &stored.image);
                    let copied = gpu.ready;
                    state.images.insert(*id, gpu);
                    copied
                }
            };
            if copied {
                stored.dirty = false;
            } else if !stored.warned {
                stored.warned = true;
                let row = stored.image.width * stored.image.format.bytes_per_pixel();
                let expected = row as usize * stored.image.height as usize;
                tracing::warn!(
                    target: "regia_render",
                    "immediate image {}x{} has {} bytes (needs {expected}); not uploaded",
                    stored.image.width,
                    stored.image.height,
                    stored.image.data.len()
                );
            }
        }

        let mut draws = batch.draws().to_vec();
        let open_from = draws.last().map_or(0, |d| d.indices.end);
        if (batch.indices().len() as u32) > open_from {
            draws.push(ImmediateDraw {
                state: batch.state(),
                indices: open_from..batch.indices().len() as u32,
            });
        }

        let reused = if skip_geometry_upload {
            match (&state.vertex, &state.index) {
                (Some((vertex, _)), Some((index, _))) => Some((vertex.clone(), index.clone())),
                _ => None,
            }
        } else {
            None
        };
        let uploaded_now = reused.is_none();
        let (vertex, index) = if let Some(pair) = reused {
            pair
        } else {
            (
                DeviceState::upload(
                    device,
                    &tracked,
                    &mut state.vertex,
                    bytemuck::cast_slice(batch.vertices()),
                    wgpu::BufferUsages::VERTEX,
                    "immediate vertices",
                ),
                DeviceState::upload(
                    device,
                    &tracked,
                    &mut state.index,
                    bytemuck::cast_slice(batch.indices()),
                    wgpu::BufferUsages::INDEX,
                    "immediate indices",
                ),
            )
        };
        let encode = shader_encodes(target.format);
        device
            .queue
            .write_buffer(&state.views[0].0, 0, &view_bytes(clip_from_local, false));
        device
            .queue
            .write_buffer(&state.views[1].0, 0, &view_bytes(clip_from_local, encode));

        let samples = if samples > 1 && layer_samples_supported(device, samples) {
            samples
        } else {
            1
        };
        let use_layer = samples > 1;
        for segment in segments(&draws, use_layer) {
            if segment.layer {
                let (msaa, resolve_view, resolve_bind) = {
                    let layer = state.layer(&tracked, (target.width, target.height), samples);
                    (
                        layer.msaa.clone(),
                        layer.resolve.clone(),
                        layer.resolve_bind.clone(),
                    )
                };
                {
                    let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("immediate layer"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &msaa,
                            depth_slice: None,
                            resolve_target: Some(&resolve_view),
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Discard,
                            },
                        })],
                        ..Default::default()
                    });
                    draw_segment(
                        state,
                        &tracked,
                        &mut pass,
                        segment.draws,
                        &vertex,
                        &index,
                        LAYER_FORMAT,
                        samples,
                        true,
                        target,
                    );
                }
                let composite = state.composite(&tracked, target.format);
                let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("immediate composite"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.counted_set_pipeline(&composite);
                pass.counted_set_bind_group(0, &state.views[1].1, &[]);
                pass.counted_set_bind_group(1, &resolve_bind, &[]);
                pass.counted_draw(0..3, 0..1);
            } else {
                let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("immediate direct"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                draw_segment(
                    state,
                    &tracked,
                    &mut pass,
                    segment.draws,
                    &vertex,
                    &index,
                    target.format,
                    1,
                    false,
                    target,
                );
            }
        }
        // `state` is no longer borrowed, so the upload key can move onto the renderer.
        // An unarmed record stores `None`, which forces the next armed record to upload.
        if uploaded_now {
            if pending.is_some() {
                self.geometry_uploads = self.geometry_uploads.saturating_add(1);
            }
            self.uploaded = pending;
        }
        Ok(())
    }
}

/// True when the layer format supports `samples` on this adapter.
fn layer_samples_supported(device: &RenderDevice, samples: u32) -> bool {
    device
        .adapter
        .get_texture_format_features(LAYER_FORMAT)
        .flags
        .sample_count_supported(samples)
}

#[allow(clippy::too_many_arguments)] // one pass's worth of shared state
fn draw_segment(
    state: &mut DeviceState,
    tracked: &TrackedDevice,
    pass: &mut wgpu::RenderPass<'_>,
    draws: &[ImmediateDraw],
    vertex: &wgpu::Buffer,
    index: &wgpu::Buffer,
    format: wgpu::TextureFormat,
    samples: u32,
    layer: bool,
    target: &ImmediateTarget<'_>,
) {
    pass.set_vertex_buffer(0, vertex.slice(..));
    pass.set_index_buffer(index.slice(..), wgpu::IndexFormat::Uint32);
    pass.counted_set_bind_group(0, &state.views[usize::from(!layer)].1, &[]);
    let mut current: Option<PipelineKey> = None;
    for draw in draws {
        let (bind, mask) = match draw.state.texture {
            ImmediateTexture::White => (state.white.clone(), false),
            ImmediateTexture::Image(id) | ImmediateTexture::Mask(id) => match state.images.get(&id) {
                // An image whose latest upload was rejected has no pixels of
                // the current buffer: skip the draw instead of sampling an
                // uninitialized or stale texture.
                Some(gpu) if gpu.ready => (
                    gpu.bind_group.clone(),
                    matches!(draw.state.texture, ImmediateTexture::Mask(_)),
                ),
                Some(_) => {
                    tracing::debug!(target: "regia_render", "immediate image {id} has no uploaded pixels yet; skipped");
                    continue;
                }
                None => {
                    tracing::debug!(target: "regia_render", "immediate draw names unknown image {id}; skipped");
                    continue;
                }
            },
        };
        let key = PipelineKey {
            format,
            samples,
            blend: draw.state.blend,
            mask,
            layer,
        };
        if current != Some(key) {
            let pipeline = state.pipeline(tracked, key);
            pass.counted_set_pipeline(&pipeline);
            current = Some(key);
        }
        let [x, y, w, h] = draw.state.scissor.unwrap_or([0, 0, target.width, target.height]);
        let x0 = x.min(target.width);
        let y0 = y.min(target.height);
        let w = w.min(target.width - x0);
        let h = h.min(target.height - y0);
        if w == 0 || h == 0 {
            continue;
        }
        pass.set_scissor_rect(x0, y0, w, h);
        pass.counted_set_bind_group(1, &bind, &[]);
        pass.counted_draw_indexed(draw.indices.clone(), 0, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::immediate::{ImmediateState, ImmediateVertex};

    fn draw(blend: ImmediateBlend, range: std::ops::Range<u32>) -> ImmediateDraw {
        ImmediateDraw {
            state: ImmediateState {
                blend,
                ..Default::default()
            },
            indices: range,
        }
    }

    #[test]
    fn segments_split_on_layerability_only_with_a_layer() {
        let draws = [
            draw(ImmediateBlend::Alpha, 0..3),
            draw(ImmediateBlend::Additive, 3..6),
            draw(ImmediateBlend::Multiply, 6..9),
            draw(ImmediateBlend::Alpha, 9..12),
        ];
        let s = segments(&draws, true);
        assert_eq!(
            s.iter().map(|s| (s.layer, s.draws.len())).collect::<Vec<_>>(),
            vec![(true, 2), (false, 1), (true, 1)]
        );
        let s = segments(&draws, false);
        assert_eq!(s.len(), 1);
        assert!(!s[0].layer);
        assert!(segments(&[], true).is_empty());
    }

    /// A short pixel buffer must not mark the image uploaded: the texture then
    /// holds uninitialized (or stale) pixels while draws sample it, and a later
    /// same-size short buffer keeps the image dirty instead of hiding the miss.
    #[test]
    fn a_short_image_buffer_never_marks_the_image_uploaded() {
        let device = crate::test_device();
        let (w, h) = (8u32, 8u32);
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("immediate short image test"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let proj = Mat4::from_translation(regia_core::math::Vec3::new(-1.0, 1.0, 0.0))
            * Mat4::from_scale(regia_core::math::Vec3::new(2.0 / w as f32, -2.0 / h as f32, 1.0));
        let mut renderer = ImmediateRenderer::new();
        let short = |bytes: &[u8]| ImmediateImage {
            width: 4,
            height: 4,
            format: ImmediateImageFormat::Rgba8SrgbPremultiplied,
            data: Arc::from(bytes),
        };
        let record = |renderer: &mut ImmediateRenderer, encoder: &mut wgpu::CommandEncoder| {
            let mut batch = ImmediateBatch::new();
            batch.triangle_list_2d(&[[0.0, 0.0], [8.0, 0.0], [0.0, 8.0]], [1.0, 1.0, 1.0, 1.0]);
            batch.set_texture(ImmediateTexture::Image(7));
            let target = ImmediateTarget {
                view: &view,
                format,
                width: w,
                height: h,
            };
            renderer
                .record(&device, encoder, &target, &batch, proj, 1)
                .expect("record");
        };
        // 4×4 RGBA8 needs 64 bytes; ten leave the texture without pixels.
        renderer.set_image(7, short(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));
        let mut encoder = device.device.create_command_encoder(&Default::default());
        record(&mut renderer, &mut encoder);
        assert!(
            renderer.images[&7].dirty,
            "a failed copy must leave the image dirty"
        );
        assert!(
            !renderer.state.as_ref().expect("state").images[&7].ready,
            "the texture never received the requested bytes"
        );
        // A second short buffer of the same size: the miss must not be hidden.
        renderer.set_image(7, short(&[10, 9, 8, 7, 6, 5, 4, 3, 2, 1]));
        let mut encoder = device.device.create_command_encoder(&Default::default());
        record(&mut renderer, &mut encoder);
        assert!(
            renderer.images[&7].dirty,
            "a second failed copy must leave the image dirty"
        );
        assert!(!renderer.state.as_ref().expect("state").images[&7].ready);
        // The full buffer uploads and clears the flag; the texture is sampled.
        renderer.set_image(7, short(&[42; 64]));
        let mut encoder = device.device.create_command_encoder(&Default::default());
        record(&mut renderer, &mut encoder);
        assert!(!renderer.images[&7].dirty, "the full buffer uploads");
        assert!(renderer.state.as_ref().expect("state").images[&7].ready);
        // A 0×0 image takes the same guard: nothing uploads, the flag stays.
        renderer.set_image(
            7,
            ImmediateImage {
                width: 0,
                height: 0,
                format: ImmediateImageFormat::Rgba8SrgbPremultiplied,
                data: Arc::from([].as_slice()),
            },
        );
        let mut encoder = device.device.create_command_encoder(&Default::default());
        record(&mut renderer, &mut encoder);
        assert!(renderer.images[&7].dirty, "0x0 uploads nothing");
        assert!(!renderer.state.as_ref().expect("state").images[&7].ready);
    }

    /// Renders a red triangle and a white mask quad over grey; checks colours and that the
    /// empty batch leaves the target untouched. Runs on any adapter (lavapipe in CI).
    #[test]
    fn draws_over_the_frame_and_empty_costs_nothing() {
        let device = crate::test_device();
        let (w, h) = (32u32, 32u32);
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("immediate test"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut renderer = ImmediateRenderer::new();
        renderer.set_image(
            9,
            ImmediateImage {
                width: 2,
                height: 1,
                format: ImmediateImageFormat::R8,
                data: Arc::from([255u8, 0].as_slice()),
            },
        );
        // Pixel space, y down: clip = (2x/w − 1, 1 − 2y/h).
        let proj = Mat4::from_translation(regia_core::math::Vec3::new(-1.0, 1.0, 0.0))
            * Mat4::from_scale(regia_core::math::Vec3::new(2.0 / w as f32, -2.0 / h as f32, 1.0));
        for (samples, empty) in [(1, false), (4, false), (4, true)] {
            let mut encoder = device.device.create_command_encoder(&Default::default());
            crate::surface::encode_clear(&mut encoder, &view, [0.2, 0.2, 0.2, 1.0]);
            let mut batch = ImmediateBatch::new();
            if !empty {
                batch.triangle_list_2d(&[[0.0, 0.0], [16.0, 0.0], [0.0, 16.0]], [1.0, 0.0, 0.0, 1.0]);
                batch.set_texture(ImmediateTexture::Mask(9));
                let white = [1.0, 1.0, 1.0, 1.0];
                batch.triangles(
                    &[
                        ImmediateVertex::textured([16.0, 16.0, 0.0], [0.25, 0.5], white),
                        ImmediateVertex::textured([32.0, 16.0, 0.0], [0.25, 0.5], white),
                        ImmediateVertex::textured([32.0, 32.0, 0.0], [0.25, 0.5], white),
                        ImmediateVertex::textured([16.0, 32.0, 0.0], [0.25, 0.5], white),
                    ],
                    &[0, 1, 2, 0, 2, 3],
                );
            }
            let target = ImmediateTarget {
                view: &view,
                format,
                width: w,
                height: h,
            };
            renderer
                .record(&device, &mut encoder, &target, &batch, proj, samples)
                .expect("record");
            let readback = crate::screenshot::encode_copy(&device, &mut encoder, &texture, format);
            device.queue.submit([encoder.finish()]);
            let image = crate::screenshot::finish(&device, readback).expect("readback");
            let px = |x: u32, y: u32| {
                let i = ((y * w + x) * 4) as usize;
                [image.rgba[i], image.rgba[i + 1], image.rgba[i + 2]]
            };
            let grey = px(28, 4);
            assert!(
                grey[0] > 100 && grey[0] < 140 && grey[0] == grey[1],
                "{grey:?} (samples {samples})"
            );
            if empty {
                assert_eq!(px(3, 3), grey, "empty batch drew");
                assert_eq!(px(24, 24), grey, "empty batch drew");
            } else {
                assert_eq!(px(3, 3), [255, 0, 0], "samples {samples}");
                assert!(
                    px(24, 24).iter().all(|c| *c > 250),
                    "mask texel 1.0: {:?}",
                    px(24, 24)
                );
            }
        }
    }

    #[test]
    fn perf_repeated_upload_is_skipped() {
        let key = UploadKey {
            generation: 1,
            vertices: 4,
            indices: 6,
        };
        assert!(
            !uploads_vertex_buffers(Some(key), key),
            "the same generation and lengths are already on the GPU"
        );
        assert!(uploads_vertex_buffers(None, key), "the first upload runs");
        let zero = UploadKey {
            generation: 0,
            vertices: 4,
            indices: 6,
        };
        assert!(
            uploads_vertex_buffers(Some(zero), zero),
            "generation 0 always uploads"
        );
        let longer = UploadKey {
            generation: 1,
            vertices: 8,
            indices: 6,
        };
        assert!(
            uploads_vertex_buffers(Some(key), longer),
            "a longer batch uploads"
        );
    }
}
