// SPDX-License-Identifier: MPL-2.0
//! Reference overlay (spec 27 § 5, ADR-2463): [`ReferenceOverlayPass`] draws a reference image over
//! the display image for look reviews, at the `overlay` slot (after the post stack, display-referred).
//!
//! The modes of [`ReferenceOverlayMode`]: `blend` mixes the reference over the frame by opacity,
//! `wipe` shows the reference right of a split line and the frame left of it, `difference` draws
//! the per-channel absolute difference, and `flip` draws a perceptual colour-difference heat map
//! (a lightweight per-pixel stand-in: CIELAB HyAB distance, no spatial filtering; the authoritative
//! FLIP score stays `seeing.compare`). `off` records nothing.
//!
//! Size mismatch: the reference is fitted to the view by [`fit_rect`]. An aspect within
//! [`ASPECT_TOLERANCE`] of the view's is scaled to cover the whole view (bilinear, or read 1:1 when
//! the sizes are equal); any other aspect is letterboxed (the whole reference visible, centred) and
//! the uncovered view area shows the frame untouched.
//!
//! Off by default: the pass exists only after [`FrameRenderer::enable_reference_overlay`], and a
//! registered pass in mode `off`, or without an image, does no GPU work. The image is uploaded once
//! per [`ReferenceOverlayHandle::set_image`]; difference and flip copy the display target once per
//! frame (the target must be copyable, else the pass reports an error and the frame goes on).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::frame::FrameRenderer;
use crate::{FramePass, FramePassContext, GraphSlot, TrackedDevice};

/// Name of the pass in `render.graph`.
pub const REFERENCE_OVERLAY_PASS_NAME: &str = "reference_overlay";

/// WGSL source of the reference overlay shader.
pub const REFERENCE_OVERLAY_WGSL: &str = include_str!("../shaders/reference_overlay.wgsl");

/// Relative aspect difference up to which a reference covers the whole view instead of being
/// letterboxed.
pub const ASPECT_TOLERANCE: f32 = 0.01;

/// Longest side of a reference image the overlay accepts.
pub const MAX_REFERENCE_SIDE: u32 = 8192;

/// How the reference is drawn over the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReferenceOverlayMode {
    /// Nothing is drawn.
    #[default]
    Off,
    /// The reference mixed over the frame by opacity.
    Blend,
    /// The reference right of the split, the frame left of it.
    Wipe,
    /// The per-channel absolute difference of frame and reference.
    Difference,
    /// A perceptual colour-difference heat map (CIELAB HyAB, per pixel).
    Flip,
}

impl ReferenceOverlayMode {
    /// Every mode, in protocol order.
    pub const ALL: [Self; 5] = [Self::Off, Self::Blend, Self::Wipe, Self::Difference, Self::Flip];

    /// The protocol name: `off`, `blend`, `wipe`, `difference` or `flip`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Blend => "blend",
            Self::Wipe => "wipe",
            Self::Difference => "difference",
            Self::Flip => "flip",
        }
    }

    /// Parses a protocol name (exact, lowercase).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == name)
    }

    fn shader_code(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::Blend => 1.0,
            Self::Wipe => 2.0,
            Self::Difference => 3.0,
            Self::Flip => 4.0,
        }
    }

    fn reads_frame(self) -> bool {
        matches!(self, Self::Difference | Self::Flip)
    }
}

/// Mode, opacity and split of the overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceOverlaySettings {
    /// How the reference is drawn.
    pub mode: ReferenceOverlayMode,
    /// Mix of the overlay over the frame, `0..=1` (`blend`, `difference` and `flip`).
    pub opacity: f32,
    /// Split line of `wipe` as a fraction of the view width, `0..=1`.
    pub split: f32,
}

impl Default for ReferenceOverlaySettings {
    fn default() -> Self {
        Self {
            mode: ReferenceOverlayMode::Off,
            opacity: 1.0,
            split: 0.5,
        }
    }
}

/// A decoded reference image: tightly packed RGBA8, sRGB-encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl ReferenceImage {
    /// Checks the size and the buffer length.
    ///
    /// # Errors
    /// A message when a side is 0 or above [`MAX_REFERENCE_SIDE`], or the buffer length is wrong.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, String> {
        if width == 0 || height == 0 || width > MAX_REFERENCE_SIDE || height > MAX_REFERENCE_SIDE {
            return Err(format!(
                "reference image {width}x{height} is outside 1..={MAX_REFERENCE_SIDE} per side"
            ));
        }
        let want = width as usize * height as usize * 4;
        if rgba.len() != want {
            return Err(format!(
                "reference image {width}x{height} needs {want} bytes, got {}",
                rgba.len()
            ));
        }
        Ok(Self { width, height, rgba })
    }
}

