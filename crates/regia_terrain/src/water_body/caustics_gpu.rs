// SPDX-License-Identifier: MPL-2.0
//! The GPU side of the caustics (ADR-2432): the tile pass, the half-resolution shaft pass and the
//! bind group (group 1) the water fragment stage reads them through.
//!
//! Nothing here exists until a body asks: with every body's caustics off (the default) the group
//! binds a 1 x 1 stand-in, no compute pipeline is built and no pass is recorded, so default frames
//! are untouched. Low compiles the pass out ([`super::WaterCaustics::active_at`]).
//!
//! ADR-3251 adds, each opt in: the frame's sun shadow in the march ([`WaterSunShadow`], bound by
//! the host), a temporal resolve of the half-resolution march (two history targets, ping-pong)
//! and the march seen from above the surface.

use bytemuck::{Pod, Zeroable};
use serde::Serialize;

use super::caustics::{CAUSTIC_TILE_SIZE, TileParams};

/// The WGSL entry point of the tile pass.
pub const TILE_ENTRY: &str = "cs_caustic_tile";

/// The WGSL entry point of the shaft pass.
pub const SHAFT_ENTRY: &str = "cs_caustic_shafts";

/// The WGSL entry point of the shafts' temporal resolve (ADR-3251).
pub const RESOLVE_ENTRY: &str = "cs_shaft_resolve";

/// The most frames the shaft history averages (ADR-3251): 1 / n blending up to this many, then
/// an exponential average with weight `1 / SHAFT_HISTORY_FRAMES`.
pub const SHAFT_HISTORY_FRAMES: u32 = 16;

/// Depth bias of the sun shadow test in the shaft march, in the map's depth units, when the host
/// gives none ([`WaterSunShadow::bias`] 0).
pub const SHAFT_SHADOW_BIAS: f32 = 1.0e-3;

/// The frame's sun shadow, as the shaft march reads it (ADR-3251): one orthographic cascade.
///
/// `view` is a single-sample 2D depth (`Depth32Float`) or `R32Float` view; the march reads it with
/// `textureLoad` as float depth, a point is shadowed when its depth in `light_view_proj`'s clip
/// space (0 at the light, 1 far) is beyond the stored depth plus `bias`. The engine frame's
/// cascades follow that convention.
#[derive(Debug, Clone)]
pub struct WaterSunShadow {
    /// The shadow map.
    pub view: wgpu::TextureView,
    /// World to the map's clip space (column-major).
    pub light_view_proj: [[f32; 4]; 4],
    /// Depth bias (0: [`SHAFT_SHADOW_BIAS`]).
    pub bias: f32,
    /// Fade the shadow out over the outer 15 % of the map, as the volumetric fog does for a
    /// cascade that is not centred on the view.
    pub edge_fade: bool,
}

/// What the caustic passes did (agent `water.caustics_stats`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct CausticStats {
    /// Bodies whose caustics are on at the frame's tier.
    pub bodies: u32,
    /// Texels per side of the tile (0 until a body asked for it).
    pub tile_size: u32,
    /// Tile passes recorded since the renderer was created.
    pub tile_passes: u64,
    /// Shaft passes recorded since the renderer was created.
    pub shaft_passes: u64,
    /// Texels of the shaft target (0 until the eye was under a body with shafts).
    pub shaft_texels: u64,
    /// Steps of the last shaft march (0: none ran).
    pub shaft_steps: u32,
    /// `true` when the last frame was under a surface and drew shafts.
    pub shafts_drawn: bool,
    /// `true` when the last shaft march read a sun shadow map (ADR-3251).
    pub shadow_bound: bool,
    /// `true` when the last frame drew shafts with the eye above the surface (ADR-3251).
    pub from_above: bool,
    /// Temporal resolves of the shafts since the renderer was created (ADR-3251).
    pub resolves: u64,
    /// Restarts of the shaft history: a camera cut, a new target, the eye crossing the surface
    /// or the accumulation switched on (ADR-3251).
    pub history_resets: u64,
    /// Frames the shaft history holds since its last restart, capped at
    /// [`SHAFT_HISTORY_FRAMES`] (0: no history).
    pub history_frames: u32,
}

