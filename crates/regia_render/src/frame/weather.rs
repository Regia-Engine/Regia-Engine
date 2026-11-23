// SPDX-License-Identifier: MPL-2.0
//! Weather on the frame (spec 99): the surface state, the precipitation's extinction and the
//! precipitation occlusion view, a depth tile of the shadow atlas seen from the sky. Off, none of
//! it costs anything: no tile is planned and the forward pass skips its uniform branches.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use super::*;
use crate::surface_weather::{
    PrecipOcclusionBinding, PrecipOcclusionInfo, PrecipOcclusionMap, PrecipOcclusionRequest,
    PrecipOcclusionView, SunOcclusionRequest, SurfaceWeatherFrame,
};

/// A view tilted less than this from straight down is straight down (degrees).
const MIN_TILT_DEG: f32 = 10.0;
/// The most the view tilts (degrees).
const MAX_TILT_DEG: f32 = 45.0;
/// The tilt's step (degrees).
const TILT_STEP_DEG: f32 = 5.0;
/// The azimuth's step (degrees), so a gust does not turn the view.
const AZIMUTH_STEP_DEG: f32 = 15.0;
/// How far above the snapped center the view starts (m).
const VIEW_HEIGHT_M: f32 = 200.0;

/// How a view's direction lands on steps, so a gust or a slow sun does not redraw the tile.
struct ViewSteps {
    /// A view tilted less than this from straight down is straight down (degrees).
    min_tilt_deg: f32,
    /// The most it tilts (degrees).
    max_tilt_deg: f32,
    /// The tilt's step (degrees).
    tilt_deg: f32,
    /// The azimuth's step (degrees).
    azimuth_deg: f32,
}

/// The fall of rain and snow.
const FALL_STEPS: ViewSteps = ViewSteps {
    min_tilt_deg: MIN_TILT_DEG,
    max_tilt_deg: MAX_TILT_DEG,
    tilt_deg: TILT_STEP_DEG,
    azimuth_deg: AZIMUTH_STEP_DEG,
};

/// The sun: finer, since a low sun's shaft moves meters per step at tree distance, and down to 10°
/// of elevation. A lower sun keeps this tilt: the shade of a trunk still falls the right way.
const SUN_STEPS: ViewSteps = ViewSteps {
    min_tilt_deg: 2.5,
    max_tilt_deg: 80.0,
    tilt_deg: 2.5,
    azimuth_deg: 5.0,
};

/// The frame's weather inputs and the occlusion view's cache.
#[derive(Default)]
pub(super) struct FrameWeather {
    /// The surface state the frame shades with.
    pub(super) surface: SurfaceWeatherFrame,
    /// The precipitation's extinction (1/m).
    pub(super) extinction_per_m: f32,
    /// The tier's occlusion view `(texels, extent m)`; `None` on a tier without one.
    pub(super) occlusion_budget: Option<(u32, f32)>,
    /// What the host asked for directly (the layers' requests come first).
    pub(super) request: Option<PrecipOcclusionRequest>,
    /// The key of the tile's stored content.
    pub(super) cached_key: Option<u64>,
    /// Which casters over the tile moved lately (ADR-2585).
    pub(super) caster_motion: crate::surface_weather::PrecipCasterMotion,
    /// The tile's report.
    pub(super) info: Option<PrecipOcclusionInfo>,
    /// The tile as the frame's layers bind it: view-projection, uv rect, atlas edge, tile edge.
    pub(super) view: Option<PrecipOcclusionView>,
    /// The tile's texel size this frame, for the binding.
    pub(super) tile_texels: u32,
    /// The tile looks along the sun this frame ([`SunOcclusionRequest`]).
    pub(super) sun_view: bool,
    /// The tile redraws this frame.
    pub(super) dirty: bool,
    /// The device's largest texture edge; 0 is unknown, taken as 8192.
    pub(super) max_atlas_edge: u32,
    /// A lite frame also builds the lite surface-weather variant while dry, so the first wet
    /// frame compiles nothing (`FrameRenderer::set_warm_surface_weather`).
    pub(super) warm_lite: bool,
    /// The one-shot copy of the tile `render.surface_exposure` asked for.
    readback: Readback,
}

/// Where the one-shot tile copy is.
#[derive(Default)]
enum Readback {
    #[default]
    Idle,
    /// The next frame copies the tile.
    Requested,
    /// The copy is recorded.
    Copied(CopiedTile),
}

/// A tile copy waiting to be read.
struct CopiedTile {
    device: wgpu::Device,
    buffer: wgpu::Buffer,
    size: u32,
    bytes_per_row: u32,
    frame: u64,
    view_proj: [[f32; 4]; 4],
    bias_per_m: f32,
    texel_m: f32,
}

impl FrameWeather {
    /// Whether the lite forward path draws this frame with its surface-weather variant
    /// (`forward_lite::LITE_SURFACE_WEATHER_DEF`): the surface is wet or snowy, or the
    /// precipitation has an extinction. A dry frame keeps the variant-free pipelines.
    pub(super) fn lite_variant(&self) -> bool {
        !self.surface.is_dry() || self.extinction_per_m > 0.0
    }