/// The rectangle `[x, y, w, h]` (view pixels) a `ref_w` x `ref_h` reference occupies in a `view_w` x
/// `view_h` view: the whole view when the aspects agree within [`ASPECT_TOLERANCE`], else the
/// reference letterboxed (whole picture visible, centred).
#[must_use]
pub fn fit_rect(ref_w: u32, ref_h: u32, view_w: f32, view_h: f32) -> [f32; 4] {
    let whole = [0.0, 0.0, view_w.max(0.0), view_h.max(0.0)];
    if ref_w == 0 || ref_h == 0 || !(view_w > 0.0 && view_h > 0.0) {
        return whole;
    }
    let ref_aspect = ref_w as f32 / ref_h as f32;
    let view_aspect = view_w / view_h;
    if (ref_aspect / view_aspect - 1.0).abs() <= ASPECT_TOLERANCE {
        return whole;
    }
    let (w, h) = if ref_aspect > view_aspect {
        (view_w, view_w / ref_aspect)
    } else {
        (view_h * ref_aspect, view_h)
    };
    [
        ((view_w - w) * 0.5).floor(),
        ((view_h - h) * 0.5).floor(),
        w.floor().max(1.0),
        h.floor().max(1.0),
    ]
}

/// Counters of the pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReferenceOverlayStats {
    /// Frames the overlay was drawn.
    pub draws: u64,
    /// Reference uploads to the GPU (one per new image).
    pub uploads: u64,
    /// Display-target copies made for `difference` and `flip`.
    pub frame_copies: u64,
    /// Frames the pass failed on (for example a display target that cannot be copied).
    pub failures: u64,
}

#[derive(Default)]
struct Shared {
    settings: ReferenceOverlaySettings,
    image: Option<Arc<ReferenceImage>>,
    serial: u64,
    stats: ReferenceOverlayStats,
}

/// The overlay's state shared between the host (agent) and the pass (clones share it).
#[derive(Clone, Default)]
pub struct ReferenceOverlayHandle {
    inner: Arc<Mutex<Shared>>,
}

impl std::fmt::Debug for ReferenceOverlayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.lock();
        f.debug_struct("ReferenceOverlayHandle")
            .field("settings", &s.settings)
            .field("image", &s.image.as_ref().map(|i| (i.width, i.height)))
            .finish()
    }
}

impl ReferenceOverlayHandle {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the reference image (uploaded once on the next frame); `None` clears it.
    pub fn set_image(&self, image: Option<Arc<ReferenceImage>>) {
        let mut s = self.lock();
        s.image = image;
        s.serial += 1;
    }

    /// Sets mode, opacity and split.
    pub fn set_settings(&self, settings: ReferenceOverlaySettings) {
        self.lock().settings = settings;
    }

    /// The current settings.
    #[must_use]
    pub fn settings(&self) -> ReferenceOverlaySettings {
        self.lock().settings
    }

    /// The current reference image.
    #[must_use]
    pub fn image(&self) -> Option<Arc<ReferenceImage>> {
        self.lock().image.clone()
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> ReferenceOverlayStats {
        self.lock().stats
    }
}

struct Gpu {
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    dummy: wgpu::TextureView,
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
}

struct Picture {
    serial: u64,
    /// Kept alive for the views.
    _texture: wgpu::Texture,
    unorm: wgpu::TextureView,
    srgb: wgpu::TextureView,
    /// Kept alive until the frame's copy has run.
    _staging: wgpu::Buffer,
}

struct FrameCopy {
    size: wgpu::Extent3d,
    format: wgpu::TextureFormat,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// Draws the reference of a [`ReferenceOverlayHandle`] at the `overlay` slot.
pub struct ReferenceOverlayPass {
    handle: ReferenceOverlayHandle,
    gpu: Option<Gpu>,
    picture: Option<Picture>,
    copy: Option<FrameCopy>,
}

impl std::fmt::Debug for ReferenceOverlayPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferenceOverlayPass")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

const UNIFORM_BYTES: u64 = 64;

impl ReferenceOverlayPass {
    /// A pass drawing `handle`.
    #[must_use]
    pub fn new(handle: ReferenceOverlayHandle) -> Self {
        Self {
            handle,
            gpu: None,
            picture: None,
            copy: None,
        }
    }

