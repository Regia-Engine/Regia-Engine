// SPDX-License-Identifier: MPL-2.0
//! Translucent shadow casters: a colored shadow map beside the opaque one.
//!
//! Alpha-blended and transmissive draws (glass, canopies, visors) are not in the opaque shadow
//! passes. They draw into a second set of maps — per sun cascade a layer of an RGB
//! transmittance array plus the nearest translucent depth, and the same pair for the
//! local-light atlas at a quarter of its edge — and the receiver multiplies its light by the
//! transmittance where it lies behind the translucent depth (`lab_forward.wgsl`
//! `sample_csm_transmittance`, `sample_atlas_transmittance`). This is the non-stochastic
//! colored shadow map of McGuire & Enderton 2011 ("Colored Stochastic Shadow Maps", § 2): a
//! receiver behind several layers gets their product; a receiver between two layers gets the
//! product of both too (the known limit of one depth per texel).
//!
//! Opaque shadowing is untouched: frames without a translucent caster record nothing here
//! and the shader skips the lookups (`ShadowUniforms::count` y/z = 0), so their pixels are
//! bit-identical. The pipelines are built with the frame's device pipelines (no create after
//! warm-up); the maps are allocated the first frame a translucent caster appears.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use std::collections::HashMap;

use naga_oil::compose::ShaderDefValue;

use super::{MAX_SHADOW_TILES, samp_entry, shader_module, storage_uniform, tex_entry};
use crate::RenderError;
use crate::paged_shadow::Aabb3;

/// Colour format of the transmittance maps (RGB transmittance; alpha unused).
pub(super) const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Translucency map edge per cascade = the cascade edge / this.
pub(super) const CSM_DIVISOR: u32 = 2;
/// Translucency atlas edge = the local-light atlas edge / this.
pub(super) const ATLAS_DIVISOR: u32 = 4;
/// Size of the `trans_bounds` uniform: one UV rect per cascade, then the casters' world
/// bounding sphere.
const BOUNDS_BYTES: u64 = 16 * (crate::forward::CASCADE_COUNT as u64 + 1);
/// Margin around the casters' footprint in translucency texels (receiver normal offset and
/// the bilinear footprint).
const BOUNDS_PAD_TEXELS: f32 = 4.0;
/// Dynamic-offset stride of one light view-projection in [`TranslucentMaps::vp_buf`].
const VP_STRIDE: u64 = 256;
/// View-projection slots: four cascades, then the atlas tiles.
const VP_SLOTS: usize = crate::forward::CASCADE_COUNT as usize + MAX_SHADOW_TILES;

/// Translucency map edge for a cascade edge (never below 16 texels).
#[must_use]
pub(super) fn csm_edge(csm_size: u32) -> u32 {
    (csm_size / CSM_DIVISOR).max(16)
}

/// Translucency atlas edge for a local-light atlas edge (never below 64 texels).
#[must_use]
pub(super) fn atlas_edge(atlas_size: u32) -> u32 {
    (atlas_size / ATLAS_DIVISOR).max(64)
}

/// One variant's caster pipelines: `[culled, cull none]` per pass.
#[derive(Clone)]
struct CasterSet {
    /// Nearest translucent depth (`fs_depth`).
    depth: [wgpu::RenderPipeline; 2],
    /// Transmittance product (`fs_color`, blend dst × src).
    color: [wgpu::RenderPipeline; 2],
}

/// Device-level pipelines (built with the frame's other pipelines, once per device:
/// [`crate::RenderDevice::shared_pipelines`]).
#[derive(Clone)]
pub(super) struct TranslucentPipes {
    /// Group 0: one light view-projection, dynamic offset.
    vp_bgl: wgpu::BindGroupLayout,
    /// Group 3 of the `GLASS` variant: glass parameters, crack page, sampler.
    glass_bgl: wgpu::BindGroupLayout,
    plain: CasterSet,
    /// Damaged or own-canopy glass instances (holes cast nothing, cracks scatter).
    glass: CasterSet,
    /// On the texture table path (ADR-863): the plain casters over a graph material's own group
    /// (constants, textures and sampler), which the record-only standard layout cannot bind.
    /// `None` off the table path, where one material layout serves both.
    own_textures: Option<CasterSet>,
}

