// SPDX-License-Identifier: MPL-2.0
//! Fullscreen video (specs/50 § 2.7, ADR-2044): [`VideoFullscreenPass`] draws pictures over the whole
//! view after the post stack (the `after_post` slot, display-referred), fitted by
//! [`FullscreenFit`] and ordered by layer. The media crate fills a shared [`VideoFullscreenQueue`]
//! each frame with the pictures of its fullscreen players; the pass uploads a picture only when its
//! serial changed (through a staging buffer and a copy in the frame's encoder) and draws nothing
//! when the queue is empty. Off by default: it exists only after
//! [`FrameRenderer::enable_video_fullscreen`].
//!
//! Every fullscreen video draws under the UI (the UI renders after the frame); `layer` orders the
//! videos among themselves. A letterboxed picture paints its bars black.

use crate::frame_work::counted::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::frame::FrameRenderer;
use crate::{FramePass, FramePassContext, GraphSlot, TrackedDevice};

/// Name of the pass in `render.graph`.
pub const VIDEO_FULLSCREEN_PASS_NAME: &str = "video_fullscreen";

/// WGSL source of the fullscreen video shader.
pub const VIDEO_FULLSCREEN_WGSL: &str = include_str!("../shaders/video_fullscreen.wgsl");

/// Most fullscreen videos drawn in one frame.
pub const MAX_FULLSCREEN_VIDEOS: usize = 8;

/// Uniform stride (the dynamic-offset alignment every adapter accepts).
const STRIDE: u64 = 256;

/// How a picture fills the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FullscreenFit {
    /// Whole picture visible, black bars on two sides.
    #[default]
    Letterbox,
    /// View covered, picture cropped.
    Fill,
    /// Picture stretched to the view.
    Stretch,
}

/// The rectangle `[x, y, w, h]` a picture of display aspect `aspect` occupies in a `vw` x `vh` view,
/// centered (it may extend past the view for [`FullscreenFit::Fill`]).
#[must_use]
pub fn fullscreen_rect(aspect: f32, vw: f32, vh: f32, fit: FullscreenFit) -> [f32; 4] {
    if !(aspect.is_finite() && aspect > 0.0 && vw > 0.0 && vh > 0.0) || fit == FullscreenFit::Stretch {
        return [0.0, 0.0, vw.max(0.0), vh.max(0.0)];
    }
    let wider = aspect > vw / vh;
    let (w, h) = match (fit, wider) {
        (FullscreenFit::Letterbox, true) | (FullscreenFit::Fill, false) => (vw, vw / aspect),
        _ => (vh * aspect, vh),
    };
    [(vw - w) * 0.5, (vh - h) * 0.5, w, h]
}

/// The visible part of `rect` inside the view `[vx, vy, vw, vh]` and the matching picture UV range
/// `[u0, v0, u1, v1]`; `None` when nothing is visible.
#[must_use]
pub fn visible_part(rect: [f32; 4], view: [f32; 4]) -> Option<([f32; 4], [f32; 4])> {
    let [rx, ry, rw, rh] = [rect[0] + view[0], rect[1] + view[1], rect[2], rect[3]];
    if rw <= 0.0 || rh <= 0.0 {
        return None;
    }
    let x0 = rx.max(view[0]);
    let y0 = ry.max(view[1]);
    let x1 = (rx + rw).min(view[0] + view[2]);
    let y1 = (ry + rh).min(view[1] + view[3]);
    if x1 - x0 < 0.5 || y1 - y0 < 0.5 {
        return None;
    }
    let uv = [(x0 - rx) / rw, (y0 - ry) / rh, (x1 - rx) / rw, (y1 - ry) / rh];
    Some(([x0, y0, x1 - x0, y1 - y0], uv))
}

/// One picture to draw fullscreen.
#[derive(Debug, Clone, PartialEq)]
pub struct FullscreenVideo {
    /// The player (entity bits); one texture is kept per player.
    pub player: u64,
    /// Order among fullscreen videos (lower draws first).
    pub layer: i32,
    /// Fit mode.
    pub fit: FullscreenFit,
    /// Display aspect (width / height).
    pub aspect: f32,
    /// Contents serial: the pass uploads when it changes.
    pub serial: u64,
    /// Width of `rgba`.
    pub width: u32,
    /// Height of `rgba`.
    pub height: u32,
    /// Tightly packed sRGB-encoded RGBA8.
    pub rgba: Arc<[u8]>,
}

