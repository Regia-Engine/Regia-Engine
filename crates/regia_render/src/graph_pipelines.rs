// SPDX-License-Identifier: MPL-2.0
//! Material-graph pipeline sets ([`crate::surface_graph`]): one set per (render target, graph
//! code), built on worker threads or inline, prewarmed at level load.
//!
//! `specs/27` § 2.4 forbids pipeline creation on the frame path after warm-up. A graph's
//! pipelines are many (forward, blend modes, depth, the instanced GPU-driven variants, and with
//! a vertex stage the shadow and velocity passes), so a graph met for the first time on the
//! frame path is a visible hitch. Two modes:
//!
//! - **Inline** (the default; labs, goldens, tests): a missing set is built on the frame that
//!   first draws the graph, exactly as before, so a captured frame is complete.
//! - **Async** ([`crate::FrameRenderer::set_async_material_graphs`], on in
//!   [`crate::RuntimeRenderer`]): a missing set is queued to a worker thread and its draws use the
//!   standard surface until it lands (the engine's "default material while compiling").
//!   [`crate::FrameRenderer::prewarm_material_graphs`] queues every graph a level uses at load,
//!   and [`crate::FrameRenderer::settle_material_graphs`] waits for them, so a loaded level
//!   draws its graphs from the first frame with no pipeline built on the frame thread.
//!
//! Pipeline creation on a worker is not counted by [`crate::frame_path_counters`] (the counter
//! is per thread), so a soak can assert that the frame thread built nothing. The workers count
//! as background builders of the map tier (`pipeline_warm::mark_background_builder`): their
//! pipelines are the warm's, not "created outside the warm queue".

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, Weak, mpsc};
use std::time::{Duration, Instant};

use crate::shader_lib::compose_source;
use crate::surface_graph::{
    GraphCode, UV1_LAYOUT, code_defs, forward_source_with_code, vertex_host_defs, vertex_host_source,
};
use crate::{LabVertex, RenderError};

/// Worker threads that build graph pipelines in async mode: a quarter of the hardware
/// threads, 2..=8 (`REGIA_MATGRAPH_WORKERS` overrides). A level's graphs are many sets of
/// about ten pipelines each, and a set is CPU-bound in the shader compiler and the driver, so
/// two workers left a big level's graphs on the standard surface (and a settling warm waiting)
/// for tens of seconds.
fn worker_count() -> usize {
    static COUNT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *COUNT.get_or_init(|| {
        worker_count_from(
            regia_core::brand::env_var("REGIA_MATGRAPH_WORKERS")
                .ok()
                .as_deref(),
            std::thread::available_parallelism()
                .ok()
                .map(std::num::NonZeroUsize::get),
        )
    })
}

/// [`worker_count`] for an environment value and a hardware thread count: a positive override
/// wins, else a quarter of the threads clamped to 2..=8 (2 when the count is unknown).
fn worker_count_from(value: Option<&str>, threads: Option<usize>) -> usize {
    value
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| threads.map_or(2, |n| (n / 4).clamp(2, 8)))
}

/// The render target a graph's pipelines are built for (the forward pass's pipeline key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GraphTarget {
    /// Colour attachment format.
    pub format: wgpu::TextureFormat,
    /// Encode to sRGB in the shader (a non-sRGB LDR target). `false` for linear HDR frames.
    pub encode: bool,
    /// Draw both faces.
    pub double_sided: bool,
    /// MSAA sample count (1 or 4).
    pub samples: u32,
    /// Also build the instanced GPU-driven variants (single-sample frames on devices with
    /// indirect first-instance; normalised to the device by the prewarm).
    pub instanced: bool,
    /// The ray-traced sun's forward module ([`crate::hwrt::SUN_SHADOW_RT_DEF`], ADR-2035). `false`
    /// everywhere unless the profile traces the sun.
    pub sun_rt: bool,
}

impl GraphTarget {
    /// The linear HDR target of a post-processed or temporally upscaled frame: `Rgba16Float`,
    /// single sample, GPU-driven. What [`crate::RuntimeRenderer`] draws with the Desktop and
    /// Deck presets.
    pub const HDR: Self = Self {
        format: wgpu::TextureFormat::Rgba16Float,
        encode: false,
        double_sided: false,
        samples: 1,
        instanced: true,
        sun_rt: false,
    };
}

/// Counters of the graph pipeline cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphPipelineStats {
    /// Pipeline sets ready to draw.
    pub ready: usize,
    /// Sets queued or building on a worker.
    pub pending: usize,
    /// Sets whose modules failed to build (their draws are skipped).
    pub failed: usize,
    /// Sets built inline on the frame path (always 0 after a settled prewarm in async mode).
    pub built_on_frame_path: u64,
    /// Sets built by a prewarm or a worker.
    pub built_off_frame_path: u64,
    /// Draws that used the standard surface because their graph's set was still building.
    pub fallback_draws: u64,
}

/// Pipeline layouts a graph set is built against (cloned into worker jobs).
#[derive(Clone)]
pub(crate) struct GraphLayouts {
    /// View, object uniform, material.
    pub forward: wgpu::PipelineLayout,
    /// Shadow view-projection, object uniform, material (vertex-stage graphs).
    pub shadow: wgpu::PipelineLayout,
    /// Instanced GPU-driven layouts.
    pub instanced: InstancedLayouts,
    /// Velocity frame, velocity objects, material (vertex-stage graphs).
    pub velocity: wgpu::PipelineLayout,
}

/// Layouts of the instanced GPU-driven passes, with the material group appended for shadows.
#[derive(Clone)]
pub(crate) struct InstancedLayouts {
    /// View, instanced objects + slots, material.
    pub color: wgpu::PipelineLayout,
    /// View, objects, material.
    pub depth: wgpu::PipelineLayout,
    /// Shadow view-projection, objects, material.
    pub shadow: wgpu::PipelineLayout,
}

