// SPDX-License-Identifier: MPL-2.0
//! GPU side of screen-space subsurface scattering ([`crate::subsurface`], ADR-2049): the
//! `SUBSURFACE` variant of the forward shader, its pass and the two blur passes.
//!
//! Only draws of a material with the factors touch any of this. Every other draw keeps its
//! pipelines and bind groups, and a frame with no scattering draw records nothing here and takes
//! nothing from the transient pool.
//!
//! The scattering draws go first (`record_subsurface`), into the HDR image and two pooled targets,
//! and the blur adds their light to the image before the forward pass loads it. The frame's
//! `after_opaque` slot comes after the forward pass's blended draws, too late for a blur that adds
//! light under them (ADR-2049). These draws take the CPU colour loop, never the GPU-driven batches
//! or the visibility buffer, as a thin film does; their depth prepass and shadows stay as they were.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use naga_oil::compose::ShaderDefValue;

use super::lifecycle::missing_draw_entry;
use super::{
    Draw, FrameRenderer, GlassPipelineTarget, LabMaterial, LabVertex, PipelineKey, RenderError, shader_module,
};
use crate::RenderDevice;
use crate::pipeline_warm::DeferredPipeline;
use crate::subsurface::{
    SUBSURFACE_PROFILES, SUBSURFACE_TAPS, SubsurfaceStats, SubsurfaceUnsupported, kernel_table,
};

/// The light that scatters (rgb) and 1 (alpha): a filtered read's alpha is its share on these pixels.
const LIGHT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The albedo (rgb) and the reach in world units (alpha), negative for the second profile, 0 outside
/// these pixels.
const ALBEDO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// `ScatterParams` in `subsurface_scatter.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ScatterUniform {
    taps: [[f32; 4]; SUBSURFACE_TAPS * SUBSURFACE_PROFILES],
    view: [f32; 4],
    size: [f32; 4],
}

/// The two blur passes of one colour format.
#[derive(Clone)]
pub(super) struct ScatterSet {
    horizontal: DeferredPipeline<wgpu::RenderPipeline>,
    vertical: DeferredPipeline<wgpu::RenderPipeline>,
}

/// The layout and pipelines (kept in [`super::Pipes`], so a renderer that adopts warmed pipelines
/// adopts these too).
#[derive(Default)]
pub(super) struct SubsurfacePipes {
    /// `sun_rt` → the module, composed by the first variant built. The target is always linear HDR.
    modules: HashMap<bool, Arc<OnceLock<wgpu::ShaderModule>>>,
    sets: HashMap<PipelineKey, DeferredPipeline<wgpu::RenderPipeline>>,
    scatter_bgl: Option<wgpu::BindGroupLayout>,
    scatter_sampler: Option<wgpu::Sampler>,
    scatter_layout: Option<wgpu::PipelineLayout>,
    scatter_module: Option<Arc<OnceLock<wgpu::ShaderModule>>>,
    scatter: HashMap<wgpu::TextureFormat, ScatterSet>,
}

impl SubsurfacePipes {
    /// The colour pipeline for `key`. Creating it registers a background build.
    fn set(
        &mut self,
        d: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        key: PipelineKey,
    ) -> DeferredPipeline<wgpu::RenderPipeline> {
        if let Some(set) = self.sets.get(&key) {
            return set.clone();
        }
        let module = self.modules.entry(key.sun_rt).or_default().clone();
        let (device, layout) = (wgpu::Device::clone(d), layout.clone());
        let set = DeferredPipeline::new("lab subsurface", move || {
            let module = module.get_or_init(|| compile(&device, key.sun_rt));
            color_pipeline(&device, &layout, module, key)
        });
        self.sets.insert(key, set.clone());
        set
    }

    fn scatter_bgl(&mut self, d: &wgpu::Device) -> wgpu::BindGroupLayout {
        self.scatter_bgl
            .get_or_insert_with(|| {
                let texture =
                    |binding: u32, sample_type: wgpu::TextureSampleType| wgpu::BindGroupLayoutEntry {
                        binding,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    };
                d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("subsurface scatter bgl"),
                    entries: &[
                        texture(0, wgpu::TextureSampleType::Float { filterable: true }),
                        texture(1, wgpu::TextureSampleType::Float { filterable: false }),
                        texture(2, wgpu::TextureSampleType::Depth),
                        wgpu::BindGroupLayoutEntry {
                            binding: 3,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 4,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                })
            })
            .clone()
    }

