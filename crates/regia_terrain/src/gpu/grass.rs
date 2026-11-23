// SPDX-License-Identifier: MPL-2.0
//! GPU side of the grass field (ADR-1400).
//!
//! [`GrassLayer`] owns a [`GrassSet`], its baked [`GrassMask`] (an `Rgba8Unorm` texture array,
//! one layer per class) and the [`TerrainGrassHandle`] the host and the agent drive it through.
//! [`crate::TerrainSharedDraw::set_grass`] moves it into the terrain draw. Each frame (main
//! camera only):
//!
//! 1. `prepare`: resolve the tier ([`GrassQuality`]: the request's, else the frame profile's),
//!    select the patches around the eye ([`crate::grass::select`]) and upload them with the
//!    frame parameters;
//! 2. `encode` (start of the frame): the three generator dispatches of `grass_cull.wgsl`
//!    (count, scan, write) fill four instance regions (near, mid, far LOD; the shadow set)
//!    and their indirect draws;
//! 3. `draw`: one indexed indirect draw per region in the shared depth, colour and velocity
//!    passes after the terrain, and the shadow region into the finest sun cascades
//!    (`grass.wgsl`). Nothing is drawn into the decal receiver: decals there see the terrain's
//!    clear (a scorch decal darkens the blades over it).
//!
//! Nothing runs while no layer is bound or the tier is [`GrassQuality::Off`]: the terrain
//! records exactly what it recorded before grass existed.

use regia_render::{SharedFrameInfo, SharedPass, SharedUpload};

use super::TerrainGpu;
use crate::grass::select::{
    ClassRange, GrassPatch, GrassSelectCache, GrassView, MAX_GRASS_PATCHES, PATCH_CAMERA, PATCH_NO_CAMERA,
    PATCH_SHADOW,
};
use crate::grass::{
    GrassBudget, GrassError, GrassMask, GrassQuality, GrassRequest, GrassSet, MAX_GRASS_CLASSES,
    TerrainGrassHandle, candidates_per_fine_patch,
};
use crate::sampler::TerrainMap;
use crate::wind::TerrainWind;

/// `GrassParams` in `grass_common.wgsl` (432 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct GrassParams {
    planes: [[f32; 4]; 6],
    eye: [f32; 4],
    map: [f32; 4],
    heights: [f32; 4],
    law: [f32; 4],
    lod: [f32; 4],
    /// The camera-body wake ([`crate::grass::GrassWake::gpu_rows`], ADR-1502).
    wake: [[f32; 4]; crate::grass::GRASS_WAKE_ROWS],
    counts: [u32; 4],
    caps: [u32; 4],
    counts2: [u32; 4],
    /// Wind direction x, z (the shader normalises), frequency scale, gust weight (ADR-1853).
    wind: [f32; 4],
    /// Foliage transmission (ADR-3170): on (1/0), blade thickness (mm), unused ×2.
    subsurface: [f32; 4],
    /// The foliage profile's Gaussians ([`foliage_transmission_words`]).
    subsurface_g: [[f32; 4]; 2],
    /// Blast gust (ADR-1497): centre x, z, front radius (m), bend (0 = no gust).
    gust: [f32; 4],
    /// Front radius the frame before (m), positive-phase length (m), unused ×2.
    gust2: [f32; 4],
}

/// Blade thickness the foliage transmission passes through at normal incidence, millimeters: a
/// grass leaf (ADR-3170). The path toward the sun is this over `|n · l|`, at most ten times it.
pub const GRASS_BLADE_THICKNESS_MM: f32 = 0.3;

/// The foliage profile ([`regia_render::subsurface::FOLIAGE_PROFILE`]) as the grass shader reads it:
/// per Gaussian, the RGB weights normalised over the profile and the variance in mm² at the
/// profile's own reach. `grass_transmission` in `grass.wgsl` with these is
/// [`regia_render::subsurface::slab_transmission`].
#[must_use]
pub fn foliage_transmission_words() -> [[f32; 4]; 2] {
    let profile = regia_render::subsurface::FOLIAGE_PROFILE;
    let total = [0, 1, 2].map(|ch| profile.iter().map(|(_, w)| w[ch]).sum::<f32>().max(1e-6));
    profile.map(|(variance, w)| [w[0] / total[0], w[1] / total[1], w[2] / total[2], variance])
}

/// `GrassClassGpu` in `grass_common.wgsl` (128 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct GrassClassGpu {
    shape: [f32; 4],
    shape2: [f32; 4],
    base: [f32; 4],
    tip: [f32; 4],
    green: [f32; 4],
    green_tip: [f32; 4],
    misc: [f32; 4],
    seed: [u32; 4],
}

/// `GrassDrawArgs` in `grass_cull.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
struct GrassDrawArgs {
    index_count: [u32; 4],
    first_index: [u32; 4],
}

/// `GrassLod` in `grass.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
struct GrassLod {
    region: u32,
    blades: u32,
    segments: u32,
    base: u32,
}

/// Instance regions: near, mid, far LOD, shadow.
const REGIONS: usize = 4;
/// Shadow region.
const SHADOW: usize = 3;
/// Words before the per-patch counts in the state buffer (`STATE_COUNTS` in the WGSL).
const STATE_COUNTS: u64 = 64;
/// Bytes of one instance (`GrassInst`).
const INSTANCE_BYTES: u64 = 32;
/// Instance capacity of each region as a fraction of the tier's `max_instances`, in fifths.
const REGION_FIFTHS: [u32; REGIONS] = [1, 2, 2, 2];

/// Blades a clump of `blades` draws in `region` (`grass_region_blades` in `grass.wgsl`).
fn region_blades(blades: u32, max_blades: u32, region: usize) -> u32 {
    let near = blades.min(max_blades).max(1);
    match region {
        0 => near,
        2 => near.div_ceil(4).max(1),
        _ => near.div_ceil(2).max(1),
    }
}

/// Segments of each region (the shadow region draws the mid LOD).
fn region_segments(budget: &GrassBudget) -> [u32; REGIONS] {
    let s = budget.segments.map(|s| s.clamp(1, 4));
    [s[0], s[1], s[2], s[1]]
}

/// Appends the static blade index list of `blades` blades with `segments` segments each
/// (`grass.wgsl`: `2 · segments + 1` vertices per blade, a quad per segment but the last, a tip
/// triangle).
pub(crate) fn extend_blade_indices(out: &mut Vec<u16>, blades: u32, segments: u32) {
    let per_blade = 2 * segments + 1;
    for b in 0..blades {
        let o = b * per_blade;
        for r in 0..segments.saturating_sub(1) {
            let a = o + 2 * r;
            out.extend_from_slice(&[a, a + 1, a + 2, a + 1, a + 3, a + 2].map(|v| v as u16));
        }
        let a = o + 2 * (segments - 1);
        out.extend_from_slice(&[a, a + 1, o + 2 * segments].map(|v| v as u16));
    }
}

/// The static blade index list of `blades` blades with `segments` segments each (`grass.wgsl`:
/// `2 · segments + 1` vertices per blade, a quad per segment but the last, a tip triangle).
#[cfg(test)]
pub(crate) fn blade_indices(blades: u32, segments: u32) -> Vec<u16> {
    let mut out = Vec::with_capacity((blades * (2 * segments - 1) * 3) as usize);
    extend_blade_indices(&mut out, blades, segments);
    out
}

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

struct GrassPipelines {
    format: wgpu::TextureFormat,
    depth: wgpu::RenderPipeline,
    color: wgpu::RenderPipeline,
    velocity: wgpu::RenderPipeline,
    shadow: Option<wgpu::RenderPipeline>,
    /// The swayed blade, for the cascades the dynamic-caster class refreshes (ADR-2028).
    shadow_sway: Option<wgpu::RenderPipeline>,
}

/// Device resources, built on the first frame against the terrain's device and layouts.
struct GrassGpu {
    params: wgpu::Buffer,
    classes: wgpu::Buffer,
    draw_args: wgpu::Buffer,
    lod: wgpu::Buffer,
    lod_stride: u32,
    patches: wgpu::Buffer,
    state: wgpu::Buffer,
    instances: wgpu::Buffer,
    caps: [u32; REGIONS],
    index: wgpu::Buffer,
    /// `(blades, segments)` per region the index buffer holds.
    index_shape: [(u32, u32); REGIONS],
    cull_layout: wgpu::BindGroupLayout,
    draw_layout: wgpu::BindGroupLayout,
    count_pipeline: wgpu::ComputePipeline,
    scan_pipeline: wgpu::ComputePipeline,
    write_pipeline: wgpu::ComputePipeline,
    cull_bind: Option<wgpu::BindGroup>,
    draw_bind: Option<wgpu::BindGroup>,
    pipelines: Option<GrassPipelines>,
    /// The colour pipeline of the surface-weather variant and its format (spec 99 § 6.4, grass
    /// stage 3): built on the first wet, snowy or rainy frame, or by the weather warm, never on a
    /// dry frame.
    weather_color: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    patches_cache: Vec<u8>,
    classes_cache: Vec<u8>,
    lod_cache: Vec<u8>,
    args_cache: Vec<u8>,
}

