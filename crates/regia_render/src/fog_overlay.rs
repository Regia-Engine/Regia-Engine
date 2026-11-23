// SPDX-License-Identifier: MPL-2.0
//! Fog of war world overlay (audit gap FW-1): [`FogOverlayPass`] darkens the scene by the viewer's
//! fog texture at the `before_post` slot, through the frame's pass extension point
//! ([`crate::FramePass`]); the frame's own passes are untouched.
//!
//! The texture is one R8 texel per fog cell (0 unexplored, 128 explored, 255 visible, values in
//! between while a cell fades), row-major from the grid's south-west corner (row = +Z, column =
//! +X). The host (the framework's fog presentation) feeds it through a [`FogOverlayHandle`]:
//! [`FogOverlayHandle::set_grid`] sizes and places it, [`FogOverlayHandle::write_rows`] hands over
//! changed rows only, and the pass uploads just those rows (one copy per contiguous run) on its
//! next frame. A grid of another size recreates the texture and uploads it whole.
//!
//! The shader (`shaders/fog_of_war_overlay.wgsl`) reconstructs each pixel's world position from
//! the scene depth, samples the texture at world XZ with a 3 x 3 tent filter whose tap spacing is
//! [`FogLook::softness`] cells (soft edges), and multiplies the HDR image by a colour between the
//! unexplored and explored looks and white (visible). The sky is left alone; ground outside the
//! grid reads as unexplored.
//!
//! Off by default: the pass exists only after [`FrameRenderer::enable_fog_overlay`], and a
//! registered pass with [`FogLook::enabled`] false, or before any grid, does no GPU work.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_app::prelude::*;

use crate::frame::FrameRenderer;
use crate::{FramePass, FramePassContext, GraphSlot, TrackedDevice};

/// Name of the pass in `render.graph`.
pub const FOG_OVERLAY_PASS_NAME: &str = "fog_of_war_overlay";

/// WGSL source of the fog overlay shader.
pub const FOG_OVERLAY_WGSL: &str = include_str!("../shaders/fog_of_war_overlay.wgsl");

/// Largest grid edge the overlay uploads, in texels (the fog grid's own limit is 4096).
pub const FOG_OVERLAY_MAX_EDGE: u32 = 4096;

/// How the fog overlay looks. A resource: the framework's fog presentation copies it into the
/// overlay every frame. Off by default.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct FogLook {
    /// Draw the overlay.
    pub enabled: bool,
    /// Overall strength, 0 (no fog drawn) to 1.
    pub strength: f32,
    /// Darkening of explored cells, 0 (none) to 1 (black).
    pub explored_darkness: f32,
    /// Linear RGB tint of explored cells (multiplied in at full strength).
    pub explored_tint: [f32; 3],
    /// Darkening of unexplored cells, 0 to 1.
    pub unexplored_darkness: f32,
    /// Linear RGB tint of unexplored cells.
    pub unexplored_tint: [f32; 3],
    /// Edge softness: the filter's tap spacing in cells, 0 (bilinear only) to 2.
    pub softness: f32,
}

impl Default for FogLook {
    fn default() -> Self {
        Self {
            enabled: false,
            strength: 1.0,
            explored_darkness: 0.55,
            explored_tint: [0.8, 0.85, 1.0],
            unexplored_darkness: 0.92,
            unexplored_tint: [1.0, 1.0, 1.0],
            softness: 0.75,
        }
    }
}

impl FogLook {
    /// The default look, switched on.
    #[must_use]
    pub fn on() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// The multiplier the overlay applies to a pixel whose filtered texel is `texel` (0–255), with
    /// NaN-safe clamps: the CPU twin of the shader's mapping (tests and the editor's legend).
    #[must_use]
    pub fn multiplier(&self, texel: f32) -> [f32; 3] {
        let clamp01 = |v: f32| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let strength = clamp01(self.strength);
        let look = |tint: [f32; 3], dark: f32| {
            let keep = 1.0 - clamp01(dark);
            tint.map(|t| 1.0 + (clamp01(t) * keep - 1.0) * strength)
        };
        let unexplored = look(self.unexplored_tint, self.unexplored_darkness);
        let explored = look(self.explored_tint, self.explored_darkness);
        let v = clamp01(texel / 255.0);
        let mid = 128.0 / 255.0;
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t);
        if v < mid {
            lerp(unexplored, explored, v / mid)
        } else {
            lerp(explored, [1.0; 3], (v - mid) / (1.0 - mid))
        }
    }
}