/// Instanced GPU-driven pipelines of one graph.
pub(crate) struct InstancedGraphPipelines {
    pub color_opaque: wgpu::RenderPipeline,
    pub color_mask: wgpu::RenderPipeline,
    pub depth_opaque: wgpu::RenderPipeline,
    pub depth_mask: wgpu::RenderPipeline,
}

/// Every pipeline one graph draws with on one target.
pub(crate) struct GraphPipelineSet {
    pub depth: wgpu::RenderPipeline,
    pub color: wgpu::RenderPipeline,
    pub blend: wgpu::RenderPipeline,
    pub premul: wgpu::RenderPipeline,
    pub add: wgpu::RenderPipeline,
    /// Transmission's first blend (`fs_transmit` on the graph's base colour): what is behind ×
    /// the tinted transmittance. A transmissive graph draw then adds its own light with
    /// [`Self::add`] (ADR-347).
    pub transmit_mul: wgpu::RenderPipeline,
    /// The accumulation of order-independent transparency (`fs_oit`, ADR-2065): composed and built
    /// only when a frame of a profile that asks draws this graph translucent. `None` when the module
    /// failed to compose (logged): the graph then keeps the sorted run.
    pub oit: crate::pipeline_warm::DeferredPipeline<Option<wgpu::RenderPipeline>>,
    pub instanced: Option<InstancedGraphPipelines>,
    /// CPU-object shadow pipeline (vertex-stage graphs only).
    pub shadow: Option<wgpu::RenderPipeline>,
    /// Instanced shadow pipeline (vertex-stage graphs on GPU-driven targets).
    pub shadow_instanced: Option<wgpu::RenderPipeline>,
    /// Motion vectors (vertex-stage graphs only).
    pub velocity: Option<wgpu::RenderPipeline>,
    /// Draws need the second UV stream in vertex buffer slot 1.
    pub uv1: bool,
}

fn module(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    file: &str,
    mut defs: HashMap<String, naga_oil::compose::ShaderDefValue>,
) -> Result<wgpu::ShaderModule, RenderError> {
    // The material binding matches this device's material layout (`bindless`).
    crate::bindless::add_material_defs(device, &mut defs);
    let module = {
        let _span = tracing::trace_span!("graph_compose").entered();
        compose_source(source, file, defs)?
    };
    let _span = tracing::trace_span!("graph_shader_module").entered();
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
    }))
}

fn primitive(double_sided: bool, cull_back: bool) -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        cull_mode: if double_sided || !cull_back {
            None
        } else {
            Some(wgpu::Face::Back)
        },
        ..Default::default()
    }
}

fn depth_state(write: bool, compare: wgpu::CompareFunction) -> wgpu::DepthStencilState {
    wgpu::DepthStencilState {
        format: wgpu::TextureFormat::Depth32Float,
        depth_write_enabled: Some(write),
        depth_compare: Some(compare),
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    }
}

/// One render pipeline of a graph set.
struct PipelineDesc<'a> {
    label: &'static str,
    layout: &'a wgpu::PipelineLayout,
    module: &'a wgpu::ShaderModule,
    fragment: &'static str,
    /// Colour target and blend; `None` for depth-only passes.
    color: Option<(wgpu::TextureFormat, Option<wgpu::BlendState>)>,
    depth: Option<wgpu::DepthStencilState>,
    primitive: wgpu::PrimitiveState,
    samples: u32,
    uv1: bool,
}

fn pipeline(device: &crate::device::TrackedDevice, desc: PipelineDesc<'_>) -> wgpu::RenderPipeline {
    let lab = LabVertex::layout();
    let two = [Some(lab.clone()), Some(UV1_LAYOUT)];
    let one = [Some(lab)];
    let targets = [desc.color.map(|(format, blend)| wgpu::ColorTargetState {
        format,
        blend,
        write_mask: wgpu::ColorWrites::ALL,
    })];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(desc.label),
        layout: Some(desc.layout),
        vertex: wgpu::VertexState {
            module: desc.module,
            entry_point: Some("vs_main"),
            buffers: if desc.uv1 { &two[..] } else { &one[..] },
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: desc.module,
            entry_point: Some(desc.fragment),
            targets: if desc.color.is_some() { &targets[..] } else { &[] },
            compilation_options: Default::default(),
        }),
        primitive: desc.primitive,
        depth_stencil: desc.depth,
        multisample: wgpu::MultisampleState {
            count: desc.samples.max(1),
            mask: u64::MAX,
            alpha_to_coverage_enabled: false,
        },
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

/// The `OIT` module of `code` and its accumulation pipeline (`crate::oit`, ADR-2065): built on first
/// use, because most graphs are never asked for it.
fn oit_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    target: GraphTarget,
    code: &GraphCode,
) -> Result<wgpu::RenderPipeline, RenderError> {
    let d = &crate::device::TrackedDevice::observe(device);
    let forward = forward_source_with_code(code)?;
    let mut extra = vec!["ALPHA_TEST", "OIT", "skip_srgb_encode"];
    if target.sun_rt {
        extra.push(crate::hwrt::SUN_SHADOW_RT_DEF);
    }
    let file = "lab_forward_graph.wgsl";
    let module = module(
        d,
        "lab_forward_graph_oit",
        &forward,
        file,
        code_defs(code, &extra),
    )?;
    let lab = LabVertex::layout();
    let two = [Some(lab.clone()), Some(UV1_LAYOUT)];
    let one = [Some(lab)];
    Ok(d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("graph oit"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            buffers: if code.uv1 { &two[..] } else { &one[..] },
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_oit"),
            targets: &crate::oit::accum_targets(),
            compilation_options: Default::default(),
        }),
        primitive: primitive(target.double_sided, true),
        depth_stencil: Some(depth_state(false, wgpu::CompareFunction::LessEqual)),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(d).as_deref(),
    }))
}

