// SPDX-License-Identifier: MPL-2.0
//! The lens effects pass on the GPU: a [`FramePass`] at [`GraphSlot::BeforePost`] that reads the
//! view's [`LensEffects`] from [`FramePassContext::post`] (ADR-1863).
//!
//! Per frame that asks for any effect: the bright pass and its mip chain (quarter size), the grid
//! of light cells and an aperture sprite per ghost and lit cell (quarter size), then one full-size
//! composite into the colour target, additive when there is no chromatic aberration, or over a
//! copy of the target when there is. A frame that asks for none returns before recording
//! anything.

use crate::frame_work::counted::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{GhostEntry, GhostTable, LensEffects, MAX_GHOSTS, MIN_GHOST_MAGNIFICATION, default_ghost_table};
use crate::bind_cache::{BindCache, PerSize};
use crate::device::TrackedDevice;
use crate::{FramePass, FramePassContext, FrameRenderer, GraphSlot, RenderError, SlotColorSpace};

/// The pass's name in the frame graph (`render.graph`).
pub const LENS_EFFECTS_PASS_NAME: &str = "lens_effects";

/// Format of the bright image and the ghosts.
const WORK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The bright image is this many times smaller than the viewport on each axis.
const REDUCTION: u32 = 4;
/// Most mips of the bright image: enough for the dirt's glare and the light cells.
const MAX_BRIGHT_MIPS: u32 = 5;
/// The grid of light cells (mirrored in the shader): small enough that every ghost costs at most
/// `CELLS_X * CELLS_Y` sprites, fine enough that separate lights keep separate ghosts.
const CELLS_X: u32 = 16;
const CELLS_Y: u32 = 9;
/// Format of the cells' centroid and spread.
const CELL_PLACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;
/// The mip of the bright image that lights the dirt (clamped to the chain): local glare, about
/// 64 viewport pixels per texel.
const DIRT_GLARE_MIP: u32 = 4;
/// The bright mip the starburst spikes gather from (the quarter-size image itself: a coarser mip
/// would fatten the spikes); clamped to the top mip of a small view.
const STARBURST_MIP: u32 = 0;
/// Side of the procedural dirt mask, texels.
const DIRT_SIZE: u32 = 256;
/// Uniform buffers kept for the views of one frame. A texture view does not advance the frame
/// clock, so a host that only draws texture views never starts a new frame: the slots wrap
/// instead of growing, and only a single submit with more lens views than this would share one.
const MAX_VIEW_SLOTS: usize = 16;
/// Fraction of the glare on the front element its dirt scatters toward the sensor at
/// [`LensEffects::dirt_intensity`] 1 (mirrored as `DIRT_SCATTER` in the shader).
pub const DIRT_SCATTER: f32 = 0.02;

/// Process-wide counters of every [`LensEffectsPass`] (the agent's `render.lens_effects`).
#[derive(Debug, Default)]
pub struct LensEffectsCounters {
    /// Views the pass recorded effects for.
    pub views_recorded: AtomicU64,
    /// Views it skipped because their strengths were all 0 (or the slot carried no settings).
    pub views_idle: AtomicU64,
    /// Views whose chromatic aberration was skipped because the colour target cannot be copied.
    pub aberration_skipped: AtomicU64,
    /// Ghosts drawn in the last recorded view.
    pub last_ghosts: AtomicU64,
    /// Sets of bright and ghost images built (one per view size in use; a steady frame builds
    /// none).
    pub targets_built: AtomicU64,
    /// Ghost tables of the built-in lens traced so far (each coating step once).
    pub tables_traced: AtomicU64,
}

/// The process-wide [`LensEffectsCounters`].
#[must_use]
pub fn lens_effects_counters() -> &'static LensEffectsCounters {
    static COUNTERS: LensEffectsCounters = LensEffectsCounters {
        views_recorded: AtomicU64::new(0),
        views_idle: AtomicU64::new(0),
        aberration_skipped: AtomicU64::new(0),
        last_ghosts: AtomicU64::new(0),
        targets_built: AtomicU64::new(0),
        tables_traced: AtomicU64::new(0),
    };
    &COUNTERS
}

/// The bright image's mips for a `width` × `height` view (its quarter-size image, at most
/// `MAX_BRIGHT_MIPS`).
fn bright_mips(width: u32, height: u32) -> u32 {
    let (w, h) = quarter(width, height);
    (32 - w.min(h).leading_zeros()).clamp(1, MAX_BRIGHT_MIPS)
}

/// The quarter-size work images' size of a `width` × `height` view.
fn quarter(width: u32, height: u32) -> (u32, u32) {
    (
        width.max(1).div_ceil(REDUCTION).max(1),
        height.max(1).div_ceil(REDUCTION).max(1),
    )
}

/// What one view of the pass records on a steady frame (ADR-2482), from its effects and size: the
/// counters `tests/lens_effects_alloc.rs` holds the frame to. The first frame also records the
/// dirt upload and, without ghosts, one clear of the ghost image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LensEffectsBudget {
    /// Render passes: the bright image and its mips (flare or dirt), the light cells and the
    /// ghosts (flare), and the composite.
    pub render_passes: u32,
    /// Copies of the HDR image: the frame's per-frame copy every writing `before_post` pass gets
    /// (`FramePass::writes_color`) and the chromatic aberration's own.
    pub copies: u32,
}