impl TranslucentPipes {
    /// Builds the caster pipelines (plain and `GLASS`) over the forward pass's object and
    /// material groups, and on the texture table path the plain set over `graph_mat_bgl` too.
    pub(super) fn new(
        d: &wgpu::Device,
        object_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
        graph_mat_bgl: &wgpu::BindGroupLayout,
    ) -> Result<Self, RenderError> {
        let vp_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("translucent shadow vp bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(64),
                },
                count: None,
            }],
        });
        let glass_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("translucent shadow glass bgl"),
            entries: &[
                storage_uniform(0, wgpu::ShaderStages::FRAGMENT),
                tex_entry(1),
                samp_entry(2),
            ],
        });
        let plain = caster_set(d, &[Some(&vp_bgl), Some(object_bgl), Some(mat_bgl)], false, false)?;
        let glass = caster_set(
            d,
            &[Some(&vp_bgl), Some(object_bgl), Some(mat_bgl), Some(&glass_bgl)],
            true,
            false,
        )?;
        let own_textures = if crate::texture_table::device_support(d).capacity().is_some() {
            Some(caster_set(
                d,
                &[Some(&vp_bgl), Some(object_bgl), Some(graph_mat_bgl)],
                false,
                true,
            )?)
        } else {
            None
        };
        Ok(Self {
            vp_bgl,
            glass_bgl,
            plain,
            glass,
            own_textures,
        })
    }

    /// Group-3 layout of the `GLASS` caster pipelines.
    pub(super) fn glass_bgl(&self) -> &wgpu::BindGroupLayout {
        &self.glass_bgl
    }
}

/// One caster variant over `groups`: `GLASS` when `glass` is set, and over a material's own
/// textures (no texture table, `PER_MATERIAL_TEXTURES_DEF`) when `own_textures` is set.
fn caster_set(
    d: &wgpu::Device,
    groups: &[Option<&wgpu::BindGroupLayout>],
    glass: bool,
    own_textures: bool,
) -> Result<CasterSet, RenderError> {
    let mut defs = HashMap::new();
    if glass {
        defs.insert("GLASS".to_string(), ShaderDefValue::Bool(true));
    }
    if own_textures {
        defs.insert(
            crate::bindless::PER_MATERIAL_TEXTURES_DEF.to_string(),
            ShaderDefValue::Bool(true),
        );
    }
    let module = shader_module(
        d,
        if glass {
            "translucent_shadow_glass"
        } else {
            "translucent_shadow"
        },
        include_str!("../../shaders/translucent_shadow.wgsl"),
        "translucent_shadow.wgsl",
        defs,
    )?;
    // The material texture table (group 4) on the table path (ADR-863), unless the variant binds
    // a material's own textures.
    let table = if own_textures {
        None
    } else {
        crate::texture_table::table_layout(d)
    };
    let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("translucent shadow layout"),
        bind_group_layouts: &crate::texture_table::with_table(groups, table.as_ref()),
        immediate_size: 0,
    });
    let pipeline = |color: bool, cull: Option<wgpu::Face>| {
        d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(if color {
                "translucent shadow color"
            } else {
                "translucent shadow depth"
            }),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[Some(super::LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(if color { "fs_color" } else { "fs_depth" }),
                targets: &[Some(wgpu::ColorTargetState {
                    format: COLOR_FORMAT,
                    blend: color.then_some(wgpu::BlendState {
                        color: MULTIPLY,
                        alpha: MULTIPLY,
                    }),
                    write_mask: if color {
                        wgpu::ColorWrites::ALL
                    } else {
                        wgpu::ColorWrites::empty()
                    },
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: cull,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(!color),
                depth_compare: Some(if color {
                    wgpu::CompareFunction::Always
                } else {
                    wgpu::CompareFunction::Less
                }),
                stencil: wgpu::StencilState::default(),
                // Receiver-side bias only, like the opaque maps (`biased_shadow_depth`).
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(d).as_deref(),
        })
    };
    let back = Some(wgpu::Face::Back);
    Ok(CasterSet {
        depth: [pipeline(false, back), pipeline(false, None)],
        color: [pipeline(true, back), pipeline(true, None)],
    })
}

/// `dst × src`: each layer multiplies its transmittance into the map.
const MULTIPLY: wgpu::BlendComponent = wgpu::BlendComponent {
    src_factor: wgpu::BlendFactor::Zero,
    dst_factor: wgpu::BlendFactor::Src,
    operation: wgpu::BlendOperation::Add,
};

/// One colour + depth map pair (a cascade array or the atlas).
struct MapPair {
    edge: u32,
    layers: u32,
    /// Whole-texture views the receiver samples (`D2Array` for cascades, `D2` for the atlas).
    color_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    /// Per-layer attachment views.
    color_layers: Vec<wgpu::TextureView>,
    depth_layers: Vec<wgpu::TextureView>,
}

impl MapPair {
    fn new(d: &wgpu::Device, edge: u32, layers: u32, array: bool, label: &str) -> Self {
        let make = |format: wgpu::TextureFormat| {
            d.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: edge,
                    height: edge,
                    depth_or_array_layers: layers,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let color = make(COLOR_FORMAT);
        let depth = make(wgpu::TextureFormat::Depth32Float);
        let whole = |texture: &wgpu::Texture| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(if array {
                    wgpu::TextureViewDimension::D2Array
                } else {
                    wgpu::TextureViewDimension::D2
                }),
                ..Default::default()
            })
        };
        let layer = |texture: &wgpu::Texture, i: u32| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D2),
                base_array_layer: i,
                array_layer_count: Some(1),
                ..Default::default()
            })
        };
        Self {
            edge,
            layers,
            color_view: whole(&color),
            depth_view: whole(&depth),
            color_layers: (0..layers).map(|i| layer(&color, i)).collect(),
            depth_layers: (0..layers).map(|i| layer(&depth, i)).collect(),
        }
    }
}

