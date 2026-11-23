// SPDX-License-Identifier: MPL-2.0
//! The compute skinning pass (Tincal B2, [`crate::SkinningOptions`]).
//!
//! Each skinned instance on the compute path gets a **twin**: two vertex buffers of its own
//! (this frame's and last frame's skinned vertices, swapped each frame) that a compute pass
//! fills from the mesh's source vertices and the instance's palette. The renderer registers
//! the twin as a mesh entry under its own key, so the shadow, depth, colour and decal passes
//! draw it like a rigid mesh (its joint weights are zero), and the velocity pass reads both
//! buffers for the motion of the deformation. A pose that holds still for two frames is not
//! dispatched again (both buffers already hold it).
//!
//! Inputs per twin: the source vertices (one storage copy per mesh key, uploaded once), the
//! optional eight-influence table, the palette (`Mat3x4` rows, or [`crate::DualQuatJoint`]s
//! for dual quaternions, converted on the CPU: a few hundred joints per frame) and a small
//! uniform. Everything is written with `queue.write_buffer` when its bytes change, so it lands
//! before the frame's command buffer; the dispatches are recorded right after the frame's
//! early uploads, before any pass.

use crate::frame_work::counted::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::RenderError;
use crate::lab_frame::LabVertex;
use crate::skin::{JointMatrix, SkinningMethod};
use crate::skinning::{SkinInfluences8, SkinningOptions};

/// Threads per workgroup (`skin_compute.wgsl`).
const WORKGROUP: u32 = 64;
const FLAG_DQ: u32 = 1;
const FLAG_EIGHT: u32 = 2;
const FLAG_DQ_SCALE: u32 = 4;
/// Frames a twin or a source copy stays resident without being drawn.
const IDLE_FRAMES: u64 = 240;
/// Top byte of a twin's mesh key (`'['`): clear of the runtime's `'M'` keys,
/// [`crate::EPHEMERAL_KEY`] (`0xFE`) and [`crate::SHADOW_LOD_MESH_TAG`] (`0xFF`).
pub const SKIN_TWIN_KEY_TAG: u64 = 0x5B00_0000_0000_0000;

/// What the compute skinning path did on the last frame
/// ([`crate::FrameRenderer::skinning_stats`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkinningStats {
    /// [`crate::FrameRenderer::set_compute_skinning`]: every skinned mesh on the compute path.
    pub compute_all: bool,
    /// Instances with options ([`crate::FrameRenderer::set_skinning_options`]).
    pub options: usize,
    /// `None` when the device runs the compute path; otherwise why every mesh stays on the
    /// vertex-shader path.
    pub unsupported: Option<String>,
    /// Skinned instances drawn from a compute twin on the last frame.
    pub instances: usize,
    /// Of those, the ones dispatched (the rest held a settled pose).
    pub dispatched: usize,
    /// Vertices skinned by the dispatches.
    pub vertices: u64,
    /// Instances blended with dual quaternions.
    pub dual_quaternion: usize,
    /// Instances read with eight influences.
    pub eight_influences: usize,
    /// Instances whose eight-influence table did not match the vertex count (drawn with four).
    pub influence_mismatch: usize,
    /// Twins resident (drawn within the idle window).
    pub resident: usize,
    /// Of [`Self::instances`], the ones skinned from a counted mesh's upload: the kernel reads
    /// the adopted vertex buffer (or a GPU copy of it), with no CPU vertices (ADR-7721).
    pub counted: usize,
}