/// Counters of the overlay (`fog.texture_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FogOverlayStats {
    /// Frames the overlay drew.
    pub draws: u64,
    /// Textures created (the first grid, then each change of size).
    pub texture_creates: u64,
    /// Rows uploaded so far.
    pub rows_uploaded: u64,
    /// Bytes uploaded so far (texel bytes, without row padding).
    pub bytes_uploaded: u64,
    /// Rows uploaded by the last frame that uploaded.
    pub last_rows: u32,
    /// Row-run copies so far (one per contiguous run of changed rows).
    pub copies: u64,
    /// Frames skipped for an error (logged by the frame).
    pub failures: u64,
}

struct Shared {
    look: FogLook,
    cols: u32,
    rows: u32,
    origin: [f32; 2],
    size: [f32; 2],
    /// The CPU mirror of the texture, row-major.
    texels: Vec<u8>,
    /// One bit per row waiting for upload.
    pending: Vec<u64>,
    stats: FogOverlayStats,
}

/// The overlay's state shared between the host and the pass (clones share it). A resource, so a
/// game system can find it; [`FrameRenderer::enable_fog_overlay`] registers the pass drawing it.
#[derive(Resource, Clone)]
pub struct FogOverlayHandle {
    inner: Arc<Mutex<Shared>>,
}

impl Default for FogOverlayHandle {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Shared {
                look: FogLook::default(),
                cols: 0,
                rows: 0,
                origin: [0.0; 2],
                size: [0.0; 2],
                texels: Vec::new(),
                pending: Vec::new(),
                stats: FogOverlayStats::default(),
            })),
        }
    }
}

impl std::fmt::Debug for FogOverlayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.lock();
        f.debug_struct("FogOverlayHandle")
            .field("look", &s.look)
            .field("cols", &s.cols)
            .field("rows", &s.rows)
            .finish_non_exhaustive()
    }
}

impl FogOverlayHandle {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the look.
    pub fn set_look(&self, look: FogLook) {
        self.lock().look = look;
    }

    /// The current look.
    #[must_use]
    pub fn look(&self) -> FogLook {
        self.lock().look
    }

    /// Sizes and places the texture: `cols` x `rows` texels covering `size` metres from the
    /// south-west corner `origin` (world X, Z). A new size clears the texture to unexplored and
    /// marks every row for upload; a new place only moves it. A size of 0 or past
    /// [`FOG_OVERLAY_MAX_EDGE`] is ignored.
    pub fn set_grid(&self, cols: u32, rows: u32, origin: [f32; 2], size: [f32; 2]) {
        if cols == 0 || rows == 0 || cols > FOG_OVERLAY_MAX_EDGE || rows > FOG_OVERLAY_MAX_EDGE {
            return;
        }
        let mut s = self.lock();
        if s.cols != cols || s.rows != rows {
            s.cols = cols;
            s.rows = rows;
            s.texels = vec![0; cols as usize * rows as usize];
            s.pending = vec![u64::MAX; (rows as usize).div_ceil(64)];
        }
        s.origin = origin;
        s.size = size;
    }

    /// Copies rows `first_row..first_row + rows` from `bytes` (`rows * cols` bytes, row-major)
    /// into the mirror and marks them for upload. Rows past the grid, or a short `bytes`, are
    /// cut off.
    pub fn write_rows(&self, first_row: u32, rows: u32, bytes: &[u8]) {
        let mut s = self.lock();
        let cols = s.cols as usize;
        if cols == 0 {
            return;
        }
        for (k, src) in bytes.chunks(cols).take(rows as usize).enumerate() {
            let row = first_row as usize + k;
            if row >= s.rows as usize || src.len() != cols {
                break;
            }
            s.texels[row * cols..(row + 1) * cols].copy_from_slice(src);
            if let Some(word) = s.pending.get_mut(row / 64) {
                *word |= 1u64 << (row % 64);
            }
        }
    }

    /// Rows waiting for the next frame's upload.
    #[must_use]
    pub fn pending_rows(&self) -> usize {
        let s = self.lock();
        pending_runs(&s.pending, s.rows)
            .iter()
            .map(|(_, count)| *count as usize)
            .sum()
    }

    /// `(cols, rows)` of the texture (0 before the first grid).
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        let s = self.lock();
        (s.cols, s.rows)
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> FogOverlayStats {
        self.lock().stats
    }
}

