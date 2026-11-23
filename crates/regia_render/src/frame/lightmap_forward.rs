// SPDX-License-Identifier: MPL-2.0
//! The baked lightmap in the forward pass (ADR-1623, ADR-1556 § 7): the receiver table (object key
//! → lightmap entry), the `LIGHTMAP` variant of the forward shader and its group 3: the atlas,
//! sampler, parameters and directions of [`crate::lightmap::LightmapGpu`], plus every entry's
//! `scale_offset` read at a dynamic offset (one block per entry), so one bind group serves every
//! receiver.
//!
//! Only draws whose object key is a receiver of the installed, enabled lightmap touch any of this.
//! Every other draw keeps its pipelines, bind groups and object block, so frames without
//! receivers are unchanged. This file holds the CPU colour loop's path, where a receiver draws with
//! its UV2 stream in vertex slot 1 (the mesh's `FrameMesh::uv1`:
//! [`crate::lightmap::receiver_mesh`] builds both); [`super::lightmap_paths`] holds the GPU-driven,
//! visibility-buffer and Mobile lite paths (ADR-2107). Group 3 bindings 12-17 do not collide with
//! the refraction (0-2, 7, 8), glass (3-6) or mask-decal (9-11) bindings declared in the same shader
//! file.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use naga_oil::compose::ShaderDefValue;
use regia_asset::lightmap::Lightmap;

use super::lightmap_paths::{
    InstancedSet, LiteSet, PathFrame, PathPipes, PathState, TABLE_BINDING, VisTableInputs,
    prepare_instanced_table, prepare_vis_table,
};
use super::{Draw, FrameRenderer, LabVertex, PipelineKey, samp_entry, shader_module, storage_uniform};
use crate::forward_lite::ForwardLite;
use crate::lightmap::{LightmapForwardStats, LightmapGpu, LightmapParamsGpu, LightmapSettings, LightmapTier};
use crate::pipeline_warm::DeferredPipeline;
use crate::visbuffer::VisBufferPass;

/// Group 3 binding of the atlas (`lightmap_atlas`).
const ATLAS_BINDING: u32 = 12;
/// Group 3 binding of the linear clamp sampler (`lightmap_sampler`).
const SAMPLER_BINDING: u32 = 13;
/// Group 3 binding of the parameter uniform (`lightmap_params`).
const PARAMS_BINDING: u32 = 14;
/// Group 3 binding of the directions (`lightmap_direction`).
const DIRECTION_BINDING: u32 = 15;
/// Group 3 binding of the draw's entry block (`lightmap_draw`, dynamic offset).
const DRAW_BINDING: u32 = 16;
/// Bytes of one entry block (`LightmapDraw` in `lab_forward.wgsl`: the entry's `scale_offset`).
const DRAW_BLOCK_BYTES: u64 = 16;

/// The `LIGHTMAP` colour variant of one target configuration.
#[derive(Clone)]
pub(super) struct LightmapSet {
    /// Opaque and alpha-tested colour (`fs_main`, depth equal to the prepass).
    pub(super) opaque: DeferredPipeline<wgpu::RenderPipeline>,
}

/// Group 3 layout, the pipeline layout and the variant sets (kept in [`super::Pipes`], so a
/// renderer that adopts warmed pipelines adopts these too).
#[derive(Default)]
pub(super) struct LightmapPipes {
    /// The layout and a process-unique id: a bind group built against another layout rebuilds.
    bgl: Option<(wgpu::BindGroupLayout, u64)>,
    layout: Option<wgpu::PipelineLayout>,
    /// `(encode, sun_rt)` → the module, composed by the first variant built.
    modules: HashMap<(bool, bool), Arc<OnceLock<wgpu::ShaderModule>>>,
    sets: HashMap<PipelineKey, LightmapSet>,
    /// The GPU-driven, visibility-buffer and lite variants ([`super::lightmap_paths`]).
    pub(super) paths: PathPipes,
}

fn next_layout_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Group 3's entries: the atlas, sampler, parameters and directions, then `last` (the draw block of
/// the CPU path, the table of the other paths).
pub(super) fn shared_entries(last: wgpu::BindGroupLayoutEntry) -> [wgpu::BindGroupLayoutEntry; 5] {
    [
        crate::lightmap::lightmap_texture_entry(ATLAS_BINDING),
        samp_entry(SAMPLER_BINDING),
        storage_uniform(PARAMS_BINDING, wgpu::ShaderStages::FRAGMENT),
        crate::lightmap::lightmap_texture_entry(DIRECTION_BINDING),
        last,
    ]
}