/// The compute pipeline and its layout, shared per device.
#[derive(Clone)]
pub(crate) struct SkinKernel {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

fn storage(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Why `device` cannot run the kernel, if it cannot.
pub(crate) fn unsupported_reason(
    adapter_flags: wgpu::DownlevelFlags,
    limits: &wgpu::Limits,
) -> Option<String> {
    if !adapter_flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS) {
        return Some("the device has no compute shaders".into());
    }
    if limits.max_storage_buffers_per_shader_stage < 4 {
        return Some(format!(
            "the device allows {} storage buffers per compute stage (4 needed)",
            limits.max_storage_buffers_per_shader_stage
        ));
    }
    None
}

impl SkinKernel {
    fn build(d: &wgpu::Device) -> Result<Self, RenderError> {
        let d = &crate::device::TrackedDevice::observe(d);
        let module = crate::shader_lib::compose_source(
            include_str!("../shaders/skin_compute.wgsl"),
            "skin_compute.wgsl",
            HashMap::new(),
        )?;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("skin compute"),
            source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
        });
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("skin compute bgl"),
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
                storage(1, true),
                storage(2, true),
                storage(3, true),
                storage(4, false),
            ],
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("skin compute layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("skin compute"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("skin"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(d).as_deref(),
        });
        Ok(Self { pipeline, layout })
    }

    /// The device's kernel, built once per device in `shared`.
    pub(crate) fn shared(
        d: &wgpu::Device,
        shared: &crate::device_pipelines::DevicePipelines,
    ) -> Result<Self, RenderError> {
        // The shader epoch is part of the key: a dev build's committed edit of the kernel (or a
        // library module) builds a new kernel instead of returning the cached one.
        let key = crate::device_pipelines::pipeline_key(&(
            "skin compute",
            include_str!("../shaders/skin_compute.wgsl"),
            crate::shader_reload::shader_epoch(),
        ));
        let built = shared.get_or_create(key, || Arc::new(Self::build(d).map_err(|e| e.to_string())));
        (*built).clone().map_err(RenderError::Shader)
    }
}

/// One mesh's source vertices as a storage buffer.
struct Source {
    buf: wgpu::Buffer,
    /// A counted mesh's adopted vertex buffer these vertices come from (ADR-7721), and whether
    /// [`Self::buf`] is that buffer itself (bound as storage, not a copy).
    adopted: Option<(wgpu::Buffer, bool)>,
    bytes: usize,
    /// Bumped on every upload (twins skin again).
    generation: u64,
    /// Bumped when the buffer is replaced (twins rebuild their bind groups).
    buf_generation: u64,
    last_used: u64,
}

/// One skinned instance's compute state (see the module docs).
struct Twin {
    source_key: u64,
    vertex_count: u32,
    out: [wgpu::Buffer; 2],
    /// The buffer holding this frame's result.
    current: usize,
    palette: wgpu::Buffer,
    palette_bytes: Vec<u8>,
    params: wgpu::Buffer,
    params_value: [u32; 4],
    /// The eight-influence table: its `Arc` address and buffer.
    influences: Option<(usize, wgpu::Buffer)>,
    /// Bind groups writing `out[0]` and `out[1]`, and the source buffer generation they bind.
    groups: Option<([wgpu::BindGroup; 2], u64)>,
    /// Source content generation of the last dispatch.
    source_seen: u64,
    /// Consecutive dispatches with the same inputs (two: both buffers hold the pose).
    settled: u8,
    /// Frame serial this twin was last drawn on.
    touched: u64,
    /// Last frame's buffer holds last frame's pose (for motion vectors).
    prev_valid: bool,
}

/// The compute skinning state of one [`crate::FrameRenderer`].
#[derive(Default)]
pub(crate) struct SkinCompute {
    /// The kernel, or why it is unavailable (`None` until first needed).
    kernel: Option<Result<SkinKernel, String>>,
    pub(crate) compute_all: bool,
    pub(crate) options: HashMap<u64, SkinningOptions>,
    sources: HashMap<u64, Source>,
    twins: HashMap<u64, Twin>,
    /// One-element stand-in for an absent eight-influence table.
    dummy: Option<wgpu::Buffer>,
    /// Frame serial (one per render call).
    serial: u64,
    /// This frame's dispatches: twin key, output index, workgroups.
    pending: Vec<(u64, usize, u32)>,
    stats: SkinningStats,
    /// The frame being built ([`Self::begin_frame`] resets it).
    frame: SkinningStats,
    /// Memory pressure ([`Self::request_trim`]): the next frame drops every twin and source copy
    /// not drawn on the frame before it, instead of waiting out the idle window.
    trim_pending: bool,
    /// GPU copies of counted meshes' adopted vertex buffers into their source copies (source,
    /// destination, bytes; ADR-7721), recorded by [`Self::encode`] before the dispatches.
    seeds: Vec<(wgpu::Buffer, wgpu::Buffer, u64)>,
    /// The palette being packed and the dual quaternions behind it, kept between instances
    /// (ADR-385): an instance whose palette did not change copies nothing new.
    palette_scratch: Vec<u8>,
    dq_scratch: Vec<crate::skin::DualQuatJoint>,
}