impl LensEffectsBudget {
    /// The budget of a `width` × `height` view with `effects` (sanitized first). Nothing for
    /// effects that are all off.
    #[must_use]
    pub fn of(effects: &LensEffects, width: u32, height: u32) -> Self {
        let effects = effects.sanitized();
        if !effects.is_active() {
            return Self::default();
        }
        let lit = effects.flare_active() || effects.dirt_active() || effects.starburst_active();
        let render_passes =
            if lit { bright_mips(width, height) } else { 0 } + if effects.flare_active() { 2 } else { 0 } + 1;
        Self {
            render_passes,
            copies: 1 + u32::from(effects.chromatic_aberration_active()),
        }
    }
}

/// GPU memory a view of the pass holds, in bytes ([`lens_effects_memory`], ADR-2482).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LensEffectsMemory {
    /// The frame's per-frame copy of the HDR image (made for any writing `before_post` pass).
    pub frame_copy: u64,
    /// The chromatic aberration's copy of the HDR image (0 without aberration).
    pub scene_copy: u64,
    /// The quarter-size bright image with its mips.
    pub bright: u64,
    /// The quarter-size ghost image.
    pub ghosts: u64,
    /// The light cells' energy and place images.
    pub cells: u64,
    /// The dirt mask.
    pub dirt: u64,
}

impl LensEffectsMemory {
    /// Every texture byte.
    #[must_use]
    pub fn texture_bytes(&self) -> u64 {
        self.frame_copy + self.scene_copy + self.bright + self.ghosts + self.cells + self.dirt
    }
}

/// The texture memory a `width` × `height` view in `hdr_format` holds with `effects`: what the
/// pass creates for it plus the frame's per-frame copy, all 0 for effects that are all off.
/// The uniform buffers (320 bytes per view slot) are not included.
#[must_use]
pub fn lens_effects_memory(
    width: u32,
    height: u32,
    hdr_format: wgpu::TextureFormat,
    effects: &LensEffects,
) -> LensEffectsMemory {
    let effects = effects.sanitized();
    if !effects.is_active() {
        return LensEffectsMemory::default();
    }
    let texel = |format: wgpu::TextureFormat| u64::from(format.block_copy_size(None).unwrap_or(4));
    let full = u64::from(width.max(1)) * u64::from(height.max(1)) * texel(hdr_format);
    let (w, h) = quarter(width, height);
    let work = texel(WORK_FORMAT);
    let bright = (0..bright_mips(width, height))
        .map(|m| u64::from((w >> m).max(1)) * u64::from((h >> m).max(1)) * work)
        .sum();
    let cells = u64::from(CELLS_X * CELLS_Y) * (work + texel(CELL_PLACE_FORMAT));
    LensEffectsMemory {
        frame_copy: full,
        scene_copy: if effects.chromatic_aberration_active() {
            full
        } else {
            0
        },
        bright,
        ghosts: u64::from(w) * u64::from(h) * work,
        cells,
        dirt: u64::from(DIRT_SIZE * DIRT_SIZE),
    }
}

/// The uniform of `shaders/lens_effects.wgsl` (`LensParams`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LensParams {
    viewport: [f32; 4],
    strengths: [f32; 4],
    info: [f32; 4],
    ghost_scale: [[f32; 4]; MAX_GHOSTS],
    ghost_energy: [[f32; 4]; MAX_GHOSTS],
    extra: [f32; 4],
}

impl LensParams {
    fn new(effects: &LensEffects, table: &GhostTable, viewport: [f32; 4], top_mip: u32) -> Self {
        let mut ghost_scale = [[0.0; 4]; MAX_GHOSTS];
        let mut ghost_energy = [[0.0; 4]; MAX_GHOSTS];
        let count = if effects.flare_active() {
            table.ghosts.len().min(MAX_GHOSTS)
        } else {
            0
        };
        for (i, ghost) in table.ghosts.iter().take(count).enumerate() {
            let (scale, energy) = ghost_uniform(ghost);
            ghost_scale[i] = scale;
            ghost_energy[i] = energy;
        }
        let aspect = if viewport[3] > 0.0 {
            viewport[2] / viewport[3]
        } else {
            1.0
        };
        Self {
            viewport,
            strengths: [
                effects.flare_threshold,
                effects.flare_intensity,
                effects.dirt_intensity,
                effects.chromatic_aberration,
            ],
            info: [count as f32, top_mip as f32, aspect, 0.0],
            ghost_scale,
            ghost_energy,
            extra: [effects.starburst, STARBURST_MIP as f32, 0.0, 0.0],
        }
    }
}

/// A ghost's two uniform rows. A host's table ([`LensEffectsPass::with_ghost_table`]) need not
/// come from [`GhostTable::trace`], so its numbers are forced into range here: a magnification
/// below [`MIN_GHOST_MAGNIFICATION`] or not finite reads as that minimum, and a radius or energy
/// that is not positive and finite as 0.
fn ghost_uniform(ghost: &GhostEntry) -> ([f32; 4], [f32; 4]) {
    let m = ghost.magnification.map(|m| {
        if m.is_finite() && m.abs() >= MIN_GHOST_MAGNIFICATION {
            m
        } else {
            MIN_GHOST_MAGNIFICATION.copysign(m)
        }
    });
    let positive = |v: f32| if v.is_finite() && v > 0.0 { v } else { 0.0 };
    let e = ghost.energy.map(positive);
    (
        [m[0], m[1], m[2], positive(ghost.radius)],
        [e[0], e[1], e[2], 0.0],
    )
}