    fn scatter_sampler(&mut self, d: &wgpu::Device) -> wgpu::Sampler {
        self.scatter_sampler
            .get_or_insert_with(|| {
                d.create_sampler(&wgpu::SamplerDescriptor {
                    label: Some("subsurface scatter sampler"),
                    address_mode_u: wgpu::AddressMode::ClampToEdge,
                    address_mode_v: wgpu::AddressMode::ClampToEdge,
                    mag_filter: wgpu::FilterMode::Linear,
                    min_filter: wgpu::FilterMode::Linear,
                    ..Default::default()
                })
            })
            .clone()
    }

    /// The blur passes for a colour target of `format`.
    fn scatter(&mut self, d: &wgpu::Device, format: wgpu::TextureFormat) -> ScatterSet {
        if let Some(set) = self.scatter.get(&format) {
            return set.clone();
        }
        let bgl = self.scatter_bgl(d);
        let layout = self
            .scatter_layout
            .get_or_insert_with(|| {
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("subsurface scatter layout"),
                    bind_group_layouts: &[Some(&bgl)],
                    immediate_size: 0,
                })
            })
            .clone();
        let module = self.scatter_module.get_or_insert_with(Default::default).clone();
        let pass = |label: &'static str,
                    entry: &'static str,
                    target: wgpu::TextureFormat,
                    blend: Option<wgpu::BlendState>| {
            let (device, layout, module) = (wgpu::Device::clone(d), layout.clone(), module.clone());
            DeferredPipeline::new(label, move || {
                let module = module.get_or_init(|| {
                    // The engine's own shader with no defines: `shader_check` validates it.
                    shader_module(
                        &device,
                        "subsurface_scatter",
                        include_str!("../../shaders/subsurface_scatter.wgsl"),
                        "subsurface_scatter.wgsl",
                        HashMap::new(),
                    )
                    .expect("subsurface scatter shader")
                });
                let device = crate::device::TrackedDevice::observe(&device);
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module,
                        entry_point: Some("vs_main"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module,
                        entry_point: Some(entry),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: target,
                            blend,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: crate::pipeline_cache::cache_for(&device).as_deref(),
                })
            })
        };
        // The light adds to what the colour target kept. Its alpha holds the reflection code.
        let add_keep_alpha = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let set = ScatterSet {
            horizontal: pass("subsurface blur x", "fs_horizontal", LIGHT_FORMAT, None),
            vertical: pass("subsurface blur y", "fs_vertical", format, Some(add_keep_alpha)),
        };
        self.scatter.insert(format, set.clone());
        set
    }
}

/// Whether a frame with this colour format is linear HDR: the only frames that scatter.
pub(super) fn scatters_into(format: wgpu::TextureFormat) -> bool {
    matches!(
        format,
        wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rg11b10Ufloat
    )
}

/// The `SUBSURFACE` forward module (alpha test compiled in, like the CPU-path forward module).
fn compile(d: &wgpu::Device, sun_rt: bool) -> wgpu::ShaderModule {
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    defs.insert("SUBSURFACE".to_string(), ShaderDefValue::Bool(true));
    defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    super::add_sun_rt_def(&mut defs, sun_rt);
    // The engine's own shader with a fixed define set: `shader_check` validates this combination,
    // so a failure here is a build defect, as for the other forward variants.
    shader_module(
        d,
        "lab_forward_subsurface",
        include_str!("../../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    )
    .expect("SUBSURFACE forward variant")
}

fn color_pipeline(
    d: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    key: PipelineKey,
) -> wgpu::RenderPipeline {
    let d = crate::device::TrackedDevice::observe(d);
    let target = |format: wgpu::TextureFormat| {
        Some(wgpu::ColorTargetState {
            format,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        })
    };
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("lab subsurface"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[Some(LabVertex::layout())],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_subsurface"),
            targets: &[target(key.format), target(LIGHT_FORMAT), target(ALBEDO_FORMAT)],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: if key.double_sided {
                None
            } else {
                Some(wgpu::Face::Back)
            },
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&d).as_deref(),
    })
}

