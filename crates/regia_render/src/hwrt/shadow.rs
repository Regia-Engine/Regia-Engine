// SPDX-License-Identifier: MPL-2.0
//! Directional shadow rays: the compute fallback, the ray-query pass, and the CPU reference
//! (ADR-515).
//!
//! All three answer the same question for a list of world-space sample points: is the light
//! visible along `light_dir` (1.0) or blocked (0.0)? [`trace_shadow_rays_cpu`] is the reference;
//! [`ShadowFallbackPass`] runs on any adapter with compute shaders; [`ShadowRayQueryPass`]
//! needs a device with `EXPERIMENTAL_RAY_QUERY`. These answer for a list of points; the frame's
//! screen-sized pass is [`super::sun_trace`].

use crate::frame_work::counted::*;
use bytemuck::{Pod, Zeroable};

use super::accel::AccelError;
use super::bvh::ShadowBvh;

/// WGSL of the compute fallback (`shaders/hwrt_shadow_fallback.wgsl` and the BVH walk).
pub const FALLBACK_SHADOW_WGSL: &str = concat!(
    include_str!("../../shaders/hwrt_shadow_fallback.wgsl"),
    include_str!("bvh_walk.wgsl")
);

/// WGSL of the ray-query pass (`shaders/hwrt_shadow_ray_query.wgsl`, `enable wgpu_ray_query`).
pub const RAY_QUERY_SHADOW_WGSL: &str = include_str!("../../shaders/hwrt_shadow_ray_query.wgsl");

/// Threads per workgroup of both shadow shaders.
pub const SHADOW_WORKGROUP_SIZE: u32 = 64;

/// Workgroups per dispatch dimension the passes use at most (the WebGPU default limit).
const MAX_GROUPS_PER_DIMENSION: u32 = 65_535;

/// One shadow-ray query: where the light is and which hit distances count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowParams {
    /// Unit direction from the surface toward the light.
    pub light_dir: [f32; 3],
    /// Hits closer than this are ignored (self-shadowing bias, world units).
    pub t_min: f32,
    /// Hits farther than this are ignored.
    pub t_max: f32,
}

impl ShadowParams {
    /// Default self-shadowing bias.
    pub const DEFAULT_T_MIN: f32 = 1e-3;
    /// Default ray length.
    pub const DEFAULT_T_MAX: f32 = 1e6;

    /// Rays toward `light_dir` (normalized here) with the default bias and length; `None` for
    /// a zero-length or non-finite direction.
    #[must_use]
    pub fn toward(light_dir: [f32; 3]) -> Option<Self> {
        let len = light_dir.iter().map(|c| c * c).sum::<f32>().sqrt();
        if !len.is_finite() || len <= 0.0 {
            return None;
        }
        Some(Self {
            light_dir: light_dir.map(|c| c / len),
            t_min: Self::DEFAULT_T_MIN,
            t_max: Self::DEFAULT_T_MAX,
        })
    }

    fn uniform(&self, sample_count: u32, node_count: u32) -> ShadowUniform {
        ShadowUniform {
            light_dir: self.light_dir,
            t_min: self.t_min,
            t_max: self.t_max,
            sample_count,
            node_count,
            pad: 0,
        }
    }
}

/// The WGSL `Params` uniform (32 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct ShadowUniform {
    light_dir: [f32; 3],
    t_min: f32,
    t_max: f32,
    sample_count: u32,
    node_count: u32,
    pad: u32,
}

/// Visibility (1.0 lit, 0.0 shadowed) of each of `origins` toward `params.light_dir`, traced
/// on the CPU through `bvh`. The reference for both GPU passes.
#[must_use]
pub fn trace_shadow_rays_cpu(bvh: &ShadowBvh, params: &ShadowParams, origins: &[[f32; 3]]) -> Vec<f32> {
    origins
        .iter()
        .map(|&o| {
            if bvh.occluded(o, params.light_dir, params.t_min, params.t_max) {
                0.0
            } else {
                1.0
            }
        })
        .collect()
}

