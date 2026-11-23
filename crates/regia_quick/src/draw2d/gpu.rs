// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/front/gpu.rs, commit c9513623f), ADR-8012.
//
// Adapted (P-05, docs/ported-sources.toml), each change for a named issue:
// - no intermediate UI target and composite pass: the list draws straight into the frame, so
//   `warm()` leaves nothing for the first frame to create and a resize allocates nothing;
//   blending happens in the target's space and the sRGB encode for unorm targets moved into the
//   draw shader;
// - the instance buffer starts at 256 KiB and only grows; the glyph atlas is `regia_ui`'s paged
//   atlas (texture arrays) instead of one 2048² page that started over when full;
// - glyphs come from `regia_ui`'s `TextSystem` (no font, font name or metric table enters);
// - the fixed brand-image slots became `IMAGE_SLOTS` caller-filled slots;
// - per-format pipelines, creation counters and the bytes-uploaded count for agents.

//! The GPU side of the 2D draw list: one instanced pipeline per target format, the glyph atlas
//! pages as texture arrays, the image slots and a grow-only instance buffer.

use regia_render::{ImmediateImage, ImmediateImageFormat, RenderDevice};
use regia_ui::atlas::PAGE_SIZE;

use super::tiles::{Draw2dTileStats, Draw2dTiles};
use super::{ATLAS_PAGES, Draw2dFrame, Draw2dImage, Draw2dList, IMAGE_SLOTS, SHADER};

/// Initial instance buffer size (3276 instances).
const INITIAL_INSTANCE_BYTES: u64 = 1 << 18;

/// The layer's GPU counters (for agents and the creation checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Draw2dGpuStats {
    /// Frames the layer drew.
    pub frames: u64,
    /// Render pipelines created so far (one per target format).
    pub pipelines: u32,
    /// Buffers created so far (the uniform buffer and every instance buffer).
    pub buffers: u32,
    /// Textures created so far (atlas arrays and images).
    pub textures: u32,
    /// Bytes written to the GPU by the last frame (instances, atlas pages, images, uniform).
    pub bytes_uploaded: u64,
    /// Instance buffer capacity in bytes.
    pub instance_capacity: u64,
    /// Instances in the last frame.
    pub instances: u32,
    /// Dirty-region counters of the last frame (all zero with dirty regions off).
    pub tiles: Draw2dTileStats,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniform {
    size: [f32; 2],
    /// 1: encode to sRGB in the shader (8-bit unorm targets).
    encode: f32,
    _pad: f32,
}

/// The draw list's GPU state. Lives with the overlay (render thread) or in a shell's renderer.
#[derive(Default)]
pub struct Draw2dLayer {
    gpu: Option<Gpu>,
    stats: Draw2dGpuStats,
    /// The frame staged by [`Draw2dLayer::draw_list`] (a shell's direct path).
    frame: Draw2dFrame,
    /// Dirty regions on: the list draws into a retained UI target, tile by tile.
    dirty_regions: bool,
    tiles: Draw2dTiles,
    /// The instances of the dirty tiles (reused).
    scratch: Vec<super::Draw2dInstance>,
}

struct Gpu {
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    shader: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    bgl: wgpu::BindGroupLayout,
    bind: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    mono: wgpu::Texture,
    mono_view: wgpu::TextureView,
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    color_layers: u32,
    images: [Option<wgpu::TextureView>; IMAGE_SLOTS],
    blank: wgpu::TextureView,
    buffer: wgpu::Buffer,
    capacity: u64,
    uniform: wgpu::Buffer,
    last_uniform: Option<Uniform>,
    /// Dirty regions: the composite's layout, the clear pipeline (at most one), the composite
    /// pipelines per frame format and the retained target.
    composite_bgl: wgpu::BindGroupLayout,
    composite_layout: wgpu::PipelineLayout,
    clear_layout: wgpu::PipelineLayout,
    clears: Vec<wgpu::RenderPipeline>,
    composites: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    retained: Option<Retained>,
}