/// The grass field's GPU resources and per-frame state (see the module docs). Build it with
/// [`GrassLayer::new`] and bind it with [`crate::TerrainSharedDraw::set_grass`].
pub struct GrassLayer {
    set: GrassSet,
    mask: GrassMask,
    handle: TerrainGrassHandle,
    queue: wgpu::Queue,
    mask_texture: wgpu::Texture,
    mask_view: wgpu::TextureView,
    mask_sampler: wgpu::Sampler,
    mask_uploaded: u64,
    gpu: Option<GrassGpu>,
    patches: Vec<GrassPatch>,
    /// Patches the last selection took out of the camera passes through the outdoor portals
    /// (ADR-8843): dropped, or kept for the cascades with [`PATCH_NO_CAMERA`].
    portal_culled: u32,
    /// Last patch walk. A held camera copies it instead of visiting the quadtree again.
    patch_select: GrassSelectCache,
    patch_count: u32,
    budget: GrassBudget,
    shadow_cascades: u32,
    request_generation: u64,
    time: f32,
    prev_time: f32,
    /// The blast gust of this frame ([`TerrainGrassHandle::set_gust`], ADR-1497).
    gust: Option<crate::grass::GrassGust>,
    frames: u64,
    /// Eye (x, z) of the last frame the patches were selected for; `None` before any
    /// (ADR-7585: a height edit outside the grass range keeps the selection).
    last_eye: Option<[f32; 2]>,
    shadow_key: u64,
    content: u64,
    /// The sway clock has changed since the layer was bound: only then do shadows sway, so a
    /// still clock keeps the rest-pose shadows (and their bytes) it always had.
    clock_moved: bool,
    /// Sun cascades (bit `i` = cascade `i`) whose grass shadow is the swayed blade this frame.
    shadow_sway_mask: u8,
    /// Sway scale in effect: the request's, times the terrain wind's strength and the scene
    /// wind's speed factor ([`effective_wind`]).
    sway_scale: f32,
    /// The `wind` uniform this frame ([`effective_wind`]).
    gpu_wind: [f32; 4],
    /// The grass takes the foliage profile's transmission this frame
    /// (`TerrainBudget::foliage_subsurface`, ADR-3170).
    subsurface: bool,
    /// The wind style word this frame (`counts2.y`; 0 classic, 1 natural, ADR-2446).
    gpu_style: u32,
    /// World boxes of the swaying shadow grass ([`regia_render::SharedPassDraw::dynamic_shadow_boxes`]).
    dynamic_boxes: Vec<[[f32; 3]; 2]>,
    /// The frame has surface weather or a precipitation extinction: the colour pass draws the
    /// blades with the weather variant ([`frame_has_weather`]).
    weather_on: bool,
    /// Frames drawn with the weather variant since the layer was bound.
    weather_frames: u64,
    /// Selection key, blade indices, LOD bytes and the shadow-box map, reused across frames.
    frame_cache: GrassFrameCache,
    /// The camera-body wake of this frame ([`TerrainGrassHandle::set_wake`], ADR-1502).
    wake: Option<crate::grass::GrassWake>,
}

/// Whether a frame needs the surface-weather variant: the terrain's own rule
/// (`upload_surface_weather`), so the blades and the ground under them switch together.
pub(crate) fn frame_has_weather(info: &SharedFrameInfo) -> bool {
    !info.surface_weather.is_dry() || info.precip_extinction_per_m > 0.0
}

/// Wind speed (m/s) at which the scene wind leaves the grass's authored sway unchanged; the sway
/// scales with speed over this, up to [`GRASS_WIND_MAX_SCALE`] times (ADR-2028).
pub(crate) const GRASS_REFERENCE_WIND_M_PER_S: f32 = 5.0;

/// Largest factor the scene wind multiplies the grass sway by.
pub(crate) const GRASS_WIND_MAX_SCALE: f32 = 2.0;

/// Edge of the cells the swaying shadow patches merge into (one dynamic box per cell), metres.
const DYNAMIC_BOX_CELL_M: f32 = 16.0;

/// The scene wind as the grass reads it: unit direction and sway factor (`None` for no wind or
/// a non-finite one, which leaves the field's own direction and scale).
pub(crate) fn grass_wind(wind: Option<[f32; 2]>) -> Option<([f32; 2], f32)> {
    let [x, z] = wind?;
    if !(x.is_finite() && z.is_finite()) {
        return None;
    }
    let speed = (x * x + z * z).sqrt();
    let factor = (speed / GRASS_REFERENCE_WIND_M_PER_S).clamp(0.0, GRASS_WIND_MAX_SCALE);
    let dir = if speed > 1.0e-4 {
        [x / speed, z / speed]
    } else {
        [1.0, 0.0]
    };
    Some((dir, factor))
}

/// The wind the blades sway in this frame: the terrain wind (ADR-1853), with a set scene wind
/// (spec 88 § 2.4) replacing its direction and scaling it by speed. Returns the `wind` uniform
/// and the factor on the request's sway scale; frequency and gust stay the terrain wind's.
pub(crate) fn effective_wind(terrain: &TerrainWind, scene: Option<[f32; 2]>) -> ([f32; 4], f32) {
    let terrain = terrain.clamped();
    let mut words = terrain.gpu_words();
    let mut factor = terrain.strength;
    if let Some(([x, z], scene_factor)) = grass_wind(scene) {
        words[0] = x;
        words[1] = z;
        factor *= scene_factor;
    }
    (words, factor)
}

/// Shadow patches merged into one box per [`DYNAMIC_BOX_CELL_M`] cell of their −X/−Z corner,
/// cells in row order (stable for a held camera). `bounds(patch)` is the patch's ground range
/// with the blade headroom, and how far its blades reach sideways past the patch.
#[cfg(test)]
pub(crate) fn dynamic_shadow_boxes(
    patches: &[GrassPatch],
    bounds: impl FnMut(&GrassPatch) -> (f32, f32, f32),
    out: &mut Vec<[[f32; 3]; 2]>,
) {
    let mut cells = std::collections::BTreeMap::new();
    dynamic_shadow_boxes_reused(patches, bounds, out, &mut cells);
}

/// `dynamic_shadow_boxes` writing into a map the caller keeps. `cells` is cleared, not replaced.
pub(crate) fn dynamic_shadow_boxes_reused(
    patches: &[GrassPatch],
    mut bounds: impl FnMut(&GrassPatch) -> (f32, f32, f32),
    out: &mut Vec<[[f32; 3]; 2]>,
    cells: &mut std::collections::BTreeMap<(i32, i32), [[f32; 3]; 2]>,
) {
    out.clear();
    cells.clear();
    for patch in patches.iter().filter(|p| p.flags & PATCH_SHADOW != 0) {
        let (lo, size) = patch.rect();
        let (y0, y1, reach) = bounds(patch);
        let key = (
            (lo[1] / DYNAMIC_BOX_CELL_M).floor() as i32,
            (lo[0] / DYNAMIC_BOX_CELL_M).floor() as i32,
        );
        let patch_box = [
            [lo[0] - reach, y0, lo[1] - reach],
            [lo[0] + size + reach, y1, lo[1] + size + reach],
        ];
        cells
            .entry(key)
            .and_modify(|b| {
                for axis in 0..3 {
                    b[0][axis] = b[0][axis].min(patch_box[0][axis]);
                    b[1][axis] = b[1][axis].max(patch_box[1][axis]);
                }
            })
            .or_insert(patch_box);
    }
    out.extend(cells.values().copied());
}