/// The renderer's per-frame state: empty until a frame scatters.
#[derive(Default)]
pub(crate) struct SubsurfaceState {
    /// Draws of a scattering material this path cannot shade, this frame's draw list.
    unsupported: usize,
    /// [`Self::unsupported`] by reason ([`SubsurfaceUnsupported::index`], ADR-3170).
    unsupported_by: [u32; SubsurfaceUnsupported::COUNT],
    /// The last main-view frame that could scatter held such a draw, visible or not: warms build
    /// for it. Clears on a frame of a profile that does not ask.
    wanted: bool,
    stats: SubsurfaceStats,
    params: Option<wgpu::Buffer>,
    param_bytes: Vec<u8>,
    /// `[horizontal, vertical]` per (light, blurred, albedo, depth).
    bg_cache: crate::bind_cache::BindCache<(u64, u64, u64, wgpu::TextureView), [wgpu::BindGroup; 2]>,
}

impl SubsurfaceState {
    /// Starts a frame's draw list.
    pub(super) fn begin_frame(&mut self) {
        self.unsupported = 0;
        self.unsupported_by = [0; SubsurfaceUnsupported::COUNT];
    }

    /// Counts a draw of a scattering material the variant cannot shade, and why.
    pub(super) fn note_unsupported(&mut self, reason: SubsurfaceUnsupported) {
        self.unsupported += 1;
        self.unsupported_by[reason.index()] += 1;
    }

    /// Whether a frame has drawn a scattering material under a profile that asks.
    pub(super) fn wanted(&self) -> bool {
        self.wanted
    }

    /// What the last main-view frame did.
    pub(super) fn stats(&self) -> SubsurfaceStats {
        self.stats
    }
}

/// The frame's scattering pipelines: `None` when nothing scatters.
pub(super) struct SubsurfaceFrame {
    sets: [Option<DeferredPipeline<wgpu::RenderPipeline>>; 2],
    scatter: Option<ScatterSet>,
    bgl: Option<wgpu::BindGroupLayout>,
    sampler: Option<wgpu::Sampler>,
}

/// What the frame's settings say about scattering.
pub(super) struct SubsurfaceAsk {
    /// The frame's profile asks ([`crate::FrameQuality::subsurface_scattering`]).
    pub(super) requested: bool,
    /// Why this frame cannot scatter, if it cannot.
    pub(super) blocked: Option<&'static str>,
    /// The frame's report: not a texture view's.
    pub(super) main_view: bool,
}

/// Why a frame of these settings cannot scatter (a step of `render_into` moved here by ADR-1562's
/// ceiling): no linear HDR image to blur, a multisampled target, or a Mobile lite forward tier.
pub(super) fn blocked_reason(
    linear_hdr: bool,
    samples: u32,
    forward_tier: crate::forward_lite::ForwardLite,
) -> Option<&'static str> {
    if !linear_hdr {
        Some("no_hdr_target")
    } else if samples != 1 {
        Some("msaa")
    } else if forward_tier != crate::forward_lite::ForwardLite::Off {
        Some("forward_lite")
    } else {
        None
    }
}