impl Draw2dLayer {
    /// A layer with no GPU state yet ([`Self::warm`] builds it).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the pipeline for `format`, the atlas textures, the image slots, the uniform and the
    /// instance buffer, so the first frame that draws creates nothing. With dirty regions on it
    /// also builds the retained path's pipelines; the retained target itself is created by the
    /// first frame (it takes the frame's size) and again on a resize.
    pub fn warm(&mut self, device: &RenderDevice, format: wgpu::TextureFormat) {
        let _span = tracing::info_span!(target: "regia_quick", "draw2d_warm").entered();
        let stats = &mut self.stats;
        let gpu = self.gpu.get_or_insert_with(|| Gpu::new(device, stats));
        gpu.pipeline(device, format, stats);
        if self.dirty_regions {
            gpu.pipeline(device, UI_FORMAT, stats);
            gpu.clear_pipeline(device, stats);
            gpu.composite_pipeline(device, format, stats);
        }
    }

    /// Turns dirty regions on or off (off by default). On, the layer keeps a retained UI target
    /// the size of the frame, hashes the list per tile ([`Draw2dTiles`]) and redraws only the
    /// tiles whose instances changed, then composites the UI target over the frame.
    pub fn set_dirty_regions(&mut self, on: bool) {
        if on != self.dirty_regions {
            self.dirty_regions = on;
            self.tiles.invalidate();
        }
    }

    /// Uses `tile_w` × `tile_h` pixel tiles for dirty regions (default 256 × 64).
    pub fn set_tile_size(&mut self, tile_w: u32, tile_h: u32) {
        self.tiles = Draw2dTiles::new(tile_w, tile_h);
    }

    /// Whether dirty regions are on.
    #[must_use]
    pub fn dirty_regions(&self) -> bool {
        self.dirty_regions
    }

    /// The layer's counters.
    #[must_use]
    pub fn gpu_stats(&self) -> Draw2dGpuStats {
        self.stats
    }

    /// Draws `list` over `view` (`width` × `height` pixels of `format`): hands the list over to
    /// the layer's staged frame, uploads what changed and records one instanced draw.
    #[allow(clippy::too_many_arguments)] // the overlay's target description plus the list
    pub fn draw_list(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        list: &mut Draw2dList,
    ) {
        let mut frame = std::mem::take(&mut self.frame);
        list.hand_over(&mut frame);
        self.draw_frame(device, encoder, view, format, width, height, &mut frame);
        self.frame = frame;
    }

    /// Draws a staged frame ([`Draw2dList::hand_over`]): uploads its atlas pages and images
    /// (then forgets them) and its instances, and records one instanced draw.
    #[allow(clippy::too_many_arguments)] // the overlay's target description plus the frame
    pub fn draw_frame(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        frame: &mut Draw2dFrame,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        let _span = tracing::info_span!(target: "regia_quick", "draw2d_draw").entered();
        if self.dirty_regions {
            self.draw_retained(device, encoder, view, format, width, height, frame);
            return;
        }
        let stats = &mut self.stats;
        stats.tiles = Draw2dTileStats::default();
        let gpu = self.gpu.get_or_insert_with(|| Gpu::new(device, stats));
        let mut uploaded = 0u64;
        uploaded += gpu.upload_pages(device, frame, stats);
        uploaded += gpu.upload_images(device, frame, stats);
        frame.invalidate = false;
        uploaded += gpu.upload_uniform(device, format, width, height);
        uploaded += gpu.upload_instances(device, &frame.instances, stats);
        stats.frames += 1;
        stats.bytes_uploaded = uploaded;
        stats.instance_capacity = gpu.capacity;
        stats.instances = u32::try_from(frame.instances.len()).unwrap_or(u32::MAX);
        if frame.instances.is_empty() {
            return;
        }
        let pipeline_index = gpu.pipeline(device, format, stats);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("regia draw2d"),
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
        pass.set_pipeline(&gpu.pipelines[pipeline_index].1);
        pass.set_bind_group(0, &gpu.bind, &[]);
        pass.set_vertex_buffer(
            0,
            gpu.buffer
                .slice(..u64::from(stats.instances) * Draw2dList::INSTANCE_BYTES),
        );
        pass.draw(0..6, 0..stats.instances);
    }