impl LightmapPipes {
    /// Group 3 layout (created on first use) and its id.
    fn bgl(&mut self, d: &wgpu::Device) -> (wgpu::BindGroupLayout, u64) {
        self.bgl
            .get_or_insert_with(|| {
                let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lab lightmap bgl"),
                    entries: &shared_entries(wgpu::BindGroupLayoutEntry {
                        binding: DRAW_BINDING,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: wgpu::BufferSize::new(DRAW_BLOCK_BYTES),
                        },
                        count: None,
                    }),
                });
                (bgl, next_layout_id())
            })
            .clone()
    }

    /// Game-frame warms: the other forward module (ADR-2035) of every set, built where the set's
    /// own variant is. Returns how many sets it asked for.
    pub(super) fn warm_sun_twins(&mut self, d: &wgpu::Device, groups: [&wgpu::BindGroupLayout; 3]) -> usize {
        let twins: Vec<(PipelineKey, LightmapSet)> = self
            .sets
            .iter()
            .map(|(key, set)| {
                (
                    PipelineKey {
                        sun_rt: !key.sun_rt,
                        ..*key
                    },
                    set.clone(),
                )
            })
            .collect();
        let asked = twins.len();
        for (key, built) in twins {
            let set = self.set(d, groups, key);
            if built.opaque.is_built() {
                set.opaque.get();
            }
        }
        asked
    }

    /// The pipeline layout of the CPU path (group 3 with the draw block).
    fn layout(&mut self, d: &wgpu::Device, groups: [&wgpu::BindGroupLayout; 3]) -> wgpu::PipelineLayout {
        let (bgl, _) = self.bgl(d);
        self.layout
            .get_or_insert_with(|| {
                // The material texture table (group 4) on the table path (ADR-863).
                let table = crate::texture_table::table_layout(d);
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lab lightmap layout"),
                    bind_group_layouts: &crate::texture_table::with_table(
                        &[Some(groups[0]), Some(groups[1]), Some(groups[2]), Some(&bgl)],
                        table.as_ref(),
                    ),
                    immediate_size: 0,
                })
            })
            .clone()
    }

    /// The lite variant for `key` at `tier` ([`super::lightmap_paths`]): this path's layout and
    /// the lab `LIGHTMAP` vertex stage.
    pub(super) fn lite_set(
        &mut self,
        d: &wgpu::Device,
        groups: [&wgpu::BindGroupLayout; 3],
        key: PipelineKey,
        tier: ForwardLite,
    ) -> LiteSet {
        // This family keeps the full forward module under either studio key (ADR-2308).
        let key = key.full_studio();
        let layout = self.layout(d, groups);
        let vertex = self.modules.entry((key.encode, key.sun_rt)).or_default().clone();
        self.paths.lite_set(d, &layout, &vertex, key, tier)
    }

    /// The variant for `key`. Creating it registers a background build (no compile here while
    /// warm workers run; without workers it builds now, like every deferred variant).
    pub(super) fn set(
        &mut self,
        d: &wgpu::Device,
        groups: [&wgpu::BindGroupLayout; 3],
        key: PipelineKey,
    ) -> LightmapSet {
        // This family keeps the full forward module under either studio key (ADR-2308).
        let key = key.full_studio();
        if let Some(set) = self.sets.get(&key) {
            return set.clone();
        }
        let layout = self.layout(d, groups);
        let module = self.modules.entry((key.encode, key.sun_rt)).or_default().clone();
        let device = wgpu::Device::clone(d);
        let set = LightmapSet {
            opaque: DeferredPipeline::new("lab lightmap", move || {
                let module = module.get_or_init(|| compile(&device, key.encode, key.sun_rt));
                color_pipeline(&device, &layout, module, module, key)
            }),
        };
        self.sets.insert(key, set.clone());
        set
    }
}

/// The `LIGHTMAP` forward module (alpha test compiled in, like the CPU-path forward module), with
/// the lookup functions spliced in by the composer ([`crate::lightmap::lightmap_forward_source`]).
pub(super) fn compile(d: &wgpu::Device, encode: bool, sun_rt: bool) -> wgpu::ShaderModule {
    let mut defs = HashMap::new();
    defs.insert("ALPHA_TEST".to_string(), ShaderDefValue::Bool(true));
    defs.insert("LIGHTMAP".to_string(), ShaderDefValue::Bool(true));
    if !encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    super::add_sun_rt_def(&mut defs, sun_rt);
    // The engine's own shader with a fixed define set: `shader_check` validates this combination,
    // so a failure here is a build defect, as for the other forward variants. The composer splices
    // the lookup functions in (`LIGHTMAP` is on).
    shader_module(
        d,
        "lab_forward_lightmap",
        include_str!("../../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    )
    .expect("LIGHTMAP forward variant")
}

/// A colour pipeline of a `LIGHTMAP` variant: the standard colour state with the UV2 stream
/// ([`crate::surface_graph::UV1_LAYOUT`], location 7) in vertex slot 1. `fragment` differs from
/// `vertex` on the lite path.
pub(super) fn color_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    vertex: &wgpu::ShaderModule,
    fragment: &wgpu::ShaderModule,
    key: PipelineKey,
) -> wgpu::RenderPipeline {
    let device = &crate::device::TrackedDevice::observe(device);
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("lab lightmap"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: vertex,
            entry_point: Some("vs_main"),
            buffers: &[Some(LabVertex::layout()), Some(crate::surface_graph::UV1_LAYOUT)],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: fragment,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: key.format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
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
        multisample: wgpu::MultisampleState {
            count: key.samples,
            mask: u64::MAX,
            alpha_to_coverage_enabled: false,
        },
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(device).as_deref(),
    })
}