/// Workgroups `(x, y)` for `sample_count` samples: `x` stays within the per-dimension limit
/// and the shaders fold `y` back into one sample index.
#[must_use]
pub fn shadow_dispatch_size(sample_count: u32) -> (u32, u32) {
    let groups = sample_count.div_ceil(SHADOW_WORKGROUP_SIZE);
    if groups <= MAX_GROUPS_PER_DIMENSION {
        (groups, 1)
    } else {
        (
            MAX_GROUPS_PER_DIMENSION,
            groups.div_ceil(MAX_GROUPS_PER_DIMENSION),
        )
    }
}

/// `contents` in a new buffer, counted by the hitch counters ([`crate::TrackedDevice`]): the
/// same padding and mapping as `wgpu::util::DeviceExt::create_buffer_init`, which creates on the
/// raw device.
fn buffer_init(
    device: &wgpu::Device,
    label: &str,
    contents: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    let device = crate::TrackedDevice::observe(device);
    if contents.is_empty() {
        return device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: 0,
            usage,
            mapped_at_creation: false,
        });
    }
    let align = wgpu::COPY_BUFFER_ALIGNMENT;
    let size = (contents.len() as u64).div_ceil(align).max(1) * align;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage,
        mapped_at_creation: true,
    });
    match buffer.slice(..).get_mapped_range_mut() {
        Ok(mut view) => view.slice(0..contents.len()).copy_from_slice(contents),
        Err(err) => tracing::warn!(target: "regia_render", "{label}: mapping the new buffer failed: {err}"),
    }
    buffer.unmap();
    buffer
}

/// Trace groups a shadow pass keeps: a group binds the uniform (its bytes are the key) and the
/// buffers it traces, so tracing the same samples toward the same light again (a held sun, every
/// frame) creates nothing (ADR-2585; one uniform and one group per trace before, on a raw device).
const TRACE_GROUPS_KEPT: usize = 4;

/// One kept trace group.
#[derive(Debug)]
struct TraceGroup {
    /// The uniform's bytes.
    uniform: [u8; 32],
    /// The buffers bound after the uniform, in binding order.
    buffers: Vec<wgpu::Buffer>,
    /// Hash of the acceleration structure bound (0 for the compute fallback). The group holds the
    /// structure, so a live entry's id cannot be reused by another one.
    accel: u64,
    /// The group (it holds its uniform buffer).
    group: wgpu::BindGroup,
}

/// The last [`TRACE_GROUPS_KEPT`] trace groups of one pass, newest last.
#[derive(Debug, Default)]
struct TraceGroups(std::sync::Mutex<Vec<TraceGroup>>);

impl TraceGroups {
    /// The kept group for `(uniform, buffers, accel)`, or `make(uniform buffer)`'s, kept.
    fn get_or_make(
        &self,
        device: &wgpu::Device,
        label: &str,
        uniform: [u8; 32],
        buffers: &[&wgpu::Buffer],
        accel: u64,
        make: impl FnOnce(&crate::TrackedDevice, &wgpu::Buffer) -> wgpu::BindGroup,
    ) -> wgpu::BindGroup {
        let same = |entry: &TraceGroup| {
            entry.uniform == uniform
                && entry.accel == accel
                && entry.buffers.len() == buffers.len()
                && entry.buffers.iter().zip(buffers).all(|(a, b)| a == *b)
        };
        let mut kept = self.0.lock().ok();
        if let Some(kept) = kept.as_ref()
            && let Some(entry) = kept.iter().find(|&entry| same(entry))
        {
            return entry.group.clone();
        }
        let buffer = buffer_init(device, label, &uniform, wgpu::BufferUsages::UNIFORM);
        let group = make(&crate::TrackedDevice::observe(device), &buffer);
        if let Some(kept) = kept.as_mut() {
            if kept.len() >= TRACE_GROUPS_KEPT {
                kept.remove(0);
            }
            kept.push(TraceGroup {
                uniform,
                buffers: buffers.iter().map(|b| (*b).clone()).collect(),
                accel,
                group: group.clone(),
            });
        }
        group
    }
}

