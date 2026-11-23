// SPDX-License-Identifier: MPL-2.0
//! GPU driver: one compute module (`pt.wgsl`), four pipelines, a persistent path-state buffer,
//! progressive accumulation and readback.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use regia_render::RenderDevice;
use wgpu::util::DeviceExt;

use crate::PtError;
use crate::output::{PtImage, PtMetadata};
use crate::pack::{FLAG_JITTER, Globals, Packed, PtCache, pack_with};
use crate::presets::PtQuality;
use crate::scene::PtScene;

pub(crate) const SOURCE: &str = include_str!("../shaders/pt.wgsl");
/// vec4 layers per pixel in the accumulation buffer (`AOV_COUNT` in the shader).
const AOV_COUNT: usize = 11;
/// vec4s per pixel in the path state (`Path` in the shader).
const PATH_VEC4S: u64 = 12;
const WORKGROUP: u32 = 64;
/// Bytes of workgroup memory `intersect` and `shade` hold: `pt.wgsl`'s `trace_stack`, the
/// traversal stack of every invocation of a workgroup (`TRACE_SLOTS` × `TRACE_GROUP` words,
/// ADR-7879). Above WebGPU's 16 KiB default, inside every desktop adapter's own limit, which
/// [`RenderDevice`] requests.
const TRACE_STACK_BYTES: u32 = 112 * WORKGROUP * 4;

/// wgpu's default `max_compute_workgroups_per_dimension`. One dispatch axis cannot exceed it.
const MAX_COMPUTE_WORKGROUPS_PER_DIM: u32 = 65_535;

/// One compute dispatch covering a run of workgroups.
///
/// The shader group is `gid.x + gid.y * gx` (`pt.wgsl` `invocation_index`), so this covers
/// `first_group .. first_group + gx * gy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkgroupDispatch {
    first_group: u32,
    gx: u32,
    gy: u32,
}

/// Dispatches that cover exactly `count` workgroups starting at `first`.
///
/// A rectangle `min(count, 65535) × ceil(count / 65535)` is larger than `count` whenever `count`
/// is not a multiple of 65535. The shader indexes `gid.x + gid.y * gx * 64 + first_group * 64`
/// and only rejects `idx >= pixel_count`, so that overhang shades real pixels outside the range
/// and a later range inside it is accumulated twice. Full rows of 65535 plus a remainder
/// dispatch (its `first_group` advanced by the rows already covered) match the range.
fn workgroup_dispatches(first: u32, count: u32) -> Vec<WorkgroupDispatch> {
    let mut out = Vec::new();
    let mut covered = 0u32;
    while covered < count {
        let rest = count - covered;
        let max = MAX_COMPUTE_WORKGROUPS_PER_DIM;
        let (gx, gy) = if rest > max {
            let rows = (rest / max).min(max);
            (max, rows)
        } else {
            (rest, 1)
        };
        let Some(first_group) = first.checked_add(covered) else {
            break;
        };
        out.push(WorkgroupDispatch { first_group, gx, gy });
        covered += gx * gy;
    }
    out
}

/// The most path vertices a trace may ask for (`PtSettings::max_bounces` is clamped to it). It was
/// 64; thick cloud slabs scatter hundreds of times before a photon leaves (ADR-1834), so 64
/// truncated them. Each extra vertex costs one intersect and one shade dispatch per sample.
pub const MAX_PATH_VERTICES: u32 = 1024;

/// Per-trace settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtSettings {
    /// Samples per pixel to accumulate.
    pub spp: u32,
    /// Maximum path vertices (1 = direct light at the first hit only), at most [`MAX_PATH_VERTICES`].
    pub max_bounces: u32,
    /// Scene seed: the same seed, scene and device give the same image bit for bit.
    pub seed: u32,
    /// Path vertex from which Russian roulette may end a path.
    pub rr_start: u32,
    /// Jitter the camera ray inside each pixel (off: pixel centres, for exact comparisons).
    pub jitter: bool,
    /// Samples recorded per GPU submission (bounds the length of one submit).
    pub samples_per_submit: u32,
}

impl Default for PtSettings {
    fn default() -> Self {
        Self {
            spp: 256,
            max_bounces: 12,
            seed: 1,
            rr_start: 3,
            jitter: true,
            samples_per_submit: 4,
        }
    }
}

/// The compiled path tracer for one device.
pub struct PathTracer {
    pub(crate) device: RenderDevice,
    layout: wgpu::BindGroupLayout,
    pub(crate) generate: wgpu::ComputePipeline,
    pub(crate) intersect: wgpu::ComputePipeline,
    pub(crate) shade: wgpu::ComputePipeline,
    accumulate: wgpu::ComputePipeline,
    /// Probe mode's per-sample fold (`probe.rs`).
    pub(crate) probe_accumulate: wgpu::ComputePipeline,
    /// The indirect dispatch size of a compacted bounce (ADR-862, `probe.rs`).
    pub(crate) queue_dispatch: wgpu::ComputePipeline,
    pub(crate) param_stride: u64,
    /// The layout every kernel set shares (graph variants are built against it).
    pipeline_layout: wgpu::PipelineLayout,
    /// Kernels with a scene's material graphs composed in, by `GraphSet::key` (ADR-2725).
    graph_kernels: Mutex<HashMap<u64, Arc<Kernels>>>,
    /// Graph kernel sets compiled so far (statistics; a cache hit does not count).
    graph_compiles: std::sync::atomic::AtomicU64,
    /// Packed probe scene reused across traces of the same content (ADR-5154).
    probe_scene: Mutex<ProbeHold>,
    /// `pack_with` calls made for probe traces.
    probe_packs: std::sync::atomic::AtomicU64,
    /// `upload` calls made for probe traces.
    probe_uploads: std::sync::atomic::AtomicU64,
    /// Packs beauty and the second-moment AOV for adaptive stopping. Built on first use.
    stopping_pack: OnceLock<Result<StoppingPack, String>>,
}

/// One compiled kernel set: the six entry points of a composed `pt.wgsl`.
pub(crate) struct Kernels {
    generate: wgpu::ComputePipeline,
    intersect: wgpu::ComputePipeline,
    shade: wgpu::ComputePipeline,
    accumulate: wgpu::ComputePipeline,
}

/// Packs two vec4s per pixel out of the accumulation buffer: beauty (AOV 0, sample count in w)
/// and the second-moment AOV. Adaptive stopping only needs those.
struct StoppingPack {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
}

impl StoppingPack {
    fn build(device: &RenderDevice) -> Result<Self, String> {
        let d = &device.device;
        let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pt stopping pack"),
            entries: &[storage(0, true), storage(1, false)],
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pt stopping pack"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pt stopping pack"),
            source: wgpu::ShaderSource::Wgsl(stopping_pack_shader().into()),
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pt stopping pack"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("pack_stopping"),
            compilation_options: Default::default(),
            cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
        });
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            return Err(err.to_string());
        }
        Ok(Self { layout, pipeline })
    }
}

/// Largest workgroup count one dispatch axis may use (the Vulkan minimum of
/// `maxComputeWorkGroupCount`).
const DISPATCH_LIMIT: u32 = 65_535;

/// The `(gx, gy)` workgroup grid of the stopping pack over `pixels`: `gx` groups of
/// [`WORKGROUP`] per row (at most `DISPATCH_LIMIT`), as many rows as the rest needs. The shader
/// strides a row by `gx × WORKGROUP` invocations (`num_workgroups.x`).
fn stopping_dispatch(pixels: u64) -> Result<(u32, u32), PtError> {
    let too_large = || PtError::Scene("the image is too large for one stopping dispatch".into());
    let groups = u32::try_from(pixels.div_ceil(u64::from(WORKGROUP))).map_err(|_| too_large())?;
    let gx = groups.clamp(1, DISPATCH_LIMIT);
    let gy = groups.div_ceil(gx);
    if gy > DISPATCH_LIMIT {
        return Err(too_large());
    }
    Ok((gx, gy))
}

/// WGSL that copies AOV 0 and the last AOV (the second moment) into a packed buffer.
fn stopping_pack_shader() -> String {
    format!(
        "\
@group(0) @binding(0) var<storage, read> accum: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> packed: array<vec4<f32>>;

@compute @workgroup_size({WORKGROUP})
fn pack_stopping(@builtin(global_invocation_id) id: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>) {{
    // One row of the 2-D dispatch is `nwg.x` groups of {WORKGROUP} invocations.
    let p = id.x + id.y * nwg.x * {WORKGROUP}u;
    let n = arrayLength(&packed) / 2u;
    if (p >= n) {{
        return;
    }}
    packed[p * 2u] = accum[p * {aov}u];
    packed[p * 2u + 1u] = accum[p * {aov}u + {moment}u];
}}
",
        aov = AOV_COUNT,
        moment = AOV_COUNT - 1,
    )
}

