// SPDX-License-Identifier: MPL-2.0
//! Backdrop blur (ADR-1836): the GPU passes behind [`crate::TextureRef::Backdrop`] quads.
//!
//! For one quad the renderer copies the frame region under it into a scratch texture, runs a
//! separable Gaussian (horizontal pass that also downsamples, vertical pass) and hands the result
//! back as a bind group the overlay shader composites through the quad's rounded rect. The maths
//! of region, downsample factor and scissors lives in [`BlurPlan`] so it is testable without a
//! device, and [`cpu_backdrop_blur`] is the same blur on the CPU: the oracle the GPU passes are
//! compared with (`tests/backdrop_oracle.rs`).

use regia_render::RenderDevice;

use crate::draw::MAX_BACKDROP_SIGMA;

/// Bytes between two blur uniform slots (dynamic offsets must be 256-aligned on every backend).
const SLOT_BYTES: u64 = 256;
/// Blur passes per frame the uniform ring holds (two or three per quad): later quads draw tint
/// only.
pub(crate) const MAX_PASSES_PER_FRAME: u32 = 192;
/// Bytes of one blur uniform block (`BlurUniforms` in the shader).
const UNIFORM_BYTES: usize = 16;

/// The numbers for blurring one rect: all in physical px of the target unless noted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BlurPlan {
    /// Downsample factor of the scratch textures (1, 2 or 4).
    pub factor: u32,
    /// Taps on each side of the centre of both passes.
    pub taps: u32,
    /// Gaussian σ in tap units (σ px divided by `factor`).
    pub sigma_taps: f32,
    /// Region of the frame copied: x, y, w, h.
    pub copy: [u32; 4],
    /// Size of the downsampled textures.
    pub small: [u32; 2],
    /// Scissor of the horizontal pass in downsampled px: x, y, w, h.
    pub h_scissor: [u32; 4],
    /// Scissor of the vertical pass in downsampled px: x, y, w, h.
    pub v_scissor: [u32; 4],
    /// Whether a box prefilter runs first ([`PREFILTER_FACTOR`]).
    pub prefilter: bool,
    /// Scissor of the prefilter pass in downsampled px: what the horizontal taps read.
    pub d_scissor: [u32; 4],
}

/// The downsample factor that needs a box prefilter. At factor 2 a bilinear tap at a downsampled
/// texel centre already averages the 2×2 block under it; at factor 4 it would read only two of
/// every four rows and columns, and a thin line behind the panel would flicker as it moves.
const PREFILTER_FACTOR: u32 = 4;

/// Fragment entry points of the blur passes: the separable blur and the factor-4 prefilter. The
/// boot warm builds both for each format (ADR-2585), so the first blurred panel compiles nothing.
const BLUR_ENTRIES: [&str; 2] = ["fs_blur", "fs_blur_down"];

/// What the backdrop blur did in the renderer's last window draw, and the render targets it holds
/// now (`ui.backdrop_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackdropStats {
    /// Backdrop quads in the draw.
    pub quads: u32,
    /// Quads that were blurred.
    pub blurred: u32,
    /// Quads that got no blur: no readable target, nothing on the target, or the pass limit.
    pub skipped: u32,
    /// Render passes the blurs took.
    pub passes: u32,
    /// Pixels the blur passes shade, the part of the cost that follows the panel size and sigma.
    pub shaded_pixels: u64,
    /// Texture samples those passes take, the shading cost in one number.
    pub samples: u64,
    /// Pixels copied out of the frame.
    pub copied_pixels: u64,
    /// Scratch render targets the blur holds right now: the frame copy plus two per downsample
    /// factor in use (ADR-5420). Read when the stats are read, not at the last draw, so it stays
    /// at its pooled level during short pauses. A resize or format change drops the old targets;
    /// 120 idle window draws or [`crate::UiRenderer::trim_backdrop`] release the whole pool.
    pub live_targets: u32,
    /// Bytes of the [`Self::live_targets`].
    pub live_target_bytes: u64,
    /// Scratch targets the blur created since the renderer was made.
    pub targets_created: u64,
    /// Scratch targets the blur dropped since the renderer was made. `live_targets` is always
    /// `targets_created - targets_freed`.
    pub targets_freed: u64,
}

