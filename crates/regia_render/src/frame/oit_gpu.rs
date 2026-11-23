// SPDX-License-Identifier: MPL-2.0
//! GPU side of weighted blended order-independent transparency ([`crate::oit`], ADR-2065): the `OIT`
//! variant of the forward shader, its accumulation pass, the composite and the reactive mask.
//!
//! A frame of a profile that asks moves the translucent draws that can take the pass out of the
//! sorted blended list. Hair cards stay in the sorted blend. The draws that move
//! accumulate in their own pass in the middle of the forward pass (after the opaque draws and the
//! sky, before the refraction copy), and the composite lays them over the image there. Glass and
//! the rest of the blended run then draw over the result, and refraction sees it. Every other draw
//! keeps its pipelines, and a frame with no such draw records nothing here and takes nothing from
//! the transient pool.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use naga_oil::compose::ShaderDefValue;

use super::lifecycle::missing_draw_entry;
use super::shadow_passes::GraphSets;
use super::{
    ALPHA_BLEND, ALPHA_PREMULTIPLIED, Draw, FrameRenderer, GlassPipelineTarget, LabMaterial, LabVertex,
    PipelineKey, RenderError, shader_module,
};
use crate::RenderDevice;
use crate::oit::{ACCUM_FORMAT, OitStats, REACTIVE_FORMAT, REVEALAGE_FORMAT};
use crate::pipeline_warm::DeferredPipeline;

/// The accumulation pipeline's key: the cull mode and the forward module.
type ColorKey = (bool, bool);

/// The layout and pipelines (kept in [`super::Pipes`], so a renderer that adopts warmed pipelines
/// adopts these too).
#[derive(Default)]
pub(super) struct OitPipes {
    /// `sun_rt` → the module, composed by the first variant built.
    modules: HashMap<bool, Arc<OnceLock<wgpu::ShaderModule>>>,
    sets: HashMap<ColorKey, DeferredPipeline<wgpu::RenderPipeline>>,
    composite_bgl: Option<wgpu::BindGroupLayout>,
    composite_layout: Option<wgpu::PipelineLayout>,
    composite_module: Option<Arc<OnceLock<wgpu::ShaderModule>>>,
    composite: HashMap<wgpu::TextureFormat, DeferredPipeline<wgpu::RenderPipeline>>,
}

impl OitPipes {
    /// The accumulation pipeline for `key`. Creating it registers a background build. The targets
    /// are the pass's own, so the image's format does not matter.
    fn set(
        &mut self,
        d: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        key: PipelineKey,
    ) -> DeferredPipeline<wgpu::RenderPipeline> {
        let color_key = (key.double_sided, key.sun_rt);
        if let Some(set) = self.sets.get(&color_key) {
            return set.clone();
        }
        let module = self.modules.entry(key.sun_rt).or_default().clone();
        let (device, layout) = (wgpu::Device::clone(d), layout.clone());
        let set = DeferredPipeline::new("lab oit", move || {
            let module = module.get_or_init(|| compile(&device, key.sun_rt));
            color_pipeline(&device, &layout, module, key.double_sided)
        });
        self.sets.insert(color_key, set.clone());
        set
    }

    fn composite_bgl(&mut self, d: &wgpu::Device) -> wgpu::BindGroupLayout {
        self.composite_bgl
            .get_or_insert_with(|| {
                let texture = |binding: u32| wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                };
                d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("oit composite bgl"),
                    entries: &[texture(0), texture(1)],
                })
            })
            .clone()
    }

    /// The composite for an image of `format`.
    fn composite(
        &mut self,
        d: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> DeferredPipeline<wgpu::RenderPipeline> {
        if let Some(pipeline) = self.composite.get(&format) {
            return pipeline.clone();
        }
        let bgl = self.composite_bgl(d);
        let layout = self
            .composite_layout
            .get_or_insert_with(|| {
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("oit composite layout"),
                    bind_group_layouts: &[Some(&bgl)],
                    immediate_size: 0,
                })
            })
            .clone();
        let module = self.composite_module.get_or_insert_with(Default::default).clone();
        let device = wgpu::Device::clone(d);
        let pipeline = DeferredPipeline::new("oit composite", move || {
            let module = module.get_or_init(|| {
                // The engine's own shader with no defines: `shader_check` validates it.
                shader_module(
                    &device,
                    "oit_composite",
                    include_str!("../../shaders/oit_composite.wgsl"),
                    "oit_composite.wgsl",
                    HashMap::new(),
                )
                .expect("oit composite shader")
            });
            composite_pipeline(&device, &layout, module, format)
        });
        self.composite.insert(format, pipeline.clone());
        pipeline
    }
}