/// Graph kernel sets a tracer keeps; past this the cache starts over.
const GRAPH_KERNEL_CACHE: usize = 16;

/// Composes `source` and builds the six entry points against `pipeline_layout`.
fn build_kernels(
    device: &RenderDevice,
    pipeline_layout: &wgpu::PipelineLayout,
    source: &str,
    label: &str,
) -> Result<[wgpu::ComputePipeline; 6], PtError> {
    let module = regia_render::compose_source(source, "pt.wgsl", HashMap::new())
        .map_err(|e| PtError::Gpu(e.to_string()))?;
    let d = &device.device;
    let allowed = d.limits().max_compute_workgroup_storage_size;
    if allowed < TRACE_STACK_BYTES {
        return Err(PtError::Gpu(format!(
            "the path tracer's traversal stacks need {TRACE_STACK_BYTES} bytes of workgroup memory and this device allows {allowed}"
        )));
    }
    let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
    let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
    });
    let make = |entry: &str| {
        d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: Some(pipeline_layout),
            module: &shader,
            entry_point: Some(entry),
            // `trace_stack` is the only workgroup memory and every entry is written before it is
            // read, so the zero fill (and its barrier) at each kernel's start is skipped.
            compilation_options: wgpu::PipelineCompilationOptions {
                zero_initialize_workgroup_memory: false,
                ..Default::default()
            },
            cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
        })
    };
    let kernels = [
        make("generate"),
        make("intersect"),
        make("shade"),
        make("accumulate"),
        make("probe_accumulate"),
        make("queue_dispatch"),
    ];
    if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
        return Err(PtError::Gpu(err.to_string()));
    }
    Ok(kernels)
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

fn uniform(binding: u32, dynamic: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: None,
        },
        count: None,
    }
}

impl PathTracer {
    /// Compiles the kernels.
    ///
    /// # Errors
    ///
    /// Shader composition or pipeline creation failed.
    pub fn new(device: &RenderDevice) -> Result<Self, PtError> {
        let _span = tracing::info_span!("pt_compile").entered();
        let d = &device.device;
        let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut entries = vec![uniform(0, false), uniform(1, true)];
        for b in 2..=10 {
            entries.push(storage(b, true));
        }
        for b in 11..=15 {
            entries.push(storage(b, false));
        }
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("regia_pt"),
            entries: &entries,
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("regia_pt"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            return Err(PtError::Gpu(err.to_string()));
        }
        let [
            generate,
            intersect,
            shade,
            accumulate,
            probe_accumulate,
            queue_dispatch,
        ] = build_kernels(device, &pipeline_layout, SOURCE, "regia_pt")?;
        let align = u64::from(device.device.limits().min_uniform_buffer_offset_alignment.max(16));
        Ok(Self {
            device: device.clone(),
            layout,
            generate,
            intersect,
            shade,
            accumulate,
            probe_accumulate,
            queue_dispatch,
            param_stride: align,
            pipeline_layout,
            graph_kernels: Mutex::new(HashMap::new()),
            graph_compiles: std::sync::atomic::AtomicU64::new(0),
            probe_scene: Mutex::new(ProbeHold::default()),
            probe_packs: std::sync::atomic::AtomicU64::new(0),
            probe_uploads: std::sync::atomic::AtomicU64::new(0),
            stopping_pack: OnceLock::new(),
        })
    }

    /// Scene packs performed for probe traces. The same scene packs once (ADR-5154).
    #[must_use]
    pub fn probe_scene_packs(&self) -> u64 {
        self.probe_packs.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Scene uploads performed for probe traces. A later trace reuses the buffers when they
    /// are large enough (ADR-5154).
    #[must_use]
    pub fn probe_scene_uploads(&self) -> u64 {
        self.probe_uploads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bottom-level mesh BVHs built while packing probe scenes (a cache hit does not count).
    #[must_use]
    pub fn probe_blas_built(&self) -> u64 {
        self.probe_scene
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cache
            .blas_built()
    }

    /// The compute pass that packs beauty and the second moment out of the accumulation buffer.
    fn stopping_pack(&self) -> Result<&StoppingPack, PtError> {
        // Clone the device before the lock is borrowed, so the builder does not re-borrow `self`.
        let device = self.device.clone();
        let built = self.stopping_pack.get_or_init(|| StoppingPack::build(&device));
        built.as_ref().map_err(|e| PtError::Gpu(e.clone()))
    }

    /// Kernel sets compiled with material graphs composed in, over this tracer's life (a set is
    /// cached by its graphs, so re-tracing the same graphs compiles nothing; ADR-2725).
    #[must_use]
    pub fn graph_kernel_compiles(&self) -> u64 {
        self.graph_compiles.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The kernels for `set` and the scene's decals: `None` for a scene without composed graphs
    /// or decals (the base kernels).
    fn kernels_for(
        &self,
        set: &crate::graph::GraphSet,
        decals: bool,
    ) -> Result<Option<Arc<Kernels>>, PtError> {
        let graph_key = set.key();
        if graph_key == 0 && !decals {
            return Ok(None);
        }
        // A decal set keys apart from the same graphs without decals (ADR-2951).
        let key = if decals {
            graph_key ^ 0x9e37_79b9_7f4a_7c15
        } else {
            graph_key
        };
        if let Ok(map) = self.graph_kernels.lock()
            && let Some(found) = map.get(&key)
        {
            return Ok(Some(found.clone()));
        }
        let _span = tracing::info_span!("pt_compile_graphs", graphs = set.sources.len(), decals).entered();
        let mut source = if graph_key == 0 {
            SOURCE.to_owned()
        } else {
            crate::graph::composed_source(SOURCE, set).map_err(PtError::Gpu)?
        };
        if decals {
            source = crate::decal::composed_source(&source).map_err(PtError::Gpu)?;
        }
        let [
            generate,
            intersect,
            shade,
            accumulate,
            _probe_accumulate,
            _queue_dispatch,
        ] = build_kernels(&self.device, &self.pipeline_layout, &source, "regia_pt variant")?;
        self.graph_compiles
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let kernels = Arc::new(Kernels {
            generate,
            intersect,
            shade,
            accumulate,
        });
        if let Ok(mut map) = self.graph_kernels.lock() {
            if map.len() >= GRAPH_KERNEL_CACHE {
                map.clear();
            }
            map.insert(key, kernels.clone());
        }
        Ok(Some(kernels))
    }

    /// The device this tracer runs on.
    #[must_use]
    pub fn device(&self) -> &RenderDevice {
        &self.device
    }

    /// Uploads `scene` and returns a progressive session (nothing traced yet).
    ///
    /// # Errors
    ///
    /// The scene is malformed or a buffer exceeds the device's limits.
    pub fn begin(&self, scene: &PtScene, settings: &PtSettings) -> Result<PtSession<'_>, PtError> {
        self.begin_with_quality(scene, settings, &PtQuality::default())
    }

    /// [`Self::begin`] with the biased light-transport limits of `quality` (caustics mode and
    /// clamps; ADR-2433). The default [`PtQuality`] is the base tracer: the same image bit for bit.
    ///
    /// # Errors
    ///
    /// See [`Self::begin`]; also a clamp that is NaN, infinite or negative.
    pub fn begin_with_quality(
        &self,
        scene: &PtScene,
        settings: &PtSettings,
        quality: &PtQuality,
    ) -> Result<PtSession<'_>, PtError> {
        quality.validate()?;
        let packed = pack_with(scene, None)?;
        PtSession::new(self, packed, scene.content_hash(), settings, quality)
    }

    /// [`Self::begin`] reusing the bottom-level BVHs `cache` holds from earlier traces (meshes
    /// are matched by content), so re-tracing a world from a moved camera or with moved
    /// instances rebuilds only the top level. The cache keeps what this scene uses.
    ///
    /// # Errors
    ///
    /// See [`Self::begin`].
    pub fn begin_cached(
        &self,
        scene: &PtScene,
        settings: &PtSettings,
        cache: &mut PtCache,
    ) -> Result<PtSession<'_>, PtError> {
        self.begin_cached_with_quality(scene, settings, cache, &PtQuality::default())
    }

    /// [`Self::begin_cached`] with the limits of `quality` (see [`Self::begin_with_quality`]).
    ///
    /// # Errors
    ///
    /// See [`Self::begin_with_quality`].
    pub fn begin_cached_with_quality(
        &self,
        scene: &PtScene,
        settings: &PtSettings,
        cache: &mut PtCache,
        quality: &PtQuality,
    ) -> Result<PtSession<'_>, PtError> {
        quality.validate()?;
        let packed = pack_with(scene, Some(cache))?;
        PtSession::new(self, packed, scene.content_hash(), settings, quality)
    }

    /// Traces `scene` to `settings.spp` samples and returns the image.
    ///
    /// # Errors
    ///
    /// See [`Self::begin`], [`PtSession::run`], [`PtSession::image`].
    pub fn render(&self, scene: &PtScene, settings: &PtSettings) -> Result<PtImage, PtError> {
        let mut session = self.begin(scene, settings)?;
        session.run(settings.spp)?;
        session.image()
    }
}

