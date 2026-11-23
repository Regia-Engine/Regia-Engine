// SPDX-License-Identifier: MPL-2.0
//! GPU side of the probe volumes ([`crate::ddgi`]): buffers, pipelines and the compute
//! passes (max-height pyramid, trace, relocate, blend). Everything is created on first use
//! and kept: bind groups are cached against resources that never change size, so a steady
//! frame creates nothing (ADR-035).

use crate::bind_cache::BindCache;
use crate::ddgi::{
    DdgiSettings, DdgiState, HeightfieldGpu, HeightfieldInfo, MAX_INSTANCES, RAYS_PER_PROBE, SdfInstanceGpu,
};
use crate::frame_work::counted::*;

const COMMON: &str = include_str!("../shaders/ddgi_common.wgsl");
const SAMPLE: &str = include_str!("../shaders/ddgi_sample.wgsl");
const TRACE: &str = include_str!("../shaders/ddgi_trace.wgsl");
const BLEND: &str = include_str!("../shaders/ddgi_blend.wgsl");
const MAXMIP: &str = include_str!("../shaders/ddgi_maxmip.wgsl");
const SCREEN: &str = include_str!("../shaders/ddgi_screen.wgsl");

/// The screen lookup shader's full source.
#[must_use]
pub(crate) fn screen_source() -> String {
    format!("{COMMON}\n{SAMPLE}\n{SCREEN}")
}

/// `ScreenParams` in `ddgi_screen.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct ScreenParamsGpu {
    pub(crate) inv_view_proj: [[f32; 4]; 4],
    pub(crate) eye: [f32; 4],
    pub(crate) size: [u32; 4],
    /// xyz: the local origin in the baked volume's space (zero until a floating-origin rebase, spec
    /// 78 § 12.2). The baked screen pass adds it to each lookup point; the DDGI pass ignores it.
    pub(crate) origin: [f32; 4],
}

/// WGSL a host shader appends after declaring `ddgi` (uniform `DdgiUniform`) and
/// `ddgi_data` (storage `array<u32>`): the layouts and the lookup
/// `ddgi_irradiance(p, n, v) -> vec4<f32>` (rgb = irradiance / π, a = coverage), and the probe
/// specular reads of the screen lookup (`ddgi_screen_gi`, `probe_specular.wgsl`, ADR-1103).
#[must_use]
pub fn ddgi_shading_wgsl() -> String {
    format!(
        "{COMMON}\n{SAMPLE}\n{}",
        crate::probe_specular::PROBE_SPECULAR_WGSL
    )
}

/// The trace shader's full source (for the shader checks).
#[must_use]
pub(crate) fn trace_source() -> String {
    format!("{COMMON}\n{SAMPLE}\n{TRACE}")
}

/// The blend shader's full source.
#[must_use]
pub(crate) fn blend_source() -> String {
    format!("{COMMON}\n{BLEND}")
}

/// The max-pyramid shader's source.
#[must_use]
pub(crate) fn maxmip_source() -> &'static str {
    MAXMIP
}

struct Pipelines {
    trace: wgpu::ComputePipeline,
    relocate: wgpu::ComputePipeline,
    blend_irr: wgpu::ComputePipeline,
    blend_vis: wgpu::ComputePipeline,
    max0: wgpu::ComputePipeline,
    maxk: wgpu::ComputePipeline,
    screen: wgpu::ComputePipeline,
    screen_bgl: wgpu::BindGroupLayout,
    classify: wgpu::ComputePipeline,
    /// The young-probe budget's passes (ADR-7870): a histogram of the young probes' keys and the digit pick.
    young_histogram: wgpu::ComputePipeline,
    young_select: wgpu::ComputePipeline,
    args_bgl: wgpu::BindGroupLayout,
    core_bgl: wgpu::BindGroupLayout,
    hf_bgl: wgpu::BindGroupLayout,
    sdf_bgl: wgpu::BindGroupLayout,
    sky_bgl: wgpu::BindGroupLayout,
    max_bgl: wgpu::BindGroupLayout,
}

struct MaxPyramid {
    samples: u32,
    _texture: wgpu::Texture,
    full: wgpu::TextureView,
    levels: Vec<wgpu::TextureView>,
    /// Texel rectangles per level, one [`REGION_SLOT`]-byte `Region` each (ADR-7659): the first
    /// `levels` slots hold the whole level (written once), the next `levels` this step's edits.
    region: wgpu::Buffer,
}

/// Bytes of one level's rectangle list in the max-pyramid region buffer: wgpu's largest
/// uniform offset alignment, and exactly `HEIGHT_EDIT_RECTS` `vec4<u32>`.
const REGION_SLOT: u64 = 256;

/// `Region` in `ddgi_maxmip.wgsl`: one level's texel rectangles (ADR-7659).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct PyramidRegionGpu {
    rects: [[u32; 4]; crate::ddgi::HEIGHT_EDIT_RECTS],
}

/// A regional max-pyramid rebuild staged by `DdgiGpu::upload` for `DdgiGpu::record`.
struct PyramidPlan {
    /// `(heights key, generation)` the plan builds.
    target: (u64, u64),
    /// The generation the pyramid must hold for the plan to apply.
    from: u64,
    samples: u32,
    /// Texel rectangles per level (`pyramid_regions`).
    levels: Vec<Vec<[u32; 4]>>,
}

/// Probe-volume GPU resources.
pub(crate) struct DdgiGpu {
    settings: DdgiSettings,
    pub(crate) uniform: wgpu::Buffer,
    pub(crate) data: wgpu::Buffer,
    _rays: wgpu::Buffer,
    /// Probes due this frame (written by `classify`).
    _queue: wgpu::Buffer,
    /// Indirect dispatch arguments of the queue (x = due probes).
    args: wgpu::Buffer,
    args_bg: wgpu::BindGroup,
    /// The young-probe budget's parameters, state and histogram (`ddgi_trace.wgsl` `DdgiYoung`, ADR-7870).
    young: wgpu::Buffer,
    young_bytes: Vec<u8>,
    sdf_values: wgpu::Buffer,
    instances: wgpu::Buffer,
    lists: wgpu::Buffer,
    /// The emitter list (`DdgiSettings::emitters`).
    emitters: wgpu::Buffer,
    hf_uniform: wgpu::Buffer,
    pipes: Pipelines,
    core_bg: wgpu::BindGroup,
    sdf_bg: wgpu::BindGroup,
    hf_bgs: BindCache<u64, wgpu::BindGroup>,
    sky_bgs: BindCache<u64, wgpu::BindGroup>,
    max_bgs: BindCache<(u64, u32), wgpu::BindGroup>,
    pyramid: Option<MaxPyramid>,
    /// The regional rebuild `upload` staged for this frame's heights (ADR-7659).
    pyramid_plan: Option<PyramidPlan>,
    /// The max pyramid's rebuild counters.
    pyramid_stats: crate::DdgiPyramidStats,
    dummy_max: wgpu::TextureView,
    dummy_heights: wgpu::TextureView,
    dummy_albedo: wgpu::TextureView,
    /// A zero hole count: bound when the heightfield has no holes.
    dummy_holes: wgpu::Buffer,
    albedo_sampler: wgpu::Sampler,
    sky_sampler: wgpu::Sampler,
    built: Option<(u64, u64)>,
    uniform_bytes: Vec<u8>,
    instance_bytes: Vec<u8>,
    list_bytes: Vec<u8>,
    emitter_bytes: Vec<u8>,
    hf_bytes: Vec<u8>,
    screen_params: wgpu::Buffer,
    screen_bytes: Vec<u8>,
    screen_bgs: BindCache<wgpu::TextureView, wgpu::BindGroup>,
    /// The depth view and size of the last recorded screen lookup (`DdgiRuntime::rerun_screen`).
    #[cfg(feature = "test-hooks")]
    last_screen: Option<(wgpu::TextureView, (u32, u32))>,
    /// Zero the probe data before the next update (a camera cut).
    clear_data: bool,
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

fn uniform(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn texture(binding: u32, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// `COPY_SRC` on the two uniform blocks when the test hooks read them back
/// (`DdgiRuntime::read_back_uniforms`); nothing otherwise.
fn debug_read_usage() -> wgpu::BufferUsages {
    if cfg!(feature = "test-hooks") {
        wgpu::BufferUsages::COPY_SRC
    } else {
        wgpu::BufferUsages::empty()
    }
}

fn buffer(d: &wgpu::Device, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    d.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16),
        usage,
        mapped_at_creation: false,
    })
}

fn pipeline(
    d: &wgpu::Device,
    label: &str,
    module: &wgpu::ShaderModule,
    entry: &str,
    layouts: &[&wgpu::BindGroupLayout],
) -> wgpu::ComputePipeline {
    let layouts: Vec<Option<&wgpu::BindGroupLayout>> = layouts.iter().map(|l| Some(*l)).collect();
    let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &layouts,
        immediate_size: 0,
    });
    d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        module,
        entry_point: Some(entry),
        compilation_options: Default::default(),
        cache: crate::pipeline_cache::cache_for(&d).as_deref(),
    })
}