/// A twin's mesh key for the instance `object_key` of mesh `mesh_key`.
#[must_use]
pub(crate) fn twin_key(mesh_key: u64, object_key: u64) -> u64 {
    let mut h = mesh_key.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ object_key.rotate_left(29);
    h ^= h >> 31;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    SKIN_TWIN_KEY_TAG | (h & 0x00FF_FFFF_FFFF_FFFF)
}

/// What one skinned instance is skinned with on the compute path.
pub(crate) struct TwinRequest<'a> {
    pub mesh_key: u64,
    pub object_key: u64,
    pub vertices: &'a [LabVertex],
    /// A counted mesh's adopted vertex buffer and its vertex count (ADR-7721): skinned from the
    /// GPU in place of `vertices` (empty then).
    pub buffer: Option<(&'a wgpu::Buffer, u32)>,
    pub joints: &'a [JointMatrix],
    pub options: Option<&'a SkinningOptions>,
    /// The vertex path would see last frame's palette for this instance (same joint count,
    /// drawn last frame, no camera cut).
    pub prev_ok: bool,
    /// The mesh key names a one-frame upload (key 0): its source is uploaded again.
    pub ephemeral: bool,
}

/// Twin output buffers: written by the kernel, drawn as vertices, and copied out by
/// [`skin_on_gpu`]'s readback (a copy source needs `COPY_SRC`).
const OUT_USAGE: wgpu::BufferUsages = wgpu::BufferUsages::STORAGE
    .union(wgpu::BufferUsages::VERTEX)
    .union(wgpu::BufferUsages::COPY_SRC);

fn make_buffer(d: &wgpu::Device, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    crate::device::TrackedDevice::observe(d).create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16).next_multiple_of(4),
        usage,
        mapped_at_creation: false,
    })
}

/// The palette bytes the kernel reads, the flags they need, and whether they blend with dual
/// quaternions.
#[cfg(test)]
fn palette_bytes(joints: &[JointMatrix], method: SkinningMethod) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let flags = palette_bytes_into(joints, method, &mut out, &mut Vec::new());
    (out, flags)
}

/// `palette_bytes` written into `out` (replacing its contents), with `dq` as the dual
/// quaternions' scratch: both keep their room between instances and frames (ADR-385).
fn palette_bytes_into(
    joints: &[JointMatrix],
    method: SkinningMethod,
    out: &mut Vec<u8>,
    dq: &mut Vec<crate::skin::DualQuatJoint>,
) -> u32 {
    out.clear();
    match method {
        SkinningMethod::Linear => {
            out.extend_from_slice(bytemuck::cast_slice(joints));
            0
        }
        SkinningMethod::DualQuaternion => {
            dq.clear();
            dq.extend(joints.iter().map(|j| j.to_dual_quat()));
            let scaled = dq.iter().any(|j| !j.is_rigid(1e-4));
            out.extend_from_slice(bytemuck::cast_slice(dq.as_slice()));
            FLAG_DQ | if scaled { FLAG_DQ_SCALE } else { 0 }
        }
    }
}