/// A storage buffer of sample points for either pass (`vec4<f32>` each, `w` = 0).
#[must_use]
pub fn upload_shadow_origins(device: &wgpu::Device, origins: &[[f32; 3]]) -> wgpu::Buffer {
    let mut data: Vec<[f32; 4]> = origins.iter().map(|o| [o[0], o[1], o[2], 0.0]).collect();
    if data.is_empty() {
        data.push([0.0; 4]);
    }
    buffer_init(
        device,
        "hwrt shadow origins",
        bytemuck::cast_slice(&data),
        wgpu::BufferUsages::STORAGE,
    )
}

/// A visibility buffer for `sample_count` samples (`f32` each), readable with a copy.
#[must_use]
pub fn create_shadow_visibility(device: &wgpu::Device, sample_count: u32) -> wgpu::Buffer {
    crate::TrackedDevice::observe(device).create_buffer(&wgpu::BufferDescriptor {
        label: Some("hwrt shadow visibility"),
        size: u64::from(sample_count.max(1)) * 4,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

/// The sample points one trace reads and the visibility it writes.
#[derive(Debug, Clone, Copy)]
pub struct ShadowTargets<'a> {
    /// Sample points ([`upload_shadow_origins`]).
    pub origins: &'a wgpu::Buffer,
    /// One `f32` per sample, 1.0 lit, 0.0 shadowed ([`create_shadow_visibility`]).
    pub visibility: &'a wgpu::Buffer,
    /// Samples to trace (at most the origins uploaded).
    pub sample_count: u32,
}

/// A [`ShadowBvh`] uploaded for [`ShadowFallbackPass`].
#[derive(Debug)]
pub struct ShadowFallbackScene {
    nodes: wgpu::Buffer,
    triangles: wgpu::Buffer,
    node_count: u32,
}

impl ShadowFallbackScene {
    /// Uploads `bvh`'s nodes and triangles. Both buffers accept a later in-place refit.
    #[must_use]
    pub fn upload(device: &wgpu::Device, bvh: &ShadowBvh) -> Self {
        let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let nodes = buffer_init(device, "hwrt bvh nodes", bytemuck::cast_slice(bvh.nodes()), usage);
        let triangles = buffer_init(
            device,
            "hwrt bvh triangles",
            bytemuck::cast_slice(bvh.triangles()),
            usage,
        );
        Self {
            nodes,
            triangles,
            node_count: bvh.nodes().len() as u32,
        }
    }

    /// Writes `bvh` into the buffers already uploaded when they are large enough.
    ///
    /// Returns whether the bytes landed in those buffers. A tree that does not fit returns
    /// `false` and leaves the buffers alone.
    pub(crate) fn refit(&mut self, queue: &crate::device::TrackedQueue, bvh: &ShadowBvh) -> bool {
        let nodes: &[u8] = bytemuck::cast_slice(bvh.nodes());
        let tris: &[u8] = bytemuck::cast_slice(bvh.triangles());
        if self.nodes.size() < nodes.len() as u64 || self.triangles.size() < tris.len() as u64 {
            return false;
        }
        if !nodes.is_empty() {
            queue.write_buffer(&self.nodes, 0, nodes);
        }
        if !tris.is_empty() {
            queue.write_buffer(&self.triangles, 0, tris);
        }
        self.node_count = bvh.nodes().len() as u32;
        true
    }

    /// Installs `bvh` into `slot`. A tree that fits the buffers already there is written in
    /// place and `epoch` stays. Anything else uploads new buffers and advances `epoch`.
    pub(crate) fn install(
        slot: &mut Option<Self>,
        epoch: &mut u64,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        bvh: &ShadowBvh,
    ) {
        let reused = slot.as_mut().is_some_and(|scene| scene.refit(queue, bvh));
        if !reused {
            *slot = Some(Self::upload(device, bvh));
            *epoch = epoch.wrapping_add(1);
        }
    }

    /// Nodes in the uploaded tree.
    #[must_use]
    pub fn node_count(&self) -> u32 {
        self.node_count
    }

    /// The storage buffers `(nodes, triangles)`.
    pub(crate) fn buffers(&self) -> (&wgpu::Buffer, &wgpu::Buffer) {
        (&self.nodes, &self.triangles)
    }
}

fn compute_entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    }
}