impl GrassLayer {
    /// Uploads `mask` (baked from `set` with [`GrassMask::build`]). Nothing draws until the
    /// layer is bound to a terrain draw.
    ///
    /// # Errors
    ///
    /// [`GrassError::Invalid`] when the set is invalid or the mask has a different class count.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        set: GrassSet,
        mask: GrassMask,
    ) -> Result<Self, GrassError> {
        let _span = tracing::info_span!("grass_layer_new", classes = set.classes.len()).entered();
        set.validate(None)?;
        if mask.classes() != set.classes.len() {
            return Err(GrassError::Invalid(format!(
                "the mask holds {} classes, the set {}",
                mask.classes(),
                set.classes.len()
            )));
        }
        let side = mask.side();
        let layers = (set.classes.len() as u32).max(1);
        let mask_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("grass mask"),
            size: wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mask_view = mask_texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let mask_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("grass mask sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let mut layer = Self {
            set,
            mask,
            handle: TerrainGrassHandle::linked(),
            queue: queue.clone(),
            mask_texture,
            mask_view,
            mask_sampler,
            mask_uploaded: 0,
            gpu: None,
            patches: Vec::new(),
            portal_culled: 0,
            patch_select: GrassSelectCache::default(),
            patch_count: 0,
            budget: GrassQuality::Off.budget(),
            shadow_cascades: 0,
            request_generation: u64::MAX,
            time: 0.0,
            prev_time: 0.0,
            gust: None,
            frames: 0,
            last_eye: None,
            shadow_key: 0,
            content: 0,
            clock_moved: false,
            shadow_sway_mask: 0,
            sway_scale: 1.0,
            gpu_wind: TerrainWind::default().gpu_words(),
            subsurface: false,
            gpu_style: 0,
            dynamic_boxes: Vec::new(),
            weather_on: false,
            weather_frames: 0,
            frame_cache: GrassFrameCache::new(),
            wake: None,
        };
        layer.upload_mask();
        Ok(layer)
    }

    /// The handle the host inserts as a resource (`terrain.grass` / `terrain.set_grass`), and
    /// sets the sway clock on ([`TerrainGrassHandle::set_time`]).
    #[must_use]
    pub fn handle(&self) -> TerrainGrassHandle {
        self.handle.clone()
    }

    /// The set this layer draws.
    #[must_use]
    pub fn set(&self) -> &GrassSet {
        &self.set
    }

    /// The mask in effect (with the handle's edits applied).
    #[must_use]
    pub fn mask(&self) -> &GrassMask {
        &self.mask
    }

    /// Patches selected on the last frame.
    #[must_use]
    pub fn patches(&self) -> &[GrassPatch] {
        &self.patches[..self.patch_count as usize]
    }

    /// The last selection's camera patches and the patches the outdoor portals took out of the
    /// camera passes (ADR-8843): `(drawn, culled)`.
    #[must_use]
    pub fn portal_counts(&self) -> (u32, u32) {
        let drawn = self
            .patches()
            .iter()
            .filter(|p| p.flags & PATCH_CAMERA != 0)
            .count() as u32;
        (drawn, self.portal_culled)
    }

    /// World boxes of the grass whose shadow sways this frame (empty unless the renderer's
    /// dynamic-caster class refreshes a cascade the grass draws into and the clock runs).
    #[must_use]
    pub fn dynamic_shadow_boxes(&self) -> &[[[f32; 3]; 2]] {
        &self.dynamic_boxes
    }

    /// Sun cascades (bit `i` = cascade `i`) the swaying grass shadow draws into this frame.
    #[must_use]
    pub fn dynamic_shadow_cascades(&self) -> u8 {
        self.shadow_sway_mask
    }

    fn upload_mask(&mut self) {
        let side = self.mask.side();
        for class in 0..self.mask.classes() {
            let Some(image) = self.mask.image(class) else {
                continue;
            };
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.mask_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: class as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                image,
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
        self.mask_uploaded = self.mask.generation();
    }

    fn ensure_gpu(&mut self, gpu: &TerrainGpu) {
        if self.gpu.is_some() {
            return;
        }
        let _span = tracing::info_span!("grass_gpu_new").entered();
        let d = &gpu.device;
        let cs = wgpu::ShaderStages::COMPUTE;
        let uniform =
            |binding: u32, visibility: wgpu::ShaderStages, dynamic: bool| wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: dynamic,
                    min_binding_size: if dynamic {
                        wgpu::BufferSize::new(size_of::<GrassLod>() as u64)
                    } else {
                        None
                    },
                },
                count: None,
            };
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
        let cull_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("grass cull layout"),
            entries: &[
                uniform(0, cs, false),
                uniform(1, cs, false),
                storage(2, true, cs),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: cs,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: cs,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: cs,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                storage(6, false, cs),
                storage(7, false, cs),
                uniform(8, cs, false),
            ],
        });
        let vf = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let draw_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("grass draw layout"),
            entries: &[
                uniform(24, vf, false),
                storage(25, true, wgpu::ShaderStages::VERTEX),
                uniform(26, vf, false),
                uniform(27, vf, true),
            ],
        });
        let cull = cull_source();
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grass_cull.wgsl"),
            source: regia_render::wgsl_source(&cull),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grass cull"),
            bind_group_layouts: &[Some(&cull_layout)],
            immediate_size: 0,
        });
        let compute = |entry: &str| {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
            })
        };
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let align = d.limits().min_uniform_buffer_offset_alignment.max(16);
        let lod_stride = (size_of::<GrassLod>() as u32).div_ceil(align) * align;
        let u = wgpu::BufferUsages::UNIFORM;
        let s = wgpu::BufferUsages::STORAGE;
        self.gpu = Some(GrassGpu {
            params: buffer("grass params", size_of::<GrassParams>() as u64, u),
            classes: buffer(
                "grass classes",
                (size_of::<GrassClassGpu>() * MAX_GRASS_CLASSES) as u64,
                u,
            ),
            draw_args: buffer("grass draw args", size_of::<GrassDrawArgs>() as u64, u),
            lod: buffer("grass lod", u64::from(lod_stride) * REGIONS as u64, u),
            lod_stride,
            patches: buffer(
                "grass patches",
                (size_of::<GrassPatch>() * MAX_GRASS_PATCHES) as u64,
                s,
            ),
            state: buffer(
                "grass state",
                (STATE_COUNTS + 4 * MAX_GRASS_PATCHES as u64) * 4,
                s | wgpu::BufferUsages::INDIRECT,
            ),
            instances: buffer("grass instances", INSTANCE_BYTES, s),
            caps: [0; REGIONS],
            index: buffer("grass blade indices", 16, wgpu::BufferUsages::INDEX),
            index_shape: [(0, 0); REGIONS],
            cull_layout,
            draw_layout,
            count_pipeline: compute("cs_count"),
            scan_pipeline: compute("cs_scan"),
            write_pipeline: compute("cs_write"),
            cull_bind: None,
            draw_bind: None,
            pipelines: None,
            weather_color: None,
            patches_cache: Vec::new(),
            classes_cache: Vec::new(),
            lod_cache: Vec::new(),
            args_cache: Vec::new(),
        });
    }

    /// Builds the blade pipelines for `format` (load time with `prewarm`, else the first frame).
    pub(crate) fn ensure_pipelines(&mut self, gpu: &TerrainGpu, format: wgpu::TextureFormat) {
        self.ensure_gpu(gpu);
        let Some(g) = self.gpu.as_mut() else {
            return;
        };
        if g.pipelines.as_ref().is_some_and(|p| p.format == format) {
            return;
        }
        let _span = tracing::info_span!("grass_pipelines").entered();
        let d = &gpu.device;
        let terrain = if gpu.height_sampler.is_some() {
            super::hw_height_source()
        } else {
            super::without_surface_weather(include_str!("terrain.wgsl"))
        };
        // The terrain source carries the virtual texture's `@early_depth_test(force)`: a device
        // without `SHADER_EARLY_DEPTH_TEST` (Metal) rejects it. `wgsl_source` specialises the
        // terrain's texture parameters away (Mali, `tests/shader_rules.rs`).
        let source = super::device_source(d, draw_source(&terrain));
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grass.wgsl"),
            source: regia_render::wgsl_source(&source),
        });
        let layout = |label: &str, groups: &[Option<&wgpu::BindGroupLayout>]| {
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: groups,
                immediate_size: 0,
            })
        };
        let plain = layout(
            "grass draw",
            &[
                Some(&gpu.frame_layout),
                Some(&gpu.terrain_layout),
                Some(&g.draw_layout),
            ],
        );
        let color_layout = layout(
            "grass color",
            &[
                Some(&gpu.frame_layout),
                Some(&gpu.terrain_layout),
                Some(&g.draw_layout),
                Some(&gpu.engine_csm_layout),
            ],
        );
        let shadow_layout =
            (d.limits().max_bind_groups >= regia_render::TERRAIN_SHARED_BIND_GROUPS).then(|| {
                layout(
                    "grass shadow",
                    &[
                        Some(&gpu.frame_layout),
                        Some(&gpu.terrain_layout),
                        Some(&g.draw_layout),
                        None,
                        Some(&gpu.cascade_layout),
                    ],
                )
            });
        let primitive = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        };
        let depth_state = |write: bool, compare: wgpu::CompareFunction, bias: wgpu::DepthBiasState| {
            wgpu::DepthStencilState {
                format: gpu.depth_format,
                depth_write_enabled: Some(write),
                depth_compare: Some(compare),
                stencil: Default::default(),
                bias,
            }
        };
        let pipeline = |label: &str,
                        layout: &wgpu::PipelineLayout,
                        vs: &str,
                        fs: Option<(&str, wgpu::TextureFormat)>,
                        depth: wgpu::DepthStencilState| {
            let targets = [fs.map(|(_, format)| wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })];
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: fs.map(|(entry, _)| wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &targets,
                }),
                primitive,
                depth_stencil: Some(depth),
                multisample: Default::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
            })
        };
        let less_equal = wgpu::CompareFunction::LessEqual;
        let no_bias = wgpu::DepthBiasState::default();
        let shadow_bias = wgpu::DepthBiasState {
            constant: 2,
            slope_scale: 1.5,
            clamp: 0.0,
        };
        g.pipelines = Some(GrassPipelines {
            format,
            depth: pipeline(
                "grass depth",
                &plain,
                "vs_grass_depth",
                None,
                depth_state(true, less_equal, no_bias),
            ),
            color: pipeline(
                "grass color",
                &color_layout,
                "vs_grass",
                Some(("fs_grass_color", format)),
                depth_state(true, less_equal, no_bias),
            ),
            velocity: pipeline(
                "grass velocity",
                &plain,
                "vs_grass_velocity",
                Some(("fs_grass_velocity", wgpu::TextureFormat::Rgba16Float)),
                depth_state(false, less_equal, no_bias),
            ),
            shadow: shadow_layout.as_ref().map(|layout| {
                pipeline(
                    "grass shadow",
                    layout,
                    "vs_grass_shadow",
                    None,
                    depth_state(true, wgpu::CompareFunction::Less, shadow_bias),
                )
            }),
            shadow_sway: shadow_layout.as_ref().map(|layout| {
                pipeline(
                    "grass shadow sway",
                    layout,
                    "vs_grass_shadow_sway",
                    None,
                    depth_state(true, wgpu::CompareFunction::Less, shadow_bias),
                )
            }),
        });
    }

    /// Builds the colour pipeline of the surface-weather variant for `format` (spec 99 § 6.4,
    /// grass stage 3): the blade draw composed with the weather code of `terrain.wgsl` and
    /// `grass.wgsl` kept ([`weather_draw_source`]). Only a frame with weather
    /// ([`frame_has_weather`]) or the weather warm ([`Self::warm_weather`]) calls it, so a dry
    /// frame compiles nothing new. The depth, velocity and shadow pipelines never shade and stay.
    pub(crate) fn ensure_weather_pipeline(&mut self, gpu: &TerrainGpu, format: wgpu::TextureFormat) {
        self.ensure_gpu(gpu);
        let Some(g) = self.gpu.as_mut() else {
            return;
        };
        if g.weather_color
            .as_ref()
            .is_some_and(|(built, _)| *built == format)
        {
            return;
        }
        let _span = tracing::info_span!("grass_pipelines_weather").entered();
        let d = &gpu.device;
        let terrain = if gpu.height_sampler.is_some() {
            super::hw_height_source()
        } else {
            include_str!("terrain.wgsl").to_owned()
        };
        let source = super::device_source(d, weather_draw_source(&terrain));
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grass.wgsl (surface weather)"),
            source: regia_render::wgsl_source(&source),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grass color (surface weather)"),
            bind_group_layouts: &[
                Some(&gpu.frame_layout),
                Some(&gpu.terrain_layout),
                Some(&g.draw_layout),
                Some(&gpu.engine_csm_layout),
            ],
            immediate_size: 0,
        });
        let targets = [Some(wgpu::ColorTargetState {
            format,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grass color (surface weather)"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_grass"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_grass_color"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: gpu.depth_format,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
        });
        g.weather_color = Some((format, pipeline));
    }

    /// The weather warm (spec 99 § 6.4): with the terrain's [`TerrainGpu::set_warm_surface_weather`]
    /// on, builds the blades' weather colour pipeline for `format` before a frame needs it, so the
    /// first wet frame compiles nothing. Off, nothing is built.
    pub(crate) fn warm_weather(&mut self, gpu: &TerrainGpu, format: wgpu::TextureFormat) {
        if gpu.warm_surface_weather {
            self.ensure_weather_pipeline(gpu, format);
        }
    }

    /// The tier this frame runs, its budget and its shadow cascades (the request's tier, else
    /// the profile's).
    fn resolve(
        request: &GrassRequest,
        info: &SharedFrameInfo,
        device: &wgpu::Device,
    ) -> (GrassQuality, GrassBudget, u32) {
        let quality = request
            .quality
            .unwrap_or_else(|| GrassQuality::from_terrain_budget(&info.terrain_budget));
        let mut budget = quality.budget();
        budget.range_m *= request.range_scale;
        budget.full_density_m = budget.full_density_m.min(budget.range_m);
        let shadows_supported = device.limits().max_bind_groups >= regia_render::TERRAIN_SHARED_BIND_GROUPS;
        let mut cascades = budget.shadow_cascades.min(info.cascade_count);
        if !request.shadows || !shadows_supported || info.cascade_count == 0 {
            budget.shadow_m = 0.0;
            cascades = 0;
        }
        if cascades == 0 {
            budget.shadow_m = 0.0;
        }
        (quality, budget, cascades)
    }

    /// Applies the handle's request and edits, selects the patches and uploads the frame.
    ///
    /// `heights_generation` is the terrain heightfield's generation. A sculpt bumps it, so a
    /// held camera selects again. The sway clock is not part of the selection key.
    ///
    /// `portals` is the outdoor list of a main view inside a cell, in the map's space, when it
    /// restricts this frame's eye (ADR-8843): a selected patch whose blade box lies in none of
    /// its frusta leaves the camera passes ([`Self::portal_counts`]); a shadow patch stays for the
    /// cascades with [`PATCH_NO_CAMERA`].
    pub(crate) fn prepare(
        &mut self,
        gpu: &TerrainGpu,
        map: &TerrainMap,
        info: &SharedFrameInfo,
        upload: &mut SharedUpload<'_>,
        heights_generation: u64,
        portals: Option<&regia_render::viscells::OutdoorPortals>,
    ) {
        let _span = tracing::info_span!("grass_prepare").entered();
        self.frames += 1;
        // A re-baked mask (biome multipliers, spec 65 § 8) replaces the drawn one when its shape
        // matches the texture; it uploads below like an edit.
        if let Some(mut mask) = self.handle.take_mask() {
            let fits = mask.side() == self.mask.side() && mask.classes() == self.mask.classes();
            if fits {
                mask.follow_generation(&self.mask);
                self.mask = mask;
            }
            self.handle.note_mask(fits);
        }
        let edits = self.handle.take_edits();
        // The mask is in the map's own frame (world minus the grid origin, ADR-8179).
        let [ox, oz] = map.grid_origin();
        for [x, z, radius, factor] in edits {
            self.mask
                .scale_density(x - ox, z - oz, radius, factor, map.world_width());
        }
        if self.mask.generation() != self.mask_uploaded {
            self.upload_mask();
        }
        let (request, generation) = self.handle.request_with_generation();
        self.request_generation = generation;
        self.wake = self.handle.wake().filter(crate::grass::GrassWake::is_active);
        self.prev_time = if self.frames == 1 {
            self.handle.time()
        } else {
            self.time
        };
        self.time = self.handle.time();
        self.clock_moved |= self.time.to_bits() != self.prev_time.to_bits();
        self.gust = self.handle.gust();
        let (quality, budget, cascades) = Self::resolve(&request, info, &gpu.device);
        self.budget = budget;
        self.shadow_cascades = cascades;
        let (gpu_wind, wind_factor) = effective_wind(&self.handle.wind(), info.foliage_wind);
        self.gpu_wind = gpu_wind;
        self.subsurface = info.terrain_budget.foliage_subsurface;
        self.gpu_style = self.handle.wind().clamped().style.gpu_word();
        self.sway_scale = request.sway_scale * wind_factor;
        let drawing = request.enabled && budget.draws() && !self.set.classes.is_empty();
        self.weather_on = frame_has_weather(info);
        if drawing {
            self.ensure_pipelines(gpu, info.color_format);
            if self.weather_on {
                self.ensure_weather_pipeline(gpu, info.color_format);
            }
            let vp = regia_core::glam::Mat4::from_cols_array_2d(&info.unjittered_view_proj);
            let planes = regia_render::Frustum::from_view_proj(&vp).planes();
            let key = GrassSelectKey {
                eye: [
                    info.eye[0].to_bits(),
                    info.eye[1].to_bits(),
                    info.eye[2].to_bits(),
                ],
                planes: plane_select_hash(&planes),
                budget: [
                    budget.range_m.to_bits(),
                    budget.full_density_m.to_bits(),
                    budget.shadow_m.to_bits(),
                ],
                classes: class_select_hash(&self.set.classes, &budget),
                wind_factor: wind_factor.to_bits(),
                mask_generation: self.mask.generation(),
                heights_generation,
                map: [map.width(), map.height(), map.world_width().to_bits()],
                scales: [request.density_scale.to_bits(), request.range_scale.to_bits()],
                request_generation: generation,
                portals: portals.map_or(0, portal_select_hash),
            };
            self.last_eye = Some([info.eye[0], info.eye[2]]);
            if self.frame_cache.needs_select(&key) {
                self.patches.clear();
                let view = GrassView {
                    eye: info.eye,
                    planes: Some(planes),
                };
                let _select = tracing::info_span!("grass_select").entered();
                self.patch_select.set_heights_generation(heights_generation);
                for (index, class) in self.set.classes.iter().enumerate() {
                    let range = ClassRange {
                        range_m: class.range.min(budget.range_m),
                        per_fine: candidates_per_fine_patch(class.density, budget.density_scale),
                        shadow_m: if class.cast_shadows { budget.shadow_m } else { 0.0 },
                        max_height_m: class.blade_height[1] * 2.0 + class.sway * wind_factor.max(1.0),
                    };
                    self.patch_select
                        .select(&view, map, &budget, index as u32, range, &mut self.patches);
                }
                self.portal_culled = match portals {
                    Some(portals) => cull_patches_through(&mut self.patches, portals, |patch| {
                        let (lo, size) = patch.rect();
                        let headroom = self
                            .set
                            .classes
                            .get(patch.class as usize)
                            .map_or(0.0, |c| c.blade_height[1] * 2.0 + c.sway * wind_factor.max(1.0));
                        let (y0, y1) = crate::grass::select::patch_heights(map, lo, size, headroom);
                        // The blades lean at most their headroom sideways past the patch.
                        (
                            [lo[0] - headroom, y0, lo[1] - headroom],
                            [lo[0] + size + headroom, y1, lo[1] + size + headroom],
                        )
                    }),
                    None => 0,
                };
                self.patch_count = self.patches.len().min(MAX_GRASS_PATCHES) as u32;
                self.frame_cache.patch_hash = fnv(PATCH_HASH_SEED, bytemuck::cast_slice(self.patches()));
            }
        } else {
            self.last_eye = None;
            self.portal_culled = 0;
            self.patches.clear();
            self.patch_count = 0;
            self.frame_cache.invalidate();
            self.frame_cache.patch_hash = PATCH_HASH_SEED;
        }
        let mut drawing_now = drawing && self.patch_count > 0;
        let mut level_counts = [0u32; crate::grass::MAX_PATCH_LEVEL as usize + 1];
        let mut camera_patches = 0u32;
        let mut shadow_patches = 0u32;
        let mut candidates = 0u64;
        for p in self.patches() {
            if let Some(slot) = level_counts.get_mut(p.level as usize) {
                *slot += 1;
            }
            camera_patches += u32::from(p.flags & PATCH_CAMERA != 0);
            shadow_patches += u32::from(p.flags & PATCH_SHADOW != 0);
            candidates += u64::from(p.candidates);
        }
        if drawing_now {
            self.upload_frame(gpu, map, info, &request, upload);
        } else {
            self.patch_count = 0;
            drawing_now = false;
        }
        // Keys: the shadow set moves with the eye and the request; the content with the clock
        // while any class sways.
        let swaying = self.sway_scale > 0.0 && self.set.classes.iter().any(|c| c.sway > 0.0);
        // Dynamic-caster class (ADR-2028): in the cascades the renderer refreshes every frame,
        // the shadow is the swayed blade, and the shadow patches are reported as dynamic boxes.
        let cascade_bits = ((1u16 << cascades.min(4)) - 1) as u8;
        self.shadow_sway_mask = if swaying && self.clock_moved && drawing_now {
            info.dynamic_shadow_cascades & cascade_bits
        } else {
            0
        };
        if self.shadow_sway_mask != 0 {
            // Per class: the blade headroom, and the sideways reach of the sway at its peak
            // (`grass_blade`'s amplitude × a wave of at most 1 + gust). Stack storage: at most
            // [`MAX_GRASS_CLASSES`] entries, so a swaying frame does not allocate the list.
            let wave_peak = 1.0 + self.gpu_wind[3];
            let mut extents = [(0.0f32, 0.0f32); MAX_GRASS_CLASSES];
            for (slot, class) in extents.iter_mut().zip(&self.set.classes) {
                *slot = (
                    class.blade_height[1] * 2.0 + class.sway * wind_factor.max(1.0),
                    class.sway * self.sway_scale * wave_peak,
                );
            }
            let mut boxes = std::mem::take(&mut self.frame_cache.boxes);
            let patch_count = self.patch_count as usize;
            dynamic_shadow_boxes_reused(
                &self.patches[..patch_count],
                |patch| {
                    let (lo, size) = patch.rect();
                    let (top, reach) = extents.get(patch.class as usize).copied().unwrap_or_default();
                    let (y0, y1) = crate::grass::select::patch_heights(map, lo, size, top);
                    (y0, y1, reach)
                },
                &mut self.dynamic_boxes,
                &mut boxes,
            );
            self.frame_cache.boxes = boxes;
        } else {
            self.dynamic_boxes.clear();
        }
        let weather_drawn = self.weather_on
            && drawing_now
            && self
                .gpu
                .as_ref()
                .and_then(|g| g.weather_color.as_ref())
                .is_some_and(|(format, _)| *format == info.color_format);
        self.weather_frames += u64::from(weather_drawn);
        let mut key = self.frame_cache.patch_hash;
        key = fnv(key, bytemuck::cast_slice(&info.eye));
        key = fnv(key, &generation.to_le_bytes());
        key = fnv(key, &self.mask.generation().to_le_bytes());
        key = fnv(key, quality.name().as_bytes());
        if self.shadow_sway_mask != 0 {
            key = fnv(key, &[self.shadow_sway_mask]);
        }
        self.shadow_key = if cascades > 0 && drawing_now { key | 1 } else { 0 };
        let mut content = fnv(key, &[u8::from(drawing_now)]);
        if self.subsurface {
            // The foliage transmission changes the colour, not the shadow (ADR-3170).
            content = fnv(content, b"foliage_subsurface");
        }
        if let Some(wake) = &self.wake {
            content = fnv(content, bytemuck::cast_slice(&wake.gpu_rows()));
        }
        if swaying {
            content = fnv(content, &self.time.to_bits().to_le_bytes());
            content = fnv(content, bytemuck::cast_slice(&self.gpu_wind));
            if self.gpu_style != 0 {
                content = fnv(content, &self.gpu_style.to_le_bytes());
            }
            content = fnv(content, &self.sway_scale.to_bits().to_le_bytes());
        }
        if self.weather_on {
            // The blades' colour follows the weather rows (a dry frame hashes as before).
            let weather = info.surface_weather.row().map(f32::to_bits);
            content = fnv(content, bytemuck::cast_slice(&weather));
            content = fnv(content, &info.precip_extinction_per_m.to_bits().to_le_bytes());
        }
        if let Some(g) = self.gust {
            content = fnv(
                content,
                bytemuck::cast_slice(&[g.radius, g.prev_radius, g.bend, g.length]),
            );
        }
        self.content = if drawing_now { content } else { 0 };
        let quality_name = quality.name();
        let class_count = self.set.classes.len();
        let mut class_names = [""; MAX_GRASS_CLASSES];
        for (slot, class) in class_names.iter_mut().zip(&self.set.classes) {
            *slot = class.name.as_str();
        }
        let range_m = self
            .set
            .classes
            .iter()
            .map(|c| c.range.min(budget.range_m))
            .fold(0.0, f32::max);
        let max_instances = if drawing { budget.max_instances } else { 0 };
        let mask_side = self.mask.side();
        let mask_generation = self.mask.generation();
        let time_s = self.time;
        let frames = self.frames;
        let subsurface = self.subsurface;
        let weather_frames = self.weather_frames;
        let gust = self.gust;
        let shadow_sway_cascades = self.shadow_sway_mask.count_ones();
        let dynamic_boxes = self.dynamic_boxes.len() as u32;
        let shadow_m = budget.shadow_m;
        let quality_auto = request.quality.is_none();
        let reported_patches = self.patch_count;
        let handle = &self.handle;
        let names_unchanged = handle.update_report(quality_name, &class_names[..class_count], |report| {
            report.installed = true;
            report.drawing = drawing_now;
            report.quality_auto = quality_auto;
            report.patches = reported_patches;
            report.patches_per_level.clear();
            report.patches_per_level.extend_from_slice(&level_counts);
            report.camera_patches = camera_patches;
            report.shadow_patches = shadow_patches;
            report.candidates = candidates;
            report.max_instances = max_instances;
            report.range_m = range_m;
            report.shadow_m = shadow_m;
            report.shadow_cascades = cascades;
            report.mask_side = mask_side;
            report.mask_generation = mask_generation;
            report.time_s = time_s;
            report.frames = frames;
            report.subsurface = subsurface;
            report.surface_weather = weather_drawn;
            report.weather_frames = weather_frames;
            report.shadow_sway_cascades = shadow_sway_cascades;
            report.dynamic_boxes = dynamic_boxes;
            report.gust = gust;
        });
        self.frame_cache.note_report_names(names_unchanged);
    }

    fn upload_frame(
        &mut self,
        gpu: &TerrainGpu,
        map: &TerrainMap,
        info: &SharedFrameInfo,
        request: &GrassRequest,
        upload: &mut SharedUpload<'_>,
    ) {
        let budget = self.budget;
        let Some(lod_stride) = self.gpu.as_ref().map(|g| g.lod_stride) else {
            return;
        };
        // Instance regions sized from the tier.
        let fifth = budget.max_instances.div_ceil(5).div_ceil(64) * 64;
        let caps = REGION_FIFTHS.map(|f| f * fifth);
        // Index sections: the widest class decides the blades of each region. Built into the
        // frame scratch, and only when the shape changes.
        let blades = self.set.classes.iter().map(|c| c.blades).max().unwrap_or(1);
        let segments = region_segments(&budget);
        let shape: [(u32, u32); REGIONS] =
            std::array::from_fn(|r| (region_blades(blades, budget.max_blades, r), segments[r]));
        let index_rebuilt = self.frame_cache.stage_index_block(shape);
        let lod_rebuilt = self.frame_cache.fill_lod_bytes(lod_stride as usize, shape, caps);
        let first = self.frame_cache.index_first;
        let counts = self.frame_cache.index_counts;
        let index_scratch = std::mem::take(&mut self.frame_cache.index_scratch);
        let lod_scratch = std::mem::take(&mut self.frame_cache.lod_scratch);
        {
            let Some(g) = self.gpu.as_mut() else {
                self.frame_cache.index_scratch = index_scratch;
                self.frame_cache.lod_scratch = lod_scratch;
                return;
            };
            let mut rebind = g.cull_bind.is_none();
            if caps != g.caps {
                let total: u64 = caps.iter().map(|&c| u64::from(c)).sum();
                g.instances = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("grass instances"),
                    size: (total * INSTANCE_BYTES).max(INSTANCE_BYTES),
                    usage: wgpu::BufferUsages::STORAGE,
                    mapped_at_creation: false,
                });
                g.caps = caps;
                rebind = true;
            }
            if index_rebuilt || shape != g.index_shape {
                g.index = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("grass blade indices"),
                    size: (index_scratch.len() * 2) as u64,
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.queue
                    .write_buffer(&g.index, 0, bytemuck::cast_slice(&index_scratch));
                g.index_shape = shape;
            }
            if rebind {
                let d = &gpu.device;
                fn buf(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
                    wgpu::BindGroupEntry {
                        binding,
                        resource: buffer.as_entire_binding(),
                    }
                }
                g.cull_bind = Some(d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("grass cull"),
                    layout: &g.cull_layout,
                    entries: &[
                        buf(0, &g.params),
                        buf(1, &g.classes),
                        buf(2, &g.patches),
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(gpu.height_view()),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(&self.mask_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: wgpu::BindingResource::Sampler(&self.mask_sampler),
                        },
                        buf(6, &g.state),
                        buf(7, &g.instances),
                        buf(8, &g.draw_args),
                    ],
                }));
                g.draw_bind = Some(d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("grass draw"),
                    layout: &g.draw_layout,
                    entries: &[
                        buf(24, &g.params),
                        buf(25, &g.instances),
                        buf(26, &g.classes),
                        wgpu::BindGroupEntry {
                            binding: 27,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &g.lod,
                                offset: 0,
                                size: wgpu::BufferSize::new(size_of::<GrassLod>() as u64),
                            }),
                        },
                    ],
                }));
            }
            let patches = &self.patches[..self.patch_count as usize];
            upload.write_if_changed(&mut g.patches_cache, &g.patches, bytemuck::cast_slice(patches));
            let mut classes = [GrassClassGpu::default(); MAX_GRASS_CLASSES];
            for (i, (slot, c)) in classes.iter_mut().zip(&self.set.classes).enumerate() {
                *slot = GrassClassGpu {
                    shape: [
                        c.blade_height[0],
                        c.blade_height[1],
                        c.blade_width,
                        c.clump_radius,
                    ],
                    shape2: [c.lean, c.bend, c.green_height, c.blades as f32],
                    base: [c.base_color[0], c.base_color[1], c.base_color[2], c.roughness],
                    tip: [c.tip_color[0], c.tip_color[1], c.tip_color[2], c.translucency],
                    green: [
                        c.green_color[0],
                        c.green_color[1],
                        c.green_color[2],
                        c.base_occlusion,
                    ],
                    green_tip: [
                        c.green_tip_color[0],
                        c.green_tip_color[1],
                        c.green_tip_color[2],
                        c.color_jitter,
                    ],
                    misc: [
                        c.range.min(budget.range_m),
                        if c.cast_shadows { budget.shadow_m } else { 0.0 },
                        c.sway,
                        c.sway_speed,
                    ],
                    seed: [c.stream_seed(self.set.seed), i as u32, 0, 0],
                };
            }
            upload.write_if_changed(&mut g.classes_cache, &g.classes, bytemuck::cast_slice(&classes));
            if lod_rebuilt {
                upload.write_if_changed(&mut g.lod_cache, &g.lod, &lod_scratch);
            }
            let args = GrassDrawArgs {
                index_count: counts,
                first_index: first,
            };
            upload.write_if_changed(&mut g.args_cache, &g.draw_args, bytemuck::bytes_of(&args));
            let vp = regia_core::glam::Mat4::from_cols_array_2d(&info.unjittered_view_proj);
            let planes = regia_render::Frustum::from_view_proj(&vp).planes();
            let params = GrassParams {
                planes,
                eye: [info.eye[0], info.eye[1], info.eye[2], self.time],
                map: [
                    // The grid origin folds into the offset lane (ADR-8179).
                    map.gpu_half_width(),
                    map.world_width(),
                    (map.width() - 1) as f32,
                    (map.height() - 1) as f32,
                ],
                heights: [
                    map.height_scale(),
                    map.height_offset(),
                    crate::grass::PATCH_M,
                    self.prev_time,
                ],
                law: [
                    budget.full_density_m,
                    request.density_scale,
                    budget.max_widen,
                    self.sway_scale,
                ],
                lod: [
                    budget.lod_m[0],
                    budget.lod_m[1],
                    budget.max_blades as f32,
                    request.debug_view as f32,
                ],
                wake: self
                    .wake
                    .map_or([[0.0; 4]; crate::grass::GRASS_WAKE_ROWS], |w| w.gpu_rows()),
                counts: [
                    self.patch_count,
                    u32::from(self.shadow_cascades > 0),
                    segments[0],
                    segments[1],
                ],
                caps,
                counts2: [segments[2], self.gpu_style, 0, 0],
                wind: self.gpu_wind,
                subsurface: [
                    if self.subsurface { 1.0 } else { 0.0 },
                    GRASS_BLADE_THICKNESS_MM,
                    0.0,
                    0.0,
                ],
                subsurface_g: foliage_transmission_words(),
                // The blast gust (ADR-1497): centre x, z, front radius, bend; previous radius,
                // positive-phase length. Bend 0 = no gust.
                gust: self
                    .gust
                    .map_or([0.0; 4], |g| [g.center[0], g.center[1], g.radius, g.bend]),
                gust2: self
                    .gust
                    .map_or([0.0; 4], |g| [g.prev_radius, g.length, 0.0, 0.0]),
            };
            upload.write(&g.params, 0, bytemuck::bytes_of(&params));
        }
        self.frame_cache.index_scratch = index_scratch;
        self.frame_cache.lod_scratch = lod_scratch;
    }

    /// Records the generator (start of the frame, after the uploads).
    pub(crate) fn encode(&self, encoder: &mut wgpu::CommandEncoder) {
        let Some(g) = self.gpu.as_ref() else {
            return;
        };
        let Some(bind) = g.cull_bind.as_ref().filter(|_| self.patch_count > 0) else {
            return;
        };
        let _span = tracing::trace_span!("grass_generate", patches = self.patch_count).entered();
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("grass generate"),
            timestamp_writes: None,
        });
        pass.set_bind_group(0, bind, &[]);
        pass.set_pipeline(&g.count_pipeline);
        pass.dispatch_workgroups(self.patch_count, 1, 1);
        pass.set_pipeline(&g.scan_pipeline);
        pass.dispatch_workgroups(1, 1, 1);
        pass.set_pipeline(&g.write_pipeline);
        pass.dispatch_workgroups(self.patch_count, 1, 1);
    }

    /// Draws the blades into a shared pass (nothing into the decal receiver).
    pub(crate) fn draw<'a>(&'a self, gpu: &'a TerrainGpu, pass: &mut wgpu::RenderPass<'a>, kind: SharedPass) {
        if self.patch_count == 0 {
            return;
        }
        let Some(g) = self.gpu.as_ref() else {
            return;
        };
        let (Some(pipelines), Some(bind)) = (g.pipelines.as_ref(), g.draw_bind.as_ref()) else {
            return;
        };
        let (pipeline, regions) = match kind {
            SharedPass::Depth => (&pipelines.depth, 0..3),
            SharedPass::Color => match g.weather_color.as_ref() {
                Some((format, weather)) if self.weather_on && *format == pipelines.format => (weather, 0..3),
                _ => (&pipelines.color, 0..3),
            },
            SharedPass::Velocity => (&pipelines.velocity, 0..3),
            SharedPass::Shadow { index } if index < self.shadow_cascades => {
                let sway = index < 8 && (self.shadow_sway_mask >> index) & 1 == 1;
                let shadow = if sway {
                    pipelines.shadow_sway.as_ref()
                } else {
                    pipelines.shadow.as_ref()
                };
                match shadow {
                    Some(shadow) => (shadow, SHADOW..SHADOW + 1),
                    None => return,
                }
            }
            SharedPass::Shadow { .. } | SharedPass::Receiver => return,
        };
        let _span = tracing::trace_span!("grass_draw", ?kind).entered();
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &gpu.frame_bind, &[]);
        pass.set_bind_group(1, &gpu.terrain_bind, &[]);
        match kind {
            SharedPass::Color => pass.set_bind_group(3, &gpu.engine_csm_bind, &[]),
            SharedPass::Shadow { index } => {
                let slot = (index as usize).min(gpu.cascade_binds.len().saturating_sub(1));
                if let Some(cascade) = gpu.cascade_binds.get(slot) {
                    pass.set_bind_group(4, cascade, &[]);
                }
            }
            SharedPass::Depth | SharedPass::Velocity | SharedPass::Receiver => {}
        }
        pass.set_index_buffer(g.index.slice(..), wgpu::IndexFormat::Uint16);
        for r in regions {
            pass.set_bind_group(2, bind, &[r as u32 * g.lod_stride]);
            pass.draw_indexed_indirect(&g.state, (r as u64) * 20);
        }
    }

    /// Whether a height edit over the world XZ box `lo..hi` can change the patches this
    /// layer selects (ADR-7585): the box meets the square around the last selection's eye that
    /// holds every patch in range (the budget's range plus two coarsest patches), or nothing
    /// was selected yet. A `false` keeps the selection and the grass shadow keys.
    pub(crate) fn meets_heights_box(&self, lo: [f32; 2], hi: [f32; 2]) -> bool {
        let Some(eye) = self.last_eye else {
            return true;
        };
        let reach = self.budget.range_m.max(self.budget.shadow_m)
            + 2.0 * crate::grass::PATCH_M * (1u32 << crate::grass::MAX_PATCH_LEVEL) as f32;
        lo[0] <= eye[0] + reach
            && hi[0] >= eye[0] - reach
            && lo[1] <= eye[1] + reach
            && hi[1] >= eye[1] - reach
    }

    /// Identity of what the grass draws into cascade `index` (0 when it draws none there).
    pub(crate) fn cascade_shadow_key(&self, index: u32) -> u64 {
        if index < self.shadow_cascades && self.patch_count > 0 {
            self.shadow_key.rotate_left(index + 7)
        } else {
            0
        }
    }

    /// Changes whenever what the grass draws may change without the camera moving.
    pub(crate) fn content_key(&self) -> u64 {
        self.content
    }
}