/// The shaft passes' uniform (`ShaftParams` in `water_body.wgsl`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct ShaftUniform {
    light_vp: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    shadow: [f32; 4],
    march: [f32; 4],
    history: [f32; 4],
}

/// The Rust and WGSL struct row of the shaft uniform (`shader_contract`, ADR-3002; ADR-5960 (11)).
pub(crate) fn contract_rows() -> Vec<regia_render::ContractRow> {
    vec![regia_render::contract_row!(
        "water_body.wgsl",
        "ShaftParams",
        ShaftUniform {
            light_vp,
            prev_view_proj,
            shadow,
            march,
            history
        }
    )]
}

/// What the frame asks of the shaft pass (ADR-3251).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ShaftFrame {
    /// The viewport (pixels).
    pub viewport: [u32; 2],
    /// March steps.
    pub steps: u32,
    /// Average the march over frames.
    pub accumulate: bool,
    /// `(slot, level)` of the body seen from above; `None` with the eye under a surface.
    pub from_above: Option<(u32, f32)>,
    /// The frame's un-jittered view-projection, for the next frame's reprojection.
    pub view_proj: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct TileUniform {
    sun: [f32; 4],
    scroll: [f32; 4],
}

struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: [u32; 2],
}

/// Pipelines, layouts and textures of the caustic passes.
pub(super) struct CausticsGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    module: wgpu::ShaderModule,
    /// Group 0 of the compute passes: frame, bodies, depth and the ripple sampler.
    layout_g0: wgpu::BindGroupLayout,
    /// Group 1 of the shaft pass: the tile.
    layout_g1_compute: wgpu::BindGroupLayout,
    /// Group 1 of the water draw: the tile and the shafts.
    pub(super) layout_g1: wgpu::BindGroupLayout,
    layout_tile: wgpu::BindGroupLayout,
    layout_shaft: wgpu::BindGroupLayout,
    layout_resolve: wgpu::BindGroupLayout,
    tile_pipeline: Option<wgpu::ComputePipeline>,
    shaft_pipeline: Option<wgpu::ComputePipeline>,
    resolve_pipeline: Option<wgpu::ComputePipeline>,
    tile_uniform: wgpu::Buffer,
    /// `ShaftParams` (binding 4 of the passes, binding 2 of the draw's group 1).
    shaft_uniform: wgpu::Buffer,
    stand_in: wgpu::TextureView,
    /// A 1 x 1 `R32Float` at depth 1 (nothing casts) bound while the host gave no shadow.
    shadow_stand_in: wgpu::TextureView,
    /// The host's sun shadow ([`super::WaterRenderer::set_sun_shadow`]).
    pub(super) sun_shadow: Option<WaterSunShadow>,
    tile: Option<Target>,
    shaft: Option<Target>,
    /// The two accumulation targets (ping-pong) of the resolve.
    history: Option<[Target; 2]>,
    /// The history target the last resolve wrote, when the draw reads the accumulation.
    history_out: Option<usize>,
    /// Whether the history holds a frame the next resolve may reproject.
    history_valid: bool,
    /// What the last shaft frame was: `(from_above slot, accumulate)`; a change restarts the
    /// history.
    last_shaft: Option<(Option<u32>, bool)>,
    /// The last shaft frame's view-projection.
    prev_view_proj: Option<[[f32; 4]; 4]>,
    /// Shaft frames run with accumulation on (drives the per-frame jitter).
    jitter_frame: u32,
    /// Whether the uniform last written says "from above" (the draw reads it).
    above_written: bool,
    /// The draw's group 1 per shaft source: 0 the raw march, 1 and 2 the history targets.
    group1: [Option<wgpu::BindGroup>; 3],
    pub(super) stats: CausticStats,
}

fn texture_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn storage_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba16Float,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