    /// The frame uniform's `weather2` row.
    pub(super) fn uniform_row(&self, tile: Option<usize>) -> [f32; 4] {
        let extent = self.view.as_ref().map_or(0.0, |view| view.extent_m);
        [
            self.surface.clock_s,
            tile.map_or(-1.0, |index| index as f32),
            extent,
            self.extinction_per_m,
        ]
    }

    /// Records the one-shot tile copy after the shadow passes (spec 99 § 5.6): no per-frame
    /// readback, the copy runs only when an agent asked.
    pub(super) fn record_readback(
        &mut self,
        d: &crate::TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        plan: &shadow_plan::ShadowPlan,
        atlas: Option<&(u32, wgpu::TextureView)>,
        frame: u64,
    ) {
        if !matches!(self.readback, Readback::Requested) {
            return;
        }
        let (Some(index), Some((_, atlas))) = (plan.precip_tile, atlas) else {
            return;
        };
        let tile = &plan.tiles[index];
        // A depth texture copies whole or not at all, and the atlas may be 8K: one fullscreen blit
        // of the tile into a float target, then a copy of that.
        let shader = format!(
            "@group(0) @binding(0) var atlas: texture_depth_2d;\n\
             @vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {{\n\
             let c = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));\n\
             return vec4<f32>(c * 2.0 - 1.0, 0.0, 1.0);\n\
             }}\n\
             @fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {{\n\
             let t = vec2<i32>(p.xy) + vec2<i32>({}, {});\n\
             return vec4<f32>(textureLoad(atlas, t, 0), 0.0, 0.0, 1.0);\n\
             }}\n",
            tile.x, tile.y
        );
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("precipitation occlusion readback"),
            source: wgpu::ShaderSource::Wgsl(shader.into()),
        });
        let format = wgpu::TextureFormat::R32Float;
        let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("precipitation occlusion readback"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(format.into())],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let extent = wgpu::Extent3d {
            width: tile.size,
            height: tile.size,
            depth_or_array_layers: 1,
        };
        let target = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("precipitation occlusion readback"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let bind_group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("precipitation occlusion readback"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(atlas),
            }],
        });
        {
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());
            let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("precipitation occlusion readback"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, Some(&bind_group), &[]);
            pass.draw(0..3, 0..1);
        }
        let bytes_per_row = (tile.size * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("precipitation occlusion readback"),
            size: u64::from(bytes_per_row) * u64::from(tile.size),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(tile.size),
                },
            },
            extent,
        );
        self.readback = Readback::Copied(CopiedTile {
            device: (**d).clone(),
            buffer,
            size: tile.size,
            bytes_per_row,
            frame,
            view_proj: tile.gpu.view_proj,
            bias_per_m: tile.gpu.bias[1],
            texel_m: tile.gpu.bias[0],
        });
    }
}

/// The occlusion view of one frame: where it looks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PrecipTilePlan {
    /// Tile edge (texels).
    pub(super) size: u32,
    /// Side of the covered square (m).
    pub(super) extent_m: f32,
    /// The snapped center.
    pub(super) center: [f32; 3],
    /// The quantized fall direction, unit, down.
    pub(super) fall_dir: [f32; 3],
    /// Its tilt from straight down (degrees).
    pub(super) tilt_deg: f32,
    /// It looks along the sun, not along the fall.
    pub(super) sun_view: bool,
}

/// The fall direction quantized for the view: straight down below 10°, clamped to 45°, the tilt in
/// 5° steps and the azimuth in 15°. Returns `(direction, tilt degrees)`.
pub(super) fn quantize_fall_dir(fall_dir: [f32; 3]) -> ([f32; 3], f32) {
    quantize_view_dir(fall_dir, &FALL_STEPS)
}

/// A downward view direction on `steps`. Returns `(direction, tilt degrees)`.
fn quantize_view_dir(fall_dir: [f32; 3], steps: &ViewSteps) -> ([f32; 3], f32) {
    let horizontal = fall_dir[0].hypot(fall_dir[2]);
    let down = (-fall_dir[1]).max(1.0e-4);
    let tilt = horizontal.atan2(down).to_degrees();
    if !tilt.is_finite() || tilt < steps.min_tilt_deg {
        return ([0.0, -1.0, 0.0], 0.0);
    }
    let tilt = (tilt.min(steps.max_tilt_deg) / steps.tilt_deg).round() * steps.tilt_deg;
    let azimuth = fall_dir[2].atan2(fall_dir[0]).to_degrees();
    let azimuth = ((azimuth / steps.azimuth_deg).round() * steps.azimuth_deg).to_radians();
    let (sin_tilt, cos_tilt) = tilt.to_radians().sin_cos();
    (
        [sin_tilt * azimuth.cos(), -cos_tilt, sin_tilt * azimuth.sin()],
        tilt,
    )
}