/// An uploaded scene being refined sample by sample.
pub struct PtSession<'a> {
    tracer: &'a PathTracer,
    bind_group: wgpu::BindGroup,
    params: wgpu::Buffer,
    /// Pass records the params buffer can hold. Grown when one submission covers several ranges.
    param_slots: u64,
    /// Binding 0, kept so a grown params buffer can be rebound.
    globals: wgpu::Buffer,
    /// Bindings 2..=15, in upload order.
    scene_buffers: Vec<wgpu::Buffer>,
    accum: wgpu::Buffer,
    /// Reused full-frame readback (`MAP_READ`).
    staging: Mutex<Option<wgpu::Buffer>>,
    /// Reused packed beauty + second moment (`STORAGE | COPY_SRC`).
    pack_buf: Mutex<Option<wgpu::Buffer>>,
    /// Reused stopping readback (`MAP_READ`).
    pack_staging: Mutex<Option<wgpu::Buffer>>,
    /// `MAP_READ` buffers this session has created.
    staging_allocs: AtomicU64,
    /// Held for a whole readback ([`PtSession::image`], `image_stopping`): the staging slots'
    /// own locks cover only the lookup, so two threads reading back one session mapped the same
    /// buffer at once, a wgpu validation error (corr-pt LOW 11).
    readback: Mutex<()>,
    settings: PtSettings,
    width: u32,
    height: u32,
    samples: u32,
    started: Instant,
    scene_hash: u64,
    triangles: u64,
    /// Kernels with the scene's material graphs; `None` = the tracer's base kernels.
    kernels: Option<Arc<Kernels>>,
    /// The scene's graphs as traced (ADR-2725).
    graph_stats: crate::graph::PtGraphStats,
    /// Submissions recorded by [`Self::run_ranges`].
    submits: AtomicU64,
    /// `device.poll(wait_indefinitely)` calls on this session.
    device_polls: AtomicU64,
    /// Bytes copied back to the CPU by [`Self::image`] and the stopping readback.
    readback_bytes: AtomicU64,
    /// Full accumulation readbacks.
    full_readbacks: AtomicU64,
    /// Beauty-and-variance readbacks used by adaptive stopping.
    stopping_readbacks: AtomicU64,
}

fn buffer_or_dummy<T: bytemuck::Pod>(d: &wgpu::Device, label: &str, data: &[T]) -> wgpu::Buffer {
    let bytes: &[u8] = bytemuck::cast_slice(data);
    let contents: Cow<'_, [u8]> = if bytes.is_empty() {
        Cow::Owned(vec![0u8; 256])
    } else {
        Cow::Borrowed(bytes)
    };
    d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: &contents,
        usage: wgpu::BufferUsages::STORAGE,
    })
}

/// GPU copies of a packed scene plus the per-trace buffers, bound for the kernels.
pub(crate) struct Uploaded {
    pub bind_group: wgpu::BindGroup,
    /// Globals (`COPY_DST`: probe batches rewrite them).
    pub globals: wgpu::Buffer,
    /// Per-pass records (`Pass`), `records` of them at the tracer's stride.
    pub params: wgpu::Buffer,
    /// The `env` words (`COPY_DST`: probe batches write their probe records into it).
    pub env: wgpu::Buffer,
    /// The accumulation buffer (`COPY_SRC` for readback, `COPY_DST` for clears).
    pub accum: wgpu::Buffer,
    /// Queue lengths and the next compacted bounce's workgroup counts (`queue_count` in
    /// `pt.wgsl`, words 2..5 at `QUEUE_ARGS_OFFSET`; `COPY_SRC`).
    pub queue_count: wgpu::Buffer,
    /// The indirect dispatch arguments of a compacted bounce, copied from `queue_count`.
    pub indirect: wgpu::Buffer,
    /// Bindings 2..=15, in order, so a larger params buffer can be rebound.
    pub scene_buffers: Vec<wgpu::Buffer>,
}

/// Byte offset of the indirect dispatch arguments in `Uploaded::queue_count`.
pub(crate) const QUEUE_ARGS_OFFSET: u64 = 8;

/// Uploads `packed` with room for `paths` path slots, an `accum_bytes` accumulator,
/// `records` pass records and, when `compact` (probe traces, ADR-862), the two path queues.
pub(crate) fn upload(
    tracer: &PathTracer,
    packed: &Packed,
    paths: u64,
    accum_bytes: u64,
    records: u64,
    compact: bool,
) -> Result<Uploaded, PtError> {
    let d = &tracer.device.device;
    let max_binding = d.limits().max_storage_buffer_binding_size;
    // Unused without compaction (the kernels never touch them): a stub binding.
    let queue_bytes = if compact { paths * 8 } else { 16 };
    let path_bytes = paths * PATH_VEC4S * 16;
    let sizes = [
        ("nodes", std::mem::size_of_val(packed.nodes.as_slice()) as u64),
        ("tri_pos", std::mem::size_of_val(packed.tri_pos.as_slice()) as u64),
        (
            "tri_attr",
            std::mem::size_of_val(packed.tri_attr.as_slice()) as u64,
        ),
        (
            "textures",
            std::mem::size_of_val(packed.textures.as_slice()) as u64,
        ),
        ("terrain", std::mem::size_of_val(packed.terrain.as_slice()) as u64),
        ("env", std::mem::size_of_val(packed.env.as_slice()) as u64),
        ("paths", path_bytes),
        ("queues", queue_bytes),
        ("accum", accum_bytes),
    ];
    for (name, size) in sizes {
        if size > max_binding {
            return Err(PtError::Scene(format!(
                "{name} needs {size} bytes; the device binds at most {max_binding}"
            )));
        }
    }
    let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
    let globals = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("pt globals"),
        contents: bytemuck::bytes_of(&packed.globals),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    let params = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pt pass params"),
        size: records.max(1) * tracer.param_stride,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let nodes = buffer_or_dummy(d, "pt nodes", &packed.nodes);
    let tri_pos = buffer_or_dummy(d, "pt tri pos", &packed.tri_pos);
    let tri_attr = buffer_or_dummy(d, "pt tri attr", &packed.tri_attr);
    let instances = buffer_or_dummy(d, "pt instances", &packed.instances);
    let materials = buffer_or_dummy(d, "pt materials", &packed.materials);
    let textures = buffer_or_dummy(d, "pt textures", &packed.textures);
    let lights = buffer_or_dummy(d, "pt lights", &packed.lights);
    let env = {
        let bytes: &[u8] = bytemuck::cast_slice(&packed.env);
        let contents: Cow<'_, [u8]> = if bytes.is_empty() {
            Cow::Owned(vec![0u8; 256])
        } else {
            Cow::Borrowed(bytes)
        };
        d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pt env"),
            contents: &contents,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        })
    };
    let terrain = buffer_or_dummy(d, "pt terrain", &packed.terrain);
    let path_buf = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pt paths"),
        size: path_bytes.max(16),
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let hits = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pt hits"),
        size: (paths * 32).max(16),
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let accum = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pt accum"),
        size: accum_bytes.max(16),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    // Two queues of path slots for compacted bounces (ADR-862), their lengths and the indirect
    // arguments derived from them.
    let queues = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pt path queues"),
        size: queue_bytes.max(16),
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let queue_count = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pt queue counts"),
        size: 32,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let indirect = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pt queue dispatch"),
        size: 16,
        usage: wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let pass_binding = wgpu::BufferBinding {
        buffer: &params,
        offset: 0,
        size: wgpu::BufferSize::new(16),
    };
    let buffers = [
        &nodes,
        &tri_pos,
        &tri_attr,
        &instances,
        &materials,
        &textures,
        &lights,
        &env,
        &terrain,
        &path_buf,
        &hits,
        &accum,
        &queues,
        &queue_count,
    ];
    let scene_buffers: Vec<wgpu::Buffer> = buffers.iter().map(|b| (*b).clone()).collect();
    let mut entries = vec![
        wgpu::BindGroupEntry {
            binding: 0,
            resource: globals.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: wgpu::BindingResource::Buffer(pass_binding),
        },
    ];
    for (i, b) in buffers.iter().enumerate() {
        entries.push(wgpu::BindGroupEntry {
            binding: 2 + i as u32,
            resource: b.as_entire_binding(),
        });
    }
    let bind_group = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("regia_pt"),
        layout: &tracer.layout,
        entries: &entries,
    });
    if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
        return Err(PtError::Gpu(err.to_string()));
    }
    Ok(Uploaded {
        bind_group,
        globals,
        params,
        env,
        accum,
        queue_count,
        indirect,
        scene_buffers,
    })
}

