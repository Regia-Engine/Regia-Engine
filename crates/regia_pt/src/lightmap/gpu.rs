// SPDX-License-Identifier: MPL-2.0
//! The GPU lightmap baker (feature `lightmap-gpu`, ADR-2024): the estimate of
//! [`LightmapScene::texel_irradiance`] as a compute shader (`shaders/lightmap_bake.wgsl`), with the
//! same task list before it and the same filter, gutters and encoding after it
//! ([`super::prepare_tasks`], [`super::finish_bake`]).
//!
//! The shader draws the same random numbers in the same order from the same per-texel streams, so
//! a texel takes the same paths as on the CPU, and the result differs only by float rounding
//! (`tests/lightmap_gpu.rs` holds it to a tolerance on the bake's own texels). The flat scene is
//! uploaded as storage buffers. Work is cut into memory chunks that fit the device and the memory
//! budget ([`plan_gpu_chunk`]) and each chunk into tiles of at most `TASKS_PER_DISPATCH` texels,
//! `SAMPLES_PER_DISPATCH` samples and about [`LightmapGpuOptions::tile_rays`] rays (wide first:
//! texels side by side fill the GPU, samples stack only when texels run out), one queued behind
//! the other and each waited for, so a long bake never holds one long command buffer and hands the
//! GPU back between tiles (ADR-2125); the cut does not change the result, because a texel's sums
//! always add sample 0, 1, 2, ... in order.

use std::borrow::Cow;
use std::time::Duration;

use super::{
    LightmapBakeReport, LightmapBakeSettings, LightmapScene, Prepared, Task, TexelEstimate, finish_bake,
    prepare_tasks, relative_error,
};
use crate::{PtError, PtLight};
use bytemuck::{Pod, Zeroable};
use regia_asset::lightmap::{Lightmap, LightmapLayout};
use regia_core::glam::Vec3;
use regia_render::RenderDevice;

const SOURCE: &str = include_str!("../../shaders/lightmap_bake.wgsl");
/// Most texels one dispatch estimates: enough invocations to fill a large GPU (a texel is one
/// invocation, its samples a loop).
const TASKS_PER_DISPATCH: u32 = 1 << 18;
/// Most samples one dispatch takes per texel.
const SAMPLES_PER_DISPATCH: u32 = 16;
/// Invocations per workgroup (`@workgroup_size` in the shader).
const WORKGROUP: u32 = 32;
/// Deepest BVH the shader's fixed traversal stack holds (its `STACK`, less the two a node pushes).
const MAX_BVH_DEPTH: usize = 90;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ParamsGpu {
    first_task: u32,
    task_count: u32,
    first_sample: u32,
    sample_count: u32,
    seed: u32,
    bounces: u32,
    direct_sun: u32,
    has_sky: u32,
    sky: [f32; 4],
    sun_toward: [f32; 4],
    sun_light: [f32; 4],
    light_count: u32,
    emitter_count: u32,
    emitter_total: f32,
    sun_lit: u32,
    /// The frame's extra directional lights (ADR-7254): xyz toward each, w = 1 when on.
    extra_toward: [[f32; 4]; regia_render::MAX_EXTRA_DIRECTIONAL],
    /// rgb illuminance × color, w = cos of the angular radius (2 for a delta light).
    extra_light: [[f32; 4]; regia_render::MAX_EXTRA_DIRECTIONAL],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TriGpu {
    p: [[f32; 4]; 3],
    n: [[f32; 4]; 3],
    face: [f32; 4],
    albedo: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct EmitterGpu {
    radiance: [f32; 3],
    tri: u32,
    area: f32,
    two_sided: u32,
    cdf: f32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LightGpu {
    kind: u32,
    inner_cos: f32,
    outer_cos: f32,
    radius: f32,
    position: [f32; 4],
    direction: [f32; 4],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TaskGpu {
    p: [f32; 3],
    key: u32,
    n: [f32; 4],
    face: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct AccumGpu {
    sum: [f32; 3],
    lum_sum: f32,
    dir: [f32; 3],
    dir_weight: f32,
    lum_sq: f32,
    back: u32,
    taken: u32,
    rays: u32,
}

fn vec4(v: [f32; 3], w: f32) -> [f32; 4] {
    [v[0], v[1], v[2], w]
}

fn tri_gpu(t: &super::Tri) -> TriGpu {
    TriGpu {
        p: t.p.map(|v| vec4(v.to_array(), 0.0)),
        n: t.n.map(|v| vec4(v.to_array(), 0.0)),
        face: vec4(t.face.to_array(), 0.0),
        albedo: vec4(t.albedo.to_array(), 0.0),
    }
}

fn light_gpu(light: &PtLight) -> Option<LightGpu> {
    let base = LightGpu {
        kind: 0,
        inner_cos: 0.0,
        outer_cos: 0.0,
        radius: 0.0,
        position: [0.0; 4],
        direction: [0.0; 4],
        color: [0.0; 4],
    };
    match *light {
        PtLight::Point { position, intensity } => Some(LightGpu {
            position: vec4(position, 0.0),
            color: vec4(intensity, 0.0),
            ..base
        }),
        PtLight::Spot {
            position,
            direction,
            intensity,
            inner_cos,
            outer_cos,
        } => Some(LightGpu {
            kind: 1,
            inner_cos,
            outer_cos,
            position: vec4(position, 0.0),
            direction: vec4(direction, 0.0),
            color: vec4(intensity, 0.0),
            ..base
        }),
        PtLight::Sphere {
            position,
            radius,
            radiance,
        } => Some(LightGpu {
            kind: 2,
            radius,
            position: vec4(position, 0.0),
            color: vec4(radiance, 0.0),
            ..base
        }),
        // A sized spot bakes as the delta spot of the same intensity (ADR-2592).
        PtLight::SphereSpot {
            position,
            radius,
            radiance,
            direction,
            inner_cos,
            outer_cos,
        } => Some(LightGpu {
            kind: 1,
            inner_cos,
            outer_cos,
            position: vec4(position, 0.0),
            direction: vec4(direction, 0.0),
            color: vec4(radiance.map(|c| c * std::f32::consts::PI * radius * radius), 0.0),
            ..base
        }),
        // `from_pt_scene` turned every quad into emitter triangles.
        PtLight::Quad { .. } => None,
    }
}

fn params(scene: &LightmapScene, settings: &LightmapBakeSettings) -> ParamsGpu {
    let sun = scene.sun;
    let toward = sun.map_or(Vec3::ZERO, |s| Vec3::from_array(s.toward).normalize_or_zero());
    let lights = scene.lights.iter().filter_map(light_gpu).count();
    ParamsGpu {
        first_task: 0,
        task_count: 0,
        first_sample: 0,
        sample_count: 0,
        seed: settings.seed,
        bounces: settings.bounces,
        direct_sun: u32::from(settings.direct_sun),
        has_sky: u32::from(scene.has_sky),
        sky: vec4(scene.sky.to_array(), 0.0),
        sun_toward: vec4(toward.to_array(), if sun.is_some() { 1.0 } else { 0.0 }),
        sun_light: vec4(
            sun.map_or([0.0; 3], |s| {
                (Vec3::from_array(s.color) * s.illuminance).to_array()
            }),
            sun.filter(|s| s.angular_radius > 0.0)
                .map_or(2.0, |s| s.angular_radius.cos()),
        ),
        light_count: lights as u32,
        emitter_count: scene.emitters.len() as u32,
        emitter_total: scene.emitter_cdf.last().copied().unwrap_or(0.0),
        sun_lit: u32::from(sun.is_some_and(|s| s.illuminance > 0.0)),
        extra_toward: std::array::from_fn(|i| {
            scene.extra_suns.get(i).map_or([0.0; 4], |s| {
                vec4(Vec3::from_array(s.toward).normalize_or_zero().to_array(), 1.0)
            })
        }),
        extra_light: std::array::from_fn(|i| {
            scene.extra_suns.get(i).map_or([0.0; 4], |s| {
                vec4(
                    (Vec3::from_array(s.color) * s.illuminance).to_array(),
                    if s.angular_radius > 0.0 {
                        s.angular_radius.cos()
                    } else {
                        2.0
                    },
                )
            })
        }),
    }
}

/// The deepest path in the flattened BVH.
fn bvh_depth(scene: &LightmapScene) -> usize {
    let nodes = &scene.bvh.nodes;
    let mut deepest = 0;
    let mut stack = vec![(0u32, 1usize)];
    while let Some((at, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        let Some(node) = nodes.get(at as usize) else {
            continue;
        };
        if !node.is_leaf() && !scene.tris.is_empty() {
            stack.push((node.a, depth + 1));
            stack.push((node.a + 1, depth + 1));
        }
    }
    deepest
}

/// The most texels and samples of one tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TileShape {
    /// Most texels.
    tasks: u32,
    /// Samples per texel (the last block of a range may take fewer).
    samples: u32,
}

/// The shape of a tile at `bounces` and `lights` within about `tile_rays` rays, for a chunk of
/// `texels` texels (a long dispatch can trip the system's GPU watchdog and lose the device, and
/// holds the GPU from other work): every path vertex traces its bounce, a sun shadow ray, an
/// emitter shadow ray and one shadow ray per light. A tile is wide (many texels, as many
/// invocations as the GPU can run side by side) before it is deep (more samples per texel). How
/// the work is cut does not change the result: each texel's random stream is its own, and its
/// samples add in order.
fn tile_shape(bounces: u32, lights: u32, tile_rays: u64, texels: usize) -> TileShape {
    let per_path = u64::from(bounces.max(1)).saturating_mul(3 + u64::from(lights));
    let texel_samples = (tile_rays / per_path).max(u64::from(WORKGROUP));
    let tasks = texel_samples
        .min(u64::from(TASKS_PER_DISPATCH))
        .min((texels as u64).max(1))
        .max(1);
    TileShape {
        tasks: tasks as u32,
        samples: (texel_samples / tasks).clamp(1, u64::from(SAMPLES_PER_DISPATCH)) as u32,
    }
}

/// Runs `f` with the device's errors captured. A headless device has no error handler of its own and
/// wgpu's default one panics, which would end the CLI with no JSON; here a device that runs out of
/// memory or refuses a call is an error of the bake.
fn scoped<T>(device: &RenderDevice, f: impl FnOnce() -> Result<T, PtError>) -> Result<T, PtError> {
    let d = &device.device;
    let memory = d.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let validation = d.push_error_scope(wgpu::ErrorFilter::Validation);
    let result = f();
    let invalid = regia_core::tasks::block_on(validation.pop());
    let out_of_memory = regia_core::tasks::block_on(memory.pop());
    if let Some(err) = out_of_memory {
        return Err(PtError::Gpu(format!(
            "lightmap_gpu_limit: the device ran out of memory ({err}): bake on the CPU"
        )));
    }
    if let Some(err) = invalid {
        return Err(PtError::Gpu(err.to_string()));
    }
    result
}

/// Refuses a scene whose largest buffer the device cannot hold or bind.
fn check_fit(largest: u64, binding_limit: u64, buffer_limit: u64) -> Result<(), PtError> {
    let limit = binding_limit.min(buffer_limit);
    if largest > limit {
        return Err(PtError::Gpu(format!(
            "lightmap_gpu_limit: the scene needs a {largest} byte storage buffer and the device allows {limit}: bake on the CPU"
        )));
    }
    Ok(())
}

/// A buffer holding `items`, with one zeroed element when there are none (a binding cannot be
/// empty). Not mapped at creation: a buffer the device refuses reports through [`scoped`].
fn storage<T: Pod + Zeroable>(device: &RenderDevice, label: &str, items: &[T]) -> wgpu::Buffer {
    let filler = [T::zeroed()];
    let data: &[T] = if items.is_empty() { &filler } else { items };
    let bytes: &[u8] = bytemuck::cast_slice(data);
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    device.queue.write_buffer(&buffer, 0, bytes);
    buffer
}

fn create_pipeline(device: &RenderDevice) -> Result<(wgpu::ComputePipeline, wgpu::BindGroupLayout), PtError> {
    let module = regia_render::compose_source(SOURCE, "lightmap_bake.wgsl", std::collections::HashMap::new())
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    let d = &device.device;
    let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
    let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("lightmap bake"),
        source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
    });
    let entry = |binding: u32, ty: wgpu::BufferBindingType| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let read = wgpu::BufferBindingType::Storage { read_only: true };
    let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("lightmap bake"),
        entries: &[
            entry(0, wgpu::BufferBindingType::Uniform),
            entry(1, read),
            entry(2, read),
            entry(3, read),
            entry(4, read),
            entry(5, read),
            entry(6, read),
            entry(7, wgpu::BufferBindingType::Storage { read_only: false }),
        ],
    });
    let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("lightmap bake"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("lightmap bake"),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("bake"),
        // `bvh_stack` is the only workgroup memory and every entry is written before it is read,
        // so the zero fill (and its barrier) at the kernel's start is skipped (ADR-7879).
        compilation_options: wgpu::PipelineCompilationOptions {
            zero_initialize_workgroup_memory: false,
            ..Default::default()
        },
        cache: None,
    });
    if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
        return Err(PtError::Gpu(err.to_string()));
    }
    Ok((pipeline, layout))
}