/// The ideal result of a backdrop quad over `pixels` (`width`×`height`, straight floats in the
/// target's own space): the Gaussian of sigma `sigma` px, renormalized per pixel over its
/// `ceil(3 sigma)` taps, with edge clamping, written inside `rect` (x, y, w, h; no rounded corners).
/// Sigma is clamped as the renderer clamps it; zero or no overlap changes nothing.
///
/// This is the oracle of the GPU passes, and it does the editor's software viewport path. It reads
/// only the rect and the reach of the blur.
pub fn cpu_backdrop_blur(pixels: &mut [[f32; 4]], width: u32, height: u32, rect: [f32; 4], sigma: f32) {
    let (w, h) = (width as usize, height as usize);
    if pixels.len() != w * h || sigma <= 0.0 || !sigma.is_finite() || !rect.iter().all(|v| v.is_finite()) {
        return;
    }
    let sigma = sigma.min(MAX_BACKDROP_SIGMA);
    let x0 = rect[0].floor().clamp(0.0, width as f32) as usize;
    let y0 = rect[1].floor().clamp(0.0, height as f32) as usize;
    let x1 = (rect[0] + rect[2]).ceil().clamp(0.0, width as f32) as usize;
    let y1 = (rect[1] + rect[3]).ceil().clamp(0.0, height as f32) as usize;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let taps = (3.0 * sigma).ceil() as isize;
    let inv = 1.0 / (2.0 * sigma * sigma);
    let weights: Vec<f32> = (-taps..=taps).map(|i| (-((i * i) as f32) * inv).exp()).collect();
    let total: f32 = weights.iter().sum();
    let clamp = |v: isize, n: usize| v.clamp(0, n as isize - 1) as usize;
    // Horizontal into the rows the vertical pass reads, then vertical into the rect.
    let ry0 = y0 as isize - taps;
    let rows = (y1 as isize + taps - ry0) as usize;
    let cols = x1 - x0;
    let mut horizontal = vec![[0.0f32; 4]; rows * cols];
    for row in 0..rows {
        let y = clamp(ry0 + row as isize, h);
        for col in 0..cols {
            let x = (x0 + col) as isize;
            let mut acc = [0.0f32; 4];
            for (i, weight) in (-taps..=taps).zip(&weights) {
                let p = pixels[y * w + clamp(x + i, w)];
                for (sum, value) in acc.iter_mut().zip(p) {
                    *sum += value * weight;
                }
            }
            horizontal[row * cols + col] = acc.map(|v| v / total);
        }
    }
    for y in y0..y1 {
        for col in 0..cols {
            let mut acc = [0.0f32; 4];
            for (i, weight) in (-taps..=taps).zip(&weights) {
                let row = ((y as isize + i) - ry0) as usize;
                let p = horizontal[row * cols + col];
                for (sum, value) in acc.iter_mut().zip(p) {
                    *sum += value * weight;
                }
            }
            pixels[y * w + x0 + col] = acc.map(|v| v / total);
        }
    }
}

impl BlurPlan {
    /// Render passes this blur takes.
    pub(crate) fn passes(&self) -> u32 {
        if self.prefilter { 3 } else { 2 }
    }

    /// Pixels the passes shade.
    pub(crate) fn shaded_pixels(&self) -> u64 {
        let area = |r: [u32; 4]| u64::from(r[2]) * u64::from(r[3]);
        let down = if self.prefilter { area(self.d_scissor) } else { 0 };
        down + area(self.h_scissor) + area(self.v_scissor)
    }

    /// Texture samples the passes take: one per tap per pixel, four per prefiltered pixel.
    pub(crate) fn samples(&self) -> u64 {
        let area = |r: [u32; 4]| u64::from(r[2]) * u64::from(r[3]);
        let per_pixel = u64::from(2 * self.taps + 1);
        let down = if self.prefilter {
            4 * area(self.d_scissor)
        } else {
            0
        };
        down + per_pixel * (area(self.h_scissor) + area(self.v_scissor))
    }

    /// Pixels copied out of the frame.
    pub(crate) fn copied_pixels(&self) -> u64 {
        u64::from(self.copy[2]) * u64::from(self.copy[3])
    }

