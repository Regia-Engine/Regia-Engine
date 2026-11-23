// SPDX-License-Identifier: MPL-2.0
//! The GPU side of the water bodies: [`WaterRenderer`] owns the mesh, the uniforms, the ripple map
//! and the pipelines, and hands the frame renderer a [`TranslucentLayer`] that draws them.
//!
//! The frame renderer calls the layer once per rendered frame: `bind_decal_targets` gives the
//! scene-colour copy (HDR frames), `simulate` gives the scene depth and the camera (the uniforms
//! are written then), `wants_scene_after_decals` asks for a fresh copy only when this frame
//! composites, and `draw_hdr` (or `draw` on a display pass) records one indexed draw, plus one
//! for the ring grid when a plane carries a swell (ADR-1620).
//!
//! # Swell
//!
//! `simulate` also sizes the ring grid for the view ([`RingGrid::for_view`]; rebuilt only when
//! the grid or the swell bodies change), snaps its center to the camera on a step of two finest
//! cells, and writes the swell uniform: per body the first wave and wave count in a shared table
//! of [`MAX_SWELL_TABLE`] waves (each body capped by the tier's [`WaterTier::swell_waves`]),
//! its rest rectangle and level, and per wave `(k_x, k_z, A, H)` and the phase at the grid's
//! center, reduced in `f64` ([`Swell::instant`]).
//!
//! # Shore (spec 87, ADR-1828)
//!
//! On High and Ultra, the plane bodies' shore fields ([`ShoreField`], at most
//! [`MAX_SHORE_FIELDS`] per frame) are bound as one `Rg32Float` texture array (depth, distance)
//! read with `textureLoad` and blended in the shader exactly as [`ShoreField::sample`] does, so the
//! drawn swell is shoaled by the same depth the CPU queries use ([`ring_vertex_on`]); the fragment
//! adds breaker and shore foam. Low binds no field (memory and fetches; the swell is unshoaled).
//!
//! # Ocean (ADR-1969)
//!
//! A plane with an [`super::FftOcean`] is on the ring grid like a swell plane, with its own swell
//! or the ocean's stand-in; on High and Ultra it also rides the frame's ocean maps (binding 11),
//! which the compute pass in `ocean_gpu.rs` fills before the water draws. The pass is a frame
//! extension at the `after_opaque` slot ([`WaterRenderer::ocean_pass`]); `simulate` records it
//! when the slot did not run (a display-only frame, a host that did not register it).

use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};
use regia_render::{
    FramePass, FramePassContext, GraphSlot, LayerCamera, RenderDevice, RenderError, TranslucentLayer,
};
use serde::Serialize;

use super::caustics::{self, WaterCaustics};
use super::caustics_gpu::{CausticStats, CausticsGpu, ShaftFrame, WaterSunShadow};
use super::clock::WaterClock;
use super::cull::{CullBody, frustum_planes, visible_ranges_through_into};
use super::mesh::{RingChunk, WaterMesh, WaterVertex, build_mesh, build_ring_grid_chunks, build_ring_mesh};
use super::normal_map::{RIPPLE_MAP_SIZE, ripple_levels};
use super::ocean::{FftOcean, OceanFrame};
use super::ocean_gpu::{OCEAN_PASS_NAME, OceanCost, OceanGpu, OceanGridError, OceanMaps};
use super::shore::{SHORE_DEEP, ShoreField};
use super::swell::{MAX_RINGS, MAX_SWELL_TABLE, RingGrid, Swell};
use super::{MAX_BODIES, MAX_OBSTACLES, WaterBody, WaterObstacle, WaterShape, WaterTier};

/// Most shore fields one frame binds (spec 87 § 9); further fields are counted as dropped.
pub const MAX_SHORE_FIELDS: usize = 4;

/// `vec4`s of the shore uniform (binding 10): two per field, then the body slots' field map.
const SHORE_VEC4S: usize = 2 * MAX_SHORE_FIELDS + MAX_BODIES / 4;

/// The water shader (`water_body.wgsl`).
pub const SHADER: &str = include_str!("water_body.wgsl");

/// Longest side (pixels) of the private target a timed draw covers.
pub const TIMING_MAX_SIDE: u32 = 2048;

/// The compute entry point appended to [`SHADER`] for [`WaterRenderer::probe_ring_vertices`]: it
/// places ring-grid vertices with the vertex shader's own `ring_world`.
pub const PROBE_ENTRY: &str = r#"
@group(1) @binding(0) var<storage, read> probe_in: array<vec4<f32>>;
@group(1) @binding(1) var<storage, read_write> probe_out: array<vec4<f32>>;

@compute @workgroup_size(64)
fn cs_probe(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= arrayLength(&probe_in)) {
        return;
    }
    let q = probe_in[i];
    probe_out[i] = vec4<f32>(ring_world(min(u32(q.z), 15u), q.xy), 1.0);
}
"#;

/// `vec4`s in the swell uniform: the grid, three per body, two per table wave.
/// `vec4`s of ring-boundary shifts at the end of the swell uniform (two boundaries each; ADR-2342).
const RING_SHIFT_VEC4S: usize = (MAX_RINGS as usize).div_ceil(2);
const SWELL_VEC4S: usize = 1 + 3 * MAX_BODIES + 2 * MAX_SWELL_TABLE + RING_SHIFT_VEC4S;

/// Ambient illuminance (lux) assumed until a host reports the frame's ([`WaterRenderer::set_ambient_lux`]).
pub const DEFAULT_AMBIENT_LUX: f32 = 20_000.0;

// A plane's vertices carry a half width above the shader's "no bank" threshold (1e8).
const _: () = assert!(super::mesh::PLANE_HALF_WIDTH > 1.0e8);

/// Ripple scroll speeds, tiles per second, of the two fine layers (independent of the flow, so
/// still water moves too).
pub(super) const SCROLL: [[f64; 2]; 2] = [[0.8 * 0.021, 0.6 * 0.021], [-0.6 * 0.017, 0.8 * 0.017]];

/// Sky radiance the water reflects where neither a reflection ray nor the frame's own sky gives
/// one (scene-referred, before exposure).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WaterSky {
    /// Radiance straight up (linear RGB).
    pub zenith: [f32; 3],
    /// Radiance at the horizon (linear RGB).
    pub horizon: [f32; 3],
}

impl WaterSky {
    /// A clear-sky estimate from the ambient illuminance: a uniform sky of radiance `L` gives
    /// `π L` lux on a level surface, so `L = lux / π`, bluer at the zenith and paler at the
    /// horizon.
    #[must_use]
    pub fn from_ambient_lux(lux: f32) -> Self {
        let l = if lux.is_finite() { lux.max(0.0) } else { 0.0 } / std::f32::consts::PI;
        Self {
            zenith: [0.62 * l, 0.8 * l, 1.25 * l],
            horizon: [1.08 * l, 1.1 * l, 1.12 * l],
        }
    }
}

/// A debug view of the water (agent `water.set { debug }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaterDebug {
    /// The shaded water.
    #[default]
    Off,
    /// What comes through the surface: the bed after absorption, plus in-scatter.
    Transmitted,
    /// What the surface reflects (screen-space hit, the frame's sky, or the fallback sky).
    Reflected,
    /// The shading normal, `n · 0.5 + 0.5`.
    Normal,
    /// The Fresnel reflectance.
    Fresnel,
    /// Whitewater (red) and ice (green) coverage.
    Masks,
}

impl WaterDebug {
    /// Every view, in protocol order.
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::Transmitted,
        Self::Reflected,
        Self::Normal,
        Self::Fresnel,
        Self::Masks,
    ];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Transmitted => "transmitted",
            Self::Reflected => "reflected",
            Self::Normal => "normal",
            Self::Fresnel => "fresnel",
            Self::Masks => "masks",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.name() == name)
    }

    fn code(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::Transmitted => 1.0,
            Self::Reflected => 2.0,
            Self::Normal => 3.0,
            Self::Fresnel => 4.0,
            Self::Masks => 5.0,
        }
    }
}

/// What the water drew on the last frame (agent `water.state`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct WaterFrameStats {
    /// Bodies in the mesh.
    pub bodies: u32,
    /// Vertices uploaded.
    pub vertices: u32,
    /// Triangles drawn per frame.
    pub triangles: u32,
    /// Obstacles evaluated on the last frame (the nearest ones, up to the tier's budget).
    pub obstacles: u32,
    /// Frames the layer drew in.
    pub frames_drawn: u64,
    /// The last frame composited over a scene copy (refraction, reflection rays).
    pub composite: bool,
    /// The last frame drew into the HDR target.
    pub hdr: bool,
    /// The tier in effect.
    pub tier: WaterTier,
    /// The tick the last frame animated.
    pub tick: u64,
    /// Plane bodies drawn with a swell on the ring grid (ADR-1620).
    pub swell_bodies: u32,
    /// Swell waves drawn, all bodies together (each body capped by the tier).
    pub swell_waves: u32,
    /// Waves left out because the frame's shared table ([`MAX_SWELL_TABLE`]) was full.
    pub swell_waves_dropped: u32,
    /// The ring grid's finest cell (m); 0 without a swell.
    pub ring_cell_m: f32,
    /// Cells across the half of the ring grid's inner square.
    pub ring_half_cells: u32,
    /// Rings of the grid, the inner square included.
    pub rings: u32,
    /// Ring-grid vertices uploaded (all swell bodies).
    pub ring_vertices: u32,
    /// Ring-grid triangles in the uploaded mesh (all swell bodies).
    pub ring_triangles: u32,
    /// Ring-grid triangles the last frame drew after frustum culling (ADR-2342).
    pub ring_triangles_drawn: u32,
    /// Ring-grid chunks (the inner square, each ring's four sides; per body) in the mesh.
    pub ring_chunks: u32,
    /// Chunks the last frame left out because they lay outside the camera's frustum.
    pub ring_chunks_culled: u32,
    /// The ring grid's center (x, z): the camera snapped to two finest cells.
    pub ring_origin: [f64; 2],
    /// Shore fields bound on the last frame (spec 87; 0 on Low or with the switch off).
    pub shore_fields: u32,
    /// Shore fields left out because the frame holds at most [`MAX_SHORE_FIELDS`].
    pub shore_fields_dropped: u32,
    /// Texels of the bound shore fields (all layers, padding included; 0 with none bound).
    pub shore_texels: u64,
    /// Bodies riding the frame's FFT ocean (ADR-1969; 0 on Low or with the switch off).
    pub ocean_bodies: u32,
    /// Ocean bodies left to their swell: another spectrum than the frame's, or no ocean drawn.
    pub ocean_dropped: u32,
    /// Why the frame's first ocean could not run on this device (its grid is too large for the
    /// device's limits; ADR-3251), if it could not: that ocean draws as swell.
    pub ocean_refused: Option<OceanGridError>,
    /// Texels per side of the bound ocean maps (0 with none).
    pub ocean_size: u32,
    /// Compute dispatches of the last evolve.
    pub ocean_dispatches: u32,
    /// Evolves of the ocean maps since the renderer was created.
    pub ocean_evolves: u64,
    /// GPU memory of the bound ocean's lingering-foam history (bytes; 0 with none, ADR-2843).
    pub ocean_history_bytes: u64,
    /// Steps of the lingering-foam history since the renderer was created.
    pub ocean_history_steps: u64,
    /// History steps that restarted it: a new ocean or grid, a change of the bodies, a camera cut,
    /// a clock step back or longer than `ocean_foam::FOAM_MAX_STEP`.
    pub ocean_history_restarts: u64,
    /// GPU time of the last timed water draw (ms; ADR-2342), 0 until one ran. The draw is made
    /// into a private target of `gpu_ms_pixels` pixels with the frame's depth bound, so it costs
    /// the water alone (no scene copy, so a composite tier is timed as a blend).
    pub gpu_ms: f32,
    /// Pixels of the target the last timed draw covered (the viewport clamped to 2048 per side).
    pub gpu_ms_pixels: u32,
    /// Timed draws so far.
    pub gpu_ms_samples: u64,
    /// `true` when the device can time passes (`TIMESTAMP_QUERY`).
    pub gpu_ms_supported: bool,
    /// The eye was under a plane body's surface on the last frame: the water column fogs the
    /// scene and the surface is drawn from below (ADR-2342).
    pub underwater: bool,
    /// How far under the surface the eye was (m; 0 above water).
    pub underwater_depth_m: f32,
    /// The last frame's eye stood inside a cell and the water was culled through its outdoor
    /// portals (ADR-8843).
    pub portal_restricted: bool,
    /// Mesh bodies (lakes, rivers; not the ring grid) the last frame drew.
    pub bodies_drawn: u32,
    /// Mesh bodies the last frame left out because no outdoor portal frustum admits their box.
    pub portal_culled_bodies: u32,
    /// Ring-grid chunks the last frame left out through the outdoor portals (after the frustum
    /// cull, when that is on).
    pub portal_culled_ring_chunks: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct FrameUniform {
    view_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    sun_dir: [f32; 4],
    sun_color: [f32; 4],
    sky_zenith: [f32; 4],
    sky_horizon: [f32; 4],
    screen: [f32; 4],
    depth: [f32; 4],
    flow: [f32; 4],
    coarse: [f32; 4],
    scroll: [f32; 4],
    mode: [f32; 4],
    misc: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct BodyUniform {
    absorption: [f32; 4],
    scatter: [f32; 4],
    surface: [f32; 4],
    wet: [f32; 4],
    ice: [f32; 4],
    ice_color: [f32; 4],
    /// Caustics (ADR-2432): strength (0 off), pattern scale, chromatic spread, shaft steps.
    caustic: [f32; 4],
}

impl BodyUniform {
    fn of(body: &WaterBody, tier: WaterTier) -> Self {
        let l = &body.look;
        let c: &WaterCaustics = &l.caustics;
        let on = c.active_at(tier);
        let ice = body.ice.unwrap_or_default();
        Self {
            absorption: [l.absorption[0], l.absorption[1], l.absorption[2], l.scattering],
            scatter: [
                l.scatter_color[0],
                l.scatter_color[1],
                l.scatter_color[2],
                l.roughness,
            ],
            surface: [l.normal_strength, l.ripple_scale, l.refraction, l.foam],
            wet: [l.wet_height, l.wet_width, l.wet_darkening, l.glint],
            ice: [
                ice.width,
                ice.max_depth,
                if body.ice.is_some() { 1.0 } else { 0.0 },
                ice.clarity,
            ],
            ice_color: [ice.color[0], ice.color[1], ice.color[2], 0.0],
            caustic: if on {
                [c.strength, c.scale, c.chromatic, c.shaft_steps as f32]
            } else {
                [0.0; 4]
            },
        }
    }
}

/// The frame inputs of one draw: everything `simulate` derives from the camera, the clock and the
/// settings. Pure, so tests check it without a GPU.
/// The per-frame switches of [`frame_uniform`].
#[derive(Debug, Clone, Copy)]
struct FrameInputs {
    tier: WaterTier,
    composite: bool,
    obstacles: u32,
    sky: WaterSky,
    ambient_lux: f32,
    debug: WaterDebug,
    /// The underwater view is on (ADR-2342): off keeps the uniform as it was before the view.
    underwater_view: bool,
    /// The plane body (table slot) whose surface is above the eye, and that surface's height
    /// (ADR-2342); `None` above water or with the view off.
    underwater: Option<(u32, f32)>,
}

/// `misc.w` of the frame uniform (ADR-2342): 0 with the underwater view off (the surface is not
/// drawn from below, as before the view), 1 with it on and the eye above water, and the body's
/// table slot + 2 with the eye under that body's surface.
fn underwater_code(view: bool, underwater: Option<(u32, f32)>) -> f32 {
    match (view, underwater) {
        (false, _) => 0.0,
        (true, None) => 1.0,
        (true, Some((slot, _))) => (slot + 2) as f32,
    }
}

/// The camera's view-projection matrix, the one the water shader draws with.
fn camera_view_proj(camera: &LayerCamera) -> regia_core::glam::Mat4 {
    use regia_core::glam::Vec3;
    let aspect = camera.width.max(1) as f32 / camera.height.max(1) as f32;
    let view = regia_render::view_math::look_at_view(
        Vec3::from_array(camera.eye),
        Vec3::from_array(camera.target),
        Vec3::Y,
    );
    let (fov_y, near, far) =
        regia_render::view_math::usable_projection(camera.fov_y, camera.near, camera.far);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(fov_y, aspect, near, far);
    proj * view
}

fn frame_uniform(camera: &LayerCamera, clock: &WaterClock, inputs: &FrameInputs) -> FrameUniform {
    let FrameInputs {
        tier,
        composite,
        obstacles,
        sky,
        ambient_lux,
        debug,
        underwater_view,
        underwater,
    } = *inputs;
    use regia_core::glam::Vec3;
    let width = camera.width.max(1);
    let height = camera.height.max(1);
    let (fov_y, near, far) =
        regia_render::view_math::usable_projection(camera.fov_y, camera.near, camera.far);
    let view_proj = camera_view_proj(camera);
    let inv = view_proj.inverse();
    // HDR: the target holds radiance × pre-exposure (post ÷ `exposure`). Display: radiance ×
    // the post exposure, tonemapped in the shader.
    let out_scale = if camera.hdr {
        let pre = camera.display_exposure / camera.exposure;
        if pre.is_finite() && pre > 0.0 { pre } else { 1.0 }
    } else if camera.display_exposure.is_finite() && camera.display_exposure > 0.0 {
        camera.display_exposure
    } else {
        1.0
    };
    let sun_e = if camera.sun_illuminance.is_finite() {
        camera.sun_illuminance.max(0.0)
    } else {
        0.0
    };
    let amb = if ambient_lux.is_finite() {
        ambient_lux.max(0.0)
    } else {
        0.0
    };
    let phases = clock.phases();
    let seconds = clock.seconds();
    let scroll = |layer: [f64; 2]| {
        [
            (seconds * layer[0]).rem_euclid(1.0) as f32,
            (seconds * layer[1]).rem_euclid(1.0) as f32,
        ]
    };
    let (sa, sb) = (scroll(SCROLL[0]), scroll(SCROLL[1]));
    let s = |c: [f32; 3]| [c[0] * out_scale, c[1] * out_scale, c[2] * out_scale, 0.0];
    let (steps, refine, shadow) = match tier {
        WaterTier::Low => (0.0, 0.0, 0.0),
        WaterTier::High => (tier.ssr_steps() as f32, 3.0, 10.0),
        WaterTier::Ultra => (tier.ssr_steps() as f32, 6.0, 16.0),
    };
    let to_sun = Vec3::from_array(camera.sun_direction).normalize_or(Vec3::Y);
    FrameUniform {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: inv.to_cols_array_2d(),
        eye: [
            camera.eye[0],
            camera.eye[1],
            camera.eye[2],
            clock.wrapped_seconds(),
        ],
        sun_dir: [to_sun.x, to_sun.y, to_sun.z, sun_e * out_scale],
        sun_color: [
            camera.sun_color[0],
            camera.sun_color[1],
            camera.sun_color[2],
            amb * out_scale,
        ],
        sky_zenith: s(sky.zenith),
        sky_horizon: s(sky.horizon),
        screen: [
            width as f32,
            height as f32,
            1.0 / width as f32,
            1.0 / height as f32,
        ],
        depth: [near, far, (0.5 * fov_y).tan(), underwater.map_or(0.0, |(_, y)| y)],
        flow: [
            phases.a,
            phases.b,
            phases.weight_a,
            super::clock::FLOW_CYCLE_SECS as f32,
        ],
        coarse: [
            phases.coarse,
            phases.coarse_weight,
            (4.0 * super::clock::FLOW_CYCLE_SECS) as f32,
            if matches!(tier, WaterTier::Low) { 0.0 } else { 1.0 },
        ],
        scroll: [sa[0], sa[1], sb[0], sb[1]],
        mode: [
            if composite { 1.0 } else { 0.0 },
            if composite { steps } else { 0.0 },
            obstacles as f32,
            if camera.hdr { 0.0 } else { 1.0 },
        ],
        misc: [
            refine,
            if composite { shadow } else { 0.0 },
            debug.code(),
            underwater_code(underwater_view, underwater),
        ],
    }
}

/// The nearest `budget` obstacles to `eye` (ties by list order), as `[x, z, radius, strength]`.
fn nearest_obstacles(all: &[WaterObstacle], eye: [f32; 3], budget: usize) -> Vec<[f32; 4]> {
    let mut order: Vec<(f32, usize)> = all
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let dx = o.position[0] - eye[0];
            let dz = o.position[2] - eye[2];
            (dx * dx + dz * dz, i)
        })
        .collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    order
        .into_iter()
        .take(budget.min(MAX_OBSTACLES))
        .map(|(_, i)| {
            let o = all[i];
            [o.position[0], o.position[2], o.radius, o.strength]
        })
        .collect()
}