/// The `SUBSURFACE` variants of the frame's scattering draws, resolved like the thin film's: built
/// here where this thread may wait; mid-play a draw whose variant is not built yet draws without
/// the blur this frame (its `subsurface` is cleared), so a first use never stalls a frame.
pub(super) fn resolve_subsurface(
    pipes: &mut SubsurfacePipes,
    forward_layout: &wgpu::PipelineLayout,
    state: &mut SubsurfaceState,
    d: &wgpu::Device,
    (key, key_ds): (PipelineKey, PipelineKey),
    draws: &mut [Draw],
    ask: &SubsurfaceAsk,
) -> SubsurfaceFrame {
    let mut frame = SubsurfaceFrame {
        sets: [None, None],
        scatter: None,
        bgl: None,
        sampler: None,
    };
    let (mut drawn, mut fallback) = (0, 0);
    let mut reason = None;
    if ask.main_view {
        state.wanted = ask.requested
            && (state.wanted || (ask.blocked.is_none() && draws.iter().any(|draw| draw.subsurface)));
    }
    if !ask.requested {
        reason = Some("profile_off");
    } else if !draws.iter().any(|draw| draw.subsurface) {
        // A lite forward frame drops each scattering draw as unsupported (`forward_lite`, ADR-3170)
        // before this point, so "no material" would hide the tier that dropped them.
        let lite_dropped = state.unsupported_by[SubsurfaceUnsupported::ForwardLite.index()] > 0;
        reason = Some(if lite_dropped && ask.blocked == Some("forward_lite") {
            "forward_lite"
        } else {
            "no_material"
        });
    } else if let Some(blocked) = ask.blocked {
        reason = Some(blocked);
    } else {
        let _span = tracing::info_span!("subsurface_resolve").entered();
        let _crumb = crate::breadcrumbs::frame_pass("subsurface_resolve");
        let set = pipes.set(d, forward_layout, key);
        let set_ds = draws
            .iter()
            .any(|draw| draw.subsurface && draw.cull_off)
            .then(|| pipes.set(d, forward_layout, key_ds));
        let scatter = pipes.scatter(d, key.format);
        frame.bgl = Some(pipes.scatter_bgl(d));
        frame.sampler = Some(pipes.scatter_sampler(d));
        if crate::pipeline_warm::may_block() {
            let jobs: Vec<_> = [
                Some(&set),
                set_ds.as_ref(),
                Some(&scatter.horizontal),
                Some(&scatter.vertical),
            ]
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
        let ready = set.try_get().is_some()
            && set_ds.as_ref().is_none_or(|p| p.try_get().is_some())
            && scatter.horizontal.try_get().is_some()
            && scatter.vertical.try_get().is_some();
        if ready {
            frame.sets = [Some(set), set_ds];
            frame.scatter = Some(scatter);
            drawn = draws
                .iter()
                .filter(|draw| draw.subsurface && draw.visible)
                .count();
            if drawn == 0 {
                reason = Some("no_visible_draw");
            }
        } else {
            reason = Some("pipelines_building");
        }
    }
    if frame.scatter.is_none() {
        for draw in draws.iter_mut().filter(|draw| draw.subsurface) {
            draw.subsurface = false;
            if draw.visible && ask.requested {
                fallback += 1;
            }
        }
    }
    if ask.main_view {
        state.stats = SubsurfaceStats {
            requested: ask.requested,
            drawn,
            fallback,
            unsupported: state.unsupported,
            unsupported_by: state.unsupported_by,
            passes: state.stats.passes,
            covered_permille: if drawn > 0 {
                state.stats.covered_permille
            } else {
                0
            },
            reason,
        };
    }
    frame
}

/// `[x, y, width, height]` in pixels around the frame's scattering draws, at least one pixel. The
/// full `size` when a draw has no bound or crosses the near plane.
fn draw_bounds(
    draws: &[Draw],
    opaque_ix: &[usize],
    meshes: &crate::key_hash::KeyMap<u64, super::MeshEntry>,
    view_proj: regia_core::glam::Mat4,
    size: [u32; 2],
) -> [u32; 4] {
    let full = [0, 0, size[0].max(1), size[1].max(1)];
    let mut union: Option<[f32; 4]> = None;
    for draw in opaque_ix
        .iter()
        .map(|&index| &draws[index])
        .filter(|draw| draw.subsurface)
    {
        let sphere = meshes
            .get(&draw.mesh)
            .and_then(|entry| super::gpu_cull::draw_sphere(draw, entry));
        let Some(rect) = sphere.and_then(|(center, radius)| sphere_rect(center, radius, view_proj, size))
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

/// The pixel rectangle `[x0, y0, x1, y1]` around a world-space sphere, through the corners of its
/// bounding box. `None` when a corner is behind the eye (the projection no longer bounds it).
pub(super) fn sphere_rect(
    center: [f32; 3],
    radius: f32,
    view_proj: regia_core::glam::Mat4,
    size: [u32; 2],
) -> Option<[f32; 4]> {
    let mut rect = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for corner in 0..8u32 {
        let sign = |bit: u32| if corner & (1 << bit) == 0 { -1.0 } else { 1.0 };
        let clip = view_proj
            * regia_core::glam::Vec4::new(
                center[0] + sign(0) * radius,
                center[1] + sign(1) * radius,
                center[2] + sign(2) * radius,
                1.0,
            );
        if clip.w <= 1e-4 {
            return None;
        }
        let x = (clip.x / clip.w * 0.5 + 0.5) * size[0] as f32;
        let y = (0.5 - clip.y / clip.w * 0.5) * size[1] as f32;
        rect = [rect[0].min(x), rect[1].min(y), rect[2].max(x), rect[3].max(y)];
    }
    Some(rect)
}

/// The renderer fields the scattering passes read and write.
pub(super) struct SubsurfaceFields<'a> {
    pub(super) state: &'a mut SubsurfaceState,
    pub(super) transients: &'a mut crate::TransientPool,
    pub(super) entries: &'a mut super::lifecycle::EntryLifecycle,
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, super::MeshEntry>,
    pub(super) materials: &'a crate::key_hash::KeyMap<u64, super::MaterialEntry>,
    pub(super) objects: &'a crate::key_hash::KeyMap<super::ObjectUniformKey, super::ObjectEntry>,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
}

/// What the scattering passes read from the frame.
pub(super) struct SubsurfaceInputs<'a> {
    pub(super) scene: &'a super::FrameScene<'a>,
    pub(super) load_color: bool,
    pub(super) draws: &'a [Draw],
    pub(super) opaque_ix: &'a [usize],
    pub(super) view_bg: &'a wgpu::BindGroup,
    pub(super) depth_view: &'a wgpu::TextureView,
    pub(super) hdr_view: &'a Option<wgpu::TextureView>,
    pub(super) texture_table_group: &'a Option<wgpu::BindGroup>,
    pub(super) unjittered_proj: regia_core::glam::Mat4,
    pub(super) view_proj: regia_core::glam::Mat4,
    pub(super) internal_w: u32,
    pub(super) internal_h: u32,
    pub(super) pre_exposure: f32,
    /// The blur's noise frame ([`super::temporal_noise_index`]): 0 without TAA.
    pub(super) noise_index: f32,
    pub(super) trims: bool,
    pub(super) main_view: bool,
}

/// Records the scattering draws and the blur into the HDR image. Returns what they drew, `None`
/// when the frame has no visible scattering draw (nothing was recorded, and the forward pass clears
/// as it always does). While a scattering draw is only culled the pooled targets stay held, so it
/// coming back into view creates nothing on the frame path; a frame with no such draw takes none.
pub(super) fn record_subsurface(
    fields: SubsurfaceFields<'_>,
    d: &crate::TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    frame: &SubsurfaceFrame,
    inputs: &SubsurfaceInputs<'_>,
    binds: &mut crate::submission::BindCounts,
) -> Result<Option<crate::frame_stats::PassDrawCounts>, RenderError> {
    let SubsurfaceFields {
        state,
        transients,
        entries,
        meshes,
        materials,
        objects,
        belt,
    } = fields;
    let SubsurfaceInputs {
        scene,
        load_color,
        draws,
        opaque_ix,
        view_bg,
        depth_view,
        hdr_view,
        texture_table_group,
        unjittered_proj,
        view_proj,
        internal_w,
        internal_h,
        pre_exposure,
        noise_index,
        trims,
        main_view,
    } = *inputs;
    let (Some(scatter), Some(bgl), Some(sampler), Some(color)) = (
        frame.scatter.as_ref(),
        frame.bgl.as_ref(),
        frame.sampler.as_ref(),
        hdr_view.as_ref(),
    ) else {
        return Ok(None);
    };
    let (Some(horizontal), Some(vertical)) = (scatter.horizontal.try_get(), scatter.vertical.try_get())
    else {
        return Ok(None);
    };
    let _span = tracing::info_span!("subsurface").entered();
    let size = color.texture().size();
    let target = |format: wgpu::TextureFormat, label: &str, pool: &mut crate::TransientPool| {
        pool.acquire(
            d,
            &crate::TextureDesc::new(size.width.max(1), size.height.max(1), format).with_usage(
                wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | crate::resource_inspector::debug_usage(),
            ),
            label,
        )
    };
    let light = target(LIGHT_FORMAT, "subsurface light", transients);
    let albedo = target(ALBEDO_FORMAT, "subsurface albedo", transients);
    let blurred = target(LIGHT_FORMAT, "subsurface blur", transients);
    if !opaque_ix.iter().any(|&index| draws[index].subsurface) {
        return Ok(None);
    }

    // The scattering draws: the rest of their light, the light that scatters, the albedo.
    let draw_counts = {
        let _crumb = crate::breadcrumbs::frame_pass("subsurface");
        let black = wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT);
        let attachment = |view, load| {
            Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })
        };
        let color_load = if load_color {
            wgpu::LoadOp::Load
        } else {
            wgpu::LoadOp::Clear(super::forward_pass::forward_clear(pre_exposure))
        };
        let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("subsurface draws"),
            color_attachments: &[
                attachment(color, color_load),
                attachment(&light.view, black),
                attachment(&albedo.view, black),
            ],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: if scene.depth_prepass {
                        wgpu::LoadOp::Load
                    } else {
                        wgpu::LoadOp::Clear(1.0)
                    },
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            ..Default::default()
        });
        let mut tally = crate::frame_stats::DrawTally::with_texture_table(texture_table_group.as_ref());
        tally.set_bind_group(&mut pass, 0, view_bg, &[]);
        pass.set_viewport(0.0, 0.0, internal_w as f32, internal_h as f32, 0.0, 1.0);
        pass.set_scissor_rect(
            0,
            0,
            internal_w.clamp(1, size.width.max(1)),
            internal_h.clamp(1, size.height.max(1)),
        );
        let mut mesh_binds = crate::submission::PassBinds::new(trims);
        for &index in opaque_ix {
            let draw = &draws[index];
            if !draw.subsurface {
                continue;
            }
            let Some(pipeline) = frame.sets[usize::from(draw.cull_off)]
                .as_ref()
                .and_then(DeferredPipeline::try_get)
            else {
                continue;
            };
            let Some(mesh) = meshes.get(&draw.mesh) else {
                missing_draw_entry(&entries.skips, "mesh", draw.mesh);
                continue;
            };
            let Some(object) = objects.get(&draw.uniform_key()) else {
                missing_draw_entry(&entries.skips, "object", draw.object);
                continue;
            };
            let Some(material) = materials.get(&draw.material) else {
                missing_draw_entry(&entries.skips, "material", draw.material);
                continue;
            };
            tally.set_pipeline(&mut pass, pipeline);
            tally.set_bind_group(&mut pass, 1, &object.bg, &[]);
            tally.set_bind_group(&mut pass, 2, &material.bg, material.offsets());
            mesh_binds.mesh(&mut pass, &mesh.vb, &mesh.ib);
            tally.draw_indexed(&mut pass, mesh.indices(), mesh.base_vertex, 0..1);
        }
        binds.add(mesh_binds.counts());
        tally.finish()
    };

    // The blur, from the scattering surface's pixels only.
    let params = ScatterUniform {
        taps: kernel_table(),
        view: [
            unjittered_proj.z_axis.z,
            unjittered_proj.w_axis.z,
            unjittered_proj.y_axis.y * internal_h as f32 * 0.5,
            if unjittered_proj.w_axis.w > 0.5 { 1.0 } else { 0.0 },
        ],
        // The kernel's word (ADR-3170): 0 keeps the screen kernel, so default frames do not move.
        size: [
            size.width as f32,
            size.height as f32,
            noise_index,
            scene.quality.subsurface_kernel.word(internal_w),
        ],
    };
    let params_buf = state
        .params
        .get_or_insert_with(|| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("subsurface params"),
                size: std::mem::size_of::<ScatterUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        })
        .clone();
    belt.write_if_changed(
        d,
        encoder,
        &params_buf,
        bytemuck::bytes_of(&params),
        &mut state.param_bytes,
    );
    let [bg_x, bg_y] = state.bg_cache.get_or_insert_with(
        (
            light.generation,
            blurred.generation,
            albedo.generation,
            depth_view.clone(),
        ),
        || {
            let group = |source: &wgpu::TextureView| {
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("subsurface scatter bg"),
                    layout: bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(source),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&albedo.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(depth_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: params_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                    ],
                })
            };
            [group(&light.view), group(&blurred.view)]
        },
    );
    // The blur writes only the scattering pixels, so their bounds are all it has to cover.
    let rect = draw_bounds(draws, opaque_ix, meshes, view_proj, [internal_w, internal_h]);
    let rect_pixels = u64::from(rect[2]) * u64::from(rect[3]);
    let image_pixels = u64::from(size.width.max(1)) * u64::from(size.height.max(1));
    let mut blur = |label: &'static str,
                    view: &wgpu::TextureView,
                    load: wgpu::LoadOp<wgpu::Color>,
                    pipeline: &wgpu::RenderPipeline,
                    bg: &wgpu::BindGroup| {
        let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            ..Default::default()
        });
        pass.set_scissor_rect(rect[0], rect[1], rect[2], rect[3]);
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bg, &[]);
        pass.draw(0..3, 0..1);
    };
    blur(
        "subsurface blur x",
        &blurred.view,
        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        horizontal,
        &bg_x,
    );
    blur("subsurface blur y", color, wgpu::LoadOp::Load, vertical, &bg_y);
    if main_view {
        state.stats.passes += 1;
        state.stats.covered_permille = (rect_pixels * 1000 / image_pixels) as u32;
    }
    Ok(Some(draw_counts))
}