/// The `OIT` forward module (alpha test compiled in, like the CPU-path forward module). The target
/// is always linear HDR.
fn compile(d: &wgpu::Device, sun_rt: bool) -> wgpu::ShaderModule {
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    defs.insert("OIT".to_string(), ShaderDefValue::Bool(true));
    defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    super::add_sun_rt_def(&mut defs, sun_rt);
    // The engine's own shader with a fixed define set: `shader_check` validates this combination,
    // so a failure here is a build defect, as for the other forward variants.
    shader_module(
        d,
        "lab_forward_oit",
        include_str!("../../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    )
    .expect("OIT forward variant")
}

fn color_pipeline(
    d: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    double_sided: bool,
) -> wgpu::RenderPipeline {
    let d = crate::device::TrackedDevice::observe(d);
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("lab oit"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[Some(LabVertex::layout())],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_oit"),
            targets: &crate::oit::accum_targets(),
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: if double_sided {
                None
            } else {
                Some(wgpu::Face::Back)
            },
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&d).as_deref(),
    })
}

fn composite_pipeline(
    d: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let d = crate::device::TrackedDevice::observe(d);
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("oit composite"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            targets: &[
                Some(wgpu::ColorTargetState {
                    format,
                    // The image's alpha holds the reflection code, as in every blended draw.
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                }),
                Some(wgpu::ColorTargetState {
                    format: REACTIVE_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                }),
            ],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&d).as_deref(),
    })
}

/// Whether a blended draw can go through the accumulation pass. The rest keep the sorted run: a
/// transmissive surface multiplies what is behind it, a mask or a film binds its own group 3,
/// an x-ray override draws through depth, additive light has no coverage to weigh, and a hair
/// card stays in the sorted blend.
fn takes_oit(draw: &Draw) -> bool {
    matches!(draw.alpha_mode, ALPHA_BLEND | ALPHA_PREMULTIPLIED)
        && !draw.transmissive
        && draw.glass.is_none()
        && !draw.xray
        && !draw.mask
        && !draw.film
        && !draw.hair
}

/// The pipeline a material-graph draw accumulates with, once its set built it. Asking starts the
/// build where it has not started (`DeferredPipeline::try_get`).
fn graph_pipeline<'a>(draw: &Draw, graph_sets: &'a GraphSets) -> Option<&'a wgpu::RenderPipeline> {
    let key = draw.graph?;
    graph_sets.get(&(key, draw.cull_off))?.oit.try_get()?.as_ref()
}

/// Whether a material's draws can go through the accumulation pass, as far as the material says.
/// Sorted hair is excluded and stays in the sorted blend.
fn material_takes_oit(material: &LabMaterial) -> bool {
    matches!(material.alpha_mode, ALPHA_BLEND | ALPHA_PREMULTIPLIED)
        && material.transmission <= 0.0
        && material.surface_graph.is_none()
        && !material.hair.enabled
}

/// The renderer's per-frame state: empty until a frame runs the pass.
#[derive(Default)]
pub(crate) struct OitState {
    /// The last main-view frame that could run the pass held such a draw, visible or not: warms
    /// build for it. Clears on a frame of a profile that does not ask.
    wanted: bool,
    stats: OitStats,
    /// The draws of the pass: its room stays between frames.
    draws: Vec<usize>,
    /// The composite's group per (accumulation, revealage).
    bg_cache: crate::bind_cache::BindCache<(u64, u64), wgpu::BindGroup>,
    /// The last main-view frame's reactive mask.
    mask: Option<wgpu::TextureView>,
}

