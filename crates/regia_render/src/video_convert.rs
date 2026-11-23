// SPDX-License-Identifier: MPL-2.0
//! The GPU video convert pass (specs/50 § 2.7, ADR-2044): planar YUV to encoded R'G'B'A8 in a
//! compute shader (`shaders/video_convert.wgsl`), one double-buffered target per player.
//!
//! [`VideoConverter`] records a conversion into any encoder (tools, tests, a host's own frame).
//! [`VideoConvertPass`] is the same work as a [`FramePass`] at [`GraphSlot::Overlay`], fed by a
//! shared [`VideoConvertJobs`] queue: a host (the media crate) submits each newly presented picture,
//! the pass converts it into the player's back texture inside the frame's encoder and swaps, so a
//! sampler never sees a half-written picture. The pass is off by default: it exists only after
//! [`FrameRenderer::enable_video_convert`], and frames without it are unchanged.
//!
//! Memory is bounded: at most one pending picture per player (a newer submit replaces an older
//! one), two RGBA8 textures per player, and one staging buffer sized to the largest picture seen.

use crate::frame_work::counted::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::frame::FrameRenderer;
use crate::video::{YuvMatrix, YuvPlanes, convert_rows};
use crate::{FramePass, FramePassContext, GraphSlot, TrackedDevice};

/// Name of the pass in `render.graph`.
pub const VIDEO_CONVERT_PASS_NAME: &str = "video_convert";

/// WGSL source of the convert shader.
pub const VIDEO_CONVERT_WGSL: &str = include_str!("../shaders/video_convert.wgsl");

/// Workgroup edge of the shader.
const GROUP: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    size: [u32; 2],
    chroma_size: [u32; 2],
    shift: [u32; 2],
    sample_bytes: u32,
    pad: u32,
    offsets: [u32; 4],
    rows: [[f32; 4]; 3],
}

/// A picture with owned planes, as queued for the pass.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedYuv {
    /// Luma width.
    pub width: u32,
    /// Luma height.
    pub height: u32,
    /// Bits per sample.
    pub bit_depth: u8,
    /// Chroma subsampling shifts.
    pub chroma_shift: (u32, u32),
    /// Matrix.
    pub matrix: YuvMatrix,
    /// Full range.
    pub full_range: bool,
    /// Y, U, V (shared with the decoder's picture, no copy).
    pub planes: [Arc<[u8]>; 3],
}

impl OwnedYuv {
    /// Borrowed view.
    #[must_use]
    pub fn planes(&self) -> YuvPlanes<'_> {
        YuvPlanes {
            width: self.width,
            height: self.height,
            bit_depth: self.bit_depth,
            chroma_shift: self.chroma_shift,
            matrix: self.matrix,
            full_range: self.full_range,
            planes: [&self.planes[0], &self.planes[1], &self.planes[2]],
        }
    }

    /// Bytes held.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.planes.iter().map(|p| p.len()).sum()
    }
}

/// Packs the three planes into one word-aligned buffer and builds the uniform.
fn pack(planes: &YuvPlanes<'_>) -> (Vec<u8>, Params) {
    let sb = planes.sample_bytes();
    let (cw, ch) = planes.chroma_size();
    let luma = planes.width as usize * planes.height as usize * sb;
    let chroma = cw as usize * ch as usize * sb;
    let align = |n: usize| n.div_ceil(4) * 4;
    let mut data = Vec::with_capacity(align(luma) + 2 * align(chroma));
    let mut offsets = [0u32; 4];
    for (i, len) in [luma, chroma, chroma].into_iter().enumerate() {
        offsets[i] = data.len() as u32;
        data.extend_from_slice(&planes.planes[i][..len]);
        data.resize(align(data.len()), 0);
    }
    let rows = convert_rows(planes.matrix, planes.full_range, planes.bit_depth).map(|r| r.map(|c| c as f32));
    let params = Params {
        size: [planes.width, planes.height],
        chroma_size: [cw, ch],
        shift: [planes.chroma_shift.0, planes.chroma_shift.1],
        sample_bytes: sb as u32,
        pad: 0,
        offsets,
        rows,
    };
    (data, params)
}