/// The threshold in the units of the HDR target, for one in exposed luminance
/// ([`LensEffects::flare_threshold`]). The target holds radiance × `pre_exposure`, and the
/// display sees radiance × `exposure`; the pre-exposure moves in whole stops, so a threshold
/// compared in its units would jump by a stop each time an adapting exposure crossed one, and a
/// source near the threshold would pop in and out of the flare.
fn hdr_threshold(threshold: f32, pre_exposure: f32, exposure: Option<f32>) -> f32 {
    match exposure {
        Some(exposure) if exposure.is_finite() && exposure > 0.0 => threshold * pre_exposure / exposure,
        _ => threshold,
    }
}

/// Pipelines, layout and sampler (created on the first frame that asks for an effect).
struct Pipelines {
    bgl: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
    bright: wgpu::RenderPipeline,
    down: wgpu::RenderPipeline,
    cells: wgpu::RenderPipeline,
    sprites: wgpu::RenderPipeline,
    cells_bgl: wgpu::BindGroupLayout,
    /// `fs_add` and `fs_replace` per colour format.
    composite: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline, wgpu::RenderPipeline)>,
    sampler: wgpu::Sampler,
}

/// Size-dependent targets.
struct Targets {
    _bright: wgpu::Texture,
    bright_all: wgpu::TextureView,
    bright_mips: Vec<wgpu::TextureView>,
    _flare: wgpu::Texture,
    flare: wgpu::TextureView,
    /// The ghost image holds only its clear (a frame without ghosts cleared it and none drew
    /// since): a later frame without ghosts skips the clear pass (ADR-2482).
    flare_clear: AtomicBool,
    _cell_energy: wgpu::Texture,
    cell_energy: wgpu::TextureView,
    _cell_place: wgpu::Texture,
    cell_place: wgpu::TextureView,
}

/// The copy of the colour target the chromatic aberration reads.
struct SceneCopy {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// The lens effects pass. [`FrameRenderer::enable_lens_effects`] registers it; the runtime
/// renderer does at start.
#[derive(Default)]
pub struct LensEffectsPass {
    pipelines: Option<Pipelines>,
    /// One set per view size in use.
    targets: PerSize<(u32, u32), Targets>,
    /// One copy per colour target size and format in use.
    copies: PerSize<(wgpu::Extent3d, wgpu::TextureFormat), SceneCopy>,
    /// The dirt mask and its view.
    dirt: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// The buffer the dirt mask was uploaded from and the frame that recorded the copy: kept
    /// until a later frame starts (the copy is in that frame's submit), then dropped (ADR-2482).
    dirt_upload: Option<(wgpu::Buffer, u64)>,
    /// One uniform buffer per view recorded in a frame (each view writes its own before the
    /// single submit), with the parameters it holds: an unchanged view writes nothing.
    uniforms: Vec<(wgpu::Buffer, Option<LensParams>)>,
    frame: Option<u64>,
    views_this_frame: usize,
    /// A ghost table the host traced from its own lens ([`Self::with_ghost_table`]); `None`
    /// uses the built-in lens at each view's coating.
    table: Option<Arc<GhostTable>>,
    bind_groups: BindCache<(usize, u8, wgpu::TextureView, wgpu::TextureView)>,
}

impl LensEffectsPass {
    /// A pass with nothing created yet, drawing the built-in lens ([`super::default_flare_lens`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A pass that draws `table` instead of the built-in lens: a host's own lens, traced with
    /// [`GhostTable::trace`]. The view's [`LensEffects::coating`] then has no effect (the table
    /// was traced at one coating); at most [`MAX_GHOSTS`] of its ghosts draw.
    #[must_use]
    pub fn with_ghost_table(table: GhostTable) -> Self {
        Self {
            table: Some(Arc::new(table)),
            ..Self::default()
        }
    }