struct Gpu {
    layout: wgpu::BindGroupLayout,
    layout_ms: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    pipeline_layout_ms: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    /// The variant reading a multisampled depth, built on first use.
    module_ms: Option<wgpu::ShaderModule>,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    /// `(format, multisampled depth, pipeline)`.
    pipelines: Vec<(wgpu::TextureFormat, bool, wgpu::RenderPipeline)>,
}

struct FogImage {
    cols: u32,
    rows: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// Draws a [`FogOverlayHandle`]'s fog over the scene at the `before_post` slot.
pub struct FogOverlayPass {
    handle: FogOverlayHandle,
    gpu: Option<Gpu>,
    image: Option<FogImage>,
}

impl std::fmt::Debug for FogOverlayPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FogOverlayPass")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

/// Bytes of the uniform block: a matrix and five `vec4`s.
const UNIFORM_BYTES: u64 = 64 + 5 * 16;

/// The shader for a multisampled scene depth: the same source with the depth binding's type
/// swapped (its `textureLoad` third argument becomes the sample index, 0).
fn multisampled_source() -> String {
    FOG_OVERLAY_WGSL.replace("texture_depth_2d", "texture_depth_multisampled_2d")
}

/// The rows of `pending` as contiguous runs `(first, count)`.
fn pending_runs(pending: &[u64], rows: u32) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for row in 0..rows {
        let on = pending
            .get(row as usize / 64)
            .is_some_and(|w| w & (1u64 << (row % 64)) != 0);
        if !on {
            continue;
        }
        match runs.last_mut() {
            Some((first, count)) if *first + *count == row => *count += 1,
            _ => runs.push((row, 1)),
        }
    }
    runs
}

impl FogOverlayPass {
    /// A pass drawing `handle`.
    #[must_use]
    pub fn new(handle: FogOverlayHandle) -> Self {
        Self {
            handle,
            gpu: None,
            image: None,
        }
    }