/// The view around `eye`: the square snapped to `extent / 8` in world space.
pub(super) fn plan_precip_tile(
    request: &PrecipOcclusionRequest,
    budget: (u32, f32),
    eye: [f32; 3],
) -> PrecipTilePlan {
    let (size, extent_m) = budget;
    let snap = extent_m / 8.0;
    let center = eye.map(|v| (v / snap).round() * snap);
    let (fall_dir, tilt_deg) = quantize_fall_dir(request.fall_dir);
    PrecipTilePlan {
        size,
        extent_m,
        center,
        fall_dir,
        tilt_deg,
        sun_view: false,
    }
}

/// The view along the sun (`sun_travel`, the way its light goes) around `eye`, snapped like the
/// fall's. `None` with the sun on or under the horizon, or an empty request.
pub(super) fn plan_sun_tile(
    request: &SunOcclusionRequest,
    sun_travel: [f32; 3],
    eye: [f32; 3],
) -> Option<PrecipTilePlan> {
    let [x, y, z] = sun_travel;
    let down = -y / (x * x + y * y + z * z).sqrt();
    // `is_finite` first: a NaN extent fails every comparison and would pass `<= 0.0`.
    if request.texels == 0
        || !request.extent_m.is_finite()
        || request.extent_m <= 0.0
        || !down.is_finite()
        || down < 0.02
    {
        return None;
    }
    let (size, extent_m) = (request.texels, request.extent_m);
    let snap = extent_m / 8.0;
    let (fall_dir, tilt_deg) = quantize_view_dir(sun_travel, &SUN_STEPS);
    Some(PrecipTilePlan {
        size,
        extent_m,
        center: eye.map(|v| (v / snap).round() * snap),
        fall_dir,
        tilt_deg,
        sun_view: true,
    })
}

/// The view a frame draws. The tile has one user: rain, snow and a host's request first (`asked`),
/// then a wet or snowy surface, straight down; both on the tier's `budget`. A sun view takes what
/// is left, on a dry frame only, because the surface shading reads the tile on any other.
pub(super) fn choose_precip_view(
    asked: Option<PrecipOcclusionRequest>,
    budget: Option<(u32, f32)>,
    surface_dry: bool,
    sun: Option<SunOcclusionRequest>,
    sun_travel: [f32; 3],
    eye: [f32; 3],
) -> Option<PrecipTilePlan> {
    let weather = asked.or_else(|| {
        (!surface_dry).then_some(PrecipOcclusionRequest {
            fall_dir: [0.0, -1.0, 0.0],
        })
    });
    if let (Some(request), Some(budget)) = (weather, budget) {
        return Some(plan_precip_tile(&request, budget, eye));
    }
    if !surface_dry {
        return None;
    }
    plan_sun_tile(&sun?, sun_travel, eye)
}

/// The plan when its tile still fits the shadow atlas: `atlas` texels already taken along the
/// edge, `limit` the device's largest edge. Checked: a host's tile edge near `u32::MAX` is refused,
/// never wrapped into a small sum (or saturated onto a `u32::MAX` limit) that passes
/// (rc1-rev-render-k 1).
pub(super) fn admit_precip_tile(plan: PrecipTilePlan, atlas: u32, limit: u32) -> Option<PrecipTilePlan> {
    atlas
        .checked_add(plan.size)
        .is_some_and(|edge| edge <= limit)
        .then_some(plan)
}

/// The tile's view-projection and its bias row `[texel m, 1 / depth extent, 0 ortho, 0]`.
pub(super) fn precip_tile_matrix(plan: &PrecipTilePlan) -> ([[f32; 4]; 4], [f32; 4]) {
    let radius = plan.extent_m * 0.5;
    let view_proj = ortho_light_vp(plan.fall_dir, plan.center, radius, VIEW_HEIGHT_M, 0);
    (
        view_proj,
        [
            plan.extent_m / plan.size as f32,
            1.0 / (VIEW_HEIGHT_M + plan.extent_m),
            0.0,
            0.0,
        ],
    )
}

/// `true` when `aabb` is over the tile's square, the tilt's reach included. A sun view tests the
/// prism it looks along: the fall's square reach would span a kilometer of forest under a low sun.
pub(super) fn caster_touches_tile(aabb: &crate::paged_shadow::Aabb3, plan: &PrecipTilePlan) -> bool {
    if plan.sun_view {
        use regia_core::glam::Vec3;
        let (min, max) = (Vec3::from_array(aabb.min), Vec3::from_array(aabb.max));
        let dir = Vec3::from_array(plan.fall_dir);
        let center = Vec3::from_array(plan.center);
        let side = plan.extent_m * std::f32::consts::FRAC_1_SQRT_2;
        // The prism's box first: this runs per caster per dry frame, tens of thousands in a forest,
        // and almost all of them are nowhere near it.
        let (start, end) = (center - dir * VIEW_HEIGHT_M, center + dir * plan.extent_m);
        if max.cmplt(start.min(end) - side).any() || min.cmpgt(start.max(end) + side).any() {
            return false;
        }
        let bound = (max - min).length() * 0.5;
        let from_center = (min + max) * 0.5 - center;
        let along = from_center.dot(dir);
        let off_axis = (from_center - dir * along).length();
        return off_axis <= side + bound && along + bound >= -VIEW_HEIGHT_M && along - bound <= plan.extent_m;
    }
    let reach = plan.extent_m * 0.5 + VIEW_HEIGHT_M * plan.tilt_deg.to_radians().tan();
    aabb.max[0] >= plan.center[0] - reach
        && aabb.min[0] <= plan.center[0] + reach
        && aabb.max[2] >= plan.center[2] - reach
        && aabb.min[2] <= plan.center[2] + reach
}