/// Counters (`media.state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FullscreenStats {
    /// Pictures drawn so far.
    pub drawn: u64,
    /// Picture uploads so far.
    pub uploads: u64,
    /// Videos waiting in the queue for the next frame.
    pub queued: u32,
    /// Videos dropped because more than [`MAX_FULLSCREEN_VIDEOS`] were queued.
    pub over_limit: u64,
}

#[derive(Default)]
struct QueueInner {
    videos: Vec<FullscreenVideo>,
    stats: FullscreenStats,
}

/// The pictures the pass draws each frame (clones share it).
#[derive(Clone, Default)]
pub struct VideoFullscreenQueue {
    inner: Arc<Mutex<QueueInner>>,
}

impl std::fmt::Debug for VideoFullscreenQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFullscreenQueue")
            .field("stats", &self.stats())
            .finish()
    }
}

impl VideoFullscreenQueue {
    fn lock(&self) -> MutexGuard<'_, QueueInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the pictures drawn from the next frame on (sorted by layer, then player).
    pub fn set(&self, mut videos: Vec<FullscreenVideo>) {
        videos.sort_by_key(|v| (v.layer, v.player));
        let mut inner = self.lock();
        if videos.len() > MAX_FULLSCREEN_VIDEOS {
            inner.stats.over_limit += (videos.len() - MAX_FULLSCREEN_VIDEOS) as u64;
            videos.truncate(MAX_FULLSCREEN_VIDEOS);
        }
        inner.videos = videos;
    }

    /// The queued pictures.
    #[must_use]
    pub fn videos(&self) -> Vec<FullscreenVideo> {
        self.lock().videos.clone()
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> FullscreenStats {
        let inner = self.lock();
        let mut s = inner.stats;
        s.queued = inner.videos.len() as u32;
        s
    }
}

struct Picture {
    serial: u64,
    size: (u32, u32),
    texture: wgpu::Texture,
    unorm: wgpu::TextureView,
    srgb: wgpu::TextureView,
    staging: wgpu::Buffer,
    /// The draw group over the unorm view (`[0]`) and the sRGB view (`[1]`), built on the first
    /// draw onto such a target and kept with the picture: the uniform buffer and sampler never
    /// change, so a playing video creates no group per frame (ADR-2585).
    groups: [Option<wgpu::BindGroup>; 2],
}

struct Gpu {
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
}

/// Draws [`VideoFullscreenQueue`] pictures at the `after_post` slot.
pub struct VideoFullscreenPass {
    queue: VideoFullscreenQueue,
    gpu: Option<Gpu>,
    pictures: BTreeMap<u64, Picture>,
}

impl std::fmt::Debug for VideoFullscreenPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFullscreenPass")
            .field("pictures", &self.pictures.len())
            .finish_non_exhaustive()
    }
}

impl VideoFullscreenPass {
    /// A pass drawing `queue`.
    #[must_use]
    pub fn new(queue: VideoFullscreenQueue) -> Self {
        Self {
            queue,
            gpu: None,
            pictures: BTreeMap::new(),
        }
    }

    fn gpu(&mut self, device: &TrackedDevice) -> &mut Gpu {
        self.gpu.get_or_insert_with(|| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("video_fullscreen.wgsl"),
                source: wgpu::ShaderSource::Wgsl(VIDEO_FULLSCREEN_WGSL.into()),
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("video fullscreen bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: wgpu::BufferSize::new(32),
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
                ],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("video fullscreen layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("video fullscreen sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("video fullscreen draws"),
                size: STRIDE * (2 * MAX_FULLSCREEN_VIDEOS as u64),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            Gpu {
                layout,
                pipeline_layout,
                module,
                sampler,
                uniforms,
                pipelines: HashMap::new(),
            }
        })
    }

    fn pipeline(gpu: &mut Gpu, device: &TrackedDevice, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        if let Some(p) = gpu.pipelines.get(&format) {
            return p.clone();
        }
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video fullscreen"),
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
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&**device).as_deref(),
        });
        gpu.pipelines.insert(format, pipeline.clone());
        pipeline
    }
}

fn new_picture(device: &TrackedDevice, w: u32, h: u32) -> Picture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("video fullscreen picture"),
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
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("video fullscreen staging"),
        size: u64::from(row) * u64::from(h),
        usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    Picture {
        serial: 0,
        size: (w, h),
        texture,
        unorm,
        srgb,
        staging,
        groups: [None, None],
    }
}

fn draw_bytes(uv: [f32; 4], mode: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, v) in uv.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out[16..20].copy_from_slice(&(mode as f32).to_le_bytes());
    out
}