/// Integers an f32 holds exactly. The coherent-order offset is read as an f32 (ADR-862).
const PROBE_ORDER_EXACT: usize = 1 << 24;

/// What one probe trace needs from the uploaded scene.
pub(crate) struct ProbeNeed {
    /// Probes in the largest batch of this call.
    pub batch: usize,
    /// Rays per probe.
    pub rays: u32,
    /// Path slots (`batch * rays`).
    pub paths: u64,
    /// Accumulation buffer bytes for one batch.
    pub accum_bytes: u64,
    /// Samples written in one submission.
    pub per_submit: u32,
    /// Bounces this call dispatches.
    pub bounces: u32,
    /// This call wants the coherent ray-order table.
    pub coherent: bool,
}

/// Buffer sizes and the env layout of one probe upload.
#[derive(Clone, Copy, Debug, Default)]
struct ProbeLayout {
    env_base: usize,
    order_offset: usize,
    /// Coherent-table floats reserved after `order_offset` (0 when the offset is not exact).
    order_words: u32,
    layout_batch: usize,
    layout_rays: u32,
    /// Ray count whose table is currently in the env buffer.
    table_rays: u32,
    paths: u64,
    accum_bytes: u64,
    records: u64,
}

impl ProbeLayout {
    fn coherent_for(&self, need: &ProbeNeed) -> bool {
        need.coherent
            && need.rays > 1
            && self.order_offset + (need.rays as usize) < PROBE_ORDER_EXACT
            && self.order_words >= need.rays
    }

    /// The stored upload can serve `need` with the same coherent-table decision a fresh
    /// upload of this batch would make.
    fn reusable(&self, need: &ProbeNeed, env_base: usize) -> bool {
        if self.env_base != env_base
            || self.layout_batch < need.batch
            || self.layout_rays < need.rays
            || self.paths < need.paths
            || self.accum_bytes < need.accum_bytes
            || self.records < u64::from(need.per_submit.max(1)) * u64::from(need.bounces.max(1))
        {
            return false;
        }
        let fresh_offset = env_base + need.batch * 4;
        let fresh_ok =
            need.coherent && need.rays > 1 && fresh_offset + (need.rays as usize) < PROBE_ORDER_EXACT;
        fresh_ok == self.coherent_for(need)
    }
}

/// CPU pack plus the GPU buffers kept for the next probe trace of the same scene.
#[derive(Default)]
struct ProbeHold {
    hash: u64,
    cache: PtCache,
    base: Option<Packed>,
    uploaded: Option<Uploaded>,
    layout: ProbeLayout,
}

/// A probe trace's scene buffers. Dropping it puts the packed scene back on the tracer.
pub(crate) struct ProbeLease<'a> {
    tracer: &'a PathTracer,
    hash: u64,
    packed: Option<Packed>,
    uploaded: Option<Uploaded>,
    base_globals: Globals,
    /// Scene words in `env` before the probe records.
    pub(crate) env_base: usize,
    /// `env` index of the coherent ray-order table.
    pub(crate) order_offset: usize,
    /// The coherent table is live for this call (the same decision as packing this call alone).
    pub(crate) coherent: bool,
    layout: ProbeLayout,
}

impl ProbeLease<'_> {
    /// The uploaded buffers.
    pub(crate) fn uploaded(&self) -> &Uploaded {
        match &self.uploaded {
            Some(up) => up,
            None => unreachable!("probe buffers already released"),
        }
    }

    /// Scene globals from the pack, before this call's probe fields.
    pub(crate) fn scene_globals(&self) -> Globals {
        self.base_globals
    }

    /// Material graphs in the packed scene (baked with their standard factors).
    pub(crate) fn graph_materials(&self) -> u32 {
        self.packed.as_ref().map_or(0, |p| p.graphs.stats.materials)
    }
}

impl Drop for ProbeLease<'_> {
    fn drop(&mut self) {
        let Some(mut packed) = self.packed.take() else {
            return;
        };
        packed.globals = self.base_globals;
        packed.env.truncate(self.env_base);
        let mut hold = self.tracer.probe_scene.lock().unwrap_or_else(|e| e.into_inner());
        if hold.hash != self.hash {
            return;
        }
        hold.base = Some(packed);
        if let Some(up) = self.uploaded.take() {
            hold.uploaded = Some(up);
            hold.layout = self.layout;
        }
    }
}

impl PathTracer {
    /// Packs `scene` at most once per content hash and uploads buffers large enough for a
    /// later probe trace of the same scene (ADR-5154). The lease restores the packed scene
    /// on drop, including when this call returns an error.
    pub(crate) fn lease_probe_scene(
        &self,
        scene: &PtScene,
        need: ProbeNeed,
    ) -> Result<ProbeLease<'_>, PtError> {
        let hash = scene.content_hash();
        let mut hold = self.probe_scene.lock().unwrap_or_else(|e| e.into_inner());
        if hold.hash != hash || hold.base.is_none() {
            let packed = pack_with(scene, Some(&mut hold.cache))?;
            self.probe_packs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            hold.hash = hash;
            hold.base = Some(packed);
            hold.uploaded = None;
            hold.layout = ProbeLayout::default();
        }
        let Some(mut packed) = hold.base.take() else {
            return Err(PtError::Scene("probe scene was not packed".into()));
        };
        let env_base = packed.env.len();
        if hold.layout.reusable(&need, env_base)
            && let Some(uploaded) = hold.uploaded.take()
        {
            let mut layout = hold.layout;
            let base_globals = packed.globals;
            drop(hold);
            self.rewrite_probe_order(&uploaded, &mut layout, &need);
            let coherent = layout.coherent_for(&need);
            return Ok(ProbeLease {
                tracer: self,
                hash,
                packed: Some(packed),
                uploaded: Some(uploaded),
                base_globals,
                env_base,
                order_offset: layout.order_offset,
                coherent,
                layout,
            });
        }
        hold.uploaded = None;
        drop(hold);

        let order_offset = env_base + need.batch * 4;
        let order_words = if need.rays > 1 && order_offset + (need.rays as usize) < PROBE_ORDER_EXACT {
            need.rays
        } else {
            0
        };
        packed.env.resize(order_offset + order_words as usize, 0.0);
        if order_words > 0 {
            for (i, ray) in crate::probe_rays::coherent_ray_order(need.rays)
                .into_iter()
                .enumerate()
            {
                packed.env[order_offset + i] = ray as f32;
            }
        }
        let records = u64::from(need.per_submit.max(1)) * 64;
        let accum_bytes = need.accum_bytes.max(16);
        let uploaded = match upload(self, &packed, need.paths, accum_bytes, records, true) {
            Ok(up) => up,
            Err(err) => {
                packed.env.truncate(env_base);
                let mut hold = self.probe_scene.lock().unwrap_or_else(|e| e.into_inner());
                if hold.hash == hash {
                    hold.base = Some(packed);
                }
                return Err(err);
            }
        };
        self.probe_uploads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let layout = ProbeLayout {
            env_base,
            order_offset,
            order_words,
            layout_batch: need.batch,
            layout_rays: need.rays,
            table_rays: order_words,
            paths: need.paths,
            accum_bytes,
            records,
        };
        let coherent = layout.coherent_for(&need);
        let base_globals = packed.globals;
        Ok(ProbeLease {
            tracer: self,
            hash,
            packed: Some(packed),
            uploaded: Some(uploaded),
            base_globals,
            env_base,
            order_offset: layout.order_offset,
            coherent,
            layout,
        })
    }

    /// Writes this call's coherent-order table when the buffer holds a different ray count.
    fn rewrite_probe_order(&self, uploaded: &Uploaded, layout: &mut ProbeLayout, need: &ProbeNeed) {
        if !layout.coherent_for(need) || layout.table_rays == need.rays {
            return;
        }
        let words: Vec<f32> = crate::probe_rays::coherent_ray_order(need.rays)
            .into_iter()
            .map(|i| i as f32)
            .collect();
        self.device.queue.write_buffer(
            &uploaded.env,
            (layout.order_offset * 4) as u64,
            bytemuck::cast_slice(&words),
        );
        layout.table_rays = need.rays;
    }
}