/// Per-renderer translucency maps, their view-projection buffer and the receiver sampler.
#[derive(Default)]
pub(super) struct TranslucentMaps {
    /// 1² placeholders bound until the first translucent caster (cascade array, atlas).
    dummy: Option<(MapPair, MapPair)>,
    csm: Option<MapPair>,
    atlas: Option<MapPair>,
    sampler: Option<wgpu::Sampler>,
    vp: Option<(wgpu::Buffer, wgpu::BindGroup)>,
    vp_bytes: Vec<u8>,
    /// Per cascade the shadow-map UV rect the translucent casters cover (`[u0, v0, u1, v1]`;
    /// empty = `[1, 1, 0, 0]`): receivers outside it skip the lookups.
    bounds: Option<wgpu::Buffer>,
    bounds_bytes: Vec<u8>,
    /// Bumped whenever a bound view changes (the frame drops its cached view groups).
    generation: u64,
    /// The frame's view-projection bytes before the changed-bytes compare, kept between frames
    /// (ADR-385).
    vp_scratch: Vec<u8>,
}

/// The views and sampler the forward pass binds (view group bindings 22–26).
pub(super) struct ReceiverViews<'a> {
    pub csm_color: &'a wgpu::TextureView,
    pub csm_depth: &'a wgpu::TextureView,
    pub atlas_color: &'a wgpu::TextureView,
    pub atlas_depth: &'a wgpu::TextureView,
    pub sampler: &'a wgpu::Sampler,
    /// [`TranslucentMaps::bounds`] (`trans_bounds`).
    pub bounds: &'a wgpu::Buffer,
}