impl OitState {
    /// What the last main-view frame did.
    pub(super) fn stats(&self) -> OitStats {
        self.stats
    }

    /// Whether a frame has drawn a translucent material under a profile that asks.
    pub(super) fn wanted(&self) -> bool {
        self.wanted
    }

    /// Takes back the pass's draw list, so its room serves the next frame.
    pub(super) fn finish(&mut self, pass: OitPass) {
        self.draws = pass.draws;
    }
}

/// What the frame's settings say about the pass.
pub(super) struct OitAsk {
    /// The frame's profile asks ([`crate::FrameQuality::order_independent_transparency`]).
    pub(super) requested: bool,
    /// Why this frame cannot run it, if it cannot.
    pub(super) blocked: Option<&'static str>,
    /// The frame's report: not a texture view's.
    pub(super) main_view: bool,
}

/// Why a frame of these settings cannot run the pass: no image to composite into, or targets it
/// cannot draw (the variant's targets are single-sample, and the accumulation and the revealage
/// blend differently).
pub(super) fn blocked_reason(
    device: &RenderDevice,
    requested: bool,
    linear_hdr: bool,
    samples: u32,
    lite: bool,
) -> Option<&'static str> {
    if !requested {
        None
    } else if !linear_hdr {
        Some("no_hdr_target")
    } else if samples != 1 {
        Some("msaa")
    } else if lite {
        Some("forward_lite")
    } else if !device
        .adapter
        .get_downlevel_capabilities()
        .flags
        .contains(wgpu::DownlevelFlags::INDEPENDENT_BLEND)
    {
        Some("no_independent_blend")
    } else {
        None
    }
}

/// The frame's pipelines and draws for the pass. `Default` is the empty frame the record site
/// leaves behind when it takes this one (`render_into`, ADR-4945).
#[derive(Default)]
pub(super) struct OitFrame {
    sets: [Option<DeferredPipeline<wgpu::RenderPipeline>>; 2],
    composite: Option<DeferredPipeline<wgpu::RenderPipeline>>,
    bgl: Option<wgpu::BindGroupLayout>,
    draws: Vec<usize>,
    /// A draw of the pass exists, visible or not: its targets stay held.
    hold: bool,
}

impl OitFrame {
    /// Whether the frame's translucent draws go through the pass: a held view's cached frame must
    /// not stand in across this changing (`ScreenCacheKey`).
    pub(super) fn runs(&self) -> bool {
        !self.draws.is_empty()
    }
}

