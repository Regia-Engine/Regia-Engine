// SPDX-License-Identifier: MPL-2.0
//! Fullscreen video above the UI (specs/50 § 2.7, ADR-2410). The renderer's `VideoFullscreenPass`
//! draws in the frame graph after the post stack, which is under the UI (the UI is a window overlay
//! drawn after the frame). A fullscreen player with `layer >= 0` must cover the UI, so
//! [`VideoOverlayPlugin`] installs [`VideoOverlayDraw`] as a `regia_render::WindowOverlay` layer at
//! [`VIDEO_OVERLAY_ORDER`] (the UI is order 0) and the publish system routes those players to its
//! queue ([`OverlayVideos`]); `layer < 0` players stay in the frame-graph pass under the UI.
//! Several videos draw in `layer` order; a letterboxed picture paints its bars black, so nothing of
//! the UI shows through a boot movie.
//!
//! Without the plugin every fullscreen player goes to the frame-graph pass as before.
//!
//! The draw reuses the renderer's fullscreen shader and fit math (`VIDEO_FULLSCREEN_WGSL`,
//! `fullscreen_rect`, `visible_part`); a picture is uploaded only when its serial changes.

use std::collections::{BTreeMap, HashMap};

use regia_app::prelude::{App, Plugin, Startup};
use regia_ecs::prelude::*;
use regia_render::video_fullscreen::{VIDEO_FULLSCREEN_WGSL, fullscreen_rect, visible_part};
use regia_render::{
    FullscreenFit, OverlayDraw, RenderDevice, TrackedDevice, VideoFullscreenQueue, WindowOverlay,
};

/// Overlay order of the above-UI video layer (the UI is `WindowOverlay::UI_ORDER`, 0).
pub const VIDEO_OVERLAY_ORDER: i32 = 100;
/// Overlay layer name.
pub const VIDEO_OVERLAY_NAME: &str = "video_above_ui";

/// Uniform stride (the dynamic-offset alignment every adapter accepts).
const STRIDE: u64 = 256;
/// Most draws per frame (bars plus picture for each of 8 videos).
const MAX_DRAWS: u64 = 16;

/// The queue of fullscreen pictures drawn above the UI (players with `layer >= 0`).
#[derive(Resource, Debug, Clone, Default)]
pub struct OverlayVideos(pub VideoFullscreenQueue);

/// Installs the above-UI video layer. Add it after `MediaPlugin` (and `VideoFramePlugin`, which
/// publishes the pictures); the layer is installed at `Startup` when a `WindowOverlay` exists.
#[derive(Debug, Default)]
pub struct VideoOverlayPlugin;

impl Plugin for VideoOverlayPlugin {
    fn build(&self, app: &mut App) {
        let queue = OverlayVideos::default();
        app.insert_resource(queue.clone()).add_systems(
            Startup,
            move |overlay: Option<ResMut<WindowOverlay>>| {
                if let Some(mut overlay) = overlay {
                    overlay.insert(
                        VIDEO_OVERLAY_ORDER,
                        VIDEO_OVERLAY_NAME,
                        Box::new(VideoOverlayDraw::new(queue.0.clone())),
                    );
                }
            },
        );
    }
}

struct Picture {
    serial: u64,
    size: (u32, u32),
    texture: wgpu::Texture,
    unorm: wgpu::TextureView,
    srgb: wgpu::TextureView,
    /// Bar group at index 1, picture group at index 0. Recreated when the texture or the
    /// target's sRGB-ness changes.
    groups: [Option<wgpu::BindGroup>; 2],
    groups_srgb: bool,
}

struct Gpu {
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
}

/// Draws [`OverlayVideos`] over the window frame (a `WindowOverlay` layer).
pub struct VideoOverlayDraw {
    queue: VideoFullscreenQueue,
    gpu: Option<Gpu>,
    pictures: BTreeMap<u64, Picture>,
    uploads: u64,
    drawn: u64,
}

impl std::fmt::Debug for VideoOverlayDraw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoOverlayDraw")
            .field("pictures", &self.pictures.len())
            .field("uploads", &self.uploads)
            .field("drawn", &self.drawn)
            .finish_non_exhaustive()
    }
}

fn new_gpu(device: &wgpu::Device) -> Gpu {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("video_fullscreen.wgsl (overlay)"),
        source: wgpu::ShaderSource::Wgsl(VIDEO_FULLSCREEN_WGSL.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("video overlay bgl"),
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
        label: Some("video overlay layout"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("video overlay sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("video overlay draws"),
        size: STRIDE * MAX_DRAWS,
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
}

fn pipeline(gpu: &mut Gpu, device: &wgpu::Device, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
    if let Some(p) = gpu.pipelines.get(&format) {
        return p.clone();
    }
    let p = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("video overlay"),
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
        cache: None,
    });
    gpu.pipelines.insert(format, p.clone());
    p
}

fn new_picture(device: &wgpu::Device, w: u32, h: u32) -> Picture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("video overlay picture"),
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
    Picture {
        serial: 0,
        size: (w, h),
        texture,
        unorm,
        srgb,
        groups: [None, None],
        groups_srgb: false,
    }
}