/// Bytes one texel of a chunk holds on the device: its task, its sums and the sums' readback copy.
const BYTES_PER_TEXEL: u64 = (size_of::<TaskGpu>() + 2 * size_of::<AccumGpu>()) as u64;

/// The scene's buffers, uploaded once per bake.
struct SceneBuffers {
    nodes: wgpu::Buffer,
    order: wgpu::Buffer,
    tris: wgpu::Buffer,
    emitters: wgpu::Buffer,
    lights: wgpu::Buffer,
    /// What they hold together, bytes.
    bytes: u64,
}

/// The per-texel buffers of one chunk, reused by every chunk.
struct ChunkBuffers {
    bind: wgpu::BindGroup,
    params: wgpu::Buffer,
    tasks: wgpu::Buffer,
    accum: wgpu::Buffer,
    staging: wgpu::Buffer,
}

fn upload_scene(device: &RenderDevice, scene: &LightmapScene) -> Result<SceneBuffers, PtError> {
    let limits = device.device.limits();
    let sizes = [
        size_of_val(scene.bvh.nodes.as_slice()),
        size_of_val(scene.bvh.order.as_slice()),
        scene.tris.len() * size_of::<TriGpu>(),
        scene.emitters.len() * size_of::<EmitterGpu>(),
        scene.lights.len() * size_of::<LightGpu>(),
    ];
    let largest = sizes.into_iter().max().unwrap_or(0) as u64;
    check_fit(
        largest,
        limits.max_storage_buffer_binding_size,
        limits.max_buffer_size,
    )?;
    let depth = bvh_depth(scene);
    if depth > MAX_BVH_DEPTH {
        return Err(PtError::Gpu(format!(
            "lightmap_gpu_limit: the scene's BVH is {depth} levels deep and the shader holds {MAX_BVH_DEPTH}: bake on the CPU"
        )));
    }
    let tris: Vec<TriGpu> = scene.tris.iter().map(tri_gpu).collect();
    let cdf = &scene.emitter_cdf;
    let emitters: Vec<EmitterGpu> = scene
        .emitters
        .iter()
        .zip(cdf)
        .map(|(e, &cdf)| EmitterGpu {
            radiance: e.radiance.to_array(),
            tri: e.tri as u32,
            area: e.area,
            two_sided: u32::from(e.two_sided),
            cdf,
            pad: 0,
        })
        .collect();
    let lights: Vec<LightGpu> = scene.lights.iter().filter_map(light_gpu).collect();
    let buffers = SceneBuffers {
        nodes: storage(device, "lightmap bake nodes", &scene.bvh.nodes),
        order: storage(device, "lightmap bake order", &scene.bvh.order),
        tris: storage(device, "lightmap bake tris", &tris),
        emitters: storage(device, "lightmap bake emitters", &emitters),
        lights: storage(device, "lightmap bake lights", &lights),
        bytes: 0,
    };
    let bytes = [
        &buffers.nodes,
        &buffers.order,
        &buffers.tris,
        &buffers.emitters,
        &buffers.lights,
    ]
    .iter()
    .map(|b| b.size())
    .sum::<u64>()
        + size_of::<ParamsGpu>() as u64;
    Ok(SceneBuffers { bytes, ..buffers })
}