/// The grid and the swell bodies (table slot, drift) a ring mesh was built for.
type RingKey = (RingGrid, Vec<(u32, [f32; 2])>);

/// What [`WaterRenderer::probe_ring_vertices`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct RingProbe {
    /// The ring grid of the probed view.
    pub grid: RingGrid,
    /// Its center (x, z): the eye snapped to two finest cells.
    pub origin: [f64; 2],
    /// The shifts of the ring boundaries ([`RingGrid::ring_shifts`]) the GPU used.
    pub shifts: Vec<[f32; 2]>,
    /// The world position of each probed vertex, as the GPU computed it.
    pub positions: Vec<[f32; 3]>,
}

/// What `prepare_swell` decided for the frame: the grid and its center.
#[derive(Debug, Clone, PartialEq)]
struct SwellFrame {
    grid: RingGrid,
    origin: [f64; 2],
    /// Shifts of the ring boundaries (ADR-2342).
    shifts: Vec<[f32; 2]>,
}

/// The swell uniform for `bodies` (their table slots and swell) on `grid` around `origin`, at
/// `seconds`, each body capped at `cap` waves. Returns the packed `vec4`s, the waves drawn and the
/// waves dropped. Pure, so tests check it without a GPU. Production packs in place
/// (`swell_uniform_into`).
#[cfg(test)]
fn swell_uniform(
    bodies: &[(u32, &WaterBody)],
    grid: &RingGrid,
    origin: [f64; 2],
    shifts: &[[f32; 2]],
    seconds: f64,
    cap: usize,
) -> (Vec<[f32; 4]>, u32, u32) {
    let mut packed = vec![[0.0f32; 4]; SWELL_VEC4S];
    let (waves, dropped) = fill_swell(
        &mut packed,
        bodies.iter().copied(),
        grid,
        origin,
        shifts,
        seconds,
        cap,
    );
    (packed, waves, dropped)
}

/// Packs into `packed`, which the caller keeps. A second pack of the same size does not allocate.
fn swell_uniform_into<'a>(
    packed: &mut Vec<[f32; 4]>,
    bodies: impl IntoIterator<Item = (u32, &'a WaterBody)>,
    grid: &RingGrid,
    origin: [f64; 2],
    shifts: &[[f32; 2]],
    seconds: f64,
    cap: usize,
) -> (u32, u32) {
    if packed.len() != SWELL_VEC4S {
        packed.resize(SWELL_VEC4S, [0.0; 4]);
    }
    fill_swell(packed, bodies, grid, origin, shifts, seconds, cap)
}

fn fill_swell<'a>(
    packed: &mut [[f32; 4]],
    bodies: impl IntoIterator<Item = (u32, &'a WaterBody)>,
    grid: &RingGrid,
    origin: [f64; 2],
    shifts: &[[f32; 2]],
    seconds: f64,
    cap: usize,
) -> (u32, u32) {
    packed.fill([0.0; 4]);
    let ring_base = 1 + 3 * MAX_BODIES + 2 * MAX_SWELL_TABLE;
    for (j, d) in shifts.iter().take(MAX_RINGS as usize).enumerate() {
        let v = &mut packed[ring_base + j / 2];
        v[2 * (j % 2)] = d[0];
        v[2 * (j % 2) + 1] = d[1];
    }
    packed[0] = [
        origin[0] as f32,
        origin[1] as f32,
        grid.cell(),
        grid.inner_half_size(),
    ];
    let mut cursor = 0usize;
    let mut dropped = 0u32;
    for (slot, body) in bodies {
        let (
            Some(swell),
            WaterShape::Plane {
                center, half_extent, ..
            },
        ) = (body.active_swell(), &body.shape)
        else {
            continue;
        };
        let slot = (slot as usize).min(MAX_BODIES - 1);
        let instant = swell.instant(seconds, origin, cap);
        let n = instant.terms().len().min(MAX_SWELL_TABLE - cursor);
        dropped += (instant.terms().len() - n) as u32;
        let base = 1 + 3 * slot;
        packed[base] = [cursor as f32, n as f32, swell.crest_foam, swell.foam_onset];
        packed[base + 1] = [center[0], center[2], half_extent[0], half_extent[1]];
        packed[base + 2] = [center[1], 1.0, 0.0, 0.0];
        let table = 1 + 3 * MAX_BODIES;
        for (i, w) in instant.terms().iter().take(n).enumerate() {
            packed[table + 2 * (cursor + i)] = [w.k[0], w.k[1], w.amplitude, w.horizontal];
            packed[table + 2 * (cursor + i) + 1] = [w.phase, w.omega, 0.0, 0.0];
        }
        cursor += n;
    }
    (cursor as u32, dropped)
}

/// Whether a plane's rest rectangle is safe to clamp into. `f32::clamp` aborts when a bound
/// is NaN, and a non-finite center would be stored in the vertex.
fn plane_rect_is_drawable(center: &[f32; 3], half: &[f32; 2]) -> bool {
    center.iter().all(|c| c.is_finite())
        && half[0].is_finite()
        && half[0] > 0.0
        && half[1].is_finite()
        && half[1] > 0.0
}

/// Where the vertex shader puts the ring-grid vertex at offset `local` from the grid's center for
/// a swell plane `body` (CPU mirror of `water_body.wgsl` `ring_world`): the center plus the
/// offset, clamped to the rest rectangle, then displaced by the first `cap` waves, each faded on
/// the grid's cell there ([`RingGrid::fade_cell`]). `None` for a body without a swell.
#[must_use]
pub fn ring_vertex(
    body: &WaterBody,
    grid: &RingGrid,
    origin: [f64; 2],
    seconds: f64,
    cap: usize,
    local: [f32; 2],
) -> Option<[f32; 3]> {
    ring_vertex_on(body, None, grid, origin, seconds, cap, local)
}

/// [`ring_vertex`] over a shore field (spec 87 § 3): the waves are shoaled by the depth the field
/// gives at the vertex's rest point (`o + l` in `f32`, as the shader forms it), the mirror of
/// `water_body.wgsl` `ring_world` with a field bound.
#[must_use]
pub fn ring_vertex_on(
    body: &WaterBody,
    shore: Option<&ShoreField>,
    grid: &RingGrid,
    origin: [f64; 2],
    seconds: f64,
    cap: usize,
    local: [f32; 2],
) -> Option<[f32; 3]> {
    let frame = RingFrame {
        grid,
        shifts: &[],
        origin,
    };
    ring_vertex_snapped(body, shore, &frame, seconds, cap, local)
}

/// The ring grid of a frame as the vertex shader sees it: the grid, its per-ring boundary shifts
/// ([`RingGrid::ring_shifts`]) and its center.
#[derive(Debug, Clone, Copy)]
pub struct RingFrame<'a> {
    /// The grid.
    pub grid: &'a RingGrid,
    /// The shifts of the ring boundaries (empty: the rings do not snap on their own).
    pub shifts: &'a [[f32; 2]],
    /// The grid's center (x, z).
    pub origin: [f64; 2],
}

/// [`ring_vertex_on`] with the per-ring snap (ADR-2342): the vertex at `local` is first moved by
/// [`RingGrid::ring_shift`] of the boundary `shifts` ([`RingGrid::ring_shifts`]; empty: no move),
/// then clamped, shoaled and displaced as before; the fade cell still follows the unmoved `local`,
/// so the two rings that share a seam vertex agree on it. Mirrors `water_body.wgsl` `ring_world`.
#[must_use]
pub fn ring_vertex_snapped(
    body: &WaterBody,
    shore: Option<&ShoreField>,
    frame: &RingFrame<'_>,
    seconds: f64,
    cap: usize,
    local: [f32; 2],
) -> Option<[f32; 3]> {
    let (swell, center, half): (&Swell, [f32; 3], [f32; 2]) = match (&body.shape, body.active_swell()) {
        (
            WaterShape::Plane {
                center, half_extent, ..
            },
            Some(swell),
        ) => (swell, *center, *half_extent),
        _ => return None,
    };
    let RingFrame { grid, shifts, origin } = *frame;
    // A hand-built or deserialized plane may carry a non-finite center or a non-positive
    // half extent. `f32::clamp` aborts when a bound is NaN, so reject the vertex instead.
    if !plane_rect_is_drawable(&center, &half) {
        return None;
    }
    let o = [origin[0] as f32, origin[1] as f32];
    let shift = grid.ring_shift(shifts, local);
    let moved = [local[0] + shift[0], local[1] + shift[1]];
    let l = [
        (o[0] + moved[0]).clamp(center[0] - half[0], center[0] + half[0]) - o[0],
        (o[1] + moved[1]).clamp(center[2] - half[1], center[2] + half[1]) - o[1],
    ];
    let instant = swell.instant(seconds, origin, cap);
    let depth = shore.map(|f| f.sample(o[0] + l[0], o[1] + l[1]).map_or(SHORE_DEEP, |b| b.depth));
    let d = instant.grid_offset_local_on(l, grid.fade_cell(local), depth);
    Some([o[0] + l[0] + d[0], center[1] + d[1], o[1] + l[1] + d[2]])
}

/// [`ring_vertex_on`] of a plane with an ocean drawn on `maps` (the mirror of `water_body.wgsl`
/// `ring_world` when the body rides the ocean): the swell, or the ocean's stand-in, and the ocean
/// blended by the depth at the rest point. `ring` is the grid and its center. `None` for a body
/// without either.
#[must_use]
pub fn ring_vertex_ocean(
    body: &WaterBody,
    shore: Option<&ShoreField>,
    maps: &OceanFrame,
    ring: (&RingGrid, [f64; 2]),
    seconds: f64,
    cap: usize,
    local: [f32; 2],
) -> Option<[f32; 3]> {
    let WaterShape::Plane {
        center, half_extent, ..
    } = &body.shape
    else {
        return None;
    };
    let ocean = body.active_ocean()?;
    let swell = body.fallback_swell()?;
    // Same guard as [`ring_vertex_snapped`]: a non-finite center aborts `f32::clamp`.
    if !plane_rect_is_drawable(center, half_extent) {
        return None;
    }
    let (grid, origin) = ring;
    let o = [origin[0] as f32, origin[1] as f32];
    let l = [
        (o[0] + local[0]).clamp(center[0] - half_extent[0], center[0] + half_extent[0]) - o[0],
        (o[1] + local[1]).clamp(center[2] - half_extent[1], center[2] + half_extent[1]) - o[1],
    ];
    let depth = shore.map(|f| f.sample(o[0] + l[0], o[1] + l[1]).map_or(SHORE_DEEP, |b| b.depth));
    let w = ocean.shore_weight(depth);
    let cell = grid.fade_cell(local);
    let mut d = [0.0f32; 3];
    if w < 1.0 {
        let s = swell
            .instant(seconds, origin, cap)
            .grid_offset_local_on(l, cell, depth);
        d = s.map(|c| (1.0 - w) * c);
    }
    if w > 0.0 {
        let f = maps.grid_offset(origin[0] + f64::from(l[0]), origin[1] + f64::from(l[1]), cell);
        for (c, v) in d.iter_mut().zip(f) {
            *c += w * v;
        }
    }
    Some([o[0] + l[0] + d[0], center[1] + d[1], o[1] + l[1] + d[2]])
}

/// The ring grid for a view of `bodies` (swell planes) from `camera` under `tier`, and its
/// center: the eye snapped to two finest cells.
fn swell_frame_for<'a>(
    bodies: impl IntoIterator<Item = &'a WaterBody>,
    camera: &LayerCamera,
    tier: WaterTier,
) -> Option<SwellFrame> {
    let (fov_y, _, far) = regia_render::view_math::usable_projection(camera.fov_y, camera.near, camera.far);
    let mut eye_height = f32::INFINITY;
    let mut reach = 0.0f32;
    for body in bodies {
        let (
            Some(swell),
            WaterShape::Plane {
                center, half_extent, ..
            },
        ) = (body.active_swell(), &body.shape)
        else {
            continue;
        };
        eye_height = eye_height.min((camera.eye[1] - center[1]).abs());
        let dx = (camera.eye[0] - center[0]).abs() + half_extent[0];
        let dz = (camera.eye[2] - center[2]).abs() + half_extent[1];
        reach = reach.max(dx.max(dz) + swell.max_shift());
    }
    if !eye_height.is_finite() {
        return None;
    }
    let reach = reach.min(far.max(1.0));
    let grid = RingGrid::for_view(fov_y, camera.height, eye_height, tier.ring_cells(), reach);
    let snap = 2.0 * f64::from(grid.cell());
    let origin = [
        (f64::from(camera.eye[0]) / snap).floor() * snap,
        (f64::from(camera.eye[2]) / snap).floor() * snap,
    ];
    let shifts = grid.ring_shifts(origin, [f64::from(camera.eye[0]), f64::from(camera.eye[2])]);
    Some(SwellFrame { grid, origin, shifts })
}