    fn gpu(&mut self, device: &TrackedDevice) -> &mut Gpu {
        self.gpu.get_or_insert_with(|| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("fog_of_war_overlay.wgsl"),
                source: wgpu::ShaderSource::Wgsl(FOG_OVERLAY_WGSL.into()),
            });
            let entries = |multisampled: bool| {
                [
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: wgpu::BufferSize::new(UNIFORM_BYTES),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
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
                            sample_type: wgpu::TextureSampleType::Depth,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled,
                        },
                        count: None,
                    },
                ]
            };
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("fog overlay bgl"),
                entries: &entries(false),
            });
            let layout_ms = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("fog overlay bgl (msaa depth)"),
                entries: &entries(true),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("fog overlay layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let pipeline_layout_ms = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("fog overlay layout (msaa depth)"),
                bind_group_layouts: &[Some(&layout_ms)],
                immediate_size: 0,
            });
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("fog overlay sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("fog overlay uniforms"),
                size: UNIFORM_BYTES,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            Gpu {
                layout,
                layout_ms,
                pipeline_layout,
                pipeline_layout_ms,
                module,
                module_ms: None,
                sampler,
                uniforms,
                pipelines: Vec::new(),
            }
        })
    }

    fn pipeline(
        gpu: &mut Gpu,
        device: &TrackedDevice,
        format: wgpu::TextureFormat,
        multisampled: bool,
    ) -> wgpu::RenderPipeline {
        if let Some((_, _, p)) = gpu
            .pipelines
            .iter()
            .find(|(f, ms, _)| *f == format && *ms == multisampled)
        {
            return p.clone();
        }
        if multisampled && gpu.module_ms.is_none() {
            gpu.module_ms = Some(device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("fog_of_war_overlay.wgsl (msaa depth)"),
                source: wgpu::ShaderSource::Wgsl(multisampled_source().into()),
            }));
        }
        let module = match (&gpu.module_ms, multisampled) {
            (Some(ms), true) => ms,
            _ => &gpu.module,
        };
        // dst * src for colour, dst kept for alpha.
        let blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Dst,
                dst_factor: wgpu::BlendFactor::Zero,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fog of war overlay"),
            layout: Some(if multisampled {
                &gpu.pipeline_layout_ms
            } else {
                &gpu.pipeline_layout
            }),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&**device).as_deref(),
        });
        gpu.pipelines.push((format, multisampled, pipeline.clone()));
        pipeline
    }

    /// Creates the texture when the grid size changed and uploads the pending rows (one copy per
    /// contiguous run). Returns the rows uploaded.
    fn upload(&mut self, ctx: &mut FramePassContext<'_>, s: &mut Shared) -> u32 {
        let (cols, rows) = (s.cols, s.rows);
        if !self
            .image
            .as_ref()
            .is_some_and(|i| i.cols == cols && i.rows == rows)
        {
            let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("fog of war texture"),
                size: wgpu::Extent3d {
                    width: cols,
                    height: rows,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.image = Some(FogImage {
                cols,
                rows,
                texture,
                view,
            });
            s.pending.fill(u64::MAX);
            s.stats.texture_creates += 1;
        }
        let runs = pending_runs(&s.pending, rows);
        if runs.is_empty() {
            return 0;
        }
        let Some(image) = self.image.as_ref() else {
            return 0;
        };
        let pitch = cols.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let total: u32 = runs.iter().map(|r| r.1).sum();
        let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fog of war staging"),
            size: u64::from(pitch) * u64::from(total),
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut padded = vec![0u8; pitch as usize * total as usize];
        let mut at = 0usize;
        for &(first, count) in &runs {
            for row in first..first + count {
                let src = &s.texels[row as usize * cols as usize..(row as usize + 1) * cols as usize];
                padded[at * pitch as usize..at * pitch as usize + cols as usize].copy_from_slice(src);
                at += 1;
            }
        }
        ctx.write_buffer(&staging, 0, &padded);
        let mut offset = 0u64;
        for &(first, count) in &runs {
            ctx.encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset,
                        bytes_per_row: Some(pitch),
                        rows_per_image: Some(count),
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &image.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: first, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: cols,
                    height: count,
                    depth_or_array_layers: 1,
                },
            );
            offset += u64::from(pitch) * u64::from(count);
        }
        s.pending.fill(0);
        s.stats.rows_uploaded += u64::from(total);
        s.stats.bytes_uploaded += u64::from(total) * u64::from(cols);
        s.stats.last_rows = total;
        s.stats.copies += runs.len() as u64;
        total
    }

    fn record_inner(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let handle = self.handle.clone();
        let mut s = handle.lock();
        if !s.look.enabled || s.cols == 0 || s.rows == 0 {
            return Ok(());
        }
        let _span = tracing::trace_span!("fog_of_war_overlay").entered();
        let device = ctx.device;
        self.upload(ctx, &mut s);
        let look = s.look;
        let world_from_clip = ctx.clip_from_world.inverse();
        let view = ctx.viewport;
        let mut floats: Vec<f32> = world_from_clip.to_cols_array().to_vec();
        floats.extend_from_slice(&view);
        floats.extend_from_slice(&[s.origin[0], s.origin[1], s.size[0], s.size[1]]);
        let softness = if look.softness.is_finite() {
            look.softness.clamp(0.0, 2.0)
        } else {
            0.0
        };
        let strength = if look.strength.is_finite() {
            look.strength.clamp(0.0, 1.0)
        } else {
            0.0
        };
        floats.extend_from_slice(&[s.cols as f32, s.rows as f32, softness, strength]);
        let unit = |v: f32| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let t = look.explored_tint.map(unit);
        floats.extend_from_slice(&[t[0], t[1], t[2], unit(look.explored_darkness)]);
        let t = look.unexplored_tint.map(unit);
        floats.extend_from_slice(&[t[0], t[1], t[2], unit(look.unexplored_darkness)]);
        let bytes: Vec<u8> = floats.iter().flat_map(|v| v.to_le_bytes()).collect();
        let multisampled = ctx.depth_multisampled;
        let format = ctx.color_format;
        let pipeline = {
            let gpu = self.gpu(device);
            ctx.write_buffer(&gpu.uniforms, 0, &bytes);
            Self::pipeline(gpu, device, format, multisampled)
        };
        let gpu = self.gpu.as_ref().ok_or("fog overlay pipeline missing")?;
        let image = self.image.as_ref().ok_or("fog overlay texture missing")?;
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fog overlay bg"),
            layout: if multisampled { &gpu.layout_ms } else { &gpu.layout },
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: gpu.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&image.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(ctx.depth),
                },
            ],
        });
        {
            let mut pass = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fog of war overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: ctx.color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_viewport(view[0], view[1], view[2].max(1.0), view[3].max(1.0), 0.0, 1.0);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        s.stats.draws += 1;
        Ok(())
    }
}

impl FramePass for FogOverlayPass {
    fn name(&self) -> &str {
        FOG_OVERLAY_PASS_NAME
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::BeforePost
    }