/// Two RGBA8 textures (storage-writable, sampled through a sRGB view) for one player.
pub struct VideoTarget {
    textures: [wgpu::Texture; 2],
    srgb_views: [wgpu::TextureView; 2],
    storage_views: [wgpu::TextureView; 2],
    front: usize,
    width: u32,
    height: u32,
    data: Option<(wgpu::Buffer, u64)>,
    uniform: wgpu::Buffer,
    serial: u64,
}

impl std::fmt::Debug for VideoTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoTarget")
            .field("size", &(self.width, self.height))
            .field("front", &self.front)
            .field("serial", &self.serial)
            .finish_non_exhaustive()
    }
}

impl VideoTarget {
    /// Two textures of `width` x `height`.
    #[must_use]
    pub fn new(device: &TrackedDevice, width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        let make = |i: usize| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(if i == 0 { "video frame a" } else { "video frame b" }),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
            })
        };
        let textures = [make(0), make(1)];
        let srgb = |t: &wgpu::Texture| {
            t.create_view(&wgpu::TextureViewDescriptor {
                label: Some("video frame srgb"),
                format: Some(wgpu::TextureFormat::Rgba8UnormSrgb),
                // Sampled only: a sRGB view cannot be a storage binding.
                usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                ..Default::default()
            })
        };
        let storage = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video convert params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            srgb_views: [srgb(&textures[0]), srgb(&textures[1])],
            storage_views: [storage(&textures[0]), storage(&textures[1])],
            textures,
            front: 0,
            width,
            height,
            data: None,
            uniform,
            serial: 0,
        }
    }

    /// Size in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The texture holding the last finished picture.
    #[must_use]
    pub fn front_texture(&self) -> &wgpu::Texture {
        &self.textures[self.front]
    }

    /// Its sRGB view (for the UI or a material).
    #[must_use]
    pub fn front_view(&self) -> &wgpu::TextureView {
        &self.srgb_views[self.front]
    }

    /// Pictures converted into this target so far.
    #[must_use]
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// GPU bytes held (two textures plus the staging buffer).
    #[must_use]
    pub fn gpu_bytes(&self) -> u64 {
        2 * u64::from(self.width) * u64::from(self.height) * 4 + self.data.as_ref().map_or(0, |d| d.1)
    }
}

/// The convert pipeline.
pub struct VideoConverter {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

impl std::fmt::Debug for VideoConverter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoConverter").finish_non_exhaustive()
    }
}

impl VideoConverter {
    /// Builds the pipeline.
    #[must_use]
    pub fn new(device: &TrackedDevice) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video_convert.wgsl"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_CONVERT_WGSL.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video convert bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video convert layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("video convert"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        Self { pipeline, layout }
    }

    /// Records the conversion of `planes` into `target`'s back texture and swaps it to the front.
    /// `write` stages buffer contents (`wgpu::Queue::write_buffer` or the frame's
    /// [`FramePassContext::write_buffer`]); they land before the encoder's submit.
    ///
    /// # Errors
    /// When the planes are shorter than their size says or the size differs from the target's.
    pub fn record(
        &self,
        device: &TrackedDevice,
        write: &dyn Fn(&wgpu::Buffer, u64, &[u8]),
        encoder: &mut wgpu::CommandEncoder,
        planes: &YuvPlanes<'_>,
        target: &mut VideoTarget,
    ) -> Result<(), String> {
        if !planes.is_complete() {
            return Err("video planes are shorter than the picture size".into());
        }
        if (planes.width, planes.height) != target.size() {
            return Err(format!(
                "picture {}x{} does not fit the {}x{} target",
                planes.width, planes.height, target.width, target.height
            ));
        }
        let _span = tracing::trace_span!("video_convert").entered();
        let (data, params) = pack(planes);
        let need = (data.len() as u64).max(4);
        if target.data.as_ref().is_none_or(|(_, cap)| *cap < need) {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("video planes"),
                size: need,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            target.data = Some((buffer, need));
        }
        let Some((buffer, _)) = target.data.as_ref() else {
            return Err("video plane buffer missing".into());
        };
        write(buffer, 0, &data);
        write(&target.uniform, 0, bytemuck::bytes_of(&params));
        let back = 1 - target.front;
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video convert bg"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: target.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&target.storage_views[back]),
                },
            ],
        });
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("video convert"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&self.pipeline);
            pass.counted_set_bind_group(0, &bind, &[]);
            pass.counted_dispatch(planes.width.div_ceil(GROUP), planes.height.div_ceil(GROUP), 1);
        }
        target.front = back;
        target.serial += 1;
        Ok(())
    }
}