    /// The dirty-region path: hash the list per tile, redraw the dirty runs of the retained UI
    /// target (scissored: clear, then the instances that touch a dirty tile), and composite the
    /// UI target over `view`. A frame with no dirty tile writes nothing to the GPU.
    #[allow(clippy::too_many_arguments)] // the overlay's target description plus the frame
    fn draw_retained(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        frame: &mut Draw2dFrame,
    ) {
        let Self {
            gpu,
            stats,
            tiles,
            scratch,
            ..
        } = self;
        let gpu = gpu.get_or_insert_with(|| Gpu::new(device, stats));
        let mut uploaded = 0u64;
        uploaded += gpu.upload_pages(device, frame, stats);
        let images = gpu.upload_images(device, frame, stats);
        uploaded += images;
        if std::mem::take(&mut frame.invalidate) || images > 0 {
            tiles.invalidate();
        }
        if gpu.ensure_retained(device, width, height, stats) {
            tiles.invalidate();
        }
        uploaded += gpu.upload_uniform(device, UI_FORMAT, width, height);
        let runs = tiles.update(&frame.instances, width, height).len();
        let tile_stats = tiles.stats();
        scratch.clear();
        if runs > 0 {
            scratch.extend(
                frame
                    .instances
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| tiles.instance_is_dirty(*i))
                    .map(|(_, inst)| *inst),
            );
        }
        uploaded += gpu.upload_instances(device, scratch, stats);
        stats.frames += 1;
        stats.bytes_uploaded = uploaded;
        stats.instance_capacity = gpu.capacity;
        stats.instances = u32::try_from(frame.instances.len()).unwrap_or(u32::MAX);
        stats.tiles = tile_stats;
        if runs > 0 {
            let full = tile_stats.dirty_tiles == tile_stats.tiles;
            let draw = gpu.pipeline(device, UI_FORMAT, stats);
            let clear = gpu.clear_pipeline(device, stats);
            let Some(retained) = gpu.retained.as_ref() else {
                return;
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("regia draw2d dirty tiles"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &retained.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if full {
                            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            let count = u32::try_from(scratch.len()).unwrap_or(u32::MAX);
            let bytes = u64::from(count) * Draw2dList::INSTANCE_BYTES;
            for &run in tiles.runs() {
                let [x, y, w, h] = tiles.run_rect(run);
                if w == 0 || h == 0 {
                    continue;
                }
                pass.set_scissor_rect(x, y, w, h);
                if !full {
                    pass.set_pipeline(&gpu.clears[clear]);
                    pass.draw(0..3, 0..1);
                }
                if count > 0 {
                    pass.set_pipeline(&gpu.pipelines[draw].1);
                    pass.set_bind_group(0, &gpu.bind, &[]);
                    pass.set_vertex_buffer(0, gpu.buffer.slice(..bytes));
                    pass.draw(0..6, 0..count);
                }
            }
        }
        if tile_stats.painted_tiles == 0 {
            // The retained target is empty (every tile that held paint was cleared).
            return;
        }
        let composite = gpu.composite_pipeline(device, format, stats);
        let Some(retained) = gpu.retained.as_ref() else {
            return;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("regia draw2d composite"),
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
        pass.set_pipeline(&gpu.composites[composite].1);
        pass.set_bind_group(0, &retained.bind, &[]);
        // Only the painted tiles hold anything: one scissored triangle per run of them, so the
        // composite reads and blends the HUD's area, not the whole frame.
        for &run in tiles.painted_runs() {
            let [x, y, w, h] = tiles.run_rect(run);
            if w == 0 || h == 0 {
                continue;
            }
            pass.set_scissor_rect(x, y, w, h);
            pass.draw(0..3, 0..1);
        }
    }
}

/// Format of the retained UI target: premultiplied colour, blended in linear and stored
/// sRGB-encoded (the precision of the direct path on an sRGB frame).
pub const UI_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// The retained UI target of the dirty-region path.
struct Retained {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    width: u32,
    height: u32,
}

fn instance_buffer(device: &RenderDevice, size: u64) -> wgpu::Buffer {
    device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("regia draw2d instances"),
        size,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn page_array(device: &RenderDevice, format: wgpu::TextureFormat, layers: u32) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("regia draw2d glyph pages"),
        size: wgpu::Extent3d {
            width: PAGE_SIZE,
            height: PAGE_SIZE,
            depth_or_array_layers: layers,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn array_view(texture: &wgpu::Texture) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    })
}

/// Uploads an image as premultiplied sRGB RGBA8 (an R8 image becomes a white mask).
fn upload_image(device: &RenderDevice, image: &ImmediateImage) -> (wgpu::TextureView, u64) {
    let (w, h) = (image.width.max(1), image.height.max(1));
    let texels = (w as usize) * (h as usize);
    let mut rgba: Vec<u8> = match image.format {
        ImmediateImageFormat::R8 => image
            .data
            .iter()
            .take(texels)
            .flat_map(|&a| [a, a, a, a])
            .collect(),
        ImmediateImageFormat::Rgba8SrgbPremultiplied => image.data.iter().take(texels * 4).copied().collect(),
    };
    rgba.resize(texels * 4, 0);
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("regia draw2d image"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    device.queue.write_texture(
        texture.as_image_copy(),
        &rgba,
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
    (
        texture.create_view(&wgpu::TextureViewDescriptor::default()),
        rgba.len() as u64,
    )
}

impl Gpu {
    fn new(device: &RenderDevice, stats: &mut Draw2dGpuStats) -> Self {
        let dev = &device.device;
        let mono = page_array(device, wgpu::TextureFormat::R8Unorm, ATLAS_PAGES);
        // Colour (emoji) pages are rare: one layer until a second page appears.
        let color = page_array(device, wgpu::TextureFormat::Rgba8UnormSrgb, 1);
        let (blank, _) = upload_image(
            device,
            &ImmediateImage {
                width: 1,
                height: 1,
                format: ImmediateImageFormat::Rgba8SrgbPremultiplied,
                data: std::sync::Arc::from(vec![0u8; 4]),
            },
        );
        stats.textures += 3;
        let sampler = dev.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("regia draw2d sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let uniform = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("regia draw2d uniform"),
            size: std::mem::size_of::<Uniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let buffer = instance_buffer(device, INITIAL_INSTANCE_BYTES);
        stats.buffers += 2;
        let texture_entry =
            |binding: u32, dimension: wgpu::TextureViewDimension| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: dimension,
                    multisampled: false,
                },
                count: None,
            };
        let mut entries = vec![
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            texture_entry(2, wgpu::TextureViewDimension::D2Array),
            texture_entry(3, wgpu::TextureViewDimension::D2Array),
        ];
        for slot in 0..IMAGE_SLOTS as u32 {
            entries.push(texture_entry(4 + slot, wgpu::TextureViewDimension::D2));
        }
        let bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("regia draw2d bgl"),
            entries: &entries,
        });
        let layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("regia draw2d pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("regia draw2d"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let composite_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("regia draw2d composite bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 8,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let composite_layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("regia draw2d composite pl"),
            bind_group_layouts: &[Some(&composite_bgl)],
            immediate_size: 0,
        });
        let clear_layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("regia draw2d clear pl"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let mono_view = array_view(&mono);
        let color_view = array_view(&color);
        let images: [Option<wgpu::TextureView>; IMAGE_SLOTS] = [const { None }; IMAGE_SLOTS];
        let bind = bind_group(
            device,
            &bgl,
            &uniform,
            &sampler,
            &mono_view,
            &color_view,
            &images,
            &blank,
        );
        Self {
            pipelines: Vec::new(),
            shader,
            layout,
            bgl,
            bind,
            sampler,
            mono,
            mono_view,
            color,
            color_view,
            color_layers: 1,
            images,
            blank,
            buffer,
            capacity: INITIAL_INSTANCE_BYTES,
            uniform,
            last_uniform: None,
            composite_bgl,
            composite_layout,
            clear_layout,
            clears: Vec::new(),
            composites: Vec::new(),
            retained: None,
        }
    }

    /// Writes the uniform when the target size or encoding changed; returns the bytes written.
    fn upload_uniform(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> u64 {
        // 8-bit unorm targets store sRGB-encoded colour; sRGB views encode in hardware and
        // float targets hold linear values.
        let encode = matches!(
            format,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
        );
        let uniform = Uniform {
            size: [width as f32, height as f32],
            encode: if encode { 1.0 } else { 0.0 },
            _pad: 0.0,
        };
        if self.last_uniform == Some(uniform) {
            return 0;
        }
        device
            .queue
            .write_buffer(&self.uniform, 0, bytemuck::bytes_of(&uniform));
        self.last_uniform = Some(uniform);
        std::mem::size_of::<Uniform>() as u64
    }

    /// Writes `instances` at the start of the instance buffer (growing it first if needed);
    /// returns the bytes written.
    fn upload_instances(
        &mut self,
        device: &RenderDevice,
        instances: &[super::Draw2dInstance],
        stats: &mut Draw2dGpuStats,
    ) -> u64 {
        let bytes: &[u8] = bytemuck::cast_slice(instances);
        let len = bytes.len() as u64;
        if len > self.capacity {
            self.capacity = len.next_power_of_two();
            self.buffer = instance_buffer(device, self.capacity);
            stats.buffers += 1;
        }
        if len > 0 {
            device.queue.write_buffer(&self.buffer, 0, bytes);
        }
        len
    }

    /// Creates (or recreates on a resize) the retained UI target; true when it did.
    fn ensure_retained(
        &mut self,
        device: &RenderDevice,
        width: u32,
        height: u32,
        stats: &mut Draw2dGpuStats,
    ) -> bool {
        if self
            .retained
            .as_ref()
            .is_some_and(|r| r.width == width && r.height == height)
        {
            return false;
        }
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("regia draw2d retained ui"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: UI_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = device.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("regia draw2d composite bg"),
            layout: &self.composite_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 8,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        });
        stats.textures += 1;
        tracing::debug!(target: "regia_quick", "draw2d retained target {width}x{height}");
        self.retained = Some(Retained {
            _texture: texture,
            view,
            bind,
            width,
            height,
        });
        true
    }

    /// The index of the clear pipeline in `self.clears` (built on first use).
    fn clear_pipeline(&mut self, device: &RenderDevice, stats: &mut Draw2dGpuStats) -> usize {
        if self.clears.is_empty() {
            let pipeline = self.full_screen_pipeline(device, &self.clear_layout, "fs_clear", UI_FORMAT, None);
            stats.pipelines += 1;
            self.clears.push(pipeline);
        }
        0
    }

    /// The index of `format`'s composite pipeline in `self.composites` (built on first use).
    fn composite_pipeline(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
        stats: &mut Draw2dGpuStats,
    ) -> usize {
        if let Some(i) = self.composites.iter().position(|(f, _)| *f == format) {
            return i;
        }
        let encode = matches!(
            format,
            wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
        );
        let entry = if encode {
            "fs_composite_encode"
        } else {
            "fs_composite"
        };
        let pipeline = self.full_screen_pipeline(
            device,
            &self.composite_layout,
            entry,
            format,
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );
        stats.pipelines += 1;
        self.composites.push((format, pipeline));
        self.composites.len() - 1
    }

    fn full_screen_pipeline(
        &self,
        device: &RenderDevice,
        layout: &wgpu::PipelineLayout,
        fragment: &str,
        format: wgpu::TextureFormat,
        blend: Option<wgpu::BlendState>,
    ) -> wgpu::RenderPipeline {
        device
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("regia draw2d full screen"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &self.shader,
                    entry_point: Some("vs_full"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &self.shader,
                    entry_point: Some(fragment),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
    }

    fn rebind(&mut self, device: &RenderDevice) {
        self.bind = bind_group(
            device,
            &self.bgl,
            &self.uniform,
            &self.sampler,
            &self.mono_view,
            &self.color_view,
            &self.images,
            &self.blank,
        );
    }

    /// The index of `format`'s pipeline in `self.pipelines`, built on first use.
    fn pipeline(
        &mut self,
        device: &RenderDevice,
        format: wgpu::TextureFormat,
        stats: &mut Draw2dGpuStats,
    ) -> usize {
        if let Some(i) = self.pipelines.iter().position(|(f, _)| *f == format) {
            return i;
        }
        let attributes = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4
        ];
        let pipeline = device
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("regia draw2d"),
                layout: Some(&self.layout),
                vertex: wgpu::VertexState {
                    module: &self.shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: Draw2dList::INSTANCE_BYTES,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &attributes,
                    })],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &self.shader,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        stats.pipelines += 1;
        self.pipelines.push((format, pipeline));
        self.pipelines.len() - 1
    }