impl FrameRenderer {
    /// What the last main-view frame did with subsurface scattering (`render.subsurface`).
    #[must_use]
    pub fn subsurface_stats(&self) -> SubsurfaceStats {
        self.subsurface.stats()
    }

    /// Builds the scattering pipelines `materials` draw with for `target`, so a level's scattering
    /// material does not draw without the blur on its first frames. [`Self::prewarm`] calls this
    /// for the HDR targets of a profile that asks.
    ///
    /// # Errors
    ///
    /// The warm queue running this shut down ([`crate::warm_cancelled`]).
    pub(super) fn prewarm_subsurface_materials(
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
        for mat in materials {
            let key = PipelineKey {
                format: target.format,
                encode: target.encode,
                double_sided: mat.double_sided,
                samples: 1,
                sun_rt,
                studio_off: false,
                restir_di: false,
            };
            let set = pipes.subsurface.set(d, &layout, key);
            let scatter = pipes.subsurface.scatter(d, target.format);
            crate::pipeline_warm::warm_checkpoint()?;
            set.get();
            scatter.horizontal.get();
            scatter.vertical.get();
        }
        Ok(())
    }

    /// Game-frame warms: the scattering pipelines of every single-sample HDR forward target the
    /// warm drew, the other forward module included. Nothing unless a material asked for them
    /// under a profile that scatters. Returns how many pipelines it asked for.
    pub(crate) fn prewarm_subsurface(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        if !self.subsurface.wanted() {
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
            .filter(|key| key.samples == 1 && !key.encode && scatters_into(key.format))
            .copied()
            .collect();
        let mut asked = 0;
        for key in keys {
            crate::pipeline_warm::warm_checkpoint()?;
            pipes.subsurface.set(d, &layout, key).get();
            if both_modules {
                let twin = PipelineKey {
                    sun_rt: !key.sun_rt,
                    ..key
                };
                pipes.subsurface.set(d, &layout, twin).get();
                asked += 1;
            }
            let scatter = pipes.subsurface.scatter(d, key.format);
            scatter.horizontal.get();
            scatter.vertical.get();
            asked += 3;
        }
        Ok(asked)
    }
}

#[cfg(test)]
mod tests {
    use super::sphere_rect;
    use regia_core::glam::camera::rh::proj::directx::perspective;
    use regia_core::glam::camera::rh::view::look_at_mat4;
    use regia_core::glam::{Mat4, Vec3};