/// Resolves the frame's accumulation pipelines like the thin film's: built here where this thread
/// may wait; mid-play a draw whose variant is not built yet stays in the sorted run this frame, so
/// a first use never stalls a frame. Moves the draws of the pass out of `blend_ix`.
pub(super) fn resolve_oit(
    pipes: &mut OitPipes,
    forward_layout: &wgpu::PipelineLayout,
    state: &mut OitState,
    d: &wgpu::Device,
    (key, key_ds): (PipelineKey, PipelineKey),
    (draws, blend_ix, graph_sets): (&[Draw], &mut Vec<usize>, &GraphSets),
    ask: &OitAsk,
) -> OitFrame {
    let mut pass_draws = std::mem::take(&mut state.draws);
    pass_draws.clear();
    let mut frame = OitFrame {
        sets: [None, None],
        composite: None,
        bgl: None,
        draws: pass_draws,
        hold: false,
    };
    if !ask.requested {
        if ask.main_view {
            state.wanted = false;
            state.mask = None;
            state.stats = OitStats {
                reason: Some("profile_off"),
                passes: state.stats.passes,
                ..OitStats::default()
            };
        }
        return frame;
    }
    let any = draws.iter().any(takes_oit);
    if ask.main_view {
        state.wanted = state.wanted || (ask.blocked.is_none() && any);
    }
    let visible = |ix: &[usize]| ix.iter().filter(|&&i| takes_oit(&draws[i])).count();
    let mut fallback = 0;
    let reason = if !any {
        Some("no_material")
    } else if let Some(blocked) = ask.blocked {
        fallback = visible(blend_ix.as_slice());
        Some(blocked)
    } else {
        let _span = tracing::info_span!("oit_resolve").entered();
        let _crumb = crate::breadcrumbs::frame_pass("oit_resolve");
        let set = pipes.set(d, forward_layout, key);
        let set_ds = draws
            .iter()
            .any(|draw| takes_oit(draw) && draw.cull_off)
            .then(|| pipes.set(d, forward_layout, key_ds));
        let composite = pipes.composite(d, key.format);
        if crate::pipeline_warm::may_block() {
            let jobs: Vec<_> = [Some(&set), set_ds.as_ref(), Some(&composite)]
                .into_iter()
                .flatten()
                .filter(|p| !p.is_built())
                .cloned()
                .map(|p| {
                    move || {
                        p.get();
                    }
                })
                .collect();
            crate::pipeline_warm::join_all(jobs);
        }
        if set.try_get().is_none()
            || set_ds.as_ref().is_some_and(|p| p.try_get().is_none())
            || composite.try_get().is_none()
        {
            fallback = visible(blend_ix.as_slice());
            Some("pipelines_building")
        } else {
            frame.bgl = Some(pipes.composite_bgl(d));
            frame.sets = [Some(set), set_ds];
            frame.composite = Some(composite);
            frame.hold = true;
            // Every translucent graph's variant, culled ones too: one that comes into view must not
            // start its build then.
            for draw in draws.iter().filter(|d| d.graph.is_some() && takes_oit(d)) {
                let _ = graph_pipeline(draw, graph_sets);
            }
            let moved = &mut frame.draws;
            // A graph draws sorted while its variant builds, like any draw mid-play.
            blend_ix.retain(|&index| {
                let draw = &draws[index];
                let takes =
                    takes_oit(draw) && (draw.graph.is_none() || graph_pipeline(draw, graph_sets).is_some());
                if takes {
                    moved.push(index);
                } else if takes_oit(draw) {
                    fallback += 1;
                }
                !takes
            });
            match (frame.draws.is_empty(), fallback) {
                (false, _) => None,
                (true, 0) => Some("no_visible_draw"),
                // Only graphs whose variant is not built yet are in view.
                (true, _) => Some("pipelines_building"),
            }
        }
    };
    if ask.main_view {
        state.mask = None;
        state.stats = OitStats {
            requested: true,
            drawn: frame.draws.len(),
            fallback,
            sorted: if frame.draws.is_empty() {
                0
            } else {
                blend_ix.len() - fallback
            },
            passes: state.stats.passes,
            covered_permille: 0,
            reactive_mask: false,
            reason,
        };
    }
    frame
}

/// The pass's targets, pipelines and group, ready to record.
pub(super) struct OitPass {
    accum: crate::TransientTexture,
    revealage: crate::TransientTexture,
    reactive: crate::TransientTexture,
    set: [Option<wgpu::RenderPipeline>; 2],
    composite: wgpu::RenderPipeline,
    group: wgpu::BindGroup,
    draws: Vec<usize>,
    /// `[x, y, width, height]` in pixels around the draws, for the composite.
    rect: [u32; 4],
}

impl OitPass {
    /// The mask the upscalers read.
    pub(super) fn reactive(&self) -> &wgpu::TextureView {
        &self.reactive.view
    }
}

/// What `prepare_oit` reads from the frame.
pub(super) struct OitPrepare<'a> {
    pub(super) hdr_view: &'a Option<wgpu::TextureView>,
    pub(super) draws: &'a [Draw],
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, super::MeshEntry>,
    pub(super) view_proj: regia_core::glam::Mat4,
    pub(super) internal: [u32; 2],
    pub(super) main_view: bool,
}