fn chunk_buffers(
    device: &RenderDevice,
    layout: &wgpu::BindGroupLayout,
    scene: &SceneBuffers,
    chunk: u32,
) -> ChunkBuffers {
    let d = &device.device;
    let texels = u64::from(chunk.max(1));
    let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
        d.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage,
            mapped_at_creation: false,
        })
    };
    let params = buffer(
        "lightmap bake params",
        size_of::<ParamsGpu>() as u64,
        wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    );
    let tasks = buffer(
        "lightmap bake tasks",
        texels * size_of::<TaskGpu>() as u64,
        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
    );
    let accum_size = texels * size_of::<AccumGpu>() as u64;
    let accum = buffer(
        "lightmap bake accum",
        accum_size,
        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
    );
    let staging = buffer(
        "lightmap bake readback",
        accum_size,
        wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
    );
    fn entry(binding: u32, b: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
        wgpu::BindGroupEntry {
            binding,
            resource: b.as_entire_binding(),
        }
    }
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("lightmap bake"),
        layout,
        entries: &[
            entry(0, &params),
            entry(1, &scene.nodes),
            entry(2, &scene.order),
            entry(3, &scene.tris),
            entry(4, &scene.emitters),
            entry(5, &scene.lights),
            entry(6, &tasks),
            entry(7, &accum),
        ],
    });
    ChunkBuffers {
        bind,
        params,
        tasks,
        accum,
        staging,
    }
}

/// The texels one chunk holds: all of them when they fit, else as many as the device's largest
/// storage binding (`max_binding`, bytes) and the memory budget (`memory_budget` bytes for the
/// whole bake, 0 for none) leave room for next to the scene's own `scene_bytes`. A chunk's texels
/// keep their own random streams, so the chunking does not change the result.
///
/// # Errors
///
/// [`PtError::Gpu`] starting `lightmap_gpu_limit` when the scene alone passes the budget or the
/// room left holds less than one workgroup of texels.
pub fn plan_gpu_chunk(
    scene_bytes: u64,
    texels: usize,
    memory_budget: u64,
    max_binding: u64,
) -> Result<u32, PtError> {
    let row = size_of::<TaskGpu>().max(size_of::<AccumGpu>()) as u64;
    let mut chunk = (texels as u64).min(max_binding / row).min(u64::from(u32::MAX));
    if memory_budget > 0 {
        let Some(room) = memory_budget.checked_sub(scene_bytes) else {
            return Err(PtError::Gpu(format!(
                "lightmap_gpu_limit: the scene alone needs {scene_bytes} bytes of GPU memory and the budget is {memory_budget}: raise the budget or bake on the CPU"
            )));
        };
        chunk = chunk.min(room / BYTES_PER_TEXEL);
    }
    if chunk < (texels as u64).min(u64::from(WORKGROUP)) {
        return Err(PtError::Gpu(format!(
            "lightmap_gpu_limit: the memory budget ({memory_budget} bytes) leaves room for {chunk} texels next to the scene's {scene_bytes} bytes; a chunk needs {WORKGROUP}: raise the budget or bake on the CPU"
        )));
    }
    Ok(chunk.max(1) as u32)
}

/// One dispatch of the bake: `task_count` texels from `first_task` (an index into the bake's
/// texel list) take samples `first_sample..first_sample + sample_count`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub struct LightmapGpuTile {
    /// The memory chunk the texels belong to ([`plan_gpu_chunk`]).
    pub chunk: u32,
    /// First texel of the tile.
    pub first_task: u32,
    /// Texels in the tile (consecutive).
    pub task_count: u32,
    /// First sample the tile takes.
    pub first_sample: u32,
    /// Samples each texel takes in the tile.
    pub sample_count: u32,
}

/// The tiles that take `active`'s texels (indices into the chunk starting at texel `chunk_base`)
/// through `samples`: sample blocks of at most `shape.samples`, each cut into runs of consecutive
/// texels of at most `shape.tasks`. A pure function of its inputs: the same bake plans the same
/// tiles on every device and every run.
fn plan_tiles(
    chunk: u32,
    chunk_base: u32,
    active: &[u32],
    samples: std::ops::Range<u32>,
    shape: TileShape,
) -> Vec<LightmapGpuTile> {
    let (most_tasks, most_samples) = (shape.tasks.max(1), shape.samples.max(1));
    let mut tiles = Vec::new();
    let mut sample = samples.start;
    while sample < samples.end {
        let count = (samples.end - sample).min(most_samples);
        let mut at = 0usize;
        while at < active.len() {
            let start = active[at];
            let mut run = 1usize;
            while at + run < active.len()
                && active[at + run] == start + run as u32
                && (run as u32) < most_tasks
            {
                run += 1;
            }
            tiles.push(LightmapGpuTile {
                chunk,
                first_task: chunk_base + start,
                task_count: run as u32,
                first_sample: sample,
                sample_count: count,
            });
            at += run;
        }
        sample += count;
    }
    tiles
}

/// The tiles a bake of `texels` texels of `scene` runs, in order, without adaptive sampling (with
/// it, the first batch is these tiles and the later ones depend on the noise read back), cut into
/// chunks of `chunk_texels` ([`plan_gpu_chunk`]). [`LightmapGpuStats::schedule_hash`] of such a
/// bake is [`lightmap_gpu_schedule_hash`] of this list.
#[must_use]
pub fn lightmap_gpu_schedule(
    scene: &LightmapScene,
    texels: usize,
    settings: &LightmapBakeSettings,
    options: &LightmapGpuOptions,
    chunk_texels: u32,
) -> Vec<LightmapGpuTile> {
    let base = params(scene, settings);
    let chunk = chunk_texels.max(1) as usize;
    // The first batch: every texel takes `samples` (adaptive sampling reads the noise after it).
    let samples = settings.samples.max(1);
    let mut tiles = Vec::new();
    for (c, first) in (0..texels).step_by(chunk).enumerate() {
        let len = (texels - first).min(chunk) as u32;
        let active: Vec<u32> = (0..len).collect();
        let shape = tile_shape(base.bounces, base.light_count, options.tile_rays, len as usize);
        tiles.extend(plan_tiles(c as u32, first as u32, &active, 0..samples, shape));
    }
    tiles
}