    fn ensure_pipelines(
        &mut self,
        device: &TrackedDevice,
        format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        if self.pipelines.is_none() {
            let module = crate::lens::shader_module(
                device,
                "lens effects",
                include_str!("../../shaders/lens_effects.wgsl"),
                "lens_effects.wgsl",
                &[],
            )?;
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("lens effects bgl"),
                entries: &[
                    crate::lens::color_entry(0),
                    crate::lens::color_entry(1),
                    crate::lens::color_entry(2),
                    crate::lens::color_entry(3),
                    crate::lens::sampler_entry(4),
                    wgpu::BindGroupLayoutEntry {
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ..crate::lens::uniform_entry(5)
                    },
                ],
            });
            let cell_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            };
            let cells_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("lens effects cells bgl"),
                entries: &[cell_entry(0), cell_entry(1)],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("lens effects layout"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let sprite_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("lens effects sprite layout"),
                bind_group_layouts: &[Some(&bgl), Some(&cells_bgl)],
                immediate_size: 0,
            });
            let work = |label: &str, entry: &str| {
                crate::lens::fullscreen_pipeline_formats(
                    device,
                    label,
                    &layout,
                    &module,
                    entry,
                    &[WORK_FORMAT],
                )
            };
            let bright = work("lens effects bright", "fs_bright");
            let down = work("lens effects downsample", "fs_down");
            let cells = crate::lens::fullscreen_pipeline_formats(
                device,
                "lens effects cells",
                &layout,
                &module,
                "fs_cells",
                &[WORK_FORMAT, CELL_PLACE_FORMAT],
            );
            let sprites = sprite_pipeline(device, &sprite_layout, &module);
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("lens effects sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                ..Default::default()
            });
            self.pipelines = Some(Pipelines {
                bgl,
                layout,
                module,
                bright,
                down,
                cells,
                sprites,
                cells_bgl,
                composite: Vec::new(),
                sampler,
            });
        }
        let Some(pipelines) = self.pipelines.as_mut() else {
            return Err(RenderError::Shader("lens effects pipelines missing".into()));
        };
        if !pipelines.composite.iter().any(|(f, _, _)| *f == format) {
            let add = composite_pipeline(device, pipelines, format, "fs_add", true);
            let replace = composite_pipeline(device, pipelines, format, "fs_replace", false);
            pipelines.composite.push((format, add, replace));
        }
        Ok(())
    }

    /// Starts a frame's per-view state when `frame_index` moved on: the uniform slots start over
    /// and resources no view asked for lately go. Runs on idle views too, so a pass whose
    /// effects were turned off lets its images go.
    fn begin_view(&mut self, frame_index: u64) {
        if self.frame == Some(frame_index) {
            return;
        }
        self.frame = Some(frame_index);
        self.views_this_frame = 0;
        if self
            .dirt_upload
            .as_ref()
            .is_some_and(|(_, recorded)| *recorded != frame_index)
        {
            self.dirt_upload = None;
        }
        let targets_went = self.targets.evict(frame_index);
        let copies_went = self.copies.evict(frame_index);
        if targets_went || copies_went {
            self.bind_groups.clear();
        }
    }

    /// The uniform slot of the view being recorded ([`MAX_VIEW_SLOTS`]).
    fn next_view_slot(&mut self) -> usize {
        let slot = self.views_this_frame % MAX_VIEW_SLOTS;
        self.views_this_frame = self.views_this_frame.wrapping_add(1);
        slot
    }

    /// Makes sure the view's targets exist; returns their key in [`Self::targets`].
    fn ensure_targets(&mut self, device: &TrackedDevice, viewport: [f32; 4], frame: u64) -> (u32, u32) {
        let w = (viewport[2].max(1.0) as u32).div_ceil(REDUCTION).max(1);
        let h = (viewport[3].max(1.0) as u32).div_ceil(REDUCTION).max(1);
        let _ = self
            .targets
            .get_or_insert_with((w, h), frame, || build_targets(device, w, h));
        (w, h)
    }

    /// The dirt mask, created and uploaded (through the frame's encoder) on first use.
    fn ensure_dirt(&mut self, ctx: &mut FramePassContext<'_>) {
        if self.dirt.is_some() {
            return;
        }
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lens effects dirt"),
            size: wgpu::Extent3d {
                width: DIRT_SIZE,
                height: DIRT_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let texels = dirt_mask(DIRT_SIZE);
        let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lens effects dirt upload"),
            size: texels.len() as u64,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.write_buffer(&staging, 0, &texels);
        ctx.encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(DIRT_SIZE),
                    rows_per_image: Some(DIRT_SIZE),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: DIRT_SIZE,
                height: DIRT_SIZE,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.dirt = Some((texture, view));
        self.dirt_upload = Some((staging, ctx.frame_index));
    }

    /// A copy of the colour target (same size and format) for the chromatic aberration; `None`
    /// when the target cannot be copied.
    fn copy_color(&mut self, ctx: &mut FramePassContext<'_>) -> Option<wgpu::TextureView> {
        let source = ctx.color.texture();
        if !source.usage().contains(wgpu::TextureUsages::COPY_SRC)
            || source.sample_count() != 1
            || source.mip_level_count() != 1
            || source.depth_or_array_layers() != 1
        {
            return None;
        }
        let size = source.size();
        let format = source.format();
        let device = ctx.device;
        let copy = self
            .copies
            .get_or_insert_with((size, format), ctx.frame_index, || {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("lens effects scene copy"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                SceneCopy { texture, view }
            });
        ctx.encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: source,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            copy.texture.as_image_copy(),
            wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
        );
        Some(copy.view.clone())
    }

    /// The uniform buffer of this frame's `index`-th view, with `params` written into it unless
    /// it already holds them (a steady view writes nothing, ADR-2482).
    fn uniform(&mut self, ctx: &FramePassContext<'_>, index: usize, params: &LensParams) -> wgpu::Buffer {
        while self.uniforms.len() <= index {
            self.uniforms.push((
                ctx.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("lens effects params"),
                    size: std::mem::size_of::<LensParams>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                None,
            ));
        }
        let (buffer, held) = &mut self.uniforms[index];
        if held
            .as_ref()
            .is_none_or(|held| bytemuck::bytes_of(held) != bytemuck::bytes_of(params))
        {
            ctx.write_buffer(buffer, 0, bytemuck::bytes_of(params));
            *held = Some(*params);
        }
        buffer.clone()
    }

    /// Records the effects of one view (the strengths are already known to be active).
    fn record_view(&mut self, ctx: &mut FramePassContext<'_>, effects: &LensEffects) -> Result<(), String> {
        let _span = tracing::info_span!("lens_effects").entered();
        let _crumb = crate::breadcrumbs::frame_pass("lens_effects");
        if ctx.color_space != SlotColorSpace::SceneLinearHdr {
            return Err("lens effects need the scene-referred HDR target".into());
        }
        if !matches!(
            ctx.color_format,
            wgpu::TextureFormat::Rgba16Float | wgpu::TextureFormat::Rg11b10Ufloat
        ) {
            return Err(format!(
                "lens effects: unsupported HDR format {:?}",
                ctx.color_format
            ));
        }
        let table = match self.table.as_ref() {
            Some(table) => Arc::clone(table),
            None => default_ghost_table(effects.coating).map_err(|err| format!("lens flare trace: {err}"))?,
        };
        let slot = self.next_view_slot();
        let viewport = ctx.viewport;
        self.ensure_pipelines(ctx.device, ctx.color_format)
            .map_err(|err| err.to_string())?;
        let targets_key = self.ensure_targets(ctx.device, viewport, ctx.frame_index);
        self.ensure_dirt(ctx);
        let scene_copy = if effects.chromatic_aberration_active() {
            let copy = self.copy_color(ctx);
            if copy.is_none() {
                lens_effects_counters()
                    .aberration_skipped
                    .fetch_add(1, Ordering::Relaxed);
            }
            copy
        } else {
            None
        };
        let top_mip = self
            .targets
            .get(&targets_key)
            .map_or(0, |targets| targets.bright_mips.len().saturating_sub(1) as u32);
        let mut params = LensParams::new(effects, &table, viewport, top_mip);
        params.strengths[0] = hdr_threshold(effects.flare_threshold, ctx.pre_exposure, ctx.exposure);
        params.info[3] = DIRT_GLARE_MIP.min(top_mip) as f32;
        if scene_copy.is_none() {
            params.strengths[3] = 0.0;
        }
        let uniform = self.uniform(ctx, slot, &params);
        let (Some(pipelines), Some(targets), Some((_, dirt))) = (
            self.pipelines.as_ref(),
            self.targets.get(&targets_key),
            self.dirt.as_ref(),
        ) else {
            return Err("lens effects resources missing".into());
        };
        let ghosts_drawn = params.info[0] as u64;

        let device = ctx.device;
        let bind = |cache: &mut BindCache<(usize, u8, wgpu::TextureView, wgpu::TextureView)>,
                    step: u8,
                    textures: [&wgpu::TextureView; 4]| {
            cache.get_or_insert_with((slot, step, textures[0].clone(), textures[1].clone()), || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("lens effects bind group"),
                    layout: &pipelines.bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(textures[0]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(textures[1]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(textures[2]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(textures[3]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::Sampler(&pipelines.sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: uniform.as_entire_binding(),
                        },
                    ],
                })
            })
        };
        // Every pass binds four textures; the ones it does not read are filled with a texture
        // it does not write (a texture cannot be sampled and rendered to in one pass).
        // One group per bright mip, in a fixed array: a steady frame allocates nothing here.
        let mut groups: [Option<wgpu::BindGroup>; MAX_BRIGHT_MIPS as usize] = Default::default();
        for (level, group) in groups.iter_mut().enumerate().take(targets.bright_mips.len()) {
            *group = Some(if level == 0 {
                bind(
                    &mut self.bind_groups,
                    0,
                    [ctx.color, &targets.flare, &targets.flare, dirt],
                )
            } else {
                bind(
                    &mut self.bind_groups,
                    level as u8,
                    [&targets.bright_mips[level - 1], dirt, &targets.flare, dirt],
                )
            });
        }
        let ghosts_group = bind(
            &mut self.bind_groups,
            100,
            [dirt, &targets.bright_all, dirt, dirt],
        );
        let cells_group = self.bind_groups.get_or_insert_with(
            (slot, 103, targets.cell_energy.clone(), targets.cell_place.clone()),
            || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("lens effects cells"),
                    layout: &pipelines.cells_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&targets.cell_energy),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&targets.cell_place),
                        },
                    ],
                })
            },
        );
        let composite_group = match scene_copy.as_ref() {
            Some(copy) => bind(
                &mut self.bind_groups,
                101,
                [copy, &targets.bright_all, &targets.flare, dirt],
            ),
            None => bind(
                &mut self.bind_groups,
                102,
                [dirt, &targets.bright_all, &targets.flare, dirt],
            ),
        };

        // Bright image and its mips: only the flare and the dirt read them (with neither, the
        // composite multiplies the old, finite contents by a zero dirt strength, and the ghost
        // pass below clears its own target).
        let lit = effects.flare_active() || effects.dirt_active() || effects.starburst_active();
        for (level, group) in groups.iter().enumerate().filter(|_| lit) {
            let Some(group) = group.as_ref() else {
                continue;
            };
            let mut pass = begin_cleared(ctx.encoder, "lens effects bright", &[&targets.bright_mips[level]]);
            pass.counted_set_pipeline(if level == 0 {
                &pipelines.bright
            } else {
                &pipelines.down
            });
            pass.counted_set_bind_group(0, group, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        // The light cells, then one sprite per ghost and cell (a dark cell's collapse).
        if ghosts_drawn > 0 {
            let mut pass = begin_cleared(
                ctx.encoder,
                "lens effects cells",
                &[&targets.cell_energy, &targets.cell_place],
            );
            pass.counted_set_pipeline(&pipelines.cells);
            pass.counted_set_bind_group(0, &ghosts_group, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        // Zero ghosts just clears the target: the dirt alone still reads it. A target that
        // holds nothing but that clear already (no ghost drew into it since) skips the pass.
        if ghosts_drawn > 0 || !targets.flare_clear.load(Ordering::Relaxed) {
            let mut pass = begin_cleared(ctx.encoder, "lens effects ghosts", &[&targets.flare]);
            if ghosts_drawn > 0 {
                pass.counted_set_pipeline(&pipelines.sprites);
                pass.counted_set_bind_group(0, &ghosts_group, &[]);
                pass.counted_set_bind_group(1, &cells_group, &[]);
                pass.counted_draw(0..6, 0..(ghosts_drawn as u32 * CELLS_X * CELLS_Y));
            }
            targets.flare_clear.store(ghosts_drawn == 0, Ordering::Relaxed);
        }
        // Composite into the colour target, inside the viewport only.
        let Some((_, add, replace)) = pipelines
            .composite
            .iter()
            .find(|(f, _, _)| *f == ctx.color_format)
        else {
            return Err("lens effects composite pipeline missing".into());
        };
        let mut pass = ctx.encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("lens effects composite"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: ctx.color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let [x, y, w, h] = viewport;
        pass.set_viewport(x, y, w.max(1.0), h.max(1.0), 0.0, 1.0);
        let size = ctx.color.texture().size();
        let x0 = (x.max(0.0) as u32).min(size.width.saturating_sub(1));
        let y0 = (y.max(0.0) as u32).min(size.height.saturating_sub(1));
        let sw = (w.max(1.0) as u32).min(size.width - x0).max(1);
        let sh = (h.max(1.0) as u32).min(size.height - y0).max(1);
        pass.set_scissor_rect(x0, y0, sw, sh);
        pass.counted_set_pipeline(if scene_copy.is_some() { replace } else { add });
        pass.counted_set_bind_group(0, &composite_group, &[]);
        pass.counted_draw(0..3, 0..1);
        drop(pass);
        let counters = lens_effects_counters();
        counters.views_recorded.fetch_add(1, Ordering::Relaxed);
        counters.last_ghosts.store(ghosts_drawn, Ordering::Relaxed);
        Ok(())
    }
}

/// A render pass that clears `targets` (at most two) to transparent, like
/// `crate::lens::begin_fullscreen`, with its attachments in a fixed array instead of a `Vec`.
fn begin_cleared<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    label: &str,
    targets: &[&wgpu::TextureView],
) -> wgpu::RenderPass<'e> {
    fn attachment<'v>(view: &'v wgpu::TextureView) -> wgpu::RenderPassColorAttachment<'v> {
        wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        }
    }
    let attachments = [
        targets.first().map(|view| attachment(view)),
        targets.get(1).map(|view| attachment(view)),
    ];
    let used = targets.len().min(attachments.len());
    encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &attachments[..used],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// The bright image with its mips and the ghost image of a `w` × `h` quarter-size view.