fn r32_texture(d: &wgpu::Device, label: &str, size: u32, mips: u32, storage: bool) -> wgpu::Texture {
    d.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.max(1),
            height: size.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: mips.max(1),
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: if storage {
            // Unit tests read the max pyramid back (ADR-7659).
            let read = if cfg!(test) {
                wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::empty()
            };
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING | read
        } else {
            wgpu::TextureUsages::TEXTURE_BINDING
        },
        view_formats: &[],
    })
}

impl DdgiGpu {
    pub(crate) fn new(d: &wgpu::Device, settings: &DdgiSettings) -> Self {
        let (_, _, _, _, _, words) = DdgiState::data_words(settings);
        let probes = settings.probe_count();
        let su = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let uniform_buf = buffer(
            d,
            "ddgi uniform",
            size_of::<crate::ddgi::DdgiUniformGpu>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST | debug_read_usage(),
        );
        // Zero-filled at creation: every probe record starts with a tag that matches no
        // cell, so the first trace resets it.
        let data = buffer(d, "ddgi data", words * 4, su | wgpu::BufferUsages::COPY_SRC);
        let rays = buffer(
            d,
            "ddgi rays",
            probes * u64::from(RAYS_PER_PROBE) * 16,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );
        let queue = buffer(d, "ddgi queue", probes.max(1) * 4, wgpu::BufferUsages::STORAGE);
        let args = buffer(
            d,
            "ddgi dispatch args",
            16,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
        );
        // Zero-filled at creation: the budget is off until `upload_young` writes one.
        let young = buffer(d, "ddgi young budget", crate::ddgi::YOUNG_WORDS * 4, su);
        let sdf_values = buffer(d, "ddgi sdf values", settings.sdf_pool_values() * 4, su);
        let instances = buffer(
            d,
            "ddgi instances",
            (MAX_INSTANCES * size_of::<SdfInstanceGpu>()) as u64,
            su,
        );
        let lists = buffer(
            d,
            "ddgi lists",
            (crate::ddgi::MAX_CASCADES * settings.max_instances_per_cascade as usize * 4) as u64,
            su,
        );
        let emitters = buffer(
            d,
            "ddgi emitters",
            (crate::ddgi::MAX_EMITTERS * size_of::<crate::ddgi::EmitterGpu>()) as u64,
            su,
        );
        let hf_uniform = buffer(
            d,
            "ddgi heightfield",
            size_of::<HeightfieldGpu>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let core_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ddgi core bgl"),
            entries: &[
                uniform(0),
                storage(1, false),
                storage(2, false),
                storage(3, false),
                storage(4, true),
            ],
        });
        let args_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ddgi args bgl"),
            // 5: the dispatch arguments; 7: the young-probe budget (`DdgiYoung`).
            entries: &[storage(5, false), storage(7, false)],
        });
        let hf_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ddgi heightfield bgl"),
            entries: &[
                uniform(0),
                texture(1, false),
                texture(2, false),
                texture(3, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // Holes through the heightfield (`SharedPassDraw::gi_holes`); binding 5 of
                // group 1 is the classify pass's dispatch arguments.
                storage(6, true),
            ],
        });
        let sdf_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ddgi sdf bgl"),
            entries: &[storage(0, true), storage(1, true), storage(2, true)],
        });
        let sky_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ddgi sky bgl"),
            entries: &[
                texture(0, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let max_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ddgi max pyramid bgl"),
            entries: &[
                uniform(0),
                texture(1, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::R32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                // The level's texel rectangles (ADR-7659).
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(REGION_SLOT),
                    },
                    count: None,
                },
            ],
        });
        let trace_mod = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ddgi trace"),
            source: wgpu::ShaderSource::Wgsl(trace_source().into()),
        });
        let blend_mod = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ddgi blend"),
            source: wgpu::ShaderSource::Wgsl(blend_source().into()),
        });
        let screen_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ddgi screen bgl"),
            entries: &[
                uniform(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let screen_mod = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ddgi screen"),
            source: wgpu::ShaderSource::Wgsl(screen_source().into()),
        });
        let max_mod = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ddgi max pyramid"),
            source: wgpu::ShaderSource::Wgsl(MAXMIP.into()),
        });
        // The ten compute pipelines compile side by side (`pipeline_warm::join_all`).
        type Job<'a> = Box<dyn FnOnce() -> wgpu::ComputePipeline + Send + 'a>;
        let jobs: Vec<Job<'_>> = vec![
            Box::new(|| {
                pipeline(
                    d,
                    "ddgi trace",
                    &trace_mod,
                    "trace",
                    &[&core_bgl, &hf_bgl, &sdf_bgl, &sky_bgl],
                )
            }),
            Box::new(|| pipeline(d, "ddgi relocate", &trace_mod, "relocate", &[&core_bgl, &hf_bgl])),
            Box::new(|| {
                pipeline(
                    d,
                    "ddgi classify",
                    &trace_mod,
                    "classify",
                    &[&core_bgl, &args_bgl],
                )
            }),
            Box::new(|| {
                pipeline(
                    d,
                    "ddgi blend irradiance",
                    &blend_mod,
                    "blend_irradiance",
                    &[&core_bgl],
                )
            }),
            Box::new(|| {
                pipeline(
                    d,
                    "ddgi blend visibility",
                    &blend_mod,
                    "blend_visibility",
                    &[&core_bgl],
                )
            }),
            Box::new(|| {
                pipeline(
                    d,
                    "ddgi young histogram",
                    &trace_mod,
                    "young_histogram",
                    &[&core_bgl, &args_bgl],
                )
            }),
            Box::new(|| {
                pipeline(
                    d,
                    "ddgi young select",
                    &trace_mod,
                    "young_select",
                    &[&core_bgl, &args_bgl],
                )
            }),
            Box::new(|| pipeline(d, "ddgi max level 0", &max_mod, "build_level0", &[&max_bgl])),
            Box::new(|| pipeline(d, "ddgi max level", &max_mod, "build_level", &[&max_bgl])),
            Box::new(|| {
                pipeline(
                    d,
                    "ddgi screen lookup",
                    &screen_mod,
                    "screen_lookup",
                    &[&core_bgl, &screen_bgl],
                )
            }),
        ];
        let [
            trace,
            relocate,
            classify,
            blend_irr,
            blend_vis,
            young_histogram,
            young_select,
            max0,
            maxk,
            screen,
        ] = <[wgpu::ComputePipeline; 10]>::try_from(crate::pipeline_warm::join_all(jobs))
            .unwrap_or_else(|_| unreachable!("join_all returns one result per job"));
        let pipes = Pipelines {
            trace,
            relocate,
            classify,
            young_histogram,
            young_select,
            blend_irr,
            blend_vis,
            max0,
            maxk,
            screen,
            screen_bgl,
            args_bgl,
            core_bgl,
            hf_bgl,
            sdf_bgl,
            sky_bgl,
            max_bgl,
        };
        let core_bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ddgi core"),
            layout: &pipes.core_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: data.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: rays.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: queue.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: emitters.as_entire_binding(),
                },
            ],
        });
        let args_bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ddgi args"),
            layout: &pipes.args_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: args.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: young.as_entire_binding(),
                },
            ],
        });
        let sdf_bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ddgi sdf"),
            layout: &pipes.sdf_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: sdf_values.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: instances.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: lists.as_entire_binding(),
                },
            ],
        });
        let dummy_max = r32_texture(d, "ddgi dummy max", 1, 1, false).create_view(&Default::default());
        let dummy_heights =
            r32_texture(d, "ddgi dummy heights", 1, 1, false).create_view(&Default::default());
        let dummy_albedo = d
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("ddgi dummy albedo"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        // Zero-filled at creation: a hole count of 0.
        let dummy_holes = buffer(d, "ddgi no holes", 16, wgpu::BufferUsages::STORAGE);
        let albedo_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ddgi albedo"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let sky_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ddgi sky"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        Self {
            settings: settings.clone(),
            uniform: uniform_buf,
            data,
            _rays: rays,
            _queue: queue,
            args,
            args_bg,
            young,
            young_bytes: Vec::new(),
            sdf_values,
            instances,
            lists,
            emitters,
            hf_uniform,
            pipes,
            core_bg,
            sdf_bg,
            hf_bgs: BindCache::default(),
            sky_bgs: BindCache::default(),
            max_bgs: BindCache::default(),
            pyramid: None,
            pyramid_plan: None,
            pyramid_stats: crate::DdgiPyramidStats::default(),
            dummy_max,
            dummy_heights,
            dummy_albedo,
            dummy_holes,
            albedo_sampler,
            sky_sampler,
            built: None,
            uniform_bytes: Vec::new(),
            instance_bytes: Vec::new(),
            list_bytes: Vec::new(),
            emitter_bytes: Vec::new(),
            hf_bytes: Vec::new(),
            screen_params: buffer(
                d,
                "ddgi screen params",
                size_of::<ScreenParamsGpu>() as u64,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST | debug_read_usage(),
            ),
            screen_bytes: Vec::new(),
            screen_bgs: BindCache::default(),
            #[cfg(feature = "test-hooks")]
            last_screen: None,
            clear_data: false,
        }
    }

    /// Whether these resources were made for `settings`.
    pub(crate) fn matches(&self, settings: &DdgiSettings) -> bool {
        self.settings.dims == settings.dims
            && self.settings.cascades == settings.cascades
            && self.settings.max_instances_per_cascade == settings.max_instances_per_cascade
            && self.settings.sdf_pool_values() == settings.sdf_pool_values()
            && self.settings.screen_region() == settings.screen_region()
    }

    /// Queues this frame's uniform, instances, lists and new bricks, and the max pyramid's
    /// regional rebuild when `hf`'s edits lead from the generation it holds (`hf_key` is the
    /// heights identity `record` gets).
    pub(crate) fn upload(
        &mut self,
        belt: &mut crate::staging::StagingBelt,
        d: &wgpu::Device,
        state: &mut DdgiState,
        hf: Option<&HeightfieldInfo>,
        hf_key: u64,
    ) {
        self.stage_pyramid_plan(belt, d, hf, hf_key);
        crate::staging::defer_if_changed(
            belt,
            d,
            &mut self.uniform_bytes,
            &self.uniform,
            bytemuck::bytes_of(&state.uniform),
        );
        if !state.instances.is_empty() {
            crate::staging::defer_if_changed(
                belt,
                d,
                &mut self.instance_bytes,
                &self.instances,
                bytemuck::cast_slice(&state.instances),
            );
        }
        if !state.lists.is_empty() {
            crate::staging::defer_if_changed(
                belt,
                d,
                &mut self.list_bytes,
                &self.lists,
                bytemuck::cast_slice(&state.lists),
            );
        }
        if !state.emitters.is_empty() {
            crate::staging::defer_if_changed(
                belt,
                d,
                &mut self.emitter_bytes,
                &self.emitters,
                bytemuck::cast_slice(&state.emitters),
            );
        }
        for (offset, values) in state.uploads.drain(..) {
            belt.defer(d, &self.sdf_values, offset * 4, bytemuck::cast_slice(&values));
        }
        let gpu = match hf {
            Some(h) => HeightfieldGpu {
                map: [h.half_width, h.world_width, h.height_scale, h.height_offset],
                size: [
                    h.samples,
                    max_levels(pyramid_size(h.samples)),
                    1,
                    u32::from(h.albedo_generation != 0),
                ],
                // w: the cone footprint (cells per metre of ray), negative for the mean-height
                // coarse surface instead of the max box.
                albedo: [
                    h.albedo[0],
                    h.albedo[1],
                    h.albedo[2],
                    if self.settings.hf_mean_surface {
                        -self.settings.hf_cone
                    } else {
                        self.settings.hf_cone
                    },
                ],
                shadow: [
                    if self.settings.hf_shadow_cone > 0.0 {
                        -self.settings.hf_shadow_cone
                    } else if self.settings.hf_mean_surface {
                        -self.settings.hf_cone
                    } else {
                        self.settings.hf_cone
                    },
                    h.origin_offset[0],
                    h.origin_offset[1],
                    h.origin_offset[2],
                ],
            },
            None => HeightfieldGpu::default(),
        };
        crate::staging::defer_if_changed(
            belt,
            d,
            &mut self.hf_bytes,
            &self.hf_uniform,
            bytemuck::bytes_of(&gpu),
        );
    }

    /// Stages the max pyramid's regional rebuild for `hf` (ADR-7659): only when its edits lead
    /// from the generation the pyramid holds for `hf_key`. Anything else (no edits, a missed
    /// step, a new map or key) leaves no plan, and `record` rebuilds the whole pyramid.
    fn stage_pyramid_plan(
        &mut self,
        belt: &mut crate::staging::StagingBelt,
        d: &wgpu::Device,
        hf: Option<&HeightfieldInfo>,
        hf_key: u64,
    ) {
        self.pyramid_plan = None;
        let (Some(h), Some(pyramid)) = (hf, self.pyramid.as_ref()) else {
            return;
        };
        let edits = &h.edits;
        if edits.count == 0
            || h.generation == edits.from_generation
            || pyramid.samples != h.samples
            || self.built != Some((hf_key, edits.from_generation))
        {
            return;
        }
        let levels = pyramid_regions(h.samples, edits.rects());
        if levels.len() != pyramid.levels.len()
            || levels.iter().any(|l| l.len() > crate::ddgi::HEIGHT_EDIT_RECTS)
        {
            return;
        }
        let slots: Vec<PyramidRegionGpu> = levels
            .iter()
            .map(|rects| {
                let mut slot = PyramidRegionGpu::default();
                slot.rects[..rects.len()].copy_from_slice(rects);
                slot
            })
            .collect();
        belt.defer(
            d,
            &pyramid.region,
            levels.len() as u64 * REGION_SLOT,
            bytemuck::cast_slice(&slots),
        );
        self.pyramid_plan = Some(PyramidPlan {
            target: (hf_key, h.generation),
            from: edits.from_generation,
            samples: h.samples,
            levels,
        });
    }

    /// Queues the young-probe budget's parameters (`ddgi_trace.wgsl` `DdgiYoung`, ADR-7870). With the budget
    /// off they are all zero, so nothing is queued after the first frame.
    pub(crate) fn upload_young(
        &mut self,
        belt: &mut crate::staging::StagingBelt,
        d: &wgpu::Device,
        params: &crate::ddgi::YoungParamsGpu,
    ) {
        crate::staging::defer_if_changed(
            belt,
            d,
            &mut self.young_bytes,
            &self.young,
            bytemuck::bytes_of(params),
        );
    }

    /// Queues the screen lookup's parameters.
    pub(crate) fn upload_screen(
        &mut self,
        belt: &mut crate::staging::StagingBelt,
        d: &wgpu::Device,
        params: &ScreenParamsGpu,
    ) {
        crate::staging::defer_if_changed(
            belt,
            d,
            &mut self.screen_bytes,
            &self.screen_params,
            bytemuck::bytes_of(params),
        );
    }

    /// Records the half-resolution screen lookup over `depth` (`half` texels).
    pub(crate) fn record_screen(
        &mut self,
        d: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
        half: (u32, u32),
    ) {
        let _span = tracing::info_span!("ddgi screen").entered();
        #[cfg(feature = "test-hooks")]
        if self
            .last_screen
            .as_ref()
            .is_none_or(|(view, size)| view != depth || *size != half)
        {
            self.last_screen = Some((depth.clone(), half));
        }
        let bg = self.screen_bgs.get_or_insert_with(depth.clone(), || {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ddgi screen"),
                layout: &self.pipes.screen_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.screen_params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(depth),
                    },
                ],
            })
        });
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("ddgi screen lookup"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&self.pipes.screen);
        pass.counted_set_bind_group(0, &self.core_bg, &[]);
        pass.counted_set_bind_group(1, &bg, &[]);
        pass.counted_dispatch(half.0.div_ceil(8), half.1.div_ceil(8), 1);
    }

    /// Records the max pyramid's rebuild when the heights changed: only the edited texels when
    /// the shared draw says which samples changed since the generation it holds (ADR-7659).
    fn record_pyramid(
        &mut self,
        d: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        info: &HeightfieldInfo,
        key: u64,
    ) {
        // A power-of-two square so every mip halves exactly (cells past the map repeat its
        // edge; rays are clipped to the map anyway).
        let cells = pyramid_size(info.samples);
        if self.pyramid.as_ref().is_none_or(|p| p.samples != info.samples) {
            self.pyramid = Some(new_pyramid(d, info.samples));
            self.max_bgs.clear();
            self.hf_bgs.clear();
            self.built = None;
        }
        if self.built != Some((key, info.generation))
            && let Some(pyramid) = self.pyramid.as_ref()
        {
            let built = self.built;
            let plan = self.pyramid_plan.take().filter(|p| {
                p.target == (key, info.generation)
                    && built == Some((key, p.from))
                    && p.samples == info.samples
                    && p.levels.len() == pyramid.levels.len()
            });
            self.built = Some((key, info.generation));
            let levels = pyramid.levels.len();
            let mut texels = 0u64;
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ddgi max pyramid"),
                timestamp_writes: None,
            });
            for (level, dst) in pyramid.levels.iter().enumerate() {
                let size = (cells >> level).max(1);
                let whole = [[0, 0, size, size]];
                let (rects, slot): (&[[u32; 4]], usize) = match plan.as_ref() {
                    Some(p) => (&p.levels[level], levels + level),
                    None => (&whole, level),
                };
                if rects.is_empty() {
                    continue;
                }
                let src = if level == 0 {
                    view
                } else {
                    &pyramid.levels[level - 1]
                };
                let bg = self.max_bgs.get_or_insert_with(max_level_key(key, level), || {
                    d.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("ddgi max level"),
                        layout: &self.pipes.max_bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: self.hf_uniform.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(src),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::TextureView(dst),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: &pyramid.region,
                                    offset: 0,
                                    size: wgpu::BufferSize::new(REGION_SLOT),
                                }),
                            },
                        ],
                    })
                });
                let (w, h) = rects.iter().fold((0, 0), |(w, h), r| {
                    texels += u64::from(r[2] - r[0]) * u64::from(r[3] - r[1]);
                    (w.max(r[2] - r[0]), h.max(r[3] - r[1]))
                });
                pass.counted_set_pipeline(if level == 0 {
                    &self.pipes.max0
                } else {
                    &self.pipes.maxk
                });
                pass.counted_set_bind_group(0, &bg, &[(slot as u64 * REGION_SLOT) as u32]);
                pass.counted_dispatch(w.div_ceil(8), h.div_ceil(8), rects.len() as u32);
            }
            let stats = &mut self.pyramid_stats;
            stats.full_texels = pyramid_texels(info.samples);
            if plan.is_some() {
                stats.regional_rebuilds += 1;
            } else {
                stats.full_rebuilds += 1;
            }
            stats.texels_reduced += texels;
            stats.last_texels = texels;
        }
    }

    /// Records the probe update: the max pyramid when the heights changed, then trace,
    /// relocate and blend. `heights` is the shared draw's height texture (level 0 read)
    /// with its identity `hf_key`; `holes` its hole words and their identity
    /// (`SharedPassDraw::gi_holes`); `sky` the sky map misses read and its identity.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        d: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        heights: Option<(&wgpu::TextureView, &HeightfieldInfo, u64)>,
        holes: Option<(&wgpu::Buffer, u64)>,
        albedo_map: Option<&wgpu::TextureView>,
        sky: (&wgpu::TextureView, u64),
        timestamps: Option<(&wgpu::QuerySet, u32)>,
    ) {
        let _span = tracing::info_span!("ddgi record").entered();
        if self.clear_data {
            self.clear_data = false;
            encoder.clear_buffer(&self.data, 0, None);
        }
        if let Some((view, info, key)) = heights {
            self.record_pyramid(d, encoder, view, info, key);
        }
        let holes = holes.filter(|_| heights.is_some());
        let hf_key = heights.map_or(0, |(_, info, k)| {
            k.wrapping_add(1) ^ info.albedo_generation.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        }) ^ holes.map_or(0, |(_, g)| {
            g.wrapping_add(1)
                .wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
                .rotate_left(17)
        });
        let holes_buffer = holes.map_or(&self.dummy_holes, |(b, _)| b);
        let albedo_view = albedo_map.unwrap_or(&self.dummy_albedo);
        let hf_bg = {
            let (max_view, height_view) = match (heights, self.pyramid.as_ref()) {
                (Some((view, _, _)), Some(p)) => (&p.full, view),
                _ => (&self.dummy_max, &self.dummy_heights),
            };
            self.hf_bgs.get_or_insert_with(hf_key, || {
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ddgi heightfield"),
                    layout: &self.pipes.hf_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.hf_uniform.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(max_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(height_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(albedo_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::Sampler(&self.albedo_sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 6,
                            resource: holes_buffer.as_entire_binding(),
                        },
                    ],
                })
            })
        };
        let sky_bg = self.sky_bgs.get_or_insert_with(sky.1, || {
            d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ddgi sky"),
                layout: &self.pipes.sky_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(sky.0),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sky_sampler),
                    },
                ],
            })
        });
        let probes = self.settings.probe_count() as u32;
        // `classify` counts the due probes into the indirect arguments from zero.
        encoder.clear_buffer(&self.args, 0, None);
        // The young-probe budget's passes start from a zero state and histogram each frame.
        let young_budget = self.settings.young_budget > 0;
        if young_budget {
            encoder.clear_buffer(&self.young, crate::ddgi::YOUNG_PARAM_WORDS * 4, None);
        }
        // One compute pass per stage when stage timestamps are requested (`timestamps`:
        // 8 queries from `base`: trace, relocate, irradiance blend, visibility blend), else
        // one pass.
        {
            let mut pass = stage_pass(encoder, timestamps, 0);
            pass.counted_set_bind_group(0, &self.core_bg, &[]);
            pass.counted_set_bind_group(1, &self.args_bg, &[]);
            if young_budget {
                // The young-probe budget (ADR-7870): four radix digits of the B-th least young key, each a
                // histogram over the probes and a one-thread pick; then `classify` queues the young probes at or
                // under it.
                for _ in 0..4 {
                    pass.counted_set_pipeline(&self.pipes.young_histogram);
                    pass.counted_dispatch(probes.div_ceil(64), 1, 1);
                    pass.counted_set_pipeline(&self.pipes.young_select);
                    pass.counted_dispatch(1, 1, 1);
                }
            }
            pass.counted_set_pipeline(&self.pipes.classify);
            pass.counted_dispatch(probes.div_ceil(64), 1, 1);
            pass.counted_set_pipeline(&self.pipes.trace);
            pass.counted_set_bind_group(1, &hf_bg, &[]);
            pass.counted_set_bind_group(2, &self.sdf_bg, &[]);
            pass.counted_set_bind_group(3, &sky_bg, &[]);
            pass.counted_dispatch_indirect(&self.args, 0);
            if timestamps.is_some() {
                drop(pass);
                pass = stage_pass(encoder, timestamps, 1);
            }
            pass.counted_set_pipeline(&self.pipes.relocate);
            pass.counted_set_bind_group(0, &self.core_bg, &[]);
            pass.counted_set_bind_group(1, &hf_bg, &[]);
            pass.counted_dispatch(probes.div_ceil(64), 1, 1);
            if timestamps.is_some() {
                drop(pass);
                pass = stage_pass(encoder, timestamps, 2);
            }
            pass.counted_set_pipeline(&self.pipes.blend_irr);
            pass.counted_set_bind_group(0, &self.core_bg, &[]);
            pass.counted_dispatch_indirect(&self.args, 0);
            if timestamps.is_some() {
                drop(pass);
                pass = stage_pass(encoder, timestamps, 3);
                pass.counted_set_bind_group(0, &self.core_bg, &[]);
            }
            pass.counted_set_pipeline(&self.pipes.blend_vis);
            pass.counted_dispatch_indirect(&self.args, 0);
        }
    }
}