/// Composed WGSL of the grass generator (common prelude, placement, compute).
#[must_use]
pub fn cull_source() -> String {
    format!(
        "{}\n{}\n{}",
        include_str!("grass_common.wgsl"),
        include_str!("grass_place.wgsl"),
        include_str!("grass_cull.wgsl")
    )
}

/// Composed WGSL of the blade draw: `terrain` (the terrain shader source the draw was built
/// with), the common prelude and the draw entry points, without the blades' surface weather
/// (the dry draw every frame without weather compiles; spec 99 § 6.4).
#[must_use]
pub fn draw_source(terrain: &str) -> String {
    format!(
        "{terrain}\n{}\n{}",
        include_str!("grass_common.wgsl"),
        super::without_surface_weather(include_str!("grass.wgsl"))
    )
}

/// [`draw_source`] with the blades' surface weather kept: the colour pass of a wet, snowy or
/// rainy frame (grass stage 3, ADR-3249). `terrain` must keep its own weather code (the full
/// `terrain.wgsl`, or [`super::hw_height_source`]): the blades read its `material.weather` rows,
/// the occlusion tile and the ground layers' response.
#[must_use]
pub fn weather_draw_source(terrain: &str) -> String {
    format!(
        "{terrain}\n{}\n{}",
        include_str!("grass_common.wgsl"),
        include_str!("grass.wgsl")
    )
}