/// Counters of the convert queue (`media.state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoConvertStats {
    /// Pictures converted on the GPU.
    pub converted: u64,
    /// Submitted pictures replaced by a newer one before the pass ran.
    pub replaced: u64,
    /// Pictures the pass refused (size changed mid-flight, short planes).
    pub failed: u64,
    /// Pictures waiting.
    pub pending: u32,
    /// GPU bytes held by the targets.
    pub gpu_bytes: u64,
}

#[derive(Default)]
struct JobsInner {
    pending: BTreeMap<u64, OwnedYuv>,
    targets: BTreeMap<u64, VideoTarget>,
    converter: Option<VideoConverter>,
    stats: VideoConvertStats,
    closed: Vec<u64>,
}

/// The queue between the media crate and [`VideoConvertPass`] (clones share it).
#[derive(Clone, Default)]
pub struct VideoConvertJobs {
    inner: Arc<Mutex<JobsInner>>,
}

impl std::fmt::Debug for VideoConvertJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoConvertJobs")
            .field("stats", &self.stats())
            .finish()
    }
}

impl VideoConvertJobs {
    fn lock(&self) -> MutexGuard<'_, JobsInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `picture` for `player`; a picture still waiting for that player is replaced.
    pub fn submit(&self, player: u64, picture: OwnedYuv) {
        let mut inner = self.lock();
        if inner.pending.insert(player, picture).is_some() {
            inner.stats.replaced += 1;
        }
    }

    /// Frees `player`'s target and drops its pending picture.
    pub fn remove(&self, player: u64) {
        let mut inner = self.lock();
        inner.pending.remove(&player);
        inner.targets.remove(&player);
        inner.closed.push(player);
    }

    /// The sRGB view of `player`'s last converted picture, with the target serial.
    #[must_use]
    pub fn view(&self, player: u64) -> Option<(u64, wgpu::TextureView)> {
        let inner = self.lock();
        inner
            .targets
            .get(&player)
            .filter(|t| t.serial > 0)
            .map(|t| (t.serial, t.front_view().clone()))
    }

    /// The texture of `player`'s last converted picture (for a copy or a readback).
    #[must_use]
    pub fn texture(&self, player: u64) -> Option<wgpu::Texture> {
        let inner = self.lock();
        inner
            .targets
            .get(&player)
            .filter(|t| t.serial > 0)
            .map(|t| t.front_texture().clone())
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> VideoConvertStats {
        let inner = self.lock();
        let mut s = inner.stats;
        s.pending = inner.pending.len() as u32;
        s.gpu_bytes = inner.targets.values().map(VideoTarget::gpu_bytes).sum();
        s
    }

    /// Converts every pending picture into `encoder` (what the pass does each frame). Returns how
    /// many it converted.
    pub fn record_pending(
        &self,
        device: &TrackedDevice,
        write: &dyn Fn(&wgpu::Buffer, u64, &[u8]),
        encoder: &mut wgpu::CommandEncoder,
    ) -> usize {
        let mut inner = self.lock();
        if inner.pending.is_empty() {
            return 0;
        }
        let inner = &mut *inner;
        let converter = inner.converter.get_or_insert_with(|| VideoConverter::new(device));
        let mut done = 0;
        for (player, picture) in std::mem::take(&mut inner.pending) {
            // A picture that cannot convert must not destroy the player's last good frame: the
            // target (and its serial) is replaced only after the picture proves convertible.
            if picture.width == 0 || picture.height == 0 || !picture.planes().is_complete() {
                inner.stats.failed += 1;
                tracing::warn!(player, "video convert skipped: incomplete planes");
                continue;
            }
            let size = (picture.width.max(1), picture.height.max(1));
            let target = inner
                .targets
                .entry(player)
                .and_modify(|t| {
                    if t.size() != size {
                        *t = VideoTarget::new(device, size.0, size.1);
                    }
                })
                .or_insert_with(|| VideoTarget::new(device, size.0, size.1));
            match converter.record(device, write, encoder, &picture.planes(), target) {
                Ok(()) => {
                    inner.stats.converted += 1;
                    done += 1;
                }
                Err(e) => {
                    inner.stats.failed += 1;
                    tracing::warn!(player, "video convert skipped: {e}");
                }
            }
        }
        done
    }
}

/// The convert work as a frame pass (registered by [`FrameRenderer::enable_video_convert`]).
#[derive(Debug, Clone, Default)]
pub struct VideoConvertPass {
    jobs: VideoConvertJobs,
    last_frame: Option<u64>,
}

impl VideoConvertPass {
    /// A pass draining `jobs`.
    #[must_use]
    pub fn new(jobs: VideoConvertJobs) -> Self {
        Self {
            jobs,
            last_frame: None,
        }
    }
}

impl FramePass for VideoConvertPass {
    fn name(&self) -> &str {
        VIDEO_CONVERT_PASS_NAME
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::Overlay
    }

