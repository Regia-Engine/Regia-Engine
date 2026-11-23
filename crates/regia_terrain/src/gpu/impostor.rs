// SPDX-License-Identifier: MPL-2.0
//! GPU side of the octahedral impostor far field (ADR-116).
//!
//! [`ImpostorLayer`] owns what the host keeps up to date: the baked capture atlas
//! ([`crate::ImpostorAtlas`], two `Rgba8` texture arrays), the per-prototype table and the
//! resident instances of a [`ScatterField`] ([`ImpostorLayer::update`] re-uploads them when
//! the field streams tiles in or out). [`crate::TerrainSharedDraw::set_impostors`] binds it:
//! each frame the terrain draw writes the camera parameters, culls the instances on the GPU
//! at the start of the frame (`impostor_cull.wgsl`: past the class fade start, inside the far
//! range, in the frustum; one indirect draw of six vertices per survivor) and draws the cards
//! in the shared depth, colour, velocity and decal-receiver passes right after the terrain
//! (`impostor.wgsl`). Impostors cast no sun shadow: the mesh keeps casting across the
//! hand-over band (its shadow shrinks out with the class's fade, as without impostors).
//!
//! Nothing here runs unless a layer is bound and enabled: an unbound draw records exactly the
//! commands it recorded before impostors existed.

use std::sync::{Arc, Mutex, PoisonError};

use regia_render::{SharedFrameInfo, SharedPass, SharedUpload};

use super::TerrainGpu;
use crate::scatter::field::ScatterField;
use crate::scatter::impostor::{ImpostorAtlas, ImpostorInstance, ImpostorProto, ImpostorSettings};

/// `ImpParams` in `impostor_common.wgsl` (160 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct ImpParams {
    planes: [[f32; 4]; 6],
    eye: [f32; 4],
    screen: [f32; 4],
    atlas: [f32; 4],
    counts: [u32; 4],
}

/// Composed WGSL of the impostor cull (common prelude + compute).
#[must_use]
pub fn cull_source() -> String {
    format!(
        "{}\n{}",
        include_str!("impostor_common.wgsl"),
        include_str!("impostor_cull.wgsl")
    )
}

/// Composed WGSL of the impostor draw: `terrain` (the terrain shader source the draw was
/// built with), the common prelude and the draw entry points.
#[must_use]
pub fn draw_source(terrain: &str) -> String {
    format!(
        "{terrain}\n{}\n{}",
        include_str!("impostor_common.wgsl"),
        include_str!("impostor.wgsl")
    )
}

/// What the layer shares with the draw it is bound to.
struct LayerState {
    albedo: wgpu::TextureView,
    normal_depth: wgpu::TextureView,
    sampler: wgpu::Sampler,
    protos: wgpu::Buffer,
    instances: wgpu::Buffer,
    visible: wgpu::Buffer,
    capacity: u32,
    /// Instance slots the cull walks (live and dead).
    walked: u32,
    /// Live instances.
    live: u32,
    enabled: bool,
    settings: ImpostorSettings,
    /// Bumped when a buffer is reallocated (bind groups must be rebuilt).
    buffers: u64,
    /// Bumped on every instance upload or toggle (the draw's content key).
    content: u64,
    texture_bytes: u64,
    /// The uploaded tiles' slot ranges and boxes, by offset (the outdoor-portal cull, ADR-8843).
    tiles: Vec<ImpostorTile>,
}

/// One uploaded tile: its slot range and a box holding every card it can draw.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ImpostorTile {
    offset: u32,
    len: u32,
    lo: [f32; 3],
    hi: [f32; 3],
}

/// How far the cards of a tile may sit above or below their unseated centres (the drawn surface's
/// morph and cliff relief), metres: the tile box's vertical pad for the outdoor-portal cull.
const TILE_SEAT_PAD_M: f32 = super::CLIFF_RELIEF_MAX_M + 1.0;