fn unfilterable_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// The march's per-frame jitter offset: the golden-ratio sequence, so 16 frames cover the step
/// evenly (0 for the first frame, the fixed pattern).
#[must_use]
pub fn shaft_jitter_offset(frame: u32) -> f32 {
    let golden = 0.618_034_f64;
    (f64::from(frame % 4096) * golden).fract() as f32
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

impl CausticsGpu {
    pub(super) fn new(device: &wgpu::Device, queue: &wgpu::Queue, module: wgpu::ShaderModule) -> Self {
        let cs = wgpu::ShaderStages::COMPUTE;
        let layout = |label: &'static str, entries: &[wgpu::BindGroupLayoutEntry]| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some(label),
                entries,
            })
        };
        let layout_g0 = layout(
            "water caustics compute frame",
            &[
                uniform_entry(0, cs),
                uniform_entry(1, cs),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: cs,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: cs,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        );
        let layout_g1_compute = layout("water caustics tile (compute)", &[texture_entry(0, cs)]);
        let layout_g1 = layout(
            "water caustics tile (draw)",
            &[
                texture_entry(0, wgpu::ShaderStages::FRAGMENT | cs),
                texture_entry(1, wgpu::ShaderStages::FRAGMENT),
                uniform_entry(2, wgpu::ShaderStages::FRAGMENT),
            ],
        );
        let layout_tile = layout(
            "water caustics tile pass",
            &[uniform_entry(0, cs), storage_entry(1), texture_entry(2, cs)],
        );
        let layout_shaft = layout(
            "water caustics shaft pass",
            &[storage_entry(3), uniform_entry(4, cs), unfilterable_entry(5)],
        );
        let layout_resolve = layout(
            "water shaft resolve",
            &[
                uniform_entry(4, cs),
                unfilterable_entry(6),
                unfilterable_entry(7),
                storage_entry(8),
            ],
        );
        let tile_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water caustics tile params"),
            size: size_of::<TileUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Zero: no shadow, the fixed jitter, the eye under the surface (the draw adds nothing).
        let shaft_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water shaft params"),
            size: size_of::<ShaftUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shadow_stand_in = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("water shafts shadow stand-in"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            shadow_stand_in.as_image_copy(),
            &1.0f32.to_le_bytes(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let stand_in = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("water caustics stand-in"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            stand_in.as_image_copy(),
            &[0u8; 8],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(8),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        Self {
            device: device.clone(),
            queue: queue.clone(),
            module,
            layout_g0,
            layout_g1_compute,
            layout_g1,
            layout_tile,
            layout_shaft,
            layout_resolve,
            tile_pipeline: None,
            shaft_pipeline: None,
            resolve_pipeline: None,
            tile_uniform,
            shaft_uniform,
            stand_in: stand_in.create_view(&wgpu::TextureViewDescriptor::default()),
            shadow_stand_in: shadow_stand_in.create_view(&wgpu::TextureViewDescriptor::default()),
            sun_shadow: None,
            tile: None,
            shaft: None,
            history: None,
            history_out: None,
            history_valid: false,
            last_shaft: None,
            prev_view_proj: None,
            jitter_frame: 0,
            above_written: false,
            group1: [None, None, None],
            stats: CausticStats::default(),
        }
    }

    fn target(&self, label: &'static str, size: [u32; 2]) -> Target {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Target { texture, view, size }
    }

    fn compute(
        &self,
        label: &'static str,
        entry: &'static str,
        groups: &[Option<&wgpu::BindGroupLayout>],
    ) -> Option<wgpu::ComputePipeline> {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: groups,
                immediate_size: 0,
            });
        let pipeline = self
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                module: &self.module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
            });
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            tracing::error!(target: "regia_terrain", "{label}: {err}");
            return None;
        }
        Some(pipeline)
    }

    /// The draw's group 1: the tile, the shafts (the accumulation when the last resolve wrote
    /// one, else the raw march) and the shaft parameters, or the stand-in for what does not exist
    /// yet.
    pub(super) fn draw_group(&mut self) -> wgpu::BindGroup {
        let source = self.history_out.map_or(0, |i| i + 1);
        if let Some(group) = &self.group1[source] {
            return group.clone();
        }
        let tile = self.tile.as_ref().map_or(&self.stand_in, |t| &t.view);
        let shaft = match (self.history_out, &self.history) {
            (Some(i), Some(history)) => &history[i].view,
            _ => self.shaft.as_ref().map_or(&self.stand_in, |t| &t.view),
        };
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water caustics draw bind"),
            layout: &self.layout_g1,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(tile),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(shaft),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.shaft_uniform.as_entire_binding(),
                },
            ],
        });
        self.group1[source] = Some(group.clone());
        group
    }

    /// Forgets every cached draw group (a target was created or replaced).
    fn drop_groups(&mut self) {
        self.group1 = [None, None, None];
    }

    /// Restarts the shaft history: the next resolve takes its frame alone (ADR-3251).
    pub(super) fn reset_history(&mut self) {
        if self.history_valid {
            self.stats.history_resets += 1;
        }
        self.history_valid = false;
        self.stats.history_frames = 0;
    }

    /// A frame that draws no shafts: the draw stops reading the view from above and the next
    /// shaft frame starts a new history.
    pub(super) fn no_shafts(&mut self) {
        if self.above_written {
            self.queue.write_buffer(
                &self.shaft_uniform,
                0,
                bytemuck::bytes_of(&ShaftUniform::zeroed()),
            );
            self.above_written = false;
        }
        self.history_out = None;
        self.last_shaft = None;
        self.reset_history();
    }

    /// Records the tile pass for `params` (the first active body drives the frame's shared tile).
    /// `compute0` is the frame's group 0 for the compute layout; `ripple` the ripple map view.
    pub(super) fn record_tile(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        compute0: &wgpu::BindGroup,
        ripple: &wgpu::TextureView,
        params: &TileParams,
    ) {
        let _span = tracing::info_span!("water_caustic_tile").entered();
        if self.tile.is_none() {
            self.tile = Some(self.target("water caustic tile", [CAUSTIC_TILE_SIZE; 2]));
            self.drop_groups();
        }
        if self.tile_pipeline.is_none() {
            self.tile_pipeline = self.compute(
                "water caustic tile",
                TILE_ENTRY,
                &[Some(&self.layout_g0), None, Some(&self.layout_tile)],
            );
        }
        let (Some(tile), Some(pipeline)) = (&self.tile, &self.tile_pipeline) else {
            return;
        };
        let uniform = TileUniform {
            sun: [params.to_sun[0], params.to_sun[1], params.to_sun[2], 1.0],
            scroll: [
                params.scroll_a[0],
                params.scroll_a[1],
                params.scroll_b[0],
                params.scroll_b[1],
            ],
        };
        self.queue
            .write_buffer(&self.tile_uniform, 0, bytemuck::bytes_of(&uniform));
        let group2 = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water caustic tile bind"),
            layout: &self.layout_tile,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.tile_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&tile.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(ripple),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("water caustic tile"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, compute0, &[]);
        pass.set_bind_group(2, &group2, &[]);
        pass.dispatch_workgroups(CAUSTIC_TILE_SIZE / 8, CAUSTIC_TILE_SIZE / 8, 1);
        drop(pass);
        self.stats.tile_size = CAUSTIC_TILE_SIZE;
        self.stats.tile_passes += 1;
    }

    /// Records the half-resolution shaft pass for `shaft` (and, with accumulation on, its
    /// resolve). The tile pass of the same frame must already be recorded.
    pub(super) fn record_shafts(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        compute0: &wgpu::BindGroup,
        shaft: &ShaftFrame,
    ) {
        let steps = shaft.steps;
        let _span = tracing::info_span!("water_caustic_shafts", steps).entered();
        let size = [(shaft.viewport[0] / 2).max(1), (shaft.viewport[1] / 2).max(1)];
        if self.shaft.as_ref().is_none_or(|t| t.size != size) {
            self.shaft = Some(self.target("water shafts (half resolution)", size));
            self.drop_groups();
        }
        if self.shaft_pipeline.is_none() {
            self.shaft_pipeline = self.compute(
                "water caustic shafts",
                SHAFT_ENTRY,
                &[
                    Some(&self.layout_g0),
                    Some(&self.layout_g1_compute),
                    Some(&self.layout_shaft),
                ],
            );
        }
        // What the history may carry over: the same view (above or under, same body) and the
        // same switch; a new target size restarts it too.
        let kind = (shaft.from_above.map(|(slot, _)| slot), shaft.accumulate);
        if self.last_shaft != Some(kind) {
            self.reset_history();
            self.last_shaft = Some(kind);
        }
        if shaft.accumulate {
            if self.history.as_ref().is_none_or(|h| h[0].size != size) {
                self.history = Some([
                    self.target("water shafts history a", size),
                    self.target("water shafts history b", size),
                ]);
                self.history_out = None;
                self.reset_history();
                self.drop_groups();
            }
            if self.resolve_pipeline.is_none() {
                self.resolve_pipeline = self.compute(
                    "water shaft resolve",
                    RESOLVE_ENTRY,
                    &[Some(&self.layout_g0), None, Some(&self.layout_resolve)],
                );
            }
        } else if self.history.is_some() {
            self.history = None;
            self.history_out = None;
            self.drop_groups();
        }
        let jitter = if shaft.accumulate {
            self.jitter_frame = self.jitter_frame.wrapping_add(1);
            shaft_jitter_offset(self.jitter_frame)
        } else {
            0.0
        };
        let (light_vp, shadow) = match &self.sun_shadow {
            Some(s) => {
                let bias = if s.bias > 0.0 && s.bias.is_finite() {
                    s.bias
                } else {
                    SHAFT_SHADOW_BIAS
                };
                (
                    s.light_view_proj,
                    [1.0, bias, f32::from(u8::from(s.edge_fade)), 0.0],
                )
            }
            None => ([[0.0; 4]; 4], [0.0; 4]),
        };
        let uniform = ShaftUniform {
            light_vp,
            prev_view_proj: self.prev_view_proj.unwrap_or(shaft.view_proj),
            shadow,
            march: match shaft.from_above {
                Some((slot, level)) => [jitter, 1.0, level, slot as f32],
                None => [jitter, 0.0, 0.0, 0.0],
            },
            history: [
                f32::from(u8::from(self.history_valid)),
                SHAFT_HISTORY_FRAMES as f32,
                0.0,
                0.0,
            ],
        };
        self.queue
            .write_buffer(&self.shaft_uniform, 0, bytemuck::bytes_of(&uniform));
        self.above_written = shaft.from_above.is_some();
        self.prev_view_proj = Some(shaft.view_proj);
        let (Some(tile), Some(target), Some(pipeline)) = (&self.tile, &self.shaft, &self.shaft_pipeline)
        else {
            return;
        };
        let shadow_view = self
            .sun_shadow
            .as_ref()
            .map_or(&self.shadow_stand_in, |s| &s.view);
        let group1 = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water shaft tile bind"),
            layout: &self.layout_g1_compute,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&tile.view),
            }],
        });
        let group2 = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water shaft out bind"),
            layout: &self.layout_shaft,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&target.view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.shaft_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(shadow_view),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("water caustic shafts"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, compute0, &[]);
        pass.set_bind_group(1, &group1, &[]);
        pass.set_bind_group(2, &group2, &[]);
        pass.dispatch_workgroups(size[0].div_ceil(8), size[1].div_ceil(8), 1);
        drop(pass);
        self.stats.shaft_passes += 1;
        self.stats.shaft_texels = u64::from(size[0]) * u64::from(size[1]);
        self.stats.shaft_steps = steps;
        self.stats.shadow_bound = self.sun_shadow.is_some();
        self.stats.from_above = shaft.from_above.is_some();
        self.history_out = None;
        if shaft.accumulate {
            self.resolve(encoder, compute0, size);
        }
    }

    /// Records the temporal resolve of the march just recorded into the history target the last
    /// resolve did not write, and points the draw at it.
    fn resolve(&mut self, encoder: &mut wgpu::CommandEncoder, compute0: &wgpu::BindGroup, size: [u32; 2]) {
        let _span = tracing::info_span!("water_shaft_resolve").entered();
        let (Some(raw), Some(history), Some(pipeline)) = (&self.shaft, &self.history, &self.resolve_pipeline)
        else {
            return;
        };
        // The target the previous resolve wrote holds the history; write the other one.
        let read = usize::from(!self.stats.resolves.is_multiple_of(2));
        let write = 1 - read;
        let group2 = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water shaft resolve bind"),
            layout: &self.layout_resolve,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.shaft_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&raw.view),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::TextureView(&history[read].view),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::TextureView(&history[write].view),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("water shaft resolve"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, compute0, &[]);
        pass.set_bind_group(2, &group2, &[]);
        pass.dispatch_workgroups(size[0].div_ceil(8), size[1].div_ceil(8), 1);
        drop(pass);
        self.stats.resolves += 1;
        self.history_out = Some(write);
        self.history_valid = true;
        self.stats.history_frames = (self.stats.history_frames + 1).min(SHAFT_HISTORY_FRAMES);
    }

    /// Reads the shafts the draw samples this frame back (the accumulation when one was resolved,
    /// else the raw march): `[r, g, b, a]` per texel, row by row, and the size. `None` before the
    /// first shaft pass.
    pub(super) fn read_shafts(&self) -> Option<(Vec<[f32; 4]>, [u32; 2])> {
        let target = match (self.history_out, &self.history) {
            (Some(i), Some(history)) => &history[i],
            _ => self.shaft.as_ref()?,
        };
        let pixels = self.read_target(target)?;
        Some((pixels, target.size))
    }

    /// The group 0 for the compute passes.
    pub(super) fn compute_group0(
        &self,
        frame: &wgpu::Buffer,
        bodies: &wgpu::Buffer,
        depth: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water caustics compute frame bind"),
            layout: &self.layout_g0,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: frame.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: bodies.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(depth),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    /// Reads the tile back (`CAUSTIC_TILE_SIZE²` texels of `[tr J, det J, dr_x, dr_z]`), for the
    /// agent's stats and tests. `None` before the first tile pass.
    pub(super) fn read_tile(&self) -> Option<Vec<[f32; 4]>> {
        self.read_target(self.tile.as_ref()?)
    }

    /// Reads an `Rgba16Float` target back, row by row.
    fn read_target(&self, tile: &Target) -> Option<Vec<[f32; 4]>> {
        let [w, n] = tile.size;
        // Rows are padded to the copy alignment and cut back below.
        let tight = w * 8;
        let row = tight.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let size = u64::from(row) * u64::from(n);
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water caustic tile readback"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("water caustic tile readback"),
            });
        encoder.copy_texture_to_buffer(
            tile.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(n),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: n,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.recv().ok()?.ok()?;
        let data = readback.slice(..).get_mapped_range().ok()?;
        let out = data
            .chunks(row as usize)
            .flat_map(|line| {
                let halves: &[u16] = bytemuck::cast_slice(&line[..tight as usize]);
                halves
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| std::array::from_fn(|k| half_to_f32(c[k])))
                    .collect::<Vec<[f32; 4]>>()
            })
            .collect();
        drop(data);
        readback.unmap();
        Some(out)
    }
}

/// IEEE half to single.
fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let frac = f32::from(h & 0x3ff);
    match exp {
        0 => sign * frac * 2.0f32.powi(-24),
        31 if frac == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + frac / 1024.0) * 2.0f32.powi(exp - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::half_to_f32;

    #[test]
    fn halves_decode() {
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert!((half_to_f32(0x3555) - 0.333_251_95).abs() < 1.0e-6);
        assert!(half_to_f32(0x7c00).is_infinite());
    }
}