/// A compute pass of the probe update, with stage `k`'s timestamps when requested.
fn stage_pass<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    timestamps: Option<(&wgpu::QuerySet, u32)>,
    k: u32,
) -> wgpu::ComputePass<'e> {
    encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some("ddgi update"),
        timestamp_writes: timestamps.map(|(set, base)| wgpu::ComputePassTimestampWrites {
            query_set: set,
            beginning_of_pass_write_index: Some(base + 2 * k),
            end_of_pass_write_index: Some(base + 2 * k + 1),
        }),
    })
}

/// The renderer's probe-volume state: settings, CPU state and GPU resources, created on
/// the first frame that asks for probes.
pub(crate) struct DdgiRuntime {
    /// `Some(false)` turns the probes off for every profile; `None` follows the profile.
    pub(crate) enabled: Option<bool>,
    pub(crate) settings: DdgiSettings,
    /// The quality tier last applied to `settings` (`crate::FrameRenderer::set_dynamic_gi_tier`).
    pub(crate) tier: crate::DynamicGiTier,
    state: Option<DdgiState>,
    gpu: Option<DdgiGpu>,
    /// Uniform and data buffers bound when the probes are off (flags 0).
    dummy: Option<(wgpu::Buffer, wgpu::Buffer)>,
    /// Bumped when the shared draw is replaced (its height view changes).
    pub(crate) hf_epoch: u64,
    /// The probes run this frame.
    pub(crate) active: bool,
    /// The half-resolution screen lookup runs this frame.
    screen_on: bool,
    /// Bake mesh bricks on the calling thread and wait for pending ones (offscreen captures:
    /// a golden or a screenshot must not depend on task-pool timing).
    pub(crate) bake_sync: bool,
    /// Per frame mesh (indexed like the frame's meshes): the range (m) of the area lights the
    /// short-range emissive pass fitted to it this frame (`emissive_lights`), 0 for none. The
    /// probes take only the far part of those meshes' emission.
    pub(crate) emitter_local_ranges: Vec<f32>,
    /// Mesh keys that skip signed-distance bricks (`MeshRenderer::gi_proxy` is false, or
    /// [`crate::FrameRenderer::set_gi_proxy_disabled`]). Replaced every frame.
    gi_proxy_off: Vec<u64>,
    /// The shared draw's holes (`SharedPassDraw::gi_holes`): the hole words and their identity.
    holes: Option<(wgpu::Buffer, u64)>,
    /// The identity of the sky the probes trace (`set_sky_identity`): the renderer's IBL
    /// generation, or `u64::MAX` for the white stand-in.
    sky_identity: Option<u64>,
    /// The traced heightfield's last generation (`set_heights_generation`, ADR-7659).
    heights_generation: Option<u64>,
    /// Where counted meshes' bricks read their geometry (ADR-7721; `None`: no uploader, counted
    /// meshes are not traced).
    counted: Option<crate::asset_upload::CountedGiSource>,
}