    /// Plans the blur of `rect` (x, y, w, h) with Gaussian `sigma` px on a `width`×`height`
    /// target. `None` when nothing of the rect is on the target or σ is not positive.
    pub(crate) fn new(rect: [f32; 4], sigma: f32, width: u32, height: u32) -> Option<Self> {
        if sigma <= 0.0
            || !sigma.is_finite()
            || width == 0
            || height == 0
            || !rect.iter().all(|v| v.is_finite())
        {
            return None;
        }
        let sigma = sigma.min(MAX_BACKDROP_SIGMA);
        let x0 = rect[0].floor().clamp(0.0, width as f32) as u32;
        let y0 = rect[1].floor().clamp(0.0, height as f32) as u32;
        let x1 = (rect[0] + rect[2]).ceil().clamp(0.0, width as f32) as u32;
        let y1 = (rect[1] + rect[3]).ceil().clamp(0.0, height as f32) as u32;
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let factor = if sigma <= 4.0 {
            1
        } else if sigma <= 16.0 {
            2
        } else {
            4
        };
        let sigma_taps = sigma / factor as f32;
        let taps = ((3.0 * sigma_taps).ceil() as u32).max(1);
        let small = [width.div_ceil(factor), height.div_ceil(factor)];
        let sx0 = (x0 / factor).saturating_sub(1);
        let sx1 = (x1.div_ceil(factor) + 1).min(small[0]);
        let vy0 = (y0 / factor).saturating_sub(1);
        let vy1 = (y1.div_ceil(factor) + 1).min(small[1]);
        let hy0 = vy0.saturating_sub(taps);
        let hy1 = (vy1 + taps).min(small[1]);
        // What the horizontal taps read, in downsampled px, and that in full px plus a bilinear
        // texel: the part of the frame that must be fresh in the copy.
        let dx0 = sx0.saturating_sub(taps);
        let dx1 = (sx1 + taps).min(small[0]);
        let cx0 = (dx0 * factor).saturating_sub(1);
        let cy0 = (hy0 * factor).saturating_sub(1);
        let cx1 = (dx1 * factor + 1).min(width);
        let cy1 = (hy1 * factor + 1).min(height);
        Some(Self {
            factor,
            taps,
            sigma_taps,
            copy: [cx0, cy0, cx1 - cx0, cy1 - cy0],
            small,
            h_scissor: [sx0, hy0, sx1 - sx0, hy1 - hy0],
            v_scissor: [sx0, vy0, sx1 - sx0, vy1 - vy0],
            prefilter: factor == PREFILTER_FACTOR,
            d_scissor: [dx0, hy0, dx1 - dx0, hy1 - hy0],
        })
    }
}

/// The full-size copy of the frame, shared by every downsample factor.
struct ScratchSource {
    key: (u32, u32, wgpu::TextureFormat),
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    /// Bytes of `texture`.
    bytes: u64,
}

/// The two downsampled textures of one (target size, format, factor).
struct ScratchSmall {
    key: (u32, u32, wgpu::TextureFormat, u32),
    a_view: wgpu::TextureView,
    a_bg: wgpu::BindGroup,
    b_view: wgpu::TextureView,
    b_bg: wgpu::BindGroup,
    _a: wgpu::Texture,
    _b: wgpu::Texture,
    /// Bytes of both textures.
    bytes: u64,
}

/// Bytes of a `width`x`height` texture of `format` (one mip, one layer).
fn target_bytes(format: wgpu::TextureFormat, width: u32, height: u32) -> u64 {
    let texel = u64::from(format.block_copy_size(None).unwrap_or(4));
    u64::from(width.max(1)) * u64::from(height.max(1)) * texel
}