/// A box holding every card `instances` can draw: the cull shrinks an instance's sphere toward its
/// pivot (`imp_shrink`), so the box holds both spheres at 1.1 × radius (the cull's own test), plus
/// [`TILE_SEAT_PAD_M`] in height.
fn tile_box(instances: &[ImpostorInstance]) -> ([f32; 3], [f32; 3]) {
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for inst in instances {
        let r = inst.radius * 1.1;
        for p in [inst.centre, inst.pivot] {
            for axis in 0..3 {
                let pad = if axis == 1 { r + TILE_SEAT_PAD_M } else { r };
                lo[axis] = lo[axis].min(p[axis] - pad);
                hi[axis] = hi[axis].max(p[axis] + pad);
            }
        }
    }
    (lo, hi)
}

/// Sets bits `start..start + len` of `words`.
fn set_bits(words: &mut [u32], start: u32, len: u32) {
    let (mut i, end) = (start, start + len);
    while i < end {
        let word = (i / 32) as usize;
        let bit = i % 32;
        let take = (32 - bit).min(end - i);
        let mask = if take == 32 {
            u32::MAX
        } else {
            ((1u32 << take) - 1) << bit
        };
        if let Some(w) = words.get_mut(word) {
            *w |= mask;
        }
        i += take;
    }
}

/// The impostor far field's GPU resources (see the module docs). Create it once per field
/// with [`ImpostorLayer::new`], bind it with [`crate::TerrainSharedDraw::set_impostors`], and
/// call [`ImpostorLayer::update`] once per frame after [`ScatterField::update`].
pub struct ImpostorLayer {
    device: wgpu::Device,
    state: Arc<Mutex<LayerState>>,
    field_generation: Option<u64>,
    /// Painted store last uploaded: a painted edit changes tiles in place, under the same id.
    painted_generation: u64,
    scratch: Vec<ImpostorInstance>,
    scratch_ids: Vec<u64>,
    /// Slot range `(offset, len)` of each uploaded tile.
    slots: std::collections::HashMap<u64, (u32, u32)>,
    /// Box of each uploaded tile ([`tile_box`]).
    tile_boxes: std::collections::HashMap<u64, ([f32; 3], [f32; 3])>,
    /// Freed slot ranges (dead instances on the GPU), first fit.
    free: Vec<(u32, u32)>,
    /// Slots in use or freed (the cull walks `0..high`).
    high: u32,
    /// Full re-uploads (initial, growth, compaction).
    rebuilds: u64,
    /// Writes to the instance buffer so far ([`Self::instance_uploads`]).
    instance_uploads: u64,
    /// The prototype table as uploaded.
    protos: Vec<ImpostorProto>,
    /// The field's season generation the table holds.
    season_generation: Option<u64>,
    scratch_seasons: Vec<[f32; 4]>,
}

/// A slot the cull skips (`packed == 0xffffffff`).
const DEAD: ImpostorInstance = ImpostorInstance {
    centre: [0.0; 3],
    radius: 0.0,
    pivot: [0.0; 3],
    scale: 0.0,
    rotation: [0; 2],
    packed: u32::MAX,
    leaf_rank: 0,
};

fn storage_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(64),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