impl Default for DdgiRuntime {
    /// Probes off until a frame asks for them, at the [`crate::DynamicGiTier::High`] layout: the
    /// tier a renderer reports until one is set (ADR-5001; `DdgiSettings::default` keeps the
    /// layout High had before, for hosts that build their own settings on it).
    fn default() -> Self {
        Self {
            enabled: None,
            settings: crate::DynamicGiTier::High.settings(),
            tier: crate::DynamicGiTier::High,
            state: None,
            gpu: None,
            dummy: None,
            hf_epoch: 0,
            active: false,
            screen_on: false,
            bake_sync: false,
            emitter_local_ranges: Vec::new(),
            gi_proxy_off: Vec::new(),
            holes: None,
            sky_identity: None,
            heights_generation: None,
            counted: None,
        }
    }
}

/// Copies `src` into a mapped buffer and returns its bytes (debugging and tests: one submit and a
/// blocking poll).
fn read_buffer(device: &crate::RenderDevice, src: &wgpu::Buffer) -> Result<Vec<u8>, crate::RenderError> {
    read_buffer_range(device, src, 0, src.size())
}

/// [`read_buffer`] for `size` bytes from `offset` (both multiples of 4, inside `src`).
fn read_buffer_range(
    device: &crate::RenderDevice,
    src: &wgpu::Buffer,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>, crate::RenderError> {
    let d = &device.device;
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ddgi readback"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ddgi readback"),
    });
    encoder.copy_buffer_to_buffer(src, offset, &staging, 0, size);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(crate::RenderError::Poll)?;
    rx.recv()
        .map_err(|_| crate::RenderError::Readback("map callback dropped".into()))?
        .map_err(|e| crate::RenderError::Readback(e.to_string()))?;
    let bytes = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|e| crate::RenderError::Readback(e.to_string()))?
        .to_vec();
    staging.unmap();
    Ok(bytes)
}