/// Inputs that decide which grass patches are selected. Time is not one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GrassSelectKey {
    /// Camera eye, bitwise.
    pub eye: [u32; 3],
    /// Frustum planes, or 0 when the frame has no frustum.
    pub planes: u64,
    /// `range_m`, `full_density_m`, `shadow_m`, bitwise.
    pub budget: [u32; 3],
    /// Class ranges, density and sway.
    pub classes: u64,
    /// Wind factor on blade height, bitwise.
    pub wind_factor: u32,
    /// Mask generation.
    pub mask_generation: u64,
    /// Heightfield generation. A sculpt while the camera is held must select again.
    pub heights_generation: u64,
    /// Map width, height and world width (the last bitwise).
    pub map: [u32; 3],
    /// Request density and range scales, bitwise.
    pub scales: [u32; 2],
    /// Request generation.
    pub request_generation: u64,
    /// The outdoor-portal list the patches were culled through ([`portal_select_hash`]), or 0.
    pub portals: u64,
}

/// Scratch reused across grass frames: selection key, blade indices, LOD bytes, shadow boxes.
pub(crate) struct GrassFrameCache {
    key: Option<GrassSelectKey>,
    /// Times [`Self::needs_select`] decided a selection was required.
    selects: u64,
    /// Times the blade index block was rebuilt.
    index_fills: u64,
    /// Times the LOD bytes were rebuilt.
    lod_fills: u64,
    /// Times the report's name strings were rewritten.
    report_rewrites: u64,
    /// Times the dynamic-box map was allocated.
    #[cfg(test)]
    box_allocs: u64,
    index_shape: [(u32, u32); REGIONS],
    index_ready: bool,
    index_first: [u32; REGIONS],
    index_counts: [u32; REGIONS],
    index_scratch: Vec<u16>,
    lod_shape: [(u32, u32); REGIONS],
    lod_caps: [u32; REGIONS],
    lod_stride: usize,
    lod_ready: bool,
    lod_scratch: Vec<u8>,
    /// FNV of the selected patch bytes. Reused while the selection key holds.
    patch_hash: u64,
    boxes: std::collections::BTreeMap<(i32, i32), [[f32; 3]; 2]>,
}