/// The installed lightmap on the GPU, with the entry blocks and group 3.
pub(super) struct LightmapForwardGpu {
    /// The lightmap this copy was uploaded from (compared by `Arc` identity).
    map: Arc<Lightmap>,
    /// Atlas, directions, sampler and parameters.
    gpu: LightmapGpu,
    /// One [`DRAW_BLOCK_BYTES`] block per entry at `stride`.
    _entries: wgpu::Buffer,
    /// Bytes between entry blocks (the device's uniform offset alignment).
    stride: u64,
    bg: wgpu::BindGroup,
    /// [`LightmapPipes::bgl`] id `bg` was built against.
    layout_id: u64,
    /// The parameters last written.
    params: LightmapParamsGpu,
    /// The direction section was uploaded (a [`LightmapTier::Bilinear`] copy leaves it out).
    directions: bool,
}

impl LightmapForwardGpu {
    fn new(
        device: &crate::RenderDevice,
        d: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        layout_id: u64,
        map: Arc<Lightmap>,
        settings: &LightmapSettings,
        directions: bool,
    ) -> Self {
        let _span = tracing::info_span!("lightmap_forward_upload", entries = map.entries.len()).entered();
        let gpu = LightmapGpu::upload_with(device, &map, settings, wgpu::ShaderStages::FRAGMENT, directions);
        let align = u64::from(d.limits().min_uniform_buffer_offset_alignment).max(DRAW_BLOCK_BYTES);
        let stride = DRAW_BLOCK_BYTES.next_multiple_of(align);
        let blocks = map.entries.len().max(1);
        let mut bytes = vec![0u8; blocks * stride as usize];
        for (block, entry) in bytes.chunks_mut(stride as usize).zip(&map.entries) {
            block[..DRAW_BLOCK_BYTES as usize]
                .copy_from_slice(bytemuck::cast_slice(&entry.packed_scale_offset()));
        }
        let entries = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightmap entry blocks"),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        device.queue.write_buffer(&entries, 0, &bytes);
        let bg = crate::device::TrackedDevice::observe(d).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lab lightmap"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: ATLAS_BINDING,
                    resource: wgpu::BindingResource::TextureView(&gpu.view),
                },
                wgpu::BindGroupEntry {
                    binding: SAMPLER_BINDING,
                    resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: PARAMS_BINDING,
                    resource: gpu.params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: DIRECTION_BINDING,
                    resource: wgpu::BindingResource::TextureView(&gpu.direction_view),
                },
                wgpu::BindGroupEntry {
                    binding: DRAW_BINDING,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &entries,
                        offset: 0,
                        size: wgpu::BufferSize::new(DRAW_BLOCK_BYTES),
                    }),
                },
            ],
        });
        Self {
            params: LightmapParamsGpu::new(&map, settings),
            map,
            gpu,
            _entries: entries,
            stride,
            bg,
            layout_id,
            directions,
        }
    }

    /// Group 3 over `table` for the paths that read a table.
    pub(super) fn table_group(
        &self,
        d: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        table: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        crate::device::TrackedDevice::observe(d).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lab lightmap table"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: ATLAS_BINDING,
                    resource: wgpu::BindingResource::TextureView(&self.gpu.view),
                },
                wgpu::BindGroupEntry {
                    binding: SAMPLER_BINDING,
                    resource: wgpu::BindingResource::Sampler(&self.gpu.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: PARAMS_BINDING,
                    resource: self.gpu.params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: DIRECTION_BINDING,
                    resource: wgpu::BindingResource::TextureView(&self.gpu.direction_view),
                },
                wgpu::BindGroupEntry {
                    binding: TABLE_BINDING,
                    resource: table.as_entire_binding(),
                },
            ],
        })
    }
}