/// Entries of the water's bind group layout (group 0): binding 0..=12, the FFT ocean's maps and
/// uniform at 11 and 12 (ADR-1969). `water_body.wgsl` declares the same bindings; the test
/// `bind_layout_is_pinned` keeps the two and this list in step.
fn bind_layout_entries() -> [wgpu::BindGroupLayoutEntry; 13] {
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
    let texture = |binding: u32, sample_type: wgpu::TextureSampleType| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let sampler = |binding: u32| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    [
        uniform(0, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT),
        uniform(1, wgpu::ShaderStages::FRAGMENT),
        uniform(2, wgpu::ShaderStages::FRAGMENT),
        texture(3, wgpu::TextureSampleType::Depth),
        texture(4, wgpu::TextureSampleType::Float { filterable: true }),
        texture(5, wgpu::TextureSampleType::Float { filterable: true }),
        sampler(6),
        sampler(7),
        uniform(8, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT),
        wgpu::BindGroupLayoutEntry {
            binding: 9,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        },
        uniform(10, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT),
        wgpu::BindGroupLayoutEntry {
            binding: 11,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        },
        uniform(12, wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT),
    ]
}

/// How far the drawn FFT ocean on grids of `size` texels can move a vertex at `seconds`:
/// `(rise, shift)` in meters (ADR-2342). The query grid's maps at that instant
/// ([`super::ocean::frame_at`], the GPU's own on the 128 grid) give each band's peak; a larger
/// grid holds the same modes plus finer ones in its last band, which add at most their strict
/// bound (the tables' `rise_bound` and `shift_bound` difference). Bilinear sampling and the wave
/// fade never exceed a band's peak. Padded by 1 % and 1 cm against the GPU's float rounding.
///
/// Ring culling reads it through `ring_cull_reach`. This tight reach is what
/// `ocean_reach_bounds_the_drawn_maps` pins.
fn ocean_reach(ocean: &FftOcean, size: u32, seconds: f64) -> (f32, f32) {
    let _span = tracing::info_span!("water_ocean_reach").entered();
    let frame = super::ocean::frame_at(ocean, seconds);
    reach_from_frame(&frame, ocean, size)
}

/// `(rise, shift)` from a frame that already exists, plus the strict bound of any modes a larger
/// drawn grid adds past the query grid. Padded by 1 % and 1 cm.
fn reach_from_frame(frame: &super::ocean::OceanFrame, ocean: &FftOcean, size: u32) -> (f32, f32) {
    let (mut rise, mut shift) = (frame.max_rise(), frame.max_shift());
    if size > super::ocean::OCEAN_QUERY_SIZE {
        let fine = super::ocean::tables_for(ocean, size);
        let query = &frame.tables;
        rise += (fine.rise_bound - query.rise_bound).max(0.0);
        shift += (fine.shift_bound - query.shift_bound).max(0.0);
    }
    (rise * 1.01 + 0.01, shift * 1.01 + 0.01)
}

/// The reach ring culling uses: the tick's query frame's peaks ([`ocean_reach`]). A frame
/// already built for this tick (by a surface query or an earlier cull) is reused; a cold tick
/// builds it once, and later queries of the tick share it.
///
/// Not the strict spectral bound: that sums every mode's amplitude as if all peaked at one
/// texel, more than twice the frame's peak, and the widened chunk boxes then reach every plane
/// of the frustum, so the cull drops nothing over an ocean (`water_extras_frame`
/// `ring_culling_keeps_the_ocean`, ADR-5555). Ring culling is opt-in, and one query-grid
/// transform a tick is what it costs.
fn ring_cull_reach(ocean: &FftOcean, size: u32, seconds: f64) -> (f32, f32) {
    if let Some(frame) = super::ocean::cached_query_frame(ocean, seconds) {
        return reach_from_frame(&frame, ocean, size);
    }
    ocean_reach(ocean, size, seconds)
}

/// Whether `eye` is under `body`'s plane, and the surface height there.
///
/// An eye outside the rectangle widened by the shift bound, or above the rise bound, is not
/// under water and does not build an ocean frame. An eye that might be under samples the surface.
fn plane_eye_under(body: &WaterBody, eye: [f32; 3], clock: &WaterClock) -> Option<f32> {
    let WaterShape::Plane {
        center, half_extent, ..
    } = &body.shape
    else {
        return None;
    };
    let (rise, shift) = plane_query_reach(body, clock);
    let pad = 1.0e-3;
    let outside = (eye[0] - center[0]).abs() > half_extent[0] + shift + pad
        || (eye[2] - center[2]).abs() > half_extent[1] + shift + pad;
    if outside || eye[1] >= center[1] + rise + pad {
        return None;
    }
    let height = body.surface(eye[0], eye[2], clock)?.height;
    (height.is_finite() && eye[1] < height).then_some(height)
}

/// Upper bound on the height and the horizontal shift [`WaterBody::surface`] can answer, without
/// building an ocean frame when none is cached. An active ocean is the whole answer until a shore
/// is supplied, and this path has no shore.
fn plane_query_reach(body: &WaterBody, clock: &WaterClock) -> (f32, f32) {
    if let Some(ocean) = body.active_ocean() {
        if let Some(frame) = super::ocean::cached_query_frame(ocean, clock.seconds()) {
            return (frame.max_rise(), frame.max_shift());
        }
        let tables = super::ocean::tables_for(ocean, super::ocean::OCEAN_QUERY_SIZE);
        return (tables.rise_bound, tables.shift_bound);
    }
    body.active_swell()
        .map_or((0.0, 0.0), |swell| (swell.max_rise(), swell.max_shift()))
}

struct Pipelines {
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    built: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    /// The underwater fog pipelines (ADR-2342), built on first use.
    fog: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
}

impl Pipelines {
    /// The full-screen pass that fogs the scene for an eye under water.
    fn get_fog(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Option<wgpu::RenderPipeline> {
        if let Some((_, p)) = self.fog.iter().find(|(f, _)| *f == format) {
            return Some(p.clone());
        }
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let premult = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("water underwater fog"),
            layout: Some(&self.layout),
            vertex: wgpu::VertexState {
                module: &self.module,
                entry_point: Some("vs_fog"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.module,
                entry_point: Some("fs_fog"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: premult,
                        alpha: premult,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(device).as_deref(),
        });
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            tracing::error!(target: "regia_terrain", "water fog pipeline for {format:?}: {err}");
            return None;
        }
        self.fog.push((format, pipeline.clone()));
        Some(pipeline)
    }

    fn get(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> Option<wgpu::RenderPipeline> {
        if let Some((_, p)) = self.built.iter().find(|(f, _)| *f == format) {
            return Some(p.clone());
        }
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let premult = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        let attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 12,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 16,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 24,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 32,
                shader_location: 4,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 36,
                shader_location: 5,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 40,
                shader_location: 6,
            },
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("water bodies"),
            layout: Some(&self.layout),
            vertex: wgpu::VertexState {
                module: &self.module,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<WaterVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attributes,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: premult,
                        alpha: premult,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            // The pass's depth is the scene depth, read-only; the shader does its own depth test
            // so the wet band can darken banks in front of the surface.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(device).as_deref(),
        });
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            tracing::error!(target: "regia_terrain", "water pipeline for {format:?}: {err}");
            return None;
        }
        self.built.push((format, pipeline.clone()));
        Some(pipeline)
    }
}

/// Writes one shore field into an `Rg32Float` layer of `w` by `h` texels (depth, distance).
///
/// An inconsistent field stays at [`SHORE_DEEP`]. A consistent field is the rectangle
/// [`ShoreField::sample`] reads (`size[0]` by `size[1]`); samples past that prefix are not
/// texels. `texels` is `w * h` long.
fn fill_shore_layer(texels: &mut [[f32; 2]], w: u32, h: u32, field: &ShoreField) {
    debug_assert_eq!(texels.len(), w as usize * h as usize);
    texels.fill([SHORE_DEEP, SHORE_DEEP]);
    // An inconsistent hand-built field stays deep rather than aborting the upload.
    // `is_consistent` allows arrays longer than the declared rectangle. The tail is not
    // part of the field [`ShoreField::sample`] reads, and walking it indexes past the layer.
    if field.is_consistent() {
        let fw = field.size[0] as usize;
        let rows = (field.size[1] as usize).min(h as usize);
        let cols = fw.min(w as usize);
        for (j, (depth, distance)) in field
            .depth
            .chunks(fw)
            .zip(field.distance.chunks(fw))
            .take(rows)
            .enumerate()
        {
            for (i, (d, s)) in depth.iter().zip(distance).take(cols).enumerate() {
                texels[j * w as usize + i] = [*d, *s];
            }
        }
    }
}

/// Width and height of the shore texture: the largest consistent field, at least 1 and at most
/// `max_dim` (the device's 2D limit). An inconsistent field stays deep ([`fill_shore_layer`]),
/// so its declared size must not size the texture; a hand-built field wider than the device
/// limit is cut to it rather than failing texture validation, which is fatal.
fn shore_texture_dims(fields: &[Arc<ShoreField>], max_dim: u32) -> (u32, u32) {
    let consistent = || fields.iter().filter(|f| f.is_consistent());
    let max_dim = max_dim.max(1);
    let w = consistent().map(|f| f.size[0]).max().unwrap_or(1);
    let h = consistent().map(|f| f.size[1]).max().unwrap_or(1);
    (w.clamp(1, max_dim), h.clamp(1, max_dim))
}

/// The GPU copy of the frame's shore fields (binding 9 and 10).
struct ShoreGpu {
    buffer: wgpu::Buffer,
    view: wgpu::TextureView,
    /// The fields in the texture, in layer order.
    fields: Vec<Arc<ShoreField>>,
    /// Each body's field as the host set it (same order as `bodies`).
    per_body: Vec<Option<Arc<ShoreField>>>,
    /// Bumped whenever `view` is replaced (the bind group is rebuilt).
    generation: u64,
    /// Texels in the texture (all layers).
    texels: u64,
}

impl ShoreGpu {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water shore"),
            size: (16 * SHORE_VEC4S) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(&[[0.0f32; 4]; SHORE_VEC4S]));
        let (view, texels) = Self::texture(device, queue, &[]);
        Self {
            buffer,
            view,
            fields: Vec::new(),
            per_body: Vec::new(),
            generation: 0,
            texels,
        }
    }

    /// One `Rg32Float` layer per field (depth, distance), sized to the largest field; a 1 × 1
    /// stand-in without fields.
    fn texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        fields: &[Arc<ShoreField>],
    ) -> (wgpu::TextureView, u64) {
        let (w, h) = shore_texture_dims(fields, device.limits().max_texture_dimension_2d);
        let layers = fields.len().max(1) as u32;
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: layers,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("water shore fields"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut texels = vec![[0.0f32; 2]; w as usize * h as usize];
        for (layer, field) in fields.iter().enumerate() {
            fill_shore_layer(&mut texels, w, h, field);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&texels),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(8 * w),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        (view, u64::from(w) * u64::from(h) * u64::from(layers))
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, fields: &[Arc<ShoreField>]) {
        let (view, texels) = Self::texture(device, queue, fields);
        self.view = view;
        self.texels = texels;
        self.fields = fields.to_vec();
        self.generation = self.generation.wrapping_add(1);
    }
}

/// The GPU state behind [`WaterRenderer`].
struct WaterGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    bind_layout: wgpu::BindGroupLayout,
    pipelines: Pipelines,
    frame_buffer: wgpu::Buffer,
    bodies_buffer: wgpu::Buffer,
    obstacles_buffer: wgpu::Buffer,
    swell_buffer: wgpu::Buffer,
    ocean: OceanGpu,
    /// The caustic tile and shaft passes (ADR-2432).
    caustics: CausticsGpu,
    /// The tier the bodies' uniform was written for (caustics are tier gated).
    uniform_tier: Option<WaterTier>,
    /// The switch of [`WaterRenderer::set_ocean_draw`].
    ocean_draw: bool,
    /// The frame renderer's camera-cut count last seen ([`WaterRenderer::set_camera_cuts`]).
    camera_cuts: Option<u64>,
    /// Per body of [`WaterRenderer::set_bodies`], whether the last evolve drew its FFT ocean.
    oceans_drawn: Vec<bool>,
    ring_vertex_buffer: Option<wgpu::Buffer>,
    ring_index_buffer: Option<wgpu::Buffer>,
    ring_index_count: u32,
    /// The chunks of one body's copy of the grid, and its index count.
    ring_chunks: Vec<RingChunk>,
    ring_body_indices: u32,
    /// Index ranges the last frame draws (the ring mesh after frustum culling).
    ring_draws: Vec<std::ops::Range<u32>>,
    /// Cull the ring grid by the camera's frustum (off by default; ADR-2342).
    ring_culling: bool,
    /// The grid and swell bodies the ring buffers were built for.
    ring_key: Option<RingKey>,
    /// `(table slot, index in bodies)` of every swell plane.
    swell_slots: Vec<(u32, usize)>,
    /// Swell uniform scratch, filled in place each frame.
    swell_packed: Vec<[f32; 4]>,
    /// Frustum boxes for the swell planes, reused each frame.
    cull_bodies: Vec<CullBody>,
    /// The outdoors the next view sees from inside a cell ([`TranslucentLayer::set_outdoor_portals`],
    /// ADR-8843), and the same moved into the bodies' world space.
    portals: regia_render::viscells::OutdoorPortals,
    portals_world: regia_render::viscells::OutdoorPortals,
    /// The frame's eye is inside a cell: cull through `portals_world`.
    portal_restricted: bool,
    /// Each mesh body's index range and vertex box, in draw order.
    mesh_bodies: Vec<(std::ops::Range<u32>, [f32; 3], [f32; 3])>,
    /// The mesh index ranges the last frame draws (every body, or the admitted ones).
    mesh_draws: Vec<std::ops::Range<u32>>,
    vertex_buffer: Option<wgpu::Buffer>,
    index_buffer: Option<wgpu::Buffer>,
    index_count: u32,
    ripple: wgpu::TextureView,
    ripple_sampler: wgpu::Sampler,
    clamp_sampler: wgpu::Sampler,
    stand_in_scene: wgpu::TextureView,
    depth: Option<wgpu::TextureView>,
    scene: Option<wgpu::TextureView>,
    bind_group: Option<(
        wgpu::BindGroup,
        wgpu::TextureView,
        Option<wgpu::TextureView>,
        u64,
        u64,
    )>,
    /// The bodies as the host set them.
    source: Vec<WaterBody>,
    /// The bodies as drawn: an ocean without a swell carries its stand-in.
    bodies: Vec<WaterBody>,
    /// Each body's mesh slot (same order as `bodies`).
    slots: Vec<Option<u32>>,
    shore: ShoreGpu,
    obstacles: Vec<WaterObstacle>,
    clock: WaterClock,
    tier: WaterTier,
    sky: Option<WaterSky>,
    ambient_lux: f32,
    enabled: bool,
    debug: WaterDebug,
    /// This frame composites (decided in `simulate`).
    composite: bool,
    /// `simulate` ran with a depth view this frame and something is drawn.
    armed: bool,
    /// The local origin in the bodies' space (spec 78 § 12.2): zero until a floating-origin rebase.
    origin_offset: [f32; 3],
    last_uniform: Option<FrameUniform>,
    stats: WaterFrameStats,
    /// The underwater view is on (ADR-2342; off by default).
    underwater: bool,
    /// Each ring snaps on its own lattice (ADR-2342; off by default).
    ring_snap: bool,
    /// Time the water draw every this many frames (0 = never, the default).
    timing_every: u32,
    /// Frames since the last timed draw.
    timing_frames: u32,
}