fn draw_bytes(uv: [f32; 4], bar: bool) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, v) in uv.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out[16..20].copy_from_slice(&(if bar { 1.0f32 } else { 0.0 }).to_le_bytes());
    out
}

impl VideoOverlayDraw {
    /// A layer drawing `queue`.
    pub fn new(queue: VideoFullscreenQueue) -> Self {
        Self {
            queue,
            gpu: None,
            pictures: BTreeMap::new(),
            uploads: 0,
            drawn: 0,
        }
    }

    /// Picture uploads so far.
    pub fn uploads(&self) -> u64 {
        self.uploads
    }

    /// Pictures drawn so far.
    pub fn drawn(&self) -> u64 {
        self.drawn
    }
}

impl OverlayDraw for VideoOverlayDraw {
    fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let videos = self.queue.videos();
        if videos.is_empty() {
            self.pictures.clear();
            return;
        }
        let _span = tracing::trace_span!("video_overlay").entered();
        let dev = &device.device;
        for v in &videos {
            let (w, h) = (v.width.max(1), v.height.max(1));
            if v.rgba.len() < w as usize * h as usize * 4 {
                tracing::warn!(player = v.player, "fullscreen picture is short; not drawn");
                continue;
            }
            let picture = self
                .pictures
                .entry(v.player)
                .or_insert_with(|| new_picture(dev, w, h));
            if picture.size != (w, h) {
                *picture = new_picture(dev, w, h);
            }
            if picture.serial == v.serial && v.serial != 0 {
                continue;
            }
            device.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &picture.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &v.rgba[..w as usize * h as usize * 4],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            picture.serial = v.serial;
            self.uploads += 1;
        }
        let keep: Vec<u64> = videos.iter().map(|v| v.player).collect();
        self.pictures.retain(|k, _| keep.contains(k));

        let view_rect = [0.0, 0.0, width as f32, height as f32];
        let mut draws: Vec<(u64, [f32; 4], [f32; 4], bool)> = Vec::new();
        for v in &videos {
            if v.fit == FullscreenFit::Letterbox {
                draws.push((v.player, view_rect, [0.0, 0.0, 1.0, 1.0], true));
            }
            let rect = fullscreen_rect(v.aspect, view_rect[2], view_rect[3], v.fit);
            if let Some((r, uv)) = visible_part(rect, view_rect) {
                draws.push((v.player, r, uv, false));
            }
        }
        draws.truncate(MAX_DRAWS as usize);
        let (pipe, layout, sampler, uniforms) = {
            let gpu = self.gpu.get_or_insert_with(|| new_gpu(dev));
            for (i, d) in draws.iter().enumerate() {
                device
                    .queue
                    .write_buffer(&gpu.uniforms, STRIDE * i as u64, &draw_bytes(d.2, d.3));
            }
            let pipe = pipeline(gpu, dev, format);
            (
                pipe,
                gpu.layout.clone(),
                gpu.sampler.clone(),
                gpu.uniforms.clone(),
            )
        };
        let srgb_target = format.is_srgb();
        let tracked = TrackedDevice::observe(dev);
        let mut groups = Vec::with_capacity(draws.len());
        for (i, d) in draws.iter().enumerate() {
            let Some(p) = self.pictures.get_mut(&d.0) else {
                continue;
            };
            // `true` is the letterbox bar, `false` the picture. One cached group each.
            let slot = usize::from(d.3);
            if p.groups_srgb != srgb_target {
                p.groups = [None, None];
                p.groups_srgb = srgb_target;
            }
            if p.groups[slot].is_none() {
                let group = {
                    let view = if srgb_target { &p.srgb } else { &p.unorm };
                    tracked.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("video overlay bg"),
                        layout: &layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: &uniforms,
                                    offset: 0,
                                    size: wgpu::BufferSize::new(32),
                                }),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&sampler),
                            },
                        ],
                    })
                };
                p.groups[slot] = Some(group);
            }
            let Some(group) = p.groups[slot].clone() else {
                continue;
            };
            groups.push((group, d.1, (STRIDE * i as u64) as u32));
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("video overlay"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
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
        pass.set_pipeline(&pipe);
        for (group, r, offset) in &groups {
            pass.set_viewport(r[0], r[1], r[2].max(1.0), r[3].max(1.0), 0.0, 1.0);
            pass.set_bind_group(0, group, &[*offset]);
            pass.draw(0..3, 0..1);
        }
        self.drawn += videos.len() as u64;
    }

    fn warm(&mut self, device: &RenderDevice, format: wgpu::TextureFormat) {
        let gpu = self.gpu.get_or_insert_with(|| new_gpu(&device.device));
        let _ = pipeline(gpu, &device.device, format);
    }
}