fn buffer_entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    compute_entry(
        binding,
        wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

fn shadow_pipeline(
    device: &wgpu::Device,
    label: &str,
    source: &'static str,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    crate::TrackedDevice::observe(device).create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("trace_shadows"),
        compilation_options: Default::default(),
        cache: crate::pipeline_cache::cache_for(device).as_deref(),
    })
}

fn dispatch(
    encoder: &mut wgpu::CommandEncoder,
    label: &str,
    pipeline: &wgpu::ComputePipeline,
    bind_group: &wgpu::BindGroup,
    sample_count: u32,
) {
    let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some(label),
        timestamp_writes: None,
    });
    pass.counted_set_pipeline(pipeline);
    pass.counted_set_bind_group(0, bind_group, &[]);
    let (x, y) = shadow_dispatch_size(sample_count);
    pass.counted_dispatch(x, y, 1);
}

/// Shadow rays through a [`ShadowBvh`] in storage buffers: plain compute, every adapter.
#[derive(Debug)]
pub struct ShadowFallbackPass {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
    groups: TraceGroups,
}

impl ShadowFallbackPass {
    /// Compiles the pass.
    #[must_use]
    pub fn new(device: &wgpu::Device) -> Self {
        let read = wgpu::BufferBindingType::Storage { read_only: true };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hwrt shadow fallback"),
            entries: &[
                buffer_entry(0, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, read),
                buffer_entry(2, read),
                buffer_entry(3, read),
                buffer_entry(4, wgpu::BufferBindingType::Storage { read_only: false }),
            ],
        });
        let pipeline = shadow_pipeline(device, "hwrt shadow fallback", FALLBACK_SHADOW_WGSL, &layout);
        Self {
            layout,
            pipeline,
            groups: TraceGroups::default(),
        }
    }

    /// Records the trace of `targets` through `scene`. Nothing is recorded for zero samples.
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        scene: &ShadowFallbackScene,
        params: &ShadowParams,
        targets: &ShadowTargets<'_>,
    ) {
        let _span = tracing::info_span!("hwrt_shadow_fallback").entered();
        let _crumb = crate::breadcrumbs::frame_pass("hwrt_shadow_fallback");
        let ShadowTargets {
            origins,
            visibility,
            sample_count,
        } = *targets;
        if sample_count == 0 {
            return;
        }
        let uniform = bytemuck::cast(params.uniform(sample_count, scene.node_count));
        let bind_group = self.groups.get_or_make(
            device,
            "hwrt shadow fallback params",
            uniform,
            &[&scene.nodes, &scene.triangles, origins, visibility],
            0,
            |device, uniform| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("hwrt shadow fallback"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: scene.nodes.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: scene.triangles.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: origins.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: visibility.as_entire_binding(),
                        },
                    ],
                })
            },
        );
        dispatch(
            encoder,
            "hwrt shadow fallback",
            &self.pipeline,
            &bind_group,
            sample_count,
        );
    }
}

/// Shadow rays with hardware ray queries against a TLAS ([`super::GpuAccel::tlas`]).
#[derive(Debug)]
pub struct ShadowRayQueryPass {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
    groups: TraceGroups,
}