impl GrassFrameCache {
    /// Empty scratch. The box map is allocated once here.
    pub(crate) fn new() -> Self {
        Self {
            key: None,
            selects: 0,
            index_fills: 0,
            lod_fills: 0,
            report_rewrites: 0,
            #[cfg(test)]
            box_allocs: 1,
            index_shape: [(0, 0); REGIONS],
            index_ready: false,
            index_first: [0; REGIONS],
            index_counts: [0; REGIONS],
            index_scratch: Vec::new(),
            lod_shape: [(0, 0); REGIONS],
            lod_caps: [0; REGIONS],
            lod_stride: 0,
            lod_ready: false,
            lod_scratch: Vec::new(),
            patch_hash: PATCH_HASH_SEED,
            boxes: std::collections::BTreeMap::new(),
        }
    }

    /// Drops the cached selection so the next frame selects again.
    pub(crate) fn invalidate(&mut self) {
        self.key = None;
    }

    /// Whether `select_patches` has to run. A repeat of `key` does not.
    pub(crate) fn needs_select(&mut self, key: &GrassSelectKey) -> bool {
        if self.key == Some(*key) {
            return false;
        }
        self.selects += 1;
        self.key = Some(*key);
        true
    }

    /// Fills the blade index scratch for `shape`. A repeat of `shape` leaves the scratch and
    /// returns false.
    pub(crate) fn stage_index_block(&mut self, shape: [(u32, u32); REGIONS]) -> bool {
        if self.index_ready && self.index_shape == shape {
            return false;
        }
        self.index_fills += 1;
        self.index_scratch.clear();
        for (r, &(blades, segments)) in shape.iter().enumerate() {
            self.index_first[r] = self.index_scratch.len() as u32;
            let before = self.index_scratch.len();
            extend_blade_indices(&mut self.index_scratch, blades, segments);
            self.index_counts[r] = (self.index_scratch.len() - before) as u32;
        }
        if !self.index_scratch.len().is_multiple_of(2) {
            self.index_scratch.push(0);
        }
        self.index_shape = shape;
        self.index_ready = true;
        true
    }