    fn gpu(&mut self, device: &TrackedDevice) -> &mut Gpu {
        self.gpu.get_or_insert_with(|| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("reference_overlay.wgsl"),
                source: wgpu::ShaderSource::Wgsl(REFERENCE_OVERLAY_WGSL.into()),
            });
            let texture_entry = |binding: u32, filterable: bool| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            };
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("reference overlay bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: wgpu::BufferSize::new(UNIFORM_BYTES),
                        },
                        count: None,
                    },
                    texture_entry(1, true),
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    texture_entry(3, false),
                ],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("reference overlay layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("reference overlay sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("reference overlay uniforms"),
                size: UNIFORM_BYTES,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let dummy = device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("reference overlay dummy"),
                    size: wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default());
            Gpu {
                layout,
                pipeline_layout,
                module,
                sampler,
                uniforms,
                dummy,
                pipelines: Vec::new(),
            }
        })
    }

    fn pipeline(gpu: &mut Gpu, device: &TrackedDevice, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        if let Some((_, p)) = gpu.pipelines.iter().find(|(f, _)| *f == format) {
            return p.clone();
        }
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("reference overlay"),
            layout: Some(&gpu.pipeline_layout),
            vertex: wgpu::VertexState {
                module: &gpu.module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &gpu.module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&**device).as_deref(),
        });
        gpu.pipelines.push((format, pipeline.clone()));
        pipeline
    }

    /// Uploads `image` as the picture when the handle's serial changed.
    fn upload(&mut self, ctx: &mut FramePassContext<'_>, image: &ReferenceImage, serial: u64) -> bool {
        if self.picture.as_ref().is_some_and(|p| p.serial == serial) {
            return false;
        }
        let (w, h) = (image.width, image.height);
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("reference overlay picture"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
        });
        let unorm = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let srgb = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(wgpu::TextureFormat::Rgba8UnormSrgb),
            ..Default::default()
        });
        let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("reference overlay staging"),
            size: u64::from(row) * u64::from(h),
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut padded = vec![0u8; row as usize * h as usize];
        for (y, src) in image.rgba.chunks_exact(w as usize * 4).enumerate() {
            padded[y * row as usize..y * row as usize + src.len()].copy_from_slice(src);
        }
        ctx.write_buffer(&staging, 0, &padded);
        ctx.encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.picture = Some(Picture {
            serial,
            _texture: texture,
            unorm,
            srgb,
            _staging: staging,
        });
        true
    }

    /// Copies the display target into the pass's own image and returns its view.
    fn copy_frame(&mut self, ctx: &mut FramePassContext<'_>) -> Result<wgpu::TextureView, String> {
        let source = ctx.color.texture();
        if !source.usage().contains(wgpu::TextureUsages::COPY_SRC)
            || source.sample_count() != 1
            || source.mip_level_count() != 1
            || source.depth_or_array_layers() != 1
        {
            return Err("the display target cannot be copied (difference and flip need COPY_SRC)".into());
        }
        let size = source.size();
        let format = source.format();
        if !self
            .copy
            .as_ref()
            .is_some_and(|c| c.size == size && c.format == format)
        {
            let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("reference overlay frame copy"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.copy = Some(FrameCopy {
                size,
                format,
                texture,
                view,
            });
        }
        let copy = self.copy.as_ref().ok_or("frame copy missing")?;
        ctx.encoder
            .copy_texture_to_texture(source.as_image_copy(), copy.texture.as_image_copy(), size);
        Ok(copy.view.clone())
    }

    fn record_inner(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let (settings, image, serial) = {
            let s = self.handle.lock();
            (s.settings, s.image.clone(), s.serial)
        };
        let Some(image) = image.filter(|_| settings.mode != ReferenceOverlayMode::Off) else {
            if self.picture.is_some() {
                self.picture = None;
            }
            return Ok(());
        };
        let _span = tracing::trace_span!("reference_overlay").entered();
        let device = ctx.device;
        let uploaded = self.upload(ctx, &image, serial);
        let frame_view = if settings.mode.reads_frame() {
            Some(self.copy_frame(ctx)?)
        } else {
            None
        };
        let format = ctx.color_format;
        let encoded = !format.is_srgb();
        let view = ctx.viewport;
        let rect = fit_rect(image.width, image.height, view[2], view[3]);
        let exact = rect[2] == image.width as f32
            && rect[3] == image.height as f32
            && rect[0] == 0.0
            && rect[1] == 0.0;
        let mut bytes = [0u8; UNIFORM_BYTES as usize];
        let floats = [
            view[0],
            view[1],
            view[2],
            view[3],
            rect[0],
            rect[1],
            rect[2],
            rect[3],
            settings.mode.shader_code(),
            settings.opacity.clamp(0.0, 1.0),
            settings.split.clamp(0.0, 1.0),
            f32::from(u8::from(encoded)),
            f32::from(u8::from(exact)),
            image.width as f32,
            image.height as f32,
            0.0,
        ];
        for (i, v) in floats.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let pipeline = {
            let gpu = self.gpu(device);
            ctx.write_buffer(&gpu.uniforms, 0, &bytes);
            Self::pipeline(gpu, device, format)
        };
        let gpu = self.gpu.as_ref().ok_or("reference overlay pipeline missing")?;
        let picture = self.picture.as_ref().ok_or("reference overlay picture missing")?;
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("reference overlay bg"),
            layout: &gpu.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: gpu.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(if format.is_srgb() {
                        &picture.srgb
                    } else {
                        &picture.unorm
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(frame_view.as_ref().unwrap_or(&gpu.dummy)),
                },
            ],
        });
        {
            let mut pass = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("reference overlay"),
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
        let mut s = self.handle.lock();
        s.stats.draws += 1;
        s.stats.uploads += u64::from(uploaded);
        s.stats.frame_copies += u64::from(frame_view.is_some());
        Ok(())
    }
}