impl WaterGpu {
    fn new(device: &RenderDevice) -> Self {
        let d = &device.device;
        let entries = bind_layout_entries();
        let bind_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("water bodies layout"),
            entries: &entries,
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("water_body.wgsl"),
            source: regia_render::wgsl_source(SHADER),
        });
        let caustics = CausticsGpu::new(d, &device.queue, module.clone());
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("water bodies pipeline layout"),
            bind_group_layouts: &[Some(&bind_layout), Some(&caustics.layout_g1)],
            immediate_size: 0,
        });
        let swell_buffer = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water swell"),
            size: (16 * SWELL_VEC4S) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Zeroed: no body is a swell plane until `prepare_swell` says so.
        device.queue.write_buffer(
            &swell_buffer,
            0,
            bytemuck::cast_slice(&[[0.0f32; 4]; SWELL_VEC4S]),
        );
        let buffer = |label: &'static str, size: usize| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let frame_buffer = buffer("water frame", size_of::<FrameUniform>());
        let bodies_buffer = buffer("water bodies", size_of::<BodyUniform>() * MAX_BODIES);
        let obstacles_buffer = buffer("water obstacles", 16 * MAX_OBSTACLES);
        let levels = ripple_levels();
        let ripple_tex = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("water ripple map"),
            size: wgpu::Extent3d {
                width: RIPPLE_MAP_SIZE,
                height: RIPPLE_MAP_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: levels.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level, data) in levels.iter().enumerate() {
            device.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &ripple_tex,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &data.texels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(data.size * 4),
                    rows_per_image: Some(data.size),
                },
                wgpu::Extent3d {
                    width: data.size,
                    height: data.size,
                    depth_or_array_layers: 1,
                },
            );
        }
        let ripple = ripple_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let ripple_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("water ripple sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 8,
            ..Default::default()
        });
        let clamp_sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("water scene sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let stand_in = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("water scene stand-in"),
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
        device.queue.write_texture(
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
        let shore = ShoreGpu::new(d, &device.queue);
        let ocean = OceanGpu::new(d, &device.queue);
        Self {
            device: d.clone(),
            queue: device.queue.clone(),
            bind_layout,
            pipelines: Pipelines {
                module,
                layout,
                built: Vec::new(),
                fog: Vec::new(),
            },
            frame_buffer,
            bodies_buffer,
            obstacles_buffer,
            swell_buffer,
            ocean,
            caustics,
            uniform_tier: None,
            ocean_draw: true,
            camera_cuts: None,
            oceans_drawn: Vec::new(),
            ring_vertex_buffer: None,
            ring_index_buffer: None,
            ring_index_count: 0,
            ring_chunks: Vec::new(),
            ring_body_indices: 0,
            ring_draws: Vec::new(),
            ring_culling: false,
            ring_key: None,
            swell_slots: Vec::new(),
            swell_packed: Vec::new(),
            cull_bodies: Vec::new(),
            portals: regia_render::viscells::OutdoorPortals::default(),
            portals_world: regia_render::viscells::OutdoorPortals::default(),
            portal_restricted: false,
            mesh_bodies: Vec::new(),
            mesh_draws: Vec::new(),
            vertex_buffer: None,
            index_buffer: None,
            index_count: 0,
            ripple,
            ripple_sampler,
            clamp_sampler,
            stand_in_scene: stand_in.create_view(&wgpu::TextureViewDescriptor::default()),
            depth: None,
            scene: None,
            slots: Vec::new(),
            shore,
            bind_group: None,
            source: Vec::new(),
            bodies: Vec::new(),
            obstacles: Vec::new(),
            clock: WaterClock::default(),
            tier: WaterTier::High,
            sky: None,
            ambient_lux: DEFAULT_AMBIENT_LUX,
            enabled: true,
            debug: WaterDebug::Off,
            composite: false,
            armed: false,
            origin_offset: [0.0; 3],
            last_uniform: None,
            stats: WaterFrameStats {
                gpu_ms_supported: device.device.features().contains(wgpu::Features::TIMESTAMP_QUERY),
                ..WaterFrameStats::default()
            },
            underwater: false,
            ring_snap: false,
            timing_every: 0,
            timing_frames: 0,
        }
    }

    fn set_bodies(&mut self, bodies: &[&WaterBody]) {
        if bodies.len() == self.source.len() && bodies.iter().zip(&self.source).all(|(a, b)| *a == b) {
            return;
        }
        let _span = tracing::info_span!("water_upload").entered();
        // Lingering foam belongs to the bodies it was made on (ADR-2843).
        self.ocean.reset_history();
        self.source = bodies.iter().map(|b| (*b).clone()).collect();
        // An ocean without a swell draws on the ring grid with its stand-in.
        let drawn: Vec<WaterBody> = self
            .source
            .iter()
            .map(|b| {
                let mut drawn = b.clone();
                if b.active_ocean().is_some() {
                    drawn.swell = b.fallback_swell().map(std::borrow::Cow::into_owned);
                }
                drawn
            })
            .collect();
        let refs: Vec<&WaterBody> = drawn.iter().collect();
        let bodies = refs.as_slice();
        let (mesh, slots) = build_mesh(bodies);
        let mut obstacles = Vec::new();
        for (body, slot) in bodies.iter().zip(&slots) {
            if slot.is_some() {
                obstacles.extend_from_slice(&body.obstacles);
            }
        }
        self.bodies = bodies.iter().map(|b| (*b).clone()).collect();
        self.slots = slots.clone();
        self.upload_body_uniforms();
        self.swell_slots = slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                slot.filter(|_| self.bodies[i].active_swell().is_some())
                    .map(|s| (s, i))
            })
            .collect();
        self.ring_key = None;
        self.stats.swell_bodies = self.swell_slots.len() as u32;
        self.obstacles = obstacles;
        self.index_count = mesh.indices.len() as u32;
        self.mesh_bodies = mesh_body_ranges(&mesh);
        self.stats.bodies = slots.iter().flatten().count() as u32;
        self.stats.vertices = mesh.vertices.len() as u32;
        self.stats.triangles = self.index_count / 3;
        if mesh.indices.is_empty() {
            self.vertex_buffer = None;
            self.index_buffer = None;
            return;
        }
        let vb_bytes: &[u8] = bytemuck::cast_slice(&mesh.vertices);
        let ib_bytes: &[u8] = bytemuck::cast_slice(&mesh.indices);
        let fits = |b: &Option<wgpu::Buffer>, n: usize| b.as_ref().is_some_and(|b| b.size() >= n as u64);
        if !fits(&self.vertex_buffer, vb_bytes.len()) {
            self.vertex_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("water vertices"),
                size: (vb_bytes.len() as u64).next_power_of_two(),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if !fits(&self.index_buffer, ib_bytes.len()) {
            self.index_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("water indices"),
                size: (ib_bytes.len() as u64).next_power_of_two(),
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if let (Some(vb), Some(ib)) = (&self.vertex_buffer, &self.index_buffer) {
            self.queue.write_buffer(vb, 0, vb_bytes);
            self.queue.write_buffer(ib, 0, ib_bytes);
        }
    }

    /// Writes the bodies' uniform for the current tier (caustics are gated by it).
    fn upload_body_uniforms(&mut self) {
        let mut uniforms = [BodyUniform::zeroed(); MAX_BODIES];
        for (body, slot) in self.bodies.iter().zip(&self.slots) {
            if let Some(slot) = slot {
                uniforms[*slot as usize] = BodyUniform::of(body, self.tier);
            }
        }
        self.queue
            .write_buffer(&self.bodies_buffer, 0, bytemuck::cast_slice(&uniforms));
        self.uniform_tier = Some(self.tier);
    }

    /// Records the caustic tile pass and, under a surface whose body asks for them, the shaft
    /// pass (ADR-2432). Nothing is recorded, and nothing is created, while no body has caustics
    /// on at this tier.
    fn simulate_caustics(
        &mut self,
        depth: &wgpu::TextureView,
        camera: &LayerCamera,
        encoder: &mut wgpu::CommandEncoder,
        underwater: Option<(u32, f32)>,
    ) {
        self.caustics.stats.bodies = 0;
        self.caustics.stats.shafts_drawn = false;
        self.caustics.stats.from_above = false;
        let mut first: Option<&WaterCaustics> = None;
        // Steps, accumulate, and the body seen from above (slot, level).
        type ShaftPick = (u32, bool, Option<(u32, f32)>);
        let mut shafts: Option<ShaftPick> = None;
        for (body, slot) in self.bodies.iter().zip(&self.slots) {
            let Some(slot) = slot else { continue };
            let c = &body.look.caustics;
            if !c.active_at(self.tier) {
                continue;
            }
            self.caustics.stats.bodies += 1;
            first.get_or_insert(c);
            if c.shaft_steps == 0 {
                continue;
            }
            match underwater {
                Some((s, _)) if s == *slot => shafts = Some((c.shaft_steps, c.shaft_accumulate, None)),
                // Seen from above (ADR-3251, opt in): the first plane below the eye that asks.
                None if shafts.is_none() && c.shafts_from_above => {
                    if let WaterShape::Plane { center, .. } = &body.shape
                        && center[1] < camera.eye[1]
                    {
                        shafts = Some((c.shaft_steps, c.shaft_accumulate, Some((*slot, center[1]))));
                    }
                }
                _ => {}
            }
        }
        let Some(first) = first else {
            self.caustics.no_shafts();
            return;
        };
        let sun =
            regia_core::glam::Vec3::from_array(camera.sun_direction).normalize_or(regia_core::glam::Vec3::Y);
        if sun.y < 0.02 || !camera.sun_illuminance.is_finite() || camera.sun_illuminance <= 0.0 {
            self.caustics.no_shafts();
            return;
        }
        let params = caustics::tile_params(&self.clock, first.speed, sun.to_array());
        let group0 = self.caustics.compute_group0(
            &self.frame_buffer,
            &self.bodies_buffer,
            depth,
            &self.ripple_sampler,
        );
        self.caustics.record_tile(encoder, &group0, &self.ripple, &params);
        match shafts {
            Some((steps, accumulate, from_above)) => {
                let shaft = ShaftFrame {
                    viewport: [camera.width, camera.height],
                    steps,
                    accumulate,
                    from_above,
                    view_proj: camera_view_proj(camera).to_cols_array_2d(),
                };
                self.caustics.record_shafts(encoder, &group0, &shaft);
                self.caustics.stats.shafts_drawn = true;
            }
            None => self.caustics.no_shafts(),
        }
    }

    fn bind_group(&mut self) -> Option<wgpu::BindGroup> {
        let depth = self.depth.clone()?;
        let scene = if self.composite { self.scene.clone() } else { None };
        if let Some((group, d, s, g, o)) = &self.bind_group
            && *d == depth
            && *s == scene
            && *g == self.shore.generation
            && *o == self.ocean.generation
        {
            return Some(group.clone());
        }
        let scene_view = scene.as_ref().unwrap_or(&self.stand_in_scene);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water bodies bind"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.frame_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.bodies_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.obstacles_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&depth),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(scene_view),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&self.ripple),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::Sampler(&self.ripple_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::Sampler(&self.clamp_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: self.swell_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: wgpu::BindingResource::TextureView(&self.shore.view),
                },
                wgpu::BindGroupEntry {
                    binding: 10,
                    resource: self.shore.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 11,
                    resource: wgpu::BindingResource::TextureView(self.ocean.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 12,
                    resource: self.ocean.uniform.as_entire_binding(),
                },
            ],
        });
        self.bind_group = Some((
            group.clone(),
            depth,
            scene,
            self.shore.generation,
            self.ocean.generation,
        ));
        Some(group)
    }

    /// Sizes the ring grid for `camera`, rebuilds its buffers when the grid or the swell bodies
    /// changed, and writes the swell uniform. Returns the frame's grid and center.
    fn prepare_swell(&mut self, camera: &LayerCamera) -> Option<SwellFrame> {
        if self.swell_slots.is_empty() {
            self.ring_index_count = 0;
            self.ring_key = None;
            self.stats.swell_waves = 0;
            self.stats.swell_waves_dropped = 0;
            self.stats.ring_cell_m = 0.0;
            self.stats.ring_half_cells = 0;
            self.stats.rings = 0;
            self.stats.ring_vertices = 0;
            self.stats.ring_triangles = 0;
            self.stats.ring_triangles_drawn = 0;
            self.stats.ring_chunks = 0;
            self.stats.ring_chunks_culled = 0;
            self.ring_draws.clear();
            self.stats.ring_origin = [0.0, 0.0];
            return None;
        }
        let _span = tracing::info_span!("water_swell").entered();
        let mut frame = swell_frame_for(
            self.swell_slots.iter().map(|(_, i)| &self.bodies[*i]),
            camera,
            self.tier,
        )?;
        if !self.ring_snap {
            // Off: every ring rides the inner grid's snap, as before the per-ring snap.
            frame.shifts.iter_mut().for_each(|d| *d = [0.0, 0.0]);
        }
        let seconds = self.clock.seconds();
        let cap = self.tier.swell_waves();
        let (waves, dropped) = {
            let grid = frame.grid;
            let origin = frame.origin;
            let WaterGpu {
                swell_packed,
                swell_slots,
                bodies,
                ..
            } = &mut *self;
            swell_uniform_into(
                swell_packed,
                swell_slots.iter().map(|(slot, i)| (*slot, &bodies[*i])),
                &grid,
                origin,
                &frame.shifts,
                seconds,
                cap,
            )
        };
        for (slot, _) in &self.swell_slots {
            let slot = (*slot as usize).min(MAX_BODIES - 1);
            if self.ocean.selection.slots[slot] {
                self.swell_packed[1 + 3 * slot + 2][2] = 1.0;
            }
        }
        self.ocean.write_uniform(&self.queue, frame.origin);
        self.queue
            .write_buffer(&self.swell_buffer, 0, bytemuck::cast_slice(&self.swell_packed));
        let unchanged = self.ring_key.as_ref().is_some_and(|(grid, flows)| {
            *grid == frame.grid
                && flows.len() == self.swell_slots.len()
                && flows
                    .iter()
                    .zip(&self.swell_slots)
                    .all(|((slot, flow), (s, i))| *slot == *s && *flow == Self::plane_flow(&self.bodies[*i]))
        });
        if !unchanged {
            let key_bodies: Vec<(u32, [f32; 2])> = self
                .swell_slots
                .iter()
                .map(|(slot, i)| (*slot, Self::plane_flow(&self.bodies[*i])))
                .collect();
            let key = (frame.grid, key_bodies);
            let _span = tracing::info_span!("water_ring_upload").entered();
            let mesh = build_ring_mesh(&key.0, &key.1);
            let (_, grid_indices, chunks) = build_ring_grid_chunks(&key.0);
            self.ring_body_indices = grid_indices.len() as u32;
            self.stats.ring_chunks = (chunks.len() * key.1.len()) as u32;
            self.ring_chunks = chunks;
            let vb_bytes: &[u8] = bytemuck::cast_slice(&mesh.vertices);
            let ib_bytes: &[u8] = bytemuck::cast_slice(&mesh.indices);
            let fits = |b: &Option<wgpu::Buffer>, n: usize| b.as_ref().is_some_and(|b| b.size() >= n as u64);
            if !fits(&self.ring_vertex_buffer, vb_bytes.len()) {
                self.ring_vertex_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("water ring vertices"),
                    size: (vb_bytes.len() as u64).next_power_of_two().max(4),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
            }
            if !fits(&self.ring_index_buffer, ib_bytes.len()) {
                self.ring_index_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("water ring indices"),
                    size: (ib_bytes.len() as u64).next_power_of_two().max(4),
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
            }
            if let (Some(vb), Some(ib)) = (&self.ring_vertex_buffer, &self.ring_index_buffer) {
                self.queue.write_buffer(vb, 0, vb_bytes);
                self.queue.write_buffer(ib, 0, ib_bytes);
            }
            self.ring_index_count = mesh.indices.len() as u32;
            self.stats.ring_vertices = mesh.vertices.len() as u32;
            self.stats.ring_triangles = self.ring_index_count / 3;
            self.ring_key = Some(key);
        }
        let culled = self.cull_ring(camera, frame.origin);
        self.stats.ring_chunks_culled = culled;
        self.stats.ring_triangles_drawn = self.ring_draws.iter().map(|r| (r.end - r.start) / 3).sum();
        self.stats.swell_waves = waves;
        self.stats.swell_waves_dropped = dropped;
        self.stats.ring_cell_m = frame.grid.cell();
        self.stats.ring_half_cells = frame.grid.half_cells;
        self.stats.rings = frame.grid.rings;
        self.stats.ring_origin = frame.origin;
        Some(frame)
    }

    fn plane_flow(body: &WaterBody) -> [f32; 2] {
        match &body.shape {
            WaterShape::Plane { flow, .. } => *flow,
            WaterShape::Ribbon { .. } => [0.0, 0.0],
        }
    }

    /// Chooses the mesh index ranges to draw (ADR-8843): every body, or on a frame whose eye is
    /// inside a cell the bodies whose vertex box some outdoor portal frustum admits.
    fn cull_mesh_bodies(&mut self) {
        self.mesh_draws.clear();
        let mut culled = 0u32;
        if self.portal_restricted {
            for (range, lo, hi) in &self.mesh_bodies {
                if !self
                    .portals_world
                    .admits_box(*lo, *hi, regia_render::viscells::OUTDOOR_NODE_MARGIN_M)
                {
                    culled += 1;
                    continue;
                }
                match self.mesh_draws.last_mut() {
                    Some(last) if last.end == range.start => last.end = range.end,
                    _ => self.mesh_draws.push(range.clone()),
                }
            }
        } else if self.index_count > 0 {
            self.mesh_draws.push(0..self.index_count);
        }
        self.stats.portal_restricted = self.portal_restricted;
        self.stats.portal_culled_bodies = culled;
        self.stats.bodies_drawn = self.mesh_bodies.len() as u32 - culled;
    }

    /// Chooses the ring index ranges to draw: all of them with culling off, otherwise the chunks
    /// whose box (widened by the swell's reach, doubled for shoaling) meets the camera's frustum.
    /// On a frame whose eye is inside a cell, the chunks no outdoor portal frustum admits are left
    /// out too (ADR-8843, whether the frustum cull is on or not). Writes `ring_draws` and returns
    /// the number of chunks the frustum left out.
    fn cull_ring(&mut self, camera: &LayerCamera, origin: [f64; 2]) -> u32 {
        let _span = tracing::info_span!("water_ring_cull").entered();
        self.stats.portal_culled_ring_chunks = 0;
        if !self.ring_culling && !self.portal_restricted {
            self.ring_draws.clear();
            self.ring_draws.push(0..self.ring_index_count);
            return 0;
        }
        let per_body = self.ring_body_indices;
        let ocean_size = self.ocean.size();
        let seconds = self.clock.seconds();
        self.cull_bodies.clear();
        for (i, (slot, index)) in self.swell_slots.iter().enumerate() {
            let body = &self.bodies[*index];
            let (Some(swell), WaterShape::Plane { center, .. }) = (body.active_swell(), &body.shape) else {
                continue;
            };
            // The vertex is the shoaled swell blended with the frame's ocean by a weight in
            // 0..1, so it stays inside the larger of the two reaches (ADR-2342).
            let mut rise = swell.max_rise() * super::shore::SHOAL_MAX;
            let mut shift = swell.max_shift() * super::shore::SHOAL_MAX;
            let rides = self.ocean.selection.slots[(*slot as usize).min(MAX_BODIES - 1)];
            if let Some(ocean) = body.active_ocean().filter(|_| rides && ocean_size > 0) {
                let (r, d) = ring_cull_reach(ocean, ocean_size, seconds);
                rise = rise.max(r);
                shift = shift.max(d);
            }
            self.cull_bodies.push(CullBody {
                index_base: i as u32 * per_body,
                level: center[1],
                rise,
                shift,
            });
        }
        let planes = frustum_planes(&camera_view_proj(camera));
        let WaterGpu {
            ring_chunks,
            cull_bodies,
            ring_draws,
            ring_culling,
            portal_restricted,
            portals_world,
            ..
        } = self;
        let (culled, portal_culled) = visible_ranges_through_into(
            ring_chunks,
            cull_bodies,
            origin,
            (*ring_culling).then_some(&planes),
            (*portal_restricted).then_some(&*portals_world),
            ring_draws,
        );
        self.stats.portal_culled_ring_chunks = portal_culled;
        culled
    }

    /// Picks the frame's ocean and evolves its maps to the clock on `encoder`, once per instant:
    /// the `after_opaque` pass runs it first, `simulate` again for frames that skip the slot.
    fn evolve_ocean(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let size = if self.enabled && self.ocean_draw {
            self.tier.ocean_size()
        } else {
            0
        };
        self.ocean
            .select(&self.device, &self.queue, &self.bodies, &self.slots, size);
        self.ocean.evolve(&self.queue, encoder, self.clock.seconds());
        self.stats.ocean_bodies = self.ocean.selection.bodies;
        self.stats.ocean_dropped = self.ocean.selection.dropped;
        self.stats.ocean_refused = self.ocean.selection.refused;
        // Which bodies the frame drew on the ocean (ADR-3251), for queries that follow it.
        self.oceans_drawn.clear();
        let selection = &self.ocean.selection;
        self.oceans_drawn
            .extend(self.bodies.iter().zip(&self.slots).map(|(body, slot)| {
                body.active_ocean().is_some()
                    && slot.is_some_and(|s| selection.slots[(s as usize).min(MAX_BODIES - 1)])
            }));
        self.stats.ocean_size = self.ocean.size();
        self.stats.ocean_dispatches = self.ocean.dispatches;
        self.stats.ocean_evolves = self.ocean.evolves;
        self.stats.ocean_history_bytes = self.ocean.history_bytes();
        self.stats.ocean_history_steps = self.ocean.history_steps;
        self.stats.ocean_history_restarts = self.ocean.history_restarts;
    }

    fn simulate(
        &mut self,
        depth: &wgpu::TextureView,
        camera: &LayerCamera,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        // Inside a cell only the water its outdoor portals show is drawn (ADR-8843): the list is
        // matched on the frame's eye, then moved into the bodies' space like the camera.
        self.portal_restricted = self.portals.restricts(camera.eye);
        if self.portal_restricted {
            self.portals
                .shift_into(self.origin_offset, &mut self.portals_world);
        }
        // Bodies keep their authored world-anchored positions: the camera moves into their space
        // here, and everything below runs as it did (spec 78 § 12.2, ADR-2022).
        let offset = self.origin_offset;
        let world_camera = LayerCamera {
            eye: std::array::from_fn(|axis| camera.eye[axis] + offset[axis]),
            target: std::array::from_fn(|axis| camera.target[axis] + offset[axis]),
            ..*camera
        };
        let camera = if offset == [0.0; 3] { camera } else { &world_camera };
        self.armed = false;
        if !self.enabled || (self.index_count == 0 && self.swell_slots.is_empty()) {
            return;
        }
        let _span = tracing::info_span!("water_prepare").entered();
        self.prepare_shore();
        self.evolve_ocean(encoder);
        self.cull_mesh_bodies();
        self.prepare_swell(camera);
        self.depth = Some(depth.clone());
        // A composite needs the HDR scene copy; without it (display pass, copy refused) the
        // frame blends.
        self.composite = self.tier.composites() && camera.hdr && self.scene.is_some();
        let obstacles = nearest_obstacles(&self.obstacles, camera.eye, self.tier.obstacle_budget());
        let mut packed = [[0.0f32; 4]; MAX_OBSTACLES];
        packed[..obstacles.len()].copy_from_slice(&obstacles);
        self.queue
            .write_buffer(&self.obstacles_buffer, 0, bytemuck::cast_slice(&packed));
        let sky = self
            .sky
            .unwrap_or_else(|| WaterSky::from_ambient_lux(self.ambient_lux));
        let underwater = self.underwater_at(camera.eye);
        self.stats.underwater = underwater.is_some();
        self.stats.underwater_depth_m = underwater.map_or(0.0, |(_, y)| y - camera.eye[1]);
        let uniform = frame_uniform(
            camera,
            &self.clock,
            &FrameInputs {
                tier: self.tier,
                composite: self.composite,
                obstacles: obstacles.len() as u32,
                sky,
                ambient_lux: self.ambient_lux,
                debug: self.debug,
                underwater_view: self.underwater,
                underwater,
            },
        );
        self.queue
            .write_buffer(&self.frame_buffer, 0, bytemuck::bytes_of(&uniform));
        self.last_uniform = Some(uniform);
        if self.uniform_tier != Some(self.tier) {
            self.upload_body_uniforms();
        }
        self.simulate_caustics(depth, camera, encoder, underwater);
        self.stats.obstacles = obstacles.len() as u32;
        self.stats.composite = self.composite;
        self.stats.hdr = camera.hdr;
        self.stats.tier = self.tier;
        self.stats.tick = self.clock.tick;
        self.armed = true;
        if self.timing_every > 0 {
            self.timing_frames += 1;
            if self.timing_frames >= self.timing_every {
                self.timing_frames = 0;
                // A failed timing (no timestamps, readback error) leaves the last value.
                let _ = self.time_draw(camera);
            }
        }
    }

    /// Draws the armed water once into a private target (the viewport clamped to
    /// [`TIMING_MAX_SIDE`] per side) bracketed by timestamps, and stores the milliseconds in the
    /// stats. The frame's own draw is unaffected. `Ok(None)` when the device has no
    /// `TIMESTAMP_QUERY` or nothing is armed.
    fn time_draw(&mut self, camera: &LayerCamera) -> Result<Option<f32>, RenderError> {
        if !self.stats.gpu_ms_supported || !self.armed {
            return Ok(None);
        }
        let _span = tracing::info_span!("water_gpu_time").entered();
        let (w, h) = (
            camera.width.clamp(1, TIMING_MAX_SIDE),
            camera.height.clamp(1, TIMING_MAX_SIDE),
        );
        let format = wgpu::TextureFormat::Rgba16Float;
        let target = |label: &str, format| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
        };
        let color = target("water timing color", format).create_view(&wgpu::TextureViewDescriptor::default());
        let depth = target("water timing depth", wgpu::TextureFormat::Depth32Float)
            .create_view(&wgpu::TextureViewDescriptor::default());
        let queries = self.device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("water timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: 2,
        });
        let buffer = |label: &str, usage| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 16,
                usage,
                mapped_at_creation: false,
            })
        };
        let resolve = buffer(
            "water timestamp resolve",
            wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        );
        let readback = buffer(
            "water timestamp readback",
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        // `record` consumes the armed state and counts a drawn frame: put both back after.
        let (armed, drawn) = (self.armed, self.stats.frames_drawn);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("water timing"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("water timing"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: None,
                    stencil_ops: None,
                }),
                timestamp_writes: Some(wgpu::RenderPassTimestampWrites {
                    query_set: &queries,
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.record(&mut pass, format);
        }
        self.armed = armed;
        self.stats.frames_drawn = drawn;
        encoder.resolve_query_set(&queries, 0..2, &resolve, 0);
        encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, 16);
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(RenderError::Poll)?;
        rx.recv()
            .map_err(|_| RenderError::Readback("map callback dropped".into()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let ticks = {
            let mapped = readback
                .slice(..)
                .get_mapped_range()
                .map_err(|e| RenderError::Readback(e.to_string()))?;
            let first = u64::from_le_bytes(mapped[0..8].try_into().unwrap_or_default());
            let last = u64::from_le_bytes(mapped[8..16].try_into().unwrap_or_default());
            last.saturating_sub(first)
        };
        readback.unmap();
        let ms = ticks as f32 * self.queue.get_timestamp_period() * 1.0e-6;
        self.stats.gpu_ms = ms;
        self.stats.gpu_ms_pixels = w * h;
        self.stats.gpu_ms_samples += 1;
        Ok(Some(ms))
    }

    /// The plane body (table slot) whose surface lies above `eye`, and the surface's height there
    /// (the swell included): `None` when underwater is off, the eye is over every plane, or it is
    /// outside them.
    fn underwater_at(&self, eye: [f32; 3]) -> Option<(u32, f32)> {
        if !self.underwater {
            return None;
        }
        // The authored bodies: an ocean plane answers its FFT surface, not the drawn stand-in.
        self.source.iter().zip(&self.slots).find_map(|(body, slot)| {
            let slot = (*slot)?;
            if !matches!(body.shape, WaterShape::Plane { .. }) {
                return None;
            }
            let height = plane_eye_under(body, eye, &self.clock)?;
            Some((slot, height))
        })
    }

    /// Binds this frame's shore fields (High and Ultra; none on Low): the texture is rebuilt only
    /// when the set of fields changes, the uniform (field rectangles, slot map) every frame.
    fn prepare_shore(&mut self) {
        let gate = self.tier != WaterTier::Low;
        let mut layers: Vec<Arc<ShoreField>> = Vec::new();
        let mut map = [0.0f32; MAX_BODIES];
        let mut dropped = 0u32;
        if gate {
            for ((body, slot), field) in self.bodies.iter().zip(&self.slots).zip(&self.shore.per_body) {
                let (Some(slot), Some(field)) = (slot, field) else {
                    continue;
                };
                if !matches!(body.shape, WaterShape::Plane { .. }) {
                    continue;
                }
                let layer = match layers.iter().position(|f| Arc::ptr_eq(f, field)) {
                    Some(i) => i,
                    None if layers.len() < MAX_SHORE_FIELDS => {
                        layers.push(Arc::clone(field));
                        layers.len() - 1
                    }
                    None => {
                        dropped += 1;
                        continue;
                    }
                };
                map[(*slot as usize).min(MAX_BODIES - 1)] = (layer + 1) as f32;
            }
        }
        let same = layers.len() == self.shore.fields.len()
            && layers
                .iter()
                .zip(&self.shore.fields)
                .all(|(a, b)| Arc::ptr_eq(a, b));
        if !same {
            // A changed set re-uploads; an emptied one (Low, switch off, fields removed) frees
            // the layers for the 1 × 1 stand-in.
            let _span = tracing::info_span!("water_shore_upload").entered();
            self.shore.upload(&self.device, &self.queue, &layers);
        }
        self.stats.shore_texels = if layers.is_empty() { 0 } else { self.shore.texels };
        let mut packed = [[0.0f32; 4]; SHORE_VEC4S];
        for (i, f) in layers.iter().enumerate() {
            packed[2 * i] = [f.origin[0], f.origin[1], f.cell, 0.0];
            packed[2 * i + 1] = [f.size[0] as f32, f.size[1] as f32, 0.0, 0.0];
        }
        for (i, chunk) in map.as_chunks::<4>().0.iter().enumerate() {
            packed[2 * MAX_SHORE_FIELDS + i] = *chunk;
        }
        self.queue
            .write_buffer(&self.shore.buffer, 0, bytemuck::cast_slice(&packed));
        self.stats.shore_fields = layers.len() as u32;
        self.stats.shore_fields_dropped = dropped;
    }

    fn run_probe(&self, points: &[(u32, [f32; 2])]) -> Result<Vec<[f32; 3]>, RenderError> {
        use wgpu::util::DeviceExt;
        if points.is_empty() {
            return Ok(Vec::new());
        }
        let d = &self.device;
        let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let source = format!("{SHADER}\n{PROBE_ENTRY}");
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("water ring probe"),
            source: regia_render::wgsl_source(&source),
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("water ring probe"),
            layout: None,
            module: &module,
            entry_point: Some("cs_probe"),
            compilation_options: Default::default(),
            cache: None,
        });
        let input: Vec<[f32; 4]> = points
            .iter()
            .map(|(body, l)| [l[0], l[1], *body as f32, 0.0])
            .collect();
        let size = (input.len() * 16) as u64;
        let in_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("water probe in"),
            contents: bytemuck::cast_slice(&input),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let out_buf = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water probe out"),
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("water probe readback"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let group0 = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water probe swell"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: self.swell_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: wgpu::BindingResource::TextureView(&self.shore.view),
                },
                wgpu::BindGroupEntry {
                    binding: 10,
                    resource: self.shore.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 11,
                    resource: wgpu::BindingResource::TextureView(self.ocean.view()),
                },
                wgpu::BindGroupEntry {
                    binding: 12,
                    resource: self.ocean.uniform.as_entire_binding(),
                },
            ],
        });
        let group1 = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("water probe io"),
            layout: &pipeline.get_bind_group_layout(1),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: in_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: out_buf.as_entire_binding(),
                },
            ],
        });
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("water probe"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("water probe"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &group0, &[]);
            pass.set_bind_group(1, &group1, &[]);
            pass.dispatch_workgroups((points.len() as u32).div_ceil(64), 1, 1);
        }
        encoder.copy_buffer_to_buffer(&out_buf, 0, &readback, 0, size);
        self.queue.submit([encoder.finish()]);
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            return Err(RenderError::Shader(format!("water ring probe: {err}")));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        d.poll(wgpu::PollType::wait_indefinitely())
            .map_err(RenderError::Poll)?;
        rx.recv()
            .map_err(|_| RenderError::Readback("map callback dropped".into()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mapped = readback
            .slice(..)
            .get_mapped_range()
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let out: Vec<[f32; 3]> = bytemuck::cast_slice::<u8, [f32; 4]>(&mapped)
            .iter()
            .map(|v| [v[0], v[1], v[2]])
            .collect();
        drop(mapped);
        readback.unmap();
        Ok(out)
    }

    fn record(&mut self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        if !self.armed {
            return;
        }
        let _span = tracing::info_span!("water_draw").entered();
        // A display target that stores encoded bytes gets the gamma in the shader.
        if let Some(mut uniform) = self.last_uniform
            && uniform.mode[3] > 0.5
        {
            let encoded = matches!(
                format,
                wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
            );
            let want = if encoded { 2.0 } else { 1.0 };
            if uniform.mode[3] != want {
                uniform.mode[3] = want;
                self.queue
                    .write_buffer(&self.frame_buffer, 0, bytemuck::bytes_of(&uniform));
                self.last_uniform = Some(uniform);
            }
        }
        let Some(group) = self.bind_group() else { return };
        let Some(pipeline) = self.pipelines.get(&self.device, format) else {
            return;
        };
        let group1 = self.caustics.draw_group();
        let mut drew = false;
        if self.stats.underwater
            && let Some(fog) = self.pipelines.get_fog(&self.device, format)
        {
            let _span = tracing::info_span!("water_underwater_fog").entered();
            pass.set_pipeline(&fog);
            pass.set_bind_group(0, &group, &[]);
            pass.set_bind_group(1, &group1, &[]);
            pass.draw(0..3, 0..1);
            drew = true;
        }
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.set_bind_group(1, &group1, &[]);
        if let (Some(vb), Some(ib)) = (&self.vertex_buffer, &self.index_buffer)
            && self.index_count > 0
            && !self.mesh_draws.is_empty()
        {
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            for range in &self.mesh_draws {
                pass.draw_indexed(range.clone(), 0, 0..1);
            }
            drew = true;
        }
        if let (Some(vb), Some(ib)) = (&self.ring_vertex_buffer, &self.ring_index_buffer)
            && self.ring_index_count > 0
        {
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            for range in &self.ring_draws {
                pass.draw_indexed(range.clone(), 0, 0..1);
                drew = true;
            }
        }
        if drew {
            self.stats.frames_drawn += 1;
        }
        self.armed = false;
    }
}