impl ImpostorLayer {
    /// Uploads `atlas` (cards and prototype table). No instances until [`Self::update`].
    #[must_use]
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, atlas: &ImpostorAtlas) -> Self {
        let _span = tracing::info_span!("impostor_layer_new", cards = atlas.cards.len()).entered();
        let settings = atlas.settings;
        let size = atlas.size().max(1);
        let mips = settings.mip_count();
        let layers = (atlas.cards.len() as u32).max(1);
        let make = |label: &str,
                    format: wgpu::TextureFormat,
                    pick: &dyn Fn(&crate::ImpostorCard, usize) -> &[u8]| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: layers,
                },
                mip_level_count: mips,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            for (layer, card) in atlas.cards.iter().enumerate() {
                for mip in 0..mips as usize {
                    let side = (size >> mip).max(1);
                    let data = pick(card, mip);
                    if data.len() != (side * side * 4) as usize {
                        continue;
                    }
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &texture,
                            mip_level: mip as u32,
                            origin: wgpu::Origin3d {
                                x: 0,
                                y: 0,
                                z: layer as u32,
                            },
                            aspect: wgpu::TextureAspect::All,
                        },
                        data,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(side * 4),
                            rows_per_image: Some(side),
                        },
                        wgpu::Extent3d {
                            width: side,
                            height: side,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
            texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some(label),
                dimension: Some(wgpu::TextureViewDimension::D2Array),
                ..Default::default()
            })
        };
        let albedo = make(
            "impostor albedo",
            wgpu::TextureFormat::Rgba8UnormSrgb,
            &|card, mip| card.albedo.get(mip).map_or(&[][..], Vec::as_slice),
        );
        let normal_depth = make(
            "impostor normal-depth",
            wgpu::TextureFormat::Rgba8Unorm,
            &|card, mip| card.normal_depth.get(mip).map_or(&[][..], Vec::as_slice),
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("impostor sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let proto_bytes: &[u8] = bytemuck::cast_slice(&atlas.protos);
        let protos = storage_buffer(device, "impostor protos", proto_bytes.len() as u64);
        if !proto_bytes.is_empty() {
            queue.write_buffer(&protos, 0, proto_bytes);
        }
        let capacity = 1024u32;
        let instances = storage_buffer(
            device,
            "impostor instances",
            u64::from(capacity) * size_of::<ImpostorInstance>() as u64,
        );
        let visible = storage_buffer(device, "impostor visible", u64::from(capacity) * 4);
        Self {
            device: device.clone(),
            state: Arc::new(Mutex::new(LayerState {
                albedo,
                normal_depth,
                sampler,
                protos,
                instances,
                visible,
                capacity,
                walked: 0,
                live: 0,
                enabled: true,
                settings,
                buffers: 0,
                content: 0,
                texture_bytes: atlas.bytes() as u64,
                tiles: Vec::new(),
            })),
            field_generation: None,
            painted_generation: 0,
            scratch: Vec::new(),
            scratch_ids: Vec::new(),
            slots: std::collections::HashMap::new(),
            tile_boxes: std::collections::HashMap::new(),
            free: Vec::new(),
            high: 0,
            rebuilds: 0,
            instance_uploads: 0,
            protos: atlas.protos.clone(),
            season_generation: None,
            scratch_seasons: Vec::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LayerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Uploads what changed in the field's impostor set since the last call
    /// ([`ScatterField::impostor_generation`]): each tile streamed in is written to free slots,
    /// each tile evicted is marked dead, so a moving camera uploads a few tiles at a time. A
    /// full re-upload happens on growth or when dead slots outnumber live ones. Returns the
    /// live instance count.
    #[tracing::instrument(skip_all, target = "regia_terrain")]
    pub fn update(&mut self, queue: &wgpu::Queue, field: &ScatterField) -> u32 {
        self.update_seasons(queue, field);
        let generation = field.impostor_generation();
        if self.field_generation == Some(generation) {
            return self.lock().live;
        }
        self.field_generation = Some(generation);
        let mut ids = std::mem::take(&mut self.scratch_ids);
        field.impostor_tiles(&mut ids);
        let current: std::collections::HashSet<u64> = ids.iter().copied().collect();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut live = state.live;
        let mut changed = false;
        // Evicted tiles: their slots die.
        let gone: Vec<u64> = self
            .slots
            .keys()
            .filter(|id| !current.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.tile_boxes.remove(&id);
            if let Some((offset, len)) = self.slots.remove(&id)
                && len > 0
            {
                let dead = vec![DEAD; len as usize];
                queue.write_buffer(
                    &state.instances,
                    u64::from(offset) * size_of::<ImpostorInstance>() as u64,
                    bytemuck::cast_slice(&dead),
                );
                self.free.push((offset, len));
                self.instance_uploads += 1;
                live -= len;
                changed = true;
            }
        }
        // New tiles: first fit in the freed ranges, else past the high-water mark. A painted edit
        // re-uploads everything: it happens at edit time, and tile ids do not show what it changed.
        let painted = field.painted_generation();
        let mut rebuild = painted != self.painted_generation;
        self.painted_generation = painted;
        for &id in &ids {
            if rebuild {
                break;
            }
            if self.slots.contains_key(&id) {
                continue;
            }
            self.scratch.clear();
            field.impostor_tile_instances(id, &mut self.scratch);
            let len = self.scratch.len() as u32;
            changed = true;
            if len == 0 {
                self.slots.insert(id, (0, 0));
                continue;
            }
            let offset = match self.free.iter().position(|&(_, n)| n >= len) {
                Some(i) => {
                    let (offset, n) = self.free[i];
                    if n == len {
                        self.free.swap_remove(i);
                    } else {
                        self.free[i] = (offset + len, n - len);
                    }
                    offset
                }
                None => {
                    if self.high + len > state.capacity {
                        rebuild = true;
                        break;
                    }
                    self.high += len;
                    self.high - len
                }
            };
            queue.write_buffer(
                &state.instances,
                u64::from(offset) * size_of::<ImpostorInstance>() as u64,
                bytemuck::cast_slice(&self.scratch),
            );
            self.tile_boxes.insert(id, tile_box(&self.scratch));
            self.slots.insert(id, (offset, len));
            self.instance_uploads += 1;
            live += len;
        }
        if rebuild || self.high > 2 * live + 4096 {
            // Compact: every tile again, contiguous, in emission order.
            self.slots.clear();
            self.tile_boxes.clear();
            self.free.clear();
            let mut all = std::mem::take(&mut self.scratch);
            all.clear();
            for &id in &ids {
                let start = all.len() as u32;
                field.impostor_tile_instances(id, &mut all);
                self.slots.insert(id, (start, all.len() as u32 - start));
                self.tile_boxes.insert(id, tile_box(&all[start as usize..]));
            }
            let total = all.len() as u32;
            if total > state.capacity {
                let capacity = total.next_power_of_two();
                state.instances = storage_buffer(
                    &self.device,
                    "impostor instances",
                    u64::from(capacity) * size_of::<ImpostorInstance>() as u64,
                );
                state.visible = storage_buffer(&self.device, "impostor visible", u64::from(capacity) * 4);
                state.capacity = capacity;
                state.buffers += 1;
            }
            if total > 0 {
                queue.write_buffer(&state.instances, 0, bytemuck::cast_slice(&all));
                self.instance_uploads += 1;
            }
            self.high = total;
            live = total;
            self.scratch = all;
            self.rebuilds += 1;
            changed = true;
        }
        state.walked = self.high;
        state.live = live;
        if changed {
            state.content += 1;
            state.tiles.clear();
            for (id, &(offset, len)) in &self.slots {
                if let (true, Some(&(lo, hi))) = (len > 0, self.tile_boxes.get(id)) {
                    state.tiles.push(ImpostorTile { offset, len, lo, hi });
                }
            }
            state.tiles.sort_unstable_by_key(|t| t.offset);
        }
        drop(state);
        self.scratch_ids = ids;
        live
    }

    /// A season change rewrites the prototype table and nothing per instance (ADR-3146): every instance
    /// holds its leaf rank from its upload on.
    fn update_seasons(&mut self, queue: &wgpu::Queue, field: &ScatterField) {
        let generation = field.season_generation();
        if self.season_generation == Some(generation) {
            return;
        }
        self.season_generation = Some(generation);
        field.impostor_seasons(&mut self.scratch_seasons);
        let mut changed = false;
        for (proto, season) in self.protos.iter_mut().zip(&self.scratch_seasons) {
            changed |= proto.season != *season;
            proto.season = *season;
        }
        if changed {
            let mut state = self.lock();
            queue.write_buffer(&state.protos, 0, bytemuck::cast_slice(&self.protos));
            state.content += 1;
        }
    }

    /// Writes to the instance buffer so far: streaming and compaction, never a season change.
    #[must_use]
    pub fn instance_uploads(&self) -> u64 {
        self.instance_uploads
    }

    /// Full re-uploads so far (the first upload, buffer growth, compaction).
    #[must_use]
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// Turns the layer's draws on or off (A/B shots, agent `terrain.set_impostors`) without
    /// unbinding it. Off, the draw records nothing for impostors.
    pub fn set_enabled(&mut self, enabled: bool) {
        let mut state = self.lock();
        if state.enabled != enabled {
            state.enabled = enabled;
            state.content += 1;
        }
    }

    /// Whether the layer draws.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    /// Live instances resident on the GPU (before the per-frame cull).
    #[must_use]
    pub fn count(&self) -> u32 {
        self.lock().live
    }

    /// GPU memory of the atlas and the instance buffers, bytes.
    #[must_use]
    pub fn gpu_bytes(&self) -> u64 {
        let state = self.lock();
        state.texture_bytes + u64::from(state.capacity) * (size_of::<ImpostorInstance>() as u64 + 4)
    }

    /// The settings the atlas was baked with.
    #[must_use]
    pub fn settings(&self) -> ImpostorSettings {
        self.lock().settings
    }
}

/// The four card pipelines for one colour format.
struct ImpostorPipelines {
    format: wgpu::TextureFormat,
    depth: wgpu::RenderPipeline,
    color: wgpu::RenderPipeline,
    velocity: wgpu::RenderPipeline,
    receiver: wgpu::RenderPipeline,
}

/// The draw-side state of a bound [`ImpostorLayer`] (owned by the terrain draw).
pub(crate) struct ImpostorDraw {
    state: Arc<Mutex<LayerState>>,
    params: wgpu::Buffer,
    args: wgpu::Buffer,
    cull_layout: wgpu::BindGroupLayout,
    cull_pipeline: wgpu::ComputePipeline,
    draw_layout: wgpu::BindGroupLayout,
    pipelines: Option<ImpostorPipelines>,
    /// `(cull, draw)` bind groups for the layer's buffers generation `.2`.
    binds: Option<(wgpu::BindGroup, wgpu::BindGroup, u64)>,
    /// Instances to cull this frame (0 = nothing recorded).
    count: u32,
    content: u64,
    /// One bit per instance slot: the slot's tile is admitted by the outdoor portals (ADR-8843).
    /// Read by the cull only on a restricted frame.
    tile_mask: wgpu::Buffer,
    /// Words `tile_mask` holds.
    mask_words: u32,
    /// The mask as last built, and its hash as last uploaded.
    mask_cpu: Vec<u32>,
    mask_key: Option<u64>,
    /// Live instances in admitted and in culled tiles on the last prepare.
    portal_kept: u32,
    portal_culled: u32,
}

impl ImpostorDraw {
    pub(crate) fn new(gpu: &TerrainGpu, layer: &ImpostorLayer) -> Self {
        let d = &gpu.device;
        let storage =
            |binding: u32, read_only: bool, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            };
        let uniform = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let cs = wgpu::ShaderStages::COMPUTE;
        let cull_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("impostor cull layout"),
            entries: &[
                uniform(0, cs),
                storage(1, true, cs),
                storage(2, true, cs),
                storage(3, false, cs),
                storage(4, false, cs),
                storage(5, true, cs),
            ],
        });
        let vf = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let texture = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        };
        let draw_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("impostor draw layout"),
            entries: &[
                uniform(8, vf),
                storage(9, true, vf),
                storage(10, true, vf),
                storage(11, true, wgpu::ShaderStages::VERTEX),
                texture(12),
                texture(13),
                wgpu::BindGroupLayoutEntry {
                    binding: 14,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("impostor_cull.wgsl"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Owned(cull_source())),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("impostor cull"),
            bind_group_layouts: &[Some(&cull_layout)],
            immediate_size: 0,
        });
        let cull_pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("impostor cull"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("cs_cull"),
            compilation_options: Default::default(),
            cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("impostor params"),
            size: size_of::<ImpParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let args = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("impostor indirect args"),
            size: 16,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            state: Arc::clone(&layer.state),
            params,
            args,
            cull_layout,
            cull_pipeline,
            draw_layout,
            pipelines: None,
            binds: None,
            count: 0,
            content: 0,
            tile_mask: storage_buffer(d, "impostor tile mask", 4),
            mask_words: 1,
            mask_cpu: Vec::new(),
            mask_key: None,
            portal_kept: 0,
            portal_culled: 0,
        }
    }

    /// Builds the card pipelines for `format` (load time with `prewarm`, else first frame).
    pub(crate) fn ensure_pipelines(&mut self, gpu: &TerrainGpu, format: wgpu::TextureFormat) {
        if self.pipelines.as_ref().is_some_and(|p| p.format == format) {
            return;
        }
        let _span = tracing::info_span!("impostor_pipelines").entered();
        let d = &gpu.device;
        let terrain = if gpu.height_sampler.is_some() {
            super::hw_height_source()
        } else {
            super::without_surface_weather(include_str!("terrain.wgsl"))
        };
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("impostor.wgsl"),
            // The terrain source carries the virtual texture's `@early_depth_test(force)`: a device
            // without `SHADER_EARLY_DEPTH_TEST` (Metal) rejects it.
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Owned(super::device_source(
                d,
                draw_source(&terrain),
            ))),
        });
        let layout = |label: &str, color: bool| {
            let mut groups = vec![
                Some(&gpu.frame_layout),
                Some(&gpu.terrain_layout),
                Some(&self.draw_layout),
            ];
            if color {
                groups.push(Some(&gpu.engine_csm_layout));
            }
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &groups,
                immediate_size: 0,
            })
        };
        let plain_layout = layout("impostor draw", false);
        let color_layout = layout("impostor color", true);
        let primitive = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        };
        let depth_state = |write: bool| wgpu::DepthStencilState {
            format: gpu.depth_format,
            depth_write_enabled: Some(write),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        };
        let pipeline = |label: &str,
                        layout: &wgpu::PipelineLayout,
                        entry: &str,
                        target: Option<wgpu::TextureFormat>,
                        write: bool| {
            let targets = [target.map(|format| wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })];
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_impostor"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: if target.is_some() { &targets } else { &[] },
                }),
                primitive,
                depth_stencil: Some(depth_state(write)),
                multisample: Default::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
            })
        };
        self.pipelines = Some(ImpostorPipelines {
            format,
            depth: pipeline("impostor depth", &plain_layout, "fs_impostor_depth", None, true),
            color: pipeline(
                "impostor color",
                &color_layout,
                "fs_impostor_color",
                Some(format),
                true,
            ),
            velocity: pipeline(
                "impostor velocity",
                &plain_layout,
                "fs_impostor_velocity",
                Some(wgpu::TextureFormat::Rgba16Float),
                false,
            ),
            receiver: pipeline(
                "impostor receiver",
                &plain_layout,
                "fs_impostor_receiver",
                Some(wgpu::TextureFormat::Rgba8Unorm),
                false,
            ),
        });
    }

    /// Writes this frame's parameters and resets the indirect draw; rebinds after a
    /// layer buffer reallocation. Records nothing when the layer is off or empty.
    ///
    /// `portals` is the outdoor list of a main view inside a cell, in world space, when it
    /// restricts this frame's eye (ADR-8843): the cull then skips the slots of every tile whose box
    /// no frustum admits ([`Self::portal_counts`]).
    pub(crate) fn prepare(
        &mut self,
        gpu: &TerrainGpu,
        info: &SharedFrameInfo,
        upload: &mut SharedUpload<'_>,
        portals: Option<&regia_render::viscells::OutdoorPortals>,
    ) {
        let _span = tracing::trace_span!("impostor_prepare").entered();
        self.ensure_pipelines(gpu, info.color_format);
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.content = state.content;
        self.count = if state.enabled { state.walked } else { 0 };
        self.portal_kept = if state.enabled { state.live } else { 0 };
        self.portal_culled = 0;
        if self.count == 0 {
            return;
        }
        let words = state.capacity.div_ceil(32).max(1);
        if words > self.mask_words {
            self.tile_mask = storage_buffer(&gpu.device, "impostor tile mask", u64::from(words) * 4);
            self.mask_words = words;
            self.mask_key = None;
            self.binds = None;
        }
        if let Some(portals) = portals {
            self.mask_cpu.clear();
            self.mask_cpu.resize(words as usize, 0);
            let (mut kept, mut culled) = (0u32, 0u32);
            for tile in &state.tiles {
                if portals.admits_box(tile.lo, tile.hi, regia_render::viscells::OUTDOOR_NODE_MARGIN_M) {
                    set_bits(&mut self.mask_cpu, tile.offset, tile.len);
                    kept += tile.len;
                } else {
                    culled += tile.len;
                }
            }
            self.portal_kept = kept;
            self.portal_culled = culled;
            let key = self.mask_cpu.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, w| {
                (h ^ u64::from(*w)).wrapping_mul(0x0100_0000_01b3)
            });
            if self.mask_key != Some(key) {
                upload.write(&self.tile_mask, 0, bytemuck::cast_slice(&self.mask_cpu));
                self.mask_key = Some(key);
            }
        }
        if self.binds.as_ref().is_none_or(|b| b.2 != state.buffers) {
            let d = &gpu.device;
            fn buf(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
                wgpu::BindGroupEntry {
                    binding,
                    resource: buffer.as_entire_binding(),
                }
            }
            let cull = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("impostor cull"),
                layout: &self.cull_layout,
                entries: &[
                    buf(0, &self.params),
                    buf(1, &state.instances),
                    buf(2, &state.protos),
                    buf(3, &state.visible),
                    buf(4, &self.args),
                    buf(5, &self.tile_mask),
                ],
            });
            let draw = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("impostor draw"),
                layout: &self.draw_layout,
                entries: &[
                    buf(8, &self.params),
                    buf(9, &state.instances),
                    buf(10, &state.protos),
                    buf(11, &state.visible),
                    wgpu::BindGroupEntry {
                        binding: 12,
                        resource: wgpu::BindingResource::TextureView(&state.albedo),
                    },
                    wgpu::BindGroupEntry {
                        binding: 13,
                        resource: wgpu::BindingResource::TextureView(&state.normal_depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 14,
                        resource: wgpu::BindingResource::Sampler(&state.sampler),
                    },
                ],
            });
            self.binds = Some((cull, draw, state.buffers));
        }
        let settings = state.settings;
        drop(state);
        let vp = regia_core::glam::Mat4::from_cols_array_2d(&info.unjittered_view_proj);
        let frustum = regia_render::Frustum::from_view_proj(&vp);
        let height = info.height.max(1) as f32;
        let tan_half = (info.fov_y * 0.5).tan().max(1e-4);
        // The test behind the forward pass's held-view flag (`interior_half.w` bit 5): the two sides
        // of the hand-over must take the same phase (ADR-3146).
        let held = u32::from(info.prev_view_proj == info.unjittered_view_proj);
        let params = ImpParams {
            planes: frustum.planes(),
            eye: [info.eye[0], info.eye[1], info.eye[2], 0.0],
            screen: [height, 1.0 / tan_half, settings.min_pixels, info.noise_index],
            atlas: [
                settings.grid as f32,
                settings.cell as f32,
                settings.mip_count() as f32,
                2.0 * tan_half / height,
            ],
            counts: [self.count, held, u32::from(portals.is_some()), 0],
        };
        upload.write(&self.params, 0, bytemuck::bytes_of(&params));
        upload.write(&self.args, 0, bytemuck::cast_slice(&[0u32, 1, 0, 0]));
    }

    /// Records the cull (start of the frame, after the uploads).
    pub(crate) fn encode(&self, encoder: &mut wgpu::CommandEncoder) {
        let Some((cull, _, _)) = self.binds.as_ref().filter(|_| self.count > 0) else {
            return;
        };
        let _span = tracing::trace_span!("impostor_cull").entered();
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("impostor cull"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.cull_pipeline);
        pass.set_bind_group(0, cull, &[]);
        pass.dispatch_workgroups(self.count.div_ceil(64), 1, 1);
    }

    /// Draws the cards into a shared pass (nothing into the sun cascades).
    pub(crate) fn draw<'a>(&'a self, gpu: &'a TerrainGpu, pass: &mut wgpu::RenderPass<'a>, kind: SharedPass) {
        let (Some(pipelines), Some((_, bind, _))) = (self.pipelines.as_ref(), self.binds.as_ref()) else {
            return;
        };
        if self.count == 0 {
            return;
        }
        let pipeline = match kind {
            SharedPass::Depth => &pipelines.depth,
            SharedPass::Color => &pipelines.color,
            SharedPass::Velocity => &pipelines.velocity,
            SharedPass::Receiver => &pipelines.receiver,
            SharedPass::Shadow { .. } => return,
        };
        let _span = tracing::trace_span!("impostor_draw", ?kind).entered();
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &gpu.frame_bind, &[]);
        pass.set_bind_group(1, &gpu.terrain_bind, &[]);
        pass.set_bind_group(2, bind, &[]);
        if kind == SharedPass::Color {
            pass.set_bind_group(3, &gpu.engine_csm_bind, &[]);
        }
        pass.draw_indirect(&self.args, 0);
    }

    /// Live instances in the tiles the outdoor portals admitted and culled on the last prepare
    /// (ADR-8843): `(kept, culled)`; every live instance is kept on an unrestricted frame.
    pub(crate) fn portal_counts(&self) -> (u32, u32) {
        (self.portal_kept, self.portal_culled)
    }

    /// Changes whenever what the impostors draw may change without the camera moving.
    pub(crate) fn content_key(&self) -> u64 {
        self.content.rotate_left(17) ^ u64::from(self.count)
    }
}