impl TranslucentMaps {
    /// Generation of the bound views ([`Self::receiver_views`]).
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Creates the placeholders, the sampler and the view-projection buffer once.
    pub(super) fn ensure_base(&mut self, d: &wgpu::Device, pipes: &TranslucentPipes) {
        if self.dummy.is_none() {
            self.dummy = Some((
                MapPair::new(d, 1, 1, true, "translucent shadow dummy"),
                MapPair::new(d, 1, 1, false, "translucent shadow dummy"),
            ));
            self.generation += 1;
        }
        if self.sampler.is_none() {
            self.sampler = Some(d.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("translucent shadow"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }));
        }
        if self.bounds.is_none() {
            self.bounds = Some(d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("translucent shadow bounds"),
                size: BOUNDS_BYTES,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if self.vp.is_none() {
            let buf = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("translucent shadow vp"),
                size: VP_STRIDE * VP_SLOTS as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bg = crate::device::TrackedDevice::observe(d).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("translucent shadow vp"),
                layout: &pipes.vp_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buf,
                        offset: 0,
                        size: wgpu::BufferSize::new(64),
                    }),
                }],
            });
            self.vp = Some((buf, bg));
        }
    }

    /// (Re)allocates the cascade maps at `edge`² × `layers` when that changed.
    pub(super) fn ensure_csm(&mut self, d: &wgpu::Device, edge: u32, layers: u32) {
        let layers = layers.clamp(1, crate::forward::CASCADE_COUNT);
        if self
            .csm
            .as_ref()
            .is_none_or(|m| m.edge != edge || m.layers != layers)
        {
            self.csm = Some(MapPair::new(d, edge, layers, true, "translucent shadow csm"));
            self.generation += 1;
        }
    }

    /// (Re)allocates the atlas maps at `edge`² when that changed.
    pub(super) fn ensure_atlas(&mut self, d: &wgpu::Device, edge: u32) {
        if self.atlas.as_ref().is_none_or(|m| m.edge != edge) {
            self.atlas = Some(MapPair::new(d, edge, 1, false, "translucent shadow atlas"));
            self.generation += 1;
        }
    }

    /// What the forward pass binds: the live maps, or the placeholders. `None` before
    /// [`Self::ensure_base`].
    pub(super) fn receiver_views(&self) -> Option<ReceiverViews<'_>> {
        let (dummy_csm, dummy_atlas) = self.dummy.as_ref()?;
        let csm = self.csm.as_ref().unwrap_or(dummy_csm);
        let atlas = self.atlas.as_ref().unwrap_or(dummy_atlas);
        Some(ReceiverViews {
            csm_color: &csm.color_view,
            csm_depth: &csm.depth_view,
            atlas_color: &atlas.color_view,
            atlas_depth: &atlas.depth_view,
            sampler: self.sampler.as_ref()?,
            bounds: self.bounds.as_ref()?,
        })
    }
}

/// One translucent caster as the passes draw it.
pub(super) struct Caster<'a> {
    pub vb: &'a wgpu::Buffer,
    pub ib: &'a wgpu::Buffer,
    pub index_count: u32,
    /// First index of the mesh in `ib` (a mesh-arena range).
    pub first_index: u32,
    /// First vertex of the mesh in `vb`.
    pub base_vertex: i32,
    pub object: &'a wgpu::BindGroup,
    pub material: &'a wgpu::BindGroup,
    /// Dynamic offsets of `material`: the record offset on the texture table path (one shared
    /// record group, ADR-863), none elsewhere.
    pub material_offsets: &'a [u32],
    /// `material` binds its own textures (a graph material on the texture table path): it draws
    /// with [`TranslucentPipes::own_textures`].
    pub own_textures: bool,
    /// Cull-none pipelines (a double-sided material).
    pub double_sided: bool,
    /// Glass instance group ([`TranslucentPipes::glass_bgl`]): the `GLASS` pipelines.
    pub glass: Option<&'a wgpu::BindGroup>,
    /// World bounds; `None` (skinned) draws into every map.
    pub aabb: Option<Aabb3>,
    /// Sun cascades the caster may shadow (bit `i` = cascade `i`, [`super::cascade_mask`]).
    pub cascades: u8,
}

/// One local-light atlas tile: its view-projection and texel rect in the opaque atlas.
pub(super) struct Tile {
    pub view_proj: [[f32; 4]; 4],
    pub rect: [u32; 4],
}