impl<'a> PtSession<'a> {
    fn new(
        tracer: &'a PathTracer,
        mut packed: Packed,
        scene_hash: u64,
        settings: &PtSettings,
        quality: &PtQuality,
    ) -> Result<Self, PtError> {
        let width = packed.globals.dims[0];
        let height = packed.globals.dims[1];
        let pixels = u64::from(width) * u64::from(height);
        let settings = PtSettings {
            spp: settings.spp,
            max_bounces: settings.max_bounces.clamp(1, MAX_PATH_VERTICES),
            seed: settings.seed,
            rr_start: settings.rr_start.max(1),
            jitter: settings.jitter,
            samples_per_submit: settings.samples_per_submit.clamp(1, 256),
        };
        packed.globals.dims[2] = settings.max_bounces;
        packed.globals.dims[3] = settings.seed;
        packed.globals.counts[2] = settings.rr_start;
        // Caustics mode, indirect clamp and caustic cap (zeros are the base tracer).
        packed.globals.misc[3] = quality.caustics.shader_code();
        packed.globals.media_f[1] = quality.clamp_indirect;
        packed.globals.media_f[2] = quality.caustic_cap();
        if settings.jitter {
            packed.globals.counts[3] |= FLAG_JITTER;
        }
        let accum_bytes = pixels * AOV_COUNT as u64 * 16;
        let records = u64::from(settings.samples_per_submit) * u64::from(settings.max_bounces);
        let kernels = tracer.kernels_for(&packed.graphs, packed.decals)?;
        let graph_stats = packed.graphs.stats.clone();
        let up = upload(tracer, &packed, pixels, accum_bytes, records, false)?;
        tracing::info!(
            target: "regia_pt",
            width,
            height,
            triangles = packed.triangle_count,
            instances = packed.instances.len(),
            lights = packed.lights.len(),
            graphs = graph_stats.graphs,
            graph_refusals = graph_stats.refusals.len(),
            "path tracer scene uploaded"
        );
        Ok(Self {
            tracer,
            bind_group: up.bind_group,
            params: up.params,
            param_slots: records.max(1),
            globals: up.globals,
            scene_buffers: up.scene_buffers,
            accum: up.accum,
            staging: Mutex::new(None),
            pack_buf: Mutex::new(None),
            pack_staging: Mutex::new(None),
            staging_allocs: AtomicU64::new(0),
            readback: Mutex::new(()),
            settings,
            width,
            height,
            samples: 0,
            started: Instant::now(),
            scene_hash,
            triangles: packed.triangle_count as u64,
            kernels,
            graph_stats,
            submits: AtomicU64::new(0),
            device_polls: AtomicU64::new(0),
            readback_bytes: AtomicU64::new(0),
            full_readbacks: AtomicU64::new(0),
            stopping_readbacks: AtomicU64::new(0),
        })
    }

    /// GPU submissions [`Self::run_ranges`] has recorded.
    #[must_use]
    pub fn submit_count(&self) -> u64 {
        self.submits.load(Ordering::Relaxed)
    }

    /// Times this session has polled the device until idle.
    #[must_use]
    pub fn device_polls(&self) -> u64 {
        self.device_polls.load(Ordering::Relaxed)
    }

    /// Bytes copied from the GPU by image readbacks on this session.
    #[must_use]
    pub fn readback_bytes(&self) -> u64 {
        self.readback_bytes.load(Ordering::Relaxed)
    }

    /// Full accumulation readbacks ([`Self::image`]).
    #[must_use]
    pub fn full_readbacks(&self) -> u64 {
        self.full_readbacks.load(Ordering::Relaxed)
    }

    /// Beauty and variance readbacks used while adaptive sampling decides which bands to stop.
    #[must_use]
    pub fn stopping_readbacks(&self) -> u64 {
        self.stopping_readbacks.load(Ordering::Relaxed)
    }

    /// `MAP_READ` staging buffers this session has created.
    ///
    /// A full readback and a stopping readback each keep one buffer and reuse it.
    #[must_use]
    pub fn readback_staging_allocs(&self) -> u64 {
        self.staging_allocs.load(Ordering::Relaxed)
    }

    /// Samples accumulated so far.
    #[must_use]
    pub fn samples_done(&self) -> u32 {
        self.samples
    }