/// Per-renderer state: the installed lightmap, its settings, the receivers and the GPU copy.
#[derive(Default)]
pub(crate) struct LightmapForwardState {
    map: Option<Arc<Lightmap>>,
    settings: LightmapSettings,
    /// Object key → entry.
    receivers: HashMap<u64, u32>,
    gpu: Option<LightmapForwardGpu>,
    /// Hash of the receiver list as last set.
    receivers_key: u64,
    /// Hash of the lightmap, settings and receivers (screen-space cache key; 0 while inactive).
    content_key: u64,
    /// Receiver draws of this frame the lightmap cannot shade.
    unsupported: usize,
    stats: LightmapForwardStats,
    /// What this frame's quality allows ([`LightmapTier::for_frame`]).
    tier: LightmapTier,
    /// The lite shader's tier this frame.
    forward_tier: ForwardLite,
    /// The tables of the GPU-driven and visibility-buffer paths.
    paths: PathState,
    /// [`FrameRenderer::set_warm_lightmaps`].
    warm: bool,
    /// The calls made on this renderer, for when it stands in during a warm.
    calls: LightmapCalls,
}

/// Which lightmap calls a renderer has seen: a stand-in hands over only what it was told.
#[derive(Clone, Copy, Default)]
struct LightmapCalls {
    map: bool,
    receivers: bool,
    warm: bool,
}

impl LightmapForwardState {
    /// This state for a renderer rebuilt after a shader edit: the map, settings and receivers
    /// stay; the GPU copy (built against the old bind group layout) is made again on its next
    /// frame.
    pub(super) fn carried(mut self) -> Self {
        self.gpu = None;
        self
    }
}

impl LightmapForwardState {
    /// Receivers shade with the lightmap: one is installed and enabled and receivers are set.
    pub(super) fn active(&self) -> bool {
        crate::lightmap::LIGHTMAP_FORWARD_PASS
            && self.settings.enabled
            && self.map.is_some()
            && !self.receivers.is_empty()
    }

    /// Starts a frame's draw list: the lookup the frame's profile asks for and the lite tier.
    pub(super) fn begin_frame(&mut self, asked: LightmapTier, forward_tier: ForwardLite) {
        self.unsupported = 0;
        let tier = asked.for_frame(forward_tier);
        if (tier, forward_tier) != (self.tier, self.forward_tier) {
            self.tier = tier;
            self.forward_tier = forward_tier;
            self.refresh();
        }
    }

    /// Frame warms build the variants: a lightmap is installed (enabled or not) or the host asked.
    pub(super) fn warms(&self) -> bool {
        self.warm || self.map.is_some()
    }

    /// The host's [`FrameRenderer::set_warm_lightmaps`] flag (not the installed lightmap).
    pub(super) fn warm_asked(&self) -> bool {
        self.warm
    }

    /// [`FrameRenderer::set_warm_lightmaps`].
    pub(super) fn set_warm(&mut self, enabled: bool) {
        self.warm = enabled;
        self.calls.warm = true;
    }

    /// The lite shader's tier of the last frame.
    pub(super) fn forward_tier(&self) -> ForwardLite {
        self.forward_tier
    }

    /// The settings as the frame's tier caps them.
    fn effective(&self) -> LightmapSettings {
        self.tier.apply(self.settings)
    }

    /// Uploads the installed lightmap when no current GPU copy exists, and rewrites its parameters
    /// after a settings or tier change.
    fn ensure_gpu(
        &mut self,
        pipes: &mut LightmapPipes,
        device: &crate::RenderDevice,
        d: &wgpu::Device,
        map: &Arc<Lightmap>,
    ) {
        let effective = self.effective();
        let (bgl, layout_id) = pipes.bgl(d);
        let directions = self.tier != LightmapTier::Bilinear;
        let stale = self.gpu.as_ref().is_none_or(|gpu| {
            !Arc::ptr_eq(&gpu.map, map) || gpu.layout_id != layout_id || gpu.directions != directions
        });
        if stale {
            self.gpu = Some(LightmapForwardGpu::new(
                device,
                d,
                &bgl,
                layout_id,
                map.clone(),
                &effective,
                directions,
            ));
            self.paths.forget_groups();
        }
        if let Some(gpu) = self.gpu.as_mut() {
            let params = LightmapParamsGpu::new(map, &effective);
            if params != gpu.params {
                gpu.gpu.update_params(device, map, &effective);
                gpu.params = params;
            }
        }
    }

    /// Frame warms: the lightmap's GPU copy and the tables for a level's sizes.
    pub(super) fn warm_gpu(
        &mut self,
        device: &crate::RenderDevice,
        d: &wgpu::Device,
        pipes: &mut LightmapPipes,
        slots: usize,
        vis_rows: usize,
    ) {
        let Some(map) = self.map.clone() else {
            return;
        };
        if !atlas_fits(&map, d.limits().max_texture_dimension_2d) {
            return;
        }
        self.ensure_gpu(pipes, device, d, &map);
        let table_bgl = pipes.paths.table_bgl(d);
        if let Some(gpu) = self.gpu.as_ref() {
            self.paths.reserve(d, &table_bgl, gpu, slots, vis_rows);
        }
    }