    fn camera() -> Mat4 {
        perspective(1.0, 1.0, 0.1, 100.0) * look_at_mat4(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y)
    }

    /// The sphere's whole outline is inside the rectangle, around the view's center and off it.
    #[test]
    fn the_rectangle_holds_the_sphere_outline() {
        let view_proj = camera();
        let size = [640u32, 480];
        for (center, radius) in [
            ([0.0, 0.0, 0.0], 0.5),
            ([1.0, -0.5, 1.0], 0.25),
            ([-1.5, 1.0, -3.0], 1.0),
        ] {
            let rect = sphere_rect(center, radius, view_proj, size).expect("in front");
            for i in 0..64 {
                let a = i as f32 / 64.0 * std::f32::consts::TAU;
                for (dir, up) in [(Vec3::X, Vec3::Y), (Vec3::X, Vec3::Z), (Vec3::Y, Vec3::Z)] {
                    let p = Vec3::from_array(center) + radius * (dir * a.cos() + up * a.sin());
                    let clip = view_proj * p.extend(1.0);
                    let x = (clip.x / clip.w * 0.5 + 0.5) * size[0] as f32;
                    let y = (0.5 - clip.y / clip.w * 0.5) * size[1] as f32;
                    assert!(x >= rect[0] && x <= rect[2], "{center:?}: x {x} outside {rect:?}");
                    assert!(y >= rect[1] && y <= rect[3], "{center:?}: y {y} outside {rect:?}");
                }
            }
        }
    }

    /// A sphere around the eye has no rectangle: the caller blurs the whole image.
    #[test]
    fn a_sphere_across_the_eye_has_no_rectangle() {
        assert!(sphere_rect([0.0, 0.0, 5.0], 1.0, camera(), [640, 480]).is_none());
        assert!(sphere_rect([0.0, 0.0, 9.0], 1.0, camera(), [640, 480]).is_none());
    }
}