impl DdgiRuntime {
    /// Gives the probes the counted meshes' geometry source (ADR-7721): a counted mesh's brick is
    /// baked from a GPU readback of its upload. `None` leaves counted meshes untraced.
    pub(crate) fn set_counted(&mut self, source: Option<crate::asset_upload::CountedGiSource>) {
        if let Some(state) = self.state.as_mut() {
            state.counted = source.clone();
        }
        self.counted = source;
    }

    /// CPU work and uploads for one frame. Returns whether the probes run.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin(
        &mut self,
        d: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        profile_wants: bool,
        linear_hdr: bool,
        eye: [f32; 3],
        light: &crate::ddgi::DdgiLight,
        geometry: &crate::ddgi::ProbeGeometry<'_, '_>,
        hf: Option<&HeightfieldInfo>,
        screen: Option<ScreenParamsGpu>,
        lookup: crate::ddgi::GiLookupResolution,
    ) -> bool {
        // No probes on Android (`GiLook::depth_available`), whatever the override says.
        self.active = self.enabled.unwrap_or(profile_wants) && linear_hdr && crate::GiLook::depth_available();
        if !self.active {
            return false;
        }
        // `matches` includes the brick-pool value count, so a new `sdf_pool_bytes`
        // drops the CPU state with the GPU buffer. The "does not fit" memo dies there.
        if self.gpu.as_ref().is_none_or(|g| !g.matches(&self.settings)) {
            self.gpu = Some(DdgiGpu::new(d, &self.settings));
            let mut state = DdgiState::new(self.settings.clone());
            state.counted = self.counted.clone();
            self.state = Some(state);
        }
        let epoch = self.hf_epoch;
        self.set_heights_generation(hf.map(|h| h.generation));
        let (Some(state), Some(gpu)) = (self.state.as_mut(), self.gpu.as_mut()) else {
            return false;
        };
        state.settings = self.settings.clone();
        state.set_gi_proxy_off(self.gi_proxy_off.iter().copied());
        // Trace tunables (cone, coarse surface) change without a rebuild.
        gpu.settings = self.settings.clone();
        state.prepare_with_sets(
            eye,
            light,
            geometry,
            &self.emitter_local_ranges,
            hf.is_some(),
            self.bake_sync,
        );
        // The full frame size: the region may refuse the wanted lookup (`set_screen` falls back
        // to half resolution, so the lookup pass dispatches over the size it chose).
        state.set_screen(screen.map(|s| (s.size[0], s.size[1])), lookup);
        self.screen_on = state.uniform_screen_on();
        if let Some(params) = screen.filter(|_| self.screen_on) {
            gpu.upload_screen(belt, d, &params);
        }
        gpu.upload(belt, d, state, hf, epoch);
        // The frame's view for the young-probe budget's order (ADR-7870): the inverse of the screen lookup's.
        let view_proj =
            screen.map(|s| regia_core::glam::Mat4::from_cols_array_2d(&s.inv_view_proj).inverse());
        gpu.upload_young(
            belt,
            d,
            &crate::ddgi::YoungParamsGpu::of(&self.settings, eye, view_proj),
        );
        true
    }

    /// Records the screen lookup (after the depth prepass, when [`Self::begin`] enabled it).
    pub(crate) fn record_screen(
        &mut self,
        d: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
    ) {
        if !(self.active && self.screen_on) {
            return;
        }
        let half = self.state.as_ref().map(|s| s.screen_half()).unwrap_or((0, 0));
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.record_screen(d, encoder, depth, half);
        }
    }

    /// Records the probe update (after [`Self::begin`] returned `true`).
    pub(crate) fn record(
        &mut self,
        d: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        heights: Option<(&wgpu::TextureView, &HeightfieldInfo)>,
        albedo_map: Option<&wgpu::TextureView>,
        sky: (&wgpu::TextureView, u64),
        timestamps: Option<(&wgpu::QuerySet, u32)>,
    ) {
        if !self.active {
            return;
        }
        let epoch = self.hf_epoch;
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.record(
                d,
                encoder,
                heights.map(|(v, i)| (v, i, epoch)),
                self.holes.as_ref().map(|(b, g)| (b, *g)),
                albedo_map,
                sky,
                timestamps,
            );
        }
    }

    /// The holes cut through the heightfield the probes trace (`SharedPassDraw::gi_holes`), set
    /// every frame before [`Self::record`]. A changed mask wakes settled probes: the light
    /// through a new hole (or over a filled one) differs.
    pub(crate) fn set_holes(&mut self, holes: Option<(&wgpu::Buffer, u64)>) {
        let before = self.holes.as_ref().map(|(_, g)| *g);
        if before != holes.map(|(_, g)| g)
            && let Some(state) = self.state.as_mut()
        {
            state.unsettle();
        }
        self.holes = holes.map(|(b, g)| (b.clone(), g));
    }

    /// The sky the probes trace on a miss is the IBL with this identity. A new one (a studio
    /// environment installed or replaced, a finished bake) wakes the probes: the settling key
    /// sees the sky's scale, not which image it is, so settled probes kept the old sky
    /// (ADR-2000).
    pub(crate) fn set_sky_identity(&mut self, identity: u64) {
        if self.sky_identity != Some(identity)
            && self.sky_identity.is_some()
            && let Some(state) = self.state.as_mut()
        {
            state.unsettle();
        }
        self.sky_identity = Some(identity);
    }

    /// The traced heightfield's generation this frame (`HeightfieldInfo::generation`; `None`
    /// without one). A change (a runtime height edit, holes, a new map) wakes settled probes, as
    /// a new hole mask or sky does: settled probes skip the update, so a crater under them was
    /// never traced, nor its pyramid rebuilt (ADR-7659). The first generation seen is not a
    /// change.
    pub(crate) fn set_heights_generation(&mut self, generation: Option<u64>) {
        if self.heights_generation.is_some()
            && self.heights_generation != generation
            && let Some(state) = self.state.as_mut()
        {
            state.unsettle();
        }
        self.heights_generation = generation;
    }

    /// A camera cut: the probes restart from zero, as in a fresh renderer (their records,
    /// irradiance and settling count); baked mesh bricks are kept.
    pub(crate) fn camera_cut(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.restart();
        }
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.clear_data = true;
        }
    }

    /// The uniform and data buffers the shading reads, and whether they are the live
    /// probes (`false`: zeroed stand-ins with shading off).
    pub(crate) fn shading_buffers(&mut self, d: &wgpu::Device) -> (wgpu::Buffer, wgpu::Buffer, bool) {
        if self.active
            && let Some(gpu) = self.gpu.as_ref()
        {
            return (gpu.uniform.clone(), gpu.data.clone(), true);
        }
        let (u, data) = self.dummy.get_or_insert_with(|| {
            (
                buffer(
                    d,
                    "ddgi uniform (off)",
                    size_of::<crate::ddgi::DdgiUniformGpu>() as u64,
                    wgpu::BufferUsages::UNIFORM,
                ),
                buffer(d, "ddgi data (off)", 64, wgpu::BufferUsages::STORAGE),
            )
        });
        (u.clone(), data.clone(), false)
    }

    /// Frames of an unchanged traced scene after which the probe update stops
    /// (`DdgiSettings::freeze_frames`, ADR-5001).
    pub(crate) fn freeze_frames(&self) -> u32 {
        self.settings.freeze_frames()
    }

    /// The heightfield max pyramid's rebuild counters (ADR-7659).
    pub(crate) fn pyramid_stats(&self) -> crate::DdgiPyramidStats {
        self.gpu.as_ref().map(|g| g.pyramid_stats).unwrap_or_default()
    }

    /// Frames the traced scene has been unchanged (0 while the probes move or converge).
    pub(crate) fn settled_frames(&self) -> u32 {
        if !self.active {
            return u32::MAX;
        }
        self.state.as_ref().map_or(0, |s| s.settled_frames)
    }

    /// Reads the probe data words and the last rays back (debugging and tests; blocks and
    /// submits its own copy).
    pub(crate) fn read_back(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<crate::DdgiReadback>, crate::RenderError> {
        let (Some(gpu), Some(state)) = (self.gpu.as_ref(), self.state.as_ref()) else {
            return Ok(None);
        };
        let data = read_buffer(device, &gpu.data)?;
        let rays = read_buffer(device, &gpu._rays)?;
        let u = &state.uniform;
        Ok(Some(crate::DdgiReadback {
            data: bytemuck::pod_collect_to_vec(&data),
            rays: bytemuck::pod_collect_to_vec(&rays),
            bases: std::array::from_fn(|c| {
                let b = u.cascade_base(c);
                [b[0], b[1], b[2]]
            }),
            dims: [u.dims()[0], u.dims()[1], u.dims()[2]],
            cascades: u.dims()[3],
            words: [u.words()[0], u.words()[1]],
            instances: state.instances.len() as u32,
            lists: std::array::from_fn(|c| u.inst_count()[c]),
            screen: state.screen_region(),
        }))
    }

    /// Records the last frame's screen lookup again, alone, and submits it: the same pipeline, bind
    /// groups, parameters and depth texture (debugging and tests: one submit and a blocking poll).
    /// `false` when no lookup has run.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn rerun_screen(&mut self, device: &crate::RenderDevice) -> Result<bool, crate::RenderError> {
        if !(self.active && self.screen_on) {
            return Ok(false);
        }
        let Some(gpu) = self.gpu.as_mut() else {
            return Ok(false);
        };
        let Some((depth, half)) = gpu.last_screen.clone() else {
            return Ok(false);
        };
        let d = &device.device;
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ddgi screen lookup rerun"),
        });
        gpu.record_screen(d, &mut encoder, &depth, half);
        device.queue.submit([encoder.finish()]);
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(crate::RenderError::Poll)?;
        Ok(true)
    }

    /// The first `texels` texels of the screen lookup, 8 words each (`ddgi_screen.wgsl`), read back
    /// alone (debugging and tests: one submit and a blocking poll). `None` while the lookup is off.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn read_back_screen_words(
        &self,
        device: &crate::RenderDevice,
        texels: u32,
    ) -> Result<Option<Vec<u32>>, crate::RenderError> {
        let (Some(gpu), Some(state)) = (self.gpu.as_ref(), self.state.as_ref()) else {
            return Ok(None);
        };
        let [first, w, h] = state.screen_region();
        let words = u64::from(texels.min(w.saturating_mul(h))) * 8;
        if words == 0 {
            return Ok(None);
        }
        let bytes = read_buffer_range(device, &gpu.data, u64::from(first) * 4, words * 4)?;
        Ok(Some(bytemuck::pod_collect_to_vec(&bytes)))
    }

    /// The probes' two uniform blocks, each as the bytes the CPU last queued and the bytes the GPU
    /// buffer holds now: `[screen CPU, screen GPU, probes CPU, probes GPU]` (`ScreenParams` of
    /// `ddgi_screen.wgsl`, `DdgiUniform` of `ddgi_common.wgsl`). Debugging and tests: blocks and
    /// submits its own copies.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn read_back_uniforms(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<[Vec<u8>; 4]>, crate::RenderError> {
        let Some(gpu) = self.gpu.as_ref() else {
            return Ok(None);
        };
        Ok(Some([
            gpu.screen_bytes.clone(),
            read_buffer(device, &gpu.screen_params)?,
            gpu.uniform_bytes.clone(),
            read_buffer(device, &gpu.uniform)?,
        ]))
    }

    /// Emitters in the last frame's emitter list.
    pub(crate) fn emitter_count(&self) -> usize {
        self.state.as_ref().map_or(0, |s| s.emitters.len())
    }

    /// Brick bakes still running.
    pub(crate) fn pending_bakes(&self) -> usize {
        self.state.as_ref().map_or(0, DdgiState::pending_bakes)
    }

    /// Replaces the mesh keys that skip GI proxies. Empty traces every candidate.
    pub(crate) fn set_gi_proxy_disabled(&mut self, keys: impl IntoIterator<Item = u64>) {
        self.gi_proxy_off.clear();
        self.gi_proxy_off.extend(keys);
    }

    /// Forgets meshes whose last `Arc` dropped ([`DdgiState::release_meshes`]).
    pub(crate) fn release_meshes(&mut self, keys: &[u64]) {
        if let Some(state) = self.state.as_mut() {
            state.release_meshes(keys);
        }
    }

    /// Whether `mesh_key` had a brick in the pool after the last probe update.
    pub(crate) fn brick_resident(&self, mesh_key: u64) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.brick_resident(mesh_key))
    }

    /// Whether `mesh_key`'s brick was in the last probe update's instance list.
    pub(crate) fn brick_traced(&self, mesh_key: u64) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.brick_traced(mesh_key))
    }
}