/// Builds every pipeline of `code` for `target`.
///
/// # Errors
///
/// [`RenderError::Shader`] when a module fails to compose (a compiler bug: graphs are validated
/// when compiled and when cooked).
pub(crate) fn build_set(
    device: &wgpu::Device,
    layouts: &GraphLayouts,
    target: GraphTarget,
    code: &GraphCode,
) -> Result<GraphPipelineSet, RenderError> {
    let d = &crate::device::TrackedDevice::observe(device);
    let forward = forward_source_with_code(code)?;
    let encode: &[&str] = if target.encode { &[] } else { &["skip_srgb_encode"] };
    let defs = |extra: &[&str]| {
        let mut all = extra.to_vec();
        all.extend_from_slice(encode);
        if target.sun_rt {
            all.push(crate::hwrt::SUN_SHADOW_RT_DEF);
        }
        code_defs(code, &all)
    };
    let file = "lab_forward_graph.wgsl";
    let cpu = module(d, "lab_forward_graph", &forward, file, defs(&["ALPHA_TEST"]))?;
    let uv1 = code.uv1;
    let additive = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    };
    let forward_desc = |label, blend: Option<wgpu::BlendState>, write: bool| PipelineDesc {
        label,
        layout: &layouts.forward,
        module: &cpu,
        fragment: "fs_main",
        color: Some((target.format, blend)),
        depth: Some(depth_state(write, wgpu::CompareFunction::LessEqual)),
        primitive: primitive(target.double_sided, true),
        samples: target.samples,
        uv1,
    };
    let color = pipeline(d, forward_desc("graph forward", None, true));
    let blend = pipeline(
        d,
        forward_desc("graph blend", Some(wgpu::BlendState::ALPHA_BLENDING), false),
    );
    let premul = pipeline(
        d,
        forward_desc(
            "graph premul",
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            false,
        ),
    );
    let add = pipeline(d, forward_desc("graph additive", Some(additive), false));
    // `dst ×= transmittance`, keeping the target's alpha (the HDR target carries the SSR
    // roughness there), as the standard "lab transmit multiply" pipeline.
    let keep_alpha = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    };
    let transmit_mul = pipeline(
        d,
        PipelineDesc {
            fragment: "fs_transmit",
            ..forward_desc(
                "graph transmit multiply",
                Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::Dst,
                        dst_factor: wgpu::BlendFactor::Zero,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: keep_alpha,
                }),
                false,
            )
        },
    );
    let depth = pipeline(
        d,
        PipelineDesc {
            label: "graph depth",
            layout: &layouts.forward,
            module: &cpu,
            fragment: "fs_depth",
            color: None,
            depth: Some(depth_state(true, wgpu::CompareFunction::LessEqual)),
            primitive: primitive(target.double_sided, true),
            samples: target.samples,
            uv1,
        },
    );

    let instanced = if target.instanced {
        let inst = &layouts.instanced;
        let color_module = |mask: bool| {
            let extra: &[&str] = if mask {
                &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"]
            } else {
                &["GPU_DRIVEN", "GPU_INSTANCED"]
            };
            module(d, "lab_forward_graph_instanced", &forward, file, defs(extra))
        };
        let depth_module = |mask: bool| {
            let extra: &[&str] = if mask {
                &["GPU_DRIVEN", "ALPHA_TEST"]
            } else {
                &["GPU_DRIVEN"]
            };
            module(
                d,
                "lab_forward_graph_depth_instanced",
                &forward,
                file,
                code_defs(code, extra),
            )
        };
        let color_pipe = |module: &wgpu::ShaderModule, label| {
            pipeline(
                d,
                PipelineDesc {
                    label,
                    layout: &inst.color,
                    module,
                    fragment: "fs_main",
                    color: Some((target.format, None)),
                    depth: Some(depth_state(true, wgpu::CompareFunction::LessEqual)),
                    primitive: primitive(target.double_sided, true),
                    samples: target.samples,
                    uv1,
                },
            )
        };
        let depth_pipe = |module: &wgpu::ShaderModule, label| {
            pipeline(
                d,
                PipelineDesc {
                    label,
                    layout: &inst.depth,
                    module,
                    fragment: "fs_depth",
                    color: None,
                    depth: Some(depth_state(true, wgpu::CompareFunction::LessEqual)),
                    primitive: primitive(target.double_sided, true),
                    samples: target.samples,
                    uv1,
                },
            )
        };
        let (opaque, mask) = (color_module(false)?, color_module(true)?);
        let (depth_opaque, depth_mask) = (depth_module(false)?, depth_module(true)?);
        Some(InstancedGraphPipelines {
            color_opaque: color_pipe(&opaque, "graph forward (instanced)"),
            color_mask: color_pipe(&mask, "graph forward mask (instanced)"),
            depth_opaque: depth_pipe(&depth_opaque, "graph depth (instanced)"),
            depth_mask: depth_pipe(&depth_mask, "graph depth mask (instanced)"),
        })
    } else {
        None
    };

    // Vertex-stage graphs: the displaced surface casts the shadow and writes the motion.
    let (mut shadow, mut shadow_instanced, mut velocity) = (None, None, None);
    if let Some(source) = vertex_host_source(code, false)? {
        let shadow_desc = |label, layout, module| PipelineDesc {
            label,
            layout,
            module,
            fragment: "fs_main",
            color: None,
            // `Less` and no pipeline bias, like the standard shadow pipelines.
            depth: Some(depth_state(true, wgpu::CompareFunction::Less)),
            primitive: primitive(target.double_sided, true),
            samples: 1,
            uv1,
        };
        let cpu_shadow = module(
            d,
            "lab_shadow_graph",
            &source,
            "lab_shadow_graph.wgsl",
            vertex_host_defs(code, &[]),
        )?;
        shadow = Some(pipeline(
            d,
            shadow_desc("graph shadow", &layouts.shadow, &cpu_shadow),
        ));
        if target.instanced {
            let gpu_shadow = module(
                d,
                "lab_shadow_graph_instanced",
                &source,
                "lab_shadow_graph.wgsl",
                vertex_host_defs(code, &["GPU_DRIVEN"]),
            )?;
            shadow_instanced = Some(pipeline(
                d,
                shadow_desc("graph shadow (instanced)", &layouts.instanced.shadow, &gpu_shadow),
            ));
        }
    }
    if let Some(source) = vertex_host_source(code, true)? {
        let velocity_module = module(
            d,
            "velocity_graph",
            &source,
            "velocity_graph.wgsl",
            vertex_host_defs(code, &[]),
        )?;
        velocity = Some(pipeline(
            d,
            PipelineDesc {
                label: "graph velocity",
                layout: &layouts.velocity,
                module: &velocity_module,
                fragment: "fs_main",
                color: Some((wgpu::TextureFormat::Rgba16Float, None)),
                // The velocity pass keeps the latest draw per pixel: each instance writes its
                // draw-order key as depth (`taau::order_depth`), as the standard pipeline does.
                // Cull matches the colour draw: a double-sided graph writes motion from behind.
                depth: Some(depth_state(true, wgpu::CompareFunction::GreaterEqual)),
                primitive: primitive(target.double_sided, true),
                samples: 1,
                uv1,
            },
        ));
    }
    let oit = {
        let (device, layout, code) = (wgpu::Device::clone(device), layouts.forward.clone(), code.clone());
        crate::pipeline_warm::DeferredPipeline::on_demand("graph oit", move || {
            oit_pipeline(&device, &layout, target, &code)
                .map_err(|err| tracing::error!(target: "regia_render", "graph oit variant: {err}"))
                .ok()
        })
    };
    Ok(GraphPipelineSet {
        depth,
        color,
        blend,
        premul,
        add,
        transmit_mul,
        oit,
        instanced,
        shadow,
        shadow_instanced,
        velocity,
        uv1,
    })
}