/// Takes the pass's targets from the pool and builds what `record_oit` binds. `None` when the frame
/// has no visible draw for it (a culled one still holds the targets, so its return into view
/// creates nothing on the frame path) or no such draw at all (nothing is taken).
pub(super) fn prepare_oit(
    state: &mut OitState,
    transients: &mut crate::TransientPool,
    d: &crate::TrackedDevice,
    frame: OitFrame,
    inputs: &OitPrepare<'_>,
) -> Option<OitPass> {
    let OitFrame {
        sets,
        composite,
        bgl,
        draws,
        hold,
    } = frame;
    let size = inputs
        .hdr_view
        .as_ref()
        .filter(|_| hold)
        .map(|view| view.texture().size());
    // Its groups hold the targets, and nothing asks the cache again on such a frame: dropped here,
    // the pool's eviction frees the memory.
    if !hold && inputs.main_view {
        state.bg_cache.clear();
    }
    let targets = size.map(|size| {
        let target = |pool: &mut crate::TransientPool,
                      format: wgpu::TextureFormat,
                      extra: wgpu::TextureUsages,
                      label: &str| {
            pool.acquire(
                d,
                &crate::TextureDesc::new(size.width.max(1), size.height.max(1), format).with_usage(
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | extra,
                ),
                label,
            )
        };
        let debug = crate::resource_inspector::debug_usage();
        (
            target(transients, ACCUM_FORMAT, debug, "oit accumulation"),
            target(transients, REVEALAGE_FORMAT, debug, "oit revealage"),
            // A host's own temporal pass may read the mask back.
            target(
                transients,
                REACTIVE_FORMAT,
                wgpu::TextureUsages::COPY_SRC,
                "oit reactive mask",
            ),
            size,
        )
    });
    let composite = composite.as_ref().and_then(|p| p.try_get().cloned());
    let (Some((accum, revealage, reactive, size)), Some(composite), Some(layout), false) =
        (targets, composite, bgl.as_ref(), draws.is_empty())
    else {
        state.draws = draws;
        return None;
    };
    let group = state
        .bg_cache
        .get_or_insert_with((accum.generation, revealage.generation), || {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("oit composite bg"),
                layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&accum.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&revealage.view),
                    },
                ],
            })
        });
    let rect = draw_bounds(
        inputs.draws,
        &draws,
        inputs.meshes,
        inputs.view_proj,
        inputs.internal,
    );
    if inputs.main_view {
        let pixels = u64::from(rect[2]) * u64::from(rect[3]);
        let image = u64::from(size.width.max(1)) * u64::from(size.height.max(1));
        state.stats.covered_permille = (pixels * 1000 / image) as u32;
        state.stats.reactive_mask = true;
        state.stats.passes += 1;
        state.mask = Some(reactive.view.clone());
    }
    let built = |set: &Option<DeferredPipeline<wgpu::RenderPipeline>>| {
        set.as_ref().and_then(|p| p.try_get().cloned())
    };
    Some(OitPass {
        set: [built(&sets[0]), built(&sets[1])],
        accum,
        revealage,
        reactive,
        composite,
        group,
        draws,
        rect,
    })
}