impl FramePass for ReferenceOverlayPass {
    fn name(&self) -> &str {
        REFERENCE_OVERLAY_PASS_NAME
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::Overlay
    }

    fn reads(&self) -> &[&'static str] {
        &["output"]
    }

    fn writes(&self) -> &[&'static str] {
        &["output"]
    }

    fn pipelines(&self) -> &[&'static str] {
        &["reference_overlay"]
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
    /// Registers the reference overlay pass ([`ReferenceOverlayPass`], ADR-2463) drawing `handle`
    /// when it is not registered yet; returns `true` when it registered it now. Off by default.
    pub fn enable_reference_overlay(&mut self, handle: &ReferenceOverlayHandle) -> bool {
        if self
            .registered_passes()
            .iter()
            .any(|name| name == REFERENCE_OVERLAY_PASS_NAME)
        {
            return false;
        }
        self.register_pass(Box::new(ReferenceOverlayPass::new(handle.clone())))
            .is_ok()
    }

    /// Removes the reference overlay pass; returns whether it was registered.
    pub fn disable_reference_overlay(&mut self) -> bool {
        self.remove_pass(REFERENCE_OVERLAY_PASS_NAME).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_and_unknown_is_none() {
        for mode in ReferenceOverlayMode::ALL {
            assert_eq!(ReferenceOverlayMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ReferenceOverlayMode::parse("Blend"), None);
        assert_eq!(ReferenceOverlayMode::parse("split"), None);
    }

    #[test]
    fn fit_covers_matching_aspects_and_letterboxes_others() {
        // Same size and a scaled copy cover the view.
        assert_eq!(fit_rect(1920, 1080, 1920.0, 1080.0), [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(fit_rect(960, 540, 1920.0, 1080.0), [0.0, 0.0, 1920.0, 1080.0]);
        // 4:3 reference in a 16:9 view: pillarboxed, centred.
        assert_eq!(fit_rect(400, 300, 160.0, 90.0), [20.0, 0.0, 120.0, 90.0]);
        // 21:9 reference in a 16:9 view: letterboxed.
        let r = fit_rect(210, 90, 160.0, 90.0);
        assert_eq!((r[0], r[2]), (0.0, 160.0));
        assert!(r[1] > 0.0 && r[3] < 90.0);
        // Degenerate input falls back to the whole view.
        assert_eq!(fit_rect(0, 5, 10.0, 10.0), [0.0, 0.0, 10.0, 10.0]);
    }

    #[test]
    fn reference_images_check_their_buffers() {
        assert!(ReferenceImage::new(2, 2, vec![0; 16]).is_ok());
        assert!(ReferenceImage::new(2, 2, vec![0; 15]).is_err());
        assert!(ReferenceImage::new(0, 2, vec![]).is_err());
        assert!(ReferenceImage::new(MAX_REFERENCE_SIDE + 1, 1, vec![]).is_err());
    }

    #[test]
    fn the_handle_counts_images_by_serial() {
        let h = ReferenceOverlayHandle::default();
        assert!(h.image().is_none());
        let img = Arc::new(ReferenceImage::new(1, 1, vec![1, 2, 3, 4]).expect("image"));
        h.set_image(Some(img.clone()));
        assert_eq!(h.image().as_deref(), Some(&*img));
        h.set_settings(ReferenceOverlaySettings {
            mode: ReferenceOverlayMode::Wipe,
            opacity: 0.25,
            split: 0.75,
        });
        assert_eq!(h.settings().mode, ReferenceOverlayMode::Wipe);
    }
}