/// The water bodies' GPU renderer. Cheap to clone (a shared handle).
///
/// A host that drives `regia_render::FrameRenderer` itself installs [`Self::layer`] with the
/// frame renderer's translucent layers, then sets the bodies, the clock and the tier before each
/// frame. [`super::WaterPlugin`] does all of that for an ECS app.
#[derive(Clone)]
pub struct WaterRenderer {
    gpu: Arc<Mutex<WaterGpu>>,
    layer: Arc<WaterLayer>,
}

impl std::fmt::Debug for WaterRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaterRenderer")
            .field("stats", &self.stats())
            .finish()
    }
}

impl WaterRenderer {
    /// Creates the renderer on `device`: the ripple map is uploaded and the HDR pipeline built.
    #[must_use]
    pub fn new(device: &RenderDevice) -> Self {
        let mut gpu = WaterGpu::new(device);
        let d = gpu.device.clone();
        let _ = gpu.pipelines.get(&d, wgpu::TextureFormat::Rgba16Float);
        let gpu = Arc::new(Mutex::new(gpu));
        Self {
            layer: Arc::new(WaterLayer {
                gpu: Arc::clone(&gpu),
            }),
            gpu,
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut WaterGpu) -> R) -> Option<R> {
        self.gpu.lock().ok().map(|mut gpu| f(&mut gpu))
    }