type SetKey = (GraphTarget, u64);

/// The work of one queued set. A real job's closure captures the code, the layouts and a
/// device clone; a closure keeps the workers' shutdown protocol testable without a GPU.
type BuildFn = Box<dyn FnOnce() -> Result<GraphPipelineSet, RenderError> + Send>;

struct Job {
    key: SetKey,
    build: BuildFn,
}

type Done = (SetKey, Result<GraphPipelineSet, RenderError>);

/// The message of a caught panic payload (`&str` or `String`; anything else is opaque).
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// What [`GraphPipelineCache::resolve`] found.
pub(crate) enum Resolved {
    /// Draw with this set.
    Ready(Arc<GraphPipelineSet>),
    /// Still building: draw with the standard surface.
    Pending,
    /// Not on the cooked permutation list and refused ([`crate::PermutationEnforcement::Fallback`]):
    /// draw with the standard surface, nothing is built.
    Refused,
    /// The set cannot be built (or the code is unknown): skip the draw.
    Failed,
}

/// Graph pipeline sets by (target, code key).
pub(crate) struct GraphPipelineCache {
    ready: HashMap<SetKey, Arc<GraphPipelineSet>>,
    pending: HashSet<SetKey>,
    failed: HashMap<SetKey, String>,
    codes: HashMap<u64, GraphCode>,
    async_builds: bool,
    jobs: Option<mpsc::Sender<Job>>,
    /// The workers' end of the queue, so a shutdown drops queued jobs on its own thread. Weak:
    /// when every worker is gone the queue disconnects and `enqueue` falls back to inline.
    job_rx: Weak<Mutex<mpsc::Receiver<Job>>>,
    /// Set on drop: workers skip queued jobs (each holds a device clone) and exit.
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// The worker threads, joined on drop so no device clone outlives the cache.
    workers: Vec<std::thread::JoinHandle<()>>,
    /// A device clone held until the workers are joined: a worker dropping a finished or
    /// cancelled job never releases the last clone, so `vkDestroyDevice` never runs on a
    /// worker (ADR-269).
    device: Option<wgpu::Device>,
    /// Longest a shutdown waits for builds already in the driver ([`shutdown_wait`]).
    join_wait: Duration,
    done_tx: mpsc::Sender<Done>,
    done_rx: Mutex<mpsc::Receiver<Done>>,
    built_inline: u64,
    built_off: u64,
    fallback_draws: u64,
    targets_seen: Vec<GraphTarget>,
    /// The cooked permutation list check (`permutation_policy`, ADR-2026). Off by default.
    guard: crate::PermutationGuard,
    /// Sets the guard refused: never built, their draws use the standard surface.
    refused: HashSet<SetKey>,
}

impl Default for GraphPipelineCache {
    fn default() -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        Self {
            ready: HashMap::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            codes: HashMap::new(),
            async_builds: false,
            jobs: None,
            job_rx: Weak::new(),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            workers: Vec::new(),
            device: None,
            join_wait: shutdown_wait(),
            done_tx,
            done_rx: Mutex::new(done_rx),
            built_inline: 0,
            built_off: 0,
            fallback_draws: 0,
            targets_seen: Vec::new(),
            guard: crate::PermutationGuard::off(),
            refused: HashSet::new(),
        }
    }
}