    /// After the cull: the per-slot table and the batches with a receiver, when draws take the
    /// instanced path. `batches`: the frame's instanced batches, when it draws them.
    pub(super) fn prepare_instanced(
        &mut self,
        frame: &mut LightmapFrame,
        device: &crate::RenderDevice,
        draws: &[Draw],
        batches: Option<&[crate::gpu_driven::GpuBatch]>,
    ) {
        let (Some(batches), Some(table_bgl), Some(map), Some(gpu)) = (
            batches,
            frame.table_bgl.as_ref(),
            self.map.as_ref(),
            self.gpu.as_ref(),
        ) else {
            return;
        };
        prepare_instanced_table(
            &mut self.paths,
            (&device.device, &device.queue),
            (table_bgl, gpu, map),
            (draws, batches),
            &mut frame.paths,
        );
    }

    /// The entry `object_key` receives, when it is a receiver (the entry may be past the
    /// lightmap's: see [`Self::has_entry`]).
    pub(super) fn receiver(&self, object_key: u64) -> Option<u32> {
        self.receivers.get(&object_key).copied()
    }

    /// The installed lightmap has entry `entry`.
    pub(super) fn has_entry(&self, entry: u32) -> bool {
        self.map
            .as_ref()
            .is_some_and(|map| (entry as usize) < map.entries.len())
    }

    /// Counts a receiver draw the lightmap cannot shade.
    pub(super) fn note_unsupported(&mut self) {
        self.unsupported += 1;
    }

    /// Changes whenever what the receivers shade with changes (0 while inactive).
    pub(super) fn content_key(&self) -> u64 {
        if self.active() { self.content_key } else { 0 }
    }

    /// Takes what `other` (a warm's stand-in) was told, keeping this state's GPU copy when the
    /// lightmap is the same. The last call wins.
    fn adopt_installed(&mut self, other: Self) {
        if other.calls.map {
            let same = match (&self.map, &other.map) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                self.gpu = None;
            }
            self.map = other.map;
            self.settings = other.settings;
        }
        if other.calls.receivers {
            self.receivers = other.receivers;
            self.receivers_key = other.receivers_key;
        }
        if other.calls.warm {
            self.warm = other.warm;
        }
        self.refresh();
    }

    fn refresh(&mut self) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.map
            .as_ref()
            .map(|m| Arc::as_ptr(m) as usize)
            .hash(&mut hasher);
        bytemuck::bytes_of(&self.map.as_ref().map_or(
            LightmapParamsGpu {
                scale: [0.0; 4],
                spec: [0.0; 4],
            },
            |m| LightmapParamsGpu::new(m, &self.effective()),
        ))
        .hash(&mut hasher);
        self.receivers_key.hash(&mut hasher);
        self.forward_tier.hash(&mut hasher);
        self.content_key = hasher.finish() | 1;
        self.stats.receivers = self.receivers.len();
        self.stats.active = self.active();
        self.note_upload();
    }

    /// Records what the GPU copy holds in [`LightmapForwardStats`] (ADR-3145).
    fn note_upload(&mut self) {
        let (bytes, native, directions) = self.gpu.as_ref().map_or((0, false, false), |g| {
            (g.gpu.gpu_bytes(), g.gpu.native_blocks, g.gpu.directional)
        });
        self.stats.uploaded_bytes = bytes;
        self.stats.native_blocks = native;
        self.stats.uploaded_directions = directions;
    }
}

/// The frame's `LIGHTMAP` state for the forward pass: the variants of this frame's targets and the
/// group 3 bind group.
pub(super) struct LightmapFrame {
    sets: [Option<LightmapSet>; 2],
    bg: Option<wgpu::BindGroup>,
    stride: u64,
    /// The instanced path's group 3 layout, when draws take that path this frame.
    table_bgl: Option<wgpu::BindGroupLayout>,
    /// The other paths' state ([`super::lightmap_paths`]).
    pub(super) paths: PathFrame,
}

impl LightmapFrame {
    /// No receiver draws this frame.
    fn none() -> Self {
        Self {
            sets: [None, None],
            bg: None,
            stride: 0,
            table_bgl: None,
            paths: PathFrame::default(),
        }
    }

    /// The pipeline, group 3 and its dynamic offset for a receiver draw of entry `entry`, once
    /// its variant is built. `tested`: the draw needs the alpha test (the lite path has a pipeline
    /// without it).
    pub(super) fn bind(
        &self,
        cull_off: bool,
        tested: bool,
        entry: u32,
    ) -> Option<(&wgpu::RenderPipeline, &wgpu::BindGroup, u32)> {
        let side = usize::from(cull_off);
        let pipe = match self.paths.lite[side].as_ref() {
            Some(lite) if tested => lite.tested.try_get()?,
            Some(lite) => lite.opaque.try_get()?,
            None => self.sets[side].as_ref()?.opaque.try_get()?,
        };
        let bg = self.bg.as_ref()?;
        let offset = u32::try_from(u64::from(entry) * self.stride).ok()?;
        Some((pipe, bg, offset))
    }
}