impl FramePass for VideoFullscreenPass {
    fn name(&self) -> &str {
        VIDEO_FULLSCREEN_PASS_NAME
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::AfterPost
    }

    fn reads(&self) -> &[&'static str] {
        &["video_frames"]
    }

    fn writes(&self) -> &[&'static str] {
        &["output"]
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let videos = self.queue.videos();
        if videos.is_empty() {
            self.pictures.clear();
            return Ok(());
        }
        let _span = tracing::trace_span!("video_fullscreen").entered();
        let device = ctx.device;
        // Uploads: one staging write and one copy per changed picture. A picture whose bytes are
        // shorter than its size is skipped, not fatal: it must not abort the encoder (the other
        // videos still draw) nor stick the pass in an error while it remains queued.
        let mut short: Vec<u64> = Vec::new();
        let mut uploads = 0u64;
        for v in &videos {
            let (w, h) = (v.width.max(1), v.height.max(1));
            if v.rgba.len() < w as usize * h as usize * 4 {
                tracing::warn!(target: "regia_render", player = v.player, "fullscreen picture is short: skipped");
                short.push(v.player);
                continue;
            }
            let picture = self
                .pictures
                .entry(v.player)
                .or_insert_with(|| new_picture(device, w, h));
            if picture.size != (w, h) {
                *picture = new_picture(device, w, h);
            }
            if picture.serial == v.serial && v.serial != 0 {
                continue;
            }
            let row =
                (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let mut padded = vec![0u8; row as usize * h as usize];
            for y in 0..h as usize {
                let src = &v.rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
                padded[y * row as usize..y * row as usize + src.len()].copy_from_slice(src);
            }
            ctx.write_buffer(&picture.staging, 0, &padded);
            ctx.encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &picture.staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(h),
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &picture.texture,
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
            picture.serial = v.serial;
            uploads += 1;
        }
        let keep: Vec<u64> = videos.iter().map(|v| v.player).collect();
        self.pictures.retain(|k, _| keep.contains(k));

        // Draw list: the bars (letterbox) then the picture per video, each with its own uniform
        // slice. Short pictures draw nothing (their texture may never have been written).
        let view = ctx.viewport;
        let mut draws: Vec<(u64, [f32; 4], [f32; 4], u32)> = Vec::new();
        for v in &videos {
            if short.contains(&v.player) {
                continue;
            }
            if v.fit == FullscreenFit::Letterbox {
                draws.push((v.player, view, [0.0, 0.0, 1.0, 1.0], 1));
            }
            let rect = fullscreen_rect(v.aspect, view[2], view[3], v.fit);
            if let Some((r, uv)) = visible_part(rect, view) {
                draws.push((v.player, r, uv, 0));
            }
        }
        if draws.is_empty() && !short.is_empty() {
            return Err(format!("fullscreen picture of player {} is short", short[0]));
        }
        let format = ctx.color_format;
        let srgb_target = format.is_srgb();
        let pipeline = {
            let gpu = self.gpu(device);
            for (i, d) in draws.iter().enumerate() {
                ctx.write_buffer(&gpu.uniforms, STRIDE * i as u64, &draw_bytes(d.2, d.3));
            }
            Self::pipeline(gpu, device, format)
        };
        let gpu = self.gpu.as_ref().ok_or("video fullscreen pipeline missing")?;
        let mut groups = Vec::with_capacity(draws.len());
        for (i, d) in draws.iter().enumerate() {
            let Some(p) = self.pictures.get_mut(&d.0) else {
                continue;
            };
            let slot = usize::from(srgb_target);
            let group = match &p.groups[slot] {
                Some(group) => group.clone(),
                None => {
                    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("video fullscreen bg"),
                        layout: &gpu.layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: &gpu.uniforms,
                                    offset: 0,
                                    size: wgpu::BufferSize::new(32),
                                }),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(if srgb_target {
                                    &p.srgb
                                } else {
                                    &p.unorm
                                }),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                            },
                        ],
                    });
                    p.groups[slot] = Some(group.clone());
                    group
                }
            };
            groups.push((group, d.1, (STRIDE * i as u64) as u32));
        }
        {
            let mut pass = ctx.encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("video fullscreen"),
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
            pass.counted_set_pipeline(&pipeline);
            for (group, r, offset) in &groups {
                pass.set_viewport(r[0], r[1], r[2].max(1.0), r[3].max(1.0), 0.0, 1.0);
                pass.counted_set_bind_group(0, group, &[*offset]);
                pass.counted_draw(0..3, 0..1);
            }
        }
        let mut inner = self.queue.lock();
        inner.stats.drawn += (videos.len() - short.len()) as u64;
        inner.stats.uploads += uploads;
        Ok(())
    }

    fn prewarm(&mut self, device: &TrackedDevice) -> Result<usize, String> {
        let fresh = self.gpu.is_none();
        let _ = self.gpu(device);
        Ok(usize::from(fresh))
    }
}