/// A 64-bit FNV-1a hash of the tile list, field by field: two bakes that ran the same tiles in the
/// same order have the same hash.
#[must_use]
pub fn lightmap_gpu_schedule_hash(tiles: &[LightmapGpuTile]) -> u64 {
    tiles.iter().fold(FNV_OFFSET, hash_tile)
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn hash_tile(mut h: u64, t: &LightmapGpuTile) -> u64 {
    for v in [
        t.chunk,
        t.first_task,
        t.task_count,
        t.first_sample,
        t.sample_count,
    ] {
        for b in v.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Most rays one tile may trace by default (about 4 M): short enough not to trip a system GPU
/// watchdog, long enough to keep a large GPU busy.
pub const DEFAULT_GPU_TILE_RAYS: u64 = 1 << 22;
/// Smallest [`LightmapGpuOptions::tile_rays`].
pub const MIN_GPU_TILE_RAYS: u64 = 1 << 12;
/// Largest [`LightmapGpuOptions::tile_rays`].
pub const MAX_GPU_TILE_RAYS: u64 = 1 << 26;
/// Longest [`LightmapGpuOptions::yield_between_tiles`].
pub const MAX_GPU_YIELD: Duration = Duration::from_secs(1);

/// How the GPU baker cuts and paces its work. None of it changes the result: every texel adds its
/// samples in the same order whatever the cut (tested byte for byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightmapGpuOptions {
    /// Most rays one tile (one dispatch, waited for) traces, about. Smaller tiles hand the GPU
    /// back sooner, so an editor drawing on the same GPU stays responsive.
    pub tile_rays: u64,
    /// Most GPU memory the bake holds, bytes (0 = the device's limits only). The texel list is
    /// baked in chunks that fit next to the scene ([`plan_gpu_chunk`]).
    pub memory_budget: u64,
    /// Pause after every tile, leaving the GPU to other work (an editor's frames). Zero by
    /// default.
    pub yield_between_tiles: Duration,
}

impl Default for LightmapGpuOptions {
    fn default() -> Self {
        Self {
            tile_rays: DEFAULT_GPU_TILE_RAYS,
            memory_budget: 0,
            yield_between_tiles: Duration::ZERO,
        }
    }
}

impl LightmapGpuOptions {
    /// Checks `tile_rays` (in `MIN_GPU_TILE_RAYS..=MAX_GPU_TILE_RAYS`) and the yield (at most
    /// [`MAX_GPU_YIELD`]).
    ///
    /// # Errors
    ///
    /// A message naming the first option out of range.
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_GPU_TILE_RAYS..=MAX_GPU_TILE_RAYS).contains(&self.tile_rays) {
            return Err(format!(
                "gpu_tile_rays {} is outside {MIN_GPU_TILE_RAYS}..={MAX_GPU_TILE_RAYS}",
                self.tile_rays
            ));
        }
        if self.yield_between_tiles > MAX_GPU_YIELD {
            return Err(format!(
                "gpu_yield {} ms is above {} ms",
                self.yield_between_tiles.as_millis(),
                MAX_GPU_YIELD.as_millis()
            ));
        }
        Ok(())
    }
}

/// Where a GPU bake is, handed to the control callback after every tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightmapGpuProgress {
    /// The tile that just finished.
    pub tile: LightmapGpuTile,
    /// Tiles finished, this one included.
    pub tiles_done: u64,
    /// Chunks of the bake.
    pub chunks: u32,
    /// Texel samples taken so far.
    pub texel_samples_done: u64,
    /// Texel samples the bake takes at most (texels times the sample cap).
    pub texel_samples_budget: u64,
    /// Texels the bake estimates (those on a surface, as the CPU baker's progress counts them).
    pub texels: usize,
}

impl LightmapGpuProgress {
    /// The texels done, counting a texel's share of its samples: what a `progress(done, total)`
    /// of the CPU baker would say.
    #[must_use]
    pub fn texels_done(&self) -> usize {
        let done = u128::from(self.texel_samples_done) * self.texels as u128
            / u128::from(self.texel_samples_budget.max(1));
        (done as usize).min(self.texels)
    }
}

/// What the control callback asks for after a tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightmapGpuControl {
    /// Run the next tile.
    Continue,
    /// Stop: the bake returns `lightmap_gpu_cancelled` and writes nothing.
    Cancel,
}

/// How a GPU bake ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightmapGpuStats {
    /// Tiles (dispatches) run.
    pub tiles: u64,
    /// Memory chunks.
    pub chunks: u32,
    /// Texels per chunk.
    pub chunk_texels: u32,
    /// Most texels per tile (of the first chunk).
    pub tile_texels: u32,
    /// Most samples per texel in a tile (of the first chunk).
    pub tile_samples: u32,
    /// [`LightmapGpuOptions::tile_rays`] of the bake.
    pub tile_rays: u64,
    /// GPU memory of the scene, bytes.
    pub scene_bytes: u64,
    /// GPU memory the bake held at most, bytes (scene and one chunk).
    pub peak_bytes: u64,
    /// Sums read back for adaptive sampling and results.
    pub readbacks: u32,
    /// [`lightmap_gpu_schedule_hash`] of the tiles run, in order.
    pub schedule_hash: u64,
}

/// The agent code of a GPU bake error: `lightmap_gpu_limit`, `lightmap_gpu_device_lost`,
/// `lightmap_gpu_cancelled`, `lightmap_gpu_options`, `lightmap_gpu` (another GPU failure) or
/// `lightmap_bake` (bad inputs, as on the CPU).
#[must_use]
pub fn lightmap_gpu_error_code(error: &PtError) -> &'static str {
    const CODES: [&str; 4] = [
        "lightmap_gpu_limit",
        "lightmap_gpu_device_lost",
        "lightmap_gpu_cancelled",
        "lightmap_gpu_options",
    ];
    match error {
        PtError::Gpu(m) => CODES
            .into_iter()
            .find(|c| m.starts_with(c))
            .unwrap_or("lightmap_gpu"),
        _ => "lightmap_bake",
    }
}

/// Whether the GPU device is lost ([`regia_render::gpu_health::device_lost`], set by the device-lost
/// callback the engine or `regia bake-lightmaps` installs).
fn lost() -> bool {
    regia_render::gpu_health::device_lost()
}

fn lost_error() -> PtError {
    let why = regia_render::gpu_health::device_lost_info()
        .map_or_else(String::new, |i| format!(" ({}: {})", i.reason, i.message));
    PtError::Gpu(format!(
        "lightmap_gpu_device_lost: the GPU device was lost{why}: nothing was written; bake again, or on the CPU"
    ))
}

/// `r`, or the device-lost error when the device is lost: a lost device fails calls in many ways
/// (or none, wgpu drops errors of a lost device), and all of them mean the same.
fn on_device<T>(r: Result<T, PtError>) -> Result<T, PtError> {
    if lost() { Err(lost_error()) } else { r }
}

/// Reads the first `count` sums of the chunk back.
fn read_accum(device: &RenderDevice, gpu: &ChunkBuffers, count: usize) -> Result<Vec<AccumGpu>, PtError> {
    let d = &device.device;
    let size = (count.max(1) * size_of::<AccumGpu>()) as u64;
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("lightmap bake readback"),
    });
    encoder.copy_buffer_to_buffer(&gpu.accum, 0, &gpu.staging, 0, size);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    gpu.staging
        .slice(..size)
        .map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
    d.poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    rx.try_recv()
        .map_err(|_| PtError::Gpu("the readback was never mapped".into()))?
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    let mapped = gpu
        .staging
        .slice(..size)
        .get_mapped_range()
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    let mut rows = bytemuck::cast_slice::<u8, AccumGpu>(&mapped).to_vec();
    drop(mapped);
    gpu.staging.unmap();
    rows.truncate(count);
    Ok(rows)
}

/// The tasks still sampling: those above the noise target (as on the CPU, a texel with no
/// measure yet, NaN, keeps sampling).
fn still_active(rows: &[AccumGpu], settings: &LightmapBakeSettings, active: &[u32]) -> Vec<u32> {
    active
        .iter()
        .copied()
        .filter(|&k| {
            let a = &rows[k as usize];
            let error = relative_error(a.taken, f64::from(a.lum_sum), f64::from(a.lum_sq));
            error.is_nan() || error > settings.noise_target
        })
        .collect()
}