/// The path a receiver draw takes this frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiverPath {
    /// A cluster instance, shaded by the visibility resolve.
    Vis,
    /// In the instanced batches.
    Instanced,
    /// From the CPU colour loop (the lab shader, or the lite shader on a lite tier).
    Cpu,
}

fn receiver_path(draw: &Draw) -> ReceiverPath {
    if draw.vis {
        ReceiverPath::Vis
    } else if draw.cpu_uniform {
        ReceiverPath::Cpu
    } else {
        ReceiverPath::Instanced
    }
}

/// What [`resolve_lightmap_sets`] borrows from the renderer.
pub(super) struct LightmapResolve<'a> {
    pub(super) pipes: &'a mut LightmapPipes,
    /// The view, object and material layouts.
    pub(super) groups: [&'a wgpu::BindGroupLayout; 3],
    pub(super) culler: &'a mut crate::gpu_driven::GpuCuller,
    pub(super) visbuffer: &'a mut VisBufferPass,
    pub(super) state: &'a mut LightmapForwardState,
    pub(super) device: &'a crate::RenderDevice,
    pub(super) d: &'a wgpu::Device,
    /// The frame's culled and cull-none targets.
    pub(super) keys: (PipelineKey, PipelineKey),
}

/// The `LIGHTMAP` variants and group 3 of the frame's receiver draws, resolved like the mask-decal
/// variants: built here where this thread may wait; mid-play a receiver whose variant is not
/// built yet keeps the standard surface (and the probe light) this frame (its `lightmap` is
/// cleared). Uploads the lightmap on first use and rewrites its parameters after a settings
/// change. Records the frame's [`LightmapForwardStats`].
pub(super) fn resolve_lightmap_sets(resolve: LightmapResolve<'_>, draws: &mut [Draw]) -> LightmapFrame {
    let LightmapResolve {
        pipes,
        groups,
        culler,
        visbuffer,
        state,
        device,
        d,
        keys: (key, key_ds),
    } = resolve;
    let mut frame = LightmapFrame::none();
    state.stats.unsupported = state.unsupported;
    state.stats.drawn = 0;
    state.stats.fallback = 0;
    state.stats.instanced = 0;
    state.stats.visbuffer = 0;
    state.stats.lite = 0;
    state.stats.tier = state.tier.name();
    if !draws.iter().any(|draw| draw.lightmap.is_some()) {
        return frame;
    }
    let _span = tracing::info_span!("lightmap_forward_resolve").entered();
    let Some(map) = state.map.clone() else {
        for draw in draws.iter_mut() {
            draw.lightmap = None;
        }
        return frame;
    };
    // An atlas past the device's texture limit (a downlevel mobile device) is not uploaded: its
    // receivers keep the probe light, counted as unsupported.
    if !atlas_fits(&map, d.limits().max_texture_dimension_2d) {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                target: "regia_render",
                width = map.width,
                height = map.height,
                limit = d.limits().max_texture_dimension_2d,
                "the lightmap atlas is larger than this device's 2D texture limit: its receivers keep \
                 the probe light (bake with a smaller --max-atlas for this tier)"
            );
        }
        for draw in draws.iter_mut().filter(|draw| draw.lightmap.is_some()) {
            draw.lightmap = None;
            state.stats.unsupported += 1;
        }
        return frame;
    }
    state.ensure_gpu(pipes, device, d, &map);
    state.note_upload();
    if let Some(gpu) = state.gpu.as_ref() {
        frame.bg = Some(gpu.bg.clone());
        frame.stride = gpu.stride;
    }
    let lite = state.forward_tier != ForwardLite::Off;
    let mut uses = [[false; 2]; 3];
    for draw in draws.iter().filter(|draw| draw.lightmap.is_some()) {
        uses[receiver_path(draw) as usize][usize::from(draw.cull_off)] = true;
    }
    let side_key = |side: usize| if side == 1 { key_ds } else { key };
    let mut need: Vec<DeferredPipeline<wgpu::RenderPipeline>> = Vec::new();
    let waits = crate::pipeline_warm::may_block();
    let mut wait_for = |pipe: &DeferredPipeline<wgpu::RenderPipeline>| {
        if waits && !pipe.is_built() && !need.iter().any(|n| n.same(pipe)) {
            need.push(pipe.clone());
        }
    };
    for (side, _) in uses[ReceiverPath::Cpu as usize]
        .iter()
        .enumerate()
        .filter(|(_, used)| **used)
    {
        if lite {
            let set = pipes.lite_set(d, groups, side_key(side), state.forward_tier);
            wait_for(&set.opaque);
            wait_for(&set.tested);
            frame.paths.lite[side] = Some(set);
        } else {
            let set = pipes.set(d, groups, side_key(side));
            wait_for(&set.opaque);
            frame.sets[side] = Some(set);
        }
    }
    if uses[ReceiverPath::Instanced as usize].contains(&true) {
        match culler.instanced_object_layout(d) {
            Ok(objects) => {
                frame.table_bgl = Some(pipes.paths.table_bgl(d));
                for (side, _) in uses[ReceiverPath::Instanced as usize]
                    .iter()
                    .enumerate()
                    .filter(|(_, used)| **used)
                {
                    let set: InstancedSet =
                        pipes
                            .paths
                            .instanced_set(d, [groups[0], &objects, groups[2]], side_key(side));
                    wait_for(&set.opaque);
                    wait_for(&set.mask);
                    frame.paths.instanced[side] = Some(set);
                }
            }
            Err(err) => tracing::warn!(target: "regia_render", %err, "no instanced lightmap layout"),
        }
    }
    let vis_pipe = if uses[ReceiverPath::Vis as usize].contains(&true) {
        match visbuffer.resolve_layout_group(d) {
            Ok(resolve) => {
                let pipe = pipes.paths.vis_pipeline(
                    d,
                    [groups[0], &resolve, groups[2]],
                    key.format,
                    key.encode,
                    key.sun_rt,
                );
                wait_for(&pipe);
                Some(pipe)
            }
            Err(err) => {
                tracing::warn!(target: "regia_render", %err, "no visibility resolve layout");
                None
            }
        }
    } else {
        None
    };
    if !need.is_empty() {
        let jobs: Vec<_> = need
            .into_iter()
            .map(|p| {
                move || {
                    p.get();
                }
            })
            .collect();
        crate::pipeline_warm::join_all(jobs);
    }
    let vis_ready = vis_pipe.as_ref().and_then(|pipe| pipe.try_get());
    let stats = &mut state.stats;
    let (mut drawn, mut fallback) = (0, 0);
    for draw in draws.iter_mut() {
        let Some(entry) = draw.lightmap else {
            continue;
        };
        let path = receiver_path(draw);
        let side = usize::from(draw.cull_off);
        let ready = match path {
            ReceiverPath::Vis => vis_ready.is_some(),
            ReceiverPath::Instanced => frame.paths.instanced[side]
                .as_ref()
                .is_some_and(|set| set.opaque.is_built() && set.mask.is_built()),
            ReceiverPath::Cpu => frame
                .bind(draw.cull_off, draw.alpha_mode != crate::ALPHA_OPAQUE, entry)
                .is_some(),
        };
        if !ready {
            draw.lightmap = None;
            if draw.visible {
                fallback += 1;
            }
        } else if draw.visible {
            drawn += 1;
            match path {
                ReceiverPath::Vis => stats.visbuffer += 1,
                ReceiverPath::Instanced => stats.instanced += 1,
                ReceiverPath::Cpu if lite => stats.lite += 1,
                ReceiverPath::Cpu => {}
            }
        }
    }
    stats.drawn = drawn;
    stats.fallback = fallback;
    if let Some(pipeline) = vis_ready
        && draws.iter().any(|draw| draw.vis && draw.lightmap.is_some())
        && let Some(gpu) = state.gpu.as_ref()
    {
        let table_bgl = pipes.paths.table_bgl(d);
        prepare_vis_table(
            &mut state.paths,
            &VisTableInputs {
                d,
                q: &device.queue,
                visbuffer,
                table_bgl: &table_bgl,
                gpu,
                map: &map,
            },
            pipeline,
            draws,
            &mut frame.paths,
        );
    }
    frame
}