fn build_targets(device: &TrackedDevice, w: u32, h: u32) -> Targets {
    let mips = (32 - w.min(h).leading_zeros()).clamp(1, MAX_BRIGHT_MIPS);
    let texture = |label: &str, levels: u32| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: WORK_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    };
    let bright = texture("lens effects bright", mips);
    let bright_all = bright.create_view(&wgpu::TextureViewDescriptor::default());
    let bright_mips = (0..mips)
        .map(|level| {
            bright.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        })
        .collect();
    let flare = texture("lens effects ghosts", 1);
    let flare_view = flare.create_view(&wgpu::TextureViewDescriptor::default());
    let cell_texture = |label: &str, format: wgpu::TextureFormat| {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: CELLS_X,
                height: CELLS_Y,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    };
    let (cell_energy_texture, cell_energy) = cell_texture("lens effects cell energy", WORK_FORMAT);
    let (cell_place_texture, cell_place) = cell_texture("lens effects cell place", CELL_PLACE_FORMAT);
    lens_effects_counters()
        .targets_built
        .fetch_add(1, Ordering::Relaxed);
    Targets {
        _bright: bright,
        bright_all,
        bright_mips,
        _flare: flare,
        flare: flare_view,
        flare_clear: AtomicBool::new(false),
        _cell_energy: cell_energy_texture,
        cell_energy,
        _cell_place: cell_place_texture,
        cell_place,
    }
}