/// `[x, y, width, height]` in pixels around the pass's draws, at least one pixel. The full `size`
/// when a draw has no bound or crosses the near plane.
fn draw_bounds(
    draws: &[Draw],
    pass_draws: &[usize],
    meshes: &crate::key_hash::KeyMap<u64, super::MeshEntry>,
    view_proj: regia_core::glam::Mat4,
    size: [u32; 2],
) -> [u32; 4] {
    let full = [0, 0, size[0].max(1), size[1].max(1)];
    let mut union: Option<[f32; 4]> = None;
    for draw in pass_draws.iter().map(|&index| &draws[index]) {
        // A graph's vertex stage moves the surface past the mesh's bound.
        if draw.graph_vertex {
            return full;
        }
        let sphere = meshes
            .get(&draw.mesh)
            .and_then(|entry| super::gpu_cull::draw_sphere(draw, entry));
        let Some(rect) = sphere
            .and_then(|(center, radius)| super::subsurface_gpu::sphere_rect(center, radius, view_proj, size))
        else {
            return full;
        };
        union = Some(union.map_or(rect, |u| {
            [
                u[0].min(rect[0]),
                u[1].min(rect[1]),
                u[2].max(rect[2]),
                u[3].max(rect[3]),
            ]
        }));
    }
    let Some(u) = union else {
        return full;
    };
    let x0 = (u[0].floor().max(0.0) as u32).min(size[0].saturating_sub(1));
    let y0 = (u[1].floor().max(0.0) as u32).min(size[1].saturating_sub(1));
    let x1 = (u[2].ceil().max(0.0) as u32).clamp(x0 + 1, size[0].max(x0 + 1));
    let y1 = (u[3].ceil().max(0.0) as u32).clamp(y0 + 1, size[1].max(y0 + 1));
    [x0, y0, x1 - x0, y1 - y0]
}

/// What the accumulation draws read.
pub(super) struct OitDraws<'a> {
    pub(super) entries: &'a super::lifecycle::EntryLifecycle,
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, super::MeshEntry>,
    pub(super) materials: &'a crate::key_hash::KeyMap<u64, super::MaterialEntry>,
    pub(super) objects: &'a crate::key_hash::KeyMap<super::ObjectUniformKey, super::ObjectEntry>,
    pub(super) draws: &'a [Draw],
    pub(super) graph_sets: &'a GraphSets,
    pub(super) view_bg: &'a wgpu::BindGroup,
    pub(super) depth: &'a wgpu::TextureView,
    pub(super) color: &'a wgpu::TextureView,
    pub(super) texture_table_group: &'a Option<wgpu::BindGroup>,
    pub(super) viewport: [f32; 4],
    pub(super) scissor: [u32; 4],
    pub(super) trims: bool,
}

/// Records the accumulation pass and the composite. The forward pass has ended its render pass and
/// begins a new one after this.
pub(super) fn record_oit(
    encoder: &mut wgpu::CommandEncoder,
    pass: &OitPass,
    inputs: &OitDraws<'_>,
    binds: &mut crate::submission::BindCounts,
) -> crate::frame_stats::PassDrawCounts {
    let _span = tracing::info_span!("oit").entered();
    let _crumb = crate::breadcrumbs::frame_pass("oit");
    let counts = {
        let attachment = |view, clear| {
            Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })
        };
        let mut accum = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("oit accumulation"),
            color_attachments: &[
                attachment(&pass.accum.view, wgpu::Color::TRANSPARENT),
                attachment(&pass.revealage.view, wgpu::Color::WHITE),
            ],
            // Read-only: the draws test against the opaque depth and write none.
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: inputs.depth,
                depth_ops: None,
                stencil_ops: None,
            }),
            timestamp_writes: None,
            ..Default::default()
        });
        let mut tally =
            crate::frame_stats::DrawTally::with_texture_table(inputs.texture_table_group.as_ref());
        tally.set_bind_group(&mut accum, 0, inputs.view_bg, &[]);
        let [x, y, w, h] = inputs.viewport;
        accum.set_viewport(x, y, w, h, 0.0, 1.0);
        let [sx, sy, sw, sh] = inputs.scissor;
        accum.set_scissor_rect(sx, sy, sw, sh);
        let mut mesh_binds = crate::submission::PassBinds::new(inputs.trims);
        for &index in &pass.draws {
            let draw = &inputs.draws[index];
            let graph = draw
                .graph
                .and_then(|key| inputs.graph_sets.get(&(key, draw.cull_off)));
            let pipeline = match graph {
                Some(_) => graph_pipeline(draw, inputs.graph_sets),
                None => pass.set[usize::from(draw.cull_off)].as_ref(),
            };
            let Some(pipeline) = pipeline else {
                continue;
            };
            let Some(mesh) = inputs.meshes.get(&draw.mesh) else {
                missing_draw_entry(&inputs.entries.skips, "mesh", draw.mesh);
                continue;
            };
            let Some(object) = inputs.objects.get(&draw.uniform_key()) else {
                missing_draw_entry(&inputs.entries.skips, "object", draw.object);
                continue;
            };
            let Some(material) = inputs.materials.get(&draw.material) else {
                missing_draw_entry(&inputs.entries.skips, "material", draw.material);
                continue;
            };
            if graph.is_some_and(|graph| graph.uv1) {
                let Some(uv1) = mesh.uv1.as_ref() else {
                    continue;
                };
                mesh_binds.vertex(&mut accum, 1, uv1);
            }
            tally.set_pipeline(&mut accum, pipeline);
            tally.set_bind_group(&mut accum, 1, &object.bg, &[]);
            tally.set_bind_group(&mut accum, 2, &material.bg, material.offsets());
            mesh_binds.mesh(&mut accum, &mesh.vb, &mesh.ib);
            tally.draw_indexed(&mut accum, mesh.indices(), mesh.base_vertex, 0..1);
        }
        binds.add(mesh_binds.counts());
        tally.finish()
    };
    let mut composite = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("oit composite"),
        color_attachments: &[
            Some(wgpu::RenderPassColorAttachment {
                view: inputs.color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            }),
            Some(wgpu::RenderPassColorAttachment {
                view: &pass.reactive.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            }),
        ],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        ..Default::default()
    });
    composite.set_scissor_rect(pass.rect[0], pass.rect[1], pass.rect[2], pass.rect[3]);
    composite.set_pipeline(&pass.composite);
    composite.set_bind_group(0, &pass.group, &[]);
    composite.draw(0..3, 0..1);
    counts
}