    /// Builds the pipelines for `formats` now, so the first frame that draws water compiles none.
    ///
    /// # Errors
    ///
    /// [`RenderError::Shader`] naming the first format whose pipeline failed.
    pub fn warm(&self, formats: &[wgpu::TextureFormat]) -> Result<(), RenderError> {
        self.with(|gpu| {
            let d = gpu.device.clone();
            for format in formats {
                if gpu.pipelines.get(&d, *format).is_none() {
                    return Err(RenderError::Shader(format!("water pipeline for {format:?}")));
                }
            }
            Ok(())
        })
        .unwrap_or(Ok(()))
    }

    /// Sets the bodies drawn from the next frame on (in order; at most [`MAX_BODIES`], invalid ones
    /// skipped). The mesh is rebuilt only when the bodies changed.
    pub fn set_bodies(&self, bodies: &[&WaterBody]) {
        self.with(|gpu| gpu.set_bodies(bodies));
    }

    /// Sets each body's shore field, in the order of [`Self::set_bodies`] (spec 87; `None` for a
    /// body without one). High and Ultra bind up to [`MAX_SHORE_FIELDS`] of them.
    pub fn set_shores(&self, shores: &[Option<Arc<ShoreField>>]) {
        self.with(|gpu| {
            let same = gpu.shore.per_body.len() == shores.len()
                && gpu
                    .shore
                    .per_body
                    .iter()
                    .zip(shores)
                    .all(|(old, new)| match (old, new) {
                        (None, None) => true,
                        (Some(old), Some(new)) => Arc::ptr_eq(old, new),
                        _ => false,
                    });
            if !same {
                gpu.shore.per_body = shores.to_vec();
            }
        });
    }

    /// Sets the clock the next frame animates by.
    pub fn set_clock(&self, clock: WaterClock) {
        self.with(|gpu| gpu.clock = clock);
    }

    /// The local origin in the bodies' space (`OriginOffset`, spec 78 § 12.2).
    pub fn set_origin_offset(&self, offset: [f32; 3]) {
        self.with(|gpu| gpu.origin_offset = offset);
    }

    /// Sets the tier.
    pub fn set_tier(&self, tier: WaterTier) {
        self.with(|gpu| gpu.tier = tier);
    }

    /// Overrides the fallback sky (`None`: [`WaterSky::from_ambient_lux`] of the ambient light).
    pub fn set_sky(&self, sky: Option<WaterSky>) {
        self.with(|gpu| gpu.sky = sky);
    }

    /// Sets the frame's ambient (sky) illuminance in lux, which lights the in-scatter, foam and ice
    /// and gives the fallback sky.
    pub fn set_ambient_lux(&self, lux: f32) {
        self.with(|gpu| gpu.ambient_lux = if lux.is_finite() { lux.max(0.0) } else { 0.0 });
    }

    /// Draws the FFT ocean on High and Ultra (`true`, the default) or the swell alone. Queries
    /// are not affected ([`super::WaterSettings::ocean_draw`]).
    pub fn set_ocean_draw(&self, draw: bool) {
        self.with(|gpu| gpu.ocean_draw = draw);
    }

    /// Forgets the ocean's lingering foam (ADR-2843): the next evolve restarts the history from
    /// no foam. Without a history this does nothing.
    pub fn reset_ocean_foam(&self) {
        self.with(|gpu| gpu.ocean.reset_history());
    }

    /// Hands over the frame renderer's camera-cut count
    /// ([`regia_render::FrameRenderer::camera_cut_count`]); a count that moved since the last call
    /// is a camera cut, and the ocean's lingering foam restarts (ADR-2843). `WaterPlugin` calls
    /// this before every frame, so a cut the frame renderer detects while drawing restarts the
    /// history on the next frame.
    pub fn set_camera_cuts(&self, count: u64) {
        self.with(|gpu| {
            if gpu.camera_cuts.is_some_and(|seen| seen != count) {
                gpu.ocean.reset_history();
                // The shafts' history too (ADR-3251).
                gpu.caustics.reset_history();
            }
            gpu.camera_cuts = Some(count);
        });
    }

    /// Sets the debug view.
    pub fn set_debug(&self, debug: WaterDebug) {
        self.with(|gpu| gpu.debug = debug);
    }

    /// Times the water draw every `every` frames (0 stops; ADR-2342). Each timing draws the water
    /// once more into a private target and waits for the GPU, so leave it off in a shipped frame.
    pub fn set_gpu_timing(&self, every: u32) {
        self.with(|gpu| {
            gpu.timing_every = every;
            gpu.timing_frames = 0;
        });
    }

    /// Times one water draw for `camera` now (the same measurement as [`Self::set_gpu_timing`]):
    /// prepares the water for the view first. `Ok(None)` without `TIMESTAMP_QUERY` or without a
    /// body to draw.
    ///
    /// # Errors
    ///
    /// [`RenderError::Poll`] / [`RenderError::Readback`] when the readback fails.
    pub fn measure_gpu_ms(
        &self,
        depth: &wgpu::TextureView,
        camera: &LayerCamera,
    ) -> Result<Option<f32>, RenderError> {
        let Ok(mut gpu) = self.gpu.lock() else {
            return Ok(None);
        };
        let every = std::mem::take(&mut gpu.timing_every);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("water timing prepare"),
            });
        gpu.simulate(depth, camera, &mut encoder);
        gpu.queue.submit([encoder.finish()]);
        gpu.timing_every = every;
        gpu.time_draw(camera)
    }

    /// Turns frustum culling of the ring grid on or off (off by default; ADR-2342). The picture
    /// is the same either way; off draws every chunk in one call.
    pub fn set_ring_culling(&self, on: bool) {
        self.with(|gpu| gpu.ring_culling = on);
    }

    /// Turns the per-ring snap of the ring grid on or off (off by default; ADR-2342): on, each
    /// ring sits on its own lattice so the far rings move rarely and by whole steps
    /// ([`RingGrid::ring_shifts`]); off, every ring follows the inner grid's snap.
    pub fn set_ring_snap(&self, on: bool) {
        self.with(|gpu| gpu.ring_snap = on);
    }

    /// Turns the underwater view on or off (off by default; ADR-2342). Off draws nothing from
    /// below and does not fog the scene.
    pub fn set_underwater(&self, on: bool) {
        self.with(|gpu| gpu.underwater = on);
    }

    /// Turns the water on or off without dropping the bodies.
    pub fn set_enabled(&self, enabled: bool) {
        self.with(|gpu| gpu.enabled = enabled);
    }

    /// What the caustic passes did (ADR-2432); all zero while no body has caustics on.
    #[must_use]
    pub fn caustic_stats(&self) -> CausticStats {
        self.with(|gpu| gpu.caustics.stats).unwrap_or_default()
    }

    /// Binds the frame's sun shadow for the shaft march (ADR-3251), or unbinds it (`None`, the
    /// default: the march is unshadowed and its output the same bytes as without this call). The
    /// engine frame hands its cascade over each frame when the host wires it (the
    /// translucent-layer hookup); a host with its own shadow map may call this itself.
    pub fn set_sun_shadow(&self, shadow: Option<WaterSunShadow>) {
        self.with(|gpu| gpu.caustics.sun_shadow = shadow);
    }

    /// Per body of [`Self::set_bodies`] (in that order), whether the last frame drew its FFT ocean
    /// (ADR-3251): `false` for a body without one, and for an ocean the renderer left to its swell
    /// (Low, the `ocean_draw` switch off, a grid too large for the device, another spectrum than
    /// the frame's ocean). Written into `out` (cleared first), so a caller that keeps the vector
    /// allocates nothing per frame. Empty until a frame ran.
    pub fn oceans_drawn_into(&self, out: &mut Vec<bool>) {
        out.clear();
        self.with(|gpu| out.extend_from_slice(&gpu.oceans_drawn));
    }

    /// Checks that this renderer's device can run `ocean`'s compute pass at `grid` texels per
    /// side (ADR-3251), with its lingering-foam history if it has one.
    ///
    /// # Errors
    ///
    /// The [`OceanGridError`] the renderer's size check refuses the grid with (it then draws that
    /// ocean as swell); a poisoned lock reports nothing.
    pub fn check_ocean_grid(&self, ocean: &FftOcean, grid: u32) -> Result<(), OceanGridError> {
        self.with(|gpu| {
            super::ocean_gpu::check_ocean_grid(
                &gpu.device.limits(),
                grid,
                super::ocean_gpu::has_history(ocean),
            )
        })
        .unwrap_or(Ok(()))
    }

    /// Restarts the shafts' temporal history (ADR-3251), as a camera cut does.
    pub fn reset_shaft_history(&self) {
        self.with(|gpu| gpu.caustics.reset_history());
    }

    /// The shafts the last frame's draw sampled, read back from the GPU (ADR-3251): the
    /// accumulation when a resolve ran, else the raw half-resolution march; `[r, g, b, a]` per
    /// texel, row by row, and the size. `None` before the first shaft pass. Blocks until the GPU
    /// is done: for checks, not per frame.
    pub fn read_shafts(&self) -> Option<(Vec<[f32; 4]>, [u32; 2])> {
        self.with(|gpu| gpu.caustics.read_shafts()).flatten()
    }

    /// The caustic tile as the last tile pass wrote it: `CAUSTIC_TILE_SIZE²` texels of
    /// `[tr J, det J, dr_x, dr_z]` (see [`super::caustics`]), read back from the GPU. `None`
    /// before the first tile pass. Blocks until the GPU is done.
    #[must_use]
    pub fn read_caustic_tile(&self) -> Option<Vec<[f32; 4]>> {
        self.with(|gpu| gpu.caustics.read_tile()).flatten()
    }

    /// What the last frame drew.
    #[must_use]
    pub fn stats(&self) -> WaterFrameStats {
        self.with(|gpu| gpu.stats).unwrap_or_default()
    }

    /// Places ring-grid vertices on the GPU with the vertex shader's own math (`ring_world`, run
    /// by the compute entry [`PROBE_ENTRY`]), for the swell bodies set last and a view from
    /// `camera`: each `(body slot, offset from the grid's center)` gives the world position the
    /// vertex is drawn at. Also returns the grid and its center, so a test can compare with
    /// [`ring_vertex`] and [`WaterBody::surface`]. Blocks until the GPU is done.
    ///
    /// # Errors
    ///
    /// [`RenderError::Shader`] when the probe pipeline does not build (or no swell body is set),
    /// [`RenderError::Poll`] / [`RenderError::Readback`] when the readback fails.
    pub fn probe_ring_vertices(
        &self,
        camera: &LayerCamera,
        points: &[(u32, [f32; 2])],
    ) -> Result<RingProbe, RenderError> {
        let Ok(mut gpu) = self.gpu.lock() else {
            return Err(RenderError::Shader("water renderer lock poisoned".into()));
        };
        gpu.prepare_shore();
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("water ocean probe"),
            });
        gpu.evolve_ocean(&mut encoder);
        gpu.queue.submit([encoder.finish()]);
        let frame = gpu
            .prepare_swell(camera)
            .ok_or_else(|| RenderError::Shader("no swell body to probe".into()))?;
        let positions = gpu.run_probe(points)?;
        Ok(RingProbe {
            grid: frame.grid,
            origin: frame.origin,
            shifts: frame.shifts,
            positions,
        })
    }

    /// Evolves the frame's ocean to the clock if needed and reads its maps back (`None` with no
    /// ocean drawn: Low, the switch off, no ocean body). For the CPU mirror's checks. Blocks until
    /// the GPU is done.
    ///
    /// # Errors
    ///
    /// [`RenderError::Shader`] on a poisoned lock, [`RenderError::Poll`] and
    /// [`RenderError::Readback`] when the readback fails.
    pub fn read_ocean_maps(&self) -> Result<Option<OceanMaps>, RenderError> {
        let Ok(mut gpu) = self.gpu.lock() else {
            return Err(RenderError::Shader("water renderer lock poisoned".into()));
        };
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("water ocean read"),
            });
        gpu.evolve_ocean(&mut encoder);
        gpu.queue.submit([encoder.finish()]);
        gpu.ocean.read_maps(&gpu.device, &gpu.queue)
    }

    /// Times the FFT ocean's compute pass at `size` texels on this renderer's device (ADR-2126),
    /// on a simulation of its own: the frame's maps are not disturbed. `None` when `size` is not
    /// one of [`super::ocean::OCEAN_GRIDS`] or the device cannot run it. Blocks until the GPU is
    /// done, for each of the `frames`, so use it for checks, not per frame.
    ///
    /// # Errors
    ///
    /// [`RenderError::Shader`] on a poisoned lock, [`RenderError::Poll`] and
    /// [`RenderError::Readback`] when reading the timestamps fails.
    pub fn measure_ocean_cost(
        &self,
        ocean: &FftOcean,
        size: u32,
        frames: u32,
    ) -> Result<Option<OceanCost>, RenderError> {
        let Ok(mut guard) = self.gpu.lock() else {
            return Err(RenderError::Shader("water renderer lock poisoned".into()));
        };
        let gpu = &mut *guard;
        gpu.ocean.measure(&gpu.device, &gpu.queue, ocean, size, frames)
    }

    /// The compute pass of the FFT ocean, for [`regia_render::FrameRenderer::register_pass`]: it
    /// evolves the maps at the `after_opaque` slot and shows in `render.graph`. `WaterPlugin`
    /// registers it when a body has an ocean. A host that does not gets the same evolve from
    /// the layer's `simulate`, unlisted.
    #[must_use]
    pub fn ocean_pass(&self) -> Box<dyn FramePass> {
        Box::new(OceanPass {
            gpu: Arc::clone(&self.gpu),
        })
    }

    /// The translucent layer to install on the frame renderer.
    #[must_use]
    pub fn layer(&self) -> Arc<dyn TranslucentLayer> {
        self.layer.clone()
    }

    /// `true` when `layer` is this renderer's layer ([`Self::layer`]); compares the data
    /// pointers only.
    #[must_use]
    pub fn owns_layer(&self, layer: &Arc<dyn TranslucentLayer>) -> bool {
        std::ptr::eq(
            Arc::as_ptr(layer).cast::<()>(),
            Arc::as_ptr(&self.layer).cast::<()>(),
        )
    }
}

/// The ocean's compute pass at the `after_opaque` slot ([`WaterRenderer::ocean_pass`]).
struct OceanPass {
    gpu: Arc<Mutex<WaterGpu>>,
}

impl FramePass for OceanPass {
    fn name(&self) -> &str {
        OCEAN_PASS_NAME
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::AfterOpaque
    }

    fn writes(&self) -> &[&'static str] {
        &["water_ocean_maps"]
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let mut gpu = self
            .gpu
            .lock()
            .map_err(|_| "water renderer lock poisoned".to_string())?;
        gpu.evolve_ocean(ctx.encoder);
        Ok(())
    }
}

/// The [`TranslucentLayer`] of a [`WaterRenderer`].
struct WaterLayer {
    gpu: Arc<Mutex<WaterGpu>>,
}