    fn reads(&self) -> &[&'static str] {
        &["color", "depth"]
    }

    fn writes(&self) -> &[&'static str] {
        &["color"]
    }

    fn pipelines(&self) -> &[&'static str] {
        &["fog_of_war_overlay"]
    }

    fn writes_color(&self, post: Option<&crate::PostSettings>) -> bool {
        let _ = post;
        let s = self.handle.lock();
        s.look.enabled && s.cols > 0
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let result = self.record_inner(ctx);
        if result.is_err() {
            self.handle.lock().stats.failures += 1;
        }
        result
    }

    fn prewarm(&mut self, device: &TrackedDevice) -> Result<usize, String> {
        let fresh = self.gpu.is_none();
        let _ = self.gpu(device);
        Ok(usize::from(fresh))
    }
}

impl FrameRenderer {
    /// Registers the fog of war overlay pass ([`FogOverlayPass`]) drawing `handle` when it is not
    /// registered yet; returns `true` when it registered it now. Off until the handle's look is
    /// enabled and a grid is set.
    pub fn enable_fog_overlay(&mut self, handle: &FogOverlayHandle) -> bool {
        if self
            .registered_passes()
            .iter()
            .any(|name| name == FOG_OVERLAY_PASS_NAME)
        {
            return false;
        }
        self.register_pass(Box::new(FogOverlayPass::new(handle.clone())))
            .is_ok()
    }

    /// Removes the fog overlay pass; returns whether it was registered.
    pub fn disable_fog_overlay(&mut self) -> bool {
        self.remove_pass(FOG_OVERLAY_PASS_NAME).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_queue_and_coalesce() {
        let h = FogOverlayHandle::default();
        h.write_rows(0, 1, &[1, 2]);
        assert_eq!(h.pending_rows(), 0, "no grid yet: ignored");
        h.set_grid(4, 70, [0.0, 0.0], [4.0, 70.0]);
        assert_eq!(h.pending_rows(), 70, "a new texture uploads whole");
        let s = h.lock();
        assert_eq!(pending_runs(&s.pending, 70), vec![(0, 70)]);
        drop(s);
        h.lock().pending.fill(0);
        h.write_rows(2, 2, &[9; 8]);
        h.write_rows(65, 1, &[7; 4]);
        h.write_rows(69, 3, &[5; 12]);
        let s = h.lock();
        assert_eq!(pending_runs(&s.pending, 70), vec![(2, 2), (65, 1), (69, 1)]);
        assert_eq!(&s.texels[8..16], &[9; 8]);
        drop(s);
        // A move keeps the texels; a resize clears them.
        h.set_grid(4, 70, [5.0, 5.0], [4.0, 70.0]);
        assert_eq!(h.pending_rows(), 4);
        h.set_grid(0, 70, [5.0, 5.0], [4.0, 70.0]);
        assert_eq!(h.size(), (4, 70));
        h.set_grid(8, 8, [5.0, 5.0], [4.0, 70.0]);
        assert_eq!(h.pending_rows(), 8);
    }

    #[test]
    fn the_msaa_variant_swaps_only_the_depth_type() {
        let ms = multisampled_source();
        assert!(ms.contains("var scene_depth: texture_depth_multisampled_2d;"));
        assert!(!ms.contains("texture_depth_2d"));
        let parsed = naga::front::wgsl::parse_str(&ms);
        assert!(parsed.is_ok(), "{parsed:?}");
        assert!(naga::front::wgsl::parse_str(FOG_OVERLAY_WGSL).is_ok());
    }

    #[test]
    fn the_multiplier_orders_the_three_states() {
        let look = FogLook::on();
        let lum = |m: [f32; 3]| (m[0] + m[1] + m[2]) / 3.0;
        let (u, e, v) = (
            lum(look.multiplier(0.0)),
            lum(look.multiplier(128.0)),
            lum(look.multiplier(255.0)),
        );
        assert!(u < e && e < v, "{u} {e} {v}");
        assert_eq!(look.multiplier(255.0), [1.0; 3]);
        let off = FogLook {
            strength: 0.0,
            ..FogLook::on()
        };
        assert_eq!(off.multiplier(0.0), [1.0; 3]);
        let nan = FogLook {
            strength: f32::NAN,
            ..FogLook::on()
        };
        assert_eq!(nan.multiplier(f32::NAN), [1.0; 3]);
    }
}