#[cfg(test)]
mod portal_mask_tests {
    use super::*;

    #[test]
    fn set_bits_covers_exactly_the_range_across_words() {
        let mut words = [0u32; 4];
        set_bits(&mut words, 30, 40);
        let set: Vec<u32> = (0..128)
            .filter(|i| (words[(i / 32) as usize] >> (i % 32)) & 1 == 1)
            .collect();
        assert_eq!(set, (30..70).collect::<Vec<_>>());
        let mut full = [0u32; 2];
        set_bits(&mut full, 0, 64);
        assert_eq!(full, [u32::MAX; 2]);
        // A range past the end is clipped, never a panic.
        let mut short = [0u32; 1];
        set_bits(&mut short, 16, 64);
        assert_eq!(short, [0xffff_0000]);
    }

    #[test]
    fn a_tile_box_holds_both_spheres_and_the_seat_pad() {
        let inst = ImpostorInstance {
            centre: [10.0, 5.0, 0.0],
            radius: 2.0,
            pivot: [10.0, 1.0, 0.0],
            ..DEAD
        };
        let (lo, hi) = tile_box(&[inst]);
        assert_eq!(lo[0], 10.0 - 2.2);
        assert_eq!(hi[0], 10.0 + 2.2);
        assert!(lo[1] <= 1.0 - 2.2 - TILE_SEAT_PAD_M && hi[1] >= 5.0 + 2.2 + TILE_SEAT_PAD_M);
    }
}