    fn writes(&self) -> &[&'static str] {
        &["video_frames"]
    }

    fn writes_color(&self, _post: Option<&crate::PostSettings>) -> bool {
        false
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        // Several views may reach the slot in one frame; convert once.
        if self.last_frame == Some(ctx.frame_index) {
            return Ok(());
        }
        self.last_frame = Some(ctx.frame_index);
        // Writes are collected while the encoder records and staged through the frame's queue
        // afterwards: staged writes land before the frame's single submit either way.
        let writes: std::cell::RefCell<Vec<(wgpu::Buffer, u64, Vec<u8>)>> = std::cell::RefCell::default();
        let collect =
            |b: &wgpu::Buffer, o: u64, d: &[u8]| writes.borrow_mut().push((b.clone(), o, d.to_vec()));
        let device = ctx.device;
        self.jobs.record_pending(device, &collect, ctx.encoder);
        for (buffer, offset, data) in writes.into_inner() {
            ctx.write_buffer(&buffer, offset, &data);
        }
        Ok(())
    }

    fn prewarm(&mut self, device: &TrackedDevice) -> Result<usize, String> {
        let mut inner = self.jobs.lock();
        if inner.converter.is_none() {
            inner.converter = Some(VideoConverter::new(device));
            return Ok(1);
        }
        Ok(0)
    }
}

impl FrameRenderer {
    /// Registers the video convert pass ([`VideoConvertPass`], ADR-2044) draining `jobs` when it is
    /// not registered yet; returns `true` when it registered it now. Off by default: until this is
    /// called no video work happens in the frame.
    pub fn enable_video_convert(&mut self, jobs: &VideoConvertJobs) -> bool {
        if self
            .registered_passes()
            .iter()
            .any(|name| name == VIDEO_CONVERT_PASS_NAME)
        {
            return false;
        }
        self.register_pass(Box::new(VideoConvertPass::new(jobs.clone())))
            .is_ok()
    }

    /// Removes the video convert pass; returns whether it was registered.
    pub fn disable_video_convert(&mut self) -> bool {
        self.remove_pass(VIDEO_CONVERT_PASS_NAME).is_some()
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::ContractRow::new(
        "video_convert.wgsl",
        "Params",
        size_of::<Params>(),
        vec![
            ("size", std::mem::offset_of!(Params, size)),
            ("chroma_size", std::mem::offset_of!(Params, chroma_size)),
            ("shift", std::mem::offset_of!(Params, shift)),
            ("sample_bytes", std::mem::offset_of!(Params, sample_bytes)),
            ("_pad0", std::mem::offset_of!(Params, pad)),
            ("offsets", std::mem::offset_of!(Params, offsets)),
            ("row_r", std::mem::offset_of!(Params, rows)),
            ("row_g", std::mem::offset_of!(Params, rows) + 16),
            ("row_b", std::mem::offset_of!(Params, rows) + 32),
        ],
    )]
}