    /// Fills the LOD-byte scratch. A repeat of `stride`, `shape` and `caps` leaves it and
    /// returns false.
    pub(crate) fn fill_lod_bytes(
        &mut self,
        stride: usize,
        shape: [(u32, u32); REGIONS],
        caps: [u32; REGIONS],
    ) -> bool {
        if self.lod_ready && self.lod_stride == stride && self.lod_shape == shape && self.lod_caps == caps {
            return false;
        }
        self.lod_fills += 1;
        self.lod_scratch.clear();
        self.lod_scratch.resize(stride.saturating_mul(REGIONS), 0);
        let mut base = 0u32;
        for (r, &(blades, segments)) in shape.iter().enumerate() {
            let block = GrassLod {
                region: r as u32,
                blades,
                segments,
                base,
            };
            base += caps[r];
            let at = r * stride;
            let bytes = bytemuck::bytes_of(&block);
            if at + bytes.len() <= self.lod_scratch.len() {
                self.lod_scratch[at..at + bytes.len()].copy_from_slice(bytes);
            }
        }
        self.lod_shape = shape;
        self.lod_caps = caps;
        self.lod_stride = stride;
        self.lod_ready = true;
        true
    }

    /// Counts a rewrite of the report's quality and class names. An unchanged set does not.
    pub(crate) fn note_report_names(&mut self, unchanged: bool) {
        if unchanged {
            return;
        }
        self.report_rewrites += 1;
    }

    /// Clears the shadow-box map. The map allocated in [`Self::new`] stays.
    #[cfg(test)]
    pub(crate) fn rebuild_shadow_boxes(&mut self) {
        self.boxes.clear();
    }
}

/// Seed of the patch-list hash folded into the grass shadow and content keys.
const PATCH_HASH_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// Hashes the class fields that change which patches are selected.
fn class_select_hash(classes: &[crate::grass::GrassClass], budget: &GrassBudget) -> u64 {
    let mut hash = PATCH_HASH_SEED;
    for class in classes {
        let words = [
            class.range.min(budget.range_m).to_bits(),
            class.density.to_bits(),
            u32::from(class.cast_shadows),
            class.blade_height[0].to_bits(),
            class.blade_height[1].to_bits(),
            class.sway.to_bits(),
        ];
        hash = fnv(hash, bytemuck::cast_slice(&words));
    }
    hash
}

/// Hashes the six frustum planes so a turn reselects without an eye move.
fn plane_select_hash(planes: &[[f32; 4]; 6]) -> u64 {
    let mut bits = [0u32; 24];
    for (plane_i, plane) in planes.iter().enumerate() {
        for (axis, component) in plane.iter().enumerate() {
            bits[plane_i * 4 + axis] = component.to_bits();
        }
    }
    fnv(PATCH_HASH_SEED, bytemuck::cast_slice(&bits))
}

/// Hash of a restricted outdoor list's frusta (bitwise) for [`GrassSelectKey::portals`]: a
/// held camera keeps its culled patches while the list holds.
fn portal_select_hash(portals: &regia_render::viscells::OutdoorPortals) -> u64 {
    let mut h = fnv(PATCH_HASH_SEED, b"portals");
    for frustum in portals.frusta() {
        for plane in frustum.planes() {
            h = fnv(h, bytemuck::cast_slice(&plane.map(f32::to_bits)));
        }
        h = fnv(h, b"|");
    }
    h | 1
}