/// The render pass the forward pass's blended run goes on in after the composite. Depth is read-only:
/// nothing blended writes it.
pub(super) fn begin_blended_run<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    color: &wgpu::TextureView,
    depth: &wgpu::TextureView,
) -> wgpu::RenderPass<'e> {
    encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("forward blended"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: color,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: depth,
            depth_ops: None,
            stencil_ops: None,
        }),
        timestamp_writes: None,
        ..Default::default()
    })
}

impl FrameRenderer {
    /// What the last main-view frame did with order-independent transparency (`render.oit`).
    #[must_use]
    pub fn oit_stats(&self) -> OitStats {
        self.oit.stats()
    }

    /// The reactive mask the last main-view frame wrote (red, 0..=1, at the render size), `None`
    /// when it ran no order-independent transparency. It holds the covered share of each pixel,
    /// scaled by [`crate::oit::REACTIVE_SCALE`]. The frame hands it to its upscaler; a host's own
    /// temporal pass may read it too, until the next frame.
    #[must_use]
    pub fn oit_reactive_mask(&self) -> Option<wgpu::TextureView> {
        self.oit.mask.clone()
    }

    /// Builds the accumulation and composite pipelines `materials` draw with for `target`, so a
    /// level's translucent material does not draw sorted on its first frames. [`Self::prewarm`]
    /// calls this for the HDR targets of a profile that asks.
    ///
    /// # Errors
    ///
    /// The warm queue running this shut down ([`crate::warm_cancelled`]).
    pub(super) fn prewarm_oit_materials(
        &mut self,
        device: &RenderDevice,
        target: GlassPipelineTarget,
        materials: &[&LabMaterial],
    ) -> Result<(), RenderError> {
        let d = &device.device;
        let sun_rt = self.culler.forward_sun_rt();
        let Some(pipes) = self.pipes.as_mut() else {
            return Ok(());
        };
        if target.samples > 1 || target.encode {
            return Ok(());
        }
        let layout = pipes.forward_layout.clone();
        for material in materials.iter().filter(|m| material_takes_oit(m)) {
            let key = PipelineKey {
                format: target.format,
                encode: target.encode,
                double_sided: material.double_sided,
                samples: 1,
                sun_rt,
                studio_off: false,
                restir_di: false,
            };
            let set = pipes.oit.set(d, &layout, key);
            let composite = pipes.oit.composite(d, target.format);
            crate::pipeline_warm::warm_checkpoint()?;
            set.get();
            composite.get();
        }
        Ok(())
    }