impl SkinCompute {
    /// The kernel, built on first use (`None` with the reason recorded when unavailable).
    fn kernel(&mut self, device: &crate::RenderDevice) -> Option<SkinKernel> {
        if self.kernel.is_none() {
            let flags = device.adapter.get_downlevel_capabilities().flags;
            self.kernel = Some(match unsupported_reason(flags, &device.device.limits()) {
                Some(reason) => Err(reason),
                None => {
                    SkinKernel::shared(&device.device, device.shared_pipelines()).map_err(|e| e.to_string())
                }
            });
            if let Some(Err(reason)) = &self.kernel {
                tracing::warn!(target: "regia_render", %reason, "compute skinning unavailable: skinned meshes stay on the vertex-shader path");
            }
        }
        self.kernel.as_ref().and_then(|k| k.as_ref().ok()).cloned()
    }

    /// Drops the kernel and the bind groups made with its layout (a shader hot swap): the next
    /// frame builds the kernel from the edited source and skins every twin again. Sources,
    /// twins and options stay.
    pub(crate) fn forget_kernel(&mut self) {
        self.kernel = None;
        for twin in self.twins.values_mut() {
            // A settled twin holds the old kernel's output: skin it again.
            twin.groups = None;
            twin.settled = 0;
            twin.prev_valid = false;
        }
    }

    /// Builds the kernel now (the warm-up), when the device can run it.
    pub(crate) fn prewarm(&mut self, device: &crate::RenderDevice) {
        let _ = self.kernel(device);
    }

    /// Whether the instance `object_key` goes through the compute path.
    pub(crate) fn wants(&self, object_key: u64) -> bool {
        if matches!(self.kernel, Some(Err(_))) {
            return false;
        }
        self.compute_all
            || self
                .options
                .get(&object_key)
                .is_some_and(SkinningOptions::wants_compute)
    }

    /// Starts a frame: advances the serial and returns the twin keys dropped as idle (the
    /// renderer removes their mesh entries).
    pub(crate) fn begin_frame(&mut self) -> Vec<u64> {
        self.serial += 1;
        // Dispatches of a frame that stopped before recording them never ran: skin again.
        for (key, _, _) in std::mem::take(&mut self.pending) {
            if let Some(twin) = self.twins.get_mut(&key) {
                twin.settled = 0;
                twin.prev_valid = false;
            }
        }
        self.frame = SkinningStats {
            compute_all: self.compute_all,
            options: self.options.len(),
            unsupported: match &self.kernel {
                Some(Err(reason)) => Some(reason.clone()),
                _ => None,
            },
            ..SkinningStats::default()
        };
        let serial = self.serial;
        let idle = if std::mem::take(&mut self.trim_pending) {
            1
        } else {
            IDLE_FRAMES
        };
        let mut dropped = Vec::new();
        self.twins.retain(|key, twin| {
            let keep = serial.saturating_sub(twin.touched) <= idle;
            if !keep {
                dropped.push(*key);
            }
            keep
        });
        self.sources
            .retain(|_, source| serial.saturating_sub(source.last_used) <= idle);
        dropped
    }

    /// Under memory pressure: the next frame frees every twin and source copy the frame before
    /// it did not draw (they are rebuilt when drawn again).
    pub(crate) fn request_trim(&mut self) {
        self.trim_pending = true;
    }

    /// GPU bytes the compute path holds: source vertex copies, twin output pairs, palettes,
    /// parameters and eight-influence tables (the stand-in table included).
    pub(crate) fn gpu_bytes(&self) -> u64 {
        // A counted mesh's adopted buffer bound in place is the asset server's, not ours.
        let sources: u64 = self
            .sources
            .values()
            .filter(|s| !s.adopted.as_ref().is_some_and(|(_, bound)| *bound))
            .map(|s| s.buf.size())
            .sum();
        let twins: u64 = self
            .twins
            .values()
            .map(|t| {
                t.out[0].size()
                    + t.out[1].size()
                    + t.palette.size()
                    + t.params.size()
                    + t.influences.as_ref().map_or(0, |(_, b)| b.size())
            })
            .sum();
        sources + twins + self.dummy.as_ref().map_or(0, wgpu::Buffer::size)
    }