/// The running state of a bake between tiles.
struct Run<'a, C> {
    device: &'a RenderDevice,
    pipeline: &'a wgpu::ComputePipeline,
    gpu: &'a ChunkBuffers,
    base: ParamsGpu,
    options: &'a LightmapGpuOptions,
    control: C,
    stats: LightmapGpuStats,
    done: u64,
    budget: u64,
    /// The tile submitted and not yet finished (only with no pause between tiles).
    queued: Option<(LightmapGpuTile, wgpu::SubmissionIndex)>,
    /// Texels of the bake.
    texels: usize,
}

impl<C: FnMut(&LightmapGpuProgress) -> LightmapGpuControl> Run<'_, C> {
    /// Submits `tile` (its first texel `chunk_base` into the chunk's buffers). With no pause between
    /// tiles, one tile stays queued behind the one running, so the GPU does not idle while the host
    /// waits and asks the control: the tile before this one is finished here. With a pause, the
    /// tile is finished at once and the GPU then left alone. Either way the control sees every tile,
    /// in order, after it ran. The parameters of a queued tile are written after the running tile's
    /// submission, and the queue orders the write after it.
    fn tile(&mut self, tile: LightmapGpuTile, chunk_base: u32) -> Result<(), PtError> {
        let _span = tracing::trace_span!("lightmap_bake_gpu_tile", texels = tile.task_count).entered();
        let d = &self.device.device;
        let p = ParamsGpu {
            first_task: tile.first_task - chunk_base,
            task_count: tile.task_count,
            first_sample: tile.first_sample,
            sample_count: tile.sample_count,
            ..self.base
        };
        self.device
            .queue
            .write_buffer(&self.gpu.params, 0, bytemuck::bytes_of(&p));
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("lightmap bake"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("lightmap bake"),
                timestamp_writes: None,
            });
            pass.set_pipeline(self.pipeline);
            pass.set_bind_group(0, &self.gpu.bind, &[]);
            pass.dispatch_workgroups(tile.task_count.div_ceil(WORKGROUP), 1, 1);
        }
        let index = self.device.queue.submit([encoder.finish()]);
        if self.options.yield_between_tiles.is_zero() {
            match self.queued.replace((tile, index)) {
                Some((before, at)) => self.finish(before, at),
                None => Ok(()),
            }
        } else {
            self.finish(tile, index)?;
            std::thread::sleep(self.options.yield_between_tiles);
            Ok(())
        }
    }

    /// Finishes the queued tile, if any: every tile submitted so far has run and been seen.
    fn flush(&mut self) -> Result<(), PtError> {
        match self.queued.take() {
            Some((tile, at)) => self.finish(tile, at),
            None => Ok(()),
        }
    }

    /// Waits for `tile` (submission `at`), counts it and asks the control what next.
    fn finish(&mut self, tile: LightmapGpuTile, at: wgpu::SubmissionIndex) -> Result<(), PtError> {
        self.device
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(at),
                timeout: None,
            })
            .map_err(|e| PtError::Gpu(e.to_string()))?;
        if lost() {
            return Err(lost_error());
        }
        self.stats.tiles += 1;
        self.stats.schedule_hash = hash_tile(self.stats.schedule_hash, &tile);
        self.done += u64::from(tile.task_count) * u64::from(tile.sample_count);
        let progress = LightmapGpuProgress {
            tile,
            tiles_done: self.stats.tiles,
            chunks: self.stats.chunks,
            texel_samples_done: self.done,
            texel_samples_budget: self.budget,
            texels: self.texels,
        };
        if (self.control)(&progress) == LightmapGpuControl::Cancel {
            return Err(PtError::Gpu(format!(
                "lightmap_gpu_cancelled: the bake was cancelled after {} tiles: nothing was written",
                self.stats.tiles
            )));
        }
        if lost() {
            return Err(lost_error());
        }
        Ok(())
    }
}

/// [`super::bake_lightmap`] on `device`: the same inputs and output, the estimate on the GPU. The
/// bake description's lights and `settings` mean the same; `settings.threads` is not used.
/// `progress(done, total)` counts texels, as the CPU baker's does. The default
/// [`LightmapGpuOptions`]; [`bake_lightmap_gpu_with`] takes others and a control callback.
///
/// # Errors
///
/// As [`bake_lightmap_gpu_with`].
pub fn bake_lightmap_gpu(
    device: &RenderDevice,
    scene: &LightmapScene,
    layout: LightmapLayout,
    receivers: &[usize],
    settings: &LightmapBakeSettings,
    progress: impl Fn(usize, usize),
) -> Result<(Lightmap, LightmapBakeReport), PtError> {
    let (mut last, mut total) = (0usize, 0usize);
    let (map, report, _) = bake_lightmap_gpu_with(
        device,
        scene,
        layout,
        receivers,
        settings,
        &LightmapGpuOptions::default(),
        |p| {
            total = p.texels;
            let done = p.texels_done();
            if done != last {
                last = done;
                progress(done, total);
            }
            LightmapGpuControl::Continue
        },
    )?;
    progress(total, total);
    Ok((map, report))
}

/// [`super::bake_lightmap`] on `device`, cut and paced by `options` (ADR-2125). The texels are
/// baked in memory chunks ([`plan_gpu_chunk`]) and each chunk in tiles ([`lightmap_gpu_schedule`]),
/// each tile waited for; after every tile `control` sees where the bake is and may cancel it, and
/// the bake then pauses for `options.yield_between_tiles`. The cut never changes the result.
///
/// # Errors
///
/// As [`super::bake_lightmap`]; [`PtError::Gpu`] when the shader does not build or a dispatch
/// fails (`lightmap_gpu`), the scene does not fit the device or the memory budget
/// (`lightmap_gpu_limit`), the device is lost (`lightmap_gpu_device_lost`), `control` cancels
/// (`lightmap_gpu_cancelled`) or `options` are out of range (`lightmap_gpu_options`); the code
/// starts the message ([`lightmap_gpu_error_code`]).
pub fn bake_lightmap_gpu_with(
    device: &RenderDevice,
    scene: &LightmapScene,
    layout: LightmapLayout,
    receivers: &[usize],
    settings: &LightmapBakeSettings,
    options: &LightmapGpuOptions,
    control: impl FnMut(&LightmapGpuProgress) -> LightmapGpuControl,
) -> Result<(Lightmap, LightmapBakeReport, LightmapGpuStats), PtError> {
    let _span = tracing::info_span!("lightmap_bake_gpu", entries = layout.entries.len()).entered();
    options
        .validate()
        .map_err(|m| PtError::Gpu(format!("lightmap_gpu_options: {m}")))?;
    let prepared: Prepared = prepare_tasks(scene, &layout, receivers, settings)?;
    bake_prepared(device, scene, layout, settings, options, control, prepared, None)
}