impl FrameRenderer {
    /// Registers the fullscreen video pass ([`VideoFullscreenPass`], ADR-2044) drawing `queue` when
    /// it is not registered yet; returns `true` when it registered it now. Off by default.
    pub fn enable_video_fullscreen(&mut self, queue: &VideoFullscreenQueue) -> bool {
        if self
            .registered_passes()
            .iter()
            .any(|name| name == VIDEO_FULLSCREEN_PASS_NAME)
        {
            return false;
        }
        self.register_pass(Box::new(VideoFullscreenPass::new(queue.clone())))
            .is_ok()
    }

    /// Removes the fullscreen video pass; returns whether it was registered.
    pub fn disable_video_fullscreen(&mut self) -> bool {
        self.remove_pass(VIDEO_FULLSCREEN_PASS_NAME).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    /// specs/50 § 8 test 12: 16:9 into 4:3 and 21:9 views.
    #[test]
    fn fit_rectangles() {
        let a = 16.0 / 9.0;
        assert!(close(
            fullscreen_rect(a, 1024.0, 768.0, FullscreenFit::Letterbox),
            [0.0, 96.0, 1024.0, 576.0]
        ));
        assert!(close(
            fullscreen_rect(a, 1024.0, 768.0, FullscreenFit::Fill),
            [-170.6667, 0.0, 1365.3334, 768.0]
        ));
        assert!(close(
            fullscreen_rect(a, 1024.0, 768.0, FullscreenFit::Stretch),
            [0.0, 0.0, 1024.0, 768.0]
        ));
        assert!(close(
            fullscreen_rect(a, 2520.0, 1080.0, FullscreenFit::Letterbox),
            [300.0, 0.0, 1920.0, 1080.0]
        ));
        assert!(close(
            fullscreen_rect(a, 2520.0, 1080.0, FullscreenFit::Fill),
            [0.0, -168.75, 2520.0, 1417.5]
        ));
        assert!(close(
            fullscreen_rect(0.0, 10.0, 10.0, FullscreenFit::Letterbox),
            [0.0, 0.0, 10.0, 10.0]
        ));
    }

    #[test]
    fn the_visible_part_crops_the_uv_range() {
        // Fill: 2:1 picture into a 100x100 view at offset (10, 20): 200 wide, half shown.
        let rect = fullscreen_rect(2.0, 100.0, 100.0, FullscreenFit::Fill);
        let (r, uv) = visible_part(rect, [10.0, 20.0, 100.0, 100.0]).expect("visible");
        assert!(close(r, [10.0, 20.0, 100.0, 100.0]));
        assert!(close(uv, [0.25, 0.0, 0.75, 1.0]));
        // Letterbox: the picture sits inside, the UV range is whole.
        let rect = fullscreen_rect(2.0, 100.0, 100.0, FullscreenFit::Letterbox);
        let (r, uv) = visible_part(rect, [0.0, 0.0, 100.0, 100.0]).expect("visible");
        assert!(close(r, [0.0, 25.0, 100.0, 50.0]));
        assert!(close(uv, [0.0, 0.0, 1.0, 1.0]));
        assert!(visible_part([0.0, 0.0, 0.0, 10.0], [0.0, 0.0, 10.0, 10.0]).is_none());
    }

    #[test]
    fn the_queue_sorts_by_layer_and_caps_its_length() {
        let q = VideoFullscreenQueue::default();
        let v = |player: u64, layer: i32| FullscreenVideo {
            player,
            layer,
            fit: FullscreenFit::Stretch,
            aspect: 1.0,
            serial: 1,
            width: 1,
            height: 1,
            rgba: Arc::from(vec![0u8; 4]),
        };
        q.set(vec![v(3, 1), v(1, -1), v(2, 1)]);
        let order: Vec<u64> = q.videos().iter().map(|v| v.player).collect();
        assert_eq!(order, vec![1, 2, 3]);
        q.set((0..10).map(|i| v(i, 0)).collect());
        assert_eq!(q.stats().queued, MAX_FULLSCREEN_VIDEOS as u32);
        assert_eq!(q.stats().over_limit, 2);
    }
}