    /// Forgets the twins and source copies of `source_keys` (released meshes); returns the
    /// twin keys dropped.
    pub(crate) fn release(&mut self, source_keys: &[u64]) -> Vec<u64> {
        let mut dropped = Vec::new();
        for key in source_keys {
            self.sources.remove(key);
        }
        self.twins.retain(|key, twin| {
            let keep = !source_keys.contains(&twin.source_key);
            if !keep {
                dropped.push(*key);
            }
            keep
        });
        dropped
    }

    /// Prepares the twin of one skinned instance for this frame: uploads what changed and
    /// queues its dispatch. Returns the twin's key and this frame's output buffer, or `None`
    /// to draw the instance on the vertex-shader path (no kernel, or the mesh is too large).
    pub(crate) fn prepare(
        &mut self,
        device: &crate::RenderDevice,
        q: &crate::device::TrackedQueue,
        request: &TwinRequest<'_>,
    ) -> Option<(u64, wgpu::Buffer)> {
        let _span = tracing::trace_span!("skin_compute_prepare").entered();
        let kernel = self.kernel(device)?;
        let d = &device.device;
        let (vertex_count, vertex_bytes) = match request.buffer {
            Some((_, count)) => (count, count as usize * std::mem::size_of::<LabVertex>()),
            None => (
                u32::try_from(request.vertices.len()).ok()?,
                std::mem::size_of_val(request.vertices),
            ),
        };
        let limits = d.limits();
        if vertex_count == 0
            || request.joints.is_empty()
            || vertex_bytes as u64 > limits.max_storage_buffer_binding_size
            || vertex_count.div_ceil(WORKGROUP) > limits.max_compute_workgroups_per_dimension
        {
            return None;
        }
        let key = twin_key(request.mesh_key, request.object_key);
        if let Some(twin) = self.twins.get(&key)
            && twin.touched == self.serial
        {
            // Drawn twice in one frame (one key submitted twice): one skinning serves both.
            return Some((key, twin.out[twin.current].clone()));
        }
        let serial = self.serial;
        // A counted mesh (ADR-7721): its adopted vertex buffer, bound as the source when it is a
        // storage buffer, else copied into one on the GPU; again when the upload changed.
        if let Some((adopted, _)) = request.buffer {
            let current = self
                .sources
                .get(&request.mesh_key)
                .is_some_and(|s| s.adopted.as_ref().is_some_and(|(buf, _)| buf == adopted));
            if !current {
                let bound = adopted.usage().contains(wgpu::BufferUsages::STORAGE);
                let buf = if bound {
                    adopted.clone()
                } else {
                    let copy = make_buffer(
                        d,
                        "skin compute source",
                        vertex_bytes as u64,
                        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    );
                    self.seeds
                        .push((adopted.clone(), copy.clone(), vertex_bytes as u64));
                    copy
                };
                let (generation, buf_generation) = self
                    .sources
                    .get(&request.mesh_key)
                    .map_or((0, 0), |s| (s.generation, s.buf_generation));
                self.sources.insert(
                    request.mesh_key,
                    Source {
                        buf,
                        adopted: Some((adopted.clone(), bound)),
                        bytes: vertex_bytes,
                        generation: generation + 1,
                        buf_generation: buf_generation + 1,
                        last_used: serial,
                    },
                );
            }
            self.frame.counted += 1;
        }
        // Source vertices: one copy per mesh key, uploaded again when the data changes size
        // (as the renderer's mesh cache does) or the key is a one-frame key.
        let source = self.sources.entry(request.mesh_key);
        let source = match source {
            std::collections::hash_map::Entry::Occupied(slot) => {
                let source = slot.into_mut();
                if request.buffer.is_none() && (source.bytes != vertex_bytes || request.ephemeral) {
                    if source.bytes != vertex_bytes {
                        source.buf = make_buffer(
                            d,
                            "skin compute source",
                            vertex_bytes as u64,
                            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                        );
                        source.bytes = vertex_bytes;
                        source.buf_generation += 1;
                    }
                    q.write_buffer(&source.buf, 0, bytemuck::cast_slice(request.vertices));
                    source.generation += 1;
                }
                source
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                let buf = make_buffer(
                    d,
                    "skin compute source",
                    vertex_bytes as u64,
                    wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                );
                q.write_buffer(&buf, 0, bytemuck::cast_slice(request.vertices));
                slot.insert(Source {
                    buf,
                    adopted: None,
                    bytes: vertex_bytes,
                    generation: 1,
                    buf_generation: 1,
                    last_used: serial,
                })
            }
        };
        source.last_used = serial;
        let (source_buf, source_generation, source_buf_generation) =
            (source.buf.clone(), source.generation, source.buf_generation);

        let method = request.options.map_or(SkinningMethod::Linear, |o| o.method);
        let table = request
            .options
            .and_then(|o| o.influences8.as_ref())
            .filter(|table| {
                let fits = table.len() == vertex_count as usize;
                if !fits {
                    self.frame.influence_mismatch += 1;
                }
                fits
            });
        let mut palette = std::mem::take(&mut self.palette_scratch);
        let mut flags = palette_bytes_into(request.joints, method, &mut palette, &mut self.dq_scratch);
        if table.is_some() {
            flags |= FLAG_EIGHT;
        }
        let params = [vertex_count, flags, request.joints.len() as u32, 0];
        let dummy = self
            .dummy
            .get_or_insert_with(|| {
                make_buffer(
                    d,
                    "skin compute no influences",
                    size_of::<SkinInfluences8>() as u64,
                    wgpu::BufferUsages::STORAGE,
                )
            })
            .clone();

        let twin = match self.twins.entry(key) {
            std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let out = |i: usize| {
                    make_buffer(
                        d,
                        if i == 0 {
                            "skin compute out 0"
                        } else {
                            "skin compute out 1"
                        },
                        vertex_bytes as u64,
                        OUT_USAGE,
                    )
                };
                slot.insert(Twin {
                    source_key: request.mesh_key,
                    vertex_count,
                    out: [out(0), out(1)],
                    current: 0,
                    palette: make_buffer(
                        d,
                        "skin compute palette",
                        palette.len() as u64,
                        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    ),
                    palette_bytes: Vec::new(),
                    params: make_buffer(
                        d,
                        "skin compute params",
                        16,
                        wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    ),
                    params_value: [u32::MAX; 4],
                    influences: None,
                    groups: None,
                    source_seen: 0,
                    settled: 0,
                    touched: 0,
                    prev_valid: false,
                })
            }
        };
        let mut changed = twin.source_seen != source_generation;
        twin.source_seen = source_generation;
        // New output buffers hold no earlier pose.
        let mut fresh = twin.touched == 0;
        if twin.vertex_count != vertex_count {
            fresh = true;
            for (i, out) in twin.out.iter_mut().enumerate() {
                *out = make_buffer(
                    d,
                    if i == 0 {
                        "skin compute out 0"
                    } else {
                        "skin compute out 1"
                    },
                    vertex_bytes as u64,
                    OUT_USAGE,
                );
            }
            twin.vertex_count = vertex_count;
            twin.groups = None;
            twin.settled = 0;
            changed = true;
        }
        if (twin.palette.size() as usize) < palette.len() {
            twin.palette = make_buffer(
                d,
                "skin compute palette",
                palette.len() as u64,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            );
            twin.palette_bytes.clear();
            twin.groups = None;
        }
        if twin.palette_bytes != palette {
            q.write_buffer(&twin.palette, 0, &palette);
            // The twin keeps the new bytes; its old buffer is the next instance's scratch.
            std::mem::swap(&mut twin.palette_bytes, &mut palette);
            changed = true;
        }
        self.palette_scratch = palette;
        if twin.params_value != params {
            q.write_buffer(&twin.params, 0, bytemuck::cast_slice(&params));
            twin.params_value = params;
            changed = true;
        }
        let table_addr = table.map(|t| Arc::as_ptr(t).cast::<u8>() as usize);
        if twin.influences.as_ref().map(|i| i.0) != table_addr {
            twin.influences = table.map(|t| {
                let buf = make_buffer(
                    d,
                    "skin compute influences",
                    std::mem::size_of_val(&**t) as u64,
                    wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                );
                q.write_buffer(&buf, 0, bytemuck::cast_slice(t));
                (Arc::as_ptr(t).cast::<u8>() as usize, buf)
            });
            twin.groups = None;
            changed = true;
        }
        if twin.groups.as_ref().is_none_or(|g| g.1 != source_buf_generation) {
            let tracked = crate::device::TrackedDevice::observe(d);
            let influences = twin.influences.as_ref().map_or(&dummy, |i| &i.1);
            let group = |out: &wgpu::Buffer| {
                tracked.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("skin compute"),
                    layout: &kernel.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: twin.params.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: source_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: influences.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: twin.palette.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: out.as_entire_binding(),
                        },
                    ],
                })
            };
            twin.groups = Some(([group(&twin.out[0]), group(&twin.out[1])], source_buf_generation));
        }
        twin.prev_valid = request.prev_ok && !fresh && twin.touched + 1 == serial;
        twin.touched = serial;
        if changed {
            twin.settled = 0;
        }
        // Two dispatches of the same inputs leave the pose in both buffers: nothing to do.
        if twin.settled < 2 {
            twin.current ^= 1;
            twin.settled += 1;
            self.pending
                .push((key, twin.current, vertex_count.div_ceil(WORKGROUP)));
            self.frame.dispatched += 1;
            self.frame.vertices += u64::from(vertex_count);
        }
        self.frame.instances += 1;
        if flags & FLAG_DQ != 0 {
            self.frame.dual_quaternion += 1;
        }
        if flags & FLAG_EIGHT != 0 {
            self.frame.eight_influences += 1;
        }
        Some((key, twin.out[twin.current].clone()))
    }

    /// Last frame's skinned vertices of twin `key` for the velocity pass: the other buffer when
    /// it holds last frame's pose, else this frame's (only the model matrix moves it).
    pub(crate) fn prev_vertices(&self, key: u64) -> Option<wgpu::Buffer> {
        let twin = self.twins.get(&key)?;
        let index = if twin.prev_valid {
            twin.current ^ 1
        } else {
            twin.current
        };
        Some(twin.out[index].clone())
    }

    /// Records this frame's dispatches (call after the frame's early uploads, before any pass)
    /// and publishes the frame's stats.
    pub(crate) fn encode(&mut self, encoder: &mut wgpu::CommandEncoder) {
        self.frame.resident = self.twins.len();
        self.stats = self.frame.clone();
        // Counted meshes' source copies first (ADR-7721): the dispatches below read them.
        for (src, dst, bytes) in self.seeds.drain(..) {
            encoder.copy_buffer_to_buffer(&src, 0, &dst, 0, bytes);
        }
        if self.pending.is_empty() {
            return;
        }
        let Some(Ok(kernel)) = self.kernel.as_ref() else {
            return;
        };
        let _span = tracing::info_span!("skin_compute", dispatches = self.pending.len()).entered();
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("skin compute"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&kernel.pipeline);
        for &(key, out, groups) in &self.pending {
            if let Some((bind, _)) = self.twins.get(&key).and_then(|t| t.groups.as_ref()) {
                pass.counted_set_bind_group(0, &bind[out], &[]);
                pass.counted_dispatch(groups, 1, 1);
            }
        }
        drop(pass);
        self.pending.clear();
    }

    /// The last frame's stats.
    pub(crate) fn stats(&self) -> SkinningStats {
        let mut stats = self.stats.clone();
        stats.compute_all = self.compute_all;
        stats.options = self.options.len();
        stats
    }
}