/// [`bake_lightmap_gpu_with`] with a checkpoint (ADR-3095): the texels are baked in memory chunks
/// of at most `plan.batch_units` texels ([`crate::checkpoint::DEFAULT_LIGHTMAP_BATCH_TEXELS`] by
/// default; a chunk's texels keep their streams, so the cut never changes the result) and the
/// finished texels' sums are written to `plan.path` as the plan's cadence asks. With `plan.resume`
/// and a checkpoint of this bake there, the bake continues after its last texel. The checkpoint is
/// removed when the bake finishes.
///
/// # Errors
///
/// As [`bake_lightmap_gpu_with`]; the checkpoint codes of [`super::bake_lightmap_checkpointed`].
#[allow(clippy::too_many_arguments)] // ADR-3095: `bake_lightmap_gpu_with`'s seven inputs and the plan.
pub fn bake_lightmap_gpu_checkpointed(
    device: &RenderDevice,
    scene: &LightmapScene,
    layout: LightmapLayout,
    receivers: &[usize],
    settings: &LightmapBakeSettings,
    options: &LightmapGpuOptions,
    plan: &crate::checkpoint::BakeCheckpointPlan,
    control: impl FnMut(&LightmapGpuProgress) -> LightmapGpuControl,
) -> Result<
    (
        Lightmap,
        LightmapBakeReport,
        LightmapGpuStats,
        crate::checkpoint::BakeCheckpointStats,
    ),
    PtError,
> {
    use regia_asset::bake_checkpoint::BakeCheckpointKind;
    let _span =
        tracing::info_span!("lightmap_bake_gpu_checkpointed", entries = layout.entries.len()).entered();
    options
        .validate()
        .map_err(|m| PtError::Gpu(format!("lightmap_gpu_options: {m}")))?;
    let prepared: Prepared = prepare_tasks(scene, &layout, receivers, settings)?;
    let total = prepared.tasks.len();
    let hash = super::lightmap_input_hash(
        scene,
        &layout,
        receivers,
        settings,
        BakeCheckpointKind::LightmapGpu,
    );
    let (mut ck, found) = crate::checkpoint::Checkpointer::open(plan, BakeCheckpointKind::LightmapGpu, hash)?;
    let (estimates, rays) = match found {
        Some(c) if c.header.units_total != total as u64 => {
            return Err(crate::checkpoint::damaged(format!(
                "the checkpoint counts {} texels, the bake {total}",
                c.header.units_total
            )));
        }
        Some(c) => super::resume::decode(&c.payload, c.header.units_done, total)?,
        None => (Vec::new(), 0),
    };
    let resume = GpuResume {
        ck: &mut ck,
        estimates,
        rays,
        batch: plan.batch_or(crate::checkpoint::DEFAULT_LIGHTMAP_BATCH_TEXELS),
    };
    let (map, report, stats) = bake_prepared(
        device,
        scene,
        layout,
        settings,
        options,
        control,
        prepared,
        Some(resume),
    )?;
    Ok((map, report, stats, ck.finish()))
}

/// A checkpointed GPU bake's running checkpoint and the prefix it resumed.
struct GpuResume<'c, 'p> {
    ck: &'c mut crate::checkpoint::Checkpointer<'p>,
    estimates: Vec<TexelEstimate>,
    rays: u64,
    batch: usize,
}

/// The GPU bake of `prepared`'s tasks, from the resumed prefix of `resume` when given.
#[allow(clippy::too_many_arguments)] // ADR-3095: the bake's inputs, the control and the checkpoint.
fn bake_prepared(
    device: &RenderDevice,
    scene: &LightmapScene,
    layout: LightmapLayout,
    settings: &LightmapBakeSettings,
    options: &LightmapGpuOptions,
    control: impl FnMut(&LightmapGpuProgress) -> LightmapGpuControl,
    prepared: Prepared,
    mut resume: Option<GpuResume<'_, '_>>,
) -> Result<(Lightmap, LightmapBakeReport, LightmapGpuStats), PtError> {
    if lost() {
        return Err(lost_error());
    }
    let total = prepared.tasks.len();
    let mut estimates = vec![TexelEstimate::default(); total];
    let mut rays = 0u64;
    let mut start = 0usize;
    if let Some(r) = resume.as_mut() {
        start = r.estimates.len().min(total);
        estimates[..start].copy_from_slice(&r.estimates[..start]);
        rays = r.rays;
    }
    let mut stats = LightmapGpuStats {
        tile_rays: options.tile_rays,
        schedule_hash: FNV_OFFSET,
        ..LightmapGpuStats::default()
    };
    if start < total {
        let (pipeline, bind_layout) = on_device(create_pipeline(device))?;
        let scene_gpu = on_device(scoped(device, || upload_scene(device, scene)))?;
        let limits = device.device.limits();
        let chunk = plan_gpu_chunk(
            scene_gpu.bytes,
            total,
            options.memory_budget,
            limits.max_storage_buffer_binding_size.min(limits.max_buffer_size),
        )?;
        // A checkpointed bake's batch is a chunk: at most `batch` texels.
        let chunk = match &resume {
            Some(r) => chunk.min(u32::try_from(r.batch.max(1)).unwrap_or(u32::MAX)),
            None => chunk,
        };
        let gpu = on_device(scoped(device, || {
            Ok(chunk_buffers(device, &bind_layout, &scene_gpu, chunk))
        }))?;
        let base = params(scene, settings);
        let shape = tile_shape(base.bounces, base.light_count, options.tile_rays, chunk as usize);
        let cap = settings.sample_cap();
        stats.chunks = (total - start).div_ceil(chunk as usize) as u32;
        stats.chunk_texels = chunk;
        stats.tile_texels = shape.tasks;
        stats.tile_samples = shape.samples;
        stats.scene_bytes = scene_gpu.bytes;
        stats.peak_bytes = scene_gpu.bytes + u64::from(chunk) * BYTES_PER_TEXEL;
        let mut run = Run {
            device,
            pipeline: &pipeline,
            gpu: &gpu,
            base,
            options,
            control,
            stats,
            done: start as u64 * u64::from(cap),
            budget: total as u64 * u64::from(cap),
            queued: None,
            texels: total,
        };
        for (c, part) in prepared.tasks[start..].chunks(chunk as usize).enumerate() {
            let chunk_base = (start + c * chunk as usize) as u32;
            let rows = on_device(scoped(device, || {
                bake_chunk(&mut run, settings, part, c as u32, chunk_base)
            }))?;
            for (out, a) in estimates[chunk_base as usize..].iter_mut().zip(&rows) {
                let n = a.taken.max(1) as f32;
                *out = TexelEstimate {
                    e: Vec3::from_array(a.sum) / n,
                    back: a.back,
                    taken: a.taken,
                    dir: if a.dir_weight > 0.0 {
                        Vec3::from_array(a.dir) / a.dir_weight
                    } else {
                        Vec3::ZERO
                    },
                    rel_err: relative_error(a.taken, f64::from(a.lum_sum), f64::from(a.lum_sq)),
                };
                rays += u64::from(a.rays);
            }
            if let Some(r) = resume.as_mut() {
                let end = chunk_base as usize + part.len();
                let done = &estimates[..end];
                r.ck.batch_done(0, total as u64, end as u64, end == total, || {
                    super::resume::encode(done, rays)
                })?;
            }
        }
        stats = run.stats;
    }
    let (map, report) = finish_bake(scene, layout, settings, &prepared, &estimates, rays)?;
    Ok((map, report, stats))
}