    /// The tracer this session runs on.
    #[must_use]
    pub fn tracer(&self) -> &'a PathTracer {
        self.tracer
    }

    /// Image size in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The (clamped) settings in effect.
    #[must_use]
    pub fn settings(&self) -> &PtSettings {
        &self.settings
    }

    /// Queues `samples` more samples per pixel.
    ///
    /// Submissions stay on the queue. The next [`Self::image`] waits until that work and the
    /// readback finish.
    ///
    /// # Errors
    ///
    /// A GPU validation error.
    pub fn run(&mut self, samples: u32) -> Result<(), PtError> {
        self.run_with(samples, |_| true)
    }

    /// [`Self::run`] calling `progress(samples_done)` after every submission; returning `false`
    /// stops early (cancellation).
    ///
    /// # Errors
    ///
    /// See [`Self::run`].
    pub fn run_with(&mut self, samples: u32, progress: impl FnMut(u32) -> bool) -> Result<(), PtError> {
        let groups = (self.width * self.height).div_ceil(WORKGROUP);
        self.run_ranges(samples, &[(0, groups)], progress)
    }

    /// Workgroups (runs of 64 consecutive pixels in row-major order) in the image.
    #[must_use]
    pub fn workgroups(&self) -> u32 {
        (self.width * self.height).div_ceil(WORKGROUP)
    }

    /// Accumulates `samples` more samples for the pixels of the given workgroup ranges only
    /// (`(first, count)` pairs): the other pixels keep their sample count, which is how adaptive
    /// sampling stops converged bands. [`Self::samples_done`] counts the sample indices spent,
    /// not the per-pixel count (the `spp_map` of [`Self::image`] has that).
    ///
    /// Every live range of one sample batch is one submission. The params buffer grows so each
    /// range reads its own pass records; a batch that would not fit is split. The device is not
    /// polled here.
    ///
    /// # Errors
    ///
    /// A GPU validation error, or a range past the image.
    pub fn run_ranges(
        &mut self,
        samples: u32,
        ranges: &[(u32, u32)],
        mut progress: impl FnMut(u32) -> bool,
    ) -> Result<(), PtError> {
        let _span = tracing::info_span!("pt_run", samples, ranges = ranges.len()).entered();
        let total_groups = self.workgroups();
        if ranges
            .iter()
            .any(|&(first, count)| u64::from(first) + u64::from(count) > u64::from(total_groups))
        {
            return Err(PtError::Scene("workgroup range past the image".into()));
        }
        let mut remaining = samples;
        let mut param_bytes = Vec::new();
        while remaining > 0 {
            let batch = remaining.min(self.settings.samples_per_submit);
            // Each range is cut into dispatches that cover exactly its workgroups (a
            // `min(count, 65535) x ceil(count / 65535)` rectangle overhangs a range that is not a
            // multiple of 65535). Every cut is one live entry: `(first_group, gx * gy)` maps back to
            // the same `gx, gy` in `submit_live_batch`.
            let live: Vec<(u32, u32)> = ranges
                .iter()
                .flat_map(|&(first, count)| workgroup_dispatches(first, count))
                .map(|split| (split.first_group, split.gx * split.gy))
                .collect();
            if !live.is_empty() {
                let per = u64::from(batch) * u64::from(self.settings.max_bounces);
                let needed = u64::try_from(live.len()).unwrap_or(u64::MAX).saturating_mul(per);
                // Grow before any pipeline borrow: rebuilding the bind group needs `&mut self`.
                self.ensure_param_records(needed)?;
                self.submit_live_batch(batch, &live, per, &mut param_bytes)?;
            }
            self.samples += batch;
            remaining -= batch;
            if !progress(self.samples) {
                break;
            }
        }
        Ok(())
    }

    /// One submission per chunk of `live`. Each range gets its own pass records so two ranges
    /// in one submit do not overwrite each other's dynamic-offset block.
    fn submit_live_batch(
        &self,
        batch: u32,
        live: &[(u32, u32)],
        per: u64,
        param_bytes: &mut Vec<u8>,
    ) -> Result<(), PtError> {
        let kernels = self.kernels.clone();
        let tracer = self.tracer;
        let (generate, intersect, shade, accumulate) = match &kernels {
            Some(k) => (&k.generate, &k.intersect, &k.shade, &k.accumulate),
            None => (
                &tracer.generate,
                &tracer.intersect,
                &tracer.shade,
                &tracer.accumulate,
            ),
        };
        let d = &tracer.device.device;
        let q = &tracer.device.queue;
        let bounces = self.settings.max_bounces;
        let stride = tracer.param_stride;
        let fit = (self.param_slots / per.max(1)).max(1) as usize;
        let sample0 = self.samples;
        for chunk in live.chunks(fit) {
            let records = chunk.len() as u64 * per;
            param_bytes.clear();
            param_bytes.resize((records * stride) as usize, 0);
            for (ri, &(first, _)) in chunk.iter().enumerate() {
                for k in 0..batch {
                    for b in 0..bounces {
                        let record = ri as u64 * per + u64::from(k) * u64::from(bounces) + u64::from(b);
                        let at = (record * stride) as usize;
                        param_bytes[at..at + 4].copy_from_slice(&(sample0 + k).to_le_bytes());
                        param_bytes[at + 4..at + 8].copy_from_slice(&b.to_le_bytes());
                        param_bytes[at + 12..at + 16].copy_from_slice(&first.to_le_bytes());
                    }
                }
            }
            q.write_buffer(&self.params, 0, param_bytes);
            let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("regia_pt"),
            });
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("regia_pt samples"),
                    timestamp_writes: None,
                });
                for (ri, &(_, count)) in chunk.iter().enumerate() {
                    let gx = count.clamp(1, 65_535);
                    let gy = count.div_ceil(gx);
                    for k in 0..batch {
                        let base = (ri as u64 * per + u64::from(k) * u64::from(bounces)) * stride;
                        let offset = |b: u32| (base + u64::from(b) * stride) as u32;
                        pass.set_pipeline(generate);
                        pass.set_bind_group(0, &self.bind_group, &[offset(0)]);
                        pass.dispatch_workgroups(gx, gy, 1);
                        for b in 0..bounces {
                            pass.set_bind_group(0, &self.bind_group, &[offset(b)]);
                            pass.set_pipeline(intersect);
                            pass.dispatch_workgroups(gx, gy, 1);
                            pass.set_pipeline(shade);
                            pass.dispatch_workgroups(gx, gy, 1);
                        }
                        pass.set_bind_group(0, &self.bind_group, &[offset(0)]);
                        pass.set_pipeline(accumulate);
                        pass.dispatch_workgroups(gx, gy, 1);
                    }
                }
            }
            q.submit([encoder.finish()]);
            self.submits.fetch_add(1, Ordering::Relaxed);
            // Validation is recorded at submit. `pop` does not wait for the GPU (wgpu 30);
            // the next readback polls.
            if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
                return Err(PtError::Gpu(err.to_string()));
            }
        }
        Ok(())
    }

    /// Grows the params buffer so one submission can hold `needed` pass records.
    ///
    /// The cap is the device's max buffer size and the largest dynamic offset that fits in a
    /// `u32`. A request past the cap leaves the buffer at the cap; the caller splits the batch.
    fn ensure_param_records(&mut self, needed: u64) -> Result<(), PtError> {
        if needed <= self.param_slots {
            return Ok(());
        }
        let stride = self.tracer.param_stride.max(1);
        let limits = self.tracer.device.device.limits();
        let by_size = limits.max_buffer_size / stride;
        let by_offset = u64::from(u32::MAX) / stride;
        let cap = by_size.min(by_offset).max(1);
        let slots = needed.min(cap);
        if slots <= self.param_slots {
            return Ok(());
        }
        let d = &self.tracer.device.device;
        let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pt pass params"),
            size: slots * stride,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let pass_binding = wgpu::BufferBinding {
            buffer: &params,
            offset: 0,
            size: wgpu::BufferSize::new(16),
        };
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: self.globals.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Buffer(pass_binding),
            },
        ];
        for (i, buffer) in self.scene_buffers.iter().enumerate() {
            entries.push(wgpu::BindGroupEntry {
                binding: 2 + i as u32,
                resource: buffer.as_entire_binding(),
            });
        }
        let bind_group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("regia_pt"),
            layout: &self.tracer.layout,
            entries: &entries,
        });
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            return Err(PtError::Gpu(err.to_string()));
        }
        self.params = params;
        self.bind_group = bind_group;
        self.param_slots = slots;
        Ok(())
    }

    /// Reads the accumulation back and returns the means.
    ///
    /// The staging buffer is kept and reused. This waits for the samples queued by [`Self::run`].
    /// Readbacks of one session from several threads take turns.
    ///
    /// # Errors
    ///
    /// The readback failed.
    pub fn image(&self) -> Result<PtImage, PtError> {
        let _span = tracing::info_span!("pt_readback").entered();
        let _readback = self.readback.lock().unwrap_or_else(|poison| poison.into_inner());
        let size = self.accum.size();
        let staging = self.map_read_buffer(&self.staging, size, "pt readback")?;
        self.copy_to_staging(&self.accum, &staging, size)?;
        self.full_readbacks.fetch_add(1, Ordering::Relaxed);
        self.readback_bytes.fetch_add(size, Ordering::Relaxed);
        self.with_mapped(&staging, size, |raw| self.decode(raw))
    }

    /// Reads beauty and the second-moment AOV and returns their means.
    ///
    /// The other planes are empty. Adaptive stopping and the halfway snapshot use this instead of
    /// copying every AOV. [`Self::image`] is the full frame.
    ///
    /// # Errors
    ///
    /// The pack pass or the readback failed.
    pub(crate) fn image_stopping(&self) -> Result<PtImage, PtError> {
        let _span = tracing::info_span!("pt_readback_stopping").entered();
        let _readback = self.readback.lock().unwrap_or_else(|poison| poison.into_inner());
        let pixels = u64::from(self.width) * u64::from(self.height);
        let bytes = pixels * 32;
        if bytes == 0 {
            self.stopping_readbacks.fetch_add(1, Ordering::Relaxed);
            return Ok(self.decode_stopping(&[]));
        }
        let (layout, pipeline) = {
            let pack = self.tracer.stopping_pack()?;
            (pack.layout.clone(), pack.pipeline.clone())
        };
        let storage = self.exact_storage(&self.pack_buf, bytes, "pt stopping pack")?;
        let staging = self.map_read_buffer(&self.pack_staging, bytes, "pt stopping readback")?;
        // Checked before the error scope: returning here must not leave a scope unpopped.
        // One axis stops at `DISPATCH_LIMIT`; a wider frame uses `id.y` in the pack shader.
        let (gx, gy) = stopping_dispatch(pixels)?;
        let d = &self.tracer.device.device;
        let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pt stopping pack"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.accum.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: storage.as_entire_binding(),
                },
            ],
        });
        let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pt stopping readback"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("pt stopping pack"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(gx, gy, 1);
        }
        encoder.copy_buffer_to_buffer(&storage, 0, &staging, 0, bytes);
        self.tracer.device.queue.submit([encoder.finish()]);
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            return Err(PtError::Gpu(err.to_string()));
        }
        self.stopping_readbacks.fetch_add(1, Ordering::Relaxed);
        self.readback_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.with_mapped(&staging, bytes, |raw| self.decode_stopping(raw))
    }

    /// A `MAP_READ` buffer of at least `size` bytes, created once per slot.
    fn map_read_buffer(
        &self,
        slot: &Mutex<Option<wgpu::Buffer>>,
        size: u64,
        label: &'static str,
    ) -> Result<wgpu::Buffer, PtError> {
        let mut guard = slot.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(buf) = guard.as_ref()
            && buf.size() >= size
        {
            return Ok(buf.clone());
        }
        let buf = self.tracer.device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: size.max(4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.staging_allocs.fetch_add(1, Ordering::Relaxed);
        *guard = Some(buf.clone());
        Ok(buf)
    }

    /// A storage buffer of exactly `size` bytes. The stopping shader reads `arrayLength`, so a
    /// larger reused buffer would walk off the end of the accumulation.
    fn exact_storage(
        &self,
        slot: &Mutex<Option<wgpu::Buffer>>,
        size: u64,
        label: &'static str,
    ) -> Result<wgpu::Buffer, PtError> {
        let mut guard = slot.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(buf) = guard.as_ref()
            && buf.size() == size
        {
            return Ok(buf.clone());
        }
        let buf = self.tracer.device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        *guard = Some(buf.clone());
        Ok(buf)
    }

    fn copy_to_staging(&self, src: &wgpu::Buffer, staging: &wgpu::Buffer, size: u64) -> Result<(), PtError> {
        let d = &self.tracer.device.device;
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pt readback"),
        });
        encoder.copy_buffer_to_buffer(src, 0, staging, 0, size);
        self.tracer.device.queue.submit([encoder.finish()]);
        Ok(())
    }

    fn with_mapped<T>(
        &self,
        staging: &wgpu::Buffer,
        size: u64,
        f: impl FnOnce(&[[f32; 4]]) -> T,
    ) -> Result<T, PtError> {
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..size).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.tracer
            .device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| PtError::Gpu(e.to_string()))?;
        self.device_polls.fetch_add(1, Ordering::Relaxed);
        rx.recv()
            .map_err(|_| PtError::Gpu("map callback dropped".into()))?
            .map_err(|e| PtError::Gpu(e.to_string()))?;
        let out = match staging.slice(..size).get_mapped_range() {
            Ok(mapped) => {
                let raw: &[[f32; 4]] = bytemuck::cast_slice(&mapped);
                let out = f(raw);
                drop(mapped);
                out
            }
            Err(e) => {
                // The buffer stays mapped until unmap. A later readback reuses it.
                staging.unmap();
                return Err(PtError::Gpu(e.to_string()));
            }
        };
        staging.unmap();
        Ok(out)
    }

    fn decode(&self, raw: &[[f32; 4]]) -> PtImage {
        let n = (self.width * self.height) as usize;
        let mut img = PtImage {
            width: self.width,
            height: self.height,
            beauty: Vec::with_capacity(n),
            albedo: Vec::with_capacity(n),
            normal: Vec::with_capacity(n),
            depth: Vec::with_capacity(n),
            direct: Vec::with_capacity(n),
            indirect_diffuse: Vec::with_capacity(n),
            indirect_specular: Vec::with_capacity(n),
            emissive: Vec::with_capacity(n),
            volumetric: Vec::with_capacity(n),
            sky: Vec::with_capacity(n),
            shadow: Vec::with_capacity(n),
            roughness: Vec::with_capacity(n),
            variance: Vec::with_capacity(n),
            spp: self.samples,
            spp_map: Vec::new(),
            metadata: PtMetadata::default(),
        };
        let mut rejected = 0u64;
        let mut counts = Vec::with_capacity(n);
        for p in 0..n {
            let px = &raw[p * AOV_COUNT..(p + 1) * AOV_COUNT];
            // Accepted samples for this pixel (non-finite ones were dropped by the shader).
            let (beauty, var, count) = beauty_and_variance(px[0], px[10]);
            counts.push(count);
            rejected += px[7][3].max(0.0) as u64;
            let inv = if count > 0.0 { 1.0 / count } else { 0.0 };
            let rgb = |v: [f32; 4]| [v[0] * inv, v[1] * inv, v[2] * inv];
            img.beauty.push(beauty);
            img.albedo.push(rgb(px[1]));
            let nd = rgb(px[2]);
            let len = (nd[0] * nd[0] + nd[1] * nd[1] + nd[2] * nd[2]).sqrt();
            img.normal.push(if len > 0.0 {
                [nd[0] / len, nd[1] / len, nd[2] / len]
            } else {
                [0.0; 3]
            });
            img.depth.push(px[2][3] * inv);
            img.direct.push(rgb(px[3]));
            img.indirect_diffuse.push(rgb(px[4]));
            img.indirect_specular.push(rgb(px[5]));
            img.emissive.push(rgb(px[6]));
            img.volumetric.push(rgb(px[7]));
            img.sky.push(rgb(px[8]));
            img.shadow.push(px[9][0] * inv);
            img.roughness.push(px[1][3] * inv);
            img.variance.push(var);
        }
        // Adaptive sampling leaves different counts per pixel; a uniform trace reports none.
        if counts.iter().any(|&c| c != self.samples as f32) {
            img.spp_map = counts;
        }
        img.metadata = self.provenance(rejected);
        img
    }

    fn decode_stopping(&self, raw: &[[f32; 4]]) -> PtImage {
        let n = (self.width as usize).saturating_mul(self.height as usize);
        let mut beauty = Vec::with_capacity(n);
        let mut variance = Vec::with_capacity(n);
        let mut counts = Vec::with_capacity(n);
        for p in 0..n {
            let sum = raw.get(p * 2).copied().unwrap_or([0.0; 4]);
            let moment = raw.get(p * 2 + 1).copied().unwrap_or([0.0; 4]);
            let (rgb, var, count) = beauty_and_variance(sum, moment);
            beauty.push(rgb);
            variance.push(var);
            counts.push(count);
        }
        let spp_map = if counts.iter().any(|&c| c != self.samples as f32) {
            counts
        } else {
            Vec::new()
        };
        PtImage {
            width: self.width,
            height: self.height,
            beauty,
            albedo: Vec::new(),
            normal: Vec::new(),
            depth: Vec::new(),
            direct: Vec::new(),
            indirect_diffuse: Vec::new(),
            indirect_specular: Vec::new(),
            emissive: Vec::new(),
            volumetric: Vec::new(),
            sky: Vec::new(),
            shadow: Vec::new(),
            roughness: Vec::new(),
            variance,
            spp: self.samples,
            spp_map,
            metadata: self.provenance(0),
        }
    }

    fn provenance(&self, rejected: u64) -> PtMetadata {
        let info = self.tracer.device.adapter.get_info();
        PtMetadata {
            scene_hash: format!("{:016x}", self.scene_hash),
            seed: self.settings.seed,
            spp: self.samples,
            max_bounces: self.settings.max_bounces,
            device: format!("{} ({:?})", info.name, info.backend),
            git_tag: regia_core::brand::env_var("REGIA_GIT_TAG").unwrap_or_default(),
            wall_seconds: self.started.elapsed().as_secs_f64(),
            rejected_samples: rejected,
            triangles: self.triangles,
            denoiser: String::new(),
            denoise_seconds: 0.0,
            surface_graphs: self.graph_stats.clone(),
        }
    }
}