/// The atlas (and so its direction texture) fits a device whose 2D textures are at most `limit`
/// texels on a side.
fn atlas_fits(map: &Lightmap, limit: u32) -> bool {
    map.width <= limit && map.height <= limit
}

impl FrameRenderer {
    /// Installs the baked lightmap the forward pass shades its receivers with
    /// ([`crate::lightmap`], ADR-1623) and its settings (the flag, intensity, bicubic and
    /// directional switches); `None` removes it. It uploads with the first frame that draws a
    /// receiver; the same `Arc` with new settings only rewrites the parameter uniform. Receivers
    /// shade with it while `settings.enabled` is set; every other draw is unchanged.
    pub fn set_lightmap(&mut self, lightmap: Option<Arc<Lightmap>>, settings: LightmapSettings) {
        self.installed_since.lightmap = true;
        let state = &mut self.lightmap_forward;
        state.calls.map = true;
        let same = match (&state.map, &lightmap) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same && state.settings == settings {
            return;
        }
        if !same {
            state.gpu = None;
        }
        state.map = lightmap;
        state.settings = settings;
        state.refresh();
    }

    /// Replaces the lightmap receivers for subsequent frames: `(object_key, entry)` pairs, the
    /// [`crate::FrameMesh::object_key`] of a drawn object and the index of its entry in the
    /// installed lightmap ([`regia_asset::lightmap::Lightmap::entry`] finds one by name). The
    /// object must be drawn from that entry's receiver geometry, with its UV2 as the mesh's
    /// second UV stream ([`crate::lightmap::receiver_mesh`]). A receiver the lightmap cannot shade
    /// (an entry it does not have, a material graph, a blended, transmissive, skinned or
    /// morphed draw, a LOD cross-fade, a mask-decal instance, a mesh without its UV2 stream) keeps
    /// the probe light and is counted in [`LightmapForwardStats::unsupported`]. An empty slice
    /// draws every mesh as without lightmaps.
    pub fn set_lightmap_receivers(&mut self, receivers: &[(u64, u32)]) {
        self.installed_since.lightmap = true;
        let state = &mut self.lightmap_forward;
        state.calls.receivers = true;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        receivers.hash(&mut hasher);
        state.receivers_key = hasher.finish();
        state.receivers.clear();
        state.receivers.extend(receivers.iter().copied());
        state.refresh();
    }

