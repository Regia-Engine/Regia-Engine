// SPDX-License-Identifier: MPL-2.0
//! Terrain GPU resources: CDLOD heightfield draw, instanced scatter and the water plane, plus
//! the offscreen renderer used by tests and `samples/terrain_lab`.
//!
//! # Render hook contract (for the M5 render graph)
//!
//! All terrain GPU code lives in this crate behind one small trait, [`TerrainRenderer`]:
//! *"draw into this render pass with these camera uniforms"*. The host renderer:
//!
//! 1. calls [`TerrainRenderer::update_frame`] once per frame with the camera and environment
//!    ([`TerrainFrame`]: view-projection, camera position, sun direction/colour, ambient,
//!    wetness, time);
//! 2. calls [`TerrainRenderer::upload_nodes`] with the frame's [`LodSelection`]
//!    (from [`crate::select_nodes`]);
//! 3. draws [`TerrainRenderer::draw_terrain`] and [`TerrainRenderer::draw_scatter`] inside its
//!    **opaque** node, and [`TerrainRenderer::draw_water`] after it (translucent, depth-read
//!    only);
//! 4. provides targets of colour format `Rgba8UnormSrgb` and depth format `Depth32Float`
//!    (sample count 1). Bind group slots 0–3 of the terrain pipeline must be free — the
//!    terrain pipelines set 0 = frame uniforms, 1 = terrain data, 2 = per-node dynamic
//!    uniform, 3 = the single sun-cascade shadow map (scatter casts, terrain receives).
//!
//! The offscreen shaders write **linear** display-referred values (EV100-exposed sun + one
//! cascaded shadow + sky ambient, height fog, AgX tonemap); the `Rgba8UnormSrgb` target
//! applies the single sRGB encode on store.
//! Do not gamma-encode in the shaders — that would double-encode.
//!
//! [`TerrainSharedDraw`] is the runtime path: the same heightfield records into
//! [`regia_render::FrameRenderer`]'s depth, cascades, `Rgba16Float` HDR colour and motion
//! vectors. That path writes scene radiance (post applies exposure) and does not sample the
//! private sun map.
//!
//! [`render_offscreen`] is the reference consumer of the hook and the test path (lavapipe).

use std::collections::HashMap;

use regia_render::RenderDevice;

use crate::TerrainError;
use crate::cdlod::{CdlodConfig, LodSelection, QuadNode};
use crate::deform::{DirtyRect, MAX_SCORCH_MARKS};
use crate::height_mips::{HeightMips, SharedHeightMips};
use crate::holes::{HolesMask, NodeHoleState, build_node_indices_lattice_at, node_hole_state_lattice_at};
use crate::layers::{MAX_LAYERS, TerrainLayers};
use crate::material::MaterialParams;
use crate::sampler::TerrainMap;
use crate::scatter::ScatterInstance;

pub(crate) mod aniso_path;
pub mod grass;
pub mod impostor;
// The passes the shared draw ran last frame (ADR-8470).
pub mod pass_selections;
mod shadow_lists;
pub mod shared;
mod view;
pub mod vt;
pub mod warm;
use crate::water::WaterPlane;
pub use aniso_path::{
    ANISO_FOOTPRINT_ENV, ANISO_LOD_OFFSET_ENV, AnisotropyPath, MALI_ANISO_LOD_OFFSET, MAX_ANISO_LOD_OFFSET,
    RADV_ANISO_LOD_OFFSET, anisotropy_footprint_for, anisotropy_lod_offset, anisotropy_lod_offset_for,
    anisotropy_lod_offset_for_device, anisotropy_path,
};
pub use grass::GrassLayer;
pub use impostor::ImpostorLayer;
pub(crate) use shadow_lists::shadow_vertex_reach;
pub use shared::{
    BandDrawCounts, RenderedSurface, SHARED_MIN_GRID_SIZE, SkyAmbient, TerrainDrawStats, TerrainSharedDraw,
    full_detail_root_level,
};
pub(crate) use view::ViewGpu;
pub use vt::{TerrainVtHandle, VirtualTextureSettings, VtCache, VtDepthTest, VtStats};
pub use warm::{TerrainWarm, terrain_warm_formats, terrain_warm_job, warm_terrain_pipelines};

/// WGSL compiled by the terrain renderer.
///
/// `(name, source)` pairs for `cargo xtask check-shaders`. The same files are
/// created through `regia_render::wgsl_source` when the pipelines are built.
#[must_use]
pub fn shader_sources() -> [(&'static str, &'static str); 5] {
    [
        ("terrain.wgsl", include_str!("terrain.wgsl")),
        ("vt.wgsl", include_str!("vt.wgsl")),
        ("scatter.wgsl", include_str!("scatter.wgsl")),
        ("water.wgsl", include_str!("water.wgsl")),
        ("sky.wgsl", include_str!("sky.wgsl")),
    ]
}

/// Camera description for one terrain frame.
#[derive(Clone, Copy, Debug)]
pub struct TerrainCamera {
    /// World-space eye point.
    pub position: [f32; 3],
    /// Point the camera looks at.
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y_radians: f32,
    /// Aspect ratio (width / height).
    pub aspect: f32,
    /// Near plane distance, metres.
    pub near: f32,
    /// Far plane distance, metres.
    pub far: f32,
}

impl TerrainCamera {
    /// Builds the view-projection matrix (right-handed, zero-to-one depth, matching
    /// `regia_render`'s forward pass).
    pub fn view_projection(&self) -> [[f32; 4]; 4] {
        // The renderer's look-at: finite for a camera looking straight down (ADR-869).
        let view = regia_render::view_math::look_at_view(
            regia_core::glam::Vec3::from_array(self.position),
            regia_core::glam::Vec3::from_array(self.target),
            regia_core::glam::Vec3::Y,
        );
        // The renderer's clip planes too: `far = ∞` or a zero near plane would be NaN here.
        let (fov_y, near, far) =
            regia_render::view_math::usable_projection(self.fov_y_radians, self.near, self.far);
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(fov_y, self.aspect, near, far);
        (proj * view).to_cols_array_2d()
    }
}

/// Time-of-day input (spec 25 § 4 Terrain row: "time of day from map data").
///
/// Lighting follows the same EV100 convention as `regia_render::forward`: sun colour is the
/// spectral radiance of the sun disc (linear RGB, normalised so white = 1.0), and
/// `illuminance_lux` is the total visible-spectrum illuminance at the surface in lux
/// (~100 000 lx for a clear day at noon). The shader multiplies them by a fixed exposure
/// derived from EV100 14.5: `sun_radiance = sun_color * illuminance_lux / (1.2 * 2^14.5)` and
/// applies the π-normalised Lambert diffuse (`albedo/π · N·L`) before the AgX tonemap.
/// `ambient` is the sky-dome ambient light in the same linear, pre-exposed units and is added
/// to the sun term directly. The default is a clear sky: ≈ 23 klx of sky light against
/// ≈ 90 klx of direct sun on level ground (about 1 : 4). It used to be ~40 % of the sun so
/// shadowed slopes survived the old AgX fit, which crushed everything below 3 % linear;
/// with the corrected display transform shadows stay readable at the physical ratio and
/// the sunlit side is no longer flattened by a near-uniform ambient.
#[derive(Clone, Copy, Debug)]
pub struct TimeOfDay {
    /// Sun azimuth, radians (0 = +Z, growing toward +X).
    pub azimuth_radians: f32,
    /// Sun elevation, radians (0 = horizon, π/2 = zenith).
    pub elevation_radians: f32,
    /// Sun colour (linear RGB, normalised).
    pub sun_color: [f32; 3],
    /// Sun illuminance at the surface, lux (≈ 100 000 on a clear sunny day).
    pub illuminance_lux: f32,
    /// Sky ambient light (linear RGB, pre-exposed; added to the π-normalised sun term).
    pub ambient: [f32; 3],
}

impl Default for TimeOfDay {
    fn default() -> Self {
        Self {
            azimuth_radians: 0.9,
            elevation_radians: 0.95,
            sun_color: [1.25, 1.12, 0.95],
            illuminance_lux: 100_000.0,
            ambient: [0.17, 0.19, 0.24],
        }
    }
}

impl TimeOfDay {
    /// Illuminance on level ground, lux: the direct sun on a horizontal plane
    /// (`lux · sin(elevation)`, weighted by the sun colour's luminance) plus the sky
    /// ambient converted back from its pre-exposed units (`ambient · π / exposure`, the
    /// illuminance whose Lambert response equals the ambient term at EV100 14.5).
    #[must_use]
    pub fn horizontal_illuminance_lux(&self) -> f32 {
        let luminance = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let direct = self.illuminance_lux * self.elevation_radians.sin().max(0.0) * luminance(self.sun_color);
        let sky = luminance(self.ambient) * std::f32::consts::PI / REFERENCE_EXPOSURE;
        direct + sky
    }

    /// Exposure compensation (EV, ≥ 0) an incident-light meter would dial in relative to
    /// the reference sunny-day exposure (EV100 14.5, metered at [`TimeOfDay::default`]):
    /// `log2(E_reference / E)`, from the reflected-light-independent incident exposure
    /// relation `EV = log2(E · S / C)` (ISO 2720). A low morning or evening sun gets
    /// 1–2 EV more exposure instead of rendering the ground near-black. Capped at 3 EV.
    #[must_use]
    pub fn metered_exposure_compensation_ev(&self) -> f32 {
        let reference = Self::default().horizontal_illuminance_lux();
        let here = self.horizontal_illuminance_lux().max(1.0);
        (reference / here).log2().clamp(0.0, 3.0)
    }

    /// The unit vector toward the sun.
    pub fn sun_direction(&self) -> [f32; 3] {
        let (sin_az, cos_az) = self.azimuth_radians.sin_cos();
        let (sin_el, cos_el) = self.elevation_radians.sin_cos();
        [sin_az * cos_el, sin_el, cos_az * cos_el]
    }
}

/// Everything the terrain shaders need per frame (the "camera uniforms" of the hook).
#[derive(Clone, Copy, Debug)]
pub struct TerrainFrame {
    /// View-projection matrix (column-major).
    pub view_proj: [[f32; 4]; 4],
    /// World-space camera position.
    pub camera_pos: [f32; 3],
    /// Unit vector toward the sun.
    pub sun_dir: [f32; 3],
    /// Sun colour (linear RGB, normalised).
    pub sun_color: [f32; 3],
    /// Sun illuminance at the surface, lux.
    pub illuminance_lux: f32,
    /// Ambient colour (linear RGB).
    pub ambient: [f32; 3],
    /// Wetness parameter in `0..=1` (darkens and glosses the ground).
    pub wetness: f32,
    /// Animation time, seconds (water ripples, foam mottle).
    pub time_seconds: f32,
    /// Diagnostic render mode (lab/debug builds): `0` = full shading, `1` = LOD tint,
    /// `2` = N·L only, `3` = blended albedo, `4` = splat weights of the first three
    /// layers, `5` = slope, `6` = normalised height, `7` = visual-vs-sim agreement
    /// flags (green/red = driveable within/outside ±0.25 m, blue/yellow = cliff
    /// within/outside ±2 m), `8` = sun light split (R cascade shadow, G heightfield and
    /// micro self-shadow, B N·L), `9` = indirect split per unit albedo (R sky/ground
    /// ambient, G planet-shine, B terrain bounce), `10` = near-field detail (R strength,
    /// G icy, B frost coverage), `11` = cascade shadow alone (grey), `12` = cascade
    /// occluder distance, `13` = 2 km heightfield sun march, `14` = indirect occlusion
    /// inputs 1/2 (R heightfield cavity, G macro AO, B irradiance-probe coverage), `15` =
    /// indirect occlusion inputs 2/2 (R layers' ORM occlusion, G SSAO), `16` = shading normal
    /// (0.5 + 0.5·n), `17` = drawn geometry (R 0.5 + the cliff relief's vertical offset / 16 m,
    /// G eye distance / 1 km, B 1 as the scale reference). Debug views output raw
    /// linear values (the sRGB target
    /// encodes them); they bypass lighting and tonemap.
    pub debug_view: u32,
    /// Distance + exponential height fog ([`regia_render::FogSettings`]).
    pub fog: regia_render::FogSettings,
    /// World-space centre of the single sun-shadow cascade.
    pub shadow_focus: [f32; 3],
    /// Orthographic half-extent of that cascade, in metres.
    pub shadow_radius: f32,
    /// Exposure compensation in EV over the fixed EV100 14.5 reference: the sun radiance
    /// and the (pre-exposed) ambient are scaled by `2^ev`. `0` (the default from
    /// [`TerrainFrame::from_camera`]) is the reference exposure;
    /// [`TimeOfDay::metered_exposure_compensation_ev`] is what an incident meter picks.
    pub exposure_compensation_ev: f32,
}

/// Exposure at the EV100 14.5 sunny-day reference, `1 / (1.2 · 2^14.5)`
/// (`regia_render::forward`'s convention).
const REFERENCE_EXPOSURE: f32 = 1.0 / (1.2 * 23_170.475);

/// Resolution of the one sun cascade. One map, not a cascade chain: the terrain
/// hook fits a single ortho frustum around [`TerrainFrame::shadow_focus`].
///
/// 2048² on desktop: at the 200 m maximum radius a 1024² map has 39 cm texels, which
/// printed as stair-stepped terrain shadows in the DO27 beauty frames. Android keeps
/// 1024² (4 MiB instead of 16 MiB of depth).
#[cfg(not(target_os = "android"))]
const SUN_SHADOW_MAP: u32 = 2048;
/// Resolution of the one sun cascade (Android: 1024², see the desktop constant).
#[cfg(target_os = "android")]
const SUN_SHADOW_MAP: u32 = 1024;

impl TerrainFrame {
    /// Builds a frame from a camera and a time of day.
    ///
    /// Exposure is computed from EV100 14.5 (sunny-day reference), matching the convention in
    /// `regia_render::forward`:
    /// `exposure = 1.0 / (1.2 * 2^14.5)`, so `sun_radiance = sun_color * illuminance_lux * exposure`.
    pub fn from_camera(camera: &TerrainCamera, tod: &TimeOfDay, wetness: f32, time_seconds: f32) -> Self {
        Self {
            view_proj: camera.view_projection(),
            camera_pos: camera.position,
            sun_dir: tod.sun_direction(),
            sun_color: tod.sun_color,
            illuminance_lux: tod.illuminance_lux,
            ambient: tod.ambient,
            wetness,
            time_seconds,
            debug_view: 0,
            fog: regia_render::FogSettings::default(),
            shadow_focus: camera.target,
            shadow_radius: {
                let dx = camera.position[0] - camera.target[0];
                let dy = camera.position[1] - camera.target[1];
                let dz = camera.position[2] - camera.target[2];
                (dx * dx + dy * dy + dz * dz).sqrt() * 0.55
            }
            .clamp(24.0, 200.0),
            exposure_compensation_ev: 0.0,
        }
    }
}

/// One ortho cascade for the sun. `sun_dir` points toward the sun; the shadow camera
/// looks along the travel direction (from the sun toward the scene).
///
/// Texel snap is Zhang / Wronski: the focus is quantised by the cascade's world texel
/// so a static scene does not shimmer when the centre moves by a fraction of a texel.
/// Receiver bias (not a polygon offset) is `texel / depth_extent`, matching the
/// forward renderer's shadow uniforms.
fn sun_shadow_uniforms(frame: &TerrainFrame) -> ([[f32; 4]; 4], [f32; 4]) {
    use regia_core::glam::Vec3;
    use regia_core::glam::camera::rh::proj::directx::orthographic;
    use regia_core::glam::camera::rh::view::look_at_mat4;

    let sun = Vec3::from_array(frame.sun_dir);
    let sun = if sun.length_squared() > 1.0e-8 {
        sun.normalize()
    } else {
        Vec3::Y
    };
    let travel = -sun;
    let radius = frame.shadow_radius.max(1.0);
    let map = SUN_SHADOW_MAP as f32;
    let texel = (radius * 2.0) / map;
    let focus = Vec3::new(
        regia_render::snap_to_texels(frame.shadow_focus[0], texel),
        frame.shadow_focus[1],
        regia_render::snap_to_texels(frame.shadow_focus[2], texel),
    );
    // look-at is undefined when the view direction is parallel to the up axis.
    let up = if travel.dot(Vec3::Y).abs() > 0.95 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let eye = focus - travel * radius;
    let view = look_at_mat4(eye, focus, up);
    let near = 0.05;
    let far = (radius * 3.0).max(near + 1.0);
    let proj = orthographic(-radius, radius, -radius, radius, near, far);
    let depth_extent = (far - near).max(1.0);
    let bias = texel / depth_extent * 1.5;
    ((proj * view).to_cols_array_2d(), [1.0, bias, texel, map])
}

// ---------------------------------------------------------------------------
// Uniform mirrors (WGSL layouts; sizes and offsets asserted)
// ---------------------------------------------------------------------------
//
// WGSL uniform-buffer alignment rules differ from Rust `#[repr(C)]`:
// `vec2` → align 8, `vec3` → align 16, `vec4` → align 16, `mat4x4` → align 16.
// Every struct here is verified to match its WGSL counterpart byte-for-byte.
// See `regia_render::forward::DirectionalLightUniform` for the same pattern.

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct FrameUniform {
    view_proj: [[f32; 4]; 4],     // @0,  64 B, align 4 (matches WGSL mat4x4 column layout)
    camera_pos_time: [f32; 4],    // @64, 16 B
    sun_dir_wetness: [f32; 4],    // @80, 16 B
    sun_color_exposure: [f32; 4], // @96, 16 B  — rgb = sun colour, a = illuminance * exposure
    ambient_debug: [f32; 4],      // @112, 16 B — rgb = ambient, a = debug view mode
    fog_params: [f32; 4],         // @128, 16 B — density, height_falloff, base_height, enabled
    fog_color: [f32; 4],          // @144, 16 B
    light_view_proj: [[f32; 4]; 4], // @160, 64 B — single sun cascade
    shadow_params: [f32; 4],      // @224, 16 B — enabled, depth bias, texel world size, map resolution
    /// `x` = 1 writes scene radiance (shared HDR); `z` = sky radiance scale for fog; `w` > 0:
    /// the sky light is the engine's irradiance map (`@group(0) @binding(1)`) times `w`.
    output_mode: [f32; 4], // @240, 16 B
    /// Previous frame's jittered view-projection (motion vectors).
    prev_view_proj: [[f32; 4]; 4], // @256, 64 B
    /// This frame's view-projection without TAA jitter.
    unjittered_view_proj: [[f32; 4]; 4], // @320, 64 B
    /// rgb = ground-bounce ambient (same units as `ambient_debug.rgb`), w = 1 when the
    /// hemisphere sky/ground model replaces the legacy sky-visibility ambient.
    ground_ambient: [f32; 4], // @384, 16 B
    /// Planet-shine fill: xyz toward the sky body, w = 1 when on.
    fill_dir: [f32; 4], // @400, 16 B
    /// Planet-shine colour · lux, scaled like the sun's illuminance; w = the shared HDR
    /// target's pre-exposure (the linear output is multiplied by it).
    fill_color: [f32; 4], // @416, 16 B
    /// `x` = texture mip bias (log2 units, ≤ 0; `TerrainGpu::set_texture_lod_bias`); `y` = 1:
    /// write the stochastic-SSR surface code in the HDR alpha (`TerrainGpu::set_sssr_codes`);
    /// `z` = anisotropic filtering level (0 = off, `TerrainGpu::set_anisotropy`); `w` = 1: the
    /// shader assembles the anisotropic layer taps itself (`TerrainGpu::set_anisotropy_footprint`),
    /// `w` ≤ 0: the sampler path, filtering `-w` mips softer (`TerrainGpu::set_anisotropy_lod_offset`).
    texture_lod: [f32; 4], // @432, 16 B
    /// The frame's extra directional lights (`TerrainGpu::set_extra_lights`, ADR-7254): per slot
    /// the travel direction (`w` = 1 when on), then colour · lux scaled like the sun's illuminance.
    extra_dir: [[f32; 4]; 2 * regia_render::MAX_EXTRA_DIRECTIONAL], // @448, 96 B
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct TerrainUniform {
    half_width: f32,      // @0
    world_width: f32,     // @4
    height_scale: f32,    // @8
    height_offset: f32,   // @12
    sample_max: [f32; 2], // @16 (WGSL vec2 wants align 8; 16 is 8-aligned ✓)
    grid_size: f32,       // @24
    root_level: f32,      // @28
    /// x = mip levels of the height texture; yzw unused.
    mip_info: [f32; 4], // @32
    /// x = half depth (`world_depth / 2 − origin z`), y = world depth, zw = 0.
    /// On a square map x equals `half_width` and y equals `world_width` (ADR-8369).
    depth: [f32; 4], // @48
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct LayerSlotUniform {
    albedo_roughness: [f32; 4], // @0,  16 B
    slope_alt: [f32; 4],        // @16, 16 B
    params: [f32; 4],           // @32, 16 B — tiling, blend_sharpness, triplanar, texel size
    params2: [f32; 4],          // @48, 16 B — detail strength, height blend, has height, has albedo
    params3: [f32; 4],          // @64, 16 B — has_orm, weather porosity, weather snow, stack weather flags
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct LayersUniform {
    count: [f32; 4],                       // @0,  16 B
    slots: [LayerSlotUniform; MAX_LAYERS], // @16, 80*N B
}

/// Photoreal material + sim-ground uniform (WGSL `Material`, binding 12 of group 1).
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct MaterialUniform {
    /// `enabled` (1 when a material is set), `macro_world_size`, `anti_tiling_cell`,
    /// `macro_strength`.
    params0: [f32; 4], // @0
    /// `detail_fade_start`, `detail_fade_end`, `triplanar_auto_slope`, `macro_normal_strength`.
    params1: [f32; 4], // @16
    /// Sim ground mapping: `half_width`, `world_width`, `height_scale`, `height_offset`
    /// (enabled when `world_width > 0`).
    sim: [f32; 4], // @32
    /// `tiles` per side, `1/tiles`, 0 (the old one-lane residency mask; [`Self::macro_mask`]
    /// replaced it, ADR-3367), macro texel.
    tiles: [f32; 4], // @48
    /// Road segment world XZ: `ax, az, bx, bz`.
    road0: [f32; 4], // @64
    /// `half_width` metres, road colour rgb. `half_width <= 0` disables the decal.
    road1: [f32; 4], // @80
    /// Strata: strength, bed thickness (m), warp amplitude (m), ledge strength.
    strata: [f32; 4], // @96
    /// Relief: sun-occlusion reach (m), cavity/crust strength, glint strength, near detail.
    relief: [f32; 4], // @112
    /// Strata dip `tan(dip)·dir` (x, z), lateral drift (m), talus strength.
    strata2: [f32; 4], // @128
    /// Fractured-rock detail normal strength, mid-range relief strength, terrain bounce,
    /// `1 −` layer occlusion strength (0 keeps the layers' ORM occlusion as authored).
    rock: [f32; 4], // @144
    /// Cliff relief: strength, fine-grid radius (m), fine-grid band (m), minimum slope (rad).
    cliff: [f32; 4], // @160
    /// [`crate::TerrainGroundLook::uniforms`] (ADR-1408): ground variation and remix.
    ground: [f32; 4], // @176
    /// Snow amount, snow line (m), aspect, melt edge.
    winter: [f32; 4], // @192
    /// Snow patch size (m), mud wetness, hoarfrost, 0.
    winter2: [f32; 4], // @208
    /// Macro tile residency (ADR-3367, [`crate::material::macro_residency`]): bit `i` set when
    /// tile `i` is resident, 256 bits (a 16 × 16 grid). WGSL `array<vec4<u32>, 2>`.
    macro_mask: [u32; crate::material::MACRO_RESIDENCY_WORDS], // @224
    /// Resident tiles before each word of [`Self::macro_mask`] (a tile's layer in the array is
    /// this plus its rank inside its word).
    macro_prefix: [u32; crate::material::MACRO_RESIDENCY_WORDS], // @256
    /// Surface weather (spec 99 § 6.4): wetness, puddles, snow cover, ripple. The weather rows
    /// stay last: the dry variant strips them from the WGSL struct.
    weather: [f32; 4], // @288
    /// The weather clock, 1 with the occlusion tile, the extinction (1/m), the atlas edge.
    weather2: [f32; 4], // @304
    /// The occlusion tile's view-projection.
    weather_tile: [[f32; 4]; 4], // @320
    /// The tile's scale and offset in the atlas.
    weather_rect: [f32; 4], // @384
    /// (texel m, 1 / depth extent, 0, 0).
    weather_bias: [f32; 4], // @400
}

/// Near-field relief and strata of the photoreal material path ([`TerrainGpu::set_relief`]).
///
/// The defaults are on: dipping, drifting strata with broken ledges and a fractured-rock
/// normal on steep faces, talus at their foot, heightfield self-shadowing, crust and
/// concavity occlusion, and glints on smooth bright layers (ice, snow). `TerrainRelief::off()` restores the
/// pre-2026-10-07 photoreal look (no strata, no self-shadowing, no cavity, no glints).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerrainRelief {
    /// Strength of the altitude-banded strata on steep faces, `0..=1` (0 = off).
    pub strata_strength: f32,
    /// Mean bed thickness of the strata, metres.
    pub strata_bed_m: f32,
    /// Amplitude of the noise warp that bends the beds, metres.
    pub strata_warp_m: f32,
    /// Strength of the broken ledges where a harder bed protrudes, `0..=2`.
    pub strata_ledge: f32,
    /// Dip of the beds, degrees from horizontal: a band climbs across the map instead of
    /// tracing an altitude contour.
    pub strata_dip_deg: f32,
    /// Compass direction the beds dip toward, degrees (0 = +Z, 90 = +X).
    pub strata_dip_azimuth_deg: f32,
    /// Amplitude of the large-scale lateral drift of the beds (warps over 40–170 m), metres.
    pub strata_drift_m: f32,
    /// Fractured-rock detail normal on steep faces (1–50 m), `0..=2` (0 = off).
    pub rock_detail: f32,
    /// Darker debris at the concave foot of faces, `0..=1` (0 = off).
    pub talus: f32,
    /// How far the heightfield self-shadow march reaches toward the sun near the camera,
    /// metres (it grows with distance); `0` turns it off.
    pub self_shadow_reach_m: f32,
    /// Crust (layer-height self-occlusion) and concavity occlusion of the sky light, `0..=1`.
    pub cavity: f32,
    /// Discrete-facet sparkle on bright crystalline layers (ice, snow), `0..=4` (0 = off).
    pub glints: f32,
    /// Near-field detail within ~30 m, `0..=2` (0 = off): broken crust plates and frost
    /// flakes on bright layers, fine grain and pebbles on darker ones, with micro shadows.
    pub near_detail: f32,
    /// Mid-range relief 12–250 m, `0..=2` (0 = off): a world-space 2–20 m rock-and-pit
    /// normal on non-steep ground, darker regolith in its hollows and on cool (icy) ground
    /// in 30–60 m patches, and a short self-shadow toward the sun.
    pub mid_relief: f32,
    /// Bounce of sunlight off the surrounding terrain onto this point, `0..=2` (0 = off;
    /// 1 = physical): sunlit walls light the shadowed floor between them (shared path).
    pub bounce: f32,
    /// Geometric relief on steep faces, `0..=2` (0 = off): CDLOD vertices on faces steeper
    /// than [`CLIFF_RELIEF_MIN_SLOPE_RAD`] move along the face normal by the same buttress,
    /// ledge and flute function the face shading uses, so silhouettes and shading agree.
    /// Nodes on steep ground within [`CLIFF_DENSE_RADIUS_M`] of the camera draw a grid twice
    /// as fine. Driveable ground is never displaced.
    pub cliff_relief: f32,
    /// Strength of the layers' ORM occlusion (R channel) on the indirect light, `0..=1`
    /// (1 = as authored, 0 = ignored): glTF 2.0 `occlusionTexture.strength`, for layer sets
    /// whose baked occlusion carries more than texture-scale crevices (DO27's aerial-scan
    /// sand bakes dune-scale shading into it).
    pub layer_occlusion: f32,
}

/// Slope from up (radians) where [`TerrainRelief::cliff_relief`] starts to displace a face;
/// full strength 0.3 rad steeper. Above [`crate::DRIVEABLE_SLOPE_RAD`], so the driveable
/// surface (and `rendered_height` on it) is untouched.
pub const CLIFF_RELIEF_MIN_SLOPE_RAD: f32 = 0.7;

/// Nodes with steep ground within this distance of the camera draw a grid twice as fine
/// (the extra vertices fade out over [`CLIFF_DENSE_BAND_M`] before it).
pub const CLIFF_DENSE_RADIUS_M: f32 = 140.0;

/// Width of the fade from the fine grid to the regular one, metres.
pub const CLIFF_DENSE_BAND_M: f32 = 40.0;

/// Largest displacement cliff relief applies at strength 2, metres (culling bounds pad).
pub const CLIFF_RELIEF_MAX_M: f32 = 8.0;

/// Height-edit rectangles the virtual texture keeps between two prepares before it merges them
/// into one (a guard for a draw that is edited but not drawn): 16 edits a frame for 16 frames
/// (ADR-7585; it was 16, so a busy frame invalidated the ground between distant craters).
pub const VT_EDIT_RECTS_MAX: usize = 256;

/// Virtual-texture invalidations by runtime height edits ([`TerrainGpu::vt_edit_counters`],
/// ADR-7585).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VtEditCounters {
    /// Edit rectangles (`[min_x, min_z, max_x, max_z]` world metres, grown by
    /// [`vt::HEIGHT_REACH_M`] and one sample) the texture tested.
    pub rects: u64,
    /// Resident pages those rectangles marked for an in-place re-bake.
    pub pages: u64,
    /// Times more than [`VT_EDIT_RECTS_MAX`] rectangles waited and were merged into one.
    pub merges: u64,
    /// The last rectangle tested.
    pub last_rect: Option<[f32; 4]>,
}

/// Nodes whose hole state [`TerrainGpu`] keeps before it starts the cache over (a few
/// hundred bytes per thousand nodes; a frame selects a few hundred).
const HOLE_STATE_CACHE_MAX: usize = 65_536;

impl Default for TerrainRelief {
    fn default() -> Self {
        Self {
            strata_strength: 0.85,
            strata_bed_m: 1.8,
            strata_warp_m: 3.0,
            strata_ledge: 1.4,
            strata_dip_deg: 4.0,
            strata_dip_azimuth_deg: 35.0,
            strata_drift_m: 9.0,
            rock_detail: 1.2,
            talus: 0.6,
            self_shadow_reach_m: 40.0,
            cavity: 0.6,
            glints: 1.0,
            near_detail: 1.0,
            mid_relief: 1.0,
            bounce: 1.0,
            cliff_relief: 1.0,
            layer_occlusion: 1.0,
        }
    }
}

impl TerrainRelief {
    /// Everything off: the photoreal look before strata and relief lighting.
    #[must_use]
    pub fn off() -> Self {
        Self {
            strata_strength: 0.0,
            strata_bed_m: 1.8,
            strata_warp_m: 0.0,
            strata_ledge: 0.0,
            strata_dip_deg: 0.0,
            strata_dip_azimuth_deg: 0.0,
            strata_drift_m: 0.0,
            rock_detail: 0.0,
            talus: 0.0,
            self_shadow_reach_m: 0.0,
            cavity: 0.0,
            glints: 0.0,
            near_detail: 0.0,
            mid_relief: 0.0,
            bounce: 0.0,
            cliff_relief: 0.0,
            layer_occlusion: 1.0,
        }
    }

    /// `cliff`: relief strength, fine-grid radius (m), fine-grid fade band (m), minimum slope.
    fn cliff_uniform(&self) -> [f32; 4] {
        let s = if self.cliff_relief.is_finite() {
            self.cliff_relief.clamp(0.0, 2.0)
        } else {
            0.0
        };
        [
            s,
            CLIFF_DENSE_RADIUS_M,
            CLIFF_DENSE_BAND_M,
            CLIFF_RELIEF_MIN_SLOPE_RAD,
        ]
    }

    fn uniforms(&self) -> ([f32; 4], [f32; 4], [f32; 4], [f32; 4]) {
        let finite = |v: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { 0.0 };
        let dip = finite(self.strata_dip_deg, 0.0, 60.0).to_radians().tan();
        let (sin_az, cos_az) = finite(self.strata_dip_azimuth_deg, -360.0, 360.0)
            .to_radians()
            .sin_cos();
        (
            [
                finite(self.strata_strength, 0.0, 1.0),
                finite(self.strata_bed_m, 0.1, 50.0),
                finite(self.strata_warp_m, 0.0, 100.0),
                finite(self.strata_ledge, 0.0, 2.0),
            ],
            [
                finite(self.self_shadow_reach_m, 0.0, 400.0),
                finite(self.cavity, 0.0, 1.0),
                finite(self.glints, 0.0, 4.0),
                finite(self.near_detail, 0.0, 2.0),
            ],
            [
                dip * sin_az,
                dip * cos_az,
                finite(self.strata_drift_m, 0.0, 200.0),
                finite(self.talus, 0.0, 1.0),
            ],
            [
                finite(self.rock_detail, 0.0, 2.0),
                finite(self.mid_relief, 0.0, 2.0),
                finite(self.bounce, 0.0, 2.0),
                1.0 - if self.layer_occlusion.is_finite() {
                    self.layer_occlusion.clamp(0.0, 1.0)
                } else {
                    1.0
                },
            ],
        )
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ScorchUniform {
    rows: [[f32; 4]; MAX_SCORCH_MARKS + 1],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct NodeUniform {
    rect_min: [f32; 2], // @0,  8 B  (WGSL vec2 wants align 8, offset 0 is 8-aligned ✓)
    rect_size: f32,     // @8
    level: f32,         // @12
    morph_start: f32,   // @16
    morph_end: f32,     // @20
    grid_size: f32,     // @24
    root_level: f32,    // @28
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ScatterParamsUniform {
    fade_start: f32, // @0
    fade_end: f32,   // @4
    align_tilt: f32, // @8
    pad: f32,        // @12
}

/// One scatter instance on the GPU (vertex buffer, instance step).
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ScatterInstanceGpu {
    /// `xyz` position (rock origin at its base), `w` yaw.
    pub pos_yaw: [f32; 4],
    /// `x` uniform scale.
    pub scale_pad: [f32; 4],
    /// Surface normal (for aligned drawing).
    pub normal: [f32; 4],
    /// Tint multiplier.
    pub tint: [f32; 4],
}

impl From<&ScatterInstance> for ScatterInstanceGpu {
    fn from(instance: &ScatterInstance) -> Self {
        Self {
            pos_yaw: [
                instance.position[0],
                instance.position[1],
                instance.position[2],
                instance.yaw,
            ],
            scale_pad: [instance.scale, 0.0, 0.0, 0.0],
            normal: [instance.normal[0], instance.normal[1], instance.normal[2], 0.0],
            tint: [instance.tint[0], instance.tint[1], instance.tint[2], 1.0],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct WaterUniform {
    rect_min: [f32; 2],    // @0,  8 B
    rect_size: f32,        // @8
    level: f32,            // @12
    shallow: [f32; 4],     // @16, 16 B
    deep: [f32; 4],        // @32, 16 B
    foam: [f32; 4],        // @48, 16 B
    wave_params: [f32; 4], // @64, 16 B — foam_width, max_depth, alpha, wave_amp
    freq_pad: [f32; 4],    // @80, 16 B — wave_freq, refraction flag, padding
}

/// The water uniform for `water` over the map's `world_width` square centred on `origin`;
/// `refraction` tells the shader a scene copy is bound (`freq_pad[1]`).
fn water_uniform(world_width: f32, origin: [f32; 2], water: &WaterPlane, refraction: bool) -> WaterUniform {
    WaterUniform {
        rect_min: [-world_width * 0.5 + origin[0], -world_width * 0.5 + origin[1]],
        rect_size: world_width,
        level: water.level,
        shallow: [
            water.shallow_color[0],
            water.shallow_color[1],
            water.shallow_color[2],
            0.0,
        ],
        deep: [water.deep_color[0], water.deep_color[1], water.deep_color[2], 0.0],
        foam: [water.foam_color[0], water.foam_color[1], water.foam_color[2], 0.0],
        wave_params: [
            water.foam_width,
            water.max_depth,
            water.alpha,
            water.wave_amplitude,
        ],
        freq_pad: [water.wave_frequency, if refraction { 1.0 } else { 0.0 }, 0.0, 0.0],
    }
}

/// 1×1 stand-ins for the water's refraction sources (scene color, scene depth) while no scene
/// copy is bound.
fn water_scene_stand_in(device: &wgpu::Device) -> (wgpu::TextureView, wgpu::TextureView) {
    let make = |label: &'static str, format: wgpu::TextureFormat, usage: wgpu::TextureUsages| {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    };
    (
        make(
            "terrain water scene stand-in",
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::TEXTURE_BINDING,
        ),
        make(
            "terrain water depth stand-in",
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        ),
    )
}

/// The water plane's bind group (group 1 of the water pipeline): the height texture, the
/// terrain uniform, the plane's uniform and the refraction sources `scene` (color, depth).
fn water_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    height_view: &wgpu::TextureView,
    terrain_buffer: &wgpu::Buffer,
    water_buffer: &wgpu::Buffer,
    scene: (&wgpu::TextureView, &wgpu::TextureView),
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("terrain water bind"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(height_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: terrain_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: water_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(scene.0),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::TextureView(scene.1),
            },
        ],
    })
}

// Size asserts.
const _: () = assert!(size_of::<FrameUniform>() == 544);
const _: () = assert!(size_of::<TerrainUniform>() == 64);
const _: () = assert!(size_of::<LayersUniform>() == 16 + 80 * MAX_LAYERS);
const _: () = assert!(size_of::<MaterialUniform>() == 416);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, macro_mask) == 224);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, weather) == 288);
const _: () = assert!(size_of::<ScorchUniform>() == 16 * (MAX_SCORCH_MARKS + 1));
const _: () = assert!(size_of::<NodeUniform>() == 32);
const _: () = assert!(size_of::<ScatterParamsUniform>() == 16);
const _: () = assert!(size_of::<ScatterInstanceGpu>() == 64);
const _: () = assert!(size_of::<WaterUniform>() == 96);

// Offset asserts — verify every field sits at the same byte offset as in the WGSL counterpart.
// Any mismatch silently breaks the shader (as happened in regia_render before the fix).
const _: () = assert!(std::mem::offset_of!(FrameUniform, view_proj) == 0);
const _: () = assert!(std::mem::offset_of!(FrameUniform, camera_pos_time) == 64);
const _: () = assert!(std::mem::offset_of!(FrameUniform, sun_dir_wetness) == 80);
const _: () = assert!(std::mem::offset_of!(FrameUniform, sun_color_exposure) == 96);
const _: () = assert!(std::mem::offset_of!(FrameUniform, ambient_debug) == 112);
const _: () = assert!(std::mem::offset_of!(FrameUniform, fog_params) == 128);
const _: () = assert!(std::mem::offset_of!(FrameUniform, fog_color) == 144);
const _: () = assert!(std::mem::offset_of!(FrameUniform, light_view_proj) == 160);
const _: () = assert!(std::mem::offset_of!(FrameUniform, shadow_params) == 224);
const _: () = assert!(std::mem::offset_of!(FrameUniform, output_mode) == 240);
const _: () = assert!(std::mem::offset_of!(FrameUniform, prev_view_proj) == 256);
const _: () = assert!(std::mem::offset_of!(FrameUniform, unjittered_view_proj) == 320);
const _: () = assert!(std::mem::offset_of!(FrameUniform, ground_ambient) == 384);
const _: () = assert!(std::mem::offset_of!(FrameUniform, fill_dir) == 400);
const _: () = assert!(std::mem::offset_of!(FrameUniform, fill_color) == 416);
const _: () = assert!(std::mem::offset_of!(FrameUniform, texture_lod) == 432);
const _: () = assert!(std::mem::offset_of!(FrameUniform, extra_dir) == 448);

/// One cascade matrix for the shared shadow pass (group 4).
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct CascadeUniform {
    light_view_proj: [[f32; 4]; 4],
    /// `[x, 0, z, 1]`: the eye the cascade's node list was selected from (its morph
    /// centre); `w = 0` morphs around the frame camera.
    morph_eye: [f32; 4],
}

/// Engine CSM for the shared colour pass. Layout matches `EngineCsm` in `terrain.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct EngineCsmUniform {
    vp0: [[f32; 4]; 4],
    vp1: [[f32; 4]; 4],
    vp2: [[f32; 4]; 4],
    vp3: [[f32; 4]; 4],
    split: [f32; 4],
    bias0: [f32; 4],
    bias1: [f32; 4],
    bias2: [f32; 4],
    bias3: [f32; 4],
    /// `x` = cascade count, `w` = PCF tap count (0 keeps 16 in the shader).
    params: [f32; 4],
    /// `x`, `y` = colour target size in pixels, `z` = 1 when the engine SSAO is bound.
    screen: [f32; 4],
    /// The engine's cloud transmittance map (ADR-320): extent, layer base and top, `w` = 1 when
    /// the sun term reads it. Zero leaves the colour pass as before.
    cloud: [f32; 4],
    /// xyz: the local origin in the terrain's space (`SharedFrameInfo::origin_offset`, zero until a
    /// floating-origin rebase). The terrain shades in its own space; the engine's froxel lights
    /// and cloud map are local. w unused.
    origin: [f32; 4],
}

const _: () = assert!(size_of::<EngineCsmUniform>() == 400);
const _: () = assert!(std::mem::offset_of!(EngineCsmUniform, split) == 256);
const _: () = assert!(std::mem::offset_of!(EngineCsmUniform, params) == 336);

fn frame_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
    sky_irradiance: &wgpu::TextureView,
    sky_sampler: &wgpu::Sampler,
    brdf: &wgpu::TextureView,
    occluders: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("terrain frame bind"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(sky_irradiance),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sky_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(brdf),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: occluders.as_entire_binding(),
            },
        ],
    })
}

/// First binding of the engine's clustered point and spot lights in the engine-CSM group
/// (`regia_render::shared_lights_layout_entries`; bindings 10–16).
pub(crate) const ENGINE_LIGHTS_BINDING: u32 = 10;

/// Binding of the engine's cloud transmittance map in the engine-CSM group (after the lights).
pub(crate) const ENGINE_CLOUD_BINDING: u32 = 17;
/// The frame's emitter probe atlas with the lights' IES profile bands, in the engine-CSM group when it carries the
/// lights (`regia_render::shared_light_profiles_layout_entry`, ADR-3247).
pub(crate) const ENGINE_LIGHT_PROFILES_BINDING: u32 = 19;
/// The shadow slot past the cascades that draws into the precipitation occlusion tile.
pub(crate) const PRECIP_SHADOW_SLOT: u32 = 4;

/// First `vertex_index` of a mouth-band draw (ADR-8569): `terrain.wgsl`'s `displaced` reads a
/// vertex at or past it from the band words, not from the node grid (a dense node grid has at
/// most 257² vertices, far below).
pub const BAND_VERTEX_BASE: u32 = 1 << 24;

/// Band vertex sets per camera in the hole words: the camera passes (colour, depth, velocity,
/// receiver) and the precipitation occlusion tile share set 0, sun cascade `i` reads set `1 + i`
/// (ADR-8569, ADR-8824).
pub const BAND_PASSES: usize = 5;

/// Cameras with their own [`BAND_PASSES`] band sets (ADR-8824): 0 is the host's main camera, 1
/// the texture view being drawn (a probe capture, mirror, screen or stereo eye). Texture views
/// take turns on camera 1: each view's prepare places its own selection there before its passes.
pub const BAND_CAMERAS: usize = 2;
/// The shadow atlas with the precipitation occlusion tile (spec 99 § 6.4), in the engine group.
pub(crate) const ENGINE_PRECIP_BINDING: u32 = 18;

#[allow(clippy::too_many_arguments)] // one bind group: four cascades, sampler, uniform, AO, contact, probes, lights, cloud map
pub(crate) fn engine_csm_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    views: [&wgpu::TextureView; 4],
    sampler: &wgpu::Sampler,
    buffer: &wgpu::Buffer,
    ao: &wgpu::TextureView,
    contact: &wgpu::TextureView,
    gi: (&wgpu::Buffer, &wgpu::Buffer),
    lights: Option<(&regia_render::EngineLights, &wgpu::TextureView)>,
    cloud: &wgpu::TextureView,
    precip: &wgpu::TextureView,
) -> wgpu::BindGroup {
    fn depth(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
        wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::TextureView(view),
        }
    }
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("terrain engine csm"),
        layout,
        entries: &[
            depth(0, views[0]),
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            depth(2, views[1]),
            depth(3, views[2]),
            depth(4, views[3]),
            wgpu::BindGroupEntry {
                binding: 5,
                resource: buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 6,
                resource: wgpu::BindingResource::TextureView(ao),
            },
            wgpu::BindGroupEntry {
                binding: 7,
                resource: wgpu::BindingResource::TextureView(contact),
            },
            wgpu::BindGroupEntry {
                binding: 8,
                resource: gi.0.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 9,
                resource: gi.1.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: ENGINE_CLOUD_BINDING,
                resource: wgpu::BindingResource::TextureView(cloud),
            },
            wgpu::BindGroupEntry {
                binding: ENGINE_PRECIP_BINDING,
                resource: wgpu::BindingResource::TextureView(precip),
            },
        ]
        .into_iter()
        .chain(
            lights
                .map(|(l, _)| l.bind_entries(ENGINE_LIGHTS_BINDING, sampler))
                .into_iter()
                .flatten(),
        )
        .chain(lights.map(|(_, profiles)| wgpu::BindGroupEntry {
            binding: ENGINE_LIGHT_PROFILES_BINDING,
            resource: wgpu::BindingResource::TextureView(profiles),
        }))
        .collect::<Vec<_>>(),
    })
}

const _: () = assert!(size_of::<CascadeUniform>() == 80);

const _: () = assert!(std::mem::offset_of!(TerrainUniform, half_width) == 0);
const _: () = assert!(std::mem::offset_of!(TerrainUniform, world_width) == 4);
const _: () = assert!(std::mem::offset_of!(TerrainUniform, height_scale) == 8);
const _: () = assert!(std::mem::offset_of!(TerrainUniform, height_offset) == 12);
const _: () = assert!(std::mem::offset_of!(TerrainUniform, sample_max) == 16);
const _: () = assert!(std::mem::offset_of!(TerrainUniform, grid_size) == 24);
const _: () = assert!(std::mem::offset_of!(TerrainUniform, root_level) == 28);
const _: () = assert!(std::mem::offset_of!(TerrainUniform, mip_info) == 32);
const _: () = assert!(std::mem::offset_of!(TerrainUniform, depth) == 48);

const _: () = assert!(std::mem::offset_of!(LayerSlotUniform, albedo_roughness) == 0);
const _: () = assert!(std::mem::offset_of!(LayerSlotUniform, slope_alt) == 16);
const _: () = assert!(std::mem::offset_of!(LayerSlotUniform, params) == 32);
const _: () = assert!(std::mem::offset_of!(LayerSlotUniform, params2) == 48);
const _: () = assert!(std::mem::offset_of!(LayerSlotUniform, params3) == 64);

const _: () = assert!(std::mem::offset_of!(LayersUniform, count) == 0);
const _: () = assert!(std::mem::offset_of!(LayersUniform, slots) == 16);

const _: () = assert!(std::mem::offset_of!(MaterialUniform, params0) == 0);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, params1) == 16);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, sim) == 32);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, tiles) == 48);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, road0) == 64);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, road1) == 80);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, strata) == 96);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, relief) == 112);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, strata2) == 128);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, rock) == 144);
const _: () = assert!(std::mem::offset_of!(MaterialUniform, cliff) == 160);

const _: () = assert!(std::mem::offset_of!(NodeUniform, rect_min) == 0);
const _: () = assert!(std::mem::offset_of!(NodeUniform, rect_size) == 8);
const _: () = assert!(std::mem::offset_of!(NodeUniform, level) == 12);
const _: () = assert!(std::mem::offset_of!(NodeUniform, morph_start) == 16);
const _: () = assert!(std::mem::offset_of!(NodeUniform, morph_end) == 20);
const _: () = assert!(std::mem::offset_of!(NodeUniform, grid_size) == 24);
const _: () = assert!(std::mem::offset_of!(NodeUniform, root_level) == 28);

const _: () = assert!(std::mem::offset_of!(ScatterParamsUniform, fade_start) == 0);
const _: () = assert!(std::mem::offset_of!(ScatterParamsUniform, fade_end) == 4);
const _: () = assert!(std::mem::offset_of!(ScatterParamsUniform, align_tilt) == 8);
const _: () = assert!(std::mem::offset_of!(ScatterParamsUniform, pad) == 12);

const _: () = assert!(std::mem::offset_of!(WaterUniform, rect_min) == 0);
const _: () = assert!(std::mem::offset_of!(WaterUniform, rect_size) == 8);
const _: () = assert!(std::mem::offset_of!(WaterUniform, level) == 12);
const _: () = assert!(std::mem::offset_of!(WaterUniform, shallow) == 16);
const _: () = assert!(std::mem::offset_of!(WaterUniform, deep) == 32);
const _: () = assert!(std::mem::offset_of!(WaterUniform, foam) == 48);
const _: () = assert!(std::mem::offset_of!(WaterUniform, wave_params) == 64);
const _: () = assert!(std::mem::offset_of!(WaterUniform, freq_pad) == 80);

/// The terrain draw hook: draw into this render pass with these camera uniforms.
///
/// See the module docs for the exact contract (formats, bind group slots, pass order).
pub trait TerrainRenderer {
    /// Uploads the frame uniforms (camera, sun, time).
    fn update_frame(&self, frame: &TerrainFrame);

    /// Uploads the per-node uniforms of this frame's selection (growing the buffer if needed)
    /// and prepares hole-culled index buffers.
    fn upload_nodes(&mut self, selection: &LodSelection);

    /// Draws a gradient sky background (full-screen triangle, no depth write).
    fn draw_sky(&self, pass: &mut wgpu::RenderPass<'_>);

    /// Draws the terrain surface (opaque, depth write).
    fn draw_terrain(&self, pass: &mut wgpu::RenderPass<'_>, selection: &LodSelection);

    /// Draws the instanced scatter (opaque with distance fade).
    fn draw_scatter(&self, pass: &mut wgpu::RenderPass<'_>);

    /// Draws the water plane (translucent, no depth write).
    fn draw_water(&self, pass: &mut wgpu::RenderPass<'_>);
}

/// Sun cascade cache state (see [`TerrainGpu::encode_sun_shadow`]).
#[derive(Default)]
struct SunShadowCache {
    /// View-projection of the last `update_frame`.
    vp: [[f32; 4]; 4],
    /// View-projection and scatter generation of the last render.
    drawn: Option<([[f32; 4]; 4], u64)>,
}

/// All GPU state of one terrain: height texture, pipelines, bind groups, per-frame buffers.
pub struct TerrainGpu {
    device: regia_render::TrackedDevice,
    queue: wgpu::Queue,
    colour_format: wgpu::TextureFormat,
    depth_format: wgpu::TextureFormat,

    height_texture: wgpu::Texture,
    world_width: f32,
    /// The map's Z extent when the draw was built ([`TerrainMap::world_depth`], ADR-8369).
    /// Equal to `world_width` for a square map.
    world_depth: f32,
    /// The map's grid origin when the draw was built ([`TerrainMap::grid_origin`], ADR-8179).
    grid_origin: [f32; 2],

    frame_layout: wgpu::BindGroupLayout,
    frame_buffer: wgpu::Buffer,
    /// `MeshOccluderBlock` of the shared frame (zero count elsewhere), group 0 binding 4.
    occluder_buffer: wgpu::Buffer,
    occluder_bytes: Vec<u8>,
    frame_bind: wgpu::BindGroup,
    /// Sky irradiance map bound at `@group(0) @binding(1)` (placeholder when off).
    sky_irradiance: wgpu::TextureView,
    /// Split-sum BRDF table at `@group(0) @binding(3)` (placeholder when off).
    sky_brdf: wgpu::TextureView,
    sky_irradiance_dummy: wgpu::TextureView,
    sky_sampler: wgpu::Sampler,
    /// `sky_irradiance` is the engine's map: the shared colour pass shades its sky light
    /// with it (`output_mode.w`).
    sky_irradiance_on: bool,
    /// When set, the sun term is raw illuminance and the fragment writes scene radiance.
    linear_output: bool,
    /// Pre-exposure of the shared HDR target (`SharedFrameInfo::hdr_scale`); the linear
    /// output is scene radiance × this scale.
    hdr_scale: f32,
    /// `set_motion_matrices` supplied prev / unjittered view-projections.
    motion_set: bool,
    prev_view_proj: [[f32; 4]; 4],
    unjittered_view_proj: [[f32; 4]; 4],
    shared_format: Option<wgpu::TextureFormat>,
    /// Every colour format [`Self::ensure_shared`] has built the shared sets for (the frame's
    /// HDR target, a probe capture's readback target): [`Self::warm_seen_formats`].
    shared_formats_seen: Vec<wgpu::TextureFormat>,
    shared_color: Option<wgpu::RenderPipeline>,
    shared_depth: Option<wgpu::RenderPipeline>,
    shared_shadow: Option<wgpu::RenderPipeline>,
    shared_velocity: Option<wgpu::RenderPipeline>,
    /// Decal receiver: layer 0 and the heightfield normal. Depth is tested, not written.
    shared_receiver: Option<wgpu::RenderPipeline>,
    /// The same five with the hole-cut entry points, for nodes a hole partly covers (built
    /// once a mask is set).
    shared_holes: Option<SharedPipelines>,
    /// The shared pipelines' shader module (kept to build `shared_holes` later).
    shared_shader: Option<wgpu::ShaderModule>,
    /// `shared_shader` carries the light loop ([`TerrainGpu::local_lights_compiled`]).
    shared_shader_lights: bool,
    /// Some ground-look row is non-zero ([`TerrainGpu::set_ground_look`]): the shared colour
    /// pipelines and the virtual texture run the full shared source, not `without_ground_look`'s
    /// (ADR-1452).
    shared_ground_look: bool,
    /// The frame's debug view is a cost view ([`cost_view_needs_full_shader`]): the shared colour
    /// pipelines and the virtual texture run the full shared source, not `without_cost_views`'s
    /// (ADR-1491). Set by [`TerrainGpu::upload_shared`].
    shared_cost_view: bool,
    /// The frame has surface weather or precipitation extinction: the shared colour pipelines and
    /// the virtual texture run the `@surface-weather` code ([`without_surface_weather`] strips it
    /// otherwise, spec 99 § 6.4). Set by [`TerrainGpu::upload_shared`].
    shared_surface_weather: bool,
    /// The frame binds an engine cloud map ([`Self::set_engine_cloud_shadow`]): the shared colour
    /// pipelines run the cloud reads; without one they run [`without_cloud_map`]'s source
    /// (ADR-7886).
    shared_cloud_map: bool,
    /// Frames shaded with the surface-weather variant.
    surface_weather_frames: u64,
    /// [`TerrainGpu::warm_shared`] builds the surface-weather variant too.
    warm_surface_weather: bool,
    /// Offscreen colour pipeline with the hole cut (`fs_main` with `hole_cut`), built with a mask.
    terrain_holes_pipeline: Option<wgpu::RenderPipeline>,
    /// The offscreen pipelines' shader module (kept to build `terrain_holes_pipeline`).
    terrain_shader: wgpu::ShaderModule,
    terrain_pipeline_layout: wgpu::PipelineLayout,
    /// The offscreen shader's source (the key of its pipelines in `shared_pipelines`).
    terrain_source: String,
    /// The shared pass shader's source (the key of its pipelines in `shared_pipelines`).
    shared_source: Option<String>,
    /// Pipelines shared by every terrain on the device
    /// ([`regia_render::RenderDevice::shared_pipelines`]).
    shared_pipelines: std::sync::Arc<regia_render::device_pipelines::DevicePipelines>,
    /// [`HolesMask::gpu_words`] of the mask (one zero word without one), bound at group 2;
    /// with a lattice cut, [`HolesMask::gpu_words_with_lattice`] (ADR-8470).
    holes_buffer: wgpu::Buffer,
    /// The lattice cut of the mesh-layer mouths ([`Self::set_hole_lattice`], ADR-8470).
    hole_lattice: Option<std::sync::Arc<crate::holes::LatticeHoleCut>>,
    /// Coverage-only hole words while a lattice cut is set (the GI trace reads coverage
    /// windows only); `None` when `holes_buffer` has no lattice window.
    gi_holes_buffer: Option<wgpu::Buffer>,
    /// Bumped by every [`Self::set_holes`].
    holes_generation: u64,
    /// The open mouths' bands, drawn in every shared pass after the hole words
    /// ([`Self::set_mouth_bands`], ADR-8569).
    mouth_bands: Option<std::sync::Arc<crate::mesh_layer::MouthBands>>,
    /// Byte offset of band pass 0's vertex positions in `holes_buffer` (0 without bands).
    band_positions_at: u64,
    /// Which [`BAND_CAMERAS`] slot the active camera's band sets are (0 the main camera, 1 a
    /// texture view; swapped with the view state, ADR-8824).
    band_camera: u32,
    cascade_layout: wgpu::BindGroupLayout,
    cascade_bufs: Vec<wgpu::Buffer>,
    cascade_binds: Vec<wgpu::BindGroup>,

    terrain_layout: wgpu::BindGroupLayout,
    /// The filtering sampler of the terrain budget's height reads, when the device can
    /// filter 32-bit floats (bindings 18/19 exist only then).
    height_sampler: Option<wgpu::Sampler>,
    /// The terrain uniform as last written (the budget rewrites `mip_info.yzw`).
    terrain_uniform: TerrainUniform,
    height_view: wgpu::TextureView,
    terrain_buffer: wgpu::Buffer,
    layers_buffer: wgpu::Buffer,
    material_sampler: wgpu::Sampler,
    /// `material_sampler` with anisotropic filtering (binding 22).
    material_aniso_sampler: wgpu::Sampler,
    material_buffer: wgpu::Buffer,
    material_state: MaterialUniform,
    material_textures: MaterialTextures,
    /// Scatter contact map + window uniform (neutral until `set_contact`).
    contact: ContactBinding,
    /// `set_contact` calls (part of [`TerrainGpu::contact_key`]).
    contact_binds: u64,
    /// Texture bytes of the height mirror + all material textures (budget accounting).
    texture_bytes: u64,
    /// Bytes of the height texture including its pyramid levels.
    height_texture_bytes: u64,
    /// CPU copy of the pyramid levels above 0, refreshed by [`Self::update_heights`]
    /// (snapshot reads: no lock is held while a reader works, ADR-303).
    height_mips: SharedHeightMips,
    /// The slope pyramid of the heights (bindings 20/21), refreshed with them.
    slope: SlopeBinding,
    /// Sim mirror bytes (0 until `set_sim_map`).
    sim_bytes: u64,
    /// The layer stack (its texture-present flags are refreshed by `set_material`).
    layers_desc: TerrainLayers,
    /// Per slot `(has_textures, has_orm)` of the installed material.
    layer_flags: LayerFlags,
    /// Unit-square size of one layer texel once a material is installed.
    layer_texel: Option<f32>,
    /// Top levels of the cooked layer arrays left out of the upload ([`Self::set_layer_mip_skip`]).
    layer_mip_skip: u32,
    /// Texel size of the splat packs in map UV (`1 / width`), 0 without them ([`Self::set_splat_weights`]).
    splat_texel: f32,
    /// Lowest and highest surface altitude of the map, metres. Height edits refresh it
    /// ([`Self::update_heights`]), so the layer uniform and the virtual texture's page distances
    /// see the edited map's range, the one a cold load of it computes.
    altitude_range: std::sync::Mutex<[f32; 2]>,
    /// Ground-bounce ambient of the hemisphere model (`None` = legacy ambient).
    ground_ambient: Option<[f32; 3]>,
    /// Planet-shine `[toward, colour · lux]` (`SharedFrameInfo::fill_light`).
    fill_light: Option<[[f32; 3]; 2]>,
    /// The frame's extra directional lights (`SharedFrameInfo::extra_directional`, ADR-7254).
    extra_lights: [[f32; 4]; 2 * regia_render::MAX_EXTRA_DIRECTIONAL],
    /// Where the fill's visibility is the canyon-scale macro AO and the SSAO only
    /// ([`Self::set_fill_canyon_scope`], ADR-8412).
    fill_canyon_scope: crate::FillCanyonScope,
    /// Layer/macro texture mip bias (`SharedFrameInfo::terrain_texture_lod_bias`).
    texture_lod_bias: f32,
    /// Anisotropic filtering level, 0 = off (`TerrainBudget::anisotropy`).
    anisotropy: u8,
    /// The shader takes the anisotropic layer taps itself (ADR-3957, [`Self::set_anisotropy_footprint`]).
    anisotropy_footprint: bool,
    /// The sampler path's LOD offset in mips (ADR-5459, [`Self::set_anisotropy_lod_offset`]).
    anisotropy_lod_offset: f32,
    /// Write stochastic-SSR surface codes into the shared HDR alpha (`set_sssr_codes`).
    sssr_codes: bool,

    terrain_bind: wgpu::BindGroup,
    scorch_buffer: wgpu::Buffer,

    node_layout: wgpu::BindGroupLayout,
    node_buffer: wgpu::Buffer,
    node_capacity: u32,
    node_stride: u32,
    node_bind: wgpu::BindGroup,
    /// Per-pass draw lists (node slots), concatenated; see [`DrawList`].
    slot_buffer: wgpu::Buffer,
    slot_capacity: u32,
    /// CPU mirror of `slot_buffer`; only changed words are uploaded.
    slot_cpu: Vec<u32>,
    /// Scratch for [`Self::stage_lists`]. Reused so a held selection does not allocate.
    slot_scratch: Vec<u32>,
    /// Hole and dense slots gathered while a draw list is staged.
    list_holes: Vec<u32>,
    list_dense: Vec<u32>,
    /// Identity slot list for the offscreen upload. Reused across frames.
    identity_slots: Vec<u32>,
    /// Where each list of the last upload sits in `slot_buffer`.
    draw_lists: Vec<DrawList>,

    grid_index_buffer: wgpu::Buffer,
    grid_index_count: u32,
    /// Decimated grid (`SHADOW_GRID_SIZE` cells per node) for the shared shadow passes.
    shadow_index_buffer: wgpu::Buffer,
    shadow_index_count: u32,
    /// Grid twice as fine (`2 · grid_size` cells) for near nodes on steep ground
    /// ([`TerrainRelief::cliff_relief`]), and its decimated shadow grid.
    dense_index_buffer: wgpu::Buffer,
    dense_index_count: u32,
    dense_shadow_index_buffer: wgpu::Buffer,
    dense_shadow_index_count: u32,
    /// Per node of the last staged selection: drawn with the fine grid.
    dense_cpu: Vec<bool>,
    /// Whether a node's ground is steep enough for cliff relief, per node (cleared when
    /// the heights change).
    steep_nodes: std::sync::Mutex<HashMap<QuadNode, bool>>,
    /// Camera XZ the fine-grid nodes are chosen around ([`Self::set_detail_eye`]).
    detail_eye: Option<[f32; 2]>,
    /// [`TerrainRelief::cliff_relief`] of the last `set_relief`.
    relief_cliff: f32,
    hole_index_buffers: HashMap<QuadNode, wgpu::Buffer>,
    /// Per node: how the hole mask covers it ([`crate::holes::node_hole_state`]), kept until the mask
    /// changes ([`Self::set_holes`]). Staging reads it instead of testing every shape again
    /// for every node of every pass list each frame.
    hole_states: HashMap<QuadNode, NodeHoleState>,
    /// Per node of the selection being staged: its hole state (empty without a mask).
    slot_holes: Vec<NodeHoleState>,
    /// Nodes whose hole state the last staging had to compute (not yet in
    /// [`Self::hole_states`]).
    hole_tests_last: u32,
    /// Partially covered nodes whose every triangle lies inside the mask (inside the union
    /// of several shapes, not inside one: [`crate::holes::node_hole_state`] calls them partial). They
    /// draw nothing, like a fully covered node, and are not rebuilt every frame; kept until
    /// the mask changes.
    hole_cut_nodes: std::collections::HashSet<QuadNode>,
    /// Hole-culled index lists the last node upload built ([`crate::holes::build_node_indices`]).
    hole_index_builds_last: u32,

    scatter_params_buffer: wgpu::Buffer,
    scatter_bind: wgpu::BindGroup,
    instance_buffer: Option<wgpu::Buffer>,
    instance_count: u32,

    water_bind: Option<wgpu::BindGroup>,
    /// Layout of `water_bind` (group 1 of the water pipeline), kept for `set_water`.
    water_layout: wgpu::BindGroupLayout,
    /// The plane `water_bind` draws and its uniform buffer.
    water_plane: Option<(WaterPlane, wgpu::Buffer)>,
    /// 1×1 refraction sources (scene color, scene depth) bound while no scene copy is.
    water_scene_stand_in: (wgpu::TextureView, wgpu::TextureView),
    /// `render_offscreen` binds its scene copy for screen-space refraction.
    water_refraction: bool,

    terrain_pipeline: wgpu::RenderPipeline,
    scatter_pipeline: wgpu::RenderPipeline,
    shadow_pipeline: wgpu::RenderPipeline,
    water_pipeline: wgpu::RenderPipeline,
    sky_pipeline: wgpu::RenderPipeline,
    shadow_view: wgpu::TextureView,
    shadow_bind: wgpu::BindGroup,
    shadow_sampler: wgpu::Sampler,
    engine_csm_layout: wgpu::BindGroupLayout,
    engine_csm_buffer: wgpu::Buffer,
    engine_csm_bind: wgpu::BindGroup,
    /// Views the engine CSM group was built from. Unchanged views keep the group.
    engine_csm_views: Vec<wgpu::TextureView>,
    /// The engine SSAO view the colour pass reads, or the white-free placeholder.
    engine_ao: wgpu::TextureView,
    /// `engine_ao` is the frame's SSAO (not the placeholder).
    engine_ao_on: bool,
    /// The engine's irradiance probes (uniform, data words) the colour pass reads, or the
    /// zeroed stand-ins (probes off).
    engine_gi: (wgpu::Buffer, wgpu::Buffer),
    engine_gi_dummy: (wgpu::Buffer, wgpu::Buffer),
    /// The engine's clustered point and spot lights the colour pass reads, or the zeroed
    /// stand-ins (no lights yet).
    engine_lights: regia_render::EngineLights,
    engine_lights_dummy: regia_render::EngineLights,
    /// The frame's emitter probe atlas (the lights' IES profile bands, ADR-3247), or the 1×1 stand-in under which
    /// a profiled light shades unshaped.
    engine_light_profiles: wgpu::TextureView,
    /// The engine-CSM group carries the light bindings and the shared shader lights the
    /// terrain with them (`local_lights_fit`).
    local_lights: bool,
    /// The host wants the local lights ([`Self::set_local_lights_enabled`]).
    local_lights_enabled: bool,
    /// 4² depth placeholder a new camera's engine-CSM group binds until the engine's cascades
    /// arrive ([`ViewGpu`]).
    engine_csm_dummy: wgpu::TextureView,
    /// 1×1 placeholder of the engine SSAO and contact-shadow slots.
    engine_ao_dummy: wgpu::TextureView,
    /// [`Self::holes_generation`] the current `node_bind` was built with: a camera's node group
    /// built before a hole change is rebuilt when that camera is swapped back in.
    node_bind_holes: u64,
    /// The engine contact-shadow view the colour pass reads, or the placeholder.
    engine_contact: wgpu::TextureView,
    /// `engine_contact` is the frame's contact shadows (not the placeholder).
    engine_contact_on: bool,
    /// The engine's cloud transmittance map the colour pass reads, or the placeholder (ADR-320).
    engine_cloud: wgpu::TextureView,
    /// Its parameters (`[extent, base, top, 1]`; zero: not read).
    engine_cloud_params: [f32; 4],
    /// The shadow atlas with the precipitation occlusion tile, when the frame has one.
    engine_precip: Option<wgpu::TextureView>,
    /// CPU mirror of `node_buffer` (`node_capacity * node_stride` bytes). Only
    /// slots whose bytes differ are uploaded.
    node_cpu: Vec<u8>,
    /// Writes and bytes of the last node upload.
    node_upload_last: (u32, u64),
    /// Last bytes written to the frame, engine CSM and cascade uniforms on the shared path.
    frame_bytes: Vec<u8>,
    engine_csm_bytes: Vec<u8>,
    cascade_bytes: Vec<Vec<u8>>,
    /// Sun cascade view-projection of the last `update_frame`, and the
    /// view-projection plus scatter generation the cascade was last rendered with.
    sun_cache: std::sync::Mutex<SunShadowCache>,
    /// Bumped whenever the scatter casters change.
    scatter_generation: u64,
    /// Sun cascade renders since construction.
    sun_renders: std::sync::atomic::AtomicU64,

    config: CdlodConfig,
    root_level: u32,
    mask: Option<HolesMask>,
    height_scale: f32,
    height_offset: f32,
    /// Terrain virtual texture settings (`set_virtual_texture`); built lazily with the shared
    /// pipelines into `vt`.
    vt_settings: Option<vt::VirtualTextureSettings>,
    /// The host (or the agent) chose the virtual texture through [`Self::set_virtual_texture`]:
    /// the profile's `TerrainBudget::virtual_texture` no longer drives it (ADR-157 addendum v0.1.2).
    vt_explicit: bool,
    /// What the profile's `TerrainBudget` last asked for: `virtual_texture` and, with it,
    /// `vt_late_depth` (ADR-2122).
    vt_profile: (bool, bool),
    vt: Option<vt::VtGpu>,
    vt_handle: vt::TerrainVtHandle,
    /// The shared colour pipeline reading the virtual texture (`fs_shared_vt`).
    shared_color_vt: Option<wgpu::RenderPipeline>,
    /// Bumped whenever something the cached surface reads changes besides the uniforms the
    /// virtual texture hashes itself (heights, material and layer textures).
    surface_generation: std::sync::atomic::AtomicU64,
    /// World rectangles (`[min_x, min_z, max_x, max_z]`, already grown by
    /// [`vt::HEIGHT_REACH_M`]) of height edits the virtual texture has not yet seen; drained
    /// by [`Self::prepare_vt`]. Merged into one only past [`VT_EDIT_RECTS_MAX`] (ADR-7585).
    vt_height_dirty: std::sync::Mutex<Vec<[f32; 4]>>,
    /// What the height edits invalidated in the virtual texture so far (ADR-7585).
    vt_edit_counters: std::sync::Mutex<VtEditCounters>,
    /// Sun direction and the hemisphere-ambient flag of the last shared frame (the surface
    /// reads both).
    vt_frame_terms: [f32; 4],
    /// The sun direction the virtual texture's content key holds: it follows the frame's
    /// sun only once that has turned more than [`VT_SUN_TOLERANCE_RAD`] away (hysteresis), so
    /// a host that re-sends a drifting or jittering sun every frame does not flush the cache.
    vt_key_sun: Option<[f32; 3]>,
}

/// How far the sun may turn from the one the cached pages were keyed with before the virtual
/// texture flushes (0.1°). The pages bake two sun-dependent terms, the heightfield sun
/// occlusion and the sunlit-wall bounce; 0.1° moves a relief shadow cast from 300 m by
/// about half a metre, under the texel size at the distances the pages serve.
pub(crate) const VT_SUN_TOLERANCE_RAD: f32 = 0.1 * std::f32::consts::PI / 180.0;

/// The sun direction the virtual texture's key holds after a frame whose sun is `sun`
/// (hysteresis around `held`: kept while within [`VT_SUN_TOLERANCE_RAD`] of it).
pub(crate) fn vt_key_sun(held: Option<[f32; 3]>, sun: [f32; 3]) -> [f32; 3] {
    let len = |v: [f32; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    match held {
        Some(h) => {
            let (lh, ls) = (len(h), len(sun));
            if lh <= 0.0 || ls <= 0.0 {
                return if h == sun { h } else { sun };
            }
            let cos = (h[0] * sun[0] + h[1] * sun[1] + h[2] * sun[2]) / (lh * ls);
            if cos.is_finite() && cos >= VT_SUN_TOLERANCE_RAD.cos() {
                h
            } else {
                sun
            }
        }
        None => sun,
    }
}

impl TerrainGpu {
    /// Uploads the terrain (height texture, layers, holes, water) and builds the pipelines.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when the heightfield exceeds the device's texture limits or
    /// the layers/configuration are invalid.
    pub fn new(
        device: &RenderDevice,
        map: &TerrainMap,
        layers: &TerrainLayers,
        config: &CdlodConfig,
        root_level: u32,
        mask: Option<&HolesMask>,
        water: Option<&WaterPlane>,
    ) -> Result<Self, TerrainError> {
        Self::new_inner(device, map, layers, config, root_level, mask, water, true)
    }

    /// [`Self::new`] delegates here; `publish_aniso_report` is false for the pipeline-warm
    /// job's throwaway terrain ([`super::TerrainSharedDraw::new_for_warm`]), whose draw exists
    /// only to compile pipelines and so does not publish its anisotropy path or LOD offset to
    /// the process-global report ([`crate::anisotropy_path`],
    /// [`crate::anisotropy_lod_offset`]). A publish there could overwrite a host terrain's
    /// report (and a host override) depending on whether the warm worker ran before or after
    /// the terrain was built.
    #[allow(clippy::too_many_arguments)] // the public `new` splits off; the flag rides along
    fn new_inner(
        device: &RenderDevice,
        map: &TerrainMap,
        layers: &TerrainLayers,
        config: &CdlodConfig,
        root_level: u32,
        mask: Option<&HolesMask>,
        water: Option<&WaterPlane>,
        publish_aniso_report: bool,
    ) -> Result<Self, TerrainError> {
        let tracked_device = regia_render::TrackedDevice::observe(&device.device);
        let d = &tracked_device;
        layers.validate()?;
        config.validate()?;
        if let Some(water) = water {
            water.validate()?;
        }
        let max_dim = d.limits().max_texture_dimension_2d;
        if map.width() > max_dim || map.height() > max_dim {
            tracing::error!(
                target: "regia_terrain",
                "heightfield {}x{} exceeds max texture dimension {max_dim}",
                map.width(),
                map.height()
            );
            return Err(TerrainError::Gpu(regia_render::RenderError::Readback(format!(
                "heightfield {}x{} exceeds max texture dimension {max_dim}",
                map.width(),
                map.height()
            ))));
        }

        // ---- Height texture (R32Float mirror of the map + the box-filtered pyramid) ----
        // Coarse CDLOD vertices and distant shading normals read the pyramid level that
        // matches their cell / pixel footprint (`height_mips`), so sharp walls do not alias
        // into a saw-tooth at every distance.
        let height_mips = HeightMips::build(map);
        let height_texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain heights"),
            size: wgpu::Extent3d {
                width: map.width(),
                height: map.height(),
                depth_or_array_layers: 1,
            },
            mip_level_count: height_mips.level_count(),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_full_heights(&device.queue, &height_texture, map);
        let mut height_texture_bytes = u64::from(map.width()) * u64::from(map.height()) * 4;
        for k in 1..height_mips.level_count() {
            if let Some(level) = height_mips.level(k) {
                let rect = (0, level.width, 0, level.height);
                write_height_mip(
                    &device.queue,
                    &height_texture,
                    k,
                    level,
                    rect,
                    map.height_offset(),
                    map.height_scale(),
                );
                height_texture_bytes += u64::from(level.width) * u64::from(level.height) * 4;
            }
        }
        let height_view = height_texture.create_view(&wgpu::TextureViewDescriptor::default());
        // ---- Slope pyramid (`slope_mips`): the per-pixel footprint normal in two filtered
        // fetches instead of 32 height loads (lane terrain-shader-perf) ----
        let slope = SlopeBinding::new(d, &device.queue, map, &height_mips);
        height_texture_bytes += slope.bytes;

        // ---- Bind group layouts ----
        let uniform_entry = |binding, visibility, dynamic| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: dynamic,
                min_binding_size: None,
            },
            count: None,
        };
        let texture_entry = |binding, visibility| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Texture {
                // Heights are filtered manually with `textureLoad` (exact CPU-sampler math),
                // so the texture needs no filterable support (portable: lavapipe, mobile).
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let filterable_texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                // Material maps are ordinary 8-bit / BCn textures (always filterable).
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        };
        // Frame and terrain groups are visible to compute too: the virtual texture's bake
        // (`cs_vt_bake`) runs the surface on them.
        let frame_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain frame layout"),
            entries: &with_compute(vec![
                uniform_entry(
                    0,
                    wgpu::ShaderStages::VERTEX.union(wgpu::ShaderStages::FRAGMENT),
                    false,
                ),
                // The engine's sky irradiance map (shared frames with IBL; a 1×1 placeholder
                // otherwise) and its filtering sampler: `set_sky_irradiance`.
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // The engine's split-sum BRDF table for the sky reflection.
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // Large meshes' occlusion proxies (`MeshOccluderBlock`; count 0 off the shared
                // pass): `upload_shared`.
                uniform_entry(4, wgpu::ShaderStages::FRAGMENT, false),
            ]),
        });
        let shadow_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain sun shadow layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
            ],
        });
        // Terrain budget: a filtered view of the height pyramid for the per-pixel normal, when
        // the device can filter 32-bit floats (`regia_render::TerrainBudget::hw_filtered_normals`).
        let hw_filter = d.features().contains(wgpu::Features::FLOAT32_FILTERABLE);
        let mut terrain_entries = vec![
            texture_entry(0, wgpu::ShaderStages::VERTEX.union(wgpu::ShaderStages::FRAGMENT)),
            uniform_entry(
                2,
                wgpu::ShaderStages::VERTEX.union(wgpu::ShaderStages::FRAGMENT),
                false,
            ),
            // The vertex stage reads the layer bands and albedo means (cliff relief's icy test).
            uniform_entry(3, wgpu::ShaderStages::VERTEX_FRAGMENT, false),
            uniform_entry(4, wgpu::ShaderStages::FRAGMENT, false),
            // Photoreal material path (neutral placeholders until `set_material`).
            filterable_texture_entry(5),
            filterable_texture_entry(6),
            filterable_texture_entry(7),
            filterable_texture_entry(8),
            wgpu::BindGroupLayoutEntry {
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ..filterable_texture_entry(9)
            },
            filterable_texture_entry(10),
            filterable_texture_entry(11),
            uniform_entry(12, wgpu::ShaderStages::VERTEX_FRAGMENT, false),
            wgpu::BindGroupLayoutEntry {
                binding: 13,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            // Sim ground heightfield (manual bilinear like the visual heights).
            texture_entry(14, wgpu::ShaderStages::FRAGMENT),
            // Per-layer glTF ORM (R occlusion, G roughness, B metallic). Linear.
            filterable_texture_entry(15),
            // Scatter contact map (R8 occlusion under instances) and its window uniform.
            wgpu::BindGroupLayoutEntry {
                binding: 16,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            uniform_entry(17, wgpu::ShaderStages::FRAGMENT, false),
            // Heightfield slope pyramid (`Rg16Float`, filterable) and its clamped bilinear sampler.
            wgpu::BindGroupLayoutEntry {
                binding: 20,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 21,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            // The material sampler with anisotropic filtering (ADR-192).
            wgpu::BindGroupLayoutEntry {
                binding: 22,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            // Splat weight packs over the map (`set_splat_weights`, ADR-1402); 1×1 zeros when off.
            filterable_texture_entry(23),
        ];
        if hw_filter {
            terrain_entries.push(wgpu::BindGroupLayoutEntry {
                binding: 18,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
            terrain_entries.push(wgpu::BindGroupLayoutEntry {
                binding: 19,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            });
        }
        let terrain_entries = with_compute(terrain_entries);
        let terrain_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain data layout"),
            entries: &terrain_entries,
        });
        let height_sampler = hw_filter.then(|| {
            d.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("terrain height filter"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                ..Default::default()
            })
        });
        let node_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain node layout"),
            entries: &[
                node_storage_entry(0),
                node_storage_entry(1),
                // Hole windows and coverage (`in_hole`), read by the hole-cut fragment stages, and
                // the lattice windows' near-ring bit by the vertex stage (relief off there, ADR-8470).
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let scatter_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain scatter layout"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX, false)],
        });
        let water_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain water layout"),
            entries: &[
                texture_entry(0, wgpu::ShaderStages::FRAGMENT),
                uniform_entry(2, wgpu::ShaderStages::FRAGMENT, false),
                uniform_entry(
                    3,
                    wgpu::ShaderStages::VERTEX.union(wgpu::ShaderStages::FRAGMENT),
                    false,
                ),
                // Refraction sources: the scene copy (read with `textureLoad`) and the
                // scene depth (read-only attachment in the same pass).
                texture_entry(4, wgpu::ShaderStages::FRAGMENT),
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        // ---- Uniform buffers + bind groups ----
        let make_uniform_buffer = |label: &str, size: u64| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let frame_buffer = make_uniform_buffer("terrain frame", size_of::<FrameUniform>() as u64);
        let occluder_buffer = make_uniform_buffer(
            "terrain mesh occluders",
            size_of::<regia_render::MeshOccluderBlock>() as u64,
        );
        let sky_irradiance_dummy = d
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("terrain sky irradiance placeholder"),
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
            .create_view(&wgpu::TextureViewDescriptor::default());
        // Equirectangular: wraps in u (azimuth), clamps in v (poles), like the mesh path's
        // IBL sampler.
        let sky_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain sky irradiance sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let frame_bind = frame_bind_group(
            d,
            &frame_layout,
            &frame_buffer,
            &sky_irradiance_dummy,
            &sky_sampler,
            &sky_irradiance_dummy,
            &occluder_buffer,
        );

        let shadow_texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain sun shadow"),
            size: wgpu::Extent3d {
                width: SUN_SHADOW_MAP,
                height: SUN_SHADOW_MAP,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let shadow_view = shadow_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("terrain sun shadow view"),
            format: Some(wgpu::TextureFormat::Depth32Float),
            dimension: Some(wgpu::TextureViewDimension::D2),
            aspect: wgpu::TextureAspect::DepthOnly,
            ..Default::default()
        });
        let shadow_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain sun shadow compare"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let shadow_bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("terrain sun shadow bind"),
            layout: &shadow_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&shadow_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&shadow_sampler),
                },
            ],
        });
        let depth_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Depth,
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        // The engine's clustered point and spot lights (bindings 10–16) when the device's
        // fragment stage has room for their four storage buffers and one uniform
        // (`local_lights_fit`); otherwise the terrain takes no local light (logged once).
        let local_lights = local_lights_fit(&d.limits(), d.features());
        if !local_lights {
            tracing::info!(
                target: "regia_terrain",
                "terrain local lights off: the device binds {} storage and {} uniform buffers per stage",
                d.limits().max_storage_buffers_per_shader_stage,
                d.limits().max_uniform_buffers_per_shader_stage
            );
        }
        let engine_csm_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain engine csm layout"),
            entries: &[
                depth_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
                depth_entry(2),
                depth_entry(3),
                depth_entry(4),
                uniform_entry(5, wgpu::ShaderStages::FRAGMENT, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // The engine contact shadows (`set_engine_contact_shadows`).
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // The engine's irradiance probes (`regia_render::ddgi_shading_wgsl`).
                uniform_entry(8, wgpu::ShaderStages::FRAGMENT, false),
                wgpu::BindGroupLayoutEntry {
                    binding: 9,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // The engine's cloud transmittance map (`set_engine_cloud_shadow`, ADR-320).
                wgpu::BindGroupLayoutEntry {
                    binding: ENGINE_CLOUD_BINDING,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // The shadow atlas with the occlusion tile (`set_engine_precip_occlusion`).
                wgpu::BindGroupLayoutEntry {
                    binding: ENGINE_PRECIP_BINDING,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ]
            .into_iter()
            .chain(
                local_lights
                    .then(|| regia_render::shared_lights_layout_entries(ENGINE_LIGHTS_BINDING))
                    .into_iter()
                    .flatten(),
            )
            .chain(
                local_lights
                    .then(|| regia_render::shared_light_profiles_layout_entry(ENGINE_LIGHT_PROFILES_BINDING)),
            )
            .collect::<Vec<_>>(),
        });
        // Zeroed probe stand-ins (flags 0: the shader takes no probe light).
        let engine_gi_dummy = (
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("terrain engine gi uniform (off)"),
                size: regia_render::DDGI_UNIFORM_BYTES,
                usage: wgpu::BufferUsages::UNIFORM,
                mapped_at_creation: false,
            }),
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("terrain engine gi data (off)"),
                size: 64,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            }),
        );
        let engine_csm_buffer =
            make_uniform_buffer("terrain engine csm", size_of::<EngineCsmUniform>() as u64);
        // The bind group retains this dummy until `set_engine_csm` replaces it
        // with the engine cascades. Dropping the `Texture` here is safe.
        let engine_csm_texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain csm dummy"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let engine_csm_dummy = engine_csm_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("terrain csm dummy view"),
            format: Some(wgpu::TextureFormat::Depth32Float),
            dimension: Some(wgpu::TextureViewDimension::D2),
            aspect: wgpu::TextureAspect::DepthOnly,
            ..Default::default()
        });
        // Zeroed light stand-ins (light count 0: the shader adds no local light).
        let engine_lights_dummy = regia_render::EngineLights::stand_in(d, engine_csm_dummy.clone());
        // Bound until `set_engine_ao` supplies the engine SSAO; the shader reads it only
        // when the uniform says SSAO is on (`screen.z`).
        let engine_ao_dummy = d
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("terrain ao dummy"),
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
            .create_view(&wgpu::TextureViewDescriptor::default());
        let engine_csm_bind = engine_csm_bind_group(
            d,
            &engine_csm_layout,
            [
                &engine_csm_dummy,
                &engine_csm_dummy,
                &engine_csm_dummy,
                &engine_csm_dummy,
            ],
            &shadow_sampler,
            &engine_csm_buffer,
            &engine_ao_dummy,
            &engine_ao_dummy,
            (&engine_gi_dummy.0, &engine_gi_dummy.1),
            local_lights.then_some((&engine_lights_dummy, &engine_ao_dummy)),
            &engine_ao_dummy,
            &engine_csm_dummy,
        );

        let terrain_uniform = TerrainUniform {
            // The grid origin folds into the map-extent lane (ADR-8179).
            half_width: map.gpu_half_width(),
            world_width: map.world_width(),
            height_scale: map.height_scale(),
            height_offset: map.height_offset(),
            sample_max: [(map.width() - 1) as f32, (map.height() - 1) as f32],
            grid_size: config.grid_size as f32,
            root_level: root_level as f32,
            mip_info: [height_mips.level_count() as f32, 0.0, 0.0, 0.0],
            // Square maps upload the X lane again, so the shader's square branch stays the
            // historical expression (ADR-8369).
            depth: [map.gpu_half_depth(), map.world_depth(), 0.0, 0.0],
        };
        let terrain_buffer = make_uniform_buffer("terrain params", size_of::<TerrainUniform>() as u64);
        device
            .queue
            .write_buffer(&terrain_buffer, 0, bytemuck::bytes_of(&terrain_uniform));

        let layers_uniform = layers_uniform(layers, &NO_LAYER_TEXTURES, None, map_altitude_range(map));
        let layers_buffer = make_uniform_buffer("terrain layers", size_of::<LayersUniform>() as u64);
        device
            .queue
            .write_buffer(&layers_buffer, 0, bytemuck::bytes_of(&layers_uniform));

        let scorch_buffer = make_uniform_buffer("terrain scorch", size_of::<ScorchUniform>() as u64);
        let empty_scorch = ScorchUniform {
            rows: [[0.0; 4]; MAX_SCORCH_MARKS + 1],
        };
        device
            .queue
            .write_buffer(&scorch_buffer, 0, bytemuck::bytes_of(&empty_scorch));

        let material_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain material sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        // The same sampler with anisotropic filtering at the hardware's 16× clamp; the shader
        // limits each tap's ratio to the frame's level (`aniso_grads`), so the level changes
        // without a new sampler. wgpu leaves it isotropic where the adapter lacks
        // `DownlevelFlags::ANISOTROPIC_FILTERING`.
        let material_aniso_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain material sampler (anisotropic)"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: u16::from(regia_render::TerrainBudget::MAX_ANISOTROPY),
            ..Default::default()
        });
        let material_textures = MaterialTextures::neutral(d, &device.queue);
        let material_buffer = make_uniform_buffer("terrain material", size_of::<MaterialUniform>() as u64);
        let material_uniform = MaterialUniform {
            params0: [0.0, 0.0, 0.0, 0.0],
            params1: [60.0, 220.0, 0.55, 0.25],
            sim: [0.0; 4],
            tiles: [1.0, 1.0, 0.0, 0.0],
            road0: [0.0; 4],
            road1: [0.0; 4],
            strata: TerrainRelief::default().uniforms().0,
            relief: TerrainRelief::default().uniforms().1,
            strata2: TerrainRelief::default().uniforms().2,
            rock: TerrainRelief::default().uniforms().3,
            cliff: TerrainRelief::default().cliff_uniform(),
            ground: [0.0; 4],
            winter: [0.0; 4],
            winter2: [0.0; 4],
            macro_mask: [0; crate::material::MACRO_RESIDENCY_WORDS],
            macro_prefix: [0; crate::material::MACRO_RESIDENCY_WORDS],
            weather: [0.0; 4],
            weather2: [0.0; 4],
            weather_tile: [[0.0; 4]; 4],
            weather_rect: [0.0; 4],
            weather_bias: [0.0; 4],
        };
        device
            .queue
            .write_buffer(&material_buffer, 0, bytemuck::bytes_of(&material_uniform));

        let contact = ContactBinding::neutral(d);
        let terrain_bind = material_textures.bind_group(
            d,
            &terrain_layout,
            &height_view,
            &terrain_buffer,
            &layers_buffer,
            &scorch_buffer,
            &material_buffer,
            [&material_sampler, &material_aniso_sampler],
            &contact,
            height_sampler.as_ref(),
            &slope,
        );

        // Nodes and per-pass draw lists live in two storage buffers; each pass draws its
        // list as one instanced draw (`node_slots[instance_index]` names the node).
        let node_stride = size_of::<NodeUniform>() as u32;
        let node_capacity = 64u32;
        let node_buffer = make_node_storage(
            d,
            "terrain nodes",
            u64::from(node_stride) * u64::from(node_capacity),
        );
        let slot_capacity = 256u32;
        let slot_buffer = make_node_storage(d, "terrain node slots", u64::from(slot_capacity) * 4);
        let holes_buffer = holes_storage(d, mask);
        let node_bind = node_bind_group(d, &node_layout, &node_buffer, &slot_buffer, &holes_buffer);

        // ---- Shared grid index buffers ----
        let g = config.grid_size;
        let (grid_index_buffer, grid_index_count) = grid_indices(d, "terrain grid indices", g, 1);
        // Shadow casters skip vertices down to `SHADOW_GRID_SIZE` cells per node: a cascade
        // texel is far coarser than a runtime cell, and every shadow vertex is drawn into up
        // to four cascades.
        let (shadow_index_buffer, shadow_index_count) =
            grid_indices(d, "terrain shadow grid indices", g, (g / SHADOW_GRID_SIZE).max(1));
        // Fine grid for near nodes on steep ground (cliff relief), and its shadow grid.
        let (dense_index_buffer, dense_index_count) = grid_indices(d, "terrain dense grid indices", g * 2, 1);
        let (dense_shadow_index_buffer, dense_shadow_index_count) = grid_indices(
            d,
            "terrain dense shadow grid indices",
            g * 2,
            (g * 2 / SHADOW_GRID_SIZE).max(1),
        );

        // ---- Scatter ----
        let scatter_params_buffer = make_uniform_buffer("terrain scatter params", 16);
        let scatter_bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("terrain scatter bind"),
            layout: &scatter_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: scatter_params_buffer.as_entire_binding(),
            }],
        });

        // ---- Water ----
        let water_scene_stand_in = water_scene_stand_in(d);
        let water_plane = water.map(|water| {
            let buffer = make_uniform_buffer("terrain water params", size_of::<WaterUniform>() as u64);
            device.queue.write_buffer(
                &buffer,
                0,
                bytemuck::bytes_of(&water_uniform(map.world_width(), map.grid_origin(), water, false)),
            );
            (water.clone(), buffer)
        });
        let water_bind = water_plane.as_ref().map(|(_, buffer)| {
            water_bind_group(
                d,
                &water_layout,
                &height_view,
                &terrain_buffer,
                buffer,
                (&water_scene_stand_in.0, &water_scene_stand_in.1),
            )
        });

        // ---- Pipelines ----
        let colour_format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let depth_format = wgpu::TextureFormat::Depth32Float;
        let depth_stencil = wgpu::DepthStencilState {
            format: depth_format,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: Default::default(),
            bias: Default::default(),
        };
        // The offscreen (lab) pipelines are the same for every terrain on a device: built once
        // per device (`RenderDevice::shared_pipelines`), by the first terrain or a warm job
        // (`terrain_warm_job`), so a terrain made later compiles nothing.
        let terrain_source = device_source(d, without_surface_weather(include_str!("terrain.wgsl")));
        let shared = std::sync::Arc::clone(device.shared_pipelines());
        let offscreen = shared.get_or_create(
            regia_render::device_pipelines::pipeline_key(&("regia_terrain offscreen", &terrain_source)),
            || {
                // Through `wgsl_source`: texture parameters (`tex_height_at`) crash Mali's compiler.
                let module = |label: &str, source: &str| {
                    d.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some(label),
                        source: regia_render::wgsl_source(source),
                    })
                };
                let terrain_shader = module("terrain.wgsl", &terrain_source);
                let scatter_shader = module("scatter.wgsl", include_str!("scatter.wgsl"));
                let water_shader = module("water.wgsl", include_str!("water.wgsl"));
                let sky_shader = module("sky.wgsl", include_str!("sky.wgsl"));

                let make_layout = |label: &str, bgl: &[&wgpu::BindGroupLayout]| {
                    d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some(label),
                        bind_group_layouts: &bgl.iter().map(|l| Some(*l)).collect::<Vec<_>>(),
                        immediate_size: 0,
                    })
                };
                let terrain_pipeline_layout = make_layout(
                    "terrain layout",
                    &[&frame_layout, &terrain_layout, &node_layout, &shadow_layout],
                );
                let scatter_pipeline_layout =
                    make_layout("scatter layout", &[&frame_layout, &scatter_layout]);
                let water_pipeline_layout = make_layout("water layout", &[&frame_layout, &water_layout]);
                // Sky uses only the frame bind group (group 0).
                let sky_pipeline_layout = make_layout("sky layout", &[&frame_layout]);

                let terrain_pipeline =
                    offscreen_terrain_pipeline(d, &terrain_pipeline_layout, &terrain_shader, false);

                let scatter_pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("terrain scatter"),
                    layout: Some(&scatter_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &scatter_shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[Some(wgpu::VertexBufferLayout {
                            array_stride: size_of::<ScatterInstanceGpu>() as u64,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &[
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 0,
                                    shader_location: 0,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 16,
                                    shader_location: 1,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 32,
                                    shader_location: 2,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 48,
                                    shader_location: 3,
                                },
                            ],
                        })],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &scatter_shader,
                        entry_point: Some("fs_main"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: colour_format,
                            blend: None,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        front_face: wgpu::FrontFace::Ccw,
                        cull_mode: Some(wgpu::Face::Back),
                        ..Default::default()
                    },
                    depth_stencil: Some(depth_stencil),
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
                });

                // Scatter casts into the sun cascade. Depth only; the colour pass samples it.
                // Back faces are culled (the same winding as the colour pass). Acne is handled
                // by the receiver bias in the terrain shader, not a polygon offset.
                let shadow_pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("terrain sun shadow"),
                    layout: Some(&scatter_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &scatter_shader,
                        entry_point: Some("vs_shadow"),
                        compilation_options: Default::default(),
                        buffers: &[Some(wgpu::VertexBufferLayout {
                            array_stride: size_of::<ScatterInstanceGpu>() as u64,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &[
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 0,
                                    shader_location: 0,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 16,
                                    shader_location: 1,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 32,
                                    shader_location: 2,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 48,
                                    shader_location: 3,
                                },
                            ],
                        })],
                    },
                    fragment: None,
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        front_face: wgpu::FrontFace::Ccw,
                        cull_mode: Some(wgpu::Face::Back),
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: depth_format,
                        depth_write_enabled: Some(true),
                        depth_compare: Some(wgpu::CompareFunction::Less),
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
                });

                let water_pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("terrain water"),
                    layout: Some(&water_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &water_shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &water_shader,
                        entry_point: Some("fs_main"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: colour_format,
                            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        front_face: wgpu::FrontFace::Ccw,
                        cull_mode: None,
                        ..Default::default()
                    },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: depth_format,
                        depth_write_enabled: Some(false),
                        depth_compare: Some(wgpu::CompareFunction::Less),
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
                });

                // Sky: full-screen triangle, no depth write (background fill before terrain).
                let sky_pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("terrain sky"),
                    layout: Some(&sky_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &sky_shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &sky_shader,
                        entry_point: Some("fs_main"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: colour_format,
                            blend: None,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        front_face: wgpu::FrontFace::Ccw,
                        cull_mode: None,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
                });

                OffscreenPipelines {
                    terrain_shader,
                    terrain_pipeline_layout,
                    terrain_pipeline,
                    scatter_pipeline,
                    shadow_pipeline,
                    water_pipeline,
                    sky_pipeline,
                }
            },
        );
        let OffscreenPipelines {
            terrain_shader,
            terrain_pipeline_layout,
            terrain_pipeline,
            scatter_pipeline,
            shadow_pipeline,
            water_pipeline,
            sky_pipeline,
        } = offscreen;
        let terrain_holes_pipeline = mask.filter(|m| !m.is_empty()).map(|_| {
            offscreen_holes_pipeline(
                &shared,
                d,
                &terrain_source,
                &terrain_pipeline_layout,
                &terrain_shader,
            )
        });

        let cascade_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("terrain cascade layout"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX, false)],
        });
        // Four cascades, then the precipitation occlusion tile (`PRECIP_SHADOW_SLOT`).
        let mut cascade_bufs = Vec::with_capacity(5);
        let mut cascade_binds = Vec::with_capacity(5);
        for i in 0..5 {
            let buf = make_uniform_buffer(
                &format!("terrain cascade {i}"),
                size_of::<CascadeUniform>() as u64,
            );
            let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("terrain cascade bind"),
                layout: &cascade_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buf.as_entire_binding(),
                }],
            });
            cascade_bufs.push(buf);
            cascade_binds.push(bind);
        }
        let identity = regia_core::glam::Mat4::IDENTITY.to_cols_array_2d();

        let gpu = Self {
            device: tracked_device.clone(),
            queue: device.queue.clone(),
            colour_format,
            depth_format,
            height_texture,
            world_width: map.world_width(),
            world_depth: map.world_depth(),
            grid_origin: map.grid_origin(),
            frame_layout,
            frame_buffer,
            occluder_buffer,
            occluder_bytes: Vec::new(),
            frame_bind,
            sky_irradiance: sky_irradiance_dummy.clone(),
            sky_brdf: sky_irradiance_dummy.clone(),
            sky_irradiance_dummy,
            sky_sampler,
            sky_irradiance_on: false,
            linear_output: false,
            hdr_scale: 1.0,
            motion_set: false,
            prev_view_proj: identity,
            unjittered_view_proj: identity,
            shared_format: None,
            shared_formats_seen: Vec::new(),
            shared_color: None,
            shared_depth: None,
            shared_shadow: None,
            shared_velocity: None,
            shared_receiver: None,
            shared_holes: None,
            shared_shader: None,
            shared_shader_lights: false,
            shared_ground_look: false,
            shared_surface_weather: false,
            shared_cloud_map: false,
            surface_weather_frames: 0,
            warm_surface_weather: false,
            shared_cost_view: false,
            terrain_holes_pipeline,
            terrain_shader,
            terrain_pipeline_layout,
            terrain_source,
            shared_source: None,
            shared_pipelines: shared,
            holes_buffer,
            hole_lattice: None,
            gi_holes_buffer: None,
            holes_generation: 0,
            mouth_bands: None,
            band_positions_at: 0,
            band_camera: 0,
            cascade_layout,
            cascade_bufs,
            cascade_binds,
            terrain_layout,
            height_sampler,
            terrain_uniform,
            height_view,
            terrain_buffer,
            layers_buffer,
            material_sampler,
            material_aniso_sampler,
            material_buffer,
            material_state: material_uniform,
            texture_bytes: height_texture_bytes + material_textures.bytes(),
            height_texture_bytes,
            height_mips: SharedHeightMips::new(height_mips),
            slope,
            sim_bytes: 0,
            layers_desc: layers.clone(),
            layer_flags: NO_LAYER_TEXTURES,
            layer_texel: None,
            layer_mip_skip: 0,
            splat_texel: 0.0,
            altitude_range: std::sync::Mutex::new(map_altitude_range(map)),
            ground_ambient: None,
            fill_light: None,
            extra_lights: [[0.0; 4]; 2 * regia_render::MAX_EXTRA_DIRECTIONAL],
            fill_canyon_scope: crate::FillCanyonScope::Off,
            texture_lod_bias: 0.0,
            anisotropy: regia_render::TerrainBudget::OFF.anisotropy_level(),
            anisotropy_footprint: aniso_path::footprint_for_device(device),
            anisotropy_lod_offset: aniso_path::lod_offset_for_device(device),
            sssr_codes: false,
            material_textures,
            contact,
            contact_binds: 0,
            terrain_bind,
            scorch_buffer,
            node_layout,
            node_buffer,
            node_capacity,
            node_stride,
            node_bind,
            slot_buffer,
            slot_capacity,
            slot_cpu: Vec::new(),
            slot_scratch: Vec::new(),
            list_holes: Vec::new(),
            list_dense: Vec::new(),
            identity_slots: Vec::new(),
            draw_lists: Vec::new(),
            grid_index_buffer,
            grid_index_count,
            shadow_index_buffer,
            shadow_index_count,
            dense_index_buffer,
            dense_index_count,
            dense_shadow_index_buffer,
            dense_shadow_index_count,
            dense_cpu: Vec::new(),
            steep_nodes: std::sync::Mutex::new(HashMap::new()),
            detail_eye: None,
            relief_cliff: TerrainRelief::default().cliff_relief,
            hole_index_buffers: HashMap::new(),
            hole_states: HashMap::new(),
            slot_holes: Vec::new(),
            hole_tests_last: 0,
            hole_cut_nodes: std::collections::HashSet::new(),
            hole_index_builds_last: 0,
            scatter_params_buffer,
            scatter_bind,
            instance_buffer: None,
            instance_count: 0,
            water_bind,
            water_layout,
            water_plane,
            water_scene_stand_in,
            water_refraction: true,
            terrain_pipeline,
            scatter_pipeline,
            shadow_pipeline,
            water_pipeline,
            sky_pipeline,
            shadow_view,
            shadow_bind,
            shadow_sampler,
            engine_csm_layout,
            engine_csm_buffer,
            engine_csm_bind,
            engine_csm_views: Vec::new(),
            engine_contact: engine_ao_dummy.clone(),
            engine_contact_on: false,
            engine_cloud: engine_ao_dummy.clone(),
            engine_cloud_params: [0.0; 4],
            engine_precip: None,
            engine_ao: engine_ao_dummy.clone(),
            engine_ao_on: false,
            engine_gi: engine_gi_dummy.clone(),
            engine_gi_dummy,
            engine_lights: engine_lights_dummy.clone(),
            engine_lights_dummy,
            engine_light_profiles: engine_ao_dummy.clone(),
            local_lights,
            local_lights_enabled: true,
            engine_csm_dummy,
            engine_ao_dummy,
            node_bind_holes: 0,
            node_cpu: vec![0; node_capacity as usize * node_stride as usize],
            node_upload_last: (0, 0),
            frame_bytes: Vec::new(),
            engine_csm_bytes: Vec::new(),
            cascade_bytes: Vec::new(),
            sun_cache: std::sync::Mutex::new(SunShadowCache::default()),
            scatter_generation: 0,
            sun_renders: std::sync::atomic::AtomicU64::new(0),
            config: config.clone(),
            root_level,
            mask: mask.cloned(),
            height_scale: map.height_scale(),
            height_offset: map.height_offset(),
            vt_settings: None,
            vt_explicit: false,
            vt_profile: (false, false),
            vt: None,
            vt_handle: vt::TerrainVtHandle::linked(vt::VtDepthTest::of_device(&d.limits(), d.features())),
            shared_color_vt: None,
            surface_generation: std::sync::atomic::AtomicU64::new(0),
            vt_height_dirty: std::sync::Mutex::new(Vec::new()),
            vt_edit_counters: std::sync::Mutex::new(VtEditCounters::default()),
            vt_frame_terms: [0.0; 4],
            vt_key_sun: None,
        };
        aniso_path::record_created_draw(
            gpu.anisotropy_footprint,
            gpu.anisotropy_lod_offset,
            publish_aniso_report,
        );
        Ok(gpu)
    }

    /// The colour format the terrain pipelines render into (hook contract).
    pub fn colour_format(&self) -> wgpu::TextureFormat {
        self.colour_format
    }

    /// The depth format the terrain pipelines expect (hook contract).
    pub fn depth_format(&self) -> wgpu::TextureFormat {
        self.depth_format
    }

    /// Refreshes the GPU height mirror over a dirty rectangle (after
    /// [`crate::deform::apply_crater`]), normalising metre heights to the 0..1 range the
    /// WGSL shader expects. Empty rects are ignored.
    ///
    /// Everything a load derives from the heights follows the edit, so the frames equal a
    /// cold load of the edited map: the height texture and its pyramid, the slope pyramid, and
    /// the map's altitude range (scanned from `map`, which costs one pass over the samples
    /// the first time after an edit; [`crate::TerrainSharedDraw`] reads it from its min/max
    /// pyramid instead).
    pub fn update_heights(&self, map: &TerrainMap, dirty: &DirtyRect) {
        if dirty.is_empty() {
            return;
        }
        self.update_heights_with_range(map, dirty, map_altitude_range(map));
    }

    /// [`Self::update_heights`] with the edited map's altitude range given (`[min, max]`
    /// metres, what `map.min_max_height()` returns).
    pub(crate) fn update_heights_with_range(&self, map: &TerrainMap, dirty: &DirtyRect, altitude: [f32; 2]) {
        if dirty.is_empty() {
            return;
        }
        self.set_altitude_range(altitude);
        // With a built virtual texture only the pages the edit reaches re-bake (in place);
        // without one the generation bump is all the surface key needs.
        if self.vt.is_some() {
            self.note_height_edit(map, dirty);
        } else {
            self.bump_surface();
        }
        self.steep_nodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let scale_recip = if self.height_scale.abs() > 1e-9 {
            self.height_scale.recip()
        } else {
            1.0
        };
        let w = dirty.col_end - dirty.col_start;
        let mut data = Vec::with_capacity((w * (dirty.row_end - dirty.row_start)) as usize);
        for row in dirty.row_start..dirty.row_end {
            let base = row as usize * map.width() as usize + dirty.col_start as usize;
            let slice = &map.heights()[base..base + w as usize];
            data.extend(slice.iter().map(|&h| (h - self.height_offset) * scale_recip));
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.height_texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: dirty.col_start,
                    y: dirty.row_start,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&data),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(dirty.row_end - dirty.row_start),
            },
            wgpu::Extent3d {
                width: w,
                height: dirty.row_end - dirty.row_start,
                depth_or_array_layers: 1,
            },
        );
        // The pyramid levels that read the edited samples.
        let level_rects = self.height_mips.write(|mips| mips.update_region(map, dirty));
        let mips = self.height_mips.snapshot();
        for (i, rect) in level_rects.iter().enumerate() {
            let k = i as u32 + 1;
            if let Some(level) = mips.level(k) {
                write_height_mip(
                    &self.queue,
                    &self.height_texture,
                    k,
                    level,
                    *rect,
                    self.height_offset,
                    self.height_scale,
                );
            }
        }
        // The slope texels whose central difference reads a changed height, every level.
        let base = (dirty.col_start, dirty.col_end, dirty.row_start, dirty.row_end);
        for (k, rect) in std::iter::once(base).chain(level_rects).enumerate() {
            let k = k as u32;
            let rect = crate::slope_mips::dependent_rect(map, &mips, k, rect);
            write_slope_rect(&self.queue, &self.slope.texture, map, &mips, k, rect);
        }
    }

    /// Uploads scatter instances plus their fade distances and alignment tilt.
    pub fn set_scatter(
        &mut self,
        instances: &[ScatterInstance],
        fade_start_distance: f32,
        fade_end_distance: f32,
        align_tilt: f32,
    ) {
        self.scatter_generation += 1;
        self.instance_count = instances.len() as u32;
        if instances.is_empty() {
            self.instance_buffer = None;
            self.instance_count = 0;
        } else {
            let gpu_instances: Vec<ScatterInstanceGpu> =
                instances.iter().map(ScatterInstanceGpu::from).collect();
            self.instance_buffer = Some(upload_bytes(
                &self.device,
                "terrain scatter instances",
                bytemuck::cast_slice(&gpu_instances),
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            ));
        }
        let params = ScatterParamsUniform {
            fade_start: fade_start_distance,
            fade_end: fade_end_distance,
            align_tilt: align_tilt.clamp(0.0, 1.0),
            pad: 0.0,
        };
        self.queue
            .write_buffer(&self.scatter_params_buffer, 0, bytemuck::bytes_of(&params));
    }

    /// Uploads the scorch marks (albedo darkening hook).
    pub fn set_scorch(&self, list: &crate::deform::ScorchList) {
        let uniform = ScorchUniform { rows: list.pack() };
        self.queue
            .write_buffer(&self.scorch_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    /// Installs the photoreal material (per-layer textures, macro maps, knobs). This switches
    /// the terrain shader onto the material path; without it (or with an empty `layers`
    /// vector and no macro set) the legacy v1 look renders unchanged.
    ///
    /// `macro_resident` names the macro tiles to upload (by index; `&[]` uploads all). Only
    /// the resident tiles are allocated in the tile array — the shader remaps tile → layer
    /// through the residency bitmask and falls back to the coarse whole-map layer for the
    /// rest, which is what keeps an 8K bake inside a streaming budget.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when the material fails validation
    /// ([`crate::material::TerrainMaterial::validate`]).
    pub fn set_material(
        &mut self,
        material: &crate::material::TerrainMaterial,
        macro_resident: &[u32],
    ) -> Result<(), TerrainError> {
        material.validate()?;
        self.bump_surface();
        let d = &self.device;

        // Refresh the layer texture-present flags: the runtime material is the truth about
        // which layers actually have GPU textures (the layer desc only names asset paths).
        let layer_texel = material
            .layers
            .iter()
            .flatten()
            .next()
            .map(|set| 1.0 / set.albedo.width as f32)
            .unwrap_or(1.0);
        let mut flags = NO_LAYER_TEXTURES;
        for (slot, flag) in flags.iter_mut().enumerate() {
            let set = material.layers.get(slot).and_then(std::option::Option::as_ref);
            *flag = (set.is_some(), set.is_some_and(|set| set.use_orm));
        }
        self.layer_flags = flags;
        self.layer_texel = Some(layer_texel);
        self.write_layers_uniform();

        // ---- Layer arrays (one layer per material slot, neutral padding) ----
        let layer_size = material
            .layers
            .iter()
            .flatten()
            .next()
            .map_or(1, |set| set.albedo.width);
        let mut albedo_layers: Vec<crate::material::Rgba8Image> =
            vec![crate::material::Rgba8Image::neutral_albedo(layer_size); MAX_LAYERS];
        let mut normal_layers = vec![crate::material::Rgba8Image::neutral_normal(layer_size); MAX_LAYERS];
        let mut height_layers = vec![crate::material::Rgba8Image::neutral_height(layer_size); MAX_LAYERS];
        // Neutral ORM: AO 1, roughness 1, metal 0. Sampled only when `use_orm` is set.
        let mut orm_layers =
            vec![crate::material::Rgba8Image::solid(layer_size, layer_size, [255, 255, 0, 255]); MAX_LAYERS];
        let recentre_normals = material.params.anti_tiling_cell > 0.0;
        for (slot, set) in material.layers.iter().take(MAX_LAYERS).enumerate() {
            if let Some(set) = set {
                albedo_layers[slot] = set.albedo.clone();
                // A biased normal map is a constant tilt; the anti-tiling lattice rotates it per
                // cell (ADR-1428). Without the lattice the tilt is just authored: left as is.
                normal_layers[slot] = if recentre_normals {
                    set.normal
                        .recentred_normal()
                        .unwrap_or_else(|| set.normal.clone())
                } else {
                    set.normal.clone()
                };
                height_layers[slot] = set.height.clone();
                orm_layers[slot] = set.orm.clone();
            }
        }
        let albedo_refs: Vec<&_> = albedo_layers.iter().collect();
        let normal_refs: Vec<&_> = normal_layers.iter().collect();
        let height_refs: Vec<&_> = height_layers.iter().collect();
        let orm_refs: Vec<&_> = orm_layers.iter().collect();
        let layer_albedo = GpuArrayTexture::upload(
            d,
            &self.queue,
            "terrain layer albedo",
            &albedo_refs,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );
        let layer_normal = GpuArrayTexture::upload(
            d,
            &self.queue,
            "terrain layer normal",
            &normal_refs,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let layer_height = GpuArrayTexture::upload(
            d,
            &self.queue,
            "terrain layer height",
            &height_refs,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let layer_orm = GpuArrayTexture::upload(
            d,
            &self.queue,
            "terrain layer orm",
            &orm_refs,
            wgpu::TextureFormat::Rgba8Unorm,
        );

        // ---- Macro tiles + coarse fallback ----
        let (macro_albedo, macro_normal, coarse_albedo, coarse_normal, tiles, mask, macro_texel) =
            if let Some(bake) = &material.macro_set {
                let all: Vec<u32> = (0..bake.tiles * bake.tiles).collect();
                // Ascending and unique: the shader ranks a tile in the mask to find its layer.
                let resident = crate::material::macro_upload_order(macro_resident, bake.tiles);
                let plan: Vec<u32> = if resident.is_empty() { all } else { resident };
                let mask = crate::material::macro_residency(&plan);
                let albedo_refs: Vec<&_> = plan.iter().map(|&i| &bake.albedo[i as usize]).collect();
                let normal_refs: Vec<&_> = plan.iter().map(|&i| &bake.normal[i as usize]).collect();
                (
                    GpuArrayTexture::upload(
                        d,
                        &self.queue,
                        "terrain macro albedo tiles",
                        &albedo_refs,
                        wgpu::TextureFormat::Rgba8UnormSrgb,
                    ),
                    GpuArrayTexture::upload(
                        d,
                        &self.queue,
                        "terrain macro normal tiles",
                        &normal_refs,
                        wgpu::TextureFormat::Rgba8Unorm,
                    ),
                    GpuArrayTexture::upload(
                        d,
                        &self.queue,
                        "terrain macro coarse albedo",
                        &[&bake.coarse_albedo],
                        wgpu::TextureFormat::Rgba8UnormSrgb,
                    ),
                    GpuArrayTexture::upload(
                        d,
                        &self.queue,
                        "terrain macro coarse normal",
                        &[&bake.coarse_normal],
                        wgpu::TextureFormat::Rgba8Unorm,
                    ),
                    bake.tiles,
                    mask,
                    // Metres per macro texel for the explicit-LOD footprint.
                    bake.world_size / (bake.tiles * bake.tile_size) as f32,
                )
            } else {
                let one = crate::material::Rgba8Image::solid(1, 1, [255, 255, 255, 255]);
                let one_mid = crate::material::Rgba8Image::solid(1, 1, [128, 128, 128, 255]);
                (
                    GpuArrayTexture::upload(
                        d,
                        &self.queue,
                        "macro (off)",
                        &[&one],
                        wgpu::TextureFormat::Rgba8UnormSrgb,
                    ),
                    GpuArrayTexture::upload(
                        d,
                        &self.queue,
                        "macro normal (off)",
                        &[&one_mid],
                        wgpu::TextureFormat::Rgba8Unorm,
                    ),
                    GpuArrayTexture::upload(
                        d,
                        &self.queue,
                        "macro coarse (off)",
                        &[&one],
                        wgpu::TextureFormat::Rgba8UnormSrgb,
                    ),
                    GpuArrayTexture::upload(
                        d,
                        &self.queue,
                        "macro coarse normal (off)",
                        &[&one_mid],
                        wgpu::TextureFormat::Rgba8Unorm,
                    ),
                    1,
                    crate::material::macro_residency(&[]),
                    1.0,
                )
            };

        // Keep the sim texture (and its cost) from the previous state.
        let sim_bytes = self.sim_bytes;
        let old = std::mem::replace(
            &mut self.material_textures,
            MaterialTextures::neutral(&self.device, &self.queue),
        );
        self.material_textures = MaterialTextures {
            macro_albedo,
            macro_normal,
            macro_coarse_albedo: coarse_albedo,
            macro_coarse_normal: coarse_normal,
            layer_albedo,
            layer_normal,
            layer_height,
            layer_orm,
            sim_height: old.sim_height,
            splat: old.splat,
        };
        self.sim_bytes = sim_bytes;
        let p = &material.params;
        let enabled = f32::from(!material.layers.is_empty() || material.macro_set.is_some());
        self.material_state.params0 = [
            enabled,
            if material.macro_set.is_some() {
                p.macro_world_size
            } else {
                0.0
            },
            p.anti_tiling_cell,
            p.macro_strength,
        ];
        self.material_state.params1 = [
            p.detail_fade_start,
            p.detail_fade_end,
            p.triplanar_auto_slope,
            p.macro_normal_strength,
        ];
        self.material_state.tiles = [tiles as f32, 1.0 / tiles as f32, 0.0, macro_texel];
        (self.material_state.macro_mask, self.material_state.macro_prefix) = mask;
        self.queue
            .write_buffer(&self.material_buffer, 0, bytemuck::bytes_of(&self.material_state));
        self.terrain_bind = self.build_terrain_bind();
        self.texture_bytes = self.height_texture_bytes + self.material_textures.bytes() + self.sim_bytes;
        Ok(())
    }

    /// Leaves the top `skip` levels of the cooked layer arrays (albedo, normal, height, ORM) out
    /// of the next [`Self::set_material_cooked`]: a lower memory tier (each level is 3/4 of
    /// the array). `0` (the default) uploads the authored size; [`crate::layer_mip_skip_for`] gives
    /// the per-preset value. The macro maps and the heightfield are unaffected.
    pub fn set_layer_mip_skip(&mut self, skip: u32) {
        self.bump_surface();
        self.layer_mip_skip = skip;
    }

    /// Installs splat weight packs over the whole map (`docs/formats/terrain.md` § 9: `macro/pack0.png` holds
    /// the weights of layers 0–3 in R, G, B, A, `macro/pack1.png` those of layers 4–7), or removes them with
    /// `None`. Column 0 is the map's −X edge and row 0 its −Z edge, like the heightfield; any size works (the
    /// packs are sampled bilinearly in map UV, and with a cubic B-spline where a pack texel spans only a few
    /// pixels: [`crate::far_splat`], ADR-1456). With packs a layer's weight is its pack weight times its slope
    /// and altitude band (open bands leave the pack weight as authored); where the packs hold no weight the
    /// bands alone decide. Takes effect on the photoreal material path.
    ///
    /// # Errors
    ///
    /// [`TerrainError::Gpu`] when the two packs differ in size or exceed the device's texture limit.
    pub fn set_splat_weights(
        &mut self,
        packs: Option<[&crate::material::Rgba8Image; 2]>,
    ) -> Result<(), TerrainError> {
        let bad = |message: String| TerrainError::Gpu(regia_render::RenderError::Readback(message));
        let (splat, texel) = match packs {
            Some([pack0, pack1]) => {
                let max_dim = self.device.limits().max_texture_dimension_2d;
                if pack0.width != pack1.width || pack0.height != pack1.height {
                    return Err(bad(format!(
                        "splat packs differ in size: {}x{} and {}x{}",
                        pack0.width, pack0.height, pack1.width, pack1.height
                    )));
                }
                if pack0.width == 0 || pack0.height == 0 || pack0.width > max_dim || pack0.height > max_dim {
                    return Err(bad(format!(
                        "splat packs {}x{} exceed the texture limit {max_dim}",
                        pack0.width, pack0.height
                    )));
                }
                if pack0.data.len() != (pack0.width * pack0.height * 4) as usize
                    || pack1.data.len() != pack0.data.len()
                {
                    return Err(bad("splat pack data does not match its size".into()));
                }
                (
                    GpuArrayTexture::upload(
                        &self.device,
                        &self.queue,
                        "terrain splat weights",
                        &[pack0, pack1],
                        wgpu::TextureFormat::Rgba8Unorm,
                    ),
                    1.0 / pack0.width.max(pack0.height) as f32,
                )
            }
            None => {
                let zero = crate::material::Rgba8Image::solid(1, 1, [0, 0, 0, 0]);
                (
                    GpuArrayTexture::upload(
                        &self.device,
                        &self.queue,
                        "terrain splat weights (off)",
                        &[&zero, &zero],
                        wgpu::TextureFormat::Rgba8Unorm,
                    ),
                    0.0,
                )
            }
        };
        self.bump_surface();
        self.material_textures.splat = splat;
        self.splat_texel = texel;
        self.write_layers_uniform();
        self.terrain_bind = self.build_terrain_bind();
        self.texture_bytes = self.height_texture_bytes + self.material_textures.bytes() + self.sim_bytes;
        Ok(())
    }

    /// Installs a photoreal material that was cooked to KTX2/BCn
    /// (`regia_asset_pipeline::cook_terrain_material`). The blobs are
    /// uploaded as hardware-sampled block-compressed mip-chained arrays — no CPU decode on a
    /// device that samples their format; elsewhere they are decoded to RGBA8.
    /// `macro_tiles` is the tile grid side of the bake (`MacroBake::tiles`);
    /// `macro_resident` selects the macro tile blobs to upload (`&[]` = all).
    ///
    /// The blobs may be owned (`Vec<u8>`), views of a mapped pak (`regia_asset::PakPayload`,
    /// `crate::stream::PakTileSource::load_cooked_material`) or borrowed slices: they are read in
    /// place, never copied on the CPU before the upload (ADR-8024).
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when a blob cannot be parsed or mapped (see
    /// [`Self::set_material`]).
    pub fn set_material_cooked<B: AsRef<[u8]>>(
        &mut self,
        cooked: &crate::material::CookedMaterialKtx2<B>,
        params: MaterialParams,
        macro_tiles: u32,
        macro_resident: &[u32],
    ) -> Result<(), TerrainError> {
        self.set_material_cooked_view(&cooked.view(), params, macro_tiles, macro_resident)
    }

    /// [`Self::set_material_cooked`] over borrowed blobs (one body for every blob type).
    fn set_material_cooked_view(
        &mut self,
        cooked: &crate::material::CookedMaterialKtx2<&[u8]>,
        params: MaterialParams,
        macro_tiles: u32,
        macro_resident: &[u32],
    ) -> Result<(), TerrainError> {
        self.bump_surface();
        let d = &self.device;

        let layer_albedo = if cooked.layer_albedo.is_empty() {
            MaterialTextures::neutral(d, &self.queue).layer_albedo
        } else {
            GpuArrayTexture::upload_ktx2_array_from(
                d,
                &self.queue,
                "terrain layer albedo (bcn)",
                &cooked.layer_albedo,
                self.layer_mip_skip,
            )?
        };
        let layer_normal = if cooked.layer_normal.is_empty() {
            MaterialTextures::neutral(d, &self.queue).layer_normal
        } else {
            GpuArrayTexture::upload_ktx2_array_from(
                d,
                &self.queue,
                "terrain layer normal (bcn)",
                &cooked.layer_normal,
                self.layer_mip_skip,
            )?
        };
        let layer_height = if cooked.layer_height.is_empty() {
            MaterialTextures::neutral(d, &self.queue).layer_height
        } else {
            GpuArrayTexture::upload_ktx2_array_from(
                d,
                &self.queue,
                "terrain layer height (bcn)",
                &cooked.layer_height,
                self.layer_mip_skip,
            )?
        };
        let layer_orm = if cooked.layer_orm.is_empty() {
            MaterialTextures::neutral(d, &self.queue).layer_orm
        } else {
            GpuArrayTexture::upload_ktx2_array_from(
                d,
                &self.queue,
                "terrain layer orm (bcn)",
                &cooked.layer_orm,
                self.layer_mip_skip,
            )?
        };
        let (macro_albedo, macro_normal, coarse_albedo, coarse_normal, mask, macro_texel) =
            if !cooked.macro_albedo_tiles.is_empty() && macro_tiles > 0 {
                let tile_count = macro_tiles
                    .checked_mul(macro_tiles)
                    .filter(|_| macro_tiles <= crate::material::MAX_MACRO_TILES_PER_SIDE)
                    .ok_or(crate::material::MaterialError::BadTileGrid {
                        tiles: macro_tiles,
                        tile_size: 0,
                    })?;
                let expected = tile_count as usize;
                if cooked.macro_albedo_tiles.len() != expected || cooked.macro_normal_tiles.len() != expected
                {
                    return Err(crate::material::MaterialError::BadTileCount {
                        albedo: cooked.macro_albedo_tiles.len(),
                        normal: cooked.macro_normal_tiles.len(),
                        expected,
                    }
                    .into());
                }
                let all: Vec<u32> = (0..tile_count).collect();
                // Ascending and unique: the shader ranks a tile in the mask to find its layer.
                let resident = crate::material::macro_upload_order(macro_resident, macro_tiles);
                let plan = if resident.is_empty() { all } else { resident };
                let mask = crate::material::macro_residency(&plan);
                let tile_size = safe_ktx2_reader(cooked.macro_albedo_tiles[0])
                    .map_err(|e| {
                        TerrainError::Gpu(regia_render::RenderError::Readback(format!(
                            "ktx2 macro tile: {e}"
                        )))
                    })?
                    .header()
                    .pixel_width;
                let albedo_blobs: Vec<&[u8]> = plan
                    .iter()
                    .map(|&i| cooked.macro_albedo_tiles[i as usize])
                    .collect();
                let normal_blobs: Vec<&[u8]> = plan
                    .iter()
                    .map(|&i| cooked.macro_normal_tiles[i as usize])
                    .collect();
                let texel = if tile_size > 0 {
                    self.world_width / (macro_tiles * tile_size) as f32
                } else {
                    1.0
                };
                let coarse_albedo = match cooked.macro_coarse_albedo {
                    Some(blob) => GpuArrayTexture::upload_ktx2_array(
                        d,
                        &self.queue,
                        "terrain macro coarse albedo (bcn)",
                        &[blob],
                    )?,
                    None => MaterialTextures::neutral(d, &self.queue).macro_coarse_albedo,
                };
                let coarse_normal = match cooked.macro_coarse_normal {
                    Some(blob) => GpuArrayTexture::upload_ktx2_array(
                        d,
                        &self.queue,
                        "terrain macro coarse normal (bcn)",
                        &[blob],
                    )?,
                    None => MaterialTextures::neutral(d, &self.queue).macro_coarse_normal,
                };
                (
                    GpuArrayTexture::upload_ktx2_array(
                        d,
                        &self.queue,
                        "terrain macro albedo tiles (bcn)",
                        &albedo_blobs,
                    )?,
                    GpuArrayTexture::upload_ktx2_array(
                        d,
                        &self.queue,
                        "terrain macro normal tiles (bcn)",
                        &normal_blobs,
                    )?,
                    coarse_albedo,
                    coarse_normal,
                    mask,
                    texel,
                )
            } else {
                let neutral = MaterialTextures::neutral(d, &self.queue);
                (
                    neutral.macro_albedo,
                    neutral.macro_normal,
                    neutral.macro_coarse_albedo,
                    neutral.macro_coarse_normal,
                    crate::material::macro_residency(&[]),
                    1.0,
                )
            };

        // Refresh the layer uniform flags from which slots were cooked.
        let layer_texel = cooked
            .layer_albedo
            .first()
            .and_then(|blob| safe_ktx2_reader(blob).ok())
            .map_or(1.0, |reader| {
                1.0 / (reader.header().pixel_width >> self.layer_mip_skip).max(1) as f32
            });
        let mut flags = NO_LAYER_TEXTURES;
        for (slot, flag) in flags.iter_mut().enumerate() {
            *flag = (slot < cooked.layer_albedo.len(), slot < cooked.layer_orm.len());
        }
        self.layer_flags = flags;
        self.layer_texel = Some(layer_texel);
        self.write_layers_uniform();

        let sim_bytes = self.sim_bytes;
        let old = std::mem::replace(
            &mut self.material_textures,
            MaterialTextures::neutral(&self.device, &self.queue),
        );
        self.material_textures = MaterialTextures {
            macro_albedo,
            macro_normal,
            macro_coarse_albedo: coarse_albedo,
            macro_coarse_normal: coarse_normal,
            layer_albedo,
            layer_normal,
            layer_height,
            layer_orm,
            sim_height: old.sim_height,
            splat: old.splat,
        };
        self.sim_bytes = sim_bytes;
        let enabled = f32::from(!cooked.layer_albedo.is_empty() || !cooked.macro_albedo_tiles.is_empty());
        self.material_state.params0 = [
            enabled,
            if !cooked.macro_albedo_tiles.is_empty() && macro_tiles > 0 {
                self.world_width
            } else {
                0.0
            },
            params.anti_tiling_cell,
            params.macro_strength,
        ];
        self.material_state.params1 = [
            params.detail_fade_start,
            params.detail_fade_end,
            params.triplanar_auto_slope,
            params.macro_normal_strength,
        ];
        self.material_state.tiles = [
            macro_tiles.max(1) as f32,
            1.0 / macro_tiles.max(1) as f32,
            0.0,
            macro_texel,
        ];
        (self.material_state.macro_mask, self.material_state.macro_prefix) = mask;
        self.queue
            .write_buffer(&self.material_buffer, 0, bytemuck::bytes_of(&self.material_state));
        self.terrain_bind = self.build_terrain_bind();
        self.texture_bytes = self.height_texture_bytes + self.material_textures.bytes() + self.sim_bytes;
        Ok(())
    }

    /// Replaces the splat layer stack (bands, tints, tiling, blend knobs) after construction.
    ///
    /// The installed material's textures stay bound: slot `i` of the new stack samples
    /// texture layer `i` of the last [`Self::set_material`] / [`Self::set_material_cooked`].
    /// Install the material again when the new stack reorders or adds textured layers.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError::InvalidLayers`] when the stack fails validation; the
    /// previous stack stays in use.
    pub fn set_layers(&mut self, layers: &TerrainLayers) -> Result<(), TerrainError> {
        layers.validate()?;
        self.bump_surface();
        self.layers_desc = layers.clone();
        self.write_layers_uniform();
        Ok(())
    }

    /// The layer stack the shader currently blends.
    #[must_use]
    pub fn layers(&self) -> &TerrainLayers {
        &self.layers_desc
    }

    /// Writes the layer uniform from the stack, the material flags and the map's altitude range.
    fn write_layers_uniform(&self) {
        let mut uniform = layers_uniform(
            &self.layers_desc,
            &self.layer_flags,
            self.layer_texel,
            self.altitude_range(),
        );
        // `count.w`: the splat packs' texel size in map UV (0 = no splat weights).
        uniform.count[3] = self.splat_texel;
        self.queue
            .write_buffer(&self.layers_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    /// Installs the simulation ground heightfield for the visual-vs-sim delta debug view
    /// (`TerrainFrame::debug_view == 7`). The sim map is uploaded with the same normalised
    /// mirror the visual heights use.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when the sim map exceeds texture limits.
    pub fn set_sim_map(&mut self, sim: &TerrainMap) -> Result<(), TerrainError> {
        let d = &self.device;
        let max_dim = d.limits().max_texture_dimension_2d;
        if sim.width() > max_dim || sim.height() > max_dim {
            return Err(TerrainError::Gpu(regia_render::RenderError::Readback(format!(
                "sim heightfield {}x{} exceeds max texture dimension {max_dim}",
                sim.width(),
                sim.height()
            ))));
        }
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain sim heights"),
            size: wgpu::Extent3d {
                width: sim.width(),
                height: sim.height(),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let scale_recip = if sim.height_scale().abs() > 1e-9 {
            sim.height_scale().recip()
        } else {
            1.0
        };
        let normalized: Vec<f32> = sim
            .heights()
            .iter()
            .map(|&h| (h - sim.height_offset()) * scale_recip)
            .collect();
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&normalized),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(sim.width() * 4),
                rows_per_image: Some(sim.height()),
            },
            wgpu::Extent3d {
                width: sim.width(),
                height: sim.height(),
                depth_or_array_layers: 1,
            },
        );
        self.sim_bytes = u64::from(sim.width()) * u64::from(sim.height()) * 4;
        self.material_state.sim = [
            sim.gpu_half_width(),
            sim.world_width(),
            sim.height_scale(),
            sim.height_offset(),
        ];
        self.queue
            .write_buffer(&self.material_buffer, 0, bytemuck::bytes_of(&self.material_state));
        self.material_textures.sim_height = texture;
        self.terrain_bind = self.build_terrain_bind();
        self.texture_bytes = self.height_texture_bytes + self.material_textures.bytes() + self.sim_bytes;
        Ok(())
    }

    /// Installs a same-depth road decal, or clears it when `road` is `None`.
    ///
    /// The fragment shader replaces albedo inside the ribbon and keeps the terrain
    /// depth, so a conforming road does not z-fight the surface it sits on.
    pub fn set_road(&mut self, road: Option<crate::road::RoadDecal>) {
        match road {
            Some(road) if road.half_width > 0.0 => {
                self.material_state.road0 = [road.a[0], road.a[1], road.b[0], road.b[1]];
                self.material_state.road1 = [road.half_width, road.color[0], road.color[1], road.color[2]];
            }
            _ => {
                self.material_state.road0 = [0.0; 4];
                self.material_state.road1 = [0.0; 4];
            }
        }
        self.queue
            .write_buffer(&self.material_buffer, 0, bytemuck::bytes_of(&self.material_state));
    }

    /// The shared height pyramid itself (the [`RenderedSurface`] handle reads it off-thread).
    pub(crate) fn height_mips_handle(&self) -> SharedHeightMips {
        self.height_mips.clone()
    }

    /// Runs `f` on the CPU copy of the box-filtered height pyramid the vertex shader reads
    /// (for [`crate::rendered_height`]).
    ///
    /// `f` reads a snapshot and runs with no lock held, so it may fan out on the compute pool
    /// (ADR-303): a thread waiting there runs other pool tasks, which may read the same pyramid.
    pub fn with_height_mips<R>(&self, f: impl FnOnce(&HeightMips) -> R) -> R {
        f(&self.height_mips.snapshot())
    }

    /// Sets the strata and near-field relief lighting of the photoreal material path
    /// ([`TerrainRelief`]; on by default). The legacy (no material) look ignores it.
    pub fn set_relief(&mut self, relief: TerrainRelief) {
        let (strata, rel, strata2, rock) = relief.uniforms();
        self.material_state.strata = strata;
        self.material_state.relief = rel;
        self.material_state.strata2 = strata2;
        self.material_state.rock = rock;
        self.material_state.cliff = relief.cliff_uniform();
        self.relief_cliff = relief.cliff_uniform()[0];
        self.queue
            .write_buffer(&self.material_buffer, 0, bytemuck::bytes_of(&self.material_state));
    }

    /// Sets the ground variation and late-winter surfaces of the photoreal material path
    /// ([`crate::TerrainGroundLook`], ADR-1408; off by default). The legacy (no material) look
    /// ignores it.
    ///
    /// The shared colour pass compiles the look's code only while it is on (ADR-1452): the first
    /// switch on after [`Self::ensure_shared`] builds the shader with the look and its colour
    /// pipelines (plain, hole-cut when a hole is cut) unless the device already has them, and
    /// rebuilds the virtual texture when it is on (its bake runs the look too); switching back
    /// off compiles nothing but the virtual texture's rebuild.
    pub fn set_ground_look(&mut self, look: crate::TerrainGroundLook) {
        let rows = look.uniforms();
        let [ground, winter, winter2] = rows;
        self.material_state.ground = ground;
        self.material_state.winter = winter;
        self.material_state.winter2 = winter2;
        self.queue
            .write_buffer(&self.material_buffer, 0, bytemuck::bytes_of(&self.material_state));
        let on = rows.iter().flatten().any(|v| *v != 0.0);
        if on != self.shared_ground_look {
            self.shared_ground_look = on;
            self.rebuild_ground_look_variant();
        }
    }

    /// The frame's surface weather (spec 99 § 6.4): the material rows, and the switch to the
    /// variant compiled with the `@surface-weather` code. The first wet frame builds its colour
    /// pipelines once, as the ground look's first use does; a dry frame keeps today's.
    fn upload_surface_weather(&mut self, info: &regia_render::SharedFrameInfo) {
        let weather = info.surface_weather;
        let on = !weather.is_dry() || info.precip_extinction_per_m > 0.0;
        if on != self.shared_surface_weather {
            self.shared_surface_weather = on;
            self.rebuild_ground_look_variant();
        }
        self.surface_weather_frames += u64::from(on);
        let tile = info.precip_occlusion;
        let atlas_edge = tile.map_or(1.0, |tile| tile.atlas_texels);
        let rows = (
            weather.row(),
            [
                weather.clock_s,
                if tile.is_some() { 1.0 } else { 0.0 },
                info.precip_extinction_per_m,
                atlas_edge,
            ],
            tile.map_or([[0.0; 4]; 4], |tile| tile.view_proj),
            tile.map_or([0.0; 4], |tile| tile.uv_rect),
            tile.map_or([0.0; 4], |tile| [tile.texel_m, tile.depth_per_m, 0.0, 0.0]),
        );
        let state = &mut self.material_state;
        let held = (
            state.weather,
            state.weather2,
            state.weather_tile,
            state.weather_rect,
            state.weather_bias,
        );
        if held == rows {
            return;
        }
        (
            state.weather,
            state.weather2,
            state.weather_tile,
            state.weather_rect,
            state.weather_bias,
        ) = rows;
        self.queue
            .write_buffer(&self.material_buffer, 0, bytemuck::bytes_of(&self.material_state));
    }

    /// Whether the shared pipelines run the surface-weather variant, and how many frames did
    /// (spec 99 § 4.5).
    #[must_use]
    pub fn surface_weather_report(&self) -> crate::TerrainSurfaceWeatherReport {
        crate::TerrainSurfaceWeatherReport {
            variant_on: self.shared_surface_weather,
            frames_wet: self.surface_weather_frames,
        }
    }

    /// Whether the built shared colour pipelines run the shader with the ground look (`None`
    /// before [`Self::ensure_shared`]): true while some row of [`Self::ground_look_uniforms`] is
    /// non-zero, else they run [`without_ground_look`]'s source (ADR-1452).
    #[must_use]
    pub fn shared_pipelines_ground_look(&self) -> Option<bool> {
        self.shared_color.as_ref().map(|_| self.shared_ground_look)
    }

    /// Swaps the shared colour pipelines (plain and hole-cut) to the ground-look variant
    /// [`Self::set_ground_look`] now wants, and rebuilds the virtual texture from that variant's
    /// shader (its bake runs the look's remix; a look change flushes its pages anyway,
    /// `vt_content_key`). ADR-1452. The depth, shadow, velocity and receiver pipelines never
    /// run the look and stay.
    fn rebuild_ground_look_variant(&mut self) {
        let (Some(format), Some(shader), Some(source)) = (
            self.shared_format,
            self.shared_shader.clone(),
            self.shared_source.clone(),
        ) else {
            return;
        };
        let _span = tracing::info_span!("terrain_pipelines_ground_look").entered();
        let lights = self.shared_shader_lights;
        let set = self.cached_pipeline_set(&source, &shader, format, false, lights);
        self.shared_color = Some(set.color);
        if self.shared_holes.is_some() {
            self.shared_holes = Some(self.cached_pipeline_set(&source, &shader, format, true, lights));
        }
        if self.vt.is_some() {
            self.vt = None;
            self.shared_color_vt = None;
            self.ensure_vt();
        }
    }

    /// Selects the shader for the configured cost view during prewarm as well as frame upload.
    pub(crate) fn set_cost_view(&mut self, view: u32) {
        let cost_view = cost_view_needs_full_shader(view);
        if cost_view != self.shared_cost_view {
            self.shared_cost_view = cost_view;
            self.rebuild_ground_look_variant();
        }
    }

    /// The shared shader with the ground look compiled in (the full [`shared_source`] for the
    /// light variant `lights`), once per device; `None` while every ground-look row is zero
    /// (the shared pipelines then run [`without_ground_look`]'s source). ADR-1452. The full
    /// source also holds the cost debug views, so a frame with one of them on runs it too
    /// (ADR-1491; the look's code draws nothing at zero rows).
    fn ground_look_shader(&self, lights: bool) -> Option<(String, wgpu::ShaderModule)> {
        let full = self.shared_ground_look || self.shared_cost_view;
        if !full && !self.shared_surface_weather && !self.shared_cloud_map {
            return None;
        }
        let mut source = shared_source(self.height_sampler.is_some(), lights);
        if !self.shared_surface_weather {
            source = without_surface_weather(&source);
        }
        if !self.shared_cloud_map {
            source = without_cloud_map(&source);
        }
        if !full {
            source = without_cost_views(&without_ground_look(&source));
        }
        let source = device_source(&self.device, self.footprint_source(source));
        let shader = self.shared_pipelines.get_or_create(
            regia_render::device_pipelines::pipeline_key(&("regia_terrain shared shader", &source)),
            || {
                self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("terrain.wgsl (shared, ground look)"),
                    source: regia_render::wgsl_source(&source),
                })
            },
        );
        Some((source, shader))
    }

    /// The ground look rows last set with [`Self::set_ground_look`] (all zero when off).
    #[must_use]
    pub fn ground_look_uniforms(&self) -> [[f32; 4]; 3] {
        [
            self.material_state.ground,
            self.material_state.winter,
            self.material_state.winter2,
        ]
    }

    /// Replaces the shoreline water plane (`None` removes it). The next
    /// [`TerrainRenderer::draw_water`] draws the new plane over the same map square.
    ///
    /// # Errors
    ///
    /// [`TerrainError::InvalidWater`] when the plane fails [`WaterPlane::validate`]; the old
    /// plane stays.
    pub fn set_water(&mut self, water: Option<&WaterPlane>) -> Result<(), TerrainError> {
        if let Some(water) = water {
            water.validate()?;
        }
        self.water_plane = water.map(|water| {
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("terrain water params"),
                size: size_of::<WaterUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            (water.clone(), buffer)
        });
        self.bind_water_scene(None);
        Ok(())
    }

    /// Whether a water plane is set ([`TerrainGpu::set_water`] or the constructor).
    pub fn has_water(&self) -> bool {
        self.water_bind.is_some()
    }

    /// Turns the water's screen-space refraction in [`render_offscreen`] on or off (on by
    /// default). Off, the water blends over the frame straight behind it, as a host that
    /// records [`TerrainRenderer::draw_water`] itself always gets.
    pub fn set_water_refraction(&mut self, on: bool) {
        self.water_refraction = on;
    }

    /// Whether [`render_offscreen`] refracts the frame through the water.
    pub fn water_refraction(&self) -> bool {
        self.water_refraction
    }

    /// Rebuilds the water bind group with the refraction sources `scene` (scene copy, scene
    /// depth view), or the stand-ins and refraction off for `None`, and rewrites the plane's
    /// uniform to match. No-op without a plane.
    fn bind_water_scene(&mut self, scene: Option<(&wgpu::TextureView, &wgpu::TextureView)>) {
        let Some((water, buffer)) = &self.water_plane else {
            self.water_bind = None;
            return;
        };
        let uniform = water_uniform(self.world_width, self.grid_origin, water, scene.is_some());
        self.queue.write_buffer(buffer, 0, bytemuck::bytes_of(&uniform));
        let sources = scene.unwrap_or((&self.water_scene_stand_in.0, &self.water_scene_stand_in.1));
        self.water_bind = Some(water_bind_group(
            &self.device,
            &self.water_layout,
            &self.height_view,
            &self.terrain_buffer,
            buffer,
            sources,
        ));
    }

    /// GPU bytes resident for this terrain: the height mirror, the material textures and the
    /// sim mirror (when installed). The budget test asserts against this.
    pub fn texture_bytes(&self) -> u64 {
        self.texture_bytes
    }

    /// Rebuilds the terrain data bind group after material textures changed.
    fn build_terrain_bind(&self) -> wgpu::BindGroup {
        self.material_textures.bind_group(
            &self.device,
            &self.terrain_layout,
            &self.height_view,
            &self.terrain_buffer,
            &self.layers_buffer,
            &self.scorch_buffer,
            &self.material_buffer,
            [&self.material_sampler, &self.material_aniso_sampler],
            &self.contact,
            self.height_sampler.as_ref(),
            &self.slope,
        )
    }

    /// Binds a scatter contact map ([`crate::ContactMap`]): the terrain colour pass darkens
    /// and tints the ground under scatter instances by its occlusion. `None` restores the
    /// neutral (no-contact) binding.
    pub fn set_contact(&mut self, contact: Option<&crate::scatter::contact::ContactMap>) {
        self.contact = match contact {
            Some(map) => ContactBinding {
                view: map.view().clone(),
                buffer: map.buffer().clone(),
                generation: Some(map.generation()),
            },
            None => ContactBinding::neutral(&self.device),
        };
        self.contact_binds += 1;
        self.terrain_bind = self.build_terrain_bind();
    }

    /// Changes whenever the bound contact map is rebound or re-rasterised.
    #[must_use]
    pub fn contact_key(&self) -> u64 {
        let generation = self
            .contact
            .generation
            .as_ref()
            .map_or(0, |g| g.load(std::sync::atomic::Ordering::Relaxed));
        generation ^ self.contact_binds.rotate_left(40)
    }

    /// Clears the sun cascade and draws scatter into it. No casters leave the map at
    /// the far clear depth, which the terrain shader treats as fully lit.
    ///
    /// The cascade is cached: it is re-rendered only when the snapped sun
    /// view-projection from the last [`TerrainRenderer::update_frame`] or the
    /// scatter casters changed since the last render. A held camera renders it once.
    pub fn encode_sun_shadow(&self, encoder: &mut wgpu::CommandEncoder) {
        {
            // A poisoned lock only means another thread panicked mid-update; the
            // cache is plain data, so render as if it were stale.
            let mut cache = self
                .sun_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let stamp = (cache.vp, self.scatter_generation);
            if cache.drawn == Some(stamp) {
                return;
            }
            cache.drawn = Some(stamp);
        }
        let _span = tracing::info_span!("terrain_sun_shadow").entered();
        self.sun_renders
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terrain sun shadow pass"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &self.shadow_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let Some(instances) = &self.instance_buffer else {
            return;
        };
        if self.instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.shadow_pipeline);
        pass.set_bind_group(0, &self.frame_bind, &[]);
        pass.set_bind_group(1, &self.scatter_bind, &[]);
        pass.set_vertex_buffer(0, instances.slice(..));
        pass.draw(0..24, 0..self.instance_count);
    }

    /// Sun cascade renders since construction (see [`Self::encode_sun_shadow`]).
    #[must_use]
    pub fn sun_shadow_renders(&self) -> u64 {
        self.sun_renders.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Writes and bytes of the last node upload. A held camera reports `(0, 0)`.
    #[must_use]
    pub fn last_node_upload(&self) -> (u32, u64) {
        self.node_upload_last
    }

    /// Hole-culled index lists the last node upload built: one per partially cut node not
    /// seen since the mask changed, so 0 for a held camera.
    #[must_use]
    pub fn last_hole_index_builds(&self) -> u32 {
        self.hole_index_builds_last
    }

    /// Nodes the hole mask removes entirely although no single shape covers them (they lie
    /// inside the union of several), among those selected since the mask last changed.
    #[must_use]
    pub fn hole_cut_node_count(&self) -> usize {
        self.hole_cut_nodes.len()
    }

    /// Scene-radiance output for the shared HDR target (`true`) or the sRGB offscreen path.
    pub fn set_linear_output(&mut self, linear: bool) {
        self.linear_output = linear;
    }

    /// Pre-exposure of the shared HDR target (`SharedFrameInfo::hdr_scale`): the linear
    /// output writes scene radiance × `scale`. Ignored on the sRGB offscreen path.
    pub fn set_hdr_scale(&mut self, scale: f32) {
        self.hdr_scale = if scale > 0.0 && scale.is_finite() {
            scale
        } else {
            1.0
        };
    }

    /// Motion-vector matrices. The first frame should pass the current view-projection as `prev`.
    pub fn set_motion_matrices(&mut self, prev: [[f32; 4]; 4], unjittered: [[f32; 4]; 4]) {
        self.motion_set = true;
        self.prev_view_proj = prev;
        self.unjittered_view_proj = unjittered;
    }

    /// Binds the engine cascade depth maps. Slot 0 is cascade 0; missing slots repeat the last view.
    pub fn set_engine_csm(&mut self, views: &[wgpu::TextureView]) {
        if views.is_empty() || self.engine_csm_views.as_slice() == views {
            return;
        }
        self.engine_csm_views = views.to_vec();
        self.rebuild_engine_csm_bind();
    }

    /// Binds the engine SSAO for the shared colour pass (`None`: no SSAO this frame). The
    /// sky and ground ambient of the shared path is multiplied by it, like the mesh path.
    pub fn set_engine_ao(&mut self, ao: Option<&wgpu::TextureView>) {
        let on = ao.is_some();
        match ao {
            Some(view) if !(self.engine_ao_on && self.engine_ao == *view) => {
                self.engine_ao = view.clone();
            }
            _ if on == self.engine_ao_on => return,
            _ => {}
        }
        self.engine_ao_on = on;
        self.rebuild_engine_csm_bind();
    }

    /// Binds the engine's screen-space contact shadows for the shared colour pass (`None`:
    /// none this frame). The sun term of the shared path is multiplied by them, like the
    /// mesh path; ambient light is untouched.
    pub fn set_engine_contact_shadows(&mut self, shadows: Option<&wgpu::TextureView>) {
        let on = shadows.is_some();
        match shadows {
            Some(view) if !(self.engine_contact_on && self.engine_contact == *view) => {
                self.engine_contact = view.clone();
            }
            _ if on == self.engine_contact_on => return,
            _ => {}
        }
        self.engine_contact_on = on;
        self.rebuild_engine_csm_bind();
    }

    /// Binds the engine's cloud transmittance map for the shared colour pass (ADR-320;
    /// `None`: no map this frame). The sun term of the shared path is multiplied by the
    /// cloud layer's transmittance along the sun, like the mesh path; ambient light is untouched.
    ///
    /// With a map the shared colour pipelines run the cloud reads, without one they compile
    /// them out ([`without_cloud_map`], ADR-7886): a change swaps the colour pipelines (the
    /// warm builds both, [`Self::warm_shared`] and the warm job).
    pub fn set_engine_cloud_shadow(&mut self, map: Option<(&wgpu::TextureView, [f32; 4])>) {
        self.engine_cloud_params = map.map_or([0.0; 4], |(_, params)| params);
        let cloud_map = self.engine_cloud_params[3] > 0.5;
        if cloud_map != self.shared_cloud_map {
            self.shared_cloud_map = cloud_map;
            self.rebuild_ground_look_variant();
        }
        let view = map.map_or(&self.engine_ao_dummy, |(view, _)| view);
        if *view == self.engine_cloud {
            return;
        }
        self.engine_cloud = view.clone();
        self.rebuild_engine_csm_bind();
    }

    /// Binds the shadow atlas with the precipitation occlusion tile for the shared colour pass
    /// (`None`: the frame has none; only the surface-weather variant reads it).
    pub fn set_engine_precip_occlusion(&mut self, atlas: Option<&wgpu::TextureView>) {
        if atlas == self.engine_precip.as_ref() {
            return;
        }
        self.engine_precip = atlas.cloned();
        self.rebuild_engine_csm_bind();
    }

    /// Binds the engine's sky irradiance map and split-sum BRDF table for the shared colour
    /// pass (`None`: the frame has none). With them the sky light and sky reflection are read
    /// per normal from the same data meshes are lit with, so a grey mesh and grey ground of
    /// the same orientation get the same sky light; without them the hemisphere model
    /// (`SkyAmbient`) stays.
    pub fn set_sky_irradiance(&mut self, sky: Option<(&wgpu::TextureView, &wgpu::TextureView)>) {
        let on = sky.is_some();
        let (irradiance, brdf) = sky.unwrap_or((&self.sky_irradiance_dummy, &self.sky_irradiance_dummy));
        if on == self.sky_irradiance_on && *irradiance == self.sky_irradiance && *brdf == self.sky_brdf {
            return;
        }
        self.sky_irradiance = irradiance.clone();
        self.sky_brdf = brdf.clone();
        self.sky_irradiance_on = on;
        self.frame_bind = frame_bind_group(
            &self.device,
            &self.frame_layout,
            &self.frame_buffer,
            &self.sky_irradiance,
            &self.sky_sampler,
            &self.sky_brdf,
            &self.occluder_buffer,
        );
    }

    fn rebuild_engine_csm_bind(&mut self) {
        let views = self.engine_csm_views.clone();
        if views.is_empty() {
            return;
        }
        let last = views.len() - 1;
        let view = |index: usize| {
            if index < views.len() {
                &views[index]
            } else {
                &views[last]
            }
        };
        self.engine_csm_bind = engine_csm_bind_group(
            &self.device,
            &self.engine_csm_layout,
            [view(0), view(1), view(2), view(3)],
            &self.shadow_sampler,
            &self.engine_csm_buffer,
            &self.engine_ao,
            &self.engine_contact,
            (&self.engine_gi.0, &self.engine_gi.1),
            self.local_lights
                .then_some((&self.engine_lights, &self.engine_light_profiles)),
            &self.engine_cloud,
            self.engine_precip.as_ref().unwrap_or(view(0)),
        );
    }

    /// Binds the engine's irradiance probes for the shared colour pass (`None`: probes off).
    /// Where they cover the ground they replace the sky term (with its canyon occlusion) and
    /// the sunlit-wall bounce, which they already trace.
    pub fn set_engine_gi(&mut self, gi: Option<(&wgpu::Buffer, &wgpu::Buffer)>) {
        let want = gi.map_or_else(|| self.engine_gi_dummy.clone(), |(u, d)| (u.clone(), d.clone()));
        if want == self.engine_gi {
            return;
        }
        self.engine_gi = want;
        self.rebuild_engine_csm_bind();
    }

    /// Binds the engine's clustered point and spot lights for the shared colour pass (`None`:
    /// none yet). The terrain then takes the light a mesh of its albedo and roughness takes
    /// (`regia_render::shared_local_lights_wgsl`), local-light shadows included. Ignored on
    /// a device without room for the bindings ([`Self::local_lights_supported`]).
    pub fn set_engine_lights(&mut self, lights: Option<&regia_render::EngineLights>) {
        if !self.local_lights {
            return;
        }
        let want = lights
            .filter(|_| self.local_lights_enabled)
            .unwrap_or(&self.engine_lights_dummy);
        if *want == self.engine_lights {
            return;
        }
        self.engine_lights = want.clone();
        self.rebuild_engine_csm_bind();
    }

    /// Binds the frame's emitter probe atlas, whose bottom bands hold the IES profiles of its point and spot lights
    /// (ADR-3247; `None`: the 1×1 stand-in, profiled lights unshaped). Ignored on a device without room for the
    /// light bindings ([`Self::local_lights_supported`]).
    pub fn set_engine_light_profiles(&mut self, atlas: Option<&wgpu::TextureView>) {
        if !self.local_lights {
            return;
        }
        let want = atlas
            .filter(|_| self.local_lights_enabled)
            .unwrap_or(&self.engine_ao_dummy);
        if *want == self.engine_light_profiles {
            return;
        }
        self.engine_light_profiles = want.clone();
        self.rebuild_engine_csm_bind();
    }

    /// Whether the terrain takes the frame's point and spot lights (default on where
    /// [`Self::local_lights_supported`]). Off, the shared colour pass is built from the shader
    /// without the light loop and binds the zeroed stand-ins: the ground shades, and costs, as
    /// before batch v020.
    ///
    /// Both variants are the device's and the warm builds both ([`Self::warm_shared`],
    /// `TerrainSharedDraw::prewarm`, [`crate::terrain_warm_job`]), so a change after the warm
    /// swaps pipelines here, at the call, and compiles nothing. Before any warm, set it before
    /// [`Self::ensure_shared`] so only the chosen variant is built.
    pub fn set_local_lights_enabled(&mut self, on: bool) {
        if self.local_lights_enabled == on {
            return;
        }
        self.local_lights_enabled = on;
        if !on && self.local_lights && self.engine_lights != self.engine_lights_dummy {
            self.engine_lights = self.engine_lights_dummy.clone();
            self.rebuild_engine_csm_bind();
        }
        self.rebuild_shared_variant();
    }

    /// Whether the shared colour pass's shader carries the light loop: the device has room for
    /// the bindings ([`Self::local_lights_supported`]) and the host has not turned them off
    /// ([`Self::set_local_lights_enabled`]).
    #[must_use]
    pub fn local_lights_compiled(&self) -> bool {
        self.local_lights && self.local_lights_enabled
    }

    /// Whether the built shared pipelines run the light loop (`None` before
    /// [`Self::ensure_shared`]).
    #[must_use]
    pub fn shared_pipelines_lit(&self) -> Option<bool> {
        self.shared_shader.as_ref().map(|_| self.shared_shader_lights)
    }

    /// Whether the shared colour pass lights the terrain with the engine's point and spot
    /// lights: the device's fragment stage has room for their bindings (four storage buffers
    /// and a uniform over the terrain's own) and their code composed.
    #[must_use]
    pub fn local_lights_supported(&self) -> bool {
        self.local_lights
    }

    /// Uploads an sRGB RGBA8 image as an `Rgba8UnormSrgb` texture (no mips).
    #[must_use]
    pub fn upload_srgb_image(&self, label: &str, image: &crate::material::Rgba8Image) -> wgpu::TextureView {
        let size = wgpu::Extent3d {
            width: image.width.max(1),
            height: image.height.max(1),
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        if image.data.len() as u64 == u64::from(size.width) * u64::from(size.height) * 4 {
            self.queue.write_texture(
                texture.as_image_copy(),
                &image.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.width * 4),
                    rows_per_image: Some(size.height),
                },
                size,
            );
        }
        texture.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// The height texture (`R32Float`, normalized heights, mips) the colour pass reads.
    #[must_use]
    pub fn height_view(&self) -> &wgpu::TextureView {
        &self.height_view
    }

    fn engine_csm_uniform(
        info: &regia_render::SharedFrameInfo,
        ao_on: bool,
        contact_on: bool,
        cloud: [f32; 4],
    ) -> EngineCsmUniform {
        let cascades = info.cascades;
        let bias = info.csm_bias;
        EngineCsmUniform {
            vp0: cascades[0],
            vp1: cascades[1],
            vp2: cascades[2],
            vp3: cascades[3],
            split: info.csm_splits,
            bias0: bias[0],
            bias1: bias[1],
            bias2: bias[2],
            bias3: bias[3],
            params: [info.cascade_count as f32, 0.0, 0.0, info.pcss_filter_taps as f32],
            screen: [
                info.width as f32,
                info.height as f32,
                if ao_on { 1.0 } else { 0.0 },
                if contact_on { 1.0 } else { 0.0 },
            ],
            cloud,
            origin: [
                info.origin_offset[0],
                info.origin_offset[1],
                info.origin_offset[2],
                0.0,
            ],
        }
    }

    /// Uploads cascade matrices, splits and the PCF tap count for [`Self::set_engine_csm`].
    pub fn upload_engine_csm(&self, info: &regia_render::SharedFrameInfo) {
        let uniform = Self::engine_csm_uniform(
            info,
            self.engine_ao_on,
            self.engine_contact_on,
            self.engine_cloud_params,
        );
        self.queue
            .write_buffer(&self.engine_csm_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    /// Uploads up to four cascade view-projections. Call before the shadow passes of the frame.
    pub fn upload_cascades(&self, cascades: &[[[f32; 4]; 4]; 4]) {
        for (buf, vp) in self.cascade_bufs.iter().zip(cascades.iter()) {
            let uniform = CascadeUniform {
                light_view_proj: *vp,
                morph_eye: [0.0; 4],
            };
            self.queue.write_buffer(buf, 0, bytemuck::bytes_of(&uniform));
        }
    }

    /// Shared-frame uploads through the renderer's staging belt: frame uniforms,
    /// engine CSM, cascade matrices and the changed node slots. Unchanged bytes
    /// are not written, so a held camera queues nothing.
    ///
    /// `lists` are the per-pass draw lists (indices into `selection`); pass `i` later draws
    /// with `list: i` in [`Self::draw_shared`].
    /// `shadow_eyes[i]` is the eye cascade `i`'s list was selected from (its morph centre);
    /// `None` morphs around the camera.
    #[allow(clippy::too_many_arguments)] // one frame's upload
    pub fn upload_shared(
        &mut self,
        frame: &TerrainFrame,
        info: &regia_render::SharedFrameInfo,
        selection: &LodSelection,
        lists: &[&[u32]],
        shadow_eyes: &[Option<[f32; 2]>; 4],
        upload: &mut regia_render::SharedUpload<'_>,
    ) {
        let _span = tracing::info_span!("terrain_upload").entered();
        self.set_cost_view(frame.debug_view);
        self.upload_surface_weather(info);
        // The frame's terrain texture filtering (ADR-192), for every host of the shared path
        // (a host wrapping `TerrainGpu` itself gets it without calling the setters).
        self.texture_lod_bias = info.terrain_texture_lod_bias;
        self.anisotropy = info.terrain_budget.anisotropy_level();
        let mut uniform = self.frame_uniform(frame);
        self.vt_frame_terms = [
            uniform.sun_dir_wetness[0],
            uniform.sun_dir_wetness[1],
            uniform.sun_dir_wetness[2],
            uniform.ground_ambient[3],
        ];
        // An LDR frame whose target does not encode sRGB in hardware (`Rgba8Unorm`): the
        // terrain writes its display-encoded colour as is, like the mesh path's
        // `encode_srgb`. Writing the linear value there decoded the image a second time
        // (everything ~2.2 gamma too dark).
        if !info.linear_hdr && !info.color_format.is_srgb() {
            uniform.output_mode[1] = 1.0;
        }
        upload.write_if_changed(
            &mut self.frame_bytes,
            &self.frame_buffer,
            bytemuck::bytes_of(&uniform),
        );
        // The proxies' radiance is physical: only the linear HDR frame takes them (an LDR
        // frame's ambient is pre-exposed).
        let occluders = if info.linear_hdr {
            info.mesh_occluders
        } else {
            regia_render::MeshOccluderBlock::default()
        };
        upload.write_if_changed(
            &mut self.occluder_bytes,
            &self.occluder_buffer,
            bytemuck::bytes_of(&occluders),
        );
        let csm = Self::engine_csm_uniform(
            info,
            self.engine_ao_on,
            self.engine_contact_on,
            self.engine_cloud_params,
        );
        upload.write_if_changed(
            &mut self.engine_csm_bytes,
            &self.engine_csm_buffer,
            bytemuck::bytes_of(&csm),
        );
        self.cascade_bytes.resize(self.cascade_bufs.len(), Vec::new());
        for (((buf, cache), vp), eye) in self
            .cascade_bufs
            .iter()
            .zip(self.cascade_bytes.iter_mut())
            .zip(info.cascades.iter())
            .zip(shadow_eyes.iter())
        {
            let uniform = CascadeUniform {
                light_view_proj: *vp,
                morph_eye: eye.map_or([0.0; 4], |e| [e[0], 0.0, e[1], 1.0]),
            };
            upload.write_if_changed(cache, buf, bytemuck::bytes_of(&uniform));
        }
        if let Some(tile) = info.precip_occlusion {
            let uniform = CascadeUniform {
                light_view_proj: tile.view_proj,
                morph_eye: [0.0; 4],
            };
            upload.write_if_changed(
                &mut self.cascade_bytes[PRECIP_SHADOW_SLOT as usize],
                &self.cascade_bufs[PRECIP_SHADOW_SLOT as usize],
                bytemuck::bytes_of(&uniform),
            );
        }
        self.stage_hole_states(selection);
        let ranges = self.stage_nodes(selection);
        let mut bytes = 0u64;
        for range in &ranges {
            upload.write(
                &self.node_buffer,
                range.start as u64,
                &self.node_cpu[range.clone()],
            );
            bytes += range.len() as u64;
        }
        self.ensure_hole_indices(selection);
        let mut writes = ranges.len() as u32;
        let slot_ranges = self.stage_lists(selection, lists);
        for range in &slot_ranges {
            upload.write(
                &self.slot_buffer,
                range.start as u64,
                bytemuck::cast_slice(&self.slot_cpu[range.start / 4..range.end / 4]),
            );
            bytes += range.len() as u64;
        }
        writes += slot_ranges.len() as u32;
        self.node_upload_last = (writes, bytes);
    }

    /// Re-uploads the engine-CSM uniform (cascades, splits, the engine-SSAO and contact-shadow
    /// flags) and the
    /// frame uniform's sky-irradiance scale after [`Self::set_engine_ao`] /
    /// [`Self::set_sky_irradiance`] changed since [`Self::upload_shared`]. Writes nothing
    /// when the bytes match.
    pub fn upload_engine_csm_shared(
        &mut self,
        info: &regia_render::SharedFrameInfo,
        upload: &mut regia_render::SharedUpload<'_>,
    ) {
        let csm = Self::engine_csm_uniform(
            info,
            self.engine_ao_on,
            self.engine_contact_on,
            self.engine_cloud_params,
        );
        upload.write_if_changed(
            &mut self.engine_csm_bytes,
            &self.engine_csm_buffer,
            bytemuck::bytes_of(&csm),
        );
        let scale = self.sky_irradiance_scale();
        if let Some(next) = patch_frame_irradiance(&mut self.frame_bytes, scale) {
            let at = (std::mem::offset_of!(FrameUniform, output_mode) + 12) as u64;
            upload.write(&self.frame_buffer, at, &next);
        }
    }

    /// `output_mode.w`: the irradiance map's scale when it is bound on a linear frame.
    fn sky_irradiance_scale(&self) -> f32 {
        if self.linear_output && self.sky_irradiance_on {
            regia_render::SKY_IBL_SCALE
        } else {
            0.0
        }
    }

    /// Texture mip bias (log2 units, ≤ 0). The shader scales the pixel footprint by `2^bias`,
    /// so layer, macro and micro-normal LODs, the heightfield normal level and the
    /// near-detail fades follow the output resolution while the frame is upscaled; 0 leaves
    /// every frame unchanged. The shared path ([`Self::upload_shared`]) sets it from
    /// `SharedFrameInfo::terrain_texture_lod_bias` every frame (TSR and TAAU, ADR-192).
    pub fn set_texture_lod_bias(&mut self, bias: f32) {
        self.texture_lod_bias = bias;
    }

    /// Anisotropic filtering level of the layer, triplanar and macro textures and the
    /// heightfield normal (`regia_render::TerrainBudget::anisotropy`: 0 or 1 = off, clamped
    /// to 16; ADR-192). The shared path ([`Self::upload_shared`]) sets it from
    /// `SharedFrameInfo::terrain_budget` every frame; the default is 16.
    pub fn set_anisotropy(&mut self, level: u8) {
        self.anisotropy = regia_render::TerrainBudget::with_anisotropy(level).anisotropy_level();
    }

    /// The anisotropic filtering level in use (0 = off; [`Self::set_anisotropy`]).
    #[must_use]
    pub fn anisotropy(&self) -> u8 {
        self.anisotropy
    }

    /// Take the anisotropic layer, triplanar and macro taps in the shader (up to the anisotropy
    /// level of trilinear taps along the footprint's long axis) instead of through the sampler's anisotropy
    /// (ADR-3957). [`Self::new`] turns it on where the driver drops the sampler's anisotropy
    /// for explicit-gradient samples of array textures (Mesa's Intel Vulkan driver,
    /// [`crate::anisotropy_footprint_for`]); a host or test may set it either way on any adapter.
    ///
    /// The shared pipelines compile the footprint loop only while it is on (ADR-7886): a change
    /// after [`Self::ensure_shared`] rebuilds them for the new variant (compiles, unless a warm
    /// already built that variant on this device). Set it before the warm or the first frame.
    pub fn set_anisotropy_footprint(&mut self, on: bool) {
        let changed = self.anisotropy_footprint != on;
        self.anisotropy_footprint = on;
        aniso_path::record(on);
        if let (true, Some(format)) = (changed, self.shared_format) {
            let _span = tracing::info_span!("terrain_pipelines_aniso_footprint").entered();
            self.shared_color = None;
            self.ensure_shared(format);
        }
    }

    /// Whether the shader takes the anisotropic taps itself ([`Self::set_anisotropy_footprint`]).
    #[must_use]
    pub fn anisotropy_footprint(&self) -> bool {
        self.anisotropy_footprint
    }

    /// How many mips softer than the footprint's own LOD the sampler path asks the anisotropic
    /// sampler to filter (ADR-5459; clamped to 0..=[`crate::MAX_ANISO_LOD_OFFSET`], non-finite
    /// values are 0). [`Self::new`] starts from what the device's own 16x sampler measures
    /// ([`crate::anisotropy_lod_offset_for_device`], ADR-5797): a quarter mip on Mesa's AMD Vulkan
    /// driver and on a Mali-G715, whose anisotropic filter is sharper across the short axis than the
    /// terrain was tuned on, and 0 on NVIDIA, Apple and lavapipe. The footprint path ignores it.
    pub fn set_anisotropy_lod_offset(&mut self, mips: f32) {
        let mips = if mips.is_finite() {
            mips.clamp(0.0, aniso_path::MAX_ANISO_LOD_OFFSET)
        } else {
            0.0
        };
        self.anisotropy_lod_offset = mips;
        aniso_path::record_lod_offset(mips);
    }

    /// The sampler path's LOD offset in mips ([`Self::set_anisotropy_lod_offset`]).
    #[must_use]
    pub fn anisotropy_lod_offset(&self) -> f32 {
        self.anisotropy_lod_offset
    }

    /// Shared HDR output only: write the engine's stochastic-SSR surface code
    /// (`regia_render::pack_surface_code`: roughness, the sky reflection's specular weight
    /// and its share of the pixel) into the alpha instead of the older fade-coded roughness,
    /// so glossy ground (ice, wet rock) gets traced reflections. Set it only for a frame whose
    /// `SharedFrameInfo::sssr_codes` is true; the older SSR pass reads the old alpha.
    pub fn set_sssr_codes(&mut self, on: bool) {
        self.sssr_codes = on;
    }

    /// Applies the frame's opt-in terrain shading cuts (`regia_render::TerrainBudget`).
    /// Hardware-filtered normals need a device that filters 32-bit floats; elsewhere that
    /// cut stays off. Writes the terrain uniform only when the budget changed.
    pub fn set_budget(&mut self, budget: &regia_render::TerrainBudget) {
        let mut flags = 0u32;
        if budget.hw_filtered_normals && self.height_sampler.is_some() {
            flags |= 1;
        }
        if budget.far_layers_m > 0 {
            flags |= 2;
        }
        if budget.vertex_normal_m > 0 {
            flags |= 4;
        }
        let info = [
            self.terrain_uniform.mip_info[0],
            flags as f32,
            budget.far_layers_m as f32,
            budget.vertex_normal_m as f32,
        ];
        if info != self.terrain_uniform.mip_info {
            self.terrain_uniform.mip_info = info;
            self.queue
                .write_buffer(&self.terrain_buffer, 0, bytemuck::bytes_of(&self.terrain_uniform));
        }
    }

    /// Frame uniform for `frame`. Records the sun cascade view-projection for the shadow cache.
    fn frame_uniform(&self, frame: &TerrainFrame) -> FrameUniform {
        // Sunny-day reference exposure (EV100 ≈ 14.5) matching regia_render's convention,
        // plus the frame's exposure compensation. Without the reference, 100 klx direct sun
        // blows past AgX to white.
        let compensation = frame.exposure_compensation_ev.clamp(-8.0, 8.0).exp2();
        // Shared HDR: the post stack multiplies by exposure. Pre-exposing here would
        // crush the terrain toward black. The offscreen sRGB path keeps the reference.
        let illuminance_exposure = if self.linear_output {
            frame.illuminance_lux
        } else {
            frame.illuminance_lux * REFERENCE_EXPOSURE * compensation
        };
        let ambient_scale = if self.linear_output { 1.0 } else { compensation };
        let hdr_scale = if self.linear_output { self.hdr_scale } else { 1.0 };
        let (light_view_proj, shadow_params) = sun_shadow_uniforms(frame);
        self.sun_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .vp = light_view_proj;
        let prev = if self.motion_set {
            self.prev_view_proj
        } else {
            frame.view_proj
        };
        let unjittered = if self.motion_set {
            self.unjittered_view_proj
        } else {
            frame.view_proj
        };

        FrameUniform {
            view_proj: frame.view_proj,
            camera_pos_time: [
                frame.camera_pos[0],
                frame.camera_pos[1],
                frame.camera_pos[2],
                frame.time_seconds,
            ],
            sun_dir_wetness: [
                frame.sun_dir[0],
                frame.sun_dir[1],
                frame.sun_dir[2],
                frame.wetness,
            ],
            sun_color_exposure: [
                frame.sun_color[0],
                frame.sun_color[1],
                frame.sun_color[2],
                illuminance_exposure,
            ],
            ambient_debug: [
                frame.ambient[0] * ambient_scale,
                frame.ambient[1] * ambient_scale,
                frame.ambient[2] * ambient_scale,
                frame.debug_view as f32,
            ],
            fog_params: frame.fog.params_vec4(),
            fog_color: frame.fog.color_vec4(),
            light_view_proj,
            shadow_params,
            output_mode: [
                if self.linear_output { 1.0 } else { 0.0 },
                0.0,
                if self.linear_output {
                    regia_render::SKY_IBL_SCALE
                } else {
                    0.0
                },
                self.sky_irradiance_scale(),
            ],
            prev_view_proj: prev,
            unjittered_view_proj: unjittered,
            ground_ambient: match self.ground_ambient {
                Some(g) => [
                    g[0] * ambient_scale,
                    g[1] * ambient_scale,
                    g[2] * ambient_scale,
                    1.0,
                ],
                None => [0.0; 4],
            },
            // w: 1 = on, 2 = on with the canyon-scale visibility only, 3 = that only in a mesh
            // occluder's sun shade (`FillCanyonScope::uniform_w`, ADR-8412).
            fill_dir: self.fill_light.map_or([0.0; 4], |[d, _]| {
                [d[0], d[1], d[2], self.fill_canyon_scope.uniform_w()]
            }),
            // w: the shared HDR target's pre-exposure (1 on the sRGB path).
            fill_color: self.fill_light.map_or([0.0, 0.0, 0.0, hdr_scale], |[_, c]| {
                // Same scale as the sun's illuminance (pre-exposed on the sRGB path).
                let k = if self.linear_output {
                    1.0
                } else {
                    REFERENCE_EXPOSURE * compensation
                };
                [c[0] * k, c[1] * k, c[2] * k, hdr_scale]
            }),
            extra_dir: {
                // Same scale as the sun's illuminance (pre-exposed on the sRGB path).
                let k = if self.linear_output {
                    1.0
                } else {
                    REFERENCE_EXPOSURE * compensation
                };
                let mut rows = self.extra_lights;
                for slot in 0..regia_render::MAX_EXTRA_DIRECTIONAL {
                    let [r, g, b, _] = rows[2 * slot + 1];
                    rows[2 * slot + 1] = [r * k, g * k, b * k, 0.0];
                }
                rows
            },
            texture_lod: [
                self.texture_lod_bias,
                if self.sssr_codes { 1.0 } else { 0.0 },
                f32::from(self.anisotropy),
                if self.anisotropy_footprint {
                    1.0
                } else if self.anisotropy_lod_offset > 0.0 {
                    -self.anisotropy_lod_offset
                } else {
                    0.0
                },
            ],
        }
    }

    /// The frame's extra directional lights (ADR-7254): rows as
    /// [`regia_render::SharedFrameInfo::extra_directional`] packs them (per slot the travel
    /// direction with `w` = 1 when on, then colour · lux). The colour pass shades each unshadowed,
    /// Lambert diffuse plus the GGX lobe, on the ground and on the grass. All zero turns them off.
    pub fn set_extra_lights(&mut self, rows: [[f32; 4]; 2 * regia_render::MAX_EXTRA_DIRECTIONAL]) {
        self.extra_lights = rows;
    }

    /// Planet-shine: a shadowless directional fill `[toward the body, colour · lux]` added
    /// as Lambert diffuse (`SharedFrameInfo::fill_light`). `None` turns it off.
    pub fn set_fill_light(&mut self, fill: Option<[[f32; 3]; 2]>) {
        self.fill_light = fill;
    }

    /// Opt-in (off by default): the planet-shine fill's visibility is the canyon-scale
    /// macro AO and the screen-space AO only, not the full indirect occlusion stack (the
    /// texture-scale layer ORM occlusion and the heightfield cavity). A shadowless fill from
    /// a small disc is not cosine-weighted sky light, and meshes take it without AO; with the
    /// stack, Europa's shadowed ground read 0.57–0.67× the path tracer (`lookdev dyn_light`,
    /// lane mesh-occlusion), with this 0.89×.
    ///
    /// `true` is [`crate::FillCanyonScope::MapWide`], `false` [`crate::FillCanyonScope::Off`];
    /// [`Self::set_fill_canyon_scope`] also has the scope near the mesh occluders only.
    pub fn set_fill_canyon_visibility(&mut self, on: bool) {
        self.set_fill_canyon_scope(if on {
            crate::FillCanyonScope::MapWide
        } else {
            crate::FillCanyonScope::Off
        });
    }

    /// Where the planet-shine fill takes the canyon-scale visibility (ADR-8412):
    /// [`crate::FillCanyonScope::Off`] (default) nowhere, [`crate::FillCanyonScope::MapWide`]
    /// everywhere (as [`Self::set_fill_canyon_visibility`]`(true)`), and
    /// [`crate::FillCanyonScope::NearOccluders`] only in the sun shade of the frame's mesh
    /// occluders (`SharedFrameInfo::mesh_occluders`, the hull boxes), weighted by
    /// [`crate::near_occluder_shade`], so the ground under and beside a hull takes the fill as
    /// a mesh does while trench and canyon floors elsewhere keep the full stack. Encoded in the
    /// fill uniform's `w` (no layout change); takes effect at the next frame.
    pub fn set_fill_canyon_scope(&mut self, scope: crate::FillCanyonScope) {
        self.fill_canyon_scope = scope;
    }

    /// The scope set with [`Self::set_fill_canyon_scope`].
    #[must_use]
    pub fn fill_canyon_scope(&self) -> crate::FillCanyonScope {
        self.fill_canyon_scope
    }

    /// Switches the ambient term to the hemisphere sky/ground model: a surface with normal
    /// `n` receives `ambient · (1 + n.y) / 2 + ground · (1 − n.y) / 2` (the view factors of
    /// an infinite sky above and an infinite ground plane below), where `ambient` is
    /// [`TerrainFrame::ambient`] and `ground` this colour, in the same units. `None` keeps
    /// the legacy single-colour sky-visibility ambient (the offscreen lab look).
    pub fn set_ground_ambient(&mut self, ground: Option<[f32; 3]>) {
        self.ground_ambient = ground;
    }

    /// Replaces the hole mask (tunnel mouths, open cuts; [`HolesMask`]). Nodes a hole covers
    /// are dropped, nodes it partly covers are drawn with the hole-cut pipelines in every
    /// pass (colour, depth prepass, velocity, decal receiver and each sun cascade), so the
    /// removed ground neither draws nor casts. `None` or an empty mask removes every hole.
    /// Takes effect on the next node upload.
    pub fn set_holes(&mut self, mask: Option<&HolesMask>) {
        // Compared before it is copied: a host may pass the same (raster-heavy) mask every frame.
        let mask = mask.filter(|m| !m.is_empty());
        if mask == self.mask.as_ref() {
            return;
        }
        let _span = tracing::info_span!("terrain_set_holes").entered();
        if let Some(m) = mask
            && m.shape_count() > crate::holes::MAX_HOLE_WINDOWS
        {
            tracing::warn!(
                target: "regia_terrain",
                shapes = m.shape_count(),
                "a holes mask with more than {} shapes: the draw cuts the first {}, picks and colliders cut them all",
                crate::holes::MAX_HOLE_WINDOWS,
                crate::holes::MAX_HOLE_WINDOWS
            );
        }
        self.mask = mask.cloned();
        self.upload_holes();
        if self.mask.is_some() && self.terrain_holes_pipeline.is_none() {
            self.terrain_holes_pipeline = Some(offscreen_holes_pipeline(
                &self.shared_pipelines,
                &self.device,
                &self.terrain_source,
                &self.terrain_pipeline_layout,
                &self.terrain_shader,
            ));
        }
        self.ensure_shared_holes();
        self.holes_generation += 1;
        self.rebuild_node_bind();
    }

    /// Sets the ground-lattice cut of the mesh-layer mouths (spec 26 § 2a.4 items 1-2, ADR-8470;
    /// [`crate::TerrainMeshLayers::lattice_hole_cut`]). An open named hole of the mask that the
    /// cut holds draws as lattice windows: the hole-cut pipelines discard a pixel whose pre-morph
    /// grid position lies on a cut lattice triangle, the node index cull keeps every node triangle
    /// over lattice ground, and cliff relief is off within two cells of the cut. `None` (the
    /// default) draws every hole from its coverage window, as before. Takes effect on the next
    /// node upload.
    pub fn set_hole_lattice(&mut self, lattice: Option<crate::holes::LatticeHoleCut>) {
        if lattice.as_ref() == self.hole_lattice.as_deref() {
            return;
        }
        let _span = tracing::info_span!("terrain_set_hole_lattice").entered();
        self.hole_lattice = lattice.map(std::sync::Arc::new);
        self.upload_holes();
        self.holes_generation += 1;
        self.rebuild_node_bind();
    }

    /// The lattice cut in use ([`Self::set_hole_lattice`]).
    #[must_use]
    pub fn hole_lattice(&self) -> Option<&crate::holes::LatticeHoleCut> {
        self.hole_lattice.as_deref()
    }

    /// Rebuilds the hole words (and the GI's coverage-only copy) and drops the node caches.
    fn upload_holes(&mut self) {
        self.hole_index_buffers.clear();
        self.hole_states.clear();
        self.hole_cut_nodes.clear();
        self.write_hole_words();
    }

    /// Builds `holes_buffer` (the hole windows, then the mouth bands) and the GI's coverage-only
    /// copy. The node caches are left alone.
    fn write_hole_words(&mut self) {
        self.gi_holes_buffer = None;
        let lattice = self.hole_lattice.as_deref();
        let mut words = match (self.mask.as_ref(), lattice) {
            (Some(mask), Some(cut)) => match mask.gpu_words_with_lattice(Some(cut)) {
                Ok(words) => {
                    let filled = mask.named.iter().any(|n| n.open && cut.hole(&n.id).is_some());
                    if filled {
                        self.gi_holes_buffer = Some(holes_storage(&self.device, Some(mask)));
                    }
                    words
                }
                Err(err) => {
                    tracing::warn!(
                        target: "regia_terrain",
                        "mesh-layer lattice windows not drawn ({err}); the mouths draw from their coverage windows"
                    );
                    mask.gpu_words()
                }
            },
            (mask, _) => mask.map_or_else(|| vec![0u32; 4], HolesMask::gpu_words),
        };
        self.band_positions_at = append_band_words(&mut words, self.mouth_bands.as_deref());
        self.holes_buffer = upload_bytes(
            &self.device,
            "terrain holes",
            bytemuck::cast_slice(&words),
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
    }

    /// Sets the open mouths' bands (spec 26 § 2a.4 step 6, ADR-8569): the zipper strip, skirt and
    /// throat of every open mouth, drawn by [`Self::draw_mouth_bands`] in every shared pass with
    /// the pass's own pipeline (no band pipeline). Until [`Self::write_band_positions`] places a
    /// pass, its band sits at the cook positions. `None` draws no band.
    pub fn set_mouth_bands(&mut self, bands: Option<std::sync::Arc<crate::mesh_layer::MouthBands>>) {
        let same = match (&bands, &self.mouth_bands) {
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b) || a == b,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        let _span = tracing::info_span!("terrain_set_mouth_bands").entered();
        self.mouth_bands = bands;
        self.write_hole_words();
        self.holes_generation += 1;
        self.rebuild_node_bind();
    }

    /// The bands in use ([`Self::set_mouth_bands`]).
    #[must_use]
    pub fn mouth_bands(&self) -> Option<&crate::mesh_layer::MouthBands> {
        self.mouth_bands.as_deref()
    }

    /// Queues band set `set`'s placed vertices (`set < BAND_PASSES`; one position per
    /// [`crate::MouthBands::vertices`] entry). A wrong length or set writes nothing.
    pub fn write_band_positions(
        &self,
        set: usize,
        positions: &[[f32; 3]],
        upload: &mut regia_render::SharedUpload<'_>,
    ) {
        let Some(bands) = self.mouth_bands.as_deref() else {
            return;
        };
        if set >= BAND_PASSES || positions.len() != bands.vertices.len() {
            return;
        }
        let stride = (bands.vertices.len() * 12) as u64;
        let set = self.band_camera as usize * BAND_PASSES + set;
        upload.write(
            &self.holes_buffer,
            self.band_positions_at + stride * set as u64,
            bytemuck::cast_slice(positions),
        );
    }

    /// The active camera's band slot ([`BAND_CAMERAS`]): 0 for the main camera, 1 while a
    /// texture view is drawn (ADR-8824).
    #[must_use]
    pub fn band_camera(&self) -> u32 {
        self.band_camera
    }

    /// Draws the mouth bands into one shared pass with that pass's pipeline (the colour variant
    /// in use: ground look, virtual texture, local lights, cloud map): the active camera's
    /// ([`Self::band_camera`]) set 0 for the camera passes and the precipitation occlusion tile
    /// (whose nodes are the camera's), `1 + index` for sun cascade `index`. Returns whether it
    /// drew. [`Self::ensure_shared`] must have run.
    pub fn draw_mouth_bands<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        kind: regia_render::SharedPass,
    ) -> bool {
        let Some(bands) = self.mouth_bands.as_deref() else {
            return false;
        };
        let set = match kind {
            regia_render::SharedPass::Shadow { index } if index < 4 => 1 + index,
            regia_render::SharedPass::Shadow {
                index: PRECIP_SHADOW_SLOT,
            } => 0,
            regia_render::SharedPass::Shadow { .. } => return false,
            _ => 0,
        };
        let corners = bands.corners.len() as u32;
        if corners == 0 {
            return false;
        }
        let set = self.band_camera * BAND_PASSES as u32 + set;
        let _span = tracing::trace_span!("terrain_draw_mouth_bands", set).entered();
        if self.bind_shared(pass, kind).is_none() {
            return false;
        }
        pass.set_bind_group(2, &self.node_bind, &[]);
        let first = BAND_VERTEX_BASE + set * corners;
        pass.draw(first..first + corners, 0..1);
        true
    }

    /// The hole words the GI trace reads: coverage windows only ([`HolesMask::gpu_words`]).
    #[must_use]
    pub fn gi_holes_buffer(&self) -> &wgpu::Buffer {
        self.gi_holes_buffer.as_ref().unwrap_or(&self.holes_buffer)
    }

    /// Rebuilds the node group over this camera's node and slot buffers and the current
    /// holes buffer.
    fn rebuild_node_bind(&mut self) {
        self.node_bind = node_bind_group(
            &self.device,
            &self.node_layout,
            &self.node_buffer,
            &self.slot_buffer,
            &self.holes_buffer,
        );
        self.node_bind_holes = self.holes_generation;
    }

    /// The hole mask in use ([`Self::set_holes`]).
    #[must_use]
    pub fn holes(&self) -> Option<&HolesMask> {
        self.mask.as_ref()
    }

    /// Bumped by every [`Self::set_holes`] that changed the mask.
    #[must_use]
    pub fn holes_generation(&self) -> u64 {
        self.holes_generation
    }

    /// The storage buffer `terrain.wgsl`'s `in_hole` reads ([`HolesMask::gpu_words`]; a zero
    /// count without holes). The irradiance probes read the same words.
    #[must_use]
    pub fn holes_buffer(&self) -> &wgpu::Buffer {
        &self.holes_buffer
    }

    /// Side of the quadtree square, metres: the longer of the map's X and Z extents
    /// (ADR-8369). A square map returns `world_width`, so node placement stays bit for bit.
    fn quad_width(&self) -> f32 {
        self.world_width.max(self.world_depth)
    }

    /// Sample count along the quadtree side ([`Self::quad_width`]): the map's columns, or its rows
    /// on a map deeper than it is wide, for [`crate::cdlod::quadtree_span`] (ADR-8421).
    fn quad_samples(&self) -> u32 {
        let axis = usize::from(self.world_depth > self.world_width);
        (self.terrain_uniform.sample_max[axis] as u32).saturating_add(1)
    }

    /// Fills [`Self::slot_holes`] for `selection` from [`Self::hole_states`], testing only the
    /// nodes not seen since the mask last changed.
    fn stage_hole_states(&mut self, selection: &LodSelection) {
        self.slot_holes.clear();
        let mut tests = 0u32;
        if let Some(mask) = &self.mask {
            let _span = tracing::info_span!("terrain_hole_states").entered();
            // A long flight over a large map visits many nodes: start over rather than grow.
            if self.hole_states.len() > HOLE_STATE_CACHE_MAX {
                self.hole_states.clear();
            }
            let (world, root, origin) = (self.quad_width(), self.root_level, self.grid_origin);
            let lattice = self.hole_lattice.as_deref();
            for draw in &selection.nodes {
                let state = *self.hole_states.entry(draw.node).or_insert_with(|| {
                    tests += 1;
                    node_hole_state_lattice_at(mask, lattice, draw.node, world, root, origin)
                });
                self.slot_holes.push(state);
            }
        }
        self.hole_tests_last = tests;
    }

    /// Nodes whose hole state the last staging computed: each node is tested once while the
    /// mask stays the same, so a held or slowly moving view tests none.
    #[must_use]
    pub fn last_hole_tests(&self) -> u32 {
        self.hole_tests_last
    }

    /// Builds a hole-culled index buffer for each newly selected, partially covered node.
    fn ensure_hole_indices(&mut self, selection: &LodSelection) {
        self.hole_index_builds_last = 0;
        if self.mask.is_none() {
            return;
        }
        let _span = tracing::info_span!("terrain_hole_indices").entered();
        for (i, draw) in selection.nodes.iter().enumerate() {
            // Ensure a hole-culled index buffer exists for partially covered nodes.
            if let Some(mask) = &self.mask
                && self.slot_holes.get(i) == Some(&NodeHoleState::Partial)
                && !self.hole_index_buffers.contains_key(&draw.node)
                && !self.hole_cut_nodes.contains(&draw.node)
            {
                let indices = build_node_indices_lattice_at(
                    mask,
                    self.hole_lattice.as_deref(),
                    draw.node,
                    self.quad_width(),
                    self.root_level,
                    self.config.grid_size,
                    self.grid_origin,
                );
                self.hole_index_builds_last += 1;
                if indices.is_empty() {
                    // Cut entirely by several shapes together: drawn as a fully covered node.
                    self.hole_cut_nodes.insert(draw.node);
                    continue;
                }
                let buffer = upload_bytes(
                    &self.device,
                    "terrain hole indices",
                    bytemuck::cast_slice(&indices),
                    wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                );
                self.hole_index_buffers.insert(draw.node, buffer);
            }
        }
    }

    /// Sets the camera XZ that nodes drawn with the fine cliff-relief grid are chosen around
    /// ([`CLIFF_DENSE_RADIUS_M`]); `None` (the default) draws every node with the regular
    /// grid. [`crate::TerrainSharedDraw`] sets it from each frame's eye.
    pub fn set_detail_eye(&mut self, eye_xz: Option<[f32; 2]>) {
        self.detail_eye = eye_xz;
    }

    /// Nodes of the last staged selection drawn with the fine grid.
    #[must_use]
    pub fn dense_node_count(&self) -> usize {
        self.dense_cpu.iter().filter(|d| **d).count()
    }

    /// Fills [`Self::dense_cpu`] for `selection`, locking the steep cache once.
    fn fill_dense_flags(&mut self, selection: &LodSelection) {
        let n = selection.nodes.len();
        let active = self.detail_eye.is_some()
            && self.relief_cliff > 0.0
            && self.material_state.params0[0] >= 0.5
            && self.config.grid_size * 2 <= 256;
        if !active {
            self.dense_cpu.clear();
            self.dense_cpu.resize(n, false);
            return;
        }
        let Some(eye) = self.detail_eye else {
            return;
        };
        let mips = self.height_mips.snapshot();
        let samples = self.quad_samples();
        let query = DenseNodeQuery {
            mips: &mips,
            // The eye in the map's own frame: node rects and the slope query are map-local.
            eye: [eye[0] - self.grid_origin[0], eye[1] - self.grid_origin[1]],
            world_width: self.quad_width(),
            span: crate::cdlod::quadtree_span(
                self.quad_width(),
                samples,
                self.config.grid_size,
                self.root_level,
            ),
            root_level: self.root_level,
            grid_size: self.config.grid_size,
        };
        let mut grows = 0u64;
        let mut locks = 0u64;
        let (dense_cpu, steep_nodes, slot_holes) = (&mut self.dense_cpu, &self.steep_nodes, &self.slot_holes);
        stage_dense_with_cache(
            dense_cpu,
            &mut grows,
            &mut locks,
            steep_nodes,
            n,
            |cache, index| {
                let node = selection.nodes[index].node;
                let hole = slot_holes.get(index).copied().unwrap_or(NodeHoleState::Empty);
                node_is_dense(cache, &query, node, hole)
            },
        );
        let _ = (grows, locks);
    }

    /// Writes this selection's node uniforms into the CPU mirror and returns the
    /// byte ranges that changed, merged across adjacent slots. Grows the buffer
    /// (and rebinds it) when the selection does not fit.
    fn stage_nodes(&mut self, selection: &LodSelection) -> Vec<std::ops::Range<usize>> {
        let needed = selection.nodes.len().max(1) as u32;
        if needed > self.node_capacity {
            let _span = tracing::info_span!("terrain_node_grow").entered();
            let capacity = needed.next_power_of_two();
            self.node_buffer = make_node_storage(
                &self.device,
                "terrain nodes",
                u64::from(self.node_stride) * u64::from(capacity),
            );
            self.node_capacity = capacity;
            // A new buffer is zero-filled, and so is the mirror: every live slot differs.
            self.node_cpu = vec![0; capacity as usize * self.node_stride as usize];
            self.rebuild_node_bind();
        }
        let stride = self.node_stride as usize;
        let size = std::mem::size_of::<NodeUniform>();
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        self.fill_dense_flags(selection);
        for (i, draw) in selection.nodes.iter().enumerate() {
            let samples = self.quad_samples();
            let span = crate::cdlod::quadtree_span(
                self.quad_width(),
                samples,
                self.config.grid_size,
                self.root_level,
            );
            let (min_x, min_z, rect) = draw.node.world_rect_at(span, self.root_level, self.grid_origin);
            let grid = if self.dense_cpu[i] {
                self.config.grid_size * 2
            } else {
                self.config.grid_size
            };
            let uniform = NodeUniform {
                rect_min: [min_x, min_z],
                rect_size: rect,
                level: draw.node.level as f32,
                morph_start: draw.morph_start,
                morph_end: draw.morph_end,
                grid_size: grid as f32,
                root_level: self.root_level as f32,
            };
            let bytes = bytemuck::bytes_of(&uniform);
            let start = i * stride;
            let slot = &mut self.node_cpu[start..start + size];
            if slot == bytes {
                continue;
            }
            slot.copy_from_slice(bytes);
            match ranges.last_mut() {
                // The previous slot was dirty too: extend over the padding.
                Some(last) if last.end + (stride - size) == start => last.end = start + size,
                _ => ranges.push(start..start + size),
            }
        }
        ranges
    }

    /// Builds the depth, cascade-shadow, HDR-colour, velocity and decal-receiver pipelines for `format`.
    ///
    /// The offscreen `Rgba8UnormSrgb` pipelines are left alone, so terrain-lab goldens stay.
    pub fn ensure_shared(&mut self, format: wgpu::TextureFormat) {
        if self.shared_format == Some(format) && self.shared_color.is_some() {
            self.ensure_shared_holes();
            return;
        }
        // Four pipeline compiles: a frame-time spike if the first frame pays it.
        // `TerrainSharedDraw::prewarm` moves this to load time.
        let _span = tracing::info_span!("terrain_pipelines").entered();
        let (source, shader, lights) = self.shared_variant(self.local_lights_compiled());
        let set = self.cached_pipeline_set(&source, &shader, format, false, lights);
        self.shared_source = Some(source);
        self.shared_depth = Some(set.depth);
        self.shared_color = Some(set.color);
        self.shared_shadow = set.shadow;
        self.shared_velocity = Some(set.velocity);
        self.shared_receiver = Some(set.receiver);
        self.shared_holes = None;
        self.shared_shader = Some(shader);
        self.shared_shader_lights = lights;
        self.shared_format = Some(format);
        if !self.shared_formats_seen.contains(&format) {
            self.shared_formats_seen.push(format);
        }
        self.vt = None;
        self.shared_color_vt = None;
        self.ensure_shared_holes();
        self.ensure_vt();
    }

    /// `source` as the shared colour pipelines compile it for this terrain's footprint path:
    /// unchanged with the path on, [`without_aniso_footprint`] with it off (ADR-7886).
    fn footprint_source(&self, source: String) -> String {
        if self.anisotropy_footprint {
            source
        } else {
            without_aniso_footprint(&source)
        }
    }

    /// The colour format the shared pipelines were last built for ([`Self::ensure_shared`]).
    #[must_use]
    pub fn shared_format(&self) -> Option<wgpu::TextureFormat> {
        self.shared_format
    }

    /// The shared shader's source and module with the light loop (`lights`, where the device
    /// has room for it: [`Self::local_lights_supported`]) or without it, and which one that
    /// is. Without it the `engine_local_lights` stub stays and the colour pass compiles exactly
    /// the v0.1 code. Once per device ([`regia_render::RenderDevice::shared_pipelines`]).
    /// The ground look's code is left out ([`without_ground_look`], ADR-1452), and so are the
    /// cost debug views ([`without_cost_views`], ADR-1491): the colour pipelines of a terrain
    /// with the look or a cost view on come from [`Self::ground_look_shader`].
    fn shared_variant(&self, lights: bool) -> (String, wgpu::ShaderModule, bool) {
        let lights = lights && self.local_lights;
        let source = without_cloud_map(&without_surface_weather(&without_cost_views(
            &without_ground_look(&shared_source(self.height_sampler.is_some(), lights)),
        )));
        let source = device_source(&self.device, self.footprint_source(source));
        // Once per device: every terrain (and the warm job's) shares the module and the sets.
        let shader = self.shared_pipelines.get_or_create(
            regia_render::device_pipelines::pipeline_key(&("regia_terrain shared shader", &source)),
            || {
                self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(if lights {
                        "terrain.wgsl (shared, local lights)"
                    } else {
                        "terrain.wgsl (shared)"
                    }),
                    // Texture parameters (`tex_height_at`) specialised away: Mali crashes on them.
                    source: regia_render::wgsl_source(&source),
                })
            },
        );
        (source, shader, lights)
    }

    /// Swaps the shared pipelines that run the shared shader (the five, the hole-cut set and
    /// the virtual texture's colour pipeline) to the light variant the host now wants, when it
    /// differs from the built one. The sets are the device's: after [`Self::warm_shared`] or
    /// [`crate::terrain_warm_job`] this compiles nothing. The virtual texture's atlas and page
    /// cache stay.
    fn rebuild_shared_variant(&mut self) {
        let Some(format) = self.shared_format else {
            return;
        };
        if self.shared_shader.is_none() || self.shared_shader_lights == self.local_lights_compiled() {
            return;
        }
        let _span = tracing::info_span!("terrain_pipelines_variant").entered();
        let (source, shader, lights) = self.shared_variant(self.local_lights_compiled());
        let set = self.cached_pipeline_set(&source, &shader, format, false, lights);
        self.shared_depth = Some(set.depth);
        self.shared_color = Some(set.color);
        self.shared_shadow = set.shadow;
        self.shared_velocity = Some(set.velocity);
        self.shared_receiver = Some(set.receiver);
        let ground = self.ground_look_shader(lights);
        let (vt_source, vt_shader) = ground
            .as_ref()
            .map_or((source.as_str(), &shader), |(s, m)| (s.as_str(), m));
        self.shared_color_vt = self
            .vt
            .as_ref()
            .map(|vt| self.cached_vt_color_pipeline(vt_source, vt_shader, format, vt.sample_layout()));
        self.shared_holes = None;
        self.shared_shader = Some(shader);
        self.shared_source = Some(source);
        self.shared_shader_lights = lights;
        self.ensure_shared_holes();
    }

    /// The shared sets (plain and hole-cut) and, with the virtual texture on, its colour
    /// pipeline, for the light variant the terrain is *not* drawing with, for the format
    /// [`Self::ensure_shared`] last built (a no-op before it, or on a device without room for
    /// the lights: one variant only). A host that turns the lights off or on after the warm
    /// ([`Self::set_local_lights_enabled`]) then compiles nothing. One step of
    /// [`Self::warm_shared`] and of the warm job.
    pub(crate) fn warm_other_light_variant(&self, holes: bool) {
        let Some(format) = self.shared_format else {
            return;
        };
        if self.shared_shader.is_none() || !self.local_lights {
            return;
        }
        let (source, shader, lights) = self.shared_variant(!self.shared_shader_lights);
        let _ = self.cached_pipeline_set(&source, &shader, format, holes, lights);
        if let (false, Some(vt)) = (holes, &self.vt) {
            let ground = self.ground_look_shader(lights);
            let (vt_source, vt_shader) = ground
                .as_ref()
                .map_or((source.as_str(), &shader), |(s, m)| (s.as_str(), m));
            let _ = self.cached_vt_color_pipeline(vt_source, vt_shader, format, vt.sample_layout());
        }
    }

    /// Turns the terrain virtual texture on (`Some`) or off. The atlas and pipelines are
    /// built with the shared pipelines ([`Self::ensure_shared`], or at once when they exist).
    /// A device with `Features::SHADER_EARLY_DEPTH_TEST` runs any settings. A device without
    /// it ([`vt::VtDepthTest::Late`]: DX12 and Metal) draws the texture when
    /// `late_depth_fallback` is set, and keeps the setting and shades every pixel in full
    /// when it is not. A device that cannot bind the texture
    /// ([`vt::VtDepthTest::Unsupported`]) also keeps the setting and shades in full.
    ///
    /// # Errors
    ///
    /// [`TerrainError::VirtualTexture`] when the settings are invalid or do not fit the map
    /// or the device; the texture stays off.
    ///
    /// Calling it makes the choice the host's: the render profile's
    /// `TerrainBudget::virtual_texture` ([`Self::set_virtual_texture_from_profile`]) stops
    /// driving the texture (ADR-157 addendum v0.1.2).
    pub fn set_virtual_texture(
        &mut self,
        settings: Option<vt::VirtualTextureSettings>,
    ) -> Result<(), TerrainError> {
        self.vt_explicit = true;
        self.apply_virtual_texture(settings)
    }

    /// The render profile's `TerrainBudget::virtual_texture` (read from the frame by the shared
    /// draw): turns the texture on with [`vt::VirtualTextureSettings::for_profile`] or off, unless
    /// the host chose through [`Self::set_virtual_texture`]. A device that cannot run it keeps
    /// it off (warned once per change).
    pub fn set_virtual_texture_from_profile(&mut self, want: bool) {
        self.set_virtual_texture_from_budget(&regia_render::TerrainBudget {
            virtual_texture: want,
            ..regia_render::TerrainBudget::OFF
        });
    }

    /// [`Self::set_virtual_texture_from_profile`] with the budget's
    /// `TerrainBudget::vt_late_depth` (ADR-2122): with it the profile's settings carry
    /// [`vt::VirtualTextureSettings::late_depth_fallback`], so a device without forced early
    /// depth tests (DX12, Metal) runs the texture too; without it such a device keeps it off.
    pub fn set_virtual_texture_from_budget(&mut self, budget: &regia_render::TerrainBudget) {
        let want = (
            budget.virtual_texture,
            budget.virtual_texture && budget.vt_late_depth,
        );
        if self.vt_explicit || want == self.vt_profile {
            return;
        }
        self.vt_profile = want;
        let settings = want.0.then(|| vt::VirtualTextureSettings {
            late_depth_fallback: want.1,
            ..vt::VirtualTextureSettings::for_profile()
        });
        if let Err(err) = self.apply_virtual_texture(settings) {
            tracing::warn!(target: "regia_terrain", "terrain virtual texture from the render profile stays off: {err}");
        } else if want.0 && !self.vt_settings.is_some_and(|s| self.vt_runs(&s)) {
            tracing::info!(target: "regia_terrain", "terrain virtual texture requested by the render profile; this device cannot run it (depth test: {}), so the terrain shades in full", self.vt_depth_test().name());
        }
    }

    fn apply_virtual_texture(
        &mut self,
        settings: Option<vt::VirtualTextureSettings>,
    ) -> Result<(), TerrainError> {
        if settings == self.vt_settings {
            return Ok(());
        }
        if let Some(s) = &settings {
            s.validate()?;
            vt::VtLayout::new(self.quad_width(), s)?;
        }
        self.vt_settings = settings;
        self.vt = None;
        self.shared_color_vt = None;
        self.ensure_vt();
        if self.vt_settings.is_some_and(|s| self.vt_runs(&s))
            && self.shared_shader.is_some()
            && self.vt.is_none()
        {
            self.vt_settings = None;
            self.vt_handle.set_settings(None);
            return Err(TerrainError::VirtualTexture(
                "the atlas or its pipelines could not be built on this device".into(),
            ));
        }
        self.vt_handle.set_settings(self.vt_settings);
        Ok(())
    }

    /// The virtual-texture settings in effect.
    #[must_use]
    pub fn virtual_texture(&self) -> Option<vt::VirtualTextureSettings> {
        self.vt_settings
    }

    /// Whether the device runs the virtual texture with forced early depth tests and a fifth
    /// bind group (the VT colour atlas binds at `@group(4)`): any settings run. A device
    /// without forced early depth tests (DX12, Metal) runs only settings with
    /// [`vt::VirtualTextureSettings::late_depth_fallback`] ([`Self::vt_depth_test`], ADR-2122).
    #[must_use]
    pub fn vt_supported(&self) -> bool {
        self.vt_depth_test().supports(self.vt_settings)
    }

    /// How this device tests depth for the virtual texture's colour pass (ADR-2122).
    #[must_use]
    pub fn vt_depth_test(&self) -> vt::VtDepthTest {
        vt::VtDepthTest::of_device(&self.device.limits(), self.device.features())
    }

    /// Whether `settings` run on this device ([`vt::VtDepthTest::runs`]).
    fn vt_runs(&self, settings: &vt::VirtualTextureSettings) -> bool {
        self.vt_depth_test().runs(settings)
    }

    /// Whether the virtual texture is built and drawing this frame.
    #[must_use]
    pub fn vt_active(&self) -> bool {
        self.vt.is_some() && self.shared_color_vt.is_some()
    }

    /// Counters of the virtual texture's last frame (`None` when it is off).
    #[must_use]
    pub fn vt_stats(&self) -> Option<vt::VtStats> {
        self.vt.as_ref().map(vt::VtGpu::stats)
    }

    /// The virtual texture's page grid (`None` when it is off).
    #[must_use]
    pub fn vt_layout(&self) -> Option<vt::VtLayout> {
        self.vt.as_ref().map(vt::VtGpu::layout)
    }

    /// Diagnostics of the virtual texture that survive boxing the draw: page grid, resident
    /// pages, page readback ([`vt::TerrainVtHandle`]). Shared by every rebuild of the texture.
    #[must_use]
    pub fn vt_handle(&self) -> vt::TerrainVtHandle {
        self.vt_handle.clone()
    }

    /// What height edits invalidated in the virtual texture since the draw was made (ADR-7585):
    /// each edit's own grown rectangle marks only the resident pages it meets.
    #[must_use]
    pub fn vt_edit_counters(&self) -> VtEditCounters {
        *self
            .vt_edit_counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queues the world rectangle of a height edit for the virtual texture, grown by
    /// [`vt::HEIGHT_REACH_M`] and one sample.
    fn note_height_edit(&self, map: &TerrainMap, dirty: &DirtyRect) {
        let [x0, z0] = map.sample_to_world(dirty.col_start, dirty.row_start);
        let [x1, z1] = map.sample_to_world(dirty.col_end - 1, dirty.row_end - 1);
        let grow = vt::HEIGHT_REACH_M + map.world_width() / map.width().max(1) as f32;
        let rect = [
            x0.min(x1) - grow,
            z0.min(z1) - grow,
            x0.max(x1) + grow,
            z0.max(z1) + grow,
        ];
        let mut list = self
            .vt_height_dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        list.push(rect);
        if list.len() > VT_EDIT_RECTS_MAX {
            self.vt_edit_counters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .merges += 1;
            let merged = list
                .iter()
                .fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |a, r| {
                    [a[0].min(r[0]), a[1].min(r[1]), a[2].max(r[2]), a[3].max(r[3])]
                });
            list.clear();
            list.push(merged);
        }
    }

    /// The map's altitude range the layer uniform carries.
    fn altitude_range(&self) -> [f32; 2] {
        *self
            .altitude_range
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes the edited map's altitude range: rewrites the layer uniform when it moved, and
    /// drops the virtual texture's pages when a layer's altitude band starts or stops reaching
    /// the lowest or highest ground (the only way the range enters the shading,
    /// [`altitude_reach`]); a range that moves without that keeps every page.
    fn set_altitude_range(&self, range: [f32; 2]) {
        let range = if range[0].is_finite() && range[1].is_finite() {
            range
        } else {
            [0.0, 0.0]
        };
        let old = {
            let mut held = self
                .altitude_range
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *held, range)
        };
        if old.map(f32::to_bits) == range.map(f32::to_bits) {
            return;
        }
        if altitude_reach(&self.layers_desc, old) != altitude_reach(&self.layers_desc, range) {
            self.bump_surface();
        }
        self.write_layers_uniform();
    }

    fn bump_surface(&self) {
        self.surface_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Builds the virtual texture and its colour pipeline once the shared shader exists, from
    /// the shader with the ground look while it is on (ADR-1452).
    fn ensure_vt(&mut self) {
        let (Some(settings), Some(shader), Some(format)) =
            (self.vt_settings, &self.shared_shader, self.shared_format)
        else {
            return;
        };
        if self.vt.is_some() || !self.vt_runs(&settings) {
            return;
        }
        let _span = tracing::info_span!("terrain_vt_pipelines").entered();
        let ground = self.ground_look_shader(self.shared_shader_lights);
        let (source, shader) = match &ground {
            Some((source, shader)) => (source.as_str(), shader),
            None => (self.shared_source.as_deref().unwrap_or_default(), shader),
        };
        let built = vt::VtGpu::new(&vt::VtGpuInit {
            device: &self.device,
            queue: &self.queue,
            settings,
            world_width: self.quad_width(),
            grid_origin: self.grid_origin,
            root_level: self.root_level,
            frame_layout: &self.frame_layout,
            terrain_layout: &self.terrain_layout,
            shader,
            shader_source: source,
            shared_pipelines: &self.shared_pipelines,
            handle: self.vt_handle.clone(),
        });
        match built {
            Ok(gpu) => {
                self.shared_color_vt =
                    Some(self.cached_vt_color_pipeline(source, shader, format, gpu.sample_layout()));
                self.vt = Some(gpu);
            }
            Err(err) => tracing::warn!(target: "regia_terrain::vt", "virtual texture off: {err}"),
        }
    }

    /// [`Self::vt_color_pipeline`] once per device, shader source, format and depth format
    /// (the group 4 layout's entries are fixed, and wgpu matches layouts by their entries).
    fn cached_vt_color_pipeline(
        &self,
        source: &str,
        shader: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        vt_layout: &wgpu::BindGroupLayout,
    ) -> wgpu::RenderPipeline {
        let key = regia_render::device_pipelines::pipeline_key(&(
            "regia_terrain shared vt color",
            source,
            format,
            self.depth_format,
        ));
        self.shared_pipelines
            .get_or_create(key, || self.vt_color_pipeline(shader, format, vt_layout))
    }

    /// The shared colour pipeline with `fs_shared_vt` and the virtual texture at group 4.
    fn vt_color_pipeline(
        &self,
        shader: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        vt_layout: &wgpu::BindGroupLayout,
    ) -> wgpu::RenderPipeline {
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("terrain shared color (vt) layout"),
                bind_group_layouts: &[
                    Some(&self.frame_layout),
                    Some(&self.terrain_layout),
                    Some(&self.node_layout),
                    Some(&self.engine_csm_layout),
                    Some(vt_layout),
                ],
                immediate_size: 0,
            });
        self.device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("terrain shared color (vt)"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_shared_vt"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(wgpu::Face::Back),
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: self.depth_format,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
            })
    }

    /// Hash of everything the cached surface depends on besides the camera. The sun enters
    /// through [`vt_key_sun`] (a turn under [`VT_SUN_TOLERANCE_RAD`] keeps the pages); the
    /// sun's colour and illuminance, the sky and ground ambient and the planet-shine fill are
    /// not baked (the colour pass applies them per pixel), so they never flush.
    fn vt_content_key(&mut self) -> u64 {
        use std::hash::{Hash, Hasher};
        let sun = [
            self.vt_frame_terms[0],
            self.vt_frame_terms[1],
            self.vt_frame_terms[2],
        ];
        let held = vt_key_sun(self.vt_key_sun, sun);
        self.vt_key_sun = Some(held);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytemuck::bytes_of(&self.material_state).hash(&mut h);
        bytemuck::bytes_of(&self.terrain_uniform).hash(&mut h);
        bytemuck::cast_slice::<f32, u8>(&held).hash(&mut h);
        self.vt_frame_terms[3].to_bits().hash(&mut h);
        // The ground-bounce share of the baked bounce reads it (`output_mode.w`).
        self.sky_irradiance_scale().to_bits().hash(&mut h);
        self.surface_generation
            .load(std::sync::atomic::Ordering::Relaxed)
            .hash(&mut h);
        self.root_level.hash(&mut h);
        h.finish()
    }

    /// The virtual texture's work for this frame: readback, page plan, uploads. `node_map`
    /// yields the camera selection as `(node, node-buffer slot)` when it changed; its first
    /// slot is `node_base`.
    pub(crate) fn prepare_vt(
        &mut self,
        eye: [f32; 3],
        selection_key: u64,
        node_base: u32,
        node_map: Option<&mut dyn Iterator<Item = (QuadNode, u32)>>,
        upload: &mut regia_render::SharedUpload<'_>,
    ) {
        let key = self.vt_content_key();
        let terms = vt::DistTerms::from_shader(
            self.material_state.params1,
            self.material_state.relief,
            self.material_state.rock,
            self.terrain_uniform.mip_info,
            self.vt_frame_terms[3] > 0.5,
        );
        let altitude = self.altitude_range();
        let height_edits = std::mem::take(
            &mut *self
                .vt_height_dirty
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let Some(vt) = self.vt.as_mut() else {
            return;
        };
        if !height_edits.is_empty() {
            let mut counters = self
                .vt_edit_counters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for r in height_edits {
                counters.rects += 1;
                counters.pages += u64::from(vt.invalidate_rect([r[0], r[1]], [r[2], r[3]]));
                counters.last_rect = Some(r);
            }
        }
        let changed = match node_map {
            Some(nodes) => vt.build_node_map(selection_key, node_base, nodes),
            None => false,
        };
        vt.prepare(
            &self.device,
            &self.queue,
            &self.node_buffer,
            changed,
            key,
            eye,
            altitude,
            &terms,
            upload,
        );
    }

    /// Records the virtual texture's page-table writes and bakes (before the frame's passes).
    pub fn encode_vt_before(&self, encoder: &mut wgpu::CommandEncoder) {
        if let (Some(vt), true) = (&self.vt, self.vt_active()) {
            vt.encode_before(encoder, &self.frame_bind, &self.terrain_bind);
        }
    }

    /// Records the virtual texture's feedback reduction (after the shared colour pass).
    pub fn encode_vt_after(&self, encoder: &mut wgpu::CommandEncoder) {
        if let (Some(vt), true) = (&self.vt, self.vt_active()) {
            vt.encode_after(encoder);
        }
    }

    /// Builds the hole-cut shared pipelines ([`SharedPipelines`] with the `*_holes` entry
    /// points) once a hole mask is set and the shared pipelines exist.
    fn ensure_shared_holes(&mut self) {
        if self.shared_holes.is_some() || self.mask.as_ref().is_none_or(HolesMask::is_empty) {
            return;
        }
        let (Some(shader), Some(format)) = (&self.shared_shader, self.shared_format) else {
            return;
        };
        let _span = tracing::info_span!("terrain_hole_pipelines").entered();
        let Some(source) = self.shared_source.as_deref() else {
            return;
        };
        let set = self.cached_pipeline_set(source, shader, format, true, self.shared_shader_lights);
        self.shared_holes = Some(set);
    }

    /// [`Self::shared_pipeline_set`] once per device, shader source, format and depth format
    /// ([`regia_render::RenderDevice::shared_pipelines`]). With the ground look on, the colour
    /// pipeline is [`Self::ground_look_shader`]'s for the light variant `lights` (ADR-1452).
    fn cached_pipeline_set(
        &self,
        source: &str,
        shader: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        holes: bool,
        lights: bool,
    ) -> SharedPipelines {
        let key = regia_render::device_pipelines::pipeline_key(&(
            "regia_terrain shared set",
            source,
            format,
            self.depth_format,
            holes,
        ));
        let mut set = self
            .shared_pipelines
            .get_or_create(key, || self.shared_pipeline_set(shader, format, holes));
        if let Some((ground_source, ground_shader)) = self.ground_look_shader(lights) {
            set.color = self.cached_ground_look_color(&ground_source, &ground_shader, format, holes);
        }
        set
    }

    /// The shared colour pipeline of [`Self::ground_look_shader`] (ADR-1452), once per device,
    /// shader source, format, depth format and hole cut. Built only for a terrain whose ground
    /// look is on; the set's own colour pipeline runs the source without the look.
    fn cached_ground_look_color(
        &self,
        source: &str,
        shader: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        holes: bool,
    ) -> wgpu::RenderPipeline {
        let key = regia_render::device_pipelines::pipeline_key(&(
            "regia_terrain shared color ground look",
            source,
            format,
            self.depth_format,
            holes,
        ));
        self.shared_pipelines
            .get_or_create(key, || self.shared_color_pipeline(shader, format, holes, true))
    }

    /// The shared colour pipeline (`fs_shared`) of `shader` for colour `format`; `holes`
    /// compiles the hole cut in; `ground_look` (`shader` is [`Self::ground_look_shader`]'s)
    /// names it.
    fn shared_color_pipeline(
        &self,
        shader: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        holes: bool,
        ground_look: bool,
    ) -> wgpu::RenderPipeline {
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("terrain shared color layout"),
                bind_group_layouts: &[
                    Some(&self.frame_layout),
                    Some(&self.terrain_layout),
                    Some(&self.node_layout),
                    Some(&self.engine_csm_layout),
                ],
                immediate_size: 0,
            });
        self.device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(match (holes, ground_look) {
                    (false, false) => "terrain shared color",
                    (true, false) => "terrain shared color (holes)",
                    (false, true) => "terrain shared color (ground look)",
                    (true, true) => "terrain shared color (holes, ground look)",
                }),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_shared"),
                    compilation_options: hole_cut_options(holes),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: Some(wgpu::Face::Back),
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: self.depth_format,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
            })
    }

    /// Builds every shared-pass pipeline this terrain can draw `format` frames with, before a
    /// frame needs one: the plain set ([`Self::ensure_shared`]), the hole-cut set even while no
    /// hole is cut (a crater or tunnel mouth cut mid-play then compiles nothing) and the virtual
    /// texture's when it is on, each with and without the local-light loop (so
    /// [`Self::set_local_lights_enabled`] after the warm compiles nothing). The sets are the device's ([`regia_render::RenderDevice::shared_pipelines`]),
    /// so a terrain built later on the same device finds them too. The engine's frame warm calls
    /// this through [`regia_render::SharedPassDraw::warm_pipelines`]; draws are unchanged.
    pub fn warm_shared(&mut self, format: wgpu::TextureFormat) {
        let _span = tracing::info_span!("terrain_warm_shared").entered();
        self.ensure_shared(format);
        self.warm_hole_set();
        self.warm_other_light_variant(false);
        self.warm_other_light_variant(true);
        self.warm_offscreen_holes();
        self.warm_surface_weather_variant();
        self.warm_cloud_map_variant();
    }

    /// The colour pipelines (plain, hole-cut and the virtual texture's) of the cloud-map state
    /// the terrain is *not* drawing with ([`Self::set_engine_cloud_shadow`], ADR-7886), for both
    /// light variants (where the device has the lights): a cloud layer switched on or off after
    /// the warm, before or after [`Self::set_local_lights_enabled`], compiles nothing. The other
    /// pipelines of a set do not read the map and are shared by both states. One step of
    /// [`Self::warm_shared`] and of the warm job.
    pub(crate) fn warm_cloud_map_variant(&mut self) {
        let (Some(format), Some(shader), Some(source)) = (
            self.shared_format,
            self.shared_shader.clone(),
            self.shared_source.clone(),
        ) else {
            return;
        };
        let _span = tracing::info_span!("terrain_warm_cloud_map").entered();
        let current = self.shared_shader_lights;
        let mut variants = vec![(source, shader, current)];
        if self.local_lights {
            variants.push(self.shared_variant(!current));
        }
        self.shared_cloud_map = !self.shared_cloud_map;
        for (source, shader, lights) in &variants {
            for holes in [false, true] {
                let _ = self.cached_pipeline_set(source, shader, format, holes, *lights);
            }
            if let Some(vt) = &self.vt {
                let ground = self.ground_look_shader(*lights);
                let (vt_source, vt_shader) = ground
                    .as_ref()
                    .map_or((source.as_str(), shader), |(s, m)| (s.as_str(), m));
                let _ = self.cached_vt_color_pipeline(vt_source, vt_shader, format, vt.sample_layout());
            }
        }
        self.shared_cloud_map = !self.shared_cloud_map;
    }

    /// Whether [`Self::warm_shared`] also builds the surface-weather variant: on for a project
    /// with `effects.surface`, so the first wet frame compiles nothing (spec 99 § 6.4).
    pub fn set_warm_surface_weather(&mut self, on: bool) {
        self.warm_surface_weather = on;
    }

    /// The surface-weather colour pipelines of the current shared variant. One step of
    /// [`Self::warm_shared`]; the sets are the device's, so the first wet frame finds them.
    fn warm_surface_weather_variant(&mut self) {
        if !self.warm_surface_weather || self.shared_surface_weather {
            return;
        }
        let (Some(format), Some(shader), Some(source)) = (
            self.shared_format,
            self.shared_shader.clone(),
            self.shared_source.clone(),
        ) else {
            return;
        };
        let _span = tracing::info_span!("terrain_warm_surface_weather").entered();
        self.shared_surface_weather = true;
        let lights = self.shared_shader_lights;
        for holes in [false, true] {
            let _ = self.cached_pipeline_set(&source, &shader, format, holes, lights);
        }
        if let (Some(vt), Some((vt_source, vt_shader))) = (&self.vt, self.ground_look_shader(lights)) {
            let _ = self.cached_vt_color_pipeline(&vt_source, &vt_shader, format, vt.sample_layout());
            if let Some(settings) = self.vt_settings {
                vt::warm_bake_pipelines(&vt::VtGpuInit {
                    device: &self.device,
                    queue: &self.queue,
                    settings,
                    world_width: self.world_width,
                    grid_origin: self.grid_origin,
                    root_level: self.root_level,
                    frame_layout: &self.frame_layout,
                    terrain_layout: &self.terrain_layout,
                    shader: &vt_shader,
                    shader_source: &vt_source,
                    shared_pipelines: &self.shared_pipelines,
                    handle: self.vt_handle.clone(),
                });
            }
        }
        self.shared_surface_weather = false;
    }

    /// The hole-cut shared set for the format [`Self::ensure_shared`] last built (a no-op
    /// before it). One step of [`Self::warm_shared`]: the warm job checks for a quit between
    /// the steps.
    pub(crate) fn warm_hole_set(&self) {
        if let (Some(shader), Some(source), Some(format)) = (
            &self.shared_shader,
            self.shared_source.as_deref(),
            self.shared_format,
        ) {
            let _ = self.cached_pipeline_set(source, shader, format, true, self.shared_shader_lights);
        }
    }

    /// The plain and hole-cut shared sets for every colour format this terrain has drawn into
    /// ([`Self::ensure_shared`]), not only the last one: a reflection-probe capture draws the
    /// terrain into the readback target between game frames, so a hole cut after it would
    /// compile the probe format's hole-cut set on the next capture (DO27 v0.1.10 smoke,
    /// ADR-210). The sets are the device's: a format already built compiles nothing.
    pub(crate) fn warm_seen_formats(&self) {
        let (Some(shader), Some(source)) = (&self.shared_shader, self.shared_source.as_deref()) else {
            return;
        };
        for &format in &self.shared_formats_seen {
            for holes in [false, true] {
                // A quit stops a warm between sets (at most one set of five in flight).
                if regia_render::pipeline_warm::warm_cancelled() {
                    return;
                }
                let _ = self.cached_pipeline_set(source, shader, format, holes, self.shared_shader_lights);
            }
        }
    }

    /// The offscreen hole-cut pipeline (the lab path), which [`Self::set_holes`] builds. One
    /// step of [`Self::warm_shared`].
    pub(crate) fn warm_offscreen_holes(&self) {
        let _ = offscreen_holes_pipeline(
            &self.shared_pipelines,
            &self.device,
            &self.terrain_source,
            &self.terrain_pipeline_layout,
            &self.terrain_shader,
        );
    }

    /// The five shared pipelines for colour `format`; `holes` picks the hole-cut entry
    /// points (`fs_holes` discards in the depth-only passes).
    fn shared_pipeline_set(
        &self,
        shader: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        holes: bool,
    ) -> SharedPipelines {
        let make_layout = |label: &str, groups: &[&wgpu::BindGroupLayout]| {
            self.device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some(label),
                    bind_group_layouts: &groups.iter().map(|layout| Some(*layout)).collect::<Vec<_>>(),
                    immediate_size: 0,
                })
        };
        let node_groups = [&self.frame_layout, &self.terrain_layout, &self.node_layout];
        let depth_layout = make_layout("terrain shared depth layout", &node_groups);
        // vs_shadow binds the cascade at @group(4). Group 3 stays empty so it does
        // not collide with the colour pipeline's sun-map group. A device with fewer than 5
        // bind groups skips the self-shadow cascade entirely (the terrain just does not cast
        // its cascade onto itself), rather than failing the layout.
        let shadow_layout =
            (self.device.limits().max_bind_groups >= regia_render::TERRAIN_SHARED_BIND_GROUPS).then(|| {
                self.device
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("terrain shared shadow layout"),
                        bind_group_layouts: &[
                            Some(&self.frame_layout),
                            Some(&self.terrain_layout),
                            Some(&self.node_layout),
                            None,
                            Some(&self.cascade_layout),
                        ],
                        immediate_size: 0,
                    })
            });
        let depth_state = |compare| wgpu::DepthStencilState {
            format: self.depth_format,
            depth_write_enabled: Some(true),
            depth_compare: Some(compare),
            stencil: Default::default(),
            bias: Default::default(),
        };
        let primitive = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
            ..Default::default()
        };
        let depth = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(if holes {
                    "terrain shared depth (holes)"
                } else {
                    "terrain shared depth"
                }),
                layout: Some(&depth_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some(if holes { "vs_depth_holes" } else { "vs_depth" }),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: holes.then(|| wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_holes"),
                    compilation_options: Default::default(),
                    targets: &[],
                }),
                primitive,
                depth_stencil: Some(depth_state(wgpu::CompareFunction::LessEqual)),
                multisample: Default::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
            });
        let color = self.shared_color_pipeline(shader, format, holes, false);
        let shadow = shadow_layout.as_ref().map(|shadow_layout| {
            self.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(if holes {
                        "terrain shared shadow (holes)"
                    } else {
                        "terrain shared shadow"
                    }),
                    layout: Some(shadow_layout),
                    vertex: wgpu::VertexState {
                        module: shader,
                        entry_point: Some(if holes { "vs_shadow_holes" } else { "vs_shadow" }),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    fragment: holes.then(|| wgpu::FragmentState {
                        module: shader,
                        entry_point: Some("fs_holes"),
                        compilation_options: Default::default(),
                        targets: &[],
                    }),
                    primitive,
                    depth_stencil: Some(depth_state(wgpu::CompareFunction::Less)),
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
                })
        });
        let velocity = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(if holes {
                    "terrain shared velocity (holes)"
                } else {
                    "terrain shared velocity"
                }),
                layout: Some(&depth_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some(if holes { "vs_velocity_holes" } else { "vs_velocity" }),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some(if holes { "fs_velocity_holes" } else { "fs_velocity" }),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba16Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive,
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: self.depth_format,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
            });
        // One ULP toward the camera so a re-raster of the surface that wrote the
        // depth still passes LessEqual. Slope bias stays off so a crate sitting
        // on the ground keeps the layer it stamped.
        let receiver = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(if holes {
                    "terrain shared receiver (holes)"
                } else {
                    "terrain shared receiver"
                }),
                layout: Some(&depth_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_receiver"),
                    compilation_options: hole_cut_options(holes),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive,
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: self.depth_format,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: wgpu::DepthBiasState {
                        constant: -1,
                        slope_scale: 0.0,
                        clamp: 0.0,
                    },
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
            });
        SharedPipelines {
            depth,
            color,
            shadow,
            velocity,
            receiver,
        }
    }

    /// Draws the heightfield into one shared pass. [`Self::ensure_shared`] must have run.
    ///
    /// `list` names one of the draw lists given to the last [`Self::upload_shared`].
    pub fn draw_shared<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        kind: regia_render::SharedPass,
        selection: &LodSelection,
        list: usize,
    ) {
        if self.bind_shared(pass, kind).is_none() {
            return;
        }
        let holes = self.shared_holes.as_ref().and_then(|set| match kind {
            regia_render::SharedPass::Depth => Some(&set.depth),
            regia_render::SharedPass::Color => Some(&set.color),
            regia_render::SharedPass::Shadow { .. } => set.shadow.as_ref(),
            regia_render::SharedPass::Velocity => Some(&set.velocity),
            regia_render::SharedPass::Receiver => Some(&set.receiver),
        });
        // The occlusion tile draws the full grid: what a splash lands on and what a road mesh is
        // compared with is the surface the colour pass shows, not the decimated shadow grid.
        let shadow =
            matches!(kind, regia_render::SharedPass::Shadow { index } if index != PRECIP_SHADOW_SLOT);
        self.draw_list(pass, selection, list, shadow, holes);
    }

    /// Sets the shared pass's pipeline (no hole cut) and its groups 0, 1, 3 and 4; `None` before
    /// [`Self::ensure_shared`] built it.
    fn bind_shared<'a>(
        &'a self,
        pass: &mut wgpu::RenderPass<'a>,
        kind: regia_render::SharedPass,
    ) -> Option<&'a wgpu::RenderPipeline> {
        let vt_bind = self
            .vt
            .as_ref()
            .filter(|_| self.shared_color_vt.is_some())
            .map(vt::VtGpu::sample_bind);
        let pipeline = match kind {
            regia_render::SharedPass::Depth => self.shared_depth.as_ref(),
            regia_render::SharedPass::Color if vt_bind.is_some() => self.shared_color_vt.as_ref(),
            regia_render::SharedPass::Color => self.shared_color.as_ref(),
            regia_render::SharedPass::Shadow { .. } => self.shared_shadow.as_ref(),
            regia_render::SharedPass::Velocity => self.shared_velocity.as_ref(),
            regia_render::SharedPass::Receiver => self.shared_receiver.as_ref(),
        }?;
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &self.frame_bind, &[]);
        pass.set_bind_group(1, &self.terrain_bind, &[]);
        match kind {
            regia_render::SharedPass::Color => {
                pass.set_bind_group(3, &self.engine_csm_bind, &[]);
                if let Some(bind) = vt_bind {
                    pass.set_bind_group(4, bind, &[]);
                }
            }
            regia_render::SharedPass::Shadow { index } => {
                let slot = (index as usize).min(self.cascade_binds.len().saturating_sub(1));
                if let Some(bind) = self.cascade_binds.get(slot) {
                    pass.set_bind_group(4, bind, &[]);
                }
            }
            regia_render::SharedPass::Depth
            | regia_render::SharedPass::Velocity
            | regia_render::SharedPass::Receiver => {}
        }
        Some(pipeline)
    }

    /// Writes the per-pass draw lists (indices into `selection`) into the slot mirror and
    /// returns the changed byte ranges of `slot_buffer`. Each list becomes one
    /// [`DrawList`]: the nodes drawn with the shared grid first (one instanced draw), then
    /// nodes cut by holes (one draw each, with their own index buffer). Nodes a hole
    /// covers entirely are dropped. Grows the buffer (and rebinds) when the lists do not fit.
    fn stage_lists(&mut self, selection: &LodSelection, lists: &[&[u32]]) -> Vec<std::ops::Range<usize>> {
        self.slot_scratch.clear();
        self.draw_lists.clear();
        for list in lists {
            let start = self.slot_scratch.len() as u32;
            self.list_holes.clear();
            self.list_dense.clear();
            for &slot in *list {
                let Some(draw) = selection.nodes.get(slot as usize) else {
                    continue;
                };
                if self.dense_cpu.get(slot as usize).copied().unwrap_or(false) {
                    self.list_dense.push(slot);
                    continue;
                }
                if self.hole_cut_nodes.contains(&draw.node) {
                    continue;
                }
                if self.mask.is_some() {
                    match self
                        .slot_holes
                        .get(slot as usize)
                        .copied()
                        .unwrap_or(NodeHoleState::Empty)
                    {
                        NodeHoleState::Full => continue,
                        _ if self.hole_index_buffers.contains_key(&draw.node) => {
                            self.list_holes.push(slot);
                            continue;
                        }
                        _ => {}
                    }
                }
                self.slot_scratch.push(slot);
            }
            let plain = self.slot_scratch.len() as u32 - start;
            let dense = std::mem::take(&mut self.list_dense);
            let holes = std::mem::take(&mut self.list_holes);
            let dense_n = dense.len() as u32;
            self.slot_scratch.extend_from_slice(&dense);
            self.slot_scratch.extend_from_slice(&holes);
            self.list_dense = dense;
            self.list_holes = holes;
            self.draw_lists.push(DrawList {
                start,
                plain,
                dense: dense_n,
                len: self.slot_scratch.len() as u32 - start,
            });
        }
        let needed = (self.slot_scratch.len() as u32).max(1);
        if needed > self.slot_capacity {
            let _span = tracing::info_span!("terrain_slot_grow").entered();
            let capacity = needed.next_power_of_two();
            self.slot_buffer = make_node_storage(&self.device, "terrain node slots", u64::from(capacity) * 4);
            self.slot_capacity = capacity;
            self.slot_cpu.clear();
            self.rebuild_node_bind();
        }
        // The scratch stays on `self`. Copy it into the mirror only when a word or the
        // length changed, so a held selection does not allocate another list.
        let mut allocs = 0u64;
        let TerrainGpu {
            slot_cpu,
            slot_scratch,
            ..
        } = self;
        commit_slot_words(slot_cpu, slot_scratch, &mut allocs)
    }

    /// Draws list `list` of the last [`Self::stage_lists`]: one instanced draw of the
    /// shared grid (the decimated shadow grid when `shadow`), then one draw per node cut
    /// by a hole, with the `holes` pipeline (the per-pixel cut) when there is one.
    fn draw_list(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        selection: &LodSelection,
        list: usize,
        shadow: bool,
        holes: Option<&wgpu::RenderPipeline>,
    ) {
        let Some(range) = self.draw_lists.get(list) else {
            return;
        };
        if range.len == 0 {
            return;
        }
        pass.set_bind_group(2, &self.node_bind, &[]);
        if range.plain > 0 {
            let (buffer, count) = if shadow {
                (&self.shadow_index_buffer, self.shadow_index_count)
            } else {
                (&self.grid_index_buffer, self.grid_index_count)
            };
            pass.set_index_buffer(buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..count, 0, range.start..range.start + range.plain);
        }
        if range.dense > 0 {
            let (buffer, count) = if shadow {
                (&self.dense_shadow_index_buffer, self.dense_shadow_index_count)
            } else {
                (&self.dense_index_buffer, self.dense_index_count)
            };
            let first = range.start + range.plain;
            pass.set_index_buffer(buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..count, 0, first..first + range.dense);
        }
        let cut = range.start + range.plain + range.dense;
        if cut < range.start + range.len
            && let Some(holes) = holes
        {
            // Same bind group layouts: the groups bound so far stay bound.
            pass.set_pipeline(holes);
        }
        for k in cut..range.start + range.len {
            let Some(draw) = self
                .slot_cpu
                .get(k as usize)
                .and_then(|slot| selection.nodes.get(*slot as usize))
            else {
                continue;
            };
            let Some(buffer) = self.hole_index_buffers.get(&draw.node) else {
                continue;
            };
            pass.set_index_buffer(buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..buffer.size() as u32 / 4, 0, k..k + 1);
        }
    }
}

impl TerrainRenderer for TerrainGpu {
    fn update_frame(&self, frame: &TerrainFrame) {
        let uniform = self.frame_uniform(frame);
        self.queue
            .write_buffer(&self.frame_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    fn upload_nodes(&mut self, selection: &LodSelection) {
        let _span = tracing::info_span!("terrain_upload_nodes").entered();
        self.stage_hole_states(selection);
        let ranges = self.stage_nodes(selection);
        let mut bytes = 0u64;
        for range in &ranges {
            self.queue.write_buffer(
                &self.node_buffer,
                range.start as u64,
                &self.node_cpu[range.clone()],
            );
            bytes += range.len() as u64;
        }
        self.ensure_hole_indices(selection);
        // The offscreen path draws the whole selection: one identity list, reused.
        let mut identity = std::mem::take(&mut self.identity_slots);
        identity.clear();
        identity.extend(0..selection.nodes.len() as u32);
        let slot_ranges = self.stage_lists(selection, &[&identity]);
        self.identity_slots = identity;
        for range in &slot_ranges {
            self.queue.write_buffer(
                &self.slot_buffer,
                range.start as u64,
                bytemuck::cast_slice(&self.slot_cpu[range.start / 4..range.end / 4]),
            );
            bytes += range.len() as u64;
        }
        self.node_upload_last = ((ranges.len() + slot_ranges.len()) as u32, bytes);
    }

    fn draw_sky(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.sky_pipeline);
        pass.set_bind_group(0, &self.frame_bind, &[]);
        pass.draw(0..3, 0..1);
    }

    fn draw_terrain(&self, pass: &mut wgpu::RenderPass<'_>, selection: &LodSelection) {
        pass.set_pipeline(&self.terrain_pipeline);
        pass.set_bind_group(0, &self.frame_bind, &[]);
        pass.set_bind_group(1, &self.terrain_bind, &[]);
        pass.set_bind_group(3, &self.shadow_bind, &[]);
        self.draw_list(pass, selection, 0, false, self.terrain_holes_pipeline.as_ref());
    }

    fn draw_scatter(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some(instances) = &self.instance_buffer else {
            return;
        };
        if self.instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.scatter_pipeline);
        pass.set_bind_group(0, &self.frame_bind, &[]);
        pass.set_bind_group(1, &self.scatter_bind, &[]);
        pass.set_vertex_buffer(0, instances.slice(..));
        pass.draw(0..24, 0..self.instance_count);
    }

    fn draw_water(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some(water_bind) = &self.water_bind else {
            return;
        };
        pass.set_pipeline(&self.water_pipeline);
        pass.set_bind_group(0, &self.frame_bind, &[]);
        pass.set_bind_group(1, water_bind, &[]);
        const WATER_GRID: u32 = 64;
        pass.draw(0..WATER_GRID * WATER_GRID * 6, 0..1);
    }
}

/// One mip-chained 2D array texture with its view and GPU byte cost (for budget accounting).
struct GpuArrayTexture {
    view: wgpu::TextureView,
    /// Kept alive for the view (dropping the texture would invalidate it).
    #[allow(dead_code)]
    texture: wgpu::Texture,
    bytes: u64,
}

/// `true` when a KTX2 header extent is non-zero and within the device's 2D texture limit, so
/// `create_texture` cannot hit a validation error (which the render error handler escalates).
fn ktx2_extent_fits(width: u32, height: u32, max_dimension: u32) -> bool {
    width != 0 && height != 0 && width <= max_dimension && height <= max_dimension
}

/// Maps a KTX2 `Format` to the wgpu block-compressed format and its bytes-per-block: BCn from a
/// desktop cook, ASTC or ETC2 / EAC from an Android cook.
fn bcn_format(format: ktx2::Format) -> Option<(wgpu::TextureFormat, u32)> {
    use ktx2::Format;
    use wgpu::{AstcBlock as B, AstcChannel as C};
    let astc = |block, srgb: bool| wgpu::TextureFormat::Astc {
        block,
        channel: if srgb { C::UnormSrgb } else { C::Unorm },
    };
    let (format, block_bytes) = match format {
        Format::BC1_RGB_UNORM_BLOCK | Format::BC1_RGBA_UNORM_BLOCK => (wgpu::TextureFormat::Bc1RgbaUnorm, 8),
        Format::BC1_RGB_SRGB_BLOCK | Format::BC1_RGBA_SRGB_BLOCK => {
            (wgpu::TextureFormat::Bc1RgbaUnormSrgb, 8)
        }
        Format::BC3_UNORM_BLOCK => (wgpu::TextureFormat::Bc3RgbaUnorm, 16),
        Format::BC3_SRGB_BLOCK => (wgpu::TextureFormat::Bc3RgbaUnormSrgb, 16),
        Format::BC4_UNORM_BLOCK => (wgpu::TextureFormat::Bc4RUnorm, 8),
        Format::BC5_UNORM_BLOCK => (wgpu::TextureFormat::Bc5RgUnorm, 16),
        Format::BC7_UNORM_BLOCK => (wgpu::TextureFormat::Bc7RgbaUnorm, 16),
        Format::BC7_SRGB_BLOCK => (wgpu::TextureFormat::Bc7RgbaUnormSrgb, 16),
        // The Android cook (`regia cook --platform android`): ASTC, 16 bytes a block.
        Format::ASTC_4x4_UNORM_BLOCK => (astc(B::B4x4, false), 16),
        Format::ASTC_4x4_SRGB_BLOCK => (astc(B::B4x4, true), 16),
        Format::ASTC_5x5_UNORM_BLOCK => (astc(B::B5x5, false), 16),
        Format::ASTC_5x5_SRGB_BLOCK => (astc(B::B5x5, true), 16),
        Format::ASTC_6x6_UNORM_BLOCK => (astc(B::B6x6, false), 16),
        Format::ASTC_6x6_SRGB_BLOCK => (astc(B::B6x6, true), 16),
        Format::ASTC_8x8_UNORM_BLOCK => (astc(B::B8x8, false), 16),
        Format::ASTC_8x8_SRGB_BLOCK => (astc(B::B8x8, true), 16),
        // The Android cook under `[cook.textures.android] format = "etc2"`.
        Format::ETC2_R8G8B8_UNORM_BLOCK => (wgpu::TextureFormat::Etc2Rgb8Unorm, 8),
        Format::ETC2_R8G8B8_SRGB_BLOCK => (wgpu::TextureFormat::Etc2Rgb8UnormSrgb, 8),
        Format::ETC2_R8G8B8A8_UNORM_BLOCK => (wgpu::TextureFormat::Etc2Rgba8Unorm, 16),
        Format::ETC2_R8G8B8A8_SRGB_BLOCK => (wgpu::TextureFormat::Etc2Rgba8UnormSrgb, 16),
        Format::EAC_R11_UNORM_BLOCK => (wgpu::TextureFormat::EacR11Unorm, 8),
        Format::EAC_R11G11_UNORM_BLOCK => (wgpu::TextureFormat::EacRg11Unorm, 16),
        _ => return None,
    };
    Some((format, block_bytes))
}

/// A stable key for [`regia_render::report_wrong_platform_texture`]: one per terrain array label
/// (FNV-1a), so a map reload does not count the same array twice.
fn wrong_platform_key(label: &str) -> u64 {
    label.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl GpuArrayTexture {
    /// Uploads `layers` (each getting a CPU box-filter mip chain) as one RGBA8 array.
    fn upload(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        label: &str,
        layers: &[&crate::material::Rgba8Image],
        format: wgpu::TextureFormat,
    ) -> Self {
        let width = layers.first().map_or(1, |l| l.width);
        let height = layers.first().map_or(1, |l| l.height);
        let chains: Vec<Vec<crate::material::Rgba8Image>> =
            layers.iter().map(|l| crate::material::mip_chain(l)).collect();
        let mips = chains.first().map_or(1, |c| c.len() as u32);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: layers.len().max(1) as u32,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut bytes = 0u64;
        for (level, first) in chains.first().into_iter().flatten().enumerate() {
            let (mw, mh) = (first.width, first.height);
            let row = mw * 4;
            let padded_row =
                row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let mut data = Vec::with_capacity((padded_row * mh * layers.len().max(1) as u32) as usize);
            for chain in &chains {
                let mip = &chain[level];
                for y in 0..mh {
                    let src = &mip.data[(y * mw * 4) as usize..((y + 1) * mw * 4) as usize];
                    data.extend_from_slice(src);
                    data.resize(data.len() + (padded_row - row) as usize, 0);
                }
            }
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(mh),
                },
                wgpu::Extent3d {
                    width: mw,
                    height: mh,
                    depth_or_array_layers: layers.len().max(1) as u32,
                },
            );
            bytes += padded_row as u64 * mh as u64 * layers.len().max(1) as u64;
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        Self { view, texture, bytes }
    }

    /// Uploads a mip-chained 2D array of KTX2 blobs (one blob per layer, identical
    /// dimensions/format) for hardware-native block-compressed sampling: BCn from a desktop
    /// cook, ASTC or ETC2 / EAC from an Android cook. Rows are padded to the copy alignment; blobs
    /// with fewer mip levels than the first are zero-filled. A format the device cannot sample is
    /// decoded to RGBA8 ([`Self::upload_decoded`]) and reported as a texture cooked for another
    /// platform.
    ///
    /// # Errors
    ///
    /// Returns [`TerrainError`] when a blob is not a KTX2 file, uses a format this build does
    /// not map, the blobs disagree on dimensions/format, or the device cannot sample the format
    /// and a level does not decode.
    fn upload_ktx2_array(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        label: &str,
        blobs: &[&[u8]],
    ) -> Result<Self, TerrainError> {
        Self::upload_ktx2_array_from(device, queue, label, blobs, 0)
    }

    /// [`Self::upload_ktx2_array`] without the top `skip` levels (clamped so one level stays):
    /// the tier-scaled layer arrays ([`TerrainRenderer::set_layer_mip_skip`]).
    fn upload_ktx2_array_from(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        label: &str,
        blobs: &[&[u8]],
        skip: u32,
    ) -> Result<Self, TerrainError> {
        let bad = |what: &str| {
            TerrainError::Gpu(regia_render::RenderError::Readback(format!(
                "ktx2 {label}: {what}"
            )))
        };
        let mut readers = Vec::with_capacity(blobs.len());
        for blob in blobs {
            let reader = safe_ktx2_reader(blob).map_err(|e| bad(&e))?;
            readers.push(reader);
        }
        let first = readers.first().ok_or_else(|| bad("empty blob set"))?;
        let full_width = first.header().pixel_width;
        let full_height = first.header().pixel_height.max(1);
        let full_mips = readers.iter().map(|r| r.levels().len() as u32).max().unwrap_or(1);
        let skip = skip.min(full_mips.saturating_sub(1));
        // Also rejects a zero width.
        if !ktx2_extent_fits(full_width, full_height, device.limits().max_texture_dimension_2d) {
            return Err(bad("level 0 extent is zero or exceeds the device texture limit"));
        }
        let vk_format = first
            .header()
            .format
            .ok_or_else(|| bad("undefined (supercompressed) format"))?;
        let (format, block_bytes) = bcn_format(vk_format).ok_or_else(|| bad("unsupported block format"))?;
        let (bw, bh) = format.block_dimensions();
        if !full_width.is_multiple_of(bw) || !full_height.is_multiple_of(bh) {
            return Err(bad("level 0 is not a whole number of blocks"));
        }
        // The top level after the skip is level 0 of the texture: it must be whole blocks too
        // (a 6×6 chain cannot start at an odd multiple of 3).
        let mut skip = skip;
        while skip > 0
            && !((full_width >> skip).is_multiple_of(bw) && (full_height >> skip).is_multiple_of(bh))
        {
            skip -= 1;
        }
        let (width, height) = ((full_width >> skip).max(1), (full_height >> skip).max(1));
        // A header may claim more levels than the post-skip size can hold; wgpu validation
        // would reject the texture (and the device hook aborts), so clamp to the real chain.
        let max_chain = u32::BITS - width.max(height).leading_zeros();
        let mips = (full_mips - skip).min(max_chain);
        for reader in &readers {
            if reader.header().pixel_width != full_width
                || reader.header().pixel_height.max(1) != full_height
                || reader.header().format.map(|f| bcn_format(f).map(|(nf, _)| nf)) != Some(Some(format))
            {
                return Err(bad("blobs disagree on dimensions or format"));
            }
        }
        if !device.features().contains(format.required_features()) {
            if regia_render::compressed_texture::decoded_rgba8_format(format).is_none() {
                regia_render::report_wrong_platform_texture(wrong_platform_key(label), label, format, None);
                return Err(bad(&format!(
                    "the device cannot sample {format:?} and there is no CPU decoder for it: \
                     the terrain was cooked for another platform"
                )));
            }
            let decoded = Self::upload_decoded(
                device,
                queue,
                label,
                &readers,
                format,
                (width, height),
                mips,
                skip,
            )?;
            regia_render::report_wrong_platform_texture(
                wrong_platform_key(label),
                label,
                format,
                Some(decoded.bytes),
            );
            return Ok(decoded);
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: blobs.len() as u32,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut bytes = 0u64;
        for level in 0..mips {
            let mw = (width >> level).max(1);
            let mh = (height >> level).max(1);
            let blocks_row = mw.div_ceil(bw);
            let block_rows = mh.div_ceil(bh);
            let row = blocks_row as u64 * u64::from(block_bytes);
            let padded_row = row.div_ceil(u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT))
                * u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let mut data = Vec::with_capacity((padded_row * block_rows as u64 * blobs.len() as u64) as usize);
            for reader in &readers {
                let mut src = reader
                    .levels()
                    .nth((level + skip) as usize)
                    .map(|l| l.data.to_vec())
                    .unwrap_or_default();
                let needed = (row * block_rows as u64) as usize;
                src.resize(needed, 0);
                let mut rest: &[u8] = &src;
                for _ in 0..block_rows {
                    let take = (row as usize).min(rest.len());
                    let (row_slice, tail) = rest.split_at(take);
                    data.extend_from_slice(row_slice);
                    data.resize(data.len() + (padded_row - row) as usize, 0);
                    rest = tail;
                }
            }
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row as u32),
                    rows_per_image: Some(block_rows),
                },
                wgpu::Extent3d {
                    // Block formats: the copy extent is in whole blocks even when the mip's
                    // texel size (2×2, 1×1) is smaller than one block.
                    width: blocks_row * bw,
                    height: block_rows * bh,
                    depth_or_array_layers: blobs.len() as u32,
                },
            );
            bytes += padded_row * block_rows as u64 * blobs.len() as u64;
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        Ok(Self { view, texture, bytes })
    }

    /// The KTX2 layers of [`Self::upload_ktx2_array_from`] decoded to RGBA8 on the CPU
    /// (`regia_render::compressed_texture::decode_blocks_rgba8`), for a device that cannot sample
    /// their format: a BCn pack on a phone (Mali and Adreno: ASTC / ETC2 only), an ASTC pack on an
    /// ETC2-only phone or a desktop GPU. The decoder writes the values the GPU would sample
    /// (BC4 / EAC R11 as `(r, 0, 0, 1)`, BC5 / EAC RG11 and the terrain's ASTC normals as
    /// `(x, y, 0, 1)`), so the shader's `.r` and `.rg` reads see the same layout. Costs 4 to 8
    /// times the block memory (more at larger ASTC blocks) and a slow load.
    #[allow(clippy::too_many_arguments)]
    fn upload_decoded(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        label: &str,
        readers: &[ktx2::Reader<&[u8]>],
        format: wgpu::TextureFormat,
        (width, height): (u32, u32),
        mips: u32,
        skip: u32,
    ) -> Result<Self, TerrainError> {
        let _span = tracing::info_span!("terrain_block_decode", label).entered();
        let bad = |what: String| {
            TerrainError::Gpu(regia_render::RenderError::Readback(format!(
                "ktx2 {label}: {what}"
            )))
        };
        let rgba = regia_render::compressed_texture::decoded_rgba8_format(format)
            .ok_or_else(|| bad(format!("no CPU decoder for {format:?}")))?;
        let layers = readers.len() as u32;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: layers,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: rgba,
            // COPY_SRC only so unit tests can read the decoded texels back.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | if cfg!(test) {
                    wgpu::TextureUsages::COPY_SRC
                } else {
                    wgpu::TextureUsages::empty()
                },
            view_formats: &[],
        });
        let mut bytes = 0u64;
        for level in 0..mips {
            let mw = (width >> level).max(1);
            let mh = (height >> level).max(1);
            let row = mw as usize * 4;
            let padded_row = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize)
                * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
            let mut data = Vec::with_capacity(padded_row * mh as usize * readers.len());
            for reader in readers {
                // Blobs with fewer levels than the first read as zero blocks (black), like the
                // native path's zero fill.
                let texels = match reader.levels().nth((level + skip) as usize) {
                    Some(l) => regia_render::compressed_texture::decode_blocks_rgba8(format, mw, mh, l.data)
                        .map_err(bad)?,
                    None => vec![0; row * mh as usize],
                };
                for src in texels.chunks_exact(row) {
                    data.extend_from_slice(src);
                    data.resize(data.len() + padded_row - row, 0);
                }
            }
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row as u32),
                    rows_per_image: Some(mh),
                },
                wgpu::Extent3d {
                    width: mw,
                    height: mh,
                    depth_or_array_layers: layers,
                },
            );
            bytes += data.len() as u64;
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        Ok(Self { view, texture, bytes })
    }
}

/// All material-path textures of one terrain (dropped together when replaced).
struct MaterialTextures {
    macro_albedo: GpuArrayTexture,
    macro_normal: GpuArrayTexture,
    macro_coarse_albedo: GpuArrayTexture,
    macro_coarse_normal: GpuArrayTexture,
    layer_albedo: GpuArrayTexture,
    layer_normal: GpuArrayTexture,
    layer_height: GpuArrayTexture,
    layer_orm: GpuArrayTexture,
    sim_height: wgpu::Texture,
    /// Splat weight packs over the whole map (two RGBA8 layers: layers 0–3, 4–7), binding 23.
    splat: GpuArrayTexture,
}

impl MaterialTextures {
    /// 1×1 neutral placeholders for every slot: white albedo, flat normal, mid height, flat sim.
    fn neutral(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let one = crate::material::Rgba8Image::solid(1, 1, [255, 255, 255, 255]);
        let one_normal = crate::material::Rgba8Image::solid(1, 1, [128, 128, 255, 255]);
        let one_mid = crate::material::Rgba8Image::solid(1, 1, [128, 128, 128, 255]);
        let one_zero = crate::material::Rgba8Image::solid(1, 1, [0, 0, 0, 0]);
        let array = |label: &str, image: &crate::material::Rgba8Image, format| {
            GpuArrayTexture::upload(device, queue, label, &[image], format)
        };
        Self {
            macro_albedo: array(
                "terrain macro albedo (neutral)",
                &one,
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ),
            macro_normal: array(
                "terrain macro normal (neutral)",
                &one_mid,
                wgpu::TextureFormat::Rgba8Unorm,
            ),
            macro_coarse_albedo: array(
                "terrain macro coarse (neutral)",
                &one,
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ),
            macro_coarse_normal: array(
                "terrain macro coarse normal (neutral)",
                &one_mid,
                wgpu::TextureFormat::Rgba8Unorm,
            ),
            layer_albedo: array(
                "terrain layer albedo (neutral)",
                &one,
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ),
            layer_normal: array(
                "terrain layer normal (neutral)",
                &one_normal,
                wgpu::TextureFormat::Rgba8Unorm,
            ),
            layer_height: array(
                "terrain layer height (neutral)",
                &one_mid,
                wgpu::TextureFormat::Rgba8Unorm,
            ),
            layer_orm: array(
                "terrain layer orm (neutral)",
                &crate::material::Rgba8Image::solid(1, 1, [255, 255, 0, 255]),
                wgpu::TextureFormat::Rgba8Unorm,
            ),
            splat: GpuArrayTexture::upload(
                device,
                queue,
                "terrain splat weights (off)",
                &[&one_zero, &one_zero],
                wgpu::TextureFormat::Rgba8Unorm,
            ),
            sim_height: device.create_texture(&wgpu::TextureDescriptor {
                label: Some("terrain sim heights (neutral)"),
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
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_group(
        &self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        height_view: &wgpu::TextureView,
        terrain_buffer: &wgpu::Buffer,
        layers_buffer: &wgpu::Buffer,
        scorch_buffer: &wgpu::Buffer,
        material_buffer: &wgpu::Buffer,
        [sampler, aniso_sampler]: [&wgpu::Sampler; 2],
        contact: &ContactBinding,
        height_filter: Option<&wgpu::Sampler>,
        slope: &SlopeBinding,
    ) -> wgpu::BindGroup {
        let sim_view = self
            .sim_height
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(height_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: terrain_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: layers_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: scorch_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::TextureView(&self.macro_albedo.view),
            },
            wgpu::BindGroupEntry {
                binding: 6,
                resource: wgpu::BindingResource::TextureView(&self.macro_normal.view),
            },
            wgpu::BindGroupEntry {
                binding: 7,
                resource: wgpu::BindingResource::TextureView(&self.macro_coarse_albedo.view),
            },
            wgpu::BindGroupEntry {
                binding: 8,
                resource: wgpu::BindingResource::TextureView(&self.macro_coarse_normal.view),
            },
            wgpu::BindGroupEntry {
                binding: 9,
                resource: wgpu::BindingResource::TextureView(&self.layer_albedo.view),
            },
            wgpu::BindGroupEntry {
                binding: 10,
                resource: wgpu::BindingResource::TextureView(&self.layer_normal.view),
            },
            wgpu::BindGroupEntry {
                binding: 11,
                resource: wgpu::BindingResource::TextureView(&self.layer_height.view),
            },
            wgpu::BindGroupEntry {
                binding: 12,
                resource: material_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 13,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 14,
                resource: wgpu::BindingResource::TextureView(&sim_view),
            },
            wgpu::BindGroupEntry {
                binding: 15,
                resource: wgpu::BindingResource::TextureView(&self.layer_orm.view),
            },
            wgpu::BindGroupEntry {
                binding: 16,
                resource: wgpu::BindingResource::TextureView(&contact.view),
            },
            wgpu::BindGroupEntry {
                binding: 17,
                resource: contact.buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 20,
                resource: wgpu::BindingResource::TextureView(&slope.view),
            },
            wgpu::BindGroupEntry {
                binding: 21,
                resource: wgpu::BindingResource::Sampler(&slope.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 22,
                resource: wgpu::BindingResource::Sampler(aniso_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 23,
                resource: wgpu::BindingResource::TextureView(&self.splat.view),
            },
        ];
        if let Some(height_sampler) = height_filter {
            entries.push(wgpu::BindGroupEntry {
                binding: 18,
                resource: wgpu::BindingResource::TextureView(height_view),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: 19,
                resource: wgpu::BindingResource::Sampler(height_sampler),
            });
        }
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("terrain data bind"),
            layout,
            entries: &entries,
        })
    }

    /// GPU bytes of every material texture (budget accounting).
    fn bytes(&self) -> u64 {
        self.macro_albedo.bytes
            + self.macro_normal.bytes
            + self.macro_coarse_albedo.bytes
            + self.macro_coarse_normal.bytes
            + self.layer_albedo.bytes
            + self.layer_normal.bytes
            + self.layer_height.bytes
            + self.layer_orm.bytes
            + self.splat.bytes
    }
}

/// The contact-map texture and uniform bound at group 1, bindings 16–17.
struct ContactBinding {
    view: wgpu::TextureView,
    buffer: wgpu::Buffer,
    /// The bound map's rebuild counter (`None` for the neutral binding).
    generation: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl ContactBinding {
    /// A 1×1 open (zero) map with a disabled window: the terrain shades exactly as without it.
    fn neutral(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain contact map (neutral)"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        // A zero-initialised uniform has `window.w = 0`: the shader skips the lookup.
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terrain contact params (neutral)"),
            size: size_of::<crate::scatter::contact::ContactUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            buffer,
            generation: None,
        }
    }
}

/// The heightfield slope pyramid ([`crate::slope_mips`]) bound at 20/21: `Rg16Float`, one
/// mip per height pyramid level, read with clamped bilinear filtering by `fragment_normal`.
struct SlopeBinding {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    /// GPU bytes of every level.
    bytes: u64,
}

impl SlopeBinding {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, map: &TerrainMap, mips: &HeightMips) -> Self {
        let _span = tracing::info_span!("terrain_slope_pyramid").entered();
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain slopes"),
            size: wgpu::Extent3d {
                width: map.width(),
                height: map.height(),
                depth_or_array_layers: 1,
            },
            mip_level_count: mips.level_count(),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut bytes = 0u64;
        for k in 0..mips.level_count() {
            let Some((w, h)) = crate::slope_mips::level_size(map, mips, k) else {
                continue;
            };
            write_slope_rect(queue, &texture, map, mips, k, (0, w, 0, h));
            bytes += u64::from(w) * u64::from(h) * 4;
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terrain slope filter"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        Self {
            texture,
            view,
            sampler,
            bytes,
        }
    }
}

/// Uploads the slope texels `rect = (col_start, col_end, row_start, row_end)` of level `k`.
fn write_slope_rect(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    map: &TerrainMap,
    mips: &HeightMips,
    k: u32,
    rect: (u32, u32, u32, u32),
) {
    let Some((lw, lh)) = crate::slope_mips::level_size(map, mips, k) else {
        return;
    };
    let (c0, c1, r0, r1) = (rect.0, rect.1.min(lw), rect.2, rect.3.min(lh));
    let texels = crate::slope_mips::slope_texels(map, mips, k, (c0, c1, r0, r1));
    if texels.is_empty() {
        return;
    }
    let w = c1 - c0;
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: k,
            origin: wgpu::Origin3d { x: c0, y: r0, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(r1 - r0),
        },
        wgpu::Extent3d {
            width: w,
            height: r1 - r0,
            depth_or_array_layers: 1,
        },
    );
}

/// Uploads the texels `rect = (col_start, col_end, row_start, row_end)` of pyramid level `k`
/// in the normalised units of the height texture.
fn write_height_mip(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    k: u32,
    level: &crate::height_mips::HeightMipLevel,
    rect: (u32, u32, u32, u32),
    offset: f32,
    scale: f32,
) {
    let (c0, c1, r0, r1) = rect;
    if c1 <= c0 || r1 <= r0 {
        return;
    }
    let recip = if scale.abs() > 1e-9 { scale.recip() } else { 1.0 };
    let w = c1 - c0;
    let mut data = Vec::with_capacity((w * (r1 - r0)) as usize);
    for row in r0..r1 {
        let base = (row * level.width + c0) as usize;
        data.extend(
            level.data[base..base + w as usize]
                .iter()
                .map(|&h| (h - offset) * recip),
        );
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: k,
            origin: wgpu::Origin3d { x: c0, y: r0, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&data),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(r1 - r0),
        },
        wgpu::Extent3d {
            width: w,
            height: r1 - r0,
            depth_or_array_layers: 1,
        },
    );
}

/// Cells per node edge the shared shadow passes draw (the camera passes draw the full grid).
pub const SHADOW_GRID_SIZE: u32 = 16;

/// Per layer slot: `(has_textures, has_orm)` of the installed material.
type LayerFlags = [(bool, bool); MAX_LAYERS];

/// No material installed: every slot renders its flat colour.
const NO_LAYER_TEXTURES: LayerFlags = [(false, false); MAX_LAYERS];

/// Lowest and highest surface altitude of `map`, metres.
fn map_altitude_range(map: &TerrainMap) -> [f32; 2] {
    let (lo, hi) = map.min_max_height();
    if lo.is_finite() && hi.is_finite() {
        [lo, hi]
    } else {
        [0.0, 0.0]
    }
}

/// Per layer of `layers` (the slots the shader blends): whether its altitude band reaches the
/// lowest and the highest ground of `range`, the two tests `altitude_band` in `terrain.wgsl`
/// makes against `layers.count.yz` (the only place the range enters the shading).
fn altitude_reach(layers: &TerrainLayers, range: [f32; 2]) -> Vec<(bool, bool)> {
    layers
        .layers
        .iter()
        .take(MAX_LAYERS)
        .map(|layer| {
            (
                layer.altitude_min <= range[0] + 1e-3,
                layer.altitude_max >= range[1] - 1e-3,
            )
        })
        .collect()
}

/// The WGSL `Layers` uniform: one slot per layer, the material's texture-present flags,
/// and (in `count.yz`) the map's altitude range so altitude bands that reach the lowest or
/// highest ground keep full weight there instead of fading out at the crest.
///
/// `layer_texel` is the unit-square size of one layer texel (`None` before a material is
/// installed, which keeps the explicit-LOD footprint at zero).
fn layers_uniform(
    layers: &TerrainLayers,
    flags: &LayerFlags,
    layer_texel: Option<f32>,
    altitude_range: [f32; 2],
) -> LayersUniform {
    let mut uniform = LayersUniform {
        count: [layers.count() as f32, altitude_range[0], altitude_range[1], 0.0],
        slots: [LayerSlotUniform {
            albedo_roughness: [0.7, 0.0, 0.7, 1.0],
            slope_alt: [-1e3, 1e3, -1e3, 1e3],
            params: [1.0, 0.2, 0.0, 0.0],
            params2: [0.0; 4],
            params3: [0.0; 4],
        }; MAX_LAYERS],
    };
    for (i, layer) in layers.layers.iter().take(MAX_LAYERS).enumerate() {
        let (has_textures, has_orm) = flags[i];
        uniform.slots[i] = LayerSlotUniform {
            albedo_roughness: [layer.albedo[0], layer.albedo[1], layer.albedo[2], layer.roughness],
            slope_alt: [
                layer.slope_min,
                layer.slope_max,
                layer.altitude_min,
                layer.altitude_max,
            ],
            params: [
                layer.tiling,
                layer.blend_sharpness,
                f32::from(layer.triplanar),
                // Metres per layer texel for the explicit-LOD footprint.
                layer_texel.map_or(0.0, |texel| texel / layer.tiling.max(1e-6)),
            ],
            params2: [
                layer.detail_normal_strength,
                layer.height_blend,
                f32::from(has_textures),
                f32::from(has_textures),
            ],
            params3: [f32::from(has_orm), 0.0, 0.0, 0.0],
        };
        let [porosity, snow] = layer_weather_lanes(layer);
        uniform.slots[i].params3[1] = porosity;
        uniform.slots[i].params3[2] = snow;
    }
    uniform.slots[0].params3[3] = stack_weather_flags(layers);
    uniform
}

/// A layer's weather block in `params3.yz` (spec 99 § 7.3): `porosity + 1` when it names one,
/// −1 when it reads the ORM map's blue channel (`porosity_from_orm`, v5, ADR-3367; the shader
/// falls back to the derived porosity where no ORM map is bound), 0 to derive it from
/// roughness, and `1 - snow`. The default response packs to zeros, so a v3 stack's uniform
/// keeps its bytes.
fn layer_weather_lanes(layer: &crate::layers::LayerDesc) -> [f32; 2] {
    let weather = layer.weather.unwrap_or_default();
    let unnamed = if weather.porosity_from_orm { -1.0 } else { 0.0 };
    [
        weather.porosity.map_or(unnamed, |p| p.clamp(0.0, 1.0) + 1.0),
        1.0 - weather.snow.clamp(0.0, 1.0),
    ]
}

/// The stack's weather flags in slot 0's `params3.w`: +1 when a layer overrides the default
/// response (the shader then blends the layers' blocks), +2 when the stack holds no puddles.
/// 0 for a default stack.
fn stack_weather_flags(layers: &TerrainLayers) -> f32 {
    f32::from(u8::from(layers.has_weather_overrides())) + 2.0 * f32::from(u8::from(!layers.puddles))
}

fn write_full_heights(queue: &wgpu::Queue, texture: &wgpu::Texture, map: &TerrainMap) {
    let scale_recip = if map.height_scale().abs() > 1e-9 {
        map.height_scale().recip()
    } else {
        1.0
    };
    let normalized: Vec<f32> = map
        .heights()
        .iter()
        .map(|&h| (h - map.height_offset()) * scale_recip)
        .collect();
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&normalized),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(map.width() * 4),
            rows_per_image: Some(map.height()),
        },
        wgpu::Extent3d {
            width: map.width(),
            height: map.height(),
            depth_or_array_layers: 1,
        },
    );
}

/// Creates a buffer with `contents` (mapped at creation) and returns it, unmapped.
/// `source` (terrain.wgsl) with the engine's irradiance-probe lookup in place of the
/// `engine_gi` stub between the `@engine-gi` markers (the shared colour pass, whose group 3
/// binds the probes at 7 and 8). The private path keeps the stub.
#[must_use]
pub fn with_engine_gi(source: &str) -> String {
    let (Some(start), Some(end)) = (
        source.find("// @engine-gi-begin"),
        source.find("// @engine-gi-end"),
    ) else {
        return source.to_owned();
    };
    format!(
        "{}@group(3) @binding(8) var<uniform> ddgi: DdgiUniform;
@group(3) @binding(9) var<storage, read> ddgi_data: array<u32>;
{}
// The engine's probes (`EngineGi`): irradiance / π (a = how much of it to take) and, with probe
// specular on (`ctrl.w` 8192, ADR-1103), the probe light in the GGX lobe of `perceptual`.
fn engine_gi(frag: vec2<f32>, p: vec3<f32>, n: vec3<f32>, v: vec3<f32>, eye: vec3<f32>, perceptual: f32) -> EngineGi {{
    if ((ddgi.ctrl.w & 1u) == 0u) {{
        return EngineGi(vec4<f32>(0.0), vec3<f32>(0.0), false);
    }}
    // The screen lookup only: the terrain is always in the depth prepass, and leaving the
    // per-pixel probe gather out keeps this (large) fragment shader's registers down. Without
    // the lookup (MSAA, no prepass) the terrain keeps its analytic terms.
    if ((ddgi.ctrl.w & 8192u) != 0u) {{
        // The same texels give the lobe light (ADR-1377): the dominant direction of the
        // specular lobe, not the normal.
        let no_v = max(dot(n, v), 1e-3);
        let m = probe_spec_lobe(n, reflect(-v, n), no_v, perceptual * perceptual);
        let g = ddgi_screen_gi(frag, p, n, m, eye);
        return EngineGi(vec4<f32>(g.diffuse.rgb, max(g.diffuse.a, 0.0)), g.spec, true);
    }}
    let r = ddgi_screen_irradiance(frag, p, n, eye);
    return EngineGi(vec4<f32>(r.rgb, max(r.a, 0.0)), vec3<f32>(0.0), false);
}}
// A baked volume's screen lookup (`regia_render::baked_gi`) holds the sky-body fill.
fn engine_gi_has_fill() -> bool {{
    return (ddgi.ctrl.w & 9u) == 9u;
}}
{}",
        &source[..start],
        regia_render::ddgi_shading_wgsl(),
        &source[end..]
    )
}

/// `source` (terrain.wgsl) with the engine's clustered point and spot lights
/// (`regia_render::shared_local_lights_wgsl`, group 3 from binding 10) in place of the
/// `engine_local_lights` stub between the `@engine-lights` markers. If the engine code fails to
/// compose (an engine shader edit broke it) the stub stays and the error is logged: the terrain
/// then takes no local light.
#[must_use]
pub fn with_engine_lights(source: &str) -> String {
    let (Some(start), Some(end)) = (
        source.find("// @engine-lights-begin"),
        source.find("// @engine-lights-end"),
    ) else {
        return source.to_owned();
    };
    // The lights' IES profiles too (ADR-3247): the frame's emitter atlas at `ENGINE_LIGHT_PROFILES_BINDING`.
    match regia_render::shared_local_lights_profiled_wgsl(
        3,
        ENGINE_LIGHTS_BINDING,
        ENGINE_LIGHT_PROFILES_BINDING,
    ) {
        // The froxels and the shadow atlas are local; the terrain shades in its own space.
        Ok(lights) => format!(
            "{}{}\n{}\n{}",
            &source[..start],
            lights.replace("fn engine_local_lights(", "fn engine_local_lights_local("),
            "fn engine_local_lights(pix: vec2<f32>, p: vec3<f32>, n: vec3<f32>, n_geo: vec3<f32>, v: vec3<f32>, \
             diffuse_color: vec3<f32>, f0: vec3<f32>, perceptual: f32) -> vec3<f32> {\n    \
             return engine_local_lights_local(pix, p - engine_csm.origin.xyz, n, n_geo, v, diffuse_color, f0, perceptual);\n}",
            &source[end..]
        ),
        Err(err) => {
            tracing::warn!(target: "regia_terrain", "terrain local lights off: {err}");
            source.to_owned()
        }
    }
}

/// `source` without the ground look (ADR-1452): every line from a `// @ground-look-begin` line
/// to the next `// @ground-look-end` line removed, and each `// @ground-look-off <code>` line
/// comment made `<code>` (the off variant of a statement the look changes). The shared pipelines
/// compile this unless some ground-look row is non-zero: the look's code, compiled in but never
/// taken, cost the terrain forward pass 0.3-0.45 ms on the RTX 5090. A source without markers,
/// or with unbalanced ones, comes back unchanged (the full source draws the same pixels).
#[must_use]
pub fn without_ground_look(source: &str) -> String {
    strip_marked(
        source,
        "// @ground-look-begin",
        "// @ground-look-end",
        "// @ground-look-off ",
    )
}

/// `source` without the surface weather (spec 99 § 6.4, the ADR-1452 mechanism): every line
/// between the `// @surface-weather-begin` and `-end` markers removed. The dry frame keeps today's
/// pipeline and bytes; only the shared colour pass of a wet, snowy or rainy frame compiles them.
#[must_use]
pub fn without_surface_weather(source: &str) -> String {
    strip_marked(
        source,
        "// @surface-weather-begin",
        "// @surface-weather-end",
        "// @surface-weather-off ",
    )
}

/// `source` without the engine cloud map's reads (ADR-7886): every line between the
/// `// @cloud-map-begin` and `-end` markers removed and each `// @cloud-map-off <code>` line made
/// code (read as [`without_ground_look`] reads its own). The shared colour pipelines of a terrain
/// drawn without a cloud map ([`TerrainGpu::set_engine_cloud_shadow`] with `None`) compile this:
/// every removed read sat behind the map's switch, so they draw the same pixels; compiled in but
/// never taken, it cost the terrain forward pass on an RTX 5090 together with the footprint path
/// ([`without_aniso_footprint`]).
#[must_use]
pub fn without_cloud_map(source: &str) -> String {
    strip_marked(
        source,
        "// @cloud-map-begin",
        "// @cloud-map-end",
        "// @cloud-map-off ",
    )
}

/// `source` without the anisotropic footprint path (ADR-7886): `terrain.wgsl`'s
/// `aniso_footprint` becomes `return false;` (the `// @aniso-footprint-begin` / `-end` / `-off`
/// markers, read as [`without_ground_look`] reads its own). The shared colour pipelines of a
/// terrain whose footprint path is off ([`TerrainGpu::set_anisotropy_footprint`]) compile this:
/// the test reads the frame uniform, which was 0 there anyway, so they draw the same pixels; the
/// footprint loop `layer_tap` carried beside every texture tap site, never taken, cost an RTX
/// 5090 terrain forward time.
#[must_use]
pub fn without_aniso_footprint(source: &str) -> String {
    strip_marked(
        source,
        "// @aniso-footprint-begin",
        "// @aniso-footprint-end",
        "// @aniso-footprint-off ",
    )
}

/// `source` without the terrain cost debug views (ADR-1491): `terrain.wgsl`'s `cost_off` and
/// `ab_off` become `return false;` (the `// @cost-views-begin` / `-end` / `-off` markers, read as
/// [`without_ground_look`] reads its own). Both are false for every view outside
/// [`cost_view_needs_full_shader`], so the stripped shader draws the same pixels there; testing
/// the view inside every pixel's layer loop cost the terrain forward pass 0.026 ms on the RTX
/// 5090. The shared colour pipelines compile this unless one of those views is on.
#[must_use]
pub fn without_cost_views(source: &str) -> String {
    strip_marked(
        source,
        "// @cost-views-begin",
        "// @cost-views-end",
        "// @cost-views-off ",
    )
}

/// Whether debug view `view` (`TerrainFrame::debug_view`) needs the shared colour pass's full
/// shader: the cost views 78-92 (`terrain.wgsl` `cost_off` / `ab_off`, compiled out by
/// [`without_cost_views`] otherwise). ADR-1491.
#[must_use]
pub fn cost_view_needs_full_shader(view: u32) -> bool {
    (78..=92).contains(&view)
}

/// Every line from a `begin` line to the next `end` line removed, and each `off <code>` line
/// comment made `<code>`; unbalanced or nested markers leave `source` unchanged.
fn strip_marked(source: &str, begin: &str, end: &str, off: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut inside = false;
    for line in source.split_inclusive('\n') {
        let code = line.trim_start();
        if code.starts_with(begin) {
            if inside {
                return source.to_owned();
            }
            inside = true;
        } else if code.starts_with(end) {
            if !inside {
                return source.to_owned();
            }
            inside = false;
        } else if !inside {
            match code.strip_prefix(off) {
                Some(kept) => {
                    out.push_str(&line[..line.len() - code.len()]);
                    out.push_str(kept);
                }
                None => out.push_str(line),
            }
        }
    }
    if inside { source.to_owned() } else { out }
}

/// The shared colour pass's terrain source: `terrain.wgsl` (with the hardware-filtered height
/// read when `hw_height`), the engine's irradiance probes, and the engine's point and spot
/// lights when `local_lights` (the engine-CSM group then carries their bindings). The ground
/// look is in it; the shared pipelines compile [`without_ground_look`] of it while the look is
/// off (ADR-1452).
#[must_use]
pub fn shared_source(hw_height: bool, local_lights: bool) -> String {
    let base = if hw_height {
        hw_height_source()
    } else {
        include_str!("terrain.wgsl").to_owned()
    };
    let source = with_engine_gi(&base);
    if local_lights {
        with_engine_lights(&source)
    } else {
        source
    }
}

/// Whether a device with `limits` and `features` can bind the engine's point and spot lights in
/// the terrain's shared colour pass: the fragment stage already holds the holes and probe-data
/// storage buffers (plus the virtual texture's page table and feedback when the device can run
/// it) and ten uniform buffers (eleven with the virtual texture); the lights add
/// `regia_render::SHARED_LIGHTS_STORAGE_BUFFERS` and `SHARED_LIGHTS_UNIFORM_BUFFERS`. The
/// min-spec Mobile and Pascal profiles (8 storage, 12 uniform) fit exactly.
#[must_use]
pub fn local_lights_fit(limits: &wgpu::Limits, features: wgpu::Features) -> bool {
    let vt = features.contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST)
        && limits.max_bind_groups >= regia_render::TERRAIN_SHARED_BIND_GROUPS;
    let storage = 2 + if vt { 2 } else { 0 } + regia_render::SHARED_LIGHTS_STORAGE_BUFFERS;
    let uniform = 10 + u32::from(vt) + regia_render::SHARED_LIGHTS_UNIFORM_BUFFERS;
    limits.max_storage_buffers_per_shader_stage >= storage
        && limits.max_uniform_buffers_per_shader_stage >= uniform
}

/// `terrain.wgsl` with the terrain budget's hardware-filtered height read (bindings 18/19)
/// in place of the manual fallback between the `@hw-height` markers. Used when the device
/// has `FLOAT32_FILTERABLE`.
#[must_use]
pub fn hw_height_source() -> String {
    let base = include_str!("terrain.wgsl");
    let (Some(start), Some(end)) = (base.find("// @hw-height-begin"), base.find("// @hw-height-end")) else {
        return base.to_owned();
    };
    let hw = "@group(1) @binding(18) var height_filtered: texture_2d<f32>;
@group(1) @binding(19) var height_filter: sampler;

// Height from the pyramid with hardware trilinear filtering (terrain budget): level k's
// texel centres sit where the manual pyramid puts them on power-of-two maps.
fn height_hw(xz: vec2<f32>, m: f32) -> f32 {
    let uv = (sample_coord(xz) + vec2<f32>(0.5)) / vec2<f32>(level_dims(0u));
    let h = textureSampleLevel(height_filtered, height_filter, uv, m).r;
    return h * terrain.height_scale + terrain.height_offset;
}
";
    format!("{}{}{}", &base[..start], hw, &base[end..])
}

/// Where one per-pass draw list sits in the slot buffer: `plain` nodes drawn with the
/// shared grid from `start`, then `len - plain` nodes cut by holes.
#[derive(Clone, Copy, Debug, Default)]
struct DrawList {
    start: u32,
    /// Nodes drawn with the regular grid (`start..start + plain`).
    plain: u32,
    /// Nodes drawn with the fine grid, after the plain ones.
    dense: u32,
    len: u32,
}

/// Index buffer of a `g × g`-cell node grid (`(g + 1)²` vertices, the shader decodes
/// `vertex_index` with the node's grid size), keeping every `stride`-th vertex. Each cell is
/// split along the `(gx, gz+1)`–`(gx+1, gz)` diagonal (`lod_error::rendered_height`).
fn grid_indices(d: &wgpu::Device, label: &str, g: u32, stride: u32) -> (wgpu::Buffer, u32) {
    let line = g + 1;
    let stride = stride.max(1);
    let mut indices = Vec::with_capacity(((g / stride).pow(2) * 6) as usize);
    for gz in (0..g).step_by(stride as usize) {
        for gx in (0..g).step_by(stride as usize) {
            let v00 = gz * line + gx;
            let v01 = (gz + stride) * line + gx;
            let v10 = v00 + stride;
            let v11 = v01 + stride;
            indices.extend_from_slice(&[v00, v01, v10, v10, v01, v11]);
        }
    }
    let count = indices.len() as u32;
    let buffer = upload_bytes(
        d,
        label,
        bytemuck::cast_slice(&indices),
        wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
    );
    (buffer, count)
}

/// `source` as this device compiles it: without `Features::SHADER_EARLY_DEPTH_TEST` the
/// virtual texture's `@early_depth_test(force)` is dropped (naga rejects the attribute). The
/// entry point then runs only with `late_depth_fallback`, depth tested after the shader
/// (`VtDepthTest::Late`, ADR-2122).
fn device_source(device: &wgpu::Device, source: String) -> String {
    if device
        .features()
        .contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST)
    {
        source
    } else {
        source.replace("@early_depth_test(force)", "")
    }
}

/// `entries` with the compute stage added to each one's visibility.
fn with_compute(mut entries: Vec<wgpu::BindGroupLayoutEntry>) -> Vec<wgpu::BindGroupLayoutEntry> {
    for entry in &mut entries {
        entry.visibility |= wgpu::ShaderStages::COMPUTE;
    }
    entries
}

/// A read-only storage binding of the node group (vertex stage).
fn node_storage_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// A `STORAGE | COPY_DST` buffer of `size` bytes (at least 4).
fn make_node_storage(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(4),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// The node group: every node, then the concatenated draw lists.
fn node_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    nodes: &wgpu::Buffer,
    slots: &wgpu::Buffer,
    holes: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("terrain node bind"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: nodes.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: slots.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: holes.as_entire_binding(),
            },
        ],
    })
}

/// Compilation options of a colour / receiver fragment stage: `holes` sets `terrain.wgsl`'s
/// `hole_cut` override, so the entry point discards inside a hole. Without it the constant is
/// false and the stage is the plain one (the cut lives in the entry point itself: a shared
/// helper called from a second `*_holes` entry point cost the forward pass 0.09 ms).
fn hole_cut_options(holes: bool) -> wgpu::PipelineCompilationOptions<'static> {
    const CUT: &[(&str, f64)] = &[("hole_cut", 1.0)];
    wgpu::PipelineCompilationOptions {
        constants: if holes { CUT } else { &[] },
        ..Default::default()
    }
}

/// The offscreen (lab) pipelines of [`TerrainGpu::new`]: the same for every terrain on a
/// device, so they are built once per device ([`regia_render::RenderDevice::shared_pipelines`]).
#[derive(Clone)]
struct OffscreenPipelines {
    terrain_shader: wgpu::ShaderModule,
    terrain_pipeline_layout: wgpu::PipelineLayout,
    terrain_pipeline: wgpu::RenderPipeline,
    scatter_pipeline: wgpu::RenderPipeline,
    shadow_pipeline: wgpu::RenderPipeline,
    water_pipeline: wgpu::RenderPipeline,
    sky_pipeline: wgpu::RenderPipeline,
}

/// The offscreen hole-cut terrain pipeline, once per device and terrain shader source.
fn offscreen_holes_pipeline(
    shared: &regia_render::device_pipelines::DevicePipelines,
    device: &wgpu::Device,
    terrain_source: &str,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
) -> wgpu::RenderPipeline {
    shared.get_or_create(
        regia_render::device_pipelines::pipeline_key(&("regia_terrain offscreen holes", terrain_source)),
        || offscreen_terrain_pipeline(device, layout, shader, true),
    )
}

/// The offscreen terrain colour pipeline (`Rgba8UnormSrgb`, `Depth32Float`); `holes` cuts in
/// `fs_main` (`hole_cut`).
fn offscreen_terrain_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    holes: bool,
) -> wgpu::RenderPipeline {
    let tracked_device = regia_render::TrackedDevice::observe(device);
    let device = &tracked_device;
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(if holes { "terrain (holes)" } else { "terrain" }),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: hole_cut_options(holes),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        multiview_mask: None,
        cache: regia_render::pipeline_cache::cache_for(device).as_deref(),
    })
}

/// The hole storage buffer `terrain.wgsl`'s `in_hole` reads ([`HolesMask::gpu_words`]).
/// Appends the mouth bands to the hole words (ADR-8569) and returns the byte offset of band set
/// 0's positions (0 without bands). Header words 1-3 (unused by the windows) become the band's
/// word offset, its corner count and its vertex count; then the corners (vertex indices), then
/// [`BAND_CAMERAS`] × [`BAND_PASSES`] sets of `xyz` positions (f32 bits), each at the cook
/// positions (camera `c`'s set `p` is set `c * BAND_PASSES + p`, ADR-8824).
fn append_band_words(words: &mut Vec<u32>, bands: Option<&crate::mesh_layer::MouthBands>) -> u64 {
    let Some(bands) = bands.filter(|b| !b.corners.is_empty() && !b.vertices.is_empty()) else {
        return 0;
    };
    if words.len() < 4 {
        words.resize(4, 0);
    }
    let start = words.len() as u32;
    words[1] = start;
    words[2] = bands.corners.len() as u32;
    words[3] = bands.vertices.len() as u32;
    words.extend_from_slice(&bands.corners);
    let at = (words.len() * 4) as u64;
    let cook = bands.place_with(&bands.ring_points);
    for _ in 0..BAND_CAMERAS * BAND_PASSES {
        words.extend(cook.iter().flatten().map(|f| f.to_bits()));
    }
    at
}

fn holes_storage(device: &wgpu::Device, mask: Option<&HolesMask>) -> wgpu::Buffer {
    let words = mask.map_or_else(|| vec![0u32; 4], HolesMask::gpu_words);
    upload_bytes(
        device,
        "terrain holes",
        bytemuck::cast_slice(&words),
        wgpu::BufferUsages::STORAGE,
    )
}

/// One shared pass's pipelines ([`TerrainGpu::ensure_shared`]).
#[derive(Clone)]
struct SharedPipelines {
    depth: wgpu::RenderPipeline,
    color: wgpu::RenderPipeline,
    shadow: Option<wgpu::RenderPipeline>,
    velocity: wgpu::RenderPipeline,
    receiver: wgpu::RenderPipeline,
}

fn upload_bytes(
    device: &wgpu::Device,
    label: &str,
    contents: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: contents.len() as u64,
        usage,
        mapped_at_creation: true,
    });
    buffer
        .slice(..)
        .get_mapped_range_mut()
        .expect("created with mapped_at_creation")
        .copy_from_slice(contents);
    buffer.unmap();
    buffer
}

/// A headless [`RenderDevice`] that additionally enables `TEXTURE_COMPRESSION_BC` when the
/// adapter has it (the stock headless device does not request it). The cooked KTX2/BCn path
/// samples the blocks natively with this feature; on adapters without it the blocks are
/// decoded to RGBA8 at upload.
///
/// This is the lane's one small seam over `regia_render` (which the render lanes own): it
/// assembles a device from parts instead of editing the shared constructor.
///
/// One device per process, created on first use and cloned per call, on the adapter of
/// [`regia_render::RenderDevice::shared_headless`] (ADR-7557). Each call used to open a stock
/// device and drop it, then open the BC device, which every test calling this dropped at its
/// end: a device destroyed while other test threads named objects, the Vulkan loader list race of
/// ADR-2113. A failed creation is not kept; the next call tries again.
///
/// # Errors
///
/// Returns [`regia_render::RenderError`] when no adapter/device can be created.
pub fn headless_bcn_device() -> Result<regia_render::RenderDevice, regia_render::RenderError> {
    static SHARED: std::sync::Mutex<Option<regia_render::RenderDevice>> = std::sync::Mutex::new(None);
    // A poisoned lock only means a caller panicked while holding it; the slot is still valid.
    let mut slot = SHARED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(device) = slot.as_ref() {
        return Ok(device.clone());
    }
    // The shared stock device stays alive in its own slot: nothing is destroyed here.
    let base = regia_render::RenderDevice::shared_headless()?;
    let device = if base
        .adapter
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
        && !base
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
    {
        let (device, queue) = regia_render::request_device_locked(
            &base.adapter,
            &wgpu::DeviceDescriptor {
                label: Some("regia terrain bcn device"),
                required_limits: base.adapter.limits(),
                // Keep what the base device enabled (timestamp queries for GPU timings,
                // …): asking for BC alone dropped them, and every pass timed 0 ms.
                required_features: base.device.features() | wgpu::Features::TEXTURE_COMPRESSION_BC,
                ..Default::default()
            },
        )
        .map_err(regia_render::RenderError::RequestDevice)?;
        let capabilities = regia_render::RenderCapabilities::from_adapter(&base.adapter, &device);
        regia_render::RenderDevice::from_parts(base.instance, base.adapter, device, queue, capabilities)
    } else {
        base
    };
    *slot = Some(device.clone());
    Ok(device)
}

/// Renders one frame of terrain + scatter + water into an offscreen `Rgba8UnormSrgb` target
/// and reads the pixels back (tightly packed RGBA8, row-major, top row first).
///
/// This is the reference consumer of the [`TerrainRenderer`] hook and the test/sample path.
/// With a water plane and [`TerrainGpu::water_refraction`] on, the water draws in a second
/// pass over a copy of the opaque frame and its depth (screen-space refraction, ADR-1237).
///
/// # Errors
///
/// Returns [`TerrainError::Gpu`] on device errors.
#[tracing::instrument(skip_all, target = "regia_terrain")]
pub fn render_offscreen(
    gpu: &mut TerrainGpu,
    device: &RenderDevice,
    frame: &TerrainFrame,
    selection: &LodSelection,
    width: u32,
    height: u32,
    clear: [f64; 4],
) -> Result<Vec<u8>, TerrainError> {
    render_offscreen_opts(gpu, device, frame, selection, width, height, clear, true)
}

/// [`render_offscreen`] with the sky pass optional. Without the sky, uncovered pixels keep
/// the clear colour — the crack-detection tests use a magenta clear and require that no
/// background leaks through the terrain's interior.
///
/// # Errors
///
/// Returns [`TerrainError::Gpu`] on device errors.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, target = "regia_terrain")]
pub fn render_offscreen_opts(
    gpu: &mut TerrainGpu,
    device: &RenderDevice,
    frame: &TerrainFrame,
    selection: &LodSelection,
    width: u32,
    height: u32,
    clear: [f64; 4],
    draw_sky: bool,
) -> Result<Vec<u8>, TerrainError> {
    let d = &device.device;
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let colour = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("terrain offscreen color"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.colour_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let colour_view = colour.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("terrain offscreen depth"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.depth_format(),
        // Sampled by the water's refraction (read-only attachment in that pass).
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    gpu.update_frame(frame);
    gpu.upload_nodes(selection);

    // Screen-space refraction: the water pass reads a copy of the opaque frame and its depth.
    let refraction = (gpu.has_water() && gpu.water_refraction()).then(|| {
        let copy = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("terrain offscreen scene copy"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.colour_format(),
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let copy_view = copy.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_read = depth.create_view(&wgpu::TextureViewDescriptor {
            label: Some("terrain offscreen depth (water refraction)"),
            aspect: wgpu::TextureAspect::DepthOnly,
            ..Default::default()
        });
        gpu.bind_water_scene(Some((&copy_view, &depth_read)));
        copy
    });

    let row = width * 4;
    let padded_row = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("terrain readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("terrain offscreen"),
    });

    // Sun cascade. Cleared to 1 (far) so a frame with no casters stays fully lit.
    gpu.encode_sun_shadow(&mut encoder);

    // Pre-pass: full-screen sky gradient (no depth write).
    if draw_sky {
        let mut sky_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("sky offscreen pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &colour_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: clear[0],
                        g: clear[1],
                        b: clear[2],
                        a: clear[3],
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.draw_sky(&mut sky_pass);
    } else {
        // An empty pass that applies the caller's clear colour (no sky gradient).
        let clear_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terrain offscreen clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &colour_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: clear[0],
                        g: clear[1],
                        b: clear[2],
                        a: clear[3],
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        drop(clear_pass);
    }
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terrain offscreen pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &colour_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.draw_terrain(&mut pass, selection);
        gpu.draw_scatter(&mut pass);
        if refraction.is_none() {
            gpu.draw_water(&mut pass);
        }
    }
    if let Some(copy) = &refraction {
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &colour,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: copy,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            size,
        );
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terrain offscreen water pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &colour_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            // Read-only: the water samples it.
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: None,
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.draw_water(&mut pass);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &colour,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        size,
    );
    device.queue.submit([encoder.finish()]);
    if refraction.is_some() {
        // Back to the stand-ins: the scene copy and this depth belong to this call.
        gpu.bind_water_scene(None);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(regia_render::RenderError::Poll)?;
    rx.recv()
        .map_err(|_| TerrainError::Gpu(regia_render::RenderError::Readback("map callback dropped".into())))?
        .map_err(|e| TerrainError::Gpu(regia_render::RenderError::Readback(e.to_string())))?;
    let mapped = readback
        .slice(..)
        .get_mapped_range()
        .map_err(|e| TerrainError::Gpu(regia_render::RenderError::Readback(e.to_string())))?;
    let mut pixels = Vec::with_capacity((row * height) as usize);
    for y in 0..height as usize {
        let start = y * padded_row as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    drop(mapped);
    readback.unmap();
    Ok(pixels)
}

/// Contract rows of the terrain's uniform structs (`regia_render::ContractRow`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<regia_render::ContractRow> {
    use regia_render::contract_row;
    vec![
        // ADR-5802: sky, scatter and water bind the same frame buffer but declare its first nine
        // members only (240 of 448 bytes); a binding larger than the shader's struct is valid.
        contract_row!(
            "sky.wgsl,scatter.wgsl,water.wgsl",
            "Frame",
            FrameUniform {
                view_proj,
                camera_pos_time,
                sun_dir_wetness,
                sun_color_exposure,
                ambient_debug,
                fog_params,
                fog_color,
                light_view_proj,
                shadow_params
            }
        )
        .host_may_extend(),
        contract_row!(
            "water.wgsl",
            "Terrain",
            TerrainUniform {
                half_width,
                world_width,
                height_scale,
                height_offset,
                sample_max,
                grid_size,
                root_level,
                mip_info,
                depth
            }
        ),
        contract_row!(
            "terrain.wgsl",
            "Frame",
            FrameUniform {
                view_proj,
                camera_pos_time,
                sun_dir_wetness,
                sun_color_exposure,
                ambient_debug,
                fog_params,
                fog_color,
                light_view_proj,
                shadow_params,
                output_mode,
                prev_view_proj,
                unjittered_view_proj,
                ground_ambient,
                fill_dir,
                fill_color,
                texture_lod,
                extra_dir
            }
        ),
        contract_row!(
            "terrain.wgsl",
            "Terrain",
            TerrainUniform {
                half_width,
                world_width,
                height_scale,
                height_offset,
                sample_max,
                grid_size,
                root_level,
                mip_info,
                depth
            }
        ),
        contract_row!("", "Layers", LayersUniform { count, slots }),
        contract_row!(
            "",
            "LayerSlot",
            LayerSlotUniform {
                albedo_roughness,
                slope_alt,
                params,
                params2,
                params3
            }
        ),
        contract_row!(
            "terrain.wgsl",
            "Material",
            MaterialUniform {
                params0,
                params1,
                sim,
                tiles,
                road0,
                road1,
                strata,
                relief,
                strata2,
                rock,
                cliff,
                ground,
                winter,
                winter2,
                macro_mask,
                macro_prefix,
                weather,
                weather2,
                weather_tile,
                weather_rect,
                weather_bias
            }
        ),
        contract_row!("terrain.wgsl", "ScorchUniform", ScorchUniform { rows }),
        contract_row!(
            "terrain.wgsl",
            "Node",
            NodeUniform {
                rect_min,
                rect_size,
                level,
                morph_start,
                morph_end,
                grid_size,
                root_level
            }
        ),
        contract_row!(
            "terrain.wgsl",
            "CascadeVp",
            CascadeUniform {
                light_view_proj,
                morph_eye
            }
        ),
        contract_row!(
            "terrain.wgsl",
            "EngineCsm",
            EngineCsmUniform {
                vp0,
                vp1,
                vp2,
                vp3,
                split,
                bias0,
                bias1,
                bias2,
                bias3,
                params,
                screen,
                cloud,
                origin
            }
        ),
        contract_row!(
            "water.wgsl",
            "Water",
            WaterUniform {
                rect_min,
                rect_size,
                level,
                shallow,
                deep,
                foam,
                wave_params,
                freq_pad
            }
        ),
    ]
}

#[cfg(test)]
mod vt_key_tests {
    use super::{VT_SUN_TOLERANCE_RAD, vt_key_sun};

    fn turn(sun: [f32; 3], rad: f32) -> [f32; 3] {
        // Rotate about +Z (the sun's elevation changes).
        let (s, c) = rad.sin_cos();
        [sun[0] * c - sun[1] * s, sun[0] * s + sun[1] * c, sun[2]]
    }

    #[test]
    fn a_jittering_sun_keeps_the_key_and_a_real_turn_moves_it() {
        let base = [0.3f32, -0.8, 0.52];
        let mut held = Some(vt_key_sun(None, base));
        // Per-frame jitter of ±1e-4 rad around the base, plus float noise in the length.
        let mut seed = 0x1234_5678u32;
        for _ in 0..300 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let j = (seed % 2001) as f32 / 1000.0 - 1.0;
            let sun = turn(base, 1e-4 * j).map(|v| v * (1.0 + 1e-4 * j));
            let next = vt_key_sun(held, sun);
            assert_eq!(Some(next), held, "jitter moved the key");
            held = Some(next);
        }
        // A 2° turn moves it to the new sun.
        let far = turn(base, 2f32.to_radians());
        assert_eq!(vt_key_sun(held, far), far);
        // Just past the tolerance moves it; just under keeps it.
        assert_eq!(
            vt_key_sun(held, turn(base, VT_SUN_TOLERANCE_RAD * 1.5)),
            turn(base, VT_SUN_TOLERANCE_RAD * 1.5)
        );
        assert_eq!(vt_key_sun(held, turn(base, VT_SUN_TOLERANCE_RAD * 0.5)), base);
        // A degenerate sun never keeps a stale key.
        assert_eq!(vt_key_sun(held, [0.0; 3]), [0.0; 3]);
    }
}

#[cfg(test)]
mod ktx2_extent_tests {
    use super::ktx2_extent_fits;

    #[test]
    fn extent_must_be_non_zero_and_within_the_device_limit() {
        assert!(ktx2_extent_fits(4096, 4096, 8192));
        assert!(ktx2_extent_fits(8192, 1, 8192));
        assert!(!ktx2_extent_fits(0, 4, 8192));
        assert!(!ktx2_extent_fits(16384, 16384, 8192));
        assert!(!ktx2_extent_fits(4, 8193, 8192));
    }
}

#[cfg(test)]
mod cloud_shadow_tests {
    /// The shared colour pass reads the cloud transmittance map with the engine's own lookup
    /// (ADR-320): `terrain.wgsl` holds the text of `regia_render::cloud_shadow_wgsl()` verbatim.
    #[test]
    fn terrain_cloud_lookup_matches_the_engine() {
        let terrain = include_str!("terrain.wgsl");
        assert!(
            terrain.contains(regia_render::cloud_shadow_wgsl()),
            "terrain.wgsl's cloud lookup differs from regia_render's cloud_shadow.wgsl"
        );
        assert!(terrain.contains(&format!(
            "@binding({}) var engine_cloud",
            super::ENGINE_CLOUD_BINDING
        )));
    }

    /// `detail_normal_strength` is authored in 0..=2. The shade clamps to that range.
    #[test]
    fn detail_normal_strength_clamps_to_the_authored_range() {
        let terrain = include_str!("terrain.wgsl");
        let needle = "clamp(slot.params2.x, 0.0, 2.0)";
        assert_eq!(terrain.matches(needle).count(), 1, "one detail-normal clamp");
        assert!(
            !terrain.contains("clamp(slot.params2.x, 0.0, 1.0)"),
            "the 0..=1 clamp is gone"
        );
    }

    /// The terrain's surface rule is the forward pass's (spec 99 § 6.4): `terrain.wgsl` holds the
    /// text of `regia_render::surface_weather_wgsl()` verbatim, inside the markers.
    #[test]
    fn terrain_surface_weather_matches_the_engine() {
        let terrain = include_str!("terrain.wgsl");
        assert!(
            terrain.contains(regia_render::surface_weather_wgsl()),
            "terrain.wgsl's surface rule differs from regia_render's forward_surface_weather.wgsl"
        );
        assert!(terrain.contains(&format!(
            "@binding({}) var engine_precip",
            super::ENGINE_PRECIP_BINDING
        )));
        let stripped = super::without_surface_weather(terrain);
        assert!(!stripped.contains("weather_apply"));
        assert!(!stripped.contains("engine_precip"));
    }
}

#[cfg(test)]
mod relief_tests {
    use super::TerrainRelief;

    #[test]
    fn layer_occlusion_packs_as_one_minus_strength() {
        // A zeroed uniform (and the default) keeps the layers' occlusion as authored.
        assert_eq!(TerrainRelief::default().uniforms().3[3], 0.0);
        let preview = TerrainRelief {
            layer_occlusion: 0.3,
            ..TerrainRelief::default()
        };
        assert!((preview.uniforms().3[3] - 0.7).abs() < 1e-6);
        let bad = TerrainRelief {
            layer_occlusion: f32::NAN,
            ..TerrainRelief::default()
        };
        assert_eq!(bad.uniforms().3[3], 0.0);
    }
}

#[cfg(test)]
mod bcn_fallback_tests {
    use super::GpuArrayTexture;

    /// Serializes these tests: each creates and drives its own `wgpu::Device` on a parallel
    /// test thread, and concurrent device creation has deadlocked the NVIDIA driver (the
    /// merge gate's GPU pass timed out in here roughly one rerun in three; the intel-arc
    /// sweep saw the same class as crashes in the Vulkan loader). `regia_render` already
    /// serializes `Instance::new`, but three parallel `request_device`s followed by submits
    /// and `poll` on each device is still enough to wedge the driver. The game only ever
    /// creates one device, so this lock only ever affects tests. It is held for the whole
    /// test body: creation, use and drop never overlap. Since ADR-7557 the devices are opened
    /// once per process and never dropped mid-run (`stock_device`, `kept_device_without`); the
    /// lock still keeps these tests' submits and polls apart.
    static ONE_DEVICE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes [`ONE_DEVICE_AT_A_TIME`] for the rest of the test.
    fn serialize() -> std::sync::MutexGuard<'static, ()> {
        // A poisoned lock only means another of these tests panicked; testing on is safe.
        ONE_DEVICE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An 8×8 BC1 sRGB KTX2 blob with two levels whose blocks are all solid `color0` (565).
    fn bc1_ktx2(color0: u16) -> Vec<u8> {
        // color0 > color1 = 0 selects 4-colour mode; all indices 0 pick color0.
        let mut block = color0.to_le_bytes().to_vec();
        block.extend_from_slice(&[0; 6]);
        ktx2_blob(ktx2::Format::BC1_RGBA_SRGB_BLOCK, &[block.repeat(4), block])
    }

    /// An 8×8 ASTC 4×4 sRGB KTX2 blob (levels 8×8, 4×4, 2×2, 1×1) of solid `rgba16` blocks:
    /// the ASTC void-extent block (Khronos Data Format Specification, ASTC § "Void-extent
    /// blocks": block mode `0x1FC`, LDR, no extent, then the colour as four UNORM16).
    fn astc_ktx2(rgba16: [u16; 4]) -> Vec<u8> {
        let mut block = 0xFFFF_FFFF_FFFF_FDFCu64.to_le_bytes().to_vec();
        for c in rgba16 {
            block.extend_from_slice(&c.to_le_bytes());
        }
        ktx2_blob(
            ktx2::Format::ASTC_4x4_SRGB_BLOCK,
            &[block.repeat(4), block.clone(), block.clone(), block],
        )
    }

    /// An 8×8 KTX2 blob of `format` holding `levels` (level 0 first).
    fn ktx2_blob(format: ktx2::Format, levels: &[Vec<u8>]) -> Vec<u8> {
        let (basic, type_size) = ktx2::dfd::Basic::from_format(format).expect("dfd");
        let dfd_body = ktx2::dfd::Block::Basic(basic).to_vec();
        let dfd_total_len = 4u32 + dfd_body.len() as u32;
        let dfd_offset = ktx2::Header::LENGTH + levels.len() * ktx2::LevelIndex::LENGTH;
        let data_offset = dfd_offset + dfd_total_len as usize;
        let header = ktx2::Header {
            format: Some(format),
            type_size,
            pixel_width: 8,
            pixel_height: 8,
            pixel_depth: 0,
            layer_count: 0,
            face_count: 1,
            level_count: levels.len() as u32,
            supercompression_scheme: None,
            index: ktx2::Index {
                dfd_byte_offset: dfd_offset as u32,
                dfd_byte_length: dfd_total_len,
                kvd_byte_offset: data_offset as u32,
                kvd_byte_length: 0,
                sgd_byte_offset: 0,
                sgd_byte_length: 0,
            },
        };
        let mut out = header.as_bytes().to_vec();
        let mut offset = data_offset as u64;
        for level in levels {
            let len = level.len() as u64;
            out.extend_from_slice(
                &ktx2::LevelIndex {
                    byte_offset: offset,
                    byte_length: len,
                    uncompressed_byte_length: len,
                }
                .as_bytes(),
            );
            offset += len;
        }
        out.extend_from_slice(&dfd_total_len.to_le_bytes());
        out.extend_from_slice(&dfd_body);
        for level in levels {
            out.extend_from_slice(level);
        }
        out
    }

    /// The process's stock test device (ADR-7557): a device of a test's own would be dropped at
    /// its end while other test threads name objects (the loader race of ADR-2113).
    fn stock_device() -> regia_render::RenderDevice {
        regia_render::RenderDevice::shared_headless()
            .unwrap_or_else(|err| panic!("no headless GPU device (lavapipe?): {err}"))
    }

    /// A device on the stock device's adapter with its features minus `removed`, opened once per
    /// process under the engine's loader-list lock and kept until the process exits. It is its
    /// own device because a device's feature set is fixed at creation; kept, so it is never
    /// destroyed while other test threads name objects (ADR-7557, the loader race of ADR-2113).
    fn kept_device_without(
        slot: &'static std::sync::Mutex<Option<(wgpu::Device, wgpu::Queue)>>,
        label: &'static str,
        removed: wgpu::Features,
    ) -> (wgpu::Device, wgpu::Queue) {
        // A poisoned lock only means a test panicked while holding it; the slot is still valid.
        let mut slot = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(kept) = slot.as_ref() {
            return kept.clone();
        }
        let base = stock_device();
        let mut features = base.device.features();
        features.remove(removed);
        let opened = regia_render::request_device_locked(
            &base.adapter,
            &wgpu::DeviceDescriptor {
                label: Some(label),
                required_limits: base.device.limits(),
                required_features: features,
                ..Default::default()
            },
        )
        .unwrap_or_else(|err| panic!("{label}: {err}"));
        *slot = Some(opened.clone());
        opened
    }

    /// A headless device with every feature of the stock one except `TEXTURE_COMPRESSION_BC`,
    /// like a Mali or Adreno phone (one per process, kept: [`kept_device_without`]).
    fn device_without_bc() -> (wgpu::Device, wgpu::Queue) {
        static KEPT: std::sync::Mutex<Option<(wgpu::Device, wgpu::Queue)>> = std::sync::Mutex::new(None);
        kept_device_without(
            &KEPT,
            "terrain no-bc device",
            wgpu::Features::TEXTURE_COMPRESSION_BC,
        )
    }

    /// A device without any block format family (BC, ASTC, ETC2) (one per process, kept:
    /// [`kept_device_without`]).
    fn device_without_block_formats() -> (wgpu::Device, wgpu::Queue) {
        static KEPT: std::sync::Mutex<Option<(wgpu::Device, wgpu::Queue)>> = std::sync::Mutex::new(None);
        kept_device_without(
            &KEPT,
            "terrain no-block-formats device",
            wgpu::Features::TEXTURE_COMPRESSION_BC
                | wgpu::Features::TEXTURE_COMPRESSION_ASTC
                | wgpu::Features::TEXTURE_COMPRESSION_ETC2,
        )
    }

    /// The stock device, the BC device and the two feature-set devices each open once: asking
    /// again creates no instance and no device (`loader_list_lock_count` counts both).
    #[test]
    fn the_test_devices_open_once_per_process() {
        let _devices = serialize();
        let first = (
            stock_device().device_id(),
            super::headless_bcn_device()
                .unwrap_or_else(|err| panic!("device: {err}"))
                .device_id(),
        );
        drop((device_without_bc(), device_without_block_formats()));
        let before = regia_render::loader_list_lock_count();
        let again = (
            stock_device().device_id(),
            super::headless_bcn_device()
                .unwrap_or_else(|err| panic!("device: {err}"))
                .device_id(),
        );
        drop((device_without_bc(), device_without_block_formats()));
        assert_eq!(
            regia_render::loader_list_lock_count(),
            before,
            "asking for a test device again created an instance or a device"
        );
        assert_eq!(first, again);
    }

    /// An 8×8 KTX2 blob of `format` whose every level repeats one `block` (4×4 blocks).
    fn solid_ktx2(format: ktx2::Format, block: &[u8]) -> Vec<u8> {
        ktx2_blob(
            format,
            &[block.repeat(4), block.to_vec(), block.to_vec(), block.to_vec()],
        )
    }

    /// An ETC2 RGB8 / ETC1 individual-mode block: base colours 0xF, 0, 0, table 0, every texel
    /// index 0 (modifier +2), so it decodes to (255, 2, 2).
    const ETC2_RED_BLOCK: [u8; 8] = [0xFF, 0, 0, 0, 0, 0, 0, 0];

    /// Reads texel (0, 0) of `layer` at `mip` of an RGBA8 array.
    fn texel(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        mip: u32,
        layer: u32,
    ) -> [u8; 4] {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: mip,
                origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap_or_else(|err| panic!("poll: {err}"));
        let mapped = buffer
            .slice(..)
            .get_mapped_range()
            .unwrap_or_else(|err| panic!("map: {err}"));
        [mapped[0], mapped[1], mapped[2], mapped[3]]
    }

    #[test]
    fn cooked_bcn_layers_decode_to_rgba8_without_the_bc_feature() {
        let _devices = serialize();
        let (device, queue) = device_without_bc();
        assert!(!device.features().contains(wgpu::Features::TEXTURE_COMPRESSION_BC));
        let red = bc1_ktx2(0xF800);
        let blue = bc1_ktx2(0x001F);
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let array =
            GpuArrayTexture::upload_ktx2_array_from(&device, &queue, "test layers (bcn)", &[&red, &blue], 0)
                .unwrap_or_else(|err| panic!("upload: {err}"));
        let error = regia_core::tasks::block_on(scope.pop());
        assert!(error.is_none(), "validation error: {error:?}");
        assert_eq!(array.texture.format(), wgpu::TextureFormat::Rgba8UnormSrgb);
        assert_eq!(array.texture.mip_level_count(), 2);
        assert_eq!(array.texture.depth_or_array_layers(), 2);
        assert_eq!(texel(&device, &queue, &array.texture, 0, 0), [255, 0, 0, 255]);
        assert_eq!(texel(&device, &queue, &array.texture, 0, 1), [0, 0, 255, 255]);
        assert_eq!(texel(&device, &queue, &array.texture, 1, 1), [0, 0, 255, 255]);
        // The fallback is reported (warn once, render.problems): the pack was cooked for desktop.
        let wrong = regia_render::wrong_platform_textures();
        assert!(wrong.decoded >= 1 && wrong.decoded_bytes > 0, "{wrong:?}");
        assert!(
            wrong.labels.iter().any(|l| l.contains("test layers (bcn)")),
            "{wrong:?}"
        );

        // A mobile tier's mip skip drops the top level before decoding.
        let skipped = GpuArrayTexture::upload_ktx2_array_from(&device, &queue, "skip", &[&red], 1)
            .unwrap_or_else(|err| panic!("upload: {err}"));
        assert_eq!(
            (skipped.texture.width(), skipped.texture.mip_level_count()),
            (4, 1)
        );
        assert_eq!(texel(&device, &queue, &skipped.texture, 0, 0), [255, 0, 0, 255]);
    }

    #[test]
    fn devices_with_bc_keep_the_blocks() {
        let _devices = serialize();
        let device = super::headless_bcn_device().unwrap_or_else(|err| panic!("device: {err}"));
        if !device
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
        {
            return; // adapter without BC: the decode test covers it
        }
        let red = bc1_ktx2(0xF800);
        let array = GpuArrayTexture::upload_ktx2_array_from(&device.device, &device.queue, "bc", &[&red], 0)
            .unwrap_or_else(|err| panic!("upload: {err}"));
        assert_eq!(array.texture.format(), wgpu::TextureFormat::Bc1RgbaUnormSrgb);
    }

    /// An Android (ASTC) cook uploads its blocks as they are on a device with
    /// `TEXTURE_COMPRESSION_ASTC` (phones): block bytes, no RGBA8 decode, every level down to
    /// 1×1 and the tier's mip skip. Without ASTC (a desktop GPU) the array decodes to RGBA8
    /// and is reported as cooked for another platform.
    #[test]
    fn cooked_astc_layers_upload_natively_or_decode_to_rgba8() {
        let _devices = serialize();
        let device = stock_device();
        let (device, queue) = (&device.device, &device.queue);
        let red = astc_ktx2([0xFFFF, 0, 0, 0xFFFF]);
        let blue = astc_ktx2([0, 0, 0xFFFF, 0xFFFF]);
        let format = wgpu::TextureFormat::Astc {
            block: wgpu::AstcBlock::B4x4,
            channel: wgpu::AstcChannel::UnormSrgb,
        };
        if device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC)
        {
            let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let array = GpuArrayTexture::upload_ktx2_array_from(
                device,
                queue,
                "test layers (astc)",
                &[&red, &blue],
                0,
            )
            .unwrap_or_else(|err| panic!("upload: {err}"));
            let skipped = GpuArrayTexture::upload_ktx2_array_from(device, queue, "skip (astc)", &[&red], 1)
                .unwrap_or_else(|err| panic!("upload: {err}"));
            let error = regia_core::tasks::block_on(scope.pop());
            assert!(error.is_none(), "validation error: {error:?}");
            assert_eq!(array.texture.format(), format);
            assert_eq!(array.texture.mip_level_count(), 4);
            // 16-byte blocks: 4 + 1 + 1 + 1 per layer, each block row padded to 256 bytes.
            assert_eq!(array.bytes, 2 * (2 * 256 + 3 * 256));
            assert_eq!(
                (skipped.texture.width(), skipped.texture.mip_level_count()),
                (4, 3)
            );
        } else {
            let array =
                GpuArrayTexture::upload_ktx2_array_from(device, queue, "test layers (astc)", &[&red], 0)
                    .unwrap_or_else(|err| panic!("upload: {err}"));
            assert_eq!(array.texture.format(), wgpu::TextureFormat::Rgba8UnormSrgb);
            let wrong = regia_render::wrong_platform_textures();
            assert!(wrong.labels.iter().any(|l| l.contains("test layers (astc)")));
        }
    }

    /// A device with none of the block families decodes an ASTC and an ETC2 / EAC cook to RGBA8:
    /// every level, every layer, the tier's mip skip, the format's sRGB-ness, and the values the
    /// GPU would sample (a void-extent colour exactly; RG11 as `(r, g, 0, 1)`). Reported as cooked
    /// for another platform. Before the decoders this array was refused and the terrain lost
    /// its material.
    #[test]
    fn android_layers_decode_to_rgba8_without_their_block_format() {
        // `render.problems` names only the first five textures of the process and counts a label
        // once, so the uploads whose report is not the point here share the other tests' label.
        const UNREPORTED: &str = "skip";
        let _devices = serialize();
        let (device, queue) = device_without_block_formats();
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);

        let red = astc_ktx2([0xFFFF, 0, 0, 0xFFFF]);
        let blue = astc_ktx2([0, 0, 0xFFFF, 0xFFFF]);
        let astc = GpuArrayTexture::upload_ktx2_array_from(
            &device,
            &queue,
            "test layers (astc decode)",
            &[&red, &blue],
            0,
        )
        .unwrap_or_else(|err| panic!("astc upload: {err}"));
        assert_eq!(astc.texture.format(), wgpu::TextureFormat::Rgba8UnormSrgb);
        assert_eq!(astc.texture.mip_level_count(), 4);
        assert_eq!(astc.texture.depth_or_array_layers(), 2);
        assert_eq!(texel(&device, &queue, &astc.texture, 0, 0), [255, 0, 0, 255]);
        assert_eq!(texel(&device, &queue, &astc.texture, 0, 1), [0, 0, 255, 255]);
        // The cook's own bottom level, not a regenerated one.
        assert_eq!(texel(&device, &queue, &astc.texture, 3, 1), [0, 0, 255, 255]);
        let skipped = GpuArrayTexture::upload_ktx2_array_from(&device, &queue, UNREPORTED, &[&red], 1)
            .unwrap_or_else(|err| panic!("astc skip: {err}"));
        assert_eq!(
            (skipped.texture.width(), skipped.texture.mip_level_count()),
            (4, 3)
        );

        let etc2 = solid_ktx2(ktx2::Format::ETC2_R8G8B8_SRGB_BLOCK, &ETC2_RED_BLOCK);
        let colour = GpuArrayTexture::upload_ktx2_array_from(&device, &queue, UNREPORTED, &[&etc2], 0)
            .unwrap_or_else(|err| panic!("etc2 upload: {err}"));
        assert_eq!(colour.texture.format(), wgpu::TextureFormat::Rgba8UnormSrgb);
        assert_eq!(texel(&device, &queue, &colour.texture, 0, 0), [255, 2, 2, 255]);
        assert_eq!(texel(&device, &queue, &colour.texture, 3, 0), [255, 2, 2, 255]);

        // EAC RG11, the terrain's normal layout: R high, G low (base 255 and 0, multiplier 1).
        let rg = [255u8, 0x10, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0];
        let rg_blob = solid_ktx2(ktx2::Format::EAC_R11G11_UNORM_BLOCK, &rg);
        let normal = GpuArrayTexture::upload_ktx2_array_from(&device, &queue, UNREPORTED, &[&rg_blob], 0)
            .unwrap_or_else(|err| panic!("rg11 upload: {err}"));
        assert_eq!(normal.texture.format(), wgpu::TextureFormat::Rgba8Unorm);
        let px = texel(&device, &queue, &normal.texture, 0, 0);
        assert!(px[0] > 240 && px[1] < 15 && (px[2], px[3]) == (0, 255), "{px:?}");

        let error = regia_core::tasks::block_on(scope.pop());
        assert!(error.is_none(), "validation error: {error:?}");
        let wrong = regia_render::wrong_platform_textures();
        assert!(
            wrong
                .labels
                .iter()
                .any(|l| l.contains("test layers (astc decode)")),
            "{wrong:?}"
        );

        // A truncated level is an error, not a panic.
        let short = ktx2_blob(ktx2::Format::ETC2_R8G8B8_SRGB_BLOCK, &[ETC2_RED_BLOCK.to_vec()]);
        assert!(GpuArrayTexture::upload_ktx2_array_from(&device, &queue, "short", &[&short], 0).is_err());
    }

    /// An ETC2 cook uploads its blocks as they are on a device with `TEXTURE_COMPRESSION_ETC2`.
    #[test]
    fn devices_with_etc2_keep_the_blocks() {
        let _devices = serialize();
        let device = stock_device();
        if !device
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_ETC2)
        {
            eprintln!("skip: the adapter has no ETC2 (lavapipe): the decode test covers it");
            return;
        }
        let etc2 = solid_ktx2(ktx2::Format::ETC2_R8G8B8_SRGB_BLOCK, &ETC2_RED_BLOCK);
        let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let array =
            GpuArrayTexture::upload_ktx2_array_from(&device.device, &device.queue, "etc2", &[&etc2], 0)
                .unwrap_or_else(|err| panic!("upload: {err}"));
        let error = regia_core::tasks::block_on(scope.pop());
        assert!(error.is_none(), "validation error: {error:?}");
        assert_eq!(array.texture.format(), wgpu::TextureFormat::Etc2Rgb8UnormSrgb);
        assert_eq!(array.texture.mip_level_count(), 4);
        // 8-byte blocks: 2 + 1 + 1 + 1 block rows per layer, each padded to 256 bytes.
        assert_eq!(array.bytes, 2 * 256 + 3 * 256);
    }
}

/// Parses a KTX2 blob, rejecting the one input `ktx2::Reader::new` (0.5) panics on: a basic
/// data-format block whose texel-dimension byte is `255` (`dim + 1` becomes a zero `NonZeroU8`).
/// Every other structural problem comes back as the reader's own error text.
fn safe_ktx2_reader(blob: &[u8]) -> Result<ktx2::Reader<&[u8]>, String> {
    if ktx2_basic_dfd_has_max_dimension(blob) {
        return Err("data-format block has an invalid texel dimension".to_string());
    }
    ktx2::Reader::new(blob).map_err(|e| e.to_string())
}

/// Walks the data-format blocks the way the reader does and reports a basic block (vendor 0,
/// type 0) with a texel-dimension byte of `255`. Malformed bounds return `false`: the reader
/// rejects those itself.
fn ktx2_basic_dfd_has_max_dimension(blob: &[u8]) -> bool {
    let u32_at = |at: usize| -> Option<usize> {
        let bytes: [u8; 4] = blob.get(at..at.checked_add(4)?)?.try_into().ok()?;
        usize::try_from(u32::from_le_bytes(bytes)).ok()
    };
    let (Some(offset), Some(length)) = (u32_at(48), u32_at(52)) else {
        return false;
    };
    let (Some(end), Some(mut at)) = (offset.checked_add(length), offset.checked_add(4)) else {
        return false;
    };
    while at < end {
        let Some(head) = blob.get(at..at.saturating_add(16)) else {
            return false;
        };
        let size = usize::from(u16::from_le_bytes([head[6], head[7]]));
        if size < 8 {
            return false;
        }
        let vendor_and_type = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        if vendor_and_type == 0 && head[12..16].contains(&255) {
            return true;
        }
        at += size;
    }
    false
}

#[cfg(test)]
mod ktx2_guard_tests {
    use super::{ktx2_basic_dfd_has_max_dimension, safe_ktx2_reader};

    /// A blob whose only content is a 16-byte basic DFD block with the given dimension bytes.
    fn blob_with_dims(dims: [u8; 4]) -> Vec<u8> {
        let mut blob = vec![0u8; 200];
        blob[48..52].copy_from_slice(&100u32.to_le_bytes());
        blob[52..56].copy_from_slice(&20u32.to_le_bytes());
        blob[104 + 4..104 + 6].copy_from_slice(&2u16.to_le_bytes());
        blob[104 + 6..104 + 8].copy_from_slice(&16u16.to_le_bytes());
        blob[116..120].copy_from_slice(&dims);
        blob
    }

    #[test]
    fn dimension_byte_255_is_caught_before_the_reader_can_panic() {
        assert!(ktx2_basic_dfd_has_max_dimension(&blob_with_dims([3, 3, 0, 255])));
        assert!(safe_ktx2_reader(&blob_with_dims([255, 0, 0, 0])).is_err());
        assert!(!ktx2_basic_dfd_has_max_dimension(&blob_with_dims([3, 3, 0, 0])));
    }

    #[test]
    fn malformed_bounds_do_not_panic() {
        assert!(!ktx2_basic_dfd_has_max_dimension(&[]));
        let mut blob = blob_with_dims([0; 4]);
        blob[52..56].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!ktx2_basic_dfd_has_max_dimension(&blob));
        assert!(safe_ktx2_reader(&blob).is_err());
    }
}

#[cfg(test)]
mod engine_gi_tests {
    use super::{shared_source, with_engine_gi};

    fn parse(name: &str, source: &str) {
        let module = naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(source)));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }

    /// ADR-1377: the private path's stub has no probe light; the shared colour pass reads the
    /// probes' lobe light along the reflection's dominant direction from the same screen texels
    /// (only with probe specular on), and both sources validate.
    #[test]
    fn the_shared_pass_reads_the_probe_lobe_light_and_the_stub_none() {
        let private = include_str!("terrain.wgsl");
        assert!(private.contains("return EngineGi(vec4<f32>(0.0), vec3<f32>(0.0), false);"));
        assert!(private.contains(
            "reflected = max(ambient + (probe.spec - probe_gi.rgb) * probe_gi.a, vec3<f32>(0.0));"
        ));
        let shared = with_engine_gi(private);
        assert!(shared.contains("if ((ddgi.ctrl.w & 8192u) != 0u) {"));
        assert!(
            shared.contains("let m = probe_spec_lobe(n, reflect(-v, n), no_v, perceptual * perceptual);")
        );
        assert!(shared.contains("let g = ddgi_screen_gi(frag, p, n, m, eye);"));
        parse("terrain.wgsl", private);
        parse("terrain.wgsl (shared)", &shared_source(false, false));
    }
}

#[cfg(test)]
mod layer_weather_tests {
    use super::{NO_LAYER_TEXTURES, layers_uniform};
    use crate::layers::{LayerDesc, LayerWeather, TerrainLayers};

    /// A stack with the default weather response packs the same uniform bytes as before v4
    /// (zeros in `params3.yzw`), so a dry or wet frame of a migrated v3 stack is unchanged.
    #[test]
    fn the_default_response_packs_zeros() {
        let mut stack = TerrainLayers {
            layers: vec![LayerDesc::ground(), LayerDesc::ground()],
            ..Default::default()
        };
        stack.layers[1].weather = Some(LayerWeather::default());
        let uniform = layers_uniform(&stack, &NO_LAYER_TEXTURES, None, [0.0, 10.0]);
        for slot in &uniform.slots {
            assert_eq!(slot.params3[1..], [0.0, 0.0, 0.0], "{:?}", slot.params3);
        }
    }

    /// Overrides pack as `porosity + 1` and `1 - snow`; the flags say "overrides" (+1) and
    /// "no puddles" (+2) in slot 0 only.
    #[test]
    fn overrides_and_flags_pack() {
        let mut stack = TerrainLayers {
            layers: vec![LayerDesc::ground(), LayerDesc::ground(), LayerDesc::ground()],
            ..Default::default()
        };
        stack.layers[1].weather = Some(LayerWeather {
            porosity: Some(0.25),
            snow: 1.0,
            porosity_from_orm: false,
        });
        stack.layers[2].weather = Some(LayerWeather {
            porosity: None,
            snow: 0.25,
            porosity_from_orm: false,
        });
        let uniform = layers_uniform(&stack, &NO_LAYER_TEXTURES, None, [0.0, 10.0]);
        assert_eq!(uniform.slots[0].params3[1..3], [0.0, 0.0]);
        assert_eq!(uniform.slots[1].params3[1..3], [1.25, 0.0]);
        assert_eq!(uniform.slots[2].params3[1..3], [0.0, 0.75]);
        assert_eq!(uniform.slots[0].params3[3], 1.0);
        assert_eq!(uniform.slots[1].params3[3], 0.0);
        stack.puddles = false;
        let uniform = layers_uniform(&stack, &NO_LAYER_TEXTURES, None, [0.0, 10.0]);
        assert_eq!(uniform.slots[0].params3[3], 3.0);
        stack.layers[1].weather = None;
        stack.layers[2].weather = None;
        let uniform = layers_uniform(&stack, &NO_LAYER_TEXTURES, None, [0.0, 10.0]);
        assert_eq!(uniform.slots[0].params3[3], 2.0, "puddles off, no overrides");
    }

    /// `porosity_from_orm` (v5, ADR-3367) packs −1 in `params3.y` and counts as an override; a
    /// named porosity still wins; the ORM flag in `params3.x` is the cooked slot's.
    #[test]
    fn porosity_from_orm_packs_minus_one() {
        let mut stack = TerrainLayers {
            layers: vec![LayerDesc::ground(), LayerDesc::ground()],
            ..TerrainLayers::default()
        };
        stack.layers[0].weather = Some(LayerWeather {
            porosity_from_orm: true,
            ..LayerWeather::default()
        });
        stack.layers[1].weather = Some(LayerWeather {
            porosity: Some(0.5),
            porosity_from_orm: true,
            ..LayerWeather::default()
        });
        let mut flags = NO_LAYER_TEXTURES;
        flags[0] = (true, true);
        let uniform = layers_uniform(&stack, &flags, None, [0.0, 10.0]);
        assert_eq!(uniform.slots[0].params3[..3], [1.0, -1.0, 0.0]);
        assert_eq!(uniform.slots[1].params3[..3], [0.0, 1.5, 0.0]);
        assert_eq!(uniform.slots[0].params3[3], 1.0, "an override");
    }
}

#[cfg(test)]
mod lab_exposure_stops_tests {
    use super::TimeOfDay;

    /// The lab's photoreal presets (`samples/terrain_lab`) meter to the stops the book
    /// publishes: 0 at noon, ≈ 1.81 EV for the morning and ≈ 2.38 EV for the evening.
    #[test]
    fn photoreal_presets_meter_to_the_published_stops() {
        assert_eq!(TimeOfDay::default().metered_exposure_compensation_ev(), 0.0);
        let morning = TimeOfDay {
            azimuth_radians: -0.9,
            elevation_radians: 0.30,
            sun_color: [1.4, 1.05, 0.72],
            illuminance_lux: 50_000.0,
            ambient: [0.14, 0.17, 0.25],
        };
        let evening = TimeOfDay {
            azimuth_radians: 2.6,
            elevation_radians: 0.22,
            sun_color: [1.5, 0.92, 0.6],
            illuminance_lux: 38_000.0,
            ambient: [0.13, 0.14, 0.21],
        };
        assert!((morning.metered_exposure_compensation_ev() - 1.81).abs() < 0.01);
        assert!((evening.metered_exposure_compensation_ev() - 2.38).abs() < 0.01);
    }
}

#[cfg(test)]
mod altitude_reach_tests {
    use super::altitude_reach;
    use crate::layers::{LayerDesc, TerrainLayers};

    fn stack(bands: &[(f32, f32)]) -> TerrainLayers {
        TerrainLayers {
            layers: bands
                .iter()
                .map(|&(lo, hi)| LayerDesc {
                    altitude_min: lo,
                    altitude_max: hi,
                    ..LayerDesc::ground()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// The range enters the shading only through whether a band reaches the lowest or the
    /// highest ground: a crater that deepens the map past a band's floor flips that band (the
    /// virtual texture's pages must go), one that stays inside every band's reach flips none.
    #[test]
    fn a_deeper_floor_flips_only_the_bands_that_stop_reaching_it() {
        let layers = stack(&[(-2.5, 0.0), (-100.0, 100.0), (0.0, 2.5)]);
        let before = altitude_reach(&layers, [-2.2, 2.2]);
        assert_eq!(before, vec![(true, false), (true, true), (false, true)]);
        // A 20 m crater: the low band no longer reaches the floor; the wide band still does.
        let after = altitude_reach(&layers, [-22.0, 2.2]);
        assert_eq!(after, vec![(false, false), (true, true), (false, true)]);
        // A shallow dent inside the low band's reach flips nothing.
        assert_eq!(altitude_reach(&layers, [-2.4, 2.2]), before);
        // The shader's tolerance: within 1 mm of the floor still reaches it.
        assert!(altitude_reach(&layers, [-2.5005, 2.2])[0].0);
    }
}

/// CPU mirror of the per-pass slot words. A matching fill does not allocate another list.
#[cfg(test)]
struct SlotWordMirror {
    cpu: Vec<u32>,
    scratch: Vec<u32>,
    allocs: u64,
}

/// Patches `output_mode.w` in a `FrameUniform` byte mirror.
///
/// Returns the four little-endian bytes to upload when the scale changed. `None` when
/// `frame_bytes` is not one uniform, or the scale is already stored. No heap copy.
fn patch_frame_irradiance(frame_bytes: &mut [u8], scale: f32) -> Option<[u8; 4]> {
    if frame_bytes.len() != size_of::<FrameUniform>() {
        return None;
    }
    let at = std::mem::offset_of!(FrameUniform, output_mode) + 12;
    let next = scale.to_le_bytes();
    if frame_bytes[at..at + 4] == next {
        return None;
    }
    frame_bytes[at..at + 4].copy_from_slice(&next);
    Some(next)
}

/// Writes `scratch` into `cpu` when they differ and returns the changed byte ranges.
/// An identical list leaves `cpu`'s allocation alone.
fn commit_slot_words(cpu: &mut Vec<u32>, scratch: &[u32], allocs: &mut u64) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, &word) in scratch.iter().enumerate() {
        if cpu.get(i) == Some(&word) {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.end == i * 4 => last.end = (i + 1) * 4,
            _ => ranges.push(i * 4..(i + 1) * 4),
        }
    }
    if ranges.is_empty() && scratch.len() == cpu.len() {
        return ranges;
    }
    if scratch.len() > cpu.capacity() {
        *allocs += 1;
    }
    cpu.clear();
    cpu.extend_from_slice(scratch);
    ranges
}

#[cfg(test)]
impl SlotWordMirror {
    fn stage(&mut self, words: &[u32]) -> Vec<std::ops::Range<usize>> {
        self.scratch.clear();
        self.scratch.extend_from_slice(words);
        let SlotWordMirror { cpu, scratch, allocs } = self;
        commit_slot_words(cpu, scratch, allocs)
    }
}

/// World inputs shared by every node of one dense-grid classification.
struct DenseNodeQuery<'a> {
    mips: &'a HeightMips,
    /// The eye's XZ in the map's own frame (world minus the grid origin, ADR-8179).
    eye: [f32; 2],
    world_width: f32,
    /// Quadtree span for the node rectangle only (ADR-8291). Slope and texel stay on `world_width`.
    span: f32,
    root_level: u32,
    grid_size: u32,
}

/// Whether `node` draws the fine cliff grid. `cache` is the steep-node memo, locked by the caller.
fn node_is_dense(
    cache: &mut HashMap<QuadNode, bool>,
    query: &DenseNodeQuery<'_>,
    node: QuadNode,
    holes: NodeHoleState,
) -> bool {
    let (min_x, min_z, size) = node.world_rect(query.span, query.root_level);
    let dx = (min_x - query.eye[0]).max(query.eye[0] - (min_x + size)).max(0.0);
    let dz = (min_z - query.eye[1]).max(query.eye[1] - (min_z + size)).max(0.0);
    if (dx * dx + dz * dz).sqrt() >= CLIFF_DENSE_RADIUS_M {
        return false;
    }
    if holes != NodeHoleState::Empty {
        return false;
    }
    if let Some(&steep) = cache.get(&node) {
        return steep;
    }
    // The node's own cell level (at least 1: level 0 is not kept on the CPU here).
    let texel = query.mips.texel_m(query.world_width);
    let cell = size / query.grid_size as f32;
    let k = (cell / texel).max(1.0).log2().round().max(1.0) as u32;
    let steep = query
        .mips
        .max_slope_in(query.world_width, [min_x, min_z], size, k)
        > CLIFF_RELIEF_MIN_SLOPE_RAD.tan();
    cache.insert(node, steep);
    steep
}

/// Classifies `count` nodes under one lock of the steep-node cache.
fn stage_dense_with_cache(
    cpu: &mut Vec<bool>,
    allocs: &mut u64,
    locks: &mut u64,
    cache: &std::sync::Mutex<HashMap<QuadNode, bool>>,
    count: usize,
    mut classify: impl FnMut(&mut HashMap<QuadNode, bool>, usize) -> bool,
) {
    let old = cpu.capacity();
    cpu.clear();
    *locks += 1;
    let mut guard = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut index = 0;
    while index < count {
        cpu.push(classify(&mut guard, index));
        index += 1;
    }
    if cpu.capacity() > old {
        *allocs += 1;
    }
}

#[cfg(test)]
mod perf_reuse_tests {
    use super::*;

    #[test]
    fn held_selection_reuses_the_slot_mirror() {
        let mut mirror = SlotWordMirror {
            cpu: Vec::new(),
            scratch: Vec::new(),
            allocs: 0,
        };
        let words = [0u32, 1, 2, 3];
        let first = mirror.stage(&words);
        assert!(!first.is_empty());
        let allocs = mirror.allocs;
        let second = mirror.stage(&words);
        assert!(second.is_empty(), "identical lists still uploaded {second:?}");
        assert_eq!(
            mirror.allocs, allocs,
            "identical lists allocated another slot mirror"
        );
        assert_eq!(mirror.cpu, words);
        let shorter = mirror.stage(&[0, 1]);
        assert!(shorter.is_empty(), "the prefix matches, so no word upload");
        assert_eq!(mirror.cpu, vec![0, 1], "a shorter list still updates the mirror");
    }

    #[test]
    fn dense_flags_lock_the_steep_cache_once() {
        let cache = std::sync::Mutex::new(HashMap::new());
        let mut cpu = Vec::new();
        let mut allocs = 0u64;
        let mut locks = 0u64;
        let root = 4u32;
        let world = 256.0f32;
        let nodes: Vec<QuadNode> = (0..4).map(|x| QuadNode { level: 0, x, z: 0 }).collect();
        let (min_x, min_z, size) = nodes[0].world_rect(world, root);
        let eye = [min_x + size * 0.5, min_z + size * 0.5];
        let classify = |map: &mut HashMap<QuadNode, bool>, node: QuadNode| {
            let dist = node.rect_distance(world, root, eye);
            assert!(
                dist < CLIFF_DENSE_RADIUS_M,
                "{dist} m is outside the dense radius"
            );
            *map.entry(node).or_insert(true)
        };
        stage_dense_with_cache(
            &mut cpu,
            &mut allocs,
            &mut locks,
            &cache,
            nodes.len(),
            |map, index| classify(map, nodes[index]),
        );
        assert_eq!(locks, 1, "the steep cache was locked once per node");
        assert_eq!(cpu, vec![true; 4]);
        let allocs_after = allocs;
        let cap = cpu.capacity();
        stage_dense_with_cache(
            &mut cpu,
            &mut allocs,
            &mut locks,
            &cache,
            nodes.len(),
            |map, index| classify(map, nodes[index]),
        );
        assert_eq!(locks, 2, "the second staging locks once, not once per node");
        assert_eq!(
            allocs, allocs_after,
            "the second staging allocated a new dense mirror"
        );
        assert_eq!(cpu.capacity(), cap);
    }

    /// The sky-irradiance scale is four bytes in the existing frame mirror. A repeat with the
    /// same scale writes nothing and allocates nothing.
    #[test]
    fn finish_prepare_does_not_allocate_the_frame_uniform() {
        let mut bytes = vec![0u8; size_of::<FrameUniform>()];
        let scale = 1.5f32;
        assert_eq!(
            patch_frame_irradiance(&mut bytes, scale),
            Some(scale.to_le_bytes())
        );
        let at = std::mem::offset_of!(FrameUniform, output_mode) + 12;
        assert_eq!(&bytes[at..at + 4], scale.to_le_bytes().as_slice());
        assert!(patch_frame_irradiance(&mut bytes, scale).is_none());
        assert_eq!(
            patch_frame_irradiance(&mut bytes, 2.0),
            Some(2.0f32.to_le_bytes())
        );
        assert!(patch_frame_irradiance(&mut bytes[..4], 1.0).is_none());
    }
}

#[cfg(test)]
mod material_contract_tests {
    /// `Material` in `terrain.wgsl` carries the macro residency words between `winter2` and
    /// `weather`. The contract row has to name them or the shader ratchet reports members
    /// with no Rust field. The byte layout is already 224 / 256 / 288.
    #[test]
    fn material_contract_lists_the_macro_residency_words() {
        let rows = super::contract_rows();
        let material = rows
            .iter()
            .find(|row| row.wgsl_struct == "Material" && row.shader == "terrain.wgsl")
            .expect("Material row");
        assert!(!material.partial);
        let offset = |name: &str| material.members.iter().find(|(n, _)| *n == name).map(|(_, o)| *o);
        assert_eq!(offset("macro_mask"), Some(224));
        assert_eq!(offset("macro_prefix"), Some(256));
        assert_eq!(offset("weather"), Some(288));
        assert_eq!(material.size, 416);
    }

    /// The terrain uniform's depth lane (ADR-8369) sits at byte 48 in both shaders that read
    /// the heightfield, and the water prefix is no longer shorter than the host struct.
    #[test]
    fn terrain_contract_lists_the_depth_lane() {
        use regia_render::shader_contract::{ContractInput, parse_plain, run_contract_ratchet};

        let rows: Vec<_> = super::contract_rows()
            .into_iter()
            .filter(|row| row.wgsl_struct == "Terrain")
            .collect();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(!row.host_may_extend, "{}", row.shader);
            assert!(!row.partial, "{}", row.shader);
            assert_eq!(row.size, 64, "{}", row.shader);
            let offset = |name: &str| row.members.iter().find(|(n, _)| *n == name).map(|(_, o)| *o);
            assert_eq!(offset("depth"), Some(48), "{}", row.shader);
            assert_eq!(offset("mip_info"), Some(32), "{}", row.shader);
        }
        let terrain = parse_plain(include_str!("terrain.wgsl")).expect("terrain.wgsl");
        let water = parse_plain(include_str!("water.wgsl")).expect("water.wgsl");
        let inputs = [
            ContractInput {
                file: "terrain.wgsl",
                module: &terrain,
            },
            ContractInput {
                file: "water.wgsl",
                module: &water,
            },
        ];
        let report = run_contract_ratchet(&inputs, &rows, &[]);
        assert!(
            report.mismatches.is_empty(),
            "depth lane drifted from the shaders:\n{}",
            report.mismatches.join("\n")
        );
    }
}