impl TranslucentLayer for WaterLayer {
    fn simulate(&self, depth: &wgpu::TextureView, camera: &LayerCamera, encoder: &mut wgpu::CommandEncoder) {
        if let Ok(mut gpu) = self.gpu.lock() {
            gpu.simulate(depth, camera, encoder);
        }
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        if let Ok(mut gpu) = self.gpu.lock() {
            gpu.record(pass, format);
        }
    }

    fn participates_in_hdr(&self) -> bool {
        true
    }

    fn draw_hdr(&self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        self.draw(pass, format);
    }

    fn bind_decal_targets(&self, _receiver: Option<&wgpu::TextureView>, scene: Option<&wgpu::TextureView>) {
        if let Ok(mut gpu) = self.gpu.lock() {
            gpu.scene = scene.cloned();
        }
    }

    fn wants_scene_after_decals(&self) -> bool {
        self.gpu.lock().is_ok_and(|gpu| gpu.armed && gpu.composite)
    }

    fn has_decals(&self) -> bool {
        false
    }

    fn set_outdoor_portals(&self, portals: Option<&regia_render::viscells::OutdoorPortals>) {
        if let Ok(mut gpu) = self.gpu.lock() {
            match portals {
                Some(portals) => gpu.portals.clone_from(portals),
                None => gpu.portals.clear(),
            }
        }
    }
}