    /// The lightmap installed with [`Self::set_lightmap`].
    #[must_use]
    pub fn lightmap(&self) -> Option<&Arc<Lightmap>> {
        self.lightmap_forward.map.as_ref()
    }

    /// What the forward pass did with the lightmap in the last frame: receivers set, draws shaded
    /// with it, draws that kept the probe light while its variant builds, and receivers it cannot
    /// shade.
    #[must_use]
    pub fn lightmap_forward_stats(&self) -> LightmapForwardStats {
        self.lightmap_forward.stats
    }

    /// What a warm stand-in was told about the lightmap, moved over to the renderer that comes back
    /// from the warm ([`Self::set_warm_lightmaps`]). The last call wins.
    pub(super) fn take_stand_in_lightmap(&mut self, stand_in: &mut Self) {
        let state = std::mem::take(&mut stand_in.lightmap_forward);
        self.lightmap_forward.adopt_installed(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::lightmap::{Coverage, LightmapBakeInfo, LightmapLayout};

    fn atlas(width: u32, height: u32) -> Lightmap {
        let texels = (width * height) as usize;
        Lightmap::from_irradiance(
            LightmapLayout {
                width,
                height,
                density: 1.0,
                padding: 1,
                entries: Vec::new(),
                unwraps: Vec::new(),
            },
            &vec![[1.0; 3]; texels],
            vec![Coverage::BAKED; texels],
            LightmapBakeInfo::default(),
        )
        .expect("valid")
    }

    /// A warm's stand-in hands over the calls it saw and nothing else: told only a lightmap, it
    /// leaves the receivers the real renderer had.
    #[test]
    fn a_stand_in_hands_over_only_what_it_was_told() {
        let map = Arc::new(atlas(4, 2));
        let on = LightmapSettings {
            enabled: true,
            ..LightmapSettings::default()
        };
        let mut real = FrameRenderer::new();
        real.set_lightmap_receivers(&[(7, 0)]);
        let mut stand_in = FrameRenderer::new();
        stand_in.set_lightmap(Some(map.clone()), on);
        real.absorb_stand_in(&mut stand_in);
        assert!(real.lightmap().is_some_and(|m| Arc::ptr_eq(m, &map)));
        assert_eq!(real.lightmap_forward.receiver(7), Some(0), "the receivers stay");
        assert!(real.lightmap_forward.active());
        assert!(real.lightmap_forward.warms(), "an installed lightmap warms");

        let mut stand_in = FrameRenderer::new();
        stand_in.set_lightmap_receivers(&[(9, 1)]);
        real.absorb_stand_in(&mut stand_in);
        assert!(real.lightmap().is_some(), "the lightmap stays");
        assert_eq!(real.lightmap_forward.receiver(9), Some(1));
        assert_eq!(real.lightmap_forward.receiver(7), None, "the last call wins");

        let mut other = FrameRenderer::new();
        assert!(!other.lightmap_forward.warms(), "nothing to warm by default");
        let mut stand_in = FrameRenderer::new();
        stand_in.set_warm_lightmaps(true);
        other.absorb_stand_in(&mut stand_in);
        assert!(other.lightmap_forward.warms());
        assert!(other.lightmap().is_none());
    }

    /// An atlas past the device's 2D texture limit on either side is not uploaded.
    #[test]
    fn an_atlas_past_the_texture_limit_does_not_fit() {
        assert!(atlas_fits(&atlas(64, 32), 64));
        assert!(!atlas_fits(&atlas(128, 32), 64));
        assert!(!atlas_fits(&atlas(32, 128), 64));
    }
}