    /// Game-frame warms: the accumulation pipelines of both cull modes and the composite of every
    /// single-sample HDR forward target the warm drew, the other forward module included. Nothing
    /// unless a translucent material showed up under a profile that asks. Returns how many
    /// pipelines it asked for.
    pub(crate) fn prewarm_oit(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        if !self.oit.wanted() {
            return Ok(0);
        }
        let d = &device.device;
        // The trace can give way to the shadow map mid-play: its other module too (ADR-2035).
        let both_modules = self.sun_trace.report().selection.request != crate::hwrt::RtShadowRequest::Off;
        let Some(pipes) = self.pipes.as_mut() else {
            return Ok(0);
        };
        let layout = pipes.forward_layout.clone();
        let keys: Vec<PipelineKey> = pipes
            .sets
            .keys()
            .filter(|key| key.samples == 1 && !key.encode && super::subsurface_gpu::scatters_into(key.format))
            .copied()
            .collect();
        let mut asked = 0;
        for key in keys {
            crate::pipeline_warm::warm_checkpoint()?;
            pipes.oit.set(d, &layout, key).get();
            if both_modules {
                let twin = PipelineKey {
                    sun_rt: !key.sun_rt,
                    ..key
                };
                pipes.oit.set(d, &layout, twin).get();
                asked += 1;
            }
            pipes.oit.composite(d, key.format).get();
            asked += 2;
        }
        Ok(asked)
    }
}

#[cfg(test)]
mod tests {
    use crate::oit::{ACCUM_FORMAT, REVEALAGE_FORMAT, accum_targets};

    /// The targets blend as the paper says: a sum, and a product of what is let through.
    #[test]
    fn the_accumulation_targets_sum_and_multiply() {
        let [accum, revealage] = accum_targets();
        let accum = accum.expect("accumulation");
        assert_eq!(accum.format, ACCUM_FORMAT);
        let sum = accum.blend.expect("blended").color;
        assert_eq!(sum.src_factor, wgpu::BlendFactor::One);
        assert_eq!(sum.dst_factor, wgpu::BlendFactor::One);
        let revealage = revealage.expect("revealage");
        assert_eq!(revealage.format, REVEALAGE_FORMAT);
        let product = revealage.blend.expect("blended").color;
        assert_eq!(product.src_factor, wgpu::BlendFactor::Zero);
        assert_eq!(product.dst_factor, wgpu::BlendFactor::OneMinusSrc);
    }

    /// Sorted hair is a blended material and still stays out of the accumulation pass.
    #[test]
    fn sorted_hair_stays_in_the_sorted_blend() {
        let hair = super::LabMaterial {
            alpha_mode: super::ALPHA_BLEND,
            hair: crate::HairFactors {
                enabled: true,
                alpha: crate::HairAlpha::Sorted,
                ..crate::HairFactors::OFF
            },
            ..super::LabMaterial::default()
        };
        assert!(!super::material_takes_oit(&hair));
        let blend = super::LabMaterial {
            alpha_mode: super::ALPHA_BLEND,
            ..super::LabMaterial::default()
        };
        assert!(super::material_takes_oit(&blend));
    }

    /// Both formats blend on every device without a feature.
    #[test]
    fn the_targets_are_blendable_formats() {
        for format in [ACCUM_FORMAT, REVEALAGE_FORMAT] {
            assert!(
                format
                    .guaranteed_format_features(wgpu::Features::empty())
                    .flags
                    .contains(wgpu::TextureFormatFeatureFlags::BLENDABLE),
                "{format:?}"
            );
        }
    }
}