/// Bakes one chunk's texels `part` to the end (every batch of adaptive sampling) and returns their
/// sums.
fn bake_chunk<C: FnMut(&LightmapGpuProgress) -> LightmapGpuControl>(
    run: &mut Run<'_, C>,
    settings: &LightmapBakeSettings,
    part: &[Task],
    chunk: u32,
    chunk_base: u32,
) -> Result<Vec<AccumGpu>, PtError> {
    let _span = tracing::info_span!("lightmap_bake_gpu_chunk", chunk, texels = part.len()).entered();
    let rows: Vec<TaskGpu> = part
        .iter()
        .map(|t| TaskGpu {
            p: t.p.to_array(),
            key: t.key,
            n: vec4(t.n.to_array(), 0.0),
            face: vec4(t.face.to_array(), 0.0),
        })
        .collect();
    let device = run.device;
    device
        .queue
        .write_buffer(&run.gpu.tasks, 0, bytemuck::cast_slice(&rows));
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("lightmap bake chunk"),
        });
    encoder.clear_buffer(&run.gpu.accum, 0, None);
    device.queue.submit([encoder.finish()]);
    let shape = tile_shape(
        run.base.bounces,
        run.base.light_count,
        run.options.tile_rays,
        part.len(),
    );
    let cap = settings.sample_cap();
    let batch = settings.samples.max(1);
    let mut active: Vec<u32> = (0..part.len() as u32).collect();
    let mut taken = 0u32;
    while taken < cap && !active.is_empty() {
        let count = (cap - taken).min(batch);
        for tile in plan_tiles(chunk, chunk_base, &active, taken..taken + count, shape) {
            run.tile(tile, chunk_base)?;
        }
        run.flush()?;
        taken += count;
        if settings.noise_target <= 0.0 {
            break;
        }
        if taken < cap {
            let rows = read_accum(device, run.gpu, part.len())?;
            run.stats.readbacks += 1;
            active = still_active(&rows, settings, &active);
        }
    }
    run.stats.readbacks += 1;
    read_accum(device, run.gpu, part.len())
}