/// The max-pyramid bind-group cache key of mip `level` built from the heights `key`: the pair, so
/// no heights key at one mip can stand for another key at another mip (an XOR of the level into
/// the key's top byte could, and the level-0 entry binds the heights view itself).
fn max_level_key(key: u64, level: usize) -> (u64, u32) {
    (key, u32::try_from(level).unwrap_or(u32::MAX))
}

/// The max pyramid of a map of `samples` per side: every level's view, and the region buffer
/// with the whole-level rectangles in its first half (ADR-7659).
fn new_pyramid(d: &wgpu::Device, samples: u32) -> MaxPyramid {
    use wgpu::util::DeviceExt as _;
    let cells = pyramid_size(samples);
    let levels = max_levels(cells);
    let texture = r32_texture(d, "ddgi max pyramid", cells, levels, true);
    let full = texture.create_view(&Default::default());
    let views = (0..levels)
        .map(|l| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: l,
                mip_level_count: Some(1),
                ..Default::default()
            })
        })
        .collect();
    // Whole-level slots, then as many (zeroed) slots for the regional rebuilds.
    let mut slots = vec![PyramidRegionGpu::default(); 2 * levels as usize];
    for (level, slot) in slots.iter_mut().take(levels as usize).enumerate() {
        let size = (cells >> level).max(1);
        slot.rects[0] = [0, 0, size, size];
    }
    let region = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("ddgi max pyramid regions"),
        contents: bytemuck::cast_slice(&slots),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    MaxPyramid {
        samples,
        _texture: texture,
        full,
        levels: views,
        region,
    }
}

/// Texels of the whole max pyramid (every level) of a map of `samples` per side.
#[must_use]
pub(crate) fn pyramid_texels(samples: u32) -> u64 {
    let cells = pyramid_size(samples);
    (0..max_levels(cells))
        .map(|l| u64::from((cells >> l).max(1)).pow(2))
        .sum()
}

/// The texel rectangles `[x0, y0, x1, y1)` of every max-pyramid level that the changed sample
/// rectangles `rects` (`[col_start, row_start, col_end, row_end)`) reach on a map of `samples`
/// per side (ADR-7659). Level 0 texel `i` reads samples `i` and `i + 1` clamped to the last, so a
/// changed sample `c` reaches texels `c - 1` and `c`, and the last sample every texel from the
/// one before it to the padded edge. Level `L + 1` covers `floor(lo / 2) .. ceil(hi / 2)` of
/// level `L`. Rectangles of one level that meet or touch merge into their bounding rectangle, so
/// no texel is written twice.
#[must_use]
pub(crate) fn pyramid_regions(samples: u32, rects: &[[u32; 4]]) -> Vec<Vec<[u32; 4]>> {
    let cells = pyramid_size(samples);
    let levels = max_levels(cells) as usize;
    let last = samples.saturating_sub(1);
    let reach = |lo: u32, hi: u32| {
        let lo = lo.min(last).saturating_sub(1);
        let hi = if hi > last { cells } else { hi.min(cells) };
        (lo.min(cells), hi.max(lo + 1).min(cells))
    };
    let mut level: Vec<[u32; 4]> = rects
        .iter()
        .filter(|r| r[0] < r[2] && r[1] < r[3])
        .map(|r| {
            let (x0, x1) = reach(r[0], r[2]);
            let (y0, y1) = reach(r[1], r[3]);
            [x0, y0, x1, y1]
        })
        .collect();
    let mut out = Vec::with_capacity(levels);
    for _ in 0..levels {
        merge_touching(&mut level);
        out.push(level.clone());
        for r in &mut level {
            *r = [r[0] / 2, r[1] / 2, r[2].div_ceil(2), r[3].div_ceil(2)];
        }
    }
    out
}

/// Merges rectangles that overlap or share an edge into their bounding rectangle, until none do.
fn merge_touching(rects: &mut Vec<[u32; 4]>) {
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let (a, b) = (rects[i], rects[j]);
                if a[0] <= b[2] && b[0] <= a[2] && a[1] <= b[3] && b[1] <= a[3] {
                    rects[i] = [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])];
                    rects.swap_remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
    }
}

/// Side of the max pyramid's level 0 for a map of `samples` per side.
#[must_use]
pub(crate) fn pyramid_size(samples: u32) -> u32 {
    samples.saturating_sub(1).max(1).next_power_of_two()
}