/// Each body's index range in `mesh` (bodies are appended whole, in order) and the box of the
/// vertices it draws: the mesh vertices are drawn where they lie (only the ring grid is displaced).
fn mesh_body_ranges(mesh: &WaterMesh) -> Vec<(std::ops::Range<u32>, [f32; 3], [f32; 3])> {
    let mut out: Vec<(std::ops::Range<u32>, [f32; 3], [f32; 3])> = Vec::new();
    let mut current: Option<u32> = None;
    for (k, index) in mesh.indices.iter().enumerate() {
        let Some(vertex) = mesh.vertices.get(*index as usize) else {
            continue;
        };
        let p = vertex.position;
        let k = k as u32;
        match out.last_mut() {
            Some((range, lo, hi)) if current == Some(vertex.body) && range.end == k => {
                range.end = k + 1;
                for axis in 0..3 {
                    lo[axis] = lo[axis].min(p[axis]);
                    hi[axis] = hi[axis].max(p[axis]);
                }
            }
            _ => {
                current = Some(vertex.body);
                out.push((k..k + 1, p, p));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(hdr: bool) -> LayerCamera {
        LayerCamera {
            eye: [0.0, 2.0, 0.0],
            target: [10.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 2000.0,
            width: 2560,
            height: 1440,
            exposure: 2.5e-5 / 4.0e-5,
            display_exposure: 2.5e-5,
            hdr,
            sun_direction: [0.3, 0.2, 0.1],
            sun_color: [1.0, 0.9, 0.8],
            sun_illuminance: 80_000.0,
            sky_ambient: None,
        }
    }

    fn inputs(tier: WaterTier, composite: bool, obstacles: u32) -> FrameInputs {
        FrameInputs {
            tier,
            composite,
            obstacles,
            sky: WaterSky::from_ambient_lux(20_000.0),
            ambient_lux: 20_000.0,
            debug: WaterDebug::Off,
            underwater_view: false,
            underwater: None,
        }
    }

    #[test]
    fn uniform_is_a_pure_function_of_camera_clock_and_tier() {
        let clock = WaterClock {
            tick: 600,
            ..WaterClock::default()
        };
        let a = frame_uniform(&camera(true), &clock, &inputs(WaterTier::High, true, 3));
        let b = frame_uniform(&camera(true), &clock, &inputs(WaterTier::High, true, 3));
        assert_eq!(bytemuck::bytes_of(&a), bytemuck::bytes_of(&b));
        let later = WaterClock { tick: 601, ..clock };
        let c = frame_uniform(&camera(true), &later, &inputs(WaterTier::High, true, 3));
        assert_ne!(a.flow, c.flow);
        assert_ne!(a.scroll, c.scroll);
        // HDR: radiance × pre-exposure (4e-5 here).
        assert!(
            (a.sun_dir[3] - 80_000.0 * 4.0e-5).abs() < 1.0e-3,
            "{:?}",
            a.sun_dir
        );
        // The sun direction is normalized.
        let l = (a.sun_dir[0].powi(2) + a.sun_dir[1].powi(2) + a.sun_dir[2].powi(2)).sqrt();
        assert!((l - 1.0).abs() < 1.0e-5);
        assert_eq!(a.mode, [1.0, 24.0, 3.0, 0.0]);
        assert_eq!(a.coarse[3], 1.0);
        // The inverse really inverts.
        let vp = regia_core::glam::Mat4::from_cols_array_2d(&a.view_proj);
        let inv = regia_core::glam::Mat4::from_cols_array_2d(&a.inv_view_proj);
        assert!((vp * inv).abs_diff_eq(regia_core::glam::Mat4::IDENTITY, 1.0e-3));
    }

    #[test]
    fn low_tier_and_display_pass_turn_the_rays_off() {
        let clock = WaterClock::default();
        let low = frame_uniform(&camera(true), &clock, &inputs(WaterTier::Low, false, 0));
        assert_eq!(low.mode[0], 0.0);
        assert_eq!(low.mode[1], 0.0);
        assert_eq!(low.misc[1], 0.0);
        assert_eq!(low.coarse[3], 0.0);
        let display = frame_uniform(&camera(false), &clock, &inputs(WaterTier::Ultra, false, 0));
        assert_eq!(display.mode[3], 1.0);
        assert!((display.sun_dir[3] - 80_000.0 * 2.5e-5).abs() < 1.0e-4);
        // A broken exposure falls back to 1 instead of NaN.
        let mut broken = camera(true);
        broken.exposure = 0.0;
        let u = frame_uniform(&broken, &clock, &inputs(WaterTier::High, true, 0));
        assert!(u.sun_dir[3].is_finite());
    }

    #[test]
    fn nearest_obstacles_are_sorted_and_capped() {
        let obstacles: Vec<WaterObstacle> = (0..100)
            .map(|i| WaterObstacle {
                position: [100.0 - i as f32, 0.0, 0.0],
                radius: 0.3,
                strength: 1.0,
            })
            .collect();
        let near = nearest_obstacles(&obstacles, [0.0, 5.0, 0.0], 32);
        assert_eq!(near.len(), 32);
        assert_eq!(near[0][0], 1.0);
        assert!(near.windows(2).all(|w| w[0][0] <= w[1][0]));
        assert!(nearest_obstacles(&obstacles, [0.0; 3], 500).len() == MAX_OBSTACLES);
        assert!(nearest_obstacles(&obstacles, [0.0; 3], 0).is_empty());
    }

    #[test]
    fn sky_from_ambient_is_bluer_overhead() {
        let sky = WaterSky::from_ambient_lux(std::f32::consts::PI * 1000.0);
        assert!(sky.zenith[2] > sky.zenith[0]);
        assert!((sky.horizon[1] - 1100.0).abs() < 1.0);
        assert_eq!(WaterSky::from_ambient_lux(f32::NAN).zenith, [0.0; 3]);
    }

    /// Every `@group(0) @binding(N)` declaration of `src`, as `(N, "var<...> name: type")`.
    fn declared_bindings(src: &str) -> Vec<(u32, String)> {
        src.lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("@group(0) @binding(")?;
                let (n, decl) = rest.split_once(')')?;
                Some((n.parse().ok()?, decl.trim().trim_end_matches(';').to_string()))
            })
            .collect()
    }

    /// The water's bindings are pinned (ADR-2342): the shader declares exactly bindings 0..=12 in
    /// this order and form, the FFT ocean's maps and uniform at 11 and 12 next to the swell (8) and
    /// the shore (9, 10); the layout gives each the kind and stages the shader needs (the ring
    /// grid's vertex stage reads the swell, the shore and the ocean); and the ocean's compute
    /// shader keeps its own five. A merge that moves, renames or drops one fails here.
    #[test]
    fn bind_layout_is_pinned() {
        let pinned: [(u32, &str); 13] = [
            (0, "var<uniform> frame: Frame"),
            (1, "var<uniform> bodies: Bodies"),
            (2, "var<uniform> obstacles: Obstacles"),
            (3, "var depth_tex: texture_depth_2d"),
            (4, "var scene_tex: texture_2d<f32>"),
            (5, "var ripple_tex: texture_2d<f32>"),
            (6, "var ripple_samp: sampler"),
            (7, "var clamp_samp: sampler"),
            (8, "var<uniform> swell: Swell"),
            (9, "var shore_tex: texture_2d_array<f32>"),
            (10, "var<uniform> shore: Shore"),
            (11, "var ocean_tex: texture_2d_array<f32>"),
            (12, "var<uniform> ocean: Ocean"),
        ];
        let declared = declared_bindings(SHADER);
        let expected: Vec<(u32, String)> = pinned.iter().map(|(n, d)| (*n, (*d).to_string())).collect();
        assert_eq!(declared, expected, "water_body.wgsl bindings moved");
        let vf = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let f = wgpu::ShaderStages::FRAGMENT;
        let entries = bind_layout_entries();
        let stages: Vec<(u32, wgpu::ShaderStages)> =
            entries.iter().map(|e| (e.binding, e.visibility)).collect();
        assert_eq!(
            stages,
            vec![
                (0, vf),
                (1, f),
                (2, f),
                (3, f),
                (4, f),
                (5, f),
                (6, f),
                (7, f),
                (8, vf),
                (9, vf),
                (10, vf),
                (11, vf),
                (12, vf),
            ]
        );
        for (entry, (n, decl)) in entries.iter().zip(&pinned) {
            let kind_ok = match entry.ty {
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    ..
                } => decl.starts_with("var<uniform>"),
                wgpu::BindingType::Sampler(_) => decl.ends_with(": sampler"),
                wgpu::BindingType::Texture {
                    sample_type,
                    view_dimension,
                    ..
                } => match (sample_type, view_dimension) {
                    (wgpu::TextureSampleType::Depth, wgpu::TextureViewDimension::D2) => {
                        decl.ends_with("texture_depth_2d")
                    }
                    (wgpu::TextureSampleType::Float { .. }, wgpu::TextureViewDimension::D2) => {
                        decl.ends_with("texture_2d<f32>")
                    }
                    (
                        wgpu::TextureSampleType::Float { filterable: false },
                        wgpu::TextureViewDimension::D2Array,
                    ) => decl.ends_with("texture_2d_array<f32>"),
                    _ => false,
                },
                _ => false,
            };
            assert!(kind_ok, "binding {n}: layout {:?} vs shader `{decl}`", entry.ty);
        }
        // The underwater view adds entry points, not bindings.
        for entry in ["fn vs_fog(", "fn fs_fog(", "fn underside(", "fn ring_shift("] {
            assert!(SHADER.contains(entry), "{entry}");
        }
        assert_eq!(
            declared_bindings(super::super::ocean_gpu::OCEAN_SHADER),
            vec![
                (0, "var<uniform> params: Params".to_string()),
                (1, "var<storage, read> modes: array<vec4<f32>>".to_string()),
                (2, "var<storage, read> twiddles: array<vec2<f32>>".to_string()),
                (3, "var<storage, read> src: array<vec4<f32>>".to_string()),
                (4, "var<storage, read_write> dst: array<vec4<f32>>".to_string()),
            ],
            "water_ocean.wgsl bindings moved"
        );
    }

    /// The ocean's reach bounds the drawn maps at the instant, on the query grid and on a finer
    /// one (its extra modes added by their strict bound), and is far tighter than the strict bound
    /// of every mode, so culling over an ocean can drop chunks.
    #[test]
    fn ocean_reach_bounds_the_drawn_maps() {
        let ocean = FftOcean {
            wind_speed: 14.0,
            ..FftOcean::default()
        };
        for seconds in [0.0, 10.0, 1234.5] {
            for size in [super::super::ocean::OCEAN_QUERY_SIZE, 256] {
                let (rise, shift) = ocean_reach(&ocean, size, seconds);
                let drawn = super::super::ocean::frame_of(&ocean, seconds, size);
                assert!(
                    drawn.max_rise() <= rise,
                    "{size} {seconds}: {} > {rise}",
                    drawn.max_rise()
                );
                assert!(
                    drawn.max_shift() <= shift,
                    "{size} {seconds}: {} > {shift}",
                    drawn.max_shift()
                );
                let strict = super::super::ocean::tables_for(&ocean, size);
                assert!(rise < 0.5 * strict.rise_bound, "{rise} vs {}", strict.rise_bound);
            }
        }
    }

    /// Ring culling reads the tick's query frame: a cold tick transforms once, and the cull, a
    /// second cull and a surface query of that tick share the frame (no second transform). The
    /// reach is the frame's tight peak, well under the strict spectral bound, which culled nothing
    /// over an ocean (ADR-5555).
    #[test]
    fn ring_cull_reach_is_tight_and_transforms_once_a_tick() {
        use super::super::ocean::OCEAN_QUERY_SIZE;
        let ocean = FftOcean {
            seed: 0xC011_u32,
            ..FftOcean::default()
        };
        let seconds = 4242.5;
        super::super::ocean::test_reset_ocean_counts();
        let before = super::super::ocean::test_ocean_evolves();
        let (rise, shift) = ring_cull_reach(&ocean, OCEAN_QUERY_SIZE, seconds);
        assert_eq!(
            super::super::ocean::test_ocean_evolves(),
            before + 1,
            "a cold ring reach transforms once"
        );
        let strict = super::super::ocean::tables_for(&ocean, OCEAN_QUERY_SIZE);
        assert!(rise < 0.5 * strict.rise_bound, "{rise} vs {}", strict.rise_bound);
        assert!(
            shift < 0.5 * strict.shift_bound,
            "{shift} vs {}",
            strict.shift_bound
        );
        let mid = super::super::ocean::test_ocean_evolves();
        let frame = super::super::ocean::frame_at(&ocean, seconds);
        let (rise2, shift2) = ring_cull_reach(&ocean, OCEAN_QUERY_SIZE, seconds);
        assert_eq!(
            super::super::ocean::test_ocean_evolves(),
            mid,
            "the tick's frame is reused by the query and the second cull"
        );
        let want = reach_from_frame(&frame, &ocean, OCEAN_QUERY_SIZE);
        assert!((rise - want.0).abs() < 1.0e-4 && (shift - want.1).abs() < 1.0e-4);
        assert!((rise2 - want.0).abs() < 1.0e-4 && (shift2 - want.1).abs() < 1.0e-4);
    }

    /// An eye above the ocean's height bound is not underwater, and does not build a frame.
    #[test]
    fn underwater_above_the_bound_does_not_build_a_frame() {
        use super::super::ocean::OCEAN_QUERY_SIZE;
        let ocean = FftOcean {
            seed: 0x0E1E_u32,
            ..FftOcean::default()
        };
        let strict = super::super::ocean::tables_for(&ocean, OCEAN_QUERY_SIZE);
        let mut body = WaterBody::plane([0.0, 0.0, 0.0], [100.0, 100.0], super::super::WaterLook::ocean());
        body.ocean = Some(ocean);
        let clock = WaterClock {
            tick: 1,
            step_seconds: 5151.25,
            running: true,
        };
        super::super::ocean::test_reset_ocean_counts();
        let eye_y = strict.rise_bound + 2.0;
        assert!(plane_eye_under(&body, [0.0, eye_y, 0.0], &clock).is_none());
        assert_eq!(
            super::super::ocean::test_ocean_evolves(),
            0,
            "an eye above the bound must not transform"
        );
    }

    /// With every extra off (the default) the frame uniform is the one before them: `misc.w` and
    /// `depth.w` are 0 whatever the eye; on, `misc.w` says above (1) or under body slot + 2.
    #[test]
    fn underwater_code_is_zero_when_off() {
        assert_eq!(underwater_code(false, None), 0.0);
        assert_eq!(underwater_code(false, Some((3, 2.0))), 0.0);
        assert_eq!(underwater_code(true, None), 1.0);
        assert_eq!(underwater_code(true, Some((0, 2.0))), 2.0);
        assert_eq!(underwater_code(true, Some((15, 2.0))), 17.0);
        let camera = camera(true);
        let clock = WaterClock::default();
        let off = frame_uniform(&camera, &clock, &inputs(WaterTier::High, true, 0));
        assert_eq!((off.misc[3], off.depth[3]), (0.0, 0.0));
        let on = frame_uniform(
            &camera,
            &clock,
            &FrameInputs {
                underwater_view: true,
                underwater: Some((4, 7.5)),
                ..inputs(WaterTier::High, true, 0)
            },
        );
        assert_eq!((on.misc[3], on.depth[3]), (6.0, 7.5));
    }

    #[test]
    fn uniform_sizes_match_the_shader() {
        assert_eq!(size_of::<FrameUniform>(), 128 + 12 * 16);
        assert_eq!(size_of::<BodyUniform>(), 7 * 16);
    }

    /// ADR-026 (no struct/array function locals in SPIR-V 1.6) and the DX12 checks (ADR-367).
    #[test]
    fn shader_passes_the_engine_shader_rules() {
        let found = regia_render::wgsl_function_layout_errors("water_body.wgsl", SHADER)
            .expect("water_body.wgsl parses");
        assert!(found.is_empty(), "{found:?}");
        let dx12 = regia_render::shader_hlsl::wgsl_dx12_problems(&[("water_body.wgsl", SHADER)]);
        assert!(dx12.is_empty(), "{}", dx12.join("\n"));
    }

    fn sea() -> WaterBody {
        let mut sea = WaterBody::plane([5.0, 1.5, -3.0], [400.0, 300.0], super::super::WaterLook::ocean());
        sea.swell = Some(Swell::open_sea([1.0, 0.3], 1.2, 24.0, 12));
        sea
    }

    /// The swell constants and layout the shader repeats agree with the Rust side.
    #[test]
    fn swell_constants_mirror_the_rust_side() {
        use super::super::swell::{FOAM_RAMP, MAX_SWELL_WAVES};
        assert!(SHADER.contains(&format!("const MAX_SWELL_WAVES: u32 = {MAX_SWELL_WAVES}u;")));
        assert!(SHADER.contains(&format!("const FOAM_RAMP: f32 = {FOAM_RAMP};")));
        assert!(SHADER.contains(&format!("b: array<SwellBody, {MAX_BODIES}>")));
        assert!(SHADER.contains(&format!("w: array<vec4<f32>, {}>", 2 * MAX_SWELL_TABLE)));
        assert!(SHADER.contains("@group(0) @binding(8) var<uniform> swell: Swell;"));
        assert_eq!(
            SWELL_VEC4S * 16,
            16 + 48 * MAX_BODIES + 32 * MAX_SWELL_TABLE + 16 * RING_SHIFT_VEC4S
        );
        // The probe module (shader + compute entry) parses too.
        let probe = format!("{SHADER}\n{PROBE_ENTRY}");
        let found = regia_render::wgsl_function_layout_errors("water_body.wgsl + probe", &probe)
            .expect("probe parses");
        assert!(found.is_empty(), "{found:?}");
    }

    /// The packed table: the body's header, rectangle and level; each wave's vector, amplitudes and
    /// phase; the tier cap and the shared table limit.
    #[test]
    fn swell_uniform_packs_bodies_and_waves() {
        let sea = sea();
        let lake = WaterBody::plane([0.0; 3], [5.0, 5.0], super::super::WaterLook::lake());
        let grid = RingGrid::for_view(1.0, 720, 10.0, 32, 500.0);
        let origin = [4.0, -2.0];
        let (packed, waves, dropped) = swell_uniform(&[(1, &lake), (3, &sea)], &grid, origin, &[], 12.5, 8);
        assert_eq!((waves, dropped), (8, 0));
        assert_eq!(packed.len(), SWELL_VEC4S);
        assert_eq!(packed[0], [4.0, -2.0, grid.cell(), grid.inner_half_size()]);
        assert_eq!(packed[1 + 3], [0.0; 4], "the lake has no swell");
        let b = 1 + 3 * 3;
        let swell = sea.swell.as_ref().expect("swell");
        assert_eq!(packed[b], [0.0, 8.0, swell.crest_foam, swell.foam_onset]);
        assert_eq!(packed[b + 1], [5.0, -3.0, 400.0, 300.0]);
        assert_eq!(packed[b + 2], [1.5, 1.0, 0.0, 0.0]);
        let instant = swell.instant(12.5, origin, 8);
        let table = 1 + 3 * MAX_BODIES;
        for (i, w) in instant.terms().iter().enumerate() {
            assert_eq!(packed[table + 2 * i], [w.k[0], w.k[1], w.amplitude, w.horizontal]);
            assert_eq!(packed[table + 2 * i + 1], [w.phase, w.omega, 0.0, 0.0]);
        }
        // Twelve 12-wave seas overflow the 128-wave table.
        let many: Vec<(u32, &WaterBody)> = (0..12).map(|i| (i, &sea)).collect();
        let (_, waves, dropped) = swell_uniform(&many, &grid, origin, &[], 0.0, 32);
        assert_eq!(waves as usize, MAX_SWELL_TABLE);
        assert_eq!(dropped as usize, 12 * 12 - MAX_SWELL_TABLE);
        // Two packs into one buffer keep that buffer and match a fresh pack.
        let mut reused = Vec::new();
        let (waves_a, dropped_a) = swell_uniform_into(&mut reused, [(3, &sea)], &grid, origin, &[], 12.5, 8);
        let ptr = reused.as_ptr();
        let cap = reused.capacity();
        let first = reused.clone();
        let (waves_b, dropped_b) = swell_uniform_into(&mut reused, [(3, &sea)], &grid, origin, &[], 12.5, 8);
        assert_eq!((waves_a, dropped_a), (waves_b, dropped_b));
        assert_eq!(reused, first);
        assert_eq!(reused.as_ptr(), ptr, "the second pack must keep the buffer");
        assert_eq!(reused.capacity(), cap);
        let (fresh, _, _) = swell_uniform(&[(3, &sea)], &grid, origin, &[], 12.5, 8);
        assert_eq!(reused, fresh);
    }

    /// Near the grid's center every wave is resolved, so the vertex the grid draws lies on the
    /// surface the CPU query answers: the same height within 1 mm at the vertex's x and z.
    #[test]
    fn ring_vertices_lie_on_the_queried_surface() {
        let sea = sea();
        let camera = camera(true);
        let frame = swell_frame_for([&sea], &camera, WaterTier::Ultra).expect("frame");
        let clock = WaterClock {
            tick: 7_777,
            ..WaterClock::default()
        };
        let cell = frame.grid.cell();
        // Waves this grid resolves fully near its center.
        let shortest = sea
            .swell
            .as_ref()
            .map(|s| s.waves.iter().map(|w| w.wavelength).fold(f32::MAX, f32::min))
            .unwrap_or(0.0);
        assert!(shortest >= 4.0 * cell, "{shortest} vs cell {cell}");
        for local in [
            [0.0f32, 0.0],
            [cell * 3.0, -cell * 5.0],
            [-cell * 7.0, cell * 2.0],
        ] {
            let v = ring_vertex(&sea, &frame.grid, frame.origin, clock.seconds(), 32, local).expect("vertex");
            let q = sea.surface(v[0], v[2], &clock).expect("inside");
            assert!(
                (q.height - v[1]).abs() < 1.0e-3,
                "{local:?}: query {} vs vertex {}",
                q.height,
                v[1]
            );
        }
        // Outside the rectangle the vertex is clamped to its edge.
        let far = ring_vertex(&sea, &frame.grid, frame.origin, 0.0, 32, [1.0e5, 0.0]).expect("vertex");
        assert!(far[0] <= 405.0 + sea.swell.as_ref().map_or(0.0, Swell::max_shift) + 1.0e-3);
        assert!(
            ring_vertex(
                &WaterBody::plane([0.0; 3], [1.0, 1.0], super::super::WaterLook::lake()),
                &frame.grid,
                frame.origin,
                0.0,
                32,
                [0.0; 2]
            )
            .is_none()
        );
    }

    /// A plane whose center is not finite yields no ring vertex. `f32::clamp` aborts when a
    /// bound is NaN, and a non-finite `y` would be written into the vertex.
    #[test]
    fn ring_vertex_rejects_a_non_finite_center() {
        let good = sea();
        let camera = camera(true);
        let frame = swell_frame_for([&good], &camera, WaterTier::Ultra).expect("frame");
        for center in [
            [f32::NAN, 1.5, -3.0],
            [5.0, f32::NAN, -3.0],
            [5.0, 1.5, f32::INFINITY],
        ] {
            let mut bad = sea();
            if let WaterShape::Plane { center: slot, .. } = &mut bad.shape {
                *slot = center;
            }
            assert!(
                ring_vertex(&bad, &frame.grid, frame.origin, 0.0, 32, [0.0, 0.0]).is_none(),
                "{center:?}"
            );
        }
    }

    /// A consistent field whose arrays are longer than `size[0] * size[1]` still fills only
    /// that rectangle. The extra samples are not texels ([`ShoreField::sample`] never reads
    /// them) and must not index past the layer.
    #[test]
    fn a_long_consistent_shore_field_fills_its_rectangle() {
        use super::super::shore::SHORE_FORMAT_VERSION;
        let field = ShoreField {
            format_version: SHORE_FORMAT_VERSION,
            origin: [0.0, 0.0],
            cell: 1.0,
            size: [2, 2],
            level: 0.0,
            distance: vec![2.0, 3.0, 4.0, 5.0, 9.0],
            depth: vec![1.0, 1.5, 2.0, 2.5, 8.0],
        };
        assert!(field.is_consistent());
        let mut texels = vec![[0.0f32; 2]; 4];
        fill_shore_layer(&mut texels, 2, 2, &field);
        assert_eq!(texels, [[1.0, 2.0], [1.5, 3.0], [2.0, 4.0], [2.5, 5.0]]);
    }

    /// The shore texture is sized by consistent fields only and stays within the device's 2D
    /// limit: an inconsistent hand-built field with a huge declared size, or a consistent one
    /// wider than the limit, must not ask for a texture that fails validation.
    #[test]
    fn shore_texture_dims_skip_inconsistent_fields_and_respect_the_limit() {
        use super::super::shore::SHORE_FORMAT_VERSION;
        let field = |size: [u32; 2], n: usize| {
            Arc::new(ShoreField {
                format_version: SHORE_FORMAT_VERSION,
                origin: [0.0, 0.0],
                cell: 1.0,
                size,
                level: 0.0,
                distance: vec![1.0; n],
                depth: vec![1.0; n],
            })
        };
        let good = field([16, 8], 128);
        let broken = field([1_000_000, 2], 0);
        assert!(!broken.is_consistent());
        assert_eq!(shore_texture_dims(&[good.clone(), broken], 8192), (16, 8));
        assert_eq!(shore_texture_dims(&[], 8192), (1, 1));
        let wide = field([9000, 2], 18_000);
        assert!(wide.is_consistent());
        assert_eq!(shore_texture_dims(&[wide.clone(), good], 8192), (8192, 8));
        // The layer fill already clips a field to the texture.
        let mut texels = vec![[0.0f32; 2]; 8192 * 2];
        fill_shore_layer(&mut texels, 8192, 2, &wide);
        assert_eq!(texels[8191], [1.0, 1.0]);
    }

    /// A plane with a non-finite or non-positive half extent yields no ring vertex (`f32::clamp`
    /// would abort on the NaN).
    #[test]
    fn ring_vertex_rejects_bad_half_extents() {
        let camera = camera(true);
        for half in [[-1.0f32, 10.0], [f32::NAN, 10.0], [10.0, 0.0]] {
            let mut bad = sea();
            if let WaterShape::Plane { half_extent, .. } = &mut bad.shape {
                *half_extent = half;
            }
            let frame = swell_frame_for([&bad], &camera, WaterTier::Ultra).expect("frame");
            assert!(
                ring_vertex(&bad, &frame.grid, frame.origin, 0.0, 32, [0.0, 0.0]).is_none(),
                "{half:?}"
            );
        }
    }

    /// The grid centers on the eye, snapped to two cells, and reaches the far side of the body.
    #[test]
    fn swell_frame_snaps_to_the_eye() {
        let sea = sea();
        let camera = camera(true);
        let f = swell_frame_for([&sea], &camera, WaterTier::High).expect("frame");
        let snap = 2.0 * f64::from(f.grid.cell());
        for (o, e) in f.origin.iter().zip([camera.eye[0], camera.eye[2]]) {
            assert!((o / snap).fract() == 0.0);
            assert!(f64::from(e) - o >= 0.0 && f64::from(e) - o < snap);
        }
        assert!(f.grid.half_cells <= WaterTier::High.ring_cells());
        let lake = WaterBody::plane([0.0; 3], [5.0, 5.0], super::super::WaterLook::lake());
        assert!(swell_frame_for([&lake], &camera, WaterTier::High).is_none());
    }

    /// The constants the shader repeats agree with the Rust side.
    #[test]
    fn shader_constants_mirror_the_rust_side() {
        for (name, value) in [
            ("SLOPE_RANGE", super::super::normal_map::SLOPE_RANGE),
            ("SLOPE_SQ_RANGE", super::super::normal_map::SLOPE_SQ_RANGE),
            ("GRAVITY", super::super::GRAVITY),
            ("WATER_IOR", crate::water::WATER_IOR),
        ] {
            let line = SHADER
                .lines()
                .find(|l| l.starts_with(&format!("const {name}: f32 = ")))
                .unwrap_or_else(|| panic!("no const {name}"));
            let text = line.trim_end_matches(';').rsplit(' ').next().unwrap_or("");
            assert_eq!(text.parse::<f32>().ok(), Some(value), "{name}");
        }
        assert!(SHADER.contains(&format!("array<Body, {MAX_BODIES}>")));
        assert!(SHADER.contains(&format!("array<vec4<f32>, {MAX_OBSTACLES}>")));
        assert!(SHADER.contains("const PLANE_HALF_WIDTH: f32 = 1.0e8;"));
    }

    /// The shore constants and layout the shader repeats agree with the Rust side (spec 87).
    #[test]
    fn shore_constants_mirror_the_rust_side() {
        use super::super::shore;
        for (name, value) in [
            ("SHORE_DEEP", shore::SHORE_DEEP),
            ("BREAKING_INDEX", shore::BREAKING_INDEX),
            ("SHOAL_MAX", shore::SHOAL_MAX),
            ("DEEP_KH", shore::DEEP_KH),
            ("MIN_KH", shore::MIN_KH),
            ("BREAKER_ONSET", shore::BREAKER_ONSET),
        ] {
            let line = SHADER
                .lines()
                .find(|l| l.starts_with(&format!("const {name}: f32 = ")))
                .unwrap_or_else(|| panic!("no const {name}"));
            let text = line.trim_end_matches(';').rsplit(' ').next().unwrap_or("");
            let parsed = text.parse::<f32>().unwrap_or(f32::NAN);
            assert!(
                (parsed - value).abs() <= value.abs() * 1.0e-6,
                "{name}: {text} vs {value}"
            );
        }
        assert!(SHADER.contains(&format!("f: array<vec4<f32>, {}>", 2 * MAX_SHORE_FIELDS)));
        assert!(SHADER.contains(&format!("map: array<vec4<f32>, {}>", MAX_BODIES / 4)));
        assert_eq!(SHORE_VEC4S, 2 * MAX_SHORE_FIELDS + MAX_BODIES / 4);
        assert!(SHADER.contains("@group(0) @binding(9) var shore_tex: texture_2d_array<f32>;"));
        assert!(SHADER.contains("@group(0) @binding(10) var<uniform> shore: Shore;"));
    }

    /// The ocean layout the shader repeats agrees with the Rust side (ADR-1969).
    #[test]
    fn ocean_constants_mirror_the_rust_side() {
        use super::super::ocean::OCEAN_BANDS;
        use super::super::ocean_gpu::OCEAN_VEC4S;
        assert_eq!(OCEAN_VEC4S, 2 + 2 * OCEAN_BANDS);
        assert!(SHADER.contains(&format!("band: array<vec4<f32>, {}>", 2 * OCEAN_BANDS)));
        assert!(SHADER.contains(&format!("b < {OCEAN_BANDS}u; b = b + 1u")));
        assert!(SHADER.contains("@group(0) @binding(11) var ocean_tex: texture_2d_array<f32>;"));
        assert!(SHADER.contains("@group(0) @binding(12) var<uniform> ocean: Ocean;"));
    }
}

#[cfg(test)]
mod portal_cull_tests {
    use super::*;

    #[test]
    fn mesh_body_ranges_split_the_index_list_per_body_with_its_box() {
        let a = WaterBody::plane([0.0, 1.0, 0.0], [5.0, 5.0], super::super::WaterLook::lake());
        let b = WaterBody::plane([100.0, 2.0, 0.0], [10.0, 5.0], super::super::WaterLook::lake());
        let (mesh, _) = build_mesh(&[&a, &b]);
        let ranges = mesh_body_ranges(&mesh);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].0.start, 0);
        assert_eq!(ranges[0].0.end, ranges[1].0.start);
        assert_eq!(ranges[1].0.end as usize, mesh.indices.len());
        assert_eq!((ranges[0].1, ranges[0].2), ([-5.0, 1.0, -5.0], [5.0, 1.0, 5.0]));
        assert_eq!((ranges[1].1, ranges[1].2), ([90.0, 2.0, -5.0], [110.0, 2.0, 5.0]));
    }
}