/// Skins `vertices` once with the compute kernel and reads the result back (tests, tools and
/// the agent protocol's checks): the vertices every pass would draw for an instance with this
/// palette and these options. Directions are unnormalised and joint weights zero, as the twin
/// buffers hold them.
///
/// # Errors
///
/// The device cannot run the kernel ([`SkinningStats::unsupported`]'s reasons), the kernel
/// failed to build, or the readback failed.
pub fn skin_on_gpu(
    device: &crate::RenderDevice,
    vertices: &[LabVertex],
    joints: &[JointMatrix],
    options: &SkinningOptions,
) -> Result<Vec<LabVertex>, RenderError> {
    let d = &device.device;
    let flags = device.adapter.get_downlevel_capabilities().flags;
    if let Some(reason) = unsupported_reason(flags, &d.limits()) {
        return Err(RenderError::Shader(format!(
            "compute skinning unavailable: {reason}"
        )));
    }
    if vertices.is_empty() {
        return Ok(Vec::new());
    }
    if joints.is_empty() {
        return Err(RenderError::Shader(
            "compute skinning needs a joint palette".into(),
        ));
    }
    let q = crate::device::TrackedQueue::observe(&device.queue);
    let mut state = SkinCompute::default();
    state.begin_frame();
    let request = TwinRequest {
        mesh_key: 1,
        object_key: 1,
        vertices,
        buffer: None,
        joints,
        options: Some(options),
        prev_ok: false,
        ephemeral: false,
    };
    let (_, out) = state.prepare(device, &q, &request).ok_or_else(|| {
        RenderError::Shader(match state.kernel.as_ref() {
            Some(Err(reason)) => format!("compute skinning unavailable: {reason}"),
            _ => "the mesh is too large for one compute skinning dispatch".into(),
        })
    })?;
    let bytes = std::mem::size_of_val(vertices) as u64;
    let readback = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("skin compute readback"),
        size: bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("skin compute once"),
    });
    state.encode(&mut encoder);
    encoder.copy_buffer_to_buffer(&out, 0, &readback, 0, bytes);
    q.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    crate::device::TrackedDevice::observe(d)
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|_| RenderError::Readback("map callback dropped".into()))?
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let mapped = readback
        .slice(..)
        .get_mapped_range()
        .map_err(|e| RenderError::Readback(e.to_string()))?;
    let out: Vec<LabVertex> = bytemuck::pod_collect_to_vec(&mapped);
    drop(mapped);
    readback.unmap();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twin_keys_carry_their_tag_and_differ_per_instance() {
        let a = twin_key(0x4D00_0000_0000_0010, 7);
        let b = twin_key(0x4D00_0000_0000_0010, 8);
        let c = twin_key(0x4D00_0000_0000_0018, 7);
        for key in [a, b, c] {
            assert_eq!(key >> 56, SKIN_TWIN_KEY_TAG >> 56);
            assert!(!crate::frame::is_shadow_lod_key(key));
        }
        assert!(a != b && a != c && b != c);
    }

    #[test]
    fn dual_quaternion_palettes_flag_scaled_joints() {
        let rigid = [JointMatrix::translation(1.0, 2.0, 3.0)];
        assert_eq!(palette_bytes(&rigid, SkinningMethod::DualQuaternion).1, FLAG_DQ);
        let scaled = [JointMatrix::from_mat4(regia_core::glam::Mat4::from_scale(
            regia_core::glam::Vec3::splat(2.0),
        ))];
        assert_eq!(
            palette_bytes(&scaled, SkinningMethod::DualQuaternion).1,
            FLAG_DQ | FLAG_DQ_SCALE
        );
        let (bytes, flags) = palette_bytes(&rigid, SkinningMethod::Linear);
        assert_eq!((bytes.len(), flags), (48, 0));
    }

    #[test]
    fn a_device_without_compute_is_reported() {
        let limits = wgpu::Limits::default();
        assert!(unsupported_reason(wgpu::DownlevelFlags::empty(), &limits).is_some());
        let few = wgpu::Limits {
            max_storage_buffers_per_shader_stage: 2,
            ..wgpu::Limits::default()
        };
        assert!(unsupported_reason(wgpu::DownlevelFlags::all(), &few).is_some());
        assert!(unsupported_reason(wgpu::DownlevelFlags::all(), &limits).is_none());
    }
}