    /// Uploads the frame's staged atlas pages; returns the bytes written.
    fn upload_pages(
        &mut self,
        device: &RenderDevice,
        frame: &mut Draw2dFrame,
        stats: &mut Draw2dGpuStats,
    ) -> u64 {
        let mut bytes = 0u64;
        let staged = frame.pages_len;
        frame.pages_len = 0;
        // A colour page past the colour array's layers grows it (and rebinds) first.
        let needed = frame.pages[..staged]
            .iter()
            .filter(|p| p.kind == 1)
            .map(|p| p.page + 1)
            .max()
            .unwrap_or(0);
        if needed > self.color_layers {
            self.color_layers = ATLAS_PAGES;
            self.color = page_array(device, wgpu::TextureFormat::Rgba8UnormSrgb, self.color_layers);
            self.color_view = array_view(&self.color);
            stats.textures += 1;
            self.rebind(device);
            tracing::debug!(target: "regia_quick", "draw2d colour glyph pages grew to {}", self.color_layers);
        }
        for page in &frame.pages[..staged] {
            let (texture, bpp) = if page.kind == 1 {
                (&self.color, 4)
            } else {
                (&self.mono, 1)
            };
            if page.page >= ATLAS_PAGES || page.data.len() < (PAGE_SIZE * PAGE_SIZE) as usize * bpp {
                continue;
            }
            device.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: page.page,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &page.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(PAGE_SIZE * bpp as u32),
                    rows_per_image: Some(PAGE_SIZE),
                },
                wgpu::Extent3d {
                    width: PAGE_SIZE,
                    height: PAGE_SIZE,
                    depth_or_array_layers: 1,
                },
            );
            bytes += page.data.len() as u64;
        }
        bytes
    }

    /// Uploads the frame's new images into their slots; returns the bytes written.
    fn upload_images(
        &mut self,
        device: &RenderDevice,
        frame: &mut Draw2dFrame,
        stats: &mut Draw2dGpuStats,
    ) -> u64 {
        if frame.images.is_empty() {
            return 0;
        }
        let mut bytes = 0u64;
        for (Draw2dImage(slot), image) in frame.images.drain(..) {
            let Some(target) = self.images.get_mut(usize::from(slot)) else {
                continue;
            };
            let (view, written) = upload_image(device, &image);
            *target = Some(view);
            stats.textures += 1;
            bytes += written;
        }
        self.rebind(device);
        bytes
    }
}

#[allow(clippy::too_many_arguments)] // one binding each
fn bind_group(
    device: &RenderDevice,
    layout: &wgpu::BindGroupLayout,
    uniform: &wgpu::Buffer,
    sampler: &wgpu::Sampler,
    mono: &wgpu::TextureView,
    color: &wgpu::TextureView,
    images: &[Option<wgpu::TextureView>; IMAGE_SLOTS],
    blank: &wgpu::TextureView,
) -> wgpu::BindGroup {
    let view = |i: usize| images[i].as_ref().unwrap_or(blank);
    let mut entries = vec![
        wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: wgpu::BindingResource::Sampler(sampler),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: wgpu::BindingResource::TextureView(mono),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: wgpu::BindingResource::TextureView(color),
        },
    ];
    for slot in 0..IMAGE_SLOTS {
        entries.push(wgpu::BindGroupEntry {
            binding: 4 + slot as u32,
            resource: wgpu::BindingResource::TextureView(view(slot)),
        });
    }
    device.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("regia draw2d bg"),
        layout,
        entries: &entries,
    })
}