impl ShadowRayQueryPass {
    /// Compiles the pass; [`AccelError::RayQueryDisabled`] unless `device` has
    /// `EXPERIMENTAL_RAY_QUERY`.
    pub fn new(device: &wgpu::Device) -> Result<Self, AccelError> {
        if !device.features().contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY) {
            return Err(AccelError::RayQueryDisabled);
        }
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hwrt shadow ray query"),
            entries: &[
                buffer_entry(0, wgpu::BufferBindingType::Uniform),
                compute_entry(
                    1,
                    wgpu::BindingType::AccelerationStructure { vertex_return: false },
                ),
                buffer_entry(2, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(3, wgpu::BufferBindingType::Storage { read_only: false }),
            ],
        });
        let pipeline = shadow_pipeline(device, "hwrt shadow ray query", RAY_QUERY_SHADOW_WGSL, &layout);
        Ok(Self {
            layout,
            pipeline,
            groups: TraceGroups::default(),
        })
    }

    /// Records the trace of `targets` against `tlas`. The TLAS build must be recorded first
    /// (same or earlier submission). Nothing is recorded for zero samples.
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        tlas: &wgpu::Tlas,
        params: &ShadowParams,
        targets: &ShadowTargets<'_>,
    ) {
        let _span = tracing::info_span!("hwrt_shadow_ray_query").entered();
        let _crumb = crate::breadcrumbs::frame_pass("hwrt_shadow_ray_query");
        let ShadowTargets {
            origins,
            visibility,
            sample_count,
        } = *targets;
        if sample_count == 0 {
            return;
        }
        let uniform = bytemuck::cast(params.uniform(sample_count, 0));
        let accel = {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            tlas.hash(&mut hasher);
            hasher.finish().max(1)
        };
        let bind_group = self.groups.get_or_make(
            device,
            "hwrt shadow ray query params",
            uniform,
            &[origins, visibility],
            accel,
            |device, uniform| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("hwrt shadow ray query"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: tlas.as_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: origins.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: visibility.as_entire_binding(),
                        },
                    ],
                })
            },
        );
        dispatch(
            encoder,
            "hwrt shadow ray query",
            &self.pipeline,
            &bind_group,
            sample_count,
        );
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "hwrt_shadow_fallback.wgsl,hwrt_shadow_ray_query.wgsl",
        "Params",
        ShadowUniform {
            light_dir,
            t_min,
            t_max,
            sample_count,
            node_count,
            pad
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(source: &str, caps: naga::valid::Capabilities) -> Result<naga::Module, String> {
        let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), caps)
            .validate(&module)
            .map_err(|e| format!("{e:?}"))?;
        Ok(module)
    }

    #[test]
    fn the_fallback_shader_needs_no_ray_query_capability() {
        let module = validate(FALLBACK_SHADOW_WGSL, naga::valid::Capabilities::empty()).expect("fallback");
        assert!(module.entry_points.iter().any(|e| e.name == "trace_shadows"));
    }

    #[test]
    fn the_ray_query_shader_validates_with_the_capability_and_only_with_it() {
        let module =
            validate(RAY_QUERY_SHADOW_WGSL, naga::valid::Capabilities::RAY_QUERY).expect("ray query shader");
        assert!(module.entry_points.iter().any(|e| e.name == "trace_shadows"));
        assert!(
            validate(RAY_QUERY_SHADOW_WGSL, naga::valid::Capabilities::default()).is_err(),
            "without RAY_QUERY the shader must be refused, or the capability gate is not doing anything"
        );
    }

    #[test]
    fn both_shaders_share_the_params_layout() {
        // The Rust uniform and both WGSL `Params` structs are 32 bytes in the same order.
        assert_eq!(std::mem::size_of::<ShadowUniform>(), 32);
        let fields = "light_dir: vec3<f32>,\n    t_min: f32,\n    t_max: f32,\n    sample_count: u32,\n    node_count: u32,\n    pad: u32,";
        assert!(FALLBACK_SHADOW_WGSL.replace("\r\n", "\n").contains(fields));
        assert!(RAY_QUERY_SHADOW_WGSL.replace("\r\n", "\n").contains(fields));
    }

    #[test]
    fn dispatch_size_covers_every_sample_within_the_dimension_limit() {
        assert_eq!(shadow_dispatch_size(0), (0, 1));
        assert_eq!(shadow_dispatch_size(1), (1, 1));
        assert_eq!(shadow_dispatch_size(64), (1, 1));
        assert_eq!(shadow_dispatch_size(65), (2, 1));
        for count in [65_535 * 64, 65_535 * 64 + 1, 10_000_000, u32::MAX] {
            let (x, y) = shadow_dispatch_size(count);
            assert!(x <= MAX_GROUPS_PER_DIMENSION && y <= MAX_GROUPS_PER_DIMENSION);
            assert!(u64::from(x) * u64::from(y) * 64 >= u64::from(count), "{count}");
        }
    }

    #[test]
    fn params_normalize_and_refuse_a_degenerate_direction() {
        let p = ShadowParams::toward([0.0, 3.0, 4.0]).expect("params");
        assert!((p.light_dir[1] - 0.6).abs() < 1e-6 && (p.light_dir[2] - 0.8).abs() < 1e-6);
        assert_eq!(p.t_min, ShadowParams::DEFAULT_T_MIN);
        assert!(ShadowParams::toward([0.0; 3]).is_none());
        assert!(ShadowParams::toward([f32::NAN, 1.0, 0.0]).is_none());
    }

    #[test]
    fn the_cpu_reference_shadows_under_an_occluder() {
        let quad = [
            [[-1.0, 2.0, -1.0], [1.0, 2.0, -1.0], [1.0, 2.0, 1.0]],
            [[-1.0, 2.0, -1.0], [1.0, 2.0, 1.0], [-1.0, 2.0, 1.0]],
        ];
        let bvh = ShadowBvh::build(&quad).expect("bvh");
        let up = ShadowParams::toward([0.0, 1.0, 0.0]).expect("params");
        let vis = trace_shadow_rays_cpu(&bvh, &up, &[[0.0, 0.0, 0.0], [3.0, 0.0, 0.0], [0.0, 3.0, 0.0]]);
        assert_eq!(vis, [0.0, 1.0, 1.0], "under, beside, above");
    }

    /// A translated caster keeps the same tree shape, so the compute fallback writes the new
    /// nodes into the buffers it already uploaded and leaves the scene epoch alone. A larger
    /// tree still allocates.
    #[test]
    fn a_compute_refit_reuses_the_bvh_buffers() {
        let device = crate::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let grid = |n: i32, shift: f32| {
            let mut tris = Vec::new();
            for x in 0..n {
                for z in 0..n {
                    let x0 = x as f32 + shift;
                    let z0 = z as f32;
                    tris.push([[x0, 0.0, z0], [x0 + 1.0, 0.0, z0], [x0 + 1.0, 0.0, z0 + 1.0]]);
                }
            }
            ShadowBvh::build(&tris).expect("bvh")
        };
        let bvh1 = grid(2, 0.0);
        let mut slot = None;
        let mut epoch = 0u64;
        ShadowFallbackScene::install(&mut slot, &mut epoch, &device.device, &queue, &bvh1);
        let scene = slot.as_ref().expect("uploaded");
        assert_eq!(epoch, 1);
        assert_eq!(scene.node_count(), bvh1.nodes().len() as u32);
        let (n1, t1) = scene.buffers();
        let n1 = n1.clone();
        let t1 = t1.clone();

        let bvh2 = grid(2, 5.0);
        assert_eq!(bvh1.nodes().len(), bvh2.nodes().len());
        assert_eq!(bvh1.triangles().len(), bvh2.triangles().len());
        ShadowFallbackScene::install(&mut slot, &mut epoch, &device.device, &queue, &bvh2);
        let scene = slot.as_ref().expect("refit");
        let (n2, t2) = scene.buffers();
        assert!(
            n2 == &n1 && t2 == &t1 && epoch == 1,
            "node buffer, triangle buffer and scene_epoch stay on a fitting refit (epoch {epoch})"
        );
        assert_eq!(scene.node_count(), bvh2.nodes().len() as u32);

        let bvh3 = grid(4, 0.0);
        assert!(bvh3.nodes().len() > bvh1.nodes().len());
        ShadowFallbackScene::install(&mut slot, &mut epoch, &device.device, &queue, &bvh3);
        let scene = slot.as_ref().expect("grown");
        let (n3, t3) = scene.buffers();
        assert!(
            n3 != &n1 && t3 != &t1 && epoch == 2,
            "a tree that does not fit allocates new buffers and advances the epoch (epoch {epoch})"
        );
        assert_eq!(scene.node_count(), bvh3.nodes().len() as u32);
    }
}