/// Longest a dropping [`GraphPipelineCache`] waits for its workers to finish the sets they are
/// building. A pipeline compile cannot be interrupted, and a worker left inside the NVIDIA
/// shader compiler when the process exits crashes the exit (ADR-269), so this is generous: it
/// only bounds a driver that hangs. Queued sets are not waited for; they are dropped unbuilt.
const WORKER_JOIN_WAIT: Duration = Duration::from_secs(30);

/// Longest a shutdown then waits for the GPU to go idle before releasing its device clone.
const GPU_IDLE_WAIT: Duration = Duration::from_secs(5);

/// [`WORKER_JOIN_WAIT`], or `REGIA_MATGRAPH_SHUTDOWN_WAIT_MS` (milliseconds) when set.
fn shutdown_wait() -> Duration {
    parse_shutdown_wait(
        regia_core::brand::env_var("REGIA_MATGRAPH_SHUTDOWN_WAIT_MS")
            .ok()
            .as_deref(),
    )
}

fn parse_shutdown_wait(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(WORKER_JOIN_WAIT, Duration::from_millis)
}

impl Drop for GraphPipelineCache {
    fn drop(&mut self) {
        self.shutdown_workers();
    }
}

impl GraphPipelineCache {
    /// Stops the workers and joins them: queued jobs are dropped unbuilt at once, and the
    /// builds already inside the driver are waited for (at most the join wait,
    /// [`WORKER_JOIN_WAIT`] by default), so no worker is inside the shader compiler when the
    /// process exits and the device is destroyed with the app (v0.1.0 NVIDIA quit crash,
    /// ADR-269). Then the GPU is waited for and the cache's device clone released on this
    /// thread. True when no worker is left running.
    pub(crate) fn shutdown_workers(&mut self) -> bool {
        self.stop_workers();
        let start = Instant::now();
        // Short once the device is lost: the deliberate exit skips the exit handlers (ADR-276).
        let deadline = start + crate::gpu_health::worker_wait(self.join_wait);
        let mut running = 0;
        for handle in std::mem::take(&mut self.workers) {
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                running += 1;
            }
        }
        let waited = start.elapsed();
        if waited >= Duration::from_millis(100) {
            tracing::info!(
                target: "regia_render",
                "material graph shutdown waited {:.0} ms for builds in the driver",
                waited.as_secs_f64() * 1e3
            );
        }
        if running > 0 {
            // Last resort: a driver that hangs in a compile. The worker's job holds a device
            // clone, so the cache's clone is leaked rather than released: the device is then
            // never destroyed on the abandoned worker.
            tracing::warn!(
                target: "regia_render",
                "{running} material graph worker(s) still building {:.1} s after shutdown; left running",
                waited.as_secs_f64()
            );
            std::mem::forget(self.device.take());
            return false;
        }
        if let Some(device) = self.device.take() {
            let _span = tracing::info_span!("material_graph_shutdown_gpu_idle").entered();
            if let Err(err) = device.poll(wgpu::PollType::Wait {
                submission_index: None,
                // Bounded like the worker join: a lost device must not hold the exit for 5 s.
                timeout: Some(crate::gpu_health::worker_wait(GPU_IDLE_WAIT)),
            }) {
                tracing::debug!(target: "regia_render", %err, "material graph shutdown: GPU not idle; going on");
            }
        }
        true
    }

    /// Tells the workers to stop without waiting for them: no more builds start, queued jobs
    /// are dropped unbuilt on this thread, and a worker inside a build ends after it. The clean
    /// exit calls it first, so a build in flight finishes while the GPU drains instead of in
    /// the renderer's drop. Returns the queued jobs dropped.
    pub(crate) fn stop_workers(&mut self) -> usize {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        // Disconnects the queue: idle workers wake from `recv` and exit. A worker waiting in
        // `recv` holds the queue lock, and gets its answer at once now.
        self.jobs = None;
        let Some(rx) = self.job_rx.upgrade() else {
            return 0;
        };
        let queued: Vec<Job> = rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_iter()
            .collect();
        drop(rx);
        for job in &queued {
            self.pending.remove(&job.key);
        }
        queued.len()
    }

    /// Async (worker) or inline builds.
    pub(crate) fn set_async(&mut self, enabled: bool) {
        self.async_builds = enabled;
    }

    /// Whether builds go to workers ([`Self::set_async`]).
    pub(crate) fn is_async(&self) -> bool {
        self.async_builds
    }

    /// Installs the permutation guard (ADR-2026). Sets it refused before are forgotten, so a
    /// new list can admit them on their next draw.
    pub(crate) fn set_guard(&mut self, guard: crate::PermutationGuard) {
        self.guard = guard;
        self.refused.clear();
    }

    /// Takes the guard out (a stand-in renderer handing it to the warmed one), leaving it off.
    pub(crate) fn take_guard(&mut self) -> crate::PermutationGuard {
        self.refused.clear();
        std::mem::take(&mut self.guard)
    }

    /// A copy of the guard, for a stand-in renderer that may end up drawing the game.
    pub(crate) fn guard(&self) -> crate::PermutationGuard {
        self.guard.clone()
    }

    /// The permutation guard's counters.
    pub(crate) fn guard_stats(&self) -> crate::PermutationGuardStats {
        self.guard.stats()
    }

    /// Asks the guard about a set not known yet; remembers a refusal. True when it may be built.
    fn admitted(&mut self, key: SetKey) -> bool {
        if self.guard.admit(key.1, &key.0) == crate::PermutationAdmission::Refused {
            self.refused.insert(key);
            return false;
        }
        true
    }

    /// Remembers the code of `key` (materials register it on upload).
    pub(crate) fn register(&mut self, key: u64, code: &GraphCode) {
        self.codes.entry(key).or_insert_with(|| code.clone());
    }

    /// Installs finished worker builds.
    pub(crate) fn poll(&mut self) {
        let finished: Vec<Done> = match self.done_rx.lock() {
            Ok(rx) => rx.try_iter().collect(),
            Err(_) => Vec::new(),
        };
        for (key, result) in finished {
            self.install(key, result, false);
        }
    }

    fn install(&mut self, key: SetKey, result: Result<GraphPipelineSet, RenderError>, inline: bool) {
        self.pending.remove(&key);
        match result {
            Ok(set) => {
                if inline {
                    self.built_inline += 1;
                } else {
                    self.built_off += 1;
                }
                self.ready.insert(key, Arc::new(set));
            }
            Err(err) => {
                tracing::warn!(
                    target: "regia_render",
                    graph = key.1,
                    %err,
                    "material graph pipelines failed; its draws are skipped"
                );
                self.failed.insert(key, err.to_string());
            }
        }
    }

    fn spawn_workers(&mut self) -> bool {
        if self.jobs.is_some() {
            return true;
        }
        if self.stop.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        self.job_rx = Arc::downgrade(&rx);
        let mut spawned = 0;
        for index in 0..worker_count() {
            let rx = Arc::clone(&rx);
            let done = self.done_tx.clone();
            let stop = Arc::clone(&self.stop);
            // Registered (ADR-287): the end of the process waits for a build in the driver.
            let worker =
                regia_platform::GpuWorkerBuilder::new(format!("regia-matgraph-{index}")).spawn(move |gpu| {
                    // A background builder, like a warm worker: never the frame path.
                    crate::pipeline_warm::mark_background_builder(crate::WarmTier::Map);
                    loop {
                        let job = {
                            let Ok(rx) = rx.lock() else {
                                return;
                            };
                            match rx.recv() {
                                Ok(job) => job,
                                Err(_) => return,
                            }
                        };
                        // The cache is going away: drop the build (and its device clone) unbuilt,
                        // but answer, so a key taken from the queue after the stop does not
                        // stay pending for a `settle` that waits on it.
                        if stop.load(std::sync::atomic::Ordering::Acquire) {
                            let Job { key, build } = job;
                            drop(build);
                            let _ = done.send((
                                key,
                                Err(RenderError::Shader(
                                    "material graph build cancelled at shutdown".into(),
                                )),
                            ));
                            return;
                        }
                        let _span = tracing::info_span!("material_graph_pipelines_async").entered();
                        let Job { key, build } = job;
                        let _call = gpu.driver_call();
                        // A panicking build (wgpu's default handler panics on a validation
                        // error) must still land a result: `settle` waits for every key. This
                        // holds in unwinding builds (dev, editor, tests); the shipping profile is
                        // `panic = "abort"`, where the panic ends the game with a crash report
                        // instead of skipping the material's draws (audit v034 net L6).
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build))
                            .unwrap_or_else(|panic| {
                                let message = panic_message(panic.as_ref());
                                tracing::error!(
                                    target: "regia_render",
                                    graph = key.1,
                                    target_format = ?key.0.format,
                                    %message,
                                    "material graph pipeline build panicked; its draws are skipped"
                                );
                                Err(RenderError::Shader(format!(
                                    "material graph build panicked: {message}"
                                )))
                            });
                        // The idle wait for the next job is not the last site's compile time.
                        crate::pipeline_warm::end_background_job();
                        if done.send((key, result)).is_err() {
                            return;
                        }
                    }
                });
            match worker {
                Ok(handle) => {
                    self.workers.push(handle);
                    spawned += 1;
                }
                Err(err) => {
                    tracing::warn!(target: "regia_render", %err, "material graph worker did not start")
                }
            }
        }
        if spawned == 0 {
            return false;
        }
        self.jobs = Some(tx);
        true
    }

    /// Queues `key` on a worker. False when the workers could not start.
    fn enqueue(&mut self, device: &wgpu::Device, layouts: &GraphLayouts, key: SetKey) -> bool {
        let Some(code) = self.codes.get(&key.1).cloned() else {
            return false;
        };
        if !self.spawn_workers() {
            return false;
        }
        if self.device.is_none() {
            self.device = Some(device.clone());
        }
        let (layouts, device) = (layouts.clone(), device.clone());
        self.send(key, Box::new(move || build_set(&device, &layouts, key.0, &code)))
    }

    /// Sends one job to the running workers; marks `key` pending when it went.
    fn send(&mut self, key: SetKey, build: BuildFn) -> bool {
        let sent = self
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(Job { key, build }).is_ok());
        if sent {
            self.pending.insert(key);
        }
        sent
    }

    /// Queues an arbitrary build on the workers (tests: a fake slow build, no GPU).
    #[cfg(test)]
    fn enqueue_build(&mut self, key: SetKey, build: BuildFn) -> bool {
        self.spawn_workers() && self.send(key, build)
    }

    fn known(&self, key: &SetKey) -> bool {
        self.ready.contains_key(key) || self.pending.contains(key) || self.failed.contains_key(key)
    }

    /// Game-frame warms: the other forward module (ADR-2035) of every graph this cache has met,
    /// for every target it has drawn. Returns the sets started.
    pub(crate) fn prewarm_sun_twins(&mut self, device: &wgpu::Device, layouts: &GraphLayouts) -> usize {
        let codes: Vec<GraphCode> = self.codes.values().cloned().collect();
        let targets = self.targets_seen().to_vec();
        let mut started = 0;
        for target in targets {
            let twin = GraphTarget {
                sun_rt: !target.sun_rt,
                ..target
            };
            for code in &codes {
                started += usize::from(self.prewarm(device, layouts, twin, code));
            }
        }
        started
    }

    /// Prewarm: queue `code` for `target` (async mode) or build it now (inline mode, off the
    /// frame path). Returns true when a build was started or done.
    pub(crate) fn prewarm(
        &mut self,
        device: &wgpu::Device,
        layouts: &GraphLayouts,
        target: GraphTarget,
        code: &GraphCode,
    ) -> bool {
        let key = (target, code.key());
        self.register(key.1, code);
        if self.known(&key) || self.refused.contains(&key) || !self.admitted(key) {
            return false;
        }
        if self.async_builds && self.enqueue(device, layouts, key) {
            return true;
        }
        let _span = tracing::info_span!("material_graph_prewarm").entered();
        let result = build_set(device, layouts, target, code);
        self.install(key, result, false);
        true
    }

    /// The set for `key` on the frame path. Inline mode builds a missing set now; async mode
    /// queues it and reports [`Resolved::Pending`]. With `defer` (the frame load budget's pipeline
    /// cap, ADR-7884) a missing set is queued even in inline mode. When the workers cannot take it
    /// (they cannot start, or were shut down), a deferred set is built here only with
    /// `inline_allowed` (the frame has built nothing yet: at most one set per frame past the cap,
    /// like the mesh that crosses the upload cap); otherwise it is [`Resolved::Pending`] and asked
    /// for again next frame (ADR-8122).
    pub(crate) fn resolve(
        &mut self,
        device: &wgpu::Device,
        layouts: &GraphLayouts,
        target: GraphTarget,
        key: u64,
        (defer, inline_allowed): (bool, bool),
    ) -> Resolved {
        if !self.targets_seen.contains(&target) {
            self.targets_seen.push(target);
        }
        let set_key = (target, key);
        if let Some(set) = self.ready.get(&set_key) {
            return Resolved::Ready(Arc::clone(set));
        }
        if self.failed.contains_key(&set_key) {
            return Resolved::Failed;
        }
        if self.pending.contains(&set_key) {
            return Resolved::Pending;
        }
        // Unknown code is not a decision: a guard would count it again every frame.
        let Some(code) = self.codes.get(&key).cloned() else {
            return Resolved::Failed;
        };
        if self.refused.contains(&set_key) || !self.admitted(set_key) {
            return Resolved::Refused;
        }
        if (self.async_builds || defer) && self.enqueue(device, layouts, set_key) {
            return Resolved::Pending;
        }
        if defer && !inline_allowed {
            // Nothing queued: the next frame asks again (and builds it first thing if the
            // workers still cannot take it).
            return Resolved::Pending;
        }
        let _span = tracing::info_span!("material_graph_pipelines").entered();
        let result = build_set(device, layouts, target, &code);
        self.install(set_key, result, true);
        match self.ready.get(&set_key) {
            Some(set) => Resolved::Ready(Arc::clone(set)),
            None => Resolved::Failed,
        }
    }

    /// Counts a draw that fell back to the standard surface.
    pub(crate) fn note_fallback(&mut self) {
        self.fallback_draws += 1;
    }

    /// Blocks until every queued build has landed (level load; never on the frame path).
    pub(crate) fn settle(&mut self) {
        while !self.pending.is_empty() {
            let next = match self.done_rx.lock() {
                Ok(rx) => rx.recv(),
                Err(_) => return,
            };
            match next {
                Ok((key, result)) => self.install(key, result, false),
                Err(_) => return,
            }
        }
    }

    /// [`Self::settle`] until `deadline`: installs every build that lands by then and returns
    /// whether none is left pending (the rest keep building and land through [`Self::poll`]).
    pub(crate) fn settle_until(&mut self, deadline: std::time::Instant) -> bool {
        self.poll();
        while !self.pending.is_empty() {
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let next = match self.done_rx.lock() {
                Ok(rx) => rx.recv_timeout(deadline - now),
                Err(_) => return false,
            };
            match next {
                Ok((key, result)) => self.install(key, result, false),
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => return self.pending.is_empty(),
            }
        }
        true
    }

    /// Targets the frame path has drawn graphs on, first use first.
    pub(crate) fn targets_seen(&self) -> &[GraphTarget] {
        &self.targets_seen
    }

    /// Counters.
    pub(crate) fn stats(&self) -> GraphPipelineStats {
        GraphPipelineStats {
            ready: self.ready.len(),
            pending: self.pending.len(),
            failed: self.failed.len(),
            built_on_frame_path: self.built_inline,
            built_off_frame_path: self.built_off,
            fallback_draws: self.fallback_draws,
        }
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// The workers own device clones while they build: the cache must join them when it goes
    /// away, so the device is not released during process exit (v0.1.0 NVIDIA quit crash).
    #[test]
    fn shutting_the_cache_down_joins_its_workers() {
        let mut cache = GraphPipelineCache::default();
        assert!(cache.spawn_workers());
        assert!(!cache.workers.is_empty());
        assert!(cache.shutdown_workers(), "idle workers must end at once");
        assert!(cache.workers.is_empty());
        assert!(!cache.spawn_workers(), "no new workers after shutdown");
    }

    fn key(n: u64) -> SetKey {
        (GraphTarget::HDR, n)
    }

    fn fake_result() -> Result<GraphPipelineSet, RenderError> {
        Err(RenderError::Shader("fake build".into()))
    }

    /// Stands in for a queued job's captures: records whether its build ran and the thread
    /// that dropped it.
    struct Probe {
        ran: Arc<AtomicBool>,
        dropped_on: Arc<Mutex<Vec<std::thread::ThreadId>>>,
    }

    impl Probe {
        fn run(&self) {
            self.ran.store(true, Ordering::SeqCst);
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.dropped_on
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(std::thread::current().id());
        }
    }

    fn wait_for(count: &AtomicUsize, n: usize) {
        let start = Instant::now();
        while count.load(Ordering::SeqCst) < n {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "fake builds did not start"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// ADR-269: a build already in the driver is waited for, even past the old 1 s limit that
    /// abandoned it inside the NVIDIA compiler at process exit; a queued build never starts and
    /// is dropped on the thread that shuts the cache down.
    #[test]
    fn shutdown_waits_for_running_builds_and_drops_queued_ones() {
        let mut cache = GraphPipelineCache::default();
        cache.join_wait = Duration::from_secs(30);
        assert!(cache.spawn_workers());
        let workers = cache.workers.len();
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        for n in 0..workers {
            let (started, finished) = (Arc::clone(&started), Arc::clone(&finished));
            assert!(cache.enqueue_build(
                key(n as u64),
                Box::new(move || {
                    started.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1500));
                    finished.fetch_add(1, Ordering::SeqCst);
                    fake_result()
                }),
            ));
        }
        // Every worker is inside a build: the next jobs stay queued.
        wait_for(&started, workers);
        let ran = Arc::new(AtomicBool::new(false));
        let dropped_on = Arc::new(Mutex::new(Vec::new()));
        for n in 0..3 {
            let probe = Probe {
                ran: Arc::clone(&ran),
                dropped_on: Arc::clone(&dropped_on),
            };
            assert!(cache.enqueue_build(
                key(1000 + n),
                Box::new(move || {
                    probe.run();
                    fake_result()
                }),
            ));
        }
        assert_eq!(cache.stats().pending, workers + 3);

        assert!(
            cache.shutdown_workers(),
            "running builds must be waited for, not abandoned"
        );
        assert!(cache.workers.is_empty());
        assert_eq!(
            finished.load(Ordering::SeqCst),
            workers,
            "every running build finished before shutdown returned"
        );
        assert!(
            !ran.load(Ordering::SeqCst),
            "a queued build must not start after shutdown"
        );
        let dropped = dropped_on.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(dropped.len(), 3, "every queued job is dropped by the shutdown");
        assert!(
            dropped.iter().all(|id| *id == std::thread::current().id()),
            "queued jobs are dropped on the shutting-down thread, not on a worker"
        );
        // The running builds' results landed; the queued keys are no longer pending.
        cache.poll();
        let stats = cache.stats();
        assert_eq!((stats.pending, stats.failed), (0, workers));
    }

    /// A job a worker takes from the queue after the stop flag is set (it was blocked in `recv`
    /// when the stop came) is answered, not left pending: `settle` would wait on it forever.
    #[test]
    fn a_job_taken_after_the_stop_is_answered_not_left_pending() {
        let mut cache = GraphPipelineCache::default();
        assert!(cache.spawn_workers());
        cache.stop.store(true, Ordering::Release);
        let ran = Arc::new(AtomicBool::new(false));
        let probe = Probe {
            ran: Arc::clone(&ran),
            dropped_on: Arc::new(Mutex::new(Vec::new())),
        };
        assert!(cache.enqueue_build(
            key(7),
            Box::new(move || {
                probe.run();
                fake_result()
            }),
        ));
        cache.settle();
        assert!(!ran.load(Ordering::SeqCst), "a stopped cache must not build");
        let stats = cache.stats();
        assert_eq!((stats.pending, stats.failed), (0, 1));
    }

    /// A driver that hangs in a compile: the shutdown gives up after its limit (the last
    /// resort) and reports the worker left running.
    #[test]
    fn shutdown_gives_up_on_a_build_past_its_limit() {
        let mut cache = GraphPipelineCache::default();
        cache.join_wait = Duration::from_millis(50);
        let started = Arc::new(AtomicUsize::new(0));
        let (release, released) = mpsc::channel::<()>();
        let s = Arc::clone(&started);
        assert!(cache.enqueue_build(
            key(1),
            Box::new(move || {
                s.fetch_add(1, Ordering::SeqCst);
                let _ = released.recv_timeout(Duration::from_secs(30));
                fake_result()
            }),
        ));
        wait_for(&started, 1);
        assert!(!cache.shutdown_workers(), "a build past the limit is reported");
        assert!(cache.workers.is_empty());
        // Lets the abandoned worker end.
        drop(release);
    }

    #[test]
    fn shutdown_wait_reads_the_override_in_milliseconds() {
        assert_eq!(parse_shutdown_wait(None), WORKER_JOIN_WAIT);
        assert_eq!(parse_shutdown_wait(Some("2500")), Duration::from_millis(2500));
        assert_eq!(parse_shutdown_wait(Some(" 0 ")), Duration::ZERO);
        assert_eq!(parse_shutdown_wait(Some("soon")), WORKER_JOIN_WAIT);
    }

    /// Spec 13 § 2c: material-graph workers are a quarter of the hardware threads, 2..=8, and a
    /// positive `REGIA_MATGRAPH_WORKERS` overrides the count.
    #[test]
    fn worker_count_is_a_quarter_of_the_threads_clamped_to_two_to_eight() {
        assert_eq!(worker_count_from(None, Some(4)), 2);
        assert_eq!(worker_count_from(None, Some(16)), 4);
        assert_eq!(worker_count_from(None, Some(32)), 8);
        assert_eq!(worker_count_from(None, Some(128)), 8);
        assert_eq!(worker_count_from(None, None), 2);
        assert_eq!(worker_count_from(Some("12"), Some(4)), 12);
        assert_eq!(worker_count_from(Some("0"), Some(16)), 4, "zero is not a count");
        assert_eq!(worker_count_from(Some("many"), Some(16)), 4);
    }
}