/// The ghost sprites: `vs_sprite` and `fs_sprite`, additive onto the ghost image.
fn sprite_pipeline(
    device: &TrackedDevice,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
) -> wgpu::RenderPipeline {
    let add = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("lens effects sprites"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_sprite"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_sprite"),
            targets: &[Some(wgpu::ColorTargetState {
                format: WORK_FORMAT,
                blend: Some(wgpu::BlendState {
                    color: add,
                    alpha: add,
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(device).as_deref(),
    })
}

/// `fs_add` (additive onto the target, its alpha kept) or `fs_replace` (writes every channel).
fn composite_pipeline(
    device: &TrackedDevice,
    pipelines: &Pipelines,
    format: wgpu::TextureFormat,
    entry: &str,
    additive: bool,
) -> wgpu::RenderPipeline {
    let blend = additive.then_some(wgpu::BlendState {
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
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("lens effects composite"),
        layout: Some(&pipelines.layout),
        vertex: wgpu::VertexState {
            module: &pipelines.module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &pipelines.module,
            entry_point: Some(entry),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(device).as_deref(),
    })
}

impl FramePass for LensEffectsPass {
    fn name(&self) -> &str {
        LENS_EFFECTS_PASS_NAME
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::BeforePost
    }

    fn reads(&self) -> &[&'static str] {
        &["color"]
    }

    /// Only a view that asks for an effect writes (and gets the frame's per-frame copy).
    fn writes_color(&self, post: Option<&crate::PostSettings>) -> bool {
        post.is_some_and(|post| post.lens_effects.sanitized().is_active())
    }

    fn writes(&self) -> &[&'static str] {
        &["color"]
    }

    /// The pipelines for the desktop HDR format (`Rgba16Float`; the presets that allow the pass
    /// all use it) and the built-in lens's table at the default coating, so the first frame that
    /// asks for an effect compiles and traces nothing. Nothing on Android, where no preset allows
    /// the pass.
    fn prewarm(&mut self, device: &TrackedDevice) -> Result<usize, String> {
        if cfg!(target_os = "android") {
            return Ok(0);
        }
        self.ensure_pipelines(device, wgpu::TextureFormat::Rgba16Float)
            .map_err(|err| err.to_string())?;
        if self.table.is_none() {
            default_ghost_table(LensEffects::OFF.coating)
                .map_err(|err| format!("lens flare trace: {err}"))?;
        }
        Ok(6)
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        self.begin_view(ctx.frame_index);
        let effects = ctx.post.map(|post| post.lens_effects.sanitized());
        match effects.filter(LensEffects::is_active) {
            Some(effects) => self.record_view(ctx, &effects),
            None => {
                lens_effects_counters().views_idle.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

impl FrameRenderer {
    /// Registers the lens effects pass ([`LensEffectsPass`], ADR-1863) when it is not registered
    /// yet. It records only on views whose post settings ask for an effect. Returns `true` when
    /// it registered the pass now.
    pub fn enable_lens_effects(&mut self) -> bool {
        if self
            .registered_passes()
            .iter()
            .any(|name| name == LENS_EFFECTS_PASS_NAME)
        {
            return false;
        }
        self.register_pass(Box::new(LensEffectsPass::new())).is_ok()
    }
}

/// Deterministic hash of `i` to `0..1`.
fn hash01(i: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^= x >> 16;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

/// The procedural dirt mask: soft specks of dust and a few wide smudges on a faint haze, `size`²
/// R8 texels. Deterministic (a fixed hash sequence). The specks are squeezed vertically by 9/16,
/// so on a 16:9 frame (the mask is stretched over the viewport) they read as round.
#[must_use]
pub(crate) fn dirt_mask(size: u32) -> Vec<u8> {
    const SPECKS: u32 = 160;
    const SMUDGES: u32 = 10;
    let n = size as usize;
    let mut field = vec![0.06f32; n * n];
    let mut stamp = |cx: f32, cy: f32, radius: f32, strength: f32| {
        let rx = radius * size as f32;
        let ry = rx * 16.0 / 9.0;
        let (x0, x1) = (
            ((cx * size as f32) - rx).floor().max(0.0) as usize,
            ((cx * size as f32) + rx).ceil().min(size as f32) as usize,
        );
        let (y0, y1) = (
            ((cy * size as f32) - ry).floor().max(0.0) as usize,
            ((cy * size as f32) + ry).ceil().min(size as f32) as usize,
        );
        for y in y0..y1 {
            for x in x0..x1 {
                let dx = (x as f32 + 0.5 - cx * size as f32) / rx;
                let dy = (y as f32 + 0.5 - cy * size as f32) / ry;
                let d2 = dx * dx + dy * dy;
                if d2 < 1.0 {
                    let falloff = (1.0 - d2) * (1.0 - d2);
                    field[y * n + x] += strength * falloff;
                }
            }
        }
    };
    for i in 0..SMUDGES {
        let k = 1000 + i * 4;
        stamp(
            hash01(k),
            hash01(k + 1),
            0.08 + 0.12 * hash01(k + 2),
            0.12 + 0.18 * hash01(k + 3),
        );
    }
    for i in 0..SPECKS {
        let k = i * 4;
        stamp(
            hash01(k),
            hash01(k + 1),
            0.004 + 0.02 * hash01(k + 2),
            0.3 + 0.7 * hash01(k + 3),
        );
    }
    field
        .into_iter()
        .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
        .collect()
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "lens_effects.wgsl",
        "LensParams",
        LensParams {
            viewport,
            strengths,
            info,
            ghost_scale,
            ghost_energy,
            extra
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uniform_matches_the_shader_layout() {
        // viewport, strengths, info, then two arrays of eight vec4, then extra (ADR-3196).
        assert_eq!(std::mem::size_of::<LensParams>(), 16 * (3 + 2 * MAX_GHOSTS + 1));
        let source = include_str!("../../shaders/lens_effects.wgsl");
        assert!(source.contains(&format!("const MAX_GHOSTS: u32 = {MAX_GHOSTS}u;")));
        assert!(source.contains("ghost_scale: array<vec4<f32>, 8>"));
        assert!(source.contains("extra: vec4<f32>,"));
        assert!(source.contains(&format!("const DIRT_SCATTER: f32 = {DIRT_SCATTER};")));
        assert!(source.contains(&format!("const CELLS_X: u32 = {CELLS_X}u;")));
        assert!(source.contains(&format!("const CELLS_Y: u32 = {CELLS_Y}u;")));
        assert!(source.contains(&format!("const REDUCTION: f32 = {REDUCTION}.0;")));
    }

    /// Spec 30 § 9's numbers, by value (the layout test above only checks that the shader mirrors
    /// the Rust constants): a 16 × 9 cell grid, dirt lit by mip 4 with a 0.02 scatter, added light
    /// capped at 4096 per texel, and chromatic aberration sampling red at `0.5 + d (1 + a)` and
    /// blue at `0.5 + d (1 − a)` (lane spec-12-29-spacemouse, ADR-5631).
    #[test]
    fn the_pass_uses_the_spec_numbers() {
        assert_eq!((CELLS_X, CELLS_Y), (16, 9));
        assert_eq!(DIRT_GLARE_MIP, 4);
        // The dirt's mip exists in the chain.
        const { assert!(DIRT_GLARE_MIP < MAX_BRIGHT_MIPS) };
        assert_eq!(DIRT_SCATTER, 0.02);
        let source = include_str!("../../shaders/lens_effects.wgsl");
        assert!(source.contains("const MAX_LENS_LIGHT: f32 = 4096.0;"));
        assert!(
            source.matches("MAX_LENS_LIGHT").count() >= 2,
            "the cap is declared and used"
        );
        assert!(
            source.contains("source_uv(0.5 + d * (1.0 + ca))"),
            "red is magnified"
        );
        assert!(
            source.contains("source_uv(0.5 + d * (1.0 - ca))"),
            "blue is minified"
        );
    }

    #[test]
    fn view_slots_stay_bounded_when_the_frame_clock_stands_still() {
        // Only texture views: the frame index never moves, the buffers must not pile up.
        let mut pass = LensEffectsPass::new();
        pass.begin_view(7);
        let most = (0..1000).map(|_| pass.next_view_slot()).max();
        assert_eq!(most, Some(MAX_VIEW_SLOTS - 1));
        pass.begin_view(7);
        assert_ne!(pass.next_view_slot(), 0, "the same frame goes on");
        pass.begin_view(8);
        assert_eq!(pass.next_view_slot(), 0, "a new frame starts over");
    }

    #[test]
    fn the_threshold_follows_the_exposure_across_a_pre_exposure_step() {
        // A stop of pre-exposure moves the HDR target's values; the same exposed threshold
        // moves with them, so what crosses it does not change.
        let exposure = 0.18;
        let below = hdr_threshold(4.0, 0.25, Some(exposure));
        let above = hdr_threshold(4.0, 0.125, Some(exposure));
        assert!((below / above - 2.0).abs() < 1.0e-6);
        assert!((below - 4.0 * 0.25 / exposure).abs() < 1.0e-5);
        // Without an exposure (outside `before_post`) or a usable one it stays as given.
        for exposure in [None, Some(0.0), Some(f32::NAN), Some(f32::INFINITY)] {
            assert_eq!(hdr_threshold(4.0, 0.25, exposure), 4.0);
        }
    }

    #[test]
    fn a_host_table_with_bad_numbers_cannot_reach_the_shader() {
        let ghost = GhostEntry {
            first_bounce: 0,
            second_bounce: 1,
            energy: [f32::NAN, f32::INFINITY, -1.0],
            magnification: [0.0, -0.01, f32::NAN],
            radius: f32::NAN,
        };
        let (scale, energy) = ghost_uniform(&ghost);
        for value in scale.iter().chain(&energy) {
            assert!(value.is_finite());
        }
        for m in &scale[..3] {
            assert!(m.abs() >= MIN_GHOST_MAGNIFICATION);
        }
        assert!(scale[1] < 0.0, "the side of the centre is kept");
        assert_eq!(energy, [0.0; 4]);
        assert_eq!(scale[3], 0.0);
        // A valid ghost passes through unchanged.
        let good = GhostEntry {
            energy: [0.1, 0.2, 0.3],
            magnification: [-0.5, 0.6, 7.0],
            radius: 0.1,
            ..ghost
        };
        assert_eq!(
            ghost_uniform(&good),
            ([-0.5, 0.6, 7.0, 0.1], [0.1, 0.2, 0.3, 0.0])
        );
    }

    #[test]
    fn params_carry_no_ghosts_without_flare() {
        let table = default_ghost_table(1.0).expect("table");
        let dirt_only = LensEffects {
            dirt_intensity: 1.0,
            ..LensEffects::OFF
        };
        let params = LensParams::new(&dirt_only, &table, [0.0, 0.0, 64.0, 32.0], 3);
        assert_eq!(params.info[0], 0.0);
        assert_eq!(params.info[2], 2.0);
        let flare = LensParams::new(&LensEffects::CINEMATIC, &table, [0.0, 0.0, 64.0, 32.0], 3);
        assert_eq!(flare.info[0] as usize, table.ghosts.len().min(MAX_GHOSTS));
        assert_eq!(flare.ghost_scale[0][1], table.ghosts[0].magnification[1]);
    }

    #[test]
    fn the_dirt_mask_is_deterministic_and_not_flat() {
        let a = dirt_mask(64);
        assert_eq!(a, dirt_mask(64));
        assert_eq!(a.len(), 64 * 64);
        let min = a.iter().copied().min().unwrap_or(0);
        let max = a.iter().copied().max().unwrap_or(0);
        assert!(max > min + 100, "specks stand out: {min}..{max}");
        let mean = a.iter().map(|&v| f32::from(v)).sum::<f32>() / a.len() as f32;
        assert!(mean < 128.0, "mostly clean glass: mean {mean}");
    }

    #[test]
    fn hash_is_in_the_unit_interval() {
        for i in 0..1000 {
            let h = hash01(i);
            assert!((0.0..1.0).contains(&h));
        }
    }
}