/// GPU state of the backdrop blur: pipelines per format, the uniform ring, the scratch textures.
pub(crate) struct BackdropGpu {
    module: wgpu::ShaderModule,
    uniform_bg: wgpu::BindGroup,
    uniform_buf: wgpu::Buffer,
    layout: wgpu::PipelineLayout,
    image_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pipelines: std::collections::HashMap<(wgpu::TextureFormat, &'static str), wgpu::RenderPipeline>,
    source: Option<ScratchSource>,
    /// One set per factor, so panels of different sigmas in one frame do not reallocate each other.
    small: [Option<ScratchSmall>; 3],
    /// Ring slots used since [`Self::begin_frame`].
    used: u32,
    /// Consecutive window draws with no successful blur (ADR-7869).
    idle_frames: u32,
    /// Includes canvas blurs recorded before the next window draw finishes.
    drawn_since_frame: bool,
    /// Scratch textures created since this state was made (ADR-5420).
    created: u64,
    /// Scratch textures dropped since this state was made.
    freed: u64,
}

impl BackdropGpu {
    /// Builds the layouts and the uniform ring. `module` is the overlay shader module.
    pub(crate) fn new(
        device: &RenderDevice,
        module: &wgpu::ShaderModule,
        image_bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
    ) -> Self {
        let dev = &regia_render::TrackedDevice::observe(&device.device);
        let uniform_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui blur bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(UNIFORM_BYTES as u64),
                },
                count: None,
            }],
        });
        let empty = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui blur empty bgl"),
            entries: &[],
        });
        let layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui blur pl"),
            bind_group_layouts: &[Some(&uniform_bgl), Some(&empty), Some(&empty), Some(image_bgl)],
            immediate_size: 0,
        });
        let uniform_buf = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui blur uniforms"),
            size: SLOT_BYTES * u64::from(MAX_PASSES_PER_FRAME),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui blur uniform bg"),
            layout: &uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &uniform_buf,
                    offset: 0,
                    size: wgpu::BufferSize::new(UNIFORM_BYTES as u64),
                }),
            }],
        });
        Self {
            module: module.clone(),
            uniform_bg,
            uniform_buf,
            layout,
            image_bgl: image_bgl.clone(),
            sampler: sampler.clone(),
            pipelines: Default::default(),
            source: None,
            small: [None, None, None],
            used: 0,
            idle_frames: 0,
            drawn_since_frame: false,
            created: 0,
            freed: 0,
        }
    }

    /// Writes the pool's live render-target count, its bytes and the lifetime counters into
    /// `stats` (ADR-5420).
    pub(crate) fn fill_target_stats(&self, stats: &mut BackdropStats) {
        let small = self.small.iter().flatten();
        stats.live_targets = u32::from(self.source.is_some()) + 2 * small.clone().count() as u32;
        stats.live_target_bytes =
            self.source.as_ref().map_or(0, |s| s.bytes) + small.map(|s| s.bytes).sum::<u64>();
        stats.targets_created = self.created;
        stats.targets_freed = self.freed;
    }

    /// Resets the per-frame uniform ring.
    pub(crate) fn begin_frame(&mut self) {
        self.used = 0;
    }

    /// Completes one window draw, including empty lists and skipped blurs.
    pub(crate) fn end_frame(&mut self) {
        if std::mem::take(&mut self.drawn_since_frame) {
            self.idle_frames = 0;
        } else if self.source.is_some() {
            self.idle_frames = self.idle_frames.saturating_add(1);
            if self.idle_frames >= crate::UiRenderer::BACKDROP_IDLE_FRAMES {
                self.trim();
            }
        }
    }

    /// Releases every scratch target and its bind groups, keeping pipelines and uniforms warm.
    pub(crate) fn trim(&mut self) {
        let _span = tracing::trace_span!("ui_backdrop_trim").entered();
        if self.source.take().is_some() {
            self.freed += 1;
        }
        for small in &mut self.small {
            if small.take().is_some() {
                self.freed += 2;
            }
        }
        self.idle_frames = 0;
        self.drawn_since_frame = false;
    }

    fn pipeline(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
        entry: &'static str,
    ) -> wgpu::RenderPipeline {
        if let Some(p) = self.pipelines.get(&(format, entry)) {
            return p.clone();
        }
        let pipeline = Self::build_pipeline(device, &self.module, &self.layout, format, entry);
        self.pipelines.insert((format, entry), pipeline.clone());
        pipeline
    }

    /// What a warm needs to compile the blur pipelines of `format` that do not exist yet: the
    /// shader module, the layout and the missing fragment entry points (`None` when all exist).
    pub(crate) fn warm_inputs(
        &self,
        format: wgpu::TextureFormat,
    ) -> Option<(wgpu::ShaderModule, wgpu::PipelineLayout, Vec<&'static str>)> {
        let missing: Vec<&'static str> = BLUR_ENTRIES
            .into_iter()
            .filter(|entry| !self.pipelines.contains_key(&(format, *entry)))
            .collect();
        (!missing.is_empty()).then(|| (self.module.clone(), self.layout.clone(), missing))
    }

    /// Stores a pipeline compiled by [`Self::build_pipeline`] unless a draw already made one.
    pub(crate) fn install_pipeline(
        &mut self,
        format: wgpu::TextureFormat,
        entry: &'static str,
        pipeline: wgpu::RenderPipeline,
    ) {
        self.pipelines.entry((format, entry)).or_insert(pipeline);
    }

    /// Builds every blur pipeline of `format` now (the boot warm, ADR-2585); returns how many
    /// this call created.
    pub(crate) fn warm(&mut self, device: &RenderDevice, format: wgpu::TextureFormat) -> usize {
        let Some((_, _, missing)) = self.warm_inputs(format) else {
            return 0;
        };
        let created = missing.len();
        for entry in missing {
            self.pipeline(device, format, entry);
        }
        created
    }

    /// Compiles the blur pipeline of `format` for the fragment `entry`. Touches no state, so a
    /// warm can run it without holding the UI renderer's lock.
    pub(crate) fn build_pipeline(
        device: &RenderDevice,
        module: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
        format: wgpu::TextureFormat,
        entry: &'static str,
    ) -> wgpu::RenderPipeline {
        regia_render::TrackedDevice::observe(&device.device).create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("ui backdrop blur pipeline"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_blur"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(&device.device).as_deref(),
            },
        )
    }

    fn image_bind_group(
        &self,
        device: &RenderDevice,
        label: &str,
        view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        regia_render::TrackedDevice::observe(&device.device).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.image_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),
                },
            ],
        })
    }

    /// Makes sure the frame copy and the downsampled textures of `factor` exist at this size.
    fn ensure_scratch(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        factor: u32,
    ) {
        // Counted on the frame-path counters, so a warm-frame test sees every scratch texture.
        let dev = regia_render::TrackedDevice::observe(&device.device);
        let make = |label: &str, w: u32, h: u32, usage: wgpu::TextureUsages| {
            dev.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        if self
            .source
            .as_ref()
            .is_none_or(|s| s.key != (width, height, format))
        {
            // A new size or format: drop every downsampled pair of the old one too, since no draw
            // can reuse it until the frame returns to that exact size (ADR-5420).
            for slot in &mut self.small {
                if slot
                    .as_ref()
                    .is_some_and(|s| (s.key.0, s.key.1, s.key.2) != (width, height, format))
                {
                    *slot = None;
                    self.freed += 2;
                }
            }
            if self.source.is_some() {
                self.freed += 1;
            }
            self.created += 1;
            let texture = make(
                "ui blur src",
                width,
                height,
                wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = self.image_bind_group(device, "ui blur src bg", &view);
            self.source = Some(ScratchSource {
                key: (width, height, format),
                texture,
                bind_group,
                bytes: target_bytes(format, width, height),
            });
        }
        let index = factor.trailing_zeros() as usize;
        let key = (width, height, format, factor);
        if self.small[index].as_ref().is_none_or(|s| s.key != key) {
            let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT;
            let (sw, sh) = (width.div_ceil(factor), height.div_ceil(factor));
            if self.small[index].is_some() {
                self.freed += 2;
            }
            self.created += 2;
            let a = make("ui blur a", sw, sh, usage);
            let b = make("ui blur b", sw, sh, usage);
            let a_view = a.create_view(&wgpu::TextureViewDescriptor::default());
            let b_view = b.create_view(&wgpu::TextureViewDescriptor::default());
            let a_bg = self.image_bind_group(device, "ui blur a bg", &a_view);
            let b_bg = self.image_bind_group(device, "ui blur b bg", &b_view);
            self.small[index] = Some(ScratchSmall {
                key,
                a_view,
                a_bg,
                b_view,
                b_bg,
                _a: a,
                _b: b,
                bytes: 2 * target_bytes(format, sw, sh),
            });
        }
    }

    /// Records the copy and the blur passes for `rect` and returns the bind group holding the
    /// blurred frame, or `None` (nothing to blur, no readable target, ring full).
    pub(crate) fn record(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::Texture,
        rect: [f32; 4],
        sigma: f32,
        stats: &mut BackdropStats,
    ) -> Option<wgpu::BindGroup> {
        let _span = tracing::trace_span!("ui_backdrop_blur", sigma).entered();
        let (width, height, format) = (target.width(), target.height(), target.format());
        if !target.usage().contains(wgpu::TextureUsages::COPY_SRC) || target.sample_count() != 1 {
            return None;
        }
        let plan = BlurPlan::new(rect, sigma, width, height)?;
        if self.used + plan.passes() > MAX_PASSES_PER_FRAME {
            return None;
        }
        let pipeline = self.pipeline(device, format, "fs_blur");
        let down = plan
            .prefilter
            .then(|| self.pipeline(device, format, "fs_blur_down"));
        let first_slot = self.used;
        self.used += plan.passes();
        let source_step = [1.0 / width as f32, 1.0 / height as f32];
        let small_step = [1.0 / plan.small[0] as f32, 1.0 / plan.small[1] as f32];
        let write = |slot: u32, step: [f32; 2]| {
            let values = [step[0], step[1], plan.sigma_taps, plan.taps as f32];
            let bytes: [u8; UNIFORM_BYTES] = bytemuck::cast(values);
            device
                .queue
                .write_buffer(&self.uniform_buf, u64::from(slot) * SLOT_BYTES, &bytes);
        };
        let slot_down = first_slot;
        let slot_h = first_slot + plan.passes() - 2;
        let slot_v = first_slot + plan.passes() - 1;
        if plan.prefilter {
            write(slot_down, source_step);
            write(slot_h, [small_step[0], 0.0]);
        } else {
            write(slot_h, [plan.factor as f32 / width as f32, 0.0]);
        }
        write(slot_v, [0.0, small_step[1]]);
        self.ensure_scratch(device, format, width, height, plan.factor);
        let source = self.source.as_ref()?;
        let small = self.small[plan.factor.trailing_zeros() as usize].as_ref()?;
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: plan.copy[0],
                    y: plan.copy[1],
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &source.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: plan.copy[0],
                    y: plan.copy[1],
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: plan.copy[2],
                height: plan.copy[3],
                depth_or_array_layers: 1,
            },
        );
        // With the prefilter the 4×4 averages land in `b`, which the vertical pass overwrites.
        let (horizontal_source, prefilter) = match &down {
            Some(down) => (
                &small.b_bg,
                Some((
                    "ui blur down",
                    down,
                    &small.b_view,
                    &source.bind_group,
                    slot_down,
                    plan.d_scissor,
                )),
            ),
            None => (&source.bind_group, None),
        };
        let passes = prefilter.into_iter().chain([
            (
                "ui blur h",
                &pipeline,
                &small.a_view,
                horizontal_source,
                slot_h,
                plan.h_scissor,
            ),
            (
                "ui blur v",
                &pipeline,
                &small.b_view,
                &small.a_bg,
                slot_v,
                plan.v_scissor,
            ),
        ]);
        for (label, pipeline, view, source, slot, scissor) in passes {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &self.uniform_bg, &[(u64::from(slot) * SLOT_BYTES) as u32]);
            pass.set_bind_group(3, source, &[]);
            pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
            pass.draw(0..3, 0..1);
        }
        stats.blurred += 1;
        stats.passes += plan.passes();
        stats.shaded_pixels += plan.shaded_pixels();
        stats.samples += plan.samples();
        stats.copied_pixels += plan.copied_pixels();
        self.drawn_since_frame = true;
        Some(small.b_bg.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_rejects_empty_and_invalid_input() {
        assert!(BlurPlan::new([10.0, 10.0, 50.0, 50.0], 0.0, 100, 100).is_none());
        assert!(BlurPlan::new([10.0, 10.0, 50.0, 50.0], f32::NAN, 100, 100).is_none());
        assert!(BlurPlan::new([10.0, 10.0, 50.0, 50.0], 4.0, 0, 100).is_none());
        // Fully off the target, or zero sized.
        assert!(BlurPlan::new([200.0, 10.0, 50.0, 50.0], 4.0, 100, 100).is_none());
        assert!(BlurPlan::new([-80.0, 10.0, 50.0, 50.0], 4.0, 100, 100).is_none());
        assert!(BlurPlan::new([10.0, 10.0, 0.0, 50.0], 4.0, 100, 100).is_none());
        assert!(BlurPlan::new([f32::INFINITY, 0.0, 5.0, 5.0], 4.0, 100, 100).is_none());
    }

    #[test]
    fn plan_picks_the_downsample_factor_from_sigma() {
        let f = |s| BlurPlan::new([0.0, 0.0, 64.0, 64.0], s, 256, 256).map(|p| p.factor);
        assert_eq!(f(2.0), Some(1));
        assert_eq!(f(4.0), Some(1));
        assert_eq!(f(8.0), Some(2));
        assert_eq!(f(16.0), Some(2));
        assert_eq!(f(30.0), Some(4));
        // σ clamps at the maximum, so the tap count is bounded.
        let big = BlurPlan::new([0.0, 0.0, 64.0, 64.0], 10_000.0, 256, 256).expect("plan");
        assert_eq!(big.factor, 4);
        assert_eq!(big.taps, 48);
        assert_eq!(big.sigma_taps, 16.0);
    }

    #[test]
    fn plan_regions_stay_inside_the_target_and_cover_the_rect() {
        for (rect, sigma) in [
            ([10.0, 20.0, 80.0, 40.0], 3.0),
            ([-30.0, -30.0, 100.0, 100.0], 12.0),
            ([200.0, 150.0, 300.0, 300.0], 40.0),
            ([0.5, 0.5, 1.0, 1.0], 1.0),
        ] {
            let (w, h) = (257u32, 193u32);
            let p = BlurPlan::new(rect, sigma, w, h).expect("plan");
            assert!(
                p.copy[0] + p.copy[2] <= w && p.copy[1] + p.copy[3] <= h,
                "{rect:?} copy {p:?}"
            );
            for s in [p.h_scissor, p.v_scissor] {
                assert!(
                    s[0] + s[2] <= p.small[0] && s[1] + s[3] <= p.small[1],
                    "{rect:?} scissor {p:?}"
                );
                assert!(s[2] > 0 && s[3] > 0);
            }
            // The copy holds the rect (clipped to the target) and the vertical pass the rect's
            // downsampled rows.
            let x0 = rect[0].max(0.0).floor() as u32;
            let y0 = rect[1].max(0.0).floor() as u32;
            let x1 = ((rect[0] + rect[2]).ceil() as u32).min(w);
            let y1 = ((rect[1] + rect[3]).ceil() as u32).min(h);
            assert!(p.copy[0] <= x0 && p.copy[1] <= y0, "{rect:?} {p:?}");
            assert!(
                p.copy[0] + p.copy[2] >= x1 && p.copy[1] + p.copy[3] >= y1,
                "{rect:?} {p:?}"
            );
            assert!(p.v_scissor[0] * p.factor <= x0 && p.v_scissor[1] * p.factor <= y0);
            assert!((p.v_scissor[0] + p.v_scissor[2]) * p.factor >= x1);
            assert!((p.v_scissor[1] + p.v_scissor[3]) * p.factor >= y1);
            // The horizontal pass covers the vertical pass's rows plus its taps.
            assert!(
                p.h_scissor[1] <= p.v_scissor[1]
                    && p.h_scissor[1] + p.h_scissor[3] >= p.v_scissor[1] + p.v_scissor[3]
            );
        }
    }

    #[test]
    fn only_the_largest_downsample_takes_a_prefilter_and_cost_follows_the_panel() {
        let plan = |rect, sigma| BlurPlan::new(rect, sigma, 1920, 1080).expect("plan");
        let small = plan([100.0, 100.0, 320.0, 180.0], 3.0);
        assert!(!small.prefilter);
        assert_eq!(small.passes(), 2);
        assert!(!plan([100.0, 100.0, 320.0, 180.0], 16.0).prefilter);
        let wide = plan([100.0, 100.0, 320.0, 180.0], 17.0);
        assert!(wide.prefilter);
        assert_eq!(wide.passes(), 3);
        // Four times the panel area shades about four times the pixels, whatever the screen.
        let big = plan([100.0, 100.0, 640.0, 360.0], 3.0);
        let ratio = big.shaded_pixels() as f64 / small.shaded_pixels() as f64;
        assert!((3.0..4.2).contains(&ratio), "{ratio}");
        let on_large = BlurPlan::new([100.0, 100.0, 320.0, 180.0], 3.0, 3840, 2160).expect("plan");
        assert_eq!(on_large.shaded_pixels(), small.shaded_pixels());
        assert_eq!(on_large.samples(), small.samples());
        // A larger sigma at the same factor costs more samples per pixel, and a higher factor
        // brings the bill back down.
        assert!(
            plan([100.0, 100.0, 640.0, 360.0], 15.0).samples()
                > plan([100.0, 100.0, 640.0, 360.0], 5.0).samples()
        );
        assert!(
            plan([100.0, 100.0, 640.0, 360.0], 17.0).samples()
                < plan([100.0, 100.0, 640.0, 360.0], 15.0).samples()
        );
    }

    #[test]
    fn the_prefilter_region_covers_the_horizontal_taps() {
        for (rect, sigma) in [
            ([100.0, 100.0, 320.0, 180.0], 20.0),
            ([0.0, 0.0, 64.0, 64.0], 64.0),
            ([1800.0, 1000.0, 120.0, 80.0], 30.0),
        ] {
            let p = BlurPlan::new(rect, sigma, 1920, 1080).expect("plan");
            let [dx, dy, dw, dh] = p.d_scissor;
            let [hx, hy, hw, hh] = p.h_scissor;
            assert!(dx == 0 || dx + p.taps <= hx, "{p:?}");
            assert!(dx + dw == p.small[0] || dx + dw >= hx + hw + p.taps, "{p:?}");
            assert_eq!((dy, dh), (hy, hh), "{p:?}");
            // The copy holds every row and column those texels average.
            assert!(p.copy[0] <= dx * p.factor && p.copy[1] <= dy * p.factor, "{p:?}");
            assert!(p.copy[0] + p.copy[2] >= ((dx + dw) * p.factor).min(1920), "{p:?}");
            assert!(p.copy[1] + p.copy[3] >= ((dy + dh) * p.factor).min(1080), "{p:?}");
        }
    }

    /// Solid colors stay solid: the weights are normalized, including at the edges.
    #[test]
    fn the_cpu_blur_keeps_a_flat_frame_and_writes_only_inside_the_rect() {
        let flat = [0.25, 0.5, 0.75, 1.0];
        let mut pixels = vec![flat; 64 * 64];
        cpu_backdrop_blur(&mut pixels, 64, 64, [0.0, 0.0, 64.0, 64.0], 5.0);
        for p in &pixels {
            for (value, expected) in p.iter().zip(flat) {
                assert!((value - expected).abs() < 1e-5, "{p:?}");
            }
        }
        let mut step: Vec<[f32; 4]> = (0..64 * 64)
            .map(|i| if i % 64 < 32 { [0.0; 4] } else { [1.0; 4] })
            .collect();
        let before = step.clone();
        cpu_backdrop_blur(&mut step, 64, 64, [16.0, 16.0, 32.0, 32.0], 3.0);
        for y in 0..64 {
            for x in 0..64 {
                let inside = (16..48).contains(&x) && (16..48).contains(&y);
                if !inside {
                    assert_eq!(step[y * 64 + x], before[y * 64 + x], "{x},{y}");
                }
            }
        }
        // The middle of the step is the mean of both sides, on each side of the edge.
        let (left, right) = (step[32 * 64 + 31][0], step[32 * 64 + 32][0]);
        assert!((left + right - 1.0).abs() < 1e-4, "{left} {right}");
        assert!(left > 0.3 && right < 0.7);
    }

    #[test]
    fn the_cpu_blur_refuses_bad_input() {
        let mut pixels = vec![[0.5f32; 4]; 16];
        let before = pixels.clone();
        cpu_backdrop_blur(&mut pixels, 4, 4, [0.0, 0.0, 4.0, 4.0], 0.0);
        cpu_backdrop_blur(&mut pixels, 4, 4, [0.0, 0.0, 4.0, 4.0], f32::NAN);
        cpu_backdrop_blur(&mut pixels, 4, 4, [f32::NAN, 0.0, 4.0, 4.0], 2.0);
        cpu_backdrop_blur(&mut pixels, 5, 4, [0.0, 0.0, 4.0, 4.0], 2.0);
        cpu_backdrop_blur(&mut pixels, 4, 4, [9.0, 9.0, 4.0, 4.0], 2.0);
        assert_eq!(pixels, before);
    }

    #[test]
    fn plan_copy_margin_covers_the_horizontal_taps() {
        let p = BlurPlan::new([100.0, 100.0, 50.0, 50.0], 8.0, 512, 512).expect("plan");
        let reach = p.taps * p.factor;
        assert!(100 - p.copy[0] >= reach, "{p:?}");
        assert!(p.copy[0] + p.copy[2] >= 150 + reach, "{p:?}");
    }
}