/// Contract rows of the baker's structs (`regia_render::ContractRow`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<regia_render::ContractRow> {
    use regia_render::{ContractRow, contract_row};
    use std::mem::offset_of;
    let p = offset_of!(TriGpu, p);
    let n = offset_of!(TriGpu, n);
    vec![
        contract_row!(
            "lightmap_bake.wgsl",
            "Params",
            ParamsGpu {
                first_task,
                task_count,
                first_sample,
                sample_count,
                seed,
                bounces,
                direct_sun,
                has_sky,
                sky,
                sun_toward,
                sun_light,
                light_count,
                emitter_count,
                emitter_total,
                sun_lit,
                extra_toward,
                extra_light
            }
        ),
        ContractRow::new(
            "lightmap_bake.wgsl",
            "Tri",
            size_of::<TriGpu>(),
            vec![
                ("p0", p),
                ("p1", p + 16),
                ("p2", p + 32),
                ("n0", n),
                ("n1", n + 16),
                ("n2", n + 32),
                ("face", offset_of!(TriGpu, face)),
                ("albedo", offset_of!(TriGpu, albedo)),
            ],
        ),
        contract_row!(
            "lightmap_bake.wgsl",
            "Emitter",
            EmitterGpu {
                radiance,
                tri,
                area,
                two_sided,
                cdf,
                pad
            }
        ),
        contract_row!(
            "lightmap_bake.wgsl",
            "Light",
            LightGpu {
                kind,
                inner_cos,
                outer_cos,
                radius,
                position,
                direction,
                color
            }
        ),
        contract_row!("lightmap_bake.wgsl", "Task", TaskGpu { p, key, n, face }),
        contract_row!(
            "lightmap_bake.wgsl",
            "Accum",
            AccumGpu {
                sum,
                lum_sum,
                dir,
                dir_weight,
                lum_sq,
                back,
                taken,
                rays
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device error during the bake comes back as an error of the bake: on a headless device it
    /// would otherwise panic, and the CLI would end with no JSON.
    #[test]
    fn device_errors_come_back_as_errors() {
        let device = RenderDevice::headless().expect("a headless device (lavapipe)");
        let ok = scoped(&device, || Ok(5)).expect("no error");
        assert_eq!(ok, 5);
        let size = device.device.limits().max_buffer_size.saturating_add(4);
        let bad = scoped(&device, || {
            let _buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("too large"),
                size,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            });
            Ok(())
        });
        assert!(matches!(bad, Err(PtError::Gpu(_))), "{bad:?}");
        // The device still works: the scopes were popped.
        assert_eq!(scoped(&device, || Ok(6)).expect("no error"), 6);
    }

    /// Both limits count, and the message carries the code the CLI maps.
    #[test]
    fn a_scene_past_either_buffer_limit_is_refused() {
        assert!(check_fit(100, 100, 100).is_ok());
        for (binding, buffer) in [(99, 100), (100, 99)] {
            let e = check_fit(100, binding, buffer).expect_err("too large");
            assert!(e.to_string().contains("lightmap_gpu_limit"), "{e}");
        }
    }

    /// A tile is wide before it is deep: a large chunk fills the dispatch with texels at one
    /// sample, a small one takes more samples; lights shorten it; it stays within the ray budget
    /// unless that is under one workgroup's single sample, so a bake always moves forward.
    #[test]
    fn tiles_are_wide_first_and_shrink_with_the_rays_they_trace() {
        let rays = DEFAULT_GPU_TILE_RAYS;
        let big = 1usize << 22;
        let shape = |b, l, r, t| tile_shape(b, l, r, t);
        // 3 bounces, no light: 9 rays a sample, 466033 texel-samples.
        assert_eq!(
            shape(3, 0, rays, big),
            TileShape {
                tasks: TASKS_PER_DISPATCH,
                samples: 1
            }
        );
        assert_eq!(
            shape(3, 0, rays, 1024),
            TileShape {
                tasks: 1024,
                samples: SAMPLES_PER_DISPATCH
            }
        );
        assert_eq!(
            shape(3, 0, rays, 100_000),
            TileShape {
                tasks: 100_000,
                samples: 4
            }
        );
        let few = shape(3, 4, rays, big);
        assert!(few.tasks < TASKS_PER_DISPATCH && few.samples == 1, "{few:?}");
        assert!(shape(3, 200, rays, big).tasks < few.tasks / 8);
        assert_eq!(
            shape(u32::MAX, u32::MAX, rays, big),
            TileShape {
                tasks: WORKGROUP,
                samples: 1
            }
        );
        assert_eq!(
            shape(3, 0, MIN_GPU_TILE_RAYS, big),
            TileShape {
                tasks: 455,
                samples: 1
            }
        );
        assert_eq!(shape(3, 0, MAX_GPU_TILE_RAYS, big).tasks, TASKS_PER_DISPATCH);
        assert_eq!(
            shape(3, 0, rays, 0),
            TileShape {
                tasks: 1,
                samples: SAMPLES_PER_DISPATCH
            }
        );
        for (b, l, r, t) in [
            (3, 0, rays, big),
            (3, 4, rays, 5000),
            (1, 0, MIN_GPU_TILE_RAYS, 77),
            (5, 9, rays, big),
        ] {
            let s = shape(b, l, r, t);
            let per_path = u64::from(b) * (3 + u64::from(l));
            assert!(
                u64::from(s.tasks) * u64::from(s.samples) * per_path <= r,
                "{s:?} over {r} rays"
            );
        }
    }

    /// Every (texel, sample) pair of the active texels is in exactly one tile, a texel's samples
    /// come in increasing order, no tile passes its texel or sample bound, and the same inputs
    /// plan the same tiles.
    #[test]
    fn the_tile_plan_covers_each_texel_sample_once_in_order() {
        let active: Vec<u32> = (0..300u32).filter(|k| k % 7 != 3).chain(400..450).collect();
        let shape = TileShape {
            tasks: 64,
            samples: 7,
        };
        let tiles = plan_tiles(2, 1000, &active, 5..45, shape);
        assert_eq!(
            tiles,
            plan_tiles(2, 1000, &active, 5..45, shape),
            "a pure function"
        );
        let mut next = std::collections::HashMap::new();
        for t in &tiles {
            assert_eq!(t.chunk, 2);
            assert!(t.task_count >= 1 && t.task_count <= 64, "{t:?}");
            assert!(t.sample_count >= 1 && t.sample_count <= 7, "{t:?}");
            for k in t.first_task..t.first_task + t.task_count {
                let at = next.entry(k).or_insert(5u32);
                assert_eq!(*at, t.first_sample, "texel {k} out of order");
                *at += t.sample_count;
            }
        }
        let expected: std::collections::HashMap<u32, u32> = active.iter().map(|&k| (1000 + k, 45)).collect();
        assert_eq!(
            next, expected,
            "every active texel reaches the end, and no other texel runs"
        );
        assert!(plan_tiles(0, 0, &[], 0..16, shape).is_empty());
        assert!(plan_tiles(0, 0, &active, 7..7, shape).is_empty());
    }

    /// A chunk takes every texel when they fit; the binding limit and the budget each cut it;
    /// a budget the scene alone passes, or one leaving under a workgroup, is `lightmap_gpu_limit`.
    #[test]
    fn chunks_fit_the_binding_limit_and_the_budget() {
        let row = size_of::<TaskGpu>().max(size_of::<AccumGpu>()) as u64;
        assert_eq!(plan_gpu_chunk(1000, 5000, 0, u64::MAX).expect("fits"), 5000);
        assert_eq!(plan_gpu_chunk(1000, 5000, 0, 100 * row).expect("binding"), 100);
        let budget = 1000 + 200 * BYTES_PER_TEXEL + 7;
        assert_eq!(plan_gpu_chunk(1000, 5000, budget, u64::MAX).expect("budget"), 200);
        assert_eq!(plan_gpu_chunk(1000, 150, budget, u64::MAX).expect("all fit"), 150);
        // A bake smaller than a workgroup fits in less.
        assert_eq!(
            plan_gpu_chunk(1000, 3, 1000 + 3 * BYTES_PER_TEXEL, u64::MAX).expect("tiny"),
            3
        );
        for (scene, budget) in [(1000, 999), (1000, 1000 + 31 * BYTES_PER_TEXEL)] {
            let e = plan_gpu_chunk(scene, 5000, budget, u64::MAX).expect_err("too small");
            assert_eq!(lightmap_gpu_error_code(&e), "lightmap_gpu_limit", "{e}");
        }
        assert_eq!(plan_gpu_chunk(0, 0, 0, u64::MAX).expect("empty"), 1);
    }

    /// The options' bounds, and the code of every error kind.
    #[test]
    fn options_are_bounded_and_errors_carry_their_code() {
        assert!(LightmapGpuOptions::default().validate().is_ok());
        for bad in [
            LightmapGpuOptions {
                tile_rays: MIN_GPU_TILE_RAYS - 1,
                ..LightmapGpuOptions::default()
            },
            LightmapGpuOptions {
                tile_rays: MAX_GPU_TILE_RAYS + 1,
                ..LightmapGpuOptions::default()
            },
            LightmapGpuOptions {
                yield_between_tiles: MAX_GPU_YIELD + Duration::from_millis(1),
                ..LightmapGpuOptions::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        let gpu = |m: &str| PtError::Gpu(m.into());
        assert_eq!(
            lightmap_gpu_error_code(&gpu("lightmap_gpu_limit: x")),
            "lightmap_gpu_limit"
        );
        assert_eq!(lightmap_gpu_error_code(&lost_error()), "lightmap_gpu_device_lost");
        assert_eq!(
            lightmap_gpu_error_code(&gpu("lightmap_gpu_cancelled: x")),
            "lightmap_gpu_cancelled"
        );
        assert_eq!(
            lightmap_gpu_error_code(&gpu("lightmap_gpu_options: x")),
            "lightmap_gpu_options"
        );
        assert_eq!(lightmap_gpu_error_code(&gpu("shader")), "lightmap_gpu");
        assert_eq!(
            lightmap_gpu_error_code(&PtError::Scene("x".into())),
            "lightmap_bake"
        );
    }

    /// The schedule hash changes with any field and with the order of the tiles.
    #[test]
    fn the_schedule_hash_sees_every_field_and_the_order() {
        let a = LightmapGpuTile {
            chunk: 0,
            first_task: 0,
            task_count: 32,
            first_sample: 0,
            sample_count: 16,
        };
        let b = LightmapGpuTile { first_task: 32, ..a };
        let base = lightmap_gpu_schedule_hash(&[a, b]);
        assert_ne!(base, lightmap_gpu_schedule_hash(&[b, a]));
        assert_ne!(base, lightmap_gpu_schedule_hash(&[a]));
        for changed in [
            LightmapGpuTile { chunk: 1, ..b },
            LightmapGpuTile { task_count: 31, ..b },
            LightmapGpuTile {
                first_sample: 16,
                ..b
            },
            LightmapGpuTile {
                sample_count: 15,
                ..b
            },
        ] {
            assert_ne!(base, lightmap_gpu_schedule_hash(&[a, changed]), "{changed:?}");
        }
    }

    /// The shader builds on its own, and its blocks match the host's: every struct the host
    /// uploads has the size the shader expects.
    #[test]
    fn the_shader_validates_and_its_blocks_match() {
        let module =
            regia_render::compose_source(SOURCE, "lightmap_bake.wgsl", std::collections::HashMap::new())
                .expect("composes");
        let mut layouter = naga::proc::Layouter::default();
        layouter.update(module.to_ctx()).expect("layout");
        let size_of_type = |name: &str| {
            module
                .types
                .iter()
                .find(|(_, t)| t.name.as_deref() == Some(name))
                .map(|(handle, _)| layouter[handle].size)
                .expect(name)
        };
        assert_eq!(size_of_type("Params") as usize, size_of::<ParamsGpu>());
        assert_eq!(size_of_type("Tri") as usize, size_of::<TriGpu>());
        assert_eq!(size_of_type("Emitter") as usize, size_of::<EmitterGpu>());
        assert_eq!(size_of_type("Light") as usize, size_of::<LightGpu>());
        assert_eq!(size_of_type("Task") as usize, size_of::<TaskGpu>());
        assert_eq!(size_of_type("Accum") as usize, size_of::<AccumGpu>());
    }

    /// The traversal stack lives in workgroup memory (ADR-7879): `STACK` words for each of the
    /// `WORKGROUP` invocations `bake` runs per group, inside WebGPU's 16 KiB default limit, and
    /// deep enough for `MAX_BVH_DEPTH` (a node pushes two children after popping itself).
    #[test]
    fn the_workgroup_stack_holds_every_invocation() {
        let module =
            regia_render::compose_source(SOURCE, "lightmap_bake.wgsl", std::collections::HashMap::new())
                .expect("composes");
        let mut layouter = naga::proc::Layouter::default();
        layouter.update(module.to_ctx()).expect("layout");
        let workgroup: Vec<(String, u32)> = module
            .global_variables
            .iter()
            .filter(|(_, g)| g.space == naga::AddressSpace::WorkGroup)
            .map(|(_, g)| (g.name.clone().unwrap_or_default(), layouter[g.ty].size))
            .collect();
        let stack = shader_const("STACK");
        assert_eq!(shader_const("GROUP"), WORKGROUP);
        assert_eq!(workgroup, [("bvh_stack".to_string(), stack * WORKGROUP * 4)]);
        assert!(stack * WORKGROUP * 4 <= wgpu::Limits::default().max_compute_workgroup_storage_size);
        assert!(MAX_BVH_DEPTH + 2 <= stack as usize);
        let bake = module
            .entry_points
            .iter()
            .find(|e| e.name == "bake")
            .expect("bake");
        assert_eq!(bake.workgroup_size, [WORKGROUP, 1, 1]);
    }

    /// `const NAME: u32 = <n>u;` of the shader.
    fn shader_const(name: &str) -> u32 {
        let prefix = format!("const {name}: u32 = ");
        SOURCE
            .lines()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .and_then(|v| v.trim_end_matches("u;").parse().ok())
            .unwrap_or_else(|| panic!("lightmap_bake.wgsl declares {name}"))
    }
}