/// The outdoor-portal cull of a patch selection (ADR-8843): a patch whose blade box (`bounds`) no
/// frustum of `portals` admits, within [`regia_render::viscells::OUTDOOR_NODE_MARGIN_M`], loses
/// its camera blades: a shadow patch keeps casting with [`PATCH_NO_CAMERA`], any other leaves the
/// list. Returns the patches culled. `portals` must restrict the view (an unrestricted list
/// admits everything, so nothing is culled).
fn cull_patches_through(
    patches: &mut Vec<GrassPatch>,
    portals: &regia_render::viscells::OutdoorPortals,
    bounds: impl Fn(&GrassPatch) -> ([f32; 3], [f32; 3]),
) -> u32 {
    let mut culled = 0u32;
    patches.retain_mut(|patch| {
        let (lo, hi) = bounds(patch);
        if portals.admits_box(lo, hi, regia_render::viscells::OUTDOOR_NODE_MARGIN_M) {
            return true;
        }
        culled += 1;
        if patch.flags & PATCH_SHADOW == 0 {
            return false;
        }
        patch.flags = (patch.flags & !PATCH_CAMERA) | PATCH_NO_CAMERA;
        true
    });
    culled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_structs_match_the_wgsl_sizes() {
        assert_eq!(size_of::<GrassParams>(), 432);
        assert_eq!(size_of::<GrassClassGpu>(), 128);
        assert_eq!(size_of::<GrassPatch>(), 32);
        assert_eq!(size_of::<GrassLod>(), 16);
        assert_eq!(size_of::<GrassDrawArgs>(), 32);
    }

    /// The span naga lays out for the WGSL struct `name` in `source`.
    fn wgsl_struct_span(source: &str, name: &str) -> u32 {
        let module = naga::front::wgsl::parse_str(source).unwrap_or_else(|e| panic!("{name}: parse: {e}"));
        module
            .types
            .iter()
            .find_map(|(_, ty)| match ty.inner {
                naga::TypeInner::Struct { span, .. } if ty.name.as_deref() == Some(name) => Some(span),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no WGSL struct {name}"))
    }

    /// Spec 82 § 8: the uniform the wind block grew (`GrassParams.wind`) and the buffers the
    /// generator and the draw share have the size the shader parses, not only a pinned number.
    #[test]
    fn gpu_structs_match_the_parsed_wgsl_layout() {
        let cull = cull_source();
        for (name, rust) in [
            ("GrassParams", size_of::<GrassParams>()),
            ("GrassClassGpu", size_of::<GrassClassGpu>()),
            ("GrassPatch", size_of::<GrassPatch>()),
            ("GrassDrawArgs", size_of::<GrassDrawArgs>()),
        ] {
            assert_eq!(wgsl_struct_span(&cull, name) as usize, rust, "{name}");
        }
        let draw = draw_source(include_str!("terrain.wgsl"));
        assert_eq!(
            wgsl_struct_span(&draw, "GrassLod") as usize,
            size_of::<GrassLod>()
        );
        assert!(
            size_of::<GrassParams>().is_multiple_of(16),
            "a uniform is 16-byte aligned"
        );
    }

    #[test]
    fn a_held_eye_reuses_grass_selection_and_blade_indices() {
        let mut cache = GrassFrameCache::new();
        let key = GrassSelectKey {
            eye: [0f32.to_bits(), 8f32.to_bits(), 0f32.to_bits()],
            planes: 1,
            budget: [100f32.to_bits(), 40f32.to_bits(), 0f32.to_bits()],
            classes: 7,
            wind_factor: 1f32.to_bits(),
            mask_generation: 1,
            heights_generation: 1,
            map: [64, 64, 256f32.to_bits()],
            scales: [1f32.to_bits(), 1f32.to_bits()],
            request_generation: 1,
            portals: 0,
        };
        assert!(cache.needs_select(&key));
        let selects = cache.selects;
        assert!(!cache.needs_select(&key), "a held eye selected again");
        assert_eq!(cache.selects, selects);
        let mut moved = key;
        moved.eye[0] = 80f32.to_bits();
        assert!(cache.needs_select(&moved), "a moved eye must select again");
        let mut sculpted = moved;
        sculpted.heights_generation = 2;
        assert!(cache.needs_select(&sculpted), "a height edit must select again");

        let shape = [(2u32, 2u32), (1, 2), (1, 1), (1, 2)];
        assert!(cache.stage_index_block(shape));
        assert_eq!(cache.index_first[0], 0);
        assert!(!cache.index_scratch.is_empty());
        assert!(cache.index_counts.iter().any(|&count| count > 0));
        let fills = cache.index_fills;
        assert!(
            !cache.stage_index_block(shape),
            "the same blade shape filled indices again"
        );
        assert_eq!(cache.index_fills, fills);
        let caps = [64u32, 128, 128, 128];
        assert!(cache.fill_lod_bytes(16, shape, caps));
        assert!(!cache.lod_scratch.is_empty());
        let lods = cache.lod_fills;
        assert!(
            !cache.fill_lod_bytes(16, shape, caps),
            "the same LOD caps filled bytes again"
        );
        assert_eq!(cache.lod_fills, lods);

        cache.note_report_names(true);
        let rewrites = cache.report_rewrites;
        cache.note_report_names(true);
        assert_eq!(cache.report_rewrites, rewrites, "unchanged names were rewritten");
        assert_eq!(cache.box_allocs, 1);
        assert!(cache.boxes.is_empty());
        cache.rebuild_shadow_boxes();
        assert_eq!(cache.box_allocs, 1, "the shadow-box map was allocated again");
    }

    #[test]
    fn blade_indices_cover_each_blade_with_a_strip_and_a_tip() {
        for segments in 1..=4 {
            let per_blade = 2 * segments + 1;
            let idx = blade_indices(3, segments);
            assert_eq!(idx.len() as u32, 3 * (2 * segments - 1) * 3);
            assert!(idx.iter().all(|&i| u32::from(i) < 3 * per_blade));
            // Every vertex of every blade is used.
            for v in 0..3 * per_blade {
                assert!(
                    idx.iter().any(|&i| u32::from(i) == v),
                    "vertex {v} unused at {segments} segments"
                );
            }
        }
    }

    #[test]
    fn far_regions_draw_fewer_blades() {
        assert_eq!(region_blades(9, 16, 0), 9);
        assert_eq!(region_blades(9, 16, 1), 5);
        assert_eq!(region_blades(9, 16, 2), 3);
        assert_eq!(region_blades(9, 2, 0), 2);
        assert_eq!(region_blades(1, 16, 2), 1);
    }

    fn patch(fine: [i32; 2], level: u32, flags: u32) -> GrassPatch {
        GrassPatch {
            fine,
            level,
            class: 0,
            candidates: 1,
            per_fine: 1,
            flags,
            pad: 0,
        }
    }

    /// Spec 88 § 2.4: only shadow patches become boxes, merged per 16 m cell in a stable order
    /// (the same patches give the same boxes), each box covering its patches and their heights.
    #[test]
    fn dynamic_boxes_merge_shadow_patches_per_cell() {
        let patches = [
            patch([0, 0], 0, PATCH_SHADOW | PATCH_CAMERA),
            patch([1, 0], 0, PATCH_SHADOW),
            patch([40, 40], 0, PATCH_CAMERA),
            patch([-8, 2], 1, PATCH_SHADOW),
        ];
        const REACH: f32 = 0.25;
        let heights = |p: &GrassPatch| (p.fine[0] as f32 * 0.1, 2.0 + p.fine[1] as f32 * 0.1, REACH);
        let mut boxes = Vec::new();
        dynamic_shadow_boxes(&patches, heights, &mut boxes);
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        let mut again = vec![[[9.0; 3]; 2]];
        dynamic_shadow_boxes(&patches, heights, &mut again);
        assert_eq!(boxes, again);
        for p in patches.iter().filter(|p| p.flags & PATCH_SHADOW != 0) {
            let (lo, size) = p.rect();
            let (y0, y1, _) = heights(p);
            // The box holds the patch and its blades at the peak of the sway.
            let inside = boxes.iter().any(|[min, max]| {
                min[0] <= lo[0] - REACH
                    && min[2] <= lo[1] - REACH
                    && max[0] >= lo[0] + size + REACH
                    && max[2] >= lo[1] + size + REACH
                    && min[1] <= y0
                    && max[1] >= y1
            });
            assert!(inside, "{p:?} outside {boxes:?}");
        }
        // The camera-only patch far away is in no box.
        let (far, _) = patches[2].rect();
        assert!(boxes.iter().all(|[_, max]| max[0] < far[0]), "{boxes:?}");
        dynamic_shadow_boxes(&[patch([0, 0], 0, PATCH_CAMERA)], heights, &mut boxes);
        assert!(boxes.is_empty());
    }

    /// The terrain wind alone drives the blades; a scene wind replaces its direction and scales
    /// its strength; frequency and gust stay the terrain wind's.
    #[test]
    fn a_scene_wind_overrides_the_terrain_winds_direction_only() {
        let terrain = TerrainWind {
            direction: [0.0, 2.0],
            strength: 3.0,
            frequency_scale: 0.5,
            gust: 0.1,
            ..TerrainWind::default()
        };
        assert_eq!(effective_wind(&terrain, None), ([0.0, 2.0, 0.5, 0.1], 3.0));
        assert_eq!(
            effective_wind(&terrain, Some([0.0, -10.0])),
            ([0.0, -1.0, 0.5, 0.1], 6.0)
        );
        // Still air stays still under a scene wind; calm scene air stops the terrain wind.
        let still = TerrainWind {
            strength: 0.0,
            ..terrain
        };
        assert_eq!(effective_wind(&still, Some([5.0, 0.0])).1, 0.0);
        assert_eq!(effective_wind(&terrain, Some([0.0, 0.0])).1, 0.0);
        // A non-finite scene wind leaves the terrain wind.
        assert_eq!(
            effective_wind(&terrain, Some([f32::NAN, 0.0])),
            ([0.0, 2.0, 0.5, 0.1], 3.0)
        );
    }

    /// The scene wind: direction normalized, sway factor speed / 5 m/s clamped to 0..2, the
    /// field's own wind for none or a non-finite vector.
    #[test]
    fn scene_wind_sets_the_grass_direction_and_scale() {
        assert_eq!(grass_wind(None), None);
        assert_eq!(grass_wind(Some([f32::NAN, 1.0])), None);
        let (dir, factor) = grass_wind(Some([3.0, 4.0])).expect("wind");
        assert!(
            (dir[0] - 0.6).abs() < 1.0e-6 && (dir[1] - 0.8).abs() < 1.0e-6,
            "{dir:?}"
        );
        assert!((factor - 1.0).abs() < 1.0e-6, "{factor}");
        assert_eq!(grass_wind(Some([0.0, 0.0])), Some(([1.0, 0.0], 0.0)));
        let (_, storm) = grass_wind(Some([0.0, -40.0])).expect("wind");
        assert_eq!(storm, GRASS_WIND_MAX_SCALE);
    }
}