/// What this frame draws.
pub(super) struct Frame<'a> {
    /// Light view-projection per translucent cascade (at most [`crate::forward::CASCADE_COUNT`]).
    pub cascades: &'a [[[f32; 4]; 4]],
    /// Local-light tiles (empty = no atlas translucency this frame).
    pub tiles: &'a [Tile],
    /// Edge of the opaque atlas the tile rects are in.
    pub atlas_size: u32,
    pub casters: &'a [Caster<'a>],
    /// The material texture table group (`texture_table`, ADR-863) on the table path: the
    /// caster pipelines bind it as group 4.
    pub texture_table: Option<&'a wgpu::BindGroup>,
}

/// Draw counts of one [`record`] (the shadow report's translucent fields).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Recorded {
    pub passes: u32,
    pub draws: u32,
}

/// Margin added to the casters' bounding sphere, metres.
const SPHERE_MARGIN_M: f32 = 0.25;

/// Bounding sphere `[x, y, z, r]` of every caster (radius 1e30 when one is unbounded): the
/// receiver skips the lookups outside the cylinder it sweeps away from the sun.
fn caster_sphere(casters: &[Caster<'_>]) -> [f32; 4] {
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for caster in casters {
        let Some(aabb) = caster.aabb else {
            return [0.0, 0.0, 0.0, 1.0e30];
        };
        for i in 0..3 {
            min[i] = min[i].min(aabb.min[i]);
            max[i] = max[i].max(aabb.max[i]);
        }
    }
    if min[0] > max[0] {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let c = [
        (min[0] + max[0]) * 0.5,
        (min[1] + max[1]) * 0.5,
        (min[2] + max[2]) * 0.5,
    ];
    let h = [
        (max[0] - min[0]) * 0.5,
        (max[1] - min[1]) * 0.5,
        (max[2] - min[2]) * 0.5,
    ];
    // + a margin for the receiver's normal offset and the filter footprint.
    [
        c[0],
        c[1],
        c[2],
        (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt() + SPHERE_MARGIN_M,
    ]
}

/// `true` when the box may be inside the clip volume of `vp` (no corner-plane separates it).
/// Conservative: a box crossing a perspective light's eye plane is kept.
pub(super) fn may_overlap(aabb: &Aabb3, vp: &[[f32; 4]; 4]) -> bool {
    let m = regia_core::glam::Mat4::from_cols_array_2d(vp);
    let mut outside = [true; 6];
    for i in 0..8 {
        let p = regia_core::glam::Vec4::new(
            if i & 1 == 0 { aabb.min[0] } else { aabb.max[0] },
            if i & 2 == 0 { aabb.min[1] } else { aabb.max[1] },
            if i & 4 == 0 { aabb.min[2] } else { aabb.max[2] },
            1.0,
        );
        let c = m * p;
        let tests = [c.x < -c.w, c.x > c.w, c.y < -c.w, c.y > c.w, c.z < 0.0, c.z > c.w];
        for (o, t) in outside.iter_mut().zip(tests) {
            *o &= t;
        }
    }
    !outside.iter().any(|&o| o)
}

/// Records the translucency passes: each cascade layer, then the atlas (one pass, a viewport
/// per tile). Every map drawn is cleared first (white, far), so nothing stale stays.
pub(super) fn record(
    d: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    belt: &mut crate::staging::StagingBelt,
    pipes: &TranslucentPipes,
    maps: &mut TranslucentMaps,
    frame: &Frame<'_>,
) -> Recorded {
    let _span = tracing::info_span!("translucent_shadows").entered();
    let _crumb = crate::breadcrumbs::frame_pass("translucent_shadows");
    let mut out = Recorded::default();
    let Some((vp_buf, _)) = maps.vp.as_ref() else {
        return out;
    };
    let cascades = frame.cascades.len().min(crate::forward::CASCADE_COUNT as usize);
    let tiles = frame.tiles.len().min(MAX_SHADOW_TILES);
    let mut bytes = std::mem::take(&mut maps.vp_scratch);
    bytes.clear();
    bytes.resize(VP_STRIDE as usize * VP_SLOTS, 0);
    let mut put = |slot: usize, vp: &[[f32; 4]; 4]| {
        let at = slot * VP_STRIDE as usize;
        bytes[at..at + 64].copy_from_slice(bytemuck::cast_slice(vp));
    };
    for (i, vp) in frame.cascades.iter().take(cascades).enumerate() {
        put(i, vp);
    }
    for (i, tile) in frame.tiles.iter().take(tiles).enumerate() {
        put(crate::forward::CASCADE_COUNT as usize + i, &tile.view_proj);
    }
    let vp_buf = vp_buf.clone();
    belt.write_if_changed(d, encoder, &vp_buf, &bytes, &mut maps.vp_bytes);
    maps.vp_scratch = bytes;
    // The casters' footprint per cascade (padded), for the receiver's early-out.
    let pad = maps
        .csm
        .as_ref()
        .map_or(1.0, |csm| BOUNDS_PAD_TEXELS / csm.edge.max(1) as f32);
    let mut rects = [[1.0f32, 1.0, 0.0, 0.0]; crate::forward::CASCADE_COUNT as usize + 1];
    rects[crate::forward::CASCADE_COUNT as usize] = caster_sphere(frame.casters);
    for (k, vp) in frame.cascades.iter().take(cascades).enumerate() {
        let bit = 1u8 << k.min(7);
        for caster in frame.casters.iter().filter(|c| c.cascades & bit != 0) {
            let (min, max) = match caster.aabb {
                Some(aabb) => crate::paged_shadow::project_aabb_to_shadow_uv(aabb, *vp),
                None => ([0.0, 0.0], [1.0, 1.0]),
            };
            let r = &mut rects[k];
            r[0] = r[0].min(min[0] - pad);
            r[1] = r[1].min(min[1] - pad);
            r[2] = r[2].max(max[0] + pad);
            r[3] = r[3].max(max[1] + pad);
        }
    }
    if let Some(bounds) = maps.bounds.clone() {
        belt.write_if_changed(
            d,
            encoder,
            &bounds,
            bytemuck::cast_slice(&rects),
            &mut maps.bounds_bytes,
        );
    }
    let Some((_, vp_bg)) = maps.vp.as_ref() else {
        return out;
    };

    // Both sweeps of one map: nearest depth first, then the transmittance product.
    let draw_sweeps = |pass: &mut wgpu::RenderPass<'_>, list: &[&Caster<'_>], out: &mut Recorded| {
        if let Some(table) = frame.texture_table {
            pass.set_bind_group(crate::texture_table::TEXTURE_TABLE_GROUP, table, &[]);
        }
        for color in [false, true] {
            let mut bound: Option<(bool, bool, bool)> = None;
            for caster in list {
                let key = (caster.glass.is_some(), caster.double_sided, caster.own_textures);
                if bound != Some(key) {
                    let set = match (&pipes.own_textures, key) {
                        (Some(own), (_, _, true)) => own,
                        (_, (true, _, _)) => &pipes.glass,
                        _ => &pipes.plain,
                    };
                    let pipelines = if color { &set.color } else { &set.depth };
                    pass.set_pipeline(&pipelines[usize::from(key.1)]);
                    bound = Some(key);
                }
                pass.set_bind_group(1, caster.object, &[]);
                pass.set_bind_group(2, caster.material, caster.material_offsets);
                if let Some(glass) = caster.glass {
                    pass.set_bind_group(3, glass, &[]);
                }
                pass.set_vertex_buffer(0, caster.vb.slice(..));
                pass.set_index_buffer(caster.ib.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(
                    caster.first_index..caster.first_index + caster.index_count,
                    caster.base_vertex,
                    0..1,
                );
                out.draws += 1;
            }
        }
    };
    // One caster list, refilled per map (ADR-385).
    let mut list: Vec<&Caster<'_>> = Vec::with_capacity(frame.casters.len());
    if let Some(csm) = maps.csm.as_ref() {
        for (k, vp) in frame
            .cascades
            .iter()
            .take(cascades.min(csm.layers as usize))
            .enumerate()
        {
            let bit = 1u8 << k.min(7);
            list.clear();
            list.extend(
                frame
                    .casters
                    .iter()
                    .filter(|c| c.cascades & bit != 0 && c.aabb.as_ref().is_none_or(|b| may_overlap(b, vp))),
            );
            // An empty layer still clears: the receiver reads every active layer.
            let mut pass = begin(encoder, &csm.color_layers[k], &csm.depth_layers[k]);
            out.passes += 1;
            if list.is_empty() {
                continue;
            }
            pass.set_bind_group(0, vp_bg, &[(k as u64 * VP_STRIDE) as u32]);
            draw_sweeps(&mut pass, &list, &mut out);
        }
    }
    if tiles > 0
        && let Some(atlas) = maps.atlas.as_ref()
    {
        let mut pass = begin(encoder, &atlas.color_layers[0], &atlas.depth_layers[0]);
        out.passes += 1;
        let scale = atlas.edge as f32 / frame.atlas_size.max(1) as f32;
        for (i, tile) in frame.tiles.iter().take(tiles).enumerate() {
            list.clear();
            list.extend(
                frame
                    .casters
                    .iter()
                    .filter(|c| c.aabb.as_ref().is_none_or(|b| may_overlap(b, &tile.view_proj))),
            );
            if list.is_empty() {
                continue;
            }
            let [x, y, w, h] = tile.rect;
            let (fx, fy, fw, fh) = (
                x as f32 * scale,
                y as f32 * scale,
                w as f32 * scale,
                h as f32 * scale,
            );
            pass.set_viewport(fx, fy, fw, fh, 0.0, 1.0);
            let x0 = (fx.floor() as u32).min(atlas.edge - 1);
            let y0 = (fy.floor() as u32).min(atlas.edge - 1);
            let x1 = ((fx + fw).ceil() as u32).clamp(x0 + 1, atlas.edge);
            let y1 = ((fy + fh).ceil() as u32).clamp(y0 + 1, atlas.edge);
            pass.set_scissor_rect(x0, y0, x1 - x0, y1 - y0);
            let slot = crate::forward::CASCADE_COUNT as usize + i;
            pass.set_bind_group(0, vp_bg, &[(slot as u64 * VP_STRIDE) as u32]);
            draw_sweeps(&mut pass, &list, &mut out);
        }
    }
    out
}

/// A translucency map pass: colour cleared to white (nothing absorbed), depth to far.
fn begin<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    color: &wgpu::TextureView,
    depth: &wgpu::TextureView,
) -> wgpu::RenderPass<'e> {
    encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("translucent shadow"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: color,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: depth,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ortho_unit() -> [[f32; 4]; 4] {
        // x, y in [-1, 1], z in [0, 1] map to themselves (an identity clip volume for the test).
        regia_core::glam::Mat4::IDENTITY.to_cols_array_2d()
    }

    #[test]
    fn overlap_keeps_boxes_inside_and_crossing_the_volume() {
        let vp = ortho_unit();
        let inside = Aabb3 {
            min: [-0.2, -0.2, 0.2],
            max: [0.2, 0.2, 0.4],
        };
        let crossing = Aabb3 {
            min: [0.8, -0.2, 0.2],
            max: [1.6, 0.2, 0.4],
        };
        assert!(may_overlap(&inside, &vp));
        assert!(may_overlap(&crossing, &vp));
    }

    #[test]
    fn overlap_rejects_boxes_past_one_plane() {
        let vp = ortho_unit();
        for aabb in [
            Aabb3 {
                min: [1.1, -0.2, 0.2],
                max: [1.6, 0.2, 0.4],
            },
            Aabb3 {
                min: [-0.2, -3.0, 0.2],
                max: [0.2, -1.5, 0.4],
            },
            Aabb3 {
                min: [-0.2, -0.2, -2.0],
                max: [0.2, 0.2, -0.1],
            },
        ] {
            assert!(!may_overlap(&aabb, &vp), "{aabb:?}");
        }
    }

    #[test]
    fn map_edges_follow_the_opaque_maps() {
        assert_eq!(csm_edge(2048), 1024);
        assert_eq!(csm_edge(4096), 2048);
        assert_eq!(csm_edge(16), 16);
        assert_eq!(atlas_edge(8192), 2048);
        assert_eq!(atlas_edge(128), 64);
    }
}