/// The tile's cache key: the place, the tilt, the atlas, the ground's content key and the casters
/// over the square. The same
/// key means the stored depth is still the view's (spec 99 § 5.6). A caster that `moving(id,
/// exact)` reports as moving joins by its cell at the square's snap instead of its exact bounds
/// and pose, so a vehicle under the eye redraws the tile about once per snap (ADR-2585).
pub(super) fn precip_tile_key(
    plan: &PrecipTilePlan,
    rect: [u32; 3],
    atlas_size: u32,
    casters: &[crate::paged_shadow::ShadowCaster],
    ground: u64,
    mut moving: impl FnMut(u64, u64) -> bool,
) -> u64 {
    fn mix(hash: u64, word: u64) -> u64 {
        (hash ^ word).wrapping_mul(0x0000_0100_0000_01B3).rotate_left(23)
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for v in plan.center {
        hash = mix(hash, u64::from(v.to_bits()));
    }
    for v in plan.fall_dir {
        hash = mix(hash, u64::from(v.to_bits()));
    }
    for v in rect {
        hash = mix(hash, u64::from(v));
    }
    hash = mix(hash, u64::from(atlas_size));
    hash = mix(hash, ground);
    for caster in casters {
        if !caster_touches_tile(&caster.aabb, plan) {
            continue;
        }
        hash = mix(hash, caster.id);
        let (min, max) = (caster.aabb.min, caster.aabb.max);
        let exact = min
            .into_iter()
            .chain(max)
            .chain(caster.pose.model.into_iter().flatten())
            .fold(caster.id, |h, v| mix(h, u64::from(v.to_bits())));
        hash = if moving(caster.id, exact) {
            let cells = crate::surface_weather::PrecipCasterMotion::cell_words(min, max, plan.extent_m / 8.0);
            cells.into_iter().fold(hash, mix)
        } else {
            mix(hash, exact)
        };
    }
    hash
}

impl FrameRenderer {
    /// The surface weather this frame shades with. All zero is a dry frame: the forward pass
    /// skips the rule on a uniform branch. [`Self::set_outdoor`] sets it too, from
    /// [`crate::OutdoorParams::surface_weather`], so a host of that call sets it there.
    pub fn set_surface_weather(&mut self, weather: SurfaceWeatherFrame) {
        self.weather.surface = weather;
    }

    /// See [`Self::set_surface_weather`].
    #[must_use]
    pub fn surface_weather(&self) -> SurfaceWeatherFrame {
        self.weather.surface
    }

    /// The precipitation's constant extinction, 1/m (`regia_scene::weather::WeatherAir`). It
    /// applies wherever the frame fog does, and whether the host's fog is on or not.
    /// [`Self::set_outdoor`] sets it too, from [`crate::OutdoorParams::precip_extinction_per_m`].
    pub fn set_precipitation_extinction(&mut self, extinction_per_m: f32) {
        self.weather.extinction_per_m = if extinction_per_m.is_finite() {
            extinction_per_m.max(0.0)
        } else {
            0.0
        };
    }

    /// See [`Self::set_precipitation_extinction`].
    #[must_use]
    pub fn precipitation_extinction(&self) -> f32 {
        self.weather.extinction_per_m
    }

    /// Asks for the occlusion view without a layer: `None` leaves it to the layers and the
    /// surface state.
    pub fn set_precipitation_occlusion(&mut self, request: Option<PrecipOcclusionRequest>) {
        self.weather.request = request;
    }

    /// The tier's occlusion view `(texels, extent m)`; `None` on a tier without one. The VFX
    /// plugin sets it from `PrecipBudget`; the default keeps the view off.
    pub fn set_precipitation_occlusion_budget(&mut self, budget: Option<(u32, f32)>, max_atlas_edge: u32) {
        self.weather.occlusion_budget = budget.filter(|&(texels, extent)| texels > 0 && extent > 0.0);
        self.weather.max_atlas_edge = max_atlas_edge;
    }

    /// The occlusion view of the last frame, when one was drawn or held. A view along the sun is
    /// [`Self::sun_occlusion`]'s: it says nothing about the sky over a surface.
    #[must_use]
    pub fn precipitation_occlusion(&self) -> Option<PrecipOcclusionInfo> {
        self.weather.info.filter(|_| !self.weather.sun_view)
    }

    /// The occlusion view of the last frame when it looked along the sun
    /// ([`crate::TranslucentLayer::sun_occlusion_request`]).
    #[must_use]
    pub fn sun_occlusion(&self) -> Option<PrecipOcclusionInfo> {
        self.weather.info.filter(|_| self.weather.sun_view)
    }

    /// Asks the next frame to copy the occlusion tile for [`Self::take_precip_occlusion_readback`].
    pub fn request_precip_occlusion_readback(&mut self) {
        if matches!(self.weather.readback, Readback::Idle) {
            self.weather.readback = Readback::Requested;
        }
    }

    /// The copy [`Self::request_precip_occlusion_readback`] asked for, once a frame made it.
    /// Blocks on the map (a one-shot agent call, never per frame). `None` while no frame has.
    pub fn take_precip_occlusion_readback(&mut self) -> Option<PrecipOcclusionMap> {
        if !matches!(self.weather.readback, Readback::Copied(_)) {
            return None;
        }
        let Readback::Copied(copy) = std::mem::take(&mut self.weather.readback) else {
            return None;
        };
        let slice = copy.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        let _ = copy.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().ok()?.ok()?;
        let mapped = slice.get_mapped_range().ok()?;
        let row = copy.size as usize * 4;
        let mut depth = Vec::with_capacity(row / 4 * copy.size as usize);
        for y in 0..copy.size as usize {
            let start = y * copy.bytes_per_row as usize;
            let (texels, _) = mapped[start..start + row].as_chunks::<4>();
            depth.extend(texels.iter().map(|&bytes| f32::from_le_bytes(bytes)));
        }
        drop(mapped);
        copy.buffer.unmap();
        Some(PrecipOcclusionMap {
            frame: copy.frame,
            size: copy.size,
            depth,
            view_proj: copy.view_proj,
            depth_per_m: copy.bias_per_m,
            texel_m: copy.texel_m,
        })
    }

    /// Whether a frame warm builds the shared draw's surface-weather variant: on for a project
    /// that uses it. On the Mobile tiers' lite forward path it also builds the lite
    /// surface-weather pipelines beside the dry ones on the first lite frame, so the first wet
    /// frame compiles nothing (off: the first wet frame composes them).
    pub fn set_warm_surface_weather(&mut self, on: bool) {
        self.weather.warm_lite = on;
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.set_warm_surface_weather(on);
        }
    }

    /// Runs the shared draw's pipeline warm now, on this thread (a host without a warm queue).
    pub fn warm_shared_pipelines(&mut self) {
        self.warm_shared_draw();
    }

    /// `true` while the shared draw runs its surface-weather variant.
    #[must_use]
    pub fn surface_weather_variant_on(&self) -> bool {
        self.shared_draw
            .as_ref()
            .is_some_and(|draw| draw.surface_weather_variant_on())
    }

    /// The view this frame draws, from the layers' requests, the host's and the surface state
    /// ([`choose_precip_view`]). `sun_travel` is the frame's sun, the way its light goes.
    pub(super) fn wanted_precip_tile(
        &self,
        scene: &FrameScene<'_>,
        local_lights: &[crate::LabLocalLight],
        sun_travel: [f32; 3],
    ) -> Option<PrecipTilePlan> {
        let asked = self
            .translucent
            .iter()
            .find_map(|layer| layer.precipitation_occlusion_request())
            .or(self.weather.request);
        let sun = self
            .translucent
            .iter()
            .find_map(|layer| layer.sun_occlusion_request());
        let plan = choose_precip_view(
            asked,
            self.weather.occlusion_budget,
            self.weather.surface.is_dry(),
            sun,
            sun_travel,
            scene.eye,
        )?;
        // The atlas grows by the tile: where that passes the device's limit, no view.
        let limit = match self.weather.max_atlas_edge {
            0 => 8192,
            edge => edge,
        };
        let quality = &scene.quality;
        let near = if scene.directional_shadows {
            quality.near_sun_tile
        } else {
            0
        };
        let lights = local_lights.iter().any(shadow_plan::light_casts) && quality.local_shadow_max_lights > 0;
        let atlas = if lights {
            quality.local_shadow_atlas_size.max(256).max(near)
        } else {
            near
        };
        admit_precip_tile(plan, atlas, limit)
    }

    /// Records this frame's tile state from the plan, and hands the shared draw its view.
    pub(super) fn note_precip_tile(&mut self, plan: &shadow_plan::ShadowPlan) {
        let Some(index) = plan.precip_tile else {
            self.weather.view = None;
            self.weather.info = None;
            self.weather.cached_key = None;
            self.weather.sun_view = false;
            return;
        };
        let tile = &plan.tiles[index];
        self.weather.tile_texels = tile.size;
        self.weather.sun_view = plan.precip_plan.is_some_and(|p| p.sun_view);
        self.weather.view = plan.precip_plan.map(|p| PrecipOcclusionView {
            view_proj: tile.gpu.view_proj,
            uv_rect: tile.gpu.uv,
            extent_m: p.extent_m,
            texel_m: tile.gpu.bias[0],
            depth_per_m: tile.gpu.bias[1],
            atlas_texels: plan.atlas_size as f32,
        });
    }

    /// Hands the shared draw the atlas that holds the tile. A texture view keeps its own atlas
    /// and draws no tile.
    pub(super) fn bind_shared_precip_atlas(&mut self) {
        if self.view_pass.is_some() {
            return;
        }
        let atlas = self
            .weather
            .view
            .and_then(|_| self.atlas.as_ref().map(|(_, view)| view.clone()));
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.set_engine_precip_occlusion(atlas.as_ref());
        }
    }

    /// Decides whether the tile redraws: its key, an atlas that lost its contents, or a caster
    /// without bounds (skinned), which may have moved anywhere.
    pub(super) fn precip_tile_dirty(
        &mut self,
        plan: &shadow_plan::ShadowPlan,
        casters: &[crate::paged_shadow::ShadowCaster],
        unbounded: bool,
        paging: bool,
    ) -> bool {
        let (Some(index), Some(view)) = (plan.precip_tile, plan.precip_plan) else {
            return false;
        };
        let tile = &plan.tiles[index];
        let ground = self.shared_draw.as_ref().map_or(0, |draw| draw.content_key());
        let (frame, motion) = (self.frame_index, &mut self.weather.caster_motion);
        motion.sweep(frame);
        let key = precip_tile_key(
            &view,
            [tile.x, tile.y, tile.size],
            plan.atlas_size,
            casters,
            ground,
            |id, exact| motion.moving(id, exact, frame),
        );
        let dirty = self.weather.cached_key != Some(key) || (paging && !self.atlas_valid) || unbounded;
        self.weather.cached_key = Some(key);
        let last = self.weather.info;
        self.weather.info = Some(PrecipOcclusionInfo {
            size_texels: view.size,
            extent_m: view.extent_m,
            origin: [f64::from(view.center[0]), f64::from(view.center[2])],
            tilt_deg: view.tilt_deg,
            redraws: last.map_or(0, |info| info.redraws) + u64::from(dirty),
            last_redraw_frame: if dirty {
                self.frame_index
            } else {
                last.map_or(0, |info| info.last_redraw_frame)
            },
            tile: index as u32,
        });
        dirty
    }

    /// Binds the tile to the frame's translucent layers.
    pub(super) fn bind_precip_occlusion(&self, atlas: &wgpu::TextureView, atlas_size: u32) {
        let binding = self.weather.view.as_ref().map(|view| PrecipOcclusionBinding {
            atlas,
            view_proj: view.view_proj,
            uv_rect: view.uv_rect,
            extent_m: view.extent_m,
            atlas_texels: atlas_size as f32,
            tile_texels: self.weather.tile_texels as f32,
        });
        let (fall, sun) = if self.weather.sun_view {
            (None, binding.as_ref())
        } else {
            (binding.as_ref(), None)
        };
        for layer in &self.translucent {
            layer.bind_precipitation_occlusion(fall);
            layer.bind_sun_occlusion(sun);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fall_direction_quantizes() {
        assert_eq!(quantize_fall_dir([0.0, -1.0, 0.0]), ([0.0, -1.0, 0.0], 0.0));
        // Below 10°: straight down.
        let (dir, tilt) = quantize_fall_dir([0.1, -1.0, 0.0]);
        assert_eq!((dir, tilt), ([0.0, -1.0, 0.0], 0.0));
        // A steady slant lands on a step, and a nudge stays on it.
        let (a, ta) = quantize_fall_dir([0.5, -1.0, 0.1]);
        let (b, tb) = quantize_fall_dir([0.505, -1.0, 0.1]);
        assert_eq!((a, ta), (b, tb));
        assert_eq!(ta % 5.0, 0.0);
        // Clamped at 45°.
        let (_, tilt) = quantize_fall_dir([20.0, -1.0, 0.0]);
        assert_eq!(tilt, 45.0);
        let length = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        assert!((length - 1.0).abs() < 1.0e-5);
    }

    #[test]
    fn the_square_snaps_to_an_eighth_of_its_side() {
        let request = PrecipOcclusionRequest {
            fall_dir: [0.0, -1.0, 0.0],
        };
        let a = plan_precip_tile(&request, (512, 64.0), [1.0, 2.0, 3.0]);
        let b = plan_precip_tile(&request, (512, 64.0), [1.5, 2.5, 3.5]);
        assert_eq!(a.center, b.center, "a small step keeps the view");
        let c = plan_precip_tile(&request, (512, 64.0), [9.0, 2.0, 3.0]);
        assert_ne!(a.center, c.center, "a whole snap moves it");
        assert_eq!(a.center[0] % 8.0, 0.0);
    }

    /// The tile has one user: the weather's view always wins, and the surface shading reads the
    /// tile on any frame that is not dry, so a sun view is drawn only on a dry frame.
    #[test]
    fn the_sun_view_takes_only_what_the_weather_leaves() {
        let rain = PrecipOcclusionRequest {
            fall_dir: [0.0, -1.0, 0.0],
        };
        let sun = SunOcclusionRequest {
            texels: 256,
            extent_m: 32.0,
        };
        let budget = Some((512, 64.0));
        let travel = [0.0, -0.5, 0.866];
        let choose = |asked, budget, dry, sun| choose_precip_view(asked, budget, dry, sun, travel, [0.0; 3]);
        assert!(choose(None, None, true, None).is_none(), "nobody asks");
        assert!(
            choose(None, budget, true, None).is_none(),
            "a dry frame without a wish"
        );
        let only = choose(None, None, true, Some(sun)).expect("the sun view");
        assert!(only.sun_view);
        assert_eq!(only.size, 256, "its own size, with the weather's budget off");
        assert_eq!(only.tilt_deg, 60.0);
        let falling = choose(Some(rain), budget, true, Some(sun)).expect("rain");
        assert!(!falling.sun_view, "rain keeps it");
        assert_eq!(falling.size, 512);
        let wet = choose(None, budget, false, Some(sun)).expect("wet");
        assert!(!wet.sun_view, "the wet surface keeps it");
        assert_eq!(wet.tilt_deg, 0.0, "straight down");
        assert!(
            choose(None, None, false, Some(sun)).is_none(),
            "wet with the view off: open sky, as without the sun's wish"
        );
    }

    #[test]
    fn the_sun_view_reaches_a_low_sun_in_fine_steps() {
        let request = SunOcclusionRequest {
            texels: 256,
            extent_m: 32.0,
        };
        let plan = |travel: [f32; 3]| plan_sun_tile(&request, travel, [1.0, 2.0, 3.0]);
        let down = plan([0.0, -1.0, 0.0]).expect("noon");
        assert_eq!(down.fall_dir, [0.0, -1.0, 0.0]);
        // 15° up: past the fall's 45°.
        let (sin_e, cos_e) = 15.0_f32.to_radians().sin_cos();
        let low = plan([0.0, -sin_e, cos_e]).expect("golden hour");
        assert_eq!(low.tilt_deg, 75.0);
        assert!((low.fall_dir[1] + sin_e).abs() < 1.0e-5, "{:?}", low.fall_dir);
        let length = low.fall_dir.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((length - 1.0).abs() < 1.0e-5);
        // A lower sun holds the steepest tilt the view has, still on its azimuth.
        let (sin_e, cos_e) = 5.0_f32.to_radians().sin_cos();
        let lower = plan([cos_e, -sin_e, 0.0]).expect("sunset");
        assert_eq!(lower.tilt_deg, 80.0);
        assert!(lower.fall_dir[0] > 0.9 && lower.fall_dir[2].abs() < 1.0e-5);
        // A nudge stays on its step; the steps are finer than the fall's.
        let a = plan([0.5, -1.0, 0.1]).expect("a");
        assert_eq!(a.fall_dir, plan([0.5005, -1.0, 0.1]).expect("b").fall_dir);
        assert_eq!(a.tilt_deg % 2.5, 0.0);
        // Night and nonsense ask for nothing.
        assert!(plan([0.0, 1.0, 0.0]).is_none());
        assert!(plan([1.0, 0.0, 0.0]).is_none(), "on the horizon");
        assert!(plan([f32::NAN, -1.0, 0.0]).is_none());
        assert!(plan([0.0; 3]).is_none());
        let empty = SunOcclusionRequest {
            texels: 0,
            extent_m: 32.0,
        };
        assert!(plan_sun_tile(&empty, [0.0, -1.0, 0.0], [0.0; 3]).is_none());
    }

    /// rc1-rev-render-k 2: a NaN or infinite extent asks for nothing, as the budget path's filter
    /// does; it used to make a NaN tile center and view-projection.
    #[test]
    fn a_sun_view_with_a_non_finite_extent_asks_for_nothing() {
        for extent_m in [f32::NAN, f32::INFINITY, -f32::NAN] {
            let request = SunOcclusionRequest {
                texels: 256,
                extent_m,
            };
            assert!(
                plan_sun_tile(&request, [0.0, -1.0, 0.0], [1.0, 2.0, 3.0]).is_none(),
                "{extent_m}"
            );
        }
    }

    /// rc1-rev-render-k 1: the atlas admission check of a host's tile size near `u32::MAX` refuses
    /// it instead of wrapping (release) or panicking on the add (debug); a wrapped sum passed the
    /// limit and sent a `u32::MAX`-wide texture to the device.
    #[test]
    fn a_huge_tile_is_refused_not_wrapped() {
        let request = PrecipOcclusionRequest {
            fall_dir: [0.0, -1.0, 0.0],
        };
        let plan = plan_precip_tile(&request, (u32::MAX, 1.0), [0.0; 3]);
        assert_eq!(admit_precip_tile(plan, 256, 8192), None);
        assert_eq!(admit_precip_tile(plan, 0, 8192), None);
        let huge = plan_precip_tile(&request, (u32::MAX - 100, 1.0), [0.0; 3]);
        assert_eq!(admit_precip_tile(huge, 4096, u32::MAX), None);
        // What fits still fits, to the texel.
        let fits = plan_precip_tile(&request, (512, 64.0), [0.0; 3]);
        assert_eq!(admit_precip_tile(fits, 4096, 4608), Some(fits));
        assert_eq!(admit_precip_tile(fits, 4097, 4608), None);
    }

    /// Under a low sun the view takes what stands in its prism, not a square a kilometer wide.
    #[test]
    fn a_low_sun_view_draws_the_casters_in_its_prism() {
        let (sin_e, cos_e) = 15.0_f32.to_radians().sin_cos();
        // The light travels toward +Z.
        let plan = plan_sun_tile(
            &SunOcclusionRequest {
                texels: 256,
                extent_m: 32.0,
            },
            [0.0, -sin_e, cos_e],
            [0.0; 3],
        )
        .expect("view");
        let tree = |x: f32, z: f32| {
            let height = -z * sin_e / cos_e;
            crate::paged_shadow::Aabb3::from_center_half([x, height, z], [1.0, 1.0, 1.0])
        };
        assert!(caster_touches_tile(&tree(0.0, -40.0), &plan), "up-sun of the eye");
        assert!(caster_touches_tile(&tree(0.0, -180.0), &plan), "far up-sun");
        assert!(!caster_touches_tile(&tree(60.0, -40.0), &plan), "off to the side");
        assert!(
            !caster_touches_tile(&tree(0.0, -400.0), &plan),
            "past the view's start"
        );
        let below = crate::paged_shadow::Aabb3::from_center_half([0.0, -60.0, -40.0], [1.0; 3]);
        assert!(!caster_touches_tile(&below, &plan), "under the sun's line");
        // Far behind the square, down-sun, it shades nothing the view looks at.
        let behind = crate::paged_shadow::Aabb3::from_center_half([0.0, -30.0, 120.0], [1.0; 3]);
        assert!(!caster_touches_tile(&behind, &plan), "down-sun of the square");
    }

    /// The cost of the per-caster test on a forest's caster count (printed, not asserted):
    /// `cargo test -p regia_render --lib bench_sun_view_caster_test -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement"]
    fn bench_sun_view_caster_test() {
        let plan = plan_sun_tile(
            &SunOcclusionRequest {
                texels: 512,
                extent_m: 64.0,
            },
            [0.3, -0.8, 0.5],
            [0.0; 3],
        )
        .expect("view");
        // 60k trees over 2 km by 2 km, seeded by index.
        let boxes: Vec<crate::paged_shadow::Aabb3> = (0..60_000u32)
            .map(|i| {
                let x = (i.wrapping_mul(2_654_435_761) >> 16) as f32 / 65_536.0 * 2_000.0 - 1_000.0;
                let z = (i.wrapping_mul(2_246_822_519) >> 16) as f32 / 65_536.0 * 2_000.0 - 1_000.0;
                crate::paged_shadow::Aabb3::from_center_half([x, 12.0, z], [3.0, 12.0, 3.0])
            })
            .collect();
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let mut touching = 0u32;
            for _ in 0..20 {
                touching += boxes
                    .iter()
                    .filter(|aabb| caster_touches_tile(std::hint::black_box(aabb), &plan))
                    .count() as u32;
            }
            let total = start.elapsed().as_secs_f64() / 20.0;
            println!(
                "60k casters: {:.3} ms per frame ({:.1} ns each), {} touch the prism",
                total * 1e3,
                total * 1e9 / 60_000.0,
                touching / 20
            );
        }
    }

    fn caster(id: u64, x: f32) -> crate::paged_shadow::ShadowCaster {
        crate::paged_shadow::ShadowCaster {
            is_static: false,
            id,
            aabb: crate::paged_shadow::Aabb3::from_center_half([x, 0.0, 0.0], [1.0; 3]),
            cascades: u8::MAX,
            pose: crate::paged_shadow::CasterPose {
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                center: [0.0; 3],
                radius: 1.0,
            },
            dynamic: false,
        }
    }

    /// § 11: the key follows the place and the casters over the square, nothing else.
    #[test]
    fn the_key_follows_what_the_tile_shows() {
        let request = PrecipOcclusionRequest {
            fall_dir: [0.0, -1.0, 0.0],
        };
        let plan = plan_precip_tile(&request, (512, 64.0), [0.0, 0.0, 0.0]);
        let key = |plan: &PrecipTilePlan, casters: &[_]| {
            precip_tile_key(plan, [0, 0, 512], 1024, casters, 7, |_, _| false)
        };
        let near = caster(1, 4.0);
        let far = caster(2, 900.0);
        let base = key(&plan, &[near, far]);
        assert_eq!(base, key(&plan, &[near, far]), "a held frame keeps it");
        let moved_far = caster(2, 905.0);
        assert_eq!(
            base,
            key(&plan, &[near, moved_far]),
            "a caster off the square is ignored"
        );
        let moved_near = caster(1, 5.0);
        assert_ne!(base, key(&plan, &[moved_near, far]), "a caster on it redraws");
        let snapped = plan_precip_tile(&request, (512, 64.0), [20.0, 0.0, 0.0]);
        assert_ne!(base, key(&snapped, &[near, far]), "a snap redraws");
        let edited = precip_tile_key(&plan, [0, 0, 512], 1024, &[near, far], 8, |_, _| false);
        assert_ne!(base, edited, "edited ground redraws");
        // A moving caster keys by its snapped cell: a step inside it holds, a cell away redraws.
        let moving = |casters: &[_]| precip_tile_key(&plan, [0, 0, 512], 1024, casters, 7, |_, _| true);
        assert_eq!(moving(&[caster(1, 4.5), far]), moving(&[caster(1, 6.5), far]));
        assert_ne!(moving(&[caster(1, 4.5), far]), moving(&[caster(1, 12.5), far]));
    }
}