/// Max-pyramid levels for `cells` level-0 cells (down to one cell).
#[must_use]
pub(crate) fn max_levels(cells: u32) -> u32 {
    let mut levels = 1;
    let mut size = cells.max(1);
    while size > 1 {
        size = size.div_ceil(2);
        levels += 1;
    }
    levels
}

/// Contract rows of the screen lookup's uniform (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        // The DDGI screen pass declares the first three members; the baked pass reads `origin` too.
        crate::contract_row!(
            "ddgi_screen.wgsl",
            "ScreenParams",
            ScreenParamsGpu {
                inv_view_proj,
                eye,
                size
            }
        )
        .host_may_extend(),
        crate::contract_row!(
            "baked_gi_screen.wgsl",
            "ScreenParams",
            ScreenParamsGpu {
                inv_view_proj,
                eye,
                size,
                origin
            }
        ),
        crate::contract_row!("ddgi_maxmip.wgsl", "Region", PyramidRegionGpu { rects }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The max-pyramid bind groups were keyed `key ^ level << 56`, so heights key `E` at mip `L` and
    /// key `E ^ (L << 56)` at mip 0 shared an entry, and the level-0 entry binds the heights view.
    #[test]
    fn max_pyramid_keys_never_collide_across_levels() {
        for key in [0u64, 1, 0x1234_5678, u64::MAX, 1 << 56, 0xff << 56] {
            for level in 1..16usize {
                let shifted = key ^ ((level as u64) << 56);
                assert_ne!(
                    max_level_key(key, level),
                    max_level_key(shifted, 0),
                    "key {key:#x} level {level}"
                );
                assert_ne!(max_level_key(key, level), max_level_key(key, 0));
                assert_ne!(
                    max_level_key(key, level),
                    max_level_key(key.wrapping_add(1), level)
                );
            }
        }
    }

    #[test]
    fn pyramid_levels_reach_one_cell() {
        assert_eq!(max_levels(1), 1);
        assert_eq!(max_levels(2), 2);
        assert_eq!(max_levels(4096), 13);
        assert_eq!(max_levels(4095), 13);
        assert_eq!(max_levels(4097), 14);
    }

    #[test]
    fn a_new_sky_wakes_settled_probes_and_the_same_sky_does_not() {
        let mut rt = DdgiRuntime {
            active: true,
            state: Some(crate::ddgi::DdgiState::new(crate::ddgi::DdgiSettings::default())),
            ..DdgiRuntime::default()
        };
        let settle = |rt: &mut DdgiRuntime| {
            if let Some(state) = rt.state.as_mut() {
                state.settled_frames = 42;
            }
        };
        // The first identity seen is not a change.
        settle(&mut rt);
        rt.set_sky_identity(3);
        assert_eq!(rt.settled_frames(), 42);
        rt.set_sky_identity(3);
        assert_eq!(rt.settled_frames(), 42, "the same sky keeps settled probes");
        rt.set_sky_identity(4);
        assert_eq!(rt.settled_frames(), 0, "a replaced IBL wakes the probes");
        settle(&mut rt);
        rt.set_sky_identity(u64::MAX);
        assert_eq!(rt.settled_frames(), 0, "dropping the IBL wakes them too");
    }

    /// ADR-7659: a runtime height edit (a new heights generation) wakes settled probes, as a new
    /// sky does; the first generation seen and an unchanged one do not.
    #[test]
    fn a_heights_change_wakes_settled_probes() {
        let mut rt = DdgiRuntime {
            active: true,
            state: Some(crate::ddgi::DdgiState::new(crate::ddgi::DdgiSettings::default())),
            ..DdgiRuntime::default()
        };
        let settle = |rt: &mut DdgiRuntime| {
            if let Some(state) = rt.state.as_mut() {
                state.settled_frames = 42;
            }
        };
        settle(&mut rt);
        rt.set_heights_generation(Some(5));
        assert_eq!(
            rt.settled_frames(),
            42,
            "the first generation seen is not a change"
        );
        rt.set_heights_generation(Some(5));
        assert_eq!(rt.settled_frames(), 42, "unchanged heights keep settled probes");
        rt.set_heights_generation(Some(6));
        assert_eq!(rt.settled_frames(), 0, "an edit wakes the probes");
        settle(&mut rt);
        rt.set_heights_generation(None);
        assert_eq!(rt.settled_frames(), 0, "losing the heightfield wakes them too");
    }

    #[test]
    fn ddgi_shaders_parse_and_validate() {
        for (name, src) in [
            ("trace", trace_source()),
            ("blend", blend_source()),
            ("maxmip", maxmip_source().to_owned()),
            ("screen", screen_source()),
        ] {
            let module = naga::front::wgsl::parse_str(&src)
                .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(&src)));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        }
    }

    /// The max-pyramid shader's arithmetic on the CPU (`ddgi_maxmip.wgsl`): level 0 texel
    /// `(x, y)` of a map of `samples` per side, heights × `scale` + `offset`.
    fn cpu_level0(samples: u32, heights: &[f32], scale: f32, offset: f32, x: u32, y: u32) -> f32 {
        let last = samples - 1;
        let at = |c: u32, r: u32| heights[(r.min(last) * samples + c.min(last)) as usize];
        let h = at(x, y).max(at(x + 1, y)).max(at(x, y + 1).max(at(x + 1, y + 1)));
        h * scale + offset
    }

    fn cpu_levelk(src: &[f32], src_size: u32, x: u32, y: u32) -> f32 {
        let last = src_size - 1;
        let at = |c: u32, r: u32| src[(r.min(last) * src_size + c.min(last)) as usize];
        let (c, r) = (x * 2, y * 2);
        at(c, r).max(at(c + 1, r)).max(at(c, r + 1).max(at(c + 1, r + 1)))
    }

    /// Re-reduces `rects` of every level (`None`: every texel) of the CPU pyramid `pyr`.
    fn cpu_reduce(samples: u32, heights: &[f32], pyr: &mut [Vec<f32>], rects: Option<&[Vec<[u32; 4]>]>) {
        let cells = pyramid_size(samples);
        for level in 0..pyr.len() {
            let size = (cells >> level).max(1);
            let whole = vec![[0, 0, size, size]];
            let list = rects.map_or(&whole, |r| &r[level]);
            let (lower, upper) = pyr.split_at_mut(level);
            for r in list {
                assert!(r[2] <= size && r[3] <= size, "level {level}: {r:?} past {size}");
                for y in r[1]..r[3] {
                    for x in r[0]..r[2] {
                        upper[0][(y * size + x) as usize] = if level == 0 {
                            cpu_level0(samples, heights, 3.0, -2.0, x, y)
                        } else {
                            cpu_levelk(&lower[level - 1], size * 2, x, y)
                        };
                    }
                }
            }
        }
    }

    fn cpu_pyramid(samples: u32, heights: &[f32]) -> Vec<Vec<f32>> {
        let cells = pyramid_size(samples);
        let mut pyr: Vec<Vec<f32>> = (0..max_levels(cells))
            .map(|l| vec![f32::NAN; ((cells >> l).max(1) as usize).pow(2)])
            .collect();
        cpu_reduce(samples, heights, &mut pyr, None);
        pyr
    }

    /// A deterministic edit sequence: rectangles at the corners and borders, single samples,
    /// overlapping pairs and an interior block, with the heights they write.
    fn edit_steps(samples: u32) -> Vec<Vec<[u32; 4]>> {
        let n = samples;
        let m = n / 2;
        let less = |a: u32, b: u32| a.saturating_sub(b);
        vec![
            vec![[0, 0, 1, 1]],
            vec![[n - 1, n - 1, n, n]],
            vec![[n - 1, 0, n, 1], [0, n - 1, 1, n]],
            vec![[m, m, m + 1, m + 1]],
            vec![[1, 2, m, m + 3], [less(m, 2), m, less(n, 3), n - 1]],
            vec![[0, m, n, m + 1]],
            vec![[less(n, 3), 1, n, 4], [less(n, 2), 2, n, n]],
            vec![[2, less(n, 4), 6, n]],
        ]
    }

    fn apply_edit(samples: u32, heights: &mut [f32], rects: &[[u32; 4]], seed: u32) {
        let mut x = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
        for r in rects {
            for row in r[1]..r[3] {
                for col in r[0]..r[2] {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    heights[(row * samples + col) as usize] =
                        (x >> 8) as f32 / (1u32 << 24) as f32 * 40.0 - 10.0;
                }
            }
        }
    }

    fn start_heights(samples: u32) -> Vec<f32> {
        (0..samples * samples)
            .map(|i| ((i % samples) as f32 * 0.37).sin() * 4.0 + ((i / samples) as f32 * 0.23).cos() * 3.0)
            .collect()
    }

    /// ADR-7659: re-reducing only `pyramid_regions` of each edit step gives, bit for bit, the
    /// pyramid a full rebuild gives, on the shader's CPU mirror, for power-of-two-plus-one and
    /// padded (non-power-of-two) maps, with edits on every border and corner.
    #[test]
    fn regional_max_pyramid_equals_a_full_rebuild_on_the_cpu_mirror() {
        for samples in [2, 3, 5, 17, 33, 37, 50, 65, 100, 129] {
            let mut heights = start_heights(samples);
            let mut pyr = cpu_pyramid(samples, &heights);
            for (step, rects) in edit_steps(samples).iter().enumerate() {
                let rects: Vec<[u32; 4]> = rects
                    .iter()
                    .map(|r| {
                        [
                            r[0].min(samples - 1),
                            r[1].min(samples - 1),
                            r[2].min(samples),
                            r[3].min(samples),
                        ]
                    })
                    .collect();
                apply_edit(samples, &mut heights, &rects, step as u32 + samples);
                let regions = pyramid_regions(samples, &rects);
                assert_eq!(regions.len(), pyr.len());
                cpu_reduce(samples, &heights, &mut pyr, Some(&regions));
                let full = cpu_pyramid(samples, &heights);
                for (level, (a, b)) in pyr.iter().zip(&full).enumerate() {
                    let bits = |v: &Vec<f32>| v.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
                    assert_eq!(bits(a), bits(b), "samples {samples} step {step} level {level}");
                }
            }
        }
    }

    /// The regional rectangles stay a small share of the pyramid: one 128 × 128 crater on a 4097
    /// map re-reduces about 0.13 % of a full rebuild's texels (ADR-7659), and rectangles that
    /// meet merge, so no level lists one texel twice.
    #[test]
    fn one_crater_reduces_a_small_share_and_no_texel_twice() {
        let samples = 4097;
        let regions = pyramid_regions(samples, &[[1000, 2000, 1128, 2128]]);
        let texels: u64 = regions
            .iter()
            .flatten()
            .map(|r| u64::from(r[2] - r[0]) * u64::from(r[3] - r[1]))
            .sum();
        let full = pyramid_texels(samples);
        assert_eq!(full, (0..13).map(|l| (4096u64 >> l).pow(2)).sum::<u64>());
        assert!(texels * 100 < full, "{texels} of {full}");
        assert_eq!(regions[0], vec![[999, 1999, 1128, 2128]]);
        // Two overlapping craters and one far away: the pair merges at level 0, all three by
        // the root.
        let regions = pyramid_regions(
            samples,
            &[[10, 10, 20, 20], [15, 15, 30, 30], [3000, 3000, 3001, 3001]],
        );
        assert_eq!(regions[0].len(), 2);
        assert_eq!(regions.last().map(Vec::len), Some(1));
        for level in &regions {
            for (i, a) in level.iter().enumerate() {
                for b in &level[i + 1..] {
                    let overlap = a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3];
                    assert!(!overlap, "{a:?} and {b:?} overlap");
                }
            }
        }
        // The last sample reaches the padded edge.
        let regions = pyramid_regions(37, &[[36, 0, 37, 1]]);
        assert_eq!(regions[0], vec![[35, 0, 64, 1]]);
    }

    /// `HeightEdits` keeps 16 rectangles and merges the 17th into the last.
    #[test]
    fn height_edits_merge_past_their_capacity() {
        let mut edits = crate::HeightEdits::since(4);
        edits.push([3, 3, 3, 9]);
        assert_eq!(edits.count, 0, "an empty rectangle is ignored");
        for i in 0..17 {
            edits.push([i, i, i + 1, i + 1]);
        }
        assert_eq!(edits.count, 16);
        assert_eq!(edits.rects().len(), 16);
        assert_eq!(edits.rects()[15], [15, 15, 17, 17]);
        assert_eq!(edits.from_generation, 4);
    }

    fn read_pyramid(device: &crate::RenderDevice, gpu: &DdgiGpu) -> Vec<Vec<u32>> {
        let d = &device.device;
        let Some(p) = gpu.pyramid.as_ref() else {
            panic!("no pyramid");
        };
        let cells = pyramid_size(p.samples);
        (0..p.levels.len() as u32)
            .map(|level| {
                let size = (cells >> level).max(1);
                let row = (size * 4).div_ceil(256) * 256;
                let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("pyramid readback"),
                    size: u64::from(row * size),
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let mut encoder = d.create_command_encoder(&Default::default());
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &p._texture,
                        mip_level: level,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(row),
                            rows_per_image: Some(size),
                        },
                    },
                    wgpu::Extent3d {
                        width: size,
                        height: size,
                        depth_or_array_layers: 1,
                    },
                );
                device.queue.submit([encoder.finish()]);
                buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
                device
                    .tracked()
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("poll");
                let bytes = buffer.slice(..).get_mapped_range().expect("mapped").to_vec();
                (0..size)
                    .flat_map(|y| {
                        let start = (y * row) as usize;
                        bytemuck::cast_slice::<u8, u32>(&bytes[start..start + size as usize * 4]).to_vec()
                    })
                    .collect()
            })
            .collect()
    }

    /// ADR-7659 on the GPU (lavapipe): after a sequence of edit steps rebuilt regionally, the
    /// max pyramid equals, bit for bit, a full rebuild of the final heights (forced by a new heights
    /// key), on a padded 37-sample map and a 33-sample one; each step took the regional path, and
    /// a step whose edits lead from a generation the pyramid does not hold rebuilds in full.
    #[test]
    fn regional_max_pyramid_is_bit_identical_to_a_full_rebuild_on_the_gpu() {
        let device = match crate::RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("skipping: no GPU adapter (lavapipe) available: {err}");
                return;
            }
        };
        let d = &device.device;
        let settings = DdgiSettings {
            sdf_pool_bytes: 1 << 20,
            ..DdgiSettings::default()
        };
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        for samples in [37u32, 33] {
            let mut gpu = DdgiGpu::new(d, &settings);
            let mut state = crate::ddgi::DdgiState::new(settings.clone());
            let mut belt = crate::staging::StagingBelt::default();
            let texture = d.create_texture(&wgpu::TextureDescriptor {
                label: Some("test heights"),
                size: wgpu::Extent3d {
                    width: samples,
                    height: samples,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let mut heights = start_heights(samples);
            let mut info = HeightfieldInfo {
                samples,
                half_width: 50.0,
                world_width: 100.0,
                height_scale: 3.0,
                height_offset: -2.0,
                albedo: [0.5; 3],
                albedo_generation: 0,
                generation: 1,
                origin_offset: [0.0; 3],
                edits: crate::HeightEdits::default(),
            };
            let mut frame = |gpu: &mut DdgiGpu, heights: &[f32], info: &HeightfieldInfo, key: u64| {
                device.queue.write_texture(
                    texture.as_image_copy(),
                    bytemuck::cast_slice(heights),
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(samples * 4),
                        rows_per_image: Some(samples),
                    },
                    wgpu::Extent3d {
                        width: samples,
                        height: samples,
                        depth_or_array_layers: 1,
                    },
                );
                belt.begin_frame();
                gpu.upload(&mut belt, d, &mut state, Some(info), key);
                let mut encoder = d.create_command_encoder(&Default::default());
                belt.flush(&mut encoder);
                gpu.record_pyramid(d, &mut encoder, &view, info, key);
                belt.upload(&queue);
                device.queue.submit([encoder.finish()]);
                belt.after_submit();
                device
                    .tracked()
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("poll");
            };
            frame(&mut gpu, &heights, &info, 7);
            assert_eq!(gpu.pyramid_stats.full_rebuilds, 1);
            for (step, rects) in edit_steps(samples).iter().enumerate() {
                apply_edit(samples, &mut heights, rects, step as u32 + samples);
                let mut edits = crate::HeightEdits::since(info.generation);
                for r in rects {
                    edits.push(*r);
                }
                info.generation += 1;
                info.edits = edits;
                frame(&mut gpu, &heights, &info, 7);
                assert_eq!(
                    gpu.pyramid_stats.regional_rebuilds,
                    step as u64 + 1,
                    "samples {samples} step {step} did not take the regional path"
                );
            }
            let regional = read_pyramid(&device, &gpu);
            // A full rebuild of the same heights: a new heights key.
            frame(&mut gpu, &heights, &info, 8);
            assert_eq!(gpu.pyramid_stats.full_rebuilds, 2);
            assert_eq!(gpu.pyramid_stats.last_texels, pyramid_texels(samples));
            let full = read_pyramid(&device, &gpu);
            for (level, (a, b)) in regional.iter().zip(&full).enumerate() {
                assert_eq!(a, b, "samples {samples} level {level}");
            }
            // The CPU mirror agrees with the GPU's full rebuild (to rounding: the GPU may fuse
            // the scale and offset).
            let cpu = cpu_pyramid(samples, &heights);
            for (level, (a, b)) in full.iter().zip(&cpu).enumerate() {
                for (x, y) in a.iter().zip(b) {
                    assert!(
                        (f32::from_bits(*x) - y).abs() <= 1e-4,
                        "level {level}: {} vs {y}",
                        f32::from_bits(*x)
                    );
                }
            }
            // A missed generation step (edits lead from 10, the pyramid holds 9) rebuilds in full.
            apply_edit(samples, &mut heights, &[[0, 0, 2, 2]], 99);
            info.edits = crate::HeightEdits::since(info.generation + 1);
            info.edits.push([0, 0, 2, 2]);
            info.generation += 2;
            frame(&mut gpu, &heights, &info, 8);
            assert_eq!(gpu.pyramid_stats.full_rebuilds, 3);
        }
    }
}