/// Beauty mean and the variance of that mean.
///
/// `sum` is accumulated radiance with the accepted sample count in `w`. `moment` is the
/// accumulated sum of squares. The variance of the mean is `(E[x²] − E[x]²) / n` with the
/// count correction the full decoder uses.
fn beauty_and_variance(sum: [f32; 4], moment: [f32; 4]) -> ([f32; 3], [f32; 3], f32) {
    let count = sum[3].max(0.0);
    let inv = if count > 0.0 { 1.0 / count } else { 0.0 };
    let beauty = [sum[0] * inv, sum[1] * inv, sum[2] * inv];
    let sq = [moment[0] * inv, moment[1] * inv, moment[2] * inv];
    let channel = |k: usize| {
        if count > 1.0 {
            ((sq[k] - beauty[k] * beauty[k]).max(0.0) * count / (count - 1.0)) / count
        } else {
            0.0
        }
    };
    (beauty, [channel(0), channel(1), channel(2)], count)
}

/// Contract rows of the path state (`regia_render::ContractRow`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<regia_render::ContractRow> {
    let members = [
        "origin",
        "dir",
        "throughput",
        "frac",
        "direct",
        "ind_d",
        "ind_s",
        "emis",
        "sky",
        "albedo",
        "normal",
        "vol",
    ];
    vec![regia_render::ContractRow::new(
        "pt.wgsl",
        "Path",
        (PATH_VEC4S * 16) as usize,
        members.iter().enumerate().map(|(i, m)| (*m, i * 16)).collect(),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `const NAME: u32 = <n>u;` of `pt.wgsl`.
    fn shader_const(name: &str) -> u32 {
        let prefix = format!("const {name}: u32 = ");
        SOURCE
            .lines()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .and_then(|v| v.trim_end_matches("u;").parse().ok())
            .unwrap_or_else(|| panic!("pt.wgsl declares {name}"))
    }

    /// The path tracer's modules (`pt.wgsl` alone and with the decal regions composed) and the
    /// GPU lightmap baker's hold no Function or Private variable with an explicit layout
    /// (ADR-7879, after ADR-7587): the gate's check (`validate_spirv_module`: the layout scan,
    /// `spirv-val` with the ADR-135 Workgroup allowlist) passes, and `spirv-val --target-env
    /// vulkan1.3` passes with no VUID allowed, as a validation layer runs it. The traversal
    /// stacks are Workgroup variables: a SPIRV-Tools release that rejects naga's `ArrayStride`
    /// there (2026.4, ADR-135) cites `VUID-StandaloneSpirv-None-10684` for them alone, which the
    /// test accepts only when that VUID is the only one cited.
    #[test]
    fn pt_and_lightmap_modules_pass_unrelaxed_spirv_val() {
        let decals = crate::decal::composed_source(SOURCE).expect("decal regions");
        let modules = [
            ("pt.wgsl", SOURCE.to_string()),
            ("pt.wgsl + decals", decals),
            (
                "lightmap_bake.wgsl",
                include_str!("../shaders/lightmap_bake.wgsl").to_string(),
            ),
        ];
        let dir = regia_test_utils::unique_temp_dir("pt-spirv-unrelaxed");
        let mut failures = Vec::new();
        for (name, source) in &modules {
            let module = regia_render::compose_source(source, name, HashMap::new())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let words = match regia_render::validate_spirv_module(name, &module) {
                Ok(words) => words,
                Err(err) => {
                    failures.push(err);
                    continue;
                }
            };
            let path = dir.join(format!("{}.spv", name.replace([' ', '+'], "_")));
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            match std::process::Command::new("spirv-val")
                .args(["--target-env", "vulkan1.3"])
                .arg(&path)
                .output()
            {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stderr),
                        String::from_utf8_lossy(&out.stdout)
                    );
                    let vuids: Vec<&str> = text
                        .split("VUID-")
                        .skip(1)
                        .map(|t| {
                            t.split(|c: char| c.is_whitespace() || c == ']')
                                .next()
                                .unwrap_or("")
                        })
                        .collect();
                    let workgroup_skew = !vuids.is_empty()
                        && vuids.iter().all(|v| *v == "StandaloneSpirv-None-10684")
                        && text.contains("Workgroup");
                    if workgroup_skew {
                        eprintln!("{name}: spirv-val cites VUID-10684 for Workgroup storage only (ADR-135)");
                    } else {
                        failures.push(format!("{name}: spirv-val --target-env vulkan1.3:\n{text}"));
                    }
                }
                // `validate_spirv_module` passed, so the gate's fallback lets this host run
                // without SPIRV-Tools: the layout scan alone checked the module.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("spirv-val not installed: {name} checked by the layout scan only");
                }
                Err(err) => panic!("spirv-val: {err}"),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The traversal stacks live in workgroup memory (ADR-7879): `trace_stack` is the only
    /// workgroup variable, holds `TRACE_SLOTS` words for each invocation of the two tracing
    /// kernels' groups, and `TRACE_SLOTS` covers the top level's stack plus the larger of a
    /// mesh's and a heightfield's (the two are never live at once). `build_kernels` checks the
    /// device against `TRACE_STACK_BYTES`.
    #[test]
    fn the_trace_stack_holds_every_invocation() {
        let module = regia_render::compose_source(SOURCE, "pt.wgsl", HashMap::new()).expect("composes");
        let mut layouter = naga::proc::Layouter::default();
        layouter.update(module.to_ctx()).expect("layout");
        let workgroup: Vec<(String, u32)> = module
            .global_variables
            .iter()
            .filter(|(_, g)| g.space == naga::AddressSpace::WorkGroup)
            .map(|(_, g)| (g.name.clone().unwrap_or_default(), layouter[g.ty].size))
            .collect();
        assert_eq!(workgroup, [("trace_stack".to_string(), TRACE_STACK_BYTES)]);
        let (bvh, hf) = (shader_const("BVH_STACK"), shader_const("HF_STACK"));
        let slots = shader_const("TRACE_SLOTS");
        assert_eq!(slots, bvh + bvh.max(hf));
        assert_eq!(shader_const("TRACE_GROUP"), WORKGROUP);
        assert_eq!(TRACE_STACK_BYTES, slots * WORKGROUP * 4);
        for entry in ["intersect", "shade"] {
            let ep = module.entry_points.iter().find(|e| e.name == entry).expect(entry);
            assert_eq!(ep.workgroup_size, [WORKGROUP, 1, 1], "{entry}");
        }
    }

    /// specs/27 § 1.1: at most 12 bounces by default (configurable up to [`MAX_PATH_VERTICES`]),
    /// Russian roulette from path vertex 3, a fixed scene seed.
    #[test]
    fn default_transport_settings_follow_the_spec() {
        let s = PtSettings::default();
        assert_eq!(s.max_bounces, 12);
        assert_eq!(s.rr_start, 3);
        assert_eq!(s.seed, 1);
        assert!(s.max_bounces <= MAX_PATH_VERTICES);
    }

    #[test]
    fn stopping_pack_shader_copies_beauty_and_the_second_moment() {
        let src = stopping_pack_shader();
        assert!(src.contains("11u"), "{src}");
        assert!(src.contains("10u"), "{src}");
        assert!(
            src.contains(&format!("id.y * nwg.x * {WORKGROUP}u")),
            "a wide frame must stride a dispatch row by its invocations: {src}"
        );
        assert!(src.contains("packed[p * 2u] = accum[p * 11u]"), "{src}");
        assert!(
            src.contains("packed[p * 2u + 1u] = accum[p * 11u + 10u]"),
            "{src}"
        );
    }

    /// A 4K frame (8,294,400 pixels) needs two dispatch rows. With the shader's row stride
    /// (`num_workgroups.x × WORKGROUP` invocations) every pixel is packed exactly once; the old
    /// stride of `DISPATCH_LIMIT` left pixels from 4,259,775 on (about half the frame) unpacked.
    #[test]
    fn stopping_pack_covers_every_pixel_of_a_4k_frame() {
        let src = stopping_pack_shader();
        assert!(src.contains("@builtin(num_workgroups) nwg"), "{src}");
        // The index the emulation below walks, as the shader writes it.
        assert!(
            src.contains(&format!("let p = id.x + id.y * nwg.x * {WORKGROUP}u;")),
            "{src}"
        );
        for (w, h) in [
            (3840u64, 2160u64),
            (7680, 4320),
            (1, 1),
            (65, 1),
            (64 * 65_535 + 1, 1),
        ] {
            let pixels = w * h;
            let (gx, gy) = stopping_dispatch(pixels).expect("dispatch");
            let row = u64::from(gx) * u64::from(WORKGROUP);
            let mut seen = vec![false; usize::try_from(pixels).expect("pixels")];
            for y in 0..u64::from(gy) {
                for x in 0..row {
                    // The shader's `p = id.x + id.y * nwg.x * WORKGROUP`.
                    let p = x + y * row;
                    if p < pixels {
                        let slot = &mut seen[p as usize];
                        assert!(!*slot, "{w}×{h}: pixel {p} packed twice");
                        *slot = true;
                    }
                }
            }
            let missed = seen.iter().filter(|s| !**s).count();
            assert_eq!(missed, 0, "{w}×{h}: {missed} pixels never packed");
        }
        assert!(stopping_dispatch(u64::from(u32::MAX) * 64 + 1).is_err());
    }

    #[test]
    fn beauty_variance_matches_the_count_correction() {
        // Two samples, 1 and 3: mean 2, E[x²] = 5, sample variance 2, variance of the mean 1.
        let (beauty, var, count) = beauty_and_variance([4.0, 0.0, 0.0, 2.0], [10.0, 0.0, 0.0, 0.0]);
        assert_eq!(count, 2.0);
        assert!((beauty[0] - 2.0).abs() < 1e-6, "{beauty:?}");
        assert!((var[0] - 1.0).abs() < 1e-5, "{var:?}");
        let (_, one, _) = beauty_and_variance([3.0, 0.0, 0.0, 1.0], [9.0, 0.0, 0.0, 0.0]);
        assert_eq!(one, [0.0; 3]);
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::{WorkgroupDispatch, workgroup_dispatches};

    fn covered_until(first: u32, dispatches: &[WorkgroupDispatch]) -> u32 {
        let mut cursor = first;
        for d in dispatches {
            assert!(
                (1..=65_535).contains(&d.gx) && (1..=65_535).contains(&d.gy),
                "dispatch axis out of range: {d:?}"
            );
            assert_eq!(d.first_group, cursor, "gap or overlap before {d:?}");
            cursor += d.gx * d.gy;
        }
        cursor
    }

    fn overlaps(a: &[WorkgroupDispatch], b: &[WorkgroupDispatch]) -> bool {
        for da in a {
            let a0 = da.first_group;
            let a1 = a0 + da.gx * da.gy;
            for db in b {
                let b0 = db.first_group;
                let b1 = b0 + db.gx * db.gy;
                if a0 < b1 && b0 < a1 {
                    return true;
                }
            }
        }
        false
    }

    /// 3840×2160 is 129_600 workgroups; `tile_rows` 16 makes a band of 960. The top 69 bands
    /// are one range of 66_240 groups, which is not a multiple of 65_535. Band 69 is the gap
    /// and band 70 runs through the last row.
    #[test]
    fn correctness_g_dispatch_covers_the_requested_groups() {
        let top = workgroup_dispatches(0, 66_240);
        let end = covered_until(0, &top);
        assert_eq!(end, 66_240, "dispatches cover 0..{end}, not 0..66240");
        let lower = workgroup_dispatches(67_200, 129_600 - 67_200);
        let lower_end = covered_until(67_200, &lower);
        assert_eq!(
            lower_end, 129_600,
            "dispatches cover 67200..{lower_end}, not 67200..129600"
        );
        assert!(
            !overlaps(&top, &lower),
            "top {top:?} overlaps the range that starts at group 67200: {lower:?}"
        );
        // An exact multiple of the dimension limit stays one rectangle.
        let exact = workgroup_dispatches(0, 65_535 * 2);
        assert_eq!(covered_until(0, &exact), 65_535 * 2);
        assert!(workgroup_dispatches(4, 0).is_empty());
    }
}
